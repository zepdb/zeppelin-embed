//! Core-only widening/one-round screen; run each arm in independent processes.
use std::time::Instant;
use zeppelin_embed::fts::{index::DEFAULT_FIELD, search::TermQuery};
use zeppelin_embed::fusion::HybridQuery;
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::{
    CancelToken, OpenOptions, QueryControl, SearchOptions, SearchTier, Store,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mode = std::env::args()
        .nth(1)
        .ok_or("widening or one-round required")?;
    if !matches!(mode.as_str(), "widening" | "one-round") {
        return Err("widening or one-round required".into());
    }
    let directory = tempfile::tempdir()?;
    let store = Store::open(directory.path(), OpenOptions::default())?;
    let documents = (0..400)
        .map(|row| {
            let identity = DocumentVersion::new(DocId::new(row as u128 + 1), Revision::new(1));
            let document =
                IngestDocument::new(identity, vec![1.0, row as f32 * 0.0025, 0.0, 0.0, 0.0, 0.0]);
            if row < 200 {
                document
            } else {
                document.with_text(vec!["zeppelin"; 1 + (400 - row) % 5].join(" ").as_str())
            }
        })
        .collect();
    store.ingest(IngestBatch::new(documents))?;
    let terms = TermQuery::flat(
        vec![if mode == "widening" {
            b"zeppelin".to_vec()
        } else {
            b"absent".to_vec()
        }],
        &[DEFAULT_FIELD],
    );
    let hybrid = HybridQuery::new(1);
    let mut samples = Vec::new();
    for index in 0_usize..532 {
        let query_id = index.saturating_sub(20) % 64;
        let vector = [1.0, 0.0, 0.0, 0.0, 0.0, query_id as f32 * 0.0001];
        let started = Instant::now();
        let result = store.search_hybrid(
            SearchRequest::new(&vector),
            &terms,
            &hybrid,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )?;
        let latency_us = started.elapsed().as_secs_f64() * 1e6;
        let fusion = result
            .diagnostics
            .fusion
            .as_ref()
            .ok_or("missing fusion report")?;
        let work = result
            .diagnostics
            .hybrid
            .as_ref()
            .ok_or("missing hybrid work")?;
        if fusion.rounds != if mode == "widening" { 3 } else { 1 } {
            return Err(format!("unexpected {} rounds for {mode}", fusion.rounds).into());
        }
        if index >= 20 {
            samples.push(serde_json::json!({
                "query_id":query_id, "latency_us":latency_us, "rounds":fusion.rounds,
                "termination":format!("{:?}",fusion.termination),
                "hits":result.hits.iter().map(|hit| serde_json::json!({"key":hit.key.get().to_string(), "score_bits":hit.fused_score.to_bits(), "vector_bits":hit.vector_squared_l2.map(f64::to_bits), "lexical_bits":hit.lexical_bm25.map(f64::to_bits)})).collect::<Vec<_>>(),
                "dimensions":result.diagnostics.counters.scan.dims_touched,
                "bytes":result.diagnostics.counters.scan.bytes_read,
                "vector_cross_scores":work.total_cross_filled_vector,
                "lexical_cross_scores":work.total_cross_filled_lexical,
            }));
        }
        if store.stats()?.temporary_bytes != 0 {
            return Err("query retained temporary bytes".into());
        }
    }
    store.close()?;
    println!(
        "{}",
        serde_json::json!({"kind":"core-only hybrid", "mode":mode, "rows":400, "dimensions":6, "warmups":20, "samples":samples, "all_threads_joined":true})
    );
    Ok(())
}
