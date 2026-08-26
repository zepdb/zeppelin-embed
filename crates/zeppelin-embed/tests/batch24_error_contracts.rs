#![allow(clippy::expect_used)]

use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;

use zeppelin_embed::epoch::{
    ComputeUnits, EmbedderError, EmbedderFailure, EmbedderTimeout, EmbeddingEpoch,
    EmbeddingRuntime, EmbeddingTower, EpochIdentity, EpochMismatch, EpochTransitionError,
    Normalization, StoreEpoch,
};
use zeppelin_embed::format::RegistryError;
use zeppelin_embed::format::frame::{FormatCheck, FormatError};
use zeppelin_embed::format::golden::HexError;
use zeppelin_embed::fts::bm25::Bm25Error;
use zeppelin_embed::fts::index::IndexError;
use zeppelin_embed::fts::postings::PostingsError;
use zeppelin_embed::fts::sealed::SealedSegmentError;
use zeppelin_embed::fts::snippet::SnippetError;
use zeppelin_embed::fts::tokenizer::vocab::VocabError;
use zeppelin_embed::fts::tokenizer::{TokenizerConfig, TokenizerError};
use zeppelin_embed::graph::block::GraphNodeError;
use zeppelin_embed::graph::build::GraphBuildError;
use zeppelin_embed::graph::search::{AdaptiveEfError, GraphSearchError};
use zeppelin_embed::ingest::wal_payload::PayloadError;
use zeppelin_embed::ingest::{DocId, IngestError, PurgeError, Revision, StoreLexicalError};
use zeppelin_embed::kernels::GatherShapeError;
use zeppelin_embed::lifecycle::{QueryError, StoreError};
use zeppelin_embed::manifest::ManifestError;
use zeppelin_embed::meta::{BuildError, ColumnId, ColumnType, DictionaryError, EvalError, Schema};
use zeppelin_embed::planner::{FilteredSearchError, LexicalFilterError, PlanError, SegmentBranch};
use zeppelin_embed::quant::{QuantError, QuantScheme, RescoreError};
use zeppelin_embed::scan::{Int8FactorsError, ScanError};
use zeppelin_embed::segment::layout::RegionKind;
use zeppelin_embed::segment::{SegmentError, SegmentId};
use zeppelin_embed::wal::header::WalHeaderError;
use zeppelin_embed::wal::record::{RecordEncodeError, RecordError};
use zeppelin_embed::wal::replay::{CorruptionLocation, CorruptionReason};
use zeppelin_embed::wal::{
    LogSeq, VisibleRecordError, WalReadError, WalRecoveryError, WalRetireError, WalWriteError,
};

fn store_epoch(model: &str, version: &str, tokenizer: TokenizerConfig) -> StoreEpoch {
    let document = EmbeddingTower {
        model_id: model.to_owned(),
        model_version: version.to_owned(),
        weights_digest: vec![0x24, model.len() as u8],
        dims: 4,
        normalization: Normalization::L2,
        prompt_prefix: "document: ".to_owned(),
        max_tokens: 32,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: Some("24A24".to_owned()),
    };
    let mut query = document.clone();
    query.prompt_prefix = "query: ".to_owned();
    StoreEpoch {
        embedding: EmbeddingEpoch {
            document,
            query,
            alignment_digest: vec![0xa1, 0x24],
        },
        tokenizer: tokenizer.epoch(),
    }
}

fn assert_contract<E: Error>(error: E, expected: &str, source: Option<&str>) {
    assert_eq!(error.to_string(), expected);
    assert_eq!(error.source().map(ToString::to_string).as_deref(), source);
}

