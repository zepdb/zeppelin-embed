//! Store lifecycle and memory accounting.

mod budget;
mod cancel;
mod clock;
mod close;
pub mod durability;
pub(crate) mod graph_cache;
pub mod lock;
mod pool;
#[cfg(test)]
mod shared_bound_tests;
mod snapshot;
pub(crate) mod stats;

pub use cancel::{
    CancelToken, Deadline, DeadlineError, QueryCancellation, QueryControl, QueryError,
};
#[cfg(any(test, feature = "test-support"))]
pub use clock::ManualMonotonicClock;
pub use clock::{MonotonicClock, SystemMonotonicClock};
pub use snapshot::{
    InMemorySegment, InMemorySegmentFactors, PreparedSegment, PublishedSnapshot, SnapshotLease,
};
pub use stats::Stats;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Duration;

use durability::{CommitTier, DurabilityMode, DurabilityPolicy, DurabilityPolicyError};
use lock::{StoreLock, StoreLockError};

use self::close::BackgroundThread;

/// Store-owned infrastructure injected only by deterministic tests.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone)]
pub struct StoreTestDependencies {
    vfs: Arc<dyn crate::vfs::Vfs>,
    clock: Arc<dyn MonotonicClock>,
    hybrid_leg_fault: Option<HybridLegTestFault>,
}

#[cfg(any(test, feature = "test-support"))]
impl StoreTestDependencies {
    /// Binds one filesystem and monotonic clock to every operation performed
    /// by a test store handle.
    #[must_use]
    pub fn new(vfs: Arc<dyn crate::vfs::Vfs>, clock: Arc<dyn MonotonicClock>) -> Self {
        Self {
            vfs,
            clock,
            hybrid_leg_fault: None,
        }
    }

    /// Arms one Store-owned hybrid fault; it is consumed exactly once.
    #[must_use]
    pub const fn with_hybrid_leg_fault(mut self, fault: HybridLegTestFault) -> Self {
        self.hybrid_leg_fault = Some(fault);
        self
    }
}

/// Narrow test-only fault at a Store-owned hybrid leg seam.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[doc(hidden)]
pub enum HybridLegTestFault {
    /// Panics after the named leg starts and before it can publish output.
    Panic(crate::fusion::FusionLeg),
}

/// Facts emitted by one Store-owned parallel hybrid execution.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Debug, Eq, PartialEq)]
#[doc(hidden)]
pub struct HybridExecutionReceipt {
    /// Thread that executed vector work.
    pub vector_thread: std::thread::ThreadId,
    /// Name of the scoped lexical worker.
    pub lexical_thread_name: Option<String>,
    /// Vector leg reached a terminal result before return.
    pub vector_completed: bool,
    /// Lexical leg reached a terminal result before return.
    pub lexical_completed: bool,
}

/// Default grace period given to admitted readers before close cancellation.
pub const DEFAULT_READER_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);

/// Explicit lifecycle state for one store handle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreState {
    /// New calls may be admitted.
    Open,
    /// New calls are rejected while admitted work drains.
    Closing,
    /// Teardown completed and only idempotent close/state calls remain valid.
    Closed,
}

/// Store-open configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenOptions {
    access_mode: AccessMode,
    durability_mode: DurabilityMode,
    commit_tier: CommitTier,
    reader_drain_timeout: Duration,
    max_resident_bytes: u64,
    max_temp_bytes: u64,
    epoch: Option<crate::epoch::StoreEpoch>,
    schema: Option<crate::meta::Schema>,
}

/// Filesystem authority requested for one store handle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessMode {
    /// Own the store's one kernel-enforced writer slot.
    ReadWrite,
    /// Load only the last committed snapshot without filesystem mutation.
    ReadOnly,
}

impl OpenOptions {
    /// Returns the default read-write, derived-durability configuration.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            access_mode: AccessMode::ReadWrite,
            durability_mode: DurabilityMode::Derived,
            commit_tier: CommitTier::Ordered,
            reader_drain_timeout: DEFAULT_READER_DRAIN_TIMEOUT,
            max_resident_bytes: u64::MAX,
            max_temp_bytes: u64::MAX,
            epoch: None,
            schema: None,
        }
    }

    /// Returns a pure-read configuration that takes no writer lock.
    #[must_use]
    pub const fn read_only() -> Self {
        Self {
            access_mode: AccessMode::ReadOnly,
            durability_mode: DurabilityMode::Derived,
            commit_tier: CommitTier::Ordered,
            reader_drain_timeout: DEFAULT_READER_DRAIN_TIMEOUT,
            max_resident_bytes: u64::MAX,
            max_temp_bytes: u64::MAX,
            epoch: None,
            schema: None,
        }
    }

    /// Selects the existing durability-policy mode and per-commit tier.
    #[must_use]
    pub const fn with_durability(mut self, mode: DurabilityMode, tier: CommitTier) -> Self {
        self.durability_mode = mode;
        self.commit_tier = tier;
        self
    }

    /// Sets the grace period close gives admitted readers before cancellation.
    #[must_use]
    pub const fn with_reader_drain_timeout(mut self, timeout: Duration) -> Self {
        self.reader_drain_timeout = timeout;
        self
    }

    /// Sets the exact ceiling for live engine-owned anonymous bytes.
    #[must_use]
    pub const fn with_max_resident_bytes(mut self, bytes: u64) -> Self {
        self.max_resident_bytes = bytes;
        self
    }

    /// Declares the typed user-column schema when creating a store.
    ///
    /// On reopen, an optional declaration must exactly match the persisted schema.
    #[must_use]
    pub fn with_schema(mut self, schema: crate::meta::Schema) -> Self {
        self.schema = Some(schema);
        self
    }

    /// Sets the exact ceiling for live temporary anonymous bytes.
    #[must_use]
    pub const fn with_max_temp_bytes(mut self, bytes: u64) -> Self {
        self.max_temp_bytes = bytes;
        self
    }

    /// Declares the embedding and tokenizer identity used by this handle.
    #[must_use]
    pub fn with_epoch(mut self, epoch: crate::epoch::StoreEpoch) -> Self {
        self.epoch = Some(epoch);
        self
    }
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Traversal controls carried only when a caller explicitly selects the graph tier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphSearchOptions {
    profile: crate::graph::search::GraphSearchProfile,
    ef: Option<usize>,
    seed: u64,
}

impl GraphSearchOptions {
    /// Creates an adaptive-width graph request for one dataset-shape profile.
    #[must_use]
    pub const fn new(profile: crate::graph::search::GraphSearchProfile) -> Self {
        Self {
            profile,
            ef: None,
            seed: 0,
        }
    }

    /// Overrides the profile's adaptive traversal width.
    #[must_use]
    pub const fn with_ef(mut self, ef: usize) -> Self {
        self.ef = Some(ef);
        self
    }

    /// Selects the deterministic Bit4 query-preparation seed.
    #[must_use]
    pub const fn with_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    pub(crate) const fn profile(self) -> crate::graph::search::GraphSearchProfile {
        self.profile
    }

    pub(crate) const fn ef(self) -> Option<usize> {
        self.ef
    }

    pub(crate) const fn seed(self) -> u64 {
        self.seed
    }
}

impl Default for GraphSearchOptions {
    fn default() -> Self {
        Self::new(crate::graph::search::GraphSearchProfile::SiftClass)
    }
}

pub(crate) fn auto_graph_search_options(
    snapshot: &PublishedSnapshot,
) -> Result<GraphSearchOptions, QueryError> {
    snapshot
        .graph_profile()
        .map(|profile| GraphSearchOptions::new(profile.search_profile()))
        .map_err(crate::graph::search::GraphSearchError::Profile)
        .map_err(QueryError::Graph)
}

/// Per-query execution tier at the public store seam.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SearchTier {
    /// Select graph traversal per sealed segment when that graph is published.
    #[default]
    Auto,
    /// Exhaustively score full-precision vectors.
    Exact,
    /// Preserve the existing exhaustive sealed-segment scan.
    Scan,
    /// Traverse every sealed segment's Vamana graph.
    Graph(GraphSearchOptions),
}

/// Store-search controls, separating scan worker budget from tier selection.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SearchOptions {
    scan: crate::scan::ScanOptions,
    tier: Option<SearchTier>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GraphBoundMode {
    Shared,
    #[cfg(test)]
    Independent,
}

impl SearchOptions {
    /// Creates options with no tier preference and the supplied scan worker budget.
    #[must_use]
    pub const fn new(scan: crate::scan::ScanOptions) -> Self {
        Self { scan, tier: None }
    }

    /// Explicitly selects one store-search tier.
    #[must_use]
    pub const fn with_tier(mut self, tier: SearchTier) -> Self {
        self.tier = Some(tier);
        self
    }

    /// Returns the scan worker controls used by scan-tier work.
    #[must_use]
    pub const fn scan(self) -> crate::scan::ScanOptions {
        self.scan
    }

    /// Returns the effective execution tier, using automatic selection when
    /// the caller expressed no preference.
    #[must_use]
    pub const fn tier(self) -> SearchTier {
        match self.tier {
            Some(tier) => tier,
            None => SearchTier::Auto,
        }
    }

    const fn explicit_tier(self) -> Option<SearchTier> {
        self.tier
    }
}

impl From<crate::scan::ScanOptions> for SearchOptions {
    fn from(scan: crate::scan::ScanOptions) -> Self {
        Self::new(scan)
    }
}

