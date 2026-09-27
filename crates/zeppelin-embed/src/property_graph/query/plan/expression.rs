use super::*;
fn expression_inner(
    description: PlanDescription<'_>,
    id: ExprId,
    scope: &NodeFacts,
    seen: &mut [bool; MAX_PLAN_NODES],
    context: &mut ValueContext<'_>,
    active: &mut [ExprId; MAX_PLAN_DEPTH],
    depth: usize,
) -> Result<ValueKinds, PlanError> {
    context.step()?;
    if active.get(..depth).ok_or(PlanError::Limit)?.contains(&id) {
        return Err(PlanError::Cycle);
    }
    *active.get_mut(depth).ok_or(PlanError::Limit)? = id;
    let expression = description
        .expressions
        .get(id.0 as usize)
        .ok_or(PlanError::Reference)?;
    *seen.get_mut(id.0 as usize).ok_or(PlanError::Reference)? = true;
    Ok(match *expression {
        Expression::Aggregate { .. } => return Err(PlanError::Aggregate),
        Expression::Unary { operation, operand } => {
            let kinds = expression_inner(
                description,
                operand,
                scope,
                seen,
                context,
                active,
                depth + 1,
            )?;
            unary(operation, kinds)?
        }
        Expression::Binary {
            operation,
            left,
            right,
        } => {
            let left =
                expression_inner(description, left, scope, seen, context, active, depth + 1)?;
            let right =
                expression_inner(description, right, scope, seen, context, active, depth + 1)?;
            binary(operation, left, right)?
        }
        Expression::List(items) => {
            for item in items {
                expression_inner(description, *item, scope, seen, context, active, depth + 1)?;
            }
            ValueKinds::LIST
        }
        Expression::Property { entity, .. } => {
            require(
                expression_inner(description, entity, scope, seen, context, active, depth + 1)?,
                ValueKinds::NODE.union(ValueKinds::REL),
            )?;
            ValueKinds::NULL
                .union(ValueKinds::BOOL)
                .union(ValueKinds::I64)
                .union(ValueKinds::F64)
                .union(ValueKinds::STRING)
                .union(ValueKinds::LIST)
        }
        Expression::HasLabel { entity, .. } => {
            let receiver =
                expression_inner(description, entity, scope, seen, context, active, depth + 1)?;
            require(receiver, ValueKinds::NODE)?;
            nullable(ValueKinds::BOOL, receiver)
        }
        Expression::Literal(Literal::Null) => ValueKinds::NULL,
        Expression::Literal(Literal::Bool(_)) => ValueKinds::BOOL,
        Expression::Literal(Literal::I64(_)) => ValueKinds::I64,
        Expression::Literal(Literal::F64(_)) => ValueKinds::F64,
        Expression::Literal(Literal::String(_)) => ValueKinds::STRING,
        Expression::Slot(id) => scope.slot(id).ok_or(PlanError::Scope)?,
        Expression::Parameter(id) => {
            description
                .parameters
                .get(id.0 as usize)
                .ok_or(PlanError::Parameter)?
                .kinds
        }
    })
}

pub(super) fn expression(
    description: PlanDescription<'_>,
    id: ExprId,
    scope: &NodeFacts,
    seen: &mut [bool; MAX_PLAN_NODES],
    context: &mut ValueContext<'_>,
) -> Result<ValueKinds, PlanError> {
    expression_inner(
        description,
        id,
        scope,
        seen,
        context,
        &mut [ExprId(0); MAX_PLAN_DEPTH],
        0,
    )
}

