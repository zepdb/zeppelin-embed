#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use super::*;

#[test]
fn graph_result_layout_aligns_every_typed_pool_and_rejects_oversize() {
    let plan = ArenaLayout::new(PoolCounts {
        values: 1,
        children: 1,
        bytes: 3,
        nodes: 1,
        relationships: 1,
        properties: 1,
        names: 1,
        vectors: 1,
        columns: 1,
        cells: 1,
        receipts: 1,
        reports: 1,
        diagnostics: 1,
        work: 1,
    })
    .unwrap();
    // Independently worked C-layout oracle, including the byte-to-node gap.
    assert_eq!(
        plan.offsets,
        [
            0, 48, 52, 56, 160, 272, 296, 304, 308, 332, 336, 408, 552, 600
        ]
    );
    assert_eq!(plan.layout.size(), 624);
    assert_eq!(plan.layout.align(), 8);
    let huge = PoolCounts {
        values: usize::MAX,
        ..PoolCounts::default()
    };
    assert!(matches!(ArenaLayout::new(huge), Err(OwnerError::Limit)));
    let too_big = PoolCounts {
        bytes: 4 * 1024 * 1024 + 1,
        ..PoolCounts::default()
    };
    assert!(matches!(ArenaLayout::new(too_big), Err(OwnerError::Limit)));
}

