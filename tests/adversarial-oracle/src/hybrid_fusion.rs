//! Independent hybrid-fusion math and campaign checks.
//!
//! The product supplies primitive ranked leg scores. This module owns the
//! normalization, fusion, ordering, bounded-round replay, and RRF oracle. It
//! deliberately remains std-only and does not depend on product code.

use std::collections::{BTreeMap, BTreeSet};

pub const I45_CHECKER_ID: &str = "hybrid.i45.provenance.v2";
pub const I46_CHECKER_ID: &str = "hybrid.i46.normalization.v2";
pub const I47_CHECKER_ID: &str = "hybrid.i47.bounded.v2";
pub const I48_CHECKER_ID: &str = "hybrid.i48.rrf.v2";
pub const I49_CHECKER_ID: &str = "hybrid.i49.legs.v2";

const RRF_K: f64 = 60.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FusionMethodFact {
    ConvexCombination,
    ReciprocalRankFusion,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FusionTerminationFact {
    StableBound,
    ListsExhausted,
    BudgetFullMaterialization,
    /// The store widened to the whole corpus and still could not prove the
    /// window stable. The oracle never predicts this; observing it is a
    /// finding, not a pass.
    WindowUnproven,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RankedScore {
    pub id: u64,
    pub score_bits: u64,
}

impl RankedScore {
    #[must_use]
    pub const fn new(id: u64, score_bits: u64) -> Self {
        Self { id, score_bits }
    }

    fn score(self) -> f64 {
        f64::from_bits(self.score_bits)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FusionCaseInput {
    pub k: usize,
    pub alpha_bits: u64,
    pub max_rounds: usize,
    pub vector: Vec<RankedScore>,
    pub lexical: Vec<RankedScore>,
}

impl FusionCaseInput {
    fn alpha(&self) -> f64 {
        f64::from_bits(self.alpha_bits)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FusedHitFact {
    pub id: u64,
    pub vector_squared_l2_bits: Option<u64>,
    pub lexical_bm25_bits: Option<u64>,
    pub fused_score_bits: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FusionReportFact {
    pub method: FusionMethodFact,
    pub rounds: usize,
    pub budget_exhausted: bool,
    pub termination: FusionTerminationFact,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FusionCaseObserved {
    pub hits: Vec<FusedHitFact>,
    pub report: FusionReportFact,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FusionLegFact {
    Vector,
    Lexical,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegFailureFact {
    Panic,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LegFaultObserved {
    pub leg: FusionLegFact,
    pub kind: LegFailureFact,
    pub detail: String,
    pub no_partial: bool,
    pub both_legs_completed: bool,
    pub retry: FusionCaseObserved,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HybridInput {
    pub generated_docs: usize,
    pub deleted_docs: usize,
    pub active_docs: usize,
    pub dimensions: usize,
    pub fixture_ids: Vec<u64>,
    pub main: FusionCaseInput,
    pub rrf: FusionCaseInput,
    pub single: FusionCaseInput,
    pub empty: FusionCaseInput,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HybridObserved {
    pub main: FusionCaseObserved,
    pub rrf: FusionCaseObserved,
    pub single: FusionCaseObserved,
    pub empty: FusionCaseObserved,
    pub leg_faults: Vec<LegFaultObserved>,
    pub same_seed_control_passed: bool,
}

#[derive(Clone, Copy, Default)]
struct Accumulator {
    vector_squared_l2_bits: Option<u64>,
    lexical_bm25_bits: Option<u64>,
    fused_score: f64,
}

#[derive(Clone, Copy)]
struct ScoreRange {
    minimum: f64,
    maximum: f64,
}

impl ScoreRange {
    fn vector(self, squared_l2: f64) -> f64 {
        (self.maximum - squared_l2) / (self.maximum - self.minimum)
    }

    fn lexical(self, bm25: f64) -> f64 {
        (bm25 - self.minimum) / (self.maximum - self.minimum)
    }
}

fn fail(checker: &str, detail: impl std::fmt::Display) -> Result<(), String> {
    Err(format!("{checker}: {detail}"))
}

fn range(values: &[RankedScore]) -> Option<ScoreRange> {
    let first = values.first()?.score();
    let last = values.last()?.score();
    (first != last).then_some(ScoreRange {
        minimum: first.min(last),
        maximum: first.max(last),
    })
}

fn method(input: &FusionCaseInput) -> FusionMethodFact {
    if range(&input.vector).is_some() && range(&input.lexical).is_some() {
        FusionMethodFact::ConvexCombination
    } else {
        FusionMethodFact::ReciprocalRankFusion
    }
}

fn rrf_contribution(rank: usize) -> f64 {
    1.0 / (RRF_K + rank.saturating_add(1) as f64)
}

fn score_prefix(
    input: &FusionCaseInput,
    vector_take: usize,
    lexical_take: usize,
    selected_method: FusionMethodFact,
) -> Vec<FusedHitFact> {
    let alpha = input.alpha();
    let vector_range = range(&input.vector);
    let lexical_range = range(&input.lexical);
    let mut scores = BTreeMap::<u64, Accumulator>::new();
    for (rank, hit) in input.vector.iter().take(vector_take).enumerate() {
        let contribution = match (selected_method, vector_range) {
            (FusionMethodFact::ConvexCombination, Some(score_range)) => {
                alpha * score_range.vector(hit.score())
            }
            (FusionMethodFact::ReciprocalRankFusion, _) => rrf_contribution(rank),
            (FusionMethodFact::ConvexCombination, None) => 0.0,
        };
        let accumulator = scores.entry(hit.id).or_default();
        accumulator.vector_squared_l2_bits = Some(hit.score_bits);
        accumulator.fused_score += contribution;
    }
    for (rank, hit) in input.lexical.iter().take(lexical_take).enumerate() {
        let contribution = match (selected_method, lexical_range) {
            (FusionMethodFact::ConvexCombination, Some(score_range)) => {
                (1.0 - alpha) * score_range.lexical(hit.score())
            }
            (FusionMethodFact::ReciprocalRankFusion, _) => rrf_contribution(rank),
            (FusionMethodFact::ConvexCombination, None) => 0.0,
        };
        let accumulator = scores.entry(hit.id).or_default();
        accumulator.lexical_bm25_bits = Some(hit.score_bits);
        accumulator.fused_score += contribution;
    }
    let mut hits = scores
        .into_iter()
        .map(|(id, accumulator)| FusedHitFact {
            id,
            vector_squared_l2_bits: accumulator.vector_squared_l2_bits,
            lexical_bm25_bits: accumulator.lexical_bm25_bits,
            fused_score_bits: accumulator.fused_score.to_bits(),
        })
        .collect::<Vec<_>>();
    hits.sort_by(|left, right| {
        f64::from_bits(right.fused_score_bits)
            .total_cmp(&f64::from_bits(left.fused_score_bits))
            .then_with(|| left.id.cmp(&right.id))
    });
    hits
}

fn next_vector_bound(
    input: &FusionCaseInput,
    take: usize,
    selected_method: FusionMethodFact,
) -> f64 {
    let Some(hit) = input.vector.get(take) else {
        return 0.0;
    };
    match (selected_method, range(&input.vector)) {
        (FusionMethodFact::ConvexCombination, Some(score_range)) => {
            input.alpha() * score_range.vector(hit.score())
        }
        (FusionMethodFact::ReciprocalRankFusion, _) => rrf_contribution(take),
        (FusionMethodFact::ConvexCombination, None) => 0.0,
    }
}

fn next_lexical_bound(
    input: &FusionCaseInput,
    take: usize,
    selected_method: FusionMethodFact,
) -> f64 {
    let Some(hit) = input.lexical.get(take) else {
        return 0.0;
    };
    match (selected_method, range(&input.lexical)) {
        (FusionMethodFact::ConvexCombination, Some(score_range)) => {
            (1.0 - input.alpha()) * score_range.lexical(hit.score())
        }
        (FusionMethodFact::ReciprocalRankFusion, _) => rrf_contribution(take),
        (FusionMethodFact::ConvexCombination, None) => 0.0,
    }
}

fn stable_bound(
    input: &FusionCaseInput,
    hits: &[FusedHitFact],
    vector_take: usize,
    lexical_take: usize,
    selected_method: FusionMethodFact,
) -> bool {
    if hits.len() < input.k || input.k == 0 {
        return false;
    }
    let vector_exhausted = vector_take == input.vector.len();
    let lexical_exhausted = lexical_take == input.lexical.len();
    let seen_vector = input
        .vector
        .iter()
        .take(vector_take)
        .map(|hit| hit.id)
        .collect::<BTreeSet<_>>();
    let seen_lexical = input
        .lexical
        .iter()
        .take(lexical_take)
        .map(|hit| hit.id)
        .collect::<BTreeSet<_>>();
    if hits.iter().take(input.k).any(|hit| {
        (!vector_exhausted && !seen_vector.contains(&hit.id))
            || (!lexical_exhausted && !seen_lexical.contains(&hit.id))
    }) {
        return false;
    }
    let Some(threshold) = hits
        .get(input.k.saturating_sub(1))
        .map(|hit| f64::from_bits(hit.fused_score_bits))
    else {
        return false;
    };
    let next_vector = next_vector_bound(input, vector_take, selected_method);
    let next_lexical = next_lexical_bound(input, lexical_take, selected_method);
    let top_keys = hits
        .iter()
        .take(input.k)
        .map(|hit| hit.id)
        .collect::<BTreeSet<_>>();
    for hit in hits.iter().filter(|hit| !top_keys.contains(&hit.id)) {
        let mut upper = f64::from_bits(hit.fused_score_bits);
        if !vector_exhausted && !seen_vector.contains(&hit.id) {
            upper += next_vector;
        }
        if !lexical_exhausted && !seen_lexical.contains(&hit.id) {
            upper += next_lexical;
        }
        if upper >= threshold {
            return false;
        }
    }
    next_vector + next_lexical < threshold
}

fn expected_case(input: &FusionCaseInput) -> FusionCaseObserved {
    let selected_method = method(input);
    let maximum_len = input.vector.len().max(input.lexical.len());
    if input.k == 0 || maximum_len == 0 {
        return FusionCaseObserved {
            hits: Vec::new(),
            report: FusionReportFact {
                method: selected_method,
                rounds: usize::from(maximum_len != 0),
                budget_exhausted: false,
                termination: FusionTerminationFact::ListsExhausted,
            },
        };
    }

    let mut width = input.k.max(1).min(maximum_len);
    let mut rounds = 0_usize;
    while rounds < input.max_rounds {
        rounds = rounds.saturating_add(1);
        let vector_take = width.min(input.vector.len());
        let lexical_take = width.min(input.lexical.len());
        let all_hits = score_prefix(input, vector_take, lexical_take, selected_method);
        let exhausted = vector_take == input.vector.len() && lexical_take == input.lexical.len();
        if exhausted || stable_bound(input, &all_hits, vector_take, lexical_take, selected_method) {
            let mut hits = all_hits;
            hits.truncate(input.k.min(hits.len()));
            return FusionCaseObserved {
                hits,
                report: FusionReportFact {
                    method: selected_method,
                    rounds,
                    budget_exhausted: false,
                    termination: if exhausted {
                        FusionTerminationFact::ListsExhausted
                    } else {
                        FusionTerminationFact::StableBound
                    },
                },
            };
        }
        width = width
            .saturating_mul(2)
            .max(width.saturating_add(1))
            .min(maximum_len);
    }

    let mut hits = score_prefix(
        input,
        input.vector.len(),
        input.lexical.len(),
        selected_method,
    );
    hits.truncate(input.k.min(hits.len()));
    FusionCaseObserved {
        hits,
        report: FusionReportFact {
            method: selected_method,
            rounds,
            budget_exhausted: true,
            termination: FusionTerminationFact::BudgetFullMaterialization,
        },
    }
}

fn score_bits_by_id(values: &[RankedScore], id: u64) -> Option<u64> {
    values
        .iter()
        .find(|candidate| candidate.id == id)
        .map(|candidate| candidate.score_bits)
}

fn guard_fixture(input: &HybridInput, checker: &str) -> Result<(), String> {
    if !(40..=200).contains(&input.generated_docs) {
        return fail(checker, "fixture document count is outside 40..=200");
    }
    if !matches!(input.dimensions, 8 | 32) {
        return fail(checker, "fixture dimensions are not 8 or 32");
    }
    if input.deleted_docs.saturating_mul(100) < input.generated_docs.saturating_mul(5)
        || input.deleted_docs.saturating_mul(100) > input.generated_docs.saturating_mul(10)
    {
        return fail(checker, "fixture deletion ratio is outside 5..=10 percent");
    }
    if !(3..=8).contains(&input.active_docs) {
        return fail(checker, "fixture did not retain a few active documents");
    }
    if input.fixture_ids.iter().any(|id| *id >= 1_u64 << 53) {
        return fail(
            checker,
            "fixture document id is not exactly representable below 2^53",
        );
    }
    if input.fixture_ids.windows(2).all(|pair| pair[0] < pair[1]) {
        return fail(checker, "fixture document ids are monotone");
    }
    let vector_ids = input
        .main
        .vector
        .iter()
        .map(|hit| hit.id)
        .collect::<BTreeSet<_>>();
    let lexical_ids = input
        .main
        .lexical
        .iter()
        .map(|hit| hit.id)
        .collect::<BTreeSet<_>>();
    let union = vector_ids.union(&lexical_ids).count();
    if union <= input.main.k || input.main.vector.is_empty() || input.main.lexical.is_empty() {
        return fail(
            checker,
            "fixture does not have two non-empty legs with union > k",
        );
    }
    if vector_ids.difference(&lexical_ids).next().is_none()
        || vector_ids.intersection(&lexical_ids).next().is_none()
    {
        return fail(
            checker,
            "fixture lacks vector-only or overlapping candidates",
        );
    }
    if !input
        .main
        .vector
        .windows(2)
        .any(|pair| pair[0].score_bits == pair[1].score_bits)
    {
        return fail(checker, "fixture lacks an exact vector tie");
    }
    if !input
        .main
        .lexical
        .windows(2)
        .any(|pair| pair[0].score_bits == pair[1].score_bits)
    {
        return fail(checker, "fixture lacks an equal BM25 tie");
    }
    if input.rrf.lexical.len() < 2
        || input
            .rrf
            .lexical
            .windows(2)
            .any(|pair| pair[0].score_bits != pair[1].score_bits)
    {
        return fail(checker, "RRF fixture lexical leg is not all-equal");
    }
    if input.single.lexical.len() != 1 {
        return fail(
            checker,
            "single-candidate fixture does not have exactly one lexical hit",
        );
    }
    if !input.empty.lexical.is_empty() {
        return fail(checker, "empty-leg fixture unexpectedly has lexical hits");
    }
    Ok(())
}

fn compare_provenance(
    label: &str,
    input: &FusionCaseInput,
    observed: &FusionCaseObserved,
) -> Result<(), String> {
    for hit in &observed.hits {
        let vector = score_bits_by_id(&input.vector, hit.id);
        let lexical = score_bits_by_id(&input.lexical, hit.id);
        if hit.vector_squared_l2_bits != vector || hit.lexical_bm25_bits != lexical {
            return fail(
                I45_CHECKER_ID,
                format_args!("{label} raw leg provenance differs for document {}", hit.id),
            );
        }
    }
    Ok(())
}

pub fn compare_i45(input: &HybridInput, observed: &HybridObserved) -> Result<(), String> {
    guard_fixture(input, I45_CHECKER_ID)?;
    compare_provenance("convex", &input.main, &observed.main)?;
    compare_provenance("all-equal", &input.rrf, &observed.rrf)?;
    compare_provenance("single-candidate", &input.single, &observed.single)?;
    compare_provenance("empty-leg", &input.empty, &observed.empty)
}

fn compare_scores(
    checker: &str,
    label: &str,
    input: &FusionCaseInput,
    observed: &FusionCaseObserved,
) -> Result<(), String> {
    let expected = expected_case(input);
    if observed.hits.len() != expected.hits.len() {
        return fail(
            checker,
            format_args!(
                "{label} returned {} hits; expected {}",
                observed.hits.len(),
                expected.hits.len()
            ),
        );
    }
    for hit in &observed.hits {
        let Some(expected_hit) = expected
            .hits
            .iter()
            .find(|candidate| candidate.id == hit.id)
        else {
            return fail(
                checker,
                format_args!("{label} returned unexpected document {}", hit.id),
            );
        };
        if hit.fused_score_bits != expected_hit.fused_score_bits {
            let vector_rank = input
                .vector
                .iter()
                .position(|candidate| candidate.id == hit.id);
            let lexical_rank = input
                .lexical
                .iter()
                .position(|candidate| candidate.id == hit.id);
            return fail(
                checker,
                format_args!(
                    "{label} fused score differs for document {} at vector rank {vector_rank:?} lexical rank {lexical_rank:?}: expected {expected_hit:?} observed {hit:?}",
                    hit.id,
                ),
            );
        }
        if !f64::from_bits(hit.fused_score_bits).is_finite() {
            return fail(checker, format_args!("{label} produced a non-finite score"));
        }
    }
    Ok(())
}

pub fn compare_i46(input: &HybridInput, observed: &HybridObserved) -> Result<(), String> {
    guard_fixture(input, I46_CHECKER_ID)?;
    compare_scores(I46_CHECKER_ID, "convex", &input.main, &observed.main)?;
    compare_scores(I46_CHECKER_ID, "all-equal", &input.rrf, &observed.rrf)?;
    compare_scores(
        I46_CHECKER_ID,
        "single-candidate",
        &input.single,
        &observed.single,
    )?;
    compare_scores(I46_CHECKER_ID, "empty-leg", &input.empty, &observed.empty)
}

pub fn compare_i47(input: &HybridInput, observed: &HybridObserved) -> Result<(), String> {
    guard_fixture(input, I47_CHECKER_ID)?;
    let expected = expected_case(&input.main);
    let union = input
        .main
        .vector
        .iter()
        .chain(&input.main.lexical)
        .map(|hit| hit.id)
        .collect::<BTreeSet<_>>()
        .len();
    if observed.main.hits.len() != input.main.k.min(union) {
        return fail(I47_CHECKER_ID, "bounded fusion returned the wrong length");
    }
    let expected_ids = expected.hits.iter().map(|hit| hit.id).collect::<Vec<_>>();
    let observed_ids = observed
        .main
        .hits
        .iter()
        .map(|hit| hit.id)
        .collect::<Vec<_>>();
    if observed_ids != expected_ids {
        return fail(
            I47_CHECKER_ID,
            format_args!(
                "bounded fusion order differs: expected {expected_ids:?} observed {observed_ids:?}"
            ),
        );
    }
    if observed.main.report.rounds != expected.report.rounds
        || observed.main.report.budget_exhausted != expected.report.budget_exhausted
        || observed.main.report.termination != expected.report.termination
    {
        return fail(
            I47_CHECKER_ID,
            format_args!(
                "stable-bound replay differs: expected {:?} observed {:?}",
                expected.report, observed.main.report
            ),
        );
    }
    Ok(())
}

pub fn compare_i48(input: &HybridInput, observed: &HybridObserved) -> Result<(), String> {
    guard_fixture(input, I48_CHECKER_ID)?;
    let expected = expected_case(&input.rrf);
    if expected.report.method != FusionMethodFact::ReciprocalRankFusion
        || observed.rrf.report.method != expected.report.method
    {
        return fail(I48_CHECKER_ID, "all-equal leg did not select RRF fallback");
    }
    if observed.rrf != expected {
        return fail(
            I48_CHECKER_ID,
            format_args!(
                "RRF ids, scores, or report differ: expected {expected:?} observed {:?}",
                observed.rrf
            ),
        );
    }
    Ok(())
}

pub fn compare_i49(input: &HybridInput, observed: &HybridObserved) -> Result<(), String> {
    guard_fixture(input, I49_CHECKER_ID)?;
    if !observed.same_seed_control_passed {
        return fail(I49_CHECKER_ID, "same-seed fixture directories differ");
    }
    let expected_retry = expected_case(&input.main);
    for (leg, detail) in [
        (FusionLegFact::Vector, "vector hybrid leg panicked"),
        (FusionLegFact::Lexical, "lexical hybrid leg panicked"),
    ] {
        let Some(fault) = observed
            .leg_faults
            .iter()
            .find(|candidate| candidate.leg == leg)
        else {
            return fail(
                I49_CHECKER_ID,
                format_args!("missing {leg:?} panic evidence"),
            );
        };
        if fault.kind != LegFailureFact::Panic
            || fault.detail != detail
            || !fault.no_partial
            || !fault.both_legs_completed
        {
            return fail(
                I49_CHECKER_ID,
                format_args!("{leg:?} panic was not typed and atomic: {fault:?}"),
            );
        }
        if fault.retry != expected_retry {
            return fail(
                I49_CHECKER_ID,
                format_args!("{leg:?} clean retry differs from the independent clean result"),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(k: usize, vector: &[(u64, f64)], lexical: &[(u64, f64)]) -> FusionCaseInput {
        FusionCaseInput {
            k,
            alpha_bits: 0.5_f64.to_bits(),
            max_rounds: 8,
            vector: vector
                .iter()
                .map(|(id, score)| RankedScore::new(*id, score.to_bits()))
                .collect(),
            lexical: lexical
                .iter()
                .map(|(id, score)| RankedScore::new(*id, score.to_bits()))
                .collect(),
        }
    }

    fn pair() -> (HybridInput, HybridObserved) {
        let vector = (0..40_u64)
            .map(|index| (100 - index, (index / 2) as f64))
            .collect::<Vec<_>>();
        let lexical = vec![(100, 3.0), (99, 3.0), (98, 1.0)];
        let main = case(2, &vector, &lexical);
        let rrf = case(2, &vector, &[(100, 1.0), (99, 1.0)]);
        let single = case(2, &vector, &[(100, 1.0)]);
        let empty = case(2, &vector, &[]);
        let expected_main = expected_case(&main);
        let input = HybridInput {
            generated_docs: 44,
            deleted_docs: 4,
            active_docs: 4,
            dimensions: 8,
            fixture_ids: vector.iter().map(|(id, _)| *id).collect(),
            main: main.clone(),
            rrf: rrf.clone(),
            single: single.clone(),
            empty: empty.clone(),
        };
        let observed = HybridObserved {
            main: expected_main.clone(),
            rrf: expected_case(&rrf),
            single: expected_case(&single),
            empty: expected_case(&empty),
            leg_faults: [
                (FusionLegFact::Vector, "vector hybrid leg panicked"),
                (FusionLegFact::Lexical, "lexical hybrid leg panicked"),
            ]
            .into_iter()
            .map(|(leg, detail)| LegFaultObserved {
                leg,
                kind: LegFailureFact::Panic,
                detail: detail.to_owned(),
                no_partial: true,
                both_legs_completed: true,
                retry: expected_main.clone(),
            })
            .collect(),
            same_seed_control_passed: true,
        };
        (input, observed)
    }

    #[test]
    fn every_hybrid_checker_accepts_independent_expected_math() {
        let (input, observed) = pair();
        compare_i45(&input, &observed).unwrap();
        compare_i46(&input, &observed).unwrap();
        compare_i47(&input, &observed).unwrap();
        compare_i48(&input, &observed).unwrap();
        compare_i49(&input, &observed).unwrap();
    }

    #[test]
    fn every_hybrid_checker_rejects_a_deliberate_plant() {
        let (input, observed) = pair();
        let mut plant = observed.clone();
        plant.single.hits[0].vector_squared_l2_bits = None;
        assert!(compare_i45(&input, &plant).is_err());

        let mut plant = observed.clone();
        plant.main.hits[0].fused_score_bits ^= 1;
        assert!(compare_i46(&input, &plant).is_err());

        let mut plant = observed.clone();
        plant.main.hits.swap(0, 1);
        assert!(compare_i47(&input, &plant).is_err());

        let mut plant = observed.clone();
        plant.rrf.hits[0].fused_score_bits ^= 1;
        assert!(compare_i48(&input, &plant).is_err());

        let mut plant = observed;
        plant.leg_faults[0].no_partial = false;
        assert!(compare_i49(&input, &plant).is_err());
    }
}
