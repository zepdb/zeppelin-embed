//! Store lifecycle and memory accounting.

mod budget;
mod cancel;
mod close;
pub mod durability;
pub mod lock;
mod pool;
mod snapshot;
pub(crate) mod stats;

pub use cancel::{
    CancelToken, Deadline, DeadlineError, QueryCancellation, QueryControl, QueryError,
};
pub use snapshot::{
    InMemorySegment, InMemorySegmentFactors, PreparedSegment, PublishedSnapshot, SnapshotLease,
};
pub use stats::Stats;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Duration;

use durability::{CommitTier, DurabilityMode, DurabilityPolicy, DurabilityPolicyError};
use lock::{StoreLock, StoreLockError};

use self::close::BackgroundThread;

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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenOptions {
    access_mode: AccessMode,
    durability_mode: DurabilityMode,
    commit_tier: CommitTier,
    reader_drain_timeout: Duration,
    max_resident_bytes: u64,
    max_temp_bytes: u64,
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

    /// Sets the exact ceiling for live temporary anonymous bytes.
    #[must_use]
    pub const fn with_max_temp_bytes(mut self, bytes: u64) -> Self {
        self.max_temp_bytes = bytes;
        self
    }
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self::new()
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
    /// A referenced immutable segment could not be mapped or validated.
    Segment(crate::segment::SegmentError),
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
    /// A write-only lifecycle operation was requested from a read-only handle.
    ReadOnly,
    /// A prepared segment was accounted to a different store handle.
    ForeignPreparedSegment,
    /// The current snapshot generation cannot be incremented.
    GenerationOverflow,
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
            Self::Segment(error) => error.fmt(formatter),
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
            Self::ReadOnly => formatter.write_str("store handle is read-only"),
            Self::ForeignPreparedSegment => {
                formatter.write_str("prepared segment belongs to another store")
            }
            Self::GenerationOverflow => formatter.write_str("store snapshot generation overflow"),
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

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Lock(error) => Some(error),
            Self::Durability(error) => Some(error),
            Self::Manifest(error) => Some(error),
            Self::Segment(error) => Some(error),
            Self::Wal(error) => Some(error),
            Self::WalRecovery(error) => Some(error),
            Self::WalRecord { source, .. } => Some(source),
            Self::WalMutation { source, .. } => Some(source),
            Self::WalVector { source, .. } => Some(source),
            Self::WalWrite(error) => Some(error),
            Self::Statistics { source, .. } => Some(source),
            Self::BackgroundStart { source } => Some(source),
            Self::QueryPoolStart { source } => Some(source),
            Self::NotDirectory { .. }
            | Self::StoreBusy { .. }
            | Self::WalRevisionOrder { .. }
            | Self::UnsupportedWalMutation { .. }
            | Self::BudgetExceeded { .. }
            | Self::AllocationFailed { .. }
            | Self::DimensionMismatch { .. }
            | Self::ActiveRowOverflow
            | Self::ReadOnly
            | Self::ForeignPreparedSegment
            | Self::GenerationOverflow
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
    pub(crate) durability_policy: DurabilityPolicy,
    pub(crate) reader_drain_timeout: Duration,
    pub(crate) accounting: Arc<stats::Accounting>,
    pub(crate) active_queries: AtomicU64,
    #[cfg(test)]
    pub(crate) teardown_probe: Arc<close::TeardownProbe>,
}

