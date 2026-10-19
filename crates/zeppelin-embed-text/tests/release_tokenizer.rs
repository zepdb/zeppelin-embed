#![allow(clippy::expect_used)]

use std::path::Path;
use std::process::Command;
use tempfile::tempdir;
use zeppelin_embed_text::bundle::Bundle;

#[path = "goldens/clean71_release.rs"]
mod golden;

#[test]
#[ignore = "needs ZE_CLEAN71_PACK with the release pair's tokenizer.json files"]
fn release_tokenizer_ids_match_hf_0_19_1_for_both_towers() {
    let pack = std::env::var_os("ZE_CLEAN71_PACK").expect("ZE_CLEAN71_PACK must be set");
    let directory = tempdir().expect("fixture directory");
    let bundle_path = directory.path().join("release-tokenizers.zem");
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/ze-model/release-tokenizer-goldens.py");
    let output = Command::new("python3")
        .arg(script)
        .arg("--pack")
        .arg(pack)
        .arg("--query-sha")
        .arg(golden::QUERY_SHA256)
        .arg("--document-sha")
        .arg(golden::DOCUMENT_SHA256)
        .arg("--fixture-out")
        .arg(&bundle_path)
        .output()
        .expect("run production tokenizer bake encoders");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let bundle = Bundle::open(bundle_path).expect("open baked release tokenizers");
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
