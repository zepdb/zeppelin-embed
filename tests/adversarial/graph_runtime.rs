//! PG9 bounded runtime probe. Synthetic typed operator/completion adapters test
//! resource/pull/freeze contracts; no GraphStore/public query admission claim.
use super::coverage::CoverageRegistry;
use super::fault_vfs::{
    FaultEvent, FaultMode, FaultSchedule, FaultSite, Layer, ScheduledQueryClock, ScheduledVfs,
};
use rand::RngCore;
use std::sync::Arc;
use std::time::Duration;
use zeppelin_embed::lifecycle::{
    CancelToken, Deadline, ManualMonotonicClock, OpenOptions, QueryControl, SnapshotLease, Store,
};
use zeppelin_embed::property_graph::query::plan::*;
use zeppelin_embed::property_graph::query::resources::*;
use zeppelin_embed::property_graph::query::runtime::*;
use zeppelin_embed::property_graph::query::{QueryError, QueryValue, QueryView, ValueContext};
use zeppelin_embed::property_graph::resources::GraphResources;
use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};
use zeppelin_embed::vfs::StdVfs;
use zeppelin_embed_adversarial_oracle::graph_runtime::{self as oracle, Observation};
pub const REQUIRED_COVERAGE: &[&str] = &[
    "property-graph.runtime.bags",
    "property-graph.runtime.counters",
    "property-graph.runtime.clock.pull",
    "property-graph.runtime.clock.final",
    "property-graph.runtime.same-seed-control",
    "property-graph.runtime.release",
];
#[derive(Default, Debug)]
pub struct ProbeReport {
    pub cases: usize,
    pub fault_fires: usize,
    pub clean_controls: usize,
}
struct View {
    token: QueryView,
    lease: SnapshotLease,
}
impl RetainedView for View {
    fn query_view(&self) -> &QueryView {
        &self.token
    }
    fn check_active(&self) -> Result<(), QueryError> {
        self.lease
            .check_active()
            .map_err(|_| QueryError::ReadCancelled)
    }
}
struct Source<'a> {
    input: &'a [i64],
    next: usize,
    clock: Option<&'a ScheduledQueryClock<StdVfs>>,
}
impl<'v, 'm, 'g> PullOperator<'v, 'm, 'g> for Source<'_> {
    fn node(&self) -> PlanNodeId {
        PlanNodeId(1)
    }
    fn prepare_search(
        &mut self,
        _: PlanNodeId,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        Err(RuntimeError::Batch)
    }
    fn pull(
        &mut self,
        context: &mut RuntimeContext<'v, 'm, 'g>,
        output: &mut RowBatch<'v, 'm, 'g>,
    ) -> Result<PullState, RuntimeError> {
        if self.next == 0
            && let Some(clock) = self.clock
        {
            clock.arm_query();
        }
        for _ in 0..3 {
            let Some(value) = self.input.get(self.next) else {
                return Ok(PullState::Done);
            };
            context.charge(WorkKind::OperatorRows, 1)?;
            output.push_row(&[QueryValue::I64(*value)], context)?;
            self.next += 1;
        }
        Ok(if self.next == self.input.len() {
            PullState::Done
        } else {
            PullState::More
        })
    }
}
struct Freeze<'a> {
    clock: Option<&'a ScheduledQueryClock<StdVfs>>,
}
impl<'m, 'g: 'm> Completion<'m, 'g> for Freeze<'_> {
    type Output = QueryArena<'m, 'g, i64>;
    fn complete<'v>(
        &mut self,
        rows: &PreparedRows<'v, 'm, 'g>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<Self::Output>, RuntimeError> {
        let mut output = QueryArena::new(context.memory(), rows.rows())?;
        for row in 0..rows.rows() {
            let Some(QueryValue::I64(value)) = rows.value(row, 0) else {
                return Err(RuntimeError::Batch);
            };
            context.charge(WorkKind::CopiedBytes, 8)?;
            output.push(value)?;
        }
        if let Some(clock) = self.clock {
            clock.arm_query();
        }
        FrozenOutput::new(output, rows.rows(), rows.rows() * 8, 0)
    }
}
fn trial(
    seed: u64,
    input: &[i64],
    phase: usize,
    fault: bool,
    row_limit: Option<(WorkKind, u64)>,
) -> Result<(usize, Option<Observation>), String> {
    let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(1024 * 1024),
    )
    .map_err(|e| e.to_string())?;
    let shared = GraphResources::from_store(&store).map_err(|e| e.to_string())?;
    let baseline = shared.reserved_bytes().map_err(|e| e.to_string())?;
    let manual = Arc::new(ManualMonotonicClock::new());
    let schedule = if fault {
        FaultSchedule::single(FaultEvent {
            id: format!("PG9-{seed}-{phase}"),
            op_index: 0,
            layer: Layer::Clock,
            site: FaultSite::Clock,
            mode: FaultMode::ClockJump { seconds: 30 },
            nth_match: if phase == 0 { 5 } else { 3 },
            expected_matches: None,
            deadline_budget_seconds: Some(1),
            path_contains: Some("clock".into()),
            fired: false,
            fire_count: 0,
            path: None,
        })
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
    let observed = {
        let memory = QueryMemory::new(&shared, 256 * 1024).map_err(|e| e.to_string())?;
        let expressions = [Expression::Literal(Literal::I64(0))];
        let projection = [Projection {
            slot: SlotId(0),
            expression: ExprId(0),
        }];
        let edges = [PlanNodeId(0)];
        let operators = [
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &edges,
                kind: OperatorKind::Project(&projection),
            },
        ];
        let mut facts = vec![NodeFacts::default(); 2];
        let input = input.to_vec(); // Harness input owner retained/charged below.
        let mut regions = vec![
            RetainedRegion::slice(&operators).unwrap(),
            RetainedRegion::slice(&expressions).unwrap(),
            RetainedRegion::slice(&projection).unwrap(),
            RetainedRegion::slice(&edges).unwrap(),
            RetainedRegion::vector(&facts).unwrap(),
        ];
        regions.sort();
        let token = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(seed));
        let validation_control = QueryControl::Cancel(CancelToken::new());
        let mut values = ValueContext::new(&token, &validation_control, 8_000_000).unwrap();
        let plan = GraphPlan::validate_with_fact_vec(
            PlanDescription {
                operators: &operators,
                expressions: &expressions,
                parameters: &[],
                root: PlanNodeId(1),
                eager_searches: &[],
            },
            &mut facts,
            PlanFootprint::declared(100000),
            PlanBacking::vector(&regions).unwrap(),
            &mut values,
        )
        .map_err(|e| e.to_string())?;
        let owners = [
            RetainedAllocation::array(&operators).unwrap(),
            RetainedAllocation::array(&expressions).unwrap(),
            RetainedAllocation::array(&projection).unwrap(),
            RetainedAllocation::array(&edges).unwrap(),
            RetainedAllocation::vector(&input).unwrap(),
            RetainedAllocation::plan_facts(&plan).unwrap(),
        ];
        let runtime_plan =
            QueryInputs::reserve(&memory, RetentionInventory::array(&owners), &mut values)
                .map_err(|e| e.to_string())?
                .admit_plan(&plan, &mut values)
                .map_err(|e| e.to_string())?;
        let before = memory.reserved_bytes();
        let view = View {
            token: QueryView::new(token.store(), token.generation()),
            lease: store.snapshot().map_err(|e| e.to_string())?,
        };
        let result = execute(
            view,
            &control,
            &memory,
            &runtime_plan,
            &mut Source {
                input: &input,
                next: 0,
                clock: (phase == 0).then_some(clock.as_ref()),
            },
            &mut Freeze {
                clock: (phase == 1).then_some(clock.as_ref()),
            },
            ExecutionCapacity {
                batch_rows: 3,
                result_rows: input.len(),
                ..ExecutionCapacity::default()
            },
            row_limit
                .map_or(Ok(RuntimeLimits::default()), |(kind, limit)| {
                    RuntimeLimits::default().with_limit(kind, limit)
                })
                .map_err(|e| e.to_string())?,
        );
        clock.finish_query()?;
        let fires: usize = vfs.events().iter().map(|e| e.fire_count).sum();
        let observation = match result {
            Ok(result) => {
                if fault {
                    return Err("PG9 scheduled failure exposed output".into());
                }
                let observation = Observation {
                    output: result.output.as_slice().to_vec(),
                    examined: result.counters.get(WorkKind::OperatorRows),
                    source_rows: result.counters.get(WorkKind::RowsOut),
                    collected_rows: result.counters.get(WorkKind::CompletedRows),
                    prepared_bytes: result.counters.get(WorkKind::PreparedPayloadBytes),
                    core_bytes: result.counters.get(WorkKind::CompletedBytes),
                    copied_bytes: result.counters.get(WorkKind::CopiedBytes),
                };
                drop(result);
                oracle::check(&input, &observation)?;
                Some(observation)
            }
            Err(error) => {
                if row_limit.is_some() {
                    if !matches!(error.error, RuntimeError::Limit(kind) if Some(kind) == row_limit.map(|(kind, _)| kind))
                    {
                        return Err(format!("PG9 expected completed-row limit: {error}"));
                    }
                } else if !fault || !matches!(error.error, RuntimeError::Value(QueryError::Timeout))
                {
                    return Err(format!("PG9 unexpected execution failure: {error}"));
                }
                let expected_copy = if row_limit.is_some() {
                    24
                } else if phase == 0 {
                    8
                } else {
                    input.len() as u64 * 24
                };
                if error.counters.get(WorkKind::CopiedBytes) != expected_copy {
                    return Err(format!("PG9 wrong fault phase/copy count: {error:?}"));
                }
                None
            }
        };
        if fault {
            oracle::check_failure(
                observation.is_some(),
                fires,
                1,
                memory.reserved_bytes() - before,
            )?;
        } else if fires != 0 || memory.reserved_bytes() != before {
            return Err("PG9 clean control fire/leak".into());
        }
        observation
    };
    if shared.reserved_bytes().map_err(|e| e.to_string())? != baseline {
        return Err("PG9 shared reservations leaked".into());
    }
    store.close().map_err(|e| e.to_string())?;
    Ok((vfs.events().iter().map(|e| e.fire_count).sum(), observed))
}
pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<ProbeReport, String> {
    let mut rng = super::test_support::seeded_rng("property_graph::runtime_probe", seed);
    let mut input = vec![i64::MIN, i64::MAX, 9_007_199_254_740_993, 7, 7];
    for _ in 0..6 {
        input.push((rng.next_u64() % 5) as i64);
    }
    let mut report = ProbeReport::default();
    for phase in 0..2 {
        let (fires, failed) = trial(seed, &input, phase, true, None)?;
        let (_, clean) = trial(seed, &input, phase, false, None)?;
        if failed.is_some() || clean.is_none() {
            return Err("PG9 fault/control output mismatch".into());
        }
        report.cases += 2;
        report.fault_fires += fires;
        report.clean_controls += 1;
        coverage.hit(REQUIRED_COVERAGE[2 + phase]);
        coverage.hit(REQUIRED_COVERAGE[4]);
        coverage.hit(REQUIRED_COVERAGE[5]);
    }
    coverage.hit(REQUIRED_COVERAGE[0]);
    coverage.hit(REQUIRED_COVERAGE[1]);
    Ok(report)
}

#[test]
fn completed_row_limit_refuses_collection_before_copying_the_unconsumed_row() {
    let (fires, output) = trial(
        49,
        &[1, 2, 3, 4],
        0,
        false,
        Some((WorkKind::CompletedRows, 0)),
    )
    .expect("row cap before collector copy");
    assert_eq!(fires, 0);
    assert!(output.is_none());
}

#[test]
fn prepared_byte_limit_refuses_collection_before_copying_the_unconsumed_payload() {
    let (fires, output) = trial(
        49,
        &[1, 2, 3, 4],
        0,
        false,
        Some((WorkKind::PreparedPayloadBytes, 0)),
    )
    .expect("byte cap before collector copy");
    assert_eq!(fires, 0);
    assert!(output.is_none());
}
