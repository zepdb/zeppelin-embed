//! Matched core API screen for combined structured lexical top-k.
use std::time::Instant;
use zeppelin_embed::fts::{
    index::DEFAULT_FIELD, preparation_observer, query::LexicalQuery, search::TermQuery,
};
use zeppelin_embed::fusion::HybridQuery;
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::{
    CancelToken, OpenOptions, QueryControl, SearchOptions, SearchTier, Store,
};

const GROUPS: [&str; 8] = [
    "alpha", "beta", "gamma", "delta", "theta", "kappa", "zeta", "sigma",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mode = std::env::args()
        .nth(1)
        .ok_or("prefix, fuzzy, hybrid, or flat required")?;
    if !matches!(mode.as_str(), "prefix" | "fuzzy" | "hybrid" | "flat") {
        return Err("invalid mode".into());
    }
    let directory = tempfile::tempdir()?;
    let store = Store::open(directory.path(), OpenOptions::default())?;
    let ingestion = Instant::now();
    for first in (0..16_000).step_by(1_000) {
        store.ingest(IngestBatch::new(
            (first..first + 1_000)
                .map(|row| {
                    let mut words = Vec::new();
                    for slot in 0..3 {
                        let term = format!(
                            "{}{:02}",
                            GROUPS[(row + slot * 3) % GROUPS.len()],
                            (row / 8 + slot) % 16
                        );
                        words.extend(vec![term; 1 + (row + slot * 7) % 6]);
                    }
                    words.extend(vec!["filler".to_owned(); 4 + row % 23]);
                    IngestDocument::new(
                        DocumentVersion::new(DocId::new(row as u128 + 1), Revision::new(1)),
                        vec![1.0, row as f32 / 16_000.0, 0.0, 0.0, 0.0, 0.0],
                    )
                    .with_text(words.join(" "))
                })
                .collect(),
        ))?;
    }
    store.seal()?;
    let ingest_ms = ingestion.elapsed().as_secs_f64() * 1_000.0;
    let mut samples = Vec::new();
    for index in 0_usize..533 {
        let query_id = index.saturating_sub(21) % 64;
        let group = GROUPS[query_id % GROUPS.len()];
        let expanded = if mode == "fuzzy" {
            LexicalQuery::fuzzy(
                format!("{group}{:02}", query_id % 16).into_bytes(),
                2,
                DEFAULT_FIELD,
            )
        } else {
            LexicalQuery::prefix(group.as_bytes().to_vec(), DEFAULT_FIELD)
        };
        let vector = [1.0, 0.0, 0.0, 0.0, 0.0, query_id as f32 * 0.0001];
        let term = TermQuery::flat(vec![format!("{group}00").into_bytes()], &[DEFAULT_FIELD]);
        // A separate untimed call after warmup observes actual work ownership.
        if index == 20 {
            preparation_observer::begin();
        }
        let started = Instant::now();
        let (elapsed, hits, counters, expansion_count, rounds) = if mode == "hybrid" {
            let result = store.search_hybrid_structured(
                SearchRequest::new(&vector),
                &expanded,
                &HybridQuery::new(10),
                SearchOptions::default().with_tier(SearchTier::Exact),
                QueryControl::Cancel(CancelToken::new()),
            )?;
            let elapsed = started.elapsed().as_secs_f64() * 1e6;
            let hits = result.hits.iter().map(|hit| serde_json::json!({"id":hit.key.get().to_string(), "score":hit.fused_score.to_bits(), "lexical":hit.lexical_bm25.map(f64::to_bits), "vector":hit.vector_squared_l2.map(f64::to_bits)})).collect::<Vec<_>>();
            let counters = result.diagnostics.counters.lexical;
            let rounds = result
                .diagnostics
                .fusion
                .as_ref()
                .ok_or("missing fusion report")?
                .rounds;
            (
                elapsed,
                hits,
                counters,
                result.lexical_expansions.len(),
                rounds,
            )
        } else if mode == "flat" {
            let result =
                store.search_lexical(&term, 10, QueryControl::Cancel(CancelToken::new()))?;
            let elapsed = started.elapsed().as_secs_f64() * 1e6;
            let hits = result.candidates.iter().map(|hit| serde_json::json!({"id":hit.document.doc_id().get().to_string(), "score":hit.score.to_bits()})).collect::<Vec<_>>();
            (elapsed, hits, result.diagnostics.counters.lexical, 1, 1)
        } else {
            let result = store.search_lexical_structured(
                &expanded,
                10,
                128,
                QueryControl::Cancel(CancelToken::new()),
            )?;
            let elapsed = started.elapsed().as_secs_f64() * 1e6;
            let hits = result.candidates.iter().map(|hit| serde_json::json!({"id":hit.document.doc_id().get().to_string(), "score":hit.score.to_bits(), "provenance":hit.provenance.iter().map(|entry| serde_json::json!({"term":entry.term,"boost":entry.boost_thousandths})).collect::<Vec<_>>(), "snippet":hit.snippet.text})).collect::<Vec<_>>();
            (
                elapsed,
                hits,
                result.diagnostics.counters.lexical,
                result.expansions.len(),
                1,
            )
        };
        if index == 20 {
            let corpus_preparations = preparation_observer::corpus_statistics_calls();
            let (scorers, frequencies, expansions, retained, bounds) =
                preparation_observer::take_with_bounds();
            eprintln!(
                "OBSERVATION {}",
                serde_json::json!({"corpus_preparations":corpus_preparations,"scorers":scorers,"frequencies":frequencies,"expansions":expansions,"peak_retained_rows":retained,"bound_term_evaluations":bounds,"posting_decodes":counters.postings_decoded,"blocks_decoded":counters.blocks_decoded,"blocks_skipped":counters.blocks_skipped})
            );
        }
        if index >= 21 {
            samples.push(serde_json::json!({"query_id":query_id,"latency_us":elapsed,"hits":hits,"expansion_count":expansion_count,"rounds":rounds,"postings":counters.postings_decoded,"blocks_decoded":counters.blocks_decoded,"blocks_skipped":counters.blocks_skipped,"docs_evaluated":counters.docs_evaluated}));
        }
        if store.stats()?.temporary_bytes != 0 {
            return Err("query temporary memory survived return".into());
        }
    }
    store.close()?;
    println!(
        "{}",
        serde_json::json!({"kind":"core API only; no embedding", "mode":mode,"rows":16_000,"dimensions":6,"k":10,"warmups":20,"untimed_work_probes":1,"ingest_ms":ingest_ms,"samples":samples,"all_threads_joined":true})
    );
    Ok(())
}
