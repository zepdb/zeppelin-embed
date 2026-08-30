//! Family-owned vector-execution fixtures and observation adapters.
//!
//! This module deliberately does not dispatch campaign operations, invoke the
//! I24-I27 checkers, construct `OracleRecord`s, or manufacture feature-fault
//! receipts. It returns operation-scoped independent-oracle inputs and honest
//! observations, public Store control facts, and receipts drained directly
//! from the production fault controllers.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tempfile::{TempDir, tempdir};
use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, Normalization, StoreEpoch,
};
use zeppelin_embed::fts::tokenizer::TokenizerConfig;
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, IngestError, Revision,
    RowSource, SearchCandidate, SearchOutcome, SearchRequest,
};
use zeppelin_embed::kernels::vector_fault::{
    KernelFaultController, KernelOperationId, KernelScoreObservation, KernelScoreValue,
};
use zeppelin_embed::kernels::{Bit4Row, Bit4Rows4, KernelBackendId, KernelVariant};
use zeppelin_embed::lifecycle::{
    CancelToken, GraphSearchOptions, OpenOptions, QueryControl, QueryError, SearchOptions,
    SearchTier, Store, StoreError, StoreErrorKind, StoreTestDependencies, SystemMonotonicClock,
};
use zeppelin_embed::quant::{
    Bit4Factors, Int8Vec, QuantError, QuantScheme as ProductQuantScheme, RescoreError,
    RescoreMetric, RescorePool, dequantize_bit4, dequantize_int8, dot_int8_query, est_dot_bit4,
    prepare_bit4_query, prepare_int8_query, quantize_bit4, quantize_int8, rescore_top_k,
};
use zeppelin_embed::scan::vector_fault::{
    MissingRescoreSite, VectorCampaign, VectorFault, VectorFaultController, VectorFaultEffect,
    VectorFaultKind as ProductVectorFaultKind, VectorFaultReceipt, VectorFaultSite,
    VectorOperation as ProductVectorOperation, VectorRowSource, VectorSearchTier,
};
use zeppelin_embed::scan::{ScanError, ScanOptions};
use zeppelin_embed::segment::SegmentId;
use zeppelin_embed::tier::{MaintenanceBudget, MaintenanceStatus, TierThresholds};
use zeppelin_embed::vfs::StdVfs;
use zeppelin_embed_adversarial_oracle::vector_execution as independent;

use super::fault_vfs::{
    FaultEvent as ScheduledFaultEvent, FaultMode as ScheduledFaultMode,
    FaultSchedule as ScheduledFaultSchedule, FaultSite as ScheduledFaultSite,
    Layer as ScheduledFaultLayer, ScheduledVfs,
};
use super::runner::FrozenStoreFixture;

const QUERY: [f32; 3] = [1.0, -1.0, 0.5];
const ROWS: [[f32; 3]; 3] = [[1.0, -1.0, 0.5], [0.5, -0.5, 0.25], [-1.0, 1.0, -0.5]];
const VECTOR_CHILD_MODE: &str = "ZE_VECTOR_ADAPTER_CHILD_MODE";
const VECTOR_CHILD_DIRECTORY: &str = "ZE_VECTOR_ADAPTER_CHILD_DIRECTORY";
const VECTOR_CHILD_BACKEND: &str = "ZE_VECTOR_ADAPTER_CHILD_BACKEND";
const VECTOR_CHILD_CASE: &str = "ZE_VECTOR_ADAPTER_CHILD_CASE";
const VECTOR_CHILD_OUTPUT: &str = "ZE_VECTOR_ADAPTER_CHILD_OUTPUT";

/// Exact ignored test path self-spawned for a process-global forced dispatch.
pub const FORCED_BACKEND_CHILD_TEST_NAME: &str =
    "adversarial::vector_execution::tests::forced_backend_store_child";

/// One vector campaign operation, independent of shared campaign dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VectorOperationKind {
    KernelParity,
    Quantization,
    Rescore,
    RowIdentity,
}

impl VectorOperationKind {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::KernelParity => "kernel-parity",
            Self::Quantization => "quantization",
            Self::Rescore => "rescore",
            Self::RowIdentity => "row-identity",
        }
    }
}

/// One catalogued vector fault, valid for exactly one operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VectorFaultKind {
    ForcedDispatchBackend,
    CorruptCodesFactors,
    MissingRescoreRows,
    RowCountCancellation,
    AllocationDenial,
}

impl VectorFaultKind {
    const fn operation(self) -> VectorOperationKind {
        match self {
            Self::ForcedDispatchBackend => VectorOperationKind::KernelParity,
            Self::CorruptCodesFactors => VectorOperationKind::Quantization,
            Self::MissingRescoreRows => VectorOperationKind::Rescore,
            Self::RowCountCancellation | Self::AllocationDenial => VectorOperationKind::RowIdentity,
        }
    }
}

const I24_DIMENSION_CASES: u64 = 17;
const I24_KERNEL_CASES: u64 = 11;
const I24_FLOAT_KERNELS: u64 = 2;
const I24_FLOAT_CASES_PER_KERNEL: u64 = 3;
const I24_CASES_PER_DIMENSION: u64 =
    I24_KERNEL_CASES + I24_FLOAT_KERNELS * (I24_FLOAT_CASES_PER_KERNEL - 1);
const I24_PUBLIC_STORE_CASES: u64 = 1;
const I25_PUBLIC_STORE_CASES: u64 = 3;
const I25_QUANTIZATION_SCHEMES: u64 = 2;
const I25_GEOMETRY_BOUNDARIES_PER_SCHEME: u64 = 6;
const I25_SPECIAL_VALUES: u64 = 3;
const I25_SPECIAL_POSITIONS: u64 = 3;
const I25_SPECIAL_SIDES: u64 = 2;
const I25_POSITIVE_CASES_PER_SCHEME: u64 = 7;
const I25_STOCHASTIC_BIT4_CASES: u64 = 4;
const I26_PRIMITIVE_CASES: u64 = 5;
const I26_ANTI_CORRELATED_TIE_CASES: u64 = 1;
const I26_ACTIVE_TIER_CASES: u64 = 3;
const I26_SEALED_EXACT_CASES: u64 = 1;
const I26_GRAPH_TIER_CASES: u64 = 2;
const I27_PRE_GRAPH_PHASE_TIER_CASES: u64 = 3;
const I27_SEALED_PHASE_TIER_CASES: u64 = 3;
const I27_FIRST_GRAPH_PHASE_TIER_CASES: u64 = 4;
const I27_SHADOW_PHASE_TIER_CASES: u64 = 3;
const I27_REOPEN_PHASE_TIER_CASES: u64 = 4;

/// Exact clean comparison counts derived from this family's required corpus.
///
/// I24 is the only runtime-dependent total: every actually available backend
/// executes nine exact kernels once and two float kernels across three
/// independent corpora over all seventeen dimension cases, followed by one
/// public Store case. The other totals are structural sums of the literal
/// I25-I27 corpus categories declared above.
pub fn expected_comparison_counts() -> Result<BTreeMap<&'static str, u64>, String> {
    let available_backends = KernelVariant::available().try_fold(0_u64, |count, _| {
        count
            .checked_add(1)
            .ok_or_else(|| "available kernel backend count overflowed u64".to_owned())
    })?;
    let i24 = available_backends
        .checked_mul(I24_CASES_PER_DIMENSION)
        .and_then(|count| count.checked_mul(I24_DIMENSION_CASES))
        .and_then(|count| count.checked_add(I24_PUBLIC_STORE_CASES))
        .ok_or_else(|| "I24 comparison count overflowed u64".to_owned())?;
    let i25_boundaries = I25_QUANTIZATION_SCHEMES
        .checked_mul(
            I25_GEOMETRY_BOUNDARIES_PER_SCHEME
                + I25_SPECIAL_VALUES * I25_SPECIAL_POSITIONS * I25_SPECIAL_SIDES,
        )
        .ok_or_else(|| "I25 boundary comparison count overflowed u64".to_owned())?;
    let i25_positive = I25_QUANTIZATION_SCHEMES
        .checked_mul(I25_POSITIVE_CASES_PER_SCHEME)
        .and_then(|count| count.checked_add(I25_STOCHASTIC_BIT4_CASES))
        .ok_or_else(|| "I25 positive comparison count overflowed u64".to_owned())?;
    let i25 = I25_PUBLIC_STORE_CASES
        .checked_add(i25_boundaries)
        .and_then(|count| count.checked_add(i25_positive))
        .ok_or_else(|| "I25 comparison count overflowed u64".to_owned())?;
    let i26 = I26_PRIMITIVE_CASES
        + I26_ANTI_CORRELATED_TIE_CASES
        + I26_ACTIVE_TIER_CASES
        + I26_SEALED_EXACT_CASES
        + I26_GRAPH_TIER_CASES;
    let i27 = I27_PRE_GRAPH_PHASE_TIER_CASES
        + I27_SEALED_PHASE_TIER_CASES
        + I27_FIRST_GRAPH_PHASE_TIER_CASES
        + I27_SHADOW_PHASE_TIER_CASES
        + I27_REOPEN_PHASE_TIER_CASES;
    Ok(BTreeMap::from([
        ("I24", i24),
        ("I25", i25),
        ("I26", i26),
        ("I27", i27),
    ]))
}

/// Exact comparison count for one deterministic operation/fault invocation.
///
/// Forced dispatch returns the normal I24 corpus plus the one comparison
/// produced by its required fresh-process child. Other faults retain their
/// operation's clean invariant corpus size.
pub fn expected_comparison_count(
    operation: VectorOperationKind,
    fault: Option<VectorFaultKind>,
) -> Result<(&'static str, u64), String> {
    if fault.is_some_and(|selected| selected.operation() != operation) {
        return Err(format!(
            "vector fault {fault:?} does not belong to operation {operation:?}"
        ));
    }
    let invariant = match operation {
        VectorOperationKind::KernelParity => "I24",
        VectorOperationKind::Quantization => "I25",
        VectorOperationKind::Rescore => "I26",
        VectorOperationKind::RowIdentity => "I27",
    };
    let clean = expected_comparison_counts()?;
    let mut count = clean
        .get(invariant)
        .copied()
        .ok_or_else(|| format!("vector comparison-count map omitted {invariant}"))?;
    if fault == Some(VectorFaultKind::ForcedDispatchBackend) {
        count = count
            .checked_add(1)
            .ok_or_else(|| "forced-dispatch I24 comparison count overflowed u64".to_owned())?;
    }
    Ok((invariant, count))
}

/// An I24 independent input and the value observed from the named real kernel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I24EvidencePair {
    pub input: independent::KernelInput,
    pub observed: independent::I24Observed,
}

/// An I25 independent codec input and product observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I25EvidencePair {
    pub input: independent::QuantInput,
    pub observed: independent::I25Observed,
}

/// An I26 independent rescore input and primitive plus Store observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I26EvidencePair {
    pub input: independent::RescoreInput,
    pub observed: independent::I26Observed,
}

/// An I27 independent mutation input and public Store observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct I27EvidencePair {
    pub input: independent::IdentityInput,
    pub observed: independent::I27Observed,
}

/// The operation-owned invariant DTO pairs returned to shared dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VectorInvariantEvidence {
    I24(Vec<I24EvidencePair>),
    I25(Vec<I25EvidencePair>),
    I26(Vec<I26EvidencePair>),
    I27(Vec<I27EvidencePair>),
}

/// Complete language-independent inputs retained for fixture serialization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VectorPrimitiveInputs {
    I24(Vec<independent::KernelInput>),
    I25(Vec<independent::QuantInput>),
    I26(Vec<independent::RescoreInput>),
    I27(Vec<independent::IdentityInput>),
}

/// Primitive runtime CPU feature facts from the same cached dispatch probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VectorRuntimeFeatures {
    pub neon: bool,
    pub dotprod: bool,
    pub fp16: bool,
    pub i8mm: bool,
    pub sme2: bool,
    pub avx2: bool,
    pub popcnt: bool,
}

/// Language-independent generic VFS fault site accepted by this family.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VectorGenericFaultSite {
    Open,
    Read,
    ReadRange,
    Write,
    Append,
    Sync,
    Rename,
    List,
    Delete,
    Clock,
}

impl VectorGenericFaultSite {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Read => "read",
            Self::ReadRange => "read_range",
            Self::Write => "write",
            Self::Append => "append",
            Self::Sync => "sync",
            Self::Rename => "rename",
            Self::List => "list",
            Self::Delete => "delete",
            Self::Clock => "clock",
        }
    }
}

/// Language-independent generic VFS fault mode accepted by this family.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VectorGenericFaultMode {
    Eio,
    Eacces,
    Enospc,
    BitFlip,
    TornWrite,
    Truncate,
    WrongObject,
    MisdirectedWrite,
    ZeroFill,
    Latency,
    SilentDrop,
    PostCommitError,
}

impl VectorGenericFaultMode {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Eio => "eio",
            Self::Eacces => "eacces",
            Self::Enospc => "enospc",
            Self::BitFlip => "bit_flip",
            Self::TornWrite => "torn_write",
            Self::Truncate => "truncate",
            Self::WrongObject => "wrong_object",
            Self::MisdirectedWrite => "misdirected_write",
            Self::ZeroFill => "zero_fill",
            Self::Latency => "latency",
            Self::SilentDrop => "silent_drop",
            Self::PostCommitError => "post_commit_error",
        }
    }
}

/// Primitive generic schedule supplied by shared campaign dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VectorGenericFaultSchedule {
    pub id: String,
    pub site: VectorGenericFaultSite,
    pub mode: VectorGenericFaultMode,
    pub nth_match: usize,
    pub path_contains: Option<String>,
}

/// Per-program-operation context supplied without runner-owned fault types.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VectorExecutionContext {
    pub program_op_index: usize,
    pub generic_fault: Option<VectorGenericFaultSchedule>,
}

/// Exact public lifecycle stage reached by one generic VFS leg.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VectorGenericFaultStage {
    Open,
    Ingest,
    Seal,
    Close,
    Reopen,
    Query,
}

/// Typed public outcome from one generic VFS leg.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VectorGenericFaultStatus {
    StoreResult(VectorStoreResultFact),
    StoreFailure {
        kind: StoreErrorKind,
    },
    IngestFailure {
        status: independent::PrimitiveStatus,
    },
}

/// Observed scheduled event, including the runtime-owned firing facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VectorGenericFaultEvent {
    pub id: String,
    pub op_index: usize,
    pub site: VectorGenericFaultSite,
    pub mode: VectorGenericFaultMode,
    pub nth_match: usize,
    pub path_contains: Option<String>,
    pub fired: bool,
    pub path: Option<String>,
}

/// One clean or armed generic VFS execution leg.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VectorGenericFaultLeg {
    pub stage: VectorGenericFaultStage,
    pub status: VectorGenericFaultStatus,
    pub event: VectorGenericFaultEvent,
    pub feature_receipts: Vec<VectorFaultReceipt>,
}

/// Same-schedule clean/fault evidence from two isolated runtimes and roots.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VectorGenericFaultEvidence {
    pub operation: VectorOperationKind,
    pub feature_fault: Option<VectorFaultKind>,
    pub feature_mutation: VectorMutationEvidence,
    pub program_op_index: usize,
    pub schedule: VectorGenericFaultSchedule,
    pub clean: VectorGenericFaultLeg,
    pub fault: VectorGenericFaultLeg,
    pub clean_initial_directory: VectorFixtureDirectoryEvidence,
    pub fault_initial_directory: VectorFixtureDirectoryEvidence,
    pub isolated_directories: bool,
    pub isolated_runtimes: bool,
}

#[derive(Clone, Debug)]
enum GenericFeatureController {
    Kernel(KernelFaultController),
    Vector(VectorFaultController),
}

impl GenericFeatureController {
    fn install(&self, dependencies: StoreTestDependencies) -> StoreTestDependencies {
        match self {
            Self::Kernel(controller) => {
                dependencies.with_kernel_fault_controller(controller.clone())
            }
            Self::Vector(controller) => {
                dependencies.with_vector_fault_controller(controller.clone())
            }
        }
    }

    fn take_typed_receipts(&self) -> Vec<VectorFaultReceipt> {
        match self {
            Self::Kernel(controller) => controller.take_typed_receipts(),
            Self::Vector(controller) => controller.take_typed_receipts(),
        }
    }
}

/// Primitive, deterministic fixture facts required by replay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VectorPrimitiveFixture {
    pub namespace: &'static str,
    pub seed: u64,
    pub operation: &'static str,
    pub inputs: VectorPrimitiveInputs,
    pub backend_inventory: Vec<independent::BackendId>,
    pub tier_inventory: Vec<u8>,
    pub source_inventory: Vec<independent::PrimitiveSource>,
    pub documents: Vec<independent::PrimitiveDocument>,
    pub public_schedule: Vec<independent::PublicStoreStep>,
    pub runtime_features: VectorRuntimeFeatures,
}

/// Exact persisted field modified by a quantization fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VectorQuantMutationField {
    Bit4OddPadding,
    Bit4Correction,
    Int8Scale,
}

/// Exact owning production view modified by a missing-rescore fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VectorRescoreMutationSite {
    ExactRescoreRows,
    QueryRescoreRows,
}

/// Typed planned mutation; receipt text is never parsed to recover these facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VectorMutationEvidence {
    None,
    ForcedDispatch {
        case_id: u64,
        requested: independent::BackendId,
    },
    QuantCorruption {
        case_id: u64,
        scheme: independent::QuantScheme,
        source: independent::PrimitiveSource,
        tier: u8,
        local_row: u32,
        field: VectorQuantMutationField,
    },
    MissingRescoreRows {
        case_id: u64,
        source: independent::PrimitiveSource,
        tier: u8,
        site: VectorRescoreMutationSite,
        expected_rows: u32,
        available_rows: u32,
    },
    RowCancellation {
        case_id: u64,
        source: independent::PrimitiveSource,
        tier: u8,
        requested_rows: u32,
    },
    AllocationDenial {
        case_id: u64,
        component: &'static str,
        items: u64,
        bytes: u64,
    },
}

/// Exact candidate and execution facts from one public Store call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VectorStoreResultFact {
    pub status: independent::PrimitiveStatus,
    pub generation: u64,
    pub candidates: Vec<independent::IdentityObservedRow>,
    pub dims_touched: u64,
    pub bytes_read: u64,
    pub exact_rescore: bool,
    pub approximate: bool,
    pub returned: u64,
}

/// One stable content fact from a pre-open Store fixture directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VectorFixtureFileFact {
    pub relative_path: String,
    pub byte_length: u64,
    pub digest: u64,
}

/// Stable pre-open Store directory evidence for one same-seed control leg.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VectorFixtureDirectoryEvidence {
    pub digest: u64,
    pub files: Vec<VectorFixtureFileFact>,
}

/// Same-seed public clean/fault/post-clear facts for one operation invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VectorControlEvidence {
    pub namespace: &'static str,
    pub operation: VectorOperationKind,
    pub seed: u64,
    pub clean: VectorStoreResultFact,
    pub fault: VectorStoreResultFact,
    pub retry: VectorStoreResultFact,
    /// Exact file inventory before the clean Store is opened.
    pub clean_initial_directory: VectorFixtureDirectoryEvidence,
    /// Exact file inventory before the faulted Store is opened.
    pub fault_initial_directory: VectorFixtureDirectoryEvidence,
    /// True only when clean and fault were materialized at different roots.
    pub isolated_directories: bool,
}

/// Closed child-evidence wire format used across fresh process boundaries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForcedBackendTransportFormat {
    TypedBinaryV1,
}

/// Actual inputs/outputs and public Store facts returned by the forced child.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForcedBackendChildEvidence {
    pub transport: ForcedBackendTransportFormat,
    pub requested: independent::BackendId,
    pub pair: I24EvidencePair,
    pub fault_result: VectorStoreResultFact,
    pub retry_result: VectorStoreResultFact,
    pub receipt: VectorFaultReceipt,
}

/// Operation-scoped data returned to shared vector campaign dispatch.
#[derive(Debug)]
pub struct VectorOperationEvidence {
    pub operation: VectorOperationKind,
    pub fault: Option<VectorFaultKind>,
    pub fixture: VectorPrimitiveFixture,
    pub mutation: VectorMutationEvidence,
    pub invariant: VectorInvariantEvidence,
    /// In-process receipts drained directly from the product controller.
    pub receipts: Vec<VectorFaultReceipt>,
    /// Forced dispatch must cross a process boundary, so its product receipt is
    /// represented by exact accessor facts written by that child.
    pub forced_child: Option<ForcedBackendChildEvidence>,
    /// Generic VFS schedule evidence, when shared dispatch supplied one.
    pub generic_fault: Option<VectorGenericFaultEvidence>,
    pub control: VectorControlEvidence,
}

/// Family-owned canonical attestation for one exact checker comparison.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VectorCanonicalEvidence {
    pub checker_id: &'static str,
    pub case_id: u64,
    pub input: independent::VectorCanonicalRecord,
    pub observed: independent::VectorCanonicalRecord,
    pub first_difference: Option<independent::VectorFirstDifference>,
}

/// Stable family-owned binary fixture contract consumed by artifact replay.
pub const VECTOR_FIXTURE_CODEC_VERSION: &str = "vector-fixture-v1";
const VECTOR_FIXTURE_MAGIC: [u8; 8] = *b"ZEVFIX01";

/// One literal input/observation pair retained inside a vector fixture.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VectorRetainedComparison {
    pub input_bytes: Vec<u8>,
    pub observed_bytes: Vec<u8>,
}

/// Closed decoded fixture. Canonical comparison bytes retain every primitive
/// input bit, Store schedule, document/revision, and observed result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VectorRetainedFixture {
    pub version: &'static str,
    pub operation: VectorOperationKind,
    pub seed: u64,
    pub fault: Option<VectorFaultKind>,
    pub mutation: VectorMutationEvidence,
    pub context: VectorExecutionContext,
    pub backend_inventory: Vec<independent::BackendId>,
    pub tier_inventory: Vec<u8>,
    pub source_inventory: Vec<independent::PrimitiveSource>,
    pub documents: Vec<independent::PrimitiveDocument>,
    pub public_schedule: Vec<independent::PublicStoreStep>,
    pub runtime_features: VectorRuntimeFeatures,
    pub comparisons: Vec<VectorRetainedComparison>,
    pub sha256: [u8; 32],
}

/// Result of decoding a retained fixture and executing its literal checkers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VectorRetainedFixtureExecution {
    pub fixture: VectorRetainedFixture,
    pub comparisons: Vec<independent::VectorCanonicalReplay>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VectorObservationPlant {
    I24FirstIntegerOutput,
    I25FirstNonFiniteStatusToOk,
    I26SwapFirstEqualStoreHits,
    I27RemapFirstPhysicalRow,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
struct ActiveObservationPlant {
    owner: std::thread::ThreadId,
    plant: VectorObservationPlant,
}

#[cfg(test)]
struct VectorObservationPlantGuard {
    _exclusive: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
fn observation_plant_slot() -> &'static Mutex<Option<ActiveObservationPlant>> {
    static SLOT: OnceLock<Mutex<Option<ActiveObservationPlant>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
fn observation_plant_exclusive() -> &'static Mutex<()> {
    static EXCLUSIVE: OnceLock<Mutex<()>> = OnceLock::new();
    EXCLUSIVE.get_or_init(|| Mutex::new(()))
}

#[cfg(test)]
fn install_observation_plant(
    plant: VectorObservationPlant,
) -> Result<VectorObservationPlantGuard, String> {
    let exclusive = observation_plant_exclusive()
        .lock()
        .map_err(|_| "vector observation plant lock was poisoned".to_owned())?;
    let mut slot = observation_plant_slot()
        .lock()
        .map_err(|_| "vector observation plant slot was poisoned".to_owned())?;
    *slot = Some(ActiveObservationPlant {
        owner: std::thread::current().id(),
        plant,
    });
    drop(slot);
    Ok(VectorObservationPlantGuard {
        _exclusive: exclusive,
    })
}

#[cfg(test)]
impl Drop for VectorObservationPlantGuard {
    fn drop(&mut self) {
        if let Ok(mut slot) = observation_plant_slot().lock() {
            *slot = None;
        }
    }
}

#[cfg(test)]
fn apply_observation_plant(evidence: &mut VectorOperationEvidence) -> Result<(), String> {
    let active = observation_plant_slot()
        .lock()
        .map_err(|_| "vector observation plant slot was poisoned".to_owned())?
        .as_ref()
        .copied();
    let Some(active) = active.filter(|active| active.owner == std::thread::current().id()) else {
        return Ok(());
    };
    match active.plant {
        VectorObservationPlant::I24FirstIntegerOutput => {
            let VectorInvariantEvidence::I24(pairs) = &mut evidence.invariant else {
                return Err("I24 adapter plant reached a non-I24 operation".to_owned());
            };
            let value = pairs
                .iter_mut()
                .find_map(|pair| match &mut pair.observed.value {
                    independent::KernelValue::S32(value) if !pair.observed.selected_for_store => {
                        Some(value)
                    }
                    _ => None,
                })
                .ok_or_else(|| "I24 adapter plant found no integer observation".to_owned())?;
            *value = value
                .checked_add(1)
                .ok_or_else(|| "I24 adapter plant output overflowed i32".to_owned())?;
        }
        VectorObservationPlant::I25FirstNonFiniteStatusToOk => {
            let VectorInvariantEvidence::I25(pairs) = &mut evidence.invariant else {
                return Err("I25 adapter plant reached a non-I25 operation".to_owned());
            };
            let status = pairs
                .iter_mut()
                .find_map(|pair| match &mut pair.observed.result.status {
                    status @ independent::PrimitiveStatus::NonFinite { .. } => Some(status),
                    _ => None,
                })
                .ok_or_else(|| "I25 adapter plant found no non-finite observation".to_owned())?;
            *status = independent::PrimitiveStatus::Ok;
        }
        VectorObservationPlant::I26SwapFirstEqualStoreHits => {
            let VectorInvariantEvidence::I26(pairs) = &mut evidence.invariant else {
                return Err("I26 adapter plant reached a non-I26 operation".to_owned());
            };
            let hits = pairs
                .iter_mut()
                .map(|pair| &mut pair.observed.store_hits)
                .find(|hits| {
                    hits.first()
                        .zip(hits.get(1))
                        .is_some_and(|(left, right)| left.score == right.score)
                })
                .ok_or_else(|| "I26 adapter plant found no tied Store hits".to_owned())?;
            hits.swap(0, 1);
        }
        VectorObservationPlant::I27RemapFirstPhysicalRow => {
            let VectorInvariantEvidence::I27(pairs) = &mut evidence.invariant else {
                return Err("I27 adapter plant reached a non-I27 operation".to_owned());
            };
            let rows = pairs
                .iter_mut()
                .map(|pair| &mut pair.observed.rows)
                .find(|rows| rows.len() >= 2)
                .ok_or_else(|| "I27 adapter plant found fewer than two physical rows".to_owned())?;
            rows[0].row = rows[1].row;
        }
    }
    Ok(())
}

impl VectorOperationEvidence {
    /// Canonical input/observation bytes and structured checker differences.
    ///
    /// Shared evidence code consumes this API rather than formatting the DTOs
    /// with `Debug` or hashing runner-owned JSON.
    pub fn canonical_attestations(&self) -> Result<Vec<VectorCanonicalEvidence>, String> {
        let mut attestations = Vec::new();
        match &self.invariant {
            VectorInvariantEvidence::I24(pairs) => {
                attestations.reserve(pairs.len());
                for pair in pairs {
                    let expected = independent::expected_kernel(&pair.input)?;
                    attestations.push(VectorCanonicalEvidence {
                        checker_id: independent::I24_CHECKER_ID,
                        case_id: pair.input.case_id,
                        input: independent::canonical_i24_input(&pair.input),
                        observed: independent::canonical_i24_observed(&pair.observed),
                        first_difference: independent::first_difference_i24(
                            &expected,
                            &pair.observed,
                        ),
                    });
                }
            }
            VectorInvariantEvidence::I25(pairs) => {
                attestations.reserve(pairs.len());
                for pair in pairs {
                    let expected = independent::expected_quantization(&pair.input);
                    attestations.push(VectorCanonicalEvidence {
                        checker_id: independent::I25_CHECKER_ID,
                        case_id: pair.input.case_id,
                        input: independent::canonical_i25_input(&pair.input),
                        observed: independent::canonical_i25_observed(&pair.observed),
                        first_difference: independent::first_difference_i25(
                            &expected,
                            &pair.observed,
                        ),
                    });
                }
            }
            VectorInvariantEvidence::I26(pairs) => {
                attestations.reserve(pairs.len());
                for pair in pairs {
                    let expected = independent::expected_rescore(&pair.input);
                    attestations.push(VectorCanonicalEvidence {
                        checker_id: independent::I26_CHECKER_ID,
                        case_id: pair.input.case_id,
                        input: independent::canonical_i26_input(&pair.input),
                        observed: independent::canonical_i26_observed(&pair.observed),
                        first_difference: independent::first_difference_i26(
                            &expected,
                            &pair.observed,
                        ),
                    });
                }
            }
            VectorInvariantEvidence::I27(pairs) => {
                attestations.reserve(pairs.len());
                for pair in pairs {
                    let expected = independent::expected_identity(&pair.input)?;
                    attestations.push(VectorCanonicalEvidence {
                        checker_id: independent::I27_CHECKER_ID,
                        case_id: pair.input.case_id,
                        input: independent::canonical_i27_input(&pair.input),
                        observed: independent::canonical_i27_observed(&pair.observed),
                        first_difference: independent::first_difference_i27(
                            &expected,
                            &pair.observed,
                        ),
                    });
                }
            }
        }
        Ok(attestations)
    }
}

struct VectorFixtureWriter {
    bytes: Vec<u8>,
}

impl VectorFixtureWriter {
    fn new() -> Self {
        Self {
            bytes: VECTOR_FIXTURE_MAGIC.to_vec(),
        }
    }

    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn len(&mut self, value: usize, field: &str) -> Result<(), String> {
        self.u32(
            u32::try_from(value)
                .map_err(|_| format!("vector retained fixture {field} exceeds u32"))?,
        );
        Ok(())
    }

    fn bytes(&mut self, value: &[u8], field: &str) -> Result<(), String> {
        self.len(value.len(), field)?;
        self.bytes.extend_from_slice(value);
        Ok(())
    }

    fn string(&mut self, value: &str, field: &str) -> Result<(), String> {
        self.bytes(value.as_bytes(), field)
    }
}

struct VectorFixtureReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> VectorFixtureReader<'a> {
    fn new(bytes: &'a [u8]) -> Result<Self, String> {
        if bytes.get(..VECTOR_FIXTURE_MAGIC.len()) != Some(&VECTOR_FIXTURE_MAGIC) {
            return Err("vector retained fixture magic/version mismatch".to_owned());
        }
        Ok(Self {
            bytes,
            offset: VECTOR_FIXTURE_MAGIC.len(),
        })
    }

    fn take<const N: usize>(&mut self, field: &str) -> Result<[u8; N], String> {
        let end = self
            .offset
            .checked_add(N)
            .ok_or_else(|| format!("vector retained fixture {field} offset overflow"))?;
        let value: [u8; N] = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| format!("vector retained fixture is truncated at {field}"))?
            .try_into()
            .map_err(|_| format!("vector retained fixture {field} width changed"))?;
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self, field: &str) -> Result<u8, String> {
        self.take::<1>(field).map(|value| value[0])
    }

    fn u32(&mut self, field: &str) -> Result<u32, String> {
        self.take::<4>(field).map(u32::from_le_bytes)
    }

    fn u64(&mut self, field: &str) -> Result<u64, String> {
        self.take::<8>(field).map(u64::from_le_bytes)
    }

    fn bytes(&mut self, field: &str) -> Result<Vec<u8>, String> {
        let length = usize::try_from(self.u32(field)?)
            .map_err(|_| format!("vector retained fixture {field} length exceeds usize"))?;
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| format!("vector retained fixture {field} offset overflow"))?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| format!("vector retained fixture is truncated at {field}"))?
            .to_vec();
        self.offset = end;
        Ok(value)
    }

    fn string(&mut self, field: &str) -> Result<String, String> {
        String::from_utf8(self.bytes(field)?)
            .map_err(|_| format!("vector retained fixture {field} is not UTF-8"))
    }

    fn count(&mut self, field: &str, minimum_item_bytes: usize) -> Result<usize, String> {
        let count = usize::try_from(self.u32(field)?)
            .map_err(|_| format!("vector retained fixture {field} exceeds usize"))?;
        let remaining = self.bytes.len().saturating_sub(self.offset);
        if minimum_item_bytes == 0 || count > remaining / minimum_item_bytes {
            return Err(format!(
                "vector retained fixture {field} {count} exceeds remaining payload"
            ));
        }
        Ok(count)
    }

    fn finish(self) -> Result<(), String> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(format!(
                "vector retained fixture has {} trailing payload bytes",
                self.bytes.len() - self.offset
            ))
        }
    }
}

const fn vector_operation_tag(operation: VectorOperationKind) -> u8 {
    match operation {
        VectorOperationKind::KernelParity => 1,
        VectorOperationKind::Quantization => 2,
        VectorOperationKind::Rescore => 3,
        VectorOperationKind::RowIdentity => 4,
    }
}

fn vector_operation_from_tag(tag: u8) -> Result<VectorOperationKind, String> {
    match tag {
        1 => Ok(VectorOperationKind::KernelParity),
        2 => Ok(VectorOperationKind::Quantization),
        3 => Ok(VectorOperationKind::Rescore),
        4 => Ok(VectorOperationKind::RowIdentity),
        _ => Err(format!(
            "vector retained fixture has unknown operation tag {tag}"
        )),
    }
}

const fn vector_fault_tag(fault: Option<VectorFaultKind>) -> u8 {
    match fault {
        None => 0,
        Some(VectorFaultKind::ForcedDispatchBackend) => 1,
        Some(VectorFaultKind::CorruptCodesFactors) => 2,
        Some(VectorFaultKind::MissingRescoreRows) => 3,
        Some(VectorFaultKind::RowCountCancellation) => 4,
        Some(VectorFaultKind::AllocationDenial) => 5,
    }
}

fn vector_fault_from_tag(tag: u8) -> Result<Option<VectorFaultKind>, String> {
    match tag {
        0 => Ok(None),
        1 => Ok(Some(VectorFaultKind::ForcedDispatchBackend)),
        2 => Ok(Some(VectorFaultKind::CorruptCodesFactors)),
        3 => Ok(Some(VectorFaultKind::MissingRescoreRows)),
        4 => Ok(Some(VectorFaultKind::RowCountCancellation)),
        5 => Ok(Some(VectorFaultKind::AllocationDenial)),
        _ => Err(format!(
            "vector retained fixture has unknown fault tag {tag}"
        )),
    }
}

