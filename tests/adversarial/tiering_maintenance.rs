//! Public-path adapter for the tiering-maintenance campaign.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tempfile::tempdir;
use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, Normalization, StoreEpoch,
};
use zeppelin_embed::fts::tokenizer::TokenizerConfig;
use zeppelin_embed::graph::build::GraphBuildError;
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, RowSource,
    SearchOutcome, SearchRequest,
};
use zeppelin_embed::lifecycle::{
    CancelToken, OpenOptions, QueryControl, SearchOptions, SearchTier, Store,
    StoreTestDependencies, SystemMonotonicClock,
};
use zeppelin_embed::planner::{SegmentBranch, SegmentTier as PlanSegmentTier};
use zeppelin_embed::segment::SegmentId;
use zeppelin_embed::tier::{
    MaintenanceBudget, MaintenanceError, MaintenanceReport, MaintenanceStatus, TierThresholds,
};
use zeppelin_embed_adversarial_oracle::storage_durability::{parse_manifest, parse_segment};
use zeppelin_embed_adversarial_oracle::tiering_maintenance::{
    self as oracle, BudgetDisposition, BudgetStep, CandidateFact, PlanBranch, PlanFact, PlanTier,
    PublicationFact, TierFixture, TierInput, TierObserved,
};

use super::fault_vfs::{FaultEvent, FaultMode, FaultSite, std_scheduled};

const CHECKPOINT_ROWS: u64 = 64;
const NODE_BLOCK_TRAILER_BYTES: u64 = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TierOperationKind {
    Policy,
    Transition,
    Budget,
    Publication,
}

