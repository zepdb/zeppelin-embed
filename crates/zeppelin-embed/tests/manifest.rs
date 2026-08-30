#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestRunner};
use tempfile::tempdir;
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::manifest::io::{
    DurableLog, MANIFEST_FILE, commit_manifest, load_manifest, open_manifest,
};
use zeppelin_embed::manifest::{Manifest, ManifestError};
use zeppelin_embed::meta::{
    AliveSet, ColumnDefinition, ColumnId, ColumnStoreBuilder, ColumnType, Schema,
};
use zeppelin_embed::segment::SegmentId;
use zeppelin_embed::segment::layout::{REGION_ENTRY_LEN, SEGMENT_PREFIX_LEN};
use zeppelin_embed::segment::writer::{SegmentBuild, SegmentFactors, write_segment};
use zeppelin_embed::vfs::{CountingVfs, StdVfs, SyncKind, Vfs, VfsFile};

fn ordered_policy() -> DurabilityPolicy {
    DurabilityPolicy::new(DurabilityMode::Durable, CommitTier::Ordered)
        .expect("ordered durability policy")
}

struct Log(u64);

impl DurableLog for Log {
    fn durable_end(&self) -> u64 {
        self.0
    }
}

struct RenameCrashVfs {
    inner: StdVfs,
    fail_rename: AtomicBool,
}

impl RenameCrashVfs {
    fn new(fail_rename: bool) -> Self {
        Self {
            inner: StdVfs,
            fail_rename: AtomicBool::new(fail_rename),
        }
    }

    fn permit_rename(&self) {
        self.fail_rename.store(false, Ordering::Relaxed);
    }
}

impl Vfs for RenameCrashVfs {
    fn open(&self, path: &Path) -> std::io::Result<u64> {
        self.inner.open(path)
    }

    fn open_for_map(&self, path: &Path) -> std::io::Result<std::fs::File> {
        self.inner.open_for_map(path)
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        self.inner.read(path)
    }

    fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        self.inner.read_range(path, offset, length)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.inner.write(path, bytes)
    }

    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        self.inner.open_append(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        if self.fail_rename.load(Ordering::Relaxed) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "simulated crash before rename",
            ));
        }
        self.inner.rename(from, to)
    }

    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        self.inner.sync(path, kind)
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        self.inner.list(directory)
    }

    fn delete(&self, path: &Path) -> std::io::Result<()> {
        self.inner.delete(path)
    }
}

fn schema() -> Schema {
    Schema::new(vec![ColumnDefinition::new(
        ColumnId::new(1),
        "tag",
        ColumnType::RawString,
        true,
    )])
    .expect("schema")
}

fn manifest(generation: u64, log_seq: u64) -> Manifest {
    Manifest {
        generation,
        log_seq,
        segments: Vec::new(),
        epochs: Vec::new(),
        epoch_alias: None,
        schema: schema(),
    }
}

fn write_empty_segment(directory: &Path, byte: u8) -> zeppelin_embed::segment::SegmentMeta {
    let columns = ColumnStoreBuilder::new(schema()).finish().expect("columns");
    let alive = AliveSet::new(0);
    write_segment(
        &StdVfs,
        directory,
        SegmentBuild {
            id: SegmentId::new(u64::from(byte), [byte; 10]),
            scheme: 4,
            dims: 3,
            codes: &[],
            factors: SegmentFactors::Bit4(&[]),
            rescore: &[],
            columns: &columns,
            alive: &alive,
        },
        ordered_policy(),
    )
    .expect("empty segment")
}

#[test]
fn manifest_commit_is_atomic_under_rename() {
    let directory = tempdir().expect("tempdir");
    let old = manifest(1, 10);
    commit_manifest(&StdVfs, directory.path(), &old, ordered_policy()).expect("old commit");

    let crash = RenameCrashVfs::new(true);
    let new = manifest(2, 11);
    let error = commit_manifest(&crash, directory.path(), &new, ordered_policy())
        .expect_err("rename crash");
    assert!(error.to_string().contains("simulated crash before rename"));
    let reopened = load_manifest(&StdVfs, &directory.path().join(MANIFEST_FILE), u64::MAX)
        .expect("old manifest remains");
    assert_eq!(reopened, old);

    crash.permit_rename();
    commit_manifest(&crash, directory.path(), &new, ordered_policy()).expect("new commit");
    let reopened = load_manifest(&StdVfs, &directory.path().join(MANIFEST_FILE), u64::MAX)
        .expect("new manifest visible");
    assert_eq!(reopened.generation, 2);
}

