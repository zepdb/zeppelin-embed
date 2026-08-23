//! Algebraic properties of the analysis pipeline.
//!
//! # The corpus matters more than the property
//!
//! An ASCII-only generator is the classic false green here: every offset
//! bug, every folding expansion that changes byte length, and every
//! combining-mark case lives outside ASCII. The generator below draws from
//! the same classes as the conformance corpus — identifiers, tickets,
//! diacritics, decomposed marks, CJK, kana, emoji, and code-switched text —
//! so a passing run means something.

#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use proptest::prelude::*;
use proptest::test_runner::{Config as ProptestConfig, RngSeed};

use super::fold::fold;
use super::{Analyzer, FoldingForm, Profile, Stemmer, Token, TokenizerConfig};

/// Fragments drawn from every fixture class, plus adversarial punctuation.
fn fragment() -> impl Strategy<Value = String> {
    prop_oneof![
        // Plain words and stopwords.
        prop::sample::select(vec![
            "the",
            "quick",
            "brown",
            "foxes",
            "jumping",
            "consistently",
            "a",
            "of",
            "and",
            "it",
        ])
        .prop_map(str::to_owned),
        // Identifiers, tickets, and languages with trailing symbols.
        prop::sample::select(vec![
            "put_if_match",
            "i-485",
            "GPT-5.6",
            "KV-cache",
            "C++",
            "F#",
            "getUserID42",
            "state-of-the-art",
            "anup@example.com",
            "ABC-1234",
        ])
        .prop_map(str::to_owned),
        // Diacritics, precomposed and decomposed.
        prop::sample::select(vec![
            "Caf\u{00E9}",
            "cafe\u{0301}",
            "na\u{00EF}ve",
            "stra\u{00DF}e",
            "\u{00C5}ngstr\u{00F6}m",
            "r\u{00E9}sum\u{00E9}",
            "\u{0301}",
        ])
        .prop_map(str::to_owned),
        // Compatibility forms and fullwidth.
        prop::sample::select(vec![
            "\u{FB01}nd",
            "\u{FF21}\u{FF22}",
            "x\u{00B2}",
            "\u{2168}",
        ])
        .prop_map(str::to_owned),
        // CJK, kana with dakuten, and Hangul.
        prop::sample::select(vec![
            "\u{4E2D}\u{6587}\u{5206}\u{8BCD}",
            "\u{3072}\u{3089}\u{304C}\u{306A}",
            "\u{30AB}\u{30BF}",
            "\u{D55C}\u{AD6D}",
        ])
        .prop_map(str::to_owned),
        // Emoji and symbols, which index to nothing but must not break offsets.
        prop::sample::select(vec!["\u{1F680}", "\u{2705}", "\u{1F9EA}", "\u{2014}"])
            .prop_map(str::to_owned),
        // Apostrophes, straight and curly.
        prop::sample::select(vec!["don't", "don\u{2019}t", "it\u{2019}s", "'"])
            .prop_map(str::to_owned),
        // Spelled and digit numbers.
        prop::sample::select(vec!["twenty", "five", "hundred", "25", "0", "999999999999"])
            .prop_map(str::to_owned),
        // Code-switched text.
        prop::sample::select(vec!["mujhe", "chahiye", "deploy", "karo"]).prop_map(str::to_owned),
        // Free-form text, which reaches codepoints the lists above miss.
        "\\PC{0,12}",
    ]
}

/// Joins fragments with realistic separators.
fn corpus() -> impl Strategy<Value = String> {
    prop::collection::vec(
        (
            fragment(),
            prop::sample::select(vec![" ", "  ", ", ", ". ", "\n", ""]),
        ),
        0..12,
    )
    .prop_map(|parts| {
        let mut text = String::new();
        for (fragment, separator) in parts {
            text.push_str(&fragment);
            text.push_str(separator);
        }
        text
    })
}

fn analyzer(config: TokenizerConfig) -> Analyzer {
    Analyzer::new(config).expect("configuration is valid")
}

/// A configuration with every rewriting stage disabled.
///
/// Under it each token is exactly one folded surface span, which is what
/// makes the strict offset property below checkable.
fn surface_only() -> TokenizerConfig {
    let mut config = Profile::Code.config();
    config.stemmer = Stemmer::None;
    config.decompose_words = false;
    config.emit_catenation = false;
    config.number_words = false;
    config
}

