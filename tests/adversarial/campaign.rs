use std::fmt;
use std::path::Path;
use std::path::PathBuf;

use super::fault_vfs::FaultEvent;
use super::oracle::CheckerKind;
use super::profiles::FaultProfile;
use super::program::{Op, Program};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum CampaignKind {
    Overall,
    StorageDurability,
    IngestRetention,
    VectorExecution,
    VamanaGraph,
    MetadataFilterPlanner,
    Fts,
    HybridFusion,
    TieringMaintenance,
    LifecycleAccounting,
    DiagnosticsHealth,
    FfiBindings,
}

impl CampaignKind {
    pub const ALL: [Self; 12] = [
        Self::Overall,
        Self::StorageDurability,
        Self::IngestRetention,
        Self::VectorExecution,
        Self::VamanaGraph,
        Self::MetadataFilterPlanner,
        Self::Fts,
        Self::HybridFusion,
        Self::TieringMaintenance,
        Self::LifecycleAccounting,
        Self::DiagnosticsHealth,
        Self::FfiBindings,
    ];

    pub const FEATURES: [Self; 11] = [
        Self::StorageDurability,
        Self::IngestRetention,
        Self::VectorExecution,
        Self::VamanaGraph,
        Self::MetadataFilterPlanner,
        Self::Fts,
        Self::HybridFusion,
        Self::TieringMaintenance,
        Self::LifecycleAccounting,
        Self::DiagnosticsHealth,
        Self::FfiBindings,
    ];

    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Overall => "overall",
            Self::StorageDurability => "storage-durability",
            Self::IngestRetention => "ingest-retention",
            Self::VectorExecution => "vector-execution",
            Self::VamanaGraph => "vamana-graph",
            Self::MetadataFilterPlanner => "metadata-filter-planner",
            Self::Fts => "fts",
            Self::HybridFusion => "hybrid-fusion",
            Self::TieringMaintenance => "tiering-maintenance",
            Self::LifecycleAccounting => "lifecycle-accounting",
            Self::DiagnosticsHealth => "diagnostics-health",
            Self::FfiBindings => "ffi-bindings",
        }
    }

    pub fn from_key(value: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|campaign| campaign.key() == value)
            .ok_or_else(|| {
                format!(
                    "unknown adversarial campaign {value:?}; valid campaigns: {}",
                    Self::catalog_text()
                )
            })
    }

    #[must_use]
    pub fn catalog_text() -> String {
        Self::ALL
            .into_iter()
            .map(Self::key)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl fmt::Display for CampaignKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.key())
    }
}

macro_rules! operation_enum {
    ($name:ident { $($variant:ident => $key:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            #[must_use]
            pub const fn key(self) -> &'static str {
                match self {
                    $(Self::$variant => $key),+
                }
            }
        }
    };
}

operation_enum!(StorageOperation {
    WalPrefix => "wal-prefix",
    Publication => "publication",
    Retry => "retry",
    FormatCheck => "format-check",
    OrphanCleanup => "orphan-cleanup",
});
operation_enum!(IngestOperation {
    BatchCommit => "batch-commit",
    Seal => "seal",
    Retention => "retention",
    Purge => "purge",
});
operation_enum!(VectorOperation {
    KernelParity => "kernel-parity",
    Quantization => "quantization",
    Rescore => "rescore",
    RowIdentity => "row-identity",
});
operation_enum!(GraphOperation {
    Shape => "shape",
    EntryPoints => "entry-points",
    Checkpoint => "checkpoint",
    BoundedBuild => "bounded-build",
    Search => "search",
    Publication => "publication",
    FilteredSearch => "filtered-search",
});
operation_enum!(MetadataOperation {
    Columns => "columns",
    Bitmap => "bitmap",
    Planner => "planner",
    Execution => "execution",
});
operation_enum!(FtsOperation {
    Tokenizer => "tokenizer",
    Regions => "regions",
    Bm25 => "bm25",
    Pruning => "pruning",
    Extras => "extras",
});
operation_enum!(HybridOperation {
    Provenance => "provenance",
    Normalization => "normalization",
    BoundedFusion => "bounded-fusion",
    Rrf => "rrf-fallback",
    Legs => "legs",
});
operation_enum!(TieringOperation {
    Policy => "policy",
    Transition => "transition",
    Budget => "budget",
    Publication => "publication",
});
operation_enum!(LifecycleOperation {
    Deadline => "deadline",
    Cancellation => "cancellation",
    CloseDrain => "close-drain",
    Locking => "locking",
    Accounting => "accounting",
});
operation_enum!(DiagnosticsOperation {
    Health => "health",
    SelfCheck => "self-check",
    Recovery => "recovery",
});
operation_enum!(FfiOperation {
    Validation => "validation",
    Ownership => "ownership",
    Containment => "containment",
    Deadline => "deadline",
    Parity => "parity",
});

/// One typed feature operation. The excluded epoch ranges have no variant.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum FeatureOperation {
    Storage(StorageOperation),
    Ingest(IngestOperation),
    Vector(VectorOperation),
    Graph(GraphOperation),
    Metadata(MetadataOperation),
    Fts(FtsOperation),
    Hybrid(HybridOperation),
    Tiering(TieringOperation),
    Lifecycle(LifecycleOperation),
    Diagnostics(DiagnosticsOperation),
    Ffi(FfiOperation),
}

impl FeatureOperation {
    #[must_use]
    pub const fn campaign(self) -> CampaignKind {
        match self {
            Self::Storage(_) => CampaignKind::StorageDurability,
            Self::Ingest(_) => CampaignKind::IngestRetention,
            Self::Vector(_) => CampaignKind::VectorExecution,
            Self::Graph(_) => CampaignKind::VamanaGraph,
            Self::Metadata(_) => CampaignKind::MetadataFilterPlanner,
            Self::Fts(_) => CampaignKind::Fts,
            Self::Hybrid(_) => CampaignKind::HybridFusion,
            Self::Tiering(_) => CampaignKind::TieringMaintenance,
            Self::Lifecycle(_) => CampaignKind::LifecycleAccounting,
            Self::Diagnostics(_) => CampaignKind::DiagnosticsHealth,
            Self::Ffi(_) => CampaignKind::FfiBindings,
        }
    }

    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Storage(operation) => operation.key(),
            Self::Ingest(operation) => operation.key(),
            Self::Vector(operation) => operation.key(),
            Self::Graph(operation) => operation.key(),
            Self::Metadata(operation) => operation.key(),
            Self::Fts(operation) => operation.key(),
            Self::Hybrid(operation) => operation.key(),
            Self::Tiering(operation) => operation.key(),
            Self::Lifecycle(operation) => operation.key(),
            Self::Diagnostics(operation) => operation.key(),
            Self::Ffi(operation) => operation.key(),
        }
    }
}

