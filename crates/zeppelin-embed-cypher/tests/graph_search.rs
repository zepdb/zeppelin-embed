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

#[test]
fn ze402_text_eligible_collect_scales_and_matches_store() {
    eligible_collect_parity(false);
}

#[test]
fn ze402_hybrid_eligible_collect_scales_and_matches_store() {
    eligible_collect_parity(true);
}

fn eligible_collect_parity(hybrid: bool) {
    use zeppelin_embed::property_graph::query::QueryValue;
    use zeppelin_embed::property_graph::query::plan::ParameterBinding;
    use zeppelin_embed::property_graph::query::runtime::WorkKind;
    let mut previous = None;
    for n in [1_024_u128, 1_025, 10_000, 100_000] {
        let (_directory, store) = fixture();
        for start in (1..=n + 1).step_by(1_000) {
            store
                .ingest(
                    IngestBatch::new(
                        (start..=(start + 999).min(n + 1))
                            .map(|id| {
                                IngestDocument::new(
                                    DocumentVersion::new(DocId::new(id), Revision::new(1)),
                                    vec![1.0, id as f32 / n as f32],
                                )
                                .with_text(if id == n + 1 {
                                    "amber amber"
                                } else {
                                    "amber birch"
                                })
                            })
                            .collect(),
                    )
                    .with_epoch(store.epoch_identity().unwrap()),
                )
                .unwrap();
        }
        store.seal().unwrap();
        store.enable_graph().unwrap();
        let eligible: Vec<_> = (1..=n).map(DocId::new).collect();
        let term = zeppelin_embed::fts::search::TermQuery::flat(
            vec![b"amber".to_vec()],
            &[zeppelin_embed::fts::index::DEFAULT_FIELD],
        );
        let expected: Vec<_> = if hybrid {
            store
                .search_hybrid(
                    zeppelin_embed::ingest::SearchRequest::new(&[1.0, 0.0])
                        .with_eligible(&eligible),
                    &term,
                    &zeppelin_embed::fusion::HybridQuery::new(10),
                    zeppelin_embed::lifecycle::SearchOptions::default()
                        .with_tier(zeppelin_embed::lifecycle::SearchTier::Exact),
                    control(),
                )
                .unwrap()
                .hits
                .iter()
                .map(|hit| (hit.key.get(), hit.fused_score.to_bits()))
                .collect()
        } else {
            store
                .search_lexical_filtered(
                    &term,
                    10,
                    control(),
                    Some(&zeppelin_embed::lifecycle::QueryFilter::eligible(
                        &zeppelin_embed::meta::Schema::timestamp_only(),
                        &eligible,
                    )),
                )
                .unwrap()
                .candidates
                .iter()
                .map(|hit| (hit.document.doc_id().get(), hit.score.to_bits()))
                .collect()
        };
        let call = if hybrid {
            "ze.hybrid_search([1,0], $text, 10, 'exact', eligible)"
        } else {
            "ze.text_search($text, 10, eligible)"
        };
        let source = format!(
            "MATCH (d:Document) WHERE ze.node_id(d) <= $lastId WITH collect(DISTINCT d) AS eligible CALL {call} YIELD node, score RETURN ze.node_id(node) AS id, score"
        );
        let last_id = format!("{n:032x}");
        let parameters = [
            ParameterBinding {
                name: "lastId",
                value: QueryValue::String(&last_id),
            },
            ParameterBinding {
                name: "text",
                value: QueryValue::String("amber"),
            },
        ];
        // Warm retrieval so cache construction does not affect work comparisons.
        run(
            &store,
            if hybrid {
                "CALL ze.hybrid_search([1,0],'amber',10,'exact') YIELD node RETURN node"
            } else {
                "CALL ze.text_search('amber',10) YIELD node RETURN node"
            },
        );
        let result = execute(
            &store,
            &control(),
            &GraphQueryOptions::default(),
            &source,
            &parameters,
            CompileLimits::default(),
        )
        .unwrap_or_else(|error| panic!("ZE402 hybrid={hybrid} n={n}: {error}"));
        let actual: Vec<_> = (0..result.metadata().rows as usize)
            .map(|row| {
                let Some(Value::String(span)) = result.cell(row, 0) else {
                    panic!("id string")
                };
                let Some(Value::F64(score)) = result.cell(row, 1) else {
                    panic!("score")
                };
                (
                    u128::from_str_radix(result.string(*span).unwrap(), 16).unwrap(),
                    *score,
                )
            })
            .collect();
        assert_eq!(actual, expected, "Cypher/Store eligible parity");
        let work = result.metadata().counters;
        assert_eq!(work.get(WorkKind::EligibilityUniqueEntries), n as u64);
        assert!(result.metadata().peak_query_bytes <= 24 * 1024 * 1024);
        if n == 10_000 {
            use zeppelin_embed::property_graph::query::completed::GraphQueryErrorKind;
            use zeppelin_embed::property_graph::query::runtime::RuntimeLimits;
            for options in [
                GraphQueryOptions::default()
                    .with_limits(1024 * 1024, RuntimeLimits::default())
                    .unwrap(),
                GraphQueryOptions::default()
                    .with_limits(
                        24 * 1024 * 1024,
                        RuntimeLimits::default()
                            .with_limit(WorkKind::CopiedBytes, work.get(WorkKind::CopiedBytes) - 1)
                            .unwrap(),
                    )
                    .unwrap(),
            ] {
                let Err(zeppelin_embed_cypher::StatementError::Query(error)) = execute(
                    &store,
                    &control(),
                    &options,
                    &source,
                    &parameters,
                    CompileLimits::default(),
                ) else {
                    panic!("tightened query budgets must refuse the complete result")
                };
                assert_eq!(error.kind(), GraphQueryErrorKind::Limit);
                assert!(error.nothing_committed());
            }
        }
        for kind in [
            WorkKind::Scans,
            WorkKind::OperatorRows,
            WorkKind::HashProbes,
            WorkKind::CopiedBytes,
            WorkKind::EligibilityEntries,
        ] {
            let current = work.get(kind);
            if let Some((old_n, old_work)) = previous {
                let old_work: zeppelin_embed::property_graph::query::runtime::WorkCounters =
                    old_work;
                assert!(
                    current <= old_work.get(kind) * (n as u64).div_ceil(old_n) + 1024,
                    "linear {kind:?}: {current}"
                );
            }
        }
        eprintln!(
            "ZE402 hybrid={hybrid} n={n} rows={} peak_query_bytes={} work={work:?}",
            result.metadata().rows,
            result.metadata().peak_query_bytes
        );
        previous = Some((n as u64, work));
    }
}

