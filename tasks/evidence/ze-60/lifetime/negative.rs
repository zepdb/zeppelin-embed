use crate::ingest::DocumentVersion;
use crate::property_graph::query::runtime::RuntimeContext;
use crate::property_graph::retrieval::{
    NativeRetrievalContext, ResolvedNativeNode, RetrievalError,
};
use crate::property_graph::storage::NativeQuerySource;

fn resolved_node_cannot_outlive_admission<'view, 's, 'lease, 'm, 'g>(
    context: &NativeRetrievalContext<'view, 's, 'lease, 'm, 'g>,
    runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    expected: DocumentVersion,
) -> Result<ResolvedNativeNode<'static, NativeQuerySource<'lease, 'm, 'g>>, RetrievalError> {
    context.resolve(expected, runtime)
}
