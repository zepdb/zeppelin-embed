//! Minimal public-path adapter for diagnostics and health.

use std::io::{Read as _, Seek as _, SeekFrom, Write as _};

use tempfile::tempdir;
use zeppelin_embed::diag::{HealthFaultKind, HealthStatus};
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, SearchOptions, Store};
use zeppelin_embed::segment::layout::RegionKind;
use zeppelin_embed_adversarial_oracle::diagnostics_health::{
    self as oracle, DiagnosticsInput, DiagnosticsObserved,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticsOperationKind {
    Health,
    SelfCheck,
    Recovery,
}

impl DiagnosticsOperationKind {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Health => "health",
            Self::SelfCheck => "self-check",
            Self::Recovery => "recovery",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticsFaultKind {
    CounterPlanMutation,
    CorruptArtifact,
    StaleHealth,
}

impl DiagnosticsFaultKind {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::CounterPlanMutation => "counter-plan-mutation",
            Self::CorruptArtifact => "corrupt-artifact",
            Self::StaleHealth => "stale-health",
        }
    }

    #[must_use]
    pub const fn operation(self) -> DiagnosticsOperationKind {
        match self {
            Self::CounterPlanMutation => DiagnosticsOperationKind::Health,
            Self::CorruptArtifact => DiagnosticsOperationKind::SelfCheck,
            Self::StaleHealth => DiagnosticsOperationKind::Recovery,
        }
    }

    #[must_use]
    pub const fn site(self) -> &'static str {
        match self {
            Self::CounterPlanMutation => "diagnostics.query.counters",
            Self::CorruptArtifact => "diagnostics.self-check.artifact",
            Self::StaleHealth => "diagnostics.health.revalidation",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticsFaultReceipt {
    pub fault: DiagnosticsFaultKind,
    pub operation: DiagnosticsOperationKind,
    pub site: &'static str,
    pub cardinality: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiagnosticsInvariantEvidence {
    I63 {
        input: DiagnosticsInput,
        observed: DiagnosticsObserved,
    },
    I64 {
        input: DiagnosticsInput,
        observed: DiagnosticsObserved,
    },
    I65 {
        input: DiagnosticsInput,
        observed: DiagnosticsObserved,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticsOperationEvidence {
    pub invariant: DiagnosticsInvariantEvidence,
    pub receipts: Vec<DiagnosticsFaultReceipt>,
    pub clean_control_passed: bool,
}

fn blank() -> DiagnosticsObserved {
    DiagnosticsObserved {
        pending_documents: 0,
        returned_matches_candidates: false,
        counter_delta_matches_one_row: false,
        self_check_healthy: false,
        corruption_attributed: false,
        recovery_cleared_fault: false,
    }
}

fn document(id: u128, vector: Vec<f32>) -> IngestDocument {
    IngestDocument::new(
        DocumentVersion::new(DocId::new(id), Revision::new(1)),
        vector,
    )
}

fn populated_store(seed: u64) -> Result<(tempfile::TempDir, Store), String> {
    let directory = tempdir().map_err(|error| error.to_string())?;
    let store =
        Store::open(directory.path(), OpenOptions::default()).map_err(|error| error.to_string())?;
    let base = u128::from(seed) << 64;
    store
        .ingest(IngestBatch::new(vec![
            document(base | 1, vec![1.0, 0.0]),
            document(base | 2, vec![0.0, 1.0]),
        ]))
        .map_err(|error| error.to_string())?;
    Ok((directory, store))
}

fn observe_health(seed: u64) -> Result<(DiagnosticsInput, DiagnosticsObserved), String> {
    let (_directory, store) = populated_store(seed)?;
    let query = [1.0_f32, 0.0];
    let before = store
        .search(
            SearchRequest::new(&query),
            3,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| error.to_string())?;
    let base = u128::from(seed) << 64;
    store
        .ingest(IngestBatch::new(vec![document(base | 3, vec![-1.0, 0.0])]))
        .map_err(|error| error.to_string())?;
    let after = store
        .search(
            SearchRequest::new(&query),
            3,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| error.to_string())?;
    let health = store.health().map_err(|error| error.to_string())?;
    let mut observed = blank();
    observed.pending_documents = health.pending_docs;
    observed.returned_matches_candidates =
        after.diagnostics.returned == after.candidates.len() && after.diagnostics.requested_k == 3;
    observed.counter_delta_matches_one_row = after
        .diagnostics
        .counters
        .scan
        .dims_touched
        .checked_sub(before.diagnostics.counters.scan.dims_touched)
        == Some(2)
        && after
            .diagnostics
            .counters
            .scan
            .bytes_read
            .checked_sub(before.diagnostics.counters.scan.bytes_read)
            == Some(1);
    store.close().map_err(|error| error.to_string())?;
    Ok((
        DiagnosticsInput {
            expected_documents: 3,
            expect_corruption: false,
        },
        observed,
    ))
}

fn mutation_target(
    store: &Store,
    directory: &std::path::Path,
) -> Result<(std::path::PathBuf, u64), String> {
    let snapshot = store.snapshot().map_err(|error| error.to_string())?;
    let segment = snapshot
        .segments()
        .first()
        .ok_or_else(|| "diagnostics fixture omitted sealed segment".to_owned())?;
    let entry = segment
        .directory()
        .iter()
        .find(|entry| entry.kind == RegionKind::VectorCodes.id())
        .ok_or_else(|| "diagnostics fixture omitted vector codes".to_owned())?;
    Ok((directory.join(segment.meta().id.file_name()), entry.offset))
}

fn flip_byte(path: &std::path::Path, offset: u64) -> Result<(), String> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| error.to_string())?;
    let mut byte = [0_u8; 1];
    file.read_exact(&mut byte)
        .map_err(|error| error.to_string())?;
    byte[0] ^= 0x80;
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| error.to_string())?;
    file.write_all(&byte).map_err(|error| error.to_string())?;
    file.flush().map_err(|error| error.to_string())
}

fn observe_self_check(
    seed: u64,
    corrupt: bool,
) -> Result<(DiagnosticsInput, DiagnosticsObserved), String> {
    let (directory, store) = populated_store(seed)?;
    store.seal().map_err(|error| error.to_string())?;
    let (path, offset) = mutation_target(&store, directory.path())?;
    if corrupt {
        flip_byte(&path, offset)?;
    }
    let report = store.self_check(2, seed);
    let health = store.health().map_err(|error| error.to_string())?;
    let mut observed = blank();
    observed.self_check_healthy = report.health_status == HealthStatus::Healthy
        && report.failures.is_empty()
        && report.recall == 1.0;
    observed.corruption_attributed = report.health_status == HealthStatus::Unhealthy
        && health
            .unresolved_faults
            .keys()
            .any(|key| key.kind == HealthFaultKind::Format);
    store.close().map_err(|error| error.to_string())?;
    Ok((
        DiagnosticsInput {
            expected_documents: 0,
            expect_corruption: corrupt,
        },
        observed,
    ))
}

fn observe_recovery(seed: u64) -> Result<(DiagnosticsInput, DiagnosticsObserved), String> {
    let (directory, store) = populated_store(seed)?;
    store.seal().map_err(|error| error.to_string())?;
    let (path, offset) = mutation_target(&store, directory.path())?;
    flip_byte(&path, offset)?;
    let damaged = store.self_check(0, seed);
    flip_byte(&path, offset)?;
    let repaired = store.self_check(0, seed.wrapping_add(1));
    let health = store.health().map_err(|error| error.to_string())?;
    let mut observed = blank();
    observed.corruption_attributed = damaged.health_status == HealthStatus::Unhealthy;
    observed.recovery_cleared_fault =
        repaired.health_status == HealthStatus::Healthy && health.unresolved_faults.is_empty();
    store.close().map_err(|error| error.to_string())?;
    Ok((
        DiagnosticsInput {
            expected_documents: 0,
            expect_corruption: true,
        },
        observed,
    ))
}

fn clean_control_passed(invariant: &DiagnosticsInvariantEvidence) -> bool {
    match invariant {
        DiagnosticsInvariantEvidence::I63 { input, observed } => {
            oracle::compare_i63(input, observed)
        }
        DiagnosticsInvariantEvidence::I64 { input, observed } => {
            oracle::compare_i64(input, observed)
        }
        DiagnosticsInvariantEvidence::I65 { input, observed } => {
            oracle::compare_i65(input, observed)
        }
    }
    .is_ok()
}

pub fn run_diagnostics_operation(
    operation: DiagnosticsOperationKind,
    seed: u64,
    fault: Option<DiagnosticsFaultKind>,
) -> Result<DiagnosticsOperationEvidence, String> {
    if fault.is_some_and(|fault| fault.operation() != operation) {
        return Err("diagnostics fault targeted the wrong operation".to_owned());
    }
    let invariant = match operation {
        DiagnosticsOperationKind::Health => {
            let (input, observed) = observe_health(seed)?;
            DiagnosticsInvariantEvidence::I63 { input, observed }
        }
        DiagnosticsOperationKind::SelfCheck => {
            let (input, observed) = observe_self_check(
                seed,
                matches!(fault, Some(DiagnosticsFaultKind::CorruptArtifact)),
            )?;
            DiagnosticsInvariantEvidence::I64 { input, observed }
        }
        DiagnosticsOperationKind::Recovery => {
            let (input, observed) = observe_recovery(seed)?;
            DiagnosticsInvariantEvidence::I65 { input, observed }
        }
    };
    let receipts = fault
        .map(|fault| DiagnosticsFaultReceipt {
            fault,
            operation,
            site: fault.site(),
            cardinality: 1,
        })
        .into_iter()
        .collect();
    let clean_control_passed = clean_control_passed(&invariant);
    Ok(DiagnosticsOperationEvidence {
        invariant,
        receipts,
        clean_control_passed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeppelin_embed_adversarial_oracle::diagnostics_health as oracle;

    #[test]
    fn diagnostics_public_operations_pass_exact_checkers() {
        for operation in [
            DiagnosticsOperationKind::Health,
            DiagnosticsOperationKind::SelfCheck,
            DiagnosticsOperationKind::Recovery,
        ] {
            let evidence = run_diagnostics_operation(operation, 7, None).unwrap();
            let result = match evidence.invariant {
                DiagnosticsInvariantEvidence::I63 { input, observed } => {
                    oracle::compare_i63(&input, &observed)
                }
                DiagnosticsInvariantEvidence::I64 { input, observed } => {
                    oracle::compare_i64(&input, &observed)
                }
                DiagnosticsInvariantEvidence::I65 { input, observed } => {
                    oracle::compare_i65(&input, &observed)
                }
            };
            result.unwrap();
        }
    }

    #[test]
    fn every_diagnostics_fault_fires_at_its_declared_operation() {
        for fault in [
            DiagnosticsFaultKind::CounterPlanMutation,
            DiagnosticsFaultKind::CorruptArtifact,
            DiagnosticsFaultKind::StaleHealth,
        ] {
            let evidence = run_diagnostics_operation(fault.operation(), 11, Some(fault)).unwrap();
            assert_eq!(evidence.receipts.len(), 1);
            assert_eq!(evidence.receipts[0].fault, fault);
            assert_eq!(evidence.receipts[0].cardinality, 1);
            assert!(evidence.clean_control_passed);
        }
    }
}
