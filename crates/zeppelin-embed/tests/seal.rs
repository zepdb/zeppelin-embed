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
fn seal_of_an_empty_active_segment_is_an_idempotent_no_op_returning_the_current_generation() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");

    assert_eq!(store.seal().expect("seal empty active segment"), 0);
    let empty_snapshot = store.snapshot().expect("empty snapshot");
    assert_eq!(empty_snapshot.generation(), 0);
    assert!(empty_snapshot.segments().is_empty());
    assert!(!directory.path().join(MANIFEST_FILE).exists());
    drop(empty_snapshot);

    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(1), Revision::new(1)),
            vec![1.0, 0.0],
        )]))
        .expect("ingest row");
    let first_generation = store.seal().expect("seal active row");
    assert!(first_generation > 0);

    assert_eq!(
        store.seal().expect("repeat seal of empty active segment"),
        first_generation
    );
    let repeated_snapshot = store.snapshot().expect("snapshot after repeat seal");
    assert_eq!(repeated_snapshot.generation(), first_generation);
    assert_eq!(repeated_snapshot.segments().len(), 1);
}

#[test]
fn seal_retried_after_a_restart_that_already_committed_the_seal_is_a_no_op() {
    let directory = tempdir().expect("store directory");
    let expected = [
        DocumentVersion::new(DocId::new(11), Revision::new(1)),
        DocumentVersion::new(DocId::new(12), Revision::new(2)),
        DocumentVersion::new(DocId::new(13), Revision::new(3)),
    ];
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(expected[0], vec![1.0, 0.0]).with_timestamp(101),
            IngestDocument::new(expected[1], vec![0.0, 1.0]).with_timestamp(102),
            IngestDocument::new(expected[2], vec![-1.0, 0.0]).with_timestamp(103),
        ]))
        .expect("ingest rows");
    let first_generation = store.seal().expect("seal rows");
    store.close().expect("close sealed store");

    let reopened = Store::open(directory.path(), OpenOptions::default()).expect("reopen store");
    let outcome = reopened
        .search(
            SearchRequest::new(&[1.0, 0.0]),
            expected.len(),
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search reopened sealed rows");
    let mut actual = outcome
        .candidates
        .iter()
        .filter_map(|candidate| candidate.document())
        .collect::<Vec<_>>();
    actual.sort_unstable();
    assert_eq!(actual, expected);

    let manifest_before = load_manifest(&StdVfs, &directory.path().join(MANIFEST_FILE), u64::MAX)
        .expect("load manifest before retried seal");
    assert_eq!(
        reopened.seal().expect("retry already committed seal"),
        first_generation
    );
    let snapshot = reopened.snapshot().expect("snapshot after retried seal");
    assert_eq!(snapshot.generation(), first_generation);
    assert_eq!(snapshot.segments().len(), 1);
    let manifest_after = load_manifest(&StdVfs, &directory.path().join(MANIFEST_FILE), u64::MAX)
        .expect("load manifest after retried seal");
    assert_eq!(manifest_after.generation, manifest_before.generation);
    assert_eq!(
        manifest_after.segments.len(),
        manifest_before.segments.len()
    );

    reopened
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(14), Revision::new(1)),
                vec![0.5, 0.5],
            )
            .with_timestamp(104),
        ]))
        .expect("ingest row after retried seal");
    let second_generation = reopened.seal().expect("seal new active row");
    assert!(second_generation > first_generation);
}

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
    fn ensure_directory(&self, path: &Path, create: bool) -> std::io::Result<bool> {
        StdVfs.ensure_directory(path, create)
    }

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

    fn for_each_direct_child(
        &self,
        directory: &Path,
        visitor: &mut dyn FnMut(&Path) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        StdVfs.for_each_direct_child(directory, visitor)
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

// ZE-233: seal truncates wal.ze to a header that continues the sequence.

fn durable_options() -> OpenOptions {
    OpenOptions::new().with_durability(DurabilityMode::Durable, CommitTier::Ordered)
}

/// Writes one replacement and one delete, so a double-applied WAL record
/// would surface as a stale revision, a duplicate or a resurrected row.
fn seed_truncation_store(directory: &Path) -> (Store, Vec<DocumentVersion>) {
    let store = Store::open(directory, durable_options()).expect("open store");
    let first = |id| DocumentVersion::new(DocId::new(id), Revision::new(1));
    store
        .ingest(IngestBatch::new(
            (1..=4_u32)
                .map(|id| IngestDocument::new(first(u128::from(id)), vec![id as f32, 1.0]))
                .collect(),
        ))
        .expect("ingest rows");
    let replaced = DocumentVersion::new(DocId::new(2), Revision::new(2));
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            replaced,
            vec![0.0, 2.0],
        )]))
        .expect("replace row");
    store
        .delete(DeleteBatch::new(vec![DocId::new(3)]))
        .expect("delete row");
    (store, vec![first(1), replaced, first(4)])
}

