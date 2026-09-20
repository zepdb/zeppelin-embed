use super::GraphReadView;
use crate::property_graph::catalog::GraphInterpretation;
use crate::property_graph::query::QueryView;
use crate::property_graph::query::runtime::{RetainedView, RuntimeContext, RuntimeError};
use crate::property_graph::storage::tree::directory::TreeError;

pub(crate) struct RetrievalBinding<'a> {
    pub(crate) view: &'a QueryView,
    pub(crate) interpretation: GraphInterpretation<'a>,
}

impl<'s, 'lease, 'm, 'g> GraphReadView<'s, 'lease, 'm, 'g> {
    pub(crate) fn retrieval_binding<'a>(
        &'a self,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<RetrievalBinding<'a>, TreeError> {
        self.lease
            .check_active()
            .map_err(RuntimeError::Value)
            .map_err(TreeError::Runtime)?;
        runtime.checkpoint().map_err(TreeError::Runtime)?;
        if !std::ptr::eq(runtime.view(), self.lease.query_view()) {
            return Err(TreeError::Invalid("foreign native retrieval view"));
        }
        if !std::ptr::eq(runtime.memory(), self.source.memory()) {
            return Err(TreeError::Invalid("foreign native retrieval memory"));
        }
        if runtime.identity() != self.source.runtime() {
            return Err(TreeError::Invalid("foreign native retrieval runtime"));
        }
        let bundle = self.lease.bundle();
        let interpretation = GraphInterpretation::new(bundle.lexical(), bundle.document())
            .map_err(|_| TreeError::Invalid("invalid native retrieval interpretation"))?;
        Ok(RetrievalBinding {
            view: self.lease.query_view(),
            interpretation,
        })
    }
}
