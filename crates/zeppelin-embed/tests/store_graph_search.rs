#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::sync::{Arc, Barrier};

use tempfile::{TempDir, tempdir};
use zeppelin_embed::graph::GraphParams;
use zeppelin_embed::graph::block::{GraphNodeBlockBuild, GraphNodeBlockInput, GraphNodeLayout};
use zeppelin_embed::graph::build::{
    CheckpointedGraphBuild, GraphBuildPasses, build_graph_checkpointed,
};
use zeppelin_embed::graph::search::{GraphSearchProfile, GraphSearchScratch};
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, RowSource, SearchRequest,
};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::{
    CancelToken, Deadline, GraphSearchOptions, OpenOptions, QueryControl, QueryError,
    SearchOptions, SearchTier, Store, StoreError,
};
use zeppelin_embed::manifest::Manifest;
use zeppelin_embed::manifest::io::commit_manifest;
use zeppelin_embed::meta::{AliveSet, ColumnStoreBuilder, Schema};
use zeppelin_embed::quant::{Bit4Factors, quantize_bit4};
use zeppelin_embed::scan::ScanOptions;
use zeppelin_embed::segment::writer::{
    SegmentBuild, SegmentFactors, write_segment, write_segment_with_graph,
};
use zeppelin_embed::segment::{SegmentId, SegmentMeta};
use zeppelin_embed::vfs::StdVfs;

const DIMS: usize = 128;
const ROWS: usize = 12;

struct GraphFixture {
    directory: TempDir,
    id: SegmentId,
    vectors: Vec<f32>,
}

fn policy() -> DurabilityPolicy {
    DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None).expect("fixture policy")
}

fn columns(rows: usize) -> zeppelin_embed::meta::ColumnStore {
    let schema = Schema::new(Vec::new()).expect("timestamp-only schema");
    let mut builder = ColumnStoreBuilder::new(schema);
    for row in 0..rows {
        builder
            .push_row(row as i64, &[])
            .expect("fixture metadata row");
    }
    builder.finish().expect("fixture columns")
}

fn fixture_vectors(rows: usize) -> Vec<f32> {
    let mut vectors = Vec::with_capacity(rows * DIMS);
    for row in 0..rows {
        let amplitude = row as f32;
        for dimension in 0..DIMS {
            let sign = if dimension.is_multiple_of(2) {
                1.0
            } else {
                -1.0
            };
            vectors.push(amplitude * sign);
        }
    }
    vectors
}

