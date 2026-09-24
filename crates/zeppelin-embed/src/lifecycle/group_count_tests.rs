#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};

use rand::Rng;
use tempfile::tempdir;

use crate::ingest::{DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
use crate::meta::{ColumnDefinition, ColumnId, ColumnType, Predicate, PredicateValue, Schema};

use super::{
    DocumentGroup, DocumentGroupCounts, DocumentGroupValue, MAX_DOCUMENT_GROUP_LIMIT, OpenOptions,
    QueryError, Store, StoreError,
};

const FOLDER: ColumnId = ColumnId::new(1);
const SPEAKER: ColumnId = ColumnId::new(2);
const DAY: ColumnId = ColumnId::new(3);
const SIZE: ColumnId = ColumnId::new(4);
const SCORE: ColumnId = ColumnId::new(5);
const PINNED: ColumnId = ColumnId::new(6);

fn schema() -> Schema {
    Schema::new(vec![
        ColumnDefinition::new(FOLDER, "folder", ColumnType::DictionaryString, true),
        ColumnDefinition::new(SPEAKER, "speaker", ColumnType::RawString, true),
        ColumnDefinition::new(DAY, "day", ColumnType::I64, true),
        ColumnDefinition::new(SIZE, "size", ColumnType::U64, true),
        ColumnDefinition::new(SCORE, "score", ColumnType::F64, true),
        ColumnDefinition::new(PINNED, "pinned", ColumnType::Bool, true),
    ])
    .expect("schema")
}

fn open(path: &std::path::Path) -> Store {
    Store::open(path, OpenOptions::default().with_schema(schema())).expect("open")
}

fn document(
    id: u128,
    revision: u64,
    timestamp: i64,
    columns: Vec<(ColumnId, PredicateValue)>,
) -> IngestDocument {
    IngestDocument::new(
        DocumentVersion::new(DocId::new(id), Revision::new(revision)),
        vec![id as f32],
    )
    .with_timestamp(timestamp)
    .with_columns(columns)
}

fn folder(id: u128, revision: u64, name: Option<&str>) -> IngestDocument {
    let columns = name
        .map(|name| vec![(FOLDER, PredicateValue::String(name.to_owned()))])
        .unwrap_or_default();
    document(id, revision, id as i64, columns)
}

fn string_group(value: &str, count: u64) -> DocumentGroup {
    DocumentGroup {
        value: DocumentGroupValue::String(value.to_owned()),
        count,
    }
}

fn invalid_detail(error: QueryError) -> String {
    match error {
        QueryError::Store(StoreError::InvalidScan { detail }) => detail,
        other => panic!("expected an invalid grouped count, got {other:?}"),
    }
}

#[test]
fn grouped_count_orders_groups_by_value_and_counts_missing_separately() {
    let directory = tempdir().expect("directory");
    let store = open(directory.path());
    store
        .ingest(IngestBatch::new(vec![
            folder(1, 1, Some("work")),
            folder(2, 1, Some("home")),
            folder(3, 1, None),
        ]))
        .expect("sealed ingest");
    store.seal().expect("seal");
    store
        .ingest(IngestBatch::new(vec![
            folder(4, 1, Some("work")),
            folder(5, 1, Some("")),
            folder(6, 1, None),
        ]))
        .expect("active ingest");

    let result = store
        .count_documents_grouped(None, None, FOLDER, 16)
        .expect("grouped count");
    let plain = store.count_documents(None, None).expect("plain count");

    assert_eq!(
        result,
        DocumentGroupCounts {
            groups: vec![
                string_group("", 1),
                string_group("home", 1),
                string_group("work", 2),
            ],
            missing: 2,
            count: 6,
            generation: plain.generation,
        }
    );
}

#[test]
fn grouped_count_never_double_counts_replaced_or_deleted_rows() {
    let directory = tempdir().expect("directory");
    let store = open(directory.path());
    store
        .ingest(IngestBatch::new(vec![
            folder(1, 1, Some("inbox")),
            folder(2, 1, Some("inbox")),
            folder(3, 1, Some("inbox")),
        ]))
        .expect("sealed ingest");
    store.seal().expect("seal");
    store
        .ingest(IngestBatch::new(vec![folder(1, 2, Some("archive"))]))
        .expect("replace into active");
    store
        .delete(DeleteBatch::new(vec![DocId::new(2)]))
        .expect("delete sealed row");
    store
        .ingest(IngestBatch::new(vec![folder(4, 1, Some("inbox"))]))
        .expect("active ingest");
    store
        .delete(DeleteBatch::new(vec![DocId::new(4)]))
        .expect("delete active row");

    let result = store
        .count_documents_grouped(None, None, FOLDER, 16)
        .expect("grouped count");

    assert_eq!(
        result.groups,
        vec![string_group("archive", 1), string_group("inbox", 1)]
    );
    assert_eq!((result.missing, result.count), (0, 2));
}

#[test]
fn grouped_count_applies_the_filter_and_timestamp_range_of_plain_count() {
    let directory = tempdir().expect("directory");
    let store = open(directory.path());
    let row = |id: u128, day: i64, speaker: &str| {
        document(
            id,
            1,
            id as i64 * 10,
            vec![
                (DAY, PredicateValue::I64(day)),
                (SPEAKER, PredicateValue::String(speaker.to_owned())),
            ],
        )
    };
    store
        .ingest(IngestBatch::new(vec![
            row(1, -2, "ana"),
            row(2, 5, "bo"),
            row(3, -2, "bo"),
            row(4, 7, "ana"),
        ]))
        .expect("ingest");
    let ana = Predicate::Eq {
        column: SPEAKER,
        value: PredicateValue::String("ana".to_owned()),
    };

    let by_day = store
        .count_documents_grouped(Some(&ana), None, DAY, 16)
        .expect("filtered");
    let in_range = store
        .count_documents_grouped(None, Some((20, 40)), DAY, 16)
        .expect("ranged");

    assert_eq!(
        by_day.groups,
        vec![
            DocumentGroup {
                value: DocumentGroupValue::I64(-2),
                count: 1,
            },
            DocumentGroup {
                value: DocumentGroupValue::I64(7),
                count: 1,
            },
        ]
    );
    assert_eq!(
        in_range.groups,
        vec![
            DocumentGroup {
                value: DocumentGroupValue::I64(-2),
                count: 1,
            },
            DocumentGroup {
                value: DocumentGroupValue::I64(5),
                count: 1,
            },
        ]
    );
}

#[test]
fn grouped_count_fails_when_distinct_values_exceed_the_limit() {
    let directory = tempdir().expect("directory");
    let store = open(directory.path());
    store
        .ingest(IngestBatch::new(vec![
            folder(1, 1, Some("a")),
            folder(2, 1, Some("b")),
            folder(3, 1, Some("b")),
            folder(4, 1, None),
        ]))
        .expect("sealed");
    store.seal().expect("seal");
    store
        .ingest(IngestBatch::new(vec![folder(5, 1, Some("c"))]))
        .expect("active");

    let exact = store
        .count_documents_grouped(None, None, FOLDER, 3)
        .expect("limit equals distinct values");
    let error = store
        .count_documents_grouped(None, None, FOLDER, 2)
        .expect_err("one more distinct value than the limit");

    assert_eq!(exact.groups.len(), 3);
    assert!(
        matches!(
            error,
            QueryError::Store(StoreError::GroupLimitExceeded { limit: 2 })
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "grouped count found more than 2 distinct values; raise the group limit"
    );
    let day = |id: u128, value: i64| document(id, 1, 0, vec![(DAY, PredicateValue::I64(value))]);
    store
        .ingest(IngestBatch::new(vec![day(6, 1), day(7, 2), day(8, 2)]))
        .expect("integer rows");
    assert_eq!(
        store
            .count_documents_grouped(None, None, DAY, 2)
            .expect("integer limit equals distinct values")
            .groups
            .len(),
        2
    );
    assert!(matches!(
        store.count_documents_grouped(None, None, DAY, 1),
        Err(QueryError::Store(StoreError::GroupLimitExceeded {
            limit: 1
        }))
    ));
}

#[test]
fn grouped_count_rejects_bad_limits_unknown_attributes_and_unsupported_types() {
    let directory = tempdir().expect("directory");
    let store = open(directory.path());

    assert_eq!(
        invalid_detail(
            store
                .count_documents_grouped(None, None, FOLDER, 0)
                .expect_err("zero limit")
        ),
        format!("group limit must be in 1..={MAX_DOCUMENT_GROUP_LIMIT}, received 0")
    );
    assert!(
        store
            .count_documents_grouped(None, None, FOLDER, MAX_DOCUMENT_GROUP_LIMIT + 1)
            .is_err()
    );
    assert_eq!(
        invalid_detail(
            store
                .count_documents_grouped(None, None, ColumnId::new(99), 4)
                .expect_err("unknown attribute")
        ),
        "group-by attribute 99 is not in the schema"
    );
    for (column, type_name) in [(SCORE, "F64"), (PINNED, "Bool")] {
        assert_eq!(
            invalid_detail(
                store
                    .count_documents_grouped(None, None, column, 4)
                    .expect_err("unsupported type")
            ),
            format!(
                "group-by attribute {} has type {type_name}; grouping supports \
                 U64, I64, DictionaryString and RawString",
                column.get()
            )
        );
    }
}

#[test]
fn grouped_count_matches_a_count_oracle_across_sealed_and_active_rows() {
    let mut random = crate::test_support::seeded_rng(
        "grouped_count_matches_a_count_oracle_across_sealed_and_active_rows",
    );
    let directory = tempdir().expect("directory");
    let store = open(directory.path());
    let folders = ["a", "b", "c", "d", "e"];
    let mut revisions = BTreeMap::<u128, u64>::new();
    let mut live = BTreeSet::<u128>::new();
    let (mut saw_groups, mut saw_missing) = (false, false);
    for round in 0..6 {
        let mut batch = Vec::new();
        for _ in 0..random.random_range(1..24) {
            let id = random.random_range(0..40_u128);
            if batch
                .iter()
                .any(|document: &IngestDocument| document.version().doc_id() == DocId::new(id))
            {
                continue;
            }
            let revision = revisions.get(&id).copied().unwrap_or(0) + 1;
            revisions.insert(id, revision);
            live.insert(id);
            let mut columns = Vec::new();
            if random.random_bool(0.8) {
                let name = folders[random.random_range(0..folders.len())];
                columns.push((FOLDER, PredicateValue::String(name.to_owned())));
                columns.push((SPEAKER, PredicateValue::String(name.repeat(2))));
            }
            if random.random_bool(0.8) {
                columns.push((DAY, PredicateValue::I64(random.random_range(-3..4))));
                columns.push((SIZE, PredicateValue::U64(random.random_range(0..5))));
            }
            batch.push(document(id, revision, random.random_range(0..100), columns));
        }
        store.ingest(IngestBatch::new(batch)).expect("ingest");
        let deletes: Vec<DocId> = live
            .iter()
            .filter(|_| random.random_bool(0.1))
            .map(|id| DocId::new(*id))
            .collect();
        if !deletes.is_empty() {
            for id in &deletes {
                live.remove(&id.get());
            }
            store.delete(DeleteBatch::new(deletes)).expect("delete");
        }
        if round % 2 == 0 {
            store.seal().expect("seal");
        }

        let filter = Predicate::Range(crate::meta::RangePredicate {
            column: crate::meta::TIMESTAMP_COLUMN,
            lower: Some(crate::meta::RangeBound::inclusive(PredicateValue::I64(
                random.random_range(0..50),
            ))),
            upper: None,
        });
        for predicate in [None, Some(&filter)] {
            for column in [FOLDER, SPEAKER, DAY, SIZE] {
                let grouped = store
                    .count_documents_grouped(predicate, None, column, 16)
                    .expect("grouped");
                let plain = store.count_documents(predicate, None).expect("plain");
                let sum: u64 = grouped.groups.iter().map(|group| group.count).sum();
                saw_groups |= grouped.groups.len() > 2;
                saw_missing |= grouped.missing > 0;
                assert_eq!(sum + grouped.missing, plain.count);
                assert_eq!(grouped.count, plain.count);
                assert_eq!(grouped.generation, plain.generation);
                let values: Vec<&DocumentGroupValue> =
                    grouped.groups.iter().map(|group| &group.value).collect();
                assert!(values.windows(2).all(|pair| pair[0] < pair[1]));
                for group in &grouped.groups {
                    let value = match &group.value {
                        DocumentGroupValue::U64(value) => PredicateValue::U64(*value),
                        DocumentGroupValue::I64(value) => PredicateValue::I64(*value),
                        DocumentGroupValue::String(value) => PredicateValue::String(value.clone()),
                    };
                    let equal = Predicate::Eq { column, value };
                    let both;
                    let oracle = match predicate {
                        Some(predicate) => {
                            both = Predicate::And(vec![predicate.clone(), equal]);
                            &both
                        }
                        None => &equal,
                    };
                    assert_eq!(
                        store
                            .count_documents(Some(oracle), None)
                            .expect("oracle")
                            .count,
                        group.count,
                        "group {:?} of column {}",
                        group.value,
                        column.get()
                    );
                    assert!(group.count > 0);
                }
                let null = Predicate::IsNull(column);
                let missing_oracle = match predicate {
                    Some(predicate) => Predicate::And(vec![predicate.clone(), null]),
                    None => null,
                };
                assert_eq!(
                    store
                        .count_documents(Some(&missing_oracle), None)
                        .expect("missing oracle")
                        .count,
                    grouped.missing
                );
            }
        }
    }
    assert!(
        saw_groups && saw_missing,
        "the oracle run must exercise both"
    );
}
