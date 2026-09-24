//! ZE-220: a consistent point-in-time snapshot of an open store.

#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::fs::File;
use std::io::IoSlice;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use tempfile::tempdir;
use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, Normalization, StoreEpoch,
};
use zeppelin_embed::fts::tokenizer::TokenizerConfig;
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision,
};
use zeppelin_embed::lifecycle::{
    DocumentFields, OpenOptions, Store, StoreError, StoreErrorKind, StoreTestDependencies,
    SystemMonotonicClock,
};
use zeppelin_embed::tier::{MaintenanceBudget, TierThresholds};
use zeppelin_embed::vfs::{StdVfs, SyncKind, Vfs, VfsFile};

const DIMS: usize = 16;
const MAX_ID: u128 = 400;

fn epoch() -> StoreEpoch {
    let document = EmbeddingTower {
        model_id: "store-snapshot-fixture".to_owned(),
        model_version: "1".to_owned(),
        weights_digest: vec![0x20],
        dims: DIMS as u32,
        normalization: Normalization::None,
        prompt_prefix: String::new(),
        max_tokens: 512,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    StoreEpoch {
        embedding: EmbeddingEpoch {
            query: document.clone(),
            document,
            alignment_digest: Vec::new(),
        },
        tokenizer: TokenizerConfig::text_default().epoch(),
    }
}

fn vector(seed: u128) -> Vec<f32> {
    (0..DIMS)
        .map(|dimension| ((seed as f32) + dimension as f32).sin())
        .collect()
}

fn document(id: u128, revision: u64) -> IngestDocument {
    IngestDocument::new(
        DocumentVersion::new(DocId::new(id), Revision::new(revision)),
        vector(id * 31 + u128::from(revision)),
    )
    .with_timestamp(id as i64)
    .with_text(format!("note {id} revision {revision}"))
}

fn ingest(store: &Store, documents: Vec<IngestDocument>) -> u64 {
    store
        .ingest(IngestBatch::new(documents).with_epoch(epoch().identity()))
        .expect("ingest")
        .generation()
}

fn ingest_range(store: &Store, first: u128, last: u128) -> u64 {
    ingest(store, (first..=last).map(|id| document(id, 1)).collect())
}

fn delete(store: &Store, id: u128) -> u64 {
    store
        .delete(DeleteBatch::new(vec![DocId::new(id)]))
        .expect("delete")
        .generation()
}

/// Every document id the tests ever write, read at one generation.
fn contents(store: &Store) -> Vec<Option<(u128, u64, Option<String>)>> {
    let ids = (1..=MAX_ID).map(DocId::new).collect::<Vec<_>>();
    store
        .get_documents(&ids, DocumentFields::TEXT)
        .expect("read contents")
        .into_iter()
        .map(|document| {
            document.map(|document| {
                (
                    document.doc_id.get(),
                    document.revision.get(),
                    document.text,
                )
            })
        })
        .collect()
}

fn generation(store: &Store) -> u64 {
    store.snapshot().expect("lease").generation()
}

fn open_read_write(path: &Path) -> Store {
    Store::open(path, OpenOptions::default().with_epoch(epoch())).expect("open read-write")
}

fn open_read_only(path: &Path) -> Store {
    Store::open(path, OpenOptions::read_only().with_epoch(epoch())).expect("open read-only")
}

fn assert_restores(snapshot: &Path, expected: &[Option<(u128, u64, Option<String>)>]) {
    let read_only = open_read_only(snapshot);
    assert_eq!(contents(&read_only), expected, "read-only restore");
    read_only.close().expect("close read-only restore");
    let restored = open_read_write(snapshot);
    assert_eq!(contents(&restored), expected, "read-write restore");
    ingest(&restored, vec![document(MAX_ID, 1)]);
    restored.close().expect("close read-write restore");
    let reopened = open_read_only(snapshot);
    assert!(
        contents(&reopened)[(MAX_ID - 1) as usize].is_some(),
        "a restored store accepts and keeps new writes"
    );
    reopened.close().expect("close reopened restore");
}

fn names(directory: &Path) -> Vec<String> {
    let mut names = std::fs::read_dir(directory)
        .expect("list directory")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .into_string()
                .expect("utf8")
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn segment_names(directory: &Path) -> Vec<String> {
    names(directory)
        .into_iter()
        .filter(|name| name.starts_with("segment-") && name.ends_with(".zseg"))
        .collect()
}

/// Test filesystem: `StdVfs` plus a probe on every mutation below one watched
/// directory (the snapshot target's parent). It can pause the first such
/// mutation, fail every mutation from index `crash_at` on (a process that
/// died at that step writes nothing more), and records the operation order.
#[derive(Default)]
struct Probe {
    watch: Mutex<Option<PathBuf>>,
    crash_at: AtomicUsize,
    mutations: AtomicUsize,
    log: Mutex<Vec<String>>,
    pause: Mutex<PauseState>,
    changed: Condvar,
}

#[derive(Default)]
struct PauseState {
    armed: bool,
    paused: bool,
    released: bool,
}

impl Probe {
    fn new() -> Arc<Self> {
        let probe = Self::default();
        probe.crash_at.store(usize::MAX, Ordering::SeqCst);
        Arc::new(probe)
    }

    fn watch(&self, directory: &Path, crash_at: usize) {
        *self.watch.lock().expect("watch") = Some(directory.canonicalize().expect("canonical"));
        self.crash_at.store(crash_at, Ordering::SeqCst);
        self.mutations.store(0, Ordering::SeqCst);
        self.log.lock().expect("log").clear();
    }

    fn arm_pause(&self) {
        *self.pause.lock().expect("pause") = PauseState {
            armed: true,
            paused: false,
            released: false,
        };
    }

    fn wait_until_paused(&self) {
        let mut state = self.pause.lock().expect("pause");
        while !state.paused {
            state = self.changed.wait(state).expect("wait paused");
        }
    }

    fn release(&self) {
        self.pause.lock().expect("pause").released = true;
        self.changed.notify_all();
    }

    fn operations(&self) -> Vec<String> {
        self.log.lock().expect("log").clone()
    }

    fn mutation_count(&self) -> usize {
        self.mutations.load(Ordering::SeqCst)
    }

    /// Relative path below the watched directory, with the random staging
    /// directory name replaced by `<staging>`.
    fn relative(&self, path: &Path) -> Option<String> {
        let watch = self.watch.lock().expect("watch").clone()?;
        let relative = path.strip_prefix(&watch).ok()?;
        let mut parts = Vec::new();
        for component in relative.components() {
            let part = component.as_os_str().to_string_lossy().into_owned();
            parts.push(if part.contains(".snapshot-") && part.ends_with(".tmp") {
                "<staging>".to_owned()
            } else {
                part
            });
        }
        Some(if parts.is_empty() {
            ".".to_owned()
        } else {
            parts.join("/")
        })
    }

    fn mutate(&self, operation: &str, paths: &[&Path]) -> std::io::Result<()> {
        let Some(first) = paths.first().and_then(|path| self.relative(path)) else {
            return Ok(());
        };
        let mut entry = format!("{operation} {first}");
        for path in paths.iter().skip(1) {
            entry.push(' ');
            entry.push_str(
                &self
                    .relative(path)
                    .unwrap_or_else(|| "<outside>".to_owned()),
            );
        }
        let index = self.mutations.fetch_add(1, Ordering::SeqCst);
        {
            let mut state = self.pause.lock().expect("pause");
            if state.armed {
                state.armed = false;
                state.paused = true;
                self.changed.notify_all();
                while !state.released {
                    state = self.changed.wait(state).expect("wait released");
                }
            }
        }
        if index >= self.crash_at.load(Ordering::SeqCst) {
            return Err(std::io::Error::other(format!("simulated crash at {entry}")));
        }
        self.log.lock().expect("log").push(entry);
        Ok(())
    }
}

struct ProbeVfs {
    inner: StdVfs,
    probe: Arc<Probe>,
}

struct ProbeFile {
    inner: Box<dyn VfsFile>,
    path: PathBuf,
    probe: Arc<Probe>,
}

impl VfsFile for ProbeFile {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.probe.mutate("append", &[&self.path])?;
        self.inner.append(bytes)
    }

    fn append_vectored(&mut self, buffers: &mut [IoSlice<'_>]) -> std::io::Result<()> {
        self.probe.mutate("append", &[&self.path])?;
        self.inner.append_vectored(buffers)
    }

    fn sync(&self, kind: SyncKind) -> std::io::Result<()> {
        self.probe.mutate("sync", &[&self.path])?;
        self.inner.sync(kind)
    }
}

impl Vfs for ProbeVfs {
    fn ensure_directory(&self, path: &Path, create: bool) -> std::io::Result<bool> {
        self.inner.ensure_directory(path, create)
    }
    fn create_directory(&self, path: &Path) -> std::io::Result<()> {
        self.probe.mutate("mkdir", &[path])?;
        self.inner.create_directory(path)
    }
    fn remove_directory(&self, path: &Path) -> std::io::Result<()> {
        self.probe.mutate("rmdir", &[path])?;
        self.inner.remove_directory(path)
    }
    fn open(&self, path: &Path) -> std::io::Result<u64> {
        self.inner.open(path)
    }
    fn open_for_map(&self, path: &Path) -> std::io::Result<File> {
        self.inner.open_for_map(path)
    }
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        self.inner.read(path)
    }
    fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        self.inner.read_range(path, offset, length)
    }
    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.probe.mutate("write", &[path])?;
        self.inner.write(path, bytes)
    }
    fn create_new(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.probe.mutate("create", &[path])?;
        self.inner.create_new(path, bytes)
    }
    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        self.probe.mutate("open", &[path])?;
        Ok(Box::new(ProbeFile {
            inner: self.inner.open_append(path)?,
            path: path.to_path_buf(),
            probe: Arc::clone(&self.probe),
        }))
    }
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.probe.mutate("rename", &[from, to])?;
        self.inner.rename(from, to)
    }
    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        self.probe.mutate("sync", &[path])?;
        self.inner.sync(path, kind)
    }
    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        self.inner.list(directory)
    }
    fn for_each_direct_child(
        &self,
        directory: &Path,
        visitor: &mut dyn FnMut(&Path) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        self.inner.for_each_direct_child(directory, visitor)
    }
    fn delete(&self, path: &Path) -> std::io::Result<()> {
        self.probe.mutate("delete", &[path])?;
        self.inner.delete(path)
    }
}

