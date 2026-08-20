//! Optimization-frontier harness command line.

use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use zeppelin_embed_bench::frontier::calibration::{
    CalibrationArtifact, CalibrationMachineContext, CalibrationRun, CalibrationTier,
    CalibrationWritePolicy, default_calibration_path, persist_calibration,
};
use zeppelin_embed_bench::frontier::ledger::{Ledger, LedgerSummary};
use zeppelin_embed_bench::frontier::measure::{
    MeasurementConfig, PreflightOutcome, StridedI8Workload, SyntheticI8Workload,
    SystemMachineProbe, Workload, WorkloadSampler, measure_source, preflight,
};
use zeppelin_embed_bench::frontier::roofline::{
    ComputeCalibrationConfig, ComputeCalibrationOutcome, WIDE_LOAD_SINGLE_CORE_GBPS,
    calibrate_compute_tiers,
};
use zeppelin_embed_bench::frontier::tune::{
    CandidateEvaluation, SearchConfig, SearchSpace, run_search,
};
use zeppelin_embed_bench::frontier::variants::VariantRegistry;

const DEFAULT_SEED: u64 = 0x27_2026_0820;

fn main() {
    if let Err(error) = run() {
        eprintln!("frontier: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    match arguments.first().map(String::as_str) {
        Some("tune") => run_tune(&arguments[1..]),
        Some("report") if arguments.len() == 1 => run_report(),
        Some("denominators") => run_denominators(&arguments[1..]),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: frontier tune --campaign kernels-i8 --smoke [--seed N] | report | denominators [--persist --date YYYY-MM-DD [--allow-lower-ceiling]]",
        )
        .into()),
    }
}

fn run_tune(arguments: &[String]) -> Result<(), Box<dyn Error>> {
    let mut campaign = None;
    let mut smoke = false;
    let mut seed = DEFAULT_SEED;
    let mut index = 0;
    while index < arguments.len() {
        match arguments.get(index).map(String::as_str) {
            Some("--campaign") => {
                campaign = arguments.get(index + 1).cloned();
                index += 2;
            }
            Some("--smoke") => {
                smoke = true;
                index += 1;
            }
            Some("--seed") => {
                let raw = arguments.get(index + 1).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--seed requires a value")
                })?;
                seed = raw.parse::<u64>().map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("invalid seed: {error}"),
                    )
                })?;
                index += 2;
            }
            Some(other) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown tune argument {other}"),
                )
                .into());
            }
            None => break,
        }
    }
    if campaign.as_deref() != Some("kernels-i8") || !smoke {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "27-H exposes only the non-campaign smoke: tune --campaign kernels-i8 --smoke",
        )
        .into());
    }
    let registry = VariantRegistry::from_kernel_knob_space()?;
    println!("campaign: kernels-i8 (HARNESS SMOKE; not B1)");
    println!("seed: {seed}");
    println!("declared knob points: {}", registry.declared_points().len());
    println!(
        "materialized task-03 builds: {}",
        registry.materialized().len()
    );
    println!("workload: synthetic-contiguous (PROVISIONAL)");
    println!("workload: synthetic-strided (PROVISIONAL)");
    match preflight(&SystemMachineProbe) {
        PreflightOutcome::Idle { reasons } => {
            for reason in reasons {
                println!("PREFLIGHT IDLE: {reason}");
            }
            println!("SMOKE IDLE: fail-closed; zero timings and zero ledger rows produced");
            Ok(())
        }
        PreflightOutcome::Ready { .. } => run_ready_smoke(&registry, seed),
    }
}

