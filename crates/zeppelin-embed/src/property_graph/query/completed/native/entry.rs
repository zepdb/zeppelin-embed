//! The GraphStore structured execution seam: one plan in, one
//! `CompletedGraphResult` or one typed `GraphQueryError` out.
//!
//! The shape mirrors the Cypher crate's `compile_read_in`: a scoped callback
//! runs inside the admission and borrows everything the admission owns. The
//! builder receives the admitted `RuntimeContext` (and so the admitted
//! `QueryMemory`), builds and validates its plan there, and hands it to the
//! [`GraphQueryExecutor`] it was given. The executor admits the plan against
//! the builder's retained owners, runs it on the right path and keeps the
//! result; the builder only gets an opaque [`Executed`] receipt back. No plan,
//! view, overlay or result can escape the callback, and nothing is copied.
//!
//! Read or write admission is chosen from the plan's own classification,
//! which only exists once the plan is built. Every statement is therefore
//! built first under a read admission. A read plan runs there. A plan whose
//! classification writes is not run: the read admission is dropped unused,
//! and the builder runs again under the writer admission, where the plan is
//! executed, copied, committed and settled. A builder must therefore be
//! re-runnable, which `with_native_mutation` already requires, since a
//! checkpoint discards and reruns its attempt.
//!
//! Once the commit tail has been entered, nothing here checks cancellation
//! again: a statement whose commit may have landed is reported as committed
//! or as `WriteIndeterminate`, never as cancelled.

use super::super::{CompletedGraphResult, UnsettledWriteResult};
use super::{
    GraphQueryError, execute_native_mutation_diagnosed, execute_native_result,
    execute_native_search_result,
};
use crate::lifecycle::native_graph::{NativeMutationConsumer, NativeReadConsumer};
use crate::lifecycle::{QueryControl, Store};
use crate::property_graph::GraphName;
use crate::property_graph::query::expression::ExpressionCapacity;
use crate::property_graph::query::pattern::{
    PatternCapacity, SearchAdapter, SearchHit, SearchInvocation,
};
use crate::property_graph::query::plan::{GraphPlan, ParameterBinding, PlanError};
use crate::property_graph::query::relational::StorageCapacity;
use crate::property_graph::query::resources::{QueryArena, QueryInputs, RetentionInventory};
use crate::property_graph::query::runtime::{
    ArenaCapacity, ExecutionCapacity, NativeExecutionError, RuntimeContext, RuntimeLimits,
};
use crate::property_graph::staging::{GraphBatchReadView, StatementImages, WriteControl};
use crate::property_graph::storage::GraphReadView;
use crate::property_graph::storage::tree::directory::TreeError;
use std::cell::Cell;

/// Every explicit limit and capacity one statement is admitted with.
#[derive(Clone, Copy)]
pub struct GraphQueryOptions {
    /// Cumulative runtime work limits.
    pub(crate) limits: RuntimeLimits,
    /// The statement's query-memory sublimit.
    pub(crate) memory_limit: usize,
    /// Native read-source mapping slots.
    pub(crate) source_slots: usize,
    /// Row and expression capacities of the native pattern tree.
    pub(crate) pattern: PatternCapacity,
    /// Batch and result capacities of the pull driver.
    pub(crate) execution: ExecutionCapacity,
    /// Write statements only: MATCH targets the admitted base resolves lazily.
    pub(crate) lazy_targets: usize,
    /// Write statements only: staged entities the overlay admits.
    pub(crate) overlay_capacity: usize,
    /// Write statements only: replacement images the statement arena admits.
    pub(crate) image_capacity: usize,
}

impl GraphQueryOptions {
    /// Sets the returned-row capacity (1..=65,536). Other operator, payload
    /// and work budgets remain independent; exceeding any budget fails.
    pub fn with_result_row_limit(
        mut self,
        rows: usize,
    ) -> Result<Self, crate::property_graph::query::runtime::RuntimeError> {
        if !(1..=65_536).contains(&rows) {
            return Err(crate::property_graph::query::runtime::RuntimeError::BatchCapacity);
        }
        self.execution.result_rows = rows;
        Ok(self)
    }
}

