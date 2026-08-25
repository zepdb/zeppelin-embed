//! Query diagnostics and health reporting.

use std::time::Duration;

use crate::epoch::EpochId;
use crate::fts::search::SearchCounters;
use crate::fusion::FusionReport;
use crate::graph::search::QueryQosClass;
use crate::ingest::GraphSearchStats;
use crate::planner::SegmentPlan;
use crate::scan::ScanStats;
use crate::wal::LogSeq;

/// Version 1 composition of the exact counters already returned by each query leg.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct QueryCounters {
    /// Version 1 vector scan work, including graph coarse and rescore reads.
    pub scan: ScanStats,
    /// Version 1 aggregate graph traversal work.
    pub graph: GraphSearchStats,
    /// Version 1 exact lexical work; zero when no lexical leg ran.
    pub lexical: SearchCounters,
}

/// Version 1 observation of the caller thread's scheduling class.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ObservedQos {
    /// QoS class observed without changing it.
    pub class: QueryQosClass,
    /// Darwin relative priority, or zero where unavailable.
    pub relative_priority: i32,
}

impl ObservedQos {
    #[cfg(target_os = "macos")]
    fn current() -> Self {
        match crate::sys::darwin::observed_qos() {
            Ok((class, relative_priority)) => Self {
                class: match class {
                    crate::sys::darwin::QosClass::UserInteractive => QueryQosClass::UserInteractive,
                    crate::sys::darwin::QosClass::UserInitiated => QueryQosClass::UserInitiated,
                    crate::sys::darwin::QosClass::Default => QueryQosClass::Default,
                    crate::sys::darwin::QosClass::Utility => QueryQosClass::Utility,
                    crate::sys::darwin::QosClass::Background => QueryQosClass::Background,
                    crate::sys::darwin::QosClass::Unspecified => QueryQosClass::Unspecified,
                },
                relative_priority,
            },
            Err(_) => Self {
                class: QueryQosClass::Unknown,
                relative_priority: 0,
            },
        }
    }

    #[cfg(not(target_os = "macos"))]
    const fn current() -> Self {
        Self {
            class: QueryQosClass::Unavailable,
            relative_priority: 0,
        }
    }
}

/// Version 1 unconditional report of what one store query actually did.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct QueryDiagnostics {
    /// Version 1 generation pinned for the complete query.
    pub snapshot_generation: u64,
    /// Highest WAL sequence represented by the pinned active and sealed state.
    pub indexed_through_seq: LogSeq,
    /// Version 1 per-segment branches in execution order.
    pub plan: Vec<SegmentPlan>,
    /// True when any candidate membership came from graph traversal or another
    /// non-exhaustive path.
    pub approximate: bool,
    /// True when every returned score was computed from full-precision rows.
    pub exact_rescore: bool,
    /// Version 1 fusion report; absent when no hybrid fusion leg ran.
    pub fusion: Option<FusionReport>,
    /// Requested result count.
    pub requested_k: usize,
    /// Result count actually returned.
    pub returned: usize,
    /// True when an execution budget fired, including an exact fallback.
    pub budget_exhausted: bool,
    /// Version 1 exact counter composition.
    pub counters: QueryCounters,
    /// Embedding identity that interpreted a vector leg.
    pub embedding_epoch: Option<EpochId>,
    /// Tokenizer identity that interpreted a lexical leg.
    pub tokenizer_epoch: Option<crate::fts::tokenizer::TokenizerEpoch>,
    /// Caller-thread QoS observed without requesting a scheduling change.
    pub observed_qos: ObservedQos,
    /// Wall time spent in the admitted query path.
    pub elapsed: Duration,
}

