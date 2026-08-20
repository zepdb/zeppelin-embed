//! Task 02 platform-truth command-line rig.

use std::error::Error;
use std::io;
use std::process::Command;
use std::time::Duration;

use zeppelin_embed_bench::platform::{bandwidth, energy, footprint, fsync, incumbent};

fn main() {
    if let Err(error) = run() {
        eprintln!("platform-truth: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments.as_slice() == ["--smoke"] {
        return run_smoke();
    }
    let subcommand = arguments.first().map(String::as_str).unwrap_or("all");
    match subcommand {
        "all" if arguments.len() == 1 => run_all(),
        "fsync" if arguments.len() == 1 => run_fsync_full(),
        "bandwidth" if arguments.len() == 1 => run_bandwidth_full(),
        "footprint" if arguments.len() == 1 => run_footprint_full(),
        "incumbent" if arguments.len() == 1 => run_incumbent_full(),
        "energy" => run_energy(&arguments[1..]),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: platform-truth [--smoke | all | fsync | bandwidth | footprint | incumbent | energy -- <workload> [args...]]",
        )
        .into()),
    }
}

fn run_all() -> Result<(), Box<dyn Error>> {
    print_machine_context();
    println!("\n== fsync ==");
    let fsync_report = fsync::measure(fsync::FsyncConfig::full())?;
    fsync::print_report(&fsync_report);

    println!("\n== bandwidth ==");
    let performance_cores = bandwidth::detect_performance_core_count()?;
    println!("detected performance cores: {performance_cores}");
    let bandwidth_report = bandwidth::measure(bandwidth::BandwidthConfig::full(performance_cores))?;
    bandwidth::print_report(&bandwidth_report);

    println!("\n== footprint ==");
    #[cfg(target_os = "macos")]
    let footprint_report = {
        let report = footprint::measure(footprint::FootprintConfig::architecture_invariant())?;
        footprint::print_report(&report);
        Some(report)
    };
    #[cfg(not(target_os = "macos"))]
    let footprint_report: Option<footprint::FootprintReport> = {
        println!("NOT MEASURED — phys_footprint is only available on Darwin");
        None
    };

    println!("\n== energy ==");
    energy::print_not_measured("<workload> [args...]");

    println!("\n== sqlite-vec anchors ==");
    let incumbent_outcome = incumbent::measure(incumbent::IncumbentConfig::full());
    incumbent::print_outcome(&incumbent_outcome);

    println!("\n== decision outputs ==");
    print_decisions(
        &fsync_report,
        &bandwidth_report,
        performance_cores,
        footprint_report.as_ref(),
    );
    Ok(())
}

fn run_fsync_full() -> Result<(), Box<dyn Error>> {
    print_machine_context();
    let report = fsync::measure(fsync::FsyncConfig::full())?;
    fsync::print_report(&report);
    Ok(())
}

fn run_bandwidth_full() -> Result<(), Box<dyn Error>> {
    print_machine_context();
    let performance_cores = bandwidth::detect_performance_core_count()?;
    println!("detected performance cores: {performance_cores}");
    let report = bandwidth::measure(bandwidth::BandwidthConfig::full(performance_cores))?;
    bandwidth::print_report(&report);
    Ok(())
}

fn run_footprint_full() -> Result<(), Box<dyn Error>> {
    print_machine_context();
    let report = footprint::measure(footprint::FootprintConfig::architecture_invariant())?;
    footprint::print_report(&report);
    Ok(())
}

fn run_incumbent_full() -> Result<(), Box<dyn Error>> {
    print_machine_context();
    let outcome = incumbent::measure(incumbent::IncumbentConfig::full());
    incumbent::print_outcome(&outcome);
    match outcome {
        incumbent::IncumbentOutcome::Measured(_) => Ok(()),
        incumbent::IncumbentOutcome::NotMeasured(reason) => {
            Err(io::Error::other(format!("sqlite-vec anchor not measured: {reason}")).into())
        }
    }
}

fn run_energy(arguments: &[String]) -> Result<(), Box<dyn Error>> {
    let workload_arguments = arguments
        .strip_prefix(&[String::from("--")])
        .unwrap_or(arguments);
    let (program, program_arguments) = workload_arguments.split_first().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "energy requires: energy -- <workload> [args...]",
        )
    })?;
    let measurement = energy::measure_command(Duration::from_secs(5), program, program_arguments)?;
    energy::print_measurement(measurement);
    Ok(())
}

