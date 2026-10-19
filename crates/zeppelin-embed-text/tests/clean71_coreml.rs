#![cfg(target_os = "macos")]
#![allow(clippy::expect_used, clippy::indexing_slicing)]

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;
use zeppelin_embed_text::runtime::ModelRuntime;
use zeppelin_embed_text::runtime::coreml::{ComputeUnits, CoreMlRuntime};
use zeppelin_embed_text::tower::TokenBatch;

fn word(input: &mut Cursor<Vec<u8>>) -> [u8; 4] {
    let mut bytes = [0; 4];
    input
        .read_exact(&mut bytes)
        .expect("reference fixture word");
    bytes
}

fn assert_parity(actual: &[f32], golden: &[f32]) -> f64 {
    assert_eq!(actual.len(), golden.len());
    assert!(actual.iter().all(|v| v.is_finite()));
    let norm = |values: &[f32]| {
        values
            .iter()
            .map(|v| f64::from(*v).powi(2))
            .sum::<f64>()
            .sqrt()
    };
    let actual_norm = norm(actual);
    assert!((actual_norm - 1.).abs() < 0.002, "norm={actual_norm}");
    let dot: f64 = actual
        .iter()
        .zip(golden)
        .map(|(a, b)| f64::from(*a) * f64::from(*b))
        .sum();
    let cosine = dot / (actual_norm * norm(golden));
    assert!(cosine >= 0.999, "cosine={cosine}; tolerance=0.001");
    cosine
}

#[test]
#[ignore = "requires ZE497_PACK, ZE497_COREML (.mlmodelc), ZE497_PYTHON and native CPU+ANE execution"]
fn clean71_coreml_matches_fp32_goldens_and_reports_batch_one_latency() {
    let pack = std::env::var_os("ZE497_PACK").expect("set ZE497_PACK to offline clean71 pack");
    let compiled = PathBuf::from(std::env::var_os("ZE497_COREML").expect("set ZE497_COREML"));
    assert_eq!(
        compiled.extension().and_then(|s| s.to_str()),
        Some("mlmodelc")
    );
    let python = std::env::var_os("ZE497_PYTHON").expect("set ZE497_PYTHON to pack's Python");
    let helper =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/ze-model/coreml-parity-inputs.py");
    let output = Command::new(python)
        .arg(helper)
        .arg(pack)
        .output()
        .expect("reference tokenizer");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut input = Cursor::new(output.stdout);
    let rows = u32::from_le_bytes(word(&mut input));
    let mut runtime = CoreMlRuntime::load(&compiled, 768, ComputeUnits::CpuAndNeuralEngine)
        .expect("load compiled clean71 on CPU+ANE");
    assert_eq!(runtime.sequence(), 96);
    assert_eq!(rows, 16, "all clean71 query goldens must be evaluated");
    let mut min_cosine = 1.0_f64;
    let mut first_fixture = None;
    for _ in 0..rows {
        let count = u32::from_le_bytes(word(&mut input)) as usize;
        let ids = (0..count)
            .map(|_| i32::from_le_bytes(word(&mut input)))
            .collect();
        let golden: Vec<f32> = (0..768)
            .map(|_| f32::from_le_bytes(word(&mut input)))
            .collect();
        let tokens = TokenBatch::new(ids, vec![1.; count], 1, count).expect("reference token ids");
        let actual = runtime
            .embed_batch(&tokens)
            .expect("native clean71 embedding");
        assert_eq!((actual.rows(), actual.dims()), (1, 768));
        min_cosine = min_cosine.min(assert_parity(actual.values(), &golden));
        if first_fixture.is_none() {
            first_fixture = Some((tokens, golden));
        }
    }
    assert_eq!(input.position(), input.get_ref().len() as u64);
    let (first_tokens, first_golden) = first_fixture.expect("at least one golden fixture");
    let overlength = TokenBatch::new(vec![0; 97], vec![1.; 97], 1, 97).expect("overlength row");
    assert!(runtime.embed_batch(&overlength).is_err());
    let tokens = first_tokens
        .padded_to(runtime.sequence())
        .expect("fixed 96 padding");
    let actual = runtime.embed_batch(&tokens).expect("fixed 96 parity");
    min_cosine = min_cosine.min(assert_parity(actual.values(), &first_golden));
    for _ in 0..20 {
        std::hint::black_box(runtime.embed_batch(&tokens).expect("warmup"));
    }
    let mut latency = Vec::with_capacity(200);
    for _ in 0..200 {
        let start = Instant::now();
        std::hint::black_box(runtime.embed_batch(&tokens).expect("timed batch one"));
        latency.push(start.elapsed().as_secs_f64() * 1000.);
    }
    latency.sort_by(f64::total_cmp);
    println!(
        "ZE497 CPU_AND_NE sequence=96 batch=1 warmup=20 runs=200 p50_ms={:.6} p95_ms={:.6}",
        latency[99], latency[189]
    );
    println!("ZE497 compared={rows} min_cosine={min_cosine:.9}");
}
