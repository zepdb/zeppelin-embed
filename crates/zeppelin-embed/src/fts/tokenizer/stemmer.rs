//! English Snowball (Porter2) stemming, implemented here rather than pulled in.
//!
//! # Provenance and licence
//!
//! The algorithm is the English stemmer published at
//! <https://snowballstem.org/algorithms/english/stemmer.html> by Martin
//! Porter, distributed under the BSD 3-Clause licence. This is a Rust
//! re-implementation from that published specification, not a translation of
//! any particular source file. Attribution is retained here per ADR-001.
//!
//! # Why hand-written
//!
//! `rust-stemmers` depends on `serde` unconditionally — `default-features =
//! false` does not remove it — and "Serde is absent from core production
//! dependencies" is a standing architecture invariant. See
//! `docs/adr/ADR-001-tokenizer.md` for the measured bake-off. Owning the
//! stemmer also makes it byte-for-byte pinnable, which is decision criterion
//! one: an upstream patch release can never silently change our token stream
//! and invalidate an index (failure class U11).
//!
//! Stemming is worth this effort: it is +1.3 nDCG@10 on average and +2.8 on
//! TREC-COVID (`research/02a:265`). It is the single largest analysis-side
//! lever on the task-13 BEIR gate.

/// Marker for a `y` acting as a consonant, per the published algorithm.
const CONSONANT_Y: char = 'Y';

const fn is_vowel(value: char) -> bool {
    matches!(value, 'a' | 'e' | 'i' | 'o' | 'u' | 'y')
}

/// Suffix pairs that count as a double for the step 1b undouble rule.
fn ends_with_double(word: &[char]) -> bool {
    let length = word.len();
    if length < 2 {
        return false;
    }
    let (Some(last), Some(previous)) = (word.get(length - 1), word.get(length - 2)) else {
        return false;
    };
    last == previous
        && matches!(
            last,
            'b' | 'd' | 'f' | 'g' | 'm' | 'n' | 'p' | 'r' | 't'
        )
}

/// Valid `li` endings from the published step 2 rule.
const fn is_li_ending(value: char) -> bool {
    matches!(
        value,
        'c' | 'd' | 'e' | 'g' | 'h' | 'k' | 'm' | 'n' | 'r' | 't'
    )
}

fn ends_with(word: &[char], suffix: &str) -> bool {
    let suffix_length = suffix.chars().count();
    if word.len() < suffix_length {
        return false;
    }
    word.iter()
        .skip(word.len() - suffix_length)
        .copied()
        .eq(suffix.chars())
}

/// Computes the R1 and R2 region starts, in character indices.
fn regions(word: &[char]) -> (usize, usize) {
    let length = word.len();
    let exceptional = ["gener", "commun", "arsen"]
        .into_iter()
        .find(|prefix| starts_with(word, prefix));
    let r1 = match exceptional {
        Some(prefix) => prefix.chars().count().min(length),
        None => region_after(word, 0),
    };
    let r2 = region_after(word, r1);
    (r1, r2)
}

fn starts_with(word: &[char], prefix: &str) -> bool {
    let prefix_length = prefix.chars().count();
    word.len() >= prefix_length && word.iter().take(prefix_length).copied().eq(prefix.chars())
}

/// Returns the index after the first non-vowel that follows a vowel.
fn region_after(word: &[char], from: usize) -> usize {
    let length = word.len();
    let mut index = from;
    while index < length {
        let Some(value) = word.get(index) else {
            return length;
        };
        if is_vowel(*value) {
            break;
        }
        index += 1;
    }
    while index < length {
        let Some(value) = word.get(index) else {
            return length;
        };
        if !is_vowel(*value) {
            return (index + 1).min(length);
        }
        index += 1;
    }
    length
}

/// True when the word ends in a short syllable, per the published rule.
fn ends_in_short_syllable(word: &[char]) -> bool {
    let length = word.len();
    if length == 2 {
        let (Some(first), Some(second)) = (word.first(), word.get(1)) else {
            return false;
        };
        return is_vowel(*first) && !is_vowel(*second);
    }
    if length < 3 {
        return false;
    }
    let (Some(third_last), Some(second_last), Some(last)) =
        (word.get(length - 3), word.get(length - 2), word.get(length - 1))
    else {
        return false;
    };
    !is_vowel(*third_last)
        && is_vowel(*second_last)
        && !is_vowel(*last)
        && !matches!(*last, 'w' | 'x' | CONSONANT_Y)
}

fn is_short_word(word: &[char], r1: usize) -> bool {
    r1 >= word.len() && ends_in_short_syllable(word)
}