#[test]
fn embedding_and_epoch_transition_errors_preserve_every_actionable_value() {
    let epoch_a = store_epoch("model-a", "1", TokenizerConfig::text_default());
    let epoch_b = store_epoch("model-b", "2", TokenizerConfig::code());
    assert_eq!(epoch_a.embedding.document.model_label(), "model-a@1");
    assert_eq!(
        epoch_a.identity().embedding.to_hex(),
        epoch_a.identity().embedding.to_string()
    );

    let failure = EmbedderFailure::new("delegate rejected revision 7");
    assert_eq!(failure.detail(), "delegate rejected revision 7");
    let failure = EmbedderError::from(failure);
    assert_contract(
        failure,
        "embedding delegate failed: delegate rejected revision 7",
        Some("embedding delegate failed: delegate rejected revision 7"),
    );
    let timeout = EmbedderTimeout::new("deadline 25 ms");
    assert_eq!(timeout.detail(), "deadline 25 ms");
    let timeout = EmbedderError::from(timeout);
    assert_contract(
        timeout,
        "embedding delegate timed out: deadline 25 ms",
        Some("embedding delegate timed out: deadline 25 ms"),
    );

    let both = EpochMismatch {
        expected: epoch_a.identity(),
        declared: epoch_b.identity(),
    };
    assert!(
        both.to_string()
            .starts_with("embedding and tokenizer epochs differ:")
    );
    let embedding_only = EpochMismatch {
        expected: epoch_a.identity(),
        declared: EpochIdentity {
            embedding: epoch_b.identity().embedding,
            tokenizer: epoch_a.identity().tokenizer,
        },
    };
    assert!(
        embedding_only
            .to_string()
            .starts_with("embedding epoch differs:")
    );
    let tokenizer_only = EpochMismatch {
        expected: epoch_a.identity(),
        declared: EpochIdentity {
            embedding: epoch_a.identity().embedding,
            tokenizer: epoch_b.identity().tokenizer,
        },
    };
    assert!(
        tokenizer_only
            .to_string()
            .starts_with("tokenizer epoch differs:")
    );
    assert_eq!(
        EpochMismatch {
            expected: epoch_a.identity(),
            declared: epoch_a.identity(),
        }
        .to_string(),
        "embedding and tokenizer epochs match"
    );

    let target = epoch_b.identity();
    let segment_id = SegmentId::new(24, [0x24; 10]);
    let cases = vec![
        (
            EpochTransitionError::EpochUnavailable { target },
            format!(
                "epoch alias ({}, {}) has no retained segment set",
                target.embedding, target.tokenizer
            ),
        ),
        (
            EpochTransitionError::IncompleteEpoch {
                target,
                missing_documents: 2,
                unexpected_documents: 3,
            },
            format!(
                "epoch alias ({}, {}) is incomplete: 2 published revisions missing and 3 unexpected target revisions",
                target.embedding, target.tokenizer
            ),
        ),
        (
            EpochTransitionError::MissingDocumentIdentity {
                epoch: target.embedding,
                segment_id,
                row: 7,
            },
            format!(
                "embedding epoch {} segment {segment_id} live row 7 has no document identity",
                target.embedding
            ),
        ),
        (
            EpochTransitionError::EmbeddingEpochUnavailable {
                target: target.embedding,
            },
            format!("embedding epoch {} has no retained segments", target.embedding),
        ),
        (
            EpochTransitionError::PublishedEpoch {
                target: target.embedding,
            },
            format!(
                "published embedding epoch {} cannot be dropped",
                target.embedding
            ),
        ),
        (
            EpochTransitionError::UnsealedWrites {
                active_rows: 4,
                absorbed_through: 11,
                durable_end: 13,
            },
            "epoch transition requires a sealed WAL: active rows 4, manifest absorbed through 11, durable WAL end 13".to_owned(),
        ),
        (
            EpochTransitionError::ReclaimedBytesOverflow,
            "dropped epoch segment bytes exceed u64".to_owned(),
        ),
    ];
    for (error, expected) in cases {
        assert_contract(error, &expected, None);
    }
    assert_contract(
        EpochTransitionError::Store(StoreError::Closed),
        "store is closed",
        Some("store is closed"),
    );
}

#[test]
fn store_errors_name_the_failed_boundary_without_hiding_sources() {
    let path = PathBuf::from("store.ze");
    let simple = vec![
        (
            StoreError::NotDirectory { path: path.clone() },
            "store path is not a directory: store.ze".to_owned(),
        ),
        (
            StoreError::StoreBusy { path: path.clone() },
            "store already has a writer: store.ze".to_owned(),
        ),
        (
            StoreError::EpochUndeclared,
            "store epoch is persisted but the caller declared none".to_owned(),
        ),
        (
            StoreError::EpochUnstamped,
            "store manifest predates epoch identity and cannot adopt a declaration".to_owned(),
        ),
        (
            StoreError::BudgetExceeded {
                needed: 25,
                budget: 24,
                component: "active vectors",
            },
            "store active vectors allocation needs 25 bytes, budget is 24 bytes".to_owned(),
        ),
        (
            StoreError::AllocationFailed {
                needed: 25,
                component: "query pool",
            },
            "store query pool allocator rejected 25 bytes".to_owned(),
        ),
        (
            StoreError::DimensionMismatch {
                expected: 4,
                actual: 3,
            },
            "active vector dimension 3 does not match 4".to_owned(),
        ),
        (
            StoreError::EmptyActiveSegment,
            "active segment is empty".to_owned(),
        ),
        (
            StoreError::SealCancelled,
            "seal was cancelled before commit".to_owned(),
        ),
        (StoreError::ReadOnly, "store handle is read-only".to_owned()),
        (
            StoreError::GenerationOverflow,
            "store snapshot generation overflow".to_owned(),
        ),
        (
            StoreError::PurgeRecovery {
                detail: "intent checksum".to_owned(),
            },
            "physical purge recovery failed: intent checksum".to_owned(),
        ),
        (StoreError::Closing, "store is closing".to_owned()),
        (StoreError::Closed, "store is closed".to_owned()),
        (
            StoreError::BackgroundHandshake,
            "store lifecycle thread startup handshake failed".to_owned(),
        ),
        (
            StoreError::BackgroundThreadPanicked,
            "store lifecycle thread panicked".to_owned(),
        ),
        (
            StoreError::QueryPoolHandshake,
            "store query worker startup handshake failed".to_owned(),
        ),
        (
            StoreError::QueryPoolThreadPanicked,
            "store query worker panicked".to_owned(),
        ),
        (
            StoreError::QueryPoolCapacity {
                source: "no physical cores".to_owned(),
            },
            "store query worker capacity failed: no physical cores".to_owned(),
        ),
        (
            StoreError::Synchronization {
                component: "snapshot",
            },
            "store lifecycle synchronization poisoned: snapshot".to_owned(),
        ),
    ];
    for (error, expected) in simple {
        assert_contract(error, &expected, None);
    }

    assert_contract(
        StoreError::Io {
            path,
            source: std::io::Error::other("disk offline"),
        },
        "store I/O store.ze: disk offline",
        Some("disk offline"),
    );
    assert_contract(
        StoreError::Statistics {
            component: "resident bytes",
            source: std::io::Error::other("mincore failed"),
        },
        "store statistics resident bytes: mincore failed",
        Some("mincore failed"),
    );
    assert_contract(
        StoreError::QueryPoolStart {
            source: std::io::Error::other("thread quota"),
        },
        "store query worker could not start: thread quota",
        Some("thread quota"),
    );

    let schema = Schema::timestamp_only();
    let mismatch = StoreError::SchemaMismatch {
        persisted: schema.clone(),
        declared: schema,
    };
    assert!(mismatch.to_string().starts_with("declared store schema"));
    assert!(mismatch.source().is_none());
}