impl Store {
    /// Opens a store directory with the requested access and durability policy.
    pub fn open(path: impl AsRef<Path>, options: OpenOptions) -> Result<Self, StoreError> {
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
        let snapshot = PublishedSnapshot::load(path, &accounting)?;
        let wal_path = path.join("wal.ze");
        let (active, recovered_wal) =
            crate::ingest::ActiveState::recover(&wal_path, snapshot.generation(), &accounting)?;
        let wal_writer = match options.access_mode {
            AccessMode::ReadWrite => Some(match recovered_wal {
                Some(recovered) => crate::ingest::StoreWal::resume(
                    &wal_path,
                    recovered,
                    durability_policy,
                    &accounting,
                )?,
                None => crate::ingest::StoreWal::create(&wal_path, durability_policy, &accounting)?,
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
            state: Mutex::new(StoreState::Open),
            state_changed: Condvar::new(),
            background: Mutex::new(background),
            query_pool: Mutex::new(None),
            snapshot: RwLock::new(None),
            active: Mutex::new(Some(active)),
            wal_writer: Mutex::new(wal_writer),
            writer_lock: Mutex::new(writer_lock),
            durability_policy,
            reader_drain_timeout: options.reader_drain_timeout,
            accounting,
            active_queries: AtomicU64::new(0),
            #[cfg(test)]
            teardown_probe,
        };
        store.publish_snapshot(snapshot)?;
        Ok(store)
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
    /// Every per-segment scan runs on the same persistent query pool and the
    /// same cancellation state as [`Self::top_k_with_options`]. Row addresses
    /// are `(RowSource, local_row)`, so two immutable segments' row zero can
    /// never collide and manifest reordering cannot rename a sealed row.
    pub fn search(
        &self,
        request: crate::ingest::SearchRequest<'_>,
        k: usize,
        options: crate::scan::ScanOptions,
        control: QueryControl,
    ) -> Result<crate::ingest::SearchOutcome, QueryError> {
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
        let active_query = ActiveQuery {
            count: &self.active_queries,
        };
        drop(pool_slot);
        drop(active_guard);
        drop(state);

        let result = search_pinned(
            &pool,
            &snapshot,
            &active_segment,
            generation,
            request,
            k,
            options,
            control,
        );
        drop(active_query);
        result
    }
}

#[allow(clippy::too_many_arguments)]
fn search_pinned(
    pool: &pool::QueryPool,
    snapshot: &Arc<PublishedSnapshot>,
    active: &crate::ingest::ActiveSegment,
    generation: u64,
    request: crate::ingest::SearchRequest<'_>,
    k: usize,
    options: crate::scan::ScanOptions,
    control: QueryControl,
) -> Result<crate::ingest::SearchOutcome, QueryError> {
    use crate::ingest::{RowSource, SearchOutcome};
    use crate::quant::{prepare_bit4_query, prepare_int8_query};
    use crate::scan::{Int8Factors, ScanQuery, ScanRequest, ScanRows, ScanStats};

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

    if !active.is_empty() {
        let alive = active.alive().map_err(QueryError::Store)?;
        let outcome = pool.execute(
            ScanRequest {
                query: ScanQuery::Bit4(&bit4_query),
                rows: ScanRows::Bit4RowMajor {
                    codes: active.codes(),
                    factors: active.factors(),
                },
                row_mask: Some(alive.scan_mask()),
            },
            k,
            options,
            control.clone(),
            SnapshotLease::new_at(Arc::clone(snapshot), generation),
        )?;
        merge_store_outcome(
            outcome,
            RowSource::Active,
            |row| active.document(row),
            &mut candidates,
            &mut dims_touched,
            &mut bytes_read,
            &mut worker_thread_ids,
        )?;
    }

    for segment in snapshot.segments() {
        let alive = segment
            .alive()
            .map_err(StoreError::Segment)
            .map_err(QueryError::Store)?;
        let source = RowSource::Sealed(segment.meta().id);
        let outcome = match segment.meta().scheme {
            4 => pool.execute(
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
                options,
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
                pool.execute(
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
                    options,
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
        };
        merge_store_outcome(
            outcome,
            source,
            |_| None,
            &mut candidates,
            &mut dims_touched,
            &mut bytes_read,
            &mut worker_thread_ids,
        )?;
    }

    candidates.sort_unstable_by(|left, right| {
        right
            .score()
            .total_cmp(&left.score())
            .then_with(|| left.row_id().cmp(&right.row_id()))
    });
    candidates.truncate(k);
    Ok(SearchOutcome {
        candidates,
        stats: ScanStats {
            dims_touched,
            bytes_read,
            threads_used: worker_thread_ids.len(),
            worker_thread_ids,
        },
        generation,
    })
}

#[allow(clippy::too_many_arguments)]
fn merge_store_outcome(
    outcome: crate::scan::ScanOutcome,
    source: crate::ingest::RowSource,
    document: impl Fn(usize) -> Option<crate::ingest::DocumentVersion>,
    candidates: &mut Vec<crate::ingest::SearchCandidate>,
    dims_touched: &mut u64,
    bytes_read: &mut u64,
    worker_thread_ids: &mut Vec<std::thread::ThreadId>,
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
            document(candidate.row_id),
            candidate.score,
        ));
    }
    Ok(())
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
    use super::{OpenOptions, Store, StoreError};

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
