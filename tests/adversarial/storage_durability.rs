//! Family-owned storage durability fixtures and typed observation adapters.
//!
//! This module deliberately does not dispatch campaign operations, invoke the
//! I15-I19 checkers, or construct shared `OracleRecord`s. It only creates the
//! seed-derived public Store fixtures and returns independent-oracle DTOs plus
//! receipts emitted by product test-support seams.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tempfile::{TempDir, tempdir};
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, IngestError, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode};
use zeppelin_embed::lifecycle::{
    CancelToken, ManualMonotonicClock, OpenOptions, QueryControl, QueryError, SearchOptions,
    SearchTier, StorageCleanupReport, StorageFaultController, StorageFaultPlan,
    StorageFaultReceipt, StorageReceiptObserved, StorageReceiptSite, StorageTestFault, Store,
    StoreError, StoreTestDependencies,
};
use zeppelin_embed::meta::{ColumnDefinition, ColumnId, ColumnType, PredicateValue, Schema};
use zeppelin_embed::scan::ScanOptions;
use zeppelin_embed::segment::layout::RegionKind;
use zeppelin_embed::vfs::StdVfs;
use zeppelin_embed_adversarial_oracle::storage_durability as independent;

const MANIFEST: &str = "manifest.ze";
const WAL: &str = "wal.ze";
const STORAGE_CHILD_DIRECTORY: &str = "ZE_STORAGE_ADAPTER_CHILD_DIRECTORY";
const STORAGE_CHILD_FAULT: &str = "ZE_STORAGE_ADAPTER_CHILD_FAULT";
const STORAGE_CHILD_OP_INDEX: &str = "ZE_STORAGE_ADAPTER_CHILD_OP_INDEX";
const STORAGE_CHILD_SEGMENT: &str = "ZE_STORAGE_ADAPTER_CHILD_SEGMENT";
#[cfg(unix)]
const SIGABRT_SIGNAL: i32 = 6;
/// Exact ignored test path the adapter self-spawns for publication checkpoints.
#[cfg(unix)]
pub const PUBLICATION_CHILD_TEST_NAME: &str =
    "adversarial::storage_durability::tests::publication_abort_child";

/// Storage fault selected by the shared campaign catalog.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageFaultKind {
    TornWalHeader,
    TornWalBody,
    TornWalChecksum,
    PostCommitError,
    ManifestPreRenameCrash,
    ManifestPostRenameCrash,
    CorruptSegmentRegion,
    WrongManifestObject,
    WrongSegmentObject,
    ListDeleteOmission,
}

/// Exact mutation authored by the family fixture before the public call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageMutationEvidence {
    pub artifact: String,
    pub offset: Option<u64>,
    pub segment: Option<[u8; 16]>,
    pub region_kind: Option<u16>,
    pub chunk: Option<u32>,
    pub before: Option<u8>,
    pub after: Option<u8>,
}

/// Canonical same-seed control relation carried by every operation adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageControlEvidence {
    pub namespace: &'static str,
    pub seed: u64,
    pub operation_fixture_id: [u8; 16],
    pub clean_fault_pair_id: [u8; 16],
    pub pre_clean_inventory: Vec<independent::FileFact>,
    pub pre_fault_inventory: Vec<independent::FileFact>,
    pub pre_clean_digest: [u8; 32],
    pub pre_fault_digest: [u8; 32],
    pub pre_clean_artifacts: Vec<StorageArtifactEvidence>,
    pub pre_fault_artifacts: Vec<StorageArtifactEvidence>,
    pub clean_inventory: Vec<independent::FileFact>,
    pub fault_inventory: Vec<independent::FileFact>,
    pub clean_digest: [u8; 32],
    pub fault_digest: [u8; 32],
}

/// One actual public mutation result retained as the I16 acknowledgement ledger.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageAckEvidence {
    pub phase: &'static str,
    pub operation_id: [u8; 16],
    pub canonical_request_digest: [u8; 32],
    pub planned_first_seq: u64,
    pub planned_last_seq: u64,
    pub returned_seq: Option<u64>,
    pub returned_generation: Option<u64>,
    pub returned_ok: bool,
    pub acknowledged: bool,
    pub durability: &'static str,
    pub commit_tier: &'static str,
}

/// One normalized persisted artifact plus optional replay bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageArtifactEvidence {
    pub role: &'static str,
    pub fact: independent::FileFact,
    pub bytes: Vec<u8>,
}

/// Canonical family-owned material required by shared artifact writers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageEvidenceEnvelope {
    pub fixture: independent::StorageFixtureV1,
    pub ack_ledger: Vec<StorageAckEvidence>,
    pub artifacts: Vec<StorageArtifactEvidence>,
}

/// Stable storage operation-fixture order for one episode.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum StorageOperationKind {
    WalPrefix,
    Publication,
    Retry,
    FormatCheck,
    OrphanCleanup,
}

/// Every operation root forked from one closed durable episode base.
pub const STORAGE_OPERATION_KINDS: [StorageOperationKind; 5] = [
    StorageOperationKind::WalPrefix,
    StorageOperationKind::Publication,
    StorageOperationKind::Retry,
    StorageOperationKind::FormatCheck,
    StorageOperationKind::OrphanCleanup,
];

impl StorageOperationKind {
    /// Stable operation key used by shared evidence serialization.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::WalPrefix => "wal-prefix",
            Self::Publication => "publication",
            Self::Retry => "retry",
            Self::FormatCheck => "format-check",
            Self::OrphanCleanup => "orphan-cleanup",
        }
    }

    const fn artifact_role(self) -> &'static str {
        match self {
            Self::WalPrefix => "wal-prefix-operation-fixture",
            Self::Publication => "publication-operation-fixture",
            Self::Retry => "retry-operation-fixture",
            Self::FormatCheck => "format-check-operation-fixture",
            Self::OrphanCleanup => "orphan-cleanup-operation-fixture",
        }
    }
}

/// Stable binary envelope for one literal retained storage operation.
pub const STORAGE_RETAINED_FIXTURE_SCHEMA: &str = "zeppelin-storage-retained-fixture-v1";
/// Current retained storage fixture envelope version.
pub const STORAGE_RETAINED_FIXTURE_VERSION: u16 = 1;
const STORAGE_RETAINED_FIXTURE_MAGIC: &[u8; 8] = b"ZESTOR01";
const STORAGE_RETAINED_FIXTURE_HEADER_LEN: usize = 48;
const STORAGE_RETAINED_FIXTURE_DIGEST_DOMAIN: u64 = 0x5354_4f52_5245_504c;
const STORAGE_RETAINED_FIXTURE_MAX_BYTES: usize = 16 * 1024 * 1024;
const STORAGE_RETAINED_FIXTURE_MAX_ITEMS: usize = 4_096;

/// Complete primitive fixture plus the exact operation/fault/subcase schedule.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetainedStorageOperationV1 {
    pub fixture: independent::StorageFixtureV1,
    pub operation: StorageOperationKind,
    pub op_index: u32,
    pub fault: Option<StorageFaultKind>,
    pub format_case: Option<independent::FormatCase>,
    pub omission_case: Option<independent::OmissionCase>,
}

impl RetainedStorageOperationV1 {
    /// Creates one validated literal replay schedule without deriving from its seed.
    pub fn new(
        fixture: independent::StorageFixtureV1,
        operation: StorageOperationKind,
        op_index: u32,
        fault: Option<StorageFaultKind>,
        format_case: Option<independent::FormatCase>,
        omission_case: Option<independent::OmissionCase>,
    ) -> Result<Self, String> {
        let retained = Self {
            fixture,
            operation,
            op_index,
            fault,
            format_case,
            omission_case,
        };
        retained.validate()?;
        Ok(retained)
    }

    fn validate(&self) -> Result<(), String> {
        validate_retained_storage_fixture(&self.fixture)?;
        let no_subcases = || {
            if self.format_case.is_some() || self.omission_case.is_some() {
                Err(format!(
                    "retained {} schedule contains an unrelated subcase",
                    self.operation.key()
                ))
            } else {
                Ok(())
            }
        };
        match self.operation {
            StorageOperationKind::WalPrefix => {
                if self.omission_case.is_some() {
                    return Err("retained wal-prefix schedule contains an omission case".to_owned());
                }
                let expected_case = match self.fault {
                    None => None,
                    Some(StorageFaultKind::TornWalHeader) => {
                        Some(independent::FormatCase::WalHeader)
                    }
                    Some(StorageFaultKind::TornWalBody) => {
                        Some(independent::FormatCase::WalRecordBody)
                    }
                    Some(StorageFaultKind::TornWalChecksum) => {
                        Some(independent::FormatCase::WalRecordChecksum)
                    }
                    Some(other) => {
                        return Err(format!(
                            "retained {other:?} fault does not belong to wal-prefix"
                        ));
                    }
                };
                if self.format_case != expected_case {
                    return Err(format!(
                        "retained wal-prefix format subcase differs expected={expected_case:?} observed={:?}",
                        self.format_case
                    ));
                }
                Ok(())
            }
            StorageOperationKind::Publication => {
                no_subcases()?;
                if matches!(
                    self.fault,
                    None | Some(StorageFaultKind::ManifestPreRenameCrash)
                        | Some(StorageFaultKind::ManifestPostRenameCrash)
                ) {
                    Ok(())
                } else {
                    Err(format!(
                        "retained {:?} fault does not belong to publication",
                        self.fault
                    ))
                }
            }
            StorageOperationKind::Retry => {
                no_subcases()?;
                if matches!(self.fault, None | Some(StorageFaultKind::PostCommitError)) {
                    Ok(())
                } else {
                    Err(format!(
                        "retained {:?} fault does not belong to retry",
                        self.fault
                    ))
                }
            }
            StorageOperationKind::FormatCheck => {
                if self.omission_case.is_some() {
                    return Err(
                        "retained format-check schedule contains an omission case".to_owned()
                    );
                }
                let case = self
                    .format_case
                    .ok_or_else(|| "retained format-check schedule omits its case".to_owned())?;
                let valid = match self.fault {
                    None => matches!(
                        case,
                        independent::FormatCase::SegmentRegion
                            | independent::FormatCase::ManifestWrongFamily
                            | independent::FormatCase::SegmentWrongFamily
                            | independent::FormatCase::SegmentWrongIdentity
                    ),
                    Some(StorageFaultKind::CorruptSegmentRegion) => {
                        case == independent::FormatCase::SegmentRegion
                    }
                    Some(StorageFaultKind::WrongManifestObject) => {
                        case == independent::FormatCase::ManifestWrongFamily
                    }
                    Some(StorageFaultKind::WrongSegmentObject) => matches!(
                        case,
                        independent::FormatCase::SegmentWrongFamily
                            | independent::FormatCase::SegmentWrongIdentity
                    ),
                    Some(_) => false,
                };
                if valid {
                    Ok(())
                } else {
                    Err(format!(
                        "retained format-check fault/case identity differs fault={:?} case={case:?}",
                        self.fault
                    ))
                }
            }
            StorageOperationKind::OrphanCleanup => {
                if self.format_case.is_some() {
                    return Err(
                        "retained orphan-cleanup schedule contains a format case".to_owned()
                    );
                }
                match (self.fault, self.omission_case) {
                    (None, None) | (Some(StorageFaultKind::ListDeleteOmission), Some(_)) => Ok(()),
                    (Some(StorageFaultKind::ListDeleteOmission), None) => Err(
                        "retained list-delete omission schedule omits its exact case".to_owned(),
                    ),
                    (None, Some(_)) => Err(
                        "retained clean orphan-cleanup schedule contains an omission case"
                            .to_owned(),
                    ),
                    (Some(other), _) => Err(format!(
                        "retained {other:?} fault does not belong to orphan-cleanup"
                    )),
                }
            }
        }
    }
}

fn validate_retained_storage_fixture(
    fixture: &independent::StorageFixtureV1,
) -> Result<(), String> {
    if fixture.namespace != "adversarial::storage-durability::v1"
        || fixture.durability != "Durable"
        || fixture.commit_tier != "Ordered"
    {
        return Err("retained storage fixture has an unknown namespace or durability".to_owned());
    }
    if fixture.scheme != 4 {
        return Err(format!(
            "retained storage fixture scheme is unsupported: {}",
            fixture.scheme
        ));
    }
    if fixture.documents.len() < 3
        || fixture.documents.len() > STORAGE_RETAINED_FIXTURE_MAX_ITEMS
        || fixture.documents.len() != fixture.mutations.len()
    {
        return Err("retained storage fixture document/mutation cardinality differs".to_owned());
    }
    let dimensions = usize::try_from(fixture.dimensions)
        .map_err(|_| "retained storage dimensions exceed usize".to_owned())?;
    if dimensions == 0 || dimensions > STORAGE_RETAINED_FIXTURE_MAX_ITEMS {
        return Err("retained storage fixture dimensions are out of bounds".to_owned());
    }
    for (index, (document, mutation)) in
        fixture.documents.iter().zip(&fixture.mutations).enumerate()
    {
        if document.vector_bits.len() != dimensions {
            return Err(format!(
                "retained storage document {index} dimensions differ"
            ));
        }
        if mutation.document_index != u32::try_from(index).unwrap_or(u32::MAX)
            || mutation.first_seq != u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1)
            || mutation.last_seq != mutation.first_seq
            || mutation.acknowledged != (index + 1 != fixture.documents.len())
            || mutation.canonical_payload_digest != canonical_document_digest(document)?
        {
            return Err(format!(
                "retained storage mutation {index} differs from its literal document"
            ));
        }
    }
    Ok(())
}

fn append_retained_bytes(output: &mut Vec<u8>, bytes: &[u8]) -> Result<(), String> {
    let length =
        u32::try_from(bytes.len()).map_err(|_| "retained storage value exceeds u32".to_owned())?;
    output.extend_from_slice(&length.to_le_bytes());
    output.extend_from_slice(bytes);
    Ok(())
}

fn append_retained_optional_bytes(
    output: &mut Vec<u8>,
    bytes: Option<&[u8]>,
) -> Result<(), String> {
    match bytes {
        Some(bytes) => {
            output.push(1);
            append_retained_bytes(output, bytes)
        }
        None => {
            output.push(0);
            Ok(())
        }
    }
}

fn append_retained_strings(output: &mut Vec<u8>, values: &[String]) -> Result<(), String> {
    output.extend_from_slice(
        &u32::try_from(values.len())
            .map_err(|_| "retained storage string count exceeds u32".to_owned())?
            .to_le_bytes(),
    );
    for value in values {
        append_retained_bytes(output, value.as_bytes())?;
    }
    Ok(())
}

const fn retained_operation_tag(operation: StorageOperationKind) -> u8 {
    match operation {
        StorageOperationKind::WalPrefix => 1,
        StorageOperationKind::Publication => 2,
        StorageOperationKind::Retry => 3,
        StorageOperationKind::FormatCheck => 4,
        StorageOperationKind::OrphanCleanup => 5,
    }
}

const fn retained_fault_tag(fault: Option<StorageFaultKind>) -> u8 {
    match fault {
        None => 0,
        Some(StorageFaultKind::TornWalHeader) => 1,
        Some(StorageFaultKind::TornWalBody) => 2,
        Some(StorageFaultKind::TornWalChecksum) => 3,
        Some(StorageFaultKind::PostCommitError) => 4,
        Some(StorageFaultKind::ManifestPreRenameCrash) => 5,
        Some(StorageFaultKind::ManifestPostRenameCrash) => 6,
        Some(StorageFaultKind::CorruptSegmentRegion) => 7,
        Some(StorageFaultKind::WrongManifestObject) => 8,
        Some(StorageFaultKind::WrongSegmentObject) => 9,
        Some(StorageFaultKind::ListDeleteOmission) => 10,
    }
}

const fn retained_format_case_tag(case: Option<independent::FormatCase>) -> u8 {
    match case {
        None => 0,
        Some(independent::FormatCase::WalHeader) => 1,
        Some(independent::FormatCase::WalRecordBody) => 2,
        Some(independent::FormatCase::WalRecordChecksum) => 3,
        Some(independent::FormatCase::SegmentRegion) => 4,
        Some(independent::FormatCase::ManifestWrongFamily) => 5,
        Some(independent::FormatCase::SegmentWrongFamily) => 6,
        Some(independent::FormatCase::SegmentWrongIdentity) => 7,
    }
}

fn append_retained_fixture_payload(
    output: &mut Vec<u8>,
    retained: &RetainedStorageOperationV1,
) -> Result<(), String> {
    let fixture = &retained.fixture;
    output.push(retained_operation_tag(retained.operation));
    output.extend_from_slice(&retained.op_index.to_le_bytes());
    output.push(retained_fault_tag(retained.fault));
    output.push(retained_format_case_tag(retained.format_case));
    match retained.omission_case {
        None => output.extend_from_slice(&[0, 0]),
        Some(case) => {
            output.push(match case.orphan {
                independent::OrphanKind::FinalSegment => 1,
                independent::OrphanKind::SegmentTemporary => 2,
                independent::OrphanKind::ManifestTemporary => 3,
            });
            output.push(match case.subsite {
                independent::OmissionSubsite::List => 1,
                independent::OmissionSubsite::Delete => 2,
            });
        }
    }
    append_retained_bytes(output, fixture.namespace.as_bytes())?;
    output.extend_from_slice(&fixture.seed.to_le_bytes());
    append_retained_bytes(output, fixture.durability.as_bytes())?;
    append_retained_bytes(output, fixture.commit_tier.as_bytes())?;
    output.extend_from_slice(&fixture.dimensions.to_le_bytes());
    output.extend_from_slice(&fixture.scheme.to_le_bytes());
    output.extend_from_slice(
        &u32::try_from(fixture.documents.len())
            .map_err(|_| "retained storage document count exceeds u32".to_owned())?
            .to_le_bytes(),
    );
    for document in &fixture.documents {
        output.extend_from_slice(&document.doc_id.to_le_bytes());
        output.extend_from_slice(&document.revision.to_le_bytes());
        output.extend_from_slice(
            &u32::try_from(document.vector_bits.len())
                .map_err(|_| "retained storage vector length exceeds u32".to_owned())?
                .to_le_bytes(),
        );
        for bits in &document.vector_bits {
            output.extend_from_slice(&bits.to_le_bytes());
        }
        match document.timestamp {
            Some(timestamp) => {
                output.push(1);
                output.extend_from_slice(&timestamp.to_le_bytes());
            }
            None => output.push(0),
        }
        append_retained_optional_bytes(output, document.metadata.as_deref())?;
        append_retained_optional_bytes(output, document.text.as_deref().map(str::as_bytes))?;
        output.extend_from_slice(
            &u32::try_from(document.columns.len())
                .map_err(|_| "retained storage column count exceeds u32".to_owned())?
                .to_le_bytes(),
        );
        for (column, value) in &document.columns {
            output.extend_from_slice(&column.to_le_bytes());
            match value {
                independent::FixtureColumnValueV1::I64(value) => {
                    output.push(1);
                    output.extend_from_slice(&value.to_le_bytes());
                }
                independent::FixtureColumnValueV1::F64Bits(value) => {
                    output.push(2);
                    output.extend_from_slice(&value.to_le_bytes());
                }
                independent::FixtureColumnValueV1::Bool(value) => {
                    output.extend_from_slice(&[3, u8::from(*value)]);
                }
                independent::FixtureColumnValueV1::Bytes(value) => {
                    output.push(4);
                    append_retained_bytes(output, value)?;
                }
            }
        }
    }
    output.extend_from_slice(
        &u32::try_from(fixture.mutations.len())
            .map_err(|_| "retained storage mutation count exceeds u32".to_owned())?
            .to_le_bytes(),
    );
    for mutation in &fixture.mutations {
        output.extend_from_slice(&mutation.operation_id);
        output.extend_from_slice(&mutation.document_index.to_le_bytes());
        output.extend_from_slice(&mutation.canonical_payload_digest);
        output.extend_from_slice(&mutation.first_seq.to_le_bytes());
        output.extend_from_slice(&mutation.last_seq.to_le_bytes());
        output.push(u8::from(mutation.acknowledged));
    }
    output.extend_from_slice(&fixture.old_generation.to_le_bytes());
    output.extend_from_slice(&fixture.planned_new_generation.to_le_bytes());
    output.extend_from_slice(&fixture.absorbed_through.to_le_bytes());
    output.extend_from_slice(&fixture.wal_mutation_offset.to_le_bytes());
    output.extend_from_slice(&fixture.segment_region_kind.to_le_bytes());
    output.extend_from_slice(&fixture.segment_chunk.to_le_bytes());
    output.extend_from_slice(&fixture.segment_byte.to_le_bytes());
    output.push(match fixture.omission_orphan {
        independent::OrphanKind::FinalSegment => 1,
        independent::OrphanKind::SegmentTemporary => 2,
        independent::OrphanKind::ManifestTemporary => 3,
    });
    output.push(u8::from(fixture.omission_is_delete));
    append_retained_strings(output, &fixture.eligible_orphans)?;
    append_retained_strings(output, &fixture.preserved_files)?;
    output.extend_from_slice(&fixture.clean_fault_pair_id);
    output.extend_from_slice(&fixture.operation_fixture_id);
    Ok(())
}

/// Encodes a checksummed, versioned literal storage replay fixture.
pub fn encode_storage_fixture(retained: &RetainedStorageOperationV1) -> Result<Vec<u8>, String> {
    retained.validate()?;
    let mut payload = Vec::new();
    append_retained_fixture_payload(&mut payload, retained)?;
    if payload.len() > STORAGE_RETAINED_FIXTURE_MAX_BYTES {
        return Err("retained storage fixture payload exceeds the byte limit".to_owned());
    }
    let mut output = Vec::with_capacity(STORAGE_RETAINED_FIXTURE_HEADER_LEN + payload.len());
    output.extend_from_slice(STORAGE_RETAINED_FIXTURE_MAGIC);
    output.extend_from_slice(&STORAGE_RETAINED_FIXTURE_VERSION.to_le_bytes());
    output.extend_from_slice(&0_u16.to_le_bytes());
    output.extend_from_slice(
        &u32::try_from(payload.len())
            .map_err(|_| "retained storage fixture payload exceeds u32".to_owned())?
            .to_le_bytes(),
    );
    output.extend_from_slice(&digest32(STORAGE_RETAINED_FIXTURE_DIGEST_DOMAIN, &payload));
    output.extend_from_slice(&payload);
    Ok(output)
}

struct RetainedStorageDecoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> RetainedStorageDecoder<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], String> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| "retained storage fixture offset overflowed".to_owned())?;
        let value = self.bytes.get(self.offset..end).ok_or_else(|| {
            format!(
                "retained storage fixture is truncated at byte {}",
                self.offset
            )
        })?;
        self.offset = end;
        Ok(value)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], String> {
        self.take(N)?
            .try_into()
            .map_err(|_| format!("retained storage fixture expected {N} bytes"))
    }

    fn u8(&mut self) -> Result<u8, String> {
        self.array::<1>().map(|value| value[0])
    }

    fn bool(&mut self) -> Result<bool, String> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            value => Err(format!(
                "retained storage fixture has invalid boolean tag {value}"
            )),
        }
    }

    fn u16(&mut self) -> Result<u16, String> {
        self.array().map(u16::from_le_bytes)
    }

    fn u32(&mut self) -> Result<u32, String> {
        self.array().map(u32::from_le_bytes)
    }

    fn u64(&mut self) -> Result<u64, String> {
        self.array().map(u64::from_le_bytes)
    }

    fn u128(&mut self) -> Result<u128, String> {
        self.array().map(u128::from_le_bytes)
    }

    fn length(&mut self, label: &str) -> Result<usize, String> {
        let length = usize::try_from(self.u32()?)
            .map_err(|_| format!("retained storage {label} length exceeds usize"))?;
        if length > STORAGE_RETAINED_FIXTURE_MAX_ITEMS {
            return Err(format!("retained storage {label} length exceeds limit"));
        }
        Ok(length)
    }

    fn bytes(&mut self, label: &str) -> Result<Vec<u8>, String> {
        let length = usize::try_from(self.u32()?)
            .map_err(|_| format!("retained storage {label} byte length exceeds usize"))?;
        if length > STORAGE_RETAINED_FIXTURE_MAX_BYTES {
            return Err(format!(
                "retained storage {label} byte length exceeds limit"
            ));
        }
        self.take(length).map(<[u8]>::to_vec)
    }

    fn string(&mut self, label: &str) -> Result<String, String> {
        String::from_utf8(self.bytes(label)?)
            .map_err(|_| format!("retained storage {label} is not UTF-8"))
    }

    fn optional_bytes(&mut self, label: &str) -> Result<Option<Vec<u8>>, String> {
        match self.u8()? {
            0 => Ok(None),
            1 => self.bytes(label).map(Some),
            tag => Err(format!(
                "retained storage {label} has invalid optional tag {tag}"
            )),
        }
    }

    fn strings(&mut self, label: &str) -> Result<Vec<String>, String> {
        let length = self.length(label)?;
        (0..length)
            .map(|_| self.string(label))
            .collect::<Result<Vec<_>, _>>()
    }

    fn finish(self) -> Result<(), String> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(format!(
                "retained storage fixture has trailing bytes at offset {}",
                self.offset
            ))
        }
    }
}