#[test]
fn wal_errors_keep_framing_ranges_sequences_and_source_chains() {
    let record = RecordError::BodyTruncated {
        payload_length: 9,
        needed: 35,
        available: 30,
    };
    assert_contract(
        VisibleRecordError::Record(record),
        "visible WAL record: WAL payload length 9 needs 35 framed bytes, got 30",
        Some("WAL payload length 9 needs 35 framed bytes, got 30"),
    );
    assert_contract(
        VisibleRecordError::InvalidEncodedRange {
            start: 4,
            end: 17,
            available: 12,
        },
        "visible WAL encoded range 4..17 exceeds 12 bytes",
        None,
    );

    let writes = vec![
        (
            WalWriteError::NonEmptyWal { length: 41 },
            "WAL path is not empty: 41 bytes".to_owned(),
        ),
        (
            WalWriteError::RecoveredBytesOverflow,
            "recovered WAL record bytes overflow usize".to_owned(),
        ),
        (
            WalWriteError::GroupTooLarge {
                encoded_bytes: 25,
                max_group_bytes: 24,
            },
            "WAL group needs 25 bytes, exceeding 24".to_owned(),
        ),
        (
            WalWriteError::SequenceExhausted,
            "WAL sequence space exhausted".to_owned(),
        ),
        (
            WalWriteError::Header(RegistryError::UnknownFamily(99)),
            "WAL header: unknown format family 99".to_owned(),
        ),
        (
            WalWriteError::Record(RecordEncodeError::PayloadTooLarge(usize::MAX)),
            format!("WAL record: WAL payload length {} exceeds u32", usize::MAX),
        ),
        (
            WalWriteError::Poisoned("commit queue"),
            "commit queue poisoned".to_owned(),
        ),
        (
            WalWriteError::Failed {
                kind: std::io::ErrorKind::BrokenPipe,
                detail: Arc::<str>::from("flush broke"),
            },
            "WAL writer failed with BrokenPipe: flush broke".to_owned(),
        ),
    ];
    for (error, expected) in writes {
        assert_eq!(error.to_string(), expected);
    }

    assert_contract(
        WalRetireError::BeyondDurable {
            requested: LogSeq::new(9),
            durable_end: Some(LogSeq::new(8)),
        },
        "cannot retire WAL sequence 9 past durable end 8",
        None,
    );
    assert_contract(
        WalRetireError::BeyondDurable {
            requested: LogSeq::new(1),
            durable_end: None,
        },
        "cannot retire WAL sequence 1 before any sequence is durable",
        None,
    );
    assert_contract(
        WalRetireError::Writer(WalWriteError::SequenceExhausted),
        "cannot retire visible WAL records: WAL sequence space exhausted",
        Some("WAL sequence space exhausted"),
    );

    assert_contract(
        WalRecoveryError::MissingFile,
        "WAL recovery path is missing",
        None,
    );
    assert_contract(
        WalRecoveryError::InvalidHeader(WalHeaderError::Missing),
        "WAL recovery header: WAL file header is missing",
        Some("WAL file header is missing"),
    );
    let reason = CorruptionReason::SequenceGap {
        expected: LogSeq::new(4),
        actual: LogSeq::new(7),
        location: CorruptionLocation::Middle,
    };
    assert_contract(
        WalRecoveryError::CorruptAt { offset: 40, reason },
        &format!("WAL recovery stopped at byte 40: {reason:?}"),
        None,
    );

    assert_contract(
        WalReadError::Io(std::io::Error::other("read failed")),
        "WAL read: read failed",
        Some("read failed"),
    );
    assert_contract(
        WalReadError::Header(WalHeaderError::NonZeroFileLength { actual: 8 }),
        "WAL read header: WAL file length must be zero, got 8",
        Some("WAL file length must be zero, got 8"),
    );
    assert_contract(
        WalReadError::InvalidRecoveredRange {
            start: 40,
            end: 70,
            available: 64,
        },
        "WAL recovered range 40..70 exceeds 64 bytes",
        None,
    );
}

