use zeppelin_embed::epoch::EpochIdentity;
use zeppelin_embed::lifecycle::{ScanRescoreOptions, SearchTier};

/// Retrieval legs selected by a text query.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum Legs {
    /// Dense vector retrieval only.
    Dense = 1,
    /// Lexical BM25 retrieval only.
    Lexical = 2,
    /// Dense and lexical fusion using the bundle alpha.
    #[default]
    Hybrid = 3,
}

/// Options for one text query.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueryOptions {
    pub(crate) k: usize,
    pub(crate) legs: Legs,
    pub(crate) tier: Option<SearchTier>,
    pub(crate) scan_rescore: Option<ScanRescoreOptions>,
}

impl QueryOptions {
    /// Constructs a hybrid query returning at most `k` hits.
    #[must_use]
    pub const fn new(k: usize) -> Self {
        Self {
            k,
            legs: Legs::Hybrid,
            tier: None,
            scan_rescore: None,
        }
    }

    /// Selects which retrieval legs execute.
    #[must_use]
    pub const fn with_legs(mut self, legs: Legs) -> Self {
        self.legs = legs;
        self
    }

    /// Selects the vector-search tier used by dense and hybrid retrieval.
    ///
    /// Leaving this unset is not the same as selecting [`SearchTier::Auto`].
    /// An unset tier lets each leg apply its own contract. Hybrid selects
    /// [`SearchTier::Auto`] when the snapshot has a published graph and
    /// [`SearchTier::Exact`] otherwise; an explicit tier is honoured as given.
    #[must_use]
    pub const fn with_tier(mut self, tier: SearchTier) -> Self {
        self.tier = Some(tier);
        self.scan_rescore = None;
        self
    }

    /// Selects quantized candidates and scores selected rows exactly.
    ///
    /// Dense and hybrid membership may be approximate. The controls apply to
    /// each segment and vector-producer invocation, including hybrid frontiers.
    /// Lexical-only queries do not use these controls. Selecting a tier later
    /// clears this mode and restores that tier's ordinary contract.
    #[must_use]
    pub const fn with_scan_rescore(mut self, options: ScanRescoreOptions) -> Self {
        self.tier = Some(SearchTier::Scan);
        self.scan_rescore = Some(options);
        self
    }

    /// Applies a tier only when the caller expressed one.
    ///
    /// `None` leaves the tier unset, which is distinct from
    /// `Some(SearchTier::Auto)`.
    #[must_use]
    pub const fn with_optional_tier(mut self, tier: Option<SearchTier>) -> Self {
        self.tier = tier;
        self.scan_rescore = None;
        self
    }
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self::new(10)
    }
}

/// Loaded query runtime selection. CoreML compute units are requested policy;
/// they are not evidence that the Neural Engine executed each operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueryBackend {
    /// Identity reported by the loaded runtime, rather than bundle metadata.
    pub runtime: crate::runtime::RuntimeIdentity,
    /// Compute units requested when constructing that runtime.
    pub requested_compute_units: zeppelin_embed::epoch::ComputeUnits,
    /// Independently observed hardware routing, when available.
    pub observed_compute_units: Option<zeppelin_embed::epoch::ComputeUnits>,
    /// Fixed CoreML input width, or `None` for dynamic MLX inputs.
    pub sequence_length: Option<usize>,
}

/// Optional version 1 outer query spans. Retrieval includes overlapping core
/// stages; neither those stages nor their medians should be summed with it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TextQueryTimings {
    /// Model tokenization including bundle prefix construction.
    pub tokenization: std::time::Duration,
    /// Lexical analyzer and term-query construction.
    pub lexical_analysis: std::time::Duration,
    /// Bounded embed-channel wait until the embed worker starts this request.
    pub embedding_queue: std::time::Duration,
    /// Runtime call wall time, including native synchronization and runtime locks.
    pub embedding_evaluation: std::time::Duration,
    /// Output dimension/unit-norm validation and normalization.
    pub embedding_normalization: std::time::Duration,
    /// Core retrieval including admission, both legs and fusion. Hybrid also
    /// includes deferred embedding, which overlaps lexical work; stage times
    /// therefore must not be added to derive end-to-end latency.
    pub retrieval: std::time::Duration,
    /// Returned text and revision construction, including its store lookups.
    pub materialization: std::time::Duration,
    /// Inclusive outer duration finalized after returned hits are constructed.
    pub end_to_end: std::time::Duration,
}

/// Version 1 text-query evidence, additive to the existing hit-only method.
#[derive(Debug)]
pub struct TextQueryOutcome {
    /// The same ordered hits returned by `TextStore::query_text`.
    pub hits: Vec<TextHit>,
    /// Core execution facts; absent for k=0, when no query is admitted.
    pub diagnostics: Option<zeppelin_embed::diag::QueryDiagnostics>,
    /// Stage clocks; absent unless the core `query-timing` feature is enabled.
    pub timings: Option<TextQueryTimings>,
    /// Runtime actually selected; absent for lexical-only and k=0 queries.
    pub backend: Option<QueryBackend>,
    /// Nonpadding model input tokens, including the model's query prefix.
    pub query_tokens: usize,
    /// Completed query embedding calls; zero for lexical-only and k=0 queries.
    pub embedding_calls: usize,
}

/// One text-bearing retrieval result.
#[derive(Clone, Debug, PartialEq)]
pub struct TextHit {
    /// Caller document id.
    pub doc_id: u128,
    /// Caller document revision.
    pub revision: u64,
    /// Zero-based text chunk.
    pub chunk: u32,
    /// Exact stored chunk text.
    pub text: String,
    /// Larger-is-better selected or fused score.
    pub score: f64,
    /// Exact squared L2 from the dense leg, when present.
    pub vector_squared_l2: Option<f64>,
    /// Exact BM25 score from the lexical leg, when present.
    pub lexical_bm25: Option<f64>,
    /// Bundle-derived epoch used by both store and query.
    pub epoch: EpochIdentity,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_options_default_to_hybrid_ten_and_allow_an_explicit_leg() {
        let default = QueryOptions::default();
        assert_eq!(default.k, 10);
        assert_eq!(default.legs, Legs::Hybrid);
        assert_eq!(default.tier, None);
        let dense = QueryOptions::new(3).with_legs(Legs::Dense);
        assert_eq!(dense.k, 3);
        assert_eq!(dense.legs, Legs::Dense);
    }
}