const fn retained_generic_site_tag(site: VectorGenericFaultSite) -> u8 {
    match site {
        VectorGenericFaultSite::Open => 0,
        VectorGenericFaultSite::Read => 1,
        VectorGenericFaultSite::ReadRange => 2,
        VectorGenericFaultSite::Write => 3,
        VectorGenericFaultSite::Append => 4,
        VectorGenericFaultSite::Sync => 5,
        VectorGenericFaultSite::Rename => 6,
        VectorGenericFaultSite::List => 7,
        VectorGenericFaultSite::Delete => 8,
        VectorGenericFaultSite::Clock => 9,
    }
}

fn retained_generic_site_from_tag(tag: u8) -> Result<VectorGenericFaultSite, String> {
    match tag {
        0 => Ok(VectorGenericFaultSite::Open),
        1 => Ok(VectorGenericFaultSite::Read),
        2 => Ok(VectorGenericFaultSite::ReadRange),
        3 => Ok(VectorGenericFaultSite::Write),
        4 => Ok(VectorGenericFaultSite::Append),
        5 => Ok(VectorGenericFaultSite::Sync),
        6 => Ok(VectorGenericFaultSite::Rename),
        7 => Ok(VectorGenericFaultSite::List),
        8 => Ok(VectorGenericFaultSite::Delete),
        9 => Ok(VectorGenericFaultSite::Clock),
        _ => Err(format!(
            "vector retained fixture has unknown generic site tag {tag}"
        )),
    }
}

const fn retained_generic_mode_tag(mode: VectorGenericFaultMode) -> u8 {
    match mode {
        VectorGenericFaultMode::Eio => 0,
        VectorGenericFaultMode::Eacces => 1,
        VectorGenericFaultMode::Enospc => 2,
        VectorGenericFaultMode::BitFlip => 3,
        VectorGenericFaultMode::TornWrite => 4,
        VectorGenericFaultMode::Truncate => 5,
        VectorGenericFaultMode::WrongObject => 6,
        VectorGenericFaultMode::MisdirectedWrite => 7,
        VectorGenericFaultMode::ZeroFill => 8,
        VectorGenericFaultMode::Latency => 9,
        VectorGenericFaultMode::SilentDrop => 10,
        VectorGenericFaultMode::PostCommitError => 11,
    }
}

fn retained_generic_mode_from_tag(tag: u8) -> Result<VectorGenericFaultMode, String> {
    match tag {
        0 => Ok(VectorGenericFaultMode::Eio),
        1 => Ok(VectorGenericFaultMode::Eacces),
        2 => Ok(VectorGenericFaultMode::Enospc),
        3 => Ok(VectorGenericFaultMode::BitFlip),
        4 => Ok(VectorGenericFaultMode::TornWrite),
        5 => Ok(VectorGenericFaultMode::Truncate),
        6 => Ok(VectorGenericFaultMode::WrongObject),
        7 => Ok(VectorGenericFaultMode::MisdirectedWrite),
        8 => Ok(VectorGenericFaultMode::ZeroFill),
        9 => Ok(VectorGenericFaultMode::Latency),
        10 => Ok(VectorGenericFaultMode::SilentDrop),
        11 => Ok(VectorGenericFaultMode::PostCommitError),
        _ => Err(format!(
            "vector retained fixture has unknown generic mode tag {tag}"
        )),
    }
}

fn encode_retained_context(
    writer: &mut VectorFixtureWriter,
    generic: Option<&VectorGenericFaultEvidence>,
) -> Result<(), String> {
    let Some(generic) = generic else {
        writer.u8(0);
        return Ok(());
    };
    writer.u8(1);
    writer.u64(
        u64::try_from(generic.program_op_index)
            .map_err(|_| "vector generic operation index exceeds u64".to_owned())?,
    );
    writer.string(&generic.schedule.id, "generic schedule id")?;
    writer.u8(retained_generic_site_tag(generic.schedule.site));
    writer.u8(retained_generic_mode_tag(generic.schedule.mode));
    writer.u64(
        u64::try_from(generic.schedule.nth_match)
            .map_err(|_| "vector generic nth-match exceeds u64".to_owned())?,
    );
    match generic.schedule.path_contains.as_deref() {
        None => writer.u8(0),
        Some(path) => {
            writer.u8(1);
            writer.string(path, "generic path filter")?;
        }
    }
    Ok(())
}

fn decode_retained_context(
    reader: &mut VectorFixtureReader<'_>,
) -> Result<VectorExecutionContext, String> {
    match reader.u8("generic context presence")? {
        0 => Ok(VectorExecutionContext::default()),
        1 => {
            let program_op_index = usize::try_from(reader.u64("generic operation index")?)
                .map_err(|_| "vector generic operation index exceeds usize".to_owned())?;
            let id = reader.string("generic schedule id")?;
            if id.is_empty() {
                return Err("vector retained fixture generic schedule id is empty".to_owned());
            }
            let site_tag = reader.u8("generic site")?;
            let mode_tag = reader.u8("generic mode")?;
            let nth_match = usize::try_from(reader.u64("generic nth-match")?)
                .map_err(|_| "vector generic nth-match exceeds usize".to_owned())?;
            if nth_match == 0 {
                return Err("vector retained fixture generic nth-match is zero".to_owned());
            }
            let path_contains = match reader.u8("generic path presence")? {
                0 => None,
                1 => Some(reader.string("generic path filter")?),
                tag => {
                    return Err(format!(
                        "vector retained fixture has unknown generic path tag {tag}"
                    ));
                }
            };
            Ok(VectorExecutionContext {
                program_op_index,
                generic_fault: Some(VectorGenericFaultSchedule {
                    id,
                    site: retained_generic_site_from_tag(site_tag)?,
                    mode: retained_generic_mode_from_tag(mode_tag)?,
                    nth_match,
                    path_contains,
                }),
            })
        }
        tag => Err(format!(
            "vector retained fixture has unknown generic context tag {tag}"
        )),
    }
}

const fn retained_backend_tag(backend: independent::BackendId) -> u8 {
    match backend {
        independent::BackendId::Scalar => 0,
        independent::BackendId::NeonWiden => 1,
        independent::BackendId::NeonDotprodU4 => 2,
        independent::BackendId::NeonI8mm => 3,
        independent::BackendId::NeonDotprodU2 => 4,
        independent::BackendId::NeonDotprodU6 => 5,
        independent::BackendId::NeonDotprodU8 => 6,
        independent::BackendId::NeonDotprodU4Prefetch => 7,
        independent::BackendId::Avx2 => 8,
    }
}

fn retained_backend_from_tag(tag: u8) -> Result<independent::BackendId, String> {
    match tag {
        0 => Ok(independent::BackendId::Scalar),
        1 => Ok(independent::BackendId::NeonWiden),
        2 => Ok(independent::BackendId::NeonDotprodU4),
        3 => Ok(independent::BackendId::NeonI8mm),
        4 => Ok(independent::BackendId::NeonDotprodU2),
        5 => Ok(independent::BackendId::NeonDotprodU6),
        6 => Ok(independent::BackendId::NeonDotprodU8),
        7 => Ok(independent::BackendId::NeonDotprodU4Prefetch),
        8 => Ok(independent::BackendId::Avx2),
        _ => Err(format!(
            "vector retained fixture has unknown backend tag {tag}"
        )),
    }
}

fn encode_retained_source(writer: &mut VectorFixtureWriter, source: independent::PrimitiveSource) {
    match source {
        independent::PrimitiveSource::Active => writer.u8(0),
        independent::PrimitiveSource::Sealed(segment) => {
            writer.u8(1);
            writer.bytes.extend_from_slice(&segment);
        }
    }
}

fn decode_retained_source(
    reader: &mut VectorFixtureReader<'_>,
) -> Result<independent::PrimitiveSource, String> {
    match reader.u8("source tag")? {
        0 => Ok(independent::PrimitiveSource::Active),
        1 => Ok(independent::PrimitiveSource::Sealed(
            reader.take::<16>("sealed source")?,
        )),
        tag => Err(format!(
            "vector retained fixture has unknown source tag {tag}"
        )),
    }
}

const fn retained_schedule_tag(step: independent::PublicStoreStep) -> u8 {
    match step {
        independent::PublicStoreStep::IngestAccepted => 0,
        independent::PublicStoreStep::IngestRejected => 1,
        independent::PublicStoreStep::Seal => 2,
        independent::PublicStoreStep::PublishPreparedSegment => 3,
        independent::PublicStoreStep::DeleteAccepted => 4,
        independent::PublicStoreStep::Reopen => 5,
        independent::PublicStoreStep::Search => 6,
    }
}

fn retained_schedule_from_tag(tag: u8) -> Result<independent::PublicStoreStep, String> {
    match tag {
        0 => Ok(independent::PublicStoreStep::IngestAccepted),
        1 => Ok(independent::PublicStoreStep::IngestRejected),
        2 => Ok(independent::PublicStoreStep::Seal),
        3 => Ok(independent::PublicStoreStep::PublishPreparedSegment),
        4 => Ok(independent::PublicStoreStep::DeleteAccepted),
        5 => Ok(independent::PublicStoreStep::Reopen),
        6 => Ok(independent::PublicStoreStep::Search),
        _ => Err(format!(
            "vector retained fixture has unknown schedule tag {tag}"
        )),
    }
}

fn encode_retained_mutation(
    writer: &mut VectorFixtureWriter,
    mutation: &VectorMutationEvidence,
) -> Result<(), String> {
    match mutation {
        VectorMutationEvidence::None => writer.u8(0),
        VectorMutationEvidence::ForcedDispatch { case_id, requested } => {
            writer.u8(1);
            writer.u64(*case_id);
            writer.u8(retained_backend_tag(*requested));
        }
        VectorMutationEvidence::QuantCorruption {
            case_id,
            scheme,
            source,
            tier,
            local_row,
            field,
        } => {
            writer.u8(2);
            writer.u64(*case_id);
            writer.u8(match scheme {
                independent::QuantScheme::Bit4 => 0,
                independent::QuantScheme::Int8 => 1,
            });
            encode_retained_source(writer, *source);
            writer.u8(*tier);
            writer.u32(*local_row);
            writer.u8(match field {
                VectorQuantMutationField::Bit4OddPadding => 0,
                VectorQuantMutationField::Bit4Correction => 1,
                VectorQuantMutationField::Int8Scale => 2,
            });
        }
        VectorMutationEvidence::MissingRescoreRows {
            case_id,
            source,
            tier,
            site,
            expected_rows,
            available_rows,
        } => {
            writer.u8(3);
            writer.u64(*case_id);
            encode_retained_source(writer, *source);
            writer.u8(*tier);
            writer.u8(match site {
                VectorRescoreMutationSite::ExactRescoreRows => 0,
                VectorRescoreMutationSite::QueryRescoreRows => 1,
            });
            writer.u32(*expected_rows);
            writer.u32(*available_rows);
        }
        VectorMutationEvidence::RowCancellation {
            case_id,
            source,
            tier,
            requested_rows,
        } => {
            writer.u8(4);
            writer.u64(*case_id);
            encode_retained_source(writer, *source);
            writer.u8(*tier);
            writer.u32(*requested_rows);
        }
        VectorMutationEvidence::AllocationDenial {
            case_id,
            component,
            items,
            bytes,
        } => {
            writer.u8(5);
            writer.u64(*case_id);
            writer.string(component, "allocation component")?;
            writer.u64(*items);
            writer.u64(*bytes);
        }
    }
    Ok(())
}

fn decode_retained_mutation(
    reader: &mut VectorFixtureReader<'_>,
) -> Result<VectorMutationEvidence, String> {
    match reader.u8("mutation tag")? {
        0 => Ok(VectorMutationEvidence::None),
        1 => Ok(VectorMutationEvidence::ForcedDispatch {
            case_id: reader.u64("forced case")?,
            requested: retained_backend_from_tag(reader.u8("forced backend")?)?,
        }),
        2 => {
            let case_id = reader.u64("quant case")?;
            let scheme = match reader.u8("quant scheme")? {
                0 => independent::QuantScheme::Bit4,
                1 => independent::QuantScheme::Int8,
                tag => {
                    return Err(format!(
                        "vector retained fixture has unknown quant scheme tag {tag}"
                    ));
                }
            };
            let source = decode_retained_source(reader)?;
            let tier = reader.u8("quant tier")?;
            let local_row = reader.u32("quant local row")?;
            let field = match reader.u8("quant field")? {
                0 => VectorQuantMutationField::Bit4OddPadding,
                1 => VectorQuantMutationField::Bit4Correction,
                2 => VectorQuantMutationField::Int8Scale,
                tag => {
                    return Err(format!(
                        "vector retained fixture has unknown quant field tag {tag}"
                    ));
                }
            };
            Ok(VectorMutationEvidence::QuantCorruption {
                case_id,
                scheme,
                source,
                tier,
                local_row,
                field,
            })
        }
        3 => {
            let case_id = reader.u64("rescore case")?;
            let source = decode_retained_source(reader)?;
            let tier = reader.u8("rescore tier")?;
            let site = match reader.u8("rescore site")? {
                0 => VectorRescoreMutationSite::ExactRescoreRows,
                1 => VectorRescoreMutationSite::QueryRescoreRows,
                tag => {
                    return Err(format!(
                        "vector retained fixture has unknown rescore site tag {tag}"
                    ));
                }
            };
            Ok(VectorMutationEvidence::MissingRescoreRows {
                case_id,
                source,
                tier,
                site,
                expected_rows: reader.u32("expected rescore rows")?,
                available_rows: reader.u32("available rescore rows")?,
            })
        }
        4 => Ok(VectorMutationEvidence::RowCancellation {
            case_id: reader.u64("cancellation case")?,
            source: decode_retained_source(reader)?,
            tier: reader.u8("cancellation tier")?,
            requested_rows: reader.u32("cancellation rows")?,
        }),
        5 => {
            let case_id = reader.u64("allocation case")?;
            let component = reader.string("allocation component")?;
            if component != "vector search global candidates" {
                return Err(format!(
                    "vector retained fixture has unknown allocation component {component:?}"
                ));
            }
            Ok(VectorMutationEvidence::AllocationDenial {
                case_id,
                component: "vector search global candidates",
                items: reader.u64("allocation items")?,
                bytes: reader.u64("allocation bytes")?,
            })
        }
        tag => Err(format!(
            "vector retained fixture has unknown mutation tag {tag}"
        )),
    }
}

fn retained_fixture_checker(operation: VectorOperationKind) -> &'static str {
    match operation {
        VectorOperationKind::KernelParity => independent::I24_CHECKER_ID,
        VectorOperationKind::Quantization => independent::I25_CHECKER_ID,
        VectorOperationKind::Rescore => independent::I26_CHECKER_ID,
        VectorOperationKind::RowIdentity => independent::I27_CHECKER_ID,
    }
}

/// Encodes one completed operation into family-owned, checksummed fixture bytes.
pub fn encode_vector_fixture(evidence: &VectorOperationEvidence) -> Result<Vec<u8>, String> {
    if evidence.fixture.namespace != "vector-execution-v1"
        || evidence.fixture.operation != evidence.operation.key()
        || evidence.fixture.seed != evidence.control.seed
    {
        return Err("vector operation and primitive fixture metadata disagree".to_owned());
    }
    let attestations = evidence.canonical_attestations()?;
    if attestations.is_empty() {
        return Err("vector retained fixture has no canonical comparisons".to_owned());
    }
    let mut writer = VectorFixtureWriter::new();
    writer.u8(vector_operation_tag(evidence.operation));
    writer.u64(evidence.fixture.seed);
    writer.u8(vector_fault_tag(evidence.fault));
    encode_retained_mutation(&mut writer, &evidence.mutation)?;
    encode_retained_context(&mut writer, evidence.generic_fault.as_ref())?;

    writer.len(evidence.fixture.backend_inventory.len(), "backend count")?;
    for backend in &evidence.fixture.backend_inventory {
        writer.u8(retained_backend_tag(*backend));
    }
    writer.len(evidence.fixture.tier_inventory.len(), "tier count")?;
    for tier in &evidence.fixture.tier_inventory {
        writer.u8(*tier);
    }
    writer.len(evidence.fixture.source_inventory.len(), "source count")?;
    for source in &evidence.fixture.source_inventory {
        encode_retained_source(&mut writer, *source);
    }
    writer.len(evidence.fixture.documents.len(), "document count")?;
    for document in &evidence.fixture.documents {
        writer.bytes.extend_from_slice(&document.doc_id_be);
        writer.u64(document.revision);
    }
    writer.len(evidence.fixture.public_schedule.len(), "schedule count")?;
    for step in &evidence.fixture.public_schedule {
        writer.u8(retained_schedule_tag(*step));
    }
    let features = evidence.fixture.runtime_features;
    writer.u8(u8::from(features.neon)
        | (u8::from(features.dotprod) << 1)
        | (u8::from(features.fp16) << 2)
        | (u8::from(features.i8mm) << 3)
        | (u8::from(features.sme2) << 4)
        | (u8::from(features.avx2) << 5)
        | (u8::from(features.popcnt) << 6));
    writer.len(attestations.len(), "comparison count")?;
    for attestation in attestations {
        writer.bytes(&attestation.input.bytes, "canonical input")?;
        writer.bytes(&attestation.observed.bytes, "canonical observation")?;
    }
    let digest = independent::canonical_sha256(&writer.bytes);
    writer.bytes.extend_from_slice(&digest);
    Ok(writer.bytes)
}

/// Decodes and validates family fixture bytes without consulting a seed or generator.
pub fn decode_vector_fixture(bytes: &[u8]) -> Result<VectorRetainedFixture, String> {
    let payload_length = bytes
        .len()
        .checked_sub(32)
        .ok_or_else(|| "vector retained fixture is shorter than its SHA-256".to_owned())?;
    let (payload, digest_bytes) = bytes.split_at(payload_length);
    let digest: [u8; 32] = digest_bytes
        .try_into()
        .map_err(|_| "vector retained fixture SHA-256 width changed".to_owned())?;
    let expected_digest = independent::canonical_sha256(payload);
    if digest != expected_digest {
        return Err("vector retained fixture SHA-256 mismatch".to_owned());
    }
    let mut reader = VectorFixtureReader::new(payload)?;
    let operation = vector_operation_from_tag(reader.u8("operation")?)?;
    let seed = reader.u64("seed")?;
    let fault = vector_fault_from_tag(reader.u8("fault")?)?;
    if fault.is_some_and(|selected| selected.operation() != operation) {
        return Err("vector retained fixture fault belongs to another operation".to_owned());
    }
    let mutation = decode_retained_mutation(&mut reader)?;
    let context = decode_retained_context(&mut reader)?;
    let backend_count = reader.count("backend count", 1)?;
    let mut backend_inventory = Vec::with_capacity(backend_count);
    for _ in 0..backend_count {
        let tag = reader.u8("backend")?;
        backend_inventory.push(retained_backend_from_tag(tag)?);
    }
    let tier_count = reader.count("tier count", 1)?;
    let mut tier_inventory = Vec::with_capacity(tier_count);
    for _ in 0..tier_count {
        tier_inventory.push(reader.u8("tier")?);
    }
    let source_count = reader.count("source count", 1)?;
    let mut source_inventory = Vec::with_capacity(source_count);
    for _ in 0..source_count {
        source_inventory.push(decode_retained_source(&mut reader)?);
    }
    let document_count = reader.count("document count", 24)?;
    let mut documents = Vec::with_capacity(document_count);
    for _ in 0..document_count {
        documents.push(independent::PrimitiveDocument {
            doc_id_be: reader.take::<16>("document id")?,
            revision: reader.u64("document revision")?,
        });
    }
    let schedule_count = reader.count("schedule count", 1)?;
    let mut public_schedule = Vec::with_capacity(schedule_count);
    for _ in 0..schedule_count {
        let tag = reader.u8("schedule")?;
        public_schedule.push(retained_schedule_from_tag(tag)?);
    }
    let feature_bits = reader.u8("runtime features")?;
    if feature_bits & 0x80 != 0 {
        return Err("vector retained fixture runtime feature reserved bit is set".to_owned());
    }
    let runtime_features = VectorRuntimeFeatures {
        neon: feature_bits & 1 != 0,
        dotprod: feature_bits & 2 != 0,
        fp16: feature_bits & 4 != 0,
        i8mm: feature_bits & 8 != 0,
        sme2: feature_bits & 16 != 0,
        avx2: feature_bits & 32 != 0,
        popcnt: feature_bits & 64 != 0,
    };
    let comparison_count = reader.count("comparison count", 8)?;
    if comparison_count == 0 {
        return Err("vector retained fixture has no comparisons".to_owned());
    }
    let mut comparisons = Vec::with_capacity(comparison_count);
    for _ in 0..comparison_count {
        comparisons.push(VectorRetainedComparison {
            input_bytes: reader.bytes("canonical input")?,
            observed_bytes: reader.bytes("canonical observation")?,
        });
    }
    reader.finish()?;
    let expected_checker = retained_fixture_checker(operation);
    for comparison in &comparisons {
        let replay = independent::replay_canonical_comparison(
            &comparison.input_bytes,
            &comparison.observed_bytes,
        )?;
        if replay.checker_id != expected_checker {
            return Err(format!(
                "vector retained fixture operation {} contains checker {}",
                operation.key(),
                replay.checker_id
            ));
        }
    }
    Ok(VectorRetainedFixture {
        version: VECTOR_FIXTURE_CODEC_VERSION,
        operation,
        seed,
        fault,
        mutation,
        context,
        backend_inventory,
        tier_inventory,
        source_inventory,
        documents,
        public_schedule,
        runtime_features,
        comparisons,
        sha256: digest,
    })
}