#[test]
fn persisted_registry_header_and_manifest_errors_are_exact_and_typed() {
    let registry = [
        (RegistryError::UnknownFamily(24), "unknown format family 24"),
        (
            RegistryError::UnsupportedVersion {
                family: 10,
                version: 3,
                minimum: 1,
                maximum: 2,
            },
            "format family 10 version 3 is outside accepted range 1..=2",
        ),
        (
            RegistryError::RetiredScheme(3),
            "quantization scheme id 3 is permanently retired",
        ),
        (
            RegistryError::UnknownScheme(9),
            "unknown quantization scheme 9",
        ),
    ];
    for (error, expected) in registry {
        assert_eq!(error.to_string(), expected);
    }

    let headers = vec![
        (
            WalHeaderError::Missing,
            "WAL file header is missing".to_owned(),
        ),
        (
            WalHeaderError::Truncated {
                needed: 40,
                available: 31,
            },
            "WAL file header needs 40 bytes, got 31".to_owned(),
        ),
        (
            WalHeaderError::WrongMagic {
                expected: *b"ZEPEMBED",
                actual: *b"NOPEMAGC",
            },
            format!(
                "WAL magic expected {:?}, got {:?}",
                *b"ZEPEMBED", *b"NOPEMAGC"
            ),
        ),
        (
            WalHeaderError::WrongFamily {
                expected: 11,
                actual: 10,
            },
            "WAL family expected 11, got 10".to_owned(),
        ),
        (
            WalHeaderError::UnsupportedVersion {
                family: 11,
                version: 2,
                minimum: 1,
                maximum: 1,
            },
            "WAL family 11 version 2 is outside accepted range 1..=1".to_owned(),
        ),
        (
            WalHeaderError::InvalidHeaderLength {
                expected: 40,
                actual: 32,
            },
            "WAL header length expected 40, got 32".to_owned(),
        ),
        (
            WalHeaderError::NonZeroFileLength { actual: 9 },
            "WAL file length must be zero, got 9".to_owned(),
        ),
        (
            WalHeaderError::InvalidSharedHeader {
                check: FormatCheck::HeaderLength,
            },
            "WAL shared header failed HeaderLength".to_owned(),
        ),
    ];
    for (error, expected) in headers {
        assert_eq!(error.to_string(), expected);
    }

    let epoch = store_epoch("manifest", "1", TokenizerConfig::text_default()).identity();
    let segment = SegmentId::new(25, [0x25; 10]);
    let manifests = vec![
        (
            ManifestError::Decode("epoch count overflow".to_owned()),
            "manifest decode failed: epoch count overflow".to_owned(),
        ),
        (
            ManifestError::AheadOfLog {
                snapshot: 9,
                durable: 8,
            },
            "manifest log sequence 9 is ahead of durable WAL end 8".to_owned(),
        ),
        (
            ManifestError::UnknownEpochAlias { alias: epoch },
            format!(
                "manifest epoch alias ({}, {}) does not name a registry entry",
                epoch.embedding, epoch.tokenizer
            ),
        ),
        (
            ManifestError::UnknownSegmentEpoch {
                segment,
                epoch: epoch.embedding,
            },
            format!(
                "manifest segment {segment} names unknown embedding epoch {}",
                epoch.embedding
            ),
        ),
    ];
    for (error, expected) in manifests {
        assert_contract(error, &expected, None);
    }
    assert_contract(
        ManifestError::Format(FormatError::new(
            "manifest.ze",
            FormatCheck::FileChecksum,
            "mismatch",
        )),
        "artifact manifest.ze failed FileChecksum: mismatch",
        Some("artifact manifest.ze failed FileChecksum: mismatch"),
    );
}

fn assert_error_fragment<E: Error>(error: E, fragment: &str, has_source: bool) {
    assert!(
        error.to_string().contains(fragment),
        "error did not preserve {fragment:?}: {error}"
    );
    assert_eq!(
        error.source().is_some(),
        has_source,
        "wrong source for {error}"
    );
}