fn run_ready_smoke(registry: &VariantRegistry, seed: u64) -> Result<(), Box<dyn Error>> {
    let points = registry
        .materialized()
        .iter()
        .map(|variant| variant.point())
        .collect::<Vec<_>>();
    let space = SearchSpace::new(points)?;
    let config = SearchConfig {
        seed,
        exhaustive_limit: registry.materialized().len().max(1),
        random_restarts: 1,
        maximum_evaluations: registry.materialized().len(),
    };
    let mut contiguous = SyntheticI8Workload::new("synthetic-contiguous", 8_192, 768, seed)?;
    let mut strided = StridedI8Workload::new("synthetic-strided", 8_192, 768, 3, seed)?;
    let mut evaluation_error = None;
    let search = run_search(&space, config, None, |point| {
        let Some(variant) = registry
            .materialized()
            .iter()
            .find(|candidate| candidate.point() == point)
        else {
            evaluation_error = Some("materialized point disappeared from registry".to_owned());
            return CandidateEvaluation::incorrect(point.stable_id(), f64::MAX);
        };
        let correctness = contiguous
            .execute(variant)
            .map(|observation| observation.correct)
            .and_then(|left| {
                strided
                    .execute(variant)
                    .map(|observation| left && observation.correct)
            });
        let correctness_green = match correctness {
            Ok(correctness_green) => correctness_green,
            Err(error) => {
                evaluation_error = Some(error.to_string());
                false
            }
        };
        let mut sampler = WorkloadSampler::new(&mut contiguous, variant);
        match measure_source(&mut sampler, MeasurementConfig::strict()) {
            Ok(measurement) if correctness_green => {
                CandidateEvaluation::correct(point.stable_id(), measurement.min_of_medians_ns)
            }
            Ok(measurement) => {
                CandidateEvaluation::incorrect(point.stable_id(), measurement.min_of_medians_ns)
            }
            Err(error) => {
                evaluation_error = Some(error.to_string());
                CandidateEvaluation::incorrect(point.stable_id(), f64::MAX)
            }
        }
    })?;
    if let Some(error) = evaluation_error {
        return Err(io::Error::other(error).into());
    }
    println!("trajectory: {}", search.trajectory.join(" -> "));
    if let Some(winner) = search.ranking.first() {
        println!(
            "smoke winner: {} at {:.3} ns (PROVISIONAL; no ledger row)",
            winner.variant, winner.median_ns
        );
    }
    println!("SMOKE GREEN: tiny existing-build grid only; B1 was not run");
    Ok(())
}

fn run_report() -> Result<(), Box<dyn Error>> {
    let directory = repository_root().join("tasks/evidence/opt-ledger");
    if !directory.exists() {
        print_summary(LedgerSummary::default(), 0);
        return Ok(());
    }
    let mut rows = Vec::new();
    let mut files = 0;
    for entry in fs::read_dir(&directory)? {
        let path = entry?.path();
        if !path.is_file() {
            continue;
        }
        let ledger = Ledger::open(&path)?;
        rows.extend(ledger.rows()?);
        files += 1;
    }
    print_summary(LedgerSummary::from_rows(&rows), files);
    Ok(())
}

fn print_summary(summary: LedgerSummary, files: usize) {
    println!("frontier ledger report");
    println!("ledger files: {files}");
    println!("rows: {}", summary.rows);
    println!("keep: {}", summary.keeps);
    println!("revert: {}", summary.reverts);
    println!("complete (PMU-attributed): {}", summary.complete);
    println!("frontier-open: {}", summary.frontier_open);
    println!("provisional synthetic-only: {}", summary.provisional);
}

