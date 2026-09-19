//! Core scoped compiler-consumer adapters; no Cypher or native graph execution.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use std::{
    cell::Cell,
    mem::{size_of, size_of_val},
};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, SnapshotLease, Store};
use zeppelin_embed::property_graph::{
    GraphGeneration, StoreInstanceId,
    query::{MAX_VALUE_WORK, QueryError, QueryValue, QueryView, plan::*, resources::*, runtime::*},
    resources::GraphResources,
};

fn fixture(run: impl FnOnce(&Store, &QueryMemory<'_>)) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new()
            .with_max_resident_bytes(16 * 1024 * 1024)
            .with_reader_drain_timeout(std::time::Duration::ZERO),
    )
    .unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let initial = shared.reserved_bytes().unwrap();
    {
        let memory = QueryMemory::new(&shared, 4 * 1024 * 1024).unwrap();
        let before = memory.reserved_bytes();
        run(&store, &memory);
        assert_eq!(memory.reserved_bytes(), before);
    }
    assert!(shared.reserved_bytes().unwrap() <= initial);
    store.close().unwrap();
}

struct View<'a> {
    token: QueryView,
    lease: SnapshotLease,
    wait_close: Option<&'a Cell<bool>>,
}
impl RetainedView for View<'_> {
    fn query_view(&self) -> &QueryView {
        &self.token
    }
    fn check_active(&self) -> Result<(), QueryError> {
        if self.wait_close.is_some_and(|armed| armed.replace(false)) {
            self.lease
                .wait_for_close_cancellation()
                .map_err(|_| QueryError::Control)?;
        }
        self.lease
            .check_active()
            .map_err(|_| QueryError::ReadCancelled)
    }
}
fn view(store: &Store) -> View<'static> {
    View {
        token: QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0)),
        lease: store.snapshot().unwrap(),
        wait_close: None,
    }
}

fn with_plan<'v, 'm, 'g, T>(
    context: &mut RuntimeContext<'v, 'm, 'g>,
    run: impl FnOnce(&RuntimePlan<'_, '_, '_, '_, '_, '_>, &mut RuntimeContext<'v, 'm, 'g>) -> T,
) -> T {
    let memory = context.memory();
    let mut scratch = memory.reserve_external_capacity().unwrap();
    scratch
        .reserve_additional(
            VALIDATION_SCRATCH_BYTES
                + size_of::<[RetainedRegion; 2]>()
                + size_of::<PlanDescription<'_>>(),
        )
        .unwrap();
    let mut operators = QueryArena::new(memory, 1).unwrap();
    operators
        .push(Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        })
        .unwrap();
    let mut facts = QueryArena::new(memory, 16).unwrap();
    facts.push(NodeFacts::default()).unwrap();
    let fact_bytes = facts.heap_bytes();
    let mut regions = [
        RetainedRegion::declared(
            operators.as_slice().as_ptr() as usize,
            operators.heap_bytes(),
        )
        .unwrap(),
        RetainedRegion::declared(facts.as_slice().as_ptr() as usize, fact_bytes).unwrap(),
    ];
    regions.sort();
    let (plan, facts_owner) = facts
        .validate_plan(
            PlanDescription {
                operators: operators.as_slice(),
                expressions: &[],
                parameters: &[],
                root: PlanNodeId(0),
                eager_searches: &[],
            },
            PlanFootprint::declared(memory.reserved_bytes()),
            PlanBacking::new(&regions, size_of_val(&regions)).unwrap(),
            context.values(),
        )
        .unwrap();
    let owners = [RetainedAllocation::arena(&operators).unwrap(), facts_owner];
    let before = memory.reserved_bytes();
    let inputs =
        QueryInputs::reserve(memory, RetentionInventory::array(&owners), context.values()).unwrap();
    let credited_delta = memory.reserved_bytes() - before;
    drop(inputs);
    let uncredited = [
        RetainedAllocation::arena(&operators).unwrap(),
        RetainedAllocation::plan_facts(&plan).unwrap(),
    ];
    let duplicate = QueryInputs::reserve(
        memory,
        RetentionInventory::array(&uncredited),
        context.values(),
    )
    .unwrap();
    assert_eq!(
        memory.reserved_bytes() - before,
        credited_delta + fact_bytes,
        "only actual fact backing is credited"
    );
    drop(duplicate);
    let inputs =
        QueryInputs::reserve(memory, RetentionInventory::array(&owners), context.values()).unwrap();
    let admitted = inputs.admit_plan(&plan, context.values()).unwrap();
    let before = memory.reserved_bytes();
    let output = run(&admitted, context);
    assert_eq!(
        memory.reserved_bytes(),
        before,
        "driver must release only its temporary owners"
    );
    output
}

