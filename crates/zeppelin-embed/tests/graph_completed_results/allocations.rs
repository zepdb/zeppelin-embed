//! Actual System allocator observation in isolated nextest test processes.
//! Disabled only when the core's allocation-audit feature owns the global hook.
use super::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    static ATTEMPTS: Cell<usize> = const { Cell::new(0) };
    static DENY: Cell<usize> = const { Cell::new(0) };
    static FIRES: Cell<usize> = const { Cell::new(0) };
    static LIVE: Cell<isize> = const { Cell::new(0) };
    static MEMORY: Cell<usize> = const { Cell::new(0) };
    static BASE: Cell<usize> = const { Cell::new(0) };
    static EARLY_RELEASE: Cell<bool> = const { Cell::new(false) };
}
struct ObservedSystem;
#[global_allocator]
static ALLOCATOR: ObservedSystem = ObservedSystem;
fn active() -> bool {
    ACTIVE.try_with(Cell::get).unwrap_or(false)
}
fn refuse() -> bool {
    if !active() {
        return false;
    }
    let ordinal = ATTEMPTS.with(|v| {
        v.set(v.get() + 1);
        v.get()
    });
    if DENY.with(Cell::get) == ordinal {
        FIRES.with(|v| v.set(v.get() + 1));
        true
    } else {
        false
    }
}
unsafe impl GlobalAlloc for ObservedSystem {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if refuse() {
            return std::ptr::null_mut();
        }
        // SAFETY: delegates the allocator caller's valid Layout unchanged.
        let ptr = unsafe { System.alloc(layout) };
        if active() && !ptr.is_null() {
            LIVE.with(|v| v.set(v.get() + layout.size() as isize));
        }
        ptr
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if refuse() {
            return std::ptr::null_mut();
        }
        // SAFETY: delegates the allocator caller's valid Layout unchanged.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if active() && !ptr.is_null() {
            LIVE.with(|v| v.set(v.get() + layout.size() as isize));
        }
        ptr
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        if refuse() {
            return std::ptr::null_mut();
        }
        // SAFETY: delegates the valid live allocation and requested new extent.
        let out = unsafe { System.realloc(ptr, layout, new) };
        if active() && !out.is_null() {
            LIVE.with(|v| v.set(v.get() + new as isize - layout.size() as isize));
        }
        out
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if active() {
            LIVE.with(|v| v.set(v.get() - layout.size() as isize));
            let address = MEMORY.with(Cell::get);
            if address != 0 && layout.size() >= 65536 {
                // SAFETY: the scoped calling-thread test installs its live QueryMemory
                // address, never sends it to another thread, and clears it before the
                // enclosing memory owner is dropped. This only reads its Cell counter.
                let memory = unsafe { &*(address as *const QueryMemory<'_>) };
                if memory.reserved_bytes() < BASE.with(Cell::get) + layout.size() {
                    EARLY_RELEASE.with(|v| v.set(true));
                }
            }
        }
        // SAFETY: delegates the exact pointer/Layout supplied by the caller.
        unsafe { System.dealloc(ptr, layout) };
    }
}
#[derive(Debug)]
struct Audit {
    attempts: usize,
    fires: usize,
    live: isize,
    early_release: bool,
}
fn audit<T>(deny: usize, memory: Option<&QueryMemory<'_>>, run: impl FnOnce() -> T) -> (T, Audit) {
    assert!(!active());
    ATTEMPTS.with(|v| v.set(0));
    DENY.with(|v| v.set(deny));
    FIRES.with(|v| v.set(0));
    LIVE.with(|v| v.set(0));
    EARLY_RELEASE.with(|v| v.set(false));
    MEMORY.with(|v| v.set(memory.map_or(0, |m| m as *const _ as usize)));
    BASE.with(|v| v.set(memory.map_or(0, QueryMemory::reserved_bytes)));
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            ACTIVE.with(|v| v.set(false));
            MEMORY.with(|v| v.set(0));
        }
    }
    ACTIVE.with(|v| v.set(true));
    let reset = Reset;
    let result = run();
    drop(reset);
    (
        result,
        Audit {
            attempts: ATTEMPTS.with(Cell::get),
            fires: FIRES.with(Cell::get),
            live: LIVE.with(Cell::get),
            early_release: EARLY_RELEASE.with(Cell::get),
        },
    )
}

