//! Write-ahead logging, checked recovery, and self-tuning group commit.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};

use crate::format::RegistryError;
use crate::lifecycle::durability::{DurabilityPolicy, SyncRequirement};
use crate::manifest::io::DurableLog;
use crate::vfs::{Vfs, VfsFile};

pub mod header;
pub mod record;
pub mod replay;

use header::encode_header;
use record::{RecordEncodeError, WalRecord, encode_record};
use replay::{ReplayTerminator, replay};

/// Default upper bound for one group append, in encoded bytes.
pub const DEFAULT_MAX_GROUP_BYTES: usize = 1_048_576;

/// Monotonic write-ahead-log sequence number.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct LogSeq(u64);

impl LogSeq {
    /// Creates a sequence number from its persisted integer.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the persisted integer.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// One record visible from in-memory state before its group reaches storage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VisibleRecord {
    /// Assigned monotonic sequence.
    pub seq: LogSeq,
    /// Caller-defined operation identifier.
    pub op: u16,
    /// Owned operation payload.
    pub payload: Vec<u8>,
}

/// Deterministic group-composition and progress snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalWriterStats {
    /// Record count in each append group whose flush returned.
    pub group_sizes: Vec<usize>,
    /// Encoded byte count in each append group whose flush returned.
    pub group_bytes: Vec<usize>,
    /// Records waiting behind the currently in-flight group.
    pub pending_records: usize,
    /// Largest sequence visible in memory.
    pub visible_end: Option<LogSeq>,
    /// Largest sequence whose tier-specific flush returned.
    pub durable_end: u64,
}

#[derive(Clone, Debug)]
struct Failure {
    kind: std::io::ErrorKind,
    detail: String,
}

#[derive(Debug)]
struct WriterState {
    next_seq: u64,
    visible: Vec<VisibleRecord>,
    durable_end: u64,
    pending_bytes: Vec<u8>,
    pending_records: usize,
    pending_last_seq: Option<LogSeq>,
    barrier_in_flight: bool,
    failure: Option<Failure>,
    group_sizes: Vec<usize>,
    group_bytes: Vec<usize>,
}

struct Group {
    bytes: Vec<u8>,
    records: usize,
    last_seq: LogSeq,
}

/// Single-store WAL writer using caller threads as leader and followers.
///
/// There is no timer and no background flusher. An idle arrival immediately
/// becomes leader. Arrivals observed while its barrier is in flight form the
/// next byte-bounded group, which that leader issues immediately afterward.
pub struct WalWriter {
    file: Mutex<Box<dyn VfsFile>>,
    state: Mutex<WriterState>,
    changed: Condvar,
    sync: SyncRequirement,
    max_group_bytes: usize,
    durable_progress: AtomicU64,
}

impl WalWriter {
    /// Creates a writer over a new or empty WAL path.
    pub fn create(
        vfs: &dyn Vfs,
        path: &Path,
        first_seq: LogSeq,
        policy: DurabilityPolicy,
    ) -> Result<Self, WalWriteError> {
        Self::create_with_max_group_bytes(vfs, path, first_seq, policy, DEFAULT_MAX_GROUP_BYTES)
    }

