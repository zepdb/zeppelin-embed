#![deny(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented,
    clippy::unwrap_used,
    unsafe_op_in_unsafe_fn
)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use zeppelin_embed::epoch::{EmbeddingEpoch, Normalization, StoreEpoch};
use zeppelin_embed::fts::tokenizer::TokenizerConfig;
use zeppelin_embed::ingest::SearchRequest;
use zeppelin_embed::lifecycle::{
    CancelToken, OpenOptions, QueryControl, ScanRescoreOptions, SearchOptions, SearchTier, Store,
};
use zeppelin_embed::tier::SegmentTier;
use zeppelin_embed_bench::beir::eval::{Run, RunEntry, mean_ndcg_at_k, mean_recall_at_k};
use zeppelin_embed_bench::beir::loader::{BeirCorpus, load_corpus};
use zeppelin_embed_bench::harness_json::{Value, from_slice, json, to_string, to_vec_pretty};
use zeppelin_embed_bench::user_bench::{
    ColdCell, QualityCell, Results, SteadyCell, Summary, percentiles, recall_at_k, render_tables,
    shuffled_order, verify_health,
};
use zeppelin_embed_text::bundle::Bundle;
use zeppelin_embed_text::runtime::ModelRuntime;
use zeppelin_embed_text::runtime::mlx::MlxRuntime;
use zeppelin_embed_text::tower::TowerRole;
use zeppelin_embed_text::{
    IngestOptions, Legs, MaintenanceBudget, MaintenanceStatus, QueryBackend, QueryOptions,
    TextDocument, TextHit, TextOpenOptions, TextQueryOutcome, TextStore,
};

const MIN_GRAPH_ROWS: u64 = 30_000;

type QueryVectors = BTreeMap<usize, Vec<f32>>;
type LoadedQueryRuntime = (Box<dyn ModelRuntime>, &'static str, &'static str);

