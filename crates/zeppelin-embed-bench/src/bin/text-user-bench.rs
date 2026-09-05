use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use zeppelin_embed::epoch::{EmbeddingEpoch, StoreEpoch};
use zeppelin_embed::fts::tokenizer::TokenizerConfig;
use zeppelin_embed::lifecycle::{OpenOptions, SearchTier, Store};
use zeppelin_embed::tier::SegmentTier;
use zeppelin_embed_bench::beir::eval::{Run, RunEntry, mean_ndcg_at_k};
use zeppelin_embed_bench::beir::loader::{BeirCorpus, load_corpus};
use zeppelin_embed_bench::harness_json::{Value, from_slice, json, to_string, to_vec_pretty};
use zeppelin_embed_bench::user_bench::{
    ColdCell, QualityCell, Results, SteadyCell, Summary, percentiles, recall_at_k, render_tables,
    shuffled_order,
};
use zeppelin_embed_text::bundle::Bundle;
use zeppelin_embed_text::runtime::ModelRuntime;
use zeppelin_embed_text::runtime::mlx::MlxRuntime;
use zeppelin_embed_text::tower::TowerRole;
use zeppelin_embed_text::{
    IngestOptions, Legs, MaintenanceBudget, MaintenanceStatus, QueryBackend, QueryOptions,
    TextDocument, TextQueryOutcome, TextStore,
};

