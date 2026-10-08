use crate::abi::ZeErrorCode;
use crate::marshal::MarshalError;

#[derive(Debug)]
pub(crate) struct FfiError {
    pub(crate) code: ZeErrorCode,
    pub(crate) message: String,
}

impl FfiError {
    #[cfg(feature = "text")]
    pub(crate) fn text(error: zeppelin_embed_text::TextError) -> Self {
        use zeppelin_embed_text::TextError;

        let message = error.to_string();
        let code = match error {
            TextError::Bundle(_) => ZeErrorCode::ZeErrBundle,
            TextError::Runtime(_) => ZeErrorCode::ZeErrModel,
            TextError::Pipeline { .. } => ZeErrorCode::ZeErrPipeline,
            TextError::DimsMismatch { .. } => ZeErrorCode::ZeErrDimensionMismatch,
            TextError::NonUnitVector | TextError::InvalidInput(_) => {
                ZeErrorCode::ZeErrInvalidArgument
            }
            TextError::Ingest(error) => Self::ingest(error).code,
            TextError::Seal(error) | TextError::Store(error) => Self::store(error).code,
            TextError::Query(error) => Self::query(error).code,
            TextError::Lexical(error) => Self::lexical(error).code,
            TextError::Hybrid(error) => Self::fusion(error).code,
            TextError::Materialization(error) => Self::materialization(error).code,
        };
        Self::new(code, message)
    }

    pub(crate) fn materialization(error: zeppelin_embed::lifecycle::MaterializationError) -> Self {
        use zeppelin_embed::lifecycle::MaterializationError;

        let message = error.to_string();
        let code = match error {
            MaterializationError::Query(error) => Self::query(error).code,
            MaterializationError::Storage(error) => Self::store(error).code,
            MaterializationError::MissingText { .. } => ZeErrorCode::ZeErrNotFound,
            MaterializationError::AllocationFailed { .. } => ZeErrorCode::ZeErrOutOfMemory,
            MaterializationError::RankOutOfRange { .. }
            | MaterializationError::ArithmeticOverflow => ZeErrorCode::ZeErrInternal,
            MaterializationError::MissingIdentity { .. }
            | MaterializationError::MissingSource { .. }
            | MaterializationError::InvalidLexicalSource { .. }
            | MaterializationError::MissingFusedIdentity { .. }
            | MaterializationError::IdentityMismatch { .. } => ZeErrorCode::ZeErrCorrupt,
        };
        Self::new(code, message)
    }

    pub(crate) fn new(code: ZeErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(ZeErrorCode::ZeErrInvalidArgument, message)
    }

    pub(crate) fn store(error: zeppelin_embed::lifecycle::StoreError) -> Self {
        let message = error.to_string();
        if let Some(code) = format_version_code(&error) {
            return Self::new(code, message);
        }
        Self::new(Self::store_code(&error), message)
    }

    fn store_code(error: &zeppelin_embed::lifecycle::StoreError) -> ZeErrorCode {
        if let Some(code) = format_version_code(error) {
            return code;
        }
        match error {
            zeppelin_embed::lifecycle::StoreError::Manifest(
                zeppelin_embed::manifest::ManifestError::GraphUnsupportedBuild,
            ) => ZeErrorCode::ZeErrGraphUnsupportedBuild,
            zeppelin_embed::lifecycle::StoreError::NativeGraphDirectory { .. } => {
                ZeErrorCode::ZeErrLegacyGraphDirectory
            }
            #[cfg(feature = "graph-cypher")]
            zeppelin_embed::lifecycle::StoreError::DocumentMutation(error) => {
                Self::ingest_code(error)
            }
            zeppelin_embed::lifecycle::StoreError::CascadeCycle { .. } => {
                ZeErrorCode::ZeErrCascadeCycle
            }
            zeppelin_embed::lifecycle::StoreError::SchemaMismatch { .. } => {
                ZeErrorCode::ZeErrSchemaMismatch
            }
            zeppelin_embed::lifecycle::StoreError::ScanStale { .. } => ZeErrorCode::ZeErrScanStale,
            _ => Self::store_kind_code(error.kind()),
        }
    }