/// Executes every exact I24-I27 checker directly from retained fixture bytes.
///
/// This entry point never derives a corpus from `seed` and never invokes the
/// current operation generator. The seed is retained metadata only.
pub fn run_vector_operation_from_fixture(
    bytes: &[u8],
) -> Result<VectorRetainedFixtureExecution, String> {
    let fixture = decode_vector_fixture(bytes)?;
    let comparisons = fixture
        .comparisons
        .iter()
        .map(|comparison| {
            independent::replay_canonical_comparison(
                &comparison.input_bytes,
                &comparison.observed_bytes,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(VectorRetainedFixtureExecution {
        fixture,
        comparisons,
    })
}

fn scheduled_fault_site(site: VectorGenericFaultSite) -> ScheduledFaultSite {
    match site {
        VectorGenericFaultSite::Open => ScheduledFaultSite::Open,
        VectorGenericFaultSite::Read => ScheduledFaultSite::Read,
        VectorGenericFaultSite::ReadRange => ScheduledFaultSite::ReadRange,
        VectorGenericFaultSite::Write => ScheduledFaultSite::Write,
        VectorGenericFaultSite::Append => ScheduledFaultSite::Append,
        VectorGenericFaultSite::Sync => ScheduledFaultSite::Sync,
        VectorGenericFaultSite::Rename => ScheduledFaultSite::Rename,
        VectorGenericFaultSite::List => ScheduledFaultSite::List,
        VectorGenericFaultSite::Delete => ScheduledFaultSite::Delete,
        VectorGenericFaultSite::Clock => ScheduledFaultSite::Clock,
    }
}

fn scheduled_fault_mode(mode: VectorGenericFaultMode) -> ScheduledFaultMode {
    match mode {
        VectorGenericFaultMode::Eio => ScheduledFaultMode::Eio,
        VectorGenericFaultMode::Eacces => ScheduledFaultMode::Eacces,
        VectorGenericFaultMode::Enospc => ScheduledFaultMode::Enospc,
        VectorGenericFaultMode::BitFlip => ScheduledFaultMode::BitFlip,
        VectorGenericFaultMode::TornWrite => ScheduledFaultMode::TornWrite,
        VectorGenericFaultMode::Truncate => ScheduledFaultMode::Truncate,
        VectorGenericFaultMode::WrongObject => ScheduledFaultMode::WrongObject,
        VectorGenericFaultMode::MisdirectedWrite => ScheduledFaultMode::MisdirectedWrite,
        VectorGenericFaultMode::ZeroFill => ScheduledFaultMode::ZeroFill,
        VectorGenericFaultMode::Latency => ScheduledFaultMode::Latency,
        VectorGenericFaultMode::SilentDrop => ScheduledFaultMode::SilentDrop,
        VectorGenericFaultMode::PostCommitError => ScheduledFaultMode::PostCommitError,
    }
}

fn materialize_fault_event(
    schedule: &VectorGenericFaultSchedule,
    program_op_index: usize,
) -> Result<ScheduledFaultEvent, String> {
    if schedule.id.is_empty() {
        return Err("vector generic fault id is empty".to_owned());
    }
    if schedule.nth_match == 0 {
        return Err("vector generic fault nth_match must be positive".to_owned());
    }
    Ok(ScheduledFaultEvent {
        id: schedule.id.clone(),
        op_index: program_op_index,
        layer: match schedule.mode {
            VectorGenericFaultMode::BitFlip
            | VectorGenericFaultMode::TornWrite
            | VectorGenericFaultMode::Truncate
            | VectorGenericFaultMode::WrongObject
            | VectorGenericFaultMode::MisdirectedWrite
            | VectorGenericFaultMode::ZeroFill
            | VectorGenericFaultMode::SilentDrop => ScheduledFaultLayer::Content,
            VectorGenericFaultMode::Eio
            | VectorGenericFaultMode::Eacces
            | VectorGenericFaultMode::Enospc
            | VectorGenericFaultMode::Latency
            | VectorGenericFaultMode::PostCommitError => ScheduledFaultLayer::Io,
        },
        site: scheduled_fault_site(schedule.site),
        mode: scheduled_fault_mode(schedule.mode),
        nth_match: schedule.nth_match,
        expected_matches: None,
        path_contains: schedule.path_contains.clone(),
        fired: false,
        fire_count: 0,
        path: None,
    })
}

fn observed_generic_event(
    schedule: &VectorGenericFaultSchedule,
    event: ScheduledFaultEvent,
) -> VectorGenericFaultEvent {
    VectorGenericFaultEvent {
        id: event.id,
        op_index: event.op_index,
        site: schedule.site,
        mode: schedule.mode,
        nth_match: event.nth_match,
        path_contains: event.path_contains,
        fired: event.fired,
        path: event.path.map(|path| path.display().to_string()),
    }
}

fn generic_store_failure(
    stage: VectorGenericFaultStage,
    error: StoreError,
) -> (VectorGenericFaultStage, VectorGenericFaultStatus) {
    (
        stage,
        VectorGenericFaultStatus::StoreFailure { kind: error.kind() },
    )
}

#[derive(Clone)]
struct GenericFaultLegRequest<'a> {
    directory: &'a Path,
    seed: u64,
    open_options: &'a OpenOptions,
    query: &'a [f32],
    ingest_vector: Option<&'a [f32]>,
    ingest_epoch: Option<zeppelin_embed::epoch::EpochIdentity>,
    tier: SearchTier,
    seal_ingested: bool,
}

fn run_generic_fault_leg(
    request: &GenericFaultLegRequest<'_>,
    scheduled: Arc<ScheduledVfs<StdVfs>>,
    feature: Option<&GenericFeatureController>,
) -> Result<(VectorGenericFaultStage, VectorGenericFaultStatus), String> {
    let dependencies = StoreTestDependencies::new(scheduled, Arc::new(SystemMonotonicClock));
    let dependencies = feature.map_or(dependencies.clone(), |controller| {
        controller.install(dependencies)
    });
    let store = match Store::open_with_test_dependencies(
        request.directory,
        request.open_options.clone(),
        dependencies.clone(),
    ) {
        Ok(store) => store,
        Err(error) => return Ok(generic_store_failure(VectorGenericFaultStage::Open, error)),
    };
    if let Some(ingest_vector) = request.ingest_vector {
        let version = DocumentVersion::new(
            DocId::new(u128::from(request.seed).wrapping_shl(64) | 0x5646_5347),
            Revision::new(1),
        );
        let batch = IngestBatch::new(vec![IngestDocument::new(version, ingest_vector.to_vec())]);
        let batch = match request.ingest_epoch {
            Some(epoch) => batch.with_epoch(epoch),
            None => batch,
        };
        if let Err(error) = store.ingest(batch) {
            return Ok(match error {
                IngestError::Store(error) => {
                    generic_store_failure(VectorGenericFaultStage::Ingest, error)
                }
                other => (
                    VectorGenericFaultStage::Ingest,
                    VectorGenericFaultStatus::IngestFailure {
                        status: ingest_status(other),
                    },
                ),
            });
        }
        if request.seal_ingested
            && let Err(error) = store.seal()
        {
            return Ok(generic_store_failure(VectorGenericFaultStage::Seal, error));
        }
    }
    if let Err(error) = store.close() {
        return Ok(generic_store_failure(VectorGenericFaultStage::Close, error));
    }
    let reopened = match Store::open_with_test_dependencies(
        request.directory,
        request.open_options.clone(),
        dependencies,
    ) {
        Ok(store) => store,
        Err(error) => {
            return Ok(generic_store_failure(
                VectorGenericFaultStage::Reopen,
                error,
            ));
        }
    };
    let generation = reopened
        .snapshot()
        .map_err(|error| format!("snapshot vector generic fault leg: {error}"))?
        .generation();
    let result = fact_with_generation(
        search_vector(&reopened, request.query, request.tier, 2),
        generation,
    )?;
    if let Err(error) = reopened.close() {
        return Ok(generic_store_failure(VectorGenericFaultStage::Close, error));
    }
    Ok((
        VectorGenericFaultStage::Query,
        VectorGenericFaultStatus::StoreResult(result),
    ))
}

fn generic_feature_controller(
    mutation: &VectorMutationEvidence,
    source: SegmentId,
) -> Result<(Option<GenericFeatureController>, VectorMutationEvidence), String> {
    let mapped_source = |original: independent::PrimitiveSource| match original {
        independent::PrimitiveSource::Active => (
            independent::PrimitiveSource::Active,
            VectorRowSource::Active,
        ),
        independent::PrimitiveSource::Sealed(_) => (
            independent::PrimitiveSource::Sealed(*source.as_bytes()),
            VectorRowSource::Sealed(*source.as_bytes()),
        ),
    };
    let (controller, mapped) = match mutation {
        VectorMutationEvidence::None => return Ok((None, VectorMutationEvidence::None)),
        VectorMutationEvidence::ForcedDispatch { case_id, requested } => {
            let requested = match requested {
                independent::BackendId::Scalar => KernelBackendId::Scalar,
                independent::BackendId::NeonWiden => KernelBackendId::NeonWiden,
                independent::BackendId::NeonDotprodU4 => KernelBackendId::NeonDotprodU4,
                independent::BackendId::NeonI8mm => KernelBackendId::NeonI8mm,
                independent::BackendId::NeonDotprodU2 => KernelBackendId::NeonDotprodU2,
                independent::BackendId::NeonDotprodU6 => KernelBackendId::NeonDotprodU6,
                independent::BackendId::NeonDotprodU8 => KernelBackendId::NeonDotprodU8,
                independent::BackendId::NeonDotprodU4Prefetch => {
                    KernelBackendId::NeonDotprodU4Prefetch
                }
                independent::BackendId::Avx2 => KernelBackendId::Avx2,
            };
            (
                GenericFeatureController::Kernel(KernelFaultController::forced_backend(
                    requested, *case_id,
                )),
                mutation.clone(),
            )
        }
        VectorMutationEvidence::QuantCorruption {
            case_id,
            scheme,
            source: original_source,
            tier,
            local_row,
            field,
        } => {
            let (mapped_source, product_source) = mapped_source(*original_source);
            let fault = match field {
                VectorQuantMutationField::Bit4OddPadding => VectorFault::CorruptBit4OddPadding {
                    source: product_source,
                    local_row: *local_row,
                },
                VectorQuantMutationField::Bit4Correction => VectorFault::CorruptBit4CorrectionNaN {
                    source: product_source,
                    local_row: *local_row,
                },
                VectorQuantMutationField::Int8Scale => VectorFault::CorruptInt8ScaleNaN {
                    source: product_source,
                    local_row: *local_row,
                },
            };
            (
                GenericFeatureController::Vector(VectorFaultController::armed(fault, *case_id)),
                VectorMutationEvidence::QuantCorruption {
                    case_id: *case_id,
                    scheme: *scheme,
                    source: mapped_source,
                    tier: *tier,
                    local_row: *local_row,
                    field: *field,
                },
            )
        }
        VectorMutationEvidence::MissingRescoreRows {
            case_id,
            source: original_source,
            tier,
            site,
            expected_rows,
            available_rows,
        } => {
            let (mapped_source, product_source) = mapped_source(*original_source);
            let product_site = match site {
                VectorRescoreMutationSite::ExactRescoreRows => MissingRescoreSite::ExactRescoreRows,
                VectorRescoreMutationSite::QueryRescoreRows => MissingRescoreSite::QueryRescoreRows,
            };
            (
                GenericFeatureController::Vector(VectorFaultController::armed(
                    VectorFault::MissingRescoreRows {
                        source: product_source,
                        site: product_site,
                        expected_rows: *expected_rows,
                        available_rows: *available_rows,
                        tier: product_vector_tier(*tier)?,
                    },
                    *case_id,
                )),
                VectorMutationEvidence::MissingRescoreRows {
                    case_id: *case_id,
                    source: mapped_source,
                    tier: *tier,
                    site: *site,
                    expected_rows: *expected_rows,
                    available_rows: *available_rows,
                },
            )
        }
        VectorMutationEvidence::RowCancellation {
            case_id,
            source: original_source,
            tier,
            requested_rows,
        } => {
            let (mapped_source, product_source) = mapped_source(*original_source);
            (
                GenericFeatureController::Vector(VectorFaultController::armed(
                    VectorFault::CancelAfterRows {
                        source: product_source,
                        requested_rows: *requested_rows,
                        tier: product_vector_tier(*tier)?,
                    },
                    *case_id,
                )),
                VectorMutationEvidence::RowCancellation {
                    case_id: *case_id,
                    source: mapped_source,
                    tier: *tier,
                    requested_rows: *requested_rows,
                },
            )
        }
        VectorMutationEvidence::AllocationDenial {
            case_id,
            component,
            items,
            bytes,
        } => (
            GenericFeatureController::Vector(VectorFaultController::armed(
                VectorFault::DenyGlobalCandidateAllocation {
                    items: *items,
                    bytes: *bytes,
                },
                *case_id,
            )),
            VectorMutationEvidence::AllocationDenial {
                case_id: *case_id,
                component,
                items: *items,
                bytes: *bytes,
            },
        ),
    };
    Ok((Some(controller), mapped))
}

fn product_vector_tier(tier: u8) -> Result<VectorSearchTier, String> {
    match tier {
        0 => Ok(VectorSearchTier::Auto),
        1 => Ok(VectorSearchTier::Exact),
        2 => Ok(VectorSearchTier::Scan),
        3 => Ok(VectorSearchTier::Graph),
        other => Err(format!("generic vector mutation has unknown tier {other}")),
    }
}

fn generic_feature_tier(mutation: &VectorMutationEvidence) -> Result<SearchTier, String> {
    let tier = match mutation {
        VectorMutationEvidence::MissingRescoreRows { tier, .. }
        | VectorMutationEvidence::RowCancellation { tier, .. } => *tier,
        VectorMutationEvidence::AllocationDenial { .. } => 1,
        VectorMutationEvidence::QuantCorruption { tier, .. } => *tier,
        VectorMutationEvidence::ForcedDispatch { .. } | VectorMutationEvidence::None => 2,
    };
    match tier {
        0 => Ok(SearchTier::Auto),
        1 => Ok(SearchTier::Exact),
        2 => Ok(SearchTier::Scan),
        3 => Ok(graph_tier(0)),
        other => Err(format!("generic vector mutation has unknown tier {other}")),
    }
}

fn run_generic_fault_evidence(
    operation: VectorOperationKind,
    seed: u64,
    feature_fault: Option<VectorFaultKind>,
    feature_mutation: &VectorMutationEvidence,
    context: &VectorExecutionContext,
    schedule: &VectorGenericFaultSchedule,
) -> Result<VectorGenericFaultEvidence, String> {
    let graph_ready = matches!(
        feature_mutation,
        VectorMutationEvidence::MissingRescoreRows { tier: 3, .. }
            | VectorMutationEvidence::RowCancellation { tier: 3, .. }
    );
    let (frozen, source, open_options, query, ingest_vector, ingest_epoch) = if graph_ready {
        let graph = graph_fixture(seed)?;
        let open_options = OpenOptions::default().with_epoch(graph.epoch.clone());
        let ingest_epoch = Some(graph.epoch.identity());
        (
            graph.frozen,
            graph.segment,
            open_options,
            graph.query,
            None,
            ingest_epoch,
        )
    } else {
        let directory =
            tempdir().map_err(|error| format!("vector generic base tempdir: {error}"))?;
        let base_scheme = match feature_mutation {
            VectorMutationEvidence::QuantCorruption {
                scheme: independent::QuantScheme::Int8,
                ..
            } => ProductQuantScheme::Int8,
            _ => ProductQuantScheme::Bit4,
        };
        let base = Store::open_with_test_dependencies(
            directory.path(),
            OpenOptions::default(),
            StoreTestDependencies::new(Arc::new(StdVfs), Arc::new(SystemMonotonicClock))
                .with_vector_seal_scheme(base_scheme),
        )
        .map_err(|error| format!("open vector generic base Store: {error}"))?;
        let fixture_rows = match feature_mutation {
            VectorMutationEvidence::MissingRescoreRows { expected_rows, .. } => *expected_rows,
            VectorMutationEvidence::RowCancellation { requested_rows, .. } => *requested_rows,
            VectorMutationEvidence::QuantCorruption { local_row, .. } => {
                local_row.saturating_add(1)
            }
            VectorMutationEvidence::ForcedDispatch { .. }
            | VectorMutationEvidence::AllocationDenial { .. }
            | VectorMutationEvidence::None => 2,
        }
        .max(1);
        let documents = (0..fixture_rows)
            .map(|row| {
                (
                    u128::from(seed).wrapping_shl(64) | 0x5646_4200 | u128::from(row),
                    1,
                    ROWS[row as usize % ROWS.len()],
                )
            })
            .collect::<Vec<_>>();
        ingest_rows(&base, &documents)?;
        base.seal()
            .map_err(|error| format!("seal vector generic base Store: {error}"))?;
        let source = base
            .snapshot()
            .map_err(|error| format!("snapshot vector generic base Store: {error}"))?
            .segments()
            .first()
            .ok_or_else(|| "vector generic base Store published no segment".to_owned())?
            .meta()
            .id;
        base.close()
            .map_err(|error| format!("close vector generic base Store: {error}"))?;
        (
            FrozenStoreFixture::capture(directory.path())?,
            source,
            OpenOptions::default(),
            QUERY.to_vec(),
            Some(QUERY.to_vec()),
            None,
        )
    };
    let pair = isolated_pair(&frozen)?;
    let event = materialize_fault_event(schedule, context.program_op_index)?;
    let clean_scheduled = Arc::new(ScheduledVfs::new(
        StdVfs,
        ScheduledFaultSchedule::single(event.clone()),
    ));
    let fault_scheduled = Arc::new(ScheduledVfs::new(
        StdVfs,
        ScheduledFaultSchedule::single(event),
    ));
    clean_scheduled.set_operation(context.program_op_index);
    fault_scheduled.set_operation(context.program_op_index);
    let (feature, mapped_feature_mutation) = generic_feature_controller(feature_mutation, source)?;
    let tier = generic_feature_tier(&mapped_feature_mutation)?;
    let seal_ingested = !matches!(
        mapped_feature_mutation,
        VectorMutationEvidence::RowCancellation {
            source: independent::PrimitiveSource::Active,
            ..
        }
    );
    let clean_request = GenericFaultLegRequest {
        directory: pair.clean.path(),
        seed,
        open_options: &open_options,
        query: &query,
        ingest_vector: ingest_vector.as_deref(),
        ingest_epoch,
        tier,
        seal_ingested,
    };
    let fault_request = GenericFaultLegRequest {
        directory: pair.fault.path(),
        ..clean_request.clone()
    };

    let (clean_stage, clean_status) =
        run_generic_fault_leg(&clean_request, Arc::clone(&clean_scheduled), None)?;
    let (fault_stage, fault_status) = run_generic_fault_leg(
        &fault_request,
        Arc::clone(&fault_scheduled),
        feature.as_ref(),
    )?;
    let clean_event = clean_scheduled
        .events()
        .into_iter()
        .next()
        .ok_or_else(|| "clean ScheduledVfs lost its vector schedule".to_owned())?;
    let fault_event = fault_scheduled
        .events()
        .into_iter()
        .next()
        .ok_or_else(|| "fault ScheduledVfs lost its vector schedule".to_owned())?;
    Ok(VectorGenericFaultEvidence {
        operation,
        feature_fault,
        feature_mutation: mapped_feature_mutation,
        program_op_index: context.program_op_index,
        schedule: schedule.clone(),
        clean: VectorGenericFaultLeg {
            stage: clean_stage,
            status: clean_status,
            event: observed_generic_event(schedule, clean_event),
            feature_receipts: Vec::new(),
        },
        fault: VectorGenericFaultLeg {
            stage: fault_stage,
            status: fault_status,
            event: observed_generic_event(schedule, fault_event),
            feature_receipts: feature
                .as_ref()
                .map_or_else(Vec::new, GenericFeatureController::take_typed_receipts),
        },
        clean_initial_directory: pair.clean_initial,
        fault_initial_directory: pair.fault_initial,
        isolated_directories: pair.clean.path() != pair.fault.path(),
        isolated_runtimes: !Arc::ptr_eq(&clean_scheduled, &fault_scheduled),
    })
}

/// Runs one vector operation and an optional operation-matching catalog fault.
pub fn run_vector_operation(
    operation: VectorOperationKind,
    seed: u64,
    fault: Option<VectorFaultKind>,
) -> Result<VectorOperationEvidence, String> {
    run_vector_operation_with_context(operation, seed, fault, VectorExecutionContext::default())
}

/// Runs one vector operation with an optional primitive generic VFS schedule.
pub fn run_vector_operation_with_context(
    operation: VectorOperationKind,
    seed: u64,
    fault: Option<VectorFaultKind>,
    context: VectorExecutionContext,
) -> Result<VectorOperationEvidence, String> {
    // Kernel score observation is process-global test support. Serialize every
    // vector operation, not only I24, so a concurrent Store query from another
    // vector corpus cannot populate I24's open observation scope.
    let _process_guard = vector_process_lock()
        .lock()
        .map_err(|_| "vector adapter process lock was poisoned".to_owned())?;
    if fault.is_some_and(|selected| selected.operation() != operation) {
        return Err(format!(
            "vector fault {fault:?} does not belong to operation {operation:?}"
        ));
    }
    let mut evidence = match operation {
        VectorOperationKind::KernelParity => run_kernel_parity(seed, fault),
        VectorOperationKind::Quantization => run_quantization(seed, fault),
        VectorOperationKind::Rescore => run_rescore(seed, fault),
        VectorOperationKind::RowIdentity => run_row_identity(seed, fault),
    }?;
    if let Some(schedule) = context.generic_fault.as_ref() {
        let feature_mutation = evidence.mutation.clone();
        evidence.generic_fault = Some(run_generic_fault_evidence(
            operation,
            seed,
            fault,
            &feature_mutation,
            &context,
            schedule,
        )?);
    }
    #[cfg(test)]
    apply_observation_plant(&mut evidence)?;
    Ok(evidence)
}

fn fact_sources(fact: &VectorStoreResultFact) -> Vec<independent::PrimitiveSource> {
    fact.candidates
        .iter()
        .map(|candidate| candidate.row.source)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn primitive_fixture(
    operation: VectorOperationKind,
    seed: u64,
    invariant: &VectorInvariantEvidence,
    extra_tiers: &[u8],
    extra_sources: &[independent::PrimitiveSource],
    public_schedule: Vec<independent::PublicStoreStep>,
) -> VectorPrimitiveFixture {
    let mut backends = BTreeSet::new();
    let mut tiers = extra_tiers.iter().copied().collect::<BTreeSet<_>>();
    let mut sources = extra_sources.iter().copied().collect::<BTreeSet<_>>();
    let mut documents = BTreeSet::new();
    let inputs = match invariant {
        VectorInvariantEvidence::I24(pairs) => {
            for pair in pairs {
                backends.insert(pair.input.backend);
            }
            VectorPrimitiveInputs::I24(pairs.iter().map(|pair| pair.input.clone()).collect())
        }
        VectorInvariantEvidence::I25(pairs) => {
            for pair in pairs {
                documents.extend(pair.input.store.document);
            }
            VectorPrimitiveInputs::I25(pairs.iter().map(|pair| pair.input.clone()).collect())
        }
        VectorInvariantEvidence::I26(pairs) => {
            for pair in pairs {
                let Some(store) = pair.input.store.as_ref() else {
                    continue;
                };
                tiers.insert(store.tier);
                sources.insert(store.source);
                documents.extend(store.documents_by_row.iter().flatten().copied());
            }
            VectorPrimitiveInputs::I26(pairs.iter().map(|pair| pair.input.clone()).collect())
        }
        VectorInvariantEvidence::I27(pairs) => {
            for pair in pairs {
                tiers.insert(pair.input.tier);
                for mutation in &pair.input.mutations {
                    match mutation {
                        independent::IdentityMutation::Ingest(document)
                        | independent::IdentityMutation::Replace(document) => {
                            documents.insert(*document);
                            sources.insert(independent::PrimitiveSource::Active);
                        }
                        independent::IdentityMutation::Seal(segment) => {
                            sources.insert(independent::PrimitiveSource::Sealed(*segment));
                        }
                        independent::IdentityMutation::Delete {
                            doc_id_be,
                            revision,
                        } => {
                            documents.insert(independent::PrimitiveDocument {
                                doc_id_be: *doc_id_be,
                                revision: *revision,
                            });
                        }
                        independent::IdentityMutation::Reopen => {}
                    }
                }
            }
            VectorPrimitiveInputs::I27(pairs.iter().map(|pair| pair.input.clone()).collect())
        }
    };
    let features = zeppelin_embed::kernels::detected_features();
    VectorPrimitiveFixture {
        namespace: "vector-execution-v1",
        seed,
        operation: operation.key(),
        inputs,
        backend_inventory: backends.into_iter().collect(),
        tier_inventory: tiers.into_iter().collect(),
        source_inventory: sources.into_iter().collect(),
        documents: documents.into_iter().collect(),
        public_schedule,
        runtime_features: VectorRuntimeFeatures {
            neon: features.neon,
            dotprod: features.dotprod,
            fp16: features.fp16,
            i8mm: features.i8mm,
            sme2: features.sme2,
            avx2: features.avx2,
            popcnt: features.popcnt,
        },
    }
}

fn search_options(tier: SearchTier) -> SearchOptions {
    SearchOptions::new(ScanOptions { thread_budget: 1 }).with_tier(tier)
}

fn search(store: &Store, tier: SearchTier, k: usize) -> Result<SearchOutcome, QueryError> {
    search_vector(store, &QUERY, tier, k)
}

fn search_vector(
    store: &Store,
    query: &[f32],
    tier: SearchTier,
    k: usize,
) -> Result<SearchOutcome, QueryError> {
    store.search(
        SearchRequest::new(query),
        k,
        search_options(tier),
        QueryControl::Cancel(CancelToken::new()),
    )
}

fn vector_dependencies(controller: VectorFaultController) -> StoreTestDependencies {
    StoreTestDependencies::new(Arc::new(StdVfs), Arc::new(SystemMonotonicClock))
        .with_vector_fault_controller(controller)
}

fn kernel_dependencies(controller: KernelFaultController) -> StoreTestDependencies {
    StoreTestDependencies::new(Arc::new(StdVfs), Arc::new(SystemMonotonicClock))
        .with_kernel_fault_controller(controller)
}

fn primitive_document(document: DocumentVersion) -> independent::PrimitiveDocument {
    independent::PrimitiveDocument {
        doc_id_be: document.doc_id().get().to_be_bytes(),
        revision: document.revision().get(),
    }
}

fn primitive_source(source: RowSource) -> independent::PrimitiveSource {
    match source {
        RowSource::Active => independent::PrimitiveSource::Active,
        RowSource::Sealed(segment) => independent::PrimitiveSource::Sealed(*segment.as_bytes()),
    }
}

fn candidate_rows(
    outcome: &SearchOutcome,
) -> Result<Vec<independent::IdentityObservedRow>, String> {
    outcome
        .candidates
        .iter()
        .map(|candidate| {
            Ok(independent::IdentityObservedRow {
                row: independent::PrimitiveRow {
                    source: primitive_source(candidate.row_id().source()),
                    local_row: candidate.row_id().local_row(),
                },
                document: candidate.document().map(primitive_document),
                score: independent::F32::from_float(candidate.score()),
            })
        })
        .collect()
}

fn result_fact(result: Result<SearchOutcome, QueryError>) -> Result<VectorStoreResultFact, String> {
    match result {
        Ok(outcome) => Ok(VectorStoreResultFact {
            status: independent::PrimitiveStatus::Ok,
            generation: outcome.generation,
            candidates: candidate_rows(&outcome)?,
            dims_touched: outcome.stats.dims_touched,
            bytes_read: outcome.stats.bytes_read,
            exact_rescore: outcome.diagnostics.exact_rescore,
            approximate: outcome.diagnostics.approximate,
            returned: u64::try_from(outcome.diagnostics.returned)
                .map_err(|_| "Store returned count exceeds u64".to_owned())?,
        }),
        Err(error) => Ok(VectorStoreResultFact {
            status: query_status(error),
            generation: 0,
            candidates: Vec::new(),
            dims_touched: 0,
            bytes_read: 0,
            exact_rescore: false,
            approximate: false,
            returned: 0,
        }),
    }
}

fn query_status(error: QueryError) -> independent::PrimitiveStatus {
    match error {
        QueryError::Cancelled { partial } => independent::PrimitiveStatus::Cancelled { partial },
        QueryError::Scan(ScanError::Quant(error)) => quant_status(error),
        QueryError::Scan(ScanError::NonFiniteScore { row_id }) => {
            independent::PrimitiveStatus::NonFiniteScore { row: row_id as u64 }
        }
        QueryError::Store(StoreError::AllocationFailed { component, needed }) => {
            independent::PrimitiveStatus::AllocationFailed {
                component: component.to_owned(),
                needed,
            }
        }
        QueryError::Store(StoreError::Segment(
            zeppelin_embed::segment::SegmentError::Geometry(detail),
        )) => independent::PrimitiveStatus::SegmentGeometry { detail },
        other => independent::PrimitiveStatus::SegmentGeometry {
            detail: other.to_string(),
        },
    }
}

fn quant_status(error: QuantError) -> independent::PrimitiveStatus {
    match error {
        QuantError::EmptyVector => independent::PrimitiveStatus::EmptyVector,
        QuantError::DimensionTooLarge { actual, maximum } => {
            independent::PrimitiveStatus::DimensionTooLarge {
                actual: actual as u64,
                maximum: maximum as u64,
            }
        }
        QuantError::NonFinite { index } => independent::PrimitiveStatus::NonFinite {
            index: index as u64,
        },
        QuantError::OutputLength { expected, actual } => {
            independent::PrimitiveStatus::OutputLength {
                expected: expected as u64,
                actual: actual as u64,
            }
        }
        QuantError::CodeLength { expected, actual } => independent::PrimitiveStatus::CodeLength {
            expected: expected as u64,
            actual: actual as u64,
        },
        QuantError::NonZeroPadding { byte, mask } => {
            independent::PrimitiveStatus::NonZeroPadding { byte, mask }
        }
    }
}

fn ingest_rows(store: &Store, documents: &[(u128, u64, [f32; 3])]) -> Result<(), String> {
    store
        .ingest(IngestBatch::new(
            documents
                .iter()
                .map(|(document, revision, vector)| {
                    IngestDocument::new(
                        DocumentVersion::new(DocId::new(*document), Revision::new(*revision)),
                        vector.to_vec(),
                    )
                })
                .collect(),
        ))
        .map(|_| ())
        .map_err(|error| format!("ingest vector fixture: {error}"))
}

fn backend_id(backend: KernelBackendId) -> independent::BackendId {
    match backend {
        KernelBackendId::Scalar => independent::BackendId::Scalar,
        KernelBackendId::NeonWiden => independent::BackendId::NeonWiden,
        KernelBackendId::NeonDotprodU4 => independent::BackendId::NeonDotprodU4,
        KernelBackendId::NeonI8mm => independent::BackendId::NeonI8mm,
        KernelBackendId::NeonDotprodU2 => independent::BackendId::NeonDotprodU2,
        KernelBackendId::NeonDotprodU6 => independent::BackendId::NeonDotprodU6,
        KernelBackendId::NeonDotprodU8 => independent::BackendId::NeonDotprodU8,
        KernelBackendId::NeonDotprodU4Prefetch => independent::BackendId::NeonDotprodU4Prefetch,
        KernelBackendId::Avx2 => independent::BackendId::Avx2,
    }
}

fn kernel_id(kernel: KernelOperationId) -> independent::KernelId {
    match kernel {
        KernelOperationId::DotI8 => independent::KernelId::DotI8,
        KernelOperationId::HammingU1 => independent::KernelId::HammingU1,
        KernelOperationId::DotF32 => independent::KernelId::DotF32,
        KernelOperationId::DotF16 => independent::KernelId::DotF16,
        KernelOperationId::DotI8Batch => independent::KernelId::DotI8Batch,
        KernelOperationId::HammingU1Batch => independent::KernelId::HammingU1Batch,
        KernelOperationId::DotBit4 => independent::KernelId::DotBit4,
        KernelOperationId::DotBit4Prepared => independent::KernelId::DotBit4Prepared,
        KernelOperationId::DotBit4Batch => independent::KernelId::DotBit4Batch,
        KernelOperationId::ScoreBit4PreparedBatch => independent::KernelId::ScoreBit4PreparedBatch,
        KernelOperationId::ScoreBit4Ptrs => independent::KernelId::ScoreBit4Ptrs,
    }
}

fn kernel_value(value: &KernelScoreValue) -> independent::KernelValue {
    match value {
        KernelScoreValue::I32(value) => independent::KernelValue::S32(*value),
        KernelScoreValue::U32(value) => independent::KernelValue::U32(*value),
        KernelScoreValue::F32(value) => independent::KernelValue::F32(independent::F32(*value)),
        KernelScoreValue::I32s(values) => independent::KernelValue::S32s(values.clone()),
        KernelScoreValue::U32s(values) => independent::KernelValue::U32s(values.clone()),
        KernelScoreValue::F32s(values) => {
            independent::KernelValue::F32s(values.iter().copied().map(independent::F32).collect())
        }
    }
}

const KERNELS: [independent::KernelId; 11] = [
    independent::KernelId::DotI8,
    independent::KernelId::HammingU1,
    independent::KernelId::DotF32,
    independent::KernelId::DotF16,
    independent::KernelId::DotI8Batch,
    independent::KernelId::HammingU1Batch,
    independent::KernelId::DotBit4,
    independent::KernelId::DotBit4Prepared,
    independent::KernelId::DotBit4Batch,
    independent::KernelId::ScoreBit4PreparedBatch,
    independent::KernelId::ScoreBit4Ptrs,
];

fn pack_kernel_rows(dimension: usize, rows: usize, seed: u64) -> Vec<u8> {
    let row_bytes = dimension.div_ceil(2);
    let mut packed = vec![0_u8; row_bytes.saturating_mul(rows)];
    for row in 0..rows {
        for coordinate in 0..dimension {
            let nibble = ((seed as usize + row * 5 + coordinate * 3) % 16) as u8;
            let byte = &mut packed[row * row_bytes + coordinate / 2];
            if coordinate.is_multiple_of(2) {
                *byte |= nibble << 4;
            } else {
                *byte |= nibble;
            }
        }
    }
    packed
}

fn prepare_kernel_query(query: &[i8]) -> Vec<i8> {
    let mut prepared = Vec::with_capacity(query.len());
    for block in query.chunks(32) {
        prepared.extend(block.iter().step_by(2).copied());
        prepared.extend(block.iter().skip(1).step_by(2).copied());
    }
    prepared
}

fn seeded_word(seed: u64, coordinate: usize) -> u64 {
    let mut value = seed
        .wrapping_add((coordinate as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15))
        .wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn seeded_raw_finite_f32_bits(seed: u64, coordinate: usize, terminal: bool) -> u32 {
    let mixed = seeded_word(seed, coordinate) as u32;
    let sign = mixed & 0x8000_0000;
    let mantissa = mixed & 0x007f_ffff;
    let biased_exponent = if terminal {
        135
    } else {
        119 + ((mixed >> 23) % 17)
    };
    sign | (biased_exponent << 23) | mantissa
}

fn legacy_seeded_raw_finite_f32_bits(seed: u64, coordinate: usize) -> u32 {
    let mixed = seeded_word(seed, coordinate) as u32;
    let sign = mixed & 0x8000_0000;
    let mantissa = mixed & 0x007f_ffff;
    let biased_exponent = 112 + ((mixed >> 23) & 0x1f);
    sign | (biased_exponent << 23) | mantissa
}

fn seeded_raw_finite_f16_bits(seed: u64, coordinate: usize, terminal: bool) -> u16 {
    let mixed = seeded_word(seed, coordinate) as u16;
    let sign = mixed & 0x8000;
    let mantissa = mixed & 0x03ff;
    let biased_exponent = if terminal {
        19
    } else {
        11 + ((mixed >> 10) % 9)
    };
    sign | (biased_exponent << 10) | mantissa
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FloatKernelCorpus {
    SeededFinite,
    Cancellation,
    Special,
}

const FLOAT_KERNEL_CORPORA: [Option<FloatKernelCorpus>; 3] = [
    Some(FloatKernelCorpus::SeededFinite),
    Some(FloatKernelCorpus::Cancellation),
    Some(FloatKernelCorpus::Special),
];
const NON_FLOAT_KERNEL_CORPUS: [Option<FloatKernelCorpus>; 1] = [None];

fn kernel_input(
    case_id: u64,
    backend: independent::BackendId,
    kernel: independent::KernelId,
    dimension: usize,
    seed: u64,
    selected_for_store: bool,
    float_corpus: Option<FloatKernelCorpus>,
) -> independent::KernelInput {
    let coordinate_query = (0..dimension)
        .map(|index| ((index as i32 * 17 + seed as i32) % 127 - 63) as i8)
        .collect::<Vec<_>>();
    let signed_row = (0..dimension)
        .map(|index| ((index as i32 * 29 - seed as i32) % 127 - 63) as i8)
        .collect::<Vec<_>>();
    let batch_rows = match kernel {
        independent::KernelId::ScoreBit4Ptrs => 4_usize,
        independent::KernelId::DotI8Batch
        | independent::KernelId::HammingU1Batch
        | independent::KernelId::DotBit4Batch
        | independent::KernelId::ScoreBit4PreparedBatch => 3_usize,
        _ => 0,
    };
    let bytes_a = (0..dimension)
        .map(|index| (seed as u8).wrapping_add((index * 37) as u8))
        .collect::<Vec<_>>();
    let hamming_rows = (0..dimension.saturating_mul(batch_rows.max(1)))
        .map(|index| (seed as u8).wrapping_add((index * 19) as u8))
        .collect::<Vec<_>>();
    let bit4_rows = pack_kernel_rows(dimension, batch_rows.max(1), seed);
    let (f32_a, f32_b, f16_a, f16_b) = match float_corpus {
        Some(FloatKernelCorpus::SeededFinite) => (
            (0..dimension)
                .map(|index| {
                    independent::F32(seeded_raw_finite_f32_bits(
                        seed,
                        index,
                        index + 1 == dimension,
                    ))
                })
                .collect(),
            (0..dimension)
                .map(|index| {
                    independent::F32(seeded_raw_finite_f32_bits(
                        seed ^ 0xa5a5_a5a5_a5a5_a5a5,
                        index,
                        index + 1 == dimension,
                    ))
                })
                .collect(),
            (0..dimension)
                .map(|index| seeded_raw_finite_f16_bits(seed, index, index + 1 == dimension))
                .collect(),
            (0..dimension)
                .map(|index| {
                    seeded_raw_finite_f16_bits(
                        seed ^ 0xa5a5_a5a5_a5a5_a5a5,
                        index,
                        index + 1 == dimension,
                    )
                })
                .collect(),
        ),
        Some(FloatKernelCorpus::Cancellation) => {
            let f32_pattern = [1.0e20_f32, 1.0, -1.0e20, -1.0];
            let f16_pattern = [0x5c00_u16, 0x3c00, 0xdc00, 0xbc00];
            (
                (0..dimension)
                    .map(|index| {
                        independent::F32::from_float(f32_pattern[index % f32_pattern.len()])
                    })
                    .collect(),
                vec![independent::F32::from_float(1.0); dimension],
                (0..dimension)
                    .map(|index| f16_pattern[index % f16_pattern.len()])
                    .collect(),
                vec![0x5c00; dimension],
            )
        }
        Some(FloatKernelCorpus::Special) => {
            let f32_pattern = [
                0x8000_0000,
                0x0000_0001,
                f32::INFINITY.to_bits(),
                f32::NEG_INFINITY.to_bits(),
                f32::NAN.to_bits(),
                1.0_f32.to_bits(),
            ];
            let f16_pattern = [0x8000_u16, 0x0001, 0x7c00, 0xfc00, 0x7e00, 0x3c00];
            (
                (0..dimension)
                    .map(|index| independent::F32(f32_pattern[index % f32_pattern.len()]))
                    .collect(),
                vec![independent::F32::from_float(1.0); dimension],
                (0..dimension)
                    .map(|index| f16_pattern[index % f16_pattern.len()])
                    .collect(),
                vec![0x3c00; dimension],
            )
        }
        None => {
            let f32_a = (0..dimension)
                .map(|index| {
                    let bits = match index % 12 {
                        0 => 1.0e20_f32.to_bits(),
                        1 => 1.0_f32.to_bits(),
                        2 => (-1.0e20_f32).to_bits(),
                        3 => (-1.0_f32).to_bits(),
                        4 => 0x8000_0000,
                        5 => 0x0000_0001,
                        6 | 7 | 11 => legacy_seeded_raw_finite_f32_bits(seed, index),
                        8 => f32::INFINITY.to_bits(),
                        9 => f32::NEG_INFINITY.to_bits(),
                        10 => f32::NAN.to_bits(),
                        _ => unreachable!(),
                    };
                    independent::F32(bits)
                })
                .collect();
            let f32_b = (0..dimension)
                .map(|index| {
                    let bits = if index % 12 < 4 {
                        1.0_f32.to_bits()
                    } else {
                        legacy_seeded_raw_finite_f32_bits(seed ^ 0xa5a5_a5a5_a5a5_a5a5, index)
                    };
                    independent::F32(bits)
                })
                .collect();
            let f16_pattern = [
                0x8000_u16, 0x0001, 0x8001, 0x3c00, 0xbc00, 0x7c00, 0xfc00, 0x7e00, 0x0400, 0x7bff,
            ];
            let f16_a = (0..dimension)
                .map(|index| f16_pattern[index % f16_pattern.len()])
                .collect();
            let f16_b = (0..dimension)
                .map(|index| f16_pattern[(index + 2) % f16_pattern.len()])
                .collect();
            (f32_a, f32_b, f16_a, f16_b)
        }
    };
    let bit4_factors = (0..batch_rows.max(1))
        .map(|row| {
            [
                independent::F32::from_float(0.5 + row as f32 / 8.0),
                independent::F32::from_float(1.0 + row as f32 / 4.0),
                independent::F32::from_float(0.25 + row as f32 / 16.0),
            ]
        })
        .collect::<Vec<_>>();
    let prepared = prepare_kernel_query(&coordinate_query);
    let query_sum = coordinate_query.iter().map(|value| i32::from(*value)).sum();
    let signed_a = match kernel {
        independent::KernelId::DotBit4Prepared
        | independent::KernelId::ScoreBit4PreparedBatch
        | independent::KernelId::ScoreBit4Ptrs => prepared,
        _ => coordinate_query,
    };
    let signed_b = if kernel == independent::KernelId::DotI8Batch {
        (0..batch_rows)
            .flat_map(|row| {
                signed_row
                    .iter()
                    .map(move |value| value.wrapping_add(row as i8))
            })
            .collect()
    } else {
        signed_row
    };
    let bytes_b = match kernel {
        independent::KernelId::HammingU1 => hamming_rows[..dimension].to_vec(),
        independent::KernelId::HammingU1Batch => hamming_rows,
        _ => bit4_rows,
    };
    independent::KernelInput {
        case_id,
        backend,
        selected_for_store,
        work_items: match kernel {
            independent::KernelId::DotI8Batch
            | independent::KernelId::HammingU1Batch
            | independent::KernelId::DotBit4Batch
            | independent::KernelId::ScoreBit4PreparedBatch
            | independent::KernelId::ScoreBit4Ptrs => dimension.saturating_mul(batch_rows) as u64,
            _ => dimension as u64,
        },
        kernel,
        dimension: dimension as u64,
        signed_a,
        signed_b,
        bytes_a,
        bytes_b,
        f32_a,
        f32_b,
        f16_a,
        f16_b,
        row_bytes: dimension.div_ceil(2) as u64,
        batch_rows: batch_rows as u64,
        pointer_order: if kernel == independent::KernelId::ScoreBit4Ptrs {
            vec![2, 0, 3, 1]
        } else {
            Vec::new()
        },
        query_sum,
        query_scale_half: independent::F64::from_float(0.5),
        bit4_factors,
    }
}

fn observe_kernel(
    variant: KernelVariant,
    input: &independent::KernelInput,
) -> Result<independent::KernelValue, String> {
    let dimension = usize::try_from(input.dimension).map_err(|_| "kernel dimension overflow")?;
    let rows = usize::try_from(input.batch_rows).map_err(|_| "kernel row count overflow")?;
    let offset = input.input_offset();
    let with_offset = |values: &[u8]| {
        let mut backing = vec![0_u8; offset];
        backing.extend_from_slice(values);
        backing
    };
    match input.kernel {
        independent::KernelId::DotI8 => {
            let mut left = vec![0_i8; offset];
            left.extend_from_slice(&input.signed_a);
            let mut right = vec![0_i8; offset];
            right.extend_from_slice(&input.signed_b);
            Ok(independent::KernelValue::S32(
                variant.dot_i8(&left[offset..], &right[offset..]),
            ))
        }
        independent::KernelId::HammingU1 => {
            let left = with_offset(&input.bytes_a);
            let right = with_offset(&input.bytes_b);
            Ok(independent::KernelValue::U32(
                variant.hamming_u1(&left[offset..], &right[offset..]),
            ))
        }
        independent::KernelId::DotF32 => {
            let mut left = vec![0.0_f32; offset];
            left.extend(input.f32_a.iter().map(|value| value.to_float()));
            let mut right = vec![0.0_f32; offset];
            right.extend(input.f32_b.iter().map(|value| value.to_float()));
            Ok(independent::KernelValue::F32(independent::F32::from_float(
                variant.dot_f32(&left[offset..], &right[offset..]),
            )))
        }
        independent::KernelId::DotF16 => {
            let mut left = vec![0_u16; offset];
            left.extend_from_slice(&input.f16_a);
            let mut right = vec![0_u16; offset];
            right.extend_from_slice(&input.f16_b);
            Ok(independent::KernelValue::F32(independent::F32::from_float(
                variant.dot_f16(&left[offset..], &right[offset..]),
            )))
        }
        independent::KernelId::DotI8Batch => {
            let mut query = vec![0_i8; offset];
            query.extend_from_slice(&input.signed_a);
            let mut row_values = vec![0_i8; offset];
            row_values.extend_from_slice(&input.signed_b);
            let mut output = vec![0_i32; rows + offset];
            variant.dot_i8_batch(
                &query[offset..],
                &row_values[offset..],
                dimension,
                &mut output[offset..],
            );
            Ok(independent::KernelValue::S32s(output[offset..].to_vec()))
        }
        independent::KernelId::HammingU1Batch => {
            let query = with_offset(&input.bytes_a);
            let row_values = with_offset(&input.bytes_b);
            let mut output = vec![0_u32; rows + offset];
            variant.hamming_u1_batch(
                &query[offset..],
                &row_values[offset..],
                dimension,
                &mut output[offset..],
            );
            Ok(independent::KernelValue::U32s(output[offset..].to_vec()))
        }
        independent::KernelId::DotBit4 => {
            let mut query = vec![0_i8; offset];
            query.extend_from_slice(&input.signed_a);
            let codes = with_offset(&input.bytes_b);
            Ok(independent::KernelValue::S32(
                variant.dot_bit4(&query[offset..], &codes[offset..]),
            ))
        }
        independent::KernelId::DotBit4Prepared => {
            let mut query = vec![0_i8; offset];
            query.extend_from_slice(&input.signed_a);
            let codes = with_offset(&input.bytes_b);
            Ok(independent::KernelValue::S32(variant.dot_bit4_prepared(
                &query[offset..],
                input.query_sum,
                &codes[offset..],
            )))
        }
        independent::KernelId::DotBit4Batch => {
            let mut query = vec![0_i8; offset];
            query.extend_from_slice(&input.signed_a);
            let row_values = with_offset(&input.bytes_b);
            let mut output = vec![0_i32; rows + offset];
            variant.dot_bit4_batch(
                &query[offset..],
                &row_values[offset..],
                dimension,
                &mut output[offset..],
            );
            Ok(independent::KernelValue::S32s(output[offset..].to_vec()))
        }
        independent::KernelId::ScoreBit4PreparedBatch => {
            let factors = input
                .bit4_factors
                .iter()
                .map(|fields| {
                    Bit4Factors::from_persisted(
                        fields[0].to_float(),
                        fields[1].to_float(),
                        fields[2].to_float(),
                    )
                })
                .collect::<Vec<_>>();
            let mut query = vec![0_i8; offset];
            query.extend_from_slice(&input.signed_a);
            let row_values = with_offset(&input.bytes_b);
            let mut output = vec![0.0_f32; rows + offset];
            variant.score_bit4_prepared_batch(
                (
                    &query[offset..],
                    input.query_sum,
                    input.query_scale_half.to_float(),
                ),
                &row_values[offset..],
                dimension,
                &factors,
                &mut output[offset..],
            );
            Ok(independent::KernelValue::F32s(
                output[offset..]
                    .iter()
                    .copied()
                    .map(independent::F32::from_float)
                    .collect(),
            ))
        }
        independent::KernelId::ScoreBit4Ptrs => {
            let row_bytes = dimension.div_ceil(2);
            let order = input
                .pointer_order
                .iter()
                .map(|row| usize::from(*row))
                .collect::<Vec<_>>();
            let mut query = vec![0_i8; offset];
            query.extend_from_slice(&input.signed_a);
            let row_values = with_offset(&input.bytes_b);
            let row_handles = std::array::from_fn(|slot| {
                Bit4Row::from_mapped_region(
                    &row_values,
                    offset + order[slot] * row_bytes,
                    row_bytes,
                )
                .expect("independent-validated pointer row")
            });
            let rows4 =
                Bit4Rows4::from_rows(row_handles, row_bytes).map_err(|error| error.to_string())?;
            let factors = std::array::from_fn(|slot| {
                let fields = input.bit4_factors[order[slot]];
                Bit4Factors::from_persisted(
                    fields[0].to_float(),
                    fields[1].to_float(),
                    fields[2].to_float(),
                )
            });
            let mut output = [0.0_f32; 4];
            variant
                .score_bit4_ptrs(
                    (
                        &query[offset..],
                        input.query_sum,
                        input.query_scale_half.to_float(),
                    ),
                    &rows4,
                    &factors,
                    &mut output,
                )
                .map_err(|error| error.to_string())?;
            Ok(independent::KernelValue::F32s(
                output
                    .into_iter()
                    .map(independent::F32::from_float)
                    .collect(),
            ))
        }
    }
}

fn store_kernel_input(
    case_id: u64,
    observation: &KernelScoreObservation,
    codes: Vec<u8>,
    factors: Vec<[f32; 3]>,
) -> Result<independent::KernelInput, String> {
    let query = prepare_bit4_query(&QUERY, 0)
        .map_err(|error| format!("prepare Store Bit4 query: {error}"))?;
    let (query_codes, query_sum, query_scale_half) = query.observation_parts();
    let kernel = kernel_id(observation.kernel());
    if kernel != independent::KernelId::ScoreBit4PreparedBatch {
        return Err(format!(
            "public sealed Bit4 Scan selected unexpected kernel {kernel:?}"
        ));
    }
    let batch_rows = match observation.value() {
        KernelScoreValue::F32s(values) => values.len(),
        other => {
            return Err(format!(
                "public Store Bit4 batch returned non-batch value {other:?}"
            ));
        }
    };
    if batch_rows == 0 || batch_rows > factors.len() {
        return Err(format!(
            "public Store kernel batch rows {batch_rows} exceed persisted rows {}",
            factors.len()
        ));
    }
    let expected_work = QUERY
        .len()
        .checked_mul(batch_rows)
        .and_then(|work| u64::try_from(work).ok())
        .ok_or_else(|| "public Store kernel work overflow".to_owned())?;
    if observation.work_items() != expected_work {
        return Err(format!(
            "public Store kernel work {} differs from bound input work {expected_work}",
            observation.work_items()
        ));
    }
    // The Store fixture pins one scan worker. Its first dispatched batch starts
    // at sealed local row zero, so the first observed result binds this exact
    // persisted prefix rather than assuming the whole segment was one batch.
    let row_bytes = QUERY.len().div_ceil(2);
    let code_bytes = row_bytes
        .checked_mul(batch_rows)
        .ok_or_else(|| "public Store kernel code prefix overflow".to_owned())?;
    let codes = codes
        .get(..code_bytes)
        .ok_or_else(|| "public Store kernel code prefix exceeds persisted codes".to_owned())?
        .to_vec();
    let factors = factors
        .get(..batch_rows)
        .ok_or_else(|| "public Store kernel factor prefix exceeds persisted factors".to_owned())?
        .to_vec();
    Ok(independent::KernelInput {
        case_id,
        backend: backend_id(observation.backend()),
        selected_for_store: true,
        work_items: observation.work_items(),
        kernel,
        dimension: QUERY.len() as u64,
        signed_a: query_codes.to_vec(),
        signed_b: Vec::new(),
        bytes_a: Vec::new(),
        bytes_b: codes,
        f32_a: Vec::new(),
        f32_b: Vec::new(),
        f16_a: Vec::new(),
        f16_b: Vec::new(),
        row_bytes: row_bytes as u64,
        batch_rows: batch_rows as u64,
        pointer_order: Vec::new(),
        query_sum,
        query_scale_half: independent::F64::from_float(query_scale_half),
        bit4_factors: factors
            .into_iter()
            .map(|fields| fields.map(independent::F32::from_float))
            .collect(),
    })
}

fn store_kernel_pair(
    case_id: u64,
    observation: &KernelScoreObservation,
    codes: Vec<u8>,
    factors: Vec<[f32; 3]>,
) -> Result<I24EvidencePair, String> {
    if !observation.result_published() {
        return Err("public Store kernel observation was not published".to_owned());
    }
    let input = store_kernel_input(case_id, observation, codes, factors)?;
    let observed = independent::I24Observed {
        case_id,
        backend: backend_id(observation.backend()),
        kernel: kernel_id(observation.kernel()),
        value: kernel_value(observation.value()),
        selected_for_store: true,
        work_items: observation.work_items(),
    };
    Ok(I24EvidencePair { input, observed })
}

fn raw_persisted_quantization(
    directory: &Path,
) -> Result<independent::PersistedQuantization, String> {
    let mut segment_paths = std::fs::read_dir(directory)
        .map_err(|error| format!("list persisted vector directory: {error}"))?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| format!("read persisted vector directory entry: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    segment_paths.retain(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("segment-") && name.ends_with(".zseg"))
    });
    segment_paths.sort();
    if segment_paths.len() != 1 {
        return Err(format!(
            "persisted vector fixture has {} segment files, expected one",
            segment_paths.len()
        ));
    }
    let bytes = std::fs::read(&segment_paths[0])
        .map_err(|error| format!("read persisted vector bytes: {error}"))?;
    independent::parse_persisted_quantization(&bytes)
        .map_err(|error| format!("independently parse persisted vector bytes: {error}"))
}

fn persisted_bit4_input(directory: &Path) -> Result<(Vec<u8>, Vec<[f32; 3]>), String> {
    let persisted = raw_persisted_quantization(directory)?;
    if persisted.scheme != independent::QuantScheme::Bit4 {
        return Err(format!(
            "Store kernel fixture persisted {:?}, expected Bit4",
            persisted.scheme
        ));
    }
    let factors = persisted
        .factor_bits
        .chunks_exact(3)
        .map(|fields| {
            let fields: [independent::F32; 3] = fields
                .try_into()
                .map_err(|_| "persisted Bit4 factor width changed".to_owned())?;
            Ok(fields.map(independent::F32::to_float))
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok((persisted.code_bytes, factors))
}

fn choose_forced_backend() -> Result<KernelBackendId, String> {
    let available = KernelVariant::available()
        .map(KernelVariant::backend_id)
        .collect::<BTreeSet<_>>();
    if available.is_empty() {
        return Err("kernel backend inventory is empty".to_owned());
    }
    Ok(if available.contains(&KernelBackendId::NeonDotprodU4) {
        KernelBackendId::NeonDotprodU4
    } else if available.contains(&KernelBackendId::NeonWiden) {
        KernelBackendId::NeonWiden
    } else if available.contains(&KernelBackendId::Avx2) {
        KernelBackendId::Avx2
    } else {
        KernelBackendId::Scalar
    })
}

fn vector_process_lock() -> &'static Mutex<()> {
    super::feature_process_lock()
}

struct IsolatedStorePair {
    clean: TempDir,
    fault: TempDir,
    clean_initial: VectorFixtureDirectoryEvidence,
    fault_initial: VectorFixtureDirectoryEvidence,
}

fn vector_fixture_digest(bytes: &[u8]) -> u64 {
    let mut digest = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        digest ^= u64::from(*byte);
        digest = digest.wrapping_mul(0x0000_0100_0000_01b3);
    }
    digest
}

fn frozen_fixture_evidence(
    fixture: &FrozenStoreFixture,
) -> Result<VectorFixtureDirectoryEvidence, String> {
    let mut canonical = Vec::new();
    let mut files = Vec::with_capacity(fixture.files().len());
    for file in fixture.files() {
        let relative_path = file
            .relative_path
            .to_str()
            .ok_or_else(|| "vector fixture relative path is not UTF-8".to_owned())?
            .replace(std::path::MAIN_SEPARATOR, "/");
        canonical.extend_from_slice(
            &u64::try_from(relative_path.len())
                .map_err(|_| "vector fixture path length exceeds u64".to_owned())?
                .to_le_bytes(),
        );
        canonical.extend_from_slice(relative_path.as_bytes());
        let byte_length = u64::try_from(file.bytes.len())
            .map_err(|_| "vector fixture file length exceeds u64".to_owned())?;
        let digest = vector_fixture_digest(&file.bytes);
        canonical.extend_from_slice(&byte_length.to_le_bytes());
        canonical.extend_from_slice(&digest.to_le_bytes());
        files.push(VectorFixtureFileFact {
            relative_path,
            byte_length,
            digest,
        });
    }
    Ok(VectorFixtureDirectoryEvidence {
        digest: vector_fixture_digest(&canonical),
        files,
    })
}

fn isolated_pair(fixture: &FrozenStoreFixture) -> Result<IsolatedStorePair, String> {
    let pair = fixture.isolated_pair()?;
    let expected = frozen_fixture_evidence(fixture)?;
    let clean_initial = frozen_fixture_evidence(&FrozenStoreFixture::capture(pair.clean.path())?)?;
    let fault_initial =
        frozen_fixture_evidence(&FrozenStoreFixture::capture(pair.faulted.path())?)?;
    if clean_initial != expected || fault_initial != expected {
        return Err("vector clean/fault fixture directories are not byte-identical".to_owned());
    }
    Ok(IsolatedStorePair {
        clean: pair.clean,
        fault: pair.faulted,
        clean_initial,
        fault_initial,
    })
}

fn run_kernel_parity(
    seed: u64,
    fault: Option<VectorFaultKind>,
) -> Result<VectorOperationEvidence, String> {
    let dimensions = [
        0_usize, 1, 2, 3, 7, 15, 16, 31, 32, 33, 63, 64, 65, 127, 128, 129, 768,
    ];
    let mut pairs = Vec::new();
    let mut available = Vec::new();
    let variants = KernelVariant::available().collect::<Vec<_>>();
    let legacy_case_count = variants
        .len()
        .checked_mul(KERNELS.len())
        .and_then(|value| value.checked_mul(dimensions.len()))
        .ok_or_else(|| "vector kernel legacy case count overflowed".to_owned())?;
    let extra_float_cases_per_backend = 2_usize
        .checked_mul(FLOAT_KERNEL_CORPORA.len() - 1)
        .and_then(|value| value.checked_mul(dimensions.len()))
        .ok_or_else(|| "vector kernel float case count overflowed".to_owned())?;
    for (backend_index, variant) in variants.into_iter().enumerate() {
        let backend = backend_id(variant.backend_id());
        for (kernel_index, kernel) in KERNELS.into_iter().enumerate() {
            for (dimension_index, dimension) in dimensions.into_iter().enumerate() {
                let backend_offset = backend_index
                    .checked_mul(KERNELS.len())
                    .and_then(|value| value.checked_mul(dimensions.len()))
                    .ok_or_else(|| "vector kernel backend offset overflowed".to_owned())?;
                let kernel_offset = kernel_index
                    .checked_mul(dimensions.len())
                    .ok_or_else(|| "vector kernel case offset overflowed".to_owned())?;
                let legacy_offset = backend_offset
                    .checked_add(kernel_offset)
                    .and_then(|value| value.checked_add(dimension_index))
                    .ok_or_else(|| "vector kernel case offset overflowed".to_owned())?;
                let float_corpora = match kernel {
                    independent::KernelId::DotF32 | independent::KernelId::DotF16 => {
                        &FLOAT_KERNEL_CORPORA[..]
                    }
                    _ => &NON_FLOAT_KERNEL_CORPUS[..],
                };
                for (corpus_index, float_corpus) in float_corpora.iter().copied().enumerate() {
                    let offset = if corpus_index == 0 {
                        legacy_offset
                    } else {
                        let float_kernel_index = match kernel {
                            independent::KernelId::DotF32 => 0_usize,
                            independent::KernelId::DotF16 => 1_usize,
                            _ => unreachable!(),
                        };
                        legacy_case_count
                            .checked_add(
                                backend_index
                                    .checked_mul(extra_float_cases_per_backend)
                                    .ok_or_else(|| {
                                        "vector kernel float backend offset overflowed".to_owned()
                                    })?,
                            )
                            .and_then(|value| {
                                value.checked_add(
                                    float_kernel_index
                                        * (FLOAT_KERNEL_CORPORA.len() - 1)
                                        * dimensions.len(),
                                )
                            })
                            .and_then(|value| {
                                value.checked_add((corpus_index - 1) * dimensions.len())
                            })
                            .and_then(|value| value.checked_add(dimension_index))
                            .ok_or_else(|| {
                                "vector kernel extra float offset overflowed".to_owned()
                            })?
                    };
                    let base = seed
                        .checked_mul(100_000)
                        .ok_or_else(|| "vector kernel case base overflowed".to_owned())?;
                    let mut case_id = base
                        .checked_add(offset as u64)
                        .ok_or_else(|| "vector kernel case id overflowed".to_owned())?
                        & !(1_u64 << 63);
                    if dimension_index % 2 == 1 {
                        case_id |= 1_u64 << 63;
                    }
                    let input = kernel_input(
                        case_id,
                        backend,
                        kernel,
                        dimension,
                        seed,
                        false,
                        float_corpus,
                    );
                    let observed = independent::I24Observed {
                        case_id,
                        backend,
                        kernel,
                        value: observe_kernel(variant, &input)?,
                        selected_for_store: false,
                        work_items: input.work_items,
                    };
                    pairs.push(I24EvidencePair { input, observed });
                }
            }
        }
        available.push(variant.backend_id());
    }
    if available.is_empty() {
        return Err("kernel backend inventory is empty".to_owned());
    }

    let directory = tempdir().map_err(|error| format!("kernel Store tempdir: {error}"))?;
    let case_id = (seed.rotate_left(24) ^ 0x2453_544f_5245) & !(1_u64 << 63);
    let controller = KernelFaultController::observing_store(case_id);
    let store = Store::open(directory.path(), OpenOptions::default())
        .map_err(|error| format!("open kernel fixture Store: {error}"))?;
    ingest_rows(
        &store,
        &[(101, 1, ROWS[0]), (102, 1, ROWS[1]), (103, 1, ROWS[2])],
    )?;
    store
        .seal()
        .map_err(|error| format!("seal kernel Store: {error}"))?;
    let (codes, factors) = persisted_bit4_input(directory.path())?;
    store
        .close()
        .map_err(|error| format!("close immutable kernel fixture Store: {error}"))?;
    let frozen = FrozenStoreFixture::capture(directory.path())?;
    let pair = isolated_pair(&frozen)?;
    let clean_store = Store::open_with_test_dependencies(
        pair.clean.path(),
        OpenOptions::default(),
        kernel_dependencies(controller.clone()),
    )
    .map_err(|error| format!("open observed kernel Store: {error}"))?;
    let clean_outcome = search(&clean_store, SearchTier::Scan, ROWS.len());
    let clean = result_fact(clean_outcome)?;
    let observations = controller.take_observations();
    if observations.len() != 1 {
        return Err(format!(
            "default public Store Scan emitted {} kernel observations, expected one",
            observations.len()
        ));
    }
    if !controller.take_typed_receipts().is_empty() {
        return Err("unarmed Store kernel observation emitted a fault receipt".to_owned());
    }
    pairs.push(store_kernel_pair(
        case_id,
        &observations[0],
        codes.clone(),
        factors.clone(),
    )?);
    clean_store
        .close()
        .map_err(|error| format!("close kernel control Store: {error}"))?;

    let mut forced_child = None;
    let mut mutation = VectorMutationEvidence::None;
    let (fault_result, retry_result) = if fault == Some(VectorFaultKind::ForcedDispatchBackend) {
        let requested = choose_forced_backend()?;
        let child_case_id = case_id ^ 1;
        let child = spawn_forced_backend_child(
            pair.fault.path(),
            requested,
            child_case_id,
            codes,
            factors,
        )?;
        mutation = VectorMutationEvidence::ForcedDispatch {
            case_id: child_case_id,
            requested: backend_id(requested),
        };
        pairs.push(child.pair.clone());
        let fault_result = child.fault_result.clone();
        let retry_result = child.retry_result.clone();
        forced_child = Some(child);
        (fault_result, retry_result)
    } else {
        let retry_store = Store::open(pair.fault.path(), OpenOptions::default())
            .map_err(|error| format!("open kernel retry Store: {error}"))?;
        let retry = result_fact(search(&retry_store, SearchTier::Scan, ROWS.len()))?;
        retry_store
            .close()
            .map_err(|error| format!("close kernel retry Store: {error}"))?;
        (clean.clone(), retry)
    };
    let invariant = VectorInvariantEvidence::I24(pairs);
    let fixture_evidence = primitive_fixture(
        VectorOperationKind::KernelParity,
        seed,
        &invariant,
        &[2],
        &fact_sources(&clean),
        vec![
            independent::PublicStoreStep::IngestAccepted,
            independent::PublicStoreStep::Seal,
        ],
    );
    Ok(VectorOperationEvidence {
        operation: VectorOperationKind::KernelParity,
        fault,
        fixture: fixture_evidence,
        mutation,
        invariant,
        receipts: Vec::new(),
        forced_child,
        generic_fault: None,
        control: VectorControlEvidence {
            namespace: "vector-execution/kernel-parity",
            operation: VectorOperationKind::KernelParity,
            seed,
            clean,
            fault: fault_result,
            retry: retry_result,
            clean_initial_directory: pair.clean_initial,
            fault_initial_directory: pair.fault_initial,
            isolated_directories: true,
        },
    })
}

fn backend_key(backend: KernelBackendId) -> &'static str {
    backend.as_str()
}

fn parse_backend(value: &str) -> Result<KernelBackendId, String> {
    for variant in KernelVariant::available() {
        if variant.backend_id().as_str() == value {
            return Ok(variant.backend_id());
        }
    }
    Err(format!("child reported unavailable backend {value}"))
}

fn primitive_segment_text(bytes: [u8; 16]) -> String {
    let mut encoded = String::with_capacity(32);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

const FORCED_CHILD_MAGIC: [u8; 8] = *b"ZEVECF01";

fn backend_tag(backend: KernelBackendId) -> u8 {
    match backend {
        KernelBackendId::Scalar => 0,
        KernelBackendId::NeonWiden => 1,
        KernelBackendId::NeonDotprodU4 => 2,
        KernelBackendId::NeonI8mm => 3,
        KernelBackendId::NeonDotprodU2 => 4,
        KernelBackendId::NeonDotprodU6 => 5,
        KernelBackendId::NeonDotprodU8 => 6,
        KernelBackendId::NeonDotprodU4Prefetch => 7,
        KernelBackendId::Avx2 => 8,
    }
}

fn backend_from_tag(tag: u8) -> Result<KernelBackendId, String> {
    let backend = match tag {
        0 => KernelBackendId::Scalar,
        1 => KernelBackendId::NeonWiden,
        2 => KernelBackendId::NeonDotprodU4,
        3 => KernelBackendId::NeonI8mm,
        4 => KernelBackendId::NeonDotprodU2,
        5 => KernelBackendId::NeonDotprodU6,
        6 => KernelBackendId::NeonDotprodU8,
        7 => KernelBackendId::NeonDotprodU4Prefetch,
        8 => KernelBackendId::Avx2,
        _ => return Err(format!("forced child reported unknown backend tag {tag}")),
    };
    if KernelVariant::available().any(|variant| variant.backend_id() == backend) {
        Ok(backend)
    } else {
        Err(format!(
            "forced child reported unavailable backend {}",
            backend.as_str()
        ))
    }
}

fn push_bool(output: &mut Vec<u8>, value: bool) {
    output.push(u8::from(value));
}

fn push_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn encode_result_fact(output: &mut Vec<u8>, fact: &VectorStoreResultFact) -> Result<(), String> {
    if fact.status != independent::PrimitiveStatus::Ok {
        return Err("forced-backend child can encode only successful Store facts".to_owned());
    }
    push_u64(output, fact.generation);
    push_u64(output, fact.dims_touched);
    push_u64(output, fact.bytes_read);
    push_bool(output, fact.exact_rescore);
    push_bool(output, fact.approximate);
    push_u64(output, fact.returned);
    push_u32(
        output,
        u32::try_from(fact.candidates.len())
            .map_err(|_| "forced child candidate count exceeds u32".to_owned())?,
    );
    for candidate in &fact.candidates {
        match candidate.row.source {
            independent::PrimitiveSource::Active => output.push(0),
            independent::PrimitiveSource::Sealed(segment) => {
                output.push(1);
                output.extend_from_slice(&segment);
            }
        }
        push_u32(output, candidate.row.local_row);
        match candidate.document {
            None => output.push(0),
            Some(document) => {
                output.push(1);
                output.extend_from_slice(&document.doc_id_be);
                push_u64(output, document.revision);
            }
        }
        push_u32(output, candidate.score.0);
    }
    Ok(())
}

struct ChildCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> ChildCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take<const N: usize>(&mut self, field: &str) -> Result<[u8; N], String> {
        let end = self
            .offset
            .checked_add(N)
            .ok_or_else(|| format!("forced child {field} offset overflow"))?;
        let source = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| format!("forced child {field} is truncated"))?;
        let mut value = [0_u8; N];
        value.copy_from_slice(source);
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self, field: &str) -> Result<u8, String> {
        self.take::<1>(field).map(|value| value[0])
    }

    fn bool(&mut self, field: &str) -> Result<bool, String> {
        match self.u8(field)? {
            0 => Ok(false),
            1 => Ok(true),
            value => Err(format!("forced child {field} has invalid bool {value}")),
        }
    }

    fn u32(&mut self, field: &str) -> Result<u32, String> {
        self.take::<4>(field).map(u32::from_le_bytes)
    }

    fn u64(&mut self, field: &str) -> Result<u64, String> {
        self.take::<8>(field).map(u64::from_le_bytes)
    }

    fn bytes(&mut self, length: usize, field: &str) -> Result<&'a [u8], String> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| format!("forced child {field} offset overflow"))?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| format!("forced child {field} is truncated"))?;
        self.offset = end;
        Ok(value)
    }

    fn finished(&self) -> bool {
        self.offset == self.bytes.len()
    }
}

fn decode_result_fact(cursor: &mut ChildCursor<'_>) -> Result<VectorStoreResultFact, String> {
    let generation = cursor.u64("result generation")?;
    let dims_touched = cursor.u64("result dims")?;
    let bytes_read = cursor.u64("result bytes")?;
    let exact_rescore = cursor.bool("result exact-rescore")?;
    let approximate = cursor.bool("result approximate")?;
    let returned = cursor.u64("result returned")?;
    let candidate_count = usize::try_from(cursor.u32("result candidate count")?)
        .map_err(|_| "forced child candidate count exceeds usize".to_owned())?;
    let mut candidates = Vec::with_capacity(candidate_count);
    for _ in 0..candidate_count {
        let source = match cursor.u8("candidate source")? {
            0 => independent::PrimitiveSource::Active,
            1 => independent::PrimitiveSource::Sealed(cursor.take::<16>("candidate segment")?),
            value => {
                return Err(format!(
                    "forced child candidate source tag {value} is invalid"
                ));
            }
        };
        let local_row = cursor.u32("candidate local row")?;
        let document = match cursor.u8("candidate document presence")? {
            0 => None,
            1 => Some(independent::PrimitiveDocument {
                doc_id_be: cursor.take::<16>("candidate document id")?,
                revision: cursor.u64("candidate document revision")?,
            }),
            value => {
                return Err(format!(
                    "forced child candidate document presence {value} is invalid"
                ));
            }
        };
        let score = independent::F32(cursor.u32("candidate score")?);
        candidates.push(independent::IdentityObservedRow {
            row: independent::PrimitiveRow { source, local_row },
            document,
            score,
        });
    }
    Ok(VectorStoreResultFact {
        status: independent::PrimitiveStatus::Ok,
        generation,
        candidates,
        dims_touched,
        bytes_read,
        exact_rescore,
        approximate,
        returned,
    })
}

struct ForcedChildWireEvidence {
    selected: KernelBackendId,
    work_items: u64,
    kernel_values: Vec<u32>,
    fault_result: VectorStoreResultFact,
    retry_result: VectorStoreResultFact,
    receipt: VectorFaultReceipt,
}

fn decode_forced_child(bytes: &[u8]) -> Result<ForcedChildWireEvidence, String> {
    let mut cursor = ChildCursor::new(bytes);
    if cursor.take::<8>("magic")? != FORCED_CHILD_MAGIC {
        return Err("forced-backend child evidence magic/version mismatch".to_owned());
    }
    let selected = backend_from_tag(cursor.u8("selected backend")?)?;
    let work_items = cursor.u64("work items")?;
    if cursor.u8("kernel value tag")? != 1 {
        return Err("forced child kernel value is not typed F32s".to_owned());
    }
    let value_count = usize::try_from(cursor.u32("kernel value count")?)
        .map_err(|_| "forced child kernel value count exceeds usize".to_owned())?;
    let mut kernel_values = Vec::with_capacity(value_count);
    for _ in 0..value_count {
        kernel_values.push(cursor.u32("kernel value")?);
    }
    let fault_result = decode_result_fact(&mut cursor)?;
    let retry_result = decode_result_fact(&mut cursor)?;
    let receipt_length = usize::try_from(cursor.u32("receipt byte length")?)
        .map_err(|_| "forced child receipt length exceeds usize".to_owned())?;
    let receipt = VectorFaultReceipt::decode_forced_backend_test_evidence(
        cursor.bytes(receipt_length, "typed production receipt")?,
    )
    .map_err(|error| format!("decode typed production receipt: {error:?}"))?;
    if !cursor.finished() {
        return Err("forced child evidence has trailing bytes".to_owned());
    }
    Ok(ForcedChildWireEvidence {
        selected,
        work_items,
        kernel_values,
        fault_result,
        retry_result,
        receipt,
    })
}

fn spawn_forced_backend_child(
    directory: &Path,
    requested: KernelBackendId,
    case_id: u64,
    codes: Vec<u8>,
    factors: Vec<[f32; 3]>,
) -> Result<ForcedBackendChildEvidence, String> {
    use std::process::Command;
    let output_directory = tempdir().map_err(|error| format!("child output tempdir: {error}"))?;
    let output_path = output_directory.path().join("forced-backend.bin");
    let status = Command::new(std::env::current_exe().map_err(|error| error.to_string())?)
        .arg("--exact")
        .arg(FORCED_BACKEND_CHILD_TEST_NAME)
        .arg("--ignored")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env(VECTOR_CHILD_MODE, "forced-backend")
        .env(VECTOR_CHILD_DIRECTORY, directory)
        .env(VECTOR_CHILD_BACKEND, backend_key(requested))
        .env(VECTOR_CHILD_CASE, case_id.to_string())
        .env(VECTOR_CHILD_OUTPUT, &output_path)
        .status()
        .map_err(|error| format!("spawn forced-backend child: {error}"))?;
    if !status.success() {
        return Err(format!("forced-backend child failed: {status:?}"));
    }
    let bytes = std::fs::read(&output_path)
        .map_err(|error| format!("read forced-backend child evidence: {error}"))?;
    let child = decode_forced_child(&bytes)?;
    let selected = child.selected;
    if selected != requested {
        return Err(format!(
            "forced-backend child selected {}, requested {}",
            selected.as_str(),
            requested.as_str()
        ));
    }
    let work_items = child.work_items;
    let observation = independent::I24Observed {
        case_id,
        backend: backend_id(selected),
        kernel: independent::KernelId::ScoreBit4PreparedBatch,
        value: independent::KernelValue::F32s(
            child
                .kernel_values
                .iter()
                .copied()
                .map(independent::F32)
                .collect(),
        ),
        selected_for_store: true,
        work_items,
    };
    let synthetic_observation = KernelScoreObservationTransport {
        backend: selected,
        work_items,
    };
    let input = store_kernel_input_transport(case_id, &synthetic_observation, codes, factors)?;
    Ok(ForcedBackendChildEvidence {
        transport: ForcedBackendTransportFormat::TypedBinaryV1,
        requested: backend_id(requested),
        pair: I24EvidencePair {
            input,
            observed: observation,
        },
        fault_result: child.fault_result,
        retry_result: child.retry_result,
        receipt: child.receipt,
    })
}

struct KernelScoreObservationTransport {
    backend: KernelBackendId,
    work_items: u64,
}

fn store_kernel_input_transport(
    case_id: u64,
    observation: &KernelScoreObservationTransport,
    codes: Vec<u8>,
    factors: Vec<[f32; 3]>,
) -> Result<independent::KernelInput, String> {
    let query = prepare_bit4_query(&QUERY, 0)
        .map_err(|error| format!("prepare transported Store query: {error}"))?;
    let (query_codes, query_sum, query_scale_half) = query.observation_parts();
    let batch_rows = factors.len();
    let expected_work = QUERY.len().saturating_mul(batch_rows) as u64;
    if observation.work_items != expected_work {
        return Err(format!(
            "child Store work {} differs from actual input work {expected_work}",
            observation.work_items
        ));
    }
    Ok(independent::KernelInput {
        case_id,
        backend: backend_id(observation.backend),
        selected_for_store: true,
        work_items: observation.work_items,
        kernel: independent::KernelId::ScoreBit4PreparedBatch,
        dimension: QUERY.len() as u64,
        signed_a: query_codes.to_vec(),
        signed_b: Vec::new(),
        bytes_a: Vec::new(),
        bytes_b: codes,
        f32_a: Vec::new(),
        f32_b: Vec::new(),
        f16_a: Vec::new(),
        f16_b: Vec::new(),
        row_bytes: QUERY.len().div_ceil(2) as u64,
        batch_rows: batch_rows as u64,
        pointer_order: Vec::new(),
        query_sum,
        query_scale_half: independent::F64::from_float(query_scale_half),
        bit4_factors: factors
            .into_iter()
            .map(|fields| fields.map(independent::F32::from_float))
            .collect(),
    })
}

fn encode_forced_child(
    observation: &KernelScoreObservation,
    fault_result: &VectorStoreResultFact,
    retry_result: &VectorStoreResultFact,
    receipt: &VectorFaultReceipt,
) -> Result<Vec<u8>, String> {
    let KernelScoreValue::F32s(kernel_values) = observation.value() else {
        return Err(format!(
            "forced public Bit4 Store selected unexpected result {:?}",
            observation.value()
        ));
    };
    let receipt_bytes = receipt
        .encode_forced_backend_test_evidence()
        .map_err(|error| format!("encode typed production receipt: {error:?}"))?;
    let mut output = Vec::new();
    output.extend_from_slice(&FORCED_CHILD_MAGIC);
    output.push(backend_tag(observation.backend()));
    push_u64(&mut output, observation.work_items());
    output.push(1);
    push_u32(
        &mut output,
        u32::try_from(kernel_values.len())
            .map_err(|_| "forced child kernel value count exceeds u32".to_owned())?,
    );
    for value in kernel_values {
        push_u32(&mut output, *value);
    }
    encode_result_fact(&mut output, fault_result)?;
    encode_result_fact(&mut output, retry_result)?;
    push_u32(
        &mut output,
        u32::try_from(receipt_bytes.len())
            .map_err(|_| "typed production receipt length exceeds u32".to_owned())?,
    );
    output.extend_from_slice(&receipt_bytes);
    Ok(output)
}

/// Fresh-child entry point. The controller is created and installed by
/// `Store::open_with_test_dependencies` in this child before any kernel table
/// is initialized; the first scored operation is a public sealed Store Scan.
pub fn forced_backend_child_from_env() -> Result<(), String> {
    if std::env::var(VECTOR_CHILD_MODE).map_err(|error| error.to_string())? != "forced-backend" {
        return Err("unknown vector adapter child mode".to_owned());
    }
    let directory = std::env::var_os(VECTOR_CHILD_DIRECTORY)
        .map(PathBuf::from)
        .ok_or_else(|| "missing vector child directory".to_owned())?;
    let requested =
        parse_backend(&std::env::var(VECTOR_CHILD_BACKEND).map_err(|error| error.to_string())?)?;
    let case_id = std::env::var(VECTOR_CHILD_CASE)
        .map_err(|error| error.to_string())?
        .parse::<u64>()
        .map_err(|error| error.to_string())?;
    let output = std::env::var_os(VECTOR_CHILD_OUTPUT)
        .map(PathBuf::from)
        .ok_or_else(|| "missing vector child output path".to_owned())?;
    let controller = KernelFaultController::forced_backend(requested, case_id);
    let store = Store::open_with_test_dependencies(
        &directory,
        OpenOptions::default(),
        kernel_dependencies(controller.clone()),
    )
    .map_err(|error| format!("open forced child Store: {error}"))?;
    let fault_result = result_fact(search(&store, SearchTier::Scan, ROWS.len()))?;
    let observations = controller.take_observations();
    if observations.len() != 1 {
        return Err(format!(
            "forced child observed {} kernel results, expected one",
            observations.len()
        ));
    }
    let observation = &observations[0];
    if observation.backend() != requested
        || observation.kernel() != KernelOperationId::ScoreBit4PreparedBatch
        || !observation.result_published()
    {
        return Err(format!(
            "forced child observation mismatch: {observation:?}"
        ));
    }
    let receipts = controller.take_typed_receipts();
    if receipts.len() != 1 {
        return Err(format!(
            "forced child drained {} receipts, expected one",
            receipts.len()
        ));
    }
    let receipt = &receipts[0];
    if receipt.campaign() != VectorCampaign::VectorExecution
        || receipt.operation() != ProductVectorOperation::KernelParity
        || receipt.fault() != ProductVectorFaultKind::ForcedDispatchBackend
        || receipt.site() != VectorFaultSite::KernelDispatchSelectedScoringTable
        || receipt.cardinality() != 1
        || receipt.seed_case_id() != case_id
        || !receipt.result_published()
    {
        return Err(format!("forced child typed receipt mismatch: {receipt:?}"));
    }
    let VectorFaultEffect::ForcedBackend {
        requested: receipt_requested,
        selected: receipt_selected,
        kernel: receipt_kernel,
        work_items: receipt_work_items,
    } = receipt.effect()
    else {
        return Err(format!("forced child effect mismatch: {receipt:?}"));
    };
    if *receipt_requested != requested
        || *receipt_selected != observation.backend()
        || *receipt_kernel != observation.kernel()
        || *receipt_work_items != observation.work_items()
    {
        return Err(format!(
            "forced child effect/observation mismatch: receipt={receipt:?} observation={observation:?}"
        ));
    }
    let retry_result = result_fact(search(&store, SearchTier::Scan, ROWS.len()))?;
    if !controller.take_typed_receipts().is_empty() || !controller.take_observations().is_empty() {
        return Err("forced child one-shot controller fired during retry".to_owned());
    }
    store
        .close()
        .map_err(|error| format!("close forced child Store: {error}"))?;
    let bytes = encode_forced_child(observation, &fault_result, &retry_result, receipt)?;
    std::fs::write(output, bytes).map_err(|error| format!("write forced child evidence: {error}"))
}

fn quant_input(
    case_id: u64,
    scheme: independent::QuantScheme,
    row: &[f32],
    seed: u64,
    output_len: usize,
    code_len: usize,
    store: independent::QuantStoreInput,
) -> independent::QuantInput {
    quant_input_with_query(
        case_id, scheme, row, &QUERY, seed, output_len, code_len, store,
    )
}

#[allow(clippy::too_many_arguments)]
fn quant_input_with_query(
    case_id: u64,
    scheme: independent::QuantScheme,
    row: &[f32],
    query: &[f32],
    seed: u64,
    output_len: usize,
    code_len: usize,
    store: independent::QuantStoreInput,
) -> independent::QuantInput {
    independent::QuantInput {
        case_id,
        scheme,
        row: row
            .iter()
            .copied()
            .map(independent::F32::from_float)
            .collect(),
        query: query
            .iter()
            .copied()
            .map(independent::F32::from_float)
            .collect(),
        query_seed: seed,
        output_len: output_len as u64,
        code_len: code_len as u64,
        sentinel: 0xa5,
        store,
    }
}

fn primitive_only_quant_store() -> independent::QuantStoreInput {
    independent::QuantStoreInput {
        generation_before: 0,
        schedule: Vec::new(),
        document: None,
        document_visible: false,
    }
}

fn primitive_only_quant_facts() -> independent::QuantStoreFacts {
    independent::QuantStoreFacts {
        ingest_status: independent::PrimitiveStatus::Ok,
        scan_status: independent::PrimitiveStatus::Ok,
        generation_before: 0,
        generation_after: 0,
        document_visible: false,
        persisted_code_bytes: Vec::new(),
        persisted_factor_bits: Vec::new(),
    }
}

fn quant_boundary_inputs(seed: u64) -> Vec<independent::QuantInput> {
    let mut inputs = Vec::new();
    let valid = [1.0_f32, -0.5, f32::from_bits(1)];
    let query = [0.25_f32, -1.0, 2.0];
    let oversized = vec![1.0_f32; 65_537];
    let mut serial = 0_u64;
    for scheme in [
        independent::QuantScheme::Bit4,
        independent::QuantScheme::Int8,
    ] {
        let expected_len = match scheme {
            independent::QuantScheme::Bit4 => valid.len().div_ceil(2),
            independent::QuantScheme::Int8 => valid.len(),
        };
        let scheme_tag = match scheme {
            independent::QuantScheme::Bit4 => 0x2500_0000,
            independent::QuantScheme::Int8 => 0x2580_0000,
        };
        let mut push = |row: &[f32], case_query: &[f32], output_len, code_len| {
            serial += 1;
            inputs.push(quant_input_with_query(
                seed.rotate_left(8) ^ scheme_tag ^ serial,
                scheme,
                row,
                case_query,
                seed ^ serial,
                output_len,
                code_len,
                primitive_only_quant_store(),
            ));
        };
        push(&[], &[], 0, 0);
        let oversized_len = match scheme {
            independent::QuantScheme::Bit4 => oversized.len().div_ceil(2),
            independent::QuantScheme::Int8 => oversized.len(),
        };
        push(&oversized, &oversized, oversized_len, oversized_len);
        push(&valid, &query, expected_len - 1, expected_len);
        push(&valid, &query, expected_len + 1, expected_len);
        push(&valid, &query, expected_len, expected_len - 1);
        push(&valid, &query, expected_len, expected_len + 1);
        for special in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            for position in 0..valid.len() {
                let mut bad_row = valid;
                bad_row[position] = special;
                push(&bad_row, &query, expected_len, expected_len);
                let mut bad_query = query;
                bad_query[position] = special;
                push(&valid, &bad_query, expected_len, expected_len);
            }
        }
    }
    inputs
}

