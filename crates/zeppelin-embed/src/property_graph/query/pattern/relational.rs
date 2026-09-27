#![allow(
    clippy::result_large_err,
    reason = "native typed errors remain allocation-free"
)]

use super::super::plan::{AggregateExpression, Expression, Projection, SortKey};
use super::super::relational::streaming::{StreamAggregate, StreamIndex};
use super::super::relational::{Aggregate, AggregateColumn, OrderKey, Rows, SlotProjection};
use super::*;

pub(super) mod eligibility;
mod top_k;
use top_k::TopRows;
#[cfg(any(test, feature = "test-support"))]
pub(crate) mod test_support;

// One charged use list per raw row. Row descriptors grow in the same chunks
// as Rows; empty-use rows reserve no relationship entries.
struct RowUses<'m, 'g> {
    chunks: QueryArena<'m, 'g, QueryArena<'m, 'g, QueryArena<'m, 'g, RelationshipUse>>>,
    capacity: super::super::relational::StorageCapacity,
    len: usize,
}
impl<'m, 'g> RowUses<'m, 'g> {
    fn new(
        capacity: super::super::relational::StorageCapacity,
        context: &RuntimeContext<'_, 'm, 'g>,
    ) -> Result<Self, RuntimeError> {
        if capacity.rows == 0 && capacity.max_rows != 0 {
            return Err(RuntimeError::Batch);
        }
        Ok(Self {
            chunks: QueryArena::new(
                context.memory(),
                capacity.max_rows.div_ceil(capacity.rows.max(1)),
            )?,
            capacity,
            len: 0,
        })
    }
    fn push(
        &mut self,
        uses: &[RelationshipUse],
        context: &RuntimeContext<'_, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        if self.len == self.capacity.max_rows {
            return Err(RuntimeError::BatchCapacity);
        }
        let chunk = self.len / self.capacity.rows;
        if chunk == self.chunks.len() {
            self.chunks.push(QueryArena::new(
                context.memory(),
                self.capacity.rows.min(self.capacity.max_rows - self.len),
            )?)?;
        }
        let mut row = QueryArena::new(context.memory(), uses.len())?;
        row.extend_copy(uses)?;
        self.chunks
            .as_mut_slice()
            .get_mut(chunk)
            .ok_or(RuntimeError::Batch)?
            .push(row)?;
        self.len += 1;
        Ok(())
    }
    fn get(&self, raw: usize) -> Result<&[RelationshipUse], RuntimeError> {
        let size = self.capacity.rows.max(1);
        self.chunks
            .as_slice()
            .get(raw / size)
            .and_then(|chunk| chunk.as_slice().get(raw % size))
            .map(QueryArena::as_slice)
            .ok_or(RuntimeError::Batch)
    }
}

pub(super) struct SortState<'v, 'm, 'g> {
    top: Option<TopRows<'v, 'm, 'g>>,
    descriptors: QueryArena<'m, 'g, SortKey>,
    order: QueryArena<'m, 'g, OrderKey>,
    visible_slots: QueryArena<'m, 'g, SlotId>,
    key_slots: QueryArena<'m, 'g, SlotId>,
    evaluated: QueryArena<'m, 'g, RowBatch<'v, 'm, 'g>>,
    visible: Option<Rows<'v, 'm, 'g>>,
    key_rows: Option<Rows<'v, 'm, 'g>>,
    uses: RowUses<'m, 'g>,
    capacity: PatternCapacity,
    started: bool,
    next: usize,
}

pub(super) struct DistinctState<'v, 'm, 'g> {
    slots: QueryArena<'m, 'g, SlotId>,
    rows: Option<Rows<'v, 'm, 'g>>,
    uses: RowUses<'m, 'g>,
    capacity: PatternCapacity,
    started: bool,
    next: usize,
}

pub(super) struct AggregateState<'v, 'm, 'g> {
    expressions: QueryArena<'m, 'g, ExprId>,
    input_slots: QueryArena<'m, 'g, SlotId>,
    key_columns: QueryArena<'m, 'g, SlotProjection>,
    aggregate_columns: QueryArena<'m, 'g, AggregateColumn>,
    evaluated: QueryArena<'m, 'g, RowBatch<'v, 'm, 'g>>,
    rows: Option<Rows<'v, 'm, 'g>>,
    representatives: Option<QueryArena<'m, 'g, Option<usize>>>,
    uses: RowUses<'m, 'g>,
    capacity: PatternCapacity,
    started: bool,
    next: usize,
}

