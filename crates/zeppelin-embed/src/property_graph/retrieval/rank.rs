//! Vector ranking restricted to a complete, execution-owned eligibility set.
//!
//! Execution builds and charges the `EligibleNodeSet` once. This module borrows
//! its sorted full-width IDs, translates membership into per-source row masks
//! and charges only those masks, its scoring scratch and its bounded top-k to
//! the same query memory. Candidate generation is restricted before ranking:
//! an unrestricted top-k is never filtered afterwards. Every failure returns no
//! partial result.
use super::{NativeRetrievalContext, PreparedEligibility, PreparedNativeVector, RetrievalError};
use crate::graph::search::{
    FilteredGraphSearchOutcome, GraphSearchError, GraphSearchProfile, GraphSearchRequest,
    GraphSearchScratch, GraphSearcher,
};
use crate::lifecycle::SearchTier;
use crate::meta::DocBitmap;
use crate::property_graph::query::completed::{
    ActualTier, CandidateCoverage, LegState, ScorePrecision,
};
use crate::property_graph::query::plan::{SearchBounds, SearchMode};
use crate::property_graph::query::resources::{QueryArena, QueryMemory, QueryReservation};
use crate::property_graph::query::runtime::{RuntimeContext, RuntimeError, WorkKind};
use crate::property_graph::storage::search::Modality;
use crate::property_graph::storage::search::NativeVectorIndex;
use crate::property_graph::storage::tree::directory::{TreeError, TreeResources};
use crate::property_graph::{GraphRevision, NodeId};

/// Same selectivity work multiplier as the legacy filtered graph planner.
const FILTERED_VISITED_BUDGET_MULTIPLIER: usize = 2;
/// Deterministic Bit4 query preparation seed, matching `GraphSearchOptions`.
const QUERY_SEED: u64 = 0;
/// Upper bound on one roaring container's heap: array-to-bitset conversion can
/// briefly hold a full 4096-entry u16 array and an 8 KiB bitset.
const ROARING_CONTAINER_BYTES: usize = 2 * 8192;
/// Per-container descriptor width used by `DocBitmap::resident_bytes`.
const ROARING_DESCRIPTOR_BYTES: usize = 32;

#[cfg(test)]
thread_local! {
    /// Test-only selectivity budget, like the legacy planner's
    /// `visited_budget_override`: authentic native sources are small enough
    /// that the production budget covers every row, so without this the
    /// exhaustive fallback could not be exercised on real artifacts.
    static VISITED_BUDGET_OVERRIDE: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
}

/// Runs `body` with a tightened filtered-traversal visit budget.
#[cfg(test)]
pub(crate) fn with_visited_budget_override<T>(budget: usize, body: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            VISITED_BUDGET_OVERRIDE.with(|slot| slot.set(None));
        }
    }
    VISITED_BUDGET_OVERRIDE.with(|slot| slot.set(Some(budget)));
    let _reset = Reset;
    body()
}

/// One ranked eligible node. Squared-L2 is lower-first; ties order by full ID.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RankedNode {
    pub(crate) node: NodeId,
    pub(crate) revision: GraphRevision,
    pub(crate) distance: f64,
}

/// Actual route, precision and coverage established by the ranking producer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct VectorRankReport {
    /// Original preference; an omitted tier differs from explicit Auto.
    pub(crate) requested_tier: Option<SearchTier>,
    /// Actual route; absent when no eligible member existed to rank.
    pub(crate) actual_tier: Option<ActualTier>,
    pub(crate) precision: ScorePrecision,
    pub(crate) coverage: CandidateCoverage,
    /// Established from membership, never from the size of a result window.
    pub(crate) leg: LegState,
    pub(crate) live_members: u64,
    pub(crate) eligible_members: u64,
    pub(crate) sources: u64,
    /// Sources whose final candidates came from approximate graph traversal.
    pub(crate) traversed_sources: u64,
    /// Sources whose graph route fell back to exhaustive eligible scoring.
    pub(crate) fallback_count: u64,
}

/// Complete bounded ranking owned by the query memory that admitted it.
pub(crate) struct RankedVector<'m, 'g> {
    hits: Option<QueryArena<'m, 'g, RankedNode>>,
    report: VectorRankReport,
}

