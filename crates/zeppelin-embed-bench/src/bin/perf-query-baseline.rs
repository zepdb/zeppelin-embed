//! Item 0 query baselines at the lexical and public Store seams.

use std::error::Error;
use std::hint::black_box;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tempfile::tempdir;
use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, Normalization, StoreEpoch,
};
use zeppelin_embed::fts::bm25::Bm25Params;
use zeppelin_embed::fts::index::{DEFAULT_FIELD, Document, LexicalIndex, SegmentIndex};
use zeppelin_embed::fts::prune::{Strategy, search_pruned};
use zeppelin_embed::fts::query::LexicalQuery;
use zeppelin_embed::fts::search::TermQuery;
use zeppelin_embed::fts::tokenizer::{Analyzer, Profile, TokenizerConfig};
use zeppelin_embed::fusion::HybridQuery;
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, SearchOptions, Store};
use zeppelin_embed::tier::{MaintenanceBudget, MaintenanceStatus, PROVISIONAL_TIER_THRESHOLDS};
use zeppelin_embed_bench::platform::memory_graph::verify_bench_profile;
use zeppelin_embed_bench::platform::taint::{
    detect_taint, format_load1, format_taint_labels, print_taint_status,
};

const DIMENSIONS: usize = 128;
const TOP_K: usize = 10;
const DEFAULT_ROWS: usize = 10_000;
const DEFAULT_QUERIES: usize = 1_000;
const DEFAULT_WARMUPS: usize = 20;
const DEFAULT_LEX_ROWS: usize = 100_000;
const DEFAULT_LOAD_LIMIT: f64 = 3.0;
const INGEST_CHUNK_ROWS: usize = 10_000;
const SEED: u64 = 0x7065_7266_305f_7172;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Probe {
    All,
    Lex,
    Store,
}

#[derive(Clone, Debug)]
struct Config {
    smoke: bool,
    probe: Probe,
    rows: usize,
    queries: usize,
    warmups: usize,
    lex_rows: usize,
    load_limit: f64,
    store_directory: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            smoke: false,
            probe: Probe::All,
            rows: DEFAULT_ROWS,
            queries: DEFAULT_QUERIES,
            warmups: DEFAULT_WARMUPS,
            lex_rows: DEFAULT_LEX_ROWS,
            load_limit: DEFAULT_LOAD_LIMIT,
            store_directory: None,
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("perf-query-baseline: {error}");
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
        "PERF_QUERY_CONTEXT mode={} profile={} opt_level={} dimensions={DIMENSIONS} k={TOP_K} queries={} warmups={} seed=0x{SEED:016x} load1={} load_limit={:.2} taint={}",
        if config.smoke { "smoke" } else { "measure" },
        if config.smoke { "test" } else { "bench" },
        env!("ZEPPELIN_BENCH_OPT_LEVEL"),
        config.queries,
        config.warmups,
        format_load1(taint.load1),
        config.load_limit,
        format_taint_labels(&taint.taints),
    );
    print_taint_status(&taint, config.load_limit, "Item 0 query baseline");

    if matches!(config.probe, Probe::All | Probe::Lex) {
        run_lexical_probe(&config)?;
    }
    if matches!(config.probe, Probe::All | Probe::Store) {
        run_store_probe(&config)?;
    }
    Ok(())
}

fn run_lexical_probe(config: &Config) -> Result<(), Box<dyn Error>> {
    let analyzer = Analyzer::new(Profile::Code.config())?;
    let mut segment = SegmentIndex::new();
    for row in 0..config.lex_rows {
        let text = lexical_text(row);
        segment.push_document(&analyzer, &Document::with_text(&text))?;
    }
    let mut index = LexicalIndex::new();
    index.push_segment(segment)?;

    for (term_count, strategy) in [
        (2_usize, Strategy::BlockMaxWand),
        (4_usize, Strategy::BlockMaxMaxscore),
    ] {
        let terms = (0..term_count)
            .map(|term| format!("t{term}").into_bytes())
            .collect::<Vec<_>>();
        let query = TermQuery::flat(terms, &[DEFAULT_FIELD]);
        for _ in 0..config.warmups {
            black_box(search_pruned(
                &index,
                &query,
                TOP_K,
                Bm25Params::default(),
                strategy,
            )?);
        }
        let mut elapsed_us = Vec::with_capacity(config.queries);
        let mut returned = 0_usize;
        for _ in 0..config.queries {
            let started = Instant::now();
            let result = search_pruned(&index, &query, TOP_K, Bm25Params::default(), strategy)?;
            elapsed_us.push(started.elapsed().as_secs_f64() * 1e6);
            returned = result.hits.len();
            black_box(&result.hits);
        }
        elapsed_us.sort_by(f64::total_cmp);
        println!(
            "PERF_LEX_RESULT terms={term_count} dataset=synthetic_zipf rows={} k={TOP_K} queries={} p50_us={:.6} returned={returned}",
            config.lex_rows,
            config.queries,
            percentile(&elapsed_us, 0.50)?,
        );
    }
    Ok(())
}

