//! Matched core phrase screen; source text and query order are deterministic.
use std::time::Instant;
use zeppelin_embed::fts::{index::DEFAULT_FIELD, preparation_observer, query::LexicalQuery};
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
        .ok_or("common, rare, repeated or hybrid required")?;
    if !matches!(mode.as_str(), "common" | "rare" | "repeated" | "hybrid") {
        return Err("unknown mode".into());
    }
    let directory = tempfile::tempdir()?;
    let store = Store::open(directory.path(), OpenOptions::default())?;
    for first in (0..4000).step_by(500) {
        store.ingest(IngestBatch::new(
            (first..first + 500)
                .map(|row| {
                    let phrase = if row % 3 == 0 {
                        "beta alpha "
                    } else {
                        "alpha beta "
                    };
                    let mut text = phrase.repeat(8);
                    if row % 4 == 0 {
                        text.push_str("alpha alpha beta ");
                    }
                    if row % 20 == 0 {
                        text.push_str(if row % 40 == 0 {
                            "rareword alpha "
                        } else {
                            "alpha rareword "
                        });
                    }
                    text.push_str(&"filler ".repeat(32 + row % 16));
                    IngestDocument::new(
                        DocumentVersion::new(DocId::new(row as u128 + 1), Revision::new(1)),
                        vec![1.0, row as f32 / 4000.0, 0.0, 0.0, 0.0, 0.0],
                    )
                    .with_text(text)
                })
                .collect(),
        ))?;
    }
    store.seal()?;
    let mut samples = Vec::new();
    for iteration in 0_usize..149 {
        let query_id = iteration.saturating_sub(21) % 64;
        let words = match mode.as_str() {
            "rare" => vec!["alpha", "rareword"],
            "repeated" => vec!["alpha", "alpha", "beta"],
            _ => vec!["alpha", "beta"],
        };
        let query = LexicalQuery::phrase(
            words.iter().map(|word| word.as_bytes().to_vec()).collect(),
            (query_id % 4) as u32,
            DEFAULT_FIELD,
        );
        if iteration == 20 {
            preparation_observer::begin();
        }
        let started = Instant::now();
        let (elapsed_us, hits, postings) = if mode == "hybrid" {
            let result = store.search_hybrid_structured(
                SearchRequest::new(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
                &query,
                &HybridQuery::new(10),
                SearchOptions::default().with_tier(SearchTier::Exact),
                QueryControl::Cancel(CancelToken::new()),
            )?;
            let elapsed_us = started.elapsed().as_secs_f64() * 1e6;
            let hits = result.hits.iter().map(|hit| serde_json::json!({"id":hit.key.get().to_string(),"score":hit.fused_score.to_bits(),"lexical":hit.lexical_bm25.map(f64::to_bits),"vector":hit.vector_squared_l2.map(f64::to_bits)})).collect::<Vec<_>>();
            (
                elapsed_us,
                hits,
                result.diagnostics.counters.lexical.postings_decoded,
            )
        } else {
            let result = store.search_lexical_structured(
                &query,
                10,
                128,
                QueryControl::Cancel(CancelToken::new()),
            )?;
            let elapsed_us = started.elapsed().as_secs_f64() * 1e6;
            let hits = result.candidates.iter().map(|hit| serde_json::json!({"id":hit.document.doc_id().get().to_string(),"score":hit.score.to_bits(),"snippet":hit.snippet.text,"provenance":hit.provenance.iter().map(|entry| serde_json::json!({"term":entry.term,"boost":entry.boost_thousandths})).collect::<Vec<_>>()})).collect::<Vec<_>>();
            (
                elapsed_us,
                hits,
                result.diagnostics.counters.lexical.postings_decoded,
            )
        };
        if iteration == 20 {
            let (calls, text_bytes) = preparation_observer::phrase_reanalysis_work();
            let (positions, position_bytes) = preparation_observer::phrase_position_work();
            preparation_observer::take();
            eprintln!(
                "OBSERVATION {}",
                serde_json::json!({"analyzer_calls":calls,"eligibility_text_bytes":text_bytes,"positions_decoded":positions,"position_bytes":position_bytes,"bm25_postings":postings})
            );
        }
        if iteration >= 21 {
            samples.push(serde_json::json!({"query_id":query_id,"latency_us":elapsed_us,"hits":hits,"bm25_postings":postings}));
        }
        if store.stats()?.temporary_bytes != 0 {
            return Err("query temporary memory survived return".into());
        }
    }
    store.close()?;
    println!(
        "{}",
        serde_json::json!({"scope":"core API; synthetic phrase stress; no embedding","rows":4000,"dimensions":6,"k":10,"warmups":20,"untimed_probes":1,"mode":mode,"samples":samples,"all_threads_joined":true})
    );
    Ok(())
}
