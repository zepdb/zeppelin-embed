//! Bounded physical replacement over one admitted native graph generation.

use super::NativePreparationSource;
use super::adjacency::{RangeEditContext, relocate_ranges};
use super::artifact::ArtifactId;
use super::inventory::{
    PreparedInventoryFold, apply_inventory, for_each_prepared_descriptor, verify_inventory_entry,
};
use super::memory::{StorageBuffer, StorageMemory};
use super::payload::{PayloadRef, prepare_stream};
use super::records::{
    NativeDirectoryValues, NodeRecordState, RecordCatalog, RecordInput, prepare_record,
    verify_node_state, verify_record,
};
use super::stream::PayloadSlice;
use super::tree::Key;
use super::tree::TreeKind;
use super::tree::directory::{
    BlockSink, DirectoryCursor, DirectoryMutation, DirectoryOp, GraphRoots, TreeError,
    TreeResources, TreeScratch, apply_sorted_checked,
};
use crate::epoch::EmbeddingTower;
use crate::property_graph::wal::{InventoryState, RequiredRef};
use crate::property_graph::{EntityId, GraphGeneration, NodeId};

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static PINNED_SELECTION: std::cell::Cell<Option<NodeId>> = const {
        std::cell::Cell::new(None)
    };
}

/// Pin relocation to one node on this thread until the guard drops. A
/// fixture that needs a fixed partly live pack uses it; production selection
/// uses the current census.
#[cfg(any(test, feature = "test-support"))]
#[must_use]
pub(crate) fn pin_selection_for_test(node: NodeId) -> SelectionPin {
    PINNED_SELECTION.with(|pinned| pinned.set(Some(node)));
    SelectionPin
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) struct SelectionPin;

#[cfg(any(test, feature = "test-support"))]
impl Drop for SelectionPin {
    fn drop(&mut self) {
        PINNED_SELECTION.with(|pinned| pinned.set(None));
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RecordRelocation {
    pub(crate) entity: EntityId,
    pub(crate) revision: u64,
    pub(crate) old_record: PayloadRef,
    pub(crate) new_record: PayloadRef,
}

pub(crate) struct ConsolidationOutcome<'m> {
    expected_base: GraphRoots,
    expected_sequence: u64,
    source_token: u64,
    roots: GraphRoots,
    replaced_physical_refs: u64,
    relocated_bytes: u64,
    inventory_fold: PreparedInventoryFold<'m>,
    inventory_fold_root: super::tree::directory::DirectoryRoot,
    adoptions: StorageBuffer<'m, crate::property_graph::wal::InventoryChange>,
    relocations: StorageBuffer<'m, RecordRelocation>,
}

impl ConsolidationOutcome<'_> {
    pub(crate) const fn relocated_bytes(&self) -> u64 {
        self.relocated_bytes
    }
    pub(crate) const fn expected_base(&self) -> GraphRoots {
        self.expected_base
    }

    pub(crate) const fn expected_sequence(&self) -> u64 {
        self.expected_sequence
    }

    pub(crate) const fn source_token(&self) -> u64 {
        self.source_token
    }

    pub(crate) const fn roots(&self) -> GraphRoots {
        self.roots
    }

    pub(crate) const fn replaced_physical_refs(&self) -> u64 {
        self.replaced_physical_refs
    }

