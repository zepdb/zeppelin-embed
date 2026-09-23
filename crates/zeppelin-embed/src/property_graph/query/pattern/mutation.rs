//! Native occurrences for the mutation-clause barrier and its SET, REMOVE and
//! label items.
//!
//! `Eager` freezes the complete upstream bag and its computed cells before any
//! mutation clause runs: on its first pull it drains its child to exhaustion,
//! retaining every row and the relationship uses that produced it, and only
//! then begins emitting. A clause that writes therefore never observes a row
//! its own statement produced, and a reset re-drains the child so a repeated
//! occurrence under a nested-loop join freezes each input scope separately.
//!
//! `Mutate` has the same drain-then-emit shape, and applies its items to each
//! row as the row is drained. It is a barrier, not a stream: every input row
//! is mutated before the first output row is emitted, so a following clause
//! sees the completed effects of the whole clause. Three input rows that each
//! run `SET n.p = n.p + 1` on one node all return the final value.
//!
//! Items run in textual order. Each item reads through the statement overlay,
//! so it sees every earlier item of this row and every earlier row. An item
//! rebuilds the target's complete image, from the image an earlier item
//! staged when there is one and from the admitted read view otherwise, applies
//! its one edit, and replaces the staged image. A null target skips the item.
//!
//! A reset re-drains the child exactly as `Eager` does, so an occurrence that
//! is executed again after a reset applies its items again, as a re-executed
//! clause would.
//!
//! A `Mutate` row is staged into the occurrence's own output batch, which has
//! the clause's output schema: the input row's cells plus one cell for every
//! entity the clause creates. Items evaluate against that row, so a later item
//! reads a node or relationship an earlier item created.
//!
//! `CreateNode` and `CreateRelationship` start from an empty image, stage it
//! with `GraphBatchReadView::create_fresh`, and bind the identity that call
//! allocates into their output cell. The identity is an ordinary `Existing`
//! reference from then on: a SET on it rebuilds from the staged image exactly
//! as it does for an existing entity, and every later read finds that image
//! first. Scans read only the admitted base view, and the `Eager` input has
//! drained them before the first item runs, so no scan observes a create.
//!
//! A plain `Delete` stages a `Restrict` tombstone through the overlay. From
//! then on every read or write of that entity through the overlay, by a later
//! item, a later row or a later clause, fails with the typed
//! `StageError::DeletedEntity`, and a CREATE that names it as an endpoint
//! fails the same way. Whether a deleted node still has a live incident
//! relationship is not decided here: the overlay decides it once, when the
//! statement is finalized, so `DELETE n, r` and `DELETE r, n` agree. Deleting
//! an entity twice is one change, and a null target skips the item.
//!
//! `DETACH DELETE` stays refused at build time by `occurrence_count`, and is
//! refused again here should one ever arrive.

use super::super::property::PropertyScratch;
use super::super::relational::Rows;
use super::relational::copy_relationship_uses_slice;
use super::*;
use crate::property_graph::staging::{
    BatchEntityRef, NodeImageBudget, RelationshipImageBudget, WriteImage,
};
use crate::property_graph::storage::records::RecordShape;
use crate::property_graph::storage::tree::directory::TreeError;
use crate::property_graph::storage::{NativeQuerySource, NodeView, RelView};
use crate::property_graph::{
    CanonicalContents, EntityId, GraphDeleteMode, GraphProperty, NodeRef as BatchNode,
    PropertyValue, RelRef as BatchRelationship,
};

#[cfg(test)]
mod tests;

pub(super) struct EagerState<'v, 'm, 'g> {
    slots: QueryArena<'m, 'g, SlotId>,
    rows: Option<Rows<'v, 'm, 'g>>,
    spans: QueryArena<'m, 'g, UseSpan>,
    uses: QueryArena<'m, 'g, RelationshipUse>,
    capacity: PatternCapacity,
    started: bool,
    next: usize,
    /// For a `Mutate`: the input column each output column copies, or `None`
    /// for a column a CREATE item binds. Empty for an `Eager`.
    sources: QueryArena<'m, 'g, Option<usize>>,
    /// For a `Mutate`: the identity each CREATE output column holds for the
    /// row being mutated. Empty for an `Eager`.
    created: QueryArena<'m, 'g, Option<EntityId>>,
}

impl<'v, 'm, 'g> EagerState<'v, 'm, 'g> {
    pub(super) fn new(
        slots: &[SlotId],
        capacity: PatternCapacity,
        context: &RuntimeContext<'v, 'm, 'g>,
    ) -> Result<QueryArena<'m, 'g, Self>, NativeExecutionError> {
        Self::build(slots, None, capacity, context)
    }

    /// The state of a `Mutate` whose output schema is `slots` over an input
    /// with schema `input`. Output columns absent from the input are the
    /// columns its CREATE items bind.
    pub(super) fn mutate(
        slots: &[SlotId],
        input: &Schema<'m, 'g>,
        capacity: PatternCapacity,
        context: &RuntimeContext<'v, 'm, 'g>,
    ) -> Result<QueryArena<'m, 'g, Self>, NativeExecutionError> {
        Self::build(slots, Some(input), capacity, context)
    }

