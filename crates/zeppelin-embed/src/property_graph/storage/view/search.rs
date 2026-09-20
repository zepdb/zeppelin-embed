use super::{GraphReadView, NativeCatalog, NativeQuerySource};
use crate::property_graph::query::resources::QueryMemory;
use crate::property_graph::query::runtime::RuntimeContext;
use crate::property_graph::storage::search::{SparseRoots, SparseView};
use crate::property_graph::storage::tree::directory::{TreeError, TreeResources};

impl<'s, 'lease, 'm, 'g> GraphReadView<'s, 'lease, 'm, 'g> {
    /// Opens retrieval state only from this view's admitted lease, source,
    /// catalog, interpretation and query owner.
    pub(crate) fn sparse_view<'v>(
        &'v self,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<
        SparseView<'v, 'v, NativeQuerySource<'lease, 'm, 'g>, NativeCatalog<'s, 'm, 'g>>,
        TreeError,
    >
    where
        'lease: 'v,
        'm: 'v,
        'g: 'v,
    {
        let binding = self.retrieval_binding(runtime)?;
        let _ = (binding.view, binding.interpretation);
        let bundle = self.lease.bundle();
        let memory: &'v QueryMemory<'v> = self.source.memory();
        let lease: &'v crate::lifecycle::native_graph::NativeReadLease = self.lease;
        let mut resources = TreeResources::for_query(runtime)?;
        SparseView::open_query(
            self.source,
            SparseRoots {
                text: bundle.text(),
                vector: bundle.vector(),
            },
            bundle.roots(),
            bundle.catalog(),
            self.catalog,
            bundle.document(),
            bundle.lexical(),
            memory,
            lease,
            &mut resources,
        )
    }
}
