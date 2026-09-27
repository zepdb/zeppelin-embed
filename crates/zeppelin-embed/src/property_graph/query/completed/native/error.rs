//! `GraphQueryError`: the one typed rejection of the structured execution
//! seam.
//!
//! Before it, a caller had to know four error types and which path produced
//! which: `NativeResultError` inside a `RuntimeFailure` for reads,
//! `NativeMutationError` for writes, `NativeGraphError` for admission and
//! `MemoryError` for plan retention. The seam now keeps the original typed
//! cause, unchanged, and adds what a caller acts on: the plan's error group
//! (`kind`), whether the store may have changed (`nothing_committed`), and
//! the failing operator and the work done so far, whenever the driver knew
//! them. No error carries a partial row.
//!
//! The groups are the ones `docs/graph/plans/execution.md` names. Every
//! inner variant is classified by an exhaustive match, so a new variant is a
//! compile error here until it is given a group.

use super::super::{CompletedError, SourceError};
use super::NativeResultError;
use crate::lifecycle::native_graph::{NativeGraphError, NativeMutationError};
use crate::lifecycle::{StoreError, StoreErrorKind};
use crate::property_graph::CanonicalError;
use crate::property_graph::query::QueryError;
use crate::property_graph::query::expression::ExpressionFailure;
use crate::property_graph::query::plan::{PlanError, PlanNodeId};
use crate::property_graph::query::resources::MemoryError;
use crate::property_graph::query::runtime::{
    NativeExecutionError, RuntimeError, RuntimeFailure, WorkCounters,
};
use crate::property_graph::staging::StageError;
use crate::property_graph::storage::tree::directory::TreeError;

/// The plan's error groups. A caller branches on this; the exact cause stays
/// available through [`GraphQueryError::cause`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphQueryErrorKind {
    /// The plan is invalid or unsupported, or the statement driver broke its
    /// own contract with the seam.
    InvalidPlan,
    /// A parameter is missing, surplus or of the wrong type.
    Parameter,
    /// An expression failed at run time: a type, arithmetic or value error.
    Expression,
    /// The statement violates a graph rule: it deletes a node that still has
    /// relationships, returns an entity it deleted, or targets one that does
    /// not exist.
    Constraint,
    /// A work, row, result, byte or memory limit was reached.
    Limit,
    /// The caller cancelled the statement.
    Cancelled,
    /// The caller's deadline expired.
    Timeout,
    /// The store closed, or began closing, under the statement.
    Closed,
    /// Stored bytes or an internal invariant failed validation.
    Corruption,
    /// A filesystem operation failed before anything could commit.
    Storage,
    /// The store cannot admit this statement now: writes or reads stopped
    /// after an earlier indeterminate commit, a checkpoint must run, or an
    /// identity space is exhausted.
    Unavailable,
    /// The commit was attempted and its outcome is unknown. The write may be
    /// durable; the store stops admitting work until it is reopened.
    WriteIndeterminate,
}

/// The typed cause, exactly as the failing layer produced it.
#[derive(Debug)]
pub(crate) enum GraphQueryCause {
    /// The plan could not be retained or admitted against its owners.
    Admission(MemoryError),
    /// The native executor rejected the statement.
    Execution(NativeExecutionError),
    /// Copying the completed result rejected the statement.
    Completed(CompletedError),
    /// Graph admission, staging or the commit tail rejected the statement.
    Graph(NativeGraphError),
    /// The statement driver broke its contract with the seam.
    Contract(&'static str),
}

/// A rejected graph statement. It never carries a partial result.
#[derive(Debug)]
pub struct GraphQueryError {
    cause: GraphQueryCause,
    operator: Option<PlanNodeId>,
    counters: Option<WorkCounters>,
}

impl GraphQueryError {
    pub(crate) const fn contract(reason: &'static str) -> Self {
        Self::from_cause(GraphQueryCause::Contract(reason))
    }

    /// A statement builder refused to produce a plan. The builder keeps its
    /// own typed reason; this only stops the seam without a result.
    pub const fn builder_rejected() -> Self {
        Self::contract("the statement builder rejected its statement")
    }

    const fn from_cause(cause: GraphQueryCause) -> Self {
        Self {
            cause,
            operator: None,
            counters: None,
        }
    }

    /// The error group a caller acts on.
    pub fn kind(&self) -> GraphQueryErrorKind {
        match &self.cause {
            GraphQueryCause::Admission(error) => memory(error),
            GraphQueryCause::Execution(error) => execution(error),
            GraphQueryCause::Completed(error) => completed(error),
            GraphQueryCause::Graph(error) => graph(error),
            GraphQueryCause::Contract(_) => GraphQueryErrorKind::InvalidPlan,
        }
    }