fn live_versions(store: &Store) -> Vec<DocumentVersion> {
    let outcome = store
        .search(
            SearchRequest::new(&[1.0, 1.0]),
            1000,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search live rows");
    let mut versions = outcome
        .candidates
        .iter()
        .filter_map(|candidate| candidate.document())
        .collect::<Vec<_>>();
    versions.sort_unstable();
    versions
}

/// Copies the store files as they are at this instant: the disk image a
/// process killed here would leave behind.
fn copy_store(source: &Path) -> tempfile::TempDir {
    let copy = tempdir().expect("crash image directory");
    for entry in std::fs::read_dir(source).expect("list store") {
        let path = entry.expect("store entry").path();
        if path.is_file() {
            std::fs::copy(&path, copy.path().join(path.file_name().expect("name")))
                .expect("copy store file");
        }
    }
    copy
}

fn wal_length(directory: &Path) -> u64 {
    std::fs::metadata(directory.join("wal.ze"))
        .expect("wal metadata")
        .len()
}

fn ingest_one(store: &Store, id: u128) -> DocumentVersion {
    let version = DocumentVersion::new(DocId::new(id), Revision::new(1));
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            version,
            vec![id as f32, 1.0],
        )]))
        .expect("ingest one row");
    version
}

#[test]
fn seal_truncates_the_wal_to_a_header_and_reopen_continues_its_sequence() {
    let directory = tempdir().expect("store directory");
    let (store, mut expected) = seed_truncation_store(directory.path());
    assert!(wal_length(directory.path()) > 40);

    store.seal().expect("seal rows");

    assert_eq!(
        wal_length(directory.path()),
        40,
        "only the WAL header remains"
    );
    expected.push(ingest_one(&store, 5));
    expected.sort_unstable();
    store.close().expect("close store");
    let reopened = Store::open(directory.path(), durable_options()).expect("reopen");
    assert_eq!(live_versions(&reopened), expected);
    assert_eq!(reopened.stats().expect("stats").active_row_count, 1);
}

#[test]
fn a_kill_at_every_seal_step_reopens_with_every_write_exactly_once() {
    let mut kill_after = 0;
    loop {
        let directory = tempdir().expect("store directory");
        let (store, expected) = seed_truncation_store(directory.path());
        let vfs = StepFaultVfs::new(StepFault::DieAfter(kill_after));
        let result = store.seal_with_cancel_on_vfs(&CancelToken::new(), &vfs);
        let steps = vfs.steps();
        if result.is_ok() {
            // Segment and manifest publication, then the WAL rotation.
            let tail = steps.get(steps.len().saturating_sub(5)..).expect("tail");
            assert_eq!(
                tail,
                [
                    "write .wal.ze.purge.tmp",
                    "sync .wal.ze.purge.tmp",
                    "rename .wal.ze.purge.tmp wal.ze",
                    "open_append wal.ze",
                    "sync .",
                ],
                "steps: {steps:?}"
            );
            assert_eq!(wal_length(directory.path()), 40);
            break;
        }
        // The killed step and any cleanup after it failed without effect.
        let killed = steps.get(kill_after).expect("killed step").clone();
        let steps = format!("#{kill_after} {killed}");
        let image = copy_store(directory.path());
        drop(store);

        let reopened = Store::open(image.path(), durable_options())
            .unwrap_or_else(|error| panic!("killed at {steps:?}: reopen failed: {error}"));
        assert_eq!(live_versions(&reopened), expected, "killed at {steps:?}");
        let mut with_later = expected.clone();
        with_later.push(ingest_one(&reopened, 9));
        with_later.sort_unstable();
        reopened.seal().expect("seal after recovery");
        with_later.push(ingest_one(&reopened, 10));
        with_later.sort_unstable();
        reopened.close().expect("close recovered store");
        let again = Store::open(image.path(), durable_options()).expect("second reopen");
        assert_eq!(live_versions(&again), with_later, "killed at {steps:?}");
        kill_after += 1;
    }
    assert!(kill_after >= 13, "seal ran only {kill_after} steps");
}

