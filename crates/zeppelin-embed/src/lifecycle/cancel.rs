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
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

/// A monotonic absolute query deadline.
#[derive(Clone, Debug)]
pub struct Deadline {
    at: Instant,
    state: Arc<AtomicU8>,
}

impl Deadline {
    /// Creates a deadline relative to the current monotonic clock.
    ///
    /// # Errors
    ///
    /// Returns [`DeadlineError`] when the requested duration cannot be
    /// represented by [`Instant`].
    pub fn after(duration: Duration) -> Result<Self, DeadlineError> {
        let at = Instant::now()
            .checked_add(duration)
            .ok_or(DeadlineError::OutOfRange)?;
        Ok(Self {
            at,
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
        }
    }
}

impl std::error::Error for QueryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::Scan(error) => Some(error),
            Self::Timeout { .. } | Self::Cancelled { .. } | Self::ReadCancelled { .. } => None,
        }
    }
}

impl QueryControl {
    pub(crate) fn deadline(&self) -> Option<Instant> {
        match self {
            Self::Deadline(deadline) => Some(deadline.at),
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

pub(crate) struct QueryCancellation<'a> {
    control: &'a QueryControl,
    lease: &'a SnapshotLease,
}

impl<'a> QueryCancellation<'a> {
    pub(crate) const fn new(control: &'a QueryControl, lease: &'a SnapshotLease) -> Self {
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
}
