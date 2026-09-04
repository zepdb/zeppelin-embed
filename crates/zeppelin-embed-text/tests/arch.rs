#![allow(clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;

use tempfile::tempdir;
use zeppelin_embed_text::bundle::{Bundle, BundleError};
use zeppelin_embed_text::runtime::ModelRuntime;
use zeppelin_embed_text::runtime::mlx::MlxRuntime;
use zeppelin_embed_text::tower::{TokenBatch, TowerRole};

mod common;

#[test]
fn bert_with_dense_head_matches_the_fp32_reference_vectors_for_mdbr_leaf_ir_to_1e_4() {
    matches_reference(
        "leaf-v1.5-pair.zem",
        TowerRole::Query,
        include_str!("goldens/mdbr_leaf_ir.json"),
    );
    bert_fixture_exercises_declared_pooling_and_mrl();
}

#[test]
fn gte_matches_the_fp32_reference_vectors_for_arctic_m_v2_to_1e_4() {
    matches_reference(
        "arctic-m-v2-symmetric.zem",
        TowerRole::Document,
        include_str!("goldens/arctic_m_v2.json"),
    );
}

#[test]
fn an_unknown_architecture_id_fails_typed_at_load() {
    let directory = tempdir().expect("tempdir");
    let path = directory.path().join("unknown.zem");
    let mut tower = common::FixtureTower::document("unknown-architecture");
    tower.architecture = 900;
    common::write_unchecked_fixture_bundle(&path, &[tower], &[]);

    let error = match Bundle::open(&path) {
        Ok(_) => panic!("unknown architecture must fail"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        BundleError::UnknownArchitecture { id: 900 }
    ));
}

fn matches_reference(bundle_name: &str, role: TowerRole, golden: &str) {
    let root = std::env::var_os("ZE_TEXT_TEST_BUNDLE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/private/tmp/ze-model-bundles-v2-c1"));
    let bundle = Arc::new(Bundle::open(root.join(bundle_name)).expect("open baked test bundle"));
    let mut runtime = MlxRuntime::load(Arc::clone(&bundle), role).expect("load MLX tower");
    let token_ids = json_i32_array(golden, "token_ids");
    let expected = json_f32_array(golden, "vector");
    let text = json_string(golden, "text");
    let prefix = json_string(golden, "prefix");
    assert_eq!(bundle.query_tower().embedding.prompt_prefix, prefix);
    let tokens = bundle.tokenize_query(text).expect("tokenize golden text");
    assert_eq!(tokens.token_ids, token_ids);
    let first = runtime
        .embed_batch(&tokens)
        .expect("evaluate MLX tower")
        .into_values();
    let second = runtime
        .embed_batch(&tokens)
        .expect("repeat MLX tower")
        .into_values();
    assert!(
        first
            .iter()
            .zip(&second)
            .all(|(left, right)| left.to_bits() == right.to_bits())
    );
    let identity = runtime.identity();
    assert_eq!(identity.name, "mlx-c");
    assert!(identity.gpu);
    let actual = first;
    assert_eq!(actual.len(), expected.len());
    let squared_norm = actual
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>()
        .sqrt();
    let actual = actual
        .into_iter()
        .map(|value| (f64::from(value) / squared_norm) as f32)
        .collect::<Vec<_>>();
    let maximum_error = actual
        .iter()
        .zip(expected)
        .map(|(actual, expected)| (actual - expected).abs())
        .fold(0.0_f32, f32::max);
    assert!(
        maximum_error <= 1.0e-4,
        "maximum absolute error {maximum_error}"
    );
}

fn bert_fixture_exercises_declared_pooling_and_mrl() {
    let directory = tempdir().expect("tempdir");
    let tokens = TokenBatch::new(vec![2, 5, 6, 3, 0], vec![1.0, 1.0, 1.0, 1.0, 0.0], 1, 5)
        .expect("fixture tokens");
    for (name, pooling) in [("mean", 1), ("last", 3)] {
        let path = directory.path().join(format!("{name}.zem"));
        let mut tower = common::FixtureTower::document(name);
        tower.pooling = pooling;
        tower.dims = 1;
        common::write_fixture_bundle(&path, &[tower], &[]);
        let bundle = Arc::new(Bundle::open(path).expect("open pooling fixture"));
        let mut runtime = MlxRuntime::load(bundle, TowerRole::Document).expect("load fixture");
        runtime.warm().expect("warm fixture runtime");
        let values = runtime
            .embed_batch(&tokens)
            .expect("evaluate fixture pooling")
            .into_values();
        assert_eq!(values.len(), 1);
    }

    let path = directory.path().join("oversized-dims.zem");
    let mut tower = common::FixtureTower::document("oversized-dims");
    tower.dims = 3;
    common::write_fixture_bundle(&path, &[tower], &[]);
    let bundle = Arc::new(Bundle::open(path).expect("open oversized fixture"));
    let mut runtime = MlxRuntime::load(bundle, TowerRole::Document).expect("load fixture");
    assert!(runtime.embed_batch(&tokens).is_err());
}

fn json_string<'a>(document: &'a str, key: &str) -> &'a str {
    let marker = format!("\"{key}\": \"");
    document
        .split_once(&marker)
        .and_then(|(_, suffix)| suffix.split_once('"'))
        .map(|(value, _)| value)
        .expect("golden string")
}

fn json_i32_array(document: &str, key: &str) -> Vec<i32> {
    json_array(document, key)
        .into_iter()
        .map(|value| value.parse().expect("golden i32"))
        .collect()
}

fn json_f32_array(document: &str, key: &str) -> Vec<f32> {
    json_array(document, key)
        .into_iter()
        .map(|value| value.parse().expect("golden f32"))
        .collect()
}

fn json_array<'a>(document: &'a str, key: &str) -> Vec<&'a str> {
    let marker = format!("\"{key}\": [");
    let body = document
        .split_once(&marker)
        .and_then(|(_, suffix)| suffix.split_once(']'))
        .map(|(body, _)| body)
        .expect("golden array");
    body.split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect()
}
