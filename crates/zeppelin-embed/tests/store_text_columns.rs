#![allow(clippy::expect_used)]

use tempfile::tempdir;
use zeppelin_embed::format::golden::decode_hex;
use zeppelin_embed::fts::index::DEFAULT_FIELD;
use zeppelin_embed::fts::query::{LexicalMatchKind, LexicalQuery};
use zeppelin_embed::fts::search::TermQuery;
use zeppelin_embed::fusion::{FusionError, FusionLeg, HYBRID_WINDOW_FLOOR, HybridQuery};
use zeppelin_embed::ingest::SearchRequest;
use zeppelin_embed::ingest::wal_payload::UPSERT_V2;
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, IngestError, Revision,
};
use zeppelin_embed::lifecycle::SearchOptions;
use zeppelin_embed::lifecycle::{
    CancelToken, GraphSearchOptions, HybridLegTestFault, OpenOptions, QueryControl, SearchTier,
    Store, StoreTestDependencies, SystemMonotonicClock,
};
use zeppelin_embed::meta::{
    BuildError, ColumnDefinition, ColumnId, ColumnType, Predicate, PredicateValue, Schema,
};
use zeppelin_embed::segment::SegmentError;
use zeppelin_embed::segment::layout::RegionKind;
use zeppelin_embed::segment::reader::SegmentReader;
use zeppelin_embed::vfs::StdVfs;
use zeppelin_embed::wal::WalReader;

#[test]
fn text_ingested_through_the_store_is_searchable_after_reopen() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(11), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_text("bronze zeppelin"),
            IngestDocument::new(
                DocumentVersion::new(DocId::new(22), Revision::new(1)),
                vec![0.0, 1.0],
            )
            .with_text("silver airship"),
        ]))
        .expect("ingest text");
    let wal = WalReader::open(&StdVfs, &directory.path().join("wal.ze")).expect("open emitted WAL");
    assert!(wal.records().iter().all(|record| record.op == UPSERT_V2));
    let active_outcome = store
        .search_lexical(
            &TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]),
            10,
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search active text");
    assert_eq!(
        active_outcome.candidates[0].document.doc_id(),
        DocId::new(11)
    );
    store.seal().expect("seal text");
    store.close().expect("close writer");

    let reopened = Store::open(directory.path(), OpenOptions::default()).expect("reopen store");
    let outcome = reopened
        .search_lexical(
            &TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]),
            10,
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search persisted text");
    assert_eq!(outcome.candidates.len(), 1);
    assert_eq!(outcome.candidates[0].document.doc_id(), DocId::new(11));
    assert!(outcome.candidates[0].score > 0.0);
    assert!(outcome.diagnostics.tokenizer_epoch.is_some());
    assert!(outcome.diagnostics.counters.lexical.docs_evaluated > 0);
    reopened.close().expect("close reader");
}

#[test]
fn stored_text_region_preserves_presence_and_utf8_after_seal() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(101), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_text("Café 🚀 zeppelin"),
            IngestDocument::new(
                DocumentVersion::new(DocId::new(102), Revision::new(1)),
                vec![0.0, 1.0],
            ),
            IngestDocument::new(
                DocumentVersion::new(DocId::new(103), Revision::new(1)),
                vec![0.5, 0.5],
            )
            .with_text(""),
        ]))
        .expect("ingest mixed text presence");
    store.seal().expect("seal stored text");
    let snapshot = store.snapshot().expect("pin snapshot");
    let rows = snapshot.segments()[0]
        .stored_text()
        .expect("validate stored text")
        .expect("stored text region");
    assert_eq!(rows.row_count(), 3);
    assert_eq!(rows.row(0), Some(Some("Café 🚀 zeppelin")));
    assert_eq!(rows.row(1), Some(None));
    assert_eq!(rows.row(2), Some(Some("")));
    drop(snapshot);
    store.close().expect("close store");
}

#[test]
fn stored_text_returns_the_exact_active_and_sealed_document_versions() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    let sealed = DocumentVersion::new(DocId::new(201), Revision::new(3));
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(sealed, vec![1.0, 0.0]).with_text("sealed text"),
        ]))
        .expect("ingest sealed row");
    store.seal().expect("seal row");
    let active = DocumentVersion::new(DocId::new(202), Revision::new(4));
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(active, vec![0.0, 1.0]).with_text("active text"),
        ]))
        .expect("ingest active row");

    assert_eq!(
        store.stored_text(sealed).expect("sealed text"),
        Some("sealed text".to_owned())
    );
    assert_eq!(
        store.stored_text(active).expect("active text"),
        Some("active text".to_owned())
    );
    assert_eq!(
        store
            .stored_text(DocumentVersion::new(DocId::new(999), Revision::new(1)))
            .expect("missing text"),
        None
    );
    store.close().expect("close store");
}