mod ze404_id_lookup {
    use super::*;
    use zeppelin_embed::ingest::DeleteBatch;
    use zeppelin_embed::property_graph::query::QueryValue;
    use zeppelin_embed::property_graph::query::plan::ParameterBinding;
    use zeppelin_embed::property_graph::query::runtime::WorkKind;

    fn strings(result: &CompletedGraphResult) -> Vec<String> {
        (0..result.metadata().rows as usize)
            .map(|row| match result.cell(row, 0) {
                Some(Value::String(span)) => result.string(*span).unwrap().to_owned(),
                other => panic!("identity expected: {other:?}"),
            })
            .collect()
    }

    #[allow(
        clippy::result_large_err,
        reason = "preserve the typed statement error for parity assertions"
    )]
    fn query(
        store: &Store,
        text: &str,
        value: QueryValue<'_>,
    ) -> Result<CompletedGraphResult, zeppelin_embed_cypher::StatementError> {
        execute(
            store,
            &control(),
            &GraphQueryOptions::default(),
            text,
            &[ParameterBinding { name: "id", value }],
            CompileLimits::default(),
        )
    }

    #[test]
    fn ze404_id_equality_uses_unified_lookup() {
        let mut receipts = Vec::new();
        for n in [5_000_u128, 10_000] {
            let directory = Directory::new();
            let options = || OpenOptions::new().with_max_resident_bytes(128 * 1024 * 1024);
            let mut store = Store::open(directory.path(), options()).unwrap();
            let wide = (1_u128 << 100) + 7;
            let documents: Vec<_> = (0..=n)
                .chain([wide])
                .map(|id| {
                    IngestDocument::new(
                        DocumentVersion::new(DocId::new(id), Revision::new(1)),
                        vec![1.0],
                    )
                    .with_timestamp(-(id as i64))
                })
                .collect();
            store.ingest(IngestBatch::new(documents)).unwrap();
            store.enable_graph().unwrap();
            use zeppelin_embed::property_graph::staging::{
                StructuredOperation, StructuredWrite, WriteImage,
            };
            use zeppelin_embed::property_graph::{
                ApplicationKey, EntityKind, GraphName, GraphRevision, NodeId, NodeRef,
            };
            store
                .graph_apply(
                    &[StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Relationship, "ze405", "adopt")
                            .unwrap(),
                        revision: GraphRevision::new(1).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Relationship {
                            relationship_type: GraphName::new("LINK").unwrap(),
                            properties: &[],
                            source: NodeRef::Existing(NodeId::new(1).unwrap()),
                            target: NodeRef::Existing(NodeId::new(2).unwrap()),
                        }),
                    }],
                    &control(),
                )
                .unwrap();
            store.delete(DeleteBatch::new(vec![DocId::new(2)])).unwrap();
            run(&store, "CREATE (:Only)");
            store.delete(DeleteBatch::new(vec![DocId::new(n)])).unwrap();
            let only =
                strings(&run(&store, "MATCH (d:Only) RETURN ze.node_id(d) AS id"))[0].clone();
            let mut expected: Vec<_> = (0..n)
                .chain([wide])
                .filter(|id| *id != 2)
                .map(|id| format!("{id:032x}"))
                .collect();
            expected.push(only.clone());
            expected.sort();
            // Active probes prove correctness; sealed probes below prove bounded work.
            for reopened in [false, true] {
                if reopened {
                    store.close().unwrap();
                    store = Store::open(directory.path(), options()).unwrap();
                }
                let mut enumerated = strings(
                    &execute(
                        &store,
                        &control(),
                        &GraphQueryOptions::default()
                            .with_result_row_limit(65_536)
                            .unwrap(),
                        "MATCH (d) RETURN ze.node_id(d) AS id",
                        &[],
                        CompileLimits::default(),
                    )
                    .unwrap(),
                );
                enumerated.sort();
                assert_eq!(enumerated, expected);
                // HasLabel probes document_version and warms the identity index
                // independently of the point access path under measurement.
                run(
                    &store,
                    "MATCH (d:Document) RETURN ze.node_id(d) AS id LIMIT 1",
                );
                for id in [
                    format!("{:032x}", 0),
                    format!("{:032x}", 1),
                    format!("{:032x}", 3),
                    format!("{wide:032x}"),
                    format!("{:032x}", 2),
                    format!("{n:032x}"),
                    format!("{:032x}", n + 100),
                    only.clone(),
                ] {
                    let wanted: Vec<_> = expected
                        .iter()
                        .filter(|candidate| **candidate == id)
                        .cloned()
                        .collect();
                    let result = query(
                        &store,
                        "MATCH (d) WHERE ze.node_id(d) = $id RETURN ze.node_id(d) AS id LIMIT 1",
                        QueryValue::String(&id),
                    )
                    .unwrap();
                    assert_eq!(
                        strings(&result),
                        wanted,
                        "n={n} reopened={reopened} id={id}"
                    );
                    if reopened {
                        receipts.push((n, id, result.metadata().counters));
                    }
                }
                if !reopened {
                    store.seal().unwrap();
                }
            }
        }
        for (n, id, work) in &receipts {
            eprintln!(
                "ZE405 n={n} id={id} scans={} lookups={}",
                work.get(WorkKind::Scans),
                work.get(WorkKind::Lookups)
            );
        }
        for (_, _, work) in receipts {
            assert_eq!(
                work.get(WorkKind::Scans),
                0,
                "canonical ID must not enumerate documents"
            );
            assert!(
                work.get(WorkKind::Lookups) <= 8,
                "point work must not grow with documents"
            );
        }
    }

    #[test]
    fn ze404_id_equality_preserves_value_semantics() {
        let (_directory, store) = fixture();
        let id = (1_u128 << 100) + 0xab;
        store
            .ingest(
                IngestBatch::new(vec![IngestDocument::new(
                    DocumentVersion::new(DocId::new(id), Revision::new(1)),
                    vec![0.0, 0.0],
                )])
                .with_epoch(store.epoch_identity().unwrap()),
            )
            .unwrap();
        store.enable_graph().unwrap();
        let canonical = format!("{id:032x}");
        let upper = canonical.to_uppercase();
        for value in [
            QueryValue::String(&canonical),
            QueryValue::String(&upper),
            QueryValue::String("ab"),
            QueryValue::String("0000000000000000000000000000000g"),
            QueryValue::Null,
            QueryValue::I64(1),
        ] {
            for label in ["", ":Document", ":Missing"] {
                let point = format!(
                    "MATCH (d{label}) WHERE ze.node_id(d) = $id RETURN ze.node_id(d) AS id"
                );
                let scan = format!(
                    "MATCH (d{label}) WHERE ze.node_id(d) = $id AND true RETURN ze.node_id(d) AS id"
                );
                match (query(&store, &point, value), query(&store, &scan, value)) {
                    (Ok(a), Ok(b)) => assert_eq!(strings(&a), strings(&b), "{point} {value:?}"),
                    (Err(a), Err(b)) => assert_eq!(error_kind(a), error_kind(b)),
                    _ => panic!("point/scan mismatch: {point} {value:?}"),
                }
            }
        }
        let node = query(
            &store,
            "MATCH (d:Document) WHERE ze.node_id(d) = $id RETURN d",
            QueryValue::String(&canonical),
        )
        .unwrap();
        assert_eq!(ids(&node), [id]);
        for predicate in [
            "$id = ze.node_id(d)",
            "ze.node_id(d) = $id",
            &format!("ze.node_id(d) = '{canonical}'"),
        ] {
            let source =
                format!("MATCH (d:Document) WHERE {predicate} RETURN ze.node_id(d) AS id LIMIT 1");
            let result = if predicate.contains("$id") {
                query(&store, &source, QueryValue::String(&canonical)).unwrap()
            } else {
                run(&store, &source)
            };
            assert_eq!(
                strings(&result).as_slice(),
                std::slice::from_ref(&canonical)
            );
            assert_eq!(result.metadata().counters.get(WorkKind::Scans), 0);
        }
        assert!(
            matches!(execute(&store, &control(), &GraphQueryOptions::default(),
            "MATCH (d) WHERE ze.node_id(d) = $missing RETURN d", &[], CompileLimits::default()),
            Err(zeppelin_embed_cypher::StatementError::Compile(error)) if error.kind == zeppelin_embed_cypher::ErrorKind::Parameter)
        );
        // Excluded fallible constraints must still fail even when the ID is absent.
        for source in [
            "MATCH (d {ts: 1 / 0}) WHERE ze.node_id(d) = $id RETURN d",
            "MATCH (d) WHERE ze.node_id(d) = $id AND 1 / 0 = 1 RETURN d",
            "MATCH (d) WHERE ze.node_id(d) = [$id][1 / 0] RETURN d",
        ] {
            assert!(
                query(
                    &store,
                    source,
                    QueryValue::String("00000000000000000000000000000000")
                )
                .is_err(),
                "{source}"
            );
        }
    }

    fn error_kind(error: zeppelin_embed_cypher::StatementError) -> String {
        match error {
            zeppelin_embed_cypher::StatementError::Compile(error) => {
                format!("compile:{:?}", error.kind)
            }
            zeppelin_embed_cypher::StatementError::Query(error) => {
                format!("query:{:?}", error.kind())
            }
        }
    }
}

