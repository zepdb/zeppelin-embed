//! Seeded exhaustive and hill-climbing search plus stop authority.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::pmu::{AttributionClass, AttributionEvidence, PmuOutcome};
use super::variants::KernelPoint;

/// Correctness and timing result for one candidate.
#[derive(Clone, Debug, PartialEq)]
pub struct CandidateEvaluation {
    /// Stable variant name.
    pub variant: String,
    /// Whether the scalar/property oracle accepted the result.
    pub correctness_green: bool,
    /// Min-of-medians elapsed time in nanoseconds.
    pub median_ns: f64,
}

impl CandidateEvaluation {
    /// Creates a correctness-green timing result.
    #[must_use]
    pub fn correct(variant: impl Into<String>, median_ns: f64) -> Self {
        Self {
            variant: variant.into(),
            correctness_green: true,
            median_ns,
        }
    }

    /// Creates a deliberately or accidentally incorrect timing result.
    #[must_use]
    pub fn incorrect(variant: impl Into<String>, median_ns: f64) -> Self {
        Self {
            variant: variant.into(),
            correctness_green: false,
            median_ns,
        }
    }
}

/// Keep/revert result after correctness-first ranking.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateDisposition {
    /// Fastest correctness-green candidate.
    Keep,
    /// Correct but slower than the winner.
    RevertRegression,
    /// Faster or slower candidate rejected by the correctness oracle.
    DiscardIncorrect,
}

/// Ranked candidate retained for ledger evidence.
#[derive(Clone, Debug, PartialEq)]
pub struct RankedCandidate {
    /// Stable variant name.
    pub variant: String,
    /// Measured min-of-medians latency.
    pub median_ns: f64,
    /// Correctness-first keep/revert disposition.
    pub disposition: CandidateDisposition,
}

/// Ranks candidates only after excluding incorrect results from speed choice.
pub fn rank_candidates(
    evaluations: Vec<CandidateEvaluation>,
) -> Result<Vec<RankedCandidate>, TuneError> {
    if evaluations.is_empty() {
        return Err(TuneError::EmptySearchSpace);
    }
    if evaluations
        .iter()
        .any(|evaluation| !evaluation.median_ns.is_finite() || evaluation.median_ns <= 0.0)
    {
        return Err(TuneError::InvalidTiming);
    }
    let winner = evaluations
        .iter()
        .enumerate()
        .filter(|(_, evaluation)| evaluation.correctness_green)
        .min_by(|(_, left), (_, right)| left.median_ns.total_cmp(&right.median_ns))
        .map(|(index, _)| index)
        .ok_or(TuneError::NoCorrectCandidate)?;
    let mut ranked = evaluations
        .into_iter()
        .enumerate()
        .map(|(index, evaluation)| RankedCandidate {
            variant: evaluation.variant,
            median_ns: evaluation.median_ns,
            disposition: if !evaluation.correctness_green {
                CandidateDisposition::DiscardIncorrect
            } else if index == winner {
                CandidateDisposition::Keep
            } else {
                CandidateDisposition::RevertRegression
            },
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        disposition_order(left.disposition)
            .cmp(&disposition_order(right.disposition))
            .then_with(|| left.median_ns.total_cmp(&right.median_ns))
    });
    Ok(ranked)
}

fn disposition_order(disposition: CandidateDisposition) -> u8 {
    match disposition {
        CandidateDisposition::Keep => 0,
        CandidateDisposition::RevertRegression => 1,
        CandidateDisposition::DiscardIncorrect => 2,
    }
}

/// Unique points available to one tuner run.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchSpace {
    points: Vec<KernelPoint>,
    fingerprint: u64,
}

impl SearchSpace {
    /// Creates a nonempty, duplicate-free search space.
    pub fn new(points: Vec<KernelPoint>) -> Result<Self, TuneError> {
        if points.is_empty() {
            return Err(TuneError::EmptySearchSpace);
        }
        for (index, point) in points.iter().enumerate() {
            if points.iter().skip(index + 1).any(|other| other == point) {
                return Err(TuneError::DuplicatePoint(point.stable_id()));
            }
        }
        let fingerprint = fingerprint_points(&points);
        Ok(Self {
            points,
            fingerprint,
        })
    }

