//! APFS synchronization latency measurements.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Synchronization primitive under measurement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FsyncPrimitive {
    /// POSIX `fsync(2)`.
    Plain,
    /// Darwin `F_BARRIERFSYNC`.
    Barrier,
    /// Darwin `F_FULLFSYNC`.
    Full,
}

impl FsyncPrimitive {
    /// Stable report label for the primitive.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Plain => "fsync",
            Self::Barrier => "F_BARRIERFSYNC",
            Self::Full => "F_FULLFSYNC",
        }
    }
}

impl fmt::Display for FsyncPrimitive {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Configuration for an fsync latency run.
#[derive(Clone, Debug)]
pub struct FsyncConfig {
    /// Appended byte counts to measure.
    pub append_sizes: Vec<usize>,
    /// Untimed iterations used to warm the filesystem path.
    pub warmup_iterations: usize,
    /// Timed iterations per size and primitive.
    pub iterations: usize,
    /// Directory in which temporary measurement files are created.
    pub directory: PathBuf,
}

impl FsyncConfig {
    /// CI-sized plausibility configuration.
    pub fn smoke() -> Self {
        Self {
            append_sizes: vec![4 * 1024],
            warmup_iterations: 2,
            iterations: 5,
            directory: PathBuf::from("/private/tmp"),
        }
    }

    /// Architecture-evidence configuration required by Task 02.
    pub fn full() -> Self {
        Self {
            append_sizes: vec![4 * 1024, 1024 * 1024],
            warmup_iterations: 50,
            iterations: 1_000,
            directory: PathBuf::from("/private/tmp"),
        }
    }
}

/// A percentile point in a latency distribution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Percentile {
    /// Percentile rank in the inclusive range 0 through 100.
    pub rank: u8,
    /// Nearest-rank latency in nanoseconds.
    pub latency_ns: u128,
}

/// Summary and deciles for one raw latency distribution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LatencyDistribution {
    /// Number of samples.
    pub count: usize,
    /// Minimum latency in nanoseconds.
    pub min_ns: u128,
    /// Median latency in nanoseconds.
    pub p50_ns: u128,
    /// 95th-percentile latency in nanoseconds.
    pub p95_ns: u128,
    /// 99th-percentile latency in nanoseconds.
    pub p99_ns: u128,
    /// Maximum latency in nanoseconds.
    pub max_ns: u128,
    /// Nearest-rank deciles, including the 10th through 90th percentiles.
    pub deciles: Vec<Percentile>,
}

/// One synchronization primitive and append-size measurement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FsyncMeasurement {
    /// Primitive used for the sample set.
    pub primitive: FsyncPrimitive,
    /// Bytes appended before each synchronization call.
    pub append_bytes: usize,
    /// Raw-distribution summary.
    pub distribution: LatencyDistribution,
}

/// Complete fsync measurement report.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FsyncReport {
    /// Directory used for temporary files.
    pub directory: PathBuf,
    /// Untimed warm-up iterations per configuration.
    pub warmup_iterations: usize,
    /// Measurements ordered by append size and primitive.
    pub measurements: Vec<FsyncMeasurement>,
}

/// Measures every configured append size across all three synchronization primitives.
pub fn measure(config: FsyncConfig) -> io::Result<FsyncReport> {
    if config.iterations == 0 || config.append_sizes.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fsync measurement needs at least one size and iteration",
        ));
    }
    std::fs::create_dir_all(&config.directory)?;
    let mut measurements = Vec::with_capacity(config.append_sizes.len() * 3);
    for append_bytes in &config.append_sizes {
        for primitive in [
            FsyncPrimitive::Plain,
            FsyncPrimitive::Barrier,
            FsyncPrimitive::Full,
        ] {
            measurements.push(measure_one(&config, *append_bytes, primitive)?);
        }
    }
    Ok(FsyncReport {
        directory: config.directory,
        warmup_iterations: config.warmup_iterations,
        measurements,
    })
}

fn measure_one(
    config: &FsyncConfig,
    append_bytes: usize,
    primitive: FsyncPrimitive,
) -> io::Result<FsyncMeasurement> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let path = config.directory.join(format!(
        "zeppelin-platform-fsync-{}-{append_bytes}-{}-{nonce}.tmp",
        std::process::id(),
        primitive.label()
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)?;
    let buffer = vec![0xa5_u8; append_bytes];
    let result = run_iterations(
        &mut file,
        &buffer,
        primitive,
        config.warmup_iterations,
        config.iterations,
    );
    drop(file);
    let remove_result = std::fs::remove_file(&path);
    let latencies = result?;
    remove_result?;
    Ok(FsyncMeasurement {
        primitive,
        append_bytes,
        distribution: summarize(latencies)?,
    })
}