fn assert_offsets_are_sound(tokens: &[Token], text: &str) {
    for token in tokens {
        assert!(
            token.offset.start < token.offset.end,
            "empty offset range for {:?}",
            token.term
        );
        assert!(
            usize::try_from(token.offset.end).unwrap_or(usize::MAX) <= text.len(),
            "offset for {:?} escapes the input",
            token.term
        );
        assert!(
            token.offset.slice(text).is_some(),
            "offset for {:?} is not on a character boundary",
            token.term
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        rng_seed: RngSeed::Fixed(0x7ec4_0c12_0001),
        cases: 512,
        ..ProptestConfig::default()
    })]

    /// Plan 12 test 3.
    #[test]
    fn offsets_are_in_bounds_monotonic_and_slice_back_to_the_surface_form(text in corpus()) {
        let tokens = analyzer(TokenizerConfig::text_default()).analyze(&text);
        assert_offsets_are_sound(&tokens, &text);

        let mut previous_position = 0_u32;
        for token in &tokens {
            prop_assert!(
                token.position >= previous_position,
                "positions moved backwards at {:?}",
                token.term
            );
            previous_position = token.position;
        }
    }

    /// Plan 12 test 3, strict form: with no rewriting, the term IS the fold
    /// of the slice. This is the property that would catch an offset that
    /// points at the wrong bytes but happens to stay in bounds.
    #[test]
    fn a_surface_token_is_exactly_the_fold_of_the_bytes_it_points_at(text in corpus()) {
        let tokens = analyzer(surface_only()).analyze(&text);
        for token in &tokens {
            let Some(slice) = token.offset.slice(&text) else {
                prop_assert!(false, "offset for {:?} is not sliceable", token.term);
                continue;
            };
            prop_assert_eq!(
                fold(slice),
                token.term.clone(),
                "token does not match the fold of its own bytes {:?}",
                slice
            );
        }
    }

    /// Plan 12 test 4.
    #[test]
    fn positions_strictly_increase_except_within_a_synonym_stack(text in corpus()) {
        let tokens = analyzer(TokenizerConfig::text_default()).analyze(&text);
        let mut seen: Vec<(u32, String)> = Vec::new();
        for token in &tokens {
            let key = (token.position, token.term.clone());
            prop_assert!(
                !seen.contains(&key),
                "term {:?} was emitted twice at position {}",
                token.term,
                token.position
            );
            seen.push(key);
        }
        for pair in tokens.windows(2) {
            if let (Some(left), Some(right)) = (pair.first(), pair.get(1)) {
                prop_assert!(right.position >= left.position);
            }
        }
    }

    /// Plan 12 test 5.
    #[test]
    fn analysis_is_deterministic_across_two_runs_of_the_same_input(text in corpus()) {
        let analyzer = analyzer(TokenizerConfig::text_default());
        prop_assert_eq!(analyzer.analyze(&text), analyzer.analyze(&text));
    }

    /// Two analyzers built from equal configurations agree, so nothing in
    /// the pipeline depends on construction order or address identity.
    #[test]
    fn two_analyzers_with_the_same_config_agree(text in corpus()) {
        let first = analyzer(TokenizerConfig::text_default());
        let second = analyzer(TokenizerConfig::text_default());
        prop_assert_eq!(first.epoch(), second.epoch());
        prop_assert_eq!(first.analyze(&text), second.analyze(&text));
    }

    /// Plan 12 R6: idempotence holds for the normalization stage, and is
    /// explicitly NOT claimed for stemmed terms, because a stem is not a
    /// surface form and re-analyzing it is undefined.
    #[test]
    fn folding_is_idempotent_where_it_is_defined(text in corpus()) {
        let once = fold(&text);
        prop_assert_eq!(fold(&once), once);
    }

    /// Analysis never panics, whatever the bytes say.
    #[test]
    fn analysis_never_panics_on_arbitrary_text(text in "\\PC{0,64}") {
        let tokens = analyzer(TokenizerConfig::text_default()).analyze(&text);
        assert_offsets_are_sound(&tokens, &text);
    }

    /// Every profile analyzes every corpus, and none emits an empty term.
    #[test]
    fn no_profile_emits_an_empty_term(text in corpus()) {
        for profile in [Profile::TextDefault, Profile::Code, Profile::Voice] {
            for token in analyzer(profile.config()).analyze(&text) {
                prop_assert!(!token.term.is_empty(), "a profile emitted an empty term");
            }
        }
    }

    /// Folding rewrites terms but never moves a token onto other bytes.
    ///
    /// It may DROP a token: a word made only of combining marks folds to the
    /// empty string and is not indexed. So the folded stream is a
    /// subsequence of the unfolded one by `(position, offset)`, not an
    /// element-wise match — asserting equal lengths was the stronger and
    /// wrong claim.
    #[test]
    fn folding_rewrites_terms_without_moving_or_inventing_offsets(text in corpus()) {
        let mut unfolded = surface_only();
        unfolded.folding = FoldingForm::None;
        let folded_tokens = analyzer(surface_only()).analyze(&text);
        let plain_tokens = analyzer(unfolded).analyze(&text);
        prop_assert!(folded_tokens.len() <= plain_tokens.len());

        let plain_sites: Vec<(u32, super::TokenOffset)> = plain_tokens
            .iter()
            .map(|token| (token.position, token.offset))
            .collect();
        let mut cursor = 0_usize;
        for folded in &folded_tokens {
            let site = (folded.position, folded.offset);
            let found = plain_sites
                .iter()
                .skip(cursor)
                .position(|candidate| *candidate == site);
            let Some(offset) = found else {
                prop_assert!(false, "folding invented a token site {site:?}");
                continue;
            };
            cursor = cursor.saturating_add(offset).saturating_add(1);
        }
    }
}