use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, SnapshotLease, Store};
use zeppelin_embed::property_graph::query::resources::QueryMemory;
use zeppelin_embed::property_graph::query::runtime::{RetainedView, RuntimeContext, RuntimeLimits};
use zeppelin_embed::property_graph::query::{QueryError, QueryView};
use zeppelin_embed::property_graph::resources::GraphResources;
use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};

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
pub(super) fn with_context<T>(test: impl FnOnce(&mut RuntimeContext<'_, '_, '_>) -> T) -> T {
    with_limits(24 * 1024 * 1024, RuntimeLimits::default(), test)
}
fn with_limits<T>(
    memory_limit: usize,
    limits: RuntimeLimits,
    test: impl FnOnce(&mut RuntimeContext<'_, '_, '_>) -> T,
) -> T {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(
        dir.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let memory = QueryMemory::new(&shared, memory_limit).unwrap();
    let view = View {
        token: QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0)),
        lease: store.snapshot().unwrap(),
    };
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context = RuntimeContext::new(&view, &control, &memory, limits).unwrap();
    test(&mut context)
}

#[test]
fn graph_result_real_aligned_owner_publishes_and_frees_without_source_lifetime() {
    static REGISTRY: GraphResultRegistry = GraphResultRegistry::new(8);
    with_context(|context| {
        let baseline = context.memory().reserved_bytes();
        // C output structs contain only scalars, ranges and pointers; all-zero
        // bit patterns are valid Rust values. These are component fixtures,
        // not admission, identity, or semantic conversion evidence.
        let values = [unsafe { std::mem::zeroed::<ZeGraphValue>() }];
        let nodes = [unsafe { std::mem::zeroed::<ZeGraphNode>() }];
        let relationships = [unsafe { std::mem::zeroed::<ZeGraphRelationship>() }];
        let properties = [unsafe { std::mem::zeroed::<ZeGraphProperty>() }];
        let columns = [unsafe { std::mem::zeroed::<ZeGraphColumn>() }];
        let receipts = [unsafe { std::mem::zeroed::<ZeGraphReceipt>() }];
        let reports = [unsafe { std::mem::zeroed::<ZeGraphSearchReport>() }];
        let diagnostics = [unsafe { std::mem::zeroed::<ZeGraphDiagnostic>() }];
        let work = [unsafe { std::mem::zeroed::<ZeGraphWorkCounter>() }];
        let names = [ZeGraphRange { start: 0, count: 3 }];
        let parts = ResponseParts {
            values: &values,
            children: &[0],
            bytes: b"a\0z",
            nodes: &nodes,
            relationships: &relationships,
            properties: &properties,
            names: &names,
            vectors: &[1.25],
            columns: &columns,
            cells: &[0],
            receipts: &receipts,
            reports: &reports,
            diagnostics: &diagnostics,
            work: &work,
        };
        let prepared = REGISTRY
            .prepare(context, parts, ResponseMetadata::new(1, Some(42)))
            .unwrap();
        let mut private = prepared.descriptor();
        assert!(matches!(
            REGISTRY.free(&mut private),
            Err(OwnerError::InvalidOwner)
        ));
        assert_eq!(prepared.arena_bytes(), 624);
        assert!(context.memory().reserved_bytes() > baseline + 624);
        let mut result = prepared.expose(SuccessfulOutcome::Read);
        assert_eq!(context.memory().reserved_bytes(), baseline);
        assert_eq!(result.admitted_generation, 42);
        let addresses = [
            result.pool.values as usize,
            result.pool.children as usize,
            result.pool.bytes as usize,
            result.pool.nodes as usize,
            result.pool.relationships as usize,
            result.pool.properties as usize,
            result.pool.names as usize,
            result.pool.vectors as usize,
            result.columns as usize,
            result.cells as usize,
            result.receipts as usize,
            result.reports as usize,
            result.diagnostics as usize,
            result.work as usize,
        ];
        for ((pointer, offset), alignment) in addresses
            .into_iter()
            .zip([
                0, 48, 52, 56, 160, 272, 296, 304, 308, 332, 336, 408, 552, 600,
            ])
            .zip([8, 4, 1, 8, 8, 4, 4, 4, 4, 4, 8, 8, 4, 8])
        {
            assert_eq!(pointer - result.pool.values as usize, offset);
            assert_eq!(pointer % alignment, 0);
        }
        assert_eq!(
            unsafe { std::slice::from_raw_parts(result.pool.bytes, 3) },
            b"a\0z"
        );
        assert_eq!(unsafe { *result.pool.vectors }, 1.25);
        let mut stale = result;
        assert_eq!(REGISTRY.free(&mut result).unwrap().examined_entries, 1);
        assert_eq!(result.owner_token, 0);
        assert_eq!(REGISTRY.free(&mut result).unwrap().examined_entries, 0);
        assert!(matches!(
            REGISTRY.free(&mut stale),
            Err(OwnerError::InvalidOwner)
        ));
        // All fourteen array kinds, including graph entities and list-child
        // storage, participate in exact heap-flat abort/free repetitions.
        let ((), audit) = super::audit::run(0, false, || {
            for iteration in 0..64 {
                let prepared = REGISTRY
                    .prepare(context, parts, ResponseMetadata::new(1, Some(42)))
                    .unwrap();
                if iteration % 2 == 0 {
                    drop(prepared);
                } else {
                    let mut root = prepared.expose(SuccessfulOutcome::Read);
                    REGISTRY.free(&mut root).unwrap();
                }
            }
        });
        assert_eq!((audit.allocations, audit.frees, audit.bytes), (128, 128, 0));
    });
}

#[test]
fn graph_result_cross_registry_empty_owner_cannot_steal_another_allocation() {
    static FIRST: GraphResultRegistry = GraphResultRegistry::new(1);
    static SECOND: GraphResultRegistry = GraphResultRegistry::new(1);
    with_context(|context| {
        let mut first = FIRST
            .prepare(
                context,
                ResponseParts::default(),
                ResponseMetadata::new(0, None),
            )
            .unwrap()
            .expose(SuccessfulOutcome::Read);
        let mut second = SECOND
            .prepare(
                context,
                ResponseParts::default(),
                ResponseMetadata::new(0, None),
            )
            .unwrap()
            .expose(SuccessfulOutcome::Read);
        assert!(matches!(
            SECOND.free(&mut first),
            Err(OwnerError::InvalidOwner)
        ));
        FIRST.free(&mut first).unwrap();
        SECOND.free(&mut second).unwrap();
    });
}

#[test]
fn graph_result_known_commit_survives_unwind_and_unknown_attempt_retains_no_ids() {
    let outcome = OutcomeCell::write();
    assert_eq!(outcome.get(), OperationOutcome::NotCommitted);
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        outcome.begin_attempt().unwrap();
        assert_eq!(outcome.get(), OperationOutcome::Indeterminate);
        outcome
            .record_success(SuccessfulOutcome::Committed(
                std::num::NonZeroU64::new(73).unwrap(),
            ))
            .unwrap();
        panic!("delivery failure after durable outcome");
    }));
    assert!(failure.is_err());
    assert_eq!(
        outcome.get(),
        OperationOutcome::Success(SuccessfulOutcome::Committed(
            std::num::NonZeroU64::new(73).unwrap()
        ))
    );
    assert!(outcome.begin_attempt().is_err());
    assert!(outcome.record_not_committed().is_err());
    assert!(outcome.record_success(SuccessfulOutcome::NoOp).is_err());
    let unknown = OutcomeCell::write();
    unknown.begin_attempt().unwrap();
    assert_eq!(unknown.get(), OperationOutcome::Indeterminate);
    let read = OutcomeCell::read();
    assert_eq!(
        read.get(),
        OperationOutcome::Success(SuccessfulOutcome::Read)
    );
    assert!(read.begin_attempt().is_err());
}