    pub(crate) const fn inventory_fold(&self) -> &PreparedInventoryFold<'_> {
        &self.inventory_fold
    }

    pub(crate) const fn inventory_fold_root(&self) -> super::tree::directory::DirectoryRoot {
        self.inventory_fold_root
    }

    /// Complete unregistered objects rooted as bookkeeping by this commit.
    pub(crate) fn adoptions(&self) -> &[crate::property_graph::wal::InventoryChange] {
        self.adoptions.as_slice()
    }

    pub(crate) fn relocations(&self) -> &[RecordRelocation] {
        self.relocations.as_slice()
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_one_replacement<'lease, 'm, T, C>(
    source: &NativePreparationSource<'lease, 'm>,
    sink: &mut T,
    base: GraphRoots,
    base_sequence: u64,
    catalog: &C,
    document: Option<&EmbeddingTower>,
    drain: &[ArtifactId],
    relocation_bytes: u64,
    inventory_fold: PreparedInventoryFold<'m>,
    pages: &[PageRelocation],
    reclaim_pending: &[crate::property_graph::wal::InventoryChange],
    adoptions: &[crate::property_graph::wal::InventoryChange],
    memory: &'m StorageMemory<'m>,
    resources: &mut TreeResources<'_>,
) -> Result<ConsolidationOutcome<'m>, TreeError>
where
    T: BlockSink,
    C: for<'s> RecordCatalog<NativePreparationSource<'s, 'm>> + RecordCatalog<T>,
{
    resources.require_preparation(memory)?;
    let generation = GraphGeneration::new(
        base.generation()
            .get()
            .checked_add(1)
            .ok_or(TreeError::Invalid("consolidation generation overflow"))?,
    );
    let context = RangeEditContext::new(base.generation(), base_sequence)?;
    let mut roots = base.for_generation(generation)?;

    let mut relocations = StorageBuffer::new(memory, RELOCATION_LIMIT)?;
    let mut tree = TreeScratch::for_prepare(memory)?;
    let mut replaced_physical_refs = 0_u64;
    let mut copied_bytes = 0_u64;
    let selected = select_live_records_in_packs(
        source,
        base,
        catalog,
        document,
        drain,
        relocation_bytes,
        resources,
    )?;
    let mut values = StorageBuffer::new(memory, RELOCATION_LIMIT)?;
    let mut fences: StorageBuffer<'_, FenceRelocation<'_>> =
        StorageBuffer::new(memory, RELOCATION_LIMIT)?;
    for selected in selected.as_slice() {
        resources.step(1)?;
        let scoped = NativePreparationSource::new(source.lease(), memory, 64)?;
        let record_reference = selected.reference;
        let record_generation = selected.generation;
        let record = verify_record(
            PayloadSlice::new(&scoped, base.store(), record_generation, record_reference),
            selected.entity,
            catalog,
            document,
            resources,
        )?;
        let [old_canonical, old_provenance] = record.required_payloads();
        let revision = record.revision().get();
        copied_bytes = copied_bytes
            .checked_add(record_reference.len())
            .and_then(|n| n.checked_add(old_canonical.len()))
            .and_then(|n| n.checked_add(old_provenance.len()))
            .ok_or(TreeError::Work)?;
        let canonical_source =
            PayloadSlice::new(&scoped, base.store(), record_generation, old_canonical);
        let canonical = prepare_stream(
            sink,
            base.store(),
            generation,
            old_canonical.role(),
            usize::try_from(canonical_source.len()).map_err(|_| TreeError::Memory)?,
            &mut |offset, output, r| {
                let read = canonical_source.read_at(offset, output, r)?;
                if read != output.len() {
                    return Err(TreeError::Invalid("short consolidation canonical read"));
                }
                Ok(())
            },
            resources,
        )?;
        let provenance_source =
            PayloadSlice::new(&scoped, base.store(), record_generation, old_provenance);
        let provenance = prepare_stream(
            sink,
            base.store(),
            generation,
            old_provenance.role(),
            usize::try_from(provenance_source.len()).map_err(|_| TreeError::Memory)?,
            &mut |offset, output, r| {
                let read = provenance_source.read_at(offset, output, r)?;
                if read != output.len() {
                    return Err(TreeError::Invalid("short consolidation provenance read"));
                }
                Ok(())
            },
            resources,
        )?;
        if let Some(row) = relocated_fence(
            &scoped, base, &record, canonical, provenance, catalog, document, memory, resources,
        )? {
            let mut position = 0;
            for (key, _) in fences.as_slice() {
                resources.step(1)?;
                let order = compare_relocation_keys(
                    TreeKind::KeyFences,
                    row.0.as_slice(),
                    key.as_slice(),
                    resources,
                )?;
                if order.is_eq() {
                    return Err(TreeError::Invalid("duplicate relocation fence"));
                }
                if order.is_lt() {
                    break;
                }
                position += 1;
            }
            fences.push(row)?;
            fences
                .as_mut_slice()
                .get_mut(position..)
                .ok_or(TreeError::Memory)?
                .rotate_right(1);
        }
        let record = prepare_record(
            sink,
            RecordInput {
                store: base.store(),
                generation,
                entity: selected.entity,
                canonical,
                provenance,
            },
            catalog,
            document,
            memory,
            resources,
        )?;
        let mut encoded_record = [0_u8; 48];
        record.encode_into(&mut encoded_record)?;
        relocations.push(RecordRelocation {
            entity: selected.entity,
            revision,
            old_record: record_reference,
            new_record: record,
        })?;
        values.push(encoded_record)?;
        replaced_physical_refs += 3;
    }
    let mut inventory_fold_root = roots.directory(TreeKind::ObjectInventory)?;
    if !inventory_fold.changes().is_empty() {
        let inventory_root = roots.directory(TreeKind::ObjectInventory)?;
        let folded = apply_inventory(
            sink,
            inventory_root,
            inventory_fold.changes(),
            generation,
            &mut tree,
            resources,
        )?;
        roots.replace(folded)?;
        inventory_fold_root = folded;
        replaced_physical_refs = replaced_physical_refs
            .checked_add(1)
            .ok_or(TreeError::Work)?;
    }
    if !reclaim_pending.is_empty() {
        let inventory_root = roots.directory(TreeKind::ObjectInventory)?;
        let pending = apply_inventory(
            sink,
            inventory_root,
            reclaim_pending,
            generation,
            &mut tree,
            resources,
        )?;
        roots.replace(pending)?;
        replaced_physical_refs = replaced_physical_refs
            .checked_add(1)
            .ok_or(TreeError::Work)?;
    }
    let mut adopted = StorageBuffer::new(memory, adoptions.len())?;
    if !adoptions.is_empty() {
        adopted.extend_from_slice(adoptions)?;
        let inventory_root = roots.directory(TreeKind::ObjectInventory)?;
        let rooted = apply_inventory(
            sink,
            inventory_root,
            adoptions,
            generation,
            &mut tree,
            resources,
        )?;
        roots.replace(rooted)?;
        replaced_physical_refs = replaced_physical_refs
            .checked_add(1)
            .ok_or(TreeError::Work)?;
    }
    replaced_physical_refs += apply_replacements(
        sink,
        &mut roots,
        pages,
        drain,
        context,
        selected.as_slice(),
        values.as_slice(),
        fences.as_slice(),
        relocation_bytes,
        &mut copied_bytes,
        catalog,
        document,
        memory,
        &mut tree,
        resources,
    )?;
    Ok(ConsolidationOutcome {
        expected_base: base,
        expected_sequence: base_sequence,
        source_token: source.lease().token(),
        roots,
        replaced_physical_refs,
        relocated_bytes: copied_bytes,
        inventory_fold,
        inventory_fold_root,
        adoptions: adopted,
        relocations,
    })
}

// Keep each maintenance pass bounded within its fixed 32 MiB storage allowance:
// it also reserves a 16 MiB WAL envelope and census/selection scratch. Leftover
// garbage beyond these caps drains on the next maintenance cycle.
pub(crate) const RELOCATION_LIMIT: usize = 512;
pub(crate) const RELOCATION_BYTES: u64 = 1024 * 1024;
const SMALL_PACK_BYTES: u32 = 64 * 1024;
const MERGE_MIN_PACKS: usize = 8;

#[derive(Clone, Copy)]
pub(crate) struct PackCensus {
    pub(crate) artifact: ArtifactId,
    pub(crate) serial: u64,
    pub(crate) bytes: u32,
    pub(crate) live: u64,
    pub(crate) live_pages: u32,
    pub(crate) live_records: u32,
    pub(crate) graph_live: bool,
}

pub(crate) fn count_live(
    census: &mut [PackCensus],
    reference: super::artifact::PhysicalRef,
    graph: bool,
) -> Result<(), TreeError> {
    if let Ok(index) = census.binary_search_by_key(&reference.artifact, |row| row.artifact) {
        let row = census.get_mut(index).ok_or(TreeError::Memory)?;
        row.graph_live |= graph;
        row.live = row
            .live
            .checked_add(u64::from(reference.length))
            .ok_or(TreeError::Work)?;
        if reference.kind == super::artifact::BlockKind::TreePage {
            row.live_pages = row.live_pages.checked_add(1).ok_or(TreeError::Work)?;
        }
        if matches!(
            reference.kind,
            super::artifact::BlockKind::NodeRecord | super::artifact::BlockKind::RelRecord
        ) {
            row.live_records = row.live_records.checked_add(1).ok_or(TreeError::Work)?;
        }
    }
    Ok(())
}

pub(crate) fn select_drain<'m>(
    census: &mut [PackCensus],
    bytes: u64,
    memory: &'m StorageMemory<'m>,
) -> Result<StorageBuffer<'m, ArtifactId>, TreeError> {
    let small = census
        .iter()
        .filter(|row| row.graph_live && row.live > 0 && row.bytes < SMALL_PACK_BYTES)
        .count();
    census.sort_unstable_by_key(|row| row.serial);
    let mut drain = StorageBuffer::new(memory, super::MAX_NATIVE_ARTIFACTS)?;
    let (mut live, mut records, mut pages) = (0_u64, 0_u64, 0_u64);
    for row in census.iter() {
        if !row.graph_live
            || row.live == 0
            || !(row.live <= u64::from(row.bytes) * 3 / 4
                || (row.bytes < SMALL_PACK_BYTES && small >= MERGE_MIN_PACKS))
        {
            continue;
        }
        let next = (
            live + row.live,
            records + u64::from(row.live_records),
            pages + u64::from(row.live_pages),
        );
        if !drain.as_slice().is_empty()
            && (next.0 > bytes
                || next.1 > RELOCATION_LIMIT as u64
                || next.2 > PAGE_RELOCATION_LIMIT as u64)
        {
            break;
        }
        drain.push(row.artifact)?;
        (live, records, pages) = next;
    }
    drain.as_mut_slice().sort_unstable();
    census.sort_unstable_by_key(|row| row.artifact);
    Ok(drain)
}

