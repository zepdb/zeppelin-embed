//! Retained node deletion evidence for lazy incident-edge sweeping (ZE-109).
use super::*;
use crate::property_graph::storage::artifact::put;
use crate::property_graph::storage::payload::{PayloadRef, prepare_payload};
use crate::property_graph::storage::tree::directory::BlockSink;
use crate::property_graph::{EntityId, GraphGeneration, GraphRevision, StoreInstanceId};

/// A retained deleted node. This preserves deletion classification until all
/// incident records are swept; it contains no dead canonical/text/vector image.
pub struct NodeTombstone<'a, S: BlockSource> {
    node: NodeId,
    document_version: Option<crate::ingest::DocumentVersion>,
    revision: GraphRevision,
    provenance: StoredProvenance<'a, S>,
    provenance_ref: PayloadRef,
}
impl<'a, S: BlockSource> NodeTombstone<'a, S> {
    /// Full retired node identity; IDs are never recycled by tombstone removal.
    pub const fn node(&self) -> NodeId {
        self.node
    }
    /// Retained backing identity for caller-chosen document IDs.
    pub const fn document_version(&self) -> Option<crate::ingest::DocumentVersion> {
        self.document_version
    }
    /// Installed deletion revision, independent of physical relocation.
    pub const fn revision(&self) -> GraphRevision {
        self.revision
    }
    /// Complete checked deletion evidence, including unkeyed Cypher deletes.
    pub const fn provenance(&self) -> &StoredProvenance<'a, S> {
        &self.provenance
    }
    pub(crate) const fn provenance_ref(&self) -> PayloadRef {
        self.provenance_ref
    }
}

/// Distinguish live node contents from explicitly retained deletion evidence.
/// Public graph readers must hide Tombstone; a missing/corrupt required record
/// remains an error rather than being reclassified as a deleted node.
#[allow(
    clippy::large_enum_variant,
    reason = "borrowed views use the fixed charged operation workspace, without a hidden heap allocation"
)]
pub enum NodeRecordState<'a, S: BlockSource> {
    /// Fully correlated live canonical image and symbol index.
    Live(RecordView<'a, S>),
    /// Fully correlated deletion evidence with no live payload.
    Tombstone(NodeTombstone<'a, S>),
}
/// Decode the explicit flag before applying the required whole-record verifier.
pub fn verify_node_state<'a, S: BlockSource>(
    source: PayloadSlice<'a, S>,
    expected: NodeId,
    catalog: &impl RecordCatalog<S>,
    document: Option<&EmbeddingTower>,
    r: &mut TreeResources<'_>,
) -> Result<NodeRecordState<'a, S>, TreeError> {
    if source.role() != BlockKind::NodeRecord || !source.is_whole() {
        return Err(TreeError::Invalid("node record role or window"));
    }
    let mut flags = PayloadCursor::new_with_resources(source.subslice(24, 4)?, r)?;
    match u32::from_le_bytes(flags.read_array(r)?) {
        0 | 2 => verify_record(source, EntityId::Node(expected), catalog, document, r)
            .map(NodeRecordState::Live),
        1 | 3 => verify_node_tombstone(source, expected, r).map(NodeRecordState::Tombstone),
        _ => Err(TreeError::Invalid("unknown node record flags")),
    }
}
/// Validate the exact88-byte tombstone and complete same-generation provenance.
pub fn verify_node_tombstone<'a, S: BlockSource>(
    source: PayloadSlice<'a, S>,
    expected: NodeId,
    r: &mut TreeResources<'_>,
) -> Result<NodeTombstone<'a, S>, TreeError> {
    r.step(1)?;
    if source.role() != BlockKind::NodeRecord
        || !source.is_whole()
        || !matches!(source.len(), 88 | 112)
    {
        return Err(TreeError::Invalid("node tombstone role or extent"));
    }
    let created = source.creation_generation(r)?;
    let mut c = PayloadCursor::new_with_resources(source, r)?;
    let node_bits = u128::from_le_bytes(c.read_array(r)?);
    let revision = GraphRevision::new(u64::from_le_bytes(c.read_array(r)?))
        .map_err(|_| TreeError::Invalid("zero tombstone revision"))?;
    let flags = u32::from_le_bytes(c.read_array(r)?);
    let node = match flags {
        1 if source.len() == 88 => {
            NodeId::new(node_bits).map_err(|_| TreeError::Invalid("zero tombstone node"))?
        }
        3 if source.len() == 112 => NodeId::from(crate::ingest::DocId::new(node_bits)),
        _ => return Err(TreeError::Invalid("node tombstone flags or extent")),
    };
    if node != expected
        || u32::from_le_bytes(c.read_array(r)?) != 0
        || u64::from_le_bytes(c.read_array(r)?) != 0
    {
        return Err(TreeError::Invalid("node tombstone identity or header"));
    }
    let reference = PayloadRef::decode(&c.read_array::<48>(r)?)?;
    let document_version = if flags == 3 {
        let id = crate::ingest::DocId::new(u128::from_le_bytes(c.read_array(r)?));
        let revision = crate::ingest::Revision::new(u64::from_le_bytes(c.read_array(r)?));
        if id.get() != node.get() {
            return Err(TreeError::Invalid("tombstone document identity"));
        }
        Some(crate::ingest::DocumentVersion::new(id, revision))
    } else {
        None
    };
    c.finish(r)?;
    let provenance = verify_provenance(source.linked(reference, r)?, r)?;
    native::validate_installing_provenance(
        &provenance,
        EntityId::Node(node),
        revision,
        created,
        true,
    )?;
    r.step(0)?;
    Ok(NodeTombstone {
        node,
        document_version,
        revision,
        provenance,
        provenance_ref: reference,
    })
}
/// Prepare one retained tombstone without traversing or mutating incident edges.
/// The caller separately updates label membership and any keyed fence atomically.
pub fn prepare_node_tombstone(
    sink: &mut impl BlockSink,
    store: StoreInstanceId,
    generation: GraphGeneration,
    node: NodeId,
    provenance: PayloadRef,
    r: &mut TreeResources<'_>,
) -> Result<PayloadRef, TreeError> {
    prepare_node_tombstone_bound(sink, store, generation, node, provenance, None, r)
}

