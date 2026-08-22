//! Store lifecycle and memory accounting.

mod close;
pub mod durability;
pub mod lock;
mod snapshot;

pub use snapshot::{PublishedSnapshot, SnapshotLease};

use std::path::{Path, PathBuf};
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
            Self::BackgroundStart { source } => Some(source),
            Self::NotDirectory { .. }
            | Self::StoreBusy { .. }
            | Self::Closing
            | Self::Closed
            | Self::ReadCancelled
            | Self::BackgroundHandshake
            | Self::BackgroundThreadPanicked
            | Self::Synchronization { .. } => None,
        }
    }
}

/// One explicitly closeable embedded-store handle.
pub struct Store {
    pub(crate) state: Mutex<StoreState>,
    pub(crate) state_changed: Condvar,
    pub(crate) snapshot: RwLock<Option<Arc<PublishedSnapshot>>>,
    pub(crate) background: Mutex<Option<BackgroundThread>>,
    pub(crate) writer_lock: Mutex<Option<StoreLock>>,
    pub(crate) _durability_policy: DurabilityPolicy,
    pub(crate) reader_drain_timeout: Duration,
}

impl Store {
    /// Opens a store directory with the requested access and durability policy.
    pub fn open(path: impl AsRef<Path>, options: OpenOptions) -> Result<Self, StoreError> {
        let path = path.as_ref();
        let durability_policy = DurabilityPolicy::new(options.durability_mode, options.commit_tier)
            .map_err(StoreError::Durability)?;
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
        let snapshot = PublishedSnapshot::load(path)?;
        let background = match options.access_mode {
            AccessMode::ReadWrite => Some(BackgroundThread::start()?),
            AccessMode::ReadOnly => None,
        };
        let store = Self {
            state: Mutex::new(StoreState::Open),
            state_changed: Condvar::new(),
            snapshot: RwLock::new(None),
            background: Mutex::new(background),
            writer_lock: Mutex::new(writer_lock),
            _durability_policy: durability_policy,
            reader_drain_timeout: options.reader_drain_timeout,
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
        let snapshot = self
            .snapshot
            .read()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })?
            .as_ref()
            .cloned()
            .ok_or(StoreError::Closed)?;
        drop(state);
        Ok(SnapshotLease::new(snapshot))
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
        let mut published = self
            .snapshot
            .write()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })?;
        *published = Some(Arc::new(snapshot));
        drop(state);
        Ok(())
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
    use tempfile::tempdir;

    use crate::lifecycle::durability::DurabilityPolicy;
    use crate::manifest::io::commit_manifest;
    use crate::manifest::{Manifest, ManifestError};
    use crate::meta::Schema;
    use crate::vfs::StdVfs;

    use super::durability::{CommitTier, DurabilityMode, DurabilityPolicyError};
    use super::{OpenOptions, Store, StoreError};

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
