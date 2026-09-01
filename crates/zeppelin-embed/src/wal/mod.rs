//! Write-ahead logging, checked recovery, and self-tuning group commit.

use std::collections::VecDeque;
use std::io::IoSlice;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};

use crate::format::RegistryError;
use crate::lifecycle::durability::{DurabilityPolicy, SyncRequirement};
use crate::manifest::io::DurableLog;
use crate::vfs::{SyncKind, Vfs, VfsFile};

pub mod header;
pub mod record;
pub mod replay;

use header::encode_header;
use record::{
    RecordEncodeError, RecordError, WalRecord, append_record_into, encode_record_into,
    encoded_record_len,
};
use replay::{ReplayTerminator, replay};

/// Default upper bound for one group append, in encoded bytes.
pub const DEFAULT_MAX_GROUP_BYTES: usize = 1_048_576;
/// Default upper bound for a full-sync group append, in encoded bytes.
///
/// Fixed sync cost dominates, while the memory cost is one bounded buffer per
/// writer. Sixteen MiB follows the InnoDB, PostgreSQL, and Lucene convention.
/// Sync cost above one MiB is modeled, not yet measured; the plan 02 sweep is
/// pending.
pub const DEFAULT_MAX_GROUP_BYTES_DURABLE: usize = 16 * 1_048_576;

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

/// Number of fixed group-size histogram buckets.
pub const GROUP_SIZE_HISTOGRAM_BUCKETS: usize = 18;
/// Maximum number of completed groups retained exactly for diagnostics/tests.
pub const RECENT_GROUP_LIMIT: usize = 64;

/// One record visible from in-memory state before its group reaches storage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VisibleRecord {
    /// Assigned monotonic sequence.
    pub seq: LogSeq,
    /// Caller-defined operation identifier.
    pub op: u16,
    encoded: Arc<Vec<u8>>,
    encoded_range: Range<usize>,
}

impl VisibleRecord {
    fn writer_owned(
        seq: LogSeq,
        op: u16,
        encoded: Arc<Vec<u8>>,
        encoded_range: Range<usize>,
    ) -> Self {
        Self {
            seq,
            op,
            encoded,
            encoded_range,
        }
    }

    fn recovered(seq: LogSeq, op: u16, encoded: Arc<Vec<u8>>, encoded_range: Range<usize>) -> Self {
        Self {
            seq,
            op,
            encoded,
            encoded_range,
        }
    }

    /// Returns the checked payload borrowed from the encoded WAL allocation.
    pub fn payload(&self) -> Result<&[u8], VisibleRecordError> {
        let encoded = self.encoded.get(self.encoded_range.clone()).ok_or(
            VisibleRecordError::InvalidEncodedRange {
                start: self.encoded_range.start,
                end: self.encoded_range.end,
                available: self.encoded.len(),
            },
        )?;
        decode_visible(encoded).map_err(VisibleRecordError::Record)
    }

    /// Returns the complete encoded width retained for this record.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        self.encoded_range
            .end
            .saturating_sub(self.encoded_range.start)
    }
}

fn decode_visible(encoded: &[u8]) -> Result<&[u8], RecordError> {
    record::decode_record(encoded).map(|decoded| decoded.record.payload)
}

/// One exact completed group retained in the bounded recent-group ring.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompletedGroup {
    /// Number of records in the group.
    pub records: usize,
    /// Complete encoded bytes appended for the group.
    pub encoded_bytes: usize,
}

/// Fixed-size logarithmic distribution of completed group record counts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GroupSizeHistogram {
    /// Counts for `1`, `2..=3`, `4..=7`, through a final overflow bucket.
    pub buckets: [u64; GROUP_SIZE_HISTOGRAM_BUCKETS],
}

impl GroupSizeHistogram {
    const fn new() -> Self {
        Self {
            buckets: [0; GROUP_SIZE_HISTOGRAM_BUCKETS],
        }
    }

    fn record(&mut self, records: usize) {
        let bucket = if records == 0 {
            0
        } else {
            (usize::BITS
                .saturating_sub(records.leading_zeros())
                .saturating_sub(1) as usize)
                .min(GROUP_SIZE_HISTOGRAM_BUCKETS.saturating_sub(1))
        };
        if let Some(count) = self.buckets.get_mut(bucket) {
            *count = count.saturating_add(1);
        }
    }
}

/// Deterministic group-composition and progress snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalWriterStats {
    /// Total append groups whose flush returned.
    pub completed_groups: u64,
    /// Total records across completed append groups.
    pub completed_records: u64,
    /// Total encoded bytes across completed append groups.
    pub completed_bytes: u64,
    /// Fixed-size logarithmic group-size distribution.
    pub group_size_histogram: GroupSizeHistogram,
    /// Exact most-recent groups, oldest to newest, capped by [`RECENT_GROUP_LIMIT`].
    pub recent_groups: Vec<CompletedGroup>,
    /// Records waiting behind the currently in-flight group.
    pub pending_records: usize,
    /// Largest sequence visible in memory.
    pub visible_end: Option<LogSeq>,
    /// Largest sequence whose tier-specific flush returned.
    pub durable_end: Option<LogSeq>,
    /// Records still retained for in-memory visibility.
    pub retained_records: usize,
    /// Encoded bytes still retained for in-memory visibility.
    pub retained_bytes: usize,
    /// Shared encoded record buffers created by successful staging calls.
    pub encoded_buffer_allocations: u64,
}

#[derive(Clone, Debug)]
struct Failure {
    kind: std::io::ErrorKind,
    detail: Arc<str>,
}