mod incident_sources {
    use super::*;
    use zeppelin_embed::property_graph::query::pattern_test_support::{
        observe_document_visits, with_original_node_sources,
    };
    use zeppelin_embed::property_graph::query::runtime::WorkKind;
    use zeppelin_embed::property_graph::staging::{
        StructuredOperation, StructuredWrite, WriteImage,
    };
    use zeppelin_embed::property_graph::{
        ApplicationKey, EntityKind, GraphName, GraphRevision, NodeId, NodeRef,
    };

    fn rows(result: &CompletedGraphResult) -> Vec<Vec<u128>> {
        (0..result.metadata().rows as usize)
            .map(|row| {
                (0..result.pools().columns.len())
                    .map(|column| match result.cell(row, column) {
                        Some(Value::Node(index)) => result.pools().nodes[*index as usize].id.get(),
                        Some(Value::Relationship(index)) => {
                            result.pools().relationships[*index as usize].id.get()
                        }
                        Some(Value::I64(value)) => *value as u128,
                        other => panic!("unexpected incident row: {other:?}"),
                    })
                    .collect()
            })
            .collect()
    }

    fn sparse(n: u128) -> Vec<[u64; 3]> {
        let directory = Directory::new();
        let store = Store::open(directory.path(), OpenOptions::new()).unwrap();
        store
            .ingest(IngestBatch::new(
                (1..=n)
                    .map(|id| {
                        IngestDocument::new(
                            DocumentVersion::new(DocId::new(id), Revision::new(1)),
                            vec![1.0],
                        )
                        .with_timestamp(-(id as i64))
                    })
                    .collect(),
            ))
            .unwrap();
        store.seal().unwrap();
        store.enable_graph().unwrap();
        let keys: Vec<_> = (0..500).map(|i| format!("edge-{i}")).collect();
        let writes: Vec<_> = keys
            .iter()
            .enumerate()
            .map(|(i, key)| StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "ze404-r2", key).unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    relationship_type: GraphName::new("PERF_LINK").unwrap(),
                    properties: &[],
                    // Deliberately create in reverse source order.
                    source: NodeRef::Existing(NodeId::new(999 - 2 * i as u128).unwrap()),
                    target: NodeRef::Existing(NodeId::new(1000 - 2 * i as u128).unwrap()),
                }),
            })
            .collect();
        for batch in writes.chunks(50) {
            store.graph_apply(batch, &control()).unwrap();
        }
        store.close().unwrap();
        let store = Store::open(directory.path(), OpenOptions::read_only()).unwrap();
        let queries = [
            "MATCH ()-[r]->() RETURN count(r)",
            "MATCH ()-[r:PERF_LINK]->() RETURN count(r)",
            "MATCH (a)-[:PERF_LINK]->(b) RETURN a, b",
            "MATCH (a)-[:PERF_LINK]->(b) RETURN a, b LIMIT 10",
            "MATCH (a)-[:PERF_LINK]->(b)-[:PERF_LINK]->(c) RETURN count(c)",
        ];
        let expected_pairs: Vec<_> = (0..500).map(|i| vec![2 * i + 1, 2 * i + 2]).collect();
        let mut counters = Vec::new();
        for (i, query) in queries.iter().enumerate() {
            let (result, document_visits) = observe_document_visits(|| run(&store, query));
            let expected = match i {
                0 | 1 => vec![vec![500]],
                2 => expected_pairs.clone(),
                3 => expected_pairs[..10].to_vec(),
                _ => vec![vec![0]],
            };
            assert_eq!(rows(&result), expected, "{query}");
            let work = result.metadata().counters;
            let counts = [
                work.get(WorkKind::Scans),
                work.get(WorkKind::Lookups),
                work.get(WorkKind::DirectoryPagesDecoded),
            ];
            eprintln!("R2 n={n} query={i} document_visits={document_visits} work={counts:?}");
            counters.push(counts);
            assert_eq!(
                document_visits, 0,
                "relationship anchors must never visit documents: {query}"
            );
        }
        counters
    }

    #[test]
    fn ze404_sparse_outgoing_patterns_ignore_isolated_documents() {
        assert_eq!(
            sparse(2_000),
            sparse(4_000),
            "isolated documents add no source work"
        );
    }

    #[test]
    fn ze404_incident_source_preserves_expand() {
        let directory = Directory::new();
        let store = Store::open(directory.path(), OpenOptions::new()).unwrap();
        store
            .ingest(IngestBatch::new(
                (0..32)
                    .chain([u128::MAX])
                    .map(|id| {
                        IngestDocument::new(
                            DocumentVersion::new(DocId::new(id), Revision::new(1)),
                            vec![1.0],
                        )
                    })
                    .collect(),
            ))
            .unwrap();
        store.seal().unwrap();
        store.enable_graph().unwrap();
        // Parallel edges, a cycle, a self-loop, two types/ranges at source 2,
        // document zero and the full-width final source. RelId order deliberately
        // disagrees with source order.
        let edges = [
            (2, 3, "S"),
            (1, 2, "R"),
            (1, 2, "R"),
            (2, 1, "R"),
            (2, 2, "R"),
            (u128::MAX, 1, "R"),
            (0, 1, "R"),
            (3, 4, "R"),
            (4, 1, "R"),
        ];
        let keys: Vec<_> = (0..edges.len()).map(|i| format!("tiny-{i}")).collect();
        let writes: Vec<_> = edges
            .iter()
            .zip(&keys)
            .map(|(&(source, target, kind), key)| StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "ze404-r2-tiny", key).unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    relationship_type: GraphName::new(kind).unwrap(),
                    properties: &[],
                    source: NodeRef::Existing(NodeId::from(DocId::new(source))),
                    target: NodeRef::Existing(NodeId::from(DocId::new(target))),
                }),
            })
            .collect();
        let receipt = store.graph_apply(&writes, &control()).unwrap();
        let rels: Vec<_> = receipt
            .receipts()
            .iter()
            .map(|receipt| match receipt.entity {
                zeppelin_embed::property_graph::EntityId::Relationship(id) => id.get(),
                _ => panic!("relationship receipt"),
            })
            .collect();
        // Deletes retain range candidates whose source/target is now tombstoned.
        run(
            &store,
            "MATCH (n) WHERE ze.node_id(n) = '00000000000000000000000000000004' DETACH DELETE n",
        );
        store.close().unwrap();
        let store = Store::open(directory.path(), OpenOptions::read_only()).unwrap();
        let expected: Vec<_> = [6, 1, 2, 3, 4, 0, 5]
            .into_iter()
            .map(|i| vec![edges[i].0, rels[i], edges[i].1])
            .collect();
        let query = "MATCH (a)-[r]->(b) RETURN a, r, b";
        let (result, visits) = observe_document_visits(|| run(&store, query));
        let (original, original_visits) =
            observe_document_visits(|| with_original_node_sources(|| run(&store, query)));
        assert_eq!(rows(&result), expected);
        assert_eq!(rows(&original), expected);
        for limit in [0, 1, 3, 7, 8] {
            let query = format!("{query} LIMIT {limit}");
            let (result, limit_visits) = observe_document_visits(|| run(&store, &query));
            assert_eq!(rows(&result), expected[..limit.min(expected.len())]);
            assert_eq!(
                rows(&result),
                rows(&with_original_node_sources(|| run(&store, &query)))
            );
            if limit == 0 {
                assert_eq!(limit_visits, 0);
            }
            if limit == 0 {
                assert_eq!(result.metadata().counters.get(WorkKind::Scans), 0);
            }
        }
        // Independently specified two-hop RelId pairs in source/Expand order.
        let paths = [
            (6, 1),
            (6, 2),
            (1, 3),
            (1, 4),
            (2, 3),
            (2, 4),
            (3, 1),
            (3, 2),
            (4, 3),
            (5, 1),
            (5, 2),
        ];
        let expected_paths: Vec<_> = paths
            .into_iter()
            .map(|(a, b)| vec![rels[a], rels[b]])
            .collect();
        let path_query = "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r, s";
        let result = run(&store, path_query);
        assert_eq!(rows(&result), expected_paths);
        assert_eq!(
            rows(&with_original_node_sources(|| run(&store, path_query))),
            expected_paths
        );
        assert!(rows(&result).iter().all(|row| row[0] != row[1]));
        for query in [
            "MATCH (a)-[r:ABSENT]->(b) RETURN a, r, b",
            "MATCH (a)-[r:S]->(b) RETURN a, r, b",
            "MATCH (a)<-[r:R]-(b) RETURN a, r, b",
            "MATCH (a)-[r:R]-(b) RETURN a, r, b",
            "MATCH (a)-[r:R*1..2]->(b) RETURN count(b)",
            "OPTIONAL MATCH (a)-[r:R]->(b) RETURN count(b)",
            "MATCH (a) OPTIONAL MATCH (a)-[r:R]->(b) RETURN count(b)",
            "MATCH (a:Document)-[r:R]->(b) RETURN count(b)",
        ] {
            let actual = run(&store, query);
            assert_eq!(
                rows(&actual),
                rows(&with_original_node_sources(|| run(&store, query))),
                "{query}"
            );
        }
        eprintln!("R2 tiny document_visits={visits} original_document_visits={original_visits}");
        assert_eq!(visits, 0);
        assert!(original_visits > 0);
    }
}

