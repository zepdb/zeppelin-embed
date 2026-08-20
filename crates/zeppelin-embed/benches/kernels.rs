use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use zeppelin_embed::kernels::{
    dot_f16, dot_f32, dot_i8, dot_i8_batch, hamming_u1, hamming_u1_batch,
};

const DIMENSIONS: [usize; 3] = [384, 768, 1_024];
const BATCH_ROWS: usize = 100_000;

fn deterministic_i8(index: usize) -> i8 {
    ((index.wrapping_mul(31).wrapping_add(17) % 255) as i16 - 127) as i8
}

fn deterministic_u8(index: usize) -> u8 {
    index.wrapping_mul(37).wrapping_add(11) as u8
}

fn deterministic_f32(index: usize) -> f32 {
    let centered = (index.wrapping_mul(43).wrapping_add(19) % 2_001) as f32 - 1_000.0;
    centered / 1_024.0
}

fn f32_to_f16_bits(value: f32) -> u16 {
    // Benchmark fixtures use only finite normal values in [-1, 1]. This
    // compact conversion is fixture generation, not a shipped quantizer.
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    let fraction = ((bits >> 13) & 0x03ff) as u16;
    if exponent <= 0 {
        sign
    } else if exponent >= 0x1f {
        sign | 0x7c00
    } else {
        sign | ((exponent as u16) << 10) | fraction
    }
}

fn single_vector_benchmarks(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("kernels/single");
    for d in DIMENSIONS {
        let i8_a: Vec<i8> = (0..d).map(deterministic_i8).collect();
        let i8_b: Vec<i8> = (d..d * 2).map(deterministic_i8).collect();
        group.bench_with_input(BenchmarkId::new("i8", d), &d, |bencher, _dimension| {
            bencher.iter(|| dot_i8(black_box(&i8_a), black_box(&i8_b)));
        });

        let d_bytes = d / 8;
        let u1_a: Vec<u8> = (0..d_bytes).map(deterministic_u8).collect();
        let u1_b: Vec<u8> = (d_bytes..d_bytes * 2).map(deterministic_u8).collect();
        group.bench_with_input(BenchmarkId::new("u1", d), &d, |bencher, _dimension| {
            bencher.iter(|| hamming_u1(black_box(&u1_a), black_box(&u1_b)));
        });

        let f32_a: Vec<f32> = (0..d).map(deterministic_f32).collect();
        let f32_b: Vec<f32> = (d..d * 2).map(deterministic_f32).collect();
        group.bench_with_input(BenchmarkId::new("f32", d), &d, |bencher, _dimension| {
            bencher.iter(|| dot_f32(black_box(&f32_a), black_box(&f32_b)));
        });

        let f16_a: Vec<u16> = f32_a.iter().copied().map(f32_to_f16_bits).collect();
        let f16_b: Vec<u16> = f32_b.iter().copied().map(f32_to_f16_bits).collect();
        group.bench_with_input(BenchmarkId::new("f16", d), &d, |bencher, _dimension| {
            bencher.iter(|| dot_f16(black_box(&f16_a), black_box(&f16_b)));
        });
    }
    group.finish();
}

fn batch_benchmarks(criterion: &mut Criterion) {
    for d in DIMENSIONS {
        let q: Vec<i8> = (0..d).map(deterministic_i8).collect();
        let rows: Vec<i8> = (0..d * BATCH_ROWS).map(deterministic_i8).collect();
        let mut out = vec![0_i32; BATCH_ROWS];
        let mut group = criterion.benchmark_group(format!("kernels/batch/i8/{d}"));
        group.throughput(Throughput::Bytes(rows.len() as u64));
        group.bench_function("100k_rows", |bencher| {
            bencher.iter(|| {
                dot_i8_batch(black_box(&q), black_box(&rows), d, black_box(&mut out));
            });
        });
        group.finish();

        let d_bytes = d / 8;
        let q: Vec<u8> = (0..d_bytes).map(deterministic_u8).collect();
        let rows: Vec<u8> = (0..d_bytes * BATCH_ROWS).map(deterministic_u8).collect();
        let mut out = vec![0_u32; BATCH_ROWS];
        let mut group = criterion.benchmark_group(format!("kernels/batch/u1/{d}"));
        group.throughput(Throughput::Bytes(rows.len() as u64));
        group.bench_function("100k_rows", |bencher| {
            bencher.iter(|| {
                hamming_u1_batch(
                    black_box(&q),
                    black_box(&rows),
                    d_bytes,
                    black_box(&mut out),
                );
            });
        });
        group.finish();
    }
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(10)
        .warm_up_time(std::time::Duration::from_secs(1))
        .measurement_time(std::time::Duration::from_secs(3));
    targets = single_vector_benchmarks, batch_benchmarks
}
criterion_main!(benches);
