//! Deterministic vector/lexical fusion for library callers and
//! [`Store::search_hybrid`](crate::lifecycle::Store::search_hybrid).
//!
//! Store-owned hybrid pins both legs to one generation and delegates score
//! validation, normalization, alpha policy, RRF fallback, termination, and
//! [`FusionReport`](crate::fusion::FusionReport) construction here. A hybrid
//! query with no explicit store
//! tier uses exact vector scores; explicit tiers retain their selected score
//! provenance, including typed refusal of estimated scores.
//!
//! See the local [architecture-decision ledger](ARCHITECTURE.md).

mod cc;
pub(crate) use cc::StoreFusionScratch;
mod normalize;
mod rrf;
mod rules;

use std::collections::BTreeSet;

pub use rules::{
    ALPHA_POLICY_VERSION, FusionRule, LEXICAL_RULE_ALPHA, RARE_DOCUMENT_FREQUENCY_THRESHOLD,
    RuleSignals,
};

/// MEASURED (`tasks/evidence/17-fusion.md`, 2026-08-26, BEIR SciFact with
/// Cohere embed-english-v3 vectors): the optimum of an eleven-point grid,
/// nDCG@10 0.7642 against 0.7181 dense-only and 0.6917 lexical-only; the
/// band within one point is 0.6..0.8. It coincides with the Bruch, TOIS
/// 2024 literature prior the placeholder started from.
pub const DEFAULT_ALPHA: f64 = 0.7;

/// Store score policy v1: fixed zero lower anchors, the validated vector norm
/// enclosure and exact maximum live combined BM25 as upper anchors. Missing
/// lexical membership is an explicit zero. An empty lexical leg gives vectors
/// full weight; a zero vector enclosure scores every present vector as one.
/// Store fusion uses these values even for equal or singleton lists. The pure
/// complete-list and bounded fusion APIs retain their min-max/RRF semantics.
pub const HYBRID_NORMALIZATION_POLICY_VERSION: u16 = 1;

/// PLACEHOLDER -- NOT YET MEASURED.
///
/// Conventional reciprocal-rank-fusion offset. No Zeppelin corpus
/// measurement has established this value.
pub const RRF_K: u32 = 60;

/// PLACEHOLDER -- NOT YET MEASURED.
///
/// Geometric fusion rounds attempted before exact full-list materialization.
/// The fallback preserves correctness; this value affects work and reporting.
pub const DEFAULT_MAX_ROUNDS: usize = 8;

/// MEASURED (`tasks/hybrid-search-optimization/PLAN.md` §A.2, BEIR SciFact
/// with Cohere embed-english-v3 vectors): with exact anchors and cross-filled
/// windows, a 50-wide window reproduces the full-list fused top-10 in 300/300
/// judged queries and the stability bound proves it in 299/300. Twenty and
/// thirty reproduce only 283 and 293. The engine's own window measurement is
/// recorded in `tasks/evidence/R04-hybrid-window.md`.
pub const HYBRID_WINDOW_FLOOR: usize = 50;

/// PLACEHOLDER -- NOT YET MEASURED.
///
/// Per-result window growth, so a large `k` keeps a window wider than the
/// requested result count. Correctness never depends on it: an unproven
/// window widens and a window covering the corpus is the full-list fusion.
pub const HYBRID_WINDOW_PER_K: usize = 5;

/// Whether a vector score is safe to blend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScorePrecision {
    /// Recomputed from the full-precision vector.
    Exact,
    /// A quantized or otherwise approximate ordering score.
    Estimated,
}

/// What a producer can prove about candidates outside its supplied window.
/// This is independent of whether the supplied scores are exact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateCoverage {
    /// Every eligible candidate is represented by the complete producer list.
    Exhaustive,
    /// The producer supplies the true ordered boundary of a bounded window.
    CertifiedBounded,
    /// Candidate membership or its unseen boundary is not certified.
    Approximate,
}

/// Store-owned facts required before a bounded fusion stop is a certificate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HybridProvenance {
    /// Precision of the vector producer's retained scores.
    pub vector_precision: ScorePrecision,
    /// Coverage supplied by the vector producer, independent of precision.
    pub vector_coverage: CandidateCoverage,
    /// Coverage supplied by the lexical producer.
    pub lexical_coverage: CandidateCoverage,
    /// Every union candidate has its other score or proven nonmembership.
    pub cross_scores_complete: bool,
}

