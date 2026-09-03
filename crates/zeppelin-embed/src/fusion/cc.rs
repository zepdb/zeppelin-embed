use std::collections::{BTreeMap, BTreeSet};

use super::normalize::{ScoreRange, degeneracies, lexical_range, vector_range};
use super::{
    ALPHA_POLICY_VERSION, DegenerateKind, DegenerateLeg, FusedHit, FusionError, FusionLeg,
    FusionMethod, FusionOutcome, FusionReport, FusionRule, FusionTermination, HybridQuery,
    JoinedLexical, JoinedVector, LegBounds,
};

#[derive(Clone, Copy, Default)]
struct Accumulator {
    vector_squared_l2: Option<f64>,
    lexical_bm25: Option<f64>,
    fused_score: f64,
}

pub(crate) fn fuse_joined<K>(
    query: &HybridQuery,
    vector: &[JoinedVector<K>],
    lexical: &[JoinedLexical<K>],
    alpha: f64,
    applied_rules: Vec<FusionRule>,
) -> Result<FusionOutcome<K>, super::FusionError>
where
    K: Clone + Ord,
{
    let degenerate_legs = degeneracies(vector, lexical);
    let method = if degenerate_legs.is_empty() {
        FusionMethod::ConvexCombination
    } else {
        FusionMethod::ReciprocalRankFusion
    };
    let vector_range = vector_range(vector);
    let lexical_range = lexical_range(lexical);
    let maximum_len = vector.len().max(lexical.len());

    if query.k == 0 || maximum_len == 0 {
        return Ok(FusionOutcome {
            hits: Vec::new(),
            report: FusionReport {
                method,
                effective_alpha: alpha,
                applied_rules,
                degenerate_legs,
                rounds: usize::from(maximum_len != 0),
                budget_exhausted: false,
                termination: FusionTermination::ListsExhausted,
                alpha_policy_version: ALPHA_POLICY_VERSION,
                epoch: query.epoch,
            },
        });
    }

    let mut width = query.k.max(1).min(maximum_len);
    let mut rounds = 0_usize;
    while rounds < query.max_rounds {
        rounds = rounds.saturating_add(1);
        let vector_take = width.min(vector.len());
        let lexical_take = width.min(lexical.len());
        let all_hits = score_prefix(
            vector,
            lexical,
            vector_take,
            lexical_take,
            method,
            alpha,
            vector_range,
            lexical_range,
        );
        let hits = top_k(all_hits.clone(), query.k);
        let exhausted = vector_take == vector.len() && lexical_take == lexical.len();
        if exhausted {
            return Ok(FusionOutcome {
                hits,
                report: FusionReport {
                    method,
                    effective_alpha: alpha,
                    applied_rules,
                    degenerate_legs,
                    rounds,
                    budget_exhausted: false,
                    termination: FusionTermination::ListsExhausted,
                    alpha_policy_version: ALPHA_POLICY_VERSION,
                    epoch: query.epoch,
                },
            });
        }
        if stable_bound(
            &all_hits,
            query.k,
            vector,
            lexical,
            vector_take,
            lexical_take,
            method,
            alpha,
            vector_range,
            lexical_range,
        ) {
            return Ok(FusionOutcome {
                hits,
                report: FusionReport {
                    method,
                    effective_alpha: alpha,
                    applied_rules,
                    degenerate_legs,
                    rounds,
                    budget_exhausted: false,
                    termination: FusionTermination::StableBound,
                    alpha_policy_version: ALPHA_POLICY_VERSION,
                    epoch: query.epoch,
                },
            });
        }
        let next = width.saturating_mul(2).max(width.saturating_add(1));
        width = next.min(maximum_len);
    }

    let hits = top_k(
        score_prefix(
            vector,
            lexical,
            vector.len(),
            lexical.len(),
            method,
            alpha,
            vector_range,
            lexical_range,
        ),
        query.k,
    );
    Ok(FusionOutcome {
        hits,
        report: FusionReport {
            method,
            effective_alpha: alpha,
            applied_rules,
            degenerate_legs,
            rounds,
            budget_exhausted: true,
            termination: FusionTermination::BudgetFullMaterialization,
            alpha_policy_version: ALPHA_POLICY_VERSION,
            epoch: query.epoch,
        },
    })
}