const MIN_GRAPH_ROWS: u64 = 30_000;

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
        "steady" => steady(&cli),
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
    let maintenance_ms = cli.u64("--maintenance-ms", 0)?;
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
    let report = store.ingest_text(
        &documents,
        IngestOptions {
            seal_every,
            maintenance_wall_time: Duration::from_millis(maintenance_ms),
            ..Default::default()
        },
    )?;
    let health = store.health()?;
    let output = json!({
        "kind": "ingest",
        "bundle": bundle_path,
        "store": store_path,
        "corpus": corpus_name,
        "documents": report.documents,
        "chunks": report.chunks,
        "tokens": report.tokens,
        "embed_batches": report.embed_batches,
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

fn steady(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let beir_root = Path::new(cli.required("--beir-root")?);
    let corpus_name = cli.required("--corpus")?;
    let bundle_path = Path::new(cli.required("--bundle")?);
    let store_path = Path::new(cli.required("--store")?);
    let legs = parse_legs(cli.required("--legs")?)?;
    let requested_tier = parse_search_tier(cli.required("--tier")?)?;
    let rounds = cli.usize("--rounds", 3)?;
    let warm = cli.usize("--warm", 20)?;
    let k = cli.usize("--k", 10)?;
    let seed = parse_seed(cli.required("--seed")?)?;
    let out = Path::new(cli.required("--out")?);
    let corpus = load_corpus(beir_root, corpus_name)?;
    let queries = test_queries(&corpus)?;
    let id_map = read_id_map(store_path, corpus_name)?;
    let store = TextStore::open(store_path, bundle_path, Default::default())?;
    let backend = if legs == Legs::Lexical {
        "(none)"
    } else {
        backend_name(store.query_backend())
    };
    let physical_tier = verify_timing_store(&store, store_path)?;
    let order = shuffled_order(queries.len(), seed);
    for index in order.iter().cycle().take(warm) {
        let query = queries
            .get(*index)
            .ok_or_else(|| io::Error::other("warmup query index is out of bounds"))?;
        let _ = store.query_text(
            &query.text,
            QueryOptions::new(k)
                .with_legs(legs)
                .with_optional_tier(requested_tier),
        )?;
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
            let started = Instant::now();
            let outcome = store.query_text_with_diagnostics(
                &query.text,
                QueryOptions::new(k)
                    .with_legs(legs)
                    .with_optional_tier(requested_tier),
            )?;
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
    let output = json!({
        "kind": "steady",
        "backend": backend,
        "leg": leg_name(legs),
        "store_tier": segment_tier_short(physical_tier),
        "query_tier": requested_tier.map_or("unset", search_tier_name),
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
        "unique_parent_ndcg_at_k": ndcg,
        "metric_policy": "first parent occurrence, compact unique parent ranks; no retrieval fill",
        "chunk_policy": "raw chunk ranks retained in query_samples; qrels judge parents, not chunks",
        "instrumentation_enabled": zeppelin_embed::diag::QUERY_TIMING_ENABLED,
        "epoch": { "embedding": store.epoch().embedding.value(), "tokenizer": store.epoch().tokenizer.value() },
        "segments": segments_json(&store.health()?),
        "latencies_ms": samples,
        "rankings": rankings,
        "query_samples": query_samples,
    });
    write_and_print_json(out, &output)?;
    store.close()?;
    Ok(())
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
            "coverage_certificate": "not emitted by this schema version",
            "requested_k": diag.requested_k,
            "returned": diag.returned,
            "budget_exhausted": diag.budget_exhausted,
            "elapsed_ms": milliseconds(diag.elapsed),
            "core_spans_ms": core_spans,
            "span_policy": "inclusive wall spans; vector and lexical overlap; round spans accumulate",
            "plans": plans,
            "fusion": fusion,
            "hybrid": hybrid,
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
    let store = TextStore::open(store_path, bundle_path, Default::default())?;
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
        "first_query_ms": first_query_ms,
        "total_ms": total_ms,
        "hits": hits.len(),
        "verified_tier": "SealedGraph",
    });
    store.close()?;
    print_json(&output)
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
    let query = MlxRuntime::load(Arc::clone(&bundle), TowerRole::Query)?;
    let query_runtime_load_ms = started.elapsed().as_secs_f64() * 1_000.0;
    let backend = if query.identity().gpu {
        "MLX GPU"
    } else {
        "MLX CPU"
    };
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
    let directory = tempfile::tempdir()?;
    let started = Instant::now();
    let store = Store::open(
        directory.path(),
        OpenOptions::default()
            .with_epoch(epoch)
            .with_tokenizer(tokenizer),
    )?;
    let store_open_ms = started.elapsed().as_secs_f64() * 1_000.0;
    store.close()?;
    print_json(&json!({
        "kind": "components",
        "backend": backend,
        "bundle_open_ms": bundle_open_ms,
        "document_tower_load_ms": document_tower_load_ms,
        "query_runtime_load_ms": query_runtime_load_ms,
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
        let approximate = store.query_text(
            &query.text,
            QueryOptions::new(k)
                .with_legs(legs)
                .with_tier(SearchTier::Auto),
        )?;
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

fn verify_health(
    health: &zeppelin_embed::diag::Health,
    expected: SegmentTier,
    minimum_rows: u64,
) -> Result<u64, Box<dyn std::error::Error>> {
    let mut rows = 0_u64;
    let mut sealed = 0_usize;
    for segment in &health.segments {
        match segment.tier {
            SegmentTier::ActiveScan => {
                return Err(io::Error::other(format!(
                    "timing refused: active segment has {} rows",
                    segment.rows
                ))
                .into());
            }
            SegmentTier::SealedScan | SegmentTier::SealedGraph => {
                if segment.tier != expected {
                    return Err(io::Error::other(format!(
                        "timing refused: expected {expected:?}, found {:?}",
                        segment.tier
                    ))
                    .into());
                }
                rows = rows
                    .checked_add(segment.rows)
                    .ok_or_else(|| io::Error::other("sealed row count overflow"))?;
                sealed = sealed.saturating_add(1);
            }
        }
    }
    if sealed == 0 || rows < minimum_rows {
        return Err(io::Error::other(format!(
            "timing refused: {sealed} sealed segments contain {rows} rows, need at least {minimum_rows}"
        ))
        .into());
    }
    Ok(rows)
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