mod ze404_counts {
    use super::*;
    use zeppelin_embed::property_graph::query::runtime::{WorkCounters, WorkKind};
    use zeppelin_embed::property_graph::staging::{
        StructuredOperation, StructuredWrite, WriteImage,
    };
    use zeppelin_embed::property_graph::{
        ApplicationKey, EntityKind, GraphName, GraphRevision, NodeId, NodeRef,
    };

    fn fixture() -> (Directory, Store) {
        let directory = Directory::new();
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(128 * 1024 * 1024),
        )
        .unwrap();
        (directory, store)
    }

    fn assert_count(store: &Store, query: &str, expected: i64) -> WorkCounters {
        let result = run(store, query);
        assert_eq!(result.metadata().rows, 1, "{query}");
        assert_eq!(result.cell(0, 0), Some(&Value::I64(expected)), "{query}");
        result.metadata().counters
    }

    fn documents(store: &Store, start: u128, end: u128, revision: u64) {
        store
            .ingest(IngestBatch::new(
                (start..end)
                    .map(|id| {
                        IngestDocument::new(
                            DocumentVersion::new(DocId::new(id), Revision::new(revision)),
                            vec![1.0, 0.0],
                        )
                        .with_timestamp(-(id as i64))
                    })
                    .collect(),
            ))
            .unwrap();
    }

    #[test]
    fn ze404_global_counts_use_document_cardinality() {
        let mut previous = None;
        for n in [64, 128] {
            let (directory, store) = fixture();
            store.enable_graph().unwrap();
            assert_count(&store, "MATCH (n) RETURN count(n)", 0);
            assert_count(&store, "MATCH (n:Document) RETURN count(*)", 0);
            documents(&store, 0, n, 1);
            assert_count(&store, "MATCH (n) RETURN count(n)", n as i64);
            store.seal().unwrap();
            // Adoption, graph-only nodes (one explicitly Document), one edge,
            // and a graph tombstone shadowing a still-live document.
            store
                .graph_apply(
                    &[StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Relationship, "ze407", "adopt")
                            .unwrap(),
                        revision: GraphRevision::new(1).unwrap(),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Relationship {
                            relationship_type: GraphName::new("ADOPT").unwrap(),
                            properties: &[],
                            source: NodeRef::Existing(NodeId::new(1).unwrap()),
                            target: NodeRef::Existing(NodeId::new(3).unwrap()),
                        }),
                    }],
                    &control(),
                )
                .unwrap();
            run(
                &store,
                "CREATE (a:Other {p: 1}), (b:Document), (a)-[:LINK]->(b)",
            );
            run(
                &store,
                "MATCH (n) WHERE ze.node_id(n) = '00000000000000000000000000000001' DETACH DELETE n",
            );
            let expected = n as i64 + 1;
            let mut all = ids(&run(&store, "MATCH (n) RETURN n"));
            all.sort_unstable();
            assert_eq!(
                all,
                (0..n)
                    .filter(|id| *id != 1)
                    .chain([n, n + 1])
                    .collect::<Vec<_>>()
            );
            let mut labelled = ids(&run(&store, "MATCH (n:Document) RETURN n"));
            labelled.sort_unstable();
            assert_eq!(
                labelled,
                (0..n)
                    .filter(|id| *id != 1)
                    .chain([n + 1])
                    .collect::<Vec<_>>()
            );
            for query in [
                "MATCH (n) RETURN count(n)",
                "MATCH (n) RETURN count(*)",
                "MATCH (n:Document) RETURN count(n)",
                "MATCH (n:Document) RETURN count(*)",
            ] {
                let count = if query.contains(":Document") {
                    n as i64
                } else {
                    expected
                };
                let work = assert_count(&store, query, count);
                eprintln!("ZE407 n={n} {query}: {work:?}");
                assert!(
                    work.get(WorkKind::Scans) < 32,
                    "implicit documents must not be visited: {work:?}"
                );
                assert!(
                    work.get(WorkKind::RowsIn) < 8,
                    "no per-document aggregate input: {work:?}"
                );
                if query == "MATCH (n) RETURN count(n)" {
                    if let Some(old) = previous {
                        assert_eq!(
                            work, old,
                            "fixed graph states and segments must have fixed work"
                        );
                    }
                    previous = Some(work);
                }
            }
            assert_count(&store, "MATCH ()-[r]->() RETURN count(r)", 1);
            assert_count(&store, "MATCH ()-[r:LINK]->() RETURN count(*)", 1);
            assert_count(&store, "MATCH (n) RETURN count(n.p)", 1);
            assert_count(&store, "MATCH (n) RETURN count(DISTINCT n)", expected);
            assert_count(&store, "MATCH (n) WHERE n.p = 1 RETURN count(n)", 1);
            assert_count(&store, "OPTIONAL MATCH (n:Missing) RETURN count(n)", 0);
            assert_count(&store, "OPTIONAL MATCH (n:Missing) RETURN count(*)", 1);
            let grouped = run(&store, "MATCH (n) RETURN n.p, count(n) ORDER BY n.p");
            assert_eq!(grouped.metadata().rows, 2);
            assert_eq!(grouped.cell(0, 1), Some(&Value::I64(1)));
            assert_eq!(grouped.cell(1, 1), Some(&Value::I64(expected - 1)));
            assert_count(&store, "MATCH (n) RETURN count(n) LIMIT 1", expected);
            assert_eq!(
                run(&store, "MATCH (n) RETURN count(n) LIMIT 0")
                    .metadata()
                    .rows,
                0
            );
            store
                .delete(zeppelin_embed::ingest::DeleteBatch::new(vec![DocId::new(
                    2,
                )]))
                .unwrap();
            assert_count(&store, "MATCH (n:Document) RETURN count(n)", n as i64 - 1);
            documents(&store, 2, 3, 2);
            assert_count(&store, "MATCH (n:Document) RETURN count(n)", n as i64);
            drop(store);
            let reopened = Store::open(
                directory.path(),
                OpenOptions::new().with_max_resident_bytes(128 * 1024 * 1024),
            )
            .unwrap();
            assert_count(&reopened, "MATCH (n) RETURN count(n)", expected);
            assert_count(&reopened, "MATCH (n:Document) RETURN count(n)", n as i64);
            // Pin the existing staged-write visibility, independently checked
            // with the baseline addon: this shape must not use the read count.
            let work = assert_count(
                &reopened,
                "MATCH (n) WITH count(n) AS c CREATE (:After) RETURN c",
                expected + 1,
            );
            assert!(
                work.get(WorkKind::RowsIn) >= n as u64,
                "writes must keep the ordinary aggregate: {work:?}"
            );
        }
    }
}

