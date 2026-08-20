//! sqlite-vec brute-force sanity anchors.

use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

/// Configuration for the two sqlite-vec sanity anchors.
#[derive(Clone, Debug)]
pub struct IncumbentConfig {
    /// Number of stored vectors.
    pub row_count: usize,
    /// Dimensions in both stored vector types.
    pub dimensions: usize,
    /// Untimed queries per vector type.
    pub warmup_queries: usize,
    /// Timed queries per vector type.
    pub measured_queries: usize,
    /// Directory for the disposable SQLite database.
    pub directory: PathBuf,
}

impl IncumbentConfig {
    /// Task 02's required 100k by 768-d anchor configuration.
    pub fn full() -> Self {
        Self {
            row_count: 100_000,
            dimensions: 768,
            warmup_queries: 5,
            measured_queries: 30,
            directory: PathBuf::from("/private/tmp"),
        }
    }
}

/// Millisecond distribution reported by SQLite's in-process statement timer.
#[derive(Clone, Debug, PartialEq)]
pub struct MillisecondDistribution {
    /// Raw observations in execution order.
    pub raw_ms: Vec<f64>,
    /// Minimum observation.
    pub min_ms: f64,
    /// Median observation.
    pub p50_ms: f64,
    /// 95th-percentile observation.
    pub p95_ms: f64,
    /// 99th-percentile observation.
    pub p99_ms: f64,
    /// Maximum observation.
    pub max_ms: f64,
}

/// One sqlite-vec vector-type anchor.
#[derive(Clone, Debug, PartialEq)]
pub struct IncumbentMeasurement {
    /// sqlite-vec declared vector type.
    pub vector_type: &'static str,
    /// Dimensions per vector.
    pub dimensions: usize,
    /// Stored vector count.
    pub row_count: usize,
    /// Query execution-time distribution.
    pub distribution: MillisecondDistribution,
}

/// Successful sqlite-vec anchor report.
#[derive(Clone, Debug, PartialEq)]
pub struct IncumbentReport {
    /// SQLite executable used.
    pub sqlite_path: PathBuf,
    /// sqlite-vec loadable extension used.
    pub extension_path: PathBuf,
    /// SQLite version string.
    pub sqlite_version: String,
    /// sqlite-vec version string.
    pub sqlite_vec_version: String,
    /// Untimed queries per vector type.
    pub warmup_queries: usize,
    /// Float32 and one-bit measurements.
    pub measurements: Vec<IncumbentMeasurement>,
}

/// Either locally measured anchors or an honest, actionable deferral.
#[derive(Clone, Debug, PartialEq)]
pub enum IncumbentOutcome {
    /// Both anchors were measured successfully.
    Measured(IncumbentReport),
    /// The installed tools could not support the measurement.
    NotMeasured(String),
}

#[derive(Debug)]
struct IncumbentError(String);

impl fmt::Display for IncumbentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for IncumbentError {}

/// Attempts both incumbent anchors without substituting an approximation on failure.
pub fn measure(config: IncumbentConfig) -> IncumbentOutcome {
    match measure_inner(config) {
        Ok(report) => IncumbentOutcome::Measured(report),
        Err(error) => IncumbentOutcome::NotMeasured(error.to_string()),
    }
}

fn measure_inner(config: IncumbentConfig) -> Result<IncumbentReport, Box<dyn std::error::Error>> {
    if config.row_count == 0 || config.dimensions == 0 || config.measured_queries == 0 {
        return Err(Box::new(IncumbentError(String::from(
            "sqlite-vec anchor needs nonzero rows, dimensions, and measured queries",
        ))));
    }
    let sqlite_path = find_sqlite().ok_or_else(|| {
        IncumbentError(String::from(
            "no extension-capable sqlite3 executable found; set SQLITE3",
        ))
    })?;
    let extension_path = find_extension().ok_or_else(|| {
        IncumbentError(String::from(
            "sqlite-vec loadable extension not found; set SQLITE_VEC_EXTENSION",
        ))
    })?;
    let versions = sqlite_output(
        &sqlite_path,
        &extension_path,
        Path::new(":memory:"),
        "select sqlite_version(), vec_version();\n",
    )?;
    let (sqlite_version, sqlite_vec_version) = versions
        .trim()
        .split_once('|')
        .ok_or_else(|| IncumbentError(format!("unexpected version output: {versions}")))?;

    fs::create_dir_all(&config.directory)?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let database_path = config.directory.join(format!(
        "zeppelin-sqlite-vec-anchor-{}-{nonce}.db",
        std::process::id()
    ));
    let result = run_anchor_queries(&config, &sqlite_path, &extension_path, &database_path);
    let cleanup_result = fs::remove_file(&database_path);
    let measurements = result?;
    cleanup_result?;
    Ok(IncumbentReport {
        sqlite_path,
        extension_path,
        sqlite_version: sqlite_version.to_owned(),
        sqlite_vec_version: sqlite_vec_version.to_owned(),
        warmup_queries: config.warmup_queries,
        measurements,
    })
}

