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

use std::cmp::Ordering;

use super::fold::{fold_controlled, segmentation_equivalent};
use super::numbers::{self, NumberWord};
use super::segment::{ideograph_bigrams_controlled, segment_controlled};
use super::stemmer;
use super::{FoldingForm, Stemmer, Token, TokenFlags, TokenOffset, TokenizerConfig};
use crate::fts::control::{BuildPolicy, GuardedString, GuardedVec, LegacyPolicy, sort_by_policy};

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
struct Emission<'m, C> {
    term: GuardedString<'m, C>,
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
    ordinal: u64,
}

pub(crate) struct ControlledToken<'m, C> {
    term: GuardedString<'m, C>,
    position: u32,
    offset: TokenOffset,
    flags: TokenFlags,
}

impl<C> ControlledToken<'_, C> {
    pub(crate) fn term(&self) -> &str {
        self.term.as_str()
    }
    pub(crate) const fn position(&self) -> u32 {
        self.position
    }
}

fn clamp_offset(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

const fn is_joiner(value: char) -> bool {
    matches!(value, '_' | '-' | '.' | '\'' | '+' | '#' | '@' | '&')
}

/// Applies the configured folding to a surface slice.
fn fold_term<'m, P: BuildPolicy<'m>>(
    config: &TokenizerConfig,
    surface: &str,
    policy: &mut P,
) -> Result<GuardedString<'m, P::Charge>, P::Error> {
    match config.folding {
        FoldingForm::None => GuardedString::copy_from(policy, surface),
        FoldingForm::NfkcSearchFold => fold_controlled(surface, policy),
    }
}