/// One ranked vector candidate. Squared-L2 is lower-is-better.
#[derive(Clone, Debug, PartialEq)]
pub struct VectorCandidate<I> {
    id: I,
    squared_l2: f64,
    precision: ScorePrecision,
}

impl<I> VectorCandidate<I> {
    /// Constructs a candidate whose squared-L2 was computed exactly.
    #[must_use]
    pub const fn exact(id: I, squared_l2: f64) -> Self {
        Self {
            id,
            squared_l2,
            precision: ScorePrecision::Exact,
        }
    }

    /// Constructs an estimated candidate so the fusion guard can reject it.
    #[must_use]
    pub const fn estimated(id: I, score: f64) -> Self {
        Self {
            id,
            squared_l2: score,
            precision: ScorePrecision::Estimated,
        }
    }

    /// Returns the leg-native identity.
    #[must_use]
    pub const fn id(&self) -> &I {
        &self.id
    }

    /// Returns exact squared-L2 when [`Self::precision`] is exact.
    #[must_use]
    pub const fn squared_l2(&self) -> f64 {
        self.squared_l2
    }

    /// Returns the score provenance checked by fusion.
    #[must_use]
    pub const fn precision(&self) -> ScorePrecision {
        self.precision
    }
}

/// One ranked lexical candidate. BM25 is higher-is-better.
#[derive(Clone, Debug, PartialEq)]
pub struct LexicalCandidate<I> {
    id: I,
    bm25: f64,
}

impl<I> LexicalCandidate<I> {
    /// Constructs one exact BM25 candidate.
    #[must_use]
    pub const fn new(id: I, bm25: f64) -> Self {
        Self { id, bm25 }
    }

    /// Returns the leg-native identity.
    #[must_use]
    pub const fn id(&self) -> &I {
        &self.id
    }

    /// Returns the exact BM25 score.
    #[must_use]
    pub const fn bm25(&self) -> f64 {
        self.bm25
    }
}

/// Pure fusion parameters over already-executed legs.
#[derive(Clone, Debug, PartialEq)]
pub struct HybridQuery {
    /// Requested fused result count.
    pub k: usize,
    /// Explicit alpha, or `None` for the versioned policy.
    pub alpha: Option<f64>,
    /// Query-shape inputs to the versioned rule policy.
    pub rule_signals: RuleSignals,
    /// Whether rule-based alpha shifts are enabled.
    pub rules_enabled: bool,
    /// Maximum geometric stability-bound rounds.
    pub max_rounds: usize,
    /// Epoch that interpreted both legs, copied into the report.
    pub epoch: Option<crate::epoch::EpochIdentity>,
}

impl HybridQuery {
    /// Constructs a query using the measured policy defaults: alpha
    /// `DEFAULT_ALPHA` and query-shape rules disabled (policy version 2).
    #[must_use]
    pub const fn new(k: usize) -> Self {
        Self {
            k,
            alpha: None,
            rule_signals: RuleSignals::none(),
            rules_enabled: false,
            max_rounds: DEFAULT_MAX_ROUNDS,
            epoch: None,
        }
    }

    /// Selects an explicit alpha. Explicit values bypass rule shifts.
    #[must_use]
    pub const fn with_alpha(mut self, alpha: f64) -> Self {
        self.alpha = Some(alpha);
        self
    }

    /// Supplies deterministic rule inputs.
    #[must_use]
    pub const fn with_rule_signals(mut self, signals: RuleSignals) -> Self {
        self.rule_signals = signals;
        self
    }

    /// Enables the query-shape alpha rules (`fusion::rules`). Off by default
    /// since policy version 2: every measured rule cell lost against the
    /// no-rule baseline on SciFact (`tasks/evidence/17-fusion.md`).
    #[must_use]
    pub const fn with_rules(mut self) -> Self {
        self.rules_enabled = true;
        self
    }

