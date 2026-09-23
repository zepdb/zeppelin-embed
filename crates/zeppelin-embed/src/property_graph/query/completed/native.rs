#![allow(
    dead_code,
    reason = "crate-private native completion is composed by focused graph consumers"
)]
#![allow(
    clippy::result_large_err,
    reason = "native and completed typed causes remain unboxed and allocation-free"
)]

use super::{
    Column, CompletedError, Outcome, Pools, PreparedGraphResult, ResultInput, ResultSource, Span,
};
use crate::property_graph::GraphName;
use crate::property_graph::query::pattern::{
    MutationScope, NativePattern, PatternCapacity, SearchAdapter, SearchReports, SearchScope,
};
use crate::property_graph::query::plan::{OperatorKind, ParameterBinding, PlanNodeId};
use crate::property_graph::query::resources::{QueryArena, QueryMemory, RuntimePlan};
use crate::property_graph::query::runtime::{
    Completion, ExecutionCapacity, FrozenOutput, NativeExecutionError, PreparedRows, PullOperator,
    PullState, RowBatch, RuntimeContext, RuntimeError, RuntimeFailure, RuntimeInstanceId,
    execute_in,
};
use crate::property_graph::staging::{GraphBatchReadView, StatementImages};
use crate::property_graph::storage::GraphReadView;
use std::cell::Cell;

mod entities;
mod entry;
#[cfg(any(test, feature = "test-support"))]
pub(crate) mod entry_probe;
mod error;
#[cfg(any(test, feature = "test-support"))]
pub(crate) mod test_support;
mod values;

pub use entry::{Executed, GraphQuery, GraphQueryExecutor, GraphQueryOptions};
pub(crate) use error::{GraphQueryCause, native_graph_error_kind};
pub use error::{GraphQueryError, GraphQueryErrorKind};

#[cfg(test)]
mod entry_tests;
#[cfg(test)]
mod lifetime_tests;
#[cfg(test)]
mod search_tests;
#[cfg(test)]
mod tests;

#[derive(Debug)]
pub(crate) enum NativeResultError {
    Native(NativeExecutionError),
    Completed(CompletedError),
}

impl From<RuntimeError> for NativeResultError {
    fn from(error: RuntimeError) -> Self {
        Self::Native(error.into())
    }
}

impl std::fmt::Display for NativeResultError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Native(error) => error.fmt(formatter),
            Self::Completed(error) => write!(formatter, "{error:?}"),
        }
    }
}

impl From<NativeResultError> for crate::lifecycle::native_graph::NativeMutationError {
    fn from(error: NativeResultError) -> Self {
        match error {
            NativeResultError::Native(error) => Self::Execution(error),
            NativeResultError::Completed(error) => Self::Completed(error),
        }
    }
}

impl std::error::Error for NativeResultError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Native(error) => Some(error),
            Self::Completed(_) => None,
        }
    }
}

struct NativeSource<O, H> {
    inner: O,
    after_pull: H,
}

#[derive(Clone, Copy, Default)]
struct Sizes {
    values: usize,
    bytes: usize,
    columns: usize,
    cells: usize,
    children: usize,
    names: usize,
    properties: usize,
    nodes: usize,
    relationships: usize,
    node_occurrences: usize,
    relationship_occurrences: usize,
    name_scratch: usize,
}

impl<'v, 'm, 'g, O, H> PullOperator<'v, 'm, 'g, NativeResultError> for NativeSource<O, H>
where
    O: PullOperator<'v, 'm, 'g, NativeExecutionError>,
    H: FnMut(usize) -> Result<(), NativeResultError>,
{
    fn node(&self) -> PlanNodeId {
        self.inner.node()
    }

    fn prepare_search(
        &mut self,
        node: PlanNodeId,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), NativeResultError> {
        self.inner
            .prepare_search(node, context)
            .map_err(NativeResultError::Native)
    }

    fn pull(
        &mut self,
        context: &mut RuntimeContext<'v, 'm, 'g>,
        output: &mut RowBatch<'v, 'm, 'g>,
    ) -> Result<PullState, NativeResultError> {
        let state = self
            .inner
            .pull(context, output)
            .map_err(NativeResultError::Native)?;
        (self.after_pull)(output.rows())?;
        Ok(state)
    }
}

