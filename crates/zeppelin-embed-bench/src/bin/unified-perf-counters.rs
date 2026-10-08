//! Explicit work-counter sampling; never part of ordinary measurement timing.
use serde_json::{Value, json};
use std::error::Error;
use std::fs::{File, OpenOptions as FileOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use zeppelin_embed::diag::QueryDiagnostics;
use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, Normalization, StoreEpoch,
};
use zeppelin_embed::fts::{
    index::DEFAULT_FIELD,
    query::LexicalQuery,
    search::{FieldWeights, TermQuery},
    tokenizer::{Analyzer, TokenFlags, TokenizerConfig},
};
use zeppelin_embed::fusion::HybridQuery;
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::{
    CancelToken, OpenOptions, QueryControl, QueryFilter, SearchOptions, SearchTier, Store,
};
use zeppelin_embed::meta::{
    ColumnDefinition, ColumnId, ColumnType, Predicate, PredicateValue, Schema,
};
use zeppelin_embed::scan::ScanOptions;

type Result<T> = std::result::Result<T, Box<dyn Error>>;
fn invalid(message: &str) -> Box<dyn Error> {
    io::Error::other(message).into()
}
fn field<'a>(v: &'a Value, name: &str) -> Result<&'a Value> {
    v.get(name)
        .ok_or_else(|| invalid(&format!("missing {name}")))
}
fn text<'a>(v: &'a Value, name: &str) -> Result<&'a str> {
    field(v, name)?.as_str().ok_or_else(|| invalid(name))
}
fn integer(v: &Value, name: &str) -> Result<u64> {
    field(v, name)?.as_u64().ok_or_else(|| invalid(name))
}
fn array<'a>(v: &'a Value, name: &str) -> Result<&'a Vec<Value>> {
    field(v, name)?.as_array().ok_or_else(|| invalid(name))
}
fn check(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(invalid(message))
    }
}
fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("unified-perf-counters: {error}");
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut manifest = None;
    let mut output = None;
    while let Some(flag) = args.next() {
        let value = args.next().ok_or_else(|| invalid("flag needs value"))?;
        match flag.as_str() {
            "--manifest" if manifest.is_none() => manifest = Some(PathBuf::from(value)),
            "--out" if output.is_none() => output = Some(PathBuf::from(value)),
            _ => return Err(invalid("use --manifest M --out O")),
        }
    }
    let manifest = manifest.ok_or_else(|| invalid("--manifest required"))?;
    let output = output.ok_or_else(|| invalid("--out required"))?;
    check(!output.exists(), "output exists; retain the old receipt")?;
    let workload = read_workload(&manifest)?;
    let schema = schema(&workload)?;
    let epoch = epoch(&workload)?;
    let directory = tempfile::tempdir()?;
    let store = Store::open(
        directory.path(),
        OpenOptions::default()
            .with_schema(schema.clone())
            .with_epoch(epoch.clone()),
    )?;
    let mut batch = Vec::new();
    let mut rows = 0_u64;
    let boundaries = [30000_u64, 60000, 90000, 120000, 150000];
    let corpus = PathBuf::from(text(field(&workload, "corpus")?, "path")?);
    for line in BufReader::new(File::open(corpus)?).lines() {
        let row: Value = serde_json::from_str(&line?)?;
        let columns = array(&row, "attributes")?
            .iter()
            .map(|a| {
                check(text(a, "type")? == "u64", "attribute must be u64")?;
                Ok((
                    ColumnId::new(u32::try_from(integer(a, "id")?)?),
                    PredicateValue::U64(text(a, "value")?.parse()?),
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        batch.push(
            IngestDocument::new(
                DocumentVersion::new(
                    DocId::new(text(&row, "id")?.parse()?),
                    Revision::new(text(&row, "revision")?.parse()?),
                ),
                vector(&row)?,
            )
            .with_timestamp(text(&row, "timestamp")?.parse()?)
            .with_text(text(&row, "text")?)
            .with_columns(columns),
        );
        rows += 1;
        if rows.is_multiple_of(300) {
            store.ingest(
                IngestBatch::new(std::mem::take(&mut batch)).with_epoch(epoch.identity()),
            )?;
        }
        if boundaries.contains(&rows) {
            store.seal()?;
        }
    }
    check(
        rows == 150000 && batch.is_empty(),
        "fixture must contain exactly 150000 rows",
    )?;
    // Like Node, measure a reopened read-only store rather than the writer's caches.
    store.close()?;
    let store = Store::open(
        directory.path(),
        OpenOptions::read_only()
            .with_schema(schema)
            .with_epoch(epoch),
    )?;
    let counters = measure_counters(&store, &workload)?;
    store.close()?;
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let revision = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["rev-parse", "HEAD"])
        .output()?;
    check(revision.status.success(), "git revision unavailable")?;
    let binary = std::env::current_exe()?;
    let receipt = json!({"schema":"zeppelin-unified-perf-counters-v1",
        "workloadSha256": workload_hash(&manifest)?,
        "manifestSha256": hash_file(&manifest)?, "revision": String::from_utf8(revision.stdout)?.trim(),
        "binarySha256": hash_file(&binary)?, "optLevel": env!("ZEPPELIN_BENCH_OPT_LEVEL"),
        "warmups":20,"samples":5,"threadBudget":1,"queries":counters});
    let mut file = FileOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    serde_json::to_writer(&mut file, &receipt)?;
    writeln!(file)?;
    Ok(())
}
fn workload_hash(path: &Path) -> Result<String> {
    let script = r#"const fs = require('node:fs'), crypto = require('node:crypto');
const value = JSON.parse(fs.readFileSync(process.argv[1], 'utf8'));
const canonical = JSON.stringify(value, (key, value) => {
  if (key === 'path') return undefined;
  if (value && typeof value === 'object' && !Array.isArray(value))
    return Object.fromEntries(Object.keys(value).sort().map(k => [k, value[k]]));
  return value;
});
process.stdout.write(crypto.createHash('sha256').update(canonical).digest('hex'));"#;
    let output = Command::new("node")
        .arg("-e")
        .arg(script)
        .arg(path)
        .output()?;
    check(output.status.success(), "logical workload hashing failed")?;
    Ok(String::from_utf8(output.stdout)?)
}
// macOS owner's tooling, no new dependency. shasum is also available on Linux.
fn hash_file(path: &Path) -> Result<String> {
    let result = Command::new("shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()?;
    check(result.status.success(), "shasum failed")?;
    String::from_utf8(result.stdout)?
        .split_whitespace()
        .next()
        .map(str::to_owned)
        .ok_or_else(|| invalid("missing SHA-256"))
}
fn read_workload(path: &Path) -> Result<Value> {
    let v: Value = serde_json::from_reader(File::open(path)?)?;
    check(
        text(&v, "schema")? == "zeppelin-unified-perf-v1",
        "wrong workload schema",
    )?;
    check(
        integer(&v, "seed")? == 386
            && integer(&v, "dimensions")? == 8
            && integer(&v, "rowCount")? == 150000,
        "wrong fixture",
    )?;
    check(
        field(&v, "segmentBoundaries")? == &json!([30000, 60000, 90000, 120000, 150000]),
        "wrong boundaries",
    )?;
    for name in ["corpus", "queryFile"] {
        let a = field(&v, name)?;
        check(
            hash_file(Path::new(text(a, "path")?))? == text(a, "sha256")?,
            "changed artifact",
        )?;
    }
    let queries: Value =
        serde_json::from_reader(File::open(text(field(&v, "queryFile")?, "path")?)?)?;
    check(
        &queries == field(&v, "queries")?,
        "query file differs from manifest",
    )?;
    check(
        array(&v, "queries")?.len() == 104,
        "expected 104 common cells",
    )?;
    schema(&v)?;
    epoch(&v)?;
    Ok(v)
}
fn schema(v: &Value) -> Result<Schema> {
    let expected = ["note", "folder", "speaker", "stream", "startMs", "endMs"];
    let attributes = array(v, "attributes")?;
    check(attributes.len() == expected.len(), "wrong schema")?;
    let columns = attributes
        .iter()
        .zip(expected)
        .enumerate()
        .map(|(i, (a, name))| {
            check(
                integer(a, "id")? == u64::try_from(i + 1)?
                    && text(a, "name")? == name
                    && text(a, "type")? == "u64"
                    && field(a, "nullable")? == &Value::Bool(false),
                "wrong attribute",
            )?;
            Ok(ColumnDefinition::new(
                ColumnId::new(u32::try_from(i + 1)?),
                name,
                ColumnType::U64,
                false,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Schema::new(columns)?)
}
fn epoch(v: &Value) -> Result<StoreEpoch> {
    let e = field(v, "epoch")?;
    check(
        text(e, "modelId")? == "zeppelin.vector-space"
            && text(e, "modelVersion")? == "1"
            && text(e, "normalization")? == "none"
            && text(e, "runtime")? == "cpuReference"
            && text(e, "computeUnits")? == "cpu"
            && integer(e, "maxTokens")? == 0
            && array(e, "weightsDigest")?.is_empty()
            && array(e, "alignmentDigest")?.is_empty(),
        "wrong namespace epoch",
    )?;
    let tower = EmbeddingTower {
        model_id: text(e, "modelId")?.into(),
        model_version: text(e, "modelVersion")?.into(),
        weights_digest: vec![],
        dims: u32::try_from(integer(v, "dimensions")?)?,
        normalization: Normalization::None,
        prompt_prefix: String::new(),
        max_tokens: 0,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    Ok(StoreEpoch {
        embedding: EmbeddingEpoch {
            document: tower.clone(),
            query: tower,
            alignment_digest: vec![],
        },
        tokenizer: TokenizerConfig::text_default().epoch(),
    })
}
fn vector(v: &Value) -> Result<Vec<f32>> {
    let values = array(v, "vector")?;
    check(values.len() == 8, "wrong vector dimensions")?;
    values
        .iter()
        .map(|v| {
            let n = v
                .as_f64()
                .ok_or_else(|| invalid("vector coordinate is not numeric"))?;
            let f = n as f32;
            check(
                n.is_finite() && f.is_finite(),
                "nonfinite float32 coordinate",
            )?;
            Ok(f)
        })
        .collect()
}
fn lexical(request: &Value) -> Result<LexicalQuery> {
    let input = text(request, "text")?;
    let tokens = Analyzer::new(TokenizerConfig::text_default())?.analyze(input);
    let trailing = if request.get("lastAsPrefix") == Some(&Value::Bool(true)) {
        tokens
            .iter()
            .enumerate()
            .filter(|(_, t)| usize::try_from(t.offset.end) == Ok(input.len()))
            .max_by_key(|(_, t)| {
                (
                    t.position,
                    t.offset.end,
                    !t.flags.contains(TokenFlags::VARIANT),
                )
            })
            .filter(|(_, t)| t.term.len() >= 3)
            .map(|(i, _)| i)
    } else {
        None
    };
    if let Some(index) = trailing {
        let prefix = tokens
            .get(index)
            .ok_or_else(|| invalid("missing prefix"))?
            .term
            .as_bytes()
            .to_vec();
        let terms = tokens
            .into_iter()
            .enumerate()
            .filter(|(i, _)| *i != index)
            .map(|(_, t)| t.term.into_bytes())
            .collect();
        Ok(LexicalQuery::TermsWithPrefix {
            terms,
            prefix,
            fields: FieldWeights::flat(&[DEFAULT_FIELD]),
        })
    } else {
        Ok(LexicalQuery::Term(TermQuery::flat(
            tokens.into_iter().map(|t| t.term.into_bytes()).collect(),
            &[DEFAULT_FIELD],
        )))
    }
}
fn filter(request: &Value, schema: &Schema) -> Result<Option<QueryFilter>> {
    if let Some(f) = request.get("filter") {
        check(
            text(f, "op")? == "eq" && integer(f, "attributeId")? == 2,
            "unsupported filter",
        )?;
        let values = array(f, "values")?;
        check(values.len() == 1, "folder filter must have one value")?;
        let value = values
            .first()
            .ok_or_else(|| invalid("missing filter value"))?;
        check(
            integer(value, "id")? == 2
                && text(value, "type")? == "u64"
                && text(value, "value")? == "0",
            "wrong folder filter",
        )?;
        Ok(QueryFilter::new(
            schema,
            Some(&Predicate::Eq {
                column: ColumnId::new(2),
                value: PredicateValue::U64(0),
            }),
            None,
        )?)
    } else {
        Ok(None)
    }
}
fn query(store: &Store, request: &Value, schema: &Schema) -> Result<(QueryDiagnostics, Value)> {
    let fields = request
        .as_object()
        .ok_or_else(|| invalid("request must be an object"))?;
    for name in fields.keys() {
        check(
            [
                "text",
                "lastAsPrefix",
                "k",
                "threadBudget",
                "vector",
                "tier",
                "alpha",
                "filter",
            ]
            .contains(&name.as_str()),
            "unsupported request field; do not drop predicates",
        )?;
    }
    if request.get("vector").is_none() {
        check(
            request.get("tier").is_none() && request.get("alpha").is_none(),
            "vector options need vector",
        )?;
    }
    if let Some(prefix) = request.get("lastAsPrefix") {
        check(prefix.is_boolean(), "lastAsPrefix must be boolean")?;
    }
    check(
        integer(request, "k")? == 10 && integer(request, "threadBudget")? == 1,
        "wrong query settings",
    )?;
    let lexical = lexical(request)?;
    let filter = filter(request, schema)?;
    let (diagnostics, hits) = if request.get("vector").is_some() {
        check(
            text(request, "tier")? == "exact" && field(request, "alpha")?.as_f64() == Some(0.5),
            "wrong hybrid settings",
        )?;
        let vector = vector(request)?;
        let hybrid = HybridQuery::new(10).with_alpha(0.5).with_epoch(
            store
                .epoch_identity()
                .ok_or_else(|| invalid("missing epoch"))?,
        );
        let result = store.search_hybrid_structured(
            SearchRequest::new(&vector).with_filter(filter.as_ref()),
            &lexical,
            &hybrid,
            SearchOptions::new(ScanOptions { thread_budget: 1 }).with_tier(SearchTier::Exact),
            control(),
        )?;
        let hits = result.hits.iter().map(|h| json!({"id":h.key.get().to_string(), "scoreBits":format!("{:016x}",h.fused_score.to_bits())})).collect::<Vec<_>>();
        (result.diagnostics, json!(hits))
    } else {
        let result = store.search_lexical_structured_filtered(
            &lexical,
            10,
            64,
            control(),
            filter.as_ref(),
        )?;
        let hits = result.candidates.iter().map(|h| json!({"id":h.document.doc_id().get().to_string(), "scoreBits":format!("{:016x}",h.score.to_bits())})).collect::<Vec<_>>();
        (result.diagnostics, json!(hits))
    };
    check(
        !diagnostics.approximate
            && (request.get("vector").is_none() || diagnostics.exact_rescore)
            && !diagnostics.budget_exhausted,
        "partial/approximate query",
    )?;
    Ok((diagnostics, hits))
}
fn measure_counters(store: &Store, workload: &Value) -> Result<Value> {
    let schema = schema(workload)?;
    store.warm_lexical(control())?;
    let mut results = Vec::new();
    for q in array(workload, "queries")? {
        let request = field(q, "request")?;
        for _ in 0..20 {
            query(store, request, &schema)?;
        }
        let mut samples = Vec::new();
        let mut descriptions = Vec::new();
        let mut expected_hits = None;
        for _ in 0..5 {
            let (d, hits) = query(store, request, &schema)?;
            let work = work_counters(&d);
            if let Some(first) = samples.first() {
                check(first == &work, "nondeterministic counters")?;
            }
            if let Some(first) = &expected_hits {
                check(first == &hits, "nondeterministic hits")?;
            }
            samples.push(work);
            expected_hits = Some(hits);
            descriptions.push(json!({"blocks_skipped":d.counters.lexical.blocks_skipped,
                "lexical_cache_hits":d.counters.lexical_cache_hits,"lexical_cache_builds":d.counters.lexical_cache_builds,
                "plan":format!("{:?}", d.plan)}));
        }
        results.push(json!({"name":text(q,"name")?,"samples":samples,"descriptive":descriptions,"hits":expected_hits}));
    }
    Ok(json!(results))
}
fn work_counters(d: &QueryDiagnostics) -> Value {
    json!({"lexical":{"docs_evaluated":d.counters.lexical.docs_evaluated,
        "postings_decoded":d.counters.lexical.postings_decoded,"blocks_decoded":d.counters.lexical.blocks_decoded},
        "scan":{"dims_touched":d.counters.scan.dims_touched,"bytes_read":d.counters.scan.bytes_read},
        "graph":{"candidates_scored":d.counters.graph.candidates_scored,"candidates_rescored":d.counters.graph.candidates_rescored},
        "hybrid":d.hybrid.as_ref().map(|h| json!({"lexical_full_materializations":h.lexical_full_materializations,
            "vector_candidates_produced":h.vector_candidates_produced,"lexical_candidates_produced":h.lexical_candidates_produced,
            "total_cross_filled_vector":h.total_cross_filled_vector,"total_cross_filled_lexical":h.total_cross_filled_lexical})),
        "fusion":d.fusion.as_ref().map(|f| json!({"rounds":f.rounds}))})
}
#[cfg(test)]
mod tests {
    use super::*;
    use zeppelin_embed::epoch::{
        ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, Normalization, StoreEpoch,
    };
    use zeppelin_embed::fts::{
        index::DEFAULT_FIELD, query::LexicalQuery, search::TermQuery, tokenizer::TokenizerConfig,
    };
    use zeppelin_embed::fusion::HybridQuery;
    use zeppelin_embed::ingest::{
        DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
    };
    use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, SearchOptions, Store};

    #[test]
    fn counter_output_keeps_work_and_excludes_time_and_thread_identity() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let tower = EmbeddingTower {
            model_id: "literal".into(),
            model_version: "1".into(),
            weights_digest: vec![],
            dims: 2,
            normalization: Normalization::None,
            prompt_prefix: String::new(),
            max_tokens: 0,
            runtime: EmbeddingRuntime::CpuReference,
            compute_units: ComputeUnits::Cpu,
            os_build: None,
        };
        let epoch = StoreEpoch {
            embedding: EmbeddingEpoch {
                document: tower.clone(),
                query: tower,
                alignment_digest: vec![],
            },
            tokenizer: TokenizerConfig::text_default().epoch(),
        };
        let store = Store::open(
            directory.path(),
            OpenOptions::default().with_epoch(epoch.clone()),
        )?;
        store.ingest(
            IngestBatch::new(vec![
                IngestDocument::new(
                    DocumentVersion::new(DocId::new(1), Revision::new(1)),
                    vec![1., 0.],
                )
                .with_text("harbour"),
            ])
            .with_epoch(epoch.identity()),
        )?;
        let result = store.search_hybrid_structured(
            SearchRequest::new(&[1., 0.]),
            &LexicalQuery::Term(TermQuery::flat(vec![b"harbour".to_vec()], &[DEFAULT_FIELD])),
            &HybridQuery::new(10).with_alpha(0.5),
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )?;
        query(
            &store,
            &json!({"text":"harbour","k":10,"threadBudget":1}),
            &Schema::timestamp_only(),
        )?;
        let error = query(
            &store,
            &json!({"text":"harbour","k":10,"threadBudget":1,"eligibleIds":[]}),
            &Schema::timestamp_only(),
        )
        .err()
        .ok_or("unsupported request succeeded")?;
        assert_eq!(
            error.to_string(),
            "unsupported request field; do not drop predicates"
        );
        let mut d = result.diagnostics;
        d.counters.lexical.docs_evaluated = 11;
        d.counters.lexical.postings_decoded = 12;
        d.counters.lexical.blocks_decoded = 13;
        d.counters.lexical.blocks_skipped = 99;
        d.counters.scan.dims_touched = 21;
        d.counters.scan.bytes_read = 22;
        d.counters.graph.candidates_scored = 31;
        d.counters.graph.candidates_rescored = 32;
        let h = d.hybrid.as_mut().ok_or("missing hybrid diagnostics")?;
        h.lexical_full_materializations = 41;
        h.vector_candidates_produced = 42;
        h.lexical_candidates_produced = 43;
        h.total_cross_filled_vector = 44;
        h.total_cross_filled_lexical = 45;
        d.fusion
            .as_mut()
            .ok_or("missing fusion diagnostics")?
            .rounds = 51;
        let expected = json!({"lexical": {"docs_evaluated":11,"postings_decoded":12,"blocks_decoded":13},
            "scan":{"dims_touched":21,"bytes_read":22},
            "graph":{"candidates_scored":31,"candidates_rescored":32},
            "hybrid":{"lexical_full_materializations":41,"vector_candidates_produced":42,
                "lexical_candidates_produced":43,"total_cross_filled_vector":44,"total_cross_filled_lexical":45},
            "fusion":{"rounds":51}});
        assert_eq!(work_counters(&d), expected);
        d.elapsed = std::time::Duration::from_secs(999);
        d.snapshot_generation = 999;
        d.counters.scan.threads_used = 999;
        d.counters.scan.worker_thread_ids = vec![std::thread::current().id()];
        assert_eq!(work_counters(&d), expected);
        store.close()?;
        Ok(())
    }
}
