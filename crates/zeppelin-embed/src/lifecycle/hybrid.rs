//! Bounded producers for `Store::search_hybrid`.
//!
//! Each leg supplies a candidate window and scoring ranges. An ANN window's
//! following item describes only its retained list. Producer coverage and
//! complete cross-scores must establish that a fusion stop is a certificate.

use super::stats::{Accounted, Accounting, AllocationComponent};
use crate::fusion::{
    FusionError, FusionLeg, HYBRID_WINDOW_FLOOR, HYBRID_WINDOW_PER_K, LegBounds, LegFailureKind,
    LexicalBounds, LexicalCandidate, VectorBounds, VectorCandidate,
};
use crate::ingest::{
    ActiveSegment, DocId, DocumentVersion, GlobalRowId, RowSource, SearchCandidate,
};
use crate::lifecycle::{PublishedSnapshot, QueryError, StructuredLexicalSource};
use std::sync::Arc;

/// Literal scoring calls on this caller thread, independent of work receipts.
#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
#[doc(hidden)]
pub struct HybridScoreTestObservations {
    /// Pinned rows actually read by the vector cross-scorer.
    pub vector_rows: Vec<(GlobalRowId, Option<DocumentVersion>)>,
    /// Pinned rows actually sent to the lexical candidate scorer.
    pub lexical_rows: Vec<(GlobalRowId, Option<DocumentVersion>)>,
    /// Peak simultaneous reservations for this query's cross-score cache,
    /// including both old and replacement buffers during a capacity increase.
    pub cache_peak_bytes: u64,
    /// Vector/lexical candidate capacities available before each round fills them.
    pub candidate_start_capacities: Vec<[usize; 2]>,
    /// Peak candidate scratch reservations, including a growing buffer's old allocation.
    pub candidate_peak_bytes: u64,
    /// Fusion union capacity available before each round.
    pub union_start_capacities: Vec<usize>,
    /// Fusion union high-water, including old and replacement allocations.
    pub union_peak_bytes: u64,
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static SCORE_TEST_OBSERVATIONS: std::cell::RefCell<Option<HybridScoreTestObservations>> =
        const { std::cell::RefCell::new(None) };
    static FRESH_ROUND_TEST_CONTROL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Starts an explicit caller-thread observation window; ordinary tests retain
/// no score trace and shipping builds contain no observer.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn begin_hybrid_score_test_observations() {
    FRESH_ROUND_TEST_CONTROL.with(|fresh| fresh.set(false));
    SCORE_TEST_OBSERVATIONS.with(|observations| {
        *observations.borrow_mut() = Some(HybridScoreTestObservations::default());
    });
}

/// Observes the same public query while discarding cross-score reuse before
/// every round. Producer frontiers and fusion termination remain unchanged.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn begin_hybrid_fresh_round_test_observations() {
    begin_hybrid_score_test_observations();
    FRESH_ROUND_TEST_CONTROL.with(|fresh| fresh.set(true));
}

