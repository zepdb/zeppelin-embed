#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::property_graph::query::pattern::checkpoint_tests::with_hook;
use crate::property_graph::storage::tree::directory::TreeError;
use std::time::{Duration, Instant};

type PipelineRow = (u128, i64, u128, u128);

struct CheckedConsumer(PipelineConsumer);
impl NativeReadConsumer<Result<PipelineExecution, RuntimeFailure<NativeExecutionError>>>
    for CheckedConsumer
{
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &crate::property_graph::storage::GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<Result<PipelineExecution, RuntimeFailure<NativeExecutionError>>, TreeError> {
        let before = runtime.memory().reserved_bytes();
        let result = self.0.consume(view, runtime);
        assert_eq!(
            runtime.memory().reserved_bytes(),
            before,
            "all query owners released"
        );
        result
    }
}

fn consumer(source: NodeId, completed: Arc<AtomicBool>) -> CheckedConsumer {
    CheckedConsumer(PipelineConsumer {
        source,
        mode: ProbeMode::Clean,
        cancel: None,
        deadline_clock: None,
        close_action: None,
        fault_vfs: None,
        completion: Some(completed),
        cancel_observation: None,
    })
}

fn fixture(
    path: &Path,
    vfs: Arc<ScheduledMapVfs>,
    clock: Arc<ManualMonotonicClock>,
) -> (Arc<Store>, NodeId, Vec<PipelineRow>) {
    let mut entropy = OsEntropy;
    let store = Arc::new(
        Store::create_native_graph_with_infrastructure(
            path,
            OpenOptions::new()
                .with_max_resident_bytes(256 * 1024 * 1024)
                .with_reader_drain_timeout(Duration::ZERO),
            None,
            vfs,
            clock,
            &mut entropy,
        )
        .unwrap(),
    );
    let receipts = crate::property_graph::with_local_refs(|refs| {
        let node = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let writes = [
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze266", "s").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&node)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze266", "t").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&node)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "ze266", "r").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &[],
                }),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "ze266", "r2").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &[],
                }),
            },
        ];
        store
            .apply_native_graph(&writes, &QueryControl::Cancel(CancelToken::new()))
            .unwrap()
    });
    let EntityId::Node(source) = receipts[0].entity else {
        panic!("source")
    };
    let EntityId::Node(target) = receipts[1].entity else {
        panic!("target")
    };
    let EntityId::Relationship(edge) = receipts[3].entity else {
        panic!("edge")
    };
    (
        store,
        source,
        vec![(target.get(), 1, edge.get(), source.get())],
    )
}