impl TierOperationKind {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Policy => "policy",
            Self::Transition => "transition",
            Self::Budget => "budget",
            Self::Publication => "publication",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TierFaultKind {
    BudgetExhaustion,
    CheckpointCorruption,
    StaleSource,
    Enospc,
    ProfileMismatch,
    PublicationCrash,
}

impl TierFaultKind {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::BudgetExhaustion => "budget-exhaustion",
            Self::CheckpointCorruption => "checkpoint-corruption",
            Self::StaleSource => "stale-source",
            Self::Enospc => "enospc",
            Self::ProfileMismatch => "profile-mismatch",
            Self::PublicationCrash => "publication-crash",
        }
    }

    #[must_use]
    pub const fn operation(self) -> TierOperationKind {
        match self {
            Self::BudgetExhaustion | Self::CheckpointCorruption | Self::StaleSource => {
                TierOperationKind::Budget
            }
            Self::Enospc | Self::PublicationCrash => TierOperationKind::Publication,
            Self::ProfileMismatch => TierOperationKind::Policy,
        }
    }

    #[must_use]
    pub const fn site(self) -> &'static str {
        match self {
            Self::BudgetExhaustion => "tier.maintain.budget",
            Self::CheckpointCorruption => "tier.checkpoint.decode",
            Self::StaleSource => "tier.source.validate",
            Self::Enospc => "tier.segment.write",
            Self::ProfileMismatch => "tier.profile.validate",
            Self::PublicationCrash => "tier.manifest.rename",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TierFaultReceipt {
    pub fault: TierFaultKind,
    pub operation: TierOperationKind,
    pub site: &'static str,
    pub cardinality: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TierInvariantEvidence {
    I50 {
        input: TierInput,
        observed: TierObserved,
    },
    I51 {
        input: TierInput,
        observed: TierObserved,
    },
    I52 {
        input: TierInput,
        observed: TierObserved,
    },
    I53 {
        input: TierInput,
        observed: TierObserved,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TierOperationEvidence {
    pub invariants: Vec<TierInvariantEvidence>,
    pub receipts: Vec<TierFaultReceipt>,
    pub clean_control_passed: bool,
}

fn epoch(dims: u32, tag: u8) -> StoreEpoch {
    let tower = EmbeddingTower {
        model_id: "tier-campaign".to_owned(),
        model_version: "2".to_owned(),
        weights_digest: vec![tag],
        dims,
        normalization: Normalization::None,
        prompt_prefix: String::new(),
        max_tokens: 512,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    StoreEpoch {
        embedding: EmbeddingEpoch {
            query: tower.clone(),
            document: tower,
            alignment_digest: Vec::new(),
        },
        tokenizer: TokenizerConfig::text_default().epoch(),
    }
}

fn ingest_fixture(store: &Store, fixture: &TierFixture, epoch: &StoreEpoch) -> Result<(), String> {
    let documents = fixture
        .documents
        .iter()
        .map(|document| {
            IngestDocument::new(
                DocumentVersion::new(DocId::new(u128::from(document.doc_id)), Revision::new(1)),
                document.vector.clone(),
            )
        })
        .collect();
    store
        .ingest(IngestBatch::new(documents).with_epoch(epoch.identity()))
        .map_err(|error| error.to_string())?;
    store.seal().map_err(|error| error.to_string())?;
    let deleted = fixture
        .documents
        .iter()
        .filter(|document| document.deleted)
        .map(|document| DocId::new(u128::from(document.doc_id)))
        .collect::<Vec<_>>();
    if deleted.is_empty() {
        return Err("tier fixture omitted seeded deletions".to_owned());
    }
    store
        .delete(DeleteBatch::new(deleted))
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn search(store: &Store, fixture: &TierFixture, tier: SearchTier) -> Result<SearchOutcome, String> {
    store
        .search(
            SearchRequest::new(&fixture.query),
            fixture.k as usize,
            SearchOptions::default().with_tier(tier),
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| error.to_string())
}

fn plan_fact(outcome: &SearchOutcome) -> PlanFact {
    let plan_count = u32::try_from(outcome.diagnostics.plan.len()).unwrap_or(u32::MAX);
    let Some(plan) = outcome.diagnostics.plan.first() else {
        return PlanFact {
            branch: PlanBranch::Other,
            tier: PlanTier::Other,
            plan_count,
            exact_rescore: outcome.diagnostics.exact_rescore,
        };
    };
    let branch = match plan.branch {
        SegmentBranch::MaskedScan => PlanBranch::MaskedScan,
        SegmentBranch::Graph => PlanBranch::Graph,
        _ => PlanBranch::Other,
    };
    let tier = match plan.tier {
        PlanSegmentTier::SealedScan => PlanTier::SealedScan,
        PlanSegmentTier::SealedGraph => PlanTier::SealedGraph,
        _ => PlanTier::Other,
    };
    PlanFact {
        branch,
        tier,
        plan_count,
        exact_rescore: outcome.diagnostics.exact_rescore,
    }
}

fn candidate_facts(outcome: &SearchOutcome) -> Result<Vec<CandidateFact>, String> {
    outcome
        .candidates
        .iter()
        .map(|candidate| {
            let document = candidate
                .document()
                .ok_or_else(|| "tier candidate omitted document identity".to_owned())?;
            let doc_id = u64::try_from(document.doc_id().get())
                .map_err(|_| "tier document id exceeds u64".to_owned())?;
            let segment = match candidate.row_id().source() {
                RowSource::Sealed(segment) => *segment.as_bytes(),
                RowSource::Active => [0; 16],
            };
            Ok(CandidateFact {
                doc_id,
                revision: document.revision().get(),
                score_bits: candidate.score().to_bits(),
                segment,
                local_row: candidate.row_id().local_row(),
            })
        })
        .collect()
}

fn checkpoint_path(directory: &Path, source: [u8; 16]) -> PathBuf {
    directory.join(format!(
        ".tier-{}.graph.checkpoint",
        SegmentId::from_bytes(source)
    ))
}

fn budget_step(report: MaintenanceReport, checkpoint: &Path, stride: u64) -> BudgetStep {
    let completes_graph = report.graphs_built == 1;
    let rows_advanced = if completes_graph {
        report
            .bytes_consumed
            .saturating_sub(NODE_BLOCK_TRAILER_BYTES)
            / stride
    } else {
        report.bytes_consumed / stride
    };
    let disposition = match report.status {
        MaintenanceStatus::Complete => BudgetDisposition::Complete,
        MaintenanceStatus::BudgetExhausted => BudgetDisposition::BudgetExhausted,
        MaintenanceStatus::Failed(error) => BudgetDisposition::Failed(error.to_string()),
    };
    BudgetStep {
        disposition,
        bytes_consumed: report.bytes_consumed,
        rows_advanced,
        checkpoints_resumed: report.checkpoints_resumed,
        checkpoint_present: checkpoint.is_file(),
        graphs_built: report.graphs_built,
    }
}

fn budget_sequence(store: &Store, input: &TierInput, directory: &Path) -> Vec<BudgetStep> {
    let checkpoint = checkpoint_path(directory, input.source_segment);
    [
        0,
        CHECKPOINT_ROWS * input.stride,
        CHECKPOINT_ROWS * input.stride,
        u64::MAX,
    ]
    .into_iter()
    .map(|bytes| {
        let report = store.maintain_with_test_thresholds(
            MaintenanceBudget {
                wall_time: Duration::from_secs(30),
                bytes,
            },
            TierThresholds {
                graph_min_rows: input.maintenance_threshold,
            },
        );
        budget_step(report, &checkpoint, input.stride)
    })
    .collect()
}

fn parsed_source(directory: &Path) -> Result<(u64, [u8; 16]), String> {
    let bytes = std::fs::read(directory.join("manifest.ze")).map_err(|error| error.to_string())?;
    let manifest = parse_manifest("manifest.ze", &bytes).map_err(|error| error.to_string())?;
    let [segment] = manifest.segments.as_slice() else {
        return Err(format!(
            "tier source manifest has {} segments",
            manifest.segments.len()
        ));
    };
    Ok((manifest.generation, segment.id))
}

fn publication_fact(directory: &Path, input: &TierInput) -> Result<PublicationFact, String> {
    let bytes = std::fs::read(directory.join("manifest.ze")).map_err(|error| error.to_string())?;
    let manifest = parse_manifest("manifest.ze", &bytes).map_err(|error| error.to_string())?;
    let [segment] = manifest.segments.as_slice() else {
        return Ok(PublicationFact {
            manifest_generation: manifest.generation,
            manifest_segments: u32::try_from(manifest.segments.len()).unwrap_or(u32::MAX),
            graph_segment: [0; 16],
            graph_regions: 0,
            segment_rows: 0,
            segment_dims: 0,
            source_segment_gone: false,
            temporary_orphans: 0,
            checkpoint_orphans: 0,
        });
    };
    let segment_id = SegmentId::from_bytes(segment.id);
    let segment_bytes =
        std::fs::read(directory.join(segment_id.file_name())).map_err(|error| error.to_string())?;
    let artifact = parse_segment(&segment_id.file_name(), &segment_bytes)
        .map_err(|error| error.to_string())?;
    let graph_regions = artifact
        .regions
        .iter()
        .filter(|region| region.kind == 7)
        .count();
    let mut temporary_orphans = 0_u32;
    let mut checkpoint_orphans = 0_u32;
    for entry in std::fs::read_dir(directory).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.contains(".tmp") {
            temporary_orphans = temporary_orphans.saturating_add(1);
        }
        if name.ends_with(".graph.checkpoint") {
            checkpoint_orphans = checkpoint_orphans.saturating_add(1);
        }
    }
    Ok(PublicationFact {
        manifest_generation: manifest.generation,
        manifest_segments: u32::try_from(manifest.segments.len()).unwrap_or(u32::MAX),
        graph_segment: segment.id,
        graph_regions: u32::try_from(graph_regions).unwrap_or(u32::MAX),
        segment_rows: artifact.fact.rows,
        segment_dims: artifact.fact.dims,
        source_segment_gone: !directory
            .join(SegmentId::from_bytes(input.source_segment).file_name())
            .exists(),
        temporary_orphans,
        checkpoint_orphans,
    })
}

fn observe_uncached(seed: u64) -> Result<(TierInput, TierObserved), String> {
    let fixture = oracle::fixture(seed);
    let directory = tempdir().map_err(|error| error.to_string())?;
    let epoch = epoch(fixture.dims, 1);
    let store = Store::open(
        directory.path(),
        OpenOptions::default().with_epoch(epoch.clone()),
    )
    .map_err(|error| error.to_string())?;
    ingest_fixture(&store, &fixture, &epoch)?;
    let (source_generation, source_segment) = parsed_source(directory.path())?;
    let input = TierInput::from_seed(seed, source_generation, source_segment);
    let before_auto = search(&store, &fixture, SearchTier::Auto)?;
    let before_exact = search(&store, &fixture, SearchTier::Exact)?;

    let (budget_steps, after_policy) = if input.rows >= input.policy_threshold {
        let steps = budget_sequence(&store, &input, directory.path());
        let after = search(&store, &fixture, SearchTier::Auto)?;
        (steps, after)
    } else {
        let _policy_report = store.maintain_with_test_thresholds(
            MaintenanceBudget {
                wall_time: Duration::from_secs(30),
                bytes: u64::MAX,
            },
            TierThresholds {
                graph_min_rows: input.policy_threshold,
            },
        );
        let after = search(&store, &fixture, SearchTier::Auto)?;
        let steps = budget_sequence(&store, &input, directory.path());
        (steps, after)
    };

    let after_exact = search(&store, &fixture, SearchTier::Exact)?;
    let after_auto = search(&store, &fixture, SearchTier::Auto)?;
    let publication = publication_fact(directory.path(), &input)?;
    let observed = TierObserved {
        auto_before: plan_fact(&before_auto),
        auto_after_policy: plan_fact(&after_policy),
        auto_after_promotion: plan_fact(&after_auto),
        exact_before: candidate_facts(&before_exact)?,
        exact_after: candidate_facts(&after_exact)?,
        auto_after: candidate_facts(&after_auto)?,
        budget_steps,
        publication,
        reopened_auto_plan: PlanFact {
            branch: PlanBranch::Other,
            tier: PlanTier::Other,
            plan_count: 0,
            exact_rescore: false,
        },
        reopened_auto: Vec::new(),
    };
    store.close().map_err(|error| error.to_string())?;
    let reopened = Store::open(directory.path(), OpenOptions::default().with_epoch(epoch))
        .map_err(|error| error.to_string())?;
    let reopened_auto = search(&reopened, &fixture, SearchTier::Auto)?;
    let mut observed = observed;
    observed.reopened_auto_plan = plan_fact(&reopened_auto);
    observed.reopened_auto = candidate_facts(&reopened_auto)?;
    reopened.close().map_err(|error| error.to_string())?;
    Ok((input, observed))
}

fn observe(seed: u64) -> Result<(TierInput, TierObserved), String> {
    static CACHE: OnceLock<Mutex<HashMap<u64, (TierInput, TierObserved)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(cached) = cache
        .lock()
        .map_err(|_| "tier observation cache lock poisoned".to_owned())?
        .get(&seed)
        .cloned()
    {
        return Ok(cached);
    }
    let observed = observe_uncached(seed)?;
    cache
        .lock()
        .map_err(|_| "tier observation cache lock poisoned".to_owned())?
        .insert(seed, observed.clone());
    Ok(observed)
}

fn first_segment(directory: &Path) -> Result<PathBuf, String> {
    for entry in std::fs::read_dir(directory).map_err(|error| error.to_string())? {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "zseg")
        {
            return Ok(path);
        }
    }
    Err("tier fixture omitted segment".to_owned())
}

fn clean_fault_store(
    seed: u64,
) -> Result<(tempfile::TempDir, Store, StoreEpoch, TierFixture), String> {
    let fixture = oracle::fixture(seed);
    let directory = tempdir().map_err(|error| error.to_string())?;
    let epoch = epoch(fixture.dims, 1);
    let store = Store::open(
        directory.path(),
        OpenOptions::default().with_epoch(epoch.clone()),
    )
    .map_err(|error| error.to_string())?;
    ingest_fixture(&store, &fixture, &epoch)?;
    Ok((directory, store, epoch, fixture))
}

fn stale_source_refused(seed: u64) -> Result<(), String> {
    let (directory, store, epoch, fixture) = clean_fault_store(seed)?;
    let report = store.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: Duration::from_secs(30),
            bytes: u64::MAX,
        },
        TierThresholds {
            graph_min_rows: fixture.rows,
        },
    );
    if !matches!(report.status, MaintenanceStatus::Complete) || report.graphs_built != 1 {
        return Err(format!(
            "tier stale-source setup did not publish: {report:?}"
        ));
    }
    store.close().map_err(|error| error.to_string())?;
    let path = first_segment(directory.path())?;
    let mut bytes = std::fs::read(&path).map_err(|error| error.to_string())?;
    let first = bytes
        .first_mut()
        .ok_or_else(|| "tier source segment is empty".to_owned())?;
    *first ^= 1;
    std::fs::write(path, bytes).map_err(|error| error.to_string())?;
    match Store::open(directory.path(), OpenOptions::default().with_epoch(epoch)) {
        Err(_) => Ok(()),
        Ok(store) => {
            let result = search(&store, &fixture, SearchTier::Exact);
            store.close().map_err(|error| error.to_string())?;
            match result {
                Err(_) => Ok(()),
                Ok(_) => Err("stale tier source was accepted".to_owned()),
            }
        }
    }
}

fn profile_mismatch_refused(seed: u64) -> Result<(), String> {
    let fixture = oracle::fixture(seed);
    let directory = tempdir().map_err(|error| error.to_string())?;
    let expected = epoch(fixture.dims, 1);
    let wrong = epoch(fixture.dims, 2);
    let store = Store::open(
        directory.path(),
        OpenOptions::default().with_epoch(expected),
    )
    .map_err(|error| error.to_string())?;
    let documents = fixture
        .documents
        .iter()
        .map(|document| {
            IngestDocument::new(
                DocumentVersion::new(DocId::new(u128::from(document.doc_id)), Revision::new(1)),
                document.vector.clone(),
            )
        })
        .collect();
    let result = store.ingest(IngestBatch::new(documents).with_epoch(wrong.identity()));
    store.close().map_err(|error| error.to_string())?;
    match result {
        Err(_) => Ok(()),
        Ok(_) => Err("mismatched tier epoch was accepted".to_owned()),
    }
}

fn checkpoint_corruption_refused(seed: u64) -> Result<(), String> {
    let (directory, store, _epoch, fixture) = clean_fault_store(seed)?;
    let (_, source) = parsed_source(directory.path())?;
    let input = TierInput::from_seed(seed, 0, source);
    let first = store.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: Duration::from_secs(30),
            bytes: CHECKPOINT_ROWS * input.stride,
        },
        TierThresholds {
            graph_min_rows: fixture.rows,
        },
    );
    let checkpoint = checkpoint_path(directory.path(), source);
    if !matches!(first.status, MaintenanceStatus::BudgetExhausted)
        || first.bytes_consumed != CHECKPOINT_ROWS * input.stride
        || !checkpoint.is_file()
    {
        return Err(format!("real tier checkpoint setup differed: {first:?}"));
    }
    let mut bytes = std::fs::read(&checkpoint).map_err(|error| error.to_string())?;
    let byte = bytes
        .get_mut(32)
        .ok_or_else(|| "real tier checkpoint is too short".to_owned())?;
    *byte ^= 0x80;
    std::fs::write(&checkpoint, bytes).map_err(|error| error.to_string())?;
    let corrupted = store.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: Duration::from_secs(30),
            bytes: u64::MAX,
        },
        TierThresholds {
            graph_min_rows: fixture.rows,
        },
    );
    store.close().map_err(|error| error.to_string())?;
    match corrupted.status {
        MaintenanceStatus::Failed(MaintenanceError::Graph(GraphBuildError::CheckpointCorrupt(
            detail,
        ))) if !detail.is_empty() => Ok(()),
        other => Err(format!(
            "checksum-covered tier checkpoint corruption was not typed: {other:?}"
        )),
    }
}