#[test]
fn graph_result_actual_allocator_failures_and_expose_free_have_exact_heap_accounting() {
    use zeppelin_embed::property_graph::query::resources::QueryArena;
    static REGISTRY: GraphResultRegistry = GraphResultRegistry::new(4);
    with_context(|context| {
        let mut source = QueryArena::<u8>::new(context.memory(), 70000).unwrap();
        for _ in 0..70000 {
            source.push(17).unwrap();
        }
        let baseline = context.memory().reserved_bytes();
        let parts = ResponseParts {
            bytes: source.as_slice(),
            ..ResponseParts::default()
        };
        for fail_at in 1..=2 {
            let (result, audit) = super::audit::run(fail_at, false, || {
                REGISTRY.prepare(context, parts, ResponseMetadata::new(0, Some(0)))
            });
            assert!(matches!(result, Err(OwnerError::Allocation)));
            assert_eq!(audit.attempts, fail_at);
            assert_eq!(audit.allocations, fail_at - 1);
            assert_eq!(audit.frees, fail_at - 1);
            assert_eq!(audit.bytes, 0);
            assert_eq!(context.memory().reserved_bytes(), baseline);
        }
        let (prepared, allocation) = super::audit::run(0, false, || {
            REGISTRY
                .prepare(context, parts, ResponseMetadata::new(0, Some(0)))
                .unwrap()
        });
        assert_eq!(allocation.allocations, 2);
        assert_eq!(allocation.bytes as usize, prepared.allocation_bytes());
        assert_eq!(allocation.peak, prepared.allocation_bytes());
        assert_eq!(
            context.memory().reserved_bytes(),
            baseline + prepared.reserved_bytes()
        );
        assert!(context.memory().peak_reserved_bytes() >= baseline + prepared.reserved_bytes());
        assert_eq!(prepared.represented_bytes(), 70000);
        let allocation_bytes = prepared.allocation_bytes();
        let ((mut result, released), delivery) = super::audit::run(0, true, || {
            let mut result = prepared.expose(SuccessfulOutcome::Read);
            assert_eq!(unsafe { *result.pool.bytes.add(69999) }, 17);
            let released = REGISTRY.free(&mut result);
            (result, released)
        });
        assert_eq!(released.unwrap().released_bytes, allocation_bytes);
        assert_eq!(delivery.attempts, 0);
        assert_eq!(delivery.frees, 2);
        assert_eq!(delivery.bytes, -(allocation_bytes as isize));
        assert_eq!(context.memory().reserved_bytes(), baseline);
        REGISTRY.free(&mut result).unwrap();
        // Exact heap-flat loops include private abort as well as public free.
        let ((), loops) = super::audit::run(0, false, || {
            for n in 0..64 {
                let prepared = REGISTRY
                    .prepare(context, parts, ResponseMetadata::new(0, None))
                    .unwrap();
                if n % 2 == 0 {
                    drop(prepared);
                } else {
                    let mut result = prepared.expose(SuccessfulOutcome::Read);
                    REGISTRY.free(&mut result).unwrap();
                }
            }
        });
        assert_eq!((loops.allocations, loops.frees, loops.bytes), (128, 128, 0));
        assert_eq!(context.memory().reserved_bytes(), baseline);
        assert_eq!(source.as_slice().len(), 70000);
    });
}

