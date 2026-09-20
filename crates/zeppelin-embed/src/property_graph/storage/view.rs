//! Private source-bound record leaves shared by native query components.
use super::{
    payload::PayloadRef,
    records::{NodeRecordState, RecordCatalog, verify_node_state},
    stream::PayloadSlice,
    tree::{
        TreeKind,
        directory::{BlockSource, GraphRoots, TreeError, TreeResources, lookup_entry},
    },
};
use crate::epoch::EmbeddingTower;
use crate::property_graph::NodeId;

/// Resolve and completely verify one node from caller-owned immutable source
/// bytes. The returned state and every payload view remain bound to `source`;
/// this leaf performs no admission, open, lease, or backing allocation.
pub(super) fn lookup_node_state<'a, S: BlockSource>(
    source: &'a S,
    roots: GraphRoots,
    node: NodeId,
    catalog: &impl RecordCatalog<S>,
    document: Option<&EmbeddingTower>,
    r: &mut TreeResources<'_>,
) -> Result<Option<NodeRecordState<'a, S>>, TreeError> {
    let root = roots.directory(TreeKind::Nodes)?;
    let Some(entry) = lookup_entry(source, root, &node.get().to_le_bytes(), r)? else {
        return Ok(None);
    };
    let payload = PayloadRef::decode(entry.value())?;
    verify_node_state(
        PayloadSlice::new(source, roots.store(), entry.creation_generation(), payload),
        node,
        catalog,
        document,
        r,
    )
    .map(Some)
}
