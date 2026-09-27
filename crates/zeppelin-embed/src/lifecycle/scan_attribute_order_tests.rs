#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::collections::BTreeMap;

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestRunner};
use rand::RngCore;
use tempfile::tempdir;

use crate::ingest::{DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
use crate::meta::{ColumnDefinition, ColumnId, ColumnType, PredicateValue, Schema};

use super::{
    CancelToken, DocumentFields, DocumentScanRequest, OpenOptions, QueryControl, QueryError,
    ScanDirection, ScanOrder, Store, StoreError,
};

const U: ColumnId = ColumnId::new(1);
const I: ColumnId = ColumnId::new(2);
const F: ColumnId = ColumnId::new(3);
const FLAG: ColumnId = ColumnId::new(4);
const NAME: ColumnId = ColumnId::new(5);

fn schema() -> Schema {
    Schema::new(vec![
        ColumnDefinition::new(U, "u", ColumnType::U64, true),
        ColumnDefinition::new(I, "i", ColumnType::I64, true),
        ColumnDefinition::new(F, "f", ColumnType::F64, true),
        ColumnDefinition::new(FLAG, "flag", ColumnType::Bool, true),
        ColumnDefinition::new(NAME, "name", ColumnType::RawString, true),
    ])
    .expect("schema")
}

fn open(directory: &std::path::Path) -> Store {
    Store::open(directory, OpenOptions::default().with_schema(schema())).expect("open")
}

fn document(id: u128, revision: u64, columns: Vec<(ColumnId, PredicateValue)>) -> IngestDocument {
    IngestDocument::new(
        DocumentVersion::new(DocId::new(id), Revision::new(revision)),
        vec![id as f32],
    )
    .with_timestamp(0)
    .with_columns(columns)
}

fn request(limit: usize) -> DocumentScanRequest<'static> {
    DocumentScanRequest::new(
        limit,
        DocumentFields::NONE,
        QueryControl::Cancel(CancelToken::new()),
    )
}

fn by(column: ColumnId, direction: ScanDirection) -> ScanOrder {
    ScanOrder::Attribute { column, direction }
}

fn scan_all(store: &Store, order: ScanOrder, limit: usize) -> Vec<u128> {
    let mut cursor = None;
    let mut ids = Vec::new();
    loop {
        let mut scan = request(limit).with_order(order);
        if let Some(next) = cursor {
            scan = scan.with_cursor(next);
        }
        let page = store.scan_documents(scan).expect("page");
        assert!(page.documents.len() <= limit);
        ids.extend(page.documents.iter().map(|document| document.doc_id.get()));
        cursor = page.continuation;
        if cursor.is_none() {
            return ids;
        }
    }
}

fn invalid_detail(result: Result<super::DocumentScanPage, QueryError>) -> String {
    match result {
        Err(QueryError::Store(StoreError::InvalidScan { detail })) => detail,
        other => panic!("expected InvalidScan, received {other:?}"),
    }
}

#[test]
fn attribute_order_sorts_each_numeric_type_with_doc_id_ties_and_unorderable_rows_last() {
    let directory = tempdir().expect("directory");
    let store = open(directory.path());
    store
        .ingest(IngestBatch::new(vec![
            document(
                5,
                1,
                vec![
                    (U, PredicateValue::U64(u64::MAX)),
                    (I, PredicateValue::I64(-7)),
                    (F, PredicateValue::F64(-0.0)),
                ],
            ),
            document(
                3,
                1,
                vec![
                    (U, PredicateValue::U64(2)),
                    (I, PredicateValue::I64(i64::MIN)),
                    (F, PredicateValue::F64(f64::NAN)),
                ],
            ),
        ]))
        .expect("sealed ingest");
    store.seal().expect("seal");
    store
        .ingest(IngestBatch::new(vec![
            document(
                4,
                1,
                vec![
                    (U, PredicateValue::U64(2)),
                    (I, PredicateValue::I64(9)),
                    (F, PredicateValue::F64(0.0)),
                ],
            ),
            document(1, 1, Vec::new()),
            document(
                2,
                1,
                vec![
                    (U, PredicateValue::U64(0)),
                    (I, PredicateValue::I64(-7)),
                    (F, PredicateValue::F64(f64::NEG_INFINITY)),
                ],
            ),
        ]))
        .expect("active ingest");

    let cases = [
        (U, ScanDirection::Ascending, vec![2, 3, 4, 5, 1]),
        (U, ScanDirection::Descending, vec![5, 3, 4, 2, 1]),
        (I, ScanDirection::Ascending, vec![3, 2, 5, 4, 1]),
        (I, ScanDirection::Descending, vec![4, 2, 5, 3, 1]),
        // -0.0 equals +0.0, so doc 4 and doc 5 tie on the value and break
        // by doc id; NaN (doc 3) and missing (doc 1) sort last by doc id.
        (F, ScanDirection::Ascending, vec![2, 4, 5, 1, 3]),
        (F, ScanDirection::Descending, vec![4, 5, 2, 1, 3]),
    ];
    for (column, direction, expected) in cases {
        for limit in [1, 2, 5, 10] {
            assert_eq!(
                scan_all(&store, by(column, direction), limit),
                expected,
                "column {column:?} {direction:?} limit {limit}"
            );
        }
    }
}

