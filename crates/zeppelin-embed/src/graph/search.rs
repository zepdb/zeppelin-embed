//! Single-core fixed-stride graph traversal.

use crate::graph::block::{CheckedNodeId, GraphNodeBlocks, GraphNodeError};
use crate::kernels::{Bit4Rows4, GatherShapeError, score_bit4_ptrs};
use crate::lifecycle::QueryCancellation;
use crate::meta::DocBitmap;
use crate::quant::{
    Bit4Query, QuantError, RescoreError, RescoreMetric, RescorePool, prepare_bit4_query,
    rescore_top_k,
};
use crate::scan::ScanError;

/// Research lower-bound multiplier from `tasks/reports/index-design-research.md` section 3.1.
const SIFT_RESEARCH_FLOOR_TENTHS: usize = 14;
/// Denominator for the section-3.1 `1.4 * k` lower bound.
const EF_TENTHS_DENOMINATOR: usize = 10;
/// Section-3.1's absolute SIFT-class floor; the measured `ef=140` smoke point
/// in `tasks/cross-benchmark/results/zeppelin-embed/sift-128-euclidean/2026-08-23T16:38:53.443Z-a34b3b7c-graph-ef-140.json`
/// reached only 0.862 recall, so this is deliberately not the shipped target.
const SIFT_RESEARCH_ABSOLUTE_FLOOR: usize = 140;
/// The measured SIFT smoke curve's first arm above hnswlib recall was `ef=200`
/// at `k=100` (0.932 recall, 0.109417 ms p50), recorded in
/// `tasks/cross-benchmark/results/zeppelin-embed/sift-128-euclidean/2026-08-23T16:38:53.443Z-a34b3b7c-graph-ef-200.json`.
const SIFT_SHIPPED_EF_PER_K: usize = 2;
/// Owner-specified angular floor from research section 3.1. The full M4b
/// measurement in `tasks/evidence/19-M4b-cross-dataset-graphs.md` shows that
/// `2 * k` is insufficient on glove. M10 ships the `4 * k` mechanism while
/// the real glove/nytimes acceptance sweep remains deliberately deferred.
const ANGULAR_EF_PER_K: usize = 4;
/// Absolute tolerance on squared L2 norm for caller-declared unit vectors.
pub const UNIT_NORM_SQUARED_TOLERANCE: f64 = 1.0e-3;

pub(crate) fn non_unit_squared_norm(vector: &[f32]) -> Option<f64> {
    if vector.is_empty() || vector.iter().any(|value| !value.is_finite()) {
        return None;
    }
    let squared_norm = vector
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>();
    ((squared_norm - 1.0).abs() > UNIT_NORM_SQUARED_TOLERANCE).then_some(squared_norm)
}

/// Dataset-shape profile used when the caller leaves `ef` adaptive.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum GraphSearchProfile {
    /// SIFT-class Euclidean data, shipping at the first measured passing curve arm.
    #[default]
    SiftClass,
    /// Angular data, using M10's section-3.1 `4 * k` floor.
    Angular,
}

/// Exact distance semantics used to match an epoch to one graph profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphDistanceMetric {
    /// Exact rescore and graph construction use squared Euclidean distance.
    SquaredL2,
}

impl std::fmt::Display for GraphDistanceMetric {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SquaredL2 => formatter.write_str("squared-l2"),
        }
    }
}

/// Persisted epoch shape consulted by the graph-profile policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphProfileShape {
    /// Persisted document-tower dimensions.
    pub document_dims: u32,
    /// Persisted document-tower normalization.
    pub document_normalization: crate::epoch::Normalization,
    /// Persisted query-tower dimensions.
    pub query_dims: u32,
    /// Persisted query-tower normalization.
    pub query_normalization: crate::epoch::Normalization,
    /// Exact metric implemented by the graph build and query paths.
    pub metric: GraphDistanceMetric,
}

/// One measured end-to-end graph profile selected from persisted epoch shape.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EpochGraphProfile {
    /// The measured 128-dimensional, unnormalized, squared-L2 SIFT class.
    SiftClass,
    /// L2-normalized document and query towers scored by squared L2.
    AngularClass,
}

impl EpochGraphProfile {
    /// Returns the adaptive query profile belonging to this epoch profile.
    #[must_use]
    pub const fn search_profile(self) -> GraphSearchProfile {
        match self {
            Self::SiftClass => GraphSearchProfile::SiftClass,
            Self::AngularClass => GraphSearchProfile::Angular,
        }
    }

    /// Returns the construction parameters belonging to this epoch profile.
    #[must_use]
    pub const fn build_params(self) -> crate::graph::GraphParams {
        match self {
            Self::SiftClass => crate::graph::GraphParams::sift_1m(),
            Self::AngularClass => crate::graph::GraphParams::angular(),
        }
    }
}

/// Typed failure to resolve one persisted epoch to a measured graph profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphProfileError {
    /// The store predates the epoch registry needed to select a profile.
    EpochUnstamped,
    /// A validated alias did not have a matching registry entry in the pinned snapshot.
    EpochNotRegistered {
        /// Alias that could not be resolved.
        epoch: crate::epoch::EpochIdentity,
    },
    /// The registry entry's complete shape has no measured profile.
    UnrecognizedEpochShape {
        /// Persisted embedding and tokenizer epoch being reported.
        epoch: crate::epoch::EpochIdentity,
        /// Complete policy key that had no table entry.
        shape: GraphProfileShape,
    },
}

impl std::fmt::Display for GraphProfileError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EpochUnstamped => formatter.write_str(
                "graph profile requires a persisted epoch registry, but the store is unstamped",
            ),
            Self::EpochNotRegistered { epoch } => write!(
                formatter,
                "graph profile epoch ({}, {}) is absent from the pinned registry",
                epoch.embedding, epoch.tokenizer
            ),
            Self::UnrecognizedEpochShape { epoch, shape } => write!(
                formatter,
                "graph profile epoch ({}, {}) has unrecognized shape: document dims {}, document normalization {:?}, query dims {}, query normalization {:?}, metric {}",
                epoch.embedding,
                epoch.tokenizer,
                shape.document_dims,
                shape.document_normalization,
                shape.query_dims,
                shape.query_normalization,
                shape.metric
            ),
        }
    }
}

impl std::error::Error for GraphProfileError {}

/// Selects the supported graph profile for one persisted epoch.
pub fn select_epoch_graph_profile(
    epoch: &crate::manifest::EpochMeta,
    metric: GraphDistanceMetric,
) -> Result<EpochGraphProfile, GraphProfileError> {
    let identity = crate::epoch::EpochIdentity {
        embedding: epoch.id,
        tokenizer: epoch.tokenizer,
    };
    let shape = GraphProfileShape {
        document_dims: epoch.embedding.document.dims,
        document_normalization: epoch.embedding.document.normalization,
        query_dims: epoch.embedding.query.dims,
        query_normalization: epoch.embedding.query.normalization,
        metric,
    };
    // The refusal axis is NORMALIZATION and METRIC, not dimension count.
    //
    // Dimension is deliberately NOT a refusal axis. The profile's parameters
    // are graph degree, alpha and ef, which track intrinsic dimensionality and
    // corpus size rather than the raw dimension count, and refusing on an
    // exact equality to 128 would refuse every real deployment -- 384, 768 and
    // 1536-dimension embeddings included -- while admitting nothing SIFT-like
    // that a 128-dimension check admits. Tuning a profile per dimension band
    // is a measurement campaign, not a predicate.
    //
    // Mismatched document and query towers are refused: a graph built in one
    // space and queried from another is not a profile question.
    if shape.document_dims == shape.query_dims && shape.metric == GraphDistanceMetric::SquaredL2 {
        match (shape.document_normalization, shape.query_normalization) {
            (crate::epoch::Normalization::None, crate::epoch::Normalization::None) => {
                return Ok(EpochGraphProfile::SiftClass);
            }
            (crate::epoch::Normalization::L2, crate::epoch::Normalization::L2) => {
                // On the unit sphere, squared-L2 and cosine induce the same
                // ordering, so M10 needs no new persisted metric or kernel.
                return Ok(EpochGraphProfile::AngularClass);
            }
            _ => {}
        }
    }
    Err(GraphProfileError::UnrecognizedEpochShape {
        epoch: identity,
        shape,
    })
}

/// Typed rejection while resolving an adaptive or explicit `ef`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdaptiveEfError {
    /// Top-k must request at least one result.
    ZeroK,
    /// Top-k exceeded the graph's row count.
    KExceedsRows {
        /// Requested result count.
        k: usize,
        /// Available graph rows.
        rows: usize,
    },
    /// An explicit override was narrower than top-k.
    ExplicitBelowK {
        /// Requested result count.
        k: usize,
        /// Supplied traversal width.
        ef: usize,
    },
    /// An explicit override exceeded the graph's row count.
    ExplicitExceedsRows {
        /// Supplied traversal width.
        ef: usize,
        /// Available graph rows.
        rows: usize,
    },
    /// Adaptive multiplier arithmetic overflowed `usize`.
    ArithmeticOverflow,
}

impl std::fmt::Display for AdaptiveEfError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroK => formatter.write_str("adaptive graph top-k must be positive"),
            Self::KExceedsRows { k, rows } => {
                write!(formatter, "graph top-k {k} exceeds graph row count {rows}")
            }
            Self::ExplicitBelowK { k, ef } => {
                write!(formatter, "explicit graph ef {ef} is below top-k {k}")
            }
            Self::ExplicitExceedsRows { ef, rows } => {
                write!(
                    formatter,
                    "explicit graph ef {ef} exceeds graph row count {rows}"
                )
            }
            Self::ArithmeticOverflow => formatter.write_str("adaptive graph ef overflowed usize"),
        }
    }
}

impl std::error::Error for AdaptiveEfError {}

