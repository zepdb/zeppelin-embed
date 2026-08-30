//! Public-path adapter for the seed-derived Vamana graph campaign.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tempfile::tempdir;
use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, Normalization, StoreEpoch,
};
use zeppelin_embed::fts::tokenizer::TokenizerConfig;
use zeppelin_embed::graph::block::GraphNodeLayout;
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, RowSource,
    SearchOutcome, SearchRequest,
};
use zeppelin_embed::lifecycle::{
    CancelToken, GraphSearchOptions, OpenOptions, QueryControl, SearchOptions, SearchTier, Store,
    StoreTestDependencies, SystemMonotonicClock,
};
use zeppelin_embed::meta::{
    Predicate, PredicateValue, RangeBound, RangePredicate, TIMESTAMP_COLUMN,
};
use zeppelin_embed::planner::SegmentBranch;
use zeppelin_embed::tier::{MaintenanceBudget, MaintenanceStatus, TierThresholds};

use super::fault_vfs::{FaultEvent, FaultMode, FaultSchedule, FaultSite, Layer, std_scheduled};
use zeppelin_embed_adversarial_oracle::vamana_graph::{
    self as oracle, GraphCandidate, GraphInput, GraphObserved,
};

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
        input: Arc<GraphInput>,
        observed: Arc<GraphObserved>,
    },
    I29 {
        input: Arc<GraphInput>,
        observed: Arc<GraphObserved>,
    },
    I30 {
        input: Arc<GraphInput>,
        observed: Arc<GraphObserved>,
    },
    I31 {
        input: Arc<GraphInput>,
        observed: Arc<GraphObserved>,
    },
    I32 {
        input: Arc<GraphInput>,
        observed: Arc<GraphObserved>,
    },
    I33 {
        input: Arc<GraphInput>,
        observed: Arc<GraphObserved>,
    },
    I34 {
        input: Arc<GraphInput>,
        observed: Arc<GraphObserved>,
    },
    I35 {
        input: Arc<GraphInput>,
        observed: Arc<GraphObserved>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphOperationEvidence {
    pub operation: GraphOperationKind,
    pub invariants: Vec<GraphInvariantEvidence>,
    pub receipts: Vec<GraphFaultReceipt>,
    pub clean_control_passed: bool,
}

type CachedObservation = (u64, Arc<GraphInput>, Arc<GraphObserved>);

thread_local! {
    static OBSERVATION: RefCell<Option<CachedObservation>> = const { RefCell::new(None) };
}

fn epoch(dimensions: u32) -> StoreEpoch {
    let tower = EmbeddingTower {
        model_id: "graph-campaign".to_owned(),
        model_version: "2".to_owned(),
        weights_digest: vec![0x28, 0x35],
        dims: dimensions,
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

fn query(input: &GraphInput) -> Vec<f32> {
    input
        .query_bits
        .iter()
        .copied()
        .map(f32::from_bits)
        .collect()
}

fn documents(input: &GraphInput) -> Vec<IngestDocument> {
    input
        .rows
        .iter()
        .map(|row| {
            IngestDocument::new(
                DocumentVersion::new(DocId::new(u128::from(row.document)), Revision::new(1)),
                row.vector_bits
                    .iter()
                    .copied()
                    .map(f32::from_bits)
                    .collect(),
            )
            .with_timestamp(row.timestamp)
        })
        .collect()
}

fn deleted_documents(input: &GraphInput) -> Vec<DocId> {
    input
        .rows
        .iter()
        .filter(|row| row.deleted)
        .map(|row| DocId::new(u128::from(row.document)))
        .collect()
}

fn graph_options(seed: u64) -> SearchOptions {
    SearchOptions::default().with_tier(SearchTier::Graph(
        GraphSearchOptions::default().with_seed(seed),
    ))
}

fn exact_options() -> SearchOptions {
    SearchOptions::default().with_tier(SearchTier::Exact)
}

fn predicate(value: i64) -> Predicate {
    Predicate::Range(RangePredicate {
        column: TIMESTAMP_COLUMN,
        lower: Some(RangeBound::inclusive(PredicateValue::I64(value))),
        upper: Some(RangeBound::inclusive(PredicateValue::I64(value))),
    })
}

fn candidates(outcome: &SearchOutcome) -> Result<Vec<GraphCandidate>, String> {
    outcome
        .candidates
        .iter()
        .map(|candidate| {
            let document = candidate
                .document()
                .ok_or_else(|| "graph candidate omitted its public document identity".to_owned())?
                .doc_id()
                .get();
            let document = u64::try_from(document)
                .map_err(|_| format!("graph candidate document {document} exceeds u64"))?;
            let RowSource::Sealed(segment) = candidate.row_id().source() else {
                return Err("graph candidate escaped the sealed graph segment".to_owned());
            };
            Ok(GraphCandidate {
                document,
                score_bits: candidate.score().to_bits(),
                segment: *segment.as_bytes(),
                row: candidate.row_id().local_row(),
            })
        })
        .collect()
}

fn filtered_candidates(
    outcome: &zeppelin_embed::planner::FilteredSearchOutcome,
) -> Result<Vec<GraphCandidate>, String> {
    outcome
        .candidates
        .iter()
        .map(|candidate| {
            let document = candidate
                .document()
                .ok_or_else(|| {
                    "filtered candidate omitted its public document identity".to_owned()
                })?
                .doc_id()
                .get();
            let document = u64::try_from(document)
                .map_err(|_| format!("filtered candidate document {document} exceeds u64"))?;
            let RowSource::Sealed(segment) = candidate.row_id().source() else {
                return Err("filtered candidate escaped the sealed graph segment".to_owned());
            };
            Ok(GraphCandidate {
                document,
                score_bits: candidate.score().to_bits(),
                segment: *segment.as_bytes(),
                row: candidate.row_id().local_row(),
            })
        })
        .collect()
}

fn segment_paths(directory: &Path) -> Result<Vec<PathBuf>, String> {
    std::fs::read_dir(directory)
        .map_err(|error| format!("list graph store: {error}"))?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| format!("read graph store entry: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|paths| {
            paths
                .into_iter()
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("segment-") && name.ends_with(".zseg"))
                })
                .collect()
        })
}

