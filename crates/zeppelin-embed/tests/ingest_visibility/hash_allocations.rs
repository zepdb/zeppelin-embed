//! Observe actual std table allocations on the calling test thread.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use zeppelin_embed::fts::index::TermKey;
use zeppelin_embed::ingest::DocId;

#[path = "../../src/lifecycle/hash_table.rs"]
mod hash_table;

thread_local! {
    static RECORD: Cell<bool> = const { Cell::new(false) };
    static BYTES: Cell<usize> = const { Cell::new(0) };
    static TARGET: Cell<usize> = const { Cell::new(0) };
    static TARGET_HITS: Cell<usize> = const { Cell::new(0) };
}
struct ObservedSystem;
#[global_allocator]
static ALLOCATOR: ObservedSystem = ObservedSystem;
unsafe impl GlobalAlloc for ObservedSystem {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if RECORD.try_with(Cell::get).unwrap_or(false) {
            BYTES.with(|bytes| bytes.set(bytes.get() + layout.size()));
            if TARGET.with(Cell::get) == layout.size() {
                TARGET_HITS.with(|hits| hits.set(hits.get() + 1));
            }
        }
        // SAFETY: forward the caller's valid layout to System unchanged.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: forward the live pointer and its original layout unchanged.
        unsafe { System.dealloc(pointer, layout) }
    }
}
fn measured<T>(run: impl FnOnce() -> T) -> (T, usize) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            RECORD.with(|record| record.set(false));
        }
    }
    BYTES.with(|bytes| bytes.set(0));
    RECORD.with(|record| record.set(true));
    let reset = Reset;
    let result = run();
    drop(reset);
    (result, BYTES.with(Cell::get))
}

#[test]
fn hash_table_helper_matches_system_allocation() {
    for capacity in [0, 1, 3, 4, 7, 8, 14, 15, 28, 29, 56, 57, 112, 113, 1024] {
        let mut ids = HashMap::<DocId, usize>::new();
        let (_, bytes) = measured(|| ids.try_reserve(capacity).expect("id reserve"));
        assert_eq!(
            hash_table::allocation_bytes::<(DocId, usize)>(capacity),
            Some(bytes),
            "id capacity {capacity}"
        );
        let mut terms = HashMap::<TermKey, usize>::new();
        let (_, bytes) = measured(|| terms.try_reserve(capacity).expect("term reserve"));
        assert_eq!(
            hash_table::allocation_bytes::<(TermKey, usize)>(capacity),
            Some(bytes),
            "term capacity {capacity}"
        );
        let mut seen = HashSet::<DocId>::new();
        let (_, bytes) = measured(|| seen.try_reserve(capacity).expect("set reserve"));
        assert_eq!(
            hash_table::allocation_bytes::<DocId>(capacity),
            Some(bytes),
            "set capacity {capacity}"
        );
    }
    assert_eq!(hash_table::allocation_bytes::<DocId>(usize::MAX), None);
}

#[test]
fn denied_hash_tables_are_not_allocated() {
    use zeppelin_embed::ingest::{
        DocumentVersion, IngestBatch, IngestDocument, IngestError, Revision,
    };
    use zeppelin_embed::lifecycle::{OpenOptions, Store, StoreError};

    let id_bytes = hash_table::allocation_bytes::<(DocId, usize)>(1).expect("id bytes");
    let term_bytes = hash_table::allocation_bytes::<(TermKey, usize)>(1).expect("term bytes");
    // One complete row including text and the ID map, but no lexical table.
    let row_bytes = std::mem::size_of::<DocId>()
        + 4 * 8
        + 4
        + 1
        + std::mem::size_of::<zeppelin_embed::quant::Bit4Factors>()
        + 1
        + 8
        + 5;
    for (table, target, allowance) in [
        ("ids", id_bytes, std::mem::size_of::<DocId>() + id_bytes - 1),
        ("terms", term_bytes, row_bytes + id_bytes),
    ] {
        let directory = tempfile::tempdir().expect("directory");
        let probe = Store::open(directory.path(), OpenOptions::default()).expect("probe");
        let resident = probe.stats().expect("stats").resident_owned_bytes;
        probe.close().expect("close probe");
        let store = Store::open(
            directory.path(),
            OpenOptions::default().with_max_resident_bytes(resident + allowance as u64),
        )
        .expect("bounded store");
        let before = store.stats().expect("before");
        let wal_before = std::fs::read(directory.path().join("wal.ze")).expect("wal");
        let batch = IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(1), Revision::new(1)),
                vec![1.0],
            )
            .with_text("alpha"),
        ]);
        TARGET.with(|value| value.set(target));
        TARGET_HITS.with(|hits| hits.set(0));
        let (result, _) = measured(|| store.ingest(batch));
        assert!(
            matches!(
                result,
                Err(IngestError::Store(StoreError::BudgetExceeded { .. }))
            ),
            "{table}: {result:?}"
        );
        assert_eq!(
            TARGET_HITS.with(Cell::get),
            0,
            "{table} allocated before admission"
        );
        assert_eq!(
            store.stats().expect("after").resident_owned_bytes,
            before.resident_owned_bytes
        );
        assert_eq!(
            std::fs::read(directory.path().join("wal.ze")).expect("wal after"),
            wal_before
        );
        store.close().expect("close");
    }
}
