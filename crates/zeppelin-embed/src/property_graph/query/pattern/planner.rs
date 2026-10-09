//! Conservative physical choices inside one already validated pattern region.

use super::super::plan::{MAX_PLAN_DEPTH, Operator, OperatorKind, PlanNodeId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum JoinStrategy {
    Nested,
    Hash,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BuildSide {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct JoinPlan {
    pub(super) strategy: JoinStrategy,
    pub(super) build: BuildSide,
}

pub(super) fn join_plan(
    shared_slots: usize,
    operators: &[Operator<'_>],
    left: PlanNodeId,
    right: PlanNodeId,
) -> JoinPlan {
    #[cfg(test)]
    if let Some(choice) = FORCED_JOIN.with(std::cell::Cell::get) {
        return JoinPlan {
            strategy: choice,
            build: BuildSide::Right,
        };
    }
    let strategy = if shared_slots == 0 {
        JoinStrategy::Nested
    } else {
        JoinStrategy::Hash
    };
    let build = match (
        safe_source_rank(operators, left, 0),
        safe_source_rank(operators, right, 0),
    ) {
        (Some(left), Some(right)) if left < right => BuildSide::Left,
        _ => BuildSide::Right,
    };
    JoinPlan { strategy, build }
}

fn safe_source_rank(operators: &[Operator<'_>], node: PlanNodeId, depth: usize) -> Option<u8> {
    if depth >= MAX_PLAN_DEPTH {
        return None;
    }
    let operator = operators.get(node.0 as usize)?;
    match operator.kind {
        OperatorKind::LookupNode { .. }
        | OperatorKind::LookupRelationship { .. }
        | OperatorKind::LookupKey { .. } => {
            safe_input_chain(operators, operator, depth).then_some(0)
        }
        OperatorKind::ScanNodes { label: Some(_), .. } => {
            safe_input_chain(operators, operator, depth).then_some(1)
        }
        OperatorKind::ScanNodes { label: None, .. } => {
            safe_input_chain(operators, operator, depth).then_some(2)
        }
        OperatorKind::Collect | OperatorKind::Expand { .. } => operator
            .inputs
            .first()
            .and_then(|input| safe_source_rank(operators, *input, depth + 1)),
        OperatorKind::BoundedExpand {
            edge_predicate: None,
            completed_edge_predicate: None,
            ..
        } => operator
            .inputs
            .first()
            .and_then(|input| safe_source_rank(operators, *input, depth + 1)),
        _ => None,
    }
}

fn safe_input_chain(operators: &[Operator<'_>], operator: &Operator<'_>, depth: usize) -> bool {
    let Some(input) = operator.inputs.first().copied() else {
        return false;
    };
    if depth + 1 >= MAX_PLAN_DEPTH {
        return false;
    }
    let Some(input) = operators.get(input.0 as usize) else {
        return false;
    };
    match input.kind {
        OperatorKind::Unit => input.inputs.is_empty(),
        OperatorKind::LookupNode { .. }
        | OperatorKind::LookupRelationship { .. }
        | OperatorKind::LookupKey { .. }
        | OperatorKind::ScanNodes { .. } => safe_input_chain(operators, input, depth + 1),
        OperatorKind::Collect | OperatorKind::Expand { .. } => {
            safe_input_chain(operators, input, depth + 1)
        }
        OperatorKind::BoundedExpand {
            edge_predicate: None,
            completed_edge_predicate: None,
            ..
        } => safe_input_chain(operators, input, depth + 1),
        _ => false,
    }
}

/// Only one uncorrelated Document scan and one scalar folder equality.
#[derive(Clone, Copy)]
pub(super) struct FolderPlan {
    pub(super) scan: PlanNodeId,
    pub(super) slot: super::SlotId,
    pub(super) value: u64,
    pub(super) label: super::ExprId,
    pub(super) predicate: super::ExprId,
    pub(super) count: Option<PlanNodeId>,
}

pub(super) fn folder_plan(
    description: super::super::plan::PlanDescription<'_>,
    root: PlanNodeId,
    bindings: &[super::ParameterBinding<'_>],
) -> Option<FolderPlan> {
    use super::super::plan::{AggregateExpression, BinaryExpression, Expression, Literal};
    let expressions = description.expressions;
    let operator = |id: PlanNodeId| description.operators.get(id.0 as usize);
    let expression = |id: super::ExprId| expressions.get(id.0 as usize);
    let mut node = root;
    let mut count = None;
    let mut counted_slot = None;
    for _ in 0..MAX_PLAN_DEPTH {
        let op = operator(node)?;
        match op.kind {
            OperatorKind::Project { .. }
            | OperatorKind::OffsetLimit { .. }
            | OperatorKind::Sort { .. } => node = *op.inputs.first()?,
            OperatorKind::Aggregate { keys, aggregates }
                if keys.is_empty() && aggregates.len() == 1 && count.is_none() =>
            {
                let Expression::Aggregate {
                    operation: AggregateExpression::Count { distinct: false },
                    operand: Some(operand),
                } = expression(aggregates.first()?.expression)?
                else {
                    return None;
                };
                let Expression::Slot(slot) = expression(*operand)? else {
                    return None;
                };
                counted_slot = Some(*slot);
                count = Some(node);
                node = *op.inputs.first()?;
                break;
            }
            _ => break,
        }
    }
    let eq = operator(node)?;
    let OperatorKind::Filter(predicate) = eq.kind else {
        return None;
    };
    let Expression::Binary {
        operation: BinaryExpression::Comparison(super::Comparison::Equal),
        left,
        right,
    } = expression(predicate)?
    else {
        return None;
    };
    let (property, scalar) = if matches!(expression(*left)?, Expression::Property { .. }) {
        (*left, *right)
    } else {
        (*right, *left)
    };
    let Expression::Property { entity, name } = expression(property)? else {
        return None;
    };
    if name.as_str() != "folder" {
        return None;
    }
    let Expression::Slot(slot) = expression(*entity)? else {
        return None;
    };
    let value = match expression(scalar)? {
        Expression::Literal(Literal::I64(value)) => u64::try_from(*value).ok()?,
        Expression::Parameter(id) => {
            let name = description.parameters.get(id.0 as usize)?.name;
            let super::QueryValue::I64(value) =
                bindings.iter().find(|binding| binding.name == name)?.value
            else {
                return None;
            };
            u64::try_from(value).ok()?
        }
        _ => return None,
    };
    let label_op = operator(*eq.inputs.first()?)?;
    let OperatorKind::Filter(label) = label_op.kind else {
        return None;
    };
    let Expression::HasLabel {
        entity,
        label: name,
    } = expression(label)?
    else {
        return None;
    };
    if name.as_str() != "Document"
        || !matches!(expression(*entity)?, Expression::Slot(found) if found == slot)
    {
        return None;
    }
    let scan = *label_op.inputs.first()?;
    let op = operator(scan)?;
    let OperatorKind::ScanNodes {
        output,
        label: None,
    } = op.kind
    else {
        return None;
    };
    if output != *slot || counted_slot.is_some_and(|counted| counted != *slot) {
        return None;
    }
    let unit = operator(*op.inputs.first()?)?;
    if !matches!(unit.kind, OperatorKind::Unit) || !unit.inputs.is_empty() {
        return None;
    }
    Some(FolderPlan {
        scan,
        slot: *slot,
        value,
        label,
        predicate,
        count,
    })
}

#[cfg(test)]
std::thread_local! {
    static FORCED_JOIN: std::cell::Cell<Option<JoinStrategy>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(super) fn force_join_strategy(choice: Option<JoinStrategy>) {
    FORCED_JOIN.with(|forced| forced.set(choice));
}

/// Fresh read anchors may omit isolated nodes. Label predicates stay residual.
pub(super) fn incident_source_scan(
    operators: &[Operator<'_>],
    expressions: &[super::super::plan::Expression<'_>],
    expand: PlanNodeId,
) -> Option<PlanNodeId> {
    #[cfg(any(test, feature = "test-seams"))]
    if super::test_support::original_node_sources() {
        return None;
    }
    if operators.iter().any(|operator| {
        matches!(
            operator.kind,
            OperatorKind::OptionalApply { .. }
                | OperatorKind::Mutate(_)
                | OperatorKind::Eager
                | OperatorKind::With(_)
        )
    }) {
        return None;
    }
    let expand = operators.get(expand.0 as usize)?;
    let OperatorKind::Expand { source, .. } = expand.kind else {
        return None;
    };
    let &[mut scan_id] = expand.inputs else {
        return None;
    };
    // Only infallible label predicates may be crossed: arbitrary expressions
    // could raise errors on isolated nodes and must retain full enumeration.
    for _ in 0..MAX_PLAN_DEPTH {
        let scan = operators.get(scan_id.0 as usize)?;
        match scan.kind {
            OperatorKind::Filter(predicate) if anchor_labels(expressions, predicate, source, 0) => {
                let [child] = scan.inputs else {
                    return None;
                };
                scan_id = *child;
            }
            OperatorKind::ScanNodes { output, .. } if output == source => {
                let [unit_id] = scan.inputs else {
                    return None;
                };
                let unit = operators.get(unit_id.0 as usize)?;
                return (matches!(unit.kind, OperatorKind::Unit) && unit.inputs.is_empty())
                    .then_some(scan_id);
            }
            _ => return None,
        }
    }
    None
}

fn anchor_labels(
    expressions: &[super::super::plan::Expression<'_>],
    predicate: super::super::plan::ExprId,
    anchor: super::super::plan::SlotId,
    depth: usize,
) -> bool {
    use super::super::plan::{BinaryExpression, Expression};
    if depth >= MAX_PLAN_DEPTH {
        return false;
    }
    match expressions.get(predicate.0 as usize) {
        Some(Expression::HasLabel { entity, .. }) => matches!(
            expressions.get(entity.0 as usize), Some(Expression::Slot(slot)) if *slot == anchor),
        Some(Expression::Binary {
            operation: BinaryExpression::And,
            left,
            right,
        }) => {
            anchor_labels(expressions, *left, anchor, depth + 1)
                && anchor_labels(expressions, *right, anchor, depth + 1)
        }
        _ => false,
    }
}

/// Only an exact global node count can bypass the row aggregate.
pub(super) fn global_node_count(
    description: super::super::plan::PlanDescription<'_>,
    operator: &Operator<'_>,
) -> Option<bool> {
    use super::super::plan::{AggregateExpression, Expression};
    let OperatorKind::Aggregate {
        keys: [],
        aggregates: [aggregate],
    } = operator.kind
    else {
        return None;
    };
    let [input] = operator.inputs else {
        return None;
    };
    let mut input = description.operators.get(input.0 as usize)?;
    let mut document = false;
    let mut labelled_slot = None;
    if let OperatorKind::Filter(predicate) = input.kind {
        let Expression::HasLabel { entity, label } =
            description.expressions.get(predicate.0 as usize)?
        else {
            return None;
        };
        if label.as_str() != "Document" {
            return None;
        }
        let Expression::Slot(slot) = description.expressions.get(entity.0 as usize)? else {
            return None;
        };
        labelled_slot = Some(*slot);
        document = true;
        let [child] = input.inputs else { return None };
        input = description.operators.get(child.0 as usize)?;
    }
    let OperatorKind::ScanNodes { output, label } = input.kind else {
        return None;
    };
    if labelled_slot.is_some_and(|slot| slot != output) {
        return None;
    }
    if let Some(label) = label {
        if label.as_str() != "Document" {
            return None;
        }
        document = true;
    }
    let [child] = input.inputs else { return None };
    let unit = description.operators.get(child.0 as usize)?;
    if !matches!(unit.kind, OperatorKind::Unit) || !unit.inputs.is_empty() {
        return None;
    }
    let Expression::Aggregate {
        operation: AggregateExpression::Count { distinct: false },
        operand,
    } = description
        .expressions
        .get(aggregate.expression.0 as usize)?
    else {
        return None;
    };
    if let Some(operand) = operand
        && !matches!(description.expressions.get(operand.0 as usize), Some(Expression::Slot(slot)) if *slot == output)
    {
        return None;
    }
    Some(document)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "fixed planner fixtures"
)]
mod incident_source_tests {
    use super::super::super::plan::{Direction, PatternId, SlotId};
    use super::*;

    #[test]
    fn incident_source_requires_fresh_read_region() {
        let unit_input = [PlanNodeId(0)];
        let scan_input = [PlanNodeId(1)];
        let expand_input = [PlanNodeId(2)];
        let mut operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &unit_input,
                kind: OperatorKind::ScanNodes {
                    output: SlotId(0),
                    label: None,
                },
            },
            Operator {
                inputs: &scan_input,
                kind: OperatorKind::Expand {
                    source: SlotId(0),
                    node: SlotId(1),
                    relationship: SlotId(2),
                    direction: Direction::Outgoing,
                    relationship_types: &[],
                    pattern: PatternId(0),
                },
            },
            Operator {
                inputs: &expand_input,
                kind: OperatorKind::Collect,
            },
        ];
        assert!(incident_source_scan(&operators, &[], PlanNodeId(2)).is_some());
        for excluded in [
            OperatorKind::OptionalApply { predicate: None },
            OperatorKind::Mutate(&[]),
            OperatorKind::Eager,
            OperatorKind::With(&[]),
        ] {
            operators[3].kind = excluded;
            assert!(incident_source_scan(&operators, &[], PlanNodeId(2)).is_none());
        }
        operators[3].kind = OperatorKind::Collect;
        for direction in [Direction::Incoming, Direction::Either] {
            if let OperatorKind::Expand {
                direction: current, ..
            } = &mut operators[2].kind
            {
                *current = direction;
            }
            assert!(incident_source_scan(&operators, &[], PlanNodeId(2)).is_some());
        }
        if let OperatorKind::Expand { direction, .. } = &mut operators[2].kind {
            *direction = Direction::Outgoing;
        }
        operators[1].kind = OperatorKind::ScanNodes {
            output: SlotId(0),
            label: Some(crate::property_graph::GraphName::new("Document").unwrap()),
        };
        assert!(incident_source_scan(&operators, &[], PlanNodeId(2)).is_some());
        operators[1].inputs = &scan_input; // A scan chain/correlated anchor is not Unit.
        assert!(incident_source_scan(&operators, &[], PlanNodeId(2)).is_none());
    }
}
