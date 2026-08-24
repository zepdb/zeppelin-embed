//! Task 14's contract: pruning changes speed, never results.
//!
//! `prop_pruned_topk_equals_exhaustive` is the permanent guard. Any bound
//! bug — a quantization that rounds the wrong way, a skip that jumps one
//! document too far, a threshold compared with the wrong strictness — lands
//! here as a changed result rather than as a silently missing document.
#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use proptest::prelude::*;
use proptest::test_runner::{Config as ProptestConfig, RngSeed};

use zeppelin_embed::fts::bm25::Bm25Params;
use zeppelin_embed::fts::index::{DEFAULT_FIELD, Document, LexicalIndex, SegmentIndex};
use zeppelin_embed::fts::prune::{Strategy as Prune, search_pruned, select_strategy};
use zeppelin_embed::fts::search::{TermQuery, search};
use zeppelin_embed::fts::tokenizer::{Analyzer, Profile, TokenizerConfig};

/// Scores must agree to the last representable bit, not merely closely:
/// both paths run the identical scoring function on identical inputs.
const EXACT: f64 = 1e-12;

fn analyzer() -> Analyzer {
    Analyzer::new(TokenizerConfig::text_default()).expect("valid configuration")
}

/// A vocabulary with a deliberately skewed frequency profile.
///
/// Uniform vocabularies are the classic false green for pruning: every term
/// has the same bound, so the essential/non-essential split never moves and
/// the pivot logic is never exercised. `common` appears in most documents,
/// `rare` in very few.
const COMMON: [&str; 4] = ["common", "frequent", "the", "engine"];
const MIDDLE: [&str; 4] = ["tuning", "lexical", "postings", "search"];
const RARE: [&str; 4] = ["quokka", "zeugma", "i-485", "put_if_match"];

fn document_text() -> impl Strategy<Value = String> {
    (
        prop::collection::vec(prop::sample::select(COMMON.as_slice()), 0..8),
        prop::collection::vec(prop::sample::select(MIDDLE.as_slice()), 0..4),
        prop::collection::vec(prop::sample::select(RARE.as_slice()), 0..2),
    )
        .prop_map(|(common, middle, rare)| {
            let mut words: Vec<&str> = Vec::new();
            words.extend(common);
            words.extend(middle);
            words.extend(rare);
            words.join(" ")
        })
}

fn query_words() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(
        prop::sample::select(
            COMMON
                .iter()
                .chain(MIDDLE.iter())
                .chain(RARE.iter())
                .copied()
                .collect::<Vec<_>>(),
        ),
        1..6,
    )
    .prop_map(|words| words.into_iter().map(str::to_owned).collect())
}

fn build_index(analyzer: &Analyzer, texts: &[String], boundaries: &[usize]) -> LexicalIndex {
    let mut index = LexicalIndex::new();
    let mut cursor = 0_usize;
    for size in boundaries {
        let mut segment = SegmentIndex::new();
        for text in texts.iter().skip(cursor).take(*size) {
            segment
                .push_document(analyzer, &Document::with_text(text))
                .expect("indexable");
        }
        cursor += size;
        index.push_segment(segment);
    }
    index
}

fn seal_boundaries(total: usize, cuts: &[usize]) -> Vec<usize> {
    if total == 0 {
        return vec![0];
    }
    let mut points: Vec<usize> = cuts.iter().map(|cut| cut % total).collect();
    points.sort_unstable();
    points.dedup();
    let mut sizes = Vec::new();
    let mut previous = 0_usize;
    for point in points {
        if point > previous {
            sizes.push(point - previous);
            previous = point;
        }
    }
    if total > previous {
        sizes.push(total - previous);
    }
    if sizes.is_empty() { vec![total] } else { sizes }
}

fn analyze_query(analyzer: &Analyzer, words: &[String]) -> Vec<Vec<u8>> {
    let mut terms: Vec<Vec<u8>> = Vec::new();
    for word in words {
        for token in analyzer.analyze(word) {
            let bytes = token.term.into_bytes();
            if !terms.contains(&bytes) {
                terms.push(bytes);
            }
        }
    }
    terms
}

