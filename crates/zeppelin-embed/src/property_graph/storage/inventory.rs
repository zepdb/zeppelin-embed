//! Allocation inventory COW, never reachability or unlink authority. Newly created
//! inventory pages remain in the prepared/WAL descriptor inventory until a later
//! fold; their checksums cannot recursively be embedded in this same tree.
use super::NativePreparationSource;
use super::artifact::{ArtifactId, MAX_ARTIFACT_BYTES, put};
use super::memory::{StorageBuffer, StorageMemory};
use super::tree::directory::{
    BlockSink, BlockSource, BulkBuffer, DirectoryCursor, DirectoryEntry, DirectoryMutation,
    DirectoryOp, DirectoryRoot, GraphRoots, LeafValidator, TreeError, TreeResources, TreeScratch,
    apply_sorted_checked, lookup_entry,
};
use super::tree::{Key, TreeKind};
use crate::format::FormatFamily;
use crate::property_graph::wal::{
    ArtifactDescriptor, BatchId, InventoryChange, InventoryState, RequiredRef,
};
use crate::property_graph::{GraphGeneration, StoreInstanceId};
#[cfg(any(test, feature = "test-support"))]
use std::cell::Cell;

const MAX_PREPARED_MANIFEST_DESCRIPTORS: usize = 8_192;
pub(crate) const INVENTORY_FOLD_ADDITION_LIMIT: usize = 32;
/// Leading prepared manifests one maintenance commit may examine and retire.
/// Every commit adds one manifest, so a fold must be able to retire several
/// or the admitted list only grows (owner decision 2026-09-20).
pub(crate) const INVENTORY_FOLD_MANIFEST_LIMIT: usize = 8;
/// Complete unregistered objects one maintenance commit may root as
/// bookkeeping. Rooting grants no deletion authority.
pub(crate) const INVENTORY_ADOPTION_LIMIT: usize = 16;

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static NEXT_FOLD_FAULT: Cell<u8> = const { Cell::new(0) };
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn force_next_incomplete_inventory_retirement() {
    NEXT_FOLD_FAULT.with(|fault| fault.set(1));
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn force_next_contradictory_inventory_addition() {
    NEXT_FOLD_FAULT.with(|fault| fault.set(2));
}

/// Opaque proof produced by authenticating a leading run of admitted prepared
/// inventories and comparing each row with the admitted inventory root. The
/// selected changes stay within one addition budget for the whole run.
pub(crate) struct PreparedInventoryFold<'m> {
    expected_base: GraphRoots,
    expected_sequence: u64,
    source_token: u64,
    /// Examined manifests: an exact prefix of the admitted list.
    selected: StorageBuffer<'m, RequiredRef>,
    /// Leading `selected` manifests that the changes cover completely.
    retired: usize,
    /// Rows of every selected manifest followed by its owner row, each
    /// manifest's segment sorted by artifact. `segments` holds segment ends.
    rows: StorageBuffer<'m, InventoryChange>,
    segments: StorageBuffer<'m, usize>,
    changes: StorageBuffer<'m, InventoryChange>,
}

