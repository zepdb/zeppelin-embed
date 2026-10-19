#![allow(clippy::expect_used, clippy::indexing_slicing)]

use std::sync::Arc;
use tempfile::tempdir;
use zeppelin_embed_text::bundle::Bundle;
use zeppelin_embed_text::runtime::ModelRuntime;
use zeppelin_embed_text::runtime::mlx::MlxRuntime;
use zeppelin_embed_text::tower::{TokenBatch, TowerRole};

#[path = "fixtures/ze115/arctic_query_reference.rs"]
#[allow(clippy::excessive_precision)]
mod reference;

fn tiny_bundle() -> Arc<Bundle> {
    let directory = tempdir().expect("fixture directory");
    let path = directory.path().join("arctic.zem");
    std::fs::write(&path, include_bytes!("fixtures/ze115/arctic_query.zem"))
        .expect("invented model");
    Arc::new(Bundle::open(path).expect("ArcticQuery architecture must load"))
}

#[test]
fn arctic_query_loads_factorized_config_from_layout_two() {
    let bundle = tiny_bundle();
    let tower = bundle.query_tower();
    assert_eq!(tower.architecture as u16, 3);
    assert_eq!(tower.config.hidden, 4);
    assert_eq!(tower.config.embedding_rank, 2);
    assert_eq!(tower.config.output_dimension, 6);
    assert_eq!(tower.embedding.dims, 6);
    assert_eq!(
        bundle
            .tensor("document/embeddings.embedding_projection.weight")
            .expect("input projection")
            .shape,
        [4, 2]
    );
    assert_eq!(
        bundle
            .tensor("document/output_projection.weight")
            .expect("output projection")
            .shape,
        [6, 4]
    );
}

#[test]
fn arctic_query_factorized_embeddings_and_projection_match_torch() {
    let mut runtime = MlxRuntime::load(tiny_bundle(), TowerRole::Query).expect("MLX runtime");
    let tokens = TokenBatch::new(
        vec![2, 5, 6, 3, 0, 2, 6, 3, 0, 0],
        vec![1., 1., 1., 1., 0., 1., 1., 1., 0., 0.],
        2,
        5,
    )
    .expect("ragged batch");
    let output = runtime
        .embed_batch(&tokens)
        .expect("factorized ArcticQuery forward");
    assert_eq!((output.rows(), output.dims()), (2, 6));
    for (actual, expected) in output
        .values()
        .iter()
        .zip(reference::ARCTIC_QUERY_REFERENCE.concat())
    {
        assert!((actual - expected).abs() < 2e-5, "{actual} vs {expected}");
    }
}

struct Golden {
    original_tokens: usize,
    ids: Vec<i32>,
    vector: Vec<f32>,
}

fn goldens() -> [Vec<Golden>; 2] {
    let mut bytes = include_bytes!("goldens/clean71.bin").as_slice();
    fn word(bytes: &mut &[u8]) -> [u8; 4] {
        let (word, rest) = bytes.split_at(4);
        *bytes = rest;
        word.try_into().expect("four-byte fixture word")
    }
    let roles = std::array::from_fn(|_| {
        let rows = u32::from_le_bytes(word(&mut bytes));
        (0..rows)
            .map(|_| {
                let original_tokens = u32::from_le_bytes(word(&mut bytes)) as usize;
                let count = u32::from_le_bytes(word(&mut bytes));
                let ids = (0..count)
                    .map(|_| i32::from_le_bytes(word(&mut bytes)))
                    .collect();
                let vector = (0..768)
                    .map(|_| f32::from_le_bytes(word(&mut bytes)))
                    .collect();
                Golden {
                    original_tokens,
                    ids,
                    vector,
                }
            })
            .collect()
    });
    assert!(bytes.is_empty(), "entire pinned fixture consumed");
    roles
}

fn padded(cases: &[Golden]) -> TokenBatch {
    let width = cases
        .iter()
        .map(|case| case.ids.len())
        .max()
        .expect("goldens");
    let mut ids = Vec::new();
    let mut mask = Vec::new();
    for case in cases {
        ids.extend(&case.ids);
        ids.extend(std::iter::repeat_n(1, width - case.ids.len())); // source pad_token_id
        mask.extend(std::iter::repeat_n(1.0, case.ids.len()));
        mask.extend(std::iter::repeat_n(0.0, width - case.ids.len()));
    }
    TokenBatch::new(ids, mask, cases.len(), width)
        .expect("padded token IDs from reference tokenizer")
}

