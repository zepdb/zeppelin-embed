#![allow(clippy::expect_used, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

use tempfile::tempdir;
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, IngestError, Revision,
    SearchRequest,
};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store, StoreError};
use zeppelin_embed::manifest::io::{MANIFEST_FILE, commit_manifest, load_manifest};
use zeppelin_embed::manifest::{Manifest, ManifestError};
use zeppelin_embed::meta::{AliveSet, ColumnStoreBuilder, Schema};
use zeppelin_embed::quant::Bit4Factors;
use zeppelin_embed::scan::ScanOptions;
use zeppelin_embed::segment::SegmentId;
use zeppelin_embed::segment::writer::{SegmentBuild, SegmentFactors, write_segment};
use zeppelin_embed::vfs::{StdVfs, SyncKind, Vfs, VfsFile};

#[test]
fn seal_publishes_an_immutable_segment_and_empties_the_active_segment() {
    let directory = tempdir().expect("store directory");
    let existing_id = SegmentId::new(0x0102_0304_0506, [0x11; 10]);
    publish_existing_segment(directory.path(), existing_id);
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    let ack = store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(10), Revision::new(1)),
                vec![1.0, 0.0],
            ),
            IngestDocument::new(
                DocumentVersion::new(DocId::new(11), Revision::new(1)),
                vec![0.0, 1.0],
            ),
        ]))
        .expect("ingest active rows");

    let generation = store.seal().expect("seal active segment");

    let snapshot = store.snapshot().expect("sealed snapshot");
    assert!(generation > ack.generation());
    assert_eq!(snapshot.generation(), generation);
    assert_eq!(snapshot.segments().len(), 2, "seal must append");
    assert_eq!(snapshot.segments()[0].meta().id, existing_id);
    assert_eq!(snapshot.segments()[1].meta().row_count, 2);
    assert_eq!(
        store.stats().expect("post-seal stats").active_segment_bytes,
        0,
        "sealed active rows must release their anonymous buffers"
    );
}

#[test]
fn sealed_rows_survive_close_and_reopen() {
    let directory = tempdir().expect("store directory");
    let expected = [
        DocumentVersion::new(DocId::new(21), Revision::new(1)),
        DocumentVersion::new(DocId::new(22), Revision::new(3)),
        DocumentVersion::new(DocId::new(23), Revision::new(8)),
    ];
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(expected[0], vec![1.0, 0.0]),
            IngestDocument::new(expected[1], vec![0.0, 1.0]),
            IngestDocument::new(expected[2], vec![-1.0, 0.0]),
        ]))
        .expect("ingest rows");
    store.seal().expect("seal rows");
    store.close().expect("close sealed store");

    let reopened = Store::open(directory.path(), OpenOptions::default()).expect("reopen store");
    let outcome = reopened
        .search(
            SearchRequest::new(&[1.0, 0.0]),
            expected.len(),
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("query reopened sealed rows");
    let mut actual = outcome
        .candidates
        .iter()
        .filter_map(|candidate| candidate.document())
        .collect::<Vec<_>>();
    actual.sort_unstable();

    assert_eq!(actual, expected);
}

#[test]
fn reopen_after_seal_does_not_replay_absorbed_wal_records() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(31), Revision::new(1)),
                vec![1.0, 0.0],
            ),
            IngestDocument::new(
                DocumentVersion::new(DocId::new(32), Revision::new(1)),
                vec![0.0, 1.0],
            ),
        ]))
        .expect("ingest rows");
    store.seal().expect("seal rows");
    store.close().expect("close sealed store");

    let reopened = Store::open(directory.path(), OpenOptions::default()).expect("reopen store");

    assert_eq!(
        reopened.stats().expect("reopened stats").active_row_count,
        0,
        "absorbed WAL records were rebuilt into the active segment"
    );
}

#[test]
fn seal_retires_wal_records_and_wal_bytes_falls() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(41), Revision::new(1)),
            vec![1.0, 0.0],
        )]))
        .expect("ingest row");
    let before = store.stats().expect("pre-seal stats").wal_bytes;

    store.seal().expect("seal row");

    let after = store.stats().expect("post-seal stats").wal_bytes;
    assert!(before > 0, "fixture did not retain a WAL record");
    assert!(
        after < before,
        "wal_bytes did not fall: {before} -> {after}"
    );
    assert_eq!(after, 0, "the complete absorbed prefix must be retired");
}

