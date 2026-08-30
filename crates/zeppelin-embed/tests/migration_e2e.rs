#![allow(clippy::expect_used, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tempfile::{TempDir, tempdir};
use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, EpochTransitionError,
    Normalization, StoreEpoch,
};
use zeppelin_embed::fts::tokenizer::TokenizerConfig;
use zeppelin_embed::ingest::{DocId, Revision, SearchRequest};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use zeppelin_embed::manifest::io::commit_manifest;
use zeppelin_embed::manifest::{EpochMeta, Manifest};
use zeppelin_embed::meta::{AliveSet, ColumnStoreBuilder, Schema};
use zeppelin_embed::quant::{Bit4Factors, quantize_bit4};
use zeppelin_embed::scan::ScanOptions;
use zeppelin_embed::segment::writer::{
    SegmentBuild, SegmentDocumentVersions, SegmentFactors, write_segment_with_documents,
};
use zeppelin_embed::segment::{SegmentId, SegmentMeta};
use zeppelin_embed::vfs::{StdVfs, SyncKind, Vfs, VfsFile};

const DIMS: usize = 4;

struct EpochFixture {
    directory: TempDir,
    epoch_a: StoreEpoch,
    epoch_b: StoreEpoch,
    segment_a: SegmentId,
}

fn policy() -> DurabilityPolicy {
    DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None).expect("fixture policy")
}

