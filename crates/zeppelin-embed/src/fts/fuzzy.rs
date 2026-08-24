//! Bounded fuzzy matching by dictionary-guided Levenshtein pruning.
//!
//! # Why not enumerate the dictionary
//!
//! The naive answer computes an edit distance against every term. That is
//! linear in vocabulary size per query term and is exactly what the counter
//! contract in `tests/lexical_extras.rs` forbids.
//!
//! Instead this walks the SORTED dictionary carrying an incremental
//! Levenshtein row. Because terms are sorted, consecutive terms share a
//! prefix, and the row for that shared prefix does not need recomputing.
//! Better: if the minimum value in the row for a prefix already exceeds the
//! distance budget, then no term starting with that prefix can match, and
//! the walk skips the whole run in one seek rather than testing each term.
//! That is the same pruning an FST intersection performs, without an FST.
//!
//! # Policy (task 15 D3)
//!
//! From the spec, which took it from Meilisearch's tested policy:
//!
//! - distance 1 for terms of length >= 5, distance 2 for length >= 9;
//! - never on `no_fuzzy` tokens — identifiers, numbers, SKUs. A fuzzy match
//!   on `i-485` or a ticket id is almost always wrong;
//! - the first character must match, which is what makes the dictionary
//!   walk cheap and also what users expect;
//! - candidates enter scoring as low-weight variants and are always
//!   reported as fuzzy in diagnostics. Fuzzy never silently replaces exact
//!   semantics.
//!
//! # Known limitation: transpositions cost two
//!
//! The metric is plain Levenshtein, so swapping two adjacent letters —
//! `recieve` for `receive`, `teh` for `the` — costs two edits, not one.
//! Damerau-Levenshtein charges one. Since the policy grants distance 1 to
//! terms of length 5..8, the single most common English typo class is out
//! of reach for short words under the shipped policy.
//!
//! This is recorded rather than fixed because moving to Damerau changes
//! which candidates every fuzzy query returns, and that is a retrieval
//! semantics change that belongs to a measured decision, not to a
//! convenience edit. The distance table was NOT widened to hide it.

use super::dict::TermDictionary;

/// Largest edit distance this engine will ever consider.
pub const MAX_EDIT_DISTANCE: u32 = 2;

/// Minimum term length for distance 1.
pub const MIN_LENGTH_DISTANCE_1: usize = 5;

/// Minimum term length for distance 2.
pub const MIN_LENGTH_DISTANCE_2: usize = 9;

/// One fuzzy candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FuzzyCandidate {
    /// Index of the term in the dictionary's flat order.
    pub term_index: usize,
    /// The matched term.
    pub term: Vec<u8>,
    /// Edit distance from the query term. Zero means an exact match.
    pub distance: u32,
}

/// What a fuzzy walk cost, for the counter contract.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FuzzyCounters {
    /// Dictionary entries whose distance was actually computed.
    pub entries_visited: u64,
    /// Runs skipped wholesale because their prefix already exceeded budget.
    pub prefix_skips: u64,
}

/// Returns the edit distance budget the policy allows for a term.
///
/// Returns zero when fuzzy matching is not permitted at all, which is the
/// safe direction: zero distance is an exact match.
#[must_use]
pub fn permitted_distance(term: &[u8], no_fuzzy: bool) -> u32 {
    if no_fuzzy {
        return 0;
    }
    // A term carrying a digit is an identifier or a quantity; both are
    // wrong to fuzz even when the token flag was not set by the analyzer.
    if term.iter().any(u8::is_ascii_digit) {
        return 0;
    }
    let length = term.iter().filter(|byte| **byte & 0xC0 != 0x80).count();
    if length >= MIN_LENGTH_DISTANCE_2 {
        2
    } else if length >= MIN_LENGTH_DISTANCE_1 {
        1
    } else {
        0
    }
}

