//! Synthetic memory-system premises for the graph-index latency budget.

use std::alloc::{Layout, alloc_zeroed, dealloc};
#[cfg(target_arch = "aarch64")]
use std::arch::asm;
use std::error::Error;
use std::hint::black_box;
use std::io;
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use super::taint::{TaintCheck, detect_taint, format_load1, format_taint_labels};

const KIB: usize = 1_024;
const MIB: usize = 1_024 * KIB;
const GIB: usize = 1_024 * MIB;
const DEFAULT_SEED: u64 = 0x4d30_706c_6174_666d;
const DEFAULT_REPEATS: usize = 3;
const DEFAULT_TARGET_MILLIS: u64 = 200;
const DEFAULT_LOAD_LIMIT: f64 = 1.0;
const H1_WORKING_SETS: [usize; 10] = [
    32 * KIB,
    128 * KIB,
    512 * KIB,
    2 * MIB,
    8 * MIB,
    24 * MIB,
    64 * MIB,
    256 * MIB,
    512 * MIB,
    GIB,
];
const H2_WORKING_SET: usize = 512 * MIB;
const H2_MAX_CHAINS: usize = 32;
const H3_WORKING_SETS: [usize; 2] = [256 * MIB, 768 * MIB];
const H3_RANKS: [usize; 4] = [16, 32, 48, 64];
const MAX_RANK: usize = 64;
const ADJACENCY_SEED_COUNT: usize = 31;
const MAX_AUTOSCALE_STEPS: usize = 12;
const P_CORE_MIN_GHZ: f64 = 3.40;
const P_CORE_MAX_GHZ: f64 = 4.20;

/// Run the platform-premises memory benchmark from `platform-truth`.
pub fn run(arguments: &[String]) -> Result<(), Box<dyn Error>> {
    let config = Config::parse(arguments)?;
    verify_bench_profile()?;
    let (cache_line_bytes, cache_line_source) =
        resolve_cache_line_size(config.cache_line_override)?;
    validate_cache_line(cache_line_bytes)?;
    let taint = detect_taint(config.load_limit);
    let calibration = calibrate_core()?;

    println!(
        "MEMORY_GRAPH_CONTEXT build_profile=bench opt_level={} cache_line_bytes={} cache_line_source={} seed=0x{:016x} repeats={} target_ms={} load_limit={:.2} mode={}",
        env!("ZEPPELIN_BENCH_OPT_LEVEL"),
        cache_line_bytes,
        cache_line_source,
        config.seed,
        config.repeats,
        config.target.as_millis(),
        config.load_limit,
        if config.smoke { "smoke" } else { "full" },
    );
    super::taint::print_taint_status(&taint, config.load_limit, "memory-latency");
    calibration.print();

    match config.harness {
        Harness::All => {
            run_h1(&config, cache_line_bytes, &taint)?;
            run_h2(&config, cache_line_bytes, &taint)?;
            run_h3(&config, cache_line_bytes, &taint)?;
        }
        Harness::H1 => run_h1(&config, cache_line_bytes, &taint)?,
        Harness::H2 => run_h2(&config, cache_line_bytes, &taint)?,
        Harness::H3 => run_h3(&config, cache_line_bytes, &taint)?,
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Harness {
    All,
    H1,
    H2,
    H3,
}

#[derive(Clone, Debug)]
struct Config {
    harness: Harness,
    seed: u64,
    repeats: usize,
    target: Duration,
    load_limit: f64,
    cache_line_override: Option<usize>,
    smoke: bool,
}

impl Config {
    fn parse(arguments: &[String]) -> Result<Self, Box<dyn Error>> {
        let mut index = 0_usize;
        let harness = match arguments.first().map(String::as_str) {
            Some("all") => {
                index = 1;
                Harness::All
            }
            Some("h1") => {
                index = 1;
                Harness::H1
            }
            Some("h2") => {
                index = 1;
                Harness::H2
            }
            Some("h3") => {
                index = 1;
                Harness::H3
            }
            Some(value) if !value.starts_with('-') => return Err(usage().into()),
            _ => Harness::All,
        };
        let mut config = Self {
            harness,
            seed: DEFAULT_SEED,
            repeats: DEFAULT_REPEATS,
            target: Duration::from_millis(DEFAULT_TARGET_MILLIS),
            load_limit: DEFAULT_LOAD_LIMIT,
            cache_line_override: None,
            smoke: false,
        };
        while index < arguments.len() {
            let flag = arguments.get(index).map(String::as_str).unwrap_or_default();
            if flag == "--smoke" {
                config.smoke = true;
                index = index.saturating_add(1);
                continue;
            }
            let value = arguments.get(index.saturating_add(1)).ok_or_else(usage)?;
            match flag {
                "--seed" => config.seed = parse_seed(value)?,
                "--repeats" => {
                    config.repeats = value.parse::<usize>()?;
                    if config.repeats < 3 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "--repeats must be at least 3",
                        )
                        .into());
                    }
                }
                "--target-ms" => {
                    let millis = value.parse::<u64>()?;
                    if millis == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "--target-ms must be positive",
                        )
                        .into());
                    }
                    config.target = Duration::from_millis(millis);
                }
                "--load-limit" => {
                    config.load_limit = value.parse::<f64>()?;
                    if !config.load_limit.is_finite() || config.load_limit < 0.0 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "--load-limit must be finite and non-negative",
                        )
                        .into());
                    }
                }
                "--cache-line-bytes" => {
                    config.cache_line_override = Some(value.parse::<usize>()?);
                }
                _ => return Err(usage().into()),
            }
            index = index.saturating_add(2);
        }
        Ok(config)
    }

    fn h1_working_sets(&self) -> &[usize] {
        if self.smoke {
            &[32 * KIB, 512 * KIB]
        } else {
            &H1_WORKING_SETS
        }
    }

    const fn h2_working_set(&self) -> usize {
        if self.smoke { 8 * MIB } else { H2_WORKING_SET }
    }

    const fn h2_max_chains(&self) -> usize {
        if self.smoke { 4 } else { H2_MAX_CHAINS }
    }

    fn h3_working_sets(&self) -> &[usize] {
        if self.smoke {
            &[8 * MIB]
        } else {
            &H3_WORKING_SETS
        }
    }

    fn h3_ranks(&self) -> &[usize] {
        if self.smoke { &[16, 32] } else { &H3_RANKS }
    }
}

