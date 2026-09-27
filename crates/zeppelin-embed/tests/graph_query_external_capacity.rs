#![allow(clippy::expect_used)]
use zeppelin_embed::lifecycle::{OpenOptions, Store};
use zeppelin_embed::property_graph::{
    query::resources::{QueryArena, QueryExternalReservation, QueryMemory},
    resources::GraphResources,
};
#[test]
fn external_compiler_capacity_shares_query_and_store_budget_and_releases_exactly() {
    let directory = tempfile::tempdir().expect("fixture");
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(2 * 1024 * 1024),
    )
    .expect("store");
    let shared = GraphResources::from_store(&store).expect("shared owner");
    let initial = shared.reserved_bytes().expect("initial");
    {
        let memory = QueryMemory::new(&shared, 1024).expect("query");
        let base = memory.reserved_bytes();
        let mut guard = memory.reserve_external_capacity().expect("frontend guard");
        assert_eq!(guard.bytes(), 0);
        guard.reserve_additional(64).expect("before allocation");
        let retained = base + std::mem::size_of::<QueryExternalReservation<'_, '_>>() + 64;
        assert_eq!(memory.reserved_bytes(), retained);
        assert_eq!(
            shared.reserved_bytes().expect("shared"),
            initial + retained as u64
        );
        assert!(guard.reserve_additional(1024).is_err());
        assert_eq!(guard.bytes(), 64);
        assert_eq!(memory.reserved_bytes(), retained);
        let arena = QueryArena::<u8>::new(&memory, 16).expect("separate real IR copy");
        assert_eq!(memory.reserved_bytes(), retained + arena.reserved_bytes());
        drop(arena);
        drop(guard);
        assert_eq!(memory.reserved_bytes(), base);
    }
    assert_eq!(shared.reserved_bytes().expect("release"), initial);
    store.close().expect("close");
}
