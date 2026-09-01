//! Family-owned ingest/retention fixtures and public Store observation adapters.
//!
//! This module never constructs shared oracle records or feature receipts. It
//! derives primitive fixtures, executes public Store operations, and returns
//! independent-oracle DTOs. The shared runner owns dispatch and serialization.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
#[cfg(unix)]
use std::process::Command;

use tempfile::tempdir;
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, IngestError,
    IngestRetentionCheckpoint, IngestRetentionFaultController, IngestRetentionFaultEffect,
    IngestRetentionFaultKind, IngestRetentionFaultReceiptV1, IngestRetentionIoKind,
    IngestRetentionOperation, IngestRetentionPurgeCrashCheckpoint, IngestRetentionTestFault,
    PartialBatchAppendVfs, PurgeError, PurgeUnlinkErrorVfs, RetentionPolicy, Revision,
    SearchRequest,
};
use zeppelin_embed::lifecycle::{
    CancelToken, OpenOptions, QueryControl, SearchOptions, SearchTier, Store, StoreError,
    StoreTestDependencies, SystemMonotonicClock,
};
use zeppelin_embed::meta::{
    Predicate, PredicateValue, RangeBound, RangePredicate, TIMESTAMP_COLUMN,
};
use zeppelin_embed::scan::ScanOptions;
use zeppelin_embed::vfs::StdVfs;
use zeppelin_embed::wal::WalWriteError;
use zeppelin_embed_adversarial_oracle::ingest_retention as independent;

const FIXTURE_NAMESPACE: &str = "adversarial::ingest-retention";
const QUERY: [f32; 2] = [1.0, 1.0];
#[cfg(unix)]
const SIGABRT_SIGNAL: i32 = 6;

/// One ingest campaign operation, kept independent from shared dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IngestOperationKind {
    BatchCommit,
    Seal,
    Retention,
    Purge,
}

impl IngestOperationKind {
    pub const fn key(self) -> &'static str {
        match self {
            Self::BatchCommit => "batch-commit",
            Self::Seal => "seal",
            Self::Retention => "retention",
            Self::Purge => "purge",
        }
    }
}

#[must_use]
pub fn ingest_case_identity(
    operation: IngestOperationKind,
    fault: Option<IngestFaultKind>,
    invocation_id: u64,
) -> String {
    format!(
        "operation={};fault={};invocation={invocation_id}",
        operation.key(),
        fault.map_or("clean", IngestFaultKind::key),
    )
}

/// One catalogued ingest-retention feature fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IngestFaultKind {
    PostAckRetry,
    PartialBatchAppend,
    SealCancellation,
    RetentionClockBoundary,
    PurgeUnlinkError,
    PurgeCrashBoundary,
}

impl IngestFaultKind {
    pub const fn key(self) -> &'static str {
        match self {
            Self::PostAckRetry => "post-ack-retry",
            Self::PartialBatchAppend => "partial-batch-append",
            Self::SealCancellation => "seal-cancellation",
            Self::RetentionClockBoundary => "retention-clock-boundary",
            Self::PurgeUnlinkError => "purge-unlink-error",
            Self::PurgeCrashBoundary => "purge-crash-boundary",
        }
    }
}

/// Primitive document materialization retained independently from Store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocumentInput {
    pub doc_id: u128,
    pub revision: u64,
    pub timestamp: i64,
    pub vector_bits: Vec<u32>,
    pub metadata_sentinel: Vec<u8>,
    pub text: Vec<u8>,
}

/// Primitive I20 fixture generated before a Store is opened.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I20FixtureV1 {
    pub namespace: &'static str,
    pub seed: u64,
    pub fixture_version: u32,
    pub sealed_baseline: Vec<DocumentInput>,
    pub active_baseline: Vec<DocumentInput>,
    pub submitted: Vec<DocumentInput>,
    pub timestamp_thresholds: Vec<i64>,
    pub partial_append_prefix_bytes: usize,
}

/// Primitive I21 lifecycle fixture generated before a Store is opened.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I21FixtureV1 {
    pub namespace: &'static str,
    pub seed: u64,
    pub fixture_version: u32,
    pub initial: Vec<DocumentInput>,
    pub upserts: Vec<DocumentInput>,
    pub deleted_doc_ids: Vec<u128>,
    pub timestamp_thresholds: Vec<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I22PartitionFixture {
    pub label: String,
    pub documents: Vec<DocumentInput>,
}

/// Primitive I22 lifecycle fixture generated before a Store is opened.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I22FixtureV1 {
    pub namespace: &'static str,
    pub seed: u64,
    pub fixture_version: u32,
    pub clock_now: i64,
    pub retention_window: i64,
    pub expected_cutoff: i64,
    pub partitions: Vec<I22PartitionFixture>,
    pub active_control: DocumentInput,
    pub timestamp_thresholds: Vec<i64>,
}

/// Primitive I23 purge fixture generated before a Store is opened.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I23FixtureV1 {
    pub namespace: &'static str,
    pub seed: u64,
    pub fixture_version: u32,
    pub target: DocumentInput,
    pub survivor: DocumentInput,
    pub target_location: independent::I23TargetLocation,
    pub sentinel_patterns: Vec<Vec<u8>>,
    pub timestamp_thresholds: Vec<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I22SegmentLabel {
    pub label: String,
    pub segment_id: [u8; 16],
}

/// One clean I20 comparison returned to shared dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I20Evidence {
    pub fixture: I20FixtureV1,
    pub expected: independent::I20Expected,
    pub observed: independent::I20Observed,
    pub initial_directory: IngestDirectoryEvidence,
    pub control: Option<I20ControlEvidence>,
    pub receipts: Vec<IngestRetentionFaultReceiptV1>,
}

/// One exact I21 seal comparison returned to shared dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I21Evidence {
    pub fixture: I21FixtureV1,
    pub expected: independent::I21Expected,
    pub observed: independent::I21Observed,
    pub initial_directory: IngestDirectoryEvidence,
    pub control: Option<I21ControlEvidence>,
    pub receipts: Vec<IngestRetentionFaultReceiptV1>,
}

/// One exact public retention-boundary comparison.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I22Evidence {
    pub fixture: I22FixtureV1,
    pub expected: independent::I22Expected,
    pub observed: independent::I22Observed,
    pub segment_labels: Vec<I22SegmentLabel>,
    pub initial_directory: IngestDirectoryEvidence,
    pub control: Option<I22ControlEvidence>,
    pub receipts: Vec<IngestRetentionFaultReceiptV1>,
}

/// One exact public physical-purge comparison.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I23Evidence {
    pub fixture: I23FixtureV1,
    pub expected: independent::I23Expected,
    pub observed: independent::I23Observed,
    pub initial_directory: IngestDirectoryEvidence,
    pub control: Option<I23ControlEvidence>,
    pub receipts: Vec<IngestRetentionFaultReceiptV1>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I23ControlEvidence {
    pub clean_initial_directory: IngestDirectoryEvidence,
    pub fault_initial_directory: IngestDirectoryEvidence,
    pub isolated_directories: bool,
    pub clean_final: Vec<independent::DocumentFact>,
    pub fault_final: Vec<independent::DocumentFact>,
    pub passed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I22ControlEvidence {
    pub clean_initial_directory: IngestDirectoryEvidence,
    pub fault_initial_directory: IngestDirectoryEvidence,
    pub isolated_directories: bool,
    pub clean_final: Vec<independent::DocumentFact>,
    pub fault_final: Vec<independent::DocumentFact>,
    pub passed: bool,
}

/// Exact clean/cancelled relation for the I21 late-cancellation fault.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I21ControlEvidence {
    pub clean_initial_directory: IngestDirectoryEvidence,
    pub fault_initial_directory: IngestDirectoryEvidence,
    pub isolated_directories: bool,
    pub clean_final: Vec<independent::DocumentFact>,
    pub fault_final: Vec<independent::DocumentFact>,
    pub passed: bool,
}

/// One canonical file fact captured before the selected operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IngestFileFact {
    pub relative_path: String,
    pub byte_length: u64,
    pub digest: u64,
}

/// Directory-independent proof of one materialized operation fixture.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IngestDirectoryEvidence {
    pub digest: u64,
    pub files: Vec<IngestFileFact>,
}

/// Exact clean/fault relation for an I20 selected fault.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I20ControlEvidence {
    pub clean_initial_directory: IngestDirectoryEvidence,
    pub fault_initial_directory: IngestDirectoryEvidence,
    pub isolated_directories: bool,
    pub clean_final: Vec<independent::DocumentFact>,
    pub fault_final: Vec<independent::DocumentFact>,
    pub passed: bool,
}

/// Operation-scoped evidence. Later vertical slices add the remaining exact
/// invariant variants without changing the BatchCommit seam.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IngestOperationEvidence {
    I20(I20Evidence),
    I21(I21Evidence),
    I22(I22Evidence),
    I23(I23Evidence),
}

pub const INGEST_RETAINED_FIXTURE_SCHEMA: &str = "zeppelin-ingest-retention-fixture-v1";
const INGEST_RETAINED_FIXTURE_MAGIC: &[u8; 8] = b"ZEINGF01";
const INGEST_RETAINED_FIXTURE_VERSION: u16 = 1;
const INGEST_RETAINED_FIXTURE_MAX_ITEMS: usize = 1 << 16;
const INGEST_RETAINED_FIXTURE_MAX_BYTES: usize = 1 << 24;

/// Literal primitive subfixture retained for product replay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RetainedIngestFixtureV1 {
    I20(I20FixtureV1),
    I21(I21FixtureV1),
    I22(I22FixtureV1),
    I23(I23FixtureV1),
}

/// Operation identity and primitive materialization consumed by replay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetainedIngestOperationV1 {
    pub operation: IngestOperationKind,
    pub fault: Option<IngestFaultKind>,
    pub invocation_id: u64,
    pub fixture: RetainedIngestFixtureV1,
}

impl RetainedIngestOperationV1 {
    pub fn from_evidence(
        operation: IngestOperationKind,
        fault: Option<IngestFaultKind>,
        invocation_id: u64,
        evidence: &IngestOperationEvidence,
    ) -> Result<Self, String> {
        let fixture = match (operation, evidence) {
            (IngestOperationKind::BatchCommit, IngestOperationEvidence::I20(evidence)) => {
                RetainedIngestFixtureV1::I20(evidence.fixture.clone())
            }
            (IngestOperationKind::Seal, IngestOperationEvidence::I21(evidence)) => {
                RetainedIngestFixtureV1::I21(evidence.fixture.clone())
            }
            (IngestOperationKind::Retention, IngestOperationEvidence::I22(evidence)) => {
                RetainedIngestFixtureV1::I22(evidence.fixture.clone())
            }
            (IngestOperationKind::Purge, IngestOperationEvidence::I23(evidence)) => {
                RetainedIngestFixtureV1::I23(evidence.fixture.clone())
            }
            _ => {
                return Err(
                    "retained ingest operation and evidence invariant do not match".to_owned(),
                );
            }
        };
        validate_operation_fault(operation, fault)?;
        validate_retained_fixture(operation, &fixture)?;
        Ok(Self {
            operation,
            fault,
            invocation_id,
            fixture,
        })
    }
}

pub fn encode_ingest_fixture(fixture: &RetainedIngestOperationV1) -> Result<Vec<u8>, String> {
    validate_operation_fault(fixture.operation, fixture.fault)?;
    validate_retained_fixture(fixture.operation, &fixture.fixture)?;
    let mut payload = Vec::new();
    put_u8(&mut payload, ingest_operation_tag(fixture.operation));
    put_u8(&mut payload, fixture.fault.map_or(0, ingest_fault_tag));
    put_u64(&mut payload, fixture.invocation_id);
    match &fixture.fixture {
        RetainedIngestFixtureV1::I20(value) => {
            put_u8(&mut payload, 1);
            encode_i20_fixture(&mut payload, value)?;
        }
        RetainedIngestFixtureV1::I21(value) => {
            put_u8(&mut payload, 2);
            encode_i21_fixture(&mut payload, value)?;
        }
        RetainedIngestFixtureV1::I22(value) => {
            put_u8(&mut payload, 3);
            encode_i22_fixture(&mut payload, value)?;
        }
        RetainedIngestFixtureV1::I23(value) => {
            put_u8(&mut payload, 4);
            encode_i23_fixture(&mut payload, value)?;
        }
    }
    let payload_len = u32::try_from(payload.len())
        .map_err(|_| "retained ingest fixture payload exceeds u32".to_owned())?;
    let mut encoded = Vec::with_capacity(24_usize.saturating_add(payload.len()));
    encoded.extend_from_slice(INGEST_RETAINED_FIXTURE_MAGIC);
    encoded.extend_from_slice(&INGEST_RETAINED_FIXTURE_VERSION.to_le_bytes());
    encoded.extend_from_slice(&0_u16.to_le_bytes());
    encoded.extend_from_slice(&payload_len.to_le_bytes());
    encoded.extend_from_slice(&fnv1a64(&payload).to_le_bytes());
    encoded.extend_from_slice(&payload);
    Ok(encoded)
}

pub fn decode_ingest_fixture(bytes: &[u8]) -> Result<RetainedIngestOperationV1, String> {
    let mut envelope = RetainedDecoder::new(bytes);
    if envelope.take(8)? != INGEST_RETAINED_FIXTURE_MAGIC {
        return Err("retained ingest fixture magic differs".to_owned());
    }
    let version = envelope.u16()?;
    if version != INGEST_RETAINED_FIXTURE_VERSION {
        return Err(format!(
            "retained ingest fixture version differs: {version}"
        ));
    }
    let flags = envelope.u16()?;
    if flags != 0 {
        return Err(format!(
            "retained ingest fixture flags are nonzero: {flags}"
        ));
    }
    let payload_len = usize::try_from(envelope.u32()?)
        .map_err(|_| "retained ingest fixture length exceeds usize".to_owned())?;
    if payload_len > INGEST_RETAINED_FIXTURE_MAX_BYTES {
        return Err("retained ingest fixture payload exceeds the closed bound".to_owned());
    }
    let expected_digest = envelope.u64()?;
    let payload = envelope.take(payload_len)?;
    envelope.finish()?;
    let observed_digest = fnv1a64(payload);
    if observed_digest != expected_digest {
        return Err(format!(
            "retained ingest fixture checksum differs expected={expected_digest:016x} observed={observed_digest:016x}"
        ));
    }
    let mut decoder = RetainedDecoder::new(payload);
    let operation = ingest_operation_from_tag(decoder.u8()?)?;
    let fault = match decoder.u8()? {
        0 => None,
        tag => Some(ingest_fault_from_tag(tag)?),
    };
    let invocation_id = decoder.u64()?;
    let fixture = match decoder.u8()? {
        1 => RetainedIngestFixtureV1::I20(decode_i20_fixture(&mut decoder)?),
        2 => RetainedIngestFixtureV1::I21(decode_i21_fixture(&mut decoder)?),
        3 => RetainedIngestFixtureV1::I22(decode_i22_fixture(&mut decoder)?),
        4 => RetainedIngestFixtureV1::I23(decode_i23_fixture(&mut decoder)?),
        tag => {
            return Err(format!(
                "retained ingest fixture variant tag is invalid: {tag}"
            ));
        }
    };
    decoder.finish()?;
    validate_operation_fault(operation, fault)?;
    validate_retained_fixture(operation, &fixture)?;
    let decoded = RetainedIngestOperationV1 {
        operation,
        fault,
        invocation_id,
        fixture,
    };
    if encode_ingest_fixture(&decoded)? != bytes {
        return Err("retained ingest fixture is not canonically encoded".to_owned());
    }
    Ok(decoded)
}

pub fn run_ingest_operation_from_fixture(bytes: &[u8]) -> Result<IngestOperationEvidence, String> {
    let retained = decode_ingest_fixture(bytes)?;
    match (retained.operation, retained.fault, retained.fixture) {
        (IngestOperationKind::BatchCommit, None, RetainedIngestFixtureV1::I20(fixture)) => {
            run_i20_clean_from_fixture(fixture).map(IngestOperationEvidence::I20)
        }
        (
            IngestOperationKind::BatchCommit,
            Some(IngestFaultKind::PostAckRetry),
            RetainedIngestFixtureV1::I20(fixture),
        ) => run_i20_post_ack_retry_from_fixture(fixture, retained.invocation_id)
            .map(IngestOperationEvidence::I20),
        (
            IngestOperationKind::BatchCommit,
            Some(IngestFaultKind::PartialBatchAppend),
            RetainedIngestFixtureV1::I20(fixture),
        ) => run_i20_partial_batch_append_from_fixture(fixture, retained.invocation_id)
            .map(IngestOperationEvidence::I20),
        (IngestOperationKind::Seal, None, RetainedIngestFixtureV1::I21(fixture)) => {
            run_i21_clean_from_fixture(fixture).map(IngestOperationEvidence::I21)
        }
        (
            IngestOperationKind::Seal,
            Some(IngestFaultKind::SealCancellation),
            RetainedIngestFixtureV1::I21(fixture),
        ) => run_i21_seal_cancellation_from_fixture(fixture, retained.invocation_id)
            .map(IngestOperationEvidence::I21),
        (IngestOperationKind::Retention, None, RetainedIngestFixtureV1::I22(fixture)) => {
            run_i22_clean_from_fixture(fixture).map(IngestOperationEvidence::I22)
        }
        (
            IngestOperationKind::Retention,
            Some(IngestFaultKind::RetentionClockBoundary),
            RetainedIngestFixtureV1::I22(fixture),
        ) => run_i22_retention_clock_boundary_from_fixture(fixture, retained.invocation_id)
            .map(IngestOperationEvidence::I22),
        (IngestOperationKind::Purge, None, RetainedIngestFixtureV1::I23(fixture)) => {
            run_i23_clean_from_fixture(fixture).map(IngestOperationEvidence::I23)
        }
        (
            IngestOperationKind::Purge,
            Some(IngestFaultKind::PurgeUnlinkError),
            RetainedIngestFixtureV1::I23(fixture),
        ) => run_i23_purge_unlink_error_from_fixture(fixture, retained.invocation_id)
            .map(IngestOperationEvidence::I23),
        (
            IngestOperationKind::Purge,
            Some(IngestFaultKind::PurgeCrashBoundary),
            RetainedIngestFixtureV1::I23(fixture),
        ) => run_i23_purge_crash_boundary_from_fixture(fixture, retained.invocation_id)
            .map(IngestOperationEvidence::I23),
        (operation, fault, _) => Err(format!(
            "retained ingest fixture operation/fault/variant mismatch {operation:?}/{fault:?}"
        )),
    }
}