struct SelectedRecord {
    key: [u8; 16],
    entity: EntityId,
    reference: PayloadRef,
    generation: GraphGeneration,
}
impl SelectedRecord {
    fn kind(&self) -> TreeKind {
        match self.entity {
            EntityId::Node(_) => TreeKind::Nodes,
            EntityId::Relationship(_) => TreeKind::Relationships,
        }
    }
}

/// Entries read through one source before its mappings are released.
const SCAN_WINDOW: usize = 64;

/// Visit every entry of a directory keyed by one unsigned 128-bit id, in key
/// order. Each window of entries reads through a fresh source, so the pass
/// holds mappings for a few pages at a time however many packs the directory
/// spans. Resumes from checked keys, never from a borrowed page.
fn scan_id_directory<'lease, 'm>(
    anchor: &NativePreparationSource<'lease, 'm>,
    root: super::tree::directory::DirectoryRoot,
    resources: &mut TreeResources<'_>,
    mut visit: impl FnMut(
        u128,
        super::tree::directory::DirectoryEntry<'_>,
        &mut TreeResources<'_>,
    ) -> Result<bool, TreeError>,
) -> Result<(), TreeError> {
    let mut lower: Option<[u8; 16]> = None;
    loop {
        let source = NativePreparationSource::new(anchor.lease(), anchor.memory(), SCAN_WINDOW)?;
        let mut cursor = DirectoryCursor::seek(
            &source,
            root,
            lower.as_ref().map(|key| key.as_slice()),
            resources,
        )?;
        let mut last = None;
        let mut rows = 0_usize;
        while rows < SCAN_WINDOW {
            let Some(entry) = cursor.next_entry(resources)? else {
                return Ok(());
            };
            let Key::Inline(key) = entry.key() else {
                return Err(TreeError::Invalid("consolidation scan key overflow"));
            };
            let id = u128::from_le_bytes(
                key.try_into()
                    .map_err(|_| TreeError::Invalid("consolidation scan key width"))?,
            );
            if !visit(id, entry, resources)? {
                return Ok(());
            }
            last = Some(id);
            rows += 1;
        }
        match last.and_then(|id| id.checked_add(1)) {
            Some(next) => lower = Some(next.to_le_bytes()),
            None => return Ok(()),
        }
    }
}

