#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::collections::BTreeMap;

use tempfile::{TempDir, tempdir};
use zeppelin_embed::graph::block::{GraphNodeBlockBuild, GraphNodeBlockInput, GraphNodeLayout};
use zeppelin_embed::graph::search::GraphSearchProfile;
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::{
    CancelToken, GraphSearchOptions, OpenOptions, QueryControl, SearchOptions, SearchTier, Store,
};
use zeppelin_embed::manifest::Manifest;
use zeppelin_embed::manifest::io::{MANIFEST_FILE, commit_manifest, load_manifest};
use zeppelin_embed::meta::{
    AliveSet, ColumnDefinition, ColumnId, ColumnInput, ColumnStore, ColumnStoreBuilder, ColumnType,
    ColumnValue, Predicate, PredicateValue, RangeBound, RangePredicate, Schema,
};
use zeppelin_embed::planner::{SegmentBranch, SegmentTier};
use zeppelin_embed::quant::{Bit4Factors, quantize_bit4, quantize_int8};
use zeppelin_embed::segment::layout::Int8Factors;
use zeppelin_embed::segment::writer::{SegmentBuild, SegmentFactors, write_segment};
use zeppelin_embed::segment::writer::{
    SegmentDocumentVersions, write_segment_with_graph, write_segment_with_graph_and_documents,
};
use zeppelin_embed::segment::{SegmentId, SegmentMeta};
use zeppelin_embed::vfs::StdVfs;

const ROWS: usize = 1_000;
const DIMS: usize = 2;
const QUERY: [f32; DIMS] = [0.25, -0.75];
const FILTER_COLUMN: ColumnId = ColumnId::new(1);
const TARGETS: [usize; 6] = [0, 1, 10, 100, 500, 1_000];

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum PredicateOp {
    Eq,
    In,
    RangeTwoSided,
    RangeHalfOpen,
    Exists,
    IsNull,
    And,
    Or,
    Not,
}

impl PredicateOp {
    const ALL: [Self; 9] = [
        Self::Eq,
        Self::In,
        Self::RangeTwoSided,
        Self::RangeHalfOpen,
        Self::Exists,
        Self::IsNull,
        Self::And,
        Self::Or,
        Self::Not,
    ];

