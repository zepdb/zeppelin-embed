use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, Normalization, StoreEpoch,
};
use zeppelin_embed::fts::{index::FieldId, search::TermQuery, tokenizer::TokenizerConfig};
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, SearchOptions, Store};
use zeppelin_embed::meta::{ColumnDefinition, ColumnId, ColumnType, PredicateValue, Schema};
pub fn epoch() -> StoreEpoch {
    let tower = EmbeddingTower {
        model_id: "release-fixture".into(),
        model_version: "1".into(),
        weights_digest: vec![0x33, 0x8],
        dims: 8,
        normalization: Normalization::None,
        prompt_prefix: String::new(),
        max_tokens: 512,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    StoreEpoch {
        embedding: EmbeddingEpoch {
            query: tower.clone(),
            document: tower,
            alignment_digest: vec![],
        },
        tokenizer: TokenizerConfig::text_default().epoch(),
    }
}
pub fn options(read_only: bool) -> OpenOptions {
    (if read_only {
        OpenOptions::read_only()
    } else {
        OpenOptions::new()
    })
    .with_epoch(epoch())
}
#[allow(dead_code)]
pub fn schema() -> Schema {
    Schema::new(vec![ColumnDefinition::new(
        ColumnId::new(1),
        "rank",
        ColumnType::U64,
        false,
    )])
    .expect("schema")
}
pub fn vector(id: u128) -> Vec<f32> {
    (0..8)
        .map(|axis| if axis == 0 { id as f32 } else { axis as f32 })
        .collect()
}
pub fn document(id: u128, text: &str) -> IngestDocument {
    document_revision(id, 1, text)
}
pub fn document_revision(id: u128, revision: u64, text: &str) -> IngestDocument {
    IngestDocument::new(
        DocumentVersion::new(DocId::new(id), Revision::new(revision)),
        vector(id),
    )
    .with_timestamp(id as i64)
    .with_text(text)
    .with_metadata(format!("metadata-{id}").into_bytes())
    .with_columns(vec![(ColumnId::new(1), PredicateValue::U64(id as u64))])
}
pub fn batch(documents: Vec<IngestDocument>) -> IngestBatch {
    IngestBatch::new(documents).with_epoch(epoch().identity())
}
pub fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}
pub fn text_hits(store: &Store, query: &str) -> Vec<u128> {
    store
        .search_lexical(
            &TermQuery::flat(vec![query.as_bytes().to_vec()], &[FieldId(0)]),
            10,
            control(),
        )
        .expect("text query")
        .candidates
        .iter()
        .map(|hit| hit.document.doc_id().get())
        .collect()
}
pub fn vector_hits(store: &Store) -> Vec<u128> {
    store
        .search(
            SearchRequest::new(&vector(1)),
            3,
            SearchOptions::default(),
            control(),
        )
        .expect("vector query")
        .candidates
        .iter()
        .map(|hit| hit.document().expect("identity").doc_id().get())
        .collect()
}