impl RankedVector<'_, '_> {
    pub(crate) fn hits(&self) -> &[RankedNode] {
        self.hits.as_ref().map_or(&[], QueryArena::as_slice)
    }

    pub(crate) const fn report(&self) -> VectorRankReport {
        self.report
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Route {
    Exact,
    Scan,
    Graph,
}

pub(super) const fn route(mode: SearchMode) -> (Route, Option<SearchTier>) {
    match mode {
        SearchMode::Default => (Route::Graph, None),
        SearchMode::Auto => (Route::Graph, Some(SearchTier::Auto)),
        SearchMode::Exact => (Route::Exact, Some(SearchTier::Exact)),
        SearchMode::Scan => (Route::Scan, Some(SearchTier::Scan)),
    }
}

fn better(left: &RankedNode, right: &RankedNode) -> bool {
    match left.distance.total_cmp(&right.distance) {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Greater => false,
        std::cmp::Ordering::Equal => left.node < right.node,
    }
}

pub(super) fn memory_error(
    error: crate::property_graph::query::resources::MemoryError,
) -> RetrievalError {
    RetrievalError::Control(RuntimeError::Memory(error))
}

/// Bounded best-first top-k in one fixed-capacity charged arena.
struct TopK<'m, 'g> {
    k: usize,
    hits: QueryArena<'m, 'g, RankedNode>,
}

impl<'m, 'g> TopK<'m, 'g> {
    fn new(memory: &'m QueryMemory<'g>, k: usize) -> Result<Self, RetrievalError> {
        Ok(Self {
            k,
            hits: QueryArena::new(memory, k).map_err(memory_error)?,
        })
    }

    fn offer(
        &mut self,
        hit: RankedNode,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), RetrievalError> {
        if !hit.distance.is_finite() {
            return Err(RetrievalError::Invariant("nonfinite vector distance"));
        }
        if self.hits.len() < self.k {
            self.hits.push(hit).map_err(memory_error)?;
        } else {
            let worst = self
                .hits
                .as_mut_slice()
                .last_mut()
                .ok_or(RetrievalError::Invariant("empty full top-k"))?;
            if !better(&hit, worst) {
                return Ok(());
            }
            *worst = hit;
        }
        let hits = self.hits.as_mut_slice();
        let mut position = hits.len().saturating_sub(1);
        while position > 0 {
            resources.step(1)?;
            let previous = position - 1;
            let (Some(left), Some(right)) = (hits.get(previous), hits.get(position)) else {
                return Err(RetrievalError::Invariant("top-k position"));
            };
            if !better(right, left) {
                break;
            }
            hits.swap(previous, position);
            position = previous;
        }
        Ok(())
    }
}

/// Source-local row mask charged at its maximum possible roaring capacity
/// before any insertion, so growth never escapes the query account.
struct RowMask<'m, 'g> {
    rows: DocBitmap,
    _charge: QueryReservation<'m, 'g>,
}

impl<'m, 'g> RowMask<'m, 'g> {
    fn new(memory: &'m QueryMemory<'g>, row_count: u32) -> Result<Self, RetrievalError> {
        let containers = (row_count as usize).div_ceil(1 << 16);
        let bytes = containers
            .checked_next_power_of_two()
            .and_then(|slots| slots.checked_mul(ROARING_DESCRIPTOR_BYTES))
            .and_then(|slots| {
                containers
                    .checked_mul(ROARING_CONTAINER_BYTES)
                    .and_then(|stores| slots.checked_add(stores))
            })
            .ok_or(RetrievalError::Memory)?;
        Ok(Self {
            rows: DocBitmap::new(),
            _charge: memory.reserve(bytes).map_err(memory_error)?,
        })
    }
}

pub(super) enum Eligible<'e> {
    All,
    Set(&'e [NodeId]),
}

impl Eligible<'_> {
    pub(super) fn contains(
        &self,
        node: NodeId,
        resources: &mut TreeResources<'_>,
    ) -> Result<bool, RetrievalError> {
        match self {
            Self::All => Ok(true),
            Self::Set(ids) => {
                // Binary search examines at most ceil(log2(n + 1)) entries.
                let probes = u64::from(usize::BITS - ids.len().leading_zeros());
                resources.charge_query_work(WorkKind::EligibilityEntries, probes)?;
                Ok(ids.binary_search(&node).is_ok())
            }
        }
    }
}

/// Mutable ranking progress shared by all sources of one invocation.
struct Ranking<'q, 'm, 'g> {
    memory: &'m QueryMemory<'g>,
    query: &'q [f32],
    k: usize,
    window: usize,
    top: Option<TopK<'m, 'g>>,
    exact_scratch: Option<QueryArena<'m, 'g, f32>>,
    live: u64,
    eligible: u64,
    sources: u64,
    traversed: u64,
    fallbacks: u64,
}