#[allow(
    clippy::too_many_arguments,
    reason = "record identity, evidence and optional document binding share one preparation"
)]
pub(crate) fn prepare_node_tombstone_bound(
    sink: &mut impl BlockSink,
    store: StoreInstanceId,
    generation: GraphGeneration,
    node: NodeId,
    provenance: PayloadRef,
    document: Option<crate::ingest::DocumentVersion>,
    r: &mut TreeResources<'_>,
) -> Result<PayloadRef, TreeError> {
    r.step(1)?;
    let revision = {
        let evidence =
            verify_provenance(PayloadSlice::new(&*sink, store, generation, provenance), r)?;
        native::validate_installing_provenance(
            &evidence,
            EntityId::Node(node),
            evidence.installed_revision(),
            generation,
            true,
        )?;
        evidence.installed_revision()
    };
    let mut bytes = [0; 112];
    put(&mut bytes, 0, &node.get().to_le_bytes())?;
    put(&mut bytes, 16, &revision.get().to_le_bytes())?;
    put(
        &mut bytes,
        24,
        &if document.is_some() { 3u32 } else { 1u32 }.to_le_bytes(),
    )?;
    provenance.encode_into(bytes.get_mut(40..88).ok_or(TreeError::Memory)?)?;
    if let Some(document) = document {
        if document.doc_id().get() != node.get() {
            return Err(TreeError::Invalid("tombstone document identity"));
        }
        put(&mut bytes, 88, &document.doc_id().get().to_le_bytes())?;
        put(&mut bytes, 104, &document.revision().get().to_le_bytes())?;
    }
    let bytes = bytes
        .get(..if document.is_some() { 112 } else { 88 })
        .ok_or(TreeError::Memory)?;
    let reference = prepare_payload(sink, store, generation, BlockKind::NodeRecord, bytes, r)?;
    verify_node_tombstone(
        PayloadSlice::new(&*sink, store, generation, reference),
        node,
        r,
    )?;
    r.step(0)?;
    Ok(reference)
}
