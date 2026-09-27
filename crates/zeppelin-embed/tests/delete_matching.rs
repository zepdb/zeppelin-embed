//! ZE-217: delete every document matching a filter in one mutation, and
//! remove the deleted bytes from every store file before returning.

#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Command;
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

use tempfile::tempdir;
use zeppelin_embed::ingest::{
    DeleteMatchingError, DocId, DocumentVersion, IngestBatch, IngestDocument, PurgeError, Revision,
};
use zeppelin_embed::lifecycle::{DocumentFields, OpenOptions, Store, StoreError};
#[cfg(unix)]
use zeppelin_embed::lifecycle::{StoreTestDependencies, SystemMonotonicClock};
use zeppelin_embed::meta::Schema;
use zeppelin_embed::meta::{ColumnDefinition, ColumnId, ColumnType, Predicate, PredicateValue};
#[cfg(unix)]
use zeppelin_embed::vfs::{StdVfs, SyncKind, Vfs, VfsFile};

const NOTE: ColumnId = ColumnId::new(1);
const MARKER: &str = "ZE217PURGEDMARKER";
const SURVIVOR_MARKER: &str = "ZE217SURVIVORMARKER";

fn schema() -> Schema {
    Schema::new(vec![ColumnDefinition::new(
        NOTE,
        "noteId",
        ColumnType::U64,
        false,
    )])
    .expect("noteId schema")
}

fn options() -> OpenOptions {
    OpenOptions::default().with_schema(schema())
}

fn note_is(note: u64) -> Predicate {
    Predicate::Eq {
        column: NOTE,
        value: PredicateValue::U64(note),
    }
}

fn document(id: u128, revision: u64, note: u64, text: &str) -> IngestDocument {
    IngestDocument::new(
        DocumentVersion::new(DocId::new(id), Revision::new(revision)),
        vec![1.0],
    )
    .with_text(text)
    .with_columns(vec![(NOTE, PredicateValue::U64(note))])
}

fn count(store: &Store, note: u64) -> u64 {
    store
        .count_documents(Some(&note_is(note)), None)
        .expect("count documents")
        .count
}

fn generation(store: &Store) -> u64 {
    store
        .count_documents(None, None)
        .expect("current generation")
        .generation
}

/// Every file under `root` whose bytes contain `needle`.
fn files_containing(root: &Path, needle: &[u8]) -> Vec<PathBuf> {
    let mut hits = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).expect("read store directory") {
            let path = entry.expect("store directory entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if fs::read(&path)
                .expect("read store file")
                .windows(needle.len())
                .any(|window| window == needle)
            {
                hits.push(path);
            }
        }
    }
    hits
}

fn marker_files(root: &Path) -> Vec<PathBuf> {
    let mut hits = files_containing(root, MARKER.as_bytes());
    hits.extend(files_containing(
        root,
        MARKER.to_ascii_lowercase().as_bytes(),
    ));
    hits
}

/// Note 7 lives in a sealed segment and in the active segment. Doc 3 moved
/// from note 7 to note 8 in the active segment; doc 20 moved the other way.
/// Only the live version decides a match.
fn populate(store: &Store) -> Vec<DocId> {
    store
        .ingest(IngestBatch::new(vec![
            document(1, 1, 7, &format!("{MARKER} one")),
            document(2, 1, 7, &format!("{MARKER} two")),
            document(3, 1, 7, "moves away from note seven"),
            document(20, 1, 8, "moves into note seven"),
            document(21, 1, 8, &format!("{SURVIVOR_MARKER} kept")),
        ]))
        .expect("ingest sealed rows");
    store.seal().expect("seal first rows");
    store
        .ingest(IngestBatch::new(vec![
            document(3, 2, 8, "now in note eight"),
            document(20, 2, 7, &format!("{MARKER} moved in")),
            document(4, 1, 7, &format!("{MARKER} active")),
            document(22, 1, 8, "active survivor"),
        ]))
        .expect("ingest active rows");
    [1, 2, 4, 20].map(DocId::new).to_vec()
}

