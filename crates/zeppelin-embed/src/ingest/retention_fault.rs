//! Test-support-only ingest/retention fault plans and production receipts.

use std::io::{IoSlice, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use xxhash_rust::xxh3::xxh3_64;

use crate::vfs::{SyncKind, Vfs, VfsFile};

const PURGE_CRASH_RECEIPT_MAGIC: &[u8; 8] = b"ZEIPRC01";
const PURGE_CRASH_RECEIPT_VERSION: u16 = 1;

/// Ingest-retention operation that consumed a selected test fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IngestRetentionOperation {
    /// Public atomic batch commit or acknowledged replay.
    BatchCommit,
    /// Public active-segment seal.
    Seal,
    /// Public explicit retention evaluation.
    Retention,
    /// Public physical-purge completion.
    Purge,
}

/// Closed feature-fault identity admitted by the current vertical slice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IngestRetentionFaultKind {
    /// Caller retries a completely acknowledged batch.
    PostAckRetry,
    /// The WAL append persisted a strict prefix before returning an error.
    PartialBatchAppend,
    /// Cancellation after segment write and before manifest commit.
    SealCancellation,
    /// Explicit host clock evaluated at the half-open retention boundary.
    RetentionClockBoundary,
    /// Old immutable segment unlink fails after replacement publication.
    PurgeUnlinkError,
    /// The process aborts after the durable purge intent and before rewriting.
    PurgeCrashBoundary,
}

/// Exact production checkpoint that emitted a receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IngestRetentionCheckpoint {
    /// Revision classification proved every submitted row was a replay.
    IngestReplayNoWalAppend,
    /// `Store::ingest` observed an error from the batch WAL append.
    IngestCommitManyAppendError,
    /// Segment cleanup completed after the late seal cancellation check.
    SealAfterSegmentWriteBeforeManifestCommit,
    /// Public retention report is complete and its boundary facts are known.
    RetentionPolicyEvaluated,
    /// Replacement manifest committed but old segment unlink failed.
    PurgeOldSegmentUnlinkError,
    /// Durable purge intent exists and no artifact rewrite has begun.
    PurgeAfterDurableIntentBeforeRewrite,
}

/// Closed public-purge crash checkpoint admitted by the feature campaign.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IngestRetentionPurgeCrashCheckpoint {
    /// `purge.ze` is durable and no segment or WAL rewrite has begun.
    AfterDurableIntentBeforeRewrite,
}

/// Closed I/O error class retained by an ingest-retention receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IngestRetentionIoKind {
    /// `std::io::ErrorKind::Other` from the deterministic partial writer.
    Other,
}

