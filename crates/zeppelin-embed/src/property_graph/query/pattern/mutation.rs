//! Native occurrences for the mutation-clause barrier.
//!
//! `Eager` freezes the complete upstream bag and its computed cells before any
//! mutation clause runs: on its first pull it drains its child to exhaustion,
//! retaining every row and the relationship uses that produced it, and only
//! then begins emitting. A clause that writes therefore never observes a row
//! its own statement produced, and a reset re-drains the child so a repeated
//! occurrence under a nested-loop join freezes each input scope separately.
//!
//! `OperatorKind::Mutate` is deliberately absent. It remains a
//! `PlanError::Reference` in `NativePattern::occurrence_count` until the
//! mutation executor lands.

use super::super::relational::Rows;
use super::relational::copy_relationship_uses_slice;
use super::*;

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
}

impl<'v, 'm, 'g> EagerState<'v, 'm, 'g> {
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

impl<'s, 'r, 'plan, 'v, 'm, 'g> NativePattern<'s, 'r, 'plan, 'v, 'm, 'g> {
    pub(super) fn next_eager(
        &mut self,
        index: usize,
        child: usize,
        state: &mut QueryArena<'m, 'g, EagerState<'v, 'm, 'g>>,
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
                rows.push(values.as_slice(), context)?;
                drop(values);
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
}