fn decode_retained_operation(tag: u8) -> Result<StorageOperationKind, String> {
    match tag {
        1 => Ok(StorageOperationKind::WalPrefix),
        2 => Ok(StorageOperationKind::Publication),
        3 => Ok(StorageOperationKind::Retry),
        4 => Ok(StorageOperationKind::FormatCheck),
        5 => Ok(StorageOperationKind::OrphanCleanup),
        _ => Err(format!(
            "retained storage fixture has unknown operation tag {tag}"
        )),
    }
}

fn decode_retained_fault(tag: u8) -> Result<Option<StorageFaultKind>, String> {
    let fault = match tag {
        0 => return Ok(None),
        1 => StorageFaultKind::TornWalHeader,
        2 => StorageFaultKind::TornWalBody,
        3 => StorageFaultKind::TornWalChecksum,
        4 => StorageFaultKind::PostCommitError,
        5 => StorageFaultKind::ManifestPreRenameCrash,
        6 => StorageFaultKind::ManifestPostRenameCrash,
        7 => StorageFaultKind::CorruptSegmentRegion,
        8 => StorageFaultKind::WrongManifestObject,
        9 => StorageFaultKind::WrongSegmentObject,
        10 => StorageFaultKind::ListDeleteOmission,
        _ => {
            return Err(format!(
                "retained storage fixture has unknown fault tag {tag}"
            ));
        }
    };
    Ok(Some(fault))
}

fn decode_retained_format_case(tag: u8) -> Result<Option<independent::FormatCase>, String> {
    let case = match tag {
        0 => return Ok(None),
        1 => independent::FormatCase::WalHeader,
        2 => independent::FormatCase::WalRecordBody,
        3 => independent::FormatCase::WalRecordChecksum,
        4 => independent::FormatCase::SegmentRegion,
        5 => independent::FormatCase::ManifestWrongFamily,
        6 => independent::FormatCase::SegmentWrongFamily,
        7 => independent::FormatCase::SegmentWrongIdentity,
        _ => {
            return Err(format!(
                "retained storage fixture has unknown format-case tag {tag}"
            ));
        }
    };
    Ok(Some(case))
}

fn decode_retained_omission_case(
    orphan: u8,
    subsite: u8,
) -> Result<Option<independent::OmissionCase>, String> {
    if orphan == 0 && subsite == 0 {
        return Ok(None);
    }
    let orphan = match orphan {
        1 => independent::OrphanKind::FinalSegment,
        2 => independent::OrphanKind::SegmentTemporary,
        3 => independent::OrphanKind::ManifestTemporary,
        _ => {
            return Err(format!(
                "retained storage fixture has unknown omission-orphan tag {orphan}"
            ));
        }
    };
    let subsite = match subsite {
        1 => independent::OmissionSubsite::List,
        2 => independent::OmissionSubsite::Delete,
        _ => {
            return Err(format!(
                "retained storage fixture has unknown omission-subsite tag {subsite}"
            ));
        }
    };
    Ok(Some(independent::OmissionCase { orphan, subsite }))
}

fn decode_retained_fixture_payload(payload: &[u8]) -> Result<RetainedStorageOperationV1, String> {
    let mut decoder = RetainedStorageDecoder::new(payload);
    let operation = decode_retained_operation(decoder.u8()?)?;
    let op_index = decoder.u32()?;
    let fault = decode_retained_fault(decoder.u8()?)?;
    let format_case = decode_retained_format_case(decoder.u8()?)?;
    let omission_case = decode_retained_omission_case(decoder.u8()?, decoder.u8()?)?;
    let namespace = decoder.string("namespace")?;
    if namespace != "adversarial::storage-durability::v1" {
        return Err("retained storage fixture namespace is unknown".to_owned());
    }
    let seed = decoder.u64()?;
    let durability = decoder.string("durability")?;
    if durability != "Durable" {
        return Err("retained storage fixture durability is unknown".to_owned());
    }
    let commit_tier = decoder.string("commit tier")?;
    if commit_tier != "Ordered" {
        return Err("retained storage fixture commit tier is unknown".to_owned());
    }
    let dimensions = decoder.u32()?;
    let scheme = decoder.u16()?;
    let document_count = decoder.length("document count")?;
    let mut documents = Vec::with_capacity(document_count);
    for _ in 0..document_count {
        let doc_id = decoder.u128()?;
        let revision = decoder.u64()?;
        let vector_length = decoder.length("vector")?;
        let vector_bits = (0..vector_length)
            .map(|_| decoder.u32())
            .collect::<Result<Vec<_>, _>>()?;
        let timestamp = match decoder.u8()? {
            0 => None,
            1 => Some(i64::from_le_bytes(decoder.array()?)),
            tag => {
                return Err(format!(
                    "retained storage timestamp has invalid optional tag {tag}"
                ));
            }
        };
        let metadata = decoder.optional_bytes("metadata")?;
        let text = decoder
            .optional_bytes("text")?
            .map(|bytes| {
                String::from_utf8(bytes)
                    .map_err(|_| "retained storage text is not UTF-8".to_owned())
            })
            .transpose()?;
        let column_count = decoder.length("column count")?;
        let mut columns = Vec::with_capacity(column_count);
        for _ in 0..column_count {
            let column = decoder.u32()?;
            let value = match decoder.u8()? {
                1 => independent::FixtureColumnValueV1::I64(i64::from_le_bytes(decoder.array()?)),
                2 => independent::FixtureColumnValueV1::F64Bits(decoder.u64()?),
                3 => independent::FixtureColumnValueV1::Bool(decoder.bool()?),
                4 => independent::FixtureColumnValueV1::Bytes(decoder.bytes("column bytes")?),
                tag => {
                    return Err(format!(
                        "retained storage column has unknown value tag {tag}"
                    ));
                }
            };
            columns.push((column, value));
        }
        documents.push(independent::StorageDocumentV1 {
            doc_id,
            revision,
            vector_bits,
            timestamp,
            metadata,
            text,
            columns,
        });
    }
    let mutation_count = decoder.length("mutation count")?;
    let mut mutations = Vec::with_capacity(mutation_count);
    for _ in 0..mutation_count {
        mutations.push(independent::StorageMutationV1 {
            operation_id: decoder.array()?,
            document_index: decoder.u32()?,
            canonical_payload_digest: decoder.array()?,
            first_seq: decoder.u64()?,
            last_seq: decoder.u64()?,
            acknowledged: decoder.bool()?,
        });
    }
    let old_generation = decoder.u64()?;
    let planned_new_generation = decoder.u64()?;
    let absorbed_through = decoder.u64()?;
    let wal_mutation_offset = decoder.u64()?;
    let segment_region_kind = decoder.u16()?;
    let segment_chunk = decoder.u32()?;
    let segment_byte = decoder.u32()?;
    let omission_orphan = match decoder.u8()? {
        1 => independent::OrphanKind::FinalSegment,
        2 => independent::OrphanKind::SegmentTemporary,
        3 => independent::OrphanKind::ManifestTemporary,
        tag => {
            return Err(format!(
                "retained storage fixture has unknown authored-orphan tag {tag}"
            ));
        }
    };
    let omission_is_delete = decoder.bool()?;
    let eligible_orphans = decoder.strings("eligible orphan")?;
    let preserved_files = decoder.strings("preserved file")?;
    let clean_fault_pair_id = decoder.array()?;
    let operation_fixture_id = decoder.array()?;
    decoder.finish()?;
    RetainedStorageOperationV1::new(
        independent::StorageFixtureV1 {
            namespace: "adversarial::storage-durability::v1",
            seed,
            durability: "Durable",
            commit_tier: "Ordered",
            dimensions,
            scheme,
            documents,
            mutations,
            old_generation,
            planned_new_generation,
            absorbed_through,
            wal_mutation_offset,
            segment_region_kind,
            segment_chunk,
            segment_byte,
            omission_orphan,
            omission_is_delete,
            eligible_orphans,
            preserved_files,
            clean_fault_pair_id,
            operation_fixture_id,
        },
        operation,
        op_index,
        fault,
        format_case,
        omission_case,
    )
}

/// Decodes a retained fixture, rejecting unknown versions, corruption, and drift.
pub fn decode_storage_fixture(bytes: &[u8]) -> Result<RetainedStorageOperationV1, String> {
    if bytes.len() < STORAGE_RETAINED_FIXTURE_HEADER_LEN {
        return Err("retained storage fixture header is truncated".to_owned());
    }
    if bytes.get(..8) != Some(STORAGE_RETAINED_FIXTURE_MAGIC) {
        return Err("retained storage fixture magic differs".to_owned());
    }
    let version = u16::from_le_bytes(
        bytes[8..10]
            .try_into()
            .map_err(|_| "retained storage fixture version is truncated".to_owned())?,
    );
    if version != STORAGE_RETAINED_FIXTURE_VERSION {
        return Err(format!(
            "retained storage fixture version {version} is unsupported"
        ));
    }
    let flags = u16::from_le_bytes(
        bytes[10..12]
            .try_into()
            .map_err(|_| "retained storage fixture flags are truncated".to_owned())?,
    );
    if flags != 0 {
        return Err(format!(
            "retained storage fixture reserved flags are nonzero: {flags:#06x}"
        ));
    }
    let payload_length =
        usize::try_from(u32::from_le_bytes(bytes[12..16].try_into().map_err(
            |_| "retained storage fixture length is truncated".to_owned(),
        )?))
        .map_err(|_| "retained storage fixture length exceeds usize".to_owned())?;
    if payload_length > STORAGE_RETAINED_FIXTURE_MAX_BYTES {
        return Err("retained storage fixture payload exceeds the byte limit".to_owned());
    }
    let expected_length = STORAGE_RETAINED_FIXTURE_HEADER_LEN
        .checked_add(payload_length)
        .ok_or_else(|| "retained storage fixture length overflowed".to_owned())?;
    if bytes.len() != expected_length {
        return Err(format!(
            "retained storage fixture length differs declared={payload_length} actual={}",
            bytes
                .len()
                .saturating_sub(STORAGE_RETAINED_FIXTURE_HEADER_LEN)
        ));
    }
    let expected_digest: [u8; 32] = bytes[16..48]
        .try_into()
        .map_err(|_| "retained storage fixture digest is truncated".to_owned())?;
    let payload = &bytes[STORAGE_RETAINED_FIXTURE_HEADER_LEN..];
    let observed_digest = digest32(STORAGE_RETAINED_FIXTURE_DIGEST_DOMAIN, payload);
    if expected_digest != observed_digest {
        return Err("retained storage fixture payload checksum differs".to_owned());
    }
    decode_retained_fixture_payload(payload)
}

/// Canonical raw/public facts captured after the base was sealed and reopened.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageEpisodeBaseEvidence {
    pub fixture: independent::StorageFixtureV1,
    pub bootstrap_document_count: usize,
    pub ack_ledger: Vec<StorageAckEvidence>,
    pub reopened_ack_boundaries: Vec<independent::WalAckBoundary>,
    pub snapshot: independent::SnapshotState,
    pub referenced_segments_complete: bool,
    pub public_versions: Vec<(u128, u64)>,
    pub inventory: Vec<independent::FileFact>,
    pub inventory_digest: [u8; 32],
    pub artifacts: Vec<StorageArtifactEvidence>,
}

/// Exact source/destination proof for one lexicographic operation fork.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageOperationForkEvidence {
    pub operation: StorageOperationKind,
    pub traversal: Vec<String>,
    pub source_inventory: Vec<independent::FileFact>,
    pub destination_inventory: Vec<independent::FileFact>,
    pub source_digest: [u8; 32],
    pub destination_digest: [u8; 32],
    pub destination_artifacts: Vec<StorageArtifactEvidence>,
}

/// One owned operation directory kept alive for the episode.
pub struct StorageOperationFixture {
    directory: TempDir,
    pub evidence: StorageOperationForkEvidence,
}

impl StorageOperationFixture {
    fn path(&self) -> &Path {
        self.directory.path()
    }
}

/// One closed durable base plus its five byte-identical operation roots.
pub struct StorageEpisodeFixtures {
    _base_directory: TempDir,
    base_evidence: StorageEpisodeBaseEvidence,
    operation_fixtures: Vec<StorageOperationFixture>,
}

impl StorageEpisodeFixtures {
    /// Canonical base facts emitted once per episode.
    #[must_use]
    pub const fn base_evidence(&self) -> &StorageEpisodeBaseEvidence {
        &self.base_evidence
    }

    /// Stable plan-order operation forks and their copy evidence.
    #[must_use]
    pub fn operation_fixtures(&self) -> &[StorageOperationFixture] {
        &self.operation_fixtures
    }

    fn operation_fixture(
        &self,
        operation: StorageOperationKind,
    ) -> Result<&StorageOperationFixture, String> {
        self.operation_fixtures
            .iter()
            .find(|fixture| fixture.evidence.operation == operation)
            .ok_or_else(|| format!("storage episode omitted {} fixture", operation.key()))
    }
}

/// I15 fixture/observation result returned to shared dispatch.
#[derive(Debug)]
pub struct PublicationOperationEvidence {
    pub expected: independent::PublicationExpected,
    pub observed: independent::PublicationObserved,
    pub receipt_expected: Option<independent::ReceiptExpected>,
    pub receipt_observed: Option<independent::ReceiptObserved>,
    pub receipt: Option<StorageFaultReceipt>,
    pub child: Option<ChildAbortEvidence>,
    pub control: StorageControlEvidence,
    pub mutation: StorageMutationEvidence,
    pub evidence: StorageEvidenceEnvelope,
}

/// I16 fixture/observation result returned to shared dispatch.
#[derive(Debug)]
pub struct WalPrefixOperationEvidence {
    pub expected: independent::WalPrefixExpected,
    pub observed: independent::WalPrefixObserved,
    pub receipt_expected: Option<independent::ReceiptExpected>,
    pub receipt_observed: Option<independent::ReceiptObserved>,
    pub receipt: Option<StorageFaultReceipt>,
    pub ack_ledger: Vec<StorageAckEvidence>,
    pub control: StorageControlEvidence,
    pub mutation: StorageMutationEvidence,
    pub evidence: StorageEvidenceEnvelope,
}

/// I17 fixture/observation result returned to shared dispatch.
#[derive(Debug)]
pub struct RetryOperationEvidence {
    pub expected: independent::RetryExpected,
    pub observed: independent::RetryObserved,
    pub receipt_expected: Option<independent::ReceiptExpected>,
    pub receipt_observed: Option<independent::ReceiptObserved>,
    pub receipt: Option<StorageFaultReceipt>,
    pub control: StorageControlEvidence,
    pub mutation: StorageMutationEvidence,
    pub evidence: StorageEvidenceEnvelope,
}

/// I18 fixture/observation result returned to shared dispatch.
#[derive(Debug)]
pub struct FormatOperationEvidence {
    pub expected: independent::FormatExpected,
    pub observed: independent::FormatObserved,
    pub receipt_expected: Option<independent::ReceiptExpected>,
    pub receipt_observed: Option<independent::ReceiptObserved>,
    pub receipt: Option<StorageFaultReceipt>,
    pub control: StorageControlEvidence,
    pub mutation: StorageMutationEvidence,
    pub evidence: StorageEvidenceEnvelope,
}

/// Exact intermediate omission facts not collapsed into I19's final retry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OmissionIntermediateEvidence {
    pub inventory: Vec<independent::FileFact>,
    pub cleanup: StorageCleanupReport,
    pub retry_cleanup: StorageCleanupReport,
}

/// I19 fixture/observation result returned to shared dispatch.
#[derive(Debug)]
pub struct ReachabilityOperationEvidence {
    pub expected: independent::ReachabilityExpected,
    pub observed: independent::ReachabilityObserved,
    pub receipt_expected: Option<independent::ReceiptExpected>,
    pub receipt_observed: Option<independent::ReceiptObserved>,
    pub receipt: Option<StorageFaultReceipt>,
    pub omission: Option<OmissionIntermediateEvidence>,
    pub control: StorageControlEvidence,
    pub mutation: StorageMutationEvidence,
    pub evidence: StorageEvidenceEnvelope,
}

/// Typed result of executing one operation from a decoded retained fixture.
#[derive(Debug)]
pub enum RetainedStorageOperationEvidence {
    WalPrefix(WalPrefixOperationEvidence),
    Publication(PublicationOperationEvidence),
    Retry(RetryOperationEvidence),
    FormatCheck(FormatOperationEvidence),
    OrphanCleanup(ReachabilityOperationEvidence),
}

impl RetainedStorageOperationEvidence {
    /// Exact operation identity consumed from the retained schedule.
    #[must_use]
    pub const fn operation(&self) -> StorageOperationKind {
        match self {
            Self::WalPrefix(_) => StorageOperationKind::WalPrefix,
            Self::Publication(_) => StorageOperationKind::Publication,
            Self::Retry(_) => StorageOperationKind::Retry,
            Self::FormatCheck(_) => StorageOperationKind::FormatCheck,
            Self::OrphanCleanup(_) => StorageOperationKind::OrphanCleanup,
        }
    }

    /// Literal primitive fixture carried through the executed evidence.
    #[must_use]
    pub const fn fixture(&self) -> &independent::StorageFixtureV1 {
        match self {
            Self::WalPrefix(evidence) => &evidence.evidence.fixture,
            Self::Publication(evidence) => &evidence.evidence.fixture,
            Self::Retry(evidence) => &evidence.evidence.fixture,
            Self::FormatCheck(evidence) => &evidence.evidence.fixture,
            Self::OrphanCleanup(evidence) => &evidence.evidence.fixture,
        }
    }
}

/// Receipt and process-exit facts observed outside an aborting child.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChildAbortEvidence {
    pub signal: i32,
    pub acknowledgment: String,
    pub fault: StorageFaultKind,
    pub site: &'static str,
    pub op_index: u32,
    pub artifact: String,
    pub temporary: String,
    pub committed: String,
    pub rename_performed: bool,
    pub new_segment_final: bool,
    pub directory_sync_returned: bool,
    pub receipt: StorageFaultReceipt,
}

/// Four domain-separated independent XXH3 words used as a stable 32-byte id.
#[must_use]
pub fn digest32(domain: u64, bytes: &[u8]) -> [u8; 32] {
    let mut output = [0_u8; 32];
    for (lane, destination) in [0_u64, 1, 2, 3].into_iter().zip(output.chunks_exact_mut(8)) {
        let mut input = Vec::with_capacity(16 + bytes.len());
        input.extend_from_slice(&domain.to_le_bytes());
        input.extend_from_slice(&lane.to_le_bytes());
        input.extend_from_slice(bytes);
        let digest = independent::xxh3_64(&input).to_le_bytes();
        destination.copy_from_slice(&digest);
    }
    output
}

fn fixture(seed: u64) -> independent::StorageFixtureV1 {
    independent::StorageFixtureV1::derive(seed)
}

fn fixture_live_versions(
    fixture: &independent::StorageFixtureV1,
    count: usize,
) -> Result<Vec<(u128, u64)>, String> {
    if count > fixture.documents.len() {
        return Err(format!(
            "logical version count {count} exceeds fixture documents {}",
            fixture.documents.len()
        ));
    }
    let mut versions = fixture
        .documents
        .iter()
        .take(count)
        .map(|document| (document.doc_id, document.revision))
        .collect::<Vec<_>>();
    versions.sort_unstable();
    if versions.windows(2).any(|pair| {
        pair.first()
            .zip(pair.get(1))
            .is_some_and(|(left, right)| left.0 == right.0)
    }) {
        return Err("storage fixture contains duplicate document ids".to_owned());
    }
    Ok(versions)
}

fn schema() -> Schema {
    Schema::new(vec![
        ColumnDefinition::new(ColumnId::new(1), "storage_i64", ColumnType::I64, false),
        ColumnDefinition::new(ColumnId::new(2), "storage_bool", ColumnType::Bool, false),
    ])
    .expect("static storage fixture schema is valid")
}

fn options() -> OpenOptions {
    OpenOptions::default()
        .with_schema(schema())
        .with_durability(DurabilityMode::Durable, CommitTier::Ordered)
}

