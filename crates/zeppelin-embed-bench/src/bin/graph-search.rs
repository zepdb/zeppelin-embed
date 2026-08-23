//! M4 single-core cached SIFT-1M traversal measurement.

use std::error::Error;
use std::fs;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::Instant;

use zeppelin_embed::graph::search::{
    GraphSearchRequest, GraphSearcher, QueryCoreClass, TraversalPrefetch,
};
use zeppelin_embed::segment::SegmentId;
use zeppelin_embed::segment::reader::SegmentReader;
use zeppelin_embed_bench::graph_recall::Sift1mPaths;
use zeppelin_embed_bench::platform::memory_graph::{calibrate_core, verify_bench_profile};
use zeppelin_embed_bench::platform::taint::{
    detect_taint, format_load1, format_taint_labels, print_taint_status,
};

const DIMS: usize = 128;
const TOP_K: usize = 100;
const DEFAULT_EF: usize = 200;
const DEFAULT_QUERIES: usize = 10_000;
const WARMUP_QUERIES: usize = 128;
const LOAD_LIMIT: f64 = 1.0;
const SEED: u64 = 0x19_0003_51f7_1a00;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BuildPasses {
    One,
    Two,
}

struct Config {
    build_passes: BuildPasses,
    prefetch: TraversalPrefetch,
    ef: usize,
    queries: usize,
    run: usize,
    cache_directory: PathBuf,
    data_directory: PathBuf,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("graph-search: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    verify_bench_profile()?;
    let config = parse_config()?;
    let taint = detect_taint(LOAD_LIMIT);
    let build_passes_label = match config.build_passes {
        BuildPasses::One => "one",
        BuildPasses::Two => "two",
    };
    let prefetch_label = match config.prefetch {
        TraversalPrefetch::Enabled => "on",
        TraversalPrefetch::Disabled => "off",
    };
    println!(
        "GRAPH_SEARCH_CONTEXT build_profile=bench opt_level={} build_passes={build_passes_label} prefetch={prefetch_label} ef={} k={TOP_K} queries={} warmup_queries={WARMUP_QUERIES} run={} single_core=true load_limit={LOAD_LIMIT:.2}",
        env!("ZEPPELIN_BENCH_OPT_LEVEL"),
        config.ef,
        config.queries,
        config.run
    );
    print_taint_status(&taint, LOAD_LIMIT, "graph-search");

    let reader = open_cached_graph(&config.cache_directory, config.build_passes)?;
    let graph = reader.graph_node_blocks()?;
    let rescore = reader.rescore_f32()?;
    let mut searcher = GraphSearcher::new(graph, rescore)?;
    let paths = Sift1mPaths::in_directory(&config.data_directory);
    let queries = read_f32_raw(&paths.queries)?;
    let truth = read_i32_raw(&paths.ground_truth)?;
    let available_queries = (queries.len() / DIMS).min(truth.len() / TOP_K);
    if config.queries == 0 || config.queries > available_queries {
        return Err(io::Error::other(format!(
            "query count {} must be in 1..={available_queries}",
            config.queries
        ))
        .into());
    }
    if config.ef < TOP_K || config.ef > graph.node_count() as usize {
        return Err(io::Error::other(format!(
            "ef {} must be in {TOP_K}..={} ",
            config.ef,
            graph.node_count()
        ))
        .into());
    }

    let warmup = WARMUP_QUERIES.min(available_queries);
    for query_index in available_queries.saturating_sub(warmup)..available_queries {
        let query = query_row(&queries, query_index)?;
        let _ = searcher.search(
            GraphSearchRequest::new(query, TOP_K, config.ef, SEED ^ query_index as u64)
                .with_prefetch(config.prefetch),
            None,
        )?;
    }
    let canary = calibrate_core()?;
    canary.print();

    let mut elapsed_us = Vec::with_capacity(config.queries);
    let mut recall_hits = 0_u64;
    let mut total_hops = 0_u64;
    let mut total_candidates = 0_u64;
    let mut total_pushes = 0_u64;
    let mut qos_class = None;
    let mut qos_priority = None;
    let mut core_class = None;
    for query_index in 0..config.queries {
        let query = query_row(&queries, query_index)?;
        let started = Instant::now();
        let result = searcher.search(
            GraphSearchRequest::new(query, TOP_K, config.ef, SEED ^ query_index as u64)
                .with_observed_core_class(QueryCoreClass::Performance)
                .with_prefetch(config.prefetch),
            None,
        )?;
        elapsed_us.push(started.elapsed().as_secs_f64() * 1e6);
        let expected = truth_row(&truth, query_index)?;
        recall_hits += result
            .candidates()
            .iter()
            .filter(|candidate| expected.contains(&(candidate.row_id() as i32)))
            .count() as u64;
        let counters = result.counters();
        total_hops = total_hops.saturating_add(counters.hops() as u64);
        total_candidates = total_candidates.saturating_add(counters.candidates_scored() as u64);
        total_pushes = total_pushes.saturating_add(counters.pushes() as u64);
        let observed_qos = format!("{:?}", counters.qos_class());
        match qos_class.as_ref() {
            None => qos_class = Some(observed_qos),
            Some(current) if current == &observed_qos => {}
            Some(_) => qos_class = Some(String::from("Mixed")),
        }
        let observed_priority = counters.qos_relative_priority();
        match qos_priority {
            None => qos_priority = Some(observed_priority),
            Some(current) if current == observed_priority => {}
            Some(_) => qos_priority = None,
        }
        let observed_core = format!("{:?}", counters.core_class());
        match core_class.as_ref() {
            None => core_class = Some(observed_core),
            Some(current) if current == &observed_core => {}
            Some(_) => core_class = Some(String::from("Mixed")),
        }
    }
    elapsed_us.sort_by(f64::total_cmp);
    let p50_us = percentile_50(&elapsed_us)?;
    let within_rsd_percent = relative_standard_deviation_percent(&elapsed_us);
    let denominator = (config.queries * TOP_K) as f64;
    let query_denominator = config.queries as f64;
    println!(
        "GRAPH_SEARCH_RESULT build_passes={build_passes_label} prefetch={prefetch_label} ef={} k={TOP_K} queries={} run={} p50_us={p50_us:.3} recall_at_100={:.6} mean_hops={:.3} mean_candidates={:.3} mean_pushes={:.3} within_rsd_percent={within_rsd_percent:.3} qos_class={} qos_priority={} core_class={} load1={} taint={}",
        config.ef,
        config.queries,
        config.run,
        recall_hits as f64 / denominator,
        total_hops as f64 / query_denominator,
        total_candidates as f64 / query_denominator,
        total_pushes as f64 / query_denominator,
        qos_class.as_deref().unwrap_or("Unavailable"),
        qos_priority
            .map(|priority| priority.to_string())
            .unwrap_or_else(|| String::from("mixed")),
        core_class.as_deref().unwrap_or("Unverified"),
        format_load1(taint.load1),
        format_taint_labels(&taint.taints),
    );
    Ok(())
}

fn parse_config() -> Result<Config, Box<dyn Error>> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut config = Config {
        build_passes: BuildPasses::Two,
        prefetch: TraversalPrefetch::Enabled,
        ef: DEFAULT_EF,
        queries: DEFAULT_QUERIES,
        run: 0,
        cache_directory: PathBuf::from("/private/tmp/zeppelin-embed-m3-sift1m"),
        data_directory: workspace.join("tasks/cross-benchmark/data"),
    };
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        let value = arguments
            .next()
            .ok_or_else(|| io::Error::other(format!("{argument} needs a value")))?;
        match argument.as_str() {
            "--build-passes" => {
                config.build_passes = match value.as_str() {
                    "one" => BuildPasses::One,
                    "two" => BuildPasses::Two,
                    _ => {
                        return Err(io::Error::other(format!(
                            "--build-passes must be one or two, got {value}"
                        ))
                        .into());
                    }
                }
            }
            "--prefetch" => {
                config.prefetch = match value.as_str() {
                    "on" => TraversalPrefetch::Enabled,
                    "off" => TraversalPrefetch::Disabled,
                    _ => {
                        return Err(io::Error::other(format!(
                            "--prefetch must be on or off, got {value}"
                        ))
                        .into());
                    }
                }
            }
            "--ef" => config.ef = value.parse()?,
            "--queries" => config.queries = value.parse()?,
            "--run" => config.run = value.parse()?,
            "--cache-dir" => config.cache_directory = PathBuf::from(value),
            "--data-dir" => config.data_directory = PathBuf::from(value),
            _ => {
                return Err(io::Error::other(format!("unknown argument {argument}")).into());
            }
        }
    }
    Ok(config)
}