    /// Ordered points in the search space.
    #[must_use]
    pub fn points(&self) -> &[KernelPoint] {
        &self.points
    }
}

/// Seed and bounds for exhaustive or large-space search.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SearchConfig {
    /// Reproducibility seed, printed and persisted by callers.
    pub seed: u64,
    /// Spaces no larger than this use a complete exhaustive grid.
    pub exhaustive_limit: usize,
    /// Random restarts after a hill-climb exhausts neighboring points.
    pub random_restarts: usize,
    /// Maximum evaluated points in this invocation or resumed campaign.
    pub maximum_evaluations: usize,
}

/// Serializable-in-principle state sufficient to resume exactly.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchState {
    /// Original search seed.
    pub seed: u64,
    /// Current deterministic RNG state.
    pub rng_state: u64,
    /// Search-space identity guarding resume.
    pub space_fingerprint: u64,
    /// Whether the run uses exhaustive enumeration.
    pub exhaustive: bool,
    /// Indices waiting to be evaluated in order.
    pub pending: Vec<usize>,
    /// Completed point results in trajectory order.
    pub evaluated: Vec<SearchEvaluation>,
    /// Local optima/anchors already expanded.
    pub expanded: Vec<usize>,
    /// Number of random starting points already selected.
    pub starts: usize,
}

/// Minimal persisted result for one searched point.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchEvaluation {
    /// Index into the immutable search space.
    pub point_index: usize,
    /// Correctness status.
    pub correctness_green: bool,
    /// Min-of-medians latency.
    pub median_ns: f64,
}

/// Completed or bounded search trajectory and resumable state.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchRun {
    /// Stable point identifiers in exact evaluation order.
    pub trajectory: Vec<String>,
    /// State to persist for a later continuation.
    pub state: SearchState,
    /// Correctness-first ranking of all evaluated points.
    pub ranking: Vec<RankedCandidate>,
}

/// Atomically persists all state required for deterministic process resumption.
pub fn save_search_state(path: impl AsRef<Path>, state: &SearchState) -> Result<(), TuneError> {
    let path = path.as_ref();
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|error| TuneError::Io {
            path: parent.to_path_buf(),
            reason: error.to_string(),
        })?;
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let encoded = serde_json::to_vec_pretty(&search_state_value(state))
        .map_err(|error| TuneError::StateFormat(error.to_string()))?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(|error| TuneError::Io {
            path: temporary.clone(),
            reason: error.to_string(),
        })?;
    if let Err(error) = file.write_all(&encoded).and_then(|()| file.sync_all()) {
        let _cleanup = fs::remove_file(&temporary);
        return Err(TuneError::Io {
            path: temporary,
            reason: error.to_string(),
        });
    }
    fs::rename(&temporary, path).map_err(|error| TuneError::Io {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })?;
    Ok(())
}