fn quant_positive_inputs(seed: u64) -> Vec<independent::QuantInput> {
    let cases = [
        (
            vec![1.0_f32, -1.0, 0.5, -0.5],
            vec![0.25_f32, -0.75, 1.0, -0.125],
        ),
        (vec![0.25_f32; 4], vec![1.0_f32, -1.0, 0.5, -0.5]),
        (
            vec![f32::from_bits(1), f32::from_bits(0x8000_0001), 1.0],
            vec![1.0_f32, -1.0, 0.0],
        ),
        (
            vec![f32::MAX, -f32::MAX, 1.0, -1.0],
            vec![0.25_f32, 0.25, -0.25, -0.25],
        ),
        (vec![-127.0_f32, 0.5, 127.0], vec![-1.0_f32, 0.0, 1.0]),
        (vec![0.0_f32, -0.0], vec![-0.0_f32, 0.0]),
        (
            vec![1.0_f32, 1.0, 0.5, -0.5],
            vec![0.137_f32, -0.413, 1.0, -0.711],
        ),
    ];
    let stochastic_row = vec![0.2_f32, -0.7, 0.3, -0.9, 0.6, -0.4, 1.0];
    let stochastic_query = vec![0.137_f32, -0.413, 1.0, -0.711, 0.219, -0.937, 0.503];
    let mut inputs = Vec::new();
    let mut serial = 0_u64;
    for scheme in [
        independent::QuantScheme::Bit4,
        independent::QuantScheme::Int8,
    ] {
        for (row, query) in &cases {
            serial += 1;
            let output_len = match scheme {
                independent::QuantScheme::Bit4 => row.len().div_ceil(2),
                independent::QuantScheme::Int8 => row.len(),
            };
            inputs.push(quant_input_with_query(
                seed.rotate_left(8) ^ 0x25f0_0000 ^ serial,
                scheme,
                row,
                query,
                seed ^ serial.rotate_left(17),
                output_len,
                output_len,
                primitive_only_quant_store(),
            ));
        }
    }
    for stochastic_seed in [
        seed ^ 0x0123_4567_89ab_cdef,
        seed ^ 0xfedc_ba98_7654_3210,
        seed.rotate_left(29),
        !seed,
    ] {
        serial += 1;
        inputs.push(quant_input_with_query(
            seed.rotate_left(8) ^ 0x25f1_0000 ^ serial,
            independent::QuantScheme::Bit4,
            &stochastic_row,
            &stochastic_query,
            stochastic_seed,
            stochastic_row.len().div_ceil(2),
            stochastic_row.len().div_ceil(2),
            primitive_only_quant_store(),
        ));
    }
    inputs
}