fn main() {
    if let Err(error) = run() {
        eprintln!("text-user-bench: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let process_started = Instant::now();
    let cli = Cli::parse()?;
    match cli.command.as_str() {
        "ingest" => ingest(&cli),
        "promote" => promote(&cli),
        "verify" => verify(&cli),
        "steady" => steady(&cli, false),
        "steady-vector" => steady(&cli, true),
        "cold" => cold(&cli, process_started),
        "components" => components(&cli),
        "recall" => recall(&cli),
        "power-loop" => power_loop(&cli),
        "report" => report(&cli),
        command => Err(io::Error::other(format!("unknown subcommand {command:?}")).into()),
    }
}

struct Cli {
    command: String,
    values: BTreeMap<String, Vec<String>>,
}

impl Cli {
    fn parse() -> Result<Self, Box<dyn std::error::Error>> {
        let mut arguments = std::env::args().skip(1);
        let command = arguments
            .next()
            .ok_or_else(|| io::Error::other("a subcommand is required"))?;
        let mut values = BTreeMap::<String, Vec<String>>::new();
        while let Some(flag) = arguments.next() {
            if !flag.starts_with("--") {
                return Err(io::Error::other(format!("expected a flag, found {flag:?}")).into());
            }
            if flag == "--inputs" {
                let inputs = arguments.collect::<Vec<_>>();
                if inputs.is_empty() {
                    return Err(io::Error::other("--inputs requires at least one file").into());
                }
                values.insert(flag, inputs);
                break;
            }
            let value = arguments
                .next()
                .ok_or_else(|| io::Error::other(format!("{flag} requires a value")))?;
            values.entry(flag).or_default().push(value);
        }
        Ok(Self { command, values })
    }

    fn required(&self, flag: &str) -> Result<&str, Box<dyn std::error::Error>> {
        self.values
            .get(flag)
            .and_then(|values| values.first())
            .map(String::as_str)
            .ok_or_else(|| io::Error::other(format!("missing {flag}")).into())
    }

    fn optional(&self, flag: &str) -> Option<&str> {
        self.values
            .get(flag)
            .and_then(|values| values.first())
            .map(String::as_str)
    }

    fn usize(&self, flag: &str, default: usize) -> Result<usize, Box<dyn std::error::Error>> {
        match self.optional(flag) {
            Some(value) => value
                .parse::<usize>()
                .map_err(|error| io::Error::other(format!("invalid {flag}: {error}")).into()),
            None => Ok(default),
        }
    }

    fn u64(&self, flag: &str, default: u64) -> Result<u64, Box<dyn std::error::Error>> {
        match self.optional(flag) {
            Some(value) => value
                .parse::<u64>()
                .map_err(|error| io::Error::other(format!("invalid {flag}: {error}")).into()),
            None => Ok(default),
        }
    }

    fn inputs(&self) -> Result<&[String], Box<dyn std::error::Error>> {
        self.values
            .get("--inputs")
            .map(Vec::as_slice)
            .ok_or_else(|| io::Error::other("missing --inputs").into())
    }
}

fn ingest(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let beir_root = Path::new(cli.required("--beir-root")?);
    let corpus_name = cli.required("--corpus")?;
    let bundle_path = Path::new(cli.required("--bundle")?);
    let store_path = Path::new(cli.required("--store")?);
    let seal_every = cli.usize("--seal-every", 200_000)?;
    let embed_batch_size = cli.usize(
        "--embed-batch-size",
        IngestOptions::default().embed_batch_size,
    )?;
    let maintenance_ms = cli.u64("--maintenance-ms", 0)?;
    let cache_limit = cli
        .optional("--mlx-cache-limit-bytes")
        .map(|value| value.parse::<usize>())
        .transpose()?;
    let clear_ms = cli
        .optional("--mlx-clear-cache-ms")
        .map(|value| value.parse::<u64>())
        .transpose()?;
    let sample_ms = cli.u64("--memory-sample-ms", 1_000)?;
    if embed_batch_size == 0 || sample_ms == 0 || clear_ms == Some(0) {
        return Err(io::Error::other("embed batch and timer intervals must be positive").into());
    }
    let mut memory_out = cli
        .optional("--memory-out")
        .map(std::fs::File::create)
        .transpose()?;
    if let Some(parent) = store_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let corpus = load_corpus(beir_root, corpus_name)?;
    let mut id_table = String::new();
    let mut documents = Vec::with_capacity(corpus.documents.len());
    for (index, document) in corpus.documents.iter().enumerate() {
        let caller_id = u128::try_from(index.saturating_add(1))?;
        writeln!(&mut id_table, "{caller_id}\t{}", document.id)?;
        documents.push(TextDocument::new(
            caller_id,
            1,
            format!("{}\n{}", document.title, document.text),
        ));
    }
    std::fs::write(id_map_path(store_path, corpus_name)?, id_table)?;
    let store = TextStore::open(store_path, bundle_path, Default::default())?;
    let after_open = memory_counters()?;
    let old_cache_limit = if let Some(limit) = cache_limit {
        let mut old = 0;
        // SAFETY: the out pointer is valid; MLX locks its allocator internally.
        mlx_check(
            unsafe { mlx_set_cache_limit(&mut old, limit) },
            "mlx_set_cache_limit",
        )?;
        Some(old)
    } else {
        None
    };
    // Reset only the MLX peak immediately before ingest; macOS lifetime peak
    // intentionally still includes startup and model loading.
    // SAFETY: this no-argument MLX call takes the allocator mutex internally.
    mlx_check(unsafe { mlx_reset_peak_memory() }, "mlx_reset_peak_memory")?;
    let initial = memory_counters()?;
    let started = Instant::now();
    MemorySample {
        t_seconds: 0.0,
        counters: initial,
    }
    .write_sample(memory_out.as_mut())?;
    // Clones share the file offset. Main writes only before spawn and after
    // join, so the initial, periodic and final lines cannot overlap.
    let sampler_out = memory_out
        .as_ref()
        .map(std::fs::File::try_clone)
        .transpose()?;
    let (stop, receiver) = mpsc::channel();
    let sampler = std::thread::Builder::new()
        .name("ze-bench-memory".to_owned())
        .spawn(move || {
            sample_memory(
                &receiver,
                started,
                Duration::from_millis(sample_ms),
                clear_ms.map(Duration::from_millis),
                sampler_out,
            )
        })?;
    let result = store.ingest_text(
        &documents,
        IngestOptions {
            seal_every,
            embed_batch_size,
            maintenance_wall_time: Duration::from_millis(maintenance_ms),
            ..Default::default()
        },
    );
    // Always stop and join, including when ingest failed. Take the final
    // snapshot after join so sample timestamps and log writes stay ordered.
    drop(stop);
    let sampled = sampler
        .join()
        .map_err(|_| io::Error::other("memory sampler panicked"))?;
    let after_ingest = memory_counters()?;
    let final_sample = MemorySample {
        t_seconds: started.elapsed().as_secs_f64(),
        counters: after_ingest,
    };
    final_sample.write_sample(memory_out.as_mut())?;
    let mut samples = vec![MemorySample {
        t_seconds: 0.0,
        counters: initial,
    }];
    samples.extend(sampled?);
    samples.push(final_sample);
    let maxima = memory_maxima(
        &samples
            .iter()
            .map(|sample| sample.counters)
            .collect::<Vec<_>>(),
    );
    let report = result?;
    let health = store.health()?;
    let output = json!({
        "kind": "ingest",
        "memory": {
            "units": "bytes",
            "after_open": after_open.json(),
            "after_ingest": after_ingest.json(),
            "sample_maxima": maxima.json(),
            "sample_count": samples.len(),
            "memory_sample_ms": sample_ms,
            "memory_out": cli.optional("--memory-out"),
            "memory_out_format": "jsonl; each sample flushed to file and stderr",
            "embed_batch": embed_batch_size,
            "mlx_cache_limit_bytes": cache_limit,
            "old_mlx_cache_limit_bytes": old_cache_limit,
            "mlx_clear_cache_ms": clear_ms,
            "mlx_peak_scope": "reset immediately before ingest; after_open precedes reset",
            "sampling_policy": "non-atomic native diagnostics; pinned MLX getters read unlocked size_t fields",
            "documents_per_second": report.documents as f64 / report.elapsed.as_secs_f64(),
        },
        "bundle": bundle_path,
        "store": store_path,
        "corpus": corpus_name,
        "documents": report.documents,
        "chunks": report.chunks,
        "tokens": report.tokens,
        "embed_batches": report.embed_batches,
        "embed_batch_size": embed_batch_size,
        "seals": report.seals,
        "generation": report.generation,
        "max_in_flight_batches": report.max_in_flight_batches,
        "all_threads_joined": report.all_threads_joined,
        "elapsed_seconds": report.elapsed.as_secs_f64(),
        "segments": segments_json(&health),
    });
    store.close()?;
    print_json(&output)
}

fn promote(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let bundle_path = Path::new(cli.required("--bundle")?);
    let store_path = Path::new(cli.required("--store")?);
    let wall_seconds = cli.u64("--wall-secs", 600)?;
    let store = TextStore::open(store_path, bundle_path, Default::default())?;
    let started = Instant::now();
    let mut graphs_built = 0_u64;
    let mut checkpoints_resumed = 0_u64;
    let mut calls = 0_u64;
    loop {
        let maintenance = store.maintain(MaintenanceBudget {
            wall_time: Duration::from_secs(wall_seconds),
            bytes: u64::MAX,
        })?;
        calls = calls.saturating_add(1);
        graphs_built = graphs_built.saturating_add(maintenance.graphs_built);
        checkpoints_resumed = checkpoints_resumed.saturating_add(maintenance.checkpoints_resumed);
        if !maintenance.promotion_deferrals.is_empty() {
            return Err(io::Error::other(format!(
                "promotion deferrals: {:?}",
                maintenance.promotion_deferrals
            ))
            .into());
        }
        match maintenance.status {
            MaintenanceStatus::Complete => break,
            MaintenanceStatus::BudgetExhausted => {}
            MaintenanceStatus::Failed(error) => {
                return Err(io::Error::other(format!("maintenance failed: {error}")).into());
            }
        }
    }
    let health = store.health()?;
    verify_health(&health, SegmentTier::SealedGraph, MIN_GRAPH_ROWS)?;
    let output = json!({
        "kind": "promote",
        "bundle": bundle_path,
        "store": store_path,
        "status": "Complete",
        "calls": calls,
        "graphs_built": graphs_built,
        "checkpoints_resumed": checkpoints_resumed,
        "promotion_deferrals": [],
        "maintenance_seconds": started.elapsed().as_secs_f64(),
        "segments": segments_json(&health),
    });
    store.close()?;
    print_json(&output)
}

fn verify(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let bundle_path = Path::new(cli.required("--bundle")?);
    let store_path = Path::new(cli.required("--store")?);
    let expected = parse_segment_tier(cli.required("--expect")?)?;
    let store = TextStore::open(store_path, bundle_path, Default::default())?;
    let health = store.health()?;
    let rows = verify_health(&health, expected, MIN_GRAPH_ROWS)?;
    let dimensions = Bundle::open(bundle_path)?.query_tower().embedding.dims;
    let output = json!({
        "kind": "verify",
        "bundle": bundle_path,
        "store": store_path,
        "expected": segment_tier_name(expected),
        "sealed_rows": rows,
        "dimensions": dimensions,
        "epoch": { "embedding": store.epoch().embedding.value(), "tokenizer": store.epoch().tokenizer.value() },
        "query_backend": backend_name(store.query_backend()),
        "promotion_deferrals": [],
        "segments": segments_json(&health),
    });
    store.close()?;
    print_json(&output)
}

fn steady(cli: &Cli, caller_vector: bool) -> Result<(), Box<dyn std::error::Error>> {
    let beir_root = Path::new(cli.required("--beir-root")?);
    let corpus_name = cli.required("--corpus")?;
    let bundle_path = Path::new(cli.required("--bundle")?);
    let store_path = Path::new(cli.required("--store")?);
    let legs = if caller_vector {
        Legs::Dense
    } else {
        parse_legs(cli.required("--legs")?)?
    };
    let requested_tier = parse_search_tier(cli.required("--tier")?)?;
    let rounds = cli.usize("--rounds", 3)?;
    let warm = cli.usize("--warm", 20)?;
    let k = cli.usize("--k", 10)?;
    let seed = parse_seed(cli.required("--seed")?)?;
    let rescore = parse_scan_rescore(cli, requested_tier)?;
    let mut query_options = QueryOptions::new(k)
        .with_legs(legs)
        .with_optional_tier(requested_tier);
    if let Some(config) = rescore {
        query_options = query_options.with_scan_rescore(config);
    }
    let out = Path::new(cli.required("--out")?);
    let corpus = load_corpus(beir_root, corpus_name)?;
    let queries = test_queries(&corpus)?;
    let id_map = read_id_map(store_path, corpus_name)?;
    let order = shuffled_order(queries.len(), seed);
    let (vectors, vector_backend) = if caller_vector {
        prepare_query_vectors(bundle_path, &queries, &order)?
    } else {
        (BTreeMap::new(), "(none)")
    };
    let open_options = if caller_vector {
        TextOpenOptions::new(OpenOptions::read_only(), TokenizerConfig::text_default())
    } else {
        TextOpenOptions::default()
    };
    let store = TextStore::open(store_path, bundle_path, open_options)?;
    let backend = if legs == Legs::Lexical {
        "(none)"
    } else {
        backend_name(store.query_backend())
    };
    let epoch = store.epoch();
    let physical_tier = verify_timing_store(&store, store_path)?;
    let health = store.health()?;
    // TextStore validates the bundle's document epoch; inspection adopts that
    // persisted epoch. Both handles are read-only for the caller-vector run.
    let vector_store = if caller_vector {
        Some(Store::open_for_inspection(store_path)?)
    } else {
        None
    };
    let mut search_options = SearchOptions::default();
    if let Some(tier) = requested_tier {
        search_options = search_options.with_tier(tier);
    }
    if let Some(config) = rescore {
        search_options = search_options.with_scan_rescore(config);
    }
    let mut parity = Vec::new();
    let mut equal_rankings = 0_usize;
    if let Some(core) = &vector_store {
        if backend != vector_backend {
            return Err(
                io::Error::other("caller-vector and TextStore query backends differ").into(),
            );
        }
        for index in &order {
            let query = queries
                .get(*index)
                .ok_or_else(|| io::Error::other("query index is out of bounds"))?;
            let vector = vectors
                .get(index)
                .ok_or_else(|| io::Error::other("missing query vector"))?;
            let dense = store.query_text(&query.text, query_options)?;
            let caller = caller_vector_query(core, vector, k, search_options, epoch)?;
            let equal = caller
                .hits
                .iter()
                .map(|hit| hit.doc_id)
                .eq(dense.iter().map(|hit| hit.doc_id));
            equal_rankings += usize::from(equal);
            parity.push(json!({"query_id": query.id, "equal": equal}));
        }
        store.close()?;
    }
    let execute_query = |vector: &[f32], text: &str| match &vector_store {
        Some(core) => caller_vector_query(core, vector, k, search_options, epoch),
        None => store
            .query_text_with_diagnostics(text, query_options)
            .map_err(Into::into),
    };
    for index in order.iter().cycle().take(warm) {
        let query = queries
            .get(*index)
            .ok_or_else(|| io::Error::other("warmup query index is out of bounds"))?;
        let vector = match &vector_store {
            Some(_) => vectors
                .get(index)
                .ok_or_else(|| io::Error::other("missing warmup vector"))?
                .as_slice(),
            None => &[],
        };
        let _ = execute_query(vector, &query.text)?;
    }
    let mut samples = Vec::with_capacity(queries.len().saturating_mul(rounds));
    let mut run = Run::new();
    let mut rankings = Vec::with_capacity(queries.len());
    let mut query_samples = Vec::with_capacity(queries.len().saturating_mul(rounds));
    for round in 0..rounds {
        for index in &order {
            let query = queries
                .get(*index)
                .ok_or_else(|| io::Error::other("query index is out of bounds"))?;
            let vector = match &vector_store {
                Some(_) => vectors
                    .get(index)
                    .ok_or_else(|| io::Error::other("missing query vector"))?
                    .as_slice(),
                None => &[],
            };
            let started = Instant::now();
            let outcome = execute_query(vector, &query.text)?;
            let latency_ms = started.elapsed().as_secs_f64() * 1_000.0;
            samples.push(latency_ms);
            let entries = run_entries(&outcome.hits, &id_map)?;
            query_samples.push(query_sample_json(
                &query.id, round, latency_ms, &outcome, &entries,
            ));
            if round == 0 {
                rankings.push(json!({
                    "query_id": query.id,
                    "doc_ids": entries.iter().map(|entry| entry.doc_id.as_str()).collect::<Vec<_>>(),
                }));
                run.insert(query.id.clone(), entries);
            }
        }
    }
    let summary = percentiles(&samples)
        .ok_or_else(|| io::Error::other("steady-state sample set is empty"))?;
    let ndcg = mean_ndcg_at_k(&run, &corpus.qrels, k);
    let dimensions = Bundle::open(bundle_path)?.query_tower().embedding.dims;
    let mut output = json!({
        "kind": if caller_vector { "steady-vector" } else { "steady" },
        "backend": backend,
        "leg": if caller_vector { "CallerVector" } else { leg_name(legs) },
        "store_tier": segment_tier_short(physical_tier),
        "query_tier": requested_tier.map_or("unset", search_tier_name),
        "scan_rescore": scan_rescore_json(rescore),
        "corpus": corpus_name,
        "documents": corpus.documents.len(),
        "dimensions": dimensions,
        "queries": queries.len(),
        "rounds": rounds,
        "warm": warm,
        "k": k,
        "seed": seed,
        "samples": samples.len(),
        "summary": summary_json(summary),
        "ndcg_at_10": (k == 10).then_some(ndcg),
        "recall_at_10": (k == 10).then(|| mean_recall_at_k(&run, &corpus.qrels, k)),
        "unique_parent_ndcg_at_k": ndcg,
        "metric_policy": "first parent occurrence, compact unique parent ranks; no retrieval fill",
        "chunk_policy": "raw chunk ranks retained in query_samples; qrels judge parents, not chunks",
        "instrumentation_enabled": zeppelin_embed::diag::QUERY_TIMING_ENABLED,
        "epoch": { "embedding": store.epoch().embedding.value(), "tokenizer": store.epoch().tokenizer.value() },
        "segments": segments_json(&health),
        "latencies_ms": samples,
        "rankings": rankings,
        "query_samples": query_samples,
    });
    if caller_vector {
        let object = output
            .as_object_mut()
            .ok_or_else(|| io::Error::other("steady output is not an object"))?;
        object.insert("equal_rankings".to_owned(), json!(equal_rankings));
        object.insert("parity".to_owned(), json!(parity));
        object.insert("timing_policy".to_owned(), json!("core search_with_text and owned text materialization; embedding and parity excluded"));
    }
    write_and_print_json(out, &output)?;
    match vector_store {
        Some(core) => core.close()?,
        None => store.close()?,
    }
    Ok(())
}

fn prepare_query_vectors(
    bundle_path: &Path,
    queries: &[&zeppelin_embed_bench::beir::BeirQuery],
    order: &[usize],
) -> Result<(QueryVectors, &'static str), Box<dyn std::error::Error>> {
    let bundle = Arc::new(Bundle::open(bundle_path)?);
    let (mut runtime, _, vector_backend) = load_query_runtime(&bundle, bundle_path)?;
    // Every query embedding is evaluated and normalized before any timing.
    let mut vectors = BTreeMap::new();
    for index in order {
        let query = queries
            .get(*index)
            .ok_or_else(|| io::Error::other("query index is out of bounds"))?;
        let tokens = bundle.tokenize_query(&query.text)?;
        let embedded = runtime.embed_batch(&tokens)?;
        let tower = &bundle.query_tower().embedding;
        if embedded.rows() != 1 || embedded.dims() != tower.dims as usize {
            return Err(io::Error::other("query embedding shape mismatch").into());
        }
        let mut vector = embedded.into_values();
        if tower.normalization == Normalization::L2 {
            let norm = vector
                .iter()
                .map(|v| f64::from(*v).powi(2))
                .sum::<f64>()
                .sqrt();
            if !norm.is_finite() || norm <= 0.0 {
                return Err(io::Error::other("query embedding has invalid norm").into());
            }
            for value in &mut vector {
                *value = (f64::from(*value) / norm) as f32;
            }
            let squared_norm = vector.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>();
            if (squared_norm - 1.0).abs() > 1.0e-5 {
                return Err(io::Error::other("query embedding is not unit length").into());
            }
        }
        vectors.insert(*index, vector);
    }
    drop(runtime);
    Ok((vectors, vector_backend))
}

// Same core search and owned text materialization as TextStore's dense leg.
fn caller_vector_query(
    store: &Store,
    vector: &[f32],
    k: usize,
    options: SearchOptions,
    epoch: zeppelin_embed::epoch::EpochIdentity,
) -> Result<TextQueryOutcome, Box<dyn std::error::Error>> {
    let (outcome, hits) = store.search_with_text(
        SearchRequest::new(vector),
        k,
        options,
        QueryControl::Cancel(CancelToken::new()),
        |outcome, materializer| {
            outcome
                .candidates
                .iter()
                .enumerate()
                .map(|(rank, candidate)| {
                    let row = materializer.text(rank)?;
                    let encoded = row.document.doc_id().get();
                    let squared_l2 = -f64::from(candidate.score());
                    Ok(TextHit {
                        doc_id: encoded >> 32,
                        revision: row.document.revision().get(),
                        chunk: u32::try_from(encoded & u128::from(u32::MAX))?,
                        text: row.text,
                        score: -squared_l2,
                        vector_squared_l2: Some(squared_l2),
                        lexical_bm25: None,
                        epoch,
                    })
                })
                .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()
        },
    )?;
    Ok(TextQueryOutcome {
        hits: hits?,
        diagnostics: Some(outcome.diagnostics),
        timings: None,
        backend: None,
        query_tokens: 0,
        embedding_calls: 0,
    })
}

// JSON construction stays outside the measured query boundary. Raw chunk
// identity and score bits make later evaluator changes independently replayable.
fn query_sample_json(
    query_id: &str,
    round: usize,
    latency_ms: f64,
    outcome: &TextQueryOutcome,
    entries: &[RunEntry],
) -> Value {
    let milliseconds = |duration: Duration| duration.as_secs_f64() * 1_000.0;
    let chunks = outcome
        .hits
        .iter()
        .zip(entries)
        .map(|(hit, entry)| {
            json!({
                "parent_id": entry.doc_id,
                "caller_id": hit.doc_id.to_string(),
                "revision": hit.revision,
                "chunk": hit.chunk,
                "score": hit.score,
                "score_bits": hit.score.to_bits(),
                "vector_squared_l2_bits": hit.vector_squared_l2.map(f64::to_bits),
                "lexical_bm25_bits": hit.lexical_bm25.map(f64::to_bits),
            })
        })
        .collect::<Vec<_>>();
    let backend = outcome.backend.map(|backend| json!({
        "runtime": backend.runtime.name,
        "requested_compute_units": format!("{:?}", backend.requested_compute_units),
        "observed_compute_units": backend.observed_compute_units.map(|units| format!("{units:?}")),
        "sequence_length": backend.sequence_length,
    }));
    let text_spans = outcome.timings.map(|timing| {
        json!({
            "tokenization": milliseconds(timing.tokenization),
            "lexical_analysis": milliseconds(timing.lexical_analysis),
            "embedding_queue": milliseconds(timing.embedding_queue),
            "embedding_evaluation": milliseconds(timing.embedding_evaluation),
            "embedding_normalization": milliseconds(timing.embedding_normalization),
            "retrieval": milliseconds(timing.retrieval),
            "materialization": milliseconds(timing.materialization),
            "end_to_end": milliseconds(timing.end_to_end),
        })
    });
    let diagnostics = outcome.diagnostics.as_ref().map(|diag| {
        let core_spans = diag.timings.map(|timing| json!({
            "admission": milliseconds(timing.admission),
            "vector": milliseconds(timing.vector),
            "lexical_queue": milliseconds(timing.lexical_queue),
            "lexical": milliseconds(timing.lexical),
            "fusion_cross_fill": milliseconds(timing.fusion_cross_fill),
        }));
        let plans = diag.plan.iter().map(|plan| json!({
            "source": format!("{:?}", plan.source),
            "tier": format!("{:?}", plan.tier),
            "branch": format!("{:?}", plan.branch),
            "filter_mode": format!("{:?}", plan.filter_mode),
            "filter_cardinality": plan.filter_cardinality,
            "approximate": plan.approximate,
            "fallback": format!("{:?}", plan.fallback),
            "scan_reason": plan.scan_reason.map(|reason| format!("{reason:?}")),
            "ef_requested": plan.ef_requested,
            "ef_effective": plan.ef_effective,
        })).collect::<Vec<_>>();
        let hybrid = diag.hybrid.as_ref().map(|report| json!({
            "normalization_policy_version": report.normalization_policy_version,
            "lexical_full_materializations": report.lexical_full_materializations,
            "provenance": {
                "vector_precision": format!("{:?}", report.provenance.vector_precision),
                "vector_coverage": format!("{:?}", report.provenance.vector_coverage),
                "lexical_coverage": format!("{:?}", report.provenance.lexical_coverage),
                "cross_scores_complete": report.provenance.cross_scores_complete,
            },
            "final_window": report.window,
            "final_vector_returned": report.vector_returned,
            "final_lexical_returned": report.lexical_returned,
            "cross_filled_vector": report.total_cross_filled_vector,
            "cross_filled_lexical": report.total_cross_filled_lexical,
            "vector_candidates_produced": report.vector_candidates_produced,
            "lexical_candidates_produced": report.lexical_candidates_produced,
        }));
        let fusion = diag.fusion.as_ref().map(|report| json!({
            "method": format!("{:?}", report.method),
            "effective_alpha": report.effective_alpha,
            "rounds": report.rounds,
            "termination": format!("{:?}", report.termination),
        }));
        let counters = &diag.counters;
        json!({
            "snapshot_generation": diag.snapshot_generation,
            "indexed_through_seq": diag.indexed_through_seq.get(),
            "approximate_membership": diag.approximate,
            "returned_scores_full_precision": diag.exact_rescore,
            // Preserve the raw producer report without treating a fusion stop
            // reason as a proof that graph candidate membership is exhaustive.
            "coverage_certificate": diag.fusion.as_ref().filter(|fusion| matches!(fusion.termination,
                zeppelin_embed::fusion::FusionTermination::StableBound
                | zeppelin_embed::fusion::FusionTermination::ListsExhausted
                | zeppelin_embed::fusion::FusionTermination::BudgetFullMaterialization
            )).map(|fusion| format!("{:?}", fusion.termination)),
            "requested_k": diag.requested_k,
            "returned": diag.returned,
            "budget_exhausted": diag.budget_exhausted,
            "elapsed_ms": milliseconds(diag.elapsed),
            "core_spans_ms": core_spans,
            "span_policy": "inclusive wall spans; vector and lexical overlap; round spans accumulate",
            "plans": plans,
            "fusion": fusion,
            "hybrid": hybrid,
            "scan_rescore": diag.scan_rescore.map(|work| json!({
                "coarse_rows": work.coarse_rows,
                "eligible_rows": work.eligible_rows,
                "candidates_rescored": work.candidates_rescored,
                "coarse_bytes": work.coarse_bytes,
                "rescore_bytes": work.rescore_bytes,
                "byte_scope": "coordinate payload only; factor metadata excluded; hybrid cross-fill separate",
            })),
            "materialization": diag.materialization.map(|work| json!({
                "row_lookups": work.row_lookups,
                "text_copies": work.text_copies,
                "text_bytes": work.text_bytes,
            })),
            "counters": {
                "vector_coordinates": counters.scan.dims_touched,
                "vector_bytes": counters.scan.bytes_read,
                "vector_workers": counters.scan.threads_used,
                "graph_segments": counters.graph.segments_traversed,
                "graph_validations": counters.graph.graph_validations,
                "graph_seed_discoveries": counters.graph.entry_seed_discoveries,
                "graph_visited_clears": counters.graph.visited_epoch_clears,
                "graph_candidates_scored": counters.graph.candidates_scored,
                "graph_candidates_rescored": counters.graph.candidates_rescored,
                "graph_segments_pruned": counters.graph.segments_pruned_by_bound,
                "lexical_docs_evaluated": counters.lexical.docs_evaluated,
                "lexical_postings_decoded": counters.lexical.postings_decoded,
                "lexical_blocks_decoded": counters.lexical.blocks_decoded,
                "lexical_blocks_skipped": counters.lexical.blocks_skipped,
                "lexical_cache_hits": counters.lexical_cache_hits,
                "lexical_cache_builds": counters.lexical_cache_builds,
            },
        })
    });
    json!({
        "query_id": query_id,
        "round": round,
        "latency_ms": latency_ms,
        "chunks_returned": chunks.len(),
        "unique_parents_returned": entries.iter().map(|entry| &entry.doc_id).collect::<BTreeSet<_>>().len(),
        "chunks": chunks,
        "backend": backend,
        "query_tokens": outcome.query_tokens,
        "embedding_calls": outcome.embedding_calls,
        "text_spans_ms": text_spans,
        "diagnostics": diagnostics,
    })
}

fn cold(cli: &Cli, process_started: Instant) -> Result<(), Box<dyn std::error::Error>> {
    let bundle_path = Path::new(cli.required("--bundle")?);
    let store_path = Path::new(cli.required("--store")?);
    let query = cli.required("--query")?;
    let legs = parse_legs(cli.required("--legs")?)?;
    let k = cli.usize("--k", 10)?;
    let text_open_started = Instant::now();
    let store = TextStore::open(store_path, bundle_path, Default::default())?;
    let text_store_open_ms = text_open_started.elapsed().as_secs_f64() * 1_000.0;
    let open_ms = process_started.elapsed().as_secs_f64() * 1_000.0;
    let verification_started = Instant::now();
    let health = store.health()?;
    verify_health(&health, SegmentTier::SealedGraph, MIN_GRAPH_ROWS)?;
    let verification = verification_started.elapsed();
    let query_started = Instant::now();
    let hits = store.query_text(query, QueryOptions::new(k).with_legs(legs))?;
    let first_query_ms = query_started.elapsed().as_secs_f64() * 1_000.0;
    let total_ms = process_started
        .elapsed()
        .saturating_sub(verification)
        .as_secs_f64()
        * 1_000.0;
    let backend = if legs == Legs::Lexical {
        "(none)"
    } else {
        backend_name(store.query_backend())
    };
    let output = json!({
        "kind": "cold",
        "backend": backend,
        "open_ms": open_ms,
        "text_store_open_ms": text_store_open_ms,
        "first_query_ms": first_query_ms,
        "total_ms": total_ms,
        "hits": hits.len(),
        "verified_tier": "SealedGraph",
    });
    store.close()?;
    print_json(&output)
}

// Match discover_query_coreml and RuntimeClient::start in the text crate.
fn load_query_runtime(
    bundle: &Arc<Bundle>,
    bundle_path: &Path,
) -> Result<LoadedQueryRuntime, Box<dyn std::error::Error>> {
    let model = match std::env::var("ZE_QUERY_COREML") {
        Ok(value) if value == "off" => None,
        Ok(value) => Some(PathBuf::from(value)),
        Err(_) => Some(bundle_path.with_extension("mlmodelc")),
    }
    .filter(|path| path.is_dir());
    if let Some(model) = model {
        #[cfg(target_os = "macos")]
        {
            use zeppelin_embed_text::runtime::coreml::{ComputeUnits, CoreMlRuntime};
            let runtime = CoreMlRuntime::load(
                &model,
                bundle.query_tower().embedding.dims as usize,
                ComputeUnits::CpuAndNeuralEngine,
            )?;
            if runtime.sequence() < bundle.query_tower().embedding.max_tokens as usize {
                return Err(
                    io::Error::other("CoreML sequence is below bundle query max_tokens").into(),
                );
            }
            return Ok((Box::new(runtime), "CoreML", "CoreML CPU_AND_NE requested"));
        }
        #[cfg(not(target_os = "macos"))]
        return Err(io::Error::other(format!("CoreML requires macOS: {}", model.display())).into());
    }
    let runtime = MlxRuntime::load(Arc::clone(bundle), TowerRole::Query)?;
    let backend = if runtime.identity().gpu {
        "MLX GPU"
    } else {
        "MLX CPU"
    };
    Ok((Box::new(runtime), "MLX", backend))
}

fn components(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let bundle_path = Path::new(cli.required("--bundle")?);
    let started = Instant::now();
    let bundle = Arc::new(Bundle::open(bundle_path)?);
    let bundle_open_ms = started.elapsed().as_secs_f64() * 1_000.0;
    let started = Instant::now();
    let document = MlxRuntime::load(Arc::clone(&bundle), TowerRole::Document)?;
    let document_tower_load_ms = started.elapsed().as_secs_f64() * 1_000.0;
    let started = Instant::now();
    let (query, query_runtime, backend) = load_query_runtime(&bundle, bundle_path)?;
    let query_runtime_load_ms = started.elapsed().as_secs_f64() * 1_000.0;
    drop(query);
    drop(document);
    let tokenizer = TokenizerConfig::text_default();
    let document_tower = bundle.document_tower().embedding.clone();
    let epoch = StoreEpoch {
        embedding: EmbeddingEpoch {
            document: document_tower.clone(),
            query: document_tower,
            alignment_digest: Vec::new(),
        },
        tokenizer: tokenizer.epoch(),
    };
    let directory = if cli.optional("--store").is_none() {
        Some(tempfile::tempdir()?)
    } else {
        None
    };
    let store_path = cli
        .optional("--store")
        .map(Path::new)
        .or_else(|| directory.as_ref().map(|directory| directory.path()))
        .ok_or_else(|| io::Error::other("missing component store path"))?;
    let started = Instant::now();
    let store = match cli.optional("--store") {
        // Inspection adopts the persisted document epoch and validates the
        // tokenizer without rewriting or relabelling the FiQA store.
        Some(path) => Store::open_for_inspection(path)?,
        None => Store::open(
            store_path,
            OpenOptions::default()
                .with_epoch(epoch)
                .with_tokenizer(tokenizer),
        )?,
    };
    let store_open_ms = started.elapsed().as_secs_f64() * 1_000.0;
    store.close()?;
    print_json(&json!({
        "kind": "components",
        "store": store_path,
        "backend": backend,
        "bundle_open_ms": bundle_open_ms,
        "document_tower_load_ms": document_tower_load_ms,
        "query_runtime_load_ms": query_runtime_load_ms,
        "query_runtime": query_runtime,
        "store_open_ms": store_open_ms,
    }))
}

fn recall(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let beir_root = Path::new(cli.required("--beir-root")?);
    let corpus_name = cli.required("--corpus")?;
    let bundle_path = Path::new(cli.required("--bundle")?);
    let store_path = Path::new(cli.required("--store")?);
    let legs = parse_legs(cli.required("--legs")?)?;
    if legs == Legs::Lexical {
        return Err(io::Error::other("lexical recall is exact by construction").into());
    }
    let k = cli.usize("--k", 10)?;
    let requested_tier = parse_search_tier(cli.optional("--tier").unwrap_or("auto"))?;
    let rescore = parse_scan_rescore(cli, requested_tier)?;
    let mut approximate_options = QueryOptions::new(k)
        .with_legs(legs)
        .with_optional_tier(requested_tier);
    if let Some(config) = rescore {
        approximate_options = approximate_options.with_scan_rescore(config);
    }
    let out = Path::new(cli.required("--out")?);
    let corpus = load_corpus(beir_root, corpus_name)?;
    let queries = test_queries(&corpus)?;
    let id_map = read_id_map(store_path, corpus_name)?;
    let store = TextStore::open(store_path, bundle_path, Default::default())?;
    let backend = backend_name(store.query_backend());
    let physical_tier = verify_timing_store(&store, store_path)?;
    let mut recall_total = 0.0_f64;
    let mut auto_run = Run::new();
    let mut exact_run = Run::new();
    let mut rows = Vec::with_capacity(queries.len());
    for query in &queries {
        let approximate = store.query_text(&query.text, approximate_options)?;
        let exact = store.query_text(
            &query.text,
            QueryOptions::new(k)
                .with_legs(legs)
                .with_tier(SearchTier::Exact),
        )?;
        let approximate_ids = approximate.iter().map(|hit| hit.doc_id).collect::<Vec<_>>();
        let exact_ids = exact.iter().map(|hit| hit.doc_id).collect::<Vec<_>>();
        let row_recall = recall_at_k(&approximate_ids, &exact_ids);
        recall_total += row_recall;
        auto_run.insert(query.id.clone(), run_entries(&approximate, &id_map)?);
        exact_run.insert(query.id.clone(), run_entries(&exact, &id_map)?);
        rows.push(json!({"query_id": query.id, "recall_at_10": row_recall}));
    }
    let mean_recall = if queries.is_empty() {
        0.0
    } else {
        recall_total / queries.len() as f64
    };
    let output = json!({
        "kind": "recall",
        "query_tier": requested_tier.map_or("unset", search_tier_name),
        "scan_rescore": scan_rescore_json(rescore),
        "backend": backend,
        "leg": leg_name(legs),
        "store_tier": segment_tier_short(physical_tier),
        "corpus": corpus_name,
        "queries": queries.len(),
        "k": k,
        "recall_at_10": mean_recall,
        "auto_ndcg_at_10": mean_ndcg_at_k(&auto_run, &corpus.qrels, k),
        "exact_ndcg_at_10": mean_ndcg_at_k(&exact_run, &corpus.qrels, k),
        "rows": rows,
    });
    write_and_print_json(out, &output)?;
    store.close()?;
    Ok(())
}

fn power_loop(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let beir_root = Path::new(cli.required("--beir-root")?);
    let corpus_name = cli.required("--corpus")?;
    let bundle_path = Path::new(cli.required("--bundle")?);
    let store_path = Path::new(cli.required("--store")?);
    let legs = parse_legs(cli.required("--legs")?)?;
    let seconds = cli.u64("--seconds", 20)?;
    let corpus = load_corpus(beir_root, corpus_name)?;
    let queries = test_queries(&corpus)?;
    let store = TextStore::open(store_path, bundle_path, Default::default())?;
    let health = store.health()?;
    verify_health(&health, SegmentTier::SealedGraph, MIN_GRAPH_ROWS)?;
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(seconds))
        .ok_or_else(|| io::Error::other("power-loop deadline overflow"))?;
    let mut count = 0_u64;
    while Instant::now() < deadline {
        for query in &queries {
            if Instant::now() >= deadline {
                break;
            }
            let _ = store.query_text(&query.text, QueryOptions::new(10).with_legs(legs))?;
            count = count.saturating_add(1);
        }
    }
    store.close()?;
    print_json(&json!({"kind": "power-loop", "seconds": seconds, "queries": count}))
}

fn report(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let mut results = Results::default();
    for input in cli.inputs()? {
        let bytes = std::fs::read(input)?;
        let value: Value = from_slice(&bytes)?;
        absorb_result(&mut results, &value)?;
    }
    print!("{}", render_tables(&results));
    Ok(())
}

fn absorb_result(results: &mut Results, value: &Value) -> Result<(), Box<dyn std::error::Error>> {
    match json_string(value, "kind")? {
        "ingest" => results.chunk_rows = Some(json_u64(value, "chunks")?),
        "verify" => {
            let rows = json_u64(value, "sealed_rows")?;
            match json_string(value, "expected")? {
                "SealedScan" => results.scan_rows = Some(rows),
                "SealedGraph" => results.graph_rows = Some(rows),
                _ => {}
            }
        }
        "promote" => results.promote_seconds = Some(json_f64(value, "maintenance_seconds")?),
        "steady" => absorb_steady(results, value)?,
        "recall" => absorb_recall(results, value)?,
        "cold-summary" => absorb_cold(results, value)?,
        "components" => absorb_components(results, value)?,
        _ => {}
    }
    Ok(())
}

fn absorb_steady(results: &mut Results, value: &Value) -> Result<(), Box<dyn std::error::Error>> {
    let summary = value
        .get("summary")
        .ok_or_else(|| io::Error::other("steady result lacks summary"))?;
    let cell = SteadyCell {
        leg: json_string(value, "leg")?.to_owned(),
        backend: reported_backend(value)?.to_owned(),
        store_tier: json_string(value, "store_tier")?.to_owned(),
        summary: Summary {
            p50: json_f64(summary, "p50")?,
            p95: json_f64(summary, "p95")?,
            p99: json_f64(summary, "p99")?,
            mean: json_f64(summary, "mean")?,
        },
    };
    if cell.leg == "Dense" && cell.backend == "MLX GPU" && cell.store_tier == "graph" {
        results.dense_graph_mlx_repetitions.push(cell.summary.p50);
    }
    let quality = QualityCell {
        leg: cell.leg.clone(),
        tier: if cell.leg == "Lexical" {
            "n/a".to_owned()
        } else {
            cell.store_tier.clone()
        },
        ndcg_at_10: json_f64(value, "ndcg_at_10")?,
        recall_mlx: None,
        recall_ane: None,
    };
    upsert_quality(results, quality);
    results.steady.push(cell);
    Ok(())
}

fn absorb_recall(results: &mut Results, value: &Value) -> Result<(), Box<dyn std::error::Error>> {
    let leg = json_string(value, "leg")?.to_owned();
    let tier = json_string(value, "store_tier")?.to_owned();
    let backend = reported_backend(value)?;
    let recall = json_f64(value, "recall_at_10")?;
    if let Some(cell) = results
        .quality
        .iter_mut()
        .find(|cell| cell.leg == leg && cell.tier == tier)
    {
        cell.ndcg_at_10 = json_f64(value, "auto_ndcg_at_10")?;
        if backend == "MLX GPU" {
            cell.recall_mlx = Some(recall);
        } else if backend == "CoreML CPU_AND_NE requested" {
            cell.recall_ane = Some(recall);
        }
    } else {
        results.quality.push(QualityCell {
            leg,
            tier,
            ndcg_at_10: json_f64(value, "auto_ndcg_at_10")?,
            recall_mlx: (backend == "MLX GPU").then_some(recall),
            recall_ane: (backend == "CoreML CPU_AND_NE requested").then_some(recall),
        });
    }
    Ok(())
}

fn absorb_cold(results: &mut Results, value: &Value) -> Result<(), Box<dyn std::error::Error>> {
    let backend = reported_backend(value)?;
    for (prefix, launch) in [
        ("first_ever", "first-ever"),
        ("relaunch_median", "relaunch (median of 10)"),
    ] {
        let row = value
            .get(prefix)
            .ok_or_else(|| io::Error::other(format!("cold summary lacks {prefix}")))?;
        results.cold.push(ColdCell {
            backend: backend.to_owned(),
            launch: if backend == "CoreML CPU_AND_NE requested" && prefix == "first_ever" {
                "first-ever (fresh model digest)".to_owned()
            } else {
                launch.to_owned()
            },
            open_ms: json_f64(row, "open_ms")?,
            first_query_ms: json_f64(row, "first_query_ms")?,
            total_ms: json_f64(row, "total_ms")?,
        });
    }
    Ok(())
}

fn absorb_components(
    results: &mut Results,
    value: &Value,
) -> Result<(), Box<dyn std::error::Error>> {
    let backend = reported_backend(value)?;
    let mut components = results.components.unwrap_or_default();
    components.bundle_open_ms = json_f64(value, "bundle_open_ms")?;
    components.document_tower_ms = json_f64(value, "document_tower_load_ms")?;
    components.store_open_ms = json_f64(value, "store_open_ms")?;
    if backend == "MLX GPU" {
        components.mlx_query_ms = json_f64(value, "query_runtime_load_ms")?;
    } else if backend == "CoreML CPU_AND_NE requested" {
        components.ane_query_ms = Some(json_f64(value, "query_runtime_load_ms")?);
    }
    results.components = Some(components);
    Ok(())
}

fn upsert_quality(results: &mut Results, quality: QualityCell) {
    if let Some(existing) = results
        .quality
        .iter_mut()
        .find(|cell| cell.leg == quality.leg && cell.tier == quality.tier)
    {
        existing.ndcg_at_10 = quality.ndcg_at_10;
    } else {
        results.quality.push(quality);
    }
}

fn test_queries(
    corpus: &BeirCorpus,
) -> Result<Vec<&zeppelin_embed_bench::beir::BeirQuery>, Box<dyn std::error::Error>> {
    let queries = corpus
        .queries
        .iter()
        .filter(|query| corpus.qrels.contains_key(&query.id))
        .collect::<Vec<_>>();
    if queries.len() != corpus.qrels.len() {
        return Err(io::Error::other(format!(
            "query/qrels mismatch: {} query rows for {} judged ids",
            queries.len(),
            corpus.qrels.len()
        ))
        .into());
    }
    Ok(queries)
}

fn id_map_path(store_path: &Path, corpus: &str) -> Result<PathBuf, io::Error> {
    let parent = store_path
        .parent()
        .ok_or_else(|| io::Error::other("store path has no parent"))?;
    Ok(parent.join(format!("{corpus}-ids.tsv")))
}

fn read_id_map(
    store_path: &Path,
    corpus: &str,
) -> Result<BTreeMap<u128, String>, Box<dyn std::error::Error>> {
    let path = id_map_path(store_path, corpus)?;
    let text = std::fs::read_to_string(&path)?;
    let mut mapping = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let (caller, source) = line.split_once('\t').ok_or_else(|| {
            io::Error::other(format!(
                "{}:{}: malformed id map",
                path.display(),
                index + 1
            ))
        })?;
        mapping.insert(caller.parse::<u128>()?, source.to_owned());
    }
    Ok(mapping)
}

