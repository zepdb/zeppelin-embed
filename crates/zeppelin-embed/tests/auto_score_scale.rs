#![allow(clippy::expect_used)]

use tempfile::{TempDir, tempdir};
use zeppelin_embed::ingest::{RowSource, SearchRequest};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::{
    CancelToken, OpenOptions, QueryControl, SearchOptions, SearchTier, Store,
};
use zeppelin_embed::manifest::Manifest;
use zeppelin_embed::manifest::io::commit_manifest;
use zeppelin_embed::meta::{AliveSet, ColumnStoreBuilder, Schema};
use zeppelin_embed::quant::quantize_bit4;
use zeppelin_embed::segment::writer::{SegmentBuild, SegmentFactors, write_segment};
use zeppelin_embed::segment::{SegmentId, SegmentMeta};
use zeppelin_embed::vfs::StdVfs;

const DIMS: usize = 4;

fn policy() -> DurabilityPolicy {
    DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None)
        .expect("test durability policy")
}

fn columns() -> zeppelin_embed::meta::ColumnStore {
    let mut builder = ColumnStoreBuilder::new(Schema::timestamp_only());
    builder.push_row(0, &[]).expect("timestamp row");
    builder.finish().expect("fixture columns")
}

fn write_f32(directory: &TempDir, id: SegmentId, vector: &[f32; DIMS]) -> SegmentMeta {
    let codes = vector
        .iter()
        .flat_map(|value| value.to_bits().to_le_bytes())
        .collect::<Vec<_>>();
    let columns = columns();
    let alive = AliveSet::new(1);
    write_segment(
        &StdVfs,
        directory.path(),
        SegmentBuild {
            id,
            scheme: 0,
            dims: DIMS as u32,
            codes: &codes,
            factors: SegmentFactors::F32,
            rescore: vector,
            columns: &columns,
            alive: &alive,
        },
        policy(),
    )
    .expect("write F32 segment through the public writer")
}

fn write_bit4(directory: &TempDir, id: SegmentId, vector: &[f32; DIMS]) -> SegmentMeta {
    let mut codes = vec![0_u8; DIMS.div_ceil(2)];
    let factors = [quantize_bit4(vector, &mut codes).expect("quantize Bit4 row")];
    let columns = columns();
    let alive = AliveSet::new(1);
    write_segment(
        &StdVfs,
        directory.path(),
        SegmentBuild {
            id,
            scheme: 4,
            dims: DIMS as u32,
            codes: &codes,
            factors: SegmentFactors::Bit4(&factors),
            rescore: vector,
            columns: &columns,
            alive: &alive,
        },
        policy(),
    )
    .expect("write Bit4 segment through the public writer")
}

fn publish_manifest(directory: &TempDir, segments: Vec<SegmentMeta>) {
    commit_manifest(
        &StdVfs,
        directory.path(),
        &Manifest {
            generation: 1,
            log_seq: 0,
            segments,
            epochs: Vec::new(),
            epoch_alias: None,
            schema: Schema::timestamp_only(),
        },
        policy(),
    )
    .expect("publish fixture manifest");
}

#[test]
fn auto_never_merges_estimated_and_exact_scores() {
    let directory = tempdir().expect("store directory");
    let f32_id = SegmentId::new(0x0001_3400_0000, [0x34; 10]);
    let bit4_id = SegmentId::new(0x0001_3400_0001, [0x35; 10]);
    let f32_meta = write_f32(&directory, f32_id, &[1.0, 0.0, 0.0, 0.0]);
    let bit4_meta = write_bit4(&directory, bit4_id, &[0.0, 2.0, 0.0, 0.0]);
    publish_manifest(&directory, vec![f32_meta, bit4_meta]);

    let store = Store::open(directory.path(), OpenOptions::default()).expect("open mixed store");
    let outcome = store
        .search(
            SearchRequest::new(&[0.0; DIMS]),
            2,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("Auto query must return one score scale or refuse with a typed error");

    let mut sources = outcome
        .candidates
        .iter()
        .map(|candidate| candidate.row_id().source())
        .collect::<Vec<_>>();
    sources.sort_unstable();
    assert_eq!(
        sources,
        vec![RowSource::Sealed(f32_id), RowSource::Sealed(bit4_id)]
    );
    assert!(
        outcome.diagnostics.exact_rescore,
        "Auto silently merged estimated and exact scores in one answer"
    );
    store.close().expect("close mixed store");
}

#[test]
fn auto_preserves_single_segment_score_scales() {
    let query = [0.0; DIMS];

    let exact_directory = tempdir().expect("exact store directory");
    let exact_id = SegmentId::new(0x0001_3400_0002, [0x36; 10]);
    let exact_meta = write_f32(&exact_directory, exact_id, &[1.0, 2.0, 3.0, 4.0]);
    publish_manifest(&exact_directory, vec![exact_meta]);
    let exact_store =
        Store::open(exact_directory.path(), OpenOptions::default()).expect("open exact store");
    let exact_auto = exact_store
        .search(
            SearchRequest::new(&query),
            1,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("single F32 Auto query");
    let exact_scan = exact_store
        .search(
            SearchRequest::new(&query),
            1,
            SearchOptions::default().with_tier(SearchTier::Scan),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("single F32 Scan query");
    assert_eq!(exact_auto.candidates, exact_scan.candidates);
    assert!(exact_auto.diagnostics.exact_rescore);
    exact_store.close().expect("close exact store");

    let estimated_directory = tempdir().expect("estimated store directory");
    let estimated_id = SegmentId::new(0x0001_3400_0003, [0x37; 10]);
    let estimated_meta = write_bit4(&estimated_directory, estimated_id, &[1.0, 2.0, 3.0, 4.0]);
    publish_manifest(&estimated_directory, vec![estimated_meta]);
    let estimated_store = Store::open(estimated_directory.path(), OpenOptions::default())
        .expect("open estimated store");
    let estimated_auto = estimated_store
        .search(
            SearchRequest::new(&query),
            1,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("single Bit4 Auto query");
    let estimated_scan = estimated_store
        .search(
            SearchRequest::new(&query),
            1,
            SearchOptions::default().with_tier(SearchTier::Scan),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("single Bit4 Scan query");
    assert_eq!(estimated_auto.candidates, estimated_scan.candidates);
    assert!(!estimated_auto.diagnostics.exact_rescore);
    estimated_store.close().expect("close estimated store");
}
