#![allow(clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use tempfile::tempdir;
use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, Normalization, StoreEpoch,
};
use zeppelin_embed::fts::tokenizer::TokenizerConfig;
use zeppelin_embed::graph::search::GraphSearchProfile;
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, RowSource,
    SearchRequest,
};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::{
    CancelToken, GraphSearchOptions, OpenOptions, QueryControl, SearchOptions, SearchTier, Store,
    StoreTestDependencies, SystemMonotonicClock,
};
use zeppelin_embed::meta::{
    AliveSet, Column, ColumnDefinition, ColumnId, ColumnInput, ColumnStoreBuilder, ColumnType,
    ColumnValue, Predicate, PredicateValue, RangeBound, RangePredicate, Schema, evaluate,
};
use zeppelin_embed::planner::{
    ALLOW_LIST_ROWS_THRESHOLD, MetadataExecutionReceipt, MetadataFeatureDetail, MetadataTestArm,
    MetadataTestController, PlanFallback, SegmentBranch, SegmentPlan,
};
use zeppelin_embed::quant::Bit4Factors;
use zeppelin_embed::scan::ScanOptions;
use zeppelin_embed::segment::SegmentId;
use zeppelin_embed::segment::reader::SegmentReader;
use zeppelin_embed::segment::writer::{SegmentBuild, SegmentFactors, write_segment};
use zeppelin_embed::tier::{MaintenanceBudget, MaintenanceStatus, TierThresholds};
use zeppelin_embed::vfs::StdVfs;

#[allow(dead_code)]
#[path = "../../../tests/adversarial-oracle/src/metadata_filter_planner.rs"]
mod oracle;

fn ordered_policy() -> DurabilityPolicy {
    DurabilityPolicy::new(DurabilityMode::Durable, CommitTier::Ordered)
        .expect("ordered durability policy")
}

fn source_label(source: RowSource) -> String {
    match source {
        RowSource::Active => "active".to_owned(),
        RowSource::Sealed(id) => format!("sealed-{}", id.file_name()),
    }
}

fn branch_dto(branch: SegmentBranch) -> oracle::ExecutionBranchDto {
    match branch {
        SegmentBranch::Pruned => oracle::ExecutionBranchDto::Pruned,
        SegmentBranch::ExactAllowList => oracle::ExecutionBranchDto::ExactAllowList,
        SegmentBranch::MaskedScan => oracle::ExecutionBranchDto::MaskedScan,
        SegmentBranch::FilteredGraph => oracle::ExecutionBranchDto::FilteredGraph,
        SegmentBranch::GraphExactFallback => oracle::ExecutionBranchDto::GraphExactFallback,
        SegmentBranch::Graph => panic!("unfiltered graph branch cannot satisfy I39"),
    }
}

fn fallback_dto(fallback: PlanFallback) -> oracle::FallbackReasonDto {
    match fallback {
        PlanFallback::None => oracle::FallbackReasonDto::None,
        PlanFallback::VisitedBudget => oracle::FallbackReasonDto::VisitedBudget,
        PlanFallback::CandidateShortfall => oracle::FallbackReasonDto::CandidateShortfall,
        PlanFallback::EfWidened => oracle::FallbackReasonDto::EfWidened,
    }
}

fn branch_report(query_id: u64, plan: &SegmentPlan) -> oracle::BranchReportDto {
    oracle::BranchReportDto {
        key: oracle::QuerySourceKey {
            query_id,
            source: source_label(plan.source),
        },
        branch: branch_dto(plan.branch),
        fallback: fallback_dto(plan.fallback),
        filter_cardinality: plan.filter_cardinality,
    }
}

fn execution_receipt(receipt: &MetadataExecutionReceipt) -> oracle::ExecutionReceiptDto {
    oracle::ExecutionReceiptDto {
        key: oracle::QuerySourceKey {
            query_id: receipt.query_id,
            source: source_label(receipt.source),
        },
        branch: branch_dto(receipt.branch),
        fallback: fallback_dto(receipt.fallback),
        row_count: receipt.row_count,
        filter_cardinality: receipt.filter_cardinality,
        rows_examined: receipt.rows_examined,
        allowed_rows_examined: receipt.allowed_rows_examined,
        vectors_scored: receipt.vectors_scored,
        graph_nodes_visited: receipt.graph_nodes_visited,
        exact_fallback_rows_examined: receipt.exact_fallback_rows_examined,
        returned_candidates: receipt.returned_candidates,
        ef_effective: receipt
            .ef_effective
            .and_then(|value| u64::try_from(value).ok()),
        visited_budget: receipt
            .visited_budget
            .and_then(|value| u64::try_from(value).ok()),
        sealed: receipt.sealed,
    }
}

const GRAPH_ROWS: usize = 80;
const GRAPH_DIMS: usize = 128;

fn metadata_graph_schema() -> Schema {
    Schema::new(vec![
        ColumnDefinition::new(ColumnId::new(1), "u", ColumnType::U64, true),
        ColumnDefinition::new(ColumnId::new(2), "i", ColumnType::I64, true),
        ColumnDefinition::new(ColumnId::new(3), "f", ColumnType::F64, true),
        ColumnDefinition::new(ColumnId::new(4), "b", ColumnType::Bool, true),
        ColumnDefinition::new(ColumnId::new(5), "d", ColumnType::DictionaryString, true),
        ColumnDefinition::new(ColumnId::new(6), "r", ColumnType::RawString, true),
    ])
    .expect("valid graph-backed metadata schema")
}

fn metadata_graph_f64(row: usize) -> f64 {
    match row {
        0 => -0.0,
        1 => 0.0,
        2 => f64::from_bits(0x7ff8_0000_0000_00a5),
        _ => f64::from(u32::try_from(row % 8).expect("row remainder fits u32")) - 1.0,
    }
}

fn metadata_graph_columns(row: usize) -> Vec<(ColumnId, PredicateValue)> {
    let mut values = vec![
        (
            ColumnId::new(1),
            PredicateValue::U64(u64::try_from(row).expect("graph row fits u64")),
        ),
        (
            ColumnId::new(2),
            PredicateValue::I64(i64::try_from(row).expect("graph row fits i64") - 40),
        ),
        (
            ColumnId::new(3),
            PredicateValue::F64(metadata_graph_f64(row)),
        ),
        (
            ColumnId::new(4),
            PredicateValue::Bool(row.is_multiple_of(2)),
        ),
    ];
    if !row.is_multiple_of(5) {
        values.push((
            ColumnId::new(5),
            PredicateValue::String(if row.is_multiple_of(2) {
                "one".to_owned()
            } else {
                "two".to_owned()
            }),
        ));
    }
    if !row.is_multiple_of(4) {
        values.push((
            ColumnId::new(6),
            PredicateValue::String(if row.is_multiple_of(3) {
                "repeated-raw".to_owned()
            } else {
                "other".to_owned()
            }),
        ));
    }
    values
}

fn metadata_graph_oracle_rows() -> Vec<oracle::MetadataRowDto> {
    (0..GRAPH_ROWS)
        .map(|row| {
            let mut cells = BTreeMap::from([
                (
                    0,
                    oracle::ScalarCell::I64(i64::try_from(row).expect("graph timestamp fits i64")),
                ),
                (
                    1,
                    oracle::ScalarCell::U64(u64::try_from(row).expect("graph row fits u64")),
                ),
                (
                    2,
                    oracle::ScalarCell::I64(i64::try_from(row).expect("graph row fits i64") - 40),
                ),
                (
                    3,
                    oracle::ScalarCell::F64Bits(metadata_graph_f64(row).to_bits()),
                ),
                (4, oracle::ScalarCell::Bool(row.is_multiple_of(2))),
            ]);
            cells.insert(
                5,
                if row.is_multiple_of(5) {
                    oracle::ScalarCell::Null
                } else {
                    oracle::ScalarCell::Utf8(if row.is_multiple_of(2) {
                        b"one".to_vec()
                    } else {
                        b"two".to_vec()
                    })
                },
            );
            cells.insert(
                6,
                if row.is_multiple_of(4) {
                    oracle::ScalarCell::Null
                } else {
                    oracle::ScalarCell::Utf8(if row.is_multiple_of(3) {
                        b"repeated-raw".to_vec()
                    } else {
                        b"other".to_vec()
                    })
                },
            );
            oracle::MetadataRowDto {
                row_id: u32::try_from(row).expect("graph row fits u32"),
                cells,
            }
        })
        .collect()
}

