#![allow(clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use tempfile::tempdir;
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, RetentionPolicy, Revision,
    SearchRequest,
};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use zeppelin_embed::scan::ScanOptions;
use zeppelin_embed::segment::ClusteringKeyRange;
use zeppelin_embed::vfs::{CountingVfs, StdVfs, SyncKind, Vfs, VfsFile};

fn seal_partition(store: &Store, first_doc: u128, timestamps: &[i64]) {
    let documents = timestamps
        .iter()
        .enumerate()
        .map(|(offset, timestamp)| {
            IngestDocument::new(
                DocumentVersion::new(DocId::new(first_doc + offset as u128), Revision::new(1)),
                vec![1.0, offset as f32],
            )
            .with_timestamp(*timestamp)
        })
        .collect();
    store
        .ingest(IngestBatch::new(documents))
        .expect("ingest partition");
    store.seal().expect("seal partition");
}

#[test]
fn drop_partition_unlinks_and_costs_o_manifest() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    seal_partition(&store, 100, &[0, 9]);
    seal_partition(&store, 200, &[10, 19]);
    seal_partition(&store, 300, &[20, 29]);
    let vfs = CountingVfs::new(StdVfs);
    vfs.audit_segment_data_reads(|| {
        let snapshot = store.snapshot().expect("counter proof snapshot");
        snapshot.segments()[0]
            .columns()
            .expect("counter proof reads real mmap payload");
    });
    assert!(
        vfs.segment_bytes_read() > 0,
        "actual SegmentReader payload access did not reach the counter"
    );
    vfs.reset();

    let report = store
        .drop_partition_on_vfs(0..20, &vfs)
        .expect("drop two complete partitions");

    assert_eq!(report.segments_dropped().len(), 2);
    assert!(report.bytes_reclaimed() > 0);
    assert!(report.straddlers_skipped().is_empty());
    assert_eq!(vfs.write_calls(), 1, "one manifest temporary write");
    assert_eq!(vfs.rename_calls(), 1, "one manifest commit rename");
    assert_eq!(vfs.delete_calls(), 2, "one unlink per dropped segment");
    assert_eq!(
        vfs.segment_bytes_read(),
        0,
        "partition selection must not read immutable segment bytes"
    );
}

#[test]
fn sealed_segments_carry_their_clustering_key_range() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");

    seal_partition(&store, 400, &[-7, 14, 3]);

    let snapshot = store.snapshot().expect("sealed snapshot");
    let segment = snapshot.segments().first().expect("sealed segment");
    assert_eq!(
        segment.meta().clustering_key_range,
        ClusteringKeyRange::Bounded {
            min_ts: -7,
            max_ts: 14,
        }
    );

    let empty_directory = tempdir().expect("zero-live store directory");
    let empty_store =
        Store::open(empty_directory.path(), OpenOptions::default()).expect("open zero-live store");
    let deleted_id = DocId::new(450);
    empty_store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(deleted_id, Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_timestamp(99),
        ]))
        .expect("ingest row to tombstone");
    empty_store
        .delete(DeleteBatch::new(vec![deleted_id]))
        .expect("tombstone only row");
    empty_store.seal().expect("seal zero-live segment");
    assert_eq!(
        empty_store
            .snapshot()
            .expect("zero-live snapshot")
            .segments()[0]
            .meta()
            .clustering_key_range,
        ClusteringKeyRange::Empty
    );
}

#[test]
fn drop_partition_skips_a_segment_that_straddles_the_range() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    seal_partition(&store, 500, &[0, 9]);
    let id = store.snapshot().expect("snapshot").segments()[0].meta().id;

    let report = store.drop_partition(0..5).expect("partial partition drop");

    assert!(report.segments_dropped().is_empty());
    assert_eq!(report.straddlers_skipped(), &[id]);
    assert!(directory.path().join(id.file_name()).exists());
}

#[test]
fn dropped_documents_are_not_returned_by_later_queries() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    seal_partition(&store, 600, &[0, 9]);

    store.drop_partition(0..10).expect("drop partition");
    let outcome = store
        .search(
            SearchRequest::new(&[1.0, 0.0]),
            10,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("query after drop");

    assert!(outcome.candidates.is_empty());
}