#[test]
fn structured_phrase_query_returns_owned_utf8_snippet_and_provenance() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(111), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_text("Café 🚀 quick brown fox"),
            IngestDocument::new(
                DocumentVersion::new(DocId::new(112), Revision::new(1)),
                vec![0.0, 1.0],
            )
            .with_text("quick red brown fox"),
        ]))
        .expect("ingest phrase rows");
    store.seal().expect("seal phrase rows");

    let outcome = store
        .search_lexical_structured(
            &LexicalQuery::phrase(vec![b"quick".to_vec(), b"brown".to_vec()], 0, DEFAULT_FIELD),
            10,
            64,
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("structured phrase search");
    assert_eq!(outcome.candidates.len(), 1);
    let candidate = &outcome.candidates[0];
    assert_eq!(candidate.document.doc_id(), DocId::new(111));
    assert_eq!(candidate.provenance.len(), 2);
    assert!(candidate
        .provenance
        .iter()
        .all(|entry| entry.kind == LexicalMatchKind::Phrase && entry.boost_thousandths == 1_000));
    let source = "Café 🚀 quick brown fox";
    assert_eq!(candidate.snippet.source.start, 11);
    assert_eq!(candidate.snippet.source.end as usize, source.len());
    assert_eq!(candidate.snippet.text, "quick brown fox");
    assert!(
        candidate
            .snippet
            .highlights
            .iter()
            .all(|range| source.is_char_boundary(range.start as usize)
                && source.is_char_boundary(range.end as usize))
    );
    assert_eq!(outcome.expansions, candidate.provenance);
    let hybrid = store
        .search_hybrid_structured(
            SearchRequest::new(&[1.0, 0.0]),
            &LexicalQuery::phrase(vec![b"quick".to_vec(), b"brown".to_vec()], 0, DEFAULT_FIELD),
            &HybridQuery::new(2),
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("structured hybrid search");
    assert_eq!(hybrid.hits[0].key, DocId::new(111));
    store.close().expect("close store");
}

#[test]
fn structured_expansions_report_pinned_prefix_fuzzy_and_phonetic_boosts() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(121), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_text("alpha Smith"),
            IngestDocument::new(
                DocumentVersion::new(DocId::new(122), Revision::new(1)),
                vec![0.0, 1.0],
            )
            .with_text("alpga Smyth"),
            IngestDocument::new(
                DocumentVersion::new(DocId::new(123), Revision::new(1)),
                vec![0.5, 0.5],
            )
            .with_text("alpine Jones"),
        ]))
        .expect("ingest expansion rows");
    store.seal().expect("seal expansion rows");

    let prefix = store
        .search_lexical_structured(
            &LexicalQuery::prefix(b"al".to_vec(), DEFAULT_FIELD),
            10,
            32,
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("prefix search");
    assert!(prefix.expansions.len() >= 3);
    assert!(prefix.expansions.iter().all(|entry| {
        entry.kind == LexicalMatchKind::Prefix && entry.boost_thousandths == 1_000
    }));

    let fuzzy = store
        .search_lexical_structured(
            &LexicalQuery::fuzzy(b"alpha".to_vec(), 1, DEFAULT_FIELD),
            10,
            32,
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("fuzzy search");
    assert!(fuzzy.expansions.iter().any(|entry| {
        entry.term == b"alpha"
            && entry.kind == LexicalMatchKind::Fuzzy { distance: 0 }
            && entry.boost_thousandths == 1_000
    }));
    assert!(fuzzy.expansions.iter().any(|entry| {
        entry.term == b"alpga"
            && entry.kind == LexicalMatchKind::Fuzzy { distance: 1 }
            && entry.boost_thousandths == 500
    }));

    let phonetic = store
        .search_lexical_structured(
            &LexicalQuery::phonetic(b"Smith".to_vec(), DEFAULT_FIELD),
            10,
            32,
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("phonetic search");
    assert!(
        phonetic
            .expansions
            .iter()
            .any(|entry| entry.term == b"smith")
    );
    assert!(
        phonetic
            .expansions
            .iter()
            .any(|entry| entry.term == b"smyth")
    );
    assert!(phonetic.expansions.iter().all(|entry| {
        entry.kind == LexicalMatchKind::Phonetic && entry.boost_thousandths == 250
    }));
    store.close().expect("close store");
}

#[test]
fn typed_column_ingested_through_public_api_filters_after_reopen() {
    let directory = tempdir().expect("store directory");
    let category = ColumnId::new(1);
    let schema = Schema::new(vec![ColumnDefinition::new(
        category,
        "category",
        ColumnType::U64,
        false,
    )])
    .expect("typed schema");
    let store = Store::open(directory.path(), OpenOptions::default().with_schema(schema))
        .expect("create typed store");
    let unknown = store.ingest(IngestBatch::new(vec![
        IngestDocument::new(
            DocumentVersion::new(DocId::new(90), Revision::new(1)),
            vec![1.0, 0.0],
        )
        .with_columns(vec![(ColumnId::new(99), PredicateValue::U64(7))]),
    ]));
    assert!(matches!(
        unknown,
        Err(IngestError::Columns(BuildError::UnknownColumn(column))) if column == ColumnId::new(99)
    ));
    let mismatch = store.ingest(IngestBatch::new(vec![
        IngestDocument::new(
            DocumentVersion::new(DocId::new(91), Revision::new(1)),
            vec![1.0, 0.0],
        )
        .with_columns(vec![(category, PredicateValue::String("wrong".to_owned()))]),
    ]));
    assert!(matches!(
        mismatch,
        Err(IngestError::Columns(BuildError::TypeMismatch { column, .. })) if column == category
    ));
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(31), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_columns(vec![(category, PredicateValue::U64(7))]),
            IngestDocument::new(
                DocumentVersion::new(DocId::new(32), Revision::new(1)),
                vec![0.5, 0.5],
            )
            .with_columns(vec![(category, PredicateValue::U64(9))]),
        ]))
        .expect("ingest typed rows");
    let active_outcome = store
        .search_filtered(
            SearchRequest::new(&[1.0, 0.0]),
            &Predicate::Eq {
                column: category,
                value: PredicateValue::U64(7),
            },
            10,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("filter active typed rows");
    assert_eq!(active_outcome.candidates.len(), 1);
    store.seal().expect("seal typed rows");
    store.close().expect("close writer");

    let wrong_schema = Schema::new(vec![ColumnDefinition::new(
        category,
        "category",
        ColumnType::I64,
        false,
    )])
    .expect("different typed schema");
    assert!(matches!(
        Store::open(
            directory.path(),
            OpenOptions::default().with_schema(wrong_schema)
        ),
        Err(zeppelin_embed::lifecycle::StoreError::SchemaMismatch { .. })
    ));

    let reopened =
        Store::open(directory.path(), OpenOptions::default()).expect("reopen typed store");
    let outcome = reopened
        .search_filtered(
            SearchRequest::new(&[1.0, 0.0]),
            &Predicate::Eq {
                column: category,
                value: PredicateValue::U64(7),
            },
            10,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("filter typed rows");
    assert_eq!(outcome.candidates.len(), 1);
    assert_eq!(
        outcome.candidates[0]
            .document()
            .expect("document identity")
            .doc_id(),
        DocId::new(31)
    );
    reopened.close().expect("close reader");
}

#[test]
fn optional_text_and_columns_activate_after_text_free_rows() {
    let directory = tempdir().expect("store directory");
    let category = ColumnId::new(2);
    let schema = Schema::new(vec![ColumnDefinition::new(
        category,
        "category",
        ColumnType::U64,
        true,
    )])
    .expect("optional typed schema");
    let store = Store::open(directory.path(), OpenOptions::default().with_schema(schema))
        .expect("create typed store");
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(33), Revision::new(1)),
            vec![0.0, 1.0],
        )]))
        .expect("ingest text-free row");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(34), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_text("late zeppelin")
            .with_columns(vec![(category, PredicateValue::U64(7))]),
        ]))
        .expect("activate optional payloads");

    let lexical = store
        .search_lexical(
            &TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]),
            10,
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search late text");
    assert_eq!(lexical.candidates.len(), 1);
    assert_eq!(lexical.candidates[0].document.doc_id(), DocId::new(34));
    let filtered = store
        .search_filtered(
            SearchRequest::new(&[1.0, 0.0]),
            &Predicate::Eq {
                column: category,
                value: PredicateValue::U64(7),
            },
            10,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("filter late typed value");
    assert_eq!(filtered.candidates.len(), 1);
    assert_eq!(
        filtered.candidates[0]
            .document()
            .expect("document identity")
            .doc_id(),
        DocId::new(34)
    );
    store.seal().expect("seal lazily activated payloads");
    store.close().expect("close store");
}

