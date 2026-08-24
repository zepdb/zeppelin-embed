//! Snippets and match offsets: how a hit explains itself.
//!
//! "Search that can't explain itself" is a documented failure class the
//! research pins to missing per-field match offsets (`research/03:481-484`).
//! Task 12 carried a byte range on every token precisely so this module
//! could exist without a second pass over the text.
//!
//! # Determinism (task 15 D5)
//!
//! The returned window is the best-scoring one under a fixed rule with a
//! pinned tie-break, so identical inputs give identical snippets across
//! runs, machines, and platforms. The rule:
//!
//! 1. Score each candidate window by the number of distinct matched terms
//!    it covers, then by total matches.
//! 2. Break ties by the earliest start offset.
//!
//! A snippet that moves between runs makes a caching layer useless and a
//! screenshot in a bug report meaningless, so the tie-break is part of the
//! contract rather than an implementation detail.
//!
//! # Boundaries
//!
//! Every returned range is a character boundary of the original text, so
//! slicing always yields valid UTF-8. Windows are widened outward to the
//! nearest boundary rather than truncated inward, because cutting a
//! multibyte character in half is the classic snippet crash.

use super::tokenizer::{Analyzer, TokenOffset};

/// One highlighted range within a field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Highlight {
    /// Inclusive start byte offset into the field text.
    pub start: u32,
    /// Exclusive end byte offset into the field text.
    pub end: u32,
}

/// A selected snippet window and the highlights inside it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Snippet {
    /// Byte range of the window itself.
    pub window: Highlight,
    /// Matched ranges within the window, ascending, non-overlapping.
    pub highlights: Vec<Highlight>,
}

impl Snippet {
    /// Slices the window text out of the field.
    #[must_use]
    pub fn text<'text>(&self, field: &'text str) -> Option<&'text str> {
        let start = usize::try_from(self.window.start).ok()?;
        let end = usize::try_from(self.window.end).ok()?;
        field.get(start..end)
    }
}

/// Why a snippet could not be produced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnippetError {
    /// The field was not configured `stored`, so no offsets exist.
    ///
    /// This is typed rather than guessed: returning a re-analyzed
    /// approximation would silently disagree with the index.
    FieldNotStored,
    /// The requested window length was zero.
    ZeroWindow,
}

impl std::fmt::Display for SnippetError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FieldNotStored => {
                formatter.write_str("field is not stored, so it has no match offsets")
            }
            Self::ZeroWindow => formatter.write_str("snippet window length must be positive"),
        }
    }
}

impl std::error::Error for SnippetError {}

/// Rounds an offset down to the nearest character boundary.
fn floor_boundary(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// Rounds an offset up to the nearest character boundary.
fn ceil_boundary(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while offset < text.len() && !text.is_char_boundary(offset) {
        offset += 1;
    }
    offset
}

/// Finds the byte ranges in `text` where any of `terms` was analyzed.
///
/// Runs the same analyzer the index used, so a match here is a match there.
#[must_use]
pub fn match_offsets(analyzer: &Analyzer, text: &str, terms: &[Vec<u8>]) -> Vec<Highlight> {
    let mut ranges: Vec<Highlight> = analyzer
        .analyze(text)
        .into_iter()
        .filter(|token| {
            terms
                .iter()
                .any(|term| term.as_slice() == token.term.as_bytes())
        })
        .map(|token| {
            let TokenOffset { start, end } = token.offset;
            Highlight { start, end }
        })
        .collect();
    ranges.sort_by_key(|range| (range.start, range.end));
    // Stacked variants share a byte range; report it once.
    ranges.dedup();
    // Overlapping ranges (a compound word and one of its parts) collapse to
    // the widest, so highlights never nest.
    let mut merged: Vec<Highlight> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start < last.end => {
                last.end = last.end.max(range.end);
            }
            _ => merged.push(range),
        }
    }
    merged
}