#[derive(Debug)]
struct WriterState {
    next_seq: u64,
    visible: VecDeque<VisibleRecord>,
    retained_bytes: usize,
    durable_end: Option<LogSeq>,
    pending_groups: VecDeque<Group>,
    spare_chunks: Vec<EncodedChunk>,
    barrier_in_flight: bool,
    failure: Option<Arc<Failure>>,
    completed_groups: u64,
    completed_records: u64,
    completed_bytes: u64,
    encoded_buffer_allocations: u64,
    group_size_histogram: GroupSizeHistogram,
    recent_groups: VecDeque<CompletedGroup>,
}

#[derive(Debug)]
struct EncodedChunk {
    encoded: Arc<Vec<u8>>,
    range: Range<usize>,
}

impl EncodedChunk {
    fn bytes(&self) -> std::io::Result<&[u8]> {
        self.encoded
            .get(self.range.clone())
            .ok_or_else(|| std::io::Error::other("invalid WAL pending chunk range"))
    }
}

#[derive(Debug)]
struct Group {
    chunks: Vec<EncodedChunk>,
    encoded_bytes: usize,
    records: usize,
    last_seq: Option<LogSeq>,
}

/// Single-store WAL writer using caller threads as leader and followers.
///
/// There is no timer and no background flusher. An idle arrival immediately
/// becomes leader. Arrivals observed while its barrier is in flight form the
/// next byte-bounded group, which that leader issues immediately afterward.
pub struct WalWriter {
    file: Mutex<Box<dyn VfsFile>>,
    created_directory_sync: Mutex<Option<CreatedDirectorySync>>,
    state: Mutex<WriterState>,
    changed: Condvar,
    sync: SyncRequirement,
    max_group_bytes: usize,
    durable_progress: AtomicU64,
}

struct CreatedDirectorySync {
    vfs: Arc<dyn Vfs>,
    directory: PathBuf,
    kind: SyncKind,
}

impl WalWriter {
    /// Creates a writer over a new or empty WAL path.
    ///
    /// A resolved full-sync data-file requirement uses the 16 MiB durable group
    /// bound. Barrier and skip requirements retain the 1 MiB default.
    pub fn create(
        vfs: &dyn Vfs,
        path: &Path,
        first_seq: LogSeq,
        policy: DurabilityPolicy,
    ) -> Result<Self, WalWriteError> {
        let max_group_bytes = default_max_group_bytes(policy);
        Self::create_with_max_group_bytes(vfs, path, first_seq, policy, max_group_bytes)
    }

    pub(crate) fn resume(
        vfs: &dyn Vfs,
        path: &Path,
        recovered: CleanWalReader,
        policy: DurabilityPolicy,
    ) -> Result<Self, WalWriteError> {
        let max_group_bytes = default_max_group_bytes(policy);
        let next_seq = recovered.next_seq;
        let durable_end = recovered.durable_end;
        let durable_progress = durable_end.map_or(0, LogSeq::get);
        let visible = VecDeque::from(recovered.records);
        let retained_bytes = visible.iter().try_fold(0_usize, |total, record| {
            total
                .checked_add(record.encoded_len())
                .ok_or(WalWriteError::RecoveredBytesOverflow)
        })?;
        let file = vfs.open_append(path).map_err(WalWriteError::from_io)?;
        Ok(Self {
            file: Mutex::new(file),
            created_directory_sync: Mutex::new(None),
            state: Mutex::new(WriterState {
                next_seq,
                visible,
                retained_bytes,
                durable_end,
                pending_groups: VecDeque::with_capacity(2),
                spare_chunks: Vec::with_capacity(16),
                barrier_in_flight: false,
                failure: None,
                completed_groups: 0,
                completed_records: 0,
                completed_bytes: 0,
                encoded_buffer_allocations: 0,
                group_size_histogram: GroupSizeHistogram::new(),
                recent_groups: VecDeque::with_capacity(RECENT_GROUP_LIMIT),
            }),
            changed: Condvar::new(),
            sync: policy.data_file_sync(),
            max_group_bytes,
            durable_progress: AtomicU64::new(durable_progress),
        })
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
        let header_len = header.len();
        let header = Arc::new(header);
        let mut header_chunks = Vec::with_capacity(16);
        header_chunks.push(EncodedChunk {
            range: 0..header_len,
            encoded: header,
        });
        let mut pending_groups = VecDeque::with_capacity(2);
        pending_groups.push_back(Group {
            chunks: header_chunks,
            encoded_bytes: header_len,
            records: 0,
            last_seq: None,
        });
        let spare_chunks = Vec::with_capacity(16);
        let recent_groups = VecDeque::with_capacity(RECENT_GROUP_LIMIT);
        Ok(Self {
            file: Mutex::new(file),
            created_directory_sync: Mutex::new(None),
            state: Mutex::new(WriterState {
                next_seq: first_seq.get(),
                visible: VecDeque::new(),
                retained_bytes: 0,
                durable_end: None,
                pending_groups,
                spare_chunks,
                barrier_in_flight: false,
                failure: None,
                completed_groups: 0,
                completed_records: 0,
                completed_bytes: 0,
                encoded_buffer_allocations: 0,
                group_size_histogram: GroupSizeHistogram::new(),
                recent_groups,
            }),
            changed: Condvar::new(),
            sync: policy.data_file_sync(),
            max_group_bytes,
            durable_progress: AtomicU64::new(first_seq.get().saturating_sub(1)),
        })
    }

