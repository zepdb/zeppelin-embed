//! Word segmentation with identifier preservation.
//!
//! # Why not UAX-29 word boundaries
//!
//! UAX-29 is the rule the incumbents follow, and it is the rule that breaks
//! them for this product. SQLite FTS5's `unicode61` splits `don't` into
//! `don` and `t` and shatters `i-485` (`research/03:421`). A search engine
//! whose users type ticket ids, SKUs, and identifiers cannot afford that.
//!
//! The rule here is: a word is a maximal run of alphanumerics, plus joiner
//! characters that sit *between* two alphanumerics, plus a trailing `+`/`#`
//! run so `C++` and `F#` survive. Decomposition into parts happens later in
//! the pipeline, so `state-of-the-art` is both one identifier-shaped term
//! and four searchable words. Nothing is lost; the incumbents lose the
//! identifier, we keep both.
//!
//! CJK runs have no spaces to segment on. V1 emits overlapping bigrams,
//! which is Lucene's `CJKBigramFilter` behaviour and the documented v1
//! answer (plan 12 R7). Its exact output is frozen by the `cjk` conformance
//! fixture so task 21 can migrate it deliberately.

use super::fold::segmentation_equivalent;

/// One segmented span of the input, addressed by byte offsets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Span {
    /// Inclusive start byte offset into the analyzed text.
    pub(crate) start: usize,
    /// Exclusive end byte offset into the analyzed text.
    pub(crate) end: usize,
    /// True when the span is a run of ideographs rather than a word.
    pub(crate) ideographic: bool,
}

/// Characters that join two alphanumerics into a single term.
const fn is_joiner(value: char) -> bool {
    matches!(value, '_' | '-' | '.' | '\'' | '+' | '#' | '@' | '&')
}

/// Characters absorbed as a trailing suffix so `C++` and `F#` survive.
const fn is_trailing_symbol(value: char) -> bool {
    matches!(value, '+' | '#')
}

/// Returns true for scripts that are written without spaces.
pub(crate) const fn is_ideograph(value: char) -> bool {
    matches!(value as u32,
        0x3040..=0x30FF      // Hiragana and Katakana
        | 0x3400..=0x4DBF    // CJK Unified Ideographs Extension A
        | 0x4E00..=0x9FFF    // CJK Unified Ideographs
        | 0xAC00..=0xD7AF    // Hangul Syllables
        | 0xF900..=0xFAFF    // CJK Compatibility Ideographs
    )
}

fn is_word_character(value: char) -> bool {
    value.is_alphanumeric() && !is_ideograph(value)
}

/// Splits `text` into word and ideograph spans.
///
/// Offsets are byte offsets into `text` itself, never into a normalized
/// copy, so every emitted token can be sliced back to its surface form.
pub(crate) fn segment(text: &str) -> Vec<Span> {
    let mut spans = Vec::new();
    let bytes = text.len();
    let mut characters = text.char_indices().peekable();

    while let Some((index, raw)) = characters.next() {
        let value = segmentation_equivalent(raw);
        if is_ideograph(raw) {
            let mut end = index + raw.len_utf8();
            while let Some(&(next_index, next_raw)) = characters.peek() {
                if !is_ideograph(next_raw) {
                    break;
                }
                end = next_index + next_raw.len_utf8();
                characters.next();
            }
            spans.push(Span {
                start: index,
                end,
                ideographic: true,
            });
            continue;
        }
        if !is_word_character(value) {
            continue;
        }

        let mut end = index + raw.len_utf8();
        loop {
            let Some(&(next_index, next_raw)) = characters.peek() else {
                break;
            };
            let next = segmentation_equivalent(next_raw);
            if is_word_character(next) {
                end = next_index + next_raw.len_utf8();
                characters.next();
                continue;
            }
            if is_joiner(next) {
                // A joiner only joins when an alphanumeric follows it.
                let after = next_index + next_raw.len_utf8();
                let follows_word = text
                    .get(after..)
                    .and_then(|rest| rest.chars().next())
                    .map(segmentation_equivalent)
                    .is_some_and(is_word_character);
                if follows_word {
                    end = after;
                    characters.next();
                    continue;
                }
            }
            break;
        }

        // Absorb a trailing `+`/`#` run: `C++`, `C#`, `F#`.
        let mut trailing = 0_usize;
        while trailing < 2 {
            let Some(&(next_index, next_raw)) = characters.peek() else {
                break;
            };
            let next = segmentation_equivalent(next_raw);
            if !is_trailing_symbol(next) {
                break;
            }
            let after = next_index + next_raw.len_utf8();
            let follows_word = text
                .get(after..)
                .and_then(|rest| rest.chars().next())
                .map(segmentation_equivalent)
                .is_some_and(is_word_character);
            if follows_word {
                break;
            }
            end = after;
            trailing += 1;
            characters.next();
        }

        debug_assert!(end <= bytes, "segment span escaped the input");
        spans.push(Span {
            start: index,
            end,
            ideographic: false,
        });
    }

    spans
}

