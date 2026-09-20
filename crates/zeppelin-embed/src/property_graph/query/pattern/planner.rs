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
    let Some(operator) = operators.get(node.0 as usize) else {
        return None;
    };
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

#[cfg(test)]
std::thread_local! {
    static FORCED_JOIN: std::cell::Cell<Option<JoinStrategy>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(super) fn force_join_strategy(choice: Option<JoinStrategy>) {
    FORCED_JOIN.with(|forced| forced.set(choice));
}
