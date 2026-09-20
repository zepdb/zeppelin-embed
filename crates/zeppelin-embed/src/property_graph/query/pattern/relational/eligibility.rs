//! Native preparation for one validated singleton search domain.

#![allow(
    dead_code,
    reason = "the eager search owner consumes this private preparation seam in ZE-64"
)]

use super::super::super::eligibility::{Eligibility, EligibleNodeSet};
use super::super::super::expression::{ExpressionCapacity, ExpressionError, ExpressionFailure};
use super::super::super::plan::{ParameterBinding, SearchRequest};
use super::super::super::relational::{RowOperator, Schema, StorageCapacity};
use super::super::super::resources::RuntimePlan;
use super::super::super::runtime::{ArenaCapacity, PullOperator, PullState};
use super::super::*;

/// Charged singleton input row and its optional materialized same-view set.
pub(super) struct PreparedEligibility<'v, 'm, 'g> {
    row: RowBatch<'v, 'm, 'g>,
    schema: Schema<'m, 'g>,
    set: Option<EligibleNodeSet<'v, 'm, 'g>>,
}

impl<'v, 'm, 'g> PreparedEligibility<'v, 'm, 'g> {
    pub(super) fn eligibility(&self) -> Eligibility<'_, 'v, 'm, 'g> {
        match &self.set {
            Some(set) => Eligibility::Set(set),
            None => Eligibility::AllIndexed,
        }
    }

    pub(super) fn value(&self, slot: SlotId) -> Result<QueryValue<'_>, RuntimeError> {
        self.row
            .value(0, self.schema.column(slot)?)
            .ok_or(RuntimeError::Batch)
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the admitted plan, native and set capacities remain explicit"
)]
pub(super) fn prepare<'s, 'r, 'plan, 'v, 'm, 'g>(
    view: &'s GraphReadView<'s, 'v, 'm, 'g>,
    plan: &'r RuntimePlan<'r, 'plan, 'r, 'm, 'g, 'r>,
    search: PlanNodeId,
    bindings: &[ParameterBinding<'_>],
    native_capacity: PatternCapacity,
    set_capacity: usize,
    context: &mut RuntimeContext<'v, 'm, 'g>,
) -> Result<PreparedEligibility<'v, 'm, 'g>, NativeExecutionError> {
    let description = plan.plan().description();
    let operator = description
        .operators
        .get(search.0 as usize)
        .ok_or(PlanError::Reference)?;
    let input = if operator.inputs.len() == 1 {
        *operator.inputs.first().ok_or(PlanError::Reference)?
    } else {
        return Err(PlanError::Arity.into());
    };
    let eligible = match operator.kind {
        OperatorKind::Search { request, .. } => match request {
            SearchRequest::Vector { eligible, .. }
            | SearchRequest::Text { eligible, .. }
            | SearchRequest::Hybrid { eligible, .. } => eligible,
        },
        _ => return Err(PlanError::Search.into()),
    };
    if !plan
        .plan()
        .facts(input)
        .ok_or(PlanError::Reference)?
        .singleton()
    {
        return Err(PlanError::Search.into());
    }

    let mut source = NativePattern::new(view, plan, input, bindings, native_capacity, context)?;
    let mut slots = QueryArena::new(context.memory(), source.schema().slots().len())
        .map_err(RuntimeError::Memory)?;
    for slot in source.schema().slots() {
        context.checkpoint()?;
        slots.push(*slot).map_err(RuntimeError::Memory)?;
    }
    let schema = Schema::new(context, slots.as_slice())?;
    drop(slots);
    let singleton_capacity = StorageCapacity {
        rows: 1,
        payload_bytes: native_capacity.rows.payload_bytes,
        variable: native_capacity.rows.variable,
    };
    let mut row = RowBatch::with_arenas(
        context,
        schema.slots().len(),
        singleton_capacity.rows,
        singleton_capacity.payload_bytes,
        singleton_capacity.variable,
    )?;
    let _ = source.pull(context, &mut row)?;
    if row.rows() != 1 {
        return Err(RuntimeError::Batch.into());
    }
    let mut exhausted = RowBatch::with_arenas(
        context,
        schema.slots().len(),
        1,
        singleton_capacity.payload_bytes,
        ArenaCapacity {
            string_bytes: singleton_capacity.variable.string_bytes,
            list_cells: singleton_capacity.variable.list_cells,
            node_ids: singleton_capacity.variable.node_ids,
            relationship_ids: singleton_capacity.variable.relationship_ids,
        },
    )?;
    let exhausted_state = source.pull(context, &mut exhausted)?;
    if exhausted_state != PullState::Done || exhausted.rows() != 0 {
        return Err(RuntimeError::Batch.into());
    }
    drop(exhausted);
    drop(source);

    let set = if let Some(expression) = eligible {
        let mut evaluator = NativeExpressionEvaluator::new(
            plan,
            bindings,
            ExpressionCapacity {
                cells: native_capacity.expression.cells,
                string_bytes: native_capacity.expression.string_bytes,
            },
            context,
        )?;
        let evaluated = evaluator.evaluate(expression, &schema, &row, 0, view, context)?;
        let QueryValue::List(list) = evaluated else {
            return Err(expression_error(expression, QueryError::Type.into()));
        };
        QueryValue::List(list)
            .validate(context.values())
            .map_err(|error| expression_error(expression, error.into()))?;
        let mut values = CheckedListValues {
            list,
            next: 0,
            missing: false,
        };
        let set = EligibleNodeSet::build(context, set_capacity, values.by_ref())
            .map_err(|error| expression_error(expression, error))?;
        if values.missing || values.next != list.len() {
            return Err(RuntimeError::Batch.into());
        }
        Some(set)
    } else {
        None
    };
    Ok(PreparedEligibility { row, schema, set })
}

fn expression_error(expression: ExprId, error: RuntimeError) -> NativeExecutionError {
    match error {
        RuntimeError::Limit(_)
        | RuntimeError::Memory(_)
        | RuntimeError::IdentityExhausted
        | RuntimeError::Value(
            QueryError::Cancelled
            | QueryError::ReadCancelled
            | QueryError::Timeout
            | QueryError::Control
            | QueryError::WorkLimit,
        ) => error.into(),
        error => ExpressionError {
            expression,
            failure: ExpressionFailure::Runtime(error),
        }
        .into(),
    }
}

struct CheckedListValues<'a> {
    list: QueryList<'a>,
    next: usize,
    missing: bool,
}

impl<'a> Iterator for CheckedListValues<'a> {
    type Item = QueryValue<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next == self.list.len() {
            return None;
        }
        let value = self.list.get(self.next);
        if value.is_some() {
            self.next += 1;
        } else {
            self.missing = true;
        }
        value
    }
}
