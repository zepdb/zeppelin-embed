//! Store-level exact filtered-vector execution.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::ingest::{RowSource, SearchCandidate, SearchRequest};
use crate::lifecycle::{
    PublishedSnapshot, QueryCancellation, QueryControl, QueryError, SearchOptions, SearchTier,
    SnapshotLease, Store, StoreError, StoreState,
};
use crate::meta::{ColumnStore, ColumnStoreBuilder, EvalError, Predicate, evaluate};
use crate::quant::{prepare_bit4_query, prepare_int8_query};
use crate::scan::{
    Int8Factors, ScanError, ScanQuery, ScanRequest as VectorScanRequest, ScanRows, ScanStats,
    gather_top_k, scan_partition,
};
use crate::segment::layout::RegionKind;

use super::{
    PlanError, PlanNode, SegmentBranch, SegmentPlan, SegmentTier, choose_scan_branch,
    segment_may_match, validate_predicate,
};

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

    let schema = match snapshot.segments().first() {
        Some(segment) => segment
            .columns()
            .map_err(StoreError::Segment)
            .map_err(QueryError::Store)?
            .schema()
            .clone(),
        None => crate::meta::Schema::timestamp_only(),
    };
    validate_predicate(predicate, &schema)?;
    if !active.is_empty() && schema.user_column_count() != 0 {
        return Err(PlanError::ActiveColumnsUnavailable.into());
    }
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
        generation,
        request,
        predicate,
        k,
        options,
        &cancellation,
    );
    drop(query_guard);
    result
}

#[allow(clippy::too_many_arguments)]
fn execute_pinned(
    snapshot: &PublishedSnapshot,
    active: &crate::ingest::ActiveSegment,
    generation: u64,
    request: SearchRequest<'_>,
    predicate: &Predicate,
    k: usize,
    options: SearchOptions,
    cancellation: &QueryCancellation<'_>,
) -> Result<FilteredSearchOutcome, FilteredSearchError> {
    let mut candidates = Vec::new();
    let mut plans = Vec::new();
    let mut stats = MutableStats::default();
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
        let columns = active_timestamp_columns(active.timestamps())?;
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
        )?;
        stats.add(local_stats)?;
        plans.push(plan);
    }

    for segment in snapshot.segments() {
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
        let branch = if graph_selected {
            SegmentBranch::GraphExactFallback
        } else {
            choose_scan_branch(allow_list.cardinality())
        };
        let plan = SegmentPlan::exact(source, tier, branch, allow_list.cardinality())
            .with_predicate(predicate);
        let row_count = segment.meta().row_count as usize;
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
        )?;
        stats.add(local_stats)?;
        plans.push(plan);
    }

    candidates.sort_unstable_by(|left, right| {
        right
            .score()
            .total_cmp(&left.score())
            .then_with(|| left.row_id().cmp(&right.row_id()))
    });
    candidates.truncate(k);
    cancellation.check_graph().map_err(map_scan_error)?;
    Ok(FilteredSearchOutcome {
        candidates,
        stats: stats.finish(),
        generation,
        plans,
    })
}

fn active_timestamp_columns(timestamps: &[i64]) -> Result<ColumnStore, FilteredSearchError> {
    let mut builder = ColumnStoreBuilder::new(crate::meta::Schema::timestamp_only());
    for timestamp in timestamps {
        builder
            .push_row(*timestamp, &[])
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
) -> Result<(), FilteredSearchError> {
    for candidate in local {
        let local_row = u32::try_from(candidate.row)
            .map_err(|_| QueryError::Scan(ScanError::ArithmeticOverflow))?;
        candidates.push(SearchCandidate::new(
            crate::ingest::GlobalRowId::new(source, local_row),
            document(candidate.row)?,
            candidate.score,
        ));
    }
    Ok(())
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