impl Default for GraphQueryOptions {
    /// Blocking storage grows in 1,024-row chunks up to the query budget.
    /// Results and staged write entities retain their separate 1,024 caps.
    fn default() -> Self {
        let variable = ArenaCapacity {
            string_bytes: 64 * 1024,
            list_cells: 4096,
            node_ids: 1024,
            relationship_ids: 1024,
        };
        Self {
            limits: RuntimeLimits::default(),
            memory_limit: 24 * 1024 * 1024,
            source_slots: crate::property_graph::storage::MAX_NATIVE_ARTIFACTS,
            pattern: PatternCapacity {
                rows: StorageCapacity {
                    rows: 1024,
                    max_rows: 65_536,
                    payload_bytes: 256 * 1024,
                    variable,
                },
                expression: ExpressionCapacity {
                    cells: 1024,
                    string_bytes: 64 * 1024,
                },
            },
            execution: ExecutionCapacity {
                batch_rows: 64,
                result_rows: 1024,
                batch_payload_bytes: 256 * 1024,
                result_payload_bytes: 256 * 1024,
                batch: variable,
                result: variable,
            },
            lazy_targets: 1024,
            overlay_capacity: 1024,
            image_capacity: 1024,
        }
    }
}

/// A validated plan and the owners that retain its backing, built by the
/// caller inside the admitted query memory.
pub struct GraphQuery<'q, 'plan, 'facts, 'a> {
    /// The validated plan.
    pub plan: &'q GraphPlan<'plan, 'facts>,
    /// The actual owners of every span the plan borrows, including its facts.
    pub inventory: RetentionInventory<'q, 'a>,
    /// The statement's parameter values, by name.
    pub bindings: &'q [ParameterBinding<'q>],
    /// One name per root output column, in column order.
    pub columns: &'q [GraphName<'q>],
}

/// Proof that a builder handed its plan to its executor. It carries nothing:
/// the result stays with the seam.
#[must_use]
pub struct Executed(());

/// The type of an absent search adapter. It has no values.
pub(crate) enum NoSearch {}

impl<'v, 'm, 'g> SearchAdapter<'v, 'm, 'g> for NoSearch {
    fn search<'s>(
        &mut self,
        _: &'s GraphReadView<'s, 'v, 'm, 'g>,
        _: &SearchInvocation<'_, '_, 'v, 'm, 'g>,
        _: &mut QueryArena<'m, 'g, SearchHit>,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<crate::property_graph::query::completed::SearchReport, NativeExecutionError> {
        match *self {}
    }
}

#[allow(
    clippy::large_enum_variant,
    reason = "one per statement, on the stack; boxing the overlay would allocate per statement"
)]
enum Admission<'w, 'i, 'lease, 'm, 'g> {
    Read {
        view: &'w GraphReadView<'w, 'lease, 'm, 'g>,
        search: Option<&'w mut dyn SearchAdapter<'lease, 'm, 'g>>,
    },
    Write {
        view: &'w GraphReadView<'w, 'lease, 'm, 'g>,
        overlay: GraphBatchReadView<'w, 'static>,
        images: &'w StatementImages<'i>,
    },
}

/// What the executor did, kept by the seam rather than the builder.
#[derive(Default)]
#[allow(
    clippy::large_enum_variant,
    reason = "one per statement, on the stack; the result moves out by value"
)]
enum Ran<'w> {
    #[default]
    Nothing,
    /// The plan writes; it was not run under the read admission.
    NeedsWrite,
    Read(CompletedGraphResult),
    Write(UnsettledWriteResult, GraphBatchReadView<'w, 'static>),
}

/// Runs one validated plan under the admission the seam chose. It is
/// consumed by [`GraphQueryExecutor::run`], so a builder runs at most one
/// plan per admission.
pub struct GraphQueryExecutor<'x, 'w, 'i, 'lease, 'm, 'g> {
    admission: Admission<'w, 'i, 'lease, 'm, 'g>,
    options: &'x GraphQueryOptions,
    ran: &'x Cell<Ran<'w>>,
}

impl<'w, 'lease, 'm, 'g> GraphQueryExecutor<'_, 'w, '_, 'lease, 'm, 'g> {
    /// Admits `query` against its retained owners and runs it.
    pub fn run(
        self,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
        query: GraphQuery<'_, '_, '_, '_>,
    ) -> Result<Executed, GraphQueryError> {
        let GraphQuery {
            plan,
            inventory,
            bindings,
            columns,
        } = query;
        let classification = plan.classification();
        let options = *self.options;
        match self.admission {
            Admission::Read { view, search } => {
                if classification.writes() {
                    // Only the writer admission may run a write. Nothing was
                    // admitted or charged for this plan here.
                    self.ran.set(Ran::NeedsWrite);
                    return Ok(Executed(()));
                }
                let admitted = QueryInputs::reserve(runtime.memory(), inventory, runtime.values())?
                    .admit_plan(plan, runtime.values())?;
                let result = if classification.searches() {
                    let adapter = search.ok_or(GraphQueryError::from(
                        NativeExecutionError::Plan(PlanError::Search),
                    ))?;
                    execute_native_search_result(
                        view,
                        runtime,
                        &admitted,
                        bindings,
                        columns,
                        options.pattern,
                        options.execution,
                        adapter,
                    )?
                } else {
                    execute_native_result(
                        view,
                        runtime,
                        &admitted,
                        bindings,
                        columns,
                        options.pattern,
                        options.execution,
                    )?
                };
                self.ran.set(Ran::Read(result));
            }
            Admission::Write {
                view,
                overlay,
                images,
            } => {
                if !classification.writes() {
                    return Err(GraphQueryError::contract(
                        "the builder's plan wrote under the read admission but not under the writer",
                    ));
                }
                let admitted = QueryInputs::reserve(runtime.memory(), inventory, runtime.values())?
                    .admit_plan(plan, runtime.values())?;
                let (unsettled, overlay) = execute_native_mutation_diagnosed(
                    view,
                    runtime,
                    &admitted,
                    bindings,
                    columns,
                    options.pattern,
                    options.execution,
                    overlay,
                    images,
                )?;
                self.ran.set(Ran::Write(unsettled, overlay));
            }
        }
        Ok(Executed(()))
    }
}