    pub(crate) const fn store_kind_code(
        kind: zeppelin_embed::lifecycle::StoreErrorKind,
    ) -> ZeErrorCode {
        use zeppelin_embed::lifecycle::StoreErrorKind;

        match kind {
            StoreErrorKind::Io => ZeErrorCode::ZeErrIo,
            StoreErrorKind::InvalidArgument => ZeErrorCode::ZeErrInvalidArgument,
            StoreErrorKind::StoreBusy => ZeErrorCode::ZeErrStoreBusy,
            StoreErrorKind::Unsupported => ZeErrorCode::ZeErrUnsupported,
            StoreErrorKind::Corrupt => ZeErrorCode::ZeErrCorrupt,
            StoreErrorKind::BudgetExceeded => ZeErrorCode::ZeErrBudgetExceeded,
            StoreErrorKind::OutOfMemory => ZeErrorCode::ZeErrOutOfMemory,
            StoreErrorKind::DimensionMismatch => ZeErrorCode::ZeErrDimensionMismatch,
            StoreErrorKind::EpochMismatch => ZeErrorCode::ZeErrEpochMismatch,
            StoreErrorKind::EpochUndeclared => ZeErrorCode::ZeErrEpochUndeclared,
            StoreErrorKind::EpochUnstamped => ZeErrorCode::ZeErrEpochUnstamped,
            StoreErrorKind::Internal => ZeErrorCode::ZeErrInternal,
            StoreErrorKind::EmptyBatch => ZeErrorCode::ZeErrEmptyBatch,
            StoreErrorKind::Cancelled => ZeErrorCode::ZeErrCancelled,
            StoreErrorKind::ReadOnly => ZeErrorCode::ZeErrAccessMode,
            StoreErrorKind::Closing => ZeErrorCode::ZeErrClosing,
            StoreErrorKind::Closed => ZeErrorCode::ZeErrClosed,
            StoreErrorKind::Panic => ZeErrorCode::ZeErrPanic,
            StoreErrorKind::Synchronization => ZeErrorCode::ZeErrSynchronization,
        }
    }

    pub(crate) fn ingest(error: zeppelin_embed::ingest::IngestError) -> Self {
        Self::new(Self::ingest_code(&error), error.to_string())
    }

    fn ingest_code(error: &zeppelin_embed::ingest::IngestError) -> ZeErrorCode {
        use zeppelin_embed::ingest::IngestError;
        match error {
            #[cfg(feature = "graph-cypher")]
            IngestError::Graph(error) => {
                use zeppelin_embed::property_graph::query::completed::GraphQueryErrorKind as Kind;
                match error.kind() {
                    Kind::Constraint => ZeErrorCode::ZeErrKeyConflict,
                    Kind::Limit => ZeErrorCode::ZeErrBudgetExceeded,
                    Kind::Cancelled => ZeErrorCode::ZeErrCancelled,
                    Kind::Timeout => ZeErrorCode::ZeErrTimeout,
                    Kind::Closed => ZeErrorCode::ZeErrClosed,
                    Kind::Corruption => ZeErrorCode::ZeErrCorrupt,
                    Kind::Storage => ZeErrorCode::ZeErrIo,
                    Kind::WriteIndeterminate => ZeErrorCode::ZeErrIndeterminateCommit,
                    Kind::Unavailable => ZeErrorCode::ZeErrAccessMode,
                    Kind::InvalidPlan | Kind::Parameter | Kind::Expression => {
                        ZeErrorCode::ZeErrInvalidArgument
                    }
                    _ => ZeErrorCode::ZeErrInternal,
                }
            }
            IngestError::Store(error) => Self::store_code(error),
            IngestError::EmptyBatch => ZeErrorCode::ZeErrEmptyBatch,
            IngestError::EpochMismatch(_) => ZeErrorCode::ZeErrEpochMismatch,
            IngestError::EpochUndeclared => ZeErrorCode::ZeErrEpochUndeclared,
            IngestError::EpochUnstamped => ZeErrorCode::ZeErrEpochUnstamped,
            IngestError::StaleRevision { .. } => ZeErrorCode::ZeErrStaleRevision,
            IngestError::RevisionConflict { .. } => ZeErrorCode::ZeErrRevisionConflict,
            IngestError::Vector(_)
            | IngestError::Lexical(_)
            | IngestError::Tokenizer(_)
            | IngestError::Columns(_)
            | IngestError::Payload(_) => ZeErrorCode::ZeErrInvalidArgument,
        }
    }

