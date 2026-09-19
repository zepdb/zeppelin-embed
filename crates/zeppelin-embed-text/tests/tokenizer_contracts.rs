#![allow(clippy::expect_used, clippy::indexing_slicing)]

use tempfile::tempdir;
use zeppelin_embed_text::bundle::{Bundle, BundleError};
use zeppelin_embed_text::runtime::RuntimeError;

#[path = "fixtures/ze115/reference.rs"]
#[allow(dead_code, clippy::excessive_precision)]
mod reference;

fn with_tokenizer(kind: u8, flags: u8, vocabulary: &[(&str, f32)], max_tokens: u32) -> Vec<u8> {
    let original = include_bytes!("fixtures/ze115/bert.zem");
    let start = reference::POOLING_OFFSET + 35;
    let mut end = start + 20;
    let count = u32::from_le_bytes(original[end..end + 4].try_into().expect("vocabulary count"));
    end += 4;
    for _ in 0..count {
        let width =
            u32::from_le_bytes(original[end..end + 4].try_into().expect("token length")) as usize;
        end += 8 + width;
    }
    let mut tokenizer = vec![kind, flags, 0, 0];
    for id in [0_u32, 1, 2, 3, vocabulary.len() as u32] {
        tokenizer.extend_from_slice(&id.to_le_bytes());
    }
    for (token, score) in vocabulary {
        tokenizer.extend_from_slice(&(token.len() as u32).to_le_bytes());
        tokenizer.extend_from_slice(token.as_bytes());
        tokenizer.extend_from_slice(&score.to_le_bytes());
    }
    let mut bytes = original[..start].to_vec();
    bytes.extend(tokenizer);
    bytes.extend_from_slice(&original[end..8192]);
    bytes.resize(8192, 0);
    bytes.extend_from_slice(&original[8192..]);
    // Empty prefix means max_tokens immediately precedes the two backend IDs.
    let max = reference::POOLING_OFFSET - 9;
    bytes[max..max + 4].copy_from_slice(&max_tokens.to_le_bytes());
    digest(&mut bytes);
    bytes
}

fn digest(bytes: &mut [u8]) {
    let end = bytes.len() - 16;
    let hash = xxhash_rust::xxh3::xxh3_128(&bytes[..end]);
    bytes[end..].copy_from_slice(&hash.to_le_bytes());
}

fn open(bytes: &[u8]) -> Result<Bundle, BundleError> {
    let directory = tempdir().expect("temporary bundle");
    let path = directory.path().join("tokenizer.zem");
    std::fs::write(&path, bytes).expect("write tokenizer fixture");
    Bundle::open(path)
}

#[test]
fn bundle_unigram_uses_global_scores_normalization_and_rectangular_masks() {
    let vocabulary = [
        ("<pad>", 0.),
        ("<unk>", 0.),
        ("<s>", 0.),
        ("</s>", 0.),
        ("▁", -1.),
        ("ab", -2.),
        ("a", -2.),
        ("b", -0.25),
        ("▁a", -0.25),
        ("▁ab", -10.),
        ("c", -1.),
    ];
    let bundle = open(&with_tokenizer(2, 3, &vocabulary, 16)).expect("unigram bundle");
    let batch = bundle
        .tokenize_queries(&["AB", "A\u{301}BC", "", "unknown"])
        .expect("scored paths");
    assert_eq!((batch.rows, batch.tokens_per_row), (4, 5));
    assert_eq!(
        batch.token_ids,
        [2, 8, 7, 3, 0, 2, 8, 7, 10, 3, 2, 3, 0, 0, 0, 2, 1, 3, 0, 0]
    );
    assert_eq!(
        batch.attention_mask,
        [
            1., 1., 1., 1., 0., 1., 1., 1., 1., 1., 1., 1., 0., 0., 0., 1., 1., 1., 0., 0.
        ]
    );
    let truncating = open(&with_tokenizer(2, 0, &vocabulary, 2)).expect("two-slot model");
    assert_eq!(
        truncating
            .tokenize_query("ab")
            .expect("truncate body")
            .token_ids,
        [2, 3]
    );
    let insufficient =
        open(&with_tokenizer(2, 0, &vocabulary, 1)).expect("positive persisted maximum");
    assert!(
        matches!(insufficient.tokenize_query("ab"), Err(RuntimeError::Tokenization(detail)) if detail.contains("two special-token slots"))
    );
    assert!(matches!(
        bundle.tokenize_queries(&[]),
        Err(RuntimeError::Tokenization(_))
    ));
}

#[test]
fn bundle_query_padding_preserves_special_tokens_and_refuses_truncation() {
    let vocabulary = [
        ("[PAD]", 0.),
        ("[UNK]", 0.),
        ("[CLS]", 0.),
        ("[SEP]", 0.),
        ("[MASK]", 0.),
        ("a", 0.),
        ("##b", 0.),
    ];
    let bundle = open(&with_tokenizer(1, 1, &vocabulary, 16)).expect("WordPiece bundle");
    let padded = bundle
        .tokenize_query_padded("AB[MASK]", 8)
        .expect("exact padding");
    assert_eq!(padded.token_ids, [2, 5, 6, 4, 3, 0, 0, 0]);
    assert_eq!(padded.attention_mask, [1., 1., 1., 1., 1., 0., 0., 0.]);
    assert!(matches!(
        bundle.tokenize_query_padded("AB[MASK]", 4),
        Err(RuntimeError::Shape(_))
    ));
    let mut bad_special = vocabulary;
    bad_special[0] = ("", 0.);
    assert!(
        matches!(open(&with_tokenizer(1, 0, &bad_special, 16)), Err(BundleError::Format(detail)) if detail == "tokenizer special token must not be empty")
    );
}
