//! The versioned analysis pipeline and its epoch identity.
//!
//! # The product feature
//!
//! Text goes in; tokens come out carrying a term, a position, and the byte
//! range they came from. The pipeline that produces them — segmentation,
//! folding, stopwords, stemming, number words, and a user vocabulary — is
//! identified by one digest, the **tokenizer epoch**. Same epoch, same bytes
//! in, same tokens out, forever.
//!
//! That identity is the point. Analyzer drift silently invalidating an index
//! is failure class U11 (`research/03:594`), and no off-the-shelf tokenizer
//! versions its own behaviour as index metadata (`research/05:202`). The
//! digest goes into `EpochMeta.tokenizer`, which task 07 already reserved,
//! so no format change is needed to carry it.
//!
//! # Query-time and index-time are one code path
//!
//! [`Analyzer::analyze`] is used for both. tantivy's missing search-analyzer
//! is a known gap this design avoids by construction. A query-side extras
//! hook is deliberately absent in v1 rather than stubbed.

pub(crate) mod fold;
mod fold_table;
pub(crate) mod numbers;
mod pipeline;
pub mod profiles;
#[cfg(test)]
mod properties;
pub(crate) mod segment;
pub(crate) mod stemmer;
pub mod stopwords;
pub mod vocab;

pub use profiles::Profile;
pub use stopwords::StopwordList;
pub use vocab::{MAX_SURFACE_TERMS, VocabError, Vocabulary};

/// Bit flags carried by every token.
///
/// Task 15 consumes these; defining the byte now avoids a format-shaped
/// change later (task 12 D5).
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TokenFlags(u8);

impl TokenFlags {
    /// The token must never be fuzzy-matched: an identifier, number, or SKU.
    ///
    /// A fuzzy match on `i-485` or a ticket id is almost always wrong.
    pub const NO_FUZZY: Self = Self(1);
    /// The token is a stacked variant, not a surface word.
    ///
    /// Variants come from the vocabulary, number normalization, or word
    /// decomposition. Diagnostics use this so a match can always explain
    /// which form produced it.
    pub const VARIANT: Self = Self(2);

    /// Returns the empty flag set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Returns the raw bits.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Returns true when every bit of `other` is set.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Returns the union of two flag sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// The byte range a token came from, into the analyzed text.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TokenOffset {
    /// Inclusive start byte offset.
    pub start: u32,
    /// Exclusive end byte offset.
    pub end: u32,
}

impl TokenOffset {
    /// Slices the token's surface form back out of the analyzed text.
    ///
    /// Returns `None` when the range is not a character boundary, which the
    /// pipeline never produces but a hand-built offset might.
    #[must_use]
    pub fn slice<'text>(&self, text: &'text str) -> Option<&'text str> {
        let start = usize::try_from(self.start).ok()?;
        let end = usize::try_from(self.end).ok()?;
        text.get(start..end)
    }
}

/// One analyzed token.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Token {
    /// The indexed term.
    pub term: String,
    /// The token position; stacked variants share a position.
    pub position: u32,
    /// The byte range the token came from.
    pub offset: TokenOffset,
    /// Token flags.
    pub flags: TokenFlags,
}

/// Word segmentation strategy; an epoch-digest input.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u16)]
pub enum Segmenter {
    /// Identifier-preserving segmentation with CJK bigram fallback.
    IdentifierPreserving = 1,
}

/// Character folding form; an epoch-digest input.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u16)]
pub enum FoldingForm {
    /// No folding at all; terms keep their original case and marks.
    None = 0,
    /// NFKC, case folding, and diacritic removal from the generated table.
    NfkcSearchFold = 1,
}

/// Stemming algorithm; an epoch-digest input.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u16)]
pub enum Stemmer {
    /// No stemming.
    None = 0,
    /// English Snowball (Porter2).
    EnglishPorter2 = 1,
}

