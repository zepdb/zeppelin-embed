//! Measures the hybrid-fusion policy constants on BEIR SciFact.
//!
//! `crates/zeppelin-embed/src/fusion` ships three placeholder constants:
//! `DEFAULT_ALPHA` (0.7), `LEXICAL_RULE_ALPHA` (0.4), and
//! `RARE_DOCUMENT_FREQUENCY_THRESHOLD` (5). This binary indexes the full
//! SciFact corpus through the public `Store` API (dense Cohere vectors plus
//! title-and-abstract text), seals it, runs the 300 judged queries, and
//! reports nDCG@10 for the lexical leg alone, the vector leg alone, and the
//! fused result at every alpha on a 0.1 grid. It then computes the rule
//! signals the engine expects the caller to supply (open item R12) and
//! sweeps the rare-token threshold and rule alpha over the same per-query
//! results, so each cell of the policy table is a measured number rather
//! than a guess.
//!
//! Alpha weights the vector leg: fused = alpha * vector + (1 - alpha) *
//! lexical, so alpha 1.0 is vector-only and alpha 0.0 is lexical-only.
//!
//! ```text
//! ZE_BEIR_DIR=/private/tmp/beir \
//! ZE_SCIFACT_VECTORS=/path/to/ragbench/data/scifact \
//!   cargo run --release -p zeppelin-embed-bench --bin hybrid-alpha -- --k 10
//! ```
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::cast_precision_loss,
    clippy::too_many_lines
)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use zeppelin_embed::fts::index::DEFAULT_FIELD;
use zeppelin_embed::fts::search::TermQuery;
use zeppelin_embed::fts::tokenizer::{Analyzer, TokenizerConfig};
use zeppelin_embed::fusion::HybridQuery;
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, SearchOptions, Store};
use zeppelin_embed_bench::beir::eval::{RunEntry, ndcg_at_k_for_query};
use zeppelin_embed_bench::beir::loader::load_corpus;
use zeppelin_embed_bench::platform::taint::{detect_taint, print_taint_status};

const DEFAULT_CORPUS: &str = "scifact";
const DEFAULT_BEIR_DIR: &str = "/private/tmp/beir";
const DEFAULT_VECTORS_DIR: &str =
    "/Users/aghatage/Documents/code/zeppelin-holdout/ragbench/data/scifact";
/// Documents per ingest batch. The store seals after every batch: the active
/// segment clones its whole lexical index on every appended document
/// (`ActiveSegment::appended_lexical`), so an unsealed 5,183-row active
/// segment costs O(n^2) and takes over ten minutes. Sealing every 512 rows
/// keeps that quadratic term small, and BM25 is proven independent of where
/// segments were sealed (`prop_engine_bm25_equals_model`).
const INGEST_BATCH: usize = 512;

/// Alpha grid for the fused arm. The policy sweep only needs 0.3..=0.9 but
/// the endpoints and 0.1/0.2 cost nothing and pin the curve's shape.
const ALPHA_GRID: [f64; 11] = [0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0];
/// Rare-token document-frequency thresholds swept by the rule policy.
const THRESHOLDS: [u64; 7] = [1, 2, 3, 5, 8, 13, 21];
/// Alphas the rule may shift a fired query to.
const RULE_ALPHAS: [f64; 4] = [0.3, 0.4, 0.5, 0.6];
/// The shipped placeholder default, always reported alongside the best.
const PLACEHOLDER_DEFAULT_ALPHA: f64 = 0.7;

struct Args {
    beir_dir: PathBuf,
    vectors_dir: PathBuf,
    corpus: String,
    k: usize,
    time_searches: bool,
    time_passes: usize,
    load_limit: f64,
    out: Option<PathBuf>,
}