/// An open, lifecycle, or close operation was rejected.
#[derive(Debug)]
pub enum StoreError {
    /// A filesystem operation failed for the named store path.
    Io {
        /// Store path being opened.
        path: PathBuf,
        /// Underlying filesystem failure.
        source: std::io::Error,
    },
    /// The supplied path exists but is not a directory.
    NotDirectory {
        /// Invalid store path.
        path: PathBuf,
    },
    /// Another writer owns the store's descriptor-scoped lock.
    StoreBusy {
        /// Busy store directory.
        path: PathBuf,
    },
    /// The kernel-owned writer lock could not be opened or acquired.
    Lock(StoreLockError),
    /// The selected durability mode/tier is unsupported.
    Durability(DurabilityPolicyError),
    /// The committed manifest could not be loaded or validated.
    Manifest(crate::manifest::ManifestError),
    /// The caller's declared interpretation differs from the persisted one.
    EpochMismatch(crate::epoch::EpochMismatch),
    /// Persisted bytes name an epoch but the caller did not declare one.
    EpochUndeclared,
    /// A pre-epoch manifest cannot be retroactively assigned an identity.
    EpochUnstamped,
    /// A schema declaration on reopen differed from the committed schema.
    SchemaMismatch {
        /// Schema already committed in the manifest.
        persisted: crate::meta::Schema,
        /// Schema supplied by the opening caller.
        declared: crate::meta::Schema,
    },
    /// A referenced immutable segment could not be mapped or validated.
    Segment(crate::segment::SegmentError),
    /// The caller selected graph traversal for a sealed segment without a graph.
    GraphUnavailable {
        /// Immutable segment that cannot satisfy the explicit graph-tier request.
        segment_id: crate::segment::SegmentId,
    },
    /// The checked durable WAL prefix could not be opened.
    Wal(crate::wal::WalReadError),
    /// A non-empty WAL did not replay through a clean end boundary.
    WalRecovery(crate::wal::WalRecoveryError),
    /// A replayed record's retained encoded bytes could not be decoded.
    WalRecord {
        /// Sequence assigned to the invalid retained record.
        seq: crate::wal::LogSeq,
        /// Checked record-access failure.
        source: crate::wal::VisibleRecordError,
    },
    /// A checksummed WAL mutation payload violated its versioned contract.
    WalMutation {
        /// Sequence assigned to the invalid mutation.
        seq: crate::wal::LogSeq,
        /// Persisted operation identifier.
        op: u16,
        /// Typed mutation-payload failure.
        source: crate::ingest::wal_payload::PayloadError,
    },
    /// A recovered upsert would make one document's revision non-monotonic.
    WalRevisionOrder {
        /// Sequence assigned to the invalid upsert.
        seq: crate::wal::LogSeq,
        /// Document whose recovered history is invalid.
        doc_id: crate::ingest::DocId,
        /// Revision already rebuilt from the trusted prefix.
        current: crate::ingest::Revision,
        /// Revision carried by this record.
        attempted: crate::ingest::Revision,
    },
    /// A valid mutation kind has no active-segment application path yet.
    UnsupportedWalMutation {
        /// Sequence assigned to the unsupported mutation.
        seq: crate::wal::LogSeq,
        /// Persisted operation identifier.
        op: u16,
    },
    /// A recovered vector passed payload validation but failed quantization.
    WalVector {
        /// Sequence assigned to the invalid vector mutation.
        seq: crate::wal::LogSeq,
        /// Typed vector-contract failure.
        source: crate::quant::QuantError,
    },
    /// The store-owned WAL writer could not be created or advanced.
    WalWrite(crate::wal::WalWriteError),
    /// Retained WAL visibility could not be released through a committed seal.
    WalRetire(crate::wal::WalRetireError),
    /// A kernel probe needed for an exact statistics snapshot failed.
    Statistics {
        /// Counter or residency component that could not be read.
        component: &'static str,
        /// Underlying operating-system failure.
        source: std::io::Error,
    },
    /// An accounted allocation would exceed a configured ceiling.
    BudgetExceeded {
        /// Total live bytes that would be needed if the operation proceeded.
        needed: u64,
        /// Configured ceiling in bytes.
        budget: u64,
        /// Engine component requesting the allocation.
        component: &'static str,
    },
    /// The allocator rejected a checked reservation without aborting.
    AllocationFailed {
        /// Exact requested bytes.
        needed: u64,
        /// Engine component requesting the allocation.
        component: &'static str,
    },
    /// An active vector did not match the collection's established dimension.
    DimensionMismatch {
        /// Established active dimension.
        expected: usize,
        /// Supplied vector dimension.
        actual: usize,
    },
    /// The active segment exceeded its dense u32 row address space.
    ActiveRowOverflow,
    /// An explicit seal was requested without any active rows.
    EmptyActiveSegment,
    /// Caller cancellation stopped a seal before its manifest commit point.
    SealCancelled,
    /// A write-only lifecycle operation was requested from a read-only handle.
    ReadOnly,
    /// A prepared segment was accounted to a different store handle.
    ForeignPreparedSegment,
    /// The current snapshot generation cannot be incremented.
    GenerationOverflow,
    /// Exact reclaimed-byte reporting overflowed its u64 contract.
    PartitionBytesOverflow,
    /// A durable physical-purge intent could not be resumed during open.
    PurgeRecovery {
        /// Typed purge failure rendered without discarding its actionable values.
        detail: String,
    },
    /// A new operation raced with close after admissions stopped.
    Closing,
    /// The handle has completed teardown.
    Closed,
    /// Close cancelled an admitted read after its drain grace period elapsed.
    ReadCancelled,
    /// The lifecycle background thread could not be created.
    BackgroundStart {
        /// Operating-system thread creation failure.
        source: std::io::Error,
    },
    /// The lifecycle background thread exited before its startup handshake.
    BackgroundHandshake,
    /// The lifecycle background thread panicked before close joined it.
    BackgroundThreadPanicked,
    /// A persistent query worker thread could not be created.
    QueryPoolStart {
        /// Operating-system thread creation failure.
        source: std::io::Error,
    },
    /// A persistent query worker exited before its startup handshake.
    QueryPoolHandshake,
    /// A persistent query worker panicked before close joined it.
    QueryPoolThreadPanicked,
    /// The operating system could not report a usable query-worker count.
    QueryPoolCapacity {
        /// CPU-topology failure text.
        source: String,
    },
    /// A lifecycle synchronization primitive was poisoned.
    Synchronization {
        /// Synchronization component that rejected the operation.
        component: &'static str,
    },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(formatter, "store I/O {}: {source}", path.display())
            }
            Self::NotDirectory { path } => {
                write!(
                    formatter,
                    "store path is not a directory: {}",
                    path.display()
                )
            }
            Self::StoreBusy { path } => {
                write!(formatter, "store already has a writer: {}", path.display())
            }
            Self::Lock(error) => error.fmt(formatter),
            Self::Durability(error) => error.fmt(formatter),
            Self::Manifest(error) => error.fmt(formatter),
            Self::EpochMismatch(error) => error.fmt(formatter),
            Self::EpochUndeclared => {
                formatter.write_str("store epoch is persisted but the caller declared none")
            }
            Self::EpochUnstamped => formatter
                .write_str("store manifest predates epoch identity and cannot adopt a declaration"),
            Self::SchemaMismatch {
                persisted,
                declared,
            } => write!(
                formatter,
                "declared store schema {declared:?} does not match persisted schema {persisted:?}"
            ),
            Self::Segment(error) => error.fmt(formatter),
            Self::GraphUnavailable { segment_id } => {
                write!(formatter, "sealed segment {segment_id} has no graph region")
            }
            Self::Wal(error) => error.fmt(formatter),
            Self::WalRecovery(error) => error.fmt(formatter),
            Self::WalRecord { seq, source } => {
                write!(
                    formatter,
                    "WAL sequence {} retained record: {source}",
                    seq.get()
                )
            }
            Self::WalMutation { seq, op, source } => write!(
                formatter,
                "WAL sequence {} operation {op} payload: {source}",
                seq.get()
            ),
            Self::WalRevisionOrder {
                seq,
                doc_id,
                current,
                attempted,
            } => write!(
                formatter,
                "WAL sequence {} document {} revision {} does not follow {}",
                seq.get(),
                doc_id.get(),
                attempted.get(),
                current.get()
            ),
            Self::UnsupportedWalMutation { seq, op } => write!(
                formatter,
                "WAL sequence {} operation {op} has no active-segment recovery path",
                seq.get()
            ),
            Self::WalVector { seq, source } => {
                write!(formatter, "WAL sequence {} vector: {source}", seq.get())
            }
            Self::WalWrite(error) => error.fmt(formatter),
            Self::WalRetire(error) => error.fmt(formatter),
            Self::Statistics { component, source } => {
                write!(formatter, "store statistics {component}: {source}")
            }
            Self::BudgetExceeded {
                needed,
                budget,
                component,
            } => write!(
                formatter,
                "store {component} allocation needs {needed} bytes, budget is {budget} bytes"
            ),
            Self::AllocationFailed { needed, component } => write!(
                formatter,
                "store {component} allocator rejected {needed} bytes"
            ),
            Self::DimensionMismatch { expected, actual } => write!(
                formatter,
                "active vector dimension {actual} does not match {expected}"
            ),
            Self::ActiveRowOverflow => {
                formatter.write_str("active segment row or byte geometry overflow")
            }
            Self::EmptyActiveSegment => formatter.write_str("active segment is empty"),
            Self::SealCancelled => formatter.write_str("seal was cancelled before commit"),
            Self::ReadOnly => formatter.write_str("store handle is read-only"),
            Self::ForeignPreparedSegment => {
                formatter.write_str("prepared segment belongs to another store")
            }
            Self::GenerationOverflow => formatter.write_str("store snapshot generation overflow"),
            Self::PartitionBytesOverflow => {
                formatter.write_str("partition reclaimed-byte count overflow")
            }
            Self::PurgeRecovery { detail } => {
                write!(formatter, "physical purge recovery failed: {detail}")
            }
            Self::Closing => formatter.write_str("store is closing"),
            Self::Closed => formatter.write_str("store is closed"),
            Self::ReadCancelled => formatter.write_str("store close cancelled the admitted read"),
            Self::BackgroundStart { source } => {
                write!(
                    formatter,
                    "store lifecycle thread could not start: {source}"
                )
            }
            Self::BackgroundHandshake => {
                formatter.write_str("store lifecycle thread startup handshake failed")
            }
            Self::BackgroundThreadPanicked => {
                formatter.write_str("store lifecycle thread panicked")
            }
            Self::QueryPoolStart { source } => {
                write!(formatter, "store query worker could not start: {source}")
            }
            Self::QueryPoolHandshake => {
                formatter.write_str("store query worker startup handshake failed")
            }
            Self::QueryPoolThreadPanicked => formatter.write_str("store query worker panicked"),
            Self::QueryPoolCapacity { source } => {
                write!(formatter, "store query worker capacity failed: {source}")
            }
            Self::Synchronization { component } => {
                write!(
                    formatter,
                    "store lifecycle synchronization poisoned: {component}"
                )
            }
        }
    }
}

/// Coarse, exhaustive classification of a [`StoreError`], stable for hosts
/// that map engine failures onto their own status codes. Every variant of
/// [`StoreError`] maps to exactly one kind; adding a variant is a compile
/// error here until it is classified.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum StoreErrorKind {
    /// An operating-system or filesystem operation failed.
    Io,
    /// A caller-supplied path or schema was invalid.
    InvalidArgument,
    /// Another process or handle owns the store's writer lock.
    StoreBusy,
    /// The requested mode or operation is unsupported.
    Unsupported,
    /// Persisted data failed validation.
    Corrupt,
    /// A configured memory, disk, or work budget was exceeded.
    BudgetExceeded,
    /// A checked allocation could not be reserved.
    OutOfMemory,
    /// Vector dimensions disagreed.
    DimensionMismatch,
    /// The declared epoch differs from the persisted identity.
    EpochMismatch,
    /// The store requires an epoch declaration and none was supplied.
    EpochUndeclared,
    /// An epoch was declared for a store with no stamped identity.
    EpochUnstamped,
    /// An internal invariant failed.
    Internal,
    /// A batch or active segment was empty.
    EmptyBatch,
    /// Cooperative cancellation stopped the operation.
    Cancelled,
    /// The store is read-only.
    ReadOnly,
    /// The store is closing.
    Closing,
    /// The store is closed.
    Closed,
    /// A background or query-pool thread panicked.
    Panic,
    /// A synchronization primitive was poisoned or unavailable.
    Synchronization,
}

impl StoreError {
    /// Classifies this error; see [`StoreErrorKind`].
    #[must_use]
    pub fn kind(&self) -> StoreErrorKind {
        match self {
            Self::Io { .. }
            | Self::Lock(_)
            | Self::Statistics { .. }
            | Self::BackgroundStart { .. }
            | Self::QueryPoolStart { .. }
            | Self::WalWrite(_)
            | Self::WalRetire(_) => StoreErrorKind::Io,
            Self::NotDirectory { .. } | Self::SchemaMismatch { .. } => {
                StoreErrorKind::InvalidArgument
            }
            Self::StoreBusy { .. } => StoreErrorKind::StoreBusy,
            Self::Durability(_)
            | Self::GraphUnavailable { .. }
            | Self::UnsupportedWalMutation { .. } => StoreErrorKind::Unsupported,
            Self::Manifest(_)
            | Self::Segment(_)
            | Self::Wal(_)
            | Self::WalRecovery(_)
            | Self::WalRecord { .. }
            | Self::WalMutation { .. }
            | Self::WalRevisionOrder { .. }
            | Self::WalVector { .. }
            | Self::PurgeRecovery { .. } => StoreErrorKind::Corrupt,
            Self::BudgetExceeded { .. } => StoreErrorKind::BudgetExceeded,
            Self::AllocationFailed { .. } => StoreErrorKind::OutOfMemory,
            Self::DimensionMismatch { .. } => StoreErrorKind::DimensionMismatch,
            Self::EpochMismatch(_) => StoreErrorKind::EpochMismatch,
            Self::EpochUndeclared => StoreErrorKind::EpochUndeclared,
            Self::EpochUnstamped => StoreErrorKind::EpochUnstamped,
            Self::ActiveRowOverflow
            | Self::GenerationOverflow
            | Self::PartitionBytesOverflow
            | Self::ForeignPreparedSegment
            | Self::BackgroundHandshake
            | Self::QueryPoolHandshake
            | Self::QueryPoolCapacity { .. } => StoreErrorKind::Internal,
            Self::EmptyActiveSegment => StoreErrorKind::EmptyBatch,
            Self::SealCancelled | Self::ReadCancelled => StoreErrorKind::Cancelled,
            Self::ReadOnly => StoreErrorKind::ReadOnly,
            Self::Closing => StoreErrorKind::Closing,
            Self::Closed => StoreErrorKind::Closed,
            Self::BackgroundThreadPanicked | Self::QueryPoolThreadPanicked => StoreErrorKind::Panic,
            Self::Synchronization { .. } => StoreErrorKind::Synchronization,
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Lock(error) => Some(error),
            Self::Durability(error) => Some(error),
            Self::Manifest(error) => Some(error),
            Self::EpochMismatch(error) => Some(error),
            Self::Segment(error) => Some(error),
            Self::Wal(error) => Some(error),
            Self::WalRecovery(error) => Some(error),
            Self::WalRecord { source, .. } => Some(source),
            Self::WalMutation { source, .. } => Some(source),
            Self::WalVector { source, .. } => Some(source),
            Self::WalWrite(error) => Some(error),
            Self::WalRetire(error) => Some(error),
            Self::Statistics { source, .. } => Some(source),
            Self::BackgroundStart { source } => Some(source),
            Self::QueryPoolStart { source } => Some(source),
            Self::NotDirectory { .. }
            | Self::StoreBusy { .. }
            | Self::EpochUndeclared
            | Self::EpochUnstamped
            | Self::SchemaMismatch { .. }
            | Self::GraphUnavailable { .. }
            | Self::WalRevisionOrder { .. }
            | Self::UnsupportedWalMutation { .. }
            | Self::BudgetExceeded { .. }
            | Self::AllocationFailed { .. }
            | Self::DimensionMismatch { .. }
            | Self::ActiveRowOverflow
            | Self::EmptyActiveSegment
            | Self::SealCancelled
            | Self::ReadOnly
            | Self::ForeignPreparedSegment
            | Self::GenerationOverflow
            | Self::PartitionBytesOverflow
            | Self::PurgeRecovery { .. }
            | Self::Closing
            | Self::Closed
            | Self::ReadCancelled
            | Self::BackgroundHandshake
            | Self::BackgroundThreadPanicked
            | Self::QueryPoolHandshake
            | Self::QueryPoolThreadPanicked
            | Self::QueryPoolCapacity { .. }
            | Self::Synchronization { .. } => None,
        }
    }
}

/// One explicitly closeable embedded-store handle.
pub struct Store {
    pub(crate) directory: PathBuf,
    pub(crate) vfs: Arc<dyn crate::vfs::Vfs>,
    pub(crate) clock: Arc<dyn MonotonicClock>,
    pub(crate) state: Mutex<StoreState>,
    pub(crate) state_changed: Condvar,
    pub(crate) background: Mutex<Option<BackgroundThread>>,
    pub(crate) query_pool: Mutex<Option<Arc<pool::QueryPool>>>,
    pub(crate) snapshot: RwLock<Option<Arc<PublishedSnapshot>>>,
    // Drop order is deliberate: immutable mappings, active buffers, and the
    // WAL descriptor all release before the kernel writer lock.
    pub(crate) active: Mutex<Option<crate::ingest::ActiveState>>,
    pub(crate) wal_writer: Mutex<Option<crate::ingest::StoreWal>>,
    pub(crate) writer_lock: Mutex<Option<StoreLock>>,
    pub(crate) maintenance: Mutex<()>,
    pub(crate) health_state: Mutex<crate::diag::HealthState>,
    pub(crate) durability_policy: DurabilityPolicy,
    pub(crate) reader_drain_timeout: Duration,
    pub(crate) accounting: Arc<stats::Accounting>,
    pub(crate) active_queries: AtomicU64,
    pub(crate) epoch: Option<crate::epoch::StoreEpoch>,
    pub(crate) epoch_alias: crate::epoch::EpochAliasCell,
    pub(crate) schema: crate::meta::Schema,
    #[cfg(any(test, feature = "test-support"))]
    hybrid_leg_fault: Mutex<Option<HybridLegTestFault>>,
    #[cfg(any(test, feature = "test-support"))]
    hybrid_execution_receipt: Mutex<Option<HybridExecutionReceipt>>,
    #[cfg(test)]
    pub(crate) teardown_probe: Arc<close::TeardownProbe>,
}

impl Store {
    /// Opens a store directory with the requested access and durability policy.
    pub fn open(path: impl AsRef<Path>, options: OpenOptions) -> Result<Self, StoreError> {
        Self::open_with_infrastructure(
            path,
            options,
            Arc::new(crate::vfs::StdVfs),
            Arc::new(SystemMonotonicClock),
            #[cfg(any(test, feature = "test-support"))]
            None,
        )
    }

