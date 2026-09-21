use super::{SpillIo, SpillMark};
use crate::property_graph::storage::adjacency::{
    RangeDescriptor, RangeScratch, validate_descriptor,
};
use crate::property_graph::storage::artifact::PhysicalRef;
use crate::property_graph::storage::inventory::verify_inventory_entry;
use crate::property_graph::storage::payload::PayloadRef;
use crate::property_graph::storage::records::{
    NodeRecordState, RecordCatalog, fence_window_reference, verify_fence_entry, verify_node_state,
    verify_record,
};
use crate::property_graph::storage::stream::PayloadSlice;
use crate::property_graph::storage::tree::directory::{
    BlockSource, DirectoryEntry, DirectoryRoot, DirectoryTraceEvent, DirectoryTraceState,
    GraphRoots, TreeError, TreeResources,
};
use crate::property_graph::storage::tree::{Key, TreeKind};
use crate::property_graph::storage::{NativePreparationCatalog, NativePreparationSource};
use crate::property_graph::{EntityId, NodeId, RelId};

pub(crate) trait TraceReferenceVisitor {
    fn visit(
        &mut self,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError>;
}

impl<F> TraceReferenceVisitor for F
where
    F: FnMut(PhysicalRef, &mut TreeResources<'_>) -> Result<(), TreeError>,
{
    fn visit(
        &mut self,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        self(reference, resources)
    }
}

pub(crate) trait TraceEntrySource<C>: BlockSource
where
    C: RecordCatalog<Self>,
    Self: Sized,
{
    #[allow(clippy::too_many_arguments, reason = "one checked record trace")]
    fn trace_record_entry<V: TraceReferenceVisitor>(
        &self,
        catalog: &C,
        kind: TreeKind,
        root: DirectoryRoot,
        document: Option<&crate::epoch::EmbeddingTower>,
        entry: DirectoryEntry<'_>,
        visitor: &mut V,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        trace_record_entry_inner(
            self, catalog, kind, root, document, entry, visitor, resources,
        )
    }

    #[allow(clippy::too_many_arguments, reason = "one checked fence trace")]
    fn trace_fence_entry<V: TraceReferenceVisitor>(
        &self,
        catalog: &C,
        root: DirectoryRoot,
        document: Option<&crate::epoch::EmbeddingTower>,
        entry: DirectoryEntry<'_>,
        visitor: &mut V,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        trace_fence_entry_inner(self, catalog, root, document, entry, visitor, resources)
    }
}

fn trace_payload<V: TraceReferenceVisitor>(
    payload: PayloadRef,
    source: &impl BlockSource,
    store: crate::property_graph::StoreInstanceId,
    generation: crate::property_graph::GraphGeneration,
    visitor: &mut V,
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    let mut index = 0_usize;
    loop {
        let reference = if source.scoped_blocks() {
            payload.physical_reference_at_scoped(source, store, generation, index, resources)?
        } else {
            payload.physical_reference_at(source, store, generation, index, resources)?
        };
        let Some(reference) = reference else {
            return Ok(());
        };
        visitor.visit(reference, resources)?;
        index = index.checked_add(1).ok_or(TreeError::Work)?;
    }
}

pub(crate) fn trace_payload_references<V: TraceReferenceVisitor>(
    payload: PayloadRef,
    source: &impl BlockSource,
    store: crate::property_graph::StoreInstanceId,
    generation: crate::property_graph::GraphGeneration,
    visitor: &mut V,
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    trace_payload(payload, source, store, generation, visitor, resources)
}

fn trace_key<V: TraceReferenceVisitor>(
    key: Key<'_>,
    source: &impl BlockSource,
    store: crate::property_graph::StoreInstanceId,
    generation: crate::property_graph::GraphGeneration,
    visitor: &mut V,
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    if let Key::Overflow {
        logical_length,
        reference,
    } = key
    {
        let payload = PayloadRef::new(
            crate::property_graph::storage::artifact::BlockKind::OverflowKey,
            logical_length,
            reference,
        )?;
        trace_payload(payload, source, store, generation, visitor, resources)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments, reason = "one checked record trace body")]
pub(crate) fn trace_record_entry_inner<S, C, V>(
    source: &S,
    catalog: &C,
    kind: TreeKind,
    root: DirectoryRoot,
    document: Option<&crate::epoch::EmbeddingTower>,
    entry: DirectoryEntry<'_>,
    visitor: &mut V,
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError>
where
    S: BlockSource,
    C: RecordCatalog<S>,
    V: TraceReferenceVisitor,
{
    let Key::Inline(key) = entry.key() else {
        return Err(TreeError::Invalid("overflow native identity"));
    };
    let id = u128::from_le_bytes(
        key.try_into()
            .map_err(|_| TreeError::Invalid("native identity key width"))?,
    );
    let record_ref = PayloadRef::decode(entry.value())?;
    trace_payload(
        record_ref,
        source,
        root.store(),
        entry.creation_generation(),
        visitor,
        resources,
    )?;
    let record = PayloadSlice::new(
        source,
        root.store(),
        entry.creation_generation(),
        record_ref,
    );
    let required = if kind == TreeKind::Nodes {
        match verify_node_state(
            record,
            NodeId::new(id).map_err(|_| TreeError::Invalid("zero traced node"))?,
            catalog,
            document,
            resources,
        )? {
            NodeRecordState::Live(record) => {
                let [canonical, provenance] = record.required_payloads();
                [Some(canonical), Some(provenance)]
            }
            NodeRecordState::Tombstone(tombstone) => [Some(tombstone.provenance_ref()), None],
        }
    } else {
        let record = verify_record(
            record,
            EntityId::Relationship(
                RelId::new(id).map_err(|_| TreeError::Invalid("zero traced relationship"))?,
            ),
            catalog,
            document,
            resources,
        )?;
        let [canonical, provenance] = record.required_payloads();
        [Some(canonical), Some(provenance)]
    };
    for payload in required.into_iter().flatten() {
        trace_payload(
            payload,
            source,
            root.store(),
            entry.creation_generation(),
            visitor,
            resources,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments, reason = "one checked fence trace body")]
pub(crate) fn trace_fence_entry_inner<S, C, V>(
    source: &S,
    catalog: &C,
    root: DirectoryRoot,
    document: Option<&crate::epoch::EmbeddingTower>,
    entry: DirectoryEntry<'_>,
    visitor: &mut V,
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError>
where
    S: BlockSource,
    C: RecordCatalog<S>,
    V: TraceReferenceVisitor,
{
    let fence = verify_fence_entry(source, root, entry, catalog, document, resources)?;
    let (provenance, canonical) = fence.required_payloads();
    trace_payload(
        provenance,
        source,
        root.store(),
        entry.creation_generation(),
        visitor,
        resources,
    )?;
    if let Some(canonical) = canonical {
        trace_payload(
            canonical,
            source,
            root.store(),
            entry.creation_generation(),
            visitor,
            resources,
        )?;
    }
    Ok(())
}

impl<'catalog, 'lease, 'm> TraceEntrySource<NativePreparationCatalog<'catalog, 'lease, 'm>>
    for NativePreparationSource<'lease, 'm>
{
    fn trace_record_entry<V: TraceReferenceVisitor>(
        &self,
        catalog: &NativePreparationCatalog<'catalog, 'lease, 'm>,
        kind: TreeKind,
        root: DirectoryRoot,
        document: Option<&crate::epoch::EmbeddingTower>,
        entry: DirectoryEntry<'_>,
        visitor: &mut V,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        let record = PayloadRef::decode(entry.value())?;
        self.with_artifact_window(record.reference(), resources, |window, resources| {
            trace_record_entry_inner(
                window, catalog, kind, root, document, entry, visitor, resources,
            )
        })
    }

    fn trace_fence_entry<V: TraceReferenceVisitor>(
        &self,
        catalog: &NativePreparationCatalog<'catalog, 'lease, 'm>,
        root: DirectoryRoot,
        document: Option<&crate::epoch::EmbeddingTower>,
        entry: DirectoryEntry<'_>,
        visitor: &mut V,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        let fence = fence_window_reference(entry)?;
        self.with_artifact_window(fence.reference(), resources, |window, resources| {
            trace_fence_entry_inner(window, catalog, root, document, entry, visitor, resources)
        })
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "one authenticated directory leaf trace"
)]
fn trace_directory_entry<S, C, V>(
    source: &S,
    catalog: &C,
    kind: TreeKind,
    root: DirectoryRoot,
    sequence: u64,
    document: Option<&crate::epoch::EmbeddingTower>,
    scratch: &mut RangeScratch<'_>,
    entry: DirectoryEntry<'_>,
    visitor: &mut V,
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError>
where
    S: TraceEntrySource<C>,
    C: RecordCatalog<S>,
    V: TraceReferenceVisitor,
{
    trace_key(
        entry.key(),
        source,
        root.store(),
        entry.creation_generation(),
        visitor,
        resources,
    )?;
    match kind {
        TreeKind::Nodes | TreeKind::Relationships => {
            source.trace_record_entry(catalog, kind, root, document, entry, visitor, resources)?;
        }
        TreeKind::KeyFences => {
            source.trace_fence_entry(catalog, root, document, entry, visitor, resources)?;
        }
        TreeKind::Labels | TreeKind::RelationshipTypes => {
            if !entry.value().is_empty() {
                return Err(TreeError::Invalid("nonempty traced membership value"));
            }
        }
        TreeKind::OutRanges | TreeKind::InRanges => {
            let Key::Inline(key) = entry.key() else {
                return Err(TreeError::Invalid("overflow traced adjacency key"));
            };
            let descriptor = RangeDescriptor::decode(kind, key, entry.value())?;
            let validated = validate_descriptor(
                source,
                root.store(),
                entry.creation_generation(),
                descriptor,
                sequence,
                scratch,
                resources,
            )?;
            if validated.edges().is_empty() {
                return Err(TreeError::Invalid("empty traced adjacency range"));
            }
            let _ = validated;
            visitor.visit(descriptor.base(), resources)?;
            for reference in descriptor.deltas() {
                visitor.visit(reference, resources)?;
            }
        }
        TreeKind::ObjectInventory => {
            let _ = verify_inventory_entry(root, entry, resources)?;
        }
        _ => return Err(TreeError::Invalid("unsupported traced tree role")),
    }
    resources.step(1)
}

#[allow(
    clippy::too_many_arguments,
    reason = "one authenticated graph bundle trace"
)]
pub(crate) fn trace_graph_state<S, C, V>(
    source: &S,
    catalog: &C,
    roots: GraphRoots,
    sequence: u64,
    document: Option<&crate::epoch::EmbeddingTower>,
    scratch: &mut RangeScratch<'_>,
    visitor: &mut V,
    resources: &mut TreeResources<'_>,
) -> Result<u64, TreeError>
where
    S: TraceEntrySource<C>,
    C: RecordCatalog<S>,
    V: TraceReferenceVisitor,
{
    let mut emitted = 0_u64;
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
        let mut cursor = DirectoryTraceState::new(root, resources)?;
        loop {
            match cursor.next(source, resources)? {
                DirectoryTraceEvent::Reference(reference) => {
                    visitor.visit(reference, resources)?;
                    emitted = emitted.checked_add(1).ok_or(TreeError::Work)?;
                }
                DirectoryTraceEvent::Leaf(leaf) => {
                    cursor.with_leaf(leaf, resources, |entry, resources| {
                        trace_directory_entry(
                            source, catalog, kind, root, sequence, document, scratch, entry,
                            visitor, resources,
                        )
                    })?;
                }
                DirectoryTraceEvent::Done => break,
            }
        }
    }
    Ok(emitted)
}

#[allow(
    clippy::too_many_arguments,
    reason = "one authenticated native graph trace window"
)]
pub(crate) fn trace_graph_bundle<'s, 'lease, 'm, T: SpillIo>(
    source: &'s NativePreparationSource<'lease, 'm>,
    catalog: &NativePreparationCatalog<'s, 'lease, 'm>,
    roots: GraphRoots,
    sequence: u64,
    document: Option<&crate::epoch::EmbeddingTower>,
    scratch: &mut RangeScratch<'_>,
    mark: &mut SpillMark<'_>,
    sink: &mut T,
    resources: &mut TreeResources<'_>,
) -> Result<u64, TreeError> {
    let mut visitor = |reference: PhysicalRef, resources: &mut TreeResources<'_>| {
        mark.emit(reference.artifact, sink, resources)
    };
    trace_graph_state(
        source,
        catalog,
        roots,
        sequence,
        document,
        scratch,
        &mut visitor,
        resources,
    )
}
