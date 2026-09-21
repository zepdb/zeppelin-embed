//! Bounded physical replacement over one admitted native graph generation.

use super::NativePreparationSource;
use super::adjacency::{RangeEditContext, consolidate_pending_range};
use super::artifact::ArtifactId;
use super::inventory::{
    PreparedInventoryFold, apply_inventory, for_each_prepared_descriptor, verify_inventory_entry,
};
use super::memory::{StorageBuffer, StorageMemory};
use super::payload::{PayloadRef, prepare_stream};
use super::records::{
    NativeDirectoryValues, NodeRecordState, RecordCatalog, RecordInput, prepare_record,
    verify_node_state,
};
use super::stream::PayloadSlice;
use super::tree::Key;
use super::tree::TreeKind;
use super::tree::directory::{
    BlockSink, DirectoryCursor, DirectoryMutation, GraphRoots, TreeError, TreeResources,
    TreeScratch, insert_checked,
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
/// always rotates.
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
    pub(crate) node: NodeId,
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
    inventory_fold: PreparedInventoryFold<'m>,
    inventory_fold_root: super::tree::directory::DirectoryRoot,
    adoptions: StorageBuffer<'m, crate::property_graph::wal::InventoryChange>,
    relocations: StorageBuffer<'m, RecordRelocation>,
}

impl ConsolidationOutcome<'_> {
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
    manifests: &[RequiredRef],
    inventory_fold: PreparedInventoryFold<'m>,
    reclaim_pending: &[crate::property_graph::wal::InventoryChange],
    adoptions: &[crate::property_graph::wal::InventoryChange],
    memory: &'m StorageMemory<'m>,
    resources: &mut TreeResources<'_>,
) -> Result<ConsolidationOutcome<'m>, TreeError>
where
    T: BlockSink,
    C: RecordCatalog<NativePreparationSource<'lease, 'm>> + RecordCatalog<T>,
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

    let node_root = base.directory(TreeKind::Nodes)?;
    let mut relocations = StorageBuffer::new(memory, 1)?;
    let mut tree = TreeScratch::for_prepare(memory)?;
    let mut replaced_physical_refs = 0_u64;
    if let Some((key, node, record_reference, record_generation, record)) = select_live_node(
        source, base, node_root, catalog, document, manifests, resources,
    )? {
        let [old_canonical, old_provenance] = record.required_payloads();
        let revision = record.revision().get();
        let canonical_source =
            PayloadSlice::new(source, base.store(), record_generation, old_canonical);
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
            PayloadSlice::new(source, base.store(), record_generation, old_provenance);
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
        let record = prepare_record(
            sink,
            RecordInput {
                store: base.store(),
                generation,
                entity: EntityId::Node(node),
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
            node,
            revision,
            old_record: record_reference,
            new_record: record,
        })?;
        let node_root = insert_checked(
            sink,
            DirectoryMutation::new(
                node_root,
                generation,
                NativeDirectoryValues::new(catalog, document),
            ),
            &key,
            &encoded_record,
            &mut tree,
            resources,
        )?;
        roots.replace(node_root)?;
        // The record, its two payload streams and the node root moved.
        replaced_physical_refs = 4;
    }

    for kind in [TreeKind::OutRanges, TreeKind::InRanges] {
        let root = roots.directory(kind)?;
        if let Some(replaced) = consolidate_pending_range(sink, root, context, memory, resources)? {
            roots.replace(replaced)?;
            replaced_physical_refs = replaced_physical_refs
                .checked_add(2)
                .ok_or(TreeError::Work)?;
        }
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
    Ok(ConsolidationOutcome {
        expected_base: base,
        expected_sequence: base_sequence,
        source_token: source.lease().token(),
        roots,
        replaced_physical_refs,
        inventory_fold,
        inventory_fold_root,
        adoptions: adopted,
        relocations,
    })
}

/// Oldest packs considered per selection round, and rounds per call. A pack
/// may hold no live node record (tree pages, sparse rows, tombstones), so a
/// round that finds none moves its serial floor past that pack set.
const OLDEST_PACKS: usize = 16;
const SELECTION_ROUNDS: usize = 8;

type SelectedNode<'a, S> = (
    [u8; 16],
    NodeId,
    PayloadRef,
    GraphGeneration,
    super::records::RecordView<'a, S>,
);

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
    ) -> Result<(), TreeError>,
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
            visit(id, entry, resources)?;
            last = Some(id);
            rows += 1;
        }
        match last.and_then(|id| id.checked_add(1)) {
            Some(next) => lower = Some(next.to_le_bytes()),
            None => return Ok(()),
        }
    }
}

type OldestPacks = ([Option<(u64, ArtifactId)>; OLDEST_PACKS], usize);

/// Keep `packs` as the ascending smallest serials seen; the newest falls off.
fn offer_pack(packs: &mut OldestPacks, floor: u64, serial: u64, artifact: ArtifactId) {
    let (slots, len) = packs;
    let full = *len == OLDEST_PACKS;
    if serial <= floor
        || slots.iter().flatten().any(|(_, other)| *other == artifact)
        || (full
            && slots
                .last()
                .copied()
                .flatten()
                .is_some_and(|(newest, _)| serial >= newest))
    {
        return;
    }
    let position = slots
        .iter()
        .take(*len)
        .position(|pack| pack.is_some_and(|(other, _)| serial < other))
        .unwrap_or(*len);
    *len = (*len + 1).min(OLDEST_PACKS);
    if let Some(tail) = slots.get_mut(position..*len) {
        tail.rotate_right(1);
    }
    if let Some(slot) = slots.get_mut(position) {
        *slot = Some((serial, artifact));
    }
}

