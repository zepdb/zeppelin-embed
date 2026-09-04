use zeppelin_embed::epoch::EpochIdentity;

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
}

impl QueryOptions {
    /// Constructs a hybrid query returning at most `k` hits.
    #[must_use]
    pub const fn new(k: usize) -> Self {
        Self {
            k,
            legs: Legs::Hybrid,
        }
    }

    /// Selects which retrieval legs execute.
    #[must_use]
    pub const fn with_legs(mut self, legs: Legs) -> Self {
        self.legs = legs;
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
        let dense = QueryOptions::new(3).with_legs(Legs::Dense);
        assert_eq!(dense.k, 3);
        assert_eq!(dense.legs, Legs::Dense);
    }
}
