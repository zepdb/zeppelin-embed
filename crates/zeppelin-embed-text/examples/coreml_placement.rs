#![allow(clippy::expect_used, clippy::indexing_slicing)]
//! Compares the CoreML query tower against MLX for agreement and speed.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use zeppelin_embed_text::bundle::Bundle;
use zeppelin_embed_text::runtime::ModelRuntime;
use zeppelin_embed_text::runtime::coreml::{ComputeUnits, CoreMlRuntime};
use zeppelin_embed_text::runtime::mlx::MlxRuntime;
use zeppelin_embed_text::tower::TowerRole;

const QUERIES: [&str; 5] = [
    "Invest in low cost small cap index funds when saving towards retirement?",
    "How do I deposit a cheque issued to an associate in my business into my account?",
    "what is the difference between a 401k and an IRA",
    "Can I open a Roth IRA if I have no earned income",
    "How should I begin investing with a small amount of money",
];

fn percentiles(mut samples: Vec<f64>) -> (f64, f64) {
    samples.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    let p50 = samples[samples.len() / 2];
    let p95 = samples[(samples.len() * 95) / 100];
    (p50, p95)
}

fn main() {
    let mut arguments = std::env::args_os().skip(1).map(PathBuf::from);
    let bundle_path = arguments.next().expect("bundle path");
    let model_path = arguments.next().expect("coreml model path");
    let sequence: usize = std::env::var("ZE_COREML_TOKENS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(32);

    let bundle = Arc::new(Bundle::open(&bundle_path).expect("open bundle"));
    let dims = bundle.query_tower().embedding.dims as usize;

    // MLX reference, one row per query at the bundle's own padding.
    let mut mlx = MlxRuntime::load(Arc::clone(&bundle), TowerRole::Query).expect("load mlx");
    let mut mlx_vectors = Vec::new();
    let mut mlx_samples = Vec::new();
    for query in QUERIES {
        let tokens = bundle.tokenize_query(query).expect("tokenize");
        let _ = mlx.embed_batch(&tokens).expect("mlx warm");
        for _ in 0..40 {
            let started = Instant::now();
            let batch = mlx.embed_batch(&tokens).expect("mlx embed");
            mlx_samples.push(started.elapsed().as_secs_f64() * 1000.0);
            if mlx_vectors.len() < QUERIES.len() * dims {
                if mlx_samples.len() % 40 == 1 {
                    mlx_vectors.extend_from_slice(batch.values());
                }
            }
        }
    }
    drop(mlx);

    let start = Instant::now();
    let mut coreml = CoreMlRuntime::load(
        &model_path,
        sequence,
        dims,
        ComputeUnits::CpuAndNeuralEngine,
    )
    .expect("load coreml");
    let load_ms = start.elapsed().as_secs_f64() * 1000.0;
    coreml.warm().expect("warm coreml");

    let mut coreml_samples = Vec::new();
    let mut worst_cosine = 1.0_f64;
    for (index, query) in QUERIES.iter().enumerate() {
        let tokens = bundle
            .tokenize_query_padded(query, sequence)
            .expect("tokenize padded");
        let mut vector = Vec::new();
        for round in 0..40 {
            let started = Instant::now();
            let batch = coreml.embed_batch(&tokens).expect("coreml embed");
            coreml_samples.push(started.elapsed().as_secs_f64() * 1000.0);
            if round == 0 {
                vector = batch.values().to_vec();
            }
        }
        let reference = &mlx_vectors[index * dims..(index + 1) * dims];
        let dot: f64 = reference
            .iter()
            .zip(&vector)
            .map(|(a, b)| f64::from(*a) * f64::from(*b))
            .sum();
        let na: f64 = reference
            .iter()
            .map(|v| f64::from(*v) * f64::from(*v))
            .sum();
        let nb: f64 = vector.iter().map(|v| f64::from(*v) * f64::from(*v)).sum();
        let cosine = dot / (na.sqrt() * nb.sqrt());
        worst_cosine = worst_cosine.min(cosine);
    }

    let (mlx_p50, mlx_p95) = percentiles(mlx_samples);
    let (ane_p50, ane_p95) = percentiles(coreml_samples);
    println!(
        "{{\"kind\":\"coreml_placement\",\"tokens\":{sequence},\"dims\":{dims},\
\"coreml_load_ms\":{load_ms:.3},\"mlx_p50_ms\":{mlx_p50:.4},\"mlx_p95_ms\":{mlx_p95:.4},\
\"ane_p50_ms\":{ane_p50:.4},\"ane_p95_ms\":{ane_p95:.4},\
\"speedup\":{:.3},\"worst_cosine_vs_mlx\":{worst_cosine:.6}}}",
        mlx_p50 / ane_p50
    );
}