fn open_probed(path: &Path, probe: &Arc<Probe>, drain: Duration) -> Store {
    let vfs: Arc<dyn Vfs> = Arc::new(ProbeVfs {
        inner: StdVfs,
        probe: Arc::clone(probe),
    });
    Store::open_with_test_dependencies(
        path,
        OpenOptions::default()
            .with_epoch(epoch())
            .with_reader_drain_timeout(drain),
        StoreTestDependencies::new(vfs, Arc::new(SystemMonotonicClock)),
    )
    .expect("open probed store")
}

/// Three sealed segments plus an unsealed tail holding upserts, a
/// replacement of a sealed row, and deletes of sealed and unsealed rows.
fn populate(store: &Store) {
    for batch in 0..3_u128 {
        ingest_range(store, batch * 64 + 1, batch * 64 + 64);
        store.seal().expect("seal batch");
    }
    ingest_range(store, 193, 200);
    ingest(store, vec![document(10, 2)]);
    delete(store, 5);
    delete(store, 194);
}

#[test]
fn snapshot_restores_exactly_the_pinned_generation() {
    let source = tempdir().expect("source");
    let parent = tempdir().expect("parent");
    let store = open_read_write(source.path());
    populate(&store);
    let expected = contents(&store);
    let pinned = generation(&store);

    // An existing empty directory is an accepted target.
    let target = parent.path().join("backup");
    std::fs::create_dir(&target).expect("empty target");
    let written = store.write_snapshot(&target).expect("snapshot");
    assert_eq!(
        written, pinned,
        "the snapshot reports the generation it pinned"
    );

    let after = ingest_range(&store, 201, 210);
    assert!(after > written);
    delete(&store, 1);
    store.close().expect("close source");

    assert_eq!(
        names(parent.path()),
        vec!["backup".to_owned()],
        "the staging directory is gone once the snapshot is published"
    );
    assert!(
        !target.join("writer.lock").exists(),
        "no lock file is copied"
    );
    assert_restores(&target, &expected);
}

