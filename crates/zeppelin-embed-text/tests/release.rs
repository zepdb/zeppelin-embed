#![allow(clippy::expect_used)]

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tempfile::tempdir;
use zeppelin_embed_text::bundle::{Bundle, TensorDtype};
use zeppelin_embed_text::runtime::ModelRuntime;
use zeppelin_embed_text::runtime::mlx::MlxRuntime;
use zeppelin_embed_text::tower::{TokenBatch, TowerRole};
use zeppelin_embed_text::{IngestOptions, Legs, QueryOptions, TextDocument, TextStore};

fn release_bundle() -> PathBuf {
    let directory = PathBuf::from(std::env::var_os("ZE498_RELEASE").expect("set ZE498_RELEASE"));
    let mut paths = std::fs::read_dir(directory)
        .expect("release directory")
        .map(|p| p.expect("entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "zem"));
    let bundle = paths.next().expect("one .zem");
    assert!(
        paths.next().is_none(),
        "release must contain exactly one bundle"
    );
    bundle
}

#[test]
#[ignore = "requires ZE498_RELEASE and native MLX/CoreML execution; ZE_QUERY_COREML must be unset"]
fn release_opens_ingests_three_documents_and_queries_all_legs() {
    assert!(std::env::var_os("ZE_QUERY_COREML").is_none());
    let bundle = release_bundle();
    assert!(bundle.with_extension("mlmodelc").is_dir());
    let scratch = tempdir().expect("scratch store");
    let store = TextStore::open(scratch.path(), &bundle, Default::default()).expect("release open");
    let report = store
        .ingest_text(
            &[
                TextDocument::new(1, 1, "The bronze zeppelin flies above the city."),
                TextDocument::new(2, 1, "Fresh bread is baked every morning."),
                TextDocument::new(3, 1, "Whales swim in the ocean."),
            ],
            IngestOptions::default(),
        )
        .expect("ingest three documents");
    assert_eq!(report.chunks, 3);
    for legs in [Legs::Dense, Legs::Lexical, Legs::Hybrid] {
        let result = store
            .query_text_with_diagnostics("bronze zeppelin", QueryOptions::new(3).with_legs(legs))
            .expect("release query");
        assert!(!result.hits.is_empty(), "{legs:?}");
        if legs != Legs::Lexical {
            assert_eq!(
                result.backend.expect("dense query backend").runtime.name,
                "coreml-cpu-ane"
            );
        }
    }
    store.close().expect("close store");
}

fn word(input: &mut Cursor<Vec<u8>>) -> [u8; 4] {
    let mut bytes = [0; 4];
    input.read_exact(&mut bytes).expect("golden input word");
    bytes
}

#[test]
#[ignore = "requires ZE498_RELEASE (FP16), ZE498_PACK, ZE498_PYTHON and native MLX"]
fn fp16_release_document_tower_matches_pack_fp32_goldens() {
    let bundle = Arc::new(Bundle::open(release_bundle()).expect("release bundle"));
    assert_eq!(
        bundle
            .tensor("document/embeddings.word_embeddings.weight")
            .expect("word weights")
            .dtype,
        TensorDtype::F16
    );
    let python = std::env::var_os("ZE498_PYTHON").expect("set ZE498_PYTHON");
    let pack = std::env::var_os("ZE498_PACK").expect("set ZE498_PACK");
    let helper = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/ze-model/document-parity-inputs.py");
    let output = Command::new(python)
        .arg(helper)
        .arg(pack)
        .output()
        .expect("pack goldens");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut input = Cursor::new(output.stdout);
    let rows = u32::from_le_bytes(word(&mut input));
    assert!(rows > 0);
    let mut runtime =
        MlxRuntime::load(bundle, TowerRole::Document).expect("load FP16 document weights");
    let mut min_cosine = 1.0_f64;
    let mut max_abs = 0.0_f32;
    for _ in 0..rows {
        let count = u32::from_le_bytes(word(&mut input)) as usize;
        let ids = (0..count)
            .map(|_| i32::from_le_bytes(word(&mut input)))
            .collect();
        let expected: Vec<_> = (0..768)
            .map(|_| f32::from_le_bytes(word(&mut input)))
            .collect();
        let tokens = TokenBatch::new(ids, vec![1.; count], 1, count).expect("golden tokens");
        let actual = runtime
            .embed_batch(&tokens)
            .expect("FP16 document inference");
        assert_eq!(actual.values().len(), 768);
        assert!(actual.values().iter().all(|v| v.is_finite()));
        let dot: f64 = actual
            .values()
            .iter()
            .zip(&expected)
            .map(|(a, b)| f64::from(*a) * f64::from(*b))
            .sum();
        let norm = |v: &[f32]| v.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
        let actual_norm = norm(actual.values());
        assert!(actual_norm.is_finite() && actual_norm > 0.0);
        let cosine = dot / (actual_norm * norm(&expected));
        min_cosine = min_cosine.min(cosine);
        // Runtime output is raw; pack goldens are L2-normalized.
        for (a, b) in actual.values().iter().zip(&expected) {
            let normalized = (f64::from(*a) / actual_norm) as f32;
            max_abs = max_abs.max((normalized - b).abs());
        }
    }
    assert_eq!(input.position() as usize, input.get_ref().len());
    println!("ZE-498 FP16 document rows={rows} min_cosine={min_cosine:.9} max_abs={max_abs:.9}");
    assert!(min_cosine >= 0.999, "min_cosine={min_cosine}");
    assert!(max_abs <= 0.005, "max_abs={max_abs}");
}