    pub(crate) fn query(error: zeppelin_embed::lifecycle::QueryError) -> Self {
        use zeppelin_embed::lifecycle::QueryError;
        use zeppelin_embed::scan::ScanError;

        let message = error.to_string();
        let code = match error {
            QueryError::Timeout { .. } => ZeErrorCode::ZeErrTimeout,
            QueryError::Cancelled { .. } | QueryError::ReadCancelled { .. } => {
                ZeErrorCode::ZeErrCancelled
            }
            QueryError::Store(error) => Self::store(error).code,
            // A scan that stopped on its deadline or on a cancel token is a
            // control outcome, not a malformed request (ZE-178).
            QueryError::Scan(ScanError::Timeout { .. }) => ZeErrorCode::ZeErrTimeout,
            QueryError::Scan(ScanError::Cancelled { .. } | ScanError::ReadCancelled { .. }) => {
                ZeErrorCode::ZeErrCancelled
            }
            QueryError::Scan(_) => ZeErrorCode::ZeErrInvalidArgument,
            QueryError::Graph(_) => ZeErrorCode::ZeErrCorrupt,
        };
        Self::new(code, message)
    }

    pub(crate) fn filtered(error: zeppelin_embed::planner::FilteredSearchError) -> Self {
        use zeppelin_embed::planner::FilteredSearchError;

        let message = error.to_string();
        #[allow(unreachable_patterns)]
        let code = match error {
            FilteredSearchError::Plan(_) => ZeErrorCode::ZeErrInvalidArgument,
            FilteredSearchError::Query(error) => Self::query(error).code,
            FilteredSearchError::ActiveMetadata(_)
            | FilteredSearchError::PlanReportMismatch { .. }
            | FilteredSearchError::InvalidPlanNode(_) => ZeErrorCode::ZeErrInternal,
            _ => ZeErrorCode::ZeErrInternal,
        };
        Self::new(code, message)
    }

    pub(crate) fn purge(error: zeppelin_embed::ingest::PurgeError) -> Self {
        use zeppelin_embed::ingest::PurgeError;

        let message = error.to_string();
        let code = match error {
            PurgeError::Store(error) => Self::store(error).code,
            #[cfg(feature = "graph-cypher")]
            PurgeError::Delete(error) => Self::ingest(error).code,
            PurgeError::InsufficientTempSpace { .. } => ZeErrorCode::ZeErrBudgetExceeded,
            PurgeError::PurgeInProgress => ZeErrorCode::ZeErrBusy,
            PurgeError::UnknownToken { .. } => ZeErrorCode::ZeErrInvalidArgument,
            PurgeError::IntentFormat(_)
            | PurgeError::IntentDecode(_)
            | PurgeError::WalRewriteWouldDropAcked { .. } => ZeErrorCode::ZeErrCorrupt,
            PurgeError::WalPayload(_) => ZeErrorCode::ZeErrInvalidArgument,
        };
        Self::new(code, message)
    }

    pub(crate) fn delete_matching(error: zeppelin_embed::ingest::DeleteMatchingError) -> Self {
        use zeppelin_embed::ingest::DeleteMatchingError;

        let message = error.to_string();
        let code = match error {
            DeleteMatchingError::Predicate(_) => ZeErrorCode::ZeErrInvalidArgument,
            DeleteMatchingError::Query(error) => Self::query(error).code,
            DeleteMatchingError::Delete(error) => Self::ingest(error).code,
            DeleteMatchingError::Purge(error) => Self::purge(error).code,
        };
        Self::new(code, message)
    }

    pub(crate) fn maintenance(error: zeppelin_embed::tier::MaintenanceError) -> Self {
        use zeppelin_embed::tier::MaintenanceError;

        let message = error.to_string();
        let code = match error {
            MaintenanceError::Store(error) => Self::store(error).code,
            MaintenanceError::Parameters(_) => ZeErrorCode::ZeErrInvalidArgument,
            MaintenanceError::Graph(_) => ZeErrorCode::ZeErrCorrupt,
            MaintenanceError::Consolidate(_) => ZeErrorCode::ZeErrCorrupt,
            MaintenanceError::Deadline(_) => ZeErrorCode::ZeErrInvalidArgument,
            #[cfg(feature = "graph-cypher")]
            MaintenanceError::PropertyGraph(error) => {
                crate::graph_abi::store_error(&error, false).code
            }
            MaintenanceError::ArithmeticOverflow => ZeErrorCode::ZeErrInternal,
        };
        Self::new(code, message)
    }
}

