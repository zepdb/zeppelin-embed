#![allow(clippy::expect_used)]

use tempfile::tempdir;
use zeppelin_embed::fts::bm25::CorpusStats;
use zeppelin_embed::fts::index::DEFAULT_FIELD;
use zeppelin_embed::fts::search::TermQuery;
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision,
};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};

const REMOVED: DocId = DocId::new(1);

fn astra_16_ingest(store: &Store, id: u128, text: &str) {
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(id), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_text(text),
        ]))
        .expect("ingest");
}

fn astra_16_absent_query(store: &Store) {
    let answer = store
        .search_lexical(
            &TermQuery::flat(vec![b"absent".to_vec()], &[DEFAULT_FIELD]),
            10,
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("query");
    assert!(answer.candidates.is_empty());
}

#[test]
fn astra_16_active_ingest_reuses_unchanged_sealed_lexical_statistics() {
    use zeppelin_embed::meta::bitmap_observer as observer;
    let directory = tempdir().expect("directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    store
        .ingest(IngestBatch::new(
            (1..=2048)
                .map(|id| {
                    IngestDocument::new(
                        DocumentVersion::new(DocId::new(id), Revision::new(1)),
                        vec![1.0, 0.0],
                    )
                    .with_text("present pair")
                })
                .collect(),
        ))
        .expect("sealed corpus");
    store.seal().expect("seal");
    astra_16_absent_query(&store);
    astra_16_ingest(&store, 3000, "active present");
    observer::begin();
    astra_16_absent_query(&store);
    let work = observer::take();
    assert_eq!(
        work.rows, 2,
        "unchanged sealed rows must not be walked again: {work:?}"
    );
    store.close().expect("close");
}

#[test]
fn astra_16_alive_change_invalidates_only_affected_contribution() {
    use zeppelin_embed::meta::bitmap_observer as observer;
    let directory = tempdir().expect("directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    for (start, count) in [(1, 32), (100, 17)] {
        store
            .ingest(IngestBatch::new(
                (start..start + count)
                    .map(|id| {
                        IngestDocument::new(
                            DocumentVersion::new(DocId::new(id), Revision::new(1)),
                            vec![1.0, 0.0],
                        )
                        .with_text("present pair")
                    })
                    .collect(),
            ))
            .expect("batch");
        store.seal().expect("seal");
    }
    astra_16_absent_query(&store);
    let before = store.lexical_contribution_cache_counters();
    store
        .delete(DeleteBatch::new(vec![DocId::new(1)]))
        .expect("delete");
    observer::begin();
    zeppelin_embed::fts::preparation_observer::begin();
    astra_16_absent_query(&store);
    let work = observer::take();
    let after = store.lexical_contribution_cache_counters();
    assert_eq!(
        after.0 - before.0,
        1,
        "unchanged second contribution reused"
    );
    assert_eq!(
        after.1 - before.1,
        1,
        "only tombstoned contribution rebuilt"
    );
    let statistics_rows = zeppelin_embed::fts::preparation_observer::live_statistics_rows();
    zeppelin_embed::fts::preparation_observer::take();
    assert_eq!(
        statistics_rows, 31,
        "only affected live counters walk: {work:?}"
    );
    let stats = store.lexical_corpus_stats().expect("uncached stats");
    assert_eq!((stats.document_count(), stats.total_tokens()), (48, 96));
    store.close().expect("close");
}

#[test]
fn astra_16_cached_global_stats_match_exhaustive_live_model() {
    fn check(store: &Store, model: &[(u128, &str)]) {
        use zeppelin_embed::fts::{
            bm25::Bm25Params,
            index::{Document, LexicalIndex, SegmentIndex},
            search::search,
            tokenizer::{Analyzer, TokenizerConfig},
        };
        let analyzer = Analyzer::new(TokenizerConfig::text_default()).expect("analyzer");
        let mut reference = SegmentIndex::new();
        for (_, text) in model {
            reference
                .push_document(&analyzer, &Document::with_text(text))
                .expect("reference row");
        }
        let mut index = LexicalIndex::new();
        index.push_segment(reference).expect("reference seal");
        let query = TermQuery::flat(vec![b"common".to_vec()], &[DEFAULT_FIELD]);
        let exhaustive =
            search(&index, &query, usize::MAX, Bm25Params::default()).expect("uncached exhaustive");
        let mut expected = exhaustive
            .hits
            .into_iter()
            .map(|h| {
                (
                    DocId::new(model.get(h.doc.row as usize).expect("reference id").0),
                    h.score.to_bits(),
                )
            })
            .collect::<Vec<_>>();
        let mut actual = query_scores(store)
            .into_iter()
            .map(|(id, score)| (id, score.to_bits()))
            .collect::<Vec<_>>();
        expected.sort_unstable();
        actual.sort_unstable();
        assert_eq!(
            actual, expected,
            "cached scores and identities match fresh exhaustive index"
        );
        // Independent literal live corpus and BM25 equation, not production stats.
        let n = model.len() as f64;
        let lengths = model
            .iter()
            .map(|(_, text)| text.split_whitespace().count())
            .collect::<Vec<_>>();
        let tokens: usize = lengths.iter().sum();
        let df = model
            .iter()
            .filter(|(_, text)| text.split_whitespace().any(|t| t == "common"))
            .count() as f64;
        let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
        for (id, text) in model {
            let tf = text.split_whitespace().filter(|t| *t == "common").count() as f64;
            if tf == 0.0 {
                continue;
            }
            let len = text.split_whitespace().count() as f64;
            let expected =
                idf * (tf * 2.2) / (tf + 1.2 * (0.25 + 0.75 * len / (tokens as f64 / n)));
            let actual = actual
                .iter()
                .find(|(doc, _)| doc.get() == *id)
                .expect("literal match");
            assert!((f64::from_bits(actual.1) - expected).abs() < 1e-12);
        }
        let stats = store.lexical_corpus_stats().expect("uncached global stats");
        assert_eq!(
            (stats.document_count(), stats.total_tokens()),
            (model.len() as u64, tokens as u64)
        );
    }
    let directory = tempdir().expect("directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    let mut model = vec![(1, "common red"), (2, "common common blue"), (3, "other")];
    for (id, text) in &model {
        astra_16_ingest(&store, *id, text);
    }
    store.seal().expect("first seal");
    check(&store, &model);
    astra_16_ingest(&store, 4, "common green green green");
    model.push((4, "common green green green"));
    check(&store, &model);
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(2), Revision::new(2)),
                vec![1.0, 0.0],
            )
            .with_text("common changed"),
        ]))
        .expect("replace sealed id");
    model
        .iter_mut()
        .find(|(id, _)| *id == 2)
        .expect("model row")
        .1 = "common changed";
    check(&store, &model);
    store
        .delete(DeleteBatch::new(vec![DocId::new(1)]))
        .expect("delete");
    model.retain(|(id, _)| *id != 1);
    check(&store, &model);
    store.seal().expect("second seal");
    check(&store, &model);
    let token = store.purge(&[DocId::new(1)]).expect("purge");
    store.await_physical_purge(token).expect("purge complete");
    check(&store, &model);
    store.close().expect("close");
}