/// Replaces the trailing `suffix_length` characters with `replacement`.
fn replace_suffix(word: &mut Vec<char>, suffix_length: usize, replacement: &str) {
    let keep = word.len().saturating_sub(suffix_length);
    word.truncate(keep);
    word.extend(replacement.chars());
}

/// Words the published algorithm exempts from the whole pipeline.
fn exceptional_form(word: &str) -> Option<&'static str> {
    let stem = match word {
        "skis" => "ski",
        "skies" => "sky",
        "dying" => "die",
        "lying" => "lie",
        "tying" => "tie",
        "idly" => "idl",
        "gently" => "gentl",
        "ugly" => "ugli",
        "early" => "earli",
        "only" => "onli",
        "singly" => "singl",
        "sky" | "news" | "howe" | "atlas" | "cosmos" | "bias" | "andes" => return Some(word_static(word)),
        _ => return None,
    };
    Some(stem)
}

/// Returns the `'static` spelling of an invariant exceptional word.
fn word_static(word: &str) -> &'static str {
    match word {
        "sky" => "sky",
        "news" => "news",
        "howe" => "howe",
        "atlas" => "atlas",
        "cosmos" => "cosmos",
        "bias" => "bias",
        _ => "andes",
    }
}

/// Words that are invariant once step 1a has run.
fn invariant_after_step_1a(word: &[char]) -> bool {
    const FORMS: [&str; 7] = [
        "inning", "outing", "canning", "herring", "earring", "proceed", "exceed",
    ];
    // `succeed` shares the `-ceed` shape and is listed with the others.
    FORMS.into_iter().any(|form| slice_equals(word, form)) || slice_equals(word, "succeed")
}

fn slice_equals(word: &[char], text: &str) -> bool {
    word.len() == text.chars().count() && word.iter().copied().eq(text.chars())
}

/// Stems one already-folded lowercase term.
///
/// The caller guarantees the input is lowercase and free of joiners; the
/// pipeline only stems all-alphabetic terms, because stemming an identifier
/// such as `put_if_match` produces a term nobody will ever query.
#[must_use]
pub(crate) fn stem(term: &str) -> String {
    if term.chars().count() <= 2 {
        return term.to_owned();
    }
    if let Some(exception) = exceptional_form(term) {
        return exception.to_owned();
    }

    let mut word: Vec<char> = term.chars().collect();
    // Preprocessing: drop a leading apostrophe, then mark consonantal y.
    if word.first() == Some(&'\'') {
        word.remove(0);
    }
    if word.first() == Some(&'y')
        && let Some(first) = word.first_mut()
    {
        *first = CONSONANT_Y;
    }
    for index in 1..word.len() {
        let previous_is_vowel = word.get(index - 1).copied().is_some_and(is_vowel);
        if previous_is_vowel
            && word.get(index) == Some(&'y')
            && let Some(slot) = word.get_mut(index)
        {
            *slot = CONSONANT_Y;
        }
    }

    let (r1, r2) = regions(&word);

    step_0(&mut word);
    step_1a(&mut word);
    if invariant_after_step_1a(&word) {
        return restore_y(&word);
    }
    step_1b(&mut word, r1);
    step_1c(&mut word);
    step_2(&mut word, r1);
    step_3(&mut word, r1, r2);
    step_4(&mut word, r2);
    step_5(&mut word, r1, r2);

    restore_y(&word)
}

fn restore_y(word: &[char]) -> String {
    word.iter()
        .map(|value| if *value == CONSONANT_Y { 'y' } else { *value })
        .collect()
}

fn step_0(word: &mut Vec<char>) {
    for suffix in ["'s'", "'s", "'"] {
        if ends_with(word, suffix) {
            let length = suffix.chars().count();
            word.truncate(word.len().saturating_sub(length));
            return;
        }
    }
}

fn step_1a(word: &mut Vec<char>) {
    if ends_with(word, "sses") {
        replace_suffix(word, 4, "ss");
        return;
    }
    if ends_with(word, "ied") || ends_with(word, "ies") {
        // More than one preceding letter keeps `i`; otherwise `ie`.
        let replacement = if word.len() > 4 { "i" } else { "ie" };
        replace_suffix(word, 3, replacement);
        return;
    }
    if ends_with(word, "us") || ends_with(word, "ss") {
        return;
    }
    if ends_with(word, "s") {
        let body = word.len().saturating_sub(1);
        let has_earlier_vowel = word
            .iter()
            .take(body.saturating_sub(1))
            .copied()
            .any(is_vowel);
        if has_earlier_vowel {
            word.truncate(body);
        }
    }
}

