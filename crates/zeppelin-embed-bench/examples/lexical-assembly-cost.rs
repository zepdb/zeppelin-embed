//! Public Store lexical setup under active mutation; build without test-support.
use serde_json::json;
use std::{error::Error, path::PathBuf, time::Instant};
use zeppelin_embed::{
    fts::{index::DEFAULT_FIELD, search::TermQuery},
    ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision},
    lifecycle::{CancelToken, OpenOptions, QueryControl, Store},
};

fn main() -> Result<(), Box<dyn Error>> {
    let output = PathBuf::from(std::env::args().nth(1).ok_or("fresh output root")?);
    std::fs::create_dir(&output)?;
    let mut cases = Vec::new();
    for (rows, segments) in [(8192_usize, 1_usize), (8192, 8), (65536, 1)] {
        let directory = output.join(format!("store-{rows}-{segments}"));
        let store = Store::open(&directory, OpenOptions::default())?;
        for segment in 0..segments {
            let docs = (segment * rows / segments..(segment + 1) * rows / segments)
                .map(|row| document(row as u128 + 1))
                .collect();
            store.ingest(IngestBatch::new(docs))?;
            store.seal()?;
        }
        let absent = TermQuery::flat(vec![b"absent".to_vec()], &[DEFAULT_FIELD]);
        let present = TermQuery::flat(vec![b"common".to_vec()], &[DEFAULT_FIELD]);
        let query =
            |q: &TermQuery| store.search_lexical(q, 10, QueryControl::Cancel(CancelToken::new()));
        query(&absent)?;
        let initial_cache_bytes = store.stats()?.cache_bytes;
        let mut warm_us = Vec::new();
        for sample in 0..148 {
            let start = Instant::now();
            let answer = query(&absent)?;
            let micros = start.elapsed().as_secs_f64() * 1e6;
            if !answer.candidates.is_empty() {
                return Err("absent query returned hits".into());
            }
            if sample >= 20 {
                warm_us.push(micros);
            }
        }
        let mut setup_us = Vec::new();
        let mut present_us = Vec::new();
        let mut controls = Vec::new();
        let mut peak_cache_bytes = initial_cache_bytes;
        for sample in 0..84_u128 {
            store.ingest(IngestBatch::new(vec![document(rows as u128 + sample + 1)]))?;
            let start = Instant::now();
            let answer = query(&absent)?;
            let setup = start.elapsed().as_secs_f64() * 1e6;
            if !answer.candidates.is_empty() {
                return Err("absent query returned hits".into());
            }
            let start = Instant::now();
            let answer = query(&present)?;
            let elapsed = start.elapsed().as_secs_f64() * 1e6;
            if sample >= 20 {
                setup_us.push(setup);
                present_us.push(elapsed);
                controls.push(answer.candidates.iter().map(|h| json!({
                    "id":h.document.doc_id().get().to_string(),"revision":h.document.revision().get(),
                    "score_bits":h.score.to_bits(),
                })).collect::<Vec<_>>());
            }
            peak_cache_bytes = peak_cache_bytes.max(store.stats()?.cache_bytes);
        }
        let final_cache_bytes = store.stats()?.cache_bytes;
        cases.push(json!({"sealed_rows":rows,"sealed_segments":segments,
            "warmups":20,"warm_us":warm_us,"setup_us":setup_us,"present_us":present_us,
            "controls":controls,"initial_cache_bytes":initial_cache_bytes,
            "final_cache_bytes":final_cache_bytes,"peak_sampled_cache_bytes":peak_cache_bytes}));
        store.close()?;
    }
    std::fs::write(
        output.join("results.json"),
        serde_json::to_vec(&json!({"cases":cases}))?,
    )?;
    Ok(())
}

fn document(id: u128) -> IngestDocument {
    IngestDocument::new(
        DocumentVersion::new(DocId::new(id), Revision::new(1)),
        vec![1.0, 0.0],
    )
    .with_text(match id % 3 {
        0 => "common pair",
        1 => "common common",
        _ => "other",
    })
}