#[test]
fn astra_16_retention_reuses_survivor_with_new_source_ordinal() {
    use zeppelin_embed::fts::preparation_observer as observer;
    use zeppelin_embed::ingest::RetentionPolicy;
    let directory = tempdir().expect("directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    for (id, timestamp) in [(1, 0), (2, 25)] {
        store
            .ingest(IngestBatch::new(vec![
                IngestDocument::new(
                    DocumentVersion::new(DocId::new(id), Revision::new(1)),
                    vec![1.0, 0.0],
                )
                .with_timestamp(timestamp)
                .with_text("common pair"),
            ]))
            .expect("ingest");
        store.seal().expect("seal");
    }
    assert_eq!(query_scores(&store).len(), 2);
    let before = store.lexical_contribution_cache_counters();
    store
        .apply_retention(RetentionPolicy::new(10).expect("window"), 30)
        .expect("retention");
    observer::begin();
    let results = query_scores(&store);
    assert_eq!(observer::live_statistics_rows(), 0);
    observer::take();
    assert_eq!(results.len(), 1);
    assert_eq!(results.first().expect("survivor").0, DocId::new(2));
    assert!((results.first().expect("score").1 - (1.0_f64 + 0.5 / 1.5).ln()).abs() < 1e-12);
    let after = store.lexical_contribution_cache_counters();
    assert_eq!((after.0 - before.0, after.1 - before.1), (1, 0));
    store.close().expect("close");
}

