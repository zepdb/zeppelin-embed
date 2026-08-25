//! Store-level exact filtered-vector execution.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::graph::search::{FilteredGraphSearchOutcome, GraphSearchCounters, GraphSearchRequest};
use crate::ingest::{RowSource, SearchCandidate, SearchRequest};
use crate::lifecycle::{
    GraphSearchOptions, PublishedSnapshot, QueryCancellation, QueryControl, QueryError,
    SearchOptions, SearchTier, SnapshotLease, Store, StoreError, StoreState,
};
use crate::meta::{ColumnStore, ColumnStoreBuilder, EvalError, Predicate, evaluate};
use crate::quant::{prepare_bit4_query, prepare_int8_query};
use crate::scan::{
    Int8Factors, ScanError, ScanQuery, ScanRequest as VectorScanRequest, ScanRows, ScanStats,
    gather_top_k, scan_partition,
};
use crate::segment::layout::RegionKind;

use super::{
    ALLOW_LIST_ROWS_THRESHOLD, PlanError, PlanFallback, PlanNode, SegmentBranch, SegmentPlan,
    SegmentTier, choose_scan_branch, segment_may_match, validate_predicate,
};

/// PLACEHOLDER -- NOT YET MEASURED. This filter-only budget multiplier is
/// calibrated by the control-armed M6 selectivity sweep; it never changes the
/// independent fail-closed `4 * ef * max_degree` traversal invariant.
const FILTERED_VISITED_BUDGET_MULTIPLIER: usize = 2;

/// Exact filtered candidates plus truthful per-segment planning reports.
#[derive(Clone, Debug, PartialEq)]
pub struct FilteredSearchOutcome {
    /// Candidates merged across active and immutable row spaces.
    pub candidates: Vec<SearchCandidate>,
    /// Deterministic exact-scan work.
    pub stats: ScanStats,
    /// Pinned active-state generation.
    pub generation: u64,
    /// One report for every active or manifest-visible segment considered.
    pub plans: Vec<SegmentPlan>,
    /// Unconditional report of the executed query path.
    pub diagnostics: crate::diag::QueryDiagnostics,
}

/// Typed failure from filtered planning or exact execution.
#[derive(Debug)]
pub enum FilteredSearchError {
    /// The closed predicate was rejected before any row scoring began.
    Plan(PlanError),
    /// Store admission, cancellation, or vector scoring failed.
    Query(QueryError),
    /// Timestamp-only active metadata could not be materialized.
    ActiveMetadata(String),
    /// Executor instrumentation disagreed with the published branch report.
    PlanReportMismatch {
        /// Branch exposed to the caller.
        reported: SegmentBranch,
        /// Branch returned independently by the executor.
        executed: SegmentBranch,
    },
    /// A planner-internal node reached an incompatible vector executor.
    InvalidPlanNode(&'static str),
}

impl std::fmt::Display for FilteredSearchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Plan(error) => error.fmt(formatter),
            Self::Query(error) => error.fmt(formatter),
            Self::ActiveMetadata(detail) => {
                write!(formatter, "active metadata construction failed: {detail}")
            }
            Self::PlanReportMismatch { reported, executed } => write!(
                formatter,
                "reported branch {reported:?} differs from executed branch {executed:?}"
            ),
            Self::InvalidPlanNode(detail) => {
                write!(formatter, "invalid vector plan node: {detail}")
            }
        }
    }
}

impl std::error::Error for FilteredSearchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Plan(error) => Some(error),
            Self::Query(error) => Some(error),
            Self::ActiveMetadata(_)
            | Self::PlanReportMismatch { .. }
            | Self::InvalidPlanNode(_) => None,
        }
    }
}

impl From<PlanError> for FilteredSearchError {
    fn from(error: PlanError) -> Self {
        Self::Plan(error)
    }
}

impl From<QueryError> for FilteredSearchError {
    fn from(error: QueryError) -> Self {
        Self::Query(error)
    }
}

impl Store {
    /// Plans and executes one exact filtered vector query.
    ///
    /// Predicate/schema failures are returned before active or sealed rows are
    /// scored. Published graph segments use a reported exact-scan fallback
    /// until task 19-M6 adds filter-aware traversal.
    pub fn search_filtered(
        &self,
        request: SearchRequest<'_>,
        predicate: &Predicate,
        k: usize,
        options: impl Into<SearchOptions>,
        control: QueryControl,
    ) -> Result<FilteredSearchOutcome, FilteredSearchError> {
        execute_store(self, request, predicate, k, options.into(), control)
    }
}

