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
use crate::property_graph::query::pattern::{
    PatternCapacity, SearchAdapter, SearchHit, SearchInvocation,
};
use crate::property_graph::query::plan::{GraphPlan, ParameterBinding, PlanError};
use crate::property_graph::query::resources::{QueryArena, QueryInputs, RetentionInventory};
use crate::property_graph::query::runtime::{
    ExecutionCapacity, NativeExecutionError, RuntimeContext, RuntimeLimits,
};
use crate::property_graph::staging::{GraphBatchReadView, StatementImages, WriteControl};
use crate::property_graph::storage::GraphReadView;
use crate::property_graph::storage::tree::directory::TreeError;
use std::cell::Cell;

/// Every explicit limit and capacity one statement is admitted with.
#[derive(Clone, Copy)]
pub(crate) struct GraphQueryOptions {
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

/// A validated plan and the owners that retain its backing, built by the
/// caller inside the admitted query memory.
pub(crate) struct GraphQuery<'q, 'plan, 'facts, 'a> {
    pub(crate) plan: &'q GraphPlan<'plan, 'facts>,
    /// The actual owners of every span the plan borrows, including its facts.
    pub(crate) inventory: RetentionInventory<'q, 'a>,
    pub(crate) bindings: &'q [ParameterBinding<'q>],
    /// One name per root output column, in column order.
    pub(crate) columns: &'q [GraphName<'q>],
}

/// Proof that a builder handed its plan to its executor. It carries nothing:
/// the result stays with the seam.
#[must_use]
pub(crate) struct Executed(());

/// The type of an absent search adapter. It has no values.
pub(crate) enum NoSearch {}

impl<'v, 'm, 'g> SearchAdapter<'v, 'm, 'g> for NoSearch {
    fn search(
        &mut self,
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
pub(crate) struct GraphQueryExecutor<'x, 'w, 'i, 'lease, 'm, 'g> {
    admission: Admission<'w, 'i, 'lease, 'm, 'g>,
    options: &'x GraphQueryOptions,
    ran: &'x Cell<Ran<'w>>,
}

impl<'w, 'lease, 'm, 'g> GraphQueryExecutor<'_, 'w, '_, 'lease, 'm, 'g> {
    /// Admits `query` against its retained owners and runs it.
    pub(crate) fn run(
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
}
