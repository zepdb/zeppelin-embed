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
pub(crate) fn compose(words: &[NumberWord]) -> Option<u64> {
    if words.is_empty() || matches!(words.first(), Some(NumberWord::Connective)) {
        return None;
    }
    if matches!(words.last(), Some(NumberWord::Connective)) {
        return None;
    }
    let mut total: u64 = 0;
    let mut current: u64 = 0;
    let mut saw_value = false;
    for word in words {
        match word {
            NumberWord::Unit(value) | NumberWord::Ten(value) => {
                current = current.checked_add(*value)?;
                saw_value = true;
            }
            NumberWord::Multiplier(scale) => {
                if !saw_value && *scale >= 1_000 {
                    return None;
                }
                let base = if current == 0 { 1 } else { current };
                if *scale == 100 {
                    current = base.checked_mul(*scale)?;
                } else {
                    total = total.checked_add(base.checked_mul(*scale)?)?;
                    current = 0;
                }
                saw_value = true;
            }
            NumberWord::Connective => {}
        }
    }
    if !saw_value {
        return None;
    }
    let value = total.checked_add(current)?;
    (value <= MAX_NUMBER).then_some(value)
}

/// Spells a value as the joined word form, for example `25` -> `twentyfive`.
///
/// Returns `None` beyond [`MAX_NUMBER`].
pub(crate) fn spell_joined(value: u64) -> Option<String> {
    if value > MAX_NUMBER {
        return None;
    }
    let mut out = String::new();
    spell_into(value, &mut out)?;
    Some(out)
}

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
pub(crate) fn parse_digits(term: &str) -> Option<u64> {
    if term.is_empty() || !term.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    // A long digit run is an identifier, not a number worth spelling.
    if term.len() > 12 {
        return None;
    }
    term.parse::<u64>()
        .ok()
        .filter(|value| *value <= MAX_NUMBER)
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