fn execute_store(
    store: &Store,
    request: SearchRequest<'_>,
    predicate: &Predicate,
    k: usize,
    options: SearchOptions,
    control: QueryControl,
) -> Result<FilteredSearchOutcome, FilteredSearchError> {
    let started = std::time::Instant::now();
    let state = store
        .state
        .lock()
        .map_err(|_| QueryError::Store(StoreError::Synchronization { component: "state" }))?;
    match *state {
        StoreState::Open => {}
        StoreState::Closing => return Err(QueryError::Store(StoreError::Closing).into()),
        StoreState::Closed => return Err(QueryError::Store(StoreError::Closed).into()),
    }
    let active_guard = store.active.lock().map_err(|_| {
        QueryError::Store(StoreError::Synchronization {
            component: "active segment",
        })
    })?;
    let active_state = active_guard
        .as_ref()
        .ok_or(QueryError::Store(StoreError::Closed))?;
    let generation = active_state.generation;
    let active = Arc::clone(&active_state.segment);
    let snapshot = store
        .snapshot
        .read()
        .map_err(|_| {
            QueryError::Store(StoreError::Synchronization {
                component: "published snapshot",
            })
        })?
        .as_ref()
        .cloned()
        .ok_or(QueryError::Store(StoreError::Closed))?;

    let schema = store.schema.clone();
    validate_predicate(predicate, &schema)?;
    if matches!(options.tier(), SearchTier::Graph(_)) {
        for segment in snapshot.segments() {
            if !has_graph(segment) {
                return Err(QueryError::Store(StoreError::GraphUnavailable {
                    segment_id: segment.meta().id,
                })
                .into());
            }
        }
    }

    store.active_queries.fetch_add(1, Ordering::Relaxed);
    let query_guard = ActiveFilteredQuery {
        count: &store.active_queries,
    };
    drop(active_guard);
    drop(state);

    let lease = SnapshotLease::new_at(Arc::clone(&snapshot), generation);
    let cancellation = QueryCancellation::new(&control, &lease);
    cancellation.check_graph().map_err(map_scan_error)?;
    let result = execute_pinned(
        &snapshot,
        &active,
        &schema,
        &store.accounting,
        generation,
        store.epoch_identity(),
        request,
        predicate,
        k,
        options,
        &cancellation,
        started,
    );
    drop(query_guard);
    result
}

