//! Prefix queries as a sorted-dictionary range scan.
//!
//! # The boundary rule, pinned (task 15 D2)
//!
//! Prefix matching is **by codepoint after analysis**, not by grapheme
//! cluster. A prefix that ends in the middle of a grapheme — between a base
//! letter and its combining mark, or inside a family emoji's zero-width
//! joiner sequence — still matches, because the dictionary is a sorted byte
//! array and the prefix is compared as bytes of the analyzed form.
//!
//! Two consequences worth stating rather than discovering:
//!
//! - The prefix is analyzed with the same pipeline as the documents, so
//!   `Caf` and `caf` are the same prefix, and `café` folds to `cafe` before
//!   the comparison. A user typing accented text gets the results they
//!   expect.
//! - Because folding can expand one character into several (`ß` becomes
//!   `ss`), a prefix can match a term whose *surface* form it is not a
//!   prefix of. That is correct: the index stores analyzed terms.
//!
//! # An empty prefix is an error, not a full scan
//!
//! Returning every term for an empty prefix is a denial-of-service wearing a
//! query's clothes. It is a typed error.

use super::dict::{DictError, TermDictionary};

/// One prefix match.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrefixMatch {
    /// Index of the term in the dictionary's flat order.
    pub term_index: usize,
    /// The matched term.
    pub term: Vec<u8>,
}

/// Returns every dictionary term starting with `prefix`.
///
/// # Errors
///
/// Returns [`DictError::EmptyTerm`] for an empty prefix.
pub fn search(
    dictionary: &TermDictionary,
    prefix: &[u8],
) -> Result<Vec<PrefixMatch>, DictError> {
    let range = dictionary.prefix_range(prefix)?;
    Ok(range
        .filter_map(|term_index| {
            dictionary.term_at(term_index).map(|term| PrefixMatch {
                term_index,
                term: term.to_vec(),
            })
        })
        .collect())
}

/// Returns the number of terms a prefix would match, without collecting.
///
/// # Errors
///
/// Returns [`DictError::EmptyTerm`] for an empty prefix.
pub fn count(dictionary: &TermDictionary, prefix: &[u8]) -> Result<usize, DictError> {
    Ok(dictionary.prefix_range(prefix)?.len())
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
    use crate::fts::tokenizer::{Analyzer, TokenizerConfig};

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

    fn matched(dictionary: &TermDictionary, prefix: &str) -> Vec<String> {
        search(dictionary, prefix.as_bytes())
            .expect("valid prefix")
            .into_iter()
            .filter_map(|entry| String::from_utf8(entry.term).ok())
            .collect()
    }

    const VOCABULARY: [&str; 10] = [
        "engine", "engineer", "engineering", "engines", "england", "lexical", "search",
        "searching", "seat", "zebra",
    ];

    #[test]
    fn prefix_results_equal_brute_force_over_the_dictionary() {
        let dictionary = dictionary(&VOCABULARY);
        for prefix in ["e", "en", "eng", "engine", "s", "sea", "search", "z", "q", "engines"] {
            let mut expected: Vec<String> = VOCABULARY
                .iter()
                .filter(|term| term.starts_with(prefix))
                .map(|term| (*term).to_owned())
                .collect();
            expected.sort();
            let mut actual = matched(&dictionary, prefix);
            actual.sort();
            assert_eq!(actual, expected, "prefix {prefix:?}");
            assert_eq!(
                count(&dictionary, prefix.as_bytes()).expect("valid"),
                expected.len()
            );
        }
    }

    #[test]
    fn empty_prefix_is_rejected_with_a_typed_error() {
        let dictionary = dictionary(&VOCABULARY);
        assert_eq!(search(&dictionary, b""), Err(DictError::EmptyTerm));
        assert_eq!(count(&dictionary, b""), Err(DictError::EmptyTerm));
    }

    #[test]
    fn a_prefix_matching_nothing_returns_an_empty_range_not_an_error() {
        let dictionary = dictionary(&VOCABULARY);
        assert!(matched(&dictionary, "qqq").is_empty());
        assert!(matched(&dictionary, "zzzz").is_empty());
    }

    #[test]
    fn a_term_is_a_prefix_of_itself() {
        let dictionary = dictionary(&VOCABULARY);
        assert!(matched(&dictionary, "zebra").contains(&String::from("zebra")));
    }

    #[test]
    fn prefix_ending_mid_grapheme_matches_by_codepoint_rule() {
        // "cafe\u{0301}" folds to "cafe"; the prefix "caf" is a codepoint
        // prefix of the ANALYZED term, which is what is compared.
        let analyzer = Analyzer::new(TokenizerConfig::text_default()).expect("valid config");
        let folded: Vec<String> = analyzer
            .analyze("cafe\u{0301}")
            .into_iter()
            .map(|token| token.term)
            .collect();
        let refs: Vec<&str> = folded.iter().map(String::as_str).collect();
        let dictionary = dictionary(&refs);
        // A prefix that stops before the accented position still matches.
        assert!(!matched(&dictionary, "caf").is_empty());
        // And the combining mark never appears in the dictionary at all,
        // because folding removed it before indexing.
        assert!(
            folded.iter().all(|term| !term.contains('\u{0301}')),
            "a combining mark survived into the dictionary"
        );
    }

    #[test]
    fn a_prefix_can_match_a_term_whose_surface_form_it_does_not_prefix() {
        // "stra" is a prefix of the ANALYZED "strasse", though the surface
        // form is "straße" and "stra" is a prefix of that too. The point is
        // that the comparison is against the analyzed term.
        let analyzer = Analyzer::new(TokenizerConfig::text_default()).expect("valid config");
        let terms: Vec<String> = analyzer
            .analyze("stra\u{00DF}e")
            .into_iter()
            .map(|token| token.term)
            .collect();
        let refs: Vec<&str> = terms.iter().map(String::as_str).collect();
        let dictionary = dictionary(&refs);
        assert!(!matched(&dictionary, "strass").is_empty());
    }

    #[test]
    fn an_empty_dictionary_answers_every_prefix_with_nothing() {
        let dictionary = TermDictionary::default();
        assert!(search(&dictionary, b"anything").expect("valid").is_empty());
    }

    #[test]
    fn matches_carry_the_index_the_dictionary_agrees_with() {
        let dictionary = dictionary(&VOCABULARY);
        for entry in search(&dictionary, b"engine").expect("valid") {
            assert_eq!(
                dictionary.term_at(entry.term_index),
                Some(entry.term.as_slice())
            );
        }
    }
}