/// Takes the calling thread's observations and disables further collection.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn take_hybrid_score_test_observations() -> HybridScoreTestObservations {
    FRESH_ROUND_TEST_CONTROL.with(|fresh| fresh.set(false));
    SCORE_TEST_OBSERVATIONS.with(|observations| observations.take().unwrap_or_default())
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn fresh_round_test_control() -> bool {
    FRESH_ROUND_TEST_CONTROL.with(std::cell::Cell::get)
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum CrossScorePolicy {
    SquaredL2F32,
    Bm25Beir,
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct CrossScoreKey {
    row: GlobalRowId,
    document: Option<DocumentVersion>,
    epoch: Option<crate::epoch::EpochIdentity>,
    policy: CrossScorePolicy,
}

#[derive(Clone, Copy)]
struct CachedScore {
    key: CrossScoreKey,
    score: f64,
}

/// Exact cross-scores owned by one pinned admission. Query vectors, lexical
/// terms and scoring parameters remain fixed for this context's lifetime.
pub(crate) struct CrossScoreCache {
    epoch: Option<crate::epoch::EpochIdentity>,
    entries: super::stats::Accounted<Vec<CachedScore>>,
    sorted: usize,
}

impl CrossScoreCache {
    pub(crate) fn new(epoch: Option<crate::epoch::EpochIdentity>) -> Self {
        Self {
            epoch,
            entries: super::stats::Accounted::unaccounted_empty(),
            sorted: 0,
        }
    }

    fn key(
        &self,
        identity: (GlobalRowId, Option<DocumentVersion>),
        policy: CrossScorePolicy,
    ) -> CrossScoreKey {
        CrossScoreKey {
            row: identity.0,
            document: identity.1,
            epoch: self.epoch,
            policy,
        }
    }

    fn get(&self, key: CrossScoreKey) -> Option<f64> {
        let sorted = self.entries.get(..self.sorted)?;
        sorted
            .binary_search_by_key(&key, |entry| entry.key)
            .ok()
            .and_then(|index| sorted.get(index))
            .map(|entry| entry.score)
    }

    fn reserve_round(
        &mut self,
        additional: usize,
        accounting: &std::sync::Arc<super::stats::Accounting>,
    ) -> Result<(), FusionError> {
        let capacity = self
            .entries
            .len()
            .checked_add(additional)
            .ok_or_else(overflow)?;
        if capacity > self.entries.capacity() {
            let mut replacement = super::stats::Accounted::try_with_capacity(
                accounting,
                capacity,
                super::stats::AllocationComponent::Temporary,
            )
            .map_err(QueryError::Store)?;
            replacement
                .extend_from_slice(&self.entries)
                .map_err(QueryError::Store)?;
            #[cfg(any(test, feature = "test-support"))]
            SCORE_TEST_OBSERVATIONS.with(|observations| {
                if let Some(observations) = observations.borrow_mut().as_mut() {
                    observations.cache_peak_bytes = observations.cache_peak_bytes.max(
                        self.entries
                            .resident_bytes()
                            .saturating_add(replacement.resident_bytes()),
                    );
                }
            });
            self.entries = replacement;
        }
        Ok(())
    }

    fn insert(&mut self, key: CrossScoreKey, score: f64) -> Result<(), FusionError> {
        self.entries
            .push(CachedScore { key, score })
            .map_err(QueryError::Store)?;
        Ok(())
    }

    fn finish_round(&mut self) -> Result<(), FusionError> {
        self.entries
            .as_mut_slice()
            .sort_unstable_by_key(|entry| entry.key);
        if self
            .entries
            .windows(2)
            .any(|pair| matches!(pair, [left, right] if left.key == right.key))
        {
            return Err(lexical_invariant(
                "hybrid cross-score cache contains a duplicate physical row/policy",
            ));
        }
        self.sorted = self.entries.len();
        Ok(())
    }
}

/// Completed producer work retained by the hybrid loop. Keeping this seam
/// independent of fusion termination lets counter receipts be tested even
/// when a particular score policy certifies its first candidate window.
pub(crate) struct HybridWork {
    pub(crate) scan: crate::scan::ScanStats,
    pub(crate) graph: crate::ingest::GraphSearchStats,
    pub(crate) lexical: crate::fts::search::SearchCounters,
    pub(crate) plans: Vec<crate::planner::SegmentPlan>,
    pub(crate) lexical_cache_hits: usize,
    pub(crate) lexical_cache_builds: usize,
    pub(crate) cross_filled_vector: usize,
    pub(crate) cross_filled_lexical: usize,
    pub(crate) vector_candidates_produced: usize,
    pub(crate) lexical_candidates_produced: usize,
}

impl HybridWork {
    pub(crate) fn new() -> Self {
        Self {
            scan: crate::scan::ScanStats {
                dims_touched: 0,
                bytes_read: 0,
                threads_used: 0,
                worker_thread_ids: Vec::new(),
            },
            graph: crate::ingest::GraphSearchStats::default(),
            lexical: crate::fts::search::SearchCounters::default(),
            plans: Vec::new(),
            lexical_cache_hits: 0,
            lexical_cache_builds: 0,
            cross_filled_vector: 0,
            cross_filled_lexical: 0,
            vector_candidates_produced: 0,
            lexical_candidates_produced: 0,
        }
    }

    pub(crate) fn complete_round(
        &mut self,
        scan: &crate::scan::ScanStats,
        graph: crate::ingest::GraphSearchStats,
        lexical: crate::fts::search::SearchCounters,
    ) -> Result<(), QueryError> {
        self.scan.dims_touched = self
            .scan
            .dims_touched
            .checked_add(scan.dims_touched)
            .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
        self.scan.bytes_read = self
            .scan
            .bytes_read
            .checked_add(scan.bytes_read)
            .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
        for worker in &scan.worker_thread_ids {
            if !self.scan.worker_thread_ids.contains(worker) {
                self.scan.worker_thread_ids.push(*worker);
            }
        }
        self.scan.threads_used = self.scan.worker_thread_ids.len();
        super::add_traversal_stats(&mut self.graph, graph)?;
        super::accumulate_search_counters(&mut self.lexical, &lexical);
        Ok(())
    }
}

/// Requested per-leg window. Both legs share one width so the stability
/// bound has exactly one (W+1)-th element per leg.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HybridWindow {
    /// Rows each producer is asked for, never above the corpus.
    pub(crate) width: usize,
}

/// Resolves the per-leg window for one hybrid round.
///
/// The bounded producers that consume it land in the next commit; the clamp
/// ships with its own test first so the window contract is pinned before a
/// caller can depend on it.
///
/// The width is `max(HYBRID_WINDOW_FLOOR, HYBRID_WINDOW_PER_K * k)` raised to
/// `k` and then clamped to the corpus, so a producer is never asked for more
/// rows than exist and never for fewer than the caller's own `k`. A `k` of
/// zero requests nothing: fusion already returns an empty result.
pub(crate) fn hybrid_window(k: usize, corpus_rows: usize) -> Result<HybridWindow, FusionError> {
    if k == 0 {
        return Ok(HybridWindow { width: 0 });
    }
    let per_k = HYBRID_WINDOW_PER_K.checked_mul(k).ok_or_else(overflow)?;
    let width = per_k.max(HYBRID_WINDOW_FLOOR).max(k).min(corpus_rows);
    Ok(HybridWindow { width })
}

fn overflow() -> FusionError {
    FusionError::from(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))
}

/// Total alive-or-tombstoned rows one hybrid query can reach. The clamp
/// target, and the width at which a window stops being a window.
pub(crate) fn corpus_rows(
    snapshot: &PublishedSnapshot,
    active: &ActiveSegment,
) -> Result<usize, FusionError> {
    snapshot
        .segments()
        .iter()
        .try_fold(active.row_count(), |total, segment| {
            usize::try_from(segment.meta().row_count)
                .ok()
                .and_then(|rows| total.checked_add(rows))
                .ok_or_else(overflow)
        })
}

/// One lexical-leg hit with the physical coordinates cross-fill needs.
pub(crate) struct LexicalHit {
    /// Lexical segment ordinal and dense row, for a direct vector row read.
    pub(crate) doc: crate::fts::search::GlobalDocId,
    /// Joined application identity, the fused key.
    pub(crate) document: Option<DocumentVersion>,
    /// Exact BM25.
    pub(crate) bm25: f64,
}