struct NativeStaging<'v, 'm, 'g> {
    view: &'v crate::property_graph::query::QueryView,
    values: QueryArena<'m, 'g, super::Value>,
    bytes: QueryArena<'m, 'g, u8>,
    columns: QueryArena<'m, 'g, Column>,
    cells: QueryArena<'m, 'g, super::ValueIndex>,
    children: QueryArena<'m, 'g, super::ValueIndex>,
    names: QueryArena<'m, 'g, Span>,
    properties: QueryArena<'m, 'g, super::Property>,
    nodes: QueryArena<'m, 'g, super::Node>,
    relationships: QueryArena<'m, 'g, super::Relationship>,
    vectors: QueryArena<'m, 'g, u32>,
    reports: QueryArena<'m, 'g, super::SearchReport>,
    receipts: QueryArena<'m, 'g, super::Receipt>,
    rows: u32,
    outcome: Outcome,
}

impl NativeStaging<'_, '_, '_> {
    fn pools(&self) -> Pools<'_> {
        Pools {
            values: self.values.as_slice(),
            bytes: self.bytes.as_slice(),
            columns: self.columns.as_slice(),
            cells: self.cells.as_slice(),
            children: self.children.as_slice(),
            names: self.names.as_slice(),
            properties: self.properties.as_slice(),
            nodes: self.nodes.as_slice(),
            relationships: self.relationships.as_slice(),
            vectors: self.vectors.as_slice(),
            reports: self.reports.as_slice(),
            receipts: self.receipts.as_slice(),
        }
    }
}

impl ResultSource for NativeStaging<'_, '_, '_> {
    fn result_input(&self) -> Result<ResultInput<'_>, super::SourceError> {
        Ok(ResultInput {
            view: self.view,
            pools: self.pools(),
            rows: self.rows,
            outcome: self.outcome,
        })
    }
}

#[derive(Clone, Copy)]
struct NativeOwner<'m, 'g> {
    query_view: *const crate::property_graph::query::QueryView,
    memory: &'m QueryMemory<'g>,
    runtime: RuntimeInstanceId,
}

impl<'m, 'g> NativeOwner<'m, 'g> {
    fn capture(runtime: &RuntimeContext<'_, 'm, 'g>) -> Self {
        Self {
            query_view: runtime.view(),
            memory: runtime.memory(),
            runtime: runtime.identity(),
        }
    }

    fn matches(&self, context: &RuntimeContext<'_, '_, '_>) -> bool {
        std::ptr::eq(self.query_view, context.view())
            && std::ptr::eq(self.memory, context.memory())
            && self.runtime == context.identity()
    }
}

#[cfg(test)]
#[derive(Clone, Copy)]
struct NativeOwnerFingerprint {
    query_view: usize,
    memory: usize,
    runtime: u64,
}

#[cfg(test)]
impl NativeOwnerFingerprint {
    fn capture(runtime: &RuntimeContext<'_, '_, '_>) -> Self {
        Self {
            query_view: runtime.view() as *const _ as usize,
            memory: runtime.memory() as *const _ as usize,
            runtime: runtime.identity().get(),
        }
    }

    fn validate(&self, runtime: &RuntimeContext<'_, '_, '_>) -> Result<(), NativeResultError> {
        if self.query_view == runtime.view() as *const _ as usize
            && self.memory == runtime.memory() as *const _ as usize
            && self.runtime == runtime.identity().get()
        {
            Ok(())
        } else {
            Err(NativeResultError::Completed(CompletedError::Source(
                super::SourceError::ForeignView,
            )))
        }
    }
}