impl PreparedInventoryFold<'_> {
    pub(crate) fn changes(&self) -> &[InventoryChange] {
        self.changes.as_slice()
    }

    /// Manifests leaving the admitted list in this commit, always a prefix.
    pub(crate) const fn retired(&self) -> usize {
        self.retired
    }

    pub(crate) fn matches_base(
        &self,
        base: GraphRoots,
        sequence: u64,
        source_token: u64,
        manifests: &[RequiredRef],
    ) -> bool {
        self.expected_base == base
            && self.expected_sequence == sequence
            && self.source_token == source_token
            && manifests.get(..self.selected.as_slice().len()) == Some(self.selected.as_slice())
    }

    fn segment(&self, index: usize) -> Result<&[InventoryChange], TreeError> {
        let ends = self.segments.as_slice();
        let end = *ends
            .get(index)
            .ok_or(TreeError::Invalid("inventory fold manifest segment"))?;
        let start = match index.checked_sub(1) {
            Some(previous) => *ends
                .get(previous)
                .ok_or(TreeError::Invalid("inventory fold manifest segment"))?,
            None => 0,
        };
        self.rows
            .as_slice()
            .get(start..end)
            .ok_or(TreeError::Invalid("inventory fold manifest segment"))
    }

    pub(crate) fn validate_candidate(
        &self,
        source: &impl BlockSource,
        root: DirectoryRoot,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        let expected_generation = GraphGeneration::new(
            self.expected_base
                .generation()
                .get()
                .checked_add(1)
                .ok_or(TreeError::Work)?,
        );
        if root.kind() != TreeKind::ObjectInventory
            || root.store() != self.expected_base.store()
            || root.generation() != expected_generation
        {
            return Err(TreeError::Invalid("inventory fold candidate root"));
        }
        for change in self.changes.as_slice() {
            let authorized = self
                .rows
                .as_slice()
                .iter()
                .any(|expected| expected.object == change.object);
            if !authorized {
                return Err(TreeError::Invalid(
                    "inventory fold selected unauthorized descriptor",
                ));
            }
            let actual = lookup_inventory(source, root, change.object, resources)?
                .ok_or(TreeError::Invalid("inventory fold selected row is absent"))?;
            if actual.state != InventoryState::Retained {
                return Err(TreeError::Invalid("inventory fold selected state"));
            }
        }
        for index in 0..self.retired {
            for expected in self.segment(index)? {
                let actual = lookup_inventory(source, root, expected.object, resources)?.ok_or(
                    TreeError::Invalid("inventory fold retirement row is absent"),
                )?;
                if !covered_state(actual.state) {
                    return Err(TreeError::Invalid("inventory fold retirement state"));
                }
            }
        }
        Ok(())
    }
}

