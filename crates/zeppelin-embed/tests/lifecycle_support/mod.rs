#![allow(clippy::expect_used)]

use std::sync::{Mutex, MutexGuard, OnceLock};

use tempfile::{TempDir, tempdir};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::manifest::Manifest;
use zeppelin_embed::manifest::io::commit_manifest;
use zeppelin_embed::meta::{AliveSet, ColumnStoreBuilder, Schema};
use zeppelin_embed::quant::Bit4Factors;
use zeppelin_embed::segment::SegmentId;
use zeppelin_embed::segment::writer::{SegmentBuild, SegmentFactors, write_segment};
use zeppelin_embed::vfs::StdVfs;

pub fn test_guard() -> MutexGuard<'static, ()> {
    static TEST_GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_GUARD
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("lifecycle test guard")
}

pub fn published_store(generation: u64) -> TempDir {
    let directory = tempdir().expect("published store directory");
    let schema = Schema::new(Vec::new()).expect("timestamp-only schema");
    let mut columns = ColumnStoreBuilder::new(schema.clone());
    columns.push_row(17, &[]).expect("fixture row");
    let columns = columns.finish().expect("fixture columns");
    let alive = AliveSet::new(1);
    let id = SegmentId::new(0x0102_0304_0506, [0x09; 10]);
    let codes = [0x88_u8];
    let factors = [Bit4Factors::from_persisted(1.0, 1.0, 1.0)];
    let rescore = [0.0_f32, 0.0_f32];
    let policy = DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::Ordered)
        .expect("derived policy");
    let segment = write_segment(
        &StdVfs,
        directory.path(),
        SegmentBuild {
            id,
            scheme: 4,
            dims: 2,
            codes: &codes,
            factors: SegmentFactors::Bit4(&factors),
            rescore: &rescore,
            columns: &columns,
            alive: &alive,
        },
        policy,
    )
    .expect("fixture segment");
    commit_manifest(
        &StdVfs,
        directory.path(),
        &Manifest {
            generation,
            log_seq: 0,
            segments: vec![segment],
            epochs: Vec::new(),
            schema,
        },
        policy,
    )
    .expect("fixture manifest");
    directory
}
