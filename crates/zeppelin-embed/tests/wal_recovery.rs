#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Barrier;
use std::thread;
use std::time::Instant;

#[cfg(unix)]
use std::fs::OpenOptions;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
#[cfg(unix)]
use std::process::{Command, Stdio};

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use tempfile::tempdir;
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::lock::{STORE_LOCK_FILE, StoreLock};
use zeppelin_embed::manifest::io::{DurableLog, MANIFEST_FILE, load_manifest};
use zeppelin_embed::manifest::{EpochMeta, Manifest, ManifestError, encode_manifest};
use zeppelin_embed::meta::Schema;
use zeppelin_embed::vfs::StdVfs;
use zeppelin_embed::vfs::{CountingVfs, SyncKind, Vfs, VfsFile};
use zeppelin_embed::wal::{LogSeq, WalReader, WalWriter};

#[path = "../src/vfs/fault.rs"]
mod fault_support;

use fault_support::{BlockingVfs, FaultImage, FaultVfs};

#[path = "../src/vfs/crash.rs"]
#[allow(dead_code)]
mod crash_support;

use crash_support::{CrashOperation, CrashStateClass, CrashStateKind, CrashVfs, MemoryVfs};

const WAL_PATH: &str = "/wal-recovery/wal.ze";

fn policy(tier: CommitTier) -> DurabilityPolicy {
    DurabilityPolicy::new(DurabilityMode::Durable, tier).expect("supported policy")
}

fn recovered_sequences(image: &FaultImage) -> Vec<u64> {
    WalReader::open(image, Path::new(WAL_PATH))
        .expect("recovery needs no manual steps")
        .records()
        .iter()
        .map(|record| record.seq.get())
        .collect()
}

#[derive(Clone, Copy, Debug)]
enum Workload {
    Ingest,
    Seal,
    Publish,
}

impl Workload {
    const fn records(self) -> usize {
        match self {
            Self::Ingest => 2,
            Self::Seal => 3,
            Self::Publish => 4,
        }
    }
}

fn operation_boundary(kind: &CrashStateKind) -> usize {
    match kind {
        CrashStateKind::Prefix {
            completed_operations,
        } => *completed_operations,
        CrashStateKind::TornWrite {
            operation_index, ..
        }
        | CrashStateKind::ExtendedWithGarbage {
            operation_index, ..
        }
        | CrashStateKind::ExtendedWithZeros {
            operation_index, ..
        }
        | CrashStateKind::InteriorDamage {
            operation_index, ..
        } => *operation_index,
        CrashStateKind::ReorderedWrites {
            through_operation, ..
        } => *through_operation,
        CrashStateKind::RenameWithOldContent { operation_index } => {
            operation_index.saturating_add(1)
        }
    }
}

fn completed_full_groups(kind: &CrashStateKind, operations: &[CrashOperation]) -> usize {
    operations
        .iter()
        .take(operation_boundary(kind))
        .filter(|operation| {
            matches!(
                operation,
                CrashOperation::Sync {
                    kind: SyncKind::Full,
                    ..
                }
            )
        })
        .count()
}

fn crash_class_counts(states: &crash_support::CrashStates) -> BTreeMap<CrashStateClass, usize> {
    let mut counts = BTreeMap::new();
    for state in states.iter() {
        *counts.entry(state.kind().class()).or_insert(0) += 1;
    }
    counts
}

