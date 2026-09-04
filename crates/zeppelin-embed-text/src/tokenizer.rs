use std::collections::HashMap;

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
    scores: Vec<f32>,
    pad: u32,
    unk: u32,
    cls: u32,
    sep: u32,
    lowercase: bool,
    strip_accents: bool,
}

impl ModelTokenizer {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        kind: TokenizerKind,
        vocabulary: Vec<String>,
        scores: Vec<f32>,
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
        let token_ids = vocabulary
            .iter()
            .enumerate()
            .filter_map(|(id, token)| u32::try_from(id).ok().map(|id| (token.clone(), id)))
            .collect();
        Ok(Self {
            kind,
            token_ids,
            scores,
            pad,
            unk,
            cls,
            sep,
            lowercase,
            strip_accents,
        })
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
            let normalized = self.normalize(text);
            let mut tokens = match self.kind {
                TokenizerKind::WordPiece => self.wordpiece(&normalized),
                TokenizerKind::Unigram => self.unigram(&normalized),
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

    fn wordpiece(&self, text: &str) -> Vec<u32> {
        let mut output = Vec::new();
        for word in basic_tokens(text) {
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
        let sentence = text
            .split_whitespace()
            .map(|word| format!("▁{word}"))
            .collect::<String>();
        let boundaries = sentence
            .char_indices()
            .map(|(offset, _)| offset)
            .chain(std::iter::once(sentence.len()))
            .collect::<Vec<_>>();
        if boundaries.len() < 2 {
            return Vec::new();
        }
        let mut best = vec![f32::NEG_INFINITY; boundaries.len()];
        let mut previous = vec![None; boundaries.len()];
        if let Some(first) = best.first_mut() {
            *first = 0.0;
        }
        for start in 0..boundaries.len().saturating_sub(1) {
            let start_score = best.get(start).copied().unwrap_or(f32::NEG_INFINITY);
            if !start_score.is_finite() {
                continue;
            }
            for end in start.saturating_add(1)..boundaries.len() {
                let byte_start = boundaries.get(start).copied().unwrap_or(sentence.len());
                let byte_end = boundaries.get(end).copied().unwrap_or(sentence.len());
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
                    .unwrap_or(f32::NEG_INFINITY);
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
        reversed
    }
}

fn basic_tokens(text: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut start = None;
    for (offset, character) in text.char_indices() {
        if character.is_whitespace() || character.is_ascii_punctuation() {
            if let Some(begin) = start.take()
                && let Some(token) = text.get(begin..offset)
            {
                tokens.push(token);
            }
            if character.is_ascii_punctuation()
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
        scores: &[f32],
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