mod ze408 {
    use super::*;
    use zeppelin_embed::meta::{ColumnDefinition, ColumnId, ColumnType, PredicateValue, Schema};
    use zeppelin_embed::property_graph::query::runtime::WorkKind;

    fn folder_store(n: u128, matches: u128) -> (Directory, Store) {
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
        for start in (0..n).step_by(1_000) {
            store
                .ingest(IngestBatch::new(
                    (start..(start + 1_000).min(n))
                        .map(|id| {
                            IngestDocument::new(
                                DocumentVersion::new(DocId::new(id), Revision::new(1)),
                                vec![1.0],
                            )
                            .with_timestamp(-(id as i64))
                            .with_columns(vec![(
                                ColumnId::new(2),
                                PredicateValue::U64(if n == 150_000 {
                                    (id % 10) as u64
                                } else if id < matches {
                                    3
                                } else {
                                    4
                                }),
                            )])
                        })
                        .collect(),
                ))
                .unwrap();
            if n == 150_000 && (start + 1_000) % 5_000 == 0 {
                store.seal().unwrap();
            }
        }
        store.seal().unwrap();
        store.enable_graph().unwrap();
        (directory, store)
    }

    fn large_run(store: &Store, source: &str) -> CompletedGraphResult {
        execute(
            store,
            &control(),
            &GraphQueryOptions::default()
                .with_result_row_limit(65_536)
                .unwrap(),
            source,
            &[],
            Default::default(),
        )
        .unwrap()
    }