/// Primitive effect facts captured at the product operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IngestRetentionFaultEffect {
    /// Exact no-mutation facts returned by an acknowledged replay.
    PostAckRetry {
        /// Documents in the caller batch.
        batch_count: u64,
        /// Documents classified as exact replays.
        replay_count: u64,
        /// Previously acknowledged final sequence.
        returned_seq: u64,
        /// Previously acknowledged generation.
        returned_generation: u64,
        /// WAL records appended by the replay.
        wal_records_appended: u64,
        /// Store generation change caused by the replay.
        generation_delta: u64,
        /// Whether a replacement active segment was published.
        active_published: bool,
    },
    /// Exact facts from a failed WAL group append.
    PartialBatchAppend {
        /// Documents submitted in the public batch.
        submitted_count: u64,
        /// WAL mutation records prepared for changed documents.
        changed_records: u64,
        /// Complete encoded WAL group length.
        encoded_bytes: u64,
        /// Strict prefix persisted before the error.
        prefix_bytes: u64,
        /// Typed injected I/O error class.
        io_kind: IngestRetentionIoKind,
        /// Stable injected error detail.
        detail: String,
        /// Whether a replacement active segment was published.
        active_published: bool,
        /// Store generation change caused by the failed batch.
        generation_delta: u64,
    },
    /// Exact facts from cancellation after an immutable segment was written.
    SealCancellation {
        /// Physical active rows offered to the segment writer.
        active_rows: u64,
        /// WAL sequence boundary the candidate would have absorbed.
        absorbed_wal_end: u64,
        /// Candidate segment identity derived by the Store.
        candidate_segment: [u8; 16],
        /// Whether the replacement manifest became visible.
        manifest_committed: bool,
        /// Whether the uncommitted candidate path was removed.
        temporary_segment_removed: bool,
        /// Store generation change caused by cancellation.
        generation_delta: u64,
    },
    /// Exact facts from one explicit retention-policy evaluation.
    RetentionClockBoundary {
        /// Caller-supplied deterministic timestamp.
        supplied_now: i64,
        /// Positive retention window.
        window: i64,
        /// Saturating half-open cutoff.
        cutoff: i64,
        /// Inclusive lower bound of the drop range.
        range_start: i64,
        /// Exclusive upper bound of the drop range.
        range_end: i64,
        /// Generation returned by the public report.
        report_generation: u64,
        /// Immutable segments selected for drop.
        dropped_count: u64,
        /// Boundary-crossing segments retained.
        straddler_count: u64,
        /// Whether the manifest commit point was crossed.
        manifest_committed: bool,
    },
    /// Exact facts from a failed old-segment unlink after replacement commit.
    PurgeUnlinkError {
        /// Original immutable segment selected for replacement.
        original_segment: [u8; 16],
        /// Replacement immutable segment published by the manifest.
        replacement_segment: [u8; 16],
        /// Canonical original segment file name.
        old_file_name: String,
        /// Typed injected I/O error class.
        io_kind: IngestRetentionIoKind,
        /// Whether the replacement manifest crossed its commit point.
        replacement_manifest_committed: bool,
        /// Whether the durable purge intent remains for reopen recovery.
        intent_present: bool,
        /// Whether the original path remains linked after the failed delete.
        old_path_linked: bool,
    },
    /// Exact facts durably emitted immediately before the injected abort.
    PurgeCrashBoundary {
        /// Canonically sorted and deduplicated target document identities.
        target_ids: Vec<u128>,
        /// Public purge token identity encoded in the durable intent.
        token_id: u64,
        /// Canonical committed intent file name.
        intent_file_name: String,
        /// Whether the existing intent durability protocol returned.
        intent_durable: bool,
        /// Artifact rewrites completed before the crash point.
        artifact_rewrites: u64,
        /// Marker declaring that the next operation is process abort.
        child_aborted: bool,
    },
}

/// Exact plan armed by a deterministic test before the public operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IngestRetentionTestFault {
    /// Observe one exact retry of a completely acknowledged batch.
    PostAckRetry {
        /// Sequence returned by the first acknowledged call.
        first_ack_seq: u64,
        /// Generation returned by the first acknowledged call.
        first_ack_generation: u64,
    },
    /// Append an exact strict prefix of one WAL batch, then return `Other`.
    PartialBatchAppend {
        /// Bytes to append before returning the injected error.
        prefix_bytes: usize,
    },
    /// Cancel only at the post-write, pre-manifest seal checkpoint.
    SealCancellation,
    /// Observe one explicit manual-clock retention boundary.
    RetentionClockBoundary {
        /// Caller-supplied deterministic timestamp.
        now: i64,
        /// Positive retention window.
        window: i64,
        /// Independently declared expected cutoff.
        expected_cutoff: i64,
    },
    /// Fail deletion of one exact replaced immutable segment.
    PurgeUnlinkError {
        /// Original segment identity whose unlink must fail.
        target_segment: [u8; 16],
    },
    /// Abort after a durable intent and write the typed receipt first.
    PurgeCrashBoundary {
        /// Exact supported purge checkpoint.
        checkpoint: IngestRetentionPurgeCrashCheckpoint,
        /// Store-owned canonical receipt sink consumed by the parent.
        receipt_sink: PathBuf,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PartialBatchAppendObservation {
    pub(crate) invocation_id: u64,
    pub(crate) prefix_bytes: usize,
    pub(crate) encoded_bytes: usize,
    pub(crate) detail: String,
}

/// Fact-only receipt constructed solely by the production operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IngestRetentionFaultReceiptV1 {
    operation: IngestRetentionOperation,
    fault: IngestRetentionFaultKind,
    checkpoint: IngestRetentionCheckpoint,
    cardinality: u8,
    invocation_id: u64,
    effect: IngestRetentionFaultEffect,
}

