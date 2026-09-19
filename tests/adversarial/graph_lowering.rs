//! PG17 actual production lowerer, primitive plan oracle and scheduled faults.
//! No alternative query evaluator, graph execution or TCK acceptance is claimed.
use super::coverage::CoverageRegistry;
use super::fault_vfs::{
    FaultEvent, FaultMode, FaultSchedule, FaultSite, Layer, ScheduledQueryClock, ScheduledVfs,
};
use rand::RngCore;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use zeppelin_embed::lifecycle::{
    Deadline, ManualMonotonicClock, MonotonicClock, OpenOptions, QueryControl, Store,
};
use zeppelin_embed::property_graph::{
    GraphGeneration, StoreInstanceId,
    query::{QueryValue, QueryView, ValueContext, plan::*, resources::*},
    resources::GraphResources,
};
use zeppelin_embed::vfs::StdVfs;
use zeppelin_embed_adversarial_oracle::graph_lowering::{self as oracle, Observation};
use zeppelin_embed_cypher::{
    CompileLimits, ErrorKind, LoweredRead, ResourceError, compile_in, compile_read_in,
};
pub const REQUIRED_COVERAGE: &[&str] = &[
    "property-graph.lowering.scalar-parameters",
    "property-graph.lowering.optional-pattern",
    "property-graph.lowering.hidden-order",
    "property-graph.lowering.grouping",
    "property-graph.lowering.clock.copy",
    "property-graph.lowering.clock.midpoint",
    "property-graph.lowering.clock.final-handoff",
    "property-graph.lowering.budget.fire",
    "property-graph.lowering.same-seed-control",
    "property-graph.lowering.comparator.fire",
    "property-graph.lowering.release",
    "property-graph.lowering.completed-path",
];
#[derive(Default, Debug)]
pub struct ProbeReport {
    pub comparisons: usize,
    pub fault_fires: usize,
    pub clean_controls: usize,
    pub comparator_fires: usize,
}
struct CountClock {
    inner: ScheduledQueryClock<StdVfs>,
    calls: AtomicUsize,
}
impl MonotonicClock for CountClock {
    fn now(&self) -> Instant {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.now()
    }
}
fn expr(plan: PlanDescription<'_>, id: ExprId) -> String {
    match plan.expressions[id.0 as usize] {
        Expression::Literal(lit) => match lit {
            Literal::Null => "null".into(),
            Literal::Bool(v) => v.to_string(),
            Literal::I64(v) => v.to_string(),
            Literal::F64(v) => format!("f64:{:016x}", v.to_bits()),
            Literal::String(v) => format!("{v:?}"),
        },
        Expression::Slot(v) => format!("s{}", v.0),
        Expression::Parameter(v) => format!("${}", v.0),
        Expression::Property { entity, name } => {
            format!("prop({},{:?})", expr(plan, entity), name.as_str())
        }
        Expression::HasLabel { entity, label } => {
            format!("label({},{:?})", expr(plan, entity), label.as_str())
        }
        Expression::List(ids) => format!(
            "[{}]",
            ids.iter()
                .map(|id| expr(plan, *id))
                .collect::<Vec<_>>()
                .join(",")
        ),
        Expression::Binary {
            operation,
            left,
            right,
        } => format!("{operation:?}({},{})", expr(plan, left), expr(plan, right)),
        Expression::Unary { operation, operand } => {
            format!("{operation:?}({})", expr(plan, operand))
        }
        Expression::Aggregate { operation, operand } => format!(
            "{operation:?}({})",
            operand.map(|id| expr(plan, id)).unwrap_or("*".into())
        ),
    }
}
fn projections(plan: PlanDescription<'_>, items: &[Projection]) -> String {
    items
        .iter()
        .map(|p| format!("{}={}", p.slot.0, expr(plan, p.expression)))
        .collect::<Vec<_>>()
        .join(",")
}
fn observe(read: &LoweredRead<'_, '_>) -> Observation {
    let plan = read.plan().description();
    let operators = plan
        .operators
        .iter()
        .map(|op| {
            let body = match op.kind {
                OperatorKind::Unit => "unit".into(),
                OperatorKind::ScanNodes { output, label } => {
                    assert!(label.is_none());
                    format!("scan {}", output.0)
                }
                OperatorKind::Expand {
                    source,
                    node,
                    relationship,
                    direction,
                    relationship_types,
                    pattern,
                } => format!(
                    "expand {}->{} rel{} {direction:?} types{:?} pattern{}",
                    source.0,
                    node.0,
                    relationship.0,
                    relationship_types
                        .iter()
                        .map(|n| n.as_str())
                        .collect::<Vec<_>>(),
                    pattern.0
                ),
                OperatorKind::BoundedExpand {
                    source,
                    node,
                    relationships,
                    direction,
                    relationship_types,
                    pattern,
                    min,
                    max,
                    edge_predicate,
                    completed_edge_predicate,
                } => {
                    format!(
                        "path {}->{} rels{} {direction:?} types{:?} pattern{} {min}..{max} {}",
                        source.0,
                        node.0,
                        relationships.0,
                        relationship_types
                            .iter()
                            .map(|n| n.as_str())
                            .collect::<Vec<_>>(),
                        pattern.0,
                        edge_predicate
                            .map(|p| format!(
                                "edge{}={}",
                                p.current_edge.0,
                                expr(plan, p.expression)
                            ))
                            .unwrap_or("none".into())
                    ) + &completed_edge_predicate
                        .map(|p| {
                            format!(
                                " completed{}={}",
                                p.current_edge.0,
                                expr(plan, p.expression)
                            )
                        })
                        .unwrap_or_default()
                }
                OperatorKind::Filter(predicate) => format!("filter {}", expr(plan, predicate)),
                OperatorKind::OptionalApply { predicate } => format!(
                    "optional {}",
                    predicate.map(|id| expr(plan, id)).unwrap_or("none".into())
                ),
                OperatorKind::Project(items) => format!("project {}", projections(plan, items)),
                OperatorKind::With(items) => format!("with {}", projections(plan, items)),
                OperatorKind::Aggregate { keys, aggregates } => format!(
                    "aggregate keys{} values{}",
                    projections(plan, keys),
                    projections(plan, aggregates)
                ),
                OperatorKind::Distinct => "distinct".into(),
                OperatorKind::Sort(keys) => format!(
                    "sort {}",
                    keys.iter()
                        .map(|k| format!(
                            "{}:{}",
                            expr(plan, k.expression),
                            if k.descending { "desc" } else { "asc" }
                        ))
                        .collect::<Vec<_>>()
                        .join(",")
                ),
                OperatorKind::OffsetLimit { offset, limit } => format!(
                    "bound {offset} {}",
                    limit.map(|n| n.to_string()).unwrap_or("none".into())
                ),
                _ => panic!("PG17 recipe unexpectedly lowered a non-read operator"),
            };
            format!(
                "{:?} {body}",
                op.inputs.iter().map(|id| id.0).collect::<Vec<_>>()
            )
        })
        .collect();
    let facts = read.plan().facts(plan.root).unwrap();
    let columns = read
        .columns()
        .iter()
        .enumerate()
        .map(|(index, c)| {
            let (slot, kinds) = facts.slot_at(index).unwrap();
            assert_eq!(slot, c.slot);
            let mask = [
                ValueKinds::NULL,
                ValueKinds::BOOL,
                ValueKinds::I64,
                ValueKinds::F64,
                ValueKinds::STRING,
                ValueKinds::NODE,
                ValueKinds::REL,
                ValueKinds::LIST,
            ]
            .iter()
            .enumerate()
            .fold(0, |mask, (i, kind)| {
                mask | if kinds.contains(*kind) { 1 << i } else { 0 }
            });
            (c.name.into(), slot.0, mask)
        })
        .collect();
    let parameters = read
        .parameters()
        .iter()
        .map(|p| {
            let QueryValue::I64(v) = p.value else {
                panic!("PG17 integer parameter")
            };
            (p.name.into(), v)
        })
        .collect();
    Observation {
        operators,
        columns,
        parameters,
        root: plan.root.0,
        ordered: facts.ordered(),
    }
}
fn recipe(case: usize, alias: &str) -> String {
    match case {
        0=>format!("RETURN $value AS {alias}, [1,'λ'] AS list"),
        1=>"MATCH (a:A)-[r:R|S]->(b) OPTIONAL MATCH (b)-[p:P|Q*0..2 {weight:$v}]->(c) WHERE c.ok=true RETURN a,r,b,p,c".into(),
        2=>"MATCH (n) RETURN n.p AS x ORDER BY n.q DESC SKIP 1 LIMIT $limit".into(),
        3=>"MATCH (n) WITH count(*) AS c, n.p AS p ORDER BY p RETURN c,p".into(),
        4=>"MATCH (a)-[r:R*0..2 {x:size(r),y:7}]->(b) RETURN r".into(),
        _=>unreachable!(),
    }
}
struct Trial {
    observation: Option<Observation>,
    compiler_polls: usize,
    consumer_polls: usize,
    compiler_peak: usize,
    fires: usize,
}
fn trial(
    seed: u64,
    case: usize,
    alias: &str,
    value: i64,
    fire: Option<usize>,
    budget: Option<usize>,
    only_compile: bool,
) -> Result<Trial, String> {
    let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(32 * 1024 * 1024),
    )
    .map_err(|e| e.to_string())?;
    let shared = GraphResources::from_store(&store).map_err(|e| e.to_string())?;
    let baseline = shared.reserved_bytes().map_err(|e| e.to_string())?;
    let manual = Arc::new(ManualMonotonicClock::new());
    let schedule = fire
        .map(|nth_match| {
            FaultSchedule::single(FaultEvent {
                id: format!("PG17-{seed}-{case}-{nth_match}"),
                op_index: 0,
                layer: Layer::Clock,
                site: FaultSite::Clock,
                mode: FaultMode::ClockJump { seconds: 30 },
                nth_match: if nth_match == usize::MAX {
                    1
                } else {
                    nth_match
                },
                expected_matches: None,
                deadline_budget_seconds: Some(1),
                path_contains: Some("clock".into()),
                fired: false,
                fire_count: 0,
                path: None,
            })
        })
        .unwrap_or_default();
    let vfs = Arc::new(ScheduledVfs::new_with_clock(
        StdVfs,
        schedule,
        Arc::clone(&manual),
    ));
    vfs.set_operation(0);
    let clock = Arc::new(CountClock {
        inner: ScheduledQueryClock::new(manual, Arc::clone(&vfs)),
        calls: AtomicUsize::new(0),
    });
    let control = QueryControl::Deadline(
        Deadline::after_with_test_clock(Duration::from_secs(1), clock.clone())
            .map_err(|e| e.to_string())?,
    );
    let mut report = Trial {
        observation: None,
        compiler_polls: 0,
        consumer_polls: 0,
        compiler_peak: 0,
        fires: 0,
    };
    {
        let memory = QueryMemory::new(&shared, budget.unwrap_or(24 * 1024 * 1024))
            .map_err(|e| e.to_string())?;
        let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
        let mut context =
            ValueContext::new(&view, &control, 8_000_000).map_err(|e| e.to_string())?;
        let name = match case {
            0 => "value",
            1 => "v",
            _ => "limit",
        };
        let parameter = [ParameterBinding {
            name,
            value: QueryValue::I64(if case == 2 { 3 } else { value }),
        }];
        let parameters = if case >= 3 { &[][..] } else { &parameter[..] };
        let query = recipe(case, alias);
        let before = memory.reserved_bytes();
        let mut calls = 0;
        clock.calls.store(0, Ordering::SeqCst);
        if fire != Some(usize::MAX) {
            clock.inner.arm_query();
        }
        let result = if only_compile {
            compile_in(
                &query,
                parameters,
                CompileLimits::default(),
                &memory,
                &control,
                |_| {
                    calls += 1;
                    report.compiler_polls = clock.calls.load(Ordering::SeqCst);
                    Ok(())
                },
            )
        } else {
            compile_read_in(
                &query,
                parameters,
                CompileLimits::default(),
                &memory,
                &mut context,
                |read, _| {
                    calls += 1;
                    report.consumer_polls = clock.calls.load(Ordering::SeqCst);
                    report.observation = Some(observe(&read));
                    if fire == Some(usize::MAX) {
                        clock.inner.arm_query();
                    }
                    Ok(())
                },
            )
        };
        clock.inner.finish_query()?;
        report.fires = vfs.events().iter().map(|e| e.fire_count).sum();
        report.compiler_peak = memory.peak_reserved_bytes();
        if fire.is_some() || budget.is_some() {
            let expected = if fire.is_some() {
                ResourceError::Timeout
            } else {
                ResourceError::Memory
            };
            if !matches!(result,Err(error) if error.kind==ErrorKind::Resource(expected)) {
                return Err(format!("PG17 expected {expected:?}, got {result:?}"));
            }
            oracle::check_failure(
                result.is_ok(),
                if budget.is_some() { 1 } else { report.fires },
                calls,
                memory.reserved_bytes() - before,
                fire == Some(usize::MAX),
            )?;
        } else {
            result.map_err(|e| format!("PG17 compiler {e:?}"))?;
            if !only_compile {
                oracle::check(case, alias, value, report.observation.as_ref().unwrap())?;
            }
            if calls != 1 || memory.reserved_bytes() != before || report.fires != 0 {
                return Err("PG17 clean call/release mismatch".into());
            }
        }
    }
    if shared.reserved_bytes().map_err(|e| e.to_string())? != baseline {
        return Err("PG17 shared owner retained".into());
    }
    store.close().map_err(|e| e.to_string())?;
    Ok(report)
}
pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<ProbeReport, String> {
    let mut rng = super::test_support::seeded_rng("property_graph::lowering_probe", seed);
    let alias = format!("value_{}", rng.next_u64());
    let value = (rng.next_u64() >> 1) as i64;
    let mut report = ProbeReport::default();
    for (case, coverage_key) in [
        REQUIRED_COVERAGE[0],
        REQUIRED_COVERAGE[1],
        REQUIRED_COVERAGE[2],
        REQUIRED_COVERAGE[3],
        "property-graph.lowering.completed-path",
    ]
    .into_iter()
    .enumerate()
    {
        let observed = trial(seed, case, &alias, value, None, None, false)?
            .observation
            .unwrap();
        for mutation in 0..3 {
            let mut changed = observed.clone();
            match mutation {
                0 => {
                    changed.operators.pop();
                }
                1 => {
                    changed.columns[0].1 += 1;
                }
                _ => {
                    changed.columns[0].2 ^= 1;
                }
            }
            if oracle::check(case, &alias, value, &changed).is_ok() {
                return Err("PG17 planted observation change escaped".into());
            }
            oracle::check(case, &alias, value, &observed)?;
            report.comparator_fires += 1;
        }
        if case == 4 {
            let mut changed = observed.clone();
            changed.operators[2] = changed.operators[2]
                .split(" completed")
                .next()
                .unwrap()
                .to_string();
            if oracle::check(case, &alias, value, &changed).is_ok() {
                return Err("PG17 completed predicate omission escaped".into());
            }
            oracle::check(case, &alias, value, &observed)?;
            report.comparator_fires += 1;
        }
        report.comparisons += 1;
        coverage.hit(coverage_key);
    }
    let compiler = trial(seed, 1, &alias, value, None, None, true)?;
    let clean = trial(seed, 1, &alias, value, None, None, false)?;
    if clean.consumer_polls <= compiler.compiler_polls + 8 {
        return Err("PG17 lowering checkpoint interval absent".into());
    }
    for (index, fire) in [
        compiler.compiler_polls + 7,
        (compiler.compiler_polls + clean.consumer_polls) / 2,
        usize::MAX,
    ]
    .into_iter()
    .enumerate()
    {
        report.fault_fires += trial(seed, 1, &alias, value, Some(fire), None, false)?.fires;
        trial(seed, 1, &alias, value, None, None, false)?;
        report.clean_controls += 1;
        coverage.hit(REQUIRED_COVERAGE[index + 4]);
    }
    trial(
        seed,
        1,
        &alias,
        value,
        None,
        Some(compiler.compiler_peak + 1),
        false,
    )?;
    report.fault_fires += 1;
    trial(seed, 1, &alias, value, None, None, false)?;
    report.clean_controls += 1;
    for key in &REQUIRED_COVERAGE[7..] {
        coverage.hit(*key);
    }
    Ok(report)
}
