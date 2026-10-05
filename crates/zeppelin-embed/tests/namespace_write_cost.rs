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

#[test]
fn previously_batched_writer_keeps_the_single_namespace_baseline() {
    use zeppelin_embed::lifecycle::{
        LiveNamespaceMutation, NamespaceMutation, namespace_batch_live,
    };
    let directory = tempfile::tempdir().expect("root");
    let vfs = Arc::new(CountingVfs::new(StdVfs));
    let options = OpenOptions::new().with_durability(DurabilityMode::Durable, CommitTier::Durable);
    let a = Store::open_with_test_dependencies(
        directory.path().join("a"),
        options.clone(),
        StoreTestDependencies::new(vfs.clone(), Arc::new(SystemMonotonicClock)),
    )
    .expect("a");
    let b = Store::open(directory.path().join("b"), options.clone()).expect("b");
    namespace_batch_live(
        directory.path(),
        [(&a, "a"), (&b, "b")]
            .into_iter()
            .map(|(store, name)| LiveNamespaceMutation {
                store,
                mutation: NamespaceMutation {
                    name: name.into(),
                    options: options.clone(),
                    upserts: vec![IngestDocument::new(
                        DocumentVersion::new(DocId::new(2), Revision::new(1)),
                        vec![1.0, 2.0],
                    )],
                    deletes: vec![],
                    delete_where: None,
                },
            })
            .collect(),
    )
    .expect("batch");
    let control_vfs = Arc::new(CountingVfs::new(StdVfs));
    let control = Store::open_with_test_dependencies(
        directory.path().join("control"),
        options,
        StoreTestDependencies::new(control_vfs.clone(), Arc::new(SystemMonotonicClock)),
    )
    .expect("unenlisted control");
    control
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(2), Revision::new(1)),
            vec![1.0, 2.0],
        )]))
        .expect("same initialized WAL");
    let control_before = counts(&control_vfs);
    control
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(1), Revision::new(1)),
            vec![1.0, 2.0],
        )]))
        .expect("control ingest");
    control
        .delete(DeleteBatch::new(vec![DocId::new(1)]))
        .expect("control delete");
    let control_delta: Vec<_> = counts(&control_vfs)
        .into_iter()
        .zip(control_before)
        .map(|(a, b)| a - b)
        .collect();
    assert_eq!(control_delta, vec![0, 0, 0, 0, 2, 108, 0, 0, 2, 0]);
    let before = counts(&vfs);
    a.ingest(IngestBatch::new(vec![IngestDocument::new(
        DocumentVersion::new(DocId::new(1), Revision::new(1)),
        vec![1.0, 2.0],
    )]))
    .expect("ingest");
    a.delete(DeleteBatch::new(vec![DocId::new(1)]))
        .expect("delete");
    let delta: Vec<_> = counts(&vfs)
        .into_iter()
        .zip(before)
        .map(|(a, b)| a - b)
        .collect();
    eprintln!("ZE256 previously batched write counters: {delta:?}");
    assert_eq!(delta, control_delta);
}