/// Index of `label` in the steps of one clean seal of the seeded store.
fn seal_step_index(label: &str) -> usize {
    let directory = tempdir().expect("probe directory");
    let (probe, _) = seed_truncation_store(directory.path());
    let vfs = StepFaultVfs::new(StepFault::DieAfter(usize::MAX));
    probe
        .seal_with_cancel_on_vfs(&CancelToken::new(), &vfs)
        .expect("probe seal");
    let steps = vfs.steps();
    let index = steps.iter().rposition(|step| step == label);
    index.unwrap_or_else(|| panic!("no {label:?} in {steps:?}"))
}

#[test]
fn a_failed_directory_sync_after_the_wal_rename_keeps_later_writes_durable() {
    let directory = tempdir().expect("store directory");
    let (store, mut expected) = seed_truncation_store(directory.path());
    let vfs = StepFaultVfs::new(StepFault::FailOnly(seal_step_index("sync .")));
    store
        .seal_with_cancel_on_vfs(&CancelToken::new(), &vfs)
        .expect_err("the directory sync failure is reported");
    expected.push(ingest_one(&store, 7));
    expected.sort_unstable();
    store.close().expect("close store");

    let reopened = Store::open(directory.path(), durable_options()).expect("reopen");
    assert_eq!(live_versions(&reopened), expected);
}

#[test]
fn a_failed_wal_reopen_after_the_rename_refuses_later_writes() {
    let directory = tempdir().expect("store directory");
    let (store, expected) = seed_truncation_store(directory.path());
    let reopen = seal_step_index("open_append wal.ze");
    let vfs = StepFaultVfs::new(StepFault::FailOnly(reopen));
    store
        .seal_with_cancel_on_vfs(&CancelToken::new(), &vfs)
        .expect_err("the reopen failure is reported");
    let later = DocumentVersion::new(DocId::new(8), Revision::new(1));
    let error = store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            later,
            vec![8.0, 1.0],
        )]))
        .expect_err("a write must not reach the replaced log");
    assert!(
        matches!(
            error,
            IngestError::Store(StoreError::WalWrite(
                zeppelin_embed::wal::WalWriteError::Failed { .. }
            ))
        ),
        "{error:?}"
    );
    drop(store);

    let reopened = Store::open(directory.path(), durable_options()).expect("reopen");
    assert_eq!(live_versions(&reopened), expected);
}

#[derive(Clone, Copy)]
enum StepFault {
    /// Every step after this many fails without touching the disk.
    DieAfter(usize),
    /// Only the step with this index fails; later steps run.
    FailOnly(usize),
}

/// Records every mutating filesystem step and fails the planned ones.
#[derive(Clone)]
struct StepFaultVfs {
    fault: StepFault,
    steps: Arc<Mutex<Vec<String>>>,
}

impl StepFaultVfs {
    fn new(fault: StepFault) -> Self {
        Self {
            fault,
            steps: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn steps(&self) -> Vec<String> {
        self.steps.lock().expect("steps").clone()
    }

    fn step(&self, label: String) -> std::io::Result<()> {
        let mut steps = self.steps.lock().expect("steps");
        let index = steps.len();
        steps.push(label);
        let fails = match self.fault {
            StepFault::DieAfter(count) => index >= count,
            StepFault::FailOnly(target) => index == target,
        };
        if fails {
            Err(std::io::Error::other("planned step fault"))
        } else {
            Ok(())
        }
    }
}

fn step_name(path: &Path) -> String {
    if path.is_dir() {
        return ".".to_owned();
    }
    path.file_name().map_or_else(
        || ".".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

struct StepFaultFile {
    inner: Box<dyn VfsFile>,
    name: String,
    vfs: StepFaultVfs,
}

impl VfsFile for StepFaultFile {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.vfs.step(format!("append {}", self.name))?;
        self.inner.append(bytes)
    }

    fn sync(&self, kind: SyncKind) -> std::io::Result<()> {
        self.vfs.step(format!("sync-file {}", self.name))?;
        self.inner.sync(kind)
    }
}

impl Vfs for StepFaultVfs {
    fn ensure_directory(&self, path: &Path, create: bool) -> std::io::Result<bool> {
        StdVfs.ensure_directory(path, create)
    }

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
        self.step(format!("write {}", step_name(path)))?;
        StdVfs.write(path, bytes)
    }

    fn create_new(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.step(format!("create_new {}", step_name(path)))?;
        StdVfs.create_new(path, bytes)
    }

    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        self.step(format!("open_append {}", step_name(path)))?;
        Ok(Box::new(StepFaultFile {
            inner: StdVfs.open_append(path)?,
            name: step_name(path),
            vfs: self.clone(),
        }))
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.step(format!("rename {} {}", step_name(from), step_name(to)))?;
        StdVfs.rename(from, to)
    }

    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        self.step(format!("sync {}", step_name(path)))?;
        StdVfs.sync(path, kind)
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        StdVfs.list(directory)
    }