    fn string_ids(result: &CompletedGraphResult) -> Vec<u128> {
        (0..result.metadata().rows as usize)
            .map(|row| {
                let Some(Value::String(span)) = result.cell(row, 0) else {
                    panic!("id string");
                };
                u128::from_str_radix(result.string(*span).unwrap(), 16).unwrap()
            })
            .collect()
    }

    #[test]
    fn ze404_folder_count_uses_metadata_candidates() {
        let mut previous = None;
        let mut previous_rows = None;
        for (n, matches) in [(1_500, 15), (15_000, 15), (150_000, 15_000)] {
            let (_directory, store) = folder_store(n, matches);
            let expected = large_run(
                &store,
                "MATCH (d:Document) WHERE d.folder IN [3] RETURN ze.node_id(d) AS id",
            );
            let actual = large_run(
                &store,
                "MATCH (d:Document) WHERE d.folder = 3 RETURN ze.node_id(d) AS id",
            );
            assert_eq!(string_ids(&actual), string_ids(&expected));
            let row_work = actual.metadata().counters;
            if let Some((old_matches, old)) = previous_rows {
                let old: zeppelin_embed::property_graph::query::runtime::WorkCounters = old;
                if matches == old_matches {
                    for kind in [
                        WorkKind::Lookups,
                        WorkKind::Expressions,
                        WorkKind::OperatorRows,
                    ] {
                        assert_eq!(
                            row_work.get(kind),
                            old.get(kind),
                            "nonmatching documents must not add row graph work: {kind:?}"
                        );
                    }
                }
            }
            previous_rows = Some((matches, row_work));
            eprintln!("ZE408 n={n} matches={matches} row_work={row_work:?}");

            let limited = large_run(
                &store,
                "MATCH (d:Document) WHERE d.folder = 3 RETURN ze.node_id(d) AS id LIMIT 10",
            );
            assert_eq!(string_ids(&limited), string_ids(&expected)[..10]);
            let count = run(
                &store,
                "MATCH (d:Document) WHERE d.folder = 3 RETURN count(d) AS c",
            );
            assert_eq!(count.cell(0, 0), Some(&Value::I64(matches as i64)));
            let work = count.metadata().counters;
            eprintln!("ZE408 n={n} matches={matches} count_work={work:?}");
            if let Some(old) = previous {
                let old: zeppelin_embed::property_graph::query::runtime::WorkCounters = old;
                for kind in [
                    WorkKind::Lookups,
                    WorkKind::Expressions,
                    WorkKind::OperatorRows,
                ] {
                    assert_eq!(
                        work.get(kind),
                        old.get(kind),
                        "nonmatching documents must not add graph work: {kind:?}"
                    );
                }
            }
            previous = Some(work);
        }
    }
    fn parity(store: &Store) -> usize {
        let expected = run(store, "MATCH (d:Document) WHERE d.folder IN [3] RETURN d");
        let actual = run(store, "MATCH (d:Document) WHERE d.folder = 3 RETURN d");
        assert_eq!(ids(&actual), ids(&expected));
        let limit = run(
            store,
            "MATCH (d:Document) WHERE d.folder = 3 RETURN d LIMIT 2",
        );
        assert_eq!(
            ids(&limit),
            ids(&expected).into_iter().take(2).collect::<Vec<_>>()
        );
        let count = run(
            store,
            "MATCH (d:Document) WHERE d.folder = 3 RETURN count(d) AS c",
        );
        assert_eq!(
            count.cell(0, 0),
            Some(&Value::I64(expected.metadata().rows as i64))
        );
        expected.metadata().rows as usize
    }