/// Loads a previously persisted deterministic search state.
pub fn load_search_state(path: impl AsRef<Path>) -> Result<SearchState, TuneError> {
    let path = path.as_ref();
    let bytes = fs::read(path).map_err(|error| TuneError::Io {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| TuneError::StateFormat(error.to_string()))?;
    search_state_from_value(&value)
}

/// Runs an exhaustive small grid or seeded hill climb with random restarts.
pub fn run_search<F>(
    space: &SearchSpace,
    config: SearchConfig,
    resume: Option<SearchState>,
    mut evaluate: F,
) -> Result<SearchRun, TuneError>
where
    F: FnMut(KernelPoint) -> CandidateEvaluation,
{
    if config.maximum_evaluations == 0 || config.exhaustive_limit == 0 {
        return Err(TuneError::InvalidSearchConfig);
    }
    let exhaustive = space.points.len() <= config.exhaustive_limit;
    let mut state = match resume {
        Some(state) => {
            if state.seed != config.seed
                || state.space_fingerprint != space.fingerprint
                || state.exhaustive != exhaustive
            {
                return Err(TuneError::ResumeMismatch);
            }
            state
        }
        None => initial_state(space, config, exhaustive),
    };
    let mut rng = SeededRng::from_state(state.rng_state);
    while state.evaluated.len() < config.maximum_evaluations {
        if state.pending.is_empty() {
            refill_pending(space, config, &mut state, &mut rng);
        }
        if state.pending.is_empty() {
            break;
        }
        let point_index = state.pending.remove(0);
        if state
            .evaluated
            .iter()
            .any(|result| result.point_index == point_index)
        {
            continue;
        }
        let point = *space
            .points
            .get(point_index)
            .ok_or(TuneError::ResumeMismatch)?;
        let result = evaluate(point);
        if !result.median_ns.is_finite() || result.median_ns <= 0.0 {
            return Err(TuneError::InvalidTiming);
        }
        state.evaluated.push(SearchEvaluation {
            point_index,
            correctness_green: result.correctness_green,
            median_ns: result.median_ns,
        });
        state.rng_state = rng.state;
    }
    let trajectory = state
        .evaluated
        .iter()
        .map(|result| {
            space.points.get(result.point_index).map_or_else(
                || "invalid-resume-point".to_owned(),
                |point| point.stable_id(),
            )
        })
        .collect();
    let evaluations = state
        .evaluated
        .iter()
        .map(|result| {
            let variant = space.points.get(result.point_index).map_or_else(
                || "invalid-resume-point".to_owned(),
                |point| point.stable_id(),
            );
            CandidateEvaluation {
                variant,
                correctness_green: result.correctness_green,
                median_ns: result.median_ns,
            }
        })
        .collect();
    let ranking = rank_candidates(evaluations)?;
    Ok(SearchRun {
        trajectory,
        state,
        ranking,
    })
}

fn initial_state(space: &SearchSpace, config: SearchConfig, exhaustive: bool) -> SearchState {
    let seed_state = SeededRng::seed_state(config.seed);
    SearchState {
        seed: config.seed,
        rng_state: seed_state,
        space_fingerprint: space.fingerprint,
        exhaustive,
        pending: if exhaustive {
            (0..space.points.len()).collect()
        } else {
            Vec::new()
        },
        evaluated: Vec::new(),
        expanded: Vec::new(),
        starts: 0,
    }
}

fn refill_pending(
    space: &SearchSpace,
    config: SearchConfig,
    state: &mut SearchState,
    rng: &mut SeededRng,
) {
    if state.exhaustive {
        return;
    }
    if let Some(anchor) = best_unexpanded(state) {
        state.expanded.push(anchor);
        let mut neighbors = neighboring_indices(space, anchor)
            .into_iter()
            .filter(|index| !was_seen(state, *index))
            .collect::<Vec<_>>();
        rng.shuffle(&mut neighbors);
        if !neighbors.is_empty() {
            state.pending = neighbors;
            state.rng_state = rng.state;
            return;
        }
    }
    if state.starts <= config.random_restarts
        && let Some(index) = random_unseen(space.points.len(), state, rng)
    {
        state.pending.push(index);
        state.starts += 1;
        state.rng_state = rng.state;
    }
}

fn best_unexpanded(state: &SearchState) -> Option<usize> {
    state
        .evaluated
        .iter()
        .filter(|result| result.correctness_green && !state.expanded.contains(&result.point_index))
        .min_by(|left, right| left.median_ns.total_cmp(&right.median_ns))
        .map(|result| result.point_index)
}

fn neighboring_indices(space: &SearchSpace, anchor: usize) -> Vec<usize> {
    let Some(anchor_point) = space.points.get(anchor) else {
        return Vec::new();
    };
    space
        .points
        .iter()
        .enumerate()
        .filter_map(|(index, point)| {
            (index != anchor && knob_distance(*anchor_point, *point) == 1).then_some(index)
        })
        .collect()
}

fn knob_distance(left: KernelPoint, right: KernelPoint) -> usize {
    usize::from(left.unroll != right.unroll)
        + usize::from(left.accumulators != right.accumulators)
        + usize::from(left.rows_per_block != right.rows_per_block)
        + usize::from(left.prefetch_dist != right.prefetch_dist)
        + usize::from(left.tier != right.tier)
}

fn random_unseen(length: usize, state: &SearchState, rng: &mut SeededRng) -> Option<usize> {
    let mut unseen = (0..length)
        .filter(|index| !was_seen(state, *index))
        .collect::<Vec<_>>();
    if unseen.is_empty() {
        return None;
    }
    rng.shuffle(&mut unseen);
    unseen.first().copied()
}

fn was_seen(state: &SearchState, index: usize) -> bool {
    state.pending.contains(&index)
        || state
            .evaluated
            .iter()
            .any(|result| result.point_index == index)
}

fn fingerprint_points(points: &[KernelPoint]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in points
        .iter()
        .flat_map(|point| point.stable_id().into_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

struct SeededRng {
    state: u64,
}

impl SeededRng {
    fn seed_state(seed: u64) -> u64 {
        if seed == 0 {
            0x9e37_79b9_7f4a_7c15
        } else {
            seed
        }
    }

    fn from_state(state: u64) -> Self {
        Self {
            state: Self::seed_state(state),
        }
    }

    fn next_u64(&mut self) -> u64 {
        let mut value = self.state;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.state = value;
        value
    }

    fn shuffle<T>(&mut self, values: &mut [T]) {
        for upper in (1..values.len()).rev() {
            let index = self.next_u64() as usize % (upper + 1);
            values.swap(upper, index);
        }
    }
}

/// Signals available to the only campaign-stop decision seam.
#[derive(Clone, Debug, PartialEq)]
pub struct CampaignSignals {
    /// Achieved percent of the workload-selected denominator.
    pub achieved_percent: f64,
    /// Diagnostic good-enough tripwire; never stop authority.
    pub percentage_tripwire: f64,
    /// Most recent accepted-methodology improvement percentages.
    pub recent_improvements_percent: Vec<f64>,
    /// PMU report or explicit unavailability.
    pub pmu: PmuOutcome,
    /// Workload named in the ledger row.
    pub workload_name: String,
    /// Whether only synthetic evidence supports the decision.
    pub workload_is_provisional: bool,
    /// Ranked next hypotheses if the frontier remains open.
    pub follow_up_hypotheses: Vec<String>,
}

/// The only two campaign stop dispositions.
#[derive(Clone, Debug, PartialEq)]
pub enum CampaignStop {
    /// Final mechanically justified stop, constructible only with typed PMU evidence.
    Complete {
        /// Nonempty irreducible-gap counter attribution.
        attribution: AttributionEvidence,
    },
    /// Campaign remains open for further hypothesis work.
    FrontierOpen {
        /// Why no completion authority exists.
        reason: String,
        /// Ranked follow-up hypotheses, never empty.
        hypotheses: Vec<String>,
    },
}

/// Stop disposition plus workload-validation scope.
#[derive(Clone, Debug, PartialEq)]
pub struct CampaignDecision {
    /// Attribution-only stop result.
    pub stop: CampaignStop,
    /// Synthetic-only stops remain provisional in the ledger.
    pub provisional: bool,
    /// Workload name copied into the ledger.
    pub workload_name: String,
}

impl CampaignDecision {
    /// Applies tripwire, stagnation, PMU, and workload-provenance policy.
    #[must_use]
    pub fn evaluate(signals: CampaignSignals) -> Self {
        let provisional = signals.workload_is_provisional;
        let workload_name = signals.workload_name.clone();
        let mut hypotheses = signals.follow_up_hypotheses;
        if hypotheses.is_empty() {
            hypotheses
                .push("collect PMU attribution and form the next mechanism hypothesis".to_owned());
        }
        if signals.achieved_percent > 100.0 {
            return Self {
                stop: CampaignStop::FrontierOpen {
                    reason: format!(
                        "DENOMINATOR STALE at {:.3}%; remeasure and revise upward before any completion decision",
                        signals.achieved_percent
                    ),
                    hypotheses,
                },
                provisional,
                workload_name,
            };
        }
        if signals.achieved_percent < signals.percentage_tripwire {
            return Self {
                stop: CampaignStop::FrontierOpen {
                    reason: format!(
                        "below {:.3}% diagnostic tripwire at {:.3}%; mandatory continuation even when PMU attributes the present gap",
                        signals.percentage_tripwire, signals.achieved_percent
                    ),
                    hypotheses,
                },
                provisional,
                workload_name,
            };
        }
        let stop = match signals.pmu {
            PmuOutcome::Measured(report) => match report.class() {
                AttributionClass::Irreducible { .. } => {
                    let evidence = report.into_irreducible_evidence();
                    match evidence {
                        Some(attribution) => CampaignStop::Complete { attribution },
                        None => CampaignStop::FrontierOpen {
                            reason: "PMU report lacked irreducible attribution evidence".to_owned(),
                            hypotheses,
                        },
                    }
                }
                AttributionClass::Reducible { reason }
                | AttributionClass::Unattributed { reason } => CampaignStop::FrontierOpen {
                    reason: format!("frontier-open: PMU attribution is not irreducible: {reason}"),
                    hypotheses,
                },
            },
            PmuOutcome::Unavailable { reason } => CampaignStop::FrontierOpen {
                reason: format!(
                    "frontier-open: PMU unavailable ({reason}); Complete requires counter attribution"
                ),
                hypotheses,
            },
        };
        let stop = match stop {
            CampaignStop::Complete { attribution } => CampaignStop::Complete { attribution },
            CampaignStop::FrontierOpen {
                mut reason,
                hypotheses,
            } => {
                reason = format!(
                    "percentage tripwire cleared at {:.3}% but has zero stop authority; {reason}",
                    signals.achieved_percent
                );
                if stagnated(&signals.recent_improvements_percent) {
                    reason.push_str(
                        "; five-iteration under-1% stagnation is a frontier-open finding, never completion",
                    );
                }
                CampaignStop::FrontierOpen { reason, hypotheses }
            }
        };
        Self {
            stop,
            provisional,
            workload_name,
        }
    }
}

fn stagnated(improvements: &[f64]) -> bool {
    let recent = improvements
        .iter()
        .rev()
        .take(5)
        .copied()
        .collect::<Vec<_>>();
    recent.len() == 5 && recent.iter().sum::<f64>() < 1.0
}

/// Typed tuner/search failure.
#[derive(Clone, Debug, PartialEq)]
pub enum TuneError {
    /// No points or evaluations were supplied.
    EmptySearchSpace,
    /// A point was registered more than once.
    DuplicatePoint(String),
    /// Timing was zero, negative, NaN, or infinite.
    InvalidTiming,
    /// No candidate passed correctness.
    NoCorrectCandidate,
    /// Search bounds were zero.
    InvalidSearchConfig,
    /// Saved state does not belong to this seed, mode, or point set.
    ResumeMismatch,
    /// Search-state persistence failed.
    Io {
        /// Affected path.
        path: PathBuf,
        /// Human-readable I/O failure.
        reason: String,
    },
    /// Persisted search state was incomplete or malformed.
    StateFormat(String),
}

impl fmt::Display for TuneError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySearchSpace => formatter.write_str("tuner search space is empty"),
            Self::DuplicatePoint(point) => write!(formatter, "duplicate tuner point {point}"),
            Self::InvalidTiming => {
                formatter.write_str("candidate timing must be positive and finite")
            }
            Self::NoCorrectCandidate => {
                formatter.write_str("no candidate passed the correctness oracle")
            }
            Self::InvalidSearchConfig => formatter.write_str("search limits must be nonzero"),
            Self::ResumeMismatch => {
                formatter.write_str("saved tuner state does not match this search")
            }
            Self::Io { path, reason } => {
                write!(
                    formatter,
                    "search state {} failed: {reason}",
                    path.display()
                )
            }
            Self::StateFormat(reason) => write!(formatter, "search state is invalid: {reason}"),
        }
    }
}