#[test]
fn replacing_an_active_revision_adds_and_then_removes_optional_payloads() {
    let directory = tempdir().expect("store directory");
    let category = ColumnId::new(24);
    let schema = Schema::new(vec![ColumnDefinition::new(
        category,
        "category",
        ColumnType::U64,
        true,
    )])
    .expect("optional typed schema");
    let store = Store::open(directory.path(), OpenOptions::default().with_schema(schema))
        .expect("create typed store");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(2401), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_metadata(b"first metadata".to_vec()),
            IngestDocument::new(
                DocumentVersion::new(DocId::new(2402), Revision::new(1)),
                vec![0.0, 1.0],
            )
            .with_metadata(b"second metadata".to_vec()),
        ]))
        .expect("ingest payload-free revisions");

    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(2401), Revision::new(2)),
                vec![0.9, 0.1],
            )
            .with_metadata(b"replacement metadata is longer".to_vec())
            .with_text("replacement zeppelin")
            .with_columns(vec![(category, PredicateValue::U64(24))]),
        ]))
        .expect("add optional payloads by replacement");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(2403), Revision::new(1)),
                vec![-1.0, 0.0],
            )
            .with_text("retained airship"),
        ]))
        .expect("retain a non-matching lexical row");

    let lexical = store
        .search_lexical(
            &TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]),
            10,
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search replacement text");
    assert_eq!(lexical.candidates.len(), 1);
    assert_eq!(lexical.candidates[0].document.revision(), Revision::new(2));
    let filtered = store
        .search_filtered(
            SearchRequest::new(&[1.0, 0.0]),
            &Predicate::Eq {
                column: category,
                value: PredicateValue::U64(24),
            },
            10,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("filter replacement columns");
    assert_eq!(filtered.candidates.len(), 1);
    assert_eq!(
        filtered.candidates[0]
            .document()
            .expect("replacement identity")
            .revision(),
        Revision::new(2)
    );

    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(2401), Revision::new(3)),
            vec![0.8, 0.2],
        )]))
        .expect("remove optional payloads by replacement");
    let lexical = store
        .search_lexical(
            &TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]),
            10,
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search after text removal");
    assert!(lexical.candidates.is_empty());
    let filtered = store
        .search_filtered(
            SearchRequest::new(&[1.0, 0.0]),
            &Predicate::Eq {
                column: category,
                value: PredicateValue::U64(24),
            },
            10,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("filter after column removal");
    assert!(filtered.candidates.is_empty());
    let vector = store
        .search(
            SearchRequest::new(&[1.0, 0.0]),
            1,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search latest replacement");
    assert_eq!(
        vector.candidates[0].document().expect("latest identity"),
        DocumentVersion::new(DocId::new(2401), Revision::new(3))
    );
    store.close().expect("close store");
    let reopened = Store::open(directory.path(), OpenOptions::default()).expect("replay revisions");
    let replayed = reopened
        .search(
            SearchRequest::new(&[1.0, 0.0]),
            1,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search replayed latest replacement");
    assert_eq!(
        replayed.candidates[0]
            .document()
            .expect("replayed identity"),
        DocumentVersion::new(DocId::new(2401), Revision::new(3))
    );
    let removed_text = reopened
        .search_lexical(
            &TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]),
            10,
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search replayed text removal");
    assert!(removed_text.candidates.is_empty());
    reopened.close().expect("close replayed store");
}

#[test]
fn prechange_wal_fixture_replays_to_the_same_active_state() {
    let directory = tempdir().expect("store directory");
    let bytes = decode_hex(include_str!("fixtures/format/store_wal_prechange_v1.hex"))
        .expect("legacy WAL fixture");
    std::fs::write(directory.path().join("wal.ze"), bytes).expect("install legacy WAL");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("replay legacy WAL");
    let outcome = store
        .search(
            SearchRequest::new(&[1.0, 0.0]),
            1,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search replayed active row");
    assert_eq!(
        outcome.candidates[0]
            .document()
            .expect("legacy identity")
            .doc_id(),
        DocId::new(41)
    );
    store.close().expect("close replayed store");
}

#[test]
fn bitmap_wal_replays_text_and_typed_columns_before_seal() {
    let directory = tempdir().expect("store directory");
    let category = ColumnId::new(7);
    let schema = Schema::new(vec![ColumnDefinition::new(
        category,
        "category",
        ColumnType::U64,
        false,
    )])
    .expect("typed schema");
    let writer = Store::open(directory.path(), OpenOptions::default().with_schema(schema))
        .expect("create store");
    writer
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(42), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_text("zeppelin replay")
            .with_columns(vec![(category, PredicateValue::U64(5))]),
        ]))
        .expect("write bitmap WAL");
    writer.close().expect("close without seal");

    let reopened =
        Store::open(directory.path(), OpenOptions::default()).expect("replay bitmap WAL");
    let lexical = reopened
        .search_lexical(
            &TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]),
            1,
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search replayed text");
    assert_eq!(lexical.candidates[0].document.doc_id(), DocId::new(42));
    let filtered = reopened
        .search_filtered(
            SearchRequest::new(&[1.0, 0.0]),
            &Predicate::Eq {
                column: category,
                value: PredicateValue::U64(5),
            },
            1,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("filter replayed columns");
    assert_eq!(filtered.candidates.len(), 1);
    reopened.close().expect("close replayed store");
}

