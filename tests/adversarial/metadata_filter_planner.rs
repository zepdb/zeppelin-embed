//! Family-owned metadata/filter/planner fixtures and observation adapters.
//!
//! This module deliberately does not dispatch campaign operations, invoke the
//! I36-I39 checkers, or construct shared `OracleRecord`s. It derives one
//! operation-scoped fixture, runs public Store calls, and returns only the
//! independent DTOs plus production receipts and exact control/mutation facts.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tempfile::{TempDir, tempdir};
use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, Normalization, StoreEpoch,
};
use zeppelin_embed::fts::tokenizer::TokenizerConfig;
use zeppelin_embed::graph::search::GraphSearchProfile;
use zeppelin_embed::ingest::wal_payload::DELETE_V1;
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, RowSource,
    SearchRequest,
};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::{
    CancelToken, GraphSearchOptions, OpenOptions, QueryControl, QueryError, SearchOptions,
    SearchTier, Store, StoreError, StoreTestDependencies, SystemMonotonicClock,
};
use zeppelin_embed::manifest::io::{MANIFEST_FILE, commit_manifest, load_manifest};
use zeppelin_embed::meta::{
    Column, ColumnDefinition, ColumnId, ColumnStore, ColumnType, Predicate, PredicateValue,
    RangeBound, RangePredicate, Schema, TIMESTAMP_COLUMN, evaluate,
};
use zeppelin_embed::planner::{
    ALLOW_LIST_ROWS_THRESHOLD, FilteredSearchError, FilteredSearchOutcome,
    MetadataExecutionReceipt, MetadataFeatureReceipt, MetadataTestArm, MetadataTestController,
    PlanFallback, SegmentBranch,
};
use zeppelin_embed::scan::ScanOptions;
use zeppelin_embed::segment::layout::RegionKind;
use zeppelin_embed::segment::reader::SegmentReader;
use zeppelin_embed::segment::{
    ClusteringKeyRange, MetadataDecodeProvenance, SegmentError, SegmentId,
};
use zeppelin_embed::tier::{MaintenanceBudget, MaintenanceStatus, TierThresholds};
use zeppelin_embed::vfs::StdVfs;
use zeppelin_embed::wal::WalReader;
use zeppelin_embed_adversarial_oracle::metadata_filter_planner as independent;
use zeppelin_embed_adversarial_oracle::storage_durability::xxh3_64;

const GRAPH_ROWS: usize = 80;
const GRAPH_DIMS: usize = 128;
const INDEPENDENT_ALLOW_LIST_THRESHOLD: u64 = 64;
const GRAPH_EXPECTED_EF: u64 = 80;
const GRAPH_EXPECTED_VISITED_BUDGET: u64 = 80;

/// One metadata campaign operation, kept independent from shared dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataOperationKind {
    Columns,
    Bitmap,
    Planner,
    Execution,
}

/// One catalogued fault valid for exactly one metadata operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataFaultKind {
    ColumnCorruption,
    BitmapTruncation,
    SelectivityBoundary,
    VisitedBudgetFallback,
}

impl MetadataFaultKind {
    const fn operation(self) -> MetadataOperationKind {
        match self {
            Self::ColumnCorruption => MetadataOperationKind::Columns,
            Self::BitmapTruncation => MetadataOperationKind::Bitmap,
            Self::SelectivityBoundary | Self::VisitedBudgetFallback => {
                MetadataOperationKind::Execution
            }
        }
    }
}

/// Exact public result fact retained for clean/fault/retry comparison.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataResultFact {
    pub source: String,
    pub row_id: u32,
    pub document_id: Option<u128>,
    pub score_bits: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataFileDigestFact {
    pub relative_path: String,
    pub byte_length: u64,
    pub digest: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataDirectoryDigestEvidence {
    pub digest: u64,
    pub files: Vec<MetadataFileDigestFact>,
}

impl MetadataDirectoryDigestEvidence {
    fn empty() -> Self {
        Self {
            digest: 0,
            files: Vec::new(),
        }
    }
}

/// The directory relationship established before either same-seed leg runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataDirectoryRelation {
    UnpairedSingleDirectory,
    DistinctByteIdentical,
}

/// The exact public relation observed between clean, fault, and retry legs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataControlOutcome {
    Only,
    FilteredSucceeded,
    FaultAndRetryEquivalent,
    FaultRefusedRetryEquivalent,
    GraphFaultFallbackRetryEquivalent,
}

/// Same-seed public control relation for one operation invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataControlEvidence {
    pub namespace: &'static str,
    pub seed: u64,
    pub query_id_base: u64,
    pub normalized_schedule_digest: u64,
    pub directory_relation: MetadataDirectoryRelation,
    pub outcome: MetadataControlOutcome,
    pub clean_results: Vec<MetadataResultFact>,
    pub fault_results: Vec<MetadataResultFact>,
    pub retry_results: Vec<MetadataResultFact>,
    pub independent_expected_results: Vec<MetadataResultFact>,
    pub fault_error: Option<String>,
    pub clean_generation: u64,
    pub fault_generation: u64,
    pub retry_generation: u64,
    pub clean_wal_digest: u64,
    pub fault_wal_digest: u64,
    pub retry_wal_digest: u64,
    pub clean_source_digest: u64,
    pub fault_source_digest: u64,
    pub retry_source_digest: u64,
    pub clean_initial_directory: MetadataDirectoryDigestEvidence,
    pub fault_initial_directory: MetadataDirectoryDigestEvidence,
}

/// One checksum field rewritten after a semantic artifact mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MetadataChecksumField {
    TargetRegion {
        region: RegionKind,
    },
    TargetRegionChunk {
        region: RegionKind,
        chunk_index: u32,
    },
    ChecksumTableRegion,
    SegmentHeader,
    SegmentWholeFile,
    GraphInternal,
}

/// Exact before/after value and artifact offset for one checksum rewrite.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataChecksumRewriteEvidence {
    pub field: MetadataChecksumField,
    pub absolute_offset: u64,
    pub before: u64,
    pub after: u64,
}

/// Exact persisted change selected by a corruption fault.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataMutationEvidence {
    pub source: String,
    pub region: RegionKind,
    pub region_offset: u64,
    pub field_offset: u64,
    pub absolute_offset: u64,
    pub before: Vec<u8>,
    pub after: Vec<u8>,
    pub left_neighbor_before: Option<u8>,
    pub left_neighbor_after: Option<u8>,
    pub right_neighbor_before: Option<u8>,
    pub right_neighbor_after: Option<u8>,
    pub declared_bytes_before: u64,
    pub declared_bytes_after: u64,
    pub observed_bytes_after: u64,
    pub checksum_rewrites: Vec<MetadataChecksumRewriteEvidence>,
    pub post_mutation_artifact_digest: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MetadataProvenanceExpected {
    ColumnsPresenceTail {
        column_id: u32,
        row_count: u32,
        byte_offset: u64,
        observed_byte: u8,
        allowed_mask: u8,
    },
    ColumnsDictionaryCode {
        column_id: u32,
        row: u32,
        byte_offset: u64,
        code: u32,
        dictionary_len: u32,
    },
    ColumnsRawStringLength {
        column_id: u32,
        row: u32,
        byte_offset: u64,
        declared_bytes: u32,
        available_bytes: u64,
    },
    AliveBitmapTruncation {
        row_count: u32,
        byte_offset: u64,
        declared_bytes: u32,
        observed_bytes: u32,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MetadataFeatureExpected {
    ColumnDecodeRefused {
        query_id: u64,
        source: String,
        operation: &'static str,
        fault: &'static str,
        site: &'static str,
        cardinality: u64,
        field_class: &'static str,
        byte_offset: u64,
        error_class: String,
        effect: String,
        provenance: MetadataProvenanceExpected,
        expected_results: u64,
    },
    AliveBitmapTruncationRefused {
        query_id: u64,
        source: String,
        operation: &'static str,
        fault: &'static str,
        site: &'static str,
        cardinality: u64,
        declared_rows: u32,
        byte_offset: u64,
        declared_bytes: u32,
        observed_bytes: u32,
        error_class: String,
        effect: String,
        provenance: MetadataProvenanceExpected,
        expected_results: u64,
    },
    SelectivityBoundaryChosen {
        query_id: u64,
        source: String,
        operation: &'static str,
        fault: &'static str,
        site: &'static str,
        cardinality: u64,
        filter_cardinality: u64,
        threshold: u64,
        branch: independent::ExecutionBranchDto,
        effect: String,
    },
    VisitedBudgetFallback {
        query_id: u64,
        source: String,
        operation: &'static str,
        fault: &'static str,
        site: &'static str,
        cardinality: u64,
        visited: u64,
        budget: u64,
        filter_cardinality: u64,
        exact_rows_examined: u64,
        returned: u64,
        reason: independent::FallbackReasonDto,
        effect: String,
    },
}

/// The one owned invariant DTO pair produced by an operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MetadataInvariantEvidence {
    I36 {
        input: independent::I36Input,
        observed: independent::I36Observed,
    },
    I37 {
        input: independent::I37Input,
        observed: independent::I37Observed,
    },
    I38 {
        input: independent::I38Input,
        observed: independent::I38Observed,
    },
    I39 {
        expected: Vec<independent::I39ExpectedCase>,
        observed: independent::I39Observed,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MetadataFixtureEvidence {
    Columns(independent::I36Input),
    Bitmap(independent::I37Input),
    Planner(independent::I38Input),
    Execution(Vec<independent::I39ExpectedCase>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataQueryEvidence {
    pub query_id: u64,
    pub phase: &'static str,
    pub predicate: independent::PredicateDto,
    pub query_vector_bits: Vec<u32>,
    pub k: u64,
    pub tier: &'static str,
    pub expected_sources: BTreeSet<String>,
}

/// Operation-scoped data returned to shared campaign dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataOperationEvidence {
    pub operation: MetadataOperationKind,
    pub fault: Option<MetadataFaultKind>,
    pub invariant: MetadataInvariantEvidence,
    pub execution_receipts: Vec<MetadataExecutionReceipt>,
    pub feature_receipts: Vec<MetadataFeatureReceipt>,
    pub feature_expected: Vec<MetadataFeatureExpected>,
    pub fixture: MetadataFixtureEvidence,
    pub queries: Vec<MetadataQueryEvidence>,
    pub control: MetadataControlEvidence,
    pub mutation: Option<MetadataMutationEvidence>,
    pub fixture_mutations: Vec<MetadataMutationEvidence>,
    pub retained_product_fixture: Option<MetadataRetainedProductFixture>,
}

/// Stable family-owned envelope for one retained metadata comparison.
pub const METADATA_RETAINED_FIXTURE_SCHEMA: &str = "zeppelin-metadata-retained-fixture-v1";
/// Current retained metadata fixture envelope version.
pub const METADATA_RETAINED_FIXTURE_VERSION: u16 = 1;
const METADATA_RETAINED_FIXTURE_MAGIC: &[u8; 8] = b"ZEMETA01";
const METADATA_RETAINED_FIXTURE_HEADER_LEN: usize = 24;
const METADATA_RETAINED_FIXTURE_MAX_BYTES: usize = 64 * 1024 * 1024;
const METADATA_RETAINED_FIXTURE_DIGEST_DOMAIN: &[u8] = b"metadata/retained-fixture-envelope/v1\0";

/// Literal independent-oracle inputs retained for one metadata operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetainedMetadataOperationV1 {
    pub operation: MetadataOperationKind,
    pub seed: u64,
    pub fault: Option<MetadataFaultKind>,
    pub normalized_schedule_digest: u64,
    pub input_bytes: Vec<u8>,
    pub observed_bytes: Vec<u8>,
    pub product_fixture: Option<MetadataRetainedProductFixture>,
}

/// One exact file retained from a closed public Store fixture.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataRetainedFile {
    pub relative_path: String,
    pub bytes: Vec<u8>,
}

/// Literal closed Stores plus the public outcomes expected from retained queries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataRetainedProductFixture {
    pub legs: Vec<MetadataRetainedProductLeg>,
}

/// One independently materialized clean, fault, or retry Store/query leg.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataRetainedProductLeg {
    pub phase: MetadataRetainedPhase,
    pub files: Vec<MetadataRetainedFile>,
    pub predicate: Option<independent::PredicateDto>,
    pub query_vector_bits: Vec<u32>,
    pub k: u64,
    pub tier: MetadataRetainedSearchTier,
    pub arm: Option<MetadataTestArm>,
    pub expected: MetadataRetainedExpectedOutcome,
}

/// Closed phase identity for retained public Store execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataRetainedPhase {
    Clean,
    Fault,
    Retry,
}

/// Exact public outcome retained for one product leg.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MetadataRetainedExpectedOutcome {
    Results(Vec<MetadataResultFact>),
    Refusal {
        message: String,
        provenance: MetadataDecodeProvenance,
    },
}

/// Closed set of public search tiers retained by metadata replay.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataRetainedSearchTier {
    Exact,
    Graph,
}

/// Result of checking one decoded retained metadata operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetainedMetadataOperationEvidence {
    pub operation: MetadataOperationKind,
    pub seed: u64,
    pub fault: Option<MetadataFaultKind>,
    pub replay: independent::MetadataCanonicalReplay,
    pub public_results: Vec<MetadataResultFact>,
    pub public_legs: Vec<MetadataRetainedProductLegEvidence>,
}

/// One observed retained public Store leg and its production receipts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataRetainedProductLegEvidence {
    pub phase: MetadataRetainedPhase,
    pub outcome: MetadataRetainedExpectedOutcome,
    pub execution_receipts: Vec<MetadataExecutionReceipt>,
    pub feature_receipts: Vec<MetadataFeatureReceipt>,
}

fn retained_metadata_operation_tag(operation: MetadataOperationKind) -> u8 {
    match operation {
        MetadataOperationKind::Columns => 0,
        MetadataOperationKind::Bitmap => 1,
        MetadataOperationKind::Planner => 2,
        MetadataOperationKind::Execution => 3,
    }
}

fn retained_metadata_operation_from_tag(tag: u8) -> Result<MetadataOperationKind, String> {
    match tag {
        0 => Ok(MetadataOperationKind::Columns),
        1 => Ok(MetadataOperationKind::Bitmap),
        2 => Ok(MetadataOperationKind::Planner),
        3 => Ok(MetadataOperationKind::Execution),
        _ => Err(format!(
            "metadata retained fixture has unknown operation tag {tag}"
        )),
    }
}

fn retained_metadata_fault_tag(fault: Option<MetadataFaultKind>) -> u8 {
    match fault {
        None => 0,
        Some(MetadataFaultKind::ColumnCorruption) => 1,
        Some(MetadataFaultKind::BitmapTruncation) => 2,
        Some(MetadataFaultKind::SelectivityBoundary) => 3,
        Some(MetadataFaultKind::VisitedBudgetFallback) => 4,
    }
}

fn retained_metadata_fault_from_tag(tag: u8) -> Result<Option<MetadataFaultKind>, String> {
    match tag {
        0 => Ok(None),
        1 => Ok(Some(MetadataFaultKind::ColumnCorruption)),
        2 => Ok(Some(MetadataFaultKind::BitmapTruncation)),
        3 => Ok(Some(MetadataFaultKind::SelectivityBoundary)),
        4 => Ok(Some(MetadataFaultKind::VisitedBudgetFallback)),
        _ => Err(format!(
            "metadata retained fixture has unknown fault tag {tag}"
        )),
    }
}

fn retained_metadata_checker(operation: MetadataOperationKind) -> &'static str {
    match operation {
        MetadataOperationKind::Columns => independent::I36_CHECKER_ID,
        MetadataOperationKind::Bitmap => independent::I37_CHECKER_ID,
        MetadataOperationKind::Planner => independent::I38_CHECKER_ID,
        MetadataOperationKind::Execution => independent::I39_CHECKER_ID,
    }
}

fn retained_metadata_canonical_bytes(
    evidence: &MetadataOperationEvidence,
) -> Result<(Vec<u8>, Vec<u8>), String> {
    match (&evidence.operation, &evidence.invariant) {
        (MetadataOperationKind::Columns, MetadataInvariantEvidence::I36 { input, observed }) => {
            Ok((
                independent::canonical_i36_input_bytes(input),
                independent::canonical_i36_observed_bytes(observed),
            ))
        }
        (MetadataOperationKind::Bitmap, MetadataInvariantEvidence::I37 { input, observed }) => {
            Ok((
                independent::canonical_i37_input_bytes(input),
                independent::canonical_i37_observed_bytes(observed),
            ))
        }
        (MetadataOperationKind::Planner, MetadataInvariantEvidence::I38 { input, observed }) => {
            Ok((
                independent::canonical_i38_input_bytes(input),
                independent::canonical_i38_observed_bytes(observed),
            ))
        }
        (
            MetadataOperationKind::Execution,
            MetadataInvariantEvidence::I39 { expected, observed },
        ) => Ok((
            independent::canonical_i39_input_bytes(expected),
            independent::canonical_i39_observed_bytes(observed),
        )),
        _ => Err("metadata operation and invariant evidence disagree".to_owned()),
    }
}

fn retained_metadata_payload_digest(payload: &[u8]) -> u64 {
    let mut bytes =
        Vec::with_capacity(METADATA_RETAINED_FIXTURE_DIGEST_DOMAIN.len() + payload.len());
    bytes.extend_from_slice(METADATA_RETAINED_FIXTURE_DIGEST_DOMAIN);
    bytes.extend_from_slice(payload);
    xxh3_64(&bytes)
}

fn append_retained_metadata_bytes(
    output: &mut Vec<u8>,
    bytes: &[u8],
    field: &str,
) -> Result<(), String> {
    let length =
        u32::try_from(bytes.len()).map_err(|_| format!("metadata retained {field} exceeds u32"))?;
    output.extend_from_slice(&length.to_le_bytes());
    output.extend_from_slice(bytes);
    Ok(())
}

fn append_retained_metadata_string(
    output: &mut Vec<u8>,
    value: &str,
    field: &str,
) -> Result<(), String> {
    append_retained_metadata_bytes(output, value.as_bytes(), field)
}

fn append_retained_metadata_result(
    output: &mut Vec<u8>,
    result: &MetadataResultFact,
) -> Result<(), String> {
    append_retained_metadata_string(output, &result.source, "result source")?;
    output.extend_from_slice(&result.row_id.to_le_bytes());
    match result.document_id {
        Some(document_id) => {
            output.push(1);
            output.extend_from_slice(&document_id.to_le_bytes());
        }
        None => output.push(0),
    }
    output.extend_from_slice(&result.score_bits.to_le_bytes());
    Ok(())
}

fn append_retained_metadata_source(output: &mut Vec<u8>, source: RowSource) {
    match source {
        RowSource::Active => output.push(0),
        RowSource::Sealed(segment) => {
            output.push(1);
            output.extend_from_slice(segment.as_bytes());
        }
    }
}

fn append_retained_metadata_arm(
    output: &mut Vec<u8>,
    arm: Option<&MetadataTestArm>,
) -> Result<(), String> {
    match arm {
        None => output.push(0),
        Some(MetadataTestArm::ObserveExecution { query_id }) => {
            output.push(1);
            output.extend_from_slice(&query_id.to_le_bytes());
        }
        Some(MetadataTestArm::ColumnCorruption {
            query_id,
            source,
            field_class,
            byte_offset,
        }) => {
            if !matches!(
                *field_class,
                "presence-tail" | "dictionary-code" | "raw-string-length"
            ) {
                return Err(format!(
                    "metadata retained Columns arm has unknown field class {field_class}"
                ));
            }
            output.push(2);
            output.extend_from_slice(&query_id.to_le_bytes());
            append_retained_metadata_source(output, *source);
            append_retained_metadata_string(output, field_class, "Columns field class")?;
            output.extend_from_slice(&byte_offset.to_le_bytes());
        }
        Some(MetadataTestArm::AliveBitmapTruncation {
            query_id,
            source,
            declared_rows,
            declared_bytes,
            observed_bytes,
        }) => {
            output.push(3);
            output.extend_from_slice(&query_id.to_le_bytes());
            append_retained_metadata_source(output, *source);
            output.extend_from_slice(&declared_rows.to_le_bytes());
            output.extend_from_slice(&declared_bytes.to_le_bytes());
            output.extend_from_slice(&observed_bytes.to_le_bytes());
        }
        Some(MetadataTestArm::VisitedBudgetFallback { query_id, budget }) => {
            output.push(4);
            output.extend_from_slice(&query_id.to_le_bytes());
            output.extend_from_slice(
                &u64::try_from(*budget)
                    .map_err(|_| "metadata retained visited budget exceeds u64".to_owned())?
                    .to_le_bytes(),
            );
        }
        Some(MetadataTestArm::SelectivityBoundary {
            query_id,
            expected_cardinality,
        }) => {
            output.push(5);
            output.extend_from_slice(&query_id.to_le_bytes());
            output.extend_from_slice(&expected_cardinality.to_le_bytes());
        }
        Some(other) => {
            return Err(format!(
                "metadata retained product leg cannot encode arm {other:?}"
            ));
        }
    }
    Ok(())
}

fn append_retained_metadata_provenance(
    output: &mut Vec<u8>,
    provenance: &MetadataDecodeProvenance,
) {
    match provenance {
        MetadataDecodeProvenance::ColumnsPresenceTail {
            column_id,
            row_count,
            byte_offset,
            observed_byte,
            allowed_mask,
        } => {
            output.push(0);
            output.extend_from_slice(&column_id.to_le_bytes());
            output.extend_from_slice(&row_count.to_le_bytes());
            output.extend_from_slice(&byte_offset.to_le_bytes());
            output.push(*observed_byte);
            output.push(*allowed_mask);
        }
        MetadataDecodeProvenance::ColumnsDictionaryCode {
            column_id,
            row,
            byte_offset,
            code,
            dictionary_cardinality,
        } => {
            output.push(1);
            output.extend_from_slice(&column_id.to_le_bytes());
            output.extend_from_slice(&row.to_le_bytes());
            output.extend_from_slice(&byte_offset.to_le_bytes());
            output.extend_from_slice(&code.to_le_bytes());
            output.extend_from_slice(&dictionary_cardinality.to_le_bytes());
        }
        MetadataDecodeProvenance::ColumnsRawStringLength {
            column_id,
            row,
            byte_offset,
            declared_bytes,
            available_bytes,
        } => {
            output.push(2);
            output.extend_from_slice(&column_id.to_le_bytes());
            output.extend_from_slice(&row.to_le_bytes());
            output.extend_from_slice(&byte_offset.to_le_bytes());
            output.extend_from_slice(&declared_bytes.to_le_bytes());
            output.extend_from_slice(&available_bytes.to_le_bytes());
        }
        MetadataDecodeProvenance::AliveBitmapTruncation {
            row_count,
            byte_offset,
            declared_bytes,
            observed_bytes,
        } => {
            output.push(3);
            output.extend_from_slice(&row_count.to_le_bytes());
            output.extend_from_slice(&byte_offset.to_le_bytes());
            output.extend_from_slice(&declared_bytes.to_le_bytes());
            output.extend_from_slice(&observed_bytes.to_le_bytes());
        }
    }
}

fn append_retained_metadata_product_fixture(
    output: &mut Vec<u8>,
    fixture: Option<&MetadataRetainedProductFixture>,
) -> Result<(), String> {
    let Some(fixture) = fixture else {
        output.push(0);
        return Ok(());
    };
    output.push(1);
    if fixture.legs.is_empty() {
        return Err("metadata retained product fixture has no legs".to_owned());
    }
    let leg_count = u32::try_from(fixture.legs.len())
        .map_err(|_| "metadata retained product leg count exceeds u32".to_owned())?;
    output.extend_from_slice(&leg_count.to_le_bytes());
    for leg in &fixture.legs {
        output.push(match leg.phase {
            MetadataRetainedPhase::Clean => 0,
            MetadataRetainedPhase::Fault => 1,
            MetadataRetainedPhase::Retry => 2,
        });
        let file_count = u32::try_from(leg.files.len())
            .map_err(|_| "metadata retained file count exceeds u32".to_owned())?;
        output.extend_from_slice(&file_count.to_le_bytes());
        let mut previous = None::<&str>;
        for file in &leg.files {
            if previous.is_some_and(|prior| prior >= file.relative_path.as_str()) {
                return Err("metadata retained file paths are not sorted and unique".to_owned());
            }
            validate_retained_metadata_path(&file.relative_path)?;
            append_retained_metadata_string(output, &file.relative_path, "file path")?;
            append_retained_metadata_bytes(output, &file.bytes, "file bytes")?;
            previous = Some(&file.relative_path);
        }
        if leg.query_vector_bits.is_empty() {
            return Err("metadata retained query vector is empty".to_owned());
        }
        let dimension_count = u32::try_from(leg.query_vector_bits.len())
            .map_err(|_| "metadata retained query dimensions exceed u32".to_owned())?;
        output.extend_from_slice(&dimension_count.to_le_bytes());
        for bits in &leg.query_vector_bits {
            output.extend_from_slice(&bits.to_le_bytes());
        }
        if leg.k == 0 {
            return Err("metadata retained query k is zero".to_owned());
        }
        output.extend_from_slice(&leg.k.to_le_bytes());
        output.push(match leg.tier {
            MetadataRetainedSearchTier::Exact => 0,
            MetadataRetainedSearchTier::Graph => 1,
        });
        match &leg.predicate {
            Some(predicate) => {
                output.push(1);
                schedule_push_predicate(output, predicate)?;
            }
            None => output.push(0),
        }
        append_retained_metadata_arm(output, leg.arm.as_ref())?;
        match &leg.expected {
            MetadataRetainedExpectedOutcome::Results(results) => {
                output.push(0);
                let result_count = u32::try_from(results.len())
                    .map_err(|_| "metadata retained result count exceeds u32".to_owned())?;
                output.extend_from_slice(&result_count.to_le_bytes());
                for result in results {
                    append_retained_metadata_result(output, result)?;
                }
            }
            MetadataRetainedExpectedOutcome::Refusal {
                message,
                provenance,
            } => {
                output.push(1);
                append_retained_metadata_string(output, message, "refusal message")?;
                append_retained_metadata_provenance(output, provenance);
            }
        }
    }
    Ok(())
}

fn validate_retained_metadata_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("metadata retained file path is empty".to_owned());
    }
    let path = Path::new(path);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err("metadata retained file path is not a safe relative path".to_owned());
    }
    Ok(())
}

/// Encodes one completed operation without retaining a seed-derived generator.
pub fn encode_metadata_fixture(evidence: &MetadataOperationEvidence) -> Result<Vec<u8>, String> {
    if evidence
        .fault
        .is_some_and(|fault| fault.operation() != evidence.operation)
    {
        return Err("metadata retained fault belongs to another operation".to_owned());
    }
    if evidence.control.normalized_schedule_digest == 0 {
        return Err("metadata retained schedule digest is zero".to_owned());
    }
    let (input_bytes, observed_bytes) = retained_metadata_canonical_bytes(evidence)?;
    let mut payload = Vec::new();
    payload.push(retained_metadata_operation_tag(evidence.operation));
    payload.push(retained_metadata_fault_tag(evidence.fault));
    payload.extend_from_slice(&0_u16.to_le_bytes());
    payload.extend_from_slice(&evidence.control.seed.to_le_bytes());
    payload.extend_from_slice(&evidence.control.normalized_schedule_digest.to_le_bytes());
    append_retained_metadata_bytes(&mut payload, &input_bytes, "input")?;
    append_retained_metadata_bytes(&mut payload, &observed_bytes, "observation")?;
    append_retained_metadata_product_fixture(
        &mut payload,
        evidence.retained_product_fixture.as_ref(),
    )?;
    if payload.len() > METADATA_RETAINED_FIXTURE_MAX_BYTES {
        return Err("metadata retained fixture exceeds the byte limit".to_owned());
    }

    let payload_length = u32::try_from(payload.len())
        .map_err(|_| "metadata retained payload exceeds u32".to_owned())?;
    let mut output = Vec::with_capacity(METADATA_RETAINED_FIXTURE_HEADER_LEN + payload.len());
    output.extend_from_slice(METADATA_RETAINED_FIXTURE_MAGIC);
    output.extend_from_slice(&METADATA_RETAINED_FIXTURE_VERSION.to_le_bytes());
    output.extend_from_slice(&0_u16.to_le_bytes());
    output.extend_from_slice(&payload_length.to_le_bytes());
    output.extend_from_slice(&retained_metadata_payload_digest(&payload).to_le_bytes());
    output.extend_from_slice(&payload);
    Ok(output)
}

struct RetainedMetadataReader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> RetainedMetadataReader<'a> {
    fn take(&mut self, length: usize, field: &str) -> Result<&'a [u8], String> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| format!("metadata retained {field} offset overflow"))?;
        let value = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| format!("metadata retained {field} is truncated"))?;
        self.position = end;
        Ok(value)
    }

    fn u8(&mut self, field: &str) -> Result<u8, String> {
        self.take(1, field)?
            .first()
            .copied()
            .ok_or_else(|| format!("metadata retained {field} is truncated"))
    }

    fn u16(&mut self, field: &str) -> Result<u16, String> {
        let bytes: [u8; 2] = self
            .take(2, field)?
            .try_into()
            .map_err(|_| format!("metadata retained {field} width changed"))?;
        Ok(u16::from_le_bytes(bytes))
    }

    fn u32(&mut self, field: &str) -> Result<u32, String> {
        let bytes: [u8; 4] = self
            .take(4, field)?
            .try_into()
            .map_err(|_| format!("metadata retained {field} width changed"))?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn u64(&mut self, field: &str) -> Result<u64, String> {
        let bytes: [u8; 8] = self
            .take(8, field)?
            .try_into()
            .map_err(|_| format!("metadata retained {field} width changed"))?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn u128(&mut self, field: &str) -> Result<u128, String> {
        let bytes: [u8; 16] = self
            .take(16, field)?
            .try_into()
            .map_err(|_| format!("metadata retained {field} width changed"))?;
        Ok(u128::from_le_bytes(bytes))
    }

    fn bytes(&mut self, field: &str) -> Result<Vec<u8>, String> {
        let length = usize::try_from(self.u32(field)?)
            .map_err(|_| format!("metadata retained {field} length exceeds usize"))?;
        if length > METADATA_RETAINED_FIXTURE_MAX_BYTES {
            return Err(format!("metadata retained {field} exceeds the byte limit"));
        }
        Ok(self.take(length, field)?.to_vec())
    }

    fn string(&mut self, field: &str) -> Result<String, String> {
        String::from_utf8(self.bytes(field)?)
            .map_err(|_| format!("metadata retained {field} is not UTF-8"))
    }

    fn finish(self) -> Result<(), String> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err("metadata retained fixture has trailing payload bytes".to_owned())
        }
    }
}

fn retained_metadata_count(
    reader: &mut RetainedMetadataReader<'_>,
    field: &str,
) -> Result<usize, String> {
    let count = usize::try_from(reader.u32(field)?)
        .map_err(|_| format!("metadata retained {field} exceeds usize"))?;
    if count > 1_000_000 {
        return Err(format!("metadata retained {field} exceeds the item limit"));
    }
    Ok(count)
}

fn decode_retained_metadata_result(
    reader: &mut RetainedMetadataReader<'_>,
) -> Result<MetadataResultFact, String> {
    let source = reader.string("result source")?;
    let row_id = reader.u32("result row")?;
    let document_id = match reader.u8("result document presence")? {
        0 => None,
        1 => Some(reader.u128("result document")?),
        tag => {
            return Err(format!(
                "metadata retained result document presence tag {tag} is unknown"
            ));
        }
    };
    let score_bits = reader.u32("result score bits")?;
    Ok(MetadataResultFact {
        source,
        row_id,
        document_id,
        score_bits,
    })
}

fn decode_retained_metadata_source(
    reader: &mut RetainedMetadataReader<'_>,
) -> Result<RowSource, String> {
    match reader.u8("row source")? {
        0 => Ok(RowSource::Active),
        1 => {
            let bytes: [u8; 16] = reader
                .take(16, "sealed source")?
                .try_into()
                .map_err(|_| "metadata retained sealed source width changed".to_owned())?;
            Ok(RowSource::Sealed(SegmentId::from_bytes(bytes)))
        }
        tag => Err(format!("metadata retained row source tag {tag} is unknown")),
    }
}

fn decode_retained_metadata_arm(
    reader: &mut RetainedMetadataReader<'_>,
) -> Result<Option<MetadataTestArm>, String> {
    match reader.u8("controller arm")? {
        0 => Ok(None),
        1 => Ok(Some(MetadataTestArm::ObserveExecution {
            query_id: reader.u64("query id")?,
        })),
        2 => {
            let query_id = reader.u64("query id")?;
            let source = decode_retained_metadata_source(reader)?;
            let field_class = match reader.string("Columns field class")?.as_str() {
                "presence-tail" => "presence-tail",
                "dictionary-code" => "dictionary-code",
                "raw-string-length" => "raw-string-length",
                other => {
                    return Err(format!(
                        "metadata retained Columns arm has unknown field class {other}"
                    ));
                }
            };
            let byte_offset = reader.u64("Columns byte offset")?;
            Ok(Some(MetadataTestArm::ColumnCorruption {
                query_id,
                source,
                field_class,
                byte_offset,
            }))
        }
        3 => Ok(Some(MetadataTestArm::AliveBitmapTruncation {
            query_id: reader.u64("query id")?,
            source: decode_retained_metadata_source(reader)?,
            declared_rows: reader.u32("Alive declared rows")?,
            declared_bytes: reader.u32("Alive declared bytes")?,
            observed_bytes: reader.u32("Alive observed bytes")?,
        })),
        4 => Ok(Some(MetadataTestArm::VisitedBudgetFallback {
            query_id: reader.u64("query id")?,
            budget: usize::try_from(reader.u64("visited budget")?)
                .map_err(|_| "metadata retained visited budget exceeds usize".to_owned())?,
        })),
        5 => Ok(Some(MetadataTestArm::SelectivityBoundary {
            query_id: reader.u64("query id")?,
            expected_cardinality: reader.u64("selectivity cardinality")?,
        })),
        tag => Err(format!(
            "metadata retained controller arm tag {tag} is unknown"
        )),
    }
}

fn decode_retained_metadata_provenance(
    reader: &mut RetainedMetadataReader<'_>,
) -> Result<MetadataDecodeProvenance, String> {
    match reader.u8("refusal provenance")? {
        0 => Ok(MetadataDecodeProvenance::ColumnsPresenceTail {
            column_id: reader.u32("presence column")?,
            row_count: reader.u32("presence rows")?,
            byte_offset: reader.u64("presence offset")?,
            observed_byte: reader.u8("presence observed byte")?,
            allowed_mask: reader.u8("presence allowed mask")?,
        }),
        1 => Ok(MetadataDecodeProvenance::ColumnsDictionaryCode {
            column_id: reader.u32("dictionary column")?,
            row: reader.u32("dictionary row")?,
            byte_offset: reader.u64("dictionary offset")?,
            code: reader.u32("dictionary code")?,
            dictionary_cardinality: reader.u32("dictionary cardinality")?,
        }),
        2 => Ok(MetadataDecodeProvenance::ColumnsRawStringLength {
            column_id: reader.u32("raw string column")?,
            row: reader.u32("raw string row")?,
            byte_offset: reader.u64("raw string offset")?,
            declared_bytes: reader.u32("raw string declared bytes")?,
            available_bytes: reader.u64("raw string available bytes")?,
        }),
        3 => Ok(MetadataDecodeProvenance::AliveBitmapTruncation {
            row_count: reader.u32("Alive rows")?,
            byte_offset: reader.u64("Alive offset")?,
            declared_bytes: reader.u32("Alive declared bytes")?,
            observed_bytes: reader.u32("Alive observed bytes")?,
        }),
        tag => Err(format!(
            "metadata retained refusal provenance tag {tag} is unknown"
        )),
    }
}

