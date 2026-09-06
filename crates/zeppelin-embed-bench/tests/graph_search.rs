#![allow(clippy::expect_used, clippy::indexing_slicing)]

use std::path::Path;
use std::process::Command;

use zeppelin_embed::graph::block::{
    GraphNodeBlockBuild, GraphNodeBlockInput, GraphNodeLayout, decode_node_blocks,
    encode_node_blocks,
};
use zeppelin_embed::graph::search::{GraphSearchRequest, GraphSearchScratch, GraphSearcher};
use zeppelin_embed::quant::quantize_bit4;
use zeppelin_embed_bench::process_median::ProcessMedian;

const COVERAGE_DIMS: usize = 128;

#[derive(Clone, Debug)]
struct Observation {
    p50_us: f64,
    recall_at_100: f64,
    ef: usize,
    ef_source: String,
    build_passes: String,
    core_class: String,
}

#[test]
fn graph_search_small_fixture_exercises_the_public_kernel() {
    exercise_graph_search_with_small_coverage_fixture();
}

#[test]
#[ignore = "requires a prepared SIFT1M cache and stable benchmark hardware"]
fn sift1m_p50_under_250us_at_recall_at_least_093() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let binary = build_bench_binary(&workspace).expect("bench-profile graph-search binary");
    let observations = (0..3)
        .map(|run| run_process(run, &binary))
        .collect::<Result<Vec<_>, _>>()
        .expect("three independent bench-profile processes must succeed");
    let p50_values = observations
        .iter()
        .map(|observation| observation.p50_us)
        .collect::<Vec<_>>();
    let process_summary =
        ProcessMedian::new(p50_values).expect("exactly three valid process observations");
    let process_median = process_summary.median();
    println!(
        "GRAPH_SEARCH_PROCESS_RESULT p50_us={:?} median_us={process_median:.3} between_process_spread_percent={:.3}",
        process_summary.values(),
        process_summary.spread_percent(),
    );

    assert!(
        observations.iter().all(|observation| observation.ef == 200
            && observation.ef_source == "adaptive"
            && observation.build_passes == "one"
            && observation.core_class == "Performance"
            && observation.recall_at_100 >= 0.93),
        "recall/ef gate failed: {observations:?}"
    );
    assert!(
        process_median <= 250.0,
        "p50 gate failed: process p50 values {:?}, median {process_median:.3} us, between-process spread {:.3}%",
        process_summary.values(),
        process_summary.spread_percent(),
    );
}

fn exercise_graph_search_with_small_coverage_fixture() {
    let values = [10.0_f32, 9.0, 8.0, 1.0, 7.0, 6.0, 5.0, 4.0, 0.0];
    let layout = GraphNodeLayout::new(COVERAGE_DIMS as u32, COVERAGE_DIMS as u32, 4)
        .expect("coverage graph layout");
    let rescore = values
        .into_iter()
        .flat_map(|value| vec![value; COVERAGE_DIMS])
        .collect::<Vec<_>>();
    let mut codes = vec![0_u8; values.len() * COVERAGE_DIMS.div_ceil(2)];
    let factors = rescore
        .chunks_exact(COVERAGE_DIMS)
        .zip(codes.chunks_exact_mut(COVERAGE_DIMS.div_ceil(2)))
        .map(|(row, output)| quantize_bit4(row, output).expect("coverage row quantizes"))
        .collect::<Vec<_>>();
    let neighbors = [
        vec![4, 5, 6, 7],
        Vec::new(),
        Vec::new(),
        vec![8],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    ];
    let nodes = (0..values.len())
        .map(|row| GraphNodeBlockInput {
            codes: &codes[row * COVERAGE_DIMS.div_ceil(2)..(row + 1) * COVERAGE_DIMS.div_ceil(2)],
            factors: factors[row],
            flags: u8::from(row < 4),
            neighbors: &neighbors[row],
        })
        .collect::<Vec<_>>();
    let encoded = encode_node_blocks(GraphNodeBlockBuild {
        layout,
        nodes: &nodes,
    })
    .expect("coverage graph encodes");
    let graph = decode_node_blocks(encoded.as_bytes()).expect("coverage graph decodes");
    let mut scratch = GraphSearchScratch::new(graph.node_count(), graph.layout().max_degree())
        .expect("coverage scratch");
    let mut searcher =
        GraphSearcher::new(graph, &rescore, &mut scratch).expect("coverage searcher");
    let query = vec![0.0_f32; COVERAGE_DIMS];
    let result = searcher
        .search(
            GraphSearchRequest::new(&query, 1, 0x19_0003_c0de)
                .with_ef(4)
                .with_candidate_trace(),
            None,
        )
        .expect("coverage traversal");

    assert_eq!(result.candidates()[0].row_id(), 8);
    assert_eq!(result.candidate_sequence(), Some(&[0, 1, 2, 3, 8][..]));

    let process_summary = ProcessMedian::new(vec![102.0, 100.0, 101.0])
        .expect("coverage process observations are valid");
    assert_eq!(process_summary.values(), &[100.0, 101.0, 102.0]);
    assert_eq!(process_summary.minimum(), 100.0);
    assert_eq!(process_summary.median(), 101.0);
    assert_eq!(process_summary.maximum(), 102.0);
    assert!((process_summary.spread_percent() - (2.0 / 101.0 * 100.0)).abs() < f64::EPSILON);
}

fn build_bench_binary(workspace: &Path) -> Result<std::path::PathBuf, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| String::from("cargo"));
    let target_root = std::env::var_os("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| workspace.join("target"));
    // Coverage and ordinary bench builds use different Cargo fingerprints but
    // publish the same final binary path. A later non-coverage build can see
    // its old fingerprint as fresh and execute the coverage-instrumented
    // binary left at `target/release/graph-search`. Keep the latency gate's
    // artifact in a target subtree that the coverage lane never writes.
    let target = target_root.join("graph-search-perf-gate");
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
            "--target-dir",
        ])
        .arg(&target)
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
    Ok(target.join("release/graph-search"))
}

fn run_process(run: usize, binary: &Path) -> Result<Observation, String> {
    let output = Command::new(binary)
        .args(["--queries", "10000", "--run", &run.to_string()])
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
        ef_source: parse_value(line, "ef_source")?,
        build_passes: parse_value(line, "build_passes")?,
        core_class: parse_value(line, "core_class")?,
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