pub fn retained_ingest_fixture_record_json(
    retained: &RetainedIngestOperationV1,
) -> Result<String, String> {
    let bytes = encode_ingest_fixture(retained)?;
    Ok(format!(
        "{{\"campaign\":\"ingest-retention\",\"schema\":\"{}\",\"operation\":\"{}\",\"seed\":{},\"fault\":{},\"invocation_id\":{},\"case_identity\":\"{}\",\"retained_fixture_bytes\":{},\"retained_fixture_digest\":\"{:016x}\",\"retained_fixture_hex\":\"{}\"}}",
        INGEST_RETAINED_FIXTURE_SCHEMA,
        retained.operation.key(),
        retained_fixture_seed(&retained.fixture),
        retained
            .fault
            .map_or_else(|| "null".to_owned(), |fault| format!("\"{}\"", fault.key())),
        retained.invocation_id,
        ingest_case_identity(retained.operation, retained.fault, retained.invocation_id),
        bytes.len(),
        fnv1a64(&bytes),
        hex_bytes(&bytes),
    ))
}

pub fn retained_ingest_observation_json(
    retained: &RetainedIngestOperationV1,
    evidence: &IngestOperationEvidence,
) -> Result<String, String> {
    let (attestation, control_present, receipt_count) = match (retained.operation, evidence) {
        (IngestOperationKind::BatchCommit, IngestOperationEvidence::I20(evidence)) => (
            independent::attest_i20(&evidence.expected, &evidence.observed),
            evidence.control.is_some(),
            evidence.receipts.len(),
        ),
        (IngestOperationKind::Seal, IngestOperationEvidence::I21(evidence)) => (
            independent::attest_i21(&evidence.expected, &evidence.observed),
            evidence.control.is_some(),
            evidence.receipts.len(),
        ),
        (IngestOperationKind::Retention, IngestOperationEvidence::I22(evidence)) => (
            independent::attest_i22(&evidence.expected, &evidence.observed),
            evidence.control.is_some(),
            evidence.receipts.len(),
        ),
        (IngestOperationKind::Purge, IngestOperationEvidence::I23(evidence)) => (
            independent::attest_i23(&evidence.expected, &evidence.observed),
            evidence.control.is_some(),
            evidence.receipts.len(),
        ),
        _ => {
            return Err(
                "retained ingest observation operation and evidence do not match".to_owned(),
            );
        }
    };
    if let Some(difference) = &attestation.first_difference {
        return Err(format!(
            "retained ingest observation failed {} at {}: {}",
            difference.checker_id, difference.path, difference.observed
        ));
    }
    Ok(format!(
        "{{\"campaign\":\"ingest-retention\",\"operation\":\"{}\",\"seed\":{},\"fault\":{},\"invocation_id\":{},\"case_identity\":\"{}\",\"checker_id\":\"{}\",\"canonical_version\":{},\"oracle_input_digest\":\"ingest-v{}:{:016x}\",\"oracle_observed_digest\":\"ingest-v{}:{:016x}\",\"oracle_input_bytes\":\"{}\",\"oracle_observed_bytes\":\"{}\",\"control_present\":{control_present},\"receipt_count\":{receipt_count},\"passed\":true}}",
        retained.operation.key(),
        retained_fixture_seed(&retained.fixture),
        retained
            .fault
            .map_or_else(|| "null".to_owned(), |fault| format!("\"{}\"", fault.key())),
        retained.invocation_id,
        ingest_case_identity(retained.operation, retained.fault, retained.invocation_id),
        attestation.checker_id,
        attestation.canonical_version,
        attestation.canonical_version,
        attestation.input_digest,
        attestation.canonical_version,
        attestation.observed_digest,
        hex_bytes(&attestation.input_bytes),
        hex_bytes(&attestation.observed_bytes),
    ))
}

fn retained_fixture_seed(fixture: &RetainedIngestFixtureV1) -> u64 {
    match fixture {
        RetainedIngestFixtureV1::I20(fixture) => fixture.seed,
        RetainedIngestFixtureV1::I21(fixture) => fixture.seed,
        RetainedIngestFixtureV1::I22(fixture) => fixture.seed,
        RetainedIngestFixtureV1::I23(fixture) => fixture.seed,
    }
}

fn json_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn validate_operation_fault(
    operation: IngestOperationKind,
    fault: Option<IngestFaultKind>,
) -> Result<(), String> {
    let legal = matches!(
        (operation, fault),
        (IngestOperationKind::BatchCommit, None)
            | (
                IngestOperationKind::BatchCommit,
                Some(IngestFaultKind::PostAckRetry | IngestFaultKind::PartialBatchAppend)
            )
            | (
                IngestOperationKind::Seal,
                None | Some(IngestFaultKind::SealCancellation)
            )
            | (
                IngestOperationKind::Retention,
                None | Some(IngestFaultKind::RetentionClockBoundary)
            )
            | (
                IngestOperationKind::Purge,
                None | Some(
                    IngestFaultKind::PurgeUnlinkError | IngestFaultKind::PurgeCrashBoundary
                )
            )
    );
    if legal {
        Ok(())
    } else {
        Err(format!(
            "retained ingest fault {fault:?} is incompatible with operation {operation:?}"
        ))
    }
}

fn validate_retained_fixture(
    operation: IngestOperationKind,
    fixture: &RetainedIngestFixtureV1,
) -> Result<(), String> {
    let valid_variant = matches!(
        (operation, fixture),
        (
            IngestOperationKind::BatchCommit,
            RetainedIngestFixtureV1::I20(_)
        ) | (IngestOperationKind::Seal, RetainedIngestFixtureV1::I21(_))
            | (
                IngestOperationKind::Retention,
                RetainedIngestFixtureV1::I22(_)
            )
            | (IngestOperationKind::Purge, RetainedIngestFixtureV1::I23(_))
    );
    if !valid_variant {
        return Err("retained ingest operation and fixture variant do not match".to_owned());
    }
    let (namespace, version, documents, thresholds): (&str, u32, Vec<&DocumentInput>, &[i64]) =
        match fixture {
            RetainedIngestFixtureV1::I20(value) => (
                value.namespace,
                value.fixture_version,
                value
                    .sealed_baseline
                    .iter()
                    .chain(&value.active_baseline)
                    .chain(&value.submitted)
                    .collect(),
                &value.timestamp_thresholds,
            ),
            RetainedIngestFixtureV1::I21(value) => (
                value.namespace,
                value.fixture_version,
                value.initial.iter().chain(&value.upserts).collect(),
                &value.timestamp_thresholds,
            ),
            RetainedIngestFixtureV1::I22(value) => (
                value.namespace,
                value.fixture_version,
                value
                    .partitions
                    .iter()
                    .flat_map(|partition| &partition.documents)
                    .chain(std::iter::once(&value.active_control))
                    .collect(),
                &value.timestamp_thresholds,
            ),
            RetainedIngestFixtureV1::I23(value) => (
                value.namespace,
                value.fixture_version,
                vec![&value.target, &value.survivor],
                &value.timestamp_thresholds,
            ),
        };
    if namespace != FIXTURE_NAMESPACE || version != 1 {
        return Err("retained ingest fixture namespace/version differs".to_owned());
    }
    if documents.is_empty()
        || documents.len() > INGEST_RETAINED_FIXTURE_MAX_ITEMS
        || thresholds.len() > INGEST_RETAINED_FIXTURE_MAX_ITEMS
        || thresholds.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err("retained ingest fixture cardinality/order is invalid".to_owned());
    }
    for document in documents {
        if document.revision == 0
            || document.vector_bits.len() != QUERY.len()
            || document.metadata_sentinel.len() > INGEST_RETAINED_FIXTURE_MAX_BYTES
            || document.text.len() > INGEST_RETAINED_FIXTURE_MAX_BYTES
            || String::from_utf8(document.text.clone()).is_err()
        {
            return Err(format!(
                "retained ingest document {} is invalid",
                document.doc_id
            ));
        }
    }
    Ok(())
}

fn ingest_operation_tag(operation: IngestOperationKind) -> u8 {
    match operation {
        IngestOperationKind::BatchCommit => 1,
        IngestOperationKind::Seal => 2,
        IngestOperationKind::Retention => 3,
        IngestOperationKind::Purge => 4,
    }
}

fn ingest_operation_from_tag(tag: u8) -> Result<IngestOperationKind, String> {
    match tag {
        1 => Ok(IngestOperationKind::BatchCommit),
        2 => Ok(IngestOperationKind::Seal),
        3 => Ok(IngestOperationKind::Retention),
        4 => Ok(IngestOperationKind::Purge),
        _ => Err(format!("retained ingest operation tag is invalid: {tag}")),
    }
}

fn ingest_fault_tag(fault: IngestFaultKind) -> u8 {
    match fault {
        IngestFaultKind::PostAckRetry => 1,
        IngestFaultKind::PartialBatchAppend => 2,
        IngestFaultKind::SealCancellation => 3,
        IngestFaultKind::RetentionClockBoundary => 4,
        IngestFaultKind::PurgeUnlinkError => 5,
        IngestFaultKind::PurgeCrashBoundary => 6,
    }
}

fn ingest_fault_from_tag(tag: u8) -> Result<IngestFaultKind, String> {
    match tag {
        1 => Ok(IngestFaultKind::PostAckRetry),
        2 => Ok(IngestFaultKind::PartialBatchAppend),
        3 => Ok(IngestFaultKind::SealCancellation),
        4 => Ok(IngestFaultKind::RetentionClockBoundary),
        5 => Ok(IngestFaultKind::PurgeUnlinkError),
        6 => Ok(IngestFaultKind::PurgeCrashBoundary),
        _ => Err(format!("retained ingest fault tag is invalid: {tag}")),
    }
}

fn encode_i20_fixture(out: &mut Vec<u8>, fixture: &I20FixtureV1) -> Result<(), String> {
    encode_fixture_header(out, fixture.seed, fixture.fixture_version);
    encode_documents(out, &fixture.sealed_baseline)?;
    encode_documents(out, &fixture.active_baseline)?;
    encode_documents(out, &fixture.submitted)?;
    encode_i64s(out, &fixture.timestamp_thresholds)?;
    put_u64(
        out,
        u64::try_from(fixture.partial_append_prefix_bytes)
            .map_err(|_| "retained I20 partial prefix exceeds u64".to_owned())?,
    );
    Ok(())
}

fn decode_i20_fixture(decoder: &mut RetainedDecoder<'_>) -> Result<I20FixtureV1, String> {
    let (seed, fixture_version) = decode_fixture_header(decoder)?;
    Ok(I20FixtureV1 {
        namespace: FIXTURE_NAMESPACE,
        seed,
        fixture_version,
        sealed_baseline: decode_documents(decoder)?,
        active_baseline: decode_documents(decoder)?,
        submitted: decode_documents(decoder)?,
        timestamp_thresholds: decode_i64s(decoder)?,
        partial_append_prefix_bytes: usize::try_from(decoder.u64()?)
            .map_err(|_| "retained I20 partial prefix exceeds usize".to_owned())?,
    })
}

fn encode_i21_fixture(out: &mut Vec<u8>, fixture: &I21FixtureV1) -> Result<(), String> {
    encode_fixture_header(out, fixture.seed, fixture.fixture_version);
    encode_documents(out, &fixture.initial)?;
    encode_documents(out, &fixture.upserts)?;
    put_len(out, fixture.deleted_doc_ids.len())?;
    for doc_id in &fixture.deleted_doc_ids {
        put_u128(out, *doc_id);
    }
    encode_i64s(out, &fixture.timestamp_thresholds)
}

fn decode_i21_fixture(decoder: &mut RetainedDecoder<'_>) -> Result<I21FixtureV1, String> {
    let (seed, fixture_version) = decode_fixture_header(decoder)?;
    let initial = decode_documents(decoder)?;
    let upserts = decode_documents(decoder)?;
    let deleted_len = decoder.len()?;
    let mut deleted_doc_ids = Vec::with_capacity(deleted_len);
    for _ in 0..deleted_len {
        deleted_doc_ids.push(decoder.u128()?);
    }
    Ok(I21FixtureV1 {
        namespace: FIXTURE_NAMESPACE,
        seed,
        fixture_version,
        initial,
        upserts,
        deleted_doc_ids,
        timestamp_thresholds: decode_i64s(decoder)?,
    })
}

fn encode_i22_fixture(out: &mut Vec<u8>, fixture: &I22FixtureV1) -> Result<(), String> {
    encode_fixture_header(out, fixture.seed, fixture.fixture_version);
    put_i64(out, fixture.clock_now);
    put_i64(out, fixture.retention_window);
    put_i64(out, fixture.expected_cutoff);
    put_len(out, fixture.partitions.len())?;
    for partition in &fixture.partitions {
        put_bytes(out, partition.label.as_bytes())?;
        encode_documents(out, &partition.documents)?;
    }
    encode_document(out, &fixture.active_control)?;
    encode_i64s(out, &fixture.timestamp_thresholds)
}

fn decode_i22_fixture(decoder: &mut RetainedDecoder<'_>) -> Result<I22FixtureV1, String> {
    let (seed, fixture_version) = decode_fixture_header(decoder)?;
    let clock_now = decoder.i64()?;
    let retention_window = decoder.i64()?;
    let expected_cutoff = decoder.i64()?;
    let partition_len = decoder.len()?;
    let mut partitions = Vec::with_capacity(partition_len);
    for _ in 0..partition_len {
        partitions.push(I22PartitionFixture {
            label: decoder.string()?,
            documents: decode_documents(decoder)?,
        });
    }
    Ok(I22FixtureV1 {
        namespace: FIXTURE_NAMESPACE,
        seed,
        fixture_version,
        clock_now,
        retention_window,
        expected_cutoff,
        partitions,
        active_control: decode_document(decoder)?,
        timestamp_thresholds: decode_i64s(decoder)?,
    })
}

fn encode_i23_fixture(out: &mut Vec<u8>, fixture: &I23FixtureV1) -> Result<(), String> {
    encode_fixture_header(out, fixture.seed, fixture.fixture_version);
    encode_document(out, &fixture.target)?;
    encode_document(out, &fixture.survivor)?;
    put_u8(
        out,
        match fixture.target_location {
            independent::I23TargetLocation::Active => 1,
            independent::I23TargetLocation::Sealed => 2,
        },
    );
    put_len(out, fixture.sentinel_patterns.len())?;
    for pattern in &fixture.sentinel_patterns {
        put_bytes(out, pattern)?;
    }
    encode_i64s(out, &fixture.timestamp_thresholds)
}

fn decode_i23_fixture(decoder: &mut RetainedDecoder<'_>) -> Result<I23FixtureV1, String> {
    let (seed, fixture_version) = decode_fixture_header(decoder)?;
    let target = decode_document(decoder)?;
    let survivor = decode_document(decoder)?;
    let target_location = match decoder.u8()? {
        1 => independent::I23TargetLocation::Active,
        2 => independent::I23TargetLocation::Sealed,
        tag => {
            return Err(format!(
                "retained I23 target-location tag is invalid: {tag}"
            ));
        }
    };
    let pattern_len = decoder.len()?;
    let mut sentinel_patterns = Vec::with_capacity(pattern_len);
    for _ in 0..pattern_len {
        sentinel_patterns.push(decoder.bytes()?);
    }
    Ok(I23FixtureV1 {
        namespace: FIXTURE_NAMESPACE,
        seed,
        fixture_version,
        target,
        survivor,
        target_location,
        sentinel_patterns,
        timestamp_thresholds: decode_i64s(decoder)?,
    })
}

fn encode_fixture_header(out: &mut Vec<u8>, seed: u64, version: u32) {
    put_u64(out, seed);
    put_u32(out, version);
}

fn decode_fixture_header(decoder: &mut RetainedDecoder<'_>) -> Result<(u64, u32), String> {
    Ok((decoder.u64()?, decoder.u32()?))
}

fn encode_documents(out: &mut Vec<u8>, documents: &[DocumentInput]) -> Result<(), String> {
    put_len(out, documents.len())?;
    for document in documents {
        encode_document(out, document)?;
    }
    Ok(())
}

fn decode_documents(decoder: &mut RetainedDecoder<'_>) -> Result<Vec<DocumentInput>, String> {
    let len = decoder.len()?;
    let mut documents = Vec::with_capacity(len);
    for _ in 0..len {
        documents.push(decode_document(decoder)?);
    }
    Ok(documents)
}

fn encode_document(out: &mut Vec<u8>, document: &DocumentInput) -> Result<(), String> {
    put_u128(out, document.doc_id);
    put_u64(out, document.revision);
    put_i64(out, document.timestamp);
    put_len(out, document.vector_bits.len())?;
    for bits in &document.vector_bits {
        put_u32(out, *bits);
    }
    put_bytes(out, &document.metadata_sentinel)?;
    put_bytes(out, &document.text)
}

fn decode_document(decoder: &mut RetainedDecoder<'_>) -> Result<DocumentInput, String> {
    let doc_id = decoder.u128()?;
    let revision = decoder.u64()?;
    let timestamp = decoder.i64()?;
    let vector_len = decoder.len()?;
    let mut vector_bits = Vec::with_capacity(vector_len);
    for _ in 0..vector_len {
        vector_bits.push(decoder.u32()?);
    }
    Ok(DocumentInput {
        doc_id,
        revision,
        timestamp,
        vector_bits,
        metadata_sentinel: decoder.bytes()?,
        text: decoder.bytes()?,
    })
}

fn encode_i64s(out: &mut Vec<u8>, values: &[i64]) -> Result<(), String> {
    put_len(out, values.len())?;
    for value in values {
        put_i64(out, *value);
    }
    Ok(())
}

