#![allow(clippy::expect_used)]

use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use tempfile::tempdir;
use zeppelin_embed::format::frame::{FormatCheck, FormatError};
use zeppelin_embed::graph::block::GraphNodeError;
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, IngestRetentionCheckpoint,
    IngestRetentionFaultController, IngestRetentionFaultEffect, IngestRetentionFaultKind,
    IngestRetentionOperation, IngestRetentionTestFault, PurgeError, RetentionPolicy, Revision,
    SearchRequest,
};
use zeppelin_embed::lifecycle::{
    CancelToken, OpenOptions, QueryControl, QueryError, Store, StoreError, StoreTestDependencies,
    SystemMonotonicClock,
};
use zeppelin_embed::manifest::io::MANIFEST_FILE;
use zeppelin_embed::meta::{
    AliveSet, BuildError, ColumnDefinition, ColumnId, ColumnType, DictionaryError, DocBitmap,
    Schema, SchemaError, TIMESTAMP_COLUMN,
};
use zeppelin_embed::quant::QuantError;
use zeppelin_embed::scan::{ScanError, ScanOptions};
use zeppelin_embed::segment::layout::RegionKind;
use zeppelin_embed::segment::{SegmentError, SegmentId};
use zeppelin_embed::tier::{MaintenanceBudget, MaintenanceError, MaintenanceStatus};
use zeppelin_embed::vfs::StdVfs;

#[test]
fn public_error_contracts_keep_values_messages_and_sources() {
    let segment_id = SegmentId::new(7, [0x11; 10]);
    let graph_unavailable = StoreError::GraphUnavailable { segment_id };
    assert_eq!(
        graph_unavailable.to_string(),
        format!("sealed segment {segment_id} has no graph region")
    );
    assert!(graph_unavailable.source().is_none());

    let wal_vector = StoreError::WalVector {
        seq: zeppelin_embed::wal::LogSeq::new(13),
        source: QuantError::NonFinite { index: 4 },
    };
    assert_eq!(
        wal_vector.to_string(),
        "WAL sequence 13 vector: quantization input is non-finite at coordinate 4"
    );
    assert_eq!(
        wal_vector.source().map(ToString::to_string),
        Some("quantization input is non-finite at coordinate 4".to_owned())
    );

    for (error, expected) in [
        (
            StoreError::ActiveRowOverflow,
            "active segment row or byte geometry overflow",
        ),
        (
            StoreError::ForeignPreparedSegment,
            "prepared segment belongs to another store",
        ),
        (
            StoreError::PartitionBytesOverflow,
            "partition reclaimed-byte count overflow",
        ),
        (
            StoreError::ReadCancelled,
            "store close cancelled the admitted read",
        ),
    ] {
        assert_eq!(error.to_string(), expected);
        assert!(error.source().is_none());
    }

    let background = StoreError::BackgroundStart {
        source: std::io::Error::other("thread quota"),
    };
    assert_eq!(
        background.to_string(),
        "store lifecycle thread could not start: thread quota"
    );
    assert_eq!(
        background.source().map(ToString::to_string),
        Some("thread quota".to_owned())
    );

    let query_cases = [
        (
            QueryError::Timeout { partial: false },
            "query deadline expired (partial=false)",
        ),
        (
            QueryError::Cancelled { partial: false },
            "query was cancelled (partial=false)",
        ),
        (
            QueryError::ReadCancelled { partial: false },
            "store close cancelled query (partial=false)",
        ),
    ];
    for (error, expected) in query_cases {
        assert_eq!(error.to_string(), expected);
        assert!(error.source().is_none());
    }
    let query_scan = QueryError::Scan(ScanError::ZeroDimension);
    assert_eq!(query_scan.to_string(), "scan dimension must not be zero");
    assert_eq!(
        query_scan.source().map(ToString::to_string),
        Some("scan dimension must not be zero".to_owned())
    );

    let expected_id = SegmentId::new(8, [0x22; 10]);
    let actual_id = SegmentId::new(9, [0x33; 10]);
    let segment_cases = [
        (
            SegmentError::WrongObject {
                artifact: "candidate.zseg".to_owned(),
                expected: expected_id,
                actual: actual_id,
            },
            format!(
                "artifact candidate.zseg failed object identity: expected {expected_id}, got {actual_id}"
            ),
        ),
        (
            SegmentError::MissingRegion(RegionKind::VectorCodes),
            "segment is missing region VectorCodes".to_owned(),
        ),
        (
            SegmentError::Columns("offset drift".to_owned()),
            "segment columns are invalid: offset drift".to_owned(),
        ),
        (
            SegmentError::Alive("cardinality drift".to_owned()),
            "segment alive set is invalid: cardinality drift".to_owned(),
        ),
    ];
    for (error, expected) in segment_cases {
        assert_eq!(error.to_string(), expected);
        assert!(error.source().is_none());
    }
    let graph = SegmentError::Graph(GraphNodeError::InvalidHeader("bad flags".to_owned()));
    assert_eq!(
        graph.to_string(),
        "segment graph region is invalid: graph node region header is invalid: bad flags"
    );
    assert_eq!(
        graph.source().map(ToString::to_string),
        Some("graph node region header is invalid: bad flags".to_owned())
    );

    let format = FormatError::new("manifest", FormatCheck::Magic, "bad magic");
    assert_eq!(format.artifact(), "manifest");
    assert_eq!(format.check(), FormatCheck::Magic);
    assert_eq!(format.detail(), "bad magic");
    assert_eq!(
        format.to_string(),
        "artifact manifest failed Magic: bad magic"
    );
}

