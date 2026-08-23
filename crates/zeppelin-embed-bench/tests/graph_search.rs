#![allow(clippy::expect_used)]

use std::path::Path;
use std::process::Command;

#[derive(Clone, Copy, Debug)]
struct Observation {
    p50_us: f64,
    recall_at_100: f64,
    ef: usize,
}

#[test]
fn sift1m_p50_under_250us_at_recall_at_least_093() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let binary = build_bench_binary(&workspace).expect("bench-profile graph-search binary");
    let observations = (0..3)
        .map(|run| run_process(run, &binary))
        .collect::<Result<Vec<_>, _>>()
        .expect("three independent bench-profile processes must succeed");
    let mut p50_values = observations
        .iter()
        .map(|observation| observation.p50_us)
        .collect::<Vec<_>>();
    p50_values.sort_by(f64::total_cmp);
    let process_median = *p50_values.get(1).expect("exactly three observations");

    assert!(
        observations
            .iter()
            .all(|observation| observation.ef == 200 && observation.recall_at_100 >= 0.93),
        "recall/ef gate failed: {observations:?}"
    );
    assert!(
        process_median <= 250.0,
        "p50 gate failed: process p50 values {p50_values:?}, median {process_median:.3} us"
    );
}

fn build_bench_binary(workspace: &Path) -> Result<std::path::PathBuf, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| String::from("cargo"));
    let output = Command::new(&cargo)
        .current_dir(workspace)
        .args([
            "build",
            "--quiet",
            "--profile",
            "bench",
            "-p",
            "zeppelin-embed-bench",
            "--bin",
            "graph-search",
        ])
        .output()
        .map_err(|error| format!("bench build failed to start: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "bench build exited {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| workspace.join("target"));
    Ok(target.join("release/graph-search"))
}

fn run_process(run: usize, binary: &Path) -> Result<Observation, String> {
    let output = Command::new(binary)
        .args([
            "--build-passes",
            "two",
            "--prefetch",
            "on",
            "--queries",
            "10000",
            "--run",
            &run.to_string(),
        ])
        .output()
        .map_err(|error| format!("run {run} failed to start: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!(
            "run {run} exited {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status
        ));
    }
    let line = stdout
        .lines()
        .find(|line| line.starts_with("GRAPH_SEARCH_RESULT "))
        .ok_or_else(|| format!("run {run} had no result line\nstdout:\n{stdout}"))?;
    Ok(Observation {
        p50_us: parse_value(line, "p50_us")?,
        recall_at_100: parse_value(line, "recall_at_100")?,
        ef: parse_value(line, "ef")?,
    })
}

fn parse_value<T>(line: &str, key: &str) -> Result<T, String>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let prefix = format!("{key}=");
    let value = line
        .split_whitespace()
        .find_map(|field| field.strip_prefix(&prefix))
        .ok_or_else(|| format!("result line has no {key}: {line}"))?;
    value
        .parse()
        .map_err(|error| format!("invalid {key}={value}: {error}"))
}
