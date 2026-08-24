#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use std::sync::{Arc, Barrier};

use rand::Rng;
use tempfile::{TempDir, tempdir};

use crate::graph::block::{GraphNodeBlockBuild, GraphNodeBlockInput, GraphNodeLayout};
use crate::graph::search::{GraphSearchProfile, GraphSearchScratch};
use crate::ingest::{GlobalRowId, SearchOutcome, SearchRequest};
use crate::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use crate::manifest::Manifest;
use crate::manifest::io::commit_manifest;
use crate::meta::{AliveSet, ColumnStoreBuilder, Schema};
use crate::quant::{Bit4Factors, quantize_bit4};
use crate::scan::ScanOptions;
use crate::segment::writer::{SegmentBuild, SegmentFactors, write_segment_with_graph};
use crate::segment::{SegmentId, SegmentMeta};
use crate::vfs::StdVfs;

use super::{
    CancelToken, GraphSearchOptions, OpenOptions, QueryControl, QueryError, SearchOptions,
    SearchTier, Store,
};

const DIMS: usize = 128;
const ROWS: usize = 32;
const K: usize = 4;

#[derive(Clone, Copy)]
enum GraphShape {
    Complete,
    Chain,
}

struct SegmentSpec {
    vectors: Vec<f32>,
    tombstones: Vec<u32>,
    shape: GraphShape,
}

struct MultiSegmentFixture {
    directory: TempDir,
}

fn policy() -> DurabilityPolicy {
    DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None)
        .expect("fixture durability policy")
}

fn columns(rows: usize) -> crate::meta::ColumnStore {
    let schema = Schema::new(Vec::new()).expect("timestamp-only schema");
    let mut builder = ColumnStoreBuilder::new(schema);
    for row in 0..rows {
        builder
            .push_row(row as i64, &[])
            .expect("fixture metadata row");
    }
    builder.finish().expect("fixture columns")
}

fn quantize_rows(vectors: &[f32]) -> (Vec<u8>, Vec<Bit4Factors>) {
    let rows = vectors.len() / DIMS;
    let row_bytes = DIMS.div_ceil(2);
    let mut codes = vec![0_u8; rows * row_bytes];
    let mut factors = Vec::with_capacity(rows);
    for (row, encoded) in vectors
        .chunks_exact(DIMS)
        .zip(codes.chunks_exact_mut(row_bytes))
    {
        factors.push(quantize_bit4(row, encoded).expect("finite fixture vector"));
    }
    (codes, factors)
}

fn neighbors(rows: usize, shape: GraphShape) -> Vec<Vec<u32>> {
    match shape {
        GraphShape::Complete => (0..rows)
            .map(|row| {
                (0..rows)
                    .filter(|candidate| *candidate != row)
                    .map(|candidate| u32::try_from(candidate).expect("fixture row fits u32"))
                    .collect()
            })
            .collect(),
        GraphShape::Chain => (0..rows)
            .map(|row| {
                u32::try_from(row.saturating_add(1))
                    .ok()
                    .filter(|next| *next < rows as u32)
                    .into_iter()
                    .collect()
            })
            .collect(),
    }
}

fn publish_graph_store(specs: Vec<SegmentSpec>) -> MultiSegmentFixture {
    let directory = tempdir().expect("multi-segment graph directory");
    let mut metas = Vec::with_capacity(specs.len());
    for (segment_index, spec) in specs.iter().enumerate() {
        assert!(spec.vectors.len().is_multiple_of(DIMS));
        let rows = spec.vectors.len() / DIMS;
        assert!(rows >= 4);
        let id = SegmentId::new(
            0x0001_9100_0000 + segment_index as u64,
            [u8::try_from(segment_index).expect("fixture segment index fits u8"); 10],
        );
        let (codes, factors) = quantize_rows(&spec.vectors);
        let columns = columns(rows);
        let mut alive = AliveSet::new(rows as u32);
        for &row in &spec.tombstones {
            alive.tombstone(row).expect("fixture tombstone");
        }
        let neighbors = neighbors(rows, spec.shape);
        let nodes = codes
            .chunks_exact(DIMS.div_ceil(2))
            .zip(&factors)
            .zip(&neighbors)
            .enumerate()
            .map(|(row, ((codes, factors), neighbors))| GraphNodeBlockInput {
                codes,
                factors: *factors,
                flags: u8::from(row < 4),
                neighbors,
            })
            .collect::<Vec<_>>();
        let max_degree = match spec.shape {
            GraphShape::Complete => u8::try_from(rows - 1).expect("complete degree fits u8"),
            GraphShape::Chain => 1,
        };
        let meta = write_segment_with_graph(
            &StdVfs,
            directory.path(),
            SegmentBuild {
                id,
                scheme: 4,
                dims: DIMS as u32,
                codes: &codes,
                factors: SegmentFactors::Bit4(&factors),
                rescore: &spec.vectors,
                columns: &columns,
                alive: &alive,
            },
            GraphNodeBlockBuild {
                layout: GraphNodeLayout::new(DIMS as u32, DIMS as u32, max_degree)
                    .expect("fixture graph layout"),
                nodes: &nodes,
            },
            policy(),
        )
        .expect("write graph segment");
        metas.push(meta);
    }
    commit(&directory, metas);
    MultiSegmentFixture { directory }
}