/// Compares semantic query behavior while intentionally excluding `elapsed`,
/// because wall-clock timing varies between otherwise identical searches. The
/// exhaustive destructuring makes every future field an explicit equality choice.
impl PartialEq for QueryDiagnostics {
    fn eq(&self, other: &Self) -> bool {
        let Self {
            snapshot_generation,
            indexed_through_seq,
            plan,
            approximate,
            exact_rescore,
            fusion,
            requested_k,
            returned,
            budget_exhausted,
            counters,
            embedding_epoch,
            tokenizer_epoch,
            observed_qos,
            elapsed: _,
        } = self;

        snapshot_generation == &other.snapshot_generation
            && indexed_through_seq == &other.indexed_through_seq
            && plan == &other.plan
            && approximate == &other.approximate
            && exact_rescore == &other.exact_rescore
            && fusion == &other.fusion
            && requested_k == &other.requested_k
            && returned == &other.returned
            && budget_exhausted == &other.budget_exhausted
            && counters == &other.counters
            && embedding_epoch == &other.embedding_epoch
            && tokenizer_epoch == &other.tokenizer_epoch
            && observed_qos == &other.observed_qos
    }
}

/// Inputs for one vector-only diagnostics value.
pub(crate) struct VectorDiagnostics {
    pub snapshot_generation: u64,
    pub indexed_through_seq: LogSeq,
    pub plan: Vec<SegmentPlan>,
    pub approximate: bool,
    pub exact_rescore: bool,
    pub requested_k: usize,
    pub returned: usize,
    pub budget_exhausted: bool,
    pub scan: ScanStats,
    pub graph: GraphSearchStats,
    pub epoch: Option<crate::epoch::EpochIdentity>,
    pub elapsed: Duration,
}

impl QueryDiagnostics {
    pub(crate) fn vector(input: VectorDiagnostics) -> Self {
        Self {
            snapshot_generation: input.snapshot_generation,
            indexed_through_seq: input.indexed_through_seq,
            plan: input.plan,
            approximate: input.approximate,
            exact_rescore: input.exact_rescore,
            fusion: None,
            requested_k: input.requested_k,
            returned: input.returned,
            budget_exhausted: input.budget_exhausted,
            counters: QueryCounters {
                scan: input.scan,
                graph: input.graph,
                lexical: SearchCounters::default(),
            },
            embedding_epoch: input.epoch.map(|epoch| epoch.embedding),
            tokenizer_epoch: None,
            observed_qos: ObservedQos::current(),
            elapsed: input.elapsed,
        }
    }
}

/// Version 1 health for one active or immutable segment.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct SegmentHealth {
    /// Collision-free segment address.
    pub source: crate::ingest::RowSource,
    /// Physical search tier present in the pinned store state.
    pub tier: crate::tier::SegmentTier,
    /// Exact stored row count, including tombstoned rows.
    pub rows: u64,
    /// Exact tombstoned row count.
    pub tombstones: u64,
    /// Exact active allocation or immutable mapped byte count.
    pub bytes: u64,
    /// Manifest-stamped clustering-key range for immutable segments.
    pub clustering_key_range: crate::segment::ClusteringKeyRange,
}

/// Cloneable version 1 outcome retained from the last maintenance call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MaintenanceOutcome {
    /// Every considered transition completed or was not due.
    Complete,
    /// Work remains because the host-supplied budget fired.
    BudgetExhausted,
    /// A typed maintenance error was returned; its display form is retained.
    Failed(String),
}

/// Version 1 retained maintenance counters.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct MaintenanceSummary {
    /// Graph generations published by the call.
    pub graphs_built: u64,
    /// Graph construction bytes charged to the call.
    pub bytes_consumed: u64,
    /// Checkpointed graph builds resumed by the call.
    pub checkpoints_resumed: u64,
    /// Final disposition retained without owning a non-cloneable I/O error.
    pub outcome: MaintenanceOutcome,
}

/// Version 1 store health extending exact task-09 statistics.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Health {
    /// Exact task-09 resource counters.
    pub stats: crate::lifecycle::Stats,
    /// Current active-state generation.
    pub generation: u64,
    /// Active and immutable segment health in stable source order.
    pub segments: Vec<SegmentHealth>,
    /// Live acknowledged rows not yet sealed into immutable segments.
    pub pending_docs: u64,
    /// Upper bound of the last explicitly executed retention drop range.
    pub retained_through: Option<i64>,
    /// Cloneable summary of the last maintenance report.
    pub last_maintenance: Option<MaintenanceSummary>,
}

/// One non-fatal failure encountered while gathering or querying self-check data.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct SelfCheckFailure {
    /// Stable stage name.
    pub stage: &'static str,
    /// Typed engine error rendered for the host report.
    pub detail: String,
}