#[test]
fn graph_result_every_authoritative_field_mutation_and_foreign_geometry_is_rejected() {
    static REGISTRY: GraphResultRegistry = GraphResultRegistry::new(1);
    with_context(|context| {
        let mut result = REGISTRY
            .prepare(
                context,
                ResponseParts {
                    bytes: b"owned",
                    ..ResponseParts::default()
                },
                ResponseMetadata::new(0, Some(9)),
            )
            .unwrap()
            .expose(SuccessfulOutcome::Committed(
                std::num::NonZeroU64::new(10).unwrap(),
            ));
        let mut mutations = 0;
        macro_rules! reject { ($($field:ident).+) => {{
            let mut forged = result;
            forged.$($field).+ = forged.$($field).+.wrapping_add(1);
            assert!(matches!(REGISTRY.free(&mut forged), Err(OwnerError::InvalidOwner)), stringify!($($field).+));
            mutations += 1;
        }}; }
        reject!(abi_size);
        reject!(abi_reserved);
        reject!(owner_token);
        reject!(disposition);
        reject!(has_admitted_generation);
        reject!(admitted_generation);
        reject!(has_changed_generation);
        reject!(reserved);
        reject!(changed_generation);
        reject!(row_count);
        reject!(column_count);
        reject!(cell_count);
        reject!(receipt_count);
        reject!(report_count);
        reject!(diagnostic_count);
        reject!(work_count);
        reject!(global_work.start);
        reject!(global_work.count);
        reject!(pool.abi_size);
        reject!(pool.abi_reserved);
        reject!(pool.value_count);
        reject!(pool.child_count);
        reject!(pool.byte_count);
        reject!(pool.node_count);
        reject!(pool.relationship_count);
        reject!(pool.property_count);
        reject!(pool.name_count);
        reject!(pool.vector_count);
        macro_rules! reject_pointer { ($($field:ident).+) => {{
            let mut forged = result;
            forged.$($field).+ = forged.$($field).+.wrapping_byte_add(1);
            assert!(matches!(REGISTRY.free(&mut forged), Err(OwnerError::InvalidOwner)), stringify!($($field).+));
            mutations += 1;
        }}; }
        reject_pointer!(columns);
        reject_pointer!(cells);
        reject_pointer!(receipts);
        reject_pointer!(reports);
        reject_pointer!(diagnostics);
        reject_pointer!(work);
        reject_pointer!(pool.values);
        reject_pointer!(pool.children);
        reject_pointer!(pool.bytes);
        reject_pointer!(pool.nodes);
        reject_pointer!(pool.relationships);
        reject_pointer!(pool.properties);
        reject_pointer!(pool.names);
        reject_pointer!(pool.vectors);
        assert_eq!(mutations, 42);
        REGISTRY.free(&mut result).unwrap();
    });
}