fn metadata_graph_epoch() -> StoreEpoch {
    let document = EmbeddingTower {
        model_id: "metadata-public-sift".to_owned(),
        model_version: "1".to_owned(),
        weights_digest: vec![0x39],
        dims: GRAPH_DIMS as u32,
        normalization: Normalization::None,
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

fn metadata_graph_document(row: usize) -> DocId {
    DocId::new(row as u128 + 1)
}

fn publish_metadata_chain_graph(directory: &std::path::Path) -> SegmentId {
    let epoch = metadata_graph_epoch();
    let store = Store::open(
        directory,
        OpenOptions::default()
            .with_epoch(epoch.clone())
            .with_schema(metadata_graph_schema()),
    )
    .expect("open public metadata graph Store");
    let documents = (0..GRAPH_ROWS)
        .map(|row| {
            IngestDocument::new(
                DocumentVersion::new(metadata_graph_document(row), Revision::new(1)),
                vec![0.0_f32; GRAPH_DIMS],
            )
            .with_timestamp(i64::try_from(row).expect("metadata graph timestamp fits i64"))
            .with_columns(metadata_graph_columns(row))
        })
        .collect();
    store
        .ingest(IngestBatch::new(documents).with_epoch(epoch.identity()))
        .expect("ingest public metadata graph Store");
    store.seal().expect("seal public metadata graph Store");
    let report = store.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: Duration::from_secs(30),
            bytes: u64::MAX,
        },
        TierThresholds {
            graph_min_rows: GRAPH_ROWS as u32,
        },
    );
    assert_eq!(report.graphs_built, 1);
    assert!(matches!(report.status, MaintenanceStatus::Complete));
    let snapshot = store
        .snapshot()
        .expect("snapshot public metadata graph Store");
    let segment = snapshot.segments().first().expect("public graph segment");
    let id = segment.meta().id;
    let graph = segment
        .graph_node_blocks()
        .expect("decode public metadata graph nodes");
    let entries = (0..graph.node_count())
        .filter(|row| graph.block(*row).expect("public graph row").flags() & 1 != 0)
        .collect::<Vec<_>>();
    assert_eq!(entries, vec![0, 1, 2, 3]);
    drop(snapshot);
    store.close().expect("close public metadata graph Store");
    id
}

fn metadata_graph_options() -> SearchOptions {
    SearchOptions::new(ScanOptions { thread_budget: 1 }).with_tier(SearchTier::Graph(
        GraphSearchOptions::new(GraphSearchProfile::SiftClass).with_seed(0x0039_0039),
    ))
}

fn i37_graph_range_bounds(
    shape: usize,
    lower: PredicateValue,
    upper: PredicateValue,
) -> (Option<RangeBound>, Option<RangeBound>) {
    match shape {
        0 => (None, None),
        1 => (Some(RangeBound::inclusive(lower)), None),
        2 => (Some(RangeBound::exclusive(lower)), None),
        3 => (None, Some(RangeBound::inclusive(upper))),
        4 => (None, Some(RangeBound::exclusive(upper))),
        5 => (
            Some(RangeBound::inclusive(lower)),
            Some(RangeBound::inclusive(upper)),
        ),
        6 => (
            Some(RangeBound::inclusive(lower)),
            Some(RangeBound::exclusive(upper)),
        ),
        7 => (
            Some(RangeBound::exclusive(lower)),
            Some(RangeBound::inclusive(upper)),
        ),
        8 => (
            Some(RangeBound::exclusive(lower)),
            Some(RangeBound::exclusive(upper)),
        ),
        _ => panic!("I37 graph Range shape is outside 0..9"),
    }
}

fn i37_graph_oracle_range_bounds(
    shape: usize,
    lower: oracle::ScalarCell,
    upper: oracle::ScalarCell,
) -> (Option<oracle::RangeBoundDto>, Option<oracle::RangeBoundDto>) {
    let bound = |value, inclusive| oracle::RangeBoundDto { value, inclusive };
    match shape {
        0 => (None, None),
        1 => (Some(bound(lower, true)), None),
        2 => (Some(bound(lower, false)), None),
        3 => (None, Some(bound(upper, true))),
        4 => (None, Some(bound(upper, false))),
        5 => (Some(bound(lower, true)), Some(bound(upper, true))),
        6 => (Some(bound(lower, true)), Some(bound(upper, false))),
        7 => (Some(bound(lower, false)), Some(bound(upper, true))),
        8 => (Some(bound(lower, false)), Some(bound(upper, false))),
        _ => panic!("I37 graph oracle Range shape is outside 0..9"),
    }
}

fn i37_graph_matrix_cases() -> Vec<(Predicate, oracle::PredicateDto)> {
    let bool_eq = |value| Predicate::Eq {
        column: ColumnId::new(4),
        value: PredicateValue::Bool(value),
    };
    let oracle_bool_eq = |value| oracle::PredicateDto::Eq {
        column: 4,
        value: oracle::ScalarCell::Bool(value),
    };
    (0..45)
        .map(|case| match case {
            0 => (
                Predicate::Eq {
                    column: ColumnId::new(1),
                    value: PredicateValue::U64(0),
                },
                oracle::PredicateDto::Eq {
                    column: 1,
                    value: oracle::ScalarCell::U64(0),
                },
            ),
            1 => (
                Predicate::Eq {
                    column: ColumnId::new(2),
                    value: PredicateValue::I64(-5),
                },
                oracle::PredicateDto::Eq {
                    column: 2,
                    value: oracle::ScalarCell::I64(-5),
                },
            ),
            2 => (
                Predicate::Eq {
                    column: ColumnId::new(3),
                    value: PredicateValue::F64(-0.0),
                },
                oracle::PredicateDto::Eq {
                    column: 3,
                    value: oracle::ScalarCell::F64Bits((-0.0_f64).to_bits()),
                },
            ),
            3 => (bool_eq(true), oracle_bool_eq(true)),
            4 => (
                Predicate::Eq {
                    column: ColumnId::new(5),
                    value: PredicateValue::String("one".to_owned()),
                },
                oracle::PredicateDto::Eq {
                    column: 5,
                    value: oracle::ScalarCell::Utf8(b"one".to_vec()),
                },
            ),
            5 => (
                Predicate::Eq {
                    column: ColumnId::new(6),
                    value: PredicateValue::String("repeated-raw".to_owned()),
                },
                oracle::PredicateDto::Eq {
                    column: 6,
                    value: oracle::ScalarCell::Utf8(b"repeated-raw".to_vec()),
                },
            ),
            6..=8 => {
                let values = match case {
                    6 => Vec::new(),
                    7 => vec![PredicateValue::String("repeated-raw".to_owned())],
                    8 => vec![
                        PredicateValue::String("repeated-raw".to_owned()),
                        PredicateValue::String("repeated-raw".to_owned()),
                    ],
                    _ => unreachable!("I37 In case is bounded"),
                };
                let oracle_values = values
                    .iter()
                    .map(|value| match value {
                        PredicateValue::String(value) => {
                            oracle::ScalarCell::Utf8(value.as_bytes().to_vec())
                        }
                        other => panic!("unexpected I37 graph In value {other:?}"),
                    })
                    .collect();
                (
                    Predicate::In {
                        column: ColumnId::new(6),
                        values,
                    },
                    oracle::PredicateDto::In {
                        column: 6,
                        values: oracle_values,
                    },
                )
            }
            9..=35 => {
                let (column, lower, upper, oracle_lower, oracle_upper, shape) = match case {
                    9..=17 => (
                        ColumnId::new(1),
                        PredicateValue::U64(0),
                        PredicateValue::U64(u64::MAX),
                        oracle::ScalarCell::U64(0),
                        oracle::ScalarCell::U64(u64::MAX),
                        case - 9,
                    ),
                    18..=26 => (
                        ColumnId::new(2),
                        PredicateValue::I64(-5),
                        PredicateValue::I64(4),
                        oracle::ScalarCell::I64(-5),
                        oracle::ScalarCell::I64(4),
                        case - 18,
                    ),
                    _ => (
                        ColumnId::new(3),
                        PredicateValue::F64(-0.0),
                        PredicateValue::F64(6.0),
                        oracle::ScalarCell::F64Bits((-0.0_f64).to_bits()),
                        oracle::ScalarCell::F64Bits(6.0_f64.to_bits()),
                        case - 27,
                    ),
                };
                let (lower, upper) = i37_graph_range_bounds(shape, lower, upper);
                let (oracle_lower, oracle_upper) =
                    i37_graph_oracle_range_bounds(shape, oracle_lower, oracle_upper);
                (
                    Predicate::Range(RangePredicate {
                        column,
                        lower,
                        upper,
                    }),
                    oracle::PredicateDto::Range {
                        column: column.get(),
                        lower: oracle_lower,
                        upper: oracle_upper,
                    },
                )
            }
            36 => {
                let nan = f64::from_bits(0x7ff8_0000_0000_00a5);
                (
                    Predicate::Range(RangePredicate {
                        column: ColumnId::new(3),
                        lower: Some(RangeBound::inclusive(PredicateValue::F64(nan))),
                        upper: None,
                    }),
                    oracle::PredicateDto::Range {
                        column: 3,
                        lower: Some(oracle::RangeBoundDto {
                            value: oracle::ScalarCell::F64Bits(nan.to_bits()),
                            inclusive: true,
                        }),
                        upper: None,
                    },
                )
            }
            37 => {
                let nan = f64::from_bits(0x7ff8_0000_0000_00a5);
                (
                    Predicate::Range(RangePredicate {
                        column: ColumnId::new(3),
                        lower: None,
                        upper: Some(RangeBound::exclusive(PredicateValue::F64(nan))),
                    }),
                    oracle::PredicateDto::Range {
                        column: 3,
                        lower: None,
                        upper: Some(oracle::RangeBoundDto {
                            value: oracle::ScalarCell::F64Bits(nan.to_bits()),
                            inclusive: false,
                        }),
                    },
                )
            }
            38 => (
                Predicate::Exists(ColumnId::new(6)),
                oracle::PredicateDto::Exists(6),
            ),
            39 => (
                Predicate::IsNull(ColumnId::new(6)),
                oracle::PredicateDto::IsNull(6),
            ),
            40 => (
                Predicate::And(Vec::new()),
                oracle::PredicateDto::And(Vec::new()),
            ),
            41 => (
                Predicate::Or(Vec::new()),
                oracle::PredicateDto::Or(Vec::new()),
            ),
            42 => (
                Predicate::And(vec![
                    Predicate::Or(vec![bool_eq(true), Predicate::IsNull(ColumnId::new(4))]),
                    Predicate::Exists(ColumnId::new(5)),
                ]),
                oracle::PredicateDto::And(vec![
                    oracle::PredicateDto::Or(vec![
                        oracle_bool_eq(true),
                        oracle::PredicateDto::IsNull(4),
                    ]),
                    oracle::PredicateDto::Exists(5),
                ]),
            ),
            43 => (
                Predicate::Not(Box::new(Predicate::Not(Box::new(bool_eq(true))))),
                oracle::PredicateDto::Not(Box::new(oracle::PredicateDto::Not(Box::new(
                    oracle_bool_eq(true),
                )))),
            ),
            44 => (
                Predicate::Not(Box::new(Predicate::IsNull(ColumnId::new(6)))),
                oracle::PredicateDto::Not(Box::new(oracle::PredicateDto::IsNull(6))),
            ),
            _ => unreachable!("I37 graph case is bounded by 45"),
        })
        .collect()
}

