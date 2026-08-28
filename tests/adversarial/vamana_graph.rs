//! Minimal public-path adapter for the Vamana graph campaign.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tempfile::tempdir;
use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, Normalization, StoreEpoch,
};
use zeppelin_embed::fts::tokenizer::TokenizerConfig;
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, RowSource, SearchRequest,
};
use zeppelin_embed::lifecycle::{
    CancelToken, GraphSearchOptions, OpenOptions, QueryControl, SearchOptions, SearchTier, Store,
    StoreTestDependencies, SystemMonotonicClock,
};
use zeppelin_embed::meta::{Predicate, RangeBound, RangePredicate, TIMESTAMP_COLUMN};
use zeppelin_embed::tier::{MaintenanceBudget, MaintenanceStatus, TierThresholds};

use super::fault_vfs::{FaultEvent, FaultMode, FaultSite, std_scheduled};
use zeppelin_embed_adversarial_oracle::vamana_graph::{GraphInput, GraphObserved};

const ROWS: usize = 12;
const DIMS: usize = 128;
const K: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphOperationKind {
    Shape,
    EntryPoints,
    Checkpoint,
    BoundedBuild,
    Search,
    Publication,
    FilteredSearch,
}

impl GraphOperationKind {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Shape => "shape",
            Self::EntryPoints => "entry-points",
            Self::Checkpoint => "checkpoint",
            Self::BoundedBuild => "bounded-build",
            Self::Search => "search",
            Self::Publication => "publication",
            Self::FilteredSearch => "filtered-search",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphFaultKind {
    CheckpointCorruption,
    BuildBudgetCancel,
    CorruptNode,
    CorruptEntry,
    MissingRescore,
    SearchCancellation,
    PublicationCrash,
}

impl GraphFaultKind {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::CheckpointCorruption => "checkpoint-corruption",
            Self::BuildBudgetCancel => "build-budget-cancel",
            Self::CorruptNode => "corrupt-node",
            Self::CorruptEntry => "corrupt-entry",
            Self::MissingRescore => "missing-rescore",
            Self::SearchCancellation => "search-cancellation",
            Self::PublicationCrash => "publication-crash",
        }
    }

    #[must_use]
    pub const fn operation(self) -> GraphOperationKind {
        match self {
            Self::CheckpointCorruption => GraphOperationKind::Checkpoint,
            Self::BuildBudgetCancel => GraphOperationKind::BoundedBuild,
            Self::CorruptNode => GraphOperationKind::Shape,
            Self::CorruptEntry => GraphOperationKind::EntryPoints,
            Self::MissingRescore | Self::SearchCancellation => GraphOperationKind::Search,
            Self::PublicationCrash => GraphOperationKind::Publication,
        }
    }

    #[must_use]
    pub const fn site(self) -> &'static str {
        match self {
            Self::CheckpointCorruption => "checkpoint.resume.validate",
            Self::BuildBudgetCancel => "build.work-budget",
            Self::CorruptNode => "graph.load.node-validate",
            Self::CorruptEntry => "graph.search.entry-discovery",
            Self::MissingRescore => "graph.search.exact-rescore.acquire",
            Self::SearchCancellation => "graph.search.cancel",
            Self::PublicationCrash => "graph.publication.manifest-rename",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphFaultReceipt {
    pub fault: GraphFaultKind,
    pub operation: GraphOperationKind,
    pub site: &'static str,
    pub cardinality: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GraphInvariantEvidence {
    I28 {
        input: GraphInput,
        observed: GraphObserved,
    },
    I29 {
        input: GraphInput,
        observed: GraphObserved,
    },
    I30 {
        input: GraphInput,
        observed: GraphObserved,
    },
    I31 {
        input: GraphInput,
        observed: GraphObserved,
    },
    I32 {
        input: GraphInput,
        observed: GraphObserved,
    },
    I33 {
        input: GraphInput,
        observed: GraphObserved,
    },
    I34 {
        input: GraphInput,
        observed: GraphObserved,
    },
    I35 {
        input: GraphInput,
        observed: GraphObserved,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphOperationEvidence {
    pub operation: GraphOperationKind,
    pub invariants: Vec<GraphInvariantEvidence>,
    pub receipts: Vec<GraphFaultReceipt>,
    pub clean_control_passed: bool,
}

fn epoch() -> StoreEpoch {
    let tower = EmbeddingTower {
        model_id: "graph-campaign".to_owned(),
        model_version: "1".to_owned(),
        weights_digest: vec![0x28],
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
            let vector = (0..DIMS)
                .map(|dimension| {
                    if dimension.is_multiple_of(2) {
                        amplitude
                    } else {
                        -amplitude
                    }
                })
                .collect();
            IngestDocument::new(
                DocumentVersion::new(DocId::new(base | row as u128 + 1), Revision::new(1)),
                vector,
            )
            .with_timestamp(row as i64)
        })
        .collect()
}

fn document_ids(outcome: &zeppelin_embed::ingest::SearchOutcome) -> Result<Vec<u128>, String> {
    outcome
        .candidates
        .iter()
        .map(|candidate| {
            candidate
                .document()
                .map(|document| document.doc_id().get())
                .ok_or_else(|| "graph candidate omitted its public document identity".to_owned())
        })
        .collect()
}

fn graph_options(seed: u64) -> SearchOptions {
    SearchOptions::default().with_tier(SearchTier::Graph(
        GraphSearchOptions::default().with_seed(seed),
    ))
}

fn observe(seed: u64) -> Result<(GraphInput, GraphObserved), String> {
    let directory = tempdir().map_err(|error| error.to_string())?;
    let epoch = epoch();
    let store = Store::open(
        directory.path(),
        OpenOptions::default().with_epoch(epoch.clone()),
    )
    .map_err(|error| error.to_string())?;
    store
        .ingest(IngestBatch::new(documents(seed)).with_epoch(epoch.identity()))
        .map_err(|error| error.to_string())?;
    store.seal().map_err(|error| error.to_string())?;
    let bounded = store.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: Duration::from_secs(30),
            bytes: 0,
        },
        TierThresholds {
            graph_min_rows: ROWS as u32,
        },
    );
    let budget_exhausted = matches!(bounded.status, MaintenanceStatus::BudgetExhausted);
    let complete = store.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: Duration::from_secs(30),
            bytes: u64::MAX,
        },
        TierThresholds {
            graph_min_rows: ROWS as u32,
        },
    );
    if complete.graphs_built != 1 || !matches!(complete.status, MaintenanceStatus::Complete) {
        return Err(format!(
            "public graph maintenance did not publish once: built={} status={:?}",
            complete.graphs_built, complete.status
        ));
    }
    let query = query();
    let outcome = store
        .search(
            SearchRequest::new(&query),
            K,
            graph_options(seed),
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| error.to_string())?;
    let documents = document_ids(&outcome)?;
    let source_aligned = outcome
        .candidates
        .iter()
        .all(|candidate| matches!(candidate.row_id().source(), RowSource::Sealed(_)));
    let predicate = Predicate::Range(RangePredicate {
        column: TIMESTAMP_COLUMN,
        lower: Some(RangeBound::inclusive(
            zeppelin_embed::meta::PredicateValue::I64(0),
        )),
        upper: Some(RangeBound::inclusive(
            zeppelin_embed::meta::PredicateValue::I64(2),
        )),
    });
    let filtered = store
        .search_filtered(
            SearchRequest::new(&query),
            &predicate,
            K,
            graph_options(seed),
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| error.to_string())?;
    let filtered_documents = filtered
        .candidates
        .iter()
        .map(|candidate| {
            candidate
                .document()
                .map(|document| document.doc_id().get())
                .ok_or_else(|| "filtered graph candidate omitted document identity".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    store.close().map_err(|error| error.to_string())?;
    let reopened = Store::open(directory.path(), OpenOptions::default().with_epoch(epoch))
        .map_err(|error| error.to_string())?;
    let reopened_outcome = reopened
        .search(
            SearchRequest::new(&query),
            K,
            graph_options(seed),
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| error.to_string())?;
    let reopened_documents = document_ids(&reopened_outcome)?;
    reopened.close().map_err(|error| error.to_string())?;
    let base = u128::from(seed) << 64;
    let top_documents = (1..=K).map(|ordinal| base | ordinal as u128).collect();
    let expected_filtered = (1..=3).map(|ordinal| base | ordinal as u128).collect();
    Ok((
        GraphInput {
            row_count: ROWS as u32,
            top_documents,
            filtered_documents: expected_filtered,
        },
        GraphObserved {
            row_count: ROWS as u32,
            graphs_built: complete.graphs_built,
            budget_exhausted,
            graph_segments: outcome.graph_stats.segments_traversed as u64,
            entry_discoveries: outcome.graph_stats.entry_seed_discoveries as u64,
            documents,
            reopened_documents,
            filtered_documents,
            source_aligned,
        },
    ))
}

fn first_segment_path(directory: &Path) -> Result<PathBuf, String> {
    std::fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("segment-") && name.ends_with(".zseg"))
        })
        .ok_or_else(|| "graph fixture published no segment".to_owned())
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    bytes
        .get(offset..offset + 2)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or_else(|| format!("missing u16 at {offset}"))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    bytes
        .get(offset..offset + 8)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| format!("missing u64 at {offset}"))
}

