//! Query cancellation and deadline types.
//!
//! Store queries check cancellation at the start of each partition and every
//! 64 rows thereafter (every 16 four-row Bit4 batches). This granularity keeps
//! the hot-loop operation to relaxed atomic loads while bounding wasted work
//! inside one large partition. The waiting query caller owns deadline timing:
//! it parks on the completion condition variable until the monotonic deadline,
//! then flips the same atomic observed by workers. Scan loops therefore make
//! no clock syscall and take no cancellation mutex.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use crate::scan::ScanError;

use super::{SnapshotLease, StoreError};

const ACTIVE: u8 = 0;
const CANCELLED: u8 = 1;
const TIMED_OUT: u8 = 2;

/// A clonable, thread-safe request to cancel one query without partial results.
#[derive(Clone, Debug)]
pub struct CancelToken {
    state: Arc<AtomicU8>,
}

impl CancelToken {
    /// Creates an active token.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Arc::new(AtomicU8::new(ACTIVE)),
        }
    }

    /// Requests cancellation. Repeated calls have no additional effect.
    pub fn cancel(&self) {
        let _ =
            self.state
                .compare_exchange(ACTIVE, CANCELLED, Ordering::Relaxed, Ordering::Relaxed);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.state.load(Ordering::Relaxed) == CANCELLED
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

/// A monotonic absolute query deadline.
#[derive(Clone)]
pub struct Deadline {
    at: Instant,
    clock: Arc<dyn super::MonotonicClock>,
    state: Arc<AtomicU8>,
}

impl std::fmt::Debug for Deadline {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Deadline")
            .field("at", &self.at)
            .field("state", &self.state.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Deadline {
    /// Creates a deadline relative to the current monotonic clock.
    ///
    /// # Errors
    ///
    /// Returns [`DeadlineError`] when the requested duration cannot be
    /// represented by [`Instant`].
    pub fn after(duration: Duration) -> Result<Self, DeadlineError> {
        Self::after_with_clock(duration, Arc::new(super::SystemMonotonicClock))
    }

    /// Creates a deadline relative to an injected monotonic clock.
    ///
    /// This is available only to deterministic test-support builds so a test
    /// can construct and evaluate the deadline in one clock domain.
    #[cfg(any(test, feature = "test-support"))]
    pub fn after_with_test_clock(
        duration: Duration,
        clock: Arc<dyn super::MonotonicClock>,
    ) -> Result<Self, DeadlineError> {
        Self::after_with_clock(duration, clock)
    }

    pub(crate) fn after_with_clock(
        duration: Duration,
        clock: Arc<dyn super::MonotonicClock>,
    ) -> Result<Self, DeadlineError> {
        let at = clock
            .now()
            .checked_add(duration)
            .ok_or(DeadlineError::OutOfRange)?;
        Ok(Self {
            at,
            clock,
            state: Arc::new(AtomicU8::new(ACTIVE)),
        })
    }
}

/// Failure while constructing a monotonic deadline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeadlineError {
    /// The relative duration exceeded the monotonic clock's range.
    OutOfRange,
}

impl std::fmt::Display for DeadlineError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutOfRange => formatter.write_str("query deadline is outside Instant's range"),
        }
    }
}

impl std::error::Error for DeadlineError {}

/// The mandatory cooperative stop mechanism carried by one store query.
#[derive(Clone, Debug)]
pub enum QueryControl {
    /// Stop when the monotonic deadline expires.
    Deadline(Deadline),
    /// Stop when any clone of the token is cancelled.
    Cancel(CancelToken),
}

/// Typed failure from a store-admitted query.
#[derive(Debug)]
pub enum QueryError {
    /// The deadline expired. Partial results are never returned.
    Timeout {
        /// Permanently false; included so callers can reject future partial paths.
        partial: bool,
    },
    /// The caller cancelled the token. Partial results are never returned.
    Cancelled {
        /// Permanently false; included so callers can reject future partial paths.
        partial: bool,
    },
    /// Store close cancelled an already admitted query.
    ReadCancelled {
        /// Permanently false; included so callers can reject future partial paths.
        partial: bool,
    },
    /// Store admission or lifecycle failed.
    Store(StoreError),
    /// Scan validation or scoring failed.
    Scan(ScanError),
    /// Graph validation or traversal failed.
    Graph(crate::graph::search::GraphSearchError),
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout { partial } => {
                write!(formatter, "query deadline expired (partial={partial})")
            }
            Self::Cancelled { partial } => {
                write!(formatter, "query was cancelled (partial={partial})")
            }
            Self::ReadCancelled { partial } => {
                write!(formatter, "store close cancelled query (partial={partial})")
            }
            Self::Store(error) => error.fmt(formatter),
            Self::Scan(error) => error.fmt(formatter),
            Self::Graph(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for QueryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::Scan(error) => Some(error),
            Self::Graph(error) => Some(error),
            Self::Timeout { .. } | Self::Cancelled { .. } | Self::ReadCancelled { .. } => None,
        }
    }
}