#[test]
fn planner_metadata_and_ingest_errors_preserve_the_rejected_values() {
    let column = ColumnId::new(24);
    let plan = vec![
        (
            PlanError::UnknownColumn(column),
            "unknown column 24".to_owned(),
        ),
        (
            PlanError::TypeMismatch {
                column,
                expected: ColumnType::U64,
                actual: ColumnType::RawString,
            },
            "column 24 expects U64, received RawString".to_owned(),
        ),
        (
            PlanError::RangeRequiresNumericColumn(column),
            "column 24 is not numeric".to_owned(),
        ),
        (
            PlanError::RowCountMismatch {
                columns: 3,
                alive: 2,
            },
            "column row count 3 differs from alive row count 2".to_owned(),
        ),
    ];
    for (error, expected) in plan {
        assert_contract(error, &expected, None);
    }

    let eval = vec![
        EvalError::RowCountMismatch {
            columns: 4,
            alive: 5,
        },
        EvalError::UnknownColumn(column),
        EvalError::TypeMismatch {
            column,
            expected: ColumnType::Bool,
            actual: ColumnType::I64,
        },
        EvalError::RangeRequiresNumericColumn(column),
    ];
    for error in eval {
        assert!(error.to_string().contains('4') || error.to_string().contains("column 24"));
    }

    assert_error_fragment(
        FilteredSearchError::Plan(PlanError::UnknownColumn(column)),
        "unknown column 24",
        true,
    );
    assert_error_fragment(
        FilteredSearchError::Query(QueryError::Cancelled { partial: false }),
        "query was cancelled (partial=false)",
        true,
    );
    assert_error_fragment(
        FilteredSearchError::ActiveMetadata("row 24".to_owned()),
        "active metadata construction failed: row 24",
        false,
    );
    assert_error_fragment(
        FilteredSearchError::PlanReportMismatch {
            reported: SegmentBranch::ExactAllowList,
            executed: SegmentBranch::MaskedScan,
        },
        "reported branch ExactAllowList differs from executed branch MaskedScan",
        false,
    );
    assert_error_fragment(
        FilteredSearchError::InvalidPlanNode("fusion entered vector execution"),
        "invalid vector plan node: fusion entered vector execution",
        false,
    );

    assert_error_fragment(
        LexicalFilterError::SegmentCount {
            expected: 3,
            actual: 2,
        },
        "supplied 2 segment bitmaps, expected 3",
        false,
    );
    assert_error_fragment(
        LexicalFilterError::RowOutOfRange {
            segment: 1,
            row: 9,
            row_count: 8,
        },
        "row 9 is outside segment 1 row count 8",
        false,
    );
    assert_error_fragment(
        LexicalFilterError::Index(IndexError::RowsNotAscending { row: 7 }),
        "documents must arrive in row order; got 7",
        true,
    );

    let build = vec![
        BuildError::UnknownColumn(column),
        BuildError::DuplicateColumn(column),
        BuildError::MissingRequiredColumn(column),
        BuildError::TimestampProvidedAsInput,
        BuildError::TypeMismatch {
            column,
            expected: ColumnType::F64,
            actual: ColumnType::Bool,
        },
        BuildError::TooManyRows,
    ];
    for error in build {
        assert!(!error.to_string().is_empty());
    }

    let epoch_a = store_epoch("ingest-a", "1", TokenizerConfig::text_default());
    let epoch_b = store_epoch("ingest-b", "2", TokenizerConfig::code());
    let ingest = vec![
        IngestError::Store(StoreError::ReadOnly),
        IngestError::EmptyBatch,
        IngestError::EpochMismatch(EpochMismatch {
            expected: epoch_a.identity(),
            declared: epoch_b.identity(),
        }),
        IngestError::EpochUndeclared,
        IngestError::EpochUnstamped,
        IngestError::StaleRevision {
            doc_id: DocId::new(24),
            current: Revision::new(7),
            attempted: Revision::new(6),
        },
        IngestError::Vector(QuantError::NonFinite { index: 3 }),
        IngestError::Lexical(IndexError::RowsNotAscending { row: 4 }),
        IngestError::Tokenizer(TokenizerError::InvalidUtf8 { offset: 5 }),
        IngestError::Columns(BuildError::DuplicateColumn(column)),
        IngestError::Payload(PayloadError::TrailingBytes(9)),
    ];
    for error in ingest {
        assert!(!error.to_string().is_empty());
        assert_eq!(
            error.source().is_some(),
            !matches!(
                error,
                IngestError::EmptyBatch
                    | IngestError::EpochUndeclared
                    | IngestError::EpochUnstamped
                    | IngestError::StaleRevision { .. }
            )
        );
    }

    let segment_id = SegmentId::new(24, [0x42; 10]);
    assert_error_fragment(
        StoreLexicalError::Query(QueryError::Timeout { partial: false }),
        "query deadline expired (partial=false)",
        true,
    );
    assert_error_fragment(
        StoreLexicalError::Lexical(LexicalFilterError::SegmentCount {
            expected: 2,
            actual: 1,
        }),
        "supplied 1 segment bitmaps, expected 2",
        true,
    );
    assert_error_fragment(
        StoreLexicalError::MissingDocumentIdentity { segment_id },
        &format!("sealed lexical segment {segment_id} has no document identity"),
        false,
    );
}