    /// Creates a writer with an explicit encoded-byte group bound.
    pub fn create_with_max_group_bytes(
        vfs: &dyn Vfs,
        path: &Path,
        first_seq: LogSeq,
        policy: DurabilityPolicy,
        max_group_bytes: usize,
    ) -> Result<Self, WalWriteError> {
        match vfs.open(path) {
            Ok(0) => {}
            Ok(length) => return Err(WalWriteError::NonEmptyWal { length }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(WalWriteError::from_io(error)),
        }
        let header = encode_header(first_seq).map_err(WalWriteError::Header)?;
        if max_group_bytes < header.len() {
            return Err(WalWriteError::GroupTooLarge {
                encoded_bytes: header.len(),
                max_group_bytes,
            });
        }
        let file = vfs.open_append(path).map_err(WalWriteError::from_io)?;
        Ok(Self {
            file: Mutex::new(file),
            state: Mutex::new(WriterState {
                next_seq: first_seq.get(),
                visible: Vec::new(),
                durable_end: first_seq.get().saturating_sub(1),
                pending_bytes: header,
                pending_records: 0,
                pending_last_seq: None,
                barrier_in_flight: false,
                failure: None,
                group_sizes: Vec::new(),
                group_bytes: Vec::new(),
            }),
            changed: Condvar::new(),
            sync: policy.data_file_sync(),
            max_group_bytes,
            durable_progress: AtomicU64::new(first_seq.get().saturating_sub(1)),
        })
    }

    /// Makes one record visible and returns without waiting when another
    /// caller already owns the in-flight barrier.
    pub fn commit(&self, op: u16, payload: &[u8]) -> Result<LogSeq, WalWriteError> {
        let (seq, leader) = self.stage(op, payload)?;
        if leader {
            self.flush_as_leader()?;
        }
        Ok(seq)
    }

    /// Makes one record visible, then waits until its tier-specific flush returns.
    pub fn commit_durable(&self, op: u16, payload: &[u8]) -> Result<LogSeq, WalWriteError> {
        let (seq, leader) = self.stage(op, payload)?;
        if leader {
            self.flush_as_leader()?;
        }
        self.wait_until_durable(seq)?;
        Ok(seq)
    }

    /// Returns a copy of every record visible to in-memory readers.
    pub fn visible_records(&self) -> Result<Vec<VisibleRecord>, WalWriteError> {
        Ok(self.lock_state()?.visible.clone())
    }

    /// Returns deterministic progress and completed-group statistics.
    pub fn stats(&self) -> Result<WalWriterStats, WalWriteError> {
        let state = self.lock_state()?;
        Ok(WalWriterStats {
            group_sizes: state.group_sizes.clone(),
            group_bytes: state.group_bytes.clone(),
            pending_records: state.pending_records,
            visible_end: state.visible.last().map(|record| record.seq),
            durable_end: state.durable_end,
        })
    }

    fn stage(&self, op: u16, payload: &[u8]) -> Result<(LogSeq, bool), WalWriteError> {
        let mut state = self.lock_state()?;
        loop {
            if let Some(failure) = &state.failure {
                return Err(WalWriteError::failed(failure.clone()));
            }
            let seq = LogSeq::new(state.next_seq);
            let encoded =
                encode_record(WalRecord { seq, op, payload }).map_err(WalWriteError::Record)?;
            let required = state.pending_bytes.len().saturating_add(encoded.len());
            if required <= self.max_group_bytes {
                state.next_seq = state
                    .next_seq
                    .checked_add(1)
                    .ok_or(WalWriteError::SequenceExhausted)?;
                state.pending_bytes.extend_from_slice(&encoded);
                state.pending_records = state.pending_records.saturating_add(1);
                state.pending_last_seq = Some(seq);
                state.visible.push(VisibleRecord {
                    seq,
                    op,
                    payload: payload.to_vec(),
                });
                let leader = !state.barrier_in_flight;
                if leader {
                    state.barrier_in_flight = true;
                }
                return Ok((seq, leader));
            }
            if state.pending_records == 0 {
                return Err(WalWriteError::GroupTooLarge {
                    encoded_bytes: required,
                    max_group_bytes: self.max_group_bytes,
                });
            }
            state = self
                .changed
                .wait(state)
                .map_err(|_| WalWriteError::Poisoned("WAL state mutex"))?;
        }
    }

    fn flush_as_leader(&self) -> Result<(), WalWriteError> {
        loop {
            let group = {
                let mut state = self.lock_state()?;
                let last_seq = state.pending_last_seq.ok_or(WalWriteError::Poisoned(
                    "WAL leader found no pending sequence",
                ))?;
                let bytes = std::mem::take(&mut state.pending_bytes);
                let records = std::mem::take(&mut state.pending_records);
                state.pending_last_seq = None;
                self.changed.notify_all();
                Group {
                    bytes,
                    records,
                    last_seq,
                }
            };
            let result = self.write_group(&group);
            let mut state = self.lock_state()?;
            match result {
                Ok(()) => {
                    state.durable_end = group.last_seq.get();
                    self.durable_progress
                        .store(group.last_seq.get(), Ordering::Release);
                    state.group_sizes.push(group.records);
                    state.group_bytes.push(group.bytes.len());
                    if state.pending_records == 0 {
                        state.barrier_in_flight = false;
                        self.changed.notify_all();
                        return Ok(());
                    }
                }
                Err(error) => {
                    let failure = Failure {
                        kind: error.kind(),
                        detail: error.to_string(),
                    };
                    state.failure = Some(failure.clone());
                    state.barrier_in_flight = false;
                    self.changed.notify_all();
                    return Err(WalWriteError::failed(failure));
                }
            }
        }
    }

    fn write_group(&self, group: &Group) -> std::io::Result<()> {
        let mut file = self
            .file
            .lock()
            .map_err(|_| std::io::Error::other("WAL file mutex poisoned"))?;
        file.append(&group.bytes)?;
        match self.sync {
            SyncRequirement::Skip => Ok(()),
            SyncRequirement::Sync(kind) => file.sync(kind),
        }
    }

    fn wait_until_durable(&self, seq: LogSeq) -> Result<(), WalWriteError> {
        let mut state = self.lock_state()?;
        while state.durable_end < seq.get() {
            if let Some(failure) = &state.failure {
                return Err(WalWriteError::failed(failure.clone()));
            }
            state = self
                .changed
                .wait(state)
                .map_err(|_| WalWriteError::Poisoned("WAL state mutex"))?;
        }
        Ok(())
    }

    fn lock_state(&self) -> Result<MutexGuard<'_, WriterState>, WalWriteError> {
        self.state
            .lock()
            .map_err(|_| WalWriteError::Poisoned("WAL state mutex"))
    }
}