#[test]
fn every_actual_allocation_failure_releases_real_buffers_before_guards() {
    context_case(|context| {
        let bytes = vec![b'x'; 131072];
        let values = [Value::String(Span::new(0, bytes.len() as u32))];
        let columns = [Column {
            name: Span::new(0, 1),
            kinds: ValueKinds::STRING,
        }];
        let cells = [ValueIndex(0)];
        let source = Source(ResultInput {
            view: context.view(),
            rows: 1,
            outcome: Outcome::Read,
            pools: Pools {
                bytes: &bytes,
                values: &values,
                columns: &columns,
                cells: &cells,
                ..Pools::default()
            },
        });
        let memory = context.memory();
        let baseline = memory.reserved_bytes();
        let (_, clean) = audit(0, Some(memory), || {
            drop(PreparedGraphResult::copy_from(&source, context).unwrap());
        });
        assert_eq!(
            clean.attempts, 5,
            "one validation scratch allocation and four real output pools"
        );
        assert_eq!(clean.live, 0);
        assert!(!clean.early_release);
        for ordinal in 1..=clean.attempts {
            let (failed, evidence) = audit(ordinal, Some(memory), || {
                PreparedGraphResult::copy_from(&source, context).map(drop)
            });
            assert!(matches!(
                failed,
                Err(CompletedError::Runtime(
                    zeppelin_embed::property_graph::query::runtime::RuntimeError::Memory(
                        zeppelin_embed::property_graph::query::resources::MemoryError::Allocation
                    )
                ))
            ));
            assert_eq!(evidence.fires, 1);
            assert_eq!(evidence.live, 0);
            assert!(!evidence.early_release);
            assert_eq!(memory.reserved_bytes(), baseline);
        }
        for _ in 0..32 {
            let (_, clean) = audit(0, Some(memory), || {
                drop(PreparedGraphResult::copy_from(&source, context).unwrap())
            });
            assert_eq!(clean.live, 0);
            assert!(!clean.early_release);
        }
    });
}

#[test]
fn consuming_detach_is_allocator_denied_and_preserves_every_live_pointer() {
    context_case(|context| {
        let values = [Value::String(Span::new(0, 3))];
        let columns = [Column {
            name: Span::new(0, 0),
            kinds: ValueKinds::STRING,
        }];
        let cells = [ValueIndex(0)];
        let source = Source(ResultInput {
            view: context.view(),
            rows: 1,
            outcome: Outcome::Read,
            pools: Pools {
                bytes: b"abc",
                values: &values,
                columns: &columns,
                cells: &cells,
                ..Pools::default()
            },
        });
        let baseline = context.memory().reserved_bytes();
        let prepared = PreparedGraphResult::copy_from(&source, context).unwrap();
        let before = prepared.pools();
        let pointers = (
            before.bytes.as_ptr(),
            before.values.as_ptr(),
            before.columns.as_ptr(),
            before.cells.as_ptr(),
        );
        let (result, evidence) = audit(1, None, || {
            prepared.detach(context.counters(), context.memory().peak_reserved_bytes())
        });
        assert_eq!(evidence.attempts, 0);
        assert_eq!(evidence.fires, 0);
        let after = result.pools();
        assert_eq!(
            pointers,
            (
                after.bytes.as_ptr(),
                after.values.as_ptr(),
                after.columns.as_ptr(),
                after.cells.as_ptr()
            )
        );
        assert_eq!(context.memory().reserved_bytes(), baseline);
        assert_eq!(result.string(Span::new(0, 3)), Some("abc"));
        let (_, freed) = audit(1, None, || drop(result));
        assert_eq!(freed.attempts, 0);
        assert!(freed.live < 0, "application backing actually deallocated");
    });
}

