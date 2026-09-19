#![allow(clippy::expect_used, clippy::panic)]
use std::cell::Cell;
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, SnapshotLease, Store};
use zeppelin_embed::property_graph::query::plan::*;
use zeppelin_embed::property_graph::query::resources::*;
use zeppelin_embed::property_graph::query::runtime::*;
use zeppelin_embed::property_graph::query::{QueryError, QueryView, ValueContext};
use zeppelin_embed::property_graph::resources::GraphResources;
use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};

fn with_plan<R>(
    operation: impl FnOnce(&Store, &QueryMemory<'_>, &RuntimePlan<'_, '_, '_, '_, '_, '_>) -> R,
) -> R {
    let root = tempfile::tempdir().expect("fixture");
    let store = Store::open(
        root.path(),
        OpenOptions::new()
            .with_max_resident_bytes(262144)
            .with_reader_drain_timeout(std::time::Duration::ZERO),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("shared accounting");
    let initial = shared.reserved_bytes().expect("baseline");
    let result = {
        let memory = QueryMemory::new(&shared, 131072).expect("query");
        let operators = [Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        }];
        let mut facts = Vec::with_capacity(8);
        facts.push(NodeFacts::default());
        let mut regions = vec![
            RetainedRegion::slice(&operators).expect("operators"),
            RetainedRegion::vector(&facts).expect("facts"),
        ];
        regions.sort();
        let token = QueryView::new(
            StoreInstanceId::new(1).expect("identity"),
            GraphGeneration::new(0),
        );
        let control = QueryControl::Cancel(CancelToken::new());
        let mut values = ValueContext::new(&token, &control, 10000).expect("values");
        let plan = GraphPlan::validate_with_fact_vec(
            PlanDescription {
                operators: &operators,
                expressions: &[],
                parameters: &[],
                root: PlanNodeId(0),
                eager_searches: &[],
            },
            &mut facts,
            PlanFootprint::declared(100000),
            PlanBacking::vector(&regions).expect("proof"),
            &mut values,
        )
        .expect("typed plan");
        let owners = [
            RetainedAllocation::array(&operators).expect("operators"),
            RetainedAllocation::plan_facts(&plan).expect("actual facts capacity"),
        ];
        let admitted =
            QueryInputs::reserve(&memory, RetentionInventory::array(&owners), &mut values)
                .expect("owners")
                .admit_plan(&plan, &mut values)
                .expect("runtime plan");
        let before = memory.reserved_bytes();
        let result = operation(&store, &memory, &admitted);
        assert_eq!(
            memory.reserved_bytes(),
            before,
            "every private buffer/guard released"
        );
        result
    };
    // Close may already have released the legacy store baseline.
    assert!(shared.reserved_bytes().expect("no graph reservations") <= initial);
    store.close().expect("no leaked runtime lease");
    result
}
struct View<'a> {
    token: QueryView,
    lease: SnapshotLease,
    wait_for_close: Option<&'a Cell<bool>>,
}
impl RetainedView for View<'_> {
    fn query_view(&self) -> &QueryView {
        &self.token
    }
    fn check_active(&self) -> Result<(), QueryError> {
        if self
            .wait_for_close
            .is_some_and(|armed| armed.replace(false))
        {
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
        token: QueryView::new(
            StoreInstanceId::new(1).expect("identity"),
            GraphGeneration::new(0),
        ),
        lease: store.snapshot().expect("real lease"),
        wait_for_close: None,
    }
}
struct Source<'a> {
    pulls: &'a Cell<u8>,
    cancel: Option<&'a CancelToken>,
    fail: bool,
}
impl PullOperator for Source<'_> {
    fn node(&self) -> PlanNodeId {
        PlanNodeId(0)
    }
    fn prepare_search(
        &mut self,
        _: PlanNodeId,
        _: &mut RuntimeContext<'_, '_, '_>,
    ) -> Result<(), RuntimeError> {
        Err(RuntimeError::Batch)
    }
    fn pull<'v, 'm, 'g>(
        &mut self,
        context: &mut RuntimeContext<'v, 'm, 'g>,
        output: &mut RowBatch<'v, 'm, 'g>,
    ) -> Result<PullState, RuntimeError> {
        self.pulls.set(self.pulls.get() + 1);
        context.charge(WorkKind::OperatorRows, 1)?;
        output.push_row(&[], context)?;
        if self.pulls.get() == 1 {
            return Ok(PullState::More);
        }
        if let Some(token) = self.cancel {
            token.cancel();
        }
        if self.fail {
            return Err(RuntimeError::Batch);
        }
        Ok(PullState::Done)
    }
}
struct CompletionProbe<'a> {
    calls: &'a Cell<usize>,
    dropped: &'a Cell<usize>,
    cancel: Option<&'a CancelToken>,
}
struct FrozenProbe<'a>(&'a Cell<usize>);
impl Drop for FrozenProbe<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}
impl<'m, 'g, 'a> Completion<'m, 'g> for CompletionProbe<'a> {
    type Output = FrozenProbe<'a>;
    fn complete<'v>(
        &mut self,
        rows: &PreparedRows<'v, 'm, 'g>,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<Self::Output>, RuntimeError> {
        assert_eq!(rows.rows(), 2);
        self.calls.set(self.calls.get() + 1);
        if let Some(token) = self.cancel {
            token.cancel();
        }
        FrozenOutput::new(FrozenProbe(self.dropped), rows.rows(), 0, 0)
    }
}

