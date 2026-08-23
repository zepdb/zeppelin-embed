//! Task 13 scorer reference tests: hand-computed BM25 on a tiny corpus.
#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use zeppelin_embed::fts::bm25::{Bm25Params, CorpusStats, Df, DocLen, Tf, idf, term_score};

/// Absolute tolerance for values computed by hand to six decimal places.
const TOLERANCE: f64 = 1e-6;

fn close(actual: f64, expected: f64, what: &str) {
    assert!(
        (actual - expected).abs() < TOLERANCE,
        "{what}: got {actual}, expected {expected}"
    );
}

#[test]
fn idf_is_the_lucene_variant_not_the_classic_robertson_form() {
    // Lucene: ln(1 + (N - df + 0.5) / (df + 0.5)).
    // N = 10, df = 3  ->  ln(1 + 7.5/3.5) = ln(3.142857142857143)
    close(idf(Df(3), 10), (1.0 + 7.5 / 3.5_f64).ln(), "N=10 df=3");

    // The classic Robertson form ln((N - df + 0.5)/(df + 0.5)) goes
    // NEGATIVE once df > N/2; the Lucene variant never does. df = 8, N = 10:
    // Robertson would give ln(2.5/8.5) < 0.
    assert!(
        idf(Df(8), 10) > 0.0,
        "the Lucene variant must never produce a negative idf"
    );
    close(idf(Df(8), 10), (1.0 + 2.5 / 8.5_f64).ln(), "N=10 df=8");

    // df == N is the degenerate case: ln(1 + 0.5/(N+0.5)).
    close(idf(Df(10), 10), (1.0 + 0.5 / 10.5_f64).ln(), "df == N");
}

#[test]
fn idf_is_monotonically_non_increasing_in_df() {
    let mut previous = f64::INFINITY;
    for df in 1..=64_u32 {
        let value = idf(Df(df), 64);
        assert!(value <= previous, "idf rose at df={df}");
        assert!(value >= 0.0, "idf went negative at df={df}");
        previous = value;
    }
}

#[test]
fn bm25_term_score_matches_hand_computed_values_for_the_tiny_corpus() {
    // Tiny corpus, computed by hand at k1 = 1.2, b = 0.75:
    //   N = 4 documents, avgdl = 5.0
    //   term "engine": df = 2  ->  idf = ln(1 + 2.5/2.5) = ln(2)
    //   doc A: tf = 3, len = 5  (len == avgdl, so the norm is exactly 1)
    //     denominator = tf + k1 * (1 - b + b * len/avgdl)
    //                 = 3 + 1.2 * (1 - 0.75 + 0.75 * 1.0) = 3 + 1.2 = 4.2
    //     score = ln(2) * 3 * (1.2 + 1) / 4.2 = ln(2) * 6.6 / 4.2
    let stats = CorpusStats::new(4, 20).expect("4 docs, 20 tokens -> avgdl 5");
    close(stats.average_document_length(), 5.0, "avgdl");

    let params = Bm25Params::default();
    assert!((params.k1 - 1.2).abs() < TOLERANCE);
    assert!((params.b - 0.75).abs() < TOLERANCE);

    close(
        term_score(Tf(3), Df(2), DocLen(5), &stats, params),
        2.0_f64.ln() * 6.6 / 4.2,
        "doc A",
    );

    //   doc B: tf = 1, len = 10 (twice avgdl)
    //     denominator = 1 + 1.2 * (0.25 + 0.75 * 2.0) = 1 + 1.2 * 1.75 = 3.1
    //     score = ln(2) * 1 * 2.2 / 3.1
    close(
        term_score(Tf(1), Df(2), DocLen(10), &stats, params),
        2.0_f64.ln() * 2.2 / 3.1,
        "doc B",
    );
}

#[test]
fn a_zero_frequency_term_scores_zero() {
    let stats = CorpusStats::new(4, 20).expect("valid stats");
    close(
        term_score(Tf(0), Df(2), DocLen(5), &stats, Bm25Params::default()),
        0.0,
        "tf == 0",
    );
}

