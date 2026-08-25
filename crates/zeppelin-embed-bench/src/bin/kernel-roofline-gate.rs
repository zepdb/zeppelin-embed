//! Per-commit roofline and cross-kernel ratio gate for Task-03 hot paths.

use std::error::Error;
use std::hint::black_box;
use std::io;
use std::time::Instant;

use zeppelin_embed::kernels::{self, KernelArm};
use zeppelin_embed_bench::kernel_gate::{
    F16_OVER_I8_CEILING, F16_ROOFLINE_FLOOR_PERCENT, F32_OVER_I8_CEILING,
    F32_ROOFLINE_FLOOR_PERCENT, I8_ROOFLINE_FLOOR_PERCENT, KernelMeasurements,
    U1_ROOFLINE_FLOOR_PERCENT, evaluate_kernel_measurements,
};
use zeppelin_embed_bench::platform::memory_graph::verify_bench_profile;
use zeppelin_embed_bench::platform::taint::{detect_taint, print_taint_status};

const DIMENSION: usize = 768;
const ITERATIONS: usize = 1_000_000;
const SAMPLES: usize = 7;

fn main() {
    if let Err(error) = run() {
        eprintln!("kernel-roofline-gate: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    verify_bench_profile()?;
    let selected = kernels::initialize()?;
    if selected != KernelArm::Neon {
        return Err(io::Error::other(format!(
            "no measured hot-kernel floor for runtime arm {selected:?}; refusing to skip"
        ))
        .into());
    }
    let taint = detect_taint(1.0);
    print_taint_status(&taint, 1.0, "kernel-roofline gate floor");
    let i8_left = generated_i8(0x11);
    let i8_right = generated_i8(0x22);
    let u1_left = generated_u1(0x33);
    let u1_right = generated_u1(0x44);
    let f16_left = generated_f16(0x55);
    let f16_right = generated_f16(0x66);
    let f32_left = generated_f32(0x77);
    let f32_right = generated_f32(0x88);

    let inputs = KernelInputs {
        i8_left: &i8_left,
        i8_right: &i8_right,
        u1_left: &u1_left,
        u1_right: &u1_right,
        f16_left: &f16_left,
        f16_right: &f16_right,
        f32_left: &f32_left,
        f32_right: &f32_right,
    };
    warm_up(&inputs);
    let measurements = KernelMeasurements {
        i8_ns: median_samples(|| measure_i8(&i8_left, &i8_right)),
        u1_ns: median_samples(|| measure_u1(&u1_left, &u1_right)),
        f16_ns: median_samples(|| measure_f16(&f16_left, &f16_right)),
        f32_ns: median_samples(|| measure_f32(&f32_left, &f32_right)),
    };

    println!(
        "KERNEL_ROOFLINE_CONTEXT machine=Apple_M3_Max model=Mac15,9 arm={selected:?} profile=bench opt_level={} dimension={DIMENSION} iterations_per_sample={ITERATIONS} samples={SAMPLES}",
        env!("ZEPPELIN_BENCH_OPT_LEVEL")
    );
    let evaluation = evaluate_kernel_measurements(measurements);
    let report = match evaluation {
        Ok(report) => report,
        Err(failures) => {
            for failure in &failures {
                eprintln!("KERNEL_ROOFLINE_FAILURE {failure}");
            }
            return Err(io::Error::other(format!(
                "{} hot-kernel invariant(s) failed",
                failures.len()
            ))
            .into());
        }
    };
    println!(
        "KERNEL_ROOFLINE_RESULT kernel=i8 ns_per_vector={:.6} roofline_percent={:.6} floor_percent={I8_ROOFLINE_FLOOR_PERCENT:.6}",
        measurements.i8_ns, report.i8_roofline_percent
    );
    println!(
        "KERNEL_ROOFLINE_RESULT kernel=u1 ns_per_vector={:.6} roofline_percent={:.6} floor_percent={U1_ROOFLINE_FLOOR_PERCENT:.6}",
        measurements.u1_ns, report.u1_roofline_percent
    );
    println!(
        "KERNEL_ROOFLINE_RESULT kernel=f16 ns_per_vector={:.6} roofline_percent={:.6} floor_percent={F16_ROOFLINE_FLOOR_PERCENT:.6}",
        measurements.f16_ns, report.f16_roofline_percent
    );
    println!(
        "KERNEL_ROOFLINE_RESULT kernel=f32 ns_per_vector={:.6} roofline_percent={:.6} floor_percent={F32_ROOFLINE_FLOOR_PERCENT:.6}",
        measurements.f32_ns, report.f32_roofline_percent
    );
    println!(
        "KERNEL_RATIO_RESULT ratio=f32/i8 actual={:.6} ceiling={F32_OVER_I8_CEILING:.6}",
        report.f32_over_i8
    );
    println!(
        "KERNEL_RATIO_RESULT ratio=f16/i8 actual={:.6} ceiling={F16_OVER_I8_CEILING:.6}",
        report.f16_over_i8
    );
    Ok(())
}

struct KernelInputs<'a> {
    i8_left: &'a [i8],
    i8_right: &'a [i8],
    u1_left: &'a [u8],
    u1_right: &'a [u8],
    f16_left: &'a [u16],
    f16_right: &'a [u16],
    f32_left: &'a [f32],
    f32_right: &'a [f32],
}

fn warm_up(inputs: &KernelInputs<'_>) {
    for _ in 0..10_000 {
        black_box(kernels::dot_i8(
            black_box(inputs.i8_left),
            black_box(inputs.i8_right),
        ));
        black_box(kernels::hamming_u1(
            black_box(inputs.u1_left),
            black_box(inputs.u1_right),
        ));
        black_box(kernels::dot_f16(
            black_box(inputs.f16_left),
            black_box(inputs.f16_right),
        ));
        black_box(kernels::dot_f32(
            black_box(inputs.f32_left),
            black_box(inputs.f32_right),
        ));
    }
}

fn median_samples(mut sample: impl FnMut() -> f64) -> f64 {
    let mut samples = (0..SAMPLES).map(|_| sample()).collect::<Vec<_>>();
    samples.sort_by(f64::total_cmp);
    samples[SAMPLES / 2]
}

fn measure_i8(left: &[i8], right: &[i8]) -> f64 {
    let started = Instant::now();
    let mut checksum = 0_i32;
    for _ in 0..ITERATIONS {
        checksum ^= black_box(kernels::dot_i8(black_box(left), black_box(right)));
    }
    black_box(checksum);
    started.elapsed().as_secs_f64() * 1e9 / ITERATIONS as f64
}

fn measure_u1(left: &[u8], right: &[u8]) -> f64 {
    let started = Instant::now();
    let mut checksum = 0_u32;
    for _ in 0..ITERATIONS {
        checksum ^= black_box(kernels::hamming_u1(black_box(left), black_box(right)));
    }
    black_box(checksum);
    started.elapsed().as_secs_f64() * 1e9 / ITERATIONS as f64
}

fn measure_f16(left: &[u16], right: &[u16]) -> f64 {
    let started = Instant::now();
    let mut checksum = 0_u32;
    for _ in 0..ITERATIONS {
        checksum ^= black_box(kernels::dot_f16(black_box(left), black_box(right))).to_bits();
    }
    black_box(checksum);
    started.elapsed().as_secs_f64() * 1e9 / ITERATIONS as f64
}

fn measure_f32(left: &[f32], right: &[f32]) -> f64 {
    let started = Instant::now();
    let mut checksum = 0_u32;
    for _ in 0..ITERATIONS {
        checksum ^= black_box(kernels::dot_f32(black_box(left), black_box(right))).to_bits();
    }
    black_box(checksum);
    started.elapsed().as_secs_f64() * 1e9 / ITERATIONS as f64
}

fn generated_i8(seed: u8) -> Vec<i8> {
    (0..DIMENSION)
        .map(|index| seed.wrapping_add((index as u8).wrapping_mul(31)) as i8)
        .collect()
}

fn generated_u1(seed: u8) -> Vec<u8> {
    (0..DIMENSION / 8)
        .map(|index| seed.wrapping_add((index as u8).wrapping_mul(17)))
        .collect()
}

fn generated_f16(seed: u8) -> Vec<u16> {
    (0..DIMENSION)
        .map(|index| 0x3c00_u16 + u16::from(seed.wrapping_add(index as u8) & 0x0f))
        .collect()
}

fn generated_f32(seed: u8) -> Vec<f32> {
    (0..DIMENSION)
        .map(|index| f32::from(seed.wrapping_add((index as u8).wrapping_mul(13))) / 255.0)
        .collect()
}