fn decode_retained_schedule_bytes(
    reader: &mut RetainedMetadataReader<'_>,
    field: &str,
) -> Result<Vec<u8>, String> {
    let length = usize::try_from(reader.u64(field)?)
        .map_err(|_| format!("metadata retained {field} length exceeds usize"))?;
    if length > METADATA_RETAINED_FIXTURE_MAX_BYTES {
        return Err(format!("metadata retained {field} exceeds the byte limit"));
    }
    Ok(reader.take(length, field)?.to_vec())
}

fn decode_retained_scalar(
    reader: &mut RetainedMetadataReader<'_>,
) -> Result<independent::ScalarCell, String> {
    match reader.u8("predicate scalar")? {
        0 => Ok(independent::ScalarCell::Null),
        1 => Ok(independent::ScalarCell::U64(reader.u64("u64 scalar")?)),
        2 => Ok(independent::ScalarCell::I64(i64::from_le_bytes(
            reader
                .take(8, "i64 scalar")?
                .try_into()
                .map_err(|_| "metadata retained i64 scalar width changed".to_owned())?,
        ))),
        3 => Ok(independent::ScalarCell::F64Bits(
            reader.u64("f64 scalar bits")?,
        )),
        4 => match reader.u8("bool scalar")? {
            0 => Ok(independent::ScalarCell::Bool(false)),
            1 => Ok(independent::ScalarCell::Bool(true)),
            tag => Err(format!(
                "metadata retained bool scalar tag {tag} is unknown"
            )),
        },
        5 => Ok(independent::ScalarCell::Utf8(
            decode_retained_schedule_bytes(reader, "UTF-8 scalar")?,
        )),
        tag => Err(format!(
            "metadata retained predicate scalar tag {tag} is unknown"
        )),
    }
}

fn decode_retained_bound(
    reader: &mut RetainedMetadataReader<'_>,
) -> Result<Option<independent::RangeBoundDto>, String> {
    match reader.u8("range bound presence")? {
        0 => Ok(None),
        1 => {
            let inclusive = match reader.u8("range bound inclusive")? {
                0 => false,
                1 => true,
                tag => {
                    return Err(format!(
                        "metadata retained range inclusive tag {tag} is unknown"
                    ));
                }
            };
            Ok(Some(independent::RangeBoundDto {
                value: decode_retained_scalar(reader)?,
                inclusive,
            }))
        }
        tag => Err(format!(
            "metadata retained range bound presence tag {tag} is unknown"
        )),
    }
}

fn decode_retained_predicate(
    reader: &mut RetainedMetadataReader<'_>,
    depth: usize,
) -> Result<independent::PredicateDto, String> {
    if depth > 64 {
        return Err("metadata retained predicate nesting exceeds 64".to_owned());
    }
    let decode_children = |reader: &mut RetainedMetadataReader<'_>,
                           depth: usize,
                           field: &str|
     -> Result<Vec<independent::PredicateDto>, String> {
        let count = usize::try_from(reader.u64(field)?)
            .map_err(|_| format!("metadata retained {field} exceeds usize"))?;
        if count > 1_000_000 {
            return Err(format!("metadata retained {field} exceeds the item limit"));
        }
        (0..count)
            .map(|_| decode_retained_predicate(reader, depth.saturating_add(1)))
            .collect()
    };
    match reader.u8("predicate")? {
        0 => Ok(independent::PredicateDto::Eq {
            column: reader.u32("Eq column")?,
            value: decode_retained_scalar(reader)?,
        }),
        1 => {
            let column = reader.u32("In column")?;
            let count = usize::try_from(reader.u64("In value count")?)
                .map_err(|_| "metadata retained In count exceeds usize".to_owned())?;
            if count > 1_000_000 {
                return Err("metadata retained In count exceeds the item limit".to_owned());
            }
            Ok(independent::PredicateDto::In {
                column,
                values: (0..count)
                    .map(|_| decode_retained_scalar(reader))
                    .collect::<Result<Vec<_>, _>>()?,
            })
        }
        2 => Ok(independent::PredicateDto::Range {
            column: reader.u32("Range column")?,
            lower: decode_retained_bound(reader)?,
            upper: decode_retained_bound(reader)?,
        }),
        3 => Ok(independent::PredicateDto::Exists(
            reader.u32("Exists column")?,
        )),
        4 => Ok(independent::PredicateDto::IsNull(
            reader.u32("IsNull column")?,
        )),
        5 => Ok(independent::PredicateDto::And(decode_children(
            reader,
            depth,
            "And child count",
        )?)),
        6 => Ok(independent::PredicateDto::Or(decode_children(
            reader,
            depth,
            "Or child count",
        )?)),
        7 => Ok(independent::PredicateDto::Not(Box::new(
            decode_retained_predicate(reader, depth.saturating_add(1))?,
        ))),
        tag => Err(format!("metadata retained predicate tag {tag} is unknown")),
    }
}

fn decode_retained_metadata_product_fixture(
    reader: &mut RetainedMetadataReader<'_>,
) -> Result<Option<MetadataRetainedProductFixture>, String> {
    match reader.u8("product fixture presence")? {
        0 => Ok(None),
        1 => {
            let leg_count = retained_metadata_count(reader, "product leg count")?;
            if leg_count == 0 {
                return Err("metadata retained product fixture has no legs".to_owned());
            }
            let mut legs = Vec::with_capacity(leg_count);
            for _ in 0..leg_count {
                let phase = match reader.u8("product phase")? {
                    0 => MetadataRetainedPhase::Clean,
                    1 => MetadataRetainedPhase::Fault,
                    2 => MetadataRetainedPhase::Retry,
                    tag => {
                        return Err(format!(
                            "metadata retained product phase tag {tag} is unknown"
                        ));
                    }
                };
                let file_count = retained_metadata_count(reader, "file count")?;
                if file_count == 0 {
                    return Err("metadata retained product leg has no files".to_owned());
                }
                let mut files = Vec::with_capacity(file_count);
                for _ in 0..file_count {
                    let relative_path = reader.string("file path")?;
                    validate_retained_metadata_path(&relative_path)?;
                    let bytes = reader.bytes("file bytes")?;
                    files.push(MetadataRetainedFile {
                        relative_path,
                        bytes,
                    });
                }
                if files
                    .windows(2)
                    .any(|pair| pair[0].relative_path.as_str() >= pair[1].relative_path.as_str())
                {
                    return Err("metadata retained file paths are not sorted and unique".to_owned());
                }
                let dimension_count = retained_metadata_count(reader, "query dimensions")?;
                if dimension_count == 0 {
                    return Err("metadata retained query vector is empty".to_owned());
                }
                let query_vector_bits = (0..dimension_count)
                    .map(|_| reader.u32("query vector bits"))
                    .collect::<Result<Vec<_>, _>>()?;
                let k = reader.u64("query k")?;
                if k == 0 {
                    return Err("metadata retained query k is zero".to_owned());
                }
                let tier = match reader.u8("query tier")? {
                    0 => MetadataRetainedSearchTier::Exact,
                    1 => MetadataRetainedSearchTier::Graph,
                    tag => {
                        return Err(format!("metadata retained query tier tag {tag} is unknown"));
                    }
                };
                let predicate = match reader.u8("predicate presence")? {
                    0 => None,
                    1 => Some(decode_retained_predicate(reader, 0)?),
                    tag => {
                        return Err(format!(
                            "metadata retained predicate presence tag {tag} is unknown"
                        ));
                    }
                };
                let arm = decode_retained_metadata_arm(reader)?;
                let expected = match reader.u8("product outcome")? {
                    0 => {
                        let result_count = retained_metadata_count(reader, "result count")?;
                        MetadataRetainedExpectedOutcome::Results(
                            (0..result_count)
                                .map(|_| decode_retained_metadata_result(reader))
                                .collect::<Result<Vec<_>, _>>()?,
                        )
                    }
                    1 => MetadataRetainedExpectedOutcome::Refusal {
                        message: reader.string("refusal message")?,
                        provenance: decode_retained_metadata_provenance(reader)?,
                    },
                    tag => {
                        return Err(format!(
                            "metadata retained product outcome tag {tag} is unknown"
                        ));
                    }
                };
                legs.push(MetadataRetainedProductLeg {
                    phase,
                    files,
                    predicate,
                    query_vector_bits,
                    k,
                    tier,
                    arm,
                    expected,
                });
            }
            Ok(Some(MetadataRetainedProductFixture { legs }))
        }
        tag => Err(format!(
            "metadata retained product fixture presence tag {tag} is unknown"
        )),
    }
}

/// Decodes retained metadata bytes without consulting a seed or product code.
pub fn decode_metadata_fixture(bytes: &[u8]) -> Result<RetainedMetadataOperationV1, String> {
    if bytes.len() < METADATA_RETAINED_FIXTURE_HEADER_LEN {
        return Err("metadata retained fixture is shorter than its header".to_owned());
    }
    let (header, payload) = bytes.split_at(METADATA_RETAINED_FIXTURE_HEADER_LEN);
    if header.get(..8) != Some(METADATA_RETAINED_FIXTURE_MAGIC.as_slice()) {
        return Err("metadata retained fixture magic differs".to_owned());
    }
    let mut header_reader = RetainedMetadataReader {
        bytes: header,
        position: 8,
    };
    let version = header_reader.u16("version")?;
    if version != METADATA_RETAINED_FIXTURE_VERSION {
        return Err(format!(
            "metadata retained fixture version differs: {version}"
        ));
    }
    if header_reader.u16("flags")? != 0 {
        return Err("metadata retained fixture flags are nonzero".to_owned());
    }
    let declared_length = usize::try_from(header_reader.u32("payload length")?)
        .map_err(|_| "metadata retained payload length exceeds usize".to_owned())?;
    let declared_digest = header_reader.u64("payload digest")?;
    header_reader.finish()?;
    if declared_length != payload.len() || payload.len() > METADATA_RETAINED_FIXTURE_MAX_BYTES {
        return Err("metadata retained fixture payload length differs".to_owned());
    }
    if declared_digest != retained_metadata_payload_digest(payload) {
        return Err("metadata retained fixture payload digest differs".to_owned());
    }

    let mut reader = RetainedMetadataReader {
        bytes: payload,
        position: 0,
    };
    let operation = retained_metadata_operation_from_tag(reader.u8("operation")?)?;
    let fault = retained_metadata_fault_from_tag(reader.u8("fault")?)?;
    if fault.is_some_and(|selected| selected.operation() != operation) {
        return Err("metadata retained fault belongs to another operation".to_owned());
    }
    if reader.u16("reserved")? != 0 {
        return Err("metadata retained fixture reserved bits are nonzero".to_owned());
    }
    let seed = reader.u64("seed")?;
    let normalized_schedule_digest = reader.u64("schedule digest")?;
    if normalized_schedule_digest == 0 {
        return Err("metadata retained schedule digest is zero".to_owned());
    }
    let input_bytes = reader.bytes("input")?;
    let observed_bytes = reader.bytes("observation")?;
    let product_fixture = decode_retained_metadata_product_fixture(&mut reader)?;
    reader.finish()?;
    if product_fixture.is_none() {
        return Err(format!(
            "metadata retained {operation:?} fixture omits its public Store image"
        ));
    }
    Ok(RetainedMetadataOperationV1 {
        operation,
        seed,
        fault,
        normalized_schedule_digest,
        input_bytes,
        observed_bytes,
        product_fixture,
    })
}

fn materialize_retained_metadata_product_fixture(
    leg: &MetadataRetainedProductLeg,
) -> Result<TempDir, String> {
    let directory = tempdir().map_err(|error| format!("metadata replay tempdir: {error}"))?;
    for file in &leg.files {
        validate_retained_metadata_path(&file.relative_path)?;
        let path = directory.path().join(&file.relative_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create metadata replay directory: {error}"))?;
        }
        std::fs::write(&path, &file.bytes)
            .map_err(|error| format!("write retained metadata file {}: {error}", path.display()))?;
    }
    Ok(directory)
}

fn retained_predicate_value(value: &independent::ScalarCell) -> Result<PredicateValue, String> {
    match value {
        independent::ScalarCell::Null => {
            Err("metadata retained predicate contains a null literal".to_owned())
        }
        independent::ScalarCell::U64(value) => Ok(PredicateValue::U64(*value)),
        independent::ScalarCell::I64(value) => Ok(PredicateValue::I64(*value)),
        independent::ScalarCell::F64Bits(bits) => Ok(PredicateValue::F64(f64::from_bits(*bits))),
        independent::ScalarCell::Bool(value) => Ok(PredicateValue::Bool(*value)),
        independent::ScalarCell::Utf8(bytes) => String::from_utf8(bytes.clone())
            .map(PredicateValue::String)
            .map_err(|_| "metadata retained predicate string is not UTF-8".to_owned()),
    }
}

fn retained_range_bound(
    bound: &Option<independent::RangeBoundDto>,
) -> Result<Option<RangeBound>, String> {
    bound
        .as_ref()
        .map(|bound| {
            let value = retained_predicate_value(&bound.value)?;
            Ok(if bound.inclusive {
                RangeBound::inclusive(value)
            } else {
                RangeBound::exclusive(value)
            })
        })
        .transpose()
}

fn retained_predicate(predicate: &independent::PredicateDto) -> Result<Predicate, String> {
    match predicate {
        independent::PredicateDto::Eq { column, value } => Ok(Predicate::Eq {
            column: ColumnId::new(*column),
            value: retained_predicate_value(value)?,
        }),
        independent::PredicateDto::In { column, values } => Ok(Predicate::In {
            column: ColumnId::new(*column),
            values: values
                .iter()
                .map(retained_predicate_value)
                .collect::<Result<Vec<_>, _>>()?,
        }),
        independent::PredicateDto::Range {
            column,
            lower,
            upper,
        } => Ok(Predicate::Range(RangePredicate {
            column: ColumnId::new(*column),
            lower: retained_range_bound(lower)?,
            upper: retained_range_bound(upper)?,
        })),
        independent::PredicateDto::Exists(column) => Ok(Predicate::Exists(ColumnId::new(*column))),
        independent::PredicateDto::IsNull(column) => Ok(Predicate::IsNull(ColumnId::new(*column))),
        independent::PredicateDto::And(children) => Ok(Predicate::And(
            children
                .iter()
                .map(retained_predicate)
                .collect::<Result<Vec<_>, _>>()?,
        )),
        independent::PredicateDto::Or(children) => Ok(Predicate::Or(
            children
                .iter()
                .map(retained_predicate)
                .collect::<Result<Vec<_>, _>>()?,
        )),
        independent::PredicateDto::Not(child) => {
            Ok(Predicate::Not(Box::new(retained_predicate(child)?)))
        }
    }
}

fn replay_retained_metadata_product(
    retained: &RetainedMetadataOperationV1,
) -> Result<Vec<MetadataRetainedProductLegEvidence>, String> {
    match (retained.operation, retained.product_fixture.as_ref()) {
        (
            operation @ (MetadataOperationKind::Columns
            | MetadataOperationKind::Bitmap
            | MetadataOperationKind::Planner
            | MetadataOperationKind::Execution),
            Some(fixture),
        ) => {
            let predicate = match operation {
                MetadataOperationKind::Columns => Predicate::And(Vec::new()),
                MetadataOperationKind::Bitmap => retained_predicate(
                    &independent::decode_canonical_i37_input(&retained.input_bytes)?.predicate,
                )?,
                MetadataOperationKind::Planner => retained_predicate(
                    &independent::decode_canonical_i38_input(&retained.input_bytes)?.predicate,
                )?,
                MetadataOperationKind::Execution => Predicate::And(Vec::new()),
            };
            fixture
                .legs
                .iter()
                .map(|leg| {
                    let leg_predicate = leg
                        .predicate
                        .as_ref()
                        .map(retained_predicate)
                        .transpose()?
                        .unwrap_or_else(|| predicate.clone());
                    let directory = materialize_retained_metadata_product_fixture(leg)?;
                    let options = match leg.tier {
                        MetadataRetainedSearchTier::Graph => {
                            OpenOptions::default().with_epoch(graph_epoch())
                        }
                        MetadataRetainedSearchTier::Exact => OpenOptions::default(),
                    };
                    let controller = Arc::new(MetadataTestController::new());
                    if let Some(arm) = &leg.arm {
                        controller.arm(arm.clone()).map_err(|error| {
                            format!("arm retained metadata {operation:?} {:?}: {error}", leg.phase)
                        })?;
                    }
                    let store = open_with_controller(directory.path(), options, &controller)?;
                    let query = leg
                        .query_vector_bits
                        .iter()
                        .map(|bits| f32::from_bits(*bits))
                        .collect::<Vec<_>>();
                    let k = usize::try_from(leg.k)
                        .map_err(|_| "metadata retained query k exceeds usize".to_owned())?;
                    let search_options = match leg.tier {
                        MetadataRetainedSearchTier::Exact => {
                            SearchOptions::default().with_tier(SearchTier::Exact)
                        }
                        MetadataRetainedSearchTier::Graph => graph_options(),
                    };
                    let result = store.search_filtered(
                        SearchRequest::new(&query),
                        &leg_predicate,
                        k,
                        search_options,
                        QueryControl::Cancel(CancelToken::new()),
                    );
                    let outcome = match (&leg.expected, result) {
                        (MetadataRetainedExpectedOutcome::Results(expected), Ok(observed)) => {
                            let observed = result_facts(&observed);
                            if &observed != expected {
                                return Err(format!(
                                    "metadata retained {operation:?} {:?} results differ expected={expected:?} observed={observed:?}",
                                    leg.phase
                                ));
                            }
                            MetadataRetainedExpectedOutcome::Results(observed)
                        }
                        (
                            MetadataRetainedExpectedOutcome::Refusal {
                                message,
                                provenance,
                            },
                            Err(error),
                        ) => {
                            require_metadata_provenance(&error, provenance)?;
                            if error.to_string() != *message {
                                return Err(format!(
                                    "metadata retained {operation:?} {:?} refusal differs expected={message:?} observed={:?}",
                                    leg.phase,
                                    error.to_string()
                                ));
                            }
                            MetadataRetainedExpectedOutcome::Refusal {
                                message: error.to_string(),
                                provenance: provenance.clone(),
                            }
                        }
                        (MetadataRetainedExpectedOutcome::Results(_), Err(error)) => {
                            return Err(format!(
                                "metadata retained {operation:?} {:?} unexpectedly refused: {error}",
                                leg.phase
                            ));
                        }
                        (MetadataRetainedExpectedOutcome::Refusal { .. }, Ok(observed)) => {
                            return Err(format!(
                                "metadata retained {operation:?} {:?} unexpectedly returned {:?}",
                                leg.phase,
                                result_facts(&observed)
                            ));
                        }
                    };
                    store.close().map_err(|error| {
                        format!(
                            "close retained metadata {operation:?} {:?} Store: {error}",
                            leg.phase
                        )
                    })?;
                    let execution_receipts = controller.drain_execution_receipts().map_err(|error| {
                        format!(
                            "drain retained metadata {operation:?} {:?} execution receipts: {error}",
                            leg.phase
                        )
                    })?;
                    let feature_receipts = controller.drain_feature_receipts().map_err(|error| {
                        format!(
                            "drain retained metadata {operation:?} {:?} feature receipts: {error}",
                            leg.phase
                        )
                    })?;
                    controller.assert_no_unconsumed_arm().map_err(|error| {
                        format!(
                            "retained metadata {operation:?} {:?} arm leak: {error}",
                            leg.phase
                        )
                    })?;
                    Ok(MetadataRetainedProductLegEvidence {
                        phase: leg.phase,
                        outcome,
                        execution_receipts,
                        feature_receipts,
                    })
                })
                .collect()
        }
        (operation, None) => Err(format!(
            "metadata retained {operation:?} fixture omits its public Store image"
        )),
    }
}

/// Executes the retained independent comparison without seed regeneration.
pub fn run_metadata_operation_from_fixture(
    bytes: &[u8],
) -> Result<RetainedMetadataOperationEvidence, String> {
    let retained = decode_metadata_fixture(bytes)?;
    let replay = independent::replay_canonical_comparison(
        retained_metadata_checker(retained.operation),
        &retained.input_bytes,
        &retained.observed_bytes,
    )?;
    let public_legs = replay_retained_metadata_product(&retained)?;
    let selected_phase = if retained.fault.is_some() {
        MetadataRetainedPhase::Fault
    } else {
        MetadataRetainedPhase::Clean
    };
    let public_results = public_legs
        .iter()
        .find(|leg| leg.phase == selected_phase)
        .map(|leg| match &leg.outcome {
            MetadataRetainedExpectedOutcome::Results(results) => results.clone(),
            MetadataRetainedExpectedOutcome::Refusal { .. } => Vec::new(),
        })
        .ok_or_else(|| format!("metadata retained fixture omits its {selected_phase:?} leg"))?;
    Ok(RetainedMetadataOperationEvidence {
        operation: retained.operation,
        seed: retained.seed,
        fault: retained.fault,
        replay,
        public_results,
        public_legs,
    })
}

/// One public-path I37 matrix cell returned to shared campaign dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataI37MatrixCaseEvidence {
    pub case_index: u64,
    pub coverage_key: &'static str,
    pub evidence: MetadataOperationEvidence,
}

/// One family-owned replay plant. Shared replay chooses which serialized
/// stream to compare; this adapter mutates only the corresponding metadata DTO.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataReplayMutation {
    OracleObserved,
    ExecutionReceipt,
    MutationByte,
    SameSeedDigest,
}

/// Returns an independently mutated copy for shared replay rejection tests.
pub fn apply_metadata_replay_mutation(
    evidence: &MetadataOperationEvidence,
    mutation: MetadataReplayMutation,
) -> Result<MetadataOperationEvidence, String> {
    let mut mutated = evidence.clone();
    match mutation {
        MetadataReplayMutation::OracleObserved => match &mut mutated.invariant {
            MetadataInvariantEvidence::I36 { observed, .. } => {
                let cell = observed
                    .public_rows
                    .first_mut()
                    .and_then(|row| row.values_mut().next())
                    .ok_or_else(|| {
                        "metadata replay I36 observation has no primitive cell".to_owned()
                    })?;
                *cell = match cell {
                    independent::ScalarCell::Null => independent::ScalarCell::Bool(true),
                    independent::ScalarCell::U64(value) => independent::ScalarCell::U64(*value ^ 1),
                    independent::ScalarCell::I64(value) => independent::ScalarCell::I64(*value ^ 1),
                    independent::ScalarCell::F64Bits(value) => {
                        independent::ScalarCell::F64Bits(*value ^ 1)
                    }
                    independent::ScalarCell::Bool(value) => independent::ScalarCell::Bool(!*value),
                    independent::ScalarCell::Utf8(value) => {
                        let mut value = value.clone();
                        value.push(0xff);
                        independent::ScalarCell::Utf8(value)
                    }
                };
            }
            MetadataInvariantEvidence::I37 { observed, .. } => {
                if !observed.evaluator.remove(&0) {
                    observed.evaluator.insert(0);
                }
            }
            MetadataInvariantEvidence::I38 { observed, .. } => {
                let hit = observed
                    .filtered_exact
                    .first_mut()
                    .ok_or_else(|| "metadata replay I38 observation has no exact hit".to_owned())?;
                hit.distance_bits ^= 1;
            }
            MetadataInvariantEvidence::I39 { observed, .. } => {
                let receipt = observed
                    .receipts
                    .first_mut()
                    .ok_or_else(|| "metadata replay I39 observation has no receipt".to_owned())?;
                receipt.returned_candidates ^= 1;
            }
        },
        MetadataReplayMutation::ExecutionReceipt => {
            let receipt = mutated.execution_receipts.first_mut().ok_or_else(|| {
                "metadata replay evidence has no production execution receipt".to_owned()
            })?;
            receipt.returned_candidates ^= 1;
        }
        MetadataReplayMutation::MutationByte => {
            let artifact = mutated
                .mutation
                .as_mut()
                .or_else(|| mutated.fixture_mutations.first_mut())
                .ok_or_else(|| "metadata replay evidence has no mutation artifact".to_owned())?;
            let byte = artifact
                .after
                .first_mut()
                .ok_or_else(|| "metadata replay mutation artifact has no after byte".to_owned())?;
            *byte ^= 1;
        }
        MetadataReplayMutation::SameSeedDigest => {
            mutated.control.fault_initial_directory.digest ^= 1;
        }
    }
    Ok(mutated)
}

/// Runs exactly one metadata operation and optional operation-matching fault.
pub fn run_metadata_operation(
    operation: MetadataOperationKind,
    seed: u64,
    fault: Option<MetadataFaultKind>,
) -> Result<MetadataOperationEvidence, String> {
    if fault.is_some_and(|selected| selected.operation() != operation) {
        return Err(format!(
            "metadata fault {fault:?} does not belong to operation {operation:?}"
        ));
    }
    let mut evidence = match operation {
        MetadataOperationKind::Columns => run_columns(seed, fault),
        MetadataOperationKind::Bitmap => run_bitmap(seed, fault),
        MetadataOperationKind::Planner => run_planner(seed, fault),
        MetadataOperationKind::Execution => run_execution(seed, fault),
    }?;
    evidence.control.normalized_schedule_digest = normalized_schedule_digest(
        evidence.operation,
        evidence.control.query_id_base,
        &evidence.queries,
    )?;
    let (directory_relation, outcome) = match (operation, fault) {
        (MetadataOperationKind::Planner, None) => (
            MetadataDirectoryRelation::UnpairedSingleDirectory,
            MetadataControlOutcome::FilteredSucceeded,
        ),
        (MetadataOperationKind::Columns, Some(MetadataFaultKind::ColumnCorruption))
        | (MetadataOperationKind::Bitmap, Some(MetadataFaultKind::BitmapTruncation)) => (
            MetadataDirectoryRelation::DistinctByteIdentical,
            MetadataControlOutcome::FaultRefusedRetryEquivalent,
        ),
        (MetadataOperationKind::Execution, Some(MetadataFaultKind::SelectivityBoundary)) => (
            MetadataDirectoryRelation::DistinctByteIdentical,
            MetadataControlOutcome::FaultAndRetryEquivalent,
        ),
        (MetadataOperationKind::Execution, Some(MetadataFaultKind::VisitedBudgetFallback)) => (
            MetadataDirectoryRelation::DistinctByteIdentical,
            MetadataControlOutcome::GraphFaultFallbackRetryEquivalent,
        ),
        (MetadataOperationKind::Columns | MetadataOperationKind::Bitmap, None)
        | (MetadataOperationKind::Execution, None) => (
            MetadataDirectoryRelation::DistinctByteIdentical,
            MetadataControlOutcome::Only,
        ),
        _ => {
            return Err(format!(
                "metadata control has no relation for operation={operation:?} fault={fault:?}"
            ));
        }
    };
    evidence.control.directory_relation = directory_relation;
    evidence.control.outcome = outcome;
    Ok(evidence)
}

/// Runs every I37 predicate case through the real public Bitmap operation.
///
/// The returned cases are the only family-owned basis for exhaustive matrix
/// credit. Callers must compare and credit every item; the case count or key
/// catalog alone is not execution evidence.
pub fn run_i37_complete_public_matrix(
    campaign_seed: u64,
) -> Result<Vec<MetadataI37MatrixCaseEvidence>, String> {
    let case_span = I37_PREDICATE_CASE_COUNT
        .checked_sub(1)
        .ok_or_else(|| "metadata I37 matrix has no cases".to_owned())?;
    let max_block = u64::MAX
        .checked_sub(case_span)
        .ok_or_else(|| "metadata I37 matrix span exceeds u64".to_owned())?
        / I37_PREDICATE_CASE_COUNT;
    let block_count = max_block
        .checked_add(1)
        .ok_or_else(|| "metadata I37 matrix block count exceeds u64".to_owned())?;
    let first_case_seed = (campaign_seed % block_count)
        .checked_mul(I37_PREDICATE_CASE_COUNT)
        .ok_or_else(|| "metadata I37 matrix seed block exceeds u64".to_owned())?;

    (0..I37_PREDICATE_CASE_COUNT)
        .map(|case_index| {
            let case_seed = first_case_seed
                .checked_add(case_index)
                .ok_or_else(|| "metadata I37 matrix case seed exceeds u64".to_owned())?;
            let evidence = run_metadata_operation(MetadataOperationKind::Bitmap, case_seed, None)?;
            let coverage_key = i37_predicate_case_key(case_seed);
            if case_seed % I37_PREDICATE_CASE_COUNT != case_index {
                return Err(format!(
                    "metadata I37 matrix case {case_index} mapped to seed {case_seed}"
                ));
            }
            match &evidence.invariant {
                MetadataInvariantEvidence::I37 { .. } => {}
                _ => {
                    return Err(format!(
                        "metadata I37 matrix case {case_index} returned a non-I37 invariant"
                    ));
                }
            }
            Ok(MetadataI37MatrixCaseEvidence {
                case_index,
                coverage_key,
                evidence,
            })
        })
        .collect()
}

fn query_id_base(seed: u64, operation: MetadataOperationKind) -> u64 {
    let tag = match operation {
        MetadataOperationKind::Columns => 0x36,
        MetadataOperationKind::Bitmap => 0x37,
        MetadataOperationKind::Planner => 0x38,
        MetadataOperationKind::Execution => 0x39,
    };
    seed.rotate_left(17) ^ tag
}

fn digest(bytes: &[u8]) -> u64 {
    xxh3_64(bytes)
}

fn schedule_push_bytes(canonical: &mut Vec<u8>, bytes: &[u8]) -> Result<(), String> {
    canonical.extend_from_slice(
        &u64::try_from(bytes.len())
            .map_err(|_| "metadata schedule field length exceeds u64".to_owned())?
            .to_le_bytes(),
    );
    canonical.extend_from_slice(bytes);
    Ok(())
}

fn schedule_push_scalar(
    canonical: &mut Vec<u8>,
    value: &independent::ScalarCell,
) -> Result<(), String> {
    match value {
        independent::ScalarCell::Null => canonical.push(0),
        independent::ScalarCell::U64(value) => {
            canonical.push(1);
            canonical.extend_from_slice(&value.to_le_bytes());
        }
        independent::ScalarCell::I64(value) => {
            canonical.push(2);
            canonical.extend_from_slice(&value.to_le_bytes());
        }
        independent::ScalarCell::F64Bits(value) => {
            canonical.push(3);
            canonical.extend_from_slice(&value.to_le_bytes());
        }
        independent::ScalarCell::Bool(value) => {
            canonical.push(4);
            canonical.push(u8::from(*value));
        }
        independent::ScalarCell::Utf8(value) => {
            canonical.push(5);
            schedule_push_bytes(canonical, value)?;
        }
    }
    Ok(())
}

fn schedule_push_bound(
    canonical: &mut Vec<u8>,
    bound: &Option<independent::RangeBoundDto>,
) -> Result<(), String> {
    let Some(bound) = bound else {
        canonical.push(0);
        return Ok(());
    };
    canonical.push(1);
    canonical.push(u8::from(bound.inclusive));
    schedule_push_scalar(canonical, &bound.value)
}

fn schedule_push_predicate(
    canonical: &mut Vec<u8>,
    predicate: &independent::PredicateDto,
) -> Result<(), String> {
    match predicate {
        independent::PredicateDto::Eq { column, value } => {
            canonical.push(0);
            canonical.extend_from_slice(&column.to_le_bytes());
            schedule_push_scalar(canonical, value)?;
        }
        independent::PredicateDto::In { column, values } => {
            canonical.push(1);
            canonical.extend_from_slice(&column.to_le_bytes());
            canonical.extend_from_slice(
                &u64::try_from(values.len())
                    .map_err(|_| "metadata schedule In length exceeds u64".to_owned())?
                    .to_le_bytes(),
            );
            for value in values {
                schedule_push_scalar(canonical, value)?;
            }
        }
        independent::PredicateDto::Range {
            column,
            lower,
            upper,
        } => {
            canonical.push(2);
            canonical.extend_from_slice(&column.to_le_bytes());
            schedule_push_bound(canonical, lower)?;
            schedule_push_bound(canonical, upper)?;
        }
        independent::PredicateDto::Exists(column) => {
            canonical.push(3);
            canonical.extend_from_slice(&column.to_le_bytes());
        }
        independent::PredicateDto::IsNull(column) => {
            canonical.push(4);
            canonical.extend_from_slice(&column.to_le_bytes());
        }
        independent::PredicateDto::And(children) => {
            canonical.push(5);
            canonical.extend_from_slice(
                &u64::try_from(children.len())
                    .map_err(|_| "metadata schedule And length exceeds u64".to_owned())?
                    .to_le_bytes(),
            );
            for child in children {
                schedule_push_predicate(canonical, child)?;
            }
        }
        independent::PredicateDto::Or(children) => {
            canonical.push(6);
            canonical.extend_from_slice(
                &u64::try_from(children.len())
                    .map_err(|_| "metadata schedule Or length exceeds u64".to_owned())?
                    .to_le_bytes(),
            );
            for child in children {
                schedule_push_predicate(canonical, child)?;
            }
        }
        independent::PredicateDto::Not(child) => {
            canonical.push(7);
            schedule_push_predicate(canonical, child)?;
        }
    }
    Ok(())
}

fn normalized_schedule_digest(
    operation: MetadataOperationKind,
    query_id_base: u64,
    queries: &[MetadataQueryEvidence],
) -> Result<u64, String> {
    let mut canonical = b"metadata/normalized-operation-query-schedule/v1".to_vec();
    canonical.push(match operation {
        MetadataOperationKind::Columns => 0,
        MetadataOperationKind::Bitmap => 1,
        MetadataOperationKind::Planner => 2,
        MetadataOperationKind::Execution => 3,
    });
    canonical.extend_from_slice(
        &u64::try_from(queries.len())
            .map_err(|_| "metadata query schedule length exceeds u64".to_owned())?
            .to_le_bytes(),
    );
    for query in queries {
        canonical.extend_from_slice(&query.query_id.wrapping_sub(query_id_base).to_le_bytes());
        schedule_push_bytes(&mut canonical, query.phase.as_bytes())?;
        schedule_push_predicate(&mut canonical, &query.predicate)?;
        canonical.extend_from_slice(
            &u64::try_from(query.query_vector_bits.len())
                .map_err(|_| "metadata query vector length exceeds u64".to_owned())?
                .to_le_bytes(),
        );
        for bits in &query.query_vector_bits {
            canonical.extend_from_slice(&bits.to_le_bytes());
        }
        canonical.extend_from_slice(&query.k.to_le_bytes());
        schedule_push_bytes(&mut canonical, query.tier.as_bytes())?;
        canonical.extend_from_slice(
            &u64::try_from(query.expected_sources.len())
                .map_err(|_| "metadata expected source count exceeds u64".to_owned())?
                .to_le_bytes(),
        );
        for source in &query.expected_sources {
            schedule_push_bytes(&mut canonical, source.as_bytes())?;
        }
    }
    Ok(digest(&canonical))
}

fn collect_directory_files(
    root: &Path,
    directory: &Path,
    output: &mut Vec<MetadataFileDigestFact>,
) -> Result<(), String> {
    let mut entries = std::fs::read_dir(directory)
        .map_err(|error| format!("read metadata directory {}: {error}", directory.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("enumerate metadata directory: {error}"))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let file_type = entry
            .file_type()
            .map_err(|error| format!("read metadata file type: {error}"))?;
        let path = entry.path();
        if file_type.is_dir() {
            collect_directory_files(root, &path, output)?;
            continue;
        }
        if !file_type.is_file() {
            return Err(format!(
                "metadata control directory contains non-file {}",
                path.display()
            ));
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|error| format!("relativize metadata file: {error}"))?
            .to_str()
            .ok_or_else(|| "metadata relative path is not UTF-8".to_owned())?
            .replace(std::path::MAIN_SEPARATOR, "/");
        let bytes = std::fs::read(&path)
            .map_err(|error| format!("read metadata control file {}: {error}", path.display()))?;
        output.push(MetadataFileDigestFact {
            relative_path: relative,
            byte_length: u64::try_from(bytes.len())
                .map_err(|_| "metadata control file length exceeds u64".to_owned())?,
            digest: digest(&bytes),
        });
    }
    Ok(())
}