fn commit(directory: &TempDir, segments: Vec<SegmentMeta>) {
    commit_manifest(
        &StdVfs,
        directory.path(),
        &Manifest {
            generation: 1,
            log_seq: 0,
            segments,
            epochs: Vec::new(),
            schema: Schema::new(Vec::new()).expect("fixture schema"),
        },
        policy(),
    )
    .expect("commit fixture manifest");
}

fn directional_vectors(center: f32, rows: usize, random: &mut impl Rng) -> Vec<f32> {
    let mut vectors = Vec::with_capacity(rows * DIMS);
    for row in 0..rows {
        let amplitude = center + row as f32 * 0.01 + random.random_range(-0.002_f32..=0.002_f32);
        for dimension in 0..DIMS {
            let sign = if dimension.is_multiple_of(2) {
                1.0
            } else {
                -1.0
            };
            let noise = random.random_range(-0.0001_f32..=0.0001_f32);
            vectors.push(amplitude * sign + noise);
        }
    }
    vectors
}

fn chain_vectors(rows: usize) -> Vec<f32> {
    let mut vectors = Vec::with_capacity(rows * DIMS);
    for row in 0..rows {
        let amplitude = row as f32 * 0.01;
        for dimension in 0..DIMS {
            vectors.push(if dimension.is_multiple_of(2) {
                amplitude
            } else {
                -amplitude
            });
        }
    }
    vectors
}

fn query(amplitude: f32) -> Vec<f32> {
    (0..DIMS)
        .map(|dimension| {
            if dimension.is_multiple_of(2) {
                amplitude
            } else {
                -amplitude
            }
        })
        .collect()
}

fn graph_options(rows: usize) -> SearchOptions {
    SearchOptions::new(ScanOptions { thread_budget: 1 }).with_tier(SearchTier::Graph(
        GraphSearchOptions::new(GraphSearchProfile::SiftClass)
            .with_ef(rows)
            .with_seed(0x19_0000_0007),
    ))
}