#[test]
fn delete_matching_removes_every_live_match_and_nothing_else() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), options()).expect("open store");
    let expected = populate(&store);
    assert_eq!(count(&store, 7), 4);
    assert_eq!(count(&store, 8), 3);
    let before = generation(&store);

    let report = store
        .delete_matching(&note_is(7))
        .expect("delete note seven");

    assert_eq!(report.deleted_ids(), expected.as_slice());
    assert!(report.generation() > before);
    assert_eq!(report.generation(), generation(&store));
    assert_eq!(count(&store, 7), 0);
    assert_eq!(count(&store, 8), 3);
    let documents = store
        .get_documents(&expected, DocumentFields::NONE)
        .expect("get deleted ids");
    assert!(documents.iter().all(Option::is_none));
    let survivors = store
        .get_documents(&[3, 21, 22].map(DocId::new), DocumentFields::NONE)
        .expect("get survivors");
    assert!(survivors.iter().all(Option::is_some));
    store.close().expect("close store");

    let reopened = Store::open(directory.path(), options()).expect("reopen store");
    assert_eq!(count(&reopened, 7), 0);
    assert_eq!(count(&reopened, 8), 3);
}

#[test]
fn delete_matching_leaves_no_matched_text_in_any_store_file() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), options()).expect("open store");
    populate(&store);
    assert!(
        !marker_files(directory.path()).is_empty(),
        "the marker must be findable before the delete"
    );

    store
        .delete_matching(&note_is(7))
        .expect("delete note seven");

    assert_eq!(marker_files(directory.path()), Vec::<PathBuf>::new());
    assert!(
        !files_containing(directory.path(), SURVIVOR_MARKER.as_bytes()).is_empty(),
        "survivor text must stay on disk"
    );
    store.close().expect("close store");
    assert_eq!(marker_files(directory.path()), Vec::<PathBuf>::new());
}

#[test]
fn delete_matching_without_a_match_changes_nothing() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), options()).expect("open store");
    populate(&store);
    let before = generation(&store);

    let report = store
        .delete_matching(&note_is(99))
        .expect("delete an absent note");

    assert!(report.deleted_ids().is_empty());
    assert_eq!(report.generation(), before);
    assert_eq!(generation(&store), before);
    assert_eq!(count(&store, 7), 4);
}

#[test]
fn delete_matching_rejects_a_predicate_outside_the_schema() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), options()).expect("open store");
    populate(&store);
    let before = generation(&store);

    let error = store
        .delete_matching(&Predicate::Eq {
            column: ColumnId::new(99),
            value: PredicateValue::U64(7),
        })
        .expect_err("unknown column must be rejected");

    assert!(
        matches!(error, DeleteMatchingError::Predicate(_)),
        "{error:?}"
    );
    assert_eq!(generation(&store), before);
    assert_eq!(count(&store, 7), 4);
}

#[test]
fn delete_matching_rejects_a_read_only_store() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), options()).expect("open store");
    populate(&store);
    store.close().expect("close writer");
    let reader =
        Store::open(directory.path(), OpenOptions::read_only()).expect("open read-only store");

    let error = reader
        .delete_matching(&note_is(7))
        .expect_err("a read-only store cannot delete");

    assert!(
        matches!(
            error,
            DeleteMatchingError::Purge(PurgeError::Store(StoreError::ReadOnly))
        ),
        "{error:?}"
    );
    assert_eq!(count(&reader, 7), 4);
}

#[test]
fn delete_matching_refuses_while_another_purge_is_pending() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), options()).expect("open store");
    populate(&store);
    let pending = store.purge(&[DocId::new(21)]).expect("schedule a purge");

    let error = store
        .delete_matching(&note_is(7))
        .expect_err("a pending purge blocks delete_matching");

    assert!(
        matches!(
            error,
            DeleteMatchingError::Purge(PurgeError::PurgeInProgress)
        ),
        "{error:?}"
    );
    assert_eq!(count(&store, 7), 4);
    store
        .await_physical_purge(pending)
        .expect("finish the pending purge");
    let report = store
        .delete_matching(&note_is(7))
        .expect("delete after the purge resolved");
    assert_eq!(report.deleted_ids().len(), 4);
}

