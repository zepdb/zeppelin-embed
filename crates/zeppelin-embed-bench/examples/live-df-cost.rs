//! Public Store live-DF screen; build with normal core features, without observers.
use serde_json::{Value, json};
use std::{error::Error, path::PathBuf, time::Instant};
use zeppelin_embed::{
    fts::{index::DEFAULT_FIELD, search::TermQuery},
    ingest::{DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision},
    lifecycle::{CancelToken, OpenOptions, QueryControl, Store},
};

fn term(prefix: &str, mut row: usize) -> String {
    let mut result = prefix.to_owned();
    for _ in 0..5 {
        result.push((b'a' + (row % 26) as u8) as char);
        row /= 26;
    }
    result
}

fn query(store: &Store, terms: Vec<String>) -> Result<(f64, Value), Box<dyn Error>> {
    let q = TermQuery::flat(
        terms.into_iter().map(String::into_bytes).collect(),
        &[DEFAULT_FIELD],
    );
    let start = Instant::now();
    let result = store.search_lexical(&q, 10, QueryControl::Cancel(CancelToken::new()))?;
    let micros = start.elapsed().as_secs_f64() * 1e6;
    let hits = result
        .candidates
        .iter()
        .map(|h| {
            json!({
                "id":h.document.doc_id().get().to_string(),
                "revision":h.document.revision().get(), "score_bits":h.score.to_bits(),
            })
        })
        .collect::<Vec<_>>();
    Ok((micros, json!(hits)))
}

fn main() -> Result<(), Box<dyn Error>> {
    let output = PathBuf::from(std::env::args().nth(1).ok_or("fresh output root")?);
    std::fs::create_dir(&output)?;
    let mut cases = Vec::new();
    for (rows, segments) in [(8192_usize, 1_usize), (8192, 8), (65536, 1)] {
        for tombstones in [false, true] {
            let directory = output.join(format!("store-{rows}-{segments}-{tombstones}"));
            let store = Store::open(&directory, OpenOptions::default())?;
            let ingestion = Instant::now();
            for segment in 0..segments {
                let docs = (segment * rows / segments..(segment + 1) * rows / segments)
                    .map(|row| {
                        IngestDocument::new(
                            DocumentVersion::new(DocId::new(row as u128 + 1), Revision::new(1)),
                            vec![1.0, 0.0],
                        )
                        .with_text(format!(
                            "common {} {}",
                            term("group", row / 4 % 32),
                            term("unique", row)
                        ))
                    })
                    .collect();
                store.ingest(IngestBatch::new(docs))?;
                store.seal()?;
            }
            let ingestion_seconds = ingestion.elapsed().as_secs_f64();
            if tombstones {
                store.delete(DeleteBatch::new(
                    (0..rows)
                        .step_by(4)
                        .map(|row| DocId::new(row as u128 + 1))
                        .collect(),
                ))?;
            }
            // Prime lexical assembly separately; common's live DF is still cold.
            query(&store, vec!["absent".to_owned()])?;
            let initial_cache_bytes = store.stats()?.cache_bytes;
            let (cold_us, cold_hits) = query(&store, vec!["common".to_owned()])?;
            let mut workloads = Vec::new();
            let mut peak_cache_bytes = initial_cache_bytes;
            for name in ["warm-common", "repeated-groups", "mostly-unique", "absent"] {
                let mut samples = Vec::new();
                let mut controls = Vec::new();
                for sample in 0..148 {
                    let terms = match name {
                        "warm-common" => vec!["common".to_owned()],
                        "repeated-groups" => vec![
                            term("group", sample % 32),
                            term("group", (sample + 11) % 32),
                        ],
                        "mostly-unique" => vec![term("unique", sample * 17 % rows)],
                        _ => vec!["absent".to_owned()],
                    };
                    let (micros, hits) = query(&store, terms)?;
                    if sample >= 20 {
                        samples.push(micros);
                        controls.push(hits);
                    }
                }
                peak_cache_bytes = peak_cache_bytes.max(store.stats()?.cache_bytes);
                workloads.push(json!({"name":name,"us":samples,"controls":controls}));
            }
            cases.push(json!({"sealed_rows":rows,"sealed_segments":segments,
                "live_rows": if tombstones { rows * 3 / 4 } else { rows },
                "tombstones":tombstones,"warmups":20,"ingestion_seconds":ingestion_seconds,
                "cold_us":cold_us,"cold_hits":cold_hits,"workloads":workloads,
                "initial_cache_bytes":initial_cache_bytes,
                "final_cache_bytes":store.stats()?.cache_bytes,
                "peak_sampled_cache_bytes":peak_cache_bytes}));
            store.close()?;
        }
    }
    std::fs::write(
        output.join("results.json"),
        serde_json::to_vec(&json!({"cases":cases}))?,
    )?;
    Ok(())
}