impl IngestRetentionFaultReceiptV1 {
    pub(crate) const fn post_ack_retry(
        invocation_id: u64,
        batch_count: u64,
        replay_count: u64,
        returned_seq: u64,
        returned_generation: u64,
    ) -> Self {
        Self {
            operation: IngestRetentionOperation::BatchCommit,
            fault: IngestRetentionFaultKind::PostAckRetry,
            checkpoint: IngestRetentionCheckpoint::IngestReplayNoWalAppend,
            cardinality: 1,
            invocation_id,
            effect: IngestRetentionFaultEffect::PostAckRetry {
                batch_count,
                replay_count,
                returned_seq,
                returned_generation,
                wal_records_appended: 0,
                generation_delta: 0,
                active_published: false,
            },
        }
    }

    pub(crate) fn partial_batch_append(
        invocation_id: u64,
        submitted_count: u64,
        changed_records: u64,
        encoded_bytes: u64,
        prefix_bytes: u64,
        detail: String,
    ) -> Self {
        Self {
            operation: IngestRetentionOperation::BatchCommit,
            fault: IngestRetentionFaultKind::PartialBatchAppend,
            checkpoint: IngestRetentionCheckpoint::IngestCommitManyAppendError,
            cardinality: 1,
            invocation_id,
            effect: IngestRetentionFaultEffect::PartialBatchAppend {
                submitted_count,
                changed_records,
                encoded_bytes,
                prefix_bytes,
                io_kind: IngestRetentionIoKind::Other,
                detail,
                active_published: false,
                generation_delta: 0,
            },
        }
    }

    pub(crate) const fn seal_cancellation(
        invocation_id: u64,
        active_rows: u64,
        absorbed_wal_end: u64,
        candidate_segment: [u8; 16],
    ) -> Self {
        Self {
            operation: IngestRetentionOperation::Seal,
            fault: IngestRetentionFaultKind::SealCancellation,
            checkpoint: IngestRetentionCheckpoint::SealAfterSegmentWriteBeforeManifestCommit,
            cardinality: 1,
            invocation_id,
            effect: IngestRetentionFaultEffect::SealCancellation {
                active_rows,
                absorbed_wal_end,
                candidate_segment,
                manifest_committed: false,
                temporary_segment_removed: true,
                generation_delta: 0,
            },
        }
    }

    pub(crate) const fn retention_clock_boundary(
        invocation_id: u64,
        supplied_now: i64,
        window: i64,
        cutoff: i64,
        report_generation: u64,
        dropped_count: u64,
        straddler_count: u64,
        manifest_committed: bool,
    ) -> Self {
        Self {
            operation: IngestRetentionOperation::Retention,
            fault: IngestRetentionFaultKind::RetentionClockBoundary,
            checkpoint: IngestRetentionCheckpoint::RetentionPolicyEvaluated,
            cardinality: 1,
            invocation_id,
            effect: IngestRetentionFaultEffect::RetentionClockBoundary {
                supplied_now,
                window,
                cutoff,
                range_start: i64::MIN,
                range_end: cutoff,
                report_generation,
                dropped_count,
                straddler_count,
                manifest_committed,
            },
        }
    }

    pub(crate) fn purge_unlink_error(
        invocation_id: u64,
        original_segment: [u8; 16],
        replacement_segment: [u8; 16],
        old_file_name: String,
        replacement_manifest_committed: bool,
        intent_present: bool,
        old_path_linked: bool,
    ) -> Self {
        Self {
            operation: IngestRetentionOperation::Purge,
            fault: IngestRetentionFaultKind::PurgeUnlinkError,
            checkpoint: IngestRetentionCheckpoint::PurgeOldSegmentUnlinkError,
            cardinality: 1,
            invocation_id,
            effect: IngestRetentionFaultEffect::PurgeUnlinkError {
                original_segment,
                replacement_segment,
                old_file_name,
                io_kind: IngestRetentionIoKind::Other,
                replacement_manifest_committed,
                intent_present,
                old_path_linked,
            },
        }
    }