/// Cross-filled windows and the ranges used to score them.
pub(crate) struct HybridRound<'a> {
    /// Source/version payload retained through fusion only for scoped hydration.
    pub(crate) addresses: Option<super::materialize::HybridAddresses>,
    /// Vector window plus every lexical-window document's exact squared-L2.
    pub(crate) vector: &'a [VectorCandidate<Option<DocId>>],
    /// Lexical window plus every vector-window document's exact BM25.
    pub(crate) lexical: &'a [LexicalCandidate<Option<DocId>>],
    /// Producer-supplied ranges and following values. Approximate producer
    /// values are not certificates about unseen corpus rows.
    pub(crate) bounds: LegBounds,
    /// Documents the lexical window contributed to the vector list.
    pub(crate) cross_filled_vector: usize,
    /// Documents the vector window contributed to the lexical list.
    pub(crate) cross_filled_lexical: usize,
    pub(crate) lexical_counters: crate::fts::search::SearchCounters,
    /// Actual new score evaluations, excluding cached union contributions.
    pub(crate) vector_scores_computed: usize,
    pub(crate) lexical_scores_computed: usize,
}

type PhysicalIdentity = (GlobalRowId, Option<DocumentVersion>);

/// Candidate and physical-union bookkeeping retained only within one admission.
pub(crate) struct HybridRoundBuffers {
    vector_keys: Accounted<Vec<PhysicalIdentity>>,
    lexical_keys: Accounted<Vec<PhysicalIdentity>>,
    vector: Accounted<Vec<VectorCandidate<Option<DocId>>>>,
    lexical: Accounted<Vec<LexicalCandidate<Option<DocId>>>>,
    missing: Accounted<Vec<SearchCandidate>>,
    newly_missing: Accounted<Vec<SearchCandidate>>,
    resident_bytes: u64,
    peak_bytes: u64,
}

impl HybridRoundBuffers {
    pub(crate) fn new() -> Self {
        Self {
            vector_keys: Accounted::unaccounted_empty(),
            lexical_keys: Accounted::unaccounted_empty(),
            vector: Accounted::unaccounted_empty(),
            lexical: Accounted::unaccounted_empty(),
            missing: Accounted::unaccounted_empty(),
            newly_missing: Accounted::unaccounted_empty(),
            resident_bytes: 0,
            peak_bytes: 0,
        }
    }

    fn prepare(
        &mut self,
        vector: usize,
        lexical: usize,
        accounting: &Arc<Accounting>,
    ) -> Result<(), FusionError> {
        #[cfg(any(test, feature = "test-support"))]
        SCORE_TEST_OBSERVATIONS.with(|observations| {
            if let Some(observations) = observations.borrow_mut().as_mut() {
                observations
                    .candidate_start_capacities
                    .push([self.vector.capacity(), self.lexical.capacity()]);
            }
        });
        let union = vector.checked_add(lexical).ok_or_else(overflow)?;
        reserve_candidate_buffer(
            &mut self.vector_keys,
            vector,
            accounting,
            &mut self.resident_bytes,
            &mut self.peak_bytes,
        )?;
        reserve_candidate_buffer(
            &mut self.lexical_keys,
            lexical,
            accounting,
            &mut self.resident_bytes,
            &mut self.peak_bytes,
        )?;
        reserve_candidate_buffer(
            &mut self.vector,
            union,
            accounting,
            &mut self.resident_bytes,
            &mut self.peak_bytes,
        )?;
        reserve_candidate_buffer(
            &mut self.lexical,
            union,
            accounting,
            &mut self.resident_bytes,
            &mut self.peak_bytes,
        )?;
        reserve_candidate_buffer(
            &mut self.missing,
            vector,
            accounting,
            &mut self.resident_bytes,
            &mut self.peak_bytes,
        )?;
        reserve_candidate_buffer(
            &mut self.newly_missing,
            vector,
            accounting,
            &mut self.resident_bytes,
            &mut self.peak_bytes,
        )?;
        #[cfg(any(test, feature = "test-support"))]
        SCORE_TEST_OBSERVATIONS.with(|observations| {
            if let Some(observations) = observations.borrow_mut().as_mut() {
                observations.candidate_peak_bytes =
                    observations.candidate_peak_bytes.max(self.peak_bytes);
            }
        });
        Ok(())
    }
}

fn reserve_candidate_buffer<T>(
    buffer: &mut Accounted<Vec<T>>,
    capacity: usize,
    accounting: &Arc<Accounting>,
    resident: &mut u64,
    peak: &mut u64,
) -> Result<(), FusionError> {
    buffer.clear();
    let old_bytes = buffer.resident_bytes();
    buffer
        .try_reserve_total(accounting, capacity, AllocationComponent::Temporary)
        .map_err(QueryError::Store)?;
    let new_bytes = buffer.resident_bytes();
    if new_bytes != old_bytes {
        // try_reserve_total held both allocations until the move completed.
        *peak = (*peak).max(resident.checked_add(new_bytes).ok_or_else(overflow)?);
        *resident = resident
            .checked_sub(old_bytes)
            .and_then(|bytes| bytes.checked_add(new_bytes))
            .ok_or_else(overflow)?;
    }
    Ok(())
}

/// Frozen once after both first-round producers join, before any widening.
#[derive(Clone, Copy)]
pub(crate) struct HybridAnchors {
    pub(crate) vector_ceiling: Option<f64>,
    pub(crate) lexical_maximum: f64,
}

pub(crate) fn round_provenance(
    vector: &crate::ingest::SearchOutcome,
    lexical_len: usize,
    width: usize,
) -> crate::fusion::HybridProvenance {
    use crate::fusion::{CandidateCoverage, HybridProvenance, ScorePrecision};
    let exact = vector.diagnostics.exact_rescore;
    let bounded_coverage = |len| {
        if len <= width {
            CandidateCoverage::Exhaustive
        } else {
            CandidateCoverage::CertifiedBounded
        }
    };
    HybridProvenance {
        vector_precision: if exact {
            ScorePrecision::Exact
        } else {
            ScorePrecision::Estimated
        },
        vector_coverage: if vector.diagnostics.approximate || !exact {
            CandidateCoverage::Approximate
        } else {
            bounded_coverage(vector.candidates.len())
        },
        lexical_coverage: bounded_coverage(lexical_len),
        // A successfully built round contains a computed score (including an
        // explicit zero for nonmembership) for every candidate in its union.
        cross_scores_complete: true,
    }
}