fn run_entries(
    hits: &[zeppelin_embed_text::TextHit],
    id_map: &BTreeMap<u128, String>,
) -> Result<Vec<RunEntry>, Box<dyn std::error::Error>> {
    hits.iter()
        .map(|hit| {
            let doc_id = id_map
                .get(&hit.doc_id)
                .cloned()
                .ok_or_else(|| io::Error::other(format!("unmapped caller id {}", hit.doc_id)))?;
            Ok(RunEntry {
                doc_id,
                score: hit.score,
            })
        })
        .collect()
}

fn verify_timing_store(
    store: &TextStore,
    store_path: &Path,
) -> Result<SegmentTier, Box<dyn std::error::Error>> {
    let file_name = store_path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| io::Error::other("store path has no UTF-8 file name"))?;
    let expected = if file_name.contains("graph") {
        SegmentTier::SealedGraph
    } else if file_name.contains("scan") {
        SegmentTier::SealedScan
    } else {
        return Err(io::Error::other(
            "timed store path must declare scan or graph in its file name",
        )
        .into());
    };
    let health = store.health()?;
    verify_health(&health, expected, MIN_GRAPH_ROWS)?;
    Ok(expected)
}

fn segments_json(health: &zeppelin_embed::diag::Health) -> Vec<Value> {
    health
        .segments
        .iter()
        .map(|segment| {
            json!({
                "source": format!("{:?}", segment.source),
                "tier": segment_tier_name(segment.tier),
                "rows": segment.rows,
                "tombstones": segment.tombstones,
                "bytes": segment.bytes,
            })
        })
        .collect()
}