#[test]
fn manifest_refuses_ahead_of_log() {
    let directory = tempdir().expect("tempdir");
    commit_manifest(&StdVfs, directory.path(), &manifest(1, 5), ordered_policy()).expect("commit");
    let error = load_manifest(&StdVfs, &directory.path().join(MANIFEST_FILE), 4)
        .expect_err("ahead manifest must fail");
    match error {
        ManifestError::AheadOfLog { snapshot, durable } => {
            assert_eq!((snapshot, durable), (5, 4));
        }
        actual => panic!("unexpected error: {actual}"),
    }
}

#[test]
fn manifest_orphan_sweep_uses_reachability_and_writing_exclusions() {
    let directory = tempdir().expect("tempdir");
    let reachable = write_empty_segment(directory.path(), 1);
    let orphan = write_empty_segment(directory.path(), 2);
    let excluded = write_empty_segment(directory.path(), 3);
    let orphan_path = directory.path().join(orphan.id.file_name());
    let excluded_path = directory.path().join(excluded.id.file_name());
    let mut committed = manifest(1, 0);
    committed.segments.push(reachable.clone());
    commit_manifest(&StdVfs, directory.path(), &committed, ordered_policy()).expect("commit");
    let opened = open_manifest(
        &StdVfs,
        directory.path(),
        &Log(0),
        &HashSet::from([excluded_path.clone()]),
    )
    .expect("open");
    assert_eq!(opened.bytes_reclaimed, orphan.file_size);
    assert!(directory.path().join(reachable.id.file_name()).exists());
    assert!(!orphan_path.exists());
    assert!(excluded_path.exists());
}

#[test]
fn open_touches_only_manifest_and_headers() {
    let directory = tempdir().expect("tempdir");
    let first = write_empty_segment(directory.path(), 4);
    let second = write_empty_segment(directory.path(), 5);
    let mut committed = manifest(3, 2);
    committed.segments = vec![first, second];
    commit_manifest(&StdVfs, directory.path(), &committed, ordered_policy()).expect("commit");
    let counting = CountingVfs::new(StdVfs);
    let opened =
        open_manifest(&counting, directory.path(), &Log(2), &HashSet::new()).expect("bounded open");
    let manifest_bytes = StdVfs
        .open(&directory.path().join(MANIFEST_FILE))
        .expect("manifest length");
    assert_eq!(counting.read_bytes(), opened.bytes_read);
    let one_header = zeppelin_embed::format::frame::FILE_HEADER_LEN
        + SEGMENT_PREFIX_LEN
        + 6 * REGION_ENTRY_LEN
        + 8;
    assert_eq!(
        counting.read_bytes(),
        manifest_bytes + 2 * one_header as u64
    );
}

#[test]
fn prop_orphan_sweep_matches_random_reachable_unreachable_mixes() {
    let mut runner = TestRunner::new(Config {
        cases: 16,
        rng_seed: RngSeed::Fixed(0x07_0f_fa_5e),
        ..Config::default()
    });
    let statuses = prop::collection::vec(0_u8..3, 1..=5);
    let result = runner.run(&statuses, |statuses| {
        let directory = tempdir().expect("tempdir");
        let mut committed = manifest(1, 0);
        let mut exclusions = HashSet::new();
        let mut expected_reclaimed = 0_u64;
        let mut paths = Vec::new();
        for (position, status) in statuses.iter().copied().enumerate() {
            let segment = write_empty_segment(directory.path(), position as u8 + 20);
            let path = directory.path().join(segment.id.file_name());
            match status {
                0 => committed.segments.push(segment),
                1 => expected_reclaimed = expected_reclaimed.saturating_add(segment.file_size),
                2 => {
                    exclusions.insert(path.clone());
                }
                _ => unreachable!(),
            }
            paths.push((path, status));
        }
        commit_manifest(&StdVfs, directory.path(), &committed, ordered_policy()).expect("commit");
        let opened = open_manifest(&StdVfs, directory.path(), &Log(0), &exclusions).expect("open");
        prop_assert_eq!(opened.bytes_reclaimed, expected_reclaimed);
        for (path, status) in paths {
            prop_assert_eq!(path.exists(), status != 1);
        }
        Ok(())
    });
    assert!(result.is_ok(), "property result: {result:?}");
}
