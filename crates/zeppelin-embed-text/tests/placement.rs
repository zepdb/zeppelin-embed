#![allow(clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;

use zeppelin_embed::epoch::ComputeUnits;
use zeppelin_embed_text::bundle::Bundle;
use zeppelin_embed_text::runtime::coreml::{ComputeUnits as CoreMlComputeUnits, CoreMlRuntime};
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

    let model = std::env::var_os("ZE_COREML_QUERY_MODEL")
        .map(PathBuf::from)
        .unwrap_or_else(|| bundle_root.join("leaf-v1.5-pair.mlmodelc"));
    let sequence = std::env::var("ZE_QUERY_COREML_TOKENS")
        .map(|value| value.parse::<usize>().expect("CoreML token count"))
        .unwrap_or(64);
    // Exercise the same public runtime and compiled model as TextStore,
    // without relying on a temporary Python environment from the exporter.
    let mut ane = CoreMlRuntime::load(
        &model,
        sequence,
        768,
        CoreMlComputeUnits::CpuAndNeuralEngine,
    )
    .expect("load CoreML query model");
    let ane_tokens = tokens.padded_to(sequence).expect("CoreML padding");
    let ane_vectors = normalized(
        ane.embed_batch(&ane_tokens)
            .expect("CoreML vectors")
            .into_values(),
    );

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
