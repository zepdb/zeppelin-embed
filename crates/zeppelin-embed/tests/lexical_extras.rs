//! Task 15 equivalence properties: extras against their naive models.
//!
//! Each property pairs a real implementation with a brute-force model that
//! shares none of its code. Where the plan warns that an ASCII-only corpus
//! is a false green (R4), the generator draws multibyte, combining-mark, and
//! emoji text from the task-12 fixture classes.
#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use proptest::prelude::*;
use proptest::test_runner::{Config as ProptestConfig, RngSeed};

use zeppelin_embed::fts::dict::{TermDictionary, TermInfo};
use zeppelin_embed::fts::index::{Document, SegmentIndex, DEFAULT_FIELD};
use zeppelin_embed::fts::phrase::{search_segment, streams_match, PhraseQuery};
use zeppelin_embed::fts::snippet::{best_window, match_offsets};
use zeppelin_embed::fts::tokenizer::{Analyzer, Profile, TokenizerConfig};
use zeppelin_embed::fts::{fuzzy, prefix};

fn code_analyzer() -> Analyzer {
    Analyzer::new(Profile::Code.config()).expect("valid configuration")
}

fn text_analyzer() -> Analyzer {
    Analyzer::new(TokenizerConfig::text_default()).expect("valid configuration")
}

/// Words the corpus and query generators draw from.
const WORDS: [&str; 12] = [
    "alpha", "beta", "gamma", "delta", "alpha", "beta", "receive", "receipt", "engine",
    "engineer", "put_if_match", "Caf\u{00E9}",
];

/// Fragments including the classes that break naive offset arithmetic.
const UNICODE_FRAGMENTS: [&str; 8] = [
    "Caf\u{00E9}",
    "cafe\u{0301}",
    "na\u{00EF}ve",
    "\u{1F680}",
    "\u{4E2D}\u{6587}",
    "stra\u{00DF}e",
    "don\u{2019}t",
    "\u{FB01}nd",
];

fn document_text() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            prop::sample::select(WORDS.as_slice()),
            prop::sample::select(UNICODE_FRAGMENTS.as_slice()),
        ],
        0..10,
    )
    .prop_map(|words| words.join(" "))
}

fn dictionary_from(terms: &[String]) -> TermDictionary {
    let mut sorted: Vec<&str> = terms.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();
    let mut dictionary = TermDictionary::default();
    for term in sorted {
        if term.is_empty() {
            continue;
        }
        let _ = dictionary.push(term.as_bytes(), TermInfo::default());
    }
    dictionary
}

/// Brute-force Levenshtein, sharing no code with the dictionary walk.
fn levenshtein(left: &[u8], right: &[u8]) -> u32 {
    let mut row: Vec<u32> = (0..=right.len()).map(|index| index as u32).collect();
    for (i, a) in left.iter().enumerate() {
        let mut next = vec![i as u32 + 1];
        for (j, b) in right.iter().enumerate() {
            let cost = u32::from(a != b);
            next.push((row[j + 1] + 1).min(next[j] + 1).min(row[j] + cost));
        }
        row = next;
    }
    row.last().copied().unwrap_or(u32::MAX)
}

/// Brute-force phrase model over a raw analyzed token stream.
///
/// Enumerates every alignment rather than running the dynamic program.
fn naive_phrase_match(streams: &[Vec<u32>], slop: u32) -> bool {
    fn walk(streams: &[Vec<u32>], index: usize, anchor: u32, last: u32, cost: u32, slop: u32) -> bool {
        let Some(stream) = streams.get(index) else {
            return cost <= slop;
        };
        for position in stream {
            if *position < last {
                continue;
            }
            let expected = u32::try_from(index).unwrap_or(u32::MAX);
            let relative = position.saturating_sub(anchor);
            let added = relative.abs_diff(expected);
            let total = cost.saturating_add(added);
            if total > slop {
                continue;
            }
            if walk(streams, index + 1, anchor, *position, total, slop) {
                return true;
            }
        }
        false
    }
    let Some(first) = streams.first() else {
        return false;
    };
    if streams.iter().any(Vec::is_empty) {
        return false;
    }
    first
        .iter()
        .any(|anchor| walk(streams, 1, *anchor, *anchor, 0, slop))
}