#[test]
fn snapshot_of_a_store_without_a_manifest_carries_its_wal() {
    let source = tempdir().expect("source");
    let parent = tempdir().expect("parent");
    let store = Store::open(source.path(), OpenOptions::default()).expect("open plain");
    store
        .ingest(IngestBatch::new(vec![document(1, 1), document(2, 1)]))
        .expect("ingest plain");
    assert!(!source.path().join("manifest.ze").exists());
    let expected = contents(&store);
    let target = parent.path().join("plain");
    store.write_snapshot(&target).expect("snapshot plain store");
    store.close().expect("close plain");

    assert!(!target.join("manifest.ze").exists());
    let restored = Store::open(&target, OpenOptions::read_only()).expect("open plain restore");
    assert_eq!(contents(&restored), expected);
    restored.close().expect("close plain restore");
}

#[test]
fn snapshot_stays_consistent_while_seal_maintenance_and_writes_run() {
    let source = tempdir().expect("source");
    let parent = tempdir().expect("parent");
    let probe = Probe::new();
    let store = open_probed(source.path(), &probe, Duration::from_secs(60));
    populate(&store);
    let expected = contents(&store);
    let pinned = generation(&store);
    let pinned_segments = segment_names(source.path());
    assert_eq!(pinned_segments.len(), 3);

    let target = parent.path().join("backup");
    probe.watch(parent.path(), usize::MAX);
    probe.arm_pause();
    let written = std::thread::scope(|scope| {
        let snapshot = scope.spawn(|| store.write_snapshot(&target));
        // The copy is paused after the pin, before its first file.
        probe.wait_until_paused();

        ingest_range(&store, 201, 210);
        ingest(&store, vec![document(20, 2)]);
        store.seal().expect("seal during snapshot");
        let report = store.maintain_with_test_thresholds(
            MaintenanceBudget {
                wall_time: Duration::from_secs(600),
                bytes: u64::MAX,
            },
            TierThresholds { graph_min_rows: 64 },
        );
        assert!(
            report.consolidations >= 1,
            "maintenance consolidated: {report:?}"
        );
        delete(&store, 1);
        ingest(&store, vec![document(30, 2)]);
        let remaining = segment_names(source.path());
        assert!(
            pinned_segments.iter().all(|name| !remaining.contains(name)),
            "every pinned segment file was unlinked while the copy was paused: \
             pinned {pinned_segments:?}, remaining {remaining:?}"
        );

        probe.release();
        snapshot.join().expect("snapshot thread")
    })
    .expect("snapshot");
    assert_eq!(written, pinned);
    let live = contents(&store);
    assert_ne!(live, expected, "the source moved on during the snapshot");
    store.close().expect("close source");

    assert_restores(&target, &expected);
}

