#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use zeppelin_embed::epoch::{EmbeddingEpoch, StoreEpoch};
use zeppelin_embed::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use zeppelin_embed::property_graph::query::completed::{
    CompletedGraphResult, GraphQueryOptions, Value,
};
use zeppelin_embed_cypher::{CompileLimits, execute};

fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}
mod support;
struct Directory(std::path::PathBuf);
impl Directory {
    fn new() -> Self {
        Self(support::unique_temp_dir("ze366"))
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn fixture() -> (Directory, Store) {
    let directory = Directory::new();
    let tower = zeppelin_embed::epoch::EmbeddingTower {
        model_id: "ze366".into(),
        model_version: "1".into(),
        weights_digest: vec![1],
        dims: 2,
        normalization: zeppelin_embed::epoch::Normalization::None,
        prompt_prefix: String::new(),
        max_tokens: 32,
        runtime: zeppelin_embed::epoch::EmbeddingRuntime::CpuReference,
        compute_units: zeppelin_embed::epoch::ComputeUnits::Cpu,
        os_build: None,
    };
    let epoch = StoreEpoch {
        embedding: EmbeddingEpoch {
            query: tower.clone(),
            document: tower,
            alignment_digest: vec![],
        },
        tokenizer: zeppelin_embed::fts::tokenizer::TokenizerConfig::text_default().epoch(),
    };
    let store = Store::open(
        directory.path(),
        OpenOptions::new()
            .with_epoch(epoch)
            .with_max_resident_bytes(128 * 1024 * 1024),
    )
    .unwrap();
    (directory, store)
}
fn run(store: &Store, source: &str) -> CompletedGraphResult {
    execute(
        store,
        &control(),
        &GraphQueryOptions::default(),
        source,
        &[],
        CompileLimits::default(),
    )
    .unwrap()
}
fn ids(result: &CompletedGraphResult) -> Vec<u128> {
    (0..result.metadata().rows as usize)
        .map(|row| match result.cell(row, 0) {
            Some(Value::Node(index)) => result.pools().nodes[*index as usize].id.get(),
            other => panic!("node expected: {other:?}"),
        })
        .collect()
}

#[test]
fn eligible_set_restricts_both_hybrid_legs() {
    let (_directory, store) = fixture();
    let eligible_id = (1_u128 << 100) + 91;
    let epoch = store.epoch_identity().unwrap();
    store
        .ingest(
            IngestBatch::new(vec![
                IngestDocument::new(
                    DocumentVersion::new(DocId::new(1), Revision::new(1)),
                    vec![0.0, 0.0],
                )
                .with_text("amber amber amber"),
                IngestDocument::new(
                    DocumentVersion::new(DocId::new(eligible_id), Revision::new(1)),
                    vec![5.0, 5.0],
                )
                .with_text("amber birch")
                .with_timestamp(1),
            ])
            .with_epoch(epoch),
        )
        .unwrap();
    store.enable_graph().unwrap();
    for sealed in [false, true] {
        if sealed {
            store.seal().unwrap();
        }
        let result = run(
            &store,
            "MATCH (d:Document) WHERE d.ts = 1 WITH collect(DISTINCT d) AS e CALL ze.hybrid_search([0,0], 'amber', 1, 'exact', e) YIELD node, score, vector_distance, lexical_score RETURN node, score, vector_distance, lexical_score",
        );
        assert_eq!(ids(&result), [eligible_id]);
        let eligible = [DocId::new(eligible_id)];
        let term = zeppelin_embed::fts::search::TermQuery::flat(
            vec![b"amber".to_vec()],
            &[zeppelin_embed::fts::index::DEFAULT_FIELD],
        );
        let expected = store
            .search_hybrid(
                zeppelin_embed::ingest::SearchRequest::new(&[0.0, 0.0]).with_eligible(&eligible),
                &term,
                &zeppelin_embed::fusion::HybridQuery::new(1),
                zeppelin_embed::lifecycle::SearchOptions::default()
                    .with_tier(zeppelin_embed::lifecycle::SearchTier::Exact),
                control(),
            )
            .unwrap();
        assert_eq!(
            result.cell(0, 1),
            Some(&Value::F64(expected.hits[0].fused_score.to_bits()))
        );
        assert_eq!(result.cell(0, 2), Some(&Value::F64(50.0_f64.to_bits())));
        assert_eq!(
            result.cell(0, 3),
            Some(&Value::F64(
                expected.hits[0].lexical_bm25.unwrap().to_bits()
            ))
        );
    }
}

#[test]
fn a_store_without_a_vector_space_refuses_vector_search() {
    let directory = Directory::new();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(128 * 1024 * 1024),
    )
    .unwrap();
    store.enable_graph().unwrap();
    for source in [
        "CALL ze.vector_search([0,0],1,'auto') YIELD node RETURN node",
        "CALL ze.vector_search([0,0],1,'exact') YIELD node RETURN node",
        "CALL ze.vector_search([0,0],1,'exact',[]) YIELD node RETURN node",
        "CALL ze.hybrid_search([0,0],'amber',1,'auto') YIELD node RETURN node",
        "CALL ze.hybrid_search([0,0],'amber',1,'exact',[]) YIELD node RETURN node",
    ] {
        let error = execute(
            &store,
            &control(),
            &GraphQueryOptions::default(),
            source,
            &[],
            CompileLimits::default(),
        )
        .err()
        .expect("typed vector refusal");
        assert!(error.to_string().contains("NoVectorSpace"), "{error}");
        let zeppelin_embed_cypher::StatementError::Query(error) = error else {
            panic!("vector-space refusal must come from retrieval");
        };
        assert_eq!(
            error.kind(),
            zeppelin_embed::property_graph::query::completed::GraphQueryErrorKind::Constraint
        );
        assert!(error.nothing_committed());
    }
}

#[test]
fn last_as_prefix_keeps_the_top_32_expansions() {
    let (_directory, store) = fixture();
    let epoch = store.epoch_identity().unwrap();
    let documents = (0..70)
        .map(|n| {
            IngestDocument::new(
                DocumentVersion::new(DocId::new(n + 1), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_text(format!(
                "prefix{}{}",
                char::from(b'a' + (n / 26) as u8),
                char::from(b'a' + (n % 26) as u8)
            ))
        })
        .collect();
    store
        .ingest(IngestBatch::new(documents).with_epoch(epoch))
        .unwrap();
    store.enable_graph().unwrap();
    let options = GraphQueryOptions::default()
        .with_search_options(zeppelin_embed::property_graph::query::plan::SearchOptions {
            last_as_prefix: true,
            ..Default::default()
        })
        .unwrap();
    let source = "CALL ze.text_search('prefix',100) YIELD node,score RETURN node,score";
    let expected_query = zeppelin_embed::fts::query::LexicalQuery::TermsWithPrefix {
        terms: vec![],
        prefix: b"prefix".to_vec(),
        fields: zeppelin_embed::fts::search::FieldWeights::flat(&[
            zeppelin_embed::fts::index::DEFAULT_FIELD,
        ]),
    };
    for sealed in [false, true] {
        if sealed {
            store.seal().unwrap();
        }
        let expected = store
            .search_lexical_structured(&expected_query, 100, 100, control())
            .unwrap();
        assert_eq!(expected.expansions.len(), 32);
        let actual = execute(
            &store,
            &control(),
            &options,
            source,
            &[],
            CompileLimits::default(),
        )
        .unwrap();
        assert_eq!(
            ids(&actual),
            expected
                .candidates
                .iter()
                .map(|hit| hit.document.doc_id().get())
                .collect::<Vec<_>>()
        );
        assert!(actual.metadata().rows >= 32);
        for (row, hit) in expected.candidates.iter().enumerate() {
            assert_eq!(actual.cell(row, 1), Some(&Value::F64(hit.score.to_bits())));
        }
    }
}

#[test]
fn text_search_returns_the_same_hits_as_store_query() {
    let (_directory, store) = fixture();
    store
        .ingest(
            IngestBatch::new(vec![
                IngestDocument::new(
                    DocumentVersion::new(DocId::new(91), Revision::new(1)),
                    vec![1.0, 0.0],
                )
                .with_text("amber birch"),
                IngestDocument::new(
                    DocumentVersion::new(DocId::new(92), Revision::new(1)),
                    vec![0.0, 1.0],
                )
                .with_text("amber"),
            ])
            .with_epoch(store.epoch_identity().unwrap()),
        )
        .unwrap();
    store.enable_graph().unwrap();
    for sealed in [false, true] {
        if sealed {
            store.seal().unwrap();
        }
        let expected = store
            .search_lexical(
                &zeppelin_embed::fts::search::TermQuery::flat(
                    vec![b"amber".to_vec()],
                    &[zeppelin_embed::fts::index::DEFAULT_FIELD],
                ),
                10,
                control(),
            )
            .unwrap();
        let actual = run(
            &store,
            "CALL ze.text_search('amber',10) YIELD node,score RETURN node,score",
        );
        assert_eq!(actual.metadata().rows as usize, expected.candidates.len());
        assert_eq!(
            ids(&actual),
            expected
                .candidates
                .iter()
                .map(|hit| hit.document.doc_id().get())
                .collect::<Vec<_>>()
        );
        for (row, hit) in expected.candidates.iter().enumerate() {
            assert_eq!(actual.cell(row, 1), Some(&Value::F64(hit.score.to_bits())));
        }
    }
}
