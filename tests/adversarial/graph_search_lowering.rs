//! PG20 production search lowering, primitive plan oracle and scheduled faults.
//! This proves compiler output and controls, not retrieval execution or ranking.
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
    query::{QueryView, ValueContext, plan::*, resources::*},
    resources::GraphResources,
};
use zeppelin_embed::vfs::StdVfs;
use zeppelin_embed_adversarial_oracle::graph_search_lowering::{
    self as oracle, Eligibility, Observation, SearchObservation,
};
use zeppelin_embed_cypher::{
    CompileLimits, ErrorKind, LoweredRead, ResourceError, compile_in, compile_read_in,
};

pub const REQUIRED_COVERAGE: &[&str] = &[
    "property-graph.search-lowering.modes",
    "property-graph.search-lowering.nullable-components",
    "property-graph.search-lowering.eligibility",
    "property-graph.search-lowering.eager-order",
    "property-graph.search-lowering.cartesian",
    "property-graph.search-lowering.invariant-backing",
    "property-graph.search-lowering.clock.fire",
    "property-graph.search-lowering.budget.fire",
    "property-graph.search-lowering.same-seed-control",
    "property-graph.search-lowering.comparator.fire",
    "property-graph.search-lowering.release",
];

#[derive(Default, Debug)]
pub struct ProbeReport {
    pub observations: usize,
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

fn mode(value: SearchMode) -> u8 {
    match value {
        SearchMode::Default => 0,
        SearchMode::Auto => 1,
        SearchMode::Exact => 2,
        SearchMode::Scan => 3,
        SearchMode::Graph => 4,
    }
}

fn mask(value: ValueKinds) -> u16 {
    [
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
    .fold(0, |result, (index, kind)| {
        result | if value.contains(*kind) { 1 << index } else { 0 }
    })
}

fn output_masks(facts: &NodeFacts, outputs: SearchOutputs) -> Result<[Option<u16>; 5], String> {
    let output = |slot: Option<SlotId>| {
        slot.map(|slot| {
            facts
                .slot(slot)
                .map(mask)
                .ok_or_else(|| format!("PG20 missing output slot {}", slot.0))
        })
        .transpose()
    };
    Ok([
        output(outputs.node)?,
        output(outputs.distance)?,
        output(outputs.score)?,
        output(outputs.vector_distance)?,
        output(outputs.lexical_score)?,
    ])
}

fn request_slots(description: PlanDescription<'_>, roots: &[ExprId]) -> Result<usize, String> {
    let mut stack = roots.to_vec();
    let mut seen = vec![false; description.expressions.len()];
    let mut slots = 0;
    while let Some(id) = stack.pop() {
        let index = usize::try_from(id.0).map_err(|_| "PG20 expression index overflow")?;
        let visited = seen
            .get_mut(index)
            .ok_or_else(|| format!("PG20 expression {} absent", id.0))?;
        if *visited {
            continue;
        }
        *visited = true;
        match description.expressions[index] {
            Expression::Aggregate { .. }
            | Expression::Property { .. }
            | Expression::HasLabel { .. } => {
                return Err("PG20 independent request reached row/entity expression".into());
            }
            Expression::List(children) => stack.extend_from_slice(children),
            Expression::Binary { left, right, .. } => {
                stack.push(left);
                stack.push(right);
            }
            Expression::Unary { operand, .. } => stack.push(operand),
            Expression::Slot(_) => slots += 1,
            Expression::Literal(_) | Expression::Parameter(_) => {}
        }
    }
    Ok(slots)
}

fn observe(read: &LoweredRead<'_, '_>) -> Result<Observation, String> {
    let description = read.plan().description();
    let mut searches = Vec::new();
    for (index, operator) in description.operators.iter().enumerate() {
        let OperatorKind::Search {
            request, outputs, ..
        } = operator.kind
        else {
            continue;
        };
        let input = operator
            .inputs
            .first()
            .ok_or_else(|| "PG20 search input absent".to_owned())?;
        let unit_input = matches!(
            description
                .operators
                .get(input.0 as usize)
                .map(|operator| operator.kind),
            Some(OperatorKind::Unit)
        );
        let (search_mode, eligible, roots) = match request {
            SearchRequest::Vector {
                vector,
                k,
                mode: search_mode,
                eligible,
                ..
            } => (mode(search_mode), eligible, vec![vector, k]),
            SearchRequest::Text {
                query, k, eligible, ..
            } => (255, eligible, vec![query, k]),
            SearchRequest::Hybrid {
                vector,
                text,
                k,
                mode: search_mode,
                eligible,
                ..
            } => (mode(search_mode), eligible, vec![vector, text, k]),
        };
        let eligibility = match eligible {
            None => Eligibility::AllIndexed,
            Some(id)
                if matches!(
                    description.expressions.get(id.0 as usize),
                    Some(Expression::List(items)) if items.is_empty()
                ) =>
            {
                Eligibility::LiteralEmpty
            }
            Some(id)
                if matches!(
                    description.expressions.get(id.0 as usize),
                    Some(Expression::Slot(_))
                ) =>
            {
                Eligibility::GlobalDistinctNodes
            }
            Some(id) => return Err(format!("PG20 unknown eligibility expression {}", id.0)),
        };
        let facts = read
            .plan()
            .facts(PlanNodeId(index as u32))
            .ok_or_else(|| format!("PG20 missing facts for operator {index}"))?;
        searches.push(SearchObservation {
            mode: search_mode,
            eligibility,
            output_masks: output_masks(facts, outputs)?,
            unit_input,
            request_slots: request_slots(description, &roots)?,
        });
    }
    let eager = description
        .eager_searches
        .iter()
        .map(
            |id| match description.operators.get(id.0 as usize).map(|op| op.kind) {
                Some(OperatorKind::Search { call, .. }) => Ok(call.0),
                _ => Err(format!("PG20 eager operator {} is not search", id.0)),
            },
        )
        .collect::<Result<Vec<_>, _>>()?;
    let cartesian_joins = description
        .operators
        .iter()
        .filter(|operator| matches!(operator.kind, OperatorKind::Join { predicate: None }))
        .count();
    let limit_zero = description.operators.iter().any(|operator| {
        matches!(
            operator.kind,
            OperatorKind::OffsetLimit {
                offset: 0,
                limit: Some(0)
            }
        )
    });
    Ok(Observation {
        searches,
        eager,
        cartesian_joins,
        limit_zero,
    })
}

fn pair_query(mode: u8, alias: &str) -> String {
    let spelling = ["default", "auto", "exact", "scan"][mode as usize];
    format!(
        "MATCH (prior) WITH prior,1+1 AS {alias} CALL ze.vector_search([{alias},2],{alias},'{spelling}',[]) YIELD node,distance CALL ze.hybrid_search([{alias},2],'x',{alias},'auto') YIELD node AS other,score,vector_distance,lexical_score RETURN prior,node,distance,other,score,vector_distance,lexical_score LIMIT 0"
    )
}

fn global_query() -> &'static str {
    "MATCH (n) WITH collect(DISTINCT n) AS eligible CALL ze.text_search('x',1,eligible) YIELD node,score RETURN node,score LIMIT 0"
}

struct Trial {
    observation: Option<Observation>,
    compiler_polls: usize,
    consumer_polls: usize,
    peak: usize,
    fires: usize,
    calls: usize,
}

fn trial(
    seed: u64,
    query: &str,
    fire: Option<usize>,
    budget: Option<usize>,
    only_compile: bool,
) -> Result<Trial, String> {
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(32 * 1024 * 1024),
    )
    .map_err(|error| error.to_string())?;
    let shared = GraphResources::from_store(&store).map_err(|error| error.to_string())?;
    let shared_baseline = shared.reserved_bytes().map_err(|error| error.to_string())?;
    let manual = Arc::new(ManualMonotonicClock::new());
    let schedule = fire
        .map(|nth_match| {
            FaultSchedule::single(FaultEvent {
                id: format!("PG20-{seed}-{nth_match}"),
                op_index: 0,
                layer: Layer::Clock,
                site: FaultSite::Clock,
                mode: FaultMode::ClockJump { seconds: 30 },
                nth_match,
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
            .map_err(|error| error.to_string())?,
    );
    let mut report = Trial {
        observation: None,
        compiler_polls: 0,
        consumer_polls: 0,
        peak: 0,
        fires: 0,
        calls: 0,
    };
    {
        let memory = QueryMemory::new(&shared, budget.unwrap_or(24 * 1024 * 1024))
            .map_err(|error| error.to_string())?;
        let baseline = memory.reserved_bytes();
        let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
        let mut context =
            ValueContext::new(&view, &control, 8_000_000).map_err(|error| error.to_string())?;
        clock.calls.store(0, Ordering::SeqCst);
        clock.inner.arm_query();
        let mut observation_error = None;
        let result = if only_compile {
            compile_in(
                query,
                &[],
                CompileLimits::default(),
                &memory,
                &control,
                |_| {
                    report.calls += 1;
                    report.compiler_polls = clock.calls.load(Ordering::SeqCst);
                    Ok(())
                },
            )
        } else {
            compile_read_in(
                query,
                &[],
                CompileLimits::default(),
                &memory,
                &mut context,
                |read, _| {
                    report.calls += 1;
                    report.consumer_polls = clock.calls.load(Ordering::SeqCst);
                    match observe(&read) {
                        Ok(observation) => report.observation = Some(observation),
                        Err(error) => observation_error = Some(error),
                    }
                    Ok(())
                },
            )
        };
        clock.inner.finish_query()?;
        if let Some(error) = observation_error {
            return Err(error);
        }
        report.fires = vfs.events().iter().map(|event| event.fire_count).sum();
        report.peak = memory.peak_reserved_bytes();
        if let Some(expected) = fire
            .map(|_| ResourceError::Timeout)
            .or_else(|| budget.map(|_| ResourceError::Memory))
        {
            if !matches!(result, Err(error) if error.kind == ErrorKind::Resource(expected)) {
                return Err(format!("PG20 expected {expected:?}, got {result:?}"));
            }
            if report.calls != 0 || memory.reserved_bytes() != baseline {
                return Err(format!(
                    "PG20 failure escaped calls={} retained={}",
                    report.calls,
                    memory.reserved_bytes() - baseline
                ));
            }
        } else {
            result.map_err(|error| format!("PG20 compiler {error:?}"))?;
            if report.calls != 1 || memory.reserved_bytes() != baseline || report.fires != 0 {
                return Err("PG20 clean call/release mismatch".into());
            }
        }
    }
    if shared.reserved_bytes().map_err(|error| error.to_string())? != shared_baseline {
        return Err("PG20 shared owner retained".into());
    }
    store.close().map_err(|error| error.to_string())?;
    Ok(report)
}

pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<ProbeReport, String> {
    let mut rng = super::test_support::seeded_rng("property_graph::search_lowering_probe", seed);
    let alias = format!("k_{}", rng.next_u64());
    let selected_mode = (rng.next_u64() % 4) as u8;
    let mut report = ProbeReport::default();
    let mut selected = None;
    for request_mode in 0..4 {
        let query = pair_query(request_mode, &alias);
        let observed = trial(seed, &query, None, None, false)?
            .observation
            .ok_or_else(|| "PG20 pair observation absent".to_owned())?;
        oracle::check_pair(request_mode, &observed)?;
        if request_mode == selected_mode {
            selected = Some((query, observed));
        }
        report.observations += 1;
    }
    coverage.hit(REQUIRED_COVERAGE[0]);
    let global = trial(seed, global_query(), None, None, false)?
        .observation
        .ok_or_else(|| "PG20 global observation absent".to_owned())?;
    oracle::check_global(&global)?;
    report.observations += 1;
    for key in &REQUIRED_COVERAGE[1..6] {
        coverage.hit(*key);
    }

    let (query, observed) = selected.ok_or_else(|| "PG20 selected mode absent".to_owned())?;
    for mutation in 0..6 {
        let mut changed = observed.clone();
        match mutation {
            0 => changed.searches[0].mode = (selected_mode + 1) % 4,
            1 => changed.searches[1].output_masks[3] = Some(8),
            2 => changed.searches[0].eligibility = Eligibility::AllIndexed,
            3 => {
                changed.eager.pop();
            }
            4 => changed.cartesian_joins -= 1,
            _ => changed.searches[0].request_slots = 1,
        }
        if oracle::check_pair(selected_mode, &changed).is_ok() {
            return Err(format!("PG20 planted mutation {mutation} escaped"));
        }
        oracle::check_pair(selected_mode, &observed)?;
        report.comparator_fires += 1;
    }
    coverage.hit(REQUIRED_COVERAGE[9]);

    let compiler = trial(seed, &query, None, None, true)?;
    let clean = trial(seed, &query, None, None, false)?;
    if clean.consumer_polls <= compiler.compiler_polls + 8 {
        return Err("PG20 search lowering checkpoint interval absent".into());
    }
    let fire = (compiler.compiler_polls + clean.consumer_polls) / 2;
    let fault = trial(seed, &query, Some(fire), None, false)?;
    if fault.fires != 1 {
        return Err(format!("PG20 clock fires={}", fault.fires));
    }
    report.fault_fires += fault.fires;
    coverage.hit(REQUIRED_COVERAGE[6]);
    let clock_control = trial(seed, &query, None, None, false)?
        .observation
        .ok_or_else(|| "PG20 clock control absent".to_owned())?;
    oracle::check_pair(selected_mode, &clock_control)?;
    report.clean_controls += 1;

    trial(seed, &query, None, Some(compiler.peak + 1), false)?;
    report.fault_fires += 1;
    coverage.hit(REQUIRED_COVERAGE[7]);
    let budget_control = trial(seed, &query, None, None, false)?
        .observation
        .ok_or_else(|| "PG20 budget control absent".to_owned())?;
    oracle::check_pair(selected_mode, &budget_control)?;
    report.clean_controls += 1;
    coverage.hit(REQUIRED_COVERAGE[8]);
    coverage.hit(REQUIRED_COVERAGE[10]);
    Ok(report)
}