/// Where a write statement's overlay waits between the pattern that staged
/// into it and the completion that copies entities through it. The drain
/// holds both at once, so the overlay moves by value: the source parks it
/// here when its last pull is done, and the completion borrows it back.
type OverlayHandoff<'s> = Cell<Option<GraphBatchReadView<'s, 'static>>>;

struct NativeCompletion<'a, 's, 'lease, 'm, 'g, C> {
    view: &'a GraphReadView<'s, 'lease, 'm, 'g>,
    owner: NativeOwner<'m, 'g>,
    columns: &'a [GraphName<'a>],
    kinds: &'a [crate::property_graph::query::plan::ValueKinds],
    /// The eager reports recorded at search time, and how many calls the
    /// plan has. Without a search scope there are none to copy.
    reports: Option<(&'a SearchReports, usize)>,
    before_copy: C,
    /// Present only for a write statement: every entity is then copied from
    /// the statement's staged image first and the admitted view second.
    handoff: Option<&'a OverlayHandoff<'s>>,
    /// A write statement without RETURN: its rows are driven for their
    /// writes only, and the result has no columns and no rows.
    no_return: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum NativeCompletionStage {
    BeforeStaging,
    BeforeDestination,
}

impl<'a, 's, 'lease, 'm, 'g, C> NativeCompletion<'a, 's, 'lease, 'm, 'g, C> {
    fn new(
        view: &'a GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
        columns: &'a [GraphName<'a>],
        kinds: &'a [crate::property_graph::query::plan::ValueKinds],
        reports: Option<(&'a SearchReports, usize)>,
        before_copy: C,
    ) -> Result<Self, NativeExecutionError> {
        view.validate_expression_owner(runtime)?;
        if columns.len() != kinds.len() {
            return Err(RuntimeError::Batch.into());
        }
        Ok(Self {
            view,
            owner: NativeOwner::capture(runtime),
            columns,
            kinds,
            reports,
            before_copy,
            handoff: None,
            no_return: false,
        })
    }
}

impl<'a, 's, 'lease, 'm, 'g, C> Completion<'m, 'g, NativeResultError>
    for NativeCompletion<'a, 's, 'lease, 'm, 'g, C>
where
    C: FnMut(
        NativeCompletionStage,
        crate::property_graph::query::runtime::WorkCounters,
    ) -> Result<(), NativeResultError>,
{
    type Output = PreparedGraphResult<'m, 'g>;

    fn complete<'v>(
        &mut self,
        rows: &PreparedRows<'v, 'm, 'g>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<Self::Output>, NativeResultError> {
        if !self.owner.matches(context) {
            return Err(NativeResultError::Completed(CompletedError::Source(
                super::SourceError::ForeignView,
            )));
        }
        let Some(handoff) = self.handoff else {
            return self.freeze(rows, context, None, Outcome::Read);
        };
        // The source parks the overlay only once its last pull is done.
        let mut overlay = handoff
            .take()
            .ok_or(NativeResultError::Native(RuntimeError::Batch.into()))?;
        // Copying precedes the commit, so the only outcome it can prove is
        // none; the commit tail settles the real one.
        let frozen = self.freeze(rows, context, Some(&mut overlay), Outcome::NoOp);
        handoff.set(Some(overlay));
        frozen
    }
}