fn independent(store: &Store, query: &[f32], k: usize, rows: usize) -> SearchOutcome {
    store
        .search_independent_for_test(
            SearchRequest::new(query),
            k,
            graph_options(rows),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("independent graph traversal")
}

fn shared(store: &Store, query: &[f32], k: usize, rows: usize) -> SearchOutcome {
    store
        .search(
            SearchRequest::new(query),
            k,
            graph_options(rows),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("shared-bound graph traversal")
}

fn candidate_bytes(outcome: &SearchOutcome) -> Vec<(GlobalRowId, u32)> {
    outcome
        .candidates
        .iter()
        .map(|candidate| (candidate.row_id(), candidate.score().to_bits()))
        .collect()
}

fn separated_specs(random: &mut impl Rng, centers: &[f32]) -> Vec<SegmentSpec> {
    centers
        .iter()
        .map(|center| SegmentSpec {
            vectors: directional_vectors(*center, ROWS, random),
            tombstones: Vec::new(),
            shape: GraphShape::Complete,
        })
        .collect()
}

#[test]
fn multi_segment_graph_search_matches_the_independent_traversal_exactly() {
    let mut random = crate::test_support::seeded_rng(
        "lifecycle::shared_bound_tests::multi_segment_graph_search_matches_the_independent_traversal_exactly",
    );
    let mut pruned_segments = 0_usize;
    for store_index in 0..3 {
        let offset = store_index as f32 * 0.1;
        let fixture = publish_graph_store(separated_specs(
            &mut random,
            &[
                1.0 + offset,
                20.0 + offset,
                200.0 + offset,
                2_000.0 + offset,
            ],
        ));
        let store = Store::open(fixture.directory.path(), OpenOptions::default())
            .expect("open randomized multi-segment store");
        for amplitude in [
            1.05 + offset,
            20.08 + offset,
            199.9 + offset,
            1_999.8 + offset,
        ] {
            let query = query(amplitude);
            let expected = independent(&store, &query, 6, ROWS);
            let actual = shared(&store, &query, 6, ROWS);
            assert_eq!(candidate_bytes(&actual), candidate_bytes(&expected));
            pruned_segments += actual.graph_stats.segments_pruned_by_bound;
        }
        store.close().expect("close randomized store");
    }
    assert!(
        pruned_segments > 0,
        "shared path never applied a segment bound: pruned={pruned_segments}"
    );
}

#[test]
fn shared_bound_reduces_candidates_scored_across_segments() {
    let mut random = crate::test_support::seeded_rng(
        "lifecycle::shared_bound_tests::shared_bound_reduces_candidates_scored_across_segments",
    );
    let fixture = publish_graph_store(separated_specs(&mut random, &[1.0, 100.0, 1_000.0]));
    let store = Store::open(fixture.directory.path(), OpenOptions::default())
        .expect("open counter fixture");
    let query = query(1.07);
    let expected = independent(&store, &query, K, ROWS);
    let actual = shared(&store, &query, K, ROWS);

    assert_eq!(candidate_bytes(&actual), candidate_bytes(&expected));
    assert_eq!(expected.graph_stats.candidates_scored, 3 * ROWS);
    assert_eq!(expected.graph_stats.candidates_rescored, 3 * ROWS);
    assert_eq!(actual.graph_stats.candidates_scored, ROWS);
    assert_eq!(actual.graph_stats.candidates_rescored, ROWS);
    assert!(
        actual.graph_stats.candidates_scored < expected.graph_stats.candidates_scored,
        "candidates scored did not fall: independent={} shared={}",
        expected.graph_stats.candidates_scored,
        actual.graph_stats.candidates_scored
    );
    assert!(
        actual.graph_stats.candidates_rescored < expected.graph_stats.candidates_rescored,
        "candidates rescored did not fall: independent={} shared={}",
        expected.graph_stats.candidates_rescored,
        actual.graph_stats.candidates_rescored
    );
    store.close().expect("close counter fixture");
}

#[test]
fn shared_bound_does_not_leak_between_concurrent_queries() {
    let mut random = crate::test_support::seeded_rng(
        "lifecycle::shared_bound_tests::shared_bound_does_not_leak_between_concurrent_queries",
    );
    let fixture = publish_graph_store(separated_specs(&mut random, &[1.0, 100.0, 1_000.0]));
    let store = Arc::new(
        Store::open(fixture.directory.path(), OpenOptions::default())
            .expect("open concurrent fixture"),
    );
    let first_query = query(1.07);
    let last_query = query(999.9);
    let first_expected = candidate_bytes(&independent(&store, &first_query, K, ROWS));
    let last_expected = candidate_bytes(&independent(&store, &last_query, K, ROWS));
    let barrier = Arc::new(Barrier::new(3));
    let mut handles = Vec::new();
    for (query, expected) in [(first_query, first_expected), (last_query, last_expected)] {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            let actual = shared(&store, &query, K, ROWS);
            (actual, expected)
        }));
    }
    barrier.wait();
    let mut pruned_segments = 0_usize;
    for handle in handles {
        let (actual, expected) = handle.join().expect("concurrent query thread");
        assert_eq!(candidate_bytes(&actual), expected);
        pruned_segments += actual.graph_stats.segments_pruned_by_bound;
    }
    assert!(
        pruned_segments > 0,
        "concurrent shared searches never installed query-local bounds"
    );
    store.close().expect("close concurrent fixture");
}

