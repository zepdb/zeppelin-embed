//! Item 0 ingest and concurrent ingest/query baselines.

use std::error::Error;
use std::hint::black_box;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Instant;

use tempfile::tempdir;
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, SearchOptions, Store};
use zeppelin_embed_bench::platform::memory_graph::verify_bench_profile;
use zeppelin_embed_bench::platform::taint::{
    detect_taint, format_load1, format_taint_labels, print_taint_status,
};

const DIMENSIONS: usize = 128;
const TOP_K: usize = 10;
const DEFAULT_MIXED_PRELOAD: usize = 10_000;
const DEFAULT_QUERIES: usize = 1_000;
const DEFAULT_WARMUPS: usize = 20;
const DEFAULT_LOAD_LIMIT: f64 = 3.0;
const PRELOAD_CHUNK_ROWS: usize = 1_000;
const MIXED_BATCH_ROWS: usize = 100;
const SEED: u64 = 0x7065_7266_305f_7772;
const PRELOAD_ROWS: [usize; 3] = [0, 10_000, 25_000];
const BATCH_ROWS: [usize; 3] = [1, 100, 1_000];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Probe {
    All,
    Ingest,
    Mixed,
}

#[derive(Clone, Copy, Debug)]
struct Config {
    smoke: bool,
    probe: Probe,
    mixed_preload: usize,
    queries: usize,
    warmups: usize,
    load_limit: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            smoke: false,
            probe: Probe::All,
            mixed_preload: DEFAULT_MIXED_PRELOAD,
            queries: DEFAULT_QUERIES,
            warmups: DEFAULT_WARMUPS,
            load_limit: DEFAULT_LOAD_LIMIT,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DurabilityArm {
    Derived,
    Durable,
}

impl DurabilityArm {
    const fn label(self) -> &'static str {
        match self {
            Self::Derived => "derived",
            Self::Durable => "durable",
        }
    }