impl DurableLog for WalWriter {
    fn durable_end(&self) -> u64 {
        self.durable_progress.load(Ordering::Acquire)
    }
}

/// Owned checked WAL prefix recovered from a filesystem image.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalReader {
    records: Vec<VisibleRecord>,
    durable_end: u64,
    terminator: Option<ReplayTerminator>,
}

impl WalReader {
    /// Opens the largest checksum- and sequence-valid WAL prefix.
    pub fn open(vfs: &dyn Vfs, path: &Path) -> Result<Self, WalReadError> {
        let bytes = match vfs.read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    records: Vec::new(),
                    durable_end: 0,
                    terminator: None,
                });
            }
            Err(error) => return Err(WalReadError::Io(error)),
        };
        let replayed = replay(&bytes);
        let terminator = replayed.terminator;
        if matches!(terminator, ReplayTerminator::InvalidHeader(_)) {
            return Ok(Self {
                records: Vec::new(),
                durable_end: 0,
                terminator: Some(terminator),
            });
        }
        let records = replayed
            .records
            .into_iter()
            .map(|record| VisibleRecord {
                seq: record.seq,
                op: record.op,
                payload: record.payload.to_vec(),
            })
            .collect::<Vec<_>>();
        let durable_end = records.last().map_or(0, |record| record.seq.get());
        Ok(Self {
            records,
            durable_end,
            terminator: Some(terminator),
        })
    }

    /// Returns the recovered ordered prefix.
    #[must_use]
    pub fn records(&self) -> &[VisibleRecord] {
        &self.records
    }

    /// Returns the checked replay boundary, or `None` when no WAL path existed.
    #[must_use]
    pub const fn terminator(&self) -> Option<ReplayTerminator> {
        self.terminator
    }
}

impl DurableLog for WalReader {
    fn durable_end(&self) -> u64 {
        self.durable_end
    }
}

/// WAL creation, staging, or group-flush failure.
#[derive(Debug)]
pub enum WalWriteError {
    /// The requested path already contains WAL bytes.
    NonEmptyWal {
        /// Existing byte length.
        length: u64,
    },
    /// One encoded record cannot fit the configured byte bound.
    GroupTooLarge {
        /// Bytes required by the pending group and record.
        encoded_bytes: usize,
        /// Configured maximum.
        max_group_bytes: usize,
    },
    /// The u64 sequence space has no successor.
    SequenceExhausted,
    /// WAL header registry lookup failed.
    Header(RegistryError),
    /// Record framing failed.
    Record(RecordEncodeError),
    /// A shared writer mutex was poisoned.
    Poisoned(&'static str),
    /// The writer failed permanently after visibility was published.
    Failed {
        /// Underlying I/O error class.
        kind: std::io::ErrorKind,
        /// Stable error detail retained for followers.
        detail: String,
    },
}

impl WalWriteError {
    fn from_io(error: std::io::Error) -> Self {
        Self::Failed {
            kind: error.kind(),
            detail: error.to_string(),
        }
    }

    fn failed(failure: Failure) -> Self {
        Self::Failed {
            kind: failure.kind,
            detail: failure.detail,
        }
    }
}

impl std::fmt::Display for WalWriteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonEmptyWal { length } => {
                write!(formatter, "WAL path is not empty: {length} bytes")
            }
            Self::GroupTooLarge {
                encoded_bytes,
                max_group_bytes,
            } => write!(
                formatter,
                "WAL group needs {encoded_bytes} bytes, exceeding {max_group_bytes}"
            ),
            Self::SequenceExhausted => formatter.write_str("WAL sequence space exhausted"),
            Self::Header(error) => write!(formatter, "WAL header: {error}"),
            Self::Record(error) => write!(formatter, "WAL record: {error}"),
            Self::Poisoned(detail) => write!(formatter, "{detail} poisoned"),
            Self::Failed { kind, detail } => {
                write!(formatter, "WAL writer failed with {:?}: {}", kind, detail)
            }
        }
    }
}

impl std::error::Error for WalWriteError {}

/// WAL recovery failure requiring the caller to stop rather than hide corruption.
#[derive(Debug)]
pub enum WalReadError {
    /// Filesystem read failed.
    Io(std::io::Error),
}

impl std::fmt::Display for WalReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "WAL read: {error}"),
        }
    }
}

impl std::error::Error for WalReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
        }
    }
}