fn run_store_probe(config: &Config) -> Result<(), Box<dyn Error>> {
    let epoch = benchmark_epoch();
    let graph_threshold = usize::try_from(PROVISIONAL_TIER_THRESHOLDS.graph_min_rows)?;
    let (store, _temporary_directory, graphs_built, store_source) =
        if let Some(directory) = config.store_directory.as_deref() {
            (
                Store::open(
                    directory,
                    OpenOptions::read_only().with_epoch(epoch.clone()),
                )?,
                None,
                0,
                "reused_read_only",
            )
        } else {
            let directory = tempdir()?;
            let store = Store::open(
                directory.path(),
                OpenOptions::default().with_epoch(epoch.clone()),
            )?;
            for first in (0..config.rows).step_by(INGEST_CHUNK_ROWS) {
                let count = (config.rows - first).min(INGEST_CHUNK_ROWS);
                let documents = fixture_documents(first, count, true)?;
                store.ingest(IngestBatch::new(documents).with_epoch(epoch.identity()))?;
            }
            store.seal()?;
            let graphs_built = build_earned_graph(&store, config.rows, graph_threshold)?;
            (store, Some(directory), graphs_built, "built")
        };
    let graphs_present = verify_store_shape(&store, config.rows, graph_threshold)?;
    println!(
        "PERF_STORE_BUILD rows={} graph_threshold={} graphs_built={graphs_built} graphs_present={graphs_present} source={store_source}",
        config.rows, graph_threshold,
    );

    let term_query = TermQuery::flat(
        vec![b"common".to_vec(), b"topic3".to_vec()],
        &[DEFAULT_FIELD],
    );
    let structured_query = LexicalQuery::prefix(b"topic".to_vec(), DEFAULT_FIELD);
    let hybrid_query = HybridQuery::new(TOP_K);

    warm_store_queries(
        &store,
        &term_query,
        &structured_query,
        &hybrid_query,
        config.warmups,
    )?;

    let mut hybrid_us = Vec::with_capacity(config.queries);
    let mut lexical_us = Vec::with_capacity(config.queries);
    let mut structured_us = Vec::with_capacity(config.queries);
    let mut hybrid_returned = 0_usize;
    let mut lexical_returned = 0_usize;
    let mut structured_returned = 0_usize;
    for query_index in 0..config.queries {
        let vector = generated_vector(SEED ^ query_index as u64);

        let started = Instant::now();
        let hybrid = store.search_hybrid(
            SearchRequest::new(&vector),
            &term_query,
            &hybrid_query,
            SearchOptions::default(),
            query_control(),
        )?;
        hybrid_us.push(started.elapsed().as_secs_f64() * 1e6);
        hybrid_returned = hybrid.hits.len();
        black_box(&hybrid.hits);

        let started = Instant::now();
        let lexical = store.search_lexical(&term_query, TOP_K, query_control())?;
        lexical_us.push(started.elapsed().as_secs_f64() * 1e6);
        lexical_returned = lexical.candidates.len();
        black_box(&lexical.candidates);

        let started = Instant::now();
        let structured =
            store.search_lexical_structured(&structured_query, TOP_K, 64, query_control())?;
        structured_us.push(started.elapsed().as_secs_f64() * 1e6);
        structured_returned = structured.candidates.len();
        black_box(&structured.candidates);
    }
    hybrid_us.sort_by(f64::total_cmp);
    lexical_us.sort_by(f64::total_cmp);
    structured_us.sort_by(f64::total_cmp);
    println!(
        "PERF_STORE_RESULT probe=hybrid rows={} dimensions={DIMENSIONS} k={TOP_K} queries={} p50_us={:.6} p95_us={:.6} returned={hybrid_returned} search_options=default query_control=cancel",
        config.rows,
        config.queries,
        percentile(&hybrid_us, 0.50)?,
        percentile(&hybrid_us, 0.95)?,
    );
    println!(
        "PERF_STORE_RESULT probe=lexical rows={} k={TOP_K} queries={} p50_us={:.6} returned={lexical_returned}",
        config.rows,
        config.queries,
        percentile(&lexical_us, 0.50)?,
    );
    println!(
        "PERF_STORE_RESULT probe=lexical_structured rows={} k={TOP_K} queries={} p50_us={:.6} returned={structured_returned}",
        config.rows,
        config.queries,
        percentile(&structured_us, 0.50)?,
    );
    store.close()?;
    Ok(())
}