    pub(crate) fn purge_crash_boundary(
        invocation_id: u64,
        target_ids: Vec<u128>,
        token_id: u64,
    ) -> Self {
        Self {
            operation: IngestRetentionOperation::Purge,
            fault: IngestRetentionFaultKind::PurgeCrashBoundary,
            checkpoint: IngestRetentionCheckpoint::PurgeAfterDurableIntentBeforeRewrite,
            cardinality: 1,
            invocation_id,
            effect: IngestRetentionFaultEffect::PurgeCrashBoundary {
                target_ids,
                token_id,
                intent_file_name: "purge.ze".to_owned(),
                intent_durable: true,
                artifact_rewrites: 0,
                child_aborted: true,
            },
        }
    }

    /// Encodes the exact durable-intent crash receipt for a child-process sink.
    pub fn encode_purge_crash_test_evidence(&self) -> Result<Vec<u8>, String> {
        let IngestRetentionFaultEffect::PurgeCrashBoundary {
            target_ids,
            token_id,
            intent_file_name,
            intent_durable,
            artifact_rewrites,
            child_aborted,
        } = &self.effect
        else {
            return Err("receipt is not purge-crash-boundary evidence".to_owned());
        };
        if self.operation != IngestRetentionOperation::Purge
            || self.fault != IngestRetentionFaultKind::PurgeCrashBoundary
            || self.checkpoint != IngestRetentionCheckpoint::PurgeAfterDurableIntentBeforeRewrite
            || self.cardinality != 1
            || target_ids.is_empty()
        {
            return Err("purge-crash receipt header or target catalog is invalid".to_owned());
        }
        let target_count = u32::try_from(target_ids.len())
            .map_err(|_| "purge-crash target count exceeds u32".to_owned())?;
        let intent_length = u16::try_from(intent_file_name.len())
            .map_err(|_| "purge-crash intent name exceeds u16".to_owned())?;
        let mut bytes = Vec::with_capacity(
            55_usize
                .saturating_add(target_ids.len().saturating_mul(16))
                .saturating_add(intent_file_name.len()),
        );
        bytes.extend_from_slice(PURGE_CRASH_RECEIPT_MAGIC);
        bytes.extend_from_slice(&PURGE_CRASH_RECEIPT_VERSION.to_le_bytes());
        bytes.push(4); // Purge
        bytes.push(6); // PurgeCrashBoundary
        bytes.push(6); // PurgeAfterDurableIntentBeforeRewrite
        bytes.push(self.cardinality);
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&self.invocation_id.to_le_bytes());
        bytes.extend_from_slice(&token_id.to_le_bytes());
        bytes.extend_from_slice(&artifact_rewrites.to_le_bytes());
        bytes.push(u8::from(*intent_durable) | (u8::from(*child_aborted) << 1));
        bytes.extend_from_slice(&target_count.to_le_bytes());
        for target_id in target_ids {
            bytes.extend_from_slice(&target_id.to_le_bytes());
        }
        bytes.extend_from_slice(&intent_length.to_le_bytes());
        bytes.extend_from_slice(intent_file_name.as_bytes());
        let digest = xxh3_64(&bytes);
        bytes.extend_from_slice(&digest.to_le_bytes());
        Ok(bytes)
    }

    /// Decodes and validates a child-process purge-crash receipt.
    pub fn decode_purge_crash_test_evidence(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < 49 {
            return Err(format!(
                "purge-crash receipt is truncated: {} bytes",
                bytes.len()
            ));
        }
        let (encoded, digest_bytes) = bytes.split_at(bytes.len().saturating_sub(8));
        let digest = u64::from_le_bytes(
            digest_bytes
                .try_into()
                .map_err(|_| "purge-crash receipt digest is truncated".to_owned())?,
        );
        let expected_digest = xxh3_64(encoded);
        if digest != expected_digest {
            return Err(format!(
                "purge-crash receipt digest mismatch expected={expected_digest:016x} observed={digest:016x}"
            ));
        }
        let mut offset = 0_usize;
        let mut take = |length: usize| -> Result<&[u8], String> {
            let end = offset
                .checked_add(length)
                .ok_or_else(|| "purge-crash receipt offset overflows".to_owned())?;
            let value = encoded.get(offset..end).ok_or_else(|| {
                format!("purge-crash receipt truncated at {offset}, need {length}")
            })?;
            offset = end;
            Ok(value)
        };
        if take(8)? != PURGE_CRASH_RECEIPT_MAGIC {
            return Err("purge-crash receipt magic mismatch".to_owned());
        }
        let version = u16::from_le_bytes(
            take(2)?
                .try_into()
                .map_err(|_| "purge-crash version is truncated".to_owned())?,
        );
        if version != PURGE_CRASH_RECEIPT_VERSION {
            return Err(format!("unsupported purge-crash receipt version {version}"));
        }
        let operation = take(1)?[0];
        let fault = take(1)?[0];
        let checkpoint = take(1)?[0];
        let cardinality = take(1)?[0];
        let reserved = u16::from_le_bytes(
            take(2)?
                .try_into()
                .map_err(|_| "purge-crash reserved field is truncated".to_owned())?,
        );
        if (operation, fault, checkpoint, cardinality, reserved) != (4, 6, 6, 1, 0) {
            return Err(format!(
                "purge-crash receipt header mismatch operation={operation} fault={fault} checkpoint={checkpoint} cardinality={cardinality} reserved={reserved}"
            ));
        }
        let invocation_id = u64::from_le_bytes(
            take(8)?
                .try_into()
                .map_err(|_| "purge-crash invocation is truncated".to_owned())?,
        );
        let token_id = u64::from_le_bytes(
            take(8)?
                .try_into()
                .map_err(|_| "purge-crash token is truncated".to_owned())?,
        );
        let artifact_rewrites = u64::from_le_bytes(
            take(8)?
                .try_into()
                .map_err(|_| "purge-crash rewrite count is truncated".to_owned())?,
        );
        let flags = take(1)?[0];
        if flags & !0b11 != 0 {
            return Err(format!(
                "purge-crash receipt flags are invalid: {flags:#04x}"
            ));
        }
        let target_count = u32::from_le_bytes(
            take(4)?
                .try_into()
                .map_err(|_| "purge-crash target count is truncated".to_owned())?,
        );
        if target_count == 0 {
            return Err("purge-crash receipt target catalog is empty".to_owned());
        }
        let mut target_ids = Vec::with_capacity(
            usize::try_from(target_count)
                .map_err(|_| "purge-crash target count exceeds usize".to_owned())?,
        );
        for _ in 0..target_count {
            target_ids.push(u128::from_le_bytes(
                take(16)?
                    .try_into()
                    .map_err(|_| "purge-crash target id is truncated".to_owned())?,
            ));
        }
        let intent_length = u16::from_le_bytes(
            take(2)?
                .try_into()
                .map_err(|_| "purge-crash intent length is truncated".to_owned())?,
        );
        let intent_file_name = String::from_utf8(take(usize::from(intent_length))?.to_vec())
            .map_err(|error| format!("purge-crash intent name is not UTF-8: {error}"))?;
        if offset != encoded.len() {
            return Err(format!(
                "purge-crash receipt has {} trailing bytes",
                encoded.len().saturating_sub(offset)
            ));
        }
        let receipt = Self {
            operation: IngestRetentionOperation::Purge,
            fault: IngestRetentionFaultKind::PurgeCrashBoundary,
            checkpoint: IngestRetentionCheckpoint::PurgeAfterDurableIntentBeforeRewrite,
            cardinality,
            invocation_id,
            effect: IngestRetentionFaultEffect::PurgeCrashBoundary {
                target_ids,
                token_id,
                intent_file_name,
                intent_durable: flags & 0b01 != 0,
                artifact_rewrites,
                child_aborted: flags & 0b10 != 0,
            },
        };
        if receipt.encode_purge_crash_test_evidence()? != bytes {
            return Err("purge-crash receipt is not canonical".to_owned());
        }
        Ok(receipt)
    }

    pub(crate) fn write_purge_crash_test_evidence(&self, path: &Path) -> std::io::Result<()> {
        let bytes = self
            .encode_purge_crash_test_evidence()
            .map_err(std::io::Error::other)?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)?;
        file.write_all(&bytes)?;
        file.sync_all()
    }

    #[must_use]
    /// Returns the owning feature campaign key.
    pub const fn campaign(&self) -> &'static str {
        "ingest-retention"
    }

    #[must_use]
    /// Returns the exact public operation.
    pub const fn operation(&self) -> IngestRetentionOperation {
        self.operation
    }

    #[must_use]
    /// Returns the selected feature fault.
    pub const fn fault(&self) -> IngestRetentionFaultKind {
        self.fault
    }

    #[must_use]
    /// Returns the exact product checkpoint.
    pub const fn checkpoint(&self) -> IngestRetentionCheckpoint {
        self.checkpoint
    }

    #[must_use]
    /// Returns the one-shot receipt cardinality.
    pub const fn cardinality(&self) -> u8 {
        self.cardinality
    }

    #[must_use]
    /// Returns the generated program operation identity.
    pub const fn invocation_id(&self) -> u64 {
        self.invocation_id
    }

    #[must_use]
    /// Returns immutable product-observed effect facts.
    pub const fn effect(&self) -> &IngestRetentionFaultEffect {
        &self.effect
    }
}