    fn for_each_direct_child(
        &self,
        directory: &Path,
        visitor: &mut dyn FnMut(&Path) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        StdVfs.for_each_direct_child(directory, visitor)
    }

    fn delete(&self, path: &Path) -> std::io::Result<()> {
        self.step(format!("delete {}", step_name(path)))?;
        StdVfs.delete(path)
    }
}

#[test]
fn idle_merge_bounds_segments_across_1000_seals_and_reopens() {
    let directory = tempdir().expect("directory");
    let mut store = Store::open(directory.path(), durable_options()).expect("open");
    let mut seen = std::collections::HashSet::new();
    let mut previous = std::collections::HashSet::new();
    let mut observe_ids = |store: &Store| {
        let snapshot = store.snapshot().expect("snapshot");
        let current = snapshot
            .segments()
            .iter()
            .map(|s| s.meta().id)
            .collect::<std::collections::HashSet<_>>();
        for id in current.difference(&previous) {
            assert!(seen.insert(*id), "reused live or retired segment {id}");
        }
        previous = current;
    };
    for id in 1..=1000 {
        ingest_one(&store, id);
        store.seal().expect("seal");
        observe_ids(&store);
        // An idle interval every eight seals, including the final interval.
        if id % 8 == 0 {
            store.merge_sealed().expect("idle merge");
        }
        observe_ids(&store);
        let count = store.snapshot().expect("snapshot").segments().len();
        assert!(count <= 8, "small seals accumulated: {count}");
        if id % 100 == 0 {
            store.close().expect("close");
            store = Store::open(directory.path(), durable_options()).expect("reopen");
            assert_eq!(live_versions(&store).len(), id as usize);
        }
    }
    assert_eq!(store.snapshot().expect("snapshot").segments().len(), 1);
}

#[test]
fn idle_merge_preserves_wal_and_purge() {
    let directory = tempdir().expect("directory");
    let store = Store::open(directory.path(), durable_options()).expect("open");
    for id in 1..=4 {
        ingest_one(&store, id);
        store.seal().expect("seal");
    }
    ingest_one(&store, 5);
    let wal = std::fs::read(directory.path().join("wal.ze")).expect("WAL");
    store.merge_sealed().expect("merge");
    assert_eq!(store.snapshot().expect("snapshot").segments().len(), 1);
    assert_eq!(
        std::fs::read(directory.path().join("wal.ze")).expect("WAL"),
        wal
    );
    let token = store.purge(&[DocId::new(2)]).expect("purge");
    store.await_physical_purge(token).expect("purge completes");
    store.close().expect("close");
    let store = Store::open(directory.path(), durable_options()).expect("reopen");
    let ids = live_versions(&store)
        .iter()
        .map(|v| v.doc_id().get())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec![1, 3, 4, 5]);
    store.seal().expect("seal WAL tail");
    store.merge_sealed().expect("merge purge output");
    assert_eq!(store.snapshot().expect("snapshot").segments().len(), 1);
}

#[test]
fn interrupted_idle_merge_reopens_with_all_documents() {
    fn seed(path: &Path) -> Store {
        let store = Store::open(path, durable_options()).expect("open");
        for id in 1..=3 {
            ingest_one(&store, id);
            store.seal().expect("seal");
        }
        store
    }
    let directory = tempdir().expect("directory");
    let store = seed(directory.path());
    let trace = StepFaultVfs::new(StepFault::DieAfter(usize::MAX));
    store
        .merge_sealed_with_cancel_on_vfs(&CancelToken::new(), &trace)
        .expect("trace merge");
    let steps = trace.steps();
    assert!(!steps.is_empty(), "merge must publish a replacement");
    for cut in 0..steps.len() {
        let directory = tempdir().expect("crash directory");
        let store = seed(directory.path());
        let fault = StepFaultVfs::new(StepFault::DieAfter(cut));
        store
            .merge_sealed_with_cancel_on_vfs(&CancelToken::new(), &fault)
            .expect_err("injected interruption");
        drop(store);
        let store = Store::open(directory.path(), durable_options()).expect("recover");
        assert_eq!(live_versions(&store).len(), 3, "cut {cut}: {}", steps[cut]);
        store.merge_sealed().expect("retry merge");
        assert_eq!(store.snapshot().expect("snapshot").segments().len(), 1);
    }
}

