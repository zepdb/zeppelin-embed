#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use std::path::Path;

use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::manifest::io::{
    MANIFEST_FILE, MANIFEST_TEMP_FILE, commit_manifest, load_manifest,
};
use zeppelin_embed::manifest::{Manifest, ManifestError, encode_manifest};
use zeppelin_embed::meta::{AliveSet, ColumnStoreBuilder, Schema};
use zeppelin_embed::segment::reader::validate_segment_bytes;
use zeppelin_embed::segment::writer::{
    SegmentBuild, SegmentFactors, encode_segment, write_segment,
};
use zeppelin_embed::segment::{SegmentError, SegmentId};
#[cfg(not(feature = "test-support"))]
use zeppelin_embed::vfs::VfsFile;
use zeppelin_embed::vfs::{CountingVfs, SyncKind, Vfs};

#[cfg(not(feature = "test-support"))]
#[path = "../src/vfs/crash.rs"]
mod crash_support;
#[cfg(not(feature = "test-support"))]
use crash_support::{CrashOperation, CrashStates, CrashVfs, MemoryVfs};
#[cfg(feature = "test-support")]
use zeppelin_embed::vfs::crash::{CrashOperation, CrashStates, CrashVfs, MemoryVfs};

const DIRECTORY: &str = "/write-path-ablation";

fn directory() -> &'static Path {
    Path::new(DIRECTORY)
}

fn policy(mode: DurabilityMode, tier: CommitTier) -> DurabilityPolicy {
    DurabilityPolicy::new(mode, tier).expect("supported durability policy")
}

fn ordered_policy() -> DurabilityPolicy {
    policy(DurabilityMode::Durable, CommitTier::Ordered)
}

fn schema() -> Schema {
    Schema::new(Vec::new()).expect("schema")
}

fn manifest() -> Manifest {
    Manifest {
        generation: 2,
        log_seq: 2,
        segments: Vec::new(),
        epochs: Vec::new(),
        schema: schema(),
    }
}

fn old_manifest() -> Manifest {
    Manifest {
        generation: 1,
        log_seq: 1,
        segments: Vec::new(),
        epochs: Vec::new(),
        schema: schema(),
    }
}

fn seed_manifest_store() -> MemoryVfs {
    let store = MemoryVfs::new();
    store
        .insert(
            directory().join(MANIFEST_FILE),
            encode_manifest(&old_manifest()).expect("old manifest bytes"),
        )
        .expect("old manifest");
    store
        .insert(
            directory().join(MANIFEST_TEMP_FILE),
            b"stale manifest temp".to_vec(),
        )
        .expect("stale manifest temp");
    store
}