#[test]
fn metadata_rejections_are_atomic_and_bitmap_algebra_is_exact() {
    let mut alive = AliveSet::new(3);
    let outside = alive.tombstone(3).expect_err("row_count is exclusive");
    assert_eq!(outside.document(), 3);
    assert_eq!(outside.row_count(), 3);
    assert_eq!(
        outside.to_string(),
        "document 3 is outside segment row count 3"
    );
    assert_eq!(alive.iter_alive().collect::<Vec<_>>(), vec![0, 1, 2]);

    let mut left = DocBitmap::from_ids([1, 3, 5]);
    let right = DocBitmap::from_ids([3, 4, 5]);
    assert!(!left.is_subset(&right));
    left.intersect_with(&right);
    assert_eq!(left.iter().collect::<Vec<_>>(), vec![3, 5]);
    left.union_with(&DocBitmap::from_ids([7]));
    assert_eq!(left.iter().collect::<Vec<_>>(), vec![3, 5, 7]);
    left.subtract(&DocBitmap::from_ids([3, 7]));
    assert_eq!(left.iter().collect::<Vec<_>>(), vec![5]);
    assert!(left.is_subset(&DocBitmap::from_ids([1, 5, 9])));

    let build_errors = [
        (
            BuildError::UnknownColumn(ColumnId::new(91)),
            "unknown column 91",
        ),
        (
            BuildError::DuplicateColumn(ColumnId::new(2)),
            "duplicate column 2",
        ),
        (
            BuildError::MissingRequiredColumn(ColumnId::new(4)),
            "missing required column 4",
        ),
        (
            BuildError::TimestampProvidedAsInput,
            "timestamp must use the dedicated ts argument",
        ),
        (
            BuildError::TypeMismatch {
                column: ColumnId::new(6),
                expected: ColumnType::Bool,
                actual: ColumnType::RawString,
            },
            "column 6 expects Bool, received RawString",
        ),
        (BuildError::TooManyRows, "segment row count exceeds u32"),
        (
            BuildError::Dictionary(DictionaryError::StringStorageOverflow),
            "string storage exceeds u32",
        ),
    ];
    for (error, expected) in build_errors {
        assert_eq!(error.to_string(), expected);
    }

    let reserved_id = Schema::new(vec![ColumnDefinition::new(
        TIMESTAMP_COLUMN,
        "other",
        ColumnType::U64,
        false,
    )])
    .expect_err("timestamp id is reserved");
    assert_eq!(reserved_id, SchemaError::ReservedTimestampId);
    assert_eq!(reserved_id.to_string(), "column id 0 is reserved for ts");

    let reserved_name = Schema::new(vec![ColumnDefinition::new(
        ColumnId::new(1),
        "ts",
        ColumnType::U64,
        false,
    )])
    .expect_err("timestamp name is reserved");
    assert_eq!(reserved_name, SchemaError::ReservedTimestampName);
    assert_eq!(reserved_name.to_string(), "column name ts is reserved");

    let duplicate_id = Schema::new(vec![
        ColumnDefinition::new(ColumnId::new(1), "one", ColumnType::U64, false),
        ColumnDefinition::new(ColumnId::new(1), "two", ColumnType::I64, false),
    ])
    .expect_err("duplicate ids are rejected");
    assert_eq!(
        duplicate_id,
        SchemaError::DuplicateColumnId(ColumnId::new(1))
    );
    assert_eq!(duplicate_id.to_string(), "duplicate column id 1");

    let duplicate_name = Schema::new(vec![
        ColumnDefinition::new(ColumnId::new(1), "same", ColumnType::U64, false),
        ColumnDefinition::new(ColumnId::new(2), "same", ColumnType::I64, false),
    ])
    .expect_err("duplicate names are rejected");
    assert_eq!(
        duplicate_name,
        SchemaError::DuplicateColumnName("same".to_owned())
    );
    assert_eq!(duplicate_name.to_string(), "duplicate column name same");
}

