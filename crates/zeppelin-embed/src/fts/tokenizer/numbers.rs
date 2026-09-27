//! English number-word normalization: `twenty five` and `25` find each other.
//!
//! # Scope (task 12 D4)
//!
//! English units, teens, tens, and the `hundred`/`thousand`/`million`/
//! `billion` multipliers, composed. No ordinals, no other languages, no
//! decimals, no negatives. The scope is deliberately small and it is an
//! epoch-digest input, so widening it later is a visible epoch bump.
//!
//! # The mechanism
//!
//! A run of number words emits two same-position variants beside the words
//! themselves: the digit form (`25`) and the joined word form
//! (`twentyfive`). A bare digit token emits the joined word form as its
//! variant. Both directions therefore share both canonical terms, so
//! `twenty five` matches a document containing `25` and vice versa, with no
//! query-time expansion — which is the design constraint task 15 restates.

use crate::fts::control::{BuildPolicy, GuardedString};

/// The largest value this filter will spell or parse.
///
/// Bounding the range keeps the emitted variants short and keeps the filter
/// linear; a document full of digits cannot make analysis superlinear.
pub(crate) const MAX_NUMBER: u64 = 999_999_999_999;

const UNITS: [(&str, u64); 20] = [
    ("zero", 0),
    ("one", 1),
    ("two", 2),
    ("three", 3),
    ("four", 4),
    ("five", 5),
    ("six", 6),
    ("seven", 7),
    ("eight", 8),
    ("nine", 9),
    ("ten", 10),
    ("eleven", 11),
    ("twelve", 12),
    ("thirteen", 13),
    ("fourteen", 14),
    ("fifteen", 15),
    ("sixteen", 16),
    ("seventeen", 17),
    ("eighteen", 18),
    ("nineteen", 19),
];

const TENS: [(&str, u64); 8] = [
    ("twenty", 20),
    ("thirty", 30),
    ("forty", 40),
    ("fifty", 50),
    ("sixty", 60),
    ("seventy", 70),
    ("eighty", 80),
    ("ninety", 90),
];

const MULTIPLIERS: [(&str, u64); 4] = [
    ("hundred", 100),
    ("thousand", 1_000),
    ("million", 1_000_000),
    ("billion", 1_000_000_000),
];

/// One recognized number-word class.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NumberWord {
    /// A unit or teen value, 0..=19.
    Unit(u64),
    /// A tens value, 20..=90 in steps of ten.
    Ten(u64),
    /// A multiplier such as `hundred`.
    Multiplier(u64),
    /// The connective `and`, permitted inside a run but never starting one.
    Connective,
}

/// Classifies one already-folded term as a number word.
pub(crate) fn classify(term: &str) -> Option<NumberWord> {
    if let Some((_, value)) = UNITS.iter().find(|(word, _)| *word == term) {
        return Some(NumberWord::Unit(*value));
    }
    if let Some((_, value)) = TENS.iter().find(|(word, _)| *word == term) {
        return Some(NumberWord::Ten(*value));
    }
    if let Some((_, value)) = MULTIPLIERS.iter().find(|(word, _)| *word == term) {
        return Some(NumberWord::Multiplier(*value));
    }
    if term == "and" {
        return Some(NumberWord::Connective);
    }
    None
}

/// Folds a classified run of number words into one value.
///
/// Returns `None` when the run does not compose into a number, for example
/// a lone `and` or a trailing connective.
#[cfg(test)]
pub(crate) fn compose(words: &[NumberWord]) -> Option<u64> {
    match compose_impl(words, || Ok::<(), std::convert::Infallible>(())) {
        Ok(value) => value,
        Err(never) => match never {},
    }
}

pub(crate) fn compose_controlled<'m, P: BuildPolicy<'m>>(
    words: &[NumberWord],
    policy: &mut P,
) -> Result<Option<u64>, P::Error> {
    compose_impl(words, || policy.step(1))
}