impl<'q, 'm, 'g> Ranking<'q, 'm, 'g> {
    fn require_window(&self, required: usize) -> Result<(), RetrievalError> {
        if required > self.window {
            return Err(RetrievalError::CandidateWindow {
                required,
                window: self.window,
            });
        }
        Ok(())
    }

    fn top(&mut self) -> Result<&mut TopK<'m, 'g>, RetrievalError> {
        if self.top.is_none() {
            self.require_window(self.k)?;
            self.top = Some(TopK::new(self.memory, self.k)?);
        }
        self.top
            .as_mut()
            .ok_or(RetrievalError::Invariant("top-k initialization"))
    }

    fn charge_original(
        &self,
        rows: usize,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), RetrievalError> {
        let coordinates = rows
            .checked_mul(self.query.len())
            .and_then(|value| u64::try_from(value).ok())
            .ok_or(RetrievalError::Memory)?;
        resources.charge_query_work(WorkKind::VectorCoordinates, coordinates)?;
        resources.charge_query_work(
            WorkKind::VectorBytes,
            coordinates.checked_mul(4).ok_or(RetrievalError::Memory)?,
        )?;
        Ok(())
    }

    /// Streams one original stored vector through bounded charged scratch and
    /// scores it with the engine's single exact squared-L2 definition.
    fn score_original<S: crate::property_graph::storage::tree::directory::BlockSource>(
        &mut self,
        node: NodeId,
        revision: u64,
        vector: crate::property_graph::storage::records::StoredVector<'_, S>,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), RetrievalError> {
        if vector.dimensions() as usize != self.query.len() {
            return Err(RetrievalError::Invariant("stored vector dimensions"));
        }
        self.charge_original(1, resources)?;
        if self.exact_scratch.is_none() {
            self.exact_scratch =
                Some(QueryArena::new(self.memory, self.query.len()).map_err(memory_error)?);
        }
        let scratch = self
            .exact_scratch
            .as_mut()
            .ok_or(RetrievalError::Invariant("exact scratch"))?;
        scratch.clear();
        for dimension in 0..vector.dimensions() {
            scratch
                .push(vector.coordinate(dimension, resources)?)
                .map_err(memory_error)?;
        }
        let distance = crate::quant::squared_l2_f64(self.query, scratch.as_slice());
        let revision =
            GraphRevision::new(revision).map_err(|_| RetrievalError::Invariant("zero revision"))?;
        self.top()?.offer(
            RankedNode {
                node,
                revision,
                distance,
            },
            resources,
        )
    }

    fn offer_row(
        &mut self,
        index: &NativeVectorIndex<'_>,
        row: u32,
        distance: f64,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), RetrievalError> {
        let (node, revision) = index.identity(row)?;
        let revision =
            GraphRevision::new(revision).map_err(|_| RetrievalError::Invariant("zero revision"))?;
        self.top()?.offer(
            RankedNode {
                node,
                revision,
                distance,
            },
            resources,
        )
    }