#[test]
fn charged_fact_arena_is_credited_once_and_rejects_another_query_owner() {
    fixture(|store, memory| {
        let control = QueryControl::Cancel(CancelToken::new());
        let retained = view(store);
        let mut context =
            RuntimeContext::new(&retained, &control, memory, RuntimeLimits::default()).unwrap();
        let shared = GraphResources::from_store(store).unwrap();
        let other_memory = QueryMemory::new(&shared, 4 * 1024 * 1024).unwrap();
        with_plan(&mut context, |_, context| {
            let mut facts = QueryArena::new(memory, 4).unwrap();
            facts.push(NodeFacts::default()).unwrap();
            let operators = [Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            }];
            let mut regions = [
                RetainedRegion::slice(&operators).unwrap(),
                RetainedRegion::declared(facts.as_slice().as_ptr() as usize, facts.heap_bytes())
                    .unwrap(),
            ];
            regions.sort();
            let (plan, owner) = facts
                .validate_plan(
                    PlanDescription {
                        operators: &operators,
                        expressions: &[],
                        parameters: &[],
                        root: PlanNodeId(0),
                        eager_searches: &[],
                    },
                    PlanFootprint::declared(memory.reserved_bytes()),
                    PlanBacking::new(&regions, size_of_val(&regions)).unwrap(),
                    context.values(),
                )
                .unwrap();
            assert_eq!(plan.facts(PlanNodeId(0)).unwrap().width(), 0);
            let before = other_memory.reserved_bytes();
            assert!(matches!(
                QueryInputs::reserve(
                    &other_memory,
                    RetentionInventory::array(&[owner]),
                    context.values()
                ),
                Err(MemoryError::UnprovedInput)
            ));
            assert_eq!(other_memory.reserved_bytes(), before);
        });
    });
}

struct Source {
    calls: usize,
}
impl<'v, 'm, 'g> PullOperator<'v, 'm, 'g> for Source {
    fn node(&self) -> PlanNodeId {
        PlanNodeId(0)
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
        self.calls += 1;
        context.charge(WorkKind::Expressions, 2)?;
        for _ in 0..3 {
            QueryValue::I64(7).equivalent(QueryValue::I64(7), context.values())?;
        }
        output.push_row(&[], context)?;
        Ok(PullState::Done)
    }
}
struct Complete {
    calls: usize,
}
impl<'m, 'g> Completion<'m, 'g> for Complete {
    type Output = u64;
    fn complete<'v>(
        &mut self,
        rows: &PreparedRows<'v, 'm, 'g>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<u64>, RuntimeError> {
        self.calls += 1;
        QueryValue::I64(9).equivalent(QueryValue::I64(9), context.values())?;
        FrozenOutput::new(context.values().work(), rows.rows(), 0, 0)
    }
}

