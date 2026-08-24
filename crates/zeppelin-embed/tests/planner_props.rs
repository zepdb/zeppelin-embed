#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use tempfile::tempdir;
use zeppelin_embed::fts::bm25::Bm25Params;
use zeppelin_embed::fts::index::{DEFAULT_FIELD, Document, LexicalIndex, SegmentIndex};
use zeppelin_embed::fts::search::TermQuery;
use zeppelin_embed::fts::tokenizer::{Analyzer, Profile};
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, SearchOptions, Store};
use zeppelin_embed::meta::{
    ColumnId, DocBitmap, Predicate, PredicateValue, RangeBound, RangePredicate, TIMESTAMP_COLUMN,
};
use zeppelin_embed::planner::{
    FilteredSearchError, LexicalBranch, PlanError, search_lexical_filtered, segment_may_match,
};
use zeppelin_embed::segment::ClusteringKeyRange;

fn populated_store() -> (tempfile::TempDir, Store) {
    let directory = tempdir().expect("planner store");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open planner store");
    let documents = (0_u32..8)
        .map(|row| {
            IngestDocument::new(
                DocumentVersion::new(DocId::new(u128::from(row + 1)), Revision::new(1)),
                vec![row as f32, 1.0],
            )
            .with_timestamp(i64::from(row))
        })
        .collect::<Vec<_>>();
    store
        .ingest(IngestBatch::new(documents))
        .expect("planner ingest");
    (directory, store)
}

fn query(
    store: &Store,
    predicate: &Predicate,
) -> Result<zeppelin_embed::planner::FilteredSearchOutcome, FilteredSearchError> {
    store.search_filtered(
        SearchRequest::new(&[0.0, 0.0]),
        predicate,
        8,
        SearchOptions::default(),
        QueryControl::Cancel(CancelToken::new()),
    )
}

#[test]
fn pruning_by_clustering_range_never_drops_a_matching_segment() {
    let straddling = ClusteringKeyRange::Bounded {
        min_ts: 10,
        max_ts: 20,
    };
    for predicate in [
        Predicate::Range(RangePredicate {
            column: TIMESTAMP_COLUMN,
            lower: Some(RangeBound::inclusive(PredicateValue::I64(20))),
            upper: None,
        }),
        Predicate::Range(RangePredicate {
            column: TIMESTAMP_COLUMN,
            lower: None,
            upper: Some(RangeBound::inclusive(PredicateValue::I64(10))),
        }),
        Predicate::Eq {
            column: TIMESTAMP_COLUMN,
            value: PredicateValue::I64(15),
        },
    ] {
        assert!(segment_may_match(straddling, &predicate));
    }
    assert!(segment_may_match(
        ClusteringKeyRange::Unstamped,
        &Predicate::IsNull(TIMESTAMP_COLUMN)
    ));
}

#[test]
fn plan_branch_is_deterministic_for_fixed_stats() {
    let (_directory, store) = populated_store();
    let predicate = Predicate::Range(RangePredicate {
        column: TIMESTAMP_COLUMN,
        lower: Some(RangeBound::inclusive(PredicateValue::I64(2))),
        upper: Some(RangeBound::exclusive(PredicateValue::I64(7))),
    });
    let first = query(&store, &predicate).expect("first plan");
    let second = query(&store, &predicate).expect("second plan");
    assert_eq!(first.plans, second.plans);
    store.close().expect("close deterministic store");
}

#[test]
fn an_unknown_column_is_rejected_at_plan_time_with_a_typed_error() {
    let (_directory, store) = populated_store();
    let error = query(
        &store,
        &Predicate::Eq {
            column: ColumnId::new(99),
            value: PredicateValue::U64(1),
        },
    )
    .expect_err("unknown column must be rejected");
    assert!(matches!(
        error,
        FilteredSearchError::Plan(PlanError::UnknownColumn(column))
            if column == ColumnId::new(99)
    ));
    store.close().expect("close rejection store");
}

#[test]
fn a_type_mismatched_value_is_rejected_at_plan_time() {
    let (_directory, store) = populated_store();
    let error = query(
        &store,
        &Predicate::Eq {
            column: TIMESTAMP_COLUMN,
            value: PredicateValue::Bool(true),
        },
    )
    .expect_err("type mismatch must be rejected");
    assert!(matches!(
        error,
        FilteredSearchError::Plan(PlanError::TypeMismatch { column, .. })
            if column == TIMESTAMP_COLUMN
    ));
    store.close().expect("close mismatch store");
}

#[test]
fn a_filtered_query_over_the_active_segment_matches_the_oracle() {
    let (_directory, store) = populated_store();
    store
        .delete(DeleteBatch::new(vec![DocId::new(3)]))
        .expect("tombstone active row");
    let outcome = query(
        &store,
        &Predicate::Range(RangePredicate {
            column: TIMESTAMP_COLUMN,
            lower: Some(RangeBound::inclusive(PredicateValue::I64(0))),
            upper: Some(RangeBound::exclusive(PredicateValue::I64(5))),
        }),
    )
    .expect("filtered active search");
    let rows = outcome
        .candidates
        .iter()
        .map(|candidate| candidate.row_id().local_row())
        .collect::<Vec<_>>();
    assert_eq!(rows, vec![0, 1, 3, 4]);
    store.close().expect("close active store");
}

#[test]
fn the_lexical_allow_list_drive_and_post_check_paths_agree_exactly() {
    let analyzer = Analyzer::new(Profile::Code.config()).expect("lexical analyzer");
    let mut segment = SegmentIndex::new();
    for row in 0_u32..160 {
        let text = if row.is_multiple_of(5) {
            "filter planner exact engine rare"
        } else if row.is_multiple_of(2) {
            "filter planner exact engine"
        } else {
            "filter planner"
        };
        segment
            .push_document(&analyzer, &Document::with_text(text))
            .expect("lexical row");
    }
    let mut index = LexicalIndex::new();
    index.push_segment(segment).expect("seal lexical segment");
    let query = TermQuery::flat(
        vec![
            b"filter".to_vec(),
            b"planner".to_vec(),
            b"exact".to_vec(),
            b"engine".to_vec(),
        ],
        &[DEFAULT_FIELD],
    );
    let allow_lists = vec![DocBitmap::from_ids((0_u32..160).filter(|row| row % 3 == 0))];
    let drive = search_lexical_filtered(
        &index,
        &query,
        20,
        Bm25Params::default(),
        &allow_lists,
        Some(LexicalBranch::AllowListDrive),
    )
    .expect("allow-list-driven search");
    let post = search_lexical_filtered(
        &index,
        &query,
        20,
        Bm25Params::default(),
        &allow_lists,
        Some(LexicalBranch::PostCheck),
    )
    .expect("post-check search");
    assert_eq!(drive.result.hits, post.result.hits);
    assert_eq!(drive.branch, LexicalBranch::AllowListDrive);
    assert_eq!(post.branch, LexicalBranch::PostCheck);
}
