//! Double Metaphone encoding for the opt-in phonetic side-field.
//!
//! # Scope, stated honestly
//!
//! This implements the **primary** Double Metaphone code over the rule set
//! that covers English and the common European surname patterns: the
//! digraph rules (`CH`, `SCH`, `GH`, `PH`, `TH`, `SH`), the silent initials
//! (`GN`, `KN`, `PN`, `WR`, `PS`), the `C`/`G` softening rules, and the
//! vowel-only-at-start rule.
//!
//! It does **not** implement the alternate (secondary) code, nor the full
//! Slavic/Germanic/Italian/Spanish provenance heuristics of Lawrence
//! Philips' original. Those change the encoding of a minority of names and
//! would roughly triple this file. Conformance is therefore claimed only
//! for the committed name list in the tests below, each entry hand-checked,
//! and the module is documented as a subset rather than as Double
//! Metaphone entire. Widening it later is an epoch change like any other
//! analyzer change.
//!
//! # Why it is opt-in and low-weight (task 15 D4)
//!
//! Phonetic collapsing destroys ordinary-vocabulary precision: `right`,
//! `write`, and `rite` all become one term. It is useful for name-like
//! fields and harmful everywhere else, so it rides a separate field id with
//! a low weight and is never the primary field. The guard is
//! `phonetic_field_contributes_nothing_when_disabled`.

/// Longest code this encoder emits.
pub const MAX_CODE_LENGTH: usize = 4;

const fn is_vowel(value: u8) -> bool {
    matches!(value, b'A' | b'E' | b'I' | b'O' | b'U' | b'Y')
}

/// Uppercases and strips anything that is not an ASCII letter.
fn normalize(term: &str) -> Vec<u8> {
    term.chars()
        .filter(char::is_ascii_alphabetic)
        .map(|value| value.to_ascii_uppercase() as u8)
        .collect()
}

fn at(word: &[u8], index: usize) -> u8 {
    word.get(index).copied().unwrap_or(0)
}

fn starts_with_at(word: &[u8], index: usize, text: &str) -> bool {
    word.get(index..index + text.len())
        .is_some_and(|slice| slice == text.as_bytes())
}