/// Inventory union, deduplicated by artifact before current-root accounting.
pub(crate) fn pack_census<'lease, 'm>(
    anchor: &NativePreparationSource<'lease, 'm>,
    base: GraphRoots,
    manifests: &[RequiredRef],
    resources: &mut TreeResources<'_>,
) -> Result<StorageBuffer<'m, PackCensus>, TreeError> {
    let mut rows = StorageBuffer::new(anchor.memory(), super::MAX_NATIVE_ARTIFACTS)?;
    let mut offer = |descriptor: crate::property_graph::wal::ArtifactDescriptor| match rows
        .as_slice()
        .binary_search_by_key(&descriptor.artifact, |row: &PackCensus| row.artifact)
    {
        Ok(_) => Ok(()),
        Err(position) => {
            rows.push(PackCensus {
                artifact: descriptor.artifact,
                serial: descriptor.serial,
                bytes: descriptor.bytes,
                live: 0,
                live_pages: 0,
                live_records: 0,
                graph_live: false,
            })?;
            rows.as_mut_slice()
                .get_mut(position..)
                .ok_or(TreeError::Memory)?
                .rotate_right(1);
            Ok(())
        }
    };
    let root = base.directory(TreeKind::ObjectInventory)?;
    scan_id_directory(anchor, root, resources, |_, entry, resources| {
        let change = verify_inventory_entry(root, entry, resources)?;
        if matches!(
            change.state,
            InventoryState::Prepared | InventoryState::Retained
        ) {
            offer(change.object)?;
        }
        Ok(true)
    })?;
    for required in manifests.iter().copied() {
        let scoped = NativePreparationSource::new_scoped(anchor.lease(), anchor.memory(), 1)?;
        for_each_prepared_descriptor(
            &scoped,
            required,
            base.store(),
            base.generation(),
            resources,
            &mut offer,
        )?;
    }
    Ok(rows)
}

