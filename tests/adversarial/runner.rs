use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use tempfile::TempDir;
use zeppelin_embed::graph::search::GraphSearchProfile;
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode};
use zeppelin_embed::lifecycle::{
    CancelToken, GraphSearchOptions, OpenOptions, QueryControl, SearchOptions, SearchTier, Store,
};
use zeppelin_embed::scan::ScanOptions;
use zeppelin_embed::tier::{MaintenanceBudget, MaintenanceStatus};
use zeppelin_embed::vfs::Vfs;
use zeppelin_embed::vfs::crash::RecordingVfs;

use super::artifacts::RunArtifacts;
use super::fault_vfs::{self, FaultEvent};
use super::model::{ExpectedHit, Model};
use super::profiles::FaultProfile;
use super::program::{self, Op, Program, SearchKind};

const THREAD_BUDGET: usize = 1;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Invariant {
    I1,
    I2,
    I3,
    I4,
    I6,
    I7,
    I8,
    I9,
    I10,
    I11,
}

impl Invariant {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::I1 => "I1 acked-implies-visible",
            Self::I2 => "I2 deleted-implies-gone",
            Self::I3 => "I3 exactness",
            Self::I4 => "I4 durability-prefix",
            Self::I6 => "I6 accounting-conservation",
            Self::I7 => "I7 corruption-never-consumed",
            Self::I8 => "I8 lifecycle",
            Self::I9 => "I9 generation-monotonicity",
            Self::I10 => "I10 purge-proof",
            Self::I11 => "I11 revision-ordering",
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
    graph_available: bool,
    graph_segments: usize,
    graph_rescored: usize,
    graph_pruned: usize,
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
}

impl RealEngine {
    fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            store: None,
            graphs_built: 0,
        }
    }

    fn store(&self) -> Result<&Store, String> {
        self.store
            .as_ref()
            .ok_or_else(|| "store is not open".to_owned())
    }

    fn options() -> OpenOptions {
        OpenOptions::new().with_durability(DurabilityMode::Durable, CommitTier::Durable)
    }
}

impl Engine for RealEngine {
    fn open(&mut self) -> Result<(), String> {
        if self.store.is_some() {
            return Err("store is already open".to_owned());
        }
        self.store =
            Some(Store::open(&self.directory, Self::options()).map_err(|error| error.to_string())?);
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
            .ingest(IngestBatch::new(batch))
            .map_err(|error| error.to_string())?;
        Ok(MutationAck {
            generation: ack.generation(),
            changed: true,
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
        let report = self.store()?.maintain(MaintenanceBudget {
            wall_time: Duration::from_secs(120),
            bytes,
        });
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
        Ok(SearchObservation {
            hits,
            generation: outcome.generation,
            graph_available: self.graphs_built != 0,
            graph_segments: outcome.graph_stats.segments_traversed,
            graph_rescored: outcome.graph_stats.candidates_rescored,
            graph_pruned: outcome.graph_stats.segments_pruned_by_bound,
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
                        Op::Crash { .. } => Invariant::I4,
                        Op::Stats => Invariant::I6,
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
        violations,
        program_bytes,
        faults_bytes,
        violations_bytes,
    })
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
    let observation = |hits, graph_available, graph_segments, graph_rescored| SearchObservation {
        hits,
        generation: 1,
        graph_available,
        graph_segments,
        graph_rescored,
        graph_pruned: 0,
    };
    match invariant {
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
    let store =
        Store::open(&directory, RealEngine::options()).map_err(|error| error.to_string())?;
    let document = IngestDocument::new(
        DocumentVersion::new(DocId::new(u128::from(doc_id)), Revision::new(revision)),
        program::vector(doc_id, revision).to_vec(),
    )
    .with_timestamp(timestamp)
    .with_metadata(program::sentinel(doc_id));
    let ack = store
        .ingest(IngestBatch::new(vec![document]))
        .map_err(|error| error.to_string())?;
    std::fs::write(marker, ack.generation().to_string())
        .map_err(|error| format!("write crash acknowledgement: {error}"))?;
    std::process::abort();
}

#[allow(dead_code)]
fn _keep_tempdir_type_visible(_: &TempDir) {}
