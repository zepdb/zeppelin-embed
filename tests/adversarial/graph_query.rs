//! PG6 component probe: values, lexical scopes and in-flight validation/value
//! deadlines. No GraphStore execution, retrieval, durability or admission claim.
use super::coverage::CoverageRegistry;
use super::fault_vfs::{
    FaultEvent, FaultMode, FaultSchedule, FaultSite, Layer, ScheduledQueryClock, ScheduledVfs,
};
use rand::RngCore;
use std::sync::Arc;
use std::time::Duration;
use zeppelin_embed::lifecycle::{CancelToken, Deadline, ManualMonotonicClock, QueryControl};
use zeppelin_embed::property_graph::query::plan::*;
use zeppelin_embed::property_graph::query::{
    Comparison, QueryError, QueryList, QueryValue, QueryView, Truth, ValueContext,
};
use zeppelin_embed::property_graph::{GraphGeneration, NodeId, RelId, StoreInstanceId};
use zeppelin_embed::vfs::StdVfs;
use zeppelin_embed_adversarial_oracle::graph_query::{self as oracle, Observation, Value};
pub const REQUIRED_COVERAGE: &[&str] = &[
    "property-graph.query.numeric",
    "property-graph.query.grouping",
    "property-graph.query.list",
    "property-graph.query.scope",
    "property-graph.query.clock.bytes",
    "property-graph.query.clock.list",
    "property-graph.query.clock.plan",
    "property-graph.query.same-seed-control",
    "property-graph.query.pattern.scope",
    "property-graph.query.pattern.metadata",
    "property-graph.query.clock.pattern",
];
#[derive(Debug, Default)]
pub struct ProbeReport {
    pub comparisons: u64,
    pub scope_cases: u64,
    pub fault_fires: usize,
    pub clean_controls: usize,
}
fn truth(value: Truth) -> Option<bool> {
    match value {
        Truth::True => Some(true),
        Truth::False => Some(false),
        Truth::Unknown => None,
    }
}
fn observation(
    left: QueryValue<'_>,
    right: QueryValue<'_>,
    context: &mut ValueContext<'_>,
) -> Result<Observation, String> {
    Ok(Observation {
        equal: truth(
            left.predicate(right, Comparison::Equal, context)
                .map_err(|e| e.to_string())?,
        ),
        less: truth(
            left.predicate(right, Comparison::Less, context)
                .map_err(|e| e.to_string())?,
        ),
        order: left.order(right, context).map_err(|e| e.to_string())?,
        equivalent: left.equivalent(right, context).map_err(|e| e.to_string())?,
        same_hash: left.group_hash(context).map_err(|e| e.to_string())?
            == right.group_hash(context).map_err(|e| e.to_string())?,
    })
}
fn scalar<'a>(value: &'a Value<'a>, view: &'a QueryView) -> QueryValue<'a> {
    match value {
        Value::Null => QueryValue::Null,
        Value::Bool(v) => QueryValue::Bool(*v),
        Value::Integer(v) => QueryValue::I64(*v),
        Value::Float(v) => QueryValue::F64(f64::from_bits(*v)),
        Value::String(v) => QueryValue::String(v),
        Value::Node(v) => view.node(NodeId::new(*v).unwrap()),
        Value::Relationship(v) => view.relationship(RelId::new(*v).unwrap()),
        Value::List(_) => panic!("nested fixture is constructed explicitly"),
    }
}
fn scope_case(input: u32, output: u32, context: &mut ValueContext<'_>) -> Result<(), PlanError> {
    let expressions = [
        Expression::Literal(Literal::Bool(true)),
        Expression::Slot(SlotId(input)),
    ];
    let first = [Projection {
        slot: SlotId(input),
        expression: ExprId(0),
    }];
    let second = [Projection {
        slot: SlotId(output),
        expression: ExprId(1),
    }];
    let edges = [[PlanNodeId(0)], [PlanNodeId(1)], [PlanNodeId(2)]];
    let operators = [
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &edges[0],
            kind: OperatorKind::Project(&first),
        },
        Operator {
            inputs: &edges[1],
            kind: OperatorKind::With(&second),
        },
        Operator {
            inputs: &edges[2],
            kind: OperatorKind::Filter(ExprId(1)),
        },
    ];
    let mut facts = vec![NodeFacts::default(); 4];
    let mut regions = vec![
        RetainedRegion::slice(&operators)?,
        RetainedRegion::slice(&expressions)?,
        RetainedRegion::slice(&first)?,
        RetainedRegion::slice(&second)?,
        RetainedRegion::slice(&edges)?,
        RetainedRegion::vector(&facts)?,
    ];
    regions.sort_unstable();
    GraphPlan::validate(
        PlanDescription {
            operators: &operators,
            expressions: &expressions,
            parameters: &[],
            root: PlanNodeId(3),
            eager_searches: &[],
        },
        &mut facts,
        PlanFootprint::declared(1024 * 1024),
        PlanBacking::vector(&regions)?,
        context,
    )
    .map(|_| ())
}
fn pattern_case(
    base: u32,
    edge: u32,
    reference: u32,
    escapes: bool,
    context: &mut ValueContext<'_>,
) -> Result<(), PlanError> {
    use zeppelin_embed::property_graph::GraphName;
    let alternatives = ["FIRST", "SECOND", "FIRST"].map(|name| GraphName::new(name).unwrap());
    let expressions = [
        Expression::Slot(SlotId(reference)),
        Expression::Property {
            entity: ExprId(0),
            name: GraphName::new("weight").unwrap(),
        },
        Expression::Literal(Literal::I64(7)),
        Expression::Binary {
            operation: BinaryExpression::Comparison(Comparison::Equal),
            left: ExprId(1),
            right: ExprId(2),
        },
    ];
    let edges = [[PlanNodeId(0)], [PlanNodeId(1)], [PlanNodeId(2)]];
    let mut operators = vec![
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &edges[0],
            kind: OperatorKind::LookupNode {
                output: SlotId(base),
                id: NodeId::new((1u128 << 100) + 7).unwrap(),
            },
        },
        Operator {
            inputs: &edges[1],
            kind: OperatorKind::BoundedExpand {
                source: SlotId(base),
                node: SlotId(base + 1),
                relationships: SlotId(base + 2),
                min: 0,
                max: 2,
                direction: Direction::Outgoing,
                relationship_types: &alternatives,
                edge_predicate: Some(EdgePredicate {
                    current_edge: SlotId(edge),
                    expression: ExprId(3),
                }),
                pattern: PatternId(9),
            },
        },
    ];
    if escapes {
        operators.push(Operator {
            inputs: &edges[2],
            kind: OperatorKind::Filter(ExprId(3)),
        });
    }
    let mut facts = vec![NodeFacts::default(); operators.len()];
    let mut regions = vec![
        RetainedRegion::vector(&operators)?,
        RetainedRegion::slice(&expressions)?,
        RetainedRegion::slice(&edges)?,
        RetainedRegion::slice(&alternatives)?,
        RetainedRegion::slice(b"FIRST")?,
        RetainedRegion::slice(b"SECOND")?,
        RetainedRegion::slice(b"weight")?,
        RetainedRegion::vector(&facts)?,
    ];
    regions.sort_unstable();
    let plan = GraphPlan::validate(
        PlanDescription {
            operators: &operators,
            expressions: &expressions,
            parameters: &[],
            root: PlanNodeId(operators.len() as u32 - 1),
            eager_searches: &[],
        },
        &mut facts,
        PlanFootprint::declared(1024 * 1024),
        PlanBacking::vector(&regions)?,
        context,
    )?;
    let output = plan.facts(PlanNodeId(2)).unwrap();
    assert_eq!(output.width(), 3);
    assert_eq!(output.slot_at(0), Some((SlotId(base), ValueKinds::NODE)));
    assert_eq!(
        output.slot_at(1),
        Some((SlotId(base + 1), ValueKinds::NODE))
    );
    assert_eq!(
        output.slot_at(2),
        Some((SlotId(base + 2), ValueKinds::LIST))
    );
    assert_eq!(output.slot_at(3), None);
    assert_eq!(
        output.slot(SlotId(edge)),
        None,
        "private edge escaped to output"
    );
    match plan.description().operators[2].kind {
        OperatorKind::BoundedExpand {
            relationship_types,
            min,
            max,
            edge_predicate,
            pattern,
            ..
        } => {
            assert_eq!(
                relationship_types
                    .iter()
                    .map(|name| name.as_str())
                    .collect::<Vec<_>>(),
                ["FIRST", "SECOND", "FIRST"]
            );
            assert_eq!((min, max, pattern), (0, 2, PatternId(9)));
            assert_eq!(
                edge_predicate,
                Some(EdgePredicate {
                    current_edge: SlotId(edge),
                    expression: ExprId(3)
                })
            );
        }
        _ => panic!("validated pattern changed its operator kind"),
    }
    Ok(())
}
fn fault_trial(seed: u64, operation: usize, fault: bool) -> Result<(usize, u64), String> {
    // Target the last three charged units of the new private expression path,
    // after all input/type-name backing and ordinary input validation finished.
    let pattern_work = if operation == 3 {
        let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(seed));
        let control = QueryControl::Cancel(CancelToken::new());
        let mut context =
            ValueContext::new(&view, &control, 8_000_000).map_err(|e| e.to_string())?;
        pattern_case(0, 3, 3, false, &mut context).map_err(|e| e.to_string())?;
        context.work()
    } else {
        0
    };
    let manual = Arc::new(ManualMonotonicClock::new());
    let event = FaultEvent {
        id: format!("PG6-{seed}-{operation}"),
        op_index: 0,
        layer: Layer::Clock,
        site: FaultSite::Clock,
        mode: FaultMode::ClockJump { seconds: 30 },
        nth_match: if operation == 3 {
            pattern_work as usize - 2
        } else {
            3
        },
        expected_matches: None,
        deadline_budget_seconds: Some(1),
        path_contains: Some("clock".into()),
        fired: false,
        fire_count: 0,
        path: None,
    };
    let schedule = if fault {
        FaultSchedule::single(event)
    } else {
        FaultSchedule::default()
    };
    let vfs = Arc::new(ScheduledVfs::new_with_clock(
        StdVfs,
        schedule,
        Arc::clone(&manual),
    ));
    vfs.set_operation(0);
    let clock = Arc::new(ScheduledQueryClock::new(manual, Arc::clone(&vfs)));
    let control = QueryControl::Deadline(
        Deadline::after_with_test_clock(Duration::from_secs(1), clock.clone())
            .map_err(|e| e.to_string())?,
    );
    let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(seed));
    let mut context = ValueContext::new(&view, &control, 8_000_000).map_err(|e| e.to_string())?;
    let text = char::from(b'a' + (seed % 26) as u8)
        .to_string()
        .repeat(65_537);
    let list = [QueryValue::I64(1); 400];
    clock.arm_query();
    let timed_out = match operation {
        0 => match QueryValue::String(&text).predicate(
            QueryValue::String(&text),
            Comparison::Equal,
            &mut context,
        ) {
            Ok(Truth::True) => false,
            Err(QueryError::Timeout) => true,
            other => return Err(format!("PG6 byte outcome {other:?}")),
        },
        1 => match QueryList::new(&list, &mut context) {
            Ok(value) if value.len() == 400 => false,
            Err(QueryError::Timeout) => true,
            other => return Err(format!("PG6 list outcome {other:?}")),
        },
        2 => match scope_case(70_000, 70_000, &mut context) {
            Ok(()) => false,
            Err(PlanError::Control(QueryError::Timeout)) => true,
            other => return Err(format!("PG6 plan outcome {other:?}")),
        },
        _ => match pattern_case(0, 3, 3, false, &mut context) {
            Ok(()) => false,
            Err(PlanError::Control(QueryError::Timeout)) => true,
            other => return Err(format!("PG6 private pattern outcome {other:?}")),
        },
    };
    clock.finish_query()?;
    let fires = vfs.events().iter().map(|event| event.fire_count).sum();
    if timed_out != fault || fires != usize::from(fault) || context.work() == 0 {
        return Err(format!(
            "PG6 fault/control seed={seed} operation={operation} timeout={timed_out} fires={fires} work={}",
            context.work()
        ));
    }
    if operation == 3 && context.work() != pattern_work - if fault { 3 } else { 0 } {
        return Err(format!(
            "PG6 private predicate work location changed: {} of {pattern_work}",
            context.work()
        ));
    }
    Ok((fires, context.work()))
}
pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<ProbeReport, String> {
    let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(seed));
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context = ValueContext::new(&view, &control, 8_000_000).map_err(|e| e.to_string())?;
    let mut report = ProbeReport::default();
    let mut rng = super::test_support::seeded_rng("property_graph::query_probe", seed);
    let fixed = [
        Value::Null,
        Value::Bool(false),
        Value::Bool(true),
        Value::Integer(i64::MIN),
        Value::Integer(i64::MAX),
        Value::Integer(9_007_199_254_740_993),
        Value::Integer(0),
        Value::Integer(1),
        Value::Float(1.0f64.to_bits()),
        Value::Float((-0.0f64).to_bits()),
        Value::Float(f64::NAN.to_bits()),
        Value::Float(0xfff8000000000001),
        Value::Float(f64::INFINITY.to_bits()),
        Value::String("é\0"),
        Value::String("e\u{301}"),
        Value::Node(1),
        Value::Node((1u128 << 100) + 1),
        Value::Relationship(1),
    ];
    for left in &fixed {
        for right in &fixed {
            oracle::check(
                left,
                right,
                observation(scalar(left, &view), scalar(right, &view), &mut context)?,
            )?;
            report.comparisons += 1;
        }
    }
    for _ in 0..64 {
        let left = Value::Integer(rng.next_u64() as i64);
        let right = Value::Float(rng.next_u64());
        oracle::check(
            &left,
            &right,
            observation(scalar(&left, &view), scalar(&right, &view), &mut context)?,
        )?;
        report.comparisons += 1;
    }
    coverage.hit(REQUIRED_COVERAGE[0]);
    coverage.hit(REQUIRED_COVERAGE[1]);
    let nested = [QueryValue::Null, QueryValue::I64(1)];
    let nested = QueryList::new(&nested, &mut context).map_err(|e| e.to_string())?;
    let left = [QueryValue::List(nested), QueryValue::Null];
    let right = [QueryValue::List(nested), QueryValue::I64(7)];
    let left = QueryValue::List(QueryList::new(&left, &mut context).map_err(|e| e.to_string())?);
    let right = QueryValue::List(QueryList::new(&right, &mut context).map_err(|e| e.to_string())?);
    let primitive = Value::List(vec![
        Value::List(vec![Value::Null, Value::Integer(1)]),
        Value::Null,
    ]);
    let other = Value::List(vec![
        Value::List(vec![Value::Null, Value::Integer(1)]),
        Value::Integer(7),
    ]);
    oracle::check(&primitive, &other, observation(left, right, &mut context)?)?;
    oracle::check(
        &primitive,
        &primitive,
        observation(left, left, &mut context)?,
    )?;
    report.comparisons += 2;
    coverage.hit(REQUIRED_COVERAGE[2]);
    let slot = rng.next_u32();
    for destination in [slot, slot ^ 1] {
        let observed = scope_case(slot, destination, &mut context);
        if !matches!(observed, Ok(()) | Err(PlanError::Scope)) {
            return Err(format!("PG6 unexpected scope outcome {observed:?}"));
        }
        oracle::check_scope(
            &[vec![(slot, None)], vec![(destination, Some(slot))]],
            slot,
            observed.is_ok(),
        )?;
        report.scope_cases += 1;
    }
    coverage.hit(REQUIRED_COVERAGE[3]);
    let base = slot & !7;
    for (edge, reference, escapes) in [
        (base + 3, base + 3, false),
        (base + 3, base, false),
        (base, base, false),
        (base + 1, base + 1, false),
        (base + 2, base + 2, false),
        (base + 3, base + 1, false),
        (base + 3, base + 2, false),
        (base + 3, base + 4, false),
        (base + 3, base + 3, true),
    ] {
        let observed = pattern_case(base, edge, reference, escapes, &mut context);
        if !matches!(observed, Ok(()) | Err(PlanError::Scope)) {
            return Err(format!("PG6 unexpected private scope outcome {observed:?}"));
        }
        oracle::check_edge_scope(
            &[base],
            [base + 1, base + 2],
            edge,
            reference,
            escapes,
            observed.is_ok(),
        )?;
        report.scope_cases += 1;
    }
    coverage.hit(REQUIRED_COVERAGE[8]);
    coverage.hit(REQUIRED_COVERAGE[9]);
    for operation in 0..4 {
        let (fires, failed_work) = fault_trial(seed, operation, true)?;
        let (_, clean_work) = fault_trial(seed, operation, false)?;
        if clean_work <= failed_work {
            return Err("PG6 clean control did not complete more work".into());
        }
        report.fault_fires += fires;
        report.clean_controls += 1;
        coverage.hit(REQUIRED_COVERAGE[if operation == 3 { 10 } else { 4 + operation }]);
        coverage.hit(REQUIRED_COVERAGE[7]);
    }
    Ok(report)
}
#[test]
fn query_oracle_rejects_corrupted_primitive_observations() {
    use std::cmp::Ordering;
    let clean = Observation {
        equal: Some(true),
        less: Some(false),
        order: Ordering::Equal,
        equivalent: true,
        same_hash: true,
    };
    for bad in [
        Observation {
            equal: Some(false),
            ..clean
        },
        Observation {
            less: None,
            ..clean
        },
        Observation {
            order: Ordering::Less,
            ..clean
        },
        Observation {
            equivalent: false,
            ..clean
        },
        Observation {
            same_hash: false,
            ..clean
        },
    ] {
        assert!(
            oracle::check(&Value::Integer(1), &Value::Float(1.0f64.to_bits()), bad)
                .unwrap_err()
                .contains("PG6")
        );
    }
    assert!(
        oracle::check_scope(&[vec![(1, None)], vec![(2, Some(1))]], 1, true)
            .unwrap_err()
            .contains("PG6")
    );
    assert!(
        oracle::check_scope(&[vec![(1, None)], vec![(1, Some(1)), (1, None)]], 1, true).is_err()
    );
    for (edge, reference, escapes) in [(1, 1, false), (2, 2, false), (4, 2, false), (4, 4, true)] {
        assert!(oracle::check_edge_scope(&[1], [2, 3], edge, reference, escapes, true).is_err());
    }
    assert!(oracle::check_edge_scope(&[1], [2, 3], 4, 4, false, false).is_err());
}