pub(crate) fn fuse_round(
    query: &crate::fusion::HybridQuery,
    round: &HybridRound<'_>,
    provenance: crate::fusion::HybridProvenance,
    require_exact: bool,
    scratch: &mut crate::fusion::StoreFusionScratch<DocId>,
) -> Result<crate::fusion::FusionOutcome<DocId>, FusionError> {
    use crate::fusion::{CandidateCoverage, FusionTermination, ScorePrecision};
    #[cfg(any(test, feature = "test-support"))]
    SCORE_TEST_OBSERVATIONS.with(|observations| {
        if let Some(observations) = observations.borrow_mut().as_mut() {
            observations.union_start_capacities.push(scratch.capacity());
        }
    });
    let mut outcome = crate::fusion::fuse_store_bounded(
        query,
        round.vector,
        round.lexical,
        round.bounds,
        |document: &Option<DocId>| *document,
        |document: &Option<DocId>| *document,
        scratch,
    )?;
    #[cfg(any(test, feature = "test-support"))]
    SCORE_TEST_OBSERVATIONS.with(|observations| {
        if let Some(observations) = observations.borrow_mut().as_mut() {
            observations.union_peak_bytes = observations.union_peak_bytes.max(scratch.peak_bytes());
        }
    });
    let bounded_producers = provenance.vector_coverage != CandidateCoverage::Approximate
        && provenance.lexical_coverage != CandidateCoverage::Approximate;
    let can_certify = bounded_producers
        && provenance.vector_precision == ScorePrecision::Exact
        && provenance.cross_scores_complete;
    if !can_certify {
        // Default ANN remains a bounded approximate query. Only an explicit
        // Exact request may widen to recover missing cross-scores; never use
        // an ANN list ending as evidence that the corpus was exhausted.
        outcome.report.termination = if require_exact && bounded_producers {
            FusionTermination::WindowUnproven
        } else {
            FusionTermination::ApproximateCandidates
        };
    }
    Ok(outcome)
}

