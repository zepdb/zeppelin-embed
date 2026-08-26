//! Task 20 Part A / 27-B6b: measured scan-to-graph tier crossover at the
//! public `Store::search` seam.
//!
//! For every row count on the ladder the binary opens a fresh store, ingests
//! the first N SIFT base rows, seals one immutable segment, measures the
//! shipped Bit4+rescore scan p50 on one thread, forces a graph build through
//! `Store::maintain` with `graph_min_rows = 1`, measures the graph p50 and
//! its recall against brute-force truth over the same N rows, and applies the
//! task-20 decision rule (a segment earns a graph when scan p50 exceeds twice
//! the graph p50). Every latency is a cross-process median (BL-103): the
//! parent process re-executes itself once per process and aggregates the
//! children through `process_median`.

use std::error::Error;
use std::fs;
use std::hint::black_box;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::tempdir;
use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, Normalization, StoreEpoch,
};
use zeppelin_embed::fts::tokenizer::TokenizerConfig;
use zeppelin_embed::graph::search::GraphSearchProfile;
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchCandidate, SearchRequest,
};
use zeppelin_embed::lifecycle::{
    CancelToken, GraphSearchOptions, OpenOptions, QueryControl, SearchOptions, SearchTier, Store,
};
use zeppelin_embed::scan::ScanOptions;
use zeppelin_embed::tier::{MaintenanceBudget, MaintenanceStatus, TierThresholds};
use zeppelin_embed_bench::platform::memory_graph::verify_bench_profile;
use zeppelin_embed_bench::platform::taint::{detect_taint, format_load1, print_taint_status};
use zeppelin_embed_bench::process_median::ProcessMedian;

const DIMENSIONS: usize = 128;
const TOP_KS: [usize; 2] = [10, 100];
const TRUTH_K: usize = 100;
const INGEST_CHUNK_ROWS: usize = 10_000;
const DEFAULT_ROWS: [usize; 6] = [1_000, 3_000, 10_000, 30_000, 100_000, 300_000];
const DEFAULT_QUERIES: usize = 200;
const DEFAULT_WARMUPS: usize = 20;
const DEFAULT_PROCESSES: usize = 3;
const DEFAULT_LOAD_LIMIT: f64 = 2.0;
const DEFAULT_BUILD_LIMIT_SECS: u64 = 600;
const DEFAULT_DATA_DIR: &str = "tasks/cross-benchmark/data";
const BASE_FILE: &str = "sift-128-euclidean-base.f32bin";
const QUERY_FILE: &str = "sift-128-euclidean-query.f32bin";
const QUERY_FILE_ROWS: usize = 10_000;
const BASE_FILE_ROWS: usize = 1_000_000;
/// Task-20 decision rule: a segment earns a graph when scan p50 >= RULE_RATIO * graph p50.
const RULE_RATIO: f64 = 2.0;

fn main() {
    if let Err(error) = run() {
        eprintln!("tier-crossover: {error}");
        std::process::exit(1);
    }
}

#[derive(Clone, Debug)]
struct Config {
    data_dir: PathBuf,
    rows: Vec<usize>,
    queries: usize,
    warmups: usize,
    processes: usize,
    load_limit: f64,
    allow_taint: bool,
    build_limit: Duration,
    child: Option<usize>,
}