/// Select live records from measured drain candidates; tombstones stay put.
#[allow(clippy::too_many_arguments)]
fn select_live_records_in_packs<'lease, 'm, C>(
    source: &NativePreparationSource<'lease, 'm>,
    base: GraphRoots,
    catalog: &C,
    document: Option<&EmbeddingTower>,
    drain: &[ArtifactId],
    relocation_bytes: u64,
    resources: &mut TreeResources<'_>,
) -> Result<StorageBuffer<'m, SelectedRecord>, TreeError>
where
    C: for<'s> RecordCatalog<NativePreparationSource<'s, 'm>>,
{
    let mut selected = StorageBuffer::new(source.memory(), RELOCATION_LIMIT)?;
    let mut bytes = 0_u64;
    #[cfg(any(test, feature = "test-support"))]
    let pinned = PINNED_SELECTION.with(std::cell::Cell::get);
    #[cfg(not(any(test, feature = "test-support")))]
    let pinned: Option<NodeId> = None;
    let mut full = false;
    for kind in [TreeKind::Nodes, TreeKind::Relationships] {
        if full {
            break;
        }
        scan_id_directory(
            source,
            base.directory(kind)?,
            resources,
            |id, entry, resources| {
                resources.step(1)?;
                let entity = match kind {
                    TreeKind::Nodes => EntityId::Node(
                        NodeId::new(id)
                            .map_err(|_| TreeError::Invalid("consolidation node identity"))?,
                    ),
                    TreeKind::Relationships => {
                        EntityId::Relationship(crate::property_graph::RelId::new(id).map_err(
                            |_| TreeError::Invalid("consolidation relationship identity"),
                        )?)
                    }
                    _ => return Err(TreeError::Invalid("consolidation directory kind")),
                };
                if pinned.is_some_and(|node| entity != EntityId::Node(node)) {
                    return Ok(true);
                }
                let reference = PayloadRef::decode(entry.value())?;
                if pinned.is_none()
                    && drain
                        .binary_search(&reference.reference().artifact)
                        .is_err()
                {
                    return Ok(true);
                }
                let generation = entry.creation_generation();
                let scoped = NativePreparationSource::new(source.lease(), source.memory(), 64)?;
                let payload = PayloadSlice::new(&scoped, base.store(), generation, reference);
                let record = match entity {
                    EntityId::Node(node) => {
                        match verify_node_state(payload, node, catalog, document, resources)? {
                            NodeRecordState::Live(record) => record,
                            // Tombstones stay in their packs until ZE-166's sweep;
                            // delete-heavy stores cannot fully shrink yet.
                            NodeRecordState::Tombstone(_) => return Ok(true),
                        }
                    }
                    EntityId::Relationship(_) => {
                        verify_record(payload, entity, catalog, document, resources)?
                    }
                };
                let size = record
                    .required_payloads()
                    .iter()
                    .try_fold(reference.len(), |total, payload| {
                        total.checked_add(payload.len()).ok_or(TreeError::Work)
                    })?;
                let next = bytes.checked_add(size).ok_or(TreeError::Work)?;
                // Admit an oversized first record alone so it cannot stall
                // the oldest pack forever behind the soft byte limit.
                if next > relocation_bytes && !selected.as_slice().is_empty() {
                    full = true;
                    return Ok(false);
                }
                selected.push(SelectedRecord {
                    key: id.to_le_bytes(),
                    entity,
                    reference,
                    generation,
                })?;
                bytes = next;
                full = selected.as_slice().len() == RELOCATION_LIMIT || bytes >= relocation_bytes;
                Ok(!full)
            },
        )?;
    }
    Ok(selected)
}

/// Same-call hints only: losing them cannot change reclamation authority.
pub(crate) const PAGE_RELOCATION_LIMIT: usize = 256;
#[derive(Clone, Copy)]
pub(crate) struct PageRelocation {
    pub(crate) kind: TreeKind,
    pub(crate) reference: super::artifact::PhysicalRef,
}