impl FfiError {
    pub(crate) fn lexical(error: zeppelin_embed::ingest::StoreLexicalError) -> Self {
        use zeppelin_embed::ingest::StoreLexicalError;

        let message = error.to_string();
        let code = match error {
            StoreLexicalError::Query(error) => Self::query(error).code,
            StoreLexicalError::Lexical(_)
            | StoreLexicalError::Structured(_)
            | StoreLexicalError::Snippet(_)
            | StoreLexicalError::Tokenizer(_) => ZeErrorCode::ZeErrInvalidArgument,
            StoreLexicalError::MissingDocumentIdentity { .. } => ZeErrorCode::ZeErrCorrupt,
            StoreLexicalError::MissingStoredText { .. } => ZeErrorCode::ZeErrNotFound,
            StoreLexicalError::MissingSnippetMatch { .. } => ZeErrorCode::ZeErrInternal,
        };
        Self::new(code, message)
    }

    pub(crate) fn fusion(error: zeppelin_embed::fusion::FusionError) -> Self {
        use zeppelin_embed::fusion::FusionError;

        let message = error.to_string();
        let code = match error {
            FusionError::InvalidAlpha(_) => ZeErrorCode::ZeErrInvalidArgument,
            FusionError::EstimatedVectorScore { .. } => ZeErrorCode::ZeErrUnsupported,
            FusionError::Timeout { .. } => ZeErrorCode::ZeErrTimeout,
            FusionError::Cancelled { .. } | FusionError::ReadCancelled { .. } => {
                ZeErrorCode::ZeErrCancelled
            }
            FusionError::LegThreadStart { .. } => ZeErrorCode::ZeErrInternal,
            FusionError::LegPanic { .. } => ZeErrorCode::ZeErrPanic,
            FusionError::Leg { kind, .. } => Self::leg_kind_code(kind),
            FusionError::NonFiniteScore { .. }
            | FusionError::NegativeScore { .. }
            | FusionError::InvalidBounds { .. }
            | FusionError::UnrankedInput { .. }
            | FusionError::MissingDocumentIdentity { .. }
            | FusionError::DuplicateDocumentIdentity { .. } => ZeErrorCode::ZeErrInternal,
        };
        Self::new(code, message)
    }

    const fn leg_kind_code(kind: zeppelin_embed::fusion::LegFailureKind) -> ZeErrorCode {
        use zeppelin_embed::fusion::LegFailureKind;

        match kind {
            LegFailureKind::Store(kind) => Self::store_kind_code(kind),
            LegFailureKind::Scan | LegFailureKind::Lexical => ZeErrorCode::ZeErrInvalidArgument,
            LegFailureKind::Graph | LegFailureKind::Segment => ZeErrorCode::ZeErrCorrupt,
            LegFailureKind::Invariant | LegFailureKind::Caller => ZeErrorCode::ZeErrInternal,
        }
    }

    pub(crate) fn epoch_transition(error: zeppelin_embed::epoch::EpochTransitionError) -> Self {
        use zeppelin_embed::epoch::EpochTransitionError;

        let message = error.to_string();
        let code = match error {
            EpochTransitionError::Store(error) => Self::store(error).code,
            EpochTransitionError::EpochUnavailable { .. }
            | EpochTransitionError::EmbeddingEpochUnavailable { .. } => ZeErrorCode::ZeErrNotFound,
            EpochTransitionError::IncompleteEpoch { .. } => ZeErrorCode::ZeErrEpochIncomplete,
            EpochTransitionError::PublishedEpoch { .. } => ZeErrorCode::ZeErrEpochPublished,
            EpochTransitionError::UnsealedWrites { .. } => ZeErrorCode::ZeErrUnsealedWrites,
            EpochTransitionError::MissingDocumentIdentity { .. } => ZeErrorCode::ZeErrCorrupt,
            EpochTransitionError::GraphEpochTransition => ZeErrorCode::ZeErrGraphEpochTransition,
            EpochTransitionError::ReclaimedBytesOverflow => ZeErrorCode::ZeErrInternal,
        };
        Self::new(code, message)
    }

