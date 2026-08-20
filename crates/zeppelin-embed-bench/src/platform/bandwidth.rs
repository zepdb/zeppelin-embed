//! Streaming-read memory-bandwidth measurements.

use std::hint::black_box;
use std::io;
use std::process::Command;
use std::sync::Barrier;
use std::thread;
use std::time::{Duration, Instant};

const GIB: usize = 1024 * 1024 * 1024;

/// Configuration for a streaming-read bandwidth run.
#[derive(Clone, Debug)]
pub struct BandwidthConfig {
    /// Total bytes streamed once per sample.
    pub buffer_bytes: usize,
    /// Thread counts to measure.
    pub core_counts: Vec<usize>,
    /// Untimed passes per core count.
    pub warmup_samples: usize,
    /// Timed passes per core count.
    pub samples: usize,
}

impl BandwidthConfig {
    /// CI-sized plausibility configuration.
    pub fn smoke() -> Self {
        Self {
            buffer_bytes: 64 * 1024 * 1024,
            core_counts: vec![1],
            warmup_samples: 1,
            samples: 3,
        }
    }

    /// Architecture-evidence configuration covering every P-core count.
    pub fn full(performance_cores: usize) -> Self {
        Self {
            buffer_bytes: 4 * GIB,
            core_counts: (1..=performance_cores).collect(),
            warmup_samples: 1,
            samples: 5,
        }
    }
}

/// Streaming-read samples for one thread count.
#[derive(Clone, Debug, PartialEq)]
pub struct BandwidthMeasurement {
    /// Number of concurrent reader threads.
    pub core_count: usize,
    /// Individual GB/s observations.
    pub samples_gb_per_second: Vec<f64>,
    /// Median GB/s observation.
    pub median_gb_per_second: f64,
}

/// Complete bandwidth measurement report.
#[derive(Clone, Debug, PartialEq)]
pub struct BandwidthReport {
    /// Total bytes read per sample across all threads.
    pub buffer_bytes: usize,
    /// Untimed samples before each measured core count.
    pub warmup_samples: usize,
    /// Measurements ordered by requested core count.
    pub measurements: Vec<BandwidthMeasurement>,
    /// XOR of all stream checksums, retained to prove the reads were observed.
    pub checksum: u64,
}

/// Detects the current machine's physical performance-core count.
pub fn detect_performance_core_count() -> io::Result<usize> {
    #[cfg(target_os = "macos")]
    {
        let sysctl = Command::new("sysctl")
            .args(["-n", "hw.perflevel0.physicalcpu"])
            .output();
        if let Ok(output) = sysctl
            && output.status.success()
            && let Ok(text) = std::str::from_utf8(&output.stdout)
            && let Ok(count) = text.trim().parse::<usize>()
            && count > 0
        {
            return Ok(count);
        }

        let output = Command::new("system_profiler")
            .arg("SPHardwareDataType")
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(
                "system_profiler could not report the performance-core count",
            ));
        }
        let text = String::from_utf8(output.stdout).map_err(io::Error::other)?;
        parse_performance_core_count(&text).ok_or_else(|| {
            io::Error::other("hardware report did not contain a performance-core count")
        })
    }

    #[cfg(not(target_os = "macos"))]
    std::thread::available_parallelism().map(usize::from)
}

/// Parses the performance-core count emitted by `system_profiler`.
pub fn parse_performance_core_count(output: &str) -> Option<usize> {
    output.lines().find_map(|line| {
        let (_, after_open) = line.split_once('(')?;
        let (count, label) = after_open.trim().split_once(' ')?;
        if label.starts_with("Performance") {
            count.parse::<usize>().ok().filter(|value| *value > 0)
        } else {
            None
        }
    })
}