fn seed_segment_store(id: SegmentId) -> MemoryVfs {
    let store = MemoryVfs::new();
    store
        .insert(
            directory().join(format!(".{}.tmp", id.file_name())),
            b"stale segment temp".to_vec(),
        )
        .expect("stale segment temp");
    store
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ManifestStep {
    WriteTemp,
    SyncTemp,
    RenameTemp,
    SyncDirectory,
}

const MANIFEST_STEPS: &[ManifestStep] = &[
    ManifestStep::WriteTemp,
    ManifestStep::SyncTemp,
    ManifestStep::RenameTemp,
    ManifestStep::SyncDirectory,
];

fn commit_manifest_steps(
    vfs: &dyn Vfs,
    value: &Manifest,
    steps: &[ManifestStep],
) -> Result<(), ManifestError> {
    let bytes = encode_manifest(value)?;
    let temporary = directory().join(MANIFEST_TEMP_FILE);
    let committed = directory().join(MANIFEST_FILE);
    if steps.contains(&ManifestStep::WriteTemp) {
        vfs.write(&temporary, &bytes)
            .map_err(|error| ManifestError::Io {
                path: temporary.clone(),
                source: error,
            })?;
    }
    if steps.contains(&ManifestStep::SyncTemp) {
        vfs.sync(&temporary, SyncKind::Barrier)
            .map_err(|error| ManifestError::Io {
                path: temporary.clone(),
                source: error,
            })?;
    }
    if steps.contains(&ManifestStep::RenameTemp) {
        vfs.rename(&temporary, &committed)
            .map_err(|error| ManifestError::Io {
                path: committed,
                source: error,
            })?;
    }
    if steps.contains(&ManifestStep::SyncDirectory) {
        vfs.sync(directory(), SyncKind::Barrier)
            .map_err(|error| ManifestError::Io {
                path: directory().to_path_buf(),
                source: error,
            })?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SegmentStep {
    WriteTemp,
    SyncTemp,
    RenameTemp,
    SyncDirectory,
}

const SEGMENT_STEPS: &[SegmentStep] = &[
    SegmentStep::WriteTemp,
    SegmentStep::SyncTemp,
    SegmentStep::RenameTemp,
    SegmentStep::SyncDirectory,
];

fn write_segment_steps(
    vfs: &dyn Vfs,
    build: SegmentBuild<'_>,
    steps: &[SegmentStep],
) -> Result<(), SegmentError> {
    let bytes = encode_segment(build)?;
    let final_path = directory().join(build.id.file_name());
    let temporary = directory().join(format!(".{}.tmp", build.id.file_name()));
    if steps.contains(&SegmentStep::WriteTemp) {
        vfs.write(&temporary, &bytes)
            .map_err(|error| SegmentError::Io {
                path: temporary.clone(),
                source: error,
            })?;
    }
    if steps.contains(&SegmentStep::SyncTemp) {
        vfs.sync(&temporary, SyncKind::Barrier)
            .map_err(|error| SegmentError::Io {
                path: temporary.clone(),
                source: error,
            })?;
    }
    if steps.contains(&SegmentStep::RenameTemp) {
        vfs.rename(&temporary, &final_path)
            .map_err(|error| SegmentError::Io {
                path: final_path,
                source: error,
            })?;
    }
    if steps.contains(&SegmentStep::SyncDirectory) {
        vfs.sync(directory(), SyncKind::Barrier)
            .map_err(|error| SegmentError::Io {
                path: directory().to_path_buf(),
                source: error,
            })?;
    }
    Ok(())
}

fn empty_segment_build<'a>(
    id: SegmentId,
    columns: &'a zeppelin_embed::meta::ColumnStore,
    alive: &'a AliveSet,
) -> SegmentBuild<'a> {
    SegmentBuild {
        id,
        scheme: 4,
        dims: 3,
        codes: &[],
        factors: SegmentFactors::Bit4(&[]),
        rescore: &[],
        columns,
        alive,
    }
}

fn sync_operations(recorder: &CrashVfs) -> Vec<CrashOperation> {
    recorder
        .operations()
        .expect("recorded operations")
        .into_iter()
        .filter(|operation| matches!(operation, CrashOperation::Sync { .. }))
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CounterSnapshot {
    open_calls: u64,
    read_calls: u64,
    read_bytes: u64,
    write_calls: u64,
    bytes_written: u64,
    append_calls: u64,
    bytes_appended: u64,
    rename_calls: u64,
    delete_calls: u64,
    barrier_sync_calls: u64,
    full_sync_calls: u64,
    handle_barrier_sync_calls: u64,
    handle_full_sync_calls: u64,
}

fn counter_snapshot<V>(counting: &CountingVfs<V>) -> CounterSnapshot {
    CounterSnapshot {
        open_calls: counting.open_calls(),
        read_calls: counting.read_calls(),
        read_bytes: counting.read_bytes(),
        write_calls: counting.write_calls(),
        bytes_written: counting.bytes_written(),
        append_calls: counting.append_calls(),
        bytes_appended: counting.bytes_appended(),
        rename_calls: counting.rename_calls(),
        delete_calls: counting.delete_calls(),
        barrier_sync_calls: counting.barrier_sync_calls(),
        full_sync_calls: counting.full_sync_calls(),
        handle_barrier_sync_calls: counting.handle_barrier_sync_calls(),
        handle_full_sync_calls: counting.handle_full_sync_calls(),
    }
}

fn expected_counts(bytes_written: u64, barrier: u64, full: u64) -> CounterSnapshot {
    CounterSnapshot {
        open_calls: 0,
        read_calls: 0,
        read_bytes: 0,
        write_calls: 1,
        bytes_written,
        append_calls: 0,
        bytes_appended: 0,
        rename_calls: 1,
        delete_calls: 0,
        barrier_sync_calls: barrier,
        full_sync_calls: full,
        handle_barrier_sync_calls: 0,
        handle_full_sync_calls: 0,
    }
}

fn expected_verified_segment_counts(
    bytes_written: u64,
    barrier: u64,
    full: u64,
) -> CounterSnapshot {
    let mut counts = expected_counts(bytes_written, barrier, full);
    counts.read_calls = 1;
    counts.read_bytes = bytes_written;
    counts
}

fn operation_summaries(operations: &[CrashOperation]) -> Vec<String> {
    operations
        .iter()
        .map(|operation| match operation {
            CrashOperation::Write { path, bytes } => {
                format!("Write({}, {} bytes)", path.display(), bytes.len())
            }
            CrashOperation::OpenAppend { path } => {
                format!("OpenAppend({})", path.display())
            }
            CrashOperation::Append { path, bytes } => {
                format!("Append({}, {} bytes)", path.display(), bytes.len())
            }
            CrashOperation::Sync { path, kind } => {
                format!("Sync({}, {kind:?})", path.display())
            }
            CrashOperation::Rename { from, to } => {
                format!("Rename({}, {})", from.display(), to.display())
            }
            CrashOperation::Delete { path } => format!("Delete({})", path.display()),
        })
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AblationFailure {
    state: String,
    assertion: String,
}

fn first_manifest_failure(states: &CrashStates, operation_count: usize) -> Option<AblationFailure> {
    states.iter().find_map(|state| {
        let assertion = match load_manifest(state.vfs(), &directory().join(MANIFEST_FILE), u64::MAX)
        {
            Ok(actual) if actual == manifest() => return None,
            Ok(actual)
                if actual == old_manifest()
                    && state.includes_complete_operation_sequence(operation_count) =>
            {
                "successfully returned manifest commit did not publish new state".to_owned()
            }
            Ok(actual) if actual == old_manifest() => return None,
            Ok(actual) => format!(
                "committed manifest generation {} was neither previous nor new",
                actual.generation
            ),
            Err(ManifestError::Format(error)) => format!(
                "committed manifest failed Format({:?}), expected previous or new",
                error.check()
            ),
            Err(error) => format!("committed manifest failed {error}, expected previous or new"),
        };
        Some(AblationFailure {
            state: format!("{:?}", state.kind()),
            assertion,
        })
    })
}

fn first_segment_failure(
    states: &CrashStates,
    operation_count: usize,
    id: SegmentId,
) -> Option<AblationFailure> {
    states.iter().find_map(|state| {
        let path = directory().join(id.file_name());
        let assertion = match state.vfs().read(&path) {
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && state.includes_complete_operation_sequence(operation_count) =>
            {
                "successfully returned segment publish did not publish new state".to_owned()
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => format!(
                "published segment read failed {:?}, expected absent or valid",
                error.kind()
            ),
            Ok(bytes) => match validate_segment_bytes(&bytes) {
                Ok(actual) if actual.id == id => return None,
                Ok(actual) => format!(
                    "published segment id {} differed from expected {id}",
                    actual.id
                ),
                Err(SegmentError::Format(error)) => format!(
                    "published segment failed Format({:?}), expected absent or valid",
                    error.check()
                ),
                Err(error) => {
                    format!("published segment failed {error}, expected absent or valid")
                }
            },
        };
        Some(AblationFailure {
            state: format!("{:?}", state.kind()),
            assertion,
        })
    })
}

#[test]
fn commit_manifest_ablation_all_steps_matches_production() {
    let production = CrashVfs::new(seed_manifest_store()).expect("production recorder");
    commit_manifest(&production, directory(), &manifest(), ordered_policy())
        .expect("production manifest commit");
    let ablation = CrashVfs::new(seed_manifest_store()).expect("ablation recorder");
    commit_manifest_steps(&ablation, &manifest(), MANIFEST_STEPS)
        .expect("all-step manifest commit");

    let ablation_operations = ablation.operations().expect("ablation operations");
    let production_operations = production.operations().expect("production operations");
    eprintln!(
        "ablation_parity protocol=commit_manifest ablation={:?} production={:?}",
        operation_summaries(&ablation_operations),
        operation_summaries(&production_operations)
    );
    assert!(
        ablation_operations == production_operations,
        "manifest ablation is void unless ALL steps exactly match production\n  ablation: {:?}\n production: {:?}",
        operation_summaries(&ablation_operations),
        operation_summaries(&production_operations)
    );
}

#[test]
fn write_segment_ablation_all_steps_matches_production() {
    let id = SegmentId::new(9, [9; 10]);
    let columns = ColumnStoreBuilder::new(schema()).finish().expect("columns");
    let alive = AliveSet::new(0);
    let production = CrashVfs::new(seed_segment_store(id)).expect("production recorder");
    write_segment(
        &production,
        directory(),
        empty_segment_build(id, &columns, &alive),
        ordered_policy(),
    )
    .expect("production segment publish");
    let ablation = CrashVfs::new(seed_segment_store(id)).expect("ablation recorder");
    write_segment_steps(
        &ablation,
        empty_segment_build(id, &columns, &alive),
        SEGMENT_STEPS,
    )
    .expect("all-step segment publish");

    let ablation_operations = ablation.operations().expect("ablation operations");
    let production_operations = production.operations().expect("production operations");
    eprintln!(
        "ablation_parity protocol=write_segment ablation={:?} production={:?}",
        operation_summaries(&ablation_operations),
        operation_summaries(&production_operations)
    );
    assert!(
        ablation_operations == production_operations,
        "segment ablation is void unless ALL steps exactly match production\n  ablation: {:?}\n production: {:?}",
        operation_summaries(&ablation_operations),
        operation_summaries(&production_operations)
    );
}

#[test]
fn commit_manifest_each_step_ablation_has_specific_verdict() {
    let actual = MANIFEST_STEPS
        .iter()
        .copied()
        .map(|ablated| {
            let enabled = MANIFEST_STEPS
                .iter()
                .copied()
                .filter(|step| *step != ablated)
                .collect::<Vec<_>>();
            let recorder = CrashVfs::new(seed_manifest_store()).expect("manifest recorder");
            commit_manifest_steps(&recorder, &manifest(), &enabled)
                .expect("ablated manifest protocol");
            let states = recorder.crash_states().expect("manifest crash states");
            assert!(!states.is_empty(), "{ablated:?} enumerated no states");
            assert!(!states.was_capped(), "{ablated:?} hit crash-state cap");
            let operation_count = recorder.operations().expect("manifest operations").len();
            let failure = first_manifest_failure(&states, operation_count);
            eprintln!(
                "write_path_ablation protocol=commit_manifest step={ablated:?} states={} capped={} first_failure={failure:?}",
                states.len(),
                states.was_capped()
            );
            (ablated, failure)
        })
        .collect::<Vec<_>>();
    let expected = vec![
        (
            ManifestStep::WriteTemp,
            Some(AblationFailure {
                state: "Prefix { completed_operations: 2 }".to_owned(),
                assertion: "committed manifest failed Format(Length), expected previous or new"
                    .to_owned(),
            }),
        ),
        (
            ManifestStep::SyncTemp,
            Some(AblationFailure {
                state: "RenameWithOldContent { operation_index: 1 }".to_owned(),
                assertion: "committed manifest failed Format(Length), expected previous or new"
                    .to_owned(),
            }),
        ),
        (
            ManifestStep::RenameTemp,
            Some(AblationFailure {
                state: "Prefix { completed_operations: 3 }".to_owned(),
                assertion: "successfully returned manifest commit did not publish new state"
                    .to_owned(),
            }),
        ),
        (ManifestStep::SyncDirectory, None),
    ];

    assert_eq!(
        actual, expected,
        "each manifest step needs a named crash-state verdict"
    );
}

#[test]
fn write_segment_each_step_ablation_has_specific_verdict() {
    let id = SegmentId::new(10, [10; 10]);
    let columns = ColumnStoreBuilder::new(schema()).finish().expect("columns");
    let alive = AliveSet::new(0);
    let actual = SEGMENT_STEPS
        .iter()
        .copied()
        .map(|ablated| {
            let enabled = SEGMENT_STEPS
                .iter()
                .copied()
                .filter(|step| *step != ablated)
                .collect::<Vec<_>>();
            let recorder = CrashVfs::new(seed_segment_store(id)).expect("segment recorder");
            write_segment_steps(
                &recorder,
                empty_segment_build(id, &columns, &alive),
                &enabled,
            )
            .expect("ablated segment protocol");
            let states = recorder.crash_states().expect("segment crash states");
            assert!(!states.is_empty(), "{ablated:?} enumerated no states");
            assert!(!states.was_capped(), "{ablated:?} hit crash-state cap");
            let operation_count = recorder.operations().expect("segment operations").len();
            let failure = first_segment_failure(&states, operation_count, id);
            eprintln!(
                "write_path_ablation protocol=write_segment step={ablated:?} states={} capped={} first_failure={failure:?}",
                states.len(),
                states.was_capped()
            );
            (ablated, failure)
        })
        .collect::<Vec<_>>();
    let expected = vec![
        (
            SegmentStep::WriteTemp,
            Some(AblationFailure {
                state: "Prefix { completed_operations: 2 }".to_owned(),
                assertion: "published segment failed Format(Length), expected absent or valid"
                    .to_owned(),
            }),
        ),
        (
            SegmentStep::SyncTemp,
            Some(AblationFailure {
                state: "RenameWithOldContent { operation_index: 1 }".to_owned(),
                assertion: "published segment failed Format(Length), expected absent or valid"
                    .to_owned(),
            }),
        ),
        (
            SegmentStep::RenameTemp,
            Some(AblationFailure {
                state: "Prefix { completed_operations: 3 }".to_owned(),
                assertion: "successfully returned segment publish did not publish new state"
                    .to_owned(),
            }),
        ),
        (SegmentStep::SyncDirectory, None),
    ];

    assert_eq!(
        actual, expected,
        "each segment step needs a named crash-state verdict"
    );
}

#[test]
fn commit_manifest_uses_exact_sync_sequence_per_tier() {
    for (tier, expected_kind, expected_counts) in [
        (CommitTier::None, None, expected_counts(88, 0, 0)),
        (
            CommitTier::Ordered,
            Some(SyncKind::Barrier),
            expected_counts(88, 2, 0),
        ),
        (
            CommitTier::Durable,
            Some(SyncKind::Full),
            expected_counts(88, 0, 2),
        ),
    ] {
        let recorder = CrashVfs::new(MemoryVfs::new()).expect("manifest recorder");
        let counting = CountingVfs::new(recorder);
        commit_manifest(
            &counting,
            directory(),
            &manifest(),
            policy(DurabilityMode::Durable, tier),
        )
        .expect("manifest commit");
        let expected_syncs = expected_kind.map_or_else(Vec::new, |kind| {
            vec![
                CrashOperation::Sync {
                    path: directory().join(MANIFEST_TEMP_FILE),
                    kind,
                },
                CrashOperation::Sync {
                    path: directory().to_path_buf(),
                    kind,
                },
            ]
        });

        assert_eq!(
            sync_operations(counting.inner()),
            expected_syncs,
            "manifest {tier:?} exact (path, SyncKind) sequence"
        );
        assert_eq!(
            counter_snapshot(&counting),
            expected_counts,
            "manifest {tier:?} exact deterministic counters"
        );
        eprintln!(
            "write_path_counters protocol=commit_manifest tier={tier:?} counts={:?}",
            counter_snapshot(&counting)
        );
    }
}

#[test]
fn write_segment_uses_exact_sync_sequence_per_tier() {
    let columns = ColumnStoreBuilder::new(schema()).finish().expect("columns");
    let alive = AliveSet::new(0);
    let id = SegmentId::new(2, [2; 10]);
    for (tier, expected_kind, expected_counts) in [
        (
            CommitTier::None,
            None,
            expected_verified_segment_counts(98_400, 0, 0),
        ),
        (
            CommitTier::Ordered,
            Some(SyncKind::Barrier),
            expected_verified_segment_counts(98_400, 2, 0),
        ),
        (
            CommitTier::Durable,
            Some(SyncKind::Full),
            expected_verified_segment_counts(98_400, 0, 2),
        ),
    ] {
        let recorder = CrashVfs::new(MemoryVfs::new()).expect("segment recorder");
        let counting = CountingVfs::new(recorder);
        write_segment(
            &counting,
            directory(),
            SegmentBuild {
                id,
                scheme: 4,
                dims: 3,
                codes: &[],
                factors: SegmentFactors::Bit4(&[]),
                rescore: &[],
                columns: &columns,
                alive: &alive,
            },
            policy(DurabilityMode::Durable, tier),
        )
        .expect("segment publish");
        let expected_syncs = expected_kind.map_or_else(Vec::new, |kind| {
            vec![
                CrashOperation::Sync {
                    path: directory().join(format!(".{}.tmp", id.file_name())),
                    kind,
                },
                CrashOperation::Sync {
                    path: directory().to_path_buf(),
                    kind,
                },
            ]
        });

        assert_eq!(
            sync_operations(counting.inner()),
            expected_syncs,
            "segment {tier:?} exact (path, SyncKind) sequence"
        );
        assert_eq!(
            counter_snapshot(&counting),
            expected_counts,
            "segment {tier:?} exact deterministic counters"
        );
        eprintln!(
            "write_path_counters protocol=write_segment tier={tier:?} counts={:?}",
            counter_snapshot(&counting)
        );
    }
}