fn parse_args() -> Args {
    let mut k = 10_usize;
    let mut corpus = String::from(DEFAULT_CORPUS);
    let mut vectors_dir = std::env::var_os("ZE_SCIFACT_VECTORS")
        .map_or_else(|| PathBuf::from(DEFAULT_VECTORS_DIR), PathBuf::from);
    let mut time_searches = false;
    let mut time_passes = 3_usize;
    let mut load_limit = 3.0_f64;
    let mut out = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--k" => {
                k = args
                    .next()
                    .expect("--k needs a value")
                    .parse()
                    .expect("--k must be a positive integer");
            }
            "--corpus" => corpus = args.next().expect("--corpus needs a value"),
            "--vectors" => {
                vectors_dir = PathBuf::from(args.next().expect("--vectors needs a value"));
            }
            "--time-searches" => time_searches = true,
            "--time-passes" => {
                time_passes = args
                    .next()
                    .expect("--time-passes needs a value")
                    .parse()
                    .expect("--time-passes must be a positive integer");
            }
            "--load-limit" => {
                load_limit = args
                    .next()
                    .expect("--load-limit needs a value")
                    .parse()
                    .expect("--load-limit must be a number");
            }
            "--out" => {
                out = Some(PathBuf::from(args.next().expect("--out needs a value")));
            }
            other => panic!(
                "unknown argument {other:?}; accepted flags are --k, --corpus, --vectors, \
                 --time-searches, --time-passes, --load-limit, and --out"
            ),
        }
    }
    assert!(k > 0, "--k must be positive");
    assert!(time_passes > 0, "--time-passes must be positive");
    assert!(
        load_limit.is_finite() && load_limit >= 0.0,
        "--load-limit must be finite and non-negative"
    );
    Args {
        beir_dir: std::env::var_os("ZE_BEIR_DIR")
            .map_or_else(|| PathBuf::from(DEFAULT_BEIR_DIR), PathBuf::from),
        vectors_dir,
        corpus,
        k,
        time_searches,
        time_passes,
        load_limit,
        out,
    }
}

struct LineTee(Option<File>);

impl LineTee {
    fn new(path: Option<&Path>) -> Self {
        Self(path.map(|path| File::create(path).expect("create --out file")))
    }

    fn emit(&mut self, line: String) {
        println!("{line}");
        if [
            "DATA ",
            "BUILD ",
            "HYBRID_ALPHA_RESULT ",
            "BEST_ALPHA ",
            "HYBRID_LATENCY ",
            "HYBRID_STAGE ",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix))
            && let Some(output) = &mut self.0
        {
            writeln!(output, "{line}").expect("write --out file");
        }
    }
}

/// Reads `meta.json` for `dims`, and the row-major little-endian f32 matrix
/// plus its one-id-per-line index file.
fn read_matrix(dir: &Path, vectors: &str, ids: &str, dims: usize) -> (Vec<String>, Vec<Vec<f32>>) {
    let ids: Vec<String> = std::fs::read_to_string(dir.join(ids))
        .expect("id file")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    let bytes = std::fs::read(dir.join(vectors)).expect("vector file");
    assert_eq!(
        bytes.len(),
        ids.len() * dims * 4,
        "{vectors}: byte length does not match {} ids x {dims} dims",
        ids.len()
    );
    let rows = bytes
        .chunks_exact(dims * 4)
        .map(|row| {
            row.chunks_exact(4)
                .map(|value| f32::from_le_bytes([value[0], value[1], value[2], value[3]]))
                .collect::<Vec<f32>>()
        })
        .collect();
    (ids, rows)
}

fn dims_from_meta(dir: &Path) -> usize {
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("meta.json")).expect("meta.json"))
            .expect("meta.json parses");
    assert_eq!(
        meta.get("metric").and_then(serde_json::Value::as_str),
        Some("cosine"),
        "vectors must be cosine so L2 normalization makes squared-L2 rank like cosine"
    );
    usize::try_from(
        meta.get("dims")
            .and_then(serde_json::Value::as_u64)
            .expect("meta.json dims"),
    )
    .expect("dims fits usize")
}