const NOT_EXECUTED: &str = "the builder returned without executing its plan";

/// The read admission's consumer: builds, then runs a read plan or reports
/// that the plan writes.
struct ReadStatement<'b, S: ?Sized, B> {
    options: &'b GraphQueryOptions,
    search: Option<&'b mut S>,
    build: &'b mut B,
}

#[allow(
    clippy::large_enum_variant,
    reason = "returned once per statement; the result moves out by value"
)]
enum ReadRan {
    Done(CompletedGraphResult),
    NeedsWrite,
}

impl<S, B> NativeReadConsumer<Result<ReadRan, GraphQueryError>> for ReadStatement<'_, S, B>
where
    S: for<'v, 'm, 'g> SearchAdapter<'v, 'm, 'g>,
    B: for<'x, 'w, 'i, 'lease, 'm, 'g> FnMut(
        &mut RuntimeContext<'lease, 'm, 'g>,
        GraphQueryExecutor<'x, 'w, 'i, 'lease, 'm, 'g>,
    ) -> Result<Executed, GraphQueryError>,
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<Result<ReadRan, GraphQueryError>, TreeError> {
        let ran = Cell::new(Ran::Nothing);
        let search = self
            .search
            .as_deref_mut()
            .map(|adapter| adapter as &mut dyn SearchAdapter<'lease, 'm, 'g>);
        let executor = GraphQueryExecutor {
            admission: Admission::Read { view, search },
            options: self.options,
            ran: &ran,
        };
        let built = (self.build)(runtime, executor);
        Ok(match (built, ran.take()) {
            (Err(error), _) => Err(error),
            (Ok(_), Ran::Read(result)) => Ok(ReadRan::Done(result)),
            (Ok(_), Ran::NeedsWrite) => Ok(ReadRan::NeedsWrite),
            (Ok(_), Ran::Nothing | Ran::Write(..)) => Err(GraphQueryError::contract(NOT_EXECUTED)),
        })
    }
}

/// The writer admission's consumer: builds and runs a write plan, copying
/// its complete result before anything commits.
struct WriteStatement<'b, B> {
    options: &'b GraphQueryOptions,
    build: &'b mut B,
}

impl<B> NativeMutationConsumer<UnsettledWriteResult, GraphQueryError> for WriteStatement<'_, B>
where
    B: for<'x, 'w, 'i, 'lease, 'm, 'g> FnMut(
        &mut RuntimeContext<'lease, 'm, 'g>,
        GraphQueryExecutor<'x, 'w, 'i, 'lease, 'm, 'g>,
    ) -> Result<Executed, GraphQueryError>,
{
    fn consume<'lease, 'm, 'g, 'w, 'i>(
        &mut self,
        view: &'w GraphReadView<'w, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
        overlay: GraphBatchReadView<'w, 'static>,
        images: &'w StatementImages<'i>,
        _: &mut WriteControl<'_>,
    ) -> Result<(UnsettledWriteResult, GraphBatchReadView<'w, 'static>), GraphQueryError> {
        let ran = Cell::new(Ran::Nothing);
        let executor = GraphQueryExecutor {
            admission: Admission::Write {
                view,
                overlay,
                images,
            },
            options: self.options,
            ran: &ran,
        };
        // A rejected builder drops the overlay with the executor or with
        // `ran`, so nothing it staged can reach the commit tail.
        let Executed(()) = (self.build)(runtime, executor)?;
        match ran.take() {
            Ran::Write(result, overlay) => Ok((result, overlay)),
            Ran::Nothing | Ran::NeedsWrite | Ran::Read(_) => {
                Err(GraphQueryError::contract(NOT_EXECUTED))
            }
        }
    }
}

