use std::collections::HashMap;
use std::sync::Arc;
use unicode_categories::UnicodeCategories;
use unicode_normalization_alignments::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

use crate::runtime::RuntimeError;
use crate::tower::TokenBatch;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TokenizerKind {
    WordPiece,
    Unigram,
}

#[derive(Clone)]
pub(crate) struct ModelTokenizer {
    kind: TokenizerKind,
    token_ids: HashMap<String, u32>,
    scores: Vec<f64>,
    unknown_score: f64,
    max_token_bytes: usize,
    precompiled: Option<Arc<Precompiled>>,
    pad: u32,
    unk: u32,
    cls: u32,
    sep: u32,
    lowercase: bool,
    strip_accents: bool,
    special_tokens: Vec<(String, u32)>,
}

impl ModelTokenizer {
    pub(crate) const fn evaluation_revision(&self) -> &'static str {
        match self.kind {
            TokenizerKind::WordPiece => ";ze-text-wordpiece=2",
            TokenizerKind::Unigram => ";ze-text-unigram=2",
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        kind: TokenizerKind,
        vocabulary: Vec<String>,
        scores: Vec<f64>,
        pad: u32,
        unk: u32,
        cls: u32,
        sep: u32,
        lowercase: bool,
        strip_accents: bool,
    ) -> Result<Self, &'static str> {
        if vocabulary.is_empty() || scores.len() != vocabulary.len() {
            return Err("tokenizer vocabulary and scores must have equal non-zero length");
        }
        let count = vocabulary.len();
        for id in [pad, unk, cls, sep] {
            if id as usize >= count {
                return Err("tokenizer special id is outside the vocabulary");
            }
        }
        let mut special_tokens = Vec::new();
        if kind == TokenizerKind::WordPiece {
            for id in [pad, unk, cls, sep] {
                let token = vocabulary.get(id as usize).ok_or("missing special token")?;
                if token.is_empty() {
                    return Err("tokenizer special token must not be empty");
                }
                special_tokens.push((token.clone(), id));
            }
            if let Some(id) = vocabulary.iter().position(|token| token == "[MASK]") {
                let id = u32::try_from(id).map_err(|_| "mask token id exceeds u32")?;
                special_tokens.push(("[MASK]".to_owned(), id));
            }
        }
        let max_token_bytes = vocabulary.iter().map(String::len).max().unwrap_or(0);
        let token_ids = vocabulary
            .iter()
            .enumerate()
            .filter_map(|(id, token)| u32::try_from(id).ok().map(|id| (token.clone(), id)))
            .collect();
        Ok(Self {
            kind,
            token_ids,
            unknown_score: scores.iter().copied().fold(f64::INFINITY, f64::min) - 10.0,
            max_token_bytes,
            precompiled: None,
            scores,
            pad,
            unk,
            cls,
            sep,
            lowercase,
            strip_accents,
            special_tokens,
        })
    }

    pub(crate) fn with_precompiled(mut self, bytes: Vec<u8>) -> Result<Self, &'static str> {
        if bytes.is_empty() {
            return Ok(self);
        }
        if self.kind != TokenizerKind::Unigram {
            return Err("precompiled normalization requires Unigram");
        }
        let size = bytes
            .get(..4)
            .and_then(|b| b.try_into().ok())
            .map(u32::from_le_bytes)
            .ok_or("truncated normalizer")? as usize;
        if size == 0 || !size.is_multiple_of(4) {
            return Err("invalid normalizer trie size");
        }
        let end = size.checked_add(4).ok_or("normalizer trie size overflow")?;
        let trie = bytes
            .get(4..end)
            .ok_or("truncated normalizer trie")?
            .chunks_exact(4)
            .map(|b| b.try_into().map(u32::from_le_bytes))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "invalid trie word")?;
        let replacements = String::from_utf8(
            bytes
                .get(end..)
                .ok_or("missing normalizer strings")?
                .to_vec(),
        )
        .map_err(|_| "normalizer strings are not UTF8")?;
        self.special_tokens = self
            .token_ids
            .iter()
            .filter(|(token, id)| {
                [self.pad, self.unk, self.cls, self.sep].contains(id) || token.as_str() == "<mask>"
            })
            .map(|(token, id)| (token.clone(), *id))
            .collect();
        self.precompiled = Some(Arc::new(Precompiled { trie, replacements }));
        Ok(self)
    }

    pub(crate) fn encode_batch(
        &self,
        texts: &[String],
        max_tokens: usize,
    ) -> Result<TokenBatch, RuntimeError> {
        if texts.is_empty() || max_tokens < 2 {
            return Err(RuntimeError::Tokenization(
                "a token batch needs at least one row and two special-token slots".to_owned(),
            ));
        }
        let mut rows = Vec::with_capacity(texts.len());
        let body_limit = max_tokens.saturating_sub(2);
        for text in texts {
            let mut tokens = match self.kind {
                TokenizerKind::WordPiece => self.model_input(text),
                TokenizerKind::Unigram if self.precompiled.is_some() => self.model_input(text),
                TokenizerKind::Unigram => self.unigram(&self.normalize(text)),
            };
            tokens.truncate(body_limit);
            let mut row = Vec::with_capacity(tokens.len().saturating_add(2));
            row.push(self.cls);
            row.extend(tokens);
            row.push(self.sep);
            rows.push(row);
        }
        let width = rows.iter().map(Vec::len).max().unwrap_or(2);
        let capacity = texts
            .len()
            .checked_mul(width)
            .ok_or_else(|| RuntimeError::Tokenization("token batch size overflow".to_owned()))?;
        let mut ids = Vec::with_capacity(capacity);
        let mut mask = Vec::with_capacity(capacity);
        for mut row in rows {
            for id in &row {
                ids.push(
                    i32::try_from(*id).map_err(|_| {
                        RuntimeError::Tokenization("token id exceeds i32".to_owned())
                    })?,
                );
                mask.push(1.0);
            }
            while row.len() < width {
                row.push(self.pad);
                ids.push(i32::try_from(self.pad).map_err(|_| {
                    RuntimeError::Tokenization("padding id exceeds i32".to_owned())
                })?);
                mask.push(0.0);
            }
        }
        TokenBatch::new(ids, mask, texts.len(), width)
            .map_err(|detail| RuntimeError::Tokenization(detail.to_owned()))
    }

    fn normalize(&self, text: &str) -> String {
        if let Some(precompiled) = &self.precompiled {
            return precompiled.normalize(text);
        }
        if self.kind == TokenizerKind::WordPiece {
            let mut cleaned = String::with_capacity(text.len());
            for character in text.chars() {
                let whitespace_control = matches!(character, '\t' | '\n' | '\r');
                if matches!(character, '\0' | '\u{fffd}')
                    || (!whitespace_control && character.is_other())
                {
                    continue;
                }
                if character.is_whitespace() {
                    cleaned.push(' ');
                } else if is_bert_cjk(character) {
                    cleaned.push(' ');
                    cleaned.push(character);
                    cleaned.push(' ');
                } else {
                    cleaned.push(character);
                }
            }
            // The legacy exporter records the BERT uncased choice in bit 0.
            // Its source default also strips accents; bit 1 forces stripping
            // for a cased tokenizer. Version this corrected interpretation.
            let stripped = if self.strip_accents || self.lowercase {
                cleaned
                    .nfd()
                    .map(|(c, _)| c)
                    .filter(|c| !c.is_mark_nonspacing())
                    .collect()
            } else {
                cleaned
            };
            return if self.lowercase {
                stripped.chars().flat_map(char::to_lowercase).collect()
            } else {
                stripped
            };
        }
        let lowered = if self.lowercase {
            text.to_lowercase()
        } else {
            text.to_owned()
        };
        if !self.strip_accents {
            return lowered;
        }
        lowered
            .chars()
            .filter(|character| !is_combining_mark(*character))
            .collect()
    }

    fn model_piece(&self, text: &str) -> Vec<u32> {
        match self.kind {
            TokenizerKind::WordPiece => self.wordpiece(&self.normalize(text)),
            TokenizerKind::Unigram => self.unigram(&self.normalize(text)),
        }
    }

    fn model_input(&self, text: &str) -> Vec<u32> {
        let mut remaining = text;
        let mut output = Vec::new();
        // Added special tokens are matched on original text before normalizing.
        // Source BERT marks them non-normalized, including within another word.
        loop {
            let next = self
                .special_tokens
                .iter()
                .filter_map(|(token, id)| remaining.find(token).map(|offset| (offset, token, *id)))
                .min_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.len().cmp(&a.1.len())));
            let Some((offset, token, id)) = next else {
                output.extend(self.model_piece(remaining));
                break;
            };
            if let Some(prefix) = remaining.get(..offset) {
                output.extend(self.model_piece(prefix));
            }
            output.push(id);
            remaining = remaining
                .get(offset.saturating_add(token.len())..)
                .unwrap_or_default();
        }
        output
    }

    fn wordpiece(&self, text: &str) -> Vec<u32> {
        let mut output = Vec::new();
        for word in basic_tokens(text) {
            if word.chars().count() > 100 {
                output.push(self.unk);
                continue;
            }
            if let Some(id) = self.token_ids.get(word).copied() {
                output.push(id);
                continue;
            }
            let boundaries = word
                .char_indices()
                .map(|(offset, _)| offset)
                .chain(std::iter::once(word.len()))
                .collect::<Vec<_>>();
            let mut start = 0_usize;
            let mut pieces = Vec::new();
            let mut failed = false;
            while start + 1 < boundaries.len() {
                let byte_start = boundaries.get(start).copied().unwrap_or(word.len());
                let mut end = boundaries.len().saturating_sub(1);
                let mut found = None;
                while end > start {
                    let byte_end = boundaries.get(end).copied().unwrap_or(word.len());
                    if let Some(slice) = word.get(byte_start..byte_end) {
                        let candidate = if start == 0 {
                            slice.to_owned()
                        } else {
                            format!("##{slice}")
                        };
                        if let Some(id) = self.token_ids.get(&candidate).copied() {
                            found = Some((id, end));
                            break;
                        }
                    }
                    end = end.saturating_sub(1);
                }
                let Some((id, next)) = found else {
                    failed = true;
                    break;
                };
                pieces.push(id);
                start = next;
            }
            if failed {
                output.push(self.unk);
            } else {
                output.extend(pieces);
            }
        }
        output
    }

    fn unigram(&self, text: &str) -> Vec<u32> {
        if self.precompiled.is_none() {
            let sentence = text
                .split_whitespace()
                .map(|word| format!("▁{word}"))
                .collect::<String>();
            return self.unigram_word(&sentence);
        }
        text.split_whitespace()
            .flat_map(|word| word.strip_prefix('▁').unwrap_or(word).split('▁'))
            .flat_map(|piece| self.unigram_word(&format!("▁{piece}")))
            .collect()
    }

    fn unigram_word(&self, sentence: &str) -> Vec<u32> {
        let boundaries = sentence
            .char_indices()
            .map(|(offset, _)| offset)
            .chain(std::iter::once(sentence.len()))
            .collect::<Vec<_>>();
        if boundaries.len() < 2 {
            return Vec::new();
        }
        let mut best = vec![f64::NEG_INFINITY; boundaries.len()];
        let mut previous = vec![None; boundaries.len()];
        if let Some(first) = best.first_mut() {
            *first = 0.0;
        }
        for start in 0..boundaries.len().saturating_sub(1) {
            let start_score = best.get(start).copied().unwrap_or(f64::NEG_INFINITY);
            if !start_score.is_finite() {
                continue;
            }
            for end in start.saturating_add(1)..boundaries.len() {
                let byte_start = boundaries.get(start).copied().unwrap_or(sentence.len());
                let byte_end = boundaries.get(end).copied().unwrap_or(sentence.len());
                // Longer substrings cannot name a vocabulary token. Boundaries
                // grow in byte order, so later ends are impossible too.
                if byte_end.saturating_sub(byte_start) > self.max_token_bytes {
                    break;
                }
                let Some(piece) = sentence.get(byte_start..byte_end) else {
                    continue;
                };
                let Some(id) = self.token_ids.get(piece).copied() else {
                    continue;
                };
                let score = self
                    .scores
                    .get(id as usize)
                    .copied()
                    .unwrap_or(f64::NEG_INFINITY);
                let candidate = start_score + score;
                if best.get(end).is_some_and(|current| candidate > *current) {
                    if let Some(slot) = best.get_mut(end) {
                        *slot = candidate;
                    }
                    if let Some(slot) = previous.get_mut(end) {
                        *slot = Some((start, id));
                    }
                }
            }
            // SentencePiece permits one unknown Unicode scalar as a lattice
            // edge, using minimum vocabulary score minus ten.
            let end = start.saturating_add(1);
            let byte_start = boundaries.get(start).copied().unwrap_or(sentence.len());
            let byte_end = boundaries.get(end).copied().unwrap_or(sentence.len());
            if self.precompiled.is_some()
                && sentence
                    .get(byte_start..byte_end)
                    .is_some_and(|piece| !self.token_ids.contains_key(piece))
            {
                let candidate = start_score + self.unknown_score;
                if best.get(end).is_some_and(|current| candidate > *current) {
                    if let Some(slot) = best.get_mut(end) {
                        *slot = candidate;
                    }
                    if let Some(slot) = previous.get_mut(end) {
                        *slot = Some((start, self.unk));
                    }
                }
            }
        }
        let mut cursor = boundaries.len().saturating_sub(1);
        let mut reversed = Vec::new();
        while cursor > 0 {
            let Some((start, id)) = previous.get(cursor).copied().flatten() else {
                return vec![self.unk];
            };
            reversed.push(id);
            cursor = start;
        }
        reversed.reverse();
        if self.precompiled.is_some() {
            reversed.dedup_by(|a, b| *a == self.unk && *b == self.unk);
        }
        reversed
    }
}