#[test]
fn failed_last_pull_and_completion_cancellation_never_expose_partial_output() {
    for mode in 0..4 {
        with_plan(|store, memory, plan| {
            let token = CancelToken::new();
            let control = QueryControl::Cancel(token.clone());
            let pulls = Cell::new(0);
            let calls = Cell::new(0);
            let dropped = Cell::new(0);
            let mut source = Source {
                pulls: &pulls,
                cancel: if mode == 2 { Some(&token) } else { None },
                fail: mode == 1,
            };
            let mut completion = CompletionProbe {
                calls: &calls,
                dropped: &dropped,
                cancel: if mode == 3 { Some(&token) } else { None },
            };
            if mode == 0 {
                token.cancel();
            }
            let result = execute(
                view(store),
                &control,
                memory,
                plan,
                &mut source,
                &mut completion,
                ExecutionCapacity {
                    batch_rows: 1,
                    result_rows: 2,
                    ..ExecutionCapacity::default()
                },
                RuntimeLimits::default(),
            );
            let Err(failure) = result else {
                panic!("no output escapes failed execution")
            };
            assert_eq!(failure.operator, PlanNodeId(0));
            if mode != 1 {
                assert!(matches!(
                    failure.error,
                    RuntimeError::Value(QueryError::Cancelled)
                ));
            }
            assert_eq!(pulls.get(), if mode == 0 { 0 } else { 2 });
            assert_eq!(calls.get(), usize::from(mode == 3));
            assert_eq!(
                dropped.get(),
                usize::from(mode == 3),
                "failed final check must drop the already frozen output"
            );
        });
    }
}