fn store_epoch(version: &str, weight: u8) -> StoreEpoch {
    let document = EmbeddingTower {
        model_id: "epoch-fixture".to_owned(),
        model_version: version.to_owned(),
        weights_digest: vec![weight],
        dims: DIMS as u32,
        normalization: Normalization::None,
        prompt_prefix: "document: ".to_owned(),
        max_tokens: 32,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    let mut query = document.clone();
    query.prompt_prefix = "query: ".to_owned();
    StoreEpoch {
        embedding: EmbeddingEpoch {
            document,
            query,
            alignment_digest: vec![weight, 0xa1],
        },
        tokenizer: TokenizerConfig::text_default().epoch(),
    }
}

fn write_epoch_segment(
    directory: &Path,
    id: SegmentId,
    epoch: &StoreEpoch,
    vectors: &[f32],
) -> SegmentMeta {
    let rows = vectors.len() / DIMS;
    let mut codes = vec![0_u8; rows * DIMS.div_ceil(2)];
    let mut factors = Vec::<Bit4Factors>::with_capacity(rows);
    for (vector, encoded) in vectors
        .chunks_exact(DIMS)
        .zip(codes.chunks_exact_mut(DIMS.div_ceil(2)))
    {
        factors.push(quantize_bit4(vector, encoded).expect("finite fixture vector"));
    }
    let mut columns = ColumnStoreBuilder::new(Schema::timestamp_only());
    for row in 0..rows {
        columns
            .push_row(row as i64, &[])
            .expect("fixture timestamp row");
    }
    let columns = columns.finish().expect("fixture columns");
    let alive = AliveSet::new(rows as u32);
    let all_doc_ids = [DocId::new(10), DocId::new(20), DocId::new(30)];
    let doc_ids = &all_doc_ids[..rows];
    let revisions = vec![Revision::new(1); rows];
    let mut meta = write_segment_with_documents(
        &StdVfs,
        directory,
        SegmentBuild {
            id,
            scheme: 4,
            dims: DIMS as u32,
            codes: &codes,
            factors: SegmentFactors::Bit4(&factors),
            rescore: vectors,
            columns: &columns,
            alive: &alive,
        },
        SegmentDocumentVersions {
            doc_ids,
            revisions: &revisions,
        },
        policy(),
    )
    .expect("epoch fixture segment");
    meta.epoch_id = Some(epoch.identity().embedding);
    meta
}

fn publish_incomplete_target_fixture() -> EpochFixture {
    let directory = tempdir().expect("incomplete epoch fixture directory");
    let epoch_a = store_epoch("A", 0xa0);
    let epoch_b = store_epoch("B", 0xb0);
    let segment_a = SegmentId::new(0x0021_0000_0011, [0xa1; 10]);
    let meta_a = write_epoch_segment(
        directory.path(),
        segment_a,
        &epoch_a,
        &[0.0, 1.0, 2.0, 3.0, 2.0, 1.0, 0.0, -1.0, 4.0, 3.0, 2.0, 1.0],
    );
    let meta_b = write_epoch_segment(
        directory.path(),
        SegmentId::new(0x0021_0000_0012, [0xb2; 10]),
        &epoch_b,
        &[4.0, 0.0, 1.0, 2.0, 0.0, -1.0, -2.0, -3.0],
    );
    commit_manifest(
        &StdVfs,
        directory.path(),
        &Manifest {
            generation: 1,
            log_seq: 0,
            segments: vec![meta_a, meta_b],
            epochs: vec![EpochMeta::from(&epoch_a), EpochMeta::from(&epoch_b)],
            epoch_alias: Some(epoch_a.identity()),
            schema: Schema::timestamp_only(),
        },
        policy(),
    )
    .expect("incomplete two-epoch manifest");
    EpochFixture {
        directory,
        epoch_a,
        epoch_b,
        segment_a,
    }
}

fn publish_two_epoch_fixture() -> EpochFixture {
    let directory = tempdir().expect("epoch fixture directory");
    let epoch_a = store_epoch("A", 0xa0);
    let epoch_b = store_epoch("B", 0xb0);
    let segment_a = SegmentId::new(0x0021_0000_0001, [0xa1; 10]);
    let segment_b = SegmentId::new(0x0021_0000_0002, [0xb2; 10]);
    let meta_a = write_epoch_segment(
        directory.path(),
        segment_a,
        &epoch_a,
        &[0.0, 1.0, 2.0, 3.0, 2.0, 1.0, 0.0, -1.0, 4.0, 3.0, 2.0, 1.0],
    );
    let meta_b = write_epoch_segment(
        directory.path(),
        segment_b,
        &epoch_b,
        &[
            4.0, 0.0, 1.0, 2.0, 0.0, -1.0, -2.0, -3.0, 1.0, 1.0, 1.0, 1.0,
        ],
    );
    commit_manifest(
        &StdVfs,
        directory.path(),
        &Manifest {
            generation: 1,
            log_seq: 0,
            segments: vec![meta_a, meta_b],
            epochs: vec![EpochMeta::from(&epoch_a), EpochMeta::from(&epoch_b)],
            epoch_alias: Some(epoch_a.identity()),
            schema: Schema::timestamp_only(),
        },
        policy(),
    )
    .expect("two-epoch manifest");
    EpochFixture {
        directory,
        epoch_a,
        epoch_b,
        segment_a,
    }
}

fn search_bits(store: &Store) -> Vec<(u128, u64, u32)> {
    store
        .search(
            SearchRequest::new(&[0.5, 0.5, 0.5, 0.5]),
            3,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("epoch search")
        .candidates
        .into_iter()
        .map(|candidate| {
            let version = candidate.document().expect("fixture document identity");
            (
                version.doc_id().get(),
                version.revision().get(),
                candidate.score().to_bits(),
            )
        })
        .collect()
}

#[test]
fn rollback_preserves_old_epoch_results_exactly_and_rollback_after_drop_epoch_is_a_typed_error() {
    let fixture = publish_two_epoch_fixture();
    let identity_a = fixture.epoch_a.identity();
    let identity_b = fixture.epoch_b.identity();
    let store = Store::open(
        fixture.directory.path(),
        OpenOptions::default().with_epoch(fixture.epoch_a.clone()),
    )
    .expect("open epoch A");

    let epoch_a_before = search_bits(&store);
    store
        .switch_epoch_alias(identity_b)
        .expect("publish epoch B alias");
    let epoch_b = search_bits(&store);
    assert_ne!(epoch_b, epoch_a_before, "fixture epochs must differ");

    store
        .switch_epoch_alias(identity_a)
        .expect("roll back to epoch A");
    assert_eq!(search_bits(&store), epoch_a_before);

    store
        .switch_epoch_alias(identity_b)
        .expect("restore epoch B before dropping A");
    let report = store
        .drop_epoch(identity_a.embedding)
        .expect("drop old epoch A");
    assert_eq!(report.segments_dropped(), &[fixture.segment_a]);
    assert!(
        !fixture
            .directory
            .path()
            .join(fixture.segment_a.file_name())
            .exists()
    );

    let error = store
        .switch_epoch_alias(identity_a)
        .expect_err("a dropped epoch cannot be restored");
    assert!(matches!(
        error,
        EpochTransitionError::EpochUnavailable { target } if target == identity_a
    ));
    assert_eq!(search_bits(&store), epoch_b);
}

#[test]
fn an_alias_cannot_publish_an_incomplete_target_epoch() {
    let fixture = publish_incomplete_target_fixture();
    let identity_b = fixture.epoch_b.identity();
    let store = Store::open(
        fixture.directory.path(),
        OpenOptions::default().with_epoch(fixture.epoch_a.clone()),
    )
    .expect("open epoch A");

    let error = store
        .switch_epoch_alias(identity_b)
        .expect_err("an N-1 target must never publish");
    assert!(matches!(
        error,
        EpochTransitionError::IncompleteEpoch {
            target,
            missing_documents: 1,
            unexpected_documents: 0,
        } if target == identity_b
    ));
    let unchanged = store
        .switch_epoch_alias(fixture.epoch_a.identity())
        .expect("epoch A remains published");
    assert!(!unchanged.manifest_committed());
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DropEvent {
    ManifestCommit,
    Unlink,
}

#[derive(Clone, Default)]
struct EventVfs {
    events: Arc<Mutex<Vec<DropEvent>>>,
}

impl EventVfs {
    fn events(&self) -> Vec<DropEvent> {
        self.events.lock().expect("event mutex").clone()
    }
}

impl Vfs for EventVfs {
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
        StdVfs.rename(from, to)?;
        if to.file_name().is_some_and(|name| name == "manifest.ze") {
            self.events
                .lock()
                .expect("event mutex")
                .push(DropEvent::ManifestCommit);
        }
        Ok(())
    }

    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        StdVfs.sync(path, kind)
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        StdVfs.list(directory)
    }

    fn delete(&self, path: &Path) -> std::io::Result<()> {
        StdVfs.delete(path)?;
        self.events
            .lock()
            .expect("event mutex")
            .push(DropEvent::Unlink);
        Ok(())
    }
}

#[test]
fn drop_epoch_commits_the_manifest_before_unlinking() {
    let fixture = publish_two_epoch_fixture();
    let identity_a = fixture.epoch_a.identity();
    let identity_b = fixture.epoch_b.identity();
    let store = Store::open(
        fixture.directory.path(),
        OpenOptions::default().with_epoch(fixture.epoch_a.clone()),
    )
    .expect("open epoch A");
    store
        .switch_epoch_alias(identity_b)
        .expect("publish epoch B alias");
    let vfs = EventVfs::default();

    store
        .drop_epoch_on_vfs(identity_a.embedding, &vfs)
        .expect("drop old epoch A");

    assert_eq!(
        vfs.events(),
        vec![DropEvent::ManifestCommit, DropEvent::Unlink]
    );
}