/// Authenticate a leading run of admitted prepared manifests and select at
/// most `addition_limit` missing/normalizing rows across the run. A manifest
/// retires only when every row and then its owner row are covered; the run
/// stops at the first manifest that stays partial, so retirement is always a
/// prefix. Repeated calls resume through exact membership in the immutable
/// admitted inventory root.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_inventory_fold<'lease, 'm>(
    source: &NativePreparationSource<'lease, 'm>,
    manifests: &[RequiredRef],
    base: GraphRoots,
    base_sequence: u64,
    source_token: u64,
    addition_limit: usize,
    memory: &'m StorageMemory<'m>,
    resources: &mut TreeResources<'_>,
) -> Result<PreparedInventoryFold<'m>, TreeError> {
    resources.require_preparation(memory)?;
    if addition_limit == 0 || addition_limit > INVENTORY_FOLD_ADDITION_LIMIT {
        return Err(TreeError::Invalid("inventory fold addition limit"));
    }
    let run = manifests
        .get(..manifests.len().min(INVENTORY_FOLD_MANIFEST_LIMIT))
        .ok_or(TreeError::Invalid("inventory fold manifest run"))?;
    let inventory_root = base.directory(TreeKind::ObjectInventory)?;
    let mut selected = StorageBuffer::new(memory, run.len())?;
    let mut segments = StorageBuffer::new(memory, run.len())?;
    let mut changes: StorageBuffer<'m, InventoryChange> =
        StorageBuffer::new(memory, addition_limit)?;
    // Size the row buffer from the authenticated manifest headers. The run
    // holds at most one full-size manifest plus small ones within the same
    // descriptor ceiling, so preflight memory matches the one-manifest bound.
    let mut row_capacity = 0_usize;
    let mut admitted_run = 0_usize;
    for required in run {
        let (_, count) = prepared_manifest(
            source,
            *required,
            base.store(),
            base.generation(),
            resources,
        )?;
        let next = row_capacity
            .checked_add(count)
            .and_then(|rows| rows.checked_add(1))
            .ok_or(TreeError::Memory)?;
        if admitted_run > 0 && next > MAX_PREPARED_MANIFEST_DESCRIPTORS {
            break;
        }
        row_capacity = next;
        admitted_run += 1;
    }
    let mut rows = StorageBuffer::new(memory, row_capacity)?;
    let mut retired = 0_usize;
    for required in run.iter().take(admitted_run).copied() {
        let segment_start = rows.as_slice().len();
        let (payload, count) =
            prepared_manifest(source, required, base.store(), base.generation(), resources)?;
        for index in 0..count {
            resources.step(1)?;
            let descriptor = decode_descriptor(
                payload
                    .get(16 + index * 64..16 + (index + 1) * 64)
                    .ok_or(TreeError::Invalid("prepared inventory descriptor extent"))?,
            )?;
            validate(descriptor, base.store(), base.generation())?;
            rows.push(InventoryChange {
                object: descriptor,
                state: InventoryState::Retained,
            })?;
        }
        let owner = InventoryChange {
            object: required.object,
            state: InventoryState::Retained,
        };
        {
            let manifest = rows
                .as_mut_slice()
                .get_mut(segment_start..)
                .ok_or(TreeError::Invalid("inventory fold manifest segment"))?;
            manifest.sort_unstable_by_key(|change| change.object.artifact);
            if manifest
                .binary_search_by_key(&owner.object.artifact, |change| change.object.artifact)
                .is_ok()
            {
                return Err(TreeError::Invalid("prepared inventory owner row collision"));
            }
            for pair in manifest.windows(2) {
                resources.step(1)?;
                if matches!(pair, [left, right] if left.object.artifact == right.object.artifact) {
                    return Err(TreeError::Invalid(
                        "duplicate prepared inventory row ownership",
                    ));
                }
            }
            manifest.sort_unstable_by_key(|change| change.object.serial);
            if manifest
                .binary_search_by_key(&owner.object.serial, |change| change.object.serial)
                .is_ok()
            {
                return Err(TreeError::Invalid(
                    "prepared inventory owner serial collision",
                ));
            }
            for pair in manifest.windows(2) {
                resources.step(1)?;
                if matches!(pair, [left, right] if left.object.serial == right.object.serial) {
                    return Err(TreeError::Invalid(
                        "duplicate prepared inventory serial ownership",
                    ));
                }
            }
            validate_root_serials_in_windows(
                source,
                inventory_root,
                manifest,
                owner.object,
                resources,
            )?;
            manifest.sort_unstable_by_key(|change| change.object.artifact);
        }
        // The owner row follows its manifest rows: it is covered last.
        rows.push(owner)?;
        let mut complete = true;
        let segment_end = rows.as_slice().len();
        for index in segment_start..segment_end {
            let expected = *rows
                .as_slice()
                .get(index)
                .ok_or(TreeError::Invalid("inventory fold manifest segment"))?;
            let already_selected = match changes
                .as_slice()
                .iter()
                .find(|change| change.object.artifact == expected.object.artifact)
            {
                Some(change) if change.object == expected.object => true,
                Some(_) => {
                    return Err(TreeError::Invalid(
                        "contradictory prepared inventory descriptor",
                    ));
                }
                None => false,
            };
            if already_selected {
                continue;
            }
            let lookup_source = NativePreparationSource::new(source.lease(), memory, 64)?;
            let existing =
                lookup_inventory(&lookup_source, inventory_root, expected.object, resources)?;
            match existing {
                Some(change) if covered_state(change.state) => {}
                Some(change) if change.state != InventoryState::Prepared => {
                    return Err(TreeError::Invalid("inventory fold rooted state"));
                }
                _ => {
                    if changes.as_slice().len() < addition_limit {
                        changes.push(expected)?;
                    } else {
                        complete = false;
                    }
                }
            }
        }
        selected.push(required)?;
        segments.push(segment_end)?;
        if !complete {
            break;
        }
        retired += 1;
    }
    changes
        .as_mut_slice()
        .sort_unstable_by_key(|change| change.object.artifact);
    #[cfg(any(test, feature = "test-support"))]
    NEXT_FOLD_FAULT.with(|fault| match fault.replace(0) {
        1 => retired = selected.as_slice().len(),
        2 => {
            if let Some(change) = changes.as_mut_slice().first_mut() {
                change.object.checksum ^= 1;
            }
        }
        _ => {}
    });
    let fold = PreparedInventoryFold {
        expected_base: base,
        expected_sequence: base_sequence,
        source_token,
        selected,
        retired,
        rows,
        segments,
        changes,
    };
    // Independent completion proof: recompute which leading manifests the
    // admitted root plus the selected changes cover, and require agreement.
    for change in fold.changes.as_slice() {
        let authorized = fold
            .rows
            .as_slice()
            .iter()
            .any(|expected| expected.object == change.object && expected.state == change.state);
        if !authorized || change.state != InventoryState::Retained {
            return Err(TreeError::Invalid(
                "inventory fold selected unauthorized descriptor",
            ));
        }
    }
    let mut observed_retired = 0_usize;
    for index in 0..fold.selected.as_slice().len() {
        let mut covered = true;
        for expected in fold.segment(index)? {
            let lookup_source = NativePreparationSource::new(source.lease(), memory, 64)?;
            let existing =
                lookup_inventory(&lookup_source, inventory_root, expected.object, resources)?;
            let selected_change = fold
                .changes
                .as_slice()
                .binary_search_by_key(&expected.object.artifact, |change| change.object.artifact)
                .ok()
                .and_then(|found| fold.changes.as_slice().get(found))
                .is_some_and(|change| {
                    change.object == expected.object && change.state == expected.state
                });
            if !existing.is_some_and(|change| covered_state(change.state)) && !selected_change {
                covered = false;
            }
        }
        if !covered {
            break;
        }
        observed_retired += 1;
    }
    if fold.retired != observed_retired {
        return Err(TreeError::Invalid(
            "inventory fold completion proof mismatch",
        ));
    }
    Ok(fold)
}

