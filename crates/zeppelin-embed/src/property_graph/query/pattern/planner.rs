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

#[cfg(test)]
std::thread_local! {
    static FORCED_JOIN: std::cell::Cell<Option<JoinStrategy>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(super) fn force_join_strategy(choice: Option<JoinStrategy>) {
    FORCED_JOIN.with(|forced| forced.set(choice));
}

/// Only a fresh, unconstrained outgoing anchor can omit isolated nodes.
pub(super) fn incident_source_scan(operators: &[Operator<'_>], expand: PlanNodeId) -> bool {
    #[cfg(any(test, feature = "test-seams"))]
    if super::test_support::original_node_sources() {
        return false;
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
        return false;
    }
    let Some(expand) = operators.get(expand.0 as usize) else {
        return false;
    };
    let OperatorKind::Expand {
        source,
        direction: super::super::plan::Direction::Outgoing,
        ..
    } = expand.kind
    else {
        return false;
    };
    let Some(scan) = expand
        .inputs
        .first()
        .and_then(|id| operators.get(id.0 as usize))
    else {
        return false;
    };
    let OperatorKind::ScanNodes {
        output,
        label: None,
    } = scan.kind
    else {
        return false;
    };
    let Some(unit) = scan
        .inputs
        .first()
        .and_then(|id| operators.get(id.0 as usize))
    else {
        return false;
    };
    source == output
        && expand.inputs.len() == 1
        && scan.inputs.len() == 1
        && matches!(unit.kind, OperatorKind::Unit)
        && unit.inputs.is_empty()
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
    fn incident_source_requires_fresh_outgoing_read_region() {
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
        assert!(incident_source_scan(&operators, PlanNodeId(2)));
        for excluded in [
            OperatorKind::OptionalApply { predicate: None },
            OperatorKind::Mutate(&[]),
            OperatorKind::Eager,
            OperatorKind::With(&[]),
        ] {
            operators[3].kind = excluded;
            assert!(!incident_source_scan(&operators, PlanNodeId(2)));
        }
        operators[3].kind = OperatorKind::Collect;
        for direction in [Direction::Incoming, Direction::Either] {
            if let OperatorKind::Expand {
                direction: current, ..
            } = &mut operators[2].kind
            {
                *current = direction;
            }
            assert!(!incident_source_scan(&operators, PlanNodeId(2)));
        }
        if let OperatorKind::Expand { direction, .. } = &mut operators[2].kind {
            *direction = Direction::Outgoing;
        }
        operators[1].inputs = &scan_input; // A scan chain/correlated anchor is not Unit.
        assert!(!incident_source_scan(&operators, PlanNodeId(2)));
    }
}
