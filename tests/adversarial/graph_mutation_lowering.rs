//! PG21 production mutation lowering, independent primitive oracle, and faults.
//! This compares compiler data only and performs no graph mutation.
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
use zeppelin_embed_adversarial_oracle::graph_mutation_lowering::{self as oracle, Expr, Item};
use zeppelin_embed_cypher::{
    CompileLimits, ErrorKind, LoweredMutation, ResourceError, compile_in, compile_mutation_in,
};

pub const REQUIRED_COVERAGE: &[&str] = &[
    "property-graph.mutation-lowering.create-shapes",
    "property-graph.mutation-lowering.create-dependencies",
    "property-graph.mutation-lowering.item-order",
    "property-graph.mutation-lowering.eager-boundaries",
    "property-graph.mutation-lowering.rhs-observation",
    "property-graph.mutation-lowering.deleted-obligation",
    "property-graph.mutation-lowering.source-parameters",
    "property-graph.mutation-lowering.detach-kinds",
    "property-graph.mutation-lowering.clock.fire",
    "property-graph.mutation-lowering.budget.fire",
    "property-graph.mutation-lowering.same-seed-control",
    "property-graph.mutation-lowering.comparator.fire",
    "property-graph.mutation-lowering.owner-proof",
    "property-graph.mutation-lowering.release",
];

