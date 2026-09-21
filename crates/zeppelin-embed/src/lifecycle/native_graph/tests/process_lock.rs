//! ZE-106: graph store lock modes proved through the real constructor.
//!
//! ZE-150 proved the `StoreLock` primitive with real child processes, but it
//! called `StoreLock::acquire_shared` directly. These tests drive
//! `Store::open_native_graph` instead, so the mode routing in
//! `acquire_native_graph_lock`, the `StoreBusy` mapping, and the read-only
//! recovery branch are all covered by the same two-process evidence.
//!
//! The child probe opens the store the way a separate application would: a
//! real process, the real constructor, and the real `StdVfs`. Fixtures are
//! built through `RecordingVfs`, which delegates every operation to `StdVfs`,
//! so the bytes a child opens are ordinary on-disk bytes.

use super::consolidation::{commit_maintenance, pending_reclaim_candidates};
use super::publication::{FaultPoint, RecordingVfs};
use super::tempfile;
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, Store, StoreErrorKind};
use crate::property_graph::query::runtime::{RuntimeContext, RuntimeLimits};
use crate::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use crate::property_graph::storage::GraphReadView;
use crate::property_graph::storage::tree::directory::{TreeError, TreeResources};
use crate::property_graph::{
    ApplicationKey, CanonicalContents, EntityId, EntityKind, GraphRevision, NodeId,
};
use crate::vfs::Vfs;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

const CHILD_HELPER: &str = "lifecycle::native_graph::tests::process_lock::ze106_child_helper";
const CHILD_TIMEOUT: Duration = Duration::from_secs(30);
const LINE_PREFIX: &str = "ZE106 ";

const CHILD_DIR: &str = "ZE_GRAPH_STORE_CHILD_DIR";
const CHILD_MODE: &str = "ZE_GRAPH_STORE_CHILD_MODE";
const CHILD_NODE: &str = "ZE_GRAPH_STORE_CHILD_NODE";

fn native_options() -> OpenOptions {
    OpenOptions::new()
        .with_durability(DurabilityMode::Durable, CommitTier::Durable)
        .with_max_resident_bytes(256 * 1024 * 1024)
}

fn read_only_options() -> OpenOptions {
    OpenOptions::read_only()
        .with_durability(DurabilityMode::Durable, CommitTier::Durable)
        .with_max_resident_bytes(256 * 1024 * 1024)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChildMode {
    ReadOnly,
    Writable,
}

impl ChildMode {
    const fn name(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::Writable => "writable",
        }
    }

    fn options(self) -> OpenOptions {
        match self {
            Self::ReadOnly => read_only_options(),
            Self::Writable => native_options(),
        }
    }
}

// ---------------------------------------------------------------------------
// Child probe: a real second process running the real constructor.
// ---------------------------------------------------------------------------

struct ChildProbe {
    child: Child,
    stdin: Option<ChildStdin>,
    _stdout: BufReader<ChildStdout>,
    pid: u32,
    mode: ChildMode,
    outcome: String,
}

impl ChildProbe {
    /// Returns the trailing detail field of an acknowledged open.
    fn assert_opened(&self) -> String {
        let expected = format!("{LINE_PREFIX}ACK {} {} ", self.pid, self.mode.name());
        assert!(
            self.outcome.starts_with(&expected),
            "child {} did not open the store {}: {}",
            self.pid,
            self.mode.name(),
            self.outcome
        );
        self.outcome
            .get(expected.len()..)
            .expect("acknowledgement detail")
            .to_owned()
    }

    /// Requires the store-busy refusal that `acquire_native_graph_lock` maps
    /// from a `WouldBlock` lock conflict.
    fn assert_store_busy(&mut self) {
        assert_eq!(
            self.outcome,
            format!("{LINE_PREFIX}BUSY {} {}", self.pid, self.mode.name()),
            "child {} did not report StoreBusy for its {} open: {}",
            self.pid,
            self.mode.name(),
            self.outcome
        );
        assert!(wait_bounded(&mut self.child).success());
    }

    fn release(mut self) -> u32 {
        let mut stdin = self.stdin.take().expect("held child stdin");
        stdin.write_all(b"release\n").expect("release child store");
        stdin.flush().expect("flush child release");
        drop(stdin);
        assert!(wait_bounded(&mut self.child).success());
        self.pid
    }

    /// Kills the child without letting it run `close()`, so only the operating
    /// system releases its ownership.
    fn kill(mut self) -> u32 {
        self.child.kill().expect("kill owning store child");
        assert!(!wait_bounded(&mut self.child).success());
        self.pid
    }
}

