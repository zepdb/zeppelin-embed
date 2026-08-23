//! M4 single-core cached graph traversal measurement.

use std::error::Error;
use std::fs;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::Instant;

use zeppelin_embed::graph::block::CACHE_LINE_BYTES;
use zeppelin_embed::graph::build::GraphBuildPasses;
use zeppelin_embed::graph::search::{
    GraphSearchRequest, GraphSearcher, QueryCoreClass, TraversalPrefetch,
};
use zeppelin_embed::segment::SegmentId;
use zeppelin_embed::segment::reader::SegmentReader;
use zeppelin_embed_bench::graph_recall::{
    CrossGraphArtifact, CrossGraphDataset, Sift1mPaths, open_cross_dataset_graph,
    read_cross_dataset_queries, read_cross_dataset_truth,
};
use zeppelin_embed_bench::platform::memory_graph::{calibrate_core, verify_bench_profile};
use zeppelin_embed_bench::platform::taint::{
    detect_taint, format_load1, format_taint_labels, print_taint_status,
};

const SIFT_DIMS: usize = 128;
const SIFT_ROWS: usize = 1_000_000;
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
    dataset_name: String,
    build_passes: BuildPasses,
    prefetch: TraversalPrefetch,
    ef: usize,
    queries: usize,
    run: usize,
    cache_directory: Option<PathBuf>,
    data_directory: PathBuf,
}

enum CachedGraph {
    Sift(SegmentReader),
    Cross(CrossGraphArtifact),
}

impl CachedGraph {
    fn reader(&self) -> &SegmentReader {
        match self {
            Self::Sift(reader) => reader,
            Self::Cross(artifact) => artifact.reader(),
        }
    }

    fn zero_norm_rows(&self) -> &[u32] {
        match self {
            Self::Sift(_) => &[],
            Self::Cross(artifact) => artifact.metadata().zero_norm_rows(),
        }
    }
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
    let default_cache = if config.dataset_name == "sift-128-euclidean" {
        PathBuf::from("/private/tmp/zeppelin-embed-m3-sift1m")
    } else {
        PathBuf::from(format!(
            "/private/tmp/zeppelin-embed-m4b-{}",
            config.dataset_name
        ))
    };
    let cache_directory = config.cache_directory.as_deref().unwrap_or(&default_cache);
    let (cached, queries, truth, dimensions, rows, metric_label) =
        if config.dataset_name == "sift-128-euclidean" {
            let paths = Sift1mPaths::in_directory(&config.data_directory);
            (
                CachedGraph::Sift(open_cached_sift_graph(
                    cache_directory,
                    config.build_passes,
                )?),
                read_f32_raw(&paths.queries)?,
                read_i32_raw(&paths.ground_truth)?,
                SIFT_DIMS,
                SIFT_ROWS,
                "l2",
            )
        } else {
            let dataset = CrossGraphDataset::named(&config.dataset_name, &config.data_directory)?;
            let queries = read_cross_dataset_queries(&dataset)?;
            let truth = read_cross_dataset_truth(&dataset)?;
            let dimensions = dataset.dimensions();
            let rows = dataset.rows();
            let metric_label = dataset.metric().label();
            let passes = match config.build_passes {
                BuildPasses::One => GraphBuildPasses::One,
                BuildPasses::Two => GraphBuildPasses::Two,
            };
            (
                CachedGraph::Cross(open_cross_dataset_graph(
                    &dataset,
                    cache_directory,
                    passes,
                    SEED,
                )?),
                queries,
                truth,
                dimensions,
                rows,
                metric_label,
            )
        };
    let reader = cached.reader();
    let zero_norm_rows = cached.zero_norm_rows();
    let zero_query_count = queries
        .chunks_exact(dimensions)
        .filter(|query| query.iter().all(|value| *value == 0.0))
        .count();
    let graph = reader.graph_node_blocks()?;
    let layout = graph.layout();
    let score_bytes = layout
        .code_bytes()
        .checked_add(12)
        .ok_or_else(|| io::Error::other("scored candidate byte count overflow"))?;
    let score_cache_lines = score_bytes.div_ceil(CACHE_LINE_BYTES);
    println!(
        "GRAPH_SEARCH_CONTEXT dataset={} rows={rows} dims={dimensions} metric={metric_label} build_profile=bench opt_level={} build_passes={build_passes_label} prefetch={prefetch_label} ef={} k={TOP_K} queries={} warmup_queries={WARMUP_QUERIES} run={} single_core=true zero_norm_rows={} zero_norm_queries={zero_query_count} padded_dims={} node_stride_bytes={} node_stride_cache_lines={} scored_candidate_bytes={} scored_candidate_cache_lines={} load_limit={LOAD_LIMIT:.2}",
        config.dataset_name,
        env!("ZEPPELIN_BENCH_OPT_LEVEL"),
        config.ef,
        config.queries,
        config.run,
        zero_norm_rows.len(),
        layout.padded_dims(),
        layout.stride(),
        (layout.stride() as usize).div_ceil(CACHE_LINE_BYTES),
        score_bytes,
        score_cache_lines,
    );
    print_taint_status(&taint, LOAD_LIMIT, "graph-search");

    let rescore = reader.rescore_f32()?;
    let mut searcher = GraphSearcher::new(graph, rescore)?;
    let available_queries = (queries.len() / dimensions).min(truth.len() / TOP_K);
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
    let exclusion_margin = zero_norm_rows.len().min(config.ef.saturating_sub(TOP_K));
    let search_k = TOP_K
        .checked_add(exclusion_margin)
        .ok_or_else(|| io::Error::other("zero-row exclusion width overflow"))?;