    fn build(
        slots: &[SlotId],
        input: Option<&Schema<'m, 'g>>,
        capacity: PatternCapacity,
        context: &RuntimeContext<'v, 'm, 'g>,
    ) -> Result<QueryArena<'m, 'g, Self>, NativeExecutionError> {
        let width = if input.is_some() { slots.len() } else { 0 };
        let mut sources = QueryArena::new(context.memory(), width).map_err(RuntimeError::Memory)?;
        let mut created = QueryArena::new(context.memory(), width).map_err(RuntimeError::Memory)?;
        if let Some(input) = input {
            for slot in slots {
                sources
                    .push(input.column(*slot).ok())
                    .map_err(RuntimeError::Memory)?;
                created.push(None).map_err(RuntimeError::Memory)?;
            }
        }
        let mut owner = QueryArena::new(context.memory(), 1).map_err(RuntimeError::Memory)?;
        let mut owned_slots =
            QueryArena::new(context.memory(), slots.len()).map_err(RuntimeError::Memory)?;
        for slot in slots {
            owned_slots.push(*slot).map_err(RuntimeError::Memory)?;
        }
        let rows = Rows::new(context, slots, capacity.rows)?;
        let use_capacity = capacity
            .rows
            .rows
            .checked_mul(16)
            .ok_or(RuntimeError::Batch)?;
        owner
            .push(Self {
                slots: owned_slots,
                rows: Some(rows),
                spans: QueryArena::new(context.memory(), capacity.rows.rows)
                    .map_err(RuntimeError::Memory)?,
                uses: QueryArena::new(context.memory(), use_capacity)
                    .map_err(RuntimeError::Memory)?,
                capacity,
                started: false,
                next: 0,
                sources,
                created,
            })
            .map_err(RuntimeError::Memory)?;
        Ok(owner)
    }

    pub(super) fn reset(
        &mut self,
        context: &RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        self.rows.take();
        self.rows = Some(Rows::new(
            context,
            self.slots.as_slice(),
            self.capacity.rows,
        )?);
        self.spans.clear();
        self.uses.clear();
        self.started = false;
        self.next = 0;
        Ok(())
    }
}

impl<'s, 'r, 'plan, 'v, 'm, 'g, 'i> NativePattern<'s, 'r, 'plan, 'v, 'm, 'g, 'i> {
    pub(super) fn next_eager(
        &mut self,
        index: usize,
        child: usize,
        state: &mut QueryArena<'m, 'g, EagerState<'v, 'm, 'g>>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<bool, NativeExecutionError> {
        self.next_barrier(index, child, state, None, context)
    }