fn validate_root_serials_in_windows(
    anchor: &NativePreparationSource<'_, '_>,
    root: DirectoryRoot,
    manifest: &[InventoryChange],
    owner: ArtifactDescriptor,
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    let mut lower: Option<[u8; 16]> = None;
    loop {
        let source = NativePreparationSource::new(anchor.lease(), anchor.memory(), 64)?;
        let mut cursor = DirectoryCursor::seek(
            &source,
            root,
            lower.as_ref().map(|key| key.as_slice()),
            resources,
        )?;
        let mut last = None;
        let mut rows = 0_usize;
        let window_rows = inventory_mapping_window_rows();
        while rows < window_rows {
            let mut key = [0_u8; 16];
            let mut value = [0_u8; 88];
            let Some((key_length, value_length)) = cursor.next(&mut key, &mut value, resources)?
            else {
                break;
            };
            if key_length != key.len() || value_length != value.len() {
                return Err(TreeError::Invalid("inventory fold rooted row width"));
            }
            let entry = lookup_entry(&source, root, &key, resources)?
                .ok_or(TreeError::Invalid("inventory fold rooted row disappeared"))?;
            let rooted = verify_inventory_entry(root, entry, resources)?;
            if let Ok(index) =
                manifest.binary_search_by_key(&rooted.object.serial, |change| change.object.serial)
                && manifest
                    .get(index)
                    .is_some_and(|change| change.object.artifact != rooted.object.artifact)
            {
                return Err(TreeError::Invalid(
                    "duplicate prepared inventory serial ownership",
                ));
            }
            if rooted.object.serial == owner.serial && rooted.object.artifact != owner.artifact {
                return Err(TreeError::Invalid(
                    "duplicate prepared inventory serial ownership",
                ));
            }
            last = Some(key);
            rows += 1;
        }
        drop(cursor);
        drop(source);
        if rows < window_rows {
            return Ok(());
        }
        let Some(next) = last.and_then(inventory_resume_after) else {
            return Ok(());
        };
        lower = Some(next);
    }
}

const fn inventory_mapping_window_rows() -> usize {
    #[cfg(any(test, feature = "test-support"))]
    {
        1
    }
    #[cfg(not(any(test, feature = "test-support")))]
    {
        8
    }
}

pub(crate) fn inventory_resume_after(key: [u8; 16]) -> Option<[u8; 16]> {
    u128::from_le_bytes(key)
        .checked_add(1)
        .map(u128::to_le_bytes)
}

fn lookup_inventory(
    source: &impl BlockSource,
    root: DirectoryRoot,
    expected: ArtifactDescriptor,
    resources: &mut TreeResources<'_>,
) -> Result<Option<InventoryChange>, TreeError> {
    let Some(entry) = lookup_entry(
        source,
        root,
        &expected.artifact.get().to_le_bytes(),
        resources,
    )?
    else {
        return Ok(None);
    };
    let actual = verify_inventory_entry(root, entry, resources)?;
    if actual.object != expected {
        return Err(TreeError::Invalid(
            "contradictory prepared inventory descriptor",
        ));
    }
    Ok(Some(actual))
}