#[test]
fn a_later_segment_that_cannot_beat_the_bound_does_almost_no_work() {
    let mut random = crate::test_support::seeded_rng(
        "lifecycle::shared_bound_tests::a_later_segment_that_cannot_beat_the_bound_does_almost_no_work",
    );
    let fixture = publish_graph_store(separated_specs(&mut random, &[1.0, 1_000.0]));
    let store = Store::open(fixture.directory.path(), OpenOptions::default())
        .expect("open late-segment fixture");
    let query = query(1.07);
    let actual = shared(&store, &query, K, ROWS);

    assert_eq!(
        actual.graph_stats.candidates_scored, ROWS,
        "later segment scored candidates despite a decisive bound"
    );
    assert_eq!(
        actual.graph_stats.candidates_rescored, ROWS,
        "later segment rescored candidates despite a decisive bound"
    );
    assert_eq!(actual.graph_stats.segments_pruned_by_bound, 1);
    store.close().expect("close late-segment fixture");
}

#[test]
fn shared_bound_still_returns_full_k_when_rows_are_tombstoned() {
    let mut random = crate::test_support::seeded_rng(
        "lifecycle::shared_bound_tests::shared_bound_still_returns_full_k_when_rows_are_tombstoned",
    );
    let mut specs = separated_specs(&mut random, &[1.0, 1_000.0]);
    specs[0].tombstones = vec![5, 6, 7];
    let fixture = publish_graph_store(specs);
    let store = Store::open(fixture.directory.path(), OpenOptions::default())
        .expect("open tombstone fixture");
    let query = query(1.06);
    let expected = independent(&store, &query, K, ROWS);
    let actual = shared(&store, &query, K, ROWS);

    assert_eq!(candidate_bytes(&actual), candidate_bytes(&expected));
    assert_eq!(actual.candidates.len(), K);
    assert!(
        actual.graph_stats.segments_pruned_by_bound > 0,
        "tombstone over-fetch never reached a shared-bound prune"
    );
    store.close().expect("close tombstone fixture");
}

#[test]
fn multi_segment_graph_search_honours_cancellation_mid_segment() {
    const CHAIN_ROWS: usize = 40_000;
    let vectors = chain_vectors(CHAIN_ROWS);
    let fixture = publish_graph_store(vec![
        SegmentSpec {
            vectors: vectors.clone(),
            tombstones: Vec::new(),
            shape: GraphShape::Chain,
        },
        SegmentSpec {
            vectors,
            tombstones: Vec::new(),
            shape: GraphShape::Chain,
        },
    ]);
    let store = Arc::new(
        Store::open(fixture.directory.path(), OpenOptions::default())
            .expect("open cancellation fixture"),
    );
    let cancellation_query = query((CHAIN_ROWS - 1) as f32 * 0.01);
    let token = CancelToken::new();
    let query_token = token.clone();
    let query_store = Arc::clone(&store);
    let handle = std::thread::spawn(move || {
        query_store.search(
            SearchRequest::new(&cancellation_query),
            K,
            graph_options(CHAIN_ROWS),
            QueryControl::Cancel(query_token),
        )
    });
    let one_scratch = GraphSearchScratch::allocation_bytes(CHAIN_ROWS as u32, 1, CHAIN_ROWS)
        .expect("chain scratch geometry") as u64;
    loop {
        if store.stats().expect("in-flight graph stats").cache_bytes
            >= one_scratch.saturating_mul(2)
        {
            break;
        }
        assert!(
            !handle.is_finished(),
            "multi-segment query completed before entering its second traversal"
        );
        std::thread::yield_now();
    }
    token.cancel();
    let error = handle
        .join()
        .expect("cancellation query thread")
        .expect_err("cancelled multi-segment graph query returned candidates");
    assert!(matches!(error, QueryError::Cancelled { partial: false }));

    let mut random = crate::test_support::seeded_rng(
        "lifecycle::shared_bound_tests::multi_segment_graph_search_honours_cancellation_mid_segment",
    );
    let bound_fixture = publish_graph_store(separated_specs(&mut random, &[1.0, 1_000.0]));
    let bound_store = Store::open(bound_fixture.directory.path(), OpenOptions::default())
        .expect("open bound probe fixture");
    let bounded = shared(&bound_store, &query(1.07), K, ROWS);
    assert!(
        bounded.graph_stats.segments_pruned_by_bound > 0,
        "cancellation path was not exercised alongside an active shared bound"
    );
    bound_store.close().expect("close bound probe fixture");
    store.close().expect("close cancellation fixture");
}
