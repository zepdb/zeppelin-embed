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

#[test]
fn ze400_hybrid_max_rounds_is_bounded_by_corpus_and_work_limit() {
    use zeppelin_embed::property_graph::query::completed::GraphQueryErrorKind;
    use zeppelin_embed::property_graph::query::plan::SearchOptions;
    use zeppelin_embed::property_graph::query::runtime::{RuntimeLimits, WorkKind};
    use zeppelin_embed_cypher::StatementError;

    let (_directory, store) = fixture();
    // Disjoint top vector/text windows require widening beyond the initial
    // 50 candidates. A 400-row corpus fits after at most four rounds.
    let documents = (0..400)
        .map(|row| {
            IngestDocument::new(
                DocumentVersion::new(DocId::new(row + 1), Revision::new(1)),
                vec![1.0, row as f32 * 0.0025],
            )
            .with_text(if row < 200 {
                "copper".to_owned()
            } else {
                vec!["zeppelin"; 1 + (400 - row as usize) % 5].join(" ")
            })
        })
        .collect();
    store
        .ingest(IngestBatch::new(documents).with_epoch(store.epoch_identity().unwrap()))
        .unwrap();
    store.enable_graph().unwrap();
    let source =
        "CALL ze.hybrid_search([1,0],'zeppelin',1,'exact') YIELD node,score RETURN node,score";
    let options = |rounds| {
        GraphQueryOptions::default()
            .with_search_options(SearchOptions {
                max_rounds: Some(rounds),
                ..Default::default()
            })
            .unwrap()
    };
    for sealed in [false, true] {
        if sealed {
            store.seal().unwrap();
        }
        // Warm both producers before comparing deterministic work receipts.
        run(&store, source);
        let finite = execute(
            &store,
            &control(),
            &options(4),
            source,
            &[],
            Default::default(),
        )
        .unwrap();
        let huge = execute(
            &store,
            &control(),
            &options(u64::MAX),
            source,
            &[],
            Default::default(),
        )
        .unwrap();
        assert_eq!(ids(&finite), ids(&huge));
        assert_eq!(finite.cell(0, 1), huge.cell(0, 1));
        assert_eq!(finite.metadata().counters, huge.metadata().counters);
        let work = huge.pools().reports[0].work;
        let coordinates = work.get(WorkKind::VectorCoordinates);
        assert!(
            coordinates > 800,
            "the fixture must require multiple scoring rounds"
        );
        assert!(
            coordinates <= 6_402,
            "at most four 400-row vector passes and four cross-fill passes: {coordinates}"
        );
        assert!(work.get(WorkKind::CandidateWindowPeak) <= 400);
        assert!(work.get(WorkKind::CandidateWindowPeak) > 50);
        for rounds in [4, u64::MAX] {
            let capped = options(rounds)
                .with_limits(
                    24 * 1024 * 1024,
                    RuntimeLimits::default()
                        .with_limit(WorkKind::VectorCoordinates, coordinates - 1)
                        .unwrap(),
                )
                .unwrap();
            let Err(StatementError::Query(error)) =
                execute(&store, &control(), &capped, source, &[], Default::default())
            else {
                panic!("hybrid work cap must refuse the complete result");
            };
            assert_eq!(error.kind(), GraphQueryErrorKind::Limit);
            assert!(error.nothing_committed());
        }
        println!(
            "ZE400 sealed={sealed} max_rounds=4/u64::MAX coordinates={coordinates} candidate_peak={}",
            work.get(WorkKind::CandidateWindowPeak)
        );
    }
}

// The scan seam must consume each document a bounded number of times even
// though the Cypher pull operator requests one node at a time.
#[test]
fn document_folder_label_scan_has_linear_work() {
    let small = document_folder_scan_work(5_000);
    let doubled = document_folder_scan_work(10_000);
    assert!(
        doubled <= 2 * small + 2,
        "doubling documents must double visits, plus at most one extra two-endpoint note"
    );
    document_folder_scan_work(151_000);
}