fn decode_i64s(decoder: &mut RetainedDecoder<'_>) -> Result<Vec<i64>, String> {
    let len = decoder.len()?;
    let mut values = Vec::with_capacity(len);
    for _ in 0..len {
        values.push(decoder.i64()?);
    }
    Ok(values)
}

fn put_len(out: &mut Vec<u8>, len: usize) -> Result<(), String> {
    if len > INGEST_RETAINED_FIXTURE_MAX_ITEMS {
        return Err("retained ingest fixture item count exceeds the closed bound".to_owned());
    }
    put_u32(
        out,
        u32::try_from(len).map_err(|_| "retained ingest fixture length exceeds u32".to_owned())?,
    );
    Ok(())
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() > INGEST_RETAINED_FIXTURE_MAX_BYTES {
        return Err("retained ingest fixture byte field exceeds the closed bound".to_owned());
    }
    put_u32(
        out,
        u32::try_from(bytes.len())
            .map_err(|_| "retained ingest fixture byte length exceeds u32".to_owned())?,
    );
    out.extend_from_slice(bytes);
    Ok(())
}

fn put_u8(out: &mut Vec<u8>, value: u8) {
    out.push(value);
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u128(out: &mut Vec<u8>, value: u128) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_i64(out: &mut Vec<u8>, value: i64) {
    out.extend_from_slice(&value.to_le_bytes());
}

struct RetainedDecoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> RetainedDecoder<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or_else(|| "retained ingest fixture offset overflow".to_owned())?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| "retained ingest fixture is truncated".to_owned())?;
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, String> {
        self.take(1)?
            .first()
            .copied()
            .ok_or_else(|| "retained ingest fixture is truncated".to_owned())
    }

    fn u16(&mut self) -> Result<u16, String> {
        let bytes: [u8; 2] = self
            .take(2)?
            .try_into()
            .map_err(|_| "retained ingest u16 is truncated".to_owned())?;
        Ok(u16::from_le_bytes(bytes))
    }

    fn u32(&mut self) -> Result<u32, String> {
        let bytes: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| "retained ingest u32 is truncated".to_owned())?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let bytes: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| "retained ingest u64 is truncated".to_owned())?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn u128(&mut self) -> Result<u128, String> {
        let bytes: [u8; 16] = self
            .take(16)?
            .try_into()
            .map_err(|_| "retained ingest u128 is truncated".to_owned())?;
        Ok(u128::from_le_bytes(bytes))
    }

    fn i64(&mut self) -> Result<i64, String> {
        let bytes: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| "retained ingest i64 is truncated".to_owned())?;
        Ok(i64::from_le_bytes(bytes))
    }

    fn len(&mut self) -> Result<usize, String> {
        let len = usize::try_from(self.u32()?)
            .map_err(|_| "retained ingest length exceeds usize".to_owned())?;
        if len > INGEST_RETAINED_FIXTURE_MAX_ITEMS {
            return Err("retained ingest item count exceeds the closed bound".to_owned());
        }
        Ok(len)
    }

    fn bytes(&mut self) -> Result<Vec<u8>, String> {
        let len = usize::try_from(self.u32()?)
            .map_err(|_| "retained ingest byte length exceeds usize".to_owned())?;
        if len > INGEST_RETAINED_FIXTURE_MAX_BYTES {
            return Err("retained ingest byte field exceeds the closed bound".to_owned());
        }
        Ok(self.take(len)?.to_vec())
    }

    fn string(&mut self) -> Result<String, String> {
        String::from_utf8(self.bytes()?)
            .map_err(|error| format!("retained ingest string is not UTF-8: {error}"))
    }

    fn finish(&self) -> Result<(), String> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(format!(
                "retained ingest fixture has {} trailing bytes",
                self.bytes.len().saturating_sub(self.offset)
            ))
        }
    }
}

/// Executes one ingest-retention operation through its public product seam.
pub fn run_ingest_operation(
    operation: IngestOperationKind,
    seed: u64,
    fault: Option<IngestFaultKind>,
) -> Result<IngestOperationEvidence, String> {
    run_ingest_operation_with_invocation(operation, seed, fault, seed)
}

/// Executes one operation while binding receipts to the generated program's
/// exact invocation identity.
pub fn run_ingest_operation_with_invocation(
    operation: IngestOperationKind,
    seed: u64,
    fault: Option<IngestFaultKind>,
    invocation_id: u64,
) -> Result<IngestOperationEvidence, String> {
    match (operation, fault) {
        (IngestOperationKind::BatchCommit, None) => {
            run_i20_clean(seed).map(IngestOperationEvidence::I20)
        }
        (IngestOperationKind::BatchCommit, Some(IngestFaultKind::PostAckRetry)) => {
            run_i20_post_ack_retry(seed, invocation_id).map(IngestOperationEvidence::I20)
        }
        (IngestOperationKind::BatchCommit, Some(IngestFaultKind::PartialBatchAppend)) => {
            run_i20_partial_batch_append(seed, invocation_id).map(IngestOperationEvidence::I20)
        }
        (IngestOperationKind::BatchCommit, Some(fault)) => Err(format!(
            "I20 fault {fault:?} has no production receipt adapter yet"
        )),
        (IngestOperationKind::Seal, None) => run_i21_clean(seed).map(IngestOperationEvidence::I21),
        (IngestOperationKind::Seal, Some(IngestFaultKind::SealCancellation)) => {
            run_i21_seal_cancellation(seed, invocation_id).map(IngestOperationEvidence::I21)
        }
        (IngestOperationKind::Seal, Some(fault)) => Err(format!(
            "I21 fault {fault:?} has no production receipt adapter yet"
        )),
        (IngestOperationKind::Retention, None) => {
            run_i22_clean(seed).map(IngestOperationEvidence::I22)
        }
        (IngestOperationKind::Retention, Some(IngestFaultKind::RetentionClockBoundary)) => {
            run_i22_retention_clock_boundary(seed, invocation_id).map(IngestOperationEvidence::I22)
        }
        (IngestOperationKind::Retention, Some(fault)) => Err(format!(
            "I22 fault {fault:?} has no production receipt adapter yet"
        )),
        (IngestOperationKind::Purge, None) => run_i23_clean(seed).map(IngestOperationEvidence::I23),
        (IngestOperationKind::Purge, Some(IngestFaultKind::PurgeUnlinkError)) => {
            run_i23_purge_unlink_error(seed, invocation_id).map(IngestOperationEvidence::I23)
        }
        (IngestOperationKind::Purge, Some(IngestFaultKind::PurgeCrashBoundary)) => {
            run_i23_purge_crash_boundary(seed, invocation_id).map(IngestOperationEvidence::I23)
        }
        (IngestOperationKind::Purge, Some(fault)) => Err(format!(
            "I23 fault {fault:?} has no production receipt adapter yet"
        )),
    }
}

fn run_i22_clean(seed: u64) -> Result<I22Evidence, String> {
    run_i22_clean_from_fixture(i22_fixture(seed)?)
}

fn run_i22_clean_from_fixture(fixture: I22FixtureV1) -> Result<I22Evidence, String> {
    let directory = tempdir().map_err(|error| format!("create I22 Store directory: {error}"))?;
    let store = Store::open(directory.path(), OpenOptions::default())
        .map_err(|error| format!("open I22 Store: {error}"))?;
    let mut segment_labels = Vec::with_capacity(fixture.partitions.len());
    for partition in &fixture.partitions {
        let segment_id = seal_i22_partition(&store, partition)?;
        segment_labels.push(I22SegmentLabel {
            label: partition.label.clone(),
            segment_id,
        });
    }
    ingest_documents(
        &store,
        std::slice::from_ref(&fixture.active_control),
        "I22 active control",
    )?;
    let initial_directory = directory_evidence(directory.path())?;
    let generation_before = store
        .snapshot()
        .map_err(|error| format!("snapshot I22 generation before retention: {error}"))?
        .generation();
    let policy = RetentionPolicy::new(fixture.retention_window)
        .map_err(|error| format!("construct I22 retention policy: {error}"))?;
    let observed_range = policy.partition_to_drop(fixture.clock_now);
    let report = store
        .apply_retention(policy, fixture.clock_now)
        .map_err(|error| format!("apply I22 retention policy: {error}"))?;
    let dropped_labels = labels_for_segments(report.segments_dropped(), &segment_labels)?;
    let straddler_labels = labels_for_segments(report.straddlers_skipped(), &segment_labels)?;
    let retained_count = fixture
        .partitions
        .iter()
        .filter(|partition| partition.label != "before")
        .map(|partition| partition.documents.len())
        .sum::<usize>()
        .saturating_add(1);
    let live_after = observe_documents(&store, retained_count, &fixture.timestamp_thresholds)?;
    let active_control_fact =
        expected_document(&fixture.active_control, &fixture.timestamp_thresholds);
    let active_control_present = live_after.contains(&active_control_fact);
    let report_generation = report.generation();
    let manifest_committed = !report.is_no_op();
    let bytes_reclaimed = report.bytes_reclaimed();
    store
        .close()
        .map_err(|error| format!("close I22 Store: {error}"))?;
    let reopened = Store::open(directory.path(), OpenOptions::default())
        .map_err(|error| format!("reopen I22 Store: {error}"))?;
    let live_after_reopen =
        observe_documents(&reopened, retained_count, &fixture.timestamp_thresholds)?;
    reopened
        .close()
        .map_err(|error| format!("close reopened I22 Store: {error}"))?;

    let partitions = fixture
        .partitions
        .iter()
        .map(|partition| independent::I22PartitionInput {
            label: partition.label.clone(),
            rows: partition
                .documents
                .iter()
                .map(|document| {
                    (
                        document.timestamp,
                        expected_document(document, &fixture.timestamp_thresholds),
                    )
                })
                .collect(),
        })
        .collect::<Vec<_>>();
    let expected = independent::expected_i22(
        fixture.clock_now,
        fixture.retention_window,
        &partitions,
        active_control_fact,
    )?;
    let observed = independent::I22Observed {
        supplied_now: fixture.clock_now,
        observed_cutoff: observed_range.end,
        observed_drop_range: independent::I22RangeFact {
            start: observed_range.start,
            end: observed_range.end,
        },
        generation_before,
        report_generation,
        manifest_committed,
        dropped_labels,
        straddler_labels,
        bytes_reclaimed,
        live_after,
        live_after_reopen,
        active_control_present,
        receipt_digest: None,
    };
    independent::compare_i22(&expected, &observed)?;
    Ok(I22Evidence {
        fixture,
        expected,
        observed,
        segment_labels,
        initial_directory,
        control: None,
        receipts: Vec::new(),
    })
}

fn run_i22_retention_clock_boundary(seed: u64, invocation_id: u64) -> Result<I22Evidence, String> {
    run_i22_retention_clock_boundary_from_fixture(i22_fixture(seed)?, invocation_id)
}

fn run_i22_retention_clock_boundary_from_fixture(
    fixture: I22FixtureV1,
    invocation_id: u64,
) -> Result<I22Evidence, String> {
    let clean = run_i22_clean_from_fixture(fixture.clone())?;
    let directory =
        tempdir().map_err(|error| format!("create I22 fault Store directory: {error}"))?;
    let controller = IngestRetentionFaultController::new(invocation_id);
    let dependencies = StoreTestDependencies::new(Arc::new(StdVfs), Arc::new(SystemMonotonicClock))
        .with_ingest_retention_fault_controller(controller.clone());
    let store =
        Store::open_with_test_dependencies(directory.path(), OpenOptions::default(), dependencies)
            .map_err(|error| format!("open I22 retention-clock Store: {error}"))?;
    let mut segment_labels = Vec::with_capacity(fixture.partitions.len());
    for partition in &fixture.partitions {
        let segment_id = seal_i22_partition(&store, partition)?;
        segment_labels.push(I22SegmentLabel {
            label: partition.label.clone(),
            segment_id,
        });
    }
    ingest_documents(
        &store,
        std::slice::from_ref(&fixture.active_control),
        "I22 retention-clock active control",
    )?;
    let initial_directory = directory_evidence(directory.path())?;
    if initial_directory != clean.initial_directory {
        return Err("I22 clean and retention-clock fixtures are not byte-identical".to_owned());
    }
    let generation_before = store
        .snapshot()
        .map_err(|error| format!("snapshot I22 retention-clock generation: {error}"))?
        .generation();
    controller
        .arm(IngestRetentionTestFault::RetentionClockBoundary {
            now: fixture.clock_now,
            window: fixture.retention_window,
            expected_cutoff: fixture.expected_cutoff,
        })
        .map_err(|error| format!("arm I22 retention-clock boundary: {error}"))?;
    let policy = RetentionPolicy::new(fixture.retention_window)
        .map_err(|error| format!("construct I22 retention-clock policy: {error}"))?;
    let observed_range = policy.partition_to_drop(fixture.clock_now);
    let report = store
        .apply_retention(policy, fixture.clock_now)
        .map_err(|error| format!("apply I22 retention-clock policy: {error}"))?;
    let dropped_labels = labels_for_segments(report.segments_dropped(), &segment_labels)?;
    let straddler_labels = labels_for_segments(report.straddlers_skipped(), &segment_labels)?;
    let receipts = controller
        .take_receipts()
        .map_err(|error| format!("take I22 retention-clock receipts: {error}"))?;
    validate_retention_clock_receipt(
        &receipts,
        invocation_id,
        fixture.clock_now,
        fixture.retention_window,
        fixture.expected_cutoff,
        report.generation(),
        report.segments_dropped().len(),
        report.straddlers_skipped().len(),
        !report.is_no_op(),
    )?;
    let retained_count = fixture
        .partitions
        .iter()
        .filter(|partition| partition.label != "before")
        .map(|partition| partition.documents.len())
        .sum::<usize>()
        .saturating_add(1);
    let live_after = observe_documents(&store, retained_count, &fixture.timestamp_thresholds)?;
    let active_control_fact =
        expected_document(&fixture.active_control, &fixture.timestamp_thresholds);
    let active_control_present = live_after.contains(&active_control_fact);
    let report_generation = report.generation();
    let manifest_committed = !report.is_no_op();
    let bytes_reclaimed = report.bytes_reclaimed();
    store
        .close()
        .map_err(|error| format!("close I22 retention-clock Store: {error}"))?;
    let reopened = Store::open(directory.path(), OpenOptions::default())
        .map_err(|error| format!("reopen I22 retention-clock Store: {error}"))?;
    let live_after_reopen =
        observe_documents(&reopened, retained_count, &fixture.timestamp_thresholds)?;
    reopened
        .close()
        .map_err(|error| format!("close reopened I22 retention-clock Store: {error}"))?;

    let partitions = fixture
        .partitions
        .iter()
        .map(|partition| independent::I22PartitionInput {
            label: partition.label.clone(),
            rows: partition
                .documents
                .iter()
                .map(|document| {
                    (
                        document.timestamp,
                        expected_document(document, &fixture.timestamp_thresholds),
                    )
                })
                .collect(),
        })
        .collect::<Vec<_>>();
    let expected = independent::expected_i22(
        fixture.clock_now,
        fixture.retention_window,
        &partitions,
        active_control_fact,
    )?;
    let observed = independent::I22Observed {
        supplied_now: fixture.clock_now,
        observed_cutoff: observed_range.end,
        observed_drop_range: independent::I22RangeFact {
            start: observed_range.start,
            end: observed_range.end,
        },
        generation_before,
        report_generation,
        manifest_committed,
        dropped_labels,
        straddler_labels,
        bytes_reclaimed,
        live_after,
        live_after_reopen,
        active_control_present,
        receipt_digest: Some(receipt_digest(&receipts[0])),
    };
    independent::compare_i22(&expected, &observed)?;
    let passed = clean.observed.live_after_reopen == observed.live_after_reopen;
    if !passed {
        return Err("I22 clean and retention-clock final multisets differ".to_owned());
    }
    let control = I22ControlEvidence {
        clean_initial_directory: clean.initial_directory.clone(),
        fault_initial_directory: initial_directory.clone(),
        isolated_directories: true,
        clean_final: clean.observed.live_after_reopen,
        fault_final: observed.live_after_reopen.clone(),
        passed,
    };
    Ok(I22Evidence {
        fixture,
        expected,
        observed,
        segment_labels,
        initial_directory,
        control: Some(control),
        receipts,
    })
}

fn run_i23_clean(seed: u64) -> Result<I23Evidence, String> {
    run_i23_clean_from_fixture(i23_fixture(seed))
}

