use zeppelin_embed::epoch::EpochIdentity;
use zeppelin_embed::lifecycle::SearchTier;

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
}

impl QueryOptions {
    /// Constructs a hybrid query returning at most `k` hits.
    #[must_use]
    pub const fn new(k: usize) -> Self {
        Self {
            k,
            legs: Legs::Hybrid,
            tier: None,
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
        self
    }

    /// Applies a tier only when the caller expressed one.
    ///
    /// `None` leaves the tier unset, which is distinct from
    /// `Some(SearchTier::Auto)`.
    #[must_use]
    pub const fn with_optional_tier(mut self, tier: Option<SearchTier>) -> Self {
        self.tier = tier;
        self
    }
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self::new(10)
    }
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