    fn open_with_infrastructure(
        path: impl AsRef<Path>,
        options: OpenOptions,
        vfs: Arc<dyn crate::vfs::Vfs>,
        clock: Arc<dyn MonotonicClock>,
        #[cfg(any(test, feature = "test-support"))] hybrid_leg_fault: Option<HybridLegTestFault>,
    ) -> Result<Self, StoreError> {
        let path = path.as_ref();
        let durability_policy = DurabilityPolicy::new(options.durability_mode, options.commit_tier)
            .map_err(StoreError::Durability)?;
        let accounting = Arc::new(stats::Accounting::new(
            options.max_resident_bytes,
            options.max_temp_bytes,
        ));
        if options.access_mode == AccessMode::ReadWrite {
            std::fs::create_dir_all(path).map_err(|source| StoreError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        }
        let metadata = std::fs::metadata(path).map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if !metadata.is_dir() {
            return Err(StoreError::NotDirectory {
                path: path.to_path_buf(),
            });
        }
        let writer_lock = match options.access_mode {
            AccessMode::ReadWrite => {
                Some(StoreLock::acquire(path).map_err(|error| match error {
                    StoreLockError::Io { path, source }
                        if source.kind() == std::io::ErrorKind::WouldBlock =>
                    {
                        StoreError::StoreBusy { path }
                    }
                    StoreLockError::Io { path, source } => {
                        StoreError::Lock(StoreLockError::Io { path, source })
                    }
                })?)
            }
            AccessMode::ReadOnly => None,
        };
        let manifest_path = path.join(crate::manifest::io::MANIFEST_FILE);
        let manifest_exists = match vfs.open(&manifest_path) {
            Ok(_) => true,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => false,
            Err(source) => {
                return Err(StoreError::Io {
                    path: manifest_path,
                    source,
                });
            }
        };
        // A persisted store gets one bounded header probe through the
        // Store-owned VFS before mmap becomes the query data plane. Internal
        // manifest-only remaps deliberately skip this probe so their exact
        // zero-segment-read accounting contracts remain intact.
        let snapshot = PublishedSnapshot::load_for_open_on_vfs(path, &accounting, vfs.as_ref())?;
        let schema = if manifest_exists {
            let persisted = snapshot.schema().clone();
            if let Some(declared) = options.schema.as_ref()
                && declared != &persisted
            {
                return Err(StoreError::SchemaMismatch {
                    persisted,
                    declared: declared.clone(),
                });
            }
            snapshot.schema().clone()
        } else {
            options
                .schema
                .clone()
                .unwrap_or_else(crate::meta::Schema::timestamp_only)
        };
        let persisted_epoch = snapshot.epoch_alias();
        let declared_epoch = options
            .epoch
            .as_ref()
            .map(crate::epoch::StoreEpoch::identity);
        match (persisted_epoch, declared_epoch) {
            (Some(expected), Some(declared)) if expected != declared => {
                return Err(StoreError::EpochMismatch(crate::epoch::EpochMismatch {
                    expected,
                    declared,
                }));
            }
            (Some(_), None) => return Err(StoreError::EpochUndeclared),
            (None, Some(_)) if manifest_exists || options.access_mode == AccessMode::ReadOnly => {
                return Err(StoreError::EpochUnstamped);
            }
            (Some(_), Some(_)) | (None, Some(_)) | (None, None) => {}
        }
        let absorbed_through = snapshot.absorbed_through();
        let wal_path = path.join("wal.ze");
        let (active, recovered_wal) = crate::ingest::ActiveState::recover(
            vfs.as_ref(),
            &wal_path,
            snapshot.generation(),
            absorbed_through,
            &accounting,
            &schema,
        )?;
        if options.access_mode == AccessMode::ReadWrite
            && !manifest_exists
            && (options.epoch.is_some() || options.schema.is_some())
        {
            crate::manifest::io::commit_manifest(
                vfs.as_ref(),
                path,
                &crate::manifest::Manifest {
                    generation: active.generation,
                    log_seq: 0,
                    segments: Vec::new(),
                    epochs: options
                        .epoch
                        .as_ref()
                        .cloned()
                        .map(crate::manifest::EpochMeta::from)
                        .into_iter()
                        .collect(),
                    epoch_alias: options
                        .epoch
                        .as_ref()
                        .map(crate::epoch::StoreEpoch::identity),
                    schema: schema.clone(),
                },
                durability_policy,
            )
            .map_err(StoreError::Manifest)?;
        }
        let wal_writer = match options.access_mode {
            AccessMode::ReadWrite => Some(match recovered_wal {
                Some(recovered) => crate::ingest::StoreWal::resume(
                    vfs.as_ref(),
                    &wal_path,
                    recovered,
                    durability_policy,
                    absorbed_through,
                    &accounting,
                )?,
                None => crate::ingest::StoreWal::create(
                    vfs.as_ref(),
                    &wal_path,
                    durability_policy,
                    &accounting,
                )?,
            }),
            AccessMode::ReadOnly => {
                drop(recovered_wal);
                None
            }
        };
        let background = match options.access_mode {
            AccessMode::ReadWrite => Some(BackgroundThread::start()?),
            AccessMode::ReadOnly => None,
        };
        #[cfg(test)]
        let (snapshot, background, teardown_probe) = {
            let mut snapshot = snapshot;
            let mut background = background;
            let teardown_probe = Arc::new(close::TeardownProbe::new());
            snapshot.set_teardown_probe(Arc::clone(&teardown_probe));
            if let Some(background) = background.as_mut() {
                background.set_teardown_probe(Arc::clone(&teardown_probe));
            }
            (snapshot, background, teardown_probe)
        };
        let store = Self {
            directory: path.to_path_buf(),
            vfs,
            clock,
            state: Mutex::new(StoreState::Open),
            state_changed: Condvar::new(),
            background: Mutex::new(background),
            query_pool: Mutex::new(None),
            snapshot: RwLock::new(None),
            active: Mutex::new(Some(active)),
            wal_writer: Mutex::new(wal_writer),
            writer_lock: Mutex::new(writer_lock),
            maintenance: Mutex::new(()),
            health_state: Mutex::new(crate::diag::HealthState::default()),
            durability_policy,
            reader_drain_timeout: options.reader_drain_timeout,
            accounting,
            active_queries: AtomicU64::new(0),
            epoch_alias: crate::epoch::EpochAliasCell::new(persisted_epoch.or(declared_epoch)),
            epoch: options.epoch,
            schema,
            #[cfg(any(test, feature = "test-support"))]
            hybrid_leg_fault: Mutex::new(hybrid_leg_fault),
            #[cfg(any(test, feature = "test-support"))]
            hybrid_execution_receipt: Mutex::new(None),
            #[cfg(test)]
            teardown_probe,
        };
        store.publish_snapshot(snapshot)?;
        store
            .recover_pending_physical_purge()
            .map_err(|error| StoreError::PurgeRecovery {
                detail: error.to_string(),
            })?;
        Ok(store)
    }

    /// Opens a store with deterministic infrastructure for adversarial tests.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn open_with_test_dependencies(
        path: impl AsRef<Path>,
        options: OpenOptions,
        dependencies: StoreTestDependencies,
    ) -> Result<Self, StoreError> {
        Self::open_with_infrastructure(
            path,
            options,
            dependencies.vfs,
            dependencies.clock,
            dependencies.hybrid_leg_fault,
        )
    }

