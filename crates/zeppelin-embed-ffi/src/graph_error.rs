//! Allocation-free component error mapping. No coordinator outcome is inferred.
use crate::ZeErrorCode;
use zeppelin_embed::property_graph::query::{QueryError, plan::PlanError};
use zeppelin_embed::property_graph::{CanonicalError, DomainError, KeyLifecycleError};

impl From<QueryError> for ZeErrorCode {
    fn from(error: QueryError) -> Self {
        match error {
            QueryError::Type => Self::ZeErrType,
            QueryError::PropertyLimit
            | QueryError::WorkLimit
            | QueryError::ListLimit
            | QueryError::ValueTooLarge => Self::ZeErrBudgetExceeded,
            QueryError::BufferTooSmall | QueryError::ForeignView => Self::ZeErrInvalidArgument,
            QueryError::Cancelled | QueryError::ReadCancelled => Self::ZeErrCancelled,
            QueryError::Timeout => Self::ZeErrTimeout,
            QueryError::Control => Self::ZeErrInternal,
            QueryError::ArithmeticOverflow => Self::ZeErrArithmeticOverflow,
            QueryError::ArithmeticDomain => Self::ZeErrArithmeticDomain,
            QueryError::DivisionByZero => Self::ZeErrDivisionByZero,
        }
    }
}
impl From<PlanError> for ZeErrorCode {
    fn from(error: PlanError) -> Self {
        match error {
            PlanError::Limit | PlanError::Footprint => Self::ZeErrBudgetExceeded,
            PlanError::Scope => Self::ZeErrScope,
            PlanError::Type => Self::ZeErrType,
            PlanError::Parameter => Self::ZeErrParameter,
            PlanError::ReadAfterWrite | PlanError::ReadWriteSearch => Self::ZeErrQueryUnsupported,
            PlanError::Reference
            | PlanError::Cycle
            | PlanError::Unreachable
            | PlanError::Arity
            | PlanError::Barrier
            | PlanError::Aggregate
            | PlanError::Search
            | PlanError::PathBound => Self::ZeErrInvalidArgument,
            PlanError::Control(error) => error.into(),
        }
    }
}
impl From<DomainError> for ZeErrorCode {
    fn from(error: DomainError) -> Self {
        match error {
            DomainError::RevisionOverflow => Self::ZeErrRevisionOverflow,
            DomainError::InputTooLarge | DomainError::ListTooLong => Self::ZeErrBudgetExceeded,
            DomainError::VectorDimensions => Self::ZeErrDimensionMismatch,
            DomainError::ZeroIdentity
            | DomainError::ZeroRevision
            | DomainError::InvalidUtf8
            | DomainError::EntityKindMismatch
            | DomainError::NonemptyUntypedList
            | DomainError::NonfiniteVector
            | DomainError::LocalReferenceOutOfRange => Self::ZeErrInvalidArgument,
        }
    }
}
impl From<CanonicalError> for ZeErrorCode {
    fn from(error: CanonicalError) -> Self {
        match error {
            CanonicalError::Domain(error) => error.into(),
            CanonicalError::DuplicateProperty
            | CanonicalError::InvalidScratch
            | CanonicalError::ProvenanceKindMismatch => Self::ZeErrInvalidArgument,
            CanonicalError::InputTooLarge => Self::ZeErrBudgetExceeded,
            CanonicalError::Cancelled => Self::ZeErrCancelled,
            CanonicalError::UnsupportedProvenanceVersion => Self::ZeErrFormatVersion,
            CanonicalError::Io(_) => Self::ZeErrIo,
        }
    }
}
impl From<KeyLifecycleError> for ZeErrorCode {
    fn from(error: KeyLifecycleError) -> Self {
        match error {
            KeyLifecycleError::Canonical(error) => error.into(),
            KeyLifecycleError::KindMismatch | KeyLifecycleError::MissingTarget => {
                Self::ZeErrInvalidArgument
            }
            KeyLifecycleError::InvalidState
            | KeyLifecycleError::InvalidInstalledIdentity
            | KeyLifecycleError::InvalidGeneration => Self::ZeErrInternal,
            KeyLifecycleError::MissingKey => Self::ZeErrNotFound,
            KeyLifecycleError::Stale { .. } => Self::ZeErrStaleRevision,
            KeyLifecycleError::RevisionConflict
            | KeyLifecycleError::AlreadyExists
            | KeyLifecycleError::DeletedKey
            | KeyLifecycleError::NotDeleted => Self::ZeErrKeyConflict,
            KeyLifecycleError::IncarnationConflict => Self::ZeErrIncarnationConflict,
            KeyLifecycleError::DeletionRevisionConflict => Self::ZeErrDeletionRevisionConflict,
            KeyLifecycleError::RelationshipIdentityChange => Self::ZeErrEndpoint,
            KeyLifecycleError::RevisionOverflow => Self::ZeErrRevisionOverflow,
            KeyLifecycleError::GenerationOverflow => Self::ZeErrGenerationOverflow,
            KeyLifecycleError::DuplicateTarget => Self::ZeErrDuplicateTarget,
            KeyLifecycleError::TooManyTargets | KeyLifecycleError::InputTooLarge => {
                Self::ZeErrBudgetExceeded
            }
        }
    }
}