fn parse_segment_tier(value: &str) -> Result<SegmentTier, Box<dyn std::error::Error>> {
    match value {
        "scan" => Ok(SegmentTier::SealedScan),
        "graph" => Ok(SegmentTier::SealedGraph),
        _ => Err(io::Error::other(format!("unsupported expected tier {value:?}")).into()),
    }
}

/// Parses a tier request, where `unset` means the caller states no
/// preference. That is not the same as `auto`: the hybrid leg selects
/// `SearchTier::Exact` for itself when no preference is stated, because
/// fusion may only fuse exactly rescored vector scores.
fn parse_scan_rescore(
    cli: &Cli,
    tier: Option<SearchTier>,
) -> Result<Option<ScanRescoreOptions>, Box<dyn std::error::Error>> {
    match (cli.optional("--scan-rescore-oversample"), cli.optional("--scan-rescore-max-candidates")) {
        (None, None) => Ok(None),
        (Some(oversample), Some(limit)) if tier == Some(SearchTier::Scan) => {
            Ok(Some(ScanRescoreOptions::new(oversample.parse()?, limit.parse()?)?))
        }
        _ => Err(io::Error::other("rescoring requires --tier scan, --scan-rescore-oversample and --scan-rescore-max-candidates").into()),
    }
}

fn scan_rescore_json(config: Option<ScanRescoreOptions>) -> Option<Value> {
    config.map(|config| {
        json!({
            "oversample": config.oversample(),
            "max_candidates_per_segment": config.max_candidates_per_segment(),
            "budget_scope": "per segment and vector-producer invocation, including boundary ties",
        })
    })
}