    pub(super) fn next_mutate(
        &mut self,
        index: usize,
        child: usize,
        items: &'plan [Mutation<'plan>],
        state: &mut QueryArena<'m, 'g, EagerState<'v, 'm, 'g>>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<bool, NativeExecutionError> {
        self.next_barrier(index, child, state, Some(items), context)
    }

    /// Drains `child` completely on the first pull, applying `items` to each
    /// row as it arrives, and only then emits the retained rows in order.
    fn next_barrier(
        &mut self,
        index: usize,
        child: usize,
        state: &mut QueryArena<'m, 'g, EagerState<'v, 'm, 'g>>,
        items: Option<&'plan [Mutation<'plan>]>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<bool, NativeExecutionError> {
        let state = state
            .as_mut_slice()
            .first_mut()
            .ok_or(RuntimeError::Batch)?;
        if !state.started {
            state.started = true;
            let mut rows = state.rows.take().ok_or(RuntimeError::Batch)?;
            while self.next_occurrence(child, context)? {
                context.charge(WorkKind::OperatorRows, 1)?;
                context.charge(WorkKind::RowsIn, 1)?;
                // A `Mutate` stages each row into its own output batch and
                // retains that row; an `Eager` retains the input row as is.
                let source = match items {
                    Some(items) => {
                        self.apply_mutations(
                            index,
                            child,
                            items,
                            state.sources.as_slice(),
                            &mut state.created,
                            context,
                        )?;
                        index
                    }
                    None => child,
                };
                let mut values = QueryArena::new(context.memory(), rows.schema().slots().len())
                    .map_err(RuntimeError::Memory)?;
                {
                    let occurrence = self.occurrence(source)?;
                    for column in 0..occurrence.output.columns() {
                        values
                            .push(
                                occurrence
                                    .output
                                    .value(0, column)
                                    .ok_or(RuntimeError::Batch)?,
                            )
                            .map_err(RuntimeError::Memory)?;
                    }
                }
                rows.push(values.as_slice(), context)?;
                drop(values);
                if items.is_some() {
                    self.output_mut(index)?.clear();
                }
                let start = state.uses.len();
                for usage in self.occurrence(child)?.uses.as_slice() {
                    state.uses.push(*usage).map_err(RuntimeError::Memory)?;
                }
                state
                    .spans
                    .push(UseSpan {
                        start,
                        len: state.uses.len() - start,
                    })
                    .map_err(RuntimeError::Memory)?;
            }
            state.rows = Some(rows);
        }
        let rows = state.rows.as_ref().ok_or(RuntimeError::Batch)?;
        if state.next == rows.len() {
            return Ok(false);
        }
        let parent = self.occurrence_mut(index)?;
        parent.output.push_from(
            |column| rows.value(state.next, column).ok_or(RuntimeError::Batch),
            context,
        )?;
        let span = state
            .spans
            .as_slice()
            .get(state.next)
            .ok_or(RuntimeError::Batch)?;
        let end = span
            .start
            .checked_add(span.len)
            .ok_or(RuntimeError::Batch)?;
        let uses = state
            .uses
            .as_slice()
            .get(span.start..end)
            .ok_or(RuntimeError::Batch)?;
        copy_relationship_uses_slice(uses, &mut parent.uses)?;
        state.next += 1;
        Ok(true)
    }

    /// Applies every item, in textual order, to the row `child` currently
    /// holds, staged as the one row of occurrence `index`'s output batch.
    #[allow(
        clippy::too_many_arguments,
        reason = "the row mapping stays explicit beside both occurrences"
    )]
    fn apply_mutations(
        &mut self,
        index: usize,
        child: usize,
        items: &'plan [Mutation<'plan>],
        sources: &[Option<usize>],
        created: &mut QueryArena<'m, 'g, Option<EntityId>>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), NativeExecutionError> {
        for cell in created.as_mut_slice() {
            *cell = None;
        }
        self.stage_row(index, child, sources, created.as_slice(), context)?;
        for item in items {
            context.checkpoint()?;
            match *item {
                Mutation::SetProperty {
                    entity,
                    name,
                    value,
                } => {
                    let Some(target) = self.mutation_target(index, entity, context)? else {
                        continue;
                    };
                    let occurrence = self
                        .occurrences
                        .as_slice()
                        .get(index)
                        .ok_or(RuntimeError::Batch)?;
                    let evaluated = evaluate_at(
                        &mut self.evaluator,
                        self.mutation.as_mut(),
                        value,
                        &occurrence.schema,
                        &occurrence.output,
                        0,
                        self.view,
                        context,
                    )?;
                    let scope = self.mutation.as_mut().ok_or(PlanError::Reference)?;
                    assign(scope, self.view, target, name, evaluated, context)?;
                }
                Mutation::RemoveProperty { entity, name } => {
                    let Some(target) = self.mutation_target(index, entity, context)? else {
                        continue;
                    };
                    let scope = self.mutation.as_mut().ok_or(PlanError::Reference)?;
                    rebuild(
                        scope,
                        self.view,
                        target,
                        Edit::Property(name, None),
                        context,
                    )?;
                }
                Mutation::SetLabel {
                    entity,
                    label,
                    present,
                } => {
                    let Some(target) = self.mutation_target(index, entity, context)? else {
                        continue;
                    };
                    if !matches!(target, BatchEntityRef::Node(_)) {
                        return Err(RuntimeError::Value(QueryError::Type).into());
                    }
                    let scope = self.mutation.as_mut().ok_or(PlanError::Reference)?;
                    rebuild(
                        scope,
                        self.view,
                        target,
                        Edit::Label(label, present),
                        context,
                    )?;
                }
                Mutation::CreateNode { output, labels } => {
                    let scope = self.mutation.as_mut().ok_or(PlanError::Reference)?;
                    let id = EntityId::Node(create_node(scope, labels, context)?);
                    self.bind_created(index, child, output, id, sources, created, context)?;
                }
                Mutation::CreateRelationship {
                    output,
                    source,
                    target,
                    relationship_type,
                } => {
                    let source = self.endpoint(index, source, context)?;
                    let target = self.endpoint(index, target, context)?;
                    let scope = self.mutation.as_mut().ok_or(PlanError::Reference)?;
                    let id =
                        create_relationship(scope, source, target, relationship_type, context)?;
                    self.bind_created(index, child, output, id, sources, created, context)?;
                }
                Mutation::Delete {
                    entity,
                    detach: false,
                } => {
                    let Some(target) = self.mutation_target(index, entity, context)? else {
                        continue;
                    };
                    let scope = self.mutation.as_mut().ok_or(PlanError::Reference)?;
                    let control = context.values().control();
                    let mut control = |_: WritePhase| writer_checkpoint(control);
                    scope
                        .overlay
                        .delete(target, GraphDeleteMode::Restrict, &mut control)?;
                }
                Mutation::Delete { detach: true, .. } => {
                    return Err(PlanError::Reference.into());
                }
            }
        }
        Ok(())
    }

    /// Replaces occurrence `index`'s output with one row: every input column
    /// copied from `child`'s current row, and every created column holding
    /// the identity bound so far, or null before its CREATE item has run.
    fn stage_row(
        &mut self,
        index: usize,
        child: usize,
        sources: &[Option<usize>],
        created: &[Option<EntityId>],
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), NativeExecutionError> {
        let view = context.view();
        let (child, parent) = child_parent(self.occurrences.as_mut_slice(), child, index)?;
        parent.output.clear();
        parent.output.push_from(
            |column| match created.get(column).ok_or(RuntimeError::Batch)? {
                Some(EntityId::Node(id)) => Ok(view.node(*id)),
                Some(EntityId::Relationship(id)) => Ok(view.relationship(*id)),
                None => match sources.get(column).ok_or(RuntimeError::Batch)? {
                    Some(input) => child.output.value(0, *input).ok_or(RuntimeError::Batch),
                    None => Ok(QueryValue::Null),
                },
            },
            context,
        )?;
        Ok(())
    }

    /// Binds a created identity into its output column and restages the row
    /// so every later item reads it.
    #[allow(
        clippy::too_many_arguments,
        reason = "the row mapping stays explicit beside both occurrences"
    )]
    fn bind_created(
        &mut self,
        index: usize,
        child: usize,
        output: SlotId,
        id: EntityId,
        sources: &[Option<usize>],
        created: &mut QueryArena<'m, 'g, Option<EntityId>>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), NativeExecutionError> {
        let column = self.occurrence(index)?.schema.column(output)?;
        if sources.get(column).ok_or(RuntimeError::Batch)?.is_some() {
            // The plan validator refuses a CREATE into a bound slot.
            return Err(PlanError::Reference.into());
        }
        *created
            .as_mut_slice()
            .get_mut(column)
            .ok_or(RuntimeError::Batch)? = Some(id);
        self.stage_row(index, child, sources, created.as_slice(), context)
    }

    /// Resolves one relationship endpoint. A null endpoint cannot be created
    /// against and is a typed endpoint failure; anything but a node is a type
    /// error.
    fn endpoint(
        &mut self,
        index: usize,
        expression: ExprId,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<NodeId, NativeExecutionError> {
        let occurrence = self
            .occurrences
            .as_slice()
            .get(index)
            .ok_or(RuntimeError::Batch)?;
        let value = evaluate_at(
            &mut self.evaluator,
            self.mutation.as_mut(),
            expression,
            &occurrence.schema,
            &occurrence.output,
            0,
            self.view,
            context,
        )?;
        match value {
            QueryValue::NodeRef(node) => Ok(node.id()),
            QueryValue::Null => Err(StageError::Endpoint.into()),
            _ => Err(RuntimeError::Value(QueryError::Type).into()),
        }
    }

    /// Resolves one item's receiver against occurrence `index`'s staged row.
    /// Null skips the item; anything but an entity reference is a type error.
    fn mutation_target(
        &mut self,
        index: usize,
        entity: ExprId,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<Option<BatchEntityRef<'static>>, NativeExecutionError> {
        let occurrence = self
            .occurrences
            .as_slice()
            .get(index)
            .ok_or(RuntimeError::Batch)?;
        let value = evaluate_at(
            &mut self.evaluator,
            self.mutation.as_mut(),
            entity,
            &occurrence.schema,
            &occurrence.output,
            0,
            self.view,
            context,
        )?;
        match value {
            QueryValue::Null => Ok(None),
            QueryValue::NodeRef(node) => {
                Ok(Some(BatchEntityRef::Node(BatchNode::Existing(node.id()))))
            }
            QueryValue::RelRef(relationship) => Ok(Some(BatchEntityRef::Relationship(
                BatchRelationship::Existing(relationship.id()),
            ))),
            _ => Err(RuntimeError::Value(QueryError::Type).into()),
        }
    }
}