const STORAGE_OPERATIONS: [FeatureOperation; 5] = [
    FeatureOperation::Storage(StorageOperation::WalPrefix),
    FeatureOperation::Storage(StorageOperation::Publication),
    FeatureOperation::Storage(StorageOperation::Retry),
    FeatureOperation::Storage(StorageOperation::FormatCheck),
    FeatureOperation::Storage(StorageOperation::OrphanCleanup),
];
const INGEST_OPERATIONS: [FeatureOperation; 4] = [
    FeatureOperation::Ingest(IngestOperation::BatchCommit),
    FeatureOperation::Ingest(IngestOperation::Seal),
    FeatureOperation::Ingest(IngestOperation::Retention),
    FeatureOperation::Ingest(IngestOperation::Purge),
];
const VECTOR_OPERATIONS: [FeatureOperation; 4] = [
    FeatureOperation::Vector(VectorOperation::KernelParity),
    FeatureOperation::Vector(VectorOperation::Quantization),
    FeatureOperation::Vector(VectorOperation::Rescore),
    FeatureOperation::Vector(VectorOperation::RowIdentity),
];
const GRAPH_OPERATIONS: [FeatureOperation; 7] = [
    FeatureOperation::Graph(GraphOperation::Shape),
    FeatureOperation::Graph(GraphOperation::EntryPoints),
    FeatureOperation::Graph(GraphOperation::Checkpoint),
    FeatureOperation::Graph(GraphOperation::BoundedBuild),
    FeatureOperation::Graph(GraphOperation::Search),
    FeatureOperation::Graph(GraphOperation::Publication),
    FeatureOperation::Graph(GraphOperation::FilteredSearch),
];
const METADATA_OPERATIONS: [FeatureOperation; 4] = [
    FeatureOperation::Metadata(MetadataOperation::Columns),
    FeatureOperation::Metadata(MetadataOperation::Bitmap),
    FeatureOperation::Metadata(MetadataOperation::Planner),
    FeatureOperation::Metadata(MetadataOperation::Execution),
];
const FTS_OPERATIONS: [FeatureOperation; 5] = [
    FeatureOperation::Fts(FtsOperation::Tokenizer),
    FeatureOperation::Fts(FtsOperation::Regions),
    FeatureOperation::Fts(FtsOperation::Bm25),
    FeatureOperation::Fts(FtsOperation::Pruning),
    FeatureOperation::Fts(FtsOperation::Extras),
];
const HYBRID_OPERATIONS: [FeatureOperation; 5] = [
    FeatureOperation::Hybrid(HybridOperation::Provenance),
    FeatureOperation::Hybrid(HybridOperation::Normalization),
    FeatureOperation::Hybrid(HybridOperation::BoundedFusion),
    FeatureOperation::Hybrid(HybridOperation::Rrf),
    FeatureOperation::Hybrid(HybridOperation::Legs),
];
const TIERING_OPERATIONS: [FeatureOperation; 4] = [
    FeatureOperation::Tiering(TieringOperation::Policy),
    FeatureOperation::Tiering(TieringOperation::Transition),
    FeatureOperation::Tiering(TieringOperation::Budget),
    FeatureOperation::Tiering(TieringOperation::Publication),
];
const LIFECYCLE_OPERATIONS: [FeatureOperation; 5] = [
    FeatureOperation::Lifecycle(LifecycleOperation::Deadline),
    FeatureOperation::Lifecycle(LifecycleOperation::Cancellation),
    FeatureOperation::Lifecycle(LifecycleOperation::CloseDrain),
    FeatureOperation::Lifecycle(LifecycleOperation::Locking),
    FeatureOperation::Lifecycle(LifecycleOperation::Accounting),
];
const DIAGNOSTICS_OPERATIONS: [FeatureOperation; 3] = [
    FeatureOperation::Diagnostics(DiagnosticsOperation::Health),
    FeatureOperation::Diagnostics(DiagnosticsOperation::SelfCheck),
    FeatureOperation::Diagnostics(DiagnosticsOperation::Recovery),
];
const FFI_OPERATIONS: [FeatureOperation; 5] = [
    FeatureOperation::Ffi(FfiOperation::Validation),
    FeatureOperation::Ffi(FfiOperation::Ownership),
    FeatureOperation::Ffi(FfiOperation::Containment),
    FeatureOperation::Ffi(FfiOperation::Deadline),
    FeatureOperation::Ffi(FfiOperation::Parity),
];

#[must_use]
pub fn feature_operations(campaign: CampaignKind) -> &'static [FeatureOperation] {
    match campaign {
        CampaignKind::Overall => &[],
        CampaignKind::StorageDurability => &STORAGE_OPERATIONS,
        CampaignKind::IngestRetention => &INGEST_OPERATIONS,
        CampaignKind::VectorExecution => &VECTOR_OPERATIONS,
        CampaignKind::VamanaGraph => &GRAPH_OPERATIONS,
        CampaignKind::MetadataFilterPlanner => &METADATA_OPERATIONS,
        CampaignKind::Fts => &FTS_OPERATIONS,
        CampaignKind::HybridFusion => &HYBRID_OPERATIONS,
        CampaignKind::TieringMaintenance => &TIERING_OPERATIONS,
        CampaignKind::LifecycleAccounting => &LIFECYCLE_OPERATIONS,
        CampaignKind::DiagnosticsHealth => &DIAGNOSTICS_OPERATIONS,
        CampaignKind::FfiBindings => &FFI_OPERATIONS,
    }
}

