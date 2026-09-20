use super::GraphReadView;
use crate::property_graph::GraphName;
use crate::property_graph::catalog::{Symbol, SymbolKind};
use crate::property_graph::query::runtime::{RetainedView, RuntimeContext};
use crate::property_graph::storage::tree::directory::{TreeError, TreeResources};

impl<'s, 'lease, 'm, 'g> GraphReadView<'s, 'lease, 'm, 'g> {
    pub(crate) fn validate_expression_owner(
        &self,
        runtime: &RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<(), TreeError> {
        self.lease
            .check_active()
            .map_err(crate::property_graph::query::runtime::RuntimeError::Value)
            .map_err(TreeError::Runtime)?;
        runtime.checkpoint().map_err(TreeError::Runtime)?;
        if !std::ptr::eq(runtime.view(), self.lease.query_view())
            || runtime.identity() != self.source.runtime()
            || !std::ptr::eq(runtime.memory(), self.source.memory())
            || !self.catalog.owns(self.source)
        {
            return Err(TreeError::Invalid("native expression owner mismatch"));
        }
        Ok(())
    }

    pub(crate) fn expression_symbol(
        &self,
        kind: SymbolKind,
        name: GraphName<'_>,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<Symbol>, TreeError> {
        self.catalog.lookup(kind, name, resources)
    }

    pub(crate) fn expression_symbol_name(
        &self,
        symbol: Symbol,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<GraphName<'s>>, TreeError> {
        self.catalog.name(symbol, resources)
    }
}