struct SendRoot(ZeGraphResponse);
// The raw descriptor is copied only for safe registry comparisons, never for
// dereferencing in competing threads. The registry exclusively owns backing.
unsafe impl Send for SendRoot {}
impl SendRoot {
    fn free(mut self, registry: &GraphResultRegistry) -> Result<FreeReport, OwnerError> {
        registry.free(&mut self.0)
    }
}
#[test]
fn graph_result_concurrent_free_publication_and_abort_have_one_owner() {
    use std::sync::{Arc, Barrier};
    static REGISTRY: GraphResultRegistry = GraphResultRegistry::new(4);
    with_context(|context| {
        for _ in 0..32 {
            let prepared = REGISTRY
                .prepare(
                    context,
                    ResponseParts {
                        bytes: b"alive",
                        ..ResponseParts::default()
                    },
                    ResponseMetadata::new(0, None),
                )
                .unwrap();
            let copy = SendRoot(prepared.descriptor());
            let barrier = Arc::new(Barrier::new(2));
            let worker_barrier = Arc::clone(&barrier);
            let worker = std::thread::spawn(move || {
                worker_barrier.wait();
                let mut root = copy;
                loop {
                    match REGISTRY.free(&mut root.0) {
                        Ok(report) => break report,
                        Err(OwnerError::InvalidOwner | OwnerError::Busy) => {
                            std::thread::yield_now()
                        }
                        other => panic!("unexpected publication/free result: {other:?}"),
                    }
                }
            });
            barrier.wait();
            let mut stale = prepared.expose(SuccessfulOutcome::Read);
            assert_eq!(worker.join().unwrap().examined_entries, 1);
            assert!(matches!(
                REGISTRY.free(&mut stale),
                Err(OwnerError::InvalidOwner)
            ));
        }
        let prepared = REGISTRY
            .prepare(
                context,
                ResponseParts {
                    bytes: b"abort",
                    ..ResponseParts::default()
                },
                ResponseMetadata::new(0, None),
            )
            .unwrap();
        let copy = SendRoot(prepared.descriptor());
        let worker = std::thread::spawn(move || copy.free(&REGISTRY));
        drop(prepared);
        assert!(matches!(
            worker.join().unwrap(),
            Err(OwnerError::InvalidOwner | OwnerError::Busy)
        ));
        let result = REGISTRY
            .prepare(
                context,
                ResponseParts::default(),
                ResponseMetadata::new(0, None),
            )
            .unwrap()
            .expose(SuccessfulOutcome::Read);
        let first = SendRoot(result);
        let second = SendRoot(result);
        let one = std::thread::spawn(move || first.free(&REGISTRY));
        let two = std::thread::spawn(move || second.free(&REGISTRY));
        let outcomes = [one.join().unwrap(), two.join().unwrap()];
        assert_eq!(outcomes.iter().filter(|value| value.is_ok()).count(), 1);
        assert_eq!(
            outcomes
                .iter()
                .filter(|value| matches!(value, Err(OwnerError::InvalidOwner | OwnerError::Busy)))
                .count(),
            1
        );
    });
}

#[test]
fn graph_result_free_rejects_returned_root_inside_authoritative_arena() {
    static REGISTRY: GraphResultRegistry = GraphResultRegistry::new(1);
    with_context(|context| {
        let bytes = [0_u8; 400];
        let mut result = REGISTRY
            .prepare(
                context,
                ResponseParts {
                    bytes: &bytes,
                    ..ResponseParts::default()
                },
                ResponseMetadata::new(0, None),
            )
            .unwrap()
            .expose(SuccessfulOutcome::Read);
        // A hostile C caller may put a copied root in the returned allocation.
        // Reject before freeing it; otherwise zeroing the caller root is UAF.
        let aliased = result.pool.bytes.cast_mut().cast::<ZeGraphResponse>();
        unsafe {
            aliased.write(result);
        }
        let rejected = unsafe { REGISTRY.free(&mut *aliased) };
        assert!(matches!(rejected, Err(OwnerError::InvalidOwner)));
        REGISTRY.free(&mut result).unwrap();
    });
}

