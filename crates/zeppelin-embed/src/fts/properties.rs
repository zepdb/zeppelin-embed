//! Task 13's model-oracle property: the engine equals a brute-force BM25.
//!
//! # Why this test is the point of task 13
//!
//! The engine computes `N`, `avgdl`, and `df` through segment structures.
//! The model below computes them from a flat list of analyzed documents,
//! knowing nothing about segments at all. The generator then randomizes
//! **where the seal boundaries fall** over the same documents.
//!
//! That combination is what makes segment-local statistics unconstructible:
//! any statistic derived per segment changes when the boundaries move, the
//! model's does not, and the two diverge on the first multi-segment case. A
//! reviewer does not have to audit the index for segment-local IDF; this
//! property refuses to pass if one exists.
//!
//! It is also the guard task 14 inherits. Pruning must reproduce the
//! exhaustive scorer exactly, and the exhaustive scorer is only worth
//! reproducing because of this test.

#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use proptest::test_runner::{Config as ProptestConfig, RngSeed};

use super::bm25::{Bm25Params, CorpusStats, Df, DocLen, Tf, term_score};
use super::index::{DEFAULT_FIELD, Document, LexicalIndex, SegmentIndex};
use super::search::{GlobalDocId, ScoredDoc, TermQuery, search};
use super::tokenizer::{Analyzer, TokenizerConfig};

/// Scores agreeing to this tolerance are equal; the spec asks for 1e-5.
const SCORE_TOLERANCE: f64 = 1e-9;

fn analyzer() -> Analyzer {
    Analyzer::new(TokenizerConfig::text_default()).expect("valid configuration")
}

/// A brute-force BM25 that knows nothing about segments.
struct Model {
    /// Per document: term -> term frequency.
    frequencies: Vec<BTreeMap<Vec<u8>, u32>>,
    /// Per document: analyzed length in positions.
    lengths: Vec<u32>,
}

impl Model {
    fn build(analyzer: &Analyzer, texts: &[String]) -> Self {
        let mut frequencies = Vec::with_capacity(texts.len());
        let mut lengths = Vec::with_capacity(texts.len());
        for text in texts {
            let tokens = analyzer.analyze(text);
            let length = tokens
                .iter()
                .map(|token| token.position)
                .max()
                .map_or(0, |highest| highest.saturating_add(1));
            let mut per_term: BTreeMap<Vec<u8>, BTreeSet<u32>> = BTreeMap::new();
            for token in tokens {
                per_term
                    .entry(token.term.into_bytes())
                    .or_default()
                    .insert(token.position);
            }
            frequencies.insert(
                frequencies.len(),
                per_term
                    .into_iter()
                    .map(|(term, positions)| {
                        (term, u32::try_from(positions.len()).unwrap_or(u32::MAX))
                    })
                    .collect(),
            );
            lengths.push(length);
        }
        Self {
            frequencies,
            lengths,
        }
    }

    fn document_count(&self) -> u64 {
        u64::try_from(self.frequencies.len()).unwrap_or(0)
    }

    fn total_tokens(&self) -> u64 {
        self.lengths.iter().map(|length| u64::from(*length)).sum()
    }

    fn document_frequency(&self, term: &[u8]) -> u32 {
        u32::try_from(
            self.frequencies
                .iter()
                .filter(|document| document.contains_key(term))
                .count(),
        )
        .unwrap_or(u32::MAX)
    }

    /// Scores every document, returning the same shape the engine returns.
    fn search(
        &self,
        terms: &[Vec<u8>],
        boundaries: &[usize],
        k: usize,
        params: Bm25Params,
    ) -> Option<Vec<ScoredDoc>> {
        let stats = CorpusStats::new(self.document_count(), self.total_tokens()).ok()?;
        let mut scored: Vec<(GlobalDocId, f64)> = Vec::new();

        for (flat, document) in self.frequencies.iter().enumerate() {
            let mut total = 0.0_f64;
            let mut matched = false;
            for term in terms {
                let Some(tf) = document.get(term).copied() else {
                    continue;
                };
                matched = true;
                let df = self.document_frequency(term);
                let length = self.lengths.get(flat).copied().unwrap_or(0);
                total += term_score(Tf(tf), Df(df), DocLen(length), &stats, params);
            }
            if !matched {
                continue;
            }
            scored.push((locate(flat, boundaries), total));
        }

        scored.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(left.0.cmp(&right.0))
        });
        scored.truncate(k);
        Some(
            scored
                .into_iter()
                .map(|(doc, score)| ScoredDoc { doc, score })
                .collect(),
        )
    }
}