// ---------------------------------------------------------------------------
// Crash sweep: a child process aborts before its k-th mutating filesystem
// operation inside delete_matching, for every k until one run completes.

#[cfg(unix)]
const CRASH_DIRECTORY: &str = "ZE217_CRASH_DIRECTORY";
#[cfg(unix)]
const CRASH_STEP: &str = "ZE217_CRASH_STEP";
#[cfg(unix)]
const COMPLETED: &str = "ZE217_DELETE_MATCHING_COMPLETED";

#[cfg(unix)]
#[derive(Default)]
struct AbortPlan {
    armed: AtomicBool,
    seen: AtomicU64,
    abort_at: AtomicU64,
}

#[cfg(unix)]
impl AbortPlan {
    fn step(&self) {
        if self.armed.load(Ordering::Acquire) {
            let seen = self.seen.fetch_add(1, Ordering::AcqRel) + 1;
            if seen == self.abort_at.load(Ordering::Acquire) {
                std::process::abort();
            }
        }
    }
}

#[cfg(unix)]
struct AbortingVfs(Arc<AbortPlan>);

#[cfg(unix)]
struct AbortingFile(Box<dyn VfsFile>, Arc<AbortPlan>);

#[cfg(unix)]
impl VfsFile for AbortingFile {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.1.step();
        self.0.append(bytes)
    }

    fn append_vectored(&mut self, buffers: &mut [std::io::IoSlice<'_>]) -> std::io::Result<()> {
        self.1.step();
        self.0.append_vectored(buffers)
    }

    fn sync(&self, kind: SyncKind) -> std::io::Result<()> {
        self.1.step();
        self.0.sync(kind)
    }
}

#[cfg(unix)]
impl Vfs for AbortingVfs {
    fn ensure_directory(&self, path: &Path, create: bool) -> std::io::Result<bool> {
        StdVfs.ensure_directory(path, create)
    }

    fn create_directory(&self, path: &Path) -> std::io::Result<()> {
        self.0.step();
        StdVfs.create_directory(path)
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
        self.0.step();
        StdVfs.write(path, bytes)
    }

    fn create_new(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.0.step();
        StdVfs.create_new(path, bytes)
    }

    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        self.0.step();
        Ok(Box::new(AbortingFile(
            StdVfs.open_append(path)?,
            Arc::clone(&self.0),
        )))
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.0.step();
        StdVfs.rename(from, to)
    }

    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        self.0.step();
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
        self.0.step();
        StdVfs.delete(path)
    }
}

#[cfg(unix)]
#[test]
#[ignore = "subprocess-only helper selected by the parent crash sweep"]
fn delete_matching_crash_child_helper() {
    let directory = std::env::var_os(CRASH_DIRECTORY)
        .map(PathBuf::from)
        .expect("crash child directory");
    let abort_at = std::env::var(CRASH_STEP)
        .expect("crash child step")
        .parse::<u64>()
        .expect("numeric crash step");
    let plan = Arc::new(AbortPlan::default());
    plan.abort_at.store(abort_at, Ordering::Release);
    let dependencies = StoreTestDependencies::new(
        Arc::new(AbortingVfs(Arc::clone(&plan))),
        Arc::new(SystemMonotonicClock),
    );
    let store = Store::open_with_test_dependencies(&directory, options(), dependencies)
        .expect("open crash child store");
    plan.armed.store(true, Ordering::Release);
    let report = store
        .delete_matching(&note_is(7))
        .expect("delete_matching in the crash child");
    plan.armed.store(false, Ordering::Release);
    assert_eq!(report.deleted_ids().len(), 4);
    println!("{COMPLETED}");
}

#[cfg(unix)]
fn copy_directory(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create crash run directory");
    for entry in fs::read_dir(from).expect("read fixture directory") {
        let path = entry.expect("fixture entry").path();
        let target = to.join(path.file_name().expect("fixture file name"));
        if path.is_dir() {
            copy_directory(&path, &target);
        } else {
            fs::copy(&path, &target).expect("copy fixture file");
        }
    }
}

