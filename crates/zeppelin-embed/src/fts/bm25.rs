//! The BM25 scorer.
//!
//! # The formula never mattered; the pipeline did
//!
//! All eight BM25 variants are statistically indistinguishable (Kamphuis
//! ECIR'20, `research/02a:40`), so this module is deliberately boring and
//! the effort goes where the measured differences live: analysis
//! (`super::tokenizer`) and correct corpus-wide statistics. The prior
//! engine's ~30% nDCG@10 gap was a pipeline defect, not a formula choice
//! (`research/02a:256`, `:269`).
//!
//! # Global statistics, never segment-local
//!
//! [`CorpusStats`] carries `doc_count` and `total_tokens` for the WHOLE
//! store, across every segment. Segment-local IDF is a documented
//! real-world failure (Milvus Lite) and the prime suspect for that 30% gap.
//! `prop_engine_bm25_equals_model` is what makes it unconstructible here:
//! it randomizes seal boundaries, so any statistic computed per segment
//! diverges from the brute-force model immediately.
//!
//! # Sign convention
//!
//! **Higher is better.** SQLite FTS5 returns a NEGATIVE bm25 so that
//! `ORDER BY rank ASC` puts the best row first; porting a query from FTS5
//! and keeping its sort direction silently inverts relevance. This engine
//! never negates. Every score here is non-negative.
//!
//! # Argument order is pinned by types
//!
//! `bm25_term_score(tf, df, len)` invites a silent transposition — three
//! integers, any order compiles. [`Tf`], [`Df`], and [`DocLen`] are
//! separate newtypes so a swap is a compile error. This trap is recorded in
//! the prior engine's own notes; it is not hypothetical.

/// Term frequency: occurrences of one term within one document.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Tf(pub u32);

/// Document frequency: documents containing one term, across the store.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Df(pub u32);

/// Document length in tokens after analysis.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DocLen(pub u32);

/// A rejected scorer configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Bm25Error {
    /// `k1` was negative or not a number.
    InvalidK1,
    /// `b` was outside `0.0..=1.0` or not a number.
    InvalidB,
    /// The corpus had no documents, so `avgdl` is undefined.
    EmptyCorpus,
    /// The corpus had no tokens, so `avgdl` is undefined.
    NoTokens,
}

impl std::fmt::Display for Bm25Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::InvalidK1 => "bm25 k1 must be a finite value at or above zero",
            Self::InvalidB => "bm25 b must be a finite value in 0.0..=1.0",
            Self::EmptyCorpus => "corpus statistics require at least one document",
            Self::NoTokens => "corpus statistics require at least one analyzed token",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for Bm25Error {}

/// The two BM25 free parameters.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bm25Params {
    /// Term-frequency saturation. Higher means tf keeps mattering longer.
    pub k1: f64,
    /// Length-normalization strength, `0.0..=1.0`.
    pub b: f64,
}

impl Bm25Params {
    /// The BEIR-paper defaults, `k1 = 1.2`, `b = 0.75`.
    ///
    /// These are what the published BEIR numbers task 13 gates against were
    /// produced with, so they are the default here.
    #[must_use]
    pub const fn beir() -> Self {
        Self { k1: 1.2, b: 0.75 }
    }

    /// Anserini's and Pyserini's defaults, `k1 = 0.9`, `b = 0.4`.
    ///
    /// Documented as the alternative. Domain tuning is claimed to be worth
    /// 5-15%, but no systematic winner exists across corpora, so this is
    /// exposed rather than chased.
    #[must_use]
    pub const fn anserini() -> Self {
        Self { k1: 0.9, b: 0.4 }
    }

    /// Builds validated parameters.
    ///
    /// # Errors
    ///
    /// Returns [`Bm25Error::InvalidK1`] or [`Bm25Error::InvalidB`] when a
    /// value is outside its domain or is not a number.
    pub fn new(k1: f64, b: f64) -> Result<Self, Bm25Error> {
        if !k1.is_finite() || k1 < 0.0 {
            return Err(Bm25Error::InvalidK1);
        }
        if !b.is_finite() || !(0.0..=1.0).contains(&b) {
            return Err(Bm25Error::InvalidB);
        }
        Ok(Self { k1, b })
    }
}

impl Default for Bm25Params {
    fn default() -> Self {
        Self::beir()
    }
}

/// Store-wide corpus statistics.
///
/// These live in the manifest and are updated at seal/commit, which is the
/// manifest's only writer. They are never derived per segment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CorpusStats {
    doc_count: u64,
    total_tokens: u64,
}

impl CorpusStats {
    /// Builds validated statistics.
    ///
    /// # Errors
    ///
    /// Returns [`Bm25Error::EmptyCorpus`] or [`Bm25Error::NoTokens`] when
    /// `avgdl` would be undefined. An empty corpus is not scored; it is
    /// refused, because a silent `avgdl` of zero produces infinities.
    pub const fn new(doc_count: u64, total_tokens: u64) -> Result<Self, Bm25Error> {
        if doc_count == 0 {
            return Err(Bm25Error::EmptyCorpus);
        }
        if total_tokens == 0 {
            return Err(Bm25Error::NoTokens);
        }
        Ok(Self {
            doc_count,
            total_tokens,
        })
    }

    /// Returns the store-wide document count, `N`.
    #[must_use]
    pub const fn document_count(&self) -> u64 {
        self.doc_count
    }

    /// Returns the store-wide analyzed token count.
    #[must_use]
    pub const fn total_tokens(&self) -> u64 {
        self.total_tokens
    }