/// L2-normalizes every row in place and returns the (min, max) norm seen
/// before normalization, so the evidence records whether the source was
/// already unit length.
fn normalize(rows: &mut [Vec<f32>]) -> (f64, f64) {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for row in rows.iter_mut() {
        let norm = row
            .iter()
            .map(|value| f64::from(*value) * f64::from(*value))
            .sum::<f64>()
            .sqrt();
        min = min.min(norm);
        max = max.max(norm);
        assert!(norm > 0.0, "zero vector cannot be normalized");
        for value in row.iter_mut() {
            *value = (f64::from(*value) / norm) as f32;
        }
    }
    (min, max)
}

/// Mirrors `analyze_query_text` in the FFI crate: every analyzed token, in
/// order, with no deduplication.
fn analyze_terms(analyzer: &Analyzer, text: &str) -> Vec<String> {
    analyzer
        .analyze(text)
        .into_iter()
        .map(|token| token.term)
        .collect()
}

/// Identifier rule: a term is identifier-class when it contains `_` or a
/// digit immediately adjacent to a letter (`il6`, `p53`, `covid19`). Pure
/// numbers (`2019`) and pure words never fire. The tokenizer emits the
/// letter/digit catenation as its own term, so `IL-6` fires through `il6`.
fn is_identifier(term: &str) -> bool {
    if term.contains('_') {
        return true;
    }
    let chars: Vec<char> = term.chars().collect();
    chars.windows(2).any(|pair| {
        (pair[0].is_ascii_digit() && pair[1].is_alphabetic())
            || (pair[0].is_alphabetic() && pair[1].is_ascii_digit())
    })
}

fn quoted_phrase(text: &str) -> bool {
    text.matches('"').count() >= 2
}

#[derive(Clone, Debug)]
struct QuerySignals {
    quoted_phrase: bool,
    identifier_token: bool,
    rarest_df: Option<u64>,
}

struct Judged {
    id: String,
    vector: Vec<f32>,
    terms: Vec<String>,
    signals: QuerySignals,
}

fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}

fn mean(values: &[f64]) -> f64 {
    values.iter().sum::<f64>() / values.len() as f64
}

fn percentile(sorted: &[f64], fraction: f64) -> Result<f64, Box<dyn std::error::Error>> {
    if sorted.is_empty() {
        return Err(io::Error::other("latency sample is empty").into());
    }
    let rank = (fraction * (sorted.len().saturating_sub(1)) as f64).round() as usize;
    sorted
        .get(rank.min(sorted.len().saturating_sub(1)))
        .copied()
        .ok_or_else(|| io::Error::other("latency percentile is unavailable").into())
}

#[derive(Default)]
struct LatencySamples {
    hybrid_us: Vec<f64>,
    lexical_us: Vec<f64>,
    vector_us: Vec<f64>,
}

fn run_search_pass(
    store: &Store,
    judged: &[Judged],
    k: usize,
    alpha: f64,
    mut samples: Option<&mut LatencySamples>,
) {
    let hybrid = HybridQuery::new(k).with_alpha(alpha).without_rules();
    for query in judged {
        let term_query = TermQuery::flat(
            query
                .terms
                .iter()
                .map(|term| term.as_bytes().to_vec())
                .collect(),
            &[DEFAULT_FIELD],
        );

        let started = Instant::now();
        store
            .search_hybrid(
                SearchRequest::new(&query.vector),
                &term_query,
                &hybrid,
                SearchOptions::default(),
                control(),
            )
            .expect("timed hybrid search");
        if let Some(observed) = &mut samples {
            observed
                .hybrid_us
                .push(started.elapsed().as_secs_f64() * 1_000_000.0);
        }

        let started = Instant::now();
        store
            .search_lexical(&term_query, k, control())
            .expect("timed lexical search");
        if let Some(observed) = &mut samples {
            observed
                .lexical_us
                .push(started.elapsed().as_secs_f64() * 1_000_000.0);
        }

        let started = Instant::now();
        store
            .search(
                SearchRequest::new(&query.vector),
                k,
                SearchOptions::default(),
                control(),
            )
            .expect("timed vector search");
        if let Some(observed) = &mut samples {
            observed
                .vector_us
                .push(started.elapsed().as_secs_f64() * 1_000_000.0);
        }
    }
}