impl<'a, 's, 'lease, 'm, 'g, C> NativeCompletion<'a, 's, 'lease, 'm, 'g, C>
where
    C: FnMut(
        NativeCompletionStage,
        crate::property_graph::query::runtime::WorkCounters,
    ) -> Result<(), NativeResultError>,
{
    fn freeze<'v>(
        &mut self,
        rows: &PreparedRows<'v, 'm, 'g>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
        mut overlay: Option<&mut GraphBatchReadView<'s, 'static>>,
        outcome: Outcome,
    ) -> Result<FrozenOutput<PreparedGraphResult<'m, 'g>>, NativeResultError> {
        (self.before_copy)(NativeCompletionStage::BeforeStaging, context.counters())?;
        let returned = (!self.no_return).then_some(rows);
        if returned.is_some_and(|rows| rows.columns() != self.columns.len()) {
            return Err(NativeResultError::Completed(CompletedError::Shape));
        }
        let row_count = returned.map_or(0, PreparedRows::rows);
        let mut sizes = match returned {
            Some(rows) => values::measure_rows(rows, context)?,
            None => Sizes::default(),
        };
        sizes.columns = self.columns.len();
        for column in self.columns {
            sizes.bytes = sizes
                .bytes
                .checked_add(column.as_str().len())
                .ok_or(NativeResultError::Completed(CompletedError::Limit))?;
        }
        let (node_occurrences, relationship_occurrences) = match returned {
            Some(rows) => values::collect_entity_ids(rows, sizes, context)?,
            None => (
                QueryArena::new(context.memory(), 0)
                    .map_err(CompletedError::from)
                    .map_err(NativeResultError::Completed)?,
                QueryArena::new(context.memory(), 0)
                    .map_err(CompletedError::from)
                    .map_err(NativeResultError::Completed)?,
            ),
        };
        let node_ids = entities::sort_unique(node_occurrences, context)?;
        let relationship_ids = entities::sort_unique(relationship_occurrences, context)?;
        entities::measure_entities(
            self.view,
            overlay.as_deref_mut(),
            node_ids.as_slice(),
            relationship_ids.as_slice(),
            &mut sizes,
            context,
        )?;
        let mut staging = NativeStaging {
            view: context.view(),
            values: QueryArena::new(context.memory(), sizes.values)
                .map_err(CompletedError::from)
                .map_err(NativeResultError::Completed)?,
            bytes: QueryArena::new(context.memory(), sizes.bytes)
                .map_err(CompletedError::from)
                .map_err(NativeResultError::Completed)?,
            columns: QueryArena::new(context.memory(), sizes.columns)
                .map_err(CompletedError::from)
                .map_err(NativeResultError::Completed)?,
            cells: QueryArena::new(context.memory(), sizes.cells)
                .map_err(CompletedError::from)
                .map_err(NativeResultError::Completed)?,
            children: QueryArena::new(context.memory(), sizes.children)
                .map_err(CompletedError::from)
                .map_err(NativeResultError::Completed)?,
            names: QueryArena::new(context.memory(), sizes.names)
                .map_err(CompletedError::from)
                .map_err(NativeResultError::Completed)?,
            properties: QueryArena::new(context.memory(), sizes.properties)
                .map_err(CompletedError::from)
                .map_err(NativeResultError::Completed)?,
            nodes: QueryArena::new(context.memory(), sizes.nodes)
                .map_err(CompletedError::from)
                .map_err(NativeResultError::Completed)?,
            relationships: QueryArena::new(context.memory(), sizes.relationships)
                .map_err(CompletedError::from)
                .map_err(NativeResultError::Completed)?,
            vectors: QueryArena::new(context.memory(), 0)
                .map_err(CompletedError::from)
                .map_err(NativeResultError::Completed)?,
            reports: QueryArena::new(context.memory(), self.reports.map_or(0, |(_, calls)| calls))
                .map_err(CompletedError::from)
                .map_err(NativeResultError::Completed)?,
            receipts: QueryArena::new(context.memory(), 0)
                .map_err(CompletedError::from)
                .map_err(NativeResultError::Completed)?,
            rows: u32::try_from(row_count)
                .map_err(|_| NativeResultError::Completed(CompletedError::Limit))?,
            outcome,
        };
        for (column, kinds) in self.columns.iter().zip(self.kinds.iter().copied()) {
            let name =
                values::append_column_bytes(column.as_str().as_bytes(), &mut staging, context)?;
            staging
                .columns
                .push(Column { name, kinds })
                .map_err(CompletedError::from)
                .map_err(NativeResultError::Completed)?;
        }
        if let Some((reports, calls)) = self.reports {
            reports.copy_into(calls, &mut staging.reports)?;
        }
        entities::fill_entities(
            self.view,
            overlay,
            node_ids.as_slice(),
            relationship_ids.as_slice(),
            sizes.name_scratch,
            &mut staging,
            context,
        )?;
        if let Some(rows) = returned {
            values::fill_rows(
                rows,
                node_ids.as_slice(),
                relationship_ids.as_slice(),
                &mut staging,
                context,
            )?;
        }
        (self.before_copy)(NativeCompletionStage::BeforeDestination, context.counters())?;
        let prepared = PreparedGraphResult::copy_from(&staging, context)
            .map_err(NativeResultError::Completed)?;
        let represented = prepared.represented_bytes();
        drop(staging);
        // The driver accounts every prepared row, including the rows a
        // statement without RETURN drove only for their writes.
        FrozenOutput::new(prepared, rows.rows(), represented, 0).map_err(NativeResultError::from)
    }
}