    #[test]
    fn folder_candidates_preserve_graph_precedence_visibility_and_reopen() {
        let (directory, store) = folder_store(12, 4);
        store
            .ingest(IngestBatch::new(vec![
                IngestDocument::new(
                    DocumentVersion::new(DocId::new(20), Revision::new(1)),
                    vec![1.0],
                )
                .with_columns(vec![(ColumnId::new(2), PredicateValue::U64(3))]),
            ]))
            .unwrap();
        run(
            &store,
            "MATCH (d:Document) WHERE ze.node_id(d) = '00000000000000000000000000000000' SET d.folder = 99",
        );
        run(
            &store,
            "MATCH (d:Document) WHERE ze.node_id(d) = '00000000000000000000000000000001' SET d.extra = 1",
        );
        run(
            &store,
            "MATCH (d:Document) WHERE ze.node_id(d) = '00000000000000000000000000000001' DELETE d",
        );
        run(&store, "CREATE (d:Document {folder: 3})");
        run(&store, "CREATE (d:Other {folder: 3})");
        assert_eq!(parity(&store), 5);
        let all = run(&store, "MATCH (d:Document) WHERE d.folder = 3 RETURN d");
        assert_eq!(
            ids(&all).first(),
            Some(&0),
            "adopted graph records remain first"
        );
        store.seal().unwrap();
        assert_eq!(parity(&store), 5);
        store.close().unwrap();
        let reopened = Store::open(
            directory.path(),
            OpenOptions::read_only().with_max_resident_bytes(128 * 1024 * 1024),
        )
        .unwrap();
        assert_eq!(parity(&reopened), 5);
    }