#[test]
fn i39_graph_fixture_is_published_by_the_public_store_lifecycle() {
    let directory = tempdir().expect("metadata public graph lifecycle directory");
    let id = publish_metadata_chain_graph(directory.path());
    let reader = SegmentReader::open(&StdVfs, &directory.path().join(id.file_name()), id)
        .expect("open metadata public graph lifecycle segment");
    for row in 0..GRAPH_ROWS {
        assert!(
            reader
                .document_version(row)
                .expect("read metadata graph document identity")
                .is_some(),
            "I39 graph fixture row {row} lacks public document identity"
        );
    }
}

fn graph_result_bits(outcome: &zeppelin_embed::planner::FilteredSearchOutcome) -> Vec<(u32, u32)> {
    outcome
        .candidates
        .iter()
        .map(|candidate| (candidate.row_id().local_row(), candidate.score().to_bits()))
        .collect()
}

fn attest_one_graph_outcome(
    query_id: u64,
    outcome: &zeppelin_embed::planner::FilteredSearchOutcome,
    receipts: &[MetadataExecutionReceipt],
) {
    oracle::compare_i39(&oracle::I39Observed {
        reports: outcome
            .plans
            .iter()
            .map(|plan| branch_report(query_id, plan))
            .collect(),
        diagnostics_reports: outcome
            .diagnostics
            .plan
            .iter()
            .map(|plan| branch_report(query_id, plan))
            .collect(),
        receipts: receipts.iter().map(execution_receipt).collect(),
        allow_list_threshold: ALLOW_LIST_ROWS_THRESHOLD,
    })
    .unwrap_or_else(|error| panic!("{error}"));
}