    /// True unless the commit was attempted with an unknown outcome. Every
    /// other rejection, including one that arrives during the commit tail
    /// before its point of no return, published nothing.
    pub fn nothing_committed(&self) -> bool {
        self.kind() != GraphQueryErrorKind::WriteIndeterminate
    }

    /// The operator the driver was running when the statement failed, when
    /// the failure came from inside the driver.
    pub const fn operator(&self) -> Option<PlanNodeId> {
        self.operator
    }

    /// The work the driver had done when the statement failed, when the
    /// failure came from inside the driver.
    pub const fn counters(&self) -> Option<WorkCounters> {
        self.counters
    }

    pub(crate) const fn cause(&self) -> &GraphQueryCause {
        &self.cause
    }

    /// Converts back into the admission's own error, for the S2 tests that
    /// still drive the write path below the seam.
    #[cfg(test)]
    pub(crate) fn into_mutation(self) -> NativeMutationError {
        match self.cause {
            GraphQueryCause::Admission(error) => {
                NativeMutationError::Execution(RuntimeError::Memory(error).into())
            }
            GraphQueryCause::Execution(error) => NativeMutationError::Execution(error),
            GraphQueryCause::Completed(error) => NativeMutationError::Completed(error),
            GraphQueryCause::Graph(error) => NativeMutationError::Graph(error),
            GraphQueryCause::Contract(reason) => {
                NativeMutationError::Graph(NativeGraphError::Invalid(reason))
            }
        }
    }
}

impl From<MemoryError> for GraphQueryError {
    fn from(error: MemoryError) -> Self {
        Self::from_cause(GraphQueryCause::Admission(error))
    }
}

impl From<PlanError> for GraphQueryError {
    fn from(error: PlanError) -> Self {
        Self::from_cause(GraphQueryCause::Admission(MemoryError::Plan(error)))
    }
}

impl From<NativeExecutionError> for GraphQueryError {
    fn from(error: NativeExecutionError) -> Self {
        Self::from_cause(GraphQueryCause::Execution(error))
    }
}

impl From<CompletedError> for GraphQueryError {
    fn from(error: CompletedError) -> Self {
        Self::from_cause(GraphQueryCause::Completed(error))
    }
}

impl From<NativeGraphError> for GraphQueryError {
    fn from(error: NativeGraphError) -> Self {
        Self::from_cause(GraphQueryCause::Graph(error))
    }
}

impl From<NativeResultError> for GraphQueryError {
    fn from(error: NativeResultError) -> Self {
        match error {
            NativeResultError::Native(error) => error.into(),
            NativeResultError::Completed(error) => error.into(),
        }
    }
}

impl From<NativeMutationError> for GraphQueryError {
    fn from(error: NativeMutationError) -> Self {
        match error {
            NativeMutationError::Graph(error) => error.into(),
            NativeMutationError::Execution(error) => error.into(),
            NativeMutationError::Completed(error) => error.into(),
        }
    }
}

/// Keeps the driver's operator and counters beside the typed cause.
impl<E: Into<GraphQueryError>> From<RuntimeFailure<E>> for GraphQueryError {
    fn from(failure: RuntimeFailure<E>) -> Self {
        let mut error = failure.error.into();
        error.operator = Some(failure.operator);
        error.counters = Some(failure.counters);
        error
    }
}

impl std::fmt::Display for GraphQueryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "graph query rejected ({:?}", self.kind())?;
        if let Some(operator) = self.operator {
            write!(formatter, " at operator {}", operator.0)?;
        }
        formatter.write_str("): ")?;
        match &self.cause {
            GraphQueryCause::Admission(error) => error.fmt(formatter),
            GraphQueryCause::Execution(error) => error.fmt(formatter),
            GraphQueryCause::Completed(error) => write!(formatter, "graph result: {error:?}"),
            GraphQueryCause::Graph(error) => error.fmt(formatter),
            GraphQueryCause::Contract(reason) => formatter.write_str(reason),
        }
    }
}