/// Deterministic version 1 comparison of live-plan results with brute force.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct SelfCheckReport {
    /// Caller-requested sample size.
    pub requested_samples: usize,
    /// Stored documents actually sampled.
    pub sampled_documents: usize,
    /// Seed used for deterministic sampling.
    pub seed: u64,
    /// Ground-truth hits across every completed sampled query.
    pub expected_hits: usize,
    /// Ground-truth hits also returned by the live plan.
    pub recalled_hits: usize,
    /// `recalled_hits / expected_hits`, or one for an empty comparison.
    pub recall: f64,
    /// Largest absolute score delta among hits present in both answers.
    pub max_score_delta: f64,
    /// Non-fatal gather or query failures. Any failure contributes no recall.
    pub failures: Vec<SelfCheckFailure>,
}

#[derive(Clone)]
struct StoredSelfCheckDocument {
    version: crate::ingest::DocumentVersion,
    vector: Vec<f32>,
}

#[derive(Default)]
pub(crate) struct HealthState {
    retained_through: Option<i64>,
    last_maintenance: Option<MaintenanceSummary>,
}

impl crate::lifecycle::Store {
    /// Samples stored vectors deterministically and reports live-plan recall
    /// against a full-precision brute-force oracle.
    #[must_use]
    pub fn self_check(&self, sample: usize, seed: u64) -> SelfCheckReport {
        self.self_check_inner(sample, seed, false)
    }

    fn self_check_inner(&self, sample: usize, seed: u64, force_miss: bool) -> SelfCheckReport {
        let documents = match self.self_check_documents() {
            Ok(documents) => documents,
            Err(failure) => {
                return SelfCheckReport {
                    requested_samples: sample,
                    sampled_documents: 0,
                    seed,
                    expected_hits: 0,
                    recalled_hits: 0,
                    recall: 0.0,
                    max_score_delta: 0.0,
                    failures: vec![failure],
                };
            }
        };
        let mut ranked_samples = (0..documents.len())
            .map(|index| {
                let index_seed = u64::try_from(index).map_or(u64::MAX, |value| value);
                (splitmix64(seed ^ index_seed), index)
            })
            .collect::<Vec<_>>();
        ranked_samples.sort_unstable();
        let mut sampled = ranked_samples
            .into_iter()
            .map(|(_, index)| index)
            .collect::<Vec<_>>();
        sampled.truncate(sample.min(sampled.len()));
        let mut expected_hits = 0_usize;
        let mut recalled_hits = 0_usize;
        let mut max_score_delta = 0.0_f64;
        let mut failures = Vec::new();
        for sampled_index in &sampled {
            let Some(query_document) = documents.get(*sampled_index) else {
                failures.push(SelfCheckFailure {
                    stage: "sample",
                    detail: format!("sample index {sampled_index} left the gathered corpus"),
                });
                continue;
            };
            let k = documents.len().min(10);
            let expected = brute_force_self_check(&documents, &query_document.vector, k);
            expected_hits = expected_hits.saturating_add(expected.len());
            let outcome = self.search(
                crate::ingest::SearchRequest::new(&query_document.vector),
                k,
                crate::lifecycle::SearchOptions::default(),
                crate::lifecycle::QueryControl::Cancel(crate::lifecycle::CancelToken::new()),
            );
            let mut actual = match outcome {
                Ok(outcome) => outcome
                    .candidates
                    .iter()
                    .filter_map(|candidate| {
                        candidate
                            .document()
                            .map(|version| (version, candidate.score()))
                    })
                    .collect::<Vec<_>>(),
                Err(error) => {
                    failures.push(SelfCheckFailure {
                        stage: "query",
                        detail: error.to_string(),
                    });
                    continue;
                }
            };
            if force_miss && !actual.is_empty() {
                actual.remove(0);
            }
            for (version, expected_score) in expected {
                if let Some((_, actual_score)) = actual
                    .iter()
                    .find(|(actual_version, _)| *actual_version == version)
                {
                    recalled_hits = recalled_hits.saturating_add(1);
                    max_score_delta = max_score_delta
                        .max((f64::from(*actual_score) - f64::from(expected_score)).abs());
                }
            }
        }
        let recall = if expected_hits == 0 {
            1.0
        } else {
            recalled_hits as f64 / expected_hits as f64
        };
        SelfCheckReport {
            requested_samples: sample,
            sampled_documents: sampled.len(),
            seed,
            expected_hits,
            recalled_hits,
            recall,
            max_score_delta,
            failures,
        }
    }

