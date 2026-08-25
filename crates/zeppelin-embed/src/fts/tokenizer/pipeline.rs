//! The analysis pipeline: segment, fold, decompose, stack, filter, emit.
//!
//! # Stage order, and why it is this order
//!
//! 1. **Segment** the original bytes, so every offset is exact.
//! 2. **Decompose** each word into parts at joiners, case changes, and
//!    letter/digit transitions, emitting the whole word and its catenation
//!    beside the parts. This is Lucene's `WordDelimiterGraphFilter` shape
//!    with `preserveOriginal`: `state-of-the-art` stays one identifier AND
//!    becomes four searchable words. The incumbents choose one or the
//!    other; keeping both is free at index time and strictly better recall.
//! 3. **Stack variants** from the vocabulary and from number words. Both
//!    read the *unfiltered* term stream, because a vocabulary surface form
//!    such as `put if match` contains a stopword and would never match after
//!    filtering.
//! 4. **Filter and stem** the surface parts only. Originals, catenations,
//!    and stacked variants are never stemmed: they are identities, not
//!    words.
//! 5. **Order** deterministically by position and emission rank.
//!
//! Stopword removal leaves a position gap rather than closing up, so phrase
//! queries stay coherent across the hole (Lucene's
//! `enablePositionIncrements`). Task 15's phrase matching depends on it.

use std::collections::HashSet;

use super::fold::{fold, segmentation_equivalent};
use super::numbers::{self, NumberWord};
use super::segment::{ideograph_bigrams, segment};
use super::stemmer;
use super::{FoldingForm, Stemmer, Token, TokenFlags, TokenOffset, TokenizerConfig};

/// Emission rank, fixing the order of tokens that share a position.
///
/// The order is part of the frozen golden streams, so it must not depend on
/// iteration order anywhere.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
enum Rank {
    /// A surface word or word part.
    Surface = 0,
    /// The undecomposed original of a compound word.
    Original = 1,
    /// The joiner-free catenation of a compound word.
    Catenation = 2,
    /// A vocabulary canonical term.
    Vocabulary = 3,
    /// A number-word or digit variant.
    Number = 4,
}

/// One token before filtering and ordering.
#[derive(Clone, Debug)]
struct Emission {
    term: String,
    position: u32,
    start: u32,
    end: u32,
    flags: TokenFlags,
    rank: Rank,
    /// True when this term is one part of a decomposed compound word.
    ///
    /// Number-word stacking skips these: spelling `1234` out of the SKU
    /// `ABC-1234` as `onethousandtwohundredthirtyfour` is pure index noise,
    /// because nobody spells a part number aloud.
    compound: bool,
}