    /// Returns `avgdl`, counted in tokens after analysis.
    ///
    /// The unit matters: `avgdl` must be counted over the same unit being
    /// scored, in analyzed tokens, never in raw bytes or pre-analysis words.
    #[must_use]
    pub fn average_document_length(&self) -> f64 {
        // Both counts are validated non-zero, so this cannot divide by zero.
        self.total_tokens as f64 / self.doc_count as f64
    }
}

/// The Lucene-variant inverse document frequency.
///
/// `ln(1 + (N - df + 0.5) / (df + 0.5))`.
///
/// The classic Robertson form omits the `1 +` and goes negative once a term
/// appears in more than half the corpus, which lets a common term subtract
/// from a document's score. The Lucene variant is non-negative everywhere,
/// which is what makes the block-max upper bounds in task 14 sound: a bound
/// built from per-term maxima is only an upper bound if no term can
/// contribute a negative amount.
#[must_use]
pub fn idf(df: Df, doc_count: u64) -> f64 {
    let n = doc_count as f64;
    let df = f64::from(df.0).min(n);
    (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
}

/// Scores one term against one document.
///
/// `idf * (tf * (k1 + 1)) / (tf + k1 * (1 - b + b * len / avgdl))`.
///
/// Returns zero when `tf` is zero, so a term absent from a document
/// contributes nothing rather than a length-only artefact.
#[must_use]
pub fn term_score(
    tf: Tf,
    df: Df,
    length: DocLen,
    stats: &CorpusStats,
    params: Bm25Params,
) -> f64 {
    if tf.0 == 0 {
        return 0.0;
    }
    let frequency = f64::from(tf.0);
    let normalization = 1.0 - params.b
        + params.b * f64::from(length.0) / stats.average_document_length();
    let denominator = frequency + params.k1 * normalization;
    if denominator <= 0.0 {
        // Only reachable with k1 = 0 and tf = 0, which returned above.
        return 0.0;
    }
    idf(df, stats.doc_count) * (frequency * (params.k1 + 1.0)) / denominator
}

/// The largest score one term can contribute to any document.
///
/// Used by task 14 to build block-max upper bounds. The term-frequency
/// factor saturates at `k1 + 1` as `tf` grows without bound, and the length
/// normalization is smallest for the shortest document, so the ceiling is
/// `idf * (k1 + 1)`. Any real score is strictly below it.
#[must_use]
pub fn term_score_ceiling(df: Df, stats: &CorpusStats, params: Bm25Params) -> f64 {
    idf(df, stats.doc_count) * (params.k1 + 1.0)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use super::*;

    #[test]
    fn newtypes_prevent_argument_transposition() {
        // This test exists to document the trap rather than to exercise
        // behaviour: swapping the first two arguments below is a type error,
        // which is the entire point of Tf/Df/DocLen being distinct.
        let stats = CorpusStats::new(4, 20).expect("valid stats");
        let score = term_score(Tf(3), Df(2), DocLen(5), &stats, Bm25Params::default());
        assert!(score > 0.0);
    }

    #[test]
    fn the_ceiling_bounds_every_reachable_score() {
        let stats = CorpusStats::new(1_000, 50_000).expect("valid stats");
        let params = Bm25Params::default();
        for df in [1_u32, 2, 17, 500, 1_000] {
            let ceiling = term_score_ceiling(Df(df), &stats, params);
            for tf in [1_u32, 3, 40, 10_000, u32::MAX] {
                for length in [1_u32, 5, 50, 5_000, u32::MAX] {
                    let score = term_score(Tf(tf), Df(df), DocLen(length), &stats, params);
                    assert!(
                        score <= ceiling,
                        "score {score} exceeded ceiling {ceiling} at df={df} tf={tf} len={length}"
                    );
                }
            }
        }
    }

    #[test]
    fn scores_are_never_negative() {
        let stats = CorpusStats::new(10, 100).expect("valid stats");
        for params in [Bm25Params::beir(), Bm25Params::anserini()] {
            for df in 1..=10_u32 {
                for tf in [0_u32, 1, 9] {
                    for length in [1_u32, 10, 1_000] {
                        let score = term_score(Tf(tf), Df(df), DocLen(length), &stats, params);
                        assert!(score >= 0.0, "negative score {score}");
                        assert!(score.is_finite(), "non-finite score {score}");
                    }
                }
            }
        }
    }

    #[test]
    fn df_above_the_document_count_is_clamped_rather_than_producing_a_negative_idf() {
        // A caller that sums df across segments while a delete is in flight
        // can momentarily exceed N. Clamping keeps idf non-negative; without
        // it the log argument could fall below one.
        let value = idf(Df(20), 10);
        assert!(value >= 0.0, "clamping failed: {value}");
        assert!((value - idf(Df(10), 10)).abs() < 1e-12);
    }

    #[test]
    fn average_document_length_uses_analyzed_tokens() {
        let stats = CorpusStats::new(3, 30).expect("valid stats");
        assert!((stats.average_document_length() - 10.0).abs() < 1e-12);
        assert_eq!(stats.document_count(), 3);
        assert_eq!(stats.total_tokens(), 30);
    }

    #[test]
    fn errors_render_useful_text() {
        assert!(Bm25Error::InvalidK1.to_string().contains("k1"));
        assert!(Bm25Error::InvalidB.to_string().contains("b"));
        assert!(Bm25Error::EmptyCorpus.to_string().contains("document"));
        assert!(Bm25Error::NoTokens.to_string().contains("token"));
    }
}
