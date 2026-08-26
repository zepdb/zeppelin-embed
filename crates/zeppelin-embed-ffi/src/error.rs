use crate::abi::ZeErrorCode;
use crate::marshal::MarshalError;

#[derive(Debug)]
pub(crate) struct FfiError {
    pub(crate) code: ZeErrorCode,
    pub(crate) message: String,
}

impl FfiError {
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
        use zeppelin_embed::lifecycle::StoreError;

        let message = error.to_string();
        let code = match &error {
            StoreError::Io { .. }
            | StoreError::Lock(_)
            | StoreError::Statistics { .. }
            | StoreError::BackgroundStart { .. }
            | StoreError::QueryPoolStart { .. } => ZeErrorCode::ZeErrIo,
            StoreError::NotDirectory { .. } | StoreError::SchemaMismatch { .. } => {
                ZeErrorCode::ZeErrInvalidArgument
            }
            StoreError::StoreBusy { .. } => ZeErrorCode::ZeErrStoreBusy,
            StoreError::Durability(_)
            | StoreError::GraphUnavailable { .. }
            | StoreError::UnsupportedWalMutation { .. } => ZeErrorCode::ZeErrUnsupported,
            StoreError::Manifest(_)
            | StoreError::Segment(_)
            | StoreError::Wal(_)
            | StoreError::WalRecovery(_)
            | StoreError::WalRecord { .. }
            | StoreError::WalMutation { .. }
            | StoreError::WalRevisionOrder { .. }
            | StoreError::WalVector { .. }
            | StoreError::PurgeRecovery { .. } => ZeErrorCode::ZeErrCorrupt,
            StoreError::WalWrite(_) | StoreError::WalRetire(_) => ZeErrorCode::ZeErrIo,
            StoreError::BudgetExceeded { .. } => ZeErrorCode::ZeErrBudgetExceeded,
            StoreError::AllocationFailed { .. } => ZeErrorCode::ZeErrOutOfMemory,
            StoreError::DimensionMismatch { .. } => ZeErrorCode::ZeErrDimensionMismatch,
            StoreError::EpochMismatch(_) => ZeErrorCode::ZeErrEpochMismatch,
            StoreError::EpochUndeclared => ZeErrorCode::ZeErrEpochUndeclared,
            StoreError::EpochUnstamped => ZeErrorCode::ZeErrEpochUnstamped,
            StoreError::ActiveRowOverflow
            | StoreError::GenerationOverflow
            | StoreError::PartitionBytesOverflow
            | StoreError::ForeignPreparedSegment
            | StoreError::BackgroundHandshake
            | StoreError::QueryPoolHandshake
            | StoreError::QueryPoolCapacity { .. } => ZeErrorCode::ZeErrInternal,
            StoreError::EmptyActiveSegment => ZeErrorCode::ZeErrEmptyBatch,
            StoreError::SealCancelled | StoreError::ReadCancelled => ZeErrorCode::ZeErrCancelled,
            StoreError::ReadOnly => ZeErrorCode::ZeErrAccessMode,
            StoreError::Closing => ZeErrorCode::ZeErrClosing,
            StoreError::Closed => ZeErrorCode::ZeErrClosed,
            StoreError::BackgroundThreadPanicked | StoreError::QueryPoolThreadPanicked => {
                ZeErrorCode::ZeErrPanic
            }
            StoreError::Synchronization { .. } => ZeErrorCode::ZeErrSynchronization,
        };
        Self::new(code, message)
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
            PurgeError::IntentFormat(_) | PurgeError::IntentDecode(_) => ZeErrorCode::ZeErrCorrupt,
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
            StoreLexicalError::Lexical(_) => ZeErrorCode::ZeErrInvalidArgument,
            StoreLexicalError::MissingDocumentIdentity { .. } => ZeErrorCode::ZeErrCorrupt,
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
            // The engine flattens every other leg failure into a display
            // string, so the finer store classification is not recoverable.
            FusionError::NonFiniteScore { .. }
            | FusionError::NegativeScore { .. }
            | FusionError::UnrankedInput { .. }
            | FusionError::MissingDocumentIdentity { .. }
            | FusionError::DuplicateDocumentIdentity { .. }
            | FusionError::Leg { .. } => ZeErrorCode::ZeErrInternal,
        };
        Self::new(code, message)
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