#[allow(clippy::too_many_arguments)]
fn execute_pinned(
    snapshot: &PublishedSnapshot,
    active: &crate::ingest::ActiveSegment,
    schema: &crate::meta::Schema,
    accounting: &Arc<crate::lifecycle::stats::Accounting>,
    generation: u64,
    epoch: Option<crate::epoch::EpochIdentity>,
    request: SearchRequest<'_>,
    predicate: &Predicate,
    k: usize,
    options: SearchOptions,
    cancellation: &QueryCancellation<'_>,
    started: std::time::Instant,
) -> Result<FilteredSearchOutcome, FilteredSearchError> {
    let mut candidates = Vec::new();
    let mut plans = Vec::new();
    let mut stats = MutableStats::default();
    let mut graph_stats = crate::ingest::GraphSearchStats::default();
    let bit4_query = prepare_bit4_query(request.vector(), 0)
        .map_err(ScanError::Quant)
        .map_err(QueryError::Scan)?;
    let int8_query = prepare_int8_query(request.vector())
        .map_err(ScanError::Quant)
        .map_err(QueryError::Scan)?;
    let auto_uses_graph =
        matches!(options.tier(), SearchTier::Auto) && snapshot.segments().iter().any(has_graph);
    let full_precision = auto_uses_graph || matches!(options.tier(), SearchTier::Graph(_));

    if !active.is_empty() {
        let alive = active.alive().map_err(QueryError::Store)?;
        let columns = active_columns(active, schema)?;
        let allow_list = evaluate(predicate, &columns, &alive).map_err(map_eval_error)?;
        let branch = choose_scan_branch(allow_list.cardinality());
        let source = RowSource::Active;
        let plan = SegmentPlan::exact(
            source,
            SegmentTier::ActiveScan,
            branch,
            allow_list.cardinality(),
        )
        .with_predicate(predicate);
        let (local, local_stats, executed) = if full_precision {
            execute_squared_l2_plan(
                &plan.node,
                active.vectors(),
                active.row_count(),
                request.vector(),
                &allow_list,
                k,
                cancellation,
            )?
        } else {
            execute_scan_plan(
                &plan.node,
                VectorScanRequest {
                    query: ScanQuery::Bit4(&bit4_query),
                    rows: ScanRows::Bit4RowMajor {
                        codes: active.codes(),
                        factors: active.factors(),
                    },
                    row_mask: Some(allow_list.as_roaring()),
                },
                active.row_count(),
                k,
                cancellation,
            )?
        };
        verify_execution_branch(plan.branch, executed)?;
        append_candidates(
            local,
            source,
            |row| Ok(active.document(row)),
            &mut candidates,
            full_precision,
        )?;
        stats.add(local_stats)?;
        plans.push(plan);
        retain_global_top_k(&mut candidates, k);
    }

    let mut ordered_segments = snapshot.segments().iter().collect::<Vec<_>>();
    if auto_uses_graph || matches!(options.tier(), SearchTier::Graph(_)) {
        ordered_segments.sort_unstable_by(|left, right| {
            right
                .meta()
                .row_count
                .cmp(&left.meta().row_count)
                .then_with(|| left.meta().id.cmp(&right.meta().id))
        });
    }
    for segment in ordered_segments {
        let source = RowSource::Sealed(segment.meta().id);
        let graph_selected =
            has_graph(segment) && matches!(options.tier(), SearchTier::Auto | SearchTier::Graph(_));
        let tier = if graph_selected {
            SegmentTier::SealedGraph
        } else {
            SegmentTier::SealedScan
        };
        if !segment_may_match(segment.meta().clustering_key_range, predicate) {
            plans.push(
                SegmentPlan::exact(source, tier, SegmentBranch::Pruned, 0)
                    .with_predicate(predicate),
            );
            continue;
        }
        let columns = segment
            .columns()
            .map_err(StoreError::Segment)
            .map_err(QueryError::Store)?;
        let alive = segment
            .alive()
            .map_err(StoreError::Segment)
            .map_err(QueryError::Store)?;
        let allow_list = evaluate(predicate, &columns, &alive).map_err(map_eval_error)?;
        let row_count = segment.meta().row_count as usize;
        let graph_options = match options.tier() {
            SearchTier::Graph(graph_options) if graph_selected => Some(graph_options),
            SearchTier::Auto if graph_selected => Some(GraphSearchOptions::default()),
            SearchTier::Auto | SearchTier::Scan | SearchTier::Graph(_) => None,
        };
        if let Some(graph_options) = graph_options {
            validate_filtered_explicit_ef(graph_options, k.min(row_count), row_count)?;
            if allow_list.cardinality() > ALLOW_LIST_ROWS_THRESHOLD {
                let prepared = segment
                    .graph_search_cache
                    .prepare_shared(segment, cancellation)
                    .map_err(map_graph_cache_error)?;
                let norm_range = prepared.norm_range;
                if let Some(competitive_distance) = global_competitive_distance(&candidates, k)
                    && norm_range.squared_l2_upper_bound(request.vector()) <= f64::from(f32::MAX)
                    && (norm_range.squared_l2_lower_bound(request.vector()) as f32)
                        > competitive_distance
                {
                    cancellation.check_graph().map_err(map_scan_error)?;
                    plans.push(
                        SegmentPlan::exact(
                            source,
                            SegmentTier::SealedGraph,
                            SegmentBranch::Pruned,
                            allow_list.cardinality(),
                        )
                        .with_predicate(predicate),
                    );
                    continue;
                }
                let execution = execute_filtered_graph(
                    segment,
                    request.vector(),
                    &allow_list,
                    k,
                    graph_options,
                    cancellation,
                    accounting,
                    prepared.graph_validated,
                    prepared.entry_seed_discovered,
                )?;
                let plan = SegmentPlan::filtered_graph(
                    source,
                    allow_list.cardinality(),
                    execution.ef_requested,
                    execution.ef_effective,
                    execution.branch,
                    execution.fallback,
                )
                .with_predicate(predicate);
                verify_execution_branch(plan.branch, execution.branch)?;
                append_candidates(
                    execution.candidates,
                    source,
                    |row| {
                        segment
                            .document_version(row)
                            .map_err(StoreError::Segment)
                            .map_err(QueryError::Store)
                    },
                    &mut candidates,
                    true,
                )?;
                stats.add(execution.stats)?;
                add_graph_stats(&mut graph_stats, execution.graph_stats)?;
                plans.push(plan);
                retain_global_top_k(&mut candidates, k);
                continue;
            }
        }
        let branch = choose_scan_branch(allow_list.cardinality());
        let plan = SegmentPlan::exact(source, tier, branch, allow_list.cardinality())
            .with_predicate(predicate);
        let (local, local_stats, executed) = if auto_uses_graph || graph_selected {
            let vectors = segment
                .rescore_f32()
                .map_err(StoreError::Segment)
                .map_err(QueryError::Store)?;
            execute_squared_l2_plan(
                &plan.node,
                vectors,
                row_count,
                request.vector(),
                &allow_list,
                k,
                cancellation,
            )?
        } else {
            match segment.meta().scheme {
                0 => execute_scan_plan(
                    &plan.node,
                    VectorScanRequest {
                        query: ScanQuery::F32(request.vector()),
                        rows: ScanRows::F32BorrowedRowMajor(
                            segment
                                .f32_codes()
                                .map_err(StoreError::Segment)
                                .map_err(QueryError::Store)?,
                        ),
                        row_mask: Some(allow_list.as_roaring()),
                    },
                    row_count,
                    k,
                    cancellation,
                )?,
                2 => {
                    let factors = segment
                        .int8_factors()
                        .map_err(StoreError::Segment)
                        .map_err(QueryError::Store)?
                        .iter()
                        .map(|factor| Int8Factors::new(factor.scale, factor.offset))
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|_| {
                            QueryError::Store(StoreError::Segment(
                                crate::segment::SegmentError::Geometry(
                                    "Int8 factor is not finite and non-negative".to_owned(),
                                ),
                            ))
                        })?;
                    execute_scan_plan(
                        &plan.node,
                        VectorScanRequest {
                            query: ScanQuery::Int8(&int8_query),
                            rows: ScanRows::Int8RowMajor {
                                codes: segment
                                    .int8_codes()
                                    .map_err(StoreError::Segment)
                                    .map_err(QueryError::Store)?,
                                factors: &factors,
                            },
                            row_mask: Some(allow_list.as_roaring()),
                        },
                        row_count,
                        k,
                        cancellation,
                    )?
                }
                4 => execute_scan_plan(
                    &plan.node,
                    VectorScanRequest {
                        query: ScanQuery::Bit4(&bit4_query),
                        rows: ScanRows::Bit4RowMajor {
                            codes: segment
                                .bit4_codes()
                                .map_err(StoreError::Segment)
                                .map_err(QueryError::Store)?,
                            factors: segment
                                .bit4_factors()
                                .map_err(StoreError::Segment)
                                .map_err(QueryError::Store)?,
                        },
                        row_mask: Some(allow_list.as_roaring()),
                    },
                    row_count,
                    k,
                    cancellation,
                )?,
                scheme => {
                    return Err(QueryError::Store(StoreError::Segment(
                        crate::segment::SegmentError::Geometry(format!(
                            "filtered store search does not support sealed scheme {scheme}"
                        )),
                    ))
                    .into());
                }
            }
        };
        verify_execution_branch(plan.branch, executed)?;
        append_candidates(
            local,
            source,
            |row| {
                segment
                    .document_version(row)
                    .map_err(StoreError::Segment)
                    .map_err(QueryError::Store)
            },
            &mut candidates,
            auto_uses_graph || graph_selected || segment.meta().scheme == 0,
        )?;
        stats.add(local_stats)?;
        plans.push(plan);
        retain_global_top_k(&mut candidates, k);
    }

    candidates.sort_unstable_by(|left, right| {
        right
            .score()
            .total_cmp(&left.score())
            .then_with(|| left.row_id().cmp(&right.row_id()))
    });
    candidates.truncate(k);
    cancellation.check_graph().map_err(map_scan_error)?;
    let stats = stats.finish();
    let diagnostics = crate::diag::QueryDiagnostics::vector(crate::diag::VectorDiagnostics {
        snapshot_generation: generation,
        indexed_through_seq: active
            .indexed_through_seq()
            .max(crate::wal::LogSeq::new(snapshot.absorbed_through())),
        approximate: plans.iter().any(|plan| plan.approximate),
        exact_rescore: candidates.iter().all(|candidate| candidate.exact_score()),
        requested_k: k,
        returned: candidates.len(),
        budget_exhausted: plans.iter().any(|plan| {
            matches!(
                plan.fallback,
                PlanFallback::VisitedBudget | PlanFallback::CandidateShortfall
            )
        }),
        plan: plans.clone(),
        scan: stats.clone(),
        graph: graph_stats,
        epoch,
        elapsed: started.elapsed(),
    });
    Ok(FilteredSearchOutcome {
        candidates,
        stats,
        generation,
        plans,
        diagnostics,
    })
}