fn parse_search_tier(value: &str) -> Result<Option<SearchTier>, Box<dyn std::error::Error>> {
    match value {
        "unset" => Ok(None),
        "auto" => Ok(Some(SearchTier::Auto)),
        "scan" => Ok(Some(SearchTier::Scan)),
        "exact" => Ok(Some(SearchTier::Exact)),
        _ => Err(io::Error::other(format!("unsupported query tier {value:?}")).into()),
    }
}

fn parse_legs(value: &str) -> Result<Legs, Box<dyn std::error::Error>> {
    match value {
        "dense" => Ok(Legs::Dense),
        "lexical" => Ok(Legs::Lexical),
        "hybrid" => Ok(Legs::Hybrid),
        _ => Err(io::Error::other(format!("unsupported query legs {value:?}")).into()),
    }
}

fn parse_seed(value: &str) -> Result<u64, Box<dyn std::error::Error>> {
    if let Some(hex) = value.strip_prefix("0x") {
        return u64::from_str_radix(hex, 16)
            .map_err(|error| io::Error::other(format!("invalid seed {value:?}: {error}")).into());
    }
    value
        .parse::<u64>()
        .map_err(|error| io::Error::other(format!("invalid seed {value:?}: {error}")).into())
}

// Older artifacts called the requested CoreML policy ANE. Normalize that
// legacy label without promoting it to observed hardware execution.
fn reported_backend(value: &Value) -> Result<&str, Box<dyn std::error::Error>> {
    let backend = json_string(value, "backend")?;
    Ok(if backend == "ANE" {
        "CoreML CPU_AND_NE requested"
    } else {
        backend
    })
}

