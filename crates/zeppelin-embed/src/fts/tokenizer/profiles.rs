//! Named analysis profiles.
//!
//! A profile is a starting configuration, not a separate code path: every
//! profile runs the same pipeline with different knobs, and the profile id
//! is itself an epoch-digest input so two profiles never collide.

use super::stopwords::StopwordList;
use super::vocab::Vocabulary;
use super::{FoldingForm, Segmenter, Stemmer, TokenizerConfig};

/// A named starting configuration.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u16)]
pub enum Profile {
    /// General-purpose English text.
    ///
    /// Stemming and stopwords on, matching the Lucene `EnglishAnalyzer`
    /// shape that the task-13 BEIR targets were produced with.
    TextDefault = 1,
    /// Source code, identifiers, tickets, and SKUs.
    ///
    /// Stemming and stopwords off: stemming an identifier produces a term
    /// nobody queries, and `no`/`not`/`if` are meaningful in code.
    Code = 2,
    /// Spoken text and transcripts.
    ///
    /// Number-word normalization matters most here, because a transcript
    /// spells what a document digitizes.
    Voice = 3,
}

impl Profile {
    /// Returns the permanent numeric identifier.
    #[must_use]
    pub const fn id(self) -> u16 {
        self as u16
    }

    /// Returns the profile's starting configuration.
    #[must_use]
    pub fn config(self) -> TokenizerConfig {
        let (stopwords, stemmer, number_words) = match self {
            Self::TextDefault => (StopwordList::LuceneEnglish, Stemmer::EnglishPorter2, true),
            Self::Code => (StopwordList::None, Stemmer::None, false),
            Self::Voice => (StopwordList::LuceneEnglish, Stemmer::EnglishPorter2, true),
        };
        // Linguistic profiles drop one-character decomposition parts --
        // stop-level noise in prose. The code profile keeps them: the `x`
        // of `x_max` is a real search target in an identifier.
        let drop_single_char_parts = !matches!(self, Self::Code);
        TokenizerConfig {
            profile: self,
            segmenter: Segmenter::IdentifierPreserving,
            folding: FoldingForm::NfkcSearchFold,
            stopwords,
            stemmer,
            decompose_words: true,
            emit_catenation: true,
            number_words,
            drop_single_char_parts,
            vocabulary: Vocabulary::new(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn every_profile_has_a_distinct_epoch() {
        let epochs = [Profile::TextDefault, Profile::Code, Profile::Voice]
            .map(|profile| profile.config().epoch());
        assert_ne!(epochs[0], epochs[1]);
        assert_ne!(epochs[1], epochs[2]);
        assert_ne!(epochs[0], epochs[2]);
    }

    #[test]
    fn the_code_profile_neither_stems_nor_drops_stopwords() {
        let config = Profile::Code.config();
        assert_eq!(config.stemmer, Stemmer::None);
        assert_eq!(config.stopwords, StopwordList::None);
    }

    #[test]
    fn profile_ids_are_stable() {
        assert_eq!(Profile::TextDefault.id(), 1);
        assert_eq!(Profile::Code.id(), 2);
        assert_eq!(Profile::Voice.id(), 3);
    }
}
