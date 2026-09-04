#![allow(clippy::expect_used, clippy::indexing_slicing)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use zeppelin_embed::epoch::ComputeUnits;
use zeppelin_embed_text::bundle::Bundle;
use zeppelin_embed_text::runtime::ModelRuntime;
use zeppelin_embed_text::runtime::mlx::MlxRuntime;
use zeppelin_embed_text::tower::{TokenBatch, TowerRole};

const QUERY: &str = "what causes pulmonary hypertension";

fn main() {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .expect("pair bundle path");
    let bundle = Arc::new(Bundle::open(path).expect("open pair bundle"));
    let output = std::env::args_os().nth(2).map(PathBuf::from);
    let source = bundle.tokenize_query(QUERY).expect("tokenize golden query");
    let gpu_load_started = Instant::now();
    let mut gpu = MlxRuntime::load_for_compute(
        Arc::clone(&bundle),
        TowerRole::Query,
        ComputeUnits::CpuAndGpu,
    )
    .expect("load MLX GPU");
    let gpu_load_ms = gpu_load_started.elapsed().as_secs_f64() * 1_000.0;
    let cpu_load_started = Instant::now();
    let mut cpu =
        MlxRuntime::load_for_compute(Arc::clone(&bundle), TowerRole::Query, ComputeUnits::Cpu)
            .expect("load MLX CPU");
    let cpu_load_ms = cpu_load_started.elapsed().as_secs_f64() * 1_000.0;
    println!("{{\"mlx_gpu_load_ms\":{gpu_load_ms:.9},\"mlx_cpu_load_ms\":{cpu_load_ms:.9}}}");
    for length in [1_usize, 16, 32] {
        let tokens = at_length(&source, length);
        if let Some(output) = &output {
            std::fs::create_dir_all(output).expect("create placement input directory");
            write_tokens(&output.join(format!("tokens-{length}.bin")), &tokens);
        }
        let (gpu_ms, gpu_vector) = measure(&mut gpu, &tokens);
        let (cpu_ms, cpu_vector) = measure(&mut cpu, &tokens);
        if let Some(output) = &output {
            write_f32(&output.join(format!("mlx-gpu-{length}.f32")), &gpu_vector);
            write_f32(&output.join(format!("mlx-cpu-{length}.f32")), &cpu_vector);
        }
        println!(
            "{{\"tokens\":{length},\"mlx_gpu_ms\":{gpu_ms:.9},\"mlx_cpu_ms\":{cpu_ms:.9},\"cpu_gpu_cosine_drift\":{:.12}}}",
            cosine_drift(&gpu_vector, &cpu_vector),
        );
    }
    let mut tokenization = Vec::with_capacity(200);
    for iteration in 0..220 {
        let started = Instant::now();
        std::hint::black_box(bundle.tokenize_query(QUERY).expect("tokenize query"));
        if iteration >= 20 {
            tokenization.push(started.elapsed().as_secs_f64() * 1_000.0);
        }
    }
    tokenization.sort_by(f64::total_cmp);
    println!(
        "{{\"tokenize_ms\":{:.9}}}",
        tokenization[tokenization.len() / 2]
    );
}

fn write_f32(path: &std::path::Path, values: &[f32]) {
    let bytes = values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect::<Vec<_>>();
    std::fs::write(path, bytes).expect("write placement vector");
}

fn write_tokens(path: &std::path::Path, tokens: &TokenBatch) {
    let mut output = Vec::new();
    output.extend_from_slice(&(tokens.rows as u32).to_le_bytes());
    output.extend_from_slice(&(tokens.tokens_per_row as u32).to_le_bytes());
    for token in &tokens.token_ids {
        output.extend_from_slice(&token.to_le_bytes());
    }
    for value in &tokens.attention_mask {
        output.extend_from_slice(&value.to_le_bytes());
    }
    std::fs::write(path, output).expect("write placement token batch");
}

fn at_length(source: &TokenBatch, length: usize) -> TokenBatch {
    let copied = source.tokens_per_row.min(length);
    let mut ids = source.token_ids[..copied].to_vec();
    let mut mask = source.attention_mask[..copied].to_vec();
    ids.resize(length, 0);
    mask.resize(length, 0.0);
    TokenBatch::new(ids, mask, 1, length).expect("placement token shape")
}

fn measure(runtime: &mut MlxRuntime, tokens: &TokenBatch) -> (f64, Vec<f32>) {
    let mut vector = Vec::new();
    let mut latencies = Vec::with_capacity(200);
    for iteration in 0..220 {
        let started = Instant::now();
        vector = runtime
            .embed_batch(tokens)
            .expect("placement embedding")
            .into_values();
        if iteration >= 20 {
            latencies.push(started.elapsed().as_secs_f64() * 1_000.0);
        }
    }
    latencies.sort_by(f64::total_cmp);
    (latencies[latencies.len() / 2], vector)
}

fn cosine_drift(left: &[f32], right: &[f32]) -> f64 {
    let mut dot = 0.0_f64;
    let mut left_norm = 0.0_f64;
    let mut right_norm = 0.0_f64;
    for (left, right) in left.iter().zip(right) {
        dot += f64::from(*left) * f64::from(*right);
        left_norm += f64::from(*left) * f64::from(*left);
        right_norm += f64::from(*right) * f64::from(*right);
    }
    1.0 - dot / (left_norm.sqrt() * right_norm.sqrt())
}