    /// Takes the most recent Store-owned hybrid execution receipt.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn take_hybrid_execution_receipt(&self) -> Option<HybridExecutionReceipt> {
        self.hybrid_execution_receipt
            .lock()
            .ok()
            .and_then(|mut receipt| receipt.take())
    }

    /// Carries the committed epoch registry forward unchanged.
    ///
    /// A declared epoch is stamped once, during open, before any write is
    /// admitted, so every later commit only propagates what is already
    /// committed. Replacement-snapshot paths may not have a prior manifest
    /// value in hand, so a declared store reconstructs its already-committed
    /// singleton entry; an unstamped store still returns an empty registry.
    pub(crate) fn epoch_registry(
        &self,
        prior: &[crate::manifest::EpochMeta],
    ) -> Vec<crate::manifest::EpochMeta> {
        if prior.is_empty() {
            self.epoch
                .as_ref()
                .map(crate::manifest::EpochMeta::from)
                .into_iter()
                .collect()
        } else {
            prior.to_vec()
        }
    }

    /// Returns the published embedding and tokenizer identity, or `None`
    /// for a store that carries no stamped epoch.
    #[must_use]
    pub fn epoch_identity(&self) -> Option<crate::epoch::EpochIdentity> {
        self.epoch_alias.load()
    }

    /// Returns the current explicit lifecycle state.
    pub fn state(&self) -> Result<StoreState, StoreError> {
        self.state
            .lock()
            .map(|state| *state)
            .map_err(|_| StoreError::Synchronization { component: "state" })
    }

    /// Acquires the complete immutable snapshot admitted for one read.
    pub fn snapshot(&self) -> Result<SnapshotLease, StoreError> {
        let state = self
            .state
            .lock()
            .map_err(|_| StoreError::Synchronization { component: "state" })?;
        match *state {
            StoreState::Open => {}
            StoreState::Closing => return Err(StoreError::Closing),
            StoreState::Closed => return Err(StoreError::Closed),
        }
        let active = self
            .active
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "active segment",
            })?;
        let generation = active.as_ref().ok_or(StoreError::Closed)?.generation;
        let snapshot = self
            .snapshot
            .read()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })?
            .as_ref()
            .cloned()
            .ok_or(StoreError::Closed)?;
        drop(active);
        drop(state);
        Ok(SnapshotLease::new_at(snapshot, generation))
    }

    pub(crate) fn publish_snapshot(&self, snapshot: PublishedSnapshot) -> Result<(), StoreError> {
        let state = self
            .state
            .lock()
            .map_err(|_| StoreError::Synchronization { component: "state" })?;
        match *state {
            StoreState::Open => {}
            StoreState::Closing => return Err(StoreError::Closing),
            StoreState::Closed => return Err(StoreError::Closed),
        }
        let generation = snapshot.generation();
        let mut active = self
            .active
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "active segment",
            })?;
        let active_generation = &mut active.as_mut().ok_or(StoreError::Closed)?.generation;
        *active_generation = (*active_generation).max(generation);
        let mut published = self
            .snapshot
            .write()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })?;
        *published = Some(Arc::new(snapshot));
        drop(active);
        drop(state);
        Ok(())
    }

    /// Runs one exact parallel scan admitted against this store's snapshot.
    ///
    /// The query holds one snapshot lease for its complete execution and uses
    /// the store's lazily started persistent worker pool.
    pub fn top_k_with_options(
        &self,
        request: crate::scan::ScanRequest<'_>,
        k: usize,
        options: crate::scan::ScanOptions,
        control: QueryControl,
    ) -> Result<crate::scan::ScanOutcome, QueryError> {
        let control = control.with_clock(Arc::clone(&self.clock));
        let state = self
            .state
            .lock()
            .map_err(|_| QueryError::Store(StoreError::Synchronization { component: "state" }))?;
        match *state {
            StoreState::Open => {}
            StoreState::Closing => return Err(QueryError::Store(StoreError::Closing)),
            StoreState::Closed => return Err(QueryError::Store(StoreError::Closed)),
        }
        let snapshot = self
            .snapshot
            .read()
            .map_err(|_| {
                QueryError::Store(StoreError::Synchronization {
                    component: "published snapshot",
                })
            })?
            .as_ref()
            .cloned()
            .ok_or(QueryError::Store(StoreError::Closed))?;
        let mut pool_slot = self.query_pool.lock().map_err(|_| {
            QueryError::Store(StoreError::Synchronization {
                component: "query pool",
            })
        })?;
        if pool_slot.is_none() {
            let capacity = crate::scan::physical_thread_capacity().map_err(|error| {
                QueryError::Store(StoreError::QueryPoolCapacity {
                    source: error.to_string(),
                })
            })?;
            let pool =
                pool::QueryPool::start(capacity, &self.accounting).map_err(QueryError::Store)?;
            *pool_slot = Some(Arc::new(pool));
        }
        let pool = pool_slot.as_ref().cloned().ok_or({
            QueryError::Store(StoreError::Synchronization {
                component: "query pool initialization",
            })
        })?;
        self.active_queries.fetch_add(1, Ordering::Relaxed);
        let active = ActiveQuery {
            count: &self.active_queries,
        };
        drop(pool_slot);
        drop(state);

        let result = pool.execute(request, k, options, control, SnapshotLease::new(snapshot));
        drop(active);
        result
    }

    /// Searches the active segment plus every immutable segment in one pinned
    /// store generation, then merges one global top-k.
    ///
    /// Scan-tier segments use the persistent query pool. Explicit graph-tier
    /// segments traverse serially on the calling thread, while the active
    /// segment is still scanned. Both tiers share one cancellation state and
    /// `(RowSource, local_row)` addressing, so immutable row zero never collides
    /// across segments and manifest reordering cannot rename a sealed row.
    pub fn search(
        &self,
        request: crate::ingest::SearchRequest<'_>,
        k: usize,
        options: impl Into<SearchOptions>,
        control: QueryControl,
    ) -> Result<crate::ingest::SearchOutcome, QueryError> {
        self.search_with_graph_bound_mode(
            request,
            k,
            options.into(),
            control,
            GraphBoundMode::Shared,
        )
    }

    /// Runs an exact structured lexical query over active and sealed text.
    pub fn search_lexical(
        &self,
        query: &crate::fts::search::TermQuery,
        k: usize,
        control: QueryControl,
    ) -> Result<crate::ingest::StoreLexicalSearchOutcome, crate::ingest::StoreLexicalError> {
        let control = control.with_clock(Arc::clone(&self.clock));
        let started = std::time::Instant::now();
        let state = self
            .state
            .lock()
            .map_err(|_| QueryError::Store(StoreError::Synchronization { component: "state" }))?;
        match *state {
            StoreState::Open => {}
            StoreState::Closing => return Err(QueryError::Store(StoreError::Closing).into()),
            StoreState::Closed => return Err(QueryError::Store(StoreError::Closed).into()),
        }
        let active_guard = self.active.lock().map_err(|_| {
            QueryError::Store(StoreError::Synchronization {
                component: "active segment",
            })
        })?;
        let active_state = active_guard
            .as_ref()
            .ok_or(QueryError::Store(StoreError::Closed))?;
        let generation = active_state.generation;
        let active = Arc::clone(&active_state.segment);
        let snapshot = self
            .snapshot
            .read()
            .map_err(|_| {
                QueryError::Store(StoreError::Synchronization {
                    component: "published snapshot",
                })
            })?
            .as_ref()
            .cloned()
            .ok_or(QueryError::Store(StoreError::Closed))?;
        self.active_queries.fetch_add(1, Ordering::Relaxed);
        let active_query = ActiveQuery {
            count: &self.active_queries,
        };
        drop(active_guard);
        drop(state);

        enum LexicalSource {
            Sealed(usize),
            Active,
        }
        let lease = SnapshotLease::new_at(Arc::clone(&snapshot), generation);
        let cancellation = QueryCancellation::new(&control, &lease);
        cancellation.check_graph().map_err(QueryError::Scan)?;
        let mut index = crate::fts::index::LexicalIndex::new();
        let mut alive_sets = Vec::new();
        let mut sources = Vec::new();
        for (ordinal, segment) in snapshot.segments().iter().enumerate() {
            if let Some(postings) = segment.query_postings().map_err(QueryError::Store)? {
                if postings.row_count() != 0
                    && segment
                        .document_version(0)
                        .map_err(StoreError::Segment)
                        .map_err(QueryError::Store)?
                        .is_none()
                {
                    return Err(crate::ingest::StoreLexicalError::MissingDocumentIdentity {
                        segment_id: segment.meta().id,
                    });
                }
                let alive = segment.query_alive().map_err(QueryError::Store)?;
                let live_rows = alive.alive_bitmap();
                index
                    .push_shared_with_live_rows(postings, live_rows)
                    .map_err(crate::planner::LexicalFilterError::from)?;
                alive_sets.push(alive);
                sources.push(LexicalSource::Sealed(ordinal));
            }
        }
        if active.has_text() {
            let sealed = active
                .sealed_lexical(&self.accounting)
                .map_err(QueryError::Store)?;
            let active_alive = Arc::new(active.alive().map_err(QueryError::Store)?);
            let live_rows = active_alive.alive_bitmap();
            index
                .push_shared_with_live_rows(sealed, live_rows)
                .map_err(crate::planner::LexicalFilterError::from)?;
            alive_sets.push(active_alive);
            sources.push(LexicalSource::Active);
        }
        let allow_lists = alive_sets
            .iter()
            .map(|alive| alive.alive_bitmap())
            .collect::<Vec<_>>();
        let lexical = crate::planner::search_lexical_filtered_refs(
            &index,
            query,
            k,
            crate::fts::bm25::Bm25Params::beir(),
            &allow_lists,
            None,
        )?;
        cancellation.check_graph().map_err(QueryError::Scan)?;
        let mut candidates = Vec::with_capacity(lexical.result.hits.len());
        for hit in &lexical.result.hits {
            let source = usize::try_from(hit.doc.segment)
                .ok()
                .and_then(|slot| sources.get(slot))
                .ok_or(QueryError::Store(StoreError::ActiveRowOverflow))?;
            let row = usize::try_from(hit.doc.row)
                .map_err(|_| QueryError::Store(StoreError::ActiveRowOverflow))?;
            let document = match source {
                LexicalSource::Sealed(ordinal) => snapshot
                    .segments()
                    .get(*ordinal)
                    .ok_or(QueryError::Store(StoreError::ActiveRowOverflow))?
                    .document_version(row)
                    .map_err(StoreError::Segment)
                    .map_err(QueryError::Store)?
                    .ok_or(crate::ingest::StoreLexicalError::MissingDocumentIdentity {
                        segment_id: snapshot
                            .segments()
                            .get(*ordinal)
                            .ok_or(QueryError::Store(StoreError::ActiveRowOverflow))?
                            .meta()
                            .id,
                    })?,
                LexicalSource::Active => active
                    .document(row)
                    .ok_or(QueryError::Store(StoreError::ActiveRowOverflow))?,
            };
            candidates.push(crate::ingest::LexicalCandidate {
                document,
                score: hit.score,
            });
        }
        let diagnostics = crate::diag::QueryDiagnostics::lexical(crate::diag::LexicalDiagnostics {
            snapshot_generation: generation,
            indexed_through_seq: active
                .indexed_through_seq()
                .max(crate::wal::LogSeq::new(snapshot.absorbed_through())),
            requested_k: k,
            returned: candidates.len(),
            counters: lexical.result.counters,
            elapsed: started.elapsed(),
        });
        drop(active_query);
        Ok(crate::ingest::StoreLexicalSearchOutcome {
            candidates,
            generation,
            diagnostics,
        })
    }

    /// Runs a structured lexical query and returns provenance plus snippets
    /// copied from the exact row text pinned for this generation.
    pub fn search_lexical_structured(
        &self,
        query: &crate::fts::query::LexicalQuery,
        k: usize,
        snippet_bytes: usize,
        control: QueryControl,
    ) -> Result<crate::ingest::StoreStructuredLexicalSearchOutcome, crate::ingest::StoreLexicalError>
    {
        let control = control.with_clock(Arc::clone(&self.clock));
        let started = std::time::Instant::now();
        let state = self
            .state
            .lock()
            .map_err(|_| QueryError::Store(StoreError::Synchronization { component: "state" }))?;
        match *state {
            StoreState::Open => {}
            StoreState::Closing => return Err(QueryError::Store(StoreError::Closing).into()),
            StoreState::Closed => return Err(QueryError::Store(StoreError::Closed).into()),
        }
        let active_guard = self.active.lock().map_err(|_| {
            QueryError::Store(StoreError::Synchronization {
                component: "active segment",
            })
        })?;
        let active_state = active_guard
            .as_ref()
            .ok_or(QueryError::Store(StoreError::Closed))?;
        let generation = active_state.generation;
        let active = Arc::clone(&active_state.segment);
        let snapshot = self
            .snapshot
            .read()
            .map_err(|_| {
                QueryError::Store(StoreError::Synchronization {
                    component: "published snapshot",
                })
            })?
            .as_ref()
            .cloned()
            .ok_or(QueryError::Store(StoreError::Closed))?;
        self.active_queries.fetch_add(1, Ordering::Relaxed);
        let active_query = ActiveQuery {
            count: &self.active_queries,
        };
        drop(active_guard);
        drop(state);

        let lease = SnapshotLease::new_at(Arc::clone(&snapshot), generation);
        let cancellation = QueryCancellation::new(&control, &lease);
        cancellation.check_graph().map_err(QueryError::Scan)?;
        let analyzer = crate::fts::tokenizer::Analyzer::new(
            crate::fts::tokenizer::TokenizerConfig::text_default(),
        )?;
        let mut index = crate::fts::index::LexicalIndex::new();
        let mut alive_sets = Vec::new();
        let mut sources = Vec::new();
        for (ordinal, segment) in snapshot.segments().iter().enumerate() {
            cancellation.check_graph().map_err(QueryError::Scan)?;
            if let Some(postings) = segment.query_postings().map_err(QueryError::Store)? {
                if postings.row_count() != 0
                    && segment
                        .document_version(0)
                        .map_err(StoreError::Segment)
                        .map_err(QueryError::Store)?
                        .is_none()
                {
                    return Err(crate::ingest::StoreLexicalError::MissingDocumentIdentity {
                        segment_id: segment.meta().id,
                    });
                }
                let alive = segment.query_alive().map_err(QueryError::Store)?;
                index
                    .push_shared_with_live_rows(Arc::clone(&postings), alive.alive_bitmap())
                    .map_err(crate::planner::LexicalFilterError::from)?;
                alive_sets.push(alive);
                sources.push(StructuredLexicalSource::Sealed(ordinal));
            }
        }
        if active.has_text() {
            let sealed = active
                .sealed_lexical(&self.accounting)
                .map_err(QueryError::Store)?;
            let alive = Arc::new(active.alive().map_err(QueryError::Store)?);
            index
                .push_shared_with_live_rows(sealed, alive.alive_bitmap())
                .map_err(crate::planner::LexicalFilterError::from)?;
            alive_sets.push(alive);
            sources.push(StructuredLexicalSource::Active);
        }
        let vocabulary = crate::fts::query::vocabulary(index.terms());
        let expansions = crate::fts::query::expand(query, &vocabulary)?;
        let allow_lists = alive_sets
            .iter()
            .map(|alive| alive.alive_bitmap())
            .collect::<Vec<_>>();
        let mut aggregate = BTreeMap::<
            crate::fts::search::GlobalDocId,
            (f64, Vec<crate::fts::query::LexicalExpansion>),
        >::new();
        let mut counters = crate::fts::search::SearchCounters::default();
        let fields = query.fields();
        let all_rows = usize::try_from(index.document_count()).unwrap_or(usize::MAX);
        for expansion in &expansions {
            cancellation.check_graph().map_err(QueryError::Scan)?;
            let term_query = crate::fts::search::TermQuery {
                terms: vec![expansion.term.clone()],
                fields: fields.clone(),
            };
            let scored = crate::fts::search::search_allow_list_driven_controlled(
                &index,
                &term_query,
                all_rows,
                crate::fts::bm25::Bm25Params::beir(),
                &allow_lists,
                || cancellation.check_graph(),
            )
            .map_err(|error| match error {
                crate::fts::search::ControlledSearchError::Index(error) => {
                    crate::ingest::StoreLexicalError::from(
                        crate::planner::LexicalFilterError::from(error),
                    )
                }
                crate::fts::search::ControlledSearchError::Control(error) => {
                    crate::ingest::StoreLexicalError::from(QueryError::Scan(error))
                }
            })?;
            counters.docs_evaluated = counters
                .docs_evaluated
                .saturating_add(scored.counters.docs_evaluated);
            counters.postings_decoded = counters
                .postings_decoded
                .saturating_add(scored.counters.postings_decoded);
            counters.blocks_decoded = counters
                .blocks_decoded
                .saturating_add(scored.counters.blocks_decoded);
            counters.blocks_skipped = counters
                .blocks_skipped
                .saturating_add(scored.counters.blocks_skipped);
            let boost = f64::from(expansion.boost_thousandths) / 1_000.0;
            for hit in scored.hits {
                let entry = aggregate.entry(hit.doc).or_default();
                entry.0 += hit.score * boost;
                entry.1.push(expansion.clone());
            }
        }
        let mut scored = Vec::with_capacity(aggregate.len());
        for (doc, (score, provenance)) in aggregate {
            if let Some((terms, slop)) = query.phrase_constraint() {
                let (text, _) = structured_lexical_row(&snapshot, &active, &sources, doc)?;
                if !crate::fts::query::phrase_matches(&analyzer, text, terms, slop) {
                    continue;
                }
            }
            scored.push((doc, score, provenance));
        }
        scored.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(left.0.cmp(&right.0))
        });
        scored.truncate(k);
        let mut candidates = Vec::with_capacity(scored.len());
        for (doc, score, provenance) in scored {
            cancellation.check_graph().map_err(QueryError::Scan)?;
            let (text, document) = structured_lexical_row(&snapshot, &active, &sources, doc)?;
            let terms = provenance
                .iter()
                .map(|entry| entry.term.clone())
                .collect::<Vec<_>>();
            let snippet =
                crate::fts::snippet::best_window(&analyzer, text, &terms, snippet_bytes, true)?
                    .ok_or(crate::ingest::StoreLexicalError::MissingSnippetMatch {
                        segment: doc.segment,
                        row: doc.row,
                    })?;
            let snippet_text = snippet
                .text(text)
                .ok_or(QueryError::Store(StoreError::ActiveRowOverflow))?;
            candidates.push(crate::ingest::ExplainedLexicalCandidate {
                document,
                score,
                provenance,
                snippet: crate::fts::query::OwnedLexicalSnippet {
                    text: snippet_text.to_owned(),
                    source: snippet.window,
                    highlights: snippet.highlights,
                },
            });
        }
        cancellation.check_graph().map_err(QueryError::Scan)?;
        let diagnostics = crate::diag::QueryDiagnostics::lexical(crate::diag::LexicalDiagnostics {
            snapshot_generation: generation,
            indexed_through_seq: active
                .indexed_through_seq()
                .max(crate::wal::LogSeq::new(snapshot.absorbed_through())),
            requested_k: k,
            returned: candidates.len(),
            counters,
            elapsed: started.elapsed(),
        });
        drop(active_query);
        Ok(crate::ingest::StoreStructuredLexicalSearchOutcome {
            candidates,
            expansions,
            generation,
            diagnostics,
        })
    }

    /// Runs the store vector engine and exact lexical leg against one pinned
    /// generation, then delegates every blend decision to `crate::fusion`.
    pub fn search_hybrid(
        &self,
        vector_query: crate::ingest::SearchRequest<'_>,
        lexical_query: &crate::fts::search::TermQuery,
        hybrid_query: &crate::fusion::HybridQuery,
        options: impl Into<SearchOptions>,
        control: QueryControl,
    ) -> Result<crate::ingest::StoreHybridSearchOutcome, crate::fusion::FusionError> {
        self.search_hybrid_inner(
            vector_query,
            PinnedLexicalQuery::Term(lexical_query),
            hybrid_query,
            options.into(),
            control,
        )
    }

    /// Runs vector work with any structured lexical operator against the
    /// same pinned snapshot and generation.
    pub fn search_hybrid_structured(
        &self,
        vector_query: crate::ingest::SearchRequest<'_>,
        lexical_query: &crate::fts::query::LexicalQuery,
        hybrid_query: &crate::fusion::HybridQuery,
        options: impl Into<SearchOptions>,
        control: QueryControl,
    ) -> Result<crate::ingest::StoreHybridSearchOutcome, crate::fusion::FusionError> {
        self.search_hybrid_inner(
            vector_query,
            PinnedLexicalQuery::Structured(lexical_query),
            hybrid_query,
            options.into(),
            control,
        )
    }

    fn search_hybrid_inner(
        &self,
        vector_query: crate::ingest::SearchRequest<'_>,
        lexical_query: PinnedLexicalQuery<'_>,
        hybrid_query: &crate::fusion::HybridQuery,
        options: SearchOptions,
        control: QueryControl,
    ) -> Result<crate::ingest::StoreHybridSearchOutcome, crate::fusion::FusionError> {
        let control = control.with_clock(Arc::clone(&self.clock));
        let started = std::time::Instant::now();
        let mut options = options;
        if options.explicit_tier().is_none() {
            options = options.with_tier(SearchTier::Exact);
        }
        let admitted = self
            .admit_vector_search(options)
            .map_err(crate::fusion::FusionError::from)?;
        let vector_k = hybrid_vector_candidate_limit(&admitted.snapshot, &admitted.active_segment)?;
        let panic_vector = self.consume_hybrid_test_fault(crate::fusion::FusionLeg::Vector);
        let panic_lexical = self.consume_hybrid_test_fault(crate::fusion::FusionLeg::Lexical);
        let parallel = std::thread::scope(|scope| {
            let lexical_thread = std::thread::Builder::new()
                .name("zeppelin-fts".to_owned())
                .spawn_scoped(scope, || {
                    let name = std::thread::current().name().map(str::to_owned);
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if panic_lexical {
                            std::panic::panic_any("injected lexical hybrid leg panic");
                        }
                        let lease = SnapshotLease::new_at(
                            Arc::clone(&admitted.snapshot),
                            admitted.generation,
                        );
                        let cancellation = QueryCancellation::new(&control, &lease);
                        cancellation
                            .check_graph()
                            .map_err(QueryError::Scan)
                            .map_err(crate::fusion::FusionError::from)?;
                        match lexical_query {
                            PinnedLexicalQuery::Term(query) => exact_lexical_leg(
                                &admitted.snapshot,
                                &admitted.active_segment,
                                &self.accounting,
                                query,
                                &cancellation,
                            ),
                            PinnedLexicalQuery::Structured(query) => exact_structured_lexical_leg(
                                &admitted.snapshot,
                                &admitted.active_segment,
                                &self.accounting,
                                query,
                                &cancellation,
                            ),
                        }
                    }))
                    .unwrap_or_else(|_| {
                        Err(crate::fusion::FusionError::LegPanic {
                            leg: crate::fusion::FusionLeg::Lexical,
                            detail: "lexical hybrid leg panicked",
                        })
                    });
                    (name, result)
                })
                .map_err(|error| crate::fusion::FusionError::LegThreadStart {
                    leg: crate::fusion::FusionLeg::Lexical,
                    detail: error.to_string(),
                })?;
            let vector_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if panic_vector {
                    std::panic::panic_any("injected vector hybrid leg panic");
                }
                search_pinned(
                    admitted.pool.as_deref(),
                    &admitted.snapshot,
                    &admitted.active_segment,
                    &self.accounting,
                    admitted.generation,
                    self.epoch_identity(),
                    vector_query,
                    vector_k,
                    options,
                    control.clone(),
                    GraphBoundMode::Shared,
                    started,
                )
                .map_err(crate::fusion::FusionError::from)
            }))
            .unwrap_or_else(|_| {
                Err(crate::fusion::FusionError::LegPanic {
                    leg: crate::fusion::FusionLeg::Vector,
                    detail: "vector hybrid leg panicked",
                })
            });
            let (lexical_thread_name, lexical_result) =
                lexical_thread.join().unwrap_or_else(|_| {
                    (
                        Some("zeppelin-fts".to_owned()),
                        Err(crate::fusion::FusionError::LegPanic {
                            leg: crate::fusion::FusionLeg::Lexical,
                            detail: "lexical hybrid leg panicked",
                        }),
                    )
                });
            Ok::<_, crate::fusion::FusionError>((
                vector_result,
                lexical_result,
                lexical_thread_name,
            ))
        })?;
        let (vector_result, lexical_result, lexical_thread_name) = parallel;
        #[cfg(any(test, feature = "test-support"))]
        if let Ok(mut receipt) = self.hybrid_execution_receipt.lock() {
            *receipt = Some(HybridExecutionReceipt {
                vector_thread: std::thread::current().id(),
                lexical_thread_name,
                vector_completed: true,
                lexical_completed: true,
            });
        }
        #[cfg(not(any(test, feature = "test-support")))]
        let _ = lexical_thread_name;
        let (vector_outcome, (lexical, lexical_counters, lexical_expansions)) =
            resolve_hybrid_leg_results(vector_result, lexical_result)?;
        let crate::ingest::SearchOutcome {
            candidates,
            stats: scan,
            graph_stats: graph,
            generation,
            epoch,
            diagnostics: vector_diagnostics,
        } = vector_outcome;
        let vector = candidates
            .into_iter()
            .map(|candidate| {
                let document = candidate.document().map(|version| version.doc_id());
                let squared_l2 = -f64::from(candidate.score());
                if candidate.exact_score() {
                    crate::fusion::VectorCandidate::exact(document, squared_l2)
                } else {
                    crate::fusion::VectorCandidate::estimated(document, squared_l2)
                }
            })
            .collect();
        let fused = crate::fusion::execute_hybrid(
            hybrid_query,
            || Ok(vector),
            || Ok(lexical),
            |document: &Option<crate::ingest::DocId>| *document,
            |document: &Option<crate::ingest::DocId>| *document,
        )?;
        let diagnostics = crate::diag::QueryDiagnostics::hybrid(crate::diag::HybridDiagnostics {
            snapshot_generation: vector_diagnostics.snapshot_generation,
            indexed_through_seq: vector_diagnostics.indexed_through_seq,
            plan: vector_diagnostics.plan,
            approximate: vector_diagnostics.approximate,
            exact_rescore: vector_diagnostics.exact_rescore,
            requested_k: hybrid_query.k,
            returned: fused.hits.len(),
            scan,
            graph,
            lexical: lexical_counters,
            report: fused.report,
            epoch,
            elapsed: started.elapsed(),
        });
        Ok(crate::ingest::StoreHybridSearchOutcome {
            hits: fused.hits,
            lexical_expansions,
            generation,
            diagnostics,
        })
    }

    fn consume_hybrid_test_fault(&self, leg: crate::fusion::FusionLeg) -> bool {
        #[cfg(any(test, feature = "test-support"))]
        {
            let Ok(mut fault) = self.hybrid_leg_fault.lock() else {
                return false;
            };
            if matches!(*fault, Some(HybridLegTestFault::Panic(candidate)) if candidate == leg) {
                *fault = None;
                return true;
            }
        }
        #[cfg(not(any(test, feature = "test-support")))]
        let _ = leg;
        false
    }

    fn search_with_graph_bound_mode(
        &self,
        request: crate::ingest::SearchRequest<'_>,
        k: usize,
        options: SearchOptions,
        control: QueryControl,
        graph_bound_mode: GraphBoundMode,
    ) -> Result<crate::ingest::SearchOutcome, QueryError> {
        let control = control.with_clock(Arc::clone(&self.clock));
        let started = std::time::Instant::now();
        let admitted = self.admit_vector_search(options)?;
        search_pinned(
            admitted.pool.as_deref(),
            &admitted.snapshot,
            &admitted.active_segment,
            &self.accounting,
            admitted.generation,
            self.epoch_identity(),
            request,
            k,
            options,
            control,
            graph_bound_mode,
            started,
        )
    }

    fn admit_vector_search(
        &self,
        options: SearchOptions,
    ) -> Result<AdmittedVectorSearch<'_>, QueryError> {
        let state = self
            .state
            .lock()
            .map_err(|_| QueryError::Store(StoreError::Synchronization { component: "state" }))?;
        match *state {
            StoreState::Open => {}
            StoreState::Closing => return Err(QueryError::Store(StoreError::Closing)),
            StoreState::Closed => return Err(QueryError::Store(StoreError::Closed)),
        }
        let active_guard = self.active.lock().map_err(|_| {
            QueryError::Store(StoreError::Synchronization {
                component: "active segment",
            })
        })?;
        let active_state = active_guard
            .as_ref()
            .ok_or(QueryError::Store(StoreError::Closed))?;
        let generation = active_state.generation;
        let active_segment = Arc::clone(&active_state.segment);
        let snapshot = self
            .snapshot
            .read()
            .map_err(|_| {
                QueryError::Store(StoreError::Synchronization {
                    component: "published snapshot",
                })
            })?
            .as_ref()
            .cloned()
            .ok_or(QueryError::Store(StoreError::Closed))?;
        let needs_query_pool = !active_segment.is_empty()
            || match options.tier() {
                SearchTier::Auto => snapshot.segments().iter().any(|segment| {
                    !segment.directory().iter().any(|entry| {
                        entry.kind == crate::segment::layout::RegionKind::GraphNodeBlocks.id()
                    })
                }),
                SearchTier::Exact => false,
                SearchTier::Scan => true,
                SearchTier::Graph(_) => false,
            };
        let pool = if needs_query_pool {
            let mut pool_slot = self.query_pool.lock().map_err(|_| {
                QueryError::Store(StoreError::Synchronization {
                    component: "query pool",
                })
            })?;
            if pool_slot.is_none() {
                let capacity = crate::scan::physical_thread_capacity().map_err(|error| {
                    QueryError::Store(StoreError::QueryPoolCapacity {
                        source: error.to_string(),
                    })
                })?;
                let pool = pool::QueryPool::start(capacity, &self.accounting)
                    .map_err(QueryError::Store)?;
                *pool_slot = Some(Arc::new(pool));
            }
            let pool = pool_slot.as_ref().cloned().ok_or({
                QueryError::Store(StoreError::Synchronization {
                    component: "query pool initialization",
                })
            })?;
            drop(pool_slot);
            Some(pool)
        } else {
            None
        };
        self.active_queries.fetch_add(1, Ordering::Relaxed);
        let active_query = ActiveQuery {
            count: &self.active_queries,
        };
        drop(active_guard);
        drop(state);
        Ok(AdmittedVectorSearch {
            pool,
            snapshot,
            active_segment,
            generation,
            _active_query: active_query,
        })
    }

    #[cfg(test)]
    fn search_independent_for_test(
        &self,
        request: crate::ingest::SearchRequest<'_>,
        k: usize,
        options: SearchOptions,
        control: QueryControl,
    ) -> Result<crate::ingest::SearchOutcome, QueryError> {
        self.search_with_graph_bound_mode(request, k, options, control, GraphBoundMode::Independent)
    }
}

