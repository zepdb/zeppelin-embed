//! Task 12 conformance corpus: every fixture class and its frozen token stream.
//!
//! These goldens are the tokenizer epoch's meaning. Changing one without
//! bumping the epoch digest is a silent index invalidation (failure class
//! U11), so `golden_streams_cannot_change_without_an_epoch_bump` pins the
//! digest beside the streams.
#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};

use zeppelin_embed::fts::tokenizer::{Analyzer, Token, TokenizerConfig};

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tokenizer")
}

fn default_analyzer() -> Analyzer {
    Analyzer::new(TokenizerConfig::text_default()).expect("text-default config is valid")
}

/// Renders one token stream in the frozen golden format.
///
/// One token per line: `position`, `start`, `end`, `flags`, `term`, tab
/// separated. The term is last so a tab can never appear before it.
fn render(tokens: &[Token]) -> String {
    let mut out = String::new();
    for token in tokens {
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\n",
            token.position,
            token.offset.start,
            token.offset.end,
            token.flags.bits(),
            token.term
        ));
    }
    out
}

fn check_fixture(name: &str) {
    let dir = fixture_dir();
    let input_path = dir.join(format!("{name}.txt"));
    let golden_path = dir.join(format!("{name}.tokens"));
    let input = fs::read_to_string(&input_path)
        .unwrap_or_else(|error| panic!("missing fixture input {}: {error}", input_path.display()));
    let expected = fs::read_to_string(&golden_path)
        .unwrap_or_else(|error| panic!("missing golden {}: {error}", golden_path.display()));
    let actual = render(&default_analyzer().analyze(&input));
    assert_eq!(
        actual,
        expected,
        "token stream for fixture {name} drifted from its committed golden\n\
         --- actual ---\n{actual}--- expected ---\n{expected}"
    );
}

const FIXTURE_CLASSES: [&str; 10] = [
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
];

#[test]
fn every_fixture_produces_its_committed_token_stream() {
    for class in FIXTURE_CLASSES {
        check_fixture(class);
    }
}

#[test]
fn english_prose_fixture_matches_its_golden_stream() {
    check_fixture("english_prose");
}

#[test]
fn code_identifier_fixture_matches_its_golden_stream() {
    check_fixture("code_identifiers");
}

#[test]
fn ticket_and_sku_fixture_matches_its_golden_stream() {
    check_fixture("tickets_and_skus");
}

#[test]
fn email_and_url_fixture_matches_its_golden_stream() {
    check_fixture("emails_and_urls");
}

#[test]
fn spoken_number_fixture_matches_its_golden_stream() {
    check_fixture("spoken_numbers");
}

#[test]
fn code_switching_fixture_matches_its_golden_stream() {
    check_fixture("code_switching");
}

#[test]
fn cjk_fixture_matches_its_golden_stream() {
    check_fixture("cjk");
}

#[test]
fn apostrophe_and_hyphen_fixture_matches_its_golden_stream() {
    check_fixture("apostrophes_and_hyphens");
}

#[test]
fn diacritic_fixture_matches_its_golden_stream() {
    check_fixture("diacritics");
}

#[test]
fn emoji_fixture_matches_its_golden_stream() {
    check_fixture("emoji");
}

/// Writes the goldens from the current pipeline. Not a gate; a generator.
///
/// This is `#[ignore]`d because it asserts nothing — it exists so a
/// deliberate epoch bump can regenerate the corpus in one step. Every
/// regenerated stream must then be read and reviewed by a human before it is
/// committed, because these files are the epoch's meaning.
///
/// ```text
/// cargo test -p zeppelin-embed --test tokenizer_conformance \
///     regenerate_goldens -- --ignored
/// ```
#[test]
#[ignore = "generator, not a gate; run deliberately when bumping the epoch"]
fn regenerate_goldens() {
    let dir = fixture_dir();
    for class in FIXTURE_CLASSES {
        let input = fs::read_to_string(dir.join(format!("{class}.txt")))
            .unwrap_or_else(|error| panic!("missing fixture input {class}: {error}"));
        let rendered = render(&default_analyzer().analyze(&input));
        fs::write(dir.join(format!("{class}.tokens")), rendered)
            .unwrap_or_else(|error| panic!("cannot write golden {class}: {error}"));
    }
    fs::write(
        dir.join("epoch.digest"),
        format!("{}\n", default_analyzer().epoch().to_hex()),
    )
    .expect("cannot write the epoch digest");
}

#[test]
fn golden_streams_cannot_change_without_an_epoch_bump() {
    let committed = fs::read_to_string(fixture_dir().join("epoch.digest"))
        .expect("committed tokenizer epoch digest");
    let actual = default_analyzer().epoch().to_hex();
    assert_eq!(
        actual,
        committed.trim(),
        "the text-default tokenizer epoch changed; every committed golden \
         stream above is now a different epoch's meaning. Bump the epoch \
         deliberately (task 21 owns migration) rather than editing goldens."
    );
}