#[test]
fn read_only_and_closed_maintenance_paths_fail_closed_without_writes() {
    let directory = tempdir().expect("store directory");
    let writer = Store::open(directory.path(), OpenOptions::default()).expect("writer");
    writer.close().expect("initialize and close writer");

    let reader = Store::open(directory.path(), OpenOptions::read_only()).expect("read-only store");
    assert!(matches!(reader.seal(), Err(StoreError::ReadOnly)));
    assert!(matches!(
        reader.drop_partition(0..1),
        Err(StoreError::ReadOnly)
    ));
    assert!(matches!(
        reader.purge_with_available_space(&[DocId::new(1)], u64::MAX),
        Err(PurgeError::Store(StoreError::ReadOnly))
    ));
    let maintenance = reader.maintain(MaintenanceBudget {
        wall_time: Duration::from_secs(1),
        bytes: 1,
    });
    assert!(matches!(
        maintenance.status,
        MaintenanceStatus::Failed(MaintenanceError::Store(StoreError::ReadOnly))
    ));

    let invalid_window = RetentionPolicy::new(0).expect_err("zero retention window");
    assert_eq!(
        invalid_window.to_string(),
        "retention window 0 must be positive"
    );

    reader.close().expect("close reader");
    assert!(matches!(reader.seal(), Err(StoreError::Closed)));
    assert!(matches!(
        reader.drop_partition(0..1),
        Err(StoreError::Closed)
    ));
    assert!(matches!(
        reader.purge_with_available_space(&[DocId::new(1)], u64::MAX),
        Err(PurgeError::Store(StoreError::Closed))
    ));
}

#[test]
fn public_seal_cancellation_preserves_the_uncommitted_active_rows() {
    let directory = tempdir().expect("seal cancellation directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("store");
    assert_eq!(store.seal().expect("empty seal no-op"), 0);
    assert!(
        store
            .snapshot()
            .expect("empty snapshot")
            .segments()
            .is_empty()
    );
    assert!(!directory.path().join(MANIFEST_FILE).exists());
    let version = DocumentVersion::new(DocId::new(71), Revision::new(1));
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            version,
            vec![1.0, 0.0],
        )]))
        .expect("ingest active row");
    let cancel = CancelToken::new();
    cancel.cancel();
    assert!(matches!(
        store.seal_with_cancel(&cancel),
        Err(StoreError::SealCancelled)
    ));
    assert!(store.snapshot().expect("snapshot").segments().is_empty());
    store.seal().expect("uncancelled seal");
    assert_eq!(
        store.snapshot().expect("sealed snapshot").segments().len(),
        1
    );
}