/// Maps a flat document ordinal onto its (segment, row) identity.
fn locate(flat: usize, boundaries: &[usize]) -> GlobalDocId {
    let mut consumed = 0_usize;
    for (segment, size) in boundaries.iter().enumerate() {
        if flat < consumed + size {
            return GlobalDocId {
                segment: u32::try_from(segment).unwrap_or(u32::MAX),
                row: u32::try_from(flat - consumed).unwrap_or(u32::MAX),
            };
        }
        consumed += size;
    }
    GlobalDocId {
        segment: u32::try_from(boundaries.len().saturating_sub(1)).unwrap_or(0),
        row: u32::try_from(flat.saturating_sub(consumed)).unwrap_or(0),
    }
}

/// Builds the engine index with the given seal boundaries.
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

/// Splits `total` documents into 1..=4 segments.
fn seal_boundaries(total: usize, cuts: Vec<usize>) -> Vec<usize> {
    if total == 0 {
        return vec![0];
    }
    let mut points: Vec<usize> = cuts.into_iter().map(|cut| cut % total.max(1)).collect();
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

/// Vocabulary the generator draws documents and queries from.
const VOCABULARY: [&str; 16] = [
    "engine",
    "engines",
    "tuning",
    "lexical",
    "search",
    "index",
    "postings",
    "score",
    "the",
    "of",
    "put_if_match",
    "i-485",
    "Caf\u{00E9}",
    "twenty five",
    "C++",
    "\u{4E2D}\u{6587}",
];

fn document_text() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(VOCABULARY.as_slice()), 0..14)
        .prop_map(|words| words.join(" "))
}

fn query_terms() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(prop::sample::select(VOCABULARY.as_slice()), 1..4)
        .prop_map(|words| words.into_iter().map(str::to_owned).collect())
}

