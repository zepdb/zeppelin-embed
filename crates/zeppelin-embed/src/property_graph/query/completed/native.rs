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
    NativePattern, PatternCapacity, SearchAdapter, SearchReports, SearchScope,
};
use crate::property_graph::query::plan::{ParameterBinding, PlanNodeId};
use crate::property_graph::query::resources::{QueryArena, QueryMemory, RuntimePlan};
use crate::property_graph::query::runtime::{
    Completion, ExecutionCapacity, FrozenOutput, NativeExecutionError, PreparedRows, PullOperator,
    PullState, RowBatch, RuntimeContext, RuntimeError, RuntimeFailure, RuntimeInstanceId,
    execute_in,
};
use crate::property_graph::storage::GraphReadView;

mod entities;
#[cfg(any(test, feature = "test-support"))]
pub(crate) mod test_support;
mod values;

#[cfg(test)]
mod search_tests;
#[cfg(test)]
mod tests;

#[derive(Debug)]
pub(super) enum NativeResultError {
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
            outcome: Outcome::Read,
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

struct NativeCompletion<'a, 's, 'lease, 'm, 'g, C> {
    view: &'a GraphReadView<'s, 'lease, 'm, 'g>,
    owner: NativeOwner<'m, 'g>,
    columns: &'a [GraphName<'a>],
    kinds: &'a [crate::property_graph::query::plan::ValueKinds],
    /// The eager reports recorded at search time, and how many calls the
    /// plan has. Without a search scope there are none to copy.
    reports: Option<(&'a SearchReports, usize)>,
    before_copy: C,
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
        (self.before_copy)(NativeCompletionStage::BeforeStaging, context.counters())?;
        if rows.columns() != self.columns.len() {
            return Err(NativeResultError::Completed(CompletedError::Shape));
        }
        let mut sizes = values::measure_rows(rows, context)?;
        sizes.columns = self.columns.len();
        for column in self.columns {
            sizes.bytes = sizes
                .bytes
                .checked_add(column.as_str().len())
                .ok_or(NativeResultError::Completed(CompletedError::Limit))?;
        }
        let (node_occurrences, relationship_occurrences) =
            values::collect_entity_ids(rows, sizes, context)?;
        let node_ids = entities::sort_unique(node_occurrences, context)?;
        let relationship_ids = entities::sort_unique(relationship_occurrences, context)?;
        entities::measure_entities(
            self.view,
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
            rows: u32::try_from(rows.rows())
                .map_err(|_| NativeResultError::Completed(CompletedError::Limit))?,
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
            node_ids.as_slice(),
            relationship_ids.as_slice(),
            sizes.name_scratch,
            &mut staging,
            context,
        )?;
        values::fill_rows(
            rows,
            node_ids.as_slice(),
            relationship_ids.as_slice(),
            &mut staging,
            context,
        )?;
        (self.before_copy)(NativeCompletionStage::BeforeDestination, context.counters())?;
        let prepared = PreparedGraphResult::copy_from(&staging, context)
            .map_err(NativeResultError::Completed)?;
        let represented = prepared.represented_bytes();
        drop(staging);
        FrozenOutput::new(prepared, rows.rows(), represented, 0).map_err(NativeResultError::from)
    }
}

#[allow(clippy::too_many_arguments, clippy::result_large_err)]
pub(super) fn execute_native_result<'s, 'r, 'plan, 'lease, 'm, 'g>(
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
