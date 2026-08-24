//! tantivy's BM25 on the same BEIR corpora, for an honest comparison.
//!
//! Run as:
//!
//! ```text
//! cargo run --release --manifest-path tools/lexical-bakeoff/Cargo.toml -- \
//!     <beir-dir> <corpus-name>
//! ```
//!
//! Prints `corpus <name> ndcg10 <value> index_ms <n> query_ms <n>` so the
//! caller can table it beside the engine's own numbers.
//!
//! # Fairness notes
//!
//! - The same corpus files, the same queries, the same qrels, the same
//!   nDCG@10 definition (recomputed here rather than shared, so a bug in
//!   one evaluator cannot flatter both).
//! - tantivy indexes `title` and `body` as separate fields and the query
//!   runs over both, matching the engine's flat two-field configuration.
//! - tantivy's default English pipeline (`en_stem`) is lowercase + Porter
//!   stemming with no stopword list — tantivy ships none (tantivy#2595).
//!   That difference is REPORTED, not corrected: it is a real property of
//!   the competitor.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Instant;

use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Schema, Value, STORED, STRING, TEXT};
use tantivy::{doc, Index, TantivyDocument};

type Qrels = BTreeMap<String, BTreeMap<String, u32>>;

fn dcg(grades: &[u32], k: usize) -> f64 {
    grades
        .iter()
        .take(k)
        .enumerate()
        .map(|(index, grade)| f64::from(*grade) / ((index as f64 + 2.0).log2()))
        .sum()
}

fn ndcg_at_10(run: &BTreeMap<String, Vec<String>>, qrels: &Qrels) -> f64 {
    let mut total = 0.0;
    let mut counted = 0usize;
    let empty: Vec<String> = Vec::new();
    for (query, judged) in qrels {
        let mut ideal: Vec<u32> = judged.values().copied().filter(|g| *g > 0).collect();
        if ideal.is_empty() {
            continue;
        }
        ideal.sort_unstable_by(|a, b| b.cmp(a));
        let ideal_dcg = dcg(&ideal, 10);
        if ideal_dcg <= 0.0 {
            continue;
        }
        let ranked = run.get(query).unwrap_or(&empty);
        let grades: Vec<u32> = ranked
            .iter()
            .take(10)
            .map(|id| judged.get(id).copied().unwrap_or(0))
            .collect();
        total += dcg(&grades, 10) / ideal_dcg;
        counted += 1;
    }
    if counted == 0 {
        0.0
    } else {
        total / counted as f64
    }
}

fn read_jsonl(path: &Path) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("valid json line"))
        .collect()
}

fn read_qrels(path: &Path) -> Qrels {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let mut qrels: Qrels = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let mut columns = line.split('\t');
        let (Some(q), Some(d), Some(g)) = (columns.next(), columns.next(), columns.next()) else {
            continue;
        };
        // Signed, then clamped: TREC-COVID carries `-1` judgements meaning
        // "assessed, not relevant", which contribute zero gain under TREC
        // convention. Same handling as the engine's own loader, so neither
        // side gets an advantage from the quirk.
        let Ok(signed) = g.trim().parse::<i64>() else {
            if index == 0 {
                continue;
            }
            panic!("bad qrels grade on line {}", index + 1);
        };
        let grade = u32::try_from(signed.max(0)).unwrap_or(0);
        qrels
            .entry(q.to_owned())
            .or_default()
            .insert(d.to_owned(), grade);
    }
    qrels
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (Some(root), Some(name)) = (args.get(1), args.get(2)) else {
        eprintln!("usage: lexical-bakeoff <beir-dir> <corpus-name>");
        std::process::exit(2);
    };
    let base = Path::new(root).join(name);

    let mut schema_builder = Schema::builder();
    let doc_id = schema_builder.add_text_field("doc_id", STRING | STORED);
    let title = schema_builder.add_text_field("title", TEXT);
    let body = schema_builder.add_text_field("body", TEXT);
    let schema = schema_builder.build();

    let index = Index::create_in_ram(schema.clone());
    let mut writer = index.writer(256_000_000).expect("writer");

    let started = Instant::now();
    let corpus = read_jsonl(&base.join("corpus.jsonl"));
    for value in &corpus {
        let id = value["_id"].as_str().unwrap_or_default();
        let t = value["title"].as_str().unwrap_or_default();
        let b = value["text"].as_str().unwrap_or_default();
        writer
            .add_document(doc!(doc_id => id, title => t, body => b))
            .expect("add");
    }
    writer.commit().expect("commit");
    let index_ms = started.elapsed().as_millis();

    let reader = index.reader().expect("reader");
    let searcher = reader.searcher();
    let parser = QueryParser::for_index(&index, vec![title, body]);

    let queries = read_jsonl(&base.join("queries.jsonl"));
    let qrels = read_qrels(&base.join("qrels").join("test.tsv"));
    let judged: BTreeSet<String> = qrels.keys().cloned().collect();

    let mut run: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let query_started = Instant::now();
    let mut executed = 0usize;
    for value in &queries {
        let id = value["_id"].as_str().unwrap_or_default().to_owned();
        if !judged.contains(&id) {
            continue;
        }
        let text = value["text"].as_str().unwrap_or_default();
        // Escape query-syntax characters: BEIR queries are natural language
        // and must not be parsed as a boolean DSL.
        let cleaned: String = text
            .chars()
            .map(|c| if c.is_alphanumeric() || c.is_whitespace() { c } else { ' ' })
            .collect();
        if cleaned.trim().is_empty() {
            continue;
        }
        let Ok(parsed) = parser.parse_query(&cleaned) else {
            continue;
        };
        let top = searcher
            .search(&parsed, &TopDocs::with_limit(10))
            .expect("search");
        executed += 1;
        let mut ids = Vec::with_capacity(top.len());
        for (_score, address) in top {
            let retrieved: TantivyDocument = searcher.doc(address).expect("doc");
            if let Some(found) = retrieved.get_first(doc_id).and_then(|v| v.as_str()) {
                ids.push(found.to_owned());
            }
        }
        run.insert(id, ids);
    }
    let query_ms = query_started.elapsed().as_millis();

    println!(
        "corpus {name} ndcg10 {:.4} docs {} queries {} index_ms {index_ms} query_ms {query_ms}",
        ndcg_at_10(&run, &qrels),
        corpus.len(),
        executed
    );
}
