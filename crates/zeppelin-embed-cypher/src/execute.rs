//! Cypher text to a completed native graph result, through the store's one
//! structured statement seam. Compilation runs inside the admission, in its
//! query memory, so the plan, its facts and every owner are charged to the
//! statement and cannot outlive it.
use crate::lowering::{Route, compile_route_in};
use crate::{CompileLimits, ParseError};
use std::cell::Cell;
use std::mem::size_of;
use zeppelin_embed::lifecycle::{QueryControl, Store};
use zeppelin_embed::property_graph::GraphName;
use zeppelin_embed::property_graph::query::completed::{
    CompletedGraphResult, GraphBoundary, GraphQuery, GraphQueryError, GraphQueryOptions,
};
use zeppelin_embed::property_graph::query::plan::ParameterBinding;
use zeppelin_embed::property_graph::query::resources::{MemoryError, RetentionInventory};

/// Why a statement produced no result. Neither variant carries a partial row.
#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "the store's typed cause stays unboxed and allocation-free, as in core"
)]
pub enum StatementError {
    /// Parsing, binding or lowering refused the statement text.
    Compile(ParseError),
    /// The store refused or failed the compiled statement.
    Query(GraphQueryError),
}

impl std::fmt::Display for StatementError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Compile(error) => write!(f, "cypher compile: {error}"),
            Self::Query(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for StatementError {}

/// Compile and run one read or write statement against a native graph store.
///
/// The statement is compiled inside each admission the store opens for it:
/// once under the read admission, and once more under the writer when its
/// plan writes. Search calls use the statement seam's real view-bound adapter.
#[allow(
    clippy::result_large_err,
    reason = "the store's typed cause stays unboxed and allocation-free, as in core"
)]
pub fn execute(
    store: &Store,
    control: &QueryControl,
    options: &GraphQueryOptions,
    text: &str,
    parameters: &[ParameterBinding<'_>],
    limits: CompileLimits,
) -> Result<CompletedGraphResult, StatementError> {
    execute_with_boundary(store, control, options, text, parameters, limits, None)
}

/// Executes with validated binding admission and synchronous precommit copying.
#[doc(hidden)]
#[allow(clippy::too_many_arguments, clippy::result_large_err)]
pub fn execute_with_boundary(
    store: &Store,
    control: &QueryControl,
    options: &GraphQueryOptions,
    text: &str,
    parameters: &[ParameterBinding<'_>],
    limits: CompileLimits,
    boundary: Option<&dyn GraphBoundary>,
) -> Result<CompletedGraphResult, StatementError> {
    let refused = Cell::new(None);
    let execution_refused = Cell::new(None);
    let outcome = store.execute_graph_statement_with_boundary(
        control,
        options,
        |runtime, executor| {
            let memory = runtime.memory();
            let compiled = compile_route_in(
                text,
                parameters,
                limits,
                memory,
                runtime,
                Route::Statement,
                |lowered, _, _, runtime| {
                    // The seam's inventory and column names are statement-owned
                    // copies of what the lowered plan lends; charge them first.
                    let owners_len = lowered.owners().len();
                    let columns_len = lowered.columns().len();
                    let mut charge = memory
                        .reserve_external_capacity()
                        .map_err(crate::lowering::memory_error)?;
                    charge
                        .reserve_additional(
                            std::mem::size_of_val(lowered.owners())
                                + columns_len * size_of::<GraphName<'_>>(),
                        )
                        .map_err(crate::lowering::memory_error)?;
                    let mut owners = Vec::new();
                    owners
                        .try_reserve_exact(owners_len)
                        .map_err(|_| crate::lowering::memory_error(MemoryError::Allocation))?;
                    owners.extend_from_slice(lowered.owners());
                    let mut names = Vec::new();
                    names
                        .try_reserve_exact(columns_len)
                        .map_err(|_| crate::lowering::memory_error(MemoryError::Allocation))?;
                    for column in lowered.columns() {
                        names.push(GraphName::new(column.name).map_err(|_| {
                            crate::lowering::invariant(crate::Span::default(), "column name")
                        })?);
                    }
                    let inventory = match RetentionInventory::vector(&owners) {
                        Ok(inventory) => inventory,
                        Err(error) => return Ok(Err(GraphQueryError::from(error))),
                    };
                    let ran = executor.run(
                        runtime,
                        GraphQuery {
                            plan: lowered.plan(),
                            inventory,
                            bindings: lowered.parameters(),
                            columns: &names,
                        },
                    );
                    match ran {
                        Ok(executed) => Ok(Ok(executed)),
                        Err(error) => {
                            // A trailing compiler checkpoint must not replace the
                            // executor's typed failure and measured work counters.
                            execution_refused.set(Some(error));
                            Ok(Err(GraphQueryError::builder_rejected()))
                        }
                    }
                },
            );
            match compiled {
                Ok(ran) => ran,
                Err(error) => {
                    refused.set(Some(error));
                    Err(GraphQueryError::builder_rejected())
                }
            }
        },
        boundary,
    );
    if let Some(error) = execution_refused.take() {
        return Err(StatementError::Query(error));
    }
    match (outcome, refused.take()) {
        (_, Some(error)) => Err(StatementError::Compile(error)),
        (Ok(result), None) => Ok(result),
        (Err(error), None) => Err(StatementError::Query(error)),
    }
}