    pub(crate) fn tokenizer(error: zeppelin_embed::fts::tokenizer::TokenizerError) -> Self {
        Self::invalid(error.to_string())
    }
}

impl From<MarshalError> for FfiError {
    fn from(error: MarshalError) -> Self {
        Self::invalid(error.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeppelin_embed::fusion::{FusionError, FusionLeg, LegFailureKind};
    use zeppelin_embed::lifecycle::StoreErrorKind;

    #[cfg(feature = "text")]
    #[test]
    #[allow(clippy::expect_used)]
    fn astra_07_materialization_failures_keep_typed_ffi_codes() {
        use zeppelin_embed::ingest::{
            DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
        };
        use zeppelin_embed::lifecycle::{
            CancelToken, MaterializationError as Error, OpenOptions, QueryControl, QueryError,
            SearchOptions, SearchTier, Store, StoreError,
        };
        let directory = tempfile::tempdir().expect("mapping fixture");
        let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
        let document = DocumentVersion::new(DocId::new(7), Revision::new(1));
        store
            .ingest(IngestBatch::new(vec![IngestDocument::new(
                document,
                vec![1.0, 0.0],
            )]))
            .expect("ingest");
        let outcome = store
            .search(
                SearchRequest::new(&[1.0, 0.0]),
                1,
                SearchOptions::default().with_tier(SearchTier::Exact),
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect("ranked address");
        let row_id = outcome.candidates.first().expect("hit").row_id();
        for (error, code) in [
            (
                Error::MissingText { row_id, document },
                ZeErrorCode::ZeErrNotFound,
            ),
            (Error::MissingIdentity { row_id }, ZeErrorCode::ZeErrCorrupt),
            (Error::MissingSource { row_id }, ZeErrorCode::ZeErrCorrupt),
            (
                Error::InvalidLexicalSource { source: 9 },
                ZeErrorCode::ZeErrCorrupt,
            ),
            (
                Error::MissingFusedIdentity {
                    document: document.doc_id(),
                },
                ZeErrorCode::ZeErrCorrupt,
            ),
            (
                Error::IdentityMismatch {
                    row_id,
                    expected: document,
                    actual: None,
                },
                ZeErrorCode::ZeErrCorrupt,
            ),
            (
                Error::AllocationFailed { bytes: 10 },
                ZeErrorCode::ZeErrOutOfMemory,
            ),
            (
                Error::RankOutOfRange {
                    rank: 1,
                    returned: 1,
                },
                ZeErrorCode::ZeErrInternal,
            ),
            (Error::ArithmeticOverflow, ZeErrorCode::ZeErrInternal),
            (
                Error::Query(QueryError::Timeout { partial: false }),
                ZeErrorCode::ZeErrTimeout,
            ),
            (
                Error::Query(QueryError::Cancelled { partial: false }),
                ZeErrorCode::ZeErrCancelled,
            ),
            (
                Error::Query(QueryError::ReadCancelled { partial: false }),
                ZeErrorCode::ZeErrCancelled,
            ),
            (Error::Storage(StoreError::Closed), ZeErrorCode::ZeErrClosed),
        ] {
            let message = error.to_string();
            let mapped = FfiError::text(zeppelin_embed_text::TextError::Materialization(error));
            assert_eq!(mapped.code, code);
            assert_eq!(mapped.message, message);
        }
        store.close().expect("close");
    }

    #[test]
    fn a_hybrid_leg_failure_keeps_its_store_classification() {
        let closing = FfiError::fusion(FusionError::Leg {
            leg: FusionLeg::Vector,
            kind: LegFailureKind::Store(StoreErrorKind::Closing),
            detail: "store is closing".to_owned(),
        });
        assert_eq!(closing.code, ZeErrorCode::ZeErrClosing);
        let lexical = FfiError::fusion(FusionError::Leg {
            leg: FusionLeg::Lexical,
            kind: LegFailureKind::Lexical,
            detail: "bad terms".to_owned(),
        });
        assert_eq!(lexical.code, ZeErrorCode::ZeErrInvalidArgument);
    }

    /// ZE-181. A leg stopped mid-flight reports its stop as a `ScanError`,
    /// which fuses through `FusionError` rather than through
    /// `FfiError::query`. That path must reach the same typed codes ZE-178
    /// pinned on the top-level one, so a hybrid query that runs out of time
    /// never looks like a malformed request. Every other `ScanError` stays a
    /// `Leg` failure and keeps `ZE_ERR_INVALID_ARGUMENT`.
    #[test]
    fn a_mid_flight_leg_stop_keeps_its_typed_code_through_fusion() {
        use zeppelin_embed::lifecycle::QueryError;
        use zeppelin_embed::scan::ScanError;

        let fused = |error| FfiError::fusion(FusionError::from(QueryError::Scan(error)));

        let timeout = fused(ScanError::Timeout { partial: false });
        assert_eq!(timeout.code, ZeErrorCode::ZeErrTimeout);
        assert_eq!(
            timeout.message,
            "hybrid query deadline expired (partial=false)"
        );

        let cancelled = fused(ScanError::Cancelled { partial: false });
        assert_eq!(cancelled.code, ZeErrorCode::ZeErrCancelled);
        assert_eq!(
            cancelled.message,
            "hybrid query was cancelled (partial=false)"
        );

        let read_cancelled = fused(ScanError::ReadCancelled { partial: false });
        assert_eq!(read_cancelled.code, ZeErrorCode::ZeErrCancelled);
        assert_eq!(
            read_cancelled.message,
            "store close cancelled hybrid query (partial=false)"
        );

        // A genuinely malformed scan request is not a control outcome.
        let malformed = fused(ScanError::ZeroDimension);
        assert_eq!(malformed.code, ZeErrorCode::ZeErrInvalidArgument);
        let overflow = fused(ScanError::ArithmeticOverflow);
        assert_eq!(overflow.code, ZeErrorCode::ZeErrInvalidArgument);
    }

    #[test]
    fn contained_hybrid_panics_keep_the_frozen_panic_code() {
        let panic = FfiError::fusion(FusionError::LegPanic {
            leg: FusionLeg::Lexical,
            detail: "lexical hybrid leg panicked",
        });
        assert_eq!(panic.code, ZeErrorCode::ZeErrPanic);
        let start = FfiError::fusion(FusionError::LegThreadStart {
            leg: FusionLeg::Lexical,
            detail: "thread unavailable".to_owned(),
        });
        assert_eq!(start.code, ZeErrorCode::ZeErrInternal);
    }

    #[test]
    fn every_store_error_kind_has_its_frozen_abi_code() {
        use StoreErrorKind as Kind;

        let cases = [
            (Kind::Io, ZeErrorCode::ZeErrIo),
            (Kind::InvalidArgument, ZeErrorCode::ZeErrInvalidArgument),
            (Kind::StoreBusy, ZeErrorCode::ZeErrStoreBusy),
            (Kind::Unsupported, ZeErrorCode::ZeErrUnsupported),
            (Kind::Corrupt, ZeErrorCode::ZeErrCorrupt),
            (Kind::BudgetExceeded, ZeErrorCode::ZeErrBudgetExceeded),
            (Kind::OutOfMemory, ZeErrorCode::ZeErrOutOfMemory),
            (Kind::DimensionMismatch, ZeErrorCode::ZeErrDimensionMismatch),
            (Kind::EpochMismatch, ZeErrorCode::ZeErrEpochMismatch),
            (Kind::EpochUndeclared, ZeErrorCode::ZeErrEpochUndeclared),
            (Kind::EpochUnstamped, ZeErrorCode::ZeErrEpochUnstamped),
            (Kind::Internal, ZeErrorCode::ZeErrInternal),
            (Kind::EmptyBatch, ZeErrorCode::ZeErrEmptyBatch),
            (Kind::Cancelled, ZeErrorCode::ZeErrCancelled),
            (Kind::ReadOnly, ZeErrorCode::ZeErrAccessMode),
            (Kind::Closing, ZeErrorCode::ZeErrClosing),
            (Kind::Closed, ZeErrorCode::ZeErrClosed),
            (Kind::Panic, ZeErrorCode::ZeErrPanic),
            (Kind::Synchronization, ZeErrorCode::ZeErrSynchronization),
        ];
        for (kind, expected) in cases {
            assert_eq!(FfiError::store_kind_code(kind), expected);
        }
    }

    /// ZE-178. A scan that stopped on its deadline or on a cancel token is a
    /// control outcome, not a malformed request, so `ze_query` reports the
    /// same codes `ze_search` and `ze_scan` already report. Every other
    /// `ScanError` stays `ZE_ERR_INVALID_ARGUMENT`, and no message changes.
    #[test]
    fn scan_control_outcomes_keep_their_typed_codes_through_query() {
        use zeppelin_embed::lifecycle::QueryError;
        use zeppelin_embed::scan::ScanError;

        let timeout = FfiError::query(QueryError::Scan(ScanError::Timeout { partial: false }));
        assert_eq!(timeout.code, ZeErrorCode::ZeErrTimeout);
        assert_eq!(timeout.message, "scan deadline expired (partial=false)");

        let cancelled = FfiError::query(QueryError::Scan(ScanError::Cancelled { partial: false }));
        assert_eq!(cancelled.code, ZeErrorCode::ZeErrCancelled);
        assert_eq!(cancelled.message, "scan was cancelled (partial=false)");

        let read_cancelled = FfiError::query(QueryError::Scan(ScanError::ReadCancelled {
            partial: false,
        }));
        assert_eq!(read_cancelled.code, ZeErrorCode::ZeErrCancelled);
        assert_eq!(
            read_cancelled.message,
            "store close cancelled scan (partial=false)"
        );

        let malformed = FfiError::query(QueryError::Scan(ScanError::ZeroDimension));
        assert_eq!(malformed.code, ZeErrorCode::ZeErrInvalidArgument);
        assert_eq!(malformed.message, "scan dimension must not be zero");

        let overflow = FfiError::query(QueryError::Scan(ScanError::ArithmeticOverflow));
        assert_eq!(overflow.code, ZeErrorCode::ZeErrInvalidArgument);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod format_version_tests {
    use super::*;
    #[test]
    fn old_and_new_manifest_versions_have_specific_codes() {
        for (version, expected) in [(1_u16, "ZeErrFormatVersion"), (4, "ZeErrFormatTooNew")] {
            let mut bytes = vec![0_u8; 32];
            bytes.get_mut(..8).unwrap().copy_from_slice(b"ZEPEMBED");
            bytes
                .get_mut(8..10)
                .unwrap()
                .copy_from_slice(&10_u16.to_le_bytes());
            bytes
                .get_mut(10..12)
                .unwrap()
                .copy_from_slice(&version.to_le_bytes());
            let error =
                zeppelin_embed::manifest::decode_manifest("manifest.ze", &bytes).unwrap_err();
            let error = FfiError::store(zeppelin_embed::lifecycle::StoreError::Manifest(error));
            assert_eq!(format!("{:?}", error.code), expected);
        }
    }
}

fn format_version_code(error: &(dyn std::error::Error + 'static)) -> Option<ZeErrorCode> {
    use zeppelin_embed::format::frame::FormatError;
    use zeppelin_embed::wal::header::WalHeaderError;
    let range = error
        .downcast_ref::<FormatError>()
        .and_then(FormatError::version_range)
        .or_else(|| match error.downcast_ref::<WalHeaderError>() {
            Some(WalHeaderError::UnsupportedVersion {
                version,
                minimum,
                maximum,
                ..
            }) => Some((*version, *minimum, *maximum)),
            _ => None,
        });
    if let Some((found, _, maximum)) = range {
        return Some(if found > maximum {
            ZeErrorCode::ZeErrFormatTooNew
        } else {
            ZeErrorCode::ZeErrFormatVersion
        });
    }
    error.source().and_then(format_version_code)
}

#[cfg(test)]
mod legacy_graph_dir {
    #[test]
    fn legacy_graph_directory_has_a_stable_error_code() {
        let error = super::FfiError::store(
            zeppelin_embed::lifecycle::StoreError::NativeGraphDirectory {
                path: std::path::PathBuf::from("graph"),
            },
        );
        assert_eq!(error.code as i32, 58);
    }
}