/// Builds one round's cross-filled windows from the two producers' output.
///
/// `vector` is the supplied list in ascending squared-L2 and `lexical` is
/// descending BM25. Both are truncated to `width`; the following item can
/// serve as an unseen bound only when producer coverage certifies it.
#[allow(
    clippy::too_many_arguments,
    reason = "one round names both producers' output, the pinned snapshot, and the width"
)]
pub(crate) fn build_round<'scratch>(
    snapshot: &PublishedSnapshot,
    active: &ActiveSegment,
    assembly: &super::LexicalAssembly,
    preparation: &super::prepared_lexical::PreparedLexicalQuery<'_>,
    lexical_query: super::PinnedLexicalQuery<'_>,
    query: &[f32],
    vector: &[SearchCandidate],
    anchors: HybridAnchors,
    lexical: &[LexicalHit],
    width: usize,
    cancellation: &super::QueryCancellation<'_>,
    capture_rows: bool,
    accounting: &std::sync::Arc<super::stats::Accounting>,
    scores: &mut CrossScoreCache,
    buffers: &'scratch mut HybridRoundBuffers,
) -> Result<HybridRound<'scratch>, FusionError> {
    #[cfg(any(test, feature = "test-support"))]
    if FRESH_ROUND_TEST_CONTROL.with(std::cell::Cell::get) {
        *scores = CrossScoreCache::new(scores.epoch);
    }
    let sources = &assembly.sources;
    let vector_window = vector.get(..width.min(vector.len())).unwrap_or_default();
    let lexical_window = lexical.get(..width.min(lexical.len())).unwrap_or_default();
    let vector_next = vector.get(width).map(candidate_squared_l2);
    let lexical_next = lexical.get(width).map(|hit| hit.bm25);
    // When both producers are exhausted, no later round can reuse a new
    // score. Existing entries can still serve this final round.
    let retain_scores = vector_next.is_some() || lexical_next.is_some();

    buffers.prepare(vector_window.len(), lexical_window.len(), accounting)?;
    let HybridRoundBuffers {
        vector_keys,
        lexical_keys,
        vector: vector_candidates,
        lexical: lexical_candidates,
        missing,
        newly_missing,
        ..
    } = buffers;
    for candidate in vector_window {
        vector_keys
            .push((candidate.row_id(), candidate.document()))
            .map_err(QueryError::Store)?;
    }
    for hit in lexical_window {
        lexical_keys
            .push(lexical_identity(snapshot, sources, hit)?)
            .map_err(QueryError::Store)?;
    }
    vector_keys.as_mut_slice().sort_unstable();
    lexical_keys.as_mut_slice().sort_unstable();
    vector_keys.dedup();
    lexical_keys.dedup();
    if retain_scores {
        scores.reserve_round(
            vector_window
                .len()
                .checked_add(lexical_window.len())
                .ok_or_else(overflow)?,
            accounting,
        )?;
    }

    for candidate in vector_window {
        let document = candidate.document().map(|version| version.doc_id());
        let squared_l2 = candidate_squared_l2(candidate);
        let candidate = if candidate.exact_score() {
            VectorCandidate::exact(document, squared_l2)
        } else {
            VectorCandidate::estimated(document, squared_l2)
        };
        vector_candidates
            .push(candidate)
            .map_err(QueryError::Store)?;
    }
    let mut cross_filled_vector = 0_usize;
    let mut vector_scores_computed = 0_usize;
    for hit in lexical_window {
        cancellation.check_graph().map_err(QueryError::Scan)?;
        let identity = lexical_identity(snapshot, sources, hit)?;
        if vector_keys.binary_search(&identity).is_ok() {
            continue;
        }
        let key = scores.key(identity, CrossScorePolicy::SquaredL2F32);
        let squared_l2 = match scores.get(key) {
            Some(score) => score,
            None => {
                let score = exact_squared_l2(snapshot, active, sources, query, hit.doc)?;
                if retain_scores {
                    scores.insert(key, score)?;
                }
                vector_scores_computed = vector_scores_computed.saturating_add(1);
                score
            }
        };
        vector_candidates
            .push(VectorCandidate::exact(
                hit.document.map(|version| version.doc_id()),
                squared_l2,
            ))
            .map_err(QueryError::Store)?;
        cross_filled_vector = cross_filled_vector.saturating_add(1);
    }

    for hit in lexical_window {
        lexical_candidates
            .push(LexicalCandidate::new(
                hit.document.map(|version| version.doc_id()),
                hit.bm25,
            ))
            .map_err(QueryError::Store)?;
    }
    for &candidate in vector_window {
        let identity = (candidate.row_id(), candidate.document());
        if lexical_keys.binary_search(&identity).is_ok() {
            continue;
        }
        missing.push(candidate).map_err(QueryError::Store)?;
        if scores
            .get(scores.key(identity, CrossScorePolicy::Bm25Beir))
            .is_none()
        {
            newly_missing.push(candidate).map_err(QueryError::Store)?;
        }
    }
    let (new_scores, lexical_counters) = cross_score_lexical(
        snapshot,
        active,
        assembly,
        preparation,
        lexical_query,
        lexical,
        newly_missing,
        cancellation,
    )?;
    let lexical_scores_computed = newly_missing.len();
    let mut new_scores = new_scores.into_iter();
    let cross_filled_lexical = missing.len();
    for candidate in missing.iter() {
        let key = scores.key(
            (candidate.row_id(), candidate.document()),
            CrossScorePolicy::Bm25Beir,
        );
        let score = match scores.get(key) {
            Some(score) => score,
            None => {
                // CandidateScoring returns the uncached rows in their
                // supplied order. Materialize directly instead of allocating
                // a persistent cache solely to read these values back.
                let score = new_scores.next().ok_or_else(|| {
                    lexical_invariant("hybrid candidate has no completed exact lexical score")
                })?;
                if retain_scores {
                    scores.insert(key, score)?;
                }
                score
            }
        };
        lexical_candidates
            .push(LexicalCandidate::new(
                candidate.document().map(|version| version.doc_id()),
                score,
            ))
            .map_err(QueryError::Store)?;
    }
    if new_scores.next().is_some() {
        return Err(lexical_invariant(
            "hybrid cross-scorer returned an unexpected extra lexical score",
        ));
    }
    scores.finish_round()?;

    vector_candidates
        .as_mut_slice()
        .sort_by(|left, right| left.squared_l2().total_cmp(&right.squared_l2()));
    lexical_candidates
        .as_mut_slice()
        .sort_by(|left, right| right.bm25().total_cmp(&left.bm25()));

    let vector_bounds = match vector_candidates.first() {
        None => None,
        Some(_) => {
            let maximum = match anchors.vector_ceiling {
                Some(maximum) => maximum,
                None => {
                    return Err(FusionError::Leg {
                        leg: FusionLeg::Vector,
                        kind: LegFailureKind::Invariant,
                        detail: "bounded hybrid vector leg did not report the farthest alive row"
                            .to_owned(),
                    });
                }
            };
            Some(VectorBounds {
                min_squared_l2: 0.0,
                max_squared_l2: maximum,
                next_unseen_squared_l2: vector_next,
            })
        }
    };
    let lexical_bounds = Some(LexicalBounds {
        max_bm25: anchors.lexical_maximum,
        min_bm25: 0.0,
        next_unseen_bm25: lexical_next,
    });

    let addresses = if capture_rows {
        let capacity = vector_keys
            .len()
            .checked_add(lexical_keys.len())
            .ok_or(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))?;
        let mut addresses = super::stats::Accounted::try_with_capacity(
            accounting,
            capacity,
            super::stats::AllocationComponent::Temporary,
        )
        .map_err(QueryError::Store)?;
        for &(row_id, document) in vector_keys.iter().chain(lexical_keys.iter()) {
            if let Some(version) = document {
                addresses
                    .push((
                        version.doc_id(),
                        super::materialize::ResultAddress { row_id, document },
                    ))
                    .map_err(QueryError::Store)?;
            }
        }
        addresses
            .as_mut_slice()
            .sort_unstable_by_key(|(key, _)| *key);
        for adjacent in addresses.windows(2) {
            if let [left, right] = adjacent
                && left.0 == right.0
                && left.1 != right.1
            {
                return Err(FusionError::Leg {
                    leg: FusionLeg::Vector,
                    kind: LegFailureKind::Invariant,
                    detail: "hybrid document key names conflicting ranked source versions"
                        .to_owned(),
                });
            }
        }
        Some(addresses)
    } else {
        None
    };
    Ok(HybridRound {
        addresses,
        vector: vector_candidates,
        lexical: lexical_candidates,
        bounds: LegBounds {
            vector: vector_bounds,
            lexical: lexical_bounds,
        },
        cross_filled_vector,
        cross_filled_lexical,
        lexical_counters,
        vector_scores_computed,
        lexical_scores_computed,
    })
}

fn lexical_identity(
    snapshot: &PublishedSnapshot,
    sources: &[StructuredLexicalSource],
    hit: &LexicalHit,
) -> Result<(GlobalRowId, Option<DocumentVersion>), FusionError> {
    let source = sources
        .get(hit.doc.segment as usize)
        .ok_or_else(|| lexical_invariant("hybrid lexical source ordinal is absent"))?;
    let source = match source {
        StructuredLexicalSource::Active => RowSource::Active,
        StructuredLexicalSource::Sealed(ordinal) => RowSource::Sealed(
            snapshot
                .segments()
                .get(*ordinal)
                .ok_or_else(|| lexical_invariant("hybrid sealed source is absent"))?
                .meta()
                .id,
        ),
    };
    Ok((GlobalRowId::new(source, hit.doc.row), hit.document))
}