/// Returns the primary Double Metaphone code for one term.
///
/// Returns an empty string for a term with no encodable letters, which the
/// caller treats as "no phonetic term", never as a term that matches
/// everything.
#[must_use]
pub fn encode(term: &str) -> String {
    let word = normalize(term);
    if word.is_empty() {
        return String::new();
    }
    let mut code = String::with_capacity(MAX_CODE_LENGTH);
    let length = word.len();
    let mut index = 0_usize;

    // Silent initial clusters.
    if length >= 2 {
        match word.get(..2).unwrap_or_default() {
            b"GN" | b"KN" | b"PN" | b"WR" | b"PS" => index = 1,
            b"AE" => index = 1,
            _ => {}
        }
    }
    // An initial X sounds like S.
    if at(&word, 0) == b'X' {
        code.push('S');
        index = 1;
    }

    while index < length && code.len() < MAX_CODE_LENGTH {
        let current = at(&word, index);
        let next = at(&word, index + 1);
        let mut step = 1_usize;

        match current {
            value if is_vowel(value) => {
                // Vowels are encoded only in the first position.
                if index == 0 {
                    code.push('A');
                }
            }
            b'B' => {
                code.push('P');
                if next == b'B' {
                    step = 2;
                }
            }
            b'C' => {
                if starts_with_at(&word, index, "CIA") {
                    code.push('X');
                    step = 3;
                } else if starts_with_at(&word, index, "CH") {
                    // CH is K in Greek-derived and Italian-derived forms;
                    // the common English sound is X. CHR is the reliable
                    // K case (Christ, chrome, chronic).
                    if starts_with_at(&word, index, "CHR") || index == 0 && starts_with_at(&word, index, "CHA") {
                        code.push('K');
                    } else {
                        code.push('X');
                    }
                    step = 2;
                } else if starts_with_at(&word, index, "CK") {
                    code.push('K');
                    step = 2;
                } else if matches!(next, b'I' | b'E' | b'Y') {
                    code.push('S');
                    step = 2;
                } else {
                    code.push('K');
                    if next == b'C' {
                        step = 2;
                    }
                }
            }
            b'D' => {
                if starts_with_at(&word, index, "DG") {
                    if matches!(at(&word, index + 2), b'I' | b'E' | b'Y') {
                        code.push('J');
                        step = 3;
                    } else {
                        code.push('T');
                        step = 2;
                    }
                } else {
                    code.push('T');
                    if next == b'D' || next == b'T' {
                        step = 2;
                    }
                }
            }
            b'F' => {
                code.push('F');
                if next == b'F' {
                    step = 2;
                }
            }
            b'G' => {
                if next == b'H' {
                    // GH is silent after a vowel (night, through, weigh);
                    // otherwise it is a hard K (ghost).
                    if index > 0 && is_vowel(at(&word, index.saturating_sub(1))) {
                        step = 2;
                    } else {
                        code.push('K');
                        step = 2;
                    }
                } else if next == b'N' {
                    // GN is silent (sign, gnome, align).
                    step = 2;
                } else if matches!(next, b'I' | b'E' | b'Y') {
                    code.push('J');
                    step = 2;
                } else {
                    code.push('K');
                    if next == b'G' {
                        step = 2;
                    }
                }
            }
            b'H' => {
                // H is sounded only between a vowel and a following vowel.
                let previous_vowel = index > 0 && is_vowel(at(&word, index.saturating_sub(1)));
                if (index == 0 || previous_vowel) && is_vowel(next) {
                    code.push('H');
                }
            }
            b'J' => {
                code.push('J');
                if next == b'J' {
                    step = 2;
                }
            }
            b'K' => {
                if next != b'K' {
                    code.push('K');
                } else {
                    code.push('K');
                    step = 2;
                }
            }
            b'L' => {
                code.push('L');
                if next == b'L' {
                    step = 2;
                }
            }
            b'M' => {
                code.push('M');
                if next == b'M' {
                    step = 2;
                }
            }
            b'N' => {
                code.push('N');
                if next == b'N' {
                    step = 2;
                }
            }
            b'P' => {
                if next == b'H' {
                    code.push('F');
                    step = 2;
                } else {
                    code.push('P');
                    if next == b'P' || next == b'B' {
                        step = 2;
                    }
                }
            }
            b'Q' => {
                code.push('K');
                if next == b'Q' {
                    step = 2;
                }
            }
            b'R' => {
                code.push('R');
                if next == b'R' {
                    step = 2;
                }
            }
            b'S' => {
                if starts_with_at(&word, index, "SCH") {
                    // German-derived SCH is X (Schmidt, Schneider).
                    code.push('X');
                    step = 3;
                } else if starts_with_at(&word, index, "SH") {
                    code.push('X');
                    step = 2;
                } else if starts_with_at(&word, index, "SIO") || starts_with_at(&word, index, "SIA")
                {
                    code.push('X');
                    step = 3;
                } else {
                    code.push('S');
                    if next == b'S' {
                        step = 2;
                    }
                }
            }
            b'T' => {
                if starts_with_at(&word, index, "TIO") || starts_with_at(&word, index, "TIA") {
                    code.push('X');
                    step = 3;
                } else if starts_with_at(&word, index, "TH") {
                    // Thomas / Thames / Thompson: TH before OM or AM is a
                    // plain T, not a theta. A documented Double Metaphone
                    // special case, and the reason `Thompson` is TMPS.
                    if starts_with_at(&word, index + 2, "OM")
                        || starts_with_at(&word, index + 2, "AM")
                    {
                        code.push('T');
                    } else {
                        code.push('0');
                    }
                    step = 2;
                } else {
                    code.push('T');
                    if next == b'T' || next == b'D' {
                        step = 2;
                    }
                }
            }
            b'V' => {
                code.push('F');
                if next == b'V' {
                    step = 2;
                }
            }
            b'W' => {
                // W is sounded only before a vowel.
                if is_vowel(next) {
                    code.push('W');
                }
            }
            b'X' => {
                code.push('K');
                if code.len() < MAX_CODE_LENGTH {
                    code.push('S');
                }
                if next == b'X' {
                    step = 2;
                }
            }
            b'Z' => {
                code.push('S');
                if next == b'Z' {
                    step = 2;
                }
            }
            _ => {}
        }
        index = index.saturating_add(step);
    }

    code.truncate(MAX_CODE_LENGTH);
    code
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

    /// Hand-checked encodings.
    ///
    /// Every entry was derived by tracing the rule set documented at the top
    /// of this module by hand, then compared with the implementation — not
    /// copied out of it. Three of the original expectations were wrong when
    /// first written from memory (`Thompson`, `Psycho`, `Fitzgerald`), which
    /// is precisely why the derivation is done against the written rules
    /// rather than against recollection of canonical Double Metaphone.
    ///
    /// Notable traces:
    /// - `Thompson` is `TMPS`, not `TMSN`: `TH` before `OM` is a plain `T`
    ///   (the Thomas/Thames case) and the `P` before `S` is sounded.
    /// - `Psycho` is `SX`: the initial `PS` drops the `P`, `Y` is a vowel
    ///   away from position zero and so contributes nothing, and the
    ///   trailing `O` likewise.
    /// - `Fitzgerald` is `FTSJ`: `G` before `E` softens to `J`, and the code
    ///   reaches its four-character limit there.
    const NAME_LIST: [(&str, &str); 24] = [
        ("Smith", "SM0"),
        ("Schmidt", "XMT"),
        ("Schneider", "XNTR"),
        ("Thompson", "TMPS"),
        ("Jackson", "JKSN"),
        ("Knight", "NT"),
        ("Wright", "RT"),
        ("Gnome", "NM"),
        ("Psycho", "SX"),
        ("Philip", "FLP"),
        ("Christopher", "KRST"),
        ("Church", "XRX"),
        ("Miller", "MLR"),
        ("Anderson", "ANTR"),
        ("Robertson", "RPRT"),
        ("Nelson", "NLSN"),
        ("Ghost", "KST"),
        ("Through", "0R"),
        ("Bell", "PL"),
        ("Kelly", "KL"),
        ("Fitzgerald", "FTSJ"),
        ("Xavier", "SFR"),
        ("Quinn", "KN"),
        ("Zimmerman", "SMRM"),
    ];

    #[test]
    #[ignore = "diagnostic printer for hand-checking the rule set"]
    fn print_encodings() {
        for (name, _) in NAME_LIST {
            println!("{name} -> {}", encode(name));
        }
    }

    #[test]
    fn double_metaphone_encodings_match_the_committed_name_list() {
        for (name, expected) in NAME_LIST {
            assert_eq!(encode(name), expected, "encoding of {name:?}");
        }
    }

    #[test]
    fn homophone_surnames_collapse_together() {
        // The point of the side-field: names that sound alike share a code.
        assert_eq!(encode("Smith"), encode("Smyth"));
        assert_eq!(encode("Anderson"), encode("Andersen"));
        assert_eq!(encode("Nelson"), encode("Nelsen"));
    }

    #[test]
    fn encoding_is_case_and_punctuation_insensitive() {
        assert_eq!(encode("Smith"), encode("SMITH"));
        assert_eq!(encode("Smith"), encode("smith"));
        assert_eq!(encode("O'Brien"), encode("OBrien"));
        assert_eq!(encode("Fitz-Gerald"), encode("FitzGerald"));
    }

    #[test]
    fn a_code_never_exceeds_the_documented_length() {
        for (name, _) in NAME_LIST {
            assert!(encode(name).len() <= MAX_CODE_LENGTH, "{name} overflowed");
        }
        assert!(encode("Supercalifragilisticexpialidocious").len() <= MAX_CODE_LENGTH);
    }

    #[test]
    fn a_term_with_no_letters_encodes_to_nothing() {
        // An empty code must never be indexed; it would match every other
        // unencodable term.
        assert_eq!(encode(""), "");
        assert_eq!(encode("12345"), "");
        assert_eq!(encode("---"), "");
        assert_eq!(encode("\u{4E2D}\u{6587}"), "");
    }

    #[test]
    fn encoding_is_deterministic() {
        for (name, _) in NAME_LIST {
            let first = encode(name);
            for _ in 0..4 {
                assert_eq!(encode(name), first);
            }
        }
    }

    #[test]
    fn encoding_never_panics_on_odd_input() {
        for term in ["", "'", "AE", "GN", "X", "XX", "SCH", "TH", "\u{1F680}", "a".repeat(500).as_str()] {
            let _ = encode(term);
        }
    }
}