#[test]
fn snapshot_rejects_unusable_targets_before_writing() {
    let source = tempdir().expect("source");
    let parent = tempdir().expect("parent");
    let store = open_read_write(source.path());
    ingest_range(&store, 1, 4);

    let not_empty = parent.path().join("not-empty");
    std::fs::create_dir(&not_empty).expect("dir");
    std::fs::write(not_empty.join("keep.txt"), b"keep").expect("file");
    let file = parent.path().join("file");
    std::fs::write(&file, b"file").expect("file");
    let cases = [
        (not_empty.clone(), "is not empty"),
        (file.clone(), "is not a directory"),
        (source.path().join("backup"), "is inside the store"),
        (source.path().to_path_buf(), "is inside the store"),
        (
            parent.path().join("missing").join("backup"),
            "parent directory does not exist",
        ),
        (parent.path().join(".."), "has no final name"),
    ];
    for (target, reason) in cases {
        let error = store.write_snapshot(&target).expect_err("rejected target");
        assert_eq!(
            error.kind(),
            StoreErrorKind::InvalidArgument,
            "{target:?}: {error}"
        );
        assert!(
            matches!(error, StoreError::SnapshotTarget { .. }),
            "{error:?}"
        );
        let message = error.to_string();
        assert!(message.contains(reason), "{target:?}: {message}");
        assert!(message.contains(&target.display().to_string()), "{message}");
    }
    assert_eq!(names(&not_empty), vec!["keep.txt".to_owned()]);
    assert_eq!(std::fs::read(&file).expect("file"), b"file");
    assert_eq!(
        names(parent.path()),
        vec!["file".to_owned(), "not-empty".to_owned()],
        "a rejected target writes nothing"
    );
    store.close().expect("close source");

    let read_only = open_read_only(source.path());
    let error = read_only
        .write_snapshot(parent.path().join("from-read-only"))
        .expect_err("read-only handle");
    assert!(matches!(error, StoreError::ReadOnly), "{error:?}");
    read_only.close().expect("close read-only");
}

#[test]
fn snapshot_is_refused_while_a_physical_purge_is_pending() {
    let source = tempdir().expect("source");
    let parent = tempdir().expect("parent");
    let store = open_read_write(source.path());
    populate(&store);
    // Physical purge requires the WAL tail to be sealed first.
    store.seal().expect("seal tail");
    let token = store.purge(&[DocId::new(3)]).expect("purge");
    let target = parent.path().join("backup");
    let error = store.write_snapshot(&target).expect_err("pending purge");
    assert!(
        matches!(error, StoreError::SnapshotPurgePending),
        "{error:?}"
    );
    assert_eq!(error.kind(), StoreErrorKind::Unsupported);
    assert!(!target.exists());
    assert!(
        names(parent.path()).is_empty(),
        "a refused snapshot writes nothing"
    );

    store.await_physical_purge(token).expect("await purge");
    let expected = contents(&store);
    assert!(expected[2].is_none(), "the purged document is gone");
    store.write_snapshot(&target).expect("snapshot after purge");
    store.close().expect("close source");
    assert_restores(&target, &expected);
}