    #[test]
    fn unsupported_folder_types_nulls_and_missing_values_keep_scan_semantics() {
        for (column_type, nullable) in [
            (ColumnType::U64, true),
            (ColumnType::I64, false),
            (ColumnType::RawString, false),
        ] {
            let directory = Directory::new();
            let schema = Schema::new(vec![ColumnDefinition::new(
                ColumnId::new(2),
                "folder",
                column_type,
                nullable,
            )])
            .unwrap();
            let store =
                Store::open(directory.path(), OpenOptions::new().with_schema(schema)).unwrap();
            let columns = if nullable {
                vec![]
            } else {
                vec![(
                    ColumnId::new(2),
                    match column_type {
                        ColumnType::I64 => PredicateValue::I64(3),
                        _ => PredicateValue::String("3".into()),
                    },
                )]
            };
            store
                .ingest(IngestBatch::new(vec![
                    IngestDocument::new(
                        DocumentVersion::new(DocId::new(0), Revision::new(1)),
                        vec![1.0],
                    )
                    .with_columns(columns),
                ]))
                .unwrap();
            store.enable_graph().unwrap();
            run(&store, "MATCH (d:Document) SET d.folder = 3");
            run(&store, "CREATE (d:Document {folder: 3})");
            assert_eq!(
                parity(&store),
                if column_type == ColumnType::RawString {
                    1
                } else {
                    2
                }
            );
            store.seal().unwrap();
            parity(&store);
        }
        let (_directory, store) = fixture();
        store
            .ingest(
                IngestBatch::new(vec![IngestDocument::new(
                    DocumentVersion::new(DocId::new(0), Revision::new(1)),
                    vec![1.0, 0.0],
                )])
                .with_epoch(store.epoch_identity().unwrap()),
            )
            .unwrap();
        store.enable_graph().unwrap();
        run(&store, "MATCH (d:Document) SET d.folder = 3");
        assert_eq!(parity(&store), 1);
    }

    #[test]
    fn oversized_folder_keeps_error_and_successful_limit_prefix() {
        let directory = Directory::new();
        let schema = Schema::new(vec![ColumnDefinition::new(
            ColumnId::new(2),
            "folder",
            ColumnType::U64,
            false,
        )])
        .unwrap();
        let store = Store::open(directory.path(), OpenOptions::new().with_schema(schema)).unwrap();
        store
            .ingest(IngestBatch::new(
                (0..2)
                    .map(|id| {
                        IngestDocument::new(
                            DocumentVersion::new(DocId::new(id), Revision::new(1)),
                            vec![1.0],
                        )
                        .with_columns(vec![(
                            ColumnId::new(2),
                            PredicateValue::U64(if id == 0 { 3 } else { u64::MAX }),
                        )])
                    })
                    .collect(),
            ))
            .unwrap();
        store.enable_graph().unwrap();
        for sealed in [false, true] {
            if sealed {
                store.seal().unwrap();
            }
            let limited = run(
                &store,
                "MATCH (d:Document) WHERE d.folder = 3 RETURN d LIMIT 1",
            );
            assert_eq!(ids(&limited), [0]);
            for predicate in ["d.folder = 3", "d.folder IN [3]"] {
                let source = format!("MATCH (d:Document) WHERE {predicate} RETURN count(d)");
                let Err(error) = execute(
                    &store,
                    &control(),
                    &GraphQueryOptions::default(),
                    &source,
                    &[],
                    Default::default(),
                ) else {
                    panic!("oversized folder must fail");
                };
                assert!(
                    format!("{error:?}").contains("document u64 property exceeds graph integer")
                );
            }
        }
    }

    #[test]
    fn folder_candidates_preserve_parameters_exclusions_and_work_limits() {
        use zeppelin_embed::property_graph::query::QueryValue;
        use zeppelin_embed::property_graph::query::completed::GraphQueryErrorKind;
        use zeppelin_embed::property_graph::query::plan::ParameterBinding;
        use zeppelin_embed::property_graph::query::runtime::RuntimeLimits;
        let (_directory, store) = folder_store(150, 15);
        let source = "MATCH (d:Document) WHERE 3 = d.folder RETURN count(d) AS c";
        assert_eq!(run(&store, source).cell(0, 0), Some(&Value::I64(15)));
        for value in [
            QueryValue::I64(3),
            QueryValue::I64(-1),
            QueryValue::Null,
            QueryValue::String("3"),
        ] {
            let bindings = [ParameterBinding {
                name: "folder",
                value,
            }];
            let query = |predicate| {
                execute(
                    &store,
                    &control(),
                    &GraphQueryOptions::default(),
                    &format!("MATCH (d:Document) WHERE {predicate} RETURN d"),
                    &bindings,
                    Default::default(),
                )
                .unwrap()
            };
            assert_eq!(
                ids(&query("d.folder = $folder")),
                ids(&query("d.folder IN [$folder]"))
            );
        }
        // Compound/fallible predicates and a LIMIT below count stay on the scan.
        assert_eq!(
            run(
                &store,
                "MATCH (d:Document) WHERE d.folder = 3 WITH d LIMIT 2 RETURN count(d)"
            )
            .cell(0, 0),
            Some(&Value::I64(2))
        );
        assert_eq!(
            run(
                &store,
                "MATCH (d:Document) WHERE d.folder = 3 AND d.folder = 4 RETURN count(d)"
            )
            .cell(0, 0),
            Some(&Value::I64(0))
        );
        let counted = run(&store, source);
        let scans = counted.metadata().counters.get(WorkKind::Scans);
        let options = GraphQueryOptions::default()
            .with_limits(
                24 * 1024 * 1024,
                RuntimeLimits::default()
                    .with_limit(WorkKind::Scans, scans - 1)
                    .unwrap(),
            )
            .unwrap();
        let Err(zeppelin_embed_cypher::StatementError::Query(error)) = execute(
            &store,
            &control(),
            &options,
            source,
            &[],
            Default::default(),
        ) else {
            panic!("bitmap work cap must fail");
        };
        assert_eq!(error.kind(), GraphQueryErrorKind::Limit);
        assert!(error.nothing_committed());
    }
    #[test]
    fn graph_only_document_folder_keeps_the_original_scan() {
        let directory = Directory::new();
        let store = Store::create_graph(directory.path(), OpenOptions::new(), None).unwrap();
        run(&store, "CREATE (d:Document {folder: 3})");
        assert_eq!(parity(&store), 1);
    }
}