    pub(crate) fn create_store_wal(
        vfs: Arc<dyn Vfs>,
        directory: &Path,
        path: &Path,
        first_seq: LogSeq,
        policy: DurabilityPolicy,
    ) -> Result<Self, WalWriteError> {
        let created = match vfs.open(path) {
            Ok(_) => false,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => return Err(WalWriteError::from_io(error)),
        };
        let mut writer = Self::create(vfs.as_ref(), path, first_seq, policy)?;
        if created && let SyncRequirement::Sync(kind) = policy.directory_sync() {
            writer.created_directory_sync = Mutex::new(Some(CreatedDirectorySync {
                vfs,
                directory: directory.to_path_buf(),
                kind,
            }));
        }
        Ok(writer)
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

    /// Makes every record in one caller batch visible in input order.
    ///
    /// Returns after every record is visible. As with [`Self::commit`], an
    /// idle caller that becomes leader runs the pending flushes inline before
    /// returning. An empty batch returns the empty range at the next sequence
    /// and performs no I/O.
    ///
    /// The returned half-open range contains exactly the assigned sequences.
    /// If staging fails, the batch consumes no sequence and publishes no
    /// visibility. After staging succeeds, an append or flush failure follows
    /// the existing poisoned-writer path: the staged records remain visible,
    /// this call returns [`WalWriteError::Failed`], and the writer rejects
    /// subsequent work.
    pub fn commit_many(&self, records: &[(u16, &[u8])]) -> Result<Range<LogSeq>, WalWriteError> {
        let (range, leader) = self.stage_many(records)?;
        if leader {
            self.flush_as_leader()?;
        }
        Ok(range)
    }

    /// Makes every record in one caller batch visible in input order, then
    /// waits until the tier-specific flush covering the final record returns.
    ///
    /// An empty batch returns the empty range at the next sequence and performs
    /// no I/O.
    ///
    /// The returned half-open range contains exactly the assigned sequences.
    /// If staging fails, the batch consumes no sequence and publishes no
    /// visibility. After staging succeeds, an append or flush failure follows
    /// the existing poisoned-writer path: the staged records remain visible,
    /// this call returns [`WalWriteError::Failed`], and the writer rejects
    /// subsequent work.
    pub fn commit_many_durable(
        &self,
        records: &[(u16, &[u8])],
    ) -> Result<Range<LogSeq>, WalWriteError> {
        let (range, leader) = self.stage_many(records)?;
        if leader {
            self.flush_as_leader()?;
        }
        if range.start != range.end {
            let last = LogSeq::new(range.end.get().saturating_sub(1));
            self.wait_until_durable(last)?;
        }
        Ok(range)
    }

    fn stage_many(&self, records: &[(u16, &[u8])]) -> Result<(Range<LogSeq>, bool), WalWriteError> {
        self.stage_many_with_encoder(records, append_record_into)
    }

    fn stage_many_with_encoder<F>(
        &self,
        records: &[(u16, &[u8])],
        mut encode: F,
    ) -> Result<(Range<LogSeq>, bool), WalWriteError>
    where
        F: for<'a> FnMut(WalRecord<'a>, &mut Vec<u8>) -> Result<(), RecordEncodeError>,
    {
        if records.is_empty() {
            let state = self.lock_state()?;
            if let Some(failure) = &state.failure {
                return Err(WalWriteError::failed(failure));
            }
            let next = LogSeq::new(state.next_seq);
            return Ok((next..next, false));
        }
        let encoded_lengths = records
            .iter()
            .map(|&(op, payload)| {
                let encoded_len =
                    encoded_record_len(payload.len()).map_err(WalWriteError::Record)?;
                Ok((op, payload, encoded_len))
            })
            .collect::<Result<Vec<_>, WalWriteError>>()?;
        let batch_bytes = encoded_lengths
            .iter()
            .fold(0_usize, |total, record| total.saturating_add(record.2));
        if let Some((_, _, encoded_bytes)) = encoded_lengths
            .iter()
            .find(|(_, _, encoded_bytes)| *encoded_bytes > self.max_group_bytes)
        {
            return Err(WalWriteError::GroupTooLarge {
                encoded_bytes: *encoded_bytes,
                max_group_bytes: self.max_group_bytes,
            });
        }
        let mut encoded = Arc::new(Vec::with_capacity(batch_bytes));
        let mut state = self.lock_state()?;
        if let Some(failure) = &state.failure {
            return Err(WalWriteError::failed(failure));
        }
        Self::ensure_pending_group(&mut state);
        let first_encoded_len = encoded_lengths
            .first()
            .map(|(_, _, encoded_len)| *encoded_len)
            .ok_or(WalWriteError::Poisoned("non-empty WAL batch"))?;
        let first_required = state
            .pending_groups
            .back()
            .map(|group| group.encoded_bytes.saturating_add(first_encoded_len))
            .ok_or(WalWriteError::Poisoned("pending WAL group"))?;
        if state
            .pending_groups
            .back()
            .is_some_and(|group| group.records == 0 && first_required > self.max_group_bytes)
        {
            return Err(WalWriteError::GroupTooLarge {
                encoded_bytes: first_required,
                max_group_bytes: self.max_group_bytes,
            });
        }
        let count = u64::try_from(records.len()).map_err(|_| WalWriteError::SequenceExhausted)?;
        let start = state.next_seq;
        let end = start
            .checked_add(count)
            .ok_or(WalWriteError::SequenceExhausted)?;
        let mut seq = start;
        let storage = Arc::get_mut(&mut encoded)
            .ok_or(WalWriteError::Poisoned("unique preallocated WAL batch"))?;
        for (op, payload, _) in &encoded_lengths {
            encode(
                WalRecord {
                    seq: LogSeq::new(seq),
                    op: *op,
                    payload,
                },
                storage,
            )
            .map_err(WalWriteError::Record)?;
            seq = seq.saturating_add(1);
        }

        seq = start;
        let mut offset = 0_usize;
        let mut chunk_start = 0_usize;
        for (op, _, encoded_len) in encoded_lengths {
            let record_seq = LogSeq::new(seq);
            let record_start = offset;
            let record_end = offset.saturating_add(encoded_len);
            while offset < record_end {
                let available = self
                    .max_group_bytes
                    .saturating_sub(Self::current_group_ref(&state)?.encoded_bytes);
                if available == 0 {
                    if chunk_start < offset {
                        let group = Self::current_group(&mut state)?;
                        group.chunks.push(EncodedChunk {
                            encoded: Arc::clone(&encoded),
                            range: chunk_start..offset,
                        });
                    }
                    Self::push_empty_pending_group(&mut state);
                    chunk_start = offset;
                    continue;
                }
                let written = available.min(record_end.saturating_sub(offset));
                let group = Self::current_group(&mut state)?;
                group.encoded_bytes = group.encoded_bytes.saturating_add(written);
                offset = offset.saturating_add(written);
            }
            let group = Self::current_group(&mut state)?;
            group.records = group.records.saturating_add(1);
            group.last_seq = Some(record_seq);
            state.retained_bytes = state.retained_bytes.saturating_add(encoded_len);
            state.visible.push_back(VisibleRecord::writer_owned(
                record_seq,
                op,
                Arc::clone(&encoded),
                record_start..record_end,
            ));
            seq = seq.saturating_add(1);
        }
        let group = Self::current_group(&mut state)?;
        group.chunks.push(EncodedChunk {
            encoded,
            range: chunk_start..offset,
        });
        state.next_seq = end;
        state.encoded_buffer_allocations = state.encoded_buffer_allocations.saturating_add(1);
        let leader = !state.barrier_in_flight;
        if leader {
            state.barrier_in_flight = true;
        }
        Ok((LogSeq::new(start)..LogSeq::new(end), leader))
    }

