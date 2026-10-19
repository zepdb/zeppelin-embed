#![allow(clippy::expect_used)]

use zeppelin_embed_text::bundle::Bundle;
use zeppelin_embed_text::{ModelOpenOptions, ModelRef, resolve_model};

#[path = "goldens/clean71_release.rs"]
mod golden;

#[test]
#[ignore = "requires the pinned HF model in ZE_MODEL_CACHE or the default cache; no MLX"]
fn release_tokenizer_ids_match_hf_0_19_1_for_both_towers() {
    let path = resolve_model(&ModelRef::default_model(), &ModelOpenOptions::default())
        .expect("resolve published release model");
    let bundle = Bundle::open(path).expect("open published release tokenizers");
    for (role, digest, maximum, cases) in [
        (
            "query",
            golden::QUERY_SHA256,
            golden::QUERY_MAX_TOKENS,
            golden::QUERY_CASES,
        ),
        (
            "document",
            golden::DOCUMENT_SHA256,
            golden::DOCUMENT_MAX_TOKENS,
            golden::DOCUMENT_CASES,
        ),
    ] {
        let tower = if role == "query" {
            bundle.query_tower()
        } else {
            bundle.document_tower()
        };
        assert_eq!(tower.embedding.max_tokens as usize, maximum);
        for (name, text, repeat, expected) in cases {
            let input = text.repeat(*repeat);
            let batch = if role == "query" {
                bundle.tokenize_query(&input)
            } else {
                bundle.tokenize_document_chunks(&[input])
            }
            .expect("tokenize release case");
            assert_eq!(&batch.token_ids, expected, "{role}/{name}, source {digest}");
        }
    }
}
