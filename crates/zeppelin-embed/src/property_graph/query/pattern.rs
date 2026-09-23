//! Native physical execution for validated property-graph pattern regions.

use super::expression::{
    ClauseOverlay, ExpressionCapacity, ExpressionError, ExpressionFailure,
    NativeExpressionEvaluator,
};
use super::plan::{
    CompletedEdgePredicate, Direction, EdgePredicate, ExprId, MAX_PLAN_NODES, Mutation,
    OperatorKind, ParameterBinding, PatternId, PlanError, PlanNodeId, Projection, SlotId,
};
use super::relational::{RowOperator, Schema, StorageCapacity};
use super::resources::{QueryArena, RuntimePlan};
use super::runtime::{
    NativeExecutionError, PullOperator, PullState, RowBatch, RuntimeContext, RuntimeError, WorkKind,
};
use super::{Comparison, QueryError, QueryList, QueryValue, QueryView};
use crate::property_graph::catalog::{LabelId, RelTypeId, Symbol, SymbolKind};
use crate::property_graph::staging::{
    GraphBatchReadView, StageError, StatementImages, WriteControl, WritePhase,
};
use crate::property_graph::storage::adjacency::RelationshipRow;
use crate::property_graph::storage::tree::directory::TreeResources;
use crate::property_graph::storage::{
    CursorState, DirectionSelection, ExpandCursor, GraphReadView, LabelSelection, NodeCursor,
    RelationshipTypeSelection,
};
use crate::property_graph::{
    ApplicationKey, EntityId, EntityKind, GraphName, MAX_GRAPH_INPUT_BYTES, NodeId, RelId,
};

mod expand;
mod join;
mod mutation;
mod planner;
pub(super) mod relational;
mod source;

#[cfg(test)]
mod oracle_generation_tests;
#[cfg(test)]
mod oracle_tests;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
#[cfg(test)]
mod tests;

/// Explicit capacities for one native pattern occurrence tree.
#[derive(Clone, Copy)]
pub(crate) struct PatternCapacity {
    pub(crate) rows: StorageCapacity,
    pub(crate) expression: ExpressionCapacity,
}

#[derive(Clone, Copy)]
enum ScalarCell {
    Node(NodeId),
    Relationship(RelId),
}

impl ScalarCell {
    fn value(self, view: &QueryView) -> QueryValue<'_> {
        match self {
            Self::Node(value) => view.node(value),
            Self::Relationship(value) => view.relationship(value),
        }
    }
}

enum ResolvedLabel {
    All,
    Known(LabelId),
    Missing,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct RelationshipUse {
    pattern: PatternId,
    origin: PlanNodeId,
    relationship: RelId,
}

struct PathFrame<'s, 'm, 'g> {
    cursor: ExpandCursor<'s, 'm, 'g>,
}

#[derive(Clone, Copy)]
struct UseSpan {
    start: usize,
    len: usize,
}

struct RetainedRows<'v, 'm, 'g> {
    rows: RowBatch<'v, 'm, 'g>,
    spans: QueryArena<'m, 'g, UseSpan>,
    uses: QueryArena<'m, 'g, RelationshipUse>,
}

impl<'v, 'm, 'g> RetainedRows<'v, 'm, 'g> {
    fn new(
        columns: usize,
        capacity: PatternCapacity,
        context: &RuntimeContext<'v, 'm, 'g>,
    ) -> Result<Self, RuntimeError> {
        let use_capacity = capacity
            .rows
            .rows
            .checked_mul(16)
            .ok_or(RuntimeError::Batch)?;
        Ok(Self {
            rows: RowBatch::storage(
                context,
                columns,
                capacity.rows.rows,
                capacity.rows.payload_bytes,
                capacity.rows.variable,
            )?,
            spans: QueryArena::new(context.memory(), capacity.rows.rows)?,
            uses: QueryArena::new(context.memory(), use_capacity)?,
        })
    }

    fn span(&self, row: usize) -> Result<&[RelationshipUse], RuntimeError> {
        let span = self.spans.as_slice().get(row).ok_or(RuntimeError::Batch)?;
        self.uses
            .as_slice()
            .get(
                span.start
                    ..span
                        .start
                        .checked_add(span.len)
                        .ok_or(RuntimeError::Batch)?,
            )
            .ok_or(RuntimeError::Batch)
    }
}

enum PhysicalState<'s, 'plan, 'v, 'm, 'g> {
    Vacant,
    Anchor {
        source: usize,
        emitted: bool,
    },
    Unit {
        emitted: bool,
    },
    ScanNodes {
        child: usize,
        output: SlotId,
        label: ResolvedLabel,
        cursor: Option<NodeCursor<'s, 'm, 'g>>,
    },
    LookupNode {
        child: usize,
        output: SlotId,
        id: NodeId,
    },
    LookupRelationship {
        child: usize,
        output: SlotId,
        id: RelId,
    },
    LookupKey {
        child: usize,
        output: SlotId,
        namespace: GraphName<'plan>,
        key: ExprId,
        kind: EntityKind,
        value: RowBatch<'v, 'm, 'g>,
    },
    Expand {
        child: usize,
        source: SlotId,
        node: SlotId,
        relationship: SlotId,
        pattern: PatternId,
        direction: Direction,
        all_types: bool,
        types: QueryArena<'m, 'g, RelTypeId>,
        cursor: Option<ExpandCursor<'s, 'm, 'g>>,
        bound: Option<NodeId>,
    },
    BoundedExpand {
        child: usize,
        source: SlotId,
        node: SlotId,
        relationships: SlotId,
        edge_predicate: Option<EdgePredicate>,
        completed_edge_predicate: Option<CompletedEdgePredicate>,
        min: u8,
        max: u8,
        direction: Direction,
        pattern: PatternId,
        all_types: bool,
        types: QueryArena<'m, 'g, RelTypeId>,
        frames: QueryArena<'m, 'g, PathFrame<'s, 'm, 'g>>,
        path: QueryArena<'m, 'g, RelId>,
        nodes: QueryArena<'m, 'g, NodeId>,
        edge_schema: Option<Schema<'m, 'g>>,
        edge_input: Option<RowBatch<'v, 'm, 'g>>,
        completed_schema: Option<Schema<'m, 'g>>,
        completed_input: Option<RowBatch<'v, 'm, 'g>>,
        zero_pending: bool,
        active: bool,
    },
    Join {
        left: usize,
        right: usize,
        predicate: Option<ExprId>,
        shared: QueryArena<'m, 'g, SlotId>,
        plan: planner::JoinPlan,
        build_rows: Option<RetainedRows<'v, 'm, 'g>>,
        bucket_heads: Option<QueryArena<'m, 'g, usize>>,
        next_links: Option<QueryArena<'m, 'g, usize>>,
        build_hashes: Option<QueryArena<'m, 'g, u64>>,
        initialized: bool,
        nested_left_active: bool,
        probe_active: bool,
        next_candidate: usize,
    },
    Optional {
        left: usize,
        right: usize,
        predicate: Option<ExprId>,
        shared: QueryArena<'m, 'g, SlotId>,
        left_active: bool,
        matched: bool,
    },
    Filter {
        child: usize,
        predicate: ExprId,
    },
    Project {
        child: usize,
        projections: &'plan [Projection],
        values: QueryArena<'m, 'g, RowBatch<'v, 'm, 'g>>,
    },
    OffsetLimit {
        child: usize,
        offset: u64,
        limit: Option<u64>,
        remaining_offset: u64,
        remaining_limit: Option<u64>,
    },
    Sort {
        child: usize,
        state: QueryArena<'m, 'g, relational::SortState<'v, 'm, 'g>>,
    },
    Distinct {
        child: usize,
        state: QueryArena<'m, 'g, relational::DistinctState<'v, 'm, 'g>>,
    },
    Eager {
        child: usize,
        state: QueryArena<'m, 'g, mutation::EagerState<'v, 'm, 'g>>,
    },
    Mutate {
        child: usize,
        items: &'plan [Mutation<'plan>],
        state: QueryArena<'m, 'g, mutation::EagerState<'v, 'm, 'g>>,
    },
    Aggregate {
        child: usize,
        state: QueryArena<'m, 'g, relational::AggregateState<'v, 'm, 'g>>,
    },
    Collect {
        child: usize,
    },
}

struct Occurrence<'s, 'plan, 'v, 'm, 'g> {
    node: PlanNodeId,
    schema: Schema<'m, 'g>,
    output: RowBatch<'v, 'm, 'g>,
    uses: QueryArena<'m, 'g, RelationshipUse>,
    state: PhysicalState<'s, 'plan, 'v, 'm, 'g>,
}

struct AnchorBinding<'a> {
    logical: PlanNodeId,
    source: usize,
    parent: Option<&'a AnchorBinding<'a>>,
}

fn anchor_source(bindings: Option<&AnchorBinding<'_>>, node: PlanNodeId) -> Option<usize> {
    let mut current = bindings;
    while let Some(binding) = current {
        if binding.logical == node {
            return Some(binding.source);
        }
        current = binding.parent;
    }
    None
}

/// The writer half of one query-driven mutation statement: the progressive
/// overlay every expression in the statement reads through, and the arena its
/// replacement images are copied into. A pattern built without a scope never
/// admits a `Mutate` occurrence.
pub(crate) struct MutationScope<'s, 'i> {
    overlay: GraphBatchReadView<'s, 'static>,
    images: &'s StatementImages<'i>,
}

impl<'s, 'i> MutationScope<'s, 'i> {
    #[allow(
        dead_code,
        reason = "ZE-52 slice D2 lands the executor; the Cypher statement driver is a later slice"
    )]
    pub(crate) fn new(
        overlay: GraphBatchReadView<'s, 'static>,
        images: &'s StatementImages<'i>,
    ) -> Self {
        Self { overlay, images }
    }
}

/// One bounded physical occurrence tree. A repeated validated DAG node is built
/// as a separate occurrence so cursor position is never accidentally shared.
pub(crate) struct NativePattern<'s, 'r, 'plan, 'v, 'm, 'g, 'i> {
    view: &'s GraphReadView<'s, 'v, 'm, 'g>,
    query_view: &'v QueryView,
    evaluator: NativeExpressionEvaluator<'r, 'plan, 'v, 'm, 'g>,
    occurrences: QueryArena<'m, 'g, Occurrence<'s, 'plan, 'v, 'm, 'g>>,
    root: usize,
    root_node: PlanNodeId,
    schema: Schema<'m, 'g>,
    mutation: Option<MutationScope<'s, 'i>>,
}

impl<'s, 'r, 'plan, 'v, 'm, 'g, 'i> NativePattern<'s, 'r, 'plan, 'v, 'm, 'g, 'i> {
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn cancel_after_expression_polls(
        &mut self,
        polls: usize,
        cancel: crate::lifecycle::CancelToken,
    ) {
        self.evaluator.cancel_after_scratch_polls(polls, cancel);
    }