#[test]
fn graph_result_seeded_cancel_sites_can_fire_and_same_seed_control_finishes() {
    use std::cell::Cell;
    use zeppelin_embed::property_graph::query::resources::QueryArena;
    use zeppelin_embed::property_graph::query::runtime::WorkKind;
    static REGISTRY: GraphResultRegistry = GraphResultRegistry::new(1);
    // A deterministic site script, not a randomized generator. Every site is
    // exercised; ZE_TEST_SEED selects the repeatable actual source byte count.
    let seed = std::env::var("ZE_TEST_SEED")
        .unwrap_or_else(|_| "128".into())
        .parse::<usize>()
        .unwrap();
    let length = 70000 + seed % 1024;
    eprintln!("ZE_TEST_SEED={seed} source_bytes={length} cancel_sites=1..=5 control=0");
    struct ControlledView {
        view: View,
        polls: Cell<usize>,
        site: Cell<usize>,
        token: CancelToken,
    }
    impl RetainedView for ControlledView {
        fn query_view(&self) -> &QueryView {
            self.view.query_view()
        }
        fn check_active(&self) -> Result<(), QueryError> {
            self.view.check_active()?;
            let poll = self.polls.get() + 1;
            self.polls.set(poll);
            if poll == self.site.get() {
                self.token.cancel();
            }
            Ok(())
        }
    }
    for site in [1, 2, 3, 4, 5, 0] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .unwrap();
        let resources = GraphResources::from_store(&store).unwrap();
        let memory = QueryMemory::new(&resources, 24 * 1024 * 1024).unwrap();
        let token = CancelToken::new();
        let view = ControlledView {
            view: View {
                token: QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0)),
                lease: store.snapshot().unwrap(),
            },
            polls: Cell::new(0),
            site: Cell::new(0),
            token: token.clone(),
        };
        let control = QueryControl::Cancel(token);
        let mut context =
            RuntimeContext::new(&view, &control, &memory, RuntimeLimits::default()).unwrap();
        let mut source = QueryArena::<u8>::new(&memory, length).unwrap();
        for _ in 0..length {
            source.push(7).unwrap();
        }
        let baseline = memory.reserved_bytes();
        view.polls.set(0);
        view.site.set(site);
        let (result, audit) = super::audit::run(0, false, || {
            REGISTRY.prepare(
                &mut context,
                ResponseParts {
                    bytes: source.as_slice(),
                    ..ResponseParts::default()
                },
                ResponseMetadata::new(0, None),
            )
        });
        if site == 0 {
            let prepared = result.unwrap();
            assert_eq!(prepared.represented_bytes(), length);
            let mut root = prepared.expose(SuccessfulOutcome::Read);
            assert_eq!(unsafe { *root.pool.bytes.add(length - 1) }, 7);
            REGISTRY.free(&mut root).unwrap();
            assert_eq!(audit.allocations, 2);
        } else {
            assert!(matches!(
                result,
                Err(OwnerError::Runtime(RuntimeError::Value(
                    QueryError::Cancelled
                )))
            ));
            assert_eq!(audit.bytes, 0);
            assert_eq!(audit.allocations, audit.frees);
            assert_eq!(view.polls.get(), site);
        }
        let copied = match site {
            1 | 2 => 0,
            3 => 65536,
            _ => length,
        };
        assert_eq!(context.counters().get(WorkKind::CopiedBytes), copied as u64);
        assert_eq!(
            context.counters().get(WorkKind::CompletedAbiBytes),
            0,
            "coordinator alone owns completed accounting"
        );
        assert_eq!(memory.reserved_bytes(), baseline);
    }
}

#[test]
fn graph_result_owned_pools_outlive_temporary_owners_and_lifecycle_fixture() {
    static REGISTRY: GraphResultRegistry = GraphResultRegistry::new(1);
    let mut result = with_context(|context| {
        let mut node: ZeGraphNode = unsafe { std::mem::zeroed() };
        node.abi_size = std::mem::size_of::<ZeGraphNode>() as u32;
        node.id = ZeNodeId { high: 11, low: 42 };
        node.revision = 3;
        node.last_change_generation = 8;
        let mut value: ZeGraphValue = unsafe { std::mem::zeroed() };
        value.abi_size = std::mem::size_of::<ZeGraphValue>() as u32;
        value.tag = ZeGraphValueTag::ZeGraphValueNode as u32;
        REGISTRY
            .prepare(
                context,
                ResponseParts {
                    nodes: &[node],
                    values: &[value],
                    bytes: b"a\0owned",
                    ..ResponseParts::default()
                },
                ResponseMetadata::new(0, Some(9)),
            )
            .unwrap()
            .expose(SuccessfulOutcome::Read)
    });
    // Store fixture, actual lease, runtime, source and memory owners all dropped.
    // This is component independence, not ZE-68 real graph-close acceptance.
    assert_eq!(
        unsafe { (*result.pool.nodes).id },
        ZeNodeId { high: 11, low: 42 }
    );
    assert_eq!(unsafe { (*result.pool.nodes).revision }, 3);
    assert_eq!(
        unsafe { (*result.pool.values).tag },
        ZeGraphValueTag::ZeGraphValueNode as u32
    );
    assert_eq!(
        unsafe { std::slice::from_raw_parts(result.pool.bytes, result.pool.byte_count) },
        b"a\0owned"
    );
    REGISTRY.free(&mut result).unwrap();
}

