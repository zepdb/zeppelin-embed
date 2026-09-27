#![allow(clippy::expect_used)]
use std::sync::Arc;
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision,
};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode};
use zeppelin_embed::lifecycle::{OpenOptions, Store, StoreTestDependencies, SystemMonotonicClock};
use zeppelin_embed::vfs::{CountingVfs, StdVfs};
fn counts(v: &CountingVfs<StdVfs>) -> [u64; 10] {
    [
        v.open_calls(),
        v.read_calls(),
        v.write_calls(),
        v.bytes_written(),
        v.append_calls(),
        v.bytes_appended(),
        v.rename_calls(),
        v.full_sync_calls(),
        v.handle_full_sync_calls(),
        v.delete_calls(),
    ]
}
#[test]
fn single_namespace_write_io_is_unchanged() {
    let directory = tempfile::tempdir().expect("directory");
    let vfs = Arc::new(CountingVfs::new(StdVfs));
    let store = Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::new().with_durability(DurabilityMode::Durable, CommitTier::Durable),
        StoreTestDependencies::new(vfs.clone(), Arc::new(SystemMonotonicClock)),
    )
    .expect("open");
    let before = counts(&vfs);
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(1), Revision::new(1)),
            vec![1.0, 2.0],
        )]))
        .expect("ingest");
    store
        .delete(DeleteBatch::new(vec![DocId::new(1)]))
        .expect("delete");
    let after = counts(&vfs);
    let delta: Vec<_> = after.into_iter().zip(before).map(|(a, b)| a - b).collect();
    eprintln!("ZE239 single namespace write counters: {delta:?}");
    assert_eq!(delta, vec![0, 0, 0, 0, 2, 148, 0, 1, 2, 0]);
}
