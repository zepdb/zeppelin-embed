//! Step 13: bounded byte fuzzy expansion through real Store query paths.
use serde_json::{Value, json};
use std::error::Error;
use std::time::Instant;
use zeppelin_embed::fts::index::DEFAULT_FIELD;
use zeppelin_embed::fts::preparation_observer as observer;
use zeppelin_embed::fts::query::LexicalQuery;
use zeppelin_embed::fts::search::TermQuery;
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision,
    StoreStructuredLexicalSearchOutcome,
};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};

fn suffix(mut n: usize) -> String {
    (0..4)
        .map(|_| {
            let c = char::from(b'a' + (n % 26) as u8);
            n /= 26;
            c
        })
        .collect()
}
fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}
fn query(id: usize, mode: &str) -> LexicalQuery {
    let term = if mode == "prefix" {
        format!("{}{}", "a".repeat(60), suffix(id * 71 % 4096))
    } else {
        match id % 4 {
            0 => "z".repeat(4),
            1 => "z".repeat(64),
            _ => format!("{}{}", "a".repeat(60), suffix(id * 71 % 4096)),
        }
    };
    if mode == "prefix" {
        LexicalQuery::prefix(term.into_bytes(), DEFAULT_FIELD)
    } else {
        LexicalQuery::fuzzy(term.into_bytes(), 2, DEFAULT_FIELD)
    }
}
fn payload(result: StoreStructuredLexicalSearchOutcome) -> Value {
    json!({
        "expansions": result.expansions.iter().map(|e| json!({"term":e.term,"boost":e.boost_thousandths,"kind":format!("{:?}",e.kind)})).collect::<Vec<_>>(),
        "hits": result.candidates.iter().map(|c| json!({"id":c.document.doc_id().get().to_string(),"score_bits":c.score.to_bits(),"snippet":c.snippet.text,"source":format!("{:?}",c.snippet.source),"highlights":format!("{:?}",c.snippet.highlights),"provenance":format!("{:?}",c.provenance)})).collect::<Vec<_>>(),
        "postings": result.diagnostics.counters.lexical.postings_decoded,
    })
}
fn work() -> Value {
    let w = observer::vocabulary_work();
    let f = observer::fuzzy_work();
    json!({"builds":w.builds,"copied_bytes":w.copied_bytes,"terms_visited":w.terms_visited,"seek_steps":w.seek_steps,"fuzzy_candidates":f.candidates,"dp_cells":f.dp_cells,"row_allocations":f.row_allocations})
}
fn measure(store: &Store, id: usize, mode: &str) -> Result<Value, Box<dyn Error>> {
    let q = query(id, mode);
    let start = Instant::now();
    let (us, payload) = if mode == "hybrid" {
        use zeppelin_embed::fusion::HybridQuery;
        use zeppelin_embed::ingest::SearchRequest;
        use zeppelin_embed::lifecycle::{SearchOptions, SearchTier};
        let vector = [1.0, id as f32 * 0.0001];
        let result = store.search_hybrid_structured(
            SearchRequest::new(&vector),
            &q,
            &HybridQuery::new(10),
            SearchOptions::default().with_tier(SearchTier::Exact),
            control(),
        )?;
        let us = start.elapsed().as_secs_f64() * 1e6;
        let payload = json!({
            "expansions":result.lexical_expansions.iter().map(|e| json!({"term":e.term,"boost":e.boost_thousandths,"kind":format!("{:?}",e.kind)})).collect::<Vec<_>>(),
            "hits":result.hits.iter().map(|h| json!({"id":h.key.get().to_string(),"score":h.fused_score.to_bits(),"lexical":h.lexical_bm25.map(f64::to_bits),"vector":h.vector_squared_l2.map(f64::to_bits)})).collect::<Vec<_>>(),
            "postings":result.diagnostics.counters.lexical.postings_decoded,
            "fusion":format!("{:?}",result.diagnostics.fusion),
        });
        (us, payload)
    } else {
        let result = store.search_lexical_structured(&q, 10, 64, control())?;
        (start.elapsed().as_secs_f64() * 1e6, payload(result))
    };
    assert_eq!(store.stats()?.temporary_bytes, 0);
    Ok(json!({"query_id":id,"latency_us":us,"payload":payload}))
}
fn main() -> Result<(), Box<dyn Error>> {
    let mode = std::env::args()
        .nth(1)
        .ok_or("fuzzy, hybrid, or prefix required")?;
    if !matches!(mode.as_str(), "fuzzy" | "hybrid" | "prefix") {
        return Err("invalid mode".into());
    }
    let dir = tempfile::tempdir()?;
    let store = Store::open(dir.path(), OpenOptions::default())?;
    let documents = (0..4096)
        .map(|id| {
            IngestDocument::new(
                DocumentVersion::new(DocId::new(id as u128 + 1), Revision::new(1)),
                vec![1.0, id as f32 / 4096.0],
            )
            .with_text(format!("anchor {}{}", "a".repeat(60), suffix(id)))
        })
        .collect();
    store.ingest(IngestBatch::new(documents))?;
    store.seal()?;
    // Prepare the ordinary immutable assembly before timing dictionary build.
    store.search_lexical(
        &TermQuery::flat(vec![b"anchor".to_vec()], &[DEFAULT_FIELD]),
        3,
        control(),
    )?;
    let cache_before = store.stats()?.cache_bytes;
    observer::begin();
    let cold = measure(&store, 3, &mode)?;
    let cold_work = work();
    observer::take();
    let cache_after = store.stats()?.cache_bytes;
    for id in 0..20 {
        measure(&store, id, &mode)?;
    }
    observer::begin();
    measure(&store, 3, &mode)?;
    let warm_work = work();
    observer::take();
    let mut steady = Vec::new();
    for id in (0..64).cycle().take(128) {
        steady.push(measure(&store, id, &mode)?);
    }
    let mut updates = Vec::new();
    for id in 0..16 {
        store.ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(7000 + id as u128), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_text(format!("anchor {}{}", "a".repeat(60), suffix(5000 + id))),
        ]))?;
        observer::begin();
        let sample = measure(&store, 3, &mode)?;
        let update_work = work();
        observer::take();
        updates.push(
            json!({"sample":sample,"work":update_work,"cache_bytes":store.stats()?.cache_bytes}),
        );
    }
    let cache_final = store.stats()?.cache_bytes;
    store.close()?;
    println!(
        "{}",
        json!({"mode":mode,"rows":4096,"dimensions":2,"warmups":20,"k":10,"cold":cold,"cold_work":cold_work,"warm_work":warm_work,"cache_before":cache_before,"cache_after":cache_after,"cache_final":cache_final,"steady":steady,"updates":updates})
    );
    Ok(())
}
