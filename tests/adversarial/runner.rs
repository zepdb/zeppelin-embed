use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use tempfile::TempDir;
use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, EpochId, EpochIdentity,
    EpochTransitionError, Normalization, StoreEpoch,
};
use zeppelin_embed::fts::bm25::Bm25Params;
use zeppelin_embed::fts::index::{DEFAULT_FIELD, Document, LexicalIndex, SegmentIndex};
use zeppelin_embed::fts::search::{TermQuery, search as lexical_search};
use zeppelin_embed::fts::tokenizer::TokenizerConfig;
use zeppelin_embed::fts::tokenizer::{Analyzer, Profile};
use zeppelin_embed::fusion::{HybridQuery, LexicalCandidate, VectorCandidate, fuse};
use zeppelin_embed::graph::search::GraphSearchProfile;
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, IngestError, Revision,
    SearchRequest,
};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::{
    CancelToken, GraphSearchOptions, OpenOptions, QueryControl, SearchOptions, SearchTier, Store,
};
use zeppelin_embed::manifest::EpochMeta;
use zeppelin_embed::manifest::io::{commit_manifest, load_manifest};
use zeppelin_embed::meta::{
    AliveSet, ColumnStoreBuilder, Predicate, PredicateValue, RangeBound, RangePredicate,
    TIMESTAMP_COLUMN,
};
use zeppelin_embed::planner::{PlanFallback, SegmentBranch, SegmentTier};
use zeppelin_embed::quant::{Bit4Factors, quantize_bit4};
use zeppelin_embed::scan::ScanOptions;
use zeppelin_embed::segment::SegmentId;
use zeppelin_embed::segment::writer::{
    SegmentBuild, SegmentDocumentVersions, SegmentFactors, write_segment_with_documents,
};
use zeppelin_embed::tier::{MaintenanceBudget, MaintenanceStatus, TierThresholds};
use zeppelin_embed::vfs::crash::RecordingVfs;
use zeppelin_embed::vfs::{StdVfs, Vfs};

use super::artifacts::RunArtifacts;
use super::fault_vfs::{self, FaultEvent};
use super::model::{ExpectedHit, Model, ModelEpoch};
use super::profiles::FaultProfile;
use super::program::{self, Op, Program, SearchKind};

const THREAD_BUDGET: usize = 1;