    /// Disables every query-shape alpha rule (the default).
    #[must_use]
    pub const fn without_rules(mut self) -> Self {
        self.rules_enabled = false;
        self
    }

    /// Selects a geometric-round cap. Zero requests immediate full-list materialization.
    #[must_use]
    pub const fn with_max_rounds(mut self, max_rounds: usize) -> Self {
        self.max_rounds = max_rounds;
        self
    }

    /// Attaches the shared interpretation epoch to the report.
    #[must_use]
    pub const fn with_epoch(mut self, epoch: crate::epoch::EpochIdentity) -> Self {
        self.epoch = Some(epoch);
        self
    }
}

/// Fusion formula actually used.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FusionMethod {
    /// Per-leg min-max normalization followed by an alpha-weighted sum.
    ConvexCombination,
    /// Rank-only fallback because at least one leg could not be normalized.
    ReciprocalRankFusion,
}

/// Leg named by validation and fallback reports.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum FusionLeg {
    /// Squared-L2 vector leg.
    Vector,
    /// BM25 lexical leg.
    Lexical,
}

/// Why min-max normalization was not defined for a leg.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DegenerateKind {
    /// The leg returned no candidates.
    Empty,
    /// The leg returned one candidate.
    SingleHit,
    /// Every raw score in the leg was equal.
    AllScoresEqual,
}

/// One reported fallback cause.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DegenerateLeg {
    /// Affected leg.
    pub leg: FusionLeg,
    /// Exact degeneracy.
    pub kind: DegenerateKind,
}

/// How the widening loop completed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FusionTermination {
    /// Bounds proved that no unseen candidate could change the ordered top-k.
    StableBound,
    /// Both complete lists fit in the current round.
    ListsExhausted,
    /// The round cap fired, then full lists were materialized for correctness.
    BudgetFullMaterialization,
    /// A bounded window could not be proved stable; the caller must widen it.
    WindowUnproven,
    /// Returned an approximate candidate union without certifying unseen rows.
    ApproximateCandidates,
}

/// Exact vector-leg extremes for one bounded window.
///
/// The extremes are corpus-wide, not window-wide: windowed min-max
/// normalization measurably loses quality (PLAN.md §A.2), so a bounded
/// producer supplies a deterministic ceiling that no alive row exceeds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VectorBounds {
    /// Lower normalization anchor. Complete-list callers use the nearest
    /// alive row; Store normalization policy v1 uses zero.
    pub min_squared_l2: f64,
    /// Producer-supplied squared-L2 ceiling over every alive row.
    pub max_squared_l2: f64,
    /// Exact squared-L2 of the (W+1)-th row, absent when the leg is complete.
    pub next_unseen_squared_l2: Option<f64>,
}

/// Exact lexical-leg extremes for one bounded window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LexicalBounds {
    /// Exact BM25 of the best matching document.
    pub max_bm25: f64,
    /// Lower normalization anchor. Complete-list callers use the worst
    /// matching score; Store normalization policy v1 uses zero.
    pub min_bm25: f64,
    /// Exact BM25 of the (W+1)-th hit, absent when the leg is complete.
    pub next_unseen_bm25: Option<f64>,
}

/// Per-leg exact ranges supplied by bounded producers, validated by fusion.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LegBounds {
    /// Vector-leg extremes, absent when the leg produced nothing.
    pub vector: Option<VectorBounds>,
    /// Lexical-leg extremes, absent when the leg produced nothing.
    pub lexical: Option<LexicalBounds>,
}

/// Truthful deterministic report for one fusion.
#[derive(Clone, Debug, PartialEq)]
pub struct FusionReport {
    /// Formula actually used.
    pub method: FusionMethod,
    /// Effective alpha, including a reported rule shift when applicable.
    pub effective_alpha: f64,
    /// Rules applied in stable policy order.
    pub applied_rules: Vec<FusionRule>,
    /// Degenerate leg conditions. Pure APIs select RRF for these conditions;
    /// Store policy v1 keeps its defined fixed-anchor value scores.
    pub degenerate_legs: Vec<DegenerateLeg>,
    /// Number of geometric rounds attempted.
    pub rounds: usize,
    /// True only when the round cap forced exact full-list materialization.
    pub budget_exhausted: bool,
    /// Termination reason.
    pub termination: FusionTermination,
    /// Policy data version used for rule selection.
    pub alpha_policy_version: u16,
    /// Shared interpretation epoch supplied by the caller.
    pub epoch: Option<crate::epoch::EpochIdentity>,
}