#[test]
fn astra_15_allow_list_validation_does_not_visit_every_valid_row() {
    use zeppelin_embed::fts::{
        bm25::Bm25Params,
        index::{Document, LexicalIndex, SegmentIndex},
        tokenizer::{Analyzer, Profile},
    };
    use zeppelin_embed::meta::{DocBitmap, bitmap_observer as observer};
    use zeppelin_embed::planner::{LexicalBranch, search_lexical_filtered};

    let analyzer = Analyzer::new(Profile::Code.config()).expect("analyzer");
    let mut segment = SegmentIndex::new();
    for _ in 0..131_072 {
        segment
            .push_document(&analyzer, &Document::with_text("present"))
            .expect("document");
    }
    let mut index = LexicalIndex::new();
    index.push_segment(segment).expect("seal");
    for allowed in [
        DocBitmap::full(131_072),
        DocBitmap::from_ids((0..131_072).step_by(97)),
    ] {
        observer::begin();
        let result = search_lexical_filtered(
            &index,
            &TermQuery::flat(vec![b"absent".to_vec()], &[DEFAULT_FIELD]),
            10,
            Bm25Params::default(),
            &[allowed],
            Some(LexicalBranch::PostCheck),
        );
        let work = observer::take();
        assert!(result.expect("valid allow list").result.hits.is_empty());
        assert_eq!(work.rows, 0, "validation walked the valid prefix: {work:?}");
        assert_eq!(work.probes, 1);
    }
}

#[test]
fn astra_15_live_assembly_only_walks_rows_for_token_summation() {
    use zeppelin_embed::fts::{
        index::{Document, LexicalIndex, SegmentIndex},
        sealed::SealedSegment,
        tokenizer::{Analyzer, Profile},
    };
    use zeppelin_embed::meta::{DocBitmap, bitmap_observer as observer};

    let analyzer = Analyzer::new(Profile::Code.config()).expect("analyzer");
    let mut segment = SegmentIndex::new();
    for _ in 0..2048 {
        segment
            .push_document(&analyzer, &Document::with_text("present pair"))
            .expect("document");
    }
    let sealed = SealedSegment::seal(&segment).expect("seal");
    for (alive, documents, tokens) in [
        (DocBitmap::full(2048), 2048, 4096),
        (DocBitmap::from_ids([0, 17, 2047]), 3, 6),
    ] {
        let mut index = LexicalIndex::new();
        observer::begin();
        index
            .push_sealed_with_live_rows(sealed.clone(), &alive)
            .expect("validated live assembly");
        let work = observer::take();
        assert_eq!(index.document_count(), documents);
        assert_eq!(index.total_tokens(), tokens);
        assert_eq!(
            work.rows as u64, documents,
            "only live token summation may walk rows: {work:?}"
        );
        assert_eq!(work.probes, 1);
    }
}

#[test]
fn astra_15_allow_list_reports_same_first_invalid_row() {
    use zeppelin_embed::fts::{
        bm25::Bm25Params,
        index::{Document, IndexError, LexicalIndex, SegmentIndex},
        sealed::SealedSegment,
        tokenizer::{Analyzer, Profile},
    };
    use zeppelin_embed::meta::DocBitmap;
    use zeppelin_embed::planner::{LexicalFilterError, search_lexical_filtered};

    let analyzer = Analyzer::new(Profile::Code.config()).expect("analyzer");
    let mut segment = SegmentIndex::new();
    for _ in 0..65_536 {
        segment
            .push_document(&analyzer, &Document::with_text("present"))
            .expect("document");
    }
    let sealed = SealedSegment::seal(&segment).expect("seal");
    let mut index = LexicalIndex::new();
    index.push_sealed(sealed.clone());
    index.push_sealed(sealed.clone());
    for mut invalid in [
        DocBitmap::full(65_536),
        DocBitmap::from_ids([0, 17, 65_535]),
    ] {
        invalid.insert(65_537);
        invalid.insert(u32::MAX);
        invalid.insert(65_536);
        let error = search_lexical_filtered(
            &index,
            &TermQuery::flat(vec![b"present".to_vec()], &[DEFAULT_FIELD]),
            10,
            Bm25Params::default(),
            &[DocBitmap::full(65_536), invalid.clone()],
            None,
        )
        .expect_err("invalid allow-list");
        assert!(matches!(
            error,
            LexicalFilterError::RowOutOfRange {
                segment: 1,
                row: 65_536,
                row_count: 65_536
            }
        ));
        let mut live_index = LexicalIndex::new();
        live_index.push_sealed(sealed.clone());
        let error = live_index
            .push_sealed_with_live_rows(sealed.clone(), &invalid)
            .expect_err("invalid live bitmap");
        assert!(matches!(
            error,
            IndexError::LiveRowOutOfRange {
                segment: 1,
                row: 65_536,
                row_count: 65_536
            }
        ));
        assert_eq!(
            live_index.segments().len(),
            1,
            "refusal published no segment"
        );
        assert_eq!(live_index.document_count(), 65_536);
        assert_eq!(live_index.total_tokens(), 65_536);
    }
}