fn open_cached_graph(
    directory: &Path,
    build_passes: BuildPasses,
) -> Result<SegmentReader, Box<dyn Error>> {
    let marker_path = directory.join("sift1m-graph-build.meta");
    let marker = fs::read_to_string(&marker_path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "cached graph marker {} is unavailable: {error}",
                marker_path.display()
            ),
        )
    })?;
    let expected_pass = match build_passes {
        BuildPasses::One => "pass=one",
        BuildPasses::Two => "pass=two",
    };
    if !marker
        .split_whitespace()
        .any(|field| field == expected_pass)
    {
        return Err(io::Error::other(format!(
            "cached marker is not {expected_pass}; refusing to rebuild SIFT-1M: {}",
            marker.trim()
        ))
        .into());
    }
    let id = SegmentId::new(19, [0x32; 10]);
    SegmentReader::open(&directory.join(id.file_name()), id).map_err(Into::into)
}

fn query_row(queries: &[f32], query_index: usize) -> Result<&[f32], Box<dyn Error>> {
    let start = query_index
        .checked_mul(DIMS)
        .ok_or_else(|| io::Error::other("query offset overflow"))?;
    let end = start
        .checked_add(DIMS)
        .ok_or_else(|| io::Error::other("query end overflow"))?;
    queries
        .get(start..end)
        .ok_or_else(|| io::Error::other(format!("query {query_index} is unavailable")).into())
}

