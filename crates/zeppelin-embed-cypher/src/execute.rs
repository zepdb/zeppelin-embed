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
    CompletedGraphResult, Executed, GraphBoundary, GraphQuery, GraphQueryError, GraphQueryExecutor,
    GraphQueryOptions,
};
use zeppelin_embed::property_graph::query::plan::ParameterBinding;
use zeppelin_embed::property_graph::query::resources::{MemoryError, RetentionInventory};
use zeppelin_embed::property_graph::query::runtime::RuntimeContext;

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
    let outcome = store.execute_graph_statement_with_boundary(
        control,
        options,
        |runtime, executor| {
            compile_statement(
                runtime, executor, options, text, parameters, limits, &refused,
            )
        },
        boundary,
    );
    match (outcome, refused.take()) {
        (_, Some(error)) => Err(StatementError::Compile(error)),
        (Ok(result), None) => Ok(result),
        (Err(error), None) => Err(StatementError::Query(error)),
    }
}

// Keep the store's builder callback separate so its returned error can be
// checked independently of the public StatementError translation.
#[allow(clippy::too_many_arguments, clippy::result_large_err)]
fn compile_statement<'lease, 'm, 'g>(
    runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    executor: GraphQueryExecutor<'_, '_, '_, 'lease, 'm, 'g>,
    options: &GraphQueryOptions,
    text: &str,
    parameters: &[ParameterBinding<'_>],
    limits: CompileLimits,
    refused: &Cell<Option<ParseError>>,
) -> Result<Executed, GraphQueryError> {
    let execution_refused = Cell::new(None);
    let memory = runtime.memory();
    let compiled = compile_route_in(
        text,
        parameters,
        limits,
        memory,
        runtime,
        Route::Statement,
        options.search_options(),
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
    if let Some(error) = execution_refused.take() {
        return Err(error);
    }
    match compiled {
        Ok(ran) => ran,
        Err(error) => {
            refused.set(Some(error));
            Err(GraphQueryError::builder_rejected())
        }
    }
}

/// Cypher statements on the unified Store, using the existing bounded compiler.
#[allow(clippy::result_large_err)]
pub trait StoreCypherExt {
    /// Compile and execute a read or write statement under Store admission.
    fn cypher(
        &self,
        control: &QueryControl,
        options: &GraphQueryOptions,
        text: &str,
        parameters: &[ParameterBinding<'_>],
        limits: CompileLimits,
    ) -> Result<CompletedGraphResult, StatementError>;

    /// Execute with synchronous binding preparation before commit.
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    fn cypher_with_boundary(
        &self,
        control: &QueryControl,
        options: &GraphQueryOptions,
        text: &str,
        parameters: &[ParameterBinding<'_>],
        limits: CompileLimits,
        boundary: Option<&dyn GraphBoundary>,
    ) -> Result<CompletedGraphResult, StatementError>;
}

impl StoreCypherExt for Store {
    fn cypher(
        &self,
        control: &QueryControl,
        options: &GraphQueryOptions,
        text: &str,
        parameters: &[ParameterBinding<'_>],
        limits: CompileLimits,
    ) -> Result<CompletedGraphResult, StatementError> {
        execute(self, control, options, text, parameters, limits)
    }

    fn cypher_with_boundary(
        &self,
        control: &QueryControl,
        options: &GraphQueryOptions,
        text: &str,
        parameters: &[ParameterBinding<'_>],
        limits: CompileLimits,
        boundary: Option<&dyn GraphBoundary>,
    ) -> Result<CompletedGraphResult, StatementError> {
        execute_with_boundary(self, control, options, text, parameters, limits, boundary)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use zeppelin_embed::lifecycle::{CancelToken, OpenOptions};
    use zeppelin_embed::property_graph::query::completed::{
        CompletedError, GraphQueryErrorKind, ResultSource,
    };
    use zeppelin_embed::property_graph::query::runtime::WorkCounters;

    struct FailExecutionAndTeardown(CancelToken);

    impl GraphBoundary for FailExecutionAndTeardown {
        fn classified(&self, _: bool) -> Result<(), GraphQueryError> {
            Ok(())
        }

        fn prepare(
            &self,
            _: &dyn ResultSource,
            _: &mut RuntimeContext<'_, '_, '_>,
        ) -> Result<usize, CompletedError> {
            self.0.cancel();
            Err(CompletedError::Limit)
        }

        fn completed(&self, _: WorkCounters, _: usize) {
            panic!("executor must fail before completion")
        }
    }

    #[test]
    fn ze284_executor_error_reaches_store_when_teardown_fails() {
        let directory = std::env::temp_dir().join(format!("ze284-{}", std::process::id()));
        std::fs::create_dir(&directory).expect("create fixture");
        let store = Store::create_graph_store(directory.join("graph"), OpenOptions::new())
            .expect("create graph store");
        let token = CancelToken::new();
        let control = QueryControl::Cancel(token.clone());
        let boundary = FailExecutionAndTeardown(token);
        let options = GraphQueryOptions::default();
        let refused = Cell::new(None);
        let observed = Cell::new(false);
        let error = store
            .execute_graph_statement_with_boundary(
                &control,
                &options,
                |runtime, executor| {
                    let result = compile_statement(
                        runtime,
                        executor,
                        &options,
                        "CREATE () RETURN 1",
                        &[],
                        CompileLimits::default(),
                        &refused,
                    );
                    // Cancellation starts inside execution, so the compiler's
                    // trailing checkpoint also fails after the executor returns.
                    if control.checkpoint().is_err() {
                        observed.set(true);
                        assert_eq!(
                            result.as_ref().err().map(GraphQueryError::kind),
                            Some(GraphQueryErrorKind::Limit),
                            "the store callback must return the executor error"
                        );
                    }
                    result
                },
                Some(&boundary),
            )
            .err()
            .expect("execution must fail");
        assert!(observed.get(), "execution and compiler teardown must fail");
        assert_eq!(error.kind(), GraphQueryErrorKind::Limit);
        let token = CancelToken::new();
        let control = QueryControl::Cancel(token.clone());
        let boundary = FailExecutionAndTeardown(token);
        let error = execute_with_boundary(
            &store,
            &control,
            &options,
            "CREATE () RETURN 1",
            &[],
            CompileLimits::default(),
            Some(&boundary),
        )
        .err()
        .expect("public execution must fail");
        let StatementError::Query(error) = error else {
            panic!("executor error must take precedence over compiler teardown");
        };
        assert_eq!(error.kind(), GraphQueryErrorKind::Limit);
        store.close_graph().expect("close graph store");
        std::fs::remove_dir_all(directory).expect("remove fixture");
    }
}