#[test]
fn ze266_independent_operator_close_map_and_deadline_checkpoints() {
    // Sort, DISTINCT, aggregate and OFFSET/LIMIT each initiate their own
    // subtree. Fail only after reaching the selected operator's entry poll.
    for target in [PlanNodeId(3), PlanNodeId(5), PlanNodeId(6), PlanNodeId(7)] {
        for fault in ["close", "map", "deadline"] {
            let dir = tempfile::tempdir().unwrap();
            let vfs = Arc::new(ScheduledMapVfs::default());
            let clock = Arc::new(ManualMonotonicClock::new());
            let (store, source, expected) =
                fixture(&dir.path().join("native"), vfs.clone(), clock.clone());
            let clean = store
                .with_native_read(
                    &QueryControl::Cancel(CancelToken::new()),
                    RuntimeLimits::default(),
                    16 * 1024 * 1024,
                    64,
                    consumer(source, Arc::new(AtomicBool::new(false))),
                )
                .unwrap()
                .unwrap();
            assert_eq!(clean.execution.output, expected);
            let deadline =
                Deadline::after_with_test_clock(Duration::from_secs(1), clock.clone()).unwrap();
            let control = if fault == "deadline" {
                QueryControl::Deadline(deadline)
            } else {
                QueryControl::Cancel(CancelToken::new())
            };
            let completed = Arc::new(AtomicBool::new(false));
            let fired = Arc::new(AtomicBool::new(false));
            let hit = fired.clone();
            let owner = store.clone();
            let maps = vfs.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            let result = with_hook(
                move |preparing, node| {
                    if preparing || node != target || hit.swap(true, Ordering::SeqCst) {
                        return;
                    }
                    match fault {
                        "map" => maps.fail_next.store(true, Ordering::SeqCst),
                        "deadline" => clock.advance(Duration::from_secs(2)),
                        _ => {
                            let close_store = owner.clone();
                            let tx = tx.clone();
                            std::thread::spawn(move || tx.send(close_store.close()).unwrap());
                            let end = Instant::now() + Duration::from_secs(10);
                            while owner.state().unwrap() == crate::lifecycle::StoreState::Open {
                                assert!(Instant::now() < end, "close checkpoint timed out");
                                std::thread::yield_now();
                            }
                        }
                    }
                },
                || {
                    store.with_native_read(
                        &control,
                        RuntimeLimits::default(),
                        16 * 1024 * 1024,
                        64,
                        consumer(source, completed.clone()),
                    )
                },
            );
            assert!(
                fired.load(Ordering::SeqCst),
                "{target:?} {fault} checkpoint reached"
            );
            assert!(
                !completed.load(Ordering::SeqCst),
                "no partial result completed"
            );
            if fault == "map" {
                let failure = result.unwrap().err().unwrap();
                assert!(
                    matches!(failure.error, NativeExecutionError::Tree(TreeError::Io(_))),
                    "{:?}",
                    failure.error
                );
                assert_eq!(vfs.fires.load(Ordering::SeqCst), 1);
                let control = store
                    .with_native_read(
                        &QueryControl::Cancel(CancelToken::new()),
                        RuntimeLimits::default(),
                        16 * 1024 * 1024,
                        64,
                        consumer(source, Arc::new(AtomicBool::new(false))),
                    )
                    .unwrap()
                    .unwrap();
                assert_eq!(control.execution.output, expected);
            } else {
                let expected_error = if fault == "close" {
                    crate::property_graph::query::QueryError::ReadCancelled
                } else {
                    crate::property_graph::query::QueryError::Timeout
                };
                assert!(
                    matches!(result, Err(crate::lifecycle::native_graph::NativeGraphError::Read(
                    TreeError::Runtime(RuntimeError::Value(actual)))) if actual == expected_error)
                );
            }
            if fault == "close" {
                rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
            }
            store.close().unwrap();
        }
    }
}

#[test]
fn ze266_preparation_cancel_releases_partly_built_operator_tree() {
    let dir = tempfile::tempdir().unwrap();
    let (store, source, expected) = fixture(
        &dir.path().join("native"),
        Arc::new(ScheduledMapVfs::default()),
        Arc::new(ManualMonotonicClock::new()),
    );
    let cancel = CancelToken::new();
    let trigger = cancel.clone();
    let completed = Arc::new(AtomicBool::new(false));
    let fired = Arc::new(AtomicBool::new(false));
    let hit = fired.clone();
    let result = with_hook(
        move |preparing, node| {
            if preparing && node == PlanNodeId(3) {
                hit.store(true, Ordering::SeqCst);
                trigger.cancel();
            }
        },
        || {
            store.with_native_read(
                &QueryControl::Cancel(cancel),
                RuntimeLimits::default(),
                16 * 1024 * 1024,
                64,
                consumer(source, completed.clone()),
            )
        },
    );
    assert!(fired.load(Ordering::SeqCst));
    assert!(!completed.load(Ordering::SeqCst));
    assert!(matches!(
        result,
        Err(crate::lifecycle::native_graph::NativeGraphError::Read(
            TreeError::Runtime(RuntimeError::Value(
                crate::property_graph::query::QueryError::Cancelled
            ))
        ))
    ));
    let clean = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            64,
            consumer(source, Arc::new(AtomicBool::new(false))),
        )
        .unwrap()
        .unwrap();
    assert_eq!(clean.execution.output, expected);
    store.close().unwrap();
}