pub fn campaign_from_replay_metadata(directory: &Path) -> Result<CampaignKind, String> {
    let metadata_path = directory.join("episode.json");
    if !metadata_path.is_file() {
        // Campaign-summary v2 and failure-v1 predate typed campaign metadata.
        return Ok(CampaignKind::Overall);
    }
    let bytes = std::fs::read(&metadata_path)
        .map_err(|error| format!("read {}: {error}", metadata_path.display()))?;
    let metadata: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse {}: {error}", metadata_path.display()))?;
    if metadata["version"].as_u64() != Some(3) {
        return Err(format!(
            "unsupported episode metadata version in {}",
            metadata_path.display()
        ));
    }
    let key = metadata["campaign"]
        .as_str()
        .ok_or_else(|| format!("{} has no campaign key", metadata_path.display()))?;
    CampaignKind::from_key(key)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Qualification {
    Exploratory,
    Release,
}

impl Qualification {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Exploratory => "exploratory",
            Self::Release => "release",
        }
    }

    pub fn from_key(value: &str) -> Result<Self, String> {
        match value {
            "exploratory" => Ok(Self::Exploratory),
            "release" => Ok(Self::Release),
            other => Err(format!(
                "invalid ZE_ADV_QUALIFICATION={other:?}; expected exploratory or release"
            )),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunConfig {
    pub campaign: CampaignKind,
    pub seed: u64,
    pub start_seed: u64,
    pub profile: FaultProfile,
    pub qualification: Qualification,
    pub minimum_seconds: u64,
    pub minimum_episodes: u64,
    pub retain_successful: usize,
    pub artifacts: PathBuf,
    pub replay_directory: Option<PathBuf>,
}

impl RunConfig {
    pub fn from_env() -> Result<Self, String> {
        let campaign = CampaignKind::from_key(
            &std::env::var("ZE_ADV_CAMPAIGN").unwrap_or_else(|_| "overall".to_owned()),
        )?;
        let profile = FaultProfile::from_key(
            &std::env::var("ZE_ADV_PROFILE").unwrap_or_else(|_| "none".to_owned()),
        )?;
        let qualification = Qualification::from_key(
            &std::env::var("ZE_ADV_QUALIFICATION").unwrap_or_else(|_| "exploratory".to_owned()),
        )?;
        let seed = env_u64("ZE_ADV_SEED", 0)?;
        let start_seed = match std::env::var("ZE_ADV_START_SEED") {
            Ok(value) => parse_u64("ZE_ADV_START_SEED", &value)?,
            Err(_) => env_u64("ZE_ADV_CAMPAIGN_START_SEED", 0)?,
        };
        let minimum_seconds = env_u64("ZE_ADV_MIN_SECONDS", 8 * 60 * 60)?;
        let minimum_episodes = env_u64("ZE_ADV_MIN_EPISODES", 10_000)?;
        let retain_successful_u64 = env_u64("ZE_ADV_RETAIN_SUCCESSFUL", 256)?;
        let retain_successful = usize::try_from(retain_successful_u64)
            .map_err(|_| "ZE_ADV_RETAIN_SUCCESSFUL does not fit usize".to_owned())?;
        let artifacts = PathBuf::from(
            std::env::var("ZE_ADV_ARTIFACTS").unwrap_or_else(|_| "target/adversarial".to_owned()),
        );
        let replay_directory = std::env::var("ZE_ADV_REPLAY_DIR")
            .ok()
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        Ok(Self {
            campaign,
            seed,
            start_seed,
            profile,
            qualification,
            minimum_seconds,
            minimum_episodes,
            retain_successful,
            artifacts,
            replay_directory,
        })
    }

    pub fn validate_campaign_thresholds(&self, test_mode: bool) -> Result<(), String> {
        if self.qualification == Qualification::Release && !test_mode {
            if self.minimum_seconds < 8 * 60 * 60 {
                return Err("release campaign must run at least eight hours".to_owned());
            }
            if self.minimum_episodes < 10_000 {
                return Err("release campaign must run at least 10,000 episodes".to_owned());
            }
        }
        Ok(())
    }
}

fn env_u64(name: &str, default: u64) -> Result<u64, String> {
    match std::env::var(name) {
        Ok(value) => parse_u64(name, &value),
        Err(_) => Ok(default),
    }
}

fn parse_u64(name: &str, value: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("{name} must be an unsigned integer, got {value:?}"))
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct InvariantId(u8);

impl InvariantId {
    #[must_use]
    pub const fn new(number: u8) -> Self {
        assert!(number >= 1 && number <= 74, "invariant ID must be I1..I74");
        Self(number)
    }

    #[must_use]
    pub const fn number(self) -> u8 {
        self.0
    }

    #[must_use]
    pub fn key(self) -> String {
        format!("I{}", self.0)
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self.0 {
            1 => "acked implies visible",
            2 => "deleted implies gone",
            3 => "exact result parity",
            4 => "durable clean prefix",
            5 => "filter soundness",
            6 => "accounting conservation",
            7 => "corruption never consumed",
            8 => "lifecycle recovery",
            9 => "generation monotonicity",
            10 => "physical purge proof",
            11 => "revision ordering",
            12 => "epoch identity",
            13 => "diagnostics match execution",
            14 => "alias target completeness",
            15 => "atomic publication",
            16 => "WAL prefix and durable acknowledgements",
            17 => "retry idempotence",
            18 => "persisted format fails closed",
            19 => "reachable files preserved and orphans cleaned",
            20 => "batch atomicity",
            21 => "seal multiset preservation",
            22 => "retention boundary correctness",
            23 => "purge completes without resurrection",
            24 => "scalar and SIMD parity",
            25 => "quantization bounds and finite input refusal",
            26 => "exact rescore and tie order",
            27 => "active and sealed row identity",
            28 => "graph shape",
            29 => "entry points",
            30 => "graph reachability",
            31 => "graph result soundness",
            32 => "bounded graph work",
            33 => "atomic graph publication",
            34 => "graph and segment alignment",
            35 => "filtered graph soundness",
            36 => "column and null round trip",
            37 => "bitmap algebra",
            38 => "pruning soundness",
            39 => "reported branch equals executed branch",
            40 => "tokenizer determinism and offsets",
            41 => "lexical region round trip",
            42 => "BM25 oracle parity",
            43 => "dynamic pruning equivalence",
            44 => "lexical extras and snippet spans",
            45 => "score provenance",
            46 => "finite normalization and scale",
            47 => "bounded fusion proof",
            48 => "deterministic RRF fallback",
            49 => "leg atomicity and error precedence",
            50 => "deterministic tier policy",
            51 => "result stability across tier transitions",
            52 => "resumable budget and checkpoint work",
            53 => "published tier matches artifact and diagnostics",
            54 => "deadline correctness",
            55 => "cancellation has no partial results",
            56 => "close drain and pool recovery",
            57 => "locking and access mode",
            58 => "exact resource accounting",
            59 => "epoch write admission",
            60 => "alias commit atomicity",
            61 => "rollback preserves results",
            62 => "safe unpublished epoch reclamation",
            63 => "truthful health state",
            64 => "self check artifact attribution",
            65 => "recovery clears only resolved faults",
            66 => "pointer length and enum validation",
            67 => "handle ownership and generation",
            68 => "panic containment and stable error codes",
            69 => "FFI cancellation and deadline semantics",
            70 => "Rust C Python and Swift parity",
            71 => "finite dimensions and epoch identity",
            72 => "delegate failure or timeout causes no mutation",
            73 => "batch alignment and retry idempotence",
            74 => "embedding transition rollback",
            _ => "invalid invariant",
        }
    }

    #[must_use]
    pub fn checked_coverage_key(self) -> String {
        format!("invariant.{}.checked", self.key())
    }
}

/// Exact operation/checker binding for one independently qualified invariant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvariantSpec {
    pub invariant: InvariantId,
    pub operation: FeatureOperation,
    pub checker_id: &'static str,
    pub checker: CheckerKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CampaignGenerator {
    OverallCompatible,
    FeatureNamespaced,
}

macro_rules! feature_fault_catalog {
    ($(($variant:ident, $campaign:ident, $key:literal, $label:literal, $operation:expr)),+ $(,)?) => {
        /// Closed, typed feature-fault vocabulary for the eleven qualified campaigns.
        #[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
        pub enum FeatureFault {
            $($variant),+
        }

        impl FeatureFault {
            #[must_use]
            pub const fn campaign(self) -> CampaignKind {
                match self {
                    $(Self::$variant => CampaignKind::$campaign),+
                }
            }

            #[must_use]
            pub const fn key(self) -> &'static str {
                match self {
                    $(Self::$variant => $key),+
                }
            }

            #[must_use]
            pub const fn label(self) -> &'static str {
                match self {
                    $(Self::$variant => $label),+
                }
            }

            #[must_use]
            pub const fn operation(self) -> FeatureOperation {
                match self {
                    $(Self::$variant => $operation),+
                }
            }

            #[must_use]
            pub fn coverage_key(self) -> String {
                format!("feature_fault.{}.{}", self.campaign().key(), self.key())
            }
        }
    };
}