fn document(value: &independent::StorageDocumentV1) -> Result<IngestDocument, String> {
    let mut document = IngestDocument::new(
        DocumentVersion::new(DocId::new(value.doc_id), Revision::new(value.revision)),
        value
            .vector_bits
            .iter()
            .copied()
            .map(f32::from_bits)
            .collect(),
    );
    if let Some(timestamp) = value.timestamp {
        document = document.with_timestamp(timestamp);
    }
    if let Some(metadata) = value.metadata.as_ref() {
        document = document.with_metadata(metadata.clone());
    }
    if let Some(text) = value.text.as_ref() {
        document = document.with_text(text.clone());
    }
    let columns = value
        .columns
        .iter()
        .map(|(column, value)| {
            let value = match value {
                independent::FixtureColumnValueV1::I64(value) => PredicateValue::I64(*value),
                independent::FixtureColumnValueV1::Bool(value) => PredicateValue::Bool(*value),
                independent::FixtureColumnValueV1::F64Bits(value) => {
                    PredicateValue::F64(f64::from_bits(*value))
                }
                independent::FixtureColumnValueV1::Bytes(_) => {
                    return Err(
                        "byte fixture value has no declared storage schema column".to_owned()
                    );
                }
            };
            Ok((ColumnId::new(*column), value))
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(document.with_columns(columns))
}

fn append_optional_bytes(output: &mut Vec<u8>, value: Option<&[u8]>) -> Result<(), String> {
    match value {
        Some(bytes) => {
            output.push(1);
            output.extend_from_slice(
                &u32::try_from(bytes.len())
                    .map_err(|_| "canonical optional value exceeds u32".to_owned())?
                    .to_le_bytes(),
            );
            output.extend_from_slice(bytes);
        }
        None => {
            output.push(0);
            output.extend_from_slice(&0_u32.to_le_bytes());
        }
    }
    Ok(())
}

fn canonical_document_digest(value: &independent::StorageDocumentV1) -> Result<[u8; 32], String> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&value.doc_id.to_le_bytes());
    bytes.extend_from_slice(&value.revision.to_le_bytes());
    bytes.extend_from_slice(
        &u32::try_from(value.vector_bits.len())
            .map_err(|_| "canonical vector length exceeds u32".to_owned())?
            .to_le_bytes(),
    );
    for bits in &value.vector_bits {
        bytes.extend_from_slice(&bits.to_le_bytes());
    }
    match value.timestamp {
        Some(timestamp) => {
            bytes.push(1);
            bytes.extend_from_slice(&timestamp.to_le_bytes());
        }
        None => {
            bytes.push(0);
            bytes.extend_from_slice(&0_i64.to_le_bytes());
        }
    }
    append_optional_bytes(&mut bytes, value.metadata.as_deref())?;
    append_optional_bytes(&mut bytes, value.text.as_deref().map(str::as_bytes))?;
    bytes.extend_from_slice(
        &u32::try_from(value.columns.len())
            .map_err(|_| "canonical column count exceeds u32".to_owned())?
            .to_le_bytes(),
    );
    for (column, value) in &value.columns {
        bytes.extend_from_slice(&column.to_le_bytes());
        match value {
            independent::FixtureColumnValueV1::I64(value) => {
                bytes.push(1);
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            independent::FixtureColumnValueV1::F64Bits(value) => {
                bytes.push(2);
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            independent::FixtureColumnValueV1::Bool(value) => {
                bytes.push(3);
                bytes.push(u8::from(*value));
            }
            independent::FixtureColumnValueV1::Bytes(value) => {
                bytes.push(4);
                bytes.extend_from_slice(
                    &u32::try_from(value.len())
                        .map_err(|_| "canonical bytes column exceeds u32".to_owned())?
                        .to_le_bytes(),
                );
                bytes.extend_from_slice(value);
            }
        }
    }
    Ok(digest32(0x4d55_5441_5449_4f4e, &bytes))
}

fn dependencies(controller: Option<StorageFaultController>) -> StoreTestDependencies {
    let dependencies =
        StoreTestDependencies::new(Arc::new(StdVfs), Arc::new(ManualMonotonicClock::new()));
    controller.map_or(dependencies.clone(), |controller| {
        dependencies.with_storage_fault_controller(controller)
    })
}

fn open_store(path: &Path, controller: Option<StorageFaultController>) -> Result<Store, String> {
    open_store_typed(path, controller).map_err(|error| error.to_string())
}

fn open_store_typed(
    path: &Path,
    controller: Option<StorageFaultController>,
) -> Result<Store, StoreError> {
    Store::open_with_test_dependencies(path, options(), dependencies(controller))
}

fn open_read_only_typed(
    path: &Path,
    controller: Option<StorageFaultController>,
) -> Result<Store, StoreError> {
    Store::open_with_test_dependencies(path, OpenOptions::read_only(), dependencies(controller))
}

fn open_durable_store(
    path: &Path,
    controller: Option<StorageFaultController>,
) -> Result<Store, String> {
    Store::open_with_test_dependencies(
        path,
        options().with_durability(DurabilityMode::Durable, CommitTier::Ordered),
        dependencies(controller),
    )
    .map_err(|error| error.to_string())
}

fn ack_evidence(
    phase: &'static str,
    mutation: &independent::StorageMutationV1,
    result: Result<(u64, u64), String>,
) -> StorageAckEvidence {
    let (returned_seq, returned_generation, returned_ok) = match result {
        Ok((seq, generation)) => (Some(seq), Some(generation), true),
        Err(_) => (None, None, false),
    };
    StorageAckEvidence {
        phase,
        operation_id: mutation.operation_id,
        canonical_request_digest: mutation.canonical_payload_digest,
        planned_first_seq: mutation.first_seq,
        planned_last_seq: mutation.last_seq,
        returned_seq,
        returned_generation,
        returned_ok,
        acknowledged: returned_ok,
        durability: "Durable",
        commit_tier: "Ordered",
    }
}

fn copy_directory(source: &Path) -> Result<TempDir, String> {
    copy_directory_with_traversal(source).map(|(destination, _)| destination)
}

fn copy_directory_with_traversal(source: &Path) -> Result<(TempDir, Vec<String>), String> {
    let destination = tempdir().map_err(|error| error.to_string())?;
    let mut entries = std::fs::read_dir(source)
        .map_err(|error| error.to_string())?
        .map(|entry| {
            let entry = entry.map_err(|error| error.to_string())?;
            let name = entry
                .file_name()
                .to_str()
                .ok_or_else(|| "non-UTF-8 storage fixture artifact".to_owned())?
                .to_owned();
            Ok((name, entry))
        })
        .collect::<Result<Vec<_>, String>>()?;
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut traversal = Vec::with_capacity(entries.len());
    for (name, entry) in entries {
        let file_type = entry.file_type().map_err(|error| error.to_string())?;
        if !file_type.is_file() {
            return Err(format!(
                "storage fixture contains non-file {}",
                entry.path().display()
            ));
        }
        std::fs::copy(entry.path(), destination.path().join(&name))
            .map_err(|error| error.to_string())?;
        traversal.push(name);
    }
    Ok((destination, traversal))
}

fn inventory(path: &Path) -> Result<Vec<independent::FileFact>, String> {
    let mut files = std::fs::read_dir(path)
        .map_err(|error| error.to_string())?
        .map(|entry| {
            let entry = entry.map_err(|error| error.to_string())?;
            if !entry
                .file_type()
                .map_err(|error| error.to_string())?
                .is_file()
            {
                return Err(format!("non-file artifact {}", entry.path().display()));
            }
            let relative = entry
                .file_name()
                .to_str()
                .ok_or_else(|| "non-UTF-8 storage artifact".to_owned())?
                .to_owned();
            let bytes = std::fs::read(entry.path()).map_err(|error| error.to_string())?;
            Ok(independent::FileFact {
                path: relative,
                length: u64::try_from(bytes.len())
                    .map_err(|_| "artifact length does not fit u64".to_owned())?,
                digest: digest32(0x4649_4c45_4641_4354, &bytes),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    files.sort();
    Ok(files)
}

fn inventory_digest(files: &[independent::FileFact]) -> Result<[u8; 32], String> {
    let mut encoded = Vec::new();
    for file in files {
        encoded.extend_from_slice(
            &u64::try_from(file.path.len())
                .map_err(|_| "inventory path length exceeds u64".to_owned())?
                .to_le_bytes(),
        );
        encoded.extend_from_slice(file.path.as_bytes());
        encoded.extend_from_slice(&file.length.to_le_bytes());
        encoded.extend_from_slice(&file.digest);
    }
    Ok(digest32(0x494e_5645_4e54_4f52, &encoded))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PreOperationControl {
    clean_inventory: Vec<independent::FileFact>,
    fault_inventory: Vec<independent::FileFact>,
    clean_digest: [u8; 32],
    fault_digest: [u8; 32],
    clean_artifacts: Vec<StorageArtifactEvidence>,
    fault_artifacts: Vec<StorageArtifactEvidence>,
}

fn capture_pre_operation_control(
    clean: &Path,
    fault: &Path,
) -> Result<PreOperationControl, String> {
    let clean_inventory = inventory(clean)?;
    let fault_inventory = inventory(fault)?;
    let clean_digest = inventory_digest(&clean_inventory)?;
    let fault_digest = inventory_digest(&fault_inventory)?;
    if clean_inventory != fault_inventory || clean_digest != fault_digest {
        return Err("clean/fault fixtures were not byte-identical before operation".to_owned());
    }
    Ok(PreOperationControl {
        clean_inventory,
        fault_inventory,
        clean_digest,
        fault_digest,
        clean_artifacts: artifact_evidence(clean, "clean-pre-operation")?,
        fault_artifacts: artifact_evidence(fault, "fault-pre-operation")?,
    })
}

fn control(
    fixture: &independent::StorageFixtureV1,
    pre: PreOperationControl,
    clean: &Path,
    fault: &Path,
) -> Result<StorageControlEvidence, String> {
    let clean_inventory = inventory(clean)?;
    let fault_inventory = inventory(fault)?;
    Ok(StorageControlEvidence {
        namespace: fixture.namespace,
        seed: fixture.seed,
        operation_fixture_id: fixture.operation_fixture_id,
        clean_fault_pair_id: fixture.clean_fault_pair_id,
        pre_clean_inventory: pre.clean_inventory,
        pre_fault_inventory: pre.fault_inventory,
        pre_clean_digest: pre.clean_digest,
        pre_fault_digest: pre.fault_digest,
        pre_clean_artifacts: pre.clean_artifacts,
        pre_fault_artifacts: pre.fault_artifacts,
        clean_digest: inventory_digest(&clean_inventory)?,
        fault_digest: inventory_digest(&fault_inventory)?,
        clean_inventory,
        fault_inventory,
    })
}

fn artifact_evidence(
    root: &Path,
    role: &'static str,
) -> Result<Vec<StorageArtifactEvidence>, String> {
    inventory(root)?
        .into_iter()
        .map(|fact| {
            let bytes = std::fs::read(root.join(&fact.path)).map_err(|error| error.to_string())?;
            Ok(StorageArtifactEvidence { role, fact, bytes })
        })
        .collect()
}

fn evidence_envelope(
    fixture: &independent::StorageFixtureV1,
    ack_ledger: Vec<StorageAckEvidence>,
    clean: &Path,
    fault: &Path,
) -> Result<StorageEvidenceEnvelope, String> {
    let mut artifacts = artifact_evidence(clean, "clean-post-operation")?;
    artifacts.extend(artifact_evidence(fault, "fault-post-operation")?);
    Ok(StorageEvidenceEnvelope {
        fixture: fixture.clone(),
        ack_ledger,
        artifacts,
    })
}

/// Builds one closed durable episode base and five operation-specific copies.
///
/// The base is created only through public `Store` calls, sealed, closed,
/// reopened for an exhaustive exact scan, and closed again before any copy.
pub fn build_storage_episode_fixtures(seed: u64) -> Result<StorageEpisodeFixtures, String> {
    build_storage_episode_fixtures_from_literal(fixture(seed))
}

fn build_storage_episode_fixtures_from_literal(
    fixture: independent::StorageFixtureV1,
) -> Result<StorageEpisodeFixtures, String> {
    validate_retained_storage_fixture(&fixture)?;
    let bootstrap_document_count =
        fixture.documents.len().checked_sub(1).ok_or_else(|| {
            "storage episode fixture has no pending operation document".to_owned()
        })?;
    if bootstrap_document_count < 2 {
        return Err("storage episode base requires at least two durable groups".to_owned());
    }
    let base_directory = tempdir().map_err(|error| error.to_string())?;
    let mut ack_ledger = Vec::with_capacity(bootstrap_document_count);
    let mut reopened_ack_boundaries = Vec::with_capacity(bootstrap_document_count);
    for (document_value, mutation) in fixture
        .documents
        .iter()
        .zip(&fixture.mutations)
        .take(bootstrap_document_count)
    {
        let store = open_store(base_directory.path(), None)?;
        let result = store
            .ingest(IngestBatch::new(vec![document(document_value)?]))
            .map(|result| (result.seq().get(), result.generation()))
            .map_err(|error| error.to_string());
        if result.is_err() {
            return Err("storage episode base mutation returned an error".to_owned());
        }
        ack_ledger.push(ack_evidence("episode-base", mutation, result));
        store.close().map_err(|error| error.to_string())?;
        let (_, parsed) = wal(base_directory.path())?;
        if parsed.terminator != independent::WalTerminator::CleanEnd {
            return Err("storage episode base acknowledgement is not a clean WAL".to_owned());
        }
        let public = wal_public_outcome(
            base_directory.path(),
            &fixture,
            &independent::WalTerminator::CleanEnd,
        )?;
        let independent::WalPublicOutcome::Opened { live_versions } = public else {
            return Err("storage episode base acknowledgement did not reopen".to_owned());
        };
        reopened_ack_boundaries.push(independent::WalAckBoundary {
            records: parsed.records,
            live_versions,
        });
    }
    let store = open_store(base_directory.path(), None)?;
    store.seal().map_err(|error| error.to_string())?;
    store.close().map_err(|error| error.to_string())?;

    let reopened = Store::open(base_directory.path(), OpenOptions::read_only())
        .map_err(|error| error.to_string())?;
    let public_versions = public_versions(&reopened, &fixture)?;
    reopened.close().map_err(|error| error.to_string())?;
    let expected_versions = fixture_live_versions(&fixture, bootstrap_document_count)?;
    if public_versions != expected_versions {
        return Err("storage episode base exact scan differs after reopen".to_owned());
    }
    let (snapshot, referenced_segments_complete) =
        raw_snapshot_state(base_directory.path(), &fixture, bootstrap_document_count)?;
    if !referenced_segments_complete || snapshot.segments.is_empty() {
        return Err("storage episode base is not a complete sealed snapshot".to_owned());
    }
    let base_inventory = inventory(base_directory.path())?;
    let base_digest = inventory_digest(&base_inventory)?;
    let base_artifacts = artifact_evidence(base_directory.path(), "storage-episode-base")?;
    let base_evidence = StorageEpisodeBaseEvidence {
        fixture: fixture.clone(),
        bootstrap_document_count,
        ack_ledger,
        reopened_ack_boundaries,
        snapshot,
        referenced_segments_complete,
        public_versions,
        inventory: base_inventory.clone(),
        inventory_digest: base_digest,
        artifacts: base_artifacts,
    };

    let mut operation_fixtures = Vec::with_capacity(STORAGE_OPERATION_KINDS.len());
    for operation in STORAGE_OPERATION_KINDS {
        let (directory, traversal) = copy_directory_with_traversal(base_directory.path())?;
        let destination_inventory = inventory(directory.path())?;
        let destination_digest = inventory_digest(&destination_inventory)?;
        if destination_inventory != base_inventory || destination_digest != base_digest {
            return Err(format!(
                "{} operation fixture differs from the closed episode base",
                operation.key()
            ));
        }
        let destination_artifacts = artifact_evidence(directory.path(), operation.artifact_role())?;
        operation_fixtures.push(StorageOperationFixture {
            directory,
            evidence: StorageOperationForkEvidence {
                operation,
                traversal,
                source_inventory: base_inventory.clone(),
                destination_inventory,
                source_digest: base_digest,
                destination_digest,
                destination_artifacts,
            },
        });
    }
    Ok(StorageEpisodeFixtures {
        _base_directory: base_directory,
        base_evidence,
        operation_fixtures,
    })
}

fn first_segment(path: &Path) -> Result<PathBuf, String> {
    std::fs::read_dir(path)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("segment-") && name.ends_with(".zseg"))
        })
        .ok_or_else(|| "sealed storage fixture has no segment".to_owned())
}

fn segment_id(path: &Path) -> Result<zeppelin_embed::segment::SegmentId, String> {
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    let parsed = independent::parse_segment(
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("segment"),
        &bytes,
    )
    .map_err(|error| error.to_string())?;
    Ok(zeppelin_embed::segment::SegmentId::from_bytes(
        parsed.fact.id,
    ))
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn segment_id_from_hex(value: &str) -> Result<zeppelin_embed::segment::SegmentId, String> {
    if value.len() != 32 {
        return Err(format!("segment identity has {} hex bytes", value.len()));
    }
    let mut bytes = [0_u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        let offset = index
            .checked_mul(2)
            .ok_or_else(|| "segment identity offset overflow".to_owned())?;
        *byte = u8::from_str_radix(
            value
                .get(offset..offset + 2)
                .ok_or_else(|| "segment identity byte is truncated".to_owned())?,
            16,
        )
        .map_err(|error| format!("segment identity byte is invalid: {error}"))?;
    }
    Ok(zeppelin_embed::segment::SegmentId::from_bytes(bytes))
}

fn take_one(controller: &StorageFaultController) -> Result<StorageFaultReceipt, String> {
    let receipt = controller
        .take_receipt()
        .ok_or_else(|| "storage production receipt missing".to_owned())?;
    if controller.take_receipt().is_some() {
        return Err("storage production receipt cardinality exceeded one".to_owned());
    }
    if receipt.cardinality() != 1 {
        return Err(format!(
            "storage receipt cardinality was {}",
            receipt.cardinality()
        ));
    }
    Ok(receipt)
}

fn independent_location(
    value: zeppelin_embed::wal::replay::CorruptionLocation,
) -> independent::CorruptionLocation {
    match value {
        zeppelin_embed::wal::replay::CorruptionLocation::Tail => {
            independent::CorruptionLocation::Tail
        }
        zeppelin_embed::wal::replay::CorruptionLocation::Middle => {
            independent::CorruptionLocation::Middle
        }
    }
}

fn independent_header_failure(
    value: zeppelin_embed::wal::header::WalHeaderError,
) -> Result<independent::WalHeaderFailure, String> {
    use independent::WalHeaderFailure as Fact;
    use zeppelin_embed::wal::header::WalHeaderError as Product;
    match value {
        Product::Missing => Ok(Fact::Missing),
        Product::Truncated { needed, available } => Ok(Fact::Truncated {
            needed: u64::try_from(needed).map_err(|_| "WAL header needed width".to_owned())?,
            available: u64::try_from(available)
                .map_err(|_| "WAL header available width".to_owned())?,
        }),
        Product::WrongMagic { expected, actual } => Ok(Fact::WrongMagic { expected, actual }),
        Product::WrongFamily { expected, actual } => Ok(Fact::WrongFamily { expected, actual }),
        Product::UnsupportedVersion {
            family,
            version,
            minimum,
            maximum,
        } => Ok(Fact::UnsupportedVersion {
            family,
            version,
            minimum,
            maximum,
        }),
        Product::InvalidHeaderLength { expected, actual } => {
            Ok(Fact::InvalidHeaderLength { expected, actual })
        }
        Product::NonZeroFileLength { actual } => Ok(Fact::NonZeroFileLength { actual }),
        Product::InvalidSharedHeader { check } => Err(format!(
            "WAL shared-header {check:?} lacks exact value provenance"
        )),
    }
}

fn independent_record_failure(
    value: zeppelin_embed::wal::replay::CorruptionReason,
) -> Result<
    (
        independent::CorruptionLocation,
        independent::WalRecordFailure,
    ),
    String,
> {
    use independent::WalRecordFailure as Fact;
    use zeppelin_embed::wal::record::RecordError;
    use zeppelin_embed::wal::replay::CorruptionReason;
    match value {
        CorruptionReason::Record { location, error } => {
            let failure = match error {
                RecordError::HeaderTruncated { needed, available } => Fact::HeaderTruncated {
                    needed: u64::try_from(needed)
                        .map_err(|_| "WAL record needed width".to_owned())?,
                    available: u64::try_from(available)
                        .map_err(|_| "WAL record available width".to_owned())?,
                },
                RecordError::LengthOverflow { payload_length } => {
                    return Err(format!(
                        "WAL payload length {payload_length} overflow has no independent receipt variant"
                    ));
                }
                RecordError::BodyTruncated {
                    payload_length,
                    needed,
                    available,
                } => Fact::BodyTruncated {
                    payload_length,
                    needed: u64::try_from(needed)
                        .map_err(|_| "WAL body needed width".to_owned())?,
                    available: u64::try_from(available)
                        .map_err(|_| "WAL body available width".to_owned())?,
                },
                RecordError::ChecksumMismatch {
                    expected,
                    actual,
                    record_length,
                } => Fact::ChecksumMismatch {
                    expected,
                    actual,
                    record_length: u64::try_from(record_length)
                        .map_err(|_| "WAL record length width".to_owned())?,
                },
            };
            Ok((independent_location(location), failure))
        }
        CorruptionReason::FirstSequenceMismatch {
            expected,
            actual,
            location,
        }
        | CorruptionReason::SequenceGap {
            expected,
            actual,
            location,
        }
        | CorruptionReason::SequenceRegression {
            expected,
            actual,
            location,
        } => Ok((
            independent_location(location),
            Fact::Sequence {
                expected: expected.get(),
                actual: actual.get(),
            },
        )),
    }
}

fn receipt_operation(value: &str) -> Result<independent::ReceiptOperation, String> {
    match value {
        "wal-prefix" => Ok(independent::ReceiptOperation::WalPrefix),
        "publication" => Ok(independent::ReceiptOperation::Publication),
        "retry" => Ok(independent::ReceiptOperation::Retry),
        "format-check" => Ok(independent::ReceiptOperation::FormatCheck),
        "orphan-cleanup" => Ok(independent::ReceiptOperation::OrphanCleanup),
        _ => Err(format!("unknown storage receipt operation {value}")),
    }
}

fn receipt_fault(value: &str) -> Result<independent::ReceiptFault, String> {
    match value {
        "torn-wal-header" => Ok(independent::ReceiptFault::TornWalHeader),
        "torn-wal-body" => Ok(independent::ReceiptFault::TornWalBody),
        "torn-wal-checksum" => Ok(independent::ReceiptFault::TornWalChecksum),
        "post-commit-error" => Ok(independent::ReceiptFault::PostCommitError),
        "manifest-pre-rename-crash" => Ok(independent::ReceiptFault::ManifestPreRenameCrash),
        "manifest-post-rename-crash" => Ok(independent::ReceiptFault::ManifestPostRenameCrash),
        "corrupt-segment-region" => Ok(independent::ReceiptFault::CorruptSegmentRegion),
        "wrong-manifest-object" => Ok(independent::ReceiptFault::WrongManifestObject),
        "wrong-segment-object" => Ok(independent::ReceiptFault::WrongSegmentObject),
        "list-delete-omission" => Ok(independent::ReceiptFault::ListDeleteOmission),
        _ => Err(format!("unknown storage receipt fault {value}")),
    }
}

fn receipt_site(value: StorageReceiptSite) -> independent::ReceiptSite {
    match value {
        StorageReceiptSite::WalOpenHeaderValidation => {
            independent::ReceiptSite::WalOpenHeaderValidation
        }
        StorageReceiptSite::WalOpenRecordValidation => {
            independent::ReceiptSite::WalOpenRecordValidation
        }
        StorageReceiptSite::WalOpenRecordChecksum => {
            independent::ReceiptSite::WalOpenRecordChecksum
        }
        StorageReceiptSite::WalCommitAppendAfterInnerSuccess => {
            independent::ReceiptSite::WalCommitAppendAfterInnerSuccess
        }
        StorageReceiptSite::ManifestCommitBeforeRename => {
            independent::ReceiptSite::ManifestCommitBeforeRename
        }
        StorageReceiptSite::ManifestCommitAfterRename => {
            independent::ReceiptSite::ManifestCommitAfterRename
        }
        StorageReceiptSite::SegmentReadRegionChecksum => {
            independent::ReceiptSite::SegmentReadRegionChecksum
        }
        StorageReceiptSite::ManifestOpenFamilyValidation => {
            independent::ReceiptSite::ManifestOpenFamilyValidation
        }
        StorageReceiptSite::SegmentOpenFamilyValidation => {
            independent::ReceiptSite::SegmentOpenFamilyValidation
        }
        StorageReceiptSite::SegmentOpenObjectIdentity => {
            independent::ReceiptSite::SegmentOpenObjectIdentity
        }
        StorageReceiptSite::OrphanCleanupList => independent::ReceiptSite::OrphanCleanupList,
        StorageReceiptSite::OrphanCleanupDelete => independent::ReceiptSite::OrphanCleanupDelete,
    }
}

fn receipt_artifact(value: &StorageFaultReceipt) -> independent::ArtifactFact {
    match value.observed() {
        StorageReceiptObserved::WalHeader { artifact, .. }
        | StorageReceiptObserved::WalRecord { artifact, .. }
        | StorageReceiptObserved::WalAppend { artifact, .. } => independent::ArtifactFact::Wal {
            path: artifact.clone(),
        },
        StorageReceiptObserved::ManifestRename { committed, .. } => {
            independent::ArtifactFact::Manifest {
                path: committed.clone(),
            }
        }
        StorageReceiptObserved::SegmentChecksum {
            artifact,
            segment,
            region_kind,
            chunk,
            ..
        } => independent::ArtifactFact::SegmentRegion {
            path: artifact.clone(),
            id: *segment.as_bytes(),
            kind: *region_kind,
            chunk: *chunk,
        },
        StorageReceiptObserved::Format { artifact, .. }
            if value.typed_site() == StorageReceiptSite::ManifestOpenFamilyValidation =>
        {
            independent::ArtifactFact::Manifest {
                path: artifact.clone(),
            }
        }
        StorageReceiptObserved::Format {
            artifact,
            expected_id,
            ..
        } => independent::ArtifactFact::Segment {
            path: artifact.clone(),
            id: expected_id.map(|id| *id.as_bytes()),
        },
        StorageReceiptObserved::Omission { artifact, .. } if artifact == ".manifest.ze.tmp" => {
            independent::ArtifactFact::Manifest {
                path: artifact.clone(),
            }
        }
        StorageReceiptObserved::Omission { artifact, .. } => independent::ArtifactFact::Segment {
            path: artifact.clone(),
            id: None,
        },
    }
}

/// Exhaustively translates one raw production storage receipt into the
/// independent oracle vocabulary. Shared dispatch re-runs this translation
/// before awarding receipt or fault credit.
pub fn receipt_observed_from_product(
    value: &StorageFaultReceipt,
) -> Result<independent::ReceiptObserved, String> {
    let effect = match value.observed() {
        StorageReceiptObserved::WalHeader { reason, .. } => {
            let observed = independent_header_failure(*reason)?;
            let planned_offset = value
                .plan()
                .offset()
                .ok_or_else(|| "WAL header receipt lacks planned offset".to_owned())?;
            let retained_len = match observed {
                independent::WalHeaderFailure::Missing => 0,
                independent::WalHeaderFailure::Truncated { available, .. } => available,
                _ => planned_offset,
            };
            independent::ReceiptEffectFact::WalHeader {
                planned_offset,
                retained_len,
                observed,
            }
        }
        StorageReceiptObserved::WalRecord { offset, reason, .. } => {
            let (location, observed) = independent_record_failure(*reason)?;
            independent::ReceiptEffectFact::WalRecord {
                planned_offset: value
                    .plan()
                    .offset()
                    .ok_or_else(|| "WAL record receipt lacks planned offset".to_owned())?,
                observed_offset: *offset,
                location,
                observed,
            }
        }
        StorageReceiptObserved::WalAppend {
            encoded_len,
            first_seq,
            last_seq,
            inner_append_completed,
            caller_saw_error,
            ..
        } => independent::ReceiptEffectFact::WalAppend {
            encoded_len: *encoded_len,
            first_seq: *first_seq,
            last_seq: *last_seq,
            inner_append_completed: *inner_append_completed,
            caller_saw_error: *caller_saw_error,
        },
        StorageReceiptObserved::ManifestRename {
            temporary,
            committed,
            rename_performed,
            new_segment_final,
            directory_sync_returned,
        } => independent::ReceiptEffectFact::ManifestRename {
            temporary: temporary.clone(),
            committed: committed.clone(),
            rename_performed: *rename_performed,
            new_segment_final: *new_segment_final,
            directory_sync_returned: *directory_sync_returned,
        },
        StorageReceiptObserved::SegmentChecksum {
            segment,
            region_kind,
            chunk,
            expected_checksum,
            actual_checksum,
            ..
        } => independent::ReceiptEffectFact::SegmentChecksum {
            segment: *segment.as_bytes(),
            region_kind: *region_kind,
            chunk: *chunk,
            expected_checksum: *expected_checksum,
            actual_checksum: *actual_checksum,
        },
        StorageReceiptObserved::Format {
            check,
            expected_family,
            actual_family,
            expected_id,
            actual_id,
            ..
        } => independent::ReceiptEffectFact::Format {
            artifact: receipt_artifact(value),
            check: format_check(*check),
            expected_family: *expected_family,
            actual_family: *actual_family,
            expected_id: expected_id.map(|id| *id.as_bytes()),
            actual_id: actual_id.map(|id| *id.as_bytes()),
        },
        StorageReceiptObserved::Omission {
            artifact,
            deletion_observed,
        } => independent::ReceiptEffectFact::Omission {
            omitted_path: artifact.clone(),
            deletion_observed: *deletion_observed,
        },
    };
    Ok(independent::ReceiptObserved {
        value: independent::ReceiptExpected {
            campaign: value.campaign(),
            operation: receipt_operation(value.operation())?,
            fault: receipt_fault(value.fault())?,
            site: receipt_site(value.typed_site()),
            op_index: value.plan().op_index(),
            artifact: receipt_artifact(value),
            effect,
        },
        cardinality: value.cardinality(),
    })
}

fn public_versions(
    store: &Store,
    fixture: &independent::StorageFixtureV1,
) -> Result<Vec<(u128, u64)>, String> {
    let mut versions = BTreeSet::new();
    for document in &fixture.documents {
        let vector = document
            .vector_bits
            .iter()
            .copied()
            .map(f32::from_bits)
            .collect::<Vec<_>>();
        let outcome = store
            .search(
                SearchRequest::new(&vector),
                fixture.documents.len(),
                SearchOptions::new(ScanOptions { thread_budget: 1 }).with_tier(SearchTier::Exact),
                QueryControl::Cancel(CancelToken::new()),
            )
            .map_err(|error| error.to_string())?;
        for candidate in outcome.candidates {
            if let Some(version) = candidate.document() {
                versions.insert((version.doc_id().get(), version.revision().get()));
            }
        }
    }
    Ok(versions.into_iter().collect())
}

fn wal(path: &Path) -> Result<(Vec<u8>, independent::WalArtifact), String> {
    let bytes = std::fs::read(path.join(WAL)).map_err(|error| error.to_string())?;
    let parsed = independent::parse_wal(WAL, &bytes).map_err(|error| error.to_string())?;
    Ok((bytes, parsed))
}

fn validate_wal_refusal(
    error: &StoreError,
    terminator: &independent::WalTerminator,
) -> Result<(), String> {
    let public = match error {
        StoreError::WalRecovery(zeppelin_embed::wal::WalRecoveryError::InvalidHeader(reason)) => {
            independent::WalTerminator::InvalidHeader {
                artifact: WAL.to_owned(),
                reason: independent_header_failure(*reason)?,
            }
        }
        StoreError::WalRecovery(zeppelin_embed::wal::WalRecoveryError::CorruptAt {
            offset,
            reason,
        }) => {
            let (location, reason) = independent_record_failure(*reason)?;
            independent::WalTerminator::CorruptAt {
                artifact: WAL.to_owned(),
                offset: u64::try_from(*offset)
                    .map_err(|_| "public WAL corruption offset exceeds u64".to_owned())?,
                location,
                reason,
            }
        }
        _ => {
            return Err(format!(
                "public WAL refusal {error:?} is not a typed recovery error"
            ));
        }
    };
    if &public == terminator {
        Ok(())
    } else {
        Err(format!(
            "public WAL refusal {public:?} differs from raw terminator {terminator:?}"
        ))
    }
}

fn wal_receipt_expected(
    fault: Option<StorageFaultKind>,
    op_index: u32,
    planned_offset: Option<u64>,
    parsed: &independent::WalArtifact,
) -> Result<Option<independent::ReceiptExpected>, String> {
    let Some(fault) = fault else {
        return Ok(None);
    };
    let planned_offset = planned_offset
        .ok_or_else(|| "selected WAL fault lacks a planned mutation offset".to_owned())?;
    let artifact = independent::ArtifactFact::Wal {
        path: WAL.to_owned(),
    };
    let (fault, site, effect) = match (fault, &parsed.terminator) {
        (
            StorageFaultKind::TornWalHeader,
            independent::WalTerminator::InvalidHeader { reason, .. },
        ) => {
            let retained_len = match reason {
                independent::WalHeaderFailure::Missing => 0,
                independent::WalHeaderFailure::Truncated { available, .. } => *available,
                _ => planned_offset,
            };
            (
                independent::ReceiptFault::TornWalHeader,
                independent::ReceiptSite::WalOpenHeaderValidation,
                independent::ReceiptEffectFact::WalHeader {
                    planned_offset,
                    retained_len,
                    observed: reason.clone(),
                },
            )
        }
        (
            StorageFaultKind::TornWalBody,
            independent::WalTerminator::CorruptAt {
                offset,
                location,
                reason,
                ..
            },
        ) => (
            independent::ReceiptFault::TornWalBody,
            independent::ReceiptSite::WalOpenRecordValidation,
            independent::ReceiptEffectFact::WalRecord {
                planned_offset,
                observed_offset: *offset,
                location: *location,
                observed: reason.clone(),
            },
        ),
        (
            StorageFaultKind::TornWalChecksum,
            independent::WalTerminator::CorruptAt {
                offset,
                location,
                reason,
                ..
            },
        ) => (
            independent::ReceiptFault::TornWalChecksum,
            independent::ReceiptSite::WalOpenRecordChecksum,
            independent::ReceiptEffectFact::WalRecord {
                planned_offset,
                observed_offset: *offset,
                location: *location,
                observed: reason.clone(),
            },
        ),
        _ => return Err("selected WAL fault did not create its exact raw terminator".to_owned()),
    };
    Ok(Some(independent::ReceiptExpected {
        campaign: "storage-durability",
        operation: independent::ReceiptOperation::WalPrefix,
        fault,
        site,
        op_index,
        artifact,
        effect,
    }))
}

fn wal_public_outcome(
    path: &Path,
    fixture: &independent::StorageFixtureV1,
    terminator: &independent::WalTerminator,
) -> Result<independent::WalPublicOutcome, String> {
    match open_read_only_typed(path, None) {
        Ok(store) => {
            let live_versions = public_versions(&store, fixture)?;
            store.close().map_err(|error| error.to_string())?;
            Ok(independent::WalPublicOutcome::Opened { live_versions })
        }
        Err(error) => {
            validate_wal_refusal(&error, terminator)?;
            Ok(independent::WalPublicOutcome::Refused {
                terminator: terminator.clone(),
            })
        }
    }
}

fn observe_wal_prefix_from_operation_fixture(
    base_evidence: &StorageEpisodeBaseEvidence,
    operation_fixture: &StorageOperationFixture,
    op_index: u32,
    fault: Option<StorageFaultKind>,
) -> Result<WalPrefixOperationEvidence, String> {
    if operation_fixture.evidence.operation != StorageOperationKind::WalPrefix {
        return Err("wal-prefix observer received the wrong operation fixture".to_owned());
    }
    let base = copy_directory(operation_fixture.path())?;
    let fixture = base_evidence.fixture.clone();
    let tail_index = base_evidence.bootstrap_document_count;
    if tail_index
        .checked_add(1)
        .is_none_or(|count| count != fixture.documents.len())
    {
        return Err("wal-prefix operation fixture lacks exactly one pending mutation".to_owned());
    }
    let tail_mutation = fixture
        .mutations
        .get(tail_index)
        .ok_or_else(|| "WAL-prefix tail mutation is absent".to_owned())?;
    let tail_document = fixture
        .documents
        .get(tail_index)
        .ok_or_else(|| "WAL-prefix tail document is absent".to_owned())?;
    let ambiguity = StorageFaultController::new(
        StorageTestFault::PostCommitError,
        StorageFaultPlan::new(u32::MAX, WAL),
    );
    let store = open_store(base.path(), Some(ambiguity.clone()))?;
    let tail_result = store
        .ingest(IngestBatch::new(vec![document(tail_document)?]))
        .map(|result| (result.seq().get(), result.generation()))
        .map_err(|error| error.to_string());
    if tail_result.is_ok() {
        return Err("unacknowledged WAL tail returned public success".to_owned());
    }
    let mut ack_ledger = base_evidence.ack_ledger.clone();
    ack_ledger.push(ack_evidence(
        "unacknowledged-tail",
        tail_mutation,
        tail_result,
    ));
    store.close().map_err(|error| error.to_string())?;
    let ambiguity_receipt = take_one(&ambiguity)?;
    if ambiguity_receipt.typed_site() != StorageReceiptSite::WalCommitAppendAfterInnerSuccess {
        return Err("WAL-prefix tail ambiguity fired at the wrong production site".to_owned());
    }
    let reopened_ack_boundaries = base_evidence.reopened_ack_boundaries.clone();
    let clean = copy_directory(base.path())?;
    let damaged = copy_directory(base.path())?;
    let pre_control = capture_pre_operation_control(clean.path(), damaged.path())?;
    let (_, clean_wal) = wal(clean.path())?;
    let clean_public = wal_public_outcome(
        clean.path(),
        &fixture,
        &independent::WalTerminator::CleanEnd,
    )?;
    let mut bytes = std::fs::read(damaged.path().join(WAL)).map_err(|error| error.to_string())?;
    let mut mutation = StorageMutationEvidence {
        artifact: WAL.to_owned(),
        offset: None,
        segment: None,
        region_kind: None,
        chunk: None,
        before: None,
        after: None,
    };
    let controller = match fault {
        None => None,
        Some(StorageFaultKind::TornWalHeader) => {
            let retained_u64 = independent::planned_wal_mutation_offset(
                &fixture,
                independent::WalMutationKind::HeaderTruncation,
            )?;
            let retained = usize::try_from(retained_u64)
                .map_err(|_| "retained WAL header exceeds usize".to_owned())?;
            mutation.offset = Some(retained_u64);
            mutation.before = bytes.get(retained).copied();
            bytes.truncate(retained);
            Some(StorageFaultController::new(
                StorageTestFault::TornWalHeader,
                StorageFaultPlan::new(op_index, WAL).with_offset(retained_u64),
            ))
        }
        Some(StorageFaultKind::TornWalBody) => {
            let retained_u64 = independent::planned_wal_mutation_offset(
                &fixture,
                independent::WalMutationKind::BodyTruncation,
            )?;
            let retained = usize::try_from(retained_u64)
                .map_err(|_| "retained WAL body exceeds usize".to_owned())?;
            mutation.offset = Some(retained_u64);
            mutation.before = bytes.get(retained).copied();
            bytes.truncate(retained);
            Some(StorageFaultController::new(
                StorageTestFault::TornWalBody,
                StorageFaultPlan::new(op_index, WAL).with_offset(retained_u64),
            ))
        }
        Some(StorageFaultKind::TornWalChecksum) => {
            let offset_u64 = independent::planned_wal_mutation_offset(
                &fixture,
                independent::WalMutationKind::ChecksumFlip,
            )?;
            let offset = usize::try_from(offset_u64)
                .map_err(|_| "WAL checksum offset exceeds usize".to_owned())?;
            let before = *bytes
                .get(offset)
                .ok_or_else(|| "checksum byte absent".to_owned())?;
            let after = before ^ 1;
            *bytes
                .get_mut(offset)
                .ok_or_else(|| "checksum byte absent".to_owned())? = after;
            mutation.offset = Some(offset_u64);
            mutation.before = Some(before);
            mutation.after = Some(after);
            Some(StorageFaultController::new(
                StorageTestFault::TornWalChecksum,
                StorageFaultPlan::new(op_index, WAL).with_offset(offset_u64),
            ))
        }
        Some(other) => return Err(format!("{other:?} is not a wal-prefix fault")),
    };
    std::fs::write(damaged.path().join(WAL), &bytes).map_err(|error| error.to_string())?;
    let expected_parse = independent::parse_wal(WAL, &bytes).map_err(|error| error.to_string())?;
    let (_, observed_parse) = wal(damaged.path())?;
    let receipt_expected = wal_receipt_expected(fault, op_index, mutation.offset, &expected_parse)?;
    let public = match controller.as_ref() {
        None => wal_public_outcome(damaged.path(), &fixture, &observed_parse.terminator)?,
        Some(controller) => {
            let result = open_read_only_typed(damaged.path(), Some(controller.clone()));
            match result {
                Ok(store) => {
                    let live_versions = public_versions(&store, &fixture)?;
                    store.close().map_err(|error| error.to_string())?;
                    independent::WalPublicOutcome::Opened { live_versions }
                }
                Err(error) => {
                    validate_wal_refusal(&error, &observed_parse.terminator)?;
                    independent::WalPublicOutcome::Refused {
                        terminator: observed_parse.terminator.clone(),
                    }
                }
            }
        }
    };
    let receipt = controller.as_ref().map(take_one).transpose()?;
    let receipt_observed = receipt
        .as_ref()
        .map(receipt_observed_from_product)
        .transpose()?;
    let expected_records = fixture
        .mutations
        .iter()
        .map(|mutation| independent::expected_wal_record(&fixture, mutation))
        .collect::<Result<Vec<_>, _>>()?;
    if clean_wal.records != expected_records {
        return Err("literal fixture WAL encoding differs from persisted clean WAL".to_owned());
    }
    let acknowledged_count = ack_ledger.iter().filter(|ack| ack.acknowledged).count();
    let ack_boundaries = (0_usize..acknowledged_count)
        .map(|boundary| {
            let count = boundary
                .checked_add(1)
                .ok_or_else(|| "WAL boundary count overflowed".to_owned())?;
            Ok(independent::WalAckBoundary {
                records: expected_records.get(..count).unwrap_or_default().to_vec(),
                live_versions: fixture_live_versions(&fixture, count)?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let expected = independent::WalPrefixExpected {
        first_seq: expected_parse.first_seq,
        acknowledged: expected_records
            .get(..acknowledged_count)
            .unwrap_or_default()
            .to_vec(),
        optional_unacknowledged_tail: expected_records
            .get(acknowledged_count..)
            .unwrap_or_default()
            .to_vec(),
        terminator: expected_parse.terminator,
        live_versions: fixture_live_versions(&fixture, fixture.documents.len())?,
        ack_boundaries,
    };
    let observed = independent::WalPrefixObserved {
        first_seq: observed_parse.first_seq,
        records: observed_parse.records,
        terminator: observed_parse.terminator,
        clean_public,
        public,
        reopened_ack_boundaries,
    };
    let control = control(&fixture, pre_control, clean.path(), damaged.path())?;
    let evidence = evidence_envelope(&fixture, ack_ledger.clone(), clean.path(), damaged.path())?;
    Ok(WalPrefixOperationEvidence {
        expected,
        observed,
        receipt_expected,
        receipt_observed,
        receipt,
        ack_ledger,
        control,
        mutation,
        evidence,
    })
}

/// Observes WAL-prefix behavior from this episode's dedicated operation root.
pub fn observe_wal_prefix_from_episode(
    episode: &StorageEpisodeFixtures,
    op_index: u32,
    fault: Option<StorageFaultKind>,
) -> Result<WalPrefixOperationEvidence, String> {
    observe_wal_prefix_from_operation_fixture(
        episode.base_evidence(),
        episode.operation_fixture(StorageOperationKind::WalPrefix)?,
        op_index,
        fault,
    )
}

/// Compatibility wrapper for focused callers that own only one operation.
pub fn observe_wal_prefix(
    seed: u64,
    op_index: u32,
    fault: Option<StorageFaultKind>,
) -> Result<WalPrefixOperationEvidence, String> {
    let episode = build_storage_episode_fixtures(seed)?;
    observe_wal_prefix_from_episode(&episode, op_index, fault)
}

/// Projects a WAL-prefix damage operation into the exhaustive typed I18 case.
pub fn format_dtos_from_wal_prefix(
    evidence: &WalPrefixOperationEvidence,
) -> Result<(independent::FormatExpected, independent::FormatObserved), String> {
    let clean = match &evidence.observed.clean_public {
        independent::WalPublicOutcome::Opened { .. } => independent::FormatCleanOutcome::Opened,
        independent::WalPublicOutcome::Refused { .. } => {
            return Err("same-seed clean WAL refused public open".to_owned());
        }
    };
    let (case, refusal) = match &evidence.expected.terminator {
        independent::WalTerminator::InvalidHeader { reason, .. } => (
            independent::FormatCase::WalHeader,
            independent::FormatRefusal::WalInvalidHeader {
                artifact: independent::ArtifactFact::Wal {
                    path: WAL.to_owned(),
                },
                reason: reason.clone(),
            },
        ),
        independent::WalTerminator::CorruptAt {
            offset,
            location,
            reason: reason @ independent::WalRecordFailure::ChecksumMismatch { .. },
            ..
        } => (
            independent::FormatCase::WalRecordChecksum,
            independent::FormatRefusal::WalCorruptAt {
                artifact: independent::ArtifactFact::Wal {
                    path: WAL.to_owned(),
                },
                offset: *offset,
                location: *location,
                reason: reason.clone(),
            },
        ),
        independent::WalTerminator::CorruptAt {
            offset,
            location,
            reason,
            ..
        } => (
            independent::FormatCase::WalRecordBody,
            independent::FormatRefusal::WalCorruptAt {
                artifact: independent::ArtifactFact::Wal {
                    path: WAL.to_owned(),
                },
                offset: *offset,
                location: *location,
                reason: reason.clone(),
            },
        ),
        independent::WalTerminator::CleanEnd => {
            return Err("clean WAL has no I18 damaged-format projection".to_owned());
        }
    };
    let observed_refusal = match &evidence.observed.public {
        independent::WalPublicOutcome::Refused { terminator }
            if terminator == &evidence.observed.terminator =>
        {
            refusal.clone()
        }
        independent::WalPublicOutcome::Refused { .. } => {
            return Err("public WAL refusal differs from observed raw terminator".to_owned());
        }
        independent::WalPublicOutcome::Opened { .. } => {
            return Err("damaged WAL opened while projecting I18".to_owned());
        }
    };
    Ok((
        independent::FormatExpected {
            case,
            call: independent::FormatPublicCall::Open,
            clean,
            refusal,
        },
        independent::FormatObserved {
            case,
            call: independent::FormatPublicCall::Open,
            clean,
            refusal: Some(observed_refusal),
            partial_candidates: 0,
        },
    ))
}

fn observe_retry_from_operation_fixture(
    base_evidence: &StorageEpisodeBaseEvidence,
    operation_fixture: &StorageOperationFixture,
    op_index: u32,
    fault: Option<StorageFaultKind>,
) -> Result<RetryOperationEvidence, String> {
    if operation_fixture.evidence.operation != StorageOperationKind::Retry {
        return Err("retry adapter received the wrong operation fixture".to_owned());
    }
    let fixture = base_evidence.fixture.clone();
    let target_index = base_evidence.bootstrap_document_count;
    let target = fixture
        .documents
        .get(target_index)
        .ok_or_else(|| "empty fixture".to_owned())?;
    let retry_mutation = fixture
        .mutations
        .get(target_index)
        .ok_or_else(|| "retry fixture mutation is absent".to_owned())?;
    let expected = independent::RetryExpected::from_fixture_after_closed_base(
        &fixture,
        target_index,
        base_evidence.snapshot.generation,
        fault == Some(StorageFaultKind::PostCommitError),
    )?;
    let mut canonical = independent::expected_wal_record(&fixture, retry_mutation)?;
    canonical.seq = expected.original_seq;
    let operation_wal_bytes =
        std::fs::read(operation_fixture.path().join(WAL)).map_err(|error| error.to_string())?;
    let operation_wal =
        independent::parse_wal(WAL, &operation_wal_bytes).map_err(|error| error.to_string())?;
    if operation_wal.terminator != independent::WalTerminator::CleanEnd {
        return Err("retry operation fixture WAL is not clean".to_owned());
    }
    let clean = copy_directory(operation_fixture.path())?;
    let faulted = copy_directory(operation_fixture.path())?;
    let pre_control = capture_pre_operation_control(clean.path(), faulted.path())?;
    let clean_store = open_store(clean.path(), None)?;
    let clean_result = clean_store
        .ingest(IngestBatch::new(vec![document(target)?]))
        .map_err(|error| error.to_string())?;
    let clean_active_retry = clean_store
        .ingest(IngestBatch::new(vec![document(target)?]))
        .map_err(|error| error.to_string())?;
    let active_versions = public_versions(&clean_store, &fixture)?;
    clean_store.close().map_err(|error| error.to_string())?;
    let clean_wal_bytes =
        std::fs::read(clean.path().join(WAL)).map_err(|error| error.to_string())?;
    let clean_parsed =
        independent::parse_wal(WAL, &clean_wal_bytes).map_err(|error| error.to_string())?;
    let mut expected_clean_records = operation_wal.records;
    expected_clean_records.push(canonical.clone());
    if clean_parsed.records != expected_clean_records {
        return Err("active same-handle retry WAL differs from primitive model".to_owned());
    }
    let active_canonical_record_occurrences = u32::try_from(
        clean_parsed
            .records
            .iter()
            .filter(|record| *record == &canonical)
            .count(),
    )
    .map_err(|_| "active canonical WAL occurrence count exceeds u32".to_owned())?;
    let active_live_version_occurrences = u32::try_from(
        active_versions
            .iter()
            .filter(|version| **version == (target.doc_id, target.revision))
            .count(),
    )
    .map_err(|_| "active live version occurrence count exceeds u32".to_owned())?;
    let clean_seal = open_store(clean.path(), None)?;
    clean_seal.seal().map_err(|error| error.to_string())?;
    clean_seal.close().map_err(|error| error.to_string())?;
    let sealed_wal_before =
        std::fs::read(clean.path().join(WAL)).map_err(|error| error.to_string())?;
    let sealed_reopened = open_store(clean.path(), None)?;
    let sealed_generation_before_retry = sealed_reopened
        .snapshot()
        .map_err(|error| error.to_string())?
        .generation();
    let sealed_retry = sealed_reopened
        .ingest(IngestBatch::new(vec![document(target)?]))
        .map_err(|error| error.to_string())?;
    let sealed_versions = public_versions(&sealed_reopened, &fixture)?;
    sealed_reopened.close().map_err(|error| error.to_string())?;
    let sealed_wal_after =
        std::fs::read(clean.path().join(WAL)).map_err(|error| error.to_string())?;
    let sealed_live_version_occurrences = u32::try_from(
        sealed_versions
            .iter()
            .filter(|version| **version == (target.doc_id, target.revision))
            .count(),
    )
    .map_err(|_| "sealed live version occurrence count exceeds u32".to_owned())?;
    let receipt_expected = match fault {
        Some(StorageFaultKind::PostCommitError) => Some(independent::ReceiptExpected {
            campaign: "storage-durability",
            operation: independent::ReceiptOperation::Retry,
            fault: independent::ReceiptFault::PostCommitError,
            site: independent::ReceiptSite::WalCommitAppendAfterInnerSuccess,
            op_index,
            artifact: independent::ArtifactFact::Wal {
                path: WAL.to_owned(),
            },
            effect: independent::ReceiptEffectFact::WalAppend {
                encoded_len: u64::try_from(canonical.payload.len())
                    .map_err(|_| "canonical WAL payload length exceeds u64".to_owned())?
                    .checked_add(22)
                    .ok_or_else(|| "canonical WAL framed length overflow".to_owned())?,
                first_seq: expected.original_seq,
                last_seq: expected.original_seq,
                inner_append_completed: true,
                caller_saw_error: true,
            },
        }),
        None => None,
        Some(other) => return Err(format!("{other:?} is not a retry fault")),
    };
    let controller = match fault {
        Some(StorageFaultKind::PostCommitError) => Some(StorageFaultController::new(
            StorageTestFault::PostCommitError,
            StorageFaultPlan::new(op_index, WAL),
        )),
        None => None,
        Some(other) => return Err(format!("{other:?} is not a retry fault")),
    };
    let first = open_store(faulted.path(), controller.clone())?;
    let first_result = first.ingest(IngestBatch::new(vec![document(target)?]));
    if controller.is_some() && first_result.is_ok() {
        return Err("post-commit fault returned success".to_owned());
    }
    first.close().map_err(|error| error.to_string())?;
    let receipt = controller.as_ref().map(take_one).transpose()?;
    let (first_seq, ambiguous_first) = match (
        &first_result,
        receipt.as_ref().map(StorageFaultReceipt::observed),
    ) {
        (Ok(result), None) => (
            result.seq().get(),
            independent::RetryPublicResult::Committed {
                seq: result.seq().get(),
                generation: result.generation(),
            },
        ),
        (
            Err(IngestError::Store(StoreError::WalWrite(
                zeppelin_embed::wal::WalWriteError::Failed { kind, detail },
            ))),
            Some(StorageReceiptObserved::WalAppend {
                first_seq,
                inner_append_completed: true,
                caller_saw_error: true,
                ..
            }),
        ) if *kind == std::io::ErrorKind::Other
            && detail.as_ref() == "scheduled post-commit error after inner append" =>
        {
            (
                *first_seq,
                independent::RetryPublicResult::ScheduledPostCommitError,
            )
        }
        _ => return Err("first retry leg lacked an exact append outcome".to_owned()),
    };
    let before = std::fs::read(faulted.path().join(WAL)).map_err(|error| error.to_string())?;
    let reopened = open_store(faulted.path(), None)?;
    let generation_before_retry = reopened
        .snapshot()
        .map_err(|error| error.to_string())?
        .generation();
    let retry = reopened
        .ingest(IngestBatch::new(vec![document(target)?]))
        .map_err(|error| error.to_string())?;
    let versions = public_versions(&reopened, &fixture)?;
    reopened.close().map_err(|error| error.to_string())?;
    let after = std::fs::read(faulted.path().join(WAL)).map_err(|error| error.to_string())?;
    let parsed = independent::parse_wal(WAL, &after).map_err(|error| error.to_string())?;
    let canonical_record_occurrences = u32::try_from(
        parsed
            .records
            .iter()
            .filter(|record| *record == &canonical)
            .count(),
    )
    .map_err(|_| "canonical record occurrence count exceeds u32".to_owned())?;
    let live_version_occurrences = u32::try_from(
        versions
            .iter()
            .filter(|version| **version == (target.doc_id, target.revision))
            .count(),
    )
    .map_err(|_| "live version occurrence count exceeds u32".to_owned())?;
    let canonical_request_digest = canonical_document_digest(target)?;
    let fixture_request_digest = fixture
        .mutations
        .last()
        .ok_or_else(|| "missing mutation".to_owned())?
        .canonical_payload_digest;
    if canonical_request_digest != fixture_request_digest {
        return Err("adapter canonical request digest differs from StorageFixtureV1".to_owned());
    }
    let observed = independent::RetryObserved {
        canonical_request_digest,
        version: (target.doc_id, target.revision),
        first_seq,
        retry_seq: retry.seq().get(),
        generation_before_retry,
        generation_after_retry: retry.generation(),
        wal_length_before_retry: u64::try_from(before.len())
            .map_err(|_| "WAL before-retry length exceeds u64".to_owned())?,
        wal_length_after_retry: u64::try_from(after.len())
            .map_err(|_| "WAL after-retry length exceeds u64".to_owned())?,
        wal_digest_before_retry: digest32(0x5741_4c42_4546_4f52, &before),
        wal_digest_after_retry: digest32(0x5741_4c42_4546_4f52, &after),
        canonical_record_occurrences,
        live_version_occurrences,
        ambiguous_first,
        active_same_handle: independent::ActiveRetryObserved {
            first: independent::RetryPublicResult::Committed {
                seq: clean_result.seq().get(),
                generation: clean_result.generation(),
            },
            retry: independent::RetryPublicResult::Committed {
                seq: clean_active_retry.seq().get(),
                generation: clean_active_retry.generation(),
            },
            canonical_record_occurrences: active_canonical_record_occurrences,
            live_version_occurrences: active_live_version_occurrences,
        },
        sealed_reopened: Some(independent::SealedRetryObserved {
            retry_seq: sealed_retry.seq().get(),
            generation_before_retry: sealed_generation_before_retry,
            generation_after_retry: sealed_retry.generation(),
            wal_length_before_retry: u64::try_from(sealed_wal_before.len())
                .map_err(|_| "sealed WAL before-retry length exceeds u64".to_owned())?,
            wal_length_after_retry: u64::try_from(sealed_wal_after.len())
                .map_err(|_| "sealed WAL after-retry length exceeds u64".to_owned())?,
            wal_digest_before_retry: digest32(0x5345_414c_5741_4c31, &sealed_wal_before),
            wal_digest_after_retry: digest32(0x5345_414c_5741_4c31, &sealed_wal_after),
            live_version_occurrences: sealed_live_version_occurrences,
        }),
    };
    let first_ack_result = match &first_result {
        Ok(result) => Ok((result.seq().get(), result.generation())),
        Err(error) => Err(error.to_string()),
    };
    let mut retry_acks = base_evidence.ack_ledger.clone();
    retry_acks.extend([
        ack_evidence(
            "clean-active-first",
            retry_mutation,
            Ok((clean_result.seq().get(), clean_result.generation())),
        ),
        ack_evidence("ambiguous-active-first", retry_mutation, first_ack_result),
        ack_evidence(
            "ambiguous-active-retry",
            retry_mutation,
            Ok((retry.seq().get(), retry.generation())),
        ),
        ack_evidence(
            "sealed-reopened-retry",
            retry_mutation,
            Ok((sealed_retry.seq().get(), sealed_retry.generation())),
        ),
    ]);
    let control = control(&fixture, pre_control, clean.path(), faulted.path())?;
    let evidence = evidence_envelope(&fixture, retry_acks, clean.path(), faulted.path())?;
    let receipt_observed = receipt
        .as_ref()
        .map(receipt_observed_from_product)
        .transpose()?;
    Ok(RetryOperationEvidence {
        expected,
        observed,
        receipt_expected,
        receipt_observed,
        receipt,
        control,
        mutation: StorageMutationEvidence {
            artifact: WAL.to_owned(),
            offset: None,
            segment: None,
            region_kind: None,
            chunk: None,
            before: None,
            after: None,
        },
        evidence,
    })
}

/// Observes I17 from the retry root of one shared storage episode.
pub fn observe_retry_from_episode(
    episode: &StorageEpisodeFixtures,
    op_index: u32,
    fault: Option<StorageFaultKind>,
) -> Result<RetryOperationEvidence, String> {
    observe_retry_from_operation_fixture(
        episode.base_evidence(),
        episode.operation_fixture(StorageOperationKind::Retry)?,
        op_index,
        fault,
    )
}

/// Compatibility entry point that creates one episode and consumes its retry root.
pub fn observe_retry(
    seed: u64,
    op_index: u32,
    fault: Option<StorageFaultKind>,
) -> Result<RetryOperationEvidence, String> {
    let episode = build_storage_episode_fixtures(seed)?;
    observe_retry_from_episode(&episode, op_index, fault)
}

fn format_check(check: zeppelin_embed::format::frame::FormatCheck) -> independent::FormatCheckFact {
    use independent::FormatCheckFact as Fact;
    use zeppelin_embed::format::frame::FormatCheck as Product;
    match check {
        Product::Length => Fact::Length,
        Product::Magic => Fact::Magic,
        Product::Family => Fact::Family,
        Product::Version => Fact::Version,
        Product::HeaderLength => Fact::HeaderLength,
        Product::FileLength => Fact::FileLength,
        Product::BlockLength => Fact::BlockLength,
        Product::BlockChecksum => Fact::BlockChecksum,
        Product::FileChecksum => Fact::FileChecksum,
        Product::ObjectIdentity => Fact::ObjectIdentity,
    }
}

fn repair_segment_file_checksum(bytes: &mut [u8]) -> Result<(), String> {
    let trailer_offset = bytes
        .len()
        .checked_sub(8)
        .ok_or_else(|| "segment lacks whole-file checksum".to_owned())?;
    let whole_checksum = independent::xxh3_64(
        bytes
            .get(..trailer_offset)
            .ok_or_else(|| "segment whole-checksum range is truncated".to_owned())?,
    );
    bytes
        .get_mut(trailer_offset..)
        .ok_or_else(|| "segment whole checksum bytes are truncated".to_owned())?
        .copy_from_slice(&whole_checksum.to_le_bytes());
    Ok(())
}

fn observed_format_error(
    error: &zeppelin_embed::format::frame::FormatError,
    selected_case: StorageFaultKind,
    segment_name: &str,
    segment_id: zeppelin_embed::segment::SegmentId,
    mutation: &StorageMutationEvidence,
) -> Result<independent::FormatRefusal, String> {
    let check = format_check(error.check());
    let (expected, actual) = match error.values() {
        zeppelin_embed::format::frame::FormatValues::Family { expected, actual } => {
            (u64::from(*expected), u64::from(*actual))
        }
        zeppelin_embed::format::frame::FormatValues::Checksum { expected, actual } => {
            (*expected, *actual)
        }
        zeppelin_embed::format::frame::FormatValues::None => {
            return Err(format!(
                "public format check {:?} omitted typed value facts",
                error.check()
            ));
        }
    };
    let offset = mutation
        .offset
        .ok_or_else(|| "format observation lacks mutation offset".to_owned())?;
    match selected_case {
        StorageFaultKind::CorruptSegmentRegion => {
            let kind = mutation
                .region_kind
                .ok_or_else(|| "segment-region observation lacks region kind".to_owned())?;
            let chunk = mutation
                .chunk
                .ok_or_else(|| "segment-region observation lacks chunk".to_owned())?;
            let expected_artifact = format!("segment:{segment_id}:VectorRescore");
            if error.artifact() != expected_artifact {
                return Err(format!(
                    "public region artifact {:?} differs from {expected_artifact:?}",
                    error.artifact()
                ));
            }
            Ok(independent::FormatRefusal::SegmentFormat {
                artifact: independent::ArtifactFact::SegmentRegion {
                    path: segment_name.to_owned(),
                    id: *segment_id.as_bytes(),
                    kind,
                    chunk,
                },
                check,
                offset,
                expected,
                actual,
            })
        }
        StorageFaultKind::WrongManifestObject => {
            if Path::new(error.artifact())
                .file_name()
                .and_then(|name| name.to_str())
                != Some(MANIFEST)
            {
                return Err(format!(
                    "public manifest refusal named {}",
                    error.artifact()
                ));
            }
            Ok(independent::FormatRefusal::ManifestFormat {
                artifact: independent::ArtifactFact::Manifest {
                    path: MANIFEST.to_owned(),
                },
                check,
                offset,
                expected,
                actual,
            })
        }
        StorageFaultKind::WrongSegmentObject => {
            if Path::new(error.artifact())
                .file_name()
                .and_then(|name| name.to_str())
                != Some(segment_name)
            {
                return Err(format!("public segment refusal named {}", error.artifact()));
            }
            Ok(independent::FormatRefusal::SegmentFormat {
                artifact: independent::ArtifactFact::Segment {
                    path: segment_name.to_owned(),
                    id: Some(*segment_id.as_bytes()),
                },
                check,
                offset,
                expected,
                actual,
            })
        }
        other => Err(format!("{other:?} has no public format-error translation")),
    }
}

fn donor_segment_bytes(
    operation_fixture: &StorageOperationFixture,
    fixture: &independent::StorageFixtureV1,
    pending_index: usize,
    target: zeppelin_embed::segment::SegmentId,
) -> Result<(Vec<u8>, [u8; 16]), String> {
    let directory = copy_directory(operation_fixture.path())?;
    let pending = fixture
        .documents
        .get(pending_index)
        .ok_or_else(|| "same-family donor lacks a pending document".to_owned())?;
    let store = open_store(directory.path(), None)?;
    store
        .ingest(IngestBatch::new(vec![document(pending)?]))
        .map_err(|error| error.to_string())?;
    store.seal().map_err(|error| error.to_string())?;
    store.close().map_err(|error| error.to_string())?;
    for entry in std::fs::read_dir(directory.path()).map_err(|error| error.to_string())? {
        let path = entry.map_err(|error| error.to_string())?.path();
        if !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("segment-") && name.ends_with(".zseg"))
        {
            continue;
        }
        let id = segment_id(&path)?;
        if id != target {
            let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
            return Ok((bytes, *id.as_bytes()));
        }
    }
    Err("same-family donor did not publish a distinct segment".to_owned())
}

fn format_receipt_expected(
    fault: StorageFaultKind,
    op_index: u32,
    refusal: &independent::FormatRefusal,
) -> Result<independent::ReceiptExpected, String> {
    let (fault, site, artifact, effect) = match (fault, refusal) {
        (
            StorageFaultKind::CorruptSegmentRegion,
            independent::FormatRefusal::SegmentFormat {
                artifact:
                    artifact @ independent::ArtifactFact::SegmentRegion {
                        id, kind, chunk, ..
                    },
                check: independent::FormatCheckFact::BlockChecksum,
                expected,
                actual,
                ..
            },
        ) => (
            independent::ReceiptFault::CorruptSegmentRegion,
            independent::ReceiptSite::SegmentReadRegionChecksum,
            artifact.clone(),
            independent::ReceiptEffectFact::SegmentChecksum {
                segment: *id,
                region_kind: *kind,
                chunk: *chunk,
                expected_checksum: *expected,
                actual_checksum: *actual,
            },
        ),
        (
            StorageFaultKind::WrongManifestObject,
            independent::FormatRefusal::ManifestFormat {
                artifact,
                check,
                expected,
                actual,
                ..
            },
        ) => (
            independent::ReceiptFault::WrongManifestObject,
            independent::ReceiptSite::ManifestOpenFamilyValidation,
            artifact.clone(),
            independent::ReceiptEffectFact::Format {
                artifact: artifact.clone(),
                check: *check,
                expected_family: Some(
                    u16::try_from(*expected)
                        .map_err(|_| "manifest expected family exceeds u16".to_owned())?,
                ),
                actual_family: Some(
                    u16::try_from(*actual)
                        .map_err(|_| "manifest actual family exceeds u16".to_owned())?,
                ),
                expected_id: None,
                actual_id: None,
            },
        ),
        (
            StorageFaultKind::WrongSegmentObject,
            independent::FormatRefusal::SegmentFormat {
                artifact,
                check,
                expected,
                actual,
                ..
            },
        ) => {
            let expected_id = match artifact {
                independent::ArtifactFact::Segment { id, .. } => *id,
                _ => return Err("wrong segment refusal lacks segment artifact".to_owned()),
            };
            (
                independent::ReceiptFault::WrongSegmentObject,
                independent::ReceiptSite::SegmentOpenFamilyValidation,
                artifact.clone(),
                independent::ReceiptEffectFact::Format {
                    artifact: artifact.clone(),
                    check: *check,
                    expected_family: Some(
                        u16::try_from(*expected)
                            .map_err(|_| "segment expected family exceeds u16".to_owned())?,
                    ),
                    actual_family: Some(
                        u16::try_from(*actual)
                            .map_err(|_| "segment actual family exceeds u16".to_owned())?,
                    ),
                    expected_id,
                    actual_id: None,
                },
            )
        }
        (
            StorageFaultKind::WrongSegmentObject,
            independent::FormatRefusal::SegmentWrongObject {
                artifact,
                expected,
                actual,
            },
        ) => (
            independent::ReceiptFault::WrongSegmentObject,
            independent::ReceiptSite::SegmentOpenObjectIdentity,
            artifact.clone(),
            independent::ReceiptEffectFact::Format {
                artifact: artifact.clone(),
                check: independent::FormatCheckFact::ObjectIdentity,
                expected_family: None,
                actual_family: None,
                expected_id: Some(*expected),
                actual_id: Some(*actual),
            },
        ),
        _ => return Err("format fault/refusal selection differs".to_owned()),
    };
    Ok(independent::ReceiptExpected {
        campaign: "storage-durability",
        operation: independent::ReceiptOperation::FormatCheck,
        fault,
        site,
        op_index,
        artifact,
        effect,
    })
}

fn observe_format_case_from_operation_fixture(
    base_evidence: &StorageEpisodeBaseEvidence,
    operation_fixture: &StorageOperationFixture,
    op_index: u32,
    case: independent::FormatCase,
    scheduled_receipt: bool,
) -> Result<FormatOperationEvidence, String> {
    if operation_fixture.evidence.operation != StorageOperationKind::FormatCheck {
        return Err("format observer received the wrong operation fixture".to_owned());
    }
    let (selected_case, same_family_identity) = match case {
        independent::FormatCase::SegmentRegion => (StorageFaultKind::CorruptSegmentRegion, false),
        independent::FormatCase::ManifestWrongFamily => {
            (StorageFaultKind::WrongManifestObject, false)
        }
        independent::FormatCase::SegmentWrongFamily => {
            (StorageFaultKind::WrongSegmentObject, false)
        }
        independent::FormatCase::SegmentWrongIdentity => {
            (StorageFaultKind::WrongSegmentObject, true)
        }
        independent::FormatCase::WalHeader
        | independent::FormatCase::WalRecordBody
        | independent::FormatCase::WalRecordChecksum => {
            return Err("WAL I18 cases project from observe_wal_prefix".to_owned());
        }
    };
    let fault = scheduled_receipt.then_some(selected_case);
    let base = copy_directory(operation_fixture.path())?;
    let fixture = base_evidence.fixture.clone();
    let bootstrap_acks = base_evidence.ack_ledger.clone();
    let clean = copy_directory(base.path())?;
    let damaged = copy_directory(base.path())?;
    let pre_control = capture_pre_operation_control(clean.path(), damaged.path())?;
    let expected_clean = if selected_case == StorageFaultKind::CorruptSegmentRegion {
        independent::FormatCleanOutcome::ExactSearch {
            candidates: u32::try_from(base_evidence.bootstrap_document_count)
                .map_err(|_| "clean format fixture count exceeds u32".to_owned())?,
        }
    } else {
        independent::FormatCleanOutcome::Opened
    };
    let clean_public = if selected_case == StorageFaultKind::CorruptSegmentRegion {
        let store = open_store_typed(clean.path(), None).map_err(|error| error.to_string())?;
        let query = fixture
            .documents
            .first()
            .ok_or_else(|| "empty clean format fixture".to_owned())?
            .vector_bits
            .iter()
            .copied()
            .map(f32::from_bits)
            .collect::<Vec<_>>();
        let outcome = store
            .search(
                SearchRequest::new(&query),
                fixture.documents.len(),
                SearchOptions::new(ScanOptions { thread_budget: 1 }).with_tier(SearchTier::Exact),
                QueryControl::Cancel(CancelToken::new()),
            )
            .map_err(|error| format!("same-seed clean exact search refused: {error:?}"))?;
        let candidates = u32::try_from(outcome.candidates.len())
            .map_err(|_| "clean exact candidate count exceeds u32".to_owned())?;
        store.close().map_err(|error| error.to_string())?;
        independent::FormatCleanOutcome::ExactSearch { candidates }
    } else {
        open_store_typed(clean.path(), None)
            .map_err(|error| format!("same-seed clean public open refused: {error:?}"))?
            .close()
            .map_err(|error| error.to_string())?;
        independent::FormatCleanOutcome::Opened
    };
    let segment = first_segment(damaged.path())?;
    let segment_name = segment
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "segment name".to_owned())?
        .to_owned();
    let id = segment_id(&segment)?;
    let mut mutation = StorageMutationEvidence {
        artifact: segment_name.clone(),
        offset: None,
        segment: Some(*id.as_bytes()),
        region_kind: None,
        chunk: None,
        before: None,
        after: None,
    };
    let (planned_controller, expected_refusal, public_call) = match selected_case {
        StorageFaultKind::CorruptSegmentRegion => {
            let mut bytes = std::fs::read(&segment).map_err(|error| error.to_string())?;
            let parsed = independent::parse_segment(&segment_name, &bytes)
                .map_err(|error| error.to_string())?;
            let region = parsed
                .regions
                .iter()
                .find(|region| region.kind == fixture.segment_region_kind)
                .ok_or_else(|| {
                    format!(
                        "segment lacks fixture-selected region {}",
                        fixture.segment_region_kind
                    )
                })?;
            let _region_kind = RegionKind::from_id(region.kind)
                .ok_or_else(|| "fixture selected an unknown segment region".to_owned())?;
            let expected_checksum = region.checksum;
            let region_start =
                usize::try_from(region.offset).map_err(|_| "region offset width".to_owned())?;
            let region_length =
                usize::try_from(region.length).map_err(|_| "region length width".to_owned())?;
            let chunk_start = usize::try_from(fixture.segment_chunk)
                .map_err(|_| "fixture segment chunk exceeds usize".to_owned())?
                .checked_mul(64 * 1024)
                .ok_or_else(|| "fixture segment chunk offset overflow".to_owned())?;
            let chunk_end = chunk_start.saturating_add(64 * 1024).min(region_length);
            if chunk_start >= chunk_end {
                return Err("fixture-selected segment chunk is outside the region".to_owned());
            }
            let chunk_absolute_start = region_start
                .checked_add(chunk_start)
                .ok_or_else(|| "segment chunk absolute offset overflow".to_owned())?;
            let chunk_absolute_end = region_start
                .checked_add(chunk_end)
                .ok_or_else(|| "segment chunk end overflow".to_owned())?;
            let clean_checksum = independent::xxh3_64(
                bytes
                    .get(chunk_absolute_start..chunk_absolute_end)
                    .ok_or_else(|| "fixture-selected region chunk is truncated".to_owned())?,
            );
            if clean_checksum != expected_checksum {
                return Err(format!(
                    "clean rescore checksum {clean_checksum:#x} differs from directory {expected_checksum:#x}"
                ));
            }
            let selected_byte = usize::try_from(fixture.segment_byte)
                .map_err(|_| "fixture segment byte exceeds usize".to_owned())?;
            let absolute = chunk_absolute_start
                .checked_add(selected_byte)
                .filter(|offset| *offset < chunk_absolute_end)
                .ok_or_else(|| "fixture segment byte is outside the selected chunk".to_owned())?;
            let before = *bytes
                .get(absolute)
                .ok_or_else(|| "rescore mutation byte absent".to_owned())?;
            let after = before ^ 1;
            *bytes
                .get_mut(absolute)
                .ok_or_else(|| "rescore mutation byte absent".to_owned())? = after;
            let actual_checksum = independent::xxh3_64(
                bytes
                    .get(chunk_absolute_start..chunk_absolute_end)
                    .ok_or_else(|| "mutated fixture region chunk is truncated".to_owned())?,
            );
            if actual_checksum == expected_checksum {
                return Err("fixture segment mutation preserved the region checksum".to_owned());
            }
            repair_segment_file_checksum(&mut bytes)?;
            std::fs::write(&segment, bytes).map_err(|error| error.to_string())?;
            let absolute_u64 = u64::try_from(absolute)
                .map_err(|_| "region mutation offset exceeds u64".to_owned())?;
            mutation.offset = Some(absolute_u64);
            mutation.region_kind = Some(region.kind);
            mutation.chunk = Some(fixture.segment_chunk);
            mutation.before = Some(before);
            mutation.after = Some(after);
            let controller = StorageFaultController::new(
                StorageTestFault::CorruptSegmentRegion,
                StorageFaultPlan::new(op_index, segment_name.clone())
                    .with_offset(absolute_u64)
                    .with_segment_region(id, region.kind, fixture.segment_chunk),
            );
            let refusal = independent::FormatRefusal::SegmentFormat {
                artifact: independent::ArtifactFact::SegmentRegion {
                    path: segment_name.clone(),
                    id: *id.as_bytes(),
                    kind: region.kind,
                    chunk: fixture.segment_chunk,
                },
                check: independent::FormatCheckFact::BlockChecksum,
                offset: u64::try_from(absolute)
                    .map_err(|_| "region mutation offset exceeds u64".to_owned())?,
                expected: expected_checksum,
                actual: actual_checksum,
            };
            (
                controller,
                refusal,
                independent::FormatPublicCall::ExactSearch,
            )
        }
        StorageFaultKind::WrongManifestObject => {
            let segment_bytes = std::fs::read(&segment).map_err(|error| error.to_string())?;
            let actual_family = u16::from_le_bytes(
                segment_bytes
                    .get(8..10)
                    .and_then(|bytes| <[u8; 2]>::try_from(bytes).ok())
                    .ok_or_else(|| "segment family bytes are truncated".to_owned())?,
            );
            std::fs::write(damaged.path().join(MANIFEST), segment_bytes)
                .map_err(|error| error.to_string())?;
            mutation.artifact = MANIFEST.to_owned();
            mutation.offset = Some(8);
            let refusal = independent::FormatRefusal::ManifestFormat {
                artifact: independent::ArtifactFact::Manifest {
                    path: MANIFEST.to_owned(),
                },
                check: independent::FormatCheckFact::Family,
                offset: 8,
                expected: independent::MANIFEST_FAMILY_ID.into(),
                actual: actual_family.into(),
            };
            (
                StorageFaultController::new(
                    StorageTestFault::WrongManifestObject,
                    StorageFaultPlan::new(op_index, MANIFEST).with_offset(8),
                ),
                refusal,
                independent::FormatPublicCall::Open,
            )
        }
        StorageFaultKind::WrongSegmentObject => {
            let refusal = if same_family_identity {
                let (donor_bytes, actual_id) = donor_segment_bytes(
                    operation_fixture,
                    &fixture,
                    base_evidence.bootstrap_document_count,
                    id,
                )?;
                if actual_id == *id.as_bytes() {
                    return Err("same-family donor reused the target segment id".to_owned());
                }
                std::fs::write(&segment, donor_bytes).map_err(|error| error.to_string())?;
                mutation.offset = Some(32);
                independent::FormatRefusal::SegmentWrongObject {
                    artifact: independent::ArtifactFact::Segment {
                        path: segment_name.clone(),
                        id: Some(*id.as_bytes()),
                    },
                    expected: *id.as_bytes(),
                    actual: actual_id,
                }
            } else {
                let manifest_bytes = std::fs::read(damaged.path().join(MANIFEST))
                    .map_err(|error| error.to_string())?;
                let actual_family = u16::from_le_bytes(
                    manifest_bytes
                        .get(8..10)
                        .and_then(|bytes| <[u8; 2]>::try_from(bytes).ok())
                        .ok_or_else(|| "manifest family bytes are truncated".to_owned())?,
                );
                std::fs::write(&segment, manifest_bytes).map_err(|error| error.to_string())?;
                mutation.offset = Some(8);
                independent::FormatRefusal::SegmentFormat {
                    artifact: independent::ArtifactFact::Segment {
                        path: segment_name.clone(),
                        id: Some(*id.as_bytes()),
                    },
                    check: independent::FormatCheckFact::Family,
                    offset: 8,
                    expected: independent::SEGMENT_FAMILY_ID.into(),
                    actual: actual_family.into(),
                }
            };
            (
                StorageFaultController::new(
                    StorageTestFault::WrongSegmentObject,
                    StorageFaultPlan::new(op_index, segment_name.clone()).with_offset(
                        mutation
                            .offset
                            .ok_or_else(|| "wrong-segment mutation lacks offset".to_owned())?,
                    ),
                ),
                refusal,
                independent::FormatPublicCall::Open,
            )
        }
        other => return Err(format!("{other:?} is not a format fault")),
    };
    let receipt_expected = fault
        .map(|_| format_receipt_expected(selected_case, op_index, &expected_refusal))
        .transpose()?;
    let controller = fault.map(|_| planned_controller);
    let result = open_store_typed(damaged.path(), controller.clone());
    let partial_candidates = 0;
    let observed_refusal = if public_call == independent::FormatPublicCall::ExactSearch {
        let store = result.map_err(|error| format!("corrupt region refused too early: {error}"))?;
        let query = fixture
            .documents
            .first()
            .ok_or_else(|| "empty fixture".to_owned())?
            .vector_bits
            .iter()
            .copied()
            .map(f32::from_bits)
            .collect::<Vec<_>>();
        let error = match store.search(
            SearchRequest::new(&query),
            fixture.documents.len(),
            SearchOptions::new(ScanOptions { thread_budget: 1 }).with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        ) {
            Ok(outcome) => {
                return Err(format!(
                    "corrupt segment region {} query returned {} candidates",
                    fixture.segment_region_kind,
                    outcome.candidates.len()
                ));
            }
            Err(QueryError::Store(StoreError::Segment(
                zeppelin_embed::segment::SegmentError::Format(error),
            ))) => error,
            Err(error) => return Err(format!("wrong corrupt-region query error: {error:?}")),
        };
        store.close().map_err(|error| error.to_string())?;
        observed_format_error(&error, selected_case, &segment_name, id, &mutation)?
    } else {
        match (selected_case, result) {
            (
                StorageFaultKind::WrongManifestObject,
                Err(StoreError::Manifest(zeppelin_embed::manifest::ManifestError::Format(error))),
            ) => observed_format_error(&error, selected_case, &segment_name, id, &mutation)?,
            (
                StorageFaultKind::WrongSegmentObject,
                Err(StoreError::Segment(zeppelin_embed::segment::SegmentError::Format(error))),
            ) => observed_format_error(&error, selected_case, &segment_name, id, &mutation)?,
            (
                StorageFaultKind::WrongSegmentObject,
                Err(StoreError::Segment(zeppelin_embed::segment::SegmentError::WrongObject {
                    artifact,
                    expected,
                    actual,
                })),
            ) => {
                if Path::new(&artifact)
                    .file_name()
                    .and_then(|name| name.to_str())
                    != Some(segment_name.as_str())
                {
                    return Err(format!("public wrong-object refusal named {artifact}"));
                }
                independent::FormatRefusal::SegmentWrongObject {
                    artifact: independent::ArtifactFact::Segment {
                        path: segment_name.clone(),
                        id: Some(*expected.as_bytes()),
                    },
                    expected: *expected.as_bytes(),
                    actual: *actual.as_bytes(),
                }
            }
            (_, Ok(store)) => {
                store.close().map_err(|error| error.to_string())?;
                return Err("wrong persisted object opened successfully".to_owned());
            }
            (_, Err(error)) => return Err(format!("wrong persisted-object error: {error:?}")),
        }
    };
    let receipt = controller.as_ref().map(take_one).transpose()?;
    let receipt_observed = receipt
        .as_ref()
        .map(receipt_observed_from_product)
        .transpose()?;
    let format_case = match selected_case {
        StorageFaultKind::CorruptSegmentRegion => independent::FormatCase::SegmentRegion,
        StorageFaultKind::WrongManifestObject => independent::FormatCase::ManifestWrongFamily,
        StorageFaultKind::WrongSegmentObject if same_family_identity => {
            independent::FormatCase::SegmentWrongIdentity
        }
        StorageFaultKind::WrongSegmentObject => independent::FormatCase::SegmentWrongFamily,
        other => return Err(format!("{other:?} has no I18 persisted-object case")),
    };
    let expected = independent::FormatExpected {
        case: format_case,
        call: public_call,
        clean: expected_clean,
        refusal: expected_refusal.clone(),
    };
    let observed = independent::FormatObserved {
        case: format_case,
        call: public_call,
        clean: clean_public,
        refusal: Some(observed_refusal),
        partial_candidates,
    };
    let control = control(&fixture, pre_control, clean.path(), damaged.path())?;
    let evidence = evidence_envelope(&fixture, bootstrap_acks, clean.path(), damaged.path())?;
    Ok(FormatOperationEvidence {
        expected,
        observed,
        receipt_expected,
        receipt_observed,
        receipt,
        control,
        mutation,
        evidence,
    })
}

/// Observes one explicit persisted-object case from the episode format root.
pub fn observe_format_case_from_episode(
    episode: &StorageEpisodeFixtures,
    op_index: u32,
    case: independent::FormatCase,
    scheduled_receipt: bool,
) -> Result<FormatOperationEvidence, String> {
    observe_format_case_from_operation_fixture(
        episode.base_evidence(),
        episode.operation_fixture(StorageOperationKind::FormatCheck)?,
        op_index,
        case,
        scheduled_receipt,
    )
}

/// Compatibility wrapper for explicit persisted-object focused tests.
pub fn observe_format_case(
    seed: u64,
    op_index: u32,
    case: independent::FormatCase,
    scheduled_receipt: bool,
) -> Result<FormatOperationEvidence, String> {
    let episode = build_storage_episode_fixtures(seed)?;
    observe_format_case_from_episode(&episode, op_index, case, scheduled_receipt)
}

/// Selects the scheduled persisted-object case while retaining the shared API.
pub fn observe_format(
    seed: u64,
    op_index: u32,
    fault: Option<StorageFaultKind>,
) -> Result<FormatOperationEvidence, String> {
    let case = match fault {
        Some(StorageFaultKind::CorruptSegmentRegion) => independent::FormatCase::SegmentRegion,
        Some(StorageFaultKind::WrongManifestObject) => independent::FormatCase::ManifestWrongFamily,
        Some(StorageFaultKind::WrongSegmentObject) if seed & 1 == 1 => {
            independent::FormatCase::SegmentWrongIdentity
        }
        Some(StorageFaultKind::WrongSegmentObject) => independent::FormatCase::SegmentWrongFamily,
        None => match seed % 4 {
            0 => independent::FormatCase::SegmentRegion,
            1 => independent::FormatCase::ManifestWrongFamily,
            2 => independent::FormatCase::SegmentWrongFamily,
            _ => independent::FormatCase::SegmentWrongIdentity,
        },
        Some(other) => return Err(format!("{other:?} is not a format-check fault")),
    };
    let episode = build_storage_episode_fixtures(seed)?;
    observe_format_case_from_episode(&episode, op_index, case, fault.is_some())
}

/// Selects and observes the scheduled case from the episode format root.
pub fn observe_format_from_episode(
    episode: &StorageEpisodeFixtures,
    op_index: u32,
    fault: Option<StorageFaultKind>,
) -> Result<FormatOperationEvidence, String> {
    let seed = episode.base_evidence().fixture.seed;
    let case = match fault {
        Some(StorageFaultKind::CorruptSegmentRegion) => independent::FormatCase::SegmentRegion,
        Some(StorageFaultKind::WrongManifestObject) => independent::FormatCase::ManifestWrongFamily,
        Some(StorageFaultKind::WrongSegmentObject) if seed & 1 == 1 => {
            independent::FormatCase::SegmentWrongIdentity
        }
        Some(StorageFaultKind::WrongSegmentObject) => independent::FormatCase::SegmentWrongFamily,
        None => match seed % 4 {
            0 => independent::FormatCase::SegmentRegion,
            1 => independent::FormatCase::ManifestWrongFamily,
            2 => independent::FormatCase::SegmentWrongFamily,
            _ => independent::FormatCase::SegmentWrongIdentity,
        },
        Some(other) => return Err(format!("{other:?} is not a format-check fault")),
    };
    observe_format_case_from_episode(episode, op_index, case, fault.is_some())
}

fn orphan_artifact(path: &str) -> independent::ArtifactFact {
    if path == ".manifest.ze.tmp" {
        independent::ArtifactFact::Manifest {
            path: path.to_owned(),
        }
    } else {
        independent::ArtifactFact::Segment {
            path: path.to_owned(),
            id: None,
        }
    }
}

fn observe_reachability_case_from_operation_fixture(
    base_evidence: &StorageEpisodeBaseEvidence,
    operation_fixture: &StorageOperationFixture,
    op_index: u32,
    omission_case: Option<independent::OmissionCase>,
) -> Result<ReachabilityOperationEvidence, String> {
    if operation_fixture.evidence.operation != StorageOperationKind::OrphanCleanup {
        return Err("orphan-cleanup observer received the wrong operation fixture".to_owned());
    }
    let base = copy_directory(operation_fixture.path())?;
    let fixture = base_evidence.fixture.clone();
    let bootstrap_acks = base_evidence.ack_ledger.clone();
    let seed = fixture.seed;
    let committed_purge = copy_directory(base.path())?;
    let purge_store = open_store(committed_purge.path(), None)?;
    let purged_id = fixture
        .documents
        .first()
        .map(|document| DocId::new(document.doc_id))
        .ok_or_else(|| "committed-purge fixture has no document".to_owned())?;
    let purge_token = purge_store
        .purge_with_available_space(&[purged_id], u64::MAX)
        .map_err(|error| error.to_string())?;
    if purge_token.is_no_op() {
        return Err("committed-purge fixture produced a no-op intent".to_owned());
    }
    purge_store.close().map_err(|error| error.to_string())?;
    let committed_purge_before = inventory(committed_purge.path())?;
    let committed_purge_outcome = match open_read_only_typed(committed_purge.path(), None) {
        Err(StoreError::PurgeRecovery { detail }) if detail == "store handle is read-only" => {
            independent::CommittedPurgeReadOnlyOutcome::RefusedPurgeRecoveryReadOnly
        }
        Err(error) => return Err(format!("wrong committed-purge read-only result: {error:?}")),
        Ok(store) => {
            store.close().map_err(|error| error.to_string())?;
            return Err("read-only open recovered a committed purge intent".to_owned());
        }
    };
    let committed_purge_read_only = independent::CommittedPurgeReadOnlyObserved {
        before: committed_purge_before,
        after: inventory(committed_purge.path())?,
        outcome: committed_purge_outcome,
    };
    let omission_target = omission_case
        .map(|case| {
            let index = match case.orphan {
                independent::OrphanKind::FinalSegment => 0,
                independent::OrphanKind::SegmentTemporary => 1,
                independent::OrphanKind::ManifestTemporary => 2,
            };
            fixture
                .eligible_orphans
                .get(index)
                .cloned()
                .ok_or_else(|| "fixture omission orphan family is absent".to_owned())
        })
        .transpose()?;
    let mutation_artifact = omission_target
        .clone()
        .or_else(|| fixture.eligible_orphans.first().cloned())
        .unwrap_or_default();
    let planted_orphans = fixture.eligible_orphans.clone();
    for (index, name) in planted_orphans.iter().enumerate() {
        std::fs::write(base.path().join(name), [seed as u8, index as u8, 0xa5])
            .map_err(|error| error.to_string())?;
    }
    std::fs::write(base.path().join("owner-sentinel.bin"), b"preserved-owner")
        .map_err(|error| error.to_string())?;
    std::fs::write(base.path().join(".purge.ze.tmp"), b"preserved-purge-temp")
        .map_err(|error| error.to_string())?;
    std::fs::write(
        base.path().join(".wal.ze.purge.tmp"),
        b"preserved-wal-purge-temp",
    )
    .map_err(|error| error.to_string())?;
    let baseline = inventory(base.path())?;
    let expected = independent::ReachabilityExpected::from_manifest(
        baseline,
        &std::fs::read(base.path().join(MANIFEST)).map_err(|error| error.to_string())?,
        true,
    )?
    .with_committed_purge_read_only();
    let expected = if omission_case.is_some() {
        expected.with_omission_recovery_leg()
    } else {
        expected
    };
    let read_only = copy_directory(base.path())?;
    Store::open(read_only.path(), OpenOptions::read_only())
        .map_err(|error| error.to_string())?
        .close()
        .map_err(|error| error.to_string())?;
    let after_read_only = inventory(read_only.path())?;
    let clean = copy_directory(base.path())?;
    let faulted = copy_directory(base.path())?;
    let pre_control = capture_pre_operation_control(clean.path(), faulted.path())?;
    let clean_probe = StorageFaultController::new(
        StorageTestFault::DeleteOmission {
            file_name: "__never_storage_orphan__".to_owned(),
        },
        StorageFaultPlan::new(op_index, "__never_storage_orphan__"),
    );
    open_durable_store(clean.path(), Some(clean_probe.clone()))?
        .close()
        .map_err(|error| error.to_string())?;
    let clean_report = clean_probe
        .take_cleanup_report()
        .ok_or_else(|| "clean cleanup report missing".to_owned())?;
    let mut receipt = None;
    let mut receipt_expected = None;
    let mut omission = None;
    let (reclaimed_bytes, directory_syncs);
    if let Some(case) = omission_case {
        let omitted = omission_target
            .clone()
            .ok_or_else(|| "no omission target".to_owned())?;
        let (site, omitted_by_delete) = if case.subsite == independent::OmissionSubsite::Delete {
            (independent::ReceiptSite::OrphanCleanupDelete, true)
        } else {
            (independent::ReceiptSite::OrphanCleanupList, false)
        };
        receipt_expected = Some(independent::ReceiptExpected {
            campaign: "storage-durability",
            operation: independent::ReceiptOperation::OrphanCleanup,
            fault: independent::ReceiptFault::ListDeleteOmission,
            site,
            op_index,
            artifact: orphan_artifact(&omitted),
            effect: independent::ReceiptEffectFact::Omission {
                omitted_path: omitted.clone(),
                deletion_observed: false,
            },
        });
        let test_fault = if case.subsite == independent::OmissionSubsite::Delete {
            StorageTestFault::DeleteOmission {
                file_name: omitted.clone(),
            }
        } else {
            StorageTestFault::ListOmission {
                file_name: omitted.clone(),
            }
        };
        let controller = StorageFaultController::new(
            test_fault,
            StorageFaultPlan::new(op_index, omitted.clone()),
        );
        open_durable_store(faulted.path(), Some(controller.clone()))?
            .close()
            .map_err(|error| error.to_string())?;
        let cleanup = controller
            .take_cleanup_report()
            .ok_or_else(|| "fault cleanup report missing".to_owned())?;
        receipt = Some(take_one(&controller)?);
        let intermediate_inventory = inventory(faulted.path())?;
        open_durable_store(faulted.path(), Some(controller.clone()))?
            .close()
            .map_err(|error| error.to_string())?;
        let retry_cleanup = controller
            .take_cleanup_report()
            .ok_or_else(|| "retry cleanup report missing".to_owned())?;
        reclaimed_bytes = cleanup
            .reclaimed_bytes()
            .checked_add(retry_cleanup.reclaimed_bytes())
            .ok_or_else(|| "fault cleanup reclaimed bytes overflow".to_owned())?;
        directory_syncs = u32::from(cleanup.directory_synced())
            .checked_add(u32::from(retry_cleanup.directory_synced()))
            .ok_or_else(|| "fault cleanup sync count overflow".to_owned())?;
        if omitted_by_delete {
            if cleanup.retained_eligible_paths().len() != 1
                || cleanup
                    .retained_eligible_paths()
                    .first()
                    .is_none_or(|path| path != &omitted)
            {
                return Err("delete omission retained-path accounting differs".to_owned());
            }
        } else if !cleanup.retained_eligible_paths().is_empty() {
            return Err("list omission falsely reported a retained deletion target".to_owned());
        }
        omission = Some(OmissionIntermediateEvidence {
            inventory: intermediate_inventory,
            cleanup,
            retry_cleanup,
        });
    } else {
        let probe = StorageFaultController::new(
            StorageTestFault::DeleteOmission {
                file_name: "__never_storage_orphan__".to_owned(),
            },
            StorageFaultPlan::new(op_index, "__never_storage_orphan__"),
        );
        open_durable_store(faulted.path(), Some(probe.clone()))?
            .close()
            .map_err(|error| error.to_string())?;
        let report = probe
            .take_cleanup_report()
            .ok_or_else(|| "observed cleanup report missing".to_owned())?;
        reclaimed_bytes = report.reclaimed_bytes();
        directory_syncs = u32::from(report.directory_synced());
    }
    let final_inventory = inventory(faulted.path())?;
    let observed = independent::ReachabilityObserved {
        after_read_only,
        final_inventory,
        reclaimed_bytes,
        directory_syncs,
        committed_purge_read_only: Some(committed_purge_read_only),
    };
    if clean_report.reclaimed_bytes() != observed.reclaimed_bytes
        || clean_report.directory_synced() != (observed.directory_syncs != 0)
    {
        return Err("same-seed clean and observed cleanup accounting differ".to_owned());
    }
    let receipt_observed = receipt
        .as_ref()
        .map(receipt_observed_from_product)
        .transpose()?;
    let control = control(&fixture, pre_control, clean.path(), faulted.path())?;
    let evidence = evidence_envelope(&fixture, bootstrap_acks, clean.path(), faulted.path())?;
    Ok(ReachabilityOperationEvidence {
        expected,
        observed,
        receipt_expected,
        receipt_observed,
        receipt,
        omission,
        control,
        mutation: StorageMutationEvidence {
            artifact: mutation_artifact,
            offset: None,
            segment: None,
            region_kind: None,
            chunk: None,
            before: None,
            after: None,
        },
        evidence,
    })
}

/// Observes one explicit omission case from the episode cleanup root.
pub fn observe_reachability_case_from_episode(
    episode: &StorageEpisodeFixtures,
    op_index: u32,
    omission_case: Option<independent::OmissionCase>,
) -> Result<ReachabilityOperationEvidence, String> {
    observe_reachability_case_from_operation_fixture(
        episode.base_evidence(),
        episode.operation_fixture(StorageOperationKind::OrphanCleanup)?,
        op_index,
        omission_case,
    )
}

/// Compatibility wrapper for explicit omission focused tests.
pub fn observe_reachability_case(
    seed: u64,
    op_index: u32,
    omission_case: Option<independent::OmissionCase>,
) -> Result<ReachabilityOperationEvidence, String> {
    let episode = build_storage_episode_fixtures(seed)?;
    observe_reachability_case_from_episode(&episode, op_index, omission_case)
}

/// Deterministically rotates the complete six-case omission catalog.
#[must_use]
pub fn omission_case_for_schedule(seed: u64, profile_ordinal: u32) -> independent::OmissionCase {
    let ordinal = (seed % 6 + u64::from(profile_ordinal) % 6) % 6;
    match ordinal {
        0 => independent::ALL_OMISSION_CASES[0],
        1 => independent::ALL_OMISSION_CASES[1],
        2 => independent::ALL_OMISSION_CASES[2],
        3 => independent::ALL_OMISSION_CASES[3],
        4 => independent::ALL_OMISSION_CASES[4],
        _ => independent::ALL_OMISSION_CASES[5],
    }
}

/// Selects the fixture-authored omission case while retaining the shared API.
pub fn observe_reachability(
    seed: u64,
    op_index: u32,
    fault: Option<StorageFaultKind>,
) -> Result<ReachabilityOperationEvidence, String> {
    let omission_case = match fault {
        Some(StorageFaultKind::ListDeleteOmission) => {
            let fixture = fixture(seed);
            Some(independent::OmissionCase {
                orphan: fixture.omission_orphan,
                subsite: if fixture.omission_is_delete {
                    independent::OmissionSubsite::Delete
                } else {
                    independent::OmissionSubsite::List
                },
            })
        }
        None => None,
        Some(other) => return Err(format!("{other:?} is not an orphan-cleanup fault")),
    };
    let episode = build_storage_episode_fixtures(seed)?;
    observe_reachability_case_from_episode(&episode, op_index, omission_case)
}

/// Selects and observes the fixture-authored case from the episode cleanup root.
pub fn observe_reachability_from_episode(
    episode: &StorageEpisodeFixtures,
    op_index: u32,
    fault: Option<StorageFaultKind>,
) -> Result<ReachabilityOperationEvidence, String> {
    let primitive = &episode.base_evidence().fixture;
    let omission_case = match fault {
        Some(StorageFaultKind::ListDeleteOmission) => Some(independent::OmissionCase {
            orphan: primitive.omission_orphan,
            subsite: if primitive.omission_is_delete {
                independent::OmissionSubsite::Delete
            } else {
                independent::OmissionSubsite::List
            },
        }),
        None => None,
        Some(other) => return Err(format!("{other:?} is not an orphan-cleanup fault")),
    };
    observe_reachability_case_from_episode(episode, op_index, omission_case)
}

fn raw_snapshot_state(
    path: &Path,
    fixture: &independent::StorageFixtureV1,
    live_count: usize,
) -> Result<(independent::SnapshotState, bool), String> {
    let live_versions = fixture_live_versions(fixture, live_count)?;
    let (_, parsed_wal) = wal(path)?;
    let manifest_path = path.join(MANIFEST);
    if !manifest_path.exists() {
        if parsed_wal.records.iter().any(|record| record.op != 7) {
            return Err("publication fixture WAL contains a non-ingest operation".to_owned());
        }
        let generation = u64::try_from(parsed_wal.records.len())
            .map_err(|_| "publication replay count exceeds u64".to_owned())?;
        return Ok((
            independent::SnapshotState {
                generation,
                manifest_log_seq: 0,
                segments: Vec::new(),
                live_versions,
                absorbed_through: 0,
            },
            true,
        ));
    }
    let manifest = independent::parse_manifest(
        MANIFEST,
        &std::fs::read(&manifest_path).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let replayed_records = parsed_wal
        .records
        .iter()
        .filter(|record| record.seq > manifest.log_seq)
        .collect::<Vec<_>>();
    if replayed_records.iter().any(|record| record.op != 7) {
        return Err("publication replay contains a non-ingest operation".to_owned());
    }
    let generation = manifest
        .generation
        .checked_add(
            u64::try_from(replayed_records.len())
                .map_err(|_| "publication replay count exceeds u64".to_owned())?,
        )
        .ok_or_else(|| "publication recovery generation overflow".to_owned())?;
    let mut segments = Vec::with_capacity(manifest.segments.len());
    let mut complete = true;
    for segment in &manifest.segments {
        let name = format!("segment-{}.zseg", hex_bytes(&segment.id));
        match std::fs::read(path.join(&name)) {
            Ok(bytes) => match independent::parse_segment(&name, &bytes) {
                Ok(parsed) => {
                    if parsed.fact.id != segment.id
                        || parsed.fact.rows != segment.rows
                        || parsed.fact.scheme != segment.scheme
                        || parsed.fact.dims != segment.dims
                        || parsed.fact.file_length != segment.file_length
                    {
                        complete = false;
                    }
                    segments.push(parsed.fact);
                }
                Err(_) => complete = false,
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => complete = false,
            Err(error) => return Err(error.to_string()),
        }
    }
    segments.sort();
    Ok((
        independent::SnapshotState {
            generation,
            manifest_log_seq: manifest.log_seq,
            segments,
            live_versions,
            absorbed_through: manifest.log_seq,
        },
        complete,
    ))
}

fn public_snapshot_state(
    path: &Path,
    fixture: &independent::StorageFixtureV1,
) -> Result<independent::PublishedSnapshotState, String> {
    let store = Store::open(path, OpenOptions::read_only()).map_err(|error| error.to_string())?;
    let snapshot = store.snapshot().map_err(|error| error.to_string())?;
    let mut segments = snapshot
        .segments()
        .iter()
        .map(|reader| {
            let meta = reader.meta();
            independent::PublishedSegmentFact {
                id: *meta.id.as_bytes(),
                rows: meta.row_count,
                scheme: meta.scheme,
                dims: meta.dims,
                file_length: meta.file_size,
            }
        })
        .collect::<Vec<_>>();
    segments.sort();
    let result = independent::PublishedSnapshotState {
        generation: snapshot.generation(),
        segments,
        live_versions: public_versions(&store, fixture)?,
        absorbed_through: snapshot.storage_absorbed_through(),
    };
    drop(snapshot);
    store.close().map_err(|error| error.to_string())?;
    Ok(result)
}

/// Entry point for the ignored subprocess test owned by shared dispatch.
#[cfg(unix)]
pub fn publication_child_from_env() -> Result<(), String> {
    use std::os::fd::FromRawFd as _;
    use std::os::unix::net::UnixStream;
    let directory = std::env::var_os(STORAGE_CHILD_DIRECTORY)
        .map(PathBuf::from)
        .ok_or_else(|| "missing child directory".to_owned())?;
    let fault = match std::env::var(STORAGE_CHILD_FAULT)
        .map_err(|error| error.to_string())?
        .as_str()
    {
        "pre" => StorageTestFault::ManifestPreRename,
        "post" => StorageTestFault::ManifestPostRename,
        value => return Err(format!("unknown publication child fault {value}")),
    };
    let op_index = std::env::var(STORAGE_CHILD_OP_INDEX)
        .map_err(|error| error.to_string())?
        .parse::<u32>()
        .map_err(|error| error.to_string())?;
    let planned_segment = segment_id_from_hex(
        &std::env::var(STORAGE_CHILD_SEGMENT).map_err(|error| error.to_string())?,
    )?;
    // SAFETY: the parent installs one end of a UnixStream as the child's stdin.
    let acknowledgment = unsafe { UnixStream::from_raw_fd(0) };
    let controller = StorageFaultController::new(
        fault,
        StorageFaultPlan::new(op_index, MANIFEST).with_segment(planned_segment),
    )
    .with_child_abort_ack(acknowledgment);
    let store = open_store(&directory, Some(controller))?;
    store.seal().map_err(|error| error.to_string())?;
    Err("publication child returned past abort checkpoint".to_owned())
}

#[cfg(unix)]
fn publication_child(
    path: &Path,
    fault: StorageFaultKind,
    op_index: u32,
    planned_segment: zeppelin_embed::segment::SegmentId,
    child_test_name: &str,
) -> Result<ChildAbortEvidence, String> {
    use std::io::Read as _;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::{Command, Stdio};
    let (mut parent, child) = UnixStream::pair().map_err(|error| error.to_string())?;
    let fault_key = match fault {
        StorageFaultKind::ManifestPreRenameCrash => "pre",
        StorageFaultKind::ManifestPostRenameCrash => "post",
        _ => return Err("non-publication child fault".to_owned()),
    };
    let mut command = Command::new(std::env::current_exe().map_err(|error| error.to_string())?);
    let child: OwnedFd = child.into();
    command
        .arg("--exact")
        .arg(child_test_name)
        .arg("--ignored")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env(STORAGE_CHILD_DIRECTORY, path)
        .env(STORAGE_CHILD_FAULT, fault_key)
        .env(STORAGE_CHILD_OP_INDEX, op_index.to_string())
        .env(STORAGE_CHILD_SEGMENT, planned_segment.to_string())
        .stdin(Stdio::from(child));
    let mut process = command.spawn().map_err(|error| error.to_string())?;
    drop(command);
    let status = process.wait().map_err(|error| error.to_string())?;
    let mut acknowledgment = String::new();
    parent
        .read_to_string(&mut acknowledgment)
        .map_err(|error| error.to_string())?;
    let signal = status
        .signal()
        .ok_or_else(|| format!("publication child did not exit by signal: {status:?}"))?;
    if signal != SIGABRT_SIGNAL {
        return Err(format!(
            "publication child signal {signal}, expected SIGABRT"
        ));
    }
    let (site, rename_performed, receipt_fault) = match fault {
        StorageFaultKind::ManifestPreRenameCrash => (
            "ManifestCommit.BeforeRename",
            false,
            "manifest-pre-rename-crash",
        ),
        StorageFaultKind::ManifestPostRenameCrash => (
            "ManifestCommit.AfterRename",
            true,
            "manifest-post-rename-crash",
        ),
        _ => return Err("non-publication child fault".to_owned()),
    };
    let expected_acknowledgment = format!(
        "campaign=storage-durability|operation=publication|fault={receipt_fault}|site={site}|op_index={op_index}|cardinality=1|artifact={MANIFEST}|temporary=.manifest.ze.tmp|committed={MANIFEST}|rename_performed={rename_performed}|new_segment_final=true|directory_sync_returned=false\n"
    );
    if acknowledgment != expected_acknowledgment {
        return Err(format!(
            "publication acknowledgment differs: {acknowledgment:?}"
        ));
    }
    let receipt = StorageFaultReceipt::from_manifest_abort_acknowledgment(&acknowledgment)?;
    Ok(ChildAbortEvidence {
        signal,
        acknowledgment,
        fault,
        site,
        op_index,
        artifact: MANIFEST.to_owned(),
        temporary: ".manifest.ze.tmp".to_owned(),
        committed: MANIFEST.to_owned(),
        rename_performed,
        new_segment_final: true,
        directory_sync_returned: false,
        receipt,
    })
}

#[cfg(unix)]
fn observe_publication_from_operation_fixture(
    base_evidence: &StorageEpisodeBaseEvidence,
    operation_fixture: &StorageOperationFixture,
    op_index: u32,
    fault: Option<StorageFaultKind>,
    child_test_name: &str,
) -> Result<PublicationOperationEvidence, String> {
    if operation_fixture.evidence.operation != StorageOperationKind::Publication {
        return Err("publication observer received the wrong operation fixture".to_owned());
    }
    let base = copy_directory(operation_fixture.path())?;
    let fixture = base_evidence.fixture.clone();
    let document_count = fixture.documents.len();
    let old_count = base_evidence.bootstrap_document_count;
    if old_count
        .checked_add(1)
        .is_none_or(|count| count != document_count)
    {
        return Err("publication operation fixture lacks exactly one pending mutation".to_owned());
    }
    let mut bootstrap_acks = base_evidence.ack_ledger.clone();
    let legal = match fault {
        Some(StorageFaultKind::ManifestPreRenameCrash) => {
            vec![independent::PublicationClass::Old]
        }
        Some(StorageFaultKind::ManifestPostRenameCrash) | None => {
            vec![independent::PublicationClass::New]
        }
        Some(other) => return Err(format!("{other:?} is not a publication fault")),
    };
    let pending_document = fixture
        .documents
        .get(old_count)
        .ok_or_else(|| "publication fixture lacks its pending document".to_owned())?;
    let pending_mutation = fixture
        .mutations
        .get(old_count)
        .ok_or_else(|| "publication fixture lacks its pending mutation".to_owned())?;
    let pending_store = open_store(base.path(), None)?;
    let pending_result = pending_store
        .ingest(IngestBatch::new(vec![document(pending_document)?]))
        .map_err(|error| error.to_string())?;
    pending_store.close().map_err(|error| error.to_string())?;
    bootstrap_acks.push(ack_evidence(
        "publication-pending",
        pending_mutation,
        Ok((pending_result.seq().get(), pending_result.generation())),
    ));
    let (old_control, old_complete) = raw_snapshot_state(base.path(), &fixture, document_count)?;
    if !old_complete {
        return Err("clean old publication references an invalid segment".to_owned());
    }
    let planned_segment = zeppelin_embed::segment::SegmentId::from_bytes(
        independent::planned_publication_segment_id(&fixture)?,
    );
    let clean = copy_directory(base.path())?;
    let faulted = copy_directory(base.path())?;
    let pre_control = capture_pre_operation_control(clean.path(), faulted.path())?;
    let clean_store = open_store(clean.path(), None)?;
    clean_store.seal().map_err(|error| error.to_string())?;
    clean_store.close().map_err(|error| error.to_string())?;
    let (new_control, new_complete) = raw_snapshot_state(clean.path(), &fixture, document_count)?;
    if !new_complete {
        return Err("clean new publication references an invalid segment".to_owned());
    }
    let expected = independent::PublicationExpected::from_fixture(
        &fixture,
        &old_control.segments,
        &new_control.segments,
        legal,
    )?;
    if expected.classify(&old_control) != independent::PublicationClass::Old {
        return Err("clean old publication differs from the primitive fixture model".to_owned());
    }
    if expected.classify(&new_control) != independent::PublicationClass::New {
        return Err("clean new publication differs from the primitive fixture model".to_owned());
    }
    if new_control
        .segments
        .iter()
        .all(|segment| segment.id != *planned_segment.as_bytes())
    {
        return Err("clean publication omitted the independently planned segment id".to_owned());
    }
    let receipt_expected = match fault {
        Some(value @ StorageFaultKind::ManifestPreRenameCrash)
        | Some(value @ StorageFaultKind::ManifestPostRenameCrash) => {
            let rename_performed = value == StorageFaultKind::ManifestPostRenameCrash;
            Some(independent::ReceiptExpected {
                campaign: "storage-durability",
                operation: independent::ReceiptOperation::Publication,
                fault: if rename_performed {
                    independent::ReceiptFault::ManifestPostRenameCrash
                } else {
                    independent::ReceiptFault::ManifestPreRenameCrash
                },
                site: if rename_performed {
                    independent::ReceiptSite::ManifestCommitAfterRename
                } else {
                    independent::ReceiptSite::ManifestCommitBeforeRename
                },
                op_index,
                artifact: independent::ArtifactFact::Manifest {
                    path: MANIFEST.to_owned(),
                },
                effect: independent::ReceiptEffectFact::ManifestRename {
                    temporary: ".manifest.ze.tmp".to_owned(),
                    committed: MANIFEST.to_owned(),
                    rename_performed,
                    new_segment_final: true,
                    directory_sync_returned: false,
                },
            })
        }
        None => None,
        Some(other) => return Err(format!("{other:?} is not a publication fault")),
    };
    let child = match fault {
        Some(value @ StorageFaultKind::ManifestPreRenameCrash) => Some(publication_child(
            faulted.path(),
            value,
            op_index,
            planned_segment,
            child_test_name,
        )?),
        Some(value @ StorageFaultKind::ManifestPostRenameCrash) => Some(publication_child(
            faulted.path(),
            value,
            op_index,
            planned_segment,
            child_test_name,
        )?),
        None => {
            let store = open_store(faulted.path(), None)?;
            store.seal().map_err(|error| error.to_string())?;
            store.close().map_err(|error| error.to_string())?;
            None
        }
        Some(other) => return Err(format!("{other:?} is not a publication fault")),
    };
    let (raw, referenced_segments_complete) =
        raw_snapshot_state(faulted.path(), &fixture, document_count)?;
    let class = expected.classify(&raw);
    let public = public_snapshot_state(faulted.path(), &fixture)?;
    let (_, parsed_wal) = wal(faulted.path())?;
    let trusted_wal_end = match parsed_wal.records.last() {
        Some(record) => record.seq,
        None => parsed_wal
            .first_seq
            .checked_sub(1)
            .ok_or_else(|| "empty WAL first sequence is zero".to_owned())?,
    };
    let observed = independent::PublicationObserved {
        class,
        raw,
        public,
        referenced_segments_complete,
        trusted_wal_end,
    };
    let receipt = child.as_ref().map(|child| child.receipt.clone());
    let receipt_observed = receipt
        .as_ref()
        .map(receipt_observed_from_product)
        .transpose()?;
    let control = control(&fixture, pre_control, clean.path(), faulted.path())?;
    let evidence = evidence_envelope(&fixture, bootstrap_acks, clean.path(), faulted.path())?;
    Ok(PublicationOperationEvidence {
        expected,
        observed,
        receipt_expected,
        receipt_observed,
        receipt,
        child,
        control,
        mutation: StorageMutationEvidence {
            artifact: MANIFEST.to_owned(),
            offset: None,
            segment: None,
            region_kind: None,
            chunk: None,
            before: None,
            after: None,
        },
        evidence,
    })
}

/// Observes publication from this episode's dedicated sealed operation root.
#[cfg(unix)]
pub fn observe_publication_from_episode(
    episode: &StorageEpisodeFixtures,
    op_index: u32,
    fault: Option<StorageFaultKind>,
    child_test_name: &str,
) -> Result<PublicationOperationEvidence, String> {
    observe_publication_from_operation_fixture(
        episode.base_evidence(),
        episode.operation_fixture(StorageOperationKind::Publication)?,
        op_index,
        fault,
        child_test_name,
    )
}

/// Compatibility wrapper for focused callers that own only one operation.
#[cfg(unix)]
pub fn observe_publication(
    seed: u64,
    op_index: u32,
    fault: Option<StorageFaultKind>,
    child_test_name: &str,
) -> Result<PublicationOperationEvidence, String> {
    let episode = build_storage_episode_fixtures(seed)?;
    observe_publication_from_episode(&episode, op_index, fault, child_test_name)
}

/// Decodes and executes one retained operation without deriving any fixture from its seed.
#[cfg(unix)]
pub fn run_storage_operation_from_fixture(
    retained_bytes: &[u8],
    publication_child_test_name: &str,
) -> Result<RetainedStorageOperationEvidence, String> {
    let retained = decode_storage_fixture(retained_bytes)?;
    let episode = build_storage_episode_fixtures_from_literal(retained.fixture)?;
    match retained.operation {
        StorageOperationKind::WalPrefix => {
            observe_wal_prefix_from_episode(&episode, retained.op_index, retained.fault)
                .map(RetainedStorageOperationEvidence::WalPrefix)
        }
        StorageOperationKind::Publication => observe_publication_from_episode(
            &episode,
            retained.op_index,
            retained.fault,
            publication_child_test_name,
        )
        .map(RetainedStorageOperationEvidence::Publication),
        StorageOperationKind::Retry => {
            observe_retry_from_episode(&episode, retained.op_index, retained.fault)
                .map(RetainedStorageOperationEvidence::Retry)
        }
        StorageOperationKind::FormatCheck => observe_format_case_from_episode(
            &episode,
            retained.op_index,
            retained
                .format_case
                .ok_or_else(|| "decoded retained format-check case disappeared".to_owned())?,
            retained.fault.is_some(),
        )
        .map(RetainedStorageOperationEvidence::FormatCheck),
        StorageOperationKind::OrphanCleanup => observe_reachability_case_from_episode(
            &episode,
            retained.op_index,
            retained.omission_case,
        )
        .map(RetainedStorageOperationEvidence::OrphanCleanup),
    }
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;

    fn assert_receipt_pair(
        expected: Option<&independent::ReceiptExpected>,
        observed: Option<&independent::ReceiptObserved>,
        receipt: Option<&StorageFaultReceipt>,
    ) {
        assert_eq!(
            observed.map(|receipt| &receipt.value),
            expected,
            "fixture-planned and production-observed receipts differ"
        );
        assert!(
            observed.is_none_or(|receipt| receipt.cardinality == 1),
            "production receipt was not one-shot"
        );
        let rebound = receipt
            .map(receipt_observed_from_product)
            .transpose()
            .expect("typed production receipt must convert exhaustively");
        assert_eq!(rebound.as_ref(), observed);
    }

    fn assert_byte_identical_pre_operation_control(control: &StorageControlEvidence) {
        assert_eq!(control.pre_clean_inventory, control.pre_fault_inventory);
        assert_eq!(control.pre_clean_digest, control.pre_fault_digest);
        assert_eq!(
            control
                .pre_clean_artifacts
                .iter()
                .map(|artifact| (&artifact.fact, &artifact.bytes))
                .collect::<Vec<_>>(),
            control
                .pre_fault_artifacts
                .iter()
                .map(|artifact| (&artifact.fact, &artifact.bytes))
                .collect::<Vec<_>>()
        );
    }

    fn assert_canonical_evidence(
        fixture: &independent::StorageFixtureV1,
        evidence: &StorageEvidenceEnvelope,
    ) {
        assert_eq!(&evidence.fixture, fixture);
        assert!(
            !evidence.ack_ledger.is_empty(),
            "storage evidence must retain actual public acknowledgement attempts"
        );
        assert!(
            !evidence.artifacts.is_empty(),
            "storage evidence must retain post-operation artifacts"
        );
        for artifact in &evidence.artifacts {
            assert_eq!(artifact.fact.length, artifact.bytes.len() as u64);
            assert_eq!(
                artifact.fact.digest,
                digest32(0x4649_4c45_4641_4354, &artifact.bytes)
            );
        }
    }

    #[test]
    #[ignore = "self-spawned publication crash checkpoint"]
    fn publication_abort_child() {
        super::publication_child_from_env()
            .expect("publication child must abort before returning an error");
    }

    #[test]
    fn storage_family_copy_directory_traverses_lexicographically() {
        let source = tempdir().expect("create copy source");
        for name in ["zeta", "alpha", "middle"] {
            std::fs::write(source.path().join(name), name.as_bytes()).expect("write source file");
        }

        let (copy, traversal) =
            copy_directory_with_traversal(source.path()).expect("copy storage fixture");

        assert_eq!(traversal, ["alpha", "middle", "zeta"]);
        assert_eq!(
            inventory(copy.path()).expect("inventory copied fixture"),
            inventory(source.path()).expect("inventory source fixture")
        );
    }

    #[test]
    fn storage_family_episode_base_forks_five_byte_identical_operation_fixtures() {
        let episode = build_storage_episode_fixtures(67).expect("build storage episode fixtures");
        let base = episode.base_evidence();
        assert!(base.bootstrap_document_count >= 2);
        assert_eq!(base.fixture.seed, 67);
        assert!(!base.snapshot.segments.is_empty());
        assert!(base.referenced_segments_complete);
        assert_eq!(
            base.ack_ledger.len(),
            base.bootstrap_document_count,
            "base evidence must retain every real durable acknowledgement"
        );
        assert!(base.ack_ledger.iter().all(|ack| {
            ack.returned_ok
                && ack.acknowledged
                && ack.durability == "Durable"
                && ack.commit_tier == "Ordered"
        }));

        assert_eq!(
            episode
                .operation_fixtures()
                .iter()
                .map(|operation| operation.evidence.operation)
                .collect::<Vec<_>>(),
            STORAGE_OPERATION_KINDS
        );
        let expected_traversal = base
            .inventory
            .iter()
            .map(|file| file.path.clone())
            .collect::<Vec<_>>();
        for operation in episode.operation_fixtures() {
            assert_eq!(operation.evidence.traversal, expected_traversal);
            assert_eq!(operation.evidence.source_inventory, base.inventory);
            assert_eq!(operation.evidence.destination_inventory, base.inventory);
            assert_eq!(operation.evidence.source_digest, base.inventory_digest);
            assert_eq!(operation.evidence.destination_digest, base.inventory_digest);
            assert_eq!(
                operation
                    .evidence
                    .destination_artifacts
                    .iter()
                    .map(|artifact| (&artifact.fact, &artifact.bytes))
                    .collect::<Vec<_>>(),
                base.artifacts
                    .iter()
                    .map(|artifact| (&artifact.fact, &artifact.bytes))
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn storage_family_episode_entry_points_consume_their_owned_forks() {
        let episode = build_storage_episode_fixtures(68).expect("build shared storage episode");
        let base_acks = episode.base_evidence().ack_ledger.clone();

        let wal = observe_wal_prefix_from_episode(&episode, 20, None)
            .expect("observe WAL operation fork");
        let publication =
            observe_publication_from_episode(&episode, 21, None, PUBLICATION_CHILD_TEST_NAME)
                .expect("observe publication operation fork");
        let retry =
            observe_retry_from_episode(&episode, 22, Some(StorageFaultKind::PostCommitError))
                .expect("observe retry operation fork");
        let format = observe_format_case_from_episode(
            &episode,
            23,
            independent::FormatCase::SegmentWrongFamily,
            false,
        )
        .expect("observe format operation fork");
        let reachability = observe_reachability_from_episode(&episode, 24, None)
            .expect("observe cleanup operation fork");

        for ack_ledger in [
            &wal.evidence.ack_ledger,
            &publication.evidence.ack_ledger,
            &retry.evidence.ack_ledger,
            &format.evidence.ack_ledger,
            &reachability.evidence.ack_ledger,
        ] {
            assert!(
                ack_ledger.starts_with(&base_acks),
                "operation evidence must retain the shared base acknowledgement ledger"
            );
        }
        for operation in episode.operation_fixtures() {
            assert_eq!(
                inventory(operation.path()).expect("inventory retained operation root"),
                operation.evidence.destination_inventory,
                "operation adapter must clone rather than mutate its episode root"
            );
        }
    }

    #[test]
    fn storage_family_omission_schedule_rotates_the_complete_catalog() {
        let observed = (0..6)
            .map(|profile| omission_case_for_schedule(71, profile))
            .collect::<Vec<_>>();
        assert_eq!(observed.len(), independent::ALL_OMISSION_CASES.len());
        for case in independent::ALL_OMISSION_CASES {
            assert!(observed.contains(&case));
        }
    }

    #[test]
    fn retained_storage_fixture_codec_round_trips_literal_operation_identity() {
        let mut fixture = independent::StorageFixtureV1::derive(91);
        fixture.seed = 9_091;
        let retained = RetainedStorageOperationV1::new(
            fixture.clone(),
            StorageOperationKind::WalPrefix,
            37,
            Some(StorageFaultKind::TornWalChecksum),
            Some(independent::FormatCase::WalRecordChecksum),
            None,
        )
        .expect("valid retained WAL schedule");

        let bytes = encode_storage_fixture(&retained).expect("encode retained storage fixture");
        assert_eq!(&bytes[..8], b"ZESTOR01");
        assert_eq!(u16::from_le_bytes([bytes[8], bytes[9]]), 1);
        assert_eq!(
            decode_storage_fixture(&bytes).expect("decode retained storage fixture"),
            retained
        );
        assert_eq!(retained.fixture, fixture);
    }

    #[test]
    fn retained_storage_fixture_codec_fails_closed_on_version_corruption_and_contract_drift() {
        let retained = RetainedStorageOperationV1::new(
            independent::StorageFixtureV1::derive(92),
            StorageOperationKind::Retry,
            38,
            Some(StorageFaultKind::PostCommitError),
            None,
            None,
        )
        .expect("valid retained retry schedule");
        let bytes = encode_storage_fixture(&retained).expect("encode retained retry fixture");

        let mut unknown_version = bytes.clone();
        unknown_version[8..10].copy_from_slice(&2_u16.to_le_bytes());
        assert_eq!(
            decode_storage_fixture(&unknown_version)
                .expect_err("unknown retained version must fail closed"),
            "retained storage fixture version 2 is unsupported"
        );

        let mut corrupt = bytes;
        *corrupt.last_mut().expect("retained fixture payload") ^= 1;
        assert_eq!(
            decode_storage_fixture(&corrupt)
                .expect_err("corrupt retained payload must fail closed"),
            "retained storage fixture payload checksum differs"
        );

        let mut unknown_scheme = independent::StorageFixtureV1::derive(92);
        unknown_scheme.scheme = 99;
        let error = RetainedStorageOperationV1::new(
            unknown_scheme,
            StorageOperationKind::Retry,
            38,
            None,
            None,
            None,
        )
        .expect_err("unknown retained product contract must fail closed");
        assert_eq!(error, "retained storage fixture scheme is unsupported: 99");
    }

    #[test]
    fn retained_storage_fixture_executes_literal_bytes_for_all_five_operations() {
        let mut fixture = independent::StorageFixtureV1::derive(93);
        fixture.seed = 90_093;
        let schedules = [
            RetainedStorageOperationV1::new(
                fixture.clone(),
                StorageOperationKind::WalPrefix,
                41,
                Some(StorageFaultKind::TornWalChecksum),
                Some(independent::FormatCase::WalRecordChecksum),
                None,
            ),
            RetainedStorageOperationV1::new(
                fixture.clone(),
                StorageOperationKind::Publication,
                42,
                Some(StorageFaultKind::ManifestPostRenameCrash),
                None,
                None,
            ),
            RetainedStorageOperationV1::new(
                fixture.clone(),
                StorageOperationKind::Retry,
                43,
                Some(StorageFaultKind::PostCommitError),
                None,
                None,
            ),
            RetainedStorageOperationV1::new(
                fixture.clone(),
                StorageOperationKind::FormatCheck,
                44,
                Some(StorageFaultKind::WrongSegmentObject),
                Some(independent::FormatCase::SegmentWrongIdentity),
                None,
            ),
            RetainedStorageOperationV1::new(
                fixture.clone(),
                StorageOperationKind::OrphanCleanup,
                45,
                Some(StorageFaultKind::ListDeleteOmission),
                None,
                Some(independent::OmissionCase {
                    orphan: independent::OrphanKind::ManifestTemporary,
                    subsite: independent::OmissionSubsite::Delete,
                }),
            ),
        ]
        .map(|schedule| schedule.expect("valid retained operation schedule"));

        for schedule in schedules {
            let expected_operation = schedule.operation;
            let bytes = encode_storage_fixture(&schedule).expect("encode retained operation");
            let observed = run_storage_operation_from_fixture(&bytes, PUBLICATION_CHILD_TEST_NAME)
                .unwrap_or_else(|error| {
                    panic!(
                        "execute retained {} operation: {error}",
                        expected_operation.key()
                    )
                });
            assert_eq!(observed.operation(), expected_operation);
            assert_eq!(observed.fixture(), &fixture);
            match observed {
                RetainedStorageOperationEvidence::WalPrefix(evidence) => {
                    independent::check_i16(&evidence.expected, &evidence.observed)
                        .expect("literal WAL replay must pass I16");
                    assert!(matches!(
                        evidence.expected.terminator,
                        independent::WalTerminator::CorruptAt {
                            reason: independent::WalRecordFailure::ChecksumMismatch { .. },
                            ..
                        }
                    ));
                }
                RetainedStorageOperationEvidence::Publication(evidence) => {
                    independent::check_i15(&evidence.expected, &evidence.observed)
                        .expect("literal publication replay must pass I15");
                    assert_eq!(
                        evidence.child.as_ref().map(|child| child.fault),
                        Some(StorageFaultKind::ManifestPostRenameCrash)
                    );
                }
                RetainedStorageOperationEvidence::Retry(evidence) => {
                    independent::check_i17(&evidence.expected, &evidence.observed)
                        .expect("literal retry replay must pass I17");
                    assert!(evidence.receipt.is_some());
                }
                RetainedStorageOperationEvidence::FormatCheck(evidence) => {
                    independent::check_i18(&evidence.expected, &evidence.observed)
                        .expect("literal format replay must pass I18");
                    assert_eq!(
                        evidence.expected.case,
                        independent::FormatCase::SegmentWrongIdentity
                    );
                }
                RetainedStorageOperationEvidence::OrphanCleanup(evidence) => {
                    independent::check_i19(&evidence.expected, &evidence.observed)
                        .expect("literal cleanup replay must pass I19");
                    assert!(matches!(
                        evidence.omission,
                        Some(OmissionIntermediateEvidence { .. })
                    ));
                }
            }
        }
    }

    #[test]
    fn storage_family_wal_prefix_adapter_emits_exact_receipts() {
        for (fault, kind) in [
            (
                StorageFaultKind::TornWalHeader,
                independent::WalMutationKind::HeaderTruncation,
            ),
            (
                StorageFaultKind::TornWalBody,
                independent::WalMutationKind::BodyTruncation,
            ),
            (
                StorageFaultKind::TornWalChecksum,
                independent::WalMutationKind::ChecksumFlip,
            ),
        ] {
            let evidence = observe_wal_prefix(17, 3, Some(fault)).expect("observe WAL prefix");
            assert_receipt_pair(
                evidence.receipt_expected.as_ref(),
                evidence.receipt_observed.as_ref(),
                evidence.receipt.as_ref(),
            );
            assert_byte_identical_pre_operation_control(&evidence.control);
            assert_canonical_evidence(&fixture(17), &evidence.evidence);
            assert_eq!(
                evidence.mutation.offset,
                Some(
                    independent::planned_wal_mutation_offset(&fixture(17), kind)
                        .expect("fixture-authored WAL offset")
                )
            );
        }
    }

    #[test]
    fn storage_family_wal_prefix_uses_actual_durable_ack_ledger() {
        let evidence = observe_wal_prefix(17, 3, Some(StorageFaultKind::TornWalBody))
            .expect("observe WAL prefix with actual acknowledgements");
        let primitive = fixture(17);
        assert_eq!(evidence.ack_ledger.len(), primitive.mutations.len());
        assert!(
            evidence
                .ack_ledger
                .iter()
                .take(primitive.mutations.len().saturating_sub(1))
                .all(|ack| ack.returned_ok && ack.acknowledged)
        );
        let tail = evidence.ack_ledger.last().expect("unacknowledged tail");
        assert!(!tail.returned_ok);
        assert!(!tail.acknowledged);
        assert!(
            evidence
                .ack_ledger
                .iter()
                .all(|ack| ack.durability == "Durable" && ack.commit_tier == "Ordered")
        );
        assert_eq!(
            evidence.expected.acknowledged.len(),
            evidence
                .ack_ledger
                .iter()
                .filter(|ack| ack.acknowledged)
                .count()
        );
        assert_eq!(
            evidence.observed.reopened_ack_boundaries.len(),
            evidence.expected.acknowledged.len(),
            "every returned durable ack must have its own public reopen"
        );
        assert_eq!(
            evidence.observed.reopened_ack_boundaries,
            evidence.expected.ack_boundaries
        );
    }

    #[test]
    fn storage_family_retry_adapter_emits_exact_receipt() {
        let retry =
            observe_retry(18, 4, Some(StorageFaultKind::PostCommitError)).expect("observe retry");
        assert_receipt_pair(
            retry.receipt_expected.as_ref(),
            retry.receipt_observed.as_ref(),
            retry.receipt.as_ref(),
        );
        assert_byte_identical_pre_operation_control(&retry.control);
        assert_canonical_evidence(&fixture(18), &retry.evidence);
    }

    #[test]
    fn storage_family_retry_covers_sealed_reopened_resolution() {
        let retry = observe_retry(18, 4, Some(StorageFaultKind::PostCommitError))
            .expect("observe active and sealed/reopened retry");
        independent::check_i17(&retry.expected, &retry.observed)
            .expect("active and sealed/reopened retry must both converge");
        let sealed = retry
            .observed
            .sealed_reopened
            .as_ref()
            .expect("sealed/reopened evidence");
        assert_eq!(sealed.retry_seq, retry.expected.original_seq);
        assert_eq!(
            sealed.generation_before_retry,
            sealed.generation_after_retry
        );
        assert_eq!(
            sealed.wal_digest_before_retry,
            sealed.wal_digest_after_retry
        );
        assert_eq!(sealed.live_version_occurrences, 1);
    }

    #[test]
    fn storage_family_format_adapter_emits_exact_receipts() {
        for fault in [
            StorageFaultKind::CorruptSegmentRegion,
            StorageFaultKind::WrongManifestObject,
            StorageFaultKind::WrongSegmentObject,
        ] {
            let evidence = observe_format(19, 5, Some(fault)).expect("observe format refusal");
            assert_receipt_pair(
                evidence.receipt_expected.as_ref(),
                evidence.receipt_observed.as_ref(),
                evidence.receipt.as_ref(),
            );
            assert_byte_identical_pre_operation_control(&evidence.control);
            assert_canonical_evidence(&fixture(19), &evidence.evidence);
        }
        let clean_format = observe_format(20, 6, None).expect("observe unarmed format refusal");
        assert_receipt_pair(
            clean_format.receipt_expected.as_ref(),
            clean_format.receipt_observed.as_ref(),
            clean_format.receipt.as_ref(),
        );
        assert_byte_identical_pre_operation_control(&clean_format.control);
        assert_canonical_evidence(&fixture(20), &clean_format.evidence);
    }

    #[test]
    fn storage_family_wrong_segment_covers_family_and_identity_refusals() {
        let family = observe_format(18, 5, Some(StorageFaultKind::WrongSegmentObject))
            .expect("observe cross-family segment refusal");
        assert!(matches!(
            family.observed.refusal,
            Some(independent::FormatRefusal::SegmentFormat {
                check: independent::FormatCheckFact::Family,
                ..
            })
        ));
        assert!(matches!(
            family
                .receipt_observed
                .as_ref()
                .map(|receipt| &receipt.value.site),
            Some(independent::ReceiptSite::SegmentOpenFamilyValidation)
        ));

        let identity = observe_format(19, 5, Some(StorageFaultKind::WrongSegmentObject))
            .expect("observe same-family segment identity refusal");
        assert!(matches!(
            identity.observed.refusal,
            Some(independent::FormatRefusal::SegmentWrongObject { .. })
        ));
        assert!(matches!(
            identity
                .receipt_observed
                .as_ref()
                .map(|receipt| &receipt.value.site),
            Some(independent::ReceiptSite::SegmentOpenObjectIdentity)
        ));
    }

    #[test]
    fn storage_family_i18_exposes_the_complete_typed_public_case_catalog() {
        let wal_cases = [
            (
                StorageFaultKind::TornWalHeader,
                independent::FormatCase::WalHeader,
            ),
            (
                StorageFaultKind::TornWalBody,
                independent::FormatCase::WalRecordBody,
            ),
            (
                StorageFaultKind::TornWalChecksum,
                independent::FormatCase::WalRecordChecksum,
            ),
        ];
        let mut observed_cases = Vec::new();
        for (fault, expected_case) in wal_cases {
            let wal = observe_wal_prefix(31, 9, Some(fault)).expect("observe typed WAL refusal");
            let (expected, observed) =
                format_dtos_from_wal_prefix(&wal).expect("project WAL refusal into I18");
            assert_eq!(expected.case, expected_case);
            independent::check_i18(&expected, &observed).expect("WAL I18 case passes");
            observed_cases.push(expected.case);
        }
        for case in [
            independent::FormatCase::SegmentRegion,
            independent::FormatCase::ManifestWrongFamily,
            independent::FormatCase::SegmentWrongFamily,
            independent::FormatCase::SegmentWrongIdentity,
        ] {
            let evidence =
                observe_format_case(32, 10, case, false).expect("observe explicit I18 case");
            assert_eq!(evidence.expected.case, case);
            independent::check_i18(&evidence.expected, &evidence.observed)
                .expect("persisted-object I18 case passes");
            observed_cases.push(case);
        }
        assert_eq!(observed_cases, independent::ALL_FORMAT_CASES);
    }

    #[test]
    fn storage_family_segment_mutation_uses_fixture_region_chunk_and_byte() {
        let primitive = fixture(33);
        let evidence = observe_format_case(33, 11, independent::FormatCase::SegmentRegion, false)
            .expect("observe fixture-selected segment mutation");
        assert_eq!(
            evidence.mutation.region_kind,
            Some(primitive.segment_region_kind)
        );
        assert_eq!(evidence.mutation.chunk, Some(primitive.segment_chunk));
        let artifact = evidence
            .control
            .pre_fault_artifacts
            .iter()
            .find(|artifact| artifact.fact.path == evidence.mutation.artifact)
            .expect("pre-mutation segment bytes");
        let parsed = independent::parse_segment(&artifact.fact.path, &artifact.bytes)
            .expect("parse pre-mutation segment");
        let region = parsed
            .regions
            .iter()
            .find(|region| region.kind == primitive.segment_region_kind)
            .expect("fixture-selected region");
        let expected_offset = region.offset
            + u64::from(primitive.segment_chunk) * 64 * 1024
            + u64::from(primitive.segment_byte);
        assert_eq!(evidence.mutation.offset, Some(expected_offset));
    }

    #[test]
    fn storage_family_reachability_adapter_emits_exact_receipt() {
        let reachability = observe_reachability(21, 7, Some(StorageFaultKind::ListDeleteOmission))
            .expect("observe omission retry");
        assert_receipt_pair(
            reachability.receipt_expected.as_ref(),
            reachability.receipt_observed.as_ref(),
            reachability.receipt.as_ref(),
        );
        assert_byte_identical_pre_operation_control(&reachability.control);
        assert_canonical_evidence(&fixture(21), &reachability.evidence);
    }

    #[test]
    fn storage_family_reachability_covers_every_omission_case() {
        for (index, case) in independent::ALL_OMISSION_CASES.into_iter().enumerate() {
            let evidence = observe_reachability_case(41, index as u32, Some(case))
                .expect("observe explicit omission case");
            independent::check_i19(&evidence.expected, &evidence.observed)
                .expect("omission recovery must satisfy I19");
            assert_receipt_pair(
                evidence.receipt_expected.as_ref(),
                evidence.receipt_observed.as_ref(),
                evidence.receipt.as_ref(),
            );
            let omitted = match case.orphan {
                independent::OrphanKind::FinalSegment => &fixture(41).eligible_orphans[0],
                independent::OrphanKind::SegmentTemporary => &fixture(41).eligible_orphans[1],
                independent::OrphanKind::ManifestTemporary => &fixture(41).eligible_orphans[2],
            };
            assert_eq!(&evidence.mutation.artifact, omitted);
            assert_eq!(
                evidence
                    .receipt_observed
                    .as_ref()
                    .map(|receipt| receipt.value.site),
                Some(match case.subsite {
                    independent::OmissionSubsite::List => {
                        independent::ReceiptSite::OrphanCleanupList
                    }
                    independent::OmissionSubsite::Delete => {
                        independent::ReceiptSite::OrphanCleanupDelete
                    }
                })
            );
        }
    }

    #[test]
    fn storage_family_publication_adapters_use_true_abort_children() {
        for fault in [
            StorageFaultKind::ManifestPreRenameCrash,
            StorageFaultKind::ManifestPostRenameCrash,
        ] {
            let evidence = observe_publication(22, 8, Some(fault), PUBLICATION_CHILD_TEST_NAME)
                .expect("observe publication abort");
            assert_receipt_pair(
                evidence.receipt_expected.as_ref(),
                evidence.receipt_observed.as_ref(),
                evidence.receipt.as_ref(),
            );
            assert_byte_identical_pre_operation_control(&evidence.control);
            assert_canonical_evidence(&fixture(22), &evidence.evidence);
            assert_eq!(
                evidence.child.as_ref().map(|child| child.signal),
                Some(SIGABRT_SIGNAL)
            );
        }
    }

    #[test]
    fn storage_family_publication_starts_from_a_sealed_old_state() {
        let evidence = observe_publication(22, 8, None, PUBLICATION_CHILD_TEST_NAME)
            .expect("observe clean publication from sealed state");
        assert!(
            !evidence.expected.old.segments.is_empty(),
            "old publication state must retain a sealed segment"
        );
        assert_eq!(
            evidence.expected.new.segments.len(),
            evidence.expected.old.segments.len() + 1,
            "publication must add exactly the independently planned segment"
        );
        for old in &evidence.expected.old.segments {
            assert!(evidence.expected.new.segments.contains(old));
        }
    }

    #[test]
    pub(crate) fn storage_oracle_i15_plant_is_rejected() {
        let mut evidence = observe_publication(52, 12, None, PUBLICATION_CHILD_TEST_NAME)
            .expect("observe clean publication");
        evidence.observed.referenced_segments_complete = false;
        assert_eq!(
            independent::check_i15(&evidence.expected, &evidence.observed)
                .expect_err("missing referenced segment plant must fail")
                .to_string(),
            "I15.storage-publication-v1: referenced segment missing"
        );
        evidence.observed.referenced_segments_complete = true;
        let clean_observed = evidence.observed.clone();
        evidence.observed.raw.segments = evidence
            .expected
            .new
            .segments
            .iter()
            .map(|segment| independent::SegmentFact {
                id: segment.id,
                rows: segment.rows,
                scheme: segment.scheme,
                dims: segment.dims,
                file_length: segment.file_length,
                header_checksum: segment.header_checksum,
                whole_file_checksum: segment.whole_file_checksum,
            })
            .collect();
        evidence.observed.raw.generation = evidence.expected.old.generation;
        evidence.observed.raw.manifest_log_seq = evidence.expected.old.manifest_log_seq;
        evidence.observed.raw.absorbed_through = evidence.expected.old.absorbed_through;
        evidence.observed.public = independent::PublishedSnapshotState {
            generation: evidence.observed.raw.generation,
            segments: evidence
                .observed
                .raw
                .segments
                .iter()
                .map(|segment| independent::PublishedSegmentFact {
                    id: segment.id,
                    rows: segment.rows,
                    scheme: segment.scheme,
                    dims: segment.dims,
                    file_length: segment.file_length,
                })
                .collect(),
            live_versions: evidence.observed.raw.live_versions.clone(),
            absorbed_through: evidence.observed.raw.absorbed_through,
        };
        evidence.observed.class = evidence.expected.classify(&evidence.observed.raw);
        assert_eq!(
            independent::check_i15(&evidence.expected, &evidence.observed)
                .expect_err("new-segments-with-old-generation-and-log plant must fail")
                .to_string(),
            "I15.storage-publication-v1: observed hybrid publication"
        );
        evidence.observed = clean_observed;
        evidence.observed.raw.segments[0].whole_file_checksum ^= 1;
        evidence.observed.public = independent::PublishedSnapshotState {
            generation: evidence.observed.raw.generation,
            segments: evidence
                .observed
                .raw
                .segments
                .iter()
                .map(|segment| independent::PublishedSegmentFact {
                    id: segment.id,
                    rows: segment.rows,
                    scheme: segment.scheme,
                    dims: segment.dims,
                    file_length: segment.file_length,
                })
                .collect(),
            live_versions: evidence.observed.raw.live_versions.clone(),
            absorbed_through: evidence.observed.raw.absorbed_through,
        };
        evidence.observed.class = evidence.expected.classify(&evidence.observed.raw);
        assert_eq!(
            independent::check_i15(&evidence.expected, &evidence.observed)
                .expect_err("whole-file checksum plant must fail")
                .to_string(),
            "I15.storage-publication-v1: observed hybrid publication"
        );
    }

    #[test]
    pub(crate) fn storage_oracle_i16_plant_is_rejected() {
        let mut reordered = observe_wal_prefix(53, 13, None).expect("observe clean WAL");
        reordered.observed.records.swap(0, 1);
        assert_eq!(
            independent::check_i16(&reordered.expected, &reordered.observed)
                .expect_err("reordered WAL plant must fail")
                .to_string(),
            "I16.storage-wal-prefix-v1: record sequence is not a prefix"
        );
        let mut dropped = observe_wal_prefix(53, 13, None).expect("observe clean WAL");
        let retained = dropped
            .expected
            .acknowledged
            .len()
            .checked_sub(1)
            .expect("acknowledged record to drop");
        dropped.observed.records.truncate(retained);
        assert_eq!(
            independent::check_i16(&dropped.expected, &dropped.observed)
                .expect_err("dropped acknowledged record plant must fail")
                .to_string(),
            "I16.storage-wal-prefix-v1: acknowledged record missing or changed"
        );
        let mut torn = observe_wal_prefix(53, 13, Some(StorageFaultKind::TornWalBody))
            .expect("observe torn WAL");
        let tail = torn
            .expected
            .optional_unacknowledged_tail
            .pop()
            .expect("unacknowledged torn tail");
        torn.expected.acknowledged.push(tail);
        assert_eq!(
            independent::check_i16(&torn.expected, &torn.observed)
                .expect_err("torn-as-acknowledged plant must fail")
                .to_string(),
            "I16.storage-wal-prefix-v1: trusted records before corrupt tail differ from acknowledged model"
        );
    }

    #[test]
    pub(crate) fn storage_oracle_i17_plant_is_rejected() {
        let mut duplicate =
            observe_retry(54, 14, Some(StorageFaultKind::PostCommitError)).expect("observe retry");
        duplicate.observed.canonical_record_occurrences = 2;
        duplicate.observed.live_version_occurrences = 2;
        assert_eq!(
            independent::check_i17(&duplicate.expected, &duplicate.observed)
                .expect_err("duplicate WAL and logical occurrence plant must fail")
                .to_string(),
            "I17.storage-retry-idempotence-v1: retry duplicated mutation"
        );

        let mut evidence =
            observe_retry(54, 14, Some(StorageFaultKind::PostCommitError)).expect("observe retry");
        evidence.observed.generation_after_retry += 1;
        assert_eq!(
            independent::check_i17(&evidence.expected, &evidence.observed)
                .expect_err("retry generation plant must fail")
                .to_string(),
            "I17.storage-retry-idempotence-v1: retry changed generation"
        );
    }

    #[test]
    pub(crate) fn storage_oracle_i18_plant_is_rejected() {
        let mut evidence =
            observe_format_case(55, 15, independent::FormatCase::SegmentRegion, true)
                .expect("observe typed segment refusal");
        let original_refusal = evidence
            .observed
            .refusal
            .take()
            .expect("typed refusal fixture");
        assert_eq!(
            independent::check_i18(&evidence.expected, &evidence.observed)
                .expect_err("damaged artifact success plant must fail")
                .to_string(),
            "I18.storage-typed-artifact-refusal-v1: damaged artifact succeeded"
        );
        evidence.observed.refusal = Some(original_refusal);
        let Some(independent::FormatRefusal::SegmentFormat { check, .. }) =
            evidence.observed.refusal.as_mut()
        else {
            panic!("segment refusal fixture");
        };
        *check = independent::FormatCheckFact::FileChecksum;
        assert_eq!(
            independent::check_i18(&evidence.expected, &evidence.observed)
                .expect_err("file-checksum plant must fail")
                .to_string(),
            "I18.storage-typed-artifact-refusal-v1: typed artifact refusal differs"
        );
        evidence.observed.refusal = Some(evidence.expected.refusal.clone());
        let Some(independent::FormatRefusal::SegmentFormat { artifact, .. }) =
            evidence.observed.refusal.as_mut()
        else {
            panic!("segment artifact fixture");
        };
        *artifact = independent::ArtifactFact::SegmentRegion {
            path: "segment-sibling.zseg".to_owned(),
            id: [0xff; 16],
            kind: 5,
            chunk: 0,
        };
        assert_eq!(
            independent::check_i18(&evidence.expected, &evidence.observed)
                .expect_err("sibling-artifact plant must fail")
                .to_string(),
            "I18.storage-typed-artifact-refusal-v1: typed artifact refusal differs"
        );
    }

    #[test]
    pub(crate) fn storage_oracle_i19_plant_is_rejected() {
        let mut evidence = observe_reachability(56, 16, None).expect("observe cleanup");
        let reachable = evidence
            .expected
            .manifest_referenced
            .first()
            .expect("reachable segment")
            .clone();
        evidence
            .observed
            .final_inventory
            .retain(|file| file.path != reachable.path);
        assert_eq!(
            independent::check_i19(&evidence.expected, &evidence.observed)
                .expect_err("reachable deletion plant must fail")
                .to_string(),
            "I19.storage-reachability-v1: reachable file removed"
        );
        evidence.observed.final_inventory = evidence.expected.preserved.clone();
        evidence
            .observed
            .final_inventory
            .push(evidence.expected.eligible_orphans[0].clone());
        assert_eq!(
            independent::check_i19(&evidence.expected, &evidence.observed)
                .expect_err("retained orphan plant must fail")
                .to_string(),
            "I19.storage-reachability-v1: eligible orphan retained"
        );
    }
}
