use crate::property_graph::NodeId;
use crate::property_graph::query::runtime::RuntimeContext;
use crate::property_graph::storage::tree::directory::{TreeError, TreeResources};
use crate::property_graph::storage::{
    GraphReadView, LabelSelection, NativePreparationSource, PreparedGraphArtifacts,
};

fn copy_inside_scope<'s, 'lease, 'm, 'g>(
    view: &'s GraphReadView<'s, 'lease, 'm, 'g>,
    runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    node: NodeId,
) -> Result<(Option<NodeId>, usize), TreeError> {
    let mut resources = TreeResources::for_query(runtime)?;
    let copied =
        view.lookup_node(node, &mut resources)?
            .map(|record| match record.record().shape() {
                crate::property_graph::storage::records::RecordShape::Node { id, .. } => id,
                _ => node,
            });
    let text_bytes = match view.stored_text(node, &mut resources)? {
        Some(text) => {
            let mut output = [0_u8; 16];
            text.read_at(0, &mut output, &mut resources)?
        }
        None => 0,
    };
    drop(resources);
    let cursor = view.node_cursor(LabelSelection::All, runtime)?;
    drop(cursor);
    Ok((copied, text_bytes))
}

fn inspect_preparation_source<'lease, 'memory>(
    source: &NativePreparationSource<'lease, 'memory>,
) -> usize {
    source.memory().reserved_bytes()
}

fn transfer_finished_preparation<'source, 'memory, 'base, S, F>(
    prepared: PreparedGraphArtifacts<'source, 'memory, 'base, S, F>,
) -> (
    crate::property_graph::storage::adjacency::NativeGraphCandidate<'memory>,
    crate::property_graph::storage::prepared::PreparedObjects<'memory, 'base, S, F>,
    crate::lifecycle::native_graph::NativeReadLease,
)
where
    S: crate::property_graph::storage::tree::directory::BlockSource,
    F: FnMut() -> Result<crate::property_graph::storage::artifact::ArtifactIdentity, TreeError>,
{
    prepared.into_parts()
}