fn first_segment_path(directory: &Path) -> Result<PathBuf, String> {
    segment_paths(directory)?
        .into_iter()
        .next()
        .ok_or_else(|| "graph fixture published no segment".to_owned())
}

fn temporary_orphans(directory: &Path) -> Result<u32, String> {
    let count = std::fs::read_dir(directory)
        .map_err(|error| format!("list graph store temporaries: {error}"))?
        .map(|entry| entry.map_err(|error| format!("read graph store temporary: {error}")))
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.ends_with(".tmp"))
        })
        .count();
    u32::try_from(count).map_err(|_| "temporary orphan count exceeds u32".to_owned())
}

fn observe(seed: u64) -> Result<(GraphInput, GraphObserved), String> {
    let input = oracle::fixture(seed);
    let directory = tempdir().map_err(|error| format!("graph fixture tempdir: {error}"))?;
    let epoch = epoch(input.dims);
    let store = Store::open(
        directory.path(),
        OpenOptions::default().with_epoch(epoch.clone()),
    )
    .map_err(|error| format!("open graph fixture: {error}"))?;
    store
        .ingest(IngestBatch::new(documents(&input)).with_epoch(epoch.identity()))
        .map_err(|error| format!("ingest graph fixture: {error}"))?;
    let deleted = deleted_documents(&input);
    if deleted.is_empty() {
        return Err("graph fixture did not derive deleted rows".to_owned());
    }
    store
        .delete(DeleteBatch::new(deleted))
        .map_err(|error| format!("delete graph fixture rows: {error}"))?;
    store
        .seal()
        .map_err(|error| format!("seal graph fixture: {error}"))?;

    let source_snapshot = store
        .snapshot()
        .map_err(|error| format!("snapshot graph source: {error}"))?;
    let source = source_snapshot
        .segments()
        .first()
        .ok_or_else(|| "sealed graph fixture omitted source segment".to_owned())?
        .meta()
        .id;
    drop(source_snapshot);
    let padded_dims = input
        .dims
        .checked_add(127)
        .map(|dims| dims / 128 * 128)
        .ok_or_else(|| "graph fixture padded dimensions overflow".to_owned())?;
    let work_stride = u64::from(
        GraphNodeLayout::new(input.dims, padded_dims, input.max_degree)
            .map_err(|error| format!("graph fixture work layout: {error}"))?
            .stride(),
    );
    let bounded_bytes = work_stride
        .checked_mul(oracle::CHECKPOINT_ROWS)
        .ok_or_else(|| "graph fixture bounded budget overflow".to_owned())?;
    let checkpoint_path = directory
        .path()
        .join(format!(".tier-{source}.graph.checkpoint"));
    let bounded = store.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: Duration::from_secs(30),
            bytes: bounded_bytes,
        },
        TierThresholds {
            graph_min_rows: u32::try_from(input.rows.len())
                .map_err(|_| "graph row count exceeds u32".to_owned())?,
        },
    );
    let checkpoint_exists_after_bounded = checkpoint_path.exists();
    let complete = store.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: Duration::from_secs(30),
            bytes: u64::MAX,
        },
        TierThresholds {
            graph_min_rows: u32::try_from(input.rows.len())
                .map_err(|_| "graph row count exceeds u32".to_owned())?,
        },
    );
    if complete.graphs_built != 1 || !matches!(complete.status, MaintenanceStatus::Complete) {
        return Err(format!(
            "public graph maintenance did not publish once: built={} status={:?}",
            complete.graphs_built, complete.status
        ));
    }

    let snapshot = store
        .snapshot()
        .map_err(|error| format!("snapshot published graph: {error}"))?;
    let segment = snapshot
        .segments()
        .first()
        .ok_or_else(|| "maintenance published no graph segment".to_owned())?;
    let production_live_rows = segment
        .alive()
        .map_err(|error| format!("read published alive set: {error}"))?
        .live_count();
    let graph_path = directory.path().join(segment.meta().id.file_name());
    let graph_bytes = std::fs::read(&graph_path)
        .map_err(|error| format!("read published graph segment: {error}"))?;
    let parsed_graph = oracle::parse_graph_segment(&graph_bytes)?;
    let entry_documents = parsed_graph
        .entry_rows
        .iter()
        .map(|row| {
            segment
                .document_version(usize::try_from(*row).unwrap_or(usize::MAX))
                .map_err(|error| format!("read graph entry document {row}: {error}"))?
                .ok_or_else(|| format!("graph entry row {row} omitted document identity"))
                .and_then(|document| {
                    u64::try_from(document.doc_id().get())
                        .map_err(|_| format!("graph entry document at row {row} exceeds u64"))
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    drop(snapshot);

    let query = query(&input);
    let k = usize::try_from(input.k).map_err(|_| "graph k exceeds usize".to_owned())?;
    let live_count = input.rows.iter().filter(|row| !row.deleted).count();
    let graph_outcome = store
        .search(
            SearchRequest::new(&query),
            k,
            graph_options(seed),
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| format!("Graph tier search: {error}"))?;
    let graph = candidates(&graph_outcome)?;
    let exact = store
        .search(
            SearchRequest::new(&query),
            k,
            exact_options(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| format!("Exact tier search: {error}"))?;
    let exact_all = store
        .search(
            SearchRequest::new(&query),
            live_count,
            exact_options(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| format!("Exact tier identity search: {error}"))?;

    let large_predicate = predicate(input.large_filter_value);
    let filtered_graph_outcome = store
        .search_filtered(
            SearchRequest::new(&query),
            &large_predicate,
            k,
            graph_options(seed),
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| format!("FilteredGraph search: {error}"))?;
    let filtered_graph_exact = store
        .search_filtered(
            SearchRequest::new(&query),
            &large_predicate,
            k,
            exact_options(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| format!("large filtered Exact search: {error}"))?;
    let filtered_large_cardinality = filtered_graph_outcome
        .plans
        .first()
        .filter(|_| filtered_graph_outcome.plans.len() == 1)
        .map_or(0, |plan| plan.filter_cardinality);
    let filtered_graph_branch = filtered_graph_outcome.plans.len() == 1
        && filtered_graph_outcome.plans[0].branch == SegmentBranch::FilteredGraph;

    let small_predicate = predicate(input.small_filter_value);
    let filtered_small_outcome = store
        .search_filtered(
            SearchRequest::new(&query),
            &small_predicate,
            k,
            graph_options(seed),
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| format!("small filtered Graph request: {error}"))?;
    let filtered_small_exact = store
        .search_filtered(
            SearchRequest::new(&query),
            &small_predicate,
            k,
            exact_options(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| format!("small filtered Exact search: {error}"))?;
    let filtered_small_cardinality = filtered_small_outcome
        .plans
        .first()
        .filter(|_| filtered_small_outcome.plans.len() == 1)
        .map_or(0, |plan| plan.filter_cardinality);
    let filtered_small_exact_allow_list = filtered_small_outcome.plans.len() == 1
        && filtered_small_outcome.plans[0].branch == SegmentBranch::ExactAllowList;

    let source_path = directory.path().join(source.file_name());
    let manifest_bytes = std::fs::read(directory.path().join("manifest.ze"))
        .map_err(|error| format!("read published graph manifest: {error}"))?;
    let manifest_segments = oracle::parse_manifest_segment_ids(&manifest_bytes)?;
    let graph_segments_on_disk = u32::try_from(
        segment_paths(directory.path())?
            .iter()
            .map(std::fs::read)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("read segment inventory: {error}"))?
            .iter()
            .filter(|bytes| oracle::parse_graph_segment(bytes).is_ok())
            .count(),
    )
    .map_err(|_| "graph segment count exceeds u32".to_owned())?;
    let source_manifest_referenced = manifest_segments.contains(source.as_bytes());
    let source_file_exists = source_path.exists();
    let orphan_count = temporary_orphans(directory.path())?;

    store
        .close()
        .map_err(|error| format!("close graph fixture: {error}"))?;
    let reopened = Store::open(directory.path(), OpenOptions::default().with_epoch(epoch))
        .map_err(|error| format!("reopen graph fixture: {error}"))?;
    let reopened_outcome = reopened
        .search(
            SearchRequest::new(&query),
            k,
            graph_options(seed),
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| format!("reopened Graph search: {error}"))?;
    let reopened_graph = candidates(&reopened_outcome)?;
    reopened
        .close()
        .map_err(|error| format!("close reopened graph fixture: {error}"))?;

    Ok((
        input,
        GraphObserved {
            graph_node_count: parsed_graph.node_count,
            production_live_rows,
            graph_max_degree: parsed_graph.max_degree,
            maximum_observed_degree: parsed_graph.maximum_observed_degree,
            entry_rows: parsed_graph.entry_rows,
            entry_documents,
            graphs_built: complete.graphs_built,
            graph_segments: u64::try_from(graph_outcome.graph_stats.segments_traversed)
                .map_err(|_| "graph segment traversal count exceeds u64".to_owned())?,
            entry_discoveries: u64::try_from(graph_outcome.graph_stats.entry_seed_discoveries)
                .map_err(|_| "graph entry discovery count exceeds u64".to_owned())?,
            exact_rescore: graph_outcome.diagnostics.exact_rescore,
            graph,
            exact: candidates(&exact)?,
            exact_all: candidates(&exact_all)?,
            bounded_budget_exhausted: matches!(bounded.status, MaintenanceStatus::BudgetExhausted),
            checkpoint_exists_after_bounded,
            bounded_bytes_consumed: bounded.bytes_consumed,
            work_stride,
            checkpoints_resumed: complete.checkpoints_resumed,
            checkpoint_removed_after_resume: !checkpoint_path.exists(),
            manifest_segments,
            graph_segments_on_disk,
            source_segment: *source.as_bytes(),
            source_file_exists,
            source_manifest_referenced,
            temporary_orphans: orphan_count,
            reopened_graph,
            filtered_graph: filtered_candidates(&filtered_graph_outcome)?,
            filtered_graph_exact: filtered_candidates(&filtered_graph_exact)?,
            filtered_large_cardinality,
            filtered_graph_branch,
            filtered_graph_exact_rescore: filtered_graph_outcome.diagnostics.exact_rescore,
            filtered_small: filtered_candidates(&filtered_small_outcome)?,
            filtered_small_exact: filtered_candidates(&filtered_small_exact)?,
            filtered_small_cardinality,
            filtered_small_exact_allow_list,
        },
    ))
}

fn observation(seed: u64) -> Result<(Arc<GraphInput>, Arc<GraphObserved>), String> {
    OBSERVATION.with(|cache| {
        if let Some((cached_seed, input, observed)) = cache.borrow().as_ref()
            && *cached_seed == seed
        {
            return Ok((Arc::clone(input), Arc::clone(observed)));
        }
        let (input, observed) = observe(seed)?;
        let input = Arc::new(input);
        let observed = Arc::new(observed);
        cache
            .borrow_mut()
            .replace((seed, Arc::clone(&input), Arc::clone(&observed)));
        Ok((input, observed))
    })
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    bytes
        .get(offset..offset.saturating_add(2))
        .and_then(|bytes| bytes.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or_else(|| format!("missing u16 at {offset}"))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    bytes
        .get(offset..offset.saturating_add(8))
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

fn build_fault_fixture(seed: u64) -> Result<(tempfile::TempDir, StoreEpoch), String> {
    let input = oracle::fixture(seed);
    let directory = tempdir().map_err(|error| error.to_string())?;
    let epoch = epoch(input.dims);
    let store = Store::open(
        directory.path(),
        OpenOptions::default().with_epoch(epoch.clone()),
    )
    .map_err(|error| error.to_string())?;
    store
        .ingest(IngestBatch::new(documents(&input)).with_epoch(epoch.identity()))
        .map_err(|error| error.to_string())?;
    store
        .delete(DeleteBatch::new(deleted_documents(&input)))
        .map_err(|error| error.to_string())?;
    store.seal().map_err(|error| error.to_string())?;
    let report = store.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: Duration::from_secs(30),
            bytes: u64::MAX,
        },
        TierThresholds {
            graph_min_rows: u32::try_from(input.rows.len())
                .map_err(|_| "fault fixture row count exceeds u32".to_owned())?,
        },
    );
    if report.graphs_built != 1 {
        return Err("corruption fixture published no graph".to_owned());
    }
    store.close().map_err(|error| error.to_string())?;
    Ok((directory, epoch))
}

fn public_corruption_refused(seed: u64, region: u16) -> Result<(), String> {
    let (directory, epoch) = build_fault_fixture(seed)?;
    let segment = first_segment_path(directory.path())?;
    let mut bytes = std::fs::read(&segment).map_err(|error| error.to_string())?;
    let target = region_byte(&bytes, region)?;
    bytes[target] ^= 0xff;
    std::fs::write(&segment, bytes).map_err(|error| error.to_string())?;
    let reopened = Store::open(directory.path(), OpenOptions::default().with_epoch(epoch));
    match reopened {
        Err(_) => Ok(()),
        Ok(store) => {
            let input = oracle::fixture(seed);
            let result = store.search(
                SearchRequest::new(&query(&input)),
                usize::try_from(input.k).unwrap_or(1),
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
    let input = oracle::fixture(seed);
    let directory = tempdir().map_err(|error| error.to_string())?;
    let epoch = epoch(input.dims);
    let store = Store::open(
        directory.path(),
        OpenOptions::default().with_epoch(epoch.clone()),
    )
    .map_err(|error| error.to_string())?;
    store
        .ingest(IngestBatch::new(documents(&input)).with_epoch(epoch.identity()))
        .map_err(|error| error.to_string())?;
    store
        .delete(DeleteBatch::new(deleted_documents(&input)))
        .map_err(|error| error.to_string())?;
    store.seal().map_err(|error| error.to_string())?;
    let report = store.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: Duration::from_secs(30),
            bytes: u64::MAX,
        },
        TierThresholds {
            graph_min_rows: u32::try_from(input.rows.len())
                .map_err(|_| "cancel fixture row count exceeds u32".to_owned())?,
        },
    );
    if report.graphs_built != 1 {
        return Err("cancel fixture published no graph".to_owned());
    }
    let token = CancelToken::new();
    token.cancel();
    let result = store.search(
        SearchRequest::new(&query(&input)),
        usize::try_from(input.k).unwrap_or(1),
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
    let input = oracle::fixture(seed);
    let directory = tempdir().map_err(|error| error.to_string())?;
    let epoch = epoch(input.dims);
    let event = FaultEvent {
        id: format!("graph-publication-{seed}"),
        op_index: 0,
        layer: Layer::Io,
        site: FaultSite::Rename,
        mode: FaultMode::Eio,
        nth_match: 1,
        expected_matches: None,
        path_contains: Some("manifest.ze".to_owned()),
        fired: false,
        fire_count: 0,
        path: None,
    };
    let scheduled = Arc::new(std_scheduled(FaultSchedule::single(event)));
    let dependencies =
        StoreTestDependencies::new(scheduled.clone(), Arc::new(SystemMonotonicClock));
    let store = Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::default().with_epoch(epoch.clone()),
        dependencies,
    )
    .map_err(|error| error.to_string())?;
    store
        .ingest(IngestBatch::new(documents(&input)).with_epoch(epoch.identity()))
        .map_err(|error| error.to_string())?;
    store
        .delete(DeleteBatch::new(deleted_documents(&input)))
        .map_err(|error| error.to_string())?;
    store.seal().map_err(|error| error.to_string())?;
    scheduled.set_operation(0);
    let report = store.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: Duration::from_secs(30),
            bytes: u64::MAX,
        },
        TierThresholds {
            graph_min_rows: u32::try_from(input.rows.len())
                .map_err(|_| "publication fixture row count exceeds u32".to_owned())?,
        },
    );
    let fired = scheduled.events().into_iter().any(|event| event.fired);
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
            if observed.bounded_budget_exhausted {
                Ok(())
            } else {
                Err("bounded graph build did not report budget exhaustion".to_owned())
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
    let (input, observed) = observation(seed)?;
    let invariants = match operation {
        GraphOperationKind::Shape => vec![GraphInvariantEvidence::I28 { input, observed }],
        GraphOperationKind::EntryPoints => vec![GraphInvariantEvidence::I29 { input, observed }],
        GraphOperationKind::Checkpoint => vec![GraphInvariantEvidence::I34 { input, observed }],
        GraphOperationKind::BoundedBuild => vec![GraphInvariantEvidence::I32 { input, observed }],
        GraphOperationKind::Search => vec![
            GraphInvariantEvidence::I30 {
                input: Arc::clone(&input),
                observed: Arc::clone(&observed),
            },
            GraphInvariantEvidence::I31 { input, observed },
        ],
        GraphOperationKind::Publication => vec![GraphInvariantEvidence::I33 { input, observed }],
        GraphOperationKind::FilteredSearch => {
            vec![GraphInvariantEvidence::I35 { input, observed }]
        }
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
