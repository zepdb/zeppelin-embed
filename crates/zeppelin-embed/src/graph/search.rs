//! Single-core fixed-stride graph traversal.

use crate::graph::block::{CheckedNodeId, GraphNodeBlocks, GraphNodeError};
use crate::kernels::{Bit4Rows4, GatherShapeError, score_bit4_ptrs};
use crate::lifecycle::QueryCancellation;
use crate::quant::{Bit4Query, QuantError, prepare_bit4_query};
use crate::scan::ScanError;

/// One graph-query invocation.
#[derive(Clone, Copy, Debug)]
pub struct GraphSearchRequest<'a> {
    query: &'a [f32],
    k: usize,
    ef: usize,
    seed: u64,
    trace_candidates: bool,
    observed_core_class: QueryCoreClass,
    prefetch: TraversalPrefetch,
}

impl<'a> GraphSearchRequest<'a> {
    /// Creates a deterministic single-segment traversal request.
    #[must_use]
    pub const fn new(query: &'a [f32], k: usize, ef: usize, seed: u64) -> Self {
        Self {
            query,
            k,
            ef,
            seed,
            trace_candidates: false,
            observed_core_class: QueryCoreClass::Unverified,
            prefetch: TraversalPrefetch::Enabled,
        }
    }

    /// Records the logical Bit4 candidate sequence for deterministic diagnostics.
    #[must_use]
    pub const fn with_candidate_trace(mut self) -> Self {
        self.trace_candidates = true;
        self
    }

    /// Attaches a caller-observed core class without changing scheduling policy.
    #[must_use]
    pub const fn with_observed_core_class(mut self, core_class: QueryCoreClass) -> Self {
        self.observed_core_class = core_class;
        self
    }

    /// Selects whether traversal and rescore issue software prefetch hints.
    #[must_use]
    pub const fn with_prefetch(mut self, prefetch: TraversalPrefetch) -> Self {
        self.prefetch = prefetch;
        self
    }
}

/// Runtime-selectable software-prefetch arm for traversal A/B measurements.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TraversalPrefetch {
    /// Issue graph-node and f32-rescore prefetch hints.
    Enabled,
    /// Execute the identical traversal without software prefetch hints.
    Disabled,
}

impl TraversalPrefetch {
    const fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }
}

/// One exact-rescored row returned by graph traversal.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GraphSearchCandidate {
    row_id: u32,
    distance: f64,
}

impl GraphSearchCandidate {
    /// Returns the dense segment-local row id.
    #[must_use]
    pub const fn row_id(self) -> u32 {
        self.row_id
    }

    /// Returns the exact squared-L2 distance from the f32 rescore row.
    #[must_use]
    pub const fn distance(self) -> f64 {
        self.distance
    }
}

/// Observed caller QoS class for latency attribution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryQosClass {
    /// Darwin user-interactive QoS.
    UserInteractive,
    /// Darwin user-initiated QoS.
    UserInitiated,
    /// Darwin default QoS.
    Default,
    /// Darwin utility QoS.
    Utility,
    /// Darwin background QoS.
    Background,
    /// Darwin reported no explicit QoS.
    Unspecified,
    /// The platform cannot report Darwin QoS.
    Unavailable,
    /// Darwin returned an unrecognized class value.
    Unknown,
}

/// Core class observed by an external fail-closed residency canary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryCoreClass {
    /// A performance-core canary passed immediately before the query run.
    Performance,
    /// An efficiency-core canary identified an invalid latency run.
    Efficiency,
    /// No external core-residency observation was supplied.
    Unverified,
}

/// Deterministic work counters for one completed query.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphSearchCounters {
    hops: usize,
    candidates_scored: usize,
    pushes: usize,
    visited: usize,
    visited_epoch_cleared: bool,
    qos_class: QueryQosClass,
    qos_relative_priority: i32,
    core_class: QueryCoreClass,
}

/// Scheduling-independent counters suitable for deterministic comparisons.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeterministicGraphWork {
    /// Expanded frontier nodes.
    pub hops: usize,
    /// Distinct Bit4 candidates scored.
    pub candidates_scored: usize,
    /// Successful frontier insertions.
    pub pushes: usize,
    /// Distinct visited nodes.
    pub visited: usize,
}

impl GraphSearchCounters {
    /// Returns the work tuple without caller scheduling attribution.
    #[must_use]
    pub const fn deterministic_work(self) -> DeterministicGraphWork {
        DeterministicGraphWork {
            hops: self.hops,
            candidates_scored: self.candidates_scored,
            pushes: self.pushes,
            visited: self.visited,
        }
    }

    /// Returns expanded frontier nodes.
    #[must_use]
    pub const fn hops(self) -> usize {
        self.hops
    }

    /// Returns distinct nodes scored by the Bit4 estimator.
    #[must_use]
    pub const fn candidates_scored(self) -> usize {
        self.candidates_scored
    }

    /// Returns successful frontier insertions.
    #[must_use]
    pub const fn pushes(self) -> usize {
        self.pushes
    }

    /// Returns distinct nodes marked visited.
    #[must_use]
    pub const fn visited(self) -> usize {
        self.visited
    }

    /// Returns whether this query paid the once-per-255-query visited memset.
    #[must_use]
    pub const fn visited_epoch_cleared(self) -> bool {
        self.visited_epoch_cleared
    }

    /// Returns the caller thread's observed QoS class.
    #[must_use]
    pub const fn qos_class(self) -> QueryQosClass {
        self.qos_class
    }

    /// Returns Darwin's observed relative QoS priority, or zero when unavailable.
    #[must_use]
    pub const fn qos_relative_priority(self) -> i32 {
        self.qos_relative_priority
    }

    /// Returns the externally observed core class for this query.
    #[must_use]
    pub const fn core_class(self) -> QueryCoreClass {
        self.core_class
    }
}

/// Exact results plus deterministic traversal evidence.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphSearchResult {
    candidates: Vec<GraphSearchCandidate>,
    counters: GraphSearchCounters,
    candidate_sequence: Option<Vec<u32>>,
}