/// Fuses one cross-filled window pair against explicit producer bounds.
pub(crate) fn fuse_bounded_joined<K>(
    query: &HybridQuery,
    vector: &[JoinedVector<K>],
    lexical: &[JoinedLexical<K>],
    bounds: LegBounds,
    alpha: f64,
    applied_rules: Vec<FusionRule>,
) -> Result<FusionOutcome<K>, FusionError>
where
    K: Clone + Ord,
{
    validate_bounds(vector, lexical, bounds)?;
    let mut degenerate_legs = Vec::new();
    if let Some(kind) = bounded_vector_degeneracy(vector, bounds) {
        degenerate_legs.push(DegenerateLeg {
            leg: FusionLeg::Vector,
            kind,
        });
    }
    if let Some(kind) = bounded_lexical_degeneracy(lexical, bounds) {
        degenerate_legs.push(DegenerateLeg {
            leg: FusionLeg::Lexical,
            kind,
        });
    }
    let method = if degenerate_legs.is_empty() {
        FusionMethod::ConvexCombination
    } else {
        FusionMethod::ReciprocalRankFusion
    };
    let vector_range = bounds
        .vector
        .and_then(|leg| ScoreRange::explicit(leg.min_squared_l2, leg.max_squared_l2));
    let lexical_range = bounds
        .lexical
        .and_then(|leg| ScoreRange::explicit(leg.min_bm25, leg.max_bm25));
    let vector_next = bounds.vector.and_then(|leg| leg.next_unseen_squared_l2);
    let lexical_next = bounds.lexical.and_then(|leg| leg.next_unseen_bm25);
    let exhausted = vector_next.is_none() && lexical_next.is_none();
    let maximum_len = vector.len().max(lexical.len());

    if query.k == 0 || maximum_len == 0 {
        return Ok(FusionOutcome {
            hits: Vec::new(),
            report: bounded_report(
                query,
                method,
                alpha,
                applied_rules,
                degenerate_legs,
                if exhausted {
                    FusionTermination::ListsExhausted
                } else {
                    FusionTermination::StableBound
                },
            ),
        });
    }

    // A convex combination scores from values, so every cross-filled entry
    // carries its exact contribution. Rank fusion scores from positions, and
    // only the entries strictly better than the (W+1)-th value are provably
    // at their corpus-wide rank, so the rank branch uses that prefix alone.
    let (vector_take, lexical_take) = match method {
        FusionMethod::ConvexCombination => (vector.len(), lexical.len()),
        FusionMethod::ReciprocalRankFusion => (
            vector_prefix_len(vector, vector_next),
            lexical_prefix_len(lexical, lexical_next),
        ),
    };
    let all_hits = score_prefix(
        vector,
        lexical,
        vector_take,
        lexical_take,
        method,
        alpha,
        vector_range,
        lexical_range,
    );
    let hits = top_k(all_hits.clone(), query.k);
    if exhausted {
        return Ok(FusionOutcome {
            hits,
            report: bounded_report(
                query,
                method,
                alpha,
                applied_rules,
                degenerate_legs,
                FusionTermination::ListsExhausted,
            ),
        });
    }
    let next_vector = match (method, vector_next) {
        (_, None) => 0.0,
        (FusionMethod::ConvexCombination, Some(score)) => {
            vector_range.map_or(0.0, |range| alpha * range.vector(score))
        }
        (FusionMethod::ReciprocalRankFusion, Some(_)) => super::rrf::contribution(vector_take),
    };
    let next_lexical = match (method, lexical_next) {
        (_, None) => 0.0,
        (FusionMethod::ConvexCombination, Some(score)) => {
            lexical_range.map_or(0.0, |range| (1.0 - alpha) * range.lexical(score))
        }
        (FusionMethod::ReciprocalRankFusion, Some(_)) => super::rrf::contribution(lexical_take),
    };
    let proved = match method {
        FusionMethod::ConvexCombination => {
            cross_filled_stable(&hits, query.k, next_vector.max(0.0) + next_lexical.max(0.0))
        }
        FusionMethod::ReciprocalRankFusion => prefix_stable(
            &all_hits,
            &hits,
            query.k,
            vector,
            lexical,
            vector_take,
            lexical_take,
            vector_next.is_none(),
            lexical_next.is_none(),
            next_vector,
            next_lexical,
        ),
    };
    Ok(FusionOutcome {
        hits,
        report: bounded_report(
            query,
            method,
            alpha,
            applied_rules,
            degenerate_legs,
            if proved {
                FusionTermination::StableBound
            } else {
                FusionTermination::WindowUnproven
            },
        ),
    })
}