#[test]
fn borrowed_driver_keeps_prior_validation_value_and_operator_work() {
    fixture(|store, memory| {
        let control = QueryControl::Cancel(CancelToken::new());
        let retained = view(store);
        let mut view_charge = memory.reserve_external_capacity().unwrap();
        view_charge
            .reserve_additional(size_of::<View<'_>>())
            .unwrap();
        let mut context =
            RuntimeContext::new(&retained, &control, memory, RuntimeLimits::default()).unwrap();
        for _ in 0..11 {
            QueryValue::I64(1)
                .equivalent(QueryValue::I64(1), context.values())
                .unwrap();
        }
        let prior = context.values().work();
        assert_eq!(prior, 11);
        context.charge(WorkKind::Expressions, 5).unwrap();
        with_plan(&mut context, |plan, context| {
            let prepared_work = context.values().work();
            assert!(prepared_work > prior);
            let mut source = Source { calls: 0 };
            let mut completion = Complete { calls: 0 };
            let result = execute_in(
                context,
                plan,
                &mut source,
                &mut completion,
                ExecutionCapacity {
                    batch_rows: 1,
                    result_rows: 1,
                    ..ExecutionCapacity::default()
                },
            )
            .unwrap();
            assert_eq!((source.calls, completion.calls), (1, 1));
            assert_eq!(result.counters.get(WorkKind::Expressions), 7);
            assert_eq!(result.counters.get(WorkKind::CompletedRows), 1);
            assert_eq!(result.output, prepared_work + 4);
            assert_eq!(context.values().work(), result.output);
        });
    });
}

#[test]
fn borrowed_driver_cannot_restart_exhausted_value_work() {
    fixture(|store, memory| {
        let control = QueryControl::Cancel(CancelToken::new());
        let retained = view(store);
        let mut view_charge = memory.reserve_external_capacity().unwrap();
        view_charge
            .reserve_additional(size_of::<View<'_>>())
            .unwrap();
        let mut context =
            RuntimeContext::new(&retained, &control, memory, RuntimeLimits::default()).unwrap();
        with_plan(&mut context, |plan, context| {
            while context.values().work() < MAX_VALUE_WORK - 2 {
                QueryValue::I64(1)
                    .equivalent(QueryValue::I64(1), context.values())
                    .unwrap();
            }
            let mut source = Source { calls: 0 };
            let mut completion = Complete { calls: 0 };
            let failure = execute_in(
                context,
                plan,
                &mut source,
                &mut completion,
                ExecutionCapacity::default(),
            )
            .err()
            .expect("prior work must remain consumed");
            assert!(matches!(
                failure.error,
                RuntimeError::Value(QueryError::WorkLimit)
            ));
            assert_eq!((source.calls, completion.calls), (1, 0));
            assert_eq!(context.values().work(), MAX_VALUE_WORK);
            assert_eq!(failure.counters.get(WorkKind::CompletedRows), 0);
        });
    });
}

#[test]
fn borrowed_driver_final_close_precedes_cancel_and_drops_completed_owner() {
    struct Owned<'a, 'm, 'g> {
        _bytes: QueryArena<'m, 'g, u8>,
        dropped: &'a Cell<usize>,
    }
    impl Drop for Owned<'_, '_, '_> {
        fn drop(&mut self) {
            self.dropped.set(self.dropped.get() + 1);
        }
    }
    struct Closing<'a> {
        start: std::sync::mpsc::SyncSender<()>,
        armed: &'a Cell<bool>,
        cancel: &'a CancelToken,
        calls: &'a Cell<usize>,
        dropped: &'a Cell<usize>,
    }
    impl<'a, 'm, 'g: 'm> Completion<'m, 'g> for Closing<'a> {
        type Output = Owned<'a, 'm, 'g>;
        fn complete<'v>(
            &mut self,
            rows: &PreparedRows<'v, 'm, 'g>,
            context: &mut RuntimeContext<'v, 'm, 'g>,
        ) -> Result<FrozenOutput<Self::Output>, RuntimeError> {
            let mut bytes = QueryArena::new(context.memory(), 64)?;
            bytes.push(7)?;
            self.calls.set(self.calls.get() + 1);
            self.armed.set(true);
            self.cancel.cancel();
            self.start.send(()).unwrap();
            FrozenOutput::new(
                Owned {
                    _bytes: bytes,
                    dropped: self.dropped,
                },
                rows.rows(),
                1,
                0,
            )
        }
    }
    fixture(|store, memory| {
        std::thread::scope(|scope| {
            let (start, wait) = std::sync::mpsc::sync_channel(0);
            let closing = scope.spawn(move || {
                wait.recv().unwrap();
                store.close().unwrap();
            });
            let armed = Cell::new(false);
            let calls = Cell::new(0);
            let dropped = Cell::new(0);
            {
                let mut view_charge = memory.reserve_external_capacity().unwrap();
                view_charge
                    .reserve_additional(size_of::<View<'_>>())
                    .unwrap();
                let mut retained = view(store);
                retained.wait_close = Some(&armed);
                let token = CancelToken::new();
                let control = QueryControl::Cancel(token.clone());
                let mut context =
                    RuntimeContext::new(&retained, &control, memory, RuntimeLimits::default())
                        .unwrap();
                with_plan(&mut context, |plan, context| {
                    let mut source = Source { calls: 0 };
                    let mut completion = Closing {
                        start,
                        armed: &armed,
                        cancel: &token,
                        calls: &calls,
                        dropped: &dropped,
                    };
                    let failure = execute_in(
                        context,
                        plan,
                        &mut source,
                        &mut completion,
                        ExecutionCapacity::default(),
                    )
                    .err()
                    .expect("closed view cannot expose owned result");
                    assert!(
                        matches!(
                            failure.error,
                            RuntimeError::Value(QueryError::ReadCancelled)
                        ),
                        "close precedes simultaneous caller cancellation: {failure}"
                    );
                    assert_eq!((calls.get(), dropped.get()), (1, 1));
                });
                // execute_in borrows this actual retained lease. The caller,
                // rather than the borrowed driver, owns its final release.
            }
            closing.join().unwrap();
        });
    });
}