#[test]
fn k1_of_zero_makes_the_score_independent_of_term_frequency() {
    // With k1 = 0 the saturation term collapses to idf * tf / tf = idf.
    let stats = CorpusStats::new(4, 20).expect("valid stats");
    let params = Bm25Params::new(0.0, 0.75).expect("k1 = 0 is legal");
    let expected = idf(Df(2), 4);
    for tf in [1_u32, 2, 7, 1000] {
        close(
            term_score(Tf(tf), Df(2), DocLen(5), &stats, params),
            expected,
            "k1 = 0",
        );
    }
}

#[test]
fn b_of_zero_makes_the_score_independent_of_document_length() {
    let stats = CorpusStats::new(4, 20).expect("valid stats");
    let params = Bm25Params::new(1.2, 0.0).expect("b = 0 is legal");
    let baseline = term_score(Tf(3), Df(2), DocLen(5), &stats, params);
    for length in [1_u32, 5, 50, 5000] {
        close(
            term_score(Tf(3), Df(2), DocLen(length), &stats, params),
            baseline,
            "b = 0",
        );
    }
}

#[test]
fn b_of_one_applies_the_full_length_normalization() {
    // At b = 1 the denominator is tf + k1 * len/avgdl.
    let stats = CorpusStats::new(4, 20).expect("valid stats");
    let params = Bm25Params::new(1.2, 1.0).expect("b = 1 is legal");
    let expected = idf(Df(2), 4) * 3.0 * 2.2 / (3.0 + 1.2 * 10.0 / 5.0);
    close(
        term_score(Tf(3), Df(2), DocLen(10), &stats, params),
        expected,
        "b = 1",
    );
}

#[test]
fn a_huge_term_frequency_saturates_below_the_idf_ceiling() {
    // BM25's whole point: tf saturates at idf * (k1 + 1).
    let stats = CorpusStats::new(4, 20).expect("valid stats");
    let params = Bm25Params::default();
    let ceiling = idf(Df(2), 4) * (params.k1 + 1.0);
    let score = term_score(Tf(u32::MAX), Df(2), DocLen(5), &stats, params);
    assert!(score < ceiling, "score {score} reached the ceiling {ceiling}");
    assert!(
        score > ceiling * 0.999,
        "score {score} did not approach the ceiling {ceiling}"
    );
}

#[test]
fn a_single_token_document_scores_finitely() {
    let stats = CorpusStats::new(4, 20).expect("valid stats");
    let score = term_score(Tf(1), Df(1), DocLen(1), &stats, Bm25Params::default());
    assert!(score.is_finite() && score > 0.0, "got {score}");
}

#[test]
fn higher_score_is_always_better() {
    // The FTS5 trap: SQLite returns NEGATIVE bm25 so that ORDER BY ascending
    // puts the best row first. This engine never does that.
    let stats = CorpusStats::new(4, 20).expect("valid stats");
    let params = Bm25Params::default();
    let rare = term_score(Tf(2), Df(1), DocLen(5), &stats, params);
    let common = term_score(Tf(2), Df(4), DocLen(5), &stats, params);
    assert!(rare > common, "a rarer term must score higher");
    assert!(rare > 0.0 && common > 0.0, "scores must be positive");
}

#[test]
fn degenerate_parameters_and_stats_are_typed_errors() {
    assert!(Bm25Params::new(-0.1, 0.5).is_err(), "negative k1");
    assert!(Bm25Params::new(1.2, -0.1).is_err(), "b below zero");
    assert!(Bm25Params::new(1.2, 1.1).is_err(), "b above one");
    assert!(Bm25Params::new(f64::NAN, 0.5).is_err(), "NaN k1");
    assert!(CorpusStats::new(0, 0).is_err(), "an empty corpus has no avgdl");
    assert!(CorpusStats::new(4, 0).is_err(), "zero tokens has no avgdl");
}

#[test]
fn anserini_defaults_are_available_and_documented() {
    let anserini = Bm25Params::anserini();
    assert!((anserini.k1 - 0.9).abs() < TOLERANCE);
    assert!((anserini.b - 0.4).abs() < TOLERANCE);
}