impl GraphSearchResult {
    /// Returns exact-rescored candidates in best-first order.
    #[must_use]
    pub fn candidates(&self) -> &[GraphSearchCandidate] {
        &self.candidates
    }

    /// Returns deterministic traversal work and caller scheduling attribution.
    #[must_use]
    pub const fn counters(&self) -> GraphSearchCounters {
        self.counters
    }

    /// Returns the logical scored-row sequence when requested.
    #[must_use]
    pub fn candidate_sequence(&self) -> Option<&[u32]> {
        self.candidate_sequence.as_deref()
    }

    /// Counts returned row ids present in an independently supplied truth set.
    #[must_use]
    pub fn recall_against(&self, truth: &[u32]) -> usize {
        self.candidates
            .iter()
            .filter(|candidate| truth.contains(&candidate.row_id))
            .count()
    }
}

/// Typed graph traversal rejection.
#[derive(Clone, Debug, PartialEq)]
pub enum GraphSearchError {
    /// Request shape or graph/rescore geometry is invalid.
    Geometry(String),
    /// A validated node became unavailable through the fixed-stride view.
    Graph(GraphNodeError),
    /// Query preparation or Bit4 scoring rejected the input.
    Quant(QuantError),
    /// The four-row gather boundary rejected a graph/query shape mismatch.
    Gather(GatherShapeError),
    /// Unfiltered traversal crossed the Task-16 collapse guard; M6 owns fallback.
    VisitedCapExceeded {
        /// Distinct nodes marked before traversal stopped.
        visited: usize,
        /// Fail-closed guard `4 * ef * R`.
        cap: usize,
    },
    /// The caller cancelled traversal; partial answers are never returned.
    Cancelled {
        /// Permanently false.
        partial: bool,
    },
    /// The query deadline expired; partial answers are never returned.
    Timeout {
        /// Permanently false.
        partial: bool,
    },
    /// Store close cancelled the admitted read; partial answers are never returned.
    ReadCancelled {
        /// Permanently false.
        partial: bool,
    },
    /// An unexpected scan-layer cancellation check failed.
    Scan(ScanError),
}

impl std::fmt::Display for GraphSearchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Geometry(detail) => {
                write!(formatter, "graph search geometry is invalid: {detail}")
            }
            Self::Graph(error) => error.fmt(formatter),
            Self::Quant(error) => error.fmt(formatter),
            Self::Gather(error) => error.fmt(formatter),
            Self::VisitedCapExceeded { visited, cap } => write!(
                formatter,
                "graph search visited {visited} nodes, exceeding the fail-closed cap {cap}"
            ),
            Self::Cancelled { partial } => {
                write!(formatter, "graph search was cancelled (partial={partial})")
            }
            Self::Timeout { partial } => {
                write!(
                    formatter,
                    "graph search deadline expired (partial={partial})"
                )
            }
            Self::ReadCancelled { partial } => write!(
                formatter,
                "store close cancelled graph search (partial={partial})"
            ),
            Self::Scan(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for GraphSearchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Graph(error) => Some(error),
            Self::Quant(error) => Some(error),
            Self::Gather(error) => Some(error),
            Self::Scan(error) => Some(error),
            Self::Geometry(_)
            | Self::VisitedCapExceeded { .. }
            | Self::Cancelled { .. }
            | Self::Timeout { .. }
            | Self::ReadCancelled { .. } => None,
        }
    }
}

impl From<GraphNodeError> for GraphSearchError {
    fn from(error: GraphNodeError) -> Self {
        Self::Graph(error)
    }
}

impl From<QuantError> for GraphSearchError {
    fn from(error: QuantError) -> Self {
        Self::Quant(error)
    }
}

impl From<GatherShapeError> for GraphSearchError {
    fn from(error: GatherShapeError) -> Self {
        Self::Gather(error)
    }
}

#[derive(Clone, Copy, Debug)]
struct ScoredNode {
    row_id: CheckedNodeId,
    distance: f64,
}

/// Reusable, allocation-stable state for single-core queries over one segment.
#[derive(Debug)]
pub struct GraphSearcher<'a> {
    graph: GraphNodeBlocks<'a>,
    rescore: &'a [f32],
    entries: [CheckedNodeId; 4],
    visited: Vec<u8>,
    epoch: u8,
    pool: Vec<ScoredNode>,
    frontier: Vec<ScoredNode>,
    hop_candidates: Vec<CheckedNodeId>,
}

impl<'a> GraphSearcher<'a> {
    /// Validates fixed graph/rescore geometry and discovers the four persisted entry seeds.
    pub fn new(graph: GraphNodeBlocks<'a>, rescore: &'a [f32]) -> Result<Self, GraphSearchError> {
        let dimensions = graph.layout().dims() as usize;
        let expected_rescore = (graph.node_count() as usize)
            .checked_mul(dimensions)
            .ok_or_else(|| GraphSearchError::Geometry("rescore length overflow".to_owned()))?;
        if rescore.len() != expected_rescore {
            return Err(GraphSearchError::Geometry(format!(
                "rescore has {} values, expected {expected_rescore}",
                rescore.len()
            )));
        }
        let mut entries = Vec::with_capacity(4);
        for row_id in 0..graph.node_count() {
            if graph.block(row_id)?.flags() & 1 != 0 {
                entries.push(graph.checked_node_id(row_id)?);
            }
        }
        let entry_count = entries.len();
        let entries = entries.try_into().map_err(|_: Vec<CheckedNodeId>| {
            GraphSearchError::Geometry(format!(
                "graph contains {entry_count} persisted entry seeds, expected medoid plus three refined seeds"
            ))
        })?;
        let mut hop_candidates = Vec::new();
        hop_candidates
            .try_reserve_exact(usize::from(graph.layout().max_degree()))
            .map_err(|error| {
                GraphSearchError::Geometry(format!("hop-candidate allocation failed: {error}"))
            })?;
        Ok(Self {
            graph,
            rescore,
            entries,
            visited: vec![0_u8; graph.node_count() as usize],
            epoch: 0,
            pool: Vec::new(),
            frontier: Vec::new(),
            hop_candidates,
        })
    }