fn report_search_latencies(
    store: &Store,
    judged: &[Judged],
    args: &Args,
    best_alpha: f64,
    tee: &mut LineTee,
) {
    run_search_pass(store, judged, args.k, best_alpha, None);
    let taint = detect_taint(args.load_limit);
    print_taint_status(&taint, args.load_limit, "hybrid latency");
    println!(
        "TAINT hybrid_latency labels={} direction=slower_only",
        if taint.taints.is_empty() {
            String::from("none")
        } else {
            taint
                .taints
                .iter()
                .map(|item| item.label())
                .collect::<Vec<_>>()
                .join(",")
        }
    );

    let mut samples = LatencySamples::default();
    for _ in 0..args.time_passes {
        run_search_pass(store, judged, args.k, best_alpha, Some(&mut samples));
    }
    samples.hybrid_us.sort_by(f64::total_cmp);
    samples.lexical_us.sort_by(f64::total_cmp);
    samples.vector_us.sort_by(f64::total_cmp);
    let hybrid_p50 = percentile(&samples.hybrid_us, 0.50).expect("hybrid p50");
    let hybrid_p90 = percentile(&samples.hybrid_us, 0.90).expect("hybrid p90");
    let hybrid_p95 = percentile(&samples.hybrid_us, 0.95).expect("hybrid p95");
    let hybrid_p99 = percentile(&samples.hybrid_us, 0.99).expect("hybrid p99");
    let lexical_p50 = percentile(&samples.lexical_us, 0.50).expect("lexical p50");
    let lexical_p95 = percentile(&samples.lexical_us, 0.95).expect("lexical p95");
    let vector_p50 = percentile(&samples.vector_us, 0.50).expect("vector p50");
    let vector_p95 = percentile(&samples.vector_us, 0.95).expect("vector p95");
    tee.emit(format!(
        "HYBRID_LATENCY corpus={} shape=batched512 arm=current thread_budget=1 legs=serial \
         queries={} passes={} samples={} alpha={best_alpha:.1} k={} p50_us={hybrid_p50:.6} \
         p90_us={hybrid_p90:.6} p95_us={hybrid_p95:.6} p99_us={hybrid_p99:.6} \
         processes=1 spread_pct=null",
        args.corpus,
        judged.len(),
        args.time_passes,
        samples.hybrid_us.len(),
        args.k,
    ));
    tee.emit(format!(
        "HYBRID_STAGE corpus={} arm=current vector_p50_us={vector_p50:.6} \
         vector_p95_us={vector_p95:.6} lexical_p50_us={lexical_p50:.6} \
         lexical_p95_us={lexical_p95:.6} join_wait_p50_us=null fusion_p50_us=null",
        args.corpus,
    ));
}