/// Analyzes a query string into the terms the engine will look up.
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
        rng_seed: RngSeed::Fixed(0x7ec4_0c13_0001),
        cases: 384,
        ..ProptestConfig::default()
    })]

    /// Task 13 test 6, the one that makes segment-local IDF unconstructible.
    #[test]
    fn prop_engine_bm25_equals_model(
        texts in prop::collection::vec(document_text(), 1..12),
        cuts in prop::collection::vec(0_usize..12, 0..3),
        words in query_terms(),
        k in 1_usize..8,
    ) {
        let analyzer = analyzer();
        let boundaries = seal_boundaries(texts.len(), cuts);
        prop_assume!(boundaries.len() <= 4);

        let index = build_index(&analyzer, &texts, &boundaries);
        let terms = analyze_query(&analyzer, &words);
        prop_assume!(!terms.is_empty());

        let params = Bm25Params::default();
        let query = TermQuery::flat(terms.clone(), &[DEFAULT_FIELD]);

        let model = Model::build(&analyzer, &texts);
        let Some(expected) = model.search(&terms, &boundaries, k, params) else {
            // An all-empty corpus has no avgdl; the engine must agree.
            prop_assert!(search(&index, &query, k, params).is_err());
            return Ok(());
        };
        let actual = search(&index, &query, k, params)
            .expect("the model produced statistics, so the engine must too");

        prop_assert_eq!(
            actual.hits.len(),
            expected.len(),
            "hit count differs; boundaries {:?}",
            boundaries
        );
        for (position, (got, want)) in actual.hits.iter().zip(expected.iter()).enumerate() {
            prop_assert_eq!(
                got.doc,
                want.doc,
                "document at rank {} differs; boundaries {:?}",
                position,
                boundaries
            );
            prop_assert!(
                (got.score - want.score).abs() < SCORE_TOLERANCE,
                "score at rank {} differs: engine {} model {} (boundaries {:?})",
                position,
                got.score,
                want.score,
                boundaries
            );
        }
    }

    /// Moving the seal boundaries must not change any score.
    ///
    /// This is the same guarantee stated directly rather than through the
    /// model: it is the property a user actually depends on.
    #[test]
    fn scores_do_not_depend_on_where_segments_were_sealed(
        texts in prop::collection::vec(document_text(), 2..10),
        cuts in prop::collection::vec(0_usize..10, 1..3),
        words in query_terms(),
    ) {
        let analyzer = analyzer();
        let terms = analyze_query(&analyzer, &words);
        prop_assume!(!terms.is_empty());
        let params = Bm25Params::default();
        let query = TermQuery::flat(terms, &[DEFAULT_FIELD]);

        let single = build_index(&analyzer, &texts, &[texts.len()]);
        let split = build_index(&analyzer, &texts, &seal_boundaries(texts.len(), cuts));

        let (Ok(one), Ok(many)) = (
            search(&single, &query, 16, params),
            search(&split, &query, 16, params),
        ) else {
            return Ok(());
        };

        prop_assert_eq!(one.hits.len(), many.hits.len(), "hit counts diverged");
        let mut one_scores: Vec<f64> = one.hits.iter().map(|hit| hit.score).collect();
        let mut many_scores: Vec<f64> = many.hits.iter().map(|hit| hit.score).collect();
        one_scores.sort_by(f64::total_cmp);
        many_scores.sort_by(f64::total_cmp);
        for (left, right) in one_scores.iter().zip(many_scores.iter()) {
            prop_assert!(
                (left - right).abs() < SCORE_TOLERANCE,
                "sealing changed a score: {} vs {}",
                left,
                right
            );
        }
    }

    /// Task 13 test 3: posting blocks round-trip arbitrary content.
    #[test]
    fn postings_blocks_round_trip_arbitrary_docids_tfs_and_positions(
        gaps in prop::collection::vec(1_u32..64, 1..200),
        block_size in prop::sample::select(vec![1_u16, 2, 7, 32, 64, 128]),
    ) {
        use super::postings::{encode, Posting, PostingList, PostingsReader};

        let mut list = PostingList::new();
        let mut docid = 0_u32;
        for (index, gap) in gaps.iter().enumerate() {
            docid = docid.saturating_add(*gap);
            let tf = (index % 5 + 1) as u32;
            let mut positions = Vec::with_capacity(tf as usize);
            let mut position = (index % 3) as u32;
            for _ in 0..tf {
                positions.push(position);
                position = position.saturating_add(1 + (index % 7) as u32);
            }
            list.push(Posting {
                docid,
                tf,
                positions,
            })
            .expect("strictly ascending by construction");
        }

        let encoded = encode(&list, block_size, &[]).expect("encodes");
        let reader = PostingsReader::open(encoded.as_bytes()).expect("opens");
        prop_assert_eq!(reader.postings_per_block(), block_size);
        prop_assert_eq!(reader.document_frequency(), list.document_frequency());
        prop_assert_eq!(reader.decode_all().expect("decodes"), list);
    }

    /// Task 13 test 4: corrupt bytes are typed errors, never panics.
    #[test]
    fn postings_decoder_returns_a_typed_error_on_corrupt_bytes_and_never_panics(
        bytes in prop::collection::vec(any::<u8>(), 0..512),
    ) {
        use super::postings::PostingsReader;

        if let Ok(reader) = PostingsReader::open(&bytes) {
            for index in 0..reader.blocks().len() {
                let _ = reader.decode_block(index);
            }
            let _ = reader.decode_all();
        }
    }

    /// Flipping one byte of a valid stream never panics the decoder.
    #[test]
    fn a_single_flipped_byte_never_panics_the_decoder(
        position in 0_usize..400,
        mask in 1_u8..255,
    ) {
        use super::postings::{encode, Posting, PostingList, PostingsReader};

        let mut list = PostingList::new();
        for index in 0..40_u32 {
            list.push(Posting {
                docid: index * 3,
                tf: 2,
                positions: vec![index, index + 5],
            })
            .expect("ascending");
        }
        let encoded = encode(&list, 8, &[]).expect("encodes");
        let mut bytes = encoded.as_bytes().to_vec();
        let target = position % bytes.len().max(1);
        if let Some(slot) = bytes.get_mut(target) {
            *slot ^= mask;
        }
        if let Ok(reader) = PostingsReader::open(&bytes) {
            for index in 0..reader.blocks().len() {
                let _ = reader.decode_block(index);
            }
        }
    }

    /// Task 13 test 7: avgdl counts analyzed tokens over the scored unit.
    #[test]
    fn avgdl_counts_analyzed_tokens_over_the_scored_unit(
        texts in prop::collection::vec(document_text(), 1..8),
    ) {
        let analyzer = analyzer();
        let index = build_index(&analyzer, &texts, &[texts.len()]);
        let model = Model::build(&analyzer, &texts);
        prop_assert_eq!(index.document_count(), model.document_count());
        prop_assert_eq!(index.total_tokens(), model.total_tokens());
    }

    /// Store-wide document frequency equals the model's, whatever the seal.
    #[test]
    fn document_frequency_is_global_under_every_seal_boundary(
        texts in prop::collection::vec(document_text(), 1..10),
        cuts in prop::collection::vec(0_usize..10, 0..3),
        word in prop::sample::select(VOCABULARY.as_slice()),
    ) {
        let analyzer = analyzer();
        let boundaries = seal_boundaries(texts.len(), cuts);
        let index = build_index(&analyzer, &texts, &boundaries);
        let model = Model::build(&analyzer, &texts);
        for term in analyze_query(&analyzer, &[word.to_owned()]) {
            prop_assert_eq!(
                index.document_frequency(&term, &[DEFAULT_FIELD]),
                model.document_frequency(&term),
                "df diverged for {:?} under boundaries {:?}",
                String::from_utf8_lossy(&term),
                boundaries
            );
        }
    }
}