impl std::error::Error for GraphQueryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.cause {
            GraphQueryCause::Execution(error) => Some(error),
            GraphQueryCause::Graph(error) => Some(error),
            GraphQueryCause::Admission(_)
            | GraphQueryCause::Completed(_)
            | GraphQueryCause::Contract(_) => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Classification: one exhaustive match per inner error type
// ---------------------------------------------------------------------------

use GraphQueryErrorKind as Kind;

fn value(error: QueryError) -> Kind {
    match error {
        QueryError::Type
        | QueryError::ArithmeticOverflow
        | QueryError::ArithmeticDomain
        | QueryError::DivisionByZero => Kind::Expression,
        QueryError::PropertyLimit
        | QueryError::BufferTooSmall
        | QueryError::WorkLimit
        | QueryError::ListLimit
        | QueryError::ValueTooLarge => Kind::Limit,
        QueryError::ForeignView => Kind::InvalidPlan,
        QueryError::Cancelled => Kind::Cancelled,
        QueryError::ReadCancelled => Kind::Closed,
        QueryError::Timeout => Kind::Timeout,
        QueryError::Control => Kind::Unavailable,
    }
}

/// The lifecycle's own query control, which storage reads poll.
fn control(error: &crate::lifecycle::QueryError) -> Kind {
    match error {
        crate::lifecycle::QueryError::Timeout { .. } => Kind::Timeout,
        crate::lifecycle::QueryError::Cancelled { .. } => Kind::Cancelled,
        crate::lifecycle::QueryError::ReadCancelled { .. } => Kind::Closed,
        crate::lifecycle::QueryError::Store(error) => store(error),
        crate::lifecycle::QueryError::Scan(_) | crate::lifecycle::QueryError::Graph(_) => {
            Kind::Corruption
        }
    }
}

fn store(error: &StoreError) -> Kind {
    match error.kind() {
        StoreErrorKind::Io => Kind::Storage,
        StoreErrorKind::InvalidArgument
        | StoreErrorKind::Unsupported
        | StoreErrorKind::DimensionMismatch
        | StoreErrorKind::EmptyBatch => Kind::InvalidPlan,
        StoreErrorKind::Corrupt => Kind::Corruption,
        StoreErrorKind::BudgetExceeded | StoreErrorKind::OutOfMemory => Kind::Limit,
        StoreErrorKind::Cancelled => Kind::Cancelled,
        StoreErrorKind::Closing | StoreErrorKind::Closed => Kind::Closed,
        StoreErrorKind::StoreBusy
        | StoreErrorKind::EpochMismatch
        | StoreErrorKind::EpochUndeclared
        | StoreErrorKind::EpochUnstamped
        | StoreErrorKind::Internal
        | StoreErrorKind::ReadOnly
        | StoreErrorKind::Panic
        | StoreErrorKind::Synchronization => Kind::Unavailable,
    }
}

fn plan(error: PlanError) -> Kind {
    match error {
        PlanError::Parameter => Kind::Parameter,
        PlanError::Limit | PlanError::Footprint => Kind::Limit,
        PlanError::Control(error) => value(error),
        PlanError::Reference
        | PlanError::Cycle
        | PlanError::Unreachable
        | PlanError::Arity
        | PlanError::Scope
        | PlanError::Type
        | PlanError::Barrier
        | PlanError::ReadAfterWrite
        | PlanError::ReadWriteSearch
        | PlanError::Aggregate
        | PlanError::Search
        | PlanError::PathBound => Kind::InvalidPlan,
    }
}

fn memory(error: &MemoryError) -> Kind {
    match error {
        MemoryError::Limit | MemoryError::Allocation => Kind::Limit,
        MemoryError::Store(error) => store(error),
        MemoryError::UnprovedInput => Kind::InvalidPlan,
        MemoryError::Value(error) => value(*error),
        MemoryError::Plan(error) => plan(*error),
    }
}

fn runtime(error: &RuntimeError) -> Kind {
    match error {
        RuntimeError::Limit(_) => Kind::Limit,
        RuntimeError::Value(error) => value(*error),
        RuntimeError::Memory(error) => memory(error),
        RuntimeError::Batch => Kind::InvalidPlan,
        RuntimeError::BatchCapacity => Kind::Limit,
        RuntimeError::IdentityExhausted => Kind::Unavailable,
    }
}

fn tree(error: &TreeError) -> Kind {
    match error {
        TreeError::Missing
        | TreeError::Invalid(_)
        | TreeError::Format(_)
        | TreeError::WalMetadata(_) => Kind::Corruption,
        TreeError::Control(error) => control(error),
        TreeError::Io(_) => Kind::Storage,
        TreeError::Memory | TreeError::Work => Kind::Limit,
        TreeError::Runtime(error) => runtime(error),
    }
}

fn canonical(error: &CanonicalError) -> Kind {
    match error {
        CanonicalError::Domain(_) | CanonicalError::DuplicateProperty => Kind::Expression,
        CanonicalError::InputTooLarge => Kind::Limit,
        CanonicalError::InvalidScratch => Kind::InvalidPlan,
        CanonicalError::Cancelled => Kind::Cancelled,
        CanonicalError::UnsupportedProvenanceVersion | CanonicalError::ProvenanceKindMismatch => {
            Kind::Corruption
        }
        CanonicalError::Io(_) => Kind::Storage,
    }
}

fn stage(error: &StageError) -> Kind {
    match error {
        StageError::Limit => Kind::Limit,
        StageError::InvalidLimits | StageError::InvalidInput => Kind::InvalidPlan,
        StageError::Cancelled => Kind::Cancelled,
        StageError::Endpoint
        | StageError::MissingEntity
        | StageError::IncidentRelationship
        | StageError::DeletedEntity
        | StageError::Lifecycle(_) => Kind::Constraint,
        StageError::ViewMismatch | StageError::Catalog(_) => Kind::Corruption,
        StageError::IdentityOverflow => Kind::Unavailable,
        StageError::Canonical(error) => canonical(error),
        StageError::Memory(error) => store(error),
        StageError::NativeStorage(error) => tree(error),
    }
}

/// A real `SearchAdapter`'s ZE-62/63 retrieval producer refused. Most causes
/// are internal-invariant classes that a correctly built adapter never
/// triggers; the few reachable ones (limits, a store lacking a vector
/// space, a stale resolved version) map to their nearest existing kind.
fn retrieval(error: &crate::property_graph::retrieval::RetrievalError) -> Kind {
    use crate::property_graph::retrieval::RetrievalError;
    match error {
        RetrievalError::Storage(error) => tree(error),
        RetrievalError::Control(error) => runtime(error),
        RetrievalError::Eligibility(error) => value(*error),
        RetrievalError::Memory | RetrievalError::CandidateWindow { .. } => Kind::Limit,
        RetrievalError::LexicalTerms { .. } => Kind::Limit,
        RetrievalError::NoVectorSpace
        | RetrievalError::UnindexedVectorSource
        | RetrievalError::AnalyzerMismatch
        | RetrievalError::MissingVersion(_)
        | RetrievalError::Version(_) => Kind::Constraint,
        RetrievalError::Dimension { .. } => Kind::Expression,
        RetrievalError::Identity(_)
        | RetrievalError::Vector(_)
        | RetrievalError::Graph(_)
        | RetrievalError::Lexical(_)
        | RetrievalError::Fusion(_)
        | RetrievalError::EligibilityMismatch
        | RetrievalError::Invariant(_) => Kind::Corruption,
    }
}

fn execution(error: &NativeExecutionError) -> Kind {
    match error {
        NativeExecutionError::Runtime(error) => runtime(error),
        NativeExecutionError::Expression(error) => match &error.failure {
            ExpressionFailure::Runtime(error) => runtime(error),
            ExpressionFailure::Plan(error) => plan(*error),
            ExpressionFailure::Tree(error) => tree(error),
            ExpressionFailure::Stage(error) => stage(error),
        },
        NativeExecutionError::Plan(error) => plan(*error),
        NativeExecutionError::Tree(error) => tree(error),
        NativeExecutionError::Stage(error) => stage(error),
        NativeExecutionError::Retrieval(error) => retrieval(error),
    }
}

fn completed(error: &CompletedError) -> Kind {
    match error {
        CompletedError::Source(SourceError::Missing(_) | SourceError::Deleted(_)) => {
            Kind::Constraint
        }
        CompletedError::Source(SourceError::ForeignView) => Kind::InvalidPlan,
        CompletedError::Source(SourceError::Storage) => Kind::Storage,
        CompletedError::Runtime(error) => runtime(error),
        CompletedError::Shape | CompletedError::Utf8 => Kind::Corruption,
        CompletedError::Limit => Kind::Limit,
    }
}

/// The error group of a graph lifecycle error, shared with the graph store
/// facade so both seams classify the same cause the same way.
pub(crate) fn native_graph_error_kind(error: &NativeGraphError) -> Kind {
    graph(error)
}

fn graph(error: &NativeGraphError) -> Kind {
    match error {
        NativeGraphError::Store(error) => store(error),
        NativeGraphError::Invalid(_) | NativeGraphError::Catalog(_) | NativeGraphError::Wal(_) => {
            Kind::Corruption
        }
        NativeGraphError::LeaseLimit => Kind::Limit,
        NativeGraphError::NotInstalled
        | NativeGraphError::IdentityExhausted
        | NativeGraphError::WritesStopped
        | NativeGraphError::ReadAdmissionsStopped
        | NativeGraphError::CheckpointRequired
        | NativeGraphError::StalePreparation
        | NativeGraphError::StoreInitializationIncomplete => Kind::Unavailable,
        NativeGraphError::Read(error) => tree(error),
        NativeGraphError::Io { .. } => Kind::Storage,
        NativeGraphError::Stage(error) => stage(error),
        NativeGraphError::CommitIndeterminate { .. } => Kind::WriteIndeterminate,
        // A statement that only creates and deletes its own entities cannot
        // publish yet: an unsupported plan, not a store fault.
        NativeGraphError::FenceOnlyStatement => Kind::InvalidPlan,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ze208_invalid_shape_batch_remains_invalid_plan() {
        let error = GraphQueryError::from(NativeExecutionError::from(RuntimeError::Batch));
        assert_eq!(error.kind(), Kind::InvalidPlan);
        assert!(error.nothing_committed());
    }
}