/// Splits an ideograph span into overlapping bigram spans.
///
/// A single ideograph yields one unigram span so a one-character query is
/// still findable.
pub(crate) fn ideograph_bigrams(text: &str, span: Span) -> Vec<Span> {
    let Some(slice) = text.get(span.start..span.end) else {
        return Vec::new();
    };
    let boundaries = slice
        .char_indices()
        .map(|(index, _)| span.start + index)
        .chain(std::iter::once(span.end))
        .collect::<Vec<_>>();
    if boundaries.len() <= 2 {
        return vec![span];
    }
    let mut spans = Vec::with_capacity(boundaries.len().saturating_sub(2));
    for window in boundaries.windows(3) {
        let (Some(start), Some(end)) = (window.first(), window.get(2)) else {
            continue;
        };
        spans.push(Span {
            start: *start,
            end: *end,
            ideographic: true,
        });
    }
    spans
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<&str> {
        segment(text)
            .into_iter()
            .map(|span| &text[span.start..span.end])
            .collect()
    }

    #[test]
    fn identifiers_survive_segmentation() {
        assert_eq!(words("put_if_match"), vec!["put_if_match"]);
        assert_eq!(words("i-485 filed"), vec!["i-485", "filed"]);
        assert_eq!(words("GPT-5.6"), vec!["GPT-5.6"]);
        assert_eq!(words("KV-cache"), vec!["KV-cache"]);
    }

    #[test]
    fn trailing_symbol_languages_survive() {
        assert_eq!(words("C++ and C# and F#"), vec!["C++", "and", "C#", "and", "F#"]);
    }

    #[test]
    fn apostrophes_and_hyphens_do_not_shatter() {
        assert_eq!(words("don't"), vec!["don't"]);
        assert_eq!(words("don\u{2019}t"), vec!["don\u{2019}t"]);
        assert_eq!(words("state-of-the-art"), vec!["state-of-the-art"]);
    }

    #[test]
    fn emails_stay_whole() {
        assert_eq!(words("mail anup@example.com now"), vec!["mail", "anup@example.com", "now"]);
    }

    #[test]
    fn trailing_punctuation_is_not_absorbed() {
        assert_eq!(words("hello, world."), vec!["hello", "world"]);
        assert_eq!(words("end-"), vec!["end"]);
    }

    #[test]
    fn ideograph_runs_become_overlapping_bigrams() {
        let text = "\u{4E2D}\u{6587}\u{5206}\u{8BCD}";
        let spans = segment(text);
        assert_eq!(spans.len(), 1);
        assert!(spans[0].ideographic);
        let bigrams = ideograph_bigrams(text, spans[0]);
        let rendered = bigrams
            .iter()
            .map(|span| &text[span.start..span.end])
            .collect::<Vec<_>>();
        assert_eq!(
            rendered,
            vec!["\u{4E2D}\u{6587}", "\u{6587}\u{5206}", "\u{5206}\u{8BCD}"]
        );
    }

    #[test]
    fn a_lone_ideograph_stays_a_unigram() {
        let text = "\u{4E2D}";
        let spans = segment(text);
        let bigrams = ideograph_bigrams(text, spans[0]);
        assert_eq!(bigrams.len(), 1);
        assert_eq!(&text[bigrams[0].start..bigrams[0].end], text);
    }

    #[test]
    fn spans_are_ordered_in_bounds_and_non_overlapping() {
        let text = "alpha beta_gamma i-485 \u{4E2D}\u{6587} delta";
        let mut previous_end = 0;
        for span in segment(text) {
            assert!(span.start >= previous_end);
            assert!(span.start < span.end);
            assert!(span.end <= text.len());
            assert!(text.get(span.start..span.end).is_some());
            previous_end = span.end;
        }
    }
}