    /// Traverses, then exact-rescores the complete retained pool.
    pub fn search(
        &mut self,
        request: GraphSearchRequest<'_>,
        cancellation: Option<&QueryCancellation<'_>>,
    ) -> Result<GraphSearchResult, GraphSearchError> {
        self.search_inner(request, cancellation)
    }

    fn search_inner(
        &mut self,
        request: GraphSearchRequest<'_>,
        cancellation: Option<&QueryCancellation<'_>>,
    ) -> Result<GraphSearchResult, GraphSearchError> {
        self.validate_request(request)?;
        check_cancellation(cancellation)?;
        let (qos_class, qos_relative_priority) = observed_qos();
        let padded_dimensions = self.graph.layout().padded_dims() as usize;
        let padded_query = if padded_dimensions == request.query.len() {
            None
        } else {
            let mut values = Vec::with_capacity(padded_dimensions);
            values.extend_from_slice(request.query);
            values.resize(padded_dimensions, 0.0);
            Some(values)
        };
        let prepared = prepare_bit4_query(
            padded_query.as_deref().unwrap_or(request.query),
            request.seed,
        )?;
        let query_norm = squared_norm(request.query);
        let visited_cap = request
            .ef
            .checked_mul(usize::from(self.graph.layout().max_degree()))
            .and_then(|value| value.checked_mul(4))
            .ok_or_else(|| GraphSearchError::Geometry("visited cap overflow".to_owned()))?;
        let visited_epoch_cleared = self.next_epoch();
        self.pool.clear();
        self.frontier.clear();
        if self.pool.capacity() < request.ef {
            self.pool.try_reserve(request.ef).map_err(|error| {
                GraphSearchError::Geometry(format!("pool allocation failed: {error}"))
            })?;
        }
        if self.frontier.capacity() < request.ef {
            self.frontier.try_reserve(request.ef).map_err(|error| {
                GraphSearchError::Geometry(format!("frontier allocation failed: {error}"))
            })?;
        }
        let mut counters = GraphSearchCounters {
            hops: 0,
            candidates_scored: 0,
            pushes: 0,
            visited: 0,
            visited_epoch_cleared,
            qos_class,
            qos_relative_priority,
            core_class: request.observed_core_class,
        };
        let mut candidate_sequence = request
            .trace_candidates
            .then(|| Vec::with_capacity(request.ef));
        let entries = self.entries;
        if request.prefetch.is_enabled() {
            for entry in &entries {
                self.graph.prefetch_line0(*entry);
            }
        }
        let mut seed_group = [None; 4];
        let mut seed_count = 0_usize;
        for entry in entries {
            if mark_visited(&mut self.visited, self.epoch, entry)? {
                counters.visited += 1;
                ensure_visited_cap(counters.visited, visited_cap)?;
                let Some(slot) = seed_group.get_mut(seed_count) else {
                    return Err(GraphSearchError::Geometry(
                        "entry seed count exceeds fixed seed group".to_owned(),
                    ));
                };
                *slot = Some(entry);
                seed_count += 1;
            }
        }
        self.score_and_push_group(
            &prepared,
            query_norm,
            &seed_group,
            seed_count,
            request.ef,
            request.prefetch,
            &mut counters,
            &mut candidate_sequence,
        )?;
        loop {
            check_cancellation(cancellation)?;
            let Some(candidate) = min_heap_pop(&mut self.frontier)? else {
                break;
            };
            if self.pool.len() >= request.ef
                && self
                    .pool
                    .first()
                    .is_some_and(|worst| candidate.distance > worst.distance)
            {
                break;
            }
            counters.hops += 1;
            let (degree, neighbors) = self.graph.adjacency_checked(candidate.row_id)?;
            let degree = usize::from(degree);
            self.hop_candidates.clear();
            for raw_neighbor in neighbors.take(degree) {
                let neighbor = self.graph.checked_node_id(raw_neighbor)?;
                if !mark_visited(&mut self.visited, self.epoch, neighbor)? {
                    continue;
                }
                counters.visited += 1;
                ensure_visited_cap(counters.visited, visited_cap)?;
                if request.prefetch.is_enabled() {
                    self.graph.prefetch_line0(neighbor);
                }
                self.hop_candidates.push(neighbor);
            }
            let mut group_start = 0_usize;
            let hop_candidate_count = self.hop_candidates.len();
            while group_start < hop_candidate_count {
                check_cancellation(cancellation)?;
                let group_count = (hop_candidate_count - group_start).min(4);
                let mut group = [None; 4];
                for lane in 0..group_count {
                    let source = self.hop_candidates.get(group_start + lane).copied();
                    let Some(destination) = group.get_mut(lane) else {
                        return Err(GraphSearchError::Geometry(
                            "candidate gather lane is unavailable".to_owned(),
                        ));
                    };
                    *destination = source;
                }
                self.score_and_push_group(
                    &prepared,
                    query_norm,
                    &group,
                    group_count,
                    request.ef,
                    request.prefetch,
                    &mut counters,
                    &mut candidate_sequence,
                )?;
                group_start += group_count;
            }
        }
        let candidates = exact_rescore(
            &self.pool,
            self.rescore,
            request.query,
            self.graph.layout().dims() as usize,
            request.k,
            cancellation,
            request.prefetch,
        )?;
        Ok(GraphSearchResult {
            candidates,
            counters,
            candidate_sequence,
        })
    }

    fn validate_request(&self, request: GraphSearchRequest<'_>) -> Result<(), GraphSearchError> {
        let dimensions = self.graph.layout().dims() as usize;
        if request.query.len() != dimensions {
            return Err(GraphSearchError::Geometry(format!(
                "query has {} dimensions, expected {dimensions}",
                request.query.len()
            )));
        }
        if request.k == 0 || request.k > request.ef {
            return Err(GraphSearchError::Geometry(format!(
                "k {} must be positive and no greater than ef {}",
                request.k, request.ef
            )));
        }
        if request.ef > self.graph.node_count() as usize {
            return Err(GraphSearchError::Geometry(format!(
                "ef {} exceeds graph row count {}",
                request.ef,
                self.graph.node_count()
            )));
        }
        Ok(())
    }