#[test]
fn close_after_last_pull_drains_the_same_lease_and_precedes_caller_cancel() {
    for during_completion in [false, true] {
        with_plan(|store, memory, plan| {
            std::thread::scope(|scope| {
                let (start, wait) = std::sync::mpsc::sync_channel(0);
                let closed = scope.spawn(move || {
                    wait.recv().expect("close trigger");
                    store.close().expect("drain runtime");
                });
                let armed = Cell::new(false);
                let token = CancelToken::new();
                let control = QueryControl::Cancel(token.clone());
                struct Closing<'a> {
                    start: std::sync::mpsc::SyncSender<()>,
                    armed: &'a Cell<bool>,
                    cancel: &'a CancelToken,
                    during_completion: bool,
                }
                impl PullOperator for Closing<'_> {
                    fn node(&self) -> PlanNodeId {
                        PlanNodeId(0)
                    }
                    fn prepare_search(
                        &mut self,
                        _: PlanNodeId,
                        _: &mut RuntimeContext<'_, '_, '_>,
                    ) -> Result<(), RuntimeError> {
                        Err(RuntimeError::Batch)
                    }
                    fn pull<'v, 'm, 'g>(
                        &mut self,
                        context: &mut RuntimeContext<'v, 'm, 'g>,
                        output: &mut RowBatch<'v, 'm, 'g>,
                    ) -> Result<PullState, RuntimeError> {
                        output.push_row(&[], context)?;
                        if !self.during_completion {
                            self.armed.set(true);
                            self.cancel.cancel();
                            self.start.send(()).expect("start close");
                        }
                        Ok(PullState::Done)
                    }
                }
                let mut retained = view(store);
                retained.wait_for_close = Some(&armed);
                let mut source = Closing {
                    start: start.clone(),
                    armed: &armed,
                    cancel: &token,
                    during_completion,
                };
                let calls = Cell::new(0);
                let dropped = Cell::new(0);
                struct CloseComplete<'a> {
                    start: std::sync::mpsc::SyncSender<()>,
                    armed: &'a Cell<bool>,
                    cancel: &'a CancelToken,
                    calls: &'a Cell<usize>,
                    dropped: &'a Cell<usize>,
                }
                impl<'m, 'g, 'a> Completion<'m, 'g> for CloseComplete<'a> {
                    type Output = FrozenProbe<'a>;
                    fn complete<'v>(
                        &mut self,
                        rows: &PreparedRows<'v, 'm, 'g>,
                        _: &mut RuntimeContext<'v, 'm, 'g>,
                    ) -> Result<FrozenOutput<Self::Output>, RuntimeError> {
                        self.calls.set(self.calls.get() + 1);
                        self.armed.set(true);
                        self.cancel.cancel();
                        self.start.send(()).expect("close during freeze");
                        FrozenOutput::new(FrozenProbe(self.dropped), rows.rows(), 0, 0)
                    }
                }
                let mut completion = CloseComplete {
                    start,
                    armed: &armed,
                    cancel: &token,
                    calls: &calls,
                    dropped: &dropped,
                };
                let result = execute(
                    retained,
                    &control,
                    memory,
                    plan,
                    &mut source,
                    &mut completion,
                    ExecutionCapacity::default(),
                    RuntimeLimits::default(),
                );
                let Err(failure) = result else {
                    panic!("closed view must discard final batch")
                };
                assert!(
                    matches!(
                        failure.error,
                        RuntimeError::Value(QueryError::ReadCancelled)
                    ),
                    "view close takes precedence: {failure}"
                );
                assert_eq!(calls.get(), usize::from(during_completion));
                assert_eq!(dropped.get(), usize::from(during_completion));
                closed
                    .join()
                    .expect("close unblocked by driver lease release");
            })
        });
    }
}

#[test]
fn every_cumulative_work_cap_checks_exact_boundary_and_overflow() {
    with_plan(|store, memory, _| {
        let retained = view(store);
        let control = QueryControl::Cancel(CancelToken::new());
        let mut context =
            RuntimeContext::new(&retained, &control, memory, RuntimeLimits::default())
                .expect("context");
        let caps = [
            (WorkKind::OperatorRows, 4_000_000),
            (WorkKind::AdjacencyEntries, 2_000_000),
            (WorkKind::Expressions, 8_000_000),
            (WorkKind::HashProbes, 16_000_000),
            (WorkKind::CompletedRows, 65_536),
            (WorkKind::CompletedBytes, 4_194_304),
            (WorkKind::VectorCoordinates, 2_147_483_648),
            (WorkKind::VectorBytes, 8_589_934_592),
            (WorkKind::LexicalPostings, 64_000_000),
            (WorkKind::LexicalBlocks, 4_000_000),
            (WorkKind::SearchInvocations, 8),
            (WorkKind::RowsIn, u64::MAX),
            (WorkKind::CopiedBytes, u64::MAX),
        ];
        for (kind, cap) in caps {
            context.charge(kind, cap).expect("exact cap");
            assert!(
                matches!(context.charge(kind, 1), Err(RuntimeError::Limit(failed)) if failed == kind)
            );
            assert_eq!(
                context.counters().get(kind),
                cap,
                "failed unit was never consumed"
            );
            if cap != u64::MAX {
                assert!(RuntimeLimits::default().with_limit(kind, cap + 1).is_err());
            }
        }
    });
}