impl std::error::Error for TuneError {}

fn search_state_value(state: &SearchState) -> Value {
    json!({
        "schema": "zeppelin-frontier-search-v1",
        "seed": state.seed,
        "rng_state": state.rng_state,
        "space_fingerprint": state.space_fingerprint,
        "exhaustive": state.exhaustive,
        "pending": state.pending,
        "evaluated": state.evaluated.iter().map(|result| json!({
            "point_index": result.point_index,
            "correctness_green": result.correctness_green,
            "median_ns": result.median_ns,
        })).collect::<Vec<_>>(),
        "expanded": state.expanded,
        "starts": state.starts,
    })
}

fn search_state_from_value(value: &Value) -> Result<SearchState, TuneError> {
    if string_value(value, "schema")? != "zeppelin-frontier-search-v1" {
        return Err(TuneError::StateFormat(
            "unsupported search state schema".to_owned(),
        ));
    }
    let evaluated_values = value
        .get("evaluated")
        .and_then(Value::as_array)
        .ok_or_else(|| TuneError::StateFormat("missing evaluated array".to_owned()))?;
    let evaluated = evaluated_values
        .iter()
        .map(|result| {
            Ok(SearchEvaluation {
                point_index: usize_value(result, "point_index")?,
                correctness_green: result
                    .get("correctness_green")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| {
                        TuneError::StateFormat("missing correctness_green".to_owned())
                    })?,
                median_ns: result
                    .get("median_ns")
                    .and_then(Value::as_f64)
                    .ok_or_else(|| TuneError::StateFormat("missing median_ns".to_owned()))?,
            })
        })
        .collect::<Result<Vec<_>, TuneError>>()?;
    Ok(SearchState {
        seed: u64_value(value, "seed")?,
        rng_state: u64_value(value, "rng_state")?,
        space_fingerprint: u64_value(value, "space_fingerprint")?,
        exhaustive: value
            .get("exhaustive")
            .and_then(Value::as_bool)
            .ok_or_else(|| TuneError::StateFormat("missing exhaustive".to_owned()))?,
        pending: usize_array(value, "pending")?,
        evaluated,
        expanded: usize_array(value, "expanded")?,
        starts: usize_value(value, "starts")?,
    })
}

fn string_value<'a>(value: &'a Value, name: &str) -> Result<&'a str, TuneError> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| TuneError::StateFormat(format!("missing string {name}")))
}

fn u64_value(value: &Value, name: &str) -> Result<u64, TuneError> {
    value
        .get(name)
        .and_then(Value::as_u64)
        .ok_or_else(|| TuneError::StateFormat(format!("missing integer {name}")))
}

fn usize_value(value: &Value, name: &str) -> Result<usize, TuneError> {
    usize::try_from(u64_value(value, name)?)
        .map_err(|_| TuneError::StateFormat(format!("integer {name} exceeds usize")))
}

fn usize_array(value: &Value, name: &str) -> Result<Vec<usize>, TuneError> {
    value
        .get(name)
        .and_then(Value::as_array)
        .ok_or_else(|| TuneError::StateFormat(format!("missing array {name}")))?
        .iter()
        .map(|item| {
            let raw = item
                .as_u64()
                .ok_or_else(|| TuneError::StateFormat(format!("non-integer in {name}")))?;
            usize::try_from(raw)
                .map_err(|_| TuneError::StateFormat(format!("integer in {name} exceeds usize")))
        })
        .collect()
}