/// One fused hit with both transparent raw leg scores.
#[derive(Clone, Debug, PartialEq)]
pub struct FusedHit<K> {
    /// Caller-defined common join key.
    pub key: K,
    /// Exact squared-L2, absent when the document appeared only lexically.
    pub vector_squared_l2: Option<f64>,
    /// Exact BM25, absent when the document appeared only in the vector leg.
    pub lexical_bm25: Option<f64>,
    /// Convex-combination or reciprocal-rank score, always higher-is-better.
    pub fused_score: f64,
}

/// Fused top-k and its report.
#[derive(Clone, Debug, PartialEq)]
pub struct FusionOutcome<K> {
    /// Deterministically ordered top-k.
    pub hits: Vec<FusedHit<K>>,
    /// Formula, rules, fallback, and termination facts.
    pub report: FusionReport,
}

/// Typed rejection from the pure fusion seam.
#[derive(Clone, Debug, PartialEq)]
pub enum FusionError {
    /// Alpha was NaN, infinite, or outside the closed unit interval.
    InvalidAlpha(f64),
    /// A raw score was NaN or infinite.
    NonFiniteScore {
        /// Affected leg.
        leg: FusionLeg,
        /// Zero-based rank.
        rank: usize,
    },
    /// Squared-L2 or BM25 was negative.
    NegativeScore {
        /// Affected leg.
        leg: FusionLeg,
        /// Zero-based rank.
        rank: usize,
    },
    /// The vector window still carried an estimated score.
    EstimatedVectorScore {
        /// Zero-based rank.
        rank: usize,
    },
    /// A bounded producer supplied a range its own window contradicts.
    InvalidBounds {
        /// Affected leg.
        leg: FusionLeg,
        /// Stable description of the violated bound.
        detail: &'static str,
    },
    /// A supposedly ranked input list changed score direction.
    UnrankedInput {
        /// Affected leg.
        leg: FusionLeg,
        /// Rank whose score violates the prior rank.
        rank: usize,
    },
    /// The caller could not map a leg-native id to the common identity.
    MissingDocumentIdentity {
        /// Affected leg.
        leg: FusionLeg,
        /// Zero-based rank.
        rank: usize,
    },
    /// Two candidates in one leg mapped to the same common identity.
    DuplicateDocumentIdentity {
        /// Affected leg.
        leg: FusionLeg,
        /// Zero-based rank of the duplicate.
        rank: usize,
    },
    /// A leg timed out; partial hits are never returned.
    Timeout {
        /// Permanently false.
        partial: bool,
    },
    /// A leg was cancelled; partial hits are never returned.
    Cancelled {
        /// Permanently false.
        partial: bool,
    },
    /// Store close cancelled a leg; partial hits are never returned.
    ReadCancelled {
        /// Permanently false.
        partial: bool,
    },
    /// A Store-owned leg thread could not be created.
    LegThreadStart {
        /// Leg whose required execution thread could not start.
        leg: FusionLeg,
        /// Stable operating-system error detail.
        detail: String,
    },
    /// A Store-owned leg panicked and was contained at the join seam.
    LegPanic {
        /// Leg that panicked.
        leg: FusionLeg,
        /// Stable detail that never exposes an unwind payload.
        detail: &'static str,
    },
    /// A caller-owned leg failed before fusion.
    Leg {
        /// Affected leg.
        leg: FusionLeg,
        /// Typed classification of the failure, so hosts do not parse `detail`.
        kind: LegFailureKind,
        /// Stable caller-provided failure description.
        detail: String,
    },
}

/// Typed classification of a leg failure carried by [`FusionError::Leg`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegFailureKind {
    /// Store admission, lifecycle, or persistence failed; see the kind.
    Store(crate::lifecycle::StoreErrorKind),
    /// Scan-tier execution failed.
    Scan,
    /// Graph-tier execution failed.
    Graph,
    /// An immutable segment read failed.
    Segment,
    /// Lexical planning, indexing, or scoring failed.
    Lexical,
    /// A leg-internal invariant failed.
    Invariant,
    /// A caller-owned leg reported an opaque failure.
    Caller,
}

