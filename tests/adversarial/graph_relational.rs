//! PG14 real relational kernels and scheduled clock failures. Typed controlled
//! inputs prove kernel semantics; native expression/admission remains ZE-51/53.
use super::coverage::CoverageRegistry;
use super::fault_vfs::{
    FaultEvent, FaultMode, FaultSchedule, FaultSite, Layer, ScheduledQueryClock, ScheduledVfs,
};
use rand::RngCore;
use std::{sync::Arc, time::Duration};
use zeppelin_embed::lifecycle::{
    Deadline, ManualMonotonicClock, OpenOptions, QueryControl, SnapshotLease, Store,
};
use zeppelin_embed::property_graph::query::{QueryError, QueryValue, QueryView};
use zeppelin_embed::property_graph::query::{
    eligibility::EligibleNodeSet, plan::SlotId, relational::*, resources::*, runtime::*,
};
use zeppelin_embed::property_graph::{
    GraphGeneration, NodeId, StoreInstanceId, resources::GraphResources,
};
use zeppelin_embed::vfs::StdVfs;
use zeppelin_embed_adversarial_oracle::graph_relational as oracle;

pub const REQUIRED_COVERAGE: &[&str] = &[
    "property-graph.relational.order-bags",
    "property-graph.relational.collect",
    "property-graph.relational.eligible",
    "property-graph.relational.clock.sort",
    "property-graph.relational.clock.aggregate",
    "property-graph.relational.clock.eligibility",
    "property-graph.relational.same-seed-control",
    "property-graph.relational.release",
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
fn trial(
    seed: u64,
    phase: usize,
    fault: bool,
    numbers: &[i64],
    nodes: &[u128],
) -> Result<usize, String> {
    let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(4 * 1024 * 1024),
    )
    .map_err(|e| e.to_string())?;
    let shared = GraphResources::from_store(&store).map_err(|e| e.to_string())?;
    let baseline = shared.reserved_bytes().map_err(|e| e.to_string())?;
    let manual = Arc::new(ManualMonotonicClock::new());
    let schedule = if fault {
        FaultSchedule::single(FaultEvent {
            id: format!("PG14-{seed}-{phase}"),
            op_index: 0,
            layer: Layer::Clock,
            site: FaultSite::Clock,
            mode: FaultMode::ClockJump { seconds: 30 },
            nth_match: match phase {
                0 => 37,
                1 => 111,
                _ => 121,
            },
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
    {
        let memory = QueryMemory::new(&shared, 2 * 1024 * 1024).map_err(|e| e.to_string())?;
        let view = View {
            token: QueryView::new(
                StoreInstanceId::new(1).map_err(|e| e.to_string())?,
                GraphGeneration::new(0),
            ),
            lease: store.snapshot().map_err(|e| e.to_string())?,
        };
        let mut context = RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default())
            .map_err(|e| e.to_string())?;
        let before = memory.reserved_bytes();
        let result = (|| -> Result<Vec<u128>, RuntimeError> {
            let capacity = StorageCapacity {
                rows: numbers.len(),
                payload_bytes: 32768,
                variable: ArenaCapacity {
                    list_cells: numbers.len(),
                    ..ArenaCapacity::default()
                },
            };
            if phase == 2 {
                clock.arm_query();
                let values = nodes
                    .iter()
                    .map(|id| NodeId::new(*id).map(|id| view.token.node(id)));
                let ids: Vec<_> = values
                    .collect::<Result<_, _>>()
                    .map_err(|_| RuntimeError::Batch)?;
                let set = EligibleNodeSet::build(&mut context, nodes.len(), ids)?;
                return Ok(set
                    .ids_for(&view.token)?
                    .iter()
                    .map(|id| id.get())
                    .collect());
            }
            let mut rows = Rows::new(&context, &[SlotId(75)], capacity)?;
            for value in numbers {
                rows.push(&[QueryValue::I64(*value)], &mut context)?;
            }
            clock.arm_query();
            if phase == 0 {
                let rows = rows.sort(
                    &[OrderKey {
                        slot: SlotId(75),
                        descending: false,
                    }],
                    &mut context,
                )?;
                (0..rows.len())
                    .map(|row| match rows.value(row, 0) {
                        Some(QueryValue::I64(v)) => Ok(v as u128),
                        _ => Err(RuntimeError::Batch),
                    })
                    .collect()
            } else {
                let rows = rows.aggregate(
                    &[],
                    &[AggregateColumn {
                        output: SlotId(5),
                        operation: Aggregate::Collect {
                            slot: SlotId(75),
                            distinct: true,
                        },
                    }],
                    StorageCapacity {
                        rows: 1,
                        ..capacity
                    },
                    &mut context,
                )?;
                let Some(QueryValue::List(list)) = rows.value(0, 0) else {
                    return Err(RuntimeError::Batch);
                };
                (0..list.len())
                    .map(|index| match list.get(index) {
                        Some(QueryValue::I64(v)) => Ok(v as u128),
                        _ => Err(RuntimeError::Batch),
                    })
                    .collect()
            }
        })();
        clock.finish_query()?;
        let fires = vfs.events().iter().map(|event| event.fire_count).sum();
        if fault {
            if !matches!(result, Err(RuntimeError::Value(QueryError::Timeout))) {
                return Err(format!("PG14 unexpected fault phase{phase}: {result:?}"));
            }
            oracle::check_failure(result.is_ok(), fires, memory.reserved_bytes() - before)?;
        } else {
            oracle::check(phase, numbers, nodes, &result.map_err(|e| e.to_string())?)?;
            if fires != 0 || memory.reserved_bytes() != before {
                return Err("PG14 clean leaked owner or fired fault".into());
            }
        }
    }
    if shared.reserved_bytes().map_err(|e| e.to_string())? != baseline {
        return Err("PG14 shared owners leaked".into());
    }
    store.close().map_err(|e| e.to_string())?;
    Ok(vfs.events().iter().map(|event| event.fire_count).sum())
}
pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<ProbeReport, String> {
    let mut rng = super::test_support::seeded_rng("property_graph::relational_probe", seed);
    let numbers: Vec<_> = (0..64).map(|_| (rng.next_u64() % 31) as i64 - 15).collect();
    let nodes: Vec<_> = numbers
        .iter()
        .enumerate()
        .map(|(position, value)| {
            ((*value + 16) as u128) | if position % 2 == 0 { 1u128 << 100 } else { 0 }
        })
        .collect();
    let mut report = ProbeReport::default();
    for phase in 0..3 {
        report.fault_fires += trial(seed, phase, true, &numbers, &nodes)?;
        trial(seed, phase, false, &numbers, &nodes)?;
        report.cases += 2;
        report.clean_controls += 1;
        coverage.hit(REQUIRED_COVERAGE[phase]);
        coverage.hit(REQUIRED_COVERAGE[phase + 3]);
    }
    coverage.hit(REQUIRED_COVERAGE[6]);
    coverage.hit(REQUIRED_COVERAGE[7]);
    Ok(report)
}