#[test]
fn scan_rescore_segment_graph_and_purge_errors_preserve_geometry_and_sources() {
    let scans = vec![
        ScanError::ZeroDimension,
        ScanError::RowDataLength {
            dimension: 4,
            actual: 7,
        },
        ScanError::SchemeMismatch {
            query: QuantScheme::F32,
            rows: QuantScheme::Bit4,
        },
        ScanError::FactorCount {
            expected: 3,
            actual: 2,
        },
        ScanError::NonFiniteInput { index: 5 },
        ScanError::NonFiniteScore { row_id: 6 },
        ScanError::Quant(QuantError::CodeLength {
            expected: 4,
            actual: 3,
        }),
        ScanError::ArithmeticOverflow,
        ScanError::WorkerPanicked,
        ScanError::CpuCount("no physical cores".to_owned()),
        ScanError::Timeout { partial: false },
        ScanError::Cancelled { partial: false },
        ScanError::ReadCancelled { partial: false },
    ];
    for error in scans {
        assert!(!error.to_string().is_empty());
    }

    let rescore = vec![
        RescoreError::ZeroDimension,
        RescoreError::QueryDimension {
            expected: 4,
            actual: 3,
        },
        RescoreError::RowDataLength {
            dimension: 4,
            actual: 7,
        },
        RescoreError::CoarseScoreCount {
            expected: 3,
            actual: 2,
        },
        RescoreError::CandidateRowCount {
            expected: 3,
            actual: 2,
        },
        RescoreError::CandidateRowOutOfRange {
            position: 1,
            row_index: 9,
            row_count: 8,
        },
        RescoreError::ZeroK,
        RescoreError::ZeroOversample,
        RescoreError::NonFiniteCoarseScore { index: 2 },
        RescoreError::NonFiniteExactScore { row_index: 3 },
        RescoreError::InsufficientCandidates {
            k: 4,
            candidates: 3,
        },
        RescoreError::ArithmeticOverflow,
    ];
    for error in rescore {
        assert!(!error.to_string().is_empty());
    }

    let segment_a = SegmentId::new(24, [0x24; 10]);
    let segment_b = SegmentId::new(25, [0x25; 10]);
    let segments = vec![
        SegmentError::Io {
            path: PathBuf::from("segment.ze"),
            source: std::io::Error::other("read failed"),
        },
        SegmentError::Format(FormatError::new(
            "segment.ze",
            FormatCheck::FileChecksum,
            "expected 24, got 25",
        )),
        SegmentError::WrongObject {
            artifact: "segment.ze".to_owned(),
            expected: segment_a,
            actual: segment_b,
        },
        SegmentError::MissingRegion(RegionKind::VectorRescore),
        SegmentError::Geometry("row stride 3".to_owned()),
        SegmentError::Columns("column count 2".to_owned()),
        SegmentError::Alive("row count 3".to_owned()),
        SegmentError::Graph(GraphNodeError::InvalidHeader("flags 1".to_owned())),
        SegmentError::Postings(SealedSegmentError::Geometry("span offset")),
    ];
    for error in segments {
        assert!(!error.to_string().is_empty());
        assert_eq!(
            error.source().is_some(),
            matches!(
                error,
                SegmentError::Io { .. }
                    | SegmentError::Format(_)
                    | SegmentError::Graph(_)
                    | SegmentError::Postings(_)
            )
        );
    }

    let adaptive = vec![
        AdaptiveEfError::ZeroK,
        AdaptiveEfError::KExceedsRows { k: 5, rows: 4 },
        AdaptiveEfError::ExplicitBelowK { k: 5, ef: 4 },
        AdaptiveEfError::ExplicitExceedsRows { ef: 6, rows: 5 },
        AdaptiveEfError::ArithmeticOverflow,
    ];
    for error in adaptive {
        assert!(!error.to_string().is_empty());
    }
    let searches = vec![
        GraphSearchError::Geometry("dimension 3".to_owned()),
        GraphSearchError::Graph(GraphNodeError::NodeIdOutOfRange {
            node_id: 5,
            node_count: 4,
        }),
        GraphSearchError::Quant(QuantError::EmptyVector),
        GraphSearchError::AdaptiveEf(AdaptiveEfError::ExplicitBelowK { k: 2, ef: 1 }),
        GraphSearchError::Rescore(RescoreError::ZeroK),
        GraphSearchError::VisitedCapExceeded {
            visited: 25,
            cap: 24,
        },
        GraphSearchError::Cancelled { partial: false },
        GraphSearchError::Timeout { partial: false },
        GraphSearchError::ReadCancelled { partial: false },
        GraphSearchError::Scan(ScanError::ArithmeticOverflow),
    ];
    for error in searches {
        assert!(!error.to_string().is_empty());
        assert_eq!(
            error.source().is_some(),
            matches!(
                error,
                GraphSearchError::Graph(_)
                    | GraphSearchError::Quant(_)
                    | GraphSearchError::AdaptiveEf(_)
                    | GraphSearchError::Rescore(_)
                    | GraphSearchError::Scan(_)
            )
        );
    }

    let builds = vec![
        GraphBuildError::CheckpointIo {
            path: PathBuf::from("graph.ckpt"),
            source: std::io::Error::other("write failed"),
        },
        GraphBuildError::CheckpointCorrupt("row 24".to_owned()),
        GraphBuildError::BudgetExhausted { rows_completed: 24 },
        GraphBuildError::Cancelled { partial: false },
        GraphBuildError::Timeout { partial: false },
        GraphBuildError::ReadCancelled { partial: false },
        GraphBuildError::Quant(QuantError::NonFinite { index: 2 }),
        GraphBuildError::NodeBlock(GraphNodeError::ArithmeticOverflow),
        GraphBuildError::Geometry("node count 0".to_owned()),
        GraphBuildError::NodeIdOutOfRange {
            node_id: 25,
            node_count: 24,
        },
        GraphBuildError::Store(StoreError::BudgetExceeded {
            needed: 25,
            budget: 24,
            component: "graph arena",
        }),
    ];
    for error in builds {
        assert!(!error.to_string().is_empty());
        assert_eq!(
            error.source().is_some(),
            matches!(
                error,
                GraphBuildError::CheckpointIo { .. }
                    | GraphBuildError::Quant(_)
                    | GraphBuildError::NodeBlock(_)
                    | GraphBuildError::Store(_)
            )
        );
    }

    let purges = vec![
        PurgeError::Store(StoreError::ReadOnly),
        PurgeError::InsufficientTempSpace {
            segment_bytes: 100,
            available_bytes: 119,
            required_bytes: 120,
        },
        PurgeError::PurgeInProgress,
        PurgeError::UnknownToken { token_id: 24 },
        PurgeError::IntentFormat(FormatError::new(
            "purge.ze",
            FormatCheck::Magic,
            "wrong magic",
        )),
        PurgeError::IntentDecode("unknown version 2".to_owned()),
        PurgeError::WalPayload(PayloadError::UnknownOperation(24)),
    ];
    for error in purges {
        assert!(!error.to_string().is_empty());
        assert_eq!(
            error.source().is_some(),
            matches!(
                error,
                PurgeError::Store(_) | PurgeError::IntentFormat(_) | PurgeError::WalPayload(_)
            )
        );
    }

    assert_eq!(
        PostingsError::BadMagic.to_string(),
        "posting stream magic did not match"
    );
}