fn run_iterations(
    file: &mut File,
    buffer: &[u8],
    primitive: FsyncPrimitive,
    warmup_iterations: usize,
    iterations: usize,
) -> io::Result<Vec<u128>> {
    for _ in 0..warmup_iterations {
        file.write_all(buffer)?;
        synchronize(file, primitive)?;
    }
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;

    let mut latencies = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        file.write_all(buffer)?;
        let started = Instant::now();
        synchronize(file, primitive)?;
        latencies.push(started.elapsed().as_nanos());
    }
    Ok(latencies)
}

fn synchronize(file: &File, primitive: FsyncPrimitive) -> io::Result<()> {
    match primitive {
        FsyncPrimitive::Plain => {
            let result = unsafe {
                // SAFETY: the descriptor is borrowed from a live `File`; `fsync` does not retain it.
                libc::fsync(file.as_raw_fd())
            };
            if result == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }
        #[cfg(target_os = "macos")]
        FsyncPrimitive::Barrier => {
            zeppelin_embed::sys::darwin::barrier_fsync(file.as_raw_fd()).map_err(io::Error::other)
        }
        #[cfg(not(target_os = "macos"))]
        FsyncPrimitive::Barrier => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "F_BARRIERFSYNC is only available on Darwin",
        )),
        #[cfg(target_os = "macos")]
        FsyncPrimitive::Full => {
            zeppelin_embed::sys::darwin::full_fsync(file.as_raw_fd()).map_err(io::Error::other)
        }
        #[cfg(not(target_os = "macos"))]
        FsyncPrimitive::Full => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "F_FULLFSYNC is only available on Darwin",
        )),
    }
}

fn summarize(mut samples: Vec<u128>) -> io::Result<LatencyDistribution> {
    if samples.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "latency distribution is empty",
        ));
    }
    samples.sort_unstable();
    let min_ns = samples[0];
    let max_ns = samples[samples.len() - 1];
    let deciles = (10_u8..=90)
        .step_by(10)
        .map(|rank| Percentile {
            rank,
            latency_ns: nearest_rank(&samples, usize::from(rank)),
        })
        .collect();
    Ok(LatencyDistribution {
        count: samples.len(),
        min_ns,
        p50_ns: nearest_rank(&samples, 50),
        p95_ns: nearest_rank(&samples, 95),
        p99_ns: nearest_rank(&samples, 99),
        max_ns,
        deciles,
    })
}

fn nearest_rank(sorted: &[u128], percentile: usize) -> u128 {
    let rank = percentile.saturating_mul(sorted.len()).div_ceil(100);
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// Prints a human-readable table followed by a machine-readable JSON block.
pub fn print_report(report: &FsyncReport) {
    println!("fsync directory: {}", report.directory.display());
    println!(
        "warmup iterations per configuration: {}",
        report.warmup_iterations
    );
    println!("primitive\tappend_bytes\tn\tmin_ns\tp50_ns\tp95_ns\tp99_ns\tmax_ns\tdeciles_ns");
    for measurement in &report.measurements {
        let deciles = measurement
            .distribution
            .deciles
            .iter()
            .map(|point| format!("p{}={}", point.rank, point.latency_ns))
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            measurement.primitive,
            measurement.append_bytes,
            measurement.distribution.count,
            measurement.distribution.min_ns,
            measurement.distribution.p50_ns,
            measurement.distribution.p95_ns,
            measurement.distribution.p99_ns,
            measurement.distribution.max_ns,
            deciles
        );
    }
    let json_measurements = report
        .measurements
        .iter()
        .map(|measurement| {
            serde_json::json!({
                "primitive": measurement.primitive.label(),
                "append_bytes": measurement.append_bytes,
                "count": measurement.distribution.count,
                "min_ns": measurement.distribution.min_ns,
                "p50_ns": measurement.distribution.p50_ns,
                "p95_ns": measurement.distribution.p95_ns,
                "p99_ns": measurement.distribution.p99_ns,
                "max_ns": measurement.distribution.max_ns,
                "deciles": measurement.distribution.deciles.iter().map(|point| {
                    serde_json::json!({"rank": point.rank, "latency_ns": point.latency_ns})
                }).collect::<Vec<_>>()
            })
        })
        .collect::<Vec<_>>();
    println!(
        "JSON {}",
        serde_json::json!({
            "kind": "fsync",
            "directory": report.directory,
            "warmup_iterations": report.warmup_iterations,
            "measurements": json_measurements
        })
    );
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::{FsyncConfig, measure};

    #[test]
    fn fsync_smoke_latencies_are_plausible() -> Result<(), Box<dyn std::error::Error>> {
        let report = measure(FsyncConfig::smoke())?;
        assert_eq!(report.measurements.len(), 3);
        for measurement in report.measurements {
            assert!(measurement.distribution.p50_ns > 10_000);
            assert!(measurement.distribution.p50_ns < 100_000_000);
        }
        Ok(())
    }
}