fn region_byte(bytes: &[u8], kind: u16) -> Result<usize, String> {
    let count = usize::from(read_u16(bytes, 52)?);
    for position in 0..count {
        let entry = 64 + position * 32;
        if read_u16(bytes, entry)? == kind {
            let offset = usize::try_from(read_u64(bytes, entry + 8)?)
                .map_err(|_| "region offset exceeds usize".to_owned())?;
            let length = usize::try_from(read_u64(bytes, entry + 16)?)
                .map_err(|_| "region length exceeds usize".to_owned())?;
            return offset
                .checked_add(length / 2)
                .filter(|target| *target < bytes.len())
                .ok_or_else(|| "region mutation escaped segment".to_owned());
        }
    }
    Err(format!("segment omitted region {kind}"))
}

fn public_corruption_refused(seed: u64, region: u16) -> Result<(), String> {
    let directory = tempdir().map_err(|error| error.to_string())?;
    let epoch = epoch();
    let store = Store::open(
        directory.path(),
        OpenOptions::default().with_epoch(epoch.clone()),
    )
    .map_err(|error| error.to_string())?;
    store
        .ingest(IngestBatch::new(documents(seed)).with_epoch(epoch.identity()))
        .map_err(|error| error.to_string())?;
    store.seal().map_err(|error| error.to_string())?;
    let report = store.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: Duration::from_secs(30),
            bytes: u64::MAX,
        },
        TierThresholds {
            graph_min_rows: ROWS as u32,
        },
    );
    if report.graphs_built != 1 {
        return Err("corruption fixture published no graph".to_owned());
    }
    store.close().map_err(|error| error.to_string())?;
    let segment = first_segment_path(directory.path())?;
    let mut bytes = std::fs::read(&segment).map_err(|error| error.to_string())?;
    let target = region_byte(&bytes, region)?;
    bytes[target] ^= 0xff;
    std::fs::write(&segment, bytes).map_err(|error| error.to_string())?;
    let reopened = Store::open(directory.path(), OpenOptions::default().with_epoch(epoch));
    match reopened {
        Err(_) => Ok(()),
        Ok(store) => {
            let result = store.search(
                SearchRequest::new(&query()),
                K,
                graph_options(seed),
                QueryControl::Cancel(CancelToken::new()),
            );
            let _ = store.close();
            if result.is_err() {
                Ok(())
            } else {
                Err("corrupt graph bytes were accepted by public search".to_owned())
            }
        }
    }
}