/// Stages one new node carrying exactly `labels` and returns its identity.
/// A CREATE has no prior image to measure: its budget is the labels alone,
/// and its properties arrive through the SET items that follow it.
fn create_node(
    scope: &mut MutationScope<'_, '_>,
    labels: &[GraphName<'_>],
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<NodeId, NativeExecutionError> {
    let control = context.values().control();
    let mut control = |_: WritePhase| writer_checkpoint(control);
    let images = scope.images;
    let mut budget = NodeImageBudget::default();
    for label in labels {
        control(WritePhase::Overlay)?;
        budget.label(*label)?;
    }
    let mut builder = images.node(budget, &mut control)?;
    for label in labels {
        builder.label(*label, &mut control)?;
    }
    let image = builder.finish(&mut control)?;
    match scope
        .overlay
        .create_fresh(WriteImage::Node(image), &mut control)?
    {
        EntityId::Node(id) => Ok(id),
        EntityId::Relationship(_) => Err(StageError::InvalidInput.into()),
    }
}

/// Stages one new relationship with no properties and returns its identity.
/// Either endpoint may be a node this statement created; neither may be one
/// it deleted.
fn create_relationship(
    scope: &mut MutationScope<'_, '_>,
    source: NodeId,
    target: NodeId,
    relationship_type: GraphName<'_>,
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<EntityId, NativeExecutionError> {
    let control = context.values().control();
    let mut control = |_: WritePhase| writer_checkpoint(control);
    let budget = RelationshipImageBudget {
        type_bytes: relationship_type.as_str().len(),
        ..RelationshipImageBudget::default()
    };
    // An endpoint this statement already deleted is typed here, at the item
    // that names it, through the same deleted-entity check every overlay read
    // makes.
    for endpoint in [source, target] {
        scope.overlay.pending_image(
            BatchEntityRef::Node(BatchNode::Existing(endpoint)),
            &mut control,
        )?;
    }
    let builder = scope.images.relationship(budget, &mut control)?;
    let image = builder.finish(
        BatchNode::Existing(source),
        BatchNode::Existing(target),
        relationship_type,
        &mut control,
    )?;
    match scope.overlay.create_fresh(image, &mut control)? {
        id @ EntityId::Relationship(_) => Ok(id),
        EntityId::Node(_) => Err(StageError::InvalidInput.into()),
    }
}

/// The one change an item makes to its target's complete image.
#[derive(Clone, Copy)]
enum Edit<'e> {
    /// Replace the named property, or remove it when the value is `None`.
    Property(GraphName<'e>, Option<PropertyValue<'e>>),
    /// Add (`true`) or remove (`false`) the named label.
    Label(GraphName<'e>, bool),
}

impl Edit<'_> {
    /// True when this edit replaces or removes the property `name`, so the
    /// copy of the prior image must leave that property out.
    fn replaces_property(self, name: GraphName<'_>) -> bool {
        matches!(self, Self::Property(target, _) if target.as_str() == name.as_str())
    }
}

/// Converts one SET value into a storable property through the shared query
/// assignment rules, then stages it. Null removes the property.
fn assign<'s, 'v, 'm, 'g>(
    scope: &mut MutationScope<'s, '_>,
    view: &'s GraphReadView<'s, 'v, 'm, 'g>,
    target: BatchEntityRef<'static>,
    name: GraphName<'_>,
    value: QueryValue<'_>,
    context: &mut RuntimeContext<'v, 'm, 'g>,
) -> Result<(), NativeExecutionError> {
    let list = match value {
        QueryValue::List(list) if !list.is_empty() => Some(list),
        _ => None,
    };
    let length = list.map_or(0, |list| list.len());
    let first = list.and_then(|list| list.get(0));
    let mut strings = None;
    let mut bools = None;
    let mut integers = None;
    let mut floats = None;
    let scratch = match first {
        None => PropertyScratch::None,
        Some(QueryValue::String(_)) => PropertyScratch::Strings(
            strings
                .insert(list_scratch(length, "", context)?)
                .as_mut_slice(),
        ),
        Some(QueryValue::Bool(_)) => PropertyScratch::Bools(
            bools
                .insert(list_scratch(length, false, context)?)
                .as_mut_slice(),
        ),
        Some(QueryValue::I64(_)) => PropertyScratch::Integers(
            integers
                .insert(list_scratch(length, 0_i64, context)?)
                .as_mut_slice(),
        ),
        Some(QueryValue::F64(_)) => PropertyScratch::Floats(
            floats
                .insert(list_scratch(length, 0.0_f64, context)?)
                .as_mut_slice(),
        ),
        Some(_) => return Err(RuntimeError::Value(QueryError::Type).into()),
    };
    let assignment = value
        .to_property(scratch, context.values())
        .map_err(RuntimeError::Value)?;
    let value = assignment
        .data()
        .map(PropertyValue::new)
        .transpose()
        .map_err(|_| RuntimeError::Value(QueryError::PropertyLimit))?;
    rebuild(scope, view, target, Edit::Property(name, value), context)
}

/// One charged, fully initialized list-conversion buffer.
fn list_scratch<'m, 'g, T: Copy>(
    length: usize,
    fill: T,
    context: &RuntimeContext<'_, 'm, 'g>,
) -> Result<QueryArena<'m, 'g, T>, NativeExecutionError> {
    let mut scratch = QueryArena::new(context.memory(), length).map_err(RuntimeError::Memory)?;
    for _ in 0..length {
        scratch.push(fill).map_err(RuntimeError::Memory)?;
    }
    Ok(scratch)
}

/// Rebuilds `target`'s complete image with `edit` applied and stages it.
///
/// The image an earlier item or row staged wins over the read view, which is
/// what makes later items read earlier ones. The image is walked twice: once
/// to measure every buffer exactly, and once to copy into buffers of exactly
/// that size.
fn rebuild<'s, 'v, 'm, 'g>(
    scope: &mut MutationScope<'s, '_>,
    view: &'s GraphReadView<'s, 'v, 'm, 'g>,
    target: BatchEntityRef<'static>,
    edit: Edit<'_>,
    context: &mut RuntimeContext<'v, 'm, 'g>,
) -> Result<(), NativeExecutionError> {
    let control = context.values().control();
    let mut control = |_: WritePhase| writer_checkpoint(control);
    let images = scope.images;
    let pending = scope.overlay.pending_image(target, &mut control)?;
    let image = match target {
        BatchEntityRef::Node(BatchNode::Existing(id)) => {
            let source = match pending {
                Some(WriteImage::Node(image)) => NodeSource::Pending(image),
                Some(WriteImage::Relationship { .. }) => {
                    return Err(StageError::InvalidInput.into());
                }
                None => {
                    let mut resources = TreeResources::for_query(context)?;
                    let node = view.lookup_node(id, &mut resources)?;
                    NodeSource::Base(node.ok_or(StageError::MissingEntity)?)
                }
            };
            let mut budget = NodeImageBudget::default();
            walk_node(
                &source,
                id,
                edit,
                view,
                &mut scope.overlay,
                &mut budget,
                &mut control,
                context,
            )?;
            let mut builder = images.node(budget, &mut control)?;
            walk_node(
                &source,
                id,
                edit,
                view,
                &mut scope.overlay,
                &mut builder,
                &mut control,
                context,
            )?;
            WriteImage::Node(builder.finish(&mut control)?)
        }
        BatchEntityRef::Relationship(BatchRelationship::Existing(id)) => {
            if matches!(edit, Edit::Label(..)) {
                return Err(RuntimeError::Value(QueryError::Type).into());
            }
            let source = match pending {
                Some(WriteImage::Relationship {
                    source,
                    target,
                    relationship_type,
                    properties,
                }) => RelationshipSource::Pending {
                    source,
                    target,
                    relationship_type,
                    properties,
                },
                Some(WriteImage::Node(_)) => return Err(StageError::InvalidInput.into()),
                None => {
                    let mut resources = TreeResources::for_query(context)?;
                    let relationship = view.lookup_relationship(id, &mut resources)?;
                    RelationshipSource::Base(relationship.ok_or(StageError::MissingEntity)?)
                }
            };
            let mut budget = RelationshipImageBudget::default();
            let (source_node, target_node, relationship_type) = walk_relationship(
                &source,
                id,
                edit,
                view,
                &mut scope.overlay,
                &mut budget,
                &mut control,
                context,
            )?;
            budget.type_bytes = relationship_type.as_str().len();
            let mut builder = images.relationship(budget, &mut control)?;
            walk_relationship(
                &source,
                id,
                edit,
                view,
                &mut scope.overlay,
                &mut builder,
                &mut control,
                context,
            )?;
            builder.finish(source_node, target_node, relationship_type, &mut control)?
        }
        BatchEntityRef::Node(BatchNode::Local(_))
        | BatchEntityRef::Relationship(BatchRelationship::Local(_)) => {
            return Err(StageError::InvalidInput.into());
        }
    };
    scope.overlay.replace(target, image, &mut control)?;
    Ok(())
}

/// Where a node's current complete image comes from.
#[allow(
    clippy::large_enum_variant,
    reason = "one short-lived stack value per mutation item, never stored"
)]
enum NodeSource<'s, 'v, 'm, 'g> {
    /// An image an earlier item of this statement staged.
    Pending(&'s CanonicalContents<'s>),
    /// The node as the admitted read view holds it.
    Base(NodeView<'s, NativeQuerySource<'v, 'm, 'g>>),
}