#[derive(Debug)]
struct IngestRetentionFaultState {
    invocation_id: u64,
    armed: Option<IngestRetentionTestFault>,
    receipts: Vec<IngestRetentionFaultReceiptV1>,
    partial_append_observation: Option<PartialBatchAppendObservation>,
    purge_unlink_fired: bool,
    purge_crash_fired: bool,
}

/// Cloneable Store-owned controller for one deterministic test fault.
#[derive(Clone, Debug)]
pub struct IngestRetentionFaultController {
    state: Arc<Mutex<IngestRetentionFaultState>>,
}

impl IngestRetentionFaultController {
    /// Creates an unarmed controller for one operation invocation.
    #[must_use]
    pub fn new(invocation_id: u64) -> Self {
        Self {
            state: Arc::new(Mutex::new(IngestRetentionFaultState {
                invocation_id,
                armed: None,
                receipts: Vec::new(),
                partial_append_observation: None,
                purge_unlink_fired: false,
                purge_crash_fired: false,
            })),
        }
    }

    /// Arms the next matching public operation.
    pub fn arm(&self, fault: IngestRetentionTestFault) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "ingest-retention controller lock poisoned".to_owned())?;
        if state.armed.is_some() {
            return Err("ingest-retention controller already has an armed fault".to_owned());
        }
        state.armed = Some(fault);
        Ok(())
    }

    pub(crate) fn post_ack_retry_plan(&self) -> Result<Option<(u64, u64, u64)>, String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "ingest-retention controller lock poisoned".to_owned())?;
        Ok(state.armed.as_ref().and_then(|fault| match fault {
            IngestRetentionTestFault::PostAckRetry {
                first_ack_seq,
                first_ack_generation,
            } => Some((state.invocation_id, *first_ack_seq, *first_ack_generation)),
            IngestRetentionTestFault::PartialBatchAppend { .. } => None,
            IngestRetentionTestFault::SealCancellation => None,
            IngestRetentionTestFault::RetentionClockBoundary { .. } => None,
            IngestRetentionTestFault::PurgeUnlinkError { .. } => None,
            IngestRetentionTestFault::PurgeCrashBoundary { .. } => None,
        }))
    }

    pub(crate) fn partial_batch_append_plan(&self) -> Result<Option<(u64, usize)>, String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "ingest-retention controller lock poisoned".to_owned())?;
        Ok(match state.armed.as_ref() {
            Some(IngestRetentionTestFault::PartialBatchAppend { prefix_bytes }) => {
                Some((state.invocation_id, *prefix_bytes))
            }
            Some(IngestRetentionTestFault::PostAckRetry { .. }) | None => None,
            Some(IngestRetentionTestFault::SealCancellation) => None,
            Some(IngestRetentionTestFault::RetentionClockBoundary { .. }) => None,
            Some(IngestRetentionTestFault::PurgeUnlinkError { .. }) => None,
            Some(IngestRetentionTestFault::PurgeCrashBoundary { .. }) => None,
        })
    }

    pub(crate) fn seal_cancellation_plan(&self) -> Result<Option<u64>, String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "ingest-retention controller lock poisoned".to_owned())?;
        Ok(matches!(
            state.armed.as_ref(),
            Some(IngestRetentionTestFault::SealCancellation)
        )
        .then_some(state.invocation_id))
    }

    pub(crate) fn retention_clock_boundary_plan(
        &self,
        now: i64,
        window: i64,
        cutoff: i64,
    ) -> Result<Option<u64>, String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "ingest-retention controller lock poisoned".to_owned())?;
        let Some(IngestRetentionTestFault::RetentionClockBoundary {
            now: expected_now,
            window: expected_window,
            expected_cutoff,
        }) = state.armed.as_ref()
        else {
            return Ok(None);
        };
        if (now, window, cutoff) != (*expected_now, *expected_window, *expected_cutoff) {
            return Err(format!(
                "retention-clock-boundary plan mismatch expected=({expected_now},{expected_window},{expected_cutoff}) observed=({now},{window},{cutoff})"
            ));
        }
        Ok(Some(state.invocation_id))
    }

    pub(crate) fn purge_unlink_error_plan(
        &self,
        original_segment: [u8; 16],
    ) -> Result<Option<u64>, String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "ingest-retention controller lock poisoned".to_owned())?;
        let Some(IngestRetentionTestFault::PurgeUnlinkError { target_segment }) =
            state.armed.as_ref()
        else {
            return Ok(None);
        };
        if *target_segment != original_segment {
            return Err(format!(
                "purge-unlink-error target mismatch expected={target_segment:02x?} observed={original_segment:02x?}"
            ));
        }
        Ok(Some(state.invocation_id))
    }

    pub(crate) fn take_purge_crash_boundary_plan(&self) -> Result<Option<(u64, PathBuf)>, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "ingest-retention controller lock poisoned".to_owned())?;
        let Some(IngestRetentionTestFault::PurgeCrashBoundary {
            checkpoint,
            receipt_sink,
        }) = state.armed.as_ref()
        else {
            return Ok(None);
        };
        if *checkpoint != IngestRetentionPurgeCrashCheckpoint::AfterDurableIntentBeforeRewrite {
            return Err("unsupported purge-crash checkpoint".to_owned());
        }
        if state.purge_crash_fired {
            return Ok(None);
        }
        let receipt_sink = receipt_sink.clone();
        state.purge_crash_fired = true;
        Ok(Some((state.invocation_id, receipt_sink)))
    }

    fn begin_purge_unlink_error(&self, path: &Path) -> std::io::Result<Option<String>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| std::io::Error::other("ingest-retention controller lock poisoned"))?;
        let Some(IngestRetentionTestFault::PurgeUnlinkError { target_segment }) =
            state.armed.as_ref()
        else {
            return Ok(None);
        };
        let target_segment = *target_segment;
        if state.purge_unlink_fired {
            return Ok(None);
        }
        let expected = crate::segment::SegmentId::from_bytes(target_segment).file_name();
        if path
            .file_name()
            .is_none_or(|name| name != expected.as_str())
        {
            return Ok(None);
        }
        state.purge_unlink_fired = true;
        Ok(Some(format!("injected purge-unlink-error for {expected}")))
    }

    fn begin_partial_batch_append(
        &self,
        encoded_bytes: usize,
    ) -> std::io::Result<Option<PartialBatchAppendObservation>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| std::io::Error::other("ingest-retention controller lock poisoned"))?;
        let Some(IngestRetentionTestFault::PartialBatchAppend { prefix_bytes }) =
            state.armed.as_ref()
        else {
            return Ok(None);
        };
        let prefix_bytes = *prefix_bytes;
        if state.partial_append_observation.is_some() {
            return Ok(None);
        }
        if prefix_bytes == 0 || prefix_bytes >= encoded_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "partial-batch-append prefix {prefix_bytes} is not within 1..{encoded_bytes}"
                ),
            ));
        }
        let detail =
            format!("injected partial-batch-append after {prefix_bytes}/{encoded_bytes} bytes");
        let observation = PartialBatchAppendObservation {
            invocation_id: state.invocation_id,
            prefix_bytes,
            encoded_bytes,
            detail,
        };
        state.partial_append_observation = Some(observation.clone());
        Ok(Some(observation))
    }

    pub(crate) fn take_partial_batch_append_observation(
        &self,
    ) -> Result<Option<PartialBatchAppendObservation>, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "ingest-retention controller lock poisoned".to_owned())?;
        Ok(state.partial_append_observation.take())
    }

    pub(crate) fn push_receipt(
        &self,
        receipt: IngestRetentionFaultReceiptV1,
    ) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "ingest-retention controller lock poisoned".to_owned())?;
        state.armed = None;
        state.receipts.push(receipt);
        Ok(())
    }

    /// Drains Store-owned receipts without exposing a construction seam.
    pub fn take_receipts(&self) -> Result<Vec<IngestRetentionFaultReceiptV1>, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "ingest-retention controller lock poisoned".to_owned())?;
        Ok(std::mem::take(&mut state.receipts))
    }
}