    #[inline(always)]
    fn current_group(state: &mut WriterState) -> Result<&mut Group, WalWriteError> {
        state
            .pending_groups
            .back_mut()
            .ok_or(WalWriteError::Poisoned("pending WAL group"))
    }

    #[inline(always)]
    fn current_group_ref(state: &WriterState) -> Result<&Group, WalWriteError> {
        state
            .pending_groups
            .back()
            .ok_or(WalWriteError::Poisoned("pending WAL group"))
    }

    fn ensure_pending_group(state: &mut WriterState) {
        if state.pending_groups.is_empty() {
            Self::push_empty_pending_group(state);
        }
    }

    fn push_empty_pending_group(state: &mut WriterState) {
        let chunks = std::mem::take(&mut state.spare_chunks);
        state.pending_groups.push_back(Group {
            chunks,
            encoded_bytes: 0,
            records: 0,
            last_seq: None,
        });
    }

    /// Waits until every record visible when this call observes the writer has flushed.
    ///
    /// No record is appended solely to force progress. If pending records have
    /// no current leader, this caller assumes leadership and drains them.
    pub fn flush(&self) -> Result<(), WalWriteError> {
        let (target, leader) = {
            let mut state = self.lock_state()?;
            if let Some(failure) = &state.failure {
                return Err(WalWriteError::failed(failure));
            }
            let Some(target) = state.visible.back().map(|record| record.seq) else {
                return Ok(());
            };
            if state
                .durable_end
                .is_some_and(|durable_end| durable_end >= target)
            {
                return Ok(());
            }
            let leader = if state.barrier_in_flight {
                false
            } else if state.pending_groups.iter().any(|group| group.records != 0) {
                state.barrier_in_flight = true;
                true
            } else {
                return Err(WalWriteError::Poisoned(
                    "non-durable WAL record has neither a pending group nor a leader",
                ));
            };
            (target, leader)
        };
        if leader {
            self.flush_as_leader()?;
        }
        self.wait_until_durable(target)
    }

    /// Returns at most `limit` retained records at or after `first_seq`.
    ///
    /// Clones share the encoded WAL allocations. The explicit bound prevents
    /// test-support callers from copying metadata for the writer's history.
    pub fn visible_records(
        &self,
        first_seq: LogSeq,
        limit: usize,
    ) -> Result<Vec<VisibleRecord>, WalWriteError> {
        let mut visible = Vec::with_capacity(limit);
        let state = self.lock_state()?;
        visible.extend(
            state
                .visible
                .iter()
                .filter(|record| record.seq >= first_seq)
                .take(limit)
                .cloned(),
        );
        Ok(visible)
    }

    /// Releases retained visibility records through an absorbed durable sequence.
    ///
    /// The ingest owner calls this only after incorporating the records into
    /// its own in-memory state. Requests beyond the tier-specific durable end
    /// fail explicitly and never clamp.
    pub fn retire_visible_through(
        &self,
        absorbed_through: LogSeq,
    ) -> Result<WalRetirement, WalRetireError> {
        let capacity = {
            let state = self.lock_state().map_err(WalRetireError::Writer)?;
            if state
                .durable_end
                .is_none_or(|durable_end| absorbed_through > durable_end)
            {
                return Err(WalRetireError::BeyondDurable {
                    requested: absorbed_through,
                    durable_end: state.durable_end,
                });
            }
            state
                .visible
                .iter()
                .take_while(|record| record.seq <= absorbed_through)
                .count()
        };
        let mut retired = Vec::with_capacity(capacity);
        let retirement = {
            let mut state = self.lock_state().map_err(WalRetireError::Writer)?;
            if state
                .durable_end
                .is_none_or(|durable_end| absorbed_through > durable_end)
            {
                return Err(WalRetireError::BeyondDurable {
                    requested: absorbed_through,
                    durable_end: state.durable_end,
                });
            }
            while state
                .visible
                .front()
                .is_some_and(|record| record.seq <= absorbed_through)
            {
                let Some(record) = state.visible.pop_front() else {
                    break;
                };
                state.retained_bytes = state.retained_bytes.saturating_sub(record.encoded_len());
                retired.push(record);
            }
            WalRetirement {
                absorbed_through,
                records_released: retired.len(),
                encoded_bytes_released: retired.iter().fold(0_usize, |total, record| {
                    total.saturating_add(record.encoded_len())
                }),
                retained_records: state.visible.len(),
                retained_bytes: state.retained_bytes,
                durable_end: state.durable_end,
            }
        };
        drop(retired);
        Ok(retirement)
    }