feature_fault_catalog![
    (
        StorageTornWalHeader,
        StorageDurability,
        "torn-wal-header",
        "torn WAL header",
        FeatureOperation::Storage(StorageOperation::WalPrefix)
    ),
    (
        StorageTornWalBody,
        StorageDurability,
        "torn-wal-body",
        "torn WAL body",
        FeatureOperation::Storage(StorageOperation::WalPrefix)
    ),
    (
        StorageTornWalChecksum,
        StorageDurability,
        "torn-wal-checksum",
        "torn WAL checksum",
        FeatureOperation::Storage(StorageOperation::WalPrefix)
    ),
    (
        StoragePostCommitError,
        StorageDurability,
        "post-commit-error",
        "post commit error",
        FeatureOperation::Storage(StorageOperation::Retry)
    ),
    (
        StorageManifestPreRenameCrash,
        StorageDurability,
        "manifest-pre-rename-crash",
        "manifest pre rename crash",
        FeatureOperation::Storage(StorageOperation::Publication)
    ),
    (
        StorageManifestPostRenameCrash,
        StorageDurability,
        "manifest-post-rename-crash",
        "manifest post rename crash",
        FeatureOperation::Storage(StorageOperation::Publication)
    ),
    (
        StorageCorruptSegmentRegion,
        StorageDurability,
        "corrupt-segment-region",
        "corrupt segment region",
        FeatureOperation::Storage(StorageOperation::FormatCheck)
    ),
    (
        StorageWrongManifestObject,
        StorageDurability,
        "wrong-manifest-object",
        "wrong manifest object",
        FeatureOperation::Storage(StorageOperation::FormatCheck)
    ),
    (
        StorageWrongSegmentObject,
        StorageDurability,
        "wrong-segment-object",
        "wrong segment object",
        FeatureOperation::Storage(StorageOperation::FormatCheck)
    ),
    (
        StorageListDeleteOmission,
        StorageDurability,
        "list-delete-omission",
        "list or delete omission",
        FeatureOperation::Storage(StorageOperation::OrphanCleanup)
    ),
    (
        IngestPostAckRetry,
        IngestRetention,
        "post-ack-retry",
        "post acknowledgement retry",
        FeatureOperation::Ingest(IngestOperation::BatchCommit)
    ),
    (
        IngestPartialBatchAppend,
        IngestRetention,
        "partial-batch-append",
        "partial batch append",
        FeatureOperation::Ingest(IngestOperation::BatchCommit)
    ),
    (
        IngestSealCancellation,
        IngestRetention,
        "seal-cancellation",
        "seal cancellation",
        FeatureOperation::Ingest(IngestOperation::Seal)
    ),
    (
        IngestRetentionClockBoundary,
        IngestRetention,
        "retention-clock-boundary",
        "retention clock boundary",
        FeatureOperation::Ingest(IngestOperation::Retention)
    ),
    (
        IngestPurgeUnlinkError,
        IngestRetention,
        "purge-unlink-error",
        "purge unlink error",
        FeatureOperation::Ingest(IngestOperation::Purge)
    ),
    (
        IngestPurgeCrashBoundary,
        IngestRetention,
        "purge-crash-boundary",
        "purge crash boundary",
        FeatureOperation::Ingest(IngestOperation::Purge)
    ),
    (
        VectorForcedDispatchBackend,
        VectorExecution,
        "forced-dispatch-backend",
        "forced dispatch backend",
        FeatureOperation::Vector(VectorOperation::KernelParity)
    ),
    (
        VectorCorruptCodesFactors,
        VectorExecution,
        "corrupt-codes-factors",
        "corrupt codes or factors",
        FeatureOperation::Vector(VectorOperation::Quantization)
    ),
    (
        VectorMissingRescoreRows,
        VectorExecution,
        "missing-rescore-rows",
        "missing rescore rows",
        FeatureOperation::Vector(VectorOperation::Rescore)
    ),
    (
        VectorRowCountCancellation,
        VectorExecution,
        "row-count-cancellation",
        "row count cancellation",
        FeatureOperation::Vector(VectorOperation::RowIdentity)
    ),
    (
        VectorAllocationDenial,
        VectorExecution,
        "allocation-denial",
        "allocation denial",
        FeatureOperation::Vector(VectorOperation::RowIdentity)
    ),
    (
        GraphCheckpointCorruption,
        VamanaGraph,
        "checkpoint-corruption",
        "checkpoint corruption",
        FeatureOperation::Graph(GraphOperation::Checkpoint)
    ),
    (
        GraphBuildBudgetCancel,
        VamanaGraph,
        "build-budget-cancel",
        "build budget or cancellation",
        FeatureOperation::Graph(GraphOperation::BoundedBuild)
    ),
    (
        GraphCorruptNode,
        VamanaGraph,
        "corrupt-node",
        "corrupt graph node",
        FeatureOperation::Graph(GraphOperation::Shape)
    ),
    (
        GraphCorruptEntry,
        VamanaGraph,
        "corrupt-entry",
        "corrupt graph entry point",
        FeatureOperation::Graph(GraphOperation::EntryPoints)
    ),
    (
        GraphMissingRescore,
        VamanaGraph,
        "missing-rescore",
        "missing graph rescore",
        FeatureOperation::Graph(GraphOperation::Search)
    ),
    (
        GraphSearchCancellation,
        VamanaGraph,
        "search-cancellation",
        "graph search cancellation",
        FeatureOperation::Graph(GraphOperation::Search)
    ),
    (
        GraphPublicationCrash,
        VamanaGraph,
        "publication-crash",
        "graph publication crash",
        FeatureOperation::Graph(GraphOperation::Publication)
    ),
    (
        MetadataColumnCorruption,
        MetadataFilterPlanner,
        "column-corruption",
        "column dictionary or null mask corruption",
        FeatureOperation::Metadata(MetadataOperation::Columns)
    ),
    (
        MetadataBitmapTruncation,
        MetadataFilterPlanner,
        "bitmap-truncation",
        "bitmap truncation",
        FeatureOperation::Metadata(MetadataOperation::Bitmap)
    ),
    (
        MetadataSelectivityBoundary,
        MetadataFilterPlanner,
        "selectivity-boundary",
        "selectivity boundary input",
        FeatureOperation::Metadata(MetadataOperation::Planner)
    ),
    (
        MetadataVisitedBudgetFallback,
        MetadataFilterPlanner,
        "visited-budget-fallback",
        "visited budget fallback",
        FeatureOperation::Metadata(MetadataOperation::Execution)
    ),
    (
        FtsPostingsCorruption,
        Fts,
        "postings-corruption",
        "postings corruption",
        FeatureOperation::Fts(FtsOperation::Regions)
    ),
    (
        FtsDictionaryCorruption,
        Fts,
        "dictionary-corruption",
        "dictionary corruption",
        FeatureOperation::Fts(FtsOperation::Regions)
    ),
    (
        FtsNormCorruption,
        Fts,
        "norm-corruption",
        "norm corruption",
        FeatureOperation::Fts(FtsOperation::Regions)
    ),
    (
        FtsBlockMaxCorruption,
        Fts,
        "block-max-corruption",
        "block max corruption",
        FeatureOperation::Fts(FtsOperation::Pruning)
    ),
    (
        FtsStoredTextCorruption,
        Fts,
        "stored-text-corruption",
        "stored text corruption",
        FeatureOperation::Fts(FtsOperation::Regions)
    ),
    (
        FtsStoredTextAbsence,
        Fts,
        "stored-text-absence",
        "stored text absence",
        FeatureOperation::Fts(FtsOperation::Extras)
    ),
    (
        FtsLexicalCancellation,
        Fts,
        "lexical-cancellation",
        "lexical cancellation",
        FeatureOperation::Fts(FtsOperation::Extras)
    ),
    (
        HybridVectorLegError,
        HybridFusion,
        "vector-leg-error",
        "vector leg error",
        FeatureOperation::Hybrid(HybridOperation::Legs)
    ),
    (
        HybridLexicalLegError,
        HybridFusion,
        "lexical-leg-error",
        "lexical leg error",
        FeatureOperation::Hybrid(HybridOperation::Legs)
    ),
    (
        HybridDualFailureOrder,
        HybridFusion,
        "dual-failure-order",
        "dual failure completion order",
        FeatureOperation::Hybrid(HybridOperation::Legs)
    ),
    (
        HybridLegPanic,
        HybridFusion,
        "leg-panic",
        "leg panic",
        FeatureOperation::Hybrid(HybridOperation::Legs)
    ),
    (
        HybridEstimatedScore,
        HybridFusion,
        "estimated-score",
        "estimated score injection",
        FeatureOperation::Hybrid(HybridOperation::Provenance)
    ),
    (
        HybridNonfiniteScore,
        HybridFusion,
        "nonfinite-score",
        "nonfinite score",
        FeatureOperation::Hybrid(HybridOperation::Normalization)
    ),
    (
        HybridCancelClose,
        HybridFusion,
        "cancel-close",
        "cancel or close mid query",
        FeatureOperation::Hybrid(HybridOperation::Legs)
    ),
    (
        TierBudgetExhaustion,
        TieringMaintenance,
        "budget-exhaustion",
        "maintenance budget exhaustion",
        FeatureOperation::Tiering(TieringOperation::Budget)
    ),
    (
        TierCheckpointCorruption,
        TieringMaintenance,
        "checkpoint-corruption",
        "checkpoint corruption",
        FeatureOperation::Tiering(TieringOperation::Budget)
    ),
    (
        TierStaleSource,
        TieringMaintenance,
        "stale-source",
        "stale source identity",
        FeatureOperation::Tiering(TieringOperation::Budget)
    ),
    (
        TierEnospc,
        TieringMaintenance,
        "enospc",
        "maintenance ENOSPC",
        FeatureOperation::Tiering(TieringOperation::Publication)
    ),
    (
        TierProfileMismatch,
        TieringMaintenance,
        "profile-mismatch",
        "profile mismatch",
        FeatureOperation::Tiering(TieringOperation::Policy)
    ),
    (
        TierPublicationCrash,
        TieringMaintenance,
        "publication-crash",
        "tier publication crash",
        FeatureOperation::Tiering(TieringOperation::Publication)
    ),
    (
        LifecycleClockFreezeJump,
        LifecycleAccounting,
        "clock-freeze-jump",
        "clock freeze or jump",
        FeatureOperation::Lifecycle(LifecycleOperation::Deadline)
    ),
    (
        LifecycleCancelAdmissionQuery,
        LifecycleAccounting,
        "cancel-admission-query",
        "cancel at admission or mid query",
        FeatureOperation::Lifecycle(LifecycleOperation::Cancellation)
    ),
    (
        LifecycleCloseActiveQuery,
        LifecycleAccounting,
        "close-active-query",
        "close with active query",
        FeatureOperation::Lifecycle(LifecycleOperation::CloseDrain)
    ),
    (
        LifecycleWorkerPanic,
        LifecycleAccounting,
        "worker-panic",
        "pool worker panic",
        FeatureOperation::Lifecycle(LifecycleOperation::CloseDrain)
    ),
    (
        LifecycleLockContention,
        LifecycleAccounting,
        "lock-contention",
        "lock contention",
        FeatureOperation::Lifecycle(LifecycleOperation::Locking)
    ),
    (
        LifecycleAllocationDenial,
        LifecycleAccounting,
        "allocation-denial",
        "allocation denial",
        FeatureOperation::Lifecycle(LifecycleOperation::Accounting)
    ),
    (
        DiagnosticsCounterPlanMutation,
        DiagnosticsHealth,
        "counter-plan-mutation",
        "counter or plan mutation",
        FeatureOperation::Diagnostics(DiagnosticsOperation::Health)
    ),
    (
        DiagnosticsCorruptArtifact,
        DiagnosticsHealth,
        "corrupt-artifact",
        "corrupt attributed artifact",
        FeatureOperation::Diagnostics(DiagnosticsOperation::SelfCheck)
    ),
    (
        DiagnosticsStaleHealth,
        DiagnosticsHealth,
        "stale-health",
        "stale health recovery state",
        FeatureOperation::Diagnostics(DiagnosticsOperation::Recovery)
    ),
    (
        FfiInvalidPointerShape,
        FfiBindings,
        "invalid-pointer-shape",
        "null misaligned or oversized input",
        FeatureOperation::Ffi(FfiOperation::Validation)
    ),
    (
        FfiInvalidEnum,
        FfiBindings,
        "invalid-enum",
        "invalid enum",
        FeatureOperation::Ffi(FfiOperation::Validation)
    ),
    (
        FfiStaleHandle,
        FfiBindings,
        "stale-handle",
        "stale or wrong type handle",
        FeatureOperation::Ffi(FfiOperation::Ownership)
    ),
    (
        FfiDoubleDestroy,
        FfiBindings,
        "double-destroy",
        "double destroy",
        FeatureOperation::Ffi(FfiOperation::Ownership)
    ),
    (
        FfiPanicBoundary,
        FfiBindings,
        "panic-boundary",
        "panic boundary",
        FeatureOperation::Ffi(FfiOperation::Containment)
    ),
    (
        FfiMalformedSequence,
        FfiBindings,
        "malformed-sequence",
        "malformed call sequence",
        FeatureOperation::Ffi(FfiOperation::Parity)
    ),
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeatureFaultEvent {
    pub fault: FeatureFault,
    pub op_index: usize,
    pub fired: bool,
    pub fire_count: usize,
}

/// Typed proof returned by the declared production operation when a selected
/// feature fault reaches its injection boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeatureFaultReceipt {
    pub fault: FeatureFault,
    pub operation: FeatureOperation,
    pub cardinality: usize,
}