impl QueryControl {
    /// Checks the same absolute deadline or cancellation token before admission
    /// and between caller-owned stages such as tokenization and embedding.
    ///
    /// This never extends a deadline. An admitted store query also checks its
    /// snapshot lease so store-close cancellation retains precedence.
    ///
    /// # Errors
    ///
    /// Returns [`QueryError::Timeout`] or [`QueryError::Cancelled`], always
    /// with `partial: false`, when the caller's control has stopped the query.
    pub fn checkpoint(&self) -> Result<(), QueryError> {
        if self
            .deadline()
            .zip(self.now())
            .is_some_and(|(deadline, now)| now >= deadline)
        {
            self.mark_timed_out();
        }
        self.error().map_or(Ok(()), Err)
    }

    pub(crate) fn with_clock(mut self, clock: Arc<dyn super::MonotonicClock>) -> Self {
        if let Self::Deadline(deadline) = &mut self {
            deadline.clock = clock;
        }
        self
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        match self {
            Self::Deadline(deadline) => Some(deadline.at),
            Self::Cancel(_) => None,
        }
    }

    pub(crate) fn now(&self) -> Option<Instant> {
        match self {
            Self::Deadline(deadline) => Some(deadline.clock.now()),
            Self::Cancel(_) => None,
        }
    }

    pub(crate) fn state(&self) -> &Arc<AtomicU8> {
        match self {
            Self::Deadline(deadline) => &deadline.state,
            Self::Cancel(token) => &token.state,
        }
    }

    pub(crate) fn mark_timed_out(&self) {
        let _ =
            self.state()
                .compare_exchange(ACTIVE, TIMED_OUT, Ordering::Relaxed, Ordering::Relaxed);
    }

    pub(crate) fn error(&self) -> Option<QueryError> {
        match self.state().load(Ordering::Relaxed) {
            CANCELLED => Some(QueryError::Cancelled { partial: false }),
            TIMED_OUT => Some(QueryError::Timeout { partial: false }),
            _ => None,
        }
    }
}

/// Cheap cooperative stop check shared by one query and its snapshot lease.
pub struct QueryCancellation<'a> {
    control: &'a QueryControl,
    lease: &'a SnapshotLease,
}

impl<'a> QueryCancellation<'a> {
    pub(crate) fn cache_lock<'lock, T>(
        &self,
        mutex: &'lock std::sync::Mutex<T>,
        component: &'static str,
    ) -> Result<std::sync::MutexGuard<'lock, T>, QueryError> {
        self.check_graph().map_err(QueryError::Scan)?;
        let guard = loop {
            match mutex.try_lock() {
                Ok(guard) => break guard,
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err(QueryError::Store(StoreError::Synchronization { component }));
                }
                Err(std::sync::TryLockError::WouldBlock) => {
                    self.check_graph().map_err(QueryError::Scan)?;
                    std::thread::park_timeout(Duration::from_millis(1));
                }
            }
        };
        self.check_graph().map_err(QueryError::Scan)?;
        Ok(guard)
    }

    /// Binds caller cancellation/deadline state to the admitted snapshot read.
    #[must_use]
    pub const fn new(control: &'a QueryControl, lease: &'a SnapshotLease) -> Self {
        Self { control, lease }
    }

    pub(crate) fn check(&self) -> Result<(), ScanError> {
        if self.lease.is_cancelled() {
            return Err(ScanError::ReadCancelled { partial: false });
        }
        match self.control.state().load(Ordering::Relaxed) {
            CANCELLED => Err(ScanError::Cancelled { partial: false }),
            TIMED_OUT => Err(ScanError::Timeout { partial: false }),
            _ => Ok(()),
        }
    }

    pub(crate) fn check_graph(&self) -> Result<(), ScanError> {
        if self
            .control
            .deadline()
            .zip(self.control.now())
            .is_some_and(|(deadline, now)| now >= deadline)
        {
            self.control.mark_timed_out();
        }
        self.check()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn cancel_token_for_test(&self) -> Option<CancelToken> {
        match self.control {
            QueryControl::Cancel(token) => Some(token.clone()),
            QueryControl::Deadline(_) => None,
        }
    }
}
