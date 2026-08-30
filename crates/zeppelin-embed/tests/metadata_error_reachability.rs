#![allow(clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::sync::Arc;

use tempfile::{TempDir, tempdir};
use xxhash_rust::xxh3::xxh3_64;
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, IngestError, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::{
    CancelToken, OpenOptions, QueryControl, QueryError, SearchOptions, SearchTier, Store,
    StoreError, StoreTestDependencies, SystemMonotonicClock,
};
use zeppelin_embed::meta::{
    AliveSet, BuildError, ColumnDefinition, ColumnId, ColumnInput, ColumnStoreBuilder, ColumnType,
    ColumnValue, DictionaryError, EvalError, MetadataBuildTestLimits, Predicate, PredicateValue,
    RangePredicate, Schema, SchemaError, TIMESTAMP_COLUMN, evaluate,
};
use zeppelin_embed::planner::{
    FilteredSearchError, MetadataTestArm, MetadataTestController, PlanError, SegmentBranch,
};
use zeppelin_embed::quant::Bit4Factors;
use zeppelin_embed::segment::reader::SegmentReader;
use zeppelin_embed::segment::writer::{SegmentBuild, SegmentFactors, encode_segment};
use zeppelin_embed::segment::{MetadataDecodeProvenance, SegmentError, SegmentId};
use zeppelin_embed::vfs::StdVfs;

#[allow(dead_code)]
#[path = "../../../tests/adversarial-oracle/src/metadata_filter_planner.rs"]
mod oracle;

const REQUIRED: ColumnId = ColumnId::new(1);

fn required_schema() -> Schema {
    Schema::new(vec![ColumnDefinition::new(
        REQUIRED,
        "required",
        ColumnType::U64,
        false,
    )])
    .expect("required schema")
}

fn region_bounds(bytes: &[u8], entry: usize) -> (usize, usize) {
    let directory = 64 + entry * 32;
    let offset = u64::from_le_bytes(
        bytes[directory + 8..directory + 16]
            .try_into()
            .expect("region offset bytes"),
    ) as usize;
    let length = u64::from_le_bytes(
        bytes[directory + 16..directory + 24]
            .try_into()
            .expect("region length bytes"),
    ) as usize;
    (offset, length)
}

fn rewrite_header_and_file(bytes: &mut [u8]) {
    let header_length = u64::from_le_bytes(bytes[16..24].try_into().expect("header length"));
    let header_length = usize::try_from(header_length).expect("header length fits usize");
    let header_checksum = xxh3_64(&bytes[..header_length - 8]).to_le_bytes();
    bytes[header_length - 8..header_length].copy_from_slice(&header_checksum);
    let trailer = bytes.len() - 8;
    let file_checksum = xxh3_64(&bytes[..trailer]).to_le_bytes();
    bytes[trailer..].copy_from_slice(&file_checksum);
}

fn rewrite_region(bytes: &mut [u8], entry: usize) {
    let directory = 64 + entry * 32;
    let (offset, length) = region_bounds(bytes, entry);
    let checksum = xxh3_64(&bytes[offset..offset + length]).to_le_bytes();
    bytes[directory + 24..directory + 32].copy_from_slice(&checksum);
    rewrite_header_and_file(bytes);
}

fn write_and_open(path: &Path, bytes: &[u8], id: SegmentId) -> SegmentReader {
    std::fs::write(path, bytes).expect("write checked semantic mutation");
    SegmentReader::open(&StdVfs, path, id).expect("outer checksums remain valid")
}

fn semantic_metadata_segment() -> (Vec<u8>, SegmentId) {
    let schema = Schema::new(vec![
        ColumnDefinition::new(ColumnId::new(1), "flag", ColumnType::Bool, true),
        ColumnDefinition::new(ColumnId::new(2), "dict", ColumnType::DictionaryString, true),
        ColumnDefinition::new(ColumnId::new(3), "zz", ColumnType::RawString, true),
    ])
    .expect("semantic metadata schema");
    let mut builder = ColumnStoreBuilder::new(schema);
    for row in 0_usize..10 {
        let raw = format!("raw-{row}");
        builder
            .push_row(
                i64::try_from(row).expect("semantic timestamp fits i64"),
                &[
                    ColumnInput {
                        column: ColumnId::new(1),
                        value: ColumnValue::Bool(row.is_multiple_of(2)),
                    },
                    ColumnInput {
                        column: ColumnId::new(2),
                        value: ColumnValue::String("one"),
                    },
                    ColumnInput {
                        column: ColumnId::new(3),
                        value: ColumnValue::String(&raw),
                    },
                ],
            )
            .expect("semantic metadata row");
    }
    let columns = builder.finish().expect("semantic metadata columns");
    let alive = AliveSet::new(10);
    let factors = [Bit4Factors::from_persisted(1.0, 1.0, 0.0); 10];
    let id = SegmentId::new(0x4936_4937, [0x38; 10]);
    let bytes = encode_segment(SegmentBuild {
        id,
        scheme: 4,
        dims: 1,
        codes: &[0x80; 10],
        factors: SegmentFactors::Bit4(&factors),
        rescore: &[1.0; 10],
        columns: &columns,
        alive: &alive,
    })
    .expect("semantic metadata segment");
    (bytes, id)
}

fn timestamp_only_metadata_segment() -> (Vec<u8>, SegmentId) {
    let mut builder = ColumnStoreBuilder::new(Schema::timestamp_only());
    builder
        .push_row(7, &[])
        .expect("timestamp-only metadata row");
    let columns = builder.finish().expect("timestamp-only metadata columns");
    let alive = AliveSet::new(1);
    let factors = [Bit4Factors::from_persisted(1.0, 1.0, 0.0)];
    let id = SegmentId::new(0x4936_0001, [0x39; 10]);
    let bytes = encode_segment(SegmentBuild {
        id,
        scheme: 4,
        dims: 1,
        codes: &[0x80],
        factors: SegmentFactors::Bit4(&factors),
        rescore: &[1.0],
        columns: &columns,
        alive: &alive,
    })
    .expect("timestamp-only metadata segment");
    (bytes, id)
}

fn public_semantic_metadata_store() -> (TempDir, SegmentId, Vec<u8>) {
    let directory = tempdir().expect("public malformed metadata Store directory");
    let schema = Schema::new(vec![
        ColumnDefinition::new(ColumnId::new(1), "flag", ColumnType::Bool, true),
        ColumnDefinition::new(ColumnId::new(2), "dict", ColumnType::DictionaryString, true),
        ColumnDefinition::new(ColumnId::new(3), "zz", ColumnType::RawString, true),
    ])
    .expect("public malformed metadata schema");
    let store = Store::open(directory.path(), OpenOptions::default().with_schema(schema))
        .expect("open public malformed metadata Store");
    let documents = (0_u128..10)
        .map(|row| {
            IngestDocument::new(
                DocumentVersion::new(DocId::new(row + 1), Revision::new(1)),
                vec![row as f32],
            )
            .with_timestamp(i64::try_from(row).expect("public row timestamp fits i64"))
            .with_columns(vec![
                (
                    ColumnId::new(1),
                    PredicateValue::Bool(row.is_multiple_of(2)),
                ),
                (ColumnId::new(2), PredicateValue::String("one".to_owned())),
                (
                    ColumnId::new(3),
                    PredicateValue::String(format!("raw-{row}")),
                ),
            ])
        })
        .collect();
    store
        .ingest(IngestBatch::new(documents))
        .expect("ingest public malformed metadata rows");
    store.seal().expect("seal public malformed metadata Store");
    let segment_id = store
        .snapshot()
        .expect("snapshot public malformed metadata Store")
        .segments()
        .first()
        .expect("sealed public malformed metadata source")
        .meta()
        .id;
    store
        .close()
        .expect("close public malformed metadata Store");
    let bytes = std::fs::read(directory.path().join(segment_id.file_name()))
        .expect("read public malformed metadata source");
    (directory, segment_id, bytes)
}