fn code_view<T: Copy + Default>(values: &[T], requested: usize) -> Vec<T> {
    let mut view = values.to_vec();
    view.resize(requested, T::default());
    view
}

fn observe_quant_result(input: &independent::QuantInput) -> independent::QuantResult {
    let row = input
        .row
        .iter()
        .map(|value| value.to_float())
        .collect::<Vec<_>>();
    let query = input
        .query
        .iter()
        .map(|value| value.to_float())
        .collect::<Vec<_>>();
    let output_len = usize::try_from(input.output_len).unwrap_or(usize::MAX);
    let code_len = usize::try_from(input.code_len).unwrap_or(usize::MAX);
    match input.scheme {
        independent::QuantScheme::Bit4 => {
            let mut output = vec![input.sentinel; output_len];
            let factors = match quantize_bit4(&row, &mut output) {
                Ok(factors) => factors,
                Err(error) => {
                    return independent::QuantResult {
                        status: quant_status(error),
                        output_after: output,
                        success: None,
                    };
                }
            };
            let prepared = match prepare_bit4_query(&query, input.query_seed) {
                Ok(prepared) => prepared,
                Err(error) => {
                    return independent::QuantResult {
                        status: quant_status(error),
                        output_after: output,
                        success: None,
                    };
                }
            };
            let supplied_codes = code_view(&output, code_len);
            let estimate = match est_dot_bit4(&prepared, &supplied_codes, factors) {
                Ok(estimate) => estimate,
                Err(error) => {
                    return independent::QuantResult {
                        status: quant_status(error),
                        output_after: output,
                        success: None,
                    };
                }
            };
            let mut reconstruction = vec![0.0_f32; row.len()];
            if let Err(error) = dequantize_bit4(&output, factors, &mut reconstruction) {
                return independent::QuantResult {
                    status: quant_status(error),
                    output_after: output,
                    success: None,
                };
            }
            let (query_codes, query_sum, query_scale) = prepared.observation_parts();
            independent::QuantResult {
                status: independent::PrimitiveStatus::Ok,
                output_after: output.clone(),
                success: Some(independent::QuantSuccess {
                    code_bytes: output,
                    factor_bits: factors
                        .persisted_fields()
                        .map(independent::F32::from_float)
                        .to_vec(),
                    query_code_bytes: query_codes.iter().map(|value| *value as u8).collect(),
                    query_code_sum: query_sum,
                    query_scale: independent::F64::from_float(query_scale),
                    reconstruction: reconstruction
                        .into_iter()
                        .map(independent::F32::from_float)
                        .collect(),
                    estimate: independent::F32::from_float(estimate),
                }),
            }
        }
        independent::QuantScheme::Int8 => {
            let mut signed_output = vec![input.sentinel as i8; output_len];
            let (scale, offset) = match quantize_int8(&row, &mut signed_output) {
                Ok(factors) => factors,
                Err(error) => {
                    return independent::QuantResult {
                        status: quant_status(error),
                        output_after: signed_output.into_iter().map(|value| value as u8).collect(),
                        success: None,
                    };
                }
            };
            let output = signed_output
                .iter()
                .copied()
                .map(|value| value as u8)
                .collect::<Vec<_>>();
            let prepared = match prepare_int8_query(&query) {
                Ok(prepared) => prepared,
                Err(error) => {
                    return independent::QuantResult {
                        status: quant_status(error),
                        output_after: output,
                        success: None,
                    };
                }
            };
            let supplied_codes = code_view(&signed_output, code_len);
            let estimate = match dot_int8_query(
                &prepared,
                Int8Vec {
                    codes: &supplied_codes,
                    scale,
                    offset,
                },
            ) {
                Ok(estimate) => estimate,
                Err(error) => {
                    return independent::QuantResult {
                        status: quant_status(error),
                        output_after: output,
                        success: None,
                    };
                }
            };
            let mut reconstruction = vec![0.0_f32; row.len()];
            if let Err(error) = dequantize_int8(
                Int8Vec {
                    codes: &signed_output,
                    scale,
                    offset,
                },
                &mut reconstruction,
            ) {
                return independent::QuantResult {
                    status: quant_status(error),
                    output_after: output,
                    success: None,
                };
            }
            let (query_codes, query_scale, query_sum) = prepared.observation_parts();
            independent::QuantResult {
                status: independent::PrimitiveStatus::Ok,
                output_after: output.clone(),
                success: Some(independent::QuantSuccess {
                    code_bytes: output,
                    factor_bits: vec![
                        independent::F32::from_float(scale),
                        independent::F32::from_float(offset),
                    ],
                    query_code_bytes: query_codes.iter().map(|value| *value as u8).collect(),
                    query_code_sum: query_sum,
                    query_scale: independent::F64::from_float(query_scale),
                    reconstruction: reconstruction
                        .into_iter()
                        .map(independent::F32::from_float)
                        .collect(),
                    estimate: independent::F32::from_float(estimate),
                }),
            }
        }
    }
}

fn ingest_status(error: IngestError) -> independent::PrimitiveStatus {
    match error {
        IngestError::Vector(error) => quant_status(error),
        other => independent::PrimitiveStatus::SegmentGeometry {
            detail: other.to_string(),
        },
    }
}

fn bit4_store_facts(row: &[f32], document: u128) -> Result<independent::QuantStoreFacts, String> {
    let directory = tempdir().map_err(|error| format!("I25 Bit4 tempdir: {error}"))?;
    let store = Store::open(directory.path(), OpenOptions::default())
        .map_err(|error| format!("open I25 Bit4 Store: {error}"))?;
    let generation_before = store
        .snapshot()
        .map_err(|error| format!("snapshot I25 Bit4 before: {error}"))?
        .generation();
    let version = DocumentVersion::new(DocId::new(document), Revision::new(1));
    let ingest = store.ingest(IngestBatch::new(vec![IngestDocument::new(
        version,
        row.to_vec(),
    )]));
    let generation_after = store
        .snapshot()
        .map_err(|error| format!("snapshot I25 Bit4 after ingest: {error}"))?
        .generation();
    if let Err(error) = ingest {
        let empty = result_fact(search(&store, SearchTier::Exact, 1))?;
        store
            .close()
            .map_err(|error| format!("close rejected I25 Bit4 Store: {error}"))?;
        return Ok(independent::QuantStoreFacts {
            ingest_status: ingest_status(error),
            scan_status: empty.status,
            generation_before,
            generation_after,
            document_visible: false,
            persisted_code_bytes: Vec::new(),
            persisted_factor_bits: Vec::new(),
        });
    }
    store
        .seal()
        .map_err(|error| format!("seal I25 Bit4 Store: {error}"))?;
    let persisted = raw_persisted_quantization(directory.path())?;
    if persisted.scheme != independent::QuantScheme::Bit4
        || persisted.dims as usize != row.len()
        || persisted.row_count != 1
    {
        return Err(format!(
            "I25 Bit4 persisted geometry mismatch: scheme={:?} dims={} rows={}",
            persisted.scheme, persisted.dims, persisted.row_count
        ));
    }
    let scan = result_fact(search(&store, SearchTier::Scan, 1))?;
    let document_visible = scan.candidates.iter().any(|candidate| {
        candidate.document
            == Some(independent::PrimitiveDocument {
                doc_id_be: document.to_be_bytes(),
                revision: 1,
            })
    });
    store
        .close()
        .map_err(|error| format!("close I25 Bit4 Store: {error}"))?;
    Ok(independent::QuantStoreFacts {
        ingest_status: independent::PrimitiveStatus::Ok,
        scan_status: scan.status,
        generation_before,
        generation_after,
        document_visible,
        persisted_code_bytes: persisted.code_bytes,
        persisted_factor_bits: persisted.factor_bits,
    })
}

fn int8_store_facts(row: &[f32], document: u128) -> Result<independent::QuantStoreFacts, String> {
    let directory = tempdir().map_err(|error| format!("I25 Int8 tempdir: {error}"))?;
    let dependencies = StoreTestDependencies::new(Arc::new(StdVfs), Arc::new(SystemMonotonicClock))
        .with_vector_seal_scheme(ProductQuantScheme::Int8);
    let store =
        Store::open_with_test_dependencies(directory.path(), OpenOptions::default(), dependencies)
            .map_err(|error| format!("open I25 Int8 Store: {error}"))?;
    let generation_before = store
        .snapshot()
        .map_err(|error| format!("snapshot I25 Int8 before: {error}"))?
        .generation();
    let version = DocumentVersion::new(DocId::new(document), Revision::new(1));
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            version,
            row.to_vec(),
        )]))
        .map_err(|error| format!("ingest I25 Int8 document: {error}"))?;
    store
        .seal()
        .map_err(|error| format!("seal I25 Int8 Store: {error}"))?;
    let generation_after = store
        .snapshot()
        .map_err(|error| format!("snapshot I25 Int8 after seal: {error}"))?
        .generation();
    let persisted = raw_persisted_quantization(directory.path())?;
    if persisted.scheme != independent::QuantScheme::Int8
        || persisted.dims as usize != row.len()
        || persisted.row_count != 1
    {
        return Err(format!(
            "I25 Int8 persisted geometry mismatch: scheme={:?} dims={} rows={}",
            persisted.scheme, persisted.dims, persisted.row_count
        ));
    }
    store
        .close()
        .map_err(|error| format!("close I25 Int8 Store: {error}"))?;
    let reopened = Store::open(directory.path(), OpenOptions::default())
        .map_err(|error| format!("reopen I25 Int8 Store: {error}"))?;
    let scan = result_fact(search(&reopened, SearchTier::Scan, 1))?;
    let document_visible = scan.candidates.iter().any(|candidate| {
        candidate.document
            == Some(independent::PrimitiveDocument {
                doc_id_be: document.to_be_bytes(),
                revision: 1,
            })
    });
    reopened
        .close()
        .map_err(|error| format!("close reopened I25 Int8 Store: {error}"))?;
    Ok(independent::QuantStoreFacts {
        ingest_status: independent::PrimitiveStatus::Ok,
        scan_status: scan.status,
        generation_before,
        generation_after,
        document_visible,
        persisted_code_bytes: persisted.code_bytes,
        persisted_factor_bits: persisted.factor_bits,
    })
}

struct Bit4Fixture {
    directory: TempDir,
    segment: SegmentId,
    clean: VectorStoreResultFact,
    frozen: FrozenStoreFixture,
}

fn sealed_bit4_fixture(seed: u64) -> Result<Bit4Fixture, String> {
    let directory = tempdir().map_err(|error| format!("Bit4 fault tempdir: {error}"))?;
    let store = Store::open(directory.path(), OpenOptions::default())
        .map_err(|error| format!("open Bit4 fault Store: {error}"))?;
    let document_base = u128::from(seed).wrapping_shl(32);
    ingest_rows(
        &store,
        &[
            (document_base | 31, 1, ROWS[0]),
            (document_base | 32, 1, ROWS[1]),
            (document_base | 33, 1, ROWS[2]),
        ],
    )?;
    store
        .seal()
        .map_err(|error| format!("seal Bit4 fault Store: {error}"))?;
    let snapshot = store
        .snapshot()
        .map_err(|error| format!("snapshot Bit4 fault Store: {error}"))?;
    let segment = snapshot
        .segments()
        .first()
        .ok_or_else(|| "Bit4 fault Store published no segment".to_owned())?
        .meta()
        .id;
    drop(snapshot);
    let clean = result_fact(search(&store, SearchTier::Scan, ROWS.len()))?;
    store
        .close()
        .map_err(|error| format!("close Bit4 fault Store: {error}"))?;
    let frozen = FrozenStoreFixture::capture(directory.path())?;
    Ok(Bit4Fixture {
        directory,
        segment,
        clean,
        frozen,
    })
}

struct Int8Fixture {
    directory: TempDir,
    segment: SegmentId,
    clean: VectorStoreResultFact,
    frozen: FrozenStoreFixture,
}

struct GraphFixture {
    directory: TempDir,
    segment: SegmentId,
    rows: u32,
    epoch: StoreEpoch,
    query: Vec<f32>,
    clean: VectorStoreResultFact,
    frozen: FrozenStoreFixture,
}

fn graph_tier(seed: u64) -> SearchTier {
    SearchTier::Graph(GraphSearchOptions::default().with_seed(seed))
}

fn graph_search(
    store: &Store,
    query: &[f32],
    seed: u64,
    k: usize,
) -> Result<SearchOutcome, QueryError> {
    store.search(
        SearchRequest::new(query),
        k,
        search_options(graph_tier(seed)),
        QueryControl::Cancel(CancelToken::new()),
    )
}

fn graph_epoch() -> StoreEpoch {
    const GRAPH_DIMS: u32 = 128;
    let document = EmbeddingTower {
        model_id: "vector-adapter-sift".to_owned(),
        model_version: "1".to_owned(),
        weights_digest: vec![0x16],
        dims: GRAPH_DIMS,
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

fn graph_fixture(seed: u64) -> Result<GraphFixture, String> {
    const GRAPH_ROWS: usize = 12;
    const GRAPH_DIMS: usize = 128;
    let directory = tempdir().map_err(|error| format!("graph fault tempdir: {error}"))?;
    let epoch = graph_epoch();
    let store = Store::open(
        directory.path(),
        OpenOptions::default().with_epoch(epoch.clone()),
    )
    .map_err(|error| format!("open graph fault Store: {error}"))?;
    let document_base = u128::from(seed).wrapping_shl(64);
    let documents = (0..GRAPH_ROWS)
        .map(|row| {
            let amplitude = row as f32 + 1.0;
            IngestDocument::new(
                version(document_base | row as u128, 1),
                (0..GRAPH_DIMS)
                    .map(|dimension| {
                        if dimension.is_multiple_of(2) {
                            amplitude
                        } else {
                            -amplitude
                        }
                    })
                    .collect(),
            )
        })
        .collect();
    store
        .ingest(IngestBatch::new(documents).with_epoch(epoch.identity()))
        .map_err(|error| format!("ingest graph fault Store: {error}"))?;
    store
        .seal()
        .map_err(|error| format!("seal graph fault Store: {error}"))?;
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
            "graph fixture maintenance did not publish one graph: built={} status={:?}",
            report.graphs_built, report.status
        ));
    }
    let snapshot = store
        .snapshot()
        .map_err(|error| format!("snapshot graph fault Store: {error}"))?;
    let segment = snapshot
        .segments()
        .first()
        .ok_or_else(|| "graph fault Store published no segment".to_owned())?
        .meta()
        .id;
    drop(snapshot);
    let query = (0..GRAPH_DIMS)
        .map(|dimension| {
            if dimension.is_multiple_of(2) {
                1.0
            } else {
                -1.0
            }
        })
        .collect::<Vec<_>>();
    let clean = result_fact(graph_search(&store, &query, seed, ROWS.len()))?;
    if clean.status != independent::PrimitiveStatus::Ok || !clean.approximate {
        return Err(format!(
            "graph fixture did not execute public Graph tier: {clean:?}"
        ));
    }
    store
        .close()
        .map_err(|error| format!("close graph fault Store: {error}"))?;
    let frozen = FrozenStoreFixture::capture(directory.path())?;
    Ok(GraphFixture {
        directory,
        segment,
        rows: GRAPH_ROWS as u32,
        epoch,
        query,
        clean,
        frozen,
    })
}

fn sealed_int8_fixture(seed: u64) -> Result<Int8Fixture, String> {
    let directory = tempdir().map_err(|error| format!("Int8 fault tempdir: {error}"))?;
    let dependencies = StoreTestDependencies::new(Arc::new(StdVfs), Arc::new(SystemMonotonicClock))
        .with_vector_seal_scheme(ProductQuantScheme::Int8);
    let store =
        Store::open_with_test_dependencies(directory.path(), OpenOptions::default(), dependencies)
            .map_err(|error| format!("open Int8 fault Store: {error}"))?;
    let document_base = u128::from(seed).wrapping_shl(32);
    ingest_rows(
        &store,
        &[
            (document_base | 41, 1, ROWS[0]),
            (document_base | 42, 1, ROWS[1]),
            (document_base | 43, 1, ROWS[2]),
        ],
    )?;
    store
        .seal()
        .map_err(|error| format!("seal public-ingested Int8 fault Store: {error}"))?;
    let snapshot = store
        .snapshot()
        .map_err(|error| format!("snapshot Int8 fault Store: {error}"))?;
    let segment = snapshot
        .segments()
        .first()
        .ok_or_else(|| "public-ingested Int8 fault Store published no segment".to_owned())?
        .meta()
        .id;
    if snapshot.segments()[0].meta().scheme != u16::from(ProductQuantScheme::Int8.id()) {
        return Err("public-ingested Int8 fault Store did not seal Int8".to_owned());
    }
    drop(snapshot);
    let clean = result_fact(search(&store, SearchTier::Scan, ROWS.len()))?;
    if clean.candidates.len() != ROWS.len()
        || clean
            .candidates
            .iter()
            .any(|candidate| candidate.document.is_none())
    {
        return Err("public-ingested Int8 fault Store lost document identity".to_owned());
    }
    store
        .close()
        .map_err(|error| format!("close Int8 fault Store: {error}"))?;
    let frozen = FrozenStoreFixture::capture(directory.path())?;
    Ok(Int8Fixture {
        directory,
        segment,
        clean,
        frozen,
    })
}

fn fact_with_generation(
    result: Result<SearchOutcome, QueryError>,
    generation: u64,
) -> Result<VectorStoreResultFact, String> {
    let mut fact = result_fact(result)?;
    if fact.status != independent::PrimitiveStatus::Ok {
        fact.generation = generation;
    }
    Ok(fact)
}

fn run_quantization(
    seed: u64,
    fault: Option<VectorFaultKind>,
) -> Result<VectorOperationEvidence, String> {
    let bit4_input = quant_input(
        seed.rotate_left(8) ^ 0x2501,
        independent::QuantScheme::Bit4,
        &ROWS[0],
        seed,
        ROWS[0].len().div_ceil(2),
        ROWS[0].len().div_ceil(2),
        independent::QuantStoreInput {
            generation_before: 0,
            schedule: vec![independent::PublicStoreStep::IngestAccepted],
            document: Some(primitive_version(u128::from(seed).wrapping_shl(32) | 25, 1)),
            document_visible: true,
        },
    );
    let int8_input = quant_input(
        seed.rotate_left(8) ^ 0x2502,
        independent::QuantScheme::Int8,
        &ROWS[1],
        seed,
        ROWS[1].len(),
        ROWS[1].len(),
        independent::QuantStoreInput {
            generation_before: 0,
            schedule: vec![
                independent::PublicStoreStep::IngestAccepted,
                independent::PublicStoreStep::Seal,
                independent::PublicStoreStep::Reopen,
                independent::PublicStoreStep::Search,
            ],
            document: Some(primitive_version(u128::from(seed).wrapping_shl(32) | 27, 1)),
            document_visible: true,
        },
    );
    let invalid_row = [1.0_f32, f32::NAN, 0.5];
    let invalid_input = quant_input(
        seed.rotate_left(8) ^ 0x2503,
        independent::QuantScheme::Bit4,
        &invalid_row,
        seed,
        invalid_row.len().div_ceil(2),
        invalid_row.len().div_ceil(2),
        independent::QuantStoreInput {
            generation_before: 0,
            schedule: vec![independent::PublicStoreStep::IngestRejected],
            document: Some(primitive_version(u128::from(seed).wrapping_shl(32) | 26, 1)),
            document_visible: false,
        },
    );
    let mut pairs = vec![
        I25EvidencePair {
            observed: independent::I25Observed {
                case_id: bit4_input.case_id,
                result: observe_quant_result(&bit4_input),
                store: bit4_store_facts(&ROWS[0], u128::from(seed).wrapping_shl(32) | 25)?,
            },
            input: bit4_input,
        },
        I25EvidencePair {
            observed: independent::I25Observed {
                case_id: int8_input.case_id,
                result: observe_quant_result(&int8_input),
                store: int8_store_facts(&ROWS[1], u128::from(seed).wrapping_shl(32) | 27)?,
            },
            input: int8_input,
        },
        I25EvidencePair {
            observed: independent::I25Observed {
                case_id: invalid_input.case_id,
                result: observe_quant_result(&invalid_input),
                store: bit4_store_facts(&invalid_row, u128::from(seed).wrapping_shl(32) | 26)?,
            },
            input: invalid_input,
        },
    ];
    pairs.extend(
        quant_boundary_inputs(seed)
            .into_iter()
            .map(|input| I25EvidencePair {
                observed: independent::I25Observed {
                    case_id: input.case_id,
                    result: observe_quant_result(&input),
                    store: primitive_only_quant_facts(),
                },
                input,
            }),
    );
    pairs.extend(
        quant_positive_inputs(seed)
            .into_iter()
            .map(|input| I25EvidencePair {
                observed: independent::I25Observed {
                    case_id: input.case_id,
                    result: observe_quant_result(&input),
                    store: primitive_only_quant_facts(),
                },
                input,
            }),
    );

    let mut receipts = Vec::new();
    let mut mutation = VectorMutationEvidence::None;
    let (clean, fault_result, retry, clean_initial_directory, fault_initial_directory) = if fault
        == Some(VectorFaultKind::CorruptCodesFactors)
    {
        match (seed / 6) % 3 {
            0 => {
                let fixture = sealed_bit4_fixture(seed)?;
                let pair = isolated_pair(&fixture.frozen)?;
                let clean_store = Store::open(pair.clean.path(), OpenOptions::default())
                    .map_err(|error| format!("open Bit4 padding control Store: {error}"))?;
                let clean = result_fact(search(&clean_store, SearchTier::Scan, ROWS.len()))?;
                clean_store
                    .close()
                    .map_err(|error| format!("close Bit4 padding control Store: {error}"))?;
                let case_id = seed.rotate_left(16) ^ 0x25_01;
                let source = independent::PrimitiveSource::Sealed(*fixture.segment.as_bytes());
                let controller = VectorFaultController::armed(
                    VectorFault::CorruptBit4OddPadding {
                        source: VectorRowSource::Sealed(*fixture.segment.as_bytes()),
                        local_row: 0,
                    },
                    case_id,
                );
                mutation = VectorMutationEvidence::QuantCorruption {
                    case_id,
                    scheme: independent::QuantScheme::Bit4,
                    source,
                    tier: 2,
                    local_row: 0,
                    field: VectorQuantMutationField::Bit4OddPadding,
                };
                let store = Store::open_with_test_dependencies(
                    pair.fault.path(),
                    OpenOptions::default(),
                    vector_dependencies(controller.clone()),
                )
                .map_err(|error| format!("open Bit4 padding fault Store: {error}"))?;
                let generation = store
                    .snapshot()
                    .map_err(|error| format!("snapshot Bit4 padding fault: {error}"))?
                    .generation();
                let fault_result =
                    fact_with_generation(search(&store, SearchTier::Scan, ROWS.len()), generation)?;
                receipts = controller.take_typed_receipts();
                let retry = result_fact(search(&store, SearchTier::Scan, ROWS.len()))?;
                if !controller.take_typed_receipts().is_empty() {
                    return Err("Bit4 padding controller fired twice".to_owned());
                }
                store
                    .close()
                    .map_err(|error| format!("close Bit4 padding fault Store: {error}"))?;
                (
                    clean,
                    fault_result,
                    retry,
                    pair.clean_initial,
                    pair.fault_initial,
                )
            }
            1 => {
                let fixture = sealed_bit4_fixture(seed)?;
                let pair = isolated_pair(&fixture.frozen)?;
                let clean_store = Store::open(pair.clean.path(), OpenOptions::default())
                    .map_err(|error| format!("open Bit4 factor control Store: {error}"))?;
                let clean = result_fact(search(&clean_store, SearchTier::Scan, ROWS.len()))?;
                clean_store
                    .close()
                    .map_err(|error| format!("close Bit4 factor control Store: {error}"))?;
                let case_id = seed.rotate_left(16) ^ 0x25_02;
                let source = independent::PrimitiveSource::Sealed(*fixture.segment.as_bytes());
                let controller = VectorFaultController::armed(
                    VectorFault::CorruptBit4CorrectionNaN {
                        source: VectorRowSource::Sealed(*fixture.segment.as_bytes()),
                        local_row: 0,
                    },
                    case_id,
                );
                mutation = VectorMutationEvidence::QuantCorruption {
                    case_id,
                    scheme: independent::QuantScheme::Bit4,
                    source,
                    tier: 2,
                    local_row: 0,
                    field: VectorQuantMutationField::Bit4Correction,
                };
                let store = Store::open_with_test_dependencies(
                    pair.fault.path(),
                    OpenOptions::default(),
                    vector_dependencies(controller.clone()),
                )
                .map_err(|error| format!("open Bit4 factor fault Store: {error}"))?;
                let generation = store
                    .snapshot()
                    .map_err(|error| format!("snapshot Bit4 factor fault: {error}"))?
                    .generation();
                let fault_result =
                    fact_with_generation(search(&store, SearchTier::Scan, ROWS.len()), generation)?;
                receipts = controller.take_typed_receipts();
                let retry = result_fact(search(&store, SearchTier::Scan, ROWS.len()))?;
                if !controller.take_typed_receipts().is_empty() {
                    return Err("Bit4 factor controller fired twice".to_owned());
                }
                store
                    .close()
                    .map_err(|error| format!("close Bit4 factor fault Store: {error}"))?;
                (
                    clean,
                    fault_result,
                    retry,
                    pair.clean_initial,
                    pair.fault_initial,
                )
            }
            _ => {
                let fixture = sealed_int8_fixture(seed)?;
                let pair = isolated_pair(&fixture.frozen)?;
                let clean_store = Store::open(pair.clean.path(), OpenOptions::default())
                    .map_err(|error| format!("open Int8 factor control Store: {error}"))?;
                let clean = result_fact(search(&clean_store, SearchTier::Scan, ROWS.len()))?;
                clean_store
                    .close()
                    .map_err(|error| format!("close Int8 factor control Store: {error}"))?;
                let case_id = seed.rotate_left(16) ^ 0x25_03;
                let source = independent::PrimitiveSource::Sealed(*fixture.segment.as_bytes());
                let controller = VectorFaultController::armed(
                    VectorFault::CorruptInt8ScaleNaN {
                        source: VectorRowSource::Sealed(*fixture.segment.as_bytes()),
                        local_row: 0,
                    },
                    case_id,
                );
                mutation = VectorMutationEvidence::QuantCorruption {
                    case_id,
                    scheme: independent::QuantScheme::Int8,
                    source,
                    tier: 2,
                    local_row: 0,
                    field: VectorQuantMutationField::Int8Scale,
                };
                let store = Store::open_with_test_dependencies(
                    pair.fault.path(),
                    OpenOptions::default(),
                    vector_dependencies(controller.clone()),
                )
                .map_err(|error| format!("open Int8 factor fault Store: {error}"))?;
                let generation = store
                    .snapshot()
                    .map_err(|error| format!("snapshot Int8 factor fault: {error}"))?
                    .generation();
                let fault_result =
                    fact_with_generation(search(&store, SearchTier::Scan, ROWS.len()), generation)?;
                receipts = controller.take_typed_receipts();
                let retry = result_fact(search(&store, SearchTier::Scan, ROWS.len()))?;
                if !controller.take_typed_receipts().is_empty() {
                    return Err("Int8 factor controller fired twice".to_owned());
                }
                store
                    .close()
                    .map_err(|error| format!("close Int8 factor fault Store: {error}"))?;
                (
                    clean,
                    fault_result,
                    retry,
                    pair.clean_initial,
                    pair.fault_initial,
                )
            }
        }
    } else {
        let fixture = sealed_bit4_fixture(seed)?;
        let pair = isolated_pair(&fixture.frozen)?;
        let clean_store = Store::open(pair.clean.path(), OpenOptions::default())
            .map_err(|error| format!("open quantization control Store: {error}"))?;
        let clean = result_fact(search(&clean_store, SearchTier::Scan, ROWS.len()))?;
        clean_store
            .close()
            .map_err(|error| format!("close quantization control Store: {error}"))?;
        let retry_store = Store::open(pair.fault.path(), OpenOptions::default())
            .map_err(|error| format!("open quantization retry Store: {error}"))?;
        let retry = result_fact(search(&retry_store, SearchTier::Scan, ROWS.len()))?;
        retry_store
            .close()
            .map_err(|error| format!("close quantization retry Store: {error}"))?;
        (
            clean.clone(),
            clean,
            retry,
            pair.clean_initial,
            pair.fault_initial,
        )
    };
    let public_schedule = pairs
        .iter()
        .flat_map(|pair| pair.input.store.schedule.iter().copied())
        .collect();
    let invariant = VectorInvariantEvidence::I25(pairs);
    let fixture_evidence = primitive_fixture(
        VectorOperationKind::Quantization,
        seed,
        &invariant,
        &[2],
        &fact_sources(&clean),
        public_schedule,
    );
    Ok(VectorOperationEvidence {
        operation: VectorOperationKind::Quantization,
        fault,
        fixture: fixture_evidence,
        mutation,
        invariant,
        receipts,
        forced_child: None,
        generic_fault: None,
        control: VectorControlEvidence {
            namespace: "vector-execution/quantization",
            operation: VectorOperationKind::Quantization,
            seed,
            clean,
            fault: fault_result,
            retry,
            clean_initial_directory,
            fault_initial_directory,
            isolated_directories: true,
        },
    })
}

