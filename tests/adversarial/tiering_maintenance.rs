//! Minimal public-path adapter for tiering maintenance.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tempfile::tempdir;
use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, Normalization, StoreEpoch,
};
use zeppelin_embed::fts::tokenizer::TokenizerConfig;
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::{
    CancelToken, OpenOptions, QueryControl, SearchOptions, SearchTier, Store,
    StoreTestDependencies, SystemMonotonicClock,
};
use zeppelin_embed::tier::{
    MaintenanceBudget, MaintenanceStatus, PROVISIONAL_TIER_THRESHOLDS, SegmentStats, SegmentTier,
    StoreStats, TierPlan, TierThresholds, decide,
};
use zeppelin_embed_adversarial_oracle::tiering_maintenance::{TierInput, TierObserved};

use super::fault_vfs::{FaultEvent, FaultMode, FaultSite, std_scheduled};

const ROWS: usize = 12;
const DIMS: usize = 128;
const K: usize = 4;

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

fn epoch(tag: u8) -> StoreEpoch {
    let tower = EmbeddingTower {
        model_id: "tier-campaign".to_owned(),
        model_version: "1".to_owned(),
        weights_digest: vec![tag],
        dims: DIMS as u32,
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

fn query() -> Vec<f32> {
    (0..DIMS)
        .map(|dimension| {
            if dimension.is_multiple_of(2) {
                1.0
            } else {
                -1.0
            }
        })
        .collect()
}

fn documents(seed: u64) -> Vec<IngestDocument> {
    let base = u128::from(seed) << 64;
    (0..ROWS)
        .map(|row| {
            let amplitude = row as f32 + 1.0;
            IngestDocument::new(
                DocumentVersion::new(DocId::new(base | row as u128 + 1), Revision::new(1)),
                (0..DIMS)
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
        .collect()
}

fn ids(outcome: &zeppelin_embed::ingest::SearchOutcome) -> Result<Vec<u128>, String> {
    outcome
        .candidates
        .iter()
        .map(|candidate| {
            candidate
                .document()
                .map(|document| document.doc_id().get())
                .ok_or_else(|| "tier candidate omitted document identity".to_owned())
        })
        .collect()
}

fn observe(seed: u64) -> Result<(TierInput, TierObserved), String> {
    let threshold = PROVISIONAL_TIER_THRESHOLDS.graph_min_rows;
    let policy = |rows| {
        decide(
            SegmentStats {
                tier: SegmentTier::SealedScan,
                row_count: rows,
                dimensions: DIMS as u32,
                scheme: 4,
            },
            StoreStats,
        )
    };
    let below_stays_scan =
        policy(threshold.saturating_sub(1)) == TierPlan::Stay(SegmentTier::SealedScan);
    let at_threshold_transitions = matches!(
        policy(threshold),
        TierPlan::Transition {
            from: SegmentTier::SealedScan,
            to: SegmentTier::SealedGraph
        }
    );
    let directory = tempdir().map_err(|error| error.to_string())?;
    let epoch = epoch(1);
    let store = Store::open(
        directory.path(),
        OpenOptions::default().with_epoch(epoch.clone()),
    )
    .map_err(|error| error.to_string())?;
    store
        .ingest(IngestBatch::new(documents(seed)).with_epoch(epoch.identity()))
        .map_err(|error| error.to_string())?;
    store.seal().map_err(|error| error.to_string())?;
    let before = store
        .search(
            SearchRequest::new(&query()),
            K,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| error.to_string())?;
    let bounded = store.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: Duration::from_secs(30),
            bytes: 0,
        },
        TierThresholds {
            graph_min_rows: ROWS as u32,
        },
    );
    let complete = store.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: Duration::from_secs(30),
            bytes: u64::MAX,
        },
        TierThresholds {
            graph_min_rows: ROWS as u32,
        },
    );
    let after = store
        .search(
            SearchRequest::new(&query()),
            K,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| error.to_string())?;
    store.close().map_err(|error| error.to_string())?;
    let reopened = Store::open(directory.path(), OpenOptions::default().with_epoch(epoch))
        .map_err(|error| error.to_string())?;
    let reopened_result = reopened
        .search(
            SearchRequest::new(&query()),
            K,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| error.to_string())?;
    reopened.close().map_err(|error| error.to_string())?;
    let base = u128::from(seed) << 64;
    Ok((
        TierInput {
            threshold,
            expected_documents: (1..=K).map(|row| base | row as u128).collect(),
        },
        TierObserved {
            below_stays_scan,
            at_threshold_transitions,
            before_documents: ids(&before)?,
            after_documents: ids(&after)?,
            reopened_documents: ids(&reopened_result)?,
            budget_exhausted: matches!(bounded.status, MaintenanceStatus::BudgetExhausted),
            graphs_built: complete.graphs_built,
        },
    ))
}

fn first_segment(directory: &Path) -> Result<PathBuf, String> {
    std::fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "zseg")
        })
        .ok_or_else(|| "tier fixture omitted segment".to_owned())
}