#[derive(Clone, Copy)]
enum StructuredLexicalSource {
    Sealed(usize),
    Active,
}

#[derive(Clone, Copy)]
enum PinnedLexicalQuery<'a> {
    Term(&'a crate::fts::search::TermQuery),
    Structured(&'a crate::fts::query::LexicalQuery),
}

fn structured_lexical_row<'a>(
    snapshot: &'a PublishedSnapshot,
    active: &'a crate::ingest::ActiveSegment,
    sources: &[StructuredLexicalSource],
    doc: crate::fts::search::GlobalDocId,
) -> Result<(&'a str, crate::ingest::DocumentVersion), crate::ingest::StoreLexicalError> {
    let source = usize::try_from(doc.segment)
        .ok()
        .and_then(|slot| sources.get(slot))
        .ok_or(QueryError::Store(StoreError::ActiveRowOverflow))?;
    let row =
        usize::try_from(doc.row).map_err(|_| QueryError::Store(StoreError::ActiveRowOverflow))?;
    match source {
        StructuredLexicalSource::Sealed(ordinal) => {
            let segment = snapshot
                .segments()
                .get(*ordinal)
                .ok_or(QueryError::Store(StoreError::ActiveRowOverflow))?;
            let text = segment
                .stored_text()
                .map_err(StoreError::Segment)
                .map_err(QueryError::Store)?
                .and_then(|rows| rows.row(row).flatten())
                .ok_or(crate::ingest::StoreLexicalError::MissingStoredText {
                    segment_id: segment.meta().id,
                    row: doc.row,
                })?;
            let document = segment
                .document_version(row)
                .map_err(StoreError::Segment)
                .map_err(QueryError::Store)?
                .ok_or(crate::ingest::StoreLexicalError::MissingDocumentIdentity {
                    segment_id: segment.meta().id,
                })?;
            Ok((text, document))
        }
        StructuredLexicalSource::Active => {
            let text = active
                .text(row)
                .map_err(QueryError::Store)?
                .ok_or(QueryError::Store(StoreError::ActiveRowOverflow))?;
            let document = active
                .document(row)
                .ok_or(QueryError::Store(StoreError::ActiveRowOverflow))?;
            Ok((text, document))
        }
    }
}

type ExactLexicalLeg = (
    Vec<crate::fusion::LexicalCandidate<Option<crate::ingest::DocId>>>,
    crate::fts::search::SearchCounters,
    Vec<crate::fts::query::LexicalExpansion>,
);

fn resolve_hybrid_leg_results<Vector, Lexical>(
    vector: Result<Vector, crate::fusion::FusionError>,
    lexical: Result<Lexical, crate::fusion::FusionError>,
) -> Result<(Vector, Lexical), crate::fusion::FusionError> {
    fn control_priority(error: &crate::fusion::FusionError) -> Option<u8> {
        match error {
            crate::fusion::FusionError::ReadCancelled { .. } => Some(0),
            crate::fusion::FusionError::Timeout { .. } => Some(1),
            crate::fusion::FusionError::Cancelled { .. } => Some(2),
            _ => None,
        }
    }

    match (vector, lexical) {
        (Ok(vector), Ok(lexical)) => Ok((vector, lexical)),
        (Err(vector), Ok(_)) => Err(vector),
        (Ok(_), Err(lexical)) => Err(lexical),
        (Err(vector), Err(lexical)) => {
            match (control_priority(&vector), control_priority(&lexical)) {
                (Some(vector_priority), Some(lexical_priority)) => {
                    if vector_priority <= lexical_priority {
                        Err(vector)
                    } else {
                        Err(lexical)
                    }
                }
                (Some(_), None) => Err(vector),
                (None, Some(_)) => Err(lexical),
                (None, None) => Err(vector),
            }
        }
    }
}