fn rescore_status(error: RescoreError) -> independent::PrimitiveStatus {
    match error {
        RescoreError::CandidateRowCount { expected, actual } => {
            independent::PrimitiveStatus::CandidateRowCount {
                expected: expected as u64,
                actual: actual as u64,
            }
        }
        RescoreError::CandidateRowOutOfRange {
            row_index,
            row_count,
            ..
        } => independent::PrimitiveStatus::CandidateRowOutOfRange {
            row: row_index as u64,
            rows: row_count as u64,
        },
        RescoreError::NonFiniteCoarseScore { index }
        | RescoreError::NonFiniteExactScore { row_index: index } => {
            independent::PrimitiveStatus::NonFiniteScore { row: index as u64 }
        }
        other => independent::PrimitiveStatus::SegmentGeometry {
            detail: other.to_string(),
        },
    }
}

fn observe_rescore(
    input: &independent::RescoreInput,
) -> Result<
    (
        independent::PrimitiveStatus,
        Vec<independent::PrimitiveRescoreHit>,
        [u64; 4],
    ),
    String,
> {
    let query = input
        .query
        .iter()
        .map(|value| value.to_float())
        .collect::<Vec<_>>();
    let rows = input
        .rows_row_major
        .iter()
        .map(|value| value.to_float())
        .collect::<Vec<_>>();
    let dimension = usize::try_from(input.dimension)
        .map_err(|_| "rescore dimension exceeds usize".to_owned())?;
    let k = usize::try_from(input.k).map_err(|_| "rescore k exceeds usize".to_owned())?;
    let result = match &input.candidates {
        independent::CandidateMode::Dense { coarse, oversample } => {
            let coarse = coarse
                .iter()
                .map(|value| value.to_float())
                .collect::<Vec<_>>();
            let oversample = usize::try_from(*oversample)
                .map_err(|_| "rescore oversample exceeds usize".to_owned())?;
            rescore_top_k(
                &query,
                &rows,
                dimension,
                RescorePool::dense(
                    &coarse,
                    oversample,
                    usize::try_from(input.coarse_bytes_per_row)
                        .map_err(|_| "coarse row bytes exceed usize".to_owned())?,
                ),
                k,
            )
        }
        independent::CandidateMode::Retained {
            rows: retained,
            coarse,
        } => {
            let coarse = coarse
                .iter()
                .map(|value| value.to_float())
                .collect::<Vec<_>>();
            let metric = match input.metric {
                independent::RescoreMetric::InnerProduct => RescoreMetric::InnerProduct,
                independent::RescoreMetric::SquaredL2 => RescoreMetric::SquaredL2,
            };
            rescore_top_k(
                &query,
                &rows,
                dimension,
                RescorePool::retained(
                    retained,
                    &coarse,
                    metric,
                    usize::try_from(input.coarse_rows_touched)
                        .map_err(|_| "coarse rows touched exceeds usize".to_owned())?,
                    usize::try_from(input.coarse_bytes_per_row)
                        .map_err(|_| "coarse row bytes exceed usize".to_owned())?,
                ),
                k,
            )
        }
    };
    match result {
        Ok(result) => Ok((
            independent::PrimitiveStatus::Ok,
            result
                .hits
                .into_iter()
                .map(|hit| independent::PrimitiveRescoreHit {
                    row: hit.row_index as u32,
                    score: independent::F64::from_float(hit.score),
                })
                .collect(),
            [
                result.candidates_rescored as u64,
                result.bytes.coarse as u64,
                result.bytes.rescore as u64,
                result.bytes.total() as u64,
            ],
        )),
        Err(error) => Ok((rescore_status(error), Vec::new(), [0; 4])),
    }
}

fn store_rescore_hits(outcome: &SearchOutcome) -> Vec<independent::StoreRescoreHit> {
    outcome
        .candidates
        .iter()
        .map(|candidate| independent::StoreRescoreHit {
            row: independent::PrimitiveRow {
                source: primitive_source(candidate.row_id().source()),
                local_row: candidate.row_id().local_row(),
            },
            document: candidate.document().map(primitive_document),
            score: independent::F32::from_float(candidate.score()),
            exact_score: outcome.diagnostics.exact_rescore,
        })
        .collect()
}

fn rescore_input(
    seed: u64,
    metric: independent::RescoreMetric,
    store: Option<independent::RescoreStoreInput>,
) -> independent::RescoreInput {
    independent::RescoreInput {
        case_id: seed.rotate_left(8)
            ^ match metric {
                independent::RescoreMetric::InnerProduct => 0x2601,
                independent::RescoreMetric::SquaredL2 => 0x2602,
            },
        metric,
        query: QUERY
            .iter()
            .copied()
            .map(independent::F32::from_float)
            .collect(),
        rows_row_major: ROWS
            .into_iter()
            .flatten()
            .map(independent::F32::from_float)
            .collect(),
        dimension: QUERY.len() as u64,
        k: ROWS.len() as u64,
        candidates: independent::CandidateMode::Retained {
            rows: vec![0, 1, 2],
            coarse: [2.25_f32, 1.125, -2.25]
                .into_iter()
                .map(independent::F32::from_float)
                .collect(),
        },
        coarse_rows_touched: ROWS.len() as u64,
        coarse_bytes_per_row: QUERY.len().div_ceil(2) as u64 + 12,
        store,
    }
}

fn primitive_rescore_corpus(seed: u64) -> Vec<independent::RescoreInput> {
    let mut retained = rescore_input(seed, independent::RescoreMetric::InnerProduct, None);
    retained.case_id ^= 0x2600_1000;

    let mut dense = retained.clone();
    dense.case_id ^= 1;
    dense.k = 2;
    dense.candidates = independent::CandidateMode::Dense {
        coarse: [2.25_f32, 1.125, -2.25]
            .into_iter()
            .map(independent::F32::from_float)
            .collect(),
        oversample: 2,
    };

    let mut count = retained.clone();
    count.case_id ^= 2;
    count.candidates = independent::CandidateMode::Retained {
        rows: vec![0, 1],
        coarse: [2.25_f32, 1.125, -2.25]
            .into_iter()
            .map(independent::F32::from_float)
            .collect(),
    };

    let mut out_of_range = retained.clone();
    out_of_range.case_id ^= 3;
    out_of_range.candidates = independent::CandidateMode::Retained {
        rows: vec![0, 1, 9],
        coarse: [2.25_f32, 1.125, -2.25]
            .into_iter()
            .map(independent::F32::from_float)
            .collect(),
    };

    let mut non_finite = retained.clone();
    non_finite.case_id ^= 4;
    non_finite.candidates = independent::CandidateMode::Retained {
        rows: vec![2, 0, 1],
        coarse: [2.25_f32, f32::NAN, -2.25]
            .into_iter()
            .map(independent::F32::from_float)
            .collect(),
    };
    vec![retained, dense, count, out_of_range, non_finite]
}

fn anti_correlated_tie_rescore_input(seed: u64, document_base: u128) -> independent::RescoreInput {
    let documents_by_row = [330_u128, 220, 110]
        .into_iter()
        .map(|document| Some(primitive_version(document_base | document, 1)))
        .collect();
    independent::RescoreInput {
        case_id: seed.rotate_left(8) ^ 0x26ac_71e0,
        metric: independent::RescoreMetric::SquaredL2,
        query: QUERY
            .iter()
            .copied()
            .map(independent::F32::from_float)
            .collect(),
        rows_row_major: std::iter::repeat_n(QUERY, 3)
            .flatten()
            .map(independent::F32::from_float)
            .collect(),
        dimension: QUERY.len() as u64,
        k: 3,
        candidates: independent::CandidateMode::Retained {
            rows: vec![0, 1, 2],
            coarse: vec![independent::F32::from_float(0.0); 3],
        },
        coarse_rows_touched: 3,
        coarse_bytes_per_row: QUERY.len().div_ceil(2) as u64 + 12,
        store: Some(independent::RescoreStoreInput {
            source: independent::PrimitiveSource::Active,
            documents_by_row,
            tier: 1,
            exact_rescore: true,
        }),
    }
}

fn graph_rescore_input(
    seed: u64,
    graph: &GraphFixture,
    tier: u8,
    outcome: &SearchOutcome,
) -> independent::RescoreInput {
    const GRAPH_DIMS: usize = 128;
    let document_base = u128::from(seed).wrapping_shl(64);
    let rows = (0..usize::try_from(graph.rows).unwrap_or(0))
        .flat_map(|row| {
            let amplitude = row as f32 + 1.0;
            (0..GRAPH_DIMS).map(move |dimension| {
                if dimension.is_multiple_of(2) {
                    amplitude
                } else {
                    -amplitude
                }
            })
        })
        .map(independent::F32::from_float)
        .collect::<Vec<_>>();
    let retained = outcome
        .candidates
        .iter()
        .map(|candidate| candidate.row_id().local_row())
        .collect::<Vec<_>>();
    let retained_len = retained.len();
    independent::RescoreInput {
        case_id: seed.rotate_left(8) ^ 0x2603 ^ u64::from(tier),
        metric: independent::RescoreMetric::SquaredL2,
        query: graph
            .query
            .iter()
            .copied()
            .map(independent::F32::from_float)
            .collect(),
        rows_row_major: rows,
        dimension: GRAPH_DIMS as u64,
        k: outcome.candidates.len() as u64,
        candidates: independent::CandidateMode::Retained {
            rows: retained,
            coarse: vec![independent::F32::from_float(0.0); retained_len],
        },
        coarse_rows_touched: u64::from(graph.rows),
        coarse_bytes_per_row: GRAPH_DIMS.div_ceil(2) as u64 + 12,
        store: Some(independent::RescoreStoreInput {
            source: independent::PrimitiveSource::Sealed(*graph.segment.as_bytes()),
            documents_by_row: (0..graph.rows)
                .map(|row| Some(primitive_version(document_base | u128::from(row), 1)))
                .collect(),
            tier,
            exact_rescore: true,
        }),
    }
}

fn public_rescore_pair(
    input: independent::RescoreInput,
    outcome: Option<&SearchOutcome>,
) -> Result<I26EvidencePair, String> {
    let (primitive_status, primitive_hits, primitive_counts) = observe_rescore(&input)?;
    Ok(I26EvidencePair {
        observed: independent::I26Observed {
            case_id: input.case_id,
            primitive_status,
            primitive_hits,
            primitive_counts,
            tier: input.store.as_ref().map_or(0, |store| store.tier),
            store_hits: outcome.map_or_else(Vec::new, store_rescore_hits),
            exact_rescore: outcome.is_some_and(|outcome| outcome.diagnostics.exact_rescore),
        },
        input,
    })
}

fn run_rescore(
    seed: u64,
    fault: Option<VectorFaultKind>,
) -> Result<VectorOperationEvidence, String> {
    let active_directory = tempdir().map_err(|error| format!("active rescore tempdir: {error}"))?;
    let active_store = Store::open(active_directory.path(), OpenOptions::default())
        .map_err(|error| format!("open active rescore Store: {error}"))?;
    let document_base = u128::from(seed).wrapping_shl(32);
    ingest_rows(
        &active_store,
        &[
            (document_base | 31, 1, ROWS[0]),
            (document_base | 32, 1, ROWS[1]),
            (document_base | 33, 1, ROWS[2]),
        ],
    )?;
    let mut pairs = primitive_rescore_corpus(seed)
        .into_iter()
        .map(|input| public_rescore_pair(input, None))
        .collect::<Result<Vec<_>, _>>()?;
    let tie_directory =
        tempdir().map_err(|error| format!("anti-correlated tie tempdir: {error}"))?;
    let tie_store = Store::open(tie_directory.path(), OpenOptions::default())
        .map_err(|error| format!("open anti-correlated tie Store: {error}"))?;
    ingest_rows(
        &tie_store,
        &[
            (document_base | 330, 1, QUERY),
            (document_base | 220, 1, QUERY),
            (document_base | 110, 1, QUERY),
        ],
    )?;
    let tie_outcome = search(&tie_store, SearchTier::Exact, 3)
        .map_err(|error| format!("anti-correlated public Exact rescore: {error}"))?;
    pairs.push(public_rescore_pair(
        anti_correlated_tie_rescore_input(seed, document_base),
        Some(&tie_outcome),
    )?);
    tie_store
        .close()
        .map_err(|error| format!("close anti-correlated tie Store: {error}"))?;
    for (tag, tier) in [
        (1_u8, SearchTier::Exact),
        (2_u8, SearchTier::Scan),
        (0_u8, SearchTier::Auto),
    ] {
        let outcome = search(&active_store, tier, ROWS.len())
            .map_err(|error| format!("active public {tier:?} rescore: {error}"))?;
        let (metric, exact_rescore) = if tag == 1 {
            (independent::RescoreMetric::SquaredL2, true)
        } else {
            (independent::RescoreMetric::InnerProduct, false)
        };
        let input = rescore_input(
            seed ^ u64::from(tag),
            metric,
            Some(independent::RescoreStoreInput {
                source: independent::PrimitiveSource::Active,
                documents_by_row: [31_u128, 32, 33]
                    .into_iter()
                    .map(|document| Some(primitive_version(document_base | document, 1)))
                    .collect(),
                tier: tag,
                exact_rescore,
            }),
        );
        pairs.push(public_rescore_pair(input, Some(&outcome))?);
    }
    active_store
        .close()
        .map_err(|error| format!("close active rescore Store: {error}"))?;

    let fixture = sealed_bit4_fixture(seed)?;
    let rescore_pair = isolated_pair(&fixture.frozen)?;
    let clean_store = Store::open(rescore_pair.clean.path(), OpenOptions::default())
        .map_err(|error| format!("open rescore control Store: {error}"))?;
    let clean_outcome = search(&clean_store, SearchTier::Exact, ROWS.len())
        .map_err(|error| format!("public exact rescore control: {error}"))?;
    let clean = result_fact(Ok(clean_outcome.clone()))?;
    clean_store
        .close()
        .map_err(|error| format!("close rescore control Store: {error}"))?;

    let squared_input = rescore_input(
        seed,
        independent::RescoreMetric::SquaredL2,
        Some(independent::RescoreStoreInput {
            source: independent::PrimitiveSource::Sealed(*fixture.segment.as_bytes()),
            documents_by_row: [31_u128, 32, 33]
                .into_iter()
                .map(|document| Some(primitive_version(document_base | document, 1)))
                .collect(),
            tier: 1,
            exact_rescore: true,
        }),
    );
    let squared = public_rescore_pair(squared_input, Some(&clean_outcome))?;
    pairs.push(squared);

    let graph_evidence = graph_fixture(seed)?;
    let graph_store = Store::open(
        graph_evidence.directory.path(),
        OpenOptions::default().with_epoch(graph_evidence.epoch.clone()),
    )
    .map_err(|error| format!("open graph rescore evidence Store: {error}"))?;
    let graph_outcome = graph_search(&graph_store, &graph_evidence.query, seed, ROWS.len())
        .map_err(|error| format!("public Graph rescore evidence: {error}"))?;
    pairs.push(public_rescore_pair(
        graph_rescore_input(seed, &graph_evidence, 3, &graph_outcome),
        Some(&graph_outcome),
    )?);
    let auto_outcome = search_vector(
        &graph_store,
        &graph_evidence.query,
        SearchTier::Auto,
        ROWS.len(),
    )
    .map_err(|error| format!("public graph-ready Auto rescore evidence: {error}"))?;
    let mut auto_input = graph_rescore_input(seed, &graph_evidence, 0, &auto_outcome);
    auto_input.case_id ^= 0x2600;
    pairs.push(public_rescore_pair(auto_input, Some(&auto_outcome))?);
    graph_store
        .close()
        .map_err(|error| format!("close graph rescore evidence Store: {error}"))?;

    let mut receipts = Vec::new();
    let mut mutation = VectorMutationEvidence::None;
    let (operation_clean, fault_result, retry, clean_initial_directory, fault_initial_directory) =
        if fault == Some(VectorFaultKind::MissingRescoreRows) && (seed / 6).is_multiple_of(2) {
            let case_id = seed.rotate_left(16) ^ 0x26_01;
            let source = independent::PrimitiveSource::Sealed(*fixture.segment.as_bytes());
            let controller = VectorFaultController::armed(
                VectorFault::MissingRescoreRows {
                    source: VectorRowSource::Sealed(*fixture.segment.as_bytes()),
                    site: MissingRescoreSite::ExactRescoreRows,
                    expected_rows: ROWS.len() as u32,
                    available_rows: (ROWS.len() - 1) as u32,
                    tier: VectorSearchTier::Exact,
                },
                case_id,
            );
            mutation = VectorMutationEvidence::MissingRescoreRows {
                case_id,
                source,
                tier: 1,
                site: VectorRescoreMutationSite::ExactRescoreRows,
                expected_rows: ROWS.len() as u32,
                available_rows: (ROWS.len() - 1) as u32,
            };
            let store = Store::open_with_test_dependencies(
                rescore_pair.fault.path(),
                OpenOptions::default(),
                vector_dependencies(controller.clone()),
            )
            .map_err(|error| format!("open missing-rescore Store: {error}"))?;
            let generation = store
                .snapshot()
                .map_err(|error| format!("snapshot missing-rescore Store: {error}"))?
                .generation();
            let fault_result =
                fact_with_generation(search(&store, SearchTier::Exact, ROWS.len()), generation)?;
            receipts = controller.take_typed_receipts();
            let retry = result_fact(search(&store, SearchTier::Exact, ROWS.len()))?;
            if !controller.take_typed_receipts().is_empty() {
                return Err("missing-rescore controller fired twice".to_owned());
            }
            store
                .close()
                .map_err(|error| format!("close missing-rescore Store: {error}"))?;
            (
                clean.clone(),
                fault_result,
                retry,
                rescore_pair.clean_initial,
                rescore_pair.fault_initial,
            )
        } else if fault == Some(VectorFaultKind::MissingRescoreRows) {
            let graph = graph_evidence;
            let graph_pair = isolated_pair(&graph.frozen)?;
            let clean_store = Store::open(
                graph_pair.clean.path(),
                OpenOptions::default().with_epoch(graph.epoch.clone()),
            )
            .map_err(|error| format!("open graph rescore control Store: {error}"))?;
            let clean = result_fact(graph_search(&clean_store, &graph.query, seed, ROWS.len()))?;
            clean_store
                .close()
                .map_err(|error| format!("close graph rescore control Store: {error}"))?;
            let case_id = seed.rotate_left(16) ^ 0x26_02;
            let source = independent::PrimitiveSource::Sealed(*graph.segment.as_bytes());
            let controller = VectorFaultController::armed(
                VectorFault::MissingRescoreRows {
                    source: VectorRowSource::Sealed(*graph.segment.as_bytes()),
                    site: MissingRescoreSite::QueryRescoreRows,
                    expected_rows: graph.rows,
                    available_rows: graph.rows - 1,
                    tier: VectorSearchTier::Graph,
                },
                case_id,
            );
            mutation = VectorMutationEvidence::MissingRescoreRows {
                case_id,
                source,
                tier: 3,
                site: VectorRescoreMutationSite::QueryRescoreRows,
                expected_rows: graph.rows,
                available_rows: graph.rows - 1,
            };
            let store = Store::open_with_test_dependencies(
                graph_pair.fault.path(),
                OpenOptions::default().with_epoch(graph.epoch.clone()),
                vector_dependencies(controller.clone()),
            )
            .map_err(|error| format!("open graph missing-rescore Store: {error}"))?;
            let generation = store
                .snapshot()
                .map_err(|error| format!("snapshot graph missing-rescore Store: {error}"))?
                .generation();
            let fault_result = fact_with_generation(
                graph_search(&store, &graph.query, seed, ROWS.len()),
                generation,
            )?;
            receipts = controller.take_typed_receipts();
            let retry = result_fact(graph_search(&store, &graph.query, seed, ROWS.len()))?;
            if !controller.take_typed_receipts().is_empty() {
                return Err("graph missing-rescore controller fired twice".to_owned());
            }
            store
                .close()
                .map_err(|error| format!("close graph missing-rescore Store: {error}"))?;
            (
                clean,
                fault_result,
                retry,
                graph_pair.clean_initial,
                graph_pair.fault_initial,
            )
        } else {
            let retry_store = Store::open(rescore_pair.fault.path(), OpenOptions::default())
                .map_err(|error| format!("open rescore retry Store: {error}"))?;
            let retry = result_fact(search(&retry_store, SearchTier::Exact, ROWS.len()))?;
            retry_store
                .close()
                .map_err(|error| format!("close rescore retry Store: {error}"))?;
            (
                clean.clone(),
                clean.clone(),
                retry,
                rescore_pair.clean_initial,
                rescore_pair.fault_initial,
            )
        };
    let invariant = VectorInvariantEvidence::I26(pairs);
    let extra_tiers = if matches!(
        mutation,
        VectorMutationEvidence::MissingRescoreRows { tier: 3, .. }
    ) {
        vec![1, 3]
    } else {
        vec![1]
    };
    let fixture_evidence = primitive_fixture(
        VectorOperationKind::Rescore,
        seed,
        &invariant,
        &extra_tiers,
        &fact_sources(&operation_clean),
        vec![
            independent::PublicStoreStep::IngestAccepted,
            independent::PublicStoreStep::Seal,
        ],
    );
    Ok(VectorOperationEvidence {
        operation: VectorOperationKind::Rescore,
        fault,
        fixture: fixture_evidence,
        mutation,
        invariant,
        receipts,
        forced_child: None,
        generic_fault: None,
        control: VectorControlEvidence {
            namespace: "vector-execution/rescore",
            operation: VectorOperationKind::Rescore,
            seed,
            clean: operation_clean,
            fault: fault_result,
            retry,
            clean_initial_directory,
            fault_initial_directory,
            isolated_directories: true,
        },
    })
}

#[derive(Clone)]
struct TierOutcome {
    tag: u8,
    tier: SearchTier,
    outcome: SearchOutcome,
}

#[derive(Clone)]
struct IdentityPhase {
    observation_phase: u8,
    mutations: Vec<independent::IdentityMutation>,
    public_schedule: Vec<independent::PublicStoreStep>,
    clean: Vec<TierOutcome>,
    control: Vec<TierOutcome>,
    retry: Vec<TierOutcome>,
}

struct IdentityFixture {
    directory: TempDir,
    frozen: FrozenStoreFixture,
    epoch: StoreEpoch,
    phases: Vec<IdentityPhase>,
    mutations: Vec<independent::IdentityMutation>,
    public_schedule: Vec<independent::PublicStoreStep>,
    phase: u8,
    clean: Vec<TierOutcome>,
    control: Vec<TierOutcome>,
}

fn version(document: u128, revision: u64) -> DocumentVersion {
    DocumentVersion::new(DocId::new(document), Revision::new(revision))
}

fn primitive_version(document: u128, revision: u64) -> independent::PrimitiveDocument {
    independent::PrimitiveDocument {
        doc_id_be: document.to_be_bytes(),
        revision,
    }
}

fn published_segments(store: &Store) -> Result<BTreeSet<SegmentId>, String> {
    store
        .snapshot()
        .map_err(|error| format!("snapshot identity segments: {error}"))
        .map(|snapshot| {
            snapshot
                .segments()
                .iter()
                .map(|segment| segment.meta().id)
                .collect()
        })
}

fn published_segment_order(store: &Store) -> Result<Vec<SegmentId>, String> {
    store
        .snapshot()
        .map_err(|error| format!("snapshot ordered identity segments: {error}"))
        .map(|snapshot| {
            snapshot
                .segments()
                .iter()
                .map(|segment| segment.meta().id)
                .collect()
        })
}

fn ingest_identity(
    store: &Store,
    epoch: &StoreEpoch,
    document: u128,
    revision: u64,
    vector: &[f32],
) -> Result<(), String> {
    store
        .ingest(
            IngestBatch::new(vec![IngestDocument::new(
                version(document, revision),
                vector.to_vec(),
            )])
            .with_epoch(epoch.identity()),
        )
        .map(|_| ())
        .map_err(|error| format!("ingest identity {document}/{revision}: {error}"))
}

fn seal_identity(store: &Store) -> Result<SegmentId, String> {
    let before = published_segments(store)?;
    store
        .seal()
        .map_err(|error| format!("seal identity fixture: {error}"))?;
    let after = published_segments(store)?;
    let new = after.difference(&before).copied().collect::<Vec<_>>();
    if new.len() != 1 {
        return Err(format!(
            "identity seal published {} new segments, expected one",
            new.len()
        ));
    }
    Ok(new[0])
}

fn identity_tiers(seed: u64, graph_ready: bool) -> Vec<(u8, SearchTier)> {
    let mut tiers = vec![
        (1, SearchTier::Exact),
        (2, SearchTier::Scan),
        (0, SearchTier::Auto),
    ];
    if graph_ready {
        tiers.push((3, graph_tier(seed)));
    }
    tiers
}

fn identity_queries(
    store: &Store,
    seed: u64,
    graph_ready: bool,
    k: usize,
) -> Result<Vec<TierOutcome>, String> {
    identity_tiers(seed, graph_ready)
        .into_iter()
        .map(|(tag, tier)| {
            let outcome = search(store, tier, k)
                .map_err(|error| format!("identity tier {tier:?}: {error}"))?;
            Ok(TierOutcome { tag, tier, outcome })
        })
        .collect()
}

fn replace_identity_source(
    mutations: &mut [independent::IdentityMutation],
    source: SegmentId,
    replacement: SegmentId,
) -> Result<(), String> {
    let matching = mutations
        .iter_mut()
        .filter(|mutation| {
            matches!(
                mutation,
                independent::IdentityMutation::Seal(segment)
                    if *segment == *source.as_bytes()
            )
        })
        .collect::<Vec<_>>();
    if matching.len() != 1 {
        return Err(format!(
            "identity primitive schedule names source {source} {} times, expected one",
            matching.len()
        ));
    }
    if let Some(seal) = matching.into_iter().next() {
        *seal = independent::IdentityMutation::Seal(*replacement.as_bytes());
    }
    Ok(())
}

fn build_identity_fixture(seed: u64) -> Result<IdentityFixture, String> {
    let directory = tempdir().map_err(|error| format!("identity tempdir: {error}"))?;
    let epoch = StoreEpoch {
        embedding: EmbeddingEpoch {
            query: EmbeddingTower {
                model_id: "vector-identity".to_owned(),
                model_version: "1".to_owned(),
                weights_digest: vec![0x27],
                dims: QUERY.len() as u32,
                normalization: Normalization::None,
                prompt_prefix: String::new(),
                max_tokens: 32,
                runtime: EmbeddingRuntime::CpuReference,
                compute_units: ComputeUnits::Cpu,
                os_build: None,
            },
            document: EmbeddingTower {
                model_id: "vector-identity".to_owned(),
                model_version: "1".to_owned(),
                weights_digest: vec![0x27],
                dims: QUERY.len() as u32,
                normalization: Normalization::None,
                prompt_prefix: String::new(),
                max_tokens: 32,
                runtime: EmbeddingRuntime::CpuReference,
                compute_units: ComputeUnits::Cpu,
                os_build: None,
            },
            alignment_digest: Vec::new(),
        },
        tokenizer: TokenizerConfig::text_default().epoch(),
    };
    let store = Store::open(
        directory.path(),
        OpenOptions::default().with_epoch(epoch.clone()),
    )
    .map_err(|error| format!("open identity Store: {error}"))?;
    let base = u128::from(seed).wrapping_shl(64);
    let mut mutations = Vec::new();
    let mut public_schedule = Vec::new();
    let mut phases = Vec::new();
    let mut record_phase = |observation_phase: u8,
                            graph_ready: bool,
                            live: usize,
                            mutations: &[independent::IdentityMutation],
                            public_schedule: &[independent::PublicStoreStep]|
     -> Result<(), String> {
        phases.push(IdentityPhase {
            observation_phase,
            mutations: mutations.to_vec(),
            public_schedule: public_schedule.to_vec(),
            clean: identity_queries(&store, seed, graph_ready, live)
                .map_err(|error| format!("identity phase {observation_phase} clean: {error}"))?,
            control: identity_queries(&store, seed, graph_ready, live)
                .map_err(|error| format!("identity phase {observation_phase} control: {error}"))?,
            retry: identity_queries(&store, seed, graph_ready, live)
                .map_err(|error| format!("identity phase {observation_phase} retry: {error}"))?,
        });
        Ok(())
    };

    for ordinal in (1_u128..=12).rev() {
        let document = base | ordinal.saturating_mul(100);
        ingest_identity(&store, &epoch, document, 1, &QUERY)?;
        mutations.push(independent::IdentityMutation::Ingest(primitive_version(
            document, 1,
        )));
        public_schedule.push(independent::PublicStoreStep::IngestAccepted);
    }
    record_phase(0, false, 12, &mutations, &public_schedule)?;
    let first = seal_identity(&store)?;
    mutations.push(independent::IdentityMutation::Seal(*first.as_bytes()));
    public_schedule.push(independent::PublicStoreStep::Seal);
    record_phase(1, false, 12, &mutations, &public_schedule)?;
    let first_promotion_sources = published_segment_order(&store)?;
    let report = store.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: Duration::from_secs(30),
            bytes: u64::MAX,
        },
        TierThresholds { graph_min_rows: 4 },
    );
    if report.graphs_built != 1 || !matches!(report.status, MaintenanceStatus::Complete) {
        return Err(format!(
            "identity graph maintenance failed: built={} status={:?}",
            report.graphs_built, report.status
        ));
    }
    if first_promotion_sources.len() != report.graph_profiles.len() {
        return Err(format!(
            "identity first promotion scheduled {} sources but reported {} outputs",
            first_promotion_sources.len(),
            report.graph_profiles.len()
        ));
    }
    for (source, profile) in first_promotion_sources.iter().zip(&report.graph_profiles) {
        replace_identity_source(&mut mutations, *source, profile.segment_id)?;
    }
    let first_graph = report
        .graph_profiles
        .first()
        .ok_or_else(|| "identity graph report omitted its output segment".to_owned())?
        .segment_id;
    public_schedule.push(independent::PublicStoreStep::PublishPreparedSegment);
    record_phase(2, true, 12, &mutations, &public_schedule)?;

    let replaced = base | 600;
    ingest_identity(&store, &epoch, replaced, 2, &QUERY)?;
    mutations.push(independent::IdentityMutation::Replace(primitive_version(
        replaced, 2,
    )));
    public_schedule.push(independent::PublicStoreStep::IngestAccepted);
    for document in [base | 50, base | 1300, base | 1400] {
        ingest_identity(&store, &epoch, document, 1, &QUERY)?;
        mutations.push(independent::IdentityMutation::Ingest(primitive_version(
            document, 1,
        )));
        public_schedule.push(independent::PublicStoreStep::IngestAccepted);
    }
    let deleted = base | 700;
    store
        .delete(DeleteBatch::new(vec![DocId::new(deleted)]))
        .map_err(|error| format!("delete identity document: {error}"))?;
    mutations.push(independent::IdentityMutation::Delete {
        doc_id_be: deleted.to_be_bytes(),
        revision: 1,
    });
    public_schedule.push(independent::PublicStoreStep::DeleteAccepted);
    let shadowed_sources = published_segment_order(&store)?;
    if shadowed_sources.len() != 1 {
        return Err(format!(
            "identity shadow transition retained {} sealed sources, expected one",
            shadowed_sources.len()
        ));
    }
    let first_shadowed = *shadowed_sources
        .first()
        .ok_or_else(|| "identity shadow transition omitted its sealed source".to_owned())?;
    replace_identity_source(&mut mutations, first_graph, first_shadowed)?;
    record_phase(3, false, 14, &mutations, &public_schedule)?;
    let second = seal_identity(&store)?;
    mutations.push(independent::IdentityMutation::Seal(*second.as_bytes()));
    public_schedule.push(independent::PublicStoreStep::Seal);
    let second_promotion_sources = published_segment_order(&store)?;
    let report = store.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: Duration::from_secs(30),
            bytes: u64::MAX,
        },
        TierThresholds { graph_min_rows: 4 },
    );
    if report.graphs_built != 2 || !matches!(report.status, MaintenanceStatus::Complete) {
        return Err(format!(
            "identity second graph maintenance failed: built={} status={:?}",
            report.graphs_built, report.status
        ));
    }
    public_schedule.extend([
        independent::PublicStoreStep::PublishPreparedSegment,
        independent::PublicStoreStep::PublishPreparedSegment,
    ]);
    if second_promotion_sources.len() != report.graph_profiles.len() {
        return Err(format!(
            "identity second promotion scheduled {} sources but reported {} outputs",
            second_promotion_sources.len(),
            report.graph_profiles.len()
        ));
    }
    for (source, profile) in second_promotion_sources.iter().zip(&report.graph_profiles) {
        replace_identity_source(&mut mutations, *source, profile.segment_id)?;
    }
    for document in [base | 1500, base | 1600] {
        ingest_identity(&store, &epoch, document, 1, &QUERY)?;
        mutations.push(independent::IdentityMutation::Ingest(primitive_version(
            document, 1,
        )));
        public_schedule.push(independent::PublicStoreStep::IngestAccepted);
    }
    store
        .close()
        .map_err(|error| format!("close identity mutation Store: {error}"))?;
    mutations.push(independent::IdentityMutation::Reopen);
    public_schedule.push(independent::PublicStoreStep::Reopen);

    let reopened = Store::open(
        directory.path(),
        OpenOptions::default().with_epoch(epoch.clone()),
    )
    .map_err(|error| format!("reopen identity Store: {error}"))?;
    let clean = identity_queries(&reopened, seed, true, 16)?;
    let control = identity_queries(&reopened, seed, true, 16)?;
    let retry = identity_queries(&reopened, seed, true, 16)?;
    phases.push(IdentityPhase {
        observation_phase: 4,
        mutations: mutations.clone(),
        public_schedule: public_schedule.clone(),
        clean: clean.clone(),
        control: control.clone(),
        retry,
    });
    reopened
        .close()
        .map_err(|error| format!("close identity control Store: {error}"))?;
    let frozen = FrozenStoreFixture::capture(directory.path())?;
    Ok(IdentityFixture {
        directory,
        frozen,
        epoch,
        phases,
        mutations,
        public_schedule,
        phase: 4,
        clean,
        control,
    })
}

