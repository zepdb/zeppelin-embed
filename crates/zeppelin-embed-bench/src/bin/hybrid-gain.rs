//! Fixed-anchor Store hybrid calibration over frozen, precomputed BEIR vectors.
use std::collections::{BTreeSet, HashMap};
use std::error::Error;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
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
use zeppelin_embed_bench::beir::eval::{Qrels, RunEntry, ndcg_at_k_for_query};
use zeppelin_embed_bench::beir::loader::parse_qrels;

const DIMS: usize = 768;
const K: usize = 10;
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

struct Args {
    prepared: PathBuf,
    source_ids: PathBuf,
    qrels: PathBuf,
    store: PathBuf,
    alphas: Vec<f64>,
    gains: Vec<f64>,
    passes: usize,
    out: PathBuf,
}

fn args() -> Result<Args, Box<dyn Error>> {
    let mut values = HashMap::new();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        if ![
            "--prepared",
            "--source-ids",
            "--qrels",
            "--store",
            "--alphas",
            "--gains",
            "--passes",
            "--out",
        ]
        .contains(&flag.as_str())
        {
            return Err(format!("unknown flag {flag}").into());
        }
        let value = args.next().ok_or("flag requires a value")?;
        if values.insert(flag, value).is_some() {
            return Err("duplicate flag".into());
        }
    }
    let get = |key: &str| values.get(key).ok_or_else(|| format!("missing {key}"));
    let numbers = |key: &str| -> Result<Vec<f64>, Box<dyn Error>> {
        get(key)?
            .split(',')
            .map(|s| Ok(s.parse::<f64>()?))
            .collect()
    };
    let result = Args {
        prepared: get("--prepared")?.into(),
        source_ids: get("--source-ids")?.into(),
        qrels: get("--qrels")?.into(),
        store: get("--store")?.into(),
        alphas: numbers("--alphas")?,
        gains: numbers("--gains")?,
        passes: get("--passes")?.parse()?,
        out: get("--out")?.into(),
    };
    if result.passes == 0
        || result
            .alphas
            .iter()
            .any(|a| !a.is_finite() || !(0.0..=1.0).contains(a))
        || result.gains.iter().any(|g| !g.is_finite() || *g <= 0.0)
    {
        return Err("passes must be positive, alphas in [0,1], gains finite and positive".into());
    }
    Ok(result)
}

fn json_lines(path: &Path) -> Result<Vec<serde_json::Value>, Box<dyn Error>> {
    BufReader::new(File::open(path)?)
        .lines()
        .map(|line| Ok(serde_json::from_str(&line?)?))
        .collect()
}

fn matrix(path: &Path, rows: usize) -> Result<Vec<Vec<f32>>, Box<dyn Error>> {
    let bytes = fs::read(path)?;
    if bytes.len() != rows * DIMS * 4 {
        return Err(format!("wrong matrix shape: {}", path.display()).into());
    }
    Ok(bytes
        .chunks_exact(DIMS * 4)
        .map(|row| {
            row.chunks_exact(4)
                .map(|v| {
                    let mut word = [0; 4];
                    word.copy_from_slice(v);
                    f32::from_le_bytes(word)
                })
                .collect()
        })
        .collect())
}

fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}

fn digest_bytes(digest: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *digest = (*digest ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
    }
}

fn metric(
    query: &str,
    ids: &[DocId],
    sources: &[String],
    qrels: &Qrels,
) -> Result<f64, Box<dyn Error>> {
    let mut seen = BTreeSet::new();
    let mut ranked = Vec::new();
    for id in ids {
        let parent = usize::try_from(id.get() >> 32)?;
        let source = sources
            .get(parent.checked_sub(1).ok_or("zero parent")?)
            .ok_or("parent outside source ids")?;
        if source != query && seen.insert(source) {
            ranked.push(RunEntry {
                doc_id: source.clone(),
                score: 0.0,
            });
        }
    }
    ndcg_at_k_for_query(
        &ranked,
        qrels.get(query).ok_or("missing query judgements")?,
        K,
    )
    .ok_or_else(|| "query has no positive judgement".into())
}

#[allow(clippy::cast_precision_loss)]
fn percentile(samples: &[f64], fraction: f64) -> Result<f64, Box<dyn Error>> {
    let index = (fraction * samples.len().saturating_sub(1) as f64).round() as usize;
    samples
        .get(index)
        .copied()
        .ok_or_else(|| "no latency samples".into())
}