/// Measures streaming-read throughput for every configured core count.
pub fn measure(config: BandwidthConfig) -> io::Result<BandwidthReport> {
    if config.buffer_bytes == 0
        || config.samples == 0
        || config.core_counts.is_empty()
        || config.core_counts.contains(&0)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bandwidth measurement needs nonzero bytes, samples, and core counts",
        ));
    }
    let mut buffer = vec![0_u8; config.buffer_bytes];
    buffer.fill(0xa5);
    black_box(&mut buffer);

    let mut checksum = 0_u64;
    let mut measurements = Vec::with_capacity(config.core_counts.len());
    for core_count in &config.core_counts {
        for _ in 0..config.warmup_samples {
            let (_, observed) = read_once(&buffer, *core_count)?;
            checksum ^= observed;
        }
        let mut rates = Vec::with_capacity(config.samples);
        for _ in 0..config.samples {
            let (duration, observed) = read_once(&buffer, *core_count)?;
            checksum ^= observed;
            rates.push(gigabytes_per_second(config.buffer_bytes, duration)?);
        }
        let mut sorted = rates.clone();
        sorted.sort_by(f64::total_cmp);
        let median = sorted[(sorted.len() - 1) / 2];
        measurements.push(BandwidthMeasurement {
            core_count: *core_count,
            samples_gb_per_second: rates,
            median_gb_per_second: median,
        });
    }
    black_box(checksum);
    Ok(BandwidthReport {
        buffer_bytes: config.buffer_bytes,
        warmup_samples: config.warmup_samples,
        measurements,
        checksum,
    })
}

fn read_once(buffer: &[u8], core_count: usize) -> io::Result<(Duration, u64)> {
    if core_count > buffer.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reader count exceeds the byte count",
        ));
    }
    let barrier = Barrier::new(core_count);
    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(core_count);
        for index in 0..core_count {
            let start = buffer.len() * index / core_count;
            let end = buffer.len() * (index + 1) / core_count;
            let chunk = &buffer[start..end];
            let ready = &barrier;
            handles.push(scope.spawn(move || {
                ready.wait();
                let started = Instant::now();
                let checksum = stream_checksum(chunk);
                (started.elapsed(), checksum)
            }));
        }
        let mut longest = Duration::ZERO;
        let mut checksum = 0_u64;
        for handle in handles {
            let (duration, observed) = handle
                .join()
                .map_err(|_| io::Error::other("bandwidth reader thread panicked"))?;
            longest = longest.max(duration);
            checksum ^= observed;
        }
        Ok((longest, checksum))
    })
}

#[inline(never)]
fn stream_checksum(bytes: &[u8]) -> u64 {
    let found = unsafe {
        // SAFETY: `bytes` is a live initialized region of exactly the supplied length. The buffer
        // is filled with `0xa5`, so searching for absent `0x5a` forces libc's optimized runtime
        // implementation to stream every byte without a Rust optimizer being able to elide it.
        libc::memchr(bytes.as_ptr().cast(), 0x5a, bytes.len())
    };
    black_box(found as usize as u64)
}

fn gigabytes_per_second(bytes: usize, duration: Duration) -> io::Result<f64> {
    let seconds = duration.as_secs_f64();
    if seconds <= 0.0 {
        return Err(io::Error::other("bandwidth timer had zero duration"));
    }
    Ok(bytes as f64 / 1_000_000_000.0 / seconds)
}

/// Prints a human-readable table followed by a machine-readable JSON block.
pub fn print_report(report: &BandwidthReport) {
    println!("buffer_bytes: {}", report.buffer_bytes);
    println!("warmup samples per core count: {}", report.warmup_samples);
    println!("cores\tmedian_GB/s\traw_GB/s");
    for measurement in &report.measurements {
        let samples = measurement
            .samples_gb_per_second
            .iter()
            .map(|value| format!("{value:.6}"))
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "{}\t{:.6}\t{}",
            measurement.core_count, measurement.median_gb_per_second, samples
        );
    }
    println!(
        "JSON {}",
        serde_json::json!({
            "kind": "bandwidth",
            "buffer_bytes": report.buffer_bytes,
            "warmup_samples": report.warmup_samples,
            "checksum": report.checksum,
            "measurements": report.measurements.iter().map(|measurement| {
                serde_json::json!({
                    "core_count": measurement.core_count,
                    "median_gb_per_second": measurement.median_gb_per_second,
                    "samples_gb_per_second": measurement.samples_gb_per_second
                })
            }).collect::<Vec<_>>()
        })
    );
}

#[cfg(test)]
mod tests {
    use super::{BandwidthConfig, measure, parse_performance_core_count};

    #[test]
    fn bandwidth_smoke_is_plausible() -> Result<(), Box<dyn std::error::Error>> {
        let report = measure(BandwidthConfig::smoke())?;
        let gb_per_second = report.measurements[0].median_gb_per_second;
        assert!(gb_per_second > 1.0);
        assert!(gb_per_second < 400.0);
        Ok(())
    }

    #[test]
    fn performance_core_count_parser_reads_system_profiler_shape() {
        let output = "Total Number of Cores: 16 (12 Performance and 4 Efficiency)";
        assert_eq!(parse_performance_core_count(output), Some(12));
    }
}