#[test]
fn wal_preallocation_crash_state_class_delta_is_exact() {
    let recorder = CrashVfs::new(MemoryVfs::new()).expect("recorder");
    let writer = WalWriter::create(
        &recorder,
        Path::new(WAL_PATH),
        LogSeq::new(1),
        policy(CommitTier::Ordered),
    )
    .expect("writer");
    writer.commit_durable(1, &[0x5a; 32]).expect("commit");
    let states = recorder.crash_states().expect("states");
    let without = crash_class_counts(&states);
    let without_vector = [
        CrashStateClass::Prefix,
        CrashStateClass::TornWrite,
        CrashStateClass::ExtendedWithGarbage,
        CrashStateClass::ExtendedWithZeros,
        CrashStateClass::InteriorDamage,
        CrashStateClass::ReorderedWrites,
        CrashStateClass::RenameWithOldContent,
    ]
    .map(|class| without.get(&class).copied().unwrap_or(0));

    // Once a finite-capacity WAL has been preallocated and zero-sized to that
    // capacity, a later append is an overwrite. Prefix/suffix and interior
    // overwrite damage remain; allocation-extension garbage is impossible and
    // zero-tail extension collapses into the existing prefix-overwrite state.
    let preallocated_states = states
        .iter()
        .filter(|state| {
            !matches!(
                state.kind().class(),
                CrashStateClass::ExtendedWithGarbage | CrashStateClass::ExtendedWithZeros
            )
        })
        .collect::<Vec<_>>();
    let mut with = BTreeMap::new();
    for state in &preallocated_states {
        *with.entry(state.kind().class()).or_insert(0) += 1;
    }
    let with_vector = [
        CrashStateClass::Prefix,
        CrashStateClass::TornWrite,
        CrashStateClass::ExtendedWithGarbage,
        CrashStateClass::ExtendedWithZeros,
        CrashStateClass::InteriorDamage,
        CrashStateClass::ReorderedWrites,
        CrashStateClass::RenameWithOldContent,
    ]
    .map(|class| with.get(&class).copied().unwrap_or(0));
    let with_total = preallocated_states.len();
    eprintln!(
        "wal_preallocation without_total={} without={without_vector:?} with_total={with_total} with={with_vector:?}",
        states.len()
    );
    assert_eq!(
        (states.len(), without_vector, with_total, with_vector),
        (54, [4, 20, 10, 10, 10, 0, 0], 34, [4, 20, 0, 0, 10, 0, 0],),
        "finite-capacity preallocation model must expose exact class delta"
    );
}

#[test]
fn exact_flush_counts_per_tier_and_workload() {
    let mut actual = Vec::new();
    for tier in [CommitTier::None, CommitTier::Ordered, CommitTier::Durable] {
        for workload in [Workload::Ingest, Workload::Seal, Workload::Publish] {
            let counting = CountingVfs::new(MemoryVfs::new());
            let writer =
                WalWriter::create(&counting, Path::new(WAL_PATH), LogSeq::new(1), policy(tier))
                    .expect("writer");
            for index in 0..workload.records() {
                writer
                    .commit_durable(index as u16 + 1, &[index as u8; 32])
                    .expect("commit");
            }
            let row = (
                tier,
                workload.records(),
                counting.append_calls(),
                counting.handle_barrier_sync_calls(),
                counting.handle_full_sync_calls(),
            );
            eprintln!(
                "wal_flush_counts tier={tier:?} workload={workload:?} records={} appends={} barrier={} full={}",
                row.1, row.2, row.3, row.4
            );
            actual.push(row);
        }
    }
    assert_eq!(
        actual,
        vec![
            (CommitTier::None, 2, 2, 0, 0),
            (CommitTier::None, 3, 3, 0, 0),
            (CommitTier::None, 4, 4, 0, 0),
            (CommitTier::Ordered, 2, 2, 2, 0),
            (CommitTier::Ordered, 3, 3, 3, 0),
            (CommitTier::Ordered, 4, 4, 4, 0),
            (CommitTier::Durable, 2, 2, 0, 2),
            (CommitTier::Durable, 3, 3, 0, 3),
            (CommitTier::Durable, 4, 4, 0, 4),
        ],
        "every workload needs exact append and SyncKind counts"
    );
}

#[test]
fn recovery_invariant_all_tiers_x_ingest_seal_publish() {
    for tier in [CommitTier::None, CommitTier::Ordered, CommitTier::Durable] {
        for workload in [Workload::Ingest, Workload::Seal, Workload::Publish] {
            let recorder = CrashVfs::new(MemoryVfs::new()).expect("recorder");
            let writer =
                WalWriter::create(&recorder, Path::new(WAL_PATH), LogSeq::new(1), policy(tier))
                    .expect("writer");
            for index in 0..workload.records() {
                writer
                    .commit_durable(index as u16 + 1, &[index as u8; 32])
                    .expect("commit");
            }
            let operations = recorder.operations().expect("operations");
            let states = recorder.crash_states().expect("states");
            assert!(!states.was_capped(), "{tier:?} {workload:?} was capped");
            let committed = (1..=workload.records() as u64).collect::<Vec<_>>();
            for state in states.iter() {
                let recovered = WalReader::open(state.vfs(), Path::new(WAL_PATH));
                let actual = match recovered {
                    Ok(reader) => {
                        let sequences = reader
                            .records()
                            .iter()
                            .map(|record| record.seq.get())
                            .collect::<Vec<_>>();
                        let is_prefix = committed.starts_with(&sequences);
                        let required = if tier == CommitTier::Durable {
                            completed_full_groups(state.kind(), &operations)
                        } else {
                            0
                        };
                        if is_prefix && sequences.len() >= required {
                            "prefix".to_owned()
                        } else {
                            format!("non-prefix {sequences:?}, required durable groups {required}")
                        }
                    }
                    Err(error) => format!("recovery error requiring manual steps: {error}"),
                };
                assert_eq!(
                    actual,
                    "prefix",
                    "tier={tier:?} workload={workload:?} state={:?}",
                    state.kind()
                );
            }
            eprintln!(
                "wal_recovery tier={tier:?} workload={workload:?} states={} committed={}",
                states.len(),
                committed.len()
            );
        }
    }
}