impl std::fmt::Display for FusionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidAlpha(alpha) => write!(formatter, "fusion alpha {alpha} is outside 0..=1"),
            Self::NonFiniteScore { leg, rank } => {
                write!(formatter, "{leg:?} score at rank {rank} is non-finite")
            }
            Self::NegativeScore { leg, rank } => {
                write!(formatter, "{leg:?} score at rank {rank} is negative")
            }
            Self::EstimatedVectorScore { rank } => {
                write!(
                    formatter,
                    "vector score at rank {rank} was not exactly rescored"
                )
            }
            Self::InvalidBounds { leg, detail } => {
                write!(formatter, "{leg:?} leg bounds are invalid: {detail}")
            }
            Self::UnrankedInput { leg, rank } => {
                write!(
                    formatter,
                    "{leg:?} input changes score direction at rank {rank}"
                )
            }
            Self::MissingDocumentIdentity { leg, rank } => write!(
                formatter,
                "{leg:?} candidate at rank {rank} lacks a common document identity"
            ),
            Self::DuplicateDocumentIdentity { leg, rank } => write!(
                formatter,
                "{leg:?} candidate at rank {rank} repeats a common document identity"
            ),
            Self::Timeout { partial } => {
                write!(
                    formatter,
                    "hybrid query deadline expired (partial={partial})"
                )
            }
            Self::Cancelled { partial } => {
                write!(formatter, "hybrid query was cancelled (partial={partial})")
            }
            Self::ReadCancelled { partial } => write!(
                formatter,
                "store close cancelled hybrid query (partial={partial})"
            ),
            Self::LegThreadStart { leg, detail } => {
                write!(formatter, "{leg:?} leg thread did not start: {detail}")
            }
            Self::LegPanic { leg, detail } => {
                write!(formatter, "{leg:?} leg panic was contained: {detail}")
            }
            Self::Leg { leg, kind, detail } => {
                write!(formatter, "{leg:?} leg failed ({kind:?}): {detail}")
            }
        }
    }
}

impl std::error::Error for FusionError {}