#[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
fn main() -> Result<(), Box<dyn Error>> {
    let args = args()?;
    let mut output = File::create(&args.out)?;
    let mut emit = |line: String| -> Result<(), Box<dyn Error>> {
        println!("{line}");
        writeln!(output, "{line}")?;
        output.flush()?;
        Ok(())
    };
    let load = std::process::Command::new("uptime").output()?;
    emit(format!("LOAD {}", String::from_utf8(load.stdout)?.trim()))?;
    let sources: Vec<String> = serde_json::from_reader(File::open(&args.source_ids)?)?;
    let qrels = parse_qrels(&fs::read_to_string(&args.qrels)?, &args.qrels)?;
    let empty = !args.store.exists() || fs::read_dir(&args.store)?.next().is_none();
    let store = Store::open(&args.store, OpenOptions::default())?;
    if empty {
        // Import the entire frozen row order in one batch.
        let started = Instant::now();
        let chunks = json_lines(&args.prepared.join("chunks.jsonl"))?;
        let vectors = matrix(&args.prepared.join("vectors.f32"), chunks.len())?;
        let documents = chunks
            .iter()
            .zip(vectors)
            .enumerate()
            .map(|(row, (chunk, vector))| {
                if chunk.get("row").and_then(serde_json::Value::as_u64) != Some(row as u64) {
                    return Err("chunk row order mismatch".into());
                }
                let parent: u128 = chunk
                    .get("parent")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("missing parent")?
                    .parse()?;
                let chunk_id = chunk
                    .get("chunk")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or("missing chunk")?;
                if parent == 0 || parent > sources.len() as u128 || chunk_id > u64::from(u32::MAX) {
                    return Err("invalid parent/chunk identity".into());
                }
                let text = chunk
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("missing text")?;
                Ok(IngestDocument::new(
                    DocumentVersion::new(
                        DocId::new((parent << 32) | u128::from(chunk_id)),
                        Revision::new(1),
                    ),
                    vector,
                )
                .with_text(text))
            })
            .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
        let rows = documents.len();
        store.ingest(IngestBatch::new(documents))?;
        let ingest_ms = started.elapsed().as_millis();
        let started = Instant::now();
        let generation = store.seal()?;
        emit(format!(
            "BUILD rows={rows} dims={DIMS} ingest_ms={ingest_ms} seal_ms={} generation={generation}",
            started.elapsed().as_millis()
        ))?;
    }
    let queries = json_lines(&args.prepared.join("queries.jsonl"))?;
    let vectors = matrix(&args.prepared.join("queries.f32"), queries.len())?;
    let analyzer = Analyzer::new(TokenizerConfig::text_default())?;
    let judged = queries
        .iter()
        .zip(vectors)
        .map(|(query, vector)| {
            let id = query
                .get("id")
                .and_then(serde_json::Value::as_str)
                .ok_or("missing query id")?;
            let text = query
                .get("text")
                .and_then(serde_json::Value::as_str)
                .ok_or("missing query text")?;
            let terms = TermQuery::flat(
                analyzer
                    .analyze(text)
                    .into_iter()
                    .map(|t| t.term.into_bytes())
                    .collect(),
                &[DEFAULT_FIELD],
            );
            Ok((id, vector, terms))
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    if judged.is_empty() {
        return Err("empty query set".into());
    }
    emit(format!("DATA queries={} dims={DIMS}", judged.len()))?;
    for leg in ["dense", "lexical"] {
        let mut total = 0.0;
        for (id, vector, terms) in &judged {
            let ids = if leg == "dense" {
                store
                    .search(
                        SearchRequest::new(vector),
                        K,
                        SearchOptions::default(),
                        control(),
                    )?
                    .candidates
                    .iter()
                    .map(|h| h.document().map(|d| d.doc_id()).ok_or("missing identity"))
                    .collect::<Result<Vec<_>, _>>()?
            } else {
                store
                    .search_lexical(terms, K, control())?
                    .candidates
                    .iter()
                    .map(|h| h.document.doc_id())
                    .collect()
            };
            total += metric(id, &ids, &sources, &qrels)?;
        }
        emit(format!(
            "LEG arm={leg} ndcg10={:.8}",
            total / judged.len() as f64
        ))?;
    }
    for gain in &args.gains {
        for alpha in &args.alphas {
            let passed_alpha = gain * alpha / (gain * alpha + (1.0 - alpha));
            let hybrid = HybridQuery::new(K).with_alpha(passed_alpha).without_rules();
            let mut latencies = Vec::new();
            let mut first = None;
            let mut effective_min = f64::INFINITY;
            let mut effective_max = f64::NEG_INFINITY;
            for _ in 0..args.passes {
                let mut total = 0.0;
                let mut rounds = 0_u64;
                let mut dims = 0_u64;
                let mut digest = FNV_OFFSET;
                for (id, vector, terms) in &judged {
                    let started = Instant::now();
                    let outcome = store.search_hybrid(
                        SearchRequest::new(vector),
                        terms,
                        &hybrid,
                        SearchOptions::default(),
                        control(),
                    )?;
                    latencies.push(started.elapsed().as_secs_f64() * 1e6);
                    let report = outcome.diagnostics.fusion.ok_or("missing fusion report")?;
                    effective_min = effective_min.min(report.effective_alpha);
                    effective_max = effective_max.max(report.effective_alpha);
                    rounds += report.rounds as u64;
                    dims += outcome.diagnostics.counters.scan.dims_touched;
                    digest_bytes(&mut digest, id.as_bytes());
                    digest_bytes(&mut digest, &[0xff]);
                    let ids: Vec<_> = outcome.hits.iter().map(|h| h.key).collect();
                    for key in &ids {
                        digest_bytes(&mut digest, &key.get().to_le_bytes());
                    }
                    total += metric(id, &ids, &sources, &qrels)?;
                }
                let observed = (total.to_bits(), rounds, dims, digest);
                if first.is_some_and(|previous| previous != observed) {
                    return Err("nondeterministic pass".into());
                }
                first = Some(observed);
            }
            latencies.sort_by(f64::total_cmp);
            let (total, rounds, dims, digest) = first.ok_or("no passes")?;
            emit(format!(
                "HYBRID alpha={alpha} gain={gain} passed_alpha={passed_alpha:.17} effective_alpha_min={effective_min:.17} effective_alpha_max={effective_max:.17} ndcg10={:.8} rounds={rounds} dims_touched={dims} digest={digest:016x} passes={} p50_us={:.3} p95_us={:.3}",
                f64::from_bits(total) / judged.len() as f64,
                args.passes,
                percentile(&latencies, 0.5)?,
                percentile(&latencies, 0.95)?
            ))?;
        }
    }
    store.close()?;
    Ok(())
}