fn parse_config(arguments: &[String]) -> Result<Config, Box<dyn Error>> {
    let mut config = Config {
        data_dir: PathBuf::from(DEFAULT_DATA_DIR),
        rows: DEFAULT_ROWS.to_vec(),
        queries: DEFAULT_QUERIES,
        warmups: DEFAULT_WARMUPS,
        processes: DEFAULT_PROCESSES,
        load_limit: DEFAULT_LOAD_LIMIT,
        allow_taint: false,
        build_limit: Duration::from_secs(DEFAULT_BUILD_LIMIT_SECS),
        child: None,
    };
    let mut iterator = arguments.iter();
    while let Some(flag) = iterator.next() {
        let mut value = || {
            iterator
                .next()
                .ok_or_else(|| format!("{flag} requires a value"))
        };
        match flag.as_str() {
            "--data-dir" => config.data_dir = PathBuf::from(value()?),
            "--rows" => {
                config.rows = value()?
                    .split(',')
                    .map(|item| item.trim().replace('_', "").parse::<usize>())
                    .collect::<Result<Vec<_>, _>>()?;
            }
            "--queries" => config.queries = value()?.parse()?,
            "--warmups" => config.warmups = value()?.parse()?,
            "--processes" => config.processes = value()?.parse()?,
            "--load-limit" => config.load_limit = value()?.parse()?,
            "--build-limit-secs" => config.build_limit = Duration::from_secs(value()?.parse()?),
            "--allow-taint" => config.allow_taint = true,
            "--child" => config.child = Some(value()?.parse()?),
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    if config.rows.is_empty() || config.rows.iter().any(|rows| *rows < TRUTH_K) {
        return Err(format!("--rows must list counts of at least {TRUTH_K}").into());
    }
    if config.rows.iter().any(|rows| *rows > BASE_FILE_ROWS) {
        return Err(format!("--rows may not exceed {BASE_FILE_ROWS}").into());
    }
    if config.queries == 0 || config.queries + config.warmups > QUERY_FILE_ROWS {
        return Err(
            format!("--queries plus --warmups must be within 1..={QUERY_FILE_ROWS}").into(),
        );
    }
    if config.processes == 0 || config.processes.is_multiple_of(2) {
        return Err("--processes must be odd (1 for a single non-median run)".into());
    }
    Ok(config)
}

fn run() -> Result<(), Box<dyn Error>> {
    verify_bench_profile()?;
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let config = parse_config(&arguments)?;
    if let Some(index) = config.child {
        return run_ladder(&config, Some(index));
    }
    let taint = detect_taint(config.load_limit);
    println!(
        "TIER_CROSSOVER_CONTEXT dataset=sift-128-euclidean dimensions={DIMENSIONS} rows={} queries={} warmups={} processes={} ks={TOP_KS:?} tier_scan=Bit4+rescore thread_budget=1 graph_profile=SiftClass rule=scan_p50>={RULE_RATIO}*graph_p50 opt_level={} load1={} load_limit={:.2} build_limit_s={}",
        config
            .rows
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(","),
        config.queries,
        config.warmups,
        config.processes,
        env!("ZEPPELIN_BENCH_OPT_LEVEL"),
        format_load1(taint.load1),
        config.load_limit,
        config.build_limit.as_secs(),
    );
    print_taint_status(&taint, config.load_limit, "tier crossover");
    if !taint.taints.is_empty() && !config.allow_taint {
        return Err(format!(
            "aborting: machine is tainted (load1={} > {:.2} or sandboxed); a crossover constant measured under contention is void (BL-125). Pass --allow-taint for a non-authoritative smoke run.",
            format_load1(taint.load1),
            config.load_limit
        )
        .into());
    }
    if config.processes == 1 {
        println!(
            "TIER_CROSSOVER_NOTE processes=1 results are single-process, not a cross-process median"
        );
        return run_ladder(&config, None);
    }
    run_parent(&config, &arguments)
}

/// One measured cell as reported by a single process.
#[derive(Clone, Copy, Debug)]
struct Cell {
    rows: usize,
    k: usize,
    scan_p50_us: f64,
    scan_recall: f64,
    graph_p50_us: f64,
    graph_recall: f64,
    graph_build_ms: f64,
}

fn run_parent(config: &Config, arguments: &[String]) -> Result<(), Box<dyn Error>> {
    let executable = std::env::current_exe()?;
    let mut cells: Vec<Vec<Cell>> = Vec::with_capacity(config.processes);
    let mut stopped_at = None;
    for process in 0..config.processes {
        let mut child = Command::new(&executable)
            .args(arguments)
            .arg("--child")
            .arg(process.to_string())
            .stdout(Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().ok_or("child stdout was not captured")?;
        let mut observed = Vec::new();
        for line in BufReader::new(stdout).lines() {
            let line = line?;
            println!("{line}");
            if let Some(rest) = line.strip_prefix("TIER_CROSSOVER_CELL ") {
                observed.push(parse_cell(rest)?);
            } else if let Some(rest) = line.strip_prefix("TIER_CROSSOVER_STOP ") {
                let rows = field(rest, "rows")?.parse::<usize>()?;
                stopped_at = Some(stopped_at.map_or(rows, |current: usize| current.min(rows)));
            }
        }
        let status = child.wait()?;
        if !status.success() {
            return Err(format!("child process {process} failed: {status}").into());
        }
        cells.push(observed);
    }
    let completed_rows = config
        .rows
        .iter()
        .copied()
        .filter(|rows| stopped_at.is_none_or(|stop| *rows < stop))
        .collect::<Vec<_>>();
    let mut results = Vec::new();
    for rows in &completed_rows {
        for k in TOP_KS {
            let per_process = cells
                .iter()
                .map(|process| {
                    process
                        .iter()
                        .find(|cell| cell.rows == *rows && cell.k == k)
                        .copied()
                        .ok_or_else(|| format!("missing cell rows={rows} k={k} in a child"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let scan =
                ProcessMedian::new(per_process.iter().map(|cell| cell.scan_p50_us).collect())?;
            let graph =
                ProcessMedian::new(per_process.iter().map(|cell| cell.graph_p50_us).collect())?;
            let build =
                ProcessMedian::new(per_process.iter().map(|cell| cell.graph_build_ms).collect())?;
            let graph_recall =
                median_f64(per_process.iter().map(|cell| cell.graph_recall).collect());
            let scan_recall = median_f64(per_process.iter().map(|cell| cell.scan_recall).collect());
            let result = Cell {
                rows: *rows,
                k,
                scan_p50_us: scan.median(),
                scan_recall,
                graph_p50_us: graph.median(),
                graph_recall,
                graph_build_ms: build.median(),
            };
            print_result(&result, config.processes);
            println!(
                "TIER_CROSSOVER_SPREAD rows={rows} k={k} scan_p50_us_range={:.3}..{:.3} scan_spread_pct={:.2} graph_p50_us_range={:.3}..{:.3} graph_spread_pct={:.2} build_ms_range={:.1}..{:.1}",
                scan.minimum(),
                scan.maximum(),
                scan.spread_percent(),
                graph.minimum(),
                graph.maximum(),
                graph.spread_percent(),
                build.minimum(),
                build.maximum(),
            );
            results.push(result);
        }
    }
    if let Some(rows) = stopped_at {
        println!(
            "TIER_CROSSOVER_LADDER_STOPPED at_rows={rows} reason=graph_build_exceeded_limit build_limit_s={}",
            config.build_limit.as_secs()
        );
    }
    print_rules(&results);
    Ok(())
}

fn parse_cell(rest: &str) -> Result<Cell, Box<dyn Error>> {
    Ok(Cell {
        rows: field(rest, "rows")?.parse()?,
        k: field(rest, "k")?.parse()?,
        scan_p50_us: field(rest, "scan_p50_us")?.parse()?,
        scan_recall: field(rest, "scan_recall")?.parse()?,
        graph_p50_us: field(rest, "graph_p50_us")?.parse()?,
        graph_recall: field(rest, "graph_recall")?.parse()?,
        graph_build_ms: field(rest, "graph_build_ms")?.parse()?,
    })
}

fn field<'a>(line: &'a str, name: &str) -> Result<&'a str, Box<dyn Error>> {
    line.split_whitespace()
        .find_map(|item| {
            item.strip_prefix(name)
                .and_then(|rest| rest.strip_prefix('='))
        })
        .ok_or_else(|| format!("field {name} missing from `{line}`").into())
}

fn print_result(cell: &Cell, processes: usize) {
    println!(
        "TIER_CROSSOVER_RESULT rows={} k={} scan_p50_us={:.3} graph_p50_us={:.3} graph_recall={:.6} graph_build_ms={:.1} ratio={:.3} scan_recall={:.6} processes={processes}",
        cell.rows,
        cell.k,
        cell.scan_p50_us,
        cell.graph_p50_us,
        cell.graph_recall,
        cell.graph_build_ms,
        cell.scan_p50_us / cell.graph_p50_us,
        cell.scan_recall,
    );
}

fn print_rules(results: &[Cell]) {
    for k in TOP_KS {
        let mut ladder = results
            .iter()
            .filter(|cell| cell.k == k)
            .collect::<Vec<_>>();
        ladder.sort_by_key(|cell| cell.rows);
        let crossing = ladder
            .iter()
            .position(|cell| cell.scan_p50_us >= RULE_RATIO * cell.graph_p50_us);
        match crossing {
            Some(index) => {
                let below = index
                    .checked_sub(1)
                    .and_then(|previous| ladder.get(previous))
                    .map_or_else(|| String::from("none"), |cell| cell.rows.to_string());
                let cell = ladder[index];
                println!(
                    "TIER_CROSSOVER_RULE k={k} crossover_rows={} below={below} ratio_at_crossover={:.3} graph_recall_at_crossover={:.6}",
                    cell.rows,
                    cell.scan_p50_us / cell.graph_p50_us,
                    cell.graph_recall,
                );
            }
            None => {
                let last = ladder.last().map_or(0, |cell| cell.rows);
                println!(
                    "TIER_CROSSOVER_RULE k={k} crossover_rows=none below={last} note=scan_never_reached_{RULE_RATIO}x_graph_on_this_ladder"
                );
            }
        }
    }
}

/// Runs the whole ladder inside one process.
fn run_ladder(config: &Config, child: Option<usize>) -> Result<(), Box<dyn Error>> {
    let max_rows = config.rows.iter().copied().max().ok_or("empty ladder")?;
    let base = read_f32_prefix(&config.data_dir.join(BASE_FILE), max_rows * DIMENSIONS)?;
    let query_rows = config.warmups + config.queries;
    let queries = read_f32_prefix(&config.data_dir.join(QUERY_FILE), query_rows * DIMENSIONS)?;
    let mut results = Vec::new();
    for rows in &config.rows {
        let outcome = measure_rows(config, &base, &queries, *rows)?;
        match outcome {
            RowOutcome::Cells(cells) => {
                for cell in cells {
                    match child {
                        Some(process) => println!(
                            "TIER_CROSSOVER_CELL process={process} rows={} k={} scan_p50_us={:.3} scan_recall={:.6} graph_p50_us={:.3} graph_recall={:.6} graph_build_ms={:.1}",
                            cell.rows,
                            cell.k,
                            cell.scan_p50_us,
                            cell.scan_recall,
                            cell.graph_p50_us,
                            cell.graph_recall,
                            cell.graph_build_ms,
                        ),
                        None => print_result(&cell, 1),
                    }
                    results.push(cell);
                }
            }
            RowOutcome::BuildExceededLimit { elapsed } => {
                println!(
                    "TIER_CROSSOVER_STOP rows={rows} reason=graph_build_exceeded_limit elapsed_s={:.1} build_limit_s={}",
                    elapsed.as_secs_f64(),
                    config.build_limit.as_secs()
                );
                break;
            }
        }
    }
    if child.is_none() {
        print_rules(&results);
    }
    Ok(())
}

enum RowOutcome {
    Cells(Vec<Cell>),
    BuildExceededLimit { elapsed: Duration },
}

fn measure_rows(
    config: &Config,
    base: &[f32],
    queries: &[f32],
    rows: usize,
) -> Result<RowOutcome, Box<dyn Error>> {
    let directory = tempdir()?;
    let store = Store::open(
        directory.path(),
        OpenOptions::default().with_epoch(sift_epoch()),
    )?;
    let vectors = base
        .get(..rows * DIMENSIONS)
        .ok_or("base prefix shorter than the requested rows")?;
    let ingest_started = Instant::now();
    for (chunk_index, chunk) in vectors.chunks(INGEST_CHUNK_ROWS * DIMENSIONS).enumerate() {
        let first_row = chunk_index * INGEST_CHUNK_ROWS;
        let documents = chunk
            .chunks_exact(DIMENSIONS)
            .enumerate()
            .map(|(offset, vector)| {
                IngestDocument::new(
                    DocumentVersion::new(
                        DocId::new((first_row + offset) as u128 + 1),
                        Revision::new(1),
                    ),
                    vector.to_vec(),
                )
            })
            .collect::<Vec<_>>();
        store.ingest(IngestBatch::new(documents).with_epoch(sift_epoch().identity()))?;
    }
    store.seal()?;
    let segments = store.snapshot()?.segments().len();
    if segments != 1 {
        return Err(format!("expected one sealed segment at rows={rows}, found {segments}").into());
    }
    println!(
        "TIER_CROSSOVER_STORE rows={rows} ingest_seal_ms={:.1} segments={segments}",
        ingest_started.elapsed().as_secs_f64() * 1e3
    );

    let measured_queries = queries
        .get(config.warmups * DIMENSIONS..)
        .ok_or("query prefix shorter than warmups")?;
    let truth = brute_force_truth(vectors, measured_queries, rows);

    let scan_options =
        SearchOptions::new(ScanOptions { thread_budget: 1 }).with_tier(SearchTier::Scan);
    let mut scan_runs = Vec::with_capacity(TOP_KS.len());
    for k in TOP_KS {
        scan_runs.push(measure_tier(
            &store,
            scan_options,
            k,
            config,
            queries,
            &truth,
        )?);
    }

    let build_started = Instant::now();
    loop {
        let elapsed = build_started.elapsed();
        if elapsed >= config.build_limit {
            store.close()?;
            return Ok(RowOutcome::BuildExceededLimit { elapsed });
        }
        let report = store.maintain_with_test_thresholds(
            MaintenanceBudget {
                wall_time: config.build_limit - elapsed,
                bytes: u64::MAX,
            },
            TierThresholds { graph_min_rows: 1 },
        );
        match report.status {
            MaintenanceStatus::Complete if report.graphs_built >= 1 => break,
            MaintenanceStatus::Complete => {
                return Err(format!(
                    "maintenance completed without building a graph at rows={rows}: deferrals={:?}",
                    report.promotion_deferrals
                )
                .into());
            }
            MaintenanceStatus::BudgetExhausted => continue,
            MaintenanceStatus::Failed(error) => {
                return Err(format!("graph build failed at rows={rows}: {error}").into());
            }
        }
    }
    let graph_build_ms = build_started.elapsed().as_secs_f64() * 1e3;
    println!(
        "TIER_CROSSOVER_BUILD rows={rows} graph_build_ms={graph_build_ms:.1} checkpoint_batch_rows=64"
    );

    let graph_options = SearchOptions::new(ScanOptions { thread_budget: 1 }).with_tier(
        SearchTier::Graph(GraphSearchOptions::new(GraphSearchProfile::SiftClass)),
    );
    let mut cells = Vec::with_capacity(TOP_KS.len());
    for (k, scan) in TOP_KS.iter().zip(scan_runs) {
        let graph = measure_tier(&store, graph_options, *k, config, queries, &truth)?;
        cells.push(Cell {
            rows,
            k: *k,
            scan_p50_us: scan.p50_us,
            scan_recall: scan.recall,
            graph_p50_us: graph.p50_us,
            graph_recall: graph.recall,
            graph_build_ms,
        });
    }
    store.close()?;
    Ok(RowOutcome::Cells(cells))
}

struct TierRun {
    p50_us: f64,
    recall: f64,
}

fn measure_tier(
    store: &Store,
    options: SearchOptions,
    k: usize,
    config: &Config,
    queries: &[f32],
    truth: &[Vec<u32>],
) -> Result<TierRun, Box<dyn Error>> {
    for query in queries.chunks_exact(DIMENSIONS).take(config.warmups) {
        black_box(store.search(
            SearchRequest::new(query),
            k,
            options,
            QueryControl::Cancel(CancelToken::new()),
        )?);
    }
    let mut elapsed_us = Vec::with_capacity(config.queries);
    let mut hits = 0_usize;
    for (query, expected) in queries
        .chunks_exact(DIMENSIONS)
        .skip(config.warmups)
        .zip(truth)
    {
        let started = Instant::now();
        let outcome = store.search(
            SearchRequest::new(query),
            k,
            options,
            QueryControl::Cancel(CancelToken::new()),
        )?;
        elapsed_us.push(started.elapsed().as_secs_f64() * 1e6);
        hits += recall_hits(&outcome.candidates, expected, k);
    }
    elapsed_us.sort_by(f64::total_cmp);
    Ok(TierRun {
        p50_us: median_f64(elapsed_us),
        recall: hits as f64 / (config.queries * k) as f64,
    })
}

fn recall_hits(candidates: &[SearchCandidate], expected: &[u32], k: usize) -> usize {
    let expected = expected.get(..k).unwrap_or(expected);
    candidates
        .iter()
        .take(k)
        .filter_map(|candidate| candidate.document())
        .filter(|version| {
            let row = version.doc_id().get().saturating_sub(1);
            u32::try_from(row).is_ok_and(|row| expected.contains(&row))
        })
        .count()
}

/// Exact top-`TRUTH_K` rows per query by squared L2 over the first `rows` rows.
fn brute_force_truth(vectors: &[f32], queries: &[f32], rows: usize) -> Vec<Vec<u32>> {
    queries
        .chunks_exact(DIMENSIONS)
        .map(|query| {
            let mut scored = vectors
                .chunks_exact(DIMENSIONS)
                .take(rows)
                .enumerate()
                .map(|(row, vector)| {
                    let distance = query
                        .iter()
                        .zip(vector)
                        .map(|(q, v)| {
                            let delta = q - v;
                            delta * delta
                        })
                        .sum::<f32>();
                    (distance, row as u32)
                })
                .collect::<Vec<_>>();
            scored.sort_by(|left, right| {
                left.0
                    .total_cmp(&right.0)
                    .then_with(|| left.1.cmp(&right.1))
            });
            scored.iter().take(TRUTH_K).map(|(_, row)| *row).collect()
        })
        .collect()
}

fn read_f32_prefix(path: &Path, values: usize) -> Result<Vec<f32>, Box<dyn Error>> {
    let bytes = values.checked_mul(4).ok_or("prefix byte count overflow")?;
    let available = usize::try_from(fs::metadata(path)?.len())?;
    if available < bytes {
        return Err(format!("{} has {available} bytes, need {bytes}", path.display()).into());
    }
    let mut reader = BufReader::new(fs::File::open(path)?);
    let mut buffer = vec![0_u8; bytes];
    reader.read_exact(&mut buffer)?;
    Ok(buffer
        .chunks_exact(4)
        .map(|word| f32::from_le_bytes([word[0], word[1], word[2], word[3]]))
        .collect())
}

fn sift_epoch() -> StoreEpoch {
    let document = EmbeddingTower {
        model_id: "tier-crossover-sift".to_owned(),
        model_version: "1".to_owned(),
        weights_digest: vec![0x20],
        dims: DIMENSIONS as u32,
        normalization: Normalization::None,
        prompt_prefix: String::new(),
        max_tokens: 512,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    StoreEpoch {
        embedding: EmbeddingEpoch {
            query: document.clone(),
            document,
            alignment_digest: Vec::new(),
        },
        tokenizer: TokenizerConfig::text_default().epoch(),
    }
}

fn median_f64(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values.get(values.len() / 2).copied().unwrap_or(f64::NAN)
}