#[cfg(unix)]
#[test]
fn a_crash_at_any_filesystem_step_leaves_all_or_none_deleted() {
    let root = tempdir().expect("crash sweep root");
    let fixture = root.path().join("fixture");
    {
        let store = Store::open(&fixture, options()).expect("open crash fixture");
        populate(&store);
        store.close().expect("close crash fixture");
    }
    let (mut none, mut all) = (0_u32, 0_u32);
    for step in 1_u64.. {
        assert!(step < 2_000, "delete_matching never completed in the sweep");
        let run = root.path().join(format!("run-{step}"));
        copy_directory(&fixture, &run);
        let output = Command::new(std::env::current_exe().expect("test executable"))
            .arg("--exact")
            .arg("delete_matching_crash_child_helper")
            .arg("--ignored")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(CRASH_DIRECTORY, &run)
            .env(CRASH_STEP, step.to_string())
            .output()
            .expect("spawn crash child");
        let completed = String::from_utf8_lossy(&output.stdout).contains(COMPLETED);
        if !completed {
            assert_eq!(
                output.status.signal(),
                Some(libc::SIGABRT),
                "step {step}: child neither completed nor aborted: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let reopened = Store::open(&run, options()).expect("reopen after crash");
        assert_eq!(count(&reopened, 8), 3, "step {step}: a survivor was lost");
        assert!(
            !run.join("purge.ze").exists(),
            "step {step}: reopen left the purge pending"
        );
        match count(&reopened, 7) {
            4 => none += 1,
            0 => {
                all += 1;
                assert_eq!(
                    marker_files(&run),
                    Vec::<PathBuf>::new(),
                    "step {step}: deleted text survived recovery"
                );
            }
            partial => panic!("step {step}: {partial} of 4 matches survived"),
        }
        reopened.close().expect("close recovered store");
        if completed {
            assert_eq!(count_after_completion(&run), 0);
            eprintln!("crash sweep: {step} runs, {none} none, {all} all");
            break;
        }
    }
    assert!(none > 0, "no crash step preceded the commit point");
    assert!(all > 1, "no crash step followed the commit point");
}

#[cfg(unix)]
fn count_after_completion(run: &Path) -> u64 {
    let store = Store::open(run, options()).expect("reopen completed run");
    count(&store, 7)
}

#[test]
fn delete_matching_after_sealed_delete_preserves_survivors_and_purges_history() {
    for seal_before_delete in [false, true] {
        let directory = tempdir().expect("store directory");
        let store = Store::open(directory.path(), options()).expect("open store");
        store
            .ingest(IngestBatch::new(vec![document(1, 1, 7, MARKER)]))
            .expect("old transcript");
        store.seal().expect("seal old transcript");
        store
            .ingest(IngestBatch::new(vec![document(
                2,
                1,
                7,
                "currenttranscript",
            )]))
            .expect("replacement transcript");
        store
            .delete(zeppelin_embed::ingest::DeleteBatch::new(vec![DocId::new(
                1,
            )]))
            .expect("delete old transcript");
        if seal_before_delete {
            store.seal().expect("seal replacement");
        }
        let report = store
            .delete_matching(&note_is(7))
            .expect("delete replacement");
        assert_eq!(report.deleted_ids(), &[DocId::new(2)]);
        store
            .ingest(IngestBatch::new(vec![document(3, 1, 8, SURVIVOR_MARKER)]))
            .expect("unrelated survivor");
        assert!(!marker_files(directory.path()).is_empty());
        let token = store.purge(&[DocId::new(1)]).expect("purge old ID");
        store
            .await_physical_purge(token)
            .expect("complete old ID purge");
        assert!(marker_files(directory.path()).is_empty());
        assert!(files_containing(directory.path(), b"currenttranscript").is_empty());
        assert_eq!(count(&store, 8), 1);
        store.close().expect("close");
        let reopened = Store::open(directory.path(), options()).expect("reopen");
        assert_eq!(count(&reopened, 7), 0);
        assert_eq!(count(&reopened, 8), 1);
    }
}