/// VFS decorator that performs one exact, visible partial WAL group append.
#[derive(Clone)]
pub struct PartialBatchAppendVfs {
    inner: Arc<dyn Vfs>,
    controller: IngestRetentionFaultController,
}

impl PartialBatchAppendVfs {
    /// Binds the deterministic partial-append controller to one filesystem.
    #[must_use]
    pub fn new(inner: Arc<dyn Vfs>, controller: IngestRetentionFaultController) -> Self {
        Self { inner, controller }
    }
}

struct PartialBatchAppendFile {
    inner: Box<dyn VfsFile>,
    controller: IngestRetentionFaultController,
}

impl VfsFile for PartialBatchAppendFile {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.inner.append(bytes)
    }

    fn append_vectored(&mut self, buffers: &mut [IoSlice<'_>]) -> std::io::Result<()> {
        let encoded_bytes = buffers.iter().try_fold(0_usize, |total, buffer| {
            total.checked_add(buffer.len()).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "partial-batch-append encoded length overflow",
                )
            })
        })?;
        let Some(observation) = self.controller.begin_partial_batch_append(encoded_bytes)? else {
            return self.inner.append_vectored(buffers);
        };
        let mut remaining = observation.prefix_bytes;
        for buffer in buffers.iter() {
            if remaining == 0 {
                break;
            }
            let take = remaining.min(buffer.len());
            let prefix = buffer.get(..take).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "partial-batch-append prefix exceeds buffer",
                )
            })?;
            self.inner.append(prefix)?;
            remaining = remaining.saturating_sub(take);
        }
        if remaining != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "partial-batch-append prefix exceeds encoded group",
            ));
        }
        Err(std::io::Error::other(observation.detail))
    }

    fn sync(&self, kind: SyncKind) -> std::io::Result<()> {
        self.inner.sync(kind)
    }
}