impl<'v, 'm, 'g> AggregateState<'v, 'm, 'g> {
    pub(super) fn inherited_key_expression(&self, slot: SlotId) -> Option<ExprId> {
        let position = self
            .key_columns
            .as_slice()
            .iter()
            .position(|key| key.output == slot)?;
        self.expressions.as_slice().get(position).copied()
    }

    pub(super) fn new(
        keys: &[Projection],
        aggregates: &[Projection],
        plan_expressions: &[Expression<'_>],
        capacity: PatternCapacity,
        context: &RuntimeContext<'v, 'm, 'g>,
    ) -> Result<QueryArena<'m, 'g, Self>, NativeExecutionError> {
        let mut owner = QueryArena::new(context.memory(), 1).map_err(RuntimeError::Memory)?;
        let expression_capacity = keys
            .len()
            .checked_add(aggregates.len())
            .ok_or(PlanError::Limit)?;
        let mut expressions =
            QueryArena::new(context.memory(), expression_capacity).map_err(RuntimeError::Memory)?;
        let mut input_slots =
            QueryArena::new(context.memory(), expression_capacity).map_err(RuntimeError::Memory)?;
        let mut key_columns =
            QueryArena::new(context.memory(), keys.len()).map_err(RuntimeError::Memory)?;
        let mut aggregate_columns =
            QueryArena::new(context.memory(), aggregates.len()).map_err(RuntimeError::Memory)?;
        let mut evaluated =
            QueryArena::new(context.memory(), expression_capacity).map_err(RuntimeError::Memory)?;
        let mut candidate = u32::MAX;
        for key in keys {
            context.checkpoint()?;
            let slot = SlotId(candidate);
            candidate = candidate.checked_sub(1).ok_or(PlanError::Limit)?;
            expressions
                .push(key.expression)
                .map_err(RuntimeError::Memory)?;
            input_slots.push(slot).map_err(RuntimeError::Memory)?;
            key_columns
                .push(SlotProjection {
                    source: slot,
                    output: key.slot,
                })
                .map_err(RuntimeError::Memory)?;
            evaluated
                .push(RowBatch::storage(
                    context,
                    1,
                    1,
                    capacity.rows.payload_bytes,
                    capacity.rows.variable,
                )?)
                .map_err(RuntimeError::Memory)?;
        }
        for projection in aggregates {
            context.checkpoint()?;
            let Expression::Aggregate { operation, operand } = plan_expressions
                .get(projection.expression.0 as usize)
                .ok_or(PlanError::Reference)?
            else {
                return Err(PlanError::Aggregate.into());
            };
            let operation = match (*operation, *operand) {
                (AggregateExpression::Count { distinct: false }, None) => Aggregate::CountAll,
                (AggregateExpression::Count { distinct }, Some(operand)) => {
                    let slot = SlotId(candidate);
                    candidate = candidate.checked_sub(1).ok_or(PlanError::Limit)?;
                    expressions.push(operand).map_err(RuntimeError::Memory)?;
                    input_slots.push(slot).map_err(RuntimeError::Memory)?;
                    evaluated
                        .push(RowBatch::storage(
                            context,
                            1,
                            1,
                            capacity.rows.payload_bytes,
                            capacity.rows.variable,
                        )?)
                        .map_err(RuntimeError::Memory)?;
                    Aggregate::Count { slot, distinct }
                }
                (AggregateExpression::Collect { distinct }, Some(operand)) => {
                    let slot = SlotId(candidate);
                    candidate = candidate.checked_sub(1).ok_or(PlanError::Limit)?;
                    expressions.push(operand).map_err(RuntimeError::Memory)?;
                    input_slots.push(slot).map_err(RuntimeError::Memory)?;
                    evaluated
                        .push(RowBatch::storage(
                            context,
                            1,
                            1,
                            capacity.rows.payload_bytes,
                            capacity.rows.variable,
                        )?)
                        .map_err(RuntimeError::Memory)?;
                    Aggregate::Collect { slot, distinct }
                }
                (
                    operation @ (AggregateExpression::Sum { .. }
                    | AggregateExpression::Min
                    | AggregateExpression::Max),
                    Some(operand),
                ) => {
                    let slot = SlotId(candidate);
                    candidate = candidate.checked_sub(1).ok_or(PlanError::Limit)?;
                    expressions.push(operand).map_err(RuntimeError::Memory)?;
                    input_slots.push(slot).map_err(RuntimeError::Memory)?;
                    evaluated
                        .push(RowBatch::storage(
                            context,
                            1,
                            1,
                            capacity.rows.payload_bytes,
                            capacity.rows.variable,
                        )?)
                        .map_err(RuntimeError::Memory)?;
                    match operation {
                        AggregateExpression::Sum { distinct } => Aggregate::Sum { slot, distinct },
                        AggregateExpression::Min => Aggregate::Min { slot },
                        AggregateExpression::Max => Aggregate::Max { slot },
                        _ => return Err(PlanError::Aggregate.into()),
                    }
                }
                _ => return Err(PlanError::Aggregate.into()),
            };
            aggregate_columns
                .push(AggregateColumn {
                    output: projection.slot,
                    operation,
                })
                .map_err(RuntimeError::Memory)?;
        }
        let rows = Rows::new(context, input_slots.as_slice(), capacity.rows)?;
        owner
            .push(Self {
                expressions,
                input_slots,
                key_columns,
                aggregate_columns,
                evaluated,
                rows: Some(rows),
                representatives: None,
                uses: RowUses::new(capacity.rows, context)?,
                capacity,
                started: false,
                next: 0,
            })
            .map_err(RuntimeError::Memory)?;
        Ok(owner)
    }

    pub(super) fn reset(
        &mut self,
        context: &RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        self.rows.take();
        self.representatives.take();
        self.rows = Some(Rows::new(
            context,
            self.input_slots.as_slice(),
            self.capacity.rows,
        )?);
        for value in self.evaluated.as_mut_slice() {
            context.checkpoint()?;
            value.clear();
        }
        self.uses = RowUses::new(self.capacity.rows, context)?;
        self.started = false;
        self.next = 0;
        Ok(())
    }
}

impl<'v, 'm, 'g> DistinctState<'v, 'm, 'g> {
    pub(super) fn new(
        slots: &[SlotId],
        capacity: PatternCapacity,
        context: &RuntimeContext<'v, 'm, 'g>,
    ) -> Result<QueryArena<'m, 'g, Self>, NativeExecutionError> {
        let mut owner = QueryArena::new(context.memory(), 1).map_err(RuntimeError::Memory)?;
        let mut owned_slots =
            QueryArena::new(context.memory(), slots.len()).map_err(RuntimeError::Memory)?;
        for slot in slots {
            owned_slots.push(*slot).map_err(RuntimeError::Memory)?;
        }
        let rows = Rows::new(context, slots, capacity.rows)?;
        owner
            .push(Self {
                slots: owned_slots,
                rows: Some(rows),
                uses: RowUses::new(capacity.rows, context)?,
                capacity,
                started: false,
                next: 0,
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
        self.uses = RowUses::new(self.capacity.rows, context)?;
        self.started = false;
        self.next = 0;
        Ok(())
    }
}

impl<'v, 'm, 'g> SortState<'v, 'm, 'g> {
    pub(super) fn bound(
        &mut self,
        limit: usize,
        context: &RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        self.visible.take();
        self.key_rows.take();
        self.top = Some(TopRows::new(limit, context)?);
        Ok(())
    }
    pub(super) fn new(
        visible_slots: &[SlotId],
        keys: &[SortKey],
        capacity: PatternCapacity,
        context: &RuntimeContext<'v, 'm, 'g>,
    ) -> Result<QueryArena<'m, 'g, Self>, NativeExecutionError> {
        let mut owner = QueryArena::new(context.memory(), 1).map_err(RuntimeError::Memory)?;
        let mut owned_visible_slots =
            QueryArena::new(context.memory(), visible_slots.len()).map_err(RuntimeError::Memory)?;
        for slot in visible_slots {
            owned_visible_slots
                .push(*slot)
                .map_err(RuntimeError::Memory)?;
        }
        let mut descriptors =
            QueryArena::new(context.memory(), keys.len()).map_err(RuntimeError::Memory)?;
        let mut key_slots =
            QueryArena::new(context.memory(), keys.len()).map_err(RuntimeError::Memory)?;
        let mut order =
            QueryArena::new(context.memory(), keys.len()).map_err(RuntimeError::Memory)?;
        let mut evaluated =
            QueryArena::new(context.memory(), keys.len()).map_err(RuntimeError::Memory)?;
        let mut candidate = u32::MAX;
        for key in keys {
            context.checkpoint()?;
            while visible_slots.contains(&SlotId(candidate))
                || key_slots.as_slice().contains(&SlotId(candidate))
            {
                candidate = candidate.checked_sub(1).ok_or(PlanError::Limit)?;
            }
            let slot = SlotId(candidate);
            candidate = candidate.checked_sub(1).ok_or(PlanError::Limit)?;
            descriptors.push(*key).map_err(RuntimeError::Memory)?;
            key_slots.push(slot).map_err(RuntimeError::Memory)?;
            order
                .push(OrderKey {
                    slot,
                    descending: key.descending,
                })
                .map_err(RuntimeError::Memory)?;
            evaluated
                .push(RowBatch::storage(
                    context,
                    1,
                    1,
                    capacity.rows.payload_bytes,
                    capacity.rows.variable,
                )?)
                .map_err(RuntimeError::Memory)?;
        }
        let visible = Rows::new(context, visible_slots, capacity.rows)?;
        let key_rows = Rows::new(context, key_slots.as_slice(), capacity.rows)?;
        owner
            .push(Self {
                top: None,
                descriptors,
                order,
                visible_slots: owned_visible_slots,
                key_slots,
                evaluated,
                visible: Some(visible),
                key_rows: Some(key_rows),
                uses: RowUses::new(capacity.rows, context)?,
                capacity,
                started: false,
                next: 0,
            })
            .map_err(RuntimeError::Memory)?;
        Ok(owner)
    }

    pub(super) fn reset(
        &mut self,
        context: &RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        self.visible.take();
        self.key_rows.take();
        if let Some(top) = &mut self.top {
            top.reset(context)?;
        } else {
            self.visible = Some(Rows::new(
                context,
                self.visible_slots.as_slice(),
                self.capacity.rows,
            )?);
            self.key_rows = Some(Rows::new(
                context,
                self.key_slots.as_slice(),
                self.capacity.rows,
            )?);
        }
        for value in self.evaluated.as_mut_slice() {
            context.checkpoint()?;
            value.clear();
        }
        self.uses = RowUses::new(self.capacity.rows, context)?;
        self.started = false;
        self.next = 0;
        Ok(())
    }
}

impl<'s, 'r, 'plan, 'v, 'm, 'g, 'i, 'q> NativePattern<'s, 'r, 'plan, 'v, 'm, 'g, 'i, 'q> {
    pub(super) fn next_offset_limit(
        &mut self,
        index: usize,
        child: usize,
        remaining_offset: &mut u64,
        remaining_limit: &mut Option<u64>,
        writes_below: bool,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<bool, NativeExecutionError> {
        if *remaining_limit == Some(0) {
            // Emit nothing more, but a write below must still run to
            // completion; a read-only child is never pulled.
            if writes_below {
                while self.next_occurrence(child, context)? {
                    context.charge(WorkKind::OperatorRows, 1)?;
                    context.charge(WorkKind::RowsIn, 1)?;
                }
            }
            return Ok(false);
        }
        loop {
            if !self.next_occurrence(child, context)? {
                return Ok(false);
            }
            context.charge(WorkKind::OperatorRows, 1)?;
            context.charge(WorkKind::RowsIn, 1)?;
            if *remaining_offset != 0 {
                *remaining_offset -= 1;
                continue;
            }
            self.copy_output(index, child, context)?;
            self.copy_uses(index, child)?;
            if let Some(remaining) = remaining_limit {
                *remaining -= 1;
            }
            return Ok(true);
        }
    }

    pub(super) fn next_sort(
        &mut self,
        index: usize,
        child: usize,
        state: &mut QueryArena<'m, 'g, SortState<'v, 'm, 'g>>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<bool, NativeExecutionError> {
        let state = state
            .as_mut_slice()
            .first_mut()
            .ok_or(RuntimeError::Batch)?;
        if !state.started {
            state.started = true;
            let mut visible = state.visible.take();
            let mut key_rows = state.key_rows.take();
            while self.next_occurrence(child, context)? {
                context.charge(WorkKind::OperatorRows, 1)?;
                context.charge(WorkKind::RowsIn, 1)?;
                for position in 0..state.descriptors.len() {
                    let descriptor = *state
                        .descriptors
                        .as_slice()
                        .get(position)
                        .ok_or(RuntimeError::Batch)?;
                    let value = state
                        .evaluated
                        .as_mut_slice()
                        .get_mut(position)
                        .ok_or(RuntimeError::Batch)?;
                    value.clear();
                    let evaluated = {
                        let occurrence = self
                            .occurrences
                            .as_slice()
                            .get(child)
                            .ok_or(RuntimeError::Batch)?;
                        evaluate_at(
                            &mut self.evaluator,
                            self.mutation.as_mut(),
                            descriptor.expression,
                            &occurrence.schema,
                            &occurrence.output,
                            0,
                            self.view,
                            context,
                        )?
                    };
                    value.push_row(&[evaluated], context)?;
                }
                let mut values = QueryArena::new(context.memory(), state.visible_slots.len())
                    .map_err(RuntimeError::Memory)?;
                {
                    let occurrence = self.occurrence(child)?;
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

                let mut key_values = QueryArena::new(context.memory(), state.evaluated.len())
                    .map_err(RuntimeError::Memory)?;
                for value in state.evaluated.as_slice() {
                    key_values
                        .push(value.value(0, 0).ok_or(RuntimeError::Batch)?)
                        .map_err(RuntimeError::Memory)?;
                }
                if let Some(top) = &mut state.top {
                    top.offer(
                        values.as_slice(),
                        key_values.as_slice(),
                        self.occurrence(child)?.uses.as_slice(),
                        state.order.as_slice(),
                        context,
                    )?;
                } else {
                    visible
                        .as_mut()
                        .ok_or(RuntimeError::Batch)?
                        .push(values.as_slice(), context)?;
                    key_rows
                        .as_mut()
                        .ok_or(RuntimeError::Batch)?
                        .push(key_values.as_slice(), context)?;
                    state
                        .uses
                        .push(self.occurrence(child)?.uses.as_slice(), context)?;
                }
            }
            if let Some(top) = &mut state.top {
                top.sort(state.order.as_slice(), context)?;
            } else {
                state.key_rows = Some(
                    key_rows
                        .ok_or(RuntimeError::Batch)?
                        .sort(state.order.as_slice(), context)?,
                );
            }
            state.visible = visible;
        }
        if let Some(top) = &state.top {
            if state.next == top.len() {
                return Ok(false);
            }
            let parent = self.occurrence_mut(index)?;
            top.emit(state.next, &mut parent.output, &mut parent.uses, context)?;
            state.next += 1;
            return Ok(true);
        }
        let key_rows = state.key_rows.as_ref().ok_or(RuntimeError::Batch)?;
        if state.next == key_rows.len() {
            return Ok(false);
        }
        let source = key_rows.selected_source_row(state.next)?;
        let visible = state.visible.as_ref().ok_or(RuntimeError::Batch)?;
        let parent = self.occurrence_mut(index)?;
        parent.output.push_from(
            |column| visible.value(source, column).ok_or(RuntimeError::Batch),
            context,
        )?;
        let uses = state.uses.get(source)?;
        copy_relationship_uses_slice(uses, &mut parent.uses)?;
        state.next += 1;
        Ok(true)
    }

    pub(super) fn next_distinct(
        &mut self,
        index: usize,
        child: usize,
        state: &mut QueryArena<'m, 'g, DistinctState<'v, 'm, 'g>>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<bool, NativeExecutionError> {
        let state = state
            .as_mut_slice()
            .first_mut()
            .ok_or(RuntimeError::Batch)?;
        if !state.started {
            state.started = true;
            let mut rows = state.rows.take().ok_or(RuntimeError::Batch)?;
            let mut seen = StreamIndex::new(context)?;
            let mut columns = QueryArena::new(context.memory(), rows.schema().slots().len())
                .map_err(RuntimeError::Memory)?;
            for column in 0..rows.schema().slots().len() {
                columns.push(column).map_err(RuntimeError::Memory)?;
            }
            while self.next_occurrence(child, context)? {
                context.charge(WorkKind::OperatorRows, 1)?;
                context.charge(WorkKind::RowsIn, 1)?;
                let mut values = QueryArena::new(context.memory(), rows.schema().slots().len())
                    .map_err(RuntimeError::Memory)?;
                {
                    let occurrence = self.occurrence(child)?;
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
                let (hash, found) =
                    seen.locate(values.as_slice(), &rows, columns.as_slice(), context)?;
                if found.is_some() {
                    continue;
                }
                seen.insert(hash, rows.len(), context)?;
                rows.push(values.as_slice(), context)?;
                drop(values);
                state
                    .uses
                    .push(self.occurrence(child)?.uses.as_slice(), context)?;
            }
            state.rows = Some(rows);
        }
        let rows = state.rows.as_ref().ok_or(RuntimeError::Batch)?;
        if state.next == rows.len() {
            return Ok(false);
        }
        let source = rows.selected_source_row(state.next)?;
        let parent = self.occurrence_mut(index)?;
        parent.output.push_from(
            |column| rows.value(state.next, column).ok_or(RuntimeError::Batch),
            context,
        )?;
        let uses = state.uses.get(source)?;
        copy_relationship_uses_slice(uses, &mut parent.uses)?;
        state.next += 1;
        Ok(true)
    }

    pub(super) fn next_aggregate(
        &mut self,
        index: usize,
        child: usize,
        state: &mut QueryArena<'m, 'g, AggregateState<'v, 'm, 'g>>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<bool, NativeExecutionError> {
        let state = state
            .as_mut_slice()
            .first_mut()
            .ok_or(RuntimeError::Batch)?;
        if !state.started {
            state.started = true;
            let mut aggregate = StreamAggregate::new(
                state.rows.take().ok_or(RuntimeError::Batch)?,
                state.key_columns.as_slice(),
                state.aggregate_columns.as_slice(),
                state.capacity.rows,
                context,
            )?;
            while self.next_occurrence(child, context)? {
                context.charge(WorkKind::OperatorRows, 1)?;
                context.charge(WorkKind::RowsIn, 1)?;
                for position in 0..state.expressions.len() {
                    let expression = *state
                        .expressions
                        .as_slice()
                        .get(position)
                        .ok_or(RuntimeError::Batch)?;
                    let value = state
                        .evaluated
                        .as_mut_slice()
                        .get_mut(position)
                        .ok_or(RuntimeError::Batch)?;
                    value.clear();
                    let evaluated = {
                        let occurrence = self
                            .occurrences
                            .as_slice()
                            .get(child)
                            .ok_or(RuntimeError::Batch)?;
                        evaluate_at(
                            &mut self.evaluator,
                            self.mutation.as_mut(),
                            expression,
                            &occurrence.schema,
                            &occurrence.output,
                            0,
                            self.view,
                            context,
                        )?
                    };
                    value.push_row(&[evaluated], context)?;
                }
                let mut values = QueryArena::new(context.memory(), state.evaluated.len())
                    .map_err(RuntimeError::Memory)?;
                for value in state.evaluated.as_slice() {
                    values
                        .push(value.value(0, 0).ok_or(RuntimeError::Batch)?)
                        .map_err(RuntimeError::Memory)?;
                }
                let fresh = aggregate.push(
                    values.as_slice(),
                    state.aggregate_columns.as_slice(),
                    context,
                )?;
                drop(values);
                if fresh {
                    state
                        .uses
                        .push(self.occurrence(child)?.uses.as_slice(), context)?;
                }
            }
            let rows = aggregate.finish(
                state.key_columns.as_slice(),
                state.aggregate_columns.as_slice(),
                context,
            )?;
            let mut representatives =
                QueryArena::new(context.memory(), rows.len()).map_err(RuntimeError::Memory)?;
            for row in 0..rows.len() {
                representatives
                    .push(if state.uses.len == 0 { None } else { Some(row) })
                    .map_err(RuntimeError::Memory)?;
            }
            state.rows = Some(rows);
            state.representatives = Some(representatives);
        }
        let rows = state.rows.as_ref().ok_or(RuntimeError::Batch)?;
        let representatives = state.representatives.as_ref().ok_or(RuntimeError::Batch)?;
        if state.next == rows.len() {
            return Ok(false);
        }
        let representative = *representatives
            .as_slice()
            .get(state.next)
            .ok_or(RuntimeError::Batch)?;
        let parent = self.occurrence_mut(index)?;
        parent.output.push_from(
            |column| rows.value(state.next, column).ok_or(RuntimeError::Batch),
            context,
        )?;
        if let Some(source) = representative {
            let uses = state.uses.get(source)?;
            copy_relationship_uses_slice(uses, &mut parent.uses)?;
        }
        state.next += 1;
        Ok(true)
    }
}

pub(super) fn copy_relationship_uses_slice(
    source: &[RelationshipUse],
    destination: &mut QueryArena<'_, '_, RelationshipUse>,
) -> Result<(), RuntimeError> {
    for usage in source {
        if !destination.as_slice().contains(usage) {
            destination.push(*usage).map_err(RuntimeError::Memory)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