#[test]
fn real_durable_log_refuses_a_snapshot_ahead_of_replay() {
    let store = MemoryVfs::new();
    let writer = WalWriter::create(
        &store,
        Path::new(WAL_PATH),
        LogSeq::new(1),
        policy(CommitTier::Durable),
    )
    .expect("writer");
    writer.commit_durable(1, b"one").expect("commit");
    let reader = WalReader::open(&store, Path::new(WAL_PATH)).expect("reader");
    assert_eq!(reader.durable_end(), 1);

    let manifest = Manifest {
        generation: 1,
        log_seq: 2,
        segments: Vec::new(),
        epochs: vec![EpochMeta {
            id: 1,
            model: "model".to_owned(),
            tokenizer: "tokenizer".to_owned(),
        }],
        schema: Schema::new(Vec::new()).expect("schema"),
    };
    let path = Path::new("/wal-recovery").join(MANIFEST_FILE);
    store
        .insert(&path, encode_manifest(&manifest).expect("manifest bytes"))
        .expect("manifest");
    let actual = load_manifest(&store, &path, reader.durable_end())
        .map(|_| "accepted".to_owned())
        .unwrap_or_else(|error| match error {
            ManifestError::AheadOfLog { snapshot, durable } => {
                format!("AheadOfLog({snapshot}, {durable})")
            }
            other => format!("unexpected {other}"),
        });
    assert_eq!(actual, "AheadOfLog(2, 1)");
}

#[cfg(unix)]
fn parse_child_tier(value: &str) -> CommitTier {
    match value {
        "none" => CommitTier::None,
        "ordered" => CommitTier::Ordered,
        "durable" => CommitTier::Durable,
        other => panic!("unknown child tier {other}"),
    }
}

#[cfg(unix)]
fn tier_name(tier: CommitTier) -> &'static str {
    match tier {
        CommitTier::None => "none",
        CommitTier::Ordered => "ordered",
        CommitTier::Durable => "durable",
    }
}