    /// Exhaustive exact scoring of every masked row from the authenticated
    /// original-f32 rows retained by the opened index.
    fn exact_rows(
        &mut self,
        index: &NativeVectorIndex<'_>,
        mask: &DocBitmap,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), RetrievalError> {
        let dimensions = self.query.len();
        for row in mask.iter() {
            self.charge_original(1, resources)?;
            let start = (row as usize)
                .checked_mul(dimensions)
                .ok_or(RetrievalError::Memory)?;
            let end = start
                .checked_add(dimensions)
                .ok_or(RetrievalError::Memory)?;
            let values = index
                .rescore()
                .get(start..end)
                .ok_or(RetrievalError::Invariant("native rescore row extent"))?;
            let distance = crate::quant::squared_l2_f64(self.query, values);
            self.offer_row(index, row, distance, resources)?;
        }
        Ok(())
    }

    /// Quantized estimates over every masked row; order is estimated only.
    fn scan_rows(
        &mut self,
        index: &NativeVectorIndex<'_>,
        mask: &DocBitmap,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), RetrievalError> {
        let mut charge = self
            .memory
            .reserve(self.query.len())
            .map_err(memory_error)?;
        let prepared = crate::quant::prepare_bit4_query(self.query, QUERY_SEED)
            .map_err(RetrievalError::Vector)?;
        charge
            .resize(prepared.resident_bytes())
            .map_err(memory_error)?;
        let query_norm: f64 = self
            .query
            .iter()
            .map(|value| f64::from(*value) * f64::from(*value))
            .sum();
        let coordinates = u64::try_from(self.query.len()).map_err(|_| RetrievalError::Memory)?;
        for row in mask.iter() {
            resources.charge_query_work(WorkKind::VectorCoordinates, coordinates)?;
            let factors = index.factors(row)?;
            let dot = f64::from(
                crate::quant::est_dot_bit4(&prepared, index.code(row)?, factors)
                    .map_err(RetrievalError::Vector)?,
            );
            let norm = factors.norm();
            let distance = (query_norm + norm * norm - 2.0 * dot).max(0.0);
            self.offer_row(index, row, distance, resources)?;
        }
        drop(prepared);
        drop(charge);
        Ok(())
    }

