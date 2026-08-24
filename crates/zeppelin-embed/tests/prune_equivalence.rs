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
use zeppelin_embed::fts::index::{Document, LexicalIndex, SegmentIndex, DEFAULT_FIELD};
use zeppelin_embed::fts::prune::{search_pruned, select_strategy, Strategy as Prune};
use zeppelin_embed::fts::search::{search, TermQuery};
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