#[test]
fn a_crash_at_any_step_never_publishes_a_partial_snapshot() {
    let source = tempdir().expect("source");
    let parents = tempdir().expect("parents");
    let probe = Probe::new();
    let store = open_probed(source.path(), &probe, Duration::from_secs(60));
    populate(&store);
    let expected = contents(&store);

    // A clean run fixes the operation order and its length.
    let clean = parents.path().join("clean");
    std::fs::create_dir(&clean).expect("clean parent");
    probe.watch(&clean, usize::MAX);
    store
        .write_snapshot(clean.join("backup"))
        .expect("clean snapshot");
    let operations = probe.operations();
    let total = probe.mutation_count();
    assert_eq!(operations.len(), total);

    // Every staged file is synced after its last append; the staging
    // directory is synced after them; only then is it renamed into place,
    // and the parent directory is synced last.
    let rename = operations
        .iter()
        .position(|operation| operation == "rename <staging> backup")
        .expect("rename into place");
    assert_eq!(
        operations.first().map(String::as_str),
        Some("mkdir <staging>")
    );
    assert_eq!(rename + 2, operations.len(), "{operations:?}");
    assert_eq!(operations[rename + 1], "sync .");
    let staging_sync = operations
        .iter()
        .position(|operation| operation == "sync <staging>")
        .expect("staging directory sync");
    assert!(staging_sync < rename);
    let files = operations
        .iter()
        .filter_map(|operation| operation.strip_prefix("open <staging>/"))
        .collect::<Vec<_>>();
    assert_eq!(
        files.len(),
        5,
        "3 segments, the WAL and the manifest: {files:?}"
    );
    for file in files {
        let last_append = operations
            .iter()
            .rposition(|operation| operation == &format!("append <staging>/{file}"))
            .expect("appended");
        let sync = operations
            .iter()
            .rposition(|operation| operation == &format!("sync <staging>/{file}"))
            .expect("synced");
        assert!(
            last_append < sync && sync < staging_sync,
            "{file}: {operations:?}"
        );
    }

    for crash_at in 0..total {
        let parent = parents.path().join(format!("crash-{crash_at}"));
        std::fs::create_dir(&parent).expect("crash parent");
        let target = parent.join("backup");
        probe.watch(&parent, crash_at);
        let error = store.write_snapshot(&target).expect_err("crashed snapshot");
        assert_eq!(error.kind(), StoreErrorKind::Io, "{crash_at}: {error}");
        if crash_at <= rename {
            assert!(
                !target.exists(),
                "crash at step {crash_at} published {target:?}"
            );
        } else {
            // Only the final parent-directory sync failed: the renamed
            // snapshot is complete.
            assert_restores(&target, &expected);
        }
    }
    store.close().expect("close source");
}

#[test]
fn close_cancels_an_in_flight_snapshot_and_publishes_nothing() {
    let source = tempdir().expect("source");
    let parent = tempdir().expect("parent");
    let probe = Probe::new();
    let store = open_probed(source.path(), &probe, Duration::from_millis(1));
    populate(&store);
    let target = parent.path().join("backup");
    probe.watch(parent.path(), usize::MAX);
    probe.arm_pause();
    std::thread::scope(|scope| {
        let snapshot = scope.spawn(|| store.write_snapshot(&target));
        probe.wait_until_paused();
        let close = scope.spawn(|| store.close());
        while store.state().expect("state") == zeppelin_embed::lifecycle::StoreState::Open {
            std::thread::yield_now();
        }
        // Close cancels the pinned lease after its 1 ms drain grace.
        std::thread::sleep(Duration::from_millis(200));
        probe.release();
        let error = snapshot
            .join()
            .expect("snapshot thread")
            .expect_err("cancelled");
        assert_eq!(error.kind(), StoreErrorKind::Cancelled, "{error}");
        close.join().expect("close thread").expect("close");
    });
    assert!(!target.exists());
    assert!(
        names(parent.path()).is_empty(),
        "the staging directory was removed"
    );
}