pub(crate) fn collect_drain_pages<'m>(
    source: &NativePreparationSource<'_, 'm>,
    roots: GraphRoots,
    drain: &[ArtifactId],
    memory: &'m StorageMemory<'m>,
    resources: &mut TreeResources<'_>,
) -> Result<StorageBuffer<'m, PageRelocation>, TreeError> {
    use super::tree::directory::{DirectoryTraceEvent, DirectoryTraceState};
    let mut pages = StorageBuffer::new(memory, PAGE_RELOCATION_LIMIT)?;
    for kind in [
        TreeKind::Nodes,
        TreeKind::Relationships,
        TreeKind::KeyFences,
        TreeKind::Labels,
        TreeKind::RelationshipTypes,
        TreeKind::OutRanges,
        TreeKind::InRanges,
        TreeKind::ObjectInventory,
    ] {
        let mut cursor = DirectoryTraceState::new(roots.directory(kind)?, resources)?;
        loop {
            match cursor.next(source, resources)? {
                DirectoryTraceEvent::Reference(reference)
                    if reference.kind == super::artifact::BlockKind::TreePage
                        && drain.binary_search(&reference.artifact).is_ok() =>
                {
                    if pages.as_slice().len() == PAGE_RELOCATION_LIMIT {
                        return Ok(pages);
                    }
                    pages.push(PageRelocation { kind, reference })?;
                }
                DirectoryTraceEvent::Done => break,
                _ => {}
            }
        }
    }
    Ok(pages)
}

