#![allow(clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
#[path = "../../../scripts/fixtures/common.rs"]
mod common;
use std::path::Path;
use zeppelin_embed::ingest::DocId;
use zeppelin_embed::lifecycle::{DocumentFields, Store};
use zeppelin_embed::meta::{ColumnId, PredicateValue};

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("copy directory");
    for entry in std::fs::read_dir(from).expect("fixture must exist") {
        let entry = entry.expect("entry");
        if entry.file_type().expect("type").is_dir() {
            copy_tree(&entry.path(), &to.join(entry.file_name()));
        } else {
            std::fs::copy(entry.path(), to.join(entry.file_name())).expect("copy file");
        }
    }
}
// The fixture oracle is intentionally a flat JSON object: decimal integers,
// integer arrays and unescaped ASCII strings. Reject anything outside that schema.
fn value<'a>(json: &'a str, key: &str) -> &'a str {
    let prefix = format!("\"{key}\": ");
    let mut values = json
        .lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix(&prefix));
    let value = values.next().expect("expected key").trim_end_matches(',');
    assert!(values.next().is_none(), "duplicate key");
    value
}
fn ids(json: &str, key: &str) -> Vec<u128> {
    let inner = value(json, key)
        .strip_prefix('[')
        .expect("array")
        .strip_suffix(']')
        .expect("array end");
    if inner.is_empty() {
        vec![]
    } else {
        inner
            .split(',')
            .map(|id| id.trim().parse().expect("decimal id"))
            .collect()
    }
}
fn check(store: &Store, json: &str, appended: bool) {
    for id in ids(json, "live_ids") {
        let rows = store
            .get_documents(&[DocId::new(id)], DocumentFields::ALL)
            .expect("get document");
        let row = rows.first().expect("row").as_ref().expect("live row");
        let text = value(json, &format!("document_{id}"))
            .strip_prefix('"')
            .expect("string")
            .strip_suffix('"')
            .expect("string end");
        assert_eq!(row.text.as_deref(), Some(text));
        assert_eq!(row.vector, Some(common::vector(id)));
        assert_eq!(row.metadata, Some(format!("metadata-{id}").into_bytes()));
        assert_eq!(
            row.attributes,
            Some(vec![(ColumnId::new(1), PredicateValue::U64(id as u64))])
        );
        assert_eq!(row.timestamp, id as i64);
    }
    for id in ids(json, "tombstoned_ids") {
        assert!(
            store
                .get_documents(&[DocId::new(id)], DocumentFields::ALL)
                .expect("deleted row")
                .first()
                .expect("row")
                .is_none()
        );
    }
    for query in ["orchard", "harbor", "deleted"] {
        assert_eq!(
            common::text_hits(store, query),
            ids(json, &format!("query_{query}")),
            "{query}"
        );
    }
    assert_eq!(common::vector_hits(store), ids(json, "query_vector"));
    let generation: u64 = value(json, "generation").parse().expect("generation");
    assert_eq!(
        store.count_documents(None, None).expect("count").generation,
        generation + u64::from(appended)
    );
    if appended {
        assert_eq!(common::text_hits(store, "zebra"), vec![100]);
    }
}
#[test]
fn every_release_fixture_opens_read_only_and_answers_expected_queries() {
    for tag in ["v0.4.2", "v0.5.0", "v0.6.0"] {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/releases")
            .join(tag);
        let scratch = tempfile::tempdir().expect("scratch");
        copy_tree(&fixture, scratch.path());
        let json = std::fs::read_to_string(scratch.path().join("expected.json")).expect("oracle");
        let path = scratch.path();
        for read_only in [true, false] {
            let store = Store::open(&path, common::options(read_only))
                .unwrap_or_else(|error| panic!("{tag} read_only={read_only}: {error:?}"));
            check(&store, &json, false);
            if !read_only {
                store
                    .ingest(common::batch(vec![
                        zeppelin_embed::ingest::IngestDocument::new(
                            zeppelin_embed::ingest::DocumentVersion::new(
                                DocId::new(100),
                                zeppelin_embed::ingest::Revision::new(1),
                            ),
                            common::vector(0),
                        )
                        .with_text("zebra")
                        .with_columns(vec![(ColumnId::new(1), PredicateValue::U64(100))]),
                    ]))
                    .expect("append");
            }
            store.close().expect("close");
        }
        let reopened = Store::open(&path, common::options(true)).expect("reopen appended store");
        check(&reopened, &json, true);
        reopened.close().expect("close reopened");
    }
}

fn file_hashes(root: &Path) -> std::collections::BTreeMap<std::path::PathBuf, u64> {
    fn visit(
        root: &Path,
        directory: &Path,
        hashes: &mut std::collections::BTreeMap<std::path::PathBuf, u64>,
    ) {
        for entry in std::fs::read_dir(directory).expect("hash directory") {
            let entry = entry.expect("entry");
            if entry.file_type().expect("type").is_dir() {
                visit(root, &entry.path(), hashes);
            } else {
                let bytes = std::fs::read(entry.path()).expect("hash file");
                hashes.insert(
                    entry
                        .path()
                        .strip_prefix(root)
                        .expect("relative path")
                        .to_owned(),
                    xxhash_rust::xxh3::xxh3_64(&bytes),
                );
            }
        }
    }
    let mut hashes = std::collections::BTreeMap::new();
    visit(root, root, &mut hashes);
    hashes
}

#[test]
#[ignore = "documents ZE-370; flip to a passing relocation test when fixed"]
fn a_relocated_0_6_0_namespace_store_is_refused_today() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/releases/v0.6.0-namespaces");
    let scratch = tempfile::tempdir().expect("scratch");
    copy_tree(&fixture, scratch.path());
    let before = file_hashes(scratch.path());
    for name in ["a", "b"] {
        // Preserve the release bytes: ZE-370 binds both references and op 9
        // participant identities to the original canonical root.
        let error = match Store::open(scratch.path().join(name), common::options(true)) {
            Err(error) => error,
            Ok(_) => panic!("relocated namespace unexpectedly opened"),
        };
        match error {
            zeppelin_embed::lifecycle::StoreError::Io { source, .. } => {
                assert_eq!(source.kind(), std::io::ErrorKind::InvalidData);
                assert_eq!(
                    source.to_string(),
                    "namespace requires its original transaction root"
                );
            }
            zeppelin_embed::lifecycle::StoreError::WalMutation {
                op: 9,
                source: zeppelin_embed::ingest::wal_payload::PayloadError::TransactionBinding,
                ..
            } => {}
            error => panic!("unexpected refusal: {error:?}"),
        }
        assert_eq!(
            file_hashes(scratch.path()),
            before,
            "failed read-only open modified the copy"
        );
    }
}