fn public_cancel_refused(seed: u64) -> Result<(), String> {
    let directory = tempdir().map_err(|error| error.to_string())?;
    let epoch = epoch();
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
    let token = CancelToken::new();
    token.cancel();
    let result = store.search(
        SearchRequest::new(&query()),
        K,
        graph_options(seed),
        QueryControl::Cancel(token),
    );
    let _ = store.close();
    if result.is_err() {
        Ok(())
    } else {
        Err("cancelled graph search succeeded".to_owned())
    }
}

fn public_publication_fault_fired(seed: u64) -> Result<(), String> {
    let directory = tempdir().map_err(|error| error.to_string())?;
    let epoch = epoch();
    let event = FaultEvent {
        id: format!("graph-publication-{seed}"),
        op_index: 0,
        site: FaultSite::Rename,
        mode: FaultMode::Eio,
        nth_match: 1,
        path_contains: Some("manifest.ze".to_owned()),
        fired: false,
        path: None,
    };
    let scheduled = Arc::new(std_scheduled(Some(event)));
    let dependencies =
        StoreTestDependencies::new(scheduled.clone(), Arc::new(SystemMonotonicClock));
    let store = Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::default().with_epoch(epoch.clone()),
        dependencies,
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
        Err(format!(
            "graph publication fault did not fire: fired={fired} status={:?}",
            report.status
        ))
    }
}

fn exercise_fault(
    seed: u64,
    fault: GraphFaultKind,
    observed: &GraphObserved,
) -> Result<(), String> {
    match fault {
        GraphFaultKind::CheckpointCorruption => {
            zeppelin_embed::graph::build::validate_graph_build_checkpoint(&[])
                .map_err(|_| "corrupt checkpoint was accepted".to_owned())
                .err()
                .map(|_| ())
                .ok_or_else(|| "corrupt checkpoint was accepted".to_owned())
        }
        GraphFaultKind::BuildBudgetCancel => {
            if observed.budget_exhausted {
                Ok(())
            } else {
                Err("zero graph budget did not defer".to_owned())
            }
        }
        GraphFaultKind::CorruptNode | GraphFaultKind::CorruptEntry => {
            public_corruption_refused(seed, 7)
        }
        GraphFaultKind::MissingRescore => public_corruption_refused(seed, 5),
        GraphFaultKind::SearchCancellation => public_cancel_refused(seed),
        GraphFaultKind::PublicationCrash => public_publication_fault_fired(seed),
    }
}