/// Analysis rejection at the engine boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TokenizerError {
    /// The supplied bytes were not valid UTF-8.
    InvalidUtf8 {
        /// Byte offset of the first invalid sequence.
        offset: usize,
    },
    /// The analyzed text exceeded [`MAX_TEXT_BYTES`].
    TextTooLong {
        /// The rejected byte length.
        bytes: usize,
        /// The configured limit.
        limit: usize,
    },
    /// The vocabulary was rejected.
    Vocab(VocabError),
}

impl std::fmt::Display for TokenizerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUtf8 { offset } => {
                write!(formatter, "input is not valid UTF-8 at byte {offset}")
            }
            Self::TextTooLong { bytes, limit } => write!(
                formatter,
                "analyzed text is {bytes} bytes, over the {limit}-byte limit"
            ),
            Self::Vocab(error) => write!(formatter, "vocabulary rejected: {error}"),
        }
    }
}

impl std::error::Error for TokenizerError {}

impl From<VocabError> for TokenizerError {
    fn from(error: VocabError) -> Self {
        Self::Vocab(error)
    }
}

/// The largest text one call will analyze.
///
/// Offsets are `u32`, so this bound is what keeps every offset
/// representable. It is not a truncation limit: task 13 forbids truncating
/// documents before indexing, so an oversized field is a typed rejection the
/// caller must split, never a silent cut.
pub const MAX_TEXT_BYTES: usize = u32::MAX as usize;

/// The complete analysis configuration.
///
/// Every field is an epoch-digest input. Changing any one of them produces a
/// different epoch and therefore a different index meaning.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TokenizerConfig {
    /// The named profile this configuration started from.
    pub profile: Profile,
    /// Word segmentation strategy.
    pub segmenter: Segmenter,
    /// Character folding form.
    pub folding: FoldingForm,
    /// Stopword list.
    pub stopwords: StopwordList,
    /// Stemming algorithm.
    pub stemmer: Stemmer,
    /// Split words at joiners, case changes, and letter/digit transitions.
    pub decompose_words: bool,
    /// Emit the joiner-free catenation of a decomposed word.
    pub emit_catenation: bool,
    /// Emit digit and word variants for spelled numbers.
    pub number_words: bool,
    /// Drop single-LETTER decomposition parts.
    ///
    /// The `k` of `401k`, the `t` of `don't`, the `s` of `investor's`:
    /// one-letter parts of a decomposed word are stop-level noise in
    /// linguistic text — they carry no identity the whole word and the
    /// catenation do not already carry, and they pollute document length
    /// and document frequency. One-DIGIT parts are kept: `type-2` and
    /// `SARS-CoV-2` are discriminated by exactly that digit. Dropping a
    /// part leaves its position spent, exactly as a removed stopword
    /// does, so phrase adjacency and the analyzed length are unchanged.
    /// Identifier-heavy profiles keep every part: the `x` of `x_max` is
    /// a real search target in code.
    pub drop_single_char_parts: bool,
    /// User vocabulary and synonyms.
    pub vocabulary: Vocabulary,
}

impl TokenizerConfig {
    /// The general-purpose text profile.
    #[must_use]
    pub fn text_default() -> Self {
        Profile::TextDefault.config()
    }

    /// The identifier-heavy profile for source code and tickets.
    #[must_use]
    pub fn code() -> Self {
        Profile::Code.config()
    }

    /// The spoken-text profile.
    #[must_use]
    pub fn voice() -> Self {
        Profile::Voice.config()
    }

    /// Replaces the user vocabulary.
    #[must_use]
    pub fn with_vocabulary(mut self, vocabulary: Vocabulary) -> Self {
        self.vocabulary = vocabulary;
        self
    }

    /// Computes the epoch digest for this configuration.
    #[must_use]
    pub fn epoch(&self) -> TokenizerEpoch {
        TokenizerEpoch::of(self)
    }
}

/// A tokenizer epoch: the identity of one analysis behaviour.
///
/// Stored as a little-endian u64 in `EpochMeta.tokenizer`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TokenizerEpoch(u64);

/// Digest-input framing magic. Changing it changes every epoch.
const EPOCH_MAGIC: &[u8; 8] = b"ZETOKEP1";

