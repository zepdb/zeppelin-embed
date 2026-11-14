#![allow(clippy::expect_used)]

mod test_support;

use rand::Rng;
use tempfile::{TempDir, tempdir};
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, RowSource,
    SearchCandidate, SearchRequest,
};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::{
    CancelToken, OpenOptions, QueryControl, SearchOptions, SearchTier, Store,
};
use zeppelin_embed::manifest::Manifest;
use zeppelin_embed::manifest::io::commit_manifest;
use zeppelin_embed::meta::{
    AliveSet, ColumnStoreBuilder, Predicate, PredicateValue, RangeBound, RangePredicate, Schema,
    TIMESTAMP_COLUMN,
};
use zeppelin_embed::planner::ScanReason;
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
            #[cfg(feature = "graph-cypher")]
            graph: None,
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
    let exact_tier = exact_store
        .search(
            SearchRequest::new(&query),
            1,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("single F32 Exact query");
    assert_eq!(exact_auto.candidates, exact_tier.candidates);
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
    let estimated_exact = estimated_store
        .search(
            SearchRequest::new(&query),
            1,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("single Bit4 Exact query");
    assert_eq!(estimated_auto.candidates, estimated_exact.candidates);
    assert!(estimated_auto.diagnostics.exact_rescore);
    estimated_store.close().expect("close estimated store");
}

const PROBE: [f32; DIMS] = [2.0, -1.0, 3.0, 0.5];

fn graphless_store(name: &str) -> (TempDir, Store, Vec<(DocId, Vec<f32>)>) {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    let mut rng = test_support::seeded_rng(name);
    let mut rows = Vec::new();
    for batch in 0..3 {
        let documents = (0..12)
            .map(|offset| {
                let index = batch * 12 + offset;
                let (id, vector) = if offset == 0 {
                    // The lowest document id is in the active tail; all three
                    // identical vectors straddle the k=2 cut across segments.
                    (
                        DocId::new((30 - batch * 10) as u128),
                        vec![3.0, -1.0, 3.0, 0.5],
                    )
                } else {
                    (
                        DocId::new(100 + index as u128),
                        (0..DIMS)
                            .map(|_| rng.random_range(32..72) as f32 / 8.0)
                            .collect(),
                    )
                };
                rows.push((id, vector.clone()));
                IngestDocument::new(DocumentVersion::new(id, Revision::new(1)), vector)
                    .with_timestamp(index as i64)
            })
            .collect();
        store.ingest(IngestBatch::new(documents)).expect("ingest");
        if batch < 2 {
            store.seal().expect("seal without graph maintenance");
        }
    }
    let deleted = [DocId::new(102), DocId::new(114), DocId::new(126)];
    store
        .delete(DeleteBatch::new(deleted.to_vec()))
        .expect("delete in each source");
    rows.retain(|(id, _)| !deleted.contains(id));
    (directory, store, rows)
}

fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}

fn scored_ids(hits: &[SearchCandidate]) -> Vec<(DocId, u32)> {
    hits.iter()
        .map(|hit| {
            (
                hit.document().expect("document").doc_id(),
                hit.score().to_bits(),
            )
        })
        .collect()
}

#[test]
fn unset_tier_without_a_graph_equals_exact() {
    let (_directory, store, _) =
        graphless_store("auto_score_scale::unset_tier_without_a_graph_equals_exact");
    for k in [1, 10, 2] {
        let default = store
            .search(
                SearchRequest::new(&PROBE),
                k,
                SearchOptions::default(),
                control(),
            )
            .expect("default");
        let exact = store
            .search(
                SearchRequest::new(&PROBE),
                k,
                SearchOptions::default().with_tier(SearchTier::Exact),
                control(),
            )
            .expect("Exact");
        assert_eq!(
            scored_ids(&default.candidates),
            scored_ids(&exact.candidates),
            "k={k}"
        );
        if k == 2 {
            assert_eq!(
                exact
                    .candidates
                    .iter()
                    .map(|hit| hit.document().expect("document").doc_id())
                    .collect::<Vec<_>>(),
                vec![DocId::new(10), DocId::new(20)]
            );
        }
    }
    store.close().expect("close");
}

#[test]
fn unset_tier_filtered_equals_exact() {
    let (_directory, store, _) =
        graphless_store("auto_score_scale::unset_tier_filtered_equals_exact");
    let predicate = Predicate::Range(RangePredicate {
        column: TIMESTAMP_COLUMN,
        lower: Some(RangeBound::inclusive(PredicateValue::I64(8))),
        upper: Some(RangeBound::inclusive(PredicateValue::I64(30))),
    });
    let ids = [10, 20, 30, 102, 111, 114, 119, 126, 129, 134].map(DocId::new);
    // Check both entry points even on RED so one failure cannot hide the other.
    let mut equal = Vec::new();
    for k in [1, 10, 2] {
        let default = store
            .search_filtered(
                SearchRequest::new(&PROBE),
                &predicate,
                k,
                SearchOptions::default(),
                control(),
            )
            .expect("filtered default");
        let exact = store
            .search_filtered(
                SearchRequest::new(&PROBE),
                &predicate,
                k,
                SearchOptions::default().with_tier(SearchTier::Exact),
                control(),
            )
            .expect("filtered Exact");
        equal.push(scored_ids(&default.candidates) == scored_ids(&exact.candidates));
        let request = SearchRequest::new(&PROBE).with_eligible(&ids);
        let default = store
            .search(request, k, SearchOptions::default(), control())
            .expect("eligible default");
        let exact = store
            .search(
                request,
                k,
                SearchOptions::default().with_tier(SearchTier::Exact),
                control(),
            )
            .expect("eligible Exact");
        equal.push(scored_ids(&default.candidates) == scored_ids(&exact.candidates));
    }
    assert_eq!(
        equal,
        vec![true; 6],
        "timestamp and eligible-id equality for each k"
    );
    store.close().expect("close");
}

#[test]
fn unset_tier_diagnostics_are_truthful() {
    let (_directory, store, rows) =
        graphless_store("auto_score_scale::unset_tier_diagnostics_are_truthful");
    let default = store
        .search(
            SearchRequest::new(&PROBE),
            10,
            SearchOptions::default(),
            control(),
        )
        .expect("default");
    assert!(default.diagnostics.exact_rescore);
    assert!(!default.diagnostics.approximate);
    assert!(
        default
            .diagnostics
            .plan
            .iter()
            .all(|plan| !matches!(plan.scan_reason, Some(ScanReason::ExplicitTier(_))))
    );
    for hit in default.candidates {
        let id = hit.document().expect("document").doc_id();
        let (_, row) = rows
            .iter()
            .find(|(key, _)| *key == id)
            .expect("live original vector");
        let distance: f64 = row
            .iter()
            .zip(PROBE)
            .map(|(a, b)| (f64::from(*a) - f64::from(b)).powi(2))
            .sum();
        assert_eq!(
            f64::from(hit.score()),
            -distance,
            "independent squared L2 for {id:?}"
        );
    }
    for tier in [SearchTier::Scan, SearchTier::Auto] {
        let explicit = store
            .search(
                SearchRequest::new(&PROBE),
                10,
                SearchOptions::default().with_tier(tier),
                control(),
            )
            .expect("explicit coarse tier");
        assert!(!explicit.diagnostics.exact_rescore, "{tier:?}");
    }
    store.close().expect("close");
}