#[test]
fn failed_final_view_check_drops_frozen_output_and_metadata_mismatch_is_private() {
    for mismatch in [false, true] {
        with_plan(|store, memory, plan| {
            let armed = Cell::new(false);
            let final_checks = Cell::new(0);
            struct FinalView<'a> {
                inner: View<'a>,
                armed: &'a Cell<bool>,
                checks: &'a Cell<u8>,
            }
            impl RetainedView for FinalView<'_> {
                fn query_view(&self) -> &QueryView {
                    self.inner.query_view()
                }
                fn check_active(&self) -> Result<(), QueryError> {
                    self.inner.check_active()?;
                    if self.armed.get() {
                        self.checks.set(self.checks.get() + 1);
                        if self.checks.get() == 3 {
                            return Err(QueryError::ReadCancelled);
                        }
                    }
                    Ok(())
                }
            }
            struct Freeze<'a> {
                armed: &'a Cell<bool>,
                dropped: &'a Cell<usize>,
                mismatch: bool,
            }
            impl<'m, 'g, 'a> Completion<'m, 'g> for Freeze<'a> {
                type Output = FrozenProbe<'a>;
                fn complete<'v>(
                    &mut self,
                    _: &PreparedRows<'v, 'm, 'g>,
                    _: &mut RuntimeContext<'v, 'm, 'g>,
                ) -> Result<FrozenOutput<Self::Output>, RuntimeError> {
                    self.armed.set(true);
                    FrozenOutput::new(
                        FrozenProbe(self.dropped),
                        if self.mismatch { 1 } else { 2 },
                        7,
                        11,
                    )
                }
            }
            let control = QueryControl::Cancel(CancelToken::new());
            let pulls = Cell::new(0);
            let dropped = Cell::new(0);
            let retained = FinalView {
                inner: view(store),
                armed: &armed,
                checks: &final_checks,
            };
            let result = execute(
                retained,
                &control,
                memory,
                plan,
                &mut Source {
                    pulls: &pulls,
                    cancel: None,
                    fail: false,
                },
                &mut Freeze {
                    armed: &armed,
                    dropped: &dropped,
                    mismatch,
                },
                ExecutionCapacity::default(),
                RuntimeLimits::default(),
            );
            let Err(failure) = result else {
                panic!("no frozen output can escape final refusal")
            };
            if mismatch {
                assert!(matches!(failure.error, RuntimeError::Batch));
                assert_eq!(final_checks.get(), 0);
            } else {
                assert!(matches!(
                    failure.error,
                    RuntimeError::Value(QueryError::ReadCancelled)
                ));
                assert_eq!(
                    final_checks.get(),
                    3,
                    "two metadata counters then mandatory final check"
                );
                assert_eq!(failure.counters.get(WorkKind::CompletedBytes), 7);
                assert_eq!(failure.counters.get(WorkKind::CompletedAbiBytes), 11);
            }
            assert_eq!(dropped.get(), 1);
        });
    }
    let drops = Cell::new(0);
    assert!(FrozenOutput::new(FrozenProbe(&drops), 65537, 0, 0).is_err());
    assert!(FrozenOutput::new(FrozenProbe(&drops), 1, 4194305, 0).is_err());
    assert!(FrozenOutput::new(FrozenProbe(&drops), 1, 0, 4194305).is_err());
    assert_eq!(drops.get(), 3);
    drop(
        FrozenOutput::new(FrozenProbe(&drops), 65536, 4194304, 4194304)
            .expect("separate exact representation caps"),
    );
    assert_eq!(drops.get(), 4);
}

