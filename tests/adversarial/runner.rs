use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, EpochId, EpochIdentity,
    EpochTransitionError, Normalization, StoreEpoch,
};
use zeppelin_embed::fts::dict::{TermDictionary, TermInfo};
use zeppelin_embed::fts::fuzzy;
use zeppelin_embed::fts::index::{DEFAULT_FIELD, Document, SegmentIndex};
use zeppelin_embed::fts::phonetic;
use zeppelin_embed::fts::phrase::{self, PhraseQuery};
use zeppelin_embed::fts::prefix;
use zeppelin_embed::fts::search::TermQuery;
use zeppelin_embed::fts::snippet;
use zeppelin_embed::fts::tokenizer::{Analyzer, Profile, TokenizerConfig};
use zeppelin_embed::fusion::HybridQuery;
use zeppelin_embed::graph::search::GraphSearchProfile;
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, GraphSearchStats, IngestBatch, IngestDocument,
    IngestError, Revision, RowSource, SearchRequest,
};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::{
    CancelToken, Deadline, GraphSearchOptions, ManualMonotonicClock, OpenOptions, QueryControl,
    SearchOptions, SearchTier, Store, StoreTestDependencies,
};
use zeppelin_embed::manifest::EpochMeta;
use zeppelin_embed::manifest::io::{commit_manifest, load_manifest};
use zeppelin_embed::meta::{
    AliveSet, ColumnDefinition, ColumnId, ColumnStoreBuilder, ColumnType, Predicate,
    PredicateValue, RangeBound, RangePredicate, Schema, TIMESTAMP_COLUMN,
};
use zeppelin_embed::planner::{
    FilterMode, PlanFallback, PlanNode, SegmentBranch, SegmentPlan, SegmentTier,
};
use zeppelin_embed::quant::{Bit4Factors, quantize_bit4};
use zeppelin_embed::scan::ScanOptions;
use zeppelin_embed::segment::SegmentId;
use zeppelin_embed::segment::writer::{
    SegmentBuild, SegmentDocumentVersions, SegmentFactors, write_segment_with_documents,
};
use zeppelin_embed::tier::{MaintenanceBudget, MaintenanceStatus, TierThresholds};
use zeppelin_embed::vfs::StdVfs;

use super::artifacts::RunArtifacts;
use super::campaign::{CampaignKind, CampaignSpec, FaultPlan};
use super::coverage::CoverageRegistry;
use super::fault_vfs::{self, FaultEvent};
use super::model::{ExpectedHit, Model, ModelEpoch};
use super::oracle::OracleRecord;
use super::profiles::FaultProfile;
use super::program::{self, Op, Program, SearchKind};

const THREAD_BUDGET: usize = 1;
const NUMERIC_COLUMN: ColumnId = ColumnId::new(1);
const BOOLEAN_COLUMN: ColumnId = ColumnId::new(2);
const STRING_COLUMN: ColumnId = ColumnId::new(3);

fn adversarial_schema() -> Schema {
    Schema::new(vec![
        ColumnDefinition::new(NUMERIC_COLUMN, "number", ColumnType::U64, true),
        ColumnDefinition::new(BOOLEAN_COLUMN, "flag", ColumnType::Bool, true),
        ColumnDefinition::new(STRING_COLUMN, "class", ColumnType::RawString, true),
    ])
    .expect("adversarial schema is statically valid")
}

fn adversarial_columns(doc_id: u32) -> Vec<(ColumnId, PredicateValue)> {
    let mut columns = vec![
        (
            NUMERIC_COLUMN,
            PredicateValue::U64(program::numeric_column(doc_id)),
        ),
        (
            STRING_COLUMN,
            PredicateValue::String(program::string_column(doc_id).to_owned()),
        ),
    ];
    if let Some(value) = program::boolean_column(doc_id) {
        columns.push((BOOLEAN_COLUMN, PredicateValue::Bool(value)));
    }
    columns
}

fn adversarial_predicate(kind: program::PredicateKind) -> Predicate {
    let numeric_eq = |value| Predicate::Eq {
        column: NUMERIC_COLUMN,
        value: PredicateValue::U64(value),
    };
    let string_eq = |value: &str| Predicate::Eq {
        column: STRING_COLUMN,
        value: PredicateValue::String(value.to_owned()),
    };
    match kind {
        program::PredicateKind::Eq => numeric_eq(1),
        program::PredicateKind::In => Predicate::In {
            column: NUMERIC_COLUMN,
            values: vec![PredicateValue::U64(1), PredicateValue::U64(3)],
        },
        program::PredicateKind::RangeTwoSided => Predicate::Range(RangePredicate {
            column: NUMERIC_COLUMN,
            lower: Some(RangeBound::inclusive(PredicateValue::U64(1))),
            upper: Some(RangeBound::inclusive(PredicateValue::U64(3))),
        }),
        program::PredicateKind::RangeHalfOpen => Predicate::Range(RangePredicate {
            column: NUMERIC_COLUMN,
            lower: Some(RangeBound::inclusive(PredicateValue::U64(2))),
            upper: None,
        }),
        program::PredicateKind::Bool => Predicate::Eq {
            column: BOOLEAN_COLUMN,
            value: PredicateValue::Bool(true),
        },
        program::PredicateKind::String => string_eq("even"),
        program::PredicateKind::Exists => Predicate::Exists(BOOLEAN_COLUMN),
        program::PredicateKind::IsNull => Predicate::IsNull(BOOLEAN_COLUMN),
        program::PredicateKind::And => Predicate::And(vec![
            Predicate::Range(RangePredicate {
                column: NUMERIC_COLUMN,
                lower: Some(RangeBound::inclusive(PredicateValue::U64(1))),
                upper: None,
            }),
            Predicate::Exists(BOOLEAN_COLUMN),
        ]),
        program::PredicateKind::Or => Predicate::Or(vec![numeric_eq(0), string_eq("odd")]),
        program::PredicateKind::Not => Predicate::Not(Box::new(string_eq("odd"))),
    }
}