    fn self_check_documents(&self) -> Result<Vec<StoredSelfCheckDocument>, SelfCheckFailure> {
        let state = self.state.lock().map_err(|_| SelfCheckFailure {
            stage: "state",
            detail: "store state lock was poisoned".to_owned(),
        })?;
        match *state {
            crate::lifecycle::StoreState::Open => {}
            crate::lifecycle::StoreState::Closing => {
                return Err(SelfCheckFailure {
                    stage: "state",
                    detail: crate::lifecycle::StoreError::Closing.to_string(),
                });
            }
            crate::lifecycle::StoreState::Closed => {
                return Err(SelfCheckFailure {
                    stage: "state",
                    detail: crate::lifecycle::StoreError::Closed.to_string(),
                });
            }
        }
        let active = self.active.lock().map_err(|_| SelfCheckFailure {
            stage: "active",
            detail: "active segment lock was poisoned".to_owned(),
        })?;
        let active = active.as_ref().ok_or_else(|| SelfCheckFailure {
            stage: "active",
            detail: crate::lifecycle::StoreError::Closed.to_string(),
        })?;
        let mut documents = Vec::new();
        let active_rows = active.segment.row_count();
        if active_rows != 0 {
            let active_vectors = active.segment.vectors();
            let dims = active_vectors
                .len()
                .checked_div(active_rows)
                .ok_or_else(|| SelfCheckFailure {
                    stage: "active",
                    detail: "active vector dimensions overflowed".to_owned(),
                })?;
            if dims.checked_mul(active_rows) != Some(active_vectors.len()) {
                return Err(SelfCheckFailure {
                    stage: "active",
                    detail: "active vector rows are not rectangular".to_owned(),
                });
            }
            let alive = active.segment.alive().map_err(|error| SelfCheckFailure {
                stage: "active",
                detail: error.to_string(),
            })?;
            for row in 0..active_rows {
                let row_u32 = u32::try_from(row).map_err(|_| SelfCheckFailure {
                    stage: "active",
                    detail: "active row exceeds u32".to_owned(),
                })?;
                if !alive.is_alive(row_u32) {
                    continue;
                }
                let start = row.checked_mul(dims).ok_or_else(|| SelfCheckFailure {
                    stage: "active",
                    detail: "active vector offset overflowed".to_owned(),
                })?;
                let end = start.checked_add(dims).ok_or_else(|| SelfCheckFailure {
                    stage: "active",
                    detail: "active vector end overflowed".to_owned(),
                })?;
                let vector = active_vectors
                    .get(start..end)
                    .ok_or_else(|| SelfCheckFailure {
                        stage: "active",
                        detail: "active vector row is out of bounds".to_owned(),
                    })?;
                let version = active
                    .segment
                    .document(row)
                    .ok_or_else(|| SelfCheckFailure {
                        stage: "active",
                        detail: format!("active row {row} has no document identity"),
                    })?;
                documents.push(StoredSelfCheckDocument {
                    version,
                    vector: vector.to_vec(),
                });
            }
        }
        let snapshot = self
            .snapshot
            .read()
            .map_err(|_| SelfCheckFailure {
                stage: "snapshot",
                detail: "published snapshot lock was poisoned".to_owned(),
            })?
            .as_ref()
            .cloned()
            .ok_or_else(|| SelfCheckFailure {
                stage: "snapshot",
                detail: crate::lifecycle::StoreError::Closed.to_string(),
            })?;
        for segment in snapshot.segments() {
            let rows = segment.meta().row_count as usize;
            let dims = segment.meta().dims as usize;
            let vectors = segment.rescore_f32().map_err(|error| SelfCheckFailure {
                stage: "sealed",
                detail: error.to_string(),
            })?;
            if rows.checked_mul(dims) != Some(vectors.len()) {
                return Err(SelfCheckFailure {
                    stage: "sealed",
                    detail: format!("segment {} f32 rows are not rectangular", segment.meta().id),
                });
            }
            let alive = segment.alive().map_err(|error| SelfCheckFailure {
                stage: "sealed",
                detail: error.to_string(),
            })?;
            for row in 0..rows {
                let row_u32 = u32::try_from(row).map_err(|_| SelfCheckFailure {
                    stage: "sealed",
                    detail: "sealed row exceeds u32".to_owned(),
                })?;
                if !alive.is_alive(row_u32) {
                    continue;
                }
                let start = row.checked_mul(dims).ok_or_else(|| SelfCheckFailure {
                    stage: "sealed",
                    detail: "sealed vector offset overflowed".to_owned(),
                })?;
                let end = start.checked_add(dims).ok_or_else(|| SelfCheckFailure {
                    stage: "sealed",
                    detail: "sealed vector end overflowed".to_owned(),
                })?;
                let vector = vectors.get(start..end).ok_or_else(|| SelfCheckFailure {
                    stage: "sealed",
                    detail: "sealed vector row is out of bounds".to_owned(),
                })?;
                let version = segment
                    .document_version(row)
                    .map_err(|error| SelfCheckFailure {
                        stage: "sealed",
                        detail: error.to_string(),
                    })?
                    .ok_or_else(|| SelfCheckFailure {
                        stage: "sealed",
                        detail: format!(
                            "segment {} row {row} has no document identity",
                            segment.meta().id
                        ),
                    })?;
                documents.push(StoredSelfCheckDocument {
                    version,
                    vector: vector.to_vec(),
                });
            }
        }
        Ok(documents)
    }