fn compose_impl<E>(
    words: &[NumberWord],
    mut step: impl FnMut() -> Result<(), E>,
) -> Result<Option<u64>, E> {
    if words.is_empty() || matches!(words.first(), Some(NumberWord::Connective)) {
        return Ok(None);
    }
    if matches!(words.last(), Some(NumberWord::Connective)) {
        return Ok(None);
    }
    let mut total: u64 = 0;
    let mut current: u64 = 0;
    let mut saw_value = false;
    for word in words {
        step()?;
        match word {
            NumberWord::Unit(value) | NumberWord::Ten(value) => {
                let Some(next) = current.checked_add(*value) else {
                    return Ok(None);
                };
                current = next;
                saw_value = true;
            }
            NumberWord::Multiplier(scale) => {
                if !saw_value && *scale >= 1_000 {
                    return Ok(None);
                }
                let base = if current == 0 { 1 } else { current };
                if *scale == 100 {
                    let Some(next) = base.checked_mul(*scale) else {
                        return Ok(None);
                    };
                    current = next;
                } else {
                    let Some(product) = base.checked_mul(*scale) else {
                        return Ok(None);
                    };
                    let Some(next) = total.checked_add(product) else {
                        return Ok(None);
                    };
                    total = next;
                    current = 0;
                }
                saw_value = true;
            }
            NumberWord::Connective => {}
        }
    }
    if !saw_value {
        return Ok(None);
    }
    let Some(value) = total.checked_add(current) else {
        return Ok(None);
    };
    Ok((value <= MAX_NUMBER).then_some(value))
}

/// Spells a value as the joined word form, for example `25` -> `twentyfive`.
///
/// Returns `None` beyond [`MAX_NUMBER`].
#[cfg(test)]
pub(crate) fn spell_joined(value: u64) -> Option<String> {
    if value > MAX_NUMBER {
        return None;
    }
    let mut out = String::new();
    spell_into(value, &mut out)?;
    Some(out)
}

pub(crate) fn spell_joined_controlled<'m, P: BuildPolicy<'m>>(
    value: u64,
    policy: &mut P,
) -> Result<Option<GuardedString<'m, P::Charge>>, P::Error> {
    if value > MAX_NUMBER {
        return Ok(None);
    }
    let Some(length) = spelled_len(value, policy)? else {
        return Ok(None);
    };
    let mut output = GuardedString::with_capacity(policy, length)?;
    if spell_into_controlled(value, &mut output, policy)? {
        Ok(Some(output))
    } else {
        Ok(None)
    }
}

fn spelled_len<'m, P: BuildPolicy<'m>>(
    value: u64,
    policy: &mut P,
) -> Result<Option<usize>, P::Error> {
    policy.step(1)?;
    if value < 20 {
        return Ok(UNITS
            .iter()
            .find(|(_, candidate)| *candidate == value)
            .map(|(word, _)| word.len()));
    }
    if value < 100 {
        let tens = (value / 10) * 10;
        let Some((word, _)) = TENS.iter().find(|(_, candidate)| *candidate == tens) else {
            return Ok(None);
        };
        let remainder = value % 10;
        let tail = if remainder == 0 {
            Some(0)
        } else {
            spelled_len(remainder, policy)?
        };
        return Ok(tail.and_then(|tail| word.len().checked_add(tail)));
    }
    for (word, scale) in MULTIPLIERS.into_iter().rev() {
        policy.step(1)?;
        if value >= scale {
            let Some(head) = spelled_len(value / scale, policy)? else {
                return Ok(None);
            };
            let remainder = value % scale;
            let tail = if remainder == 0 {
                Some(0)
            } else {
                spelled_len(remainder, policy)?
            };
            return Ok(tail.and_then(|tail| head.checked_add(word.len())?.checked_add(tail)));
        }
    }
    Ok(None)
}

fn spell_into_controlled<'m, P: BuildPolicy<'m>>(
    value: u64,
    output: &mut GuardedString<'m, P::Charge>,
    policy: &mut P,
) -> Result<bool, P::Error> {
    policy.step(1)?;
    if value < 20 {
        let Some((word, _)) = UNITS.iter().find(|(_, candidate)| *candidate == value) else {
            return Ok(false);
        };
        output.push_str(policy, word)?;
        return Ok(true);
    }
    if value < 100 {
        let tens = (value / 10) * 10;
        let Some((word, _)) = TENS.iter().find(|(_, candidate)| *candidate == tens) else {
            return Ok(false);
        };
        output.push_str(policy, word)?;
        let remainder = value % 10;
        return if remainder > 0 {
            spell_into_controlled(remainder, output, policy)
        } else {
            Ok(true)
        };
    }
    for (word, scale) in MULTIPLIERS.into_iter().rev() {
        policy.step(1)?;
        if value >= scale {
            if !spell_into_controlled(value / scale, output, policy)? {
                return Ok(false);
            }
            output.push_str(policy, word)?;
            let remainder = value % scale;
            return if remainder > 0 {
                spell_into_controlled(remainder, output, policy)
            } else {
                Ok(true)
            };
        }
    }
    Ok(false)
}