impl TokenizerEpoch {
    pub(crate) const fn from_value(value: u64) -> Self {
        Self(value)
    }

    /// Computes the epoch of a configuration.
    ///
    /// # The canonical digest input
    ///
    /// Little-endian throughout, length-prefixed for every variable field,
    /// in exactly this order: magic, segmenter id, folding form id, folding
    /// table version, Unicode version string, stopword list id, stopword
    /// term count, stemmer id, the three boolean knobs, profile id, then the
    /// vocabulary. The vocabulary contributes its entry count followed by
    /// every (surface form, canonical term) pair in sorted order, so two
    /// vocabularies built by different insertion orders digest identically.
    ///
    /// This layout is a persisted-meaning contract frozen by
    /// `golden_streams_cannot_change_without_an_epoch_bump`.
    #[must_use]
    pub fn of(config: &TokenizerConfig) -> Self {
        let mut input: Vec<u8> = Vec::with_capacity(256);
        input.extend_from_slice(EPOCH_MAGIC);
        push_u16(&mut input, config.segmenter as u16);
        push_u16(&mut input, config.folding as u16);
        push_u16(&mut input, fold::FOLDING_TABLE_VERSION);
        push_str(&mut input, fold::FOLDING_UNICODE_VERSION);
        push_u16(&mut input, config.stopwords.id());
        push_u32(
            &mut input,
            u32::try_from(config.stopwords.terms().len()).unwrap_or(u32::MAX),
        );
        push_u16(&mut input, config.stemmer as u16);
        input.push(u8::from(config.decompose_words));
        input.push(u8::from(config.emit_catenation));
        input.push(u8::from(config.number_words));
        input.push(u8::from(config.drop_single_char_parts));
        push_u16(&mut input, config.profile.id());
        push_u32(
            &mut input,
            u32::try_from(config.vocabulary.len()).unwrap_or(u32::MAX),
        );
        for (surface, canonical) in config.vocabulary.entries() {
            push_u32(&mut input, u32::try_from(surface.len()).unwrap_or(u32::MAX));
            for term in surface {
                push_str(&mut input, term);
            }
            push_str(&mut input, canonical);
        }
        Self(xxhash_rust::xxh3::xxh3_64(&input))
    }

    /// Returns the raw digest.
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }

    /// Returns the lowercase hex form used in diagnostics.
    #[must_use]
    pub fn to_hex(self) -> String {
        format!("{:016x}", self.0)
    }
}

impl std::fmt::Display for TokenizerEpoch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:016x}", self.0)
    }
}

fn push_u16(buffer: &mut Vec<u8>, value: u16) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(buffer: &mut Vec<u8>, value: u32) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

fn push_str(buffer: &mut Vec<u8>, value: &str) {
    push_u32(buffer, u32::try_from(value.len()).unwrap_or(u32::MAX));
    buffer.extend_from_slice(value.as_bytes());
}

/// A compiled analyzer.
///
/// Construction validates the configuration once so [`Analyzer::analyze`]
/// cannot fail on well-formed UTF-8.
#[derive(Clone, Debug)]
pub struct Analyzer {
    config: TokenizerConfig,
    epoch: TokenizerEpoch,
}

impl Analyzer {
    /// Compiles a configuration.
    ///
    /// # Errors
    ///
    /// Returns [`TokenizerError::Vocab`] when the vocabulary is malformed.
    pub fn new(config: TokenizerConfig) -> Result<Self, TokenizerError> {
        let epoch = TokenizerEpoch::of(&config);
        Ok(Self { config, epoch })
    }

    /// Returns the epoch identifying this analyzer's behaviour.
    #[must_use]
    pub const fn epoch(&self) -> TokenizerEpoch {
        self.epoch
    }

    /// Returns the configuration.
    #[must_use]
    pub const fn config(&self) -> &TokenizerConfig {
        &self.config
    }

    /// Analyzes text into tokens.
    ///
    /// Index-time and query-time analysis are this same call.
    #[must_use]
    pub fn analyze(&self, text: &str) -> Vec<Token> {
        pipeline::analyze(&self.config, text)
    }