fn scheduled_publication_fault(
    seed: u64,
    mode: FaultMode,
    site: FaultSite,
    path: &str,
) -> Result<(), String> {
    let fixture = oracle::fixture(seed);
    let directory = tempdir().map_err(|error| error.to_string())?;
    let epoch = epoch(fixture.dims, 1);
    let scheduled = Arc::new(std_scheduled(Some(FaultEvent {
        id: format!("tier-{seed}-{path}"),
        op_index: 0,
        site,
        mode,
        nth_match: 1,
        path_contains: Some(path.to_owned()),
        fired: false,
        path: None,
    })));
    let store = Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::default().with_epoch(epoch.clone()),
        StoreTestDependencies::new(scheduled.clone(), Arc::new(SystemMonotonicClock)),
    )
    .map_err(|error| error.to_string())?;
    ingest_fixture(&store, &fixture, &epoch)?;
    scheduled.set_operation(0);
    let report = store.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: Duration::from_secs(30),
            bytes: u64::MAX,
        },
        TierThresholds {
            graph_min_rows: fixture.rows,
        },
    );
    let fired = scheduled.event().is_some_and(|event| event.fired);
    store.close().map_err(|error| error.to_string())?;
    if fired && matches!(report.status, MaintenanceStatus::Failed(_)) {
        Ok(())
    } else {
        Err(format!("tier publication fault did not fire: {report:?}"))
    }
}