fn truth_row(truth: &[i32], query_index: usize) -> Result<&[i32], Box<dyn Error>> {
    let start = query_index
        .checked_mul(TOP_K)
        .ok_or_else(|| io::Error::other("truth offset overflow"))?;
    let end = start
        .checked_add(TOP_K)
        .ok_or_else(|| io::Error::other("truth end overflow"))?;
    truth
        .get(start..end)
        .ok_or_else(|| io::Error::other(format!("truth row {query_index} is unavailable")).into())
}

fn read_f32_raw(path: &Path) -> Result<Vec<f32>, Box<dyn Error>> {
    read_raw_words(path, f32::from_le_bytes)
}

fn read_i32_raw(path: &Path) -> Result<Vec<i32>, Box<dyn Error>> {
    read_raw_words(path, i32::from_le_bytes)
}

fn read_raw_words<T>(path: &Path, decode: impl Fn([u8; 4]) -> T) -> Result<Vec<T>, Box<dyn Error>> {
    let byte_length = usize::try_from(fs::metadata(path)?.len())?;
    if !byte_length.is_multiple_of(4) {
        return Err(io::Error::other(format!(
            "{} length {byte_length} is not a four-byte multiple",
            path.display()
        ))
        .into());
    }
    let mut reader = BufReader::new(fs::File::open(path)?);
    let mut bytes = vec![0_u8; byte_length];
    reader.read_exact(&mut bytes)?;
    bytes
        .chunks_exact(4)
        .map(|word| {
            let raw: [u8; 4] = word
                .try_into()
                .map_err(|_| io::Error::other("raw word width is invalid"))?;
            Ok(decode(raw))
        })
        .collect::<Result<Vec<_>, io::Error>>()
        .map_err(Into::into)
}

fn percentile_50(sorted: &[f64]) -> Result<f64, Box<dyn Error>> {
    sorted
        .get(sorted.len() / 2)
        .copied()
        .ok_or_else(|| io::Error::other("latency sample is empty").into())
}

fn relative_standard_deviation_percent(values: &[f64]) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    if mean == 0.0 {
        return 0.0;
    }
    let variance = values
        .iter()
        .map(|value| {
            let delta = *value - mean;
            delta * delta
        })
        .sum::<f64>()
        / values.len() as f64;
    variance.sqrt() / mean * 100.0
}