pub(crate) fn validate_inventory_changes(
    source: &impl BlockSource,
    root: DirectoryRoot,
    expected: &[InventoryChange],
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    if root.kind() != TreeKind::ObjectInventory {
        return Err(TreeError::Invalid("inventory validation root"));
    }
    let mut previous = None;
    for change in expected {
        if previous.is_some_and(|artifact| artifact >= change.object.artifact) {
            return Err(TreeError::Invalid(
                "inventory validation duplicate or unordered change",
            ));
        }
        previous = Some(change.object.artifact);
        let actual = lookup_inventory(source, root, change.object, resources)?
            .ok_or(TreeError::Invalid("inventory validation row is absent"))?;
        if actual.object != change.object || actual.state != change.state {
            return Err(TreeError::Invalid("inventory validation state mismatch"));
        }
    }
    Ok(())
}

pub(crate) fn validate_inventory_retirement(
    source: &impl BlockSource,
    root: DirectoryRoot,
    retired: &[InventoryChange],
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    if root.kind() != TreeKind::ObjectInventory {
        return Err(TreeError::Invalid("inventory retirement validation root"));
    }
    let mut previous = None;
    for change in retired {
        if !matches!(change.state, InventoryState::Reclaimed(_))
            || previous.is_some_and(|artifact| artifact >= change.object.artifact)
        {
            return Err(TreeError::Invalid("inventory retirement validation order"));
        }
        previous = Some(change.object.artifact);
        if lookup_inventory(source, root, change.object, resources)?.is_some() {
            return Err(TreeError::Invalid("retired inventory row remains"));
        }
    }
    Ok(())
}

const fn covered_state(state: InventoryState) -> bool {
    matches!(
        state,
        InventoryState::Retained | InventoryState::ReclaimPending(_) | InventoryState::Reclaimed(_)
    )
}

/// Require that a materialized fold contains exactly the normalized admitted
/// union. This comparison treats inventory values as allocation bookkeeping;
/// it neither traces them as live roots nor weakens immutable descriptors.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn validate_fold_conservation(
    expected: &[InventoryChange],
    actual: &[InventoryChange],
) -> Result<(), TreeError> {
    if expected.len() != actual.len() {
        return Err(TreeError::Invalid("inventory fold omitted descriptor"));
    }
    for (expected, actual) in expected.iter().zip(actual) {
        if expected.object != actual.object
            || expected.state != InventoryState::Retained
            || actual.state != InventoryState::Retained
        {
            return Err(TreeError::Invalid(
                "inventory fold descriptor or state changed",
            ));
        }
    }
    Ok(())
}

/// Stream the allocation descriptors of one admitted prepared manifest
/// through a scoped read: the manifest's mapping is released before return.
pub(crate) fn for_each_prepared_descriptor<S: BlockSource>(
    source: &S,
    required: RequiredRef,
    store: StoreInstanceId,
    generation: GraphGeneration,
    resources: &mut TreeResources<'_>,
    mut visit: impl FnMut(ArtifactDescriptor) -> Result<(), TreeError>,
) -> Result<(), TreeError> {
    if required.object.store != store
        || required.object.generation > generation
        || required.object.family != FormatFamily::NativeGraphObject.id()
        || required.object.version != 1
        || required.block.artifact != required.object.artifact
        || required.block.kind != super::artifact::BlockKind::CommitParticipant
        || required.block.version != 1
    {
        return Err(TreeError::Invalid("prepared inventory required reference"));
    }
    source.with_block(required.block, resources, |block, resources| {
        let identity = block.identity();
        if identity.store != required.object.store
            || identity.artifact != required.object.artifact
            || identity.generation != required.object.generation
            || identity.creation_serial != required.object.serial
            || block.reference() != required.block
            || block.file_length() != required.object.bytes as usize
            || block.file_checksum() != required.object.checksum
        {
            return Err(TreeError::Invalid(
                "prepared inventory immutable descriptor",
            ));
        }
        let payload = block.payload();
        if payload.get(..4) != Some(b"ZGCP".as_slice())
            || payload.get(4..6) != Some(2_u16.to_le_bytes().as_slice())
            || payload.get(6..8) != Some(1_u16.to_le_bytes().as_slice())
            || payload.get(12..16) != Some([0_u8; 4].as_slice())
        {
            return Err(TreeError::Invalid("prepared inventory role or reserved"));
        }
        let rows = payload
            .get(16..)
            .ok_or(TreeError::Invalid("prepared inventory length"))?;
        let count = usize::try_from(u32::from_le_bytes(read(payload, 8)?))
            .map_err(|_| TreeError::Memory)?;
        if count > MAX_PREPARED_MANIFEST_DESCRIPTORS || Some(rows.len()) != count.checked_mul(64) {
            return Err(TreeError::Invalid("prepared inventory length"));
        }
        for row in rows.chunks_exact(64) {
            resources.step(1)?;
            let descriptor = decode_descriptor(row)?;
            validate(descriptor, store, generation)?;
            visit(descriptor)?;
        }
        Ok(())
    })
}