impl FeatureFaultEvent {
    #[must_use]
    pub fn json_line(&self) -> String {
        format!(
            "{{\"type\":\"feature\",\"campaign\":\"{}\",\"key\":\"{}\",\"label\":\"{}\",\"op\":{},\"fired\":{},\"fire_count\":{}}}",
            self.fault.campaign().key(),
            self.fault.key(),
            self.fault.label(),
            self.op_index,
            self.fired,
            self.fire_count
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FaultPlan {
    pub generic: Option<FaultEvent>,
    pub feature: Vec<FeatureFaultEvent>,
}

impl FaultPlan {
    #[must_use]
    pub fn for_program(
        campaign: CampaignKind,
        seed: u64,
        profile: FaultProfile,
        program: &Program,
        generic: Option<FaultEvent>,
    ) -> Self {
        let spec = CampaignSpec::for_kind(campaign);
        if campaign == CampaignKind::Overall || spec.feature_faults.is_empty() {
            return Self {
                generic,
                feature: Vec::new(),
            };
        }

        let salt = campaign.key().bytes().fold(0_usize, |value, byte| {
            value.wrapping_mul(131) ^ usize::from(byte)
        });
        let selected = if profile == FaultProfile::Full {
            let first = (seed as usize).wrapping_add(salt) % spec.feature_faults.len();
            let offset = 1
                + ((seed as usize).wrapping_add(salt.rotate_left(7))
                    % spec.feature_faults.len().saturating_sub(1));
            let second = (first + offset) % spec.feature_faults.len();
            vec![spec.feature_faults[first], spec.feature_faults[second]]
        } else {
            let slot = (seed as usize).wrapping_add(salt) % (spec.feature_faults.len() + 1);
            if slot == spec.feature_faults.len() {
                Vec::new()
            } else {
                vec![spec.feature_faults[slot]]
            }
        };
        let feature = selected
            .into_iter()
            .map(|fault| FeatureFaultEvent {
                fault,
                op_index: program
                    .ops
                    .iter()
                    .position(|operation| {
                        matches!(
                            operation,
                            Op::Feature(operation)
                                if operation.campaign() == campaign
                                    && *operation == fault.operation()
                        )
                    })
                    .unwrap_or(usize::MAX),
                fired: false,
                fire_count: 0,
            })
            .collect();
        Self { generic, feature }
    }

    #[must_use]
    pub fn missing_feature_faults(&self) -> Vec<&'static str> {
        self.feature
            .iter()
            .filter(|fault| !fault.fired)
            .map(|fault| fault.fault.key())
            .collect()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CampaignSpec {
    pub kind: CampaignKind,
    pub label: &'static str,
    pub generator: CampaignGenerator,
    pub owned_invariants: &'static [InvariantId],
    pub reused_invariants: &'static [InvariantId],
    pub invariant_specs: &'static [InvariantSpec],
    pub required_operations: &'static [&'static str],
    pub fault_profiles: &'static [FaultProfile],
    pub feature_faults: &'static [FeatureFault],
    pub required_coverage: &'static [&'static str],
    pub smoke_seeds: &'static [u64],
}

impl CampaignSpec {
    #[must_use]
    pub fn catalog() -> &'static [Self] {
        &CAMPAIGN_SPECS
    }

    #[must_use]
    pub fn for_kind(kind: CampaignKind) -> &'static Self {
        CAMPAIGN_SPECS
            .iter()
            .find(|spec| spec.kind == kind)
            .expect("every CampaignKind has one static specification")
    }

    #[must_use]
    pub fn required_invariants(self) -> Vec<InvariantId> {
        let mut invariants = self.reused_invariants.to_vec();
        invariants.extend_from_slice(self.owned_invariants);
        invariants.sort_unstable();
        invariants.dedup();
        invariants
    }

    #[must_use]
    pub fn all_required_coverage(self) -> Vec<String> {
        let mut keys = self
            .required_coverage
            .iter()
            .map(|key| (*key).to_owned())
            .collect::<Vec<_>>();
        keys.extend(
            self.required_invariants()
                .into_iter()
                .map(InvariantId::checked_coverage_key),
        );
        keys.extend(self.feature_faults.iter().map(|fault| fault.coverage_key()));
        keys.extend(
            self.required_operations
                .iter()
                .map(|operation| format!("campaign.op.{}.{}", self.kind.key(), operation)),
        );
        keys.sort();
        keys.dedup();
        keys
    }
}