fn clamp_offset(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

const fn is_joiner(value: char) -> bool {
    matches!(value, '_' | '-' | '.' | '\'' | '+' | '#' | '@' | '&')
}

/// Applies the configured folding to a surface slice.
fn fold_term(config: &TokenizerConfig, surface: &str) -> String {
    match config.folding {
        FoldingForm::None => surface.to_owned(),
        FoldingForm::NfkcSearchFold => fold(surface),
    }
}

/// Splits a word's byte range into part ranges.
///
/// Splits happen at joiners, at lower-to-upper case transitions, and at
/// letter/digit transitions. Ranges are byte ranges into the original text.
fn split_parts(text: &str, start: usize, end: usize) -> Vec<(usize, usize)> {
    let Some(slice) = text.get(start..end) else {
        return Vec::new();
    };
    let mut parts = Vec::new();
    let mut part_start: Option<usize> = None;
    let mut previous: Option<char> = None;

    for (offset, raw) in slice.char_indices() {
        let absolute = start + offset;
        let value = segmentation_equivalent(raw);
        if is_joiner(value) {
            if let Some(open) = part_start.take() {
                parts.push((open, absolute));
            }
            previous = None;
            continue;
        }
        if let (Some(last), Some(open)) = (previous, part_start) {
            let case_break = last.is_lowercase() && value.is_uppercase();
            let digit_break = last.is_numeric() != value.is_numeric();
            if case_break || digit_break {
                parts.push((open, absolute));
                part_start = Some(absolute);
            }
        }
        if part_start.is_none() {
            part_start = Some(absolute);
        }
        previous = Some(value);
    }
    if let Some(open) = part_start
        && open < end
    {
        parts.push((open, end));
    }
    parts
}

/// Returns true when the term is exactly one alphabetic character.
fn is_single_letter(term: &str) -> bool {
    let mut chars = term.chars();
    matches!(
        (chars.next(), chars.next()),
        (Some(only), None) if only.is_alphabetic()
    )
}

fn contains_digit(term: &str) -> bool {
    term.chars().any(char::is_numeric)
}

fn is_all_alphabetic(term: &str) -> bool {
    !term.is_empty() && term.chars().all(char::is_alphabetic)
}

/// Decides the `no_fuzzy` flag for a surface term.
///
/// A term is protected from fuzzy matching when it carries a digit or a
/// joiner. `C++`, `i-485`, and `GPT-5.6` are the cases that matter: a fuzzy
/// match on any of them is almost always wrong.
fn surface_flags(surface: &str) -> TokenFlags {
    let joined = surface.chars().map(segmentation_equivalent).any(is_joiner);
    if joined || contains_digit(surface) {
        TokenFlags::NO_FUZZY
    } else {
        TokenFlags::empty()
    }
}

/// Stage 1 and 2: segmentation and word decomposition.
fn emit_surface(config: &TokenizerConfig, text: &str) -> (Vec<Emission>, u32) {
    let mut emissions = Vec::new();
    let mut position: u32 = 0;

    for span in segment(text) {
        if span.ideographic {
            for bigram in ideograph_bigrams(text, span) {
                let Some(surface) = text.get(bigram.start..bigram.end) else {
                    continue;
                };
                let term = fold_term(config, surface);
                if term.is_empty() {
                    continue;
                }
                emissions.push(Emission {
                    term,
                    position,
                    start: clamp_offset(bigram.start),
                    end: clamp_offset(bigram.end),
                    // CJK bigrams are synthetic adjacency, never fuzzy targets.
                    flags: TokenFlags::NO_FUZZY,
                    rank: Rank::Surface,
                    compound: false,
                });
                position = position.saturating_add(1);
            }
            continue;
        }

        let Some(surface) = text.get(span.start..span.end) else {
            continue;
        };
        let whole = fold_term(config, surface);
        let parts = if config.decompose_words {
            split_parts(text, span.start, span.end)
        } else {
            vec![(span.start, span.end)]
        };

        if parts.len() <= 1 {
            if !whole.is_empty() {
                emissions.push(Emission {
                    term: whole,
                    position,
                    start: clamp_offset(span.start),
                    end: clamp_offset(span.end),
                    flags: surface_flags(surface),
                    rank: Rank::Surface,
                    compound: false,
                });
            }
            position = position.saturating_add(1);
            continue;
        }

        // The undecomposed identifier, preserved.
        if !whole.is_empty() {
            emissions.push(Emission {
                term: whole.clone(),
                position,
                start: clamp_offset(span.start),
                end: clamp_offset(span.end),
                flags: TokenFlags::NO_FUZZY,
                rank: Rank::Original,
                compound: false,
            });
        }

        let mut catenation = String::new();
        for (part_start, part_end) in &parts {
            let Some(part_surface) = text.get(*part_start..*part_end) else {
                continue;
            };
            catenation.push_str(&fold_term(config, part_surface));
        }
        if config.emit_catenation && !catenation.is_empty() && catenation != whole {
            emissions.push(Emission {
                term: catenation,
                position,
                start: clamp_offset(span.start),
                end: clamp_offset(span.end),
                flags: TokenFlags::NO_FUZZY.union(TokenFlags::VARIANT),
                rank: Rank::Catenation,
                compound: false,
            });
        }

        for (index, (part_start, part_end)) in parts.iter().enumerate() {
            let Some(part_surface) = text.get(*part_start..*part_end) else {
                continue;
            };
            let term = fold_term(config, part_surface);
            if term.is_empty() {
                continue;
            }
            // A one-LETTER part of a decomposed word is stop-level noise
            // in linguistic text; the whole word and the catenation still
            // carry its identity. A one-DIGIT part is kept: `type-2` and
            // `SARS-CoV-2` are discriminated by exactly that digit. The
            // dropped part's position stays spent, exactly as a removed
            // stopword's does.
            if config.drop_single_char_parts && is_single_letter(&term) {
                continue;
            }
            let offset = u32::try_from(index).unwrap_or(u32::MAX);
            emissions.push(Emission {
                term,
                position: position.saturating_add(offset),
                start: clamp_offset(*part_start),
                end: clamp_offset(*part_end),
                flags: surface_flags(part_surface),
                rank: Rank::Surface,
                compound: true,
            });
        }
        position = position.saturating_add(u32::try_from(parts.len()).unwrap_or(1));
    }

    (emissions, position)
}

/// Returns the FIRST surface term at each position.
///
/// Both stacking stages only ever read a position's first surface
/// emission, so the table holds one index per position rather than a
/// heap-allocated bucket per position — the buckets were one `Vec` per
/// token of every analyzed document.
fn surface_by_position(emissions: &[Emission], positions: u32) -> Vec<Option<usize>> {
    let count = usize::try_from(positions).unwrap_or(0);
    let mut table = vec![None; count];
    for (index, emission) in emissions.iter().enumerate() {
        if emission.rank != Rank::Surface {
            continue;
        }
        let Ok(slot) = usize::try_from(emission.position) else {
            continue;
        };
        if let Some(entry) = table.get_mut(slot)
            && entry.is_none()
        {
            *entry = Some(index);
        }
    }
    table
}

/// Stage 3a: vocabulary stacking.
fn stack_vocabulary(config: &TokenizerConfig, emissions: &mut Vec<Emission>, positions: u32) {
    if config.vocabulary.is_empty() {
        return;
    }
    let table = surface_by_position(emissions, positions);
    let longest = config.vocabulary.longest_surface_terms().max(1);
    let mut stacked: Vec<Emission> = Vec::new();

    for start in 0..table.len() {
        let mut length = longest.min(table.len().saturating_sub(start));
        while length >= 1 {
            // A run is only considered when every position in it has a term.
            let mut run: Vec<String> = Vec::with_capacity(length);
            let mut span_start = u32::MAX;
            let mut span_end = 0_u32;
            let mut complete = true;
            for offset in 0..length {
                let Some(first) = table
                    .get(start + offset)
                    .copied()
                    .flatten()
                    .and_then(|index| emissions.get(index))
                else {
                    complete = false;
                    break;
                };
                run.push(first.term.clone());
                span_start = span_start.min(first.start);
                span_end = span_end.max(first.end);
            }
            if complete && let Some(canonical) = config.vocabulary.canonical_for(&run) {
                let position = u32::try_from(start).unwrap_or(u32::MAX);
                {
                    stacked.push(Emission {
                        term: canonical.to_owned(),
                        position,
                        start: span_start,
                        end: span_end,
                        flags: TokenFlags::NO_FUZZY.union(TokenFlags::VARIANT),
                        rank: Rank::Vocabulary,
                        compound: false,
                    });
                    break;
                }
            }
            length -= 1;
        }
    }
    emissions.extend(stacked);
}

/// Stage 3b: number-word and digit variants.
///
/// Runs are consecutive positions only. A stopword inside a spelled number
/// splits the run, which is predictable and never composes two unrelated
/// numbers into one; the alternative — bridging position gaps — silently
/// invents values such as `eleven` from `five the six`.
fn stack_numbers(config: &TokenizerConfig, emissions: &mut Vec<Emission>, positions: u32) {
    if !config.number_words {
        return;
    }
    let table = surface_by_position(emissions, positions);
    let mut stacked: Vec<Emission> = Vec::new();

    // Digit terms gain their spelled variant.
    for entry in &table {
        let Some(emission) = entry.and_then(|index| emissions.get(index)) else {
            continue;
        };
        if emission.compound {
            continue;
        }
        let Some(value) = numbers::parse_digits(&emission.term) else {
            continue;
        };
        let Some(spelled) = numbers::spell_joined(value) else {
            continue;
        };
        stacked.push(Emission {
            term: spelled,
            position: emission.position,
            start: emission.start,
            end: emission.end,
            flags: TokenFlags::NO_FUZZY.union(TokenFlags::VARIANT),
            rank: Rank::Number,
            compound: false,
        });
    }

    // Spelled runs gain both the digit form and the joined word form.
    let mut start = 0_usize;
    while start < table.len() {
        let mut words: Vec<NumberWord> = Vec::new();
        let mut joined = String::new();
        let mut span_start = u32::MAX;
        let mut span_end = 0_u32;
        let mut end = start;
        while end < table.len() {
            let Some(emission) = table
                .get(end)
                .copied()
                .flatten()
                .and_then(|index| emissions.get(index))
            else {
                break;
            };
            if emission.compound {
                break;
            }
            let Some(word) = numbers::classify(&emission.term) else {
                break;
            };
            words.push(word);
            if !matches!(word, NumberWord::Connective) {
                joined.push_str(&emission.term);
            }
            span_start = span_start.min(emission.start);
            span_end = span_end.max(emission.end);
            end += 1;
        }
        if end > start {
            if let Some(value) = numbers::compose(&words) {
                let position = u32::try_from(start).unwrap_or(u32::MAX);
                let digits = value.to_string();
                for term in [digits, joined] {
                    if term.is_empty() {
                        continue;
                    }
                    stacked.push(Emission {
                        term,
                        position,
                        start: span_start,
                        end: span_end,
                        flags: TokenFlags::NO_FUZZY.union(TokenFlags::VARIANT),
                        rank: Rank::Number,
                        compound: false,
                    });
                }
            }
            start = end;
        } else {
            start += 1;
        }
    }
    emissions.extend(stacked);
}

/// Stage 4: stopword removal and stemming, applied to surface terms only.
fn filter_and_stem(config: &TokenizerConfig, emissions: Vec<Emission>) -> Vec<Emission> {
    emissions
        .into_iter()
        .filter_map(|mut emission| {
            if emission.rank != Rank::Surface {
                return Some(emission);
            }
            if config.stopwords.contains(&emission.term) {
                // Dropped, but the position it occupied stays spent.
                return None;
            }
            if config.stemmer == Stemmer::EnglishPorter2 && is_all_alphabetic(&emission.term) {
                emission.term = stemmer::stem(&emission.term);
            }
            Some(emission)
        })
        .collect()
}

/// Runs the whole pipeline.
pub(crate) fn analyze(config: &TokenizerConfig, text: &str) -> Vec<Token> {
    if text.is_empty() {
        return Vec::new();
    }
    let (mut emissions, positions) = emit_surface(config, text);
    stack_vocabulary(config, &mut emissions, positions);
    stack_numbers(config, &mut emissions, positions);
    let mut emissions = filter_and_stem(config, emissions);

    emissions.sort_by(|left, right| {
        left.position
            .cmp(&right.position)
            .then(left.rank.cmp(&right.rank))
            .then(left.term.cmp(&right.term))
            .then(left.start.cmp(&right.start))
    });
    // One position must never carry the same term twice, whatever produced
    // it. A word whose catenation collapses onto one of its own parts —
    // `B#` plus a combining mark that folds away — otherwise emits `b` as
    // both a part and a catenation, and task 13 would read that duplicate
    // as a term frequency of two. Sorting put the lowest rank first, so the
    // surviving copy is the most surface-like one.
    let mut seen: HashSet<(u32, &str)> = HashSet::with_capacity(emissions.len());
    let mut kept = Vec::with_capacity(emissions.len());
    for emission in &emissions {
        if seen.insert((emission.position, emission.term.as_str())) {
            kept.push(emission.clone());
        }
    }
    let emissions = kept;

    emissions
        .into_iter()
        .map(|emission| Token {
            term: emission.term,
            position: emission.position,
            offset: TokenOffset {
                start: emission.start,
                end: emission.end,
            },
            flags: emission.flags,
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::fts::tokenizer::vocab::Vocabulary;
    use crate::fts::tokenizer::{Analyzer, Profile};

    fn analyze_with(config: TokenizerConfig, text: &str) -> Vec<Token> {
        Analyzer::new(config).expect("valid config").analyze(text)
    }

    fn terms(tokens: &[Token]) -> Vec<String> {
        tokens.iter().map(|token| token.term.clone()).collect()
    }

    fn terms_at(tokens: &[Token], position: u32) -> Vec<String> {
        tokens
            .iter()
            .filter(|token| token.position == position)
            .map(|token| token.term.clone())
            .collect()
    }

    #[test]
    fn single_character_parts_are_dropped_from_linguistic_profiles() {
        // `401k` must keep its identity (original, catenation, and the
        // `401` part) while the one-character `k` part disappears; the
        // apostrophe splits of `don't` and `investor's` lose only their
        // `t` and `s`. Dropped parts leave their positions spent, so the
        // analyzed length is unchanged.
        let tokens = analyze_with(Profile::TextDefault.config(), "401k don't investor's fund");
        let all = terms(&tokens);
        assert!(all.iter().any(|term| term == "401k"), "original survives");
        assert!(all.iter().any(|term| term == "401"), "long part survives");
        assert!(
            all.iter().any(|term| term == "investor"),
            "long part survives"
        );
        assert!(all.iter().any(|term| term == "don"), "long part survives");
        for junk in ["k", "t", "s"] {
            assert!(
                !all.iter().any(|term| term == junk),
                "single-character part {junk:?} must be dropped, got {all:?}"
            );
        }
        // The positions the dropped parts occupied stay spent: `fund`
        // starts a fresh word after `investor's` two part positions.
        let fund_position = tokens
            .iter()
            .find(|token| token.term == "fund")
            .map(|token| token.position);
        assert_eq!(fund_position, Some(6), "dropped parts keep their positions");

        // A single DIGIT part survives: `type-2` is discriminated by it.
        let typed = terms(&analyze_with(
            Profile::TextDefault.config(),
            "type-2 diabetes",
        ));
        assert!(
            typed.iter().any(|term| term == "2"),
            "single digit parts are kept, got {typed:?}"
        );

        // The identifier profile keeps one-character parts: `x` in `x_max`
        // is a real search target in code.
        let code = terms(&analyze_with(Profile::Code.config(), "x_max"));
        assert!(
            code.iter().any(|term| term == "x"),
            "code keeps short parts"
        );
    }

    #[test]
    fn an_identifier_keeps_its_whole_form_its_catenation_and_its_parts() {
        let tokens = analyze_with(Profile::Code.config(), "put_if_match");
        assert_eq!(
            terms_at(&tokens, 0),
            vec!["put", "put_if_match", "putifmatch"]
        );
        assert_eq!(terms_at(&tokens, 1), vec!["if"]);
        assert_eq!(terms_at(&tokens, 2), vec!["match"]);
    }

    #[test]
    fn stopwords_leave_a_position_gap_rather_than_closing_up() {
        let tokens = analyze_with(TokenizerConfig::text_default(), "put if match");
        assert!(terms_at(&tokens, 1).is_empty(), "the stopword must be gone");
        assert_eq!(terms_at(&tokens, 0), vec!["put"]);
        assert_eq!(terms_at(&tokens, 2), vec!["match"]);
    }

    #[test]
    fn case_and_digit_transitions_split_parts() {
        let tokens = analyze_with(Profile::Code.config(), "getUserID42");
        let all = terms(&tokens);
        assert!(all.contains(&"get".to_owned()), "{all:?}");
        assert!(all.contains(&"user".to_owned()), "{all:?}");
        assert!(all.contains(&"42".to_owned()), "{all:?}");
    }

    #[test]
    fn number_words_and_digits_emit_each_others_variants_at_one_position() {
        let spelled = analyze_with(TokenizerConfig::text_default(), "twenty five");
        assert!(terms_at(&spelled, 0).contains(&"25".to_owned()));
        assert!(terms_at(&spelled, 0).contains(&"twentyfive".to_owned()));

        let digits = analyze_with(TokenizerConfig::text_default(), "25");
        assert!(terms_at(&digits, 0).contains(&"25".to_owned()));
        assert!(terms_at(&digits, 0).contains(&"twentyfive".to_owned()));
    }

    #[test]
    fn positions_strictly_increase_except_within_a_stack() {
        let tokens = analyze_with(TokenizerConfig::text_default(), "alpha put_if_match beta");
        let mut previous = 0;
        for token in &tokens {
            assert!(token.position >= previous, "positions went backwards");
            previous = token.position;
        }
    }

    #[test]
    fn a_vocabulary_entry_stacks_its_canonical_term_at_the_run_start() {
        let mut vocabulary = Vocabulary::new();
        vocabulary
            .declare("put_if_match", &[&["put", "if", "match"][..]])
            .expect("valid declaration");
        let config = Profile::Code.config().with_vocabulary(vocabulary);
        let tokens = analyze_with(config, "put if match");
        assert!(terms_at(&tokens, 0).contains(&"put_if_match".to_owned()));
    }

    #[test]
    fn analysis_is_deterministic_across_two_runs_of_the_same_input() {
        let text = "Café put_if_match twenty five i-485 \u{4E2D}\u{6587}";
        let config = TokenizerConfig::text_default();
        let first = analyze_with(config.clone(), text);
        let second = analyze_with(config, text);
        assert_eq!(first, second);
    }

    #[test]
    fn empty_input_produces_no_tokens() {
        assert!(analyze_with(TokenizerConfig::text_default(), "").is_empty());
        assert!(analyze_with(TokenizerConfig::text_default(), "   ").is_empty());
    }

    #[test]
    fn offsets_slice_back_to_the_surface_form_for_parts() {
        let text = "put_if_match";
        let tokens = analyze_with(Profile::Code.config(), text);
        let part = tokens
            .iter()
            .find(|token| token.term == "match")
            .expect("the third part");
        assert_eq!(part.offset.slice(text), Some("match"));
    }

    #[test]
    fn disabling_decomposition_keeps_the_word_whole() {
        let mut config = Profile::Code.config();
        config.decompose_words = false;
        config.emit_catenation = false;
        let tokens = analyze_with(config, "put_if_match");
        assert_eq!(terms(&tokens), vec!["put_if_match"]);
    }
}