fn prepared_manifest<'a>(
    source: &'a impl BlockSource,
    required: RequiredRef,
    store: StoreInstanceId,
    generation: GraphGeneration,
    resources: &mut TreeResources<'_>,
) -> Result<(&'a [u8], usize), TreeError> {
    if required.object.store != store
        || required.object.generation > generation
        || required.object.family != FormatFamily::NativeGraphObject.id()
        || required.object.version != 1
        || required.block.artifact != required.object.artifact
        || required.block.kind != super::artifact::BlockKind::CommitParticipant
        || required.block.version != 1
    {
        return Err(TreeError::Invalid("prepared inventory required reference"));
    }
    let block = source.resolve(required.block, resources)?;
    let identity = block.identity();
    if identity.store != required.object.store
        || identity.artifact != required.object.artifact
        || identity.generation != required.object.generation
        || identity.creation_serial != required.object.serial
        || block.reference() != required.block
        || block.file_length() != required.object.bytes as usize
        || block.file_checksum() != required.object.checksum
    {
        return Err(TreeError::Invalid(
            "prepared inventory immutable descriptor",
        ));
    }
    let payload = block.payload();
    if payload.get(..4) != Some(b"ZGCP".as_slice())
        || payload.get(4..6) != Some(2_u16.to_le_bytes().as_slice())
        || payload.get(6..8) != Some(1_u16.to_le_bytes().as_slice())
        || payload.get(12..16) != Some([0_u8; 4].as_slice())
    {
        return Err(TreeError::Invalid("prepared inventory role or reserved"));
    }
    let count =
        usize::try_from(u32::from_le_bytes(read(payload, 8)?)).map_err(|_| TreeError::Memory)?;
    if count > MAX_PREPARED_MANIFEST_DESCRIPTORS
        || payload.len()
            != count
                .checked_mul(64)
                .and_then(|bytes| bytes.checked_add(16))
                .ok_or(TreeError::Memory)?
    {
        return Err(TreeError::Invalid("prepared inventory length"));
    }
    Ok((payload, count))
}

fn decode_descriptor(bytes: &[u8]) -> Result<ArtifactDescriptor, TreeError> {
    if bytes.len() != 64 {
        return Err(TreeError::Invalid("prepared inventory descriptor width"));
    }
    Ok(ArtifactDescriptor {
        store: StoreInstanceId::new(u128::from_le_bytes(read(bytes, 0)?))
            .map_err(|_| TreeError::Invalid("zero prepared inventory store"))?,
        artifact: ArtifactId::new(u128::from_le_bytes(read(bytes, 16)?))?,
        generation: GraphGeneration::new(u64::from_le_bytes(read(bytes, 32)?)),
        serial: u64::from_le_bytes(read(bytes, 40)?),
        bytes: u32::from_le_bytes(read(bytes, 48)?),
        family: u16::from_le_bytes(read(bytes, 52)?),
        version: u16::from_le_bytes(read(bytes, 54)?),
        checksum: u64::from_le_bytes(read(bytes, 56)?),
    })
}

