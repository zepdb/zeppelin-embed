//! Synthetic filter preparation cost, with measured work and raw timings.
use std::{error::Error, time::Instant};
use zeppelin_embed::{
    fts::{index::DEFAULT_FIELD, preparation_observer as observer, search::TermQuery},
    ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision},
    lifecycle::{
        CancelToken, DocumentFields, DocumentScanRequest, OpenOptions, QueryControl, QueryFilter,
        Store,
    },
    meta::{ColumnDefinition, ColumnId, ColumnType, Predicate, PredicateValue, Schema},
};
fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}
fn main() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let column = ColumnId::new(1);
    let schema = Schema::new(vec![ColumnDefinition::new(
        column,
        "note_id",
        ColumnType::RawString,
        false,
    )])?;
    let store = Store::open(directory.path(), OpenOptions::default().with_schema(schema))?;
    for batch in 0..15 {
        let documents = (batch * 10000..(batch + 1) * 10000)
            .map(|row| {
                IngestDocument::new(
                    DocumentVersion::new(DocId::new(row as u128 + 1), Revision::new(1)),
                    vec![1.0, 0.0],
                )
                .with_text(if row % 20 == 0 {
                    "amber common"
                } else {
                    "other common"
                })
                .with_columns(vec![(
                    column,
                    PredicateValue::String(format!("note-{}", row / 300)),
                )])
            })
            .collect();
        store.ingest(IngestBatch::new(documents))?;
    }
    store.seal()?;
    let query = TermQuery::flat(vec![b"amber".to_vec()], &[DEFAULT_FIELD]);
    // Separate unfiltered assembly preparation from cold predicate preparation.
    store.search_lexical(&query, 20, control())?;
    for ids in [1, 10, 100, 500] {
        let predicate = Predicate::In {
            column,
            values: (0..ids)
                .map(|id| PredicateValue::String(format!("note-{id}")))
                .collect(),
        };
        let filter = QueryFilter::new(store.schema(), Some(&predicate), None)?;
        for sample in 0..6 {
            observer::begin();
            let start = Instant::now();
            let result = store.search_lexical_filtered(&query, 20, control(), filter.as_ref())?;
            let micros = start.elapsed().as_secs_f64() * 1e6;
            let work = observer::filter_work();
            observer::take();
            println!(
                "lexical ids={ids} sample={sample} us={micros:.3} hits={} work={work:?} cache_bytes={}",
                result.candidates.len(),
                store.stats()?.cache_bytes
            );
        }
        observer::begin();
        let start = Instant::now();
        let count = store.count_documents(Some(&predicate), None)?;
        let micros = start.elapsed().as_secs_f64() * 1e6;
        println!(
            "count ids={ids} us={micros:.3} rows={} work={:?}",
            count.count,
            observer::filter_work()
        );
        observer::take();
        observer::begin();
        let start = Instant::now();
        let page = store.scan_documents(
            DocumentScanRequest::new(150000, DocumentFields::NONE, control())
                .with_predicate(&predicate),
        )?;
        let micros = start.elapsed().as_secs_f64() * 1e6;
        println!(
            "scan ids={ids} us={micros:.3} rows={} work={:?}",
            page.documents.len(),
            observer::filter_work()
        );
        observer::take();
    }
    let predicate = Predicate::In {
        column,
        values: vec![PredicateValue::String("note-0".into())],
    };
    let filter = QueryFilter::new(store.schema(), Some(&predicate), None)?;
    let exact = zeppelin_embed::fts::query::LexicalQuery::term(query.clone());
    let prefix = zeppelin_embed::fts::query::LexicalQuery::prefix(b"amb".to_vec(), DEFAULT_FIELD);
    for sample in 0..6 {
        for (name, query) in [("exact", &exact), ("prefix", &prefix)] {
            observer::begin();
            let start = Instant::now();
            let answer = store.search_lexical_structured_filtered(
                query,
                20,
                64,
                control(),
                filter.as_ref(),
            )?;
            let micros = start.elapsed().as_secs_f64() * 1e6;
            println!(
                "structured {name} sample={sample} us={micros:.3} hits={} dictionary={:?}",
                answer.candidates.len(),
                observer::vocabulary_work()
            );
            observer::take();
        }
    }
    store.close()?;
    Ok(())
}
