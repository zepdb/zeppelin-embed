//! M1 arbitrary-row Bit4 gather benchmark.

use std::error::Error;
use std::hint::black_box;
use std::io;
use std::time::Instant;

use zeppelin_embed::kernels::{
    Bit4Row, Bit4Rows4, GatherShapeError, KernelVariant, prefetch_bit4_row_group,
};
use zeppelin_embed::quant::Bit4Factors;
use zeppelin_embed_bench::platform::memory_graph::{calibrate_core, verify_bench_profile};
use zeppelin_embed_bench::platform::taint::{
    detect_taint, format_load1, format_taint_labels, print_taint_status,
};

const D: usize = 128;
const ROW_BYTES: usize = D.div_ceil(2);
const MIB: usize = 1_024 * 1_024;
const L2_WORKING_SET_BYTES: usize = 8 * MIB;
const DRAM_WORKING_SET_BYTES: usize = 768 * MIB;
const WARMUP_PASSES: usize = 1;
const REPEATS: usize = 7;
const LOAD_LIMIT: f64 = 1.0;

fn main() {
    if let Err(error) = run() {
        eprintln!("gather-kernel: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    verify_bench_profile()?;
    let taint = detect_taint(LOAD_LIMIT);
    let canary = calibrate_core()?;
    println!(
        "GATHER_KERNEL_CONTEXT build_profile=bench opt_level={} dimension={D} row_bytes={ROW_BYTES} warmup_passes={WARMUP_PASSES} repeats={REPEATS} load_limit={LOAD_LIMIT:.2}",
        env!("ZEPPELIN_BENCH_OPT_LEVEL")
    );
    print_taint_status(&taint, LOAD_LIMIT, "gather-kernel");
    canary.print();

    measure_working_set(
        "l2_resident",
        L2_WORKING_SET_BYTES,
        false,
        taint.load1,
        &taint.taints,
    )?;
    measure_working_set(
        "dram_scattered",
        DRAM_WORKING_SET_BYTES,
        false,
        taint.load1,
        &taint.taints,
    )?;
    measure_working_set(
        "dram_scattered_prefetch_next_group",
        DRAM_WORKING_SET_BYTES,
        true,
        taint.load1,
        &taint.taints,
    )?;
    Ok(())
}

fn measure_working_set(
    label: &str,
    working_set_bytes: usize,
    prefetch_next: bool,
    load1: Option<f64>,
    taints: &[zeppelin_embed_bench::platform::taint::Taint],
) -> Result<(), Box<dyn Error>> {
    if !working_set_bytes.is_multiple_of(ROW_BYTES) {
        return Err(io::Error::other("working set is not row-exact").into());
    }
    let row_count = working_set_bytes / ROW_BYTES;
    if !row_count.is_multiple_of(4) {
        return Err(io::Error::other("working set is not four-row exact").into());
    }
    let storage = (0..working_set_bytes)
        .map(|index| (index.wrapping_mul(37) as u8).wrapping_add(11))
        .collect::<Vec<_>>();
    let stride = coprime_stride(row_count);
    let groups = build_groups(&storage, row_count, stride)?;
    let query = (0..D)
        .map(|index| (index.wrapping_mul(29) as i8).wrapping_sub(61))
        .collect::<Vec<_>>();
    let prepared_query = prepare_query(&query);
    let query_sum = query.iter().map(|&code| i32::from(code)).sum();
    let factors = [
        Bit4Factors::from_persisted(1.0, 1.0, 0.75),
        Bit4Factors::from_persisted(1.25, 1.0, 0.5),
        Bit4Factors::from_persisted(0.75, 1.0, 1.25),
        Bit4Factors::from_persisted(2.0, 1.0, 0.25),
    ];
    let mut observations = Vec::with_capacity(REPEATS);
    let mut repeat_checksums = Vec::with_capacity(REPEATS);
    let mut outputs = vec![[0.0_f32; 4]; groups.len()];
    let kernel = KernelVariant::selected();
    for _ in 0..WARMUP_PASSES {
        score_groups(
            &kernel,
            (&prepared_query, query_sum, 0.125),
            &groups,
            &factors,
            &mut outputs,
            prefetch_next,
        )?;
    }
    let warmup_checksum = black_box(output_checksum(&outputs));
    for _ in 0..REPEATS {
        let started = Instant::now();
        score_groups(
            &kernel,
            (&prepared_query, query_sum, 0.125),
            &groups,
            &factors,
            &mut outputs,
            prefetch_next,
        )?;
        let elapsed = started.elapsed();
        observations.push(elapsed.as_secs_f64() * 1e9 / row_count as f64);
        repeat_checksums.push(black_box(output_checksum(&outputs)));
    }
    let mean = observations.iter().sum::<f64>() / observations.len() as f64;
    let mut sorted_observations = observations.clone();
    sorted_observations.sort_by(f64::total_cmp);
    let median = sorted_observations
        .get(sorted_observations.len() / 2)
        .copied()
        .ok_or_else(|| io::Error::other("no timed gather observations"))?;
    let squared_error = observations
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>();
    let variance = squared_error / observations.len().saturating_sub(1).max(1) as f64;
    let rsd_percent = variance.sqrt() / mean * 100.0;
    println!(
        "GATHER_KERNEL_RESULT case={label} working_set_bytes={working_set_bytes} rows={row_count} groups={} stride={stride} prefetch_next={prefetch_next} warmup_passes={WARMUP_PASSES} warmup_checksum={warmup_checksum} repeat_ns_per_row={} mean_ns_per_row={mean:.6} median_ns_per_row={median:.6} rsd_percent={rsd_percent:.6} checksums={} load1={} taint={}",
        groups.len(),
        format_f64s(&observations),
        format_u32s(&repeat_checksums),
        format_load1(load1),
        format_taint_labels(taints),
    );
    black_box(&storage);
    black_box(&groups);
    Ok(())
}

fn score_groups(
    kernel: &KernelVariant,
    query: (&[i8], i32, f64),
    groups: &[Bit4Rows4<'_>],
    factors: &[Bit4Factors; 4],
    outputs: &mut [[f32; 4]],
    prefetch_next: bool,
) -> Result<(), GatherShapeError> {
    if prefetch_next {
        for (group_index, (rows, out)) in groups.iter().zip(outputs.iter_mut()).enumerate() {
            if let Some(next) = groups.get(group_index + 1) {
                prefetch_bit4_row_group(next);
            }
            kernel.score_bit4_ptrs(query, rows, factors, out)?;
        }
    } else {
        for (rows, out) in groups.iter().zip(outputs.iter_mut()) {
            kernel.score_bit4_ptrs(query, rows, factors, out)?;
        }
    }
    Ok(())
}

fn output_checksum(outputs: &[[f32; 4]]) -> u32 {
    outputs
        .iter()
        .flatten()
        .map(|score| score.to_bits())
        .fold(0x811c_9dc5_u32, |sum, bits| sum.rotate_left(5) ^ bits)
}

fn build_groups<'a>(
    storage: &'a [u8],
    row_count: usize,
    stride: usize,
) -> Result<Vec<Bit4Rows4<'a>>, Box<dyn Error>> {
    let mut groups = Vec::with_capacity(row_count / 4);
    for group_index in 0..row_count / 4 {
        let position = group_index * 4;
        let rows = [
            row_at(storage, permuted_index(position, row_count, stride))?,
            row_at(storage, permuted_index(position + 1, row_count, stride))?,
            row_at(storage, permuted_index(position + 2, row_count, stride))?,
            row_at(storage, permuted_index(position + 3, row_count, stride))?,
        ];
        groups.push(Bit4Rows4::from_rows(rows, ROW_BYTES)?);
    }
    Ok(groups)
}

fn row_at(storage: &[u8], row_index: usize) -> Result<Bit4Row<'_>, Box<dyn Error>> {
    let offset = row_index
        .checked_mul(ROW_BYTES)
        .ok_or_else(|| io::Error::other("row offset overflow"))?;
    Bit4Row::from_mapped_region(storage, offset, ROW_BYTES)
        .ok_or_else(|| io::Error::other("row offset escaped working set").into())
}

fn permuted_index(position: usize, row_count: usize, stride: usize) -> usize {
    ((position as u128 * stride as u128) % row_count as u128) as usize
}

fn coprime_stride(row_count: usize) -> usize {
    let mut stride = (0x9e37_79b1_usize % row_count).max(1);
    while gcd(stride, row_count) != 1 {
        stride += 1;
    }
    stride
}

const fn gcd(mut left: usize, mut right: usize) -> usize {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left
}

fn prepare_query(query: &[i8]) -> Vec<i8> {
    let mut prepared = Vec::with_capacity(query.len());
    for block in query.chunks(32) {
        prepared.extend(block.iter().step_by(2).copied());
        prepared.extend(block.iter().skip(1).step_by(2).copied());
    }
    prepared
}

fn format_f64s(values: &[f64]) -> String {
    values
        .iter()
        .map(|value| format!("{value:.6}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn format_u32s(values: &[u32]) -> String {
    values
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}