#[test]
fn lexical_payload_and_kernel_error_matrices_name_every_rejected_boundary() {
    let payloads = vec![
        PayloadError::UnknownOperation(24),
        PayloadError::LengthOverflow,
        PayloadError::Truncated,
        PayloadError::Version(2),
        PayloadError::Flags(0x24),
        PayloadError::Reserved(0x24),
        PayloadError::EmptyVector,
        PayloadError::EmptyDelete,
        PayloadError::NonFiniteVector { index: 3 },
        PayloadError::ValueKind(9),
        PayloadError::ValueLength {
            kind: 2,
            expected: 8,
            actual: 7,
        },
        PayloadError::Boolean(2),
        PayloadError::Utf8,
        PayloadError::TrailingBytes(5),
        PayloadError::FieldBitmap(0x8000_0000),
        PayloadError::Lexical("bad position".to_owned()),
        PayloadError::Columns("duplicate column".to_owned()),
    ];
    let payload_fragments = [
        "operation 24",
        "length overflow",
        "truncated",
        "version 2",
        "0x0024",
        "0x000024",
        "vector is empty",
        "document list is empty",
        "coordinate 3",
        "kind 9",
        "needs 8 bytes, got 7",
        "Boolean byte 2",
        "not valid UTF-8",
        "5 trailing bytes",
        "0x80000000",
        "bad position",
        "duplicate column",
    ];
    for (error, fragment) in payloads.into_iter().zip(payload_fragments) {
        assert_error_fragment(error, fragment, false);
    }

    let postings = vec![
        PostingsError::Truncated {
            needed: 8,
            available: 7,
        },
        PostingsError::BadMagic,
        PostingsError::UnsupportedVersion { found: 3 },
        PostingsError::BitWidthTooLarge { bits: 33 },
        PostingsError::ZeroBlockSize,
        PostingsError::DocidsNotAscending { docid: 24 },
        PostingsError::PositionsNotAscending { position: 25 },
        PostingsError::ZeroTermFrequency,
        PostingsError::InconsistentPostingCount {
            declared: 8,
            described: 7,
        },
    ];
    let posting_fragments = [
        "needed 8 bytes, had 7",
        "magic did not match",
        "version 3",
        "bit width 33",
        "block size was zero",
        "ascending at 24",
        "ascending at 25",
        "zero term frequency",
        "declares 8 postings, blocks describe 7",
    ];
    for (error, fragment) in postings.into_iter().zip(posting_fragments) {
        assert_error_fragment(error, fragment, false);
    }

    let indexes = vec![
        IndexError::Postings(PostingsError::BadMagic),
        IndexError::Stats(Bm25Error::NoTokens),
        IndexError::RowsNotAscending { row: 24 },
        IndexError::LiveRowOutOfRange {
            segment: 2,
            row: 9,
            row_count: 8,
        },
        IndexError::LiveLengthMissing { segment: 3, row: 7 },
    ];
    let index_fragments = [
        "postings rejected",
        "at least one analyzed token",
        "got 24",
        "row 9 is outside segment 2 row count 8",
        "row 7 has no length counter in segment 3",
    ];
    for (error, fragment) in indexes.into_iter().zip(index_fragments) {
        assert_error_fragment(error, fragment, false);
    }

    let vocab = vec![
        VocabError::EmptyCanonicalTerm,
        VocabError::EmptySurfaceForm {
            canonical: "zed".to_owned(),
        },
        VocabError::SurfaceFormTooLong {
            canonical: "zed".to_owned(),
            terms: 9,
        },
        VocabError::ConflictingSurfaceForm {
            surface: "z e".to_owned(),
            existing: "zed".to_owned(),
            attempted: "zee".to_owned(),
        },
    ];
    for error in vocab {
        assert!(!error.to_string().is_empty());
    }

    for (error, expected) in [
        (
            SnippetError::FieldNotStored,
            "field is not stored, so it has no match offsets",
        ),
        (
            SnippetError::ZeroWindow,
            "snippet window length must be positive",
        ),
    ] {
        assert_contract(error, expected, None);
    }
    for (error, expected) in [
        (
            HexError::OddLength,
            "golden hex has an odd number of nibbles",
        ),
        (
            HexError::InvalidDigit(b'x'),
            "golden hex contains invalid byte 120",
        ),
    ] {
        assert_contract(error, expected, None);
    }
    for error in [
        GatherShapeError::DimensionTooLarge {
            actual: 65_536,
            maximum: 65_535,
        },
        GatherShapeError::RowBytes {
            expected: 64,
            actual: 63,
        },
        GatherShapeError::RowLength {
            index: 3,
            expected: 64,
            actual: 63,
        },
    ] {
        assert!(!error.to_string().is_empty());
    }
    for (error, expected) in [
        (
            Int8FactorsError::NonFiniteScale,
            "Int8 row scale must be finite",
        ),
        (
            Int8FactorsError::NegativeScale,
            "Int8 row scale must not be negative",
        ),
        (
            Int8FactorsError::NonFiniteOffset,
            "Int8 row offset must be finite",
        ),
    ] {
        assert_contract(error, expected, None);
    }
    for (error, expected) in [
        (
            DictionaryError::CardinalityOverflow,
            "dictionary cardinality exceeds u32",
        ),
        (
            DictionaryError::StringStorageOverflow,
            "string storage exceeds u32",
        ),
    ] {
        assert_contract(error, expected, None);
    }
}

