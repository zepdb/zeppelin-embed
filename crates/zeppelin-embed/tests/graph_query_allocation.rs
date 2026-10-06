#![cfg(feature = "allocation-audit")]
#![allow(clippy::expect_used, clippy::panic)]
use zeppelin_embed::adversarial_test_support::{audit_engine_path, fail_attributed_allocation};
use zeppelin_embed::lifecycle::{OpenOptions, Store};
use zeppelin_embed::property_graph::query::resources::*;
use zeppelin_embed::property_graph::resources::GraphResources;

#[test]
fn actual_allocator_failure_releases_new_capacity_and_preserves_old_arena() {
    let root = tempfile::tempdir().expect("fixture");
    let store = Store::open(
        root.path(),
        // Graph registry backing exceeds the old 64 KiB store fixture.
        // The 8 KiB query allowance below remains the allocation-fault subject.
        OpenOptions::new().with_max_resident_bytes(4 * 1024 * 1024),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("shared");
    let memory = QueryMemory::new(&shared, 8192).expect("query");
    let baseline = memory.reserved_bytes();
    let ((result, fires), audit) =
        audit_engine_path(|| fail_attributed_allocation(1, || QueryArena::<u64>::new(&memory, 16)));
    assert_eq!(fires, 1, "the actual System allocator call was refused");
    assert!(matches!(result, Err(MemoryError::Allocation)));
    assert_eq!(audit.allocations, 0);
    assert_eq!(memory.reserved_bytes(), baseline);
    let mut arena = QueryArena::new(&memory, 16).expect("clean same-size control");
    arena.push(41u64).expect("initial value");
    let before = memory.reserved_bytes();
    let (result, fires) = fail_attributed_allocation(1, || QueryArena::<u64>::new(&memory, 32));
    assert_eq!(fires, 1);
    assert!(matches!(result, Err(MemoryError::Allocation)));
    assert_eq!(arena.as_slice(), &[41]);
    assert_eq!(arena.capacity(), 16);
    assert_eq!(memory.reserved_bytes(), before);
    let (result, audit) = audit_engine_path(|| QueryArena::<u64>::new(&memory, 32));
    let replacement = result.expect("same-size clean replacement allocation");
    assert_eq!(audit.allocations, 1);
    assert_eq!(audit.attributed_bytes, 256);
    assert_eq!(audit.unattributed_bytes, 0);
    assert_eq!(arena.as_slice(), &[41]);
    drop(arena);
    drop(replacement);
    assert_eq!(memory.reserved_bytes(), baseline);
    // Prove attribution audit sensitivity on an actual unaccounted allocation.
    let (unaccounted, audit) = audit_engine_path(|| Vec::<u8>::with_capacity(37));
    assert_eq!(audit.unattributed_bytes, 37);
    drop(unaccounted);
    store.close().expect("close");
}