#[test]
fn i36_all_null_nullable_dictionary_reopens_without_inventing_a_value() {
    let schema = Schema::new(vec![ColumnDefinition::new(
        ColumnId::new(1),
        "nullable_dictionary",
        ColumnType::DictionaryString,
        true,
    )])
    .expect("valid nullable dictionary schema");
    let mut builder = ColumnStoreBuilder::new(schema.clone());
    builder.push_row(7, &[]).expect("first null row");
    builder.push_row(11, &[]).expect("second null row");
    let columns = builder.finish().expect("all-null column store");
    let alive = AliveSet::new(2);
    let factors = [Bit4Factors::from_persisted(1.0, 1.0, 0.0); 2];
    let id = SegmentId::new(0x4933_3600, [0x36; 10]);
    let directory = tempdir().expect("segment directory");

    write_segment(
        &StdVfs,
        directory.path(),
        SegmentBuild {
            id,
            scheme: 4,
            dims: 1,
            codes: &[0x80, 0x70],
            factors: SegmentFactors::Bit4(&factors),
            rescore: &[1.0, -1.0],
            columns: &columns,
            alive: &alive,
        },
        ordered_policy(),
    )
    .expect("write checked segment");

    let reader = SegmentReader::open(&StdVfs, &directory.path().join(id.file_name()), id)
        .expect("reopen checked segment");
    let decoded = reader.columns().unwrap_or_else(|error| {
        panic!(
            "I36.column-roundtrip.v2 source={id:?} column=1 row=0 expected=Null observed={error}"
        )
    });
    match decoded.column(ColumnId::new(1)) {
        Some(Column::DictionaryString(column)) => {
            assert!(column.dictionary().is_empty());
            assert_eq!(column.codes().get(0), Some(0));
            assert_eq!(column.codes().get(1), Some(0));
            assert_eq!(column.get(0), None);
            assert_eq!(column.get(1), None);
        }
        actual => panic!("unexpected reopened column: {actual:?}"),
    }

    let store_directory = tempdir().expect("all-null public Store");
    let store = Store::open(
        store_directory.path(),
        OpenOptions::default().with_schema(schema.clone()),
    )
    .expect("open all-null public Store");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(1), Revision::new(1)),
                vec![1.0],
            ),
            IngestDocument::new(
                DocumentVersion::new(DocId::new(2), Revision::new(1)),
                vec![2.0],
            ),
        ]))
        .expect("ingest all-null public rows");
    store.seal().expect("seal all-null public rows");
    store.close().expect("close all-null public Store");
    let reopened = Store::open(
        store_directory.path(),
        OpenOptions::default().with_schema(schema),
    )
    .expect("reopen all-null public Store");
    let observed = reopened
        .search_filtered(
            SearchRequest::new(&[0.0]),
            &Predicate::IsNull(ColumnId::new(1)),
            2,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("query reopened all-null dictionary")
        .candidates
        .iter()
        .map(|candidate| {
            candidate
                .document()
                .expect("all-null public document")
                .doc_id()
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(observed, BTreeSet::from([DocId::new(1), DocId::new(2)]));
    reopened.close().expect("close reopened all-null Store");
}

#[test]
fn i37_public_evaluator_matches_independent_btreeset_algebra() {
    let schema = Schema::new(vec![
        ColumnDefinition::new(ColumnId::new(1), "u", ColumnType::U64, true),
        ColumnDefinition::new(ColumnId::new(2), "i", ColumnType::I64, true),
        ColumnDefinition::new(ColumnId::new(3), "f", ColumnType::F64, true),
        ColumnDefinition::new(ColumnId::new(4), "b", ColumnType::Bool, true),
        ColumnDefinition::new(ColumnId::new(5), "d", ColumnType::DictionaryString, true),
        ColumnDefinition::new(ColumnId::new(6), "r", ColumnType::RawString, true),
    ])
    .expect("complete metadata schema");
    let mut builder = ColumnStoreBuilder::new(schema.clone());
    builder
        .push_row(
            -1,
            &[
                ColumnInput {
                    column: ColumnId::new(1),
                    value: ColumnValue::U64(1),
                },
                ColumnInput {
                    column: ColumnId::new(2),
                    value: ColumnValue::I64(-1),
                },
                ColumnInput {
                    column: ColumnId::new(3),
                    value: ColumnValue::F64(-0.0),
                },
                ColumnInput {
                    column: ColumnId::new(4),
                    value: ColumnValue::Bool(true),
                },
                ColumnInput {
                    column: ColumnId::new(5),
                    value: ColumnValue::String("a"),
                },
                ColumnInput {
                    column: ColumnId::new(6),
                    value: ColumnValue::String("raw"),
                },
            ],
        )
        .expect("first row");
    builder
        .push_row(
            0,
            &[
                ColumnInput {
                    column: ColumnId::new(1),
                    value: ColumnValue::U64(2),
                },
                ColumnInput {
                    column: ColumnId::new(2),
                    value: ColumnValue::I64(0),
                },
                ColumnInput {
                    column: ColumnId::new(3),
                    value: ColumnValue::F64(0.5),
                },
                ColumnInput {
                    column: ColumnId::new(4),
                    value: ColumnValue::Bool(false),
                },
                ColumnInput {
                    column: ColumnId::new(5),
                    value: ColumnValue::String("b"),
                },
            ],
        )
        .expect("tombstoned row");
    builder
        .push_row(
            1,
            &[
                ColumnInput {
                    column: ColumnId::new(1),
                    value: ColumnValue::U64(3),
                },
                ColumnInput {
                    column: ColumnId::new(2),
                    value: ColumnValue::I64(1),
                },
                ColumnInput {
                    column: ColumnId::new(3),
                    value: ColumnValue::F64(f64::from_bits(0x7ff8_0000_0000_0001)),
                },
                ColumnInput {
                    column: ColumnId::new(4),
                    value: ColumnValue::Bool(true),
                },
                ColumnInput {
                    column: ColumnId::new(5),
                    value: ColumnValue::String("a"),
                },
                ColumnInput {
                    column: ColumnId::new(6),
                    value: ColumnValue::String(""),
                },
            ],
        )
        .expect("third row");
    let columns = builder.finish().expect("metadata columns");
    let mut alive = AliveSet::new(3);
    alive.tombstone(1).expect("middle row exists");

    let store_directory = tempdir().expect("public metadata Store");
    let store = Store::open(
        store_directory.path(),
        OpenOptions::default().with_schema(schema.clone()),
    )
    .expect("open public metadata Store");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(1), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_timestamp(-1)
            .with_columns(vec![
                (ColumnId::new(1), PredicateValue::U64(1)),
                (ColumnId::new(2), PredicateValue::I64(-1)),
                (ColumnId::new(3), PredicateValue::F64(-0.0)),
                (ColumnId::new(4), PredicateValue::Bool(true)),
                (ColumnId::new(5), PredicateValue::String("a".to_owned())),
                (ColumnId::new(6), PredicateValue::String("raw".to_owned())),
            ]),
            IngestDocument::new(
                DocumentVersion::new(DocId::new(2), Revision::new(1)),
                vec![0.0, 1.0],
            )
            .with_timestamp(0)
            .with_columns(vec![
                (ColumnId::new(1), PredicateValue::U64(2)),
                (ColumnId::new(2), PredicateValue::I64(0)),
                (ColumnId::new(3), PredicateValue::F64(0.5)),
                (ColumnId::new(4), PredicateValue::Bool(false)),
                (ColumnId::new(5), PredicateValue::String("b".to_owned())),
            ]),
            IngestDocument::new(
                DocumentVersion::new(DocId::new(3), Revision::new(1)),
                vec![-1.0, 0.0],
            )
            .with_timestamp(1)
            .with_columns(vec![
                (ColumnId::new(1), PredicateValue::U64(3)),
                (ColumnId::new(2), PredicateValue::I64(1)),
                (
                    ColumnId::new(3),
                    PredicateValue::F64(f64::from_bits(0x7ff8_0000_0000_0001)),
                ),
                (ColumnId::new(4), PredicateValue::Bool(true)),
                (ColumnId::new(5), PredicateValue::String("a".to_owned())),
                (ColumnId::new(6), PredicateValue::String(String::new())),
            ]),
        ]))
        .expect("ingest public metadata rows");
    store
        .delete(DeleteBatch::new(vec![DocId::new(2)]))
        .expect("tombstone middle public row");

    let rows = vec![
        oracle::MetadataRowDto {
            row_id: 0,
            cells: BTreeMap::from([
                (0, oracle::ScalarCell::I64(-1)),
                (1, oracle::ScalarCell::U64(1)),
                (2, oracle::ScalarCell::I64(-1)),
                (3, oracle::ScalarCell::F64Bits((-0.0_f64).to_bits())),
                (4, oracle::ScalarCell::Bool(true)),
                (5, oracle::ScalarCell::Utf8(b"a".to_vec())),
                (6, oracle::ScalarCell::Utf8(b"raw".to_vec())),
            ]),
        },
        oracle::MetadataRowDto {
            row_id: 1,
            cells: BTreeMap::from([
                (0, oracle::ScalarCell::I64(0)),
                (1, oracle::ScalarCell::U64(2)),
                (2, oracle::ScalarCell::I64(0)),
                (3, oracle::ScalarCell::F64Bits(0.5_f64.to_bits())),
                (4, oracle::ScalarCell::Bool(false)),
                (5, oracle::ScalarCell::Utf8(b"b".to_vec())),
                (6, oracle::ScalarCell::Null),
            ]),
        },
        oracle::MetadataRowDto {
            row_id: 2,
            cells: BTreeMap::from([
                (0, oracle::ScalarCell::I64(1)),
                (1, oracle::ScalarCell::U64(3)),
                (2, oracle::ScalarCell::I64(1)),
                (3, oracle::ScalarCell::F64Bits(0x7ff8_0000_0000_0001)),
                (4, oracle::ScalarCell::Bool(true)),
                (5, oracle::ScalarCell::Utf8(b"a".to_vec())),
                (6, oracle::ScalarCell::Utf8(Vec::new())),
            ]),
        },
    ];
    let mut cases = vec![
        (
            Predicate::Eq {
                column: ColumnId::new(1),
                value: PredicateValue::U64(1),
            },
            oracle::PredicateDto::Eq {
                column: 1,
                value: oracle::ScalarCell::U64(1),
            },
        ),
        (
            Predicate::Eq {
                column: ColumnId::new(2),
                value: PredicateValue::I64(1),
            },
            oracle::PredicateDto::Eq {
                column: 2,
                value: oracle::ScalarCell::I64(1),
            },
        ),
        (
            Predicate::Eq {
                column: ColumnId::new(3),
                value: PredicateValue::F64(-0.0),
            },
            oracle::PredicateDto::Eq {
                column: 3,
                value: oracle::ScalarCell::F64Bits((-0.0_f64).to_bits()),
            },
        ),
        (
            Predicate::Eq {
                column: ColumnId::new(4),
                value: PredicateValue::Bool(true),
            },
            oracle::PredicateDto::Eq {
                column: 4,
                value: oracle::ScalarCell::Bool(true),
            },
        ),
        (
            Predicate::Eq {
                column: ColumnId::new(5),
                value: PredicateValue::String("a".to_owned()),
            },
            oracle::PredicateDto::Eq {
                column: 5,
                value: oracle::ScalarCell::Utf8(b"a".to_vec()),
            },
        ),
        (
            Predicate::Eq {
                column: ColumnId::new(6),
                value: PredicateValue::String("raw".to_owned()),
            },
            oracle::PredicateDto::Eq {
                column: 6,
                value: oracle::ScalarCell::Utf8(b"raw".to_vec()),
            },
        ),
        (
            Predicate::In {
                column: ColumnId::new(1),
                values: vec![
                    PredicateValue::U64(3),
                    PredicateValue::U64(3),
                    PredicateValue::U64(99),
                ],
            },
            oracle::PredicateDto::In {
                column: 1,
                values: vec![
                    oracle::ScalarCell::U64(3),
                    oracle::ScalarCell::U64(3),
                    oracle::ScalarCell::U64(99),
                ],
            },
        ),
        (
            Predicate::Range(RangePredicate {
                column: ColumnId::new(2),
                lower: Some(RangeBound::exclusive(PredicateValue::I64(-1))),
                upper: Some(RangeBound::inclusive(PredicateValue::I64(1))),
            }),
            oracle::PredicateDto::Range {
                column: 2,
                lower: Some(oracle::RangeBoundDto {
                    value: oracle::ScalarCell::I64(-1),
                    inclusive: false,
                }),
                upper: Some(oracle::RangeBoundDto {
                    value: oracle::ScalarCell::I64(1),
                    inclusive: true,
                }),
            },
        ),
        (
            Predicate::Exists(ColumnId::new(6)),
            oracle::PredicateDto::Exists(6),
        ),
        (
            Predicate::IsNull(ColumnId::new(6)),
            oracle::PredicateDto::IsNull(6),
        ),
        (
            Predicate::And(Vec::new()),
            oracle::PredicateDto::And(Vec::new()),
        ),
        (
            Predicate::Or(Vec::new()),
            oracle::PredicateDto::Or(Vec::new()),
        ),
        (
            Predicate::Not(Box::new(Predicate::Not(Box::new(Predicate::Eq {
                column: ColumnId::new(4),
                value: PredicateValue::Bool(true),
            })))),
            oracle::PredicateDto::Not(Box::new(oracle::PredicateDto::Not(Box::new(
                oracle::PredicateDto::Eq {
                    column: 4,
                    value: oracle::ScalarCell::Bool(true),
                },
            )))),
        ),
    ];
    for values in [Vec::new(), vec![PredicateValue::U64(1)]] {
        cases.push((
            Predicate::In {
                column: ColumnId::new(1),
                values: values.clone(),
            },
            oracle::PredicateDto::In {
                column: 1,
                values: values
                    .into_iter()
                    .map(|value| match value {
                        PredicateValue::U64(value) => oracle::ScalarCell::U64(value),
                        other => panic!("unexpected U64 matrix value {other:?}"),
                    })
                    .collect(),
            },
        ));
    }
    for (lower, upper) in [
        (None, None),
        (Some((1_u64, true)), None),
        (Some((1_u64, false)), None),
        (None, Some((3_u64, true))),
        (None, Some((3_u64, false))),
        (Some((1_u64, true)), Some((3_u64, true))),
        (Some((1_u64, true)), Some((3_u64, false))),
        (Some((1_u64, false)), Some((3_u64, true))),
        (Some((1_u64, false)), Some((3_u64, false))),
    ] {
        cases.push((
            Predicate::Range(RangePredicate {
                column: ColumnId::new(1),
                lower: lower.map(|(value, inclusive)| RangeBound {
                    value: PredicateValue::U64(value),
                    inclusive,
                }),
                upper: upper.map(|(value, inclusive)| RangeBound {
                    value: PredicateValue::U64(value),
                    inclusive,
                }),
            }),
            oracle::PredicateDto::Range {
                column: 1,
                lower: lower.map(|(value, inclusive)| oracle::RangeBoundDto {
                    value: oracle::ScalarCell::U64(value),
                    inclusive,
                }),
                upper: upper.map(|(value, inclusive)| oracle::RangeBoundDto {
                    value: oracle::ScalarCell::U64(value),
                    inclusive,
                }),
            },
        ));
    }
    for (lower, upper) in [
        (None, None),
        (Some((-1_i64, true)), None),
        (Some((-1_i64, false)), None),
        (None, Some((1_i64, true))),
        (None, Some((1_i64, false))),
        (Some((-1_i64, true)), Some((1_i64, true))),
        (Some((-1_i64, true)), Some((1_i64, false))),
        (Some((-1_i64, false)), Some((1_i64, true))),
        (Some((-1_i64, false)), Some((1_i64, false))),
    ] {
        cases.push((
            Predicate::Range(RangePredicate {
                column: ColumnId::new(2),
                lower: lower.map(|(value, inclusive)| RangeBound {
                    value: PredicateValue::I64(value),
                    inclusive,
                }),
                upper: upper.map(|(value, inclusive)| RangeBound {
                    value: PredicateValue::I64(value),
                    inclusive,
                }),
            }),
            oracle::PredicateDto::Range {
                column: 2,
                lower: lower.map(|(value, inclusive)| oracle::RangeBoundDto {
                    value: oracle::ScalarCell::I64(value),
                    inclusive,
                }),
                upper: upper.map(|(value, inclusive)| oracle::RangeBoundDto {
                    value: oracle::ScalarCell::I64(value),
                    inclusive,
                }),
            },
        ));
    }
    for (lower, upper) in [
        (None, None),
        (Some((-0.0_f64, true)), None),
        (Some((-0.0_f64, false)), None),
        (None, Some((1.0_f64, true))),
        (None, Some((1.0_f64, false))),
        (Some((-0.0_f64, true)), Some((1.0_f64, true))),
        (Some((-0.0_f64, true)), Some((1.0_f64, false))),
        (Some((-0.0_f64, false)), Some((1.0_f64, true))),
        (Some((-0.0_f64, false)), Some((1.0_f64, false))),
    ] {
        cases.push((
            Predicate::Range(RangePredicate {
                column: ColumnId::new(3),
                lower: lower.map(|(value, inclusive)| RangeBound {
                    value: PredicateValue::F64(value),
                    inclusive,
                }),
                upper: upper.map(|(value, inclusive)| RangeBound {
                    value: PredicateValue::F64(value),
                    inclusive,
                }),
            }),
            oracle::PredicateDto::Range {
                column: 3,
                lower: lower.map(|(value, inclusive)| oracle::RangeBoundDto {
                    value: oracle::ScalarCell::F64Bits(value.to_bits()),
                    inclusive,
                }),
                upper: upper.map(|(value, inclusive)| oracle::RangeBoundDto {
                    value: oracle::ScalarCell::F64Bits(value.to_bits()),
                    inclusive,
                }),
            },
        ));
    }
    for (lower_nan, upper_nan) in [(true, false), (false, true)] {
        let nan = f64::from_bits(0x7ff8_0000_0000_00a5);
        cases.push((
            Predicate::Range(RangePredicate {
                column: ColumnId::new(3),
                lower: lower_nan.then(|| RangeBound::inclusive(PredicateValue::F64(nan))),
                upper: upper_nan.then(|| RangeBound::exclusive(PredicateValue::F64(nan))),
            }),
            oracle::PredicateDto::Range {
                column: 3,
                lower: lower_nan.then_some(oracle::RangeBoundDto {
                    value: oracle::ScalarCell::F64Bits(nan.to_bits()),
                    inclusive: true,
                }),
                upper: upper_nan.then_some(oracle::RangeBoundDto {
                    value: oracle::ScalarCell::F64Bits(nan.to_bits()),
                    inclusive: false,
                }),
            },
        ));
    }
    let mut matrix_coverage = BTreeSet::new();
    for (predicate, _) in &cases {
        match predicate {
            Predicate::In { values, .. } => {
                matrix_coverage.insert(format!("in-cardinality-{}", values.len()));
                if values
                    .iter()
                    .enumerate()
                    .any(|(index, value)| values[..index].contains(value))
                {
                    matrix_coverage.insert("in-duplicates".to_owned());
                }
            }
            Predicate::Range(range) => {
                let numeric = match range.column.get() {
                    1 => "u64",
                    2 => "i64",
                    3 => "f64",
                    other => panic!("unexpected I37 numeric range column {other}"),
                };
                let bound = |bound: &Option<RangeBound>| match bound {
                    None => "unbounded",
                    Some(bound) if bound.inclusive => "inclusive",
                    Some(_) => "exclusive",
                };
                matrix_coverage.insert(format!(
                    "range-{numeric}-lower-{}-upper-{}",
                    bound(&range.lower),
                    bound(&range.upper),
                ));
                if range.lower.as_ref().is_some_and(
                    |bound| matches!(bound.value, PredicateValue::F64(value) if value.is_nan()),
                ) {
                    matrix_coverage.insert("range-f64-nan-lower".to_owned());
                }
                if range.upper.as_ref().is_some_and(
                    |bound| matches!(bound.value, PredicateValue::F64(value) if value.is_nan()),
                ) {
                    matrix_coverage.insert("range-f64-nan-upper".to_owned());
                }
            }
            _ => {}
        }
    }
    for required in [
        "in-cardinality-0",
        "in-cardinality-1",
        "in-duplicates",
        "range-u64-lower-unbounded-upper-unbounded",
        "range-u64-lower-inclusive-upper-unbounded",
        "range-u64-lower-exclusive-upper-unbounded",
        "range-u64-lower-unbounded-upper-inclusive",
        "range-u64-lower-unbounded-upper-exclusive",
        "range-u64-lower-inclusive-upper-inclusive",
        "range-u64-lower-inclusive-upper-exclusive",
        "range-u64-lower-exclusive-upper-inclusive",
        "range-u64-lower-exclusive-upper-exclusive",
        "range-i64-lower-unbounded-upper-unbounded",
        "range-i64-lower-inclusive-upper-unbounded",
        "range-i64-lower-exclusive-upper-unbounded",
        "range-i64-lower-unbounded-upper-inclusive",
        "range-i64-lower-unbounded-upper-exclusive",
        "range-i64-lower-inclusive-upper-inclusive",
        "range-i64-lower-inclusive-upper-exclusive",
        "range-i64-lower-exclusive-upper-inclusive",
        "range-i64-lower-exclusive-upper-exclusive",
        "range-f64-lower-unbounded-upper-unbounded",
        "range-f64-lower-inclusive-upper-unbounded",
        "range-f64-lower-exclusive-upper-unbounded",
        "range-f64-lower-unbounded-upper-inclusive",
        "range-f64-lower-unbounded-upper-exclusive",
        "range-f64-lower-inclusive-upper-inclusive",
        "range-f64-lower-inclusive-upper-exclusive",
        "range-f64-lower-exclusive-upper-inclusive",
        "range-f64-lower-exclusive-upper-exclusive",
        "range-f64-nan-lower",
        "range-f64-nan-upper",
    ] {
        assert!(
            matrix_coverage.contains(required),
            "I37 public matrix missing {required}; observed={matrix_coverage:?}"
        );
    }
    let run_matrix = |store: &Store, phase: &str| {
        for (production, expected) in &cases {
            let evaluator = evaluate(production, &columns, &alive)
                .expect("schema-valid predicate")
                .iter()
                .collect::<BTreeSet<_>>();
            let public_results = store
                .search_filtered(
                    SearchRequest::new(&[1.0, 0.0]),
                    production,
                    3,
                    SearchOptions::default().with_tier(SearchTier::Exact),
                    QueryControl::Cancel(CancelToken::new()),
                )
                .unwrap_or_else(|error| panic!("{phase} public filtered search: {error}"))
                .candidates
                .into_iter()
                .map(|candidate| {
                    let document = candidate.document().expect("public candidate document");
                    u32::try_from(document.doc_id().get() - 1).expect("fixture row fits u32")
                })
                .collect();
            oracle::compare_i37(
                &oracle::I37Input {
                    rows: rows.clone(),
                    live: BTreeSet::from([0, 2]),
                    predicate: expected.clone(),
                    sources: Vec::new(),
                },
                &oracle::I37Observed {
                    evaluator,
                    public_results,
                    sources: Vec::new(),
                    allow_list_threshold: 64,
                },
            )
            .unwrap_or_else(|error| panic!("{error}"));
        }
    };
    run_matrix(&store, "active");
    store.seal().expect("seal public metadata matrix");
    run_matrix(&store, "sealed");
    store.close().expect("close public metadata Store");
    let reopened = Store::open(
        store_directory.path(),
        OpenOptions::default().with_schema(schema),
    )
    .expect("reopen public metadata matrix");
    run_matrix(&reopened, "reopened");
    reopened.close().expect("close reopened metadata Store");
}