fn hybrid_store() -> (tempfile::TempDir, Store) {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(51), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_text("bronze zeppelin"),
            IngestDocument::new(
                DocumentVersion::new(DocId::new(52), Revision::new(1)),
                vec![0.0, 1.0],
            )
            .with_text("silver airship"),
        ]))
        .expect("ingest hybrid rows");
    (directory, store)
}

#[test]
fn store_level_hybrid_default_is_exact_and_populates_diagnostics() {
    let (_directory, store) = hybrid_store();
    let vector = SearchRequest::new(&[1.0, 0.0]);
    let lexical = TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]);
    let outcome = store
        .search_hybrid(
            vector,
            &lexical,
            &HybridQuery::new(2),
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("default hybrid tier is exact");
    assert_eq!(outcome.hits[0].key, DocId::new(51));
    assert!(outcome.diagnostics.fusion.is_some());
    assert!(outcome.diagnostics.tokenizer_epoch.is_some());
    assert!(outcome.diagnostics.exact_rescore);
    assert!(outcome.diagnostics.counters.lexical.docs_evaluated > 0);
    store.seal().expect("seal hybrid rows without a graph");
    let sealed = store
        .search_hybrid(
            vector,
            &lexical,
            &HybridQuery::new(2),
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("default hybrid tier exactly scans a graphless sealed segment");
    assert_eq!(sealed.hits[0].key, DocId::new(51));
    assert!(sealed.diagnostics.exact_rescore);
    store.close().expect("close store");
}

#[test]
fn store_level_hybrid_requests_the_window_not_the_corpus() {
    const ROWS: usize = 300;
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    let documents = (0..ROWS)
        .map(|row| {
            IngestDocument::new(
                DocumentVersion::new(DocId::new(row as u128 + 1), Revision::new(1)),
                vec![1.0 - row as f32 * 0.002, row as f32 * 0.002],
            )
            .with_text(if row % 3 == 0 {
                "bronze zeppelin"
            } else {
                "silver airship"
            })
        })
        .collect::<Vec<_>>();
    store
        .ingest(IngestBatch::new(documents))
        .expect("ingest a corpus wider than the hybrid window");
    let outcome = store
        .search_hybrid(
            SearchRequest::new(&[1.0, 0.0]),
            &TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]),
            &HybridQuery::new(5),
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("bounded hybrid search");
    let report = outcome
        .diagnostics
        .hybrid
        .expect("hybrid diagnostics report the bounded producers");
    assert!(
        report.window >= HYBRID_WINDOW_FLOOR && report.window < ROWS,
        "k = 5 resolves to the window floor and widens by doubling, never to \
         the corpus, got {}",
        report.window
    );
    assert_eq!(
        report.vector_returned,
        report.window + report.cross_filled_vector,
        "fusion saw the window plus its cross-fill, never the corpus"
    );
    assert!(
        report.vector_returned <= 2 * report.window,
        "a cross-filled window can at most double, got {}",
        report.vector_returned
    );
    assert!(
        report.lexical_returned <= 2 * report.window,
        "a cross-filled window can at most double, got {}",
        report.lexical_returned
    );
    assert!(
        report.cross_filled_vector > 0,
        "this fixture ranks matching rows outside the vector window, so cross-fill must fire"
    );
    assert_eq!(outcome.hits.len(), 5);
    assert!(
        outcome
            .hits
            .iter()
            .all(|hit| hit.vector_squared_l2.is_some()),
        "every fused hit carries the exact vector score its window supplied"
    );
    store.close().expect("close store");
}

