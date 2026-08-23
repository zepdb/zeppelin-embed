//! Task 12 fuzz target: arbitrary bytes through the analysis pipeline.
//!
//! Two failure modes are hunted here. The obvious one is a panic: analysis
//! sits at the engine boundary and must return a typed error for invalid
//! UTF-8, never unwind. The subtler one is superlinear blowup — a vocabulary
//! scan or a decomposition rule that turns a pathological word into
//! quadratic work. The emission bound below is the guard: token count stays
//! within a fixed multiple of the input length, so a quadratic expansion
//! trips the assert rather than the clock.

#![no_main]

use libfuzzer_sys::fuzz_target;
use zeppelin_embed::fts::tokenizer::{Analyzer, Profile, TokenizerError, Vocabulary};

/// Tokens a single input byte may produce across every stacking stage.
///
/// A compound word emits its original, its catenation, and one term per
/// part, and each part can additionally carry a number variant. Eight per
/// input byte is far above anything the pipeline should reach and far below
/// quadratic.
const MAX_TOKENS_PER_BYTE: usize = 8;

fn check(analyzer: &Analyzer, data: &[u8]) {
    match analyzer.analyze_bytes(data) {
        Ok(tokens) => {
            let text = core::str::from_utf8(data).expect("analyze_bytes accepted these bytes");
            assert!(
                tokens.len() <= data.len().saturating_mul(MAX_TOKENS_PER_BYTE) + MAX_TOKENS_PER_BYTE,
                "token count {} is superlinear in {} input bytes",
                tokens.len(),
                data.len()
            );
            let mut previous_position = 0_u32;
            for token in &tokens {
                assert!(!token.term.is_empty(), "an empty term was emitted");
                assert!(
                    token.offset.start < token.offset.end,
                    "empty offset range for {:?}",
                    token.term
                );
                assert!(
                    token.offset.slice(text).is_some(),
                    "offset for {:?} is not on a character boundary",
                    token.term
                );
                assert!(
                    token.position >= previous_position,
                    "positions moved backwards"
                );
                previous_position = token.position;
            }
            // Analysis is deterministic: the same bytes give the same stream.
            assert_eq!(tokens, analyzer.analyze(text));
        }
        Err(TokenizerError::InvalidUtf8 { offset }) => {
            assert!(
                core::str::from_utf8(data).is_err(),
                "valid UTF-8 was rejected at byte {offset}"
            );
        }
        Err(other) => panic!("unexpected analysis error: {other}"),
    }
}

fuzz_target!(|data: &[u8]| {
    for profile in [Profile::TextDefault, Profile::Code, Profile::Voice] {
        let Ok(analyzer) = Analyzer::new(profile.config()) else {
            continue;
        };
        check(&analyzer, data);
    }

    // A vocabulary exercises the multi-position stacking scan, which is the
    // stage most likely to go superlinear.
    let mut vocabulary = Vocabulary::new();
    if vocabulary
        .declare("put_if_match", &[&["put", "if", "match"][..], &["putifmatch"][..]])
        .is_ok()
    {
        if let Ok(analyzer) =
            Analyzer::new(Profile::Code.config().with_vocabulary(vocabulary))
        {
            check(&analyzer, data);
        }
    }
});