#[test]
fn late_seal_cancellation_preserves_exact_active_multiset_and_emits_receipt() {
    let directory = tempdir().expect("late seal cancellation directory");
    let controller = IngestRetentionFaultController::new(21);
    let dependencies = StoreTestDependencies::new(Arc::new(StdVfs), Arc::new(SystemMonotonicClock))
        .with_ingest_retention_fault_controller(controller.clone());
    let store =
        Store::open_with_test_dependencies(directory.path(), OpenOptions::default(), dependencies)
            .expect("open late seal cancellation Store");
    let versions = [
        DocumentVersion::new(DocId::new(211), Revision::new(2)),
        DocumentVersion::new(DocId::new(212), Revision::new(3)),
    ];
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(versions[0], vec![1.0, 0.0]).with_timestamp(21),
            IngestDocument::new(versions[1], vec![0.0, 1.0]).with_timestamp(22),
        ]))
        .expect("ingest late seal rows");
    let generation_before = store
        .snapshot()
        .expect("snapshot before late seal")
        .generation();
    controller
        .arm(IngestRetentionTestFault::SealCancellation)
        .expect("arm late seal cancellation");

    let error = store
        .seal_with_cancel(&CancelToken::new())
        .expect_err("late seal cancellation must refuse publication");
    assert!(matches!(error, StoreError::SealCancelled));
    let receipts = controller.take_receipts().expect("take late seal receipts");
    assert_eq!(
        receipts.len(),
        1,
        "selected feature fault seal-cancellation produced {} Store receipts, expected 1",
        receipts.len()
    );

    let snapshot = store.snapshot().expect("snapshot after late cancellation");
    assert_eq!(snapshot.generation(), generation_before);
    assert!(snapshot.segments().is_empty());
    assert_eq!(
        store
            .stats()
            .expect("stats after late cancellation")
            .active_row_count,
        2
    );
    let outcome = store
        .search(
            SearchRequest::new(&[1.0, 1.0]),
            versions.len(),
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search after late cancellation");
    let mut observed = outcome
        .candidates
        .iter()
        .filter_map(|candidate| candidate.document())
        .collect::<Vec<_>>();
    observed.sort_unstable();
    assert_eq!(observed, versions);
    assert!(
        std::fs::read_dir(directory.path())
            .expect("list late seal directory")
            .filter_map(Result::ok)
            .all(|entry| !entry.file_name().to_string_lossy().ends_with(".zseg")),
        "late cancellation left an uncommitted segment"
    );

    let receipt = &receipts[0];
    assert_eq!(receipt.campaign(), "ingest-retention");
    assert_eq!(receipt.operation(), IngestRetentionOperation::Seal);
    assert_eq!(receipt.fault(), IngestRetentionFaultKind::SealCancellation);
    assert_eq!(
        receipt.checkpoint(),
        IngestRetentionCheckpoint::SealAfterSegmentWriteBeforeManifestCommit
    );
    assert_eq!(receipt.cardinality(), 1);
    assert_eq!(receipt.invocation_id(), 21);
    let mut candidate_segment = [0_u8; 16];
    candidate_segment[..8].copy_from_slice(&(generation_before + 1).to_be_bytes());
    candidate_segment[8..].copy_from_slice(&2_u64.to_be_bytes());
    assert_eq!(
        receipt.effect(),
        &IngestRetentionFaultEffect::SealCancellation {
            active_rows: 2,
            absorbed_wal_end: 2,
            candidate_segment,
            manifest_committed: false,
            temporary_segment_removed: true,
            generation_delta: 0,
        }
    );
    store.seal().expect("clean seal after late cancellation");
    store.close().expect("close late seal Store");
    let reopened =
        Store::open(directory.path(), OpenOptions::default()).expect("reopen after clean seal");
    let reopened_outcome = reopened
        .search(
            SearchRequest::new(&[1.0, 1.0]),
            versions.len(),
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search after clean seal reopen");
    let mut reopened_versions = reopened_outcome
        .candidates
        .iter()
        .filter_map(|candidate| candidate.document())
        .collect::<Vec<_>>();
    reopened_versions.sort_unstable();
    assert_eq!(reopened_versions, versions);
}