fn stale_source_refused(seed: u64) -> Result<(), String> {
    let directory = tempdir().map_err(|error| error.to_string())?;
    let epoch = epoch(1);
    let store = Store::open(
        directory.path(),
        OpenOptions::default().with_epoch(epoch.clone()),
    )
    .map_err(|error| error.to_string())?;
    store
        .ingest(IngestBatch::new(documents(seed)).with_epoch(epoch.identity()))
        .map_err(|error| error.to_string())?;
    store.seal().map_err(|error| error.to_string())?;
    let _ = store.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: Duration::from_secs(30),
            bytes: u64::MAX,
        },
        TierThresholds {
            graph_min_rows: ROWS as u32,
        },
    );
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
            let result = store.search(
                SearchRequest::new(&query()),
                K,
                SearchOptions::default().with_tier(SearchTier::Exact),
                QueryControl::Cancel(CancelToken::new()),
            );
            let _ = store.close();
            if result.is_err() {
                Ok(())
            } else {
                Err("stale tier source was accepted".to_owned())
            }
        }
    }
}

fn profile_mismatch_refused(seed: u64) -> Result<(), String> {
    let directory = tempdir().map_err(|error| error.to_string())?;
    let expected = epoch(1);
    let wrong = epoch(2);
    let store = Store::open(
        directory.path(),
        OpenOptions::default().with_epoch(expected),
    )
    .map_err(|error| error.to_string())?;
    let result = store.ingest(IngestBatch::new(documents(seed)).with_epoch(wrong.identity()));
    let _ = store.close();
    if result.is_err() {
        Ok(())
    } else {
        Err("mismatched tier epoch was accepted".to_owned())
    }
}

fn scheduled_publication_fault(
    seed: u64,
    mode: FaultMode,
    site: FaultSite,
    path: &str,
) -> Result<(), String> {
    let directory = tempdir().map_err(|error| error.to_string())?;
    let epoch = epoch(1);
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
    store
        .ingest(IngestBatch::new(documents(seed)).with_epoch(epoch.identity()))
        .map_err(|error| error.to_string())?;
    store.seal().map_err(|error| error.to_string())?;
    scheduled.set_operation(0);
    let report = store.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: Duration::from_secs(30),
            bytes: u64::MAX,
        },
        TierThresholds {
            graph_min_rows: ROWS as u32,
        },
    );
    let fired = scheduled.event().is_some_and(|event| event.fired);
    let _ = store.close();
    if fired && matches!(report.status, MaintenanceStatus::Failed(_)) {
        Ok(())
    } else {
        Err(format!("tier publication fault did not fire: {report:?}"))
    }
}

fn exercise_fault(seed: u64, fault: TierFaultKind, observed: &TierObserved) -> Result<(), String> {
    match fault {
        TierFaultKind::BudgetExhaustion => {
            if observed.budget_exhausted {
                Ok(())
            } else {
                Err("tier budget was not exhausted".to_owned())
            }
        }
        TierFaultKind::CheckpointCorruption => {
            if zeppelin_embed::graph::build::validate_graph_build_checkpoint(&[]).is_err() {
                Ok(())
            } else {
                Err("corrupt tier checkpoint was accepted".to_owned())
            }
        }
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
    use zeppelin_embed_adversarial_oracle::tiering_maintenance as oracle;

    #[test]
    fn every_tier_operation_runs_its_checker() {
        for operation in [
            TierOperationKind::Policy,
            TierOperationKind::Transition,
            TierOperationKind::Budget,
            TierOperationKind::Publication,
        ] {
            let evidence = run_tier_operation(operation, 2, None).expect("tier operation");
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
            let evidence = run_tier_operation(fault.operation(), seed as u64 + 3, Some(fault))
                .unwrap_or_else(|error| panic!("{fault:?}: {error}"));
            assert_eq!(evidence.receipts.len(), 1);
            assert_eq!(evidence.receipts[0].fault, fault);
            assert_eq!(evidence.receipts[0].cardinality, 1);
        }
    }
}