#[derive(Clone)]
struct Precompiled {
    trie: Vec<u32>,
    replacements: String,
}

impl Precompiled {
    // SentencePiece stores a Darts double-array trie followed by NUL-terminated strings.
    fn transform(&self, text: &str) -> Option<&str> {
        let offset = |u: u32| ((u >> 10) << ((u & 512) >> 6)) as usize;
        let mut node = offset(*self.trie.first()?);
        for byte in text.bytes().take_while(|b| *b != 0) {
            node ^= byte as usize;
            let unit = *self.trie.get(node)?;
            if unit & 0x8000_00ff != u32::from(byte) {
                return None;
            }
            node ^= offset(unit);
            if unit & 256 != 0 {
                let begin = (*self.trie.get(node)? & 0x7fff_ffff) as usize;
                let tail = self.replacements.get(begin..)?;
                return tail.get(..tail.find('\0')?);
            }
        }
        None
    }
    fn normalize(&self, text: &str) -> String {
        let mut result = String::with_capacity(text.len());
        // HF tries short grapheme clusters first, then falls back to individual scalars.
        for cluster in text.graphemes(true) {
            if cluster.len() < 6
                && let Some(replacement) = self.transform(cluster)
            {
                result.push_str(replacement);
                continue;
            }
            for character in cluster.chars() {
                let mut utf8 = [0_u8; 4];
                let part = character.encode_utf8(&mut utf8);
                result.push_str(self.transform(part).unwrap_or(part));
            }
        }
        result
    }
}