#[test]
fn eager_sources_run_once_in_source_order_even_when_limit_zero_produces_no_rows() {
    let root = tempfile::tempdir().expect("fixture");
    let store = Store::open(
        root.path(),
        OpenOptions::new().with_max_resident_bytes(262144),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("accounting");
    let memory = QueryMemory::new(&shared, 131072).expect("query");
    let expressions = [
        Expression::Literal(Literal::String("")),
        Expression::Literal(Literal::I64(1)),
    ];
    let edges = [PlanNodeId(0)];
    let eager = [PlanNodeId(1), PlanNodeId(2)];
    let search = |call| Operator {
        inputs: &edges,
        kind: OperatorKind::Search {
            call: SearchCallId(call),
            request: SearchRequest::Text {
                query: ExprId(0),
                k: ExprId(1),
                eligible: None,
            },
            node: SlotId(call * 2),
            score: SlotId(call * 2 + 1),
        },
    };
    let operators = [
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        search(0),
        search(1),
        Operator {
            inputs: &edges,
            kind: OperatorKind::OffsetLimit {
                offset: 0,
                limit: Some(0),
            },
        },
    ];
    let mut facts = vec![NodeFacts::default(); 4];
    let mut regions = vec![
        RetainedRegion::slice(&operators).expect("operators"),
        RetainedRegion::slice(&expressions).expect("expressions"),
        RetainedRegion::slice(&edges).expect("edges"),
        RetainedRegion::slice(&eager).expect("eager"),
        RetainedRegion::vector(&facts).expect("facts"),
    ];
    regions.sort();
    let token = QueryView::new(
        StoreInstanceId::new(1).expect("identity"),
        GraphGeneration::new(0),
    );
    let control = QueryControl::Cancel(CancelToken::new());
    let mut values = ValueContext::new(&token, &control, 10000).expect("values");
    let plan = GraphPlan::validate_with_fact_vec(
        PlanDescription {
            operators: &operators,
            expressions: &expressions,
            parameters: &[],
            root: PlanNodeId(3),
            eager_searches: &eager,
        },
        &mut facts,
        PlanFootprint::declared(100000),
        PlanBacking::vector(&regions).expect("proof"),
        &mut values,
    )
    .expect("typed search obligations");
    let owners = [
        RetainedAllocation::array(&operators).expect("operators"),
        RetainedAllocation::array(&expressions).expect("expressions"),
        RetainedAllocation::array(&edges).expect("edges"),
        RetainedAllocation::array(&eager).expect("eager"),
        RetainedAllocation::plan_facts(&plan).expect("facts"),
    ];
    let admitted = QueryInputs::reserve(&memory, RetentionInventory::array(&owners), &mut values)
        .expect("owners")
        .admit_plan(&plan, &mut values)
        .expect("runtime plan");
    struct Eager<'m, 'g> {
        calls: QueryArena<'m, 'g, PlanNodeId>,
        pulls: u8,
        fail_second: bool,
    }
    impl PullOperator for Eager<'_, '_> {
        fn node(&self) -> PlanNodeId {
            PlanNodeId(3)
        }
        fn prepare_search(
            &mut self,
            node: PlanNodeId,
            _: &mut RuntimeContext<'_, '_, '_>,
        ) -> Result<(), RuntimeError> {
            self.calls.push(node)?;
            if self.fail_second && node == PlanNodeId(2) {
                return Err(RuntimeError::Batch);
            }
            Ok(())
        }
        fn pull<'v, 'm, 'g>(
            &mut self,
            _: &mut RuntimeContext<'v, 'm, 'g>,
            _: &mut RowBatch<'v, 'm, 'g>,
        ) -> Result<PullState, RuntimeError> {
            assert_eq!(self.calls.as_slice(), &[PlanNodeId(1), PlanNodeId(2)]);
            self.pulls += 1;
            Ok(PullState::Done)
        }
    }
    struct Empty;
    impl<'m, 'g> Completion<'m, 'g> for Empty {
        type Output = ();
        fn complete<'v>(
            &mut self,
            rows: &PreparedRows<'v, 'm, 'g>,
            _: &mut RuntimeContext<'v, 'm, 'g>,
        ) -> Result<FrozenOutput<()>, RuntimeError> {
            assert_eq!(rows.rows(), 0);
            FrozenOutput::new((), 0, 0, 0)
        }
    }
    for fail_second in [false, true] {
        let mut source = Eager {
            calls: QueryArena::new(&memory, 2).expect("charged reports"),
            pulls: 0,
            fail_second,
        };
        let result = execute(
            view(&store),
            &control,
            &memory,
            &admitted,
            &mut source,
            &mut Empty,
            ExecutionCapacity {
                result_rows: 0,
                ..ExecutionCapacity::default()
            },
            RuntimeLimits::default(),
        );
        assert_eq!(source.calls.as_slice(), &[PlanNodeId(1), PlanNodeId(2)]);
        if fail_second {
            let Err(error) = result else {
                panic!("failed eager call exposes no result")
            };
            assert_eq!(error.operator, PlanNodeId(2));
            assert_eq!(source.pulls, 0);
        } else {
            assert_eq!(
                result
                    .expect("empty success after reports")
                    .counters
                    .get(WorkKind::SearchInvocations),
                2
            );
            assert_eq!(source.pulls, 1);
        }
    }
    store.close().expect("close");
}