#[test]
fn schema_errors_are_reached_through_schema_new() {
    assert_eq!(
        Schema::new(vec![ColumnDefinition::new(
            TIMESTAMP_COLUMN,
            "other",
            ColumnType::I64,
            false,
        )]),
        Err(SchemaError::ReservedTimestampId)
    );
    assert_eq!(
        Schema::new(vec![ColumnDefinition::new(
            ColumnId::new(1),
            "ts",
            ColumnType::I64,
            false,
        )]),
        Err(SchemaError::ReservedTimestampName)
    );
    assert_eq!(
        Schema::new(vec![
            ColumnDefinition::new(ColumnId::new(1), "a", ColumnType::U64, false),
            ColumnDefinition::new(ColumnId::new(1), "b", ColumnType::I64, false),
        ]),
        Err(SchemaError::DuplicateColumnId(ColumnId::new(1)))
    );
    assert_eq!(
        Schema::new(vec![
            ColumnDefinition::new(ColumnId::new(1), "same", ColumnType::U64, false),
            ColumnDefinition::new(ColumnId::new(2), "same", ColumnType::I64, false),
        ]),
        Err(SchemaError::DuplicateColumnName("same".to_owned()))
    );
}

#[test]
fn public_ingest_build_errors_leave_generation_wal_and_query_state_unchanged() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(
        directory.path(),
        OpenOptions::default().with_schema(required_schema()),
    )
    .expect("open typed store");
    let baseline_generation = store.snapshot().expect("snapshot").generation();
    let wal_path = directory.path().join("wal.ze");
    let baseline_wal = std::fs::read(&wal_path).expect("initial WAL");

    let cases = vec![
        (
            IngestDocument::new(
                DocumentVersion::new(DocId::new(1), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_columns(vec![(ColumnId::new(99), PredicateValue::U64(1))]),
            BuildError::UnknownColumn(ColumnId::new(99)),
        ),
        (
            IngestDocument::new(
                DocumentVersion::new(DocId::new(2), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_columns(vec![
                (REQUIRED, PredicateValue::U64(1)),
                (REQUIRED, PredicateValue::U64(2)),
            ]),
            BuildError::DuplicateColumn(REQUIRED),
        ),
        (
            IngestDocument::new(
                DocumentVersion::new(DocId::new(3), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_columns(Vec::new()),
            BuildError::MissingRequiredColumn(REQUIRED),
        ),
        (
            IngestDocument::new(
                DocumentVersion::new(DocId::new(4), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_columns(vec![(TIMESTAMP_COLUMN, PredicateValue::I64(7))]),
            BuildError::TimestampProvidedAsInput,
        ),
        (
            IngestDocument::new(
                DocumentVersion::new(DocId::new(5), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_columns(vec![(REQUIRED, PredicateValue::Bool(true))]),
            BuildError::TypeMismatch {
                column: REQUIRED,
                expected: ColumnType::U64,
                actual: ColumnType::Bool,
            },
        ),
    ];
    for (document, expected) in cases {
        let error = store
            .ingest(IngestBatch::new(vec![document]))
            .expect_err("invalid typed row must be refused");
        match error {
            IngestError::Columns(actual) => assert_eq!(actual, expected),
            actual => panic!("expected typed column refusal, observed {actual:?}"),
        }
        assert_eq!(
            store
                .snapshot()
                .expect("snapshot after refusal")
                .generation(),
            baseline_generation
        );
        assert_eq!(
            std::fs::read(&wal_path).expect("WAL after refusal"),
            baseline_wal
        );
        let visible = store
            .search_filtered(
                SearchRequest::new(&[0.0, 0.0]),
                &Predicate::And(Vec::new()),
                10,
                SearchOptions::default(),
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect("store remains queryable");
        assert!(visible.candidates.is_empty());
    }
    store.close().expect("close store");
}

#[test]
fn alive_out_of_range_is_typed_and_bitmap_atomic() {
    let mut alive = AliveSet::new(2);
    let before = alive.clone();
    let error = alive.tombstone(2).expect_err("row two is out of range");
    assert_eq!(error.document(), 2);
    assert_eq!(error.row_count(), 2);
    assert_eq!(alive, before);
}

#[test]
fn eval_errors_fire_through_public_evaluate_with_real_inputs() {
    let schema = Schema::new(vec![
        ColumnDefinition::new(ColumnId::new(1), "number", ColumnType::U64, false),
        ColumnDefinition::new(ColumnId::new(2), "flag", ColumnType::Bool, false),
    ])
    .expect("eval schema");
    let mut builder = ColumnStoreBuilder::new(schema);
    builder
        .push_row(
            7,
            &[
                ColumnInput {
                    column: ColumnId::new(1),
                    value: ColumnValue::U64(1),
                },
                ColumnInput {
                    column: ColumnId::new(2),
                    value: ColumnValue::Bool(true),
                },
            ],
        )
        .expect("eval row");
    let columns = builder.finish().expect("eval columns");
    let alive = AliveSet::new(1);

    assert_eq!(
        evaluate(
            &Predicate::Eq {
                column: ColumnId::new(99),
                value: PredicateValue::U64(1),
            },
            &columns,
            &alive,
        ),
        Err(EvalError::UnknownColumn(ColumnId::new(99)))
    );
    assert_eq!(
        evaluate(
            &Predicate::Eq {
                column: ColumnId::new(1),
                value: PredicateValue::Bool(true),
            },
            &columns,
            &alive,
        ),
        Err(EvalError::TypeMismatch {
            column: ColumnId::new(1),
            expected: ColumnType::U64,
            actual: ColumnType::Bool,
        })
    );
    assert_eq!(
        evaluate(
            &Predicate::Range(RangePredicate {
                column: ColumnId::new(2),
                lower: None,
                upper: None,
            }),
            &columns,
            &alive,
        ),
        Err(EvalError::RangeRequiresNumericColumn(ColumnId::new(2)))
    );
    assert_eq!(
        evaluate(&Predicate::And(Vec::new()), &columns, &AliveSet::new(2)),
        Err(EvalError::RowCountMismatch {
            columns: 1,
            alive: 2,
        })
    );
}

#[test]
fn public_filtered_search_maps_predicate_errors_without_mutation() {
    let directory = tempdir().expect("store directory");
    let schema = Schema::new(vec![ColumnDefinition::new(
        ColumnId::new(1),
        "flag",
        ColumnType::Bool,
        true,
    )])
    .expect("planner schema");
    let store = Store::open(directory.path(), OpenOptions::default().with_schema(schema))
        .expect("open planner store");
    let generation = store.snapshot().expect("snapshot").generation();
    let wal = std::fs::read(directory.path().join("wal.ze")).expect("WAL");
    let cases = [
        (
            Predicate::Eq {
                column: ColumnId::new(99),
                value: PredicateValue::U64(1),
            },
            PlanError::UnknownColumn(ColumnId::new(99)),
        ),
        (
            Predicate::Eq {
                column: ColumnId::new(1),
                value: PredicateValue::U64(1),
            },
            PlanError::TypeMismatch {
                column: ColumnId::new(1),
                expected: ColumnType::Bool,
                actual: ColumnType::U64,
            },
        ),
        (
            Predicate::Range(RangePredicate {
                column: ColumnId::new(1),
                lower: None,
                upper: None,
            }),
            PlanError::RangeRequiresNumericColumn(ColumnId::new(1)),
        ),
    ];
    for (predicate, expected) in cases {
        let error = store
            .search_filtered(
                SearchRequest::new(&[0.0, 0.0]),
                &predicate,
                10,
                SearchOptions::default(),
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect_err("invalid predicate must not produce a partial result");
        assert!(matches!(error, FilteredSearchError::Plan(actual) if actual == expected));
        assert_eq!(store.snapshot().expect("snapshot").generation(), generation);
        assert_eq!(
            std::fs::read(directory.path().join("wal.ze")).expect("WAL after query"),
            wal
        );
    }
    store.close().expect("close store");
}

fn open_public_executor_fixture(
    directory: &Path,
    controller: &Arc<MetadataTestController>,
) -> Store {
    let store = Store::open_with_test_dependencies(
        directory,
        OpenOptions::default(),
        StoreTestDependencies::new(Arc::new(StdVfs), Arc::new(SystemMonotonicClock))
            .with_metadata_test_controller(Arc::clone(controller)),
    )
    .expect("open public executor fixture");
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(1), Revision::new(1)),
            vec![1.0, 0.0],
        )]))
        .expect("ingest public executor fixture");
    store
}

#[test]
fn public_executor_rejects_a_planted_reported_branch_lie() {
    let directory = tempdir().expect("reported-branch directory");
    let controller = Arc::new(MetadataTestController::new());
    let store = open_public_executor_fixture(directory.path(), &controller);
    controller
        .arm(MetadataTestArm::PlanReportMismatch {
            query_id: 3901,
            reported: SegmentBranch::MaskedScan,
        })
        .expect("arm public reported-branch plant");
    let error = store
        .search_filtered(
            SearchRequest::new(&[0.0, 0.0]),
            &Predicate::And(Vec::new()),
            1,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect_err("public executor accepted a planted reported-branch lie");
    assert!(matches!(
        error,
        FilteredSearchError::PlanReportMismatch {
            reported: SegmentBranch::MaskedScan,
            executed: SegmentBranch::ExactAllowList,
        }
    ));
    assert_eq!(
        controller
            .drain_execution_receipts()
            .expect("drain completed executor receipt")
            .len(),
        1,
        "the exact allow-list loop completed before the report mismatch was detected"
    );
    store.close().expect("close reported-branch fixture");
}

#[test]
fn public_executor_maps_evaluator_row_count_mismatch_exactly() {
    let directory = tempdir().expect("row-count directory");
    let controller = Arc::new(MetadataTestController::new());
    let store = open_public_executor_fixture(directory.path(), &controller);
    let generation = store.snapshot().expect("row-count snapshot").generation();
    let wal = std::fs::read(directory.path().join("wal.ze")).expect("row-count WAL before query");
    controller
        .arm(MetadataTestArm::RowCountMismatch {
            query_id: 3902,
            columns: 1,
            alive: 2,
        })
        .expect("arm public evaluator row-count plant");
    let error = store
        .search_filtered(
            SearchRequest::new(&[0.0, 0.0]),
            &Predicate::And(Vec::new()),
            1,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect_err("public executor accepted mismatched evaluator row spaces");
    assert!(matches!(
        error,
        FilteredSearchError::Plan(PlanError::RowCountMismatch {
            columns: 1,
            alive: 2,
        })
    ));
    controller
        .assert_no_unconsumed_arm()
        .expect("real public evaluator consumed row-count plant");
    assert!(
        controller
            .drain_execution_receipts()
            .expect("drain pre-execution row-count receipts")
            .is_empty(),
        "row-count refusal must occur before an executor branch starts"
    );
    assert_eq!(
        store
            .snapshot()
            .expect("row-count snapshot after query")
            .generation(),
        generation
    );
    assert_eq!(
        std::fs::read(directory.path().join("wal.ze")).expect("row-count WAL after query"),
        wal
    );
    store.close().expect("close row-count fixture");
}

#[test]
fn builder_overflow_guards_are_typed_distinct_and_row_atomic() {
    let schema = Schema::new(vec![
        ColumnDefinition::new(
            ColumnId::new(1),
            "dictionary",
            ColumnType::DictionaryString,
            false,
        ),
        ColumnDefinition::new(ColumnId::new(2), "raw", ColumnType::RawString, false),
    ])
    .expect("overflow schema");

    let mut row_limited = ColumnStoreBuilder::new_with_test_limits(
        schema.clone(),
        MetadataBuildTestLimits::new(0, u64::MAX, u64::MAX),
    );
    assert_eq!(
        row_limited.push_row(
            1,
            &[
                ColumnInput {
                    column: ColumnId::new(1),
                    value: ColumnValue::String("dictionary"),
                },
                ColumnInput {
                    column: ColumnId::new(2),
                    value: ColumnValue::String("raw"),
                },
            ],
        ),
        Err(BuildError::TooManyRows)
    );
    assert_eq!(
        row_limited
            .finish()
            .expect("atomic empty builder")
            .row_count(),
        0
    );

    let mut dictionary_limited = ColumnStoreBuilder::new_with_test_limits(
        schema.clone(),
        MetadataBuildTestLimits::new(u32::MAX, 0, u64::MAX),
    );
    assert_eq!(
        dictionary_limited.push_row(
            2,
            &[
                ColumnInput {
                    column: ColumnId::new(1),
                    value: ColumnValue::String("first"),
                },
                ColumnInput {
                    column: ColumnId::new(2),
                    value: ColumnValue::String("raw"),
                },
            ],
        ),
        Err(BuildError::Dictionary(DictionaryError::CardinalityOverflow))
    );
    assert_eq!(
        dictionary_limited
            .finish()
            .expect("dictionary refusal is atomic")
            .row_count(),
        0
    );

    let mut string_limited = ColumnStoreBuilder::new_with_test_limits(
        schema,
        MetadataBuildTestLimits::new(u32::MAX, u64::MAX, 3),
    );
    assert_eq!(
        string_limited.push_row(
            3,
            &[
                ColumnInput {
                    column: ColumnId::new(1),
                    value: ColumnValue::String("ok"),
                },
                ColumnInput {
                    column: ColumnId::new(2),
                    value: ColumnValue::String("four"),
                },
            ],
        ),
        Err(BuildError::Dictionary(
            DictionaryError::StringStorageOverflow
        ))
    );
    assert_eq!(
        string_limited
            .finish()
            .expect("string refusal is atomic")
            .row_count(),
        0
    );
}

#[test]
fn checked_columns_and_alive_corruptions_reach_exact_semantic_decoders() {
    let directory = tempdir().expect("semantic mutation directory");
    let path = directory.path().join("semantic-metadata.zseg");
    let (valid, id) = semantic_metadata_segment();
    let clean = write_and_open(&path, &valid, id);
    assert_eq!(clean.columns().expect("clean Columns").row_count(), 10);
    assert_eq!(clean.alive().expect("clean Alive").row_count(), 10);

    let (columns_offset, columns_length) = region_bounds(&valid, 0);
    let parsed_columns =
        oracle::parse_columns(&valid[columns_offset..columns_offset + columns_length])
            .expect("independent clean Columns parser");
    let assert_columns = |mutated: Vec<u8>, expected: &str| {
        let error = write_and_open(&path, &mutated, id)
            .columns()
            .expect_err("checked Columns mutation must be refused");
        match error {
            SegmentError::Columns(detail) => assert!(
                detail.contains(expected),
                "expected Columns detail {expected:?}, observed {detail:?}"
            ),
            other => panic!("expected SegmentError::Columns, observed {other:?}"),
        }
    };

    let mut missing_timestamp = valid.clone();
    missing_timestamp[columns_offset + 4..columns_offset + 8].copy_from_slice(&0_u32.to_le_bytes());
    rewrite_region(&mut missing_timestamp, 0);
    assert_columns(missing_timestamp, "required timestamp column is absent");

    let boolean_definition = parsed_columns
        .definition_spans
        .get(&1)
        .expect("Boolean definition spans");
    let dictionary_definition = parsed_columns
        .definition_spans
        .get(&2)
        .expect("dictionary definition spans");
    let raw_definition = parsed_columns
        .definition_spans
        .get(&3)
        .expect("raw-string definition spans");
    let timestamp_definition = parsed_columns
        .definition_spans
        .get(&0)
        .expect("timestamp definition spans");

    let mut unknown_type = valid.clone();
    unknown_type[columns_offset + boolean_definition.kind.start
        ..columns_offset + boolean_definition.kind.end]
        .copy_from_slice(&99_u16.to_le_bytes());
    rewrite_region(&mut unknown_type, 0);
    assert_columns(unknown_type, "unknown column type 99");

    let mut invalid_nullable = valid.clone();
    invalid_nullable[columns_offset + boolean_definition.nullable.start
        ..columns_offset + boolean_definition.nullable.end]
        .copy_from_slice(&2_u16.to_le_bytes());
    rewrite_region(&mut invalid_nullable, 0);
    assert_columns(invalid_nullable, "invalid nullable flag 2");

    let mut invalid_name_utf8 = valid.clone();
    invalid_name_utf8[columns_offset + boolean_definition.name.payload.start] = 0xff;
    rewrite_region(&mut invalid_name_utf8, 0);
    assert_columns(invalid_name_utf8, "column name UTF-8");

    let mut noncanonical_timestamp = valid.clone();
    noncanonical_timestamp[columns_offset + timestamp_definition.name.payload.start
        ..columns_offset + timestamp_definition.name.payload.end]
        .copy_from_slice(b"xx");
    rewrite_region(&mut noncanonical_timestamp, 0);
    assert_columns(
        noncanonical_timestamp,
        "timestamp definition is not canonical",
    );

    let mut reserved_timestamp_id = valid.clone();
    reserved_timestamp_id
        [columns_offset + boolean_definition.id.start..columns_offset + boolean_definition.id.end]
        .copy_from_slice(&TIMESTAMP_COLUMN.get().to_le_bytes());
    rewrite_region(&mut reserved_timestamp_id, 0);
    assert_columns(reserved_timestamp_id, "column id 0 is reserved for ts");

    let mut reserved_timestamp_name = valid.clone();
    reserved_timestamp_name[columns_offset + raw_definition.name.payload.start
        ..columns_offset + raw_definition.name.payload.end]
        .copy_from_slice(b"ts");
    rewrite_region(&mut reserved_timestamp_name, 0);
    assert_columns(reserved_timestamp_name, "column name ts is reserved");

    let mut duplicate_id = valid.clone();
    duplicate_id[columns_offset + dictionary_definition.id.start
        ..columns_offset + dictionary_definition.id.end]
        .copy_from_slice(&1_u32.to_le_bytes());
    rewrite_region(&mut duplicate_id, 0);
    assert_columns(duplicate_id, "duplicate column id 1");

    let mut duplicate_name = valid.clone();
    duplicate_name[columns_offset + dictionary_definition.name.payload.start
        ..columns_offset + dictionary_definition.name.payload.end]
        .copy_from_slice(b"flag");
    rewrite_region(&mut duplicate_name, 0);
    assert_columns(duplicate_name, "duplicate column name flag");

    let boolean_presence = parsed_columns
        .presence_spans
        .get(&1)
        .expect("Boolean presence spans");
    let mut invalid_presence_length = valid.clone();
    invalid_presence_length[columns_offset + boolean_presence.length.start
        ..columns_offset + boolean_presence.length.end]
        .copy_from_slice(&1_u32.to_le_bytes());
    rewrite_region(&mut invalid_presence_length, 0);
    assert_columns(invalid_presence_length, "presence length 1, expected 2");

    let dictionary_spans = parsed_columns
        .dictionary_spans
        .get(&2)
        .expect("dictionary spans");
    let mut invalid_dictionary_width = valid.clone();
    invalid_dictionary_width[columns_offset + dictionary_spans.width.start
        ..columns_offset + dictionary_spans.width.end]
        .copy_from_slice(&3_u16.to_le_bytes());
    rewrite_region(&mut invalid_dictionary_width, 0);
    assert_columns(invalid_dictionary_width, "invalid dictionary width 3");

    let mut invalid_dictionary_reserved = valid.clone();
    invalid_dictionary_reserved[columns_offset + dictionary_spans.reserved.start
        ..columns_offset + dictionary_spans.reserved.end]
        .copy_from_slice(&1_u16.to_le_bytes());
    rewrite_region(&mut invalid_dictionary_reserved, 0);
    assert_columns(
        invalid_dictionary_reserved,
        "dictionary reserved field is non-zero",
    );

    let dictionary_entry = dictionary_spans
        .entries
        .first()
        .expect("dictionary entry spans");
    let remaining_dictionary_bytes = columns_length - dictionary_spans.count.end;
    let impossible_dictionary_cardinality = u32::try_from(remaining_dictionary_bytes / 4 + 1)
        .expect("small checked Columns region cardinality plant fits u32");
    let mut dictionary_cardinality_overflow = valid.clone();
    dictionary_cardinality_overflow[columns_offset + dictionary_spans.count.start
        ..columns_offset + dictionary_spans.count.end]
        .copy_from_slice(&impossible_dictionary_cardinality.to_le_bytes());
    rewrite_region(&mut dictionary_cardinality_overflow, 0);
    assert_columns(
        dictionary_cardinality_overflow,
        "dictionary cardinality exceeds remaining entry capacity",
    );

    let mut invalid_dictionary_utf8 = valid.clone();
    invalid_dictionary_utf8[columns_offset + dictionary_entry.payload.start] = 0xff;
    rewrite_region(&mut invalid_dictionary_utf8, 0);
    assert_columns(invalid_dictionary_utf8, "columns UTF-8");

    let mut truncated_dictionary_payload = valid.clone();
    truncated_dictionary_payload[columns_offset + dictionary_entry.length.start
        ..columns_offset + dictionary_entry.length.end]
        .copy_from_slice(&u32::MAX.to_le_bytes());
    rewrite_region(&mut truncated_dictionary_payload, 0);
    assert_columns(
        truncated_dictionary_payload,
        "dictionary string storage exceeds remaining Columns bytes",
    );

    let mut invalid_bool = valid.clone();
    let boolean = parsed_columns
        .cells
        .get(&(0, 1))
        .expect("Boolean cell span")
        .payload_span;
    invalid_bool[columns_offset + boolean.start] = 2;
    rewrite_region(&mut invalid_bool, 0);
    assert_columns(invalid_bool, "invalid Boolean byte 2");

    let mut invalid_dictionary_code = valid.clone();
    let dictionary_code = parsed_columns
        .cells
        .get(&(0, 2))
        .expect("dictionary code span")
        .payload_span;
    invalid_dictionary_code
        [columns_offset + dictionary_code.start..columns_offset + dictionary_code.end]
        .copy_from_slice(&1_u16.to_le_bytes());
    rewrite_region(&mut invalid_dictionary_code, 0);
    let dictionary_error = write_and_open(&path, &invalid_dictionary_code, id)
        .columns()
        .expect_err("dictionary-code guard must refuse the checked mutation");
    assert!(matches!(
        dictionary_error,
        SegmentError::MetadataSemantic {
            ref detail,
            provenance: MetadataDecodeProvenance::ColumnsDictionaryCode {
                column_id: 2,
                row: 0,
                byte_offset,
                code: 1,
                dictionary_cardinality: 1,
            },
        } if detail == "dictionary code 1 out of range"
            && byte_offset == u64::try_from(dictionary_code.start).expect("dictionary offset")
    ));

    let raw_payload = parsed_columns
        .cells
        .get(&(0, 3))
        .expect("raw-string payload span")
        .payload_span;
    let mut invalid_raw_utf8 = valid.clone();
    invalid_raw_utf8[columns_offset + raw_payload.start] = 0xff;
    rewrite_region(&mut invalid_raw_utf8, 0);
    assert_columns(invalid_raw_utf8, "columns UTF-8");

    let mut invalid_raw_length = valid.clone();
    let raw_length = parsed_columns
        .cells
        .get(&(0, 3))
        .and_then(|cell| cell.length_span)
        .expect("raw-string length span");
    invalid_raw_length[columns_offset + raw_length.start..columns_offset + raw_length.end]
        .copy_from_slice(&u32::MAX.to_le_bytes());
    rewrite_region(&mut invalid_raw_length, 0);
    let raw_error = write_and_open(&path, &invalid_raw_length, id)
        .columns()
        .expect_err("raw-string length guard must refuse the checked mutation");
    assert!(matches!(
        raw_error,
        SegmentError::MetadataSemantic {
            ref detail,
            provenance: MetadataDecodeProvenance::ColumnsRawStringLength {
                column_id: 3,
                row: 0,
                byte_offset,
                declared_bytes: u32::MAX,
                available_bytes,
            },
        } if detail.contains("columns truncated")
            && byte_offset == u64::try_from(raw_length.start).expect("raw length offset")
            && available_bytes
                == u64::try_from(columns_length - raw_length.end).expect("available raw bytes")
    ));

    let mut trailing_columns = valid.clone();
    let columns_directory = 64;
    trailing_columns[columns_directory + 16..columns_directory + 24].copy_from_slice(
        &u64::try_from(columns_length + 1)
            .expect("long Columns length")
            .to_le_bytes(),
    );
    rewrite_region(&mut trailing_columns, 0);
    assert_columns(trailing_columns, "columns has 1 trailing bytes");

    let mut invalid_presence_tail = valid.clone();
    let presence = parsed_columns
        .presence_spans
        .get(&1)
        .expect("Boolean presence span")
        .bitmap;
    invalid_presence_tail[columns_offset + presence.end - 1] |= 0x80;
    rewrite_region(&mut invalid_presence_tail, 0);
    let presence_error = write_and_open(&path, &invalid_presence_tail, id)
        .columns()
        .expect_err("presence-tail guard must refuse the checked mutation");
    assert!(matches!(
        presence_error,
        SegmentError::MetadataSemantic {
            ref detail,
            provenance: MetadataDecodeProvenance::ColumnsPresenceTail {
                column_id: 1,
                row_count: 10,
                byte_offset,
                observed_byte: 0x83,
                allowed_mask: 0x03,
            },
        } if detail == "non-zero presence tail padding"
            && byte_offset == u64::try_from(presence.end - 1).expect("presence-tail offset")
    ));

    let (alive_offset, alive_length) = region_bounds(&valid, 1);
    let parsed_alive = oracle::parse_alive(&valid[alive_offset..alive_offset + alive_length])
        .expect("independent clean Alive parser");
    assert_eq!(
        parsed_alive.bitmap_span,
        oracle::ByteSpan { start: 8, end: 10 }
    );
    let assert_alive = |mutated: Vec<u8>, expected: &str| {
        let error = write_and_open(&path, &mutated, id)
            .alive()
            .expect_err("checked Alive mutation must be refused");
        match error {
            SegmentError::Alive(detail) => assert!(
                detail.contains(expected),
                "expected Alive detail {expected:?}, observed {detail:?}"
            ),
            other => panic!("expected SegmentError::Alive, observed {other:?}"),
        }
    };

    let mut truncated_alive = valid.clone();
    let alive_directory = 64 + 32;
    truncated_alive[alive_directory + 16..alive_directory + 24].copy_from_slice(
        &u64::try_from(alive_length - 1)
            .expect("short length")
            .to_le_bytes(),
    );
    truncated_alive[alive_offset + alive_length - 1] = 0;
    rewrite_region(&mut truncated_alive, 1);
    let alive_error = write_and_open(&path, &truncated_alive, id)
        .alive()
        .expect_err("one-byte Alive truncation must reach the bitmap guard");
    assert!(matches!(
        alive_error,
        SegmentError::MetadataSemantic {
            ref detail,
            provenance: MetadataDecodeProvenance::AliveBitmapTruncation {
                row_count: 10,
                byte_offset: 8,
                declared_bytes: 2,
                observed_bytes: 1,
            },
        } if detail == "alive truncated at 8, need 2, total 9"
    ));

    let mut invalid_alive_tail = valid.clone();
    invalid_alive_tail[alive_offset + parsed_alive.bitmap_span.end - 1] |= 0x80;
    rewrite_region(&mut invalid_alive_tail, 1);
    assert_alive(invalid_alive_tail, "non-zero bitmap tail padding");

    let mut trailing_alive = valid.clone();
    trailing_alive[alive_directory + 16..alive_directory + 24].copy_from_slice(
        &u64::try_from(alive_length + 1)
            .expect("long length")
            .to_le_bytes(),
    );
    rewrite_region(&mut trailing_alive, 1);
    assert_alive(trailing_alive, "alive has 1 trailing bytes");
}

#[test]
fn checked_columns_fixed_header_and_numeric_truncations_reach_the_decoder() {
    let directory = tempdir().expect("numeric truncation directory");
    let path = directory.path().join("timestamp-only.zseg");
    let (valid, id) = timestamp_only_metadata_segment();
    let (columns_offset, columns_length) = region_bounds(&valid, 0);

    for (retained, expected) in [
        (3_usize, "columns truncated at 0, need 4"),
        (columns_length.saturating_sub(1), "columns truncated"),
    ] {
        let mut mutated = valid.clone();
        mutated[columns_offset + retained..columns_offset + columns_length].fill(0);
        mutated[64 + 16..64 + 24].copy_from_slice(
            &u64::try_from(retained)
                .expect("retained Columns length")
                .to_le_bytes(),
        );
        rewrite_region(&mut mutated, 0);
        let error = write_and_open(&path, &mutated, id)
            .columns()
            .expect_err("truncated Columns bytes must be refused");
        assert!(
            matches!(error, SegmentError::Columns(ref detail) if detail.contains(expected)),
            "expected {expected:?}, observed {error:?}"
        );
    }
}

#[test]
fn checked_malformed_catalog_covers_remaining_row_and_length_guards() {
    enum RegionLoad {
        Columns,
        Alive,
    }

    enum ExpectedRefusal {
        Columns(&'static str),
        Alive(&'static str),
        Geometry(&'static str),
    }

    let directory = tempdir().expect("remaining catalog directory");
    let path = directory.path().join("remaining-catalog.zseg");
    let (valid, id) = semantic_metadata_segment();
    let (columns_offset, columns_length) = region_bounds(&valid, 0);
    let (alive_offset, alive_length) = region_bounds(&valid, 1);
    let parsed_columns =
        oracle::parse_columns(&valid[columns_offset..columns_offset + columns_length])
            .expect("independent clean Columns spans");
    let parsed_alive = oracle::parse_alive(&valid[alive_offset..alive_offset + alive_length])
        .expect("independent clean Alive spans");

    let with_region_length = |mut bytes: Vec<u8>, entry: usize, length: usize| {
        let directory_offset = 64 + entry * 32;
        bytes[directory_offset + 16..directory_offset + 24].copy_from_slice(
            &u64::try_from(length)
                .expect("catalog region length fits u64")
                .to_le_bytes(),
        );
        rewrite_region(&mut bytes, entry);
        bytes
    };

    let definition_length = parsed_columns
        .definition_spans
        .get(&1)
        .expect("Boolean definition spans")
        .nullable
        .start
        + 1;
    let dictionary_length = parsed_columns
        .dictionary_spans
        .get(&2)
        .and_then(|spans| spans.entries.first())
        .expect("dictionary entry spans")
        .length
        .start
        + 2;
    let raw_length = parsed_columns
        .cells
        .get(&(0, 3))
        .and_then(|cell| cell.length_span)
        .expect("raw-string length spans")
        .start
        + 2;

    let mut columns_row_mismatch = valid.clone();
    columns_row_mismatch[48..52].copy_from_slice(&9_u32.to_le_bytes());
    rewrite_header_and_file(&mut columns_row_mismatch);

    let alive_header = with_region_length(valid.clone(), 1, 7);

    let mut alive_declared_length = valid.clone();
    alive_declared_length[alive_offset + 4..alive_offset + 8].copy_from_slice(&1_u32.to_le_bytes());
    rewrite_region(&mut alive_declared_length, 1);

    let mut alive_row_mismatch = valid.clone();
    alive_row_mismatch[alive_offset..alive_offset + 4].copy_from_slice(&9_u32.to_le_bytes());
    alive_row_mismatch[alive_offset + parsed_alive.bitmap_span.end - 1] &= 0x01;
    rewrite_region(&mut alive_row_mismatch, 1);

    let cases = vec![
        (
            "columns-definition-truncation",
            with_region_length(valid.clone(), 0, definition_length),
            RegionLoad::Columns,
            ExpectedRefusal::Columns("columns truncated"),
        ),
        (
            "dictionary-length-truncation",
            with_region_length(valid.clone(), 0, dictionary_length),
            RegionLoad::Columns,
            ExpectedRefusal::Columns("columns truncated"),
        ),
        (
            "raw-string-length-truncation",
            with_region_length(valid.clone(), 0, raw_length),
            RegionLoad::Columns,
            ExpectedRefusal::Columns("columns truncated"),
        ),
        (
            "columns-row-count-mismatch",
            columns_row_mismatch,
            RegionLoad::Columns,
            ExpectedRefusal::Geometry("column rows 10, header rows 9"),
        ),
        (
            "alive-header-truncation",
            alive_header,
            RegionLoad::Alive,
            ExpectedRefusal::Alive("alive truncated at 4, need 4, total 7"),
        ),
        (
            "alive-declared-length-mismatch",
            alive_declared_length,
            RegionLoad::Alive,
            ExpectedRefusal::Alive("bitmap length 1, expected 2"),
        ),
        (
            "alive-row-count-mismatch",
            alive_row_mismatch,
            RegionLoad::Alive,
            ExpectedRefusal::Geometry("alive rows 9, header rows 10"),
        ),
    ];

    let mut covered = Vec::new();
    for (label, bytes, load, expected) in cases {
        let reader = write_and_open(&path, &bytes, id);
        let error = match load {
            RegionLoad::Columns => reader.columns().expect_err(label),
            RegionLoad::Alive => reader.alive().expect_err(label),
        };
        match (error, expected) {
            (SegmentError::Columns(detail), ExpectedRefusal::Columns(expected))
            | (SegmentError::Alive(detail), ExpectedRefusal::Alive(expected))
            | (SegmentError::Geometry(detail), ExpectedRefusal::Geometry(expected)) => assert!(
                detail.contains(expected),
                "{label}: expected {expected:?}, observed {detail:?}"
            ),
            (error, _) => panic!("{label}: wrong typed refusal {error:?}"),
        }
        covered.push(label);
    }
    assert_eq!(
        covered,
        [
            "columns-definition-truncation",
            "dictionary-length-truncation",
            "raw-string-length-truncation",
            "columns-row-count-mismatch",
            "alive-header-truncation",
            "alive-declared-length-mismatch",
            "alive-row-count-mismatch",
        ]
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MetadataRefusalStage {
    Open,
    QueryColumns,
    QueryAlive,
}

#[derive(Clone, Copy)]
enum PublicMetadataRefusal {
    Columns(&'static str),
    Alive(&'static str),
    Geometry(&'static str),
}

fn assert_public_metadata_refusal(
    directory: &Path,
    expected_stage: MetadataRefusalStage,
    expected: PublicMetadataRefusal,
) {
    let store = match Store::open(directory, OpenOptions::default()) {
        Ok(store) => store,
        Err(error) => {
            assert_eq!(
                expected_stage,
                MetadataRefusalStage::Open,
                "malformed metadata refused at the wrong stage: {error:?}"
            );
            match (error, expected) {
                (
                    StoreError::Segment(SegmentError::Columns(detail)),
                    PublicMetadataRefusal::Columns(anchor),
                )
                | (
                    StoreError::Segment(SegmentError::Alive(detail)),
                    PublicMetadataRefusal::Alive(anchor),
                )
                | (
                    StoreError::Segment(SegmentError::Geometry(detail)),
                    PublicMetadataRefusal::Geometry(anchor),
                ) => assert!(
                    detail.contains(anchor),
                    "open refusal expected {anchor:?}, observed {detail:?}"
                ),
                (error, _) => panic!("wrong typed public open refusal: {error:?}"),
            }
            return;
        }
    };
    let error = store
        .search_filtered(
            SearchRequest::new(&[0.0]),
            &Predicate::And(Vec::new()),
            10,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect_err("malformed installed metadata must not return a partial result");
    let actual = match &error {
        FilteredSearchError::Query(QueryError::Store(StoreError::Segment(
            SegmentError::Columns(_),
        )))
        | FilteredSearchError::Query(QueryError::Store(StoreError::Segment(
            SegmentError::MetadataSemantic {
                provenance:
                    MetadataDecodeProvenance::ColumnsPresenceTail { .. }
                    | MetadataDecodeProvenance::ColumnsDictionaryCode { .. }
                    | MetadataDecodeProvenance::ColumnsRawStringLength { .. },
                ..
            },
        ))) => MetadataRefusalStage::QueryColumns,
        FilteredSearchError::Query(QueryError::Store(StoreError::Segment(
            SegmentError::Alive(_),
        )))
        | FilteredSearchError::Query(QueryError::Store(StoreError::Segment(
            SegmentError::MetadataSemantic {
                provenance: MetadataDecodeProvenance::AliveBitmapTruncation { .. },
                ..
            },
        ))) => MetadataRefusalStage::QueryAlive,
        FilteredSearchError::Query(QueryError::Store(StoreError::Segment(
            SegmentError::Geometry(_),
        ))) => expected_stage,
        other => panic!("wrong typed public query refusal: {other:?}"),
    };
    assert_eq!(
        actual, expected_stage,
        "typed refusal-stage ledger mismatch"
    );
    match (error, expected) {
        (
            FilteredSearchError::Query(QueryError::Store(StoreError::Segment(
                SegmentError::Columns(detail),
            ))),
            PublicMetadataRefusal::Columns(anchor),
        )
        | (
            FilteredSearchError::Query(QueryError::Store(StoreError::Segment(
                SegmentError::Alive(detail),
            ))),
            PublicMetadataRefusal::Alive(anchor),
        )
        | (
            FilteredSearchError::Query(QueryError::Store(StoreError::Segment(
                SegmentError::Geometry(detail),
            ))),
            PublicMetadataRefusal::Geometry(anchor),
        ) => assert!(
            detail.contains(anchor),
            "query refusal expected {anchor:?}, observed {detail:?}"
        ),
        (
            FilteredSearchError::Query(QueryError::Store(StoreError::Segment(
                SegmentError::MetadataSemantic { detail, .. },
            ))),
            PublicMetadataRefusal::Columns(anchor) | PublicMetadataRefusal::Alive(anchor),
        ) => assert!(
            detail.contains(anchor),
            "semantic query refusal expected {anchor:?}, observed {detail:?}"
        ),
        (error, _) => panic!("wrong exact public metadata refusal: {error:?}"),
    }
    store
        .close()
        .expect("close Store after exact metadata refusal");
}

#[test]
fn every_installable_malformed_columns_and_alive_case_reaches_public_store() {
    let (directory, segment_id, clean) = public_semantic_metadata_store();
    let path = directory.path().join(segment_id.file_name());
    let (columns_offset, columns_length) = region_bounds(&clean, 0);
    let parsed = oracle::parse_columns(&clean[columns_offset..columns_offset + columns_length])
        .expect("parse public clean Columns spans");
    let boolean = parsed
        .definition_spans
        .get(&1)
        .expect("public Boolean definition");
    let timestamp = parsed
        .definition_spans
        .get(&0)
        .expect("public timestamp definition");
    let dictionary = parsed
        .definition_spans
        .get(&2)
        .expect("public dictionary definition");
    let raw = parsed
        .definition_spans
        .get(&3)
        .expect("public raw-string definition");
    let boolean_presence = parsed
        .presence_spans
        .get(&1)
        .expect("public Boolean presence");
    let dictionary_spans = parsed
        .dictionary_spans
        .get(&2)
        .expect("public dictionary spans");
    let dictionary_entry = dictionary_spans
        .entries
        .first()
        .expect("public dictionary entry");
    let boolean_cell = parsed
        .cells
        .get(&(0, 1))
        .expect("public Boolean cell")
        .payload_span;
    let dictionary_code = parsed
        .cells
        .get(&(0, 2))
        .expect("public dictionary code")
        .payload_span;
    let raw_cell = parsed.cells.get(&(0, 3)).expect("public raw-string cell");
    let raw_length = raw_cell.length_span.expect("public raw-string length");
    let raw_payload = raw_cell.payload_span;
    let (alive_offset, alive_length) = region_bounds(&clean, 1);
    let alive = oracle::parse_alive(&clean[alive_offset..alive_offset + alive_length])
        .expect("parse public clean Alive spans");
    let set_region_length = |bytes: &mut Vec<u8>, entry: usize, length: usize| {
        let directory_offset = 64 + entry * 32;
        bytes[directory_offset + 16..directory_offset + 24].copy_from_slice(
            &u64::try_from(length)
                .expect("public region length fits u64")
                .to_le_bytes(),
        );
        rewrite_region(bytes, entry);
    };

    let mut cases = Vec::new();
    let mut unknown_type = clean.clone();
    unknown_type[columns_offset + boolean.kind.start..columns_offset + boolean.kind.end]
        .copy_from_slice(&99_u16.to_le_bytes());
    rewrite_region(&mut unknown_type, 0);
    cases.push((
        "columns-unknown-type",
        unknown_type,
        MetadataRefusalStage::QueryColumns,
        PublicMetadataRefusal::Columns("unknown column type 99"),
    ));

    let mut missing_timestamp = clean.clone();
    missing_timestamp[columns_offset + 4..columns_offset + 8].copy_from_slice(&0_u32.to_le_bytes());
    rewrite_region(&mut missing_timestamp, 0);
    cases.push((
        "columns-missing-timestamp",
        missing_timestamp,
        MetadataRefusalStage::QueryColumns,
        PublicMetadataRefusal::Columns("required timestamp column is absent"),
    ));

    let mut invalid_nullable = clean.clone();
    invalid_nullable
        [columns_offset + boolean.nullable.start..columns_offset + boolean.nullable.end]
        .copy_from_slice(&2_u16.to_le_bytes());
    rewrite_region(&mut invalid_nullable, 0);
    cases.push((
        "columns-invalid-nullable",
        invalid_nullable,
        MetadataRefusalStage::QueryColumns,
        PublicMetadataRefusal::Columns("invalid nullable flag 2"),
    ));

    let mut invalid_name_utf8 = clean.clone();
    invalid_name_utf8[columns_offset + boolean.name.payload.start] = 0xff;
    rewrite_region(&mut invalid_name_utf8, 0);
    cases.push((
        "columns-invalid-name-utf8",
        invalid_name_utf8,
        MetadataRefusalStage::QueryColumns,
        PublicMetadataRefusal::Columns("column name UTF-8"),
    ));

    let mut noncanonical_timestamp = clean.clone();
    noncanonical_timestamp[columns_offset + timestamp.name.payload.start
        ..columns_offset + timestamp.name.payload.end]
        .copy_from_slice(b"xx");
    rewrite_region(&mut noncanonical_timestamp, 0);
    cases.push((
        "columns-noncanonical-timestamp",
        noncanonical_timestamp,
        MetadataRefusalStage::QueryColumns,
        PublicMetadataRefusal::Columns("timestamp definition is not canonical"),
    ));

    let mut reserved_timestamp_id = clean.clone();
    reserved_timestamp_id[columns_offset + boolean.id.start..columns_offset + boolean.id.end]
        .copy_from_slice(&TIMESTAMP_COLUMN.get().to_le_bytes());
    rewrite_region(&mut reserved_timestamp_id, 0);
    cases.push((
        "columns-reserved-timestamp-id",
        reserved_timestamp_id,
        MetadataRefusalStage::QueryColumns,
        PublicMetadataRefusal::Columns("column id 0 is reserved for ts"),
    ));

    let mut reserved_timestamp_name = clean.clone();
    reserved_timestamp_name
        [columns_offset + raw.name.payload.start..columns_offset + raw.name.payload.end]
        .copy_from_slice(b"ts");
    rewrite_region(&mut reserved_timestamp_name, 0);
    cases.push((
        "columns-reserved-timestamp-name",
        reserved_timestamp_name,
        MetadataRefusalStage::QueryColumns,
        PublicMetadataRefusal::Columns("column name ts is reserved"),
    ));

    let mut duplicate_id = clean.clone();
    duplicate_id[columns_offset + dictionary.id.start..columns_offset + dictionary.id.end]
        .copy_from_slice(&1_u32.to_le_bytes());
    rewrite_region(&mut duplicate_id, 0);
    cases.push((
        "columns-duplicate-id",
        duplicate_id,
        MetadataRefusalStage::QueryColumns,
        PublicMetadataRefusal::Columns("duplicate column id 1"),
    ));

    let mut duplicate_name = clean.clone();
    duplicate_name[columns_offset + dictionary.name.payload.start
        ..columns_offset + dictionary.name.payload.end]
        .copy_from_slice(b"flag");
    rewrite_region(&mut duplicate_name, 0);
    cases.push((
        "columns-duplicate-name",
        duplicate_name,
        MetadataRefusalStage::QueryColumns,
        PublicMetadataRefusal::Columns("duplicate column name flag"),
    ));

    let mut presence_length = clean.clone();
    presence_length[columns_offset + boolean_presence.length.start
        ..columns_offset + boolean_presence.length.end]
        .copy_from_slice(&1_u32.to_le_bytes());
    rewrite_region(&mut presence_length, 0);
    cases.push((
        "columns-presence-length",
        presence_length,
        MetadataRefusalStage::QueryColumns,
        PublicMetadataRefusal::Columns("presence length 1, expected 2"),
    ));

    let mut presence_tail = clean.clone();
    presence_tail[columns_offset + boolean_presence.bitmap.end - 1] |= 0x80;
    rewrite_region(&mut presence_tail, 0);
    cases.push((
        "columns-presence-tail",
        presence_tail,
        MetadataRefusalStage::QueryColumns,
        PublicMetadataRefusal::Columns("non-zero presence tail padding"),
    ));

    let mut invalid_bool = clean.clone();
    invalid_bool[columns_offset + boolean_cell.start] = 2;
    rewrite_region(&mut invalid_bool, 0);
    cases.push((
        "columns-invalid-bool",
        invalid_bool,
        MetadataRefusalStage::QueryColumns,
        PublicMetadataRefusal::Columns("invalid Boolean byte 2"),
    ));

    let mut columns_header_truncation = clean.clone();
    set_region_length(&mut columns_header_truncation, 0, 3);
    cases.push((
        "columns-header-truncation",
        columns_header_truncation,
        MetadataRefusalStage::QueryColumns,
        PublicMetadataRefusal::Columns("columns truncated at 0, need 4"),
    ));

    let mut definition_truncation = clean.clone();
    set_region_length(&mut definition_truncation, 0, boolean.nullable.start + 1);
    cases.push((
        "columns-definition-truncation",
        definition_truncation,
        MetadataRefusalStage::QueryColumns,
        PublicMetadataRefusal::Columns("columns truncated"),
    ));

    let mut dictionary_length_truncation = clean.clone();
    set_region_length(
        &mut dictionary_length_truncation,
        0,
        dictionary_entry.length.start + 2,
    );
    cases.push((
        "columns-dictionary-length-truncation",
        dictionary_length_truncation,
        MetadataRefusalStage::QueryColumns,
        PublicMetadataRefusal::Columns("columns truncated"),
    ));

    let mut dictionary_payload_truncation = clean.clone();
    set_region_length(
        &mut dictionary_payload_truncation,
        0,
        dictionary_entry.payload.end - 1,
    );
    cases.push((
        "columns-dictionary-payload-truncation",
        dictionary_payload_truncation,
        MetadataRefusalStage::QueryColumns,
        PublicMetadataRefusal::Columns("columns truncated"),
    ));

    let mut dictionary_utf8 = clean.clone();
    dictionary_utf8[columns_offset + dictionary_entry.payload.start] = 0xff;
    rewrite_region(&mut dictionary_utf8, 0);
    cases.push((
        "columns-dictionary-invalid-utf8",
        dictionary_utf8,
        MetadataRefusalStage::QueryColumns,
        PublicMetadataRefusal::Columns("columns UTF-8"),
    ));

    let remaining_dictionary_bytes = columns_length - dictionary_spans.count.end;
    let impossible_dictionary_cardinality = u32::try_from(remaining_dictionary_bytes / 4 + 1)
        .expect("small public Columns region cardinality plant fits u32");
    let mut dictionary_cardinality_overflow = clean.clone();
    dictionary_cardinality_overflow[columns_offset + dictionary_spans.count.start
        ..columns_offset + dictionary_spans.count.end]
        .copy_from_slice(&impossible_dictionary_cardinality.to_le_bytes());
    rewrite_region(&mut dictionary_cardinality_overflow, 0);
    cases.push((
        "columns-dictionary-cardinality-overflow",
        dictionary_cardinality_overflow,
        MetadataRefusalStage::QueryColumns,
        PublicMetadataRefusal::Columns("dictionary cardinality exceeds remaining entry capacity"),
    ));

    let mut dictionary_string_storage_overflow = clean.clone();
    dictionary_string_storage_overflow[columns_offset + dictionary_entry.length.start
        ..columns_offset + dictionary_entry.length.end]
        .copy_from_slice(&u32::MAX.to_le_bytes());
    rewrite_region(&mut dictionary_string_storage_overflow, 0);
    cases.push((
        "columns-dictionary-string-storage-overflow",
        dictionary_string_storage_overflow,
        MetadataRefusalStage::QueryColumns,
        PublicMetadataRefusal::Columns("dictionary string storage exceeds remaining Columns bytes"),
    ));

    let mut dictionary_reserved = clean.clone();
    dictionary_reserved[columns_offset + dictionary_spans.reserved.start
        ..columns_offset + dictionary_spans.reserved.end]
        .copy_from_slice(&1_u16.to_le_bytes());
    rewrite_region(&mut dictionary_reserved, 0);
    cases.push((
        "columns-dictionary-reserved",
        dictionary_reserved,
        MetadataRefusalStage::QueryColumns,
        PublicMetadataRefusal::Columns("dictionary reserved field is non-zero"),
    ));

    let mut dictionary_width = clean.clone();
    dictionary_width[columns_offset + dictionary_spans.width.start
        ..columns_offset + dictionary_spans.width.end]
        .copy_from_slice(&3_u16.to_le_bytes());
    rewrite_region(&mut dictionary_width, 0);
    cases.push((
        "columns-dictionary-width",
        dictionary_width,
        MetadataRefusalStage::QueryColumns,
        PublicMetadataRefusal::Columns("invalid dictionary width 3"),
    ));

    let mut dictionary_code_mutation = clean.clone();
    dictionary_code_mutation
        [columns_offset + dictionary_code.start..columns_offset + dictionary_code.end]
        .copy_from_slice(&1_u16.to_le_bytes());
    rewrite_region(&mut dictionary_code_mutation, 0);
    cases.push((
        "columns-dictionary-code",
        dictionary_code_mutation,
        MetadataRefusalStage::QueryColumns,
        PublicMetadataRefusal::Columns("dictionary code 1 out of range"),
    ));

    let mut raw_length_truncation = clean.clone();
    set_region_length(&mut raw_length_truncation, 0, raw_length.start + 2);
    cases.push((
        "columns-raw-length-truncation",
        raw_length_truncation,
        MetadataRefusalStage::QueryColumns,
        PublicMetadataRefusal::Columns("columns truncated"),
    ));

    let mut raw_payload_truncation = clean.clone();
    set_region_length(&mut raw_payload_truncation, 0, raw_payload.end - 1);
    cases.push((
        "columns-raw-payload-truncation",
        raw_payload_truncation,
        MetadataRefusalStage::QueryColumns,
        PublicMetadataRefusal::Columns("columns truncated"),
    ));

    let mut raw_utf8 = clean.clone();
    raw_utf8[columns_offset + raw_payload.start] = 0xff;
    rewrite_region(&mut raw_utf8, 0);
    cases.push((
        "columns-raw-invalid-utf8",
        raw_utf8,
        MetadataRefusalStage::QueryColumns,
        PublicMetadataRefusal::Columns("columns UTF-8"),
    ));

    let mut columns_row_count = clean.clone();
    columns_row_count[48..52].copy_from_slice(&9_u32.to_le_bytes());
    rewrite_header_and_file(&mut columns_row_count);
    cases.push((
        "columns-row-count-mismatch",
        columns_row_count,
        MetadataRefusalStage::Open,
        PublicMetadataRefusal::Geometry("manifest metadata"),
    ));

    let mut columns_trailing = clean.clone();
    set_region_length(&mut columns_trailing, 0, columns_length + 1);
    cases.push((
        "columns-trailing-byte",
        columns_trailing,
        MetadataRefusalStage::QueryColumns,
        PublicMetadataRefusal::Columns("columns has 1 trailing bytes"),
    ));

    let mut alive_header_truncation = clean.clone();
    set_region_length(&mut alive_header_truncation, 1, 7);
    cases.push((
        "alive-header-truncation",
        alive_header_truncation,
        MetadataRefusalStage::QueryAlive,
        PublicMetadataRefusal::Alive("alive truncated at 4, need 4, total 7"),
    ));

    let mut alive_payload_truncation = clean.clone();
    alive_payload_truncation[alive_offset + alive_length - 1] = 0;
    set_region_length(&mut alive_payload_truncation, 1, alive_length - 1);
    cases.push((
        "alive-payload-truncation",
        alive_payload_truncation,
        MetadataRefusalStage::QueryAlive,
        PublicMetadataRefusal::Alive("alive truncated at 8, need 2, total 9"),
    ));

    let mut alive_declared_length = clean.clone();
    alive_declared_length[alive_offset + 4..alive_offset + 8].copy_from_slice(&1_u32.to_le_bytes());
    rewrite_region(&mut alive_declared_length, 1);
    cases.push((
        "alive-declared-length-mismatch",
        alive_declared_length,
        MetadataRefusalStage::QueryAlive,
        PublicMetadataRefusal::Alive("bitmap length 1, expected 2"),
    ));

    let mut alive_tail = clean.clone();
    alive_tail[alive_offset + alive.bitmap_span.end - 1] |= 0x80;
    rewrite_region(&mut alive_tail, 1);
    cases.push((
        "alive-tail-padding",
        alive_tail,
        MetadataRefusalStage::QueryAlive,
        PublicMetadataRefusal::Alive("non-zero bitmap tail padding"),
    ));

    let mut alive_row_count = clean.clone();
    alive_row_count[alive_offset..alive_offset + 4].copy_from_slice(&9_u32.to_le_bytes());
    alive_row_count[alive_offset + alive.bitmap_span.end - 1] &= 0x01;
    rewrite_region(&mut alive_row_count, 1);
    cases.push((
        "alive-row-count-mismatch",
        alive_row_count,
        MetadataRefusalStage::QueryAlive,
        PublicMetadataRefusal::Geometry("alive rows 9, header rows 10"),
    ));

    let mut alive_trailing = clean.clone();
    set_region_length(&mut alive_trailing, 1, alive_length + 1);
    cases.push((
        "alive-trailing-byte",
        alive_trailing,
        MetadataRefusalStage::QueryAlive,
        PublicMetadataRefusal::Alive("alive has 1 trailing bytes"),
    ));

    let mut observed = Vec::new();
    for (label, bytes, stage, expected) in cases {
        std::fs::write(&path, bytes)
            .unwrap_or_else(|error| panic!("install public metadata mutation {label}: {error}"));
        assert_public_metadata_refusal(directory.path(), stage, expected);
        observed.push(label);
    }
    let required = [
        "columns-unknown-type",
        "columns-missing-timestamp",
        "columns-invalid-nullable",
        "columns-invalid-name-utf8",
        "columns-noncanonical-timestamp",
        "columns-reserved-timestamp-id",
        "columns-reserved-timestamp-name",
        "columns-duplicate-id",
        "columns-duplicate-name",
        "columns-presence-length",
        "columns-presence-tail",
        "columns-invalid-bool",
        "columns-header-truncation",
        "columns-definition-truncation",
        "columns-dictionary-length-truncation",
        "columns-dictionary-payload-truncation",
        "columns-dictionary-invalid-utf8",
        "columns-dictionary-cardinality-overflow",
        "columns-dictionary-string-storage-overflow",
        "columns-dictionary-reserved",
        "columns-dictionary-width",
        "columns-dictionary-code",
        "columns-raw-length-truncation",
        "columns-raw-payload-truncation",
        "columns-raw-invalid-utf8",
        "columns-row-count-mismatch",
        "columns-trailing-byte",
        "alive-header-truncation",
        "alive-payload-truncation",
        "alive-declared-length-mismatch",
        "alive-tail-padding",
        "alive-row-count-mismatch",
        "alive-trailing-byte",
    ];
    assert_eq!(
        observed.as_slice(),
        required.as_slice(),
        "public typed refusal-stage ledger omits installable Columns/Alive catalog cases"
    );
}