impl Drop for ChildProbe {
    fn drop(&mut self) {
        drop(self.stdin.take());
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn wait_bounded(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + CHILD_TIMEOUT;
    loop {
        match child.try_wait().expect("poll child") {
            Some(status) => return status,
            None if Instant::now() < deadline => std::thread::yield_now(),
            None => {
                child.kill().expect("kill timed-out child");
                return child.wait().expect("reap timed-out child");
            }
        }
    }
}

fn spawn_probe(directory: &Path, mode: ChildMode, node: Option<NodeId>) -> ChildProbe {
    let mut command = Command::new(std::env::current_exe().expect("unit-test executable"));
    command
        .args(["--exact", CHILD_HELPER, "--nocapture"])
        .env(CHILD_DIR, directory)
        .env(CHILD_MODE, mode.name())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if let Some(node) = node {
        command.env(CHILD_NODE, node.get().to_string());
    } else {
        command.env_remove(CHILD_NODE);
    }
    let mut child = command.spawn().expect("spawn native graph store child");
    let pid = child.id();
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().expect("child stdout");
    let (line_tx, line_rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = line_tx.send((format!("{LINE_PREFIX}EOF"), reader));
                    break;
                }
                Ok(_) if line.starts_with(LINE_PREFIX) => {
                    let _ = line_tx.send((line.trim_end().to_owned(), reader));
                    break;
                }
                Ok(_) => {}
                Err(error) => {
                    let _ = line_tx.send((format!("{LINE_PREFIX}READ_ERROR {error}"), reader));
                    break;
                }
            }
        }
    });
    let (outcome, stdout) = match line_rx.recv_timeout(CHILD_TIMEOUT) {
        Ok(result) => result,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child {pid} acknowledgement timed out: {error}");
        }
    };
    ChildProbe {
        child,
        stdin,
        _stdout: stdout,
        pid,
        mode,
        outcome,
    }
}

struct ObserveNode {
    node: NodeId,
}

impl super::super::NativeReadConsumer<Option<u64>> for ObserveNode {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<Option<u64>, TreeError> {
        let mut resources = TreeResources::for_query(runtime)?;
        Ok(view
            .lookup_node(self.node, &mut resources)?
            .map(|node| node.record().revision().get()))
    }
}

fn observe_node(store: &Store, node: NodeId) -> Option<u64> {
    store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            8 * 1024 * 1024,
            32,
            ObserveNode { node },
        )
        .expect("observe native node")
}