fn lexical_invariant(detail: &str) -> FusionError {
    FusionError::Leg {
        leg: FusionLeg::Lexical,
        kind: LegFailureKind::Invariant,
        detail: detail.to_owned(),
    }
}

#[allow(clippy::too_many_arguments)]
fn cross_score_lexical(
    snapshot: &PublishedSnapshot,
    active: &ActiveSegment,
    assembly: &super::LexicalAssembly,
    preparation: &super::prepared_lexical::PreparedLexicalQuery<'_>,
    query: super::PinnedLexicalQuery<'_>,
    lexical: &[LexicalHit],
    missing: &[SearchCandidate],
    cancellation: &super::QueryCancellation<'_>,
) -> Result<(Vec<f64>, crate::fts::search::SearchCounters), FusionError> {
    use crate::fts::search::{CandidateScoring, CandidateScoringWork, GlobalDocId, SearchCounters};
    let mut scores = vec![0.0; missing.len()];
    let mut counters = SearchCounters::default();
    cancellation.check_graph().map_err(QueryError::Scan)?;
    if let super::PinnedLexicalQuery::Structured(_) = query {
        // Until combined top-k lands, this producer returns the full exact
        // expansion/phrase aggregate. Absence here is proven nonmembership.
        let complete = lexical
            .iter()
            .map(|hit| {
                Ok((
                    lexical_identity(snapshot, &assembly.sources, hit)?,
                    hit.bm25,
                ))
            })
            .collect::<Result<std::collections::BTreeMap<_, _>, FusionError>>()?;
        for (candidate, score) in missing.iter().zip(&mut scores) {
            cancellation.check_graph().map_err(QueryError::Scan)?;
            *score = complete
                .get(&(candidate.row_id(), candidate.document()))
                .copied()
                .unwrap_or(0.0);
        }
        return Ok((scores, counters));
    }
    let mut batches = vec![Vec::<(u32, usize)>::new(); assembly.index.segments().len()];
    for (position, candidate) in missing.iter().enumerate() {
        cancellation.check_graph().map_err(QueryError::Scan)?;
        let ordinal = assembly.sources.iter().position(|source| match source {
            StructuredLexicalSource::Active => candidate.row_id().source() == RowSource::Active,
            StructuredLexicalSource::Sealed(ordinal) => {
                snapshot.segments().get(*ordinal).is_some_and(|segment| {
                    candidate.row_id().source() == RowSource::Sealed(segment.meta().id)
                })
            }
        });
        // A source with no lexical index has no text contribution.
        let Some(ordinal) = ordinal else {
            continue;
        };
        let row = candidate.row_id().local_row();
        let doc = GlobalDocId {
            segment: u32::try_from(ordinal).map_err(|_| overflow())?,
            row,
        };
        let document =
            super::structured_lexical_document(snapshot, active, &assembly.sources, doc, false)
                .map_err(super::map_term_leg_document_error)?;
        if document != candidate.document()
            || !assembly
                .alive_sets
                .get(ordinal)
                .is_some_and(|alive| alive.alive_bitmap().contains(row))
        {
            return Err(lexical_invariant(
                "hybrid candidate does not match the pinned live row and revision",
            ));
        }
        batches
            .get_mut(ordinal)
            .ok_or_else(|| lexical_invariant("hybrid lexical index/source count differs"))?
            .push((row, position));
    }
    let super::PinnedLexicalQuery::Term(query) = query else {
        return Err(lexical_invariant("hybrid lexical query changed kind"));
    };
    if batches.iter().all(Vec::is_empty) {
        return Ok((scores, counters));
    }
    let prepared = CandidateScoring::prepared(preparation.term_scoring(assembly, query)?);
    let mut work = CandidateScoringWork::default();
    for (ordinal, batch) in batches
        .iter()
        .enumerate()
        .filter(|(_, batch)| !batch.is_empty())
    {
        let rows = batch.iter().map(|(row, _)| *row).collect::<Vec<_>>();
        let segment = assembly
            .index
            .segments()
            .get(ordinal)
            .ok_or_else(|| lexical_invariant("hybrid lexical segment is absent"))?;
        #[cfg(any(test, feature = "test-support"))]
        SCORE_TEST_OBSERVATIONS.with(|observations| {
            if let Some(observations) = observations.borrow_mut().as_mut() {
                for (_, position) in batch {
                    if let Some(candidate) = missing.get(*position) {
                        observations
                            .lexical_rows
                            .push((candidate.row_id(), candidate.document()));
                    }
                }
            }
        });
        let result = prepared
            .score_rows(ordinal, segment, &rows, &mut work, |_| {
                cancellation.check_graph()
            })
            .map_err(super::map_fusion_controlled_lexical_error)?;
        super::accumulate_search_counters(&mut counters, &result.counters);
        for ((_, position), score) in batch.iter().zip(result.scores) {
            *scores.get_mut(*position).ok_or_else(|| {
                lexical_invariant("hybrid candidate routing position is absent")
            })? = score.unwrap_or(0.0);
        }
    }
    cancellation.check_graph().map_err(QueryError::Scan)?;
    Ok((scores, counters))
}

fn candidate_squared_l2(candidate: &SearchCandidate) -> f64 {
    -f64::from(candidate.score())
}