    pub(crate) fn record_retention(
        &self,
        retained_through: i64,
    ) -> Result<(), crate::lifecycle::StoreError> {
        let mut state = self.health_state.lock().map_err(|_| {
            crate::lifecycle::StoreError::Synchronization {
                component: "health state",
            }
        })?;
        state.retained_through = Some(retained_through);
        Ok(())
    }

    pub(crate) fn record_maintenance(
        &self,
        report: &crate::tier::MaintenanceReport,
    ) -> Result<(), crate::lifecycle::StoreError> {
        let outcome = match &report.status {
            crate::tier::MaintenanceStatus::Complete => MaintenanceOutcome::Complete,
            crate::tier::MaintenanceStatus::BudgetExhausted => MaintenanceOutcome::BudgetExhausted,
            crate::tier::MaintenanceStatus::Failed(error) => {
                MaintenanceOutcome::Failed(error.to_string())
            }
        };
        let mut state = self.health_state.lock().map_err(|_| {
            crate::lifecycle::StoreError::Synchronization {
                component: "health state",
            }
        })?;
        state.last_maintenance = Some(MaintenanceSummary {
            graphs_built: report.graphs_built,
            bytes_consumed: report.bytes_consumed,
            checkpoints_resumed: report.checkpoints_resumed,
            outcome,
        });
        Ok(())
    }

