//! Public delete-where crash probe used by the ingest/retention campaign.
//! Receipts are emitted by the armed VFS before each filesystem mutation.

use super::coverage::CoverageRegistry;
use std::fs;
use std::io::Write;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tempfile::tempdir;
use zeppelin_embed::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
use zeppelin_embed::lifecycle::{
    DocumentFields, OpenOptions, Store, StoreTestDependencies, SystemMonotonicClock,
};
use zeppelin_embed::meta::{Predicate, PredicateValue, TIMESTAMP_COLUMN};
use zeppelin_embed::vfs::{StdVfs, SyncKind, Vfs, VfsFile};
use zeppelin_embed_bench::harness_json::json;

fn predicate() -> Predicate {
    Predicate::Eq {
        column: TIMESTAMP_COLUMN,
        value: PredicateValue::I64(7),
    }
}
fn marker(seed: u64) -> String {
    format!("ze237purged{seed:016x}")
}
fn targets(seed: u64) -> Vec<DocId> {
    (1..=2 + u128::from(seed % 3)).map(DocId::new).collect()
}
fn document(id: DocId, timestamp: i64, text: &str) -> IngestDocument {
    IngestDocument::new(DocumentVersion::new(id, Revision::new(1)), vec![1.0])
        .with_timestamp(timestamp)
        .with_text(text)
        .with_metadata(text.as_bytes().to_vec())
}
fn populate(root: &Path, seed: u64) {
    let store = Store::open(root, OpenOptions::default()).expect("open delete-where fixture");
    let ids = targets(seed);
    let (active, sealed) = ids.split_last().expect("multiple targets");
    let mut docs: Vec<_> = sealed
        .iter()
        .map(|id| document(*id, 7, &marker(seed)))
        .collect();
    docs.push(document(DocId::new(100), 8, "ze237survivor"));
    store
        .ingest(IngestBatch::new(docs))
        .expect("sealed fixture");
    store.seal().expect("seal targets and survivor");
    store
        .ingest(IngestBatch::new(vec![document(*active, 7, &marker(seed))]))
        .expect("active target");
    store.close().expect("close fixture");
}
fn copy_directory(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create run directory");
    for entry in fs::read_dir(from).expect("read fixture") {
        let path = entry.expect("fixture entry").path();
        let target = to.join(path.file_name().expect("file name"));
        if path.is_dir() {
            copy_directory(&path, &target);
        } else {
            fs::copy(&path, &target).expect("copy fixture");
        }
    }
}
fn marker_hits(root: &Path, needle: &[u8]) -> usize {
    fs::read_dir(root)
        .expect("scan store files")
        .map(|entry| {
            let path = entry.expect("store file").path();
            if path.is_dir() {
                marker_hits(&path, needle)
            } else {
                usize::from(
                    fs::read(path)
                        .expect("read store bytes")
                        .windows(needle.len())
                        .any(|w| w == needle),
                )
            }
        })
        .sum()
}

#[derive(Clone, Debug)]
pub struct Observation {
    pub live_targets: Vec<u128>,
    pub survivors: Vec<u128>,
    pub marker_hits: usize,
    pub pending_before: bool,
    pub pending_after: bool,
    pub completed: bool,
}

pub fn check(seed: u64, observed: &Observation) -> Result<(), String> {
    let all: Vec<_> = targets(seed).iter().map(|id| id.get()).collect();
    if !observed.live_targets.is_empty() && observed.live_targets != all {
        return Err("delete-where exposed a partial target set".to_owned());
    }
    if observed.survivors != [100] {
        return Err("delete-where lost a survivor".to_owned());
    }
    if observed.pending_after {
        return Err("delete-where reopen left a pending intent".to_owned());
    }
    if (observed.completed || observed.pending_before) && !observed.live_targets.is_empty() {
        return Err("delete-where failed to complete a committed intent".to_owned());
    }
    if observed.live_targets.is_empty() && observed.marker_hits != 0 {
        return Err("delete-where left purged bytes on disk".to_owned());
    }
    if !observed.live_targets.is_empty() && observed.marker_hits == 0 {
        return Err("delete-where lost bytes before deletion".to_owned());
    }
    Ok(())
}