#[test]
fn store_level_hybrid_explicit_estimated_tier_is_rejected() {
    let (_directory, store) = hybrid_store();
    let vector = SearchRequest::new(&[1.0, 0.0]);
    let lexical = TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]);
    assert_eq!(
        store.search_hybrid(
            vector,
            &lexical,
            &HybridQuery::new(2),
            SearchOptions::default().with_tier(SearchTier::Auto),
            QueryControl::Cancel(CancelToken::new()),
        ),
        Err(FusionError::EstimatedVectorScore { rank: 0 })
    );
    store.close().expect("close store");
}

#[test]
fn store_level_hybrid_explicit_exact_tier_preserves_ordered_score_bits() {
    let (_directory, store) = hybrid_store();
    let vector = SearchRequest::new(&[1.0, 0.0]);
    let lexical = TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]);
    let outcome = store
        .search_hybrid(
            vector,
            &lexical,
            &HybridQuery::new(2),
            SearchOptions::default().with_tier(SearchTier::Graph(GraphSearchOptions::default())),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search hybrid");
    let ordered_score_bits = outcome
        .hits
        .iter()
        .map(|hit| (hit.key, hit.vector_squared_l2.map(f64::to_bits)))
        .collect::<Vec<_>>();
    assert_eq!(
        ordered_score_bits,
        [
            (DocId::new(51), Some(0.0_f64.to_bits())),
            (DocId::new(52), Some(2.0_f64.to_bits())),
        ]
    );
    store.close().expect("close store");
}

#[test]
fn store_owned_hybrid_runs_both_legs_and_names_the_lexical_thread() {
    let (_directory, store) = hybrid_store();
    let caller = std::thread::current().id();
    store
        .search_hybrid(
            SearchRequest::new(&[0.0, 0.0]),
            &TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]),
            &HybridQuery::new(2),
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("parallel hybrid search");
    let receipt = store
        .take_hybrid_execution_receipt()
        .expect("hybrid execution receipt");
    assert_eq!(receipt.vector_thread, caller);
    assert_eq!(receipt.lexical_thread_name.as_deref(), Some("zeppelin-fts"));
    assert!(receipt.vector_completed);
    assert!(receipt.lexical_completed);
    store.close().expect("close store");
}