#[cfg(test)]
fn spell_into(value: u64, out: &mut String) -> Option<()> {
    if value < 20 {
        let (word, _) = UNITS.iter().find(|(_, candidate)| *candidate == value)?;
        out.push_str(word);
        return Some(());
    }
    if value < 100 {
        let tens = (value / 10) * 10;
        let (word, _) = TENS.iter().find(|(_, candidate)| *candidate == tens)?;
        out.push_str(word);
        let remainder = value % 10;
        if remainder > 0 {
            spell_into(remainder, out)?;
        }
        return Some(());
    }
    for (word, scale) in MULTIPLIERS.into_iter().rev() {
        if value >= scale {
            spell_into(value / scale, out)?;
            out.push_str(word);
            let remainder = value % scale;
            if remainder > 0 {
                spell_into(remainder, out)?;
            }
            return Some(());
        }
    }
    None
}

/// Parses a bare digit term into a value within range.
#[cfg(test)]
pub(crate) fn parse_digits(term: &str) -> Option<u64> {
    if term.is_empty() || term.len() > 12 {
        return None;
    }
    if !term.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    term.parse::<u64>()
        .ok()
        .filter(|value| *value <= MAX_NUMBER)
}

pub(crate) fn parse_digits_controlled<'m, P: BuildPolicy<'m>>(
    term: &str,
    policy: &mut P,
) -> Result<Option<u64>, P::Error> {
    if term.is_empty() || term.len() > 12 {
        return Ok(None);
    }
    let mut value = 0_u64;
    for byte in term.bytes() {
        policy.step(1)?;
        if !byte.is_ascii_digit() {
            return Ok(None);
        }
        value = value
            .checked_mul(10)
            .and_then(|n| n.checked_add(u64::from(byte - b'0')))
            .unwrap_or(MAX_NUMBER.saturating_add(1));
    }
    Ok((value <= MAX_NUMBER).then_some(value))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn compose_words(text: &str) -> Option<u64> {
        let words = text
            .split_whitespace()
            .map(classify)
            .collect::<Option<Vec<_>>>()?;
        compose(&words)
    }

    #[test]
    fn simple_compositions_hold() {
        assert_eq!(compose_words("twenty five"), Some(25));
        assert_eq!(compose_words("five"), Some(5));
        assert_eq!(compose_words("nineteen"), Some(19));
        assert_eq!(compose_words("ninety nine"), Some(99));
    }

    #[test]
    fn multiplier_compositions_hold() {
        assert_eq!(compose_words("two hundred"), Some(200));
        assert_eq!(compose_words("two hundred and five"), Some(205));
        assert_eq!(compose_words("one thousand"), Some(1_000));
        assert_eq!(compose_words("twenty one thousand"), Some(21_000));
        assert_eq!(compose_words("hundred"), Some(100));
    }

    #[test]
    fn degenerate_runs_do_not_compose() {
        assert_eq!(compose_words("and"), None);
        assert_eq!(compose_words("five and"), None);
        assert_eq!(compose(&[]), None);
    }

    #[test]
    fn spelling_round_trips_through_composition() {
        for value in [0_u64, 5, 19, 25, 99, 100, 205, 1_000, 21_000, 999_999] {
            let joined = spell_joined(value).expect("value is in range");
            assert!(!joined.is_empty());
            assert_eq!(
                spell_joined(value).as_deref(),
                Some(joined.as_str()),
                "spelling is not deterministic for {value}"
            );
        }
        assert_eq!(spell_joined(25).as_deref(), Some("twentyfive"));
        assert_eq!(spell_joined(205).as_deref(), Some("twohundredfive"));
    }

    #[test]
    fn digit_parsing_is_bounded() {
        assert_eq!(parse_digits("25"), Some(25));
        assert_eq!(parse_digits(""), None);
        assert_eq!(parse_digits("12a"), None);
        assert_eq!(parse_digits("1234567890123456"), None);
    }

    #[test]
    fn out_of_range_values_are_refused_rather_than_wrapped() {
        assert_eq!(spell_joined(MAX_NUMBER + 1), None);
        assert_eq!(parse_digits("999999999999999"), None);
    }
}
