//! Bounded producers for `Store::search_hybrid`.
//!
//! The hybrid legs are windowed: each produces its own top-W plus the exact
//! extreme its normalization range needs, and fusion proves the window with
//! the (W+1)-th element. Nothing here scales with the corpus except the
//! clamp that keeps a window from exceeding it.

use crate::fusion::{
    FusionError, FusionLeg, HYBRID_WINDOW_FLOOR, HYBRID_WINDOW_PER_K, LegBounds, LegFailureKind,
    LexicalBounds, LexicalCandidate, VectorBounds, VectorCandidate,
};
use crate::ingest::{ActiveSegment, DocId, SearchCandidate};
use crate::lifecycle::{PublishedSnapshot, QueryError, StructuredLexicalSource};

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
    pub(crate) document: Option<DocId>,
    /// Exact BM25.
    pub(crate) bm25: f64,
}

/// Cross-filled windows and the exact ranges fusion validates against.
pub(crate) struct HybridRound {
    /// Vector window plus every lexical-window document's exact squared-L2.
    pub(crate) vector: Vec<VectorCandidate<Option<DocId>>>,
    /// Lexical window plus every vector-window document's exact BM25.
    pub(crate) lexical: Vec<LexicalCandidate<Option<DocId>>>,
    /// Corpus-wide extremes and the (W+1)-th values.
    pub(crate) bounds: LegBounds,
    /// Documents the lexical window contributed to the vector list.
    pub(crate) cross_filled_vector: usize,
    /// Documents the vector window contributed to the lexical list.
    pub(crate) cross_filled_lexical: usize,
}

/// Builds one round's cross-filled windows from the two producers' output.
///
/// `vector` is the producer's top-(W+1) in ascending squared-L2 and `lexical`
/// is its hits in descending BM25. Both are truncated to `width` here; the
/// element just past the window becomes that leg's unseen bound.
#[allow(
    clippy::too_many_arguments,
    reason = "one round names both producers' output, the pinned snapshot, and the width"
)]
pub(crate) fn build_round(
    snapshot: &PublishedSnapshot,
    active: &ActiveSegment,
    sources: &[StructuredLexicalSource],
    query: &[f32],
    vector: &[SearchCandidate],
    vector_ceiling: Option<f64>,
    lexical: &[LexicalHit],
    width: usize,
) -> Result<HybridRound, FusionError> {
    let vector_window = vector.get(..width.min(vector.len())).unwrap_or_default();
    let lexical_window = lexical.get(..width.min(lexical.len())).unwrap_or_default();
    let vector_next = vector.get(width).map(candidate_squared_l2);
    let lexical_next = lexical.get(width).map(|hit| hit.bm25);

    let vector_keys = vector_window
        .iter()
        .filter_map(|candidate| candidate.document().map(|version| version.doc_id()))
        .collect::<std::collections::BTreeSet<_>>();
    let lexical_keys = lexical_window
        .iter()
        .filter_map(|hit| hit.document)
        .collect::<std::collections::BTreeSet<_>>();

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
        let Some(document) = hit.document else {
            continue;
        };
        if vector_keys.contains(&document) {
            continue;
        }
        let squared_l2 = exact_squared_l2(snapshot, active, sources, query, hit.doc)?;
        vector_candidates.push(VectorCandidate::exact(Some(document), squared_l2));
        cross_filled_vector = cross_filled_vector.saturating_add(1);
    }

    let mut lexical_candidates = lexical_window
        .iter()
        .map(|hit| LexicalCandidate::new(hit.document, hit.bm25))
        .collect::<Vec<_>>();
    let mut cross_filled_lexical = 0_usize;
    for hit in lexical.get(width.min(lexical.len())..).unwrap_or_default() {
        let Some(document) = hit.document else {
            continue;
        };
        if !vector_keys.contains(&document) || lexical_keys.contains(&document) {
            continue;
        }
        lexical_candidates.push(LexicalCandidate::new(Some(document), hit.bm25));
        cross_filled_lexical = cross_filled_lexical.saturating_add(1);
    }

    vector_candidates.sort_by(|left, right| left.squared_l2().total_cmp(&right.squared_l2()));
    lexical_candidates.sort_by(|left, right| right.bm25().total_cmp(&left.bm25()));

    let vector_bounds = match vector_window.first() {
        None => None,
        Some(best) => {
            let minimum = candidate_squared_l2(best);
            let maximum = match vector_ceiling {
                Some(maximum) => maximum,
                None if vector_next.is_none() => {
                    vector_window.last().map_or(minimum, candidate_squared_l2)
                }
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
                min_squared_l2: minimum,
                max_squared_l2: maximum,
                next_unseen_squared_l2: vector_next,
            })
        }
    };
    let lexical_bounds = match (lexical_window.first(), lexical.last()) {
        (Some(best), Some(worst)) => Some(LexicalBounds {
            max_bm25: best.bm25,
            min_bm25: worst.bm25,
            next_unseen_bm25: lexical_next,
        }),
        _ => None,
    };

    Ok(HybridRound {
        vector: vector_candidates,
        lexical: lexical_candidates,
        bounds: LegBounds {
            vector: vector_bounds,
            lexical: lexical_bounds,
        },
        cross_filled_vector,
        cross_filled_lexical,
    })
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