/// Where a relationship's current complete image comes from.
#[allow(
    clippy::large_enum_variant,
    reason = "one short-lived stack value per mutation item, never stored"
)]
enum RelationshipSource<'s, 'v, 'm, 'g> {
    /// An image an earlier item of this statement staged.
    Pending {
        source: BatchNode<'static>,
        target: BatchNode<'static>,
        relationship_type: GraphName<'s>,
        properties: &'s [GraphProperty<'s>],
    },
    /// The relationship as the admitted read view holds it.
    Base(RelView<'s, NativeQuerySource<'v, 'm, 'g>>),
}

/// Receives one image in the order the walk produces it. The measuring pass
/// and the copying pass implement this over the same walk, so the budget and
/// the copy cannot disagree.
trait ImageSink {
    fn label(
        &mut self,
        _name: GraphName<'_>,
        _control: &mut WriteControl<'_>,
    ) -> Result<(), StageError> {
        Err(StageError::InvalidInput)
    }
    fn property(
        &mut self,
        name: GraphName<'_>,
        value: PropertyValue<'_>,
        control: &mut WriteControl<'_>,
    ) -> Result<(), StageError>;
    fn text(&mut self, _text: &str, _control: &mut WriteControl<'_>) -> Result<(), StageError> {
        Err(StageError::InvalidInput)
    }
    fn vector(
        &mut self,
        _dimensions: u32,
        _coordinates: &[f32],
        _control: &mut WriteControl<'_>,
    ) -> Result<(), StageError> {
        Err(StageError::InvalidInput)
    }
}