    /// Returns deterministic progress and completed-group statistics.
    pub fn stats(&self) -> Result<WalWriterStats, WalWriteError> {
        let mut recent_groups = Vec::with_capacity(RECENT_GROUP_LIMIT);
        let state = self.lock_state()?;
        recent_groups.extend(state.recent_groups.iter().copied());
        let pending_records = state
            .pending_groups
            .iter()
            .fold(0_usize, |total, group| total.saturating_add(group.records));
        Ok(WalWriterStats {
            completed_groups: state.completed_groups,
            completed_records: state.completed_records,
            completed_bytes: state.completed_bytes,
            group_size_histogram: state.group_size_histogram,
            recent_groups,
            pending_records,
            visible_end: state.visible.back().map(|record| record.seq),
            durable_end: state.durable_end,
            retained_records: state.visible.len(),
            retained_bytes: state.retained_bytes,
            encoded_buffer_allocations: state.encoded_buffer_allocations,
        })
    }

    fn stage(&self, op: u16, payload: &[u8]) -> Result<(LogSeq, bool), WalWriteError> {
        let encoded_len = encoded_record_len(payload.len()).map_err(WalWriteError::Record)?;
        let mut encoded = Arc::new(Vec::with_capacity(encoded_len));
        let mut state = self.lock_state()?;
        loop {
            if let Some(failure) = &state.failure {
                return Err(WalWriteError::failed(failure));
            }
            Self::ensure_pending_group(&mut state);
            let seq = LogSeq::new(state.next_seq);
            let (required, pending_records) = state
                .pending_groups
                .back()
                .map(|group| {
                    (
                        group.encoded_bytes.saturating_add(encoded_len),
                        group.records,
                    )
                })
                .ok_or(WalWriteError::Poisoned("pending WAL group"))?;
            if required <= self.max_group_bytes {
                let next_seq = state
                    .next_seq
                    .checked_add(1)
                    .ok_or(WalWriteError::SequenceExhausted)?;
                let storage = Arc::get_mut(&mut encoded)
                    .ok_or(WalWriteError::Poisoned("unique preallocated WAL record"))?;
                encode_record_into(WalRecord { seq, op, payload }, storage)
                    .map_err(WalWriteError::Record)?;
                state.next_seq = next_seq;
                state.encoded_buffer_allocations =
                    state.encoded_buffer_allocations.saturating_add(1);
                let group = state
                    .pending_groups
                    .back_mut()
                    .ok_or(WalWriteError::Poisoned("pending WAL group"))?;
                group.chunks.push(EncodedChunk {
                    encoded: Arc::clone(&encoded),
                    range: 0..encoded_len,
                });
                group.encoded_bytes = required;
                group.records = group.records.saturating_add(1);
                group.last_seq = Some(seq);
                state.retained_bytes = state.retained_bytes.saturating_add(encoded_len);
                state.visible.push_back(VisibleRecord::writer_owned(
                    seq,
                    op,
                    encoded,
                    0..encoded_len,
                ));
                let leader = !state.barrier_in_flight;
                if leader {
                    state.barrier_in_flight = true;
                }
                return Ok((seq, leader));
            }
            if pending_records == 0 {
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
            let (group, last_seq) = {
                let mut state = self.lock_state()?;
                let group = state
                    .pending_groups
                    .pop_front()
                    .ok_or(WalWriteError::Poisoned("WAL leader found no pending group"))?;
                let last_seq = group.last_seq.ok_or(WalWriteError::Poisoned(
                    "WAL leader found no pending sequence",
                ))?;
                self.changed.notify_all();
                (group, last_seq)
            };
            let failure = self.write_group(&group).err().map(|error| {
                Arc::new(Failure {
                    kind: error.kind(),
                    detail: Arc::from(error.to_string()),
                })
            });
            let completed = CompletedGroup {
                records: group.records,
                encoded_bytes: group.encoded_bytes,
            };
            let mut reusable_chunks = group.chunks;
            reusable_chunks.clear();
            let mut state = self.lock_state()?;
            state.spare_chunks = reusable_chunks;
            match failure {
                None => {
                    state.durable_end = Some(last_seq);
                    self.durable_progress
                        .store(last_seq.get(), Ordering::Release);
                    state.completed_groups = state.completed_groups.saturating_add(1);
                    state.completed_records = state
                        .completed_records
                        .saturating_add(completed.records as u64);
                    state.completed_bytes = state
                        .completed_bytes
                        .saturating_add(completed.encoded_bytes as u64);
                    state.group_size_histogram.record(completed.records);
                    if state.recent_groups.len() == RECENT_GROUP_LIMIT {
                        state.recent_groups.pop_front();
                    }
                    state.recent_groups.push_back(completed);
                    self.changed.notify_all();
                    if state.pending_groups.is_empty() {
                        state.barrier_in_flight = false;
                        return Ok(());
                    }
                }
                Some(failure) => {
                    state.failure = Some(Arc::clone(&failure));
                    state.barrier_in_flight = false;
                    self.changed.notify_all();
                    return Err(WalWriteError::failed(&failure));
                }
            }
        }
    }

    fn write_group(&self, group: &Group) -> std::io::Result<()> {
        let mut buffers = Vec::with_capacity(group.chunks.len());
        for chunk in &group.chunks {
            buffers.push(IoSlice::new(chunk.bytes()?));
        }
        let mut file = self
            .file
            .lock()
            .map_err(|_| std::io::Error::other("WAL file mutex poisoned"))?;
        file.append_vectored(&mut buffers)?;
        match self.sync {
            SyncRequirement::Skip => Ok(()),
            SyncRequirement::Sync(kind) => file.sync(kind),
        }?;
        drop(file);
        let mut created_directory_sync = self
            .created_directory_sync
            .lock()
            .map_err(|_| std::io::Error::other("WAL directory-sync mutex poisoned"))?;
        if let Some(sync) = created_directory_sync.as_ref() {
            sync.vfs.sync(&sync.directory, sync.kind)?;
            created_directory_sync.take();
        }
        Ok(())
    }

    fn wait_until_durable(&self, seq: LogSeq) -> Result<(), WalWriteError> {
        let mut state = self.lock_state()?;
        while state
            .durable_end
            .is_none_or(|durable_end| durable_end < seq)
        {
            if let Some(failure) = &state.failure {
                return Err(WalWriteError::failed(failure));
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

pub(crate) fn encode_wal_image(
    first_seq: LogSeq,
    records: &[(u16, Vec<u8>)],
) -> Result<Vec<u8>, WalWriteError> {
    let mut encoded = header::encode_header(first_seq).map_err(WalWriteError::Header)?;
    let mut sequence = first_seq.get();
    for (op, payload) in records {
        record::append_record_into(
            record::WalRecord {
                seq: LogSeq::new(sequence),
                op: *op,
                payload,
            },
            &mut encoded,
        )
        .map_err(WalWriteError::Record)?;
        sequence = sequence
            .checked_add(1)
            .ok_or(WalWriteError::SequenceExhausted)?;
    }
    Ok(encoded)
}

fn default_max_group_bytes(policy: DurabilityPolicy) -> usize {
    match policy.data_file_sync() {
        SyncRequirement::Sync(SyncKind::Full) => DEFAULT_MAX_GROUP_BYTES_DURABLE,
        SyncRequirement::Skip | SyncRequirement::Sync(SyncKind::Barrier) => DEFAULT_MAX_GROUP_BYTES,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::path::Path;

    use super::{LogSeq, RecordEncodeError, WalWriteError, WalWriter, append_record_into};
    use crate::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
    use crate::vfs::CountingVfs;
    use crate::vfs::crash::MemoryVfs;

    #[test]
    fn durable_policy_defaults_to_a_sixteen_mib_group_cap() {
        let counting = CountingVfs::new(MemoryVfs::new());
        let policy = DurabilityPolicy::new(DurabilityMode::Durable, CommitTier::Durable)
            .expect("durable policy");
        let writer = WalWriter::create(
            &counting,
            Path::new("/wal/default-durable-cap.ze"),
            LogSeq::new(1),
            policy,
        )
        .expect("writer");
        let payload = vec![0x5a; 1_024];
        let records = (0..1_024)
            .map(|_| (7, payload.as_slice()))
            .collect::<Vec<_>>();

        writer.commit_many(&records).expect("large durable batch");
        let stats = writer.stats().expect("writer stats");

        assert_eq!(
            (
                counting.append_calls(),
                counting.handle_full_sync_calls(),
                stats.completed_groups,
                stats.completed_records,
            ),
            (1, 1, 1, 1_024),
            "a 1,071,144-byte durable batch must share one append and full sync"
        );
    }

    #[test]
    fn ordered_and_skip_policies_keep_the_one_mib_default() {
        let payload = vec![0x6b; 1_024];
        let records = (0..1_024)
            .map(|_| (8, payload.as_slice()))
            .collect::<Vec<_>>();
        let mut actual = Vec::new();

        for (tier, path) in [
            (CommitTier::Ordered, "/wal/default-ordered-cap.ze"),
            (CommitTier::None, "/wal/default-skip-cap.ze"),
        ] {
            let counting = CountingVfs::new(MemoryVfs::new());
            let policy =
                DurabilityPolicy::new(DurabilityMode::Durable, tier).expect("supported policy");
            let writer = WalWriter::create(&counting, Path::new(path), LogSeq::new(1), policy)
                .expect("writer");

            writer.commit_many(&records).expect("large batch");
            let stats = writer.stats().expect("writer stats");
            actual.push((
                tier,
                counting.append_calls(),
                counting.handle_barrier_sync_calls(),
                counting.handle_full_sync_calls(),
                stats.completed_groups,
            ));
        }

        assert_eq!(
            actual,
            vec![
                (CommitTier::Ordered, 2, 2, 0, 2),
                (CommitTier::None, 2, 0, 0, 2),
            ],
            "ordered and skip policies must split a 1,071,144-byte batch at one MiB"
        );
    }

    #[test]
    fn an_explicit_group_cap_still_wins() {
        let counting = CountingVfs::new(MemoryVfs::new());
        let policy = DurabilityPolicy::new(DurabilityMode::Durable, CommitTier::Durable)
            .expect("durable policy");
        let writer = WalWriter::create_with_max_group_bytes(
            &counting,
            Path::new("/wal/explicit-group-cap.ze"),
            LogSeq::new(1),
            policy,
            128,
        )
        .expect("writer");
        writer.commit(0, &[0; 10]).expect("flush WAL header");
        let groups_before = writer.stats().expect("prime stats").recent_groups.len();
        counting.reset();
        let payload = [0x7c; 10];
        let records = (0..12).map(|_| (9, payload.as_slice())).collect::<Vec<_>>();

        writer.commit_many(&records).expect("explicit-cap batch");
        let stats = writer.stats().expect("batch stats");
        let groups = stats
            .recent_groups
            .iter()
            .skip(groups_before)
            .map(|group| (group.records, group.encoded_bytes))
            .collect::<Vec<_>>();

        assert_eq!(
            (
                counting.append_calls(),
                counting.handle_full_sync_calls(),
                groups,
            ),
            (3, 3, vec![(4, 128), (4, 128), (4, 128)]),
            "an explicit 128-byte cap must override the durable default"
        );
    }

    #[test]
    fn encoding_failure_in_middle_of_commit_many_is_atomic() {
        let policy = DurabilityPolicy::new(DurabilityMode::Durable, CommitTier::None)
            .expect("supported policy");
        let writer = WalWriter::create(
            &MemoryVfs::new(),
            Path::new("/wal/unit-encode-failure.ze"),
            LogSeq::new(1),
            policy,
        )
        .expect("writer");
        let records = [(51, b"first".as_slice()), (52, b"second".as_slice())];
        let mut encode_calls = 0_usize;

        let result = writer.stage_many_with_encoder(&records, |record, storage| {
            encode_calls = encode_calls.saturating_add(1);
            if encode_calls == 2 {
                Err(RecordEncodeError::PayloadTooLarge(usize::MAX))
            } else {
                append_record_into(record, storage)
            }
        });
        let failure = match result {
            Err(WalWriteError::Record(error)) => format!("error={error}"),
            other => format!("unexpected={other:?}"),
        };
        let visible = writer
            .visible_records(LogSeq::new(1), records.len())
            .expect("visibility after failed encoding")
            .len();
        let next = writer
            .commit(53, b"after")
            .expect("sequence remains reusable");

        assert_eq!(
            (failure, visible, next),
            (
                format!("error=WAL payload length {} exceeds u32", usize::MAX),
                0,
                LogSeq::new(1),
            ),
            "a failure encoding any batch record must consume no sequence and publish no visibility"
        );
    }
}

impl DurableLog for WalWriter {
    fn durable_end(&self) -> u64 {
        self.durable_progress.load(Ordering::Acquire)
    }
}

/// Checked access failure for a retained encoded record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VisibleRecordError {
    /// The retained range does not fit its shared encoded allocation.
    InvalidEncodedRange {
        /// Inclusive byte offset.
        start: usize,
        /// Exclusive byte offset.
        end: usize,
        /// Bytes available in the allocation.
        available: usize,
    },
    /// The retained bytes no longer decode as the record published by the writer.
    Record(RecordError),
}

impl std::fmt::Display for VisibleRecordError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidEncodedRange {
                start,
                end,
                available,
            } => write!(
                formatter,
                "visible WAL encoded range {start}..{end} exceeds {available} bytes"
            ),
            Self::Record(error) => write!(formatter, "visible WAL record: {error}"),
        }
    }
}

impl std::error::Error for VisibleRecordError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidEncodedRange { .. } => None,
            Self::Record(error) => Some(error),
        }
    }
}