fn assert_pack_parity(actual: &[f32], expected: &[f32]) {
    // ModelRuntime returns projected values; TextStore applies the declared L2
    // normalization. Apply that same contract before comparing pack unit vectors.
    // FP32 parity gate: max absolute error <= 1e-4, cosine >= 0.99999.
    assert_eq!(actual.len(), 768);
    assert!(actual.iter().all(|x| x.is_finite()));
    let norm = actual
        .iter()
        .map(|x| f64::from(*x).powi(2))
        .sum::<f64>()
        .sqrt();
    assert!(norm > 0.0);
    let unit: Vec<_> = actual.iter().map(|x| f64::from(*x) / norm).collect();
    let max_abs = unit
        .iter()
        .zip(expected)
        .map(|(a, b)| (a - f64::from(*b)).abs())
        .fold(0.0_f64, f64::max);
    let expected_norm = expected
        .iter()
        .map(|x| f64::from(*x).powi(2))
        .sum::<f64>()
        .sqrt();
    let cosine = unit
        .iter()
        .zip(expected)
        .map(|(a, b)| a * f64::from(*b))
        .sum::<f64>()
        / expected_norm;
    eprintln!("max_abs_error={max_abs:.9} cosine={cosine:.9}");
    assert!(max_abs <= 1e-4, "maximum absolute error {max_abs}");
    assert!(cosine >= 0.99999, "cosine {cosine}");
}

#[test]
#[ignore = "requires ZE_CLEAN71_PACK, local ze-model Python dependencies, and MLX/Metal"]
fn clean71_pair_matches_pack_fp32_goldens_with_padding_unicode_batching_and_truncation() {
    let pack = std::path::PathBuf::from(
        std::env::var_os("ZE_CLEAN71_PACK").expect("set ZE_CLEAN71_PACK to extracted pack"),
    );
    let directory = tempdir().expect("bake directory");
    let path = directory.path().join("clean71-pair.zem");
    let baker =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/ze-model/ze-model");
    let result = std::process::Command::new("python3")
        .arg(baker)
        .arg("bake")
        .arg("--document")
        .arg(pack.join("document"))
        .args(["--document-id", "Snowflake/snowflake-arctic-embed-m-v2.0"])
        .arg("--query")
        .arg(pack.join("query"))
        .args([
            "--query-id",
            "clean71-v1",
            "--query-prefix",
            "query: ",
            "--alignment-digest",
            "01",
            "--output",
        ])
        .arg(&path)
        .status()
        .expect("run local ze-model bake");
    assert!(result.success(), "pair must bake successfully");
    let bundle = Arc::new(Bundle::open(path).expect("layout-2 pair loads"));
    let tower = bundle.query_tower();
    assert_eq!(tower.architecture as u16, 3);
    assert_eq!(
        (
            tower.config.layer_count,
            tower.config.hidden,
            tower.config.heads,
            tower.config.intermediate
        ),
        (6, 512, 8, 1536)
    );
    assert_eq!(tower.config.rope_theta, 160000.0);
    assert_eq!(tower.config.embedding_rank, 512);
    assert_eq!(tower.config.output_dimension, 768);
    assert_eq!(tower.embedding.prompt_prefix, "query: ");
    assert_eq!(tower.embedding.max_tokens, 96);
    assert_eq!(tower.embedding.dims, 768);
    assert_eq!(bundle.document_tower().architecture as u16, 2);
    let [queries, documents] = goldens();
    assert!(
        queries[2].original_tokens > 96,
        "explicit source truncation"
    );
    assert_eq!(queries[2].ids.len(), 96);
    for (role, cases) in [
        (TowerRole::Query, queries),
        (TowerRole::Document, documents),
    ] {
        let mut runtime = MlxRuntime::load(Arc::clone(&bundle), role).expect("pair runtime");
        // Each row alone and the unequal-length padded batch must match the
        // same independent pack vectors. No Rust tokenizer participates.
        for subset in cases
            .iter()
            .map(std::slice::from_ref)
            .chain(std::iter::once(cases.as_slice()))
        {
            let output = runtime.embed_batch(&padded(subset)).expect("MLX inference");
            assert_eq!((output.rows(), output.dims()), (subset.len(), 768));
            for (actual, case) in output.values().chunks_exact(768).zip(subset) {
                assert_pack_parity(actual, &case.vector);
            }
        }
    }
}