/// Reads one row's full-precision vector and scores it with the same f64
/// arithmetic and f32 narrowing the exact scan uses, so a cross-filled score
/// is bit-identical to the one the unbounded leg would have produced.
fn exact_squared_l2(
    snapshot: &PublishedSnapshot,
    active: &ActiveSegment,
    sources: &[StructuredLexicalSource],
    query: &[f32],
    doc: crate::fts::search::GlobalDocId,
) -> Result<f64, FusionError> {
    let invariant = |detail: &str| FusionError::Leg {
        leg: FusionLeg::Vector,
        kind: LegFailureKind::Invariant,
        detail: detail.to_owned(),
    };
    let source = usize::try_from(doc.segment)
        .ok()
        .and_then(|slot| sources.get(slot))
        .ok_or_else(|| invariant("hybrid cross-fill named an unknown lexical segment"))?;
    let rows = match source {
        StructuredLexicalSource::Sealed(ordinal) => {
            let segment = snapshot
                .segments()
                .get(*ordinal)
                .ok_or_else(|| invariant("hybrid cross-fill named an absent sealed segment"))?;
            crate::lifecycle::exact_rescore_rows(segment).map_err(FusionError::from)?
        }
        StructuredLexicalSource::Active => active.vectors(),
    };
    let dimension = query.len();
    let row = usize::try_from(doc.row)
        .ok()
        .and_then(|row| row.checked_mul(dimension))
        .and_then(|start| start.checked_add(dimension).map(|end| start..end))
        .and_then(|range| rows.get(range))
        .ok_or_else(|| invariant("hybrid cross-fill row is outside its segment"))?;
    #[cfg(any(test, feature = "test-support"))]
    if SCORE_TEST_OBSERVATIONS.with(|observations| observations.borrow().is_some()) {
        let document = super::structured_lexical_document(snapshot, active, sources, doc, false)
            .map_err(super::map_term_leg_document_error)?;
        let key = lexical_identity(
            snapshot,
            sources,
            &LexicalHit {
                doc,
                document,
                bm25: 0.0,
            },
        )?;
        SCORE_TEST_OBSERVATIONS.with(|observations| {
            if let Some(observations) = observations.borrow_mut().as_mut() {
                observations.vector_rows.push(key);
            }
        });
    }
    let squared_l2 = crate::quant::squared_l2_f64(query, row) as f32;
    if !squared_l2.is_finite() {
        return Err(invariant(
            "hybrid cross-fill produced a non-finite distance",
        ));
    }
    Ok(f64::from(squared_l2))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use super::{HYBRID_WINDOW_FLOOR, HybridWindow, hybrid_window};
    use crate::fusion::{FusionError, LegFailureKind};

    #[test]
    fn astra_09_candidate_capacity_is_accounted_through_reuse_and_failed_growth() {
        use super::{Accounted, Accounting, HybridRoundBuffers, reserve_candidate_buffer};
        use std::sync::Arc;
        let accounting = Arc::new(Accounting::new(u64::MAX, u64::MAX));
        let mut buffers = HybridRoundBuffers::new();
        buffers.prepare(50, 50, &accounting).expect("first window");
        let first = buffers.resident_bytes;
        assert!(first > 0);
        assert_eq!(
            accounting.audit().expect("first charge").temporary_bytes,
            first
        );
        let pointer = buffers.vector.as_ptr();
        buffers
            .prepare(25, 25, &accounting)
            .expect("smaller window");
        assert_eq!(buffers.vector.as_ptr(), pointer, "same allocation reused");
        assert_eq!(buffers.resident_bytes, first);
        buffers
            .prepare(100, 100, &accounting)
            .expect("wider window");
        assert_eq!(buffers.resident_bytes, first * 2);
        assert!(
            buffers.peak_bytes > buffers.resident_bytes,
            "old allocation charged during growth"
        );
        assert_eq!(
            accounting.audit().expect("wider charge").temporary_bytes,
            buffers.resident_bytes
        );
        eprintln!(
            "first={first} retained={} growth_peak={}",
            buffers.resident_bytes, buffers.peak_bytes
        );
        drop(buffers);
        assert_eq!(accounting.audit().expect("released").temporary_bytes, 0);

        // Four old bytes plus eight replacement bytes need twelve, even though
        // only eight will remain. A failed reservation leaves the old owner intact.
        let accounting = Arc::new(Accounting::new(u64::MAX, 11));
        let mut buffer = Accounted::<Vec<u8>>::unaccounted_empty();
        let (mut resident, mut peak) = (0, 0);
        reserve_candidate_buffer(&mut buffer, 4, &accounting, &mut resident, &mut peak)
            .expect("four bytes");
        buffer.push(7).expect("one element");
        let error = reserve_candidate_buffer(&mut buffer, 8, &accounting, &mut resident, &mut peak)
            .err()
            .expect("growth refused");
        assert!(matches!(
            error,
            FusionError::Leg {
                kind: LegFailureKind::Store(crate::lifecycle::StoreErrorKind::BudgetExceeded),
                ..
            }
        ));
        assert_eq!(buffer.capacity(), 4);
        assert!(buffer.is_empty(), "failed round publishes no elements");
        assert_eq!(accounting.audit().expect("old owner").temporary_bytes, 4);
        drop(buffer);
        assert_eq!(
            accounting
                .audit()
                .expect("failed query released")
                .temporary_bytes,
            0
        );
    }

    #[test]
    fn astra_01_ann_boundary_is_not_an_unseen_certificate() {
        use crate::fusion::{
            CandidateCoverage, FusionTermination, HybridProvenance, HybridQuery, ScorePrecision,
        };
        use crate::ingest::DocId;
        // Independent raw corpus: document 3 is the nearest row but a test-only
        // ANN producer omitted it. All supplied distances remain exactly right.
        let corpus = [(1, 1.0_f64), (2, 4.0), (3, 0.0)];
        let reference = corpus
            .iter()
            .min_by(|left, right| left.1.total_cmp(&right.1))
            .expect("nonempty reference")
            .0;
        assert_eq!(reference, 3);
        for next in [None, Some(9.0)] {
            let round = super::HybridRound {
                addresses: None,
                vector: &[
                    super::VectorCandidate::exact(Some(DocId::new(1)), 1.0),
                    super::VectorCandidate::exact(Some(DocId::new(2)), 4.0),
                ],
                lexical: &[],
                bounds: super::LegBounds {
                    vector: Some(super::VectorBounds {
                        min_squared_l2: 1.0,
                        max_squared_l2: 10.0,
                        next_unseen_squared_l2: next,
                    }),
                    lexical: None,
                },
                cross_filled_vector: 0,
                cross_filled_lexical: 0,
                lexical_counters: crate::fts::search::SearchCounters::default(),
                vector_scores_computed: 0,
                lexical_scores_computed: 0,
            };
            let outcome = super::fuse_round(
                &HybridQuery::new(1),
                &round,
                HybridProvenance {
                    vector_precision: ScorePrecision::Exact,
                    vector_coverage: CandidateCoverage::Approximate,
                    lexical_coverage: CandidateCoverage::Exhaustive,
                    cross_scores_complete: true,
                },
                false,
                &mut crate::fusion::StoreFusionScratch::new(super::Arc::new(
                    super::Accounting::new(u64::MAX, u64::MAX),
                )),
            )
            .expect("ANN round");
            assert_eq!(
                outcome.hits.first().expect("supplied winner").key,
                DocId::new(1)
            );
            assert_ne!(
                outcome.hits.first().expect("supplied winner").key,
                DocId::new(reference)
            );
            assert_eq!(
                outcome.report.termination,
                FusionTermination::ApproximateCandidates,
                "an absent ANN next item or an exact retained boundary cannot certify the missing winner"
            );
        }
    }

    #[test]
    fn astra_01_exact_scores_do_not_imply_complete_candidates() {
        use crate::fusion::{CandidateCoverage, ScorePrecision};
        use crate::ingest::{
            DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
        };
        use crate::lifecycle::{
            CancelToken, OpenOptions, QueryControl, SearchOptions, SearchTier, Store,
        };
        let directory = tempfile::tempdir().expect("omission store");
        let store =
            Store::open(directory.path(), OpenOptions::default()).expect("open omission store");
        let docs = [0.0, 1.0, 2.0]
            .into_iter()
            .enumerate()
            .map(|(row, value)| {
                IngestDocument::new(
                    DocumentVersion::new(DocId::new(row as u128 + 1), Revision::new(1)),
                    vec![value, 0.0],
                )
            })
            .collect();
        store
            .ingest(IngestBatch::new(docs))
            .expect("ingest raw reference");
        let mut supplied = store
            .search(
                SearchRequest::new(&[0.0, 0.0]),
                3,
                SearchOptions::default().with_tier(SearchTier::Exact),
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect("exact supplied scores");
        // Test-only producer seam: omit the literal zero-distance winner and
        // correctly mark the remaining membership as approximate.
        supplied.candidates.remove(0);
        supplied.diagnostics.approximate = true;
        let provenance = super::round_provenance(&supplied, 0, 50);
        assert_eq!(provenance.vector_precision, ScorePrecision::Exact);
        assert_eq!(provenance.vector_coverage, CandidateCoverage::Approximate);
        assert_eq!(provenance.lexical_coverage, CandidateCoverage::Exhaustive);
        assert!(provenance.cross_scores_complete);
        store.close().expect("close omission store");
    }

    #[test]
    fn astra_00_all_hybrid_rounds_contribute_work() {
        let caller = std::thread::current().id();
        let mut work = super::HybridWork::new();
        // Independent completed receipts: three four-dimensional f32 rows,
        // then five six-dimensional f32 rows; lexical work is disjoint.
        for (dims, bytes, docs, postings, traversals, rescored) in
            [(12, 48, 3, 7, 1, 2), (30, 120, 5, 11, 2, 4)]
        {
            work.complete_round(
                &crate::scan::ScanStats {
                    dims_touched: dims,
                    bytes_read: bytes,
                    threads_used: 1,
                    worker_thread_ids: vec![caller],
                },
                crate::ingest::GraphSearchStats {
                    segments_traversed: traversals,
                    candidates_rescored: rescored,
                    ..Default::default()
                },
                crate::fts::search::SearchCounters {
                    docs_evaluated: docs,
                    postings_decoded: postings,
                    blocks_decoded: 1,
                    blocks_skipped: 2,
                },
            )
            .expect("completed round receipt");
        }
        assert_eq!(
            work.scan.dims_touched, 42,
            "both round receipts must survive"
        );
        assert_eq!(work.scan.bytes_read, 168);
        assert_eq!(work.scan.worker_thread_ids, vec![caller]);
        assert_eq!(
            work.scan.threads_used, 1,
            "count distinct executors, not round uses"
        );
        assert_eq!(work.graph.segments_traversed, 3);
        assert_eq!(work.graph.candidates_rescored, 6);
        assert_eq!(work.lexical.docs_evaluated, 8);
        assert_eq!(work.lexical.postings_decoded, 18);
        assert_eq!(work.lexical.blocks_decoded, 2);
        assert_eq!(work.lexical.blocks_skipped, 4);
    }

    #[test]
    fn hybrid_window_is_clamped_between_k_and_the_corpus() {
        assert_eq!(
            hybrid_window(10, 4_096).expect("window"),
            HybridWindow {
                width: HYBRID_WINDOW_FLOOR
            },
            "the floor dominates a small k on a large corpus"
        );
        assert_eq!(
            hybrid_window(40, 4_096).expect("window").width,
            200,
            "the per-k term dominates once it passes the floor"
        );
        assert_eq!(
            hybrid_window(3, 2).expect("window").width,
            2,
            "the corpus clamps the floor down"
        );
        assert_eq!(
            hybrid_window(4_096, 4_096).expect("window").width,
            4_096,
            "a k at the corpus size still fits inside the corpus"
        );
        assert_eq!(
            hybrid_window(0, 9).expect("window").width,
            0,
            "k of zero asks the producers for nothing"
        );
        assert!(
            matches!(
                hybrid_window(usize::MAX, 9),
                Err(FusionError::Leg {
                    kind: LegFailureKind::Scan,
                    ..
                })
            ),
            "an unrepresentable window is typed, never a panic"
        );
    }
}