#[test]
fn hybrid_lexical_leg_reuses_one_pooled_thread_across_queries() {
    const CHILD_PROCESS: &str = "ZE_TEST_HYBRID_LEXICAL_POOL_CHILD";
    if std::env::var_os(CHILD_PROCESS).is_none() {
        let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .arg("hybrid_lexical_leg_reuses_one_pooled_thread_across_queries")
            .arg("--exact")
            .env(CHILD_PROCESS, "1")
            .status()
            .expect("run isolated thread-census test");
        assert!(status.success(), "isolated thread-census test failed");
        return;
    }

    let (_directory, store) = hybrid_store();
    let vector = [0.0_f32, 0.0_f32];
    let lexical = TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]);
    store
        .search(
            SearchRequest::new(&vector),
            2,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("warm vector query pool");
    let before = os_thread_ids().expect("census before first hybrid query");

    store
        .search_hybrid(
            SearchRequest::new(&vector),
            &lexical,
            &HybridQuery::new(2),
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("first hybrid query");
    let after_first = os_thread_ids().expect("census after first hybrid query");
    let started = after_first.difference(&before).copied().collect::<Vec<_>>();
    assert_eq!(
        started.len(),
        1,
        "first hybrid query did not leave exactly one lexical worker: {started:?}"
    );

    store
        .search_hybrid(
            SearchRequest::new(&vector),
            &lexical,
            &HybridQuery::new(2),
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("second hybrid query");
    let after_second = os_thread_ids().expect("census after second hybrid query");
    let newly_started = after_second
        .difference(&after_first)
        .copied()
        .collect::<Vec<_>>();
    assert!(
        newly_started.is_empty(),
        "second hybrid query started new OS threads: {newly_started:?}"
    );
    assert!(
        started.iter().all(|thread| after_second.contains(thread)),
        "first hybrid query's lexical worker was not reused: {started:?}"
    );
    store.close().expect("close store");
}

#[test]
fn lexical_leg_panic_is_typed_and_the_store_remains_usable() {
    let directory = tempdir().expect("store directory");
    let dependencies = StoreTestDependencies::new(
        std::sync::Arc::new(StdVfs),
        std::sync::Arc::new(SystemMonotonicClock),
    )
    .with_hybrid_leg_fault(HybridLegTestFault::Panic(FusionLeg::Lexical));
    let store =
        Store::open_with_test_dependencies(directory.path(), OpenOptions::default(), dependencies)
            .expect("open panic fixture");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(131), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_text("zeppelin panic containment"),
        ]))
        .expect("ingest panic fixture");
    let term = TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]);
    assert_eq!(
        store
            .search_hybrid(
                SearchRequest::new(&[1.0, 0.0]),
                &term,
                &HybridQuery::new(1),
                SearchOptions::default(),
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect_err("injected lexical panic must be contained"),
        FusionError::LegPanic {
            leg: FusionLeg::Lexical,
            detail: "lexical hybrid leg panicked",
        }
    );
    assert_eq!(
        store
            .search_hybrid(
                SearchRequest::new(&[1.0, 0.0]),
                &term,
                &HybridQuery::new(1),
                SearchOptions::default(),
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect("store reusable after contained panic")
            .hits
            .len(),
        1
    );
    store.close().expect("close store");
}

#[test]
fn vector_leg_panic_is_typed_after_join_and_the_store_remains_usable() {
    let directory = tempdir().expect("store directory");
    let dependencies = StoreTestDependencies::new(
        std::sync::Arc::new(StdVfs),
        std::sync::Arc::new(SystemMonotonicClock),
    )
    .with_hybrid_leg_fault(HybridLegTestFault::Panic(FusionLeg::Vector));
    let store =
        Store::open_with_test_dependencies(directory.path(), OpenOptions::default(), dependencies)
            .expect("open vector-panic fixture");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(132), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_text("zeppelin vector panic containment"),
        ]))
        .expect("ingest vector-panic fixture");
    let term = TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]);
    assert_eq!(
        store
            .search_hybrid(
                SearchRequest::new(&[1.0, 0.0]),
                &term,
                &HybridQuery::new(1),
                SearchOptions::default(),
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect_err("injected vector panic must be contained"),
        FusionError::LegPanic {
            leg: FusionLeg::Vector,
            detail: "vector hybrid leg panicked",
        }
    );
    let receipt = store
        .take_hybrid_execution_receipt()
        .expect("both joined legs report completion");
    assert!(receipt.vector_completed && receipt.lexical_completed);
    assert_eq!(
        store
            .search_hybrid(
                SearchRequest::new(&[1.0, 0.0]),
                &term,
                &HybridQuery::new(1),
                SearchOptions::default(),
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect("store reusable after contained vector panic")
            .hits
            .len(),
        1
    );
    store.close().expect("close store");
}

#[test]
fn physical_purge_remaps_the_postings_region_with_survivors() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(61), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_text("zeppelin survivor"),
            IngestDocument::new(
                DocumentVersion::new(DocId::new(62), Revision::new(1)),
                vec![0.0, 1.0],
            )
            .with_text("zeppelin removed"),
        ]))
        .expect("ingest purge rows");
    store.seal().expect("seal purge rows");
    let token = store.purge(&[DocId::new(62)]).expect("schedule purge");
    store.await_physical_purge(token).expect("complete purge");
    let outcome = store
        .search_lexical(
            &TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]),
            10,
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search rewritten postings");
    assert_eq!(outcome.candidates.len(), 1);
    assert_eq!(outcome.candidates[0].document.doc_id(), DocId::new(61));
    let snapshot = store.snapshot().expect("pin purged snapshot");
    let text = snapshot.segments()[0]
        .stored_text()
        .expect("validate rewritten stored text")
        .expect("stored text survives purge");
    assert_eq!(text.row_count(), 1);
    assert_eq!(text.row(0), Some(Some("zeppelin survivor")));
    drop(snapshot);
    store.close().expect("close store");
}

