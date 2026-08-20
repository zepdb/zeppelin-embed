//! Full coarse-scoring throughput comparison across quantization schemes.

use std::error::Error;
use std::hint::black_box;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use zeppelin_embed::kernels::{self, KernelArm};
use zeppelin_embed::quant::QuantScheme;
use zeppelin_embed_bench::frontier::attestation::{
    CampaignPreflightOutcome, FileAttestationSource, MachineStateProvenance,
    default_attestation_path, preflight_with_attestation,
};
use zeppelin_embed_bench::frontier::measure::SystemMachineProbe;
use zeppelin_embed_bench::scheme_level::{
    BenchmarkConfig, EncodedCorpus, MIN_WORKING_SET_BYTES, MeasurementMetrics, PreparedQuery,
    SYSTEM_LEVEL_CACHE_BYTES, SchemeReport, WIDE_LOAD_MEMORY_CEILING_GBPS, enforce_cache_floor,
    parse_arguments, scheme_label, scoring_path,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("scheme-level: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let config = parse_arguments(&arguments)?;
    let selected_arm = kernels::initialize()?;
    print_protocol(&config, selected_arm);

    let root = repository_root()?;
    let attestation = FileAttestationSource::new(default_attestation_path(&root));
    let provenance = match preflight_with_attestation(&SystemMachineProbe, &attestation) {
        CampaignPreflightOutcome::Idle { reasons } => {
            for reason in &reasons {
                println!("PREFLIGHT IDLE: {reason}");
            }
            println!(
                "NOT MEASURED — machine-state preflight is unavailable or unfavorable; fail-closed with zero timing samples"
            );
            let reason = reasons.join("; ");
            for scheme in config.scheme.schemes() {
                print_not_measured(scheme, &config, selected_arm, &reason)?;
            }
            return Ok(());
        }
        CampaignPreflightOutcome::Ready {
            power_evidence,
            thermal_evidence,
            provenance,
        } => {
            println!("PREFLIGHT READY: {}", provenance_label(&provenance));
            println!("power evidence: {}", one_line(&power_evidence));
            println!("thermal evidence: {}", one_line(&thermal_evidence));
            provenance
        }
    };

    for scheme in config.scheme.schemes() {
        measure_one_scheme(scheme, &config, selected_arm, &provenance)?;
    }
    Ok(())
}

fn print_protocol(config: &BenchmarkConfig, selected_arm: KernelArm) {
    println!("SCHEME-LEVEL COARSE SCORING BENCHMARK");
    println!(
        "shape: rows={} dimension={} queries={} seed={} repeats={} scheme={:?}",
        config.rows, config.dimension, config.queries, config.seed, config.repeats, config.scheme
    );
    println!("runtime kernel dispatch arm: {selected_arm:?}");
    println!(
        "memory denominator: {:.6} GB/s (Task-02 four-accumulator wide-load single-reader median)",
        WIDE_LOAD_MEMORY_CEILING_GBPS
    );
    println!(
        "cache floor: {} B = 8 x {} B approximate system-level cache",
        MIN_WORKING_SET_BYTES, SYSTEM_LEVEL_CACHE_BYTES
    );
    println!(
        "cache/reuse defense: timing is refused unless a scheme's concrete code and factor buffers reach the cache floor; every byte is initialized before timing; each prepared query then streams every row before returning to row zero, so more than eight cache capacities intervene between reuse"
    );
    println!(
        "elision defense: each scorer writes one f32 per row; the complete output is checksummed after every timed query outside its interval, and corpus/query/output/checksum values pass through black_box"
    );
    println!(
        "traffic numerator: concrete encoded code plus per-row factor slice bytes only; prepared-query bytes and output stores are common across schemes and excluded from effective GB/s, although output writes remain inside the timed scoring path"
    );
    println!(
        "measurement authority: SINGLE_TENANT REQUIRED; this program's timing is a candidate until the orchestrator confirms isolation"
    );
}

fn print_not_measured(
    scheme: QuantScheme,
    config: &BenchmarkConfig,
    arm: KernelArm,
    reason: &str,
) -> Result<(), Box<dyn Error>> {
    let path = scoring_path(scheme, arm);
    println!("\n== {} ==", scheme_label(scheme));
    println!("status: NOT MEASURED / NOT AUTHORITATIVE");
    println!("bytes/row: NOT MEASURED");
    println!("working-set bytes: NOT MEASURED");
    println!("ns/row: NOT MEASURED");
    println!("effective GB/s: NOT MEASURED");
    println!("percent of wide-load ceiling: NOT MEASURED");
    print_path(path);
    println!("reason: {reason}");
    let report = SchemeReport::not_measured(scheme, config, path, reason);
    println!("{}", report.machine_summary_line()?);
    Ok(())
}

fn measure_one_scheme(
    scheme: QuantScheme,
    config: &BenchmarkConfig,
    arm: KernelArm,
    provenance: &MachineStateProvenance,
) -> Result<(), Box<dyn Error>> {
    println!("\n== {} ==", scheme_label(scheme));
    println!("fixture setup: encoding and fully materializing deterministic row buffers");
    let corpus = EncodedCorpus::synthetic(scheme, config.rows, config.dimension, config.seed)?;
    let buffers = corpus.encoded_buffer_bytes();
    let path = scoring_path(scheme, arm);
    println!(
        "bytes/row (actual code + factor slices): {} B",
        corpus.bytes_per_row()
    );
    println!("code buffer: {} B", buffers.code_bytes);
    println!("factor buffer: {} B", buffers.factor_bytes);
    println!("working-set bytes: {} B", buffers.total);
    println!(
        "working-set/cache multiple: {:.6}x",
        buffers.total as f64 / SYSTEM_LEVEL_CACHE_BYTES as f64
    );
    print_path(path);

    if let Err(error) = enforce_cache_floor(&corpus) {
        println!("status: NOT MEASURED / NOT AUTHORITATIVE");
        println!("ns/row: NOT MEASURED");
        println!("effective GB/s: NOT MEASURED");
        println!("percent of wide-load ceiling: NOT MEASURED");
        println!("reason: {error}");
        let report = SchemeReport::not_measured_with_buffers(
            scheme,
            config,
            path,
            buffers,
            error.to_string(),
        );
        println!("{}", report.machine_summary_line()?);
        return Ok(());
    }

    let queries = prepare_queries(scheme, config)?;
    let outcome = time_complete_sweeps(&corpus, &queries, config.repeats)?;
    let scored_rows = (config.rows as f64) * (config.queries as f64);
    let median_ns = median(outcome.repeat_elapsed_ns.clone());
    let ns_per_row = median_ns / scored_rows;
    let effective_gbps = buffers.total as f64 * config.queries as f64 / median_ns;
    let percent = effective_gbps / WIDE_LOAD_MEMORY_CEILING_GBPS * 100.0;
    let metrics = MeasurementMetrics {
        bytes_per_row: corpus.bytes_per_row(),
        working_set_bytes: buffers.total,
        ns_per_row,
        effective_gbps,
        percent_of_wide_load_ceiling: percent,
        checksum: outcome.checksum,
    };
    let provenance = provenance_label(provenance);
    let report =
        SchemeReport::measured_candidate(scheme, config, path, metrics, provenance.clone());

    println!("timing repeat elapsed ns: {:?}", outcome.repeat_elapsed_ns);
    println!("median ns/row: {ns_per_row:.6}");
    println!("effective GB/s: {effective_gbps:.6}");
    println!("percent of 80.689179 GB/s: {percent:.6}%");
    println!("score checksum: {}", outcome.checksum);
    println!("machine-state provenance: {provenance}");
    if percent > 100.0 {
        println!(
            "status: MEASUREMENT DEFECT — above 100% of the denominator; explain, never count as success"
        );
    } else {
        println!(
            "status: MEASURED CANDIDATE — SINGLE-TENANT REQUIRED before these timing values are authoritative"
        );
    }
    println!("{}", report.machine_summary_line()?);
    Ok(())
}

fn print_path(path: zeppelin_embed_bench::scheme_level::ScoringPath) {
    println!("scoring path: {}", path.label);
    println!(
        "native runtime-dispatched SIMD scorer: {}",
        path.native_runtime_dispatched_simd
    );
    println!(
        "expanded row materialized: {}; unpack cost: {} B/row",
        path.expanded_row_materialized, path.unpack_bytes_per_row
    );
}

fn prepare_queries(
    scheme: QuantScheme,
    config: &BenchmarkConfig,
) -> Result<Vec<PreparedQuery>, Box<dyn Error>> {
    let mut random = SplitMix64::new(config.seed ^ 0x7175_6572_795f_7365);
    let mut prepared = Vec::with_capacity(config.queries);
    for query_index in 0..config.queries {
        let mut values = Vec::with_capacity(config.dimension);
        for _ in 0..config.dimension {
            values.push(random.next_signed_f32());
        }
        let seed = config.seed
            ^ (u64::from(scheme.id()) << 56)
            ^ query_index as u64
            ^ 0x7072_6570_6172_6564;
        prepared.push(PreparedQuery::new(scheme, &values, seed)?);
    }
    Ok(prepared)
}

struct TimingOutcome {
    repeat_elapsed_ns: Vec<f64>,
    checksum: u64,
}

fn time_complete_sweeps(
    corpus: &EncodedCorpus,
    queries: &[PreparedQuery],
    repeats: usize,
) -> Result<TimingOutcome, Box<dyn Error>> {
    let mut repeat_elapsed_ns = Vec::with_capacity(repeats);
    let mut output = vec![0.0_f32; corpus.rows()];
    let mut checksum = 0x7363_6f72_655f_6f75_u64;
    for repeat in 0..repeats {
        let mut elapsed = Duration::ZERO;
        for (query_index, query) in queries.iter().enumerate() {
            let started = Instant::now();
            black_box(corpus).score_prepared(black_box(query), black_box(&mut output))?;
            elapsed += started.elapsed();
            let observed = checksum_scores(black_box(&output));
            checksum = checksum.rotate_left(9)
                ^ observed
                ^ (repeat as u64).rotate_left(17)
                ^ query_index as u64;
            black_box(checksum);
        }
        repeat_elapsed_ns.push(elapsed.as_secs_f64() * 1_000_000_000.0);
    }
    Ok(TimingOutcome {
        repeat_elapsed_ns,
        checksum,
    })
}

fn checksum_scores(scores: &[f32]) -> u64 {
    scores
        .iter()
        .enumerate()
        .fold(0xcbf2_9ce4_8422_2325_u64, |checksum, (index, score)| {
            checksum.rotate_left(5)
                ^ u64::from(score.to_bits())
                ^ (index as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
        })
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[middle - 1] + values[middle]) * 0.5
    } else {
        values[middle]
    }
}

#[derive(Clone, Copy)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_signed_f32(&mut self) -> f32 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^= value >> 31;
        let fraction = (value >> 40) as f32 * (1.0 / 16_777_216.0);
        fraction * 2.0 - 1.0
    }
}

fn provenance_label(provenance: &MachineStateProvenance) -> String {
    match provenance {
        MachineStateProvenance::DirectProbe => String::from("direct-probe"),
        MachineStateProvenance::OperatorAttestation {
            timestamp,
            machine_identifier,
        } => format!("operator-attestation:{timestamp}:{machine_identifier}"),
    }
}

fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn repository_root() -> Result<PathBuf, io::Error> {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| io::Error::other("bench crate is not nested under repository/crates"))
}