#[test]
fn i37_graph_backed_filter_matches_independent_timestamp_range() {
    let directory = tempdir().expect("I37 graph-backed directory");
    publish_metadata_chain_graph(directory.path());
    let store = Store::open(
        directory.path(),
        OpenOptions::default().with_epoch(metadata_graph_epoch()),
    )
    .expect("open I37 graph-backed Store");
    let snapshot = store.snapshot().expect("I37 graph-backed snapshot");
    let segment = snapshot
        .segments()
        .first()
        .expect("I37 graph-backed segment");
    let columns = segment.columns().expect("I37 graph-backed Columns");
    let alive = segment.alive().expect("I37 graph-backed Alive");
    drop(snapshot);
    let predicate = Predicate::Range(RangePredicate {
        column: zeppelin_embed::meta::TIMESTAMP_COLUMN,
        lower: Some(RangeBound::inclusive(PredicateValue::I64(10))),
        upper: Some(RangeBound::inclusive(PredicateValue::I64(20))),
    });
    let evaluator = evaluate(&predicate, &columns, &alive)
        .expect("I37 graph-backed evaluator")
        .iter()
        .collect::<BTreeSet<_>>();
    let public_results = store
        .search_filtered(
            SearchRequest::new(&vec![0.0_f32; 128]),
            &predicate,
            11,
            metadata_graph_options(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("I37 graph-backed public filter")
        .candidates
        .into_iter()
        .map(|candidate| candidate.row_id().local_row())
        .collect::<Vec<_>>();
    oracle::compare_i37(
        &oracle::I37Input {
            rows: (0..80_u32)
                .map(|row_id| oracle::MetadataRowDto {
                    row_id,
                    cells: BTreeMap::from([(0, oracle::ScalarCell::I64(i64::from(row_id)))]),
                })
                .collect(),
            live: (0..80_u32).collect(),
            predicate: oracle::PredicateDto::Range {
                column: 0,
                lower: Some(oracle::RangeBoundDto {
                    value: oracle::ScalarCell::I64(10),
                    inclusive: true,
                }),
                upper: Some(oracle::RangeBoundDto {
                    value: oracle::ScalarCell::I64(20),
                    inclusive: true,
                }),
            },
            sources: Vec::new(),
        },
        &oracle::I37Observed {
            evaluator,
            public_results,
            sources: Vec::new(),
            allow_list_threshold: 64,
        },
    )
    .unwrap_or_else(|error| panic!("{error}"));
    store.close().expect("close I37 graph-backed Store");
}

#[test]
fn i37_full_45_case_matrix_runs_against_graph_published_public_store() {
    let cases = i37_graph_matrix_cases();
    assert_eq!(
        cases.len(),
        45,
        "I37 graph-backed public matrix must execute exactly 45 predicate cases"
    );
    let directory = tempdir().expect("I37 full graph-backed matrix directory");
    publish_metadata_chain_graph(directory.path());
    let store = Store::open(
        directory.path(),
        OpenOptions::default().with_epoch(metadata_graph_epoch()),
    )
    .expect("open I37 full graph-backed matrix Store");
    let snapshot = store.snapshot().expect("snapshot I37 graph-backed matrix");
    let segment = snapshot
        .segments()
        .first()
        .expect("I37 graph-backed matrix segment");
    assert!(
        segment.graph_node_blocks().is_ok(),
        "I37 public matrix source is not graph-backed"
    );
    let columns = segment.columns().expect("I37 graph-backed matrix Columns");
    let alive = segment.alive().expect("I37 graph-backed matrix Alive");
    drop(snapshot);

    let rows = metadata_graph_oracle_rows();
    let live = (0..u32::try_from(GRAPH_ROWS).expect("graph rows fit u32")).collect::<BTreeSet<_>>();
    let query = vec![0.0_f32; GRAPH_DIMS];
    let mut graph_execution_observed = false;
    for (case, (predicate, predicate_dto)) in cases.iter().enumerate() {
        let evaluator = evaluate(predicate, &columns, &alive)
            .unwrap_or_else(|error| panic!("I37 graph case {case} evaluator: {error}"))
            .iter()
            .collect::<BTreeSet<_>>();
        let outcome = store
            .search_filtered(
                SearchRequest::new(&query),
                predicate,
                GRAPH_ROWS,
                metadata_graph_options(),
                QueryControl::Cancel(CancelToken::new()),
            )
            .unwrap_or_else(|error| panic!("I37 graph case {case} public filter: {error}"));
        graph_execution_observed |= outcome.plans.iter().any(|plan| {
            matches!(
                plan.branch,
                SegmentBranch::FilteredGraph | SegmentBranch::GraphExactFallback
            )
        });
        let public_results = outcome
            .candidates
            .into_iter()
            .map(|candidate| candidate.row_id().local_row())
            .collect();
        oracle::compare_i37(
            &oracle::I37Input {
                rows: rows.clone(),
                live: live.clone(),
                predicate: predicate_dto.clone(),
                sources: Vec::new(),
            },
            &oracle::I37Observed {
                evaluator,
                public_results,
                sources: Vec::new(),
                allow_list_threshold: ALLOW_LIST_ROWS_THRESHOLD,
            },
        )
        .unwrap_or_else(|error| panic!("I37 graph case {case}: {error}"));
    }
    assert!(
        graph_execution_observed,
        "I37 full matrix never executed a graph-backed public branch"
    );
    store.close().expect("close I37 graph-backed matrix Store");
}

#[test]
fn i39_public_store_attests_both_sides_of_the_exact_threshold() {
    let directory = tempdir().expect("threshold Store directory");
    let keep = ColumnId::new(1);
    let schema = Schema::new(vec![ColumnDefinition::new(
        keep,
        "keep",
        ColumnType::U64,
        false,
    )])
    .expect("threshold schema");
    let controller = Arc::new(MetadataTestController::new());
    controller
        .arm_selectivity_pair(3900, 3901, ALLOW_LIST_ROWS_THRESHOLD)
        .expect("arm exact threshold pair");
    let dependencies = StoreTestDependencies::new(Arc::new(StdVfs), Arc::new(SystemMonotonicClock))
        .with_metadata_test_controller(Arc::clone(&controller));
    let store = Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::default().with_schema(schema),
        dependencies,
    )
    .expect("open threshold Store");
    let row_count = usize::try_from(ALLOW_LIST_ROWS_THRESHOLD + 1).expect("threshold fits usize");
    let documents = (0..row_count)
        .map(|row| {
            let document = u128::try_from(row + 1).expect("document fits u128");
            IngestDocument::new(
                DocumentVersion::new(DocId::new(document), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_columns(vec![(
                keep,
                PredicateValue::U64(u64::from(row < row_count - 1)),
            )])
        })
        .collect::<Vec<_>>();
    store
        .ingest(IngestBatch::new(documents))
        .expect("ingest threshold rows");
    let options = SearchOptions::default().with_tier(SearchTier::Exact);
    let threshold = store
        .search_filtered(
            SearchRequest::new(&[1.0, 0.0]),
            &Predicate::Eq {
                column: keep,
                value: PredicateValue::U64(1),
            },
            row_count,
            options,
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("execute threshold allow-list");
    let above = store
        .search_filtered(
            SearchRequest::new(&[1.0, 0.0]),
            &Predicate::And(Vec::new()),
            row_count,
            options,
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("execute threshold-plus masked scan");

    assert_eq!(threshold.candidates.len(), row_count - 1);
    assert_eq!(above.candidates.len(), row_count);
    let mut reports = Vec::new();
    let mut diagnostics_reports = Vec::new();
    for (query_id, outcome) in [(3900, &threshold), (3901, &above)] {
        reports.extend(
            outcome
                .plans
                .iter()
                .map(|plan| branch_report(query_id, plan)),
        );
        diagnostics_reports.extend(
            outcome
                .diagnostics
                .plan
                .iter()
                .map(|plan| branch_report(query_id, plan)),
        );
    }
    let receipts = controller
        .drain_execution_receipts()
        .expect("drain production execution receipts")
        .iter()
        .map(execution_receipt)
        .collect();
    oracle::compare_i39(&oracle::I39Observed {
        reports,
        diagnostics_reports,
        receipts,
        allow_list_threshold: ALLOW_LIST_ROWS_THRESHOLD,
    })
    .unwrap_or_else(|error| panic!("{error}"));

    let feature = controller
        .drain_feature_receipts()
        .expect("drain production feature receipts");
    assert_eq!(feature.len(), 2, "both threshold operations must emit once");
    assert!(matches!(
        feature[0].detail,
        MetadataFeatureDetail::SelectivityBoundaryChosen {
            cardinality: ALLOW_LIST_ROWS_THRESHOLD,
            threshold: ALLOW_LIST_ROWS_THRESHOLD,
            branch: SegmentBranch::ExactAllowList,
            ..
        }
    ));
    assert!(matches!(
        feature[1].detail,
        MetadataFeatureDetail::SelectivityBoundaryChosen {
            cardinality,
            threshold: ALLOW_LIST_ROWS_THRESHOLD,
            branch: SegmentBranch::MaskedScan,
            ..
        } if cardinality == ALLOW_LIST_ROWS_THRESHOLD + 1
    ));
    assert!(feature.iter().all(|receipt| {
        receipt.origin.campaign() == "metadata-filter-planner"
            && receipt.origin.fault() == "selectivity-boundary"
            && receipt.origin.cardinality() == 1
            && receipt.origin.site() == "planner.choose.allow-list-threshold"
    }));
    controller
        .assert_no_unconsumed_arm()
        .expect("both threshold arms were consumed");
    store.close().expect("close threshold Store");
}

#[test]
fn filtered_public_ties_order_document_id_before_physical_source() {
    let directory = tempdir().expect("filtered tie-order Store directory");
    let store = Store::open(
        directory.path(),
        OpenOptions::default().with_schema(Schema::timestamp_only()),
    )
    .expect("open filtered tie-order Store");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(1), Revision::new(1)),
                vec![0.0, 0.0],
            )
            .with_timestamp(10),
        ]))
        .expect("ingest sealed tie row");
    store.seal().expect("seal lower document id");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(2), Revision::new(1)),
                vec![0.0, 0.0],
            )
            .with_timestamp(10),
        ]))
        .expect("ingest active tie row");
    let outcome = store
        .search_filtered(
            SearchRequest::new(&[0.0, 0.0]),
            &Predicate::Eq {
                column: zeppelin_embed::meta::TIMESTAMP_COLUMN,
                value: PredicateValue::I64(10),
            },
            2,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("query equal-score active/sealed tie");
    let documents = outcome
        .candidates
        .iter()
        .map(|candidate| {
            candidate
                .document()
                .expect("filtered candidate identity")
                .doc_id()
                .get()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        documents,
        vec![1, 2],
        "equal-score filtered results must order document ID before physical source"
    );
    store.close().expect("close filtered tie-order Store");
}

#[test]
fn i38_public_multisegment_pruning_preserves_every_independent_exact_hit() {
    let directory = tempdir().expect("pruning Store directory");
    let number = ColumnId::new(1);
    let schema = Schema::new(vec![ColumnDefinition::new(
        number,
        "number",
        ColumnType::U64,
        false,
    )])
    .expect("pruning schema");
    let controller = Arc::new(MetadataTestController::new());
    controller
        .arm(MetadataTestArm::ObserveExecution { query_id: 3800 })
        .expect("arm pruning execution observation");
    let store = Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::default().with_schema(schema),
        StoreTestDependencies::new(Arc::new(StdVfs), Arc::new(SystemMonotonicClock))
            .with_metadata_test_controller(Arc::clone(&controller)),
    )
    .expect("open pruning Store");
    for rows in [
        [(1_u128, 1_u64, [1.0_f32, 0.0_f32]), (2, 2, [2.0, 0.0])],
        [(3_u128, 100_u64, [3.0_f32, 0.0_f32]), (4, 101, [4.0, 0.0])],
    ] {
        store
            .ingest(IngestBatch::new(
                rows.into_iter()
                    .map(|(document, value, vector)| {
                        IngestDocument::new(
                            DocumentVersion::new(DocId::new(document), Revision::new(1)),
                            vector.to_vec(),
                        )
                        .with_columns(vec![(number, PredicateValue::U64(value))])
                    })
                    .collect(),
            ))
            .expect("ingest pruning segment");
        store.seal().expect("seal pruning segment");
    }

    let snapshot = store.snapshot().expect("pruning snapshot");
    let mut sources = Vec::new();
    let mut rows = Vec::new();
    let mut live = BTreeSet::new();
    let mut unfiltered_exact = Vec::new();
    for segment in snapshot.segments() {
        let source = source_label(RowSource::Sealed(segment.meta().id));
        let range = match segment.meta().clustering_key_range {
            zeppelin_embed::segment::ClusteringKeyRange::Unstamped => {
                oracle::SourceRangeDto::Unstamped
            }
            zeppelin_embed::segment::ClusteringKeyRange::Empty => oracle::SourceRangeDto::Empty,
            zeppelin_embed::segment::ClusteringKeyRange::Bounded { min_ts, max_ts } => {
                oracle::SourceRangeDto::Bounded {
                    min: min_ts,
                    max: max_ts,
                }
            }
        };
        sources.push(oracle::I38SourceDto {
            source: source.clone(),
            sealed: true,
            range,
        });
        for local_row in 0..segment.meta().row_count {
            let document = segment
                .document_version(local_row as usize)
                .expect("read document identity")
                .expect("document region is present")
                .doc_id()
                .get();
            let value = match document {
                1 => 1,
                2 => 2,
                3 => 100,
                4 => 101,
                other => panic!("unexpected pruning document {other}"),
            };
            rows.push(oracle::SourceMetadataRowDto {
                source: source.clone(),
                row_id: local_row,
                document_id: document,
                cells: BTreeMap::from([(1, oracle::ScalarCell::U64(value))]),
            });
            live.insert((source.clone(), local_row));
            let coordinate = document as f32;
            unfiltered_exact.push(oracle::ExactHitDto {
                source: source.clone(),
                row_id: local_row,
                document_id: document,
                distance_bits: (coordinate * coordinate).to_bits(),
            });
        }
    }
    unfiltered_exact.sort_by_key(|hit| hit.document_id);
    drop(snapshot);

    let public_unfiltered = store
        .search(
            SearchRequest::new(&[0.0, 0.0]),
            4,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("public unpruned exact baseline")
        .candidates
        .iter()
        .map(|candidate| oracle::ExactHitDto {
            source: source_label(candidate.row_id().source()),
            row_id: candidate.row_id().local_row(),
            document_id: candidate
                .document()
                .expect("unpruned candidate identity")
                .doc_id()
                .get(),
            distance_bits: (-candidate.score()).to_bits(),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        public_unfiltered, unfiltered_exact,
        "public unpruned exact baseline diverged from independent squared-L2 fixture"
    );

    let predicate = Predicate::Eq {
        column: number,
        value: PredicateValue::U64(1),
    };
    let filtered = store
        .search_filtered(
            SearchRequest::new(&[0.0, 0.0]),
            &predicate,
            4,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("public exact filtered pruning query");
    let filtered_exact = filtered
        .candidates
        .iter()
        .map(|candidate| oracle::ExactHitDto {
            source: source_label(candidate.row_id().source()),
            row_id: candidate.row_id().local_row(),
            document_id: candidate
                .document()
                .expect("filtered candidate identity")
                .doc_id()
                .get(),
            distance_bits: (-candidate.score()).to_bits(),
        })
        .collect();
    let pruned_sources = filtered
        .plans
        .iter()
        .filter(|plan| plan.branch == SegmentBranch::Pruned)
        .map(|plan| source_label(plan.source))
        .collect();
    let execution_receipts = controller
        .drain_execution_receipts()
        .expect("drain pruning receipts")
        .iter()
        .map(execution_receipt)
        .collect();
    oracle::compare_i38(
        &oracle::I38Input {
            sources,
            rows,
            live,
            predicate: oracle::PredicateDto::Eq {
                column: 1,
                value: oracle::ScalarCell::U64(1),
            },
            unfiltered_exact,
            expected_delete_records: 0,
        },
        &oracle::I38Observed {
            filtered_exact,
            pruned_sources,
            reports: filtered
                .plans
                .iter()
                .map(|plan| branch_report(3800, plan))
                .collect(),
            execution_receipts,
            allow_list_threshold: 64,
            wal_delete_records: 0,
        },
    )
    .unwrap_or_else(|error| panic!("{error}"));
    controller
        .assert_no_unconsumed_arm()
        .expect("pruning observation was consumed");
    store.close().expect("close pruning Store");
}

#[test]
fn i39_public_graph_clean_fault_retry_uses_actual_traversal_and_fallback_work() {
    let directory = tempdir().expect("metadata graph directory");
    let id = publish_metadata_chain_graph(directory.path());
    let original_segment = std::fs::read(directory.path().join(id.file_name()))
        .expect("read original metadata graph artifact");
    let controller = Arc::new(MetadataTestController::new());
    let store = Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::default().with_epoch(metadata_graph_epoch()),
        StoreTestDependencies::new(Arc::new(StdVfs), Arc::new(SystemMonotonicClock))
            .with_metadata_test_controller(Arc::clone(&controller)),
    )
    .expect("open metadata graph Store");
    let query = vec![0.0_f32; GRAPH_DIMS];
    let predicate = Predicate::And(Vec::new());

    controller
        .arm(MetadataTestArm::ObserveExecution { query_id: 3910 })
        .expect("arm clean graph execution");
    let clean = store
        .search_filtered(
            SearchRequest::new(&query),
            &predicate,
            3,
            metadata_graph_options(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("clean filtered graph query");
    let clean_receipts = controller
        .drain_execution_receipts()
        .expect("drain clean graph receipt");
    attest_one_graph_outcome(3910, &clean, &clean_receipts);
    assert_eq!(clean.plans[0].branch, SegmentBranch::FilteredGraph);
    assert!(clean_receipts[0].graph_nodes_visited > 0);
    assert_eq!(clean_receipts[0].exact_fallback_rows_examined, 0);

    controller
        .arm(MetadataTestArm::VisitedBudgetFallback {
            query_id: 3911,
            budget: 1,
        })
        .expect("arm visited-budget guard");
    let fault = store
        .search_filtered(
            SearchRequest::new(&query),
            &predicate,
            3,
            metadata_graph_options(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("fault-leg graph query");
    let fault_receipts = controller
        .drain_execution_receipts()
        .expect("drain fault graph receipt");
    attest_one_graph_outcome(3911, &fault, &fault_receipts);
    let feature = controller
        .drain_feature_receipts()
        .expect("drain graph feature receipt");
    assert_eq!(
        feature.len(),
        1,
        "missing production feature receipt: visited-budget-fallback"
    );
    assert_eq!(fault.plans[0].branch, SegmentBranch::GraphExactFallback);
    assert_eq!(fault.plans[0].fallback, PlanFallback::VisitedBudget);
    assert!(fault_receipts[0].graph_nodes_visited > 0);
    assert!(fault_receipts[0].exact_fallback_rows_examined > 0);
    assert_eq!(graph_result_bits(&clean), graph_result_bits(&fault));
    assert!(matches!(
        feature[0].detail,
        MetadataFeatureDetail::VisitedBudgetFallback {
            visited,
            budget: 1,
            filter_cardinality,
            exact_rows_examined,
            returned: 3,
            reason: PlanFallback::VisitedBudget,
            ..
        } if visited > 1
            && filter_cardinality == GRAPH_ROWS as u64
            && exact_rows_examined == GRAPH_ROWS as u64
    ));
    assert_eq!(
        feature[0].origin.site(),
        "planner.exec.filtered-graph.visited-budget-fallback"
    );

    controller
        .arm(MetadataTestArm::ObserveExecution { query_id: 3912 })
        .expect("arm clean retry execution");
    let retry = store
        .search_filtered(
            SearchRequest::new(&query),
            &predicate,
            3,
            metadata_graph_options(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("clean graph retry");
    let retry_receipts = controller
        .drain_execution_receipts()
        .expect("drain retry graph receipt");
    attest_one_graph_outcome(3912, &retry, &retry_receipts);
    assert_eq!(graph_result_bits(&clean), graph_result_bits(&retry));
    assert_eq!(
        std::fs::read(directory.path().join(id.file_name()))
            .expect("read graph artifact after clean/fault/retry"),
        original_segment
    );
    controller
        .assert_no_unconsumed_arm()
        .expect("all graph arms consumed");
    store.close().expect("close metadata graph Store");
}