fn run_smoke() -> Result<(), Box<dyn Error>> {
    println!("platform-truth smoke mode");
    #[cfg(target_os = "macos")]
    {
        let report = fsync::measure(fsync::FsyncConfig::smoke())?;
        for measurement in &report.measurements {
            if !(10_000..100_000_000).contains(&measurement.distribution.p50_ns) {
                return Err(io::Error::other(format!(
                    "{} p50 {} ns is outside the fixed (10 us, 100 ms) smoke range",
                    measurement.primitive, measurement.distribution.p50_ns
                ))
                .into());
            }
        }
        fsync::print_report(&report);
    }
    #[cfg(not(target_os = "macos"))]
    println!("fsync smoke skipped: Darwin-only primitives");

    let bandwidth_report = bandwidth::measure(bandwidth::BandwidthConfig::smoke())?;
    let bandwidth_value = bandwidth_report
        .measurements
        .first()
        .ok_or_else(|| io::Error::other("bandwidth smoke returned no measurements"))?
        .median_gb_per_second;
    if !(1.0..400.0).contains(&bandwidth_value) {
        return Err(io::Error::other(format!(
            "bandwidth {bandwidth_value} GB/s is outside the fixed (1, 400) smoke range"
        ))
        .into());
    }
    bandwidth::print_report(&bandwidth_report);

    #[cfg(target_os = "macos")]
    {
        let report = footprint::measure(footprint::FootprintConfig::smoke())?;
        if report.clean.phys_footprint_delta.unsigned_abs() >= report.file_size / 20 {
            return Err(io::Error::other(format!(
                "clean mmap phys_footprint delta {} exceeds the fixed 5% invariant",
                report.clean.phys_footprint_delta
            ))
            .into());
        }
        if report.private_dirty.phys_footprint_delta <= 0 {
            return Err(io::Error::other(
                "dirty MAP_PRIVATE counter-case did not increase phys_footprint",
            )
            .into());
        }
        footprint::print_report(&report);
    }
    #[cfg(not(target_os = "macos"))]
    println!("footprint smoke skipped: Darwin-only counters");

    let energy_sample = energy::parse_package_energy(
        "Combined Power (CPU + GPU + ANE): 2500 mW",
        Duration::from_secs(2),
    )?;
    if energy_sample.package_joules <= 0.0 {
        return Err(io::Error::other("energy parser did not report positive joules").into());
    }
    println!("energy parser smoke: ok");
    println!("platform-truth smoke: ok");
    Ok(())
}

fn print_decisions(
    fsync_report: &fsync::FsyncReport,
    bandwidth_report: &bandwidth::BandwidthReport,
    performance_cores: usize,
    footprint_report: Option<&footprint::FootprintReport>,
) {
    let barrier_p95_ns = fsync_report
        .measurements
        .iter()
        .filter(|measurement| measurement.primitive == fsync::FsyncPrimitive::Barrier)
        .map(|measurement| measurement.distribution.p95_ns)
        .max();
    if let Some(window_ns) = barrier_p95_ns {
        println!(
            "task08_ordered_group_window_ns={window_ns} (derived recommendation: maximum measured barrier p95 across append sizes)"
        );
    }
    for measurement in &bandwidth_report.measurements {
        println!(
            "task03_05_bandwidth_denominator_{}_cores_GBps={:.6}",
            measurement.core_count, measurement.median_gb_per_second
        );
    }
    println!("detected_performance_cores={performance_cores}");
    if let Some(report) = footprint_report {
        let percent = report.clean.phys_footprint_delta as f64 / report.file_size as f64 * 100.0;
        let verdict = if report.clean.phys_footprint_delta.unsigned_abs() < report.file_size / 20 {
            "HOLDS"
        } else {
            "FAILS"
        };
        println!(
            "task09_clean_mmap_phys_footprint_verdict={verdict} delta_bytes={} delta_percent={percent:.9}",
            report.clean.phys_footprint_delta
        );
    }
    for append_bytes in [4 * 1024, 1024 * 1024] {
        let barrier = fsync_report.measurements.iter().find(|measurement| {
            measurement.primitive == fsync::FsyncPrimitive::Barrier
                && measurement.append_bytes == append_bytes
        });
        let full = fsync_report.measurements.iter().find(|measurement| {
            measurement.primitive == fsync::FsyncPrimitive::Full
                && measurement.append_bytes == append_bytes
        });
        if let (Some(barrier), Some(full)) = (barrier, full) {
            let ratio = barrier.distribution.p50_ns as f64 / full.distribution.p50_ns as f64;
            let surprise = if ratio >= 0.8 { "YES" } else { "NO" };
            println!(
                "barrier_to_full_p50_ratio_append_{}={ratio:.9} architecture_surprise={surprise}",
                append_bytes
            );
        }
    }
}

fn print_machine_context() {
    println!("== machine context ==");
    for (label, program, arguments) in [
        ("uname -a", "uname", &["-a"][..]),
        ("sw_vers", "sw_vers", &[][..]),
        (
            "system_profiler SPHardwareDataType",
            "system_profiler",
            &["SPHardwareDataType"][..],
        ),
        ("pmset -g ps", "pmset", &["-g", "ps"][..]),
        ("pmset -g therm", "pmset", &["-g", "therm"][..]),
    ] {
        println!("$ {label}");
        match Command::new(program).args(arguments).output() {
            Ok(output) => {
                print!("{}", String::from_utf8_lossy(&output.stdout));
                eprint!("{}", String::from_utf8_lossy(&output.stderr));
                if !output.status.success() {
                    println!("[exit status: {}]", output.status);
                }
            }
            Err(error) => println!("NOT MEASURED — command failed to start: {error}"),
        }
    }
}