#[test]
fn idle_merge_refuses_batches_over_the_input_byte_bound() {
    let directory = tempdir().expect("directory");
    let store = Store::open(directory.path(), durable_options()).expect("open");
    for id in 1..=20 {
        store
            .ingest(IngestBatch::new(vec![
                IngestDocument::new(
                    DocumentVersion::new(DocId::new(id), Revision::new(1)),
                    vec![1.0, 0.0],
                )
                .with_metadata(vec![7; 512 * 1024]),
            ]))
            .expect("large payload");
        if id % 10 == 0 {
            store.seal().expect("seal");
        }
    }
    let before = store.snapshot().expect("snapshot");
    assert_eq!(
        store.merge_sealed().expect("bounded no-op"),
        before.generation()
    );
    assert_eq!(store.snapshot().expect("snapshot").segments().len(), 2);
}

#[test]
fn idle_merge_respects_cancellation_read_only_and_closed_handles() {
    let directory = tempdir().expect("directory");
    let store = Store::open(directory.path(), durable_options()).expect("open");
    ingest_one(&store, 1);
    store.seal().expect("seal");
    let cancel = CancelToken::new();
    cancel.cancel();
    assert!(matches!(
        store.merge_sealed_with_cancel(&cancel),
        Err(StoreError::SealCancelled)
    ));
    store.close().expect("close");
    assert!(matches!(store.merge_sealed(), Err(StoreError::Closed)));
    let reader = Store::open(directory.path(), OpenOptions::read_only()).expect("reader");
    assert!(matches!(reader.merge_sealed(), Err(StoreError::ReadOnly)));
}

#[test]
fn snapshot_cursor_survives_idle_merge_and_reopen() {
    use zeppelin_embed::lifecycle::{DocumentFields, DocumentScanRequest};
    let directory = tempdir().expect("snapshot merge directory");
    let store = Store::open(directory.path(), durable_options()).expect("open");
    for id in 1..=2 {
        ingest_one(&store, id);
        store.seal().expect("seal initial row");
    }
    let files = store
        .snapshot()
        .expect("published")
        .segments()
        .iter()
        .map(|segment| directory.path().join(segment.meta().id.file_name()))
        .collect::<Vec<_>>();
    ingest_one(&store, 3);
    let view = store.open_snapshot().expect("pin active and sealed rows");
    let request = || {
        DocumentScanRequest::new(
            1,
            DocumentFields::NONE,
            QueryControl::Cancel(CancelToken::new()),
        )
    };
    let first = view.scan_documents(request()).expect("first page");
    store.seal().expect("seal pinned active rows");
    ingest_one(&store, 4);
    store.seal().expect("seal later row");
    store.merge_sealed().expect("merge proceeds while pinned");
    assert_eq!(store.snapshot().expect("merged").segments().len(), 1);
    for path in &files {
        assert!(
            path.exists(),
            "idle merge unlinked pinned input: {}",
            path.display()
        );
    }
    store.close().expect("close source");
    let reader = Store::open(directory.path(), OpenOptions::read_only()).expect("reopen reader");
    assert_eq!(live_versions(&reader).len(), 4);
    let mut ids = first
        .documents
        .iter()
        .map(|row| row.doc_id.get())
        .collect::<Vec<_>>();
    let mut cursor = first.continuation;
    while let Some(next) = cursor {
        let page = view
            .scan_documents(request().with_cursor(next))
            .expect("resume after reopen");
        assert_eq!(page.generation, first.generation);
        ids.extend(page.documents.iter().map(|row| row.doc_id.get()));
        cursor = page.continuation;
    }
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2, 3]);
    reader.close().expect("close reader");
    view.close().expect("release pin");
    let reopened = Store::open(directory.path(), durable_options()).expect("reopen writer");
    assert_eq!(live_versions(&reopened).len(), 4);
    for path in files {
        assert!(!path.exists(), "retired input not reclaimed");
    }
    reopened.close().expect("close reopened writer");
}