/// Plan FTS-optimizations P2.5 guard 2, landed ahead of the wiring it
/// guards.
///
/// # Why this test exists before the code it protects
///
/// The persisted `u8 block_max` is `quantize(score / ceiling)`, and that
/// fraction reduces to `tf / (tf + k1 * (1 - b + b * len / avgdl))`. `idf`
/// cancels, so drift in `df` and `N` is harmless. `avgdl` and `(k1, b)` do
/// not cancel: a bound sealed when documents were short is too low once
/// longer documents arrive, and an index sealed under `beir()` and queried
/// under `anserini()` carries stale bounds. A bound that is too low lets
/// pruning skip a document that belonged in the top-k, which is a wrong
/// answer rather than a slow one. `tests/block_max_soundness.rs` measures
/// the shortfall at 15.21% and 19.56%.
///
/// Today this test CANNOT FAIL, because `search_pruned` rebuilds bounds
/// from live statistics and never reads the stored byte. That is exactly
/// why it is written now: the defect bites whichever change first reads
/// that byte, and this is the shape that catches it. When the sealed format
/// reaches the query path, this test starts being able to fail — and if it
/// were written afterwards, it would be written by someone who had already
/// convinced themselves the bounds were fine.
///
/// Segment A holds short documents; segment B holds documents an order of
/// magnitude longer, so `avgdl` rises sharply between them. That is the
/// direction that breaks the bound: a larger `avgdl` shrinks `len / avgdl`,
/// shrinks the denominator, and grows the fraction.
///
/// # The fixture detail that gives this test teeth
///
/// A single-term BM25 score is monotone in document length, so the winners
/// are the shortest documents. If every short document sits in the first
/// segment, a scan in row order finds the whole top-k before any bound is
/// ever consulted, and the test passes no matter how badly the bounds
/// under-state — verified by injecting a stale-`avgdl` bound, which this
/// test's first draft failed to catch. Segment B therefore carries
/// occasional two-token documents that outrank everything in segment A and
/// sit late in traversal order, so an over-aggressive skip loses a result
/// the exhaustive scorer keeps.
#[test]
fn pruning_agrees_after_average_document_length_drifts_across_segments() {
    let analyzer = analyzer();
    let mut texts: Vec<String> = Vec::new();
    // Segment A: short documents, every term present so the lists span
    // several blocks and pruning actually engages.
    for ordinal in 0..320_usize {
        let padding = vec!["filler"; 4 + ordinal % 11].join(" ");
        texts.push(format!("common engine rare{} {padding}", ordinal % 7));
    }
    let short = texts.len();
    // Segment B: mostly documents an order of magnitude longer, which is
    // what moves avgdl. Every 37th is a two-token document that outranks
    // everything in segment A while sitting late in traversal order.
    for ordinal in 0..320_usize {
        if ordinal % 37 == 0 {
            texts.push("common engine".to_owned());
            continue;
        }
        let padding = vec!["filler"; 80 + ordinal % 40].join(" ");
        texts.push(format!("common engine rare{} {padding}", ordinal % 7));
    }
    let index = build_index(&analyzer, &texts, &[short, texts.len() - short]);

    let sealed = index.corpus_stats().expect("statistics");
    assert!(
        sealed.average_document_length() > 30.0,
        "the fixture must actually move avgdl, got {}",
        sealed.average_document_length()
    );
    // The top-ranked document must live in the later segment, or an
    // over-aggressive skip costs nothing and this test proves nothing.
    let probe = TermQuery::flat(vec![b"common".to_vec()], &[DEFAULT_FIELD]);
    let best = search(&index, &probe, 1, Bm25Params::beir()).expect("scores");
    assert_eq!(
        best.hits.first().map(|hit| hit.doc.segment),
        Some(1),
        "the fixture must put the winner in the later segment"
    );

    for params in [Bm25Params::beir(), Bm25Params::anserini()] {
        for terms in [
            vec![b"common".to_vec()],
            vec![b"common".to_vec(), b"engine".to_vec()],
            vec![b"engine".to_vec(), b"rare3".to_vec(), b"common".to_vec()],
        ] {
            let query = TermQuery::flat(terms, &[DEFAULT_FIELD]);
            for k in [1_usize, 10, 50] {
                let expected = search(&index, &query, k, params).expect("scores");
                for strategy in [Prune::BlockMaxWand, Prune::BlockMaxMaxscore] {
                    let actual =
                        search_pruned(&index, &query, k, params, strategy).expect("scores");
                    assert_eq!(
                        actual.hits, expected.hits,
                        "{strategy:?} diverged from the exhaustive scorer under                          avgdl drift at k={k} with k1={} b={}",
                        params.k1, params.b
                    );
                }
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        rng_seed: RngSeed::Fixed(0x7ec4_0c14_0001),
        cases: 512,
        ..ProptestConfig::default()
    })]

    /// Task 14 test 1. The permanent guard.
    #[test]
    fn prop_pruned_topk_equals_exhaustive(
        texts in prop::collection::vec(document_text(), 1..40),
        cuts in prop::collection::vec(0_usize..40, 0..3),
        words in query_words(),
        k in 1_usize..12,
    ) {
        let analyzer = analyzer();
        let boundaries = seal_boundaries(texts.len(), &cuts);
        prop_assume!(boundaries.len() <= 4);
        let index = build_index(&analyzer, &texts, &boundaries);
        let terms = analyze_query(&analyzer, &words);
        prop_assume!(!terms.is_empty());

        let params = Bm25Params::default();
        let query = TermQuery::flat(terms, &[DEFAULT_FIELD]);
        let Ok(expected) = search(&index, &query, k, params) else {
            return Ok(());
        };

        for strategy in [Prune::BlockMaxWand, Prune::BlockMaxMaxscore] {
            let actual = search_pruned(&index, &query, k, params, strategy)
                .expect("the exhaustive path produced results, so pruning must too");
            prop_assert_eq!(
                actual.hits.len(),
                expected.hits.len(),
                "{:?} returned {} hits, exhaustive returned {}",
                strategy,
                actual.hits.len(),
                expected.hits.len()
            );
            for (rank, (got, want)) in
                actual.hits.iter().zip(expected.hits.iter()).enumerate()
            {
                prop_assert_eq!(
                    got.doc,
                    want.doc,
                    "{:?} put a different document at rank {}",
                    strategy,
                    rank
                );
                prop_assert!(
                    (got.score - want.score).abs() < EXACT,
                    "{:?} scored rank {} as {} against {}",
                    strategy,
                    rank,
                    got.score,
                    want.score
                );
            }
        }
    }

    /// The selection rule must never change the answer either.
    #[test]
    fn the_selected_strategy_agrees_with_the_exhaustive_scorer(
        texts in prop::collection::vec(document_text(), 1..30),
        words in query_words(),
        k in 1_usize..15,
    ) {
        let analyzer = analyzer();
        let index = build_index(&analyzer, &texts, &[texts.len()]);
        let terms = analyze_query(&analyzer, &words);
        prop_assume!(!terms.is_empty());
        let params = Bm25Params::default();
        let query = TermQuery::flat(terms.clone(), &[DEFAULT_FIELD]);
        let Ok(expected) = search(&index, &query, k, params) else {
            return Ok(());
        };
        let strategy = select_strategy(terms.len(), k);
        let actual = search_pruned(&index, &query, k, params, strategy).expect("scores");
        prop_assert_eq!(actual.hits, expected.hits);
    }

    /// Pruning must not invent or lose documents at any k, including k
    /// larger than the corpus.
    #[test]
    fn pruning_agrees_when_k_exceeds_the_corpus(
        texts in prop::collection::vec(document_text(), 1..12),
        words in query_words(),
    ) {
        let analyzer = analyzer();
        let index = build_index(&analyzer, &texts, &[texts.len()]);
        let terms = analyze_query(&analyzer, &words);
        prop_assume!(!terms.is_empty());
        let params = Bm25Params::default();
        let query = TermQuery::flat(terms, &[DEFAULT_FIELD]);
        let Ok(expected) = search(&index, &query, 1_000, params) else {
            return Ok(());
        };
        for strategy in [Prune::BlockMaxWand, Prune::BlockMaxMaxscore] {
            let actual = search_pruned(&index, &query, 1_000, params, strategy)
                .expect("scores");
            prop_assert_eq!(actual.hits, expected.hits.clone(), "{:?}", strategy);
        }
    }

    /// A corpus large enough to span several blocks is where skipping
    /// actually engages; small corpora take the short-list fast path and
    /// prove nothing about the pruning logic.
    #[test]
    fn pruning_agrees_on_a_corpus_that_spans_many_blocks(
        seed in 0_u64..64,
        k in 1_usize..11,
    ) {
        let analyzer = Analyzer::new(Profile::Code.config()).expect("valid config");
        // 400 documents: over six blocks of 64 for the common term.
        let texts: Vec<String> = (0..400_u64)
            .map(|index| {
                let mixed = (index * 2_654_435_761_u64).wrapping_add(seed);
                let mut words = vec!["common"];
                if mixed % 3 == 0 {
                    words.push("tuning");
                }
                if mixed % 17 == 0 {
                    words.push("quokka");
                }
                if mixed % 5 == 0 {
                    words.push("common");
                }
                words.join(" ")
            })
            .collect();
        let index = build_index(&analyzer, &texts, &[texts.len()]);
        let params = Bm25Params::default();
        let query = TermQuery::flat(
            vec![b"common".to_vec(), b"tuning".to_vec(), b"quokka".to_vec()],
            &[DEFAULT_FIELD],
        );
        let expected = search(&index, &query, k, params).expect("scores");
        for strategy in [Prune::BlockMaxWand, Prune::BlockMaxMaxscore] {
            let actual = search_pruned(&index, &query, k, params, strategy)
                .expect("scores");
            prop_assert_eq!(actual.hits.len(), expected.hits.len(), "{:?}", strategy);
            for (rank, (got, want)) in
                actual.hits.iter().zip(expected.hits.iter()).enumerate()
            {
                prop_assert_eq!(got.doc, want.doc, "{:?} at rank {}", strategy, rank);
                prop_assert!((got.score - want.score).abs() < EXACT);
            }
        }
    }
}
