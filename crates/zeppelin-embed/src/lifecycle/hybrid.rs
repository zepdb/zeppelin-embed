//! Bounded producers for `Store::search_hybrid`.
//!
//! Each leg supplies a candidate window and scoring ranges. An ANN window's
//! following item describes only its retained list. Producer coverage and
//! complete cross-scores must establish that a fusion stop is a certificate.

use crate::fusion::{
    FusionError, FusionLeg, HYBRID_WINDOW_FLOOR, HYBRID_WINDOW_PER_K, LegBounds, LegFailureKind,
    LexicalBounds, LexicalCandidate, VectorBounds, VectorCandidate,
};
use crate::ingest::{
    ActiveSegment, DocId, DocumentVersion, GlobalRowId, RowSource, SearchCandidate,
};
use crate::lifecycle::{PublishedSnapshot, QueryError, StructuredLexicalSource};

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
pub(crate) struct HybridRound {
    /// Source/version payload retained through fusion only for scoped hydration.
    pub(crate) addresses: Option<super::materialize::HybridAddresses>,
    /// Vector window plus every lexical-window document's exact squared-L2.
    pub(crate) vector: Vec<VectorCandidate<Option<DocId>>>,
    /// Lexical window plus every vector-window document's exact BM25.
    pub(crate) lexical: Vec<LexicalCandidate<Option<DocId>>>,
    /// Producer-supplied ranges and following values. Approximate producer
    /// values are not certificates about unseen corpus rows.
    pub(crate) bounds: LegBounds,
    /// Documents the lexical window contributed to the vector list.
    pub(crate) cross_filled_vector: usize,
    /// Documents the vector window contributed to the lexical list.
    pub(crate) cross_filled_lexical: usize,
    pub(crate) lexical_counters: crate::fts::search::SearchCounters,
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
    round: &HybridRound,
    provenance: crate::fusion::HybridProvenance,
    require_exact: bool,
) -> Result<crate::fusion::FusionOutcome<DocId>, FusionError> {
    use crate::fusion::{CandidateCoverage, FusionTermination, ScorePrecision};
    let mut outcome = crate::fusion::fuse_store_bounded(
        query,
        &round.vector,
        &round.lexical,
        round.bounds,
        |document: &Option<DocId>| *document,
        |document: &Option<DocId>| *document,
    )?;
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
pub(crate) fn build_round(
    snapshot: &PublishedSnapshot,
    active: &ActiveSegment,
    assembly: &super::LexicalAssembly,
    lexical_query: super::PinnedLexicalQuery<'_>,
    query: &[f32],
    vector: &[SearchCandidate],
    anchors: HybridAnchors,
    lexical: &[LexicalHit],
    width: usize,
    cancellation: &super::QueryCancellation<'_>,
    capture_rows: bool,
    accounting: &std::sync::Arc<super::stats::Accounting>,
) -> Result<HybridRound, FusionError> {
    let sources = &assembly.sources;
    let vector_window = vector.get(..width.min(vector.len())).unwrap_or_default();
    let lexical_window = lexical.get(..width.min(lexical.len())).unwrap_or_default();
    let vector_next = vector.get(width).map(candidate_squared_l2);
    let lexical_next = lexical.get(width).map(|hit| hit.bm25);

    let vector_keys = vector_window
        .iter()
        .map(|candidate| (candidate.row_id(), candidate.document()))
        .collect::<std::collections::BTreeSet<_>>();
    let lexical_keys = lexical_window
        .iter()
        .map(|hit| lexical_identity(snapshot, sources, hit))
        .collect::<Result<std::collections::BTreeSet<_>, _>>()?;

    let mut vector_candidates = vector_window
        .iter()
        .map(|candidate| {
            let document = candidate.document().map(|version| version.doc_id());
            let squared_l2 = candidate_squared_l2(candidate);
            if candidate.exact_score() {
                VectorCandidate::exact(document, squared_l2)
            } else {
                VectorCandidate::estimated(document, squared_l2)
            }
        })
        .collect::<Vec<_>>();
    let mut cross_filled_vector = 0_usize;
    for hit in lexical_window {
        cancellation.check_graph().map_err(QueryError::Scan)?;
        if vector_keys.contains(&lexical_identity(snapshot, sources, hit)?) {
            continue;
        }
        let squared_l2 = exact_squared_l2(snapshot, active, sources, query, hit.doc)?;
        vector_candidates.push(VectorCandidate::exact(
            hit.document.map(|version| version.doc_id()),
            squared_l2,
        ));
        cross_filled_vector = cross_filled_vector.saturating_add(1);
    }

    let mut lexical_candidates = lexical_window
        .iter()
        .map(|hit| LexicalCandidate::new(hit.document.map(|version| version.doc_id()), hit.bm25))
        .collect::<Vec<_>>();
    let missing = vector_window
        .iter()
        .filter(|candidate| !lexical_keys.contains(&(candidate.row_id(), candidate.document())))
        .copied()
        .collect::<Vec<_>>();
    let (scores, lexical_counters) = cross_score_lexical(
        snapshot,
        active,
        assembly,
        lexical_query,
        lexical,
        &missing,
        cancellation,
    )?;
    let cross_filled_lexical = missing.len();
    for (candidate, score) in missing.iter().zip(scores) {
        lexical_candidates.push(LexicalCandidate::new(
            candidate.document().map(|version| version.doc_id()),
            score,
        ));
    }

    vector_candidates.sort_by(|left, right| left.squared_l2().total_cmp(&right.squared_l2()));
    lexical_candidates.sort_by(|left, right| right.bm25().total_cmp(&left.bm25()));

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
        for &(row_id, document) in vector_keys.iter().chain(&lexical_keys) {
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
    let prepared =
        CandidateScoring::new(&assembly.index, query, crate::fts::bm25::Bm25Params::beir())
            .map_err(|error| FusionError::Leg {
                leg: FusionLeg::Lexical,
                kind: LegFailureKind::Lexical,
                detail: error.to_string(),
            })?;
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
                vector: vec![
                    super::VectorCandidate::exact(Some(DocId::new(1)), 1.0),
                    super::VectorCandidate::exact(Some(DocId::new(2)), 4.0),
                ],
                lexical: Vec::new(),
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