const SMOKE_SEEDS: [u64; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const OVERALL_ACTIVE_INVARIANTS: [InvariantId; 12] = [
    InvariantId::new(1),
    InvariantId::new(2),
    InvariantId::new(3),
    InvariantId::new(4),
    InvariantId::new(5),
    InvariantId::new(6),
    InvariantId::new(7),
    InvariantId::new(8),
    InvariantId::new(9),
    InvariantId::new(10),
    InvariantId::new(11),
    InvariantId::new(13),
];
/// Append-only identifiers intentionally omitted from non-epoch qualification.
pub const RESERVED_INVARIANTS: [InvariantId; 10] = [
    InvariantId::new(12),
    InvariantId::new(14),
    InvariantId::new(59),
    InvariantId::new(60),
    InvariantId::new(61),
    InvariantId::new(62),
    InvariantId::new(71),
    InvariantId::new(72),
    InvariantId::new(73),
    InvariantId::new(74),
];
const NO_INVARIANTS: [InvariantId; 0] = [];
const I15_I19: [InvariantId; 5] = invariant_range::<5>(15);
const I20_I23: [InvariantId; 4] = invariant_range::<4>(20);
const I24_I27: [InvariantId; 4] = invariant_range::<4>(24);
const I28_I35: [InvariantId; 8] = invariant_range::<8>(28);
const I36_I39: [InvariantId; 4] = invariant_range::<4>(36);
const I40_I44: [InvariantId; 5] = invariant_range::<5>(40);
const I45_I49: [InvariantId; 5] = invariant_range::<5>(45);
const I50_I53: [InvariantId; 4] = invariant_range::<4>(50);
const I54_I58: [InvariantId; 5] = invariant_range::<5>(54);
const I63_I65: [InvariantId; 3] = invariant_range::<3>(63);
const I66_I70: [InvariantId; 5] = invariant_range::<5>(66);

const fn invariant_range<const N: usize>(start: u8) -> [InvariantId; N] {
    let mut result = [InvariantId::new(1); N];
    let mut index = 0;
    while index < N {
        result[index] = InvariantId::new(start + index as u8);
        index += 1;
    }
    result
}

const STORAGE_REUSED: [InvariantId; 7] = [
    InvariantId::new(1),
    InvariantId::new(4),
    InvariantId::new(7),
    InvariantId::new(8),
    InvariantId::new(9),
    InvariantId::new(10),
    InvariantId::new(11),
];
const INGEST_REUSED: [InvariantId; 6] = [
    InvariantId::new(1),
    InvariantId::new(2),
    InvariantId::new(4),
    InvariantId::new(9),
    InvariantId::new(10),
    InvariantId::new(11),
];
const QUERY_REUSED: [InvariantId; 3] = [
    InvariantId::new(1),
    InvariantId::new(2),
    InvariantId::new(3),
];
const FILTER_REUSED: [InvariantId; 3] = [
    InvariantId::new(3),
    InvariantId::new(5),
    InvariantId::new(13),
];
const LIFECYCLE_REUSED: [InvariantId; 3] = [
    InvariantId::new(6),
    InvariantId::new(8),
    InvariantId::new(9),
];

macro_rules! feature_faults {
    ($($fault:ident),+ $(,)?) => {
        [$(FeatureFault::$fault),+]
    };
}

const STORAGE_FAULTS: [FeatureFault; 10] = feature_faults![
    StorageTornWalHeader,
    StorageTornWalBody,
    StorageTornWalChecksum,
    StoragePostCommitError,
    StorageManifestPreRenameCrash,
    StorageManifestPostRenameCrash,
    StorageCorruptSegmentRegion,
    StorageWrongManifestObject,
    StorageWrongSegmentObject,
    StorageListDeleteOmission,
];
const INGEST_FAULTS: [FeatureFault; 6] = feature_faults![
    IngestPostAckRetry,
    IngestPartialBatchAppend,
    IngestSealCancellation,
    IngestRetentionClockBoundary,
    IngestPurgeUnlinkError,
    IngestPurgeCrashBoundary,
];
const VECTOR_FAULTS: [FeatureFault; 5] = feature_faults![
    VectorForcedDispatchBackend,
    VectorCorruptCodesFactors,
    VectorMissingRescoreRows,
    VectorRowCountCancellation,
    VectorAllocationDenial,
];
const GRAPH_FAULTS: [FeatureFault; 7] = feature_faults![
    GraphCheckpointCorruption,
    GraphBuildBudgetCancel,
    GraphCorruptNode,
    GraphCorruptEntry,
    GraphMissingRescore,
    GraphSearchCancellation,
    GraphPublicationCrash,
];
const FILTER_FAULTS: [FeatureFault; 4] = feature_faults![
    MetadataColumnCorruption,
    MetadataBitmapTruncation,
    MetadataSelectivityBoundary,
    MetadataVisitedBudgetFallback,
];
const FTS_FAULTS: [FeatureFault; 7] = feature_faults![
    FtsPostingsCorruption,
    FtsDictionaryCorruption,
    FtsNormCorruption,
    FtsBlockMaxCorruption,
    FtsStoredTextCorruption,
    FtsStoredTextAbsence,
    FtsLexicalCancellation,
];
const HYBRID_FAULTS: [FeatureFault; 7] = feature_faults![
    HybridVectorLegError,
    HybridLexicalLegError,
    HybridDualFailureOrder,
    HybridLegPanic,
    HybridEstimatedScore,
    HybridNonfiniteScore,
    HybridCancelClose,
];
const TIER_FAULTS: [FeatureFault; 6] = feature_faults![
    TierBudgetExhaustion,
    TierCheckpointCorruption,
    TierStaleSource,
    TierEnospc,
    TierProfileMismatch,
    TierPublicationCrash,
];
const LIFECYCLE_FAULTS: [FeatureFault; 6] = feature_faults![
    LifecycleClockFreezeJump,
    LifecycleCancelAdmissionQuery,
    LifecycleCloseActiveQuery,
    LifecycleWorkerPanic,
    LifecycleLockContention,
    LifecycleAllocationDenial,
];
const DIAGNOSTIC_FAULTS: [FeatureFault; 3] = feature_faults![
    DiagnosticsCounterPlanMutation,
    DiagnosticsCorruptArtifact,
    DiagnosticsStaleHealth,
];
const FFI_FAULTS: [FeatureFault; 6] = feature_faults![
    FfiInvalidPointerShape,
    FfiInvalidEnum,
    FfiStaleHandle,
    FfiDoubleDestroy,
    FfiPanicBoundary,
    FfiMalformedSequence,
];
const NO_FAULTS: [FeatureFault; 0] = [];

const OVERALL_OPS: [&str; 1] = ["overall-program"];
const STORAGE_OPS: [&str; 5] = [
    "wal-prefix",
    "publication",
    "retry",
    "format-check",
    "orphan-cleanup",
];
const INGEST_OPS: [&str; 4] = ["batch-commit", "seal", "retention", "purge"];
const VECTOR_OPS: [&str; 4] = ["kernel-parity", "quantization", "rescore", "row-identity"];
const GRAPH_OPS: [&str; 7] = [
    "shape",
    "entry-points",
    "checkpoint",
    "bounded-build",
    "search",
    "publication",
    "filtered-search",
];
const FILTER_OPS: [&str; 4] = ["columns", "bitmap", "planner", "execution"];
const FTS_OPS: [&str; 5] = ["tokenizer", "regions", "bm25", "pruning", "extras"];
const HYBRID_OPS: [&str; 5] = [
    "provenance",
    "normalization",
    "bounded-fusion",
    "rrf-fallback",
    "legs",
];
const TIER_OPS: [&str; 4] = ["policy", "transition", "budget", "publication"];
const LIFECYCLE_OPS: [&str; 5] = [
    "deadline",
    "cancellation",
    "close-drain",
    "locking",
    "accounting",
];
const DIAGNOSTIC_OPS: [&str; 3] = ["health", "self-check", "recovery"];
const FFI_OPS: [&str; 5] = [
    "validation",
    "ownership",
    "containment",
    "deadline",
    "parity",
];

const OVERALL_COVERAGE: [&str; 1] = ["op.open"];
const STORAGE_COVERAGE: [&str; 2] = ["op.ingest", "op.crash"];
const INGEST_COVERAGE: [&str; 3] = ["op.ingest", "op.seal", "op.purge"];
const VECTOR_COVERAGE: [&str; 2] = ["op.search", "search.scan"];
const GRAPH_COVERAGE: [&str; 2] = ["search.graph", "search.filtered_graph"];
const FILTER_COVERAGE: [&str; 2] = ["op.predicate_search", "op.filtered_search"];
const FTS_COVERAGE: [&str; 2] = ["store.lexical_search", "op.fts_extras_probe"];
const HYBRID_COVERAGE: [&str; 2] = ["store.hybrid_search", "op.hybrid_search"];
const TIER_COVERAGE: [&str; 2] = ["op.maintain", "search.auto"];
const LIFECYCLE_COVERAGE: [&str; 3] = ["op.deadline_probe", "op.close", "op.stats"];
const DIAGNOSTIC_COVERAGE: [&str; 2] = ["op.stats", "op.search"];
const FFI_COVERAGE: [&str; 1] = ["op.ffi_probe"];

macro_rules! invariant_specs {
    ($(($id:literal, $operation:expr, $checker:ident, $checker_id:literal)),+ $(,)?) => {
        [$(InvariantSpec {
            invariant: InvariantId::new($id),
            operation: $operation,
            checker_id: $checker_id,
            checker: CheckerKind::$checker,
        }),+]
    };
}

const NO_INVARIANT_SPECS: [InvariantSpec; 0] = [];
const STORAGE_INVARIANT_SPECS: [InvariantSpec; 5] = invariant_specs![
    (
        15,
        FeatureOperation::Storage(StorageOperation::Publication),
        ExactSet,
        "I15.publication-atomicity"
    ),
    (
        16,
        FeatureOperation::Storage(StorageOperation::WalPrefix),
        Prefix,
        "I16.wal-prefix"
    ),
    (
        17,
        FeatureOperation::Storage(StorageOperation::Retry),
        ExactSet,
        "I17.retry-idempotence"
    ),
    (
        18,
        FeatureOperation::Storage(StorageOperation::FormatCheck),
        TypedRefusal,
        "I18.format-refusal"
    ),
    (
        19,
        FeatureOperation::Storage(StorageOperation::OrphanCleanup),
        ExactSet,
        "I19.reachability"
    ),
];
const INGEST_INVARIANT_SPECS: [InvariantSpec; 4] = invariant_specs![
    (
        20,
        FeatureOperation::Ingest(IngestOperation::BatchCommit),
        ExactSet,
        "I20.batch-atomicity"
    ),
    (
        21,
        FeatureOperation::Ingest(IngestOperation::Seal),
        ExactSet,
        "I21.seal-multiset"
    ),
    (
        22,
        FeatureOperation::Ingest(IngestOperation::Retention),
        Range,
        "I22.retention-boundary"
    ),
    (
        23,
        FeatureOperation::Ingest(IngestOperation::Purge),
        ExactSet,
        "I23.purge-proof"
    ),
];
const VECTOR_INVARIANT_SPECS: [InvariantSpec; 4] = invariant_specs![
    (
        24,
        FeatureOperation::Vector(VectorOperation::KernelParity),
        StableBits,
        "I24.kernel-parity"
    ),
    (
        25,
        FeatureOperation::Vector(VectorOperation::Quantization),
        Finite,
        "I25.quantization-finite"
    ),
    (
        26,
        FeatureOperation::Vector(VectorOperation::Rescore),
        ExactSequence,
        "I26.rescore-order"
    ),
    (
        27,
        FeatureOperation::Vector(VectorOperation::RowIdentity),
        ExactSet,
        "I27.row-identity"
    ),
];
const GRAPH_INVARIANT_SPECS: [InvariantSpec; 8] = invariant_specs![
    (
        28,
        FeatureOperation::Graph(GraphOperation::Shape),
        Bounded,
        "I28.graph-shape"
    ),
    (
        29,
        FeatureOperation::Graph(GraphOperation::EntryPoints),
        ExactSet,
        "I29.entry-points"
    ),
    (
        30,
        FeatureOperation::Graph(GraphOperation::Search),
        ExactSet,
        "I30.reachability"
    ),
    (
        31,
        FeatureOperation::Graph(GraphOperation::Search),
        ExactSequence,
        "I31.graph-results"
    ),
    (
        32,
        FeatureOperation::Graph(GraphOperation::BoundedBuild),
        Bounded,
        "I32.graph-work-cap"
    ),
    (
        33,
        FeatureOperation::Graph(GraphOperation::Publication),
        ExactSet,
        "I33.graph-publication"
    ),
    (
        34,
        FeatureOperation::Graph(GraphOperation::Checkpoint),
        Attribution,
        "I34.graph-segment-binding"
    ),
    (
        35,
        FeatureOperation::Graph(GraphOperation::FilteredSearch),
        ExactSet,
        "I35.filtered-graph"
    ),
];
const METADATA_INVARIANT_SPECS: [InvariantSpec; 4] = invariant_specs![
    (
        36,
        FeatureOperation::Metadata(MetadataOperation::Columns),
        ExactSequence,
        "I36.column-roundtrip"
    ),
    (
        37,
        FeatureOperation::Metadata(MetadataOperation::Bitmap),
        ExactSet,
        "I37.bitmap-algebra"
    ),
    (
        38,
        FeatureOperation::Metadata(MetadataOperation::Planner),
        ExactSet,
        "I38.pruning-soundness"
    ),
    (
        39,
        FeatureOperation::Metadata(MetadataOperation::Execution),
        Attribution,
        "I39.executed-branch"
    ),
];
const FTS_INVARIANT_SPECS: [InvariantSpec; 5] = invariant_specs![
    (
        40,
        FeatureOperation::Fts(FtsOperation::Tokenizer),
        ExactSequence,
        "I40.tokenizer-offsets"
    ),
    (
        41,
        FeatureOperation::Fts(FtsOperation::Regions),
        ExactSequence,
        "I41.lexical-regions"
    ),
    (
        42,
        FeatureOperation::Fts(FtsOperation::Bm25),
        StableBits,
        "I42.bm25"
    ),
    (
        43,
        FeatureOperation::Fts(FtsOperation::Pruning),
        ExactSequence,
        "I43.pruning-equivalence"
    ),
    (
        44,
        FeatureOperation::Fts(FtsOperation::Extras),
        ExactSequence,
        "I44.lexical-extras"
    ),
];
const HYBRID_INVARIANT_SPECS: [InvariantSpec; 5] = invariant_specs![
    (
        45,
        FeatureOperation::Hybrid(HybridOperation::Provenance),
        Attribution,
        "I45.provenance"
    ),
    (
        46,
        FeatureOperation::Hybrid(HybridOperation::Normalization),
        Finite,
        "I46.normalization"
    ),
    (
        47,
        FeatureOperation::Hybrid(HybridOperation::BoundedFusion),
        Bounded,
        "I47.fusion-bound"
    ),
    (
        48,
        FeatureOperation::Hybrid(HybridOperation::Rrf),
        ExactSequence,
        "I48.rrf"
    ),
    (
        49,
        FeatureOperation::Hybrid(HybridOperation::Legs),
        NoPartial,
        "I49.leg-atomicity"
    ),
];
const TIER_INVARIANT_SPECS: [InvariantSpec; 4] = invariant_specs![
    (
        50,
        FeatureOperation::Tiering(TieringOperation::Policy),
        ExactScalar,
        "I50.tier-policy"
    ),
    (
        51,
        FeatureOperation::Tiering(TieringOperation::Transition),
        StableBits,
        "I51.transition-results"
    ),
    (
        52,
        FeatureOperation::Tiering(TieringOperation::Budget),
        Monotonic,
        "I52.checkpoint-progress"
    ),
    (
        53,
        FeatureOperation::Tiering(TieringOperation::Publication),
        Attribution,
        "I53.tier-artifact"
    ),
];
const LIFECYCLE_INVARIANT_SPECS: [InvariantSpec; 5] = invariant_specs![
    (
        54,
        FeatureOperation::Lifecycle(LifecycleOperation::Deadline),
        Range,
        "I54.deadline"
    ),
    (
        55,
        FeatureOperation::Lifecycle(LifecycleOperation::Cancellation),
        NoPartial,
        "I55.cancellation"
    ),
    (
        56,
        FeatureOperation::Lifecycle(LifecycleOperation::CloseDrain),
        ExactSet,
        "I56.close-drain"
    ),
    (
        57,
        FeatureOperation::Lifecycle(LifecycleOperation::Locking),
        ExactScalar,
        "I57.lock-matrix"
    ),
    (
        58,
        FeatureOperation::Lifecycle(LifecycleOperation::Accounting),
        ExactScalar,
        "I58.resource-accounting"
    ),
];
const DIAGNOSTIC_INVARIANT_SPECS: [InvariantSpec; 3] = invariant_specs![
    (
        63,
        FeatureOperation::Diagnostics(DiagnosticsOperation::Health),
        ExactScalar,
        "I63.health-truth"
    ),
    (
        64,
        FeatureOperation::Diagnostics(DiagnosticsOperation::SelfCheck),
        Attribution,
        "I64.artifact-attribution"
    ),
    (
        65,
        FeatureOperation::Diagnostics(DiagnosticsOperation::Recovery),
        ExactSet,
        "I65.scoped-recovery"
    ),
];
const FFI_INVARIANT_SPECS: [InvariantSpec; 5] = invariant_specs![
    (
        66,
        FeatureOperation::Ffi(FfiOperation::Validation),
        TypedRefusal,
        "I66.abi-validation"
    ),
    (
        67,
        FeatureOperation::Ffi(FfiOperation::Ownership),
        ExactScalar,
        "I67.handle-state"
    ),
    (
        68,
        FeatureOperation::Ffi(FfiOperation::Containment),
        TypedRefusal,
        "I68.panic-containment"
    ),
    (
        69,
        FeatureOperation::Ffi(FfiOperation::Deadline),
        NoPartial,
        "I69.ffi-control"
    ),
    (
        70,
        FeatureOperation::Ffi(FfiOperation::Parity),
        Parity,
        "I70.binding-parity"
    ),
];

const CAMPAIGN_SPECS: [CampaignSpec; 12] = [
    CampaignSpec {
        kind: CampaignKind::Overall,
        label: "overall outcome",
        generator: CampaignGenerator::OverallCompatible,
        owned_invariants: &OVERALL_ACTIVE_INVARIANTS,
        reused_invariants: &NO_INVARIANTS,
        invariant_specs: &NO_INVARIANT_SPECS,
        required_operations: &OVERALL_OPS,
        fault_profiles: &FaultProfile::DEFAULTS,
        feature_faults: &NO_FAULTS,
        required_coverage: &OVERALL_COVERAGE,
        smoke_seeds: &SMOKE_SEEDS,
    },
    CampaignSpec {
        kind: CampaignKind::StorageDurability,
        label: "storage durability",
        generator: CampaignGenerator::FeatureNamespaced,
        owned_invariants: &I15_I19,
        reused_invariants: &STORAGE_REUSED,
        invariant_specs: &STORAGE_INVARIANT_SPECS,
        required_operations: &STORAGE_OPS,
        fault_profiles: &FaultProfile::DEFAULTS,
        feature_faults: &STORAGE_FAULTS,
        required_coverage: &STORAGE_COVERAGE,
        smoke_seeds: &SMOKE_SEEDS,
    },
    CampaignSpec {
        kind: CampaignKind::IngestRetention,
        label: "ingest retention",
        generator: CampaignGenerator::FeatureNamespaced,
        owned_invariants: &I20_I23,
        reused_invariants: &INGEST_REUSED,
        invariant_specs: &INGEST_INVARIANT_SPECS,
        required_operations: &INGEST_OPS,
        fault_profiles: &FaultProfile::DEFAULTS,
        feature_faults: &INGEST_FAULTS,
        required_coverage: &INGEST_COVERAGE,
        smoke_seeds: &SMOKE_SEEDS,
    },
    CampaignSpec {
        kind: CampaignKind::VectorExecution,
        label: "vector execution",
        generator: CampaignGenerator::FeatureNamespaced,
        owned_invariants: &I24_I27,
        reused_invariants: &QUERY_REUSED,
        invariant_specs: &VECTOR_INVARIANT_SPECS,
        required_operations: &VECTOR_OPS,
        fault_profiles: &FaultProfile::DEFAULTS,
        feature_faults: &VECTOR_FAULTS,
        required_coverage: &VECTOR_COVERAGE,
        smoke_seeds: &SMOKE_SEEDS,
    },
    CampaignSpec {
        kind: CampaignKind::VamanaGraph,
        label: "Vamana graph",
        generator: CampaignGenerator::FeatureNamespaced,
        owned_invariants: &I28_I35,
        reused_invariants: &QUERY_REUSED,
        invariant_specs: &GRAPH_INVARIANT_SPECS,
        required_operations: &GRAPH_OPS,
        fault_profiles: &FaultProfile::DEFAULTS,
        feature_faults: &GRAPH_FAULTS,
        required_coverage: &GRAPH_COVERAGE,
        smoke_seeds: &SMOKE_SEEDS,
    },
    CampaignSpec {
        kind: CampaignKind::MetadataFilterPlanner,
        label: "metadata filter planner",
        generator: CampaignGenerator::FeatureNamespaced,
        owned_invariants: &I36_I39,
        reused_invariants: &FILTER_REUSED,
        invariant_specs: &METADATA_INVARIANT_SPECS,
        required_operations: &FILTER_OPS,
        fault_profiles: &FaultProfile::DEFAULTS,
        feature_faults: &FILTER_FAULTS,
        required_coverage: &FILTER_COVERAGE,
        smoke_seeds: &SMOKE_SEEDS,
    },
    CampaignSpec {
        kind: CampaignKind::Fts,
        label: "full text search",
        generator: CampaignGenerator::FeatureNamespaced,
        owned_invariants: &I40_I44,
        reused_invariants: &QUERY_REUSED,
        invariant_specs: &FTS_INVARIANT_SPECS,
        required_operations: &FTS_OPS,
        fault_profiles: &FaultProfile::DEFAULTS,
        feature_faults: &FTS_FAULTS,
        required_coverage: &FTS_COVERAGE,
        smoke_seeds: &SMOKE_SEEDS,
    },
    CampaignSpec {
        kind: CampaignKind::HybridFusion,
        label: "hybrid fusion",
        generator: CampaignGenerator::FeatureNamespaced,
        owned_invariants: &I45_I49,
        reused_invariants: &QUERY_REUSED,
        invariant_specs: &HYBRID_INVARIANT_SPECS,
        required_operations: &HYBRID_OPS,
        fault_profiles: &FaultProfile::DEFAULTS,
        feature_faults: &HYBRID_FAULTS,
        required_coverage: &HYBRID_COVERAGE,
        smoke_seeds: &SMOKE_SEEDS,
    },
    CampaignSpec {
        kind: CampaignKind::TieringMaintenance,
        label: "tiering maintenance",
        generator: CampaignGenerator::FeatureNamespaced,
        owned_invariants: &I50_I53,
        reused_invariants: &QUERY_REUSED,
        invariant_specs: &TIER_INVARIANT_SPECS,
        required_operations: &TIER_OPS,
        fault_profiles: &FaultProfile::DEFAULTS,
        feature_faults: &TIER_FAULTS,
        required_coverage: &TIER_COVERAGE,
        smoke_seeds: &SMOKE_SEEDS,
    },
    CampaignSpec {
        kind: CampaignKind::LifecycleAccounting,
        label: "lifecycle accounting",
        generator: CampaignGenerator::FeatureNamespaced,
        owned_invariants: &I54_I58,
        reused_invariants: &LIFECYCLE_REUSED,
        invariant_specs: &LIFECYCLE_INVARIANT_SPECS,
        required_operations: &LIFECYCLE_OPS,
        fault_profiles: &FaultProfile::DEFAULTS,
        feature_faults: &LIFECYCLE_FAULTS,
        required_coverage: &LIFECYCLE_COVERAGE,
        smoke_seeds: &SMOKE_SEEDS,
    },
    CampaignSpec {
        kind: CampaignKind::DiagnosticsHealth,
        label: "diagnostics health",
        generator: CampaignGenerator::FeatureNamespaced,
        owned_invariants: &I63_I65,
        reused_invariants: &FILTER_REUSED,
        invariant_specs: &DIAGNOSTIC_INVARIANT_SPECS,
        required_operations: &DIAGNOSTIC_OPS,
        fault_profiles: &FaultProfile::DEFAULTS,
        feature_faults: &DIAGNOSTIC_FAULTS,
        required_coverage: &DIAGNOSTIC_COVERAGE,
        smoke_seeds: &SMOKE_SEEDS,
    },
    CampaignSpec {
        kind: CampaignKind::FfiBindings,
        label: "FFI bindings",
        generator: CampaignGenerator::FeatureNamespaced,
        owned_invariants: &I66_I70,
        reused_invariants: &LIFECYCLE_REUSED,
        invariant_specs: &FFI_INVARIANT_SPECS,
        required_operations: &FFI_OPS,
        fault_profiles: &FaultProfile::DEFAULTS,
        feature_faults: &FFI_FAULTS,
        required_coverage: &FFI_COVERAGE,
        smoke_seeds: &SMOKE_SEEDS,
    },
];
