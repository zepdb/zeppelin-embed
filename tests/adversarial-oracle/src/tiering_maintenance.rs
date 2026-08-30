//! Independent expected values for tiering policy, transition, budget, and publication.

pub const I50_CHECKER_ID: &str = "tier.i50.policy.v2";
pub const I51_CHECKER_ID: &str = "tier.i51.transition.v2";
pub const I52_CHECKER_ID: &str = "tier.i52.progress.v2";
pub const I53_CHECKER_ID: &str = "tier.i53.publication.v2";

const MIN_ROWS: u32 = 129;
const ROW_SPAN: u64 = 172;
const MAX_SAFE_JSON_INTEGER: u64 = 1_u64 << 53;
const CHECKPOINT_ROWS: u64 = 64;
const NODE_BLOCK_TRAILER_BYTES: u64 = 128;
const GRAPH_MAX_DEGREE: u64 = 44;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TierInput {
    pub seed: u64,
    pub rows: u32,
    pub dims: u32,
    pub policy_threshold: u32,
    pub maintenance_threshold: u32,
    pub stride: u64,
    pub k: u32,
    pub source_generation: u64,
    pub source_segment: [u8; 16],
}

#[derive(Clone, Debug, PartialEq)]
pub struct FixtureDocument {
    pub doc_id: u64,
    pub vector: Vec<f32>,
    pub deleted: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TierFixture {
    pub rows: u32,
    pub dims: u32,
    pub policy_threshold: u32,
    pub maintenance_threshold: u32,
    pub stride: u64,
    pub k: u32,
    pub query: Vec<f32>,
    pub documents: Vec<FixtureDocument>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanBranch {
    MaskedScan,
    Graph,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanTier {
    SealedScan,
    SealedGraph,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlanFact {
    pub branch: PlanBranch,
    pub tier: PlanTier,
    pub plan_count: u32,
    pub exact_rescore: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CandidateFact {
    pub doc_id: u64,
    pub revision: u64,
    pub score_bits: u32,
    pub segment: [u8; 16],
    pub local_row: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BudgetDisposition {
    Complete,
    BudgetExhausted,
    Failed(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BudgetStep {
    pub disposition: BudgetDisposition,
    pub bytes_consumed: u64,
    pub rows_advanced: u64,
    pub checkpoints_resumed: u64,
    pub checkpoint_present: bool,
    pub graphs_built: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PublicationFact {
    pub manifest_generation: u64,
    pub manifest_segments: u32,
    pub graph_segment: [u8; 16],
    pub graph_regions: u32,
    pub segment_rows: u32,
    pub segment_dims: u32,
    pub source_segment_gone: bool,
    pub temporary_orphans: u32,
    pub checkpoint_orphans: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TierObserved {
    pub auto_before: PlanFact,
    pub auto_after_policy: PlanFact,
    pub auto_after_promotion: PlanFact,
    pub exact_before: Vec<CandidateFact>,
    pub exact_after: Vec<CandidateFact>,
    pub auto_after: Vec<CandidateFact>,
    pub budget_steps: Vec<BudgetStep>,
    pub publication: PublicationFact,
    pub reopened_auto_plan: PlanFact,
    pub reopened_auto: Vec<CandidateFact>,
}

#[derive(Clone, Copy, Debug)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }
}

pub fn fixture(seed: u64) -> TierFixture {
    let mut random = SplitMix64::new(seed ^ 0x7469_6572_2d76_3200);
    let rows = MIN_ROWS + u32::try_from(random.next() % ROW_SPAN).unwrap_or(0);
    let dimensions = [8_u32, 32, 128];
    let dims = dimensions[usize::try_from(random.next() % 3).unwrap_or(0)];
    let policy_threshold = if seed.is_multiple_of(2) {
        rows
    } else {
        rows.saturating_add(1)
    };
    let stride = graph_stride(dims);
    let query = (0..dims)
        .map(|_| {
            let numerator = i32::try_from(random.next() % 2_001).unwrap_or(0) - 1_000;
            numerator as f32 / 997.0
        })
        .collect::<Vec<_>>();
    let mut ranks = (0..rows).collect::<Vec<_>>();
    shuffle(&mut ranks, &mut random);
    let mut ids = (0..rows).map(u64::from).collect::<Vec<_>>();
    shuffle(&mut ids, &mut random);
    if ids.windows(2).all(|pair| pair[0] < pair[1]) && ids.len() >= 2 {
        ids.swap(0, 1);
    }
    let base = random.next() % (MAX_SAFE_JSON_INTEGER - 1_024);
    let delete_percent = 5 + u32::try_from(random.next() % 6).unwrap_or(0);
    let delete_count = (rows.saturating_mul(delete_percent).saturating_add(99)) / 100;
    let delete_from_rank = rows.saturating_sub(delete_count);
    let documents = ranks
        .into_iter()
        .zip(ids)
        .map(|(rank, id)| {
            let distance_class = rank / 2 + 1;
            let vector = query
                .iter()
                .enumerate()
                .map(|(dimension, value)| {
                    let dimension = u64::try_from(dimension).unwrap_or(0);
                    let magnitude =
                        ((seed.rotate_left(17) ^ dimension.wrapping_mul(31)) % 17 + 1) as f32;
                    let sign = if (dimension + seed).is_multiple_of(2) {
                        1.0
                    } else {
                        -1.0
                    };
                    *value + sign * magnitude * distance_class as f32 / 4_093.0
                })
                .collect();
            FixtureDocument {
                doc_id: base + id + 1,
                vector,
                deleted: rank >= delete_from_rank,
            }
        })
        .collect();
    TierFixture {
        rows,
        dims,
        policy_threshold,
        maintenance_threshold: rows,
        stride,
        k: 8,
        query,
        documents,
    }
}

impl TierInput {
    #[must_use]
    pub fn from_seed(seed: u64, source_generation: u64, source_segment: [u8; 16]) -> Self {
        let fixture = fixture(seed);
        Self {
            seed,
            rows: fixture.rows,
            dims: fixture.dims,
            policy_threshold: fixture.policy_threshold,
            maintenance_threshold: fixture.maintenance_threshold,
            stride: fixture.stride,
            k: fixture.k,
            source_generation,
            source_segment,
        }
    }
}

fn shuffle<T>(values: &mut [T], random: &mut SplitMix64) {
    for upper in (1..values.len()).rev() {
        let selected = usize::try_from(random.next() % (upper as u64 + 1)).unwrap_or(0);
        values.swap(upper, selected);
    }
}

fn graph_stride(dims: u32) -> u64 {
    let padded_dims = u64::from(dims).div_ceil(128) * 128;
    let unaligned = padded_dims.div_ceil(2) + 16 + 4 * GRAPH_MAX_DEGREE;
    unaligned.div_ceil(128) * 128
}

fn fail<T>(checker: &str, detail: &str) -> Result<T, String> {
    Err(format!("{checker}: {detail}"))
}

fn validate_input(input: &TierInput, checker: &str) -> Result<TierFixture, String> {
    let expected = fixture(input.seed);
    if input.rows != expected.rows
        || input.dims != expected.dims
        || input.policy_threshold != expected.policy_threshold
        || input.maintenance_threshold != expected.maintenance_threshold
        || input.stride != expected.stride
        || input.k != expected.k
    {
        return fail(checker, "seed-derived fixture geometry differs");
    }
    let live = expected
        .documents
        .iter()
        .filter(|document| !document.deleted)
        .count();
    if input.rows < 100 || live < input.k as usize || input.source_segment == [0; 16] {
        return fail(
            checker,
            "minimum fixture cardinality or source identity is absent",
        );
    }
    if !matches!(input.dims, 8 | 32 | 128) || input.stride != graph_stride(input.dims) {
        return fail(
            checker,
            "fixture dimensions or independent graph stride differ",
        );
    }
    Ok(expected)
}

fn expected_ranked(input: &TierInput, fixture: &TierFixture) -> Vec<CandidateFact> {
    let mut candidates = fixture
        .documents
        .iter()
        .enumerate()
        .filter(|(_, document)| !document.deleted)
        .map(|(row, document)| {
            let distance = document
                .vector
                .iter()
                .zip(&fixture.query)
                .map(|(left, right)| {
                    let delta = f64::from(*left) - f64::from(*right);
                    delta * delta
                })
                .sum::<f64>();
            CandidateFact {
                doc_id: document.doc_id,
                revision: 1,
                score_bits: (-(distance as f32)).to_bits(),
                segment: input.source_segment,
                local_row: u32::try_from(row).unwrap_or(u32::MAX),
            }
        })
        .collect::<Vec<_>>();
    candidates.sort_unstable_by(|left, right| {
        f32::from_bits(right.score_bits)
            .total_cmp(&f32::from_bits(left.score_bits))
            .then_with(|| left.doc_id.cmp(&right.doc_id))
            .then_with(|| right.revision.cmp(&left.revision))
            .then_with(|| left.local_row.cmp(&right.local_row))
    });
    candidates.truncate(input.k as usize);
    candidates
}

fn semantic(candidate: CandidateFact) -> (u64, u64, u32, u32) {
    (
        candidate.doc_id,
        candidate.revision,
        candidate.score_bits,
        candidate.local_row,
    )
}

pub fn compare_i50(input: &TierInput, observed: &TierObserved) -> Result<(), String> {
    validate_input(input, I50_CHECKER_ID)?;
    let before = PlanFact {
        branch: PlanBranch::MaskedScan,
        tier: PlanTier::SealedScan,
        plan_count: 1,
        exact_rescore: false,
    };
    if observed.auto_before != before {
        return fail(
            I50_CHECKER_ID,
            "Auto before maintenance did not execute masked scan",
        );
    }
    let promoted = input.rows >= input.policy_threshold;
    let after = if promoted {
        PlanFact {
            branch: PlanBranch::Graph,
            tier: PlanTier::SealedGraph,
            plan_count: 1,
            exact_rescore: true,
        }
    } else {
        before
    };
    if observed.auto_after_policy != after {
        return fail(
            I50_CHECKER_ID,
            "Auto after maintenance differs from the rows >= threshold policy",
        );
    }
    Ok(())
}

pub fn compare_i51(input: &TierInput, observed: &TierObserved) -> Result<(), String> {
    let fixture = validate_input(input, I51_CHECKER_ID)?;
    let expected = expected_ranked(input, &fixture);
    if observed.exact_before != expected {
        return fail(
            I51_CHECKER_ID,
            "Exact before maintenance differs from f64 brute force",
        );
    }
    if observed.exact_after != observed.auto_after {
        return fail(
            I51_CHECKER_ID,
            "Auto after promotion differs bit-for-bit from Exact",
        );
    }
    if observed
        .exact_after
        .iter()
        .any(|candidate| candidate.segment != observed.publication.graph_segment)
    {
        return fail(
            I51_CHECKER_ID,
            "post-promotion physical segment identity differs",
        );
    }
    let after_semantic = observed
        .exact_after
        .iter()
        .copied()
        .map(semantic)
        .collect::<Vec<_>>();
    let expected_semantic = expected.into_iter().map(semantic).collect::<Vec<_>>();
    if after_semantic != expected_semantic {
        return fail(
            I51_CHECKER_ID,
            "Exact after maintenance differs from f64 brute force or changed rows",
        );
    }
    let promoted_plan = PlanFact {
        branch: PlanBranch::Graph,
        tier: PlanTier::SealedGraph,
        plan_count: 1,
        exact_rescore: true,
    };
    if observed.auto_after_promotion != promoted_plan {
        return fail(
            I51_CHECKER_ID,
            "Auto did not execute the promoted graph with exact rescore",
        );
    }
    Ok(())
}

fn expected_budget_steps(input: &TierInput) -> Vec<BudgetStep> {
    let partial = |resumed| BudgetStep {
        disposition: BudgetDisposition::BudgetExhausted,
        bytes_consumed: CHECKPOINT_ROWS * input.stride,
        rows_advanced: CHECKPOINT_ROWS,
        checkpoints_resumed: resumed,
        checkpoint_present: true,
        graphs_built: 0,
    };
    vec![
        BudgetStep {
            disposition: BudgetDisposition::BudgetExhausted,
            bytes_consumed: 0,
            rows_advanced: 0,
            checkpoints_resumed: 0,
            checkpoint_present: false,
            graphs_built: 0,
        },
        partial(0),
        partial(1),
        BudgetStep {
            disposition: BudgetDisposition::Complete,
            bytes_consumed: (u64::from(input.rows) - 2 * CHECKPOINT_ROWS) * input.stride
                + NODE_BLOCK_TRAILER_BYTES,
            rows_advanced: u64::from(input.rows) - 2 * CHECKPOINT_ROWS,
            checkpoints_resumed: 1,
            checkpoint_present: false,
            graphs_built: 1,
        },
    ]
}

pub fn compare_i52(input: &TierInput, observed: &TierObserved) -> Result<(), String> {
    validate_input(input, I52_CHECKER_ID)?;
    if observed.budget_steps != expected_budget_steps(input) {
        return fail(
            I52_CHECKER_ID,
            "maintenance status, byte ledger, row progress, or checkpoint lifecycle differs",
        );
    }
    if observed
        .budget_steps
        .iter()
        .map(|step| step.graphs_built)
        .sum::<u64>()
        != 1
    {
        return fail(
            I52_CHECKER_ID,
            "budget sequence did not build exactly one graph",
        );
    }
    Ok(())
}

pub fn compare_i53(input: &TierInput, observed: &TierObserved) -> Result<(), String> {
    validate_input(input, I53_CHECKER_ID)?;
    let publication = observed.publication;
    if publication.manifest_generation != input.source_generation.saturating_add(1)
        || publication.manifest_segments != 1
        || publication.graph_regions != 1
        || publication.segment_rows != input.rows
        || publication.segment_dims != input.dims
        || publication.graph_segment == input.source_segment
        || !publication.source_segment_gone
        || publication.temporary_orphans != 0
        || publication.checkpoint_orphans != 0
    {
        return fail(
            I53_CHECKER_ID,
            "independently parsed publication facts differ from one replacement graph generation",
        );
    }
    if observed.reopened_auto_plan
        != (PlanFact {
            branch: PlanBranch::Graph,
            tier: PlanTier::SealedGraph,
            plan_count: 1,
            exact_rescore: true,
        })
        || observed.reopened_auto != observed.auto_after
    {
        return fail(
            I53_CHECKER_ID,
            "close/reopen changed Auto plan or bit-for-bit results",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate_with_segment(mut candidate: CandidateFact, segment: [u8; 16]) -> CandidateFact {
        candidate.segment = segment;
        candidate
    }

    fn pair(seed: u64) -> (TierInput, TierObserved) {
        let source = [3; 16];
        let graph = [4; 16];
        let input = TierInput::from_seed(seed, 9, source);
        let fixture = fixture(seed);
        let before = expected_ranked(&input, &fixture);
        let after = before
            .iter()
            .copied()
            .map(|candidate| candidate_with_segment(candidate, graph))
            .collect::<Vec<_>>();
        let scan = PlanFact {
            branch: PlanBranch::MaskedScan,
            tier: PlanTier::SealedScan,
            plan_count: 1,
            exact_rescore: false,
        };
        let graph_plan = PlanFact {
            branch: PlanBranch::Graph,
            tier: PlanTier::SealedGraph,
            plan_count: 1,
            exact_rescore: true,
        };
        let policy = if input.rows >= input.policy_threshold {
            graph_plan
        } else {
            scan
        };
        let observed = TierObserved {
            auto_before: scan,
            auto_after_policy: policy,
            auto_after_promotion: graph_plan,
            exact_before: before,
            exact_after: after.clone(),
            auto_after: after.clone(),
            budget_steps: expected_budget_steps(&input),
            publication: PublicationFact {
                manifest_generation: 10,
                manifest_segments: 1,
                graph_segment: graph,
                graph_regions: 1,
                segment_rows: input.rows,
                segment_dims: input.dims,
                source_segment_gone: true,
                temporary_orphans: 0,
                checkpoint_orphans: 0,
            },
            reopened_auto_plan: graph_plan,
            reopened_auto: after,
        };
        (input, observed)
    }

    #[test]
    fn fixture_spans_both_policy_outcomes_and_has_real_shape() {
        let even = fixture(2);
        let odd = fixture(3);
        assert!(even.rows >= even.policy_threshold);
        assert!(odd.rows < odd.policy_threshold);
        assert!(even.rows > 128 && odd.rows > 128);
        assert!(
            even.documents
                .windows(2)
                .any(|pair| pair[0].vector == pair[1].vector)
        );
        assert!(
            even.documents
                .iter()
                .all(|document| document.doc_id < MAX_SAFE_JSON_INTEGER)
        );
    }

    #[test]
    fn every_tier_checker_accepts_a_valid_observation() {
        for seed in [2, 3] {
            let (input, observed) = pair(seed);
            compare_i50(&input, &observed).unwrap();
            compare_i51(&input, &observed).unwrap();
            compare_i52(&input, &observed).unwrap();
            compare_i53(&input, &observed).unwrap();
        }
    }

    #[test]
    fn every_tier_checker_rejects_a_deliberate_plant() {
        let (input, observed) = pair(2);
        let mut plant = observed.clone();
        plant.auto_after_policy = plant.auto_before;
        assert!(compare_i50(&input, &plant).is_err());
        let mut plant = observed.clone();
        plant.auto_after.reverse();
        assert!(compare_i51(&input, &plant).is_err());
        let mut plant = observed.clone();
        plant.budget_steps[1].checkpoints_resumed = 1;
        assert!(compare_i52(&input, &plant).is_err());
        let mut plant = observed;
        plant.publication.source_segment_gone = false;
        assert!(compare_i53(&input, &plant).is_err());
    }
}
