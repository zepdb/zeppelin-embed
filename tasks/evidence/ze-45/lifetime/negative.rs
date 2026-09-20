use crate::property_graph::NodeId;
use crate::property_graph::query::resources::QueryMemory;
use crate::property_graph::query::runtime::RuntimeContext;
use crate::property_graph::storage::tree::directory::{BlockSource, TreeError, TreeResources};
use crate::property_graph::storage::{
    GraphReadView, LabelSelection, NativePreparationSource, NativeQuerySource, NodeCursor,
    NodeView, PreparedGraphArtifacts, RelView, TextPayloadReader,
};
use crate::property_graph::RelId;

fn node_cannot_escape<'s, 'lease, 'm, 'g>(
    view: &'s GraphReadView<'s, 'lease, 'm, 'g>,
    resources: &mut TreeResources<'_>,
    node: NodeId,
) -> Result<Option<NodeView<'static, NativeQuerySource<'lease, 'm, 'g>>>, TreeError> {
    view.lookup_node(node, resources)
}

fn cursor_cannot_outlive_admission<'s, 'lease, 'm, 'g>(
    view: &'s GraphReadView<'s, 'lease, 'm, 'g>,
    runtime: &mut RuntimeContext<'lease, 'm, 'g>,
) -> Result<NodeCursor<'static, 'm, 'g>, TreeError> {
    view.node_cursor(LabelSelection::All, runtime)
}

fn text_cannot_outlive_source<'s, 'lease, 'm, 'g>(
    view: &'s GraphReadView<'s, 'lease, 'm, 'g>,
    resources: &mut TreeResources<'_>,
    node: NodeId,
) -> Result<Option<TextPayloadReader<'static, NativeQuerySource<'lease, 'm, 'g>>>, TreeError> {
    view.stored_text(node, resources)
}

fn relationship_cannot_outlive_source<'s, 'lease, 'm, 'g>(
    view: &'s GraphReadView<'s, 'lease, 'm, 'g>,
    resources: &mut TreeResources<'_>,
    relationship: RelId,
) -> Result<Option<RelView<'static, NativeQuerySource<'lease, 'm, 'g>>>, TreeError> {
    view.lookup_relationship(relationship, resources)
}

fn source_cannot_be_replaced_while_view_is_live<'source, 'lease, 'm, 'g>(
    source: &'source mut Option<NativeQuerySource<'lease, 'm, 'g>>,
    catalog: &'source crate::property_graph::storage::NativeCatalog<'source, 'm, 'g>,
) {
    let view = GraphReadView::new(source.as_ref().unwrap(), catalog).unwrap();
    *source = None;
    std::hint::black_box(view.sequence());
}

fn prepared_cannot_outlive_source<'short, 'memory, 'base, S, F>(
    prepared: PreparedGraphArtifacts<'short, 'memory, 'base, S, F>,
) -> PreparedGraphArtifacts<'static, 'memory, 'base, S, F>
where
    S: BlockSource,
    F: FnMut() -> Result<crate::property_graph::storage::artifact::ArtifactIdentity, TreeError>,
{
    prepared
}

fn preparation_source_cannot_outlive_storage<'lease, 'memory>(
    source: NativePreparationSource<'lease, 'memory>,
) -> NativePreparationSource<'lease, 'static> {
    source
}

fn memory_is_part_of_the_source_owner<'lease, 'm, 'g>(
    source: NativeQuerySource<'lease, 'm, 'g>,
    _memory: &'m QueryMemory<'g>,
) -> NativeQuerySource<'lease, 'static, 'static> {
    source
}
