#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::sync::{Arc, Barrier};

use tempfile::{TempDir, tempdir};
use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, Normalization, StoreEpoch,
};
use zeppelin_embed::fts::tokenizer::TokenizerConfig;
use zeppelin_embed::graph::GraphParams;
use zeppelin_embed::graph::block::{GraphNodeBlockBuild, GraphNodeBlockInput, GraphNodeLayout};
use zeppelin_embed::graph::build::{
    CheckpointedGraphBuild, GraphBuildError, GraphBuildPasses, build_graph_checkpointed,
};
use zeppelin_embed::graph::search::{
    EpochGraphProfile, GraphDistanceMetric, GraphProfileError, GraphSearchProfile,
    GraphSearchScratch, select_epoch_graph_profile,
};
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, IngestError, Revision, RowSource,
    SearchRequest,
};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::{
    CancelToken, Deadline, GraphSearchOptions, OpenOptions, QueryControl, QueryError,
    SearchOptions, SearchTier, Store, StoreError,
};
use zeppelin_embed::manifest::Manifest;
use zeppelin_embed::manifest::io::commit_manifest;
use zeppelin_embed::meta::{
    AliveSet, ColumnStoreBuilder, Predicate, PredicateValue, RangeBound, RangePredicate, Schema,
    TIMESTAMP_COLUMN,
};
use zeppelin_embed::planner::{PlanFallback, SegmentBranch, SegmentTier};
use zeppelin_embed::quant::QuantError;
use zeppelin_embed::quant::{Bit4Factors, quantize_bit4};
use zeppelin_embed::scan::ScanOptions;
use zeppelin_embed::segment::reader::SegmentReader;
use zeppelin_embed::segment::writer::{
    SegmentBuild, SegmentFactors, write_segment, write_segment_with_graph,
};
use zeppelin_embed::segment::{SegmentId, SegmentMeta};
use zeppelin_embed::vfs::StdVfs;

const DIMS: usize = 128;
const ROWS: usize = 12;
const BEST_FIRST_ROWS: usize = 145;
const BEST_FIRST_EF: usize = 140;

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