impl From<crate::lifecycle::QueryError> for FusionError {
    fn from(error: crate::lifecycle::QueryError) -> Self {
        match error {
            crate::lifecycle::QueryError::Timeout { .. } => Self::Timeout { partial: false },
            crate::lifecycle::QueryError::Cancelled { .. } => Self::Cancelled { partial: false },
            crate::lifecycle::QueryError::ReadCancelled { .. } => {
                Self::ReadCancelled { partial: false }
            }
            other => {
                let kind = match &other {
                    crate::lifecycle::QueryError::Store(error) => {
                        LegFailureKind::Store(error.kind())
                    }
                    crate::lifecycle::QueryError::Scan(_) => LegFailureKind::Scan,
                    crate::lifecycle::QueryError::Graph(_) => LegFailureKind::Graph,
                    crate::lifecycle::QueryError::Timeout { .. }
                    | crate::lifecycle::QueryError::Cancelled { .. }
                    | crate::lifecycle::QueryError::ReadCancelled { .. } => LegFailureKind::Caller,
                };
                Self::Leg {
                    leg: FusionLeg::Vector,
                    kind,
                    detail: other.to_string(),
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct JoinedVector<K> {
    pub(crate) key: K,
    pub(crate) squared_l2: f64,
}

#[derive(Clone, Debug)]
pub(crate) struct JoinedLexical<K> {
    pub(crate) key: K,
    pub(crate) bm25: f64,
}

/// Executes caller-owned legs and fuses only when both complete successfully.
///
/// The closures are the library seam for a caller holding a `Store` and a
/// `LexicalIndex`. A typed timeout or cancellation escapes without partial
/// fused hits.
pub fn execute_hybrid<K, V, L, VectorRun, LexicalRun, VectorJoin, LexicalJoin>(
    query: &HybridQuery,
    vector_run: VectorRun,
    lexical_run: LexicalRun,
    vector_join: VectorJoin,
    lexical_join: LexicalJoin,
) -> Result<FusionOutcome<K>, FusionError>
where
    K: Clone + Ord,
    VectorRun: FnOnce() -> Result<Vec<VectorCandidate<V>>, FusionError>,
    LexicalRun: FnOnce() -> Result<Vec<LexicalCandidate<L>>, FusionError>,
    VectorJoin: FnMut(&V) -> Option<K>,
    LexicalJoin: FnMut(&L) -> Option<K>,
{
    let vector = vector_run()?;
    let lexical = lexical_run()?;
    fuse(query, &vector, &lexical, vector_join, lexical_join)
}

/// Fuses two ranked, exactly-scored candidate lists on a caller-defined key.
///
/// Vector scores must be ascending exact squared-L2 and lexical scores must be
/// descending exact BM25. Tie order in either input is irrelevant: fused ties
/// use the ordered common key.
pub fn fuse<K, V, L, VectorJoin, LexicalJoin>(
    query: &HybridQuery,
    vector: &[VectorCandidate<V>],
    lexical: &[LexicalCandidate<L>],
    mut vector_join: VectorJoin,
    mut lexical_join: LexicalJoin,
) -> Result<FusionOutcome<K>, FusionError>
where
    K: Clone + Ord,
    VectorJoin: FnMut(&V) -> Option<K>,
    LexicalJoin: FnMut(&L) -> Option<K>,
{
    let (effective_alpha, applied_rules) = rules::effective_alpha(query)?;
    let joined_vector = join_vector(vector, &mut vector_join)?;
    let joined_lexical = join_lexical(lexical, &mut lexical_join)?;
    cc::fuse_joined(
        query,
        &joined_vector,
        &joined_lexical,
        effective_alpha,
        applied_rules,
    )
}

/// Fuses two bounded windows plus their exact per-leg ranges.
///
/// This is the seam for a bounded producer. It differs from [`fuse`] in
/// exactly two ways: the normalization ranges come from `bounds` rather than
/// from the ends of the supplied lists, and stability is decided against the
/// supplied (W+1)-th values rather than against the next element of a
/// complete list. It scores each window once; there is no geometric round
/// loop, because widening means re-running the producers.
///
/// The caller must cross-fill: every document appearing in one window must
/// also appear in the other with its exact score, or be absent from the other
/// leg entirely. Under that contract every candidate this function can see
/// carries complete information, so only a document outside both windows can
/// still change the answer, and `bounds` decides whether one could.
///
/// Returns [`FusionTermination::WindowUnproven`] when it could not, which the
/// caller answers by widening the window and producing again.
///
/// # Errors
///
/// Returns [`FusionError`] for the same leg violations as [`fuse`], plus
/// [`FusionError::InvalidBounds`] when a supplied range is non-finite,
/// inverted, or contradicted by its own window.
pub fn fuse_bounded<K, V, L, VectorJoin, LexicalJoin>(
    query: &HybridQuery,
    vector_window: &[VectorCandidate<V>],
    lexical_window: &[LexicalCandidate<L>],
    bounds: LegBounds,
    mut vector_join: VectorJoin,
    mut lexical_join: LexicalJoin,
) -> Result<FusionOutcome<K>, FusionError>
where
    K: Clone + Ord,
    VectorJoin: FnMut(&V) -> Option<K>,
    LexicalJoin: FnMut(&L) -> Option<K>,
{
    let (effective_alpha, applied_rules) = rules::effective_alpha(query)?;
    let joined_vector = join_vector(vector_window, &mut vector_join)?;
    let joined_lexical = join_lexical(lexical_window, &mut lexical_join)?;
    cc::fuse_bounded_joined(
        query,
        &joined_vector,
        &joined_lexical,
        bounds,
        effective_alpha,
        applied_rules,
    )
}

/// Store-only entry: callers supply complete cross-scores and fixed anchors.
pub(crate) fn fuse_store_bounded<K, V, L, VectorJoin, LexicalJoin>(
    query: &HybridQuery,
    vector_window: &[VectorCandidate<V>],
    lexical_window: &[LexicalCandidate<L>],
    bounds: LegBounds,
    mut vector_join: VectorJoin,
    mut lexical_join: LexicalJoin,
    scratch: &mut StoreFusionScratch<K>,
) -> Result<FusionOutcome<K>, FusionError>
where
    K: Clone + Ord,
    VectorJoin: FnMut(&V) -> Option<K>,
    LexicalJoin: FnMut(&L) -> Option<K>,
{
    let (alpha, rules) = rules::effective_alpha(query)?;
    let vector = join_vector(vector_window, &mut vector_join)?;
    let lexical = join_lexical(lexical_window, &mut lexical_join)?;
    cc::fuse_store_bounded_joined(query, &vector, &lexical, bounds, alpha, rules, scratch)
}

fn join_vector<K, V, Join>(
    candidates: &[VectorCandidate<V>],
    join: &mut Join,
) -> Result<Vec<JoinedVector<K>>, FusionError>
where
    K: Clone + Ord,
    Join: FnMut(&V) -> Option<K>,
{
    let mut joined = Vec::with_capacity(candidates.len());
    let mut identities = BTreeSet::new();
    let mut previous = None;
    for (rank, candidate) in candidates.iter().enumerate() {
        if candidate.precision != ScorePrecision::Exact {
            return Err(FusionError::EstimatedVectorScore { rank });
        }
        if !candidate.squared_l2.is_finite() {
            return Err(FusionError::NonFiniteScore {
                leg: FusionLeg::Vector,
                rank,
            });
        }
        if candidate.squared_l2 < 0.0 {
            return Err(FusionError::NegativeScore {
                leg: FusionLeg::Vector,
                rank,
            });
        }
        if previous.is_some_and(|score| candidate.squared_l2 < score) {
            return Err(FusionError::UnrankedInput {
                leg: FusionLeg::Vector,
                rank,
            });
        }
        previous = Some(candidate.squared_l2);
        let key = join(&candidate.id).ok_or(FusionError::MissingDocumentIdentity {
            leg: FusionLeg::Vector,
            rank,
        })?;
        if !identities.insert(key.clone()) {
            return Err(FusionError::DuplicateDocumentIdentity {
                leg: FusionLeg::Vector,
                rank,
            });
        }
        joined.push(JoinedVector {
            key,
            squared_l2: candidate.squared_l2,
        });
    }
    joined.sort_by(|left, right| {
        left.squared_l2
            .total_cmp(&right.squared_l2)
            .then_with(|| left.key.cmp(&right.key))
    });
    Ok(joined)
}

fn join_lexical<K, L, Join>(
    candidates: &[LexicalCandidate<L>],
    join: &mut Join,
) -> Result<Vec<JoinedLexical<K>>, FusionError>
where
    K: Clone + Ord,
    Join: FnMut(&L) -> Option<K>,
{
    let mut joined = Vec::with_capacity(candidates.len());
    let mut identities = BTreeSet::new();
    let mut previous = None;
    for (rank, candidate) in candidates.iter().enumerate() {
        if !candidate.bm25.is_finite() {
            return Err(FusionError::NonFiniteScore {
                leg: FusionLeg::Lexical,
                rank,
            });
        }
        if candidate.bm25 < 0.0 {
            return Err(FusionError::NegativeScore {
                leg: FusionLeg::Lexical,
                rank,
            });
        }
        if previous.is_some_and(|score| candidate.bm25 > score) {
            return Err(FusionError::UnrankedInput {
                leg: FusionLeg::Lexical,
                rank,
            });
        }
        previous = Some(candidate.bm25);
        let key = join(&candidate.id).ok_or(FusionError::MissingDocumentIdentity {
            leg: FusionLeg::Lexical,
            rank,
        })?;
        if !identities.insert(key.clone()) {
            return Err(FusionError::DuplicateDocumentIdentity {
                leg: FusionLeg::Lexical,
                rank,
            });
        }
        joined.push(JoinedLexical {
            key,
            bm25: candidate.bm25,
        });
    }
    joined.sort_by(|left, right| {
        right
            .bm25
            .total_cmp(&left.bm25)
            .then_with(|| left.key.cmp(&right.key))
    });
    Ok(joined)
}