#[cfg(unix)]
fn flock(file: &std::fs::File, operation: libc::c_int) -> std::io::Result<()> {
    let result = unsafe {
        // SAFETY: `file` owns a live descriptor for the duration of the call.
        libc::flock(file.as_raw_fd(), operation)
    };
    if result == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[test]
#[ignore = "spawned only by kill9_recovery_requires_zero_manual_cleanup"]
#[cfg(unix)]
fn child_process() {
    let directory = PathBuf::from(std::env::var_os("ZE_KILL9_CHILD_DIR").expect("child dir"));
    let tier = parse_child_tier(&std::env::var("ZE_KILL9_CHILD_TIER").expect("child tier"));
    let _store_lock = StoreLock::acquire(&directory).expect("lock writer fd");
    std::fs::write(directory.join("ready"), b"ready").expect("ready marker");

    let writer = WalWriter::create(
        &StdVfs,
        &directory.join("wal.ze"),
        LogSeq::new(1),
        policy(tier),
    )
    .expect("child writer");
    let mut sequence = 1_u64;
    loop {
        writer
            .commit_durable(1, &sequence.to_le_bytes())
            .expect("child commit");
        sequence = sequence.saturating_add(1);
    }
}

#[test]
#[ignore = "real-process SIGKILL loop"]
#[cfg(unix)]
fn kill9_recovery_requires_zero_manual_cleanup() {
    let iterations = std::env::var("ZE_KILL9_ITERS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(6);
    let tiers = [CommitTier::None, CommitTier::Ordered, CommitTier::Durable];
    let mut rng = ChaCha8Rng::seed_from_u64(0x0008_b2b0_0000_0001);
    let mut manual_cleanup = 0_usize;

    for iteration in 0..iterations {
        let tier = tiers[iteration % tiers.len()];
        let directory = tempdir().expect("kill9 tempdir");
        let mut child = Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", "child_process", "--ignored"])
            .env("ZE_KILL9_CHILD_DIR", directory.path())
            .env("ZE_KILL9_CHILD_TIER", tier_name(tier))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn child");

        let ready = directory.path().join("ready");
        loop {
            if ready.exists() {
                break;
            }
            if let Some(status) = child.try_wait().expect("poll child") {
                panic!("child exited before SIGKILL: {status}");
            }
            thread::yield_now();
        }
        for _ in 0..rng.random_range(0..=256_u16) {
            thread::yield_now();
        }
        let killed = unsafe {
            // SAFETY: the pid belongs to the live child process above.
            libc::kill(child.id() as libc::pid_t, libc::SIGKILL)
        };
        assert_eq!(killed, 0, "iteration={iteration} tier={tier:?} SIGKILL");
        let status = child.wait().expect("wait child");
        assert_eq!(
            status.signal(),
            Some(libc::SIGKILL),
            "iteration={iteration} tier={tier:?} child status"
        );

        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(directory.path().join(STORE_LOCK_FILE))
            .expect("reopen stale lock path");
        if flock(&lock, libc::LOCK_EX | libc::LOCK_NB).is_err() {
            manual_cleanup = manual_cleanup.saturating_add(1);
        }
        let reader = WalReader::open(&StdVfs, &directory.path().join("wal.ze"))
            .expect("reopen without manual WAL cleanup");
        let sequences = reader
            .records()
            .iter()
            .map(|record| record.seq.get())
            .collect::<Vec<_>>();
        assert!(
            sequences.iter().copied().eq(1..=sequences.len() as u64),
            "iteration={iteration} tier={tier:?} recovered non-prefix {sequences:?}"
        );
        flock(&lock, libc::LOCK_UN).expect("unlock parent fd");
    }

    eprintln!("kill9 iterations={iterations} manual_cleanup={manual_cleanup}");
    assert_eq!(manual_cleanup, 0, "stale flock required manual cleanup");
}

#[test]
#[cfg(unix)]
fn store_lock_rejects_a_second_writer_and_releases_on_drop() {
    let directory = tempdir().expect("lock tempdir");
    let first = StoreLock::acquire(directory.path()).expect("first writer");
    let second = StoreLock::acquire(directory.path()).expect_err("second writer rejected");
    let actual = match second {
        zeppelin_embed::lifecycle::lock::StoreLockError::Io { source, .. } => source.kind(),
    };
    assert_eq!(actual, std::io::ErrorKind::WouldBlock);
    drop(first);
    StoreLock::acquire(directory.path()).expect("kernel released lock on close");
}

#[test]
#[ignore = "local evidence run only"]
fn evidence_ingest_throughput_per_tier() {
    const THREADS: usize = 16;
    const DOCUMENTS: usize = 256;
    const PAYLOAD_BYTES: usize = 1_024;

    for tier in [CommitTier::None, CommitTier::Ordered, CommitTier::Durable] {
        let directory = tempdir().expect("throughput tempdir");
        let writer = Arc::new(
            WalWriter::create(
                &StdVfs,
                &directory.path().join("wal.ze"),
                LogSeq::new(1),
                policy(tier),
            )
            .expect("writer"),
        );
        let start_gate = Arc::new(Barrier::new(THREADS + 1));
        let workers = (0..THREADS)
            .map(|worker| {
                let worker_writer = Arc::clone(&writer);
                let worker_gate = Arc::clone(&start_gate);
                thread::spawn(move || {
                    let payload = [worker as u8; PAYLOAD_BYTES];
                    worker_gate.wait();
                    for _ in 0..DOCUMENTS / THREADS {
                        worker_writer
                            .commit_durable(1, &payload)
                            .expect("throughput commit");
                    }
                })
            })
            .collect::<Vec<_>>();
        let started = Instant::now();
        start_gate.wait();
        for worker in workers {
            worker.join().expect("throughput worker");
        }
        let elapsed = started.elapsed();
        let stats = writer.stats().expect("stats");
        let docs_per_second = DOCUMENTS as f64 / elapsed.as_secs_f64();
        let mut distribution = BTreeMap::new();
        for group_size in &stats.group_sizes {
            *distribution.entry(*group_size).or_insert(0_usize) += 1;
        }
        eprintln!(
            "wal_ingest tier={tier:?} documents={DOCUMENTS} payload_bytes={PAYLOAD_BYTES} threads={THREADS} elapsed_ns={} docs_per_second={docs_per_second:.3} flushes={} group_size_distribution={distribution:?} group_sizes={:?}",
            elapsed.as_nanos(),
            stats.group_sizes.len(),
            stats.group_sizes
        );
        assert_eq!(writer.visible_records().expect("visible").len(), DOCUMENTS);
        assert_eq!(stats.durable_end, DOCUMENTS as u64);
    }
}

#[test]
fn none_loses_tail_under_power_cut_and_nothing_under_app_crash() {
    let fault = FaultVfs::new();
    let writer = WalWriter::create(
        &fault,
        Path::new(WAL_PATH),
        LogSeq::new(1),
        policy(CommitTier::None),
    )
    .expect("writer");
    writer.commit_durable(1, b"visible").expect("commit");

    assert_eq!(
        recovered_sequences(&fault.application_crash().expect("app crash")),
        vec![1],
        "none must retain page-cache bytes across an application crash"
    );
    assert_eq!(
        recovered_sequences(&fault.power_cut().expect("power cut")),
        Vec::<u64>::new(),
        "none must be able to lose its unsynchronized tail on power loss"
    );
}

#[test]
fn ordered_never_recovers_out_of_order_and_may_lose_the_tail() {
    let fault = FaultVfs::new();
    let writer = WalWriter::create(
        &fault,
        Path::new(WAL_PATH),
        LogSeq::new(1),
        policy(CommitTier::Ordered),
    )
    .expect("writer");
    writer.commit_durable(1, b"one").expect("first commit");
    writer.commit_durable(1, b"two").expect("second commit");

    let recovered = recovered_sequences(&fault.power_cut().expect("power cut"));
    assert_eq!(
        recovered,
        Vec::<u64>::new(),
        "scripted power loss drops the ordered tail without exposing a later record"
    );
}

#[test]
fn durable_retains_every_group_whose_flush_returned() {
    let fault = FaultVfs::new();
    let writer = WalWriter::create(
        &fault,
        Path::new(WAL_PATH),
        LogSeq::new(1),
        policy(CommitTier::Durable),
    )
    .expect("writer");
    writer.commit_durable(1, b"one").expect("first commit");
    writer.commit_durable(1, b"two").expect("second commit");

    assert_eq!(
        recovered_sequences(&fault.power_cut().expect("power cut")),
        vec![1, 2],
        "full-sync groups that returned must survive power loss"
    );
}

#[test]
fn n_appends_produce_ceil_n_over_group_flushes() {
    let blocking = BlockingVfs::new(FaultVfs::new());
    blocking.block_next_syncs(1).expect("arm first barrier");
    let counting = CountingVfs::new(blocking.clone());
    let writer = Arc::new(
        WalWriter::create_with_max_group_bytes(
            &counting,
            Path::new(WAL_PATH),
            LogSeq::new(1),
            policy(CommitTier::Ordered),
            128,
        )
        .expect("writer"),
    );

    let leader_writer = Arc::clone(&writer);
    let leader = thread::spawn(move || leader_writer.commit_durable(7, &[0; 10]));
    blocking
        .wait_until_blocked(1)
        .expect("first barrier blocked");

    let followers = (0..4)
        .map(|_| {
            let follower_writer = Arc::clone(&writer);
            thread::spawn(move || follower_writer.commit(7, &[0; 10]))
        })
        .collect::<Vec<_>>();
    for follower in followers {
        follower
            .join()
            .expect("follower thread")
            .expect("visible commit");
    }
    assert_eq!(
        writer.visible_records().expect("visible records").len(),
        5,
        "followers must become visible while the first barrier is parked"
    );

    blocking.release_syncs(1).expect("release first barrier");
    leader
        .join()
        .expect("leader thread")
        .expect("durable commit");

    let stats = writer.stats().expect("stats");
    eprintln!(
        "wal_group_commit commits=5 max_group_bytes=128 appends={} barrier={} full={} group_sizes={:?} group_bytes={:?}",
        counting.append_calls(),
        counting.handle_barrier_sync_calls(),
        counting.handle_full_sync_calls(),
        stats.group_sizes,
        stats.group_bytes
    );
    assert_eq!(
        (
            counting.append_calls(),
            counting.handle_barrier_sync_calls(),
            counting.handle_full_sync_calls(),
            stats.group_sizes,
            stats.group_bytes,
            stats.pending_records,
        ),
        (2, 2, 0, vec![1, 4], vec![72, 128], 0),
        "five commits with a four-record byte cap must produce exactly ceil(5/4) groups"
    );
}

// Keep the path types imported above available to the standalone test-support
// module, whose `super` is this integration-test crate.
const _: Option<PathBuf> = None;
const _: Option<SyncKind> = None;
fn _vfs_file_type(_: Option<Box<dyn VfsFile>>) {}