#[test]
fn seal_refuses_a_boundary_ahead_of_the_durable_wal_end() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(51), Revision::new(1)),
            vec![1.0, 0.0],
        )]))
        .expect("ingest row");
    store.seal().expect("seal row");
    let mut manifest = load_manifest(&StdVfs, &directory.path().join(MANIFEST_FILE), u64::MAX)
        .expect("load committed manifest");
    manifest.log_seq = 2;
    let policy = DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::Ordered)
        .expect("derived policy");
    commit_manifest(&StdVfs, directory.path(), &manifest, policy)
        .expect("write ahead boundary fixture");
    store.close().expect("close forged store");

    let error = Store::open(directory.path(), OpenOptions::default())
        .err()
        .expect("ahead boundary must refuse open");

    assert!(
        matches!(
            &error,
            zeppelin_embed::lifecycle::StoreError::Manifest(ManifestError::AheadOfLog {
                snapshot: 2,
                durable: 1
            })
        ),
        "wrong ahead-boundary error: {error:?}"
    );
}

#[test]
fn cancelled_seal_leaves_the_store_unchanged() {
    let directory = tempdir().expect("store directory");
    let store = Arc::new(
        Store::open(
            directory.path(),
            OpenOptions::new().with_durability(DurabilityMode::Durable, CommitTier::Durable),
        )
        .expect("open durable store"),
    );
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(61), Revision::new(1)),
            vec![1.0, 0.0],
        )]))
        .expect("ingest row");
    let before_snapshot = store.snapshot().expect("snapshot before seal");
    let before_generation = before_snapshot.generation();
    let before_segments = before_snapshot
        .segments()
        .iter()
        .map(|segment| segment.meta().clone())
        .collect::<Vec<_>>();
    drop(before_snapshot);
    let before_stats = store.stats().expect("stats before seal");
    let before_files = file_names(directory.path());
    let cancel = CancelToken::new();
    let blocking = BlockingVfs::new(StdVfs);
    blocking.block_next_syncs(1).expect("arm segment sync");
    let sealing_store = Arc::clone(&store);
    let sealing_cancel = cancel.clone();
    let sealing_vfs = blocking.clone();
    let seal = std::thread::spawn(move || {
        sealing_store.seal_with_cancel_on_vfs(&sealing_cancel, &sealing_vfs)
    });
    blocking
        .wait_until_blocked(1)
        .expect("seal reached segment data sync");
    cancel.cancel();
    blocking.release_syncs(1).expect("release segment sync");

    let error = seal
        .join()
        .expect("seal thread")
        .expect_err("cancelled seal must fail");

    assert!(matches!(error, StoreError::SealCancelled));
    let after_snapshot = store.snapshot().expect("snapshot after cancellation");
    assert_eq!(after_snapshot.generation(), before_generation);
    assert_eq!(
        after_snapshot
            .segments()
            .iter()
            .map(|segment| segment.meta().clone())
            .collect::<Vec<_>>(),
        before_segments
    );
    let after_stats = store.stats().expect("stats after cancellation");
    assert_eq!(after_stats.active_row_count, before_stats.active_row_count);
    assert_eq!(
        after_stats.active_segment_bytes,
        before_stats.active_segment_bytes
    );
    assert_eq!(after_stats.wal_bytes, before_stats.wal_bytes);
    assert_eq!(file_names(directory.path()), before_files);
}

#[test]
fn tombstoned_active_rows_do_not_become_live_sealed_rows() {
    let directory = tempdir().expect("store directory");
    let deleted = DocumentVersion::new(DocId::new(71), Revision::new(1));
    let retained = DocumentVersion::new(DocId::new(72), Revision::new(1));
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(deleted, vec![1.0, 0.0]),
            IngestDocument::new(retained, vec![0.0, 1.0]),
        ]))
        .expect("ingest rows");
    store
        .delete(DeleteBatch::new(vec![deleted.doc_id()]))
        .expect("tombstone row");

    store.seal().expect("seal rows");

    let snapshot = store.snapshot().expect("sealed snapshot");
    let sealed = snapshot.segments().first().expect("sealed segment");
    let alive = sealed.alive().expect("sealed alive set");
    assert_eq!(alive.live_count(), 1);
    assert_eq!(alive.tombstone_count(), 1);
    let outcome = store
        .search(
            SearchRequest::new(&[1.0, 0.0]),
            2,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search sealed rows");
    let versions = outcome
        .candidates
        .iter()
        .filter_map(|candidate| candidate.document())
        .collect::<Vec<_>>();
    assert_eq!(versions, vec![retained]);
}