fn basic_tokens(text: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut start = None;
    for (offset, character) in text.char_indices() {
        let punctuation = character.is_ascii_punctuation() || character.is_punctuation();
        if character.is_whitespace() || punctuation {
            if let Some(begin) = start.take()
                && let Some(token) = text.get(begin..offset)
            {
                tokens.push(token);
            }
            if punctuation
                && let Some(token) = text.get(offset..offset.saturating_add(character.len_utf8()))
            {
                tokens.push(token);
            }
        } else if start.is_none() {
            start = Some(offset);
        }
    }
    if let Some(begin) = start
        && let Some(token) = text.get(begin..)
    {
        tokens.push(token);
    }
    tokens
}

const fn is_bert_cjk(character: char) -> bool {
    matches!(character as u32,
        0x4e00..=0x9fff | 0x3400..=0x4dbf | 0x20000..=0x2a6df |
        0x2a700..=0x2b73f | 0x2b740..=0x2b81f | 0x2b920..=0x2ceaf |
        0xf900..=0xfaff | 0x2f800..=0x2fa1f)
}

const fn is_combining_mark(character: char) -> bool {
    matches!(character as u32, 0x0300..=0x036f | 0x1ab0..=0x1aff | 0x1dc0..=0x1dff)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn tokenizer(
        kind: TokenizerKind,
        vocabulary: &[&str],
        scores: &[f64],
        lowercase: bool,
        strip_accents: bool,
    ) -> ModelTokenizer {
        ModelTokenizer::new(
            kind,
            vocabulary.iter().map(|token| (*token).to_owned()).collect(),
            scores.to_vec(),
            0,
            1,
            2,
            3,
            lowercase,
            strip_accents,
        )
        .expect("valid tokenizer")
    }

    #[test]
    #[allow(clippy::indexing_slicing)]
    fn precompiled_replaces_graphemes_and_falls_back_to_scalars() {
        // Tiny Darts trie: a + combining acute -> X; b -> B.
        let mut trie = vec![0_u32; 901];
        trie[0] = 1 << 10;
        trie[96] = 97 | ((96 ^ 256) << 10);
        trie[460] = 204 | ((460 ^ 512) << 10);
        trie[641] = 129 | 256 | ((641 ^ 768) << 10);
        trie[768] = 0x8000_0000;
        trie[99] = 98 | 256 | ((99 ^ 900) << 10);
        trie[900] = 0x8000_0002;
        let mut bytes = ((trie.len() * 4) as u32).to_le_bytes().to_vec();
        bytes.extend(trie.iter().flat_map(|unit| unit.to_le_bytes()));
        bytes.extend_from_slice(b"X\0B\0");
        let model = tokenizer(
            TokenizerKind::Unigram,
            &["<pad>", "<unk>", "<s>", "</s>"],
            &[0.; 4],
            false,
            false,
        );
        assert_eq!(
            model
                .clone()
                .with_precompiled(bytes)
                .expect("charsmap")
                .normalize("a\u{301} b\u{301}\u{301}\u{301} z"),
            "X B\u{301}\u{301}\u{301} z"
        );
        assert_eq!(
            model
                .clone()
                .with_precompiled(Vec::new())
                .expect("empty charsmap")
                .normalize("a\u{301}"),
            "a\u{301}"
        );
        for malformed in [vec![1], vec![0; 4], vec![4, 0, 0, 0]] {
            assert!(model.clone().with_precompiled(malformed).is_err());
        }
    }

    #[test]
    fn unigram_f64_scores_preserve_a_one_point_path_advantage() {
        let model = tokenizer(
            TokenizerKind::Unigram,
            &["<pad>", "<unk>", "<s>", "</s>", "▁", "a", "▁a"],
            &[0., 0., 0., 0., -100_000_000., 1., -100_000_000.],
            false,
            false,
        );
        assert_eq!(
            model
                .encode_batch(&["a".to_owned()], 8)
                .expect("encode")
                .token_ids,
            [2, 4, 5, 3]
        );
    }

    #[test]
    fn wordpiece_normalizes_splits_unknowns_and_pads_rectangular_batches() {
        let vocabulary = [
            "[PAD]", "[UNK]", "[CLS]", "[SEP]", "br", "##onze", "!", "zeppelin",
        ];
        let model = tokenizer(TokenizerKind::WordPiece, &vocabulary, &[0.0; 8], true, true);
        let batch = model
            .encode_batch(&["BRONZE!".to_owned(), "missing".to_owned()], 8)
            .expect("wordpiece batch");
        assert_eq!(batch.rows, 2);
        assert_eq!(batch.tokens_per_row, 5);
        assert_eq!(batch.token_ids, [2, 4, 5, 6, 3, 2, 1, 3, 0, 0]);
        assert_eq!(
            batch.attention_mask,
            [1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 0.0, 0.0]
        );
        assert_eq!(model.normalize("E\u{301}"), "e");
    }

    #[test]
    fn unigram_chooses_the_highest_scoring_complete_segmentation_and_fails_unknown_wholes() {
        let vocabulary = [
            "<pad>",
            "<unk>",
            "<s>",
            "</s>",
            "▁bronze",
            "▁zeppelin",
            "▁bron",
            "ze",
        ];
        let model = tokenizer(
            TokenizerKind::Unigram,
            &vocabulary,
            &[0.0, -10.0, 0.0, 0.0, 5.0, 4.0, 1.0, 1.0],
            false,
            false,
        );
        let known = model
            .encode_batch(&["bronze zeppelin".to_owned()], 16)
            .expect("known unigram");
        assert_eq!(known.token_ids, [2, 4, 5, 3]);
        let unknown = model
            .encode_batch(&["unknown".to_owned()], 16)
            .expect("unknown unigram");
        assert_eq!(unknown.token_ids, [2, 1, 3]);
        let empty = model
            .encode_batch(&[String::new()], 16)
            .expect("empty text");
        assert_eq!(empty.token_ids, [2, 3]);
    }

    #[test]
    fn tokenizer_rejects_invalid_tables_and_batch_shapes() {
        assert!(
            ModelTokenizer::new(
                TokenizerKind::WordPiece,
                Vec::new(),
                Vec::new(),
                0,
                1,
                2,
                3,
                false,
                false,
            )
            .is_err()
        );
        assert!(
            ModelTokenizer::new(
                TokenizerKind::WordPiece,
                vec!["a".to_owned()],
                vec![0.0],
                0,
                1,
                2,
                3,
                false,
                false,
            )
            .is_err()
        );
        let model = tokenizer(
            TokenizerKind::WordPiece,
            &["[PAD]", "[UNK]", "[CLS]", "[SEP]"],
            &[0.0; 4],
            false,
            false,
        );
        assert!(model.encode_batch(&[], 8).is_err());
        assert!(model.encode_batch(&["text".to_owned()], 1).is_err());
    }
}