fn run_denominators(arguments: &[String]) -> Result<(), Box<dyn Error>> {
    let options = parse_denominator_arguments(arguments)?;
    println!("memory one-core wide-load denominator: {WIDE_LOAD_SINGLE_CORE_GBPS:.6} GB/s");
    println!(
        "memory provenance command: cargo run --release -p zeppelin-embed-bench --bin platform-truth -- bandwidth-compare"
    );
    match calibrate_compute_tiers(&SystemMachineProbe, ComputeCalibrationConfig::evidence())? {
        ComputeCalibrationOutcome::Idle { reasons } => {
            for reason in reasons {
                println!("COMPUTE DENOMINATORS NOT MEASURED: {reason}");
            }
            println!("CALIBRATION IDLE: fail-closed; no timing loop executed");
        }
        ComputeCalibrationOutcome::Measured {
            calibrations,
            not_measured,
        } => {
            for calibration in &calibrations {
                println!(
                    "{}: {:.6} GMAC/s raw_medians_ns={:?} checksum={}",
                    calibration.tier.as_str(),
                    calibration.operations_per_second / 1_000_000_000.0,
                    calibration.measurement.accepted_run_medians_ns,
                    calibration.checksum
                );
            }
            for (tier, reason) in not_measured {
                println!("{}: {reason}", tier.as_str());
            }
            if options.persist {
                let date = options.date.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--persist requires caller-supplied --date YYYY-MM-DD",
                    )
                })?;
                let tiers = calibrations
                    .iter()
                    .map(|calibration| {
                        CalibrationTier::new(
                            calibration.tier,
                            calibration.operations_per_second / 1_000_000_000.0,
                            vec![CalibrationRun {
                                run: 1,
                                gmac_per_second: calibration.operations_per_second
                                    / 1_000_000_000.0,
                                raw_medians_ns: calibration
                                    .measurement
                                    .accepted_run_medians_ns
                                    .clone(),
                                checksum: calibration.checksum,
                            }],
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let command = denominator_command(date, options.allow_lower_ceiling);
                let artifact =
                    CalibrationArtifact::new(current_machine_context()?, date, command, tiers)?;
                let policy = if options.allow_lower_ceiling {
                    CalibrationWritePolicy::AllowLower {
                        reason: "operator explicitly passed --allow-lower-ceiling".to_owned(),
                    }
                } else {
                    CalibrationWritePolicy::UpwardOnly
                };
                let path = default_calibration_path();
                persist_calibration(&path, &artifact, policy)?;
                println!("PERSISTED COMPUTE CALIBRATION: {}", path.display());
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct DenominatorArguments<'a> {
    persist: bool,
    date: Option<&'a str>,
    allow_lower_ceiling: bool,
}

fn parse_denominator_arguments(
    arguments: &[String],
) -> Result<DenominatorArguments<'_>, io::Error> {
    let mut options = DenominatorArguments::default();
    let mut index = 0;
    while index < arguments.len() {
        match arguments.get(index).map(String::as_str) {
            Some("--persist") => {
                options.persist = true;
                index += 1;
            }
            Some("--date") => {
                options.date = Some(arguments.get(index + 1).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--date requires YYYY-MM-DD")
                })?);
                index += 2;
            }
            Some("--allow-lower-ceiling") => {
                options.allow_lower_ceiling = true;
                index += 1;
            }
            Some(other) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown denominators argument {other}"),
                ));
            }
            None => break,
        }
    }
    if (options.date.is_some() || options.allow_lower_ceiling) && !options.persist {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--date and --allow-lower-ceiling apply only with --persist",
        ));
    }
    if options.persist && options.date.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--persist requires caller-supplied --date YYYY-MM-DD",
        ));
    }
    Ok(options)
}

fn denominator_command(date: &str, allow_lower_ceiling: bool) -> String {
    let override_flag = if allow_lower_ceiling {
        " --allow-lower-ceiling"
    } else {
        ""
    };
    format!(
        "cargo run --release -p zeppelin-embed-bench --bin frontier -- denominators --persist --date {date}{override_flag}"
    )
}

fn current_machine_context() -> Result<CalibrationMachineContext, io::Error> {
    let hardware = command_stdout("system_profiler", &["SPHardwareDataType"])?;
    Ok(CalibrationMachineContext {
        model_name: profiler_field(&hardware, "Model Name")?,
        model_identifier: profiler_field(&hardware, "Model Identifier")?,
        chip: profiler_field(&hardware, "Chip")?,
        os_product_version: command_stdout("sw_vers", &["-productVersion"])?
            .trim()
            .to_owned(),
        os_build: command_stdout("sw_vers", &["-buildVersion"])?
            .trim()
            .to_owned(),
        power_state: "AC Power established by fail-closed pmset preflight".to_owned(),
        thermal_state: "Nominal thermal state established by fail-closed pmset preflight"
            .to_owned(),
    })
}

fn command_stdout(program: &str, arguments: &[&str]) -> Result<String, io::Error> {
    let output = std::process::Command::new(program)
        .args(arguments)
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn profiler_field(output: &str, name: &str) -> Result<String, io::Error> {
    output
        .lines()
        .find_map(|line| line.trim().strip_prefix(name))
        .and_then(|value| value.strip_prefix(':'))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| io::Error::other(format!("system_profiler omitted {name}")))
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}