impl ImageSink for NodeImageBudget {
    fn label(&mut self, name: GraphName<'_>, _: &mut WriteControl<'_>) -> Result<(), StageError> {
        NodeImageBudget::label(self, name)
    }
    fn property(
        &mut self,
        name: GraphName<'_>,
        value: PropertyValue<'_>,
        _: &mut WriteControl<'_>,
    ) -> Result<(), StageError> {
        NodeImageBudget::property(self, name, value)
    }
    fn text(&mut self, text: &str, _: &mut WriteControl<'_>) -> Result<(), StageError> {
        if self.text_bytes.is_some() {
            return Err(StageError::InvalidInput);
        }
        self.text_bytes = Some(text.len());
        Ok(())
    }
    fn vector(
        &mut self,
        dimensions: u32,
        _: &[f32],
        _: &mut WriteControl<'_>,
    ) -> Result<(), StageError> {
        if self.vector_dims.is_some_and(|known| known != dimensions) {
            return Err(StageError::InvalidInput);
        }
        self.vector_dims = Some(dimensions);
        Ok(())
    }
}

impl ImageSink for crate::property_graph::staging::NodeImageBuilder<'_, '_> {
    fn label(
        &mut self,
        name: GraphName<'_>,
        control: &mut WriteControl<'_>,
    ) -> Result<(), StageError> {
        crate::property_graph::staging::NodeImageBuilder::label(self, name, control)
    }
    fn property(
        &mut self,
        name: GraphName<'_>,
        value: PropertyValue<'_>,
        control: &mut WriteControl<'_>,
    ) -> Result<(), StageError> {
        crate::property_graph::staging::NodeImageBuilder::property(self, name, value, control)
    }
    fn text(&mut self, text: &str, control: &mut WriteControl<'_>) -> Result<(), StageError> {
        crate::property_graph::staging::NodeImageBuilder::text(self, text, control)
    }
    fn vector(
        &mut self,
        _: u32,
        coordinates: &[f32],
        control: &mut WriteControl<'_>,
    ) -> Result<(), StageError> {
        crate::property_graph::staging::NodeImageBuilder::vector(self, coordinates, control)
    }
}