fn bounded_report(
    query: &HybridQuery,
    method: FusionMethod,
    alpha: f64,
    applied_rules: Vec<FusionRule>,
    degenerate_legs: Vec<DegenerateLeg>,
    termination: FusionTermination,
) -> FusionReport {
    FusionReport {
        method,
        effective_alpha: alpha,
        applied_rules,
        degenerate_legs,
        rounds: 1,
        budget_exhausted: false,
        termination,
        alpha_policy_version: ALPHA_POLICY_VERSION,
        epoch: query.epoch,
    }
}

/// Every candidate a cross-filled window can see carries both leg scores, so
/// only a document outside both windows can still enter the top-k.
fn cross_filled_stable<K>(hits: &[FusedHit<K>], k: usize, unseen_ceiling: f64) -> bool {
    if k == 0 || hits.len() < k {
        return false;
    }
    hits.get(k.saturating_sub(1))
        .is_some_and(|hit| unseen_ceiling < hit.fused_score)
}

#[allow(clippy::too_many_arguments)]
fn prefix_stable<K>(
    all_hits: &[FusedHit<K>],
    hits: &[FusedHit<K>],
    k: usize,
    vector: &[JoinedVector<K>],
    lexical: &[JoinedLexical<K>],
    vector_take: usize,
    lexical_take: usize,
    vector_exhausted: bool,
    lexical_exhausted: bool,
    next_vector: f64,
    next_lexical: f64,
) -> bool
where
    K: Clone + Ord,
{
    if hits.len() < k || k == 0 {
        return false;
    }
    let seen_vector = vector
        .iter()
        .take(vector_take)
        .map(|hit| hit.key.clone())
        .collect::<BTreeSet<_>>();
    let seen_lexical = lexical
        .iter()
        .take(lexical_take)
        .map(|hit| hit.key.clone())
        .collect::<BTreeSet<_>>();
    if hits.iter().take(k).any(|hit| {
        (!vector_exhausted && !seen_vector.contains(&hit.key))
            || (!lexical_exhausted && !seen_lexical.contains(&hit.key))
    }) {
        return false;
    }
    let Some(threshold) = hits.get(k.saturating_sub(1)).map(|hit| hit.fused_score) else {
        return false;
    };
    let top_keys = hits
        .iter()
        .take(k)
        .map(|hit| hit.key.clone())
        .collect::<BTreeSet<_>>();
    for hit in all_hits.iter().filter(|hit| !top_keys.contains(&hit.key)) {
        let mut upper = hit.fused_score;
        if !vector_exhausted && !seen_vector.contains(&hit.key) {
            upper += next_vector;
        }
        if !lexical_exhausted && !seen_lexical.contains(&hit.key) {
            upper += next_lexical;
        }
        if upper >= threshold {
            return false;
        }
    }
    next_vector + next_lexical < threshold
}

fn vector_prefix_len<K>(vector: &[JoinedVector<K>], next_unseen: Option<f64>) -> usize {
    match next_unseen {
        None => vector.len(),
        Some(bound) => vector
            .iter()
            .take_while(|hit| hit.squared_l2 < bound)
            .count(),
    }
}

fn lexical_prefix_len<K>(lexical: &[JoinedLexical<K>], next_unseen: Option<f64>) -> usize {
    match next_unseen {
        None => lexical.len(),
        Some(bound) => lexical.iter().take_while(|hit| hit.bm25 > bound).count(),
    }
}

fn bounded_vector_degeneracy<K>(
    vector: &[JoinedVector<K>],
    bounds: LegBounds,
) -> Option<DegenerateKind> {
    let Some(leg) = bounds.vector else {
        return Some(DegenerateKind::Empty);
    };
    if vector.is_empty() {
        return Some(DegenerateKind::Empty);
    }
    if vector.len() == 1 && leg.next_unseen_squared_l2.is_none() {
        return Some(DegenerateKind::SingleHit);
    }
    (leg.min_squared_l2 == leg.max_squared_l2).then_some(DegenerateKind::AllScoresEqual)
}