fn run_i23_clean_from_fixture(fixture: I23FixtureV1) -> Result<I23Evidence, String> {
    let directory = tempdir().map_err(|error| format!("create I23 Store directory: {error}"))?;
    let store = Store::open(directory.path(), OpenOptions::default())
        .map_err(|error| format!("open I23 Store: {error}"))?;
    ingest_documents(
        &store,
        &[fixture.target.clone(), fixture.survivor.clone()],
        "I23 target and survivor",
    )?;
    if fixture.target_location == independent::I23TargetLocation::Sealed {
        store
            .seal()
            .map_err(|error| format!("seal I23 target fixture: {error}"))?;
    }
    let initial_directory = directory_evidence(directory.path())?;
    let generation_before_delete = store
        .snapshot()
        .map_err(|error| format!("snapshot I23 generation before delete: {error}"))?
        .generation();
    let delete_ack = store
        .delete(DeleteBatch::new(vec![DocId::new(fixture.target.doc_id)]))
        .map_err(|error| format!("logically delete I23 purge target: {error}"))?;
    let generation_after_delete = store
        .snapshot()
        .map_err(|error| format!("snapshot I23 generation after delete: {error}"))?
        .generation();
    let logical_search = observe_documents(&store, 1, &fixture.timestamp_thresholds)?;
    let pre_purge_hits = scan_i23_sentinels(directory.path(), &fixture.sentinel_patterns)?;
    let token = store
        .purge(&[DocId::new(fixture.target.doc_id)])
        .map_err(|error| format!("schedule I23 physical purge: {error}"))?;
    let token_fact = independent::I23PurgeTokenFact {
        token_id: token.id(),
        no_op: token.is_no_op(),
    };
    let report = store
        .await_physical_purge(token)
        .map_err(|error| format!("await I23 physical purge: {error}"))?;
    let post_purge_hits = scan_i23_sentinels(directory.path(), &fixture.sentinel_patterns)?;
    let intent_present = directory.path().join("purge.ze").exists();
    let immediate_live = observe_documents(&store, 1, &fixture.timestamp_thresholds)?;
    let generation_after_purge = store
        .snapshot()
        .map_err(|error| format!("snapshot I23 generation after purge: {error}"))?
        .generation();
    store
        .close()
        .map_err(|error| format!("close I23 Store: {error}"))?;

    let reopened = Store::open(directory.path(), OpenOptions::default())
        .map_err(|error| format!("first reopen I23 Store: {error}"))?;
    let reopen_live = observe_documents(&reopened, 1, &fixture.timestamp_thresholds)?;
    let generation_after_reopen = reopened
        .snapshot()
        .map_err(|error| format!("snapshot first reopened I23 Store: {error}"))?
        .generation();
    reopened
        .close()
        .map_err(|error| format!("close first reopened I23 Store: {error}"))?;
    let second = Store::open(directory.path(), OpenOptions::default())
        .map_err(|error| format!("second reopen I23 Store: {error}"))?;
    let second_reopen_live = observe_documents(&second, 1, &fixture.timestamp_thresholds)?;
    let generation_after_second_reopen = second
        .snapshot()
        .map_err(|error| format!("snapshot second reopened I23 Store: {error}"))?
        .generation();
    let mut sequence_probe = fixture.survivor.clone();
    sequence_probe.revision = sequence_probe
        .revision
        .checked_add(1)
        .ok_or_else(|| "I23 sequence-probe revision overflow".to_owned())?;
    let post_purge_ack = second
        .ingest(IngestBatch::new(vec![product_document(&sequence_probe)?]))
        .map_err(|error| format!("ingest I23 post-purge sequence probe: {error}"))?;
    if post_purge_ack.seq() <= delete_ack.seq() {
        return Err(format!(
            "I23 WAL sequence regressed across physical purge: prior={} post_purge={}",
            delete_ack.seq().get(),
            post_purge_ack.seq().get()
        ));
    }
    second
        .close()
        .map_err(|error| format!("close second reopened I23 Store: {error}"))?;

    let target = expected_document(&fixture.target, &fixture.timestamp_thresholds);
    let survivor = expected_document(&fixture.survivor, &fixture.timestamp_thresholds);
    let sentinel_patterns = fixture
        .sentinel_patterns
        .iter()
        .enumerate()
        .map(|(index, bytes)| {
            Ok(independent::I23SentinelPattern {
                index: u64::try_from(index)
                    .map_err(|_| "I23 sentinel index exceeds u64".to_owned())?,
                bytes: bytes.clone(),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let expected = independent::expected_i23(
        &[target, survivor],
        fixture.target.doc_id,
        fixture.target_location,
        sentinel_patterns,
    )?;
    let observed = independent::I23Observed {
        delete_result: independent::I23DeleteResultFact::Committed(independent::AckFact {
            seq: delete_ack.seq().get(),
            generation: delete_ack.generation(),
        }),
        logical_search,
        pre_purge_hits,
        purge_token: token_fact,
        fault_error: None,
        await_result: independent::I23AwaitResultFact {
            completed: true,
            generation: report.generation(),
        },
        post_purge_hits,
        intent_present,
        immediate_live,
        reopen_live,
        second_reopen_live,
        generation_facts: independent::I23GenerationFacts {
            before_delete: generation_before_delete,
            after_delete: generation_after_delete,
            after_purge: generation_after_purge,
            after_reopen: generation_after_reopen,
            after_second_reopen: generation_after_second_reopen,
        },
        receipt_digest: None,
    };
    independent::compare_i23(&expected, &observed)?;
    Ok(I23Evidence {
        fixture,
        expected,
        observed,
        initial_directory,
        control: None,
        receipts: Vec::new(),
    })
}

fn run_i23_purge_unlink_error(seed: u64, invocation_id: u64) -> Result<I23Evidence, String> {
    run_i23_purge_unlink_error_from_fixture(
        i23_fixture_for_location(seed, independent::I23TargetLocation::Sealed),
        invocation_id,
    )
}

fn run_i23_purge_unlink_error_from_fixture(
    fixture: I23FixtureV1,
    invocation_id: u64,
) -> Result<I23Evidence, String> {
    let clean = run_i23_clean_from_fixture(fixture.clone())?;
    let directory =
        tempdir().map_err(|error| format!("create I23 purge-unlink directory: {error}"))?;
    let controller = IngestRetentionFaultController::new(invocation_id);
    let vfs = Arc::new(PurgeUnlinkErrorVfs::new(
        Arc::new(StdVfs),
        controller.clone(),
    ));
    let dependencies = StoreTestDependencies::new(vfs, Arc::new(SystemMonotonicClock))
        .with_ingest_retention_fault_controller(controller.clone());
    let store =
        Store::open_with_test_dependencies(directory.path(), OpenOptions::default(), dependencies)
            .map_err(|error| format!("open I23 purge-unlink Store: {error}"))?;
    ingest_documents(
        &store,
        &[fixture.target.clone(), fixture.survivor.clone()],
        "I23 purge-unlink target and survivor",
    )?;
    store
        .seal()
        .map_err(|error| format!("seal I23 purge-unlink fixture: {error}"))?;
    let initial_directory = directory_evidence(directory.path())?;
    if initial_directory != clean.initial_directory {
        return Err("I23 clean and purge-unlink fixtures are not byte-identical".to_owned());
    }

    let generation_before_delete = store
        .snapshot()
        .map_err(|error| format!("snapshot I23 purge-unlink before delete: {error}"))?
        .generation();
    let delete_ack = store
        .delete(DeleteBatch::new(vec![DocId::new(fixture.target.doc_id)]))
        .map_err(|error| format!("logically delete I23 purge-unlink target: {error}"))?;
    let generation_after_delete = store
        .snapshot()
        .map_err(|error| format!("snapshot I23 purge-unlink after delete: {error}"))?
        .generation();
    let logical_search = observe_documents(&store, 1, &fixture.timestamp_thresholds)?;
    let pre_purge_hits = scan_i23_sentinels(directory.path(), &fixture.sentinel_patterns)?;
    let original = store
        .snapshot()
        .map_err(|error| format!("snapshot I23 purge-unlink original segment: {error}"))?
        .segments()
        .first()
        .ok_or_else(|| "I23 purge-unlink fixture has no sealed segment".to_owned())?
        .meta()
        .id;
    let old_path = directory.path().join(original.file_name());
    controller
        .arm(IngestRetentionTestFault::PurgeUnlinkError {
            target_segment: *original.as_bytes(),
        })
        .map_err(|error| format!("arm I23 purge-unlink error: {error}"))?;
    let token = store
        .purge(&[DocId::new(fixture.target.doc_id)])
        .map_err(|error| format!("schedule I23 purge-unlink physical purge: {error}"))?;
    let token_fact = independent::I23PurgeTokenFact {
        token_id: token.id(),
        no_op: token.is_no_op(),
    };
    let error = match store.await_physical_purge(token) {
        Ok(report) => {
            return Err(format!(
                "I23 purge-unlink error unexpectedly completed generation {}",
                report.generation()
            ));
        }
        Err(error) => error,
    };
    let fault_error = match &error {
        PurgeError::Store(StoreError::Io { path, source })
            if path == &old_path && source.kind() == std::io::ErrorKind::Other =>
        {
            format!("io:Other:{}", original.file_name())
        }
        _ => {
            return Err(format!(
                "I23 purge-unlink returned the wrong typed error: {error:?}"
            ));
        }
    };
    let receipts = controller
        .take_receipts()
        .map_err(|error| format!("take I23 purge-unlink receipts: {error}"))?;
    let replacement = store
        .snapshot()
        .map_err(|error| format!("snapshot I23 purge-unlink replacement: {error}"))?
        .segments()
        .iter()
        .map(|segment| segment.meta().id)
        .find(|segment| *segment != original)
        .ok_or_else(|| "I23 purge-unlink did not publish a replacement segment".to_owned())?;
    validate_purge_unlink_receipt(
        &receipts,
        invocation_id,
        *original.as_bytes(),
        *replacement.as_bytes(),
        &original.file_name(),
    )?;
    if !old_path.exists() {
        return Err("I23 purge-unlink error removed the original segment path".to_owned());
    }
    if !directory.path().join("purge.ze").exists() {
        return Err("I23 purge-unlink error removed the durable purge intent".to_owned());
    }
    store
        .close()
        .map_err(|error| format!("close failed I23 purge-unlink Store: {error}"))?;

    let recovered = Store::open(directory.path(), OpenOptions::default())
        .map_err(|error| format!("recover I23 purge-unlink Store: {error}"))?;
    let post_purge_hits = scan_i23_sentinels(directory.path(), &fixture.sentinel_patterns)?;
    let intent_present = directory.path().join("purge.ze").exists();
    let immediate_live = observe_documents(&recovered, 1, &fixture.timestamp_thresholds)?;
    let generation_after_purge = recovered
        .snapshot()
        .map_err(|error| format!("snapshot recovered I23 purge-unlink Store: {error}"))?
        .generation();
    recovered
        .close()
        .map_err(|error| format!("close recovered I23 purge-unlink Store: {error}"))?;

    let reopened = Store::open(directory.path(), OpenOptions::default())
        .map_err(|error| format!("first stable reopen I23 purge-unlink Store: {error}"))?;
    let reopen_live = observe_documents(&reopened, 1, &fixture.timestamp_thresholds)?;
    let generation_after_reopen = reopened
        .snapshot()
        .map_err(|error| format!("snapshot first stable I23 purge-unlink reopen: {error}"))?
        .generation();
    reopened
        .close()
        .map_err(|error| format!("close first stable I23 purge-unlink reopen: {error}"))?;
    let second = Store::open(directory.path(), OpenOptions::default())
        .map_err(|error| format!("second stable reopen I23 purge-unlink Store: {error}"))?;
    let second_reopen_live = observe_documents(&second, 1, &fixture.timestamp_thresholds)?;
    let generation_after_second_reopen = second
        .snapshot()
        .map_err(|error| format!("snapshot second stable I23 purge-unlink reopen: {error}"))?
        .generation();
    second
        .close()
        .map_err(|error| format!("close second stable I23 purge-unlink reopen: {error}"))?;

    let observed = independent::I23Observed {
        delete_result: independent::I23DeleteResultFact::Committed(independent::AckFact {
            seq: delete_ack.seq().get(),
            generation: delete_ack.generation(),
        }),
        logical_search,
        pre_purge_hits,
        purge_token: token_fact,
        fault_error: Some(fault_error),
        await_result: independent::I23AwaitResultFact {
            completed: true,
            generation: generation_after_purge,
        },
        post_purge_hits,
        intent_present,
        immediate_live,
        reopen_live,
        second_reopen_live,
        generation_facts: independent::I23GenerationFacts {
            before_delete: generation_before_delete,
            after_delete: generation_after_delete,
            after_purge: generation_after_purge,
            after_reopen: generation_after_reopen,
            after_second_reopen: generation_after_second_reopen,
        },
        receipt_digest: Some(receipt_digest(&receipts[0])),
    };
    independent::compare_i23(&clean.expected, &observed)?;
    let passed = clean.observed.second_reopen_live == observed.second_reopen_live;
    if !passed {
        return Err("I23 clean and purge-unlink final multisets differ".to_owned());
    }
    let control = I23ControlEvidence {
        clean_initial_directory: clean.initial_directory.clone(),
        fault_initial_directory: initial_directory.clone(),
        isolated_directories: true,
        clean_final: clean.observed.second_reopen_live,
        fault_final: observed.second_reopen_live.clone(),
        passed,
    };
    Ok(I23Evidence {
        fixture,
        expected: clean.expected,
        observed,
        initial_directory,
        control: Some(control),
        receipts,
    })
}

#[cfg(unix)]
fn run_i23_purge_crash_boundary(seed: u64, invocation_id: u64) -> Result<I23Evidence, String> {
    run_i23_purge_crash_boundary_from_fixture(
        i23_fixture_for_location(seed, independent::I23TargetLocation::Sealed),
        invocation_id,
    )
}

#[cfg(unix)]
fn run_i23_purge_crash_boundary_from_fixture(
    fixture: I23FixtureV1,
    invocation_id: u64,
) -> Result<I23Evidence, String> {
    let clean = run_i23_clean_from_fixture(fixture.clone())?;
    let directory =
        tempdir().map_err(|error| format!("create I23 purge-crash directory: {error}"))?;
    let store = Store::open(directory.path(), OpenOptions::default())
        .map_err(|error| format!("open I23 purge-crash Store: {error}"))?;
    ingest_documents(
        &store,
        &[fixture.target.clone(), fixture.survivor.clone()],
        "I23 purge-crash target and survivor",
    )?;
    store
        .seal()
        .map_err(|error| format!("seal I23 purge-crash fixture: {error}"))?;
    let initial_directory = directory_evidence(directory.path())?;
    if initial_directory != clean.initial_directory {
        return Err("I23 clean and purge-crash fixtures are not byte-identical".to_owned());
    }
    let generation_before_delete = store
        .snapshot()
        .map_err(|error| format!("snapshot I23 purge-crash before delete: {error}"))?
        .generation();
    let delete_ack = store
        .delete(DeleteBatch::new(vec![DocId::new(fixture.target.doc_id)]))
        .map_err(|error| format!("logically delete I23 purge-crash target: {error}"))?;
    let generation_after_delete = store
        .snapshot()
        .map_err(|error| format!("snapshot I23 purge-crash after delete: {error}"))?
        .generation();
    let logical_search = observe_documents(&store, 1, &fixture.timestamp_thresholds)?;
    let pre_purge_hits = scan_i23_sentinels(directory.path(), &fixture.sentinel_patterns)?;
    store
        .close()
        .map_err(|error| format!("close I23 purge-crash parent Store: {error}"))?;

    let receipt_directory =
        tempdir().map_err(|error| format!("create I23 purge-crash receipt directory: {error}"))?;
    let receipt_path = receipt_directory.path().join("receipt.bin");
    let status = Command::new(
        std::env::current_exe().map_err(|error| format!("I23 test executable: {error}"))?,
    )
    .arg("--exact")
    .arg("adversarial::ingest_retention::tests::purge_crash_receipt_child_helper")
    .arg("--ignored")
    .arg("--nocapture")
    .arg("--test-threads=1")
    .env("ZE_INGEST_PURGE_CRASH_DIRECTORY", directory.path())
    .env("ZE_INGEST_PURGE_CRASH_RECEIPT", &receipt_path)
    .env(
        "ZE_INGEST_PURGE_CRASH_TARGET",
        fixture.target.doc_id.to_string(),
    )
    .env(
        "ZE_INGEST_PURGE_CRASH_INVOCATION",
        invocation_id.to_string(),
    )
    .status()
    .map_err(|error| format!("spawn I23 purge-crash child: {error}"))?;
    let signal = status
        .signal()
        .ok_or_else(|| format!("I23 purge-crash child did not exit by signal: {status:?}"))?;
    if signal != SIGABRT_SIGNAL {
        return Err(format!(
            "I23 purge-crash child signal {signal}, expected SIGABRT"
        ));
    }
    let receipt_bytes = std::fs::read(&receipt_path)
        .map_err(|error| format!("read I23 durable purge-crash receipt: {error}"))?;
    let receipt = IngestRetentionFaultReceiptV1::decode_purge_crash_test_evidence(&receipt_bytes)?;
    let token_id = validate_purge_crash_receipt(&receipt, invocation_id, fixture.target.doc_id)?;
    if !directory.path().join("purge.ze").exists() {
        return Err("I23 purge-crash child did not leave a durable purge intent".to_owned());
    }

    let recovered = Store::open(directory.path(), OpenOptions::default())
        .map_err(|error| format!("recover I23 purge-crash Store: {error}"))?;
    let post_purge_hits = scan_i23_sentinels(directory.path(), &fixture.sentinel_patterns)?;
    let intent_present = directory.path().join("purge.ze").exists();
    let immediate_live = observe_documents(&recovered, 1, &fixture.timestamp_thresholds)?;
    let generation_after_purge = recovered
        .snapshot()
        .map_err(|error| format!("snapshot recovered I23 purge-crash Store: {error}"))?
        .generation();
    recovered
        .close()
        .map_err(|error| format!("close recovered I23 purge-crash Store: {error}"))?;
    let reopened = Store::open(directory.path(), OpenOptions::default())
        .map_err(|error| format!("first stable reopen I23 purge-crash Store: {error}"))?;
    let reopen_live = observe_documents(&reopened, 1, &fixture.timestamp_thresholds)?;
    let generation_after_reopen = reopened
        .snapshot()
        .map_err(|error| format!("snapshot first stable I23 purge-crash reopen: {error}"))?
        .generation();
    reopened
        .close()
        .map_err(|error| format!("close first stable I23 purge-crash reopen: {error}"))?;
    let second = Store::open(directory.path(), OpenOptions::default())
        .map_err(|error| format!("second stable reopen I23 purge-crash Store: {error}"))?;
    let second_reopen_live = observe_documents(&second, 1, &fixture.timestamp_thresholds)?;
    let generation_after_second_reopen = second
        .snapshot()
        .map_err(|error| format!("snapshot second stable I23 purge-crash reopen: {error}"))?
        .generation();
    second
        .close()
        .map_err(|error| format!("close second stable I23 purge-crash reopen: {error}"))?;

    let observed = independent::I23Observed {
        delete_result: independent::I23DeleteResultFact::Committed(independent::AckFact {
            seq: delete_ack.seq().get(),
            generation: delete_ack.generation(),
        }),
        logical_search,
        pre_purge_hits,
        purge_token: independent::I23PurgeTokenFact {
            token_id,
            no_op: false,
        },
        fault_error: Some(format!(
            "SIGABRT:{signal}:after-durable-intent-before-rewrite"
        )),
        await_result: independent::I23AwaitResultFact {
            completed: true,
            generation: generation_after_purge,
        },
        post_purge_hits,
        intent_present,
        immediate_live,
        reopen_live,
        second_reopen_live,
        generation_facts: independent::I23GenerationFacts {
            before_delete: generation_before_delete,
            after_delete: generation_after_delete,
            after_purge: generation_after_purge,
            after_reopen: generation_after_reopen,
            after_second_reopen: generation_after_second_reopen,
        },
        receipt_digest: Some(receipt_digest(&receipt)),
    };
    independent::compare_i23(&clean.expected, &observed)?;
    let passed = clean.observed.second_reopen_live == observed.second_reopen_live;
    if !passed {
        return Err("I23 clean and purge-crash final multisets differ".to_owned());
    }
    let control = I23ControlEvidence {
        clean_initial_directory: clean.initial_directory.clone(),
        fault_initial_directory: initial_directory.clone(),
        isolated_directories: true,
        clean_final: clean.observed.second_reopen_live,
        fault_final: observed.second_reopen_live.clone(),
        passed,
    };
    Ok(I23Evidence {
        fixture,
        expected: clean.expected,
        observed,
        initial_directory,
        control: Some(control),
        receipts: vec![receipt],
    })
}

#[cfg(not(unix))]
fn run_i23_purge_crash_boundary(_seed: u64, _invocation_id: u64) -> Result<I23Evidence, String> {
    Err("purge-crash-boundary requires Unix child-process signal evidence".to_owned())
}

#[cfg(not(unix))]
fn run_i23_purge_crash_boundary_from_fixture(
    _fixture: I23FixtureV1,
    _invocation_id: u64,
) -> Result<I23Evidence, String> {
    Err("purge-crash-boundary requires Unix child-process signal evidence".to_owned())
}

#[cfg(unix)]
fn run_i23_purge_crash_child_from_env() -> Result<(), String> {
    let directory = std::env::var_os("ZE_INGEST_PURGE_CRASH_DIRECTORY")
        .map(PathBuf::from)
        .ok_or_else(|| "ZE_INGEST_PURGE_CRASH_DIRECTORY is unset".to_owned())?;
    let receipt_sink = std::env::var_os("ZE_INGEST_PURGE_CRASH_RECEIPT")
        .map(PathBuf::from)
        .ok_or_else(|| "ZE_INGEST_PURGE_CRASH_RECEIPT is unset".to_owned())?;
    let target = std::env::var("ZE_INGEST_PURGE_CRASH_TARGET")
        .map_err(|_| "ZE_INGEST_PURGE_CRASH_TARGET is unset".to_owned())?
        .parse::<u128>()
        .map_err(|error| format!("parse I23 purge-crash target: {error}"))?;
    let invocation_id = std::env::var("ZE_INGEST_PURGE_CRASH_INVOCATION")
        .map_err(|_| "ZE_INGEST_PURGE_CRASH_INVOCATION is unset".to_owned())?
        .parse::<u64>()
        .map_err(|error| format!("parse I23 purge-crash invocation: {error}"))?;
    let controller = IngestRetentionFaultController::new(invocation_id);
    controller
        .arm(IngestRetentionTestFault::PurgeCrashBoundary {
            checkpoint: IngestRetentionPurgeCrashCheckpoint::AfterDurableIntentBeforeRewrite,
            receipt_sink,
        })
        .map_err(|error| format!("arm I23 purge-crash boundary: {error}"))?;
    let dependencies = StoreTestDependencies::new(Arc::new(StdVfs), Arc::new(SystemMonotonicClock))
        .with_ingest_retention_fault_controller(controller);
    let store =
        Store::open_with_test_dependencies(&directory, OpenOptions::default(), dependencies)
            .map_err(|error| format!("open I23 purge-crash child Store: {error}"))?;
    let token = store
        .purge(&[DocId::new(target)])
        .map_err(|error| format!("run I23 purge-crash child operation: {error}"))?;
    Err(format!(
        "I23 purge-crash child returned token {} instead of aborting",
        token.id()
    ))
}

fn scan_i23_sentinels(
    root: &Path,
    patterns: &[Vec<u8>],
) -> Result<Vec<independent::I23SentinelHit>, String> {
    let mut files = Vec::new();
    collect_i23_regular_files(root, &mut files)?;
    files.sort();
    let mut hits = Vec::new();
    for path in files {
        let bytes = std::fs::read(&path)
            .map_err(|error| format!("read I23 sentinel artifact {}: {error}", path.display()))?;
        let relative_path = path
            .strip_prefix(root)
            .map_err(|error| format!("strip I23 artifact root: {error}"))?
            .to_string_lossy()
            .replace('\\', "/");
        for (pattern_index, pattern) in patterns.iter().enumerate() {
            if pattern.is_empty() {
                return Err("I23 sentinel pattern is empty".to_owned());
            }
            for (offset, window) in bytes.windows(pattern.len()).enumerate() {
                if window == pattern.as_slice() {
                    hits.push(independent::I23SentinelHit {
                        relative_path: relative_path.clone(),
                        offset: u64::try_from(offset)
                            .map_err(|_| "I23 sentinel offset exceeds u64".to_owned())?,
                        sentinel_index: u64::try_from(pattern_index)
                            .map_err(|_| "I23 sentinel index exceeds u64".to_owned())?,
                    });
                }
            }
        }
    }
    hits.sort();
    Ok(hits)
}

fn collect_i23_regular_files(directory: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let mut entries = std::fs::read_dir(directory)
        .map_err(|error| {
            format!(
                "read I23 artifact directory {}: {error}",
                directory.display()
            )
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read I23 artifact entry: {error}"))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("stat I23 artifact {}: {error}", path.display()))?;
        if file_type.is_dir() {
            collect_i23_regular_files(&path, files)?;
        } else if file_type.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

fn seal_i22_partition(store: &Store, partition: &I22PartitionFixture) -> Result<[u8; 16], String> {
    let before = store
        .snapshot()
        .map_err(|error| format!("snapshot I22 {} before seal: {error}", partition.label))?
        .segments()
        .iter()
        .map(|segment| *segment.meta().id.as_bytes())
        .collect::<Vec<_>>();
    ingest_documents(
        store,
        &partition.documents,
        &format!("I22 {}", partition.label),
    )?;
    store
        .seal()
        .map_err(|error| format!("seal I22 {} partition: {error}", partition.label))?;
    let after = store
        .snapshot()
        .map_err(|error| format!("snapshot I22 {} after seal: {error}", partition.label))?;
    let mut created = after
        .segments()
        .iter()
        .map(|segment| *segment.meta().id.as_bytes())
        .filter(|segment_id| !before.contains(segment_id));
    let segment_id = created
        .next()
        .ok_or_else(|| format!("I22 {} seal published no new segment", partition.label))?;
    if created.next().is_some() {
        return Err(format!(
            "I22 {} seal published more than one segment",
            partition.label
        ));
    }
    Ok(segment_id)
}

fn labels_for_segments(
    segments: &[zeppelin_embed::segment::SegmentId],
    labels: &[I22SegmentLabel],
) -> Result<Vec<String>, String> {
    segments
        .iter()
        .map(|segment| {
            labels
                .iter()
                .find(|label| label.segment_id == *segment.as_bytes())
                .map(|label| label.label.clone())
                .ok_or_else(|| format!("I22 report returned unknown segment {segment}"))
        })
        .collect()
}

fn run_i21_clean(seed: u64) -> Result<I21Evidence, String> {
    run_i21_clean_from_fixture(i21_fixture(seed))
}

fn run_i21_clean_from_fixture(fixture: I21FixtureV1) -> Result<I21Evidence, String> {
    let directory = tempdir().map_err(|error| format!("create I21 Store directory: {error}"))?;
    let store = Store::open(directory.path(), OpenOptions::default())
        .map_err(|error| format!("open I21 Store: {error}"))?;
    ingest_documents(&store, &fixture.initial, "I21 initial documents")?;
    ingest_documents(&store, &fixture.upserts, "I21 higher revisions")?;
    let deletes = fixture
        .deleted_doc_ids
        .iter()
        .copied()
        .map(DocId::new)
        .collect::<Vec<_>>();
    store
        .delete(DeleteBatch::new(deletes))
        .map_err(|error| format!("delete I21 tombstoned row: {error}"))?;
    let live_count = i21_live_count(&fixture);
    let live_before = observe_documents(&store, live_count, &fixture.timestamp_thresholds)?;
    let before_stats = store
        .stats()
        .map_err(|error| format!("observe I21 stats before seal: {error}"))?;
    let generation_before = store
        .snapshot()
        .map_err(|error| format!("snapshot I21 generation before seal: {error}"))?
        .generation();
    let initial_directory = directory_evidence(directory.path())?;

    let sealed_generation = store
        .seal()
        .map_err(|error| format!("seal I21 Store: {error}"))?;
    let live_after = observe_documents(&store, live_count, &fixture.timestamp_thresholds)?;
    let after_stats = store
        .stats()
        .map_err(|error| format!("observe I21 stats after seal: {error}"))?;
    let generation_after = store
        .snapshot()
        .map_err(|error| format!("snapshot I21 generation after seal: {error}"))?
        .generation();
    let orphan_paths = temporary_seal_paths(directory.path())?;
    store
        .close()
        .map_err(|error| format!("close I21 Store: {error}"))?;
    let reopened = Store::open(directory.path(), OpenOptions::default())
        .map_err(|error| format!("reopen I21 Store: {error}"))?;
    let live_after_reopen =
        observe_documents(&reopened, live_count, &fixture.timestamp_thresholds)?;
    reopened
        .close()
        .map_err(|error| format!("close reopened I21 Store: {error}"))?;

    let initial = fixture
        .initial
        .iter()
        .map(|document| expected_document(document, &fixture.timestamp_thresholds))
        .collect::<Vec<_>>();
    let upserts = fixture
        .upserts
        .iter()
        .map(|document| expected_document(document, &fixture.timestamp_thresholds))
        .collect::<Vec<_>>();
    let expected = independent::expected_i21(
        &initial,
        &upserts,
        &fixture.deleted_doc_ids,
        u64::try_from(fixture.initial.len())
            .map_err(|_| "I21 initial row count exceeds u64".to_owned())?,
    );
    let observed = independent::I21Observed {
        seal_result: independent::SealResultFact::Committed {
            generation: sealed_generation,
        },
        live_before,
        cancelled_live: None,
        cancelled_active_rows: None,
        cancelled_generation: None,
        cancelled_orphan_paths: None,
        retry_seal_generation: None,
        live_after,
        live_after_reopen,
        active_rows_before: before_stats.active_row_count,
        active_rows_after: after_stats.active_row_count,
        generation_before,
        generation_after,
        orphan_paths,
        receipt_digest: None,
    };
    independent::compare_i21(&expected, &observed)?;
    Ok(I21Evidence {
        fixture,
        expected,
        observed,
        initial_directory,
        control: None,
        receipts: Vec::new(),
    })
}

fn run_i21_seal_cancellation(seed: u64, invocation_id: u64) -> Result<I21Evidence, String> {
    run_i21_seal_cancellation_from_fixture(i21_fixture(seed), invocation_id)
}

fn run_i21_seal_cancellation_from_fixture(
    fixture: I21FixtureV1,
    invocation_id: u64,
) -> Result<I21Evidence, String> {
    let clean = run_i21_clean_from_fixture(fixture.clone())?;
    let directory = tempdir().map_err(|error| format!("create I21 fault directory: {error}"))?;
    let controller = IngestRetentionFaultController::new(invocation_id);
    let dependencies = StoreTestDependencies::new(Arc::new(StdVfs), Arc::new(SystemMonotonicClock))
        .with_ingest_retention_fault_controller(controller.clone());
    let store =
        Store::open_with_test_dependencies(directory.path(), OpenOptions::default(), dependencies)
            .map_err(|error| format!("open I21 cancellation Store: {error}"))?;
    ingest_documents(
        &store,
        &fixture.initial,
        "I21 cancellation initial documents",
    )?;
    ingest_documents(
        &store,
        &fixture.upserts,
        "I21 cancellation higher revisions",
    )?;
    let delete_ack = store
        .delete(DeleteBatch::new(
            fixture
                .deleted_doc_ids
                .iter()
                .copied()
                .map(DocId::new)
                .collect(),
        ))
        .map_err(|error| format!("delete I21 cancellation tombstone: {error}"))?;
    let live_count = i21_live_count(&fixture);
    let live_before = observe_documents(&store, live_count, &fixture.timestamp_thresholds)?;
    let before_stats = store
        .stats()
        .map_err(|error| format!("observe I21 cancellation stats before seal: {error}"))?;
    let generation_before = store
        .snapshot()
        .map_err(|error| format!("snapshot I21 cancellation generation: {error}"))?
        .generation();
    let initial_directory = directory_evidence(directory.path())?;
    if initial_directory != clean.initial_directory {
        return Err("I21 clean and cancellation fixtures are not byte-identical".to_owned());
    }
    controller
        .arm(IngestRetentionTestFault::SealCancellation)
        .map_err(|error| format!("arm I21 late seal cancellation: {error}"))?;
    let error = match store.seal_with_cancel(&CancelToken::new()) {
        Ok(generation) => {
            return Err(format!(
                "I21 late seal cancellation unexpectedly committed generation {generation}"
            ));
        }
        Err(error) => error,
    };
    if !matches!(error, StoreError::SealCancelled) {
        return Err(format!(
            "I21 late seal cancellation returned wrong typed error: {error:?}"
        ));
    }
    let receipts = controller
        .take_receipts()
        .map_err(|error| format!("take I21 cancellation receipts: {error}"))?;
    validate_seal_cancellation_receipt(
        &receipts,
        invocation_id,
        fixture.initial.len(),
        delete_ack.seq().get(),
        generation_before,
    )?;
    let cancelled_live = observe_documents(&store, live_count, &fixture.timestamp_thresholds)?;
    let cancelled_stats = store
        .stats()
        .map_err(|error| format!("observe I21 cancelled stats: {error}"))?;
    let cancelled_generation = store
        .snapshot()
        .map_err(|error| format!("snapshot I21 cancelled generation: {error}"))?
        .generation();
    let cancelled_orphan_paths = temporary_seal_paths(directory.path())?;

    let retry_seal_generation = store
        .seal()
        .map_err(|error| format!("clean seal after I21 cancellation: {error}"))?;
    let live_after = observe_documents(&store, live_count, &fixture.timestamp_thresholds)?;
    let after_stats = store
        .stats()
        .map_err(|error| format!("observe I21 cancellation stats after clean seal: {error}"))?;
    let generation_after = store
        .snapshot()
        .map_err(|error| format!("snapshot I21 cancellation after clean seal: {error}"))?
        .generation();
    let orphan_paths = temporary_seal_paths(directory.path())?;
    store
        .close()
        .map_err(|error| format!("close I21 cancellation Store: {error}"))?;
    let reopened = Store::open(directory.path(), OpenOptions::default())
        .map_err(|error| format!("reopen I21 cancellation Store: {error}"))?;
    let live_after_reopen =
        observe_documents(&reopened, live_count, &fixture.timestamp_thresholds)?;
    reopened
        .close()
        .map_err(|error| format!("close reopened I21 cancellation Store: {error}"))?;

    let initial = fixture
        .initial
        .iter()
        .map(|document| expected_document(document, &fixture.timestamp_thresholds))
        .collect::<Vec<_>>();
    let upserts = fixture
        .upserts
        .iter()
        .map(|document| expected_document(document, &fixture.timestamp_thresholds))
        .collect::<Vec<_>>();
    let expected = independent::expected_i21(
        &initial,
        &upserts,
        &fixture.deleted_doc_ids,
        u64::try_from(fixture.initial.len())
            .map_err(|_| "I21 initial row count exceeds u64".to_owned())?,
    );
    let observed = independent::I21Observed {
        seal_result: independent::SealResultFact::Cancelled,
        live_before,
        cancelled_live: Some(cancelled_live),
        cancelled_active_rows: Some(cancelled_stats.active_row_count),
        cancelled_generation: Some(cancelled_generation),
        cancelled_orphan_paths: Some(cancelled_orphan_paths),
        retry_seal_generation: Some(retry_seal_generation),
        live_after,
        live_after_reopen,
        active_rows_before: before_stats.active_row_count,
        active_rows_after: after_stats.active_row_count,
        generation_before,
        generation_after,
        orphan_paths,
        receipt_digest: Some(receipt_digest(&receipts[0])),
    };
    independent::compare_i21(&expected, &observed)?;
    let passed = clean.observed.live_after_reopen == observed.live_after_reopen;
    if !passed {
        return Err("I21 clean and late-cancel retry final multisets differ".to_owned());
    }
    let control = I21ControlEvidence {
        clean_initial_directory: clean.initial_directory.clone(),
        fault_initial_directory: initial_directory.clone(),
        isolated_directories: true,
        clean_final: clean.observed.live_after_reopen,
        fault_final: observed.live_after_reopen.clone(),
        passed,
    };
    Ok(I21Evidence {
        fixture,
        expected,
        observed,
        initial_directory,
        control: Some(control),
        receipts,
    })
}

fn run_i20_clean(seed: u64) -> Result<I20Evidence, String> {
    run_i20_clean_from_fixture(i20_fixture(seed, false))
}

fn run_i20_clean_from_fixture(fixture: I20FixtureV1) -> Result<I20Evidence, String> {
    let directory = tempdir().map_err(|error| format!("create I20 Store directory: {error}"))?;
    let store = Store::open(directory.path(), OpenOptions::default())
        .map_err(|error| format!("open I20 Store: {error}"))?;

    ingest_documents(&store, &fixture.sealed_baseline, "sealed baseline")?;
    store
        .seal()
        .map_err(|error| format!("seal I20 baseline: {error}"))?;
    ingest_documents(&store, &fixture.active_baseline, "active baseline")?;

    let baseline_count = fixture
        .sealed_baseline
        .len()
        .saturating_add(fixture.active_baseline.len());
    let before = observe_documents(&store, baseline_count, &fixture.timestamp_thresholds)?;
    let initial_directory = directory_evidence(directory.path())?;
    let generation_before = store
        .snapshot()
        .map_err(|error| format!("snapshot I20 generation before batch: {error}"))?
        .generation();

    let submitted_batch = fixture
        .submitted
        .iter()
        .map(product_document)
        .collect::<Result<Vec<_>, _>>()?;
    let ack = store
        .ingest(IngestBatch::new(submitted_batch))
        .map_err(|error| format!("commit I20 submitted batch: {error}"))?;
    let final_count = independent_final_count(&fixture);
    let immediate = observe_documents(&store, final_count, &fixture.timestamp_thresholds)?;
    let stats = store
        .stats()
        .map_err(|error| format!("observe I20 Store stats: {error}"))?;
    let generation_after = store
        .snapshot()
        .map_err(|error| format!("snapshot I20 generation after batch: {error}"))?
        .generation();
    store
        .close()
        .map_err(|error| format!("close I20 Store: {error}"))?;

    let reopened = Store::open(directory.path(), OpenOptions::default())
        .map_err(|error| format!("reopen I20 Store: {error}"))?;
    let post_reopen = observe_documents(&reopened, final_count, &fixture.timestamp_thresholds)?;
    reopened
        .close()
        .map_err(|error| format!("close reopened I20 Store: {error}"))?;

    let baseline = fixture
        .sealed_baseline
        .iter()
        .chain(&fixture.active_baseline)
        .map(|document| expected_document(document, &fixture.timestamp_thresholds))
        .collect::<Vec<_>>();
    let submitted = fixture
        .submitted
        .iter()
        .map(|document| expected_document(document, &fixture.timestamp_thresholds))
        .collect::<Vec<_>>();
    let expected_ack = independent::AckFact {
        seq: u64::try_from(
            fixture
                .sealed_baseline
                .len()
                .saturating_add(fixture.active_baseline.len())
                .saturating_add(fixture.submitted.len()),
        )
        .map_err(|_| "I20 expected sequence exceeds u64".to_owned())?,
        generation: generation_before
            .checked_add(1)
            .ok_or_else(|| "I20 expected generation overflows".to_owned())?,
    };
    let expected = independent::expected_i20(
        &baseline,
        &submitted,
        independent::I20Outcome::Commit,
        Some(expected_ack),
        1,
        None,
    )?;
    let observed = independent::I20Observed {
        typed_result: independent::IngestResultFact::Committed(independent::AckFact {
            seq: ack.seq().get(),
            generation: ack.generation(),
        }),
        before_live: before,
        immediate_live: immediate,
        stats: independent::I20StatsFact {
            active_rows: stats.active_row_count,
            tombstones: stats.tombstone_count,
        },
        post_reopen_live: post_reopen,
        ack: Some(independent::AckFact {
            seq: ack.seq().get(),
            generation: ack.generation(),
        }),
        generation_before,
        generation_after,
        retry_ack: None,
        retry_live: None,
        retry_wal_records_appended: None,
        retry_generation_delta: None,
        receipt_digest: None,
    };
    independent::compare_i20(&expected, &observed)?;
    Ok(I20Evidence {
        fixture,
        expected,
        observed,
        initial_directory,
        control: None,
        receipts: Vec::new(),
    })
}

fn run_i20_post_ack_retry(seed: u64, invocation_id: u64) -> Result<I20Evidence, String> {
    run_i20_post_ack_retry_from_fixture(i20_fixture(seed, false), invocation_id)
}

fn run_i20_post_ack_retry_from_fixture(
    fixture: I20FixtureV1,
    invocation_id: u64,
) -> Result<I20Evidence, String> {
    let clean = run_i20_clean_from_fixture(fixture.clone())?;
    let directory = tempdir().map_err(|error| format!("create I20 fault directory: {error}"))?;
    let controller = IngestRetentionFaultController::new(invocation_id);
    let dependencies = StoreTestDependencies::new(Arc::new(StdVfs), Arc::new(SystemMonotonicClock))
        .with_ingest_retention_fault_controller(controller.clone());
    let store =
        Store::open_with_test_dependencies(directory.path(), OpenOptions::default(), dependencies)
            .map_err(|error| format!("open I20 post-ack Store: {error}"))?;
    ingest_documents(&store, &fixture.sealed_baseline, "sealed baseline")?;
    store
        .seal()
        .map_err(|error| format!("seal I20 post-ack baseline: {error}"))?;
    ingest_documents(&store, &fixture.active_baseline, "active baseline")?;
    let baseline_count = fixture
        .sealed_baseline
        .len()
        .saturating_add(fixture.active_baseline.len());
    let before = observe_documents(&store, baseline_count, &fixture.timestamp_thresholds)?;
    let initial_directory = directory_evidence(directory.path())?;
    if initial_directory != clean.initial_directory {
        return Err("I20 clean and post-ack fixtures are not byte-identical".to_owned());
    }
    let generation_before = store
        .snapshot()
        .map_err(|error| format!("snapshot I20 post-ack generation: {error}"))?
        .generation();
    let submitted_batch = fixture
        .submitted
        .iter()
        .map(product_document)
        .collect::<Result<Vec<_>, _>>()?;
    let batch = IngestBatch::new(submitted_batch);
    let first = store
        .ingest(batch.clone())
        .map_err(|error| format!("commit I20 post-ack batch: {error}"))?;
    let final_count = independent_final_count(&fixture);
    let immediate = observe_documents(&store, final_count, &fixture.timestamp_thresholds)?;
    let stats = store
        .stats()
        .map_err(|error| format!("observe I20 post-ack stats: {error}"))?;
    let wal_before = std::fs::read(directory.path().join("wal.ze"))
        .map_err(|error| format!("read I20 WAL before retry: {error}"))?;
    controller
        .arm(IngestRetentionTestFault::PostAckRetry {
            first_ack_seq: first.seq().get(),
            first_ack_generation: first.generation(),
        })
        .map_err(|error| format!("arm I20 post-ack retry: {error}"))?;
    let retry = store
        .ingest(batch)
        .map_err(|error| format!("retry I20 acknowledged batch: {error}"))?;
    let wal_after = std::fs::read(directory.path().join("wal.ze"))
        .map_err(|error| format!("read I20 WAL after retry: {error}"))?;
    if wal_before != wal_after {
        return Err("I20 post-ack retry appended or changed WAL bytes".to_owned());
    }
    let generation_after = store
        .snapshot()
        .map_err(|error| format!("snapshot I20 generation after retry: {error}"))?
        .generation();
    let retry_live = observe_documents(&store, final_count, &fixture.timestamp_thresholds)?;
    let receipts = controller
        .take_receipts()
        .map_err(|error| format!("take I20 post-ack receipts: {error}"))?;
    validate_post_ack_receipt(&receipts, invocation_id, fixture.submitted.len(), first)?;
    store
        .close()
        .map_err(|error| format!("close I20 post-ack Store: {error}"))?;
    let reopened = Store::open(directory.path(), OpenOptions::default())
        .map_err(|error| format!("reopen I20 post-ack Store: {error}"))?;
    let post_reopen = observe_documents(&reopened, final_count, &fixture.timestamp_thresholds)?;
    reopened
        .close()
        .map_err(|error| format!("close reopened I20 post-ack Store: {error}"))?;

    let baseline = fixture
        .sealed_baseline
        .iter()
        .chain(&fixture.active_baseline)
        .map(|document| expected_document(document, &fixture.timestamp_thresholds))
        .collect::<Vec<_>>();
    let submitted = fixture
        .submitted
        .iter()
        .map(|document| expected_document(document, &fixture.timestamp_thresholds))
        .collect::<Vec<_>>();
    let expected_ack = independent::AckFact {
        seq: u64::try_from(
            fixture
                .sealed_baseline
                .len()
                .saturating_add(fixture.active_baseline.len())
                .saturating_add(fixture.submitted.len()),
        )
        .map_err(|_| "I20 expected post-ack sequence exceeds u64".to_owned())?,
        generation: generation_before
            .checked_add(1)
            .ok_or_else(|| "I20 expected post-ack generation overflows".to_owned())?,
    };
    let mut expected = independent::expected_i20(
        &baseline,
        &submitted,
        independent::I20Outcome::Commit,
        Some(expected_ack),
        1,
        None,
    )?;
    expected.retry = Some(independent::RetryExpected {
        ack: expected_ack,
        live: expected.final_live.clone(),
        wal_records_appended: 0,
        generation_delta: 0,
    });
    let observed = independent::I20Observed {
        typed_result: independent::IngestResultFact::Committed(independent::AckFact {
            seq: first.seq().get(),
            generation: first.generation(),
        }),
        before_live: before,
        immediate_live: immediate,
        stats: independent::I20StatsFact {
            active_rows: stats.active_row_count,
            tombstones: stats.tombstone_count,
        },
        post_reopen_live: post_reopen,
        ack: Some(independent::AckFact {
            seq: first.seq().get(),
            generation: first.generation(),
        }),
        generation_before,
        generation_after,
        retry_ack: Some(independent::AckFact {
            seq: retry.seq().get(),
            generation: retry.generation(),
        }),
        retry_live: Some(retry_live),
        retry_wal_records_appended: Some(0),
        retry_generation_delta: Some(
            generation_after
                .checked_sub(first.generation())
                .ok_or_else(|| "I20 post-ack retry generation regressed".to_owned())?,
        ),
        receipt_digest: Some(receipt_digest(&receipts[0])),
    };
    independent::compare_i20(&expected, &observed)?;
    let passed = clean.observed.post_reopen_live == observed.post_reopen_live;
    if !passed {
        return Err("I20 clean and post-ack final multisets differ".to_owned());
    }
    let control = I20ControlEvidence {
        clean_initial_directory: clean.initial_directory.clone(),
        fault_initial_directory: initial_directory.clone(),
        isolated_directories: true,
        clean_final: clean.observed.post_reopen_live,
        fault_final: observed.post_reopen_live.clone(),
        passed,
    };
    Ok(I20Evidence {
        fixture,
        expected,
        observed,
        initial_directory,
        control: Some(control),
        receipts,
    })
}

fn run_i20_partial_batch_append(seed: u64, invocation_id: u64) -> Result<I20Evidence, String> {
    run_i20_partial_batch_append_from_fixture(i20_fixture(seed, true), invocation_id)
}

fn run_i20_partial_batch_append_from_fixture(
    fixture: I20FixtureV1,
    invocation_id: u64,
) -> Result<I20Evidence, String> {
    let clean = run_i20_clean_from_fixture(fixture.clone())?;
    let directory = tempdir().map_err(|error| format!("create I20 partial directory: {error}"))?;
    let controller = IngestRetentionFaultController::new(invocation_id);
    let vfs = Arc::new(PartialBatchAppendVfs::new(
        Arc::new(StdVfs),
        controller.clone(),
    ));
    let dependencies = StoreTestDependencies::new(vfs, Arc::new(SystemMonotonicClock))
        .with_ingest_retention_fault_controller(controller.clone());
    let store =
        Store::open_with_test_dependencies(directory.path(), OpenOptions::default(), dependencies)
            .map_err(|error| format!("open I20 partial-batch Store: {error}"))?;
    ingest_documents(&store, &fixture.sealed_baseline, "sealed baseline")?;
    store
        .seal()
        .map_err(|error| format!("seal I20 partial-batch baseline: {error}"))?;
    let active_documents = fixture
        .active_baseline
        .iter()
        .map(product_document)
        .collect::<Result<Vec<_>, _>>()?;
    let baseline_ack = store
        .ingest(IngestBatch::new(active_documents))
        .map_err(|error| format!("ingest I20 partial active baseline: {error}"))?;
    let baseline_count = fixture
        .sealed_baseline
        .len()
        .saturating_add(fixture.active_baseline.len());
    let before = observe_documents(&store, baseline_count, &fixture.timestamp_thresholds)?;
    let initial_directory = directory_evidence(directory.path())?;
    if initial_directory != clean.initial_directory {
        return Err("I20 clean and partial-batch fixtures are not byte-identical".to_owned());
    }
    let generation_before = store
        .snapshot()
        .map_err(|error| format!("snapshot I20 partial generation: {error}"))?
        .generation();
    let encoded_bytes = independent_partial_group_bytes(&fixture.submitted)?;
    let prefix_bytes = u64::try_from(fixture.partial_append_prefix_bytes)
        .map_err(|_| "I20 partial prefix exceeds u64".to_owned())?;
    let expected_detail =
        format!("injected partial-batch-append after {prefix_bytes}/{encoded_bytes} bytes");
    let submitted_batch = fixture
        .submitted
        .iter()
        .map(product_document)
        .collect::<Result<Vec<_>, _>>()?;
    let batch = IngestBatch::new(submitted_batch);
    controller
        .arm(IngestRetentionTestFault::PartialBatchAppend {
            prefix_bytes: fixture.partial_append_prefix_bytes,
        })
        .map_err(|error| format!("arm I20 partial batch append: {error}"))?;
    let error = store
        .ingest(batch.clone())
        .expect_err("I20 partial append unexpectedly committed");
    match &error {
        IngestError::Store(StoreError::WalWrite(WalWriteError::Failed { kind, detail }))
            if *kind == std::io::ErrorKind::Other && detail.as_ref() == expected_detail => {}
        other => {
            return Err(format!(
                "I20 partial append returned wrong typed error expected=Other/{expected_detail:?} observed={other:?}"
            ));
        }
    }
    let receipts = controller
        .take_receipts()
        .map_err(|error| format!("take I20 partial receipts: {error}"))?;
    validate_partial_batch_receipt(
        &receipts,
        invocation_id,
        fixture.submitted.len(),
        encoded_bytes,
        prefix_bytes,
        &expected_detail,
    )?;
    let immediate = observe_documents(&store, baseline_count, &fixture.timestamp_thresholds)?;
    let stats = store
        .stats()
        .map_err(|error| format!("observe I20 partial Store stats: {error}"))?;
    let generation_after = store
        .snapshot()
        .map_err(|error| format!("snapshot I20 generation after partial error: {error}"))?
        .generation();
    store
        .close()
        .map_err(|error| format!("close I20 partial Store: {error}"))?;

    let reopened = Store::open(directory.path(), OpenOptions::default())
        .map_err(|error| format!("reopen I20 partial Store: {error}"))?;
    let post_reopen = observe_documents(&reopened, baseline_count, &fixture.timestamp_thresholds)?;
    let retry_ack = reopened
        .ingest(batch)
        .map_err(|error| format!("clean retry I20 partial batch: {error}"))?;
    let final_count = independent_final_count(&fixture);
    let retry_live = observe_documents(&reopened, final_count, &fixture.timestamp_thresholds)?;
    let retry_wal_records_appended = retry_ack
        .seq()
        .get()
        .checked_sub(baseline_ack.seq().get())
        .ok_or_else(|| "I20 partial retry sequence regressed".to_owned())?;
    let retry_generation_delta = retry_ack
        .generation()
        .checked_sub(generation_after)
        .ok_or_else(|| "I20 partial retry generation regressed".to_owned())?;
    reopened
        .close()
        .map_err(|error| format!("close retried I20 partial Store: {error}"))?;

    let baseline = fixture
        .sealed_baseline
        .iter()
        .chain(&fixture.active_baseline)
        .map(|document| expected_document(document, &fixture.timestamp_thresholds))
        .collect::<Vec<_>>();
    let submitted = fixture
        .submitted
        .iter()
        .map(|document| expected_document(document, &fixture.timestamp_thresholds))
        .collect::<Vec<_>>();
    let expected_retry_ack = independent::AckFact {
        seq: u64::try_from(
            fixture
                .sealed_baseline
                .len()
                .saturating_add(fixture.active_baseline.len())
                .saturating_add(fixture.submitted.len()),
        )
        .map_err(|_| "I20 partial expected sequence exceeds u64".to_owned())?,
        generation: generation_before
            .checked_add(1)
            .ok_or_else(|| "I20 partial expected generation overflows".to_owned())?,
    };
    let committed_model = independent::expected_i20(
        &baseline,
        &submitted,
        independent::I20Outcome::Commit,
        Some(expected_retry_ack),
        1,
        None,
    )?;
    let expected = independent::expected_i20(
        &baseline,
        &submitted,
        independent::I20Outcome::Reject {
            kind: "wal-write/other".to_owned(),
            detail: expected_detail.clone(),
        },
        None,
        0,
        Some(independent::RetryExpected {
            ack: expected_retry_ack,
            live: committed_model.final_live,
            wal_records_appended: u64::try_from(fixture.submitted.len())
                .map_err(|_| "I20 partial submitted count exceeds u64".to_owned())?,
            generation_delta: 1,
        }),
    )?;
    let observed = independent::I20Observed {
        typed_result: independent::IngestResultFact::Rejected {
            kind: "wal-write/other".to_owned(),
            detail: expected_detail,
        },
        before_live: before,
        immediate_live: immediate,
        stats: independent::I20StatsFact {
            active_rows: stats.active_row_count,
            tombstones: stats.tombstone_count,
        },
        post_reopen_live: post_reopen,
        ack: None,
        generation_before,
        generation_after,
        retry_ack: Some(independent::AckFact {
            seq: retry_ack.seq().get(),
            generation: retry_ack.generation(),
        }),
        retry_live: Some(retry_live.clone()),
        retry_wal_records_appended: Some(retry_wal_records_appended),
        retry_generation_delta: Some(retry_generation_delta),
        receipt_digest: Some(receipt_digest(&receipts[0])),
    };
    independent::compare_i20(&expected, &observed)?;
    let passed = clean.observed.post_reopen_live == retry_live;
    if !passed {
        return Err("I20 clean and partial-batch retry final multisets differ".to_owned());
    }
    let control = I20ControlEvidence {
        clean_initial_directory: clean.initial_directory.clone(),
        fault_initial_directory: initial_directory.clone(),
        isolated_directories: true,
        clean_final: clean.observed.post_reopen_live,
        fault_final: retry_live,
        passed,
    };
    Ok(I20Evidence {
        fixture,
        expected,
        observed,
        initial_directory,
        control: Some(control),
        receipts,
    })
}

fn independent_partial_group_bytes(submitted: &[DocumentInput]) -> Result<u64, String> {
    let shapes = submitted
        .iter()
        .map(|document| {
            Ok(independent::UpsertV2Shape {
                vector_dimensions: u64::try_from(document.vector_bits.len())
                    .map_err(|_| "I20 vector dimension exceeds u64".to_owned())?,
                text_bytes: Some(
                    u64::try_from(document.text.len())
                        .map_err(|_| "I20 text length exceeds u64".to_owned())?,
                ),
                timestamp_present: true,
                metadata_bytes: Some(
                    u64::try_from(document.metadata_sentinel.len())
                        .map_err(|_| "I20 metadata length exceeds u64".to_owned())?,
                ),
                typed_columns_encoded_bytes: None,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    independent::encoded_upsert_v2_group_bytes(&shapes)
}

#[allow(clippy::too_many_arguments)]
fn validate_retention_clock_receipt(
    receipts: &[IngestRetentionFaultReceiptV1],
    invocation_id: u64,
    now: i64,
    window: i64,
    cutoff: i64,
    report_generation: u64,
    dropped_count: usize,
    straddler_count: usize,
    manifest_committed: bool,
) -> Result<(), String> {
    let [receipt] = receipts else {
        return Err(format!(
            "selected feature fault retention-clock-boundary produced {} Store receipts, expected 1",
            receipts.len()
        ));
    };
    if receipt.campaign() != "ingest-retention"
        || receipt.operation() != IngestRetentionOperation::Retention
        || receipt.fault() != IngestRetentionFaultKind::RetentionClockBoundary
        || receipt.checkpoint() != IngestRetentionCheckpoint::RetentionPolicyEvaluated
        || receipt.cardinality() != 1
        || receipt.invocation_id() != invocation_id
    {
        return Err(format!(
            "retention-clock receipt header mismatch: {receipt:?}"
        ));
    }
    let expected = IngestRetentionFaultEffect::RetentionClockBoundary {
        supplied_now: now,
        window,
        cutoff,
        range_start: i64::MIN,
        range_end: cutoff,
        report_generation,
        dropped_count: u64::try_from(dropped_count)
            .map_err(|_| "I22 dropped count exceeds u64".to_owned())?,
        straddler_count: u64::try_from(straddler_count)
            .map_err(|_| "I22 straddler count exceeds u64".to_owned())?,
        manifest_committed,
    };
    if receipt.effect() != &expected {
        return Err(format!(
            "retention-clock receipt effect mismatch expected={expected:?} observed={:?}",
            receipt.effect()
        ));
    }
    Ok(())
}

fn validate_purge_unlink_receipt(
    receipts: &[IngestRetentionFaultReceiptV1],
    invocation_id: u64,
    original_segment: [u8; 16],
    replacement_segment: [u8; 16],
    old_file_name: &str,
) -> Result<(), String> {
    let [receipt] = receipts else {
        return Err(format!(
            "selected feature fault purge-unlink-error produced {} Store receipts, expected 1",
            receipts.len()
        ));
    };
    if receipt.campaign() != "ingest-retention"
        || receipt.operation() != IngestRetentionOperation::Purge
        || receipt.fault() != IngestRetentionFaultKind::PurgeUnlinkError
        || receipt.checkpoint() != IngestRetentionCheckpoint::PurgeOldSegmentUnlinkError
        || receipt.cardinality() != 1
        || receipt.invocation_id() != invocation_id
    {
        return Err(format!("purge-unlink receipt header mismatch: {receipt:?}"));
    }
    let expected = IngestRetentionFaultEffect::PurgeUnlinkError {
        original_segment,
        replacement_segment,
        old_file_name: old_file_name.to_owned(),
        io_kind: IngestRetentionIoKind::Other,
        replacement_manifest_committed: true,
        intent_present: true,
        old_path_linked: true,
    };
    if receipt.effect() != &expected {
        return Err(format!(
            "purge-unlink receipt effect mismatch expected={expected:?} observed={:?}",
            receipt.effect()
        ));
    }
    Ok(())
}

fn validate_purge_crash_receipt(
    receipt: &IngestRetentionFaultReceiptV1,
    invocation_id: u64,
    target_id: u128,
) -> Result<u64, String> {
    if receipt.campaign() != "ingest-retention"
        || receipt.operation() != IngestRetentionOperation::Purge
        || receipt.fault() != IngestRetentionFaultKind::PurgeCrashBoundary
        || receipt.checkpoint() != IngestRetentionCheckpoint::PurgeAfterDurableIntentBeforeRewrite
        || receipt.cardinality() != 1
        || receipt.invocation_id() != invocation_id
    {
        return Err(format!("purge-crash receipt header mismatch: {receipt:?}"));
    }
    let IngestRetentionFaultEffect::PurgeCrashBoundary {
        target_ids,
        token_id,
        intent_file_name,
        intent_durable,
        artifact_rewrites,
        child_aborted,
    } = receipt.effect()
    else {
        return Err(format!("purge-crash receipt effect mismatch: {receipt:?}"));
    };
    if target_ids != &[target_id]
        || *token_id == 0
        || intent_file_name != "purge.ze"
        || !*intent_durable
        || *artifact_rewrites != 0
        || !*child_aborted
    {
        return Err(format!(
            "purge-crash receipt carried illegal effect facts: {:?}",
            receipt.effect()
        ));
    }
    Ok(*token_id)
}

fn validate_seal_cancellation_receipt(
    receipts: &[IngestRetentionFaultReceiptV1],
    invocation_id: u64,
    active_rows: usize,
    absorbed_wal_end: u64,
    generation_before: u64,
) -> Result<(), String> {
    let [receipt] = receipts else {
        return Err(format!(
            "selected feature fault seal-cancellation produced {} Store receipts, expected 1",
            receipts.len()
        ));
    };
    if receipt.campaign() != "ingest-retention"
        || receipt.operation() != IngestRetentionOperation::Seal
        || receipt.fault() != IngestRetentionFaultKind::SealCancellation
        || receipt.checkpoint()
            != IngestRetentionCheckpoint::SealAfterSegmentWriteBeforeManifestCommit
        || receipt.cardinality() != 1
        || receipt.invocation_id() != invocation_id
    {
        return Err(format!(
            "seal-cancellation receipt header mismatch: {receipt:?}"
        ));
    }
    let mut candidate_segment = [0_u8; 16];
    candidate_segment
        .get_mut(..8)
        .ok_or_else(|| "I21 candidate prefix is absent".to_owned())?
        .copy_from_slice(
            &generation_before
                .checked_add(1)
                .ok_or_else(|| "I21 candidate generation overflows".to_owned())?
                .to_be_bytes(),
        );
    candidate_segment
        .get_mut(8..)
        .ok_or_else(|| "I21 candidate suffix is absent".to_owned())?
        .copy_from_slice(&absorbed_wal_end.to_be_bytes());
    let expected = IngestRetentionFaultEffect::SealCancellation {
        active_rows: u64::try_from(active_rows)
            .map_err(|_| "I21 receipt active rows exceed u64".to_owned())?,
        absorbed_wal_end,
        candidate_segment,
        manifest_committed: false,
        temporary_segment_removed: true,
        generation_delta: 0,
    };
    if receipt.effect() != &expected {
        return Err(format!(
            "seal-cancellation receipt effect mismatch expected={expected:?} observed={:?}",
            receipt.effect()
        ));
    }
    Ok(())
}

fn validate_partial_batch_receipt(
    receipts: &[IngestRetentionFaultReceiptV1],
    invocation_id: u64,
    submitted_count: usize,
    encoded_bytes: u64,
    prefix_bytes: u64,
    detail: &str,
) -> Result<(), String> {
    let [receipt] = receipts else {
        return Err(format!(
            "selected feature fault partial-batch-append produced {} Store receipts, expected 1",
            receipts.len()
        ));
    };
    if receipt.campaign() != "ingest-retention"
        || receipt.operation() != IngestRetentionOperation::BatchCommit
        || receipt.fault() != IngestRetentionFaultKind::PartialBatchAppend
        || receipt.checkpoint() != IngestRetentionCheckpoint::IngestCommitManyAppendError
        || receipt.cardinality() != 1
        || receipt.invocation_id() != invocation_id
    {
        return Err(format!(
            "partial-batch receipt header mismatch: {receipt:?}"
        ));
    }
    let submitted_count = u64::try_from(submitted_count)
        .map_err(|_| "I20 partial receipt submitted count exceeds u64".to_owned())?;
    let expected = IngestRetentionFaultEffect::PartialBatchAppend {
        submitted_count,
        changed_records: submitted_count,
        encoded_bytes,
        prefix_bytes,
        io_kind: IngestRetentionIoKind::Other,
        detail: detail.to_owned(),
        active_published: false,
        generation_delta: 0,
    };
    if receipt.effect() != &expected {
        return Err(format!(
            "partial-batch receipt effect mismatch expected={expected:?} observed={:?}",
            receipt.effect()
        ));
    }
    Ok(())
}

fn validate_post_ack_receipt(
    receipts: &[IngestRetentionFaultReceiptV1],
    invocation_id: u64,
    batch_count: usize,
    ack: zeppelin_embed::ingest::IngestAck,
) -> Result<(), String> {
    let [receipt] = receipts else {
        return Err(format!(
            "selected feature fault post-ack-retry produced {} Store receipts, expected 1",
            receipts.len()
        ));
    };
    if receipt.campaign() != "ingest-retention"
        || receipt.operation() != IngestRetentionOperation::BatchCommit
        || receipt.fault() != IngestRetentionFaultKind::PostAckRetry
        || receipt.checkpoint() != IngestRetentionCheckpoint::IngestReplayNoWalAppend
        || receipt.cardinality() != 1
        || receipt.invocation_id() != invocation_id
    {
        return Err(format!(
            "post-ack retry receipt header mismatch: {receipt:?}"
        ));
    }
    let count = u64::try_from(batch_count)
        .map_err(|_| "I20 post-ack receipt batch count exceeds u64".to_owned())?;
    let expected = IngestRetentionFaultEffect::PostAckRetry {
        batch_count: count,
        replay_count: count,
        returned_seq: ack.seq().get(),
        returned_generation: ack.generation(),
        wal_records_appended: 0,
        generation_delta: 0,
        active_published: false,
    };
    if receipt.effect() != &expected {
        return Err(format!(
            "post-ack retry receipt effect mismatch expected={expected:?} observed={:?}",
            receipt.effect()
        ));
    }
    Ok(())
}

fn receipt_digest(receipt: &IngestRetentionFaultReceiptV1) -> u64 {
    fnv1a64(format!("{receipt:?}").as_bytes())
}

fn directory_evidence(root: &Path) -> Result<IngestDirectoryEvidence, String> {
    let mut paths = std::fs::read_dir(root)
        .map_err(|error| format!("read ingest fixture directory: {error}"))?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| format!("read ingest fixture entry: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();
    let mut files = Vec::new();
    for path in paths {
        if !path.is_file() {
            continue;
        }
        let relative_path = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "ingest fixture path is not canonical UTF-8".to_owned())?
            .to_owned();
        let bytes = std::fs::read(&path)
            .map_err(|error| format!("read ingest fixture {relative_path}: {error}"))?;
        files.push(IngestFileFact {
            relative_path,
            byte_length: u64::try_from(bytes.len())
                .map_err(|_| "ingest fixture file length exceeds u64".to_owned())?,
            digest: fnv1a64(&bytes),
        });
    }
    let mut canonical = Vec::new();
    for file in &files {
        canonical.extend_from_slice(file.relative_path.as_bytes());
        canonical.push(0);
        canonical.extend_from_slice(&file.byte_length.to_le_bytes());
        canonical.extend_from_slice(&file.digest.to_le_bytes());
    }
    Ok(IngestDirectoryEvidence {
        digest: fnv1a64(&canonical),
        files,
    })
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn i20_fixture(seed: u64, force_multi: bool) -> I20FixtureV1 {
    let base = 0x20_0000_u128.saturating_add(u128::from(seed).saturating_mul(16));
    let input = |offset: u128, revision: u64, timestamp: i64, vector: [f32; 2]| DocumentInput {
        doc_id: base.saturating_add(offset),
        revision,
        timestamp,
        vector_bits: vector.into_iter().map(f32::to_bits).collect(),
        metadata_sentinel: format!("i20/{seed}/{offset}/{revision}").into_bytes(),
        text: format!("i20 document {seed} {offset} revision {revision}").into_bytes(),
    };
    let sealed_baseline = vec![input(1, 1, 10, [1.0, 0.0])];
    let active_baseline = vec![input(2, 1, 20, [0.0, 1.0])];
    let submitted = if seed.is_multiple_of(2) && !force_multi {
        vec![input(1, 2, 30, [0.75, 0.25])]
    } else {
        vec![input(1, 2, 30, [0.75, 0.25]), input(3, 1, 40, [0.25, 0.75])]
    };
    let mut timestamp_thresholds = sealed_baseline
        .iter()
        .chain(&active_baseline)
        .chain(&submitted)
        .map(|document| document.timestamp)
        .collect::<Vec<_>>();
    timestamp_thresholds.sort_unstable();
    timestamp_thresholds.dedup();
    I20FixtureV1 {
        namespace: FIXTURE_NAMESPACE,
        seed,
        fixture_version: 1,
        sealed_baseline,
        active_baseline,
        submitted,
        timestamp_thresholds,
        partial_append_prefix_bytes: 7,
    }
}

fn i21_fixture(seed: u64) -> I21FixtureV1 {
    let base = 0x21_0000_u128.saturating_add(u128::from(seed).saturating_mul(16));
    let input = |offset: u128, revision: u64, timestamp: i64, vector: [f32; 2]| DocumentInput {
        doc_id: base.saturating_add(offset),
        revision,
        timestamp,
        vector_bits: vector.into_iter().map(f32::to_bits).collect(),
        metadata_sentinel: format!("i21/{seed}/{offset}/{revision}/sentinel").into_bytes(),
        text: format!("i21 document {seed} {offset} revision {revision}").into_bytes(),
    };
    let initial = vec![
        input(1, 1, 10, [1.0, 0.0]),
        input(2, 1, 20, [0.0, 1.0]),
        input(3, 1, 30, [0.5, 0.5]),
    ];
    let upserts = vec![input(1, 2, 40, [0.75, 0.25])];
    let deleted_doc_ids = vec![base.saturating_add(2)];
    let mut timestamp_thresholds = initial
        .iter()
        .chain(&upserts)
        .map(|document| document.timestamp)
        .collect::<Vec<_>>();
    timestamp_thresholds.sort_unstable();
    timestamp_thresholds.dedup();
    I21FixtureV1 {
        namespace: FIXTURE_NAMESPACE,
        seed,
        fixture_version: 1,
        initial,
        upserts,
        deleted_doc_ids,
        timestamp_thresholds,
    }
}

fn i22_fixture(seed: u64) -> Result<I22FixtureV1, String> {
    let seed_offset = i64::try_from(seed % 1_000)
        .map_err(|_| "I22 seed offset exceeds i64".to_owned())?
        .checked_mul(8)
        .ok_or_else(|| "I22 seed offset overflows".to_owned())?;
    let clock_now = 10_000_i64
        .checked_add(seed_offset)
        .ok_or_else(|| "I22 clock now overflows".to_owned())?;
    let retention_window = 100_i64;
    let expected_cutoff = clock_now
        .checked_sub(retention_window)
        .ok_or_else(|| "I22 cutoff overflows".to_owned())?;
    let before_timestamp = expected_cutoff
        .checked_sub(1)
        .ok_or_else(|| "I22 before timestamp overflows".to_owned())?;
    let after_timestamp = expected_cutoff
        .checked_add(1)
        .ok_or_else(|| "I22 after timestamp overflows".to_owned())?;
    let active_timestamp = expected_cutoff
        .checked_add(2)
        .ok_or_else(|| "I22 active timestamp overflows".to_owned())?;
    let base = 0x22_0000_u128.saturating_add(u128::from(seed).saturating_mul(16));
    let input = |offset: u128, timestamp: i64, label: &str| DocumentInput {
        doc_id: base.saturating_add(offset),
        revision: 1,
        timestamp,
        vector_bits: vec![(1.0_f32 + offset as f32).to_bits(), 1.0_f32.to_bits()],
        metadata_sentinel: format!("i22/{seed}/{label}/{offset}/sentinel").into_bytes(),
        text: format!("i22 {label} document {seed} {offset}").into_bytes(),
    };
    let partitions = vec![
        I22PartitionFixture {
            label: "before".to_owned(),
            documents: vec![input(1, before_timestamp, "before")],
        },
        I22PartitionFixture {
            label: "at".to_owned(),
            documents: vec![input(2, expected_cutoff, "at")],
        },
        I22PartitionFixture {
            label: "after".to_owned(),
            documents: vec![input(3, after_timestamp, "after")],
        },
        I22PartitionFixture {
            label: "straddler".to_owned(),
            documents: vec![
                input(4, before_timestamp, "straddler-before"),
                input(5, expected_cutoff, "straddler-at"),
            ],
        },
    ];
    let active_control = input(6, active_timestamp, "active-control");
    let timestamp_thresholds = vec![
        before_timestamp,
        expected_cutoff,
        after_timestamp,
        active_timestamp,
    ];
    Ok(I22FixtureV1 {
        namespace: FIXTURE_NAMESPACE,
        seed,
        fixture_version: 1,
        clock_now,
        retention_window,
        expected_cutoff,
        partitions,
        active_control,
        timestamp_thresholds,
    })
}

fn i23_fixture(seed: u64) -> I23FixtureV1 {
    let target_location = if seed.is_multiple_of(2) {
        independent::I23TargetLocation::Sealed
    } else {
        independent::I23TargetLocation::Active
    };
    i23_fixture_for_location(seed, target_location)
}

fn i23_fixture_for_location(
    seed: u64,
    target_location: independent::I23TargetLocation,
) -> I23FixtureV1 {
    let base = 0x23_0000_u128.saturating_add(u128::from(seed).saturating_mul(16));
    let low_seed = u32::try_from(seed & 0xffff).unwrap_or(0);
    let target_vector_bits = vec![
        0x3f10_0000 | low_seed,
        0xbe20_0000 | low_seed.rotate_left(3),
    ];
    let target_metadata = format!("ZE_I23_METADATA_{seed:016x}_7f4a91c2").into_bytes();
    let target_text = format!("ZE I23 TEXT {seed:016x} c28e5d71").into_bytes();
    let target = DocumentInput {
        doc_id: base.saturating_add(1),
        revision: 3,
        timestamp: 71,
        vector_bits: target_vector_bits.clone(),
        metadata_sentinel: target_metadata.clone(),
        text: target_text.clone(),
    };
    let survivor = DocumentInput {
        doc_id: base.saturating_add(2),
        revision: 1,
        timestamp: 83,
        vector_bits: vec![0.25_f32.to_bits(), 0.75_f32.to_bits()],
        metadata_sentinel: format!("i23-survivor-{seed:016x}").into_bytes(),
        text: format!("i23 survivor {seed:016x}").into_bytes(),
    };
    let mut sentinel_patterns = vec![target_metadata, target_text];
    sentinel_patterns.extend(
        target_vector_bits
            .iter()
            .map(|bits| bits.to_le_bytes().to_vec()),
    );
    I23FixtureV1 {
        namespace: FIXTURE_NAMESPACE,
        seed,
        fixture_version: 1,
        target,
        survivor,
        target_location,
        sentinel_patterns,
        timestamp_thresholds: vec![71, 83],
    }
}

fn i21_live_count(fixture: &I21FixtureV1) -> usize {
    let mut live = fixture
        .initial
        .iter()
        .map(|document| document.doc_id)
        .collect::<BTreeSet<_>>();
    for document in &fixture.upserts {
        live.insert(document.doc_id);
    }
    for doc_id in &fixture.deleted_doc_ids {
        live.remove(doc_id);
    }
    live.len()
}

fn temporary_seal_paths(root: &Path) -> Result<Vec<String>, String> {
    let mut paths = std::fs::read_dir(root)
        .map_err(|error| format!("read I21 seal directory: {error}"))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            name.contains(".tmp").then_some(name)
        })
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn independent_final_count(fixture: &I20FixtureV1) -> usize {
    fixture
        .sealed_baseline
        .iter()
        .chain(&fixture.active_baseline)
        .chain(&fixture.submitted)
        .map(|document| document.doc_id)
        .collect::<BTreeSet<_>>()
        .len()
}

fn ingest_documents(store: &Store, inputs: &[DocumentInput], label: &str) -> Result<(), String> {
    let documents = inputs
        .iter()
        .map(product_document)
        .collect::<Result<Vec<_>, _>>()?;
    store
        .ingest(IngestBatch::new(documents))
        .map(|_| ())
        .map_err(|error| format!("ingest I20 {label}: {error}"))
}

fn product_document(input: &DocumentInput) -> Result<IngestDocument, String> {
    let vector = input
        .vector_bits
        .iter()
        .copied()
        .map(f32::from_bits)
        .collect::<Vec<_>>();
    let text = String::from_utf8(input.text.clone())
        .map_err(|error| format!("I20 fixture text is not UTF-8: {error}"))?;
    Ok(IngestDocument::new(
        DocumentVersion::new(DocId::new(input.doc_id), Revision::new(input.revision)),
        vector,
    )
    .with_timestamp(input.timestamp)
    .with_metadata(input.metadata_sentinel.clone())
    .with_text(text))
}

fn expected_document(input: &DocumentInput, thresholds: &[i64]) -> independent::DocumentFact {
    independent::DocumentFact {
        doc_id: input.doc_id,
        revision: input.revision,
        timestamp_witness: thresholds
            .iter()
            .map(|threshold| input.timestamp <= *threshold)
            .collect(),
    }
}

fn observe_documents(
    store: &Store,
    cardinality: usize,
    thresholds: &[i64],
) -> Result<Vec<independent::DocumentFact>, String> {
    let options = SearchOptions::new(ScanOptions { thread_budget: 1 }).with_tier(SearchTier::Scan);
    // Ask for one more row than the model expects: a resurrected, duplicated,
    // or partially published row must surface as an extra candidate instead
    // of being hidden by the top-k cap.
    let probe_k = cardinality
        .checked_add(1)
        .ok_or_else(|| "I20 probe cardinality overflowed".to_owned())?;
    let outcome = store
        .search(
            SearchRequest::new(&QUERY),
            probe_k,
            options,
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| format!("observe I20 live documents: {error}"))?;
    let mut documents = outcome
        .candidates
        .iter()
        .map(|candidate| {
            candidate
                .document()
                .ok_or_else(|| "I20 public candidate omitted document identity".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    if documents.len() != cardinality {
        return Err(format!(
            "I20 public full scan returned {} documents, expected {cardinality}",
            documents.len()
        ));
    }
    documents.sort_unstable();

    let mut witnesses = Vec::with_capacity(thresholds.len());
    for threshold in thresholds {
        let predicate = Predicate::Range(RangePredicate {
            column: TIMESTAMP_COLUMN,
            lower: None,
            upper: Some(RangeBound::inclusive(PredicateValue::I64(*threshold))),
        });
        let filtered = store
            .search_filtered(
                SearchRequest::new(&QUERY),
                &predicate,
                probe_k,
                options,
                QueryControl::Cancel(CancelToken::new()),
            )
            .map_err(|error| format!("observe I20 timestamp threshold {threshold}: {error}"))?;
        witnesses.push(
            filtered
                .candidates
                .iter()
                .map(|candidate| {
                    candidate.document().ok_or_else(|| {
                        "I20 filtered candidate omitted document identity".to_owned()
                    })
                })
                .collect::<Result<BTreeSet<_>, _>>()?,
        );
    }

    Ok(documents
        .into_iter()
        .map(|document| independent::DocumentFact {
            doc_id: document.doc_id().get(),
            revision: document.revision().get(),
            timestamp_witness: witnesses
                .iter()
                .map(|visible| visible.contains(&document))
                .collect(),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    #[ignore = "subprocess-only helper selected by the parent purge crash test"]
    fn purge_crash_receipt_child_helper() {
        run_i23_purge_crash_child_from_env()
            .expect("I23 purge-crash checkpoint must abort before returning");
    }

    #[test]
    fn clean_batch_commit_uses_public_store_and_exact_i20_checker() {
        let evidence = run_ingest_operation(IngestOperationKind::BatchCommit, 5, None)
            .expect("clean I20 public Store evidence");
        let IngestOperationEvidence::I20(evidence) = evidence else {
            panic!("batch-commit adapter returned the wrong invariant");
        };
        independent::compare_i20(&evidence.expected, &evidence.observed)
            .expect("clean public I20 exact comparison");
        assert_eq!(evidence.fixture.submitted.len(), 2);
        assert_eq!(evidence.observed.generation_after, 4);
    }

    #[test]
    fn post_ack_retry_runs_i20_on_same_seed_pair_and_keeps_product_receipt() {
        let evidence = run_ingest_operation(
            IngestOperationKind::BatchCommit,
            5,
            Some(IngestFaultKind::PostAckRetry),
        )
        .expect("post-ack-retry I20 operation-specific observation and product receipt");
        let IngestOperationEvidence::I20(evidence) = evidence else {
            panic!("post-ack adapter returned the wrong invariant");
        };
        independent::compare_i20(&evidence.expected, &evidence.observed)
            .expect("post-ack retry exact I20 comparison");
        assert_eq!(evidence.receipts.len(), 1);
        let control = evidence.control.expect("post-ack retry same-seed control");
        assert!(control.isolated_directories);
        assert_eq!(
            control.clean_initial_directory,
            control.fault_initial_directory
        );
        assert!(control.passed);
    }

    #[test]
    fn partial_batch_append_runs_rejected_i20_then_clean_retry_with_product_receipt() {
        let evidence = run_ingest_operation(
            IngestOperationKind::BatchCommit,
            5,
            Some(IngestFaultKind::PartialBatchAppend),
        )
        .expect("partial-batch-append I20 rejected observation, clean retry, and receipt");
        let IngestOperationEvidence::I20(evidence) = evidence else {
            panic!("partial-batch adapter returned the wrong invariant");
        };
        independent::compare_i20(&evidence.expected, &evidence.observed)
            .expect("partial-batch-append exact I20 comparison");
        assert_eq!(evidence.receipts.len(), 1);
        let control = evidence.control.expect("partial-batch same-seed control");
        assert!(control.isolated_directories);
        assert_eq!(
            control.clean_initial_directory,
            control.fault_initial_directory
        );
        assert!(control.passed);
    }

    #[test]
    fn clean_seal_uses_public_store_and_exact_i21_checker() {
        let evidence = run_ingest_operation(IngestOperationKind::Seal, 5, None)
            .expect("clean I21 public Store evidence");
        let IngestOperationEvidence::I21(evidence) = evidence else {
            panic!("seal adapter returned the wrong invariant");
        };
        independent::compare_i21(&evidence.expected, &evidence.observed)
            .expect("clean public I21 exact comparison");
        assert_eq!(evidence.expected.live_after.len(), 2);
        assert_eq!(evidence.observed.active_rows_before, 3);
        assert_eq!(evidence.observed.active_rows_after, 0);
    }

    #[test]
    fn late_seal_cancellation_runs_i21_on_same_seed_pair_and_keeps_product_receipt() {
        let evidence = run_ingest_operation(
            IngestOperationKind::Seal,
            5,
            Some(IngestFaultKind::SealCancellation),
        )
        .expect("late seal cancellation I21 evidence");
        let IngestOperationEvidence::I21(evidence) = evidence else {
            panic!("seal-cancellation adapter returned the wrong invariant");
        };
        independent::compare_i21(&evidence.expected, &evidence.observed)
            .expect("late seal cancellation exact I21 comparison");
        assert_eq!(evidence.receipts.len(), 1);
        let control = evidence
            .control
            .expect("I21 cancellation same-seed control");
        assert!(control.isolated_directories);
        assert_eq!(
            control.clean_initial_directory,
            control.fault_initial_directory
        );
        assert!(control.passed);
    }

    #[test]
    fn clean_retention_uses_public_store_and_exact_i22_checker() {
        let evidence = run_ingest_operation(IngestOperationKind::Retention, 5, None)
            .expect("clean I22 public Store evidence");
        let IngestOperationEvidence::I22(evidence) = evidence else {
            panic!("retention adapter returned the wrong invariant");
        };
        independent::compare_i22(&evidence.expected, &evidence.observed)
            .expect("clean public I22 exact comparison");
        assert_eq!(evidence.observed.dropped_labels, ["before"]);
        assert_eq!(evidence.observed.straddler_labels, ["straddler"]);
        assert!(evidence.observed.active_control_present);
    }

    #[test]
    fn retention_clock_boundary_runs_i22_on_same_seed_pair_and_keeps_product_receipt() {
        let evidence = run_ingest_operation(
            IngestOperationKind::Retention,
            5,
            Some(IngestFaultKind::RetentionClockBoundary),
        )
        .expect("retention-clock-boundary I22 evidence");
        let IngestOperationEvidence::I22(evidence) = evidence else {
            panic!("retention-clock adapter returned the wrong invariant");
        };
        independent::compare_i22(&evidence.expected, &evidence.observed)
            .expect("retention-clock-boundary exact I22 comparison");
        assert_eq!(evidence.receipts.len(), 1);
    }

    #[test]
    fn retained_ingest_fixture_executes_literal_inputs_without_seed_regeneration() {
        let original = run_ingest_operation(IngestOperationKind::Retention, 5, None)
            .expect("original retained I22 operation");
        let retained = RetainedIngestOperationV1::from_evidence(
            IngestOperationKind::Retention,
            None,
            17,
            &original,
        )
        .expect("construct retained ingest fixture");
        let encoded = encode_ingest_fixture(&retained).expect("encode retained ingest fixture");
        let mut decoded = decode_ingest_fixture(&encoded).expect("decode retained ingest fixture");
        let RetainedIngestFixtureV1::I22(fixture) = &mut decoded.fixture else {
            panic!("retained retention operation decoded the wrong fixture variant");
        };
        let literal_documents = fixture
            .partitions
            .iter()
            .flat_map(|partition| &partition.documents)
            .map(|document| document.doc_id)
            .collect::<Vec<_>>();
        fixture.seed = 9_005;
        let mutated = encode_ingest_fixture(&decoded).expect("re-encode retained literal fixture");
        let replayed = run_ingest_operation_from_fixture(&mutated)
            .expect("execute retained ingest fixture without generator");
        let IngestOperationEvidence::I22(replayed) = replayed else {
            panic!("retained retention execution returned the wrong invariant");
        };
        let replayed_documents = replayed
            .fixture
            .partitions
            .iter()
            .flat_map(|partition| &partition.documents)
            .map(|document| document.doc_id)
            .collect::<Vec<_>>();
        assert_eq!(replayed.fixture.seed, 9_005);
        assert_eq!(replayed_documents, literal_documents);
        assert_ne!(
            replayed_documents[0],
            i22_fixture(9_005).unwrap().partitions[0].documents[0].doc_id
        );
        independent::compare_i22(&replayed.expected, &replayed.observed)
            .expect("retained public I22 exact comparison");
    }

    #[test]
    fn retained_ingest_fixture_roundtrips_and_executes_every_operation_and_fault() {
        let cases = [
            (IngestOperationKind::BatchCommit, None, 4),
            (
                IngestOperationKind::BatchCommit,
                Some(IngestFaultKind::PostAckRetry),
                5,
            ),
            (
                IngestOperationKind::BatchCommit,
                Some(IngestFaultKind::PartialBatchAppend),
                5,
            ),
            (IngestOperationKind::Seal, None, 5),
            (
                IngestOperationKind::Seal,
                Some(IngestFaultKind::SealCancellation),
                5,
            ),
            (IngestOperationKind::Retention, None, 5),
            (
                IngestOperationKind::Retention,
                Some(IngestFaultKind::RetentionClockBoundary),
                5,
            ),
            (IngestOperationKind::Purge, None, 5),
            (
                IngestOperationKind::Purge,
                Some(IngestFaultKind::PurgeUnlinkError),
                5,
            ),
            (
                IngestOperationKind::Purge,
                Some(IngestFaultKind::PurgeCrashBoundary),
                6,
            ),
        ];
        for (case_index, (operation, fault, seed)) in cases.into_iter().enumerate() {
            let original = run_ingest_operation(operation, seed, fault)
                .unwrap_or_else(|error| panic!("original {operation:?}/{fault:?}: {error}"));
            let invocation_id = 700_u64.saturating_add(case_index as u64);
            let retained = RetainedIngestOperationV1::from_evidence(
                operation,
                fault,
                invocation_id,
                &original,
            )
            .unwrap_or_else(|error| panic!("retain {operation:?}/{fault:?}: {error}"));
            let encoded = encode_ingest_fixture(&retained)
                .unwrap_or_else(|error| panic!("encode {operation:?}/{fault:?}: {error}"));
            assert_eq!(
                decode_ingest_fixture(&encoded).expect("decode retained ingest operation"),
                retained
            );
            let replayed = run_ingest_operation_from_fixture(&encoded)
                .unwrap_or_else(|error| panic!("execute {operation:?}/{fault:?}: {error}"));
            match replayed {
                IngestOperationEvidence::I20(evidence) => {
                    independent::compare_i20(&evidence.expected, &evidence.observed)
                }
                IngestOperationEvidence::I21(evidence) => {
                    independent::compare_i21(&evidence.expected, &evidence.observed)
                }
                IngestOperationEvidence::I22(evidence) => {
                    independent::compare_i22(&evidence.expected, &evidence.observed)
                }
                IngestOperationEvidence::I23(evidence) => {
                    independent::compare_i23(&evidence.expected, &evidence.observed)
                }
            }
            .unwrap_or_else(|error| panic!("compare {operation:?}/{fault:?}: {error}"));
        }
    }

    #[test]
    fn clean_physical_purge_uses_public_store_bytes_and_exact_i23_checker() {
        for seed in [4, 5] {
            let evidence = run_ingest_operation(IngestOperationKind::Purge, seed, None)
                .expect("clean I23 public Store evidence");
            let IngestOperationEvidence::I23(evidence) = evidence else {
                panic!("purge adapter returned the wrong invariant");
            };
            independent::compare_i23(&evidence.expected, &evidence.observed)
                .expect("clean public I23 exact comparison");
            assert!(!evidence.observed.pre_purge_hits.is_empty());
            assert!(evidence.observed.post_purge_hits.is_empty());
            assert!(!evidence.observed.intent_present);
        }
    }

    #[test]
    fn purge_unlink_error_runs_i23_recovery_on_same_seed_pair_with_product_receipt() {
        let evidence = run_ingest_operation(
            IngestOperationKind::Purge,
            5,
            Some(IngestFaultKind::PurgeUnlinkError),
        )
        .expect("purge-unlink-error I23 recovery evidence");
        let IngestOperationEvidence::I23(evidence) = evidence else {
            panic!("purge-unlink adapter returned the wrong invariant");
        };
        independent::compare_i23(&evidence.expected, &evidence.observed)
            .expect("purge-unlink exact I23 comparison");
        assert_eq!(evidence.receipts.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn purge_crash_boundary_runs_i23_recovery_on_same_seed_pair_with_durable_product_receipt() {
        let evidence = run_ingest_operation(
            IngestOperationKind::Purge,
            6,
            Some(IngestFaultKind::PurgeCrashBoundary),
        )
        .expect("purge-crash-boundary I23 recovery evidence");
        let IngestOperationEvidence::I23(evidence) = evidence else {
            panic!("purge-crash adapter returned the wrong invariant");
        };
        independent::compare_i23(&evidence.expected, &evidence.observed)
            .expect("purge-crash exact I23 comparison");
        assert_eq!(evidence.receipts.len(), 1);
        assert!(
            evidence
                .observed
                .fault_error
                .as_deref()
                .is_some_and(|error| error.contains("SIGABRT"))
        );
    }
}