fn build_earned_graph(
    store: &Store,
    rows: usize,
    graph_threshold: usize,
) -> Result<u64, Box<dyn Error>> {
    let mut graphs_built = 0_u64;
    if rows < graph_threshold {
        return Ok(graphs_built);
    }
    loop {
        let report = store.maintain(MaintenanceBudget {
            wall_time: Duration::from_secs(120),
            bytes: u64::MAX,
        });
        graphs_built = graphs_built.saturating_add(report.graphs_built);
        match report.status {
            MaintenanceStatus::Complete | MaintenanceStatus::BudgetExhausted
                if graphs_built > 0 =>
            {
                return Ok(graphs_built);
            }
            MaintenanceStatus::BudgetExhausted => {}
            MaintenanceStatus::Complete => {
                return Err(io::Error::other(format!(
                    "maintenance completed without a graph at {rows} rows"
                ))
                .into());
            }
            MaintenanceStatus::Failed(error) => {
                return Err(io::Error::other(format!(
                    "maintenance failed at {rows} rows: {error}"
                ))
                .into());
            }
        }
    }
}

fn verify_store_shape(
    store: &Store,
    expected_rows: usize,
    graph_threshold: usize,
) -> Result<usize, Box<dyn Error>> {
    let snapshot = store.snapshot()?;
    let mut rows = 0_usize;
    let mut graphs = 0_usize;
    for segment in snapshot.segments() {
        rows = rows
            .checked_add(usize::try_from(segment.meta().row_count)?)
            .ok_or_else(|| io::Error::other("sealed row count overflow"))?;
        if expected_rows >= graph_threshold {
            black_box(segment.graph_node_blocks()?);
            graphs = graphs.saturating_add(1);
        }
    }
    if rows != expected_rows {
        return Err(io::Error::other(format!(
            "store row count mismatch: expected {expected_rows}, found {rows}"
        ))
        .into());
    }
    if expected_rows >= graph_threshold && graphs == 0 {
        return Err(io::Error::other("graph-eligible store has no graph segment").into());
    }
    Ok(graphs)
}

fn warm_store_queries(
    store: &Store,
    term_query: &TermQuery,
    structured_query: &LexicalQuery,
    hybrid_query: &HybridQuery,
    warmups: usize,
) -> Result<(), Box<dyn Error>> {
    for query_index in 0..warmups {
        let vector = generated_vector(SEED.wrapping_add(query_index as u64));
        black_box(store.search_hybrid(
            SearchRequest::new(&vector),
            term_query,
            hybrid_query,
            SearchOptions::default(),
            query_control(),
        )?);
        black_box(store.search_lexical(term_query, TOP_K, query_control())?);
        black_box(store.search_lexical_structured(structured_query, TOP_K, 64, query_control())?);
    }
    Ok(())
}

fn fixture_documents(
    first: usize,
    count: usize,
    include_text: bool,
) -> Result<Vec<IngestDocument>, Box<dyn Error>> {
    (first..first.saturating_add(count))
        .map(|row| {
            let id = u128::try_from(row)?.saturating_add(1);
            let document = IngestDocument::new(
                DocumentVersion::new(DocId::new(id), Revision::new(1)),
                generated_vector(SEED ^ row as u64),
            );
            Ok(if include_text {
                document.with_text(store_text(row))
            } else {
                document
            })
        })
        .collect()
}

fn lexical_text(row: usize) -> String {
    let mut terms = Vec::new();
    for term in 0..12_usize {
        if row.is_multiple_of(term.saturating_add(1)) {
            terms.push(format!("t{term}"));
        }
    }
    if terms.is_empty() {
        String::from("filler")
    } else {
        terms.join(" ")
    }
}

fn store_text(row: usize) -> String {
    let mut random = SplitMix64::new(SEED ^ row as u64);
    format!(
        "common topic{} cluster{} seeded document",
        random.next_u64() % 16,
        random.next_u64() % 64,
    )
}

fn generated_vector(seed: u64) -> Vec<f32> {
    let mut random = SplitMix64::new(seed);
    (0..DIMENSIONS)
        .map(|_| random.open_unit() as f32 * 2.0 - 1.0)
        .collect()
}

fn benchmark_epoch() -> StoreEpoch {
    let document = EmbeddingTower {
        model_id: "perf0-synthetic".to_owned(),
        model_version: "1".to_owned(),
        weights_digest: vec![0x70],
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
            config.rows = 128;
            config.lex_rows = 256;
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
                    "lex" => Probe::Lex,
                    "store" => Probe::Store,
                    _ => return Err(io::Error::other("--probe requires all, lex, or store").into()),
                }
            }
            "--rows" => config.rows = value.parse()?,
            "--queries" => config.queries = value.parse()?,
            "--warmups" => config.warmups = value.parse()?,
            "--lex-rows" => config.lex_rows = value.parse()?,
            "--load-limit" => config.load_limit = value.parse()?,
            "--store-dir" => config.store_directory = Some(PathBuf::from(value)),
            _ => return Err(io::Error::other(format!("unknown argument {flag}")).into()),
        }
    }
    if config.rows == 0 || config.queries == 0 || config.lex_rows == 0 {
        return Err(io::Error::other("rows and query counts must be positive").into());
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