fn backend_name(backend: QueryBackend) -> &'static str {
    if backend.sequence_length.is_some() {
        "CoreML CPU_AND_NE requested"
    } else if backend.runtime.gpu {
        "MLX GPU"
    } else {
        "MLX CPU"
    }
}

fn leg_name(legs: Legs) -> &'static str {
    match legs {
        Legs::Dense => "Dense",
        Legs::Lexical => "Lexical",
        Legs::Hybrid => "Hybrid",
    }
}

fn segment_tier_name(tier: SegmentTier) -> &'static str {
    match tier {
        SegmentTier::ActiveScan => "ActiveScan",
        SegmentTier::SealedScan => "SealedScan",
        SegmentTier::SealedGraph => "SealedGraph",
    }
}

fn segment_tier_short(tier: SegmentTier) -> &'static str {
    match tier {
        SegmentTier::ActiveScan => "active",
        SegmentTier::SealedScan => "scan",
        SegmentTier::SealedGraph => "graph",
    }
}

fn search_tier_name(tier: SearchTier) -> &'static str {
    match tier {
        SearchTier::Auto => "auto",
        SearchTier::Exact => "exact",
        SearchTier::Scan => "scan",
        SearchTier::Graph(_) => "graph",
    }
}

fn summary_json(summary: Summary) -> Value {
    json!({
        "p50": summary.p50,
        "p95": summary.p95,
        "p99": summary.p99,
        "mean": summary.mean,
    })
}

