//! Step 12: immutable dictionary reuse, prefix ranges, and active-input changes.
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
fn query(id: usize) -> LexicalQuery {
    let prefix = if id.is_multiple_of(8) {
        "mrare".to_string()
    } else {
        format!("aaa{}", suffix(id * 71 % 6000))
    };
    LexicalQuery::prefix(prefix.into_bytes(), DEFAULT_FIELD)
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
    json!({"builds":w.builds,"copied_bytes":w.copied_bytes,"terms_visited":w.terms_visited,"seek_steps":w.seek_steps})
}
fn measure(store: &Store, id: usize) -> Result<Value, Box<dyn Error>> {
    let q = query(id);
    let start = Instant::now();
    let result = store.search_lexical_structured(&q, 3, 64, control())?;
    let us = start.elapsed().as_secs_f64() * 1e6;
    let payload = payload(result);
    assert_eq!(store.stats()?.temporary_bytes, 0);
    Ok(json!({"query_id":id,"latency_us":us,"payload":payload}))
}
fn main() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let store = Store::open(dir.path(), OpenOptions::default())?;
    let documents = (0..6000)
        .map(|id| {
            let marker = if id < 3 {
                format!(" mrare{}", char::from(b'a' + id as u8))
            } else {
                String::new()
            };
            IngestDocument::new(
                DocumentVersion::new(DocId::new(id as u128 + 1), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_text(format!(
                "anchor aaa{} zzz{}{marker}",
                suffix(id),
                suffix(id)
            ))
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
    let cold = measure(&store, 0)?;
    let cold_work = work();
    observer::take();
    let cache_after = store.stats()?.cache_bytes;
    for id in 0..20 {
        measure(&store, id)?;
    }
    observer::begin();
    measure(&store, 0)?;
    let warm_work = work();
    observer::take();
    let mut steady = Vec::new();
    for id in (0..64).cycle().take(128) {
        steady.push(measure(&store, id)?);
    }
    let mut updates = Vec::new();
    for id in 0..16 {
        store.ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(7000 + id as u128), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_text(format!("patch{} mrarepatch", suffix(id))),
        ]))?;
        observer::begin();
        let sample = measure(&store, 0)?;
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
        json!({"rows":6000,"warmups":20,"k":3,"cold":cold,"cold_work":cold_work,"warm_work":warm_work,"cache_before":cache_before,"cache_after":cache_after,"cache_final":cache_final,"steady":steady,"updates":updates})
    );
    Ok(())
}