    /// A read-only occurrence tree. Any `Mutate` operator is refused.
    #[allow(
        clippy::too_many_arguments,
        reason = "all authentic native owners stay explicit"
    )]
    pub(crate) fn new(
        view: &'s GraphReadView<'s, 'v, 'm, 'g>,
        plan: &'r RuntimePlan<'r, 'plan, 'r, 'm, 'g, 'r>,
        root_node: PlanNodeId,
        bindings: &[ParameterBinding<'_>],
        capacity: PatternCapacity,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<Self, NativeExecutionError> {
        Self::build(view, plan, root_node, bindings, capacity, context, None)
    }

    /// An occurrence tree whose every expression reads through `mutation`'s
    /// overlay, and whose `Mutate` occurrences stage into it. Only SET, REMOVE
    /// and label items are admitted; CREATE and DELETE remain refused.
    #[allow(
        clippy::too_many_arguments,
        reason = "all authentic native owners stay explicit"
    )]
    #[allow(
        dead_code,
        reason = "ZE-52 slice D2 lands the executor; the Cypher statement driver is a later slice"
    )]
    pub(crate) fn new_with_mutation(
        view: &'s GraphReadView<'s, 'v, 'm, 'g>,
        plan: &'r RuntimePlan<'r, 'plan, 'r, 'm, 'g, 'r>,
        root_node: PlanNodeId,
        bindings: &[ParameterBinding<'_>],
        capacity: PatternCapacity,
        context: &mut RuntimeContext<'v, 'm, 'g>,
        mutation: MutationScope<'s, 'i>,
    ) -> Result<Self, NativeExecutionError> {
        Self::build(
            view,
            plan,
            root_node,
            bindings,
            capacity,
            context,
            Some(mutation),
        )
    }

    /// Returns the overlay this pattern staged into, for the admission's
    /// final staging step. A read-only pattern returns `None`.
    #[allow(
        dead_code,
        reason = "ZE-52 slice D2 lands the executor; the Cypher statement driver is a later slice"
    )]
    pub(crate) fn into_mutation(self) -> Option<GraphBatchReadView<'s, 'static>> {
        self.mutation.map(|scope| scope.overlay)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "all authentic native owners stay explicit"
    )]
    fn build(
        view: &'s GraphReadView<'s, 'v, 'm, 'g>,
        plan: &'r RuntimePlan<'r, 'plan, 'r, 'm, 'g, 'r>,
        root_node: PlanNodeId,
        bindings: &[ParameterBinding<'_>],
        capacity: PatternCapacity,
        context: &mut RuntimeContext<'v, 'm, 'g>,
        mutation: Option<MutationScope<'s, 'i>>,
    ) -> Result<Self, NativeExecutionError> {
        let description = plan.plan().description();
        let count = occurrence_count(
            description.operators,
            root_node,
            0,
            None,
            mutation.is_some(),
        )?;
        let mut occurrences =
            QueryArena::new(context.memory(), count).map_err(RuntimeError::Memory)?;
        let root = build_occurrence(
            view,
            plan,
            root_node,
            capacity,
            &mut occurrences,
            context,
            None,
        )?;
        if occurrences.len() != count {
            return Err(RuntimeError::Batch.into());
        }
        let evaluator =
            NativeExpressionEvaluator::new(plan, bindings, capacity.expression, context)?;
        let schema = schema_for(plan, root_node, context)?;
        Ok(Self {
            view,
            query_view: context.view(),
            evaluator,
            occurrences,
            root,
            root_node,
            schema,
            mutation,
        })
    }

    fn next_occurrence(
        &mut self,
        index: usize,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<bool, NativeExecutionError> {
        context.checkpoint()?;
        self.output_mut(index)?.clear();
        self.uses_mut(index)?.clear();
        let mut state = {
            let occurrence = self
                .occurrences
                .as_mut_slice()
                .get_mut(index)
                .ok_or(RuntimeError::Batch)?;
            std::mem::replace(&mut occurrence.state, PhysicalState::Vacant)
        };
        let result = (|| match &mut state {
            PhysicalState::Vacant => Err(RuntimeError::Batch.into()),
            PhysicalState::Anchor { source, emitted } => {
                if *emitted {
                    return Ok(false);
                }
                self.copy_output(index, *source, context)?;
                self.copy_uses(index, *source)?;
                *emitted = true;
                Ok(true)
            }
            PhysicalState::Unit { emitted } => {
                if *emitted {
                    return Ok(false);
                }
                self.output_mut(index)?.push_row(&[], context)?;
                *emitted = true;
                Ok(true)
            }
            PhysicalState::ScanNodes {
                child,
                output,
                label,
                cursor,
            } => self.next_scan(index, *child, *output, label, cursor, context),
            PhysicalState::LookupNode { child, output, id } => loop {
                if !self.next_occurrence(*child, context)? {
                    break Ok(false);
                }
                let mut resources = TreeResources::for_query(context)?;
                if self.view.lookup_node(*id, &mut resources)?.is_some() {
                    drop(resources);
                    self.push_extended(
                        index,
                        *child,
                        &[(*output, ScalarCell::Node(*id))],
                        context,
                    )?;
                    break Ok(true);
                }
            },
            PhysicalState::LookupRelationship { child, output, id } => loop {
                if !self.next_occurrence(*child, context)? {
                    break Ok(false);
                }
                let mut resources = TreeResources::for_query(context)?;
                if self
                    .view
                    .lookup_relationship(*id, &mut resources)?
                    .is_some()
                {
                    drop(resources);
                    self.push_extended(
                        index,
                        *child,
                        &[(*output, ScalarCell::Relationship(*id))],
                        context,
                    )?;
                    break Ok(true);
                }
            },
            PhysicalState::LookupKey {
                child,
                output,
                namespace,
                key,
                kind,
                value,
            } => self.next_key(
                index, *child, *output, *namespace, *key, *kind, value, context,
            ),
            PhysicalState::Expand {
                child,
                source,
                node,
                relationship,
                pattern,
                direction,
                all_types,
                types,
                cursor,
                bound,
            } => self.next_expand(
                index,
                *child,
                *source,
                *node,
                *relationship,
                *pattern,
                *direction,
                *all_types,
                types,
                cursor,
                bound,
                context,
            ),
            PhysicalState::BoundedExpand {
                child,
                source,
                node,
                relationships,
                edge_predicate,
                completed_edge_predicate,
                min,
                max,
                direction,
                pattern,
                all_types,
                types,
                frames,
                path,
                nodes,
                edge_schema,
                edge_input,
                completed_schema,
                completed_input,
                zero_pending,
                active,
            } => self.next_bounded_expand(
                index,
                *child,
                *source,
                *node,
                *relationships,
                *edge_predicate,
                *completed_edge_predicate,
                *min,
                *max,
                *direction,
                *pattern,
                *all_types,
                types,
                frames,
                path,
                nodes,
                edge_schema.as_ref(),
                edge_input.as_mut(),
                completed_schema.as_ref(),
                completed_input.as_mut(),
                zero_pending,
                active,
                context,
            ),
            PhysicalState::Join {
                left,
                right,
                predicate,
                shared,
                plan,
                build_rows,
                bucket_heads,
                next_links,
                build_hashes,
                initialized,
                nested_left_active,
                probe_active,
                next_candidate,
            } => self.next_join(
                index,
                *left,
                *right,
                *predicate,
                shared,
                *plan,
                build_rows.as_mut(),
                bucket_heads.as_mut(),
                next_links.as_mut(),
                build_hashes.as_mut(),
                initialized,
                nested_left_active,
                probe_active,
                next_candidate,
                context,
            ),
            PhysicalState::Optional {
                left,
                right,
                predicate,
                shared,
                left_active,
                matched,
            } => self.next_optional(
                index,
                *left,
                *right,
                *predicate,
                shared,
                left_active,
                matched,
                context,
            ),
            PhysicalState::Filter { child, predicate } => loop {
                if !self.next_occurrence(*child, context)? {
                    break Ok(false);
                }
                context.charge(WorkKind::OperatorRows, 1)?;
                context.charge(WorkKind::RowsIn, 1)?;
                let retained = {
                    let child = self
                        .occurrences
                        .as_slice()
                        .get(*child)
                        .ok_or(RuntimeError::Batch)?;
                    evaluate_at(
                        &mut self.evaluator,
                        self.mutation.as_mut(),
                        *predicate,
                        &child.schema,
                        &child.output,
                        0,
                        self.view,
                        context,
                    )?
                    .truth()
                    .map_err(RuntimeError::Value)?
                    .retained()
                };
                if retained {
                    self.copy_output(index, *child, context)?;
                    break Ok(true);
                }
            },
            PhysicalState::Project {
                child,
                projections,
                values,
            } => {
                if !self.next_occurrence(*child, context)? {
                    return Ok(false);
                }
                context.charge(WorkKind::OperatorRows, 1)?;
                context.charge(WorkKind::RowsIn, 1)?;
                for (position, projection) in projections.iter().enumerate() {
                    let value = values
                        .as_mut_slice()
                        .get_mut(position)
                        .ok_or(RuntimeError::Batch)?;
                    value.clear();
                    let evaluated = {
                        let child = self
                            .occurrences
                            .as_slice()
                            .get(*child)
                            .ok_or(RuntimeError::Batch)?;
                        evaluate_at(
                            &mut self.evaluator,
                            self.mutation.as_mut(),
                            projection.expression,
                            &child.schema,
                            &child.output,
                            0,
                            self.view,
                            context,
                        )?
                    };
                    value.push_row(&[evaluated], context)?;
                }
                self.push_projection(index, *projections, values, context)?;
                self.copy_uses(index, *child)?;
                Ok(true)
            }
            PhysicalState::OffsetLimit {
                child,
                remaining_offset,
                remaining_limit,
                ..
            } => self.next_offset_limit(index, *child, remaining_offset, remaining_limit, context),
            PhysicalState::Sort { child, state } => self.next_sort(index, *child, state, context),
            PhysicalState::Distinct { child, state } => {
                self.next_distinct(index, *child, state, context)
            }
            PhysicalState::Eager { child, state } => self.next_eager(index, *child, state, context),
            PhysicalState::Mutate {
                child,
                items,
                state,
            } => self.next_mutate(index, *child, items, state, context),
            PhysicalState::Aggregate { child, state } => {
                self.next_aggregate(index, *child, state, context)
            }
            PhysicalState::Collect { child } => {
                if !self.next_occurrence(*child, context)? {
                    return Ok(false);
                }
                self.copy_output(index, *child, context)?;
                Ok(true)
            }
        })();
        self.occurrence_mut(index)?.state = state;
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn reset_occurrence(
        &mut self,
        index: usize,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        self.output_mut(index)?.clear();
        self.uses_mut(index)?.clear();
        let mut state = {
            let occurrence = self.occurrence_mut(index)?;
            std::mem::replace(&mut occurrence.state, PhysicalState::Vacant)
        };
        let result = (|| {
            match &mut state {
                PhysicalState::Vacant => return Err(RuntimeError::Batch),
                PhysicalState::Anchor { emitted, .. } => *emitted = false,
                PhysicalState::Unit { emitted } => *emitted = false,
                PhysicalState::ScanNodes { child, cursor, .. } => {
                    *cursor = None;
                    self.reset_occurrence(*child, context)?;
                }
                PhysicalState::LookupNode { child, .. }
                | PhysicalState::LookupRelationship { child, .. }
                | PhysicalState::Filter { child, .. }
                | PhysicalState::Collect { child } => self.reset_occurrence(*child, context)?,
                PhysicalState::LookupKey { child, value, .. } => {
                    value.clear();
                    self.reset_occurrence(*child, context)?;
                }
                PhysicalState::Expand {
                    child,
                    cursor,
                    bound,
                    ..
                } => {
                    *cursor = None;
                    *bound = None;
                    self.reset_occurrence(*child, context)?;
                }
                PhysicalState::BoundedExpand {
                    child,
                    frames,
                    path,
                    nodes,
                    edge_input,
                    completed_input,
                    zero_pending,
                    active,
                    ..
                } => {
                    frames.clear();
                    path.clear();
                    nodes.clear();
                    if let Some(input) = edge_input {
                        input.clear();
                    }
                    if let Some(input) = completed_input {
                        input.clear();
                    }
                    *zero_pending = false;
                    *active = false;
                    self.reset_occurrence(*child, context)?;
                }
                PhysicalState::Join {
                    left,
                    right,
                    build_rows,
                    bucket_heads,
                    next_links,
                    build_hashes,
                    initialized,
                    nested_left_active,
                    probe_active,
                    next_candidate,
                    ..
                } => {
                    if let Some(rows) = build_rows {
                        rows.rows.clear();
                        rows.spans.clear();
                        rows.uses.clear();
                    }
                    if let Some(heads) = bucket_heads {
                        heads.clear();
                    }
                    if let Some(links) = next_links {
                        links.clear();
                    }
                    if let Some(hashes) = build_hashes {
                        hashes.clear();
                    }
                    *initialized = false;
                    *nested_left_active = false;
                    *probe_active = false;
                    *next_candidate = usize::MAX;
                    self.reset_occurrence(*left, context)?;
                    self.reset_occurrence(*right, context)?;
                }
                PhysicalState::Optional {
                    left,
                    right,
                    left_active,
                    matched,
                    ..
                } => {
                    *left_active = false;
                    *matched = false;
                    self.reset_occurrence(*left, context)?;
                    self.reset_occurrence(*right, context)?;
                }
                PhysicalState::Project { child, values, .. } => {
                    for value in values.as_mut_slice() {
                        value.clear();
                    }
                    self.reset_occurrence(*child, context)?;
                }
                PhysicalState::OffsetLimit {
                    child,
                    offset,
                    limit,
                    remaining_offset,
                    remaining_limit,
                } => {
                    *remaining_offset = *offset;
                    *remaining_limit = *limit;
                    self.reset_occurrence(*child, context)?;
                }
                PhysicalState::Sort { child, state } => {
                    let state = state
                        .as_mut_slice()
                        .first_mut()
                        .ok_or(RuntimeError::Batch)?;
                    state.reset(context)?;
                    self.reset_occurrence(*child, context)?;
                }
                PhysicalState::Distinct { child, state } => {
                    let state = state
                        .as_mut_slice()
                        .first_mut()
                        .ok_or(RuntimeError::Batch)?;
                    state.reset(context)?;
                    self.reset_occurrence(*child, context)?;
                }
                PhysicalState::Eager { child, state }
                | PhysicalState::Mutate { child, state, .. } => {
                    let state = state
                        .as_mut_slice()
                        .first_mut()
                        .ok_or(RuntimeError::Batch)?;
                    state.reset(context)?;
                    self.reset_occurrence(*child, context)?;
                }
                PhysicalState::Aggregate { child, state } => {
                    let state = state
                        .as_mut_slice()
                        .first_mut()
                        .ok_or(RuntimeError::Batch)?;
                    state.reset(context)?;
                    self.reset_occurrence(*child, context)?;
                }
            }
            Ok(())
        })();
        self.occurrence_mut(index)?.state = state;
        result
    }

    fn next_scan(
        &mut self,
        index: usize,
        child: usize,
        output: SlotId,
        label: &ResolvedLabel,
        cursor: &mut Option<NodeCursor<'s, 'm, 'g>>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<bool, NativeExecutionError> {
        loop {
            if let Some(active) = cursor.as_mut() {
                let mut nodes = [minimum_node()?];
                let (count, state) = self.view.scan_nodes(active, &mut nodes, context)?;
                if state == CursorState::Done {
                    *cursor = None;
                }
                if count == 1 {
                    let node = *nodes.first().ok_or(RuntimeError::Batch)?;
                    self.push_extended(index, child, &[(output, ScalarCell::Node(node))], context)?;
                    return Ok(true);
                }
                if count != 0 {
                    return Err(RuntimeError::Batch.into());
                }
                continue;
            }
            if !self.next_occurrence(child, context)? {
                return Ok(false);
            }
            *cursor = Some(match label {
                ResolvedLabel::All => self.view.node_cursor(LabelSelection::All, context)?,
                ResolvedLabel::Known(label) => self
                    .view
                    .node_cursor(LabelSelection::AllOf(&[*label]), context)?,
                ResolvedLabel::Missing => continue,
            });
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn next_key(
        &mut self,
        index: usize,
        child: usize,
        output: SlotId,
        namespace: GraphName<'plan>,
        key: ExprId,
        kind: EntityKind,
        value: &mut RowBatch<'v, 'm, 'g>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<bool, NativeExecutionError> {
        loop {
            if !self.next_occurrence(child, context)? {
                return Ok(false);
            }
            let evaluated = {
                let child = self
                    .occurrences
                    .as_slice()
                    .get(child)
                    .ok_or(RuntimeError::Batch)?;
                evaluate_at(
                    &mut self.evaluator,
                    self.mutation.as_mut(),
                    key,
                    &child.schema,
                    &child.output,
                    0,
                    self.view,
                    context,
                )?
            };
            let text = match evaluated {
                QueryValue::Null => continue,
                QueryValue::String(text) => text,
                _ => {
                    return Err(key_expression_error(
                        key,
                        RuntimeError::Value(QueryError::Type),
                    ));
                }
            };
            validate_lookup_key_bounds(namespace.as_str().len(), text.len(), key)?;
            value.clear();
            value.push_row(&[QueryValue::String(text)], context)?;
            let text = match value.value(0, 0) {
                Some(QueryValue::String(text)) => text,
                _ => return Err(RuntimeError::Batch.into()),
            };
            let application_key =
                ApplicationKey::new(kind, namespace.as_str(), text).map_err(|_| {
                    NativeExecutionError::Expression(ExpressionError {
                        expression: key,
                        failure: ExpressionFailure::Plan(PlanError::Limit),
                    })
                })?;
            let mut resources = TreeResources::for_query(context)?;
            let found = self
                .view
                .lookup_application_key(application_key, &mut resources)?;
            drop(resources);
            let Some(found) = found else {
                continue;
            };
            let cell = match found {
                EntityId::Node(id) => ScalarCell::Node(id),
                EntityId::Relationship(id) => ScalarCell::Relationship(id),
            };
            self.push_extended(index, child, &[(output, cell)], context)?;
            return Ok(true);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn next_expand(
        &mut self,
        index: usize,
        child: usize,
        source_slot: SlotId,
        node_slot: SlotId,
        relationship_slot: SlotId,
        pattern: PatternId,
        direction: Direction,
        all_types: bool,
        types: &QueryArena<'m, 'g, RelTypeId>,
        cursor: &mut Option<ExpandCursor<'s, 'm, 'g>>,
        bound: &mut Option<NodeId>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<bool, NativeExecutionError> {
        loop {
            if let Some(active) = cursor.as_mut() {
                let mut relationships = [empty_relationship()?];
                let (count, state) = self.view.expand(active, &mut relationships, context)?;
                if state == CursorState::Done {
                    *cursor = None;
                }
                if count == 1 {
                    let relationship = *relationships.first().ok_or(RuntimeError::Batch)?;
                    let source = bound.ok_or(RuntimeError::Batch)?;
                    let neighbor = neighbor(source, relationship, direction)?;
                    if self.relationship_used(child, pattern, relationship.rel, index)? {
                        continue;
                    }
                    self.push_extended(
                        index,
                        child,
                        &[
                            (node_slot, ScalarCell::Node(neighbor)),
                            (
                                relationship_slot,
                                ScalarCell::Relationship(relationship.rel),
                            ),
                        ],
                        context,
                    )?;
                    self.add_relationship_use(
                        index,
                        RelationshipUse {
                            pattern,
                            origin: self.occurrence(index)?.node,
                            relationship: relationship.rel,
                        },
                    )?;
                    return Ok(true);
                }
                if count != 0 {
                    return Err(RuntimeError::Batch.into());
                }
                continue;
            }
            if !self.next_occurrence(child, context)? {
                return Ok(false);
            }
            let source = self.node_at(child, source_slot)?;
            *bound = Some(source);
            let selection = if all_types {
                RelationshipTypeSelection::All
            } else {
                RelationshipTypeSelection::Any(types.as_slice())
            };
            *cursor = Some(self.view.expansion_cursor(
                source,
                direction_selection(direction),
                selection,
                context,
            )?);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn next_bounded_expand(
        &mut self,
        index: usize,
        child: usize,
        source_slot: SlotId,
        node_slot: SlotId,
        relationships_slot: SlotId,
        edge_predicate: Option<EdgePredicate>,
        completed_edge_predicate: Option<CompletedEdgePredicate>,
        min: u8,
        max: u8,
        direction: Direction,
        pattern: PatternId,
        all_types: bool,
        types: &QueryArena<'m, 'g, RelTypeId>,
        frames: &mut QueryArena<'m, 'g, PathFrame<'s, 'm, 'g>>,
        path: &mut QueryArena<'m, 'g, RelId>,
        nodes: &mut QueryArena<'m, 'g, NodeId>,
        edge_schema: Option<&Schema<'m, 'g>>,
        mut edge_input: Option<&mut RowBatch<'v, 'm, 'g>>,
        completed_schema: Option<&Schema<'m, 'g>>,
        mut completed_input: Option<&mut RowBatch<'v, 'm, 'g>>,
        zero_pending: &mut bool,
        active: &mut bool,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<bool, NativeExecutionError> {
        loop {
            if !*active {
                if !self.next_occurrence(child, context)? {
                    return Ok(false);
                }
                frames.clear();
                path.clear();
                nodes.clear();
                let start = self.node_at(child, source_slot)?;
                nodes.push(start).map_err(RuntimeError::Memory)?;
                frames
                    .push(PathFrame {
                        cursor: self.view.expansion_cursor(
                            start,
                            direction_selection(direction),
                            relationship_selection(all_types, types),
                            context,
                        )?,
                    })
                    .map_err(RuntimeError::Memory)?;
                *zero_pending = min == 0;
                *active = true;
            }

            if *zero_pending {
                *zero_pending = false;
                let start = *nodes.as_slice().first().ok_or(RuntimeError::Batch)?;
                if max == 0 {
                    frames.clear();
                    *active = false;
                }
                self.push_path(
                    index,
                    child,
                    node_slot,
                    relationships_slot,
                    start,
                    path,
                    pattern,
                    context,
                )?;
                context.charge(WorkKind::Paths, 1)?;
                return Ok(true);
            }

            if frames.len() == path.len() {
                if path.is_empty() {
                    *active = false;
                    continue;
                }
                path.truncate(path.len() - 1);
                nodes.truncate(nodes.len().saturating_sub(1));
            }
            if frames.is_empty() {
                *active = false;
                continue;
            }

            let mut candidates = [empty_relationship()?];
            let (count, state) = {
                let frame = frames
                    .as_mut_slice()
                    .last_mut()
                    .ok_or(RuntimeError::Batch)?;
                self.view
                    .expand(&mut frame.cursor, &mut candidates, context)?
            };
            if count == 0 {
                if state != CursorState::Done {
                    return Err(RuntimeError::Batch.into());
                }
                frames.truncate(frames.len() - 1);
                path.truncate(frames.len().saturating_sub(1));
                nodes.truncate(frames.len());
                if frames.is_empty() {
                    *active = false;
                }
                continue;
            }
            if count != 1 {
                return Err(RuntimeError::Batch.into());
            }
            let relationship = *candidates.first().ok_or(RuntimeError::Batch)?;
            if path.as_slice().contains(&relationship.rel)
                || self.relationship_used(child, pattern, relationship.rel, index)?
            {
                continue;
            }
            let current = *nodes.as_slice().last().ok_or(RuntimeError::Batch)?;
            let next = neighbor(current, relationship, direction)?;
            if let Some(predicate) = edge_predicate {
                let schema = edge_schema.ok_or(RuntimeError::Batch)?;
                let input = edge_input.as_deref_mut().ok_or(RuntimeError::Batch)?;
                self.fill_private_edge(
                    child,
                    schema,
                    input,
                    predicate.current_edge,
                    relationship.rel,
                    context,
                )?;
                let retained = evaluate_at(
                    &mut self.evaluator,
                    self.mutation.as_mut(),
                    predicate.expression,
                    schema,
                    input,
                    0,
                    self.view,
                    context,
                )?
                .truth()
                .map_err(RuntimeError::Value)?
                .retained();
                if !retained {
                    continue;
                }
            }
            path.push(relationship.rel).map_err(RuntimeError::Memory)?;
            nodes.push(next).map_err(RuntimeError::Memory)?;
            let depth = u8::try_from(path.len()).map_err(|_| RuntimeError::Batch)?;
            if depth < max {
                frames
                    .push(PathFrame {
                        cursor: self.view.expansion_cursor(
                            next,
                            direction_selection(direction),
                            relationship_selection(all_types, types),
                            context,
                        )?,
                    })
                    .map_err(RuntimeError::Memory)?;
            }
            if depth < min {
                continue;
            }
            if let Some(predicate) = completed_edge_predicate {
                let schema = completed_schema.ok_or(RuntimeError::Batch)?;
                let input = completed_input.as_deref_mut().ok_or(RuntimeError::Batch)?;
                let mut retained = true;
                for relationship in path.as_slice() {
                    self.fill_completed_edge(
                        child,
                        schema,
                        input,
                        relationships_slot,
                        predicate.current_edge,
                        path.as_slice(),
                        *relationship,
                        context,
                    )?;
                    if !evaluate_at(
                        &mut self.evaluator,
                        self.mutation.as_mut(),
                        predicate.expression,
                        schema,
                        input,
                        0,
                        self.view,
                        context,
                    )?
                    .truth()
                    .map_err(RuntimeError::Value)?
                    .retained()
                    {
                        retained = false;
                        break;
                    }
                }
                if !retained {
                    continue;
                }
            }
            self.push_path(
                index,
                child,
                node_slot,
                relationships_slot,
                next,
                path,
                pattern,
                context,
            )?;
            context.charge(WorkKind::Paths, 1)?;
            return Ok(true);
        }
    }

    fn fill_private_edge(
        &mut self,
        child: usize,
        schema: &Schema<'m, 'g>,
        input: &mut RowBatch<'v, 'm, 'g>,
        edge_slot: SlotId,
        relationship: RelId,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        input.clear();
        let query_view = self.query_view;
        let child = self.occurrence(child)?;
        input.push_from(
            |column| {
                let slot = *schema.slots().get(column).ok_or(RuntimeError::Batch)?;
                if slot == edge_slot {
                    return Ok(query_view.relationship(relationship));
                }
                let source = child.schema.column(slot)?;
                child.output.value(0, source).ok_or(RuntimeError::Batch)
            },
            context,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn fill_completed_edge(
        &mut self,
        child: usize,
        schema: &Schema<'m, 'g>,
        input: &mut RowBatch<'v, 'm, 'g>,
        relationships_slot: SlotId,
        edge_slot: SlotId,
        path: &[RelId],
        relationship: RelId,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        input.clear();
        let query_view = self.query_view;
        let list = QueryList::relationships(query_view, path, context.values())?;
        let child = self.occurrence(child)?;
        input.push_from(
            |column| {
                let slot = *schema.slots().get(column).ok_or(RuntimeError::Batch)?;
                if slot == edge_slot {
                    return Ok(query_view.relationship(relationship));
                }
                if slot == relationships_slot {
                    return Ok(QueryValue::List(list));
                }
                let source = child.schema.column(slot)?;
                child.output.value(0, source).ok_or(RuntimeError::Batch)
            },
            context,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn push_path(
        &mut self,
        parent: usize,
        child: usize,
        node_slot: SlotId,
        relationships_slot: SlotId,
        endpoint: NodeId,
        path: &QueryArena<'m, 'g, RelId>,
        pattern: PatternId,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        let query_view = self.query_view;
        let list = QueryList::relationships(query_view, path.as_slice(), context.values())?;
        let origin = self.occurrence(parent)?.node;
        let (child, parent) = child_parent(self.occurrences.as_mut_slice(), child, parent)?;
        parent.output.push_from(
            |column| {
                let slot = *parent
                    .schema
                    .slots()
                    .get(column)
                    .ok_or(RuntimeError::Batch)?;
                if let Ok(source) = child.schema.column(slot) {
                    return child.output.value(0, source).ok_or(RuntimeError::Batch);
                }
                if slot == node_slot {
                    return Ok(query_view.node(endpoint));
                }
                if slot == relationships_slot {
                    return Ok(QueryValue::List(list));
                }
                Err(RuntimeError::Batch)
            },
            context,
        )?;
        copy_relationship_uses(&child.uses, &mut parent.uses)?;
        for relationship in path.as_slice() {
            let usage = RelationshipUse {
                pattern,
                origin,
                relationship: *relationship,
            };
            if !parent.uses.as_slice().contains(&usage) {
                parent.uses.push(usage).map_err(RuntimeError::Memory)?;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn next_join(
        &mut self,
        index: usize,
        left: usize,
        right: usize,
        predicate: Option<ExprId>,
        shared: &QueryArena<'m, 'g, SlotId>,
        plan: planner::JoinPlan,
        build_rows: Option<&mut RetainedRows<'v, 'm, 'g>>,
        bucket_heads: Option<&mut QueryArena<'m, 'g, usize>>,
        next_links: Option<&mut QueryArena<'m, 'g, usize>>,
        build_hashes: Option<&mut QueryArena<'m, 'g, u64>>,
        initialized: &mut bool,
        nested_left_active: &mut bool,
        probe_active: &mut bool,
        next_candidate: &mut usize,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<bool, NativeExecutionError> {
        match plan.strategy {
            planner::JoinStrategy::Nested => self.next_nested_join(
                index,
                left,
                right,
                predicate,
                shared.as_slice(),
                nested_left_active,
                context,
            ),
            planner::JoinStrategy::Hash => {
                let rows = build_rows.ok_or(RuntimeError::Batch)?;
                let heads = bucket_heads.ok_or(RuntimeError::Batch)?;
                let links = next_links.ok_or(RuntimeError::Batch)?;
                let hashes = build_hashes.ok_or(RuntimeError::Batch)?;
                self.next_hash_join(
                    index,
                    left,
                    right,
                    predicate,
                    shared.as_slice(),
                    plan.build,
                    rows,
                    heads,
                    links,
                    hashes,
                    initialized,
                    probe_active,
                    next_candidate,
                    context,
                )
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn next_nested_join(
        &mut self,
        index: usize,
        left: usize,
        right: usize,
        predicate: Option<ExprId>,
        shared: &[SlotId],
        left_active: &mut bool,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<bool, NativeExecutionError> {
        loop {
            if !*left_active {
                if !self.next_occurrence(left, context)? {
                    return Ok(false);
                }
                self.reset_occurrence(right, context)?;
                *left_active = true;
            }
            while self.next_occurrence(right, context)? {
                context.charge(WorkKind::OperatorRows, 1)?;
                context.charge(WorkKind::RowsIn, 1)?;
                context.charge(WorkKind::JoinProbes, 1)?;
                if !self.current_rows_join(left, right, shared, context)?
                    || !uses_compatible(
                        self.occurrence(left)?.uses.as_slice(),
                        self.occurrence(right)?.uses.as_slice(),
                    )
                {
                    continue;
                }
                self.combine_current_rows(index, left, right, context)?;
                if self.join_predicate_retained(index, predicate, context)? {
                    return Ok(true);
                }
            }
            *left_active = false;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn next_hash_join(
        &mut self,
        index: usize,
        left: usize,
        right: usize,
        predicate: Option<ExprId>,
        shared: &[SlotId],
        build_side: planner::BuildSide,
        build_rows: &mut RetainedRows<'v, 'm, 'g>,
        bucket_heads: &mut QueryArena<'m, 'g, usize>,
        next_links: &mut QueryArena<'m, 'g, usize>,
        build_hashes: &mut QueryArena<'m, 'g, u64>,
        initialized: &mut bool,
        probe_active: &mut bool,
        next_candidate: &mut usize,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<bool, NativeExecutionError> {
        let (build, probe) = match build_side {
            planner::BuildSide::Left => (left, right),
            planner::BuildSide::Right => (right, left),
        };
        if !*initialized {
            self.drain_occurrence(build, build_rows, context)?;
            self.build_hash_index(
                build,
                build_rows,
                shared,
                bucket_heads,
                next_links,
                build_hashes,
                context,
            )?;
            *initialized = true;
        }
        loop {
            if !*probe_active {
                if !self.next_occurrence(probe, context)? {
                    return Ok(false);
                }
                let hash = self.current_row_hash(probe, shared, context)?;
                let bucket = usize::try_from(
                    hash % u64::try_from(bucket_heads.len()).map_err(|_| RuntimeError::Batch)?,
                )
                .map_err(|_| RuntimeError::Batch)?;
                *next_candidate = *bucket_heads
                    .as_slice()
                    .get(bucket)
                    .ok_or(RuntimeError::Batch)?;
                *probe_active = true;
            }
            while *next_candidate != usize::MAX {
                let candidate = *next_candidate;
                *next_candidate = *next_links
                    .as_slice()
                    .get(candidate)
                    .ok_or(RuntimeError::Batch)?;
                context.charge(WorkKind::OperatorRows, 1)?;
                context.charge(WorkKind::RowsIn, 1)?;
                context.charge(WorkKind::JoinProbes, 1)?;
                context.charge(WorkKind::HashProbes, 1)?;
                let probe_hash = self.current_row_hash(probe, shared, context)?;
                if build_hashes.as_slice().get(candidate).copied() != Some(probe_hash)
                    || !self.current_retained_rows_join(
                        probe, build, build_rows, candidate, shared, context,
                    )?
                    || !uses_compatible(
                        self.occurrence(probe)?.uses.as_slice(),
                        build_rows.span(candidate)?,
                    )
                {
                    continue;
                }
                self.combine_hash_rows(
                    index, left, right, probe, build, build_rows, candidate, context,
                )?;
                if self.join_predicate_retained(index, predicate, context)? {
                    return Ok(true);
                }
            }
            *probe_active = false;
        }
    }

    fn join_predicate_retained(
        &mut self,
        index: usize,
        predicate: Option<ExprId>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<bool, NativeExecutionError> {
        let Some(expression) = predicate else {
            return Ok(true);
        };
        let occurrence = self
            .occurrences
            .as_slice()
            .get(index)
            .ok_or(RuntimeError::Batch)?;
        let retained = evaluate_at(
            &mut self.evaluator,
            self.mutation.as_mut(),
            expression,
            &occurrence.schema,
            &occurrence.output,
            0,
            self.view,
            context,
        )?
        .truth()
        .map_err(RuntimeError::Value)?
        .retained();
        if !retained {
            self.output_mut(index)?.clear();
            self.uses_mut(index)?.clear();
        }
        Ok(retained)
    }

    #[allow(clippy::too_many_arguments)]
    fn next_optional(
        &mut self,
        index: usize,
        left: usize,
        right: usize,
        predicate: Option<ExprId>,
        shared: &QueryArena<'m, 'g, SlotId>,
        left_active: &mut bool,
        matched: &mut bool,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<bool, NativeExecutionError> {
        loop {
            if !*left_active {
                if !self.next_occurrence(left, context)? {
                    return Ok(false);
                }
                self.reset_occurrence(right, context)?;
                *left_active = true;
                *matched = false;
            }
            while self.next_occurrence(right, context)? {
                context.charge(WorkKind::OperatorRows, 1)?;
                context.charge(WorkKind::RowsIn, 1)?;
                context.charge(WorkKind::JoinProbes, 1)?;
                if !self.current_rows_join(left, right, shared.as_slice(), context)?
                    || !uses_compatible(
                        self.occurrence(left)?.uses.as_slice(),
                        self.occurrence(right)?.uses.as_slice(),
                    )
                {
                    continue;
                }
                self.combine_current_rows(index, left, right, context)?;
                if !self.join_predicate_retained(index, predicate, context)? {
                    continue;
                }
                *matched = true;
                return Ok(true);
            }
            *left_active = false;
            if !*matched {
                self.push_optional_null_current(index, left, context)?;
                return Ok(true);
            }
        }
    }

    fn drain_occurrence(
        &mut self,
        child: usize,
        destination: &mut RetainedRows<'v, 'm, 'g>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), NativeExecutionError> {
        while self.next_occurrence(child, context)? {
            let occurrence = self.occurrence(child)?;
            let start = destination.uses.len();
            destination.rows.push_from(
                |column| {
                    occurrence
                        .output
                        .value(0, column)
                        .ok_or(RuntimeError::Batch)
                },
                context,
            )?;
            for usage in occurrence.uses.as_slice() {
                destination
                    .uses
                    .push(*usage)
                    .map_err(RuntimeError::Memory)?;
            }
            destination
                .spans
                .push(UseSpan {
                    start,
                    len: occurrence.uses.len(),
                })
                .map_err(RuntimeError::Memory)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn build_hash_index(
        &self,
        occurrence: usize,
        rows: &RetainedRows<'v, 'm, 'g>,
        shared: &[SlotId],
        bucket_heads: &mut QueryArena<'m, 'g, usize>,
        next_links: &mut QueryArena<'m, 'g, usize>,
        hashes: &mut QueryArena<'m, 'g, u64>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        bucket_heads.clear();
        next_links.clear();
        hashes.clear();
        let bucket_count = bucket_heads.capacity();
        if bucket_count == 0 {
            return Err(RuntimeError::Batch);
        }
        for _ in 0..bucket_count {
            bucket_heads
                .push(usize::MAX)
                .map_err(RuntimeError::Memory)?;
        }
        for row in 0..rows.rows.rows() {
            let hash = self.retained_row_hash(occurrence, rows, row, shared, context)?;
            let bucket = usize::try_from(
                hash % u64::try_from(bucket_count).map_err(|_| RuntimeError::Batch)?,
            )
            .map_err(|_| RuntimeError::Batch)?;
            let head = *bucket_heads
                .as_slice()
                .get(bucket)
                .ok_or(RuntimeError::Batch)?;
            next_links.push(head).map_err(RuntimeError::Memory)?;
            hashes.push(hash).map_err(RuntimeError::Memory)?;
            let new_head = hashes.len().checked_sub(1).ok_or(RuntimeError::Batch)?;
            *bucket_heads
                .as_mut_slice()
                .get_mut(bucket)
                .ok_or(RuntimeError::Batch)? = new_head;
        }
        Ok(())
    }

    fn current_row_hash(
        &self,
        occurrence: usize,
        shared: &[SlotId],
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<u64, RuntimeError> {
        let occurrence = self.occurrence(occurrence)?;
        let mut hash = 0x9e37_79b9_7f4a_7c15_u64;
        for slot in shared {
            let value = occurrence
                .output
                .value(0, occurrence.schema.column(*slot)?)
                .ok_or(RuntimeError::Batch)?;
            hash = (hash.rotate_left(13) ^ value.group_hash(context.values())?)
                .wrapping_mul(0x9e37_79b1_85eb_ca87);
        }
        Ok(hash)
    }

    fn retained_row_hash(
        &self,
        occurrence: usize,
        rows: &RetainedRows<'v, 'm, 'g>,
        row: usize,
        shared: &[SlotId],
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<u64, RuntimeError> {
        let occurrence = self.occurrence(occurrence)?;
        let mut hash = 0x9e37_79b9_7f4a_7c15_u64;
        for slot in shared {
            let value = rows
                .rows
                .value(row, occurrence.schema.column(*slot)?)
                .ok_or(RuntimeError::Batch)?;
            hash = (hash.rotate_left(13) ^ value.group_hash(context.values())?)
                .wrapping_mul(0x9e37_79b1_85eb_ca87);
        }
        Ok(hash)
    }

    fn current_rows_join(
        &self,
        left: usize,
        right: usize,
        shared: &[SlotId],
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<bool, RuntimeError> {
        let left = self.occurrence(left)?;
        let right = self.occurrence(right)?;
        for slot in shared {
            let left_value = left
                .output
                .value(0, left.schema.column(*slot)?)
                .ok_or(RuntimeError::Batch)?;
            let right_value = right
                .output
                .value(0, right.schema.column(*slot)?)
                .ok_or(RuntimeError::Batch)?;
            if !left_value
                .predicate(right_value, Comparison::Equal, context.values())?
                .retained()
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    #[allow(clippy::too_many_arguments)]
    fn current_retained_rows_join(
        &self,
        current: usize,
        retained: usize,
        retained_rows: &RetainedRows<'v, 'm, 'g>,
        retained_row: usize,
        shared: &[SlotId],
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<bool, RuntimeError> {
        let current = self.occurrence(current)?;
        let retained = self.occurrence(retained)?;
        for slot in shared {
            let current_value = current
                .output
                .value(0, current.schema.column(*slot)?)
                .ok_or(RuntimeError::Batch)?;
            let retained_value = retained_rows
                .rows
                .value(retained_row, retained.schema.column(*slot)?)
                .ok_or(RuntimeError::Batch)?;
            if !current_value
                .predicate(retained_value, Comparison::Equal, context.values())?
                .retained()
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn combine_current_rows(
        &mut self,
        parent: usize,
        left: usize,
        right: usize,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        if left >= parent || right >= parent {
            return Err(RuntimeError::Batch);
        }
        let (children, parents) = self.occurrences.as_mut_slice().split_at_mut(parent);
        let left = children.get(left).ok_or(RuntimeError::Batch)?;
        let right = children.get(right).ok_or(RuntimeError::Batch)?;
        let parent = parents.first_mut().ok_or(RuntimeError::Batch)?;
        parent.output.push_from(
            |column| {
                let slot = *parent
                    .schema
                    .slots()
                    .get(column)
                    .ok_or(RuntimeError::Batch)?;
                if let Ok(source) = left.schema.column(slot) {
                    return left.output.value(0, source).ok_or(RuntimeError::Batch);
                }
                right
                    .output
                    .value(0, right.schema.column(slot)?)
                    .ok_or(RuntimeError::Batch)
            },
            context,
        )?;
        for usage in left.uses.as_slice().iter().chain(right.uses.as_slice()) {
            if !parent.uses.as_slice().contains(usage) {
                parent.uses.push(*usage).map_err(RuntimeError::Memory)?;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn combine_hash_rows(
        &mut self,
        parent: usize,
        left: usize,
        right: usize,
        probe: usize,
        build: usize,
        build_rows: &RetainedRows<'v, 'm, 'g>,
        build_row: usize,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        if left >= parent || right >= parent || probe >= parent || build >= parent {
            return Err(RuntimeError::Batch);
        }
        let (children, parents) = self.occurrences.as_mut_slice().split_at_mut(parent);
        let left_occurrence = children.get(left).ok_or(RuntimeError::Batch)?;
        let _right_occurrence = children.get(right).ok_or(RuntimeError::Batch)?;
        let probe_occurrence = children.get(probe).ok_or(RuntimeError::Batch)?;
        let build_occurrence = children.get(build).ok_or(RuntimeError::Batch)?;
        let parent = parents.first_mut().ok_or(RuntimeError::Batch)?;
        parent.output.push_from(
            |column| {
                let slot = *parent
                    .schema
                    .slots()
                    .get(column)
                    .ok_or(RuntimeError::Batch)?;
                let logical_index = if left_occurrence.schema.column(slot).is_ok() {
                    left
                } else {
                    right
                };
                if logical_index == probe {
                    return probe_occurrence
                        .output
                        .value(0, probe_occurrence.schema.column(slot)?)
                        .ok_or(RuntimeError::Batch);
                }
                build_rows
                    .rows
                    .value(build_row, build_occurrence.schema.column(slot)?)
                    .ok_or(RuntimeError::Batch)
            },
            context,
        )?;
        for usage in probe_occurrence
            .uses
            .as_slice()
            .iter()
            .chain(build_rows.span(build_row)?)
        {
            if !parent.uses.as_slice().contains(usage) {
                parent.uses.push(*usage).map_err(RuntimeError::Memory)?;
            }
        }
        Ok(())
    }

    fn push_optional_null_current(
        &mut self,
        parent: usize,
        left: usize,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        if left >= parent {
            return Err(RuntimeError::Batch);
        }
        let (children, parents) = self.occurrences.as_mut_slice().split_at_mut(parent);
        let left = children.get(left).ok_or(RuntimeError::Batch)?;
        let parent = parents.first_mut().ok_or(RuntimeError::Batch)?;
        parent.output.push_from(
            |column| {
                let slot = *parent
                    .schema
                    .slots()
                    .get(column)
                    .ok_or(RuntimeError::Batch)?;
                match left.schema.column(slot) {
                    Ok(source) => left.output.value(0, source).ok_or(RuntimeError::Batch),
                    Err(_) => Ok(QueryValue::Null),
                }
            },
            context,
        )?;
        copy_relationship_uses(&left.uses, &mut parent.uses)
    }

    fn occurrence(&self, index: usize) -> Result<&Occurrence<'s, 'plan, 'v, 'm, 'g>, RuntimeError> {
        self.occurrences
            .as_slice()
            .get(index)
            .ok_or(RuntimeError::Batch)
    }

    fn occurrence_mut(
        &mut self,
        index: usize,
    ) -> Result<&mut Occurrence<'s, 'plan, 'v, 'm, 'g>, RuntimeError> {
        self.occurrences
            .as_mut_slice()
            .get_mut(index)
            .ok_or(RuntimeError::Batch)
    }

    fn output_mut(&mut self, index: usize) -> Result<&mut RowBatch<'v, 'm, 'g>, RuntimeError> {
        Ok(&mut self.occurrence_mut(index)?.output)
    }

    fn uses_mut(
        &mut self,
        index: usize,
    ) -> Result<&mut QueryArena<'m, 'g, RelationshipUse>, RuntimeError> {
        Ok(&mut self.occurrence_mut(index)?.uses)
    }

    fn node_at(&self, index: usize, slot: SlotId) -> Result<NodeId, RuntimeError> {
        let occurrence = self.occurrence(index)?;
        let column = occurrence.schema.column(slot)?;
        match occurrence.output.value(0, column) {
            Some(QueryValue::NodeRef(value)) => Ok(value.id()),
            _ => Err(RuntimeError::Batch),
        }
    }

    fn copy_output(
        &mut self,
        parent: usize,
        child: usize,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        let (child, parent) = child_parent(self.occurrences.as_mut_slice(), child, parent)?;
        parent.output.push_from(
            |column| child.output.value(0, column).ok_or(RuntimeError::Batch),
            context,
        )?;
        copy_relationship_uses(&child.uses, &mut parent.uses)
    }

    fn push_extended(
        &mut self,
        parent: usize,
        child: usize,
        additions: &[(SlotId, ScalarCell)],
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        let query_view = self.query_view;
        let (child, parent) = child_parent(self.occurrences.as_mut_slice(), child, parent)?;
        parent.output.push_from(
            |column| {
                let slot = parent
                    .schema
                    .slots()
                    .get(column)
                    .copied()
                    .ok_or(RuntimeError::Batch)?;
                if let Ok(source) = child.schema.column(slot) {
                    return child.output.value(0, source).ok_or(RuntimeError::Batch);
                }
                additions
                    .iter()
                    .find(|(candidate, _)| *candidate == slot)
                    .map(|(_, value)| value.value(query_view))
                    .ok_or(RuntimeError::Batch)
            },
            context,
        )?;
        copy_relationship_uses(&child.uses, &mut parent.uses)
    }

    fn push_projection(
        &mut self,
        parent: usize,
        projections: &[Projection],
        values: &QueryArena<'m, 'g, RowBatch<'v, 'm, 'g>>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        let parent = self.occurrence_mut(parent)?;
        parent.output.push_from(
            |column| {
                let slot = parent
                    .schema
                    .slots()
                    .get(column)
                    .copied()
                    .ok_or(RuntimeError::Batch)?;
                let position = projections
                    .iter()
                    .position(|projection| projection.slot == slot)
                    .ok_or(RuntimeError::Batch)?;
                values
                    .as_slice()
                    .get(position)
                    .and_then(|value| value.value(0, 0))
                    .ok_or(RuntimeError::Batch)
            },
            context,
        )
    }

    fn relationship_used(
        &self,
        child: usize,
        pattern: PatternId,
        relationship: RelId,
        origin: usize,
    ) -> Result<bool, RuntimeError> {
        let origin = self.occurrence(origin)?.node;
        Ok(self.occurrence(child)?.uses.as_slice().iter().any(|used| {
            used.pattern == pattern && used.relationship == relationship && used.origin != origin
        }))
    }

    fn add_relationship_use(
        &mut self,
        index: usize,
        value: RelationshipUse,
    ) -> Result<(), RuntimeError> {
        let uses = self.uses_mut(index)?;
        if !uses.as_slice().contains(&value) {
            uses.push(value).map_err(RuntimeError::Memory)?;
        }
        Ok(())
    }

    fn copy_uses(&mut self, parent: usize, child: usize) -> Result<(), RuntimeError> {
        let (child, parent) = child_parent(self.occurrences.as_mut_slice(), child, parent)?;
        copy_relationship_uses(&child.uses, &mut parent.uses)
    }
}

impl<'s, 'r, 'plan, 'v, 'm, 'g, 'i> RowOperator<'v, 'm, 'g, NativeExecutionError>
    for NativePattern<'s, 'r, 'plan, 'v, 'm, 'g, 'i>
{
    fn schema(&self) -> &Schema<'m, 'g> {
        &self.schema
    }
}

impl<'s, 'r, 'plan, 'v, 'm, 'g, 'i> PullOperator<'v, 'm, 'g, NativeExecutionError>
    for NativePattern<'s, 'r, 'plan, 'v, 'm, 'g, 'i>
{
    fn node(&self) -> PlanNodeId {
        self.root_node
    }

    fn prepare_search(
        &mut self,
        _: PlanNodeId,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), NativeExecutionError> {
        Err(PlanError::Search.into())
    }

    fn pull(
        &mut self,
        context: &mut RuntimeContext<'v, 'm, 'g>,
        output: &mut RowBatch<'v, 'm, 'g>,
    ) -> Result<PullState, NativeExecutionError> {
        while output.rows() < output.capacity() {
            if !self.next_occurrence(self.root, context)? {
                return Ok(PullState::Done);
            }
            let root = self.occurrence(self.root)?;
            if output.columns() != root.schema.slots().len() {
                return Err(RuntimeError::Batch.into());
            }
            output.push_from(
                |column| root.output.value(0, column).ok_or(RuntimeError::Batch),
                context,
            )?;
        }
        Ok(PullState::More)
    }
}

/// Counts the occurrences a validated plan needs, refusing every operator this
/// executor does not implement. `mutation` is true only under a writer scope:
/// `Mutate` is admitted there when every item is a SET, REMOVE or label edit,
/// and is refused everywhere else, as are CREATE and DELETE items.
fn occurrence_count(
    operators: &[super::plan::Operator<'_>],
    node: PlanNodeId,
    depth: usize,
    bindings: Option<&AnchorBinding<'_>>,
    mutation: bool,
) -> Result<usize, PlanError> {
    if depth >= super::plan::MAX_PLAN_DEPTH {
        return Err(PlanError::Limit);
    }
    if anchor_source(bindings, node).is_some() {
        return Ok(1);
    }
    let operator = operators.get(node.0 as usize).ok_or(PlanError::Reference)?;
    let implemented = matches!(
        operator.kind,
        OperatorKind::Unit
            | OperatorKind::ScanNodes { .. }
            | OperatorKind::LookupNode { .. }
            | OperatorKind::LookupRelationship { .. }
            | OperatorKind::LookupKey { .. }
            | OperatorKind::Expand { .. }
            | OperatorKind::BoundedExpand { .. }
            | OperatorKind::Join { .. }
            | OperatorKind::OptionalApply { .. }
            | OperatorKind::Filter(_)
            | OperatorKind::Project(_)
            | OperatorKind::With(_)
            | OperatorKind::OffsetLimit { .. }
            | OperatorKind::Sort(_)
            | OperatorKind::Distinct
            | OperatorKind::Eager
            | OperatorKind::Aggregate { .. }
            | OperatorKind::Collect
    );
    let mutation_admitted = mutation
        && matches!(operator.kind, OperatorKind::Mutate(items) if supported_mutations(items));
    if !implemented && !mutation_admitted {
        return Err(PlanError::Reference);
    }
    if matches!(operator.kind, OperatorKind::OptionalApply { .. }) {
        let left = operator.inputs.first().copied().ok_or(PlanError::Arity)?;
        let right = operator.inputs.get(1).copied().ok_or(PlanError::Arity)?;
        if operator.inputs.len() != 2 {
            return Err(PlanError::Arity);
        }
        let left_count = occurrence_count(operators, left, depth + 1, bindings, mutation)?;
        let scoped = AnchorBinding {
            logical: left,
            source: 0,
            parent: bindings,
        };
        let right_count = occurrence_count(operators, right, depth + 1, Some(&scoped), mutation)?;
        return 1usize
            .checked_add(left_count)
            .and_then(|count| count.checked_add(right_count))
            .filter(|count| *count <= MAX_PLAN_NODES)
            .ok_or(PlanError::Limit);
    }
    let mut count = 1usize;
    for input in operator.inputs {
        count = count
            .checked_add(occurrence_count(
                operators,
                *input,
                depth + 1,
                bindings,
                mutation,
            )?)
            .filter(|count| *count <= MAX_PLAN_NODES)
            .ok_or(PlanError::Limit)?;
    }
    Ok(count)
}

/// Evaluates one expression for one occurrence row. Outside a mutation scope
/// this is exactly `evaluate` against the read view. Inside one, every read
/// goes through the statement's overlay first, so a clause after a `Mutate`
/// sees the values that clause staged, and a SET item sees the items before
/// it. The overlay's checkpoint is the runtime's own caller control.
#[allow(
    clippy::too_many_arguments,
    reason = "the scalar boundary keeps every authentic owner explicit"
)]
fn evaluate_at<'a, 'r, 'plan, 'v, 'm, 'g>(
    evaluator: &'a mut NativeExpressionEvaluator<'r, 'plan, 'v, 'm, 'g>,
    mutation: Option<&mut MutationScope<'_, '_>>,
    expression: ExprId,
    schema: &Schema<'_, '_>,
    input: &RowBatch<'v, 'm, 'g>,
    row: usize,
    view: &GraphReadView<'_, 'v, 'm, 'g>,
    context: &mut RuntimeContext<'v, 'm, 'g>,
) -> Result<QueryValue<'a>, ExpressionError> {
    let Some(scope) = mutation else {
        return evaluator.evaluate(expression, schema, input, row, view, context);
    };
    let control = context.values().control();
    let mut write_control = |_: WritePhase| writer_checkpoint(control);
    let mut overlay = ClauseOverlay::new(&mut scope.overlay, &mut write_control);
    evaluator.evaluate_with_overlay(expression, schema, input, row, view, &mut overlay, context)
}

/// The same caller-control adapter the admission's own writer checkpoint uses.
fn writer_checkpoint(control: &crate::lifecycle::QueryControl) -> Result<(), StageError> {
    control.checkpoint().map_err(|_| StageError::Cancelled)
}

/// The mutation items this executor implements. CREATE needs fresh identity
/// allocation and DELETE needs tombstone staging; both stay refused.
fn supported_mutations(items: &[Mutation<'_>]) -> bool {
    items.iter().all(|item| {
        matches!(
            item,
            Mutation::SetProperty { .. }
                | Mutation::RemoveProperty { .. }
                | Mutation::SetLabel { .. }
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn build_occurrence<'s, 'r, 'plan, 'v, 'm, 'g>(
    view: &'s GraphReadView<'s, 'v, 'm, 'g>,
    plan: &'r RuntimePlan<'r, 'plan, 'r, 'm, 'g, 'r>,
    node: PlanNodeId,
    capacity: PatternCapacity,
    occurrences: &mut QueryArena<'m, 'g, Occurrence<'s, 'plan, 'v, 'm, 'g>>,
    context: &mut RuntimeContext<'v, 'm, 'g>,
    bindings: Option<&AnchorBinding<'_>>,
) -> Result<usize, NativeExecutionError> {
    context.checkpoint()?;
    if let Some(source) = anchor_source(bindings, node) {
        let schema = schema_for(plan, node, context)?;
        let output = RowBatch::storage(
            context,
            schema.slots().len(),
            1,
            capacity.rows.payload_bytes,
            capacity.rows.variable,
        )?;
        let uses = QueryArena::new(context.memory(), capacity.rows.rows.max(16))
            .map_err(RuntimeError::Memory)?;
        let index = occurrences.len();
        occurrences
            .push(Occurrence {
                node,
                schema,
                output,
                uses,
                state: PhysicalState::Anchor {
                    source,
                    emitted: false,
                },
            })
            .map_err(RuntimeError::Memory)?;
        return Ok(index);
    }
    let operator = plan
        .plan()
        .description()
        .operators
        .get(node.0 as usize)
        .ok_or(PlanError::Reference)?;
    let state = match operator.kind {
        OperatorKind::Unit => PhysicalState::Unit { emitted: false },
        OperatorKind::ScanNodes { output, label } => {
            let child = build_unary(
                view,
                plan,
                operator,
                capacity,
                occurrences,
                context,
                bindings,
            )?;
            let label = match label {
                None => ResolvedLabel::All,
                Some(name) => {
                    let mut resources = TreeResources::for_query(context)?;
                    match view.expression_symbol(SymbolKind::Label, name, &mut resources)? {
                        Some(Symbol::Label(label)) => ResolvedLabel::Known(label),
                        Some(_) => return Err(PlanError::Type.into()),
                        None => ResolvedLabel::Missing,
                    }
                }
            };
            PhysicalState::ScanNodes {
                child,
                output,
                label,
                cursor: None,
            }
        }
        OperatorKind::LookupNode { output, id } => PhysicalState::LookupNode {
            child: build_unary(
                view,
                plan,
                operator,
                capacity,
                occurrences,
                context,
                bindings,
            )?,
            output,
            id,
        },
        OperatorKind::LookupRelationship { output, id } => PhysicalState::LookupRelationship {
            child: build_unary(
                view,
                plan,
                operator,
                capacity,
                occurrences,
                context,
                bindings,
            )?,
            output,
            id,
        },
        OperatorKind::LookupKey {
            output,
            namespace,
            key,
            kind,
        } => PhysicalState::LookupKey {
            child: build_unary(
                view,
                plan,
                operator,
                capacity,
                occurrences,
                context,
                bindings,
            )?,
            output,
            namespace,
            key,
            kind,
            value: RowBatch::storage(
                context,
                1,
                1,
                capacity.rows.payload_bytes,
                capacity.rows.variable,
            )?,
        },
        OperatorKind::Expand {
            source,
            node,
            relationship,
            direction,
            relationship_types,
            pattern,
        } => {
            let child = build_unary(
                view,
                plan,
                operator,
                capacity,
                occurrences,
                context,
                bindings,
            )?;
            let mut types = QueryArena::new(context.memory(), relationship_types.len())
                .map_err(RuntimeError::Memory)?;
            let mut resources = TreeResources::for_query(context)?;
            for name in relationship_types {
                match view.expression_symbol(SymbolKind::RelationshipType, *name, &mut resources)? {
                    Some(Symbol::RelationshipType(kind)) if !types.as_slice().contains(&kind) => {
                        types.push(kind).map_err(RuntimeError::Memory)?;
                    }
                    Some(Symbol::RelationshipType(_)) | None => {}
                    Some(_) => return Err(PlanError::Type.into()),
                }
            }
            drop(resources);
            PhysicalState::Expand {
                child,
                source,
                node,
                relationship,
                pattern,
                direction,
                all_types: relationship_types.is_empty(),
                types,
                cursor: None,
                bound: None,
            }
        }
        OperatorKind::BoundedExpand {
            source,
            node,
            relationships,
            edge_predicate,
            completed_edge_predicate,
            min,
            max,
            direction,
            relationship_types,
            pattern,
        } => {
            let child = build_unary(
                view,
                plan,
                operator,
                capacity,
                occurrences,
                context,
                bindings,
            )?;
            let types = resolve_relationship_types(view, relationship_types, context)?;
            let edge_schema = match edge_predicate {
                Some(predicate) => Some(private_schema(
                    occurrences
                        .as_slice()
                        .get(child)
                        .ok_or(RuntimeError::Batch)?
                        .schema
                        .slots(),
                    &[predicate.current_edge],
                    context,
                )?),
                None => None,
            };
            let edge_input = match edge_schema.as_ref() {
                Some(schema) => Some(RowBatch::storage(
                    context,
                    schema.slots().len(),
                    1,
                    capacity.rows.payload_bytes,
                    capacity.rows.variable,
                )?),
                None => None,
            };
            let completed_schema = match completed_edge_predicate {
                Some(predicate) => Some(private_schema(
                    occurrences
                        .as_slice()
                        .get(child)
                        .ok_or(RuntimeError::Batch)?
                        .schema
                        .slots(),
                    &[relationships, predicate.current_edge],
                    context,
                )?),
                None => None,
            };
            let completed_input = match completed_schema.as_ref() {
                Some(schema) => Some(RowBatch::storage(
                    context,
                    schema.slots().len(),
                    1,
                    capacity.rows.payload_bytes,
                    capacity.rows.variable,
                )?),
                None => None,
            };
            PhysicalState::BoundedExpand {
                child,
                source,
                node,
                relationships,
                edge_predicate,
                completed_edge_predicate,
                min,
                max,
                direction,
                pattern,
                all_types: relationship_types.is_empty(),
                types,
                frames: QueryArena::new(context.memory(), usize::from(max) + 1)
                    .map_err(RuntimeError::Memory)?,
                path: QueryArena::new(context.memory(), usize::from(max))
                    .map_err(RuntimeError::Memory)?,
                nodes: QueryArena::new(context.memory(), usize::from(max) + 1)
                    .map_err(RuntimeError::Memory)?,
                edge_schema,
                edge_input,
                completed_schema,
                completed_input,
                zero_pending: false,
                active: false,
            }
        }
        OperatorKind::Join { predicate } => {
            let (left, right) = build_binary(
                view,
                plan,
                operator,
                capacity,
                occurrences,
                context,
                bindings,
            )?;
            let shared = shared_slots(occurrences, left, right, context)?;
            let logical_left = operator
                .inputs
                .first()
                .copied()
                .ok_or(PlanError::Reference)?;
            let logical_right = operator
                .inputs
                .get(1)
                .copied()
                .ok_or(PlanError::Reference)?;
            let join_plan = planner::join_plan(
                shared.len(),
                plan.plan().description().operators,
                logical_left,
                logical_right,
            );
            let build = match join_plan.build {
                planner::BuildSide::Left => left,
                planner::BuildSide::Right => right,
            };
            let build_columns = occurrences
                .as_slice()
                .get(build)
                .ok_or(RuntimeError::Batch)?
                .schema
                .slots()
                .len();
            let hash = join_plan.strategy == planner::JoinStrategy::Hash;
            let bucket_count = capacity
                .rows
                .rows
                .checked_next_power_of_two()
                .ok_or(PlanError::Limit)?;
            PhysicalState::Join {
                left,
                right,
                predicate,
                shared,
                plan: join_plan,
                build_rows: hash
                    .then(|| RetainedRows::new(build_columns, capacity, context))
                    .transpose()?,
                bucket_heads: hash
                    .then(|| QueryArena::new(context.memory(), bucket_count))
                    .transpose()
                    .map_err(RuntimeError::Memory)?,
                next_links: hash
                    .then(|| QueryArena::new(context.memory(), capacity.rows.rows))
                    .transpose()
                    .map_err(RuntimeError::Memory)?,
                build_hashes: hash
                    .then(|| QueryArena::new(context.memory(), capacity.rows.rows))
                    .transpose()
                    .map_err(RuntimeError::Memory)?,
                initialized: false,
                nested_left_active: false,
                probe_active: false,
                next_candidate: usize::MAX,
            }
        }
        OperatorKind::OptionalApply { predicate } => {
            if operator.inputs.len() != 2 {
                return Err(PlanError::Arity.into());
            }
            let logical_left = operator
                .inputs
                .first()
                .copied()
                .ok_or(PlanError::Reference)?;
            let logical_right = operator
                .inputs
                .get(1)
                .copied()
                .ok_or(PlanError::Reference)?;
            let left = build_occurrence(
                view,
                plan,
                logical_left,
                capacity,
                occurrences,
                context,
                bindings,
            )?;
            let scoped = AnchorBinding {
                logical: logical_left,
                source: left,
                parent: bindings,
            };
            let right = build_occurrence(
                view,
                plan,
                logical_right,
                capacity,
                occurrences,
                context,
                Some(&scoped),
            )?;
            let all_shared = shared_slots(occurrences, left, right, context)?;
            let mut shared = QueryArena::new(context.memory(), all_shared.len())
                .map_err(RuntimeError::Memory)?;
            for slot in all_shared.as_slice() {
                if !slot_inherited_from_anchor(
                    occurrences.as_slice(),
                    plan.plan().description().expressions,
                    right,
                    *slot,
                    left,
                )? {
                    shared.push(*slot).map_err(RuntimeError::Memory)?;
                }
            }
            PhysicalState::Optional {
                left,
                right,
                predicate,
                shared,
                left_active: false,
                matched: false,
            }
        }
        OperatorKind::Filter(predicate) => PhysicalState::Filter {
            child: build_unary(
                view,
                plan,
                operator,
                capacity,
                occurrences,
                context,
                bindings,
            )?,
            predicate,
        },
        OperatorKind::Project(projections) | OperatorKind::With(projections) => {
            let child = build_unary(
                view,
                plan,
                operator,
                capacity,
                occurrences,
                context,
                bindings,
            )?;
            let mut values = QueryArena::new(context.memory(), projections.len())
                .map_err(RuntimeError::Memory)?;
            for _ in projections {
                values
                    .push(RowBatch::storage(
                        context,
                        1,
                        1,
                        capacity.rows.payload_bytes,
                        capacity.rows.variable,
                    )?)
                    .map_err(RuntimeError::Memory)?;
            }
            PhysicalState::Project {
                child,
                projections,
                values,
            }
        }
        OperatorKind::OffsetLimit { offset, limit } => PhysicalState::OffsetLimit {
            child: build_unary(
                view,
                plan,
                operator,
                capacity,
                occurrences,
                context,
                bindings,
            )?,
            offset,
            limit,
            remaining_offset: offset,
            remaining_limit: limit,
        },
        OperatorKind::Sort(keys) => {
            let child = build_unary(
                view,
                plan,
                operator,
                capacity,
                occurrences,
                context,
                bindings,
            )?;
            PhysicalState::Sort {
                child,
                state: relational::SortState::new(
                    occurrences
                        .as_slice()
                        .get(child)
                        .ok_or(RuntimeError::Batch)?
                        .schema
                        .slots(),
                    keys,
                    capacity,
                    context,
                )?,
            }
        }
        OperatorKind::Distinct => {
            let child = build_unary(
                view,
                plan,
                operator,
                capacity,
                occurrences,
                context,
                bindings,
            )?;
            PhysicalState::Distinct {
                child,
                state: relational::DistinctState::new(
                    occurrences
                        .as_slice()
                        .get(child)
                        .ok_or(RuntimeError::Batch)?
                        .schema
                        .slots(),
                    capacity,
                    context,
                )?,
            }
        }
        OperatorKind::Eager => {
            let child = build_unary(
                view,
                plan,
                operator,
                capacity,
                occurrences,
                context,
                bindings,
            )?;
            PhysicalState::Eager {
                child,
                state: mutation::EagerState::new(
                    occurrences
                        .as_slice()
                        .get(child)
                        .ok_or(RuntimeError::Batch)?
                        .schema
                        .slots(),
                    capacity,
                    context,
                )?,
            }
        }
        OperatorKind::Mutate(items) if supported_mutations(items) => {
            let child = build_unary(
                view,
                plan,
                operator,
                capacity,
                occurrences,
                context,
                bindings,
            )?;
            PhysicalState::Mutate {
                child,
                items,
                state: mutation::EagerState::new(
                    occurrences
                        .as_slice()
                        .get(child)
                        .ok_or(RuntimeError::Batch)?
                        .schema
                        .slots(),
                    capacity,
                    context,
                )?,
            }
        }
        OperatorKind::Aggregate { keys, aggregates } => {
            let child = build_unary(
                view,
                plan,
                operator,
                capacity,
                occurrences,
                context,
                bindings,
            )?;
            PhysicalState::Aggregate {
                child,
                state: relational::AggregateState::new(
                    keys,
                    aggregates,
                    plan.plan().description().expressions,
                    capacity,
                    context,
                )?,
            }
        }
        OperatorKind::Collect => PhysicalState::Collect {
            child: build_unary(
                view,
                plan,
                operator,
                capacity,
                occurrences,
                context,
                bindings,
            )?,
        },
        _ => return Err(PlanError::Reference.into()),
    };
    let schema = schema_for(plan, node, context)?;
    let output = RowBatch::storage(
        context,
        schema.slots().len(),
        1,
        capacity.rows.payload_bytes,
        capacity.rows.variable,
    )?;
    let uses = QueryArena::new(context.memory(), capacity.rows.rows.max(16))
        .map_err(RuntimeError::Memory)?;
    let index = occurrences.len();
    occurrences
        .push(Occurrence {
            node,
            schema,
            output,
            uses,
            state,
        })
        .map_err(RuntimeError::Memory)?;
    Ok(index)
}

#[allow(clippy::too_many_arguments)]
fn build_unary<'s, 'r, 'plan, 'v, 'm, 'g>(
    view: &'s GraphReadView<'s, 'v, 'm, 'g>,
    plan: &'r RuntimePlan<'r, 'plan, 'r, 'm, 'g, 'r>,
    operator: &super::plan::Operator<'plan>,
    capacity: PatternCapacity,
    occurrences: &mut QueryArena<'m, 'g, Occurrence<'s, 'plan, 'v, 'm, 'g>>,
    context: &mut RuntimeContext<'v, 'm, 'g>,
    bindings: Option<&AnchorBinding<'_>>,
) -> Result<usize, NativeExecutionError> {
    if operator.inputs.len() != 1 {
        return Err(PlanError::Arity.into());
    }
    let input = operator
        .inputs
        .first()
        .copied()
        .ok_or(PlanError::Reference)?;
    build_occurrence(view, plan, input, capacity, occurrences, context, bindings)
}

#[allow(clippy::too_many_arguments)]
fn build_binary<'s, 'r, 'plan, 'v, 'm, 'g>(
    view: &'s GraphReadView<'s, 'v, 'm, 'g>,
    plan: &'r RuntimePlan<'r, 'plan, 'r, 'm, 'g, 'r>,
    operator: &super::plan::Operator<'plan>,
    capacity: PatternCapacity,
    occurrences: &mut QueryArena<'m, 'g, Occurrence<'s, 'plan, 'v, 'm, 'g>>,
    context: &mut RuntimeContext<'v, 'm, 'g>,
    bindings: Option<&AnchorBinding<'_>>,
) -> Result<(usize, usize), NativeExecutionError> {
    if operator.inputs.len() != 2 {
        return Err(PlanError::Arity.into());
    }
    let left = operator
        .inputs
        .first()
        .copied()
        .ok_or(PlanError::Reference)?;
    let right = operator
        .inputs
        .get(1)
        .copied()
        .ok_or(PlanError::Reference)?;
    Ok((
        build_occurrence(view, plan, left, capacity, occurrences, context, bindings)?,
        build_occurrence(view, plan, right, capacity, occurrences, context, bindings)?,
    ))
}

fn slot_inherited_from_anchor(
    occurrences: &[Occurrence<'_, '_, '_, '_, '_>],
    expressions: &[super::plan::Expression<'_>],
    index: usize,
    slot: SlotId,
    anchor: usize,
) -> Result<bool, NativeExecutionError> {
    let occurrence = occurrences.get(index).ok_or(RuntimeError::Batch)?;
    if !occurrence.schema.slots().contains(&slot) {
        return Ok(false);
    }
    let inherited = match &occurrence.state {
        PhysicalState::Vacant => return Err(RuntimeError::Batch.into()),
        PhysicalState::Anchor { source, .. } => *source == anchor,
        PhysicalState::Unit { .. } => false,
        PhysicalState::ScanNodes { child, output, .. }
        | PhysicalState::LookupNode { child, output, .. }
        | PhysicalState::LookupRelationship { child, output, .. }
        | PhysicalState::LookupKey { child, output, .. } => {
            *output != slot
                && slot_inherited_from_anchor(occurrences, expressions, *child, slot, anchor)?
        }
        PhysicalState::Expand {
            child,
            node,
            relationship,
            ..
        } => {
            *node != slot
                && *relationship != slot
                && slot_inherited_from_anchor(occurrences, expressions, *child, slot, anchor)?
        }
        PhysicalState::BoundedExpand {
            child,
            node,
            relationships,
            ..
        } => {
            *node != slot
                && *relationships != slot
                && slot_inherited_from_anchor(occurrences, expressions, *child, slot, anchor)?
        }
        PhysicalState::Join { left, right, .. } | PhysicalState::Optional { left, right, .. } => {
            let left_occurrence = occurrences.get(*left).ok_or(RuntimeError::Batch)?;
            if left_occurrence.schema.slots().contains(&slot) {
                slot_inherited_from_anchor(occurrences, expressions, *left, slot, anchor)?
            } else {
                slot_inherited_from_anchor(occurrences, expressions, *right, slot, anchor)?
            }
        }
        PhysicalState::Filter { child, .. }
        | PhysicalState::OffsetLimit { child, .. }
        | PhysicalState::Sort { child, .. }
        | PhysicalState::Distinct { child, .. }
        | PhysicalState::Eager { child, .. }
        | PhysicalState::Mutate { child, .. }
        | PhysicalState::Collect { child } => {
            slot_inherited_from_anchor(occurrences, expressions, *child, slot, anchor)?
        }
        PhysicalState::Aggregate { child, state } => {
            let state = state.as_slice().first().ok_or(RuntimeError::Batch)?;
            let Some(expression) = state.inherited_key_expression(slot) else {
                return Ok(false);
            };
            matches!(
                expressions.get(expression.0 as usize),
                Some(super::plan::Expression::Slot(source)) if *source == slot
            ) && slot_inherited_from_anchor(occurrences, expressions, *child, slot, anchor)?
        }
        PhysicalState::Project {
            child, projections, ..
        } => {
            let Some(projection) = projections
                .iter()
                .find(|projection| projection.slot == slot)
            else {
                return Ok(false);
            };
            matches!(
                expressions.get(projection.expression.0 as usize),
                Some(super::plan::Expression::Slot(source)) if *source == slot
            ) && slot_inherited_from_anchor(occurrences, expressions, *child, slot, anchor)?
        }
    };
    Ok(inherited)
}

fn shared_slots<'m, 'g>(
    occurrences: &QueryArena<'m, 'g, Occurrence<'_, '_, '_, 'm, 'g>>,
    left: usize,
    right: usize,
    context: &RuntimeContext<'_, 'm, 'g>,
) -> Result<QueryArena<'m, 'g, SlotId>, NativeExecutionError> {
    let left = occurrences
        .as_slice()
        .get(left)
        .ok_or(RuntimeError::Batch)?;
    let right = occurrences
        .as_slice()
        .get(right)
        .ok_or(RuntimeError::Batch)?;
    let capacity = left.schema.slots().len().min(right.schema.slots().len());
    let mut shared = QueryArena::new(context.memory(), capacity).map_err(RuntimeError::Memory)?;
    for slot in left.schema.slots() {
        if right.schema.column(*slot).is_ok() {
            shared.push(*slot).map_err(RuntimeError::Memory)?;
        }
    }
    Ok(shared)
}

fn schema_for<'r, 'plan, 'm, 'g>(
    plan: &RuntimePlan<'r, 'plan, 'r, 'm, 'g, 'r>,
    node: PlanNodeId,
    context: &RuntimeContext<'_, 'm, 'g>,
) -> Result<Schema<'m, 'g>, NativeExecutionError> {
    let facts = plan.plan().facts(node).ok_or(PlanError::Reference)?;
    let mut slots =
        QueryArena::new(context.memory(), facts.width()).map_err(RuntimeError::Memory)?;
    for index in 0..facts.width() {
        slots
            .push(facts.slot_at(index).ok_or(PlanError::Reference)?.0)
            .map_err(RuntimeError::Memory)?;
    }
    Ok(Schema::new(context, slots.as_slice())?)
}

fn private_schema<'m, 'g>(
    input: &[SlotId],
    additions: &[SlotId],
    context: &RuntimeContext<'_, 'm, 'g>,
) -> Result<Schema<'m, 'g>, NativeExecutionError> {
    let capacity = input
        .len()
        .checked_add(additions.len())
        .ok_or(RuntimeError::Batch)?;
    let mut slots = QueryArena::new(context.memory(), capacity).map_err(RuntimeError::Memory)?;
    for slot in input.iter().chain(additions) {
        slots.push(*slot).map_err(RuntimeError::Memory)?;
    }
    Ok(Schema::new(context, slots.as_slice())?)
}

fn resolve_relationship_types<'m, 'g>(
    view: &GraphReadView<'_, '_, 'm, 'g>,
    names: &[GraphName<'_>],
    context: &mut RuntimeContext<'_, 'm, 'g>,
) -> Result<QueryArena<'m, 'g, RelTypeId>, NativeExecutionError> {
    let mut types = QueryArena::new(context.memory(), names.len()).map_err(RuntimeError::Memory)?;
    let mut resources = TreeResources::for_query(context)?;
    for name in names {
        match view.expression_symbol(SymbolKind::RelationshipType, *name, &mut resources)? {
            Some(Symbol::RelationshipType(kind)) if !types.as_slice().contains(&kind) => {
                types.push(kind).map_err(RuntimeError::Memory)?;
            }
            Some(Symbol::RelationshipType(_)) | None => {}
            Some(_) => return Err(PlanError::Type.into()),
        }
    }
    Ok(types)
}

fn child_parent<'a, 's, 'plan, 'v, 'm, 'g>(
    occurrences: &'a mut [Occurrence<'s, 'plan, 'v, 'm, 'g>],
    child: usize,
    parent: usize,
) -> Result<
    (
        &'a Occurrence<'s, 'plan, 'v, 'm, 'g>,
        &'a mut Occurrence<'s, 'plan, 'v, 'm, 'g>,
    ),
    RuntimeError,
> {
    if child >= parent {
        return Err(RuntimeError::Batch);
    }
    let (before, after) = occurrences.split_at_mut(parent);
    let child = before.get(child).ok_or(RuntimeError::Batch)?;
    let parent = after.first_mut().ok_or(RuntimeError::Batch)?;
    Ok((child, parent))
}

fn copy_relationship_uses(
    source: &QueryArena<'_, '_, RelationshipUse>,
    destination: &mut QueryArena<'_, '_, RelationshipUse>,
) -> Result<(), RuntimeError> {
    for value in source.as_slice() {
        if !destination.as_slice().contains(value) {
            destination.push(*value).map_err(RuntimeError::Memory)?;
        }
    }
    Ok(())
}

fn uses_compatible(left: &[RelationshipUse], right: &[RelationshipUse]) -> bool {
    !left.iter().any(|left| {
        right.iter().any(|right| {
            left.pattern == right.pattern
                && left.relationship == right.relationship
                && left.origin != right.origin
        })
    })
}

fn relationship_selection<'a>(
    all: bool,
    types: &'a QueryArena<'_, '_, RelTypeId>,
) -> RelationshipTypeSelection<'a> {
    if all {
        RelationshipTypeSelection::All
    } else {
        RelationshipTypeSelection::Any(types.as_slice())
    }
}

fn key_expression_error(expression: ExprId, error: RuntimeError) -> NativeExecutionError {
    NativeExecutionError::Expression(ExpressionError {
        expression,
        failure: ExpressionFailure::Runtime(error),
    })
}

fn validate_lookup_key_bounds(
    namespace_bytes: usize,
    key_bytes: usize,
    expression: ExprId,
) -> Result<(), NativeExecutionError> {
    if namespace_bytes
        .checked_add(key_bytes)
        .is_none_or(|bytes| bytes > MAX_GRAPH_INPUT_BYTES)
        || key_bytes
            .checked_add(9)
            .is_none_or(|bytes| bytes > MAX_GRAPH_INPUT_BYTES)
    {
        return Err(NativeExecutionError::Expression(ExpressionError {
            expression,
            failure: ExpressionFailure::Plan(PlanError::Limit),
        }));
    }
    Ok(())
}

const fn direction_selection(direction: Direction) -> DirectionSelection {
    match direction {
        Direction::Outgoing => DirectionSelection::Out,
        Direction::Incoming => DirectionSelection::In,
        Direction::Either => DirectionSelection::Undirected,
    }
}

fn neighbor(
    source: NodeId,
    relationship: RelationshipRow,
    direction: Direction,
) -> Result<NodeId, RuntimeError> {
    match direction {
        Direction::Outgoing if relationship.source == source => Ok(relationship.target),
        Direction::Incoming if relationship.target == source => Ok(relationship.source),
        Direction::Either if relationship.source == source => Ok(relationship.target),
        Direction::Either if relationship.target == source => Ok(relationship.source),
        _ => Err(RuntimeError::Batch),
    }
}

fn minimum_node() -> Result<NodeId, RuntimeError> {
    NodeId::new(1).map_err(|_| RuntimeError::Batch)
}

fn empty_relationship() -> Result<RelationshipRow, RuntimeError> {
    Ok(RelationshipRow {
        rel: RelId::new(1).map_err(|_| RuntimeError::Batch)?,
        source: minimum_node()?,
        target: minimum_node()?,
        relationship_type: RelTypeId::new(1).map_err(|_| RuntimeError::Batch)?,
    })
}
