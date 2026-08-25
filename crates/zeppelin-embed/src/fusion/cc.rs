use std::collections::{BTreeMap, BTreeSet};

use super::normalize::{ScoreRange, degeneracies, lexical_range, vector_range};
use super::{
    ALPHA_POLICY_VERSION, FusedHit, FusionMethod, FusionOutcome, FusionReport, FusionRule,
    FusionTermination, HybridQuery, JoinedLexical, JoinedVector,
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