fn quantize_rows(vectors: &[f32], rows: usize) -> (Vec<u8>, Vec<Bit4Factors>) {
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

fn publish_graph_fixture(alive: AliveSet) -> GraphFixture {
    let directory = tempdir().expect("graph store directory");
    let input_id = SegmentId::new(0x0001_9000_0000, [0x30; 10]);
    let id = SegmentId::new(0x0001_9000_0001, [0x31; 10]);
    let vectors = fixture_vectors(ROWS);
    let (codes, factors) = quantize_rows(&vectors, ROWS);
    let columns = columns(ROWS);
    alive.debug_assert_consistent();
    let input_meta = write_segment(
        &StdVfs,
        directory.path(),
        SegmentBuild {
            id: input_id,
            scheme: 4,
            dims: DIMS as u32,
            codes: &codes,
            factors: SegmentFactors::Bit4(&factors),
            rescore: &vectors,
            columns: &columns,
            alive: &alive,
        },
        policy(),
    )
    .expect("sealed graph-build input");
    commit(&directory, &columns, input_meta, 1);

    let builder =
        Store::open(directory.path(), OpenOptions::default()).expect("open graph builder");
    let lease = builder.snapshot().expect("graph-build snapshot");
    let input = lease.segments().first().expect("graph-build input segment");
    let build_control = QueryControl::Cancel(CancelToken::new());
    let checkpoint = directory.path().join("fixture.graph.checkpoint");
    let artifact = build_graph_checkpointed(
        &builder,
        input,
        CheckpointedGraphBuild::new(
            GraphParams::new(4, (ROWS - 1) as u8, 1.0, 1.2, ROWS as u16, ROWS as u32)
                .expect("fixture graph params"),
            0x00c0_ffee,
            GraphBuildPasses::One,
            &checkpoint,
            &build_control,
        ),
        &lease,
    )
    .expect("build fixture graph");
    let meta = artifact
        .write_segment_with_graph(&StdVfs, directory.path(), input, id, policy())
        .expect("sealed graph segment");
    drop(artifact);
    drop(lease);
    builder.close().expect("close graph builder");
    commit(&directory, &columns, meta, 2);
    GraphFixture {
        directory,
        id,
        vectors,
    }
}

fn publish_segment_without_graph() -> (TempDir, SegmentId) {
    let directory = tempdir().expect("scan-only store directory");
    let id = SegmentId::new(0x0001_9000_0002, [0x32; 10]);
    let vectors = fixture_vectors(ROWS);
    let (codes, factors) = quantize_rows(&vectors, ROWS);
    let columns = columns(ROWS);
    let alive = AliveSet::new(ROWS as u32);
    let meta = write_segment(
        &StdVfs,
        directory.path(),
        SegmentBuild {
            id,
            scheme: 4,
            dims: DIMS as u32,
            codes: &codes,
            factors: SegmentFactors::Bit4(&factors),
            rescore: &vectors,
            columns: &columns,
            alive: &alive,
        },
        policy(),
    )
    .expect("sealed segment without graph");
    commit(&directory, &columns, meta, 1);
    (directory, id)
}

fn publish_long_chain_graph(rows: usize) -> (TempDir, SegmentId) {
    let directory = tempdir().expect("cancellation graph directory");
    let id = SegmentId::new(0x0001_9000_0003, [0x33; 10]);
    let vectors = fixture_vectors(rows);
    let (codes, factors) = quantize_rows(&vectors, rows);
    let columns = columns(rows);
    let alive = AliveSet::new(rows as u32);
    let neighbors = (0..rows)
        .map(|row| {
            u32::try_from(row + 1)
                .ok()
                .filter(|next| *next < rows as u32)
                .into_iter()
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
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
    let meta = write_segment_with_graph(
        &StdVfs,
        directory.path(),
        SegmentBuild {
            id,
            scheme: 4,
            dims: DIMS as u32,
            codes: &codes,
            factors: SegmentFactors::Bit4(&factors),
            rescore: &vectors,
            columns: &columns,
            alive: &alive,
        },
        GraphNodeBlockBuild {
            layout: GraphNodeLayout::new(DIMS as u32, DIMS as u32, 1).expect("chain graph layout"),
            nodes: &nodes,
        },
        policy(),
    )
    .expect("sealed chain graph");
    commit(&directory, &columns, meta, 1);
    (directory, id)
}

fn commit(
    directory: &TempDir,
    columns: &zeppelin_embed::meta::ColumnStore,
    meta: SegmentMeta,
    generation: u64,
) {
    commit_manifest(
        &StdVfs,
        directory.path(),
        &Manifest {
            generation,
            log_seq: 0,
            segments: vec![meta],
            epochs: Vec::new(),
            schema: columns.schema().clone(),
        },
        policy(),
    )
    .expect("fixture manifest");
}

fn graph_options(ef: usize) -> SearchOptions {
    SearchOptions::new(ScanOptions { thread_budget: 1 }).with_tier(SearchTier::Graph(
        GraphSearchOptions::new(GraphSearchProfile::SiftClass)
            .with_ef(ef)
            .with_seed(0x00c0_ffee),
    ))
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

fn brute_force(vectors: &[f32], query: &[f32], k: usize) -> Vec<u32> {
    let mut rows = vectors
        .chunks_exact(DIMS)
        .enumerate()
        .map(|(row, vector)| {
            let distance = vector
                .iter()
                .zip(query)
                .map(|(left, right)| {
                    let delta = f64::from(*left) - f64::from(*right);
                    delta * delta
                })
                .sum::<f64>();
            (row as u32, distance)
        })
        .collect::<Vec<_>>();
    rows.sort_unstable_by(|left, right| {
        left.1
            .total_cmp(&right.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    rows.truncate(k);
    rows.into_iter().map(|(row, _)| row).collect()
}

#[test]
fn store_search_reaches_the_graph_tier_end_to_end() {
    let fixture = publish_graph_fixture(AliveSet::new(ROWS as u32));
    let store = Store::open(fixture.directory.path(), OpenOptions::default()).expect("open store");
    let query = query(5.25);
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(1), Revision::new(1)),
            query.clone(),
        )]))
        .expect("ingest active graph-tier row");

    let outcome = store
        .search(
            SearchRequest::new(&query),
            4,
            graph_options(ROWS),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("graph-tier store search");

    let actual = outcome
        .candidates
        .iter()
        .map(|candidate| (candidate.row_id().source(), candidate.row_id().local_row()))
        .collect::<Vec<_>>();
    let mut expected = vec![(RowSource::Active, 0)];
    expected.extend(
        brute_force(&fixture.vectors, &query, 3)
            .into_iter()
            .map(|row| (RowSource::Sealed(fixture.id), row)),
    );
    assert_eq!(actual, expected);
    assert_eq!(outcome.graph_stats.segments_traversed, 1);
    assert_eq!(outcome.graph_stats.candidates_rescored, ROWS);
    store.close().expect("close store");
}

#[test]
fn graph_tier_reuses_scratch_and_entry_seeds_across_queries() {
    let fixture = publish_graph_fixture(AliveSet::new(ROWS as u32));
    let store = Arc::new(
        Store::open(fixture.directory.path(), OpenOptions::default()).expect("open graph store"),
    );
    let query = query(4.5);

    let first = store
        .search(
            SearchRequest::new(&query),
            3,
            graph_options(ROWS),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("first graph query");
    let second = store
        .search(
            SearchRequest::new(&query),
            3,
            graph_options(ROWS),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("second graph query");

    assert_eq!(first.graph_stats.entry_seed_discoveries, 1);
    assert_eq!(second.graph_stats.entry_seed_discoveries, 0);
    assert_eq!(first.graph_stats.graph_validations, 1);
    assert_eq!(second.graph_stats.graph_validations, 0);
    assert_eq!(first.graph_stats.visited_epoch_clears, 0);
    assert_eq!(second.graph_stats.visited_epoch_clears, 0);

    let barrier = Arc::new(Barrier::new(3));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        let query = query.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            store.search(
                SearchRequest::new(&query),
                3,
                graph_options(ROWS),
                QueryControl::Cancel(CancelToken::new()),
            )
        }));
    }
    barrier.wait();
    for handle in handles {
        let outcome = handle.join().expect("concurrent query thread");
        assert_eq!(outcome.expect("concurrent graph query").candidates.len(), 3);
    }
    store.close().expect("close graph store");
}

#[test]
fn graph_tier_scratch_is_exactly_accounted() {
    let fixture = publish_graph_fixture(AliveSet::new(ROWS as u32));
    let store = Store::open(fixture.directory.path(), OpenOptions::default()).expect("open store");
    let before = store.stats().expect("stats before graph query");
    let query = query(3.5);

    store
        .search(
            SearchRequest::new(&query),
            3,
            graph_options(ROWS),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("graph query");

    let after = store.stats().expect("stats after graph query");
    let expected = GraphSearchScratch::allocation_bytes(ROWS as u32, (ROWS - 1) as u8, ROWS)
        .expect("scratch byte geometry") as u64;
    assert_eq!(after.cache_bytes - before.cache_bytes, expected);
    assert_eq!(
        after.resident_owned_bytes - before.resident_owned_bytes,
        expected
    );
    store.close().expect("close releases scratch");

    let reopened = Store::open(fixture.directory.path(), OpenOptions::read_only())
        .expect("reopen after close");
    let released = reopened.stats().expect("stats after close and reopen");
    assert_eq!(released.cache_bytes, before.cache_bytes);
    reopened.close().expect("close reopened store");
}

#[test]
fn graph_tier_rejects_a_sealed_segment_without_a_graph() {
    let (directory, segment_id) = publish_segment_without_graph();
    let store =
        Store::open(directory.path(), OpenOptions::default()).expect("open scan-only store");
    let query = query(1.0);

    let error = store
        .search(
            SearchRequest::new(&query),
            3,
            graph_options(ROWS),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect_err("explicit graph tier must reject a missing graph");

    assert!(matches!(
        error,
        QueryError::Store(StoreError::GraphUnavailable { segment_id: actual })
            if actual == segment_id
    ));
    store.close().expect("close store");
}

#[test]
fn graph_tier_query_honours_cancellation() {
    const CHAIN_ROWS: usize = 40_000;
    let (directory, _) = publish_long_chain_graph(CHAIN_ROWS);
    let store =
        Arc::new(Store::open(directory.path(), OpenOptions::default()).expect("open store"));
    let query = query((CHAIN_ROWS - 1) as f32);
    let token = CancelToken::new();
    let query_token = token.clone();
    let query_store = Arc::clone(&store);
    let query_vector = query.clone();
    let handle = std::thread::spawn(move || {
        query_store.search(
            SearchRequest::new(&query_vector),
            4,
            graph_options(CHAIN_ROWS),
            QueryControl::Cancel(query_token),
        )
    });
    loop {
        if store.stats().expect("in-flight graph stats").cache_bytes > 0 {
            break;
        }
        assert!(
            !handle.is_finished(),
            "graph query completed before its scratch became observable"
        );
        std::thread::yield_now();
    }
    token.cancel();
    let error = handle
        .join()
        .expect("graph cancellation thread")
        .expect_err("cancelled graph query must not return candidates");

    assert!(matches!(error, QueryError::Cancelled { partial: false }));

    let timeout = store
        .search(
            SearchRequest::new(&query),
            4,
            graph_options(CHAIN_ROWS),
            QueryControl::Deadline(
                Deadline::after(std::time::Duration::ZERO).expect("zero deadline"),
            ),
        )
        .expect_err("expired graph deadline must not return candidates");
    assert!(matches!(timeout, QueryError::Timeout { partial: false }));
    store.close().expect("close store");
}

#[test]
fn graph_tier_never_returns_a_tombstoned_row() {
    let mut alive = AliveSet::new(ROWS as u32);
    alive.tombstone(5).expect("fixture tombstone");
    let fixture = publish_graph_fixture(alive);
    let store = Store::open(fixture.directory.path(), OpenOptions::default()).expect("open store");
    let query = query(5.0);

    let outcome = store
        .search(
            SearchRequest::new(&query),
            4,
            graph_options(ROWS),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("graph search with tombstone");

    assert_eq!(outcome.graph_stats.segments_traversed, 1);
    assert_eq!(
        outcome.candidates.len(),
        4,
        "requested k=4 with one tombstoned row"
    );
    assert!(
        outcome
            .candidates
            .iter()
            .all(|candidate| candidate.row_id().local_row() != 5)
    );
    store.close().expect("close store");
}

#[test]
fn graph_tier_returns_full_k_when_many_rows_are_tombstoned() {
    let mut alive = AliveSet::new(ROWS as u32);
    for row in [5, 4, 6] {
        alive.tombstone(row).expect("fixture tombstone");
    }
    let fixture = publish_graph_fixture(alive);
    let store = Store::open(fixture.directory.path(), OpenOptions::default()).expect("open store");
    let query = query(5.0);

    let outcome = store
        .search(
            SearchRequest::new(&query),
            4,
            graph_options(ROWS),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("graph search with several tombstones at the front of the ranking");

    assert!(
        outcome
            .candidates
            .iter()
            .all(|candidate| ![5, 4, 6].contains(&candidate.row_id().local_row()))
    );
    assert_eq!(
        outcome.candidates.len(),
        4,
        "requested k=4 with three tombstoned rows at the front of the ranking"
    );
    store.close().expect("close store");
}