fn exact_structured_lexical_leg(
    snapshot: &PublishedSnapshot,
    active: &crate::ingest::ActiveSegment,
    accounting: &Arc<stats::Accounting>,
    query: &crate::fts::query::LexicalQuery,
    cancellation: &QueryCancellation<'_>,
) -> Result<ExactLexicalLeg, crate::fusion::FusionError> {
    let lexical_error = |detail: String| crate::fusion::FusionError::Leg {
        leg: crate::fusion::FusionLeg::Lexical,
        kind: crate::fusion::LegFailureKind::Lexical,
        detail,
    };
    let mut index = crate::fts::index::LexicalIndex::new();
    let mut alive_sets = Vec::new();
    let mut sources = Vec::new();
    for (ordinal, segment) in snapshot.segments().iter().enumerate() {
        cancellation
            .check_graph()
            .map_err(QueryError::Scan)
            .map_err(crate::fusion::FusionError::from)?;
        if let Some(postings) =
            segment
                .query_postings()
                .map_err(|error| crate::fusion::FusionError::Leg {
                    leg: crate::fusion::FusionLeg::Lexical,
                    kind: crate::fusion::LegFailureKind::Store(error.kind()),
                    detail: error.to_string(),
                })?
        {
            let alive = segment
                .query_alive()
                .map_err(|error| crate::fusion::FusionError::Leg {
                    leg: crate::fusion::FusionLeg::Lexical,
                    kind: crate::fusion::LegFailureKind::Store(error.kind()),
                    detail: error.to_string(),
                })?;
            index
                .push_shared_with_live_rows(postings, alive.alive_bitmap())
                .map_err(|error| lexical_error(error.to_string()))?;
            alive_sets.push(alive);
            sources.push(StructuredLexicalSource::Sealed(ordinal));
        }
    }
    if active.has_text() {
        let sealed =
            active
                .sealed_lexical(accounting)
                .map_err(|error| crate::fusion::FusionError::Leg {
                    leg: crate::fusion::FusionLeg::Lexical,
                    kind: crate::fusion::LegFailureKind::Store(error.kind()),
                    detail: error.to_string(),
                })?;
        let alive = Arc::new(
            active
                .alive()
                .map_err(|error| crate::fusion::FusionError::Leg {
                    leg: crate::fusion::FusionLeg::Lexical,
                    kind: crate::fusion::LegFailureKind::Store(error.kind()),
                    detail: error.to_string(),
                })?,
        );
        index
            .push_shared_with_live_rows(sealed, alive.alive_bitmap())
            .map_err(|error| lexical_error(error.to_string()))?;
        alive_sets.push(alive);
        sources.push(StructuredLexicalSource::Active);
    }
    let vocabulary = crate::fts::query::vocabulary(index.terms());
    let expansions = crate::fts::query::expand(query, &vocabulary)
        .map_err(|error| lexical_error(error.to_string()))?;
    if index.segments().is_empty() || expansions.is_empty() {
        return Ok((
            Vec::new(),
            crate::fts::search::SearchCounters::default(),
            expansions,
        ));
    }
    let allow_lists = alive_sets
        .iter()
        .map(|alive| alive.alive_bitmap())
        .collect::<Vec<_>>();
    let fields = query.fields();
    let all_rows = usize::try_from(index.document_count()).unwrap_or(usize::MAX);
    let mut counters = crate::fts::search::SearchCounters::default();
    let mut aggregate = BTreeMap::<crate::fts::search::GlobalDocId, f64>::new();
    for expansion in &expansions {
        let term_query = crate::fts::search::TermQuery {
            terms: vec![expansion.term.clone()],
            fields: fields.clone(),
        };
        let result = crate::fts::search::search_allow_list_driven_controlled(
            &index,
            &term_query,
            all_rows,
            crate::fts::bm25::Bm25Params::beir(),
            &allow_lists,
            || cancellation.check_graph(),
        )
        .map_err(|error| match error {
            crate::fts::search::ControlledSearchError::Index(error) => {
                lexical_error(error.to_string())
            }
            crate::fts::search::ControlledSearchError::Control(error) => {
                crate::fusion::FusionError::from(QueryError::Scan(error))
            }
        })?;
        counters.docs_evaluated = counters
            .docs_evaluated
            .saturating_add(result.counters.docs_evaluated);
        counters.postings_decoded = counters
            .postings_decoded
            .saturating_add(result.counters.postings_decoded);
        counters.blocks_decoded = counters
            .blocks_decoded
            .saturating_add(result.counters.blocks_decoded);
        counters.blocks_skipped = counters
            .blocks_skipped
            .saturating_add(result.counters.blocks_skipped);
        let boost = f64::from(expansion.boost_thousandths) / 1_000.0;
        for hit in result.hits {
            *aggregate.entry(hit.doc).or_default() += hit.score * boost;
        }
    }
    if let Some((terms, slop)) = query.phrase_constraint() {
        let analyzer = crate::fts::tokenizer::Analyzer::new(
            crate::fts::tokenizer::TokenizerConfig::text_default(),
        )
        .map_err(|error| lexical_error(error.to_string()))?;
        let mut retained = BTreeMap::new();
        for (doc, score) in aggregate {
            let (text, _) = structured_lexical_row(snapshot, active, &sources, doc)
                .map_err(|error| lexical_error(error.to_string()))?;
            if crate::fts::query::phrase_matches(&analyzer, text, terms, slop) {
                retained.insert(doc, score);
            }
        }
        aggregate = retained;
    }
    let mut scored = aggregate.into_iter().collect::<Vec<_>>();
    scored.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(left.0.cmp(&right.0))
    });
    let mut joined = Vec::with_capacity(scored.len());
    for (doc, score) in scored {
        let source = usize::try_from(doc.segment)
            .ok()
            .and_then(|slot| sources.get(slot))
            .ok_or_else(|| lexical_error("lexical source ordinal is out of range".to_owned()))?;
        let row = usize::try_from(doc.row)
            .map_err(|_| lexical_error("lexical row exceeds usize".to_owned()))?;
        let document = match source {
            StructuredLexicalSource::Sealed(ordinal) => snapshot
                .segments()
                .get(*ordinal)
                .ok_or_else(|| lexical_error("lexical sealed source is absent".to_owned()))?
                .document_version(row)
                .map_err(|error| lexical_error(error.to_string()))?
                .map(|version| version.doc_id()),
            StructuredLexicalSource::Active => active.document(row).map(|version| version.doc_id()),
        };
        joined.push(crate::fusion::LexicalCandidate::new(document, score));
    }
    Ok((joined, counters, expansions))
}

fn hybrid_vector_candidate_limit(
    snapshot: &PublishedSnapshot,
    active: &crate::ingest::ActiveSegment,
) -> Result<usize, crate::fusion::FusionError> {
    snapshot
        .segments()
        .iter()
        .try_fold(active.row_count(), |total, segment| {
            usize::try_from(segment.meta().row_count)
                .ok()
                .and_then(|rows| total.checked_add(rows))
                .ok_or_else(|| {
                    crate::fusion::FusionError::from(QueryError::Scan(
                        crate::scan::ScanError::ArithmeticOverflow,
                    ))
                })
        })
}

fn exact_lexical_leg(
    snapshot: &PublishedSnapshot,
    active: &crate::ingest::ActiveSegment,
    accounting: &Arc<stats::Accounting>,
    query: &crate::fts::search::TermQuery,
    cancellation: &QueryCancellation<'_>,
) -> Result<ExactLexicalLeg, crate::fusion::FusionError> {
    enum Source {
        Sealed(usize),
        Active,
    }
    let mut index = crate::fts::index::LexicalIndex::new();
    let mut alive_sets = Vec::new();
    let mut sources = Vec::new();
    for (ordinal, segment) in snapshot.segments().iter().enumerate() {
        cancellation
            .check_graph()
            .map_err(QueryError::Scan)
            .map_err(crate::fusion::FusionError::from)?;
        if let Some(postings) =
            segment
                .query_postings()
                .map_err(|error| crate::fusion::FusionError::Leg {
                    leg: crate::fusion::FusionLeg::Lexical,
                    kind: crate::fusion::LegFailureKind::Store(error.kind()),
                    detail: error.to_string(),
                })?
        {
            let alive = segment
                .query_alive()
                .map_err(|error| crate::fusion::FusionError::Leg {
                    leg: crate::fusion::FusionLeg::Lexical,
                    kind: crate::fusion::LegFailureKind::Store(error.kind()),
                    detail: error.to_string(),
                })?;
            let live_rows = alive.alive_bitmap();
            index
                .push_shared_with_live_rows(postings, live_rows)
                .map_err(|error| crate::fusion::FusionError::Leg {
                    leg: crate::fusion::FusionLeg::Lexical,
                    kind: crate::fusion::LegFailureKind::Lexical,
                    detail: error.to_string(),
                })?;
            alive_sets.push(alive);
            sources.push(Source::Sealed(ordinal));
        }
    }
    if active.has_text() {
        let sealed =
            active
                .sealed_lexical(accounting)
                .map_err(|error| crate::fusion::FusionError::Leg {
                    leg: crate::fusion::FusionLeg::Lexical,
                    kind: crate::fusion::LegFailureKind::Store(error.kind()),
                    detail: error.to_string(),
                })?;
        let active_alive =
            Arc::new(
                active
                    .alive()
                    .map_err(|error| crate::fusion::FusionError::Leg {
                        leg: crate::fusion::FusionLeg::Lexical,
                        kind: crate::fusion::LegFailureKind::Store(error.kind()),
                        detail: error.to_string(),
                    })?,
            );
        let live_rows = active_alive.alive_bitmap();
        index
            .push_shared_with_live_rows(sealed, live_rows)
            .map_err(|error| crate::fusion::FusionError::Leg {
                leg: crate::fusion::FusionLeg::Lexical,
                kind: crate::fusion::LegFailureKind::Lexical,
                detail: error.to_string(),
            })?;
        alive_sets.push(active_alive);
        sources.push(Source::Active);
    }
    if index.segments().is_empty() {
        return Ok((
            Vec::new(),
            crate::fts::search::SearchCounters::default(),
            query
                .terms
                .iter()
                .cloned()
                .map(|term| crate::fts::query::LexicalExpansion {
                    term,
                    boost_thousandths: 1_000,
                    kind: crate::fts::query::LexicalMatchKind::Term,
                })
                .collect(),
        ));
    }
    let k = usize::try_from(index.document_count()).unwrap_or(usize::MAX);
    let allow_lists = alive_sets
        .iter()
        .map(|alive| alive.alive_bitmap())
        .collect::<Vec<_>>();
    let result = crate::fts::search::search_allow_list_driven_controlled(
        &index,
        query,
        k,
        crate::fts::bm25::Bm25Params::beir(),
        &allow_lists,
        || cancellation.check_graph(),
    )
    .map_err(|error| match error {
        crate::fts::search::ControlledSearchError::Index(error) => {
            crate::fusion::FusionError::Leg {
                leg: crate::fusion::FusionLeg::Lexical,
                kind: crate::fusion::LegFailureKind::Lexical,
                detail: error.to_string(),
            }
        }
        crate::fts::search::ControlledSearchError::Control(error) => {
            crate::fusion::FusionError::from(QueryError::Scan(error))
        }
    })?;
    let mut joined = Vec::with_capacity(result.hits.len());
    for hit in result.hits {
        let source = usize::try_from(hit.doc.segment)
            .ok()
            .and_then(|slot| sources.get(slot))
            .ok_or_else(|| crate::fusion::FusionError::Leg {
                leg: crate::fusion::FusionLeg::Lexical,
                kind: crate::fusion::LegFailureKind::Invariant,
                detail: "lexical source ordinal is out of range".to_owned(),
            })?;
        let row = usize::try_from(hit.doc.row).map_err(|_| crate::fusion::FusionError::Leg {
            leg: crate::fusion::FusionLeg::Lexical,
            kind: crate::fusion::LegFailureKind::Invariant,
            detail: "lexical row exceeds usize".to_owned(),
        })?;
        let document = match source {
            Source::Sealed(ordinal) => snapshot
                .segments()
                .get(*ordinal)
                .ok_or_else(|| crate::fusion::FusionError::Leg {
                    leg: crate::fusion::FusionLeg::Lexical,
                    kind: crate::fusion::LegFailureKind::Invariant,
                    detail: "lexical sealed source is absent".to_owned(),
                })?
                .document_version(row)
                .map_err(|error| crate::fusion::FusionError::Leg {
                    leg: crate::fusion::FusionLeg::Lexical,
                    kind: crate::fusion::LegFailureKind::Segment,
                    detail: error.to_string(),
                })?
                .map(|version| version.doc_id()),
            Source::Active => active.document(row).map(|version| version.doc_id()),
        };
        joined.push(crate::fusion::LexicalCandidate::new(document, hit.score));
    }
    Ok((
        joined,
        result.counters,
        query
            .terms
            .iter()
            .cloned()
            .map(|term| crate::fts::query::LexicalExpansion {
                term,
                boost_thousandths: 1_000,
                kind: crate::fts::query::LexicalMatchKind::Term,
            })
            .collect(),
    ))
}