/// One graph-query invocation.
#[derive(Clone, Copy, Debug)]
pub struct GraphSearchRequest<'a> {
    query: &'a [f32],
    k: usize,
    ef: Option<usize>,
    profile: GraphSearchProfile,
    seed: u64,
    trace_candidates: bool,
    observed_core_class: QueryCoreClass,
    prefetch: TraversalPrefetch,
}

impl<'a> GraphSearchRequest<'a> {
    /// Creates a deterministic SIFT-class request with adaptive shipped `ef`.
    #[must_use]
    pub const fn new(query: &'a [f32], k: usize, seed: u64) -> Self {
        Self {
            query,
            k,
            ef: None,
            profile: GraphSearchProfile::SiftClass,
            seed,
            trace_candidates: false,
            observed_core_class: QueryCoreClass::Unverified,
            prefetch: TraversalPrefetch::Enabled,
        }
    }

    /// Selects the dataset-shape profile used by adaptive `ef`.
    #[must_use]
    pub const fn with_profile(mut self, profile: GraphSearchProfile) -> Self {
        self.profile = profile;
        self
    }

    /// Overrides adaptive `ef` with one explicit traversal width.
    #[must_use]
    pub const fn with_ef(mut self, ef: usize) -> Self {
        self.ef = Some(ef);
        self
    }

    /// Returns the profile's documented research floor before row-count clamping.
    ///
    /// # Errors
    ///
    /// Returns [`AdaptiveEfError`] for zero `k` or multiplier overflow.
    pub fn research_floor_ef(self) -> Result<usize, AdaptiveEfError> {
        if self.k == 0 {
            return Err(AdaptiveEfError::ZeroK);
        }
        match self.profile {
            GraphSearchProfile::SiftClass => self
                .k
                .checked_mul(SIFT_RESEARCH_FLOOR_TENTHS)
                .map(|value| value.div_ceil(EF_TENTHS_DENOMINATOR))
                .map(|value| value.max(SIFT_RESEARCH_ABSOLUTE_FLOOR))
                .ok_or(AdaptiveEfError::ArithmeticOverflow),
            GraphSearchProfile::Angular => self
                .k
                .checked_mul(ANGULAR_EF_PER_K)
                .ok_or(AdaptiveEfError::ArithmeticOverflow),
        }
    }