/// Exact effect of one visibility-retirement request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WalRetirement {
    /// Caller-declared sequence absorbed into its own in-memory state.
    pub absorbed_through: LogSeq,
    /// Retained records released by this call.
    pub records_released: usize,
    /// Encoded record bytes released by this call.
    pub encoded_bytes_released: usize,
    /// Records that remain retained by the writer.
    pub retained_records: usize,
    /// Encoded record bytes that remain retained by the writer.
    pub retained_bytes: usize,
    /// Durable end observed while applying retirement.
    pub durable_end: Option<LogSeq>,
}

/// Visibility retirement failure.
#[derive(Debug)]
pub enum WalRetireError {
    /// The caller asked to retire bytes whose tier-specific flush has not returned.
    BeyondDurable {
        /// Requested inclusive retirement sequence.
        requested: LogSeq,
        /// Largest sequence whose flush returned.
        durable_end: Option<LogSeq>,
    },
    /// Writer state could not be inspected safely.
    Writer(WalWriteError),
}

impl std::fmt::Display for WalRetireError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BeyondDurable {
                requested,
                durable_end: Some(durable_end),
            } => write!(
                formatter,
                "cannot retire WAL sequence {} past durable end {}",
                requested.get(),
                durable_end.get()
            ),
            Self::BeyondDurable {
                requested,
                durable_end: None,
            } => write!(
                formatter,
                "cannot retire WAL sequence {} before any sequence is durable",
                requested.get()
            ),
            Self::Writer(error) => write!(formatter, "cannot retire visible WAL records: {error}"),
        }
    }
}