    /// Existing filtered Vamana traversal with the legacy selectivity widening
    /// and exhaustive allow-list fallback. Retained hits are exactly rescored,
    /// which fixes their distances but never proves omitted rows cannot win.
    fn graph_rows(
        &mut self,
        index: &NativeVectorIndex<'_>,
        mask: &DocBitmap,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), RetrievalError> {
        let graph = index.graph()?;
        let node_count = graph.node_count() as usize;
        let allow_count =
            usize::try_from(mask.cardinality()).map_err(|_| RetrievalError::Memory)?;
        let target_k = self.k.min(allow_count).min(node_count);
        if target_k == 0 {
            return Err(RetrievalError::Invariant(
                "graph route with empty eligible mask",
            ));
        }
        let profile = match index.profile_tag() {
            1 => GraphSearchProfile::SiftClass,
            2 => GraphSearchProfile::Angular,
            _ => return Err(RetrievalError::Invariant("native vector profile")),
        };
        let base_ef = GraphSearchRequest::new(self.query, target_k, QUERY_SEED)
            .with_profile(profile)
            .effective_ef(node_count)
            .map_err(GraphSearchError::AdaptiveEf)
            .map_err(RetrievalError::Graph)?;
        let ef_effective = base_ef
            .checked_mul(node_count)
            .map(|value| value.div_ceil(allow_count))
            .ok_or(RetrievalError::Memory)?
            .min(allow_count)
            .max(target_k);
        let max_degree = graph.layout().max_degree();
        let visited_budget = ef_effective
            .checked_mul(usize::from(max_degree))
            .and_then(|value| value.checked_mul(FILTERED_VISITED_BUDGET_MULTIPLIER))
            .ok_or(RetrievalError::Memory)?
            .min(node_count);
        #[cfg(test)]
        let visited_budget = VISITED_BUDGET_OVERRIDE
            .with(std::cell::Cell::get)
            .map_or(visited_budget, |budget| budget.min(node_count));
        let scratch_capacity = ef_effective.max(visited_budget);
        let maximum_corrective_ef = allow_count.min(scratch_capacity);
        self.require_window(scratch_capacity.max(self.k))?;
        // Reserve the scratch plus every per-call kernel allocation (padded
        // query copy, prepared Bit4 codes and the retained result list) first.
        let padded = graph.layout().padded_dims() as usize;
        let kernel_bytes =
            GraphSearchScratch::allocation_bytes(graph.node_count(), max_degree, scratch_capacity)
                .map_err(RetrievalError::Graph)?
                .checked_add(padded.checked_mul(5).ok_or(RetrievalError::Memory)?)
                .and_then(|bytes| {
                    scratch_capacity
                        .checked_mul(std::mem::size_of::<
                            crate::graph::search::GraphSearchCandidate,
                        >())
                        .and_then(|result| bytes.checked_add(result))
                })
                .ok_or(RetrievalError::Memory)?;
        let charge = self.memory.reserve(kernel_bytes).map_err(memory_error)?;
        let mut scratch =
            GraphSearchScratch::with_ef_capacity(graph.node_count(), max_degree, scratch_capacity)
                .map_err(RetrievalError::Graph)?;
        let mut searcher = GraphSearcher::with_entry_row_ids(
            graph,
            index.rescore(),
            index.seed_row_ids(),
            &mut scratch,
        )
        .map_err(RetrievalError::Graph)?;
        let mut current_ef = ef_effective;
        let traversed = loop {
            resources.step(1)?;
            let request = GraphSearchRequest::new(self.query, target_k, QUERY_SEED)
                .with_profile(profile)
                .with_ef(current_ef);
            let outcome = match searcher.search_filtered(request, mask, visited_budget, None) {
                Ok(outcome) => outcome,
                Err(GraphSearchError::FilteredCandidateShortfall { counters, .. }) => {
                    self.charge_graph(counters, resources)?;
                    break None;
                }
                Err(error) => return Err(RetrievalError::Graph(error)),
            };
            self.charge_graph(outcome.counters(), resources)?;
            match outcome {
                FilteredGraphSearchOutcome::Traversed(result)
                    if result.candidates().len() >= target_k =>
                {
                    break Some(result);
                }
                FilteredGraphSearchOutcome::Traversed(_) if current_ef < maximum_corrective_ef => {
                    current_ef = current_ef
                        .saturating_mul(2)
                        .max(current_ef.saturating_add(1))
                        .min(maximum_corrective_ef);
                }
                FilteredGraphSearchOutcome::Traversed(_)
                | FilteredGraphSearchOutcome::VisitedBudgetExceeded { .. } => break None,
            }
        };
        drop(searcher);
        drop(scratch);
        match traversed {
            Some(result) => {
                self.traversed += 1;
                for candidate in result.candidates() {
                    // Defense in depth: the kernel's retained pool must be a
                    // subset of the eligible mask; anything else is corruption.
                    if !mask.contains(candidate.row_id()) {
                        return Err(RetrievalError::Invariant("graph emitted ineligible row"));
                    }
                    self.offer_row(index, candidate.row_id(), candidate.distance(), resources)?;
                }
            }
            None => {
                self.fallbacks += 1;
                self.exact_rows(index, mask, resources)?;
            }
        }
        drop(charge);
        Ok(())
    }

    fn charge_graph(
        &self,
        counters: crate::graph::search::GraphSearchCounters,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), RetrievalError> {
        resources.charge_query_work(WorkKind::VectorCoordinates, counters.dims_touched())?;
        let rescored = counters
            .candidates_rescored()
            .checked_mul(self.query.len())
            .and_then(|value| value.checked_mul(4))
            .and_then(|value| u64::try_from(value).ok())
            .ok_or(RetrievalError::Memory)?;
        resources.charge_query_work(WorkKind::VectorBytes, rescored)?;
        Ok(())
    }
}