impl ImageSink for RelationshipImageBudget {
    fn property(
        &mut self,
        name: GraphName<'_>,
        value: PropertyValue<'_>,
        _: &mut WriteControl<'_>,
    ) -> Result<(), StageError> {
        RelationshipImageBudget::property(self, name, value)
    }
}

impl ImageSink for crate::property_graph::staging::RelationshipImageBuilder<'_, '_> {
    fn property(
        &mut self,
        name: GraphName<'_>,
        value: PropertyValue<'_>,
        control: &mut WriteControl<'_>,
    ) -> Result<(), StageError> {
        crate::property_graph::staging::RelationshipImageBuilder::property(
            self, name, value, control,
        )
    }
}

/// Emits one label unless `edit` removes it, and reports whether `edit` adds
/// a label that is already present.
fn emit_label(
    name: GraphName<'_>,
    edit: Edit<'_>,
    sink: &mut dyn ImageSink,
    control: &mut WriteControl<'_>,
) -> Result<bool, StageError> {
    match edit {
        Edit::Label(label, present) if label.as_str() == name.as_str() => {
            if present {
                sink.label(name, control)?;
            }
            Ok(true)
        }
        _ => {
            sink.label(name, control)?;
            Ok(false)
        }
    }
}

/// Emits the edit's own contribution after the prior image has been copied.
fn emit_edit(
    edit: Edit<'_>,
    matched_label: bool,
    sink: &mut dyn ImageSink,
    control: &mut WriteControl<'_>,
) -> Result<(), StageError> {
    match edit {
        Edit::Property(name, Some(value)) => sink.property(name, value, control),
        Edit::Label(label, true) if !matched_label => sink.label(label, control),
        Edit::Property(_, None) | Edit::Label(..) => Ok(()),
    }
}

/// A missing symbol name in an admitted catalog is corruption, not absence.
fn unnamed() -> TreeError {
    TreeError::Invalid("native record symbol has no admitted name")
}