fn tier_outcome(values: &[TierOutcome], tag: u8) -> Result<&TierOutcome, String> {
    values
        .iter()
        .find(|outcome| outcome.tag == tag)
        .ok_or_else(|| format!("identity tier tag {tag} was not observed"))
}

struct IdentityPairContext<'a> {
    seed: u64,
    mutations: &'a [independent::IdentityMutation],
    public_schedule: &'a [independent::PublicStoreStep],
    phase: u8,
    fault_status: independent::PrimitiveStatus,
}

fn identity_pair(
    context: &IdentityPairContext<'_>,
    clean: &TierOutcome,
    control: &TierOutcome,
    retry: &TierOutcome,
) -> Result<I27EvidencePair, String> {
    let case_id = context.seed.rotate_left(8)
        ^ 0x2700
        ^ u64::from(clean.tag)
        ^ (u64::from(context.phase) << 24);
    let input = independent::IdentityInput {
        case_id,
        mutations: context.mutations.to_vec(),
        query: QUERY
            .iter()
            .copied()
            .map(independent::F32::from_float)
            .collect(),
        k: clean.outcome.candidates.len() as u64,
        tier: clean.tag,
        observation_phase: context.phase,
        public_schedule: context.public_schedule.to_vec(),
    };
    Ok(I27EvidencePair {
        observed: independent::I27Observed {
            case_id,
            rows: candidate_rows(&clean.outcome)?,
            generation: clean.outcome.generation,
            phase: context.phase,
            tier: clean.tag,
            control_rows: candidate_rows(&control.outcome)?,
            control_generation: control.outcome.generation,
            retry_rows: candidate_rows(&retry.outcome)?,
            retry_generation: retry.outcome.generation,
            fault_status: context.fault_status.clone(),
            retry_status: independent::PrimitiveStatus::Ok,
        },
        input,
    })
}

struct RowCancellationFixtureEvidence {
    clean: VectorStoreResultFact,
    fault: VectorStoreResultFact,
    retry: VectorStoreResultFact,
    receipts: Vec<VectorFaultReceipt>,
    identity_retry_tiers: Option<Vec<TierOutcome>>,
    mutation: VectorMutationEvidence,
    clean_initial_directory: VectorFixtureDirectoryEvidence,
    fault_initial_directory: VectorFixtureDirectoryEvidence,
}

fn run_row_cancellation_fixture(
    seed: u64,
    identity: &IdentityFixture,
) -> Result<RowCancellationFixtureEvidence, String> {
    let requested_rows = 1 + (seed % 2) as u32;
    match (seed / 6) % 4 {
        0 => {
            let pair = isolated_pair(&identity.frozen)?;
            let clean_store = Store::open(
                pair.clean.path(),
                OpenOptions::default().with_epoch(identity.epoch.clone()),
            )
            .map_err(|error| format!("open active cancellation control Store: {error}"))?;
            let clean_tiers = identity_queries(&clean_store, seed, true, 16)?;
            let clean = result_fact(Ok(tier_outcome(&clean_tiers, 1)?.outcome.clone()))?;
            clean_store
                .close()
                .map_err(|error| format!("close active cancellation control Store: {error}"))?;
            let case_id = seed.rotate_left(16) ^ 0x27_01;
            let controller = VectorFaultController::armed(
                VectorFault::CancelAfterRows {
                    source: VectorRowSource::Active,
                    requested_rows,
                    tier: VectorSearchTier::Exact,
                },
                case_id,
            );
            let store = Store::open_with_test_dependencies(
                pair.fault.path(),
                OpenOptions::default().with_epoch(identity.epoch.clone()),
                vector_dependencies(controller.clone()),
            )
            .map_err(|error| format!("open active cancellation Store: {error}"))?;
            let generation = store
                .snapshot()
                .map_err(|error| format!("snapshot active cancellation Store: {error}"))?
                .generation();
            let fault =
                fact_with_generation(search(&store, SearchTier::Exact, ROWS.len()), generation)?;
            let receipts = controller.take_typed_receipts();
            let retry_tiers = identity_queries(&store, seed, true, 16)?;
            if !controller.take_typed_receipts().is_empty() {
                return Err("active cancellation controller fired twice".to_owned());
            }
            let retry = result_fact(Ok(tier_outcome(&retry_tiers, 1)?.outcome.clone()))?;
            store
                .close()
                .map_err(|error| format!("close active cancellation Store: {error}"))?;
            Ok(RowCancellationFixtureEvidence {
                clean,
                fault,
                retry,
                receipts,
                identity_retry_tiers: Some(retry_tiers),
                mutation: VectorMutationEvidence::RowCancellation {
                    case_id,
                    source: independent::PrimitiveSource::Active,
                    tier: 1,
                    requested_rows,
                },
                clean_initial_directory: pair.clean_initial,
                fault_initial_directory: pair.fault_initial,
            })
        }
        1 => {
            let fixture = sealed_bit4_fixture(seed)?;
            let pair = isolated_pair(&fixture.frozen)?;
            let clean_store = Store::open(pair.clean.path(), OpenOptions::default())
                .map_err(|error| format!("open sealed Bit4 cancellation control Store: {error}"))?;
            let clean = result_fact(search(&clean_store, SearchTier::Scan, ROWS.len()))?;
            clean_store.close().map_err(|error| {
                format!("close sealed Bit4 cancellation control Store: {error}")
            })?;
            let case_id = seed.rotate_left(16) ^ 0x27_11;
            let source = independent::PrimitiveSource::Sealed(*fixture.segment.as_bytes());
            let controller = VectorFaultController::armed(
                VectorFault::CancelAfterRows {
                    source: VectorRowSource::Sealed(*fixture.segment.as_bytes()),
                    requested_rows,
                    tier: VectorSearchTier::Scan,
                },
                case_id,
            );
            let store = Store::open_with_test_dependencies(
                pair.fault.path(),
                OpenOptions::default(),
                vector_dependencies(controller.clone()),
            )
            .map_err(|error| format!("open sealed Bit4 cancellation Store: {error}"))?;
            let generation = store
                .snapshot()
                .map_err(|error| format!("snapshot sealed Bit4 cancellation Store: {error}"))?
                .generation();
            let fault =
                fact_with_generation(search(&store, SearchTier::Scan, ROWS.len()), generation)?;
            let receipts = controller.take_typed_receipts();
            let retry = result_fact(search(&store, SearchTier::Scan, ROWS.len()))?;
            if !controller.take_typed_receipts().is_empty() {
                return Err("sealed Bit4 cancellation controller fired twice".to_owned());
            }
            store
                .close()
                .map_err(|error| format!("close sealed Bit4 cancellation Store: {error}"))?;
            Ok(RowCancellationFixtureEvidence {
                clean,
                fault,
                retry,
                receipts,
                identity_retry_tiers: None,
                mutation: VectorMutationEvidence::RowCancellation {
                    case_id,
                    source,
                    tier: 2,
                    requested_rows,
                },
                clean_initial_directory: pair.clean_initial,
                fault_initial_directory: pair.fault_initial,
            })
        }
        2 => {
            let fixture = sealed_int8_fixture(seed)?;
            let pair = isolated_pair(&fixture.frozen)?;
            let clean_store = Store::open(pair.clean.path(), OpenOptions::default())
                .map_err(|error| format!("open sealed Int8 cancellation control Store: {error}"))?;
            let clean = result_fact(search(&clean_store, SearchTier::Scan, ROWS.len()))?;
            clean_store.close().map_err(|error| {
                format!("close sealed Int8 cancellation control Store: {error}")
            })?;
            let case_id = seed.rotate_left(16) ^ 0x27_12;
            let source = independent::PrimitiveSource::Sealed(*fixture.segment.as_bytes());
            let controller = VectorFaultController::armed(
                VectorFault::CancelAfterRows {
                    source: VectorRowSource::Sealed(*fixture.segment.as_bytes()),
                    requested_rows,
                    tier: VectorSearchTier::Scan,
                },
                case_id,
            );
            let store = Store::open_with_test_dependencies(
                pair.fault.path(),
                OpenOptions::default(),
                vector_dependencies(controller.clone()),
            )
            .map_err(|error| format!("open sealed Int8 cancellation Store: {error}"))?;
            let generation = store
                .snapshot()
                .map_err(|error| format!("snapshot sealed Int8 cancellation Store: {error}"))?
                .generation();
            let fault =
                fact_with_generation(search(&store, SearchTier::Scan, ROWS.len()), generation)?;
            let receipts = controller.take_typed_receipts();
            let retry = result_fact(search(&store, SearchTier::Scan, ROWS.len()))?;
            if !controller.take_typed_receipts().is_empty() {
                return Err("sealed Int8 cancellation controller fired twice".to_owned());
            }
            store
                .close()
                .map_err(|error| format!("close sealed Int8 cancellation Store: {error}"))?;
            Ok(RowCancellationFixtureEvidence {
                clean,
                fault,
                retry,
                receipts,
                identity_retry_tiers: None,
                mutation: VectorMutationEvidence::RowCancellation {
                    case_id,
                    source,
                    tier: 2,
                    requested_rows,
                },
                clean_initial_directory: pair.clean_initial,
                fault_initial_directory: pair.fault_initial,
            })
        }
        _ => {
            let fixture = graph_fixture(seed)?;
            let pair = isolated_pair(&fixture.frozen)?;
            let clean_store = Store::open(
                pair.clean.path(),
                OpenOptions::default().with_epoch(fixture.epoch.clone()),
            )
            .map_err(|error| format!("open graph cancellation control Store: {error}"))?;
            let clean = result_fact(graph_search(&clean_store, &fixture.query, seed, ROWS.len()))?;
            clean_store
                .close()
                .map_err(|error| format!("close graph cancellation control Store: {error}"))?;
            let case_id = seed.rotate_left(16) ^ 0x27_13;
            let source = independent::PrimitiveSource::Sealed(*fixture.segment.as_bytes());
            let controller = VectorFaultController::armed(
                VectorFault::CancelAfterRows {
                    source: VectorRowSource::Sealed(*fixture.segment.as_bytes()),
                    requested_rows,
                    tier: VectorSearchTier::Graph,
                },
                case_id,
            );
            let store = Store::open_with_test_dependencies(
                pair.fault.path(),
                OpenOptions::default().with_epoch(fixture.epoch.clone()),
                vector_dependencies(controller.clone()),
            )
            .map_err(|error| format!("open graph cancellation Store: {error}"))?;
            let generation = store
                .snapshot()
                .map_err(|error| format!("snapshot graph cancellation Store: {error}"))?
                .generation();
            let fault = fact_with_generation(
                graph_search(&store, &fixture.query, seed, ROWS.len()),
                generation,
            )?;
            let receipts = controller.take_typed_receipts();
            let retry = result_fact(graph_search(&store, &fixture.query, seed, ROWS.len()))?;
            if !controller.take_typed_receipts().is_empty() {
                return Err("graph cancellation controller fired twice".to_owned());
            }
            store
                .close()
                .map_err(|error| format!("close graph cancellation Store: {error}"))?;
            Ok(RowCancellationFixtureEvidence {
                clean,
                fault,
                retry,
                receipts,
                identity_retry_tiers: None,
                mutation: VectorMutationEvidence::RowCancellation {
                    case_id,
                    source,
                    tier: 3,
                    requested_rows,
                },
                clean_initial_directory: pair.clean_initial,
                fault_initial_directory: pair.fault_initial,
            })
        }
    }
}