fn declared_store_epoch() -> StoreEpoch {
    let document = EmbeddingTower {
        model_id: "adversarial-embedding".to_owned(),
        model_version: "1".to_owned(),
        weights_digest: vec![0xad, 0x12],
        dims: program::DIMENSIONS as u32,
        normalization: Normalization::L2,
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
        format!(
            "VIOLATION {} seed={} profile={} op={}: {} | reproduce: {}",
            self.invariant.label(),
            self.seed,
            self.profile.key(),
            self.op_index,
            self.detail,
            reproduction(self.seed, self.profile)
        )
    }

    #[must_use]
    pub fn json(&self) -> String {
        format!(
            "{{\"invariant\":\"{}\",\"seed\":{},\"profile\":\"{}\",\"op\":{},\"detail\":\"{}\",\"reproduce\":\"{}\"}}",
            json_escape(self.invariant.label()),
            self.seed,
            self.profile.key(),
            self.op_index,
            json_escape(&self.detail),
            json_escape(&reproduction(self.seed, self.profile))
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunOutcome {
    pub seed: u64,
    pub profile: FaultProfile,
    pub operations: usize,
    pub faults_fired: usize,
    pub graph_searches: usize,
    pub filtered_searches: usize,
    pub filtered_graph_searches: usize,
    pub hybrid_searches: usize,
    pub hybrid_sealed_vector_documents: usize,
    pub hybrid_lexical_documents: usize,
    pub epoch_preparations: usize,
    pub epoch_alias_switches: usize,
    pub epoch_rollbacks: usize,
    pub epoch_drops: usize,
    pub rejected_dropped_epoch_rollbacks: usize,
    pub violations: Vec<Violation>,
    pub program_bytes: Vec<u8>,
    pub faults_bytes: Vec<u8>,
    pub violations_bytes: Vec<u8>,
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
    fn seal(&mut self, vfs: &dyn Vfs) -> Result<MutationAck, String>;
    fn drop_partition(
        &mut self,
        start: i64,
        end: i64,
        vfs: &dyn Vfs,
    ) -> Result<MutationAck, String>;
    fn purge(&mut self, doc_id: u32, vfs: &dyn Vfs) -> Result<MutationAck, String>;
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
    fn stats(&mut self) -> Result<StatsObservation, String>;
    fn close(&mut self) -> Result<(), String>;
    fn reopen(&mut self) -> Result<(), String>;
    fn crash_ingest(&mut self, mutation: DocMutation) -> Result<MutationAck, String>;
    fn generation(&mut self) -> Result<u64, String>;
}

struct RealEngine {
    directory: PathBuf,
    store: Option<Store>,
    graphs_built: u64,
    open_epoch: ModelEpoch,
}

impl RealEngine {
    fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            store: None,
            graphs_built: 0,
            open_epoch: ModelEpoch::A,
        }
    }

    fn store(&self) -> Result<&Store, String> {
        self.store
            .as_ref()
            .ok_or_else(|| "store is not open".to_owned())
    }

    fn options(epoch: ModelEpoch) -> OpenOptions {
        OpenOptions::new()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
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
            Store::open(&self.directory, Self::options(self.open_epoch))
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
            .with_metadata(program::sentinel(document.doc_id)),
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
        let mut manifest =
            load_manifest(&StdVfs, &manifest_path, u64::MAX).map_err(|error| error.to_string())?;
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
            &StdVfs,
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
        commit_manifest(&StdVfs, &self.directory, &manifest, policy)
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

    fn seal(&mut self, vfs: &dyn Vfs) -> Result<MutationAck, String> {
        let generation = self
            .store()?
            .seal_with_cancel_on_vfs(&CancelToken::new(), vfs)
            .map_err(|error| error.to_string())?;
        Ok(MutationAck {
            generation,
            changed: true,
        })
    }

    fn drop_partition(
        &mut self,
        start: i64,
        end: i64,
        vfs: &dyn Vfs,
    ) -> Result<MutationAck, String> {
        let report = self
            .store()?
            .drop_partition_on_vfs(start..end, vfs)
            .map_err(|error| error.to_string())?;
        Ok(MutationAck {
            generation: report.generation(),
            changed: !report.is_no_op(),
        })
    }

    fn purge(&mut self, doc_id: u32, vfs: &dyn Vfs) -> Result<MutationAck, String> {
        let token = self
            .store()?
            .purge(&[DocId::new(u128::from(doc_id))])
            .map_err(|error| error.to_string())?;
        let report = self
            .store()?
            .await_physical_purge_on_vfs(token, vfs)
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
        let outcome = self
            .store()?
            .search(
                SearchRequest::new(query),
                k,
                SearchOptions::new(ScanOptions {
                    thread_budget: THREAD_BUDGET,
                })
                .with_tier(tier),
                QueryControl::Cancel(CancelToken::new()),
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
            diagnostics_plan_matches_execution: true,
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

    fn crash_ingest(&mut self, mutation: DocMutation) -> Result<MutationAck, String> {
        if let Some(store) = self.store.take() {
            store.close().map_err(|error| error.to_string())?;
        }
        let marker = self.directory.join(".adversarial-crash-ack");
        let output = Command::new(std::env::current_exe().map_err(|error| error.to_string())?)
            .args(["crash_child", "--ignored", "--exact", "--nocapture"])
            .env("ZE_ADV_CRASH_CHILD_PATH", &self.directory)
            .env("ZE_ADV_CRASH_CHILD_MARKER", &marker)
            .env("ZE_ADV_CRASH_CHILD_DOC", mutation.doc_id.to_string())
            .env("ZE_ADV_CRASH_CHILD_REV", mutation.revision.to_string())
            .env("ZE_ADV_CRASH_CHILD_TS", mutation.timestamp.to_string())
            .output()
            .map_err(|error| format!("spawn crash child: {error}"))?;
        if output.status.success() {
            return Err("crash child exited successfully instead of crashing".to_owned());
        }
        let generation = std::fs::read_to_string(&marker)
            .map_err(|error| format!("crash child did not persist acknowledgement: {error}"))?
            .trim()
            .parse::<u64>()
            .map_err(|error| format!("parse crash acknowledgement: {error}"))?;
        std::fs::remove_file(&marker).map_err(|error| format!("remove crash marker: {error}"))?;
        self.open()?;
        Ok(MutationAck {
            generation,
            changed: true,
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

    fn seal(&mut self, vfs: &dyn Vfs) -> Result<MutationAck, String> {
        self.inner.seal(vfs)
    }

    fn drop_partition(
        &mut self,
        start: i64,
        end: i64,
        vfs: &dyn Vfs,
    ) -> Result<MutationAck, String> {
        self.inner.drop_partition(start, end, vfs)
    }

    fn purge(&mut self, doc_id: u32, vfs: &dyn Vfs) -> Result<MutationAck, String> {
        self.inner.purge(doc_id, vfs)
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

    fn stats(&mut self) -> Result<StatsObservation, String> {
        self.inner.stats()
    }

    fn close(&mut self) -> Result<(), String> {
        self.inner.close()
    }

    fn reopen(&mut self) -> Result<(), String> {
        self.inner.reopen()
    }

    fn crash_ingest(&mut self, mutation: DocMutation) -> Result<MutationAck, String> {
        self.inner.crash_ingest(mutation)
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
    let real = RealEngine::new(directory.path().to_path_buf());
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
    let program = Program::generate(seed);
    let artifacts = RunArtifacts::create(artifact_root, seed, profile)?;
    let program_bytes = artifacts.write_program(&program)?;
    let reproduction = reproduction(seed, profile);
    artifacts.write_reproduction(&reproduction)?;
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let mut engine = RealEngine::new(directory.path().to_path_buf());
    let mut model = Model::default();
    let mut violations = Vec::new();
    let mut faults = Vec::<FaultEvent>::new();
    let mut last_generation = 0_u64;
    let mut fault_targeted = false;
    let mut content_fault_fired = false;
    let mut graph_searches = 0_usize;
    let mut filtered_searches = 0_usize;
    let mut filtered_graph_searches = 0_usize;
    let mut hybrid_searches = 0_usize;
    let mut hybrid_sealed_vector_documents = 0_usize;
    let mut hybrid_lexical_documents = 0_usize;
    let mut epoch_preparations = 0_usize;
    let mut epoch_alias_switches = 0_usize;
    let mut epoch_rollbacks = 0_usize;
    let mut epoch_drops = 0_usize;
    let mut rejected_dropped_epoch_rollbacks = 0_usize;

    for (op_index, op) in program.ops.iter().enumerate() {
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
                    model.acknowledge(*doc_id, *revision, *timestamp);
                    Some(ack)
                })
            }
            Op::Delete { doc_id } => engine.delete(*doc_id).map(|ack| {
                model.delete(*doc_id);
                Some(ack)
            }),
            Op::Seal => {
                let event = if fault_targeted {
                    None
                } else {
                    let event = fault_vfs::scheduled_event(seed, profile, op_index);
                    fault_targeted = event.is_some();
                    event
                };
                let scheduled = fault_vfs::std_scheduled(event);
                let recording = RecordingVfs::new(scheduled.clone());
                let result = engine.seal(&recording);
                if let Some(event) = scheduled.event() {
                    content_fault_fired |= event.fired && profile == FaultProfile::Content;
                    faults.push(event);
                }
                let _recorded_boundaries = recording
                    .operations()
                    .map_err(|error| format!("record vfs::crash boundaries: {error}"))?;
                result.map(|ack| {
                    model.seal();
                    Some(ack)
                })
            }
            Op::Maintain { bytes } => engine.maintain(*bytes).map(Some),
            Op::Search { query, k, kind } => {
                let query = program::query(*query);
                let requested = if *k == usize::MAX { model.len() } else { *k };
                engine
                    .search(&query, requested, *kind, seed)
                    .map(|observed| {
                        if observed.graph_segments > 0 {
                            graph_searches = graph_searches.saturating_add(1);
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
            } => {
                let crash_event = fault_vfs::audit_crash_seam(seed, op_index)?;
                faults.push(crash_event);
                let document = DocMutation {
                    doc_id: *doc_id,
                    revision: *revision,
                    timestamp: *timestamp,
                };
                engine.crash_ingest(document).map(|ack| {
                    model.acknowledge(*doc_id, *revision, *timestamp);
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
                    Some(ack)
                })
            }
            Op::DropPartition { start, end } => {
                let scheduled = fault_vfs::std_scheduled(None);
                engine.drop_partition(*start, *end, &scheduled).map(|ack| {
                    if ack.changed {
                        model.drop_partition(*start, *end);
                    }
                    Some(ack)
                })
            }
            Op::Purge { doc_id } => {
                let scheduled = fault_vfs::std_scheduled(None);
                engine.purge(*doc_id, &scheduled).map(|ack| {
                    if ack.changed {
                        model.purge(*doc_id);
                    }
                    if let Some(violation) =
                        purge_proof_violation(seed, profile, op_index, directory.path(), *doc_id)
                    {
                        violations.push(violation);
                    }
                    Some(ack)
                })
            }
        };

        match operation_result {
            Ok(Some(ack)) => {
                if ack.changed {
                    if let Some(generation) =
                        generation_violation(seed, profile, op_index, last_generation, ack)
                    {
                        violations.push(generation);
                    }
                    last_generation = last_generation.max(ack.generation);
                }
            }
            Ok(None) => {}
            Err(error) => {
                let injected = faults
                    .last()
                    .is_some_and(|fault| fault.op_index == op_index && fault.fired);
                if !injected {
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
    }

    let faults_fired = faults.iter().filter(|fault| fault.fired).count();
    let faults_bytes = artifacts.write_faults(&faults)?;
    let violations_bytes = artifacts.write_violations(&violations)?;
    Ok(RunOutcome {
        seed,
        profile,
        operations: program.ops.len(),
        faults_fired,
        graph_searches,
        filtered_searches,
        filtered_graph_searches,
        hybrid_searches,
        hybrid_sealed_vector_documents,
        hybrid_lexical_documents,
        epoch_preparations,
        epoch_alias_switches,
        epoch_rollbacks,
        epoch_drops,
        rejected_dropped_epoch_rollbacks,
        violations,
        program_bytes,
        faults_bytes,
        violations_bytes,
    })
}

struct HybridCheck {
    sealed_vector_documents: usize,
    lexical_documents: usize,
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

    let lexical_rows = model.lexical_documents();
    let analyzer = Analyzer::new(Profile::Code.config()).map_err(|error| error.to_string())?;
    let mut segment = SegmentIndex::new();
    for (doc_id, revision) in &lexical_rows {
        segment
            .push_document(
                &analyzer,
                &Document::with_text(&program::lexical_text(*doc_id, *revision)),
            )
            .map_err(|error| error.to_string())?;
    }
    let mut lexical_index = LexicalIndex::new();
    lexical_index
        .push_segment(segment)
        .map_err(|error| error.to_string())?;
    let lexical_result = lexical_search(
        &lexical_index,
        &TermQuery::flat(
            vec![program::lexical_query(query_slot).to_vec()],
            &[DEFAULT_FIELD],
        ),
        model.len(),
        Bm25Params::default(),
    )
    .map_err(|error| error.to_string())?;
    let lexical = lexical_result
        .hits
        .iter()
        .map(|hit| LexicalCandidate::new(hit.doc, hit.score))
        .collect::<Vec<_>>();
    let lexical_join = |doc: &zeppelin_embed::fts::search::GlobalDocId| {
        usize::try_from(doc.row)
            .ok()
            .and_then(|row| lexical_rows.get(row))
            .map(|(doc_id, _)| *doc_id)
    };

    let actual_vector = observed
        .hits
        .iter()
        .map(|hit| VectorCandidate::exact(hit.doc_id, -f64::from(hit.score)))
        .collect::<Vec<_>>();
    let expected_vector = model
        .expected_exact(&query_vector, model.len())
        .into_iter()
        .map(|hit| VectorCandidate::exact(hit.doc_id, -f64::from(hit.score)))
        .collect::<Vec<_>>();
    let fusion_query = HybridQuery::new(k).with_epoch(declared_identity());
    let actual = fuse(
        &fusion_query,
        &actual_vector,
        &lexical,
        |doc_id| Some(*doc_id),
        lexical_join,
    )
    .map_err(|error| error.to_string())?;
    let expected = fuse(
        &fusion_query,
        &expected_vector,
        &lexical,
        |doc_id| Some(*doc_id),
        lexical_join,
    )
    .map_err(|error| error.to_string())?;
    let mismatch = if actual != expected {
        Some(format!(
            "hybrid exact result mismatch: model={:?} engine={:?}",
            expected.hits, actual.hits
        ))
    } else if actual.report.epoch != Some(declared_identity()) {
        Some(format!(
            "hybrid report epoch {:?} did not name the declared store epoch",
            actual.report.epoch
        ))
    } else {
        None
    };
    Ok(Some(HybridCheck {
        sealed_vector_documents,
        lexical_documents: lexical_rows.len(),
        mismatch,
    }))
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
    format!(
        "ZE_ADV_SEED={seed} ZE_ADV_PROFILE={} cargo test -p zeppelin-embed-workspace-tests --test adversarial_tests run -- --ignored --exact --nocapture",
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
    let store = Store::open(&directory, RealEngine::options(ModelEpoch::A))
        .map_err(|error| error.to_string())?;
    let document = IngestDocument::new(
        DocumentVersion::new(DocId::new(u128::from(doc_id)), Revision::new(revision)),
        program::vector(doc_id, revision).to_vec(),
    )
    .with_timestamp(timestamp)
    .with_metadata(program::sentinel(doc_id));
    let ack = store
        .ingest(IngestBatch::new(vec![document]).with_epoch(declared_identity()))
        .map_err(|error| error.to_string())?;
    std::fs::write(marker, ack.generation().to_string())
        .map_err(|error| format!("write crash acknowledgement: {error}"))?;
    std::process::abort();
}

#[allow(dead_code)]
fn _keep_tempdir_type_visible(_: &TempDir) {}