fn require(actual: ValueKinds, allowed: ValueKinds) -> Result<(), PlanError> {
    if !actual.overlaps(allowed.union(ValueKinds::NULL)) {
        return Err(PlanError::Type);
    }
    Ok(())
}
fn nullable(result: ValueKinds, input: ValueKinds) -> ValueKinds {
    if input.overlaps(ValueKinds::NULL) {
        result.union(ValueKinds::NULL)
    } else {
        result
    }
}
fn unary(operation: UnaryExpression, input: ValueKinds) -> Result<ValueKinds, PlanError> {
    let (allowed, result) = match operation {
        UnaryExpression::Not => (ValueKinds::BOOL, ValueKinds::BOOL),
        UnaryExpression::Positive | UnaryExpression::Negate => (
            ValueKinds::I64.union(ValueKinds::F64),
            ValueKinds(input.0 & 12),
        ),
        UnaryExpression::IsNull | UnaryExpression::IsNotNull => return Ok(ValueKinds::BOOL),
        UnaryExpression::Size => (ValueKinds::LIST.union(ValueKinds::STRING), ValueKinds::I64),
        UnaryExpression::Labels => (ValueKinds::NODE, ValueKinds::LIST),
        UnaryExpression::RelType | UnaryExpression::RelIdText => {
            (ValueKinds::REL, ValueKinds::STRING)
        }
        UnaryExpression::NodeIdText => (ValueKinds::NODE, ValueKinds::STRING),
        UnaryExpression::StoredText => {
            (ValueKinds::NODE, ValueKinds::STRING.union(ValueKinds::NULL))
        }
    };
    require(input, allowed)?;
    Ok(nullable(result, input))
}
fn binary(
    operation: BinaryExpression,
    left: ValueKinds,
    right: ValueKinds,
) -> Result<ValueKinds, PlanError> {
    let result = match operation {
        BinaryExpression::And | BinaryExpression::Or | BinaryExpression::Xor => {
            require(left, ValueKinds::BOOL)?;
            require(right, ValueKinds::BOOL)?;
            ValueKinds::BOOL
        }
        BinaryExpression::Comparison(_) => ValueKinds::BOOL.union(ValueKinds::NULL),
        BinaryExpression::Arithmetic(_) => {
            let number = ValueKinds::I64.union(ValueKinds::F64);
            require(left, number)?;
            require(right, number)?;
            let mut result = ValueKinds::default();
            if left.overlaps(ValueKinds::I64) && right.overlaps(ValueKinds::I64) {
                result = result.union(ValueKinds::I64);
            }
            if left.overlaps(ValueKinds::F64) || right.overlaps(ValueKinds::F64) {
                result = result.union(ValueKinds::F64);
            }
            result
        }
        BinaryExpression::String(_) => {
            require(left, ValueKinds::STRING)?;
            require(right, ValueKinds::STRING)?;
            ValueKinds::BOOL
        }
        BinaryExpression::In => {
            require(right, ValueKinds::LIST)?;
            ValueKinds::BOOL.union(ValueKinds::NULL)
        }
        BinaryExpression::Index => {
            require(left, ValueKinds::LIST)?;
            require(right, ValueKinds::I64)?;
            ValueKinds::ANY
        }
    };
    Ok(nullable(result, left.union(right)))
}

pub(super) fn aggregate(
    description: PlanDescription<'_>,
    id: ExprId,
    scope: &NodeFacts,
    seen: &mut [bool; MAX_PLAN_NODES],
    context: &mut ValueContext<'_>,
) -> Result<ValueKinds, PlanError> {
    context.step()?;
    let Expression::Aggregate { operation, operand } = description
        .expressions
        .get(id.0 as usize)
        .ok_or(PlanError::Reference)?
    else {
        return Err(PlanError::Aggregate);
    };
    *seen.get_mut(id.0 as usize).ok_or(PlanError::Reference)? = true;
    if let Some(operand) = operand {
        expression(description, *operand, scope, seen, context)?;
    }
    Ok(match operation {
        AggregateExpression::Count { distinct: false } => ValueKinds::I64,
        AggregateExpression::Count { distinct: true } if operand.is_some() => ValueKinds::I64,
        AggregateExpression::Collect { .. } if operand.is_some() => ValueKinds::LIST,
        AggregateExpression::Sum { .. } if operand.is_some() => {
            ValueKinds::I64.union(ValueKinds::F64)
        }
        AggregateExpression::Min | AggregateExpression::Max if operand.is_some() => expression(
            description,
            operand.ok_or(PlanError::Aggregate)?,
            scope,
            seen,
            context,
        )?
        .union(ValueKinds::NULL),
        _ => return Err(PlanError::Aggregate),
    })
}