fn run_row_identity(
    seed: u64,
    fault: Option<VectorFaultKind>,
) -> Result<VectorOperationEvidence, String> {
    let fixture = build_identity_fixture(seed)?;
    let mut receipts = Vec::new();
    let mut fault_status = independent::PrimitiveStatus::Ok;
    let mut mutation = VectorMutationEvidence::None;
    let (
        operation_clean,
        fault_fact,
        operation_retry,
        retry_tiers,
        clean_initial_directory,
        fault_initial_directory,
    ) = match fault {
        Some(VectorFaultKind::RowCountCancellation) => {
            let cancellation = run_row_cancellation_fixture(seed, &fixture)?;
            fault_status = cancellation.fault.status.clone();
            receipts = cancellation.receipts;
            mutation = cancellation.mutation;
            let retry_tiers = match cancellation.identity_retry_tiers {
                Some(retry_tiers) => retry_tiers,
                None => {
                    let store = Store::open(
                        fixture.directory.path(),
                        OpenOptions::default().with_epoch(fixture.epoch.clone()),
                    )
                    .map_err(|error| format!("open identity retry after cancellation: {error}"))?;
                    identity_queries(&store, seed, true, 16)?
                }
            };
            (
                cancellation.clean,
                cancellation.fault,
                cancellation.retry,
                retry_tiers,
                cancellation.clean_initial_directory,
                cancellation.fault_initial_directory,
            )
        }
        Some(VectorFaultKind::AllocationDenial) => {
            let pair = isolated_pair(&fixture.frozen)?;
            let clean_store = Store::open(
                pair.clean.path(),
                OpenOptions::default().with_epoch(fixture.epoch.clone()),
            )
            .map_err(|error| format!("open identity allocation control Store: {error}"))?;
            let clean_tiers = identity_queries(&clean_store, seed, true, 16)?;
            let clean = result_fact(Ok(tier_outcome(&clean_tiers, 1)?.outcome.clone()))?;
            clean_store
                .close()
                .map_err(|error| format!("close identity allocation control Store: {error}"))?;
            let items = 2_u64;
            let bytes = items
                .checked_mul(
                    u64::try_from(std::mem::size_of::<SearchCandidate>())
                        .map_err(|_| "SearchCandidate width exceeds u64".to_owned())?,
                )
                .ok_or_else(|| "identity allocation byte count overflow".to_owned())?;
            let case_id = seed.rotate_left(16) ^ 0x27_02;
            let controller = VectorFaultController::armed(
                VectorFault::DenyGlobalCandidateAllocation { items, bytes },
                case_id,
            );
            mutation = VectorMutationEvidence::AllocationDenial {
                case_id,
                component: "vector search global candidates",
                items,
                bytes,
            };
            let store = Store::open_with_test_dependencies(
                pair.fault.path(),
                OpenOptions::default().with_epoch(fixture.epoch.clone()),
                vector_dependencies(controller.clone()),
            )
            .map_err(|error| format!("open identity allocation Store: {error}"))?;
            let generation = store
                .snapshot()
                .map_err(|error| format!("snapshot identity allocation Store: {error}"))?
                .generation();
            let fault =
                fact_with_generation(search(&store, SearchTier::Exact, ROWS.len()), generation)?;
            fault_status = fault.status.clone();
            receipts = controller.take_typed_receipts();
            let retry = identity_queries(&store, seed, true, 16)?;
            if !controller.take_typed_receipts().is_empty() {
                return Err("identity allocation controller fired twice".to_owned());
            }
            let retry_exact = tier_outcome(&retry, 1)?;
            store
                .close()
                .map_err(|error| format!("close identity allocation Store: {error}"))?;
            (
                clean,
                fault,
                result_fact(Ok(retry_exact.outcome.clone()))?,
                retry,
                pair.clean_initial,
                pair.fault_initial,
            )
        }
        None => {
            let pair = isolated_pair(&fixture.frozen)?;
            let clean_store = Store::open(
                pair.clean.path(),
                OpenOptions::default().with_epoch(fixture.epoch.clone()),
            )
            .map_err(|error| format!("open identity clean Store: {error}"))?;
            let clean_tiers = identity_queries(&clean_store, seed, true, 16)?;
            let clean_exact = tier_outcome(&clean_tiers, 1)?;
            let clean_fact = result_fact(Ok(clean_exact.outcome.clone()))?;
            clean_store
                .close()
                .map_err(|error| format!("close identity clean Store: {error}"))?;
            let store = Store::open(
                pair.fault.path(),
                OpenOptions::default().with_epoch(fixture.epoch.clone()),
            )
            .map_err(|error| format!("open identity no-fault Store: {error}"))?;
            let retry = identity_queries(&store, seed, true, 16)?;
            let retry_exact = tier_outcome(&retry, 1)?;
            store
                .close()
                .map_err(|error| format!("close identity no-fault Store: {error}"))?;
            (
                clean_fact.clone(),
                clean_fact,
                result_fact(Ok(retry_exact.outcome.clone()))?,
                retry,
                pair.clean_initial,
                pair.fault_initial,
            )
        }
        Some(other) => {
            return Err(format!(
                "row-identity received unrelated vector fault {other:?}"
            ));
        }
    };

    let fault_tier = match mutation {
        VectorMutationEvidence::RowCancellation { tier, .. } => Some(tier),
        VectorMutationEvidence::AllocationDenial { .. } => Some(1),
        _ => None,
    };
    let mut pairs = Vec::new();
    for phase in &fixture.phases {
        for clean in &phase.clean {
            let tag = clean.tag;
            let control = tier_outcome(&phase.control, tag)?;
            let retry = if phase.observation_phase == fixture.phase {
                tier_outcome(&retry_tiers, tag)?
            } else {
                tier_outcome(&phase.retry, tag)?
            };
            if clean.tier != control.tier || clean.tier != retry.tier {
                return Err(format!(
                    "identity phase {} tier tag {tag} changed product tier",
                    phase.observation_phase
                ));
            }
            let status = if phase.observation_phase == fixture.phase && fault_tier == Some(tag) {
                fault_status.clone()
            } else {
                independent::PrimitiveStatus::Ok
            };
            let context = IdentityPairContext {
                seed,
                mutations: &phase.mutations,
                public_schedule: &phase.public_schedule,
                phase: phase.observation_phase,
                fault_status: status,
            };
            pairs.push(identity_pair(&context, clean, control, retry)?);
        }
    }
    let invariant = VectorInvariantEvidence::I27(pairs);
    let extra_tiers = vec![0, 1, 2, 3];
    let fixture_evidence = primitive_fixture(
        VectorOperationKind::RowIdentity,
        seed,
        &invariant,
        &extra_tiers,
        &fact_sources(&operation_clean),
        fixture.public_schedule.clone(),
    );
    Ok(VectorOperationEvidence {
        operation: VectorOperationKind::RowIdentity,
        fault,
        fixture: fixture_evidence,
        mutation,
        invariant,
        receipts,
        forced_child: None,
        generic_fault: None,
        control: VectorControlEvidence {
            namespace: "vector-execution/row-identity",
            operation: VectorOperationKind::RowIdentity,
            seed,
            clean: operation_clean,
            fault: fault_fact,
            retry: operation_retry,
            clean_initial_directory,
            fault_initial_directory,
            isolated_directories: true,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_expected_comparison_counts_are_family_owned_and_fault_exact() {
        let clean = expected_comparison_counts().expect("vector comparison-count contract");
        let available_backends = KernelVariant::available().map(|_| 1_u64).sum::<u64>();
        assert_eq!(clean["I24"], available_backends * 15 * 17 + 1);
        assert_eq!(clean["I25"], 69);
        assert_eq!(clean["I26"], 12);
        assert_eq!(clean["I27"], 17);

        for (operation, invariant) in [
            (VectorOperationKind::KernelParity, "I24"),
            (VectorOperationKind::Quantization, "I25"),
            (VectorOperationKind::Rescore, "I26"),
            (VectorOperationKind::RowIdentity, "I27"),
        ] {
            assert_eq!(
                expected_comparison_count(operation, None)
                    .expect("clean operation comparison count"),
                (invariant, clean[invariant])
            );
        }
        assert_eq!(
            expected_comparison_count(
                VectorOperationKind::KernelParity,
                Some(VectorFaultKind::ForcedDispatchBackend),
            )
            .expect("forced-dispatch comparison count"),
            ("I24", clean["I24"] + 1)
        );
        assert!(
            expected_comparison_count(
                VectorOperationKind::Quantization,
                Some(VectorFaultKind::ForcedDispatchBackend),
            )
            .is_err(),
            "operation/fault count contract accepted an invalid pairing"
        );
    }

    #[test]
    #[ignore = "self-spawned forced dispatch requires a fresh process"]
    fn forced_backend_store_child() {
        forced_backend_child_from_env().expect("forced backend child evidence");
    }

    #[test]
    fn vector_adapter_clean_operations_return_owned_dto_pairs() {
        for operation in [
            VectorOperationKind::KernelParity,
            VectorOperationKind::Quantization,
            VectorOperationKind::Rescore,
            VectorOperationKind::RowIdentity,
        ] {
            let evidence = run_vector_operation(operation, 7, None)
                .unwrap_or_else(|error| panic!("{operation:?}: {error}"));
            assert_eq!(evidence.operation, operation);
            assert_eq!(evidence.operation.key(), operation.key());
            assert_eq!(evidence.fault, None);
            assert!(matches!(
                (&evidence.invariant, operation),
                (
                    VectorInvariantEvidence::I24(_),
                    VectorOperationKind::KernelParity
                ) | (
                    VectorInvariantEvidence::I25(_),
                    VectorOperationKind::Quantization
                ) | (
                    VectorInvariantEvidence::I26(_),
                    VectorOperationKind::Rescore
                ) | (
                    VectorInvariantEvidence::I27(_),
                    VectorOperationKind::RowIdentity
                )
            ));
            let (invariant, actual_count) = match &evidence.invariant {
                VectorInvariantEvidence::I24(pairs) => ("I24", pairs.len()),
                VectorInvariantEvidence::I25(pairs) => ("I25", pairs.len()),
                VectorInvariantEvidence::I26(pairs) => ("I26", pairs.len()),
                VectorInvariantEvidence::I27(pairs) => ("I27", pairs.len()),
            };
            let expected = expected_comparison_count(operation, None)
                .expect("clean family-owned comparison count");
            assert_eq!(expected.0, invariant);
            assert_eq!(
                u64::try_from(actual_count).expect("comparison count fits u64"),
                expected.1,
                "{invariant} family corpus drifted from its exact count API"
            );
            assert!(evidence.receipts.is_empty());
            assert!(evidence.forced_child.is_none());
            assert_eq!(
                evidence.control.clean.status,
                independent::PrimitiveStatus::Ok
            );
            assert_eq!(
                evidence.control.retry.status,
                independent::PrimitiveStatus::Ok
            );
        }
    }

    #[test]
    fn vector_adapter_returns_family_owned_canonical_attestations() {
        for operation in [
            VectorOperationKind::KernelParity,
            VectorOperationKind::Quantization,
            VectorOperationKind::Rescore,
            VectorOperationKind::RowIdentity,
        ] {
            let evidence = run_vector_operation(operation, 7, None)
                .unwrap_or_else(|error| panic!("{operation:?}: {error}"));
            let attestations = evidence
                .canonical_attestations()
                .unwrap_or_else(|error| panic!("{operation:?} canonical evidence: {error}"));
            assert!(!attestations.is_empty());
            assert!(attestations.iter().all(|attestation| {
                attestation.checker_id.starts_with('I')
                    && attestation.input.version == independent::VECTOR_CANONICAL_VERSION
                    && attestation.observed.version == independent::VECTOR_CANONICAL_VERSION
                    && attestation.first_difference.is_none()
            }));
        }
    }

    #[test]
    fn vector_retained_canonical_bytes_replay_without_seed_derivation() {
        for operation in [
            VectorOperationKind::KernelParity,
            VectorOperationKind::Quantization,
            VectorOperationKind::Rescore,
            VectorOperationKind::RowIdentity,
        ] {
            let evidence = run_vector_operation(operation, 7, None)
                .unwrap_or_else(|error| panic!("{operation:?}: {error}"));
            for attestation in evidence
                .canonical_attestations()
                .unwrap_or_else(|error| panic!("{operation:?} canonical evidence: {error}"))
            {
                let replay = independent::replay_canonical_comparison(
                    &attestation.input.bytes,
                    &attestation.observed.bytes,
                )
                .unwrap_or_else(|error| panic!("{operation:?} retained replay: {error}"));
                assert_eq!(replay.checker_id, attestation.checker_id);
                assert_eq!(replay.case_id, attestation.case_id);
                assert_eq!(replay.input_sha256, attestation.input.sha256);
                assert_eq!(replay.observed_sha256, attestation.observed.sha256);
                assert!(replay.first_difference().is_none());
            }
        }
    }

    #[test]
    fn vector_retained_fixture_codec_executes_literal_bytes_without_seed_derivation() {
        for operation in [
            VectorOperationKind::KernelParity,
            VectorOperationKind::Quantization,
            VectorOperationKind::Rescore,
            VectorOperationKind::RowIdentity,
        ] {
            let evidence = run_vector_operation(operation, 7, None)
                .unwrap_or_else(|error| panic!("{operation:?}: {error}"));
            let expected_attestations = evidence
                .canonical_attestations()
                .unwrap_or_else(|error| panic!("{operation:?} canonical evidence: {error}"));
            let fixture_bytes = encode_vector_fixture(&evidence)
                .unwrap_or_else(|error| panic!("{operation:?} fixture encoding: {error}"));
            let decoded = decode_vector_fixture(&fixture_bytes)
                .unwrap_or_else(|error| panic!("{operation:?} fixture decoding: {error}"));
            assert_eq!(decoded.operation, operation);
            assert_eq!(decoded.seed, 7);
            assert_eq!(decoded.fault, None);
            assert_eq!(decoded.mutation, VectorMutationEvidence::None);
            assert_eq!(decoded.context, VectorExecutionContext::default());
            assert_eq!(decoded.public_schedule, evidence.fixture.public_schedule);
            assert_eq!(decoded.runtime_features, evidence.fixture.runtime_features);
            assert_eq!(decoded.comparisons.len(), expected_attestations.len());
            for (retained, expected) in decoded.comparisons.iter().zip(&expected_attestations) {
                assert_eq!(retained.input_bytes, expected.input.bytes);
                assert_eq!(retained.observed_bytes, expected.observed.bytes);
            }
            let executed = run_vector_operation_from_fixture(&fixture_bytes)
                .unwrap_or_else(|error| panic!("{operation:?} literal fixture execution: {error}"));
            assert!(
                executed
                    .comparisons
                    .iter()
                    .all(|comparison| comparison.first_difference().is_none())
            );

            let mut changed_seed = fixture_bytes.clone();
            let payload_length = changed_seed.len() - 32;
            let seed_offset = VECTOR_FIXTURE_MAGIC.len() + 1;
            changed_seed[seed_offset..seed_offset + 8].copy_from_slice(&u64::MAX.to_le_bytes());
            let digest = independent::canonical_sha256(&changed_seed[..payload_length]);
            changed_seed[payload_length..].copy_from_slice(&digest);
            let changed = run_vector_operation_from_fixture(&changed_seed)
                .expect("retained fixture execution must not derive inputs from seed metadata");
            assert_eq!(changed.fixture.seed, u64::MAX);
            assert_eq!(changed.comparisons, executed.comparisons);

            let mut corrupted = fixture_bytes.clone();
            corrupted[VECTOR_FIXTURE_MAGIC.len()] ^= 0xff;
            assert!(
                decode_vector_fixture(&corrupted)
                    .expect_err("fixture payload corruption must fail closed")
                    .contains("SHA-256 mismatch")
            );
            let mut trailing = fixture_bytes.clone();
            trailing.push(0);
            assert!(decode_vector_fixture(&trailing).is_err());
        }

        let context = VectorExecutionContext {
            program_op_index: 91,
            generic_fault: Some(VectorGenericFaultSchedule {
                id: "retained-generic-latency".to_owned(),
                site: VectorGenericFaultSite::Read,
                mode: VectorGenericFaultMode::Latency,
                nth_match: 1,
                path_contains: Some("manifest.ze".to_owned()),
            }),
        };
        let evidence = run_vector_operation_with_context(
            VectorOperationKind::Quantization,
            26,
            Some(VectorFaultKind::CorruptCodesFactors),
            context.clone(),
        )
        .expect("generic paired fixture evidence");
        let bytes = encode_vector_fixture(&evidence).expect("encode generic paired fixture");
        let decoded = decode_vector_fixture(&bytes).expect("decode generic paired fixture");
        assert_eq!(decoded.context, context);
    }

    #[test]
    fn vector_retained_fixture_round_trips_every_typed_fault_choice() {
        for (operation, fault, seed) in [
            (
                VectorOperationKind::KernelParity,
                VectorFaultKind::ForcedDispatchBackend,
                0,
            ),
            (
                VectorOperationKind::Quantization,
                VectorFaultKind::CorruptCodesFactors,
                0,
            ),
            (
                VectorOperationKind::Rescore,
                VectorFaultKind::MissingRescoreRows,
                0,
            ),
            (
                VectorOperationKind::RowIdentity,
                VectorFaultKind::RowCountCancellation,
                0,
            ),
            (
                VectorOperationKind::RowIdentity,
                VectorFaultKind::AllocationDenial,
                0,
            ),
        ] {
            let evidence = run_vector_operation(operation, seed, Some(fault))
                .unwrap_or_else(|error| panic!("{operation:?}/{fault:?}: {error}"));
            let fixture_bytes = encode_vector_fixture(&evidence)
                .unwrap_or_else(|error| panic!("{operation:?}/{fault:?} encoding: {error}"));
            let decoded = decode_vector_fixture(&fixture_bytes)
                .unwrap_or_else(|error| panic!("{operation:?}/{fault:?} decoding: {error}"));
            assert_eq!(decoded.fault, Some(fault));
            assert_eq!(decoded.mutation, evidence.mutation);
            let replay = run_vector_operation_from_fixture(&fixture_bytes)
                .unwrap_or_else(|error| panic!("{operation:?}/{fault:?} replay: {error}"));
            assert!(
                replay
                    .comparisons
                    .iter()
                    .all(|comparison| comparison.first_difference().is_none()),
                "{operation:?}/{fault:?} retained a checker disagreement"
            );
        }
    }

    #[test]
    fn vector_i24_adapter_plant_fails_real_observation_and_raii_restores() {
        let planted = {
            let _guard = install_observation_plant(VectorObservationPlant::I24FirstIntegerOutput)
                .expect("install scoped I24 observation plant");
            run_vector_operation(VectorOperationKind::KernelParity, 24, None)
                .expect("I24 planted production adapter evidence")
        };
        let differences = planted
            .canonical_attestations()
            .expect("I24 planted canonical evidence")
            .into_iter()
            .filter_map(|attestation| attestation.first_difference)
            .collect::<Vec<_>>();
        assert_eq!(differences.len(), 1);
        assert_eq!(differences[0].checker_id, independent::I24_CHECKER_ID);
        assert_eq!(differences[0].path, "value");

        let restored = run_vector_operation(VectorOperationKind::KernelParity, 24, None)
            .expect("I24 adapter after RAII restoration");
        assert!(
            restored
                .canonical_attestations()
                .expect("restored I24 canonical evidence")
                .iter()
                .all(|attestation| attestation.first_difference.is_none())
        );
    }

    #[test]
    fn vector_i25_adapter_plant_fails_real_observation_and_raii_restores() {
        let planted = {
            let _guard =
                install_observation_plant(VectorObservationPlant::I25FirstNonFiniteStatusToOk)
                    .expect("install scoped I25 observation plant");
            run_vector_operation(VectorOperationKind::Quantization, 25, None)
                .expect("I25 planted production adapter evidence")
        };
        let differences = planted
            .canonical_attestations()
            .expect("I25 planted canonical evidence")
            .into_iter()
            .filter_map(|attestation| attestation.first_difference)
            .collect::<Vec<_>>();
        assert_eq!(differences.len(), 1);
        assert_eq!(differences[0].checker_id, independent::I25_CHECKER_ID);
        assert_eq!(differences[0].path, "result");

        let restored = run_vector_operation(VectorOperationKind::Quantization, 25, None)
            .expect("I25 adapter after RAII restoration");
        assert!(
            restored
                .canonical_attestations()
                .expect("restored I25 canonical evidence")
                .iter()
                .all(|attestation| attestation.first_difference.is_none())
        );
    }

    #[test]
    fn vector_i25_int8_uses_same_public_ingest_document_leg() {
        let evidence = run_vector_operation(VectorOperationKind::Quantization, 25, None)
            .expect("I25 public Int8 adapter evidence");
        let VectorInvariantEvidence::I25(pairs) = evidence.invariant else {
            panic!("quantization operation returned a non-I25 invariant");
        };
        let pair = pairs
            .iter()
            .find(|pair| {
                pair.input.scheme == independent::QuantScheme::Int8
                    && !pair.input.store.schedule.is_empty()
            })
            .expect("I25 Int8 public Store pair");
        let document = pair
            .input
            .store
            .document
            .expect("I25 Int8 public Store input document");
        assert_eq!(
            pair.input.store.schedule,
            vec![
                independent::PublicStoreStep::IngestAccepted,
                independent::PublicStoreStep::Seal,
                independent::PublicStoreStep::Reopen,
                independent::PublicStoreStep::Search,
            ]
        );
        assert!(pair.input.store.document_visible);
        assert_eq!(
            pair.observed.store.ingest_status,
            independent::PrimitiveStatus::Ok
        );
        assert_eq!(
            pair.observed.store.scan_status,
            independent::PrimitiveStatus::Ok
        );
        assert!(pair.observed.store.document_visible);
        assert!(!pair.observed.store.persisted_code_bytes.is_empty());
        assert_eq!(document.revision, 1);
    }

    #[test]
    fn vector_generic_fault_uses_distinct_scheduled_vfs_runtimes_at_program_operation() {
        let context = VectorExecutionContext {
            program_op_index: 17,
            generic_fault: Some(VectorGenericFaultSchedule {
                id: "vector-generic-append-eio".to_owned(),
                site: VectorGenericFaultSite::Append,
                mode: VectorGenericFaultMode::Eio,
                nth_match: 1,
                path_contains: Some("wal.ze".to_owned()),
            }),
        };
        let evidence = run_vector_operation_with_context(
            VectorOperationKind::Quantization,
            25,
            Some(VectorFaultKind::CorruptCodesFactors),
            context,
        )
        .expect("vector operation with generic fault schedule");
        let generic = evidence.generic_fault.expect("generic fault evidence");
        assert_eq!(generic.program_op_index, 17);
        assert!(generic.isolated_runtimes);
        assert!(generic.isolated_directories);
        assert_eq!(
            generic.clean_initial_directory,
            generic.fault_initial_directory
        );
        assert!(generic.clean.event.fired);
        assert!(generic.fault.event.fired);
        assert_eq!(generic.clean.event.path, generic.fault.event.path);
        assert_eq!(generic.clean.stage, generic.fault.stage);
        assert_eq!(generic.clean.status, generic.fault.status);
        assert!(generic.clean.feature_receipts.is_empty());
        assert!(
            generic.fault.feature_receipts.is_empty(),
            "generic ingest failure must block the later typed vector corruption site"
        );
        assert_eq!(generic.fault.stage, VectorGenericFaultStage::Ingest);
        assert!(matches!(
            generic.fault.status,
            VectorGenericFaultStatus::StoreFailure { .. }
        ));
    }

    #[test]
    fn vector_generic_fault_pair_keeps_typed_feature_fault_as_only_delta() {
        let evidence = run_vector_operation_with_context(
            VectorOperationKind::Quantization,
            26,
            Some(VectorFaultKind::CorruptCodesFactors),
            VectorExecutionContext {
                program_op_index: 18,
                generic_fault: Some(VectorGenericFaultSchedule {
                    id: "vector-generic-read-latency".to_owned(),
                    site: VectorGenericFaultSite::Read,
                    mode: VectorGenericFaultMode::Latency,
                    nth_match: 1,
                    path_contains: Some("manifest.ze".to_owned()),
                }),
            },
        )
        .expect("vector generic + typed feature pair");
        let main_mutation = evidence.mutation.clone();
        let generic = evidence.generic_fault.expect("generic pair evidence");
        assert!(generic.clean.event.fired && generic.fault.event.fired);
        assert_eq!(generic.clean.event.path, Some("manifest.ze".to_owned()));
        assert_eq!(generic.clean.event.path, generic.fault.event.path);
        assert!(generic.clean.feature_receipts.is_empty());
        assert_eq!(generic.fault.feature_receipts.len(), 1);
        assert_ne!(generic.clean.status, generic.fault.status);
        let (
            VectorMutationEvidence::QuantCorruption {
                case_id: main_case,
                scheme: main_scheme,
                tier: main_tier,
                local_row: main_row,
                field: main_field,
                ..
            },
            VectorMutationEvidence::QuantCorruption {
                case_id: paired_case,
                scheme: paired_scheme,
                tier: paired_tier,
                local_row: paired_row,
                field: paired_field,
                ..
            },
        ) = (main_mutation, generic.feature_mutation)
        else {
            panic!("quant generic pair did not retain its operation-owned mutation kind");
        };
        assert_eq!(
            (
                paired_case,
                paired_scheme,
                paired_tier,
                paired_row,
                paired_field
            ),
            (main_case, main_scheme, main_tier, main_row, main_field)
        );
    }

    #[test]
    fn vector_generic_fault_pair_fires_each_store_owned_feature_site() {
        for (operation, fault, seed) in [
            (
                VectorOperationKind::KernelParity,
                VectorFaultKind::ForcedDispatchBackend,
                0,
            ),
            (
                VectorOperationKind::Quantization,
                VectorFaultKind::CorruptCodesFactors,
                30,
            ),
            (
                VectorOperationKind::Quantization,
                VectorFaultKind::CorruptCodesFactors,
                36,
            ),
            (
                VectorOperationKind::Rescore,
                VectorFaultKind::MissingRescoreRows,
                27,
            ),
            (
                VectorOperationKind::Rescore,
                VectorFaultKind::MissingRescoreRows,
                30,
            ),
            (
                VectorOperationKind::RowIdentity,
                VectorFaultKind::RowCountCancellation,
                28,
            ),
            (
                VectorOperationKind::RowIdentity,
                VectorFaultKind::RowCountCancellation,
                30,
            ),
            (
                VectorOperationKind::RowIdentity,
                VectorFaultKind::RowCountCancellation,
                36,
            ),
            (
                VectorOperationKind::RowIdentity,
                VectorFaultKind::RowCountCancellation,
                42,
            ),
            (
                VectorOperationKind::RowIdentity,
                VectorFaultKind::AllocationDenial,
                29,
            ),
        ] {
            let evidence = run_vector_operation_with_context(
                operation,
                seed,
                Some(fault),
                VectorExecutionContext {
                    program_op_index: 19,
                    generic_fault: Some(VectorGenericFaultSchedule {
                        id: format!("vector-generic-read-latency-{seed}"),
                        site: VectorGenericFaultSite::Read,
                        mode: VectorGenericFaultMode::Latency,
                        nth_match: 1,
                        path_contains: Some("manifest.ze".to_owned()),
                    }),
                },
            )
            .unwrap_or_else(|error| panic!("{operation:?}/{fault:?}: {error}"));
            let generic = evidence.generic_fault.expect("generic pair evidence");
            assert!(generic.clean.event.fired && generic.fault.event.fired);
            assert!(generic.clean.feature_receipts.is_empty());
            assert_eq!(
                generic.fault.feature_receipts.len(),
                1,
                "{operation:?}/{fault:?} did not fire at the paired Store site"
            );
        }
    }

    #[test]
    fn vector_i26_adapter_plant_fails_real_observation_and_raii_restores() {
        let planted = {
            let _guard =
                install_observation_plant(VectorObservationPlant::I26SwapFirstEqualStoreHits)
                    .expect("install scoped I26 observation plant");
            run_vector_operation(VectorOperationKind::Rescore, 26, None)
                .expect("I26 planted production adapter evidence")
        };
        let differences = planted
            .canonical_attestations()
            .expect("I26 planted canonical evidence")
            .into_iter()
            .filter_map(|attestation| attestation.first_difference)
            .collect::<Vec<_>>();
        assert_eq!(differences.len(), 1);
        assert_eq!(differences[0].checker_id, independent::I26_CHECKER_ID);
        assert_eq!(differences[0].path, "store_hits");

        let restored = run_vector_operation(VectorOperationKind::Rescore, 26, None)
            .expect("I26 adapter after RAII restoration");
        assert!(
            restored
                .canonical_attestations()
                .expect("restored I26 canonical evidence")
                .iter()
                .all(|attestation| attestation.first_difference.is_none())
        );
    }

    #[test]
    fn vector_i27_adapter_plant_fails_real_observation_and_raii_restores() {
        let planted = {
            let _guard =
                install_observation_plant(VectorObservationPlant::I27RemapFirstPhysicalRow)
                    .expect("install scoped I27 observation plant");
            run_vector_operation(VectorOperationKind::RowIdentity, 27, None)
                .expect("I27 planted production adapter evidence")
        };
        let differences = planted
            .canonical_attestations()
            .expect("I27 planted canonical evidence")
            .into_iter()
            .filter_map(|attestation| attestation.first_difference)
            .collect::<Vec<_>>();
        assert_eq!(differences.len(), 1);
        assert_eq!(differences[0].checker_id, independent::I27_CHECKER_ID);
        assert_eq!(differences[0].path, "rows");

        let restored = run_vector_operation(VectorOperationKind::RowIdentity, 27, None)
            .expect("I27 adapter after RAII restoration");
        assert!(
            restored
                .canonical_attestations()
                .expect("restored I27 canonical evidence")
                .iter()
                .all(|attestation| attestation.first_difference.is_none())
        );
    }

    #[test]
    fn vector_quantization_fault_uses_isolated_byte_identical_fixture_directories() {
        let evidence = run_vector_operation(
            VectorOperationKind::Quantization,
            0,
            Some(VectorFaultKind::CorruptCodesFactors),
        )
        .expect("quantization fault evidence");
        assert!(
            evidence.control.isolated_directories,
            "clean and fault quantization Stores reused one directory"
        );
        assert!(
            !evidence.control.clean_initial_directory.files.is_empty(),
            "quantization control omitted its immutable Store bytes"
        );
        assert_eq!(
            evidence.control.clean_initial_directory, evidence.control.fault_initial_directory,
            "quantization clean/fault Stores were not byte-identical before opening"
        );
    }

    #[test]
    fn vector_rescore_fault_uses_isolated_byte_identical_fixture_directories() {
        for seed in [0, 6] {
            let evidence = run_vector_operation(
                VectorOperationKind::Rescore,
                seed,
                Some(VectorFaultKind::MissingRescoreRows),
            )
            .expect("rescore fault evidence");
            assert!(
                evidence.control.isolated_directories,
                "seed {seed} clean and fault rescore Stores reused one directory"
            );
            assert!(
                !evidence.control.clean_initial_directory.files.is_empty(),
                "seed {seed} rescore control omitted its immutable Store bytes"
            );
            assert_eq!(
                evidence.control.clean_initial_directory, evidence.control.fault_initial_directory,
                "seed {seed} rescore clean/fault Stores were not byte-identical before opening"
            );
        }
    }

    #[test]
    fn vector_row_identity_faults_use_isolated_byte_identical_fixture_directories() {
        for (seed, fault) in [
            (0, VectorFaultKind::RowCountCancellation),
            (6, VectorFaultKind::RowCountCancellation),
            (12, VectorFaultKind::RowCountCancellation),
            (18, VectorFaultKind::RowCountCancellation),
            (0, VectorFaultKind::AllocationDenial),
        ] {
            let evidence =
                run_vector_operation(VectorOperationKind::RowIdentity, seed, Some(fault))
                    .expect("row-identity fault evidence");
            assert!(
                evidence.control.isolated_directories,
                "seed {seed} {fault:?} clean and fault Stores reused one directory"
            );
            assert!(
                !evidence.control.clean_initial_directory.files.is_empty(),
                "seed {seed} {fault:?} omitted its immutable Store bytes"
            );
            assert_eq!(
                evidence.control.clean_initial_directory, evidence.control.fault_initial_directory,
                "seed {seed} {fault:?} clean/fault Stores were not byte-identical before opening"
            );
        }
    }

    #[test]
    fn vector_forced_backend_uses_isolated_byte_identical_fixture_directories() {
        let evidence = run_vector_operation(
            VectorOperationKind::KernelParity,
            0,
            Some(VectorFaultKind::ForcedDispatchBackend),
        )
        .expect("forced-backend evidence");
        assert!(
            evidence.control.isolated_directories,
            "default and forced-backend Stores reused one directory"
        );
        assert!(
            !evidence.control.clean_initial_directory.files.is_empty(),
            "forced-backend control omitted its immutable Store bytes"
        );
        assert_eq!(
            evidence.control.clean_initial_directory, evidence.control.fault_initial_directory,
            "default and forced-backend Stores were not byte-identical before opening"
        );
    }

    #[test]
    fn vector_i24_corpus_covers_zero_misalignment_boundaries_and_ieee_specials() {
        let evidence = run_vector_operation(VectorOperationKind::KernelParity, 7, None)
            .expect("complete I24 adapter corpus");
        let VectorInvariantEvidence::I24(pairs) = evidence.invariant else {
            panic!("kernel operation returned a non-I24 invariant");
        };
        let dimensions = pairs
            .iter()
            .filter(|pair| !pair.input.selected_for_store)
            .map(|pair| pair.input.dimension)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            dimensions,
            [
                0_u64, 1, 2, 3, 7, 15, 16, 31, 32, 33, 63, 64, 65, 127, 128, 129, 768
            ]
            .into_iter()
            .collect(),
            "I24 omitted a zero/tail/boundary dimension"
        );
        assert!(
            pairs.iter().any(|pair| pair.input.input_offset() == 1),
            "I24 never passed a deliberately offset slice to a kernel"
        );
        assert!(
            pairs
                .iter()
                .flat_map(|pair| &pair.input.f32_a)
                .any(|value| {
                    let value = value.to_float();
                    value == 0.0 && value.is_sign_negative()
                }),
            "I24 omitted negative zero"
        );
        assert!(
            pairs
                .iter()
                .flat_map(|pair| &pair.input.f32_a)
                .any(|value| value.to_float().is_subnormal()),
            "I24 omitted f32 subnormals"
        );
        assert!(
            pairs
                .iter()
                .flat_map(|pair| &pair.input.f16_a)
                .any(|bits| bits & 0x7c00 == 0x7c00),
            "I24 omitted f16 infinity/NaN encodings"
        );
        for pair in &pairs {
            let expected = independent::expected_kernel(&pair.input).expect("I24 expected result");
            independent::check_i24(&expected, &pair.observed)
                .unwrap_or_else(|error| panic!("I24 corpus mismatch: {error}"));
        }
    }

    #[test]
    fn vector_i24_float_corpora_are_separate_and_live() {
        let first_finite = kernel_input(
            0x2401,
            independent::BackendId::Scalar,
            independent::KernelId::DotF32,
            32,
            7,
            false,
            Some(FloatKernelCorpus::SeededFinite),
        );
        let second_finite = kernel_input(
            0x2402,
            independent::BackendId::Scalar,
            independent::KernelId::DotF32,
            32,
            8,
            false,
            Some(FloatKernelCorpus::SeededFinite),
        );
        let cancellation_input = kernel_input(
            0x2403,
            independent::BackendId::Scalar,
            independent::KernelId::DotF32,
            32,
            7,
            false,
            Some(FloatKernelCorpus::Cancellation),
        );
        let special_input = kernel_input(
            0x2404,
            independent::BackendId::Scalar,
            independent::KernelId::DotF32,
            32,
            7,
            false,
            Some(FloatKernelCorpus::Special),
        );
        let cancellation = [1.0e20_f32, 1.0, -1.0e20, -1.0].map(f32::to_bits);
        assert_eq!(
            cancellation_input.f32_a[..cancellation.len()]
                .iter()
                .map(|value| value.0)
                .collect::<Vec<_>>(),
            cancellation,
            "I24 omitted its cancellation-heavy alternating-magnitude prefix"
        );
        assert!(
            cancellation_input.f32_b[..cancellation.len()]
                .iter()
                .all(|value| value.to_float() == 1.0),
            "I24 cancellation prefix is not scored against unit multipliers"
        );
        assert!(
            first_finite
                .f32_a
                .iter()
                .chain(&first_finite.f32_b)
                .all(|value| (119..=135).contains(&((value.0 >> 23) & 0xff)))
        );
        assert_ne!(
            first_finite.f32_a[6].0, second_finite.f32_a[6].0,
            "I24 raw finite bits did not vary with the episode seed"
        );
        assert!(
            special_input
                .f32_a
                .iter()
                .any(|value| value.to_float().is_infinite())
                && special_input
                    .f32_a
                    .iter()
                    .any(|value| value.to_float().is_nan()),
            "I24 special-value corpus omitted infinity or NaN"
        );

        let finite_case = kernel_input(
            400_038,
            independent::BackendId::Scalar,
            independent::KernelId::DotF32,
            7,
            4,
            false,
            Some(FloatKernelCorpus::SeededFinite),
        );
        let scalar = KernelVariant::available()
            .find(|variant| variant.backend_id() == KernelBackendId::Scalar)
            .expect("scalar backend is always available");
        let observed = independent::I24Observed {
            case_id: finite_case.case_id,
            backend: finite_case.backend,
            kernel: finite_case.kernel,
            value: observe_kernel(scalar, &finite_case).expect("real scalar DotF32 observation"),
            selected_for_store: false,
            work_items: finite_case.work_items,
        };
        let expected = independent::expected_kernel(&finite_case)
            .expect("independent seeded finite DotF32 reference");
        independent::check_i24(&expected, &observed)
            .expect("seeded finite DotF32 corpus must not overflow f32 accumulation");
    }

    #[test]
    fn vector_i25_corpus_covers_every_special_position_boundary_and_scheme() {
        let evidence = run_vector_operation(VectorOperationKind::Quantization, 7, None)
            .expect("complete I25 adapter corpus");
        let VectorInvariantEvidence::I25(pairs) = evidence.invariant else {
            panic!("quantization operation returned a non-I25 invariant");
        };
        for scheme in [
            independent::QuantScheme::Bit4,
            independent::QuantScheme::Int8,
        ] {
            let scheme_pairs = pairs
                .iter()
                .filter(|pair| pair.input.scheme == scheme)
                .collect::<Vec<_>>();
            assert!(
                scheme_pairs.iter().any(|pair| pair.input.row.is_empty()),
                "{scheme:?} omitted empty input"
            );
            assert!(
                scheme_pairs
                    .iter()
                    .any(|pair| pair.input.row.len() == 65_537),
                "{scheme:?} omitted the maximum+1 boundary"
            );
            assert!(
                scheme_pairs.iter().any(|pair| {
                    pair.input.row.len() >= 3
                        && pair.input.row.len() % 2 == 1
                        && pair.observed.result.status == independent::PrimitiveStatus::Ok
                }),
                "{scheme:?} omitted a finite odd-dimension success cell"
            );
            assert!(
                scheme_pairs.iter().any(|pair| {
                    !pair.input.row.is_empty()
                        && pair.input.row.len() % 2 == 0
                        && pair.observed.result.status == independent::PrimitiveStatus::Ok
                }),
                "{scheme:?} omitted a finite even-dimension success cell"
            );
            assert!(
                scheme_pairs.iter().any(|pair| {
                    pair.input.row.len() > 1
                        && pair
                            .input
                            .row
                            .windows(2)
                            .all(|window| window[0] == window[1])
                        && pair.observed.result.status == independent::PrimitiveStatus::Ok
                }),
                "{scheme:?} omitted a constant-row success cell"
            );
            assert!(
                scheme_pairs.iter().any(|pair| {
                    pair.input
                        .row
                        .iter()
                        .any(|value| value.to_float().is_subnormal())
                        && pair.observed.result.status == independent::PrimitiveStatus::Ok
                }),
                "{scheme:?} omitted a subnormal-row success cell"
            );
            assert!(
                scheme_pairs.iter().any(|pair| {
                    pair.input
                        .row
                        .iter()
                        .any(|value| value.to_float().abs() == f32::MAX)
                        && pair.observed.result.status == independent::PrimitiveStatus::Ok
                }),
                "{scheme:?} omitted an extreme-finite success cell"
            );
            assert!(
                scheme_pairs.iter().any(|pair| {
                    pair.input.row.iter().any(|value| {
                        matches!(value.to_float().to_bits(), 0x3f00_0000 | 0xbf00_0000)
                    }) && pair.observed.result.status == independent::PrimitiveStatus::Ok
                }),
                "{scheme:?} omitted a halfway-value success cell"
            );
            for special in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
                for position in [0_usize, 1, 2] {
                    assert!(
                        scheme_pairs.iter().any(|pair| {
                            pair.input.row.len() == 3
                                && pair.input.row[position].to_float().to_bits()
                                    == special.to_bits()
                        }),
                        "{scheme:?} omitted row special={special:?} position={position}"
                    );
                    assert!(
                        scheme_pairs.iter().any(|pair| {
                            pair.input.query.len() == 3
                                && pair.input.query[position].to_float().to_bits()
                                    == special.to_bits()
                        }),
                        "{scheme:?} omitted query special={special:?} position={position}"
                    );
                }
            }
            assert!(
                scheme_pairs.iter().any(|pair| {
                    matches!(
                        pair.observed.result.status,
                        independent::PrimitiveStatus::OutputLength { .. }
                    )
                }),
                "{scheme:?} omitted exact output-length refusal"
            );
            assert!(
                scheme_pairs.iter().any(|pair| {
                    matches!(
                        pair.observed.result.status,
                        independent::PrimitiveStatus::CodeLength { .. }
                    )
                }),
                "{scheme:?} omitted exact code-length refusal"
            );
        }
        let scheme = independent::QuantScheme::Bit4;
        let stochastic = pairs
            .iter()
            .filter(|pair| {
                pair.input.scheme == scheme
                    && pair.observed.result.status == independent::PrimitiveStatus::Ok
            })
            .filter_map(|pair| {
                pair.observed.result.success.as_ref().map(|success| {
                    (
                        &pair.input.row,
                        &pair.input.query,
                        pair.input.query_seed,
                        &success.query_code_bytes,
                    )
                })
            })
            .collect::<Vec<_>>();
        assert!(
            stochastic.iter().enumerate().any(|(index, left)| {
                stochastic.iter().skip(index + 1).any(|right| {
                    left.0 == right.0 && left.1 == right.1 && left.2 != right.2 && left.3 != right.3
                })
            }),
            "{scheme:?} omitted paired stochastic-query seeds with distinct exact bytes"
        );
        for pair in &pairs {
            let expected = independent::expected_quantization(&pair.input);
            independent::check_i25(&expected, &pair.observed)
                .unwrap_or_else(|error| panic!("I25 corpus mismatch: {error}"));
        }
    }

    #[test]
    fn vector_i26_corpus_covers_modes_negative_candidates_and_all_store_tiers() {
        let evidence = run_vector_operation(VectorOperationKind::Rescore, 7, None)
            .expect("complete I26 adapter corpus");
        let VectorInvariantEvidence::I26(pairs) = evidence.invariant else {
            panic!("rescore operation returned a non-I26 invariant");
        };
        assert!(pairs.iter().any(|pair| matches!(
            pair.input.candidates,
            independent::CandidateMode::Dense { .. }
        )));
        assert!(pairs.iter().any(|pair| matches!(
            pair.input.candidates,
            independent::CandidateMode::Retained { .. }
        )));
        for status in [
            "candidate-count",
            "candidate-out-of-range",
            "non-finite-coarse",
        ] {
            assert!(
                pairs.iter().any(|pair| match status {
                    "candidate-count" => matches!(
                        pair.observed.primitive_status,
                        independent::PrimitiveStatus::CandidateRowCount { .. }
                    ),
                    "candidate-out-of-range" => matches!(
                        pair.observed.primitive_status,
                        independent::PrimitiveStatus::CandidateRowOutOfRange { .. }
                    ),
                    _ => matches!(
                        pair.observed.primitive_status,
                        independent::PrimitiveStatus::NonFiniteScore { .. }
                    ),
                }),
                "I26 omitted {status} validation"
            );
        }
        let tiers = pairs
            .iter()
            .filter_map(|pair| pair.input.store.as_ref().map(|store| store.tier))
            .collect::<BTreeSet<_>>();
        assert_eq!(tiers, [0_u8, 1, 2, 3].into_iter().collect());
        assert!(
            pairs.iter().any(|pair| {
                let Some(store) = &pair.input.store else {
                    return false;
                };
                let documents = store
                    .documents_by_row
                    .iter()
                    .flatten()
                    .map(|document| document.doc_id_be)
                    .collect::<Vec<_>>();
                let anti_correlated = documents.windows(2).all(|window| window[0] > window[1]);
                let tied = pair
                    .observed
                    .store_hits
                    .windows(2)
                    .all(|window| window[0].score == window[1].score);
                let document_ordered = pair
                    .observed
                    .store_hits
                    .windows(2)
                    .all(|window| window[0].document < window[1].document);
                anti_correlated && tied && document_ordered
            }),
            "I26 omitted the anti-correlated physical-row/document tie cell"
        );
        for pair in &pairs {
            let expected = independent::expected_rescore(&pair.input);
            independent::check_i26(&expected, &pair.observed)
                .unwrap_or_else(|error| panic!("I26 corpus mismatch: {error}"));
        }
    }

    #[test]
    fn vector_i27_corpus_covers_every_phase_and_tier_with_same_fixture_controls() {
        let evidence = run_vector_operation(VectorOperationKind::RowIdentity, 7, None)
            .expect("complete I27 adapter corpus");
        let VectorInvariantEvidence::I27(pairs) = evidence.invariant else {
            panic!("identity operation returned a non-I27 invariant");
        };
        let phases = pairs
            .iter()
            .map(|pair| pair.input.observation_phase)
            .collect::<BTreeSet<_>>();
        assert_eq!(phases, [0_u8, 1, 2, 3, 4].into_iter().collect());
        let tiers = pairs
            .iter()
            .map(|pair| pair.input.tier)
            .collect::<BTreeSet<_>>();
        assert_eq!(tiers, [0_u8, 1, 2, 3].into_iter().collect());
        assert!(pairs.iter().all(|pair| {
            pair.observed.rows == pair.observed.control_rows
                && pair.observed.rows == pair.observed.retry_rows
        }));
        for pair in &pairs {
            let expected =
                independent::expected_identity(&pair.input).expect("I27 expected lifecycle state");
            independent::check_i27(&expected, &pair.observed)
                .unwrap_or_else(|error| panic!("I27 corpus mismatch: {error}"));
        }
    }

    #[test]
    fn vector_i27_expected_sources_are_derived_without_a_store_query() {
        let source = include_str!("vector_execution.rs");
        let forbidden = ["identity_segment", "for_document"].join("_");
        assert!(
            !source.contains(&format!("fn {forbidden}"))
                && !source.contains(&format!("{forbidden}(")),
            "I27 expected source identities still come from a Store query instead of the primitive mutation schedule"
        );
    }

    #[test]
    fn vector_adapter_in_process_catalog_faults_drain_one_product_receipt() {
        for (operation, fault, seed) in [
            (
                VectorOperationKind::Quantization,
                VectorFaultKind::CorruptCodesFactors,
                0,
            ),
            (
                VectorOperationKind::Quantization,
                VectorFaultKind::CorruptCodesFactors,
                6,
            ),
            (
                VectorOperationKind::Quantization,
                VectorFaultKind::CorruptCodesFactors,
                12,
            ),
            (
                VectorOperationKind::Rescore,
                VectorFaultKind::MissingRescoreRows,
                0,
            ),
            (
                VectorOperationKind::RowIdentity,
                VectorFaultKind::RowCountCancellation,
                13,
            ),
            (
                VectorOperationKind::RowIdentity,
                VectorFaultKind::AllocationDenial,
                14,
            ),
        ] {
            let evidence = run_vector_operation(operation, seed, Some(fault))
                .unwrap_or_else(|error| panic!("{operation:?}/{fault:?}: {error}"));
            assert_eq!(evidence.receipts.len(), 1);
            assert_eq!(evidence.receipts[0].cardinality(), 1);
            assert_eq!(
                evidence.receipts[0].campaign(),
                VectorCampaign::VectorExecution
            );
            assert!(!evidence.receipts[0].result_published());
            let expected_fault_status = match &evidence.mutation {
                VectorMutationEvidence::QuantCorruption {
                    field: VectorQuantMutationField::Bit4OddPadding,
                    ..
                } => {
                    let VectorInvariantEvidence::I25(pairs) = &evidence.invariant else {
                        panic!("Bit4 padding mutation did not carry I25 evidence");
                    };
                    let expected_byte = pairs
                        .iter()
                        .find(|pair| {
                            pair.input.scheme == independent::QuantScheme::Bit4
                                && pair.input.store.document_visible
                        })
                        .and_then(|pair| {
                            independent::expected_quantization(&pair.input)
                                .result
                                .success
                        })
                        .and_then(|success| success.code_bytes.last().copied())
                        .map(|byte| byte | 0x0f)
                        .expect("independent Bit4 persisted code byte");
                    independent::PrimitiveStatus::NonZeroPadding {
                        byte: expected_byte,
                        mask: 0x0f,
                    }
                }
                VectorMutationEvidence::QuantCorruption { .. } => {
                    independent::PrimitiveStatus::NonFiniteScore { row: 0 }
                }
                VectorMutationEvidence::MissingRescoreRows {
                    source: independent::PrimitiveSource::Sealed(segment),
                    expected_rows,
                    available_rows,
                    ..
                } => independent::PrimitiveStatus::SegmentGeometry {
                    detail: format!(
                        "exact scores unavailable for segment {}: expected {expected_rows} rows, got {available_rows}",
                        primitive_segment_text(*segment)
                    ),
                },
                VectorMutationEvidence::RowCancellation { .. } => {
                    independent::PrimitiveStatus::Cancelled { partial: false }
                }
                VectorMutationEvidence::AllocationDenial {
                    component, bytes, ..
                } => independent::PrimitiveStatus::AllocationFailed {
                    component: (*component).to_owned(),
                    needed: *bytes,
                },
                other => panic!("catalog fault omitted an exact typed status: {other:?}"),
            };
            assert_eq!(evidence.control.fault.status, expected_fault_status);
            assert_eq!(
                evidence.control.retry.status,
                independent::PrimitiveStatus::Ok
            );
            assert_eq!(evidence.control.clean, evidence.control.retry);
            assert_eq!(
                evidence.control.clean.generation,
                evidence.control.fault.generation
            );
        }
    }

    #[test]
    fn vector_adapter_forced_backend_uses_fresh_child() {
        let evidence = run_vector_operation(
            VectorOperationKind::KernelParity,
            24,
            Some(VectorFaultKind::ForcedDispatchBackend),
        )
        .expect("forced backend adapter");
        let child = evidence
            .forced_child
            .as_ref()
            .expect("forced dispatch evidence must come from child");
        assert_eq!(
            child.transport,
            ForcedBackendTransportFormat::TypedBinaryV1,
            "forced child receipt crossed the process boundary as generic text"
        );
        assert_eq!(child.receipt.campaign(), VectorCampaign::VectorExecution);
        assert_eq!(
            child.receipt.operation(),
            ProductVectorOperation::KernelParity
        );
        assert_eq!(
            child.receipt.fault(),
            ProductVectorFaultKind::ForcedDispatchBackend
        );
        assert_eq!(
            child.receipt.site(),
            VectorFaultSite::KernelDispatchSelectedScoringTable
        );
        assert_eq!(child.receipt.cardinality(), 1);
        let VectorMutationEvidence::ForcedDispatch { case_id, .. } = evidence.mutation else {
            panic!("forced child omitted its typed mutation plan");
        };
        assert_eq!(child.receipt.seed_case_id(), case_id);
        assert!(child.receipt.result_published());
        assert_eq!(child.pair.input.backend, child.requested);
        assert_eq!(child.pair.observed.backend, child.requested);
        assert_eq!(child.pair.input.signed_a.len(), QUERY.len());
        assert_eq!(
            child.pair.input.bytes_b.len(),
            QUERY.len().div_ceil(2) * ROWS.len()
        );
        assert_eq!(child.fault_result, evidence.control.fault);
        assert_eq!(child.retry_result, evidence.control.retry);
        assert_eq!(evidence.control.clean, evidence.control.fault);
        assert_eq!(evidence.control.clean, evidence.control.retry);
        let VectorInvariantEvidence::I24(pairs) = &evidence.invariant else {
            panic!("forced dispatch returned a non-I24 corpus");
        };
        let expected = expected_comparison_count(
            VectorOperationKind::KernelParity,
            Some(VectorFaultKind::ForcedDispatchBackend),
        )
        .expect("forced family-owned comparison count");
        assert_eq!(expected.0, "I24");
        assert_eq!(
            u64::try_from(pairs.len()).expect("comparison count fits u64"),
            expected.1
        );
    }

    #[test]
    fn vector_missing_rescore_query_site_uses_graph_public_store() {
        let evidence = run_vector_operation(
            VectorOperationKind::Rescore,
            6,
            Some(VectorFaultKind::MissingRescoreRows),
        )
        .expect("graph missing-rescore adapter");
        assert_eq!(evidence.receipts.len(), 1);
        assert_eq!(
            evidence.receipts[0].site(),
            VectorFaultSite::QueryRescoreRows
        );
        assert!(matches!(
            evidence.receipts[0].effect(),
            VectorFaultEffect::MissingRescoreRows {
                requested_tier: VectorSearchTier::Graph,
                ..
            }
        ));
        let VectorMutationEvidence::MissingRescoreRows {
            source: independent::PrimitiveSource::Sealed(segment),
            expected_rows,
            available_rows,
            ..
        } = evidence.mutation
        else {
            panic!("graph missing-rescore evidence omitted its exact mutation DTO");
        };
        assert_eq!(
            evidence.control.fault.status,
            independent::PrimitiveStatus::SegmentGeometry {
                detail: format!(
                    "exact scores unavailable for segment {}: expected {expected_rows} rows, got {available_rows}",
                    primitive_segment_text(segment)
                ),
            }
        );
        assert_eq!(evidence.control.clean, evidence.control.retry);
    }

    #[test]
    fn vector_row_cancellation_covers_active_bit4_int8_and_graph_subsites() {
        for (seed, active, tier) in [
            (0, true, VectorSearchTier::Exact),
            (6, false, VectorSearchTier::Scan),
            (12, false, VectorSearchTier::Scan),
            (18, false, VectorSearchTier::Graph),
        ] {
            let evidence = run_vector_operation(
                VectorOperationKind::RowIdentity,
                seed,
                Some(VectorFaultKind::RowCountCancellation),
            )
            .unwrap_or_else(|error| panic!("cancellation seed {seed}: {error}"));
            assert_eq!(evidence.receipts.len(), 1, "seed {seed}");
            let effect = evidence.receipts[0].effect();
            let VectorFaultEffect::CancelledAfterRows {
                source,
                requested_tier,
                requested_rows,
                observed_rows,
                ..
            } = effect
            else {
                panic!("seed {seed}: wrong typed cancellation effect {effect:?}");
            };
            assert_eq!(matches!(source, VectorRowSource::Active), active);
            assert_eq!(*requested_tier, tier);
            assert_eq!(requested_rows, observed_rows);
            assert_eq!(evidence.control.clean, evidence.control.retry);
        }
    }
}