#[allow(clippy::too_many_arguments, clippy::result_large_err)]
pub(crate) fn execute_native_result<'s, 'r, 'plan, 'lease, 'm, 'g>(
    view: &'s GraphReadView<'s, 'lease, 'm, 'g>,
    runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    plan: &'r RuntimePlan<'r, 'plan, 'r, 'm, 'g, 'r>,
    bindings: &[ParameterBinding<'_>],
    column_names: &[GraphName<'_>],
    pattern_capacity: PatternCapacity,
    execution_capacity: ExecutionCapacity,
) -> Result<super::CompletedGraphResult, RuntimeFailure<NativeResultError>> {
    execute_native_result_with(
        view,
        runtime,
        plan,
        bindings,
        column_names,
        pattern_capacity,
        execution_capacity,
        None,
        |_| Ok(()),
        |_, _| Ok(()),
    )
}

/// Executes a read plan whose eager `Search` calls invoke `adapter` once each,
/// in call order, and copies every call's report into the completed result.
#[allow(clippy::too_many_arguments, clippy::result_large_err)]
pub(super) fn execute_native_search_result<'s, 'r, 'plan, 'lease, 'm, 'g>(
    view: &'s GraphReadView<'s, 'lease, 'm, 'g>,
    runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    plan: &'r RuntimePlan<'r, 'plan, 'r, 'm, 'g, 'r>,
    bindings: &[ParameterBinding<'_>],
    column_names: &[GraphName<'_>],
    pattern_capacity: PatternCapacity,
    execution_capacity: ExecutionCapacity,
    adapter: &mut dyn SearchAdapter<'lease, 'm, 'g>,
) -> Result<super::CompletedGraphResult, RuntimeFailure<NativeResultError>> {
    execute_native_result_with(
        view,
        runtime,
        plan,
        bindings,
        column_names,
        pattern_capacity,
        execution_capacity,
        Some(adapter),
        |_| Ok(()),
        |_, _| Ok(()),
    )
}

/// The root's output value kinds, one per column, in column order.
#[allow(clippy::result_large_err)]
fn output_kinds<'m, 'g>(
    plan: &RuntimePlan<'_, '_, '_, 'm, 'g, '_>,
    runtime: &mut RuntimeContext<'_, 'm, 'g>,
) -> Result<
    QueryArena<'m, 'g, crate::property_graph::query::plan::ValueKinds>,
    RuntimeFailure<NativeResultError>,
> {
    let root = plan.plan().description().root;
    let facts = plan.plan().facts(root).ok_or_else(|| RuntimeFailure {
        operator: root,
        error: NativeResultError::Native(RuntimeError::Batch.into()),
        counters: runtime.counters(),
    })?;
    let mut kinds = QueryArena::new(runtime.memory(), facts.width())
        .map_err(RuntimeError::Memory)
        .map_err(|error| RuntimeFailure {
            operator: root,
            error: NativeResultError::Native(error.into()),
            counters: runtime.counters(),
        })?;
    for ordinal in 0..facts.width() {
        let (_, value_kinds) = facts.slot_at(ordinal).ok_or_else(|| RuntimeFailure {
            operator: root,
            error: NativeResultError::Native(RuntimeError::Batch.into()),
            counters: runtime.counters(),
        })?;
        kinds
            .push(value_kinds)
            .map_err(RuntimeError::Memory)
            .map_err(|error| RuntimeFailure {
                operator: root,
                error: NativeResultError::Native(error.into()),
                counters: runtime.counters(),
            })?;
    }
    Ok(kinds)
}

#[allow(clippy::too_many_arguments, clippy::result_large_err)]
fn execute_native_result_with<'s, 'r, 'plan, 'lease, 'm, 'g, H, C>(
    view: &'s GraphReadView<'s, 'lease, 'm, 'g>,
    runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    plan: &'r RuntimePlan<'r, 'plan, 'r, 'm, 'g, 'r>,
    bindings: &[ParameterBinding<'_>],
    column_names: &[GraphName<'_>],
    pattern_capacity: PatternCapacity,
    execution_capacity: ExecutionCapacity,
    search: Option<&mut dyn SearchAdapter<'lease, 'm, 'g>>,
    after_pull: H,
    before_copy: C,
) -> Result<super::CompletedGraphResult, RuntimeFailure<NativeResultError>>
where
    H: FnMut(usize) -> Result<(), NativeResultError>,
    C: FnMut(
        NativeCompletionStage,
        crate::property_graph::query::runtime::WorkCounters,
    ) -> Result<(), NativeResultError>,
{
    let root = plan.plan().description().root;
    let kinds = output_kinds(plan, runtime)?;
    let reports = SearchReports::new();
    let calls = plan.plan().description().eager_searches.len();
    let searched = search.is_some();
    let inner = match search {
        Some(adapter) => NativePattern::new_with_search(
            view,
            plan,
            root,
            bindings,
            pattern_capacity,
            runtime,
            SearchScope::new(adapter, &reports),
        ),
        None => NativePattern::new(view, plan, root, bindings, pattern_capacity, runtime),
    }
    .map_err(|error| RuntimeFailure {
        operator: root,
        error: NativeResultError::Native(error),
        counters: runtime.counters(),
    })?;
    let mut source = NativeSource { inner, after_pull };
    let mut completion = NativeCompletion::new(
        view,
        runtime,
        column_names,
        kinds.as_slice(),
        searched.then_some((&reports, calls)),
        before_copy,
    )
    .map_err(|error| RuntimeFailure {
        operator: root,
        error: NativeResultError::Native(error),
        counters: runtime.counters(),
    })?;
    let execution = execute_in(
        runtime,
        plan,
        &mut source,
        &mut completion,
        execution_capacity,
    )?;
    Ok(execution
        .output
        .detach(execution.counters, execution.peak_query_bytes))
}

/// The write statement's source: its `NativePattern`, which stages into the
/// statement overlay, parked into `handoff` once its last pull is done.
struct WriteSource<'c, 's, 'r, 'plan, 'v, 'm, 'g, 'i, 'q> {
    root: PlanNodeId,
    pattern: Option<NativePattern<'s, 'r, 'plan, 'v, 'm, 'g, 'i, 'q>>,
    handoff: &'c OverlayHandoff<'s>,
}

impl<'s, 'r, 'plan, 'v, 'm, 'g, 'i, 'q> PullOperator<'v, 'm, 'g, NativeResultError>
    for WriteSource<'_, 's, 'r, 'plan, 'v, 'm, 'g, 'i, 'q>
{
    fn node(&self) -> PlanNodeId {
        self.root
    }

    fn prepare_search(
        &mut self,
        node: PlanNodeId,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), NativeResultError> {
        self.pattern
            .as_mut()
            .ok_or(NativeResultError::Native(RuntimeError::Batch.into()))?
            .prepare_search(node, context)
            .map_err(NativeResultError::Native)
    }

    /// A pull after the last one finds no pattern and fails loudly, rather
    /// than evaluating anything without the statement's overlay.
    fn pull(
        &mut self,
        context: &mut RuntimeContext<'v, 'm, 'g>,
        output: &mut RowBatch<'v, 'm, 'g>,
    ) -> Result<PullState, NativeResultError> {
        let state = self
            .pattern
            .as_mut()
            .ok_or(NativeResultError::Native(RuntimeError::Batch.into()))?
            .pull(context, output)
            .map_err(NativeResultError::Native)?;
        if state == PullState::Done {
            let pattern = self
                .pattern
                .take()
                .ok_or(NativeResultError::Native(RuntimeError::Batch.into()))?;
            self.handoff.set(pattern.into_mutation());
        }
        Ok(state)
    }
}

/// Runs one write statement's plan under its writer scope and copies its
/// complete result before anything commits.
///
/// Every entity the result names is copied from the image this statement
/// staged for it first, and from the admitted view only when nothing is
/// staged: a returned entity shows its SET values, a created one exists only
/// in its staged image, and one this statement deleted is refused with a
/// typed `Deleted` rather than copied from the view. Every fallible copy
/// finishes here; any failure drops the whole result and the overlay with it,
/// so nothing partial escapes and nothing can commit.
///
/// The result is returned unsettled beside the overlay, for the admission's
/// commit tail: its outcome and each changed entity's revision and generation
/// are stamped by `UnsettledWriteResult::settle` once that tail has run.
///
/// A rejection from inside the driver keeps its failing operator and the
/// work done so far, exactly as a read's `RuntimeFailure` does.
#[allow(
    clippy::too_many_arguments,
    reason = "all authentic native and writer owners stay explicit"
)]
fn execute_native_mutation_diagnosed<'w, 'r, 'plan, 'lease, 'm, 'g, 'i>(
    view: &'w GraphReadView<'w, 'lease, 'm, 'g>,
    runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    plan: &'r RuntimePlan<'r, 'plan, 'r, 'm, 'g, 'r>,
    bindings: &[ParameterBinding<'_>],
    column_names: &[GraphName<'_>],
    pattern_capacity: PatternCapacity,
    execution_capacity: ExecutionCapacity,
    overlay: GraphBatchReadView<'w, 'static>,
    images: &'w StatementImages<'i>,
) -> Result<(super::UnsettledWriteResult, GraphBatchReadView<'w, 'static>), GraphQueryError> {
    let root = plan.plan().description().root;
    // A statement whose last clause writes has no RETURN: its root is the
    // Mutate operator itself, and its result has no columns and no rows.
    let no_return = matches!(
        plan.plan()
            .description()
            .operators
            .get(root.0 as usize)
            .map(|operator| operator.kind),
        Some(OperatorKind::Mutate(_))
    );
    let kinds = if no_return {
        QueryArena::new(runtime.memory(), 0)?
    } else {
        output_kinds(plan, runtime)?
    };
    let handoff: OverlayHandoff<'w> = Cell::new(None);
    let pattern = NativePattern::new_with_mutation(
        view,
        plan,
        root,
        bindings,
        pattern_capacity,
        runtime,
        MutationScope::new(overlay, images),
    )
    .map_err(|error| RuntimeFailure {
        operator: root,
        error,
        counters: runtime.counters(),
    })?;
    let mut source = WriteSource {
        root,
        pattern: Some(pattern),
        handoff: &handoff,
    };
    let mut completion = NativeCompletion::new(
        view,
        runtime,
        column_names,
        kinds.as_slice(),
        None,
        |_, _| Ok(()),
    )
    .map_err(|error| RuntimeFailure {
        operator: root,
        error,
        counters: runtime.counters(),
    })?;
    completion.handoff = Some(&handoff);
    completion.no_return = no_return;
    let execution = execute_in(
        runtime,
        plan,
        &mut source,
        &mut completion,
        execution_capacity,
    )?;
    let overlay = handoff
        .take()
        .ok_or(NativeResultError::Native(RuntimeError::Batch.into()))?;
    Ok((
        super::UnsettledWriteResult(
            execution
                .output
                .detach(execution.counters, execution.peak_query_bytes),
        ),
        overlay,
    ))
}

/// The S2 write path below the seam, in the admission's own error, for the
/// tests that drive `with_native_mutation_settled` directly.
#[cfg(test)]
#[allow(
    clippy::too_many_arguments,
    reason = "all authentic native and writer owners stay explicit"
)]
pub(crate) fn execute_native_mutation_result<'w, 'r, 'plan, 'lease, 'm, 'g, 'i>(
    view: &'w GraphReadView<'w, 'lease, 'm, 'g>,
    runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    plan: &'r RuntimePlan<'r, 'plan, 'r, 'm, 'g, 'r>,
    bindings: &[ParameterBinding<'_>],
    column_names: &[GraphName<'_>],
    pattern_capacity: PatternCapacity,
    execution_capacity: ExecutionCapacity,
    overlay: GraphBatchReadView<'w, 'static>,
    images: &'w StatementImages<'i>,
) -> Result<
    (super::UnsettledWriteResult, GraphBatchReadView<'w, 'static>),
    crate::lifecycle::native_graph::NativeMutationError,
> {
    execute_native_mutation_diagnosed(
        view,
        runtime,
        plan,
        bindings,
        column_names,
        pattern_capacity,
        execution_capacity,
        overlay,
        images,
    )
    .map_err(GraphQueryError::into_mutation)
}

#[cfg(any(test, feature = "test-support"))]
#[allow(clippy::too_many_arguments, clippy::result_large_err)]
fn execute_native_result_observed<'s, 'r, 'plan, 'lease, 'm, 'g, C>(
    view: &'s GraphReadView<'s, 'lease, 'm, 'g>,
    runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    plan: &'r RuntimePlan<'r, 'plan, 'r, 'm, 'g, 'r>,
    bindings: &[ParameterBinding<'_>],
    column_names: &[GraphName<'_>],
    pattern_capacity: PatternCapacity,
    execution_capacity: ExecutionCapacity,
    before_copy: C,
) -> Result<super::CompletedGraphResult, RuntimeFailure<NativeResultError>>
where
    C: FnMut(
        NativeCompletionStage,
        crate::property_graph::query::runtime::WorkCounters,
    ) -> Result<(), NativeResultError>,
{
    execute_native_result_with(
        view,
        runtime,
        plan,
        bindings,
        column_names,
        pattern_capacity,
        execution_capacity,
        None,
        |_| Ok(()),
        before_copy,
    )
}

#[cfg(any(test, feature = "test-support"))]
#[allow(clippy::too_many_arguments, clippy::result_large_err)]
fn execute_native_result_source_observed<'s, 'r, 'plan, 'lease, 'm, 'g, H, C>(
    view: &'s GraphReadView<'s, 'lease, 'm, 'g>,
    runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    plan: &'r RuntimePlan<'r, 'plan, 'r, 'm, 'g, 'r>,
    bindings: &[ParameterBinding<'_>],
    column_names: &[GraphName<'_>],
    pattern_capacity: PatternCapacity,
    execution_capacity: ExecutionCapacity,
    after_pull: H,
    before_copy: C,
) -> Result<super::CompletedGraphResult, RuntimeFailure<NativeResultError>>
where
    H: FnMut(usize) -> Result<(), NativeResultError>,
    C: FnMut(
        NativeCompletionStage,
        crate::property_graph::query::runtime::WorkCounters,
    ) -> Result<(), NativeResultError>,
{
    execute_native_result_with(
        view,
        runtime,
        plan,
        bindings,
        column_names,
        pattern_capacity,
        execution_capacity,
        None,
        after_pull,
        before_copy,
    )
}
