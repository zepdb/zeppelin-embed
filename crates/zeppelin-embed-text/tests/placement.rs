#![allow(clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use tempfile::tempdir;
use zeppelin_embed::epoch::ComputeUnits;
use zeppelin_embed_text::bundle::Bundle;
use zeppelin_embed_text::runtime::mlx::MlxRuntime;
use zeppelin_embed_text::runtime::{ModelRuntime, RuntimeError};
use zeppelin_embed_text::tower::{TokenBatch, TowerRole};

const QUERIES: [&str; 3] = [
    "what causes pulmonary hypertension",
    "a treatment does not improve survival",
    "how does white matter develop before birth",
];

#[test]
fn gpu_cpu_and_ane_query_vectors_agree_within_tolerance_on_the_golden_queries() {
    let bundle_root = std::env::var_os("ZE_TEXT_TEST_BUNDLE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/private/tmp/ze-model-bundles-v2-c1"));
    let bundle = Arc::new(
        Bundle::open(bundle_root.join("leaf-v1.5-pair.zem")).expect("open LEAF pair bundle"),
    );
    let tokens = pad_to_32(
        bundle
            .tokenize_queries(&QUERIES)
            .expect("tokenize placement queries"),
    );

    let mut gpu = MlxRuntime::load_for_compute(
        Arc::clone(&bundle),
        TowerRole::Query,
        ComputeUnits::CpuAndGpu,
    )
    .expect("load MLX GPU query tower");
    let mut cpu =
        MlxRuntime::load_for_compute(Arc::clone(&bundle), TowerRole::Query, ComputeUnits::Cpu)
            .expect("load MLX CPU query tower");
    let gpu_vectors = normalized(gpu.embed_batch(&tokens).expect("GPU vectors").into_values());
    let cpu_vectors = normalized(cpu.embed_batch(&tokens).expect("CPU vectors").into_values());

    let directory = tempdir().expect("placement tempdir");
    let input = directory.path().join("tokens.bin");
    let output = directory.path().join("ane.f32");
    write_tokens(&input, &tokens);
    let model = std::env::var_os("ZE_COREML_QUERY_MODEL")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/private/tmp/ze-coreml-v2-c2/mdbr-leaf-ir.mlpackage"));
    assert!(
        model.is_dir(),
        "CoreML query model is absent: {}",
        model.display()
    );
    let ze_model = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/ze-model/ze-model");
    let status = Command::new("/private/tmp/ze-coreml-v2-c2/bin/python")
        .arg(ze_model)
        .arg("coreml-run")
        .arg("--model")
        .arg(&model)
        .arg("--input")
        .arg(&input)
        .arg("--compute-units")
        .arg("cpu-and-neural-engine")
        .arg("--output")
        .arg(&output)
        .status()
        .expect("run CoreML placement arm");
    assert!(status.success(), "CoreML placement arm failed");
    let ane_vectors = normalized(read_f32(&output));

    assert_cosine_drift(&gpu_vectors, &cpu_vectors, 768, 1.0e-5, "MLX CPU");
    assert_cosine_drift(&gpu_vectors, &ane_vectors, 768, 1.0e-3, "ANE");

    assert_eq!(MlxRuntime::max_eval_rows(), 32);
    let rows = MlxRuntime::max_eval_rows() + 1;
    let chunked = TokenBatch::new(
        tokens.token_ids[..32].repeat(rows),
        tokens.attention_mask[..32].repeat(rows),
        rows,
        32,
    )
    .expect("watchdog-bound batch");
    let output = gpu.embed_batch(&chunked).expect("chunked GPU vectors");
    assert_eq!(output.rows(), rows);
    assert_eq!(output.dims(), 768);

    let error = MlxRuntime::load_for_compute(
        Arc::clone(&bundle),
        TowerRole::Query,
        ComputeUnits::CpuAndNeuralEngine,
    )
    .err()
    .expect("MLX must refuse Neural Engine routing");
    assert!(matches!(
        error,
        RuntimeError::Mlx(detail) if detail == "MLX does not target the Neural Engine"
    ));
}

fn pad_to_32(tokens: TokenBatch) -> TokenBatch {
    if tokens.tokens_per_row == 32 {
        return tokens;
    }
    assert!(tokens.tokens_per_row < 32, "golden query exceeds 32 tokens");
    let mut ids = Vec::with_capacity(tokens.rows * 32);
    let mut mask = Vec::with_capacity(tokens.rows * 32);
    for row in 0..tokens.rows {
        let start = row * tokens.tokens_per_row;
        let end = start + tokens.tokens_per_row;
        ids.extend_from_slice(&tokens.token_ids[start..end]);
        mask.extend_from_slice(&tokens.attention_mask[start..end]);
        ids.resize(ids.len() + 32 - tokens.tokens_per_row, 0);
        mask.resize(mask.len() + 32 - tokens.tokens_per_row, 0.0);
    }
    TokenBatch::new(ids, mask, tokens.rows, 32).expect("32-token placement batch")
}

fn normalized(mut values: Vec<f32>) -> Vec<f32> {
    for row in values.chunks_exact_mut(768) {
        let norm = row
            .iter()
            .map(|value| f64::from(*value) * f64::from(*value))
            .sum::<f64>()
            .sqrt();
        for value in row {
            *value = (f64::from(*value) / norm) as f32;
        }
    }
    values
}

fn assert_cosine_drift(left: &[f32], right: &[f32], dims: usize, tolerance: f64, arm: &str) {
    assert_eq!(left.len(), right.len());
    for (row, (left, right)) in left
        .chunks_exact(dims)
        .zip(right.chunks_exact(dims))
        .enumerate()
    {
        let cosine = left
            .iter()
            .zip(right)
            .map(|(left, right)| f64::from(*left) * f64::from(*right))
            .sum::<f64>();
        assert!(
            1.0 - cosine <= tolerance,
            "{arm} row {row} cosine drift {} exceeds {tolerance}",
            1.0 - cosine
        );
    }
}

fn write_tokens(path: &Path, tokens: &TokenBatch) {
    let mut output = Vec::new();
    output.extend_from_slice(&(tokens.rows as u32).to_le_bytes());
    output.extend_from_slice(&(tokens.tokens_per_row as u32).to_le_bytes());
    for token in &tokens.token_ids {
        output.extend_from_slice(&token.to_le_bytes());
    }
    for value in &tokens.attention_mask {
        output.extend_from_slice(&value.to_le_bytes());
    }
    std::fs::write(path, output).expect("write placement tokens");
}

fn read_f32(path: &Path) -> Vec<f32> {
    std::fs::read(path)
        .expect("read CoreML vectors")
        .chunks_exact(4)
        .map(|bytes| f32::from_le_bytes(bytes.try_into().expect("f32 bytes")))
        .collect()
}