fn usage() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "usage: platform-truth memory-graph [all|h1|h2|h3] [--seed N|0xHEX] [--repeats N>=3] [--target-ms N] [--load-limit F] [--cache-line-bytes N] [--smoke]",
    )
}

fn parse_seed(value: &str) -> Result<u64, Box<dyn Error>> {
    if let Some(hex) = value.strip_prefix("0x") {
        Ok(u64::from_str_radix(hex, 16)?)
    } else {
        Ok(value.parse::<u64>()?)
    }
}

fn resolve_cache_line_size(
    operator_override: Option<usize>,
) -> Result<(usize, &'static str), Box<dyn Error>> {
    match read_cache_line_size() {
        Ok(value) => Ok((value, "hw.cachelinesize")),
        Err(error) => match operator_override {
            Some(value) => {
                eprintln!(
                    "WARNING: hw.cachelinesize unavailable ({error}); using explicit --cache-line-bytes={value} override"
                );
                Ok((value, "operator_override_after_hw.cachelinesize_denied"))
            }
            None => Err(io::Error::other(format!(
                "hw.cachelinesize unavailable ({error}); refusing to hardcode it (restricted operators may pass an explicit --cache-line-bytes value)"
            ))
            .into()),
        },
    }
}

/// Refuses size-optimized release and debug builds for latency measurements.
#[doc(hidden)]
pub fn verify_bench_profile() -> Result<(), Box<dyn Error>> {
    let opt_level = env!("ZEPPELIN_BENCH_OPT_LEVEL");
    if opt_level != "3" || cfg!(debug_assertions) {
        return Err(io::Error::other(format!(
            "refusing measurement: build opt-level is {opt_level:?} (debug_assertions={}); use `cargo run --profile bench -p zeppelin-embed-bench --bin platform-truth -- memory-graph <h1|h2|h3>`",
            cfg!(debug_assertions)
        ))
        .into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn read_cache_line_size() -> Result<usize, Box<dyn Error>> {
    let mut value = 0_usize;
    let mut value_len = std::mem::size_of::<usize>();
    // SAFETY: the name is a static NUL-terminated string and value/value_len are writable.
    let result = unsafe {
        libc::sysctlbyname(
            c"hw.cachelinesize".as_ptr(),
            std::ptr::addr_of_mut!(value).cast(),
            std::ptr::addr_of_mut!(value_len),
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error().into());
    }
    if value_len != std::mem::size_of::<usize>() {
        return Err(io::Error::other(format!(
            "hw.cachelinesize returned {value_len} bytes, expected {}",
            std::mem::size_of::<usize>()
        ))
        .into());
    }
    Ok(value)
}

#[cfg(not(target_os = "macos"))]
fn read_cache_line_size() -> Result<usize, Box<dyn Error>> {
    let text =
        std::fs::read_to_string("/sys/devices/system/cpu/cpu0/cache/index0/coherency_line_size")?;
    Ok(text.trim().parse::<usize>()?)
}

fn validate_cache_line(cache_line_bytes: usize) -> Result<(), Box<dyn Error>> {
    if cache_line_bytes < 64
        || !cache_line_bytes.is_power_of_two()
        || cache_line_bytes < std::mem::size_of::<u64>()
    {
        return Err(
            io::Error::other(format!("unsupported hw.cachelinesize={cache_line_bytes}")).into(),
        );
    }
    Ok(())
}

/// Result from the shared P-core residency canary.
#[doc(hidden)]
pub struct CoreCalibration {
    implied_ghz: Option<f64>,
    elapsed: Duration,
    assumed_cycles: u64,
}

impl CoreCalibration {
    /// Prints the shared machine-readable canary verdict.
    #[doc(hidden)]
    pub fn print(&self) {
        match self.implied_ghz {
            Some(ghz) => println!(
                "MEMORY_GRAPH_CANARY implied_clock_ghz={ghz:.6} assumed_cycles={} elapsed_ns={} verdict=P_CORE_RANGE",
                self.assumed_cycles,
                self.elapsed.as_nanos()
            ),
            None => println!(
                "MEMORY_GRAPH_CANARY implied_clock_ghz=NA assumed_cycles={} elapsed_ns={} verdict=NOT_VERIFIED_NON_AARCH64",
                self.assumed_cycles,
                self.elapsed.as_nanos()
            ),
        }
    }
}

#[cfg(target_arch = "aarch64")]
/// Runs the shared P-core residency canary used by latency benchmarks.
#[doc(hidden)]
pub fn calibrate_core() -> Result<CoreCalibration, Box<dyn Error>> {
    const OUTER_ITERATIONS: u64 = 5_000_000;
    const DEPENDENT_ADDS: u64 = 32;
    let mut loops = OUTER_ITERATIONS;
    let mut dependency = 0_u64;
    let start = Instant::now();
    // SAFETY: the assembly touches only its register operands. Thirty-two dependent adds give
    // a known one-cycle dependency chain per loop on Apple Silicon; loop overhead is overlapped.
    unsafe {
        asm!(
            "2:",
            "add {dependency}, {dependency}, #1", "add {dependency}, {dependency}, #1",
            "add {dependency}, {dependency}, #1", "add {dependency}, {dependency}, #1",
            "add {dependency}, {dependency}, #1", "add {dependency}, {dependency}, #1",
            "add {dependency}, {dependency}, #1", "add {dependency}, {dependency}, #1",
            "add {dependency}, {dependency}, #1", "add {dependency}, {dependency}, #1",
            "add {dependency}, {dependency}, #1", "add {dependency}, {dependency}, #1",
            "add {dependency}, {dependency}, #1", "add {dependency}, {dependency}, #1",
            "add {dependency}, {dependency}, #1", "add {dependency}, {dependency}, #1",
            "add {dependency}, {dependency}, #1", "add {dependency}, {dependency}, #1",
            "add {dependency}, {dependency}, #1", "add {dependency}, {dependency}, #1",
            "add {dependency}, {dependency}, #1", "add {dependency}, {dependency}, #1",
            "add {dependency}, {dependency}, #1", "add {dependency}, {dependency}, #1",
            "add {dependency}, {dependency}, #1", "add {dependency}, {dependency}, #1",
            "add {dependency}, {dependency}, #1", "add {dependency}, {dependency}, #1",
            "add {dependency}, {dependency}, #1", "add {dependency}, {dependency}, #1",
            "add {dependency}, {dependency}, #1", "add {dependency}, {dependency}, #1",
            "subs {loops}, {loops}, #1",
            "b.ne 2b",
            dependency = inout(reg) dependency,
            loops = inout(reg) loops,
            options(nostack),
        );
    }
    black_box((dependency, loops));
    let elapsed = start.elapsed();
    let assumed_cycles = OUTER_ITERATIONS * DEPENDENT_ADDS;
    let implied_ghz = assumed_cycles as f64 / elapsed.as_secs_f64() / 1e9;
    if !(P_CORE_MIN_GHZ..=P_CORE_MAX_GHZ).contains(&implied_ghz) {
        return Err(io::Error::other(format!(
            "core-residency canary failed: implied {implied_ghz:.3} GHz is outside the P-core range {P_CORE_MIN_GHZ:.2}-{P_CORE_MAX_GHZ:.2} GHz"
        ))
        .into());
    }
    Ok(CoreCalibration {
        implied_ghz: Some(implied_ghz),
        elapsed,
        assumed_cycles,
    })
}

#[cfg(not(target_arch = "aarch64"))]
/// Reports that P-core residency cannot be verified on non-AArch64 targets.
#[doc(hidden)]
pub fn calibrate_core() -> Result<CoreCalibration, Box<dyn Error>> {
    Ok(CoreCalibration {
        implied_ghz: None,
        elapsed: Duration::ZERO,
        assumed_cycles: 0,
    })
}

struct AlignedBuffer {
    pointer: NonNull<u8>,
    len: usize,
    layout: Layout,
}

impl AlignedBuffer {
    fn new(len: usize, alignment: usize) -> Result<Self, Box<dyn Error>> {
        let layout = Layout::from_size_align(len, alignment)?;
        // SAFETY: layout is non-zero and valid. The allocation is owned until Drop.
        let raw = unsafe { alloc_zeroed(layout) };
        let pointer = NonNull::new(raw)
            .ok_or_else(|| io::Error::other(format!("failed to allocate {len} aligned bytes")))?;
        Ok(Self {
            pointer,
            len,
            layout,
        })
    }

    const fn len(&self) -> usize {
        self.len
    }

    const fn as_ptr(&self) -> *const u8 {
        self.pointer.as_ptr()
    }

    fn write_u64(&mut self, offset: usize, value: u64) {
        debug_assert!(offset.saturating_add(8) <= self.len);
        // SAFETY: the bounds are established by callers and checked in debug builds; unaligned
        // access is allowed even though benchmark allocations are cache-line aligned.
        unsafe {
            self.pointer
                .as_ptr()
                .add(offset)
                .cast::<u64>()
                .write_unaligned(value)
        };
    }

    fn write_u32(&mut self, offset: usize, value: u32) {
        debug_assert!(offset.saturating_add(4) <= self.len);
        // SAFETY: the bounds are established by callers and checked in debug builds.
        unsafe {
            self.pointer
                .as_ptr()
                .add(offset)
                .cast::<u32>()
                .write_unaligned(value)
        };
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        // SAFETY: pointer was allocated with this exact layout and has not been deallocated.
        unsafe { dealloc(self.pointer.as_ptr(), self.layout) };
    }
}

fn working_set_bytes(node_count: usize, cache_line_bytes: usize) -> Option<usize> {
    node_count.checked_mul(cache_line_bytes)
}

#[derive(Clone, Copy)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        mix64(self.state)
    }
}

const fn mix64(mut value: u64) -> u64 {
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn shuffled_order(node_count: usize, seed: u64) -> Vec<usize> {
    let mut order = (0..node_count).collect::<Vec<_>>();
    let mut rng = SplitMix64::new(seed);
    for upper in (1..node_count).rev() {
        let selected = reduce_to_range(rng.next(), upper.saturating_add(1));
        order.swap(upper, selected);
    }
    order
}

fn build_cycle(node_count: usize, seed: u64) -> Vec<usize> {
    let order = shuffled_order(node_count, seed);
    let mut next = vec![0_usize; node_count];
    if let Some((&first, rest)) = order.split_first() {
        let mut previous = first;
        for &current in rest {
            next[previous] = current;
            previous = current;
        }
        next[previous] = first;
    }
    next
}

fn reduce_to_range(value: u64, bound: usize) -> usize {
    debug_assert!(bound > 0);
    ((u128::from(value) * bound as u128) >> 64) as usize
}

fn install_cycle(buffer: &mut AlignedBuffer, next: &[usize], cache_line_bytes: usize) {
    for (node, &successor) in next.iter().enumerate() {
        buffer.write_u64(node * cache_line_bytes, successor as u64);
    }
}

fn run_h1(
    config: &Config,
    cache_line_bytes: usize,
    taint: &TaintCheck,
) -> Result<(), Box<dyn Error>> {
    let working_sets = config.h1_working_sets();
    let maximum = *working_sets
        .iter()
        .max()
        .ok_or_else(|| io::Error::other("H1 has no working sets"))?;
    let mut buffer = AlignedBuffer::new(maximum, cache_line_bytes)?;
    let mut results = Vec::new();
    println!("\n== H1 dependent pointer chase ==");
    for &working_set in working_sets {
        let node_count = working_set / cache_line_bytes;
        if working_set_bytes(node_count, cache_line_bytes) != Some(working_set) {
            return Err(io::Error::other("H1 working set is not cache-line exact").into());
        }
        let cell_seed = mix64(config.seed ^ working_set as u64);
        let next = build_cycle(node_count, cell_seed);
        install_cycle(&mut buffer, &next, cache_line_bytes);
        black_box(chase(&buffer, cache_line_bytes, node_count as u64));
        let iterations = auto_scale_iterations_with(
            config.target,
            node_count as u64,
            u64::MAX / 4,
            |candidate| chase_duration(&buffer, cache_line_bytes, candidate).0,
        );
        let observations = repeat_measurements(config.repeats, || {
            let (elapsed, checksum) = chase_duration(&buffer, cache_line_bytes, iterations);
            black_box(checksum);
            elapsed.as_secs_f64() * 1e9 / iterations as f64
        });
        let stats = Statistics::new(&observations);
        let result = H1Result {
            working_set,
            node_count,
            iterations,
            seed: cell_seed,
            observations,
            stats,
        };
        print_h1_machine(&result, cache_line_bytes, taint);
        results.push(result);
    }
    println!("working-set  nodes       hops         mean ns/hop  RSD");
    for result in results {
        println!(
            "{:>10}  {:>10}  {:>11}  {:>12.3}  {:>6.3}%",
            format_bytes(result.working_set),
            result.node_count,
            result.iterations,
            result.stats.mean,
            result.stats.rsd_percent
        );
    }
    Ok(())
}

struct H1Result {
    working_set: usize,
    node_count: usize,
    iterations: u64,
    seed: u64,
    observations: Vec<f64>,
    stats: Statistics,
}

fn print_h1_machine(result: &H1Result, cache_line_bytes: usize, taint: &TaintCheck) {
    println!(
        "MEMORY_GRAPH_H1 working_set_bytes={} cache_line_bytes={} nodes={} seed=0x{:016x} iterations={} repeat_ns_per_hop={} mean_ns_per_hop={:.6} rsd_percent={:.6} load1={} taint={}",
        result.working_set,
        cache_line_bytes,
        result.node_count,
        result.seed,
        result.iterations,
        format_observations(&result.observations),
        result.stats.mean,
        result.stats.rsd_percent,
        format_load1(taint.load1),
        format_taint_labels(&taint.taints),
    );
}

fn chase(buffer: &AlignedBuffer, cache_line_bytes: usize, hops: u64) -> usize {
    let mut current = 0_usize;
    let base = buffer.as_ptr();
    for _ in 0..hops {
        // SAFETY: every installed successor is a valid node in the allocated working set.
        current = unsafe {
            base.add(current * cache_line_bytes)
                .cast::<u64>()
                .read_unaligned() as usize
        };
    }
    black_box(current)
}

fn chase_duration(buffer: &AlignedBuffer, cache_line_bytes: usize, hops: u64) -> (Duration, usize) {
    let start = Instant::now();
    let checksum = chase(buffer, cache_line_bytes, hops);
    (start.elapsed(), checksum)
}

fn run_h2(
    config: &Config,
    cache_line_bytes: usize,
    taint: &TaintCheck,
) -> Result<(), Box<dyn Error>> {
    let working_set = config.h2_working_set();
    let node_count = working_set / cache_line_bytes;
    if working_set_bytes(node_count, cache_line_bytes) != Some(working_set) {
        return Err(io::Error::other("H2 working set is not cache-line exact").into());
    }
    let mut buffer = AlignedBuffer::new(working_set, cache_line_bytes)?;
    let order_seed = mix64(config.seed ^ 0x4832);
    let order = shuffled_order(node_count, order_seed);
    let mut results = Vec::new();
    let mut baseline_ns = None;
    println!("\n== H2 interleaved independent chains ==");
    for chain_count in 1..=config.h2_max_chains() {
        let heads = install_partitioned_cycles(&mut buffer, &order, chain_count, cache_line_bytes);
        let minimum_rounds = node_count.div_ceil(chain_count) as u64;
        black_box(interleaved_chase(
            &buffer,
            cache_line_bytes,
            &heads,
            minimum_rounds,
        ));
        let iterations = auto_scale_iterations_with(
            config.target,
            minimum_rounds,
            u64::MAX / chain_count as u64,
            |candidate| interleaved_duration(&buffer, cache_line_bytes, &heads, candidate).0,
        );
        let total_hops = iterations.saturating_mul(chain_count as u64);
        let observations = repeat_measurements(config.repeats, || {
            let (elapsed, checksum) =
                interleaved_duration(&buffer, cache_line_bytes, &heads, iterations);
            black_box(checksum);
            elapsed.as_secs_f64() * 1e9 / total_hops as f64
        });
        let stats = Statistics::new(&observations);
        let baseline = *baseline_ns.get_or_insert(stats.mean);
        let speedup = baseline / stats.mean;
        let result = H2Result {
            working_set,
            chain_count,
            iterations,
            total_hops,
            seed: order_seed,
            observations,
            stats,
            speedup,
        };
        print_h2_machine(&result, cache_line_bytes, taint);
        results.push(result);
    }
    println!("chains  rounds       total hops    mean ns/hop  speedup  RSD");
    for result in results {
        println!(
            "{:>6}  {:>11}  {:>13}  {:>12.3}  {:>7.3}x  {:>6.3}%",
            result.chain_count,
            result.iterations,
            result.total_hops,
            result.stats.mean,
            result.speedup,
            result.stats.rsd_percent
        );
    }
    Ok(())
}

struct H2Result {
    working_set: usize,
    chain_count: usize,
    iterations: u64,
    total_hops: u64,
    seed: u64,
    observations: Vec<f64>,
    stats: Statistics,
    speedup: f64,
}

fn print_h2_machine(result: &H2Result, cache_line_bytes: usize, taint: &TaintCheck) {
    println!(
        "MEMORY_GRAPH_H2 working_set_bytes={} cache_line_bytes={} chains={} seed=0x{:016x} rounds={} total_hops={} repeat_ns_per_hop={} mean_ns_per_hop={:.6} rsd_percent={:.6} speedup_over_k1={:.6} load1={} taint={}",
        result.working_set,
        cache_line_bytes,
        result.chain_count,
        result.seed,
        result.iterations,
        result.total_hops,
        format_observations(&result.observations),
        result.stats.mean,
        result.stats.rsd_percent,
        result.speedup,
        format_load1(taint.load1),
        format_taint_labels(&taint.taints),
    );
}

fn install_partitioned_cycles(
    buffer: &mut AlignedBuffer,
    order: &[usize],
    chain_count: usize,
    cache_line_bytes: usize,
) -> Vec<usize> {
    let mut heads = Vec::with_capacity(chain_count);
    let base_len = order.len() / chain_count;
    let remainder = order.len() % chain_count;
    let mut start = 0_usize;
    for chain in 0..chain_count {
        let len = base_len + usize::from(chain < remainder);
        let end = start + len;
        let nodes = &order[start..end];
        let first = nodes[0];
        heads.push(first);
        for pair in nodes.windows(2) {
            buffer.write_u64(pair[0] * cache_line_bytes, pair[1] as u64);
        }
        buffer.write_u64(nodes[len - 1] * cache_line_bytes, first as u64);
        start = end;
    }
    heads
}

fn interleaved_chase(
    buffer: &AlignedBuffer,
    cache_line_bytes: usize,
    heads: &[usize],
    rounds: u64,
) -> usize {
    let base = buffer.as_ptr();
    let mut current = heads.to_vec();
    for _ in 0..rounds {
        for node in &mut current {
            // SAFETY: every partition successor is within the buffer's node set.
            *node = unsafe {
                base.add(*node * cache_line_bytes)
                    .cast::<u64>()
                    .read_unaligned() as usize
            };
        }
    }
    black_box(current.into_iter().fold(0_usize, usize::wrapping_add))
}

fn interleaved_duration(
    buffer: &AlignedBuffer,
    cache_line_bytes: usize,
    heads: &[usize],
    rounds: u64,
) -> (Duration, usize) {
    let start = Instant::now();
    let checksum = interleaved_chase(buffer, cache_line_bytes, heads, rounds);
    (start.elapsed(), checksum)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HopArm {
    Ooo,
    Prefetched,
    AddressSerialized,
}

impl HopArm {
    const ALL: [Self; 3] = [Self::Ooo, Self::Prefetched, Self::AddressSerialized];

    const fn label(self) -> &'static str {
        match self {
            Self::Ooo => "ooo",
            Self::Prefetched => "prefetched",
            Self::AddressSerialized => "address_serialized",
        }
    }
}

struct HopFixture {
    buffer: AlignedBuffer,
    cache_line_bytes: usize,
    adjacency_lines: usize,
    node_start_line: usize,
    node_lines: usize,
}

impl HopFixture {
    fn new(working_set: usize, cache_line_bytes: usize, seed: u64) -> Result<Self, Box<dyn Error>> {
        let total_lines = working_set / cache_line_bytes;
        if working_set_bytes(total_lines, cache_line_bytes) != Some(working_set) {
            return Err(io::Error::other("H3 working set is not cache-line exact").into());
        }
        let adjacency_lines = total_lines / 32;
        let node_start_line = adjacency_lines;
        let node_lines = total_lines.saturating_sub(adjacency_lines);
        if adjacency_lines < 2 || node_lines > u32::MAX as usize {
            return Err(io::Error::other("H3 fixture line counts are unsupported").into());
        }
        let mut buffer = AlignedBuffer::new(working_set, cache_line_bytes)?;
        let mut rng = SplitMix64::new(seed);
        for adjacency in 0..adjacency_lines {
            let offset = adjacency * cache_line_bytes;
            for lane in 0..ADJACENCY_SEED_COUNT {
                buffer.write_u32(
                    offset + lane * 4,
                    reduce_to_range(rng.next(), node_lines) as u32,
                );
            }
        }
        let adjacency_order = shuffled_order(adjacency_lines, rng.next());
        for pair in adjacency_order.windows(2) {
            buffer.write_u32(
                pair[0] * cache_line_bytes + cache_line_bytes - 4,
                pair[1] as u32,
            );
        }
        let first = adjacency_order[0];
        let last = adjacency_order[adjacency_order.len() - 1];
        buffer.write_u32(last * cache_line_bytes + cache_line_bytes - 4, first as u32);
        for node in 0..node_lines {
            let offset = (node_start_line + node) * cache_line_bytes;
            for word in 1..8 {
                buffer.write_u64(offset + word * 8, rng.next());
            }
        }
        let node_order = shuffled_order(node_lines, rng.next());
        for pair in node_order.windows(2) {
            buffer.write_u64(
                (node_start_line + pair[0]) * cache_line_bytes,
                pair[1] as u64,
            );
        }
        let first_node = node_order[0];
        let last_node = node_order[node_order.len() - 1];
        buffer.write_u64(
            (node_start_line + last_node) * cache_line_bytes,
            first_node as u64,
        );
        // The serialized anti-pattern must not collapse into hot attractor cycles. Each adjacency
        // starts at the beginning of a disjoint 31-node prefix of the one full node cycle, so the
        // minimum pass covers the entire node region once before reuse. This invariant is tested.
        for adjacency in 0..adjacency_lines {
            buffer.write_u32(
                adjacency * cache_line_bytes,
                node_order[adjacency * ADJACENCY_SEED_COUNT] as u32,
            );
        }
        Ok(Self {
            buffer,
            cache_line_bytes,
            adjacency_lines,
            node_start_line,
            node_lines,
        })
    }

    const fn working_set(&self) -> usize {
        self.buffer.len()
    }

    fn adjacency_ptr(&self, adjacency: usize) -> *const u8 {
        // SAFETY: adjacency is always a successor from the installed adjacency cycle.
        unsafe { self.buffer.as_ptr().add(adjacency * self.cache_line_bytes) }
    }

    fn node_ptr(&self, node: usize) -> *const u8 {
        // SAFETY: node indices are reduced to node_lines and the node region is in-buffer.
        unsafe {
            self.buffer
                .as_ptr()
                .add((self.node_start_line + node) * self.cache_line_bytes)
        }
    }

    fn derive_node(&self, adjacency: *const u8, rank: usize) -> usize {
        let lane = rank % ADJACENCY_SEED_COUNT;
        // SAFETY: lane is within the first 124 bytes of the adjacency cache line.
        let seed = unsafe { adjacency.add(lane * 4).cast::<u32>().read_unaligned() };
        if rank < ADJACENCY_SEED_COUNT {
            seed as usize
        } else {
            reduce_to_range(
                mix64(u64::from(seed) ^ (rank as u64).wrapping_mul(0x9e37_79b9)),
                self.node_lines,
            )
        }
    }

    fn next_adjacency(&self, adjacency: *const u8) -> usize {
        // SAFETY: the final u32 lies within the adjacency cache line.
        unsafe {
            adjacency
                .add(self.cache_line_bytes - 4)
                .cast::<u32>()
                .read_unaligned() as usize
        }
    }
}

fn run_h3(
    config: &Config,
    cache_line_bytes: usize,
    taint: &TaintCheck,
) -> Result<(), Box<dyn Error>> {
    println!("\n== H3 graph-hop model ==");
    let mut all_results = Vec::new();
    for &working_set in config.h3_working_sets() {
        let fixture_seed = mix64(config.seed ^ 0x4833 ^ working_set as u64);
        let fixture = HopFixture::new(working_set, cache_line_bytes, fixture_seed)?;
        for &rank in config.h3_ranks() {
            let mut ooo_mean = None;
            for arm in HopArm::ALL {
                black_box(measure_hops(
                    &fixture,
                    rank,
                    arm,
                    fixture.adjacency_lines as u64,
                ));
                let iterations = auto_scale_iterations_with(
                    config.target,
                    fixture.adjacency_lines as u64,
                    u64::MAX / 4,
                    |candidate| measure_hops(&fixture, rank, arm, candidate).0,
                );
                let observations = repeat_measurements(config.repeats, || {
                    let (elapsed, checksum) = measure_hops(&fixture, rank, arm, iterations);
                    black_box(checksum);
                    elapsed.as_secs_f64() * 1e9 / iterations as f64
                });
                let stats = Statistics::new(&observations);
                let baseline = *ooo_mean.get_or_insert(stats.mean);
                let ratio = match arm {
                    HopArm::Ooo => 1.0,
                    HopArm::Prefetched => baseline / stats.mean,
                    HopArm::AddressSerialized => stats.mean / baseline,
                };
                let result = H3Result {
                    working_set: fixture.working_set(),
                    adjacency_lines: fixture.adjacency_lines,
                    node_lines: fixture.node_lines,
                    rank,
                    arm,
                    iterations,
                    seed: fixture_seed,
                    observations,
                    stats,
                    ratio,
                };
                print_h3_machine(&result, cache_line_bytes, taint);
                all_results.push(result);
            }
        }
    }
    println!("working-set  R   arm                 hops       mean ns/hop  vs OoO   RSD");
    for result in all_results {
        let ratio_label = match result.arm {
            HopArm::Ooo => String::from("1.000x"),
            HopArm::Prefetched => format!("{:.3}x speedup", result.ratio),
            HopArm::AddressSerialized => format!("{:.3}x slower", result.ratio),
        };
        println!(
            "{:>10}  {:>2}  {:<19}  {:>9}  {:>12.3}  {:>13}  {:>6.3}%",
            format_bytes(result.working_set),
            result.rank,
            result.arm.label(),
            result.iterations,
            result.stats.mean,
            ratio_label,
            result.stats.rsd_percent
        );
    }
    Ok(())
}

struct H3Result {
    working_set: usize,
    adjacency_lines: usize,
    node_lines: usize,
    rank: usize,
    arm: HopArm,
    iterations: u64,
    seed: u64,
    observations: Vec<f64>,
    stats: Statistics,
    ratio: f64,
}

fn print_h3_machine(result: &H3Result, cache_line_bytes: usize, taint: &TaintCheck) {
    let ratio_name = match result.arm {
        HopArm::Ooo => "baseline_ratio",
        HopArm::Prefetched => "speedup_over_ooo",
        HopArm::AddressSerialized => "penalty_over_ooo",
    };
    println!(
        "MEMORY_GRAPH_H3 working_set_bytes={} cache_line_bytes={} adjacency_lines={} node_lines={} rank={} arm={} seed=0x{:016x} iterations={} repeat_ns_per_hop={} mean_ns_per_hop={:.6} rsd_percent={:.6} {}={:.6} load1={} taint={}",
        result.working_set,
        cache_line_bytes,
        result.adjacency_lines,
        result.node_lines,
        result.rank,
        result.arm.label(),
        result.seed,
        result.iterations,
        format_observations(&result.observations),
        result.stats.mean,
        result.stats.rsd_percent,
        ratio_name,
        result.ratio,
        format_load1(taint.load1),
        format_taint_labels(&taint.taints),
    );
}

fn measure_hops(
    fixture: &HopFixture,
    rank: usize,
    arm: HopArm,
    iterations: u64,
) -> (Duration, u64) {
    debug_assert!(rank <= MAX_RANK);
    let mut current_adjacency = 0_usize;
    let mut checksum = 0_u64;
    let start = Instant::now();
    for _ in 0..iterations {
        let adjacency = fixture.adjacency_ptr(current_adjacency);
        checksum = checksum.wrapping_add(match arm {
            HopArm::Ooo => gather_ooo(fixture, adjacency, rank),
            HopArm::Prefetched => gather_prefetched(fixture, adjacency, rank),
            HopArm::AddressSerialized => gather_serialized(fixture, adjacency, rank),
        });
        current_adjacency = fixture.next_adjacency(adjacency);
    }
    let elapsed = start.elapsed();
    black_box((checksum, current_adjacency));
    (elapsed, checksum ^ current_adjacency as u64)
}

fn gather_ooo(fixture: &HopFixture, adjacency: *const u8, rank: usize) -> u64 {
    let mut addresses = [std::ptr::null::<u8>(); MAX_RANK];
    for (candidate, address) in addresses[..rank].iter_mut().enumerate() {
        *address = fixture.node_ptr(fixture.derive_node(adjacency, candidate));
    }
    let mut checksum = 0_u64;
    for &address in &addresses[..rank] {
        // SAFETY: every derived node pointer names a full 128-byte node block.
        checksum = checksum.wrapping_add(unsafe { touch_block(address).0 });
    }
    checksum
}

fn gather_prefetched(fixture: &HopFixture, adjacency: *const u8, rank: usize) -> u64 {
    let mut addresses = [std::ptr::null::<u8>(); MAX_RANK];
    for (candidate, address) in addresses[..rank].iter_mut().enumerate() {
        *address = fixture.node_ptr(fixture.derive_node(adjacency, candidate));
        prefetch(*address);
    }
    let mut checksum = 0_u64;
    for &address in &addresses[..rank] {
        // SAFETY: every derived node pointer names a full 128-byte node block.
        checksum = checksum.wrapping_add(unsafe { touch_block(address).0 });
    }
    checksum
}

fn gather_serialized(fixture: &HopFixture, adjacency: *const u8, rank: usize) -> u64 {
    let mut node = fixture.derive_node(adjacency, 0);
    let mut checksum = 0_u64;
    for _ in 0..rank {
        // SAFETY: node is derived from either a valid adjacency seed or an in-block value reduced
        // to the fixture's node count.
        let (sum, next_word) = unsafe { touch_block(fixture.node_ptr(node)) };
        checksum = checksum.wrapping_add(sum);
        node = next_word as usize;
        debug_assert!(node < fixture.node_lines);
    }
    checksum ^ node as u64
}

#[cfg(target_arch = "aarch64")]
fn prefetch(address: *const u8) {
    // SAFETY: PRFM is a non-faulting hint and address is a valid in-fixture block pointer.
    unsafe {
        asm!(
            "prfm pldl1keep, [{address}]",
            address = in(reg) address,
            options(readonly, nostack, preserves_flags),
        );
    }
}

#[cfg(not(target_arch = "aarch64"))]
fn prefetch(_address: *const u8) {}

unsafe fn touch_block(address: *const u8) -> (u64, u64) {
    // SAFETY: caller guarantees at least the first 64 bytes of a node block are addressable.
    let first = unsafe { address.cast::<u64>().read_unaligned() };
    let mut sum = first;
    for word in 1..8 {
        // SAFETY: word spans bytes 8..64 within the promised block prefix.
        sum = sum.wrapping_add(unsafe { address.add(word * 8).cast::<u64>().read_unaligned() });
    }
    (sum, first)
}

fn auto_scale_iterations_with<F>(
    target: Duration,
    minimum: u64,
    maximum: u64,
    mut measure: F,
) -> u64
where
    F: FnMut(u64) -> Duration,
{
    let mut candidate = minimum.max(1).min(maximum);
    for _ in 0..MAX_AUTOSCALE_STEPS {
        let elapsed = measure(candidate);
        if elapsed >= target || candidate == maximum {
            return candidate;
        }
        let elapsed_nanos = elapsed.as_nanos().max(1);
        let target_nanos = target.as_nanos();
        let scaled = (u128::from(candidate) * target_nanos)
            .div_ceil(elapsed_nanos)
            .saturating_mul(105)
            .div_ceil(100)
            .min(u128::from(maximum)) as u64;
        candidate = scaled.max(candidate.saturating_add(1)).min(maximum);
    }
    candidate
}

fn repeat_measurements<F>(repeats: usize, mut measure: F) -> Vec<f64>
where
    F: FnMut() -> f64,
{
    (0..repeats).map(|_| measure()).collect()
}

#[derive(Clone, Copy, Debug)]
struct Statistics {
    mean: f64,
    rsd_percent: f64,
}

impl Statistics {
    fn new(observations: &[f64]) -> Self {
        let mean = observations.iter().sum::<f64>() / observations.len() as f64;
        let squared_error = observations
            .iter()
            .map(|value| (value - mean).powi(2))
            .sum::<f64>();
        let sample_variance = squared_error / observations.len().saturating_sub(1).max(1) as f64;
        Self {
            mean,
            rsd_percent: sample_variance.sqrt() / mean * 100.0,
        }
    }
}

fn format_observations(observations: &[f64]) -> String {
    observations
        .iter()
        .map(|value| format!("{value:.6}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn format_bytes(bytes: usize) -> String {
    if bytes >= GIB && bytes.is_multiple_of(GIB) {
        format!("{}GiB", bytes / GIB)
    } else if bytes >= MIB && bytes.is_multiple_of(MIB) {
        format!("{}MiB", bytes / MIB)
    } else {
        format!("{}KiB", bytes / KIB)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::time::Duration;

    use super::{
        AlignedBuffer, Config, Harness, HopFixture, Statistics, auto_scale_iterations_with,
        build_cycle, gather_serialized, working_set_bytes,
    };

    #[test]
    fn chain_construction_is_one_full_cycle_without_early_repetition() {
        let next = build_cycle(257, 0x5eed_cafe);
        let mut visited = HashSet::new();
        let mut current = 0_usize;
        for _ in 0..next.len() {
            assert!(
                visited.insert(current),
                "chain repeated before visiting every node"
            );
            current = next[current];
        }
        assert_eq!(current, 0, "chain must close only after every node");
        assert_eq!(visited.len(), next.len());
    }

    #[test]
    fn working_set_size_is_exactly_the_claimed_line_count() {
        assert_eq!(working_set_bytes(8_192, 128), Some(1_048_576));
        assert_eq!(working_set_bytes(16_384, 64), Some(1_048_576));
        let allocation = AlignedBuffer::new(1_048_576, 128).expect("aligned test allocation");
        assert_eq!(allocation.len(), 1_048_576);
        assert_eq!(allocation.as_ptr().addr() % 128, 0);
    }

    #[test]
    fn address_serialized_next_block_derives_from_current_block_contents() {
        let mut fixture =
            HopFixture::new(1_048_576, 128, 0xc0de).expect("address-serialized fixture");
        fixture.buffer.write_u32(0, 0);
        for node in 0..fixture.node_lines {
            for word in 0..8 {
                fixture
                    .buffer
                    .write_u64((fixture.node_start_line + node) * 128 + word * 8, 0);
            }
        }
        for (node, next) in [1_u64, 2, 3, 0].into_iter().enumerate() {
            fixture
                .buffer
                .write_u64((fixture.node_start_line + node) * 128, next);
        }
        let adjacency = fixture.adjacency_ptr(0);
        assert_eq!(gather_serialized(&fixture, adjacency, 4), 6);

        fixture
            .buffer
            .write_u64((fixture.node_start_line + 2) * 128, 0);
        assert_eq!(
            gather_serialized(&fixture, adjacency, 4),
            5,
            "changing block 2 contents must change the next address used by gather_serialized"
        );
    }

    #[test]
    fn address_serialized_fixture_is_one_full_cycle_without_hot_attractors() {
        let fixture = HopFixture::new(1_048_576, 128, 0x5eed).expect("small hop fixture");
        let mut visited = HashSet::new();
        let mut current = 0_usize;
        for _ in 0..fixture.node_lines {
            assert!(
                visited.insert(current),
                "serialized node chain repeated before covering the working set"
            );
            let pointer = fixture.node_ptr(current);
            // SAFETY: node_ptr identifies a complete initialized node block.
            let encoded = unsafe { pointer.cast::<u64>().read_unaligned() };
            current = encoded as usize;
        }
        assert_eq!(current, 0);
        assert_eq!(visited.len(), fixture.node_lines);
    }

    #[test]
    fn address_serialized_starts_cover_every_node_once_before_reuse() {
        let fixture = HopFixture::new(1_048_576, 128, 0xcafe).expect("small hop fixture");
        let mut visited = HashSet::new();
        for adjacency_index in 0..fixture.adjacency_lines {
            let adjacency = fixture.adjacency_ptr(adjacency_index);
            let mut current = fixture.derive_node(adjacency, 0);
            for _ in 0..super::ADJACENCY_SEED_COUNT {
                assert!(
                    visited.insert(current),
                    "serialized minimum-pass schedule reused a node before full coverage"
                );
                let pointer = fixture.node_ptr(current);
                // SAFETY: node_ptr identifies a complete initialized node block.
                current = unsafe { pointer.cast::<u64>().read_unaligned() } as usize;
            }
        }
        assert_eq!(visited.len(), fixture.node_lines);
    }

    #[test]
    fn iteration_auto_scaling_terminates_at_a_positive_bounded_count() {
        let mut calls = 0_u64;
        let iterations =
            auto_scale_iterations_with(Duration::from_millis(100), 1, 1_000_000, |candidate| {
                calls += 1;
                Duration::from_nanos(candidate.saturating_mul(100))
            });
        assert!((900_000..=1_000_000).contains(&iterations));
        assert!(
            calls <= 12,
            "auto-scaling must have a fixed termination bound"
        );
    }

    #[test]
    fn cli_requires_three_repeats_and_parses_explicit_seed() {
        let valid = [
            "h3",
            "--seed",
            "0x2a",
            "--repeats",
            "4",
            "--target-ms",
            "7",
            "--cache-line-bytes",
            "128",
        ]
        .map(String::from);
        let config = Config::parse(&valid).expect("valid memory-graph CLI");
        assert_eq!(config.harness, Harness::H3);
        assert_eq!(config.seed, 42);
        assert_eq!(config.repeats, 4);
        assert_eq!(config.target, Duration::from_millis(7));
        assert_eq!(config.cache_line_override, Some(128));

        let invalid = ["h1", "--repeats", "2"].map(String::from);
        assert!(Config::parse(&invalid).is_err());
    }

    #[test]
    fn relative_standard_deviation_uses_all_repeats() {
        let stats = Statistics::new(&[9.0, 10.0, 11.0]);
        assert_eq!(stats.mean, 10.0);
        assert!((stats.rsd_percent - 10.0).abs() < f64::EPSILON);
    }
}