/// The child process. It opens the real store through the real constructor,
/// performs a real read when a node is named, acknowledges, and only then
/// waits for the parent's release so ownership is held across the assertions.
#[test]
fn ze106_child_helper() {
    let Some(directory) = std::env::var_os(CHILD_DIR).map(PathBuf::from) else {
        return;
    };
    let mode = match std::env::var(CHILD_MODE)
        .expect("native graph child mode")
        .as_str()
    {
        "read_only" => ChildMode::ReadOnly,
        "writable" => ChildMode::Writable,
        other => panic!("unknown native graph child mode {other}"),
    };
    let node = std::env::var(CHILD_NODE).ok().map(|value| {
        NodeId::new(value.parse::<u128>().expect("child node identity"))
            .expect("nonzero child node identity")
    });
    let report = |line: String| {
        println!("{line}");
        std::io::stdout().flush().expect("flush child line");
    };
    match Store::open_native_graph(&directory, mode.options(), None) {
        Ok(store) => {
            let detail = match node {
                Some(node) => match observe_node(&store, node) {
                    Some(revision) => format!("revision={revision}"),
                    None => "revision=absent".to_owned(),
                },
                None => "-".to_owned(),
            };
            report(format!(
                "{LINE_PREFIX}ACK {} {} {detail}",
                std::process::id(),
                mode.name()
            ));
            let mut release = [0_u8; 1];
            let _ = std::io::stdin().read(&mut release);
            store.close().expect("close child store");
        }
        Err(super::super::NativeGraphError::Store(error))
            if error.kind() == StoreErrorKind::StoreBusy =>
        {
            report(format!(
                "{LINE_PREFIX}BUSY {} {}",
                std::process::id(),
                mode.name()
            ));
        }
        Err(error) => {
            report(format!(
                "{LINE_PREFIX}ERROR {} {} {error:?}",
                std::process::id(),
                mode.name()
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// Fixtures.
// ---------------------------------------------------------------------------

struct Fixture {
    _parent: tempfile::TempDir,
    path: PathBuf,
    vfs: Arc<RecordingVfs>,
}

fn create_fixture(name: &str) -> (Fixture, Store) {
    let parent = tempfile::tempdir().expect("ze106 fixture parent");
    let path = parent.path().join(name);
    let vfs = Arc::new(RecordingVfs::default());
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let store = Store::create_native_graph_with_infrastructure(
        &path,
        native_options(),
        None,
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh ze106 native store");
    (
        Fixture {
            _parent: parent,
            path,
            vfs,
        },
        store,
    )
}

fn create_node(store: &Store, name: &str) -> NodeId {
    let image = CanonicalContents::node(&mut [], &mut [], Some(name), None).expect("node image");
    let receipts = store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze106", name).expect("node key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("durable node commit");
    match receipts[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("node receipt domain"),
    }
}

// ---------------------------------------------------------------------------
// On-disk oracle.
// ---------------------------------------------------------------------------

/// One directory entry's complete observable on-disk state.
///
/// `writer.lock` carries no content hash: opening it independently would drop
/// this process's own POSIX record lock, so only its metadata is observed.
#[derive(Debug, Eq, PartialEq)]
struct EntryImage {
    length: u64,
    modified: Option<SystemTime>,
    content: Option<u64>,
}

fn directory_oracle(path: &Path) -> BTreeMap<OsString, EntryImage> {
    std::fs::read_dir(path)
        .expect("read store directory")
        .map(|entry| {
            let entry = entry.expect("store directory entry");
            let name = entry.file_name();
            let metadata = entry.metadata().expect("store entry metadata");
            let content = if name == crate::lifecycle::lock::STORE_LOCK_FILE {
                None
            } else {
                Some(xxhash_rust::xxh3::xxh3_64(
                    &std::fs::read(entry.path()).expect("read store entry"),
                ))
            };
            (
                name,
                EntryImage {
                    length: metadata.len(),
                    modified: metadata.modified().ok(),
                    content,
                },
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

/// A live writable owner refuses a separate process's read-only open. The
/// refusal is `StoreBusy`, not an I/O error and not a silent degradation.
#[test]
fn ze106_writable_parent_refuses_read_only_child_with_store_busy() {
    let (fixture, store) = create_fixture("writable-owner");
    let node = create_node(&store, "owned");

    let mut reader = spawn_probe(&fixture.path, ChildMode::ReadOnly, Some(node));
    reader.assert_store_busy();
    let mut second_writer = spawn_probe(&fixture.path, ChildMode::Writable, None);
    second_writer.assert_store_busy();

    // The refusals cost the owner nothing: it still owns a writable store.
    assert_eq!(observe_node(&store, node), Some(1));
    store.close().expect("close writable owner");

    // Ownership is released by that close, so the same open now succeeds.
    let reader = spawn_probe(&fixture.path, ChildMode::ReadOnly, Some(node));
    assert_eq!(reader.assert_opened(), "revision=1");
    let reader_pid = reader.release();
    eprintln!("ze106 read-only child admitted after writer close: pid={reader_pid}");
}

/// Two separate read-only processes hold the store at the same time, and
/// together they refuse a third process's writable open.
#[test]
fn ze106_two_read_only_children_coexist_and_refuse_writable_child() {
    let (fixture, store) = create_fixture("shared-readers");
    let node = create_node(&store, "shared");
    store.close().expect("close seeding writer");

    let first = spawn_probe(&fixture.path, ChildMode::ReadOnly, Some(node));
    assert_eq!(first.assert_opened(), "revision=1");
    let second = spawn_probe(&fixture.path, ChildMode::ReadOnly, Some(node));
    assert_eq!(second.assert_opened(), "revision=1");

    let mut writer = spawn_probe(&fixture.path, ChildMode::Writable, None);
    writer.assert_store_busy();

    // One reader leaving is not enough; the other still holds the store.
    let first_pid = first.release();
    let mut writer = spawn_probe(&fixture.path, ChildMode::Writable, None);
    writer.assert_store_busy();

    let second_pid = second.release();
    let writer = spawn_probe(&fixture.path, ChildMode::Writable, None);
    assert_eq!(writer.assert_opened(), "-");
    let writer_pid = writer.release();
    eprintln!(
        "ze106 coexisting read-only child pids: first={first_pid} second={second_pid}; writable={writer_pid}"
    );
}

/// A killed writable owner never runs `close()`. Only the operating system
/// releases its ownership, and the next open must still be admitted.
#[test]
fn ze106_killed_writable_child_releases_ownership_for_read_only_reopen() {
    let (fixture, store) = create_fixture("killed-writer");
    let node = create_node(&store, "survivor");
    store.close().expect("close seeding writer");

    let writer = spawn_probe(&fixture.path, ChildMode::Writable, Some(node));
    assert_eq!(writer.assert_opened(), "revision=1");
    let mut blocked = spawn_probe(&fixture.path, ChildMode::ReadOnly, Some(node));
    blocked.assert_store_busy();

    let killed_pid = writer.kill();

    let reader = spawn_probe(&fixture.path, ChildMode::ReadOnly, Some(node));
    assert_eq!(reader.assert_opened(), "revision=1");
    let reader_pid = reader.release();

    // A writable open after the kill is admitted too, so nothing leaked a
    // stale shared claim either.
    let rewriter = spawn_probe(&fixture.path, ChildMode::Writable, Some(node));
    assert_eq!(rewriter.assert_opened(), "revision=1");
    let rewriter_pid = rewriter.release();
    eprintln!(
        "ze106 killed writable child pid={killed_pid}; readmitted read-only={reader_pid} writable={rewriter_pid}"
    );
}

/// A read-only open of a store that still owes recovery work performs that
/// recovery in memory only. The complete on-disk image is byte-identical
/// across the read-only process's whole lifetime, and a writable open
/// afterwards proves the pending work was genuinely outstanding.
#[test]
fn ze106_read_only_child_recovers_torn_tail_and_pending_intent_without_disk_mutation() {
    let (fixture, store) = create_fixture("read-only-recovery");
    let node = create_node(&store, "recovered");

    // Build reclaimable history, then leave one durable pending reclaim intent
    // that no writable owner has resumed.
    let first = store
        .admit_native_graph_maintenance()
        .expect("history admission");
    store
        .commit_native_graph_maintenance(&first, &QueryControl::Cancel(CancelToken::new()))
        .expect("history replacement");
    drop(first);
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .expect("checkpoint history");
    commit_maintenance(&store).expect("durable pending reclaim intent");
    let candidates = {
        let lease = store.admit_native_read().expect("pending intent reader");
        pending_reclaim_candidates(&store, &lease)
    };
    assert!(
        !candidates.is_empty(),
        "fixture must leave at least one pending reclaim candidate"
    );

    // Tear the WAL tail with a real partial append inside a real commit.
    let wal_before = {
        let guard = store.native_graph.writer.lock().expect("native writer");
        guard.as_ref().expect("installed writer").wal.path.clone()
    };
    let complete_prefix = std::fs::read(&wal_before).expect("complete WAL prefix");
    let torn_image = CanonicalContents::node(&mut [], &mut [], Some("torn"), None).expect("image");
    fixture.vfs.arm_fault(FaultPoint::PartialAppend);
    assert!(matches!(
        store.apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze106", "torn").expect("torn key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&torn_image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        ),
        Err(super::super::NativeGraphError::CommitIndeterminate { .. })
    ));
    fixture.vfs.assert_fired_once();
    let torn_bytes = std::fs::read(&wal_before).expect("torn WAL");
    assert!(torn_bytes.starts_with(&complete_prefix));
    assert!(torn_bytes.len() > complete_prefix.len());
    store.close().expect("close store owing recovery");
    drop(store);

    // The fixture genuinely owes work: every pending candidate is still on
    // disk, and the WAL still carries the torn tail.
    for candidate in &candidates {
        assert!(
            crate::property_graph::storage::allocation::artifact_path(
                &fixture.path,
                candidate.artifact
            )
            .exists(),
            "pending reclaim candidate was already swept before the read-only open"
        );
    }

    let before = directory_oracle(&fixture.path);
    let reader = spawn_probe(&fixture.path, ChildMode::ReadOnly, Some(node));
    assert_eq!(reader.assert_opened(), "revision=1");
    // Observed while the read-only process still holds the store open.
    assert_eq!(directory_oracle(&fixture.path), before);
    let reader_pid = reader.release();
    let after = directory_oracle(&fixture.path);
    assert_eq!(
        after, before,
        "a read-only open truncated, wrote, or unlinked store bytes"
    );

    // Positive control: the very same on-disk state, opened writable, does
    // resume and does unlink. The read-only result above is therefore a
    // property of the access mode, not of an already-clean fixture.
    let resumed =
        Store::open_native_graph(&fixture.path, native_options(), None).expect("writable resume");
    assert_eq!(observe_node(&resumed, node), Some(1));
    let wal_after = {
        let guard = resumed.native_graph.writer.lock().expect("resumed writer");
        guard
            .as_ref()
            .expect("resumed writer state")
            .wal
            .path
            .clone()
    };
    assert_ne!(
        wal_after, wal_before,
        "a writable resume of a torn tail must rotate the WAL"
    );
    resumed.close().expect("close resumed store");
    for candidate in &candidates {
        assert!(
            !crate::property_graph::storage::allocation::artifact_path(
                &fixture.path,
                candidate.artifact
            )
            .exists(),
            "writable resume left a pending reclaim candidate on disk"
        );
    }
    assert_ne!(
        directory_oracle(&fixture.path),
        before,
        "the writable resume must change the store directory"
    );
    eprintln!(
        "ze106 read-only recovery child pid={reader_pid}; pending candidates={}; entries={}",
        candidates.len(),
        before.len()
    );
}