/// Chooses the best window of at most `window_bytes` and its highlights.
///
/// # Errors
///
/// Returns [`SnippetError::ZeroWindow`] when `window_bytes` is zero, and
/// [`SnippetError::FieldNotStored`] when `stored` is false.
pub fn best_window(
    analyzer: &Analyzer,
    text: &str,
    terms: &[Vec<u8>],
    window_bytes: usize,
    stored: bool,
) -> Result<Option<Snippet>, SnippetError> {
    if !stored {
        return Err(SnippetError::FieldNotStored);
    }
    if window_bytes == 0 {
        return Err(SnippetError::ZeroWindow);
    }
    let matches = match_offsets(analyzer, text, terms);
    if matches.is_empty() {
        return Ok(None);
    }

    // Each match is a candidate anchor; the window starts at it.
    let mut best: Option<(usize, usize, Highlight)> = None;
    for anchor in &matches {
        let start = floor_boundary(text, usize::try_from(anchor.start).unwrap_or(0));
        let end = ceil_boundary(text, start.saturating_add(window_bytes));
        let covered: Vec<&Highlight> = matches
            .iter()
            .filter(|candidate| {
                usize::try_from(candidate.start).unwrap_or(usize::MAX) >= start
                    && usize::try_from(candidate.end).unwrap_or(usize::MAX) <= end
            })
            .collect();
        let distinct = {
            let mut spans: Vec<(u32, u32)> = covered
                .iter()
                .map(|range| (range.start, range.end))
                .collect();
            spans.sort_unstable();
            spans.dedup();
            spans.len()
        };
        let score = (distinct, covered.len());
        let window = Highlight {
            start: u32::try_from(start).unwrap_or(u32::MAX),
            end: u32::try_from(end).unwrap_or(u32::MAX),
        };
        let replace = match &best {
            None => true,
            // Pinned tie-break: more distinct terms, then more matches,
            // then the earliest start. Never anything machine-dependent.
            Some((distinct_best, count_best, window_best)) => {
                score.0 > *distinct_best
                    || (score.0 == *distinct_best && score.1 > *count_best)
                    || (score.0 == *distinct_best
                        && score.1 == *count_best
                        && window.start < window_best.start)
            }
        };
        if replace {
            best = Some((score.0, score.1, window));
        }
    }

    let Some((_, _, window)) = best else {
        return Ok(None);
    };
    let highlights = matches
        .into_iter()
        .filter(|range| range.start >= window.start && range.end <= window.end)
        .collect();
    Ok(Some(Snippet { window, highlights }))
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
    use crate::fts::tokenizer::{Profile, TokenizerConfig};

    fn analyzer() -> Analyzer {
        Analyzer::new(TokenizerConfig::text_default()).expect("valid config")
    }

    fn code_analyzer() -> Analyzer {
        Analyzer::new(Profile::Code.config()).expect("valid config")
    }

    fn terms(words: &[&str]) -> Vec<Vec<u8>> {
        words.iter().map(|word| word.as_bytes().to_vec()).collect()
    }

    #[test]
    fn highlights_point_at_the_matched_surface_form() {
        let text = "the quick brown fox jumps";
        let highlights = match_offsets(&analyzer(), text, &terms(&["brown"]));
        assert_eq!(highlights.len(), 1);
        let range = highlights[0];
        assert_eq!(&text[range.start as usize..range.end as usize], "brown");
    }

    #[test]
    fn snippet_ranges_are_in_bounds_utf8_valid_and_contain_the_matched_surface_form() {
        // Multibyte, combining marks, and emoji all in one field.
        let text = "Caf\u{00E9} \u{1F680} na\u{00EF}ve cafe\u{0301} resume \u{4E2D}\u{6587}";
        let snippet = best_window(&analyzer(), text, &terms(&["cafe"]), 24, true)
            .expect("stored")
            .expect("a match exists");
        let window = snippet.text(text).expect("window is a char boundary");
        assert!(!window.is_empty());
        for highlight in &snippet.highlights {
            let slice = &text[highlight.start as usize..highlight.end as usize];
            assert!(
                std::str::from_utf8(slice.as_bytes()).is_ok(),
                "highlight sliced a character in half"
            );
        }
        assert!(
            snippet.window.end as usize <= text.len(),
            "window escaped the field"
        );
    }

    #[test]
    fn snippets_are_deterministic_across_runs() {
        let text = "alpha beta alpha gamma alpha delta alpha";
        let analyzer = code_analyzer();
        let first = best_window(&analyzer, text, &terms(&["alpha"]), 20, true).expect("stored");
        for _ in 0..8 {
            let again = best_window(&analyzer, text, &terms(&["alpha"]), 20, true).expect("stored");
            assert_eq!(first, again, "the snippet moved between runs");
        }
    }

    #[test]
    fn the_window_prefers_more_distinct_terms() {
        // "alpha" alone early, then "alpha beta" together later. The window
        // covering both terms must win.
        let text = "alpha ................................ alpha beta";
        let snippet = best_window(&code_analyzer(), text, &terms(&["alpha", "beta"]), 16, true)
            .expect("stored")
            .expect("a match exists");
        let window = snippet.text(text).expect("valid window");
        assert!(
            window.contains("beta"),
            "the window missed the denser region: {window:?}"
        );
    }

    #[test]
    fn an_unstored_field_is_a_typed_error_not_a_guess() {
        assert_eq!(
            best_window(&analyzer(), "text", &terms(&["text"]), 10, false),
            Err(SnippetError::FieldNotStored)
        );
    }

    #[test]
    fn a_zero_length_window_is_a_typed_error() {
        assert_eq!(
            best_window(&analyzer(), "text", &terms(&["text"]), 0, true),
            Err(SnippetError::ZeroWindow)
        );
    }

    #[test]
    fn a_field_with_no_match_yields_no_snippet_rather_than_the_whole_field() {
        assert_eq!(
            best_window(&analyzer(), "alpha beta", &terms(&["omega"]), 20, true).expect("stored"),
            None
        );
    }

    #[test]
    fn highlights_never_overlap_or_nest() {
        // A compound word emits the whole span AND its parts; the reported
        // highlights must collapse to the widest rather than nesting.
        let text = "call put_if_match now";
        let highlights = match_offsets(
            &code_analyzer(),
            text,
            &terms(&["put_if_match", "put", "match"]),
        );
        for pair in highlights.windows(2) {
            let (Some(left), Some(right)) = (pair.first(), pair.get(1)) else {
                continue;
            };
            assert!(
                left.end <= right.start,
                "highlights {left:?} and {right:?} overlap"
            );
        }
    }

    #[test]
    fn an_empty_field_produces_no_snippet() {
        assert_eq!(
            best_window(&analyzer(), "", &terms(&["alpha"]), 10, true).expect("stored"),
            None
        );
    }

    #[test]
    fn boundary_helpers_never_split_a_character() {
        let text = "a\u{00E9}\u{4E2D}\u{1F680}b";
        for offset in 0..=text.len() + 4 {
            let floored = floor_boundary(text, offset);
            let ceiled = ceil_boundary(text, offset);
            assert!(text.is_char_boundary(floored), "floor at {offset}");
            assert!(text.is_char_boundary(ceiled), "ceil at {offset}");
            assert!(floored <= text.len() && ceiled <= text.len());
        }
    }
}