/// Computes the DP row after consuming one more byte of the candidate.
///
/// `previous` is the row for the candidate prefix one byte shorter, and
/// `row_index` is that prefix's length. Returning a row rather than a single
/// distance is what makes prefix pruning possible: if every cell already
/// exceeds the budget, no extension of this prefix can match.
fn step_row(query: &[u8], previous: &[u32], row_index: usize, byte: u8) -> Vec<u32> {
    let width = query.len() + 1;
    let mut next: Vec<u32> = Vec::with_capacity(width);
    next.push(u32::try_from(row_index + 1).unwrap_or(u32::MAX));
    for column in 1..width {
        let substitution_cost = u32::from(query.get(column - 1).copied() != Some(byte));
        let deletion = previous
            .get(column)
            .copied()
            .unwrap_or(u32::MAX)
            .saturating_add(1);
        let insertion = next
            .get(column - 1)
            .copied()
            .unwrap_or(u32::MAX)
            .saturating_add(1);
        let substitution = previous
            .get(column - 1)
            .copied()
            .unwrap_or(u32::MAX)
            .saturating_add(substitution_cost);
        next.push(deletion.min(insertion).min(substitution));
    }
    next
}

fn shared_prefix(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .zip(right.iter())
        .take_while(|(a, b)| a == b)
        .count()
}

/// Finds every dictionary term within `max_distance` of `query`.
///
/// Walks the sorted dictionary with prefix pruning; never enumerates the
/// whole vocabulary unless every prefix stays within budget, which only
/// happens on a pathologically small dictionary.
#[must_use]
pub fn search(
    dictionary: &TermDictionary,
    query: &[u8],
    max_distance: u32,
) -> (Vec<FuzzyCandidate>, FuzzyCounters) {
    let mut counters = FuzzyCounters::default();
    let mut candidates = Vec::new();
    if query.is_empty() || max_distance == 0 {
        // Distance zero is an exact lookup, which the dictionary answers in
        // one binary search rather than a walk.
        if let Some(index) = dictionary.seek_exact(query) {
            counters.entries_visited = 1;
            candidates.push(FuzzyCandidate {
                term_index: index,
                term: query.to_vec(),
                distance: 0,
            });
        }
        return (candidates, counters);
    }

    // The first character must match, so the walk is confined to that
    // prefix range rather than the whole dictionary.
    let Some(first) = query.first().copied() else {
        return (candidates, counters);
    };
    let range = match dictionary.prefix_range(&[first]) {
        Ok(range) => range,
        Err(_) => return (candidates, counters),
    };

    // `rows[i]` is the DP row after consuming the first `i` bytes of the
    // term currently being examined. Moving to the next sorted term keeps
    // the rows its shared prefix already computed and rebuilds only the
    // suffix; that is the whole saving, and reusing a row beyond the shared
    // prefix is exactly the bug this replaced.
    let identity: Vec<u32> = (0..=query.len())
        .map(|index| u32::try_from(index).unwrap_or(u32::MAX))
        .collect();
    let mut rows: Vec<Vec<u32>> = vec![identity];
    let mut previous_term: Vec<u8> = Vec::new();
    let mut index = range.start;

    while index < range.end {
        let Some(term) = dictionary.term_at(index) else {
            break;
        };
        let prefix = shared_prefix(&previous_term, term).min(rows.len().saturating_sub(1));
        rows.truncate(prefix + 1);
        for (offset, byte) in term.iter().enumerate().skip(prefix) {
            let Some(previous) = rows.get(offset) else {
                break;
            };
            let next = step_row(query, previous, offset, *byte);
            rows.push(next);
        }
        counters.entries_visited = counters.entries_visited.saturating_add(1);

        let Some(final_row) = rows.get(term.len()) else {
            index += 1;
            continue;
        };
        let distance = final_row.last().copied().unwrap_or(u32::MAX);
        if distance <= max_distance {
            candidates.push(FuzzyCandidate {
                term_index: index,
                term: term.to_vec(),
                distance,
            });
        }

        // If every cell of this term's row exceeds the budget, no term
        // extending it can match, so skip the whole run in one seek.
        let minimum = final_row.iter().copied().min().unwrap_or(u32::MAX);
        previous_term = term.to_vec();
        if minimum > max_distance {
            let skip_to = next_term_outside_prefix(dictionary, term, range.end);
            if skip_to > index + 1 {
                counters.prefix_skips = counters.prefix_skips.saturating_add(1);
            }
            index = skip_to.max(index + 1);
            continue;
        }
        index += 1;
    }

    (candidates, counters)
}