impl Vfs for PartialBatchAppendVfs {
    fn segment_data_read_counter(&self) -> Option<Arc<AtomicU64>> {
        self.inner.segment_data_read_counter()
    }

    fn open(&self, path: &Path) -> std::io::Result<u64> {
        self.inner.open(path)
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        self.inner.read(path)
    }

    fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        self.inner.read_range(path, offset, length)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.inner.write(path, bytes)
    }

    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        Ok(Box::new(PartialBatchAppendFile {
            inner: self.inner.open_append(path)?,
            controller: self.controller.clone(),
        }))
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.inner.rename(from, to)
    }

    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        self.inner.sync(path, kind)
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        self.inner.list(directory)
    }

    fn delete(&self, path: &Path) -> std::io::Result<()> {
        self.inner.delete(path)
    }
}

/// VFS decorator that fails one exact old-segment unlink after purge publication.
#[derive(Clone)]
pub struct PurgeUnlinkErrorVfs {
    inner: Arc<dyn Vfs>,
    controller: IngestRetentionFaultController,
}

impl PurgeUnlinkErrorVfs {
    /// Binds the deterministic purge-unlink controller to one filesystem.
    #[must_use]
    pub fn new(inner: Arc<dyn Vfs>, controller: IngestRetentionFaultController) -> Self {
        Self { inner, controller }
    }
}

impl Vfs for PurgeUnlinkErrorVfs {
    fn segment_data_read_counter(&self) -> Option<Arc<AtomicU64>> {
        self.inner.segment_data_read_counter()
    }

    fn open(&self, path: &Path) -> std::io::Result<u64> {
        self.inner.open(path)
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        self.inner.read(path)
    }

    fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        self.inner.read_range(path, offset, length)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.inner.write(path, bytes)
    }

    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        self.inner.open_append(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.inner.rename(from, to)
    }

    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        self.inner.sync(path, kind)
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        self.inner.list(directory)
    }

    fn delete(&self, path: &Path) -> std::io::Result<()> {
        if let Some(detail) = self.controller.begin_purge_unlink_error(path)? {
            return Err(std::io::Error::other(detail));
        }
        self.inner.delete(path)
    }
}
