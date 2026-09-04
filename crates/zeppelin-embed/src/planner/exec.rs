//! Store-level exact filtered-vector execution.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::graph::search::{FilteredGraphSearchOutcome, GraphSearchCounters, GraphSearchRequest};
use crate::ingest::{RowSource, SearchCandidate, SearchRequest};
use crate::lifecycle::{
    GraphSearchOptions, PublishedSnapshot, QueryCancellation, QueryControl, QueryError,
    SearchOptions, SearchTier, SnapshotLease, Store, StoreError, StoreState,
    auto_graph_search_options, auto_uses_full_precision, exact_rescore_rows,
};
use crate::meta::{ColumnStore, ColumnStoreBuilder, EvalError, Predicate, evaluate};
use crate::quant::{Bit4Query, Int8Query, prepare_bit4_query, prepare_int8_query};
use crate::scan::{
    Int8Factors, ScanError, ScanQuery, ScanRequest as VectorScanRequest, ScanRows, ScanStats,
    gather_top_k, scan_partition,
};
use crate::segment::layout::RegionKind;

use super::{
    ALLOW_LIST_ROWS_THRESHOLD, PlanError, PlanFallback, PlanNode, SegmentBranch, SegmentPlan,
    SegmentTier, choose_scan_branch, segment_may_match, validate_predicate,
};
#[cfg(any(test, feature = "test-support"))]
use super::{
    MetadataControllerError, MetadataExecutionReceipt, MetadataQueryContext, MetadataTestController,
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
    /// Hidden metadata qualification controller was misused or poisoned.
    #[cfg(any(test, feature = "test-support"))]
    TestController(MetadataControllerError),
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
            #[cfg(any(test, feature = "test-support"))]
            Self::TestController(error) => error.fmt(formatter),
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
            #[cfg(any(test, feature = "test-support"))]
            Self::TestController(error) => Some(error),
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

#[cfg(any(test, feature = "test-support"))]
impl From<MetadataControllerError> for FilteredSearchError {
    fn from(error: MetadataControllerError) -> Self {
        Self::TestController(error)
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
        execute_store(
            self,
            request,
            predicate,
            k,
            options.into(),
            control.with_clock(Arc::clone(&self.clock)),
        )
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
    #[cfg(any(test, feature = "test-support"))]
    let metadata_controller = store.metadata_test_controller.as_deref();
    #[cfg(any(test, feature = "test-support"))]
    let metadata_query = metadata_controller
        .map(MetadataTestController::begin_query)
        .transpose()?
        .flatten();
    let score = || {
        execute_pinned(
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
            #[cfg(any(test, feature = "test-support"))]
            metadata_controller,
            #[cfg(any(test, feature = "test-support"))]
            metadata_query.as_ref(),
            #[cfg(any(test, feature = "test-support"))]
            store.vector_fault_controller.as_ref(),
        )
    };
    #[cfg(any(test, feature = "test-support"))]
    let result = crate::kernels::vector_fault::run_store_scoring(
        store.kernel_fault_controller.as_ref(),
        score,
    );
    #[cfg(not(any(test, feature = "test-support")))]
    let result = score();
    #[cfg(any(test, feature = "test-support"))]
    {
        if let Some(controller) = store.vector_fault_controller.as_ref() {
            controller.finalize_search(result.is_ok());
        }
    }
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
    #[cfg(any(test, feature = "test-support"))] metadata_controller: Option<
        &MetadataTestController,
    >,
    #[cfg(any(test, feature = "test-support"))] metadata_query: Option<&MetadataQueryContext>,
    #[cfg(any(test, feature = "test-support"))] vector_fault_controller: Option<
        &crate::scan::vector_fault::VectorFaultController,
    >,
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
    let auto_graph_options = if auto_uses_graph {
        Some(auto_graph_search_options(snapshot)?)
    } else {
        None
    };
    let full_precision = matches!(options.tier(), SearchTier::Exact | SearchTier::Graph(_))
        || (matches!(options.tier(), SearchTier::Auto)
            && auto_uses_full_precision(snapshot, active));

    if !active.is_empty() {
        execute_active_filtered(
            active,
            schema,
            request,
            predicate,
            k,
            options,
            cancellation,
            full_precision,
            &bit4_query,
            &mut candidates,
            &mut stats,
            &mut plans,
            #[cfg(any(test, feature = "test-support"))]
            metadata_controller,
            #[cfg(any(test, feature = "test-support"))]
            metadata_query,
            #[cfg(any(test, feature = "test-support"))]
            vector_fault_controller,
        )?;
    }

    for segment in ordered_filtered_segments(snapshot, options, auto_uses_graph) {
        let source = RowSource::Sealed(segment.meta().id);
        let graph_selected =
            has_graph(segment) && matches!(options.tier(), SearchTier::Auto | SearchTier::Graph(_));
        let tier = if graph_selected {
            SegmentTier::SealedGraph
        } else {
            SegmentTier::SealedScan
        };
        if !segment_may_match(segment.meta().clustering_key_range, predicate) {
            #[cfg(any(test, feature = "test-support"))]
            record_pruned_receipt(
                metadata_controller,
                metadata_query,
                source,
                segment.meta().row_count,
                0,
            )?;
            plans.push(
                SegmentPlan::exact(source, tier, SegmentBranch::Pruned, 0, None)
                    .with_predicate(predicate),
            );
            continue;
        }
        let columns = match segment.query_columns() {
            Ok(columns) => columns,
            Err(error) => {
                #[cfg(any(test, feature = "test-support"))]
                record_column_refusal(metadata_controller, metadata_query, source, &error)?;
                return Err(QueryError::Store(error).into());
            }
        };
        let alive = match segment.query_alive() {
            Ok(alive) => alive,
            Err(error) => {
                #[cfg(any(test, feature = "test-support"))]
                record_alive_refusal(metadata_controller, metadata_query, source, &error)?;
                return Err(QueryError::Store(error).into());
            }
        };
        let allow_list = evaluate_for_plan(predicate, &columns, &alive)?;
        let row_count = segment.meta().row_count as usize;
        let graph_options = match options.tier() {
            SearchTier::Graph(graph_options) if graph_selected => Some(graph_options),
            SearchTier::Auto if graph_selected => auto_graph_options,
            SearchTier::Auto | SearchTier::Exact | SearchTier::Scan | SearchTier::Graph(_) => None,
        };
        let graph_outcome = sealed_graph_filtered(
            segment,
            request.vector(),
            predicate,
            &allow_list,
            k,
            graph_options,
            cancellation,
            accounting,
            source,
            row_count,
            options,
            &mut candidates,
            &mut stats,
            &mut graph_stats,
            &mut plans,
            #[cfg(any(test, feature = "test-support"))]
            metadata_controller,
            #[cfg(any(test, feature = "test-support"))]
            metadata_query,
            #[cfg(any(test, feature = "test-support"))]
            vector_fault_controller,
        )?;
        if !matches!(graph_outcome, SealedGraphOutcome::Scan) {
            continue;
        }
        let branch = choose_scan_branch(allow_list.cardinality());
        #[cfg(any(test, feature = "test-support"))]
        record_selectivity_decision(
            metadata_controller,
            metadata_query,
            source,
            allow_list.cardinality(),
            branch,
        )?;
        let scan_reason = (tier == SegmentTier::SealedScan)
            .then(|| sealed_scan_reason(segment, options.explicit_tier()));
        let plan = SegmentPlan::exact(source, tier, branch, allow_list.cardinality(), scan_reason)
            .with_predicate(predicate);
        #[cfg(any(test, feature = "test-support"))]
        let receipt_context = metadata_execution_receipt_context(
            metadata_controller,
            metadata_query,
            source,
            row_count,
            allow_list.cardinality(),
            request.vector().len(),
            true,
        )?;
        let (local, local_stats, execution) = scan_sealed_filtered(
            segment,
            &plan,
            row_count,
            request.vector(),
            &allow_list,
            k,
            cancellation,
            full_precision,
            &bit4_query,
            &int8_query,
            #[cfg(any(test, feature = "test-support"))]
            receipt_context,
        )?;
        #[cfg(any(test, feature = "test-support"))]
        let reported_branch =
            metadata_query.map_or(plan.branch, |query| query.reported_branch(plan.branch));
        #[cfg(not(any(test, feature = "test-support")))]
        let reported_branch = plan.branch;
        verify_execution_branch(reported_branch, execution.branch)?;
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
            full_precision || segment.meta().scheme == 0,
            #[cfg(any(test, feature = "test-support"))]
            vector_fault_controller,
            #[cfg(any(test, feature = "test-support"))]
            vector_fault_tier(options.tier()),
        )?;
        stats.add(local_stats)?;
        plans.push(plan);
        retain_global_top_k(&mut candidates, k);
    }

    candidates.sort_unstable_by(crate::ingest::compare_search_candidates);
    candidates.truncate(k);
    cancellation.check_graph().map_err(map_scan_error)?;
    let stats = stats.finish();
    accounting.record_plans(&plans).map_err(QueryError::Store)?;
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

fn ordered_filtered_segments(
    snapshot: &PublishedSnapshot,
    options: SearchOptions,
    auto_uses_graph: bool,
) -> Vec<&crate::segment::reader::SegmentReader> {
    let mut segments = snapshot.segments().iter().collect::<Vec<_>>();
    if auto_uses_graph || matches!(options.tier(), SearchTier::Graph(_)) {
        segments.sort_unstable_by(|left, right| {
            right
                .meta()
                .row_count
                .cmp(&left.meta().row_count)
                .then_with(|| left.meta().id.cmp(&right.meta().id))
        });
    }
    segments
}

#[allow(clippy::too_many_arguments)]
fn execute_active_filtered(
    active: &crate::ingest::ActiveSegment,
    schema: &crate::meta::Schema,
    request: SearchRequest<'_>,
    predicate: &Predicate,
    k: usize,
    options: SearchOptions,
    cancellation: &QueryCancellation<'_>,
    full_precision: bool,
    bit4_query: &Bit4Query,
    candidates: &mut Vec<SearchCandidate>,
    stats: &mut MutableStats,
    plans: &mut Vec<SegmentPlan>,
    #[cfg(any(test, feature = "test-support"))] metadata_controller: Option<
        &MetadataTestController,
    >,
    #[cfg(any(test, feature = "test-support"))] metadata_query: Option<&MetadataQueryContext>,
    #[cfg(any(test, feature = "test-support"))] vector_fault_controller: Option<
        &crate::scan::vector_fault::VectorFaultController,
    >,
) -> Result<(), FilteredSearchError> {
    let alive = active.alive().map_err(QueryError::Store)?;
    let columns = active_columns(active, schema)?;
    #[cfg(any(test, feature = "test-support"))]
    let alive = match metadata_query {
        Some(query) => query.evaluator_alive(columns.row_count(), alive)?,
        None => alive,
    };
    let allow_list = evaluate_for_plan(predicate, &columns, &alive)?;
    let branch = choose_scan_branch(allow_list.cardinality());
    let source = RowSource::Active;
    #[cfg(any(test, feature = "test-support"))]
    record_selectivity_decision(
        metadata_controller,
        metadata_query,
        source,
        allow_list.cardinality(),
        branch,
    )?;
    let plan = SegmentPlan::exact(
        source,
        SegmentTier::ActiveScan,
        branch,
        allow_list.cardinality(),
        Some(crate::planner::ScanReason::ActiveSegment),
    )
    .with_predicate(predicate);
    #[cfg(any(test, feature = "test-support"))]
    let receipt_context = metadata_execution_receipt_context(
        metadata_controller,
        metadata_query,
        source,
        active.row_count(),
        allow_list.cardinality(),
        request.vector().len(),
        false,
    )?;
    let (local, local_stats, execution) = if full_precision {
        execute_squared_l2_plan(
            &plan.node,
            active.vectors(),
            active.row_count(),
            request.vector(),
            &allow_list,
            k,
            cancellation,
            #[cfg(any(test, feature = "test-support"))]
            receipt_context,
        )?
    } else {
        execute_scan_plan(
            &plan.node,
            VectorScanRequest {
                query: ScanQuery::Bit4(bit4_query),
                rows: ScanRows::Bit4RowMajor {
                    codes: active.codes(),
                    factors: active.factors(),
                },
                row_mask: Some(allow_list.as_roaring()),
            },
            active.row_count(),
            k,
            cancellation,
            #[cfg(any(test, feature = "test-support"))]
            receipt_context,
        )?
    };
    #[cfg(any(test, feature = "test-support"))]
    let reported_branch =
        metadata_query.map_or(plan.branch, |query| query.reported_branch(plan.branch));
    #[cfg(not(any(test, feature = "test-support")))]
    let reported_branch = plan.branch;
    verify_execution_branch(reported_branch, execution.branch)?;
    append_candidates(
        local,
        source,
        |row| Ok(active.document(row)),
        candidates,
        full_precision,
        #[cfg(any(test, feature = "test-support"))]
        vector_fault_controller,
        #[cfg(any(test, feature = "test-support"))]
        vector_fault_tier(options.tier()),
    )?;
    stats.add(local_stats)?;
    plans.push(plan);
    retain_global_top_k(candidates, k);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn scan_sealed_filtered(
    segment: &crate::segment::reader::SegmentReader,
    plan: &SegmentPlan,
    row_count: usize,
    query: &[f32],
    allow_list: &crate::meta::DocBitmap,
    k: usize,
    cancellation: &QueryCancellation<'_>,
    full_precision: bool,
    bit4_query: &Bit4Query,
    int8_query: &Int8Query,
    #[cfg(any(test, feature = "test-support"))] receipt_context: Option<
        MetadataExecutionReceiptContext<'_>,
    >,
) -> Result<(Vec<LocalCandidate>, ScanStats, ScanExecutionFacts), FilteredSearchError> {
    if full_precision {
        let vectors = exact_rescore_rows(segment)?;
        return execute_squared_l2_plan(
            &plan.node,
            vectors,
            row_count,
            query,
            allow_list,
            k,
            cancellation,
            #[cfg(any(test, feature = "test-support"))]
            receipt_context,
        );
    }
    match segment.meta().scheme {
        0 => execute_scan_plan(
            &plan.node,
            VectorScanRequest {
                query: ScanQuery::F32(query),
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
            #[cfg(any(test, feature = "test-support"))]
            receipt_context,
        ),
        2 => {
            let factors = segment
                .query_int8_factors_slice()
                .map_err(StoreError::Segment)
                .map_err(QueryError::Store)?
                .iter()
                .map(|factor| Int8Factors::new(factor.scale, factor.offset))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| {
                    QueryError::Store(StoreError::Segment(crate::segment::SegmentError::Geometry(
                        "Int8 factor is not finite and non-negative".to_owned(),
                    )))
                })?;
            execute_scan_plan(
                &plan.node,
                VectorScanRequest {
                    query: ScanQuery::Int8(int8_query),
                    rows: ScanRows::Int8RowMajor {
                        codes: segment
                            .query_int8_codes()
                            .map_err(StoreError::Segment)
                            .map_err(QueryError::Store)?,
                        factors: &factors,
                    },
                    row_mask: Some(allow_list.as_roaring()),
                },
                row_count,
                k,
                cancellation,
                #[cfg(any(test, feature = "test-support"))]
                receipt_context,
            )
        }
        4 => execute_scan_plan(
            &plan.node,
            VectorScanRequest {
                query: ScanQuery::Bit4(bit4_query),
                rows: ScanRows::Bit4RowMajor {
                    codes: segment
                        .query_bit4_codes()
                        .map_err(StoreError::Segment)
                        .map_err(QueryError::Store)?,
                    factors: segment
                        .query_bit4_factors()
                        .map_err(StoreError::Segment)
                        .map_err(QueryError::Store)?,
                },
                row_mask: Some(allow_list.as_roaring()),
            },
            row_count,
            k,
            cancellation,
            #[cfg(any(test, feature = "test-support"))]
            receipt_context,
        ),
        scheme => Err(QueryError::Store(StoreError::Segment(
            crate::segment::SegmentError::Geometry(format!(
                "filtered store search does not support sealed scheme {scheme}"
            )),
        ))
        .into()),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SealedGraphOutcome {
    Scan,
    Pruned,
    Executed,
}

#[allow(clippy::too_many_arguments)]
fn sealed_graph_filtered(
    segment: &crate::segment::reader::SegmentReader,
    query: &[f32],
    predicate: &Predicate,
    allow_list: &crate::meta::DocBitmap,
    k: usize,
    graph_options: Option<GraphSearchOptions>,
    cancellation: &QueryCancellation<'_>,
    accounting: &Arc<crate::lifecycle::stats::Accounting>,
    source: RowSource,
    row_count: usize,
    options: SearchOptions,
    candidates: &mut Vec<SearchCandidate>,
    stats: &mut MutableStats,
    graph_stats: &mut crate::ingest::GraphSearchStats,
    plans: &mut Vec<SegmentPlan>,
    #[cfg(any(test, feature = "test-support"))] metadata_controller: Option<
        &MetadataTestController,
    >,
    #[cfg(any(test, feature = "test-support"))] metadata_query: Option<&MetadataQueryContext>,
    #[cfg(any(test, feature = "test-support"))] vector_fault_controller: Option<
        &crate::scan::vector_fault::VectorFaultController,
    >,
) -> Result<SealedGraphOutcome, FilteredSearchError> {
    let Some(graph_options) = graph_options else {
        return Ok(SealedGraphOutcome::Scan);
    };
    validate_filtered_explicit_ef(graph_options, k.min(row_count), row_count)?;
    if allow_list.cardinality() <= ALLOW_LIST_ROWS_THRESHOLD {
        return Ok(SealedGraphOutcome::Scan);
    }
    let prepared = segment
        .graph_search_cache
        .prepare_shared(segment, cancellation)
        .map_err(map_graph_cache_error)?;
    let norm_range = prepared.norm_range;
    if let Some(competitive_distance) = global_competitive_distance(candidates, k)
        && norm_range.squared_l2_upper_bound(query) <= f64::from(f32::MAX)
        && (norm_range.squared_l2_lower_bound(query) as f32) > competitive_distance
    {
        cancellation.check_graph().map_err(map_scan_error)?;
        #[cfg(any(test, feature = "test-support"))]
        record_pruned_receipt(
            metadata_controller,
            metadata_query,
            source,
            segment.meta().row_count,
            allow_list.cardinality(),
        )?;
        plans.push(
            SegmentPlan::exact(
                source,
                SegmentTier::SealedGraph,
                SegmentBranch::Pruned,
                allow_list.cardinality(),
                None,
            )
            .with_predicate(predicate),
        );
        return Ok(SealedGraphOutcome::Pruned);
    }
    let execution = execute_filtered_graph(
        segment,
        query,
        allow_list,
        k,
        graph_options,
        cancellation,
        accounting,
        prepared.graph_validated,
        prepared.entry_seed_discovered,
        #[cfg(any(test, feature = "test-support"))]
        metadata_query,
        #[cfg(any(test, feature = "test-support"))]
        metadata_execution_receipt_context(
            metadata_controller,
            metadata_query,
            source,
            row_count,
            allow_list.cardinality(),
            query.len(),
            true,
        )?,
        #[cfg(any(test, feature = "test-support"))]
        vector_fault_controller,
    )?;
    let plan = SegmentPlan::filtered_graph(
        source,
        allow_list.cardinality(),
        execution.ef_requested,
        execution.ef_effective,
        execution.branch,
        execution.fallback,
        graph_options.profile(),
    )
    .with_predicate(predicate);
    #[cfg(any(test, feature = "test-support"))]
    let reported_branch =
        metadata_query.map_or(plan.branch, |query| query.reported_branch(plan.branch));
    #[cfg(not(any(test, feature = "test-support")))]
    let reported_branch = plan.branch;
    verify_execution_branch(reported_branch, execution.branch)?;
    #[cfg(any(test, feature = "test-support"))]
    record_visited_fallback_feature(
        metadata_controller,
        metadata_query,
        source,
        allow_list.cardinality(),
        &execution,
    )?;
    append_candidates(
        execution.candidates,
        source,
        |row| {
            segment
                .document_version(row)
                .map_err(StoreError::Segment)
                .map_err(QueryError::Store)
        },
        candidates,
        true,
        #[cfg(any(test, feature = "test-support"))]
        vector_fault_controller,
        #[cfg(any(test, feature = "test-support"))]
        vector_fault_tier(options.tier()),
    )?;
    stats.add(execution.stats)?;
    add_graph_stats(graph_stats, execution.graph_stats)?;
    plans.push(plan);
    retain_global_top_k(candidates, k);
    Ok(SealedGraphOutcome::Executed)
}

fn sealed_scan_reason(
    segment: &crate::segment::reader::SegmentReader,
    requested_tier: Option<SearchTier>,
) -> crate::planner::ScanReason {
    use crate::planner::{ExplicitScanTier, ScanReason};

    match requested_tier {
        Some(SearchTier::Exact) => return ScanReason::ExplicitTier(ExplicitScanTier::Exact),
        Some(SearchTier::Scan) => return ScanReason::ExplicitTier(ExplicitScanTier::Scan),
        Some(SearchTier::Auto | SearchTier::Graph(_)) | None => {}
    }
    let rows = segment.meta().row_count;
    let min_rows = crate::tier::thresholds::for_bucket(segment.meta().dims, segment.meta().scheme)
        .graph_min_rows;
    if rows < min_rows {
        ScanReason::BelowGraphThreshold { rows, min_rows }
    } else {
        ScanReason::GraphPending
    }
}

struct FilteredGraphExecution {
    candidates: Vec<LocalCandidate>,
    stats: ScanStats,
    branch: SegmentBranch,
    fallback: PlanFallback,
    ef_requested: Option<usize>,
    ef_effective: usize,
    graph_stats: crate::ingest::GraphSearchStats,
    #[cfg(any(test, feature = "test-support"))]
    graph_nodes_visited: usize,
    #[cfg(any(test, feature = "test-support"))]
    exact_fallback_rows_examined: u64,
    #[cfg(any(test, feature = "test-support"))]
    visited_budget: usize,
    #[cfg(any(test, feature = "test-support"))]
    visited_budget_guard: Option<(usize, usize)>,
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

struct FilteredGraphBudget {
    target_k: usize,
    ef_effective: usize,
    filtered_visited_budget: usize,
    scratch_capacity: usize,
    maximum_corrective_ef: usize,
}

#[allow(clippy::too_many_arguments)]
fn filtered_graph_budget(
    graph: crate::graph::block::GraphNodeBlocks<'_>,
    query: &[f32],
    options: GraphSearchOptions,
    allow_count: usize,
    node_count: usize,
    k: usize,
    #[cfg(any(test, feature = "test-support"))] metadata_query: Option<&MetadataQueryContext>,
) -> Result<FilteredGraphBudget, FilteredSearchError> {
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
    let computed_visited_budget = ef_effective
        .checked_mul(max_degree)
        .and_then(|value| value.checked_mul(FILTERED_VISITED_BUDGET_MULTIPLIER))
        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?
        .min(node_count);
    #[cfg(any(test, feature = "test-support"))]
    let filtered_visited_budget = metadata_query
        .and_then(MetadataQueryContext::visited_budget_override)
        .unwrap_or(computed_visited_budget)
        .min(node_count);
    #[cfg(not(any(test, feature = "test-support")))]
    let filtered_visited_budget = computed_visited_budget;
    let scratch_capacity = ef_effective.max(filtered_visited_budget);
    let maximum_corrective_ef = allow_count.min(scratch_capacity);
    Ok(FilteredGraphBudget {
        target_k,
        ef_effective,
        filtered_visited_budget,
        scratch_capacity,
        maximum_corrective_ef,
    })
}

struct FallbackContext<'a, 'cancel> {
    rescore: &'a [f32],
    node_count: usize,
    query: &'a [f32],
    allow_list: &'a crate::meta::DocBitmap,
    k: usize,
    cancellation: &'a QueryCancellation<'cancel>,
    ef_requested: Option<usize>,
    #[cfg(any(test, feature = "test-support"))]
    filtered_visited_budget: usize,
    #[cfg(any(test, feature = "test-support"))]
    receipt_context: Option<MetadataExecutionReceiptContext<'a>>,
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
    #[cfg(any(test, feature = "test-support"))] metadata_query: Option<&MetadataQueryContext>,
    #[cfg(any(test, feature = "test-support"))] receipt_context: Option<
        MetadataExecutionReceiptContext<'_>,
    >,
    #[cfg(any(test, feature = "test-support"))] vector_fault_controller: Option<
        &crate::scan::vector_fault::VectorFaultController,
    >,
) -> Result<FilteredGraphExecution, FilteredSearchError> {
    cancellation.check_graph().map_err(map_scan_error)?;
    let prepared = segment
        .graph_search_cache
        .prepare_shared(segment, cancellation)
        .map_err(map_graph_cache_error)?;
    let graph = prepared.graph;
    let rescore = crate::lifecycle::query_rescore_rows(segment)?;
    let node_count = graph.node_count() as usize;
    let allow_count = usize::try_from(allow_list.cardinality())
        .map_err(|_| QueryError::Scan(ScanError::ArithmeticOverflow))?;
    let FilteredGraphBudget {
        target_k,
        ef_effective,
        filtered_visited_budget,
        scratch_capacity,
        maximum_corrective_ef,
    } = filtered_graph_budget(
        graph,
        query,
        options,
        allow_count,
        node_count,
        k,
        #[cfg(any(test, feature = "test-support"))]
        metadata_query,
    )?;
    let mut scratch = segment
        .graph_search_cache
        .checkout(graph, scratch_capacity, accounting, cancellation)
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
    .map_err(map_graph_error)?
    .with_rescore_validator(segment);
    #[cfg(any(test, feature = "test-support"))]
    if let Some(controller) = vector_fault_controller {
        searcher = searcher.with_vector_fault_controller(
            controller,
            crate::scan::vector_fault::VectorRowSource::Sealed(*segment.meta().id.as_bytes()),
            crate::scan::vector_fault::VectorSearchTier::Graph,
        );
    }
    let ef_requested = options.ef();
    let mut current_ef = ef_effective;
    let mut traversal_stats = MutableStats::default();
    let mut graph_stats = crate::ingest::GraphSearchStats {
        graph_validations: usize::from(graph_validated_before || prepared.graph_validated),
        entry_seed_discoveries: usize::from(entry_seed_discovered),
        ..crate::ingest::GraphSearchStats::default()
    };
    #[cfg(any(test, feature = "test-support"))]
    let mut graph_nodes_visited = 0_usize;
    let fallback_context = FallbackContext {
        rescore,
        node_count,
        query,
        allow_list,
        k,
        cancellation,
        ef_requested,
        #[cfg(any(test, feature = "test-support"))]
        filtered_visited_budget,
        #[cfg(any(test, feature = "test-support"))]
        receipt_context,
    };
    loop {
        let request = GraphSearchRequest::new(query, target_k, options.seed())
            .with_profile(options.profile())
            .with_ef(current_ef);
        let outcome = match searcher.search_filtered(
            request,
            allow_list,
            filtered_visited_budget,
            Some(cancellation),
        ) {
            Ok(outcome) => outcome,
            Err(crate::graph::search::GraphSearchError::FilteredCandidateShortfall {
                counters,
                ..
            }) => {
                #[cfg(any(test, feature = "test-support"))]
                {
                    graph_nodes_visited = graph_nodes_visited
                        .checked_add(counters.visited())
                        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
                }
                add_graph_counters(&mut graph_stats, counters)?;
                traversal_stats.add(graph_scan_stats(counters)?)?;
                return exact_filtered_graph_fallback(
                    &fallback_context,
                    traversal_stats,
                    PlanFallback::CandidateShortfall,
                    current_ef,
                    graph_stats,
                    #[cfg(any(test, feature = "test-support"))]
                    graph_nodes_visited,
                    #[cfg(any(test, feature = "test-support"))]
                    None,
                );
            }
            Err(error) => return Err(map_graph_error(error)),
        };
        match outcome {
            FilteredGraphSearchOutcome::Traversed(result) => {
                #[cfg(any(test, feature = "test-support"))]
                {
                    graph_nodes_visited = graph_nodes_visited
                        .checked_add(result.counters().visited())
                        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
                }
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
                    let execution = FilteredGraphExecution {
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
                        #[cfg(any(test, feature = "test-support"))]
                        graph_nodes_visited,
                        #[cfg(any(test, feature = "test-support"))]
                        exact_fallback_rows_examined: 0,
                        #[cfg(any(test, feature = "test-support"))]
                        visited_budget: filtered_visited_budget,
                        #[cfg(any(test, feature = "test-support"))]
                        visited_budget_guard: None,
                    };
                    #[cfg(any(test, feature = "test-support"))]
                    if let Some(context) = fallback_context.receipt_context {
                        context.record_graph(&execution)?;
                    }
                    return Ok(execution);
                }
                if current_ef < maximum_corrective_ef {
                    current_ef = current_ef
                        .saturating_mul(2)
                        .max(current_ef.saturating_add(1))
                        .min(maximum_corrective_ef);
                    continue;
                }
                return exact_filtered_graph_fallback(
                    &fallback_context,
                    traversal_stats,
                    PlanFallback::CandidateShortfall,
                    current_ef,
                    graph_stats,
                    #[cfg(any(test, feature = "test-support"))]
                    graph_nodes_visited,
                    #[cfg(any(test, feature = "test-support"))]
                    None,
                );
            }
            FilteredGraphSearchOutcome::VisitedBudgetExceeded {
                visited,
                budget,
                counters,
            } => {
                #[cfg(not(any(test, feature = "test-support")))]
                let _ = (visited, budget);
                #[cfg(any(test, feature = "test-support"))]
                {
                    graph_nodes_visited = graph_nodes_visited
                        .checked_add(counters.visited())
                        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
                }
                add_graph_counters(&mut graph_stats, counters)?;
                traversal_stats.add(graph_scan_stats(counters)?)?;
                return exact_filtered_graph_fallback(
                    &fallback_context,
                    traversal_stats,
                    PlanFallback::VisitedBudget,
                    current_ef,
                    graph_stats,
                    #[cfg(any(test, feature = "test-support"))]
                    graph_nodes_visited,
                    #[cfg(any(test, feature = "test-support"))]
                    Some((visited, budget)),
                );
            }
        }
    }
}

fn exact_filtered_graph_fallback(
    context: &FallbackContext<'_, '_>,
    mut traversal_stats: MutableStats,
    fallback: PlanFallback,
    ef_effective: usize,
    graph_stats: crate::ingest::GraphSearchStats,
    #[cfg(any(test, feature = "test-support"))] graph_nodes_visited: usize,
    #[cfg(any(test, feature = "test-support"))] visited_budget_guard: Option<(usize, usize)>,
) -> Result<FilteredGraphExecution, FilteredSearchError> {
    let (candidates, exact_stats, _) = execute_squared_l2_rows(
        context.rescore,
        context.node_count,
        context.query,
        context.allow_list,
        SegmentBranch::ExactAllowList,
        context.k,
        context.cancellation,
        #[cfg(any(test, feature = "test-support"))]
        None,
    )?;
    traversal_stats.add(exact_stats)?;
    #[cfg(any(test, feature = "test-support"))]
    let exact_fallback_rows_examined = context.allow_list.cardinality();
    let execution = FilteredGraphExecution {
        candidates,
        stats: traversal_stats.finish(),
        branch: SegmentBranch::GraphExactFallback,
        fallback,
        ef_requested: context.ef_requested,
        ef_effective,
        graph_stats: with_traversed_segment(graph_stats)?,
        #[cfg(any(test, feature = "test-support"))]
        graph_nodes_visited,
        #[cfg(any(test, feature = "test-support"))]
        exact_fallback_rows_examined,
        #[cfg(any(test, feature = "test-support"))]
        visited_budget: context.filtered_visited_budget,
        #[cfg(any(test, feature = "test-support"))]
        visited_budget_guard,
    };
    #[cfg(any(test, feature = "test-support"))]
    if let Some(receipt_context) = context.receipt_context {
        receipt_context.record_graph(&execution)?;
    }
    Ok(execution)
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
        crate::graph::search::GraphSearchError::ExactRescoreUnavailable(detail) => {
            QueryError::Store(StoreError::Segment(crate::segment::SegmentError::Geometry(
                detail,
            )))
        }
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

#[cfg(any(test, feature = "test-support"))]
fn record_selectivity_decision(
    controller: Option<&MetadataTestController>,
    query: Option<&MetadataQueryContext>,
    source: RowSource,
    cardinality: u64,
    branch: SegmentBranch,
) -> Result<(), FilteredSearchError> {
    if let (Some(controller), Some(query)) = (controller, query) {
        query.record_selectivity(
            controller,
            source,
            cardinality,
            ALLOW_LIST_ROWS_THRESHOLD,
            branch,
        )?;
    }
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
fn record_pruned_receipt(
    controller: Option<&MetadataTestController>,
    query: Option<&MetadataQueryContext>,
    source: RowSource,
    row_count: u32,
    filter_cardinality: u64,
) -> Result<(), FilteredSearchError> {
    if let (Some(controller), Some(query)) = (controller, query) {
        controller.record_execution(MetadataExecutionReceipt {
            query_id: query.query_id(),
            source,
            row_count: u64::from(row_count),
            filter_cardinality,
            branch: SegmentBranch::Pruned,
            fallback: PlanFallback::None,
            rows_examined: 0,
            allowed_rows_examined: 0,
            vectors_scored: 0,
            graph_nodes_visited: 0,
            exact_fallback_rows_examined: 0,
            returned_candidates: 0,
            ef_effective: None,
            visited_budget: None,
            sealed: true,
        })?;
    }
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
fn record_column_refusal(
    controller: Option<&MetadataTestController>,
    query: Option<&MetadataQueryContext>,
    source: RowSource,
    error: &StoreError,
) -> Result<(), FilteredSearchError> {
    if let (Some(controller), Some(query)) = (controller, query) {
        query.record_column_refusal(controller, source, error)?;
    }
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
fn record_alive_refusal(
    controller: Option<&MetadataTestController>,
    query: Option<&MetadataQueryContext>,
    source: RowSource,
    error: &StoreError,
) -> Result<(), FilteredSearchError> {
    if let (Some(controller), Some(query)) = (controller, query) {
        query.record_alive_refusal(controller, source, error)?;
    }
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
fn record_visited_fallback_feature(
    controller: Option<&MetadataTestController>,
    query: Option<&MetadataQueryContext>,
    source: RowSource,
    filter_cardinality: u64,
    execution: &FilteredGraphExecution,
) -> Result<(), FilteredSearchError> {
    let (Some(controller), Some(query), Some((visited, budget))) =
        (controller, query, execution.visited_budget_guard)
    else {
        return Ok(());
    };
    query.record_visited_fallback(
        controller,
        source,
        visited,
        budget,
        filter_cardinality,
        execution.exact_fallback_rows_examined,
        u64::try_from(execution.candidates.len())
            .map_err(|_| QueryError::Scan(ScanError::ArithmeticOverflow))?,
        execution.fallback,
    )?;
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct LocalCandidate {
    row: usize,
    score: f32,
}

/// Facts set by the concrete executor branch that actually ran and returned
/// independently to the public report guard.
#[derive(Clone, Copy, Debug)]
struct ScanExecutionFacts {
    branch: SegmentBranch,
    #[cfg(any(test, feature = "test-support"))]
    rows_examined: u64,
    #[cfg(any(test, feature = "test-support"))]
    allowed_rows_examined: u64,
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Copy)]
struct MetadataExecutionReceiptContext<'a> {
    controller: &'a MetadataTestController,
    query: &'a MetadataQueryContext,
    source: RowSource,
    row_count: u64,
    filter_cardinality: u64,
    dimensions: u64,
    sealed: bool,
}

#[cfg(any(test, feature = "test-support"))]
impl MetadataExecutionReceiptContext<'_> {
    fn record_scan(
        self,
        execution: ScanExecutionFacts,
        stats: &ScanStats,
        returned_candidates: usize,
    ) -> Result<(), FilteredSearchError> {
        let vectors_scored = if self.dimensions == 0 {
            0
        } else {
            stats.dims_touched / self.dimensions
        };
        self.controller.record_execution(MetadataExecutionReceipt {
            query_id: self.query.query_id(),
            source: self.source,
            row_count: self.row_count,
            filter_cardinality: self.filter_cardinality,
            branch: execution.branch,
            fallback: PlanFallback::None,
            rows_examined: execution.rows_examined,
            allowed_rows_examined: execution.allowed_rows_examined,
            vectors_scored,
            graph_nodes_visited: 0,
            exact_fallback_rows_examined: 0,
            returned_candidates: u64::try_from(returned_candidates)
                .map_err(|_| QueryError::Scan(ScanError::ArithmeticOverflow))?,
            ef_effective: None,
            visited_budget: None,
            sealed: self.sealed,
        })?;
        Ok(())
    }

    fn record_graph(self, execution: &FilteredGraphExecution) -> Result<(), FilteredSearchError> {
        let graph_nodes_visited = u64::try_from(execution.graph_nodes_visited)
            .map_err(|_| QueryError::Scan(ScanError::ArithmeticOverflow))?;
        self.controller.record_execution(MetadataExecutionReceipt {
            query_id: self.query.query_id(),
            source: self.source,
            row_count: self.row_count,
            filter_cardinality: self.filter_cardinality,
            branch: execution.branch,
            fallback: execution.fallback,
            rows_examined: graph_nodes_visited,
            allowed_rows_examined: self.filter_cardinality,
            vectors_scored: u64::try_from(execution.graph_stats.candidates_scored)
                .map_err(|_| QueryError::Scan(ScanError::ArithmeticOverflow))?,
            graph_nodes_visited,
            exact_fallback_rows_examined: execution.exact_fallback_rows_examined,
            returned_candidates: u64::try_from(execution.candidates.len())
                .map_err(|_| QueryError::Scan(ScanError::ArithmeticOverflow))?,
            ef_effective: Some(execution.ef_effective),
            visited_budget: Some(execution.visited_budget),
            sealed: self.sealed,
        })?;
        Ok(())
    }
}

#[cfg(any(test, feature = "test-support"))]
#[allow(clippy::too_many_arguments)]
fn metadata_execution_receipt_context<'a>(
    controller: Option<&'a MetadataTestController>,
    query: Option<&'a MetadataQueryContext>,
    source: RowSource,
    row_count: usize,
    filter_cardinality: u64,
    dimensions: usize,
    sealed: bool,
) -> Result<Option<MetadataExecutionReceiptContext<'a>>, FilteredSearchError> {
    let (Some(controller), Some(query)) = (controller, query) else {
        return Ok(None);
    };
    Ok(Some(MetadataExecutionReceiptContext {
        controller,
        query,
        source,
        row_count: u64::try_from(row_count)
            .map_err(|_| QueryError::Scan(ScanError::ArithmeticOverflow))?,
        filter_cardinality,
        dimensions: u64::try_from(dimensions)
            .map_err(|_| QueryError::Scan(ScanError::ArithmeticOverflow))?,
        sealed,
    }))
}

fn execute_scan_request(
    request: VectorScanRequest<'_>,
    row_count: usize,
    branch: SegmentBranch,
    k: usize,
    cancellation: &QueryCancellation<'_>,
    #[cfg(any(test, feature = "test-support"))] receipt_context: Option<
        MetadataExecutionReceiptContext<'_>,
    >,
) -> Result<(Vec<LocalCandidate>, ScanStats, ScanExecutionFacts), FilteredSearchError> {
    match branch {
        SegmentBranch::FilteredGraph | SegmentBranch::Graph => {
            Err(FilteredSearchError::InvalidPlanNode(
                "filtered graph branch reached exact scan request",
            ))
        }
        SegmentBranch::Pruned => finish_scan_request(
            crate::scan::ScanOutcome {
                candidates: Vec::new(),
                worst_score: None,
                stats: ScanStats {
                    dims_touched: 0,
                    bytes_read: 0,
                    threads_used: 0,
                    worker_thread_ids: Vec::new(),
                },
            },
            ScanExecutionFacts {
                branch: SegmentBranch::Pruned,
                #[cfg(any(test, feature = "test-support"))]
                rows_examined: 0,
                #[cfg(any(test, feature = "test-support"))]
                allowed_rows_examined: 0,
            },
            #[cfg(any(test, feature = "test-support"))]
            receipt_context,
        ),
        SegmentBranch::ExactAllowList => {
            let outcome = gather_top_k(request, k, Some(cancellation)).map_err(map_scan_error)?;
            finish_scan_request(
                outcome,
                ScanExecutionFacts {
                    branch: SegmentBranch::ExactAllowList,
                    #[cfg(any(test, feature = "test-support"))]
                    rows_examined: request.row_mask.map_or(0, roaring::RoaringBitmap::len),
                    #[cfg(any(test, feature = "test-support"))]
                    allowed_rows_examined: request.row_mask.map_or(0, roaring::RoaringBitmap::len),
                },
                #[cfg(any(test, feature = "test-support"))]
                receipt_context,
            )
        }
        SegmentBranch::MaskedScan | SegmentBranch::GraphExactFallback => {
            let partition = scan_partition(request, k, 0..row_count, Some(cancellation))
                .map_err(map_scan_error)?;
            finish_scan_request(
                crate::scan::ScanOutcome {
                    candidates: partition.candidates,
                    worst_score: None,
                    stats: ScanStats {
                        dims_touched: partition.dims_touched,
                        bytes_read: partition.bytes_read,
                        threads_used: 1,
                        worker_thread_ids: vec![partition.worker_thread_id],
                    },
                },
                ScanExecutionFacts {
                    branch,
                    #[cfg(any(test, feature = "test-support"))]
                    rows_examined: u64::try_from(row_count)
                        .map_err(|_| QueryError::Scan(ScanError::ArithmeticOverflow))?,
                    #[cfg(any(test, feature = "test-support"))]
                    allowed_rows_examined: request
                        .row_mask
                        .map_or_else(|| u64::try_from(row_count), |mask| Ok(mask.len()))
                        .map_err(|_| QueryError::Scan(ScanError::ArithmeticOverflow))?,
                },
                #[cfg(any(test, feature = "test-support"))]
                receipt_context,
            )
        }
    }
}

fn finish_scan_request(
    outcome: crate::scan::ScanOutcome,
    execution: ScanExecutionFacts,
    #[cfg(any(test, feature = "test-support"))] receipt_context: Option<
        MetadataExecutionReceiptContext<'_>,
    >,
) -> Result<(Vec<LocalCandidate>, ScanStats, ScanExecutionFacts), FilteredSearchError> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(context) = receipt_context {
        context.record_scan(execution, &outcome.stats, outcome.candidates.len())?;
    }
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
        execution,
    ))
}

fn execute_scan_plan(
    node: &PlanNode,
    request: VectorScanRequest<'_>,
    row_count: usize,
    k: usize,
    cancellation: &QueryCancellation<'_>,
    #[cfg(any(test, feature = "test-support"))] receipt_context: Option<
        MetadataExecutionReceiptContext<'_>,
    >,
) -> Result<(Vec<LocalCandidate>, ScanStats, ScanExecutionFacts), FilteredSearchError> {
    match node {
        PlanNode::BitmapIntersect { input, .. } => execute_scan_plan(
            input,
            request,
            row_count,
            k,
            cancellation,
            #[cfg(any(test, feature = "test-support"))]
            receipt_context,
        ),
        PlanNode::Scan { branch, .. } => execute_scan_request(
            request,
            row_count,
            *branch,
            k,
            cancellation,
            #[cfg(any(test, feature = "test-support"))]
            receipt_context,
        ),
        PlanNode::Graph {
            fallback: Some(fallback),
            ..
        } => {
            let (candidates, stats, mut execution) = execute_scan_plan(
                fallback,
                request,
                row_count,
                k,
                cancellation,
                #[cfg(any(test, feature = "test-support"))]
                None,
            )?;
            execution.branch = SegmentBranch::GraphExactFallback;
            #[cfg(any(test, feature = "test-support"))]
            if let Some(context) = receipt_context {
                context.record_scan(execution, &stats, candidates.len())?;
            }
            Ok((candidates, stats, execution))
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
    #[cfg(any(test, feature = "test-support"))] receipt_context: Option<
        MetadataExecutionReceiptContext<'_>,
    >,
) -> Result<(Vec<LocalCandidate>, ScanStats, ScanExecutionFacts), FilteredSearchError> {
    match node {
        PlanNode::BitmapIntersect { input, .. } => execute_squared_l2_plan(
            input,
            vectors,
            row_count,
            query,
            allow_list,
            k,
            cancellation,
            #[cfg(any(test, feature = "test-support"))]
            receipt_context,
        ),
        PlanNode::Scan { branch, .. } => execute_squared_l2_rows(
            vectors,
            row_count,
            query,
            allow_list,
            *branch,
            k,
            cancellation,
            #[cfg(any(test, feature = "test-support"))]
            receipt_context,
        ),
        PlanNode::Graph {
            fallback: Some(fallback),
            ..
        } => {
            let (candidates, stats, mut execution) = execute_squared_l2_plan(
                fallback,
                vectors,
                row_count,
                query,
                allow_list,
                k,
                cancellation,
                #[cfg(any(test, feature = "test-support"))]
                None,
            )?;
            execution.branch = SegmentBranch::GraphExactFallback;
            #[cfg(any(test, feature = "test-support"))]
            if let Some(context) = receipt_context {
                context.record_scan(execution, &stats, candidates.len())?;
            }
            Ok((candidates, stats, execution))
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
fn execute_squared_l2_rows(
    vectors: &[f32],
    row_count: usize,
    query: &[f32],
    allow_list: &crate::meta::DocBitmap,
    branch: SegmentBranch,
    k: usize,
    cancellation: &QueryCancellation<'_>,
    #[cfg(any(test, feature = "test-support"))] receipt_context: Option<
        MetadataExecutionReceiptContext<'_>,
    >,
) -> Result<(Vec<LocalCandidate>, ScanStats, ScanExecutionFacts), FilteredSearchError> {
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
    #[cfg(any(test, feature = "test-support"))]
    let mut rows_examined = 0_u64;
    #[cfg(any(test, feature = "test-support"))]
    let mut allowed_rows_examined = 0_u64;
    match branch {
        SegmentBranch::FilteredGraph | SegmentBranch::Graph => {
            Err(FilteredSearchError::InvalidPlanNode(
                "filtered graph branch reached exact squared-L2 rows",
            ))
        }
        SegmentBranch::Pruned => finish_squared_l2_rows(
            candidates,
            scored_rows,
            query.len(),
            SegmentBranch::Pruned,
            k,
            #[cfg(any(test, feature = "test-support"))]
            rows_examined,
            #[cfg(any(test, feature = "test-support"))]
            allowed_rows_examined,
            #[cfg(any(test, feature = "test-support"))]
            receipt_context,
        ),
        SegmentBranch::ExactAllowList => {
            for row in allow_list.iter() {
                #[cfg(any(test, feature = "test-support"))]
                {
                    rows_examined = rows_examined
                        .checked_add(1)
                        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
                    allowed_rows_examined = allowed_rows_examined
                        .checked_add(1)
                        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
                }
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
            finish_squared_l2_rows(
                candidates,
                scored_rows,
                query.len(),
                SegmentBranch::ExactAllowList,
                k,
                #[cfg(any(test, feature = "test-support"))]
                rows_examined,
                #[cfg(any(test, feature = "test-support"))]
                allowed_rows_examined,
                #[cfg(any(test, feature = "test-support"))]
                receipt_context,
            )
        }
        SegmentBranch::MaskedScan | SegmentBranch::GraphExactFallback => {
            for row in 0..row_count {
                #[cfg(any(test, feature = "test-support"))]
                {
                    rows_examined = rows_examined
                        .checked_add(1)
                        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
                }
                if row.is_multiple_of(64) {
                    cancellation.check_graph().map_err(map_scan_error)?;
                }
                let local_row = u32::try_from(row)
                    .map_err(|_| QueryError::Scan(ScanError::ArithmeticOverflow))?;
                if allow_list.contains(local_row) {
                    #[cfg(any(test, feature = "test-support"))]
                    {
                        allowed_rows_examined = allowed_rows_examined
                            .checked_add(1)
                            .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
                    }
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
            finish_squared_l2_rows(
                candidates,
                scored_rows,
                query.len(),
                branch,
                k,
                #[cfg(any(test, feature = "test-support"))]
                rows_examined,
                #[cfg(any(test, feature = "test-support"))]
                allowed_rows_examined,
                #[cfg(any(test, feature = "test-support"))]
                receipt_context,
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn finish_squared_l2_rows(
    mut candidates: Vec<LocalCandidate>,
    scored_rows: u64,
    dimensions: usize,
    executed_branch: SegmentBranch,
    k: usize,
    #[cfg(any(test, feature = "test-support"))] rows_examined: u64,
    #[cfg(any(test, feature = "test-support"))] allowed_rows_examined: u64,
    #[cfg(any(test, feature = "test-support"))] receipt_context: Option<
        MetadataExecutionReceiptContext<'_>,
    >,
) -> Result<(Vec<LocalCandidate>, ScanStats, ScanExecutionFacts), FilteredSearchError> {
    candidates.sort_unstable_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.row.cmp(&right.row))
    });
    crate::scan::truncate_to_k_with_score_ties(&mut candidates, k, |candidate| candidate.score);
    let dimensions =
        u64::try_from(dimensions).map_err(|_| QueryError::Scan(ScanError::ArithmeticOverflow))?;
    let dims_touched = scored_rows
        .checked_mul(dimensions)
        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
    let bytes_read = dims_touched
        .checked_mul(std::mem::size_of::<f32>() as u64)
        .ok_or(QueryError::Scan(ScanError::ArithmeticOverflow))?;
    let stats = ScanStats {
        dims_touched,
        bytes_read,
        threads_used: usize::from(scored_rows != 0),
        worker_thread_ids: (scored_rows != 0)
            .then(|| std::thread::current().id())
            .into_iter()
            .collect(),
    };
    let execution = ScanExecutionFacts {
        branch: executed_branch,
        #[cfg(any(test, feature = "test-support"))]
        rows_examined,
        #[cfg(any(test, feature = "test-support"))]
        allowed_rows_examined,
    };
    #[cfg(any(test, feature = "test-support"))]
    if let Some(context) = receipt_context {
        context.record_scan(execution, &stats, candidates.len())?;
    }
    Ok((candidates, stats, execution))
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
    #[cfg(any(test, feature = "test-support"))] vector_fault_controller: Option<
        &crate::scan::vector_fault::VectorFaultController,
    >,
    #[cfg(any(test, feature = "test-support"))]
    vector_fault_tier: crate::scan::vector_fault::VectorSearchTier,
) -> Result<(), FilteredSearchError> {
    crate::lifecycle::reserve_global_candidates(
        candidates,
        local.len(),
        #[cfg(any(test, feature = "test-support"))]
        vector_fault_controller,
        #[cfg(any(test, feature = "test-support"))]
        vector_fault_tier,
    )?;
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

#[cfg(any(test, feature = "test-support"))]
fn vector_fault_tier(tier: SearchTier) -> crate::scan::vector_fault::VectorSearchTier {
    match tier {
        SearchTier::Exact => crate::scan::vector_fault::VectorSearchTier::Exact,
        SearchTier::Scan => crate::scan::vector_fault::VectorSearchTier::Scan,
        SearchTier::Graph(_) => crate::scan::vector_fault::VectorSearchTier::Graph,
        SearchTier::Auto => crate::scan::vector_fault::VectorSearchTier::Auto,
    }
}

fn retain_global_top_k(candidates: &mut Vec<SearchCandidate>, k: usize) {
    candidates.sort_unstable_by(crate::ingest::compare_search_candidates);
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

fn evaluate_for_plan(
    predicate: &Predicate,
    columns: &ColumnStore,
    alive: &crate::meta::AliveSet,
) -> Result<crate::meta::DocBitmap, FilteredSearchError> {
    evaluate(predicate, columns, alive).map_err(map_eval_error)
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

#[allow(clippy::expect_used, clippy::panic)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::{AliveSet, ColumnStoreBuilder, Schema};

    #[test]
    fn metadata_execution_receipt_report_mismatch_is_typed() {
        assert!(matches!(
            verify_execution_branch(SegmentBranch::MaskedScan, SegmentBranch::ExactAllowList),
            Err(FilteredSearchError::PlanReportMismatch {
                reported: SegmentBranch::MaskedScan,
                executed: SegmentBranch::ExactAllowList,
            })
        ));
    }

    #[test]
    fn metadata_execution_receipt_row_count_mismatch_uses_real_evaluator_inputs() {
        let mut builder = ColumnStoreBuilder::new(Schema::timestamp_only());
        builder.push_row(1, &[]).expect("one real metadata row");
        let columns = builder.finish().expect("real metadata columns");
        let alive = AliveSet::new(2);

        assert!(matches!(
            evaluate_for_plan(&Predicate::And(Vec::new()), &columns, &alive),
            Err(FilteredSearchError::Plan(PlanError::RowCountMismatch {
                columns: 1,
                alive: 2,
            }))
        ));
    }
}