    /// Resolves the explicit override or adaptive profile against one graph.
    ///
    /// # Errors
    ///
    /// Returns [`AdaptiveEfError`] when `k` or an explicit override is outside
    /// the graph, or when adaptive multiplier arithmetic overflows.
    pub fn effective_ef(self, rows: usize) -> Result<usize, AdaptiveEfError> {
        if self.k == 0 {
            return Err(AdaptiveEfError::ZeroK);
        }
        if self.k > rows {
            return Err(AdaptiveEfError::KExceedsRows { k: self.k, rows });
        }
        if let Some(ef) = self.ef {
            if ef < self.k {
                return Err(AdaptiveEfError::ExplicitBelowK { k: self.k, ef });
            }
            if ef > rows {
                return Err(AdaptiveEfError::ExplicitExceedsRows { ef, rows });
            }
            return Ok(ef);
        }
        let floor = self.research_floor_ef()?;
        let target = match self.profile {
            GraphSearchProfile::SiftClass => self
                .k
                .checked_mul(SIFT_SHIPPED_EF_PER_K)
                .map(|ef| ef.max(floor))
                .ok_or(AdaptiveEfError::ArithmeticOverflow)?,
            GraphSearchProfile::Angular => floor,
        };
        Ok(target.min(rows))
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
    effective_ef: usize,
    hops: usize,
    candidates_scored: usize,
    candidates_rescored: usize,
    pushes: usize,
    visited: usize,
    visited_epoch_cleared: bool,
    dims_touched: u64,
    bytes_read: u64,
    qos_class: QueryQosClass,
    qos_relative_priority: i32,
    core_class: QueryCoreClass,
}

/// Scheduling-independent counters suitable for deterministic comparisons.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeterministicGraphWork {
    /// Adaptive or explicit traversal width used for this query.
    pub effective_ef: usize,
    /// Expanded frontier nodes.
    pub hops: usize,
    /// Distinct Bit4 candidates scored.
    pub candidates_scored: usize,
    /// Rows from the retained pool read from f32 storage.
    pub candidates_rescored: usize,
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
            effective_ef: self.effective_ef,
            hops: self.hops,
            candidates_scored: self.candidates_scored,
            candidates_rescored: self.candidates_rescored,
            pushes: self.pushes,
            visited: self.visited,
        }
    }

    /// Returns the adaptive or explicit traversal width used by the query.
    #[must_use]
    pub const fn effective_ef(self) -> usize {
        self.effective_ef
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

    /// Returns the number of retained candidates exactly rescored from f32 rows.
    #[must_use]
    pub const fn candidates_rescored(self) -> usize {
        self.candidates_rescored
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

    /// Returns logical coordinates scored by coarse traversal plus exact rescore.
    #[must_use]
    pub const fn dims_touched(self) -> u64 {
        self.dims_touched
    }

    /// Returns exact stored coarse and f32-rescore bytes read by this query.
    #[must_use]
    pub const fn bytes_read(self) -> u64 {
        self.bytes_read
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

/// Completion of one filter-aware traversal before any store-level exact fallback.
#[derive(Clone, Debug, PartialEq)]
pub enum FilteredGraphSearchOutcome {
    /// Traversal retained only allowed rows and exact-rescored that complete pool.
    Traversed(GraphSearchResult),
    /// Expected selectivity pressure crossed its independent work budget.
    VisitedBudgetExceeded {
        /// Distinct graph nodes visited before traversal abandoned.
        visited: usize,
        /// Filter-only selectivity budget supplied by the planner.
        budget: usize,
        /// Work performed before the exact allow-list fallback takes over.
        counters: GraphSearchCounters,
    },
}

impl FilteredGraphSearchOutcome {
    /// Returns deterministic work performed before either completion disposition.
    #[must_use]
    pub const fn counters(&self) -> GraphSearchCounters {
        match self {
            Self::Traversed(result) => result.counters,
            Self::VisitedBudgetExceeded { counters, .. } => *counters,
        }
    }
}

enum SearchInnerOutcome {
    Traversed(GraphSearchResult),
    VisitedBudgetExceeded {
        visited: usize,
        budget: usize,
        counters: GraphSearchCounters,
    },
}

/// Typed graph traversal rejection.
#[derive(Clone, Debug, PartialEq)]
pub enum GraphSearchError {
    /// Request shape or graph/rescore geometry is invalid.
    Geometry(String),
    /// The pinned epoch has no measured automatic graph profile.
    Profile(GraphProfileError),
    /// A validated node became unavailable through the fixed-stride view.
    Graph(GraphNodeError),
    /// Query preparation or Bit4 scoring rejected the input.
    Quant(QuantError),
    /// The four-row gather boundary rejected a graph/query shape mismatch.
    Gather(GatherShapeError),
    /// Adaptive or explicit `ef` resolution rejected the request.
    AdaptiveEf(AdaptiveEfError),
    /// The canonical f32 pool-rescore path rejected its inputs.
    Rescore(RescoreError),
    /// A filtered traversal retained fewer allowed rows than the requested result count.
    FilteredCandidateShortfall {
        /// Requested result count.
        k: usize,
        /// Allowed candidates retained by the original traversal.
        candidates: usize,
        /// Exact work performed by that original traversal before refusal.
        counters: GraphSearchCounters,
    },
    /// Chunk-local integrity validation rejected exact-rescore bytes.
    ExactRescoreUnavailable(String),
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
            Self::Profile(error) => error.fmt(formatter),
            Self::Graph(error) => error.fmt(formatter),
            Self::Quant(error) => error.fmt(formatter),
            Self::Gather(error) => error.fmt(formatter),
            Self::AdaptiveEf(error) => error.fmt(formatter),
            Self::Rescore(error) => error.fmt(formatter),
            Self::FilteredCandidateShortfall { k, candidates, .. } => write!(
                formatter,
                "filtered graph search retained {candidates} candidates for requested k={k}"
            ),
            Self::ExactRescoreUnavailable(detail) => formatter.write_str(detail),
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
            Self::Profile(error) => Some(error),
            Self::Graph(error) => Some(error),
            Self::Quant(error) => Some(error),
            Self::Gather(error) => Some(error),
            Self::AdaptiveEf(error) => Some(error),
            Self::Rescore(error) => Some(error),
            Self::Scan(error) => Some(error),
            Self::Geometry(_)
            | Self::ExactRescoreUnavailable(_)
            | Self::FilteredCandidateShortfall { .. }
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

impl From<AdaptiveEfError> for GraphSearchError {
    fn from(error: AdaptiveEfError) -> Self {
        Self::AdaptiveEf(error)
    }
}

impl From<RescoreError> for GraphSearchError {
    fn from(error: RescoreError) -> Self {
        Self::Rescore(error)
    }
}

#[derive(Clone, Copy, Debug)]
struct ScoredNode {
    row_id: CheckedNodeId,
    distance: f64,
}

/// Query-independent exact-row norm enclosure for one immutable graph segment.
///
/// The graph stores the original row norm as `scale * normalized_norm`, where
/// only the normalized factor is rounded to f32. One full normalized-factor
/// ULP on either side therefore encloses the norm used to build the segment.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GraphSegmentNormRange {
    minimum: f64,
    maximum: f64,
}

impl GraphSegmentNormRange {
    pub(crate) fn from_graph(
        graph: GraphNodeBlocks<'_>,
        cancellation: Option<&QueryCancellation<'_>>,
    ) -> Result<Self, GraphSearchError> {
        let mut minimum = f64::INFINITY;
        let mut maximum = 0.0_f64;
        for raw_row_id in 0..graph.node_count() {
            if raw_row_id.is_multiple_of(256) {
                check_cancellation(cancellation)?;
            }
            let row_id = graph.checked_node_id(raw_row_id)?;
            let (_, factors) = graph.score_row_checked(row_id)?;
            let [scale, normalized_norm, _] = factors.persisted_fields();
            if scale < 0.0 || normalized_norm < 0.0 {
                return Ok(Self::unbounded());
            }
            let center = f64::from(scale) * f64::from(normalized_norm);
            let normalized_ulp = f64::from(next_up_f32(normalized_norm) - normalized_norm);
            let uncertainty = f64::from(scale) * normalized_ulp;
            minimum = minimum.min((center - uncertainty).max(0.0));
            maximum = maximum.max(center + uncertainty);
        }
        check_cancellation(cancellation)?;
        if !minimum.is_finite() || !maximum.is_finite() {
            return Ok(Self::unbounded());
        }
        Ok(Self { minimum, maximum })
    }

    const fn unbounded() -> Self {
        Self {
            minimum: 0.0,
            maximum: f64::INFINITY,
        }
    }

    /// Returns a conservative lower bound in the exact-rescore f64 arithmetic.
    ///
    /// The query norm interval covers positive f64 accumulation error for the
    /// crate-wide maximum dimension. The final factor covers the same bounded
    /// accumulation error in `squared_l2_f64`; both endpoints are rounded
    /// outward. A returned value is therefore never above a distance that the
    /// existing independent exact-rescore path can compute for this segment.
    pub(crate) fn squared_l2_lower_bound(self, query: &[f32]) -> f64 {
        if !self.maximum.is_finite() || query.is_empty() {
            return 0.0;
        }
        let squared_norm = query
            .iter()
            .map(|value| {
                let value = f64::from(*value);
                value * value
            })
            .sum::<f64>();
        let relative_error = accumulation_relative_error(query.len());
        let query_minimum = next_down_f64((squared_norm / (1.0 + relative_error)).max(0.0).sqrt());
        let query_maximum = next_up_f64((squared_norm / (1.0 - relative_error)).max(0.0).sqrt());
        let gap = if query_maximum < self.minimum {
            self.minimum - query_maximum
        } else if query_minimum > self.maximum {
            query_minimum - self.maximum
        } else {
            0.0
        };
        next_down_f64(gap * gap * (1.0 - relative_error))
    }

    /// Returns an upper bound used only to preserve today's finite-f32 result
    /// validation when deciding whether a whole segment may be skipped.
    pub(crate) fn squared_l2_upper_bound(self, query: &[f32]) -> f64 {
        if !self.maximum.is_finite() || query.is_empty() {
            return f64::INFINITY;
        }
        let squared_norm = query
            .iter()
            .map(|value| {
                let value = f64::from(*value);
                value * value
            })
            .sum::<f64>();
        let relative_error = accumulation_relative_error(query.len());
        let query_maximum = next_up_f64((squared_norm / (1.0 - relative_error)).max(0.0).sqrt());
        next_up_f64(
            (query_maximum + self.maximum)
                * (query_maximum + self.maximum)
                * (1.0 + relative_error),
        )
    }
}

fn accumulation_relative_error(terms: usize) -> f64 {
    let scaled = terms as f64 * f64::EPSILON;
    (scaled / (1.0 - scaled)) * 2.0
}

fn next_up_f32(value: f32) -> f32 {
    if value == f32::INFINITY {
        return value;
    }
    if value == -0.0 {
        return f32::from_bits(1);
    }
    if value >= 0.0 {
        f32::from_bits(value.to_bits().saturating_add(1))
    } else {
        f32::from_bits(value.to_bits().saturating_sub(1))
    }
}

fn next_up_f64(value: f64) -> f64 {
    if value == f64::INFINITY {
        return value;
    }
    if value == -0.0 {
        return f64::from_bits(1);
    }
    if value >= 0.0 {
        f64::from_bits(value.to_bits().saturating_add(1))
    } else {
        f64::from_bits(value.to_bits().saturating_sub(1))
    }
}

fn next_down_f64(value: f64) -> f64 {
    if value <= 0.0 {
        return 0.0;
    }
    f64::from_bits(value.to_bits().saturating_sub(1))
}

/// Owned, lifetime-free reusable state for single-core graph queries.
#[derive(Debug)]
pub struct GraphSearchScratch {
    node_count: u32,
    max_degree: u8,
    visited: Vec<u8>,
    epoch: u8,
    pool: Vec<ScoredNode>,
    frontier: Vec<ScoredNode>,
    hop_candidates: Vec<CheckedNodeId>,
    rescore_row_ids: Vec<u32>,
    rescore_coarse_scores: Vec<f32>,
}

impl GraphSearchScratch {
    /// Allocates the node-count and maximum-degree state that is independent of `ef`.
    pub fn new(node_count: u32, max_degree: u8) -> Result<Self, GraphSearchError> {
        Self::with_ef_capacity(node_count, max_degree, 0)
    }

    pub(crate) fn with_ef_capacity(
        node_count: u32,
        max_degree: u8,
        ef: usize,
    ) -> Result<Self, GraphSearchError> {
        let node_count_usize = node_count as usize;
        let mut visited = Vec::new();
        visited
            .try_reserve_exact(node_count_usize)
            .map_err(|error| {
                GraphSearchError::Geometry(format!("visited allocation failed: {error}"))
            })?;
        visited.resize(node_count_usize, 0);
        let mut pool = Vec::new();
        pool.try_reserve_exact(ef).map_err(|error| {
            GraphSearchError::Geometry(format!("pool allocation failed: {error}"))
        })?;
        let mut frontier = Vec::new();
        frontier.try_reserve_exact(ef).map_err(|error| {
            GraphSearchError::Geometry(format!("frontier allocation failed: {error}"))
        })?;
        let mut hop_candidates = Vec::new();
        hop_candidates
            .try_reserve_exact(usize::from(max_degree))
            .map_err(|error| {
                GraphSearchError::Geometry(format!("hop-candidate allocation failed: {error}"))
            })?;
        let mut rescore_row_ids = Vec::new();
        rescore_row_ids.try_reserve_exact(ef).map_err(|error| {
            GraphSearchError::Geometry(format!("rescore row-id allocation failed: {error}"))
        })?;
        let mut rescore_coarse_scores = Vec::new();
        rescore_coarse_scores
            .try_reserve_exact(ef)
            .map_err(|error| {
                GraphSearchError::Geometry(format!("rescore score allocation failed: {error}"))
            })?;
        Ok(Self {
            node_count,
            max_degree,
            visited,
            epoch: 0,
            pool,
            frontier,
            hop_candidates,
            rescore_row_ids,
            rescore_coarse_scores,
        })
    }

    /// Returns the exact requested bytes for one scratch at the supplied geometry.
    pub fn allocation_bytes(
        node_count: u32,
        max_degree: u8,
        ef: usize,
    ) -> Result<usize, GraphSearchError> {
        let scored = ef
            .checked_mul(std::mem::size_of::<ScoredNode>())
            .and_then(|bytes| bytes.checked_mul(2))
            .ok_or_else(|| GraphSearchError::Geometry("scratch byte count overflow".to_owned()))?;
        let hops = usize::from(max_degree)
            .checked_mul(std::mem::size_of::<CheckedNodeId>())
            .ok_or_else(|| GraphSearchError::Geometry("scratch byte count overflow".to_owned()))?;
        let rescore = ef
            .checked_mul(std::mem::size_of::<u32>() + std::mem::size_of::<f32>())
            .ok_or_else(|| GraphSearchError::Geometry("scratch byte count overflow".to_owned()))?;
        (node_count as usize)
            .checked_add(scored)
            .and_then(|bytes| bytes.checked_add(hops))
            .and_then(|bytes| bytes.checked_add(rescore))
            .ok_or_else(|| GraphSearchError::Geometry("scratch byte count overflow".to_owned()))
    }

    /// Returns the exact capacities, in bytes, of every owned scratch vector.
    pub fn resident_bytes(&self) -> Result<usize, GraphSearchError> {
        let widths = [
            (self.visited.capacity(), std::mem::size_of::<u8>()),
            (self.pool.capacity(), std::mem::size_of::<ScoredNode>()),
            (self.frontier.capacity(), std::mem::size_of::<ScoredNode>()),
            (
                self.hop_candidates.capacity(),
                std::mem::size_of::<CheckedNodeId>(),
            ),
            (self.rescore_row_ids.capacity(), std::mem::size_of::<u32>()),
            (
                self.rescore_coarse_scores.capacity(),
                std::mem::size_of::<f32>(),
            ),
        ];
        widths.iter().try_fold(0_usize, |total, (capacity, width)| {
            capacity
                .checked_mul(*width)
                .and_then(|bytes| total.checked_add(bytes))
                .ok_or_else(|| GraphSearchError::Geometry("scratch byte count overflow".to_owned()))
        })
    }

    pub(crate) fn supports(&self, node_count: u32, max_degree: u8, ef: usize) -> bool {
        self.node_count == node_count
            && self.max_degree == max_degree
            && self.pool.capacity() >= ef
            && self.frontier.capacity() >= ef
            && self.rescore_row_ids.capacity() >= ef
            && self.rescore_coarse_scores.capacity() >= ef
    }
}

/// Per-query binding of mmap-backed graph views to owned reusable scratch.
pub struct GraphSearcher<'a> {
    graph: GraphNodeBlocks<'a>,
    rescore: &'a [f32],
    rescore_validator: Option<&'a dyn RescoreValidator>,
    entries: [CheckedNodeId; 4],
    scratch: &'a mut GraphSearchScratch,
    #[cfg(any(test, feature = "test-support"))]
    vector_fault: Option<(
        &'a crate::scan::vector_fault::VectorFaultController,
        crate::scan::vector_fault::VectorRowSource,
        crate::scan::vector_fault::VectorSearchTier,
    )>,
    #[cfg(any(test, feature = "test-support"))]
    hop_cancellation: Option<TestHopCancellation>,
}

pub(crate) trait RescoreValidator {
    fn validate_rows(&self, rows: &[u32]) -> Result<(), String>;
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Debug)]
struct TestHopCancellation {
    after_hops: usize,
    token: crate::lifecycle::CancelToken,
    observed_hops: std::rc::Rc<std::cell::Cell<usize>>,
}

impl<'a> GraphSearcher<'a> {
    /// Validates geometry and retains the existing O(N) entry-seed discovery behavior.
    pub fn new(
        graph: GraphNodeBlocks<'a>,
        rescore: &'a [f32],
        scratch: &'a mut GraphSearchScratch,
    ) -> Result<Self, GraphSearchError> {
        let entries = Self::discover_entry_row_ids(graph)?;
        Self::with_entry_row_ids(graph, rescore, entries, scratch)
    }

    /// Discovers all four persisted entry seeds by validating every graph block.
    pub fn discover_entry_row_ids(
        graph: GraphNodeBlocks<'_>,
    ) -> Result<[u32; 4], GraphSearchError> {
        let mut entries = Vec::with_capacity(4);
        for row_id in 0..graph.node_count() {
            if graph.block(row_id)?.flags() & 1 != 0 {
                entries.push(row_id);
            }
        }
        let entry_count = entries.len();
        entries.try_into().map_err(|_: Vec<u32>| {
            GraphSearchError::Geometry(format!(
                "graph contains {entry_count} persisted entry seeds, expected medoid plus three refined seeds"
            ))
        })
    }

    /// Binds cached entry row ids after validating only those four graph blocks.
    pub fn with_entry_row_ids(
        graph: GraphNodeBlocks<'a>,
        rescore: &'a [f32],
        entry_row_ids: [u32; 4],
        scratch: &'a mut GraphSearchScratch,
    ) -> Result<Self, GraphSearchError> {
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
        if scratch.node_count != graph.node_count()
            || scratch.max_degree != graph.layout().max_degree()
        {
            return Err(GraphSearchError::Geometry(format!(
                "scratch geometry nodes/degree {}/{}, graph {}/{}",
                scratch.node_count,
                scratch.max_degree,
                graph.node_count(),
                graph.layout().max_degree()
            )));
        }
        let mut entries = [None; 4];
        for (position, row_id) in entry_row_ids.into_iter().enumerate() {
            let block = graph.block(row_id)?;
            if block.flags() & 1 == 0 {
                return Err(GraphSearchError::Geometry(format!(
                    "cached entry row {row_id} does not carry the persisted entry flag"
                )));
            }
            let destination = entries.get_mut(position).ok_or_else(|| {
                GraphSearchError::Geometry("cached entry position overflow".to_owned())
            })?;
            *destination = Some(graph.checked_node_id(row_id)?);
        }
        let entries = entries.map(|entry| {
            entry.ok_or_else(|| GraphSearchError::Geometry("cached entry is absent".to_owned()))
        });
        let [first, second, third, fourth] = entries;
        Ok(Self {
            graph,
            rescore,
            rescore_validator: None,
            entries: [first?, second?, third?, fourth?],
            scratch,
            #[cfg(any(test, feature = "test-support"))]
            vector_fault: None,
            #[cfg(any(test, feature = "test-support"))]
            hop_cancellation: None,
        })
    }

    pub(crate) fn with_rescore_validator(mut self, validator: &'a dyn RescoreValidator) -> Self {
        self.rescore_validator = Some(validator);
        self
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn with_vector_fault_controller(
        mut self,
        controller: &'a crate::scan::vector_fault::VectorFaultController,
        source: crate::scan::vector_fault::VectorRowSource,
        tier: crate::scan::vector_fault::VectorSearchTier,
    ) -> Self {
        self.vector_fault = Some((controller, source, tier));
        self
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn cancel_after_hops(
        &mut self,
        after_hops: usize,
        token: crate::lifecycle::CancelToken,
    ) -> std::rc::Rc<std::cell::Cell<usize>> {
        let observed_hops = std::rc::Rc::new(std::cell::Cell::new(0));
        self.hop_cancellation = Some(TestHopCancellation {
            after_hops,
            token,
            observed_hops: std::rc::Rc::clone(&observed_hops),
        });
        observed_hops
    }

    #[cfg(any(test, feature = "test-support"))]
    fn observe_hop_for_cancellation(&self, hops: usize) {
        let Some(source) = &self.hop_cancellation else {
            return;
        };
        source.observed_hops.set(hops);
        if hops == source.after_hops {
            source.token.cancel();
        }
    }

    /// Traverses, then exact-rescores the complete retained pool.
    pub fn search(
        &mut self,
        request: GraphSearchRequest<'_>,
        cancellation: Option<&QueryCancellation<'_>>,
    ) -> Result<GraphSearchResult, GraphSearchError> {
        match self.search_inner(request, None, None, cancellation)? {
            SearchInnerOutcome::Traversed(result) => Ok(result),
            SearchInnerOutcome::VisitedBudgetExceeded { .. } => Err(GraphSearchError::Geometry(
                "unfiltered traversal produced a filtered-budget disposition".to_owned(),
            )),
        }
    }

    /// Traverses every reachable node while retaining only rows in `allow_list`.
    ///
    /// `filtered_visited_budget` is a selectivity work budget. Crossing it is a
    /// normal disposition for the store planner to answer through its exact
    /// allow-list branch; it is deliberately distinct from the fail-closed
    /// unfiltered visited-cap error.
    pub fn search_filtered(
        &mut self,
        request: GraphSearchRequest<'_>,
        allow_list: &DocBitmap,
        filtered_visited_budget: usize,
        cancellation: Option<&QueryCancellation<'_>>,
    ) -> Result<FilteredGraphSearchOutcome, GraphSearchError> {
        match self.search_inner(
            request,
            Some(allow_list),
            Some(filtered_visited_budget),
            cancellation,
        )? {
            SearchInnerOutcome::Traversed(result) => {
                Ok(FilteredGraphSearchOutcome::Traversed(result))
            }
            SearchInnerOutcome::VisitedBudgetExceeded {
                visited,
                budget,
                counters,
            } => Ok(FilteredGraphSearchOutcome::VisitedBudgetExceeded {
                visited,
                budget,
                counters,
            }),
        }
    }

    fn search_inner(
        &mut self,
        request: GraphSearchRequest<'_>,
        allow_list: Option<&DocBitmap>,
        filtered_visited_budget: Option<usize>,
        cancellation: Option<&QueryCancellation<'_>>,
    ) -> Result<SearchInnerOutcome, GraphSearchError> {
        let ef = self.validate_request(request)?;
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
        let visited_cap = ef
            .checked_mul(usize::from(self.graph.layout().max_degree()))
            .and_then(|value| value.checked_mul(4))
            .ok_or_else(|| GraphSearchError::Geometry("visited cap overflow".to_owned()))?;
        let visited_epoch_cleared = self.next_epoch();
        let mut counters = GraphSearchCounters {
            effective_ef: ef,
            candidates_rescored: 0,
            hops: 0,
            candidates_scored: 0,
            pushes: 0,
            visited: 0,
            visited_epoch_cleared,
            dims_touched: 0,
            bytes_read: 0,
            qos_class,
            qos_relative_priority,
            core_class: request.observed_core_class,
        };
        self.scratch.pool.clear();
        self.scratch.frontier.clear();
        reserve_exact_scratch(&mut self.scratch.pool, ef, "pool")?;
        reserve_exact_scratch(&mut self.scratch.frontier, ef, "frontier")?;
        let mut candidate_sequence = request.trace_candidates.then(|| Vec::with_capacity(ef));
        let entries = self.entries;
        if request.prefetch.is_enabled() {
            for entry in &entries {
                self.graph.prefetch_line0(*entry);
            }
        }
        let mut seed_group = [None; 4];
        let mut seed_count = 0_usize;
        for entry in entries {
            if mark_visited(&mut self.scratch.visited, self.scratch.epoch, entry)? {
                counters.visited += 1;
                ensure_visited_cap(counters.visited, visited_cap)?;
                if filtered_visited_budget.is_some_and(|budget| counters.visited > budget) {
                    return self.filtered_budget_outcome(counters, filtered_visited_budget);
                }
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
            ef,
            allow_list,
            request.prefetch,
            &mut counters,
            &mut candidate_sequence,
        )?;
        loop {
            check_cancellation(cancellation)?;
            let Some(candidate) = min_heap_pop(&mut self.scratch.frontier)? else {
                break;
            };
            if self.scratch.pool.len() >= ef
                && self
                    .scratch
                    .pool
                    .first()
                    .is_some_and(|worst| candidate.distance > worst.distance)
            {
                break;
            }
            counters.hops += 1;
            #[cfg(any(test, feature = "test-support"))]
            self.observe_hop_for_cancellation(counters.hops);
            let (degree, neighbors) = self.graph.adjacency_checked(candidate.row_id)?;
            let degree = usize::from(degree);
            self.scratch.hop_candidates.clear();
            for raw_neighbor in neighbors.take(degree) {
                let neighbor = self.graph.checked_node_id(raw_neighbor)?;
                if !mark_visited(&mut self.scratch.visited, self.scratch.epoch, neighbor)? {
                    continue;
                }
                counters.visited += 1;
                ensure_visited_cap(counters.visited, visited_cap)?;
                if filtered_visited_budget.is_some_and(|budget| counters.visited > budget) {
                    return self.filtered_budget_outcome(counters, filtered_visited_budget);
                }
                if request.prefetch.is_enabled() {
                    self.graph.prefetch_line0(neighbor);
                }
                self.scratch.hop_candidates.push(neighbor);
            }
            let mut group_start = 0_usize;
            let hop_candidate_count = self.scratch.hop_candidates.len();
            while group_start < hop_candidate_count {
                check_cancellation(cancellation)?;
                let group_count = (hop_candidate_count - group_start).min(4);
                let mut group = [None; 4];
                for lane in 0..group_count {
                    let source = self.scratch.hop_candidates.get(group_start + lane).copied();
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
                    ef,
                    allow_list,
                    request.prefetch,
                    &mut counters,
                    &mut candidate_sequence,
                )?;
                group_start += group_count;
            }
        }
        self.finalize_traversal(
            request,
            allow_list,
            cancellation,
            counters,
            candidate_sequence,
        )
    }

    fn finalize_traversal(
        &mut self,
        request: GraphSearchRequest<'_>,
        allow_list: Option<&DocBitmap>,
        cancellation: Option<&QueryCancellation<'_>>,
        mut counters: GraphSearchCounters,
        candidate_sequence: Option<Vec<u32>>,
    ) -> Result<SearchInnerOutcome, GraphSearchError> {
        check_cancellation(cancellation)?;
        self.scratch.rescore_row_ids.clear();
        self.scratch.rescore_coarse_scores.clear();
        let retained = self.scratch.pool.len();
        reserve_exact_scratch(
            &mut self.scratch.rescore_row_ids,
            retained,
            "rescore row-id",
        )?;
        reserve_exact_scratch(
            &mut self.scratch.rescore_coarse_scores,
            retained,
            "rescore score",
        )?;
        for candidate in &self.scratch.pool {
            self.scratch.rescore_row_ids.push(candidate.row_id.raw());
            let score = -(candidate.distance as f32);
            if !score.is_finite() {
                return Err(GraphSearchError::Geometry(format!(
                    "coarse distance for row {} is not representable as f32",
                    candidate.row_id.raw()
                )));
            }
            self.scratch.rescore_coarse_scores.push(score);
        }
        if let Some(validator) = self.rescore_validator {
            validator
                .validate_rows(&self.scratch.rescore_row_ids)
                .map_err(GraphSearchError::ExactRescoreUnavailable)?;
        }
        let coarse_bytes_per_row = self
            .graph
            .layout()
            .code_bytes()
            .checked_add(12)
            .ok_or_else(|| GraphSearchError::Geometry("coarse row bytes overflow".to_owned()))?;
        let pool = RescorePool::retained(
            &self.scratch.rescore_row_ids,
            &self.scratch.rescore_coarse_scores,
            RescoreMetric::SquaredL2,
            counters.candidates_scored,
            coarse_bytes_per_row,
        )
        .with_prefetch(request.prefetch.is_enabled());
        let rescored = match rescore_top_k(
            request.query,
            self.rescore,
            self.graph.layout().dims() as usize,
            pool,
            request.k,
        ) {
            Ok(rescored) => rescored,
            Err(RescoreError::InsufficientCandidates { k, candidates }) if allow_list.is_some() => {
                counters.candidates_rescored = 0;
                counters.dims_touched = u64::try_from(counters.candidates_scored)
                    .ok()
                    .and_then(|rows| rows.checked_mul(u64::from(self.graph.layout().dims())))
                    .ok_or_else(|| {
                        GraphSearchError::Geometry("dimension counter overflow".to_owned())
                    })?;
                counters.bytes_read = counters
                    .candidates_scored
                    .checked_mul(coarse_bytes_per_row)
                    .and_then(|bytes| u64::try_from(bytes).ok())
                    .ok_or_else(|| {
                        GraphSearchError::Geometry("byte counter exceeds u64".to_owned())
                    })?;
                return Err(GraphSearchError::FilteredCandidateShortfall {
                    k,
                    candidates,
                    counters,
                });
            }
            Err(error) => return Err(GraphSearchError::Rescore(error)),
        };
        check_cancellation(cancellation)?;
        counters.candidates_rescored = rescored.candidates_rescored;
        let touched_rows = counters
            .candidates_scored
            .checked_add(counters.candidates_rescored)
            .ok_or_else(|| GraphSearchError::Geometry("dimension counter overflow".to_owned()))?;
        counters.dims_touched = u64::try_from(touched_rows)
            .ok()
            .and_then(|rows| rows.checked_mul(u64::from(self.graph.layout().dims())))
            .ok_or_else(|| GraphSearchError::Geometry("dimension counter overflow".to_owned()))?;
        counters.bytes_read = u64::try_from(rescored.bytes.total())
            .map_err(|_| GraphSearchError::Geometry("byte counter exceeds u64".to_owned()))?;
        let candidates = rescored
            .hits
            .into_iter()
            .map(|hit| {
                let row_id = u32::try_from(hit.row_index).map_err(|_| {
                    GraphSearchError::Geometry("rescored row id exceeds u32".to_owned())
                })?;
                Ok(GraphSearchCandidate {
                    row_id,
                    distance: (-hit.score).max(0.0),
                })
            })
            .collect::<Result<Vec<_>, GraphSearchError>>()?;
        Ok(SearchInnerOutcome::Traversed(GraphSearchResult {
            candidates,
            counters,
            candidate_sequence,
        }))
    }

    fn filtered_budget_outcome(
        &self,
        mut counters: GraphSearchCounters,
        filtered_visited_budget: Option<usize>,
    ) -> Result<SearchInnerOutcome, GraphSearchError> {
        let budget = filtered_visited_budget.ok_or_else(|| {
            GraphSearchError::Geometry(
                "filtered budget disposition has no configured budget".to_owned(),
            )
        })?;
        counters.dims_touched = u64::try_from(counters.candidates_scored)
            .ok()
            .and_then(|rows| rows.checked_mul(u64::from(self.graph.layout().dims())))
            .ok_or_else(|| GraphSearchError::Geometry("dimension counter overflow".to_owned()))?;
        let coarse_bytes_per_row = self
            .graph
            .layout()
            .code_bytes()
            .checked_add(12)
            .ok_or_else(|| GraphSearchError::Geometry("coarse row bytes overflow".to_owned()))?;
        counters.bytes_read = counters
            .candidates_scored
            .checked_mul(coarse_bytes_per_row)
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or_else(|| GraphSearchError::Geometry("byte counter exceeds u64".to_owned()))?;
        Ok(SearchInnerOutcome::VisitedBudgetExceeded {
            visited: counters.visited,
            budget,
            counters,
        })
    }

    fn validate_request(&self, request: GraphSearchRequest<'_>) -> Result<usize, GraphSearchError> {
        let dimensions = self.graph.layout().dims() as usize;
        if request.query.len() != dimensions {
            return Err(GraphSearchError::Geometry(format!(
                "query has {} dimensions, expected {dimensions}",
                request.query.len()
            )));
        }
        request
            .effective_ef(self.graph.node_count() as usize)
            .map_err(Into::into)
    }

    fn next_epoch(&mut self) -> bool {
        self.scratch.epoch = self.scratch.epoch.wrapping_add(1);
        if self.scratch.epoch == 0 {
            self.scratch.visited.fill(0);
            self.scratch.epoch = 1;
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
        allow_list: Option<&DocBitmap>,
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
            let allowed = allow_list.is_none_or(|mask| mask.contains(candidate.row_id.raw()));
            #[cfg(any(test, feature = "test-support"))]
            if allowed && let Some((controller, source, tier)) = self.vector_fault {
                let local_row = usize::try_from(candidate.row_id.raw()).map_err(|_| {
                    GraphSearchError::Geometry("graph row id exceeds usize".to_owned())
                })?;
                if controller.after_eligible_row(source, tier, local_row) {
                    return Err(GraphSearchError::Cancelled { partial: false });
                }
            }
            if let Some(sequence) = candidate_sequence.as_mut() {
                sequence.push(candidate.row_id.raw());
            }
            let mut pushed = false;
            if allowed && max_heap_would_keep(&self.scratch.pool, candidate, ef) {
                max_heap_insert_bounded(&mut self.scratch.pool, candidate, ef)?;
                min_heap_push(&mut self.scratch.frontier, candidate)?;
                counters.pushes = counters.pushes.checked_add(1).ok_or_else(|| {
                    GraphSearchError::Geometry("push counter overflow".to_owned())
                })?;
                pushed = true;
            } else if !allowed
                && (self.scratch.pool.len() < ef
                    || self
                        .scratch
                        .pool
                        .first()
                        .is_some_and(|worst| candidate.distance <= worst.distance))
            {
                min_heap_push(&mut self.scratch.frontier, candidate)?;
                counters.pushes = counters.pushes.checked_add(1).ok_or_else(|| {
                    GraphSearchError::Geometry("push counter overflow".to_owned())
                })?;
                pushed = true;
            }
            if pushed
                && prefetch.is_enabled()
                && let Some(head) = self.scratch.frontier.first()
            {
                self.graph.prefetch_head_block(head.row_id);
            }
        }
        Ok(())
    }
}

fn reserve_exact_scratch<T>(
    buffer: &mut Vec<T>,
    needed: usize,
    what: &'static str,
) -> Result<(), GraphSearchError> {
    if buffer.capacity() < needed {
        buffer.try_reserve_exact(needed).map_err(|error| {
            GraphSearchError::Geometry(format!("{what} allocation failed: {error}"))
        })?;
    }
    Ok(())
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
    match cancellation.check_graph() {
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
    use std::time::Duration;

    use proptest::prelude::*;
    use proptest::test_runner::{Config, RngSeed, TestRunner};
    use rand::RngCore;

    use super::{
        AdaptiveEfError, FilteredGraphSearchOutcome, GraphSearchError, GraphSearchProfile,
        GraphSearchRequest, GraphSearchScratch, GraphSearcher, GraphSegmentNormRange,
        QueryCoreClass, TraversalPrefetch, next_down_f64, next_up_f32, next_up_f64,
    };
    use crate::graph::block::{
        GraphNodeBlockBuild, GraphNodeBlockInput, GraphNodeLayout, decode_node_blocks,
        encode_node_blocks,
    };
    use crate::kernels::GatherShapeError;
    use crate::lifecycle::{CancelToken, OpenOptions, QueryCancellation, QueryControl, Store};
    use crate::meta::DocBitmap;
    use crate::quant::{Bit4Factors, QuantError, RescoreError, quantize_bit4};
    use crate::scan::ScanError;

    const DIMS: usize = 128;

    #[test]
    fn adaptive_defaults_follow_the_measured_profile_rules() {
        let query = [0.0_f32; DIMS];

        let sift = GraphSearchRequest::new(&query, 100, 0x19_05);
        assert_eq!(sift.research_floor_ef(), Ok(140));
        assert_eq!(sift.effective_ef(1_000_000), Ok(200));
        assert_eq!(sift.effective_ef(180), Ok(180));

        let angular =
            GraphSearchRequest::new(&query, 100, 0x19_05).with_profile(GraphSearchProfile::Angular);
        assert_eq!(angular.research_floor_ef(), Ok(400));
        assert_eq!(angular.effective_ef(1_000_000), Ok(400));

        let explicit = GraphSearchRequest::new(&query, 100, 0x19_05).with_ef(300);
        assert_eq!(explicit.effective_ef(1_000_000), Ok(300));
    }

    #[test]
    fn adaptive_ef_overflow_is_typed() {
        let request = GraphSearchRequest::new(&[], usize::MAX, 0x19_05);
        assert_eq!(
            request.effective_ef(usize::MAX),
            Err(AdaptiveEfError::ArithmeticOverflow)
        );
    }

    #[test]
    fn request_and_search_geometry_rejections_preserve_the_invalid_values() {
        let query = vector(0.0);
        assert_eq!(
            GraphSearchRequest::new(&query, 0, 7).effective_ef(9),
            Err(AdaptiveEfError::ZeroK)
        );
        assert_eq!(
            GraphSearchRequest::new(&query, 10, 7).effective_ef(9),
            Err(AdaptiveEfError::KExceedsRows { k: 10, rows: 9 })
        );
        assert_eq!(
            GraphSearchRequest::new(&query, 4, 7)
                .with_ef(3)
                .effective_ef(9),
            Err(AdaptiveEfError::ExplicitBelowK { k: 4, ef: 3 })
        );
        assert_eq!(
            GraphSearchRequest::new(&query, 4, 7)
                .with_ef(10)
                .effective_ef(9),
            Err(AdaptiveEfError::ExplicitExceedsRows { ef: 10, rows: 9 })
        );

        let (encoded, rescore) = best_first_graph_fixture();
        let graph = decode_node_blocks(encoded.as_bytes()).expect("fixture graph is valid");
        let mut wrong_scratch =
            GraphSearchScratch::new(graph.node_count() + 1, graph.layout().max_degree())
                .expect("mismatched scratch allocates");
        assert!(matches!(
            GraphSearcher::new(graph, &rescore, &mut wrong_scratch),
            Err(GraphSearchError::Geometry(detail))
                if detail.contains("scratch geometry")
        ));

        let mut scratch = GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())
            .expect("fixture scratch");
        assert!(matches!(
            GraphSearcher::new(graph, &rescore[..rescore.len() - 1], &mut scratch),
            Err(GraphSearchError::Geometry(detail))
                if detail.contains("rescore has") && detail.contains("expected")
        ));
        assert!(matches!(
            GraphSearcher::with_entry_row_ids(graph, &rescore, [0, 1, 2, 4], &mut scratch),
            Err(GraphSearchError::Geometry(detail))
                if detail.contains("cached entry row 4")
        ));

        let mut searcher =
            GraphSearcher::new(graph, &rescore, &mut scratch).expect("valid searcher");
        assert!(matches!(
            searcher.search(
                GraphSearchRequest::new(&query[..DIMS - 1], 1, 7).with_ef(4),
                None,
            ),
            Err(GraphSearchError::Geometry(detail))
                if detail.contains("127 dimensions") && detail.contains("expected 128")
        ));
        let mut non_finite = query.clone();
        non_finite[17] = f32::NAN;
        assert_eq!(
            searcher.search(GraphSearchRequest::new(&non_finite, 1, 7).with_ef(4), None,),
            Err(GraphSearchError::Quant(QuantError::NonFinite { index: 17 }))
        );
        let recovered = searcher
            .search(GraphSearchRequest::new(&query, 1, 7).with_ef(4), None)
            .expect("a rejected request leaves reusable scratch valid");
        assert_eq!(recovered.candidates()[0].row_id(), 8);

        let one_entry = factor_graph_fixture(Bit4Factors::from_persisted(1.0, 1.0, 0.0), 1);
        let one_entry = decode_node_blocks(one_entry.as_bytes()).expect("one-entry graph is valid");
        assert!(matches!(
            GraphSearcher::discover_entry_row_ids(one_entry),
            Err(GraphSearchError::Geometry(detail)) if detail.contains("contains 1 persisted entry")
        ));
    }

    #[test]
    fn segment_norm_bounds_round_outward_or_fail_open() {
        let bounded = factor_graph_fixture(Bit4Factors::from_persisted(2.0, 10.0, 0.0), 0);
        let bounded = decode_node_blocks(bounded.as_bytes()).expect("bounded graph is valid");
        let bounded = GraphSegmentNormRange::from_graph(bounded, None).expect("norm scan succeeds");
        let zero_query = vec![0.0_f32; DIMS];
        assert!(bounded.squared_l2_lower_bound(&zero_query) <= 400.0);
        assert!(bounded.squared_l2_upper_bound(&zero_query) >= 400.0);
        assert!(bounded.squared_l2_lower_bound(&zero_query) > 0.0);
        assert_eq!(bounded.squared_l2_lower_bound(&[]), 0.0);
        assert_eq!(bounded.squared_l2_upper_bound(&[]), f64::INFINITY);

        for factors in [
            Bit4Factors::from_persisted(-1.0, 1.0, 0.0),
            Bit4Factors::from_persisted(f32::MAX, f32::MAX, 0.0),
        ] {
            let encoded = factor_graph_fixture(factors, 0);
            let graph = decode_node_blocks(encoded.as_bytes()).expect("finite factors decode");
            let range = GraphSegmentNormRange::from_graph(graph, None).expect("norm scan succeeds");
            assert_eq!(range.squared_l2_lower_bound(&zero_query), 0.0);
            assert_eq!(range.squared_l2_upper_bound(&zero_query), f64::INFINITY);
        }
    }

    #[test]
    fn norm_bound_successors_are_exact_ieee_neighbours() {
        assert_eq!(next_up_f32(f32::INFINITY), f32::INFINITY);
        assert_eq!(next_up_f32(-0.0).to_bits(), 1);
        assert_eq!(next_up_f32(-1.0).to_bits(), (-1.0_f32).to_bits() - 1);
        assert_eq!(next_up_f64(f64::INFINITY), f64::INFINITY);
        assert_eq!(next_up_f64(-0.0).to_bits(), 1);
        assert_eq!(next_up_f64(-1.0).to_bits(), (-1.0_f64).to_bits() - 1);
        assert_eq!(next_down_f64(0.0), 0.0);
        assert_eq!(next_down_f64(1.0).to_bits(), 1.0_f64.to_bits() - 1);
    }

    #[test]
    fn graph_search_errors_keep_typed_sources_and_actionable_messages() {
        let sourced = [
            GraphSearchError::from(crate::graph::block::GraphNodeError::InvalidHeader(
                "bad graph header".to_owned(),
            )),
            GraphSearchError::from(QuantError::EmptyVector),
            GraphSearchError::from(GatherShapeError::RowBytes {
                expected: 64,
                actual: 63,
            }),
            GraphSearchError::from(AdaptiveEfError::ExplicitBelowK { k: 4, ef: 3 }),
            GraphSearchError::from(AdaptiveEfError::ZeroK),
            GraphSearchError::from(AdaptiveEfError::KExceedsRows { k: 10, rows: 9 }),
            GraphSearchError::from(AdaptiveEfError::ExplicitExceedsRows { ef: 10, rows: 9 }),
            GraphSearchError::from(AdaptiveEfError::ArithmeticOverflow),
            GraphSearchError::from(RescoreError::InsufficientCandidates {
                k: 4,
                candidates: 3,
            }),
            GraphSearchError::Scan(ScanError::ZeroDimension),
        ];
        for error in sourced {
            assert!(!error.to_string().is_empty());
            assert!(std::error::Error::source(&error).is_some());
        }

        let unsourced = [
            GraphSearchError::Geometry("bad request shape".to_owned()),
            GraphSearchError::VisitedCapExceeded {
                visited: 17,
                cap: 16,
            },
            GraphSearchError::Cancelled { partial: false },
            GraphSearchError::Timeout { partial: false },
            GraphSearchError::ReadCancelled { partial: false },
        ];
        let messages = unsourced
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        assert!(messages[0].contains("bad request shape"));
        assert!(messages[1].contains("17") && messages[1].contains("16"));
        assert!(messages[2].contains("cancelled") && messages[2].contains("partial=false"));
        assert!(messages[3].contains("deadline") && messages[3].contains("partial=false"));
        assert!(messages[4].contains("store close") && messages[4].contains("partial=false"));
        assert!(
            unsourced
                .iter()
                .all(|error| std::error::Error::source(error).is_none())
        );
    }

    #[test]
    fn adaptive_ef_stays_between_k_and_rows_under_adversarial_shapes() {
        let mut seeded = crate::test_support::seeded_rng(
            "graph::search::tests::adaptive_ef_stays_between_k_and_rows_under_adversarial_shapes",
        );
        let mut runner = TestRunner::new(Config {
            cases: 256,
            rng_seed: RngSeed::Fixed(seeded.next_u64()),
            ..Config::default()
        });
        let result = runner.run(
            &(1_usize..1_000_001, 1_usize..1_000_001, any::<bool>()),
            |(rows, raw_k, angular)| {
                let k = raw_k.min(rows);
                let profile = if angular {
                    GraphSearchProfile::Angular
                } else {
                    GraphSearchProfile::SiftClass
                };
                let request = GraphSearchRequest::new(&[], k, 0x19_05).with_profile(profile);
                let ef = request.effective_ef(rows).expect("valid shape resolves");
                prop_assert!(ef >= k);
                prop_assert!(ef <= rows);
                Ok(())
            },
        );

        assert!(result.is_ok(), "property result: {result:?}");
    }

    #[test]
    fn traversal_returns_the_same_topk_as_brute_force_at_high_ef() {
        let (encoded, rescore) = complete_graph_fixture(12);
        let graph = decode_node_blocks(encoded.as_bytes()).expect("fixture graph is valid");
        let query = vector(4.25);
        let expected = brute_force_topk(&rescore, &query, 5);
        let mut scratch = GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())
            .expect("fixture scratch");
        let mut searcher =
            GraphSearcher::new(graph, &rescore, &mut scratch).expect("fixture geometry is valid");

        let result = searcher
            .search(GraphSearchRequest::new(&query, 5, 0x19_04), None)
            .expect("high-ef traversal succeeds");
        let actual = result
            .candidates()
            .iter()
            .map(|candidate| candidate.row_id())
            .collect::<Vec<_>>();

        assert_eq!(actual, expected);
        assert_eq!(result.counters().effective_ef(), 12);
        assert_eq!(result.counters().candidates_rescored(), 12);
    }

    #[test]
    fn an_unfiltered_traversal_is_byte_identical_with_the_mask_parameter_absent() {
        let (encoded, rescore) = complete_graph_fixture(12);
        let graph = decode_node_blocks(encoded.as_bytes()).expect("fixture graph is valid");
        let query = vector(4.25);
        let request = GraphSearchRequest::new(&query, 5, 0x19_06).with_ef(12);
        let mut unfiltered_scratch =
            GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())
                .expect("unfiltered scratch");
        let mut full_mask_scratch =
            GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())
                .expect("full-mask scratch");
        let mut unfiltered = GraphSearcher::new(graph, &rescore, &mut unfiltered_scratch)
            .expect("unfiltered searcher");
        let mut full_mask = GraphSearcher::new(graph, &rescore, &mut full_mask_scratch)
            .expect("full-mask searcher");

        let expected = unfiltered.search(request, None).expect("unfiltered search");
        let actual = full_mask
            .search_filtered(
                request,
                &DocBitmap::full(graph.node_count()),
                usize::MAX,
                None,
            )
            .expect("full-mask search");
        let FilteredGraphSearchOutcome::Traversed(actual) = actual else {
            panic!("an unbounded full mask must traverse");
        };

        assert_eq!(actual.candidates(), expected.candidates());
        assert_eq!(
            actual.counters().deterministic_work(),
            expected.counters().deterministic_work()
        );
        assert_eq!(actual.candidate_sequence(), expected.candidate_sequence());
    }

    #[test]
    fn every_filtered_graph_result_satisfies_the_predicate_and_is_alive() {
        let (encoded, rescore) = complete_graph_fixture(12);
        let graph = decode_node_blocks(encoded.as_bytes()).expect("fixture graph is valid");
        let query = vector(4.0);
        let allowed = DocBitmap::from_ids([1, 2, 6, 9, 11]);
        let mut scratch = GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())
            .expect("filtered scratch");
        let mut searcher =
            GraphSearcher::new(graph, &rescore, &mut scratch).expect("filtered searcher");

        let outcome = searcher
            .search_filtered(
                GraphSearchRequest::new(&query, 5, 0x19_06).with_ef(12),
                &allowed,
                usize::MAX,
                None,
            )
            .expect("filtered graph search");
        let FilteredGraphSearchOutcome::Traversed(result) = outcome else {
            panic!("an unbounded complete graph must traverse");
        };

        assert_eq!(
            result
                .candidates()
                .iter()
                .map(|candidate| candidate.row_id())
                .collect::<Vec<_>>(),
            vec![2, 6, 1, 9, 11]
        );
        assert!(
            result
                .candidates()
                .iter()
                .all(|candidate| allowed.contains(candidate.row_id()))
        );
        assert!(
            result
                .candidates()
                .iter()
                .all(|candidate| candidate.row_id() != 4),
            "the exact nearest row is planted outside the effective alive-and-filter mask"
        );
        assert_eq!(
            result.counters().candidates_rescored(),
            allowed.cardinality() as usize
        );

        let mut seeded = crate::test_support::seeded_rng(
            "graph::search::tests::every_filtered_graph_result_satisfies_the_predicate_and_is_alive",
        );
        let mut runner = TestRunner::new(Config {
            cases: 1_024,
            rng_seed: RngSeed::Fixed(seeded.next_u64()),
            ..Config::default()
        });
        let property = runner.run(
            &(
                prop::collection::vec(any::<bool>(), 12),
                prop::collection::vec(any::<bool>(), 12),
            ),
            |(predicate_matches, alive_rows)| {
                let effective = DocBitmap::from_ids(
                    predicate_matches
                        .iter()
                        .zip(&alive_rows)
                        .enumerate()
                        .filter_map(|(row, (matches, alive))| {
                            (*matches && *alive).then_some(row as u32)
                        }),
                );
                prop_assume!(!effective.is_empty());
                let k = effective.cardinality() as usize;
                let mut scratch =
                    GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())
                        .expect("property scratch");
                let mut searcher =
                    GraphSearcher::new(graph, &rescore, &mut scratch).expect("property searcher");
                let outcome = searcher
                    .search_filtered(
                        GraphSearchRequest::new(&query, k, 0x19_0602).with_ef(12),
                        &effective,
                        usize::MAX,
                        None,
                    )
                    .expect("property filtered traversal");
                let FilteredGraphSearchOutcome::Traversed(result) = outcome else {
                    return Err(TestCaseError::fail("unbounded complete graph abandoned"));
                };
                prop_assert_eq!(result.candidates().len(), k);
                let all_valid = result.candidates().iter().all(|candidate| {
                    let row = candidate.row_id() as usize;
                    effective.contains(candidate.row_id())
                        && predicate_matches[row]
                        && alive_rows[row]
                });
                prop_assert!(all_valid);
                Ok(())
            },
        );
        assert!(property.is_ok(), "property result: {property:?}");
    }

    #[test]
    fn vector_row_checkpoint_counts_only_filter_accepted_graph_rows() {
        let (encoded, rescore) = complete_graph_fixture(12);
        let graph = decode_node_blocks(encoded.as_bytes()).expect("fixture graph is valid");
        let query = vector(4.0);
        let request = GraphSearchRequest::new(&query, 1, 0x27_24)
            .with_ef(12)
            .with_candidate_trace();
        let mut trace_scratch =
            GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())
                .expect("trace scratch");
        let mut trace =
            GraphSearcher::new(graph, &rescore, &mut trace_scratch).expect("trace searcher");
        let trace_result = trace.search(request, None).expect("trace traversal");
        let sequence = trace_result.candidate_sequence().expect("candidate trace");
        let first = *sequence.first().expect("first scored row");
        let accepted = *sequence
            .iter()
            .find(|&&row| row != first)
            .expect("later filter-accepted row");
        let allow_list = DocBitmap::from_ids([accepted]);
        let controller = crate::scan::vector_fault::VectorFaultController::armed(
            crate::scan::vector_fault::VectorFault::CancelAfterRows {
                source: crate::scan::vector_fault::VectorRowSource::Sealed([0x27; 16]),
                requested_rows: 1,
                tier: crate::scan::vector_fault::VectorSearchTier::Graph,
            },
            27_240,
        );
        let mut scratch = GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())
            .expect("fault scratch");
        let mut searcher = GraphSearcher::new(graph, &rescore, &mut scratch)
            .expect("fault searcher")
            .with_vector_fault_controller(
                &controller,
                crate::scan::vector_fault::VectorRowSource::Sealed([0x27; 16]),
                crate::scan::vector_fault::VectorSearchTier::Graph,
            );
        let error = searcher
            .search_filtered(request, &allow_list, usize::MAX, None)
            .expect_err("first filter-accepted row must cancel");
        assert_eq!(error, GraphSearchError::Cancelled { partial: false });
        controller.finalize_search(false);
        let receipts = controller.take_typed_receipts();
        assert_eq!(receipts.len(), 1);
        assert!(
            matches!(
                receipts[0].effect(),
                crate::scan::vector_fault::VectorFaultEffect::CancelledAfterRows {
                    local_row,
                    requested_rows: 1,
                    observed_rows: 1,
                    ..
                } if *local_row == accepted
            ),
            "filtered-out row {first} incorrectly consumed the eligible-row checkpoint: {:?}",
            receipts[0].effect()
        );
    }

    #[test]
    fn traversal_expands_the_nearest_frontier_node_before_insertion_order() {
        let (encoded, rescore) = best_first_graph_fixture();
        let graph = decode_node_blocks(encoded.as_bytes()).expect("fixture graph is valid");
        let query = vector(0.0);
        let mut scratch = GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())
            .expect("fixture scratch");
        let mut searcher =
            GraphSearcher::new(graph, &rescore, &mut scratch).expect("fixture geometry is valid");

        let result = searcher
            .search(
                GraphSearchRequest::new(&query, 1, 0x19_04_bf)
                    .with_ef(4)
                    .with_observed_core_class(QueryCoreClass::Performance)
                    .with_candidate_trace(),
                None,
            )
            .expect("best-first traversal reaches the hidden nearest row");

        assert_eq!(
            result
                .candidates()
                .iter()
                .map(|candidate| candidate.row_id())
                .collect::<Vec<_>>(),
            vec![8]
        );
        assert_eq!(result.candidate_sequence(), Some(&[0, 1, 2, 3, 8][..]));
        assert_eq!(result.counters().hops(), 4);
        assert_eq!(result.counters().pushes(), 5);
        assert_eq!(result.counters().core_class(), QueryCoreClass::Performance);
        assert_eq!(result.counters().candidates_rescored(), 4);
        assert!(result.counters().candidates_scored() < 9);
    }

    #[test]
    fn visited_epoch_wraps_correctly_at_255_queries() {
        let (encoded, rescore) = complete_graph_fixture(12);
        let graph = decode_node_blocks(encoded.as_bytes()).expect("fixture graph is valid");
        let query = vector(4.25);
        let mut scratch = GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())
            .expect("fixture scratch");
        let mut searcher =
            GraphSearcher::new(graph, &rescore, &mut scratch).expect("fixture geometry is valid");

        for query_index in 0..255 {
            let result = searcher
                .search(
                    GraphSearchRequest::new(&query, 5, query_index).with_ef(12),
                    None,
                )
                .expect("epoch before wrap succeeds");
            assert!(!result.counters().visited_epoch_cleared());
        }
        let wrapped = searcher
            .search(GraphSearchRequest::new(&query, 5, 255).with_ef(12), None)
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
        let mut scratch = GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())
            .expect("fixture scratch");
        let mut searcher =
            GraphSearcher::new(graph, &rescore, &mut scratch).expect("fixture geometry is valid");

        let result = searcher
            .search(
                GraphSearchRequest::new(&query, 5, 0x19_04)
                    .with_ef(12)
                    .with_candidate_trace(),
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
        let request = GraphSearchRequest::new(&query, 5, 0x19_0400)
            .with_ef(9)
            .with_candidate_trace();
        let mut first_scratch =
            GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())
                .expect("first scratch");
        let mut second_scratch =
            GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())
                .expect("second scratch");
        let mut first = GraphSearcher::new(graph, &rescore, &mut first_scratch)
            .expect("fixture geometry is valid");
        let mut second = GraphSearcher::new(graph, &rescore, &mut second_scratch)
            .expect("fixture geometry is valid");

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
        let mut enabled_scratch =
            GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())
                .expect("enabled scratch");
        let mut disabled_scratch =
            GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())
                .expect("disabled scratch");
        let mut enabled =
            GraphSearcher::new(graph, &rescore, &mut enabled_scratch).expect("enabled searcher");
        let mut disabled =
            GraphSearcher::new(graph, &rescore, &mut disabled_scratch).expect("disabled searcher");
        let enabled_result = enabled
            .search(
                GraphSearchRequest::new(&query, 5, 0x19_0400)
                    .with_ef(9)
                    .with_candidate_trace(),
                None,
            )
            .expect("prefetch-enabled traversal succeeds");
        let disabled_result = disabled
            .search(
                GraphSearchRequest::new(&query, 5, 0x19_0400)
                    .with_ef(9)
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
        let mut scratch = GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())
            .expect("fixture scratch");
        let mut searcher =
            GraphSearcher::new(graph, &rescore, &mut scratch).expect("fixture geometry is valid");

        let error = searcher
            .search(
                GraphSearchRequest::new(&query, 1, 0x19_0405).with_ef(1),
                None,
            )
            .expect_err("the visited guard must fail closed");

        assert_eq!(
            error,
            super::GraphSearchError::VisitedCapExceeded { visited: 5, cap: 4 }
        );
    }

    #[test]
    fn filtered_budget_and_fail_closed_visited_cap_fire_independently() {
        let (encoded, rescore) = chain_graph_fixture(10);
        let graph = decode_node_blocks(encoded.as_bytes()).expect("fixture graph is valid");
        let query = vector(9.0);
        let allowed = DocBitmap::full(graph.node_count());
        let request = GraphSearchRequest::new(&query, 1, 0x19_0605).with_ef(1);

        let mut budget_scratch =
            GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())
                .expect("budget scratch");
        let mut budget_searcher =
            GraphSearcher::new(graph, &rescore, &mut budget_scratch).expect("budget searcher");
        let budget_outcome = budget_searcher
            .search_filtered(request, &allowed, 2, None)
            .expect("selectivity budget is a normal filtered disposition");
        assert!(matches!(
            budget_outcome,
            FilteredGraphSearchOutcome::VisitedBudgetExceeded {
                visited: 3,
                budget: 2,
                ..
            }
        ));

        let mut invariant_scratch =
            GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())
                .expect("invariant scratch");
        let mut invariant_searcher = GraphSearcher::new(graph, &rescore, &mut invariant_scratch)
            .expect("invariant searcher");
        let error = invariant_searcher
            .search_filtered(request, &allowed, usize::MAX, None)
            .expect_err("removing the selectivity budget must expose the invariant guard");
        assert_eq!(
            error,
            GraphSearchError::VisitedCapExceeded { visited: 5, cap: 4 }
        );
    }

    #[test]
    fn cancellation_is_visible_across_threads_during_traversal() {
        let (encoded, rescore) = long_chain_graph_fixture(40_000);
        let graph = decode_node_blocks(encoded.as_bytes()).expect("fixture graph is valid");
        let query = vec![0.0_f32; DIMS];
        let mut scratch = GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())
            .expect("fixture scratch");
        let mut searcher =
            GraphSearcher::new(graph, &rescore, &mut scratch).expect("fixture geometry is valid");
        let directory = tempfile::tempdir().expect("temporary store directory");
        let store = Store::open(directory.path(), OpenOptions::default()).expect("store opens");
        let lease = store.snapshot().expect("snapshot lease");
        let token = CancelToken::new();
        let control = QueryControl::Cancel(token.clone());
        let cancellation = QueryCancellation::new(&control, &lease);
        let error = std::thread::scope(|scope| {
            let canceller = scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(2));
                token.cancel();
            });
            let result = searcher.search(
                GraphSearchRequest::new(&query, 1, 0x19_0406).with_ef(200),
                Some(&cancellation),
            );
            canceller.join().expect("canceller thread joins");
            result
        })
        .expect_err("in-flight traversal must stop");

        assert_eq!(error, super::GraphSearchError::Cancelled { partial: false });
    }

    #[test]
    fn pre_cancelled_token_is_rejected_at_search_entry() {
        let (encoded, rescore) = long_chain_graph_fixture(16);
        let graph = decode_node_blocks(encoded.as_bytes()).expect("fixture graph is valid");
        let query = vec![0.0_f32; DIMS];
        let mut scratch = GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())
            .expect("fixture scratch");
        let mut searcher =
            GraphSearcher::new(graph, &rescore, &mut scratch).expect("fixture geometry is valid");
        let directory = tempfile::tempdir().expect("temporary store directory");
        let store = Store::open(directory.path(), OpenOptions::default()).expect("store opens");
        let lease = store.snapshot().expect("snapshot lease");
        let token = CancelToken::new();
        token.cancel();
        let control = QueryControl::Cancel(token);
        let cancellation = QueryCancellation::new(&control, &lease);

        let error = searcher
            .search(
                GraphSearchRequest::new(&query, 1, 0x19_0406).with_ef(16),
                Some(&cancellation),
            )
            .expect_err("pre-cancelled search must stop at entry");

        assert_eq!(error, GraphSearchError::Cancelled { partial: false });
    }

    #[test]
    fn counting_token_cancels_after_exact_number_of_hops() {
        let (encoded, rescore) = long_chain_graph_fixture(40_000);
        let graph = decode_node_blocks(encoded.as_bytes()).expect("fixture graph is valid");
        let query = vec![0.0_f32; DIMS];
        let mut scratch = GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())
            .expect("fixture scratch");
        let mut searcher =
            GraphSearcher::new(graph, &rescore, &mut scratch).expect("fixture geometry is valid");
        let directory = tempfile::tempdir().expect("temporary store directory");
        let store = Store::open(directory.path(), OpenOptions::default()).expect("store opens");
        let lease = store.snapshot().expect("snapshot lease");
        let token = CancelToken::new();
        let observed_hops = searcher.cancel_after_hops(7, token.clone());
        let control = QueryControl::Cancel(token);
        let cancellation = QueryCancellation::new(&control, &lease);

        let error = searcher
            .search(
                GraphSearchRequest::new(&query, 1, 0x19_0406).with_ef(200),
                Some(&cancellation),
            )
            .expect_err("counting token must cancel during traversal");

        assert_eq!(error, GraphSearchError::Cancelled { partial: false });
        assert_eq!(observed_hops.get(), 7);
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
            let mut first_scratch =
                GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())
                    .expect("first scratch");
            let mut second_scratch =
                GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())
                    .expect("second scratch");
            let mut first =
                GraphSearcher::new(graph, &rescore, &mut first_scratch).expect("first searcher");
            let mut second =
                GraphSearcher::new(graph, &rescore, &mut second_scratch).expect("second searcher");
            let result1 = first
                .search(
                    GraphSearchRequest::new(&query, 5, 0x19_0408).with_ef(ef1),
                    None,
                )
                .expect("ef1 traversal");
            let result2 = second
                .search(
                    GraphSearchRequest::new(&query, 5, 0x19_0408).with_ef(ef2),
                    None,
                )
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

    fn best_first_graph_fixture() -> (crate::graph::block::EncodedNodeBlocks, Vec<f32>) {
        let values = [10.0_f32, 9.0, 8.0, 1.0, 7.0, 6.0, 5.0, 4.0, 0.0];
        let layout =
            GraphNodeLayout::new(DIMS as u32, DIMS as u32, 4).expect("best-first layout is valid");
        let rescore = values.into_iter().flat_map(vector).collect::<Vec<_>>();
        let mut codes = vec![0_u8; values.len() * DIMS.div_ceil(2)];
        let factors = rescore
            .chunks_exact(DIMS)
            .zip(codes.chunks_exact_mut(DIMS.div_ceil(2)))
            .map(|(row, destination)| {
                quantize_bit4(row, destination).expect("best-first row is finite")
            })
            .collect::<Vec<_>>();
        let neighbors = [
            vec![4, 5, 6, 7],
            Vec::new(),
            Vec::new(),
            vec![8],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ];
        let nodes = (0..values.len())
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
        .expect("best-first graph encodes");
        (encoded, rescore)
    }

    fn factor_graph_fixture(
        factors: Bit4Factors,
        flags: u8,
    ) -> crate::graph::block::EncodedNodeBlocks {
        let layout = GraphNodeLayout::new(DIMS as u32, DIMS as u32, 0)
            .expect("factor graph layout is valid");
        let codes = vec![0_u8; DIMS.div_ceil(2)];
        let node = GraphNodeBlockInput {
            codes: &codes,
            factors,
            flags,
            neighbors: &[],
        };
        encode_node_blocks(GraphNodeBlockBuild {
            layout,
            nodes: &[node],
        })
        .expect("factor graph encodes")
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