    fn next_epoch(&mut self) -> bool {
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            self.visited.fill(0);
            self.epoch = 1;
            return true;
        }
        false
    }

    #[allow(clippy::too_many_arguments)]
    fn score_and_push_group(
        &mut self,
        query: &Bit4Query,
        query_norm: f64,
        row_ids: &[Option<CheckedNodeId>; 4],
        row_count: usize,
        ef: usize,
        prefetch: TraversalPrefetch,
        counters: &mut GraphSearchCounters,
        candidate_sequence: &mut Option<Vec<u32>>,
    ) -> Result<(), GraphSearchError> {
        let scored = score_group(&self.graph, query, query_norm, row_ids, row_count)?;
        counters.candidates_scored = counters
            .candidates_scored
            .checked_add(row_count)
            .ok_or_else(|| GraphSearchError::Geometry("candidate counter overflow".to_owned()))?;
        for candidate in scored.into_iter().flatten() {
            if let Some(sequence) = candidate_sequence.as_mut() {
                sequence.push(candidate.row_id.raw());
            }
            if max_heap_would_keep(&self.pool, candidate, ef) {
                max_heap_insert_bounded(&mut self.pool, candidate, ef)?;
                min_heap_push(&mut self.frontier, candidate)?;
                counters.pushes = counters.pushes.checked_add(1).ok_or_else(|| {
                    GraphSearchError::Geometry("push counter overflow".to_owned())
                })?;
                if prefetch.is_enabled()
                    && let Some(head) = self.frontier.first()
                {
                    self.graph.prefetch_head_block(head.row_id);
                }
            }
        }
        Ok(())
    }
}

fn mark_visited(
    visited: &mut [u8],
    epoch: u8,
    row_id: CheckedNodeId,
) -> Result<bool, GraphSearchError> {
    let marker = visited.get_mut(row_id.raw() as usize).ok_or_else(|| {
        GraphSearchError::Geometry(format!("row id {} is unavailable", row_id.raw()))
    })?;
    if *marker == epoch {
        return Ok(false);
    }
    *marker = epoch;
    Ok(true)
}

fn ensure_visited_cap(visited: usize, cap: usize) -> Result<(), GraphSearchError> {
    if visited > cap {
        return Err(GraphSearchError::VisitedCapExceeded { visited, cap });
    }
    Ok(())
}

fn check_cancellation(
    cancellation: Option<&QueryCancellation<'_>>,
) -> Result<(), GraphSearchError> {
    let Some(cancellation) = cancellation else {
        return Ok(());
    };
    match cancellation.check() {
        Ok(()) => Ok(()),
        Err(ScanError::Cancelled { partial }) => Err(GraphSearchError::Cancelled { partial }),
        Err(ScanError::Timeout { partial }) => Err(GraphSearchError::Timeout { partial }),
        Err(ScanError::ReadCancelled { partial }) => {
            Err(GraphSearchError::ReadCancelled { partial })
        }
        Err(error) => Err(GraphSearchError::Scan(error)),
    }
}

fn scored_best_first(left: &ScoredNode, right: &ScoredNode) -> std::cmp::Ordering {
    left.distance
        .total_cmp(&right.distance)
        .then_with(|| left.row_id.raw().cmp(&right.row_id.raw()))
}

fn score_group(
    graph: &GraphNodeBlocks<'_>,
    query: &Bit4Query,
    query_norm: f64,
    row_ids: &[Option<CheckedNodeId>; 4],
    row_count: usize,
) -> Result<[Option<ScoredNode>; 4], GraphSearchError> {
    let first_id = row_ids
        .first()
        .copied()
        .flatten()
        .ok_or_else(|| GraphSearchError::Geometry("empty gather group".to_owned()))?;
    let (first_row, first_factors) = graph.score_row_checked(first_id)?;
    let mut rows = [first_row; 4];
    let mut factors = [first_factors; 4];
    for lane in 1..row_count {
        let row_id =
            row_ids.get(lane).copied().flatten().ok_or_else(|| {
                GraphSearchError::Geometry(format!("gather lane {lane} is empty"))
            })?;
        let (row, factors_for_row) = graph.score_row_checked(row_id)?;
        let row_slot = rows
            .get_mut(lane)
            .ok_or_else(|| GraphSearchError::Geometry("gather row lane overflow".to_owned()))?;
        *row_slot = row;
        let factor_slot = factors
            .get_mut(lane)
            .ok_or_else(|| GraphSearchError::Geometry("gather factor lane overflow".to_owned()))?;
        *factor_slot = factors_for_row;
    }
    let gathered = Bit4Rows4::from_rows(rows, graph.layout().code_bytes())?;
    let (query_codes, query_sum, query_scale_half) = query.kernel_parts();
    let mut dots = [0.0_f32; 4];
    score_bit4_ptrs(
        query_codes,
        query_sum,
        query_scale_half,
        &gathered,
        &factors,
        &mut dots,
    )?;
    let mut scored = [None; 4];
    for lane in 0..row_count {
        let row_id =
            row_ids.get(lane).copied().flatten().ok_or_else(|| {
                GraphSearchError::Geometry(format!("scored lane {lane} is empty"))
            })?;
        let dot = f64::from(
            *dots
                .get(lane)
                .ok_or_else(|| GraphSearchError::Geometry("dot lane overflow".to_owned()))?,
        );
        let norm = factors.get(lane).copied().unwrap_or(first_factors).norm();
        let destination = scored
            .get_mut(lane)
            .ok_or_else(|| GraphSearchError::Geometry("score lane overflow".to_owned()))?;
        *destination = Some(ScoredNode {
            row_id,
            distance: (query_norm + norm * norm - 2.0 * dot).max(0.0),
        });
    }
    Ok(scored)
}