fn document_folder_scan_work(n: u128) -> u64 {
    use zeppelin_embed::meta::{ColumnDefinition, ColumnId, ColumnType, PredicateValue, Schema};
    use zeppelin_embed::property_graph::query::plan::ParameterBinding;
    use zeppelin_embed::property_graph::query::runtime::{RuntimeLimits, WorkKind};
    use zeppelin_embed::property_graph::query::{QueryList, QueryValue, QueryView, ValueContext};
    use zeppelin_embed::property_graph::staging::{
        StructuredOperation, StructuredWrite, WriteImage,
    };
    use zeppelin_embed::property_graph::{
        ApplicationKey, EntityKind, GraphGeneration, GraphName, GraphRevision, NodeId, NodeRef,
        StoreInstanceId,
    };
    let directory = Directory::new();
    let schema = Schema::new(vec![ColumnDefinition::new(
        ColumnId::new(2),
        "folder",
        ColumnType::U64,
        false,
    )])
    .unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new()
            .with_schema(schema)
            .with_max_resident_bytes(128 * 1024 * 1024),
    )
    .unwrap();
    for start in (1..=n).step_by(1_000) {
        store
            .ingest(IngestBatch::new(
                (start..=(start + 999).min(n))
                    .map(|id| {
                        IngestDocument::new(
                            DocumentVersion::new(DocId::new(id), Revision::new(1)),
                            vec![1.0],
                        )
                        .with_timestamp(-(id as i64)) // Physical row order differs from identity order.
                        .with_columns(vec![(
                            ColumnId::new(2),
                            PredicateValue::U64(((id - 1) / 302) as u64 % 10),
                        )])
                    })
                    .collect(),
            ))
            .unwrap();
        if start % 30_000 == 1 && start > 1 {
            store.seal().unwrap();
        }
    }
    store.seal().unwrap();
    store.enable_graph().unwrap();
    let keys: Vec<_> = (0..n / 302).map(|note| format!("note-{note}")).collect();
    let writes: Vec<_> = keys
        .iter()
        .enumerate()
        .map(|(note, key)| StructuredWrite {
            key: ApplicationKey::new(EntityKind::Relationship, "ze401", key).unwrap(),
            revision: GraphRevision::new(1).unwrap(),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Relationship {
                relationship_type: GraphName::new("NOTE").unwrap(),
                properties: &[],
                source: NodeRef::Existing(NodeId::new(note as u128 * 302 + 301).unwrap()),
                target: NodeRef::Existing(NodeId::new(note as u128 * 302 + 302).unwrap()),
            }),
        })
        .collect();
    for batch in writes.chunks(50) {
        store.graph_apply(batch, &control()).unwrap();
    }
    store.close().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::read_only().with_max_resident_bytes(128 * 1024 * 1024),
    )
    .unwrap();
    let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
    let query_control = control();
    let mut values = ValueContext::new(&view, &query_control, 1_000_000).unwrap();
    let folders = [QueryValue::I64(0)];
    let parameters = [ParameterBinding {
        name: "folders",
        value: QueryValue::List(QueryList::new(&folders, &mut values).unwrap()),
    }];
    let options = GraphQueryOptions::default()
        .with_result_row_limit(65_536)
        .unwrap()
        .with_limits(
            24 * 1024 * 1024,
            RuntimeLimits::default()
                .with_limit(WorkKind::Scans, 4 * n as u64)
                .unwrap(),
        )
        .unwrap();
    let start = std::time::Instant::now();
    let result = execute(
        &store,
        &query_control,
        &options,
        "MATCH (d:Document) WHERE d.folder IN $folders RETURN ze.node_id(d) AS id ORDER BY id",
        &parameters,
        CompileLimits::default(),
    )
    .unwrap();
    let work = result.metadata().counters;
    eprintln!(
        "ZE401 n={n} rows={} elapsed={:?} scans={} lookups={} pages={}",
        result.metadata().rows,
        start.elapsed(),
        work.get(WorkKind::Scans),
        work.get(WorkKind::Lookups),
        work.get(WorkKind::DirectoryPagesDecoded)
    );
    let expected: Vec<_> = (1..=n).filter(|id| ((id - 1) / 302) % 10 == 0).collect();
    let actual: Vec<_> = (0..result.metadata().rows as usize)
        .map(|row| {
            let Some(Value::String(span)) = result.cell(row, 0) else {
                panic!("document id string");
            };
            u128::from_str_radix(result.string(*span).unwrap(), 16).unwrap()
        })
        .collect();
    assert_eq!(
        actual, expected,
        "complete ordered IDs, including adopted relationship endpoints"
    );
    assert!(work.get(WorkKind::Scans) <= 4 * n as u64);
    work.get(WorkKind::Scans)
}