#[cfg(target_os = "macos")]
fn os_thread_ids() -> std::io::Result<std::collections::BTreeSet<u64>> {
    unsafe extern "C" {
        static mach_task_self_: libc::mach_port_t;
        fn mach_port_deallocate(
            task: libc::mach_port_t,
            name: libc::mach_port_t,
        ) -> libc::kern_return_t;
    }

    let task = unsafe {
        // SAFETY: libSystem initializes the current-task port before Rust `main`.
        mach_task_self_
    };
    let mut threads = std::ptr::null_mut();
    let mut count = 0_u32;
    let result = unsafe {
        // SAFETY: the two out pointers are valid writable storage for Mach's allocated array.
        libc::task_threads(task, &raw mut threads, &raw mut count)
    };
    if result != libc::KERN_SUCCESS {
        return Err(std::io::Error::other(format!(
            "task_threads failed with Mach code {result}"
        )));
    }
    let count_usize =
        usize::try_from(count).map_err(|_| std::io::Error::other("thread count exceeds usize"))?;
    let ports = unsafe {
        // SAFETY: successful `task_threads` returned `count` initialized port names.
        std::slice::from_raw_parts(threads, count_usize)
    };
    let ids = ports.iter().map(|port| u64::from(*port)).collect();
    for port in ports {
        let _ = unsafe {
            // SAFETY: each name is a send right returned by `task_threads` to this task.
            mach_port_deallocate(task, *port)
        };
    }
    let bytes = count_usize
        .checked_mul(std::mem::size_of::<libc::thread_t>())
        .ok_or_else(|| std::io::Error::other("thread array byte length overflow"))?;
    let address = threads as libc::vm_address_t;
    let size = libc::vm_size_t::try_from(bytes)
        .map_err(|_| std::io::Error::other("thread array byte length exceeds vm_size_t"))?;
    let release = unsafe {
        // SAFETY: this is the exact task-allocated array returned by `task_threads`.
        libc::vm_deallocate(task, address, size)
    };
    if release != libc::KERN_SUCCESS {
        return Err(std::io::Error::other(format!(
            "vm_deallocate failed with Mach code {release}"
        )));
    }
    Ok(ids)
}

#[cfg(target_os = "linux")]
fn os_thread_ids() -> std::io::Result<std::collections::BTreeSet<u64>> {
    std::fs::read_dir("/proc/self/task")?
        .map(|entry| {
            let entry = entry?;
            entry
                .file_name()
                .to_string_lossy()
                .parse::<u64>()
                .map_err(std::io::Error::other)
        })
        .collect()
}

#[test]
fn row_for_document_version_locates_every_sealed_row_and_rejects_absent_revisions() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    // Doc ids are deliberately out of order so the lookup cannot rely on
    // ingest order matching id order.
    let ids: [u128; 6] = [900, 7, 512, 3, 1_000_000, 64];
    store
        .ingest(IngestBatch::new(
            ids.iter()
                .enumerate()
                .map(|(row, id)| {
                    IngestDocument::new(
                        DocumentVersion::new(DocId::new(*id), Revision::new(row as u64 + 1)),
                        vec![row as f32, 1.0],
                    )
                    .with_text(format!("row {row}"))
                })
                .collect(),
        ))
        .expect("ingest unordered rows");
    store.seal().expect("seal unordered rows");
    let snapshot = store.snapshot().expect("pin snapshot");
    let segment = &snapshot.segments()[0];
    for (row, id) in ids.iter().enumerate() {
        let version = DocumentVersion::new(DocId::new(*id), Revision::new(row as u64 + 1));
        assert_eq!(
            segment.row_for_document_version(version).expect("lookup"),
            Some(row),
            "doc {id} must resolve to its sealed row"
        );
        assert_eq!(
            segment
                .row_for_document_version(DocumentVersion::new(
                    DocId::new(*id),
                    Revision::new(row as u64 + 2)
                ))
                .expect("stale revision lookup"),
            None,
            "a different revision of doc {id} is not present"
        );
    }
    assert_eq!(
        segment
            .row_for_document_version(DocumentVersion::new(DocId::new(8), Revision::new(1)))
            .expect("absent doc lookup"),
        None
    );
    drop(snapshot);
    store.close().expect("close store");
}