struct PageValues<'a, 'm, C> {
    native: NativeDirectoryValues<'a, C>,
    context: RangeEditContext,
    ranges: super::adjacency::RangeScratch<'m>,
}
impl<S: super::tree::directory::BlockSource, C: RecordCatalog<S>>
    super::tree::directory::LeafValidator<S> for PageValues<'_, '_, C>
{
    fn verify(
        &mut self,
        source: &S,
        root: super::tree::directory::DirectoryRoot,
        entry: super::tree::directory::DirectoryEntry<'_>,
        r: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        use super::adjacency::{RangeDescriptor, validate_descriptor};
        match root.kind() {
            TreeKind::ObjectInventory => verify_inventory_entry(root, entry, r).map(|_| ()),
            TreeKind::OutRanges | TreeKind::InRanges => {
                entry.require_root(root)?;
                let Key::Inline(key) = entry.key() else {
                    return Err(TreeError::Invalid("overflow adjacency key"));
                };
                let descriptor = RangeDescriptor::decode(root.kind(), key, entry.value())?;
                let range = validate_descriptor(
                    source,
                    root.store(),
                    entry.creation_generation(),
                    descriptor,
                    self.context.cutoff(entry)?,
                    &mut self.ranges,
                    r,
                )?;
                if range.edges().is_empty() {
                    return Err(TreeError::Invalid("empty relocation range"));
                }
                Ok(())
            }
            _ => self.native.verify(source, root, entry, r),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_replacements<'m, T: BlockSink, C: RecordCatalog<T>>(
    sink: &mut T,
    roots: &mut GraphRoots,
    pages: &[PageRelocation],
    drain: &[ArtifactId],
    context: RangeEditContext,
    selected: &[SelectedRecord],
    values: &[[u8; 48]],
    fences: &[FenceRelocation<'_>],
    relocation_bytes: u64,
    copied_bytes: &mut u64,
    catalog: &C,
    document: Option<&EmbeddingTower>,
    memory: &'m StorageMemory<'m>,
    tree: &mut TreeScratch<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<u64, TreeError> {
    use super::tree::directory::lookup_entry;
    use super::tree::{Cell, decode_page};
    let mut replaced = 0;
    for kind in [
        TreeKind::Nodes,
        TreeKind::Relationships,
        TreeKind::KeyFences,
        TreeKind::Labels,
        TreeKind::RelationshipTypes,
        TreeKind::OutRanges,
        TreeKind::InRanges,
        TreeKind::ObjectInventory,
    ] {
        let root = roots.directory(kind)?;
        let mut rows: StorageBuffer<'_, (StorageBuffer<'_, u8>, StorageBuffer<'_, u8>)> =
            StorageBuffer::new(memory, PAGE_RELOCATION_LIMIT)?;
        for page in pages.iter().filter(|page| page.kind == kind) {
            resources.step(1)?;
            // Descend the first child: an exclusive upper separator need not
            // itself be a live key, and the final child has no separator.
            let mut reference = page.reference;
            let key = loop {
                resources.step(1)?;
                let block = sink.resolve(reference, resources)?;
                match decode_page(kind, block.payload())?.cell(0)? {
                    Cell::Branch { child, .. } => reference = child,
                    Cell::Leaf { key, .. } => {
                        let length = match key {
                            Key::Inline(bytes) => bytes.len(),
                            Key::Overflow { logical_length, .. } => {
                                usize::try_from(logical_length).map_err(|_| TreeError::Memory)?
                            }
                        };
                        let mut bytes = StorageBuffer::new(memory, length)?;
                        match key {
                            Key::Inline(value) => bytes.extend_from_slice(value)?,
                            Key::Overflow {
                                logical_length,
                                reference,
                            } => {
                                let payload = PayloadRef::new(
                                    super::artifact::BlockKind::OverflowKey,
                                    logical_length,
                                    reference,
                                )?;
                                let stream = PayloadSlice::new(
                                    sink,
                                    root.store(),
                                    root.generation(),
                                    payload,
                                );
                                let mut buffer = [0_u8; 4096];
                                let mut offset = 0;
                                while offset < length {
                                    resources.step(1)?;
                                    let take = buffer.len().min(length - offset);
                                    let output = buffer.get_mut(..take).ok_or(TreeError::Memory)?;
                                    if stream.read_at(offset as u64, output, resources)? != take {
                                        return Err(TreeError::Invalid("short relocation key"));
                                    }
                                    bytes.extend_from_slice(output)?;
                                    offset += take;
                                }
                            }
                        }
                        break bytes;
                    }
                }
            };
            // At most 1024 hints: bounded insertion sort with fallible exact
            // comparators and cancellation at every comparison. Deduplicate
            // branch/leaf hints that select the same path.
            let mut position = 0;
            let mut duplicate = false;
            for (other, _) in rows.as_slice() {
                resources.step(1)?;
                let order =
                    compare_relocation_keys(kind, key.as_slice(), other.as_slice(), resources)?;
                if order.is_eq() {
                    duplicate = true;
                    break;
                }
                if order.is_lt() {
                    break;
                }
                position += 1;
            }
            if duplicate {
                continue;
            }
            let entry = lookup_entry(sink, root, key.as_slice(), resources)?
                .ok_or(TreeError::Invalid("relocation key disappeared"))?;
            // Inventory applies remain separate for replay validation and may
            // already have copied this leaf and every ancestor.
            if entry.creation_generation() == roots.generation() {
                continue;
            }
            let mut value = StorageBuffer::new(memory, entry.value().len())?;
            value.extend_from_slice(entry.value())?;
            *copied_bytes = copied_bytes
                .checked_add(u64::from(page.reference.length))
                .ok_or(TreeError::Work)?;
            rows.push((key, value))?;
            rows.as_mut_slice()
                .get_mut(position..)
                .ok_or(TreeError::Memory)?
                .rotate_right(1);
        }
        let ranges = if matches!(kind, TreeKind::OutRanges | TreeKind::InRanges) {
            relocate_ranges(
                sink,
                root,
                context,
                drain,
                relocation_bytes,
                copied_bytes,
                memory,
                resources,
            )?
        } else {
            StorageBuffer::new(memory, 0)?
        };
        let mut range_rows = StorageBuffer::new(memory, ranges.as_slice().len())?;
        for descriptor in ranges.as_slice() {
            let mut value = [0; super::adjacency::RANGE_DESCRIPTOR_BYTES];
            descriptor.encode(&mut value)?;
            range_rows.push((descriptor.directory_key()?, value))?;
        }
        let mut ops = StorageBuffer::new(
            memory,
            RELOCATION_LIMIT + PAGE_RELOCATION_LIMIT + range_rows.as_slice().len(),
        )?;
        if matches!(kind, TreeKind::Nodes | TreeKind::Relationships) {
            for (selected, value) in selected.iter().zip(values) {
                resources.step(1)?;
                if selected.kind() == kind {
                    ops.push(DirectoryOp::Insert {
                        key: &selected.key,
                        value,
                    })?;
                }
            }
        } else if kind == TreeKind::KeyFences {
            for (key, value) in fences {
                resources.step(1)?;
                ops.push(DirectoryOp::Insert {
                    key: key.as_slice(),
                    value,
                })?;
            }
        }
        for (key, value) in range_rows.as_slice() {
            ops.push(DirectoryOp::Insert { key, value })?;
            replaced += 2;
        }
        if !ops.as_slice().is_empty() {
            replaced += 1;
        }
        for (key, value) in rows.as_slice() {
            resources.step(1)?;
            let mut position = 0;
            let mut duplicate = false;
            for op in ops.as_slice() {
                resources.step(1)?;
                let (DirectoryOp::Insert { key: other, .. } | DirectoryOp::Remove { key: other }) =
                    op;
                let order = compare_relocation_keys(kind, key.as_slice(), other, resources)?;
                if order.is_eq() {
                    duplicate = true;
                    break;
                }
                if order.is_lt() {
                    break;
                }
                position += 1;
            }
            // A real edit takes precedence over an equal-value relocation hint.
            if duplicate {
                continue;
            }
            ops.push(DirectoryOp::Insert {
                key: key.as_slice(),
                value: value.as_slice(),
            })?;
            ops.as_mut_slice()
                .get_mut(position..)
                .ok_or(TreeError::Memory)?
                .rotate_right(1);
            replaced += 1;
        }
        if ops.as_slice().is_empty() {
            continue;
        }
        let validator = PageValues {
            native: NativeDirectoryValues::new(catalog, document),
            context,
            ranges: super::adjacency::RangeScratch::for_prepare(memory, resources)?,
        };
        roots.replace(apply_sorted_checked(
            sink,
            DirectoryMutation::new(root, roots.generation(), validator),
            ops.as_slice(),
            tree,
            resources,
        )?)?;
    }
    Ok(replaced)
}

type FenceRelocation<'m> = (StorageBuffer<'m, u8>, [u8; 144]);

#[allow(clippy::too_many_arguments)]
fn relocated_fence<'m, S: super::tree::directory::BlockSource, C: RecordCatalog<S>>(
    source: &S,
    roots: GraphRoots,
    record: &super::records::RecordView<'_, S>,
    canonical: PayloadRef,
    provenance: PayloadRef,
    catalog: &C,
    document: Option<&EmbeddingTower>,
    memory: &'m StorageMemory<'m>,
    resources: &mut TreeResources<'_>,
) -> Result<Option<FenceRelocation<'m>>, TreeError> {
    use crate::property_graph::catalog::{Symbol, SymbolKind};
    let Some(key) = record.provenance().key() else {
        return Ok(None);
    };
    let Symbol::Namespace(namespace) =
        catalog.resolve(SymbolKind::Namespace, key.namespace(), resources)?
    else {
        return Err(TreeError::Invalid("relocation fence namespace"));
    };
    let length = usize::try_from(key.key().len()).map_err(|_| TreeError::Memory)?;
    let mut encoded = StorageBuffer::new(memory, length.checked_add(9).ok_or(TreeError::Memory)?)?;
    encoded.push(match key.kind() {
        crate::property_graph::EntityKind::Node => 1,
        crate::property_graph::EntityKind::Relationship => 2,
    })?;
    encoded.extend_from_slice(&namespace.get().to_le_bytes())?;
    let mut buffer = [0_u8; 4096];
    let mut offset = 0;
    while offset < length {
        resources.step(1)?;
        let take = buffer.len().min(length - offset);
        let output = buffer.get_mut(..take).ok_or(TreeError::Memory)?;
        if key.key().read_at(offset as u64, output, resources)? != take {
            return Err(TreeError::Invalid("short fence relocation key"));
        }
        encoded.extend_from_slice(output)?;
        offset += take;
    }
    let root = roots.directory(TreeKind::KeyFences)?;
    let entry = super::tree::directory::lookup_entry(source, root, encoded.as_slice(), resources)?
        .ok_or(TreeError::Invalid("missing relocation fence"))?;
    let fence =
        super::records::verify_fence_entry(source, root, entry, catalog, document, resources)?;
    let [old_canonical, old_provenance] = record.required_payloads();
    if fence.incarnation() != record.incarnation()
        || fence.revision() != record.revision()
        || fence.required_payloads() != (old_provenance, Some(old_canonical))
    {
        return Err(TreeError::Invalid("relocation fence/record correlation"));
    }
    let mut value: [u8; 144] = entry
        .value()
        .try_into()
        .map_err(|_| TreeError::Invalid("relocation fence width"))?;
    provenance.encode_into(value.get_mut(40..88).ok_or(TreeError::Memory)?)?;
    canonical.encode_into(value.get_mut(96..144).ok_or(TreeError::Memory)?)?;
    Ok(Some((encoded, value)))
}

fn compare_relocation_keys(
    kind: TreeKind,
    left: &[u8],
    right: &[u8],
    resources: &mut TreeResources<'_>,
) -> Result<std::cmp::Ordering, TreeError> {
    if kind != TreeKind::KeyFences {
        return Ok(super::tree::compare_inline_keys(kind, left, right)?);
    }
    let prefix = super::tree::compare_inline_keys(
        kind,
        left.get(..9).ok_or(TreeError::Memory)?,
        right.get(..9).ok_or(TreeError::Memory)?,
    )?;
    if !prefix.is_eq() {
        return Ok(prefix);
    }
    for (a, b) in left
        .get(9..)
        .ok_or(TreeError::Memory)?
        .chunks(64 * 1024)
        .zip(right.get(9..).ok_or(TreeError::Memory)?.chunks(64 * 1024))
    {
        resources.step(a.len().max(b.len()) as u64)?;
        let order = a.cmp(b);
        if !order.is_eq() {
            return Ok(order);
        }
    }
    Ok(left.len().cmp(&right.len()))
}