fn unit_fixture_vectors(rows: usize) -> Vec<f32> {
    let mut vectors = vec![0.0_f32; rows * DIMS];
    for row in 0..rows {
        vectors[row * DIMS + row] = 1.0;
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
    publish_graph_fixture_with_epoch(alive, None)
}

fn publish_graph_fixture_with_epoch(alive: AliveSet, epoch: Option<&StoreEpoch>) -> GraphFixture {
    let directory = tempdir().expect("graph store directory");
    let input_id = SegmentId::new(0x0001_9000_0000, [0x30; 10]);
    let id = SegmentId::new(0x0001_9000_0001, [0x31; 10]);
    let vectors = if epoch
        .is_some_and(|declared| declared.embedding.document.normalization == Normalization::L2)
    {
        unit_fixture_vectors(ROWS)
    } else {
        fixture_vectors(ROWS)
    };
    let (codes, factors) = quantize_rows(&vectors, ROWS);
    let columns = columns(ROWS);
    alive.debug_assert_consistent();
    let mut input_meta = write_segment(
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
    input_meta.epoch_id = epoch.map(|declared| declared.identity().embedding);
    commit_with_epoch(&directory, &columns, input_meta, 1, epoch);

    let options = epoch.map_or_else(OpenOptions::default, |declared| {
        OpenOptions::default().with_epoch(declared.clone())
    });
    let builder = Store::open(directory.path(), options).expect("open graph builder");
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
    let mut meta = artifact
        .write_segment_with_graph(&StdVfs, directory.path(), input, id, policy())
        .expect("sealed graph segment");
    meta.epoch_id = epoch.map(|declared| declared.identity().embedding);
    drop(artifact);
    drop(lease);
    builder.close().expect("close graph builder");
    commit_with_epoch(&directory, &columns, meta, 2, epoch);
    GraphFixture {
        directory,
        id,
        vectors,
    }
}

fn graph_epoch(normalization: Normalization) -> StoreEpoch {
    let document = EmbeddingTower {
        model_id: "graph-profile-fixture".to_owned(),
        model_version: "1".to_owned(),
        weights_digest: vec![0x16],
        dims: DIMS as u32,
        normalization,
        prompt_prefix: String::new(),
        max_tokens: 512,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    StoreEpoch {
        embedding: EmbeddingEpoch {
            query: document.clone(),
            document,
            alignment_digest: Vec::new(),
        },
        tokenizer: TokenizerConfig::text_default().epoch(),
    }
}

#[test]
fn auto_profile_follows_epoch_normalization() {
    let epoch = graph_epoch(Normalization::L2);
    let fixture = publish_graph_fixture_with_epoch(AliveSet::new(ROWS as u32), Some(&epoch));
    let store = Store::open(
        fixture.directory.path(),
        OpenOptions::default().with_epoch(epoch.clone()),
    )
    .expect("open normalized graph store");

    let outcome = store
        .search(
            SearchRequest::new(&unit_fixture_vectors(1)),
            1,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("normalized epoch selects the angular graph profile");

    assert_eq!(
        outcome.diagnostics.plan[0].graph_profile,
        Some(GraphSearchProfile::Angular)
    );
    store.close().expect("close normalized graph store");
}

#[test]
fn epoch_profile_selection_admits_matching_towers_and_refuses_mixed_towers() {
    let sift = graph_epoch(Normalization::None);
    let angular = graph_epoch(Normalization::L2);
    let sift_meta = (&sift).into();
    let angular_meta = (&angular).into();

    assert_eq!(
        select_epoch_graph_profile(&sift_meta, GraphDistanceMetric::SquaredL2),
        Ok(EpochGraphProfile::SiftClass)
    );
    let angular_profile = select_epoch_graph_profile(&angular_meta, GraphDistanceMetric::SquaredL2)
        .expect("matching L2 towers select angular");
    assert_eq!(
        angular_profile.search_profile(),
        GraphSearchProfile::Angular
    );
    assert_eq!(angular_profile.build_params().r_target(), 48);
    assert_eq!(angular_profile.build_params().r_max(), 64);

    for (document, query) in [
        (Normalization::L2, Normalization::None),
        (Normalization::None, Normalization::L2),
    ] {
        let mut mixed = graph_epoch(document);
        mixed.embedding.query.normalization = query;
        let mixed_meta = (&mixed).into();
        assert!(matches!(
            select_epoch_graph_profile(&mixed_meta, GraphDistanceMetric::SquaredL2),
            Err(GraphProfileError::UnrecognizedEpochShape { .. })
        ));
    }
}

#[test]
fn angular_ingest_refuses_a_non_unit_document_before_mutation() {
    let epoch = graph_epoch(Normalization::L2);
    let directory = tempdir().expect("angular ingest directory");
    let store = Store::open(
        directory.path(),
        OpenOptions::default().with_epoch(epoch.clone()),
    )
    .expect("open angular ingest store");
    let before = store.snapshot().expect("pre-ingest snapshot").generation();
    let mut vector = vec![0.0_f32; DIMS];
    vector[0] = 2.0;

    let error = store
        .ingest(
            IngestBatch::new(vec![IngestDocument::new(
                DocumentVersion::new(DocId::new(1), Revision::new(1)),
                vector,
            )])
            .with_epoch(epoch.identity()),
        )
        .expect_err("finite non-unit angular document must be refused");

    assert!(matches!(
        error,
        IngestError::Vector(QuantError::NonUnitNorm {
            squared_norm_bits,
            ..
        }) if f64::from_bits(squared_norm_bits) == 4.0
    ));
    assert_eq!(
        store
            .snapshot()
            .expect("post-refusal snapshot")
            .generation(),
        before
    );
    store.close().expect("close angular ingest store");
}

#[test]
fn angular_graph_build_refuses_a_non_unit_persisted_row() {
    let epoch = graph_epoch(Normalization::L2);
    let directory = tempdir().expect("angular graph-build directory");
    let input_id = SegmentId::new(0x0001_9000_0010, [0x40; 10]);
    let vectors = fixture_vectors(ROWS);
    let (codes, factors) = quantize_rows(&vectors, ROWS);
    let columns = columns(ROWS);
    let mut input_meta = write_segment(
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
            alive: &AliveSet::new(ROWS as u32),
        },
        policy(),
    )
    .expect("write non-unit angular input");
    input_meta.epoch_id = Some(epoch.identity().embedding);
    commit_with_epoch(&directory, &columns, input_meta, 1, Some(&epoch));
    let store = Store::open(directory.path(), OpenOptions::default().with_epoch(epoch))
        .expect("open non-unit angular input");
    let lease = store.snapshot().expect("angular build snapshot");
    let input = lease.segments().first().expect("angular build input");
    let checkpoint = directory.path().join("angular.graph.checkpoint");
    let control = QueryControl::Cancel(CancelToken::new());

    let result = build_graph_checkpointed(
        &store,
        input,
        CheckpointedGraphBuild::new(
            GraphParams::angular()
                .with_checkpoint_batch_rows(ROWS as u32)
                .expect("angular fixture batch"),
            0x0019_000a,
            GraphBuildPasses::One,
            &checkpoint,
            &control,
        ),
        &lease,
    );
    let error = match result {
        Ok(_) => panic!("non-unit persisted angular row must be refused"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        GraphBuildError::UnitNorm {
            row_id: 0,
            squared_norm: 0.0,
            ..
        }
    ));
    assert!(!checkpoint.exists());
    drop(lease);
    store.close().expect("close angular build store");
}

#[test]
fn genuine_sift_epoch_selects_sift_class() {
    let epoch = graph_epoch(Normalization::None);
    let fixture = publish_graph_fixture_with_epoch(AliveSet::new(ROWS as u32), Some(&epoch));
    let store = Store::open(
        fixture.directory.path(),
        OpenOptions::default().with_epoch(epoch),
    )
    .expect("open SIFT-shaped graph store");

    let outcome = store
        .search(
            SearchRequest::new(&query(0.0)),
            1,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("SIFT-shaped Auto query");

    assert_eq!(
        outcome.diagnostics.plan[0].graph_profile,
        Some(GraphSearchProfile::SiftClass)
    );
    store.close().expect("close SIFT-shaped graph store");
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

#[test]
fn a_segment_that_earned_a_graph_still_answers_column_filters() {
    let fixture = publish_graph_fixture(AliveSet::new(ROWS as u32));
    let store = Store::open(fixture.directory.path(), OpenOptions::default()).expect("open graph");
    let predicate = Predicate::Range(RangePredicate {
        column: TIMESTAMP_COLUMN,
        lower: Some(RangeBound::inclusive(PredicateValue::I64(0))),
        upper: Some(RangeBound::inclusive(PredicateValue::I64(5))),
    });
    let outcome = store
        .search_filtered(
            SearchRequest::new(&query(0.0)),
            &predicate,
            ROWS,
            adaptive_sift_graph_options(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("filtered graph query");
    assert_eq!(
        outcome
            .candidates
            .iter()
            .map(|candidate| candidate.row_id().local_row())
            .collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4, 5]
    );
    assert_eq!(outcome.plans.len(), 1);
    assert_eq!(outcome.plans[0].tier, SegmentTier::SealedGraph);
    assert_eq!(outcome.plans[0].branch, SegmentBranch::ExactAllowList);
    assert_eq!(outcome.plans[0].fallback, PlanFallback::None);
    assert!(!outcome.plans[0].approximate);
    store.close().expect("close graph store");
}

#[test]
fn the_zero_point_one_percent_cell_takes_the_exact_fallback_and_the_plan_says_so() {
    let (directory, _) = publish_long_chain_graph(1_000);
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open graph");
    let predicate = Predicate::Range(RangePredicate {
        column: TIMESTAMP_COLUMN,
        lower: Some(RangeBound::inclusive(PredicateValue::I64(999))),
        upper: Some(RangeBound::inclusive(PredicateValue::I64(999))),
    });

    let outcome = store
        .search_filtered(
            SearchRequest::new(&query(999.0)),
            &predicate,
            1,
            adaptive_sift_graph_options(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("one-in-one-thousand filtered graph query");

    assert_eq!(outcome.candidates[0].row_id().local_row(), 999);
    assert_eq!(outcome.plans[0].tier, SegmentTier::SealedGraph);
    assert_eq!(outcome.plans[0].filter_cardinality, 1);
    assert_eq!(outcome.plans[0].branch, SegmentBranch::ExactAllowList);
    assert_eq!(outcome.plans[0].fallback, PlanFallback::None);
    store.close().expect("close graph store");
}

#[test]
fn a_traversal_that_crosses_the_filtered_budget_falls_back_and_still_returns_the_true_top_k() {
    let (directory, _) = publish_long_chain_graph(1_000);
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open graph");
    let predicate = Predicate::Range(RangePredicate {
        column: TIMESTAMP_COLUMN,
        lower: Some(RangeBound::inclusive(PredicateValue::I64(935))),
        upper: Some(RangeBound::inclusive(PredicateValue::I64(999))),
    });

    let outcome = store
        .search_filtered(
            SearchRequest::new(&query(999.0)),
            &predicate,
            1,
            adaptive_sift_graph_options(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("stranded filtered graph query");

    assert_eq!(outcome.candidates[0].row_id().local_row(), 999);
    assert_eq!(outcome.plans[0].branch, SegmentBranch::GraphExactFallback);
    assert_eq!(outcome.plans[0].fallback, PlanFallback::VisitedBudget);
    assert!(!outcome.plans[0].approximate);
    assert_eq!(outcome.diagnostics.plan, outcome.plans);
    assert!(outcome.diagnostics.budget_exhausted);
    assert!(!outcome.diagnostics.approximate);
    assert!(outcome.diagnostics.exact_rescore);
    assert_eq!(outcome.diagnostics.counters.graph.segments_traversed, 1);
    store.close().expect("close graph store");
}

#[test]
fn a_widened_explicit_ef_under_a_filter_is_reported_in_the_plan() {
    let (directory, _) = publish_long_chain_graph(1_000);
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open graph");
    let predicate = Predicate::Range(RangePredicate {
        column: TIMESTAMP_COLUMN,
        lower: Some(RangeBound::inclusive(PredicateValue::I64(0))),
        upper: Some(RangeBound::inclusive(PredicateValue::I64(99))),
    });

    let outcome = store
        .search_filtered(
            SearchRequest::new(&query(0.0)),
            &predicate,
            1,
            graph_options(1),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("explicit filtered graph query");

    assert_eq!(outcome.plans[0].branch, SegmentBranch::FilteredGraph);
    assert_eq!(outcome.plans[0].ef_requested, Some(1));
    assert!(outcome.plans[0].ef_effective.is_some_and(|ef| ef > 1));
    assert_eq!(outcome.plans[0].fallback, PlanFallback::EfWidened);
    assert_eq!(outcome.diagnostics.plan, outcome.plans);
    assert!(!outcome.diagnostics.budget_exhausted);
    assert!(outcome.diagnostics.approximate);
    assert!(outcome.diagnostics.exact_rescore);
    store.close().expect("close graph store");
}

#[test]
fn a_filtered_graph_query_with_tombstones_uses_one_effective_mask() {
    let mut alive = AliveSet::new(1_000);
    alive.tombstone(999).expect("plant nearest-row tombstone");
    let (directory, _) = publish_long_chain_graph_with_alive(1_000, alive);
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open graph");
    let predicate = Predicate::Range(RangePredicate {
        column: TIMESTAMP_COLUMN,
        lower: Some(RangeBound::inclusive(PredicateValue::I64(900))),
        upper: Some(RangeBound::inclusive(PredicateValue::I64(999))),
    });

    let outcome = store
        .search_filtered(
            SearchRequest::new(&query(999.0)),
            &predicate,
            1,
            adaptive_sift_graph_options(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("filtered graph query with a tombstone");

    assert_eq!(outcome.candidates[0].row_id().local_row(), 998);
    assert_eq!(outcome.plans[0].filter_cardinality, 99);
    assert!(
        outcome
            .candidates
            .iter()
            .all(|candidate| candidate.row_id().local_row() != 999)
    );
    store.close().expect("close graph store");
}

fn publish_long_chain_graph(rows: usize) -> (TempDir, SegmentId) {
    publish_long_chain_graph_with_alive(rows, AliveSet::new(rows as u32))
}

fn publish_long_chain_graph_with_alive(rows: usize, alive: AliveSet) -> (TempDir, SegmentId) {
    let directory = tempdir().expect("cancellation graph directory");
    let id = SegmentId::new(0x0001_9000_0003, [0x33; 10]);
    let vectors = fixture_vectors(rows);
    let (codes, factors) = quantize_rows(&vectors, rows);
    let columns = columns(rows);
    alive.debug_assert_consistent();
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

fn publish_best_first_auto_graph() -> (GraphFixture, StoreEpoch) {
    let directory = tempdir().expect("best-first graph directory");
    let epoch = graph_epoch(Normalization::None);
    let id = SegmentId::new(0x0001_9000_0004, [0x34; 10]);
    let values = std::iter::once(1_000.0_f32)
        .chain([900.0, 800.0, 1.0])
        .chain((0..BEST_FIRST_EF).map(|offset| 700.0 - offset as f32))
        .chain(std::iter::once(0.0))
        .collect::<Vec<_>>();
    assert_eq!(values.len(), BEST_FIRST_ROWS);
    let vectors = values
        .iter()
        .flat_map(|value| {
            let mut row = vec![0.0_f32; DIMS];
            row[0] = *value;
            row
        })
        .collect::<Vec<_>>();
    let (codes, factors) = quantize_rows(&vectors, BEST_FIRST_ROWS);
    let columns = columns(BEST_FIRST_ROWS);
    let alive = AliveSet::new(BEST_FIRST_ROWS as u32);
    let mut neighbors = vec![Vec::new(); BEST_FIRST_ROWS];
    neighbors[0] = (4_u32..BEST_FIRST_ROWS as u32 - 1).collect();
    neighbors[3] = vec![(BEST_FIRST_ROWS - 1) as u32];
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
    let mut meta = write_segment_with_graph(
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
            layout: GraphNodeLayout::new(DIMS as u32, DIMS as u32, BEST_FIRST_EF as u8)
                .expect("best-first graph layout"),
            nodes: &nodes,
        },
        policy(),
    )
    .expect("sealed best-first graph");
    meta.epoch_id = Some(epoch.identity().embedding);
    commit_with_epoch(&directory, &columns, meta, 1, Some(&epoch));
    (
        GraphFixture {
            directory,
            id,
            vectors,
        },
        epoch,
    )
}

fn commit(
    directory: &TempDir,
    columns: &zeppelin_embed::meta::ColumnStore,
    meta: SegmentMeta,
    generation: u64,
) {
    commit_with_epoch(directory, columns, meta, generation, None);
}

fn commit_with_epoch(
    directory: &TempDir,
    columns: &zeppelin_embed::meta::ColumnStore,
    meta: SegmentMeta,
    generation: u64,
    epoch: Option<&StoreEpoch>,
) {
    commit_manifest(
        &StdVfs,
        directory.path(),
        &Manifest {
            generation,
            log_seq: 0,
            segments: vec![meta],
            epochs: epoch
                .cloned()
                .map(zeppelin_embed::manifest::EpochMeta::from)
                .into_iter()
                .collect(),
            epoch_alias: epoch.map(StoreEpoch::identity),
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

fn adaptive_sift_graph_options() -> SearchOptions {
    SearchOptions::new(ScanOptions { thread_budget: 1 }).with_tier(SearchTier::Graph(
        GraphSearchOptions::new(GraphSearchProfile::SiftClass).with_seed(0x00c0_ffee),
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
fn graph_segment_accounting_moves_only_when_traversal_serves_the_plan() {
    let fixture = publish_graph_fixture(AliveSet::new(ROWS as u32));
    let store = Store::open(fixture.directory.path(), OpenOptions::default())
        .expect("open graph accounting fixture");
    let before = store.stats().expect("stats before graph query");
    assert_eq!(
        store.health().expect("graph fixture health").graph_coverage,
        1.0
    );
    store
        .search(
            SearchRequest::new(&query(3.5)),
            3,
            adaptive_sift_graph_options(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("query graph accounting fixture");
    let after = store.stats().expect("stats after graph query");

    assert_eq!(
        after.graph_segments_served,
        before.graph_segments_served + 1
    );
    assert_eq!(after.scans_by_reason, before.scans_by_reason);
    store.close().expect("close graph accounting fixture");
}

#[test]
fn graph_diagnostics_make_membership_and_score_provenance_independent() {
    let fixture = publish_graph_fixture(AliveSet::new(ROWS as u32));
    let store = Store::open(fixture.directory.path(), OpenOptions::default()).expect("open store");
    let query = query(5.25);
    let outcome = store
        .search(
            SearchRequest::new(&query),
            4,
            graph_options(ROWS),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("graph-tier store search");

    assert!(outcome.diagnostics.approximate);
    assert!(outcome.diagnostics.exact_rescore);
    assert_eq!(outcome.diagnostics.returned, outcome.candidates.len());
    assert_eq!(outcome.diagnostics.counters.graph, outcome.graph_stats);
    assert!(
        outcome
            .diagnostics
            .plan
            .iter()
            .any(|plan| plan.branch == SegmentBranch::Graph)
    );
    store.close().expect("close store");
}

#[test]
fn automatic_store_search_uses_best_first_frontier_before_the_ef_cutoff() {
    let (fixture, epoch) = publish_best_first_auto_graph();
    let store = Store::open(
        fixture.directory.path(),
        OpenOptions::default().with_epoch(epoch),
    )
    .expect("open store");
    let query = vec![0.0_f32; DIMS];

    let outcome = store
        .search(
            SearchRequest::new(&query),
            1,
            SearchOptions::new(ScanOptions { thread_budget: 1 }),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("automatic graph search");

    assert_eq!(outcome.candidates.len(), 1);
    assert_eq!(
        outcome.candidates[0].row_id().source(),
        RowSource::Sealed(fixture.id)
    );
    assert_eq!(
        outcome.candidates[0].row_id().local_row(),
        (BEST_FIRST_ROWS - 1) as u32
    );
    assert_eq!(outcome.graph_stats.segments_traversed, 1);
    assert_eq!(outcome.graph_stats.candidates_rescored, BEST_FIRST_EF);
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
    let before_query_views = store
        .snapshot()
        .expect("snapshot before graph query")
        .segments()
        .iter()
        .map(SegmentReader::retained_query_view_bytes)
        .sum::<u64>();
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
    let after_query_views = store
        .snapshot()
        .expect("snapshot after graph query")
        .segments()
        .iter()
        .map(SegmentReader::retained_query_view_bytes)
        .sum::<u64>();
    let query_view_delta = after_query_views - before_query_views;
    let expected = GraphSearchScratch::allocation_bytes(ROWS as u32, (ROWS - 1) as u8, ROWS)
        .expect("scratch byte geometry") as u64;
    assert_eq!(after.cache_bytes - before.cache_bytes, expected);
    assert_eq!(
        after.snapshot_bytes - before.snapshot_bytes,
        query_view_delta
    );
    assert_eq!(
        after.resident_owned_bytes - before.resident_owned_bytes,
        expected + query_view_delta
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

#[test]
fn explicit_ef_equal_to_k_still_answers_when_rows_are_deleted() {
    let mut alive = AliveSet::new(ROWS as u32);
    alive.tombstone(5).expect("fixture tombstone");
    let fixture = publish_graph_fixture(alive);
    let store = Store::open(fixture.directory.path(), OpenOptions::default()).expect("open store");
    let query = query(5.0);

    let outcome = store
        .search(
            SearchRequest::new(&query),
            4,
            graph_options(4),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("explicit ef=k must reserve deleted-row headroom");

    assert_eq!(outcome.candidates.len(), 4);
    assert!(
        outcome
            .candidates
            .iter()
            .all(|candidate| candidate.row_id().local_row() != 5)
    );
    store.close().expect("close store");
}

#[test]
fn astra_01_approximate_stop_does_not_force_full_materialization() {
    use zeppelin_embed::fts::index::DEFAULT_FIELD;
    use zeppelin_embed::fts::search::TermQuery;
    use zeppelin_embed::fusion::{FusionTermination, HybridQuery};
    use zeppelin_embed::tier::{MaintenanceBudget, MaintenanceStatus, TierThresholds};

    let directory = tempdir().expect("coverage graph directory");
    let epoch = graph_epoch(Normalization::None);
    let store = Store::open(
        directory.path(),
        OpenOptions::default().with_epoch(epoch.clone()),
    )
    .expect("coverage graph store");
    let rows = 1_024;
    let vectors = fixture_vectors(rows);
    let documents = vectors
        .chunks_exact(DIMS)
        .enumerate()
        .map(|(row, vector)| {
            IngestDocument::new(
                DocumentVersion::new(DocId::new(row as u128 + 1), Revision::new(1)),
                vector.to_vec(),
            )
            .with_text("zeppelin")
        })
        .collect();
    store
        .ingest(IngestBatch::new(documents).with_epoch(epoch.identity()))
        .expect("ingest graph identities");
    store.seal().expect("seal graph input");
    let maintenance = store.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: std::time::Duration::from_secs(600),
            bytes: u64::MAX,
        },
        TierThresholds { graph_min_rows: 32 },
    );
    assert!(matches!(maintenance.status, MaintenanceStatus::Complete));
    assert_eq!(maintenance.graphs_built, 1);
    let outcome = store
        .search_hybrid(
            SearchRequest::new(&vec![0.0; DIMS]),
            &TermQuery::flat(vec![b"absent".to_vec()], &[DEFAULT_FIELD]),
            &HybridQuery::new(1),
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("default ANN hybrid query");
    let diagnostics = &outcome.diagnostics;
    let report = diagnostics.fusion.as_ref().expect("fusion report");
    assert!(
        diagnostics.exact_rescore,
        "retained ANN rows have exact distances"
    );
    assert!(
        diagnostics.approximate,
        "candidate membership came from ANN"
    );
    assert_eq!(diagnostics.counters.graph.segments_traversed, 1);
    assert_eq!(diagnostics.plan.len(), 1);
    assert_eq!(diagnostics.plan[0].branch, SegmentBranch::Graph);
    assert_eq!(report.rounds, 1);
    assert!(!diagnostics.budget_exhausted);
    let provenance = diagnostics
        .hybrid
        .as_ref()
        .expect("hybrid provenance")
        .provenance;
    assert_eq!(
        provenance.vector_precision,
        zeppelin_embed::fusion::ScorePrecision::Exact
    );
    assert_eq!(
        provenance.vector_coverage,
        zeppelin_embed::fusion::CandidateCoverage::Approximate
    );
    assert!(diagnostics.counters.graph.candidates_rescored < rows);
    assert!(
        !matches!(
            report.termination,
            FusionTermination::StableBound
                | FusionTermination::ListsExhausted
                | FusionTermination::BudgetFullMaterialization
        ),
        "exact ANN scores cannot certify unseen membership: {:?}",
        report.termination
    );
    // Primitive independent coverage oracle and its false-certificate plant.
    let accepts = |approximate_membership: bool, claims_certificate: bool| {
        !approximate_membership || !claims_certificate
    };
    let claims_certificate = matches!(
        report.termination,
        FusionTermination::StableBound
            | FusionTermination::ListsExhausted
            | FusionTermination::BudgetFullMaterialization
    );
    assert!(
        accepts(true, claims_certificate),
        "actual graph clean control"
    );
    assert!(
        !accepts(true, true),
        "a planted ANN certificate must be rejected"
    );
    store.close().expect("close coverage graph store");
}
