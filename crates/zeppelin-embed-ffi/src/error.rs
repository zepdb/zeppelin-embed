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
        Self::new(ZeErrorCode::InvalidArgument, message)
    }

    pub(crate) fn store(error: zeppelin_embed::lifecycle::StoreError) -> Self {
        use zeppelin_embed::lifecycle::StoreError;

        let message = error.to_string();
        let code = match &error {
            StoreError::Io { .. }
            | StoreError::Lock(_)
            | StoreError::Statistics { .. }
            | StoreError::BackgroundStart { .. }
            | StoreError::QueryPoolStart { .. } => ZeErrorCode::Io,
            StoreError::NotDirectory { .. } | StoreError::SchemaMismatch { .. } => {
                ZeErrorCode::InvalidArgument
            }
            StoreError::StoreBusy { .. } => ZeErrorCode::StoreBusy,
            StoreError::Durability(_)
            | StoreError::GraphUnavailable { .. }
            | StoreError::UnsupportedWalMutation { .. } => ZeErrorCode::Unsupported,
            StoreError::Manifest(_)
            | StoreError::Segment(_)
            | StoreError::Wal(_)
            | StoreError::WalRecovery(_)
            | StoreError::WalRecord { .. }
            | StoreError::WalMutation { .. }
            | StoreError::WalRevisionOrder { .. }
            | StoreError::WalVector { .. }
            | StoreError::PurgeRecovery { .. } => ZeErrorCode::Corrupt,
            StoreError::WalWrite(_) | StoreError::WalRetire(_) => ZeErrorCode::Io,
            StoreError::BudgetExceeded { .. } => ZeErrorCode::BudgetExceeded,
            StoreError::AllocationFailed { .. } => ZeErrorCode::OutOfMemory,
            StoreError::DimensionMismatch { .. } => ZeErrorCode::DimensionMismatch,
            StoreError::EpochMismatch(_) => ZeErrorCode::EpochMismatch,
            StoreError::EpochUndeclared => ZeErrorCode::EpochUndeclared,
            StoreError::EpochUnstamped => ZeErrorCode::EpochUnstamped,
            StoreError::ActiveRowOverflow
            | StoreError::GenerationOverflow
            | StoreError::PartitionBytesOverflow
            | StoreError::ForeignPreparedSegment
            | StoreError::BackgroundHandshake
            | StoreError::QueryPoolHandshake
            | StoreError::QueryPoolCapacity { .. } => ZeErrorCode::Internal,
            StoreError::EmptyActiveSegment => ZeErrorCode::EmptyBatch,
            StoreError::SealCancelled | StoreError::ReadCancelled => ZeErrorCode::Cancelled,
            StoreError::ReadOnly => ZeErrorCode::AccessMode,
            StoreError::Closing => ZeErrorCode::Closing,
            StoreError::Closed => ZeErrorCode::Closed,
            StoreError::BackgroundThreadPanicked | StoreError::QueryPoolThreadPanicked => {
                ZeErrorCode::Panic
            }
            StoreError::Synchronization { .. } => ZeErrorCode::Synchronization,
        };
        Self::new(code, message)
    }

    pub(crate) fn ingest(error: zeppelin_embed::ingest::IngestError) -> Self {
        use zeppelin_embed::ingest::IngestError;

        let message = error.to_string();
        let code = match error {
            IngestError::Store(error) => Self::store(error).code,
            IngestError::EmptyBatch => ZeErrorCode::EmptyBatch,
            IngestError::EpochMismatch(_) => ZeErrorCode::EpochMismatch,
            IngestError::EpochUndeclared => ZeErrorCode::EpochUndeclared,
            IngestError::EpochUnstamped => ZeErrorCode::EpochUnstamped,
            IngestError::StaleRevision { .. } => ZeErrorCode::StaleRevision,
            IngestError::Vector(_)
            | IngestError::Lexical(_)
            | IngestError::Tokenizer(_)
            | IngestError::Columns(_)
            | IngestError::Payload(_) => ZeErrorCode::InvalidArgument,
        };
        Self::new(code, message)
    }

    pub(crate) fn query(error: zeppelin_embed::lifecycle::QueryError) -> Self {
        use zeppelin_embed::lifecycle::QueryError;

        let message = error.to_string();
        let code = match error {
            QueryError::Timeout { .. } => ZeErrorCode::Timeout,
            QueryError::Cancelled { .. } | QueryError::ReadCancelled { .. } => {
                ZeErrorCode::Cancelled
            }
            QueryError::Store(error) => Self::store(error).code,
            QueryError::Scan(_) => ZeErrorCode::InvalidArgument,
            QueryError::Graph(_) => ZeErrorCode::Corrupt,
        };
        Self::new(code, message)
    }

    pub(crate) fn purge(error: zeppelin_embed::ingest::PurgeError) -> Self {
        use zeppelin_embed::ingest::PurgeError;

        let message = error.to_string();
        let code = match error {
            PurgeError::Store(error) => Self::store(error).code,
            PurgeError::InsufficientTempSpace { .. } => ZeErrorCode::BudgetExceeded,
            PurgeError::PurgeInProgress => ZeErrorCode::Busy,
            PurgeError::UnknownToken { .. } => ZeErrorCode::InvalidArgument,
            PurgeError::IntentFormat(_) | PurgeError::IntentDecode(_) => ZeErrorCode::Corrupt,
            PurgeError::WalPayload(_) => ZeErrorCode::InvalidArgument,
        };
        Self::new(code, message)
    }

    pub(crate) fn maintenance(error: zeppelin_embed::tier::MaintenanceError) -> Self {
        use zeppelin_embed::tier::MaintenanceError;

        let message = error.to_string();
        let code = match error {
            MaintenanceError::Store(error) => Self::store(error).code,
            MaintenanceError::Parameters(_) => ZeErrorCode::InvalidArgument,
            MaintenanceError::Graph(_) => ZeErrorCode::Corrupt,
            MaintenanceError::Deadline(_) => ZeErrorCode::InvalidArgument,
            MaintenanceError::ArithmeticOverflow => ZeErrorCode::Internal,
        };
        Self::new(code, message)
    }
}

impl From<MarshalError> for FfiError {
    fn from(error: MarshalError) -> Self {
        Self::invalid(error.0)
    }
}