fn print_json(value: &Value) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", to_string(value)?);
    Ok(())
}

fn write_and_print_json(path: &Path, value: &Value) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::write(path, to_vec_pretty(value)?)?;
    print_json(value)
}

fn json_string<'a>(value: &'a Value, key: &str) -> Result<&'a str, Box<dyn std::error::Error>> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| io::Error::other(format!("JSON field {key:?} is not a string")).into())
}

fn json_f64(value: &Value, key: &str) -> Result<f64, Box<dyn std::error::Error>> {
    value
        .get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| io::Error::other(format!("JSON field {key:?} is not a number")).into())
}

fn json_u64(value: &Value, key: &str) -> Result<u64, Box<dyn std::error::Error>> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| io::Error::other(format!("JSON field {key:?} is not a u64")).into())
}

// Signatures checked against mlx-sys 0.2.0's mlx-c/mlx/c/memory.h.
// mlx-c is linked transitively by zeppelin-embed-text; no direct mlx-sys dependency.
unsafe extern "C" {
    fn mlx_get_active_memory(result: *mut usize) -> libc::c_int;
    fn mlx_get_peak_memory(result: *mut usize) -> libc::c_int;
    fn mlx_get_cache_memory(result: *mut usize) -> libc::c_int;
    fn mlx_set_cache_limit(old: *mut usize, limit: usize) -> libc::c_int;
    fn mlx_clear_cache() -> libc::c_int;
    fn mlx_reset_peak_memory() -> libc::c_int;
}