#[test]
fn graph_result_shape_capacity_work_and_registry_lookup_bounds_are_real() {
    static REGISTRY: GraphResultRegistry = GraphResultRegistry::new(3);
    let invalid = [
        ResponseMetadata::new(65537, None),
        ResponseMetadata::new(1, None),
        ResponseMetadata {
            row_count: 0,
            admitted_generation: None,
            global_work: ZeGraphRange {
                start: u32::MAX,
                count: 1,
            },
        },
    ];
    with_context(|context| {
        let column = [unsafe { std::mem::zeroed::<ZeGraphColumn>() }];
        for metadata in invalid {
            let (result, audit) = super::audit::run(0, true, || {
                REGISTRY.prepare(
                    context,
                    ResponseParts {
                        columns: &column,
                        ..ResponseParts::default()
                    },
                    metadata,
                )
            });
            assert!(matches!(result, Err(OwnerError::InvalidShape)));
            assert_eq!(audit.attempts, 0);
        }
        let mut first = REGISTRY
            .prepare(
                context,
                ResponseParts::default(),
                ResponseMetadata::new(0, None),
            )
            .unwrap()
            .expose(SuccessfulOutcome::Read);
        let mut second = REGISTRY
            .prepare(
                context,
                ResponseParts::default(),
                ResponseMetadata::new(0, None),
            )
            .unwrap()
            .expose(SuccessfulOutcome::Read);
        let third = REGISTRY
            .prepare(
                context,
                ResponseParts::default(),
                ResponseMetadata::new(0, None),
            )
            .unwrap();
        let (result, audit) = super::audit::run(0, true, || {
            REGISTRY.prepare(
                context,
                ResponseParts::default(),
                ResponseMetadata::new(0, None),
            )
        });
        assert!(matches!(result, Err(OwnerError::RegistryFull)));
        assert_eq!(audit.attempts, 0);
        assert_eq!(REGISTRY.free(&mut first).unwrap().examined_entries, 3);
        drop(third);
        assert_eq!(REGISTRY.free(&mut second).unwrap().examined_entries, 1);
        let mut forged_empty = empty_response();
        forged_empty.pool.byte_count = 1;
        assert!(matches!(
            REGISTRY.free(&mut forged_empty),
            Err(OwnerError::InvalidOwner)
        ));
    });
    with_limits(8192, RuntimeLimits::default(), |context| {
        let baseline = context.memory().reserved_bytes();
        let (result, audit) = super::audit::run(0, true, || {
            REGISTRY.prepare(
                context,
                ResponseParts {
                    bytes: &[0; 8192],
                    ..ResponseParts::default()
                },
                ResponseMetadata::new(0, None),
            )
        });
        assert!(matches!(
            result,
            Err(OwnerError::Memory(MemoryError::Limit))
        ));
        assert_eq!(audit.attempts, 0);
        assert_eq!(context.memory().reserved_bytes(), baseline);
    });
    let limits = RuntimeLimits::default()
        .with_limit(WorkKind::CopiedBytes, 0)
        .unwrap();
    with_limits(8192, limits, |context| {
        let baseline = context.memory().reserved_bytes();
        let (result, audit) = super::audit::run(0, false, || {
            REGISTRY.prepare(
                context,
                ResponseParts {
                    bytes: b"a",
                    ..ResponseParts::default()
                },
                ResponseMetadata::new(0, None),
            )
        });
        assert!(matches!(
            result,
            Err(OwnerError::Runtime(RuntimeError::Limit(
                WorkKind::CopiedBytes
            )))
        ));
        assert_eq!((audit.allocations, audit.frees, audit.bytes), (1, 1, 0));
        assert_eq!(context.memory().reserved_bytes(), baseline);
        assert_eq!(context.counters().get(WorkKind::CopiedBytes), 0);
    });
}