fn run_anchor_queries(
    config: &IncumbentConfig,
    sqlite_path: &Path,
    extension_path: &Path,
    database_path: &Path,
) -> Result<Vec<IncumbentMeasurement>, Box<dyn std::error::Error>> {
    let f32_document = f32_blob_hex(config.dimensions, false);
    let f32_query = f32_blob_hex(config.dimensions, true);
    let bit_document = bit_blob_hex(config.dimensions, false)?;
    let bit_query = bit_blob_hex(config.dimensions, true)?;
    let setup = format!(
        "PRAGMA journal_mode=OFF;\nPRAGMA synchronous=OFF;\nBEGIN;\nCREATE VIRTUAL TABLE f32_vectors USING vec0(embedding float[{}]);\nWITH RECURSIVE seq(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM seq WHERE n < {}) INSERT INTO f32_vectors(rowid, embedding) SELECT n, X'{}' FROM seq;\nCREATE VIRTUAL TABLE bit_vectors USING vec0(embedding bit[{}]);\nWITH RECURSIVE seq(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM seq WHERE n < {}) INSERT INTO bit_vectors(rowid, embedding) SELECT n, vec_bit(X'{}') FROM seq;\nCOMMIT;\n",
        config.dimensions,
        config.row_count,
        f32_document,
        config.dimensions,
        config.row_count,
        bit_document
    );
    sqlite_output(sqlite_path, extension_path, database_path, &setup)?;

    let f32_query_sql = format!(
        "SELECT rowid, distance FROM f32_vectors WHERE embedding MATCH X'{f32_query}' AND k = 10;"
    );
    let bit_query_sql = format!(
        "SELECT rowid, distance FROM bit_vectors WHERE embedding MATCH vec_bit(X'{bit_query}') AND k = 10;"
    );
    let f32_times = timed_queries(
        sqlite_path,
        extension_path,
        database_path,
        &f32_query_sql,
        config.warmup_queries,
        config.measured_queries,
    )?;
    let bit_times = timed_queries(
        sqlite_path,
        extension_path,
        database_path,
        &bit_query_sql,
        config.warmup_queries,
        config.measured_queries,
    )?;
    Ok(vec![
        IncumbentMeasurement {
            vector_type: "float32",
            dimensions: config.dimensions,
            row_count: config.row_count,
            distribution: summarize_ms(f32_times)?,
        },
        IncumbentMeasurement {
            vector_type: "bit",
            dimensions: config.dimensions,
            row_count: config.row_count,
            distribution: summarize_ms(bit_times)?,
        },
    ])
}

fn timed_queries(
    sqlite_path: &Path,
    extension_path: &Path,
    database_path: &Path,
    query: &str,
    warmup_queries: usize,
    measured_queries: usize,
) -> Result<Vec<f64>, Box<dyn std::error::Error>> {
    let mut script = String::from(".mode off\n.timer off\n");
    for _ in 0..warmup_queries {
        script.push_str(query);
        script.push('\n');
    }
    script.push_str(".timer on\n");
    for _ in 0..measured_queries {
        script.push_str(query);
        script.push('\n');
    }
    let output = sqlite_output(sqlite_path, extension_path, database_path, &script)?;
    let times = output
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("Run Time: real ")?;
            rest.split_whitespace().next()?.parse::<f64>().ok()
        })
        .map(|seconds| seconds * 1_000.0)
        .collect::<Vec<_>>();
    if times.len() != measured_queries {
        return Err(Box::new(IncumbentError(format!(
            "sqlite timer returned {} observations for {measured_queries} queries: {output}",
            times.len()
        ))));
    }
    Ok(times)
}

fn sqlite_output(
    sqlite_path: &Path,
    extension_path: &Path,
    database_path: &Path,
    script: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let load_command = format!(".load {}", extension_path.display());
    let mut child = Command::new(sqlite_path)
        .args(["-batch", "-cmd"])
        .arg(load_command)
        .arg(database_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| IncumbentError(String::from("sqlite stdin was not piped")))?;
    stdin.write_all(script.as_bytes())?;
    drop(stdin);
    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");
    if !output.status.success() {
        return Err(Box::new(IncumbentError(format!(
            "sqlite-vec command failed with {}: {}",
            output.status,
            combined.trim()
        ))));
    }
    Ok(combined)
}

fn find_sqlite() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("SQLITE3").map(PathBuf::from)
        && path.is_file()
    {
        return Some(path);
    }
    [
        PathBuf::from("/opt/homebrew/opt/sqlite/bin/sqlite3"),
        PathBuf::from("/opt/homebrew/bin/sqlite3"),
        PathBuf::from("/usr/local/bin/sqlite3"),
        PathBuf::from("/usr/bin/sqlite3"),
    ]
    .into_iter()
    .find(|path| path.is_file())
}