    const fn options(self) -> OpenOptions {
        match self {
            Self::Derived => {
                OpenOptions::new().with_durability(DurabilityMode::Derived, CommitTier::Ordered)
            }
            Self::Durable => {
                OpenOptions::new().with_durability(DurabilityMode::Durable, CommitTier::Durable)
            }
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("perf-write-baseline: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let config = parse_config(std::env::args().skip(1))?;
    if !config.smoke {
        verify_bench_profile()?;
    }
    let taint = detect_taint(config.load_limit);
    println!(
        "PERF_WRITE_CONTEXT mode={} profile={} opt_level={} dimensions={DIMENSIONS} k={TOP_K} seed=0x{SEED:016x} load1={} load_limit={:.2} taint={}",
        if config.smoke { "smoke" } else { "measure" },
        if config.smoke { "test" } else { "bench" },
        env!("ZEPPELIN_BENCH_OPT_LEVEL"),
        format_load1(taint.load1),
        config.load_limit,
        format_taint_labels(&taint.taints),
    );
    print_taint_status(&taint, config.load_limit, "Item 0 write baseline");

    if matches!(config.probe, Probe::All | Probe::Ingest) {
        run_ingest_probe(config)?;
    }
    if matches!(config.probe, Probe::All | Probe::Mixed) {
        run_mixed_probe(config)?;
    }
    Ok(())
}

fn run_ingest_probe(config: Config) -> Result<(), Box<dyn Error>> {
    let preloads: &[usize] = if config.smoke { &[8] } else { &PRELOAD_ROWS };
    let batches: &[usize] = if config.smoke { &[3] } else { &BATCH_ROWS };
    for preload in preloads {
        for batch_rows in batches {
            let directory = tempdir()?;
            let store = Store::open(directory.path(), DurabilityArm::Derived.options())?;
            preload_store(&store, *preload, 0)?;
            let documents = fixture_documents(*preload, *batch_rows)?;
            let started = Instant::now();
            let acknowledgment = store.ingest(IngestBatch::new(documents))?;
            let elapsed = started.elapsed();
            if elapsed.is_zero() {
                return Err(io::Error::other("ingest timer had zero duration").into());
            }
            let docs_per_second = *batch_rows as f64 / elapsed.as_secs_f64();
            black_box(acknowledgment);
            println!(
                "PERF_INGEST_RESULT durability=derived preload_rows={preload} batch_rows={batch_rows} dimensions={DIMENSIONS} elapsed_us={:.6} docs_per_second={docs_per_second:.6}",
                elapsed.as_secs_f64() * 1e6,
            );
            store.close()?;
        }
    }
    Ok(())
}

fn run_mixed_probe(config: Config) -> Result<(), Box<dyn Error>> {
    for durability in [DurabilityArm::Derived, DurabilityArm::Durable] {
        let directory = tempdir()?;
        let store = Arc::new(Store::open(directory.path(), durability.options())?);
        preload_store(&store, config.mixed_preload, 0)?;
        for warmup in 0..config.warmups {
            let query = generated_vector(SEED.wrapping_add(warmup as u64));
            black_box(store.search(
                SearchRequest::new(&query),
                TOP_K,
                SearchOptions::default(),
                query_control(),
            )?);
        }

        let stop = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let batches_completed = Arc::new(AtomicUsize::new(0));
        let ingest_store = Arc::clone(&store);
        let ingest_stop = Arc::clone(&stop);
        let ingest_ready = Arc::clone(&ready);
        let ingest_failed = Arc::clone(&failed);
        let ingest_batches = Arc::clone(&batches_completed);
        let first_id = config.mixed_preload;
        let ingester = thread::Builder::new()
            .name("perf0-ingest".to_owned())
            .spawn(move || -> Result<(), String> {
                let mut first = first_id;
                while !ingest_stop.load(Ordering::Acquire) {
                    let documents = fixture_documents(first, MIXED_BATCH_ROWS)
                        .map_err(|error| error.to_string())?;
                    if let Err(error) = ingest_store.ingest(IngestBatch::new(documents)) {
                        ingest_failed.store(true, Ordering::Release);
                        return Err(error.to_string());
                    }
                    first = first
                        .checked_add(MIXED_BATCH_ROWS)
                        .ok_or_else(|| String::from("mixed ingest row id overflow"))?;
                    ingest_batches.fetch_add(1, Ordering::Relaxed);
                    ingest_ready.store(true, Ordering::Release);
                }
                Ok(())
            })?;

        while !ready.load(Ordering::Acquire) {
            if failed.load(Ordering::Acquire) {
                break;
            }
            thread::yield_now();
        }
        let measurement = (|| -> Result<(Vec<f64>, usize), Box<dyn Error>> {
            if failed.load(Ordering::Acquire) {
                return Err(io::Error::other("mixed ingester failed before measurement").into());
            }
            let mut elapsed_us = Vec::with_capacity(config.queries);
            let mut scan_threads = 0_usize;
            for query_index in 0..config.queries {
                let query = generated_vector(SEED ^ query_index as u64);
                let started = Instant::now();
                let outcome = store.search(
                    SearchRequest::new(&query),
                    TOP_K,
                    SearchOptions::default(),
                    query_control(),
                )?;
                elapsed_us.push(started.elapsed().as_secs_f64() * 1e6);
                scan_threads = outcome.diagnostics.counters.scan.threads_used;
                black_box(&outcome.candidates);
            }
            Ok((elapsed_us, scan_threads))
        })();
        stop.store(true, Ordering::Release);
        let ingest_result = ingester
            .join()
            .map_err(|_| io::Error::other("mixed ingester panicked"))?;
        ingest_result.map_err(io::Error::other)?;
        let (mut elapsed_us, scan_threads) = measurement?;
        let completed = batches_completed.load(Ordering::Acquire);
        if completed == 0 {
            return Err(io::Error::other("mixed ingester completed no batches").into());
        }
        elapsed_us.sort_by(f64::total_cmp);
        println!(
            "PERF_MIXED_RESULT durability={} preload_rows={} dimensions={DIMENSIONS} ingest_batch_rows={MIXED_BATCH_ROWS} queries={} k={TOP_K} p50_us={:.6} p99_us={:.6} ingest_batches={completed} ingested_docs={} client_threads=2 search_options=default scan_threads={} query_control=cancel",
            durability.label(),
            config.mixed_preload,
            config.queries,
            percentile(&elapsed_us, 0.50)?,
            percentile(&elapsed_us, 0.99)?,
            completed.saturating_mul(MIXED_BATCH_ROWS),
            scan_threads,
        );
        store.close()?;
    }
    Ok(())
}

fn preload_store(store: &Store, rows: usize, first_id: usize) -> Result<(), Box<dyn Error>> {
    let mut first = first_id;
    let end = first_id
        .checked_add(rows)
        .ok_or_else(|| io::Error::other("preload row range overflow"))?;
    while first < end {
        let count = (end - first).min(PRELOAD_CHUNK_ROWS);
        store.ingest(IngestBatch::new(fixture_documents(first, count)?))?;
        first = first
            .checked_add(count)
            .ok_or_else(|| io::Error::other("preload row id overflow"))?;
    }
    Ok(())
}

fn fixture_documents(first: usize, count: usize) -> Result<Vec<IngestDocument>, Box<dyn Error>> {
    let end = first
        .checked_add(count)
        .ok_or_else(|| io::Error::other("fixture row range overflow"))?;
    (first..end)
        .map(|row| {
            Ok(IngestDocument::new(
                DocumentVersion::new(
                    DocId::new(u128::try_from(row)?.saturating_add(1)),
                    Revision::new(1),
                ),
                generated_vector(SEED ^ row as u64),
            ))
        })
        .collect()
}

fn generated_vector(seed: u64) -> Vec<f32> {
    let mut random = SplitMix64::new(seed);
    (0..DIMENSIONS)
        .map(|_| random.open_unit() as f32 * 2.0 - 1.0)
        .collect()
}

fn query_control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}

fn percentile(sorted: &[f64], fraction: f64) -> Result<f64, Box<dyn Error>> {
    if sorted.is_empty() {
        return Err(io::Error::other("latency sample is empty").into());
    }
    let rank = (fraction * (sorted.len().saturating_sub(1)) as f64).round() as usize;
    sorted
        .get(rank.min(sorted.len().saturating_sub(1)))
        .copied()
        .ok_or_else(|| io::Error::other("latency percentile is unavailable").into())
}

fn parse_config(arguments: impl IntoIterator<Item = String>) -> Result<Config, Box<dyn Error>> {
    let mut config = Config::default();
    let mut arguments = arguments.into_iter();
    while let Some(flag) = arguments.next() {
        if flag == "--smoke" {
            config.smoke = true;
            config.mixed_preload = 32;
            config.queries = 3;
            config.warmups = 1;
            continue;
        }
        let value = arguments
            .next()
            .ok_or_else(|| io::Error::other(format!("{flag} requires a value")))?;
        match flag.as_str() {
            "--probe" => {
                config.probe = match value.as_str() {
                    "all" => Probe::All,
                    "ingest" => Probe::Ingest,
                    "mixed" => Probe::Mixed,
                    _ => {
                        return Err(
                            io::Error::other("--probe requires all, ingest, or mixed").into()
                        );
                    }
                }
            }
            "--mixed-preload" => config.mixed_preload = value.parse()?,
            "--queries" => config.queries = value.parse()?,
            "--warmups" => config.warmups = value.parse()?,
            "--load-limit" => config.load_limit = value.parse()?,
            _ => return Err(io::Error::other(format!("unknown argument {flag}")).into()),
        }
    }
    if config.queries == 0 {
        return Err(io::Error::other("--queries must be positive").into());
    }
    if !config.load_limit.is_finite() || config.load_limit < 0.0 {
        return Err(io::Error::other("--load-limit must be finite and non-negative").into());
    }
    Ok(config)
}

#[derive(Clone, Copy, Debug)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn open_unit(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 0.5) * (1.0 / 9_007_199_254_740_992.0)
    }
}