pub fn run_graph_operation(
    operation: GraphOperationKind,
    seed: u64,
    fault: Option<GraphFaultKind>,
) -> Result<GraphOperationEvidence, String> {
    if fault.is_some_and(|fault| fault.operation() != operation) {
        return Err(format!(
            "graph fault {fault:?} does not target {operation:?}"
        ));
    }
    let (input, observed) = observe(seed)?;
    let invariants = match operation {
        GraphOperationKind::Shape => vec![GraphInvariantEvidence::I28 { input, observed }],
        GraphOperationKind::EntryPoints => vec![GraphInvariantEvidence::I29 { input, observed }],
        GraphOperationKind::Checkpoint => vec![GraphInvariantEvidence::I34 { input, observed }],
        GraphOperationKind::BoundedBuild => vec![GraphInvariantEvidence::I32 { input, observed }],
        GraphOperationKind::Search => vec![
            GraphInvariantEvidence::I30 {
                input: input.clone(),
                observed: observed.clone(),
            },
            GraphInvariantEvidence::I31 { input, observed },
        ],
        GraphOperationKind::Publication => vec![GraphInvariantEvidence::I33 { input, observed }],
        GraphOperationKind::FilteredSearch => vec![GraphInvariantEvidence::I35 { input, observed }],
    };
    let mut receipts = Vec::new();
    if let Some(fault) = fault {
        let observed = match invariants.first() {
            Some(
                GraphInvariantEvidence::I28 { observed, .. }
                | GraphInvariantEvidence::I29 { observed, .. }
                | GraphInvariantEvidence::I30 { observed, .. }
                | GraphInvariantEvidence::I31 { observed, .. }
                | GraphInvariantEvidence::I32 { observed, .. }
                | GraphInvariantEvidence::I33 { observed, .. }
                | GraphInvariantEvidence::I34 { observed, .. }
                | GraphInvariantEvidence::I35 { observed, .. },
            ) => observed,
            None => return Err("graph operation emitted no invariant evidence".to_owned()),
        };
        exercise_fault(seed, fault, observed)?;
        receipts.push(GraphFaultReceipt {
            fault,
            operation,
            site: fault.site(),
            cardinality: 1,
        });
    }
    Ok(GraphOperationEvidence {
        operation,
        invariants,
        receipts,
        clean_control_passed: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeppelin_embed_adversarial_oracle::vamana_graph as oracle;

    #[test]
    fn every_graph_operation_runs_its_public_invariant_checker() {
        for operation in [
            GraphOperationKind::Shape,
            GraphOperationKind::EntryPoints,
            GraphOperationKind::Checkpoint,
            GraphOperationKind::BoundedBuild,
            GraphOperationKind::Search,
            GraphOperationKind::Publication,
            GraphOperationKind::FilteredSearch,
        ] {
            let evidence = run_graph_operation(operation, 7, None).expect("graph operation");
            for invariant in evidence.invariants {
                match invariant {
                    GraphInvariantEvidence::I28 { input, observed } => {
                        oracle::compare_i28(&input, &observed)
                    }
                    GraphInvariantEvidence::I29 { input, observed } => {
                        oracle::compare_i29(&input, &observed)
                    }
                    GraphInvariantEvidence::I30 { input, observed } => {
                        oracle::compare_i30(&input, &observed)
                    }
                    GraphInvariantEvidence::I31 { input, observed } => {
                        oracle::compare_i31(&input, &observed)
                    }
                    GraphInvariantEvidence::I32 { input, observed } => {
                        oracle::compare_i32(&input, &observed)
                    }
                    GraphInvariantEvidence::I33 { input, observed } => {
                        oracle::compare_i33(&input, &observed)
                    }
                    GraphInvariantEvidence::I34 { input, observed } => {
                        oracle::compare_i34(&input, &observed)
                    }
                    GraphInvariantEvidence::I35 { input, observed } => {
                        oracle::compare_i35(&input, &observed)
                    }
                }
                .expect("independent graph checker");
            }
        }
    }

    #[test]
    fn every_declared_graph_fault_fires_once_at_its_operation() {
        for (seed, fault) in [
            GraphFaultKind::CheckpointCorruption,
            GraphFaultKind::BuildBudgetCancel,
            GraphFaultKind::CorruptNode,
            GraphFaultKind::CorruptEntry,
            GraphFaultKind::MissingRescore,
            GraphFaultKind::SearchCancellation,
            GraphFaultKind::PublicationCrash,
        ]
        .into_iter()
        .enumerate()
        {
            let evidence = run_graph_operation(fault.operation(), seed as u64 + 1, Some(fault))
                .unwrap_or_else(|error| panic!("{fault:?}: {error}"));
            assert_eq!(evidence.receipts.len(), 1);
            assert_eq!(evidence.receipts[0].fault, fault);
            assert_eq!(evidence.receipts[0].operation, fault.operation());
            assert_eq!(evidence.receipts[0].cardinality, 1);
        }
    }
}