#[test]
fn every_typed_pool_uses_real_fallible_allocation_and_exact_cleanup() {
    use zeppelin_embed::property_graph::staging::ItemReceipt;
    use zeppelin_embed::property_graph::{EntityId, GraphRevision, NodeId, RelId};
    context_case(|context| {
        let bytes = vec![b'x'; 131072];
        let values = [
            Value::Bool(true),
            Value::Node(0),
            Value::Relationship(0),
            Value::List {
                children: Span::new(0, 1),
                element: ListKind::Query,
            },
        ];
        let children = [ValueIndex(0)];
        let columns = [Column {
            name: Span::new(0, 1),
            kinds: ValueKinds::LIST,
        }];
        let cells = [ValueIndex(3)];
        let names = [Span::new(0, 1)];
        let properties = [Property {
            name: Span::new(0, 1),
            value: ValueIndex(0),
        }];
        let id = NodeId::new((1_u128 << 100) + 7).unwrap();
        let nodes = [Node {
            id,
            revision: GraphRevision::new(3).unwrap(),
            generation: GraphGeneration::new(7),
            key: None,
            labels: Span::new(0, 1),
            properties: Span::new(0, 1),
            text: Some(Span::new(0, 0)),
            vector: Some(Span::new(0, 1)),
        }];
        let relationships = [Relationship {
            id: RelId::new((1_u128 << 99) + 7).unwrap(),
            revision: GraphRevision::new(1).unwrap(),
            generation: GraphGeneration::new(7),
            key: None,
            source: id,
            target: id,
            relationship_type: Span::new(0, 1),
            properties: Span::new(0, 1),
        }];
        let vectors = [(-0.0_f32).to_bits()];
        let reports = [lexical_report()];
        let receipts = [Receipt {
            item_index: 0,
            deleted: false,
            receipt: ItemReceipt {
                entity: EntityId::Node(id),
                revision: GraphRevision::new(3).unwrap(),
                generation: GraphGeneration::new(8),
                replayed: false,
            },
        }];
        for outcome in [
            Outcome::Read,
            Outcome::Committed {
                changed: GraphGeneration::new(8),
            },
        ] {
            let read = outcome == Outcome::Read;
            let source = Source(ResultInput {
                view: context.view(),
                rows: 1,
                outcome,
                pools: Pools {
                    bytes: &bytes,
                    values: &values,
                    columns: &columns,
                    cells: &cells,
                    children: &children,
                    names: &names,
                    properties: &properties,
                    nodes: &nodes,
                    relationships: &relationships,
                    vectors: &vectors,
                    reports: if read { &reports } else { &[] },
                    receipts: if read { &[] } else { &receipts },
                },
            });
            let memory = context.memory();
            let baseline = memory.reserved_bytes();
            let (_, clean) = audit(0, Some(memory), || {
                drop(PreparedGraphResult::copy_from(&source, context).unwrap())
            });
            assert_eq!(
                clean.attempts, 12,
                "eleven nonempty output pools plus one actual validation scratch"
            );
            assert_eq!(clean.live, 0);
            assert!(!clean.early_release);
            for ordinal in 1..=clean.attempts {
                let (result, failed) = audit(ordinal, Some(memory), || {
                    PreparedGraphResult::copy_from(&source, context).map(drop)
                });
                assert!(result.is_err());
                assert_eq!(failed.fires, 1);
                assert_eq!(failed.live, 0);
                assert!(!failed.early_release);
                assert_eq!(memory.reserved_bytes(), baseline);
            }
            for _ in 0..8 {
                let (_, clean) = audit(0, None, || {
                    let owner = PreparedGraphResult::copy_from(&source, context).unwrap();
                    let result = owner.detach(context.counters(), memory.peak_reserved_bytes());
                    assert_eq!(result.pools().vectors, [0x8000_0000]);
                    drop(result);
                });
                assert_eq!(clean.live, 0);
                assert_eq!(memory.reserved_bytes(), baseline);
            }
            let owner = PreparedGraphResult::copy_from(&source, context).unwrap();
            let pointers = pool_pointers(owner.pools());
            let (result, detached) = audit(1, None, || {
                owner.detach(context.counters(), memory.peak_reserved_bytes())
            });
            assert_eq!(detached.attempts, 0);
            assert_eq!(pointers, pool_pointers(result.pools()));
            assert_eq!(memory.reserved_bytes(), baseline);
            let (_, freed) = audit(1, None, || drop(result));
            assert_eq!(freed.attempts, 0);
            assert!(freed.live < 0);
        }
    });
}

fn pool_pointers(pools: Pools<'_>) -> [usize; 12] {
    [
        pools.values.as_ptr() as usize,
        pools.bytes.as_ptr() as usize,
        pools.columns.as_ptr() as usize,
        pools.cells.as_ptr() as usize,
        pools.children.as_ptr() as usize,
        pools.names.as_ptr() as usize,
        pools.properties.as_ptr() as usize,
        pools.nodes.as_ptr() as usize,
        pools.relationships.as_ptr() as usize,
        pools.vectors.as_ptr() as usize,
        pools.reports.as_ptr() as usize,
        pools.receipts.as_ptr() as usize,
    ]
}