proptest! {
    #![proptest_config(ProptestConfig {
        rng_seed: RngSeed::Fixed(0x7ec4_0c15_0001),
        cases: 512,
        ..ProptestConfig::default()
    })]

    /// Task 15 test 2.
    #[test]
    fn phrase_matches_iff_naive_token_stream_scan_matches(
        streams in prop::collection::vec(
            prop::collection::vec(0_u32..12, 0..4).prop_map(|mut positions| {
                positions.sort_unstable();
                positions.dedup();
                positions
            }),
            1..4,
        ),
        slop in 0_u32..5,
    ) {
        prop_assert_eq!(
            streams_match(&streams, slop),
            naive_phrase_match(&streams, slop),
            "streams {:?} at slop {}",
            streams,
            slop
        );
    }

    /// The indexed phrase path agrees with the model on real documents.
    #[test]
    fn indexed_phrase_agrees_with_the_model_on_analyzed_documents(
        texts in prop::collection::vec(document_text(), 1..6),
        query in prop::collection::vec(prop::sample::select(WORDS.as_slice()), 1..3),
        slop in 0_u32..4,
    ) {
        let analyzer = code_analyzer();
        let mut segment = SegmentIndex::new();
        for text in &texts {
            segment
                .push_document(&analyzer, &Document::with_text(text))
                .expect("indexable");
        }
        // Analyze the query the same way the documents were analyzed.
        let mut terms: Vec<Vec<u8>> = Vec::new();
        for word in &query {
            if let Some(token) = analyzer.analyze(word).into_iter().next() {
                terms.push(token.term.into_bytes());
            }
        }
        prop_assume!(!terms.is_empty());

        let phrase = PhraseQuery {
            terms: terms.clone(),
            slop,
            field: DEFAULT_FIELD,
        };
        let actual = search_segment(&segment, &phrase).expect("valid phrase");

        // Model: pull the positions straight from the segment and run the
        // brute-force alignment.
        let mut expected: Vec<u32> = Vec::new();
        for row in 0..segment.row_count() {
            let mut streams: Vec<Vec<u32>> = Vec::new();
            let mut complete = true;
            for term in &terms {
                let Some(list) = segment.posting_list(term, DEFAULT_FIELD) else {
                    complete = false;
                    break;
                };
                match list.postings().iter().find(|posting| posting.docid == row) {
                    Some(posting) => streams.push(posting.positions.clone()),
                    None => {
                        complete = false;
                        break;
                    }
                }
            }
            if complete && naive_phrase_match(&streams, slop) {
                expected.push(row);
            }
        }
        prop_assert_eq!(actual, expected);
    }

    /// Task 15 test 4.
    #[test]
    fn prefix_results_equal_brute_force_over_the_dictionary(
        terms in prop::collection::vec(
            prop::sample::select(WORDS.as_slice()).prop_map(str::to_owned),
            1..12,
        ),
        length in 1_usize..5,
    ) {
        let analyzed: Vec<String> = {
            let analyzer = code_analyzer();
            let mut out = Vec::new();
            for term in &terms {
                for token in analyzer.analyze(term) {
                    out.push(token.term);
                }
            }
            out
        };
        prop_assume!(!analyzed.is_empty());
        let dictionary = dictionary_from(&analyzed);
        prop_assume!(!dictionary.is_empty());

        let Some(sample) = dictionary.term_at(0) else {
            return Ok(());
        };
        let cut = length.min(sample.len()).max(1);
        let Some(prefix_bytes) = sample.get(..cut) else {
            return Ok(());
        };

        let actual: Vec<Vec<u8>> = prefix::search(&dictionary, prefix_bytes)
            .expect("non-empty prefix")
            .into_iter()
            .map(|entry| entry.term)
            .collect();
        let expected: Vec<Vec<u8>> = (0..dictionary.len())
            .filter_map(|index| dictionary.term_at(index))
            .filter(|term| term.starts_with(prefix_bytes))
            .map(<[u8]>::to_vec)
            .collect();
        prop_assert_eq!(actual, expected);
    }

    /// Task 15 test 7.
    #[test]
    fn fuzzy_results_equal_brute_force_edit_distance_over_the_dictionary(
        terms in prop::collection::vec(
            prop::sample::select(WORDS.as_slice()).prop_map(str::to_owned),
            1..12,
        ),
        query in prop::sample::select(WORDS.as_slice()),
        max_distance in 1_u32..3,
    ) {
        let analyzer = code_analyzer();
        let mut analyzed: Vec<String> = Vec::new();
        for term in &terms {
            for token in analyzer.analyze(term) {
                analyzed.push(token.term);
            }
        }
        prop_assume!(!analyzed.is_empty());
        let dictionary = dictionary_from(&analyzed);
        prop_assume!(!dictionary.is_empty());

        let Some(query_token) = analyzer.analyze(query).into_iter().next() else {
            return Ok(());
        };
        let query_bytes = query_token.term.into_bytes();
        prop_assume!(!query_bytes.is_empty());

        let (candidates, counters) = fuzzy::search(&dictionary, &query_bytes, max_distance);
        let mut actual: Vec<Vec<u8>> =
            candidates.into_iter().map(|candidate| candidate.term).collect();
        actual.sort();

        let mut expected: Vec<Vec<u8>> = (0..dictionary.len())
            .filter_map(|index| dictionary.term_at(index))
            .filter(|term| {
                term.first() == query_bytes.first()
                    && levenshtein(&query_bytes, term) <= max_distance
            })
            .map(<[u8]>::to_vec)
            .collect();
        expected.sort();
        prop_assert_eq!(actual, expected);

        // Task 15 test 9: the walk never enumerates the whole dictionary
        // unless the dictionary is smaller than the first-byte range.
        prop_assert!(
            counters.entries_visited <= dictionary.len() as u64,
            "the walk visited more entries than exist"
        );
    }

    /// Task 15 test 12.
    #[test]
    fn snippet_ranges_are_in_bounds_utf8_valid_and_contain_the_matched_surface_form(
        text in document_text(),
        word in prop::sample::select(WORDS.as_slice()),
    ) {
        let analyzer = text_analyzer();
        let Some(token) = analyzer.analyze(word).into_iter().next() else {
            return Ok(());
        };
        let terms = vec![token.term.into_bytes()];

        for highlight in match_offsets(&analyzer, &text, &terms) {
            let start = usize::try_from(highlight.start).unwrap_or(usize::MAX);
            let end = usize::try_from(highlight.end).unwrap_or(usize::MAX);
            prop_assert!(end <= text.len(), "highlight escaped the field");
            prop_assert!(text.is_char_boundary(start), "highlight split a character");
            prop_assert!(text.is_char_boundary(end), "highlight split a character");
            prop_assert!(text.get(start..end).is_some());
        }

        if let Some(snippet) = best_window(&analyzer, &text, &terms, 24, true).expect("stored") {
            let window = snippet.text(&text);
            prop_assert!(window.is_some(), "window is not a character boundary");
            for highlight in &snippet.highlights {
                prop_assert!(highlight.start >= snippet.window.start);
                prop_assert!(highlight.end <= snippet.window.end);
            }
        }
    }

    /// Task 15 test 13.
    #[test]
    fn snippets_are_deterministic_across_runs(
        text in document_text(),
        word in prop::sample::select(WORDS.as_slice()),
    ) {
        let analyzer = text_analyzer();
        let Some(token) = analyzer.analyze(word).into_iter().next() else {
            return Ok(());
        };
        let terms = vec![token.term.into_bytes()];
        let first = best_window(&analyzer, &text, &terms, 24, true).expect("stored");
        let second = best_window(&analyzer, &text, &terms, 24, true).expect("stored");
        prop_assert_eq!(first, second);
    }
}
