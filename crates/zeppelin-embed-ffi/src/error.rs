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
        Self::new(Self::store_kind_code(error.kind()), message)
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
        use zeppelin_embed::ingest::IngestError;

        let message = error.to_string();
        let code = match error {
            IngestError::Store(error) => Self::store(error).code,
            IngestError::EmptyBatch => ZeErrorCode::ZeErrEmptyBatch,
            IngestError::EpochMismatch(_) => ZeErrorCode::ZeErrEpochMismatch,
            IngestError::EpochUndeclared => ZeErrorCode::ZeErrEpochUndeclared,
            IngestError::EpochUnstamped => ZeErrorCode::ZeErrEpochUnstamped,
            IngestError::StaleRevision { .. } => ZeErrorCode::ZeErrStaleRevision,
            IngestError::Vector(_)
            | IngestError::Lexical(_)
            | IngestError::Tokenizer(_)
            | IngestError::Columns(_)
            | IngestError::Payload(_) => ZeErrorCode::ZeErrInvalidArgument,
        };
        Self::new(code, message)
    }

    pub(crate) fn query(error: zeppelin_embed::lifecycle::QueryError) -> Self {
        use zeppelin_embed::lifecycle::QueryError;

        let message = error.to_string();
        let code = match error {
            QueryError::Timeout { .. } => ZeErrorCode::ZeErrTimeout,
            QueryError::Cancelled { .. } | QueryError::ReadCancelled { .. } => {
                ZeErrorCode::ZeErrCancelled
            }
            QueryError::Store(error) => Self::store(error).code,
            QueryError::Scan(_) => ZeErrorCode::ZeErrInvalidArgument,
            QueryError::Graph(_) => ZeErrorCode::ZeErrCorrupt,
        };
        Self::new(code, message)
    }

    pub(crate) fn purge(error: zeppelin_embed::ingest::PurgeError) -> Self {
        use zeppelin_embed::ingest::PurgeError;

        let message = error.to_string();
        let code = match error {
            PurgeError::Store(error) => Self::store(error).code,
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

    pub(crate) fn maintenance(error: zeppelin_embed::tier::MaintenanceError) -> Self {
        use zeppelin_embed::tier::MaintenanceError;

        let message = error.to_string();
        let code = match error {
            MaintenanceError::Store(error) => Self::store(error).code,
            MaintenanceError::Parameters(_) => ZeErrorCode::ZeErrInvalidArgument,
            MaintenanceError::Graph(_) => ZeErrorCode::ZeErrCorrupt,
            MaintenanceError::Consolidate(_) => ZeErrorCode::ZeErrCorrupt,
            MaintenanceError::Deadline(_) => ZeErrorCode::ZeErrInvalidArgument,
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
}