/// Splits a word's byte range into part ranges.
///
/// Splits happen at joiners, at lower-to-upper case transitions, and at
/// letter/digit transitions. Ranges are byte ranges into the original text.
fn split_parts<'m, P: BuildPolicy<'m>>(
    text: &str,
    start: usize,
    end: usize,
    policy: &mut P,
) -> Result<GuardedVec<'m, (usize, usize), P::Charge>, P::Error> {
    let Some(slice) = text.get(start..end) else {
        return GuardedVec::with_capacity(policy, 0);
    };
    let mut parts = GuardedVec::with_capacity(policy, 0)?;
    let mut part_start: Option<usize> = None;
    let mut previous: Option<char> = None;

    for (offset, raw) in slice.char_indices() {
        policy.step(1)?;
        let absolute = start + offset;
        let value = segmentation_equivalent(raw);
        if is_joiner(value) {
            if let Some(open) = part_start.take() {
                parts.push((open, absolute), policy)?;
            }
            previous = None;
            continue;
        }
        if let (Some(last), Some(open)) = (previous, part_start) {
            let case_break = last.is_lowercase() && value.is_uppercase();
            let digit_break = last.is_numeric() != value.is_numeric();
            if case_break || digit_break {
                parts.push((open, absolute), policy)?;
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
        parts.push((open, end), policy)?;
    }
    Ok(parts)
}

/// Returns true when the term is exactly one alphabetic character.
fn is_single_letter<'m, P: BuildPolicy<'m>>(term: &str, policy: &mut P) -> Result<bool, P::Error> {
    let mut chars = term.chars();
    policy.step(1)?;
    let first = chars.next();
    policy.step(1)?;
    let second = chars.next();
    Ok(matches!(
        (first, second),
        (Some(only), None) if only.is_alphabetic()
    ))
}

fn is_all_alphabetic<'m, P: BuildPolicy<'m>>(term: &str, policy: &mut P) -> Result<bool, P::Error> {
    if term.is_empty() {
        return Ok(false);
    }
    for value in term.chars() {
        policy.step(1)?;
        if !value.is_alphabetic() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Decides the `no_fuzzy` flag for a surface term.
///
/// A term is protected from fuzzy matching when it carries a digit or a
/// joiner. `C++`, `i-485`, and `GPT-5.6` are the cases that matter: a fuzzy
/// match on any of them is almost always wrong.
fn surface_flags<'m, P: BuildPolicy<'m>>(
    surface: &str,
    policy: &mut P,
) -> Result<TokenFlags, P::Error> {
    for value in surface.chars() {
        policy.step(1)?;
        let equivalent = segmentation_equivalent(value);
        if is_joiner(equivalent) || value.is_numeric() {
            return Ok(TokenFlags::NO_FUZZY);
        }
    }
    Ok(TokenFlags::empty())
}

/// Stage 1 and 2: segmentation and word decomposition.
fn emit_surface<'m, P: BuildPolicy<'m>>(
    config: &TokenizerConfig,
    text: &str,
    policy: &mut P,
) -> Result<(GuardedVec<'m, Emission<'m, P::Charge>, P::Charge>, u32), P::Error> {
    let mut emissions = GuardedVec::with_capacity(policy, 0)?;
    let mut position: u32 = 0;

    let spans = segment_controlled(text, policy)?;
    for span in spans.as_slice() {
        policy.step(1)?;
        if span.ideographic {
            let bigrams = ideograph_bigrams_controlled(text, *span, policy)?;
            for bigram in bigrams.as_slice() {
                policy.step(1)?;
                let Some(surface) = text.get(bigram.start..bigram.end) else {
                    continue;
                };
                let term = fold_term(config, surface, policy)?;
                if term.is_empty() {
                    continue;
                }
                let ordinal = u64::try_from(emissions.len()).unwrap_or(u64::MAX);
                emissions.push(
                    Emission {
                        term,
                        position,
                        start: clamp_offset(bigram.start),
                        end: clamp_offset(bigram.end),
                        // CJK bigrams are synthetic adjacency, never fuzzy targets.
                        flags: TokenFlags::NO_FUZZY,
                        rank: Rank::Surface,
                        compound: false,
                        ordinal,
                    },
                    policy,
                )?;
                position = position.saturating_add(1);
            }
            continue;
        }

        let Some(surface) = text.get(span.start..span.end) else {
            continue;
        };
        let whole = fold_term(config, surface, policy)?;
        let parts = if config.decompose_words {
            split_parts(text, span.start, span.end, policy)?
        } else {
            let mut parts = GuardedVec::with_capacity(policy, 1)?;
            parts.push((span.start, span.end), policy)?;
            parts
        };

        if parts.len() <= 1 {
            if !whole.is_empty() {
                let ordinal = u64::try_from(emissions.len()).unwrap_or(u64::MAX);
                emissions.push(
                    Emission {
                        term: whole,
                        position,
                        start: clamp_offset(span.start),
                        end: clamp_offset(span.end),
                        flags: surface_flags(surface, policy)?,
                        rank: Rank::Surface,
                        compound: false,
                        ordinal,
                    },
                    policy,
                )?;
            }
            position = position.saturating_add(1);
            continue;
        }

        // The undecomposed identifier, preserved.
        if !whole.is_empty() {
            let original = GuardedString::copy_from(policy, whole.as_str())?;
            let ordinal = u64::try_from(emissions.len()).unwrap_or(u64::MAX);
            emissions.push(
                Emission {
                    term: original,
                    position,
                    start: clamp_offset(span.start),
                    end: clamp_offset(span.end),
                    flags: TokenFlags::NO_FUZZY,
                    rank: Rank::Original,
                    compound: false,
                    ordinal,
                },
                policy,
            )?;
        }

        let mut catenation = GuardedString::with_capacity(policy, 0)?;
        for (part_start, part_end) in parts.as_slice() {
            policy.step(1)?;
            let Some(part_surface) = text.get(*part_start..*part_end) else {
                continue;
            };
            let folded = fold_term(config, part_surface, policy)?;
            catenation.push_str(policy, folded.as_str())?;
        }
        if config.emit_catenation
            && !catenation.is_empty()
            && compare_bytes(catenation.as_str(), whole.as_str(), policy)? != Ordering::Equal
        {
            let ordinal = u64::try_from(emissions.len()).unwrap_or(u64::MAX);
            emissions.push(
                Emission {
                    term: catenation,
                    position,
                    start: clamp_offset(span.start),
                    end: clamp_offset(span.end),
                    flags: TokenFlags::NO_FUZZY.union(TokenFlags::VARIANT),
                    rank: Rank::Catenation,
                    compound: false,
                    ordinal,
                },
                policy,
            )?;
        }

        for (index, (part_start, part_end)) in parts.as_slice().iter().enumerate() {
            policy.step(1)?;
            let Some(part_surface) = text.get(*part_start..*part_end) else {
                continue;
            };
            let term = fold_term(config, part_surface, policy)?;
            if term.is_empty() {
                continue;
            }
            // A one-LETTER part of a decomposed word is stop-level noise
            // in linguistic text; the whole word and the catenation still
            // carry its identity. A one-DIGIT part is kept: `type-2` and
            // `SARS-CoV-2` are discriminated by exactly that digit. The
            // dropped part's position stays spent, exactly as a removed
            // stopword's does.
            if config.drop_single_char_parts && is_single_letter(term.as_str(), policy)? {
                continue;
            }
            let offset = u32::try_from(index).unwrap_or(u32::MAX);
            let ordinal = u64::try_from(emissions.len()).unwrap_or(u64::MAX);
            emissions.push(
                Emission {
                    term,
                    position: position.saturating_add(offset),
                    start: clamp_offset(*part_start),
                    end: clamp_offset(*part_end),
                    flags: surface_flags(part_surface, policy)?,
                    rank: Rank::Surface,
                    compound: true,
                    ordinal,
                },
                policy,
            )?;
        }
        position = position.saturating_add(u32::try_from(parts.len()).unwrap_or(1));
    }

    Ok((emissions, position))
}

/// Returns the FIRST surface term at each position.
///
/// Both stacking stages only ever read a position's first surface
/// emission, so the table holds one index per position rather than a
/// heap-allocated bucket per position — the buckets were one `Vec` per
/// token of every analyzed document.
fn surface_by_position<'m, P: BuildPolicy<'m>>(
    emissions: &[Emission<'m, P::Charge>],
    positions: u32,
    policy: &mut P,
) -> Result<GuardedVec<'m, Option<usize>, P::Charge>, P::Error> {
    let count = usize::try_from(positions).unwrap_or(0);
    let mut table = GuardedVec::with_capacity(policy, count)?;
    for _ in 0..count {
        table.push(None, policy)?;
    }
    for (index, emission) in emissions.iter().enumerate() {
        policy.step(1)?;
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
    Ok(table)
}

/// Stage 3a: vocabulary stacking.
fn stack_vocabulary<'m, P: BuildPolicy<'m>>(
    config: &TokenizerConfig,
    emissions: &mut GuardedVec<'m, Emission<'m, P::Charge>, P::Charge>,
    positions: u32,
    policy: &mut P,
) -> Result<(), P::Error> {
    if config.vocabulary.is_empty() {
        return Ok(());
    }
    let table = surface_by_position(emissions.as_slice(), positions, policy)?;
    let longest = config.vocabulary.longest_surface_terms().max(1);
    let mut stacked = GuardedVec::with_capacity(policy, 0)?;

    for start in 0..table.len() {
        policy.step(1)?;
        let mut length = longest.min(table.len().saturating_sub(start));
        while length >= 1 {
            let mut span_start = u32::MAX;
            let mut span_end = 0_u32;
            let mut complete = true;
            for offset in 0..length {
                policy.step(1)?;
                let Some(first) = table
                    .get(start + offset)
                    .copied()
                    .flatten()
                    .and_then(|index| emissions.get(index))
                else {
                    complete = false;
                    break;
                };
                span_start = span_start.min(first.start);
                span_end = span_end.max(first.end);
            }
            let canonical = if complete {
                canonical_for_run(
                    config,
                    emissions.as_slice(),
                    table.as_slice(),
                    start,
                    length,
                    policy,
                )?
            } else {
                None
            };
            if let Some(canonical) = canonical {
                let position = u32::try_from(start).unwrap_or(u32::MAX);
                let term = GuardedString::copy_from(policy, canonical)?;
                let ordinal = u64::try_from(emissions.len().saturating_add(stacked.len()))
                    .unwrap_or(u64::MAX);
                stacked.push(
                    Emission {
                        term,
                        position,
                        start: span_start,
                        end: span_end,
                        flags: TokenFlags::NO_FUZZY.union(TokenFlags::VARIANT),
                        rank: Rank::Vocabulary,
                        compound: false,
                        ordinal,
                    },
                    policy,
                )?;
                break;
            }
            length -= 1;
        }
    }
    append_guarded(emissions, stacked, policy)?;
    Ok(())
}

fn canonical_for_run<'a, 'm, P: BuildPolicy<'m>>(
    config: &'a TokenizerConfig,
    emissions: &[Emission<'m, P::Charge>],
    table: &[Option<usize>],
    start: usize,
    length: usize,
    policy: &mut P,
) -> Result<Option<&'a str>, P::Error> {
    for (surface, canonical) in config.vocabulary.entries() {
        policy.step(1)?;
        if surface.len() != length {
            continue;
        }
        let mut matches = true;
        for offset in 0..length {
            policy.step(1)?;
            let Some(actual) = table
                .get(start.saturating_add(offset))
                .copied()
                .flatten()
                .and_then(|index| emissions.get(index))
            else {
                matches = false;
                break;
            };
            let Some(expected) = surface.get(offset) else {
                matches = false;
                break;
            };
            if compare_bytes(actual.term.as_str(), expected, policy)? != Ordering::Equal {
                matches = false;
                break;
            }
        }
        if matches {
            return Ok(Some(canonical.as_str()));
        }
    }
    Ok(None)
}

/// Stage 3b: number-word and digit variants.
///
/// Runs are consecutive positions only. A stopword inside a spelled number
/// splits the run, which is predictable and never composes two unrelated
/// numbers into one; the alternative — bridging position gaps — silently
/// invents values such as `eleven` from `five the six`.
fn stack_numbers<'m, P: BuildPolicy<'m>>(
    config: &TokenizerConfig,
    emissions: &mut GuardedVec<'m, Emission<'m, P::Charge>, P::Charge>,
    positions: u32,
    policy: &mut P,
) -> Result<(), P::Error> {
    if !config.number_words {
        return Ok(());
    }
    let table = surface_by_position(emissions.as_slice(), positions, policy)?;
    let mut stacked = GuardedVec::with_capacity(policy, 0)?;

    // Digit terms gain their spelled variant.
    for entry in table.as_slice() {
        policy.step(1)?;
        let Some(emission) = entry.and_then(|index| emissions.get(index)) else {
            continue;
        };
        if emission.compound {
            continue;
        }
        let Some(value) = numbers::parse_digits_controlled(emission.term.as_str(), policy)? else {
            continue;
        };
        let Some(spelled) = numbers::spell_joined_controlled(value, policy)? else {
            continue;
        };
        let ordinal =
            u64::try_from(emissions.len().saturating_add(stacked.len())).unwrap_or(u64::MAX);
        stacked.push(
            Emission {
                term: spelled,
                position: emission.position,
                start: emission.start,
                end: emission.end,
                flags: TokenFlags::NO_FUZZY.union(TokenFlags::VARIANT),
                rank: Rank::Number,
                compound: false,
                ordinal,
            },
            policy,
        )?;
    }

    // Spelled runs gain both the digit form and the joined word form.
    let mut start = 0_usize;
    while start < table.len() {
        let mut words = GuardedVec::with_capacity(policy, 0)?;
        let mut joined = GuardedString::with_capacity(policy, 0)?;
        let mut span_start = u32::MAX;
        let mut span_end = 0_u32;
        let mut end = start;
        while end < table.len() {
            policy.step(1)?;
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
            let Some(word) = numbers::classify(emission.term.as_str()) else {
                break;
            };
            words.push(word, policy)?;
            if !matches!(word, NumberWord::Connective) {
                joined.push_str(policy, emission.term.as_str())?;
            }
            span_start = span_start.min(emission.start);
            span_end = span_end.max(emission.end);
            end += 1;
        }
        if end > start {
            if let Some(value) = numbers::compose_controlled(words.as_slice(), policy)? {
                let position = u32::try_from(start).unwrap_or(u32::MAX);
                let digits = decimal_string(value, policy)?;
                for term in [Some(digits), (!joined.is_empty()).then_some(joined)]
                    .into_iter()
                    .flatten()
                {
                    let ordinal = u64::try_from(emissions.len().saturating_add(stacked.len()))
                        .unwrap_or(u64::MAX);
                    stacked.push(
                        Emission {
                            term,
                            position,
                            start: span_start,
                            end: span_end,
                            flags: TokenFlags::NO_FUZZY.union(TokenFlags::VARIANT),
                            rank: Rank::Number,
                            compound: false,
                            ordinal,
                        },
                        policy,
                    )?;
                }
            }
            start = end;
        } else {
            start += 1;
        }
    }
    append_guarded(emissions, stacked, policy)?;
    Ok(())
}

fn decimal_string<'m, P: BuildPolicy<'m>>(
    mut value: u64,
    policy: &mut P,
) -> Result<GuardedString<'m, P::Charge>, P::Error> {
    let mut digits = [0_u8; 20];
    let mut start = digits.len();
    loop {
        policy.step(1)?;
        start = start.saturating_sub(1);
        if let Some(slot) = digits.get_mut(start) {
            *slot = b'0'.saturating_add(u8::try_from(value % 10).unwrap_or(0));
        }
        value /= 10;
        if value == 0 {
            break;
        }
    }
    let mut output = GuardedString::with_capacity(policy, digits.len().saturating_sub(start))?;
    for byte in digits.iter().skip(start) {
        output.push_char(policy, char::from(*byte))?;
    }
    Ok(output)
}

/// Stage 4: stopword removal and stemming, applied to surface terms only.
fn filter_and_stem<'m, P: BuildPolicy<'m>>(
    config: &TokenizerConfig,
    emissions: GuardedVec<'m, Emission<'m, P::Charge>, P::Charge>,
    policy: &mut P,
) -> Result<GuardedVec<'m, Emission<'m, P::Charge>, P::Charge>, P::Error> {
    let capacity = emissions.len();
    let mut kept = GuardedVec::with_capacity(policy, capacity)?;
    let (values, charge) = emissions.into_parts();
    for mut emission in values {
        policy.step(1)?;
        if emission.rank == Rank::Surface {
            if is_stopword(config, emission.term.as_str(), policy)? {
                continue;
            }
            if config.stemmer == Stemmer::EnglishPorter2
                && is_all_alphabetic(emission.term.as_str(), policy)?
            {
                emission.term = stemmer::stem_controlled(emission.term.as_str(), policy)?;
            }
        }
        kept.push(emission, policy)?;
    }
    drop(charge);
    Ok(kept)
}

fn is_stopword<'m, P: BuildPolicy<'m>>(
    config: &TokenizerConfig,
    term: &str,
    policy: &mut P,
) -> Result<bool, P::Error> {
    for candidate in config.stopwords.terms() {
        policy.step(1)?;
        match compare_bytes(term, candidate, policy)? {
            Ordering::Equal => return Ok(true),
            Ordering::Less => return Ok(false),
            Ordering::Greater => {}
        }
    }
    Ok(false)
}

/// Runs the whole pipeline.
pub(crate) fn analyze(config: &TokenizerConfig, text: &str) -> Vec<Token> {
    let mut policy = LegacyPolicy;
    match analyze_with_policy(config, text, &mut policy) {
        Ok(tokens) => into_public(tokens),
        Err(never) => match never {},
    }
}

pub(crate) fn analyze_with_policy<'m, P: BuildPolicy<'m>>(
    config: &TokenizerConfig,
    text: &str,
    policy: &mut P,
) -> Result<GuardedVec<'m, ControlledToken<'m, P::Charge>, P::Charge>, P::Error> {
    if text.is_empty() {
        return GuardedVec::with_capacity(policy, 0);
    }
    policy.checkpoint()?;
    let (mut emissions, positions) = emit_surface(config, text, policy)?;
    stack_vocabulary(config, &mut emissions, positions, policy)?;
    stack_numbers(config, &mut emissions, positions, policy)?;
    let mut emissions = filter_and_stem(config, emissions, policy)?;
    sort_by_policy(emissions.as_mut_slice(), policy, |left, right, policy| {
        let prefix = left
            .position
            .cmp(&right.position)
            .then(left.rank.cmp(&right.rank));
        if prefix != Ordering::Equal {
            return Ok(prefix);
        }
        let term = compare_bytes(left.term.as_str(), right.term.as_str(), policy)?;
        Ok(term
            .then(left.start.cmp(&right.start))
            .then(left.ordinal.cmp(&right.ordinal)))
    })?;
    // One position must never carry the same term twice, whatever produced
    // it. A word whose catenation collapses onto one of its own parts —
    // `B#` plus a combining mark that folds away — otherwise emits `b` as
    // both a part and a catenation, and task 13 would read that duplicate
    // as a term frequency of two. Sorting put the lowest rank first, so the
    // surviving copy is the most surface-like one.
    let mut indices = GuardedVec::with_capacity(policy, emissions.len())?;
    let mut keep = GuardedVec::with_capacity(policy, emissions.len())?;
    for index in 0..emissions.len() {
        indices.push(index, policy)?;
        keep.push(false, policy)?;
    }
    sort_by_policy(indices.as_mut_slice(), policy, |left, right, policy| {
        let Some(a) = emissions.get(*left) else {
            return Ok(Ordering::Equal);
        };
        let Some(b) = emissions.get(*right) else {
            return Ok(Ordering::Equal);
        };
        let position = a.position.cmp(&b.position);
        if position != Ordering::Equal {
            return Ok(position);
        }
        Ok(compare_bytes(a.term.as_str(), b.term.as_str(), policy)?.then(left.cmp(right)))
    })?;
    let mut previous: Option<usize> = None;
    for index in indices.as_slice() {
        policy.step(1)?;
        let duplicate = if let Some(prior) = previous {
            match (emissions.get(prior), emissions.get(*index)) {
                (Some(left), Some(right)) if left.position == right.position => {
                    compare_bytes(left.term.as_str(), right.term.as_str(), policy)?
                        == Ordering::Equal
                }
                _ => false,
            }
        } else {
            false
        };
        if !duplicate {
            if let Some(slot) = keep.get_mut(*index) {
                *slot = true;
            }
            previous = Some(*index);
        }
    }
    let mut output_capacity = 0_usize;
    for value in keep.as_slice() {
        policy.step(1)?;
        if *value {
            output_capacity = output_capacity.saturating_add(1);
        }
    }
    let mut tokens = GuardedVec::with_capacity(policy, output_capacity)?;
    let (values, emissions_charge) = emissions.into_parts();
    for (index, emission) in values.into_iter().enumerate() {
        policy.step(1)?;
        if !keep.get(index).copied().unwrap_or(false) {
            continue;
        }
        tokens.push(
            ControlledToken {
                term: emission.term,
                position: emission.position,
                offset: TokenOffset {
                    start: emission.start,
                    end: emission.end,
                },
                flags: emission.flags,
            },
            policy,
        )?;
    }
    drop(emissions_charge);
    policy.checkpoint()?;
    Ok(tokens)
}

fn into_public(tokens: GuardedVec<'static, ControlledToken<'static, ()>, ()>) -> Vec<Token> {
    let (values, charge) = tokens.into_parts();
    let mut output = Vec::with_capacity(values.len());
    for token in values {
        let (term, term_charge) = token.term.into_parts();
        output.push(Token {
            term,
            position: token.position,
            offset: token.offset,
            flags: token.flags,
        });
        let _ = term_charge;
    }
    let _ = charge;
    output
}

fn append_guarded<'m, T, P: BuildPolicy<'m>>(
    output: &mut GuardedVec<'m, T, P::Charge>,
    appended: GuardedVec<'m, T, P::Charge>,
    policy: &mut P,
) -> Result<(), P::Error> {
    let (values, charge) = appended.into_parts();
    for value in values {
        output.push(value, policy)?;
    }
    drop(charge);
    Ok(())
}

fn compare_bytes<'m, P: BuildPolicy<'m>>(
    left: &str,
    right: &str,
    policy: &mut P,
) -> Result<Ordering, P::Error> {
    for (a, b) in left.as_bytes().iter().zip(right.as_bytes()) {
        policy.step(1)?;
        match a.cmp(b) {
            Ordering::Equal => {}
            other => return Ok(other),
        }
    }
    Ok(left.len().cmp(&right.len()))
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

    fn analyze_controlled(config: &TokenizerConfig, text: &str) -> Vec<Token> {
        let mut policy = LegacyPolicy;
        match analyze_with_policy(config, text, &mut policy) {
            Ok(tokens) => into_public(tokens),
            Err(never) => match never {},
        }
    }

    fn render(tokens: &[Token]) -> String {
        let mut rendered = String::new();
        for token in tokens {
            rendered.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\n",
                token.position,
                token.offset.start,
                token.offset.end,
                token.flags.bits(),
                token.term,
            ));
        }
        rendered
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

    #[test]
    fn controlled_pipeline_matches_all_frozen_streams_and_collision_cases() {
        let directory =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tokenizer");
        for name in [
            "english_prose",
            "code_identifiers",
            "tickets_and_skus",
            "emails_and_urls",
            "spoken_numbers",
            "code_switching",
            "cjk",
            "apostrophes_and_hyphens",
            "diacritics",
            "emoji",
        ] {
            let input = std::fs::read_to_string(directory.join(format!("{name}.txt")))
                .expect("fixture input");
            let expected = std::fs::read_to_string(directory.join(format!("{name}.tokens")))
                .expect("fixture golden");
            assert_eq!(
                render(&analyze_controlled(
                    &TokenizerConfig::text_default(),
                    &input
                )),
                expected
            );
        }

        let mut vocabulary = Vocabulary::new();
        vocabulary
            .declare("put_if_match", &[&["put", "if", "match"][..]])
            .expect("vocabulary");
        let configured = Profile::Code.config().with_vocabulary(vocabulary);
        let configured = analyze_controlled(&configured, "put if match");
        assert!(terms_at(&configured, 0).contains(&"put_if_match".to_owned()));

        let collision_config = Profile::Code.config();
        let expected_collision = analyze(&collision_config, "B#\u{0363}");
        let collision = analyze_controlled(&collision_config, "B#\u{0363}");
        assert_eq!(collision, expected_collision);
        let survivors = collision
            .iter()
            .filter(|token| token.term == "b")
            .collect::<Vec<_>>();
        assert_eq!(survivors.len(), 1);
        assert_eq!(survivors[0].position, 0);
        assert_eq!(survivors[0].offset, TokenOffset { start: 0, end: 1 });
        assert_eq!(survivors[0].flags, TokenFlags::empty());
    }
}
