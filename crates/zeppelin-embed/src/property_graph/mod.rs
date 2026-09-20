//! Logical property-graph data. No physical row or vector membership is an identity.

mod canonical;
mod identity;
mod local;
mod names;
mod provenance;
mod values;

pub use canonical::{
    CanonicalComparison, CanonicalContents, CanonicalEmbedding, CanonicalError,
    CanonicalFingerprint, CanonicalStats, GraphProperty, MAX_CANONICAL_SCRATCH,
    compare_canonical_streams,
};

/// Immutable native graph artifact and page framing.
pub mod storage;

#[cfg(feature = "graph-cypher")]
pub(crate) mod retrieval;

pub use identity::{
    EntityId, EntityKind, GraphGeneration, GraphRevision, NodeId, RelId, StoreInstanceId,
};
pub use local::{LocalNodeRef, LocalRefs, LocalRelRef, NodeRef, RelRef, with_local_refs};
pub use names::{ApplicationKey, EntityMetadata, GraphName};
pub use provenance::{
    ExpectedGraphState, GraphDeleteMode, GraphOperation, OperationFields, OperationProvenance,
    ReplayEvidence, compare_replay_evidence,
};
pub use values::{GraphVector, PropertyData, PropertyValue};

/// Maximum canonical input bytes per graph statement/batch (ZE-102).
/// Individual borrowed inputs cannot exceed this; staging charges their total.
pub const MAX_GRAPH_INPUT_BYTES: usize = 8 * 1024 * 1024;

/// Maximum scalar elements in a stored property list (ZE-102).
pub const MAX_PROPERTY_LIST_ELEMENTS: usize = 524_288;

/// Maximum normalized entity changes in one atomic graph batch (ZE-102).
pub const MAX_GRAPH_CHANGES: usize = 16_384;

/// Invalid logical graph input, rejected before staging or publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DomainError {
    /// Zero is reserved and cannot identify an entity or store.
    ZeroIdentity,
    /// Revisions start at one.
    ZeroRevision,
    /// No larger revision can be represented.
    RevisionOverflow,
    /// A name supplied as bytes is not valid UTF-8.
    InvalidUtf8,
    /// One input alone exceeds the complete batch payload limit.
    InputTooLarge,
    /// A key and its entity refer to different identity domains.
    EntityKindMismatch,
    /// Untyped lists may represent only the count-zero empty list.
    NonemptyUntypedList,
    /// A property list exceeds the shared element limit.
    ListTooLong,
    /// A supplied vector is empty or differs from its declared dimensions.
    VectorDimensions,
    /// A supplied vector contains NaN or infinity.
    NonfiniteVector,
    /// A batch-local slot exceeds the maximum batch size.
    LocalReferenceOutOfRange,
}

impl std::fmt::Display for DomainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ZeroIdentity => "graph identity must be nonzero",
            Self::ZeroRevision => "graph revision must be positive",
            Self::RevisionOverflow => "graph revision overflow",
            Self::InvalidUtf8 => "graph name is not UTF-8",
            Self::InputTooLarge => "graph input exceeds the byte limit",
            Self::EntityKindMismatch => "graph key and entity kinds differ",
            Self::NonemptyUntypedList => "untyped graph list must be empty",
            Self::ListTooLong => "graph property list exceeds the element limit",
            Self::VectorDimensions => "graph vector dimensions do not match a positive declaration",
            Self::NonfiniteVector => "graph vector coordinates must be finite",
            Self::LocalReferenceOutOfRange => "graph batch-local reference is out of range",
        })
    }
}

impl std::error::Error for DomainError {}

#[cfg(test)]
mod tests;

/// Logical symbol dictionary and graph interpretation validation.
pub mod catalog;

mod key_lifecycle;

pub use canonical::EntityShape;
pub use key_lifecycle::{
    BatchClassification, BatchDisposition, BatchTarget, CanonicalRecord, CurrentEntity, CypherEdit,
    KeyDecision, KeyLifecycleError, KeyRequest, KeyState, PendingKeyChange, classify_cypher,
    classify_key, summarize_key_batch, validate_distinct_targets,
};

/// Validated typed query plans and query-specific value semantics.
pub mod query;

/// Complete native graph WAL envelopes and private replay validation.
pub mod wal;

/// Shared graph participant reservations backed by lifecycle accounting.
pub mod resources;

/// Bounded private mixed-write admission and pending property/text access.
pub mod staging;

mod utf8;
pub use utf8::{Utf8CheckError, checked_utf8};