#[test]
fn astra_15_wrong_segment_count_precedes_row_validation() {
    use zeppelin_embed::fts::{
        bm25::Bm25Params,
        index::{Document, LexicalIndex, SegmentIndex},
        tokenizer::{Analyzer, Profile},
    };
    use zeppelin_embed::meta::{DocBitmap, bitmap_observer as observer};
    use zeppelin_embed::planner::{LexicalFilterError, search_lexical_filtered};

    let analyzer = Analyzer::new(Profile::Code.config()).expect("analyzer");
    let mut segment = SegmentIndex::new();
    segment
        .push_document(&analyzer, &Document::with_text("present"))
        .expect("document");
    let mut index = LexicalIndex::new();
    index.push_segment(segment).expect("seal");
    for allowed in [Vec::new(), vec![DocBitmap::from_ids([u32::MAX]); 2]] {
        let actual = allowed.len();
        observer::begin();
        let error = search_lexical_filtered(
            &index,
            &TermQuery::flat(vec![b"present".to_vec()], &[DEFAULT_FIELD]),
            10,
            Bm25Params::default(),
            &allowed,
            None,
        )
        .expect_err("wrong segment count");
        let work = observer::take();
        assert!(matches!(error, LexicalFilterError::SegmentCount {
            expected: 1, actual: count
        } if count == actual));
        assert_eq!(
            work,
            observer::Work::default(),
            "count refusal precedes all bitmap probes"
        );
    }
}