fn max_heap_would_keep(pool: &[ScoredNode], node: ScoredNode, limit: usize) -> bool {
    pool.len() < limit
        || pool
            .first()
            .is_some_and(|worst| scored_best_first(&node, worst).is_lt())
}

fn max_heap_insert_bounded(
    pool: &mut Vec<ScoredNode>,
    node: ScoredNode,
    limit: usize,
) -> Result<(), GraphSearchError> {
    if limit == 0 {
        return Err(GraphSearchError::Geometry(
            "bounded pool capacity is zero".to_owned(),
        ));
    }
    if pool.len() < limit {
        pool.push(node);
        let child = pool.len().saturating_sub(1);
        max_heap_sift_up(pool, child)?;
        return Ok(());
    }
    let root = pool
        .first_mut()
        .ok_or_else(|| GraphSearchError::Geometry("full pool has no root".to_owned()))?;
    *root = node;
    max_heap_sift_down(pool, 0)
}

fn min_heap_push(frontier: &mut Vec<ScoredNode>, node: ScoredNode) -> Result<(), GraphSearchError> {
    frontier.push(node);
    let child = frontier.len().saturating_sub(1);
    min_heap_sift_up(frontier, child)
}

fn min_heap_pop(frontier: &mut Vec<ScoredNode>) -> Result<Option<ScoredNode>, GraphSearchError> {
    let Some(last) = frontier.pop() else {
        return Ok(None);
    };
    let Some(root) = frontier.first_mut() else {
        return Ok(Some(last));
    };
    let result = *root;
    *root = last;
    min_heap_sift_down(frontier, 0)?;
    Ok(Some(result))
}

fn min_heap_sift_up(heap: &mut [ScoredNode], mut child: usize) -> Result<(), GraphSearchError> {
    let start = child;
    let moving = heap_node(heap, child)?;
    while child != 0 {
        let parent = child.saturating_sub(1) / 2;
        let parent_node = heap_node(heap, parent)?;
        if !scored_best_first(&moving, &parent_node).is_lt() {
            break;
        }
        heap_store(heap, child, parent_node)?;
        child = parent;
    }
    if child != start {
        heap_store(heap, child, moving)?;
    }
    Ok(())
}

fn max_heap_sift_up(heap: &mut [ScoredNode], mut child: usize) -> Result<(), GraphSearchError> {
    let start = child;
    let moving = heap_node(heap, child)?;
    while child != 0 {
        let parent = child.saturating_sub(1) / 2;
        let parent_node = heap_node(heap, parent)?;
        if !scored_best_first(&moving, &parent_node).is_gt() {
            break;
        }
        heap_store(heap, child, parent_node)?;
        child = parent;
    }
    if child != start {
        heap_store(heap, child, moving)?;
    }
    Ok(())
}

fn min_heap_sift_down(heap: &mut [ScoredNode], mut parent: usize) -> Result<(), GraphSearchError> {
    let start = parent;
    let moving = heap_node(heap, parent)?;
    loop {
        let left = parent
            .checked_mul(2)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| GraphSearchError::Geometry("min-heap child overflow".to_owned()))?;
        if left >= heap.len() {
            break;
        }
        let right = left.saturating_add(1);
        let best_child = if right < heap.len()
            && scored_best_first(&heap_node(heap, right)?, &heap_node(heap, left)?).is_lt()
        {
            right
        } else {
            left
        };
        let child_node = heap_node(heap, best_child)?;
        if !scored_best_first(&child_node, &moving).is_lt() {
            break;
        }
        heap_store(heap, parent, child_node)?;
        parent = best_child;
    }
    if parent != start {
        heap_store(heap, parent, moving)?;
    }
    Ok(())
}

fn max_heap_sift_down(heap: &mut [ScoredNode], mut parent: usize) -> Result<(), GraphSearchError> {
    let start = parent;
    let moving = heap_node(heap, parent)?;
    loop {
        let left = parent
            .checked_mul(2)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| GraphSearchError::Geometry("max-heap child overflow".to_owned()))?;
        if left >= heap.len() {
            break;
        }
        let right = left.saturating_add(1);
        let worst_child = if right < heap.len()
            && scored_best_first(&heap_node(heap, right)?, &heap_node(heap, left)?).is_gt()
        {
            right
        } else {
            left
        };
        let child_node = heap_node(heap, worst_child)?;
        if !scored_best_first(&child_node, &moving).is_gt() {
            break;
        }
        heap_store(heap, parent, child_node)?;
        parent = worst_child;
    }
    if parent != start {
        heap_store(heap, parent, moving)?;
    }
    Ok(())
}

fn heap_node(heap: &[ScoredNode], index: usize) -> Result<ScoredNode, GraphSearchError> {
    heap.get(index)
        .copied()
        .ok_or_else(|| GraphSearchError::Geometry(format!("heap slot {index} is unavailable")))
}

fn heap_store(
    heap: &mut [ScoredNode],
    index: usize,
    value: ScoredNode,
) -> Result<(), GraphSearchError> {
    let slot = heap
        .get_mut(index)
        .ok_or_else(|| GraphSearchError::Geometry(format!("heap slot {index} is unavailable")))?;
    *slot = value;
    Ok(())
}