fn find_extension() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("SQLITE_VEC_EXTENSION").map(PathBuf::from)
        && extension_exists(&path)
    {
        return Some(path);
    }
    let output = Command::new("python3")
        .args(["-c", "import sqlite_vec; print(sqlite_vec.loadable_path())"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
    extension_exists(&path).then_some(path)
}

fn extension_exists(path: &Path) -> bool {
    path.is_file() || path.with_extension("dylib").is_file() || path.with_extension("so").is_file()
}

fn f32_blob_hex(dimensions: usize, query: bool) -> String {
    let mut output = String::with_capacity(dimensions * 8);
    for dimension in 0..dimensions {
        let centered = (dimension % 31) as f32 - 15.0;
        let value = if query {
            -centered / 16.0
        } else {
            centered / 16.0
        };
        append_hex(&mut output, &value.to_le_bytes());
    }
    output
}

fn bit_blob_hex(dimensions: usize, query: bool) -> Result<String, Box<dyn std::error::Error>> {
    if !dimensions.is_multiple_of(8) {
        return Err(Box::new(IncumbentError(String::from(
            "bit-vector dimensions must be divisible by eight",
        ))));
    }
    let mut bytes = Vec::with_capacity(dimensions / 8);
    for index in 0..(dimensions / 8) {
        let value = (index as u8).wrapping_mul(37).wrapping_add(11);
        bytes.push(if query { !value } else { value });
    }
    let mut output = String::with_capacity(bytes.len() * 2);
    append_hex(&mut output, &bytes);
    Ok(output)
}

fn append_hex(output: &mut String, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
}

fn summarize_ms(mut samples: Vec<f64>) -> Result<MillisecondDistribution, IncumbentError> {
    if samples.is_empty() {
        return Err(IncumbentError(String::from(
            "sqlite latency distribution is empty",
        )));
    }
    let raw_ms = samples.clone();
    samples.sort_by(f64::total_cmp);
    Ok(MillisecondDistribution {
        min_ms: samples[0],
        p50_ms: nearest_rank(&samples, 50),
        p95_ms: nearest_rank(&samples, 95),
        p99_ms: nearest_rank(&samples, 99),
        max_ms: samples[samples.len() - 1],
        raw_ms,
    })
}

fn nearest_rank(sorted: &[f64], percentile: usize) -> f64 {
    let rank = percentile.saturating_mul(sorted.len()).div_ceil(100);
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// Prints a human-readable table followed by a machine-readable JSON block.
pub fn print_outcome(outcome: &IncumbentOutcome) {
    match outcome {
        IncumbentOutcome::Measured(report) => {
            println!(
                "sqlite: {} ({})",
                report.sqlite_path.display(),
                report.sqlite_version
            );
            println!(
                "sqlite-vec: {} ({})",
                report.extension_path.display(),
                report.sqlite_vec_version
            );
            println!("warmup queries per vector type: {}", report.warmup_queries);
            println!("type\trows\tdimensions\tmin_ms\tp50_ms\tp95_ms\tp99_ms\tmax_ms\traw_ms");
            for measurement in &report.measurements {
                let raw = measurement
                    .distribution
                    .raw_ms
                    .iter()
                    .map(|value| format!("{value:.6}"))
                    .collect::<Vec<_>>()
                    .join(",");
                println!(
                    "{}\t{}\t{}\t{:.6}\t{:.6}\t{:.6}\t{:.6}\t{:.6}\t{}",
                    measurement.vector_type,
                    measurement.row_count,
                    measurement.dimensions,
                    measurement.distribution.min_ms,
                    measurement.distribution.p50_ms,
                    measurement.distribution.p95_ms,
                    measurement.distribution.p99_ms,
                    measurement.distribution.max_ms,
                    raw
                );
            }
            println!(
                "JSON {}",
                serde_json::json!({
                    "kind": "sqlite_vec",
                    "status": "measured",
                    "sqlite_path": report.sqlite_path,
                    "extension_path": report.extension_path,
                    "sqlite_version": report.sqlite_version,
                    "sqlite_vec_version": report.sqlite_vec_version,
                    "warmup_queries": report.warmup_queries,
                    "measurements": report.measurements.iter().map(|measurement| {
                        serde_json::json!({
                            "vector_type": measurement.vector_type,
                            "row_count": measurement.row_count,
                            "dimensions": measurement.dimensions,
                            "min_ms": measurement.distribution.min_ms,
                            "p50_ms": measurement.distribution.p50_ms,
                            "p95_ms": measurement.distribution.p95_ms,
                            "p99_ms": measurement.distribution.p99_ms,
                            "max_ms": measurement.distribution.max_ms,
                            "raw_ms": measurement.distribution.raw_ms
                        })
                    }).collect::<Vec<_>>()
                })
            );
        }
        IncumbentOutcome::NotMeasured(reason) => {
            println!("NOT MEASURED — {reason}");
            println!(
                "JSON {}",
                serde_json::json!({
                    "kind": "sqlite_vec",
                    "status": "not_measured",
                    "reason": reason
                })
            );
        }
    }
}