#[test]
fn attribute_order_rejects_undeclared_timestamp_and_non_numeric_attributes() {
    let directory = tempdir().expect("directory");
    let store = open(directory.path());
    let cases = [
        (
            ColumnId::new(9),
            "scan order attribute 9 is not a declared attribute",
        ),
        (
            ColumnId::new(0),
            "scan order attribute 0 is the document timestamp; use a timestamp order",
        ),
        (
            FLAG,
            "scan order attribute 4 has type Bool; only u64, i64 and f64 attributes are orderable",
        ),
        (
            NAME,
            "scan order attribute 5 has type RawString; only u64, i64 and f64 attributes are orderable",
        ),
    ];
    for (column, expected) in cases {
        let detail = invalid_detail(
            store.scan_documents(request(10).with_order(by(column, ScanDirection::Ascending))),
        );
        assert_eq!(detail, expected);
    }
}

#[test]
fn a_cursor_issued_under_a_different_order_is_rejected() {
    let directory = tempdir().expect("directory");
    let store = open(directory.path());
    store
        .ingest(IngestBatch::new(vec![
            document(
                1,
                1,
                vec![(U, PredicateValue::U64(1)), (I, PredicateValue::I64(1))],
            ),
            document(
                2,
                1,
                vec![(U, PredicateValue::U64(2)), (I, PredicateValue::I64(2))],
            ),
            document(
                3,
                1,
                vec![(U, PredicateValue::U64(3)), (I, PredicateValue::I64(3))],
            ),
        ]))
        .expect("ingest");
    let issued = by(U, ScanDirection::Ascending);
    let cursor = store
        .scan_documents(request(1).with_order(issued))
        .expect("first page")
        .continuation
        .expect("cursor");

    for other in [
        by(U, ScanDirection::Descending),
        by(I, ScanDirection::Ascending),
        ScanOrder::Storage,
        ScanOrder::TimestampAscending,
    ] {
        let detail =
            invalid_detail(store.scan_documents(request(1).with_order(other).with_cursor(cursor)));
        assert_eq!(
            detail,
            format!(
                "scan cursor was issued for order {issued:?}, but the request orders by {other:?}"
            )
        );
    }
    let storage_cursor = store
        .scan_documents(request(1))
        .expect("storage page")
        .continuation
        .expect("storage cursor");
    let detail = invalid_detail(
        store.scan_documents(request(1).with_order(issued).with_cursor(storage_cursor)),
    );
    assert_eq!(
        detail,
        format!(
            "scan cursor was issued for order {:?}, but the request orders by {issued:?}",
            ScanOrder::Storage
        )
    );
}

#[test]
fn attribute_order_cursor_is_stale_after_any_write_between_pages() {
    let directory = tempdir().expect("directory");
    let store = open(directory.path());
    let order = by(U, ScanDirection::Descending);
    let rows = |base: u128| {
        IngestBatch::new(vec![
            document(base, 1, vec![(U, PredicateValue::U64(1))]),
            document(base + 1, 1, vec![(U, PredicateValue::U64(2))]),
        ])
    };
    store.ingest(rows(1)).expect("ingest");
    let writes: [&dyn Fn(&Store); 3] = [
        &|store| {
            store.ingest(rows(10)).expect("ingest write");
        },
        &|store| {
            store
                .delete(DeleteBatch::new(vec![DocId::new(10)]))
                .expect("delete write");
        },
        &|store| {
            store.seal().expect("seal write");
        },
    ];
    for write in writes {
        let first = store
            .scan_documents(request(1).with_order(order))
            .expect("first page");
        let cursor = first.continuation.expect("cursor");
        write(&store);
        match store.scan_documents(request(1).with_order(order).with_cursor(cursor)) {
            Err(QueryError::Store(StoreError::ScanStale {
                cursor_generation,
                current_generation,
            })) => {
                assert_eq!(cursor_generation, first.generation);
                assert!(current_generation > cursor_generation);
            }
            other => panic!("expected ScanStale, received {other:?}"),
        }
    }
}