#[test]
fn public_error_conversions_keep_their_original_typed_causes() {
    assert!(matches!(
        BuildError::from(DictionaryError::StringStorageOverflow),
        BuildError::Dictionary(DictionaryError::StringStorageOverflow)
    ));
    assert!(matches!(
        TokenizerError::from(VocabError::EmptyCanonicalTerm),
        TokenizerError::Vocab(VocabError::EmptyCanonicalTerm)
    ));
    assert!(matches!(
        IndexError::from(PostingsError::BadMagic),
        IndexError::Postings(PostingsError::BadMagic)
    ));
    assert!(matches!(
        IndexError::from(Bm25Error::EmptyCorpus),
        IndexError::Stats(Bm25Error::EmptyCorpus)
    ));
    assert!(matches!(
        SealedSegmentError::from(PostingsError::ZeroBlockSize),
        SealedSegmentError::Postings(PostingsError::ZeroBlockSize)
    ));
    assert!(matches!(
        SegmentError::from(FormatError::new("x", FormatCheck::Length, "short")),
        SegmentError::Format(_)
    ));
    assert!(matches!(
        SegmentError::from(GraphNodeError::ArithmeticOverflow),
        SegmentError::Graph(GraphNodeError::ArithmeticOverflow)
    ));
    assert!(matches!(
        SegmentError::from(SealedSegmentError::Geometry("short")),
        SegmentError::Postings(SealedSegmentError::Geometry("short"))
    ));

    assert!(matches!(
        GraphSearchError::from(GraphNodeError::ArithmeticOverflow),
        GraphSearchError::Graph(GraphNodeError::ArithmeticOverflow)
    ));
    assert!(matches!(
        GraphSearchError::from(QuantError::EmptyVector),
        GraphSearchError::Quant(QuantError::EmptyVector)
    ));
    assert!(matches!(
        GraphSearchError::from(GatherShapeError::RowBytes {
            expected: 2,
            actual: 1,
        }),
        GraphSearchError::Gather(GatherShapeError::RowBytes {
            expected: 2,
            actual: 1,
        })
    ));
    assert!(matches!(
        GraphSearchError::from(AdaptiveEfError::ZeroK),
        GraphSearchError::AdaptiveEf(AdaptiveEfError::ZeroK)
    ));
    assert!(matches!(
        GraphSearchError::from(RescoreError::ZeroK),
        GraphSearchError::Rescore(RescoreError::ZeroK)
    ));

    assert!(matches!(
        GraphBuildError::from(SegmentError::Geometry("rows".to_owned())),
        GraphBuildError::Segment(SegmentError::Geometry(_))
    ));
    assert!(matches!(
        GraphBuildError::from(QuantError::EmptyVector),
        GraphBuildError::Quant(QuantError::EmptyVector)
    ));
    assert!(matches!(
        GraphBuildError::from(GraphNodeError::ArithmeticOverflow),
        GraphBuildError::NodeBlock(GraphNodeError::ArithmeticOverflow)
    ));
    assert!(matches!(
        GraphBuildError::from(StoreError::ReadOnly),
        GraphBuildError::Store(StoreError::ReadOnly)
    ));

    assert!(matches!(
        FilteredSearchError::from(PlanError::UnknownColumn(ColumnId::new(9))),
        FilteredSearchError::Plan(PlanError::UnknownColumn(column))
            if column == ColumnId::new(9)
    ));
    assert!(matches!(
        FilteredSearchError::from(QueryError::Cancelled { partial: false }),
        FilteredSearchError::Query(QueryError::Cancelled { partial: false })
    ));
    assert!(matches!(
        PurgeError::from(StoreError::ReadOnly),
        PurgeError::Store(StoreError::ReadOnly)
    ));
    assert!(matches!(
        IngestError::from(StoreError::ReadOnly),
        IngestError::Store(StoreError::ReadOnly)
    ));
    assert!(matches!(
        StoreLexicalError::from(QueryError::Timeout { partial: false }),
        StoreLexicalError::Query(QueryError::Timeout { partial: false })
    ));
    assert!(matches!(
        StoreLexicalError::from(StoreError::ReadOnly),
        StoreLexicalError::Query(QueryError::Store(StoreError::ReadOnly))
    ));
    assert!(matches!(
        StoreLexicalError::from(LexicalFilterError::SegmentCount {
            expected: 2,
            actual: 1,
        }),
        StoreLexicalError::Lexical(LexicalFilterError::SegmentCount {
            expected: 2,
            actual: 1,
        })
    ));
}