fn step_1b(word: &mut Vec<char>, r1: usize) {
    for suffix in ["eedly", "eed"] {
        if ends_with(word, suffix) {
            let length = suffix.chars().count();
            if word.len().saturating_sub(length) >= r1 {
                replace_suffix(word, length, "ee");
            }
            return;
        }
    }
    for suffix in ["ingly", "edly", "ing", "ed"] {
        if !ends_with(word, suffix) {
            continue;
        }
        let length = suffix.chars().count();
        let keep = word.len().saturating_sub(length);
        let preceding_has_vowel = word.iter().take(keep).copied().any(is_vowel);
        if !preceding_has_vowel {
            return;
        }
        word.truncate(keep);
        if ends_with(word, "at") || ends_with(word, "bl") || ends_with(word, "iz") {
            word.push('e');
        } else if ends_with_double(word) {
            word.truncate(word.len().saturating_sub(1));
        } else if is_short_word(word, r1) {
            word.push('e');
        }
        return;
    }
}

fn step_1c(word: &mut [char]) {
    let length = word.len();
    if length < 3 {
        return;
    }
    let Some(last) = word.last().copied() else {
        return;
    };
    if last != 'y' && last != CONSONANT_Y {
        return;
    }
    let Some(previous) = word.get(length - 2).copied() else {
        return;
    };
    if is_vowel(previous) {
        return;
    }
    if let Some(slot) = word.last_mut() {
        *slot = 'i';
    }
}

const STEP_2_RULES: [(&str, &str); 25] = [
    ("ational", "ate"),
    ("fulness", "ful"),
    ("iveness", "ive"),
    ("ousness", "ous"),
    ("ization", "ize"),
    ("tional", "tion"),
    ("biliti", "ble"),
    ("lessli", "less"),
    ("entli", "ent"),
    ("ation", "ate"),
    ("alism", "al"),
    ("aliti", "al"),
    ("ousli", "ous"),
    ("iviti", "ive"),
    ("fulli", "ful"),
    ("enci", "ence"),
    ("anci", "ance"),
    ("abli", "able"),
    ("izer", "ize"),
    ("ator", "ate"),
    ("alli", "al"),
    ("bli", "ble"),
    ("ogi", "og"),
    ("li", ""),
    ("", ""),
];

fn step_2(word: &mut Vec<char>, r1: usize) {
    for (suffix, replacement) in STEP_2_RULES {
        if suffix.is_empty() || !ends_with(word, suffix) {
            continue;
        }
        let length = suffix.chars().count();
        let keep = word.len().saturating_sub(length);
        if keep < r1 {
            return;
        }
        if suffix == "ogi" {
            if word.get(keep.wrapping_sub(1)) == Some(&'l') {
                replace_suffix(word, length, replacement);
            }
            return;
        }
        if suffix == "li" {
            let valid = word.get(keep.wrapping_sub(1)).copied().is_some_and(is_li_ending);
            if valid {
                word.truncate(keep);
            }
            return;
        }
        replace_suffix(word, length, replacement);
        return;
    }
}

const STEP_3_RULES: [(&str, &str); 7] = [
    ("ational", "ate"),
    ("tional", "tion"),
    ("alize", "al"),
    ("icate", "ic"),
    ("iciti", "ic"),
    ("ical", "ic"),
    ("ness", ""),
];

fn step_3(word: &mut Vec<char>, r1: usize, r2: usize) {
    for (suffix, replacement) in STEP_3_RULES {
        if !ends_with(word, suffix) {
            continue;
        }
        let length = suffix.chars().count();
        let keep = word.len().saturating_sub(length);
        if keep < r1 {
            return;
        }
        if replacement.is_empty() {
            word.truncate(keep);
        } else {
            replace_suffix(word, length, replacement);
        }
        return;
    }
    if ends_with(word, "ative") {
        let keep = word.len().saturating_sub(5);
        if keep >= r2 {
            word.truncate(keep);
        }
        return;
    }
    if ends_with(word, "ful") {
        let keep = word.len().saturating_sub(3);
        if keep >= r1 {
            word.truncate(keep);
        }
    }
}

const STEP_4_SUFFIXES: [&str; 18] = [
    "ement", "able", "ible", "ance", "ence", "ment", "ant", "ent", "ism", "ate", "iti", "ous",
    "ive", "ize", "al", "er", "ic", "ion",
];