fn exact_rescore(
    pool: &[ScoredNode],
    base: &[f32],
    query: &[f32],
    dimensions: usize,
    k: usize,
    cancellation: Option<&QueryCancellation<'_>>,
    prefetch: TraversalPrefetch,
) -> Result<Vec<GraphSearchCandidate>, GraphSearchError> {
    let mut exact = Vec::with_capacity(pool.len());
    for (index, candidate) in pool.iter().enumerate() {
        if index.is_multiple_of(4) {
            check_cancellation(cancellation)?;
        }
        if prefetch.is_enabled()
            && let Some(ahead) = pool.get(index.saturating_add(4))
        {
            prefetch_f32_row(base, ahead.row_id.raw(), dimensions);
        }
        let start = (candidate.row_id.raw() as usize)
            .checked_mul(dimensions)
            .ok_or_else(|| GraphSearchError::Geometry("rescore offset overflow".to_owned()))?;
        let end = start
            .checked_add(dimensions)
            .ok_or_else(|| GraphSearchError::Geometry("rescore end overflow".to_owned()))?;
        let row = base.get(start..end).ok_or_else(|| {
            GraphSearchError::Geometry(format!(
                "rescore row {} is unavailable",
                candidate.row_id.raw()
            ))
        })?;
        let distance = row
            .iter()
            .zip(query)
            .map(|(left, right)| {
                let delta = f64::from(*left) - f64::from(*right);
                delta * delta
            })
            .sum();
        exact.push(GraphSearchCandidate {
            row_id: candidate.row_id.raw(),
            distance,
        });
    }
    if exact.len() > k {
        let _ = exact.select_nth_unstable_by(k, exact_best_first);
        exact.truncate(k);
    }
    exact.sort_unstable_by(exact_best_first);
    Ok(exact)
}

fn exact_best_first(
    left: &GraphSearchCandidate,
    right: &GraphSearchCandidate,
) -> std::cmp::Ordering {
    left.distance
        .total_cmp(&right.distance)
        .then_with(|| left.row_id.cmp(&right.row_id))
}

fn prefetch_f32_row(base: &[f32], row_id: u32, dimensions: usize) {
    let Some(start) = (row_id as usize).checked_mul(dimensions) else {
        return;
    };
    let Some(value) = base.get(start) else {
        return;
    };
    let address = std::ptr::from_ref(value);
    #[cfg(target_arch = "aarch64")]
    // SAFETY: the address points into the live immutable f32 rescore mapping.
    unsafe {
        std::arch::asm!(
            "prfm pldl1keep, [{address}]",
            address = in(reg) address,
            options(readonly, nostack)
        );
    }
    #[cfg(not(target_arch = "aarch64"))]
    let _ = address;
}

fn squared_norm(values: &[f32]) -> f64 {
    values
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum()
}

#[cfg(target_vendor = "apple")]
fn observed_qos() -> (QueryQosClass, i32) {
    let mut class = libc::qos_class_t::QOS_CLASS_UNSPECIFIED;
    let mut priority = 0_i32;
    // SAFETY: pthread_self returns the current live thread and both output pointers are valid.
    let status =
        unsafe { libc::pthread_get_qos_class_np(libc::pthread_self(), &mut class, &mut priority) };
    if status != 0 {
        return (QueryQosClass::Unknown, 0);
    }
    let class = match class {
        libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE => QueryQosClass::UserInteractive,
        libc::qos_class_t::QOS_CLASS_USER_INITIATED => QueryQosClass::UserInitiated,
        libc::qos_class_t::QOS_CLASS_DEFAULT => QueryQosClass::Default,
        libc::qos_class_t::QOS_CLASS_UTILITY => QueryQosClass::Utility,
        libc::qos_class_t::QOS_CLASS_BACKGROUND => QueryQosClass::Background,
        libc::qos_class_t::QOS_CLASS_UNSPECIFIED => QueryQosClass::Unspecified,
    };
    (class, priority)
}