#[test]
fn delete_after_seal_removes_the_row_from_search() {
    let directory = tempdir().expect("store directory");
    let deleted = DocumentVersion::new(DocId::new(73), Revision::new(1));
    let retained = DocumentVersion::new(DocId::new(74), Revision::new(1));
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(deleted, vec![1.0, 0.0]),
            IngestDocument::new(retained, vec![0.0, 1.0]),
        ]))
        .expect("ingest rows");
    let sealed_generation = store.seal().expect("seal rows");
    let original_id = store.snapshot().expect("original snapshot").segments()[0]
        .meta()
        .id;

    let ack = store
        .delete(DeleteBatch::new(vec![deleted.doc_id()]))
        .expect("delete sealed row");

    let snapshot = store.snapshot().expect("tombstoned snapshot");
    let replacement = snapshot.segments().first().expect("replacement segment");
    assert!(ack.generation() > sealed_generation);
    assert_eq!(snapshot.generation(), ack.generation());
    assert_ne!(replacement.meta().id, original_id);
    assert!(!directory.path().join(original_id.file_name()).exists());
    assert_eq!(
        replacement
            .alive()
            .expect("replacement alive set")
            .live_count(),
        1
    );
    drop(snapshot);
    let outcome = store
        .search(
            SearchRequest::new(&[1.0, 0.0]),
            2,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search after sealed delete");
    let versions = outcome
        .candidates
        .iter()
        .filter_map(|candidate| candidate.document())
        .collect::<Vec<_>>();
    assert_eq!(versions, vec![retained]);

    store.close().expect("close tombstoned store");
    let reopened = Store::open(directory.path(), OpenOptions::default()).expect("reopen store");
    let reopened_outcome = reopened
        .search(
            SearchRequest::new(&[1.0, 0.0]),
            2,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search reopened tombstoned store");
    assert_eq!(
        reopened_outcome
            .candidates
            .iter()
            .filter_map(|candidate| candidate.document())
            .collect::<Vec<_>>(),
        vec![retained]
    );
}

#[test]
fn reingest_after_seal_replaces_rather_than_duplicating() {
    let directory = tempdir().expect("store directory");
    let doc_id = DocId::new(75);
    let old = DocumentVersion::new(doc_id, Revision::new(1));
    let replacement = DocumentVersion::new(doc_id, Revision::new(2));
    let other = DocumentVersion::new(DocId::new(76), Revision::new(1));
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(old, vec![1.0, 0.0]),
            IngestDocument::new(other, vec![-1.0, 0.0]),
        ]))
        .expect("ingest rows");
    let sealed_generation = store.seal().expect("seal rows");

    let ack = store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            replacement,
            vec![0.0, 1.0],
        )]))
        .expect("replace sealed revision");

    assert!(ack.generation() > sealed_generation);
    assert_eq!(
        store.snapshot().expect("replacement snapshot").generation(),
        ack.generation()
    );
    let outcome = store
        .search(
            SearchRequest::new(&[1.0, 1.0]),
            3,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search replacement");
    let versions = outcome
        .candidates
        .iter()
        .filter_map(|candidate| candidate.document())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        versions,
        std::collections::BTreeSet::from([replacement, other])
    );
    assert!(!versions.contains(&old));

    store.close().expect("close replacement store");
    let reopened = Store::open(directory.path(), OpenOptions::default()).expect("reopen store");
    let reopened_outcome = reopened
        .search(
            SearchRequest::new(&[1.0, 1.0]),
            3,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search reopened replacement");
    assert_eq!(
        reopened_outcome
            .candidates
            .iter()
            .filter_map(|candidate| candidate.document())
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from([replacement, other])
    );
}

#[test]
fn stale_revision_after_seal_is_rejected() {
    let directory = tempdir().expect("store directory");
    let doc_id = DocId::new(77);
    let current = DocumentVersion::new(doc_id, Revision::new(5));
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            current,
            vec![1.0, 0.0],
        )]))
        .expect("ingest current revision");
    let sealed_generation = store.seal().expect("seal current revision");

    let error = store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(doc_id, Revision::new(4)),
            vec![0.0, 1.0],
        )]))
        .expect_err("stale sealed revision must fail");

    assert!(matches!(
        error,
        IngestError::StaleRevision {
            doc_id: rejected,
            current: current_revision,
            attempted,
        } if rejected == doc_id
            && current_revision == Revision::new(5)
            && attempted == Revision::new(4)
    ));
    assert_eq!(
        store.snapshot().expect("unchanged snapshot").generation(),
        sealed_generation
    );
    let outcome = store
        .search(
            SearchRequest::new(&[1.0, 0.0]),
            2,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search unchanged sealed revision");
    assert_eq!(outcome.candidates.len(), 1);
    assert_eq!(outcome.candidates[0].document(), Some(current));
}