#[test]
fn a_query_running_during_a_drop_still_completes_and_sees_its_pinned_rows() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    seal_partition(&store, 700, &[0, 9]);
    let lease = store.snapshot().expect("query pins snapshot");
    let id = lease.segments()[0].meta().id;
    let (started_tx, started_rx) = mpsc::channel();
    let (continue_tx, continue_rx) = mpsc::channel();
    let query = std::thread::spawn(move || {
        started_tx.send(()).expect("signal pinned query");
        continue_rx.recv().expect("continue pinned query");
        let segment = lease.segments().first().expect("pinned segment");
        let vectors = segment.rescore_f32().expect("read pinned vectors");
        let document = segment
            .document_version(0)
            .expect("read pinned document version");
        (vectors.len(), document)
    });
    started_rx.recv().expect("query started");

    let report = store.drop_partition(0..10).expect("drop pinned partition");
    continue_tx.send(()).expect("release pinned query");
    let (vector_values, document) = query.join().expect("pinned query completes");

    assert_eq!(report.segments_dropped(), &[id]);
    assert!(!directory.path().join(id.file_name()).exists());
    assert_eq!(vector_values, 4);
    assert_eq!(
        document,
        Some(DocumentVersion::new(DocId::new(700), Revision::new(1)))
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DropEvent {
    ManifestCommit,
    Unlink,
}

#[derive(Clone, Default)]
struct EventVfs {
    events: Arc<Mutex<Vec<DropEvent>>>,
}

impl EventVfs {
    fn events(&self) -> Vec<DropEvent> {
        self.events.lock().expect("event mutex").clone()
    }
}

impl Vfs for EventVfs {
    fn open(&self, path: &Path) -> std::io::Result<u64> {
        StdVfs.open(path)
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        StdVfs.read(path)
    }

    fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        StdVfs.read_range(path, offset, length)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        StdVfs.write(path, bytes)
    }

    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        StdVfs.open_append(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        StdVfs.rename(from, to)?;
        if to.file_name().is_some_and(|name| name == "manifest.ze") {
            self.events
                .lock()
                .expect("event mutex")
                .push(DropEvent::ManifestCommit);
        }
        Ok(())
    }

    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        StdVfs.sync(path, kind)
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        StdVfs.list(directory)
    }

    fn delete(&self, path: &Path) -> std::io::Result<()> {
        StdVfs.delete(path)?;
        self.events
            .lock()
            .expect("event mutex")
            .push(DropEvent::Unlink);
        Ok(())
    }
}

#[test]
fn drop_partition_commits_the_manifest_before_unlinking() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    seal_partition(&store, 800, &[0, 9]);
    let vfs = EventVfs::default();

    let report = store
        .drop_partition_on_vfs(0..10, &vfs)
        .expect("drop partition");

    assert_eq!(report.segments_dropped().len(), 1);
    assert_eq!(
        vfs.events(),
        vec![DropEvent::ManifestCommit, DropEvent::Unlink]
    );
}

#[test]
fn retention_hook_drops_only_partitions_outside_the_window() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    seal_partition(&store, 900, &[0, 9]);
    seal_partition(&store, 1_000, &[15, 20]);
    seal_partition(&store, 1_100, &[21, 29]);
    let before = store.snapshot().expect("snapshot before retention");
    let old = before.segments()[0].meta().id;
    let crossing = before.segments()[1].meta().id;
    let retained = before.segments()[2].meta().id;
    drop(before);
    let policy = RetentionPolicy::new(10).expect("positive retention window");

    let report = store
        .apply_retention(policy, 30)
        .expect("explicit retention hook");

    assert_eq!(report.segments_dropped(), &[old]);
    assert_eq!(report.straddlers_skipped(), &[crossing]);
    let after = store.snapshot().expect("snapshot after retention");
    let remaining = after
        .segments()
        .iter()
        .map(|segment| segment.meta().id)
        .collect::<Vec<_>>();
    assert_eq!(remaining, vec![crossing, retained]);
}

#[test]
fn drop_partition_on_an_empty_range_is_a_no_op_and_reports_it() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    seal_partition(&store, 1_200, &[0, 9]);
    let generation = store
        .snapshot()
        .expect("snapshot before no-op")
        .generation();
    let vfs = CountingVfs::new(StdVfs);
    vfs.reset();

    let report = store
        .drop_partition_on_vfs(5..5, &vfs)
        .expect("empty range no-op");

    assert!(report.is_no_op());
    assert_eq!(report.generation(), generation);
    assert!(report.segments_dropped().is_empty());
    assert!(report.straddlers_skipped().is_empty());
    assert_eq!(report.bytes_reclaimed(), 0);
    assert_eq!(vfs.write_calls(), 0);
    assert_eq!(vfs.rename_calls(), 0);
    assert_eq!(vfs.delete_calls(), 0);
    assert_eq!(vfs.segment_bytes_read(), 0);
}