impl<'view, 's, 'lease, 'm, 'g> NativeRetrievalContext<'view, 's, 'lease, 'm, 'g> {
    /// Ranks the complete eligible vector population of this admitted view.
    ///
    /// `AllIndexed` ranks every live vector member; an explicit empty set ranks
    /// nothing. The selected method's retained candidate capacity must fit the
    /// caller's window. Membership, not a result window, establishes the leg.
    pub(crate) fn rank_vector(
        &self,
        prepared: &PreparedNativeVector<'_, '_>,
        bounds: SearchBounds,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<RankedVector<'m, 'g>, RetrievalError> {
        self.validate_runtime(runtime)?;
        if !std::ptr::eq(prepared.view, self.query_view) {
            return Err(RetrievalError::Storage(TreeError::Invalid(
                "foreign prepared native vector",
            )));
        }
        let (route, requested_tier) = route(prepared.mode());
        let eligible = match prepared.eligibility() {
            PreparedEligibility::AllIndexed => Eligible::All,
            PreparedEligibility::Set(ids) => Eligible::Set(ids),
        };
        let mut ranking = Ranking {
            memory: self.memory,
            query: prepared.coordinates(),
            k: bounds.k() as usize,
            window: bounds.candidate_window() as usize,
            top: None,
            exact_scratch: None,
            live: 0,
            eligible: 0,
            sources: 0,
            traversed: 0,
            fallbacks: 0,
        };
        let sparse = self.view.sparse_view(runtime)?;
        let mut resources = TreeResources::for_query(runtime)?;
        let mut sources = sparse.sources(Modality::Vector, &mut resources)?;
        while let Some(source) = sources.next(&mut resources)? {
            ranking.sources += 1;
            let mut mask = match route {
                Route::Exact => None,
                Route::Scan | Route::Graph => Some(RowMask::new(self.memory, source.row_count())?),
            };
            for ordinal in 0..source.row_count() {
                resources.step(1)?;
                let Some(member) = source.resolve_row(ordinal, &mut resources)? else {
                    continue;
                };
                ranking.live += 1;
                if !eligible.contains(member.node, &mut resources)? {
                    continue;
                }
                ranking.eligible += 1;
                match &mut mask {
                    Some(mask) => {
                        mask.rows.insert(ordinal);
                    }
                    None => {
                        let vector = member
                            .vector
                            .ok_or(RetrievalError::Invariant("vector member lacks payload"))?;
                        ranking.score_original(
                            member.node,
                            member.revision,
                            vector,
                            &mut resources,
                        )?;
                    }
                }
            }
            let Some(mask) = mask else {
                continue;
            };
            if mask.rows.is_empty() {
                continue;
            }
            let index = source
                .vector_index(&mut resources)?
                .ok_or(RetrievalError::UnindexedVectorSource)?;
            match route {
                Route::Scan => ranking.scan_rows(&index, &mask.rows, &mut resources)?,
                Route::Graph => ranking.graph_rows(&index, &mask.rows, &mut resources)?,
                Route::Exact => return Err(RetrievalError::Invariant("exact route mask")),
            }
        }
        drop(sources);
        resources.step(0)?;
        drop(resources);
        let leg = if ranking.live == 0 {
            LegState::NoIndexedPopulation
        } else if ranking.eligible == 0 {
            LegState::NoEligibleMembers
        } else {
            LegState::Nonempty
        };
        let (actual_tier, precision, coverage) = if ranking.eligible == 0 {
            // Membership proves there is nothing eligible to rank.
            (
                None,
                ScorePrecision::NotApplicable,
                CandidateCoverage::Exact,
            )
        } else {
            match route {
                Route::Exact => (
                    Some(ActualTier::Exact),
                    ScorePrecision::Original,
                    CandidateCoverage::Exact,
                ),
                Route::Scan => (
                    Some(ActualTier::Scan),
                    ScorePrecision::Quantized,
                    CandidateCoverage::Approximate,
                ),
                Route::Graph if ranking.traversed == 0 => (
                    Some(ActualTier::Exact),
                    ScorePrecision::Original,
                    CandidateCoverage::Exact,
                ),
                Route::Graph => (
                    Some(ActualTier::Graph),
                    ScorePrecision::Original,
                    CandidateCoverage::Approximate,
                ),
            }
        };
        runtime.checkpoint().map_err(RetrievalError::Control)?;
        Ok(RankedVector {
            hits: ranking.top.map(|top| top.hits),
            report: VectorRankReport {
                requested_tier,
                actual_tier,
                precision,
                coverage,
                leg,
                live_members: ranking.live,
                eligible_members: ranking.eligible,
                sources: ranking.sources,
                traversed_sources: ranking.traversed,
                fallback_count: ranking.fallbacks,
            },
        })
    }
}
