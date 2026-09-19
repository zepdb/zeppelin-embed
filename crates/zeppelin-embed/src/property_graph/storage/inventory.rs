//! Allocation inventory COW, never reachability or unlink authority. Newly created
//! inventory pages remain in the prepared/WAL descriptor inventory until a later
//! fold; their checksums cannot recursively be embedded in this same tree.
use super::artifact::{ArtifactId, MAX_ARTIFACT_BYTES, put};
use super::tree::directory::{
    BlockSink, DirectoryEntry, DirectoryMutation, DirectoryRoot, LeafValidator, TreeError,
    TreeResources, TreeScratch, insert_checked, lookup_entry,
};
use super::tree::{Key, TreeKind};
use crate::property_graph::wal::{ArtifactDescriptor, BatchId, InventoryChange, InventoryState};
use crate::property_graph::{GraphGeneration, StoreInstanceId};

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
    let mut candidate = root;
    for change in changes {
        let bytes = encode(*change, r)?;
        candidate = insert_checked(
            store,
            DirectoryMutation::new(candidate, generation, InventoryValues),
            &change.object.artifact.get().to_le_bytes(),
            &bytes,
            scratch,
            r,
        )?;
    }
    r.step(0)?;
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
