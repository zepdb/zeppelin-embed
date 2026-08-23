//! Versioned stopword lists shipped as data, never invented here.
//!
//! Task 13's guardrail is explicit: "No stopword-list creativity: ship the
//! standard Lucene English list versioned as data; changes are epoch
//! changes." This is that list, verbatim — Lucene's
//! `EnglishAnalyzer.ENGLISH_STOP_WORDS_SET`, the same 33 terms Anserini and
//! Pyserini use for the BEIR runs task 13 gates against. Matching the
//! reference implementation's stopword set is part of matching its numbers.
//!
//! Removing a stopword leaves a position gap rather than closing up, so
//! phrase queries stay coherent across the hole. That is Lucene's
//! `enablePositionIncrements` behaviour, and task 15's phrase matching
//! depends on it.

/// Identifies a shipped stopword list; an epoch-digest input.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u16)]
pub enum StopwordList {
    /// No stopword removal.
    None = 0,
    /// Lucene's 33-term English set, as used by Anserini and Pyserini.
    LuceneEnglish = 1,
}

/// Lucene's `ENGLISH_STOP_WORDS_SET`, sorted for binary search.
static LUCENE_ENGLISH: [&str; 33] = [
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "if", "in", "into", "is", "it",
    "no", "not", "of", "on", "or", "such", "that", "the", "their", "then", "there", "these",
    "they", "this", "to", "was", "will", "with",
];

impl StopwordList {
    /// Returns the permanent numeric identifier.
    #[must_use]
    pub const fn id(self) -> u16 {
        self as u16
    }

    /// Returns the terms in this list, ascending.
    #[must_use]
    pub const fn terms(self) -> &'static [&'static str] {
        match self {
            Self::None => &[],
            Self::LuceneEnglish => &LUCENE_ENGLISH,
        }
    }

    /// Returns true when `term` is a stopword of this list.
    #[must_use]
    pub fn contains(self, term: &str) -> bool {
        self.terms().binary_search(&term).is_ok()
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_lucene_list_is_sorted_and_complete() {
        assert_eq!(LUCENE_ENGLISH.len(), 33);
        assert!(
            LUCENE_ENGLISH
                .windows(2)
                .all(|pair| match (pair.first(), pair.get(1)) {
                    (Some(left), Some(right)) => left < right,
                    _ => true,
                }),
            "the stopword list must be sorted for binary search"
        );
    }

    #[test]
    fn membership_matches_the_shipped_list() {
        assert!(StopwordList::LuceneEnglish.contains("the"));
        assert!(StopwordList::LuceneEnglish.contains("with"));
        assert!(!StopwordList::LuceneEnglish.contains("engine"));
        assert!(!StopwordList::None.contains("the"));
    }
}
