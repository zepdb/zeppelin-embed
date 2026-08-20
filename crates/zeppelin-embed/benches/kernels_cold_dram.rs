use std::time::Duration;

use criterion::{Criterion, SamplingMode, Throughput, black_box, criterion_group, criterion_main};
use zeppelin_embed::kernels::{dot_i8_batch, hamming_u1_batch};

const D: usize = 768;
const D_BYTES: usize = D / 8;
const I8_ROWS: usize = 2_000_000;
const U1_ROWS: usize = 16_000_000;
const MATCHED_ROW_BYTES: usize = 1_536_000_000;
const MANUAL_WARMUP_PASSES: usize = 5;
const OBSERVATION_COUNT: usize = 256;

const _: [(); 1] = [(); (D * I8_ROWS == MATCHED_ROW_BYTES) as usize];
const _: [(); 1] = [(); (D_BYTES * U1_ROWS == MATCHED_ROW_BYTES) as usize];

fn deterministic_i8(index: usize) -> i8 {
    ((index.wrapping_mul(31).wrapping_add(17) % 255) as i16 - 127) as i8
}

fn deterministic_u8(index: usize) -> u8 {
    index.wrapping_mul(37).wrapping_add(11) as u8
}

fn cold_dram_enabled() -> bool {
    std::env::var_os("ZE_KERNEL_COLD_DRAM").is_some_and(|value| value == "1")
}

fn fill_i8_rows() -> Vec<i8> {
    let mut rows = vec![0_i8; MATCHED_ROW_BYTES];
    let mut state = 0x4d59_5df4_d0f3_3173_u64;
    for byte in &mut rows {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = state as u8 as i8;
    }
    rows
}

fn fill_u1_rows() -> Vec<u8> {
    let mut rows = vec![0_u8; MATCHED_ROW_BYTES];
    let mut state = 0x8a5c_d789_635d_2dff_u64;
    for byte in &mut rows {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = state as u8;
    }
    rows
}

fn observe_i32_output(out: &[i32]) {
    let stride = (out.len() / OBSERVATION_COUNT).max(1);
    let checksum = out
        .iter()
        .step_by(stride)
        .fold(0_i32, |sum, &value| sum.wrapping_add(value));
    black_box(checksum);
}

fn observe_u32_output(out: &[u32]) {
    let stride = (out.len() / OBSERVATION_COUNT).max(1);
    let checksum = out
        .iter()
        .step_by(stride)
        .fold(0_u32, |sum, &value| sum.wrapping_add(value));
    black_box(checksum);
}

fn bench_i8(criterion: &mut Criterion) {
    let q: Vec<i8> = (0..D).map(deterministic_i8).collect();
    let rows = fill_i8_rows();
    let mut out = vec![0_i32; I8_ROWS];

    for _ in 0..MANUAL_WARMUP_PASSES {
        dot_i8_batch(black_box(&q), black_box(&rows), D, black_box(&mut out));
        observe_i32_output(&out);
    }

    let mut group = criterion.benchmark_group("kernels/cold_dram/i8/768");
    group
        .sample_size(10)
        .warm_up_time(Duration::from_secs(2))
        .measurement_time(Duration::from_secs(5))
        .sampling_mode(SamplingMode::Flat)
        .throughput(Throughput::Bytes(MATCHED_ROW_BYTES as u64));
    group.bench_function("2m_rows_1536mb", |bencher| {
        bencher.iter(|| {
            dot_i8_batch(black_box(&q), black_box(&rows), D, black_box(&mut out));
            observe_i32_output(&out);
        });
    });
    group.finish();
    let checksum = out
        .iter()
        .fold(0_i64, |sum, &value| sum.wrapping_add(i64::from(value)));
    eprintln!("cold-DRAM i8 output checksum: {checksum}");
    black_box(checksum);
}

fn bench_u1(criterion: &mut Criterion) {
    let q: Vec<u8> = (0..D_BYTES).map(deterministic_u8).collect();
    let rows = fill_u1_rows();
    let mut out = vec![0_u32; U1_ROWS];

    for _ in 0..MANUAL_WARMUP_PASSES {
        hamming_u1_batch(
            black_box(&q),
            black_box(&rows),
            D_BYTES,
            black_box(&mut out),
        );
        observe_u32_output(&out);
    }

    let mut group = criterion.benchmark_group("kernels/cold_dram/u1/768");
    group
        .sample_size(10)
        .warm_up_time(Duration::from_secs(2))
        .measurement_time(Duration::from_secs(5))
        .sampling_mode(SamplingMode::Flat)
        .throughput(Throughput::Bytes(MATCHED_ROW_BYTES as u64));
    group.bench_function("16m_rows_1536mb", |bencher| {
        bencher.iter(|| {
            hamming_u1_batch(
                black_box(&q),
                black_box(&rows),
                D_BYTES,
                black_box(&mut out),
            );
            observe_u32_output(&out);
        });
    });
    group.finish();
    let checksum = out
        .iter()
        .fold(0_u64, |sum, &value| sum.wrapping_add(u64::from(value)));
    eprintln!("cold-DRAM u1 output checksum: {checksum}");
    black_box(checksum);
}

fn cold_dram_benchmarks(criterion: &mut Criterion) {
    if !cold_dram_enabled() {
        eprintln!("cold-DRAM kernels skipped; set ZE_KERNEL_COLD_DRAM=1 to run");
        return;
    }

    // Each high-entropy fixture is dropped before the next 1.536 GB fixture is
    // allocated. One full pass is over 30 times the measured ~48 MB SLC, so
    // returning to the first row cannot reuse cache lines from the prior pass.
    bench_i8(criterion);
    bench_u1(criterion);
}

criterion_group!(cold_dram, cold_dram_benchmarks);
criterion_main!(cold_dram);