#[derive(Default, Debug)]
pub struct ProbeReport {
    pub observations: usize,
    pub orientations: usize,
    pub fault_fires: usize,
    pub clean_controls: usize,
    pub comparator_fires: usize,
    pub compiler_polls: usize,
    pub consumer_polls: usize,
    pub selected_fire: usize,
    pub compile_peak: usize,
    pub full_peak: usize,
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

fn query(recipe: oracle::Recipe) -> String {
    let relationship = if recipe.incoming {
        "(a:A {p:frozen})<-[r:R {q:type(r)}]-(b:B {x:r.q})"
    } else {
        "(a:A {p:frozen})-[r:R {q:type(r)}]->(b:B {x:r.q})"
    };
    format!(
        "WITH $value AS frozen CREATE {relationship} SET b.p=b.p+1,b.p=frozen REMOVE b.missing,b:Old DETACH DELETE r RETURN b,frozen LIMIT 0"
    )
}

fn expr(description: PlanDescription<'_>, id: ExprId) -> Result<Expr, String> {
    match description
        .expressions
        .get(id.0 as usize)
        .ok_or_else(|| format!("PG21 missing expression {}", id.0))?
    {
        Expression::Literal(Literal::I64(value)) => Ok(Expr::I64(*value)),
        Expression::Slot(slot) => Ok(Expr::Slot(slot.0)),
        Expression::Parameter(parameter) => Ok(Expr::Parameter(parameter.0)),
        Expression::Property { entity, name } => Ok(Expr::Property(
            Box::new(expr(description, *entity)?),
            name.as_str().to_owned(),
        )),
        Expression::Unary {
            operation: UnaryExpression::RelType,
            operand,
        } => Ok(Expr::RelType(Box::new(expr(description, *operand)?))),
        Expression::Binary {
            operation:
                BinaryExpression::Arithmetic(zeppelin_embed::property_graph::query::Arithmetic::Add),
            left,
            right,
        } => Ok(Expr::Add(
            Box::new(expr(description, *left)?),
            Box::new(expr(description, *right)?),
        )),
        other => Err(format!("PG21 unexpected expression {other:?}")),
    }
}

fn observe_main(
    mutation: &LoweredMutation<'_, '_>,
    source: &str,
    memory: &QueryMemory<'_>,
    context: &mut ValueContext<'_>,
) -> Result<oracle::Observation, String> {
    let description = mutation.plan().description();
    let mut items = Vec::new();
    let mut eager_count = 0;
    for operator in description.operators {
        if matches!(operator.kind, OperatorKind::Eager) {
            eager_count += 1;
        }
        let OperatorKind::Mutate(changes) = operator.kind else {
            continue;
        };
        for change in changes {
            items.push(match change {
                Mutation::CreateNode { output, labels } => Item::CreateNode(
                    output.0,
                    labels.iter().map(|name| name.as_str().to_owned()).collect(),
                ),
                Mutation::CreateRelationship {
                    output,
                    source,
                    target,
                    relationship_type,
                } => Item::CreateRelationship(
                    output.0,
                    expr(description, *source)?,
                    expr(description, *target)?,
                    relationship_type.as_str().to_owned(),
                ),
                Mutation::SetProperty {
                    entity,
                    name,
                    value,
                } => Item::SetProperty(
                    expr(description, *entity)?,
                    name.as_str().to_owned(),
                    expr(description, *value)?,
                ),
                Mutation::RemoveProperty { entity, name } => {
                    Item::RemoveProperty(expr(description, *entity)?, name.as_str().to_owned())
                }
                Mutation::SetLabel {
                    entity,
                    label,
                    present,
                } => Item::SetLabel(
                    expr(description, *entity)?,
                    label.as_str().to_owned(),
                    *present,
                ),
                Mutation::Delete { entity, detach } => {
                    Item::Delete(expr(description, *entity)?, *detach)
                }
            });
        }
    }
    let mut inventory_charge = memory
        .reserve_external_capacity()
        .map_err(|error| error.to_string())?;
    inventory_charge
        .reserve_additional(
            std::mem::size_of::<Vec<RetainedAllocation<'_>>>()
                + std::mem::size_of_val(mutation.owners()),
        )
        .map_err(|error| error.to_string())?;
    let mut owners = Vec::new();
    owners
        .try_reserve_exact(mutation.owners().len())
        .map_err(|error| error.to_string())?;
    owners.extend_from_slice(mutation.owners());
    let inputs = QueryInputs::reserve(
        memory,
        RetentionInventory::vector(&owners).map_err(|error| error.to_string())?,
        context,
    )
    .map_err(|error| error.to_string())?;
    inputs
        .verify_span(mutation.mutation_spans(), context)
        .map_err(|error| error.to_string())?;
    let runtime = inputs
        .admit_plan(mutation.plan(), context)
        .map_err(|error| error.to_string())?;
    let owner_proof = runtime.backing_bytes() > 0;
    drop(runtime);
    let parameter_bits = match mutation
        .parameters()
        .first()
        .map(|parameter| parameter.value)
    {
        Some(QueryValue::F64(value)) => value.to_bits(),
        other => return Err(format!("PG21 parameter {other:?}")),
    };
    Ok(oracle::Observation {
        items,
        eager_count,
        limit_zero: description.operators.iter().any(|operator| {
            matches!(
                operator.kind,
                OperatorKind::OffsetLimit {
                    offset: 0,
                    limit: Some(0)
                }
            )
        }),
        columns: mutation
            .columns()
            .iter()
            .map(|column| column.name.to_owned())
            .collect(),
        dynamic_deleted: false,
        no_return_columns: usize::MAX,
        source_exact: mutation.source() == source && mutation.source().as_ptr() != source.as_ptr(),
        parameter_bits,
        owner_proof,
    })
}

struct Trial {
    observation: Option<oracle::Observation>,
    compiler_polls: usize,
    consumer_polls: usize,
    peak: usize,
    fires: usize,
    calls: usize,
}

fn trial(
    seed: u64,
    recipe: oracle::Recipe,
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
                id: format!("PG21-{seed}-{nth_match}"),
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
    let source = query(recipe);
    let parameters = [ParameterBinding {
        name: "value",
        value: QueryValue::F64(f64::from_bits(recipe.parameter_bits)),
    }];
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
                &source,
                &parameters,
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
            (|| {
                let mut observation = None;
                compile_mutation_in(
                    &source,
                    &parameters,
                    CompileLimits::default(),
                    &memory,
                    &mut context,
                    |mutation, context| {
                        report.calls += 1;
                        report.consumer_polls = clock.calls.load(Ordering::SeqCst);
                        match observe_main(&mutation, &source, &memory, context) {
                            Ok(value) => observation = Some(value),
                            Err(error) => observation_error = Some(error),
                        }
                        Ok(())
                    },
                )?;
                compile_mutation_in(
                    "OPTIONAL MATCH (n) DELETE n RETURN n",
                    &[],
                    CompileLimits::default(),
                    &memory,
                    &mut context,
                    |mutation, _| {
                        report.calls += 1;
                        if let Some(value) = &mut observation {
                            value.dynamic_deleted = mutation.requires_deleted_runtime_validation();
                        }
                        Ok(())
                    },
                )?;
                compile_mutation_in(
                    "CREATE (n) SET n.p=1",
                    &[],
                    CompileLimits::default(),
                    &memory,
                    &mut context,
                    |mutation, _| {
                        report.calls += 1;
                        if let Some(value) = &mut observation {
                            value.no_return_columns = mutation.columns().len();
                        }
                        Ok(())
                    },
                )?;
                report.observation = observation;
                Ok(())
            })()
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
                return Err(format!("PG21 expected {expected:?}, got {result:?}"));
            }
            if report.calls != 0 || memory.reserved_bytes() != baseline {
                return Err(format!(
                    "PG21 failure escaped calls={} retained={}",
                    report.calls,
                    memory.reserved_bytes() - baseline
                ));
            }
        } else {
            result.map_err(|error| format!("PG21 compiler {error:?}"))?;
            let expected_calls = if only_compile { 1 } else { 3 };
            if report.calls != expected_calls
                || memory.reserved_bytes() != baseline
                || report.fires != 0
            {
                return Err("PG21 clean call/release mismatch".into());
            }
        }
    }
    if shared.reserved_bytes().map_err(|error| error.to_string())? != shared_baseline {
        return Err("PG21 shared owner retained".into());
    }
    store.close().map_err(|error| error.to_string())?;
    Ok(report)
}

pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<ProbeReport, String> {
    let mut rng = super::test_support::seeded_rng("property_graph::mutation_lowering_probe", seed);
    let parameter_bits = 0x3ff0_0000_0000_0000 ^ (rng.next_u64() & 0x000f_ffff_ffff_ffff);
    let recipes = [
        oracle::Recipe {
            incoming: false,
            parameter_bits,
        },
        oracle::Recipe {
            incoming: true,
            parameter_bits,
        },
    ];
    let mut clean_trials = Vec::with_capacity(recipes.len());
    let mut orientations = 0usize;
    let mut observations = 0usize;
    for recipe in recipes {
        let clean = trial(seed, recipe, None, None, false)?;
        let observed = clean
            .observation
            .as_ref()
            .ok_or_else(|| "PG21 orientation observation absent".to_owned())?;
        oracle::check(recipe, observed)?;
        orientations += 1;
        observations += clean.calls;
        clean_trials.push((recipe, clean));
    }
    let selected =
        usize::try_from(rng.next_u64() & 1).map_err(|_| "PG21 selected orientation".to_owned())?;
    let (recipe, clean) = clean_trials.swap_remove(selected);
    let observed = clean
        .observation
        .ok_or_else(|| "PG21 selected observation absent".to_owned())?;
    let mut report = ProbeReport {
        observations,
        orientations,
        full_peak: clean.peak,
        consumer_polls: clean.consumer_polls,
        ..ProbeReport::default()
    };
    for key in &REQUIRED_COVERAGE[..8] {
        coverage.hit(*key);
    }
    coverage.hit(REQUIRED_COVERAGE[12]);

    for mutation in 0..9 {
        let mut changed = observed.clone();
        let expected = match mutation {
            0 => {
                changed.items.swap(0, 1);
                "items"
            }
            1 => {
                changed.eager_count -= 1;
                "eager_count"
            }
            2 => {
                changed.limit_zero = false;
                "limit_zero"
            }
            3 => {
                changed.columns.pop();
                "columns"
            }
            4 => {
                changed.dynamic_deleted = false;
                "dynamic_deleted"
            }
            5 => {
                changed.no_return_columns = 1;
                "no_return_columns"
            }
            6 => {
                changed.source_exact = false;
                "source_exact"
            }
            7 => {
                changed.parameter_bits ^= 1;
                "parameter_bits"
            }
            _ => {
                changed.owner_proof = false;
                "owner_proof"
            }
        };
        match oracle::check(recipe, &changed) {
            Err(error) if error == expected => {}
            result => {
                return Err(format!(
                    "PG21 planted comparator {mutation} escaped: {result:?}"
                ));
            }
        }
        oracle::check(recipe, &observed)?;
        report.comparator_fires += 1;
    }
    coverage.hit(REQUIRED_COVERAGE[11]);

    let compiler = trial(seed, recipe, None, None, true)?;
    report.compiler_polls = compiler.compiler_polls;
    report.compile_peak = compiler.peak;
    if clean.consumer_polls <= compiler.compiler_polls + 8 {
        return Err("PG21 mutation lowering checkpoint interval absent".into());
    }
    let fire = (compiler.compiler_polls + clean.consumer_polls) / 2;
    report.selected_fire = fire;
    let fault = trial(seed, recipe, Some(fire), None, false)?;
    if fault.fires != 1 {
        return Err(format!("PG21 clock fires={}", fault.fires));
    }
    report.fault_fires += fault.fires;
    coverage.hit(REQUIRED_COVERAGE[8]);
    let clock_control = trial(seed, recipe, None, None, false)?
        .observation
        .ok_or_else(|| "PG21 clock control absent".to_owned())?;
    oracle::check(recipe, &clock_control)?;
    report.clean_controls += 1;

    if clean.peak <= compiler.peak + 1 {
        return Err("PG21 memory interval absent".into());
    }
    trial(seed, recipe, None, Some(compiler.peak + 1), false)?;
    report.fault_fires += 1;
    coverage.hit(REQUIRED_COVERAGE[9]);
    let budget_control = trial(seed, recipe, None, None, false)?
        .observation
        .ok_or_else(|| "PG21 budget control absent".to_owned())?;
    oracle::check(recipe, &budget_control)?;
    report.clean_controls += 1;
    coverage.hit(REQUIRED_COVERAGE[10]);
    coverage.hit(REQUIRED_COVERAGE[13]);
    Ok(report)
}