fn directory_digest(directory: &Path) -> Result<MetadataDirectoryDigestEvidence, String> {
    let mut files = Vec::new();
    collect_directory_files(directory, directory, &mut files)?;
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    let mut canonical = Vec::new();
    for file in &files {
        let path = file.relative_path.as_bytes();
        canonical.extend_from_slice(
            &u64::try_from(path.len())
                .map_err(|_| "metadata relative path length exceeds u64".to_owned())?
                .to_le_bytes(),
        );
        canonical.extend_from_slice(path);
        canonical.extend_from_slice(&file.byte_length.to_le_bytes());
        canonical.extend_from_slice(&file.digest.to_le_bytes());
    }
    Ok(MetadataDirectoryDigestEvidence {
        digest: digest(&canonical),
        files,
    })
}

fn capture_metadata_retained_product_leg(
    directory: &Path,
    phase: MetadataRetainedPhase,
    query_vector_bits: Vec<u32>,
    k: u64,
    tier: MetadataRetainedSearchTier,
    arm: Option<MetadataTestArm>,
    expected: MetadataRetainedExpectedOutcome,
) -> Result<MetadataRetainedProductLeg, String> {
    fn collect(
        root: &Path,
        directory: &Path,
        files: &mut Vec<MetadataRetainedFile>,
    ) -> Result<(), String> {
        let mut entries = std::fs::read_dir(directory)
            .map_err(|error| format!("read retained metadata directory: {error}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("enumerate retained metadata directory: {error}"))?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let file_type = entry
                .file_type()
                .map_err(|error| format!("read retained metadata file type: {error}"))?;
            let path = entry.path();
            if file_type.is_dir() {
                collect(root, &path, files)?;
            } else if file_type.is_file() {
                let relative_path = path
                    .strip_prefix(root)
                    .map_err(|error| format!("relativize retained metadata file: {error}"))?
                    .to_str()
                    .ok_or_else(|| "retained metadata path is not UTF-8".to_owned())?
                    .replace(std::path::MAIN_SEPARATOR, "/");
                files.push(MetadataRetainedFile {
                    relative_path,
                    bytes: std::fs::read(&path).map_err(|error| {
                        format!("read retained metadata file {}: {error}", path.display())
                    })?,
                });
            } else {
                return Err(format!(
                    "retained metadata fixture contains non-file {}",
                    path.display()
                ));
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    collect(directory, directory, &mut files)?;
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    if files.is_empty() {
        return Err("retained metadata product fixture has no files".to_owned());
    }
    Ok(MetadataRetainedProductLeg {
        phase,
        files,
        predicate: None,
        query_vector_bits,
        k,
        tier,
        arm,
        expected,
    })
}

fn capture_metadata_retained_product_fixture(
    directory: &Path,
    query_vector_bits: Vec<u32>,
    k: u64,
    tier: MetadataRetainedSearchTier,
    expected_results: Vec<MetadataResultFact>,
) -> Result<MetadataRetainedProductFixture, String> {
    Ok(MetadataRetainedProductFixture {
        legs: vec![capture_metadata_retained_product_leg(
            directory,
            MetadataRetainedPhase::Clean,
            query_vector_bits,
            k,
            tier,
            None,
            MetadataRetainedExpectedOutcome::Results(expected_results),
        )?],
    })
}

fn copy_directory(source: &Path) -> Result<TempDir, String> {
    fn copy_tree(source: &Path, target: &Path) -> Result<(), String> {
        let mut entries = std::fs::read_dir(source)
            .map_err(|error| format!("read metadata copy source: {error}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("enumerate metadata copy source: {error}"))?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let file_type = entry
                .file_type()
                .map_err(|error| format!("read metadata copy file type: {error}"))?;
            let from = entry.path();
            let to = target.join(entry.file_name());
            if file_type.is_dir() {
                std::fs::create_dir(&to)
                    .map_err(|error| format!("create metadata copy directory: {error}"))?;
                copy_tree(&from, &to)?;
            } else if file_type.is_file() {
                std::fs::copy(&from, &to)
                    .map_err(|error| format!("copy metadata control file: {error}"))?;
            } else {
                return Err(format!(
                    "metadata copy source contains non-file {}",
                    from.display()
                ));
            }
        }
        Ok(())
    }

    let target = tempdir().map_err(|error| format!("metadata copy tempdir: {error}"))?;
    copy_tree(source, target.path())?;
    Ok(target)
}

fn wal_digest(directory: &Path) -> Result<u64, String> {
    std::fs::read(directory.join("wal.ze"))
        .map(|bytes| digest(&bytes))
        .map_err(|error| format!("read metadata WAL: {error}"))
}

fn source_label(source: RowSource) -> String {
    match source {
        RowSource::Active => "active".to_owned(),
        RowSource::Sealed(id) => format!("sealed-{}", id.file_name()),
    }
}

fn result_facts(outcome: &FilteredSearchOutcome) -> Vec<MetadataResultFact> {
    outcome
        .candidates
        .iter()
        .map(|candidate| MetadataResultFact {
            source: source_label(candidate.row_id().source()),
            row_id: candidate.row_id().local_row(),
            document_id: candidate.document().map(|document| document.doc_id().get()),
            score_bits: candidate.score().to_bits(),
        })
        .collect()
}

fn independent_exact_result_facts(
    seed: u64,
    source: RowSource,
    rows: impl IntoIterator<Item = u32>,
) -> Result<Vec<MetadataResultFact>, String> {
    let source = source_label(source);
    rows.into_iter()
        .map(|row| {
            let document_id = (u128::from(seed) << 32)
                .checked_add(u128::from(row))
                .and_then(|value| value.checked_add(1))
                .ok_or_else(|| "metadata expected document id overflow".to_owned())?;
            let coordinate = row as f32;
            let squared_l2 = coordinate.mul_add(coordinate, coordinate * coordinate);
            Ok(MetadataResultFact {
                source: source.clone(),
                row_id: row,
                document_id: Some(document_id),
                score_bits: (-squared_l2).to_bits(),
            })
        })
        .collect()
}

fn ordered_policy() -> DurabilityPolicy {
    DurabilityPolicy::new(DurabilityMode::Durable, CommitTier::Ordered)
        .expect("static ordered durability policy")
}

fn open_with_controller(
    directory: &Path,
    options: OpenOptions,
    controller: &Arc<MetadataTestController>,
) -> Result<Store, String> {
    Store::open_with_test_dependencies(
        directory,
        options,
        StoreTestDependencies::new(Arc::new(StdVfs), Arc::new(SystemMonotonicClock))
            .with_metadata_test_controller(Arc::clone(controller)),
    )
    .map_err(|error| format!("open metadata Store: {error}"))
}

const SEGMENT_FILE_MAGIC: &[u8; 8] = b"ZEPEMBED";
const SEGMENT_FILE_FAMILY: u16 = 2;
const SEGMENT_FILE_VERSION: u16 = 1;
const SEGMENT_FIXED_HEADER_BYTES: usize = 32;
const SEGMENT_PREFIX_BYTES: usize = 32;
const SEGMENT_DIRECTORY_ENTRY_BYTES: usize = 32;
const SEGMENT_HEADER_CHECKSUM_BYTES: usize = 8;
const SEGMENT_FILE_TRAILER_BYTES: usize = 8;
const SEGMENT_REGION_ALIGNMENT: usize = 16 * 1024;
const SEGMENT_CHECKSUM_CHUNK_BYTES: usize = 64 * 1024;

fn read_u16(bytes: &[u8], offset: usize, field: &str) -> Result<u16, String> {
    bytes
        .get(offset..offset.saturating_add(2))
        .ok_or_else(|| format!("segment envelope {field} is truncated"))?
        .try_into()
        .map(u16::from_le_bytes)
        .map_err(|_| format!("segment envelope {field} is not u16"))
}

fn read_u32(bytes: &[u8], offset: usize, field: &str) -> Result<u32, String> {
    bytes
        .get(offset..offset.saturating_add(4))
        .ok_or_else(|| format!("segment envelope {field} is truncated"))?
        .try_into()
        .map(u32::from_le_bytes)
        .map_err(|_| format!("segment envelope {field} is not u32"))
}

fn read_u64(bytes: &[u8], offset: usize, field: &str) -> Result<u64, String> {
    bytes
        .get(offset..offset.saturating_add(8))
        .ok_or_else(|| format!("segment envelope {field} is truncated"))?
        .try_into()
        .map(u64::from_le_bytes)
        .map_err(|_| format!("segment envelope {field} is not u64"))
}

fn directory_entry_bounds(bytes: &[u8], entry: usize) -> Result<(usize, usize), String> {
    let directory = 64_usize
        .checked_add(
            entry
                .checked_mul(32)
                .ok_or_else(|| "metadata directory offset overflow".to_owned())?,
        )
        .ok_or_else(|| "metadata directory offset overflow".to_owned())?;
    let offset = usize::try_from(read_u64(bytes, directory + 8, "region offset")?)
        .map_err(|_| "metadata region offset exceeds usize".to_owned())?;
    let length = usize::try_from(read_u64(bytes, directory + 16, "region length")?)
        .map_err(|_| "metadata region length exceeds usize".to_owned())?;
    Ok((offset, length))
}

fn directory_entry_for_kind(bytes: &[u8], kind: RegionKind) -> Result<usize, String> {
    let count = usize::from(read_u16(bytes, 52, "region count")?);
    (0..count)
        .find(|entry| {
            let directory = 64_usize.saturating_add(entry.saturating_mul(32));
            read_u16(bytes, directory, "region kind") == Ok(kind.id())
        })
        .ok_or_else(|| format!("metadata segment lacks {kind:?} directory entry"))
}

fn validate_rewritten_region_integrity(bytes: &[u8], target: RegionKind) -> Result<(), String> {
    let target_entry = directory_entry_for_kind(bytes, target)?;
    let target_directory = 64_usize.saturating_add(target_entry.saturating_mul(32));
    let (target_offset, target_length) = directory_entry_bounds(bytes, target_entry)?;
    let target_bytes = bytes
        .get(target_offset..target_offset.saturating_add(target_length))
        .ok_or_else(|| "metadata target region is truncated".to_owned())?;
    let target_checksum = read_u64(bytes, target_directory + 24, "target region checksum")?;
    if target_checksum != digest(target_bytes) {
        return Err(format!("{target:?} directory checksum is stale"));
    }

    let table_entry = directory_entry_for_kind(bytes, RegionKind::ChecksumTable)?;
    let table_directory = 64_usize.saturating_add(table_entry.saturating_mul(32));
    let (table_offset, table_length) = directory_entry_bounds(bytes, table_entry)?;
    let table = bytes
        .get(table_offset..table_offset.saturating_add(table_length))
        .ok_or_else(|| "metadata ChecksumTable region is truncated".to_owned())?;
    let count = usize::try_from(read_u32(table, 0, "ChecksumTable count")?)
        .map_err(|_| "metadata ChecksumTable count exceeds usize".to_owned())?;
    let chunk_size = usize::try_from(read_u32(table, 4, "ChecksumTable chunk size")?)
        .map_err(|_| "metadata ChecksumTable chunk size exceeds usize".to_owned())?;
    if chunk_size != SEGMENT_CHECKSUM_CHUNK_BYTES {
        return Err(format!(
            "metadata ChecksumTable chunk size {chunk_size} is not {SEGMENT_CHECKSUM_CHUNK_BYTES}"
        ));
    }
    let mut observed_chunks = 0_usize;
    for index in 0..count {
        let entry = 8_usize
            .checked_add(index.saturating_mul(16))
            .ok_or_else(|| "metadata ChecksumTable entry offset overflow".to_owned())?;
        let entry_kind = read_u16(table, entry, "ChecksumTable region kind")?;
        let chunk_index = usize::try_from(read_u32(table, entry + 4, "ChecksumTable chunk")?)
            .map_err(|_| "metadata ChecksumTable chunk exceeds usize".to_owned())?;
        if entry_kind != target.id() {
            continue;
        }
        observed_chunks = observed_chunks.saturating_add(1);
        let relative_start = chunk_index
            .checked_mul(chunk_size)
            .ok_or_else(|| "metadata target chunk offset overflow".to_owned())?;
        let relative_end = relative_start.saturating_add(chunk_size).min(target_length);
        let chunk = target_bytes
            .get(relative_start..relative_end)
            .ok_or_else(|| "metadata target chunk is outside region".to_owned())?;
        let observed = read_u64(table, entry + 8, "ChecksumTable checksum")?;
        let expected = digest(chunk);
        if observed != expected {
            return Err(format!(
                "{target:?} ChecksumTable chunk {chunk_index} checksum is stale expected={expected} observed={observed}"
            ));
        }
    }
    if observed_chunks != target_length.div_ceil(chunk_size) {
        return Err(format!(
            "{target:?} ChecksumTable chunk count expected={} observed={observed_chunks}",
            target_length.div_ceil(chunk_size)
        ));
    }
    let table_checksum = read_u64(bytes, table_directory + 24, "ChecksumTable region checksum")?;
    if table_checksum != digest(table) {
        return Err("ChecksumTable directory checksum is stale".to_owned());
    }
    region_bounds(bytes, target_entry).map(|_| ())
}

fn region_bounds(bytes: &[u8], entry: usize) -> Result<(usize, usize), String> {
    if bytes.get(..8) != Some(SEGMENT_FILE_MAGIC.as_slice()) {
        return Err("segment envelope magic is not ZEPEMBED".to_owned());
    }
    if read_u16(bytes, 8, "family")? != SEGMENT_FILE_FAMILY {
        return Err("segment envelope family is not Segment".to_owned());
    }
    if read_u16(bytes, 10, "version")? != SEGMENT_FILE_VERSION {
        return Err("segment envelope version is not v1".to_owned());
    }
    if read_u32(bytes, 12, "flags")? != 0 {
        return Err("segment envelope flags are nonzero".to_owned());
    }
    let header_length = usize::try_from(read_u64(bytes, 16, "header length")?)
        .map_err(|_| "segment envelope header length exceeds usize".to_owned())?;
    let file_length = usize::try_from(read_u64(bytes, 24, "file length")?)
        .map_err(|_| "segment envelope file length exceeds usize".to_owned())?;
    if file_length != bytes.len() {
        return Err(format!(
            "segment envelope file length mismatch declared={file_length} observed={}",
            bytes.len()
        ));
    }
    if read_u16(bytes, 54, "prefix reserved")? != 0 || read_u16(bytes, 58, "scheme padding")? != 0 {
        return Err("segment envelope prefix reserved bytes are nonzero".to_owned());
    }
    let region_count = usize::from(read_u16(bytes, 52, "region count")?);
    let expected_header_length = SEGMENT_FIXED_HEADER_BYTES
        .checked_add(SEGMENT_PREFIX_BYTES)
        .and_then(|value| {
            value.checked_add(region_count.saturating_mul(SEGMENT_DIRECTORY_ENTRY_BYTES))
        })
        .and_then(|value| value.checked_add(SEGMENT_HEADER_CHECKSUM_BYTES))
        .ok_or_else(|| "segment envelope header length overflow".to_owned())?;
    if header_length != expected_header_length {
        return Err(format!(
            "segment envelope header length mismatch declared={header_length} implied={expected_header_length}"
        ));
    }
    if header_length > bytes.len().saturating_sub(SEGMENT_FILE_TRAILER_BYTES) {
        return Err("segment envelope header escapes file".to_owned());
    }
    let expected_header_checksum = read_u64(
        bytes,
        header_length.saturating_sub(SEGMENT_HEADER_CHECKSUM_BYTES),
        "header checksum",
    )?;
    let observed_header_checksum = digest(
        bytes
            .get(..header_length.saturating_sub(SEGMENT_HEADER_CHECKSUM_BYTES))
            .ok_or_else(|| "segment envelope header checksum span is invalid".to_owned())?,
    );
    if expected_header_checksum != observed_header_checksum {
        return Err("segment envelope header checksum mismatch".to_owned());
    }
    let trailer = bytes
        .len()
        .checked_sub(SEGMENT_FILE_TRAILER_BYTES)
        .ok_or_else(|| "segment envelope file trailer is absent".to_owned())?;
    if read_u64(bytes, trailer, "file checksum")? != digest(&bytes[..trailer]) {
        return Err("segment envelope file checksum mismatch".to_owned());
    }
    if entry >= region_count {
        return Err(format!(
            "segment envelope directory entry {entry} is outside count {region_count}"
        ));
    }
    let mut kinds = BTreeSet::new();
    let mut spans = Vec::with_capacity(region_count);
    for position in 0..region_count {
        let directory = SEGMENT_FIXED_HEADER_BYTES
            .checked_add(SEGMENT_PREFIX_BYTES)
            .and_then(|value| {
                value.checked_add(position.saturating_mul(SEGMENT_DIRECTORY_ENTRY_BYTES))
            })
            .ok_or_else(|| "segment envelope directory offset overflow".to_owned())?;
        let kind = read_u16(bytes, directory, "region kind")?;
        if !kinds.insert(kind) {
            return Err(format!("segment envelope duplicate region kind {kind}"));
        }
        if read_u16(bytes, directory + 2, "region version")? == 0 {
            return Err(format!("segment envelope region {kind} has version zero"));
        }
        if read_u32(bytes, directory + 4, "region reserved")? != 0 {
            return Err(format!(
                "segment envelope region {kind} reserved is nonzero"
            ));
        }
        let (offset, length) = directory_entry_bounds(bytes, position)?;
        if !offset.is_multiple_of(SEGMENT_REGION_ALIGNMENT) {
            return Err(format!(
                "segment envelope region {kind} offset is unaligned"
            ));
        }
        let end = offset
            .checked_add(length)
            .ok_or_else(|| "segment envelope region end overflow".to_owned())?;
        if offset < header_length || end > trailer {
            return Err(format!("segment envelope region {kind} escapes payload"));
        }
        if spans
            .iter()
            .any(|(prior_start, prior_end)| offset < *prior_end && *prior_start < end)
        {
            return Err(format!(
                "segment envelope region {kind} overlaps another region"
            ));
        }
        spans.push((offset, end));
    }
    directory_entry_bounds(bytes, entry)
}

fn rewrite_checksum_field(
    bytes: &mut [u8],
    absolute_offset: usize,
    after: u64,
    field: MetadataChecksumField,
    rewrites: &mut Vec<MetadataChecksumRewriteEvidence>,
) -> Result<(), String> {
    let before = read_u64(bytes, absolute_offset, "checksum rewrite field")?;
    bytes
        .get_mut(absolute_offset..absolute_offset.saturating_add(8))
        .ok_or_else(|| "metadata checksum rewrite field is truncated".to_owned())?
        .copy_from_slice(&after.to_le_bytes());
    rewrites.push(MetadataChecksumRewriteEvidence {
        field,
        absolute_offset: u64::try_from(absolute_offset)
            .map_err(|_| "metadata checksum rewrite offset exceeds u64".to_owned())?,
        before,
        after,
    });
    Ok(())
}

fn rewrite_header_and_file(
    bytes: &mut [u8],
    rewrites: &mut Vec<MetadataChecksumRewriteEvidence>,
) -> Result<(), String> {
    let header_length = bytes
        .get(16..24)
        .ok_or_else(|| "metadata segment header length is truncated".to_owned())?;
    let header_length = usize::try_from(u64::from_le_bytes(
        header_length
            .try_into()
            .map_err(|_| "metadata header length is not u64".to_owned())?,
    ))
    .map_err(|_| "metadata header length exceeds usize".to_owned())?;
    let header_payload = bytes
        .get(..header_length.saturating_sub(8))
        .ok_or_else(|| "metadata header checksum span is invalid".to_owned())?;
    let header_checksum = digest(header_payload);
    rewrite_checksum_field(
        bytes,
        header_length.saturating_sub(8),
        header_checksum,
        MetadataChecksumField::SegmentHeader,
        rewrites,
    )?;
    let trailer = bytes
        .len()
        .checked_sub(8)
        .ok_or_else(|| "metadata segment trailer is truncated".to_owned())?;
    let file_checksum = digest(
        bytes
            .get(..trailer)
            .ok_or_else(|| "metadata file checksum span is invalid".to_owned())?,
    );
    rewrite_checksum_field(
        bytes,
        trailer,
        file_checksum,
        MetadataChecksumField::SegmentWholeFile,
        rewrites,
    )
}

fn rewrite_region(
    bytes: &mut [u8],
    entry: usize,
) -> Result<Vec<MetadataChecksumRewriteEvidence>, String> {
    let mut checksum_rewrites = Vec::new();
    let directory = 64_usize
        .checked_add(
            entry
                .checked_mul(32)
                .ok_or_else(|| "metadata directory offset overflow".to_owned())?,
        )
        .ok_or_else(|| "metadata directory offset overflow".to_owned())?;
    let target_kind = read_u16(bytes, directory, "target region kind")?;
    if target_kind == RegionKind::ChecksumTable.id() {
        return Err("metadata target region cannot be ChecksumTable".to_owned());
    }
    let target_region = RegionKind::from_id(target_kind)
        .ok_or_else(|| format!("metadata target region kind {target_kind} is unknown"))?;
    let (offset, length) = directory_entry_bounds(bytes, entry)?;
    let checksum = digest(
        bytes
            .get(offset..offset.saturating_add(length))
            .ok_or_else(|| "metadata region span is truncated".to_owned())?,
    );
    rewrite_checksum_field(
        bytes,
        directory + 24,
        checksum,
        MetadataChecksumField::TargetRegion {
            region: target_region,
        },
        &mut checksum_rewrites,
    )?;

    let table_entry = directory_entry_for_kind(bytes, RegionKind::ChecksumTable)?;
    let table_directory = 64_usize
        .checked_add(
            table_entry
                .checked_mul(32)
                .ok_or_else(|| "metadata ChecksumTable directory offset overflow".to_owned())?,
        )
        .ok_or_else(|| "metadata ChecksumTable directory offset overflow".to_owned())?;
    let (table_offset, table_length) = directory_entry_bounds(bytes, table_entry)?;
    let table = bytes
        .get(table_offset..table_offset.saturating_add(table_length))
        .ok_or_else(|| "metadata ChecksumTable region is truncated".to_owned())?;
    let table_count = usize::try_from(read_u32(table, 0, "ChecksumTable count")?)
        .map_err(|_| "metadata ChecksumTable count exceeds usize".to_owned())?;
    let chunk_size = usize::try_from(read_u32(table, 4, "ChecksumTable chunk size")?)
        .map_err(|_| "metadata ChecksumTable chunk size exceeds usize".to_owned())?;
    if chunk_size != SEGMENT_CHECKSUM_CHUNK_BYTES {
        return Err(format!(
            "metadata ChecksumTable chunk size {chunk_size} is not {SEGMENT_CHECKSUM_CHUNK_BYTES}"
        ));
    }
    let mut chunk_rewrites = Vec::new();
    for index in 0..table_count {
        let table_entry_offset = 8_usize
            .checked_add(index.saturating_mul(16))
            .ok_or_else(|| "metadata ChecksumTable entry offset overflow".to_owned())?;
        if read_u16(table, table_entry_offset, "ChecksumTable region kind")? != target_kind {
            continue;
        }
        let chunk_index = usize::try_from(read_u32(
            table,
            table_entry_offset + 4,
            "ChecksumTable chunk",
        )?)
        .map_err(|_| "metadata ChecksumTable chunk exceeds usize".to_owned())?;
        let relative_start = chunk_index
            .checked_mul(chunk_size)
            .ok_or_else(|| "metadata target chunk offset overflow".to_owned())?;
        let relative_end = relative_start.saturating_add(chunk_size).min(length);
        let region_chunk = bytes
            .get(offset.saturating_add(relative_start)..offset.saturating_add(relative_end))
            .ok_or_else(|| "metadata target chunk is outside region".to_owned())?;
        chunk_rewrites.push((table_entry_offset + 8, chunk_index, digest(region_chunk)));
    }
    if chunk_rewrites.len() != length.div_ceil(chunk_size) {
        return Err(format!(
            "metadata ChecksumTable target chunk count expected={} observed={}",
            length.div_ceil(chunk_size),
            chunk_rewrites.len()
        ));
    }
    for (relative_offset, chunk_index, checksum) in chunk_rewrites {
        let absolute = table_offset
            .checked_add(relative_offset)
            .ok_or_else(|| "metadata ChecksumTable checksum offset overflow".to_owned())?;
        rewrite_checksum_field(
            bytes,
            absolute,
            checksum,
            MetadataChecksumField::TargetRegionChunk {
                region: target_region,
                chunk_index: u32::try_from(chunk_index)
                    .map_err(|_| "metadata ChecksumTable chunk exceeds u32".to_owned())?,
            },
            &mut checksum_rewrites,
        )?;
    }
    let table_checksum = digest(
        bytes
            .get(table_offset..table_offset.saturating_add(table_length))
            .ok_or_else(|| "metadata ChecksumTable region is truncated".to_owned())?,
    );
    rewrite_checksum_field(
        bytes,
        table_directory + 24,
        table_checksum,
        MetadataChecksumField::ChecksumTableRegion,
        &mut checksum_rewrites,
    )?;
    rewrite_header_and_file(bytes, &mut checksum_rewrites)?;
    Ok(checksum_rewrites)
}

fn set_region_length(
    bytes: &mut [u8],
    entry: usize,
    length: usize,
) -> Result<Vec<MetadataChecksumRewriteEvidence>, String> {
    let directory = 64_usize
        .checked_add(
            entry
                .checked_mul(32)
                .ok_or_else(|| "metadata directory offset overflow".to_owned())?,
        )
        .ok_or_else(|| "metadata directory offset overflow".to_owned())?;
    bytes
        .get_mut(directory + 16..directory + 24)
        .ok_or_else(|| "metadata directory length field is truncated".to_owned())?
        .copy_from_slice(
            &u64::try_from(length)
                .map_err(|_| "metadata region length exceeds u64".to_owned())?
                .to_le_bytes(),
        );
    rewrite_region(bytes, entry)
}

fn branch_dto(branch: SegmentBranch) -> independent::ExecutionBranchDto {
    match branch {
        SegmentBranch::Pruned => independent::ExecutionBranchDto::Pruned,
        SegmentBranch::ExactAllowList => independent::ExecutionBranchDto::ExactAllowList,
        SegmentBranch::MaskedScan => independent::ExecutionBranchDto::MaskedScan,
        SegmentBranch::FilteredGraph => independent::ExecutionBranchDto::FilteredGraph,
        SegmentBranch::GraphExactFallback => independent::ExecutionBranchDto::GraphExactFallback,
        SegmentBranch::Graph => independent::ExecutionBranchDto::FilteredGraph,
    }
}

fn fallback_dto(fallback: PlanFallback) -> independent::FallbackReasonDto {
    match fallback {
        PlanFallback::None => independent::FallbackReasonDto::None,
        PlanFallback::VisitedBudget => independent::FallbackReasonDto::VisitedBudget,
        PlanFallback::CandidateShortfall => independent::FallbackReasonDto::CandidateShortfall,
        PlanFallback::EfWidened => independent::FallbackReasonDto::EfWidened,
    }
}

fn execution_receipt(receipt: &MetadataExecutionReceipt) -> independent::ExecutionReceiptDto {
    independent::ExecutionReceiptDto {
        key: independent::QuerySourceKey {
            query_id: receipt.query_id,
            source: source_label(receipt.source),
        },
        branch: branch_dto(receipt.branch),
        fallback: fallback_dto(receipt.fallback),
        row_count: receipt.row_count,
        filter_cardinality: receipt.filter_cardinality,
        rows_examined: receipt.rows_examined,
        allowed_rows_examined: receipt.allowed_rows_examined,
        vectors_scored: receipt.vectors_scored,
        graph_nodes_visited: receipt.graph_nodes_visited,
        exact_fallback_rows_examined: receipt.exact_fallback_rows_examined,
        returned_candidates: receipt.returned_candidates,
        ef_effective: receipt
            .ef_effective
            .and_then(|value| u64::try_from(value).ok()),
        visited_budget: receipt
            .visited_budget
            .and_then(|value| u64::try_from(value).ok()),
        sealed: receipt.sealed,
    }
}

fn report(
    query_id: u64,
    plan: &zeppelin_embed::planner::SegmentPlan,
) -> independent::BranchReportDto {
    independent::BranchReportDto {
        key: independent::QuerySourceKey {
            query_id,
            source: source_label(plan.source),
        },
        branch: branch_dto(plan.branch),
        fallback: fallback_dto(plan.fallback),
        filter_cardinality: plan.filter_cardinality,
    }
}

fn expected_scan_case(
    query_id: u64,
    source: RowSource,
    row_count: u64,
    filter_cardinality: u64,
    source_may_match: bool,
    sealed: bool,
) -> independent::I39ExpectedCase {
    let (rows_examined, allowed_rows_examined, vectors_scored, returned_candidates) =
        if !source_may_match {
            (0, 0, 0, 0)
        } else if filter_cardinality <= INDEPENDENT_ALLOW_LIST_THRESHOLD {
            (
                filter_cardinality,
                filter_cardinality,
                filter_cardinality,
                filter_cardinality,
            )
        } else {
            (
                row_count,
                filter_cardinality,
                filter_cardinality,
                filter_cardinality,
            )
        };
    independent::I39ExpectedCase {
        key: independent::QuerySourceKey {
            query_id,
            source: source_label(source),
        },
        mode: independent::I39ExecutionModeDto::ExactScan { source_may_match },
        row_count,
        filter_cardinality,
        allow_list_threshold: INDEPENDENT_ALLOW_LIST_THRESHOLD,
        rows_examined,
        allowed_rows_examined,
        vectors_scored,
        graph_nodes_visited: 0,
        exact_fallback_rows_examined: 0,
        returned_candidates,
        ef_effective: None,
        visited_budget: None,
        sealed,
    }
}

fn expected_graph_case(
    query_id: u64,
    source: RowSource,
    fallback: independent::FallbackReasonDto,
    visited_budget: u64,
    independently_reachable_nodes: u64,
) -> independent::I39ExpectedCase {
    let (
        rows_examined,
        allowed_rows_examined,
        vectors_scored,
        graph_nodes_visited,
        exact_fallback_rows_examined,
        returned_candidates,
    ) = match fallback {
        independent::FallbackReasonDto::None | independent::FallbackReasonDto::EfWidened => (
            independently_reachable_nodes,
            80,
            independently_reachable_nodes,
            independently_reachable_nodes,
            0,
            3,
        ),
        independent::FallbackReasonDto::VisitedBudget => (2, 80, 0, 2, 80, GRAPH_ROWS as u64),
        independent::FallbackReasonDto::CandidateShortfall => (4, 80, 4, 4, 80, GRAPH_ROWS as u64),
    };
    independent::I39ExpectedCase {
        key: independent::QuerySourceKey {
            query_id,
            source: source_label(source),
        },
        mode: independent::I39ExecutionModeDto::FilteredGraph {
            required_fallback: fallback,
        },
        row_count: GRAPH_ROWS as u64,
        filter_cardinality: GRAPH_ROWS as u64,
        allow_list_threshold: INDEPENDENT_ALLOW_LIST_THRESHOLD,
        rows_examined,
        allowed_rows_examined,
        vectors_scored,
        graph_nodes_visited,
        exact_fallback_rows_examined,
        returned_candidates,
        ef_effective: Some(GRAPH_EXPECTED_EF),
        visited_budget: Some(visited_budget),
        sealed: true,
    }
}

fn execution_query_evidence(case: &independent::I39ExpectedCase) -> MetadataQueryEvidence {
    let expected_sources = BTreeSet::from([case.key.source.clone()]);
    match case.mode {
        independent::I39ExecutionModeDto::ExactScan {
            source_may_match: false,
        } => query_evidence(
            case.key.query_id,
            "sealed-pruned-exact",
            independent::PredicateDto::Eq {
                column: 0,
                value: independent::ScalarCell::I64(i64::MAX),
            },
            2,
            case.row_count,
            "exact",
            expected_sources,
        ),
        independent::I39ExecutionModeDto::ExactScan {
            source_may_match: true,
        } => {
            let (phase, predicate) = if case.filter_cardinality < case.row_count {
                (
                    "active-allow-list-boundary",
                    independent::PredicateDto::Eq {
                        column: 1,
                        value: independent::ScalarCell::U64(1),
                    },
                )
            } else {
                (
                    "public-exact-scan",
                    independent::PredicateDto::And(Vec::new()),
                )
            };
            query_evidence(
                case.key.query_id,
                phase,
                predicate,
                2,
                case.row_count,
                "exact",
                expected_sources,
            )
        }
        independent::I39ExecutionModeDto::FilteredGraph { required_fallback } => query_evidence(
            case.key.query_id,
            match required_fallback {
                independent::FallbackReasonDto::None => "filtered-graph",
                independent::FallbackReasonDto::VisitedBudget => "visited-budget-fallback",
                independent::FallbackReasonDto::CandidateShortfall => {
                    "candidate-shortfall-fallback"
                }
                independent::FallbackReasonDto::EfWidened => "ef-widened-fallback",
            },
            independent::PredicateDto::And(Vec::new()),
            GRAPH_DIMS,
            if required_fallback == independent::FallbackReasonDto::CandidateShortfall {
                10
            } else {
                3
            },
            "auto",
            expected_sources,
        ),
    }
}

struct TypedFixture {
    directory: TempDir,
    schema: Schema,
    controller: Arc<MetadataTestController>,
    segment_id: SegmentId,
    segment_path: PathBuf,
    clean_segment: Vec<u8>,
    parsed_columns: independent::ParsedColumns,
    input: independent::I36Input,
    active_rows: Vec<BTreeMap<u32, independent::ScalarCell>>,
    reader_rows: Vec<BTreeMap<u32, independent::ScalarCell>>,
    public_rows: Vec<BTreeMap<u32, independent::ScalarCell>>,
    clean_outcome: FilteredSearchOutcome,
    clean_generation: u64,
    clean_wal_digest: u64,
}

fn typed_schema() -> Schema {
    Schema::new(vec![
        ColumnDefinition::new(ColumnId::new(1), "u", ColumnType::U64, true),
        ColumnDefinition::new(ColumnId::new(2), "i", ColumnType::I64, true),
        ColumnDefinition::new(ColumnId::new(3), "f", ColumnType::F64, true),
        ColumnDefinition::new(ColumnId::new(4), "b", ColumnType::Bool, true),
        ColumnDefinition::new(ColumnId::new(5), "d", ColumnType::DictionaryString, true),
        ColumnDefinition::new(ColumnId::new(6), "r", ColumnType::RawString, true),
        ColumnDefinition::new(ColumnId::new(7), "u_required", ColumnType::U64, false),
        ColumnDefinition::new(ColumnId::new(8), "i_required", ColumnType::I64, false),
        ColumnDefinition::new(ColumnId::new(9), "f_required", ColumnType::F64, false),
        ColumnDefinition::new(ColumnId::new(10), "b_required", ColumnType::Bool, false),
        ColumnDefinition::new(
            ColumnId::new(11),
            "d_required",
            ColumnType::DictionaryString,
            false,
        ),
        ColumnDefinition::new(
            ColumnId::new(12),
            "r_required",
            ColumnType::RawString,
            false,
        ),
    ])
    .expect("static metadata adapter schema")
}

fn typed_definitions() -> Vec<independent::ColumnDefinitionDto> {
    vec![
        independent::ColumnDefinitionDto {
            id: 0,
            name: b"ts".to_vec(),
            kind: independent::ColumnKind::I64,
            nullable: false,
        },
        independent::ColumnDefinitionDto {
            id: 1,
            name: b"u".to_vec(),
            kind: independent::ColumnKind::U64,
            nullable: true,
        },
        independent::ColumnDefinitionDto {
            id: 2,
            name: b"i".to_vec(),
            kind: independent::ColumnKind::I64,
            nullable: true,
        },
        independent::ColumnDefinitionDto {
            id: 3,
            name: b"f".to_vec(),
            kind: independent::ColumnKind::F64,
            nullable: true,
        },
        independent::ColumnDefinitionDto {
            id: 4,
            name: b"b".to_vec(),
            kind: independent::ColumnKind::Bool,
            nullable: true,
        },
        independent::ColumnDefinitionDto {
            id: 5,
            name: b"d".to_vec(),
            kind: independent::ColumnKind::DictionaryString,
            nullable: true,
        },
        independent::ColumnDefinitionDto {
            id: 6,
            name: b"r".to_vec(),
            kind: independent::ColumnKind::RawString,
            nullable: true,
        },
        independent::ColumnDefinitionDto {
            id: 7,
            name: b"u_required".to_vec(),
            kind: independent::ColumnKind::U64,
            nullable: false,
        },
        independent::ColumnDefinitionDto {
            id: 8,
            name: b"i_required".to_vec(),
            kind: independent::ColumnKind::I64,
            nullable: false,
        },
        independent::ColumnDefinitionDto {
            id: 9,
            name: b"f_required".to_vec(),
            kind: independent::ColumnKind::F64,
            nullable: false,
        },
        independent::ColumnDefinitionDto {
            id: 10,
            name: b"b_required".to_vec(),
            kind: independent::ColumnKind::Bool,
            nullable: false,
        },
        independent::ColumnDefinitionDto {
            id: 11,
            name: b"d_required".to_vec(),
            kind: independent::ColumnKind::DictionaryString,
            nullable: false,
        },
        independent::ColumnDefinitionDto {
            id: 12,
            name: b"r_required".to_vec(),
            kind: independent::ColumnKind::RawString,
            nullable: false,
        },
    ]
}

fn typed_rows(seed: u64) -> Vec<BTreeMap<u32, independent::ScalarCell>> {
    let base = i64::from_ne_bytes(seed.to_ne_bytes()).wrapping_shr(8);
    let cycle = seed % 24;
    (0..10_u32)
        .map(|row| {
            let timestamp = base.wrapping_add(i64::from(row));
            let float_bits = match row {
                0 => (-0.0_f64).to_bits(),
                1 => f64::INFINITY.to_bits(),
                2 => f64::NEG_INFINITY.to_bits(),
                3 => 0x7ff8_0000_0000_0001,
                4 => 0x7ff8_0000_0000_0002,
                _ => (f64::from(row) - 4.5_f64).to_bits(),
            };
            let dictionary = if row == 1 {
                Vec::new()
            } else if row.is_multiple_of(3) {
                b"one".to_vec()
            } else {
                format!("dict-{row}").into_bytes()
            };
            let raw = if row == 1 {
                Vec::new()
            } else if row == 2 {
                b"repeated-raw".to_vec()
            } else {
                format!("raw-{seed:016x}-{row}").into_bytes()
            };
            let mut cells = BTreeMap::from([
                (0, independent::ScalarCell::I64(timestamp)),
                (
                    1,
                    independent::ScalarCell::U64(seed.rotate_left(row) ^ u64::from(row)),
                ),
                (
                    2,
                    independent::ScalarCell::I64(i64::from(row).wrapping_sub(5)),
                ),
                (3, independent::ScalarCell::F64Bits(float_bits)),
                (4, independent::ScalarCell::Bool(row.is_multiple_of(2))),
                (5, independent::ScalarCell::Utf8(dictionary)),
                (6, independent::ScalarCell::Utf8(raw)),
                (
                    7,
                    independent::ScalarCell::U64(
                        seed.rotate_right(row).wrapping_add(u64::from(row)),
                    ),
                ),
                (8, independent::ScalarCell::I64(timestamp.wrapping_neg())),
                (9, independent::ScalarCell::F64Bits(float_bits)),
                (10, independent::ScalarCell::Bool(!row.is_multiple_of(2))),
                (
                    11,
                    independent::ScalarCell::Utf8(
                        format!("required-dict-{}", row % 3).into_bytes(),
                    ),
                ),
                (
                    12,
                    independent::ScalarCell::Utf8(
                        format!("required-raw-{seed:016x}-{row}").into_bytes(),
                    ),
                ),
            ]);
            if row == 9 {
                for column in 1..=6 {
                    cells.insert(column, independent::ScalarCell::Null);
                }
            }
            if cycle == 1 && row == 0 {
                for column in 1..=5 {
                    cells.insert(column, independent::ScalarCell::Null);
                }
            }
            if cycle == 2 && row == 5 {
                for column in 1..=6 {
                    cells.insert(column, independent::ScalarCell::Null);
                }
            }
            if cycle == 4 {
                cells.insert(5, independent::ScalarCell::Null);
            }
            if cycle == 5 {
                cells.insert(6, independent::ScalarCell::Null);
            }
            cells
        })
        .collect()
}

fn input_document(
    seed: u64,
    row: u32,
    cells: &BTreeMap<u32, independent::ScalarCell>,
) -> Result<IngestDocument, String> {
    let document_id = (u128::from(seed) << 32)
        .checked_add(u128::from(row))
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| "metadata document id overflow".to_owned())?;
    let mut document = IngestDocument::new(
        DocumentVersion::new(DocId::new(document_id), Revision::new(1)),
        vec![row as f32, -(row as f32)],
    );
    let timestamp = match cells.get(&0) {
        Some(independent::ScalarCell::I64(value)) => *value,
        other => return Err(format!("metadata timestamp is not I64: {other:?}")),
    };
    document = document.with_timestamp(timestamp);
    let mut columns = Vec::new();
    for (column, cell) in cells {
        if *column == 0 || *cell == independent::ScalarCell::Null {
            continue;
        }
        let value = match cell {
            independent::ScalarCell::U64(value) => PredicateValue::U64(*value),
            independent::ScalarCell::I64(value) => PredicateValue::I64(*value),
            independent::ScalarCell::F64Bits(value) => PredicateValue::F64(f64::from_bits(*value)),
            independent::ScalarCell::Bool(value) => PredicateValue::Bool(*value),
            independent::ScalarCell::Utf8(value) => PredicateValue::String(
                String::from_utf8(value.clone())
                    .map_err(|error| format!("metadata fixture UTF-8: {error}"))?,
            ),
            independent::ScalarCell::Null => continue,
        };
        columns.push((ColumnId::new(*column), value));
    }
    Ok(document.with_columns(columns))
}

fn column_cell(column: &Column, row: u32) -> independent::ScalarCell {
    match column {
        Column::U64(column) => column
            .get(row)
            .map_or(independent::ScalarCell::Null, independent::ScalarCell::U64),
        Column::I64(column) => column
            .get(row)
            .map_or(independent::ScalarCell::Null, independent::ScalarCell::I64),
        Column::F64(column) => column
            .get(row)
            .map_or(independent::ScalarCell::Null, |value| {
                independent::ScalarCell::F64Bits(value.to_bits())
            }),
        Column::Bool(column) => column
            .get(row)
            .map_or(independent::ScalarCell::Null, independent::ScalarCell::Bool),
        Column::DictionaryString(column) => column
            .get(row)
            .map_or(independent::ScalarCell::Null, |value| {
                independent::ScalarCell::Utf8(value.as_bytes().to_vec())
            }),
        Column::RawString(column) => column
            .get(row)
            .map_or(independent::ScalarCell::Null, |value| {
                independent::ScalarCell::Utf8(value.as_bytes().to_vec())
            }),
    }
}

fn predicate_cell(value: Option<PredicateValue>) -> independent::ScalarCell {
    match value {
        None => independent::ScalarCell::Null,
        Some(PredicateValue::U64(value)) => independent::ScalarCell::U64(value),
        Some(PredicateValue::I64(value)) => independent::ScalarCell::I64(value),
        Some(PredicateValue::F64(value)) => independent::ScalarCell::F64Bits(value.to_bits()),
        Some(PredicateValue::Bool(value)) => independent::ScalarCell::Bool(value),
        Some(PredicateValue::String(value)) => independent::ScalarCell::Utf8(value.into_bytes()),
    }
}

fn column_rows(
    columns: &ColumnStore,
) -> Result<Vec<BTreeMap<u32, independent::ScalarCell>>, String> {
    (0..columns.row_count())
        .map(|row| {
            columns
                .schema()
                .columns()
                .iter()
                .map(|definition| {
                    let column = columns.column(definition.id()).ok_or_else(|| {
                        format!(
                            "metadata decoded column {} is absent",
                            definition.id().get()
                        )
                    })?;
                    Ok((definition.id().get(), column_cell(column, row)))
                })
                .collect::<Result<BTreeMap<_, _>, String>>()
        })
        .collect()
}

fn public_metadata_rows(
    store: &Store,
    outcome: &FilteredSearchOutcome,
    row_count: usize,
) -> Result<Vec<BTreeMap<u32, independent::ScalarCell>>, String> {
    let mut public_rows = vec![BTreeMap::new(); row_count];
    for candidate in &outcome.candidates {
        let row_id = candidate.row_id();
        let row = usize::try_from(row_id.local_row())
            .map_err(|_| "metadata public row exceeds usize".to_owned())?;
        let output = public_rows
            .get_mut(row)
            .ok_or_else(|| format!("metadata public row {row} is outside fixture"))?;
        for (column, value) in store
            .test_metadata_row_values(row_id)
            .map_err(|error| format!("read public metadata row: {error}"))?
        {
            output.insert(column.get(), predicate_cell(value));
        }
    }
    Ok(public_rows)
}

fn build_typed_fixture(
    seed: u64,
    operation: MetadataOperationKind,
) -> Result<TypedFixture, String> {
    let directory = tempdir().map_err(|error| format!("metadata tempdir: {error}"))?;
    let schema = typed_schema();
    let rows = typed_rows(seed);
    let controller = Arc::new(MetadataTestController::new());
    let store = open_with_controller(
        directory.path(),
        OpenOptions::default().with_schema(schema.clone()),
        &controller,
    )?;
    let documents = rows
        .iter()
        .enumerate()
        .map(|(row, cells)| {
            input_document(
                seed,
                u32::try_from(row).map_err(|_| "metadata row exceeds u32".to_owned())?,
                cells,
            )
        })
        .collect::<Result<Vec<_>, String>>()?;
    store
        .ingest(IngestBatch::new(documents))
        .map_err(|error| format!("ingest metadata fixture: {error}"))?;
    let query_id = query_id_base(seed, operation);
    let active_rows = if operation == MetadataOperationKind::Columns {
        controller
            .arm(MetadataTestArm::ObserveExecution {
                query_id: query_id.wrapping_add(0x100),
            })
            .map_err(|error| format!("arm metadata active observation: {error}"))?;
        let active_outcome = store
            .search_filtered(
                SearchRequest::new(&[0.0, 0.0]),
                &Predicate::And(Vec::new()),
                rows.len(),
                SearchOptions::default().with_tier(SearchTier::Exact),
                QueryControl::Cancel(CancelToken::new()),
            )
            .map_err(|error| format!("active metadata public query: {error}"))?;
        public_metadata_rows(&store, &active_outcome, rows.len())?
    } else {
        Vec::new()
    };
    store
        .seal()
        .map_err(|error| format!("seal metadata fixture: {error}"))?;
    let snapshot = store
        .snapshot()
        .map_err(|error| format!("snapshot metadata fixture: {error}"))?;
    let segment = snapshot
        .segments()
        .first()
        .ok_or_else(|| "metadata fixture published no segment".to_owned())?;
    let segment_id = segment.meta().id;
    drop(snapshot);
    store
        .close()
        .map_err(|error| format!("close sealed metadata fixture: {error}"))?;

    let segment_path = directory.path().join(segment_id.file_name());
    let clean_segment = std::fs::read(&segment_path)
        .map_err(|error| format!("read closed metadata segment: {error}"))?;
    let (columns_offset, columns_length) = region_bounds(&clean_segment, 0)?;
    let columns_region = clean_segment
        .get(columns_offset..columns_offset.saturating_add(columns_length))
        .ok_or_else(|| "independent Columns envelope span escapes segment".to_owned())?;
    let parsed_columns = independent::parse_columns(columns_region)?;
    let reader = SegmentReader::open(&StdVfs, &segment_path, segment_id)
        .map_err(|error| format!("open closed metadata SegmentReader: {error}"))?;
    let decoded_columns = reader
        .columns()
        .map_err(|error| format!("decode closed metadata Columns region: {error}"))?;
    let reader_rows = column_rows(&decoded_columns)?;

    controller
        .arm(MetadataTestArm::ObserveExecution { query_id })
        .map_err(|error| format!("arm metadata clean observation: {error}"))?;
    let clean_predicate = operation_predicate(seed, operation);
    let reopened = open_with_controller(
        directory.path(),
        OpenOptions::default().with_schema(schema.clone()),
        &controller,
    )?;
    let clean_outcome = reopened
        .search_filtered(
            SearchRequest::new(&[0.0, 0.0]),
            &clean_predicate,
            rows.len(),
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| format!("clean metadata public query: {error}"))?;
    let clean_generation = clean_outcome.generation;
    let public_rows = public_metadata_rows(&reopened, &clean_outcome, rows.len())?;
    let clean_wal_digest = wal_digest(directory.path())?;
    reopened
        .close()
        .map_err(|error| format!("close reopened metadata fixture: {error}"))?;
    Ok(TypedFixture {
        directory,
        schema,
        controller,
        segment_id,
        segment_path,
        clean_segment,
        parsed_columns,
        input: independent::I36Input {
            source: source_label(RowSource::Sealed(segment_id)),
            definitions: typed_definitions(),
            rows,
        },
        active_rows,
        reader_rows,
        public_rows,
        clean_outcome,
        clean_generation,
        clean_wal_digest,
    })
}

pub const I37_PREDICATE_CASE_COUNT: u64 = 45;

const I37_U64_RANGE_KEYS: [&str; 9] = [
    "range-u64-lower-unbounded-upper-unbounded",
    "range-u64-lower-inclusive-upper-unbounded",
    "range-u64-lower-exclusive-upper-unbounded",
    "range-u64-lower-unbounded-upper-inclusive",
    "range-u64-lower-unbounded-upper-exclusive",
    "range-u64-lower-inclusive-upper-inclusive",
    "range-u64-lower-inclusive-upper-exclusive",
    "range-u64-lower-exclusive-upper-inclusive",
    "range-u64-lower-exclusive-upper-exclusive",
];

const I37_I64_RANGE_KEYS: [&str; 9] = [
    "range-i64-lower-unbounded-upper-unbounded",
    "range-i64-lower-inclusive-upper-unbounded",
    "range-i64-lower-exclusive-upper-unbounded",
    "range-i64-lower-unbounded-upper-inclusive",
    "range-i64-lower-unbounded-upper-exclusive",
    "range-i64-lower-inclusive-upper-inclusive",
    "range-i64-lower-inclusive-upper-exclusive",
    "range-i64-lower-exclusive-upper-inclusive",
    "range-i64-lower-exclusive-upper-exclusive",
];

const I37_F64_RANGE_KEYS: [&str; 9] = [
    "range-f64-lower-unbounded-upper-unbounded",
    "range-f64-lower-inclusive-upper-unbounded",
    "range-f64-lower-exclusive-upper-unbounded",
    "range-f64-lower-unbounded-upper-inclusive",
    "range-f64-lower-unbounded-upper-exclusive",
    "range-f64-lower-inclusive-upper-inclusive",
    "range-f64-lower-inclusive-upper-exclusive",
    "range-f64-lower-exclusive-upper-inclusive",
    "range-f64-lower-exclusive-upper-exclusive",
];

/// Stable family-owned coverage key for the seed-selected I37 matrix cell.
pub fn i37_predicate_case_key(seed: u64) -> &'static str {
    match seed % I37_PREDICATE_CASE_COUNT {
        0 => "eq-u64",
        1 => "eq-i64",
        2 => "eq-f64",
        3 => "eq-bool",
        4 => "eq-dictionary-string",
        5 => "eq-raw-string",
        6 => "in-empty",
        7 => "in-single",
        8 => "in-duplicate",
        case @ 9..=17 => I37_U64_RANGE_KEYS[(case - 9) as usize],
        case @ 18..=26 => I37_I64_RANGE_KEYS[(case - 18) as usize],
        case @ 27..=35 => I37_F64_RANGE_KEYS[(case - 27) as usize],
        36 => "range-f64-nan-lower",
        37 => "range-f64-nan-upper",
        38 => "exists",
        39 => "is-null",
        40 => "and-empty",
        41 => "or-empty",
        42 => "and-nested-or",
        43 => "not-double",
        44 => "not-live-scope",
        _ => unreachable!("I37 coverage case is reduced modulo the matrix size"),
    }
}

/// Stable per-row identity of one I37 oracle record: the operation index,
/// the seed-selected matrix cell, and the adapter fault that ran (or `none`).
pub fn i37_case_identity(op_index: usize, seed: u64, fault: Option<&str>) -> String {
    format!(
        "op-{op_index}-{}-fault-{}",
        i37_predicate_case_key(seed),
        fault.unwrap_or("none")
    )
}

/// Recovers the bare matrix-cell key from an identity written by
/// [`i37_case_identity`]. Cell keys contain `-` but never `-fault-`, so the
/// right-most split is unambiguous.
pub fn i37_case_key_from_identity(identity: &str) -> Option<&'static str> {
    let rest = identity.strip_prefix("op-")?;
    let (_op_index, rest) = rest.split_once('-')?;
    let (key, _fault) = rest.rsplit_once("-fault-")?;
    (0..I37_PREDICATE_CASE_COUNT)
        .map(i37_predicate_case_key)
        .find(|candidate| *candidate == key)
}

fn production_range_bounds(
    shape: u64,
    lower: PredicateValue,
    upper: PredicateValue,
) -> (Option<RangeBound>, Option<RangeBound>) {
    let lower_inclusive = || Some(RangeBound::inclusive(lower.clone()));
    let lower_exclusive = || Some(RangeBound::exclusive(lower.clone()));
    let upper_inclusive = || Some(RangeBound::inclusive(upper.clone()));
    let upper_exclusive = || Some(RangeBound::exclusive(upper.clone()));
    match shape {
        0 => (None, None),
        1 => (lower_inclusive(), None),
        2 => (lower_exclusive(), None),
        3 => (None, upper_inclusive()),
        4 => (None, upper_exclusive()),
        5 => (lower_inclusive(), upper_inclusive()),
        6 => (lower_inclusive(), upper_exclusive()),
        7 => (lower_exclusive(), upper_inclusive()),
        8 => (lower_exclusive(), upper_exclusive()),
        _ => unreachable!("I37 production Range shape is reduced modulo nine"),
    }
}

fn oracle_range_bounds(
    shape: u64,
    lower: independent::ScalarCell,
    upper: independent::ScalarCell,
) -> (
    Option<independent::RangeBoundDto>,
    Option<independent::RangeBoundDto>,
) {
    let lower_inclusive = || {
        Some(independent::RangeBoundDto {
            value: lower.clone(),
            inclusive: true,
        })
    };
    let lower_exclusive = || {
        Some(independent::RangeBoundDto {
            value: lower.clone(),
            inclusive: false,
        })
    };
    let upper_inclusive = || {
        Some(independent::RangeBoundDto {
            value: upper.clone(),
            inclusive: true,
        })
    };
    let upper_exclusive = || {
        Some(independent::RangeBoundDto {
            value: upper.clone(),
            inclusive: false,
        })
    };
    match shape {
        0 => (None, None),
        1 => (lower_inclusive(), None),
        2 => (lower_exclusive(), None),
        3 => (None, upper_inclusive()),
        4 => (None, upper_exclusive()),
        5 => (lower_inclusive(), upper_inclusive()),
        6 => (lower_inclusive(), upper_exclusive()),
        7 => (lower_exclusive(), upper_inclusive()),
        8 => (lower_exclusive(), upper_exclusive()),
        _ => unreachable!("I37 oracle Range shape is reduced modulo nine"),
    }
}

fn operation_predicate(seed: u64, operation: MetadataOperationKind) -> Predicate {
    if operation != MetadataOperationKind::Bitmap {
        return Predicate::And(Vec::new());
    }
    let bool_eq = |value| Predicate::Eq {
        column: ColumnId::new(4),
        value: PredicateValue::Bool(value),
    };
    let case = seed % I37_PREDICATE_CASE_COUNT;
    match case {
        0 => Predicate::Eq {
            column: ColumnId::new(1),
            value: PredicateValue::U64(seed),
        },
        1 => Predicate::Eq {
            column: ColumnId::new(2),
            value: PredicateValue::I64(-5),
        },
        2 => Predicate::Eq {
            column: ColumnId::new(3),
            value: PredicateValue::F64(-0.0),
        },
        3 => bool_eq(true),
        4 => Predicate::Eq {
            column: ColumnId::new(5),
            value: PredicateValue::String("one".to_owned()),
        },
        5 => Predicate::Eq {
            column: ColumnId::new(6),
            value: PredicateValue::String("repeated-raw".to_owned()),
        },
        6 => Predicate::In {
            column: ColumnId::new(6),
            values: Vec::new(),
        },
        7 => Predicate::In {
            column: ColumnId::new(6),
            values: vec![PredicateValue::String("repeated-raw".to_owned())],
        },
        8 => Predicate::In {
            column: ColumnId::new(6),
            values: vec![
                PredicateValue::String("repeated-raw".to_owned()),
                PredicateValue::String("repeated-raw".to_owned()),
            ],
        },
        9..=35 => {
            let (column, lower_value, upper_value, shape) = match case {
                9..=17 => (
                    ColumnId::new(1),
                    PredicateValue::U64(0),
                    PredicateValue::U64(u64::MAX),
                    case - 9,
                ),
                18..=26 => (
                    ColumnId::new(2),
                    PredicateValue::I64(-5),
                    PredicateValue::I64(4),
                    case - 18,
                ),
                _ => (
                    ColumnId::new(3),
                    PredicateValue::F64(-0.0),
                    PredicateValue::F64(6.0),
                    case - 27,
                ),
            };
            let (lower, upper) = production_range_bounds(shape, lower_value, upper_value);
            Predicate::Range(RangePredicate {
                column,
                lower,
                upper,
            })
        }
        36 => Predicate::Range(RangePredicate {
            column: ColumnId::new(3),
            lower: Some(RangeBound::inclusive(PredicateValue::F64(f64::from_bits(
                0x7ff8_0000_0000_00a5,
            )))),
            upper: None,
        }),
        37 => Predicate::Range(RangePredicate {
            column: ColumnId::new(3),
            lower: None,
            upper: Some(RangeBound::exclusive(PredicateValue::F64(f64::from_bits(
                0x7ff8_0000_0000_00a5,
            )))),
        }),
        38 => Predicate::Exists(ColumnId::new(6)),
        39 => Predicate::IsNull(ColumnId::new(6)),
        40 => Predicate::And(Vec::new()),
        41 => Predicate::Or(Vec::new()),
        42 => Predicate::And(vec![
            Predicate::Or(vec![bool_eq(true), Predicate::IsNull(ColumnId::new(4))]),
            Predicate::Exists(ColumnId::new(5)),
        ]),
        43 => Predicate::Not(Box::new(Predicate::Not(Box::new(bool_eq(true))))),
        44 => Predicate::Not(Box::new(Predicate::IsNull(ColumnId::new(6)))),
        _ => unreachable!("I37 predicate case is reduced modulo the matrix size"),
    }
}

fn operation_predicate_dto(
    seed: u64,
    operation: MetadataOperationKind,
) -> independent::PredicateDto {
    if operation != MetadataOperationKind::Bitmap {
        return independent::PredicateDto::And(Vec::new());
    }
    let bool_eq = |value| independent::PredicateDto::Eq {
        column: 4,
        value: independent::ScalarCell::Bool(value),
    };
    let case = seed % I37_PREDICATE_CASE_COUNT;
    match case {
        0 => independent::PredicateDto::Eq {
            column: 1,
            value: independent::ScalarCell::U64(seed),
        },
        1 => independent::PredicateDto::Eq {
            column: 2,
            value: independent::ScalarCell::I64(-5),
        },
        2 => independent::PredicateDto::Eq {
            column: 3,
            value: independent::ScalarCell::F64Bits((-0.0_f64).to_bits()),
        },
        3 => bool_eq(true),
        4 => independent::PredicateDto::Eq {
            column: 5,
            value: independent::ScalarCell::Utf8(b"one".to_vec()),
        },
        5 => independent::PredicateDto::Eq {
            column: 6,
            value: independent::ScalarCell::Utf8(b"repeated-raw".to_vec()),
        },
        6 => independent::PredicateDto::In {
            column: 6,
            values: Vec::new(),
        },
        7 => independent::PredicateDto::In {
            column: 6,
            values: vec![independent::ScalarCell::Utf8(b"repeated-raw".to_vec())],
        },
        8 => independent::PredicateDto::In {
            column: 6,
            values: vec![
                independent::ScalarCell::Utf8(b"repeated-raw".to_vec()),
                independent::ScalarCell::Utf8(b"repeated-raw".to_vec()),
            ],
        },
        9..=35 => {
            let (column, lower_value, upper_value, shape) = match case {
                9..=17 => (
                    1,
                    independent::ScalarCell::U64(0),
                    independent::ScalarCell::U64(u64::MAX),
                    case - 9,
                ),
                18..=26 => (
                    2,
                    independent::ScalarCell::I64(-5),
                    independent::ScalarCell::I64(4),
                    case - 18,
                ),
                _ => (
                    3,
                    independent::ScalarCell::F64Bits((-0.0_f64).to_bits()),
                    independent::ScalarCell::F64Bits(6.0_f64.to_bits()),
                    case - 27,
                ),
            };
            let (lower, upper) = oracle_range_bounds(shape, lower_value, upper_value);
            independent::PredicateDto::Range {
                column,
                lower,
                upper,
            }
        }
        36 => independent::PredicateDto::Range {
            column: 3,
            lower: Some(independent::RangeBoundDto {
                value: independent::ScalarCell::F64Bits(0x7ff8_0000_0000_00a5),
                inclusive: true,
            }),
            upper: None,
        },
        37 => independent::PredicateDto::Range {
            column: 3,
            lower: None,
            upper: Some(independent::RangeBoundDto {
                value: independent::ScalarCell::F64Bits(0x7ff8_0000_0000_00a5),
                inclusive: false,
            }),
        },
        38 => independent::PredicateDto::Exists(6),
        39 => independent::PredicateDto::IsNull(6),
        40 => independent::PredicateDto::And(Vec::new()),
        41 => independent::PredicateDto::Or(Vec::new()),
        42 => independent::PredicateDto::And(vec![
            independent::PredicateDto::Or(vec![
                bool_eq(true),
                independent::PredicateDto::IsNull(4),
            ]),
            independent::PredicateDto::Exists(5),
        ]),
        43 => independent::PredicateDto::Not(Box::new(independent::PredicateDto::Not(Box::new(
            bool_eq(true),
        )))),
        44 => independent::PredicateDto::Not(Box::new(independent::PredicateDto::IsNull(6))),
        _ => unreachable!("I37 oracle predicate case is reduced modulo the matrix size"),
    }
}

struct I37LifecycleEvidence {
    inputs: Vec<independent::I37SourceInputDto>,
    observed: Vec<independent::I37SourceObservedDto>,
    receipts: Vec<MetadataExecutionReceipt>,
    query: MetadataQueryEvidence,
}

fn observe_i37_lifecycle_sources(seed: u64) -> Result<I37LifecycleEvidence, String> {
    let fixture_seed = seed ^ 0x37c0_11fe;
    let primitive_rows = typed_rows(fixture_seed);
    let directory = tempdir().map_err(|error| format!("metadata I37 tempdir: {error}"))?;
    let schema = typed_schema();
    let controller = Arc::new(MetadataTestController::new());
    let store = open_with_controller(
        directory.path(),
        OpenOptions::default().with_schema(schema),
        &controller,
    )?;
    let sealed_count = 6_usize;
    let sealed_documents = primitive_rows
        .iter()
        .take(sealed_count)
        .enumerate()
        .map(|(row, cells)| {
            input_document(
                fixture_seed,
                u32::try_from(row).map_err(|_| "metadata I37 sealed row exceeds u32".to_owned())?,
                cells,
            )
        })
        .collect::<Result<Vec<_>, String>>()?;
    store
        .ingest(IngestBatch::new(sealed_documents))
        .map_err(|error| format!("ingest metadata I37 sealed rows: {error}"))?;
    store
        .seal()
        .map_err(|error| format!("seal metadata I37 source: {error}"))?;
    let deleted_row = 2_u32;
    let deleted_id = (u128::from(fixture_seed) << 32)
        .checked_add(u128::from(deleted_row) + 1)
        .map(DocId::new)
        .ok_or_else(|| "metadata I37 delete document id overflow".to_owned())?;
    store
        .delete(DeleteBatch::new(vec![deleted_id]))
        .map_err(|error| format!("publicly tombstone metadata I37 sealed row: {error}"))?;
    let active_documents = primitive_rows
        .iter()
        .enumerate()
        .skip(sealed_count)
        .map(|(row, cells)| {
            input_document(
                fixture_seed,
                u32::try_from(row).map_err(|_| "metadata I37 active row exceeds u32".to_owned())?,
                cells,
            )
        })
        .collect::<Result<Vec<_>, String>>()?;
    store
        .ingest(IngestBatch::new(active_documents))
        .map_err(|error| format!("ingest metadata I37 active rows: {error}"))?;

    let snapshot = store
        .snapshot()
        .map_err(|error| format!("snapshot metadata I37 sources: {error}"))?;
    let sealed_id = snapshot
        .segments()
        .first()
        .map(|segment| segment.meta().id)
        .ok_or_else(|| "metadata I37 snapshot lacks sealed source".to_owned())?;
    drop(snapshot);
    let predicate = operation_predicate(seed, MetadataOperationKind::Bitmap);
    let predicate_dto = operation_predicate_dto(seed, MetadataOperationKind::Bitmap);
    let query_id = query_id_base(seed, MetadataOperationKind::Bitmap).wrapping_add(0x200);
    controller
        .arm(MetadataTestArm::ObserveExecution { query_id })
        .map_err(|error| format!("arm metadata I37 lifecycle query: {error}"))?;
    let outcome = store
        .search_filtered(
            SearchRequest::new(&[0.0, 0.0]),
            &predicate,
            primitive_rows.len(),
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| format!("metadata I37 lifecycle public query: {error}"))?;
    let receipts = controller
        .drain_execution_receipts()
        .map_err(|error| format!("drain metadata I37 lifecycle receipts: {error}"))?;
    controller
        .assert_no_unconsumed_arm()
        .map_err(|error| format!("metadata I37 lifecycle arm leak: {error}"))?;

    let sources = [RowSource::Sealed(sealed_id), RowSource::Active];
    let inputs = sources
        .iter()
        .copied()
        .map(|source| {
            let (cells, live) = match source {
                RowSource::Sealed(_) => (
                    primitive_rows[..sealed_count].to_vec(),
                    (0..u32::try_from(sealed_count)
                        .map_err(|_| "metadata I37 sealed count exceeds u32".to_owned())?)
                        .filter(|row| *row != deleted_row)
                        .collect::<BTreeSet<_>>(),
                ),
                RowSource::Active => (
                    primitive_rows[sealed_count..].to_vec(),
                    (0..u32::try_from(primitive_rows.len().saturating_sub(sealed_count))
                        .map_err(|_| "metadata I37 active count exceeds u32".to_owned())?)
                        .collect::<BTreeSet<_>>(),
                ),
            };
            Ok(independent::I37SourceInputDto {
                source: source_label(source),
                sealed: matches!(source, RowSource::Sealed(_)),
                rows: cells
                    .into_iter()
                    .enumerate()
                    .map(|(row, cells)| independent::MetadataRowDto {
                        row_id: u32::try_from(row).unwrap_or(u32::MAX),
                        cells,
                    })
                    .collect(),
                live,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let observed = sources
        .iter()
        .copied()
        .map(|source| {
            let source_name = source_label(source);
            let (row_count, live, evaluator) = store
                .test_metadata_evaluate_source(source, &predicate)
                .map_err(|error| format!("evaluate metadata I37 source {source_name}: {error}"))?;
            let public_plan = outcome
                .plans
                .iter()
                .find(|plan| plan.source == source)
                .ok_or_else(|| format!("metadata I37 source {source_name} lacks public report"))?;
            let receipt = receipts
                .iter()
                .find(|receipt| receipt.source == source)
                .ok_or_else(|| {
                    format!("metadata I37 source {source_name} lacks production receipt")
                })?;
            Ok(independent::I37SourceObservedDto {
                source: source_name,
                sealed: matches!(source, RowSource::Sealed(_)),
                row_count,
                live: live.into_iter().collect(),
                evaluator: evaluator.into_iter().collect(),
                public_results: outcome
                    .candidates
                    .iter()
                    .filter(|candidate| candidate.row_id().source() == source)
                    .map(|candidate| candidate.row_id().local_row())
                    .collect(),
                report: report(query_id, public_plan),
                receipt: execution_receipt(receipt),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    store
        .close()
        .map_err(|error| format!("close metadata I37 lifecycle Store: {error}"))?;
    Ok(I37LifecycleEvidence {
        inputs,
        observed,
        receipts,
        query: query_evidence(
            query_id,
            "active-sealed-tombstoned",
            predicate_dto,
            2,
            u64::try_from(primitive_rows.len())
                .map_err(|_| "metadata I37 k exceeds u64".to_owned())?,
            "exact",
            sources.into_iter().map(source_label).collect(),
        ),
    })
}

fn planner_predicate(seed: u64) -> (Predicate, independent::PredicateDto) {
    match seed % 3 {
        0 => (
            Predicate::Eq {
                column: TIMESTAMP_COLUMN,
                value: PredicateValue::I64(100),
            },
            independent::PredicateDto::Eq {
                column: 0,
                value: independent::ScalarCell::I64(100),
            },
        ),
        1 => (
            Predicate::Range(RangePredicate {
                column: TIMESTAMP_COLUMN,
                lower: Some(RangeBound::inclusive(PredicateValue::I64(100))),
                upper: Some(RangeBound::inclusive(PredicateValue::I64(100))),
            }),
            independent::PredicateDto::Range {
                column: 0,
                lower: Some(independent::RangeBoundDto {
                    value: independent::ScalarCell::I64(100),
                    inclusive: true,
                }),
                upper: Some(independent::RangeBoundDto {
                    value: independent::ScalarCell::I64(100),
                    inclusive: true,
                }),
            },
        ),
        _ => (
            Predicate::Range(RangePredicate {
                column: TIMESTAMP_COLUMN,
                lower: Some(RangeBound::exclusive(PredicateValue::I64(100))),
                upper: Some(RangeBound::inclusive(PredicateValue::I64(101))),
            }),
            independent::PredicateDto::Range {
                column: 0,
                lower: Some(independent::RangeBoundDto {
                    value: independent::ScalarCell::I64(100),
                    inclusive: false,
                }),
                upper: Some(independent::RangeBoundDto {
                    value: independent::ScalarCell::I64(101),
                    inclusive: true,
                }),
            },
        ),
    }
}

fn exact_query(
    store: &Store,
    seed: u64,
    operation: MetadataOperationKind,
    k: usize,
) -> Result<FilteredSearchOutcome, FilteredSearchError> {
    store.search_filtered(
        SearchRequest::new(&[0.0, 0.0]),
        &operation_predicate(seed, operation),
        k,
        SearchOptions::default().with_tier(SearchTier::Exact),
        QueryControl::Cancel(CancelToken::new()),
    )
}

fn query_evidence(
    query_id: u64,
    phase: &'static str,
    predicate: independent::PredicateDto,
    dimensions: usize,
    k: u64,
    tier: &'static str,
    expected_sources: BTreeSet<String>,
) -> MetadataQueryEvidence {
    MetadataQueryEvidence {
        query_id,
        phase,
        predicate,
        query_vector_bits: vec![0.0_f32.to_bits(); dimensions],
        k,
        tier,
        expected_sources,
    }
}

struct ColumnPlant {
    arm: MetadataTestArm,
    provenance: MetadataDecodeProvenance,
    mutation: MetadataMutationEvidence,
    bytes: Vec<u8>,
}

fn column_plant(fixture: &TypedFixture, seed: u64, query_id: u64) -> Result<ColumnPlant, String> {
    let (region_offset, region_length) = region_bounds(&fixture.clean_segment, 0)?;
    let mut bytes = fixture.clean_segment.clone();
    let source = RowSource::Sealed(fixture.segment_id);
    let (field_class, span, replacement, provenance) = match seed % 3 {
        0 => {
            let cell = fixture
                .parsed_columns
                .cells
                .get(&(0, 5))
                .ok_or_else(|| "metadata dictionary code span is absent".to_owned())?;
            let cardinality = fixture
                .parsed_columns
                .dictionary_spans
                .get(&5)
                .ok_or_else(|| "metadata dictionary spans are absent".to_owned())?
                .entries
                .len();
            let code = u32::try_from(cardinality)
                .map_err(|_| "metadata dictionary cardinality exceeds u32".to_owned())?;
            (
                "dictionary-code",
                cell.payload_span,
                u16::try_from(code)
                    .map_err(|_| "metadata dictionary code exceeds u16".to_owned())?
                    .to_le_bytes()
                    .to_vec(),
                MetadataDecodeProvenance::ColumnsDictionaryCode {
                    column_id: 5,
                    row: 0,
                    byte_offset: u64::try_from(cell.payload_span.start)
                        .map_err(|_| "metadata dictionary offset exceeds u64".to_owned())?,
                    code,
                    dictionary_cardinality: code,
                },
            )
        }
        1 => {
            let cell = fixture
                .parsed_columns
                .cells
                .get(&(0, 6))
                .ok_or_else(|| "metadata raw-string span is absent".to_owned())?;
            let span = cell
                .length_span
                .ok_or_else(|| "metadata raw-string length span is absent".to_owned())?;
            let available_bytes = u64::try_from(region_length.saturating_sub(span.end))
                .map_err(|_| "metadata raw available bytes exceed u64".to_owned())?;
            (
                "raw-string-length",
                span,
                u32::MAX.to_le_bytes().to_vec(),
                MetadataDecodeProvenance::ColumnsRawStringLength {
                    column_id: 6,
                    row: 0,
                    byte_offset: u64::try_from(span.start)
                        .map_err(|_| "metadata raw offset exceeds u64".to_owned())?,
                    declared_bytes: u32::MAX,
                    available_bytes,
                },
            )
        }
        _ => {
            let presence = fixture
                .parsed_columns
                .presence_spans
                .get(&1)
                .ok_or_else(|| "metadata presence span is absent".to_owned())?
                .bitmap;
            let position = presence
                .end
                .checked_sub(1)
                .ok_or_else(|| "metadata presence span is empty".to_owned())?;
            let old = *bytes
                .get(region_offset + position)
                .ok_or_else(|| "metadata presence byte escaped segment".to_owned())?;
            let observed = old | 0x80;
            (
                "presence-tail",
                independent::ByteSpan {
                    start: position,
                    end: position + 1,
                },
                vec![observed],
                MetadataDecodeProvenance::ColumnsPresenceTail {
                    column_id: 1,
                    row_count: 10,
                    byte_offset: u64::try_from(position)
                        .map_err(|_| "metadata presence offset exceeds u64".to_owned())?,
                    observed_byte: observed,
                    allowed_mask: 0x03,
                },
            )
        }
    };
    let absolute_start = region_offset
        .checked_add(span.start)
        .ok_or_else(|| "metadata mutation offset overflow".to_owned())?;
    let absolute_end = region_offset
        .checked_add(span.end)
        .ok_or_else(|| "metadata mutation offset overflow".to_owned())?;
    let before = bytes
        .get(absolute_start..absolute_end)
        .ok_or_else(|| "metadata mutation span escaped segment".to_owned())?
        .to_vec();
    if before.len() != replacement.len() {
        return Err("metadata mutation changed persisted field width".to_owned());
    }
    let left_neighbor_before = absolute_start
        .checked_sub(1)
        .and_then(|position| bytes.get(position).copied());
    let right_neighbor_before = bytes.get(absolute_end).copied();
    bytes
        .get_mut(absolute_start..absolute_end)
        .ok_or_else(|| "metadata mutation span escaped segment".to_owned())?
        .copy_from_slice(&replacement);
    let checksum_rewrites = rewrite_region(&mut bytes, 0)?;
    let byte_offset = match provenance {
        MetadataDecodeProvenance::ColumnsPresenceTail { byte_offset, .. }
        | MetadataDecodeProvenance::ColumnsDictionaryCode { byte_offset, .. }
        | MetadataDecodeProvenance::ColumnsRawStringLength { byte_offset, .. } => byte_offset,
        MetadataDecodeProvenance::AliveBitmapTruncation { .. } => {
            return Err("Alive provenance selected for Columns fault".to_owned());
        }
    };
    let mutation = MetadataMutationEvidence {
        source: source_label(source),
        region: RegionKind::Columns,
        region_offset: u64::try_from(region_offset)
            .map_err(|_| "metadata region offset exceeds u64".to_owned())?,
        field_offset: byte_offset,
        absolute_offset: u64::try_from(absolute_start)
            .map_err(|_| "metadata absolute offset exceeds u64".to_owned())?,
        before,
        after: replacement,
        left_neighbor_before,
        left_neighbor_after: absolute_start
            .checked_sub(1)
            .and_then(|position| bytes.get(position).copied()),
        right_neighbor_before,
        right_neighbor_after: bytes.get(absolute_end).copied(),
        declared_bytes_before: u64::try_from(region_length)
            .map_err(|_| "metadata Columns length exceeds u64".to_owned())?,
        declared_bytes_after: u64::try_from(region_length)
            .map_err(|_| "metadata Columns length exceeds u64".to_owned())?,
        observed_bytes_after: u64::try_from(region_length)
            .map_err(|_| "metadata Columns length exceeds u64".to_owned())?,
        checksum_rewrites,
        post_mutation_artifact_digest: digest(&bytes),
    };
    Ok(ColumnPlant {
        arm: MetadataTestArm::ColumnCorruption {
            query_id,
            source,
            field_class,
            byte_offset,
        },
        provenance,
        mutation,
        bytes,
    })
}

fn require_metadata_provenance(
    error: &FilteredSearchError,
    expected: &MetadataDecodeProvenance,
) -> Result<(), String> {
    match error {
        FilteredSearchError::Query(QueryError::Store(StoreError::Segment(
            SegmentError::MetadataSemantic { provenance, .. },
        ))) if provenance == expected => Ok(()),
        other => Err(format!(
            "metadata fault returned wrong typed provenance expected={expected:?} observed={other:?}"
        )),
    }
}

fn expected_provenance(provenance: &MetadataDecodeProvenance) -> MetadataProvenanceExpected {
    match provenance {
        MetadataDecodeProvenance::ColumnsPresenceTail {
            column_id,
            row_count,
            byte_offset,
            observed_byte,
            allowed_mask,
        } => MetadataProvenanceExpected::ColumnsPresenceTail {
            column_id: *column_id,
            row_count: *row_count,
            byte_offset: *byte_offset,
            observed_byte: *observed_byte,
            allowed_mask: *allowed_mask,
        },
        MetadataDecodeProvenance::ColumnsDictionaryCode {
            column_id,
            row,
            byte_offset,
            code,
            dictionary_cardinality,
        } => MetadataProvenanceExpected::ColumnsDictionaryCode {
            column_id: *column_id,
            row: *row,
            byte_offset: *byte_offset,
            code: *code,
            dictionary_len: *dictionary_cardinality,
        },
        MetadataDecodeProvenance::ColumnsRawStringLength {
            column_id,
            row,
            byte_offset,
            declared_bytes,
            available_bytes,
        } => MetadataProvenanceExpected::ColumnsRawStringLength {
            column_id: *column_id,
            row: *row,
            byte_offset: *byte_offset,
            declared_bytes: *declared_bytes,
            available_bytes: *available_bytes,
        },
        MetadataDecodeProvenance::AliveBitmapTruncation {
            row_count,
            byte_offset,
            declared_bytes,
            observed_bytes,
        } => MetadataProvenanceExpected::AliveBitmapTruncation {
            row_count: *row_count,
            byte_offset: *byte_offset,
            declared_bytes: *declared_bytes,
            observed_bytes: *observed_bytes,
        },
    }
}

fn independent_error_class(provenance: &MetadataDecodeProvenance) -> String {
    match provenance {
        MetadataDecodeProvenance::ColumnsPresenceTail { .. } => {
            "non-zero presence tail padding".to_owned()
        }
        MetadataDecodeProvenance::ColumnsDictionaryCode { code, .. } => {
            format!("dictionary code {code} out of range")
        }
        MetadataDecodeProvenance::ColumnsRawStringLength {
            byte_offset,
            declared_bytes,
            available_bytes,
            ..
        } => {
            let payload_offset = byte_offset.saturating_add(4);
            let total = payload_offset.saturating_add(*available_bytes);
            format!("columns truncated at {payload_offset}, need {declared_bytes}, total {total}")
        }
        MetadataDecodeProvenance::AliveBitmapTruncation {
            byte_offset,
            declared_bytes,
            observed_bytes,
            ..
        } => {
            let total = byte_offset.saturating_add(u64::from(*observed_bytes));
            format!("alive truncated at {byte_offset}, need {declared_bytes}, total {total}")
        }
    }
}

fn run_columns(
    seed: u64,
    fault: Option<MetadataFaultKind>,
) -> Result<MetadataOperationEvidence, String> {
    let fixture = build_typed_fixture(seed, MetadataOperationKind::Columns)?;
    let query_id = query_id_base(seed, MetadataOperationKind::Columns);
    let clean_directory = copy_directory(fixture.directory.path())?;
    let fault_directory = copy_directory(fixture.directory.path())?;
    let clean_initial_directory = directory_digest(clean_directory.path())?;
    let fault_initial_directory = directory_digest(fault_directory.path())?;
    if clean_directory.path() == fault_directory.path() {
        return Err("metadata Columns clean/fault directories are not distinct".to_owned());
    }
    if clean_initial_directory != fault_initial_directory {
        return Err("metadata Columns clean/fault directory copies differ".to_owned());
    }
    let clean_controller = Arc::new(MetadataTestController::new());
    let clean_store = open_with_controller(
        clean_directory.path(),
        OpenOptions::default().with_schema(fixture.schema.clone()),
        &clean_controller,
    )?;
    let clean_outcome = exact_query(&clean_store, seed, MetadataOperationKind::Columns, 10)
        .map_err(|error| format!("metadata Columns same-seed clean query: {error}"))?;
    let clean_results = result_facts(&clean_outcome);
    let clean_generation = clean_outcome.generation;
    let clean_wal_digest = wal_digest(clean_directory.path())?;
    let clean_segment_path = clean_directory.path().join(fixture.segment_id.file_name());
    let clean_source_digest = digest(
        &std::fs::read(&clean_segment_path)
            .map_err(|error| format!("read metadata Columns clean source: {error}"))?,
    );
    clean_store
        .close()
        .map_err(|error| format!("close metadata Columns clean Store: {error}"))?;
    let mut fault_results = clean_results.clone();
    let mut retry_results = clean_results.clone();
    let mut fault_error = None;
    let mut fault_generation = clean_generation;
    let mut retry_generation = clean_generation;
    let mut fault_wal_digest = clean_wal_digest;
    let mut retry_wal_digest = clean_wal_digest;
    let mut fault_source_digest = clean_source_digest;
    let mut retry_source_digest = clean_source_digest;
    let mut mutation = None;
    let mut feature_expected = Vec::new();
    let mut retained_fault_leg = None;
    let mut retained_retry_leg = None;
    let fault_controller = Arc::new(MetadataTestController::new());
    let fault_segment_path = fault_directory.path().join(fixture.segment_id.file_name());

    if fault == Some(MetadataFaultKind::ColumnCorruption) {
        let plant = column_plant(&fixture, seed, query_id.wrapping_add(1))?;
        let (expected_query_id, expected_source, field_class, byte_offset) = match &plant.arm {
            MetadataTestArm::ColumnCorruption {
                query_id,
                source,
                field_class,
                byte_offset,
            } => (*query_id, *source, *field_class, *byte_offset),
            _ => return Err("metadata Columns plant armed another fault".to_owned()),
        };
        let error_class = independent_error_class(&plant.provenance);
        let effect = format!(
            "source={expected_source:?} field={field_class} offset={byte_offset} error={error_class}"
        );
        feature_expected.push(MetadataFeatureExpected::ColumnDecodeRefused {
            query_id: expected_query_id,
            source: source_label(expected_source),
            operation: "metadata_columns_roundtrip",
            fault: "column-corruption",
            site: "planner.exec.query_columns.refusal",
            cardinality: 1,
            field_class,
            byte_offset,
            error_class,
            effect,
            provenance: expected_provenance(&plant.provenance),
            expected_results: 0,
        });
        std::fs::write(&fault_segment_path, &plant.bytes)
            .map_err(|error| format!("write metadata Columns mutation: {error}"))?;
        fault_source_digest = digest(&plant.bytes);
        fault_controller
            .arm(plant.arm.clone())
            .map_err(|error| format!("arm metadata Columns fault: {error}"))?;
        let store = open_with_controller(
            fault_directory.path(),
            OpenOptions::default().with_schema(fixture.schema.clone()),
            &fault_controller,
        )?;
        fault_generation = store
            .snapshot()
            .map_err(|error| format!("snapshot metadata Columns fault: {error}"))?
            .generation();
        let error = exact_query(&store, seed, MetadataOperationKind::Columns, 10)
            .expect_err("metadata Columns fault must not return results");
        require_metadata_provenance(&error, &plant.provenance)?;
        let retained_fault_message = error.to_string();
        fault_error = Some(retained_fault_message.clone());
        fault_results.clear();
        fault_wal_digest = wal_digest(fault_directory.path())?;
        let after_fault = std::fs::read(&fault_segment_path)
            .map_err(|error| format!("read metadata Columns after fault: {error}"))?;
        if after_fault != plant.bytes {
            return Err("metadata Columns query changed the planted segment".to_owned());
        }
        store
            .close()
            .map_err(|error| format!("close metadata Columns fault Store: {error}"))?;
        retained_fault_leg = Some(capture_metadata_retained_product_leg(
            fault_directory.path(),
            MetadataRetainedPhase::Fault,
            vec![0.0_f32.to_bits(); 2],
            10,
            MetadataRetainedSearchTier::Exact,
            Some(plant.arm.clone()),
            MetadataRetainedExpectedOutcome::Refusal {
                message: retained_fault_message,
                provenance: plant.provenance.clone(),
            },
        )?);
        std::fs::write(&fault_segment_path, &fixture.clean_segment)
            .map_err(|error| format!("restore metadata Columns segment: {error}"))?;
        let retry_arm = MetadataTestArm::ObserveExecution {
            query_id: query_id.wrapping_add(2),
        };
        fault_controller
            .arm(retry_arm.clone())
            .map_err(|error| format!("arm metadata Columns retry: {error}"))?;
        let retry = open_with_controller(
            fault_directory.path(),
            OpenOptions::default().with_schema(fixture.schema.clone()),
            &fault_controller,
        )?;
        let retry_outcome = exact_query(&retry, seed, MetadataOperationKind::Columns, 10)
            .map_err(|error| format!("retry metadata Columns query: {error}"))?;
        retry_results = result_facts(&retry_outcome);
        retry_generation = retry_outcome.generation;
        retry_wal_digest = wal_digest(fault_directory.path())?;
        retry_source_digest = digest(
            &std::fs::read(&fault_segment_path)
                .map_err(|error| format!("read restored metadata Columns: {error}"))?,
        );
        retry
            .close()
            .map_err(|error| format!("close metadata Columns retry Store: {error}"))?;
        retained_retry_leg = Some(capture_metadata_retained_product_leg(
            fault_directory.path(),
            MetadataRetainedPhase::Retry,
            vec![0.0_f32.to_bits(); 2],
            10,
            MetadataRetainedSearchTier::Exact,
            Some(retry_arm),
            MetadataRetainedExpectedOutcome::Results(retry_results.clone()),
        )?);
        if clean_results != retry_results {
            return Err("metadata Columns clean/retry public results differ".to_owned());
        }
        mutation = Some(plant.mutation);
    }

    let mut execution_receipts = fixture
        .controller
        .drain_execution_receipts()
        .map_err(|error| format!("drain metadata Columns execution receipts: {error}"))?;
    execution_receipts.extend(
        fault_controller
            .drain_execution_receipts()
            .map_err(|error| format!("drain metadata Columns fault executions: {error}"))?,
    );
    let feature_receipts = fault_controller
        .drain_feature_receipts()
        .map_err(|error| format!("drain metadata Columns feature receipts: {error}"))?;
    let expected_feature_receipts = usize::from(fault == Some(MetadataFaultKind::ColumnCorruption));
    if feature_receipts.len() != expected_feature_receipts {
        return Err(format!(
            "metadata Columns expected {expected_feature_receipts} production feature receipts, observed {}",
            feature_receipts.len()
        ));
    }
    fixture
        .controller
        .assert_no_unconsumed_arm()
        .map_err(|error| format!("metadata Columns arm leak: {error}"))?;
    fault_controller
        .assert_no_unconsumed_arm()
        .map_err(|error| format!("metadata Columns fault arm leak: {error}"))?;
    let row_count = u32::try_from(fixture.input.rows.len())
        .map_err(|_| "metadata Columns expected row count exceeds u32".to_owned())?;
    let independent_expected_results =
        independent_exact_result_facts(seed, RowSource::Sealed(fixture.segment_id), 0..row_count)?;
    let fixture_evidence = MetadataFixtureEvidence::Columns(fixture.input.clone());
    let expected_sources = BTreeSet::from([source_label(RowSource::Sealed(fixture.segment_id))]);
    let mut queries = vec![
        query_evidence(
            query_id.wrapping_add(0x100),
            "active",
            independent::PredicateDto::And(Vec::new()),
            2,
            10,
            "exact",
            BTreeSet::from([source_label(RowSource::Active)]),
        ),
        query_evidence(
            query_id,
            "clean-reopened",
            independent::PredicateDto::And(Vec::new()),
            2,
            10,
            "exact",
            expected_sources.clone(),
        ),
    ];
    if fault == Some(MetadataFaultKind::ColumnCorruption) {
        queries.push(query_evidence(
            query_id.wrapping_add(1),
            "fault-refusal",
            independent::PredicateDto::And(Vec::new()),
            2,
            10,
            "exact",
            expected_sources.clone(),
        ));
        queries.push(query_evidence(
            query_id.wrapping_add(2),
            "retry-reopened",
            independent::PredicateDto::And(Vec::new()),
            2,
            10,
            "exact",
            expected_sources,
        ));
    }
    let mut retained_product_fixture = capture_metadata_retained_product_fixture(
        clean_directory.path(),
        vec![0.0_f32.to_bits(); 2],
        10,
        MetadataRetainedSearchTier::Exact,
        clean_results.clone(),
    )?;
    if let Some(fault_leg) = retained_fault_leg {
        retained_product_fixture.legs.push(fault_leg);
    }
    if let Some(retry_leg) = retained_retry_leg {
        retained_product_fixture.legs.push(retry_leg);
    }
    Ok(MetadataOperationEvidence {
        operation: MetadataOperationKind::Columns,
        fault,
        invariant: MetadataInvariantEvidence::I36 {
            input: fixture.input,
            observed: independent::I36Observed {
                active_rows: fixture.active_rows,
                raw: fixture.parsed_columns,
                reader_rows: fixture.reader_rows,
                public_rows: fixture.public_rows,
            },
        },
        execution_receipts,
        feature_receipts,
        feature_expected,
        fixture: fixture_evidence,
        queries,
        control: MetadataControlEvidence {
            namespace: "metadata_columns_roundtrip",
            seed,
            query_id_base: query_id,
            normalized_schedule_digest: 0,
            directory_relation: MetadataDirectoryRelation::DistinctByteIdentical,
            outcome: if fault == Some(MetadataFaultKind::ColumnCorruption) {
                MetadataControlOutcome::FaultRefusedRetryEquivalent
            } else {
                MetadataControlOutcome::Only
            },
            clean_results: clean_results.clone(),
            fault_results,
            retry_results,
            independent_expected_results,
            fault_error,
            clean_generation,
            fault_generation,
            retry_generation,
            clean_wal_digest,
            fault_wal_digest,
            retry_wal_digest,
            clean_source_digest,
            fault_source_digest,
            retry_source_digest,
            clean_initial_directory,
            fault_initial_directory,
        },
        mutation,
        fixture_mutations: Vec::new(),
        retained_product_fixture: Some(retained_product_fixture),
    })
}

fn run_bitmap(
    seed: u64,
    fault: Option<MetadataFaultKind>,
) -> Result<MetadataOperationEvidence, String> {
    let fixture = build_typed_fixture(seed, MetadataOperationKind::Bitmap)?;
    let query_id = query_id_base(seed, MetadataOperationKind::Bitmap);
    let clean_directory = copy_directory(fixture.directory.path())?;
    let fault_directory = copy_directory(fixture.directory.path())?;
    let clean_initial_directory = directory_digest(clean_directory.path())?;
    let fault_initial_directory = directory_digest(fault_directory.path())?;
    if clean_directory.path() == fault_directory.path() {
        return Err("metadata bitmap clean/fault directories are not distinct".to_owned());
    }
    if clean_initial_directory != fault_initial_directory {
        return Err("metadata bitmap clean/fault directory copies differ".to_owned());
    }
    let clean_controller = Arc::new(MetadataTestController::new());
    let clean_store = open_with_controller(
        clean_directory.path(),
        OpenOptions::default().with_schema(fixture.schema.clone()),
        &clean_controller,
    )?;
    let clean_outcome = exact_query(&clean_store, seed, MetadataOperationKind::Bitmap, 10)
        .map_err(|error| format!("metadata bitmap same-seed clean query: {error}"))?;
    let clean_results = result_facts(&clean_outcome);
    let clean_generation = clean_outcome.generation;
    let clean_wal_digest = wal_digest(clean_directory.path())?;
    let clean_source_digest = digest(
        &std::fs::read(clean_directory.path().join(fixture.segment_id.file_name()))
            .map_err(|error| format!("read metadata bitmap clean source: {error}"))?,
    );
    clean_store
        .close()
        .map_err(|error| format!("close metadata bitmap clean Store: {error}"))?;
    let reader = SegmentReader::open(&StdVfs, &fixture.segment_path, fixture.segment_id)
        .map_err(|error| format!("open clean metadata bitmap reader: {error}"))?;
    let columns = reader
        .columns()
        .map_err(|error| format!("decode clean metadata bitmap columns: {error}"))?;
    let alive = reader
        .alive()
        .map_err(|error| format!("decode clean metadata Alive: {error}"))?;
    let production_predicate = operation_predicate(seed, MetadataOperationKind::Bitmap);
    let evaluator = evaluate(&production_predicate, &columns, &alive)
        .map_err(|error| format!("evaluate clean metadata bitmap predicate: {error}"))?
        .iter()
        .collect::<BTreeSet<_>>();
    let input_rows = fixture
        .input
        .rows
        .iter()
        .enumerate()
        .map(|(row, cells)| independent::MetadataRowDto {
            row_id: u32::try_from(row).unwrap_or(u32::MAX),
            cells: cells.clone(),
        })
        .collect();
    let live = alive.iter_alive().collect::<BTreeSet<_>>();
    let public_results = fixture
        .clean_outcome
        .candidates
        .iter()
        .map(|candidate| candidate.row_id().local_row())
        .collect();

    let mut fault_results = clean_results.clone();
    let mut retry_results = clean_results.clone();
    let mut fault_error = None;
    let mut fault_generation = clean_generation;
    let mut retry_generation = clean_generation;
    let mut fault_wal_digest = clean_wal_digest;
    let mut retry_wal_digest = clean_wal_digest;
    let mut fault_source_digest = clean_source_digest;
    let mut retry_source_digest = clean_source_digest;
    let mut mutation = None;
    let mut feature_expected = Vec::new();
    let mut retained_fault_leg = None;
    let mut retained_retry_leg = None;
    let fault_controller = Arc::new(MetadataTestController::new());
    let fault_segment_path = fault_directory.path().join(fixture.segment_id.file_name());

    if fault == Some(MetadataFaultKind::BitmapTruncation) {
        let (alive_offset, alive_length) = region_bounds(&fixture.clean_segment, 1)?;
        let alive_region = fixture
            .clean_segment
            .get(alive_offset..alive_offset.saturating_add(alive_length))
            .ok_or_else(|| "metadata Alive region escaped segment".to_owned())?;
        let parsed_alive = independent::parse_alive(alive_region)?;
        let removed_position = parsed_alive
            .bitmap_span
            .end
            .checked_sub(1)
            .ok_or_else(|| "metadata Alive bitmap is empty".to_owned())?;
        let absolute = alive_offset
            .checked_add(removed_position)
            .ok_or_else(|| "metadata Alive mutation offset overflow".to_owned())?;
        let mut bytes = fixture.clean_segment.clone();
        let before = *bytes
            .get(absolute)
            .ok_or_else(|| "metadata Alive removed byte escaped segment".to_owned())?;
        let left_before = absolute
            .checked_sub(1)
            .and_then(|position| bytes.get(position).copied());
        let right_before = bytes.get(absolute.saturating_add(1)).copied();
        *bytes
            .get_mut(absolute)
            .ok_or_else(|| "metadata Alive removed byte escaped segment".to_owned())? = 0;
        let shortened_length = alive_length
            .checked_sub(1)
            .ok_or_else(|| "metadata Alive region cannot be shortened".to_owned())?;
        let checksum_rewrites = set_region_length(&mut bytes, 1, shortened_length)?;
        let provenance = MetadataDecodeProvenance::AliveBitmapTruncation {
            row_count: 10,
            byte_offset: u64::try_from(parsed_alive.bitmap_span.start)
                .map_err(|_| "metadata Alive bitmap offset exceeds u64".to_owned())?,
            declared_bytes: 2,
            observed_bytes: 1,
        };
        let arm = MetadataTestArm::AliveBitmapTruncation {
            query_id: query_id.wrapping_add(1),
            source: RowSource::Sealed(fixture.segment_id),
            declared_rows: 10,
            declared_bytes: 2,
            observed_bytes: 1,
        };
        let error_class = independent_error_class(&provenance);
        let effect = format!(
            "source={:?} rows={} byte_offset={} declared_bytes={} observed_bytes={} error={error_class}",
            RowSource::Sealed(fixture.segment_id),
            10,
            parsed_alive.bitmap_span.start,
            2,
            1,
        );
        feature_expected.push(MetadataFeatureExpected::AliveBitmapTruncationRefused {
            query_id: query_id.wrapping_add(1),
            source: source_label(RowSource::Sealed(fixture.segment_id)),
            operation: "metadata_bitmap_algebra",
            fault: "bitmap-truncation",
            site: "planner.exec.query_alive.refusal",
            cardinality: 1,
            declared_rows: 10,
            byte_offset: u64::try_from(parsed_alive.bitmap_span.start)
                .map_err(|_| "metadata Alive bitmap offset exceeds u64".to_owned())?,
            declared_bytes: 2,
            observed_bytes: 1,
            error_class,
            effect,
            provenance: expected_provenance(&provenance),
            expected_results: 0,
        });
        let planted = bytes.clone();
        std::fs::write(&fault_segment_path, &bytes)
            .map_err(|error| format!("write metadata Alive truncation: {error}"))?;
        fault_source_digest = digest(&bytes);
        fault_controller
            .arm(arm.clone())
            .map_err(|error| format!("arm metadata Alive fault: {error}"))?;
        let store = open_with_controller(
            fault_directory.path(),
            OpenOptions::default().with_schema(fixture.schema.clone()),
            &fault_controller,
        )?;
        fault_generation = store
            .snapshot()
            .map_err(|error| format!("snapshot metadata Alive fault: {error}"))?
            .generation();
        let error = store
            .search_filtered(
                SearchRequest::new(&[0.0, 0.0]),
                &Predicate::And(Vec::new()),
                10,
                SearchOptions::default().with_tier(SearchTier::Exact),
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect_err("metadata Alive truncation must not return results");
        require_metadata_provenance(&error, &provenance)?;
        let retained_fault_message = error.to_string();
        fault_error = Some(retained_fault_message.clone());
        fault_results.clear();
        fault_wal_digest = wal_digest(fault_directory.path())?;
        if std::fs::read(&fault_segment_path)
            .map_err(|error| format!("read metadata Alive after fault: {error}"))?
            != planted
        {
            return Err("metadata Alive query changed the planted segment".to_owned());
        }
        store
            .close()
            .map_err(|error| format!("close metadata Alive fault Store: {error}"))?;
        let mut retained_fault = capture_metadata_retained_product_leg(
            fault_directory.path(),
            MetadataRetainedPhase::Fault,
            vec![0.0_f32.to_bits(); 2],
            10,
            MetadataRetainedSearchTier::Exact,
            Some(arm),
            MetadataRetainedExpectedOutcome::Refusal {
                message: retained_fault_message,
                provenance: provenance.clone(),
            },
        )?;
        retained_fault.predicate = Some(independent::PredicateDto::And(Vec::new()));
        retained_fault_leg = Some(retained_fault);
        std::fs::write(&fault_segment_path, &fixture.clean_segment)
            .map_err(|error| format!("restore metadata Alive segment: {error}"))?;
        let retry_arm = MetadataTestArm::ObserveExecution {
            query_id: query_id.wrapping_add(2),
        };
        fault_controller
            .arm(retry_arm.clone())
            .map_err(|error| format!("arm metadata Alive retry: {error}"))?;
        let retry = open_with_controller(
            fault_directory.path(),
            OpenOptions::default().with_schema(fixture.schema.clone()),
            &fault_controller,
        )?;
        let retry_outcome = exact_query(&retry, seed, MetadataOperationKind::Bitmap, 10)
            .map_err(|error| format!("retry metadata bitmap query: {error}"))?;
        retry_results = result_facts(&retry_outcome);
        retry_generation = retry_outcome.generation;
        retry_wal_digest = wal_digest(fault_directory.path())?;
        retry_source_digest = digest(
            &std::fs::read(&fault_segment_path)
                .map_err(|error| format!("read restored metadata Alive: {error}"))?,
        );
        retry
            .close()
            .map_err(|error| format!("close metadata Alive retry Store: {error}"))?;
        retained_retry_leg = Some(capture_metadata_retained_product_leg(
            fault_directory.path(),
            MetadataRetainedPhase::Retry,
            vec![0.0_f32.to_bits(); 2],
            10,
            MetadataRetainedSearchTier::Exact,
            Some(retry_arm),
            MetadataRetainedExpectedOutcome::Results(retry_results.clone()),
        )?);
        if clean_results != retry_results {
            return Err("metadata bitmap clean/retry public results differ".to_owned());
        }
        mutation = Some(MetadataMutationEvidence {
            source: source_label(RowSource::Sealed(fixture.segment_id)),
            region: RegionKind::Alive,
            region_offset: u64::try_from(alive_offset)
                .map_err(|_| "metadata Alive region offset exceeds u64".to_owned())?,
            field_offset: u64::try_from(removed_position)
                .map_err(|_| "metadata Alive field offset exceeds u64".to_owned())?,
            absolute_offset: u64::try_from(absolute)
                .map_err(|_| "metadata Alive absolute offset exceeds u64".to_owned())?,
            before: vec![before],
            after: vec![0],
            left_neighbor_before: left_before,
            left_neighbor_after: absolute
                .checked_sub(1)
                .and_then(|position| bytes.get(position).copied()),
            right_neighbor_before: right_before,
            right_neighbor_after: bytes.get(absolute.saturating_add(1)).copied(),
            declared_bytes_before: u64::try_from(alive_length)
                .map_err(|_| "metadata Alive length exceeds u64".to_owned())?,
            declared_bytes_after: u64::try_from(shortened_length)
                .map_err(|_| "metadata Alive length exceeds u64".to_owned())?,
            observed_bytes_after: 1,
            checksum_rewrites,
            post_mutation_artifact_digest: digest(&bytes),
        });
    }

    let mut execution_receipts = fixture
        .controller
        .drain_execution_receipts()
        .map_err(|error| format!("drain metadata bitmap execution receipts: {error}"))?;
    execution_receipts.extend(
        fault_controller
            .drain_execution_receipts()
            .map_err(|error| format!("drain metadata bitmap fault executions: {error}"))?,
    );
    let feature_receipts = fault_controller
        .drain_feature_receipts()
        .map_err(|error| format!("drain metadata bitmap feature receipts: {error}"))?;
    let expected_feature_receipts = usize::from(fault == Some(MetadataFaultKind::BitmapTruncation));
    if feature_receipts.len() != expected_feature_receipts {
        return Err(format!(
            "metadata bitmap expected {expected_feature_receipts} production feature receipts, observed {}",
            feature_receipts.len()
        ));
    }
    fixture
        .controller
        .assert_no_unconsumed_arm()
        .map_err(|error| format!("metadata bitmap arm leak: {error}"))?;
    fault_controller
        .assert_no_unconsumed_arm()
        .map_err(|error| format!("metadata bitmap fault arm leak: {error}"))?;
    let lifecycle = observe_i37_lifecycle_sources(seed)?;
    execution_receipts.extend(lifecycle.receipts.iter().cloned());
    let invariant_input = independent::I37Input {
        rows: input_rows,
        live,
        predicate: operation_predicate_dto(seed, MetadataOperationKind::Bitmap),
        sources: lifecycle.inputs,
    };
    let independent_expected_results = independent_exact_result_facts(
        seed,
        RowSource::Sealed(fixture.segment_id),
        independent::evaluate_i37(&invariant_input)?.result,
    )?;
    let fixture_evidence = MetadataFixtureEvidence::Bitmap(invariant_input.clone());
    let expected_sources = BTreeSet::from([source_label(RowSource::Sealed(fixture.segment_id))]);
    let mut queries = vec![query_evidence(
        query_id,
        "clean-reopened",
        invariant_input.predicate.clone(),
        2,
        10,
        "exact",
        expected_sources.clone(),
    )];
    queries.push(lifecycle.query);
    if fault == Some(MetadataFaultKind::BitmapTruncation) {
        queries.push(query_evidence(
            query_id.wrapping_add(1),
            "fault-refusal",
            invariant_input.predicate.clone(),
            2,
            10,
            "exact",
            expected_sources.clone(),
        ));
        queries.push(query_evidence(
            query_id.wrapping_add(2),
            "retry-reopened",
            invariant_input.predicate.clone(),
            2,
            10,
            "exact",
            expected_sources,
        ));
    }
    let mut retained_product_fixture = capture_metadata_retained_product_fixture(
        clean_directory.path(),
        vec![0.0_f32.to_bits(); 2],
        10,
        MetadataRetainedSearchTier::Exact,
        clean_results.clone(),
    )?;
    if let Some(fault_leg) = retained_fault_leg {
        retained_product_fixture.legs.push(fault_leg);
    }
    if let Some(retry_leg) = retained_retry_leg {
        retained_product_fixture.legs.push(retry_leg);
    }
    Ok(MetadataOperationEvidence {
        operation: MetadataOperationKind::Bitmap,
        fault,
        invariant: MetadataInvariantEvidence::I37 {
            input: invariant_input,
            observed: independent::I37Observed {
                evaluator,
                public_results,
                sources: lifecycle.observed,
                allow_list_threshold: INDEPENDENT_ALLOW_LIST_THRESHOLD,
            },
        },
        execution_receipts,
        feature_receipts,
        feature_expected,
        fixture: fixture_evidence,
        queries,
        control: MetadataControlEvidence {
            namespace: "metadata_bitmap_algebra",
            seed,
            query_id_base: query_id,
            normalized_schedule_digest: 0,
            directory_relation: MetadataDirectoryRelation::DistinctByteIdentical,
            outcome: if fault == Some(MetadataFaultKind::BitmapTruncation) {
                MetadataControlOutcome::FaultRefusedRetryEquivalent
            } else {
                MetadataControlOutcome::Only
            },
            clean_results: clean_results.clone(),
            fault_results,
            retry_results,
            independent_expected_results,
            fault_error,
            clean_generation,
            fault_generation,
            retry_generation,
            clean_wal_digest,
            fault_wal_digest,
            retry_wal_digest,
            clean_source_digest,
            fault_source_digest,
            retry_source_digest,
            clean_initial_directory,
            fault_initial_directory,
        },
        mutation,
        fixture_mutations: Vec::new(),
        retained_product_fixture: Some(retained_product_fixture),
    })
}

fn exact_hits(
    candidates: &[zeppelin_embed::ingest::SearchCandidate],
) -> Vec<independent::ExactHitDto> {
    candidates
        .iter()
        .map(|candidate| independent::ExactHitDto {
            source: source_label(candidate.row_id().source()),
            row_id: candidate.row_id().local_row(),
            document_id: candidate
                .document()
                .map_or(0, |document| document.doc_id().get()),
            distance_bits: (-candidate.score()).to_bits(),
        })
        .collect()
}

fn range_dto(range: ClusteringKeyRange) -> independent::SourceRangeDto {
    match range {
        ClusteringKeyRange::Unstamped => independent::SourceRangeDto::Unstamped,
        ClusteringKeyRange::Empty => independent::SourceRangeDto::Empty,
        ClusteringKeyRange::Bounded { min_ts, max_ts } => independent::SourceRangeDto::Bounded {
            min: min_ts,
            max: max_ts,
        },
    }
}

fn run_planner(
    seed: u64,
    fault: Option<MetadataFaultKind>,
) -> Result<MetadataOperationEvidence, String> {
    if fault.is_some() {
        return Err("metadata pruning operation has no catalogued fault".to_owned());
    }
    let directory = tempdir().map_err(|error| format!("metadata pruning tempdir: {error}"))?;
    let controller = Arc::new(MetadataTestController::new());
    let store = open_with_controller(
        directory.path(),
        OpenOptions::default().with_schema(Schema::timestamp_only()),
        &controller,
    )?;
    let mut segment_ids = Vec::new();
    let mut segment_document_ids = Vec::new();
    let rows_per_segment = 5_u32;
    let sealed_segment_count = 4_u32;
    for segment_index in 0..sealed_segment_count {
        let document_ids = (0..rows_per_segment)
            .map(|row| {
                (u128::from(seed) << 40)
                    .checked_add(u128::from(segment_index) << 32)
                    .and_then(|value| value.checked_add(u128::from(row) + 1))
                    .map(DocId::new)
                    .ok_or_else(|| "metadata pruning document id overflow".to_owned())
            })
            .collect::<Result<Vec<_>, String>>()?;
        let documents = document_ids
            .iter()
            .copied()
            .enumerate()
            .map(|(row, document_id)| {
                Ok(IngestDocument::new(
                    DocumentVersion::new(document_id, Revision::new(1)),
                    vec![row as f32, -(row as f32)],
                )
                .with_timestamp(
                    i64::from(segment_index) * 100
                        + i64::try_from(row)
                            .map_err(|_| "metadata pruning row exceeds i64".to_owned())?,
                ))
            })
            .collect::<Result<Vec<_>, String>>()?;
        store
            .ingest(IngestBatch::new(documents))
            .map_err(|error| format!("ingest metadata pruning segment: {error}"))?;
        store
            .seal()
            .map_err(|error| format!("seal metadata pruning segment: {error}"))?;
        let snapshot = store
            .snapshot()
            .map_err(|error| format!("snapshot metadata pruning segment: {error}"))?;
        let new_id = snapshot
            .segments()
            .iter()
            .map(|segment| segment.meta().id)
            .find(|id| !segment_ids.contains(id))
            .ok_or_else(|| "metadata pruning seal published no new segment".to_owned())?;
        segment_ids.push(new_id);
        segment_document_ids.push(document_ids);
        drop(snapshot);
    }
    let active_timestamps = [100_i64, 999_i64];
    let active_documents = active_timestamps
        .iter()
        .enumerate()
        .map(|(row, timestamp)| {
            let row = u128::try_from(row)
                .map_err(|_| "metadata pruning active row exceeds u128".to_owned())?;
            let document_id = (u128::from(seed) << 40)
                .checked_add(u128::from(u32::MAX) << 32)
                .and_then(|value| value.checked_add(row + 1))
                .ok_or_else(|| "metadata pruning active document id overflow".to_owned())?;
            Ok(IngestDocument::new(
                DocumentVersion::new(DocId::new(document_id), Revision::new(1)),
                vec![row as f32, -(row as f32)],
            )
            .with_timestamp(*timestamp))
        })
        .collect::<Result<Vec<_>, String>>()?;
    store
        .ingest(IngestBatch::new(active_documents))
        .map_err(|error| format!("ingest metadata pruning active rows: {error}"))?;
    let tombstoned_documents = segment_document_ids
        .last()
        .cloned()
        .ok_or_else(|| "metadata pruning tombstone documents are absent".to_owned())?;
    store
        .delete(DeleteBatch::new(tombstoned_documents))
        .map_err(|error| format!("publicly tombstone metadata pruning segment: {error}"))?;
    let published = store
        .snapshot()
        .map_err(|error| format!("snapshot metadata pruning delete: {error}"))?;
    segment_ids = published
        .segments()
        .iter()
        .map(|segment| segment.meta().id)
        .collect();
    if segment_ids.len() != usize::try_from(sealed_segment_count).unwrap_or(usize::MAX) {
        return Err(format!(
            "metadata pruning delete changed sealed source count to {}",
            segment_ids.len()
        ));
    }
    drop(published);
    store
        .close()
        .map_err(|error| format!("close metadata pruning fixture after public delete: {error}"))?;
    let delete_records = WalReader::open(&StdVfs, &directory.path().join("wal.ze"))
        .map_err(|error| format!("read metadata pruning WAL: {error}"))?
        .records()
        .iter()
        .filter(|record| record.op == DELETE_V1)
        .count();
    if delete_records != 1 {
        return Err(format!(
            "metadata pruning expected exactly one public delete WAL record, observed {delete_records}"
        ));
    }
    let mut pruning_manifest =
        load_manifest(&StdVfs, &directory.path().join(MANIFEST_FILE), u64::MAX)
            .map_err(|error| format!("load metadata pruning manifest: {error}"))?;
    let unstamped = pruning_manifest
        .segments
        .iter_mut()
        .find(|segment| segment.clustering_key_range != ClusteringKeyRange::Empty)
        .ok_or_else(|| "metadata pruning manifest lacks a bounded source".to_owned())?;
    unstamped.clustering_key_range = ClusteringKeyRange::Unstamped;
    commit_manifest(
        &StdVfs,
        directory.path(),
        &pruning_manifest,
        ordered_policy(),
    )
    .map_err(|error| format!("commit metadata pruning missing-bounds source: {error}"))?;
    let store = open_with_controller(
        directory.path(),
        OpenOptions::default().with_schema(Schema::timestamp_only()),
        &controller,
    )?;
    let planner_snapshot = store
        .snapshot()
        .map_err(|error| format!("snapshot metadata pruning source states: {error}"))?;
    let mut source_states = planner_snapshot
        .segments()
        .iter()
        .map(|segment| independent::I38SourceDto {
            source: source_label(RowSource::Sealed(segment.meta().id)),
            sealed: true,
            range: range_dto(segment.meta().clustering_key_range),
        })
        .collect::<Vec<_>>();
    drop(planner_snapshot);
    source_states.push(independent::I38SourceDto {
        source: source_label(RowSource::Active),
        sealed: false,
        range: independent::SourceRangeDto::Unstamped,
    });
    let query_id = query_id_base(seed, MetadataOperationKind::Planner);
    let query = [0.0_f32, 0.0_f32];
    let total_rows = usize::try_from(rows_per_segment)
        .map_err(|_| "metadata pruning rows exceed usize".to_owned())?
        .checked_mul(segment_ids.len().saturating_sub(1))
        .and_then(|rows| rows.checked_add(active_timestamps.len()))
        .ok_or_else(|| "metadata pruning total row count overflow".to_owned())?;
    let unfiltered = store
        .search(
            SearchRequest::new(&query),
            total_rows,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| format!("metadata unfiltered exact baseline: {error}"))?;
    controller
        .arm(MetadataTestArm::ObserveExecution { query_id })
        .map_err(|error| format!("arm metadata pruning observation: {error}"))?;
    let (predicate, predicate_dto) = planner_predicate(seed);
    let filtered = store
        .search_filtered(
            SearchRequest::new(&query),
            &predicate,
            total_rows,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| format!("metadata pruning filtered query: {error}"))?;
    let mut rows = segment_ids
        .iter()
        .copied()
        .enumerate()
        .flat_map(|(segment_index, id)| {
            (0..rows_per_segment).map(move |row| {
                let segment_index = u32::try_from(segment_index).unwrap_or(u32::MAX);
                independent::SourceMetadataRowDto {
                    source: source_label(RowSource::Sealed(id)),
                    row_id: row,
                    document_id: (u128::from(seed) << 40)
                        + (u128::from(segment_index) << 32)
                        + u128::from(row)
                        + 1,
                    cells: BTreeMap::from([(
                        0,
                        independent::ScalarCell::I64(
                            i64::from(segment_index) * 100 + i64::from(row),
                        ),
                    )]),
                }
            })
        })
        .collect::<Vec<_>>();
    rows.extend(
        active_timestamps
            .iter()
            .enumerate()
            .map(|(row, timestamp)| {
                let row_id = u32::try_from(row).unwrap_or(u32::MAX);
                independent::SourceMetadataRowDto {
                    source: source_label(RowSource::Active),
                    row_id,
                    document_id: (u128::from(seed) << 40)
                        + (u128::from(u32::MAX) << 32)
                        + u128::from(row_id)
                        + 1,
                    cells: BTreeMap::from([(0, independent::ScalarCell::I64(*timestamp))]),
                }
            }),
    );
    let tombstoned_source = segment_ids
        .last()
        .copied()
        .map(RowSource::Sealed)
        .map(source_label)
        .ok_or_else(|| "metadata pruning tombstone source is absent".to_owned())?;
    let live = rows
        .iter()
        .filter(|row| row.source != tombstoned_source)
        .map(|row| (row.source.clone(), row.row_id))
        .collect::<BTreeSet<_>>();
    let clean_results = result_facts(&filtered);
    let generation = filtered.generation;
    let wal = wal_digest(directory.path())?;
    let mut source_bytes = Vec::new();
    for id in &segment_ids {
        source_bytes.extend_from_slice(id.as_bytes());
        source_bytes.extend_from_slice(
            &std::fs::read(directory.path().join(id.file_name()))
                .map_err(|error| format!("read metadata pruning source: {error}"))?,
        );
    }
    let source_digest = digest(&source_bytes);
    let execution_receipts = controller
        .drain_execution_receipts()
        .map_err(|error| format!("drain metadata planner execution receipts: {error}"))?;
    let feature_receipts = controller
        .drain_feature_receipts()
        .map_err(|error| format!("drain metadata planner feature receipts: {error}"))?;
    controller
        .assert_no_unconsumed_arm()
        .map_err(|error| format!("metadata planner arm leak: {error}"))?;
    store
        .close()
        .map_err(|error| format!("close metadata pruning Store: {error}"))?;
    let initial_directory = directory_digest(directory.path())?;

    let control = MetadataControlEvidence {
        namespace: "metadata_pruning_soundness",
        seed,
        query_id_base: query_id,
        normalized_schedule_digest: 0,
        directory_relation: MetadataDirectoryRelation::UnpairedSingleDirectory,
        outcome: MetadataControlOutcome::FilteredSucceeded,
        clean_results: clean_results.clone(),
        fault_results: clean_results.clone(),
        retry_results: clean_results.clone(),
        independent_expected_results: Vec::new(),
        fault_error: None,
        clean_generation: generation,
        fault_generation: generation,
        retry_generation: generation,
        clean_wal_digest: wal,
        fault_wal_digest: wal,
        retry_wal_digest: wal,
        clean_source_digest: source_digest,
        fault_source_digest: source_digest,
        retry_source_digest: source_digest,
        clean_initial_directory: initial_directory.clone(),
        fault_initial_directory: initial_directory,
    };
    if !feature_receipts.is_empty() {
        return Err(format!(
            "metadata pruning expected no production feature receipts, observed {}",
            feature_receipts.len()
        ));
    }
    let invariant_input = independent::I38Input {
        sources: source_states,
        rows,
        live,
        predicate: predicate_dto,
        unfiltered_exact: exact_hits(&unfiltered.candidates),
        expected_delete_records: 1,
    };
    let fixture_evidence = MetadataFixtureEvidence::Planner(invariant_input.clone());
    let expected_sources = invariant_input
        .rows
        .iter()
        .map(|row| row.source.clone())
        .collect::<BTreeSet<_>>();
    let queries = vec![
        query_evidence(
            query_id.wrapping_sub(1),
            "unpruned-public-exact-baseline",
            independent::PredicateDto::And(Vec::new()),
            2,
            u64::try_from(total_rows).map_err(|_| "metadata pruning k exceeds u64".to_owned())?,
            "exact",
            expected_sources.clone(),
        ),
        query_evidence(
            query_id,
            "filtered-public-exact",
            invariant_input.predicate.clone(),
            2,
            u64::try_from(total_rows).map_err(|_| "metadata pruning k exceeds u64".to_owned())?,
            "exact",
            expected_sources,
        ),
    ];
    Ok(MetadataOperationEvidence {
        operation: MetadataOperationKind::Planner,
        fault,
        invariant: MetadataInvariantEvidence::I38 {
            input: invariant_input,
            observed: independent::I38Observed {
                filtered_exact: exact_hits(&filtered.candidates),
                pruned_sources: filtered
                    .plans
                    .iter()
                    .filter(|plan| plan.branch == SegmentBranch::Pruned)
                    .map(|plan| source_label(plan.source))
                    .collect(),
                reports: filtered
                    .plans
                    .iter()
                    .map(|plan| report(query_id, plan))
                    .collect(),
                execution_receipts: execution_receipts.iter().map(execution_receipt).collect(),
                allow_list_threshold: INDEPENDENT_ALLOW_LIST_THRESHOLD,
                wal_delete_records: u64::try_from(delete_records)
                    .map_err(|_| "metadata pruning delete count exceeds u64".to_owned())?,
            },
        },
        execution_receipts,
        feature_receipts,
        feature_expected: Vec::new(),
        fixture: fixture_evidence,
        queries,
        control,
        mutation: None,
        fixture_mutations: Vec::new(),
        retained_product_fixture: Some(capture_metadata_retained_product_fixture(
            directory.path(),
            query.iter().map(|value| value.to_bits()).collect(),
            u64::try_from(total_rows)
                .map_err(|_| "metadata retained pruning k exceeds u64".to_owned())?,
            MetadataRetainedSearchTier::Exact,
            clean_results,
        )?),
    })
}

struct SelectivityBoundaryEvidence {
    control: MetadataControlEvidence,
    execution: Vec<MetadataExecutionReceipt>,
    feature: Vec<MetadataFeatureReceipt>,
    feature_expected: Vec<MetadataFeatureExpected>,
    outcomes: Vec<(u64, FilteredSearchOutcome)>,
    expected: Vec<independent::I39ExpectedCase>,
    retained_product_fixture: MetadataRetainedProductFixture,
}

fn run_selectivity_boundary(
    seed: u64,
    query_id: u64,
) -> Result<SelectivityBoundaryEvidence, String> {
    let directory = tempdir().map_err(|error| format!("metadata boundary tempdir: {error}"))?;
    let keep = ColumnId::new(1);
    let schema = Schema::new(vec![ColumnDefinition::new(
        keep,
        "keep",
        ColumnType::U64,
        false,
    )])
    .map_err(|error| format!("metadata boundary schema: {error}"))?;
    let materialize_controller = Arc::new(MetadataTestController::new());
    let store = open_with_controller(
        directory.path(),
        OpenOptions::default().with_schema(schema.clone()),
        &materialize_controller,
    )?;
    let rows = usize::try_from(ALLOW_LIST_ROWS_THRESHOLD + 1)
        .map_err(|_| "metadata boundary row count exceeds usize".to_owned())?;
    let documents = (0..rows)
        .map(|row| {
            IngestDocument::new(
                DocumentVersion::new(
                    DocId::new((u128::from(seed) << 32) + row as u128 + 1),
                    Revision::new(1),
                ),
                vec![row as f32, -(row as f32)],
            )
            .with_columns(vec![(
                keep,
                PredicateValue::U64(u64::from(row < rows.saturating_sub(1))),
            )])
        })
        .collect::<Vec<_>>();
    store
        .ingest(IngestBatch::new(documents))
        .map_err(|error| format!("ingest metadata boundary rows: {error}"))?;
    store
        .close()
        .map_err(|error| format!("close materialized metadata boundary Store: {error}"))?;
    let clean_directory = copy_directory(directory.path())?;
    let fault_directory = copy_directory(directory.path())?;
    let clean_initial_directory = directory_digest(clean_directory.path())?;
    let fault_initial_directory = directory_digest(fault_directory.path())?;
    if clean_directory.path() == fault_directory.path() {
        return Err("metadata selectivity clean/fault directories are not distinct".to_owned());
    }
    if clean_initial_directory != fault_initial_directory {
        return Err("metadata selectivity clean/fault directory copies differ".to_owned());
    }
    let threshold = Predicate::Eq {
        column: keep,
        value: PredicateValue::U64(1),
    };
    let all = Predicate::And(Vec::new());
    let run_pair = |store: &Store| -> Result<_, String> {
        let mut facts = Vec::new();
        let mut outcomes = Vec::new();
        for predicate in [&threshold, &all] {
            let outcome = store
                .search_filtered(
                    SearchRequest::new(&[0.0, 0.0]),
                    predicate,
                    rows,
                    SearchOptions::default().with_tier(SearchTier::Exact),
                    QueryControl::Cancel(CancelToken::new()),
                )
                .map_err(|error| format!("metadata boundary query: {error}"))?;
            facts.extend(result_facts(&outcome));
            outcomes.push(outcome);
        }
        Ok((facts, outcomes))
    };
    let clean_controller = Arc::new(MetadataTestController::new());
    let clean_store = open_with_controller(
        clean_directory.path(),
        OpenOptions::default().with_schema(schema.clone()),
        &clean_controller,
    )?;
    let (clean_results, clean_outcomes) = run_pair(&clean_store)?;
    let generation = clean_store
        .snapshot()
        .map_err(|error| format!("snapshot metadata boundary Store: {error}"))?
        .generation();
    let wal = wal_digest(clean_directory.path())?;
    let clean_source = directory_digest(clean_directory.path())?.digest;
    clean_store
        .close()
        .map_err(|error| format!("close metadata boundary clean Store: {error}"))?;

    let controller = Arc::new(MetadataTestController::new());
    let fault_store = open_with_controller(
        fault_directory.path(),
        OpenOptions::default().with_schema(schema),
        &controller,
    )?;
    controller
        .arm_selectivity_pair(
            query_id,
            query_id.wrapping_add(1),
            ALLOW_LIST_ROWS_THRESHOLD,
        )
        .map_err(|error| format!("arm metadata boundary pair: {error}"))?;
    let (fault_results, fault_outcomes) = run_pair(&fault_store)?;
    let fault_generation = fault_store
        .snapshot()
        .map_err(|error| format!("snapshot metadata boundary fault Store: {error}"))?
        .generation();
    let fault_wal = wal_digest(fault_directory.path())?;
    let fault_source = directory_digest(fault_directory.path())?.digest;
    let (retry_results, retry_outcomes) = run_pair(&fault_store)?;
    let retry_generation = fault_store
        .snapshot()
        .map_err(|error| format!("snapshot metadata boundary retry Store: {error}"))?
        .generation();
    let retry_wal = wal_digest(fault_directory.path())?;
    let retry_source = directory_digest(fault_directory.path())?.digest;
    if clean_results != fault_results || clean_results != retry_results {
        return Err("metadata selectivity clean/fault/retry results differ".to_owned());
    }
    let execution = controller
        .drain_execution_receipts()
        .map_err(|error| format!("drain metadata boundary execution receipts: {error}"))?;
    let feature = controller
        .drain_feature_receipts()
        .map_err(|error| format!("drain metadata boundary feature receipts: {error}"))?;
    controller
        .assert_no_unconsumed_arm()
        .map_err(|error| format!("metadata boundary arm leak: {error}"))?;
    fault_store
        .close()
        .map_err(|error| format!("close metadata boundary Store: {error}"))?;
    let retained_predicates = [
        independent::PredicateDto::Eq {
            column: keep.get(),
            value: independent::ScalarCell::U64(1),
        },
        independent::PredicateDto::And(Vec::new()),
    ];
    let retained_k = u64::try_from(rows)
        .map_err(|_| "metadata retained selectivity k exceeds u64".to_owned())?;
    let mut retained_legs = Vec::with_capacity(6);
    for (phase, retained_directory, retained_outcomes) in [
        (
            MetadataRetainedPhase::Clean,
            clean_directory.path(),
            &clean_outcomes,
        ),
        (
            MetadataRetainedPhase::Fault,
            fault_directory.path(),
            &fault_outcomes,
        ),
        (
            MetadataRetainedPhase::Retry,
            fault_directory.path(),
            &retry_outcomes,
        ),
    ] {
        for (offset, (predicate, outcome)) in retained_predicates
            .iter()
            .zip(retained_outcomes)
            .enumerate()
        {
            let offset = u64::try_from(offset)
                .map_err(|_| "metadata retained selectivity offset exceeds u64".to_owned())?;
            let arm = (phase == MetadataRetainedPhase::Fault).then_some(
                MetadataTestArm::SelectivityBoundary {
                    query_id: query_id.wrapping_add(offset),
                    expected_cardinality: INDEPENDENT_ALLOW_LIST_THRESHOLD + offset,
                },
            );
            let mut leg = capture_metadata_retained_product_leg(
                retained_directory,
                phase,
                vec![0.0_f32.to_bits(); 2],
                retained_k,
                MetadataRetainedSearchTier::Exact,
                arm,
                MetadataRetainedExpectedOutcome::Results(result_facts(outcome)),
            )?;
            leg.predicate = Some(predicate.clone());
            retained_legs.push(leg);
        }
    }
    let retained_product_fixture = MetadataRetainedProductFixture {
        legs: retained_legs,
    };
    let outcomes = fault_outcomes
        .into_iter()
        .enumerate()
        .map(|(offset, outcome)| {
            let offset = u64::try_from(offset).unwrap_or(u64::MAX);
            (query_id.wrapping_add(offset), outcome)
        })
        .collect();
    let expected = vec![
        expected_scan_case(
            query_id,
            RowSource::Active,
            INDEPENDENT_ALLOW_LIST_THRESHOLD + 1,
            INDEPENDENT_ALLOW_LIST_THRESHOLD,
            true,
            false,
        ),
        expected_scan_case(
            query_id.wrapping_add(1),
            RowSource::Active,
            INDEPENDENT_ALLOW_LIST_THRESHOLD + 1,
            INDEPENDENT_ALLOW_LIST_THRESHOLD + 1,
            true,
            false,
        ),
    ];
    let row_count = u32::try_from(rows)
        .map_err(|_| "metadata boundary expected row count exceeds u32".to_owned())?;
    let mut independent_expected_results =
        independent_exact_result_facts(seed, RowSource::Active, 0..row_count.saturating_sub(1))?;
    independent_expected_results.extend(independent_exact_result_facts(
        seed,
        RowSource::Active,
        0..row_count,
    )?);
    Ok(SelectivityBoundaryEvidence {
        control: MetadataControlEvidence {
            namespace: "metadata_selectivity_boundary",
            seed,
            query_id_base: query_id,
            normalized_schedule_digest: 0,
            directory_relation: MetadataDirectoryRelation::DistinctByteIdentical,
            outcome: MetadataControlOutcome::FaultAndRetryEquivalent,
            clean_results,
            fault_results,
            retry_results,
            independent_expected_results,
            fault_error: None,
            clean_generation: generation,
            fault_generation,
            retry_generation,
            clean_wal_digest: wal,
            fault_wal_digest: fault_wal,
            retry_wal_digest: retry_wal,
            clean_source_digest: clean_source,
            fault_source_digest: fault_source,
            retry_source_digest: retry_source,
            clean_initial_directory,
            fault_initial_directory,
        },
        outcomes,
        expected,
        execution,
        feature,
        retained_product_fixture,
        feature_expected: vec![
            MetadataFeatureExpected::SelectivityBoundaryChosen {
                query_id,
                source: "active".to_owned(),
                operation: "metadata_execution_truth",
                fault: "selectivity-boundary",
                site: "planner.choose.allow-list-threshold",
                cardinality: 1,
                filter_cardinality: INDEPENDENT_ALLOW_LIST_THRESHOLD,
                threshold: INDEPENDENT_ALLOW_LIST_THRESHOLD,
                branch: independent::ExecutionBranchDto::ExactAllowList,
                effect: format!(
                    "source={:?} cardinality={} threshold={} branch=ExactAllowList",
                    RowSource::Active,
                    INDEPENDENT_ALLOW_LIST_THRESHOLD,
                    INDEPENDENT_ALLOW_LIST_THRESHOLD,
                ),
            },
            MetadataFeatureExpected::SelectivityBoundaryChosen {
                query_id: query_id.wrapping_add(1),
                source: "active".to_owned(),
                operation: "metadata_execution_truth",
                fault: "selectivity-boundary",
                site: "planner.choose.allow-list-threshold",
                cardinality: 1,
                filter_cardinality: INDEPENDENT_ALLOW_LIST_THRESHOLD + 1,
                threshold: INDEPENDENT_ALLOW_LIST_THRESHOLD,
                branch: independent::ExecutionBranchDto::MaskedScan,
                effect: format!(
                    "source={:?} cardinality={} threshold={} branch=MaskedScan",
                    RowSource::Active,
                    INDEPENDENT_ALLOW_LIST_THRESHOLD + 1,
                    INDEPENDENT_ALLOW_LIST_THRESHOLD,
                ),
            },
        ],
    })
}

fn graph_epoch() -> StoreEpoch {
    let document = EmbeddingTower {
        model_id: "metadata-i39-sift".to_owned(),
        model_version: "1".to_owned(),
        weights_digest: vec![0x39],
        dims: GRAPH_DIMS as u32,
        normalization: Normalization::None,
        prompt_prefix: String::new(),
        max_tokens: 512,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    StoreEpoch {
        embedding: EmbeddingEpoch {
            query: document.clone(),
            document,
            alignment_digest: Vec::new(),
        },
        tokenizer: TokenizerConfig::text_default().epoch(),
    }
}

fn graph_document_id(seed: u64, row: usize) -> DocId {
    DocId::new((u128::from(seed) << 64) | (row as u128 + 1))
}

struct GraphPublication {
    id: SegmentId,
    reachable_nodes: u64,
    mutations: Vec<MetadataMutationEvidence>,
}

fn inspect_public_graph(
    path: &Path,
    id: SegmentId,
    disconnect: bool,
) -> Result<(Vec<MetadataMutationEvidence>, u64), String> {
    const GRAPH_TRAILER_BYTES: usize = 128;
    const GRAPH_CACHE_LINE_BYTES: usize = 128;
    let mut bytes = std::fs::read(path)
        .map_err(|error| format!("read public metadata graph for fixture mutation: {error}"))?;
    let entry = directory_entry_for_kind(&bytes, RegionKind::GraphNodeBlocks)?;
    let (region_offset, region_length) = region_bounds(&bytes, entry)?;
    let trailer_relative = region_length
        .checked_sub(GRAPH_TRAILER_BYTES)
        .ok_or_else(|| "metadata graph region is shorter than its trailer".to_owned())?;
    let trailer = region_offset
        .checked_add(trailer_relative)
        .ok_or_else(|| "metadata graph trailer offset overflow".to_owned())?;
    if bytes.get(trailer..trailer.saturating_add(8)) != Some(b"ZEGRNB01".as_slice()) {
        return Err("metadata graph fixture magic is not ZEGRNB01".to_owned());
    }
    if read_u16(&bytes, trailer + 8, "graph version")? != 1
        || read_u16(&bytes, trailer + 10, "graph flags")? != 0
    {
        return Err("metadata graph fixture version/flags are invalid".to_owned());
    }
    let dims = read_u32(&bytes, trailer + 12, "graph dimensions")?;
    let padded_dims = read_u32(&bytes, trailer + 16, "graph padded dimensions")?;
    let max_degree = usize::from(
        *bytes
            .get(trailer + 20)
            .ok_or_else(|| "metadata graph max degree is truncated".to_owned())?,
    );
    if bytes
        .get(trailer + 21..trailer + 24)
        .is_none_or(|reserved| reserved.iter().any(|byte| *byte != 0))
    {
        return Err("metadata graph degree padding is nonzero".to_owned());
    }
    let stride = usize::try_from(read_u32(&bytes, trailer + 24, "graph stride")?)
        .map_err(|_| "metadata graph stride exceeds usize".to_owned())?;
    let node_count = usize::try_from(read_u32(&bytes, trailer + 28, "graph nodes")?)
        .map_err(|_| "metadata graph node count exceeds usize".to_owned())?;
    if dims == 0 || padded_dims < dims || !padded_dims.is_multiple_of(128) {
        return Err("metadata graph dimensions violate frozen geometry".to_owned());
    }
    let code_bytes = usize::try_from(padded_dims)
        .map_err(|_| "metadata graph padded dimensions exceed usize".to_owned())?
        .div_ceil(2);
    let unaligned = code_bytes
        .checked_add(16)
        .and_then(|value| value.checked_add(max_degree.saturating_mul(4)))
        .ok_or_else(|| "metadata graph stride formula overflow".to_owned())?;
    let expected_stride = unaligned
        .checked_add(GRAPH_CACHE_LINE_BYTES - 1)
        .map(|value| value / GRAPH_CACHE_LINE_BYTES * GRAPH_CACHE_LINE_BYTES)
        .ok_or_else(|| "metadata graph stride rounding overflow".to_owned())?;
    if stride != expected_stride
        || stride
            .checked_mul(node_count)
            .is_none_or(|block_bytes| block_bytes != trailer_relative)
    {
        return Err(format!(
            "metadata graph fixture geometry mismatch stride={stride}/{expected_stride} nodes={node_count} block_bytes={trailer_relative}"
        ));
    }
    let graph_checksum_end = trailer
        .checked_add(32)
        .ok_or_else(|| "metadata graph checksum span overflow".to_owned())?;
    if read_u64(&bytes, trailer + 32, "graph checksum")?
        != digest(
            bytes
                .get(region_offset..graph_checksum_end)
                .ok_or_else(|| "metadata graph checksum span is truncated".to_owned())?,
        )
    {
        return Err("metadata graph internal checksum is stale before mutation".to_owned());
    }
    if bytes
        .get(trailer + 40..trailer + GRAPH_TRAILER_BYTES)
        .is_none_or(|reserved| reserved.iter().any(|byte| *byte != 0))
    {
        return Err("metadata graph trailer reserved bytes are nonzero".to_owned());
    }

    let source = source_label(RowSource::Sealed(id));
    let adjacency_width = 4_usize
        .checked_add(max_degree.saturating_mul(4))
        .ok_or_else(|| "metadata graph adjacency width overflow".to_owned())?;
    let declared_region_bytes = u64::try_from(region_length)
        .map_err(|_| "metadata graph region length exceeds u64".to_owned())?;
    let mut mutations = Vec::with_capacity(node_count);
    let mut entries = Vec::new();
    let mut adjacency = Vec::with_capacity(node_count);
    for row in 0..node_count {
        let field_relative = row
            .checked_mul(stride)
            .and_then(|offset| offset.checked_add(code_bytes + 12))
            .ok_or_else(|| "metadata graph adjacency offset overflow".to_owned())?;
        let absolute = region_offset
            .checked_add(field_relative)
            .ok_or_else(|| "metadata graph adjacency absolute offset overflow".to_owned())?;
        let end = absolute
            .checked_add(adjacency_width)
            .ok_or_else(|| "metadata graph adjacency end overflow".to_owned())?;
        let before = bytes
            .get(absolute..end)
            .ok_or_else(|| format!("metadata graph row {row} adjacency is truncated"))?
            .to_vec();
        let degree = usize::from(
            *before
                .first()
                .ok_or_else(|| "metadata graph degree is absent".to_owned())?,
        );
        if degree > max_degree || before.get(2..4) != Some(&[0, 0]) {
            return Err(format!(
                "metadata graph row {row} adjacency header is invalid"
            ));
        }
        if before.get(1).is_some_and(|flags| flags & 1 != 0) {
            entries.push(row);
        }
        let mut neighbors = Vec::with_capacity(degree);
        for slot in 0..max_degree {
            let offset = 4_usize.saturating_add(slot.saturating_mul(4));
            let neighbor = u32::from_le_bytes(
                before
                    .get(offset..offset.saturating_add(4))
                    .ok_or_else(|| "metadata graph neighbor is truncated".to_owned())?
                    .try_into()
                    .map_err(|_| "metadata graph neighbor is not u32".to_owned())?,
            );
            if (slot < degree
                && usize::try_from(neighbor)
                    .ok()
                    .is_none_or(|id| id >= node_count))
                || (slot >= degree && neighbor != u32::MAX)
            {
                return Err(format!(
                    "metadata graph row {row} slot {slot} violates neighbor geometry"
                ));
            }
            if slot < degree {
                neighbors.push(
                    usize::try_from(neighbor)
                        .map_err(|_| "metadata graph neighbor exceeds usize".to_owned())?,
                );
            }
        }
        adjacency.push(neighbors);
        if !disconnect {
            continue;
        }
        let left_before = absolute
            .checked_sub(1)
            .and_then(|position| bytes.get(position).copied());
        let right_before = bytes.get(end).copied();
        *bytes
            .get_mut(absolute)
            .ok_or_else(|| "metadata graph degree field is truncated".to_owned())? = 0;
        bytes
            .get_mut(absolute + 4..end)
            .ok_or_else(|| "metadata graph neighbor span is truncated".to_owned())?
            .fill(u8::MAX);
        let after = bytes
            .get(absolute..end)
            .ok_or_else(|| "metadata graph mutated adjacency is truncated".to_owned())?
            .to_vec();
        mutations.push(MetadataMutationEvidence {
            source: source.clone(),
            region: RegionKind::GraphNodeBlocks,
            region_offset: u64::try_from(region_offset)
                .map_err(|_| "metadata graph region offset exceeds u64".to_owned())?,
            field_offset: u64::try_from(field_relative)
                .map_err(|_| "metadata graph field offset exceeds u64".to_owned())?,
            absolute_offset: u64::try_from(absolute)
                .map_err(|_| "metadata graph absolute offset exceeds u64".to_owned())?,
            before,
            after,
            left_neighbor_before: left_before,
            left_neighbor_after: left_before,
            right_neighbor_before: right_before,
            right_neighbor_after: right_before,
            declared_bytes_before: declared_region_bytes,
            declared_bytes_after: declared_region_bytes,
            observed_bytes_after: declared_region_bytes,
            checksum_rewrites: Vec::new(),
            post_mutation_artifact_digest: 0,
        });
    }
    if entries.len() != 4 {
        return Err(format!(
            "metadata graph independent parser expected four entries, observed {entries:?}"
        ));
    }
    let reachable_nodes = if disconnect {
        u64::try_from(entries.len())
            .map_err(|_| "metadata graph entry count exceeds u64".to_owned())?
    } else {
        let mut seen = vec![false; node_count];
        let mut pending = entries.clone();
        while let Some(row) = pending.pop() {
            let Some(marked) = seen.get_mut(row) else {
                return Err("metadata graph traversal row escaped node count".to_owned());
            };
            if *marked {
                continue;
            }
            *marked = true;
            pending.extend(
                adjacency
                    .get(row)
                    .ok_or_else(|| "metadata graph adjacency row is missing".to_owned())?
                    .iter()
                    .copied(),
            );
        }
        u64::try_from(seen.into_iter().filter(|marked| *marked).count())
            .map_err(|_| "metadata graph reachability exceeds u64".to_owned())?
    };
    if !disconnect {
        return Ok((mutations, reachable_nodes));
    }
    let graph_checksum = digest(
        bytes
            .get(region_offset..graph_checksum_end)
            .ok_or_else(|| "metadata graph mutated checksum span is truncated".to_owned())?,
    );
    let mut checksum_rewrites = Vec::new();
    rewrite_checksum_field(
        &mut bytes,
        trailer + 32,
        graph_checksum,
        MetadataChecksumField::GraphInternal,
        &mut checksum_rewrites,
    )?;
    checksum_rewrites.extend(rewrite_region(&mut bytes, entry)?);
    validate_rewritten_region_integrity(&bytes, RegionKind::GraphNodeBlocks)?;
    let post_mutation_artifact_digest = digest(&bytes);
    for mutation in &mut mutations {
        mutation.checksum_rewrites.clone_from(&checksum_rewrites);
        mutation.post_mutation_artifact_digest = post_mutation_artifact_digest;
    }
    std::fs::write(path, &bytes)
        .map_err(|error| format!("write disconnected public metadata graph: {error}"))?;
    let reader = SegmentReader::open(&StdVfs, path, id)
        .map_err(|error| format!("open disconnected public metadata graph: {error}"))?;
    let graph = reader
        .graph_node_blocks()
        .map_err(|error| format!("decode disconnected public metadata graph: {error}"))?;
    for row in 0..graph.node_count() {
        if graph
            .block(row)
            .map_err(|error| format!("read disconnected graph row {row}: {error}"))?
            .degree()
            != 0
        {
            return Err(format!("metadata graph row {row} retained a neighbor"));
        }
    }
    Ok((mutations, reachable_nodes))
}

fn publish_graph(
    directory: &Path,
    seed: u64,
    disconnected: bool,
) -> Result<GraphPublication, String> {
    let epoch = graph_epoch();
    let store = Store::open(directory, OpenOptions::default().with_epoch(epoch.clone()))
        .map_err(|error| format!("open metadata graph Store: {error}"))?;
    let vector = vec![0.0_f32; GRAPH_DIMS];
    let documents = (0..GRAPH_ROWS)
        .map(|row| {
            IngestDocument::new(
                DocumentVersion::new(graph_document_id(seed, row), Revision::new(1)),
                vector.clone(),
            )
            .with_timestamp(i64::try_from(row).unwrap_or(i64::MAX))
        })
        .collect();
    store
        .ingest(IngestBatch::new(documents).with_epoch(epoch.identity()))
        .map_err(|error| format!("ingest metadata graph Store: {error}"))?;
    store
        .seal()
        .map_err(|error| format!("seal metadata graph Store: {error}"))?;
    let report = store.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: Duration::from_secs(30),
            bytes: u64::MAX,
        },
        TierThresholds {
            graph_min_rows: GRAPH_ROWS as u32,
        },
    );
    if report.graphs_built != 1 || !matches!(report.status, MaintenanceStatus::Complete) {
        return Err(format!(
            "metadata graph maintenance expected one graph, built={} status={:?}",
            report.graphs_built, report.status
        ));
    }
    let snapshot = store
        .snapshot()
        .map_err(|error| format!("snapshot public metadata graph Store: {error}"))?;
    let segment = snapshot
        .segments()
        .first()
        .ok_or_else(|| "public metadata graph Store has no sealed segment".to_owned())?;
    let id = segment.meta().id;
    let graph = segment
        .graph_node_blocks()
        .map_err(|error| format!("read public metadata graph nodes: {error}"))?;
    let entries = (0..graph.node_count())
        .filter_map(|row| {
            graph
                .block(row)
                .ok()
                .filter(|block| block.flags() & 1 != 0)
                .map(|_| row)
        })
        .collect::<Vec<_>>();
    if entries != vec![0, 1, 2, 3] {
        return Err(format!(
            "metadata graph equal-vector fixture expected entry rows [0, 1, 2, 3], observed {entries:?}"
        ));
    }
    drop(snapshot);
    store
        .close()
        .map_err(|error| format!("close public metadata graph Store: {error}"))?;
    let (mutations, reachable_nodes) =
        inspect_public_graph(&directory.join(id.file_name()), id, disconnected)?;
    Ok(GraphPublication {
        id,
        reachable_nodes,
        mutations,
    })
}

fn graph_options() -> SearchOptions {
    SearchOptions::new(ScanOptions { thread_budget: 1 }).with_tier(SearchTier::Graph(
        GraphSearchOptions::new(GraphSearchProfile::SiftClass).with_seed(0x0039_0039),
    ))
}

fn independent_graph_exact_results(
    segment_id: SegmentId,
    seed: u64,
    k: usize,
) -> Vec<MetadataResultFact> {
    (0..k.min(GRAPH_ROWS))
        .map(|row| MetadataResultFact {
            source: source_label(RowSource::Sealed(segment_id)),
            row_id: u32::try_from(row).unwrap_or(u32::MAX),
            document_id: Some(graph_document_id(seed, row).get()),
            score_bits: (-0.0_f32).to_bits(),
        })
        .collect()
}

struct ActiveThresholdEvidence {
    directory: TempDir,
    outcomes: Vec<(u64, FilteredSearchOutcome)>,
    receipts: Vec<MetadataExecutionReceipt>,
    expected: Vec<independent::I39ExpectedCase>,
}

fn active_threshold_i39(seed: u64, query_id: u64) -> Result<ActiveThresholdEvidence, String> {
    let directory = tempdir().map_err(|error| format!("metadata I39 tempdir: {error}"))?;
    let keep = ColumnId::new(1);
    let schema = Schema::new(vec![ColumnDefinition::new(
        keep,
        "keep",
        ColumnType::U64,
        false,
    )])
    .map_err(|error| format!("metadata I39 threshold schema: {error}"))?;
    let controller = Arc::new(MetadataTestController::new());
    let store = open_with_controller(
        directory.path(),
        OpenOptions::default().with_schema(schema),
        &controller,
    )?;
    let rows = usize::try_from(ALLOW_LIST_ROWS_THRESHOLD + 1)
        .map_err(|_| "metadata I39 threshold exceeds usize".to_owned())?;
    let documents = (0..rows)
        .map(|row| {
            IngestDocument::new(
                DocumentVersion::new(
                    DocId::new((u128::from(seed) << 32) + row as u128 + 1),
                    Revision::new(1),
                ),
                vec![row as f32, -(row as f32)],
            )
            .with_columns(vec![(
                keep,
                PredicateValue::U64(u64::from(row < rows.saturating_sub(1))),
            )])
        })
        .collect::<Vec<_>>();
    store
        .ingest(IngestBatch::new(documents))
        .map_err(|error| format!("ingest metadata I39 threshold rows: {error}"))?;
    let predicates = [
        Predicate::Eq {
            column: keep,
            value: PredicateValue::U64(1),
        },
        Predicate::And(Vec::new()),
    ];
    let mut outcomes = Vec::new();
    for (offset, predicate) in predicates.iter().enumerate() {
        let id = query_id.wrapping_add(offset as u64);
        controller
            .arm(MetadataTestArm::ObserveExecution { query_id: id })
            .map_err(|error| format!("arm metadata I39 threshold: {error}"))?;
        let outcome = store
            .search_filtered(
                SearchRequest::new(&[0.0, 0.0]),
                predicate,
                rows,
                SearchOptions::default().with_tier(SearchTier::Exact),
                QueryControl::Cancel(CancelToken::new()),
            )
            .map_err(|error| format!("metadata I39 threshold query: {error}"))?;
        outcomes.push((id, outcome));
    }
    let receipts = controller
        .drain_execution_receipts()
        .map_err(|error| format!("drain metadata I39 threshold receipts: {error}"))?;
    controller
        .assert_no_unconsumed_arm()
        .map_err(|error| format!("metadata I39 threshold arm leak: {error}"))?;
    store
        .close()
        .map_err(|error| format!("close metadata I39 threshold Store: {error}"))?;
    Ok(ActiveThresholdEvidence {
        directory,
        outcomes,
        receipts,
        expected: vec![
            expected_scan_case(
                query_id,
                RowSource::Active,
                INDEPENDENT_ALLOW_LIST_THRESHOLD + 1,
                INDEPENDENT_ALLOW_LIST_THRESHOLD,
                true,
                false,
            ),
            expected_scan_case(
                query_id.wrapping_add(1),
                RowSource::Active,
                INDEPENDENT_ALLOW_LIST_THRESHOLD + 1,
                INDEPENDENT_ALLOW_LIST_THRESHOLD + 1,
                true,
                false,
            ),
        ],
    })
}

fn run_execution(
    seed: u64,
    fault: Option<MetadataFaultKind>,
) -> Result<MetadataOperationEvidence, String> {
    let query_id = query_id_base(seed, MetadataOperationKind::Execution);
    let ActiveThresholdEvidence {
        directory: _threshold_directory,
        mut outcomes,
        receipts: mut execution_receipts,
        mut expected,
    } = active_threshold_i39(seed, query_id)?;
    let mut feature_receipts = Vec::new();
    let mut feature_expected = Vec::new();
    let mut control_override = None;
    let mut retained_product_override = None;
    if fault == Some(MetadataFaultKind::SelectivityBoundary) {
        let boundary = run_selectivity_boundary(seed, query_id.wrapping_add(16))?;
        outcomes.extend(boundary.outcomes);
        expected.extend(boundary.expected);
        execution_receipts.extend(boundary.execution);
        feature_receipts.extend(boundary.feature);
        feature_expected.extend(boundary.feature_expected);
        retained_product_override = Some(boundary.retained_product_fixture);
        control_override = Some(boundary.control);
    }

    let prune = build_typed_fixture(seed ^ 0x00f0, MetadataOperationKind::Execution)?;
    let prune_source = RowSource::Sealed(prune.segment_id);
    let clean_prune_query_id = query_id_base(seed ^ 0x00f0, MetadataOperationKind::Execution);
    outcomes.push((clean_prune_query_id, prune.clean_outcome));
    expected.push(expected_scan_case(
        clean_prune_query_id,
        prune_source,
        10,
        10,
        true,
        true,
    ));
    prune
        .controller
        .arm(MetadataTestArm::ObserveExecution {
            query_id: query_id.wrapping_add(3),
        })
        .map_err(|error| format!("arm metadata I39 prune: {error}"))?;
    let prune_store = open_with_controller(
        prune.directory.path(),
        OpenOptions::default().with_schema(prune.schema.clone()),
        &prune.controller,
    )?;
    let pruned = prune_store
        .search_filtered(
            SearchRequest::new(&[0.0, 0.0]),
            &Predicate::Eq {
                column: ColumnId::new(0),
                value: PredicateValue::I64(i64::MAX),
            },
            10,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| format!("metadata I39 prune query: {error}"))?;
    outcomes.push((query_id.wrapping_add(3), pruned));
    expected.push(expected_scan_case(
        query_id.wrapping_add(3),
        prune_source,
        10,
        0,
        false,
        true,
    ));
    prune_store
        .close()
        .map_err(|error| format!("close metadata I39 prune Store: {error}"))?;
    execution_receipts.extend(
        prune
            .controller
            .drain_execution_receipts()
            .map_err(|error| format!("drain metadata I39 prune receipts: {error}"))?,
    );

    let graph_directory = tempdir().map_err(|error| format!("metadata graph tempdir: {error}"))?;
    let graph_publication = publish_graph(graph_directory.path(), seed, false)?;
    if !graph_publication.mutations.is_empty() {
        return Err("clean public graph unexpectedly retained fixture mutations".to_owned());
    }
    let graph_id = graph_publication.id;
    let graph_reachable_nodes = graph_publication.reachable_nodes;
    let clean_graph_directory = copy_directory(graph_directory.path())?;
    let fault_graph_directory = copy_directory(graph_directory.path())?;
    let clean_graph_initial = directory_digest(clean_graph_directory.path())?;
    let fault_graph_initial = directory_digest(fault_graph_directory.path())?;
    if clean_graph_directory.path() == fault_graph_directory.path() {
        return Err("metadata graph clean/fault directories are not distinct".to_owned());
    }
    if clean_graph_initial != fault_graph_initial {
        return Err("metadata graph clean/fault directory copies differ".to_owned());
    }
    let graph_controller = Arc::new(MetadataTestController::new());
    let graph_store = open_with_controller(
        clean_graph_directory.path(),
        OpenOptions::default().with_epoch(graph_epoch()),
        &graph_controller,
    )?;
    let query = vec![0.0_f32; GRAPH_DIMS];
    let predicate = Predicate::And(Vec::new());
    graph_controller
        .arm(MetadataTestArm::ObserveExecution {
            query_id: query_id.wrapping_add(4),
        })
        .map_err(|error| format!("arm metadata I39 graph clean: {error}"))?;
    let graph_clean = graph_store
        .search_filtered(
            SearchRequest::new(&query),
            &predicate,
            3,
            graph_options(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| format!("metadata I39 graph clean query: {error}"))?;
    let clean_results = result_facts(&graph_clean);
    let retained_graph_results = clean_results.clone();
    outcomes.push((query_id.wrapping_add(4), graph_clean));
    expected.push(expected_graph_case(
        query_id.wrapping_add(4),
        RowSource::Sealed(graph_id),
        independent::FallbackReasonDto::None,
        GRAPH_EXPECTED_VISITED_BUDGET,
        graph_reachable_nodes,
    ));
    let generation = graph_store
        .snapshot()
        .map_err(|error| format!("snapshot metadata I39 graph: {error}"))?
        .generation();
    let wal = wal_digest(clean_graph_directory.path())?;
    let source_path = clean_graph_directory.path().join(graph_id.file_name());
    let source_digest = digest(
        &std::fs::read(&source_path)
            .map_err(|error| format!("read metadata I39 graph source: {error}"))?,
    );
    let mut fault_results = clean_results.clone();
    let mut retry_results = clean_results.clone();
    let mut fault_wal = wal;
    let mut retry_wal = wal;
    let mut fault_source = source_digest;
    let mut retry_source = source_digest;
    let mut fault_generation = generation;
    let mut retry_generation = generation;
    graph_store
        .close()
        .map_err(|error| format!("close metadata I39 graph clean Store: {error}"))?;
    execution_receipts.extend(
        graph_controller
            .drain_execution_receipts()
            .map_err(|error| format!("drain metadata I39 graph clean receipts: {error}"))?,
    );
    graph_controller
        .assert_no_unconsumed_arm()
        .map_err(|error| format!("metadata I39 graph clean arm leak: {error}"))?;
    let fault_graph_controller = Arc::new(MetadataTestController::new());
    let mut retained_fault_leg = None;
    let mut retained_retry_leg = None;
    if fault == Some(MetadataFaultKind::VisitedBudgetFallback) {
        let fault_graph_store = open_with_controller(
            fault_graph_directory.path(),
            OpenOptions::default().with_epoch(graph_epoch()),
            &fault_graph_controller,
        )?;
        let visited_arm = MetadataTestArm::VisitedBudgetFallback {
            query_id: query_id.wrapping_add(5),
            budget: 1,
        };
        fault_graph_controller
            .arm(visited_arm.clone())
            .map_err(|error| format!("arm metadata I39 visited budget: {error}"))?;
        let graph_fault = fault_graph_store
            .search_filtered(
                SearchRequest::new(&query),
                &predicate,
                3,
                graph_options(),
                QueryControl::Cancel(CancelToken::new()),
            )
            .map_err(|error| format!("metadata I39 visited fallback: {error}"))?;
        fault_generation = graph_fault.generation;
        fault_results = result_facts(&graph_fault);
        fault_wal = wal_digest(fault_graph_directory.path())?;
        fault_source = digest(
            &std::fs::read(fault_graph_directory.path().join(graph_id.file_name()))
                .map_err(|error| format!("read metadata graph after fault: {error}"))?,
        );
        outcomes.push((query_id.wrapping_add(5), graph_fault));
        expected.push(expected_graph_case(
            query_id.wrapping_add(5),
            RowSource::Sealed(graph_id),
            independent::FallbackReasonDto::VisitedBudget,
            1,
            graph_reachable_nodes,
        ));
        feature_expected.push(MetadataFeatureExpected::VisitedBudgetFallback {
            query_id: query_id.wrapping_add(5),
            source: source_label(RowSource::Sealed(graph_id)),
            operation: "metadata_execution_truth",
            fault: "visited-budget-fallback",
            site: "planner.exec.filtered-graph.visited-budget-fallback",
            cardinality: 1,
            visited: 2,
            budget: 1,
            filter_cardinality: GRAPH_ROWS as u64,
            exact_rows_examined: GRAPH_ROWS as u64,
            returned: GRAPH_ROWS as u64,
            reason: independent::FallbackReasonDto::VisitedBudget,
            effect: format!(
                "source={:?} visited=2 budget=1 filter_cardinality={} exact_rows_examined={} returned={} reason=VisitedBudget",
                RowSource::Sealed(graph_id),
                GRAPH_ROWS,
                GRAPH_ROWS,
                GRAPH_ROWS,
            ),
        });
        let retry_arm = MetadataTestArm::ObserveExecution {
            query_id: query_id.wrapping_add(6),
        };
        fault_graph_controller
            .arm(retry_arm.clone())
            .map_err(|error| format!("arm metadata I39 graph retry: {error}"))?;
        let graph_retry = fault_graph_store
            .search_filtered(
                SearchRequest::new(&query),
                &predicate,
                3,
                graph_options(),
                QueryControl::Cancel(CancelToken::new()),
            )
            .map_err(|error| format!("metadata I39 graph retry: {error}"))?;
        retry_generation = graph_retry.generation;
        retry_results = result_facts(&graph_retry);
        retry_wal = wal_digest(fault_graph_directory.path())?;
        retry_source = digest(
            &std::fs::read(fault_graph_directory.path().join(graph_id.file_name()))
                .map_err(|error| format!("read metadata graph after retry: {error}"))?,
        );
        outcomes.push((query_id.wrapping_add(6), graph_retry));
        expected.push(expected_graph_case(
            query_id.wrapping_add(6),
            RowSource::Sealed(graph_id),
            independent::FallbackReasonDto::None,
            GRAPH_EXPECTED_VISITED_BUDGET,
            graph_reachable_nodes,
        ));
        if clean_results != fault_results || clean_results != retry_results {
            return Err("metadata graph clean/fault/retry results differ".to_owned());
        }
        fault_graph_store
            .close()
            .map_err(|error| format!("close metadata I39 graph fault Store: {error}"))?;
        retained_fault_leg = Some(capture_metadata_retained_product_leg(
            fault_graph_directory.path(),
            MetadataRetainedPhase::Fault,
            vec![0.0_f32.to_bits(); GRAPH_DIMS],
            3,
            MetadataRetainedSearchTier::Graph,
            Some(visited_arm),
            MetadataRetainedExpectedOutcome::Results(fault_results.clone()),
        )?);
        retained_retry_leg = Some(capture_metadata_retained_product_leg(
            fault_graph_directory.path(),
            MetadataRetainedPhase::Retry,
            vec![0.0_f32.to_bits(); GRAPH_DIMS],
            3,
            MetadataRetainedSearchTier::Graph,
            Some(retry_arm),
            MetadataRetainedExpectedOutcome::Results(retry_results.clone()),
        )?);
    }
    execution_receipts.extend(
        fault_graph_controller
            .drain_execution_receipts()
            .map_err(|error| format!("drain metadata I39 graph fault receipts: {error}"))?,
    );
    feature_receipts.extend(
        fault_graph_controller
            .drain_feature_receipts()
            .map_err(|error| format!("drain metadata I39 graph feature receipts: {error}"))?,
    );
    let expected_feature_receipts = match fault {
        Some(MetadataFaultKind::SelectivityBoundary) => 2,
        Some(MetadataFaultKind::VisitedBudgetFallback) => 1,
        _ => 0,
    };
    if feature_receipts.len() != expected_feature_receipts {
        return Err(format!(
            "metadata execution expected {expected_feature_receipts} production feature receipts, observed {}",
            feature_receipts.len()
        ));
    }
    fault_graph_controller
        .assert_no_unconsumed_arm()
        .map_err(|error| format!("metadata I39 graph arm leak: {error}"))?;

    let shortfall_directory =
        tempdir().map_err(|error| format!("metadata shortfall tempdir: {error}"))?;
    let shortfall_publication = publish_graph(shortfall_directory.path(), seed, true)?;
    let shortfall_id = shortfall_publication.id;
    if shortfall_publication.mutations.len() != GRAPH_ROWS {
        return Err(format!(
            "metadata shortfall graph expected {GRAPH_ROWS} adjacency mutations, observed {}",
            shortfall_publication.mutations.len()
        ));
    }
    if shortfall_publication.reachable_nodes != 4 {
        return Err(format!(
            "metadata shortfall graph expected four reachable entry nodes, observed {}",
            shortfall_publication.reachable_nodes
        ));
    }
    let shortfall_controller = Arc::new(MetadataTestController::new());
    let shortfall_store = open_with_controller(
        shortfall_directory.path(),
        OpenOptions::default().with_epoch(graph_epoch()),
        &shortfall_controller,
    )?;
    shortfall_controller
        .arm(MetadataTestArm::ObserveExecution {
            query_id: query_id.wrapping_add(7),
        })
        .map_err(|error| format!("arm metadata I39 shortfall: {error}"))?;
    let shortfall = shortfall_store
        .search_filtered(
            SearchRequest::new(&query),
            &predicate,
            10,
            graph_options(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| format!("metadata I39 candidate shortfall: {error}"))?;
    outcomes.push((query_id.wrapping_add(7), shortfall));
    expected.push(expected_graph_case(
        query_id.wrapping_add(7),
        RowSource::Sealed(shortfall_id),
        independent::FallbackReasonDto::CandidateShortfall,
        GRAPH_EXPECTED_VISITED_BUDGET,
        shortfall_publication.reachable_nodes,
    ));
    shortfall_store
        .close()
        .map_err(|error| format!("close metadata I39 shortfall Store: {error}"))?;
    execution_receipts.extend(
        shortfall_controller
            .drain_execution_receipts()
            .map_err(|error| format!("drain metadata I39 shortfall receipts: {error}"))?,
    );
    shortfall_controller
        .assert_no_unconsumed_arm()
        .map_err(|error| format!("metadata I39 shortfall arm leak: {error}"))?;

    let reports = outcomes
        .iter()
        .flat_map(|(id, outcome)| outcome.plans.iter().map(|plan| report(*id, plan)))
        .collect();
    let diagnostics_reports = outcomes
        .iter()
        .flat_map(|(id, outcome)| {
            outcome
                .diagnostics
                .plan
                .iter()
                .map(|plan| report(*id, plan))
        })
        .collect();
    let observed_receipts = execution_receipts.iter().map(execution_receipt).collect();
    let control = control_override.unwrap_or(MetadataControlEvidence {
        namespace: "metadata_execution_truth",
        seed,
        query_id_base: query_id,
        normalized_schedule_digest: 0,
        directory_relation: MetadataDirectoryRelation::DistinctByteIdentical,
        outcome: if fault == Some(MetadataFaultKind::VisitedBudgetFallback) {
            MetadataControlOutcome::GraphFaultFallbackRetryEquivalent
        } else {
            MetadataControlOutcome::Only
        },
        clean_results,
        fault_results,
        retry_results,
        independent_expected_results: independent_graph_exact_results(graph_id, seed, 3),
        fault_error: None,
        clean_generation: generation,
        fault_generation,
        retry_generation,
        clean_wal_digest: wal,
        fault_wal_digest: fault_wal,
        retry_wal_digest: retry_wal,
        clean_source_digest: source_digest,
        fault_source_digest: fault_source,
        retry_source_digest: retry_source,
        clean_initial_directory: clean_graph_initial,
        fault_initial_directory: fault_graph_initial,
    });
    let fixture = MetadataFixtureEvidence::Execution(expected.clone());
    let queries = expected.iter().map(execution_query_evidence).collect();
    let mut retained_product_fixture = capture_metadata_retained_product_fixture(
        clean_graph_directory.path(),
        vec![0.0_f32.to_bits(); GRAPH_DIMS],
        3,
        MetadataRetainedSearchTier::Graph,
        retained_graph_results,
    )?;
    if let Some(fault_leg) = retained_fault_leg {
        retained_product_fixture.legs.push(fault_leg);
    }
    if let Some(retry_leg) = retained_retry_leg {
        retained_product_fixture.legs.push(retry_leg);
    }
    if let Some(boundary_fixture) = retained_product_override {
        retained_product_fixture = boundary_fixture;
    }
    Ok(MetadataOperationEvidence {
        operation: MetadataOperationKind::Execution,
        fault,
        invariant: MetadataInvariantEvidence::I39 {
            expected,
            observed: independent::I39Observed {
                reports,
                diagnostics_reports,
                receipts: observed_receipts,
                allow_list_threshold: ALLOW_LIST_ROWS_THRESHOLD,
            },
        },
        execution_receipts,
        feature_receipts,
        feature_expected,
        fixture,
        queries,
        control,
        mutation: None,
        fixture_mutations: shortfall_publication.mutations,
        retained_product_fixture: Some(retained_product_fixture),
    })
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[test]
    fn i37_case_identity_round_trips_every_matrix_cell_and_fault() {
        for seed in 0..I37_PREDICATE_CASE_COUNT {
            for fault in [None, Some("bitmap-truncation"), Some("column-corruption")] {
                let identity = i37_case_identity(9, seed, fault);
                assert_eq!(
                    i37_case_key_from_identity(&identity),
                    Some(i37_predicate_case_key(seed)),
                    "{identity}"
                );
            }
        }
        assert_eq!(i37_case_key_from_identity("eq-u64"), None);
        assert_eq!(i37_case_key_from_identity("op-9-eq-u64"), None);
        assert_eq!(i37_case_key_from_identity("op-9-unknown-fault-none"), None);
    }

    fn predicate_shapes(predicate: &Predicate, shapes: &mut BTreeSet<&'static str>) {
        let shape = match predicate {
            Predicate::Eq { .. } => "eq",
            Predicate::In { .. } => "in",
            Predicate::Range(_) => "range",
            Predicate::Exists(_) => "exists",
            Predicate::IsNull(_) => "is-null",
            Predicate::And(children) => {
                for child in children {
                    predicate_shapes(child, shapes);
                }
                "and"
            }
            Predicate::Or(children) => {
                for child in children {
                    predicate_shapes(child, shapes);
                }
                "or"
            }
            Predicate::Not(child) => {
                predicate_shapes(child, shapes);
                "not"
            }
        };
        shapes.insert(shape);
    }

    fn predicate_matrix_contracts(predicate: &Predicate, contracts: &mut BTreeSet<String>) {
        match predicate {
            Predicate::And(children) if children.is_empty() => {
                contracts.insert("and-empty".to_owned());
            }
            Predicate::And(children)
                if children
                    .iter()
                    .any(|child| matches!(child, Predicate::Or(_))) =>
            {
                contracts.insert("and-nested-or".to_owned());
            }
            Predicate::Or(children) if children.is_empty() => {
                contracts.insert("or-empty".to_owned());
            }
            Predicate::Not(child) if matches!(child.as_ref(), Predicate::Not(_)) => {
                contracts.insert("not-double".to_owned());
            }
            Predicate::Not(child) if matches!(child.as_ref(), Predicate::IsNull(column) if column.get() == 6) =>
            {
                contracts.insert("not-live-scope".to_owned());
            }
            _ => {}
        }
        match predicate {
            Predicate::Eq { column, .. } => {
                contracts.insert(format!("eq-column-{}", column.get()));
            }
            Predicate::In { values, .. } => {
                contracts.insert(format!("in-cardinality-{}", values.len()));
                if values
                    .iter()
                    .enumerate()
                    .any(|(index, value)| values[..index].contains(value))
                {
                    contracts.insert("in-duplicates".to_owned());
                }
            }
            Predicate::Range(range) => {
                let kind = match range.column.get() {
                    1 => "u64",
                    2 => "i64",
                    3 => "f64",
                    other => panic!("unexpected campaign Range column {other}"),
                };
                let bound = |bound: &Option<RangeBound>| match bound {
                    None => "unbounded",
                    Some(bound) if bound.inclusive => "inclusive",
                    Some(_) => "exclusive",
                };
                contracts.insert(format!(
                    "range-{kind}-lower-{}-upper-{}",
                    bound(&range.lower),
                    bound(&range.upper),
                ));
                if range.lower.as_ref().is_some_and(
                    |bound| matches!(bound.value, PredicateValue::F64(value) if value.is_nan()),
                ) {
                    contracts.insert("range-f64-nan-lower".to_owned());
                }
                if range.upper.as_ref().is_some_and(
                    |bound| matches!(bound.value, PredicateValue::F64(value) if value.is_nan()),
                ) {
                    contracts.insert("range-f64-nan-upper".to_owned());
                }
            }
            Predicate::Exists(_) | Predicate::IsNull(_) => {}
            Predicate::And(children) | Predicate::Or(children) => {
                for child in children {
                    predicate_matrix_contracts(child, contracts);
                }
            }
            Predicate::Not(child) => predicate_matrix_contracts(child, contracts),
        }
    }

    #[test]
    fn typed_fixture_seed_cycle_covers_nullable_extremes_and_string_shapes() {
        let rows = (0..24_u64).map(typed_rows).collect::<Vec<_>>();
        let has_first_null = rows.iter().any(|fixture| {
            fixture.first().is_some_and(|row| {
                row.values()
                    .any(|cell| *cell == independent::ScalarCell::Null)
            })
        });
        let has_middle_null = rows.iter().any(|fixture| {
            fixture.get(5).is_some_and(|row| {
                row.values()
                    .any(|cell| *cell == independent::ScalarCell::Null)
            })
        });
        let has_all_null_dictionary = rows.iter().any(|fixture| {
            fixture
                .iter()
                .all(|row| row.get(&5) == Some(&independent::ScalarCell::Null))
        });
        let has_empty_dictionary = rows
            .iter()
            .flatten()
            .any(|row| row.get(&5) == Some(&independent::ScalarCell::Utf8(Vec::new())));
        let has_empty_raw = rows
            .iter()
            .flatten()
            .any(|row| row.get(&6) == Some(&independent::ScalarCell::Utf8(Vec::new())));
        let float_bits = rows
            .iter()
            .flatten()
            .filter_map(|row| match row.get(&3) {
                Some(independent::ScalarCell::F64Bits(bits)) => Some(*bits),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        assert!(has_first_null, "seed grammar lacks a first-row null");
        assert!(has_middle_null, "seed grammar lacks a middle-row null");
        assert!(
            has_all_null_dictionary,
            "seed grammar lacks an all-null nullable dictionary"
        );
        assert!(
            has_empty_dictionary,
            "seed grammar lacks an empty dictionary value"
        );
        assert!(has_empty_raw, "seed grammar lacks an empty raw string");
        for bits in [
            (-0.0_f64).to_bits(),
            f64::INFINITY.to_bits(),
            f64::NEG_INFINITY.to_bits(),
            0x7ff8_0000_0000_0001,
            0x7ff8_0000_0000_0002,
        ] {
            assert!(
                float_bits.contains(&bits),
                "seed grammar lacks F64 bits {bits:#018x}"
            );
        }
    }

    #[test]
    fn bitmap_operation_exposes_complete_45_case_public_matrix() {
        let matrix = run_i37_complete_public_matrix(12)
            .expect("run one complete public bitmap matrix operation");
        assert_eq!(
            matrix.len(),
            usize::try_from(I37_PREDICATE_CASE_COUNT).expect("literal matrix count fits usize"),
            "one Bitmap operation must expose every I37 matrix case"
        );
        let expected_keys = (0..I37_PREDICATE_CASE_COUNT)
            .map(i37_predicate_case_key)
            .collect::<BTreeSet<_>>();
        let observed_keys = matrix
            .iter()
            .map(|case| case.coverage_key)
            .collect::<BTreeSet<_>>();
        assert_eq!(observed_keys, expected_keys);
        for (expected_index, case) in (0..I37_PREDICATE_CASE_COUNT).zip(matrix) {
            assert_eq!(case.case_index, expected_index);
            assert_eq!(
                case.coverage_key,
                i37_predicate_case_key(case.evidence.control.seed)
            );
            assert_eq!(case.evidence.operation, MetadataOperationKind::Bitmap);
            assert_eq!(case.evidence.fault, None);
            assert!(
                !case.evidence.execution_receipts.is_empty(),
                "I37 case {expected_index} lacks a production execution receipt"
            );
            let MetadataInvariantEvidence::I37 { input, observed } = &case.evidence.invariant
            else {
                panic!("I37 case {expected_index} returned the wrong invariant");
            };
            independent::compare_i37(input, observed)
                .unwrap_or_else(|error| panic!("I37 case {expected_index} failed: {error}"));
        }
    }

    #[test]
    fn bitmap_campaign_seed_cycle_covers_the_structured_predicate_grammar() {
        let mut shapes = BTreeSet::new();
        let mut contracts = BTreeSet::new();
        let mut coverage_keys = BTreeSet::new();
        for seed in 0..I37_PREDICATE_CASE_COUNT {
            let predicate = operation_predicate(seed, MetadataOperationKind::Bitmap);
            predicate_shapes(&predicate, &mut shapes);
            predicate_matrix_contracts(&predicate, &mut contracts);
            coverage_keys.insert(i37_predicate_case_key(seed));
        }
        assert_eq!(coverage_keys.len(), 45, "I37 coverage keys must be unique");
        assert_eq!(
            shapes,
            BTreeSet::from(["and", "eq", "exists", "in", "is-null", "not", "or", "range"])
        );
        for required in [
            "eq-column-1",
            "eq-column-2",
            "eq-column-3",
            "eq-column-4",
            "eq-column-5",
            "eq-column-6",
            "in-cardinality-0",
            "in-cardinality-1",
            "in-duplicates",
            "range-u64-lower-unbounded-upper-unbounded",
            "range-u64-lower-inclusive-upper-unbounded",
            "range-u64-lower-exclusive-upper-unbounded",
            "range-u64-lower-unbounded-upper-inclusive",
            "range-u64-lower-unbounded-upper-exclusive",
            "range-u64-lower-inclusive-upper-inclusive",
            "range-u64-lower-inclusive-upper-exclusive",
            "range-u64-lower-exclusive-upper-inclusive",
            "range-u64-lower-exclusive-upper-exclusive",
            "range-i64-lower-unbounded-upper-unbounded",
            "range-i64-lower-inclusive-upper-unbounded",
            "range-i64-lower-exclusive-upper-unbounded",
            "range-i64-lower-unbounded-upper-inclusive",
            "range-i64-lower-unbounded-upper-exclusive",
            "range-i64-lower-inclusive-upper-inclusive",
            "range-i64-lower-inclusive-upper-exclusive",
            "range-i64-lower-exclusive-upper-inclusive",
            "range-i64-lower-exclusive-upper-exclusive",
            "range-f64-lower-unbounded-upper-unbounded",
            "range-f64-lower-inclusive-upper-unbounded",
            "range-f64-lower-exclusive-upper-unbounded",
            "range-f64-lower-unbounded-upper-inclusive",
            "range-f64-lower-unbounded-upper-exclusive",
            "range-f64-lower-inclusive-upper-inclusive",
            "range-f64-lower-inclusive-upper-exclusive",
            "range-f64-lower-exclusive-upper-inclusive",
            "range-f64-lower-exclusive-upper-exclusive",
            "range-f64-nan-lower",
            "range-f64-nan-upper",
            "and-empty",
            "and-nested-or",
            "or-empty",
            "not-double",
            "not-live-scope",
        ] {
            assert!(
                contracts.contains(required),
                "I37 campaign smoke matrix missing {required}; observed={contracts:?}"
            );
        }
    }

    #[test]
    fn bitmap_seed_cycle_public_results_match_independent_recursive_algebra() {
        for seed in 0..I37_PREDICATE_CASE_COUNT {
            let evidence = run_metadata_operation(MetadataOperationKind::Bitmap, seed, None)
                .unwrap_or_else(|error| panic!("run bitmap seed {seed}: {error}"));
            let MetadataInvariantEvidence::I37 { input, observed } = &evidence.invariant else {
                panic!("bitmap seed {seed} returned another invariant")
            };
            independent::compare_i37(input, observed)
                .unwrap_or_else(|error| panic!("bitmap seed {seed}: {error}"));
            assert!(matches!(
                evidence.fixture,
                MetadataFixtureEvidence::Bitmap(ref fixture) if fixture == input
            ));
            assert!(!evidence.queries.is_empty());
        }
    }

    #[test]
    fn i37_fixture_requires_active_sealed_and_publicly_tombstoned_source_receipts() {
        let evidence = run_metadata_operation(MetadataOperationKind::Bitmap, 0x37a0, None)
            .expect("run I37 lifecycle fixture");
        let MetadataInvariantEvidence::I37 { input, observed } = &evidence.invariant else {
            panic!("I37 lifecycle fixture returned another invariant")
        };
        assert!(
            input.sources.iter().any(|source| !source.sealed),
            "I37 fixture lacks an active source"
        );
        assert!(
            input.sources.iter().any(|source| source.sealed),
            "I37 fixture lacks a sealed source"
        );
        assert!(
            input
                .sources
                .iter()
                .any(|source| source.sealed && source.live.len() < source.rows.len()),
            "I37 fixture lacks a publicly tombstoned sealed source"
        );
        assert_eq!(
            observed.sources.len(),
            input.sources.len(),
            "I37 source observations are incomplete"
        );
        independent::compare_i37(input, observed).expect("compare I37 lifecycle sources");
    }

    #[test]
    fn typed_seed_cycle_round_trips_special_bits_and_all_null_dictionary() {
        for seed in 0..6_u64 {
            let evidence = run_metadata_operation(MetadataOperationKind::Columns, seed, None)
                .unwrap_or_else(|error| panic!("run Columns seed {seed}: {error}"));
            let MetadataInvariantEvidence::I36 { input, observed } = &evidence.invariant else {
                panic!("Columns seed {seed} returned another invariant")
            };
            independent::compare_i36(input, observed)
                .unwrap_or_else(|error| panic!("Columns seed {seed}: {error}"));
            assert!(evidence.feature_expected.is_empty());
        }
    }

    #[test]
    fn i36_fixture_observes_active_and_reopened_rows_for_each_nullability_mode() {
        let evidence = run_metadata_operation(MetadataOperationKind::Columns, 0x36a0, None)
            .expect("run I36 active/reopened fixture");
        let MetadataInvariantEvidence::I36 { input, observed } = &evidence.invariant else {
            panic!("I36 fixture returned another invariant")
        };
        let matrix = input
            .definitions
            .iter()
            .filter(|definition| definition.id != 0)
            .map(|definition| (definition.kind, definition.nullable))
            .collect::<Vec<_>>();
        for kind in [
            independent::ColumnKind::U64,
            independent::ColumnKind::I64,
            independent::ColumnKind::F64,
            independent::ColumnKind::Bool,
            independent::ColumnKind::DictionaryString,
            independent::ColumnKind::RawString,
        ] {
            assert!(
                matrix.contains(&(kind, true)),
                "I36 fixture lacks nullable {kind:?}"
            );
            assert!(
                matrix.contains(&(kind, false)),
                "I36 fixture lacks required {kind:?}"
            );
        }
        assert!(
            evidence
                .execution_receipts
                .iter()
                .any(|receipt| receipt.source == RowSource::Active),
            "I36 fixture lacks an active production execution receipt"
        );
        assert_eq!(
            observed.active_rows, input.rows,
            "I36 active public observation differs from the primitive fixture"
        );
        independent::compare_i36(input, observed).expect("compare I36 active/reopened fixture");
    }

    #[test]
    fn planner_seed_cycle_covers_equal_and_inclusive_exclusive_boundaries() {
        let mut predicates = BTreeSet::new();
        for seed in 0..3_u64 {
            let evidence = run_metadata_operation(MetadataOperationKind::Planner, seed, None)
                .unwrap_or_else(|error| panic!("run Planner seed {seed}: {error}"));
            let MetadataInvariantEvidence::I38 { input, observed } = &evidence.invariant else {
                panic!("Planner seed {seed} returned another invariant")
            };
            independent::compare_i38(input, observed)
                .unwrap_or_else(|error| panic!("Planner seed {seed}: {error}"));
            let label = match &input.predicate {
                independent::PredicateDto::Eq { .. } => "eq",
                independent::PredicateDto::Range {
                    lower: Some(lower),
                    upper: Some(upper),
                    ..
                } if lower.inclusive && upper.inclusive => "inclusive",
                independent::PredicateDto::Range {
                    lower: Some(lower),
                    upper: Some(upper),
                    ..
                } if !lower.inclusive && upper.inclusive => "exclusive-lower",
                predicate => panic!("unexpected Planner predicate {predicate:?}"),
            };
            predicates.insert(label);
        }
        assert_eq!(
            predicates,
            BTreeSet::from(["eq", "exclusive-lower", "inclusive"])
        );
    }

    #[test]
    fn i38_fixture_contains_an_all_tombstoned_sealed_source() {
        let evidence = run_metadata_operation(MetadataOperationKind::Planner, 0x3800, None)
            .expect("run I38 tombstone fixture");
        let MetadataInvariantEvidence::I38 { input, observed } = &evidence.invariant else {
            panic!("I38 tombstone fixture returned another invariant")
        };
        independent::compare_i38(input, observed).expect("compare I38 tombstone fixture");
        let sealed_sources = input
            .rows
            .iter()
            .map(|row| row.source.clone())
            .filter(|source| source != "active")
            .collect::<BTreeSet<_>>();
        let all_tombstoned = sealed_sources.iter().any(|source| {
            input.rows.iter().any(|row| &row.source == source)
                && !input
                    .live
                    .iter()
                    .any(|(live_source, _)| live_source == source)
        });
        assert!(
            all_tombstoned,
            "I38 fixture lacks an all-tombstoned sealed source"
        );
    }

    #[test]
    fn i38_fixture_requires_public_delete_empty_and_sealed_missing_bounds_sources() {
        let evidence = run_metadata_operation(MetadataOperationKind::Planner, 0x38d0, None)
            .expect("run I38 lifecycle fixture");
        let MetadataInvariantEvidence::I38 { input, observed } = &evidence.invariant else {
            panic!("I38 lifecycle fixture returned another invariant")
        };
        assert!(
            input
                .sources
                .iter()
                .any(|source| source.sealed && source.range == independent::SourceRangeDto::Empty),
            "I38 fixture lacks a sealed Empty source"
        );
        assert!(
            input
                .sources
                .iter()
                .any(|source| source.sealed
                    && source.range == independent::SourceRangeDto::Unstamped),
            "I38 fixture lacks a sealed missing-bounds source"
        );
        assert!(
            input.sources.iter().any(|source| !source.sealed),
            "I38 fixture lacks an active source"
        );
        independent::compare_i38(input, observed)
            .expect("I38 public-delete/source-lifecycle comparison");
    }

    #[test]
    fn i36_raw_acquisition_rejects_an_invalid_independent_segment_envelope() {
        let fixture = build_typed_fixture(0x3600, MetadataOperationKind::Columns)
            .expect("build checked metadata fixture");
        let mut invalid = fixture.clean_segment.clone();
        invalid[0] ^= 0xff;
        let error = match region_bounds(&invalid, 0) {
            Ok(_) => panic!("independent segment envelope accepted invalid magic"),
            Err(error) => error,
        };
        assert!(
            error.contains("segment envelope magic"),
            "independent segment envelope returned the wrong refusal: {error}"
        );
    }

    #[test]
    fn i36_adapter_does_not_acquire_raw_columns_through_segment_reader_region() {
        let source = include_str!("metadata_filter_planner.rs");
        let forbidden = [".region(", "RegionKind::Columns", ")"].concat();
        assert!(
            !source.contains(&forbidden),
            "I36 raw acquisition still calls SegmentReader::region"
        );
    }

    #[test]
    pub fn bitmap_truncation_requires_short_alive_region() {
        let evidence = run_metadata_operation(
            MetadataOperationKind::Bitmap,
            0x3700,
            Some(MetadataFaultKind::BitmapTruncation),
        )
        .expect("run bitmap adapter");
        let mutation = evidence.mutation.expect("bitmap mutation evidence");
        assert_eq!(mutation.region, RegionKind::Alive);
        assert_eq!(
            mutation.declared_bytes_after + 1,
            mutation.declared_bytes_before
        );
        assert_eq!(mutation.observed_bytes_after, 1);
        assert_eq!(evidence.feature_receipts.len(), 1);
        assert!(matches!(
            evidence.feature_receipts[0].detail,
            zeppelin_embed::planner::MetadataFeatureDetail::AliveBitmapTruncationRefused {
                declared_rows: 10,
                declared_bytes: 2,
                observed_bytes: 1,
                byte_offset: 8,
                provenance: MetadataDecodeProvenance::AliveBitmapTruncation {
                    row_count: 10,
                    byte_offset: 8,
                    declared_bytes: 2,
                    observed_bytes: 1,
                },
                ..
            }
        ));
        assert_eq!(
            evidence.control.clean_results,
            evidence.control.retry_results
        );
        assert_eq!(
            evidence.control.clean_results, evidence.control.independent_expected_results,
            "bitmap refusal control lacks an independent exact result model",
        );
        assert!(evidence.control.fault_results.is_empty());
    }

    #[test]
    fn operation_fault_mismatch_is_rejected_before_any_store_work() {
        let error = run_metadata_operation(
            MetadataOperationKind::Columns,
            1,
            Some(MetadataFaultKind::VisitedBudgetFallback),
        )
        .expect_err("cross-operation metadata fault must be rejected");
        assert!(error.contains("does not belong"));
    }

    #[test]
    fn every_metadata_fault_starts_from_byte_identical_nonempty_directory_copies() {
        for (operation, fault) in [
            (
                MetadataOperationKind::Columns,
                MetadataFaultKind::ColumnCorruption,
            ),
            (
                MetadataOperationKind::Bitmap,
                MetadataFaultKind::BitmapTruncation,
            ),
            (
                MetadataOperationKind::Execution,
                MetadataFaultKind::SelectivityBoundary,
            ),
            (
                MetadataOperationKind::Execution,
                MetadataFaultKind::VisitedBudgetFallback,
            ),
        ] {
            let evidence = run_metadata_operation(operation, 0x5eed, Some(fault))
                .unwrap_or_else(|error| panic!("run {fault:?}: {error}"));
            assert!(
                !evidence.control.clean_initial_directory.files.is_empty(),
                "metadata {fault:?} clean initial directory evidence is empty"
            );
            assert_eq!(
                evidence.control.clean_initial_directory, evidence.control.fault_initial_directory,
                "metadata {fault:?} clean/fault directories differ before the selected fault"
            );
        }
    }

    #[test]
    pub fn column_corruption_requires_decoder_receipt() {
        let mut provenances = BTreeSet::new();
        for seed in 0..3_u64 {
            let evidence = run_metadata_operation(
                MetadataOperationKind::Columns,
                seed,
                Some(MetadataFaultKind::ColumnCorruption),
            )
            .expect("run Columns adapter");
            assert!(matches!(
                evidence.invariant,
                MetadataInvariantEvidence::I36 { .. }
            ));
            assert_eq!(evidence.feature_receipts.len(), 1);
            let detail = &evidence.feature_receipts[0].detail;
            if let zeppelin_embed::planner::MetadataFeatureDetail::ColumnDecodeRefused {
                field_class,
                provenance,
                ..
            } = detail
            {
                provenances.insert(*field_class);
                assert!(matches!(
                    (field_class, provenance),
                    (
                        &"dictionary-code",
                        MetadataDecodeProvenance::ColumnsDictionaryCode { .. }
                    ) | (
                        &"raw-string-length",
                        MetadataDecodeProvenance::ColumnsRawStringLength { .. }
                    ) | (
                        &"presence-tail",
                        MetadataDecodeProvenance::ColumnsPresenceTail { .. }
                    )
                ));
            } else {
                panic!("Columns adapter emitted the wrong feature receipt: {detail:?}");
            }
            assert_eq!(
                evidence.control.clean_results,
                evidence.control.retry_results
            );
            assert_eq!(
                evidence.control.clean_results, evidence.control.independent_expected_results,
                "Columns refusal control lacks an independent exact result model",
            );
            assert!(evidence.control.fault_results.is_empty());
        }
        assert_eq!(
            provenances,
            BTreeSet::from(["dictionary-code", "presence-tail", "raw-string-length"])
        );
    }

    #[test]
    fn column_corruption_repairs_chunk_table_directory_header_and_file_checksums() {
        let fixture = build_typed_fixture(0, MetadataOperationKind::Columns)
            .expect("build metadata checked-byte fixture");
        let plant = column_plant(&fixture, 0, 0x36ff).expect("plant dictionary-code corruption");
        validate_rewritten_region_integrity(&plant.bytes, RegionKind::Columns)
            .expect("Columns semantic mutation must preserve every outer checksum layer");
    }

    #[test]
    pub fn selectivity_boundary_straddles_production_threshold() {
        let evidence = run_metadata_operation(
            MetadataOperationKind::Execution,
            0x3800,
            Some(MetadataFaultKind::SelectivityBoundary),
        )
        .expect("run execution boundary adapter");
        assert!(matches!(
            evidence.invariant,
            MetadataInvariantEvidence::I39 { .. }
        ));
        assert_eq!(evidence.feature_receipts.len(), 2);
        assert_eq!(
            evidence.control.clean_results,
            evidence.control.fault_results
        );
        assert_eq!(
            evidence.control.clean_results,
            evidence.control.retry_results
        );
        assert_eq!(
            evidence.control.clean_results, evidence.control.independent_expected_results,
            "selectivity control lacks an independent exact result model",
        );
    }

    #[test]
    fn i38_fixture_requires_active_and_three_sealed_sources_with_exact_ledgers() {
        let evidence = run_metadata_operation(MetadataOperationKind::Planner, 0x3800, None)
            .expect("run I38 adapter");
        let MetadataInvariantEvidence::I38 { input, observed } = evidence.invariant else {
            panic!("I38 adapter returned another invariant")
        };
        let sources = input
            .rows
            .iter()
            .map(|row| row.source.clone())
            .collect::<BTreeSet<_>>();
        assert!(
            sources.contains("active")
                && sources.iter().filter(|source| *source != "active").count() >= 3,
            "I38 fixture must include active plus at least three sealed sources; observed {sources:?}"
        );
        assert_eq!(observed.reports.len(), sources.len());
        assert_eq!(observed.execution_receipts.len(), sources.len());
    }

    #[test]
    pub fn visited_budget_fault_executes_query_fallback() {
        let evidence = run_metadata_operation(
            MetadataOperationKind::Execution,
            0x3900,
            Some(MetadataFaultKind::VisitedBudgetFallback),
        )
        .expect("run execution adapter");
        let MetadataInvariantEvidence::I39 { expected, observed } = &evidence.invariant else {
            panic!("execution adapter returned another invariant")
        };
        independent::compare_i39_expected(expected, observed)
            .unwrap_or_else(|error| panic!("{error}"));
        let branches = evidence
            .execution_receipts
            .iter()
            .map(|receipt| (receipt.branch, receipt.fallback))
            .collect::<Vec<_>>();
        for expected in [
            (SegmentBranch::Pruned, PlanFallback::None),
            (SegmentBranch::ExactAllowList, PlanFallback::None),
            (SegmentBranch::MaskedScan, PlanFallback::None),
            (SegmentBranch::FilteredGraph, PlanFallback::None),
            (
                SegmentBranch::GraphExactFallback,
                PlanFallback::VisitedBudget,
            ),
            (
                SegmentBranch::GraphExactFallback,
                PlanFallback::CandidateShortfall,
            ),
        ] {
            assert!(
                branches.contains(&expected),
                "missing I39 branch {expected:?}"
            );
        }
        assert_eq!(evidence.feature_receipts.len(), 1);
        assert_eq!(
            evidence.control.clean_results,
            evidence.control.fault_results
        );
        assert_eq!(
            evidence.control.clean_results,
            evidence.control.retry_results
        );
        assert!(
            !evidence.control.independent_expected_results.is_empty(),
            "visited-budget fallback lacks an independently derived exact baseline"
        );
        assert_eq!(
            evidence.control.fault_results, evidence.control.independent_expected_results,
            "visited-budget fallback output differs from independent exact baseline"
        );
    }

    #[test]
    fn candidate_shortfall_fixture_attests_every_graph_adjacency_mutation() {
        let evidence = run_metadata_operation(MetadataOperationKind::Execution, 0x3901, None)
            .expect("run I39 shortfall fixture");
        assert_eq!(
            evidence.fixture_mutations.len(),
            GRAPH_ROWS,
            "shortfall fixture must attest one GraphNodeBlocks adjacency mutation per row"
        );
        assert!(
            evidence
                .fixture_mutations
                .iter()
                .any(|mutation| mutation.before != mutation.after),
            "shortfall fixture did not mutate any adjacency bytes"
        );
        for (row, mutation) in evidence.fixture_mutations.iter().enumerate() {
            assert_eq!(mutation.region, RegionKind::GraphNodeBlocks);
            assert_eq!(
                mutation.absolute_offset,
                mutation.region_offset + mutation.field_offset,
                "row {row} absolute mutation offset is not independently reconstructible"
            );
            assert_eq!(mutation.before.len(), mutation.after.len());
            assert!(mutation.after.len() >= 8);
            assert_eq!(mutation.after[0], 0, "row {row} retained a graph degree");
            assert_eq!(
                mutation.after[1], mutation.before[1],
                "row {row} graph flags changed"
            );
            assert_eq!(&mutation.after[2..4], &[0, 0]);
            assert!(
                mutation.after[4..].iter().all(|byte| *byte == u8::MAX),
                "row {row} retained a graph neighbor"
            );
            assert_eq!(
                mutation.left_neighbor_after, mutation.left_neighbor_before,
                "row {row} changed the left boundary byte"
            );
            assert_eq!(
                mutation.right_neighbor_after, mutation.right_neighbor_before,
                "row {row} changed the right boundary byte"
            );
            assert_eq!(
                mutation.declared_bytes_after, mutation.declared_bytes_before,
                "row {row} changed the declared GraphNodeBlocks length"
            );
            assert_eq!(
                mutation.observed_bytes_after, mutation.declared_bytes_after,
                "row {row} changed the observed GraphNodeBlocks length"
            );
            if let Some(previous) = row
                .checked_sub(1)
                .and_then(|previous| evidence.fixture_mutations.get(previous))
            {
                assert!(
                    mutation.absolute_offset > previous.absolute_offset,
                    "row {row} mutation offsets are not strictly ordered"
                );
                assert_eq!(
                    mutation.field_offset - previous.field_offset,
                    mutation.absolute_offset - previous.absolute_offset,
                    "row {row} relative and absolute strides differ"
                );
            }
        }
    }

    #[test]
    fn metadata_replay_helpers_mutate_each_owned_canonical_input() {
        let evidence = run_metadata_operation(
            MetadataOperationKind::Columns,
            0,
            Some(MetadataFaultKind::ColumnCorruption),
        )
        .expect("build metadata replay mutation fixture");
        for mutation in [
            MetadataReplayMutation::OracleObserved,
            MetadataReplayMutation::ExecutionReceipt,
            MetadataReplayMutation::MutationByte,
            MetadataReplayMutation::SameSeedDigest,
        ] {
            let mutated = apply_metadata_replay_mutation(&evidence, mutation)
                .unwrap_or_else(|error| panic!("apply {mutation:?}: {error}"));
            assert_ne!(
                mutated, evidence,
                "metadata replay mutation helper left {mutation:?} unchanged"
            );
        }
    }

    #[test]
    fn metadata_control_records_normalized_operation_query_schedule_digest() {
        let evidence = run_metadata_operation(
            MetadataOperationKind::Columns,
            0,
            Some(MetadataFaultKind::ColumnCorruption),
        )
        .expect("run metadata schedule evidence fixture");
        assert_ne!(
            evidence.control.normalized_schedule_digest, 0,
            "metadata control missing normalized operation/query schedule digest"
        );
    }

    #[test]
    fn metadata_control_records_explicit_directory_relation_and_outcome() {
        for (operation, fault, expected_outcome) in [
            (
                MetadataOperationKind::Columns,
                MetadataFaultKind::ColumnCorruption,
                MetadataControlOutcome::FaultRefusedRetryEquivalent,
            ),
            (
                MetadataOperationKind::Bitmap,
                MetadataFaultKind::BitmapTruncation,
                MetadataControlOutcome::FaultRefusedRetryEquivalent,
            ),
            (
                MetadataOperationKind::Execution,
                MetadataFaultKind::SelectivityBoundary,
                MetadataControlOutcome::FaultAndRetryEquivalent,
            ),
            (
                MetadataOperationKind::Execution,
                MetadataFaultKind::VisitedBudgetFallback,
                MetadataControlOutcome::GraphFaultFallbackRetryEquivalent,
            ),
        ] {
            let evidence = run_metadata_operation(operation, 0, Some(fault))
                .expect("run metadata directory relation fixture");
            assert_eq!(
                evidence.control.directory_relation,
                MetadataDirectoryRelation::DistinctByteIdentical,
                "metadata control missing explicit distinct clean/fault directory relation"
            );
            assert_eq!(
                evidence.control.clean_initial_directory, evidence.control.fault_initial_directory,
                "metadata same-seed directory relation is not byte-identical"
            );
            assert_eq!(
                evidence.control.outcome, expected_outcome,
                "metadata control missing exact clean/fault/retry outcome"
            );
        }
        let planner = run_metadata_operation(MetadataOperationKind::Planner, 0, None)
            .expect("run unpaired metadata planner control");
        assert_eq!(
            planner.control.directory_relation,
            MetadataDirectoryRelation::UnpairedSingleDirectory
        );
        assert_eq!(
            planner.control.outcome,
            MetadataControlOutcome::FilteredSucceeded
        );
    }

    #[test]
    fn selected_metadata_mutation_records_every_checksum_rewrite_and_final_digest() {
        for (operation, fault, region) in [
            (
                MetadataOperationKind::Columns,
                MetadataFaultKind::ColumnCorruption,
                RegionKind::Columns,
            ),
            (
                MetadataOperationKind::Bitmap,
                MetadataFaultKind::BitmapTruncation,
                RegionKind::Alive,
            ),
        ] {
            let evidence = run_metadata_operation(operation, 0, Some(fault))
                .expect("run selected metadata mutation fixture");
            let mutation = evidence
                .mutation
                .expect("catalog fault selected one artifact mutation");
            assert_eq!(
                mutation
                    .checksum_rewrites
                    .iter()
                    .map(|rewrite| rewrite.field.clone())
                    .collect::<Vec<_>>(),
                vec![
                    MetadataChecksumField::TargetRegion { region },
                    MetadataChecksumField::TargetRegionChunk {
                        region,
                        chunk_index: 0,
                    },
                    MetadataChecksumField::ChecksumTableRegion,
                    MetadataChecksumField::SegmentHeader,
                    MetadataChecksumField::SegmentWholeFile,
                ],
                "selected metadata mutation checksum rewrite ledger is incomplete or reordered"
            );
            assert_eq!(
                mutation.post_mutation_artifact_digest, evidence.control.fault_source_digest,
                "selected metadata mutation missing post-mutation artifact digest"
            );
        }
    }

    #[test]
    fn graph_fixture_mutations_record_every_checksum_rewrite_and_final_digest() {
        let evidence = run_metadata_operation(MetadataOperationKind::Execution, 0x3901, None)
            .expect("run graph fixture mutation evidence");
        let expected_digest = evidence
            .fixture_mutations
            .first()
            .expect("graph fixture has mutations")
            .post_mutation_artifact_digest;
        assert_ne!(
            expected_digest, 0,
            "fixture mutation missing post-mutation artifact digest"
        );
        for mutation in &evidence.fixture_mutations {
            assert_eq!(
                mutation
                    .checksum_rewrites
                    .iter()
                    .map(|rewrite| rewrite.field.clone())
                    .collect::<Vec<_>>(),
                vec![
                    MetadataChecksumField::GraphInternal,
                    MetadataChecksumField::TargetRegion {
                        region: RegionKind::GraphNodeBlocks,
                    },
                    MetadataChecksumField::TargetRegionChunk {
                        region: RegionKind::GraphNodeBlocks,
                        chunk_index: 0,
                    },
                    MetadataChecksumField::ChecksumTableRegion,
                    MetadataChecksumField::SegmentHeader,
                    MetadataChecksumField::SegmentWholeFile,
                ],
                "fixture mutation checksum rewrite ledger is incomplete or reordered"
            );
            assert_eq!(
                mutation.post_mutation_artifact_digest, expected_digest,
                "fixture mutations disagree on the final artifact digest"
            );
        }
    }

    #[test]
    fn metadata_retained_fixture_replays_all_four_exact_checkers_without_seed_generation() {
        for operation in [
            MetadataOperationKind::Columns,
            MetadataOperationKind::Bitmap,
            MetadataOperationKind::Planner,
            MetadataOperationKind::Execution,
        ] {
            let evidence = run_metadata_operation(operation, 0x5eed, None)
                .expect("observe metadata retained fixture");
            let retained =
                encode_metadata_fixture(&evidence).expect("encode metadata retained fixture");
            let replayed = run_metadata_operation_from_fixture(&retained)
                .expect("replay metadata retained fixture");
            assert_eq!(replayed.operation, operation);
            assert_eq!(replayed.seed, evidence.control.seed);
            assert!(replayed.replay.first_difference.is_none());
        }
    }

    #[test]
    fn metadata_retained_columns_fixture_reopens_and_runs_the_public_query() {
        let evidence = run_metadata_operation(MetadataOperationKind::Columns, 0x36ee, None)
            .expect("observe metadata retained Columns fixture");
        let retained =
            encode_metadata_fixture(&evidence).expect("encode metadata retained Columns fixture");
        let replayed = run_metadata_operation_from_fixture(&retained)
            .expect("replay metadata retained Columns fixture");
        assert_eq!(
            replayed.public_results, evidence.control.clean_results,
            "retained metadata Columns fixture did not rerun the public Store query"
        );
    }

    #[test]
    fn metadata_retained_columns_fault_executes_the_selected_fault_leg() {
        let evidence = run_metadata_operation(
            MetadataOperationKind::Columns,
            0x36ef,
            Some(MetadataFaultKind::ColumnCorruption),
        )
        .expect("observe selected metadata Columns fault fixture");
        let retained = encode_metadata_fixture(&evidence)
            .expect("encode selected metadata Columns fault fixture");
        let replayed = run_metadata_operation_from_fixture(&retained)
            .expect("replay selected metadata Columns fault fixture");
        assert_eq!(
            replayed.public_results, evidence.control.fault_results,
            "selected metadata Columns fault retained replay executed the clean Store leg"
        );
        assert_eq!(
            replayed
                .public_legs
                .iter()
                .map(|leg| leg.phase)
                .collect::<Vec<_>>(),
            vec![
                MetadataRetainedPhase::Clean,
                MetadataRetainedPhase::Fault,
                MetadataRetainedPhase::Retry,
            ],
            "selected metadata Columns fault retained replay omitted an ordered control leg"
        );
        assert_eq!(
            replayed.public_legs[1].feature_receipts, evidence.feature_receipts,
            "selected metadata Columns fault retained replay did not emit the exact production receipt"
        );
        assert_eq!(
            replayed.public_legs[2].outcome,
            MetadataRetainedExpectedOutcome::Results(evidence.control.retry_results.clone()),
            "selected metadata Columns fault retained retry differs from the recorded public retry"
        );
    }

    #[test]
    fn metadata_retained_bitmap_fixture_uses_the_literal_predicate_not_seed_generation() {
        let evidence = run_metadata_operation(MetadataOperationKind::Bitmap, 0, None)
            .expect("observe metadata retained Bitmap fixture");
        let mut retained =
            encode_metadata_fixture(&evidence).expect("encode metadata retained Bitmap fixture");
        retained[28..36].copy_from_slice(&u64::MAX.to_le_bytes());
        let payload_digest = retained_metadata_payload_digest(&retained[24..]);
        retained[16..24].copy_from_slice(&payload_digest.to_le_bytes());

        let replayed = run_metadata_operation_from_fixture(&retained)
            .expect("replay metadata retained Bitmap fixture");
        assert_eq!(replayed.seed, u64::MAX, "test did not change seed metadata");
        assert_eq!(
            replayed.public_results, evidence.control.clean_results,
            "retained metadata Bitmap fixture regenerated its predicate from seed metadata"
        );
    }

    #[test]
    fn metadata_retained_bitmap_fault_executes_the_selected_fault_leg() {
        let evidence = run_metadata_operation(
            MetadataOperationKind::Bitmap,
            41,
            Some(MetadataFaultKind::BitmapTruncation),
        )
        .expect("observe selected metadata Bitmap fault fixture");
        let retained = encode_metadata_fixture(&evidence)
            .expect("encode selected metadata Bitmap fault fixture");
        let replayed = run_metadata_operation_from_fixture(&retained)
            .expect("replay selected metadata Bitmap fault fixture");
        assert_eq!(
            replayed.public_results, evidence.control.fault_results,
            "selected metadata Bitmap fault retained replay executed the clean Store leg"
        );
        assert_eq!(
            replayed
                .public_legs
                .iter()
                .map(|leg| leg.phase)
                .collect::<Vec<_>>(),
            vec![
                MetadataRetainedPhase::Clean,
                MetadataRetainedPhase::Fault,
                MetadataRetainedPhase::Retry,
            ],
            "selected metadata Bitmap fault retained replay omitted an ordered control leg"
        );
        assert_eq!(
            replayed.public_legs[1].feature_receipts, evidence.feature_receipts,
            "selected metadata Bitmap fault retained replay did not emit the exact production receipt"
        );
    }

    #[test]
    fn metadata_retained_planner_fixture_uses_literal_multisegment_store_and_predicate() {
        let evidence = run_metadata_operation(MetadataOperationKind::Planner, 0x3800, None)
            .expect("observe metadata retained Planner fixture");
        let retained =
            encode_metadata_fixture(&evidence).expect("encode metadata retained Planner fixture");
        let replayed = run_metadata_operation_from_fixture(&retained)
            .expect("replay metadata retained Planner fixture");
        assert_eq!(
            replayed.public_results, evidence.control.clean_results,
            "retained metadata Planner fixture did not rerun the public filtered query"
        );
    }

    #[test]
    fn metadata_retained_execution_fixture_reopens_the_literal_graph_store() {
        let evidence = run_metadata_operation(MetadataOperationKind::Execution, 0x3900, None)
            .expect("observe metadata retained Execution fixture");
        let retained =
            encode_metadata_fixture(&evidence).expect("encode metadata retained Execution fixture");
        let replayed = run_metadata_operation_from_fixture(&retained)
            .expect("replay metadata retained Execution fixture");
        assert_eq!(
            replayed.public_results, evidence.control.clean_results,
            "retained metadata Execution fixture did not rerun the public graph query"
        );
    }

    #[test]
    fn metadata_retained_visited_budget_fault_executes_clean_fault_and_retry_legs() {
        let evidence = run_metadata_operation(
            MetadataOperationKind::Execution,
            0x39ef,
            Some(MetadataFaultKind::VisitedBudgetFallback),
        )
        .expect("observe selected metadata visited-budget fixture");
        let retained = encode_metadata_fixture(&evidence)
            .expect("encode selected metadata visited-budget fixture");
        let replayed = run_metadata_operation_from_fixture(&retained)
            .expect("replay selected metadata visited-budget fixture");
        assert_eq!(
            replayed
                .public_legs
                .iter()
                .map(|leg| leg.phase)
                .collect::<Vec<_>>(),
            vec![
                MetadataRetainedPhase::Clean,
                MetadataRetainedPhase::Fault,
                MetadataRetainedPhase::Retry,
            ],
            "selected metadata visited-budget replay omitted an ordered control leg"
        );
        assert_eq!(
            replayed.public_legs[1].feature_receipts, evidence.feature_receipts,
            "selected metadata visited-budget replay did not emit the exact production receipt"
        );
        assert_eq!(
            replayed.public_results, evidence.control.fault_results,
            "selected metadata visited-budget replay did not execute the fault query"
        );
    }

    #[test]
    fn metadata_retained_selectivity_fault_replays_both_boundary_queries_on_every_leg() {
        let evidence = run_metadata_operation(
            MetadataOperationKind::Execution,
            0x39f0,
            Some(MetadataFaultKind::SelectivityBoundary),
        )
        .expect("observe selected metadata selectivity fixture");
        let retained = encode_metadata_fixture(&evidence)
            .expect("encode selected metadata selectivity fixture");
        let replayed = run_metadata_operation_from_fixture(&retained)
            .expect("replay selected metadata selectivity fixture");
        assert_eq!(
            replayed
                .public_legs
                .iter()
                .map(|leg| leg.phase)
                .collect::<Vec<_>>(),
            vec![
                MetadataRetainedPhase::Clean,
                MetadataRetainedPhase::Clean,
                MetadataRetainedPhase::Fault,
                MetadataRetainedPhase::Fault,
                MetadataRetainedPhase::Retry,
                MetadataRetainedPhase::Retry,
            ],
            "selected metadata selectivity replay omitted a boundary query or control phase"
        );
        assert_eq!(
            replayed
                .public_legs
                .iter()
                .flat_map(|leg| leg.feature_receipts.iter())
                .cloned()
                .collect::<Vec<_>>(),
            evidence.feature_receipts,
            "selected metadata selectivity replay did not emit both exact production receipts"
        );
    }
}