/// Decode the same88-byte descriptor/state geometry retained in ZE38 WAL changes.
/// Whole-artifact identity/checksum validation and reclaim proof remain mandatory
/// at admission/publication; inventory membership itself cannot make an entity live.
pub fn verify_inventory_entry(
    root: DirectoryRoot,
    entry: DirectoryEntry<'_>,
    r: &mut TreeResources<'_>,
) -> Result<InventoryChange, TreeError> {
    r.step(88)?;
    entry.require_root(root)?;
    if root.kind() != TreeKind::ObjectInventory
        || entry.creation_generation() > root.generation()
        || entry.value().len() != 88
    {
        return Err(TreeError::Invalid("inventory entry context/length"));
    }
    let Key::Inline(key) = entry.key() else {
        return Err(TreeError::Invalid("overflow inventory identity"));
    };
    if key.len() != 16 {
        return Err(TreeError::Invalid("inventory key width"));
    }
    let bytes = entry.value();
    let object = ArtifactDescriptor {
        store: StoreInstanceId::new(u128::from_le_bytes(read(bytes, 0)?))
            .map_err(|_| TreeError::Invalid("zero inventory store"))?,
        artifact: ArtifactId::new(u128::from_le_bytes(read(bytes, 16)?))?,
        generation: GraphGeneration::new(u64::from_le_bytes(read(bytes, 32)?)),
        serial: u64::from_le_bytes(read(bytes, 40)?),
        bytes: u32::from_le_bytes(read(bytes, 48)?),
        family: u16::from_le_bytes(read(bytes, 52)?),
        version: u16::from_le_bytes(read(bytes, 54)?),
        checksum: u64::from_le_bytes(read(bytes, 56)?),
    };
    if key != object.artifact.get().to_le_bytes() || read::<7>(bytes, 65)? != [0; 7] {
        return Err(TreeError::Invalid("inventory key/reserved"));
    }
    let intent = u128::from_le_bytes(read(bytes, 72)?);
    let state = match (read::<1>(bytes, 64)?, intent) {
        ([1], 0) => InventoryState::Prepared,
        ([2], 0) => InventoryState::Retained,
        ([3], id) => InventoryState::ReclaimPending(
            BatchId::new(id).map_err(|_| TreeError::Invalid("zero reclaim intent"))?,
        ),
        ([4], id) => InventoryState::Reclaimed(
            BatchId::new(id).map_err(|_| TreeError::Invalid("zero reclaim intent"))?,
        ),
        _ => return Err(TreeError::Invalid("inventory state/intent")),
    };
    validate(object, root.store(), entry.creation_generation())?;
    r.step(0)?;
    Ok(InventoryChange { object, state })
}
/// Fold already finalized descriptors and coordinator-validated state changes.
/// Input is strictly ordered by full artifact identity; all descriptors and old
/// immutable identity matches are checked before the first private COW append.
/// Newly emitted pages remain covered by the sink's explicit prepared inventory.
pub fn apply_inventory(
    store: &mut impl BlockSink,
    root: DirectoryRoot,
    changes: &[InventoryChange],
    generation: GraphGeneration,
    scratch: &mut TreeScratch<'_>,
    r: &mut TreeResources<'_>,
) -> Result<DirectoryRoot, TreeError> {
    if root.kind() != TreeKind::ObjectInventory || generation < root.generation() {
        return Err(TreeError::Invalid("inventory root/generation"));
    }
    let mut previous = None;
    for change in changes {
        r.step(1)?;
        validate(change.object, root.store(), generation)?;
        if previous.is_some_and(|id| id >= change.object.artifact) {
            return Err(TreeError::Invalid("duplicate/unordered inventory change"));
        }
        previous = Some(change.object.artifact);
        if let Some(entry) =
            lookup_entry(store, root, &change.object.artifact.get().to_le_bytes(), r)?
            && verify_inventory_entry(root, entry, r)?.object != change.object
        {
            return Err(TreeError::Invalid("inventory immutable descriptor changed"));
        }
    }
    let mut rows = BulkBuffer::new(changes.len(), r)?;
    for change in changes {
        rows.push((
            change.object.artifact.get().to_le_bytes(),
            encode(*change, r)?,
        ))?;
    }
    let mut ops = BulkBuffer::new(changes.len(), r)?;
    for (key, value) in &rows.values {
        r.step(1)?;
        ops.push(DirectoryOp::Insert { key, value })?;
    }
    let candidate = apply_sorted_checked(
        store,
        DirectoryMutation::new(root, generation, InventoryValues),
        &ops.values,
        scratch,
        r,
    )?;
    r.step(0)?;
    DirectoryRoot::from_reference(root.store(), root.kind(), generation, candidate.reference())
}

