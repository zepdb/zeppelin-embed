//! Typed pull runtime foundation. Storage admission is a required owned adapter.
use super::resources::{MemoryError, QueryMemory, QueryReservation};
use super::{QueryError, QueryView, ValueContext};
use crate::lifecycle::QueryControl;
mod batch;
mod driver;
pub use batch::{ArenaCapacity, RowBatch};
pub use driver::{
    Completion, Execution, ExecutionCapacity, FrozenOutput, OperatorFactory, PreparedRows,
    PullOperator, PullState, RuntimeFailure, execute, execute_factory,
};

/// Required admission-owner adapter. This is not a constructor for GraphReadView.
/// Its token must remain stable and its actual lease must remain retained until
/// all pulling, completion copying and final checks have stopped.
pub trait RetainedView {
    /// The one pointer-owned token shared by every participant of this execution.
    fn query_view(&self) -> &QueryView;
    /// Checks the actual retained lifecycle capability. Close takes precedence
    /// over caller cancellation and must return QueryError::ReadCancelled.
    fn check_active(&self) -> Result<(), QueryError>;
}

/// Real consumed work categories, not estimates derived from requested LIMIT.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub enum WorkKind {
    /// Each operator's actually examined row, including filtered/discarded rows.
    OperatorRows,
    /// Actual adjacency entries examined, including tombstoned edges.
    AdjacencyEntries,
    /// Top-level expression evaluations at their real evaluation sites.
    Expressions,
    /// Hash table probes, including collisions and failed probes.
    HashProbes,
    /// Completed output rows retained before exposure.
    CompletedRows,
    /// Completed core payload bytes; registry/control overhead is separate.
    CompletedBytes,
    /// Logical collector payload copied before representation-specific freezing.
    PreparedPayloadBytes,
    /// Initialized completed ABI arena bytes; controls/capacity are separate.
    CompletedAbiBytes,
    /// Coordinates consumed by every scoring/preparation pass.
    VectorCoordinates,
    /// Original vector bytes consumed, including repeated passes.
    VectorBytes,
    /// Posting visits across all lexical passes.
    LexicalPostings,
    /// Lexical block visits across all passes.
    LexicalBlocks,
    /// Executed syntactic search invocations.
    SearchInvocations,
    /// Actual directory lookups.
    Lookups,
    /// Actual source scan operations.
    Scans,
    /// Paths actually emitted by traversal.
    Paths,
    /// Actual input rows at operator boundaries.
    RowsIn,
    /// Actual output rows at operator boundaries.
    RowsOut,
    /// Actual join probes, also charge HashProbes for hash-based joins.
    JoinProbes,
    /// Group keys actually constructed.
    GroupKeys,
    /// Total eligibility entries examined; the per-set cap is checked separately.
    EligibilityEntries,
    /// Bytes actually copied during intermediate/result construction.
    CopiedBytes,
}
const KINDS: [WorkKind; 22] = [
    WorkKind::OperatorRows,
    WorkKind::AdjacencyEntries,
    WorkKind::Expressions,
    WorkKind::HashProbes,
    WorkKind::CompletedRows,
    WorkKind::CompletedBytes,
    WorkKind::PreparedPayloadBytes,
    WorkKind::CompletedAbiBytes,
    WorkKind::VectorCoordinates,
    WorkKind::VectorBytes,
    WorkKind::LexicalPostings,
    WorkKind::LexicalBlocks,
    WorkKind::SearchInvocations,
    WorkKind::Lookups,
    WorkKind::Scans,
    WorkKind::Paths,
    WorkKind::RowsIn,
    WorkKind::RowsOut,
    WorkKind::JoinProbes,
    WorkKind::GroupKeys,
    WorkKind::EligibilityEntries,
    WorkKind::CopiedBytes,
];
impl WorkKind {
    const fn hard_max(self) -> u64 {
        match self {
            Self::OperatorRows => 4_000_000,
            Self::AdjacencyEntries => 2_000_000,
            Self::Expressions => 8_000_000,
            Self::HashProbes => 16_000_000,
            Self::CompletedRows => 65_536,
            Self::CompletedBytes | Self::PreparedPayloadBytes | Self::CompletedAbiBytes => {
                4 * 1024 * 1024
            }
            Self::VectorCoordinates => 2_147_483_648,
            Self::VectorBytes => 8 * 1024 * 1024 * 1024,
            Self::LexicalPostings => 64_000_000,
            Self::LexicalBlocks => 4_000_000,
            Self::SearchInvocations => 8,
            _ => u64::MAX,
        }
    }
}
/// Caller-tightened shared hard limits; no constructor permits widening defaults.
#[derive(Clone, Copy)]
pub struct RuntimeLimits {
    values: [u64; KINDS.len()],
}
impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            values: KINDS.map(WorkKind::hard_max),
        }
    }
}
impl RuntimeLimits {
    /// Tightens one cumulative limit; rejected widening changes no execution.
    pub fn with_limit(mut self, kind: WorkKind, value: u64) -> Result<Self, RuntimeError> {
        if value > kind.hard_max() {
            return Err(RuntimeError::Limit(kind));
        }
        *self
            .values
            .get_mut(kind as usize)
            .ok_or(RuntimeError::Limit(kind))? = value;
        Ok(self)
    }
}
/// Exact cumulative work for one execution. Zero work is never inferred from LIMIT.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WorkCounters {
    values: [u64; KINDS.len()],
}
impl WorkCounters {
    /// The selected actual-site count.
    pub fn get(self, kind: WorkKind) -> u64 {
        self.values.get(kind as usize).copied().unwrap_or(0)
    }
}
/// Runtime rejection; errors contain no prepared/completed row collection.
#[derive(Debug)]
pub enum RuntimeError {
    /// A cumulative real-work or completed-payload limit would be exceeded.
    Limit(WorkKind),
    /// Existing query/value control rejected execution.
    Value(QueryError),
    /// Capacity reservation/allocation failed.
    Memory(MemoryError),
    /// A pull produced an invalid shape, exceeded fixed batch bounds or mixed owners.
    Batch,
}
impl From<QueryError> for RuntimeError {
    fn from(e: QueryError) -> Self {
        Self::Value(e)
    }
}
impl From<MemoryError> for RuntimeError {
    fn from(e: MemoryError) -> Self {
        Self::Memory(e)
    }
}
impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Limit(k) => write!(f, "graph query work limit: {k:?}"),
            Self::Value(e) => e.fmt(f),
            Self::Memory(e) => e.fmt(f),
            Self::Batch => f.write_str("invalid or exhausted flat query batch"),
        }
    }
}
impl std::error::Error for RuntimeError {}