#[test]
fn sealed_segment_is_searchable_through_store_search() {
    let directory = tempdir().expect("store directory");
    let version = DocumentVersion::new(DocId::new(81), Revision::new(4));
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            version,
            vec![1.0, 0.0],
        )]))
        .expect("ingest row");
    store.seal().expect("seal row");

    let outcome = store
        .search(
            SearchRequest::new(&[1.0, 0.0]),
            1,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search store");

    assert_eq!(outcome.candidates.len(), 1);
    assert_eq!(outcome.candidates[0].document(), Some(version));
    assert!(matches!(
        outcome.candidates[0].row_id().source(),
        zeppelin_embed::ingest::RowSource::Sealed(_)
    ));
}

fn file_names(directory: &std::path::Path) -> Vec<std::ffi::OsString> {
    let mut names = std::fs::read_dir(directory)
        .expect("read store directory")
        .map(|entry| entry.expect("directory entry").file_name())
        .collect::<Vec<_>>();
    names.sort_unstable();
    names
}

#[derive(Clone, Default)]
struct BlockingVfs {
    state: Arc<(Mutex<BlockState>, Condvar)>,
}

#[derive(Default)]
struct BlockState {
    armed: bool,
    blocked: bool,
    released: bool,
}

impl BlockingVfs {
    fn new(_: StdVfs) -> Self {
        Self::default()
    }

    fn block_next_syncs(&self, count: usize) -> std::io::Result<()> {
        if count != 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "fixture supports exactly one blocked sync",
            ));
        }
        let mut state = self
            .state
            .0
            .lock()
            .map_err(|_| std::io::Error::other("blocking VFS mutex poisoned"))?;
        state.armed = true;
        state.blocked = false;
        state.released = false;
        Ok(())
    }

    fn wait_until_blocked(&self, count: usize) -> std::io::Result<()> {
        if count != 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "fixture supports exactly one blocked sync",
            ));
        }
        let mut state = self
            .state
            .0
            .lock()
            .map_err(|_| std::io::Error::other("blocking VFS mutex poisoned"))?;
        while !state.blocked {
            state = self
                .state
                .1
                .wait(state)
                .map_err(|_| std::io::Error::other("blocking VFS mutex poisoned"))?;
        }
        Ok(())
    }

    fn release_syncs(&self, count: usize) -> std::io::Result<()> {
        if count != 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "fixture supports exactly one blocked sync",
            ));
        }
        let mut state = self
            .state
            .0
            .lock()
            .map_err(|_| std::io::Error::other("blocking VFS mutex poisoned"))?;
        state.released = true;
        self.state.1.notify_all();
        Ok(())
    }

    fn block_sync(&self) -> std::io::Result<()> {
        let mut state = self
            .state
            .0
            .lock()
            .map_err(|_| std::io::Error::other("blocking VFS mutex poisoned"))?;
        if !state.armed {
            return Ok(());
        }
        state.armed = false;
        state.blocked = true;
        self.state.1.notify_all();
        while !state.released {
            state = self
                .state
                .1
                .wait(state)
                .map_err(|_| std::io::Error::other("blocking VFS mutex poisoned"))?;
        }
        Ok(())
    }
}

impl Vfs for BlockingVfs {
    fn open(&self, path: &Path) -> std::io::Result<u64> {
        StdVfs.open(path)
    }

    fn open_for_map(&self, path: &Path) -> std::io::Result<std::fs::File> {
        StdVfs.open_for_map(path)
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
        StdVfs.rename(from, to)
    }

    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        self.block_sync()?;
        StdVfs.sync(path, kind)
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        StdVfs.list(directory)
    }

    fn delete(&self, path: &Path) -> std::io::Result<()> {
        StdVfs.delete(path)
    }
}

fn publish_existing_segment(directory: &std::path::Path, id: SegmentId) {
    let schema = Schema::new(Vec::new()).expect("timestamp-only schema");
    let mut builder = ColumnStoreBuilder::new(schema.clone());
    builder.push_row(0, &[]).expect("fixture row");
    let columns = builder.finish().expect("fixture columns");
    let policy = DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::Ordered)
        .expect("derived policy");
    let meta = write_segment(
        &StdVfs,
        directory,
        SegmentBuild {
            id,
            scheme: 4,
            dims: 2,
            codes: &[0x88],
            factors: SegmentFactors::Bit4(&[Bit4Factors::from_persisted(1.0, 1.0, 1.0)]),
            rescore: &[0.0, 0.0],
            columns: &columns,
            alive: &AliveSet::new(1),
        },
        policy,
    )
    .expect("write existing segment");
    commit_manifest(
        &StdVfs,
        directory,
        &Manifest {
            generation: 1,
            log_seq: 0,
            segments: vec![meta],
            epochs: Vec::new(),
            epoch_alias: None,
            schema,
        },
        policy,
    )
    .expect("publish existing segment");
}