#[test]
fn astra_02_candidate_bm25_matches_exhaustive_scores() {
    use zeppelin_embed::fts::bm25::Bm25Params;
    use zeppelin_embed::fts::index::{Document, LexicalIndex, SegmentIndex};
    use zeppelin_embed::fts::sealed::SealedSegment;
    use zeppelin_embed::fts::search::search;
    use zeppelin_embed::fts::tokenizer::{Analyzer, Profile};
    use zeppelin_embed::meta::DocBitmap;
    use zeppelin_embed::planner::LexicalBranch;
    use zeppelin_embed::planner::search_lexical_filtered;

    let analyzer = Analyzer::new(Profile::Code.config()).expect("analyzer");
    let mut persisted = SegmentIndex::new();
    let mut active = SegmentIndex::new();
    for row in 0..512 {
        let text = ["alpha beta", "alpha alpha", "omega", "beta"][row % 4];
        for segment in [&mut persisted, &mut active] {
            segment
                .push_document(&analyzer, &Document::with_text(text))
                .expect("document");
        }
    }
    let bytes = SealedSegment::seal(&persisted)
        .expect("seal")
        .encode_region()
        .expect("persist");
    let mut index = LexicalIndex::new();
    index
        .push_sealed_with_live_rows(
            SealedSegment::decode_region(&bytes).expect("decode persisted segment"),
            &DocBitmap::from_ids((0..512).filter(|row| *row != 1)),
        )
        .expect("tombstoned segment statistics");
    index.push_segment(active).expect("active lexical assembly");
    assert_eq!(index.document_count(), 1023);
    let allow_lists = [
        DocBitmap::from_ids([0, 2, 257, 510]),
        DocBitmap::from_ids([3, 31, 300, 509]),
    ];
    let query = TermQuery::flat(
        vec![b"alpha".to_vec(), b"beta".to_vec(), b"alpha".to_vec()],
        &[DEFAULT_FIELD],
    );
    let expected = search(&index, &query, usize::MAX, Bm25Params::default())
        .expect("exhaustive")
        .hits
        .into_iter()
        .filter(|hit| {
            allow_lists
                .get(hit.doc.segment as usize)
                .is_some_and(|rows| rows.contains(hit.doc.row))
        })
        .map(|hit| (hit.doc, hit.score.to_bits()))
        .collect::<Vec<_>>();
    let actual =
        search_lexical_filtered(&index, &query, 8, Bm25Params::default(), &allow_lists, None)
            .expect("sparse filtered search");
    assert_eq!(actual.branch, LexicalBranch::AllowListDrive);
    // Eight eligible rows in 1,023 live documents also select the inner
    // row-driven branch (divisor 64), so this reaches the candidate scorer.
    assert_eq!(
        actual
            .result
            .hits
            .into_iter()
            .map(|hit| (hit.doc, hit.score.to_bits()))
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(actual.result.counters.docs_evaluated, 6);
}

fn text_corpus() -> Vec<IngestDocument> {
    [
        (
            1_u128,
            "removed padding padding padding padding padding padding padding",
        ),
        (2, "common short"),
        (3, "common medium filler"),
        (4, "other filler"),
    ]
    .into_iter()
    .map(|(id, text)| {
        IngestDocument::new(
            DocumentVersion::new(DocId::new(id), Revision::new(1)),
            vec![id as f32, 1.0],
        )
        .with_text(text)
    })
    .collect()
}

fn sealed_text_store(path: &std::path::Path) -> Store {
    let store = Store::open(path, OpenOptions::default()).expect("open text store");
    store
        .ingest(IngestBatch::new(text_corpus()))
        .expect("ingest text corpus");
    store.seal().expect("seal text corpus");
    store
}

fn query_scores(store: &Store) -> Vec<(DocId, f64)> {
    store
        .search_lexical(
            &TermQuery::flat(vec![b"common".to_vec()], &[DEFAULT_FIELD]),
            10,
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search fixed lexical query")
        .candidates
        .into_iter()
        .map(|candidate| (candidate.document.doc_id(), candidate.score))
        .collect()
}

fn stats_tuple(stats: CorpusStats) -> (u64, u64) {
    (stats.document_count(), stats.total_tokens())
}

#[test]
fn delete_and_purge_of_the_same_rows_yield_identical_bm25_stats() {
    let deleted_directory = tempdir().expect("delete store directory");
    let purged_directory = tempdir().expect("purge store directory");
    let deleted = sealed_text_store(deleted_directory.path());
    let purged = sealed_text_store(purged_directory.path());

    deleted
        .delete(DeleteBatch::new(vec![REMOVED]))
        .expect("delete row");
    let token = purged.purge(&[REMOVED]).expect("schedule purge");
    purged.await_physical_purge(token).expect("complete purge");

    let deleted_stats = stats_tuple(
        deleted
            .lexical_corpus_stats()
            .expect("deleted corpus statistics"),
    );
    let purged_stats = stats_tuple(
        purged
            .lexical_corpus_stats()
            .expect("purged corpus statistics"),
    );
    assert_eq!(
        deleted_stats, purged_stats,
        "delete stats {deleted_stats:?}; purge stats {purged_stats:?}"
    );

    let deleted_scores = query_scores(&deleted);
    let purged_scores = query_scores(&purged);
    assert_eq!(
        deleted_scores, purged_scores,
        "delete scores {deleted_scores:?}; purge scores {purged_scores:?}"
    );
}

#[test]
fn purging_already_deleted_rows_is_a_no_op_on_bm25_stats() {
    let directory = tempdir().expect("store directory");
    let store = sealed_text_store(directory.path());
    store
        .delete(DeleteBatch::new(vec![REMOVED]))
        .expect("delete row");
    let stats_after_delete = store
        .lexical_corpus_stats()
        .expect("statistics after delete");
    let scores_after_delete = query_scores(&store);

    let token = store
        .purge(&[REMOVED])
        .expect("schedule already-deleted purge");
    store
        .await_physical_purge(token)
        .expect("complete already-deleted purge");

    assert_eq!(
        stats_tuple(
            store
                .lexical_corpus_stats()
                .expect("statistics after purge"),
        ),
        stats_tuple(stats_after_delete),
        "purging a tombstoned row changed N or total tokens"
    );
    assert_eq!(
        query_scores(&store),
        scores_after_delete,
        "purging a tombstoned row changed BM25 scores"
    );
}

#[test]
fn tombstoned_term_rows_do_not_change_bm25_at_physical_purge() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open text store");
    let documents = [
        (1_u128, "common removed one"),
        (2, "common removed two"),
        (3, "common survivor"),
        (4, "other survivor"),
    ]
    .into_iter()
    .map(|(id, text)| {
        IngestDocument::new(
            DocumentVersion::new(DocId::new(id), Revision::new(1)),
            vec![id as f32, 1.0],
        )
        .with_text(text)
    })
    .collect();
    store
        .ingest(IngestBatch::new(documents))
        .expect("ingest shared-term corpus");
    store.seal().expect("seal shared-term corpus");

    let removed = [DocId::new(1), DocId::new(2)];
    store
        .delete(DeleteBatch::new(removed.to_vec()))
        .expect("tombstone shared-term rows");
    let score_after_delete = query_scores(&store);

    let token = store.purge(&removed).expect("schedule physical purge");
    store
        .await_physical_purge(token)
        .expect("complete physical purge");

    assert_eq!(
        query_scores(&store),
        score_after_delete,
        "physical purge changed the surviving document's BM25 score"
    );
}