fn mlx_check(code: libc::c_int, operation: &str) -> io::Result<()> {
    if code != 0 {
        return Err(io::Error::other(format!(
            "{operation} failed with code {code}"
        )));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn memory_counters() -> io::Result<MemoryCounters> {
    let (mut active, mut cache, mut peak) = (0, 0, 0);
    // SAFETY: each MLX out pointer is valid for one size_t.
    // Upstream limitation: the pinned native getters read size_t fields without
    // the allocator mutex. Aligned size_t reads cannot tear on arm64, but the
    // reads remain unsynchronized and can be stale or mutually inconsistent.
    // These counters are diagnostic only; they do not control engine behavior.
    unsafe {
        mlx_check(mlx_get_active_memory(&mut active), "mlx_get_active_memory")?;
        mlx_check(mlx_get_cache_memory(&mut cache), "mlx_get_cache_memory")?;
        mlx_check(mlx_get_peak_memory(&mut peak), "mlx_get_peak_memory")?;
    }
    // SAFETY: zero is valid for this all-integer C struct, and V4 writes its
    // full layout into the supplied buffer. libc's rusage_info_t is void*;
    // proc_pid_rusage treats the argument itself as the output buffer.
    let mut usage: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    let code = unsafe {
        libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V4,
            std::ptr::from_mut(&mut usage).cast(),
        )
    };
    if code != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(MemoryCounters {
        mlx_active: u64::try_from(active).map_err(io::Error::other)?,
        mlx_cache: u64::try_from(cache).map_err(io::Error::other)?,
        mlx_peak: u64::try_from(peak).map_err(io::Error::other)?,
        phys_footprint: usage.ri_phys_footprint,
        lifetime_max_phys_footprint: usage.ri_lifetime_max_phys_footprint,
        resident: usage.ri_resident_size,
    })
}

#[cfg(not(target_os = "macos"))]
fn memory_counters() -> io::Result<MemoryCounters> {
    Err(io::Error::other(
        "ingest memory sampling requires macOS proc_pid_rusage V4",
    ))
}

impl MemoryCounters {
    fn json(self) -> Value {
        json!({
            "mlx_active": self.mlx_active, "mlx_cache": self.mlx_cache,
            "mlx_peak": self.mlx_peak, "phys_footprint": self.phys_footprint,
            "lifetime_max_phys_footprint": self.lifetime_max_phys_footprint,
            "resident": self.resident,
        })
    }
}

struct MemorySample {
    t_seconds: f64,
    counters: MemoryCounters,
}

impl MemorySample {
    fn write_sample(&self, file: Option<&mut impl io::Write>) -> io::Result<()> {
        let line = to_string(&self.json()).map_err(io::Error::other)?;
        if let Some(file) = file {
            writeln!(file, "{line}")?;
            // Flush every complete line before returning: previously emitted
            // samples must survive a later native GPU timeout/SIGABRT.
            file.flush()?;
        }
        let mut stderr = io::stderr().lock();
        writeln!(stderr, "{line}")?;
        stderr.flush()
    }

    fn json(&self) -> Value {
        let counters = self.counters;
        json!({
            "t_seconds": self.t_seconds,
            "mlx_active": counters.mlx_active, "mlx_cache": counters.mlx_cache,
            "mlx_peak": counters.mlx_peak, "phys_footprint": counters.phys_footprint,
            "lifetime_max_phys_footprint": counters.lifetime_max_phys_footprint,
            "resident": counters.resident,
        })
    }
}

// One stoppable thread services both timers. In pinned MLX, clear_cache and
// malloc/free all lock MetalAllocator::mutex_; clear releases only cached
// buffers. The malloc path drops the lock for newBuffer and re-locks before
// publishing allocator state, so concurrent cache clearing is supported.
fn sample_memory(
    stop: &mpsc::Receiver<()>,
    started: Instant,
    sample_interval: Duration,
    clear_interval: Option<Duration>,
    mut file: Option<std::fs::File>,
) -> io::Result<Vec<MemorySample>> {
    let mut samples = Vec::new();
    let next_deadline = |interval| {
        Instant::now()
            .checked_add(interval)
            .ok_or_else(|| io::Error::other("memory timer interval exceeds Instant range"))
    };
    let mut next_sample = next_deadline(sample_interval)?;
    let mut next_clear = clear_interval.map(next_deadline).transpose()?;
    loop {
        let deadline = next_clear.map_or(next_sample, |clear| clear.min(next_sample));
        match stop.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        let now = Instant::now();
        if now >= next_sample {
            let sample = MemorySample {
                t_seconds: started.elapsed().as_secs_f64(),
                counters: memory_counters()?,
            };
            sample.write_sample(file.as_mut())?;
            samples.push(sample);
            next_sample = next_deadline(sample_interval)?;
        }
        if next_clear.is_some_and(|clear| now >= clear) {
            // SAFETY: clear_cache takes the allocator mutex; it does not touch
            // active evaluation buffers. Return codes are never discarded.
            mlx_check(unsafe { mlx_clear_cache() }, "mlx_clear_cache")?;
            next_clear = clear_interval.map(next_deadline).transpose()?;
        }
    }
    Ok(samples)
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct MemoryCounters {
    mlx_active: u64,
    mlx_cache: u64,
    mlx_peak: u64,
    phys_footprint: u64,
    lifetime_max_phys_footprint: u64,
    resident: u64,
}

fn memory_maxima(samples: &[MemoryCounters]) -> MemoryCounters {
    samples
        .iter()
        .fold(MemoryCounters::default(), |max, sample| MemoryCounters {
            mlx_active: max.mlx_active.max(sample.mlx_active),
            mlx_cache: max.mlx_cache.max(sample.mlx_cache),
            mlx_peak: max.mlx_peak.max(sample.mlx_peak),
            phys_footprint: max.phys_footprint.max(sample.phys_footprint),
            lifetime_max_phys_footprint: max
                .lifetime_max_phys_footprint
                .max(sample.lifetime_max_phys_footprint),
            resident: max.resident.max(sample.resident),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_jsonl_is_flushed_after_each_sample() -> Result<(), Box<dyn std::error::Error>> {
        let file = tempfile::NamedTempFile::new()?;
        let mut writer = io::BufWriter::new(file.reopen()?);
        let mut sample = MemorySample {
            t_seconds: 0.0,
            counters: MemoryCounters {
                mlx_active: 17,
                ..Default::default()
            },
        };
        sample.write_sample(Some(&mut writer))?;
        // Read before drop: a process abort must not lose a buffered sample.
        let first = std::fs::read_to_string(file.path())?;
        assert!(
            first.ends_with('\n'),
            "first sample must already be a complete line"
        );
        let row: Value = from_slice(first.as_bytes())?;
        assert_eq!(row.get("mlx_active").and_then(Value::as_u64), Some(17));
        sample.t_seconds = 1.0;
        sample.counters.mlx_active = 23;
        sample.write_sample(Some(&mut writer))?;
        let contents = std::fs::read_to_string(file.path())?;
        assert!(contents.ends_with('\n'));
        let rows = contents
            .lines()
            .map(|line| from_slice::<Value>(line.as_bytes()))
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.last()
                .and_then(|row| row.get("mlx_active"))
                .and_then(Value::as_u64),
            Some(23)
        );
        Ok(())
    }

    #[test]
    fn memory_maxima_retains_independent_high_water_marks() {
        let samples = [
            MemoryCounters {
                mlx_active: 9,
                mlx_cache: 2,
                mlx_peak: 10,
                phys_footprint: 40,
                lifetime_max_phys_footprint: 50,
                resident: 60,
            },
            MemoryCounters {
                mlx_active: 3,
                mlx_cache: 8,
                mlx_peak: 12,
                phys_footprint: 30,
                lifetime_max_phys_footprint: 55,
                resident: 70,
            },
        ];
        assert_eq!(
            memory_maxima(&samples),
            MemoryCounters {
                mlx_active: 9,
                mlx_cache: 8,
                mlx_peak: 12,
                phys_footprint: 40,
                lifetime_max_phys_footprint: 55,
                resident: 70,
            }
        );
        assert_eq!(memory_maxima(&[]), MemoryCounters::default());
    }
}