fn bounded_lexical_degeneracy<K>(
    lexical: &[JoinedLexical<K>],
    bounds: LegBounds,
) -> Option<DegenerateKind> {
    let Some(leg) = bounds.lexical else {
        return Some(DegenerateKind::Empty);
    };
    if lexical.is_empty() {
        return Some(DegenerateKind::Empty);
    }
    if lexical.len() == 1 && leg.next_unseen_bm25.is_none() {
        return Some(DegenerateKind::SingleHit);
    }
    (leg.min_bm25 == leg.max_bm25).then_some(DegenerateKind::AllScoresEqual)
}

fn validate_bounds<K>(
    vector: &[JoinedVector<K>],
    lexical: &[JoinedLexical<K>],
    bounds: LegBounds,
) -> Result<(), FusionError> {
    if let Some(leg) = bounds.vector {
        let invalid = |detail| FusionError::InvalidBounds {
            leg: FusionLeg::Vector,
            detail,
        };
        if !leg.min_squared_l2.is_finite() || !leg.max_squared_l2.is_finite() {
            return Err(invalid("a squared-L2 extreme is not finite"));
        }
        if leg.min_squared_l2 > leg.max_squared_l2 {
            return Err(invalid("the squared-L2 extremes are inverted"));
        }
        if vector
            .iter()
            .any(|hit| hit.squared_l2 < leg.min_squared_l2 || hit.squared_l2 > leg.max_squared_l2)
        {
            return Err(invalid("a window score lies outside the supplied extremes"));
        }
        if let Some(next) = leg.next_unseen_squared_l2
            && (!next.is_finite() || next < leg.min_squared_l2 || next > leg.max_squared_l2)
        {
            return Err(invalid(
                "the unseen bound lies outside the supplied extremes",
            ));
        }
    } else if !vector.is_empty() {
        return Err(FusionError::InvalidBounds {
            leg: FusionLeg::Vector,
            detail: "a non-empty window supplied no extremes",
        });
    }
    if let Some(leg) = bounds.lexical {
        let invalid = |detail| FusionError::InvalidBounds {
            leg: FusionLeg::Lexical,
            detail,
        };
        if !leg.min_bm25.is_finite() || !leg.max_bm25.is_finite() {
            return Err(invalid("a BM25 extreme is not finite"));
        }
        if leg.min_bm25 > leg.max_bm25 {
            return Err(invalid("the BM25 extremes are inverted"));
        }
        if lexical
            .iter()
            .any(|hit| hit.bm25 < leg.min_bm25 || hit.bm25 > leg.max_bm25)
        {
            return Err(invalid("a window score lies outside the supplied extremes"));
        }
        if let Some(next) = leg.next_unseen_bm25
            && (!next.is_finite() || next < leg.min_bm25 || next > leg.max_bm25)
        {
            return Err(invalid(
                "the unseen bound lies outside the supplied extremes",
            ));
        }
    } else if !lexical.is_empty() {
        return Err(FusionError::InvalidBounds {
            leg: FusionLeg::Lexical,
            detail: "a non-empty window supplied no extremes",
        });
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn score_prefix<K>(
    vector: &[JoinedVector<K>],
    lexical: &[JoinedLexical<K>],
    vector_take: usize,
    lexical_take: usize,
    method: FusionMethod,
    alpha: f64,
    vector_range: Option<ScoreRange>,
    lexical_range: Option<ScoreRange>,
) -> Vec<FusedHit<K>>
where
    K: Clone + Ord,
{
    let mut scores = BTreeMap::<K, Accumulator>::new();
    for (rank, hit) in vector.iter().take(vector_take).enumerate() {
        let contribution = match (method, vector_range) {
            (FusionMethod::ConvexCombination, Some(range)) => alpha * range.vector(hit.squared_l2),
            (FusionMethod::ReciprocalRankFusion, _) => super::rrf::contribution(rank),
            (FusionMethod::ConvexCombination, None) => 0.0,
        };
        let accumulator = scores.entry(hit.key.clone()).or_default();
        accumulator.vector_squared_l2 = Some(hit.squared_l2);
        accumulator.fused_score += contribution;
    }
    for (rank, hit) in lexical.iter().take(lexical_take).enumerate() {
        let contribution = match (method, lexical_range) {
            (FusionMethod::ConvexCombination, Some(range)) => {
                (1.0 - alpha) * range.lexical(hit.bm25)
            }
            (FusionMethod::ReciprocalRankFusion, _) => super::rrf::contribution(rank),
            (FusionMethod::ConvexCombination, None) => 0.0,
        };
        let accumulator = scores.entry(hit.key.clone()).or_default();
        accumulator.lexical_bm25 = Some(hit.bm25);
        accumulator.fused_score += contribution;
    }
    let mut hits = scores
        .into_iter()
        .map(|(key, accumulator)| FusedHit {
            key,
            vector_squared_l2: accumulator.vector_squared_l2,
            lexical_bm25: accumulator.lexical_bm25,
            fused_score: accumulator.fused_score,
        })
        .collect::<Vec<_>>();
    hits.sort_by(|left, right| {
        right
            .fused_score
            .total_cmp(&left.fused_score)
            .then_with(|| left.key.cmp(&right.key))
    });
    hits
}

fn top_k<K>(mut hits: Vec<FusedHit<K>>, k: usize) -> Vec<FusedHit<K>> {
    hits.truncate(k.min(hits.len()));
    hits
}

#[allow(clippy::too_many_arguments)]
fn stable_bound<K>(
    hits: &[FusedHit<K>],
    k: usize,
    vector: &[JoinedVector<K>],
    lexical: &[JoinedLexical<K>],
    vector_take: usize,
    lexical_take: usize,
    method: FusionMethod,
    alpha: f64,
    vector_range: Option<ScoreRange>,
    lexical_range: Option<ScoreRange>,
) -> bool
where
    K: Clone + Ord,
{
    if hits.len() < k || k == 0 {
        return false;
    }
    let vector_exhausted = vector_take == vector.len();
    let lexical_exhausted = lexical_take == lexical.len();
    let seen_vector = vector
        .iter()
        .take(vector_take)
        .map(|hit| hit.key.clone())
        .collect::<BTreeSet<_>>();
    let seen_lexical = lexical
        .iter()
        .take(lexical_take)
        .map(|hit| hit.key.clone())
        .collect::<BTreeSet<_>>();
    if hits.iter().take(k).any(|hit| {
        (!vector_exhausted && !seen_vector.contains(&hit.key))
            || (!lexical_exhausted && !seen_lexical.contains(&hit.key))
    }) {
        return false;
    }
    let Some(threshold) = hits.get(k.saturating_sub(1)).map(|hit| hit.fused_score) else {
        return false;
    };
    let next_vector = next_vector_bound(vector, vector_take, method, alpha, vector_range);
    let next_lexical = next_lexical_bound(lexical, lexical_take, method, alpha, lexical_range);
    let top_keys = hits
        .iter()
        .take(k)
        .map(|hit| hit.key.clone())
        .collect::<BTreeSet<_>>();
    for hit in hits.iter().filter(|hit| !top_keys.contains(&hit.key)) {
        let mut upper = hit.fused_score;
        if !vector_exhausted && !seen_vector.contains(&hit.key) {
            upper += next_vector;
        }
        if !lexical_exhausted && !seen_lexical.contains(&hit.key) {
            upper += next_lexical;
        }
        if upper >= threshold {
            return false;
        }
    }
    next_vector + next_lexical < threshold
}

fn next_vector_bound<K>(
    vector: &[JoinedVector<K>],
    take: usize,
    method: FusionMethod,
    alpha: f64,
    range: Option<ScoreRange>,
) -> f64 {
    let Some(hit) = vector.get(take) else {
        return 0.0;
    };
    match (method, range) {
        (FusionMethod::ConvexCombination, Some(range)) => alpha * range.vector(hit.squared_l2),
        (FusionMethod::ReciprocalRankFusion, _) => super::rrf::contribution(take),
        (FusionMethod::ConvexCombination, None) => 0.0,
    }
}

fn next_lexical_bound<K>(
    lexical: &[JoinedLexical<K>],
    take: usize,
    method: FusionMethod,
    alpha: f64,
    range: Option<ScoreRange>,
) -> f64 {
    let Some(hit) = lexical.get(take) else {
        return 0.0;
    };
    match (method, range) {
        (FusionMethod::ConvexCombination, Some(range)) => (1.0 - alpha) * range.lexical(hit.bm25),
        (FusionMethod::ReciprocalRankFusion, _) => super::rrf::contribution(take),
        (FusionMethod::ConvexCombination, None) => 0.0,
    }
}