impl std::error::Error for WalRetireError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BeyondDurable { .. } => None,
            Self::Writer(error) => Some(error),
        }
    }
}

/// Owned checked WAL prefix recovered from a filesystem image.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalReader {
    records: Vec<VisibleRecord>,
    next_seq: u64,
    durable_end: u64,
    terminator: Option<ReplayTerminator>,
}

pub(crate) struct CleanWalReader {
    records: Vec<VisibleRecord>,
    next_seq: u64,
    durable_end: Option<LogSeq>,
}

impl CleanWalReader {
    pub(crate) fn records(&self) -> &[VisibleRecord] {
        &self.records
    }
}

impl WalReader {
    /// Opens the largest checksum- and sequence-valid WAL prefix.
    pub fn open(vfs: &dyn Vfs, path: &Path) -> Result<Self, WalReadError> {
        let bytes = match vfs.read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    records: Vec::new(),
                    next_seq: 0,
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
                next_seq: 0,
                durable_end: 0,
                terminator: Some(terminator),
            });
        }
        let header = header::decode_header(&bytes).map_err(WalReadError::Header)?;
        let mut offset = header::WAL_HEADER_LEN;
        let recovered = replayed
            .records
            .iter()
            .map(|record| {
                let encoded_len = record::MIN_RECORD_LEN.saturating_add(record.payload.len());
                let range = offset..offset.saturating_add(encoded_len);
                offset = range.end;
                (record.seq, record.op, range)
            })
            .collect::<Vec<_>>();
        drop(replayed);
        let records = recovered
            .into_iter()
            .map(|(seq, op, range)| {
                let start = range.start;
                let end = range.end;
                let encoded = bytes
                    .get(range)
                    .ok_or(WalReadError::InvalidRecoveredRange {
                        start,
                        end,
                        available: bytes.len(),
                    })?;
                let encoded = Arc::new(encoded.to_vec());
                let length = encoded.len();
                Ok(VisibleRecord::recovered(seq, op, encoded, 0..length))
            })
            .collect::<Result<Vec<_>, WalReadError>>()?;
        let durable_end = records.last().map_or_else(
            || header.first_seq.get().saturating_sub(1),
            |record| record.seq.get(),
        );
        let next_seq = records.last().map_or(header.first_seq.get(), |record| {
            record.seq.get().saturating_add(1)
        });
        Ok(Self {
            records,
            next_seq,
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

    pub(crate) fn into_clean(self) -> Result<CleanWalReader, WalRecoveryError> {
        match self.terminator {
            Some(ReplayTerminator::CleanEnd) => {
                let durable_end = Some(LogSeq::new(self.durable_end));
                Ok(CleanWalReader {
                    records: self.records,
                    next_seq: self.next_seq,
                    durable_end,
                })
            }
            Some(ReplayTerminator::InvalidHeader(error)) => {
                let error = WalRecoveryError::InvalidHeader(error);
                #[cfg(any(test, feature = "test-support"))]
                crate::lifecycle::record_storage_wal_recovery_fault(&error);
                Err(error)
            }
            Some(ReplayTerminator::CorruptAt { offset, reason }) => {
                let error = WalRecoveryError::CorruptAt { offset, reason };
                #[cfg(any(test, feature = "test-support"))]
                crate::lifecycle::record_storage_wal_recovery_fault(&error);
                Err(error)
            }
            None => Err(WalRecoveryError::MissingFile),
        }
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
    /// Recovered record byte accounting exceeded the address space.
    RecoveredBytesOverflow,
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
        detail: Arc<str>,
    },
}

impl WalWriteError {
    fn from_io(error: std::io::Error) -> Self {
        Self::Failed {
            kind: error.kind(),
            detail: Arc::from(error.to_string()),
        }
    }

    fn failed(failure: &Failure) -> Self {
        Self::Failed {
            kind: failure.kind,
            detail: Arc::clone(&failure.detail),
        }
    }
}

impl std::fmt::Display for WalWriteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonEmptyWal { length } => {
                write!(formatter, "WAL path is not empty: {length} bytes")
            }
            Self::RecoveredBytesOverflow => {
                formatter.write_str("recovered WAL record bytes overflow usize")
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

/// A non-empty WAL cannot be resumed because replay did not reach a clean end.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalRecoveryError {
    /// The requested WAL path did not exist and therefore cannot be resumed.
    MissingFile,
    /// The persisted WAL header is absent, truncated, or invalid.
    InvalidHeader(header::WalHeaderError),
    /// Record replay stopped at a typed corruption boundary.
    CorruptAt {
        /// Byte offset of the first invalid record.
        offset: usize,
        /// Exact framing, checksum, or sequence failure.
        reason: replay::CorruptionReason,
    },
}

impl std::fmt::Display for WalRecoveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingFile => formatter.write_str("WAL recovery path is missing"),
            Self::InvalidHeader(error) => write!(formatter, "WAL recovery header: {error}"),
            Self::CorruptAt { offset, reason } => {
                write!(
                    formatter,
                    "WAL recovery stopped at byte {offset}: {reason:?}"
                )
            }
        }
    }
}

impl std::error::Error for WalRecoveryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidHeader(error) => Some(error),
            Self::MissingFile | Self::CorruptAt { .. } => None,
        }
    }
}

/// WAL recovery failure requiring the caller to stop rather than hide corruption.
#[derive(Debug)]
pub enum WalReadError {
    /// Filesystem read failed.
    Io(std::io::Error),
    /// A header accepted by replay could not be decoded for resume state.
    Header(header::WalHeaderError),
    /// A replay-derived encoded record range exceeded the WAL image.
    InvalidRecoveredRange {
        /// First encoded byte of the recovered record.
        start: usize,
        /// Exclusive end of the recovered record.
        end: usize,
        /// Complete WAL image length.
        available: usize,
    },
}

impl std::fmt::Display for WalReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "WAL read: {error}"),
            Self::Header(error) => write!(formatter, "WAL read header: {error}"),
            Self::InvalidRecoveredRange {
                start,
                end,
                available,
            } => write!(
                formatter,
                "WAL recovered range {start}..{end} exceeds {available} bytes"
            ),
        }
    }
}

impl std::error::Error for WalReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Header(error) => Some(error),
            Self::InvalidRecoveredRange { .. } => None,
        }
    }
}