/// The unreclaimed packs with the smallest creation serials above `floor`,
/// over the complete allocation union: the rooted inventory plus every
/// admitted prepared manifest. Memory is the fixed array, whatever the union
/// size.
fn oldest_packs<'lease, 'm>(
    anchor: &NativePreparationSource<'lease, 'm>,
    base: GraphRoots,
    manifests: &[RequiredRef],
    floor: u64,
    resources: &mut TreeResources<'_>,
) -> Result<OldestPacks, TreeError> {
    let inventory_root = base.directory(TreeKind::ObjectInventory)?;
    let mut packs: OldestPacks = ([None; OLDEST_PACKS], 0);
    scan_id_directory(anchor, inventory_root, resources, |_, entry, resources| {
        let change = verify_inventory_entry(inventory_root, entry, resources)?;
        if matches!(
            change.state,
            InventoryState::Prepared | InventoryState::Retained
        ) {
            offer_pack(
                &mut packs,
                floor,
                change.object.serial,
                change.object.artifact,
            );
        }
        Ok(())
    })?;
    for required in manifests.iter().copied() {
        // A scoped source releases each manifest's mapping on return.
        let scoped = NativePreparationSource::new_scoped(anchor.lease(), anchor.memory(), 1)?;
        for_each_prepared_descriptor(
            &scoped,
            required,
            base.store(),
            base.generation(),
            resources,
            |descriptor| {
                offer_pack(&mut packs, floor, descriptor.serial, descriptor.artifact);
                Ok(())
            },
        )?;
    }
    Ok(packs)
}

/// The live node whose record lives in the oldest pack. The rooted object
/// inventory gives each pack's creation serial; a relocated record moves to
/// the newest pack, so successive calls rotate through every node and drain
/// the oldest packs first, with no persisted cursor. Records in packs that
/// are not folded into the inventory yet are the youngest and wait. An empty
/// graph, or one whose oldest packs hold no live node, selects nothing: that
/// is a valid maintenance base, and fold and reclaim work never depend on it.
fn select_live_node<'a, 'lease, 'm, C>(
    source: &'a NativePreparationSource<'lease, 'm>,
    base: GraphRoots,
    node_root: super::tree::directory::DirectoryRoot,
    catalog: &C,
    document: Option<&EmbeddingTower>,
    manifests: &[RequiredRef],
    resources: &mut TreeResources<'_>,
) -> Result<Option<SelectedNode<'a, NativePreparationSource<'lease, 'm>>>, TreeError>
where
    C: RecordCatalog<NativePreparationSource<'lease, 'm>>,
{
    #[cfg(any(test, feature = "test-support"))]
    if let Some(node) = PINNED_SELECTION.with(std::cell::Cell::get) {
        let key = node.get().to_le_bytes();
        let entry = super::tree::directory::lookup_entry(source, node_root, &key, resources)?
            .ok_or(TreeError::Invalid("pinned consolidation node is absent"))?;
        let record_reference = PayloadRef::decode(entry.value())?;
        let record_generation = entry.creation_generation();
        return match verify_node_state(
            PayloadSlice::new(source, base.store(), record_generation, record_reference),
            node,
            catalog,
            document,
            resources,
        )? {
            NodeRecordState::Live(record) => Ok(Some((
                key,
                node,
                record_reference,
                record_generation,
                record,
            ))),
            NodeRecordState::Tombstone(_) => {
                Err(TreeError::Invalid("pinned consolidation node is deleted"))
            }
        };
    }
    let mut floor = 0_u64;
    for _ in 0..SELECTION_ROUNDS {
        let (packs, len) = oldest_packs(source, base, manifests, floor, resources)?;
        let Some(packs) = packs.get(..len).filter(|packs| !packs.is_empty()) else {
            return Ok(None);
        };
        let mut oldest: Option<(u64, u128, PayloadRef, GraphGeneration)> = None;
        scan_id_directory(source, node_root, resources, |id, entry, _| {
            let record_reference = PayloadRef::decode(entry.value())?;
            let artifact = record_reference.reference().artifact;
            if let Some((serial, _)) = packs.iter().flatten().find(|(_, pack)| *pack == artifact)
                && oldest.is_none_or(|(other, other_id, _, _)| (*serial, id) < (other, other_id))
            {
                oldest = Some((*serial, id, record_reference, entry.creation_generation()));
            }
            Ok(())
        })?;
        if let Some((serial, id, record_reference, record_generation)) = oldest {
            let node =
                NodeId::new(id).map_err(|_| TreeError::Invalid("consolidation node identity"))?;
            if let NodeRecordState::Live(record) = verify_node_state(
                PayloadSlice::new(source, base.store(), record_generation, record_reference),
                node,
                catalog,
                document,
                resources,
            )? {
                return Ok(Some((
                    id.to_le_bytes(),
                    node,
                    record_reference,
                    record_generation,
                    record,
                )));
            }
            // A tombstone: look past its pack's serial. Tombstones in one pack
            // share the floor, which only delays their live neighbours a call.
            floor = serial;
        } else {
            floor = packs
                .last()
                .copied()
                .flatten()
                .map_or(u64::MAX, |(serial, _)| serial);
        }
    }
    Ok(None)
}