/// Returns the first dictionary index whose term leaves this term's prefix.
fn next_term_outside_prefix(dictionary: &TermDictionary, term: &[u8], end: usize) -> usize {
    // Seek past every term sharing the whole of `term` as a prefix.
    let mut probe = term.to_vec();
    // Increment the last byte to form the smallest strictly greater prefix.
    while let Some(last) = probe.pop() {
        if last < u8::MAX {
            probe.push(last.saturating_add(1));
            break;
        }
    }
    if probe.is_empty() {
        return end;
    }
    dictionary.seek_ceiling(&probe).unwrap_or(end).min(end)
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
    use crate::fts::dict::TermInfo;

    fn dictionary(terms: &[&str]) -> TermDictionary {
        let mut sorted: Vec<&str> = terms.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        let mut dictionary = TermDictionary::default();
        for term in sorted {
            dictionary
                .push(term.as_bytes(), TermInfo::default())
                .expect("sorted");
        }
        dictionary
    }

    /// The naive model: edit distance against every term.
    fn brute_force(terms: &[&str], query: &str, max_distance: u32) -> Vec<String> {
        let mut sorted: Vec<&str> = terms.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        sorted
            .into_iter()
            .filter(|term| {
                // The policy's first-character rule applies to the model too.
                term.as_bytes().first() == query.as_bytes().first()
                    && levenshtein(query.as_bytes(), term.as_bytes()) <= max_distance
            })
            .map(str::to_owned)
            .collect()
    }

    fn levenshtein(left: &[u8], right: &[u8]) -> u32 {
        let mut row: Vec<u32> = (0..=right.len()).map(|index| index as u32).collect();
        for (i, a) in left.iter().enumerate() {
            let mut next = vec![i as u32 + 1];
            for (j, b) in right.iter().enumerate() {
                let cost = u32::from(a != b);
                next.push(
                    (row[j + 1] + 1)
                        .min(next[j] + 1)
                        .min(row[j] + cost),
                );
            }
            row = next;
        }
        row.last().copied().unwrap_or(u32::MAX)
    }

    const VOCABULARY: [&str; 16] = [
        "receive", "recieve", "receipt", "recipe", "receiver", "rest", "resting", "restore",
        "retrieve", "retriever", "return", "returns", "read", "ready", "real", "realise",
    ];

    #[test]
    fn fuzzy_results_equal_brute_force_edit_distance_over_the_dictionary() {
        let dictionary = dictionary(&VOCABULARY);
        for query in [
            "receive", "recieve", "recipe", "restore", "retreive", "returm", "reeal", "read",
        ] {
            for max_distance in 1..=MAX_EDIT_DISTANCE {
                let (candidates, _) = search(&dictionary, query.as_bytes(), max_distance);
                let mut actual: Vec<String> = candidates
                    .into_iter()
                    .filter_map(|candidate| String::from_utf8(candidate.term).ok())
                    .collect();
                actual.sort();
                let mut expected = brute_force(&VOCABULARY, query, max_distance);
                expected.sort();
                assert_eq!(
                    actual, expected,
                    "query {query:?} at distance {max_distance}"
                );
            }
        }
    }

    #[test]
    fn the_classic_typo_finds_its_correction_at_the_distance_it_actually_costs() {
        // `recieve` -> `receive` is a TRANSPOSITION. Plain Levenshtein
        // charges two substitutions for it; only Damerau-Levenshtein counts
        // it as one. The policy grants distance 1 to a 7-letter term, so
        // this very common typo is NOT reachable under the shipped policy —
        // a real limitation, recorded rather than papered over by widening
        // the distance table. See the module docs.
        let dictionary = dictionary(&VOCABULARY);
        let (at_one, _) = search(&dictionary, b"recieve", 1);
        assert!(
            !at_one.iter().any(|candidate| candidate.term == b"receive"),
            "a transposition is distance 2, not 1"
        );
        let (at_two, _) = search(&dictionary, b"recieve", 2);
        assert!(
            at_two.iter().any(|candidate| candidate.term == b"receive"),
            "recieve must find receive at distance 2"
        );
    }

    #[test]
    fn fuzzy_policy_matrix_is_enforced() {
        // Length thresholds.
        assert_eq!(permitted_distance(b"cat", false), 0);
        assert_eq!(permitted_distance(b"four", false), 0);
        assert_eq!(permitted_distance(b"fives", false), 1);
        assert_eq!(permitted_distance(b"eighteens", false), 2);
        // The no_fuzzy flag always wins.
        assert_eq!(permitted_distance(b"eighteens", true), 0);
        // Anything carrying a digit is never fuzzed.
        assert_eq!(permitted_distance(b"i-485", false), 0);
        assert_eq!(permitted_distance(b"gpt-5.6", false), 0);
        assert_eq!(permitted_distance(b"abc1234567", false), 0);
        // The distance never exceeds the documented maximum.
        for term in [b"a".as_slice(), b"averyverylongtermindeed".as_slice()] {
            assert!(permitted_distance(term, false) <= MAX_EDIT_DISTANCE);
        }
    }

    #[test]
    fn the_first_character_rule_confines_the_walk() {
        let dictionary = dictionary(&["alpha", "alpha1", "blpha", "clpha"]);
        let (candidates, _) = search(&dictionary, b"alpha", 2);
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.term.first() == Some(&b'a')),
            "a candidate escaped the first-character rule"
        );
    }

    #[test]
    fn fuzzy_never_visits_more_dictionary_entries_than_the_contract() {
        // A dictionary where only a small share shares the query's first
        // byte; the walk must not touch the rest.
        let mut terms: Vec<String> = Vec::new();
        for letter in b'a'..=b'z' {
            for index in 0..40 {
                terms.push(format!("{}term{index:03}", char::from(letter)));
            }
        }
        let refs: Vec<&str> = terms.iter().map(String::as_str).collect();
        let dictionary = dictionary(&refs);
        assert_eq!(dictionary.len(), 26 * 40);

        let (_, counters) = search(&dictionary, b"aterm001", 1);
        assert!(
            counters.entries_visited <= 40,
            "visited {} entries; the first-character rule bounds this at 40",
            counters.entries_visited
        );
        assert!(
            counters.entries_visited < dictionary.len() as u64,
            "the walk enumerated the whole dictionary"
        );
    }

    #[test]
    fn distance_zero_is_an_exact_lookup() {
        let dictionary = dictionary(&VOCABULARY);
        let (candidates, counters) = search(&dictionary, b"receive", 0);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].distance, 0);
        assert_eq!(counters.entries_visited, 1);

        let (missing, _) = search(&dictionary, b"absent", 0);
        assert!(missing.is_empty());
    }

    #[test]
    fn an_empty_query_matches_nothing() {
        let dictionary = dictionary(&VOCABULARY);
        let (candidates, _) = search(&dictionary, b"", 2);
        assert!(candidates.is_empty());
    }

    #[test]
    fn an_empty_dictionary_yields_no_candidates() {
        let dictionary = TermDictionary::default();
        let (candidates, counters) = search(&dictionary, b"receive", 2);
        assert!(candidates.is_empty());
        assert_eq!(counters.entries_visited, 0);
    }

    #[test]
    fn every_reported_candidate_carries_its_true_distance() {
        let dictionary = dictionary(&VOCABULARY);
        let (candidates, _) = search(&dictionary, b"recieve", 2);
        for candidate in &candidates {
            assert_eq!(
                candidate.distance,
                levenshtein(b"recieve", &candidate.term),
                "reported distance for {:?} is wrong",
                String::from_utf8_lossy(&candidate.term)
            );
            assert_eq!(
                dictionary.term_at(candidate.term_index),
                Some(candidate.term.as_slice())
            );
        }
    }
}