#[allow(clippy::too_many_arguments)]
fn search_pinned(
    pool: Option<&pool::QueryPool>,
    snapshot: &Arc<PublishedSnapshot>,
    active: &crate::ingest::ActiveSegment,
    accounting: &Arc<stats::Accounting>,
    generation: u64,
    epoch: Option<crate::epoch::EpochIdentity>,
    request: crate::ingest::SearchRequest<'_>,
    k: usize,
    options: SearchOptions,
    control: QueryControl,
    graph_bound_mode: GraphBoundMode,
    started: std::time::Instant,
) -> Result<crate::ingest::SearchOutcome, QueryError> {
    use crate::ingest::{GraphSearchStats, RowSource, SearchOutcome};
    use crate::quant::{prepare_bit4_query, prepare_int8_query};
    use crate::scan::{Int8Factors, ScanQuery, ScanRequest, ScanRows, ScanStats};

    let scan_options = options.scan();
    let bit4_query = prepare_bit4_query(request.vector(), 0)
        .map_err(crate::scan::ScanError::Quant)
        .map_err(QueryError::Scan)?;
    let int8_query = prepare_int8_query(request.vector())
        .map_err(crate::scan::ScanError::Quant)
        .map_err(QueryError::Scan)?;
    let mut candidates = Vec::new();
    let mut dims_touched = 0_u64;
    let mut bytes_read = 0_u64;
    let mut worker_thread_ids = Vec::new();
    let mut graph_stats = GraphSearchStats::default();
    let mut plans = Vec::new();
    let auto_graph_options = if matches!(options.tier(), SearchTier::Auto)
        && snapshot.segments().iter().any(|segment| {
            segment
                .directory()
                .iter()
                .any(|entry| entry.kind == crate::segment::layout::RegionKind::GraphNodeBlocks.id())
        }) {
        Some(auto_graph_search_options(snapshot)?)
    } else {
        None
    };

    if matches!(options.tier(), SearchTier::Graph(_)) {
        for segment in snapshot.segments() {
            if !segment
                .directory()
                .iter()
                .any(|entry| entry.kind == crate::segment::layout::RegionKind::GraphNodeBlocks.id())
            {
                return Err(QueryError::Store(StoreError::GraphUnavailable {
                    segment_id: segment.meta().id,
                }));
            }
        }
    }
    let full_precision = matches!(options.tier(), SearchTier::Exact | SearchTier::Graph(_))
        || (matches!(options.tier(), SearchTier::Auto)
            && auto_uses_full_precision(snapshot, active));

    if !active.is_empty() {
        let alive = active.alive().map_err(QueryError::Store)?;
        let exact_score = full_precision;
        let outcome = match options.tier() {
            SearchTier::Auto if full_precision => {
                let lease = SnapshotLease::new_at(Arc::clone(snapshot), generation);
                let cancellation = QueryCancellation::new(&control, &lease);
                scan_active_squared_l2(active, &alive, request.vector(), k, &cancellation)?
            }
            SearchTier::Auto | SearchTier::Scan => {
                let query_pool = pool.ok_or(QueryError::Store(StoreError::Synchronization {
                    component: "active scan-tier query pool",
                }))?;
                query_pool.execute(
                    ScanRequest {
                        query: ScanQuery::Bit4(&bit4_query),
                        rows: ScanRows::Bit4RowMajor {
                            codes: active.codes(),
                            factors: active.factors(),
                        },
                        row_mask: Some(alive.scan_mask()),
                    },
                    k,
                    scan_options,
                    control.clone(),
                    SnapshotLease::new_at(Arc::clone(snapshot), generation),
                )?
            }
            SearchTier::Exact | SearchTier::Graph(_) => {
                let lease = SnapshotLease::new_at(Arc::clone(snapshot), generation);
                let cancellation = QueryCancellation::new(&control, &lease);
                scan_active_squared_l2(active, &alive, request.vector(), k, &cancellation)?
            }
        };
        merge_store_outcome(
            outcome,
            RowSource::Active,
            |row| Ok(active.document(row)),
            &mut candidates,
            &mut dims_touched,
            &mut bytes_read,
            &mut worker_thread_ids,
            exact_score,
        )?;
        plans.push(crate::planner::SegmentPlan::unfiltered_scan(
            RowSource::Active,
            crate::planner::SegmentTier::ActiveScan,
            alive.live_count(),
        ));
        if matches!(graph_bound_mode, GraphBoundMode::Shared) {
            retain_global_top_k(&mut candidates, k);
        }
    }

    let mut ordered_segments = snapshot.segments().iter().collect::<Vec<_>>();
    if matches!(graph_bound_mode, GraphBoundMode::Shared) {
        // Larger immutable segments have more opportunities to supply the
        // first competitive top-k. Row count is already in the manifest, so
        // this ordering tightens the bound without query-time artifact I/O.
        ordered_segments.sort_unstable_by(|left, right| {
            right
                .meta()
                .row_count
                .cmp(&left.meta().row_count)
                .then_with(|| left.meta().id.cmp(&right.meta().id))
        });
    }

    for segment in ordered_segments {
        let alive = segment.query_alive().map_err(QueryError::Store)?;
        let source = RowSource::Sealed(segment.meta().id);
        // Automatic tiering follows the artifact that is atomically published
        // now, not the tier policy's desired future state. A due-but-unbuilt
        // graph is therefore scanned silently and correctly: that is the
        // adaptive ladder working, not a fallback hiding a broken contract.
        let graph_options = match options.tier() {
            SearchTier::Graph(graph_options) => Some(graph_options),
            SearchTier::Auto
                if segment.directory().iter().any(|entry| {
                    entry.kind == crate::segment::layout::RegionKind::GraphNodeBlocks.id()
                }) =>
            {
                auto_graph_options
            }
            SearchTier::Auto | SearchTier::Exact | SearchTier::Scan => None,
        };
        if let Some(graph_options) = graph_options {
            let lease = SnapshotLease::new_at(Arc::clone(snapshot), generation);
            let cancellation = QueryCancellation::new(&control, &lease);
            cancellation.check_graph().map_err(map_scan_error)?;
            let (graph, graph_validated, prepared_entry_seed_discovered, norm_range) =
                match graph_bound_mode {
                    GraphBoundMode::Shared => {
                        let prepared = segment
                            .graph_search_cache
                            .prepare_shared(segment, &cancellation)
                            .map_err(map_graph_cache_error)?;
                        (
                            prepared.graph,
                            prepared.graph_validated,
                            prepared.entry_seed_discovered,
                            Some(prepared.norm_range),
                        )
                    }
                    #[cfg(test)]
                    GraphBoundMode::Independent => {
                        let (graph, graph_validated) = segment
                            .graph_search_cache
                            .bind_graph(segment)
                            .map_err(map_graph_cache_error)?;
                        (graph, graph_validated, false, None)
                    }
                };
            let rescore = query_rescore_rows(segment)?;
            let node_count = graph.node_count() as usize;
            let segment_k = k.min(node_count);
            let has_tombstones = alive.tombstone_count() != 0;
            let target_live_k = if has_tombstones {
                usize::try_from(alive.live_count())
                    .map_err(|_| QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?
                    .min(k)
            } else {
                segment_k
            };
            let maximum_candidate_k = if has_tombstones {
                let tombstone_count = usize::try_from(alive.tombstone_count())
                    .map_err(|_| QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
                target_live_k
                    .checked_add(tombstone_count)
                    .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?
                    .min(node_count)
                    .max(segment_k)
            } else {
                segment_k
            };
            if let Some(ef) = graph_options.ef()
                && ef < segment_k
            {
                return Err(QueryError::Graph(
                    crate::graph::search::GraphSearchError::AdaptiveEf(
                        crate::graph::search::AdaptiveEfError::ExplicitBelowK { k: segment_k, ef },
                    ),
                ));
            }
            let request_for = |candidate_k| {
                let graph_request = crate::graph::search::GraphSearchRequest::new(
                    request.vector(),
                    candidate_k,
                    graph_options.seed(),
                )
                .with_profile(graph_options.profile());
                // An explicit ef controls the caller's live-candidate window.
                // Tombstones are engine state, so reserve only the additional
                // width needed to replace deleted rows instead of turning a
                // previously valid ef=k request into an internal below-k error.
                graph_options.ef().map_or(graph_request, |ef| {
                    graph_request.with_ef(ef.max(candidate_k))
                })
            };
            let mut candidate_k = segment_k;
            let mut graph_request = request_for(candidate_k);
            let effective_ef = graph_request
                .effective_ef(graph.node_count() as usize)
                .map_err(crate::graph::search::GraphSearchError::AdaptiveEf)
                .map_err(QueryError::Graph)?;
            let requestable_max_k = maximum_candidate_k;
            let scratch_ef = if has_tombstones && requestable_max_k > candidate_k {
                request_for(requestable_max_k)
                    .effective_ef(node_count)
                    .map_err(crate::graph::search::GraphSearchError::AdaptiveEf)
                    .map_err(QueryError::Graph)?
            } else {
                effective_ef
            };
            if let (Some(norm_range), Some(competitive_distance)) =
                (norm_range, global_competitive_distance(&candidates, k))
                && norm_range.squared_l2_upper_bound(request.vector()) <= f64::from(f32::MAX)
                && (norm_range.squared_l2_lower_bound(request.vector()) as f32)
                    > competitive_distance
            {
                cancellation.check_graph().map_err(map_scan_error)?;
                graph_stats.graph_validations = graph_stats
                    .graph_validations
                    .checked_add(usize::from(graph_validated))
                    .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
                graph_stats.entry_seed_discoveries = graph_stats
                    .entry_seed_discoveries
                    .checked_add(usize::from(prepared_entry_seed_discovered))
                    .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
                graph_stats.segments_pruned_by_bound = graph_stats
                    .segments_pruned_by_bound
                    .checked_add(1)
                    .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
                plans.push(crate::planner::SegmentPlan::unfiltered_pruned(
                    source,
                    alive.live_count(),
                ));
                continue;
            }
            let mut scratch = segment
                .graph_search_cache
                .checkout(graph, scratch_ef, accounting)
                .map_err(map_graph_cache_error)?;
            let entry_seed_discovered =
                prepared_entry_seed_discovered || scratch.entry_seed_discovered();
            let entries = scratch.entries();
            let mut searcher = crate::graph::search::GraphSearcher::with_entry_row_ids(
                graph,
                rescore,
                entries,
                scratch.scratch_mut().map_err(map_graph_error)?,
            )
            .map_err(map_graph_error)?
            .with_rescore_validator(segment);
            let mut traversal_dims_touched = 0_u64;
            let mut traversal_bytes_read = 0_u64;
            let mut traversal_epoch_clears = 0_usize;
            let mut traversal_candidates_scored = 0_usize;
            let mut traversal_candidates_rescored = 0_usize;
            let result = loop {
                let result = searcher
                    .search(graph_request, Some(&cancellation))
                    .map_err(map_graph_error)?;
                let counters = result.counters();
                traversal_dims_touched = traversal_dims_touched
                    .checked_add(counters.dims_touched())
                    .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
                traversal_bytes_read = traversal_bytes_read
                    .checked_add(counters.bytes_read())
                    .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
                traversal_epoch_clears = traversal_epoch_clears
                    .checked_add(usize::from(counters.visited_epoch_cleared()))
                    .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
                traversal_candidates_scored = traversal_candidates_scored
                    .checked_add(counters.candidates_scored())
                    .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
                traversal_candidates_rescored = traversal_candidates_rescored
                    .checked_add(counters.candidates_rescored())
                    .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;

                if !has_tombstones {
                    break result;
                }
                let retained_live = result
                    .candidates()
                    .iter()
                    .filter(|candidate| alive.is_alive(candidate.row_id()))
                    .count();
                if retained_live >= target_live_k {
                    break result;
                }
                if candidate_k >= requestable_max_k {
                    return Err(QueryError::Graph(
                        crate::graph::search::GraphSearchError::Geometry(format!(
                            "graph traversal exhausted {candidate_k} candidates but retained {retained_live} live rows, expected {target_live_k}"
                        )),
                    ));
                }
                let widened_k = candidate_k
                    .saturating_mul(2)
                    .max(candidate_k.saturating_add(1))
                    .min(requestable_max_k);
                if widened_k <= candidate_k {
                    return Err(QueryError::Graph(
                        crate::graph::search::GraphSearchError::Geometry(format!(
                            "graph traversal could not widen beyond {candidate_k} candidates"
                        )),
                    ));
                }
                candidate_k = widened_k;
                graph_request = request_for(candidate_k);
            };
            dims_touched = dims_touched
                .checked_add(traversal_dims_touched)
                .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
            bytes_read = bytes_read
                .checked_add(traversal_bytes_read)
                .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
            let caller_thread = std::thread::current().id();
            if !worker_thread_ids.contains(&caller_thread) {
                worker_thread_ids.push(caller_thread);
            }
            graph_stats.segments_traversed = graph_stats
                .segments_traversed
                .checked_add(1)
                .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
            graph_stats.graph_validations = graph_stats
                .graph_validations
                .checked_add(usize::from(graph_validated))
                .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
            graph_stats.entry_seed_discoveries = graph_stats
                .entry_seed_discoveries
                .checked_add(usize::from(entry_seed_discovered))
                .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
            graph_stats.visited_epoch_clears = graph_stats
                .visited_epoch_clears
                .checked_add(traversal_epoch_clears)
                .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
            graph_stats.candidates_scored = graph_stats
                .candidates_scored
                .checked_add(traversal_candidates_scored)
                .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
            graph_stats.candidates_rescored = graph_stats
                .candidates_rescored
                .checked_add(traversal_candidates_rescored)
                .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
            for candidate in result.candidates() {
                if !alive.is_alive(candidate.row_id()) {
                    continue;
                }
                let score = -(candidate.distance() as f32);
                if !score.is_finite() {
                    return Err(QueryError::Graph(
                        crate::graph::search::GraphSearchError::Geometry(format!(
                            "graph result distance for row {} is not representable as f32",
                            candidate.row_id()
                        )),
                    ));
                }
                candidates.push(crate::ingest::SearchCandidate::new(
                    crate::ingest::GlobalRowId::new(source, candidate.row_id()),
                    segment
                        .document_version(candidate.row_id() as usize)
                        .map_err(StoreError::Segment)
                        .map_err(QueryError::Store)?,
                    score,
                    true,
                ));
            }
            plans.push(crate::planner::SegmentPlan::unfiltered_graph(
                source,
                alive.live_count(),
                graph_options.ef(),
                effective_ef,
                graph_options.profile(),
            ));
            if matches!(graph_bound_mode, GraphBoundMode::Shared) {
                retain_global_top_k(&mut candidates, k);
            }
            continue;
        }

        let outcome = if full_precision {
            let lease = SnapshotLease::new_at(Arc::clone(snapshot), generation);
            let cancellation = QueryCancellation::new(&control, &lease);
            let vectors = exact_rescore_rows(segment)?;
            scan_squared_l2(
                vectors,
                segment.meta().row_count as usize,
                &alive,
                request.vector(),
                k,
                &cancellation,
            )?
        } else {
            let query_pool = pool.ok_or(QueryError::Store(StoreError::Synchronization {
                component: "sealed scan-tier query pool",
            }))?;
            match segment.meta().scheme {
                0 => query_pool.execute(
                    ScanRequest {
                        query: ScanQuery::F32(request.vector()),
                        rows: ScanRows::F32BorrowedRowMajor(
                            segment
                                .f32_codes()
                                .map_err(StoreError::Segment)
                                .map_err(QueryError::Store)?,
                        ),
                        row_mask: Some(alive.scan_mask()),
                    },
                    k,
                    scan_options,
                    control.clone(),
                    SnapshotLease::new_at(Arc::clone(snapshot), generation),
                )?,
                4 => query_pool.execute(
                    ScanRequest {
                        query: ScanQuery::Bit4(&bit4_query),
                        rows: ScanRows::Bit4RowMajor {
                            codes: segment
                                .bit4_codes()
                                .map_err(StoreError::Segment)
                                .map_err(QueryError::Store)?,
                            factors: segment
                                .bit4_factors()
                                .map_err(StoreError::Segment)
                                .map_err(QueryError::Store)?,
                        },
                        row_mask: Some(alive.scan_mask()),
                    },
                    k,
                    scan_options,
                    control.clone(),
                    SnapshotLease::new_at(Arc::clone(snapshot), generation),
                )?,
                2 => {
                    let factors = segment
                        .int8_factors()
                        .map_err(StoreError::Segment)
                        .map_err(QueryError::Store)?
                        .iter()
                        .map(|factor| Int8Factors::new(factor.scale, factor.offset))
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|_| {
                            QueryError::Store(StoreError::Segment(
                                crate::segment::SegmentError::Geometry(
                                    "Int8 factor is not finite and non-negative".to_owned(),
                                ),
                            ))
                        })?;
                    query_pool.execute(
                        ScanRequest {
                            query: ScanQuery::Int8(&int8_query),
                            rows: ScanRows::Int8RowMajor {
                                codes: segment
                                    .int8_codes()
                                    .map_err(StoreError::Segment)
                                    .map_err(QueryError::Store)?,
                                factors: &factors,
                            },
                            row_mask: Some(alive.scan_mask()),
                        },
                        k,
                        scan_options,
                        control.clone(),
                        SnapshotLease::new_at(Arc::clone(snapshot), generation),
                    )?
                }
                scheme => {
                    return Err(QueryError::Store(StoreError::Segment(
                        crate::segment::SegmentError::Geometry(format!(
                            "store search does not support sealed scheme {scheme}"
                        )),
                    )));
                }
            }
        };
        let exact_score = full_precision || segment.meta().scheme == 0;
        merge_store_outcome(
            outcome,
            source,
            |row| {
                segment
                    .document_version(row)
                    .map_err(StoreError::Segment)
                    .map_err(QueryError::Store)
            },
            &mut candidates,
            &mut dims_touched,
            &mut bytes_read,
            &mut worker_thread_ids,
            exact_score,
        )?;
        let tier = if segment
            .directory()
            .iter()
            .any(|entry| entry.kind == crate::segment::layout::RegionKind::GraphNodeBlocks.id())
        {
            crate::planner::SegmentTier::SealedGraph
        } else {
            crate::planner::SegmentTier::SealedScan
        };
        plans.push(crate::planner::SegmentPlan::unfiltered_scan(
            source,
            tier,
            alive.live_count(),
        ));
        if matches!(graph_bound_mode, GraphBoundMode::Shared) {
            retain_global_top_k(&mut candidates, k);
        }
    }

    retain_global_top_k(&mut candidates, k);
    let stats = ScanStats {
        dims_touched,
        bytes_read,
        threads_used: worker_thread_ids.len(),
        worker_thread_ids,
    };
    let diagnostics = crate::diag::QueryDiagnostics::vector(crate::diag::VectorDiagnostics {
        snapshot_generation: generation,
        indexed_through_seq: active
            .indexed_through_seq()
            .max(crate::wal::LogSeq::new(snapshot.absorbed_through())),
        approximate: plans.iter().any(|plan| plan.approximate),
        exact_rescore: candidates.iter().all(|candidate| candidate.exact_score()),
        requested_k: k,
        returned: candidates.len(),
        budget_exhausted: false,
        plan: plans,
        scan: stats.clone(),
        graph: graph_stats,
        epoch,
        elapsed: started.elapsed(),
    });
    Ok(SearchOutcome {
        candidates,
        stats,
        graph_stats,
        generation,
        epoch,
        diagnostics,
    })
}

pub(crate) fn auto_uses_full_precision(
    snapshot: &PublishedSnapshot,
    active: &crate::ingest::ActiveSegment,
) -> bool {
    let mut has_exact = false;
    let mut has_estimated = !active.is_empty();
    for segment in snapshot.segments() {
        if segment
            .directory()
            .iter()
            .any(|entry| entry.kind == crate::segment::layout::RegionKind::GraphNodeBlocks.id())
        {
            return true;
        }
        if segment.meta().row_count == 0 {
            continue;
        }
        match segment.meta().scheme {
            0 => has_exact = true,
            2 | 4 => has_estimated = true,
            _ => {}
        }
    }
    has_exact && has_estimated
}

pub(crate) fn exact_rescore_rows(
    segment: &crate::segment::reader::SegmentReader,
) -> Result<&[f32], QueryError> {
    segment.rescore_f32().map_err(|source| {
        QueryError::Store(StoreError::Segment(crate::segment::SegmentError::Geometry(
            format!(
                "exact scores unavailable for segment {}: {source}",
                segment.meta().id
            ),
        )))
    })
}

pub(crate) fn query_rescore_rows(
    segment: &crate::segment::reader::SegmentReader,
) -> Result<&[f32], QueryError> {
    segment.query_rescore_f32().map_err(|source| {
        QueryError::Store(StoreError::Segment(crate::segment::SegmentError::Geometry(
            format!(
                "exact scores unavailable for segment {}: {source}",
                segment.meta().id
            ),
        )))
    })
}

fn retain_global_top_k(candidates: &mut Vec<crate::ingest::SearchCandidate>, k: usize) {
    candidates.sort_unstable_by(|left, right| {
        right
            .score()
            .total_cmp(&left.score())
            .then_with(|| left.row_id().cmp(&right.row_id()))
    });
    candidates.truncate(k);
}

fn global_competitive_distance(
    candidates: &[crate::ingest::SearchCandidate],
    k: usize,
) -> Option<f32> {
    if k == 0 || candidates.len() < k {
        return None;
    }
    candidates
        .get(k.saturating_sub(1))
        .map(|candidate| -candidate.score())
        .filter(|distance| distance.is_finite() && *distance >= 0.0)
}

fn map_graph_cache_error(error: graph_cache::GraphCacheError) -> QueryError {
    match error {
        graph_cache::GraphCacheError::Store(error) => QueryError::Store(error),
        graph_cache::GraphCacheError::Search(error) => map_graph_error(error),
    }
}

fn map_graph_error(error: crate::graph::search::GraphSearchError) -> QueryError {
    match error {
        crate::graph::search::GraphSearchError::ExactRescoreUnavailable(detail) => {
            QueryError::Store(StoreError::Segment(crate::segment::SegmentError::Geometry(
                detail,
            )))
        }
        crate::graph::search::GraphSearchError::Cancelled { partial } => {
            QueryError::Cancelled { partial }
        }
        crate::graph::search::GraphSearchError::Timeout { partial } => {
            QueryError::Timeout { partial }
        }
        crate::graph::search::GraphSearchError::ReadCancelled { partial } => {
            QueryError::ReadCancelled { partial }
        }
        crate::graph::search::GraphSearchError::Scan(error) => QueryError::Scan(error),
        error => QueryError::Graph(error),
    }
}

fn scan_active_squared_l2(
    active: &crate::ingest::ActiveSegment,
    alive: &crate::meta::AliveSet,
    query: &[f32],
    k: usize,
    cancellation: &QueryCancellation<'_>,
) -> Result<crate::scan::ScanOutcome, QueryError> {
    scan_squared_l2(
        active.vectors(),
        active.row_count(),
        alive,
        query,
        k,
        cancellation,
    )
}

fn scan_squared_l2(
    vectors: &[f32],
    row_count: usize,
    alive: &crate::meta::AliveSet,
    query: &[f32],
    k: usize,
    cancellation: &QueryCancellation<'_>,
) -> Result<crate::scan::ScanOutcome, QueryError> {
    if query.is_empty() {
        return Err(QueryError::Scan(crate::scan::ScanError::ZeroDimension));
    }
    let expected = row_count
        .checked_mul(query.len())
        .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
    if vectors.len() != expected {
        return Err(QueryError::Scan(crate::scan::ScanError::RowDataLength {
            dimension: query.len(),
            actual: vectors.len(),
        }));
    }
    let mut candidates = Vec::new();
    let mut scored_rows = 0_u64;
    for row in 0..row_count {
        if row.is_multiple_of(64) {
            cancellation.check_graph().map_err(map_scan_error)?;
        }
        let local_row = u32::try_from(row)
            .map_err(|_| QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
        if !alive.is_alive(local_row) {
            continue;
        }
        let start = row
            .checked_mul(query.len())
            .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
        let end = start
            .checked_add(query.len())
            .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
        let vector = vectors.get(start..end).ok_or(QueryError::Scan(
            crate::scan::ScanError::RowDataLength {
                dimension: query.len(),
                actual: vectors.len(),
            },
        ))?;
        let distance = vector
            .iter()
            .zip(query)
            .map(|(left, right)| {
                let delta = f64::from(*left) - f64::from(*right);
                delta * delta
            })
            .sum::<f64>();
        let score = -(distance as f32);
        if !score.is_finite() {
            return Err(QueryError::Scan(crate::scan::ScanError::NonFiniteScore {
                row_id: row,
            }));
        }
        candidates.push(crate::scan::ScanCandidate { row_id: row, score });
        scored_rows = scored_rows
            .checked_add(1)
            .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
    }
    cancellation.check_graph().map_err(map_scan_error)?;
    candidates.sort_unstable_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.row_id.cmp(&right.row_id))
    });
    candidates.truncate(k);
    let dims = u64::try_from(query.len())
        .map_err(|_| QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
    let dims_touched = scored_rows
        .checked_mul(dims)
        .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
    let bytes_read = dims_touched
        .checked_mul(std::mem::size_of::<f32>() as u64)
        .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
    Ok(crate::scan::ScanOutcome {
        candidates,
        stats: crate::scan::ScanStats {
            dims_touched,
            bytes_read,
            threads_used: 1,
            worker_thread_ids: vec![std::thread::current().id()],
        },
    })
}