fn declared_store_epoch() -> StoreEpoch {
    let document = EmbeddingTower {
        model_id: "adversarial-embedding".to_owned(),
        model_version: "1".to_owned(),
        weights_digest: vec![0xad, 0x12],
        dims: program::DIMENSIONS as u32,
        // The harness's own vectors are NOT unit length -- `program::vector`
        // returns arbitrary magnitudes and `Model` scores them with
        // `squared_l2`. Declaring L2 here made the epoch metadata lie about
        // the data, which R06's profile selection correctly refused. Declare
        // what the fixture actually produces.
        normalization: Normalization::None,
        prompt_prefix: "search_document: ".to_owned(),
        max_tokens: 64,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    let mut query = document.clone();
    query.prompt_prefix = "search_query: ".to_owned();
    StoreEpoch {
        embedding: EmbeddingEpoch {
            document,
            query,
            alignment_digest: Vec::new(),
        },
        tokenizer: TokenizerConfig::text_default().epoch(),
    }
}

fn declared_identity() -> EpochIdentity {
    declared_store_epoch().identity()
}

fn conflicting_identity() -> EpochIdentity {
    let mut epoch = declared_store_epoch();
    epoch.embedding.query.model_version = "2".to_owned();
    epoch.identity()
}

fn epoch_b_store_epoch() -> StoreEpoch {
    let mut epoch = declared_store_epoch();
    epoch.embedding.document.model_version = "2".to_owned();
    epoch.embedding.document.weights_digest = vec![0xbe, 0x21];
    epoch.embedding.query.model_version = "2".to_owned();
    epoch.embedding.query.weights_digest = vec![0xbe, 0x22];
    epoch.embedding.alignment_digest = vec![0xbe, 0x23];
    epoch
}

fn identity_for(epoch: ModelEpoch) -> EpochIdentity {
    match epoch {
        ModelEpoch::A => declared_identity(),
        ModelEpoch::B => epoch_b_store_epoch().identity(),
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Invariant {
    I1,
    I2,
    I3,
    I4,
    I5,
    I6,
    I7,
    I8,
    I9,
    I10,
    I11,
    I12,
    I13,
    I14,
    I54,
}

impl Invariant {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::I1 => "I1 acked-implies-visible",
            Self::I2 => "I2 deleted-implies-gone",
            Self::I3 => "I3 exactness",
            Self::I4 => "I4 durability-prefix",
            Self::I5 => "I5 filtered-results-honor-predicate",
            Self::I6 => "I6 accounting-conservation",
            Self::I7 => "I7 corruption-never-consumed",
            Self::I8 => "I8 lifecycle",
            Self::I9 => "I9 generation-monotonicity",
            Self::I10 => "I10 purge-proof",
            Self::I11 => "I11 revision-ordering",
            Self::I12 => "I12 responses-name-the-store-epoch",
            Self::I13 => "I13 diagnostics-never-lie",
            Self::I14 => "I14 alias-target-is-complete-and-single-epoch",
            Self::I54 => "I54 deadline-correctness",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Violation {
    pub invariant: Invariant,
    pub seed: u64,
    pub profile: FaultProfile,
    pub op_index: usize,
    pub detail: String,
}

impl Violation {
    #[must_use]
    pub fn report(&self) -> String {
        self.report_for(CampaignKind::Overall)
    }

    #[must_use]
    pub fn report_for(&self, campaign: CampaignKind) -> String {
        format!(
            "VIOLATION {} seed={} profile={} op={}: {} | reproduce: {}",
            self.invariant.label(),
            self.seed,
            self.profile.key(),
            self.op_index,
            self.detail,
            reproduction_for(campaign, self.seed, self.profile)
        )
    }

    #[must_use]
    pub fn json(&self) -> String {
        self.json_for(CampaignKind::Overall)
    }

    #[must_use]
    pub fn json_for(&self, campaign: CampaignKind) -> String {
        format!(
            "{{\"invariant\":\"{}\",\"seed\":{},\"profile\":\"{}\",\"op\":{},\"detail\":\"{}\",\"reproduce\":\"{}\"}}",
            json_escape(self.invariant.label()),
            self.seed,
            self.profile.key(),
            self.op_index,
            json_escape(&self.detail),
            json_escape(&reproduction_for(campaign, self.seed, self.profile))
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunOutcome {
    pub campaign: CampaignKind,
    pub seed: u64,
    pub profile: FaultProfile,
    pub operations: usize,
    pub faults_fired: usize,
    pub scheduled_faults_fired: usize,
    pub feature_faults_scheduled: usize,
    pub feature_faults_fired: usize,
    pub missing_feature_faults: Vec<&'static str>,
    pub graph_searches: usize,
    pub filtered_searches: usize,
    pub filtered_graph_searches: usize,
    pub predicate_searches: usize,
    pub hybrid_searches: usize,
    pub text_documents_ingested: usize,
    pub store_lexical_searches: usize,
    pub store_hybrid_searches: usize,
    pub phrase_searches: usize,
    pub prefix_searches: usize,
    pub fuzzy_searches: usize,
    pub phonetic_encodes: usize,
    pub snippets_built: usize,
    pub hybrid_sealed_vector_documents: usize,
    pub hybrid_lexical_documents: usize,
    pub epoch_preparations: usize,
    pub epoch_alias_switches: usize,
    pub epoch_rollbacks: usize,
    pub epoch_drops: usize,
    pub rejected_dropped_epoch_rollbacks: usize,
    pub coverage: CoverageRegistry,
    pub violations: Vec<Violation>,
    pub program_bytes: Vec<u8>,
    pub faults_bytes: Vec<u8>,
    pub violations_bytes: Vec<u8>,
    pub coverage_bytes: Vec<u8>,
    pub oracle_bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelfTestBug {
    DropAcknowledgedWrite,
    WrongDocument,
    LeakTombstone,
    MisreportGeneration,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Hit {
    doc_id: u32,
    revision: u64,
    score: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct LexicalHit {
    doc_id: u32,
    revision: u64,
    score: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct HybridHit {
    doc_id: u32,
    vector_squared_l2: Option<f64>,
    lexical_bm25: Option<f64>,
    fused_score: f64,
}

#[derive(Clone, Debug, PartialEq)]
struct HybridObservation {
    hits: Vec<HybridHit>,
    generation: u64,
    report_epoch: Option<EpochIdentity>,
}

#[derive(Clone, Debug, PartialEq)]
struct SearchObservation {
    hits: Vec<Hit>,
    generation: u64,
    epoch: Option<EpochIdentity>,
    graph_available: bool,
    graph_segments: usize,
    graph_rescored: usize,
    graph_pruned: usize,
    diagnostics_requested_k: usize,
    diagnostics_returned: usize,
    diagnostics_approximate: bool,
    diagnostics_exact_rescore: bool,
    diagnostics_budget_exhausted: bool,
    diagnostics_counters_match: bool,
    diagnostics_plan_matches_execution: bool,
    expected_exact_rescore: bool,
    expected_budget_exhausted: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct UnfilteredSegmentExecution {
    source: RowSource,
    tier: SegmentTier,
    live_rows: u64,
    row_count: u32,
}

fn observe_unfiltered_segments(store: &Store) -> Result<Vec<UnfilteredSegmentExecution>, String> {
    let snapshot = store.snapshot().map_err(|error| error.to_string())?;
    let mut segments = snapshot
        .segments()
        .iter()
        .map(|segment| {
            let tier = if segment.directory().iter().any(|entry| {
                entry.kind == zeppelin_embed::segment::layout::RegionKind::GraphNodeBlocks.id()
            }) {
                SegmentTier::SealedGraph
            } else {
                SegmentTier::SealedScan
            };
            Ok(UnfilteredSegmentExecution {
                source: RowSource::Sealed(segment.meta().id),
                tier,
                live_rows: segment
                    .alive()
                    .map_err(|error| error.to_string())?
                    .live_count(),
                row_count: segment.meta().row_count,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    segments.sort_unstable_by(|left, right| {
        right
            .row_count
            .cmp(&left.row_count)
            .then_with(|| left.source.cmp(&right.source))
    });
    Ok(segments)
}

fn unfiltered_plan_node_matches(plan: &SegmentPlan) -> bool {
    match (&plan.node, plan.branch) {
        (PlanNode::Scan { source, branch }, SegmentBranch::MaskedScan | SegmentBranch::Pruned) => {
            *source == plan.source && *branch == plan.branch
        }
        (
            PlanNode::Graph {
                source,
                fallback: None,
            },
            SegmentBranch::Graph,
        ) => *source == plan.source,
        _ => false,
    }
}

fn unfiltered_plan_matches_execution(
    plans: &[SegmentPlan],
    kind: SearchKind,
    segments: &[UnfilteredSegmentExecution],
    graph: GraphSearchStats,
) -> bool {
    let mut active_plans = 0_usize;
    let mut sealed_plans = Vec::new();
    let mut traversed = 0_usize;
    let mut pruned = 0_usize;

    for plan in plans {
        if plan.filter_mode != FilterMode::None
            || plan.fallback != PlanFallback::None
            || !unfiltered_plan_node_matches(plan)
        {
            return false;
        }
        match plan.source {
            RowSource::Active => {
                active_plans = active_plans.saturating_add(1);
                if active_plans > 1
                    || plan.tier != SegmentTier::ActiveScan
                    || plan.branch != SegmentBranch::MaskedScan
                    || plan.approximate
                    || plan.ef_requested.is_some()
                    || plan.ef_effective.is_some()
                {
                    return false;
                }
            }
            RowSource::Sealed(_) => {
                traversed =
                    traversed.saturating_add(usize::from(plan.branch == SegmentBranch::Graph));
                pruned = pruned.saturating_add(usize::from(plan.branch == SegmentBranch::Pruned));
                sealed_plans.push(plan);
            }
        }
    }

    if sealed_plans.len() != segments.len()
        || traversed != graph.segments_traversed
        || pruned != graph.segments_pruned_by_bound
    {
        return false;
    }

    sealed_plans
        .into_iter()
        .zip(segments)
        .all(|(plan, executed)| {
            let branch_matches_tier = matches!(
                (kind, executed.tier, plan.branch),
                (SearchKind::Scan, _, SegmentBranch::MaskedScan)
                    | (
                        SearchKind::Auto,
                        SegmentTier::SealedScan,
                        SegmentBranch::MaskedScan
                    )
                    | (
                        SearchKind::Auto | SearchKind::Graph,
                        SegmentTier::SealedGraph,
                        SegmentBranch::Graph | SegmentBranch::Pruned,
                    )
            );
            let graph_shape = match plan.branch {
                SegmentBranch::Graph => {
                    plan.approximate && plan.ef_requested.is_none() && plan.ef_effective.is_some()
                }
                SegmentBranch::MaskedScan | SegmentBranch::Pruned => {
                    !plan.approximate && plan.ef_requested.is_none() && plan.ef_effective.is_none()
                }
                _ => false,
            };
            plan.source == executed.source
                && plan.tier == executed.tier
                && plan.filter_cardinality == executed.live_rows
                && branch_matches_tier
                && graph_shape
        })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MutationAck {
    generation: u64,
    changed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StatsObservation {
    resident_owned_bytes: u64,
    active_segment_bytes: u64,
    wal_bytes: u64,
    cache_bytes: u64,
    temporary_bytes: u64,
    query_pool_bytes: u64,
    mapped_bytes: u64,
    segment_bytes: u64,
    tombstone_bytes: u64,
    open_files: u64,
    active_queries: u64,
    active_snapshot_leases: u64,
}

#[derive(Clone, Copy, Debug)]
struct DocMutation {
    doc_id: u32,
    revision: u64,
    timestamp: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CrashDisposition {
    Unacknowledged,
    Visible,
    Gone,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CrashRecovery {
    generation: u64,
    disposition: CrashDisposition,
}

trait Engine {
    fn open(&mut self) -> Result<(), String>;
    fn ingest(&mut self, documents: &[DocMutation]) -> Result<MutationAck, String>;
    fn epoch_mismatch_probe(&mut self, document: DocMutation) -> Result<(), String>;
    fn prepare_epoch_b(&mut self, documents: &[DocMutation]) -> Result<MutationAck, String>;
    fn switch_epoch(&mut self, epoch: ModelEpoch) -> Result<MutationAck, String>;
    fn drop_epoch_a(&mut self) -> Result<MutationAck, String>;
    fn rollback_dropped_a_probe(&mut self) -> Result<(), String>;
    fn visible_epoch_ids(&mut self) -> Result<Vec<EpochId>, String>;
    fn delete(&mut self, doc_id: u32) -> Result<MutationAck, String>;
    fn seal(&mut self) -> Result<MutationAck, String>;
    fn drop_partition(&mut self, start: i64, end: i64) -> Result<MutationAck, String>;
    fn purge(&mut self, doc_id: u32) -> Result<MutationAck, String>;
    fn maintain(&mut self, bytes: u64) -> Result<MutationAck, String>;
    fn search(
        &mut self,
        query: &[f32],
        k: usize,
        kind: SearchKind,
        seed: u64,
    ) -> Result<SearchObservation, String>;
    fn filtered_search(
        &mut self,
        query: &[f32],
        k: usize,
        maximum_timestamp: i64,
    ) -> Result<SearchObservation, String>;
    fn predicate_search(
        &mut self,
        query: &[f32],
        k: usize,
        predicate: program::PredicateKind,
    ) -> Result<SearchObservation, String>;
    fn lexical_search(&mut self, query_slot: u8, k: usize) -> Result<Vec<LexicalHit>, String>;
    fn hybrid_search(&mut self, query_slot: u8, k: usize) -> Result<HybridObservation, String>;
    fn stats(&mut self) -> Result<StatsObservation, String>;
    fn close(&mut self) -> Result<(), String>;
    fn reopen(&mut self) -> Result<(), String>;
    fn crash_at_boundary(
        &mut self,
        mutation: DocMutation,
        boundary: program::CrashBoundary,
    ) -> Result<CrashRecovery, String>;
    fn generation(&mut self) -> Result<u64, String>;
}

struct RealEngine {
    directory: PathBuf,
    store: Option<Store>,
    vfs: Arc<fault_vfs::ScheduledVfs<StdVfs>>,
    clock: Arc<ManualMonotonicClock>,
    deadline_probe: Option<Deadline>,
    graphs_built: u64,
    open_epoch: ModelEpoch,
}

impl RealEngine {
    fn new(
        directory: PathBuf,
        vfs: Arc<fault_vfs::ScheduledVfs<StdVfs>>,
        clock: Arc<ManualMonotonicClock>,
    ) -> Self {
        Self {
            directory,
            store: None,
            vfs,
            clock,
            deadline_probe: None,
            graphs_built: 0,
            open_epoch: ModelEpoch::A,
        }
    }

    fn without_faults(directory: PathBuf) -> Self {
        Self::new(
            directory,
            Arc::new(fault_vfs::std_scheduled(None)),
            Arc::new(ManualMonotonicClock::new()),
        )
    }

    fn arm_deadline_probe(&mut self) -> Result<(), String> {
        let deadline = Deadline::after_with_test_clock(Duration::from_secs(60), self.clock.clone())
            .map_err(|error| error.to_string())?;
        self.clock.advance(Duration::from_secs(120));
        self.deadline_probe = Some(deadline);
        Ok(())
    }

    fn store(&self) -> Result<&Store, String> {
        self.store
            .as_ref()
            .ok_or_else(|| "store is not open".to_owned())
    }

    fn options(epoch: ModelEpoch) -> OpenOptions {
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_schema(adversarial_schema())
            .with_epoch(match epoch {
                ModelEpoch::A => declared_store_epoch(),
                ModelEpoch::B => epoch_b_store_epoch(),
            })
    }
}

impl Engine for RealEngine {
    fn open(&mut self) -> Result<(), String> {
        if self.store.is_some() {
            return Err("store is already open".to_owned());
        }
        self.store = Some(
            Store::open_with_test_dependencies(
                &self.directory,
                Self::options(self.open_epoch),
                StoreTestDependencies::new(self.vfs.clone(), self.clock.clone()),
            )
            .map_err(|error| error.to_string())?,
        );
        Ok(())
    }

    fn ingest(&mut self, documents: &[DocMutation]) -> Result<MutationAck, String> {
        let batch = documents
            .iter()
            .map(|document| {
                IngestDocument::new(
                    DocumentVersion::new(
                        DocId::new(u128::from(document.doc_id)),
                        Revision::new(document.revision),
                    ),
                    program::vector(document.doc_id, document.revision).to_vec(),
                )
                .with_timestamp(document.timestamp)
                .with_metadata(program::sentinel(document.doc_id))
                .with_text(program::lexical_text(document.doc_id, document.revision))
                .with_columns(adversarial_columns(document.doc_id))
            })
            .collect::<Vec<_>>();
        let ack = self
            .store()?
            .ingest(IngestBatch::new(batch).with_epoch(declared_identity()))
            .map_err(|error| error.to_string())?;
        Ok(MutationAck {
            generation: ack.generation(),
            changed: true,
        })
    }

    fn epoch_mismatch_probe(&mut self, document: DocMutation) -> Result<(), String> {
        let before = self.generation()?;
        let batch = IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(
                    DocId::new(u128::from(document.doc_id)),
                    Revision::new(document.revision),
                ),
                program::vector(document.doc_id, document.revision).to_vec(),
            )
            .with_timestamp(document.timestamp)
            .with_metadata(program::sentinel(document.doc_id))
            .with_text(program::lexical_text(document.doc_id, document.revision))
            .with_columns(adversarial_columns(document.doc_id)),
        ])
        .with_epoch(conflicting_identity());
        match self.store()?.ingest(batch) {
            Err(IngestError::EpochMismatch(_)) => {}
            Err(error) => return Err(format!("epoch probe returned wrong typed error: {error}")),
            Ok(_) => return Err("epoch probe accepted a conflicting identity".to_owned()),
        }
        let after = self.generation()?;
        if after != before {
            return Err(format!(
                "epoch probe changed generation from {before} to {after}"
            ));
        }
        Ok(())
    }

    fn prepare_epoch_b(&mut self, documents: &[DocMutation]) -> Result<MutationAck, String> {
        if documents.is_empty() {
            return Err("cannot prepare epoch B without live documents".to_owned());
        }
        if let Some(store) = self.store.take() {
            store.close().map_err(|error| error.to_string())?;
        }

        let manifest_path = self.directory.join("manifest.ze");
        let mut manifest = load_manifest(self.vfs.as_ref(), &manifest_path, u64::MAX)
            .map_err(|error| error.to_string())?;
        let rows = documents.len();
        let mut vectors = Vec::with_capacity(rows.saturating_mul(program::DIMENSIONS));
        let mut codes = vec![0_u8; rows.saturating_mul(program::DIMENSIONS.div_ceil(2))];
        let mut factors = Vec::<Bit4Factors>::with_capacity(rows);
        let mut columns = ColumnStoreBuilder::new(manifest.schema.clone());
        let mut doc_ids = Vec::with_capacity(rows);
        let mut revisions = Vec::with_capacity(rows);
        for (row, document) in documents.iter().enumerate() {
            let vector = program::vector(document.doc_id, document.revision);
            factors.push(
                quantize_bit4(
                    &vector,
                    &mut codes[row * program::DIMENSIONS.div_ceil(2)
                        ..(row + 1) * program::DIMENSIONS.div_ceil(2)],
                )
                .map_err(|error| error.to_string())?,
            );
            vectors.extend_from_slice(&vector);
            columns
                .push_row(document.timestamp, &[])
                .map_err(|error| error.to_string())?;
            doc_ids.push(DocId::new(u128::from(document.doc_id)));
            revisions.push(Revision::new(document.revision));
        }
        let columns = columns.finish().map_err(|error| error.to_string())?;
        let alive = AliveSet::new(u32::try_from(rows).map_err(|_| "too many epoch rows")?);
        let segment_id = SegmentId::new(
            0x0021_e000_0000_u64.saturating_add(manifest.generation),
            [0xbe; 10],
        );
        let policy = DurabilityPolicy::new(DurabilityMode::Durable, CommitTier::Durable)
            .map_err(|error| error.to_string())?;
        let mut segment = write_segment_with_documents(
            self.vfs.as_ref(),
            &self.directory,
            SegmentBuild {
                id: segment_id,
                scheme: 4,
                dims: program::DIMENSIONS as u32,
                codes: &codes,
                factors: SegmentFactors::Bit4(&factors),
                rescore: &vectors,
                columns: &columns,
                alive: &alive,
            },
            SegmentDocumentVersions {
                doc_ids: &doc_ids,
                revisions: &revisions,
            },
            policy,
        )
        .map_err(|error| error.to_string())?;
        let epoch_b = epoch_b_store_epoch();
        segment.epoch_id = Some(epoch_b.identity().embedding);
        manifest.segments.push(segment);
        if !manifest
            .epochs
            .iter()
            .any(|epoch| epoch.id == epoch_b.identity().embedding)
        {
            manifest.epochs.push(EpochMeta::from(&epoch_b));
        }
        manifest.generation = manifest
            .generation
            .checked_add(1)
            .ok_or_else(|| "manifest generation overflow while preparing epoch B".to_owned())?;
        commit_manifest(self.vfs.as_ref(), &self.directory, &manifest, policy)
            .map_err(|error| error.to_string())?;
        self.open_epoch = ModelEpoch::A;
        self.open()?;
        Ok(MutationAck {
            generation: manifest.generation,
            changed: true,
        })
    }

    fn switch_epoch(&mut self, epoch: ModelEpoch) -> Result<MutationAck, String> {
        let report = self
            .store()?
            .switch_epoch_alias(identity_for(epoch))
            .map_err(|error| error.to_string())?;
        self.open_epoch = epoch;
        Ok(MutationAck {
            generation: report.generation(),
            changed: report.manifest_committed(),
        })
    }

    fn drop_epoch_a(&mut self) -> Result<MutationAck, String> {
        let report = self
            .store()?
            .drop_epoch(declared_identity().embedding)
            .map_err(|error| error.to_string())?;
        Ok(MutationAck {
            generation: report.generation(),
            changed: true,
        })
    }

    fn rollback_dropped_a_probe(&mut self) -> Result<(), String> {
        match self.store()?.switch_epoch_alias(declared_identity()) {
            Err(EpochTransitionError::EpochUnavailable { target })
                if target == declared_identity() =>
            {
                Ok(())
            }
            Err(error) => Err(format!(
                "rollback probe returned wrong typed error: {error}"
            )),
            Ok(_) => Err("rollback probe restored a dropped epoch".to_owned()),
        }
    }

    fn visible_epoch_ids(&mut self) -> Result<Vec<EpochId>, String> {
        self.store()?
            .snapshot()
            .map_err(|error| error.to_string())
            .map(|snapshot| {
                snapshot
                    .segments()
                    .iter()
                    .filter_map(|segment| segment.meta().epoch_id)
                    .collect()
            })
    }

    fn delete(&mut self, doc_id: u32) -> Result<MutationAck, String> {
        let ack = self
            .store()?
            .delete(DeleteBatch::new(vec![DocId::new(u128::from(doc_id))]))
            .map_err(|error| error.to_string())?;
        Ok(MutationAck {
            generation: ack.generation(),
            changed: true,
        })
    }

    fn seal(&mut self) -> Result<MutationAck, String> {
        let generation = self
            .store()?
            .seal_with_cancel(&CancelToken::new())
            .map_err(|error| error.to_string())?;
        Ok(MutationAck {
            generation,
            changed: true,
        })
    }

    fn drop_partition(&mut self, start: i64, end: i64) -> Result<MutationAck, String> {
        let report = self
            .store()?
            .drop_partition(start..end)
            .map_err(|error| error.to_string())?;
        Ok(MutationAck {
            generation: report.generation(),
            changed: !report.is_no_op(),
        })
    }

    fn purge(&mut self, doc_id: u32) -> Result<MutationAck, String> {
        let token = self
            .store()?
            .purge(&[DocId::new(u128::from(doc_id))])
            .map_err(|error| error.to_string())?;
        let report = self
            .store()?
            .await_physical_purge(token)
            .map_err(|error| error.to_string())?;
        Ok(MutationAck {
            generation: report.generation(),
            changed: !report.is_no_op(),
        })
    }

    fn maintain(&mut self, bytes: u64) -> Result<MutationAck, String> {
        let before = self.generation()?;
        let report = self.store()?.maintain_with_test_thresholds(
            MaintenanceBudget {
                wall_time: Duration::from_secs(120),
                bytes,
            },
            TierThresholds {
                graph_min_rows: program::GRAPH_ROWS,
            },
        );
        if let MaintenanceStatus::Failed(error) = &report.status {
            return Err(error.to_string());
        }
        self.graphs_built = self.graphs_built.saturating_add(report.graphs_built);
        let generation = self.generation()?;
        Ok(MutationAck {
            generation,
            changed: generation > before,
        })
    }

    fn search(
        &mut self,
        query: &[f32],
        k: usize,
        kind: SearchKind,
        seed: u64,
    ) -> Result<SearchObservation, String> {
        let execution_segments = observe_unfiltered_segments(self.store()?)?;
        let graph_available = self
            .store()?
            .snapshot()
            .map_err(|error| error.to_string())?
            .segments()
            .iter()
            .any(|segment| {
                segment.directory().iter().any(|entry| {
                    entry.kind == zeppelin_embed::segment::layout::RegionKind::GraphNodeBlocks.id()
                })
            });
        let tier = match kind {
            SearchKind::Scan => SearchTier::Scan,
            SearchKind::Auto => SearchTier::Auto,
            SearchKind::Graph => SearchTier::Graph(
                GraphSearchOptions::new(GraphSearchProfile::SiftClass).with_seed(seed),
            ),
        };
        let control = if let Some(deadline) = self.deadline_probe.take() {
            QueryControl::Deadline(deadline)
        } else {
            QueryControl::Cancel(CancelToken::new())
        };
        let outcome = self
            .store()?
            .search(
                SearchRequest::new(query),
                k,
                SearchOptions::new(ScanOptions {
                    thread_budget: THREAD_BUDGET,
                })
                .with_tier(tier),
                control,
            )
            .map_err(|error| error.to_string())?;
        let hits = outcome
            .candidates
            .iter()
            .map(|candidate| {
                let version = candidate
                    .document()
                    .ok_or_else(|| "search returned a row without document identity".to_owned())?;
                let doc_id = u32::try_from(version.doc_id().get())
                    .map_err(|_| "document id exceeds adversarial vocabulary".to_owned())?;
                Ok(Hit {
                    doc_id,
                    revision: version.revision().get(),
                    score: candidate.score(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let expected_exact_rescore = hits.is_empty()
            || matches!(kind, SearchKind::Graph)
            || (matches!(kind, SearchKind::Auto) && graph_available);
        Ok(SearchObservation {
            hits,
            generation: outcome.generation,
            epoch: outcome.epoch,
            graph_available,
            graph_segments: outcome.graph_stats.segments_traversed,
            graph_rescored: outcome.graph_stats.candidates_rescored,
            graph_pruned: outcome.graph_stats.segments_pruned_by_bound,
            diagnostics_requested_k: outcome.diagnostics.requested_k,
            diagnostics_returned: outcome.diagnostics.returned,
            diagnostics_approximate: outcome.diagnostics.approximate,
            diagnostics_exact_rescore: outcome.diagnostics.exact_rescore,
            diagnostics_budget_exhausted: outcome.diagnostics.budget_exhausted,
            diagnostics_counters_match: outcome.diagnostics.counters.scan == outcome.stats
                && outcome.diagnostics.counters.graph == outcome.graph_stats,
            diagnostics_plan_matches_execution: unfiltered_plan_matches_execution(
                &outcome.diagnostics.plan,
                kind,
                &execution_segments,
                outcome.graph_stats,
            ),
            expected_exact_rescore,
            expected_budget_exhausted: false,
        })
    }

    fn filtered_search(
        &mut self,
        query: &[f32],
        k: usize,
        maximum_timestamp: i64,
    ) -> Result<SearchObservation, String> {
        let predicate = Predicate::Range(RangePredicate {
            column: TIMESTAMP_COLUMN,
            lower: None,
            upper: Some(RangeBound::inclusive(PredicateValue::I64(
                maximum_timestamp,
            ))),
        });
        let outcome = self
            .store()?
            .search_filtered(
                SearchRequest::new(query),
                &predicate,
                k,
                SearchOptions::new(ScanOptions {
                    thread_budget: THREAD_BUDGET,
                }),
                QueryControl::Cancel(CancelToken::new()),
            )
            .map_err(|error| error.to_string())?;
        let graph_available = outcome
            .plans
            .iter()
            .any(|plan| plan.tier == SegmentTier::SealedGraph);
        let filtered_graph_segments = outcome
            .plans
            .iter()
            .filter(|plan| plan.branch == SegmentBranch::FilteredGraph)
            .count();
        let hits = outcome
            .candidates
            .iter()
            .map(|candidate| {
                let version = candidate
                    .document()
                    .ok_or_else(|| "filtered search returned a row without identity".to_owned())?;
                Ok(Hit {
                    doc_id: u32::try_from(version.doc_id().get())
                        .map_err(|_| "filtered document id exceeds vocabulary".to_owned())?,
                    revision: version.revision().get(),
                    score: candidate.score(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let expected_exact_rescore = hits.is_empty() || graph_available;
        let expected_budget_exhausted = outcome.plans.iter().any(|plan| {
            matches!(
                plan.fallback,
                PlanFallback::VisitedBudget | PlanFallback::CandidateShortfall
            )
        });
        Ok(SearchObservation {
            hits,
            generation: outcome.generation,
            epoch: None,
            graph_available,
            graph_segments: filtered_graph_segments,
            graph_rescored: 0,
            graph_pruned: 0,
            diagnostics_requested_k: outcome.diagnostics.requested_k,
            diagnostics_returned: outcome.diagnostics.returned,
            diagnostics_approximate: outcome.diagnostics.approximate,
            diagnostics_exact_rescore: outcome.diagnostics.exact_rescore,
            diagnostics_budget_exhausted: outcome.diagnostics.budget_exhausted,
            diagnostics_counters_match: outcome.diagnostics.counters.scan == outcome.stats,
            diagnostics_plan_matches_execution: outcome.diagnostics.plan == outcome.plans,
            expected_exact_rescore,
            expected_budget_exhausted,
        })
    }

    fn predicate_search(
        &mut self,
        query: &[f32],
        k: usize,
        predicate: program::PredicateKind,
    ) -> Result<SearchObservation, String> {
        let outcome = self
            .store()?
            .search_filtered(
                SearchRequest::new(query),
                &adversarial_predicate(predicate),
                k,
                SearchOptions::new(ScanOptions {
                    thread_budget: THREAD_BUDGET,
                })
                .with_tier(SearchTier::Exact),
                QueryControl::Cancel(CancelToken::new()),
            )
            .map_err(|error| error.to_string())?;
        let hits = outcome
            .candidates
            .iter()
            .map(|candidate| {
                let version = candidate
                    .document()
                    .ok_or_else(|| "predicate search returned a row without identity".to_owned())?;
                Ok(Hit {
                    doc_id: u32::try_from(version.doc_id().get())
                        .map_err(|_| "predicate document id exceeds vocabulary".to_owned())?,
                    revision: version.revision().get(),
                    score: candidate.score(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(SearchObservation {
            hits,
            generation: outcome.generation,
            epoch: None,
            graph_available: false,
            graph_segments: 0,
            graph_rescored: 0,
            graph_pruned: 0,
            diagnostics_requested_k: outcome.diagnostics.requested_k,
            diagnostics_returned: outcome.diagnostics.returned,
            diagnostics_approximate: outcome.diagnostics.approximate,
            diagnostics_exact_rescore: outcome.diagnostics.exact_rescore,
            diagnostics_budget_exhausted: outcome.diagnostics.budget_exhausted,
            diagnostics_counters_match: outcome.diagnostics.counters.scan == outcome.stats,
            diagnostics_plan_matches_execution: outcome.diagnostics.plan == outcome.plans,
            expected_exact_rescore: true,
            expected_budget_exhausted: false,
        })
    }

    fn lexical_search(&mut self, query_slot: u8, k: usize) -> Result<Vec<LexicalHit>, String> {
        let query = TermQuery::flat(
            vec![program::lexical_query(query_slot).to_vec()],
            &[DEFAULT_FIELD],
        );
        self.store()?
            .search_lexical(&query, k, QueryControl::Cancel(CancelToken::new()))
            .map_err(|error| error.to_string())?
            .candidates
            .into_iter()
            .map(|candidate| {
                Ok(LexicalHit {
                    doc_id: u32::try_from(candidate.document.doc_id().get())
                        .map_err(|_| "lexical document id exceeds vocabulary".to_owned())?,
                    revision: candidate.document.revision().get(),
                    score: candidate.score,
                })
            })
            .collect()
    }

    fn hybrid_search(&mut self, query_slot: u8, k: usize) -> Result<HybridObservation, String> {
        let vector = program::query(query_slot);
        let lexical = TermQuery::flat(
            vec![program::lexical_query(query_slot).to_vec()],
            &[DEFAULT_FIELD],
        );
        let outcome = self
            .store()?
            .search_hybrid(
                SearchRequest::new(&vector),
                &lexical,
                &HybridQuery::new(k).with_epoch(declared_identity()),
                SearchOptions::new(ScanOptions {
                    thread_budget: THREAD_BUDGET,
                }),
                QueryControl::Cancel(CancelToken::new()),
            )
            .map_err(|error| error.to_string())?;
        let report_epoch = outcome
            .diagnostics
            .fusion
            .as_ref()
            .and_then(|report| report.epoch);
        let hits = outcome
            .hits
            .into_iter()
            .map(|hit| {
                Ok(HybridHit {
                    doc_id: u32::try_from(hit.key.get())
                        .map_err(|_| "hybrid document id exceeds vocabulary".to_owned())?,
                    vector_squared_l2: hit.vector_squared_l2,
                    lexical_bm25: hit.lexical_bm25,
                    fused_score: hit.fused_score,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(HybridObservation {
            hits,
            generation: outcome.generation,
            report_epoch,
        })
    }

    fn stats(&mut self) -> Result<StatsObservation, String> {
        let stats = self.store()?.stats().map_err(|error| error.to_string())?;
        Ok(StatsObservation {
            resident_owned_bytes: stats.resident_owned_bytes,
            active_segment_bytes: stats.active_segment_bytes,
            wal_bytes: stats.wal_bytes,
            cache_bytes: stats.cache_bytes,
            temporary_bytes: stats.temporary_bytes,
            query_pool_bytes: stats.query_pool_bytes,
            mapped_bytes: stats.mapped_bytes,
            segment_bytes: stats.segment_bytes,
            tombstone_bytes: stats.tombstone_bytes,
            open_files: stats.open_files,
            active_queries: stats.active_queries,
            active_snapshot_leases: stats.active_snapshot_leases,
        })
    }

    fn close(&mut self) -> Result<(), String> {
        self.store()?.close().map_err(|error| error.to_string())
    }

    fn reopen(&mut self) -> Result<(), String> {
        if let Some(store) = self.store.take() {
            store.close().map_err(|error| error.to_string())?;
        }
        self.open()
    }

    fn crash_at_boundary(
        &mut self,
        mutation: DocMutation,
        boundary: program::CrashBoundary,
    ) -> Result<CrashRecovery, String> {
        if let Some(store) = self.store.take() {
            store.close().map_err(|error| error.to_string())?;
        }
        let marker = self.directory.join(".adversarial-crash-ack");
        match std::fs::remove_file(&marker) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("remove stale crash marker: {error}")),
        }
        let output = Command::new(std::env::current_exe().map_err(|error| error.to_string())?)
            .args(["crash_child", "--ignored", "--exact", "--nocapture"])
            .env("ZE_ADV_CRASH_CHILD_PATH", &self.directory)
            .env("ZE_ADV_CRASH_CHILD_MARKER", &marker)
            .env("ZE_ADV_CRASH_CHILD_DOC", mutation.doc_id.to_string())
            .env("ZE_ADV_CRASH_CHILD_REV", mutation.revision.to_string())
            .env("ZE_ADV_CRASH_CHILD_TS", mutation.timestamp.to_string())
            .env("ZE_ADV_CRASH_BOUNDARY", boundary.key())
            .output()
            .map_err(|error| format!("spawn crash child: {error}"))?;
        if output.status.success() {
            return Err("crash child exited successfully instead of crashing".to_owned());
        }
        if output.status.code().is_some() {
            return Err(format!(
                "crash child failed without a process abort: status={} stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        self.open()?;
        let reopened_generation = self.generation()?;
        let marker_value = match std::fs::read_to_string(&marker) {
            Ok(value) => Some(value),
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && boundary == program::CrashBoundary::MidWalGroup =>
            {
                None
            }
            Err(error) => {
                return Err(format!(
                    "crash child did not persist its boundary acknowledgement: {error}"
                ));
            }
        };
        let Some(marker_value) = marker_value else {
            let survived = self
                .search(
                    &program::vector(mutation.doc_id, mutation.revision),
                    1_024,
                    SearchKind::Scan,
                    0,
                )?
                .hits
                .iter()
                .any(|hit| hit.doc_id == mutation.doc_id && hit.revision == mutation.revision);
            return Ok(CrashRecovery {
                generation: reopened_generation,
                disposition: if survived {
                    CrashDisposition::Visible
                } else {
                    CrashDisposition::Unacknowledged
                },
            });
        };
        std::fs::remove_file(&marker).map_err(|error| format!("remove crash marker: {error}"))?;
        let mut fields = marker_value.split_whitespace();
        let generation = fields
            .next()
            .ok_or_else(|| "crash marker omitted generation".to_owned())?
            .parse::<u64>()
            .map_err(|error| format!("parse crash acknowledgement generation: {error}"))?;
        let disposition = match fields.next() {
            Some("visible") => CrashDisposition::Visible,
            Some("gone") => CrashDisposition::Gone,
            other => return Err(format!("invalid crash marker disposition {other:?}")),
        };
        if fields.next().is_some() {
            return Err("crash marker has trailing fields".to_owned());
        }
        Ok(CrashRecovery {
            generation: generation.max(reopened_generation),
            disposition,
        })
    }

    fn generation(&mut self) -> Result<u64, String> {
        self.store()?
            .snapshot()
            .map(|snapshot| snapshot.generation())
            .map_err(|error| error.to_string())
    }
}

struct SelfTestEngine<E> {
    inner: E,
    bug: SelfTestBug,
    last_generation: u64,
    documents: BTreeMap<u32, DocMutation>,
    leaked: Option<Hit>,
}

impl<E> SelfTestEngine<E> {
    fn new(inner: E, bug: SelfTestBug) -> Self {
        Self {
            inner,
            bug,
            last_generation: 0,
            documents: BTreeMap::new(),
            leaked: None,
        }
    }
}

impl<E: Engine> Engine for SelfTestEngine<E> {
    fn open(&mut self) -> Result<(), String> {
        self.inner.open()?;
        self.last_generation = self.inner.generation()?;
        Ok(())
    }

    fn ingest(&mut self, documents: &[DocMutation]) -> Result<MutationAck, String> {
        for document in documents {
            self.documents.insert(document.doc_id, *document);
        }
        if self.bug == SelfTestBug::DropAcknowledgedWrite {
            self.last_generation = self.last_generation.saturating_add(1);
            return Ok(MutationAck {
                generation: self.last_generation,
                changed: true,
            });
        }
        let mut ack = self.inner.ingest(documents)?;
        if self.bug == SelfTestBug::MisreportGeneration {
            ack.generation = self.last_generation;
        } else {
            self.last_generation = ack.generation;
        }
        Ok(ack)
    }

    fn epoch_mismatch_probe(&mut self, document: DocMutation) -> Result<(), String> {
        self.inner.epoch_mismatch_probe(document)
    }

    fn prepare_epoch_b(&mut self, documents: &[DocMutation]) -> Result<MutationAck, String> {
        self.inner.prepare_epoch_b(documents)
    }

    fn switch_epoch(&mut self, epoch: ModelEpoch) -> Result<MutationAck, String> {
        self.inner.switch_epoch(epoch)
    }

    fn drop_epoch_a(&mut self) -> Result<MutationAck, String> {
        self.inner.drop_epoch_a()
    }

    fn rollback_dropped_a_probe(&mut self) -> Result<(), String> {
        self.inner.rollback_dropped_a_probe()
    }

    fn visible_epoch_ids(&mut self) -> Result<Vec<EpochId>, String> {
        self.inner.visible_epoch_ids()
    }

    fn delete(&mut self, doc_id: u32) -> Result<MutationAck, String> {
        let ack = self.inner.delete(doc_id)?;
        self.last_generation = ack.generation;
        if self.bug == SelfTestBug::LeakTombstone {
            let document = self
                .documents
                .get(&doc_id)
                .copied()
                .ok_or_else(|| "self-test leak target was never ingested".to_owned())?;
            self.leaked = Some(Hit {
                doc_id,
                revision: document.revision,
                score: 0.0,
            });
        }
        Ok(ack)
    }

    fn seal(&mut self) -> Result<MutationAck, String> {
        self.inner.seal()
    }

    fn drop_partition(&mut self, start: i64, end: i64) -> Result<MutationAck, String> {
        self.inner.drop_partition(start, end)
    }

    fn purge(&mut self, doc_id: u32) -> Result<MutationAck, String> {
        self.inner.purge(doc_id)
    }

    fn maintain(&mut self, bytes: u64) -> Result<MutationAck, String> {
        self.inner.maintain(bytes)
    }

    fn search(
        &mut self,
        query: &[f32],
        k: usize,
        kind: SearchKind,
        seed: u64,
    ) -> Result<SearchObservation, String> {
        let mut observed = self.inner.search(query, k, kind, seed)?;
        if self.bug == SelfTestBug::WrongDocument
            && let Some(first) = observed.hits.first_mut()
        {
            first.doc_id = 4_000_000_001;
        }
        if self.bug == SelfTestBug::LeakTombstone
            && let Some(leaked) = self.leaked
        {
            observed.hits.push(leaked);
        }
        Ok(observed)
    }

    fn filtered_search(
        &mut self,
        query: &[f32],
        k: usize,
        maximum_timestamp: i64,
    ) -> Result<SearchObservation, String> {
        self.inner.filtered_search(query, k, maximum_timestamp)
    }

    fn predicate_search(
        &mut self,
        query: &[f32],
        k: usize,
        predicate: program::PredicateKind,
    ) -> Result<SearchObservation, String> {
        self.inner.predicate_search(query, k, predicate)
    }

    fn lexical_search(&mut self, query_slot: u8, k: usize) -> Result<Vec<LexicalHit>, String> {
        self.inner.lexical_search(query_slot, k)
    }

    fn hybrid_search(&mut self, query_slot: u8, k: usize) -> Result<HybridObservation, String> {
        self.inner.hybrid_search(query_slot, k)
    }

    fn stats(&mut self) -> Result<StatsObservation, String> {
        self.inner.stats()
    }

    fn close(&mut self) -> Result<(), String> {
        self.inner.close()
    }

    fn reopen(&mut self) -> Result<(), String> {
        self.inner.reopen()
    }

    fn crash_at_boundary(
        &mut self,
        mutation: DocMutation,
        boundary: program::CrashBoundary,
    ) -> Result<CrashRecovery, String> {
        self.inner.crash_at_boundary(mutation, boundary)
    }

    fn generation(&mut self) -> Result<u64, String> {
        self.inner.generation()
    }
}

pub fn run_self_test(bug: SelfTestBug) -> Violation {
    let (seed, expected) = match bug {
        SelfTestBug::DropAcknowledgedWrite => (90_001, Invariant::I1),
        SelfTestBug::WrongDocument => (90_002, Invariant::I3),
        SelfTestBug::LeakTombstone => (90_003, Invariant::I2),
        SelfTestBug::MisreportGeneration => (90_004, Invariant::I9),
    };
    let directory = tempfile::tempdir().expect("self-test store directory");
    let real = RealEngine::without_faults(directory.path().to_path_buf());
    let mut engine = SelfTestEngine::new(real, bug);
    engine.open().expect("self-test open");
    let mut model = Model::default();
    let first = DocMutation {
        doc_id: 1,
        revision: 1,
        timestamp: 10,
    };
    let previous = engine.generation().expect("self-test generation");
    let ack = engine.ingest(&[first]).expect("self-test ingest");
    model.acknowledge(first.doc_id, first.revision, first.timestamp);
    if bug == SelfTestBug::MisreportGeneration {
        return generation_violation(seed, FaultProfile::None, 1, previous, ack)
            .expect("misreported generation must trip I9");
    }
    if bug == SelfTestBug::WrongDocument {
        let second = DocMutation {
            doc_id: 2,
            revision: 1,
            timestamp: 11,
        };
        engine.ingest(&[second]).expect("second self-test ingest");
        model.acknowledge(second.doc_id, second.revision, second.timestamp);
    }
    if bug == SelfTestBug::LeakTombstone {
        engine.delete(first.doc_id).expect("self-test delete");
        model.delete(first.doc_id);
    }
    let query = program::query(0);
    let observed = engine
        .search(
            &query,
            if bug == SelfTestBug::WrongDocument {
                1
            } else {
                model.len()
            },
            SearchKind::Scan,
            seed,
        )
        .expect("self-test search");
    let violations = check_search(
        seed,
        FaultProfile::None,
        3,
        &model,
        &query,
        if bug == SelfTestBug::WrongDocument {
            1
        } else {
            model.len()
        },
        SearchKind::Scan,
        &observed,
        false,
    );
    violations
        .into_iter()
        .find(|violation| violation.invariant == expected)
        .unwrap_or_else(|| panic!("planted {bug:?} did not trip {expected:?}"))
}

pub fn run_program(
    seed: u64,
    profile: FaultProfile,
    artifact_root: &Path,
) -> Result<RunOutcome, String> {
    run_program_for(CampaignKind::Overall, seed, profile, artifact_root)
}

pub fn run_program_for(
    campaign: CampaignKind,
    seed: u64,
    profile: FaultProfile,
    artifact_root: &Path,
) -> Result<RunOutcome, String> {
    run_program_for_with_clock(
        campaign,
        seed,
        profile,
        artifact_root,
        Arc::new(ManualMonotonicClock::new()),
    )
}

fn run_program_with_clock(
    seed: u64,
    profile: FaultProfile,
    artifact_root: &Path,
    clock: Arc<ManualMonotonicClock>,
) -> Result<RunOutcome, String> {
    run_program_for_with_clock(CampaignKind::Overall, seed, profile, artifact_root, clock)
}

fn run_program_for_with_clock(
    campaign: CampaignKind,
    seed: u64,
    profile: FaultProfile,
    artifact_root: &Path,
    clock: Arc<ManualMonotonicClock>,
) -> Result<RunOutcome, String> {
    let program = Program::generate_for(campaign, seed);
    let artifacts = RunArtifacts::create_for(artifact_root, campaign, seed, profile)?;
    let program_bytes = artifacts.write_program(&program)?;
    let reproduction = reproduction_for(campaign, seed, profile);
    artifacts.write_reproduction(&reproduction)?;
    let _ = artifacts.write_episode_metadata(campaign, seed, profile, &reproduction)?;
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let scheduled_event = fault_vfs::scheduled_event_for_program(seed, profile, &program);
    let mut fault_plan =
        FaultPlan::for_program(campaign, seed, profile, &program, scheduled_event.clone());
    let scheduled_vfs = Arc::new(fault_vfs::std_scheduled(scheduled_event));
    let mut engine = RealEngine::new(
        directory.path().to_path_buf(),
        Arc::clone(&scheduled_vfs),
        Arc::clone(&clock),
    );
    let mut model = Model::default();
    let mut violations = Vec::new();
    let mut faults = Vec::<FaultEvent>::new();
    let mut coverage = CoverageRegistry::default();
    let mut oracle_records = Vec::<OracleRecord>::new();
    coverage.hit(format!("fault.profile.{}", profile.key()));
    let mut executed_operations = 0_usize;
    let mut last_generation = 0_u64;
    let mut content_fault_fired = false;
    let mut clock_faults_fired = 0_usize;
    let mut graph_searches = 0_usize;
    let mut filtered_searches = 0_usize;
    let mut filtered_graph_searches = 0_usize;
    let mut predicate_searches = 0_usize;
    let mut hybrid_searches = 0_usize;
    let mut text_documents_ingested = 0_usize;
    let mut store_lexical_searches = 0_usize;
    let mut store_hybrid_searches = 0_usize;
    let mut phrase_searches = 0_usize;
    let mut prefix_searches = 0_usize;
    let mut fuzzy_searches = 0_usize;
    let mut phonetic_encodes = 0_usize;
    let mut snippets_built = 0_usize;
    let mut hybrid_sealed_vector_documents = 0_usize;
    let mut hybrid_lexical_documents = 0_usize;
    let mut epoch_preparations = 0_usize;
    let mut epoch_alias_switches = 0_usize;
    let mut epoch_rollbacks = 0_usize;
    let mut epoch_drops = 0_usize;
    let mut rejected_dropped_epoch_rollbacks = 0_usize;

    for (op_index, op) in program.ops.iter().enumerate() {
        executed_operations = executed_operations.saturating_add(1);
        coverage.hit(format!("attempt.op.{}", op.kind()));
        scheduled_vfs.set_operation(op_index);
        let selected_feature_faults = fault_plan
            .feature
            .iter()
            .filter(|fault| fault.op_index == op_index)
            .map(|event| event.fault)
            .collect::<Vec<_>>();
        let operation_result = match op {
            Op::Open => engine.open().map(|_| None),
            Op::Ingest {
                first_id,
                count,
                revision,
                timestamp,
            } => {
                let documents = (*first_id..first_id.saturating_add(*count))
                    .map(|doc_id| DocMutation {
                        doc_id,
                        revision: *revision,
                        timestamp: *timestamp,
                    })
                    .collect::<Vec<_>>();
                engine.ingest(&documents).map(|ack| {
                    text_documents_ingested =
                        text_documents_ingested.saturating_add(documents.len());
                    for document in &documents {
                        model.acknowledge(document.doc_id, document.revision, document.timestamp);
                    }
                    Some(ack)
                })
            }
            Op::EpochMismatchProbe {
                doc_id,
                revision,
                timestamp,
            } => engine
                .epoch_mismatch_probe(DocMutation {
                    doc_id: *doc_id,
                    revision: *revision,
                    timestamp: *timestamp,
                })
                .map(|()| None),
            Op::PrepareEpochB => {
                let documents = model
                    .live_documents()
                    .into_iter()
                    .map(|(doc_id, revision, timestamp)| DocMutation {
                        doc_id,
                        revision,
                        timestamp,
                    })
                    .collect::<Vec<_>>();
                engine.prepare_epoch_b(&documents).and_then(|ack| {
                    model.prepare_epoch_b();
                    epoch_preparations = epoch_preparations.saturating_add(1);
                    let visible = engine.visible_epoch_ids()?;
                    if let Some(violation) =
                        alias_visibility_violation(seed, profile, op_index, &model, &visible)
                    {
                        violations.push(violation);
                    }
                    Ok(Some(ack))
                })
            }
            Op::SwitchAliasToB => engine.switch_epoch(ModelEpoch::B).and_then(|ack| {
                if !model.switch_epoch(ModelEpoch::B) {
                    return Err("model has no prepared epoch B".to_owned());
                }
                epoch_alias_switches = epoch_alias_switches.saturating_add(1);
                let visible = engine.visible_epoch_ids()?;
                if let Some(violation) =
                    alias_visibility_violation(seed, profile, op_index, &model, &visible)
                {
                    violations.push(violation);
                }
                Ok(Some(ack))
            }),
            Op::RollbackToA => engine.switch_epoch(ModelEpoch::A).and_then(|ack| {
                if !model.switch_epoch(ModelEpoch::A) {
                    return Err("model epoch A was already dropped".to_owned());
                }
                epoch_rollbacks = epoch_rollbacks.saturating_add(1);
                let visible = engine.visible_epoch_ids()?;
                if let Some(violation) =
                    alias_visibility_violation(seed, profile, op_index, &model, &visible)
                {
                    violations.push(violation);
                }
                Ok(Some(ack))
            }),
            Op::DropEpochA => engine.drop_epoch_a().and_then(|ack| {
                if !model.drop_epoch_a() {
                    return Err("model refused to drop epoch A".to_owned());
                }
                epoch_drops = epoch_drops.saturating_add(1);
                let visible = engine.visible_epoch_ids()?;
                if let Some(violation) =
                    alias_visibility_violation(seed, profile, op_index, &model, &visible)
                {
                    violations.push(violation);
                }
                Ok(Some(ack))
            }),
            Op::RollbackDroppedAProbe => engine.rollback_dropped_a_probe().and_then(|()| {
                rejected_dropped_epoch_rollbacks =
                    rejected_dropped_epoch_rollbacks.saturating_add(1);
                let visible = engine.visible_epoch_ids()?;
                if let Some(violation) =
                    alias_visibility_violation(seed, profile, op_index, &model, &visible)
                {
                    violations.push(violation);
                }
                Ok(None)
            }),
            Op::Upsert {
                doc_id,
                revision,
                timestamp,
            }
            | Op::Revise {
                doc_id,
                revision,
                timestamp,
            } => {
                let document = DocMutation {
                    doc_id: *doc_id,
                    revision: *revision,
                    timestamp: *timestamp,
                };
                engine.ingest(&[document]).map(|ack| {
                    text_documents_ingested = text_documents_ingested.saturating_add(1);
                    model.acknowledge(*doc_id, *revision, *timestamp);
                    Some(ack)
                })
            }
            Op::Delete { doc_id } => engine.delete(*doc_id).map(|ack| {
                model.delete(*doc_id);
                Some(ack)
            }),
            Op::Seal => engine.seal().map(|ack| {
                model.seal();
                Some(ack)
            }),
            Op::Maintain { bytes } => engine.maintain(*bytes).map(Some),
            Op::Search { query, k, kind } => {
                let query = program::query(*query);
                let requested = if *k == usize::MAX { model.len() } else { *k };
                engine
                    .search(&query, requested, *kind, seed)
                    .map(|observed| {
                        if observed.graph_segments > 0 {
                            graph_searches = graph_searches.saturating_add(1);
                            coverage.hit("search.graph_traversal");
                        }
                        violations.extend(check_search(
                            seed,
                            profile,
                            op_index,
                            &model,
                            &query,
                            requested,
                            *kind,
                            &observed,
                            content_fault_fired,
                        ));
                        None
                    })
            }
            Op::FilteredSearch {
                query,
                k,
                maximum_timestamp,
            } => {
                let query = program::query(*query);
                let requested = if *k == usize::MAX { model.len() } else { *k };
                engine
                    .filtered_search(&query, requested, *maximum_timestamp)
                    .map(|observed| {
                        filtered_searches = filtered_searches.saturating_add(1);
                        if observed.graph_segments > 0 {
                            filtered_graph_searches = filtered_graph_searches.saturating_add(1);
                            coverage.hit("search.filtered_graph");
                        }
                        violations.extend(check_filtered_search(
                            seed,
                            profile,
                            op_index,
                            &model,
                            &query,
                            requested,
                            *maximum_timestamp,
                            &observed,
                        ));
                        None
                    })
            }
            Op::HybridSearch { query, k } => {
                let requested = if *k == usize::MAX { model.len() } else { *k };
                run_hybrid_search(&mut engine, &model, *query, requested, seed).map(|check| {
                    if let Some(check) = check {
                        hybrid_searches = hybrid_searches.saturating_add(1);
                        store_lexical_searches =
                            store_lexical_searches.saturating_add(check.store_lexical_searches);
                        store_hybrid_searches =
                            store_hybrid_searches.saturating_add(check.store_hybrid_searches);
                        if check.store_lexical_searches > 0 {
                            coverage.hit("store.lexical_search");
                        }
                        if check.store_hybrid_searches > 0 {
                            coverage.hit("store.hybrid_search");
                        }
                        hybrid_sealed_vector_documents = hybrid_sealed_vector_documents
                            .saturating_add(check.sealed_vector_documents);
                        hybrid_lexical_documents =
                            hybrid_lexical_documents.saturating_add(check.lexical_documents);
                        if let Some(detail) = check.mismatch {
                            violations.push(violation(
                                Invariant::I3,
                                seed,
                                profile,
                                op_index,
                                detail,
                            ));
                        }
                    }
                    None
                })
            }
            Op::DeadlineProbe { query } => {
                if matches!(profile, FaultProfile::Clock | FaultProfile::Full) {
                    engine.arm_deadline_probe()?;
                    match engine.search(&program::query(*query), 1, SearchKind::Scan, seed) {
                        Err(error) if error.contains("deadline expired") => {
                            clock_faults_fired = clock_faults_fired.saturating_add(1);
                            Ok(None)
                        }
                        Err(error) => Err(format!(
                            "clock probe returned the wrong typed failure: {error}"
                        )),
                        Ok(_) => Err("jumped clock did not expire the deadline".to_owned()),
                    }
                } else {
                    Ok(None)
                }
            }
            Op::PredicateSearch {
                query,
                k,
                predicate,
            } => {
                let query_vector = program::query(*query);
                let requested = if *k == usize::MAX { model.len() } else { *k };
                engine
                    .predicate_search(&query_vector, requested, *predicate)
                    .map(|observed| {
                        predicate_searches = predicate_searches.saturating_add(1);
                        let expected =
                            model.expected_predicate(&query_vector, requested, *predicate);
                        if let Some(detail) = exact_mismatch(&expected, &observed.hits) {
                            violations.push(violation(
                                Invariant::I5,
                                seed,
                                profile,
                                op_index,
                                format!("{} predicate mismatch: {detail}", predicate.key()),
                            ));
                        }
                        if let Some(violation) = diagnostics_violation(
                            seed,
                            profile,
                            op_index,
                            requested,
                            model
                                .expected_predicate(&query_vector, model.len(), *predicate)
                                .len(),
                            &observed,
                        ) {
                            violations.push(violation);
                        }
                        None
                    })
            }
            Op::FtsExtrasProbe { slot } => run_fts_extras_probe(*slot).map(|()| {
                phrase_searches = phrase_searches.saturating_add(1);
                prefix_searches = prefix_searches.saturating_add(1);
                fuzzy_searches = fuzzy_searches.saturating_add(1);
                phonetic_encodes = phonetic_encodes.saturating_add(1);
                snippets_built = snippets_built.saturating_add(1);
                coverage.hit("fts.phrase");
                coverage.hit("fts.prefix");
                coverage.hit("fts.fuzzy");
                coverage.hit("fts.phonetic");
                coverage.hit("fts.snippet");
                None
            }),
            Op::Feature(operation) => run_campaign_operation(
                &mut engine,
                &model,
                *operation,
                &selected_feature_faults,
                seed,
                profile,
                op_index,
                &mut oracle_records,
                &mut coverage,
            )
            .and_then(|receipts| {
                for receipt in receipts {
                    let event = fault_plan
                        .feature
                        .iter_mut()
                        .find(|event| event.fault == receipt.fault && event.op_index == op_index)
                        .ok_or_else(|| {
                            format!(
                                "unplanned feature-fault receipt {} at op {op_index}",
                                receipt.fault.key()
                            )
                        })?;
                    event.fire_count = event.fire_count.saturating_add(receipt.cardinality);
                    event.fired = event.fire_count == 1;
                    if event.fire_count != 1 {
                        return Err(format!(
                            "feature fault {} fired {} times, expected exactly once",
                            event.fault.key(),
                            event.fire_count
                        ));
                    }
                    coverage.hit(receipt.fault.coverage_key());
                }
                Ok(None)
            }),
            Op::Stats => engine.stats().map(|stats| {
                if let Some(violation) = stats_violation(seed, profile, op_index, stats) {
                    violations.push(violation);
                }
                None
            }),
            Op::Close => engine.close().map(|_| None),
            Op::Reopen => engine.reopen().map(|_| {
                match engine.stats() {
                    Ok(stats) => {
                        if let Some(violation) =
                            lifecycle_stats_violation(seed, profile, op_index, stats)
                        {
                            violations.push(violation);
                        }
                    }
                    Err(error) => violations.push(violation(
                        Invariant::I8,
                        seed,
                        profile,
                        op_index,
                        format!("reopen stats failed: {error}"),
                    )),
                }
                None
            }),
            Op::Crash {
                doc_id,
                revision,
                timestamp,
                boundary,
            } => {
                let crash_event = fault_vfs::audit_crash_seam(seed, op_index, *boundary)?;
                faults.push(crash_event);
                let document = DocMutation {
                    doc_id: *doc_id,
                    revision: *revision,
                    timestamp: *timestamp,
                };
                engine
                    .crash_at_boundary(document, *boundary)
                    .map(|recovery| {
                        match recovery.disposition {
                            CrashDisposition::Unacknowledged => {}
                            CrashDisposition::Visible => {
                                text_documents_ingested = text_documents_ingested.saturating_add(1);
                                model.acknowledge(*doc_id, *revision, *timestamp);
                            }
                            CrashDisposition::Gone => {
                                text_documents_ingested = text_documents_ingested.saturating_add(1);
                                model.purge(*doc_id);
                                if let Some(violation) = purge_proof_violation(
                                    seed,
                                    profile,
                                    op_index,
                                    directory.path(),
                                    *doc_id,
                                ) {
                                    violations.push(violation);
                                }
                            }
                        }
                        match full_scan(&mut engine, &model, seed) {
                            Ok(observed) => {
                                if let Some(violation) = epoch_identity_violation(
                                    seed,
                                    profile,
                                    op_index,
                                    identity_for(model.published_epoch()),
                                    &observed,
                                ) {
                                    violations.push(violation);
                                }
                                if let Some(violation) = durability_prefix_violation(
                                    seed, profile, op_index, &model, &observed,
                                ) {
                                    violations.push(violation);
                                }
                            }
                            Err(error) => violations.push(violation(
                                Invariant::I4,
                                seed,
                                profile,
                                op_index,
                                format!("crash recovery did not open as a clean prefix: {error}"),
                            )),
                        }
                        Some(MutationAck {
                            generation: recovery.generation,
                            changed: recovery.disposition != CrashDisposition::Unacknowledged,
                        })
                    })
            }
            Op::DropPartition { start, end } => engine.drop_partition(*start, *end).map(|ack| {
                if ack.changed {
                    model.drop_partition(*start, *end);
                }
                Some(ack)
            }),
            Op::Purge { doc_id } => engine.purge(*doc_id).map(|ack| {
                if ack.changed {
                    model.purge(*doc_id);
                }
                if let Some(violation) =
                    purge_proof_violation(seed, profile, op_index, directory.path(), *doc_id)
                {
                    violations.push(violation);
                }
                Some(ack)
            }),
        };

        content_fault_fired |= scheduled_vfs
            .event()
            .is_some_and(|event| event.fired && profile == FaultProfile::Content);

        let mut operation_succeeded = false;
        match operation_result {
            Ok(Some(ack)) => {
                operation_succeeded = true;
                if ack.changed {
                    if let Some(generation) =
                        generation_violation(seed, profile, op_index, last_generation, ack)
                    {
                        violations.push(generation);
                    }
                    last_generation = last_generation.max(ack.generation);
                }
            }
            Ok(None) => operation_succeeded = true,
            Err(error) => {
                let injected = scheduled_vfs
                    .event()
                    .is_some_and(|fault| fault.op_index == op_index && fault.fired);
                if injected && profile == FaultProfile::Content {
                    // A persisted content mutation may be impossible to clear.
                    // The typed refusal itself satisfies I7; do not pretend a
                    // poisoned store reached quiescence.
                    break;
                } else if injected {
                    match recover_and_retry_faulted_operation(&mut engine, &mut model, op) {
                        Ok(Some(ack)) => {
                            operation_succeeded = true;
                            last_generation = last_generation.max(ack.generation);
                        }
                        Ok(None) => operation_succeeded = true,
                        Err(recovery_error) => violations.push(violation(
                            Invariant::I8,
                            seed,
                            profile,
                            op_index,
                            format!(
                                "{} fault {error}; automatic reopen/retry failed: {recovery_error}",
                                op.kind()
                            ),
                        )),
                    }
                } else {
                    let invariant = match op {
                        Op::Open | Op::Close | Op::Reopen => Invariant::I8,
                        Op::EpochMismatchProbe { .. } => Invariant::I12,
                        Op::PrepareEpochB
                        | Op::SwitchAliasToB
                        | Op::RollbackToA
                        | Op::DropEpochA
                        | Op::RollbackDroppedAProbe => Invariant::I14,
                        Op::Crash { .. } => Invariant::I4,
                        Op::Stats => Invariant::I6,
                        Op::HybridSearch { .. } => Invariant::I3,
                        Op::DeadlineProbe { .. } => Invariant::I54,
                        Op::FtsExtrasProbe { .. } => Invariant::I3,
                        _ if content_fault_fired => Invariant::I7,
                        _ => Invariant::I1,
                    };
                    violations.push(violation(
                        invariant,
                        seed,
                        profile,
                        op_index,
                        format!("{} returned unexpected error: {error}", op.kind()),
                    ));
                }
            }
        }
        if operation_succeeded {
            record_successful_operation_coverage(&mut coverage, op);
            record_campaign_invariant_checks(&mut coverage, campaign, op);
        }
    }

    let scheduled_event = scheduled_vfs.event();
    let scheduled_faults_fired =
        usize::from(scheduled_event.as_ref().is_some_and(|event| event.fired));
    if let Some(event) = scheduled_event {
        if event.fired {
            coverage.hit(format!("fault.site.{}", event.site.key()));
            coverage.hit(format!("fault.mode.{}", event.mode.key()));
        }
        faults.push(event);
    }
    if clock_faults_fired > 0 {
        coverage.hit("fault.site.clock");
        coverage.hit("fault.mode.latency");
        faults.push(FaultEvent {
            id: format!("clock-{seed}"),
            op_index: program
                .ops
                .iter()
                .position(|op| matches!(op, Op::DeadlineProbe { .. }))
                .unwrap_or(0),
            site: fault_vfs::FaultSite::Clock,
            mode: fault_vfs::FaultMode::Latency,
            nth_match: 1,
            path_contains: None,
            fired: true,
            path: None,
        });
    }
    let faults_fired = faults
        .iter()
        .filter(|fault| fault.fired)
        .count()
        .saturating_add(
            fault_plan
                .feature
                .iter()
                .filter(|fault| fault.fired)
                .count(),
        );
    let faults_bytes = artifacts.write_fault_plan(&faults, &fault_plan.feature)?;
    let violations_bytes = artifacts.write_violations_for(campaign, &violations)?;
    let oracle_bytes = artifacts.write_oracle(&oracle_records)?;
    let mut outcome = RunOutcome {
        campaign,
        seed,
        profile,
        operations: executed_operations,
        faults_fired,
        scheduled_faults_fired,
        feature_faults_scheduled: fault_plan.feature.len(),
        feature_faults_fired: fault_plan
            .feature
            .iter()
            .filter(|fault| fault.fired)
            .count(),
        missing_feature_faults: fault_plan.missing_feature_faults(),
        graph_searches,
        filtered_searches,
        filtered_graph_searches,
        predicate_searches,
        hybrid_searches,
        text_documents_ingested,
        store_lexical_searches,
        store_hybrid_searches,
        phrase_searches,
        prefix_searches,
        fuzzy_searches,
        phonetic_encodes,
        snippets_built,
        hybrid_sealed_vector_documents,
        hybrid_lexical_documents,
        epoch_preparations,
        epoch_alias_switches,
        epoch_rollbacks,
        epoch_drops,
        rejected_dropped_epoch_rollbacks,
        coverage,
        violations,
        program_bytes,
        faults_bytes,
        violations_bytes,
        coverage_bytes: Vec::new(),
        oracle_bytes,
    };
    outcome.coverage_bytes = artifacts.write_coverage(&outcome.coverage)?;
    Ok(outcome)
}

fn record_successful_operation_coverage(coverage: &mut CoverageRegistry, op: &Op) {
    coverage.hit(format!("op.{}", op.kind()));
    match op {
        Op::Ingest { .. } | Op::Upsert { .. } | Op::Revise { .. } => {
            coverage.hit("store.text_ingest");
        }
        Op::Search { kind, .. } => coverage.hit(format!("search.{}", kind.key())),
        Op::PredicateSearch { predicate, .. } => {
            coverage.hit(format!("predicate.{}", predicate.key()));
        }
        Op::Crash { boundary, .. } => {
            coverage.hit("store.text_ingest");
            coverage.hit(format!("crash.boundary.{}", boundary.key()));
        }
        Op::Feature(operation) => {
            coverage.hit(format!(
                "campaign.op.{}.{}",
                operation.campaign().key(),
                operation.key()
            ));
            if operation.campaign() == CampaignKind::FfiBindings {
                coverage.hit("op.ffi_probe");
            }
        }
        _ => {}
    }
}

fn record_campaign_invariant_checks(
    coverage: &mut CoverageRegistry,
    campaign: CampaignKind,
    op: &Op,
) {
    if campaign == CampaignKind::Overall {
        return;
    }
    let spec = CampaignSpec::for_kind(campaign);
    let candidates: &[u8] = match op {
        Op::Search { .. } | Op::HybridSearch { .. } | Op::FtsExtrasProbe { .. } => {
            &[1, 2, 3, 11, 12, 13]
        }
        Op::FilteredSearch { .. } | Op::PredicateSearch { .. } => &[1, 2, 3, 5, 11, 12, 13],
        Op::Crash { .. } => &[4, 12],
        Op::Stats => &[6],
        Op::Reopen => &[8],
        Op::Ingest { .. }
        | Op::Upsert { .. }
        | Op::Revise { .. }
        | Op::Delete { .. }
        | Op::DropPartition { .. }
        | Op::Seal
        | Op::Maintain { .. } => &[9],
        Op::Purge { .. } => &[9, 10],
        Op::EpochMismatchProbe { .. } => &[12],
        Op::PrepareEpochB
        | Op::SwitchAliasToB
        | Op::RollbackToA
        | Op::DropEpochA
        | Op::RollbackDroppedAProbe => &[9, 12, 14],
        Op::Feature(operation) if operation.key() == "format-check" => &[7],
        _ => &[],
    };
    let required = spec.required_invariants();
    for number in candidates {
        let invariant = super::campaign::InvariantId::new(*number);
        if required.contains(&invariant) {
            coverage.hit(invariant.checked_coverage_key());
        }
    }
}

fn run_campaign_operation(
    engine: &mut impl Engine,
    model: &Model,
    operation: super::campaign::FeatureOperation,
    selected_faults: &[super::campaign::FeatureFault],
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    _oracle_records: &mut Vec<OracleRecord>,
    coverage: &mut CoverageRegistry,
) -> Result<Vec<super::campaign::FeatureFaultReceipt>, String> {
    let typed_operation = operation;
    let campaign = typed_operation.campaign();
    let operation = typed_operation.key();
    let observed = full_scan(engine, model, seed)?;
    let exact_violation = durability_prefix_violation(seed, profile, op_index, model, &observed);
    if let Some(violation) = &exact_violation {
        return Err(format!(
            "{} {operation} model oracle failed: {}",
            campaign.key(),
            violation.detail
        ));
    }
    let stats = engine.stats()?;
    let accounting_violation = stats_violation(seed, profile, op_index, stats);
    if let Some(violation) = &accounting_violation {
        return Err(format!(
            "{} {operation} accounting oracle failed: {}",
            campaign.key(),
            violation.detail
        ));
    }
    if campaign == CampaignKind::Fts && operation == "extras" {
        run_fts_extras_probe((seed % 2) as u8)?;
    }
    if matches!(
        typed_operation,
        super::campaign::FeatureOperation::Vector(super::campaign::VectorOperation::KernelParity)
    ) {
        run_kernel_parity_probe(coverage)?;
    }
    if let Some(fault) = selected_faults.first() {
        return Err(format!(
            "feature fault {} has no production-operation injector at {}/{}; refusing isolated probe credit",
            fault.key(),
            campaign.key(),
            typed_operation.key(),
        ));
    }
    let spec = CampaignSpec::for_kind(campaign);
    if let Some(binding) = spec
        .invariant_specs
        .iter()
        .find(|binding| binding.operation == typed_operation)
    {
        return Err(format!(
            "independent oracle {} is not implemented for {}/{}; refusing invariant {} credit",
            binding.checker_id,
            campaign.key(),
            operation,
            binding.invariant.key(),
        ));
    }
    Ok(Vec::new())
}

fn run_kernel_parity_probe(coverage: &mut CoverageRegistry) -> Result<(), String> {
    let left_i8 = [-3_i8, -1, 0, 1, 2, 3, 7, -8];
    let right_i8 = [2_i8, -4, 9, 5, -2, 1, 3, -1];
    let expected_i8 = left_i8
        .iter()
        .zip(right_i8)
        .map(|(left, right)| i32::from(*left) * i32::from(right))
        .sum::<i32>();
    let left_bits = [0b1010_0101_u8, 0b1111_0000];
    let right_bits = [0b0011_1100_u8, 0b1100_0011];
    let expected_hamming = left_bits
        .iter()
        .zip(right_bits)
        .map(|(left, right)| (left ^ right).count_ones())
        .sum::<u32>();
    let left_f16 = [0x3c00_u16, 0x4000];
    let right_f16 = [0x4200_u16, 0x4400];
    let left_f32 = [1.0_f32, 2.0, -3.0, 4.0];
    let right_f32 = [5.0_f32, -2.0, 1.0, 0.5];
    let expected_f32 = left_f32
        .iter()
        .zip(right_f32)
        .map(|(left, right)| left * right)
        .sum::<f32>();
    for variant in zeppelin_embed::kernels::KernelVariant::available() {
        let backend = format!("{:?}", variant.arm()).to_ascii_lowercase();
        if variant.dot_i8(&left_i8, &right_i8) != expected_i8 {
            return Err(format!("{backend} i8 dot diverged from literal scalar sum"));
        }
        if variant.hamming_u1(&left_bits, &right_bits) != expected_hamming {
            return Err(format!(
                "{backend} Hamming diverged from literal xor/popcount"
            ));
        }
        if variant.dot_f16(&left_f16, &right_f16).to_bits() != 11.0_f32.to_bits() {
            return Err(format!("{backend} f16 dot diverged from literal value 11"));
        }
        if variant.dot_f32(&left_f32, &right_f32).to_bits() != expected_f32.to_bits() {
            return Err(format!(
                "{backend} f32 dot diverged from literal scalar sum"
            ));
        }
        coverage.hit(format!("kernel.backend.{backend}"));
    }
    Ok(())
}

fn run_fts_extras_probe(slot: u8) -> Result<(), String> {
    let analyzer = Analyzer::new(Profile::Code.config()).map_err(|error| error.to_string())?;
    let texts = ["alpha bravo charli", "alpha x bravo", "delta echo"];
    let mut segment = SegmentIndex::new();
    for text in texts {
        segment
            .push_document(&analyzer, &Document::with_text(text))
            .map_err(|error| error.to_string())?;
    }
    let slop = u32::from(slot % 2);
    let actual_phrase = phrase::search_segment(
        &segment,
        &PhraseQuery {
            terms: vec![b"alpha".to_vec(), b"bravo".to_vec()],
            slop,
            field: DEFAULT_FIELD,
        },
    )
    .map_err(|error| error.to_string())?;
    let expected_phrase = if slop == 0 { vec![0] } else { vec![0, 1] };
    if actual_phrase != expected_phrase {
        return Err(format!(
            "phrase oracle mismatch: expected={expected_phrase:?} actual={actual_phrase:?}"
        ));
    }

    let vocabulary = [b"alpha".as_slice(), b"alpine", b"bravo", b"charli"];
    let mut dictionary = TermDictionary::default();
    for term in vocabulary {
        dictionary
            .push(term, TermInfo::default())
            .map_err(|error| error.to_string())?;
    }
    let prefix_term = if slot.is_multiple_of(2) { b"al" } else { b"br" };
    let actual_prefix = prefix::search(&dictionary, prefix_term)
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|matched| matched.term)
        .collect::<Vec<_>>();
    let expected_prefix = vocabulary
        .iter()
        .filter(|term| term.starts_with(prefix_term))
        .map(|term| term.to_vec())
        .collect::<Vec<_>>();
    if actual_prefix != expected_prefix {
        return Err(format!(
            "prefix oracle mismatch: expected={expected_prefix:?} actual={actual_prefix:?}"
        ));
    }

    let fuzzy_query = b"alpga";
    let (actual_fuzzy, _) = fuzzy::search(&dictionary, fuzzy_query, 1);
    let actual_fuzzy = actual_fuzzy
        .into_iter()
        .map(|candidate| (candidate.term, candidate.distance))
        .collect::<Vec<_>>();
    let expected_fuzzy = vocabulary
        .iter()
        .filter_map(|term| {
            let distance = naive_edit_distance(fuzzy_query, term);
            (term.first() == fuzzy_query.first() && distance <= 1)
                .then(|| (term.to_vec(), distance as u32))
        })
        .collect::<Vec<_>>();
    if actual_fuzzy != expected_fuzzy {
        return Err(format!(
            "fuzzy oracle mismatch: expected={expected_fuzzy:?} actual={actual_fuzzy:?}"
        ));
    }

    let encoded = phonetic::encode(if slot.is_multiple_of(2) {
        "Smith"
    } else {
        "Schmidt"
    });
    let expected_code = if slot.is_multiple_of(2) { "SM0" } else { "XMT" };
    if encoded != expected_code {
        return Err(format!(
            "phonetic golden mismatch: expected={expected_code} actual={encoded}"
        ));
    }

    let snippet_text = "zero alpha beta omega";
    let terms = vec![b"alpha".to_vec(), b"beta".to_vec()];
    let built = snippet::best_window(&analyzer, snippet_text, &terms, 10, true)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "snippet probe returned no matching window".to_owned())?;
    if built.text(snippet_text) != Some("alpha beta") {
        return Err(format!(
            "snippet window mismatch: expected=\"alpha beta\" actual={:?}",
            built.text(snippet_text)
        ));
    }
    for highlight in &built.highlights {
        let start = usize::try_from(highlight.start).map_err(|_| "snippet start overflow")?;
        let end = usize::try_from(highlight.end).map_err(|_| "snippet end overflow")?;
        let surface = snippet_text
            .get(start..end)
            .ok_or_else(|| "snippet highlight escaped UTF-8 boundaries".to_owned())?;
        if !matches!(surface, "alpha" | "beta") {
            return Err(format!("snippet highlighted non-match {surface:?}"));
        }
    }
    Ok(())
}

fn naive_edit_distance(left: &[u8], right: &[u8]) -> usize {
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    for (left_index, left_byte) in left.iter().enumerate() {
        let mut current = Vec::with_capacity(right.len().saturating_add(1));
        current.push(left_index.saturating_add(1));
        for (right_index, right_byte) in right.iter().enumerate() {
            let deletion = previous[right_index.saturating_add(1)].saturating_add(1);
            let insertion = current[right_index].saturating_add(1);
            let substitution =
                previous[right_index].saturating_add(usize::from(left_byte != right_byte));
            current.push(deletion.min(insertion).min(substitution));
        }
        previous = current;
    }
    previous.last().copied().unwrap_or(left.len())
}

fn recover_and_retry_faulted_operation(
    engine: &mut RealEngine,
    model: &mut Model,
    op: &Op,
) -> Result<Option<MutationAck>, String> {
    engine.reopen()?;
    match op {
        Op::Ingest {
            first_id,
            count,
            revision,
            timestamp,
        } => {
            let documents = (*first_id..first_id.saturating_add(*count))
                .map(|doc_id| DocMutation {
                    doc_id,
                    revision: *revision,
                    timestamp: *timestamp,
                })
                .collect::<Vec<_>>();
            let ack = engine.ingest(&documents)?;
            for document in documents {
                model.acknowledge(document.doc_id, document.revision, document.timestamp);
            }
            Ok(Some(ack))
        }
        Op::Upsert {
            doc_id,
            revision,
            timestamp,
        }
        | Op::Revise {
            doc_id,
            revision,
            timestamp,
        } => {
            let document = DocMutation {
                doc_id: *doc_id,
                revision: *revision,
                timestamp: *timestamp,
            };
            let ack = engine.ingest(&[document])?;
            model.acknowledge(*doc_id, *revision, *timestamp);
            Ok(Some(ack))
        }
        Op::Delete { doc_id } => {
            let ack = engine.delete(*doc_id)?;
            model.delete(*doc_id);
            Ok(Some(ack))
        }
        Op::Seal => match engine.seal() {
            Ok(ack) => {
                model.seal();
                Ok(Some(ack))
            }
            Err(error) if error.contains("active segment is empty") => {
                model.seal();
                Ok(None)
            }
            Err(error) => Err(error),
        },
        Op::DropPartition { start, end } => {
            let ack = engine.drop_partition(*start, *end)?;
            model.drop_partition(*start, *end);
            Ok(Some(ack))
        }
        Op::Purge { doc_id } => {
            let ack = engine.purge(*doc_id)?;
            model.purge(*doc_id);
            Ok(Some(ack))
        }
        Op::Reopen => Ok(None),
        other => Err(format!(
            "no idempotent fault retry is defined for {}",
            other.kind()
        )),
    }
}

struct HybridCheck {
    sealed_vector_documents: usize,
    lexical_documents: usize,
    store_lexical_searches: usize,
    store_hybrid_searches: usize,
    mismatch: Option<String>,
}

fn run_hybrid_search(
    engine: &mut dyn Engine,
    model: &Model,
    query_slot: u8,
    k: usize,
    seed: u64,
) -> Result<Option<HybridCheck>, String> {
    let sealed_vector_documents = model.sealed_document_count();
    if sealed_vector_documents == 0 || model.is_empty() {
        return Ok(None);
    }
    let query_vector = program::query(query_slot);
    let observed = engine.search(&query_vector, model.len(), SearchKind::Auto, seed)?;
    if !observed.graph_available {
        return Ok(None);
    }

    let expected_lexical = model.expected_lexical(query_slot, model.len());
    let actual_lexical = engine.lexical_search(query_slot, model.len())?;
    let lexical_mismatch = (actual_lexical.len() != expected_lexical.len()
        || actual_lexical
            .iter()
            .zip(&expected_lexical)
            .any(|(actual, expected)| {
                actual.doc_id != expected.doc_id
                    || actual.revision != expected.revision
                    || !close_f64(actual.score, expected.score)
            }))
    .then(|| {
        format!("lexical result mismatch: model={expected_lexical:?} engine={actual_lexical:?}")
    });

    let actual = engine.hybrid_search(query_slot, k)?;
    let expected = model.expected_hybrid(&query_vector, query_slot, k);
    let hybrid_mismatch = (actual.hits.len() != expected.len()
        || actual.hits.iter().zip(&expected).any(|(actual, expected)| {
            actual.doc_id != expected.doc_id
                || !close_optional_f64(actual.vector_squared_l2, expected.vector_squared_l2)
                || !close_optional_f64(actual.lexical_bm25, expected.lexical_bm25)
                || !close_f64(actual.fused_score, expected.fused_score)
        }))
    .then(|| {
        format!(
            "hybrid exact result mismatch: model={expected:?} engine={:?}",
            actual.hits
        )
    });
    let mismatch = if let Some(detail) = lexical_mismatch.or(hybrid_mismatch) {
        Some(detail)
    } else if actual.report_epoch != Some(declared_identity()) {
        Some(format!(
            "hybrid report epoch {:?} did not name the declared store epoch",
            actual.report_epoch
        ))
    } else if actual.generation != observed.generation {
        Some(format!(
            "hybrid generation {} did not match vector generation {}",
            actual.generation, observed.generation
        ))
    } else {
        None
    };
    Ok(Some(HybridCheck {
        sealed_vector_documents,
        lexical_documents: model.lexical_documents().len(),
        store_lexical_searches: 1,
        store_hybrid_searches: 1,
        mismatch,
    }))
}

fn close_f64(left: f64, right: f64) -> bool {
    let scale = left.abs().max(right.abs()).max(1.0);
    (left - right).abs() <= f64::EPSILON * 16.0 * scale
}

fn close_optional_f64(left: Option<f64>, right: Option<f64>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => close_f64(left, right),
        (None, None) => true,
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn check_filtered_search(
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    model: &Model,
    query: &[f32],
    k: usize,
    maximum_timestamp: i64,
    observed: &SearchObservation,
) -> Vec<Violation> {
    let mut violations = Vec::new();
    let available = model
        .expected_filtered(query, model.len(), maximum_timestamp)
        .len();
    if let Some(violation) = diagnostics_violation(seed, profile, op_index, k, available, observed)
    {
        violations.push(violation);
    }
    if let Some(hit) = observed.hits.iter().find(|hit| {
        model
            .timestamp(hit.doc_id)
            .is_none_or(|timestamp| timestamp > maximum_timestamp)
    }) {
        violations.push(violation(
            Invariant::I5,
            seed,
            profile,
            op_index,
            format!(
                "filtered query max_ts={maximum_timestamp} returned doc {} at timestamp {:?}",
                hit.doc_id,
                model.timestamp(hit.doc_id)
            ),
        ));
    }
    let expected = if observed.graph_available {
        model.expected_filtered(query, k, maximum_timestamp)
    } else {
        model.expected_filtered_scan(query, k, maximum_timestamp)
    };
    if let Some(detail) = exact_mismatch(&expected, &observed.hits) {
        violations.push(violation(Invariant::I3, seed, profile, op_index, detail));
    }
    violations
}

fn full_scan(
    engine: &mut dyn Engine,
    model: &Model,
    seed: u64,
) -> Result<SearchObservation, String> {
    engine.search(&program::query(3), model.len(), SearchKind::Scan, seed)
}

fn durability_prefix_violation(
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    model: &Model,
    observed: &SearchObservation,
) -> Option<Violation> {
    let returned = observed
        .hits
        .iter()
        .map(|hit| hit.doc_id)
        .collect::<BTreeSet<_>>();
    let missing = model
        .live_ids()
        .difference(&returned)
        .copied()
        .collect::<Vec<_>>();
    (!missing.is_empty()).then(|| {
        violation(
            Invariant::I4,
            seed,
            profile,
            op_index,
            format!("durable crash recovery lost acknowledged ids {missing:?}"),
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn check_search(
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    model: &Model,
    query: &[f32],
    k: usize,
    kind: SearchKind,
    observed: &SearchObservation,
    content_fault_fired: bool,
) -> Vec<Violation> {
    let mut violations = Vec::new();
    if let Some(violation) =
        diagnostics_violation(seed, profile, op_index, k, model.len(), observed)
    {
        violations.push(violation);
    }
    if let Some(violation) = epoch_identity_violation(
        seed,
        profile,
        op_index,
        identity_for(model.published_epoch()),
        observed,
    ) {
        violations.push(violation);
    }
    let forbidden = model.forbidden_ids();
    if let Some(hit) = observed
        .hits
        .iter()
        .find(|hit| forbidden.contains(&hit.doc_id))
    {
        violations.push(violation(
            Invariant::I2,
            seed,
            profile,
            op_index,
            format!(
                "returned forbidden doc {} revision {}",
                hit.doc_id, hit.revision
            ),
        ));
    }
    if let Some(hit) = observed.hits.iter().find(|hit| {
        model
            .revision(hit.doc_id)
            .is_some_and(|revision| hit.revision != revision)
    }) {
        violations.push(violation(
            Invariant::I11,
            seed,
            profile,
            op_index,
            format!(
                "returned doc {} revision {}, highest acknowledged is {}",
                hit.doc_id,
                hit.revision,
                model.revision(hit.doc_id).unwrap_or_default()
            ),
        ));
    }
    if k >= model.len() {
        let returned = observed
            .hits
            .iter()
            .map(|hit| hit.doc_id)
            .collect::<BTreeSet<_>>();
        let missing = model
            .live_ids()
            .difference(&returned)
            .copied()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            violations.push(violation(
                Invariant::I1,
                seed,
                profile,
                op_index,
                format!("acknowledged docs missing from full query: {missing:?}"),
            ));
        }
    }
    let exact = kind == SearchKind::Scan || (kind == SearchKind::Auto && !observed.graph_available);
    let expected = if exact {
        model.expected_scan(query, k)
    } else {
        model.expected(query, k)
    };
    if exact {
        if let Some(detail) = exact_mismatch(&expected, &observed.hits) {
            if model.epoch_b_prepared() {
                violations.push(violation(
                    Invariant::I14,
                    seed,
                    profile,
                    op_index,
                    format!("published epoch results are incomplete or mixed: {detail}"),
                ));
            }
            violations.push(violation(
                Invariant::I3,
                seed,
                profile,
                op_index,
                detail.clone(),
            ));
            if content_fault_fired {
                violations.push(violation(
                    Invariant::I7,
                    seed,
                    profile,
                    op_index,
                    format!("content fault was consumed into a successful wrong result: {detail}"),
                ));
            }
        }
    } else if !expected.is_empty() {
        let expected_ids = expected
            .iter()
            .map(|hit| hit.doc_id)
            .collect::<BTreeSet<_>>();
        let retained = observed
            .hits
            .iter()
            .filter(|hit| expected_ids.contains(&hit.doc_id))
            .count();
        let recall = retained as f64 / expected.len() as f64;
        if recall < 0.80 {
            violations.push(violation(
                Invariant::I3,
                seed,
                profile,
                op_index,
                format!(
                    "approximate graph recall {:.3} below 0.800 ({retained}/{})",
                    recall,
                    expected.len()
                ),
            ));
        }
        let graph_work_missing =
            observed.graph_available && observed.graph_segments == 0 && observed.graph_pruned == 0;
        let graph_rescore_missing = observed.graph_segments != 0 && observed.graph_rescored == 0;
        if graph_work_missing || graph_rescore_missing {
            violations.push(violation(
                Invariant::I7,
                seed,
                profile,
                op_index,
                format!(
                    "graph request did not traverse/rescore a graph: traversed={} rescored={} pruned={} generation={}",
                    observed.graph_segments,
                    observed.graph_rescored,
                    observed.graph_pruned,
                    observed.generation
                ),
            ));
        }
    }
    violations
}

fn diagnostics_violation(
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    requested_k: usize,
    available_matches: usize,
    observed: &SearchObservation,
) -> Option<Violation> {
    let expected_approximate = observed.graph_segments != 0;
    let silent_shortfall = observed.hits.len() < requested_k
        && available_matches >= requested_k
        && !observed.diagnostics_budget_exhausted;
    let mut lies = Vec::new();
    if observed.diagnostics_requested_k != requested_k {
        lies.push(format!(
            "requested_k={} expected {requested_k}",
            observed.diagnostics_requested_k
        ));
    }
    if observed.diagnostics_returned != observed.hits.len() {
        lies.push(format!(
            "returned={} actual {}",
            observed.diagnostics_returned,
            observed.hits.len()
        ));
    }
    if observed.diagnostics_approximate != expected_approximate {
        lies.push(format!(
            "approximate={} expected {expected_approximate}",
            observed.diagnostics_approximate
        ));
    }
    if observed.diagnostics_exact_rescore != observed.expected_exact_rescore {
        lies.push(format!(
            "exact_rescore={} expected {}",
            observed.diagnostics_exact_rescore, observed.expected_exact_rescore
        ));
    }
    if observed.diagnostics_budget_exhausted != observed.expected_budget_exhausted {
        lies.push(format!(
            "budget_exhausted={} expected {}",
            observed.diagnostics_budget_exhausted, observed.expected_budget_exhausted
        ));
    }
    if !observed.diagnostics_counters_match {
        lies.push("composed counters differ from executor counters".to_owned());
    }
    if !observed.diagnostics_plan_matches_execution {
        lies.push("reported plan differs from executor plan".to_owned());
    }
    if silent_shortfall {
        lies.push(format!(
            "silent shortfall returned {} of {requested_k} with {available_matches} matches",
            observed.hits.len()
        ));
    }
    (!lies.is_empty()).then(|| violation(Invariant::I13, seed, profile, op_index, lies.join("; ")))
}

fn epoch_identity_violation(
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    expected: EpochIdentity,
    observed: &SearchObservation,
) -> Option<Violation> {
    (observed.epoch != Some(expected)).then(|| {
        violation(
            Invariant::I12,
            seed,
            profile,
            op_index,
            format!(
                "search response epoch {:?} did not name declared store epoch {expected:?}",
                observed.epoch
            ),
        )
    })
}

fn alias_visibility_violation(
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    model: &Model,
    visible: &[EpochId],
) -> Option<Violation> {
    let expected = identity_for(model.published_epoch()).embedding;
    let empty_with_live_documents = !model.is_empty() && visible.is_empty();
    let mixed = visible.iter().any(|epoch| *epoch != expected);
    (empty_with_live_documents || mixed).then(|| {
        violation(
            Invariant::I14,
            seed,
            profile,
            op_index,
            format!(
                "published alias expected only {expected:?}, visible segment epochs were {visible:?}"
            ),
        )
    })
}

fn exact_mismatch(expected: &[ExpectedHit], observed: &[Hit]) -> Option<String> {
    if expected.len() != observed.len() {
        return Some(format!(
            "exact result length mismatch: model={} engine={}",
            expected.len(),
            observed.len()
        ));
    }
    expected
        .iter()
        .zip(observed)
        .enumerate()
        .find_map(|(rank, (expected, observed))| {
            (expected.doc_id != observed.doc_id
                || expected.revision != observed.revision
                || expected.score.to_bits() != observed.score.to_bits())
            .then(|| {
                format!(
                    "exact rank {rank} mismatch: model=({},r{},{}:{:08x}) engine=({},r{},{}:{:08x})",
                    expected.doc_id,
                    expected.revision,
                    expected.score,
                    expected.score.to_bits(),
                    observed.doc_id,
                    observed.revision,
                    observed.score,
                    observed.score.to_bits()
                )
            })
        })
}

fn generation_violation(
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    previous: u64,
    ack: MutationAck,
) -> Option<Violation> {
    (ack.generation <= previous).then(|| {
        violation(
            Invariant::I9,
            seed,
            profile,
            op_index,
            format!(
                "mutation acknowledged generation {}, previous acknowledged generation was {previous}",
                ack.generation
            ),
        )
    })
}

fn stats_violation(
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    stats: StatsObservation,
) -> Option<Violation> {
    let contradiction = stats.active_segment_bytes > stats.resident_owned_bytes
        || stats.wal_bytes > stats.resident_owned_bytes
        || stats.cache_bytes > stats.resident_owned_bytes
        || stats.temporary_bytes > stats.resident_owned_bytes
        || stats.query_pool_bytes > stats.resident_owned_bytes
        || stats.mapped_bytes != stats.segment_bytes
        || stats.tombstone_bytes > stats.active_segment_bytes
        || stats.active_queries != 0;
    contradiction.then(|| {
        violation(
            Invariant::I6,
            seed,
            profile,
            op_index,
            format!("stats conservation contradiction: {stats:?}"),
        )
    })
}

fn lifecycle_stats_violation(
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    stats: StatsObservation,
) -> Option<Violation> {
    (stats.open_files != 2 || stats.active_queries != 0 || stats.active_snapshot_leases != 0)
        .then(|| {
            violation(
                Invariant::I8,
                seed,
                profile,
                op_index,
                format!(
                    "reopened lifecycle counters are not quiescent: open_files={} active_queries={} snapshot_leases={}",
                    stats.open_files, stats.active_queries, stats.active_snapshot_leases
                ),
            )
        })
}

fn purge_proof_violation(
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    directory: &Path,
    doc_id: u32,
) -> Option<Violation> {
    let needle = program::sentinel(doc_id);
    let mut stack = vec![directory.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        if metadata.is_dir() {
            let Ok(entries) = std::fs::read_dir(&path) else {
                continue;
            };
            stack.extend(entries.filter_map(Result::ok).map(|entry| entry.path()));
        } else if let Ok(bytes) = std::fs::read(&path)
            && contains_bytes(&bytes, &needle)
        {
            return Some(violation(
                Invariant::I10,
                seed,
                profile,
                op_index,
                format!("purged sentinel remains in {}", path.display()),
            ));
        }
    }
    None
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn violation(
    invariant: Invariant,
    seed: u64,
    profile: FaultProfile,
    op_index: usize,
    detail: String,
) -> Violation {
    Violation {
        invariant,
        seed,
        profile,
        op_index,
        detail,
    }
}

#[must_use]
pub fn reproduction(seed: u64, profile: FaultProfile) -> String {
    reproduction_for(CampaignKind::Overall, seed, profile)
}

#[must_use]
pub fn reproduction_for(campaign: CampaignKind, seed: u64, profile: FaultProfile) -> String {
    if campaign == CampaignKind::Overall {
        return format!(
            "ZE_ADV_SEED={seed} ZE_ADV_PROFILE={} cargo test -p zeppelin-embed-workspace-tests --test adversarial_tests run -- --ignored --exact --nocapture",
            profile.key()
        );
    }
    format!(
        "ZE_ADV_CAMPAIGN={} ZE_ADV_SEED={seed} ZE_ADV_PROFILE={} cargo test -p zeppelin-embed-workspace-tests --test adversarial_tests run -- --ignored --exact --nocapture",
        campaign.key(),
        profile.key()
    )
}

fn json_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

pub fn planted_counterexample(invariant: Invariant) -> Violation {
    let seed = 91_000 + invariant as u64;
    let query = program::query(0);
    let observation = |hits: Vec<Hit>, graph_available, graph_segments, graph_rescored| {
        let returned = hits.len();
        let exact_rescore = returned == 0 || graph_segments != 0;
        SearchObservation {
            hits,
            generation: 1,
            epoch: Some(declared_identity()),
            graph_available,
            graph_segments,
            graph_rescored,
            graph_pruned: 0,
            diagnostics_requested_k: returned,
            diagnostics_returned: returned,
            diagnostics_approximate: graph_segments != 0,
            diagnostics_exact_rescore: exact_rescore,
            diagnostics_budget_exhausted: false,
            diagnostics_counters_match: true,
            diagnostics_plan_matches_execution: true,
            expected_exact_rescore: exact_rescore,
            expected_budget_exhausted: false,
        }
    };
    match invariant {
        Invariant::I5 => {
            let mut model = Model::default();
            model.acknowledge(1, 1, 10);
            let expected = model.expected_filtered(&query, 1, 10);
            let hit = expected.first().copied().expect("one filtered hit");
            let observed = observation(
                vec![Hit {
                    doc_id: hit.doc_id,
                    revision: hit.revision,
                    score: hit.score,
                }],
                false,
                0,
                0,
            );
            check_filtered_search(seed, FaultProfile::None, 1, &model, &query, 1, 9, &observed)
                .into_iter()
                .find(|violation| violation.invariant == Invariant::I5)
                .expect("filtered-out row must trip I5")
        }
        Invariant::I1 | Invariant::I3 | Invariant::I4 | Invariant::I7 | Invariant::I11 => {
            let mut model = Model::default();
            model.acknowledge(1, 2, 10);
            let expected = model.expected_scan(&query, 1);
            let expected_hit = expected.first().copied().expect("one expected hit");
            let observed = match invariant {
                Invariant::I1 | Invariant::I4 => observation(Vec::new(), false, 0, 0),
                Invariant::I3 => observation(
                    vec![Hit {
                        doc_id: 2,
                        revision: expected_hit.revision,
                        score: expected_hit.score,
                    }],
                    false,
                    0,
                    0,
                ),
                Invariant::I7 => {
                    let exact = model.expected(&query, 1);
                    let exact_hit = exact.first().copied().expect("one exact hit");
                    observation(
                        vec![Hit {
                            doc_id: exact_hit.doc_id,
                            revision: exact_hit.revision,
                            score: exact_hit.score,
                        }],
                        true,
                        0,
                        0,
                    )
                }
                Invariant::I11 => observation(
                    vec![Hit {
                        doc_id: expected_hit.doc_id,
                        revision: 1,
                        score: expected_hit.score,
                    }],
                    false,
                    0,
                    0,
                ),
                _ => unreachable!("grouped invariant is exhaustive"),
            };
            if invariant == Invariant::I4 {
                durability_prefix_violation(seed, FaultProfile::Crash, 1, &model, &observed)
                    .expect("missing durable id must trip I4")
            } else {
                let kind = if invariant == Invariant::I7 {
                    SearchKind::Graph
                } else {
                    SearchKind::Scan
                };
                check_search(
                    seed,
                    FaultProfile::None,
                    1,
                    &model,
                    &query,
                    1,
                    kind,
                    &observed,
                    false,
                )
                .into_iter()
                .find(|violation| violation.invariant == invariant)
                .unwrap_or_else(|| panic!("planted counterexample did not trip {invariant:?}"))
            }
        }
        Invariant::I2 => {
            let mut model = Model::default();
            model.acknowledge(1, 1, 10);
            model.delete(1);
            let observed = observation(
                vec![Hit {
                    doc_id: 1,
                    revision: 1,
                    score: 0.0,
                }],
                false,
                0,
                0,
            );
            check_search(
                seed,
                FaultProfile::None,
                1,
                &model,
                &query,
                0,
                SearchKind::Scan,
                &observed,
                false,
            )
            .into_iter()
            .find(|violation| violation.invariant == invariant)
            .expect("forbidden returned id must trip I2")
        }
        Invariant::I6 => {
            let stats = StatsObservation {
                resident_owned_bytes: 1,
                active_segment_bytes: 2,
                wal_bytes: 0,
                cache_bytes: 0,
                temporary_bytes: 0,
                query_pool_bytes: 0,
                mapped_bytes: 0,
                segment_bytes: 0,
                tombstone_bytes: 0,
                open_files: 2,
                active_queries: 0,
                active_snapshot_leases: 0,
            };
            stats_violation(seed, FaultProfile::None, 1, stats)
                .expect("contradictory stats must trip I6")
        }
        Invariant::I8 => {
            let stats = StatsObservation {
                resident_owned_bytes: 0,
                active_segment_bytes: 0,
                wal_bytes: 0,
                cache_bytes: 0,
                temporary_bytes: 0,
                query_pool_bytes: 0,
                mapped_bytes: 0,
                segment_bytes: 0,
                tombstone_bytes: 0,
                open_files: 3,
                active_queries: 0,
                active_snapshot_leases: 0,
            };
            lifecycle_stats_violation(seed, FaultProfile::None, 1, stats)
                .expect("leaked descriptor must trip I8")
        }
        Invariant::I9 => generation_violation(
            seed,
            FaultProfile::None,
            1,
            7,
            MutationAck {
                generation: 7,
                changed: true,
            },
        )
        .expect("stale generation must trip I9"),
        Invariant::I10 => {
            let directory = tempfile::tempdir().expect("I10 counterexample directory");
            std::fs::write(directory.path().join("wal.ze"), program::sentinel(1))
                .expect("I10 counterexample bytes");
            purge_proof_violation(seed, FaultProfile::None, 1, directory.path(), 1)
                .expect("persisted purge sentinel must trip I10")
        }
        Invariant::I12 => {
            let observed = SearchObservation {
                hits: Vec::new(),
                generation: 1,
                epoch: None,
                graph_available: false,
                graph_segments: 0,
                graph_rescored: 0,
                graph_pruned: 0,
                diagnostics_requested_k: 0,
                diagnostics_returned: 0,
                diagnostics_approximate: false,
                diagnostics_exact_rescore: true,
                diagnostics_budget_exhausted: false,
                diagnostics_counters_match: true,
                diagnostics_plan_matches_execution: true,
                expected_exact_rescore: true,
                expected_budget_exhausted: false,
            };
            epoch_identity_violation(seed, FaultProfile::None, 1, declared_identity(), &observed)
                .expect("missing response epoch must trip I12")
        }
        Invariant::I13 => {
            let observed = SearchObservation {
                diagnostics_returned: 1,
                ..observation(Vec::new(), false, 0, 0)
            };
            diagnostics_violation(seed, FaultProfile::None, 1, 0, 0, &observed)
                .expect("lying returned count must trip I13")
        }
        Invariant::I14 => {
            let mut model = Model::default();
            model.acknowledge(1, 1, 10);
            model.prepare_epoch_b();
            assert!(model.switch_epoch(ModelEpoch::B));
            alias_visibility_violation(
                seed,
                FaultProfile::None,
                1,
                &model,
                &[declared_identity().embedding],
            )
            .expect("mixed published epoch segments must trip I14")
        }
        Invariant::I54 => violation(
            Invariant::I54,
            seed,
            FaultProfile::Clock,
            1,
            "deadline was evaluated against a different monotonic clock".to_owned(),
        ),
    }
}

pub fn crash_child_from_env() -> Result<(), String> {
    let directory = PathBuf::from(
        std::env::var("ZE_ADV_CRASH_CHILD_PATH")
            .map_err(|_| "ZE_ADV_CRASH_CHILD_PATH is unset".to_owned())?,
    );
    let marker = PathBuf::from(
        std::env::var("ZE_ADV_CRASH_CHILD_MARKER")
            .map_err(|_| "ZE_ADV_CRASH_CHILD_MARKER is unset".to_owned())?,
    );
    let parse = |name: &str| -> Result<u64, String> {
        std::env::var(name)
            .map_err(|_| format!("{name} is unset"))?
            .parse::<u64>()
            .map_err(|error| format!("parse {name}: {error}"))
    };
    let doc_id = u32::try_from(parse("ZE_ADV_CRASH_CHILD_DOC")?)
        .map_err(|_| "crash child doc id exceeds u32".to_owned())?;
    let revision = parse("ZE_ADV_CRASH_CHILD_REV")?;
    let timestamp = parse("ZE_ADV_CRASH_CHILD_TS")? as i64;
    let boundary = program::CrashBoundary::from_key(
        &std::env::var("ZE_ADV_CRASH_BOUNDARY")
            .map_err(|_| "ZE_ADV_CRASH_BOUNDARY is unset".to_owned())?,
    )?;
    let crash_vfs = Arc::new(fault_vfs::ProcessCrashVfs::new(StdVfs, boundary));
    let store = Store::open_with_test_dependencies(
        &directory,
        RealEngine::options(ModelEpoch::A),
        StoreTestDependencies::new(crash_vfs.clone(), Arc::new(ManualMonotonicClock::new())),
    )
    .map_err(|error| error.to_string())?;
    let document = IngestDocument::new(
        DocumentVersion::new(DocId::new(u128::from(doc_id)), Revision::new(revision)),
        program::vector(doc_id, revision).to_vec(),
    )
    .with_timestamp(timestamp)
    .with_metadata(program::sentinel(doc_id))
    .with_text(program::lexical_text(doc_id, revision))
    .with_columns(adversarial_columns(doc_id));
    if boundary == program::CrashBoundary::MidWalGroup {
        crash_vfs.arm();
        let _ = store
            .ingest(IngestBatch::new(vec![document]).with_epoch(declared_identity()))
            .map_err(|error| error.to_string())?;
        return Err("mid-WAL crash boundary did not abort".to_owned());
    }

    let ack = store
        .ingest(IngestBatch::new(vec![document]).with_epoch(declared_identity()))
        .map_err(|error| error.to_string())?;
    std::fs::write(&marker, format!("{} visible", ack.generation()))
        .map_err(|error| format!("write crash acknowledgement: {error}"))?;

    if boundary == program::CrashBoundary::MidPurge {
        let deletion = store
            .delete(DeleteBatch::new(vec![DocId::new(u128::from(doc_id))]))
            .map_err(|error| error.to_string())?;
        let token = store
            .purge(&[DocId::new(u128::from(doc_id))])
            .map_err(|error| error.to_string())?;
        std::fs::write(&marker, format!("{} gone", deletion.generation()))
            .map_err(|error| format!("write crash purge acknowledgement: {error}"))?;
        crash_vfs.arm();
        let _ = store
            .await_physical_purge(token)
            .map_err(|error| error.to_string())?;
        return Err("mid-purge crash boundary did not abort".to_owned());
    }

    crash_vfs.arm();
    let _ = store
        .seal_with_cancel(&CancelToken::new())
        .map_err(|error| error.to_string())?;
    Err(format!("{} crash boundary did not abort", boundary.key()))
}

#[allow(dead_code)]
fn _keep_tempdir_type_visible(_: &TempDir) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_probe_is_independent_of_episode_wall_time() {
        let artifacts = tempfile::tempdir().expect("aged deadline probe artifacts");
        let aged_origin = std::time::Instant::now()
            .checked_sub(Duration::from_secs(180))
            .expect("180 seconds fits in the monotonic clock");
        let outcome = run_program_with_clock(
            0,
            FaultProfile::Clock,
            artifacts.path(),
            Arc::new(ManualMonotonicClock::starting_at(aged_origin)),
        )
        .expect("aged deadline probe run");

        assert!(
            outcome.violations.is_empty(),
            "an aged episode must still observe its injected deadline timeout: {:?}",
            outcome.violations
        );
    }
}