struct FilteredGraphExecution {
    candidates: Vec<LocalCandidate>,
    stats: ScanStats,
    branch: SegmentBranch,
    fallback: PlanFallback,
    ef_requested: Option<usize>,
    ef_effective: usize,
    graph_stats: crate::ingest::GraphSearchStats,
}

fn validate_filtered_explicit_ef(
    options: GraphSearchOptions,
    segment_k: usize,
    rows: usize,
) -> Result<(), FilteredSearchError> {
    let Some(ef) = options.ef() else {
        return Ok(());
    };
    if ef < segment_k {
        return Err(
            QueryError::Graph(crate::graph::search::GraphSearchError::AdaptiveEf(
                crate::graph::search::AdaptiveEfError::ExplicitBelowK { k: segment_k, ef },
            ))
            .into(),
        );
    }
    if ef > rows {
        return Err(
            QueryError::Graph(crate::graph::search::GraphSearchError::AdaptiveEf(
                crate::graph::search::AdaptiveEfError::ExplicitExceedsRows { ef, rows },
            ))
            .into(),
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn execute_filtered_graph(
    segment: &crate::segment::reader::SegmentReader,
    query: &[f32],
    allow_list: &crate::meta::DocBitmap,
    k: usize,
    options: GraphSearchOptions,
    cancellation: &QueryCancellation<'_>,
    accounting: &Arc<crate::lifecycle::stats::Accounting>,
    graph_validated_before: bool,
    entry_seed_discovered_before: bool,
) -> Result<FilteredGraphExecution, FilteredSearchError> {
    cancellation.check_graph().map_err(map_scan_error)?;
    let prepared = segment
        .graph_search_cache
        .prepare_shared(segment, cancellation)
        .map_err(map_graph_cache_error)?;
    let graph = prepared.graph;
    let rescore = segment
        .rescore_f32()
        .map_err(StoreError::Segment)
        .map_err(QueryError::Store)?;
    let node_count = graph.node_count() as usize;
    let allow_count = usize::try_from(allow_list.cardinality())
        .map_err(|_| QueryError::Scan(ScanError::ArithmeticOverflow))?;
    let target_k = k.min(node_count).min(allow_count);
    if target_k == 0 {
        return Err(
            QueryError::Graph(crate::graph::search::GraphSearchError::Geometry(
                "filtered graph traversal was selected with an empty effective mask".to_owned(),
            ))
            .into(),
        );
    }
    let base_request =
        GraphSearchRequest::new(query, target_k, options.seed()).with_profile(options.profile());
    let base_request = options
        .ef()
        .map_or(base_request, |ef| base_request.with_ef(ef));
    let base_ef = base_request
        .effective_ef(node_count)
        .map_err(crate::graph::search::GraphSearchError::AdaptiveEf)
        .map_err(QueryError::Graph)?;
    let selectivity_scaled = base_ef
        .checked_mul(node_count)
        .map(|value| value.div_ceil(allow_count))
        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
    let ef_effective = selectivity_scaled.min(allow_count).max(target_k);
    let max_degree = usize::from(graph.layout().max_degree());
    let filtered_visited_budget = ef_effective
        .checked_mul(max_degree)
        .and_then(|value| value.checked_mul(FILTERED_VISITED_BUDGET_MULTIPLIER))
        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?
        .min(node_count);
    let scratch_capacity = ef_effective.max(filtered_visited_budget);
    let maximum_corrective_ef = allow_count.min(scratch_capacity);
    let mut scratch = segment
        .graph_search_cache
        .checkout(graph, scratch_capacity, accounting)
        .map_err(map_graph_cache_error)?;
    let entry_seed_discovered = entry_seed_discovered_before
        || prepared.entry_seed_discovered
        || scratch.entry_seed_discovered();
    let entries = scratch.entries();
    let mut searcher = crate::graph::search::GraphSearcher::with_entry_row_ids(
        graph,
        rescore,
        entries,
        scratch.scratch_mut().map_err(map_graph_error)?,
    )
    .map_err(map_graph_error)?;
    let ef_requested = options.ef();
    let mut current_ef = ef_effective;
    let mut traversal_stats = MutableStats::default();
    let mut graph_stats = crate::ingest::GraphSearchStats {
        graph_validations: usize::from(graph_validated_before || prepared.graph_validated),
        entry_seed_discoveries: usize::from(entry_seed_discovered),
        ..crate::ingest::GraphSearchStats::default()
    };
    loop {
        let request = GraphSearchRequest::new(query, target_k, options.seed())
            .with_profile(options.profile())
            .with_ef(current_ef);
        let outcome = searcher
            .search_filtered(
                request,
                allow_list,
                filtered_visited_budget,
                Some(cancellation),
            )
            .map_err(map_graph_error)?;
        match outcome {
            FilteredGraphSearchOutcome::Traversed(result) => {
                add_graph_counters(&mut graph_stats, result.counters())?;
                traversal_stats.add(graph_scan_stats(result.counters())?)?;
                if result.candidates().len() >= target_k {
                    let candidates = result
                        .candidates()
                        .iter()
                        .map(|candidate| LocalCandidate {
                            row: candidate.row_id() as usize,
                            score: -(candidate.distance() as f32),
                        })
                        .collect();
                    return Ok(FilteredGraphExecution {
                        candidates,
                        stats: traversal_stats.finish(),
                        branch: SegmentBranch::FilteredGraph,
                        fallback: if ef_requested.is_some_and(|requested| current_ef > requested) {
                            PlanFallback::EfWidened
                        } else {
                            PlanFallback::None
                        },
                        ef_requested,
                        ef_effective: current_ef,
                        graph_stats: with_traversed_segment(graph_stats)?,
                    });
                }
                if current_ef < maximum_corrective_ef {
                    current_ef = current_ef
                        .saturating_mul(2)
                        .max(current_ef.saturating_add(1))
                        .min(maximum_corrective_ef);
                    continue;
                }
                return exact_filtered_graph_fallback(
                    rescore,
                    node_count,
                    query,
                    allow_list,
                    k,
                    cancellation,
                    traversal_stats,
                    PlanFallback::CandidateShortfall,
                    ef_requested,
                    current_ef,
                    graph_stats,
                );
            }
            FilteredGraphSearchOutcome::VisitedBudgetExceeded { counters, .. } => {
                add_graph_counters(&mut graph_stats, counters)?;
                traversal_stats.add(graph_scan_stats(counters)?)?;
                return exact_filtered_graph_fallback(
                    rescore,
                    node_count,
                    query,
                    allow_list,
                    k,
                    cancellation,
                    traversal_stats,
                    PlanFallback::VisitedBudget,
                    ef_requested,
                    current_ef,
                    graph_stats,
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn exact_filtered_graph_fallback(
    rescore: &[f32],
    node_count: usize,
    query: &[f32],
    allow_list: &crate::meta::DocBitmap,
    k: usize,
    cancellation: &QueryCancellation<'_>,
    mut traversal_stats: MutableStats,
    fallback: PlanFallback,
    ef_requested: Option<usize>,
    ef_effective: usize,
    graph_stats: crate::ingest::GraphSearchStats,
) -> Result<FilteredGraphExecution, FilteredSearchError> {
    let (candidates, exact_stats, _) = execute_squared_l2_rows(
        rescore,
        node_count,
        query,
        allow_list,
        SegmentBranch::ExactAllowList,
        k,
        cancellation,
    )?;
    traversal_stats.add(exact_stats)?;
    Ok(FilteredGraphExecution {
        candidates,
        stats: traversal_stats.finish(),
        branch: SegmentBranch::GraphExactFallback,
        fallback,
        ef_requested,
        ef_effective,
        graph_stats: with_traversed_segment(graph_stats)?,
    })
}

fn add_graph_counters(
    stats: &mut crate::ingest::GraphSearchStats,
    counters: GraphSearchCounters,
) -> Result<(), FilteredSearchError> {
    stats.visited_epoch_clears = stats
        .visited_epoch_clears
        .checked_add(usize::from(counters.visited_epoch_cleared()))
        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
    stats.candidates_scored = stats
        .candidates_scored
        .checked_add(counters.candidates_scored())
        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
    stats.candidates_rescored = stats
        .candidates_rescored
        .checked_add(counters.candidates_rescored())
        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
    Ok(())
}

fn with_traversed_segment(
    mut stats: crate::ingest::GraphSearchStats,
) -> Result<crate::ingest::GraphSearchStats, FilteredSearchError> {
    stats.segments_traversed = stats
        .segments_traversed
        .checked_add(1)
        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
    Ok(stats)
}

fn add_graph_stats(
    total: &mut crate::ingest::GraphSearchStats,
    delta: crate::ingest::GraphSearchStats,
) -> Result<(), FilteredSearchError> {
    total.segments_traversed = total
        .segments_traversed
        .checked_add(delta.segments_traversed)
        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
    total.graph_validations = total
        .graph_validations
        .checked_add(delta.graph_validations)
        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
    total.entry_seed_discoveries = total
        .entry_seed_discoveries
        .checked_add(delta.entry_seed_discoveries)
        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
    total.visited_epoch_clears = total
        .visited_epoch_clears
        .checked_add(delta.visited_epoch_clears)
        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
    total.candidates_scored = total
        .candidates_scored
        .checked_add(delta.candidates_scored)
        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
    total.candidates_rescored = total
        .candidates_rescored
        .checked_add(delta.candidates_rescored)
        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
    total.segments_pruned_by_bound = total
        .segments_pruned_by_bound
        .checked_add(delta.segments_pruned_by_bound)
        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
    Ok(())
}

fn graph_scan_stats(counters: GraphSearchCounters) -> Result<ScanStats, FilteredSearchError> {
    let did_work = counters.candidates_scored() != 0 || counters.candidates_rescored() != 0;
    Ok(ScanStats {
        dims_touched: counters.dims_touched(),
        bytes_read: counters.bytes_read(),
        threads_used: usize::from(did_work),
        worker_thread_ids: did_work
            .then(|| std::thread::current().id())
            .into_iter()
            .collect(),
    })
}

fn map_graph_cache_error(
    error: crate::lifecycle::graph_cache::GraphCacheError,
) -> FilteredSearchError {
    match error {
        crate::lifecycle::graph_cache::GraphCacheError::Store(error) => {
            QueryError::Store(error).into()
        }
        crate::lifecycle::graph_cache::GraphCacheError::Search(error) => map_graph_error(error),
    }
}

fn map_graph_error(error: crate::graph::search::GraphSearchError) -> FilteredSearchError {
    let query = match error {
        crate::graph::search::GraphSearchError::Cancelled { partial } => {
            QueryError::Cancelled { partial }
        }
        crate::graph::search::GraphSearchError::Timeout { partial } => {
            QueryError::Timeout { partial }
        }
        crate::graph::search::GraphSearchError::ReadCancelled { partial } => {
            QueryError::ReadCancelled { partial }
        }
        error => QueryError::Graph(error),
    };
    query.into()
}

fn active_columns(
    active: &crate::ingest::ActiveSegment,
    schema: &crate::meta::Schema,
) -> Result<ColumnStore, FilteredSearchError> {
    let mut builder = ColumnStoreBuilder::new(schema.clone());
    for (row, timestamp) in active.timestamps().iter().enumerate() {
        let values = active.column_values(row).map_err(QueryError::Store)?;
        builder
            .push_row(*timestamp, &crate::ingest::column_inputs(&values))
            .map_err(|error| FilteredSearchError::ActiveMetadata(error.to_string()))?;
    }
    builder
        .finish()
        .map_err(|error| FilteredSearchError::ActiveMetadata(error.to_string()))
}

#[derive(Clone, Copy, Debug)]
struct LocalCandidate {
    row: usize,
    score: f32,
}

fn execute_scan_request(
    request: VectorScanRequest<'_>,
    row_count: usize,
    branch: SegmentBranch,
    k: usize,
    cancellation: &QueryCancellation<'_>,
) -> Result<(Vec<LocalCandidate>, ScanStats, SegmentBranch), FilteredSearchError> {
    let outcome = match branch {
        SegmentBranch::FilteredGraph | SegmentBranch::Graph => {
            return Err(FilteredSearchError::InvalidPlanNode(
                "filtered graph branch reached exact scan request",
            ));
        }
        SegmentBranch::Pruned => crate::scan::ScanOutcome {
            candidates: Vec::new(),
            stats: ScanStats {
                dims_touched: 0,
                bytes_read: 0,
                threads_used: 0,
                worker_thread_ids: Vec::new(),
            },
        },
        SegmentBranch::ExactAllowList => {
            gather_top_k(request, k, Some(cancellation)).map_err(map_scan_error)?
        }
        SegmentBranch::MaskedScan | SegmentBranch::GraphExactFallback => {
            let partition = scan_partition(request, k, 0..row_count, Some(cancellation))
                .map_err(map_scan_error)?;
            crate::scan::ScanOutcome {
                candidates: partition.candidates,
                stats: ScanStats {
                    dims_touched: partition.dims_touched,
                    bytes_read: partition.bytes_read,
                    threads_used: 1,
                    worker_thread_ids: vec![partition.worker_thread_id],
                },
            }
        }
    };
    Ok((
        outcome
            .candidates
            .into_iter()
            .map(|candidate| LocalCandidate {
                row: candidate.row_id,
                score: candidate.score,
            })
            .collect(),
        outcome.stats,
        branch,
    ))
}

fn execute_scan_plan(
    node: &PlanNode,
    request: VectorScanRequest<'_>,
    row_count: usize,
    k: usize,
    cancellation: &QueryCancellation<'_>,
) -> Result<(Vec<LocalCandidate>, ScanStats, SegmentBranch), FilteredSearchError> {
    match node {
        PlanNode::BitmapIntersect { input, .. } => {
            execute_scan_plan(input, request, row_count, k, cancellation)
        }
        PlanNode::Scan { branch, .. } => {
            execute_scan_request(request, row_count, *branch, k, cancellation)
        }
        PlanNode::Graph {
            fallback: Some(fallback),
            ..
        } => {
            let (candidates, stats, _) =
                execute_scan_plan(fallback, request, row_count, k, cancellation)?;
            Ok((candidates, stats, SegmentBranch::GraphExactFallback))
        }
        PlanNode::Graph { fallback: None, .. } => Err(FilteredSearchError::InvalidPlanNode(
            "graph node has no exact fallback before task 19-M6",
        )),
        PlanNode::Lexical { .. } => Err(FilteredSearchError::InvalidPlanNode(
            "lexical node reached vector executor",
        )),
        PlanNode::Fusion { .. } => Err(FilteredSearchError::InvalidPlanNode(
            "fusion node reached vector executor before task 17",
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_squared_l2_plan(
    node: &PlanNode,
    vectors: &[f32],
    row_count: usize,
    query: &[f32],
    allow_list: &crate::meta::DocBitmap,
    k: usize,
    cancellation: &QueryCancellation<'_>,
) -> Result<(Vec<LocalCandidate>, ScanStats, SegmentBranch), FilteredSearchError> {
    match node {
        PlanNode::BitmapIntersect { input, .. } => execute_squared_l2_plan(
            input,
            vectors,
            row_count,
            query,
            allow_list,
            k,
            cancellation,
        ),
        PlanNode::Scan { branch, .. } => execute_squared_l2_rows(
            vectors,
            row_count,
            query,
            allow_list,
            *branch,
            k,
            cancellation,
        ),
        PlanNode::Graph {
            fallback: Some(fallback),
            ..
        } => {
            let (candidates, stats, _) = execute_squared_l2_plan(
                fallback,
                vectors,
                row_count,
                query,
                allow_list,
                k,
                cancellation,
            )?;
            Ok((candidates, stats, SegmentBranch::GraphExactFallback))
        }
        PlanNode::Graph { fallback: None, .. } => Err(FilteredSearchError::InvalidPlanNode(
            "graph node has no exact fallback before task 19-M6",
        )),
        PlanNode::Lexical { .. } => Err(FilteredSearchError::InvalidPlanNode(
            "lexical node reached vector executor",
        )),
        PlanNode::Fusion { .. } => Err(FilteredSearchError::InvalidPlanNode(
            "fusion node reached vector executor before task 17",
        )),
    }
}

fn execute_squared_l2_rows(
    vectors: &[f32],
    row_count: usize,
    query: &[f32],
    allow_list: &crate::meta::DocBitmap,
    branch: SegmentBranch,
    k: usize,
    cancellation: &QueryCancellation<'_>,
) -> Result<(Vec<LocalCandidate>, ScanStats, SegmentBranch), FilteredSearchError> {
    if query.is_empty() {
        return Err(QueryError::Scan(ScanError::ZeroDimension).into());
    }
    let expected = row_count
        .checked_mul(query.len())
        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
    if vectors.len() != expected {
        return Err(QueryError::Scan(ScanError::RowDataLength {
            dimension: query.len(),
            actual: vectors.len(),
        })
        .into());
    }
    let mut candidates = Vec::new();
    let mut scored_rows = 0_u64;
    match branch {
        SegmentBranch::FilteredGraph | SegmentBranch::Graph => {
            return Err(FilteredSearchError::InvalidPlanNode(
                "filtered graph branch reached exact squared-L2 rows",
            ));
        }
        SegmentBranch::Pruned => {}
        SegmentBranch::ExactAllowList => {
            for row in allow_list.iter() {
                let row = usize::try_from(row)
                    .map_err(|_| QueryError::Scan(ScanError::ArithmeticOverflow))?;
                score_one(
                    vectors,
                    row_count,
                    query,
                    row,
                    &mut candidates,
                    &mut scored_rows,
                    cancellation,
                )?;
            }
        }
        SegmentBranch::MaskedScan | SegmentBranch::GraphExactFallback => {
            for row in 0..row_count {
                if row.is_multiple_of(64) {
                    cancellation.check_graph().map_err(map_scan_error)?;
                }
                let local_row = u32::try_from(row)
                    .map_err(|_| QueryError::Scan(ScanError::ArithmeticOverflow))?;
                if allow_list.contains(local_row) {
                    score_one(
                        vectors,
                        row_count,
                        query,
                        row,
                        &mut candidates,
                        &mut scored_rows,
                        cancellation,
                    )?;
                }
            }
        }
    }
    candidates.sort_unstable_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.row.cmp(&right.row))
    });
    candidates.truncate(k);
    let dimensions =
        u64::try_from(query.len()).map_err(|_| QueryError::Scan(ScanError::ArithmeticOverflow))?;
    let dims_touched = scored_rows
        .checked_mul(dimensions)
        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
    let bytes_read = dims_touched
        .checked_mul(std::mem::size_of::<f32>() as u64)
        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
    Ok((
        candidates,
        ScanStats {
            dims_touched,
            bytes_read,
            threads_used: usize::from(scored_rows != 0),
            worker_thread_ids: (scored_rows != 0)
                .then(|| std::thread::current().id())
                .into_iter()
                .collect(),
        },
        branch,
    ))
}

#[allow(clippy::too_many_arguments)]
fn score_one(
    vectors: &[f32],
    row_count: usize,
    query: &[f32],
    row: usize,
    candidates: &mut Vec<LocalCandidate>,
    scored_rows: &mut u64,
    cancellation: &QueryCancellation<'_>,
) -> Result<(), FilteredSearchError> {
    if row >= row_count {
        return Err(QueryError::Scan(ScanError::ArithmeticOverflow).into());
    }
    if row.is_multiple_of(64) {
        cancellation.check_graph().map_err(map_scan_error)?;
    }
    let start = row
        .checked_mul(query.len())
        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
    let end = start
        .checked_add(query.len())
        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
    let vector = vectors
        .get(start..end)
        .ok_or(QueryError::Scan(ScanError::RowDataLength {
            dimension: query.len(),
            actual: vectors.len(),
        }))?;
    let distance = vector
        .iter()
        .zip(query)
        .map(|(left, right)| {
            let delta = f64::from(*left) - f64::from(*right);
            delta * delta
        })
        .sum::<f64>();
    let score = -(distance as f32);
    if !score.is_finite() {
        return Err(QueryError::Scan(ScanError::NonFiniteScore { row_id: row }).into());
    }
    candidates.push(LocalCandidate { row, score });
    *scored_rows = scored_rows
        .checked_add(1)
        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
    Ok(())
}

fn append_candidates(
    local: Vec<LocalCandidate>,
    source: RowSource,
    document: impl Fn(usize) -> Result<Option<crate::ingest::DocumentVersion>, QueryError>,
    candidates: &mut Vec<SearchCandidate>,
    exact_score: bool,
) -> Result<(), FilteredSearchError> {
    for candidate in local {
        let local_row = u32::try_from(candidate.row)
            .map_err(|_| QueryError::Scan(ScanError::ArithmeticOverflow))?;
        candidates.push(SearchCandidate::new(
            crate::ingest::GlobalRowId::new(source, local_row),
            document(candidate.row)?,
            candidate.score,
            exact_score,
        ));
    }
    Ok(())
}

fn retain_global_top_k(candidates: &mut Vec<SearchCandidate>, k: usize) {
    candidates.sort_unstable_by(|left, right| {
        right
            .score()
            .total_cmp(&left.score())
            .then_with(|| left.row_id().cmp(&right.row_id()))
    });
    candidates.truncate(k);
}

fn global_competitive_distance(candidates: &[SearchCandidate], k: usize) -> Option<f32> {
    if k == 0 || candidates.len() < k {
        return None;
    }
    candidates
        .get(k - 1)
        .map(|candidate| (-candidate.score()).max(0.0))
        .filter(|distance| distance.is_finite())
}

fn has_graph(segment: &crate::segment::reader::SegmentReader) -> bool {
    segment
        .directory()
        .iter()
        .any(|entry| entry.kind == RegionKind::GraphNodeBlocks.id())
}

fn map_eval_error(error: EvalError) -> FilteredSearchError {
    let plan = match error {
        EvalError::UnknownColumn(column) => PlanError::UnknownColumn(column),
        EvalError::TypeMismatch {
            column,
            expected,
            actual,
        } => PlanError::TypeMismatch {
            column,
            expected,
            actual,
        },
        EvalError::RangeRequiresNumericColumn(column) => {
            PlanError::RangeRequiresNumericColumn(column)
        }
        EvalError::RowCountMismatch { columns, alive } => {
            PlanError::RowCountMismatch { columns, alive }
        }
    };
    FilteredSearchError::Plan(plan)
}

fn map_scan_error(error: ScanError) -> FilteredSearchError {
    let query = match error {
        ScanError::Cancelled { partial } => QueryError::Cancelled { partial },
        ScanError::Timeout { partial } => QueryError::Timeout { partial },
        ScanError::ReadCancelled { partial } => QueryError::ReadCancelled { partial },
        error => QueryError::Scan(error),
    };
    FilteredSearchError::Query(query)
}

fn verify_execution_branch(
    reported: SegmentBranch,
    executed: SegmentBranch,
) -> Result<(), FilteredSearchError> {
    if reported == executed {
        Ok(())
    } else {
        Err(FilteredSearchError::PlanReportMismatch { reported, executed })
    }
}

#[derive(Default)]
struct MutableStats {
    dims_touched: u64,
    bytes_read: u64,
    worker_thread_ids: Vec<std::thread::ThreadId>,
}

impl MutableStats {
    fn add(&mut self, stats: ScanStats) -> Result<(), FilteredSearchError> {
        self.dims_touched = self
            .dims_touched
            .checked_add(stats.dims_touched)
            .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
        self.bytes_read = self
            .bytes_read
            .checked_add(stats.bytes_read)
            .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
        for thread in stats.worker_thread_ids {
            if !self.worker_thread_ids.contains(&thread) {
                self.worker_thread_ids.push(thread);
            }
        }
        Ok(())
    }

    fn finish(self) -> ScanStats {
        ScanStats {
            dims_touched: self.dims_touched,
            bytes_read: self.bytes_read,
            threads_used: self.worker_thread_ids.len(),
            worker_thread_ids: self.worker_thread_ids,
        }
    }
}

struct ActiveFilteredQuery<'a> {
    count: &'a AtomicU64,
}

impl Drop for ActiveFilteredQuery<'_> {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_reported_plan_matches_the_branch_actually_executed() {
        let error =
            verify_execution_branch(SegmentBranch::MaskedScan, SegmentBranch::ExactAllowList)
                .expect_err("a planted plan lie must fail");
        assert!(matches!(
            error,
            FilteredSearchError::PlanReportMismatch { .. }
        ));
    }
}