/// Walks one node's complete image with `edit` applied: labels, properties,
/// stored text and vector, each copied byte for byte from its source.
#[allow(
    clippy::too_many_arguments,
    reason = "the walk keeps the view, overlay, sink and both controls explicit"
)]
fn walk_node<'s, 'v, 'm, 'g>(
    source: &NodeSource<'s, 'v, 'm, 'g>,
    id: NodeId,
    edit: Edit<'_>,
    view: &'s GraphReadView<'s, 'v, 'm, 'g>,
    overlay: &mut GraphBatchReadView<'s, 'static>,
    sink: &mut dyn ImageSink,
    control: &mut WriteControl<'_>,
    context: &mut RuntimeContext<'v, 'm, 'g>,
) -> Result<(), NativeExecutionError> {
    let mut matched_label = false;
    match source {
        NodeSource::Pending(image) => {
            let (labels, properties, text, embedding) =
                image.staging_node_parts().ok_or(StageError::InvalidInput)?;
            for label in labels {
                matched_label |= emit_label(*label, edit, sink, control)?;
            }
            for property in properties {
                control(WritePhase::Overlay)?;
                if !edit.replaces_property(property.name()) {
                    sink.property(property.name(), property.value(), control)?;
                }
            }
            if let Some(text) = text {
                sink.text(text, control)?;
            }
            if let Some(embedding) = embedding {
                let coordinates = embedding.vector().coordinates();
                let dimensions =
                    u32::try_from(coordinates.len()).map_err(|_| StageError::InvalidInput)?;
                sink.vector(dimensions, coordinates, control)?;
            }
        }
        NodeSource::Base(node) => {
            let record = node.record();
            let RecordShape::Node { labels, .. } = record.shape() else {
                return Err(TreeError::Invalid("node record role").into());
            };
            let mut resources = TreeResources::for_query(context)?;
            for index in 0..labels {
                let label = record.label(index, &mut resources)?;
                let name = view
                    .expression_symbol_name(Symbol::Label(label), &mut resources)?
                    .ok_or_else(unnamed)?;
                matched_label |= emit_label(name, edit, sink, control)?;
            }
            for index in 0..record.canonical().property_count() {
                let (key, _) = record.property_at(index, &mut resources)?;
                let name = view
                    .expression_symbol_name(Symbol::Property(key), &mut resources)?
                    .ok_or_else(unnamed)?;
                if edit.replaces_property(name) {
                    continue;
                }
                let value = overlay
                    .property(BatchEntityRef::Node(BatchNode::Existing(id)), name, control)?
                    .ok_or(StageError::MissingEntity)?;
                sink.property(name, value, control)?;
            }
            if let Some(text) = overlay.stored_text(BatchNode::Existing(id), control)? {
                sink.text(text, control)?;
            }
            if let Some(vector) = record.canonical().stored_vector() {
                let dimensions = vector.dimensions();
                let mut chunk = [0.0_f32; 256];
                let mut next = 0_u32;
                while next < dimensions {
                    let count = (dimensions - next).min(256);
                    let output = chunk.get_mut(..count as usize).ok_or(RuntimeError::Batch)?;
                    for (offset, coordinate) in output.iter_mut().enumerate() {
                        let offset = u32::try_from(offset).map_err(|_| RuntimeError::Batch)?;
                        *coordinate = vector.coordinate(next + offset, &mut resources)?;
                    }
                    sink.vector(dimensions, output, control)?;
                    next += count;
                }
            }
        }
    }
    emit_edit(edit, matched_label, sink, control)?;
    Ok(())
}

/// Walks one relationship's properties with `edit` applied and returns its
/// immutable topology, which the image must carry unchanged.
#[allow(
    clippy::too_many_arguments,
    reason = "the walk keeps the view, overlay, sink and both controls explicit"
)]
fn walk_relationship<'s, 'v, 'm, 'g>(
    source: &RelationshipSource<'s, 'v, 'm, 'g>,
    id: RelId,
    edit: Edit<'_>,
    view: &'s GraphReadView<'s, 'v, 'm, 'g>,
    overlay: &mut GraphBatchReadView<'s, 'static>,
    sink: &mut dyn ImageSink,
    control: &mut WriteControl<'_>,
    context: &mut RuntimeContext<'v, 'm, 'g>,
) -> Result<(BatchNode<'static>, BatchNode<'static>, GraphName<'s>), NativeExecutionError> {
    let topology = match source {
        RelationshipSource::Pending {
            source,
            target,
            relationship_type,
            properties,
        } => {
            for property in *properties {
                control(WritePhase::Overlay)?;
                if !edit.replaces_property(property.name()) {
                    sink.property(property.name(), property.value(), control)?;
                }
            }
            (*source, *target, *relationship_type)
        }
        RelationshipSource::Base(relationship) => {
            let row = relationship.row();
            let record = relationship.record();
            let mut resources = TreeResources::for_query(context)?;
            let relationship_type = view
                .expression_symbol_name(
                    Symbol::RelationshipType(row.relationship_type),
                    &mut resources,
                )?
                .ok_or_else(unnamed)?;
            for index in 0..record.canonical().property_count() {
                let (key, _) = record.property_at(index, &mut resources)?;
                let name = view
                    .expression_symbol_name(Symbol::Property(key), &mut resources)?
                    .ok_or_else(unnamed)?;
                if edit.replaces_property(name) {
                    continue;
                }
                let value = overlay
                    .property(
                        BatchEntityRef::Relationship(BatchRelationship::Existing(id)),
                        name,
                        control,
                    )?
                    .ok_or(StageError::MissingEntity)?;
                sink.property(name, value, control)?;
            }
            (
                BatchNode::Existing(row.source),
                BatchNode::Existing(row.target),
                relationship_type,
            )
        }
    };
    emit_edit(edit, false, sink, control)?;
    Ok(topology)
}