fn observe(root: &Path, seed: u64, completed: bool) -> Observation {
    let pending_before = root.join("purge.ze").exists();
    let store = Store::open(root, OpenOptions::default()).expect("recover delete-where");
    let ids = targets(seed);
    let documents = store
        .get_documents(&ids, DocumentFields::NONE)
        .expect("read target visibility");
    let live_targets = ids
        .iter()
        .zip(documents)
        .filter_map(|(id, doc)| doc.map(|_| id.get()))
        .collect();
    let survivor = store
        .get_documents(&[DocId::new(100)], DocumentFields::NONE)
        .expect("read survivor");
    let survivors = if survivor.iter().all(Option::is_some) {
        vec![100]
    } else {
        vec![]
    };
    let total = store
        .count_documents(None, None)
        .expect("count all live rows")
        .count;
    let result = Observation {
        live_targets,
        survivors,
        marker_hits: marker_hits(root, marker(seed).as_bytes()),
        pending_before,
        pending_after: root.join("purge.ze").exists(),
        completed,
    };
    assert_eq!(
        total as usize,
        result.live_targets.len() + result.survivors.len(),
        "unexpected live rows"
    );
    store.close().expect("close recovered store");
    result
}

fn child(root: &Path, trace: &Path, seed: u64, step: u64) -> (Vec<String>, bool) {
    let output = Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "adversarial::delete_where::delete_where_crash_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("ZE237_ROOT", root)
        .env("ZE237_TRACE", trace)
        .env("ZE237_SEED", seed.to_string())
        .env("ZE237_STEP", step.to_string())
        .output()
        .expect("spawn delete-where child");
    let completed = output.status.success()
        && String::from_utf8_lossy(&output.stdout).contains("ZE237_COMPLETE");
    if step == 0 {
        assert!(
            completed,
            "clean child failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    } else {
        assert_eq!(
            output.status.signal(),
            Some(6),
            "child missed crash: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let receipts = fs::read_to_string(trace)
        .expect("actual VFS receipts")
        .lines()
        .map(str::to_owned)
        .collect();
    (receipts, completed)
}

/// A clean control traces the whole operation. Each seed selects an actual
/// boundary from that trace; the targeted sweep covers all boundaries.
pub fn probe(
    seed: u64,
    sweep: bool,
    coverage: &mut CoverageRegistry,
) -> Result<Vec<String>, String> {
    let root = tempdir().expect("delete-where probe");
    let fixture = root.path().join("fixture");
    populate(&fixture, seed);
    assert!(
        marker_hits(&fixture, marker(seed).as_bytes()) > 0,
        "marker must exist before purge"
    );
    let clean = root.path().join("clean");
    copy_directory(&fixture, &clean);
    let (sites, completed) = child(&clean, &root.path().join("clean.trace"), seed, 0);
    check(seed, &observe(&clean, seed, completed))?;
    if sites.is_empty() {
        return Err("delete-where produced no VFS receipts".to_owned());
    }
    let selected = 1 + seed % sites.len() as u64;
    let steps: Vec<u64> = if sweep {
        (1..=sites.len() as u64).collect()
    } else {
        vec![selected]
    };
    let mut records = Vec::new();
    for step in steps {
        let run = root.path().join(format!("fault-{step}"));
        copy_directory(&fixture, &run);
        let (receipts, completed) = child(
            &run,
            &root.path().join(format!("fault-{step}.trace")),
            seed,
            step,
        );
        if receipts != sites[..step as usize] {
            return Err(format!("delete-where receipt prefix differs at {step}"));
        }
        let observation = observe(&run, seed, completed);
        check(seed, &observation)?;
        // A second writable reopen must preserve the recovered state.
        let second = observe(&run, seed, completed);
        check(seed, &second)?;
        if observation.live_targets != second.live_targets {
            return Err("delete-where recovery is not stable".to_owned());
        }
        coverage.hit("ingest.delete-where.checked");
        coverage.hit("campaign.op.ingest-retention.delete-where");
        coverage.hit(format!("ingest.delete-where.crash-step.{step}"));
        if observation.pending_before {
            coverage.hit("ingest.delete-where.pending-intent-recovered");
        }
        if observation.live_targets.is_empty() {
            coverage.hit("ingest.delete-where.all-deleted");
        } else {
            coverage.hit("ingest.delete-where.none-deleted");
        }
        records.push(json!({"checker_id":"delete-where.v1", "campaign":"ingest-retention", "operation":"delete-where",
            "seed":seed, "crash_step":step, "signal":6, "clean_sites":sites, "receipts":receipts,
            "expected_targets":targets(seed).iter().map(|id| id.get()).collect::<Vec<_>>(),
            "live_targets":observation.live_targets, "survivors":observation.survivors,
            "marker_hits":observation.marker_hits, "pending_before":observation.pending_before,
            "pending_after":observation.pending_after, "completed":observation.completed,
            "passed":true}).to_string());
    }
    Ok(records)
}

#[test]
#[ignore = "child-process-only delete-where fault adapter"]
fn delete_where_crash_child() {
    let root = PathBuf::from(std::env::var_os("ZE237_ROOT").expect("root"));
    let trace = PathBuf::from(std::env::var_os("ZE237_TRACE").expect("trace"));
    let seed = std::env::var("ZE237_SEED")
        .expect("seed")
        .parse::<u64>()
        .expect("numeric seed");
    let abort_at = std::env::var("ZE237_STEP")
        .expect("step")
        .parse::<u64>()
        .expect("numeric step");
    let plan = Arc::new(AbortPlan {
        armed: AtomicBool::new(false),
        seen: AtomicU64::new(0),
        abort_at,
        trace,
    });
    let dependencies = StoreTestDependencies::new(
        Arc::new(AbortingVfs(plan.clone())),
        Arc::new(SystemMonotonicClock),
    );
    let store = Store::open_with_test_dependencies(&root, OpenOptions::default(), dependencies)
        .expect("open child");
    plan.armed.store(true, Ordering::Release);
    let report = store.delete_matching(&predicate()).expect("delete where");
    plan.armed.store(false, Ordering::Release);
    assert_eq!(report.deleted_ids(), targets(seed));
    assert_eq!(
        store
            .count_documents(None, None)
            .expect("generation")
            .generation,
        report.generation()
    );
    println!("ZE237_COMPLETE");
}

struct AbortPlan {
    armed: AtomicBool,
    seen: AtomicU64,
    abort_at: u64,
    trace: PathBuf,
}

impl AbortPlan {
    fn step(&self, site: &str) {
        if self.armed.load(Ordering::Acquire) {
            let step = self.seen.fetch_add(1, Ordering::AcqRel) + 1;
            let mut file = fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&self.trace)
                .expect("open boundary receipt");
            writeln!(file, "{step}:{site}").expect("write boundary receipt");
            file.sync_all().expect("sync boundary receipt before crash");
            if step == self.abort_at {
                std::process::abort();
            }
        }
    }
}

struct AbortingVfs(Arc<AbortPlan>);

struct AbortingFile(Box<dyn VfsFile>, Arc<AbortPlan>);

impl VfsFile for AbortingFile {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.1.step("append");
        self.0.append(bytes)
    }

    fn append_vectored(&mut self, buffers: &mut [std::io::IoSlice<'_>]) -> std::io::Result<()> {
        self.1.step("append_vectored");
        self.0.append_vectored(buffers)
    }

    fn sync(&self, kind: SyncKind) -> std::io::Result<()> {
        self.1.step("sync");
        self.0.sync(kind)
    }
}

impl Vfs for AbortingVfs {
    fn ensure_directory(&self, path: &Path, create: bool) -> std::io::Result<bool> {
        StdVfs.ensure_directory(path, create)
    }

    fn create_directory(&self, path: &Path) -> std::io::Result<()> {
        self.0.step("create_directory");
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
        self.0.step("write");
        StdVfs.write(path, bytes)
    }

    fn create_new(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.0.step("create_new");
        StdVfs.create_new(path, bytes)
    }

    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        self.0.step("open_append");
        Ok(Box::new(AbortingFile(
            StdVfs.open_append(path)?,
            Arc::clone(&self.0),
        )))
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.0.step("rename");
        StdVfs.rename(from, to)
    }

    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        self.0.step("sync");
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
        self.0.step("delete");
        StdVfs.delete(path)
    }
}