pub(crate) fn retire_reclaimed_inventory(
    store: &mut impl BlockSink,
    root: DirectoryRoot,
    reclaimed: &[InventoryChange],
    generation: GraphGeneration,
    scratch: &mut TreeScratch<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<DirectoryRoot, TreeError> {
    if root.kind() != TreeKind::ObjectInventory || generation < root.generation() {
        return Err(TreeError::Invalid("inventory retirement root/generation"));
    }
    let mut previous = None;
    for change in reclaimed {
        resources.step(1)?;
        validate(change.object, root.store(), generation)?;
        if !matches!(change.state, InventoryState::Reclaimed(_))
            || previous.is_some_and(|id| id >= change.object.artifact)
        {
            return Err(TreeError::Invalid("inventory retirement state or order"));
        }
        previous = Some(change.object.artifact);
        let entry = lookup_entry(
            store,
            root,
            &change.object.artifact.get().to_le_bytes(),
            resources,
        )?
        .ok_or(TreeError::Invalid("inventory retirement row is absent"))?;
        let actual = verify_inventory_entry(root, entry, resources)?;
        if actual.object != change.object || actual.state != change.state {
            return Err(TreeError::Invalid("inventory retirement row mismatch"));
        }
    }
    let mut keys = BulkBuffer::new(reclaimed.len(), resources)?;
    for change in reclaimed {
        resources.step(1)?;
        keys.push(change.object.artifact.get().to_le_bytes())?;
    }
    let mut ops = BulkBuffer::new(reclaimed.len(), resources)?;
    for key in &keys.values {
        resources.step(1)?;
        ops.push(DirectoryOp::Remove { key })?;
    }
    let candidate = apply_sorted_checked(
        store,
        DirectoryMutation::new(root, generation, InventoryValues),
        &ops.values,
        scratch,
        resources,
    )?;
    DirectoryRoot::from_reference(root.store(), root.kind(), generation, candidate.reference())
}
fn validate(
    object: ArtifactDescriptor,
    store: StoreInstanceId,
    generation: GraphGeneration,
) -> Result<(), TreeError> {
    if object.store != store
        || object.generation > generation
        || object.serial == 0
        || object.family != 17
        || object.version != 1
        || !(104..=MAX_ARTIFACT_BYTES).contains(&(object.bytes as usize))
    {
        return Err(TreeError::Invalid("inventory artifact descriptor"));
    }
    Ok(())
}
fn encode(change: InventoryChange, r: &mut TreeResources<'_>) -> Result<[u8; 88], TreeError> {
    r.step(88)?;
    let mut bytes = [0; 88];
    let object = change.object;
    put(&mut bytes, 0, &object.store.get().to_le_bytes())?;
    put(&mut bytes, 16, &object.artifact.get().to_le_bytes())?;
    put(&mut bytes, 32, &object.generation.get().to_le_bytes())?;
    put(&mut bytes, 40, &object.serial.to_le_bytes())?;
    put(&mut bytes, 48, &object.bytes.to_le_bytes())?;
    put(&mut bytes, 52, &object.family.to_le_bytes())?;
    put(&mut bytes, 54, &object.version.to_le_bytes())?;
    put(&mut bytes, 56, &object.checksum.to_le_bytes())?;
    let (tag, intent) = match change.state {
        InventoryState::Prepared => (1, 0),
        InventoryState::Retained => (2, 0),
        InventoryState::ReclaimPending(id) => (3, id.get()),
        InventoryState::Reclaimed(id) => (4, id.get()),
    };
    put(&mut bytes, 64, &[tag])?;
    put(&mut bytes, 72, &intent.to_le_bytes())?;
    r.step(0)?;
    Ok(bytes)
}
fn read<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], TreeError> {
    bytes
        .get(
            offset
                ..offset
                    .checked_add(N)
                    .ok_or(TreeError::Invalid("inventory offset overflow"))?,
        )
        .and_then(|part| part.try_into().ok())
        .ok_or(TreeError::Invalid("truncated inventory field"))
}

struct InventoryValues;
impl<S: super::tree::directory::BlockSource> LeafValidator<S> for InventoryValues {
    fn verify(
        &mut self,
        _: &S,
        root: DirectoryRoot,
        entry: DirectoryEntry<'_>,
        r: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        verify_inventory_entry(root, entry, r).map(|_| ())
    }
}