    let warmup = WARMUP_QUERIES.min(available_queries);
    for query_index in available_queries.saturating_sub(warmup)..available_queries {
        let query = query_row(&queries, query_index, dimensions)?;
        if !query.iter().all(|value| *value == 0.0) {
            let _ = searcher.search(
                GraphSearchRequest::new(query, search_k, config.ef, SEED ^ query_index as u64)
                    .with_prefetch(config.prefetch),
                None,
            )?;
        }
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
        let query = query_row(&queries, query_index, dimensions)?;
        let started = Instant::now();
        let mut returned = [u32::MAX; TOP_K];
        let counters = if query.iter().all(|value| *value == 0.0) {
            fill_zero_query_results(&mut returned, rows, zero_norm_rows)?;
            None
        } else {
            let result = searcher.search(
                GraphSearchRequest::new(query, search_k, config.ef, SEED ^ query_index as u64)
                    .with_observed_core_class(QueryCoreClass::Performance)
                    .with_prefetch(config.prefetch),
                None,
            )?;
            let mut returned_count = 0_usize;
            for candidate in result.candidates() {
                if zero_norm_rows.contains(&candidate.row_id()) {
                    continue;
                }
                let Some(slot) = returned.get_mut(returned_count) else {
                    break;
                };
                *slot = candidate.row_id();
                returned_count += 1;
            }
            if returned_count != TOP_K {
                return Err(io::Error::other(format!(
                    "query {query_index} returned {returned_count} nonzero neighbours, expected {TOP_K}"
                ))
                .into());
            }
            Some(result.counters())
        };
        elapsed_us.push(started.elapsed().as_secs_f64() * 1e6);
        let expected = truth_row(&truth, query_index)?;
        recall_hits += returned
            .iter()
            .filter(|row_id| expected.contains(&(**row_id as i32)))
            .count() as u64;
        let Some(counters) = counters else {
            continue;
        };
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
        "GRAPH_SEARCH_RESULT dataset={} rows={rows} dims={dimensions} metric={metric_label} build_passes={build_passes_label} prefetch={prefetch_label} ef={} k={TOP_K} queries={} run={} p50_us={p50_us:.3} recall_at_100={:.6} mean_hops={:.3} mean_candidates={:.3} mean_pushes={:.3} within_rsd_percent={within_rsd_percent:.3} zero_norm_rows={} zero_norm_queries={zero_query_count} node_stride_bytes={} scored_candidate_cache_lines={} qos_class={} qos_priority={} core_class={} load1={} taint={}",
        config.dataset_name,
        config.ef,
        config.queries,
        config.run,
        recall_hits as f64 / denominator,
        total_hops as f64 / query_denominator,
        total_candidates as f64 / query_denominator,
        total_pushes as f64 / query_denominator,
        zero_norm_rows.len(),
        layout.stride(),
        score_cache_lines,
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
        dataset_name: String::from("sift-128-euclidean"),
        build_passes: BuildPasses::Two,
        prefetch: TraversalPrefetch::Enabled,
        ef: DEFAULT_EF,
        queries: DEFAULT_QUERIES,
        run: 0,
        cache_directory: None,
        data_directory: workspace.join("tasks/cross-benchmark/data"),
    };
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        let value = arguments
            .next()
            .ok_or_else(|| io::Error::other(format!("{argument} needs a value")))?;
        match argument.as_str() {
            "--dataset" => config.dataset_name = value,
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
            "--cache-dir" => config.cache_directory = Some(PathBuf::from(value)),
            "--data-dir" => config.data_directory = PathBuf::from(value),
            _ => {
                return Err(io::Error::other(format!("unknown argument {argument}")).into());
            }
        }
    }
    Ok(config)
}

fn open_cached_sift_graph(
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

fn fill_zero_query_results(
    output: &mut [u32; TOP_K],
    rows: usize,
    excluded: &[u32],
) -> Result<(), Box<dyn Error>> {
    let mut count = 0_usize;
    for row in 0..rows {
        let row_id =
            u32::try_from(row).map_err(|_| io::Error::other("zero-query row id exceeds u32"))?;
        if excluded.contains(&row_id) {
            continue;
        }
        let Some(slot) = output.get_mut(count) else {
            return Ok(());
        };
        *slot = row_id;
        count += 1;
    }
    Err(io::Error::other(format!(
        "zero query found only {count} eligible rows, expected {TOP_K}"
    ))
    .into())
}

fn query_row(
    queries: &[f32],
    query_index: usize,
    dimensions: usize,
) -> Result<&[f32], Box<dyn Error>> {
    let start = query_index
        .checked_mul(dimensions)
        .ok_or_else(|| io::Error::other("query offset overflow"))?;
    let end = start
        .checked_add(dimensions)
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

#[cfg(test)]
mod tests {
    use super::{TOP_K, fill_zero_query_results};

    #[test]
    fn zero_cosine_query_returns_lowest_eligible_rows() {
        let mut output = [u32::MAX; TOP_K];
        fill_zero_query_results(&mut output, 200, &[0, 2, 5]).expect("enough eligible rows");
        assert_eq!(&output[..5], &[1, 3, 4, 6, 7]);
        assert!(!output.contains(&0));
        assert!(!output.contains(&2));
        assert!(!output.contains(&5));
    }
}