fn main() {
    let args = parse_args();
    let mut tee = LineTee::new(args.out.as_deref());
    let k = args.k;
    let started = Instant::now();

    // ---- data ---------------------------------------------------------
    let corpus = load_corpus(&args.beir_dir, &args.corpus).expect("BEIR corpus loads");
    let dims = dims_from_meta(&args.vectors_dir);
    let (corpus_ids, mut corpus_vectors) = read_matrix(
        &args.vectors_dir,
        "corpus_vectors.f32",
        "corpus_ids.txt",
        dims,
    );
    let (query_ids, mut query_vectors) = read_matrix(
        &args.vectors_dir,
        "query_vectors.f32",
        "query_ids.txt",
        dims,
    );
    let (corpus_min_norm, corpus_max_norm) = normalize(&mut corpus_vectors);
    let (query_min_norm, query_max_norm) = normalize(&mut query_vectors);
    tee.emit(format!(
        "DATA docs={} queries_total={} judged_queries={} dims={dims} \
         corpus_norm_range=[{corpus_min_norm:.6},{corpus_max_norm:.6}] \
         query_norm_range=[{query_min_norm:.6},{query_max_norm:.6}]",
        corpus.documents.len(),
        corpus.queries.len(),
        corpus.qrels.len()
    ));
    assert_eq!(
        corpus_ids.len(),
        corpus.documents.len(),
        "vector rows and corpus documents disagree"
    );

    // Vector row index is the store DocId; keep the string id for qrels.
    let text_of: HashMap<&str, String> = corpus
        .documents
        .iter()
        .map(|document| {
            (
                document.id.as_str(),
                if document.title.is_empty() {
                    document.text.clone()
                } else {
                    format!("{} {}", document.title, document.text)
                },
            )
        })
        .collect();
    let analyzer =
        Analyzer::new(TokenizerConfig::text_default()).expect("text-default analyzer config");

    // ---- document frequency for the rare-token signal -----------------
    let df_started = Instant::now();
    let mut df: HashMap<String, u64> = HashMap::new();
    for (index, id) in corpus_ids.iter().enumerate() {
        let text = text_of.get(id.as_str()).expect("every vector row has text");
        let unique: HashSet<String> = analyze_terms(&analyzer, text).into_iter().collect();
        for term in unique {
            *df.entry(term).or_insert(0) += 1;
        }
        if index % 1000 == 999 {
            eprintln!(
                "df progress {}/{} elapsed_ms={}",
                index + 1,
                corpus_ids.len(),
                df_started.elapsed().as_millis()
            );
        }
    }
    println!(
        "DF vocabulary={} elapsed_ms={}",
        df.len(),
        df_started.elapsed().as_millis()
    );

    // ---- store build ---------------------------------------------------
    let directory = tempfile::tempdir().expect("scratch store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    let ingest_started = Instant::now();
    let mut seal_ms = 0_u128;
    let mut generation = 0_u64;
    for (batch_index, chunk) in corpus_ids.chunks(INGEST_BATCH).enumerate() {
        let documents = chunk
            .iter()
            .enumerate()
            .map(|(offset, id)| {
                let row = batch_index * INGEST_BATCH + offset;
                IngestDocument::new(
                    DocumentVersion::new(DocId::new(row as u128), Revision::new(1)),
                    corpus_vectors[row].clone(),
                )
                .with_text(text_of.get(id.as_str()).expect("text").as_str())
            })
            .collect();
        store
            .ingest(IngestBatch::new(documents))
            .expect("ingest batch");
        let seal_started = Instant::now();
        generation = store.seal().expect("seal");
        seal_ms += seal_started.elapsed().as_millis();
        eprintln!(
            "ingest progress batch {batch_index} elapsed_ms={}",
            ingest_started.elapsed().as_millis()
        );
    }
    let ingest_ms = ingest_started.elapsed().as_millis() - seal_ms;
    tee.emit(format!(
        "BUILD ingest_ms={ingest_ms} seal_ms={seal_ms} segments={} generation={generation}",
        corpus_ids.len().div_ceil(INGEST_BATCH)
    ));

    // ---- judged queries -------------------------------------------------
    let query_row: HashMap<&str, usize> = query_ids
        .iter()
        .enumerate()
        .map(|(row, id)| (id.as_str(), row))
        .collect();
    let mut judged: Vec<Judged> = Vec::new();
    for query in &corpus.queries {
        if !corpus.qrels.contains_key(&query.id) {
            continue;
        }
        let row = *query_row
            .get(query.id.as_str())
            .expect("every judged query has a dense vector");
        let terms = analyze_terms(&analyzer, &query.text);
        assert!(!terms.is_empty(), "query {} analyzed to nothing", query.id);
        let signals = QuerySignals {
            quoted_phrase: quoted_phrase(&query.text),
            identifier_token: terms.iter().any(|term| is_identifier(term)),
            rarest_df: terms
                .iter()
                .map(|term| df.get(term).copied().unwrap_or(0))
                .min(),
        };
        judged.push(Judged {
            id: query.id.clone(),
            vector: query_vectors[row].clone(),
            terms,
            signals,
        });
    }
    let queries = judged.len();
    let quoted = judged.iter().filter(|q| q.signals.quoted_phrase).count();
    let identifiers = judged.iter().filter(|q| q.signals.identifier_token).count();
    let mut df_histogram: BTreeMap<u64, usize> = BTreeMap::new();
    for query in &judged {
        *df_histogram
            .entry(query.signals.rarest_df.unwrap_or(0))
            .or_insert(0) += 1;
    }
    println!(
        "SIGNALS judged={queries} quoted_phrase={quoted} identifier_token={identifiers} \
         rarest_df_histogram={}",
        df_histogram
            .iter()
            .map(|(df, count)| format!("{df}:{count}"))
            .collect::<Vec<_>>()
            .join(",")
    );
    let identifier_examples: Vec<String> = judged
        .iter()
        .filter(|q| q.signals.identifier_token)
        .take(8)
        .map(|q| {
            format!(
                "{}={:?}",
                q.id,
                q.terms
                    .iter()
                    .filter(|term| is_identifier(term))
                    .cloned()
                    .collect::<Vec<_>>()
            )
        })
        .collect();
    println!("IDENTIFIER_EXAMPLES {}", identifier_examples.join(" "));

    let doc_label = |doc: DocId| -> String {
        corpus_ids[usize::try_from(doc.get()).expect("row fits usize")].clone()
    };
    let score_run = |query_id: &str, ranked: Vec<RunEntry>| -> f64 {
        let judgements = corpus.qrels.get(query_id).expect("judged");
        ndcg_at_k_for_query(&ranked, judgements, k).expect("positive judgement")
    };

    // ---- single legs ----------------------------------------------------
    let leg_started = Instant::now();
    let mut lexical_scores = Vec::with_capacity(queries);
    let mut vector_scores = Vec::with_capacity(queries);
    let mut approximate_seen = 0_usize;
    for query in &judged {
        let term_query = TermQuery::flat(
            query.terms.iter().map(|t| t.as_bytes().to_vec()).collect(),
            &[DEFAULT_FIELD],
        );
        let lexical = store
            .search_lexical(&term_query, k, control())
            .expect("lexical search");
        lexical_scores.push(score_run(
            &query.id,
            lexical
                .candidates
                .iter()
                .map(|hit| RunEntry {
                    doc_id: doc_label(hit.document.doc_id()),
                    score: hit.score,
                })
                .collect(),
        ));
        let vector = store
            .search(
                SearchRequest::new(&query.vector),
                k,
                SearchOptions::default(),
                control(),
            )
            .expect("vector search");
        if vector.diagnostics.approximate {
            approximate_seen += 1;
        }
        vector_scores.push(score_run(
            &query.id,
            vector
                .candidates
                .iter()
                .map(|hit| RunEntry {
                    doc_id: doc_label(hit.document().expect("store rows carry identity").doc_id()),
                    score: f64::from(hit.score()),
                })
                .collect(),
        ));
    }
    println!(
        "LEGS elapsed_ms={} vector_approximate_queries={approximate_seen}",
        leg_started.elapsed().as_millis()
    );
    tee.emit(format!(
        "HYBRID_ALPHA_RESULT arm=lexical alpha=0.0 ndcg10={:.4} queries={queries}",
        mean(&lexical_scores)
    ));
    tee.emit(format!(
        "HYBRID_ALPHA_RESULT arm=vector alpha=1.0 ndcg10={:.4} queries={queries}",
        mean(&vector_scores)
    ));

    // ---- fused arm over the alpha grid, per query -----------------------
    let fused_started = Instant::now();
    // per_query[alpha_index][query_index]
    let mut per_query: Vec<Vec<f64>> = Vec::with_capacity(ALPHA_GRID.len());
    for alpha in ALPHA_GRID {
        let hybrid = HybridQuery::new(k).with_alpha(alpha).without_rules();
        let mut scores = Vec::with_capacity(queries);
        for query in &judged {
            let term_query = TermQuery::flat(
                query.terms.iter().map(|t| t.as_bytes().to_vec()).collect(),
                &[DEFAULT_FIELD],
            );
            let outcome = store
                .search_hybrid(
                    SearchRequest::new(&query.vector),
                    &term_query,
                    &hybrid,
                    SearchOptions::default(),
                    control(),
                )
                .expect("hybrid search");
            scores.push(score_run(
                &query.id,
                outcome
                    .hits
                    .iter()
                    .map(|hit| RunEntry {
                        doc_id: doc_label(hit.key),
                        score: hit.fused_score,
                    })
                    .collect(),
            ));
        }
        tee.emit(format!(
            "HYBRID_ALPHA_RESULT arm=hybrid alpha={alpha:.1} ndcg10={:.4} queries={queries}",
            mean(&scores)
        ));
        per_query.push(scores);
    }
    println!(
        "FUSED elapsed_ms={} searches={}",
        fused_started.elapsed().as_millis(),
        ALPHA_GRID.len() * queries
    );
    let alpha_index = |alpha: f64| -> usize {
        ALPHA_GRID
            .iter()
            .position(|candidate| (candidate - alpha).abs() < 1e-9)
            .expect("alpha on the grid")
    };
    let best_alpha = ALPHA_GRID
        .iter()
        .copied()
        .max_by(|left, right| {
            mean(&per_query[alpha_index(*left)]).total_cmp(&mean(&per_query[alpha_index(*right)]))
        })
        .expect("non-empty grid");
    tee.emit(format!(
        "BEST_ALPHA alpha={best_alpha:.1} ndcg10={:.4}",
        mean(&per_query[alpha_index(best_alpha)])
    ));

    if args.time_searches {
        report_search_latencies(&store, &judged, &args, best_alpha, &mut tee);
    }

    // ---- rule sweep -----------------------------------------------------
    let mut defaults = vec![best_alpha];
    if (best_alpha - PLACEHOLDER_DEFAULT_ALPHA).abs() > 1e-9 {
        defaults.push(PLACEHOLDER_DEFAULT_ALPHA);
    }
    for default_alpha in defaults {
        let default_scores = &per_query[alpha_index(default_alpha)];
        println!(
            "RULE_BASELINE default_alpha={default_alpha:.1} ndcg10={:.4} fired=0",
            mean(default_scores)
        );
        // Identifier/quoted only: isolates the threshold's contribution.
        for rule_alpha in RULE_ALPHAS {
            let rule_scores = &per_query[alpha_index(rule_alpha)];
            let mut fired = 0_usize;
            let mut total = 0.0;
            for (index, query) in judged.iter().enumerate() {
                let fire = query.signals.identifier_token || query.signals.quoted_phrase;
                if fire {
                    fired += 1;
                    total += rule_scores[index];
                } else {
                    total += default_scores[index];
                }
            }
            println!(
                "IDENTIFIER_RULE_RESULT default_alpha={default_alpha:.1} rule_alpha={rule_alpha:.1} \
                 fired={fired} ndcg10={:.4}",
                total / queries as f64
            );
        }
        for threshold in THRESHOLDS {
            for rule_alpha in RULE_ALPHAS {
                let rule_scores = &per_query[alpha_index(rule_alpha)];
                let mut fired = 0_usize;
                let mut total = 0.0;
                for (index, query) in judged.iter().enumerate() {
                    let rare = query.signals.rarest_df.is_some_and(|df| df <= threshold);
                    let fire =
                        rare || query.signals.identifier_token || query.signals.quoted_phrase;
                    if fire {
                        fired += 1;
                        total += rule_scores[index];
                    } else {
                        total += default_scores[index];
                    }
                }
                println!(
                    "RULE_SWEEP_RESULT default_alpha={default_alpha:.1} threshold={threshold} \
                     rule_alpha={rule_alpha:.1} fired={fired} ndcg10={:.4}",
                    total / queries as f64
                );
            }
        }
    }

    store.close().expect("close store");
    println!("DONE total_ms={}", started.elapsed().as_millis());
}