#[test]
fn stored_text_resolves_rows_across_segments_with_stale_revisions_and_absent_text() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    let stale = DocumentVersion::new(DocId::new(10), Revision::new(1));
    let textless = DocumentVersion::new(DocId::new(11), Revision::new(1));
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(stale, vec![1.0, 0.0]).with_text("stale body"),
            IngestDocument::new(textless, vec![0.0, 1.0]),
        ]))
        .expect("ingest first segment");
    store.seal().expect("seal first segment");
    let fresh = DocumentVersion::new(DocId::new(10), Revision::new(2));
    let last = DocumentVersion::new(DocId::new(12), Revision::new(1));
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(fresh, vec![1.0, 1.0]).with_text("fresh body"),
            IngestDocument::new(last, vec![0.5, 0.5]).with_text("last body"),
        ]))
        .expect("ingest second segment");
    store.seal().expect("seal second segment");
    let before = store.stats().expect("stats before lookups");

    assert_eq!(
        store.stored_text(fresh).expect("fresh"),
        Some("fresh body".to_owned())
    );
    assert_eq!(
        store.stored_text(last).expect("last"),
        Some("last body".to_owned())
    );
    // The stale revision's row is still physically present in segment one;
    // its exact revision is what the caller asked for, so its text returns.
    assert_eq!(
        store.stored_text(stale).expect("stale"),
        Some("stale body".to_owned())
    );
    assert_eq!(store.stored_text(textless).expect("textless"), None);
    assert_eq!(
        store
            .stored_text(DocumentVersion::new(DocId::new(10), Revision::new(3)))
            .expect("absent revision"),
        None
    );
    assert_eq!(
        store
            .stored_text(DocumentVersion::new(DocId::new(99), Revision::new(1)))
            .expect("absent doc"),
        None
    );
    // The identity index each lookup builds is a retained query view and
    // must reach snapshot accounting exactly, like every other cached view.
    let snapshot = store.snapshot().expect("accounted snapshot");
    let retained = snapshot
        .segments()
        .iter()
        .map(zeppelin_embed::segment::reader::SegmentReader::retained_query_view_bytes)
        .sum::<u64>();
    drop(snapshot);
    let after = store.stats().expect("stats after lookups");
    assert!(retained > 0, "lookups must build the identity index");
    assert_eq!(after.snapshot_bytes - before.snapshot_bytes, retained);
    store.close().expect("close store");
}

#[test]
fn stored_text_verifies_the_region_checksum_on_every_call_of_the_same_reader() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(1), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_text("alpha body"),
            IngestDocument::new(
                DocumentVersion::new(DocId::new(2), Revision::new(1)),
                vec![0.0, 1.0],
            )
            .with_text("omega body"),
        ]))
        .expect("ingest rows");
    store.seal().expect("seal rows");
    let snapshot = store.snapshot().expect("pin snapshot");
    let id = snapshot.segments()[0].meta().id;
    drop(snapshot);
    store.close().expect("close store");
    let path = directory.path().join(id.file_name());

    // One mapped reader: validated once, then corrupted underneath it. The
    // flip lands in the private mapping (copy-on-write), never in the file.
    let mut reader = SegmentReader::open(&StdVfs, &path, id).expect("open sealed segment");
    {
        let first = reader
            .stored_text()
            .expect("first call verifies")
            .expect("text region");
        assert_eq!(first.row(1), Some(Some("omega body")));
    }
    let last_payload_byte = reader
        .directory()
        .iter()
        .find(|entry| entry.kind == RegionKind::StoredText.id())
        .map(|entry| entry.offset + entry.length - 1)
        .expect("stored-text region entry");
    reader
        .corrupt_mapped_byte_for_test(last_payload_byte as usize, 0x01)
        .expect("corrupt one mapped stored-text payload byte");

    match reader.stored_text() {
        Err(SegmentError::Format(error)) => {
            assert!(
                error
                    .to_string()
                    .contains("StoredText failed BlockChecksum"),
                "second call must fail the region checksum, got {error}"
            );
        }
        other => panic!("second call must re-verify the region checksum, got {other:?}"),
    }
}