    /// Analyzes bytes, rejecting invalid UTF-8 with a typed error.
    ///
    /// # Errors
    ///
    /// Returns [`TokenizerError::InvalidUtf8`] rather than lossily
    /// converting, and [`TokenizerError::TextTooLong`] rather than
    /// truncating.
    pub fn analyze_bytes(&self, bytes: &[u8]) -> Result<Vec<Token>, TokenizerError> {
        if bytes.len() > MAX_TEXT_BYTES {
            return Err(TokenizerError::TextTooLong {
                bytes: bytes.len(),
                limit: MAX_TEXT_BYTES,
            });
        }
        let text = std::str::from_utf8(bytes).map_err(|error| TokenizerError::InvalidUtf8 {
            offset: error.valid_up_to(),
        })?;
        Ok(self.analyze(text))
    }
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

    fn analyzer() -> Analyzer {
        Analyzer::new(TokenizerConfig::text_default()).expect("valid config")
    }

    fn terms(tokens: &[Token]) -> Vec<&str> {
        tokens.iter().map(|token| token.term.as_str()).collect()
    }

    #[test]
    fn invalid_utf8_is_rejected_with_a_typed_error_at_the_boundary() {
        let error = analyzer()
            .analyze_bytes(&[b'o', b'k', 0xFF, 0xFE])
            .expect_err("invalid UTF-8 must be refused");
        assert_eq!(error, TokenizerError::InvalidUtf8 { offset: 2 });
    }

    #[test]
    fn valid_utf8_bytes_analyze_identically_to_the_string_path() {
        let text = "Café put_if_match";
        let analyzer = analyzer();
        assert_eq!(
            analyzer
                .analyze_bytes(text.as_bytes())
                .expect("valid UTF-8"),
            analyzer.analyze(text)
        );
    }

    #[test]
    fn changing_any_pipeline_knob_changes_the_epoch_digest() {
        let base = TokenizerConfig::text_default();
        let baseline = base.epoch();

        let mut changed = base.clone();
        changed.stemmer = Stemmer::None;
        assert_ne!(changed.epoch(), baseline, "stemmer must affect the epoch");

        let mut changed = base.clone();
        changed.stopwords = StopwordList::None;
        assert_ne!(changed.epoch(), baseline, "stopwords must affect the epoch");

        let mut changed = base.clone();
        changed.folding = FoldingForm::None;
        assert_ne!(changed.epoch(), baseline, "folding must affect the epoch");

        let mut changed = base.clone();
        changed.decompose_words = !changed.decompose_words;
        assert_ne!(changed.epoch(), baseline, "decomposition must affect it");

        let mut changed = base.clone();
        changed.emit_catenation = !changed.emit_catenation;
        assert_ne!(changed.epoch(), baseline, "catenation must affect it");

        let mut changed = base.clone();
        changed.number_words = !changed.number_words;
        assert_ne!(changed.epoch(), baseline, "number words must affect it");

        let mut changed = base;
        changed.profile = Profile::Code;
        assert_ne!(changed.epoch(), baseline, "profile must affect the epoch");
    }

    #[test]
    fn removing_a_vocab_entry_changes_the_epoch_digest() {
        let mut vocabulary = Vocabulary::new();
        vocabulary
            .declare("put_if_match", &[&["put", "if", "match"][..]])
            .expect("valid declaration");
        let with_entry = TokenizerConfig::text_default().with_vocabulary(vocabulary.clone());
        let populated = with_entry.epoch();

        vocabulary.remove("put_if_match");
        let without_entry = TokenizerConfig::text_default().with_vocabulary(vocabulary);
        assert_ne!(populated, without_entry.epoch());
        assert_eq!(
            without_entry.epoch(),
            TokenizerConfig::text_default().epoch()
        );
    }