fn map_scan_error(error: crate::scan::ScanError) -> QueryError {
    match error {
        crate::scan::ScanError::Cancelled { partial } => QueryError::Cancelled { partial },
        crate::scan::ScanError::Timeout { partial } => QueryError::Timeout { partial },
        crate::scan::ScanError::ReadCancelled { partial } => QueryError::ReadCancelled { partial },
        error => QueryError::Scan(error),
    }
}

#[allow(clippy::too_many_arguments)]
fn merge_store_outcome(
    outcome: crate::scan::ScanOutcome,
    source: crate::ingest::RowSource,
    document: impl Fn(usize) -> Result<Option<crate::ingest::DocumentVersion>, QueryError>,
    candidates: &mut Vec<crate::ingest::SearchCandidate>,
    dims_touched: &mut u64,
    bytes_read: &mut u64,
    worker_thread_ids: &mut Vec<std::thread::ThreadId>,
    exact_score: bool,
) -> Result<(), QueryError> {
    *dims_touched = dims_touched
        .checked_add(outcome.stats.dims_touched)
        .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
    *bytes_read = bytes_read
        .checked_add(outcome.stats.bytes_read)
        .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
    for worker in outcome.stats.worker_thread_ids {
        if !worker_thread_ids.contains(&worker) {
            worker_thread_ids.push(worker);
        }
    }
    for candidate in outcome.candidates {
        let local_row = u32::try_from(candidate.row_id)
            .map_err(|_| QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
        candidates.push(crate::ingest::SearchCandidate::new(
            crate::ingest::GlobalRowId::new(source, local_row),
            document(candidate.row_id)?,
            candidate.score,
            exact_score,
        ));
    }
    Ok(())
}

struct AdmittedVectorSearch<'a> {
    pool: Option<Arc<pool::QueryPool>>,
    snapshot: Arc<PublishedSnapshot>,
    active_segment: Arc<crate::ingest::ActiveSegment>,
    generation: u64,
    _active_query: ActiveQuery<'a>,
}

struct ActiveQuery<'a> {
    count: &'a AtomicU64,
}

impl Drop for ActiveQuery<'_> {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        self.close_best_effort();
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use std::error::Error;

    use tempfile::tempdir;

    use crate::lifecycle::durability::DurabilityPolicy;
    use crate::manifest::io::commit_manifest;
    use crate::manifest::{Manifest, ManifestError};
    use crate::meta::Schema;
    use crate::vfs::StdVfs;

    use super::durability::{CommitTier, DurabilityMode, DurabilityPolicyError};
    use super::{OpenOptions, Store, StoreError, resolve_hybrid_leg_results};

    #[test]
    fn hybrid_dual_failure_precedence_is_independent_of_leg_completion_order() {
        use crate::fusion::{FusionError, FusionLeg, LegFailureKind};

        let vector_failure = FusionError::Leg {
            leg: FusionLeg::Vector,
            kind: LegFailureKind::Scan,
            detail: "vector failure".to_owned(),
        };
        let lexical_failure = FusionError::Leg {
            leg: FusionLeg::Lexical,
            kind: LegFailureKind::Lexical,
            detail: "lexical failure".to_owned(),
        };
        assert_eq!(
            resolve_hybrid_leg_results::<(), ()>(
                Err(vector_failure.clone()),
                Err(lexical_failure.clone()),
            ),
            Err(vector_failure.clone()),
        );
        assert_eq!(
            resolve_hybrid_leg_results::<(), ()>(
                Err(vector_failure.clone()),
                Err(FusionError::Cancelled { partial: false }),
            ),
            Err(FusionError::Cancelled { partial: false }),
        );
        assert_eq!(
            resolve_hybrid_leg_results::<(), ()>(
                Err(FusionError::Timeout { partial: false }),
                Err(FusionError::ReadCancelled { partial: false }),
            ),
            Err(FusionError::ReadCancelled { partial: false }),
        );
        assert_eq!(
            resolve_hybrid_leg_results::<(), ()>(
                Err(FusionError::Cancelled { partial: false }),
                Err(FusionError::Timeout { partial: false }),
            ),
            Err(FusionError::Timeout { partial: false }),
        );
        assert_eq!(
            resolve_hybrid_leg_results::<(), ()>(
                Err(FusionError::ReadCancelled { partial: false }),
                Err(lexical_failure),
            ),
            Err(FusionError::ReadCancelled { partial: false }),
        );
    }

    #[test]
    fn accounting_errors_report_the_typed_context() {
        let statistics = StoreError::Statistics {
            component: "mapped resident bytes",
            source: std::io::Error::other("mincore failed"),
        };
        assert_eq!(
            statistics.to_string(),
            "store statistics mapped resident bytes: mincore failed"
        );
        assert_eq!(
            statistics.source().map(ToString::to_string),
            Some("mincore failed".to_owned())
        );

        let budget = StoreError::BudgetExceeded {
            needed: 65,
            budget: 64,
            component: "wal",
        };
        assert_eq!(
            budget.to_string(),
            "store wal allocation needs 65 bytes, budget is 64 bytes"
        );
        assert!(budget.source().is_none());

        let allocation = StoreError::AllocationFailed {
            needed: 23,
            component: "snapshot",
        };
        assert_eq!(
            allocation.to_string(),
            "store snapshot allocator rejected 23 bytes"
        );
        assert!(allocation.source().is_none());
    }

    #[test]
    fn open_options_resolve_durability_before_filesystem_mutation() {
        let parent = tempdir().expect("parent directory");
        let store_path = parent.path().join("not-created");
        let options =
            OpenOptions::new().with_durability(DurabilityMode::Attached, CommitTier::Durable);

        let error = match Store::open(&store_path, options) {
            Ok(store) => {
                drop(store);
                panic!("attached durability unexpectedly opened")
            }
            Err(error) => error,
        };

        assert!(matches!(
            error,
            StoreError::Durability(DurabilityPolicyError::AttachedNotYetSupported)
        ));
        assert!(!store_path.exists(), "rejected open created store files");
    }

    #[test]
    fn open_refuses_a_manifest_ahead_of_the_checked_wal() {
        let directory = tempdir().expect("store directory");
        let policy = DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::Ordered)
            .expect("derived policy");
        commit_manifest(
            &StdVfs,
            directory.path(),
            &Manifest {
                generation: 1,
                log_seq: 1,
                segments: Vec::new(),
                epochs: Vec::new(),
                epoch_alias: None,
                schema: Schema::new(Vec::new()).expect("schema"),
            },
            policy,
        )
        .expect("manifest");

        let error = match Store::open(directory.path(), OpenOptions::default()) {
            Ok(store) => {
                drop(store);
                panic!("manifest ahead of missing WAL unexpectedly opened")
            }
            Err(error) => error,
        };

        assert!(matches!(
            error,
            StoreError::Manifest(ManifestError::AheadOfLog {
                snapshot: 1,
                durable: 0
            })
        ));
    }
}