#[cfg(not(target_vendor = "apple"))]
const fn observed_qos() -> (QueryQosClass, i32) {
    (QueryQosClass::Unavailable, 0)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use std::time::{Duration, Instant};

    use proptest::prelude::*;
    use proptest::test_runner::{Config, RngSeed, TestRunner};
    use rand::RngCore;

    use super::{GraphSearchRequest, GraphSearcher, TraversalPrefetch};
    use crate::graph::block::{
        GraphNodeBlockBuild, GraphNodeBlockInput, GraphNodeLayout, decode_node_blocks,
        encode_node_blocks,
    };
    use crate::lifecycle::{CancelToken, OpenOptions, QueryCancellation, QueryControl, Store};
    use crate::quant::{Bit4Factors, quantize_bit4};

    const DIMS: usize = 128;

    #[test]
    fn traversal_returns_the_same_topk_as_brute_force_at_high_ef() {
        let (encoded, rescore) = complete_graph_fixture(12);
        let graph = decode_node_blocks(encoded.as_bytes()).expect("fixture graph is valid");
        let query = vector(4.25);
        let expected = brute_force_topk(&rescore, &query, 5);
        let mut searcher = GraphSearcher::new(graph, &rescore).expect("fixture geometry is valid");

        let result = searcher
            .search(GraphSearchRequest::new(&query, 5, 12, 0x19_04), None)
            .expect("high-ef traversal succeeds");
        let actual = result
            .candidates()
            .iter()
            .map(|candidate| candidate.row_id())
            .collect::<Vec<_>>();

        assert_eq!(actual, expected);
    }

    #[test]
    fn visited_epoch_wraps_correctly_at_255_queries() {
        let (encoded, rescore) = complete_graph_fixture(12);
        let graph = decode_node_blocks(encoded.as_bytes()).expect("fixture graph is valid");
        let query = vector(4.25);
        let mut searcher = GraphSearcher::new(graph, &rescore).expect("fixture geometry is valid");

        for query_index in 0..255 {
            let result = searcher
                .search(GraphSearchRequest::new(&query, 5, 12, query_index), None)
                .expect("epoch before wrap succeeds");
            assert!(!result.counters().visited_epoch_cleared());
        }
        let wrapped = searcher
            .search(GraphSearchRequest::new(&query, 5, 12, 255), None)
            .expect("first query after 255 epochs succeeds");

        assert!(wrapped.counters().visited_epoch_cleared());
        assert_eq!(
            wrapped
                .candidates()
                .iter()
                .map(|candidate| candidate.row_id())
                .collect::<Vec<_>>(),
            brute_force_topk(&rescore, &query, 5)
        );
    }

    #[test]
    fn no_node_is_scored_twice_within_one_query() {
        let (encoded, rescore) = complete_graph_fixture(12);
        let graph = decode_node_blocks(encoded.as_bytes()).expect("fixture graph is valid");
        let query = vector(4.25);
        let mut searcher = GraphSearcher::new(graph, &rescore).expect("fixture geometry is valid");

        let result = searcher
            .search(
                GraphSearchRequest::new(&query, 5, 12, 0x19_04).with_candidate_trace(),
                None,
            )
            .expect("traced traversal succeeds");
        let sequence = result
            .candidate_sequence()
            .expect("candidate tracing was requested");
        let mut seen = [false; 12];
        for row_id in sequence {
            let slot = &mut seen[*row_id as usize];
            assert!(!*slot, "row {row_id} was scored twice");
            *slot = true;
        }

        assert_eq!(sequence.len(), result.counters().candidates_scored());
        assert_eq!(sequence.len(), result.counters().visited());
        assert_eq!(sequence, &(0_u32..12).collect::<Vec<_>>());
    }

    #[test]
    fn traversal_is_deterministic_under_seed() {
        let (encoded, rescore) = complete_graph_fixture(12);
        let graph = decode_node_blocks(encoded.as_bytes()).expect("fixture graph is valid");
        let query = vector(4.25);
        let request = GraphSearchRequest::new(&query, 5, 9, 0x19_0400).with_candidate_trace();
        let mut first = GraphSearcher::new(graph, &rescore).expect("fixture geometry is valid");
        let mut second = GraphSearcher::new(graph, &rescore).expect("fixture geometry is valid");

        let first_result = first
            .search(request, None)
            .expect("first traversal succeeds");
        let second_result = second
            .search(request, None)
            .expect("second traversal succeeds");

        assert_eq!(
            first_result.candidate_sequence(),
            second_result.candidate_sequence()
        );
        assert_eq!(
            first_result.counters().deterministic_work(),
            second_result.counters().deterministic_work()
        );
        assert_eq!(first_result.candidates(), second_result.candidates());
    }

    #[test]
    fn prefetch_switch_preserves_results_work_and_candidate_sequence() {
        let (encoded, rescore) = complete_graph_fixture(12);
        let graph = decode_node_blocks(encoded.as_bytes()).expect("fixture graph is valid");
        let query = vector(4.25);
        let mut enabled = GraphSearcher::new(graph, &rescore).expect("enabled searcher");
        let mut disabled = GraphSearcher::new(graph, &rescore).expect("disabled searcher");
        let enabled_result = enabled
            .search(
                GraphSearchRequest::new(&query, 5, 9, 0x19_0400).with_candidate_trace(),
                None,
            )
            .expect("prefetch-enabled traversal succeeds");
        let disabled_result = disabled
            .search(
                GraphSearchRequest::new(&query, 5, 9, 0x19_0400)
                    .with_candidate_trace()
                    .with_prefetch(TraversalPrefetch::Disabled),
                None,
            )
            .expect("prefetch-disabled traversal succeeds");

        assert_eq!(
            enabled_result.candidate_sequence(),
            disabled_result.candidate_sequence()
        );
        assert_eq!(
            enabled_result.counters().deterministic_work(),
            disabled_result.counters().deterministic_work()
        );
        assert_eq!(enabled_result.candidates(), disabled_result.candidates());
    }

    #[test]
    fn visited_cap_returns_a_typed_error_not_a_wrong_answer() {
        let (encoded, rescore) = chain_graph_fixture(10);
        let graph = decode_node_blocks(encoded.as_bytes()).expect("fixture graph is valid");
        let query = vector(9.0);
        let mut searcher = GraphSearcher::new(graph, &rescore).expect("fixture geometry is valid");

        let error = searcher
            .search(GraphSearchRequest::new(&query, 1, 1, 0x19_0405), None)
            .expect_err("the visited guard must fail closed");

        assert_eq!(
            error,
            super::GraphSearchError::VisitedCapExceeded { visited: 5, cap: 4 }
        );
    }

    #[test]
    fn cancellation_interrupts_a_traversal_promptly() {
        let (encoded, rescore) = long_chain_graph_fixture(40_000);
        let graph = decode_node_blocks(encoded.as_bytes()).expect("fixture graph is valid");
        let query = vec![0.0_f32; DIMS];
        let mut searcher = GraphSearcher::new(graph, &rescore).expect("fixture geometry is valid");
        let directory = tempfile::tempdir().expect("temporary store directory");
        let store = Store::open(directory.path(), OpenOptions::default()).expect("store opens");
        let lease = store.snapshot().expect("snapshot lease");
        let token = CancelToken::new();
        let control = QueryControl::Cancel(token.clone());
        let cancellation = QueryCancellation::new(&control, &lease);
        let started = Instant::now();
        let error = std::thread::scope(|scope| {
            let canceller = scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(2));
                token.cancel();
            });
            let result = searcher.search(
                GraphSearchRequest::new(&query, 1, 200, 0x19_0406),
                Some(&cancellation),
            );
            canceller.join().expect("canceller thread joins");
            result
        })
        .expect_err("in-flight traversal must stop");
        let elapsed = started.elapsed();

        assert_eq!(error, super::GraphSearchError::Cancelled { partial: false });
        assert!(
            elapsed < Duration::from_millis(25),
            "in-flight cancelled traversal took {elapsed:?}"
        );
    }

    #[test]
    fn recall_is_monotonic_in_ef() {
        let (encoded, rescore) = complete_graph_fixture(12);
        let graph = decode_node_blocks(encoded.as_bytes()).expect("fixture graph is valid");
        let mut seeded =
            crate::test_support::seeded_rng("graph::search::tests::recall_is_monotonic_in_ef");
        let mut runner = TestRunner::new(Config {
            cases: 64,
            rng_seed: RngSeed::Fixed(seeded.next_u64()),
            ..Config::default()
        });
        let result = runner.run(&(5_usize..12, 1_usize..8, -1.0_f32..13.0), |input| {
            let (ef1, delta, query_value) = input;
            let ef2 = ef1.saturating_add(delta).min(12);
            prop_assume!(ef1 < ef2);
            let query = vector(query_value);
            let truth = brute_force_topk(&rescore, &query, 5);
            let mut first = GraphSearcher::new(graph, &rescore).expect("first searcher");
            let mut second = GraphSearcher::new(graph, &rescore).expect("second searcher");
            let result1 = first
                .search(GraphSearchRequest::new(&query, 5, ef1, 0x19_0408), None)
                .expect("ef1 traversal");
            let result2 = second
                .search(GraphSearchRequest::new(&query, 5, ef2, 0x19_0408), None)
                .expect("ef2 traversal");
            prop_assert!(result2.recall_against(&truth) >= result1.recall_against(&truth));
            Ok(())
        });

        assert!(result.is_ok(), "property result: {result:?}");
    }

    fn complete_graph_fixture(rows: usize) -> (crate::graph::block::EncodedNodeBlocks, Vec<f32>) {
        let layout = GraphNodeLayout::new(DIMS as u32, DIMS as u32, (rows - 1) as u8)
            .expect("fixture layout is valid");
        let rescore = (0..rows)
            .flat_map(|row| vector(row as f32))
            .collect::<Vec<_>>();
        let mut codes = vec![0_u8; rows * DIMS.div_ceil(2)];
        let mut factors = Vec::with_capacity(rows);
        for (row, destination) in rescore
            .chunks_exact(DIMS)
            .zip(codes.chunks_exact_mut(DIMS.div_ceil(2)))
        {
            factors.push(quantize_bit4(row, destination).expect("fixture row is finite"));
        }
        let neighbors = (0..rows)
            .map(|row| {
                (0..rows)
                    .filter(|candidate| *candidate != row)
                    .map(|candidate| candidate as u32)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let nodes = (0..rows)
            .map(|row| GraphNodeBlockInput {
                codes: &codes[row * DIMS.div_ceil(2)..(row + 1) * DIMS.div_ceil(2)],
                factors: factors[row],
                flags: u8::from(row < 4),
                neighbors: &neighbors[row],
            })
            .collect::<Vec<_>>();
        let encoded = encode_node_blocks(GraphNodeBlockBuild {
            layout,
            nodes: &nodes,
        })
        .expect("fixture graph encodes");
        (encoded, rescore)
    }

    fn chain_graph_fixture(rows: usize) -> (crate::graph::block::EncodedNodeBlocks, Vec<f32>) {
        let layout =
            GraphNodeLayout::new(DIMS as u32, DIMS as u32, 1).expect("fixture layout is valid");
        let rescore = (0..rows)
            .flat_map(|row| vector(row as f32))
            .collect::<Vec<_>>();
        let mut codes = vec![0_u8; rows * DIMS.div_ceil(2)];
        let mut factors = Vec::with_capacity(rows);
        for (row, destination) in rescore
            .chunks_exact(DIMS)
            .zip(codes.chunks_exact_mut(DIMS.div_ceil(2)))
        {
            factors.push(quantize_bit4(row, destination).expect("fixture row is finite"));
        }
        let neighbors = (0..rows)
            .map(|row| {
                (row + 1 < rows)
                    .then_some((row + 1) as u32)
                    .into_iter()
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let nodes = (0..rows)
            .map(|row| GraphNodeBlockInput {
                codes: &codes[row * DIMS.div_ceil(2)..(row + 1) * DIMS.div_ceil(2)],
                factors: factors[row],
                flags: u8::from(row < 4),
                neighbors: &neighbors[row],
            })
            .collect::<Vec<_>>();
        let encoded = encode_node_blocks(GraphNodeBlockBuild {
            layout,
            nodes: &nodes,
        })
        .expect("fixture graph encodes");
        (encoded, rescore)
    }

    fn long_chain_graph_fixture(rows: usize) -> (crate::graph::block::EncodedNodeBlocks, Vec<f32>) {
        let layout =
            GraphNodeLayout::new(DIMS as u32, DIMS as u32, 44).expect("fixture layout is valid");
        let codes = vec![0_u8; rows * DIMS.div_ceil(2)];
        let factors = (0..rows)
            .map(|row| Bit4Factors::from_persisted(1.0, (rows - row) as f32, 0.0))
            .collect::<Vec<_>>();
        let neighbors = (0..rows)
            .map(|row| {
                (row + 1 < rows)
                    .then_some((row + 1) as u32)
                    .into_iter()
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let nodes = (0..rows)
            .map(|row| GraphNodeBlockInput {
                codes: &codes[row * DIMS.div_ceil(2)..(row + 1) * DIMS.div_ceil(2)],
                factors: factors[row],
                flags: u8::from(row < 4),
                neighbors: &neighbors[row],
            })
            .collect::<Vec<_>>();
        let encoded = encode_node_blocks(GraphNodeBlockBuild {
            layout,
            nodes: &nodes,
        })
        .expect("fixture graph encodes");
        (encoded, vec![0.0_f32; rows * DIMS])
    }

    fn vector(value: f32) -> Vec<f32> {
        let mut vector = vec![0.0_f32; DIMS];
        vector[0] = value;
        vector[1] = value * value * 0.1;
        vector[17] = value.sin();
        vector
    }

    fn brute_force_topk(base: &[f32], query: &[f32], k: usize) -> Vec<u32> {
        let mut scored = base
            .chunks_exact(DIMS)
            .enumerate()
            .map(|(row_id, row)| {
                let distance = row
                    .iter()
                    .zip(query)
                    .map(|(left, right)| {
                        let delta = f64::from(*left) - f64::from(*right);
                        delta * delta
                    })
                    .sum::<f64>();
                (distance, row_id as u32)
            })
            .collect::<Vec<_>>();
        scored.sort_unstable_by(|left, right| {
            left.0
                .total_cmp(&right.0)
                .then_with(|| left.1.cmp(&right.1))
        });
        scored
            .into_iter()
            .take(k)
            .map(|(_, row_id)| row_id)
            .collect()
    }
}