    #[test]
    fn the_epoch_does_not_depend_on_vocabulary_insertion_order() {
        let mut first = Vocabulary::new();
        first
            .declare("alpha", &[&["a", "one"][..]])
            .expect("valid declaration");
        first
            .declare("beta", &[&["b", "two"][..]])
            .expect("valid declaration");

        let mut second = Vocabulary::new();
        second
            .declare("beta", &[&["b", "two"][..]])
            .expect("valid declaration");
        second
            .declare("alpha", &[&["a", "one"][..]])
            .expect("valid declaration");

        assert_eq!(
            TokenizerConfig::text_default()
                .with_vocabulary(first)
                .epoch(),
            TokenizerConfig::text_default()
                .with_vocabulary(second)
                .epoch()
        );
    }

    #[test]
    fn the_epoch_hex_form_is_stable_and_sixteen_digits() {
        let epoch = TokenizerConfig::text_default().epoch();
        let hex = epoch.to_hex();
        assert_eq!(hex.len(), 16);
        assert_eq!(hex, epoch.to_string());
        assert!(hex.chars().all(|value| value.is_ascii_hexdigit()));
    }

    #[test]
    fn adding_a_vocab_entry_unifies_all_three_surface_forms_at_one_position() {
        let mut vocabulary = Vocabulary::new();
        vocabulary
            .declare(
                "put_if_match",
                &[&["put", "if", "match"][..], &["putifmatch"][..]],
            )
            .expect("valid declaration");
        let analyzer = Analyzer::new(TokenizerConfig::text_default().with_vocabulary(vocabulary))
            .expect("valid config");

        for surface in ["put_if_match", "put if match", "putifmatch"] {
            let tokens = analyzer.analyze(surface);
            let canonical = tokens
                .iter()
                .find(|token| token.term == "put_if_match")
                .unwrap_or_else(|| panic!("{surface:?} did not emit the canonical term"));
            assert_eq!(
                canonical.position, 0,
                "{surface:?} emitted the canonical term off position zero"
            );
        }
    }

    #[test]
    fn identifier_and_number_tokens_carry_the_no_fuzzy_flag() {
        let tokens = analyzer().analyze("ticket i-485 filed 2026 by anup");
        for token in &tokens {
            let expected =
                token.term.contains('-') || token.term.chars().any(|value| value.is_ascii_digit());
            if expected {
                assert!(
                    token.flags.contains(TokenFlags::NO_FUZZY),
                    "{:?} must be flagged no_fuzzy",
                    token.term
                );
            }
        }
        assert!(tokens.iter().any(|token| token.term == "i-485"));
    }

    #[test]
    fn plain_words_are_not_flagged_no_fuzzy() {
        let tokens = analyzer().analyze("engine");
        assert_eq!(terms(&tokens), vec!["engin"]);
        assert!(
            !tokens
                .first()
                .expect("one token")
                .flags
                .contains(TokenFlags::NO_FUZZY)
        );
    }

    #[test]
    fn oversized_input_is_refused_rather_than_truncated() {
        // The limit is u32::MAX; constructing it is wasteful, so this only
        // pins that the typed error exists and names both numbers.
        let error = TokenizerError::TextTooLong {
            bytes: MAX_TEXT_BYTES + 1,
            limit: MAX_TEXT_BYTES,
        };
        assert!(error.to_string().contains("over the"));
    }

    #[test]
    fn token_offsets_slice_back_to_the_surface_form() {
        let text = "Café put_if_match";
        for token in analyzer().analyze(text) {
            assert!(
                token.offset.slice(text).is_some(),
                "offset for {:?} is not a character boundary",
                token.term
            );
        }
    }

    #[test]
    fn flag_algebra_is_consistent() {
        let both = TokenFlags::NO_FUZZY.union(TokenFlags::VARIANT);
        assert!(both.contains(TokenFlags::NO_FUZZY));
        assert!(both.contains(TokenFlags::VARIANT));
        assert!(!TokenFlags::NO_FUZZY.contains(TokenFlags::VARIANT));
        assert!(TokenFlags::empty().bits() == 0);
        assert!(both.contains(TokenFlags::empty()));
    }
}