/// One stable retained view, cumulative value/control context and query memory.
/// No context creation admits a view, starts a worker or allocates a new budget.
pub struct RuntimeContext<'v, 'm, 'g> {
    values: ValueContext<'v>,
    memory: &'m QueryMemory<'g>,
    limits: RuntimeLimits,
    counters: WorkCounters,
    _charge: QueryReservation<'m, 'g>,
}
impl<'v, 'm, 'g> RuntimeContext<'v, 'm, 'g> {
    /// Binds the mandatory retained-view adapter and caller control once.
    pub fn new(
        view: &'v dyn RetainedView,
        control: &'v QueryControl,
        memory: &'m QueryMemory<'g>,
        limits: RuntimeLimits,
    ) -> Result<Self, RuntimeError> {
        let values = ValueContext::retained(view, control, super::MAX_VALUE_WORK)?;
        let charge = memory.reserve(std::mem::size_of::<Self>())?;
        Ok(Self {
            values,
            memory,
            limits,
            counters: WorkCounters::default(),
            _charge: charge,
        })
    }
    /// Checks close first, then the same absolute deadline/cancellation token.
    pub fn checkpoint(&self) -> Result<(), RuntimeError> {
        Ok(self.values.checkpoint()?)
    }
    /// Checks an upcoming operation without claiming any work was consumed.
    /// Operators use this before fallible copying and charge only performed work.
    pub fn check_work(&self, kind: WorkKind, units: u64) -> Result<(), RuntimeError> {
        self.checkpoint()?;
        let count = self
            .counters
            .values
            .get(kind as usize)
            .ok_or(RuntimeError::Limit(kind))?;
        let next = count.checked_add(units).ok_or(RuntimeError::Limit(kind))?;
        if next
            > *self
                .limits
                .values
                .get(kind as usize)
                .ok_or(RuntimeError::Limit(kind))?
        {
            return Err(RuntimeError::Limit(kind));
        }
        Ok(())
    }
    /// Reserves the next units before their corresponding operation is consumed.
    pub fn charge(&mut self, kind: WorkKind, units: u64) -> Result<(), RuntimeError> {
        self.check_work(kind, units)?;
        let count = self
            .counters
            .values
            .get_mut(kind as usize)
            .ok_or(RuntimeError::Limit(kind))?;
        *count = count.checked_add(units).ok_or(RuntimeError::Limit(kind))?;
        Ok(())
    }
    /// Cumulative known-site counters, including work later discarded on failure.
    pub const fn counters(&self) -> WorkCounters {
        self.counters
    }
    /// Same query allocation owner, never a per-operator independent allowance.
    pub const fn memory(&self) -> &'m QueryMemory<'g> {
        self.memory
    }
    /// Same cumulative value context across all batches and expression operations.
    pub fn values(&mut self) -> &mut ValueContext<'v> {
        &mut self.values
    }
    /// Stable view identity for intermediate references, not an entity existence proof.
    pub const fn view(&self) -> &'v QueryView {
        self.values.view
    }
}