    fn predicate(self, column: ColumnId) -> Predicate {
        let eq = |value| Predicate::Eq {
            column,
            value: PredicateValue::U64(value),
        };
        match self {
            Self::Eq => eq(1),
            Self::In => Predicate::In {
                column,
                values: vec![PredicateValue::U64(1), PredicateValue::U64(2)],
            },
            Self::RangeTwoSided => Predicate::Range(RangePredicate {
                column,
                lower: Some(RangeBound::inclusive(PredicateValue::U64(1))),
                upper: Some(RangeBound::inclusive(PredicateValue::U64(1))),
            }),
            Self::RangeHalfOpen => Predicate::Range(RangePredicate {
                column,
                lower: Some(RangeBound::inclusive(PredicateValue::U64(1))),
                upper: None,
            }),
            Self::Exists => Predicate::Exists(column),
            Self::IsNull => Predicate::IsNull(column),
            Self::And => Predicate::And(vec![eq(1), Predicate::Exists(column)]),
            Self::Or => Predicate::Or(vec![eq(1), eq(2)]),
            Self::Not => Predicate::Not(Box::new(eq(0))),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum QuantScheme {
    Bit4,
    Int8,
    F32,
}

impl QuantScheme {
    const ALL: [Self; 3] = [Self::Bit4, Self::Int8, Self::F32];
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Tier {
    ActiveScan,
    SealedScan,
    SealedGraph,
}

impl Tier {
    const ALL: [Self; 3] = [Self::ActiveScan, Self::SealedScan, Self::SealedGraph];
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum StoreState {
    WriterSeamFresh,
    WriterSeamReopened,
    PublicIngestActive,
    PublicIngestReopened,
}

impl StoreState {
    const ALL: [Self; 4] = [
        Self::WriterSeamFresh,
        Self::WriterSeamReopened,
        Self::PublicIngestActive,
        Self::PublicIngestReopened,
    ];
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum SkipReason {
    TierStateNotRepresentable,
    PublicIngestHasNoQuantSelection,
}

fn skip_reason(
    _op: PredicateOp,
    tier: Tier,
    scheme: QuantScheme,
    state: StoreState,
) -> Option<SkipReason> {
    let represented = matches!(
        (tier, state),
        (Tier::SealedScan, StoreState::WriterSeamFresh)
            | (Tier::SealedScan, StoreState::WriterSeamReopened)
            | (Tier::SealedGraph, StoreState::WriterSeamFresh)
            | (Tier::SealedGraph, StoreState::WriterSeamReopened)
            | (Tier::ActiveScan, StoreState::PublicIngestActive)
            | (Tier::SealedScan, StoreState::PublicIngestReopened)
            | (Tier::SealedGraph, StoreState::PublicIngestReopened)
    );
    if !represented {
        return Some(SkipReason::TierStateNotRepresentable);
    }
    if matches!(
        state,
        StoreState::PublicIngestActive | StoreState::PublicIngestReopened
    ) && scheme != QuantScheme::Bit4
    {
        return Some(SkipReason::PublicIngestHasNoQuantSelection);
    }
    None
}

#[test]
fn every_registered_filter_cell_is_exact_or_counted_as_skipped() {
    let mut registered = 0_usize;
    let mut expected_run = 0_usize;
    let mut skipped = BTreeMap::<SkipReason, usize>::new();
    let mut expected_graph_run = 0_usize;
    let mut graph_skipped = BTreeMap::<SkipReason, usize>::new();
    for op in PredicateOp::ALL {
        for tier in Tier::ALL {
            for scheme in QuantScheme::ALL {
                for _target in TARGETS {
                    for state in StoreState::ALL {
                        registered += 1;
                        if let Some(reason) = skip_reason(op, tier, scheme, state) {
                            *skipped.entry(reason).or_default() += 1;
                            if tier == Tier::SealedGraph {
                                *graph_skipped.entry(reason).or_default() += 1;
                            }
                        } else {
                            expected_run += 1;
                            if tier == Tier::SealedGraph {
                                expected_graph_run += 1;
                            }
                        }
                    }
                }
            }
        }
    }

    let mut ran = 0_usize;
    let mut graph_ran = 0_usize;
    for op in PredicateOp::ALL {
        for target in TARGETS {
            let columns = writer_columns(op, target);
            let vectors = fixture_vectors();
            for scheme in QuantScheme::ALL {
                let fixture = publish_writer_fixture(&columns, &vectors, scheme);
                let store =
                    Store::open(fixture.path(), OpenOptions::default()).expect("open fresh");
                assert_writer_cell(&store, op, target);
                ran += 1;
                store.close().expect("close fresh");
                let reopened =
                    Store::open(fixture.path(), OpenOptions::default()).expect("open reopened");
                assert_writer_cell(&reopened, op, target);
                ran += 1;
                reopened.close().expect("close reopened");

                let graph_fixture = publish_writer_graph_fixture(&columns, &vectors, scheme);
                let graph_store = Store::open(graph_fixture.path(), OpenOptions::default())
                    .expect("open fresh graph");
                assert_writer_graph_cell(&graph_store, op, target);
                ran += 1;
                graph_ran += 1;
                graph_store.close().expect("close fresh graph");
                let reopened_graph = Store::open(graph_fixture.path(), OpenOptions::default())
                    .expect("open reopened graph");
                assert_writer_graph_cell(&reopened_graph, op, target);
                ran += 1;
                graph_ran += 1;
                reopened_graph.close().expect("close reopened graph");
            }
        }
    }

    for op in PredicateOp::ALL {
        for target in TARGETS {
            let fixture = tempdir().expect("public-ingest fixture");
            let store = Store::open(
                fixture.path(),
                OpenOptions::default().with_schema(filter_schema()),
            )
            .expect("open ingest");
            ingest_public_rows(&store, op, target);
            assert_public_cell(&store, op, target, SegmentTier::ActiveScan);
            ran += 1;
            store.seal().expect("seal public-ingest rows");
            store.close().expect("close ingested store");
            let reopened =
                Store::open(fixture.path(), OpenOptions::default()).expect("reopen ingest");
            assert_public_cell(&reopened, op, target, SegmentTier::SealedScan);
            ran += 1;
            reopened.close().expect("close public reopened");

            promote_public_fixture_to_graph(&fixture);
            let graph_reopened =
                Store::open(fixture.path(), OpenOptions::default()).expect("reopen public graph");
            assert_public_cell(&graph_reopened, op, target, SegmentTier::SealedGraph);
            ran += 1;
            graph_ran += 1;
            graph_reopened.close().expect("close public graph");
        }
    }

    let skipped_count = skipped.values().sum::<usize>();
    println!(
        "FILTER_MATRIX registered={registered} ran={ran} skipped={skipped_count} reasons={skipped:?}"
    );
    let graph_skipped_count = graph_skipped.values().sum::<usize>();
    println!(
        "FILTERED_GRAPH_M6 previous_skipped=648 ran={graph_ran} still_skipped={graph_skipped_count} reasons={graph_skipped:?}"
    );
    assert_eq!(registered, 1_944);
    assert_eq!(expected_run, 810);
    assert_eq!(graph_ran, expected_graph_run);
    assert_eq!(graph_ran, 378);
    assert_eq!(graph_skipped_count, 270);
    assert_eq!(ran, expected_run);
    assert_eq!(registered, ran + skipped_count);
}

fn writer_columns(op: PredicateOp, target: usize) -> ColumnStore {
    let mut builder = ColumnStoreBuilder::new(filter_schema());
    for row in 0..ROWS {
        let inputs = public_column_values(op, row, target)
            .into_iter()
            .map(|(column, value)| ColumnInput {
                column,
                value: match value {
                    PredicateValue::U64(value) => ColumnValue::U64(value),
                    _ => unreachable!("matrix fixture uses only u64 filter values"),
                },
            })
            .collect::<Vec<_>>();
        builder
            .push_row(row as i64, &inputs)
            .expect("writer metadata row");
    }
    builder.finish().expect("writer columns")
}

fn filter_schema() -> Schema {
    Schema::new(vec![ColumnDefinition::new(
        FILTER_COLUMN,
        "filter",
        ColumnType::U64,
        true,
    )])
    .expect("filter schema")
}

fn fixture_vectors() -> Vec<f32> {
    (0..ROWS)
        .flat_map(|row| [row as f32 / ROWS as f32, 1.0])
        .collect()
}

fn row_is_selected(row: usize, target: usize) -> bool {
    row >= ROWS.saturating_sub(target)
}

fn publish_writer_fixture(columns: &ColumnStore, vectors: &[f32], scheme: QuantScheme) -> TempDir {
    publish_writer_fixture_at_tier(columns, vectors, scheme, false)
}

fn publish_writer_graph_fixture(
    columns: &ColumnStore,
    vectors: &[f32],
    scheme: QuantScheme,
) -> TempDir {
    publish_writer_fixture_at_tier(columns, vectors, scheme, true)
}

fn publish_writer_fixture_at_tier(
    columns: &ColumnStore,
    vectors: &[f32],
    scheme: QuantScheme,
    with_graph: bool,
) -> TempDir {
    let directory = tempdir().expect("writer-seam directory");
    let id = SegmentId::new(0x0001_6000_0000 + scheme as u64, [scheme as u8; 10]);
    let alive = AliveSet::new(ROWS as u32);
    let meta = match scheme {
        QuantScheme::Bit4 => {
            let mut codes = vec![0_u8; ROWS * DIMS.div_ceil(2)];
            let mut factors = Vec::<Bit4Factors>::with_capacity(ROWS);
            for (vector, encoded) in vectors
                .chunks_exact(DIMS)
                .zip(codes.chunks_exact_mut(DIMS.div_ceil(2)))
            {
                factors.push(quantize_bit4(vector, encoded).expect("Bit4 fixture row"));
            }
            write_fixture_segment(
                &directory,
                id,
                4,
                &codes,
                SegmentFactors::Bit4(&factors),
                vectors,
                columns,
                &alive,
                with_graph,
            )
        }
        QuantScheme::Int8 => {
            let mut signed = vec![0_i8; ROWS * DIMS];
            let mut factors = Vec::<Int8Factors>::with_capacity(ROWS);
            for (vector, encoded) in vectors
                .chunks_exact(DIMS)
                .zip(signed.chunks_exact_mut(DIMS))
            {
                let (scale, offset) = quantize_int8(vector, encoded).expect("Int8 fixture row");
                factors.push(Int8Factors { scale, offset });
            }
            let codes = signed
                .into_iter()
                .map(|value| value as u8)
                .collect::<Vec<_>>();
            write_fixture_segment(
                &directory,
                id,
                2,
                &codes,
                SegmentFactors::Int8(&factors),
                vectors,
                columns,
                &alive,
                with_graph,
            )
        }
        QuantScheme::F32 => {
            let codes = vectors
                .iter()
                .flat_map(|value| value.to_bits().to_le_bytes())
                .collect::<Vec<_>>();
            write_fixture_segment(
                &directory,
                id,
                0,
                &codes,
                SegmentFactors::F32,
                vectors,
                columns,
                &alive,
                with_graph,
            )
        }
    };
    commit(&directory, columns, meta);
    directory
}

#[allow(clippy::too_many_arguments)]
fn write_fixture_segment(
    directory: &TempDir,
    id: SegmentId,
    scheme: u16,
    codes: &[u8],
    factors: SegmentFactors<'_>,
    vectors: &[f32],
    columns: &ColumnStore,
    alive: &AliveSet,
    with_graph: bool,
) -> SegmentMeta {
    let build = SegmentBuild {
        id,
        scheme,
        dims: DIMS as u32,
        codes,
        factors,
        rescore: vectors,
        columns,
        alive,
    };
    if !with_graph {
        return write_segment(&StdVfs, directory.path(), build, policy())
            .expect("write matrix segment");
    }
    let (graph_codes, graph_factors, graph_neighbors) = graph_rows(vectors);
    let nodes = graph_codes
        .chunks_exact(128_usize.div_ceil(2))
        .zip(&graph_factors)
        .zip(&graph_neighbors)
        .enumerate()
        .map(|(row, ((codes, factors), neighbors))| GraphNodeBlockInput {
            codes,
            factors: *factors,
            flags: u8::from(row < 4),
            neighbors,
        })
        .collect::<Vec<_>>();
    write_segment_with_graph(
        &StdVfs,
        directory.path(),
        build,
        GraphNodeBlockBuild {
            layout: GraphNodeLayout::new(DIMS as u32, 128, 1).expect("matrix graph layout"),
            nodes: &nodes,
        },
        policy(),
    )
    .expect("write matrix graph segment")
}

fn graph_rows(vectors: &[f32]) -> (Vec<u8>, Vec<Bit4Factors>, Vec<Vec<u32>>) {
    let mut codes = vec![0_u8; ROWS * 128_usize.div_ceil(2)];
    let mut factors = Vec::with_capacity(ROWS);
    for (row, encoded) in vectors
        .chunks_exact(DIMS)
        .zip(codes.chunks_exact_mut(128_usize.div_ceil(2)))
    {
        factors
            .push(quantize_bit4(row, &mut encoded[..DIMS.div_ceil(2)]).expect("matrix graph row"));
    }
    let neighbors = (0..ROWS)
        .map(|row| {
            u32::try_from(row + 1)
                .ok()
                .filter(|next| *next < ROWS as u32)
                .into_iter()
                .collect::<Vec<_>>()
        })
        .collect();
    (codes, factors, neighbors)
}

fn commit(directory: &TempDir, columns: &ColumnStore, segment: SegmentMeta) {
    commit_manifest(
        &StdVfs,
        directory.path(),
        &Manifest {
            generation: 1,
            log_seq: 0,
            segments: vec![segment],
            epochs: Vec::new(),
            epoch_alias: None,
            schema: columns.schema().clone(),
        },
        policy(),
    )
    .expect("commit matrix manifest");
}

fn promote_public_fixture_to_graph(directory: &TempDir) {
    let reader = Store::open(directory.path(), OpenOptions::read_only())
        .expect("open public segment for graph promotion");
    let lease = reader.snapshot().expect("public promotion snapshot");
    let segment = lease.segments().first().expect("sealed public segment");
    let old_id = segment.meta().id;
    let columns = segment.columns().expect("public graph columns");
    let alive = segment.alive().expect("public graph alive set");
    let vectors = segment.rescore_f32().expect("public graph rescore rows");
    let (graph_codes, graph_factors, graph_neighbors) = graph_rows(vectors);
    let nodes = graph_codes
        .chunks_exact(128_usize.div_ceil(2))
        .zip(&graph_factors)
        .zip(&graph_neighbors)
        .enumerate()
        .map(|(row, ((codes, factors), neighbors))| GraphNodeBlockInput {
            codes,
            factors: *factors,
            flags: u8::from(row < 4),
            neighbors,
        })
        .collect::<Vec<_>>();
    let documents = (0..ROWS)
        .map(|row| {
            segment
                .document_version(row)
                .expect("public graph document read")
                .expect("public row has document identity")
        })
        .collect::<Vec<_>>();
    let doc_ids = documents
        .iter()
        .map(|document| document.doc_id())
        .collect::<Vec<_>>();
    let revisions = documents
        .iter()
        .map(|document| document.revision())
        .collect::<Vec<_>>();
    let graph_id = SegmentId::new(0x0001_6000_00f0, [0xf0; 10]);
    let meta = write_segment_with_graph_and_documents(
        &StdVfs,
        directory.path(),
        SegmentBuild {
            id: graph_id,
            scheme: segment.meta().scheme,
            dims: segment.meta().dims,
            codes: segment.bit4_codes().expect("public graph Bit4 rows"),
            factors: SegmentFactors::Bit4(
                segment.bit4_factors().expect("public graph Bit4 factors"),
            ),
            rescore: vectors,
            columns: &columns,
            alive: &alive,
        },
        GraphNodeBlockBuild {
            layout: GraphNodeLayout::new(DIMS as u32, 128, 1).expect("public graph layout"),
            nodes: &nodes,
        },
        SegmentDocumentVersions {
            doc_ids: &doc_ids,
            revisions: &revisions,
        },
        policy(),
    )
    .expect("write public graph segment");
    drop(lease);
    reader.close().expect("close public promotion reader");

    let mut manifest = load_manifest(&StdVfs, &directory.path().join(MANIFEST_FILE), u64::MAX)
        .expect("load public promotion manifest");
    let slot = manifest
        .segments
        .iter_mut()
        .find(|candidate| candidate.id == old_id)
        .expect("public source remains manifested");
    let mut meta = meta;
    meta.clustering_key_range = slot.clustering_key_range;
    *slot = meta;
    manifest.generation = manifest
        .generation
        .checked_add(1)
        .expect("promotion generation");
    commit_manifest(&StdVfs, directory.path(), &manifest, policy())
        .expect("commit public graph manifest");
}

fn policy() -> DurabilityPolicy {
    DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None).expect("matrix policy")
}

fn assert_writer_cell(store: &Store, op: PredicateOp, target: usize) {
    let options = SearchOptions::default();
    let outcome = store
        .search_filtered(
            SearchRequest::new(&QUERY),
            &op.predicate(FILTER_COLUMN),
            ROWS,
            options,
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("writer filtered search");
    assert_exact_rows(store, &outcome.candidates, target, options);
    assert_eq!(outcome.plans.len(), 1);
    assert_eq!(outcome.plans[0].tier, SegmentTier::SealedScan);
    assert_eq!(outcome.plans[0].filter_cardinality, target as u64);
    assert_eq!(
        outcome.plans[0].branch,
        if target <= 64 {
            SegmentBranch::ExactAllowList
        } else {
            SegmentBranch::MaskedScan
        }
    );
}

fn assert_writer_graph_cell(store: &Store, op: PredicateOp, target: usize) {
    let options = explicit_sift_graph_options();
    let outcome = store
        .search_filtered(
            SearchRequest::new(&QUERY),
            &op.predicate(FILTER_COLUMN),
            ROWS,
            options,
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("writer filtered graph search");
    assert_exact_rows(store, &outcome.candidates, target, options);
    assert_eq!(outcome.plans.len(), 1);
    assert_eq!(outcome.plans[0].tier, SegmentTier::SealedGraph);
    assert_eq!(outcome.plans[0].filter_cardinality, target as u64);
    if target <= 64 {
        assert_eq!(outcome.plans[0].branch, SegmentBranch::ExactAllowList);
        assert!(!outcome.plans[0].approximate);
    } else {
        assert!(matches!(
            outcome.plans[0].branch,
            SegmentBranch::FilteredGraph | SegmentBranch::GraphExactFallback
        ));
    }
}

fn ingest_public_rows(store: &Store, op: PredicateOp, target: usize) {
    let documents = (0..ROWS)
        .map(|row| {
            IngestDocument::new(
                DocumentVersion::new(DocId::new((row + 1) as u128), Revision::new(1)),
                vec![row as f32 / ROWS as f32, 1.0],
            )
            .with_timestamp(i64::from(row_is_selected(row, target)))
            .with_columns(public_column_values(op, row, target))
        })
        .collect::<Vec<_>>();
    store
        .ingest(IngestBatch::new(documents))
        .expect("public ingest rows");
}

fn assert_public_cell(store: &Store, op: PredicateOp, target: usize, tier: SegmentTier) {
    let options = if tier == SegmentTier::SealedGraph {
        explicit_sift_graph_options()
    } else {
        SearchOptions::default()
    };
    let outcome = store
        .search_filtered(
            SearchRequest::new(&QUERY),
            &op.predicate(FILTER_COLUMN),
            ROWS,
            options,
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("public-ingest filtered search");
    assert_exact_rows(store, &outcome.candidates, target, options);
    assert_eq!(outcome.plans.len(), 1);
    assert_eq!(outcome.plans[0].tier, tier);
    assert_eq!(outcome.plans[0].filter_cardinality, target as u64);
}

fn public_column_values(
    op: PredicateOp,
    row: usize,
    target: usize,
) -> Vec<(ColumnId, PredicateValue)> {
    let value = match op {
        PredicateOp::Exists => row_is_selected(row, target).then_some(1_u64),
        PredicateOp::IsNull => (!row_is_selected(row, target)).then_some(0_u64),
        PredicateOp::Eq
        | PredicateOp::In
        | PredicateOp::RangeTwoSided
        | PredicateOp::RangeHalfOpen
        | PredicateOp::And
        | PredicateOp::Or
        | PredicateOp::Not => Some(u64::from(row_is_selected(row, target))),
    };
    value
        .map(|value| (FILTER_COLUMN, PredicateValue::U64(value)))
        .into_iter()
        .collect()
}

fn assert_exact_rows(
    store: &Store,
    candidates: &[zeppelin_embed::ingest::SearchCandidate],
    target: usize,
    options: SearchOptions,
) {
    let expected = store
        .search(
            SearchRequest::new(&QUERY),
            ROWS,
            options,
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("brute-force unfiltered scan")
        .candidates
        .into_iter()
        .filter(|candidate| row_is_selected(candidate.row_id().local_row() as usize, target))
        .collect::<Vec<_>>();
    assert_eq!(candidates.len(), target);
    assert_eq!(candidates.len(), expected.len());
    for (actual, expected) in candidates.iter().zip(expected) {
        assert_eq!(actual.row_id(), expected.row_id());
        assert_eq!(actual.document(), expected.document());
        assert_eq!(actual.score().to_bits(), expected.score().to_bits());
    }
}

fn explicit_sift_graph_options() -> SearchOptions {
    SearchOptions::default().with_tier(SearchTier::Graph(GraphSearchOptions::new(
        GraphSearchProfile::SiftClass,
    )))
}