impl Store {
    /// Runs one structured graph statement and returns its complete owned
    /// result, or one typed rejection with no partial result.
    ///
    /// `build` is called inside an admission with that admission's runtime
    /// context and an executor; it builds its plan in the context's query
    /// memory and passes it to `executor.run`. It may be called more than
    /// once, always from scratch: once under a read admission, and again
    /// under the writer admission (and once per checkpoint retry there) when
    /// the plan's classification writes.
    ///
    /// A searching read plan invokes `search` once per eager call; a
    /// searching plan without an adapter is refused as an invalid plan.
    pub(crate) fn execute_graph_query<S, B>(
        &self,
        control: &QueryControl,
        options: &GraphQueryOptions,
        search: Option<&mut S>,
        mut build: B,
    ) -> Result<CompletedGraphResult, GraphQueryError>
    where
        S: for<'v, 'm, 'g> SearchAdapter<'v, 'm, 'g>,
        B: for<'x, 'w, 'i, 'lease, 'm, 'g> FnMut(
            &mut RuntimeContext<'lease, 'm, 'g>,
            GraphQueryExecutor<'x, 'w, 'i, 'lease, 'm, 'g>,
        ) -> Result<Executed, GraphQueryError>,
    {
        let read = self.with_native_read(
            control,
            options.limits,
            options.memory_limit,
            options.source_slots,
            ReadStatement {
                options,
                search,
                build: &mut build,
            },
        )?;
        match read? {
            ReadRan::Done(result) => return Ok(result),
            ReadRan::NeedsWrite => {}
        }
        let (result, _) = self.with_native_mutation_settled(
            control,
            options.limits,
            options.memory_limit,
            options.source_slots,
            options.lazy_targets,
            options.overlay_capacity,
            options.image_capacity,
            WriteStatement {
                options,
                build: &mut build,
            },
            |unsettled: UnsettledWriteResult, receipts, changed| {
                unsettled.settle(receipts, changed)
            },
        )?;
        Ok(result)
    }

    /// [`Store::execute_graph_query`] for a statement that does not search.
    /// A searching plan is refused as an invalid plan.
    ///
    /// This is the internal seam `zeppelin-embed-cypher` compiles into; it is
    /// not the release graph API, which ZE-66's `GraphStore` owns.
    #[doc(hidden)]
    pub fn execute_graph_statement<B>(
        &self,
        control: &QueryControl,
        options: &GraphQueryOptions,
        build: B,
    ) -> Result<CompletedGraphResult, GraphQueryError>
    where
        B: for<'x, 'w, 'i, 'lease, 'm, 'g> FnMut(
            &mut RuntimeContext<'lease, 'm, 'g>,
            GraphQueryExecutor<'x, 'w, 'i, 'lease, 'm, 'g>,
        ) -> Result<Executed, GraphQueryError>,
    {
        self.execute_graph_query(control, options, None::<&mut NoSearch>, build)
    }

    /// Creates a new native graph store with no document embedding tower,
    /// for tests outside this crate. The release lifecycle API and its typed
    /// error belong to ZE-66's `GraphStore`, not to this helper.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn create_graph_store(
        path: impl AsRef<std::path::Path>,
        options: crate::lifecycle::OpenOptions,
    ) -> Result<Self, GraphQueryError> {
        Ok(Self::create_native_graph(path, options, None)?)
    }

    /// Creates a native graph store using the supplied test filesystem and clock.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn create_graph_store_with_test_dependencies(
        path: impl AsRef<std::path::Path>,
        options: crate::lifecycle::OpenOptions,
        dependencies: crate::lifecycle::StoreTestDependencies,
    ) -> Result<Self, GraphQueryError> {
        Ok(Self::create_native_graph_with_infrastructure(
            path,
            options,
            None,
            dependencies.vfs,
            dependencies.clock,
            &mut crate::property_graph::storage::allocation::OsEntropy,
        )?)
    }

    /// Opens a native graph store using the supplied test filesystem and clock.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn open_graph_store_with_test_dependencies(
        path: impl AsRef<std::path::Path>,
        options: crate::lifecycle::OpenOptions,
        dependencies: crate::lifecycle::StoreTestDependencies,
    ) -> Result<Self, GraphQueryError> {
        Ok(Self::open_native_graph_with_infrastructure(
            path,
            options,
            None,
            dependencies.vfs,
            dependencies.clock,
        )?)
    }

    /// Opens an existing native graph store with no document embedding tower,
    /// for tests outside this crate (see [`Store::create_graph_store`]).
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn open_graph_store(
        path: impl AsRef<std::path::Path>,
        options: crate::lifecycle::OpenOptions,
    ) -> Result<Self, GraphQueryError> {
        Ok(Self::open_native_graph(path, options, None)?)
    }
}