#[derive(Clone, Debug)]
enum Op {
    Put { id: u8, value: Option<i8> },
    Delete { id: u8 },
    Seal,
}

fn value_of(column: ColumnId, value: i8) -> PredicateValue {
    match column {
        U => PredicateValue::U64(u64::from(value.unsigned_abs())),
        I => PredicateValue::I64(i64::from(value)),
        _ if value == i8::MIN => PredicateValue::F64(f64::NAN),
        _ => PredicateValue::F64(f64::from(value) / 4.0),
    }
}

/// The documented contract written independently of the engine: orderable
/// values first in the requested direction, then missing or NaN values;
/// ties break by ascending doc id.
fn oracle(live: &BTreeMap<u8, Option<i8>>, column: ColumnId, descending: bool) -> Vec<u128> {
    let key = |value: &Option<i8>| -> Option<f64> {
        match value.map(|value| value_of(column, value)) {
            Some(PredicateValue::U64(value)) => Some(value as f64),
            Some(PredicateValue::I64(value)) => Some(value as f64),
            Some(PredicateValue::F64(value)) if !value.is_nan() => Some(value),
            _ => None,
        }
    };
    let mut rows = live
        .iter()
        .map(|(id, value)| (u128::from(*id), key(value)))
        .collect::<Vec<_>>();
    rows.sort_by(|(left_id, left), (right_id, right)| {
        let value = match (left, right) {
            (Some(left), Some(right)) if descending => right.total_cmp(left),
            (Some(left), Some(right)) => left.total_cmp(right),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        };
        value.then(left_id.cmp(right_id))
    });
    rows.into_iter().map(|(id, _)| id).collect()
}

#[test]
fn attribute_order_pagination_matches_a_sort_oracle() {
    let mut seeded = crate::test_support::seeded_rng(
        "lifecycle::scan_attribute_order_tests::attribute_order_pagination_matches_a_sort_oracle",
    );
    let mut runner = TestRunner::new(Config {
        cases: 48,
        rng_seed: RngSeed::Fixed(seeded.next_u64()),
        ..Config::default()
    });
    let op = prop_oneof![
        6 => (0_u8..24, proptest::option::weighted(0.8, -6_i8..=6)
            .prop_map(|value| value.map(|value| if value == -6 { i8::MIN } else { value })))
            .prop_map(|(id, value)| Op::Put { id, value }),
        1 => (0_u8..24).prop_map(|id| Op::Delete { id }),
        1 => Just(Op::Seal),
    ];
    let strategy = (
        proptest::collection::vec(op, 1..60),
        prop_oneof![Just(U), Just(I), Just(F)],
        any::<bool>(),
        1_usize..8,
    );
    let result = runner.run(&strategy, |(ops, column, descending, limit)| {
        let directory = tempdir().expect("directory");
        let store = open(directory.path());
        let mut live = BTreeMap::<u8, Option<i8>>::new();
        let mut revisions = BTreeMap::<u8, u64>::new();
        for op in ops {
            match op {
                Op::Put { id, value } => {
                    let revision = revisions.entry(id).or_insert(0);
                    *revision += 1;
                    let columns = value
                        .map(|value| vec![(column, value_of(column, value))])
                        .unwrap_or_default();
                    store
                        .ingest(IngestBatch::new(vec![document(
                            u128::from(id),
                            *revision,
                            columns,
                        )]))
                        .expect("put");
                    live.insert(id, value);
                }
                Op::Delete { id } => {
                    if live.remove(&id).is_some() {
                        store
                            .delete(DeleteBatch::new(vec![DocId::new(u128::from(id))]))
                            .expect("delete");
                    }
                }
                Op::Seal => {
                    store.seal().expect("seal");
                }
            }
        }
        let direction = if descending {
            ScanDirection::Descending
        } else {
            ScanDirection::Ascending
        };
        prop_assert_eq!(
            scan_all(&store, by(column, direction), limit),
            oracle(&live, column, descending)
        );
        Ok(())
    });
    assert!(result.is_ok(), "property result: {result:?}");
}