fn step_4(word: &mut Vec<char>, r2: usize) {
    for suffix in STEP_4_SUFFIXES {
        if !ends_with(word, suffix) {
            continue;
        }
        let length = suffix.chars().count();
        let keep = word.len().saturating_sub(length);
        if keep < r2 {
            return;
        }
        if suffix == "ion" {
            let preceded = word.get(keep.wrapping_sub(1)).copied();
            if matches!(preceded, Some('s') | Some('t')) {
                word.truncate(keep);
            }
            return;
        }
        word.truncate(keep);
        return;
    }
}

fn step_5(word: &mut Vec<char>, r1: usize, r2: usize) {
    if word.last() == Some(&'e') {
        let keep = word.len().saturating_sub(1);
        if keep >= r2 {
            word.truncate(keep);
            return;
        }
        if keep >= r1 {
            let body: Vec<char> = word.iter().take(keep).copied().collect();
            if !ends_in_short_syllable(&body) {
                word.truncate(keep);
            }
        }
        return;
    }
    if word.last() == Some(&'l') {
        let keep = word.len().saturating_sub(1);
        if keep >= r2 && word.get(keep.wrapping_sub(1)) == Some(&'l') {
            word.truncate(keep);
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Vocabulary/output pairs taken from the published Porter2 sample set.
    const PUBLISHED_PAIRS: [(&str, &str); 40] = [
        ("consign", "consign"),
        ("consigned", "consign"),
        ("consigning", "consign"),
        ("consignment", "consign"),
        ("consist", "consist"),
        ("consisted", "consist"),
        ("consistency", "consist"),
        ("consistent", "consist"),
        ("consistently", "consist"),
        ("consisting", "consist"),
        ("consists", "consist"),
        ("consolation", "consol"),
        ("consolations", "consol"),
        ("consolatory", "consolatori"),
        ("console", "consol"),
        ("consoled", "consol"),
        ("consoles", "consol"),
        ("consolidate", "consolid"),
        ("consolidated", "consolid"),
        ("consolidating", "consolid"),
        ("consoling", "consol"),
        ("consols", "consol"),
        ("consonant", "conson"),
        ("consort", "consort"),
        ("consorted", "consort"),
        ("consorting", "consort"),
        ("conspicuous", "conspicu"),
        ("conspicuously", "conspicu"),
        ("conspiracy", "conspiraci"),
        ("conspirator", "conspir"),
        ("conspirators", "conspir"),
        ("conspire", "conspir"),
        ("conspired", "conspir"),
        ("conspiring", "conspir"),
        ("constable", "constabl"),
        ("constables", "constabl"),
        ("constance", "constanc"),
        ("constancy", "constanc"),
        ("constant", "constant"),
        ("knack", "knack"),
    ];

    #[test]
    fn published_vocabulary_pairs_stem_as_specified() {
        for (input, expected) in PUBLISHED_PAIRS {
            assert_eq!(stem(input), expected, "stem({input:?})");
        }
    }

    #[test]
    fn documented_algorithm_examples_hold() {
        // Step 1a
        assert_eq!(stem("ties"), "tie");
        assert_eq!(stem("cries"), "cri");
        assert_eq!(stem("gas"), "gas");
        assert_eq!(stem("gaps"), "gap");
        assert_eq!(stem("kiwis"), "kiwi");
        // Step 1b
        assert_eq!(stem("hopping"), "hop");
        assert_eq!(stem("hoping"), "hope");
        // Step 1c
        assert_eq!(stem("cry"), "cri");
        assert_eq!(stem("by"), "by");
        assert_eq!(stem("say"), "say");
    }

    #[test]
    fn exceptional_forms_are_honoured() {
        assert_eq!(stem("skies"), "sky");
        assert_eq!(stem("dying"), "die");
        assert_eq!(stem("news"), "news");
        assert_eq!(stem("inning"), "inning");
        assert_eq!(stem("succeed"), "succeed");
    }

    #[test]
    fn stemming_is_idempotent_on_its_own_output() {
        for (input, _) in PUBLISHED_PAIRS {
            let once = stem(input);
            assert_eq!(stem(&once), once, "stem is not stable on stem({input:?})");
        }
    }

    #[test]
    fn short_terms_pass_through_untouched() {
        for term in ["a", "of", "id", ""] {
            assert_eq!(stem(term), term);
        }
    }

    #[test]
    fn stemming_never_panics_on_odd_input() {
        for term in ["'", "''", "yyy", "aeiou", "\u{4E2D}\u{6587}", "e"] {
            let _ = stem(term);
        }
    }
}