    /// Returns current store health without changing engine behaviour.
    pub fn health(&self) -> Result<Health, crate::lifecycle::StoreError> {
        let lifecycle_state = self
            .state
            .lock()
            .map_err(|_| crate::lifecycle::StoreError::Synchronization { component: "state" })?;
        match *lifecycle_state {
            crate::lifecycle::StoreState::Open => {}
            crate::lifecycle::StoreState::Closing => {
                return Err(crate::lifecycle::StoreError::Closing);
            }
            crate::lifecycle::StoreState::Closed => {
                return Err(crate::lifecycle::StoreError::Closed);
            }
        }
        let stats = self.stats_while_open()?;
        let active =
            self.active
                .lock()
                .map_err(|_| crate::lifecycle::StoreError::Synchronization {
                    component: "active segment",
                })?;
        let active = active
            .as_ref()
            .ok_or(crate::lifecycle::StoreError::Closed)?;
        let generation = active.generation;
        let active_tombstones = active.segment.tombstone_count();
        let mut segments = Vec::new();
        if stats.active_row_count != 0 {
            segments.push(SegmentHealth {
                source: crate::ingest::RowSource::Active,
                tier: crate::tier::SegmentTier::ActiveScan,
                rows: stats.active_row_count,
                tombstones: active_tombstones,
                bytes: stats.active_segment_bytes,
                clustering_key_range: crate::segment::ClusteringKeyRange::Unstamped,
            });
        }
        let snapshot = self
            .snapshot
            .read()
            .map_err(|_| crate::lifecycle::StoreError::Synchronization {
                component: "published snapshot",
            })?
            .as_ref()
            .cloned()
            .ok_or(crate::lifecycle::StoreError::Closed)?;
        for segment in snapshot.segments() {
            let alive = segment
                .alive()
                .map_err(crate::lifecycle::StoreError::Segment)?;
            let tier =
                if segment.directory().iter().any(|entry| {
                    entry.kind == crate::segment::layout::RegionKind::GraphNodeBlocks.id()
                }) {
                    crate::tier::SegmentTier::SealedGraph
                } else {
                    crate::tier::SegmentTier::SealedScan
                };
            segments.push(SegmentHealth {
                source: crate::ingest::RowSource::Sealed(segment.meta().id),
                tier,
                rows: u64::from(segment.meta().row_count),
                tombstones: alive.tombstone_count(),
                bytes: u64::try_from(segment.mapped_bytes()).map_err(|_| {
                    crate::lifecycle::StoreError::Statistics {
                        component: "segment health bytes",
                        source: std::io::Error::other("mapped segment bytes exceed u64"),
                    }
                })?,
                clustering_key_range: segment.meta().clustering_key_range,
            });
        }
        segments.sort_unstable_by_key(|segment| segment.source);
        let health_state = self.health_state.lock().map_err(|_| {
            crate::lifecycle::StoreError::Synchronization {
                component: "health state",
            }
        })?;
        Ok(Health {
            stats,
            generation,
            segments,
            pending_docs: stats.active_row_count.saturating_sub(active_tombstones),
            retained_through: health_state.retained_through,
            last_maintenance: health_state.last_maintenance.clone(),
        })
    }
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut mixed = value;
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    mixed ^ (mixed >> 31)
}

fn brute_force_self_check(
    documents: &[StoredSelfCheckDocument],
    query: &[f32],
    k: usize,
) -> Vec<(crate::ingest::DocumentVersion, f32)> {
    let mut scored = documents
        .iter()
        .filter(|document| document.vector.len() == query.len())
        .map(|document| {
            let distance = document
                .vector
                .iter()
                .zip(query)
                .map(|(left, right)| {
                    let delta = f64::from(*left) - f64::from(*right);
                    delta * delta
                })
                .sum::<f64>();
            (document.version, -(distance as f32))
        })
        .collect::<Vec<_>>();
    scored.sort_unstable_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.doc_id().cmp(&right.0.doc_id()))
            .then_with(|| left.0.revision().cmp(&right.0.revision()))
    });
    scored.truncate(k);
    scored
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use tempfile::tempdir;

    use crate::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
    use crate::lifecycle::{OpenOptions, Store};

    fn populated_store() -> (tempfile::TempDir, Store) {
        let directory = tempdir().expect("self-check directory");
        let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
        let documents = (0..8)
            .map(|row| {
                IngestDocument::new(
                    DocumentVersion::new(DocId::new(row + 1), Revision::new(1)),
                    vec![row as f32, -(row as f32)],
                )
            })
            .collect();
        store
            .ingest(IngestBatch::new(documents))
            .expect("ingest self-check corpus");
        (directory, store)
    }

    #[test]
    fn self_check_is_deterministic_per_seed() {
        let (_directory, store) = populated_store();
        assert_eq!(store.self_check(5, 0x18), store.self_check(5, 0x18));
        store.close().expect("close store");
    }

    #[test]
    fn self_check_reports_a_planted_degradation() {
        let (_directory, store) = populated_store();
        let report = store.self_check_inner(5, 0x18, true);
        println!(
            "planted self-check degradation recall={} recalled={}/{}",
            report.recall, report.recalled_hits, report.expected_hits
        );
        assert!(report.failures.is_empty());
        assert!(report.recall < 1.0);
        store.close().expect("close store");
    }
}