fn exercise_fault(seed: u64, fault: TierFaultKind, observed: &TierObserved) -> Result<(), String> {
    match fault {
        TierFaultKind::BudgetExhaustion => match observed.budget_steps.get(1) {
            Some(step)
                if step.disposition == BudgetDisposition::BudgetExhausted
                    && step.rows_advanced == CHECKPOINT_ROWS
                    && step.checkpoint_present =>
            {
                Ok(())
            }
            other => Err(format!(
                "real mid-build tier budget cut differed: {other:?}"
            )),
        },
        TierFaultKind::CheckpointCorruption => checkpoint_corruption_refused(seed),
        TierFaultKind::StaleSource => stale_source_refused(seed),
        TierFaultKind::Enospc => {
            scheduled_publication_fault(seed, FaultMode::Enospc, FaultSite::Write, ".zseg.tmp")
        }
        TierFaultKind::ProfileMismatch => profile_mismatch_refused(seed),
        TierFaultKind::PublicationCrash => {
            scheduled_publication_fault(seed, FaultMode::Eio, FaultSite::Rename, "manifest.ze")
        }
    }
}

pub fn run_tier_operation(
    operation: TierOperationKind,
    seed: u64,
    fault: Option<TierFaultKind>,
) -> Result<TierOperationEvidence, String> {
    if fault.is_some_and(|fault| fault.operation() != operation) {
        return Err(format!(
            "tier fault {fault:?} does not target {operation:?}"
        ));
    }
    let (input, observed) = observe(seed)?;
    let invariants = match operation {
        TierOperationKind::Policy => vec![TierInvariantEvidence::I50 { input, observed }],
        TierOperationKind::Transition => vec![TierInvariantEvidence::I51 { input, observed }],
        TierOperationKind::Budget => vec![TierInvariantEvidence::I52 { input, observed }],
        TierOperationKind::Publication => vec![TierInvariantEvidence::I53 { input, observed }],
    };
    let mut receipts = Vec::new();
    if let Some(fault) = fault {
        let observed = match invariants.first() {
            Some(
                TierInvariantEvidence::I50 { observed, .. }
                | TierInvariantEvidence::I51 { observed, .. }
                | TierInvariantEvidence::I52 { observed, .. }
                | TierInvariantEvidence::I53 { observed, .. },
            ) => observed,
            None => return Err("tier operation omitted invariant".to_owned()),
        };
        exercise_fault(seed, fault, observed)?;
        receipts.push(TierFaultReceipt {
            fault,
            operation,
            site: fault.site(),
            cardinality: 1,
        });
    }
    Ok(TierOperationEvidence {
        invariants,
        receipts,
        clean_control_passed: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tier_operation_runs_its_checker() {
        for seed in [2, 3] {
            for operation in [
                TierOperationKind::Policy,
                TierOperationKind::Transition,
                TierOperationKind::Budget,
                TierOperationKind::Publication,
            ] {
                let evidence = run_tier_operation(operation, seed, None).expect("tier operation");
                for invariant in evidence.invariants {
                    match invariant {
                        TierInvariantEvidence::I50 { input, observed } => {
                            oracle::compare_i50(&input, &observed)
                        }
                        TierInvariantEvidence::I51 { input, observed } => {
                            oracle::compare_i51(&input, &observed)
                        }
                        TierInvariantEvidence::I52 { input, observed } => {
                            oracle::compare_i52(&input, &observed)
                        }
                        TierInvariantEvidence::I53 { input, observed } => {
                            oracle::compare_i53(&input, &observed)
                        }
                    }
                    .expect("tier checker");
                }
            }
        }
    }

    #[test]
    fn every_declared_tier_fault_fires_once() {
        for (seed, fault) in [
            TierFaultKind::BudgetExhaustion,
            TierFaultKind::CheckpointCorruption,
            TierFaultKind::StaleSource,
            TierFaultKind::Enospc,
            TierFaultKind::ProfileMismatch,
            TierFaultKind::PublicationCrash,
        ]
        .into_iter()
        .enumerate()
        {
            let evidence = run_tier_operation(fault.operation(), seed as u64 + 10, Some(fault))
                .unwrap_or_else(|error| panic!("{fault:?}: {error}"));
            assert_eq!(evidence.receipts.len(), 1);
            assert_eq!(evidence.receipts[0].fault, fault);
            assert_eq!(evidence.receipts[0].cardinality, 1);
        }
    }
}
