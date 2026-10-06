#![allow(clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
#[allow(dead_code)]
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
            let store = Store::open(path, common::options(read_only))
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
        let reopened = Store::open(path, common::options(true)).expect("reopen appended store");
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
#[ignore = "legacy roots copied without explicit conversion remain path-bound"]
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

#[test]
fn a_relocated_0_6_0_namespace_store_opens_after_explicit_conversion() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/releases/v0.6.0-namespaces");
    let fixture_before = file_hashes(&fixture);
    let scratch = tempfile::tempdir().expect("scratch");
    let source = scratch.path().join("source");
    copy_tree(&fixture, &source);
    let source_before = file_hashes(&source);
    let json = std::fs::read_to_string(source.join("expected.json")).expect("oracle");
    let destination = scratch.path().join("destination");
    zeppelin_embed::lifecycle::namespace_relocate(&source, &destination).expect("convert");
    for name in ["a", "b"] {
        let store = Store::open(destination.join(name), common::options(true))
            .expect("explicitly converted participant opens");
        check(&store, &json, false);
        store.close().expect("close");
    }
    let relocated = scratch.path().join("relocated-again");
    std::fs::rename(&destination, &relocated).expect("whole-root rename");
    for name in ["a", "b"] {
        let store =
            Store::open(relocated.join(name), common::options(true)).expect("relocated reader");
        check(&store, &json, false);
        store.close().expect("close reader");
    }
    let generations = zeppelin_embed::lifecycle::namespace_batch(
        &relocated,
        ["a", "b"]
            .into_iter()
            .map(|name| zeppelin_embed::lifecycle::NamespaceMutation {
                name: name.into(),
                options: common::options(false),
                upserts: vec![
                    zeppelin_embed::ingest::IngestDocument::new(
                        zeppelin_embed::ingest::DocumentVersion::new(
                            DocId::new(100),
                            zeppelin_embed::ingest::Revision::new(1),
                        ),
                        common::vector(0),
                    )
                    .with_text("zebra")
                    .with_columns(vec![(ColumnId::new(1), PredicateValue::U64(100))]),
                ],
                deletes: vec![],
                delete_where: None,
            })
            .collect(),
    )
    .expect("writable batch");
    assert_eq!(generations, vec![7, 7]);
    for name in ["a", "b"] {
        let store = Store::open(relocated.join(name), common::options(true)).expect("reopen batch");
        check(&store, &json, true);
        store.close().expect("close reopened");
    }
    assert_eq!(file_hashes(&source), source_before);
    assert_eq!(file_hashes(&fixture), fixture_before);
}

fn old_reader() -> &'static std::path::PathBuf {
    static READER: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    READER.get_or_init(|| {
        assert_eq!(
            std::env::var("ZE_FORMAT_COMPAT").as_deref(),
            Ok("1"),
            "set ZE_FORMAT_COMPAT=1 for release-reader tests"
        );
        let scratch = tempfile::tempdir().expect("reader scratch").keep();
        let binary = scratch.join("old-reader");
        let status = std::process::Command::new("bash")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../scripts/fixtures/build-old-reader.sh"
            ))
            .arg(&binary)
            .status()
            .expect("build old reader");
        assert!(status.success(), "old-reader build failed");
        binary
    })
}

fn run_old_reader(path: &Path, mode: &str) -> String {
    let output = std::process::Command::new(old_reader())
        .arg(path)
        .arg(mode)
        .output()
        .expect("run old reader");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("reader JSON")
}

// Exercise the actual version barrier, including its empty catalog object.
#[cfg(feature = "graph-cypher")]
fn mint_v3_manifest(path: &Path) {
    let store = Store::open(path, common::options(false)).expect("open v2 store");
    store.enable_graph().expect("commit real v3 manifest");
    store.close().expect("close v3 store");
}

#[cfg(feature = "graph-cypher")]
#[test]
fn the_v3_old_reader_fixture_is_a_real_writer_manifest() {
    let scratch = tempfile::tempdir().expect("scratch");
    copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/releases/v0.6.0"),
        scratch.path(),
    );
    mint_v3_manifest(scratch.path());
    let bytes = std::fs::read(scratch.path().join("manifest.ze")).expect("v3 manifest");
    let manifest =
        zeppelin_embed::manifest::decode_manifest("writer fixture", &bytes).expect("real v3");
    assert!(manifest.graph.is_some());
    let reopened =
        Store::open(scratch.path(), common::options(true)).expect("new reader accepts v3");
    let oracle = std::fs::read_to_string(scratch.path().join("expected.json")).expect("oracle");
    assert_eq!(
        common::text_hits(&reopened, "orchard"),
        ids(&oracle, "query_orchard")
    );
    reopened.close().expect("close");
}

fn data_bytes(root: &Path) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    // Exact bytes also detect additions/removals, without hash collisions.
    std::fs::read_dir(root)
        .expect("data directory")
        .map(|entry| {
            let entry = entry.expect("data entry");
            (
                std::path::PathBuf::from(entry.file_name()),
                std::fs::read(entry.path()).expect("data bytes"),
            )
        })
        .collect()
}

#[cfg(feature = "graph-cypher")]
#[test]
#[ignore = "builds v0.6.0; run with ZE_FORMAT_COMPAT=1 in the format-compat job"]
fn a_v3_store_is_refused_by_the_v0_6_0_reader_and_its_data_files_are_byte_identical() {
    let scratch = tempfile::tempdir().expect("scratch");
    copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/releases/v0.6.0"),
        scratch.path(),
    );
    mint_v3_manifest(scratch.path());
    let before = data_bytes(scratch.path());
    for mode in ["ro", "rw"] {
        // A real v3 store has a catalog .zgraph object, so frozen v0.6.0
        // refuses at NativeGraphDirectory before decoding the manifest. Its
        // ABI code is 1 (as documented by ZE-340; the new binary uses 58).
        // Version refusal/56 is unreachable here today; manifest_v3.rs and
        // ZE-343's unmodified v2_reader fixture cover that version check.
        let response = run_old_reader(scratch.path(), mode);
        let response = response.trim();
        assert!(
            matches!(
                response,
                "{\"refused\": true, \"abi_code\": 56}" | "{\"refused\": true, \"abi_code\": 1}"
            ),
            "unexpected refusal: {response}"
        );
        eprintln!("v0.6.0 {mode}: {response}");
        assert_eq!(
            data_bytes(scratch.path()),
            before,
            "refused {mode} open changed files"
        );
    }
}

#[test]
#[ignore = "builds v0.6.0; run with ZE_FORMAT_COMPAT=1 in the format-compat job"]
fn a_graph_free_store_written_by_this_build_opens_in_the_v0_6_0_reader() {
    let scratch = tempfile::tempdir().expect("scratch");
    let store = Store::open(
        scratch.path(),
        common::options(false).with_schema(common::schema()),
    )
    .expect("new store");
    store
        .ingest(common::batch(vec![common::document(1, "orchard")]))
        .expect("ingest");
    store.close().expect("close");
    for mode in ["ro", "rw"] {
        assert!(run_old_reader(scratch.path(), mode).contains("\"version_refused\": false"));
    }
}

#[test]
#[ignore = "builds v0.6.0; run with ZE_FORMAT_COMPAT=1 in the format-compat job"]
fn the_v0_6_0_reader_refuses_portable_namespaces_without_changing_data() {
    use zeppelin_embed::lifecycle::{NamespaceMutation, namespace_batch, namespace_delete_cascade};
    let root = tempfile::tempdir().expect("root");
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/releases/v0.6.0");
    for name in ["a", "b"] {
        copy_tree(&fixture, &root.path().join(name));
    }
    let mutations = |writing| {
        ["a", "b"]
            .into_iter()
            .map(|name| NamespaceMutation {
                name: name.into(),
                options: common::options(false),
                upserts: if writing {
                    vec![common::document(100, "orchard")]
                } else {
                    vec![]
                },
                deletes: vec![],
                delete_where: None,
            })
            .collect()
    };
    namespace_batch(root.path(), mutations(true)).expect("portable live root");
    let assert_refused = |root: &Path, path: &Path| {
        for mode in ["ro", "rw"] {
            let payloads = |root: &Path| {
                file_hashes(root)
                    .into_iter()
                    .filter(|(path, _)| {
                        !matches!(
                            path.file_name().and_then(|n| n.to_str()),
                            Some("writer.lock" | ".ze-readers.lock")
                        )
                    })
                    .collect::<std::collections::BTreeMap<_, _>>()
            };
            let before = payloads(root);
            let all_before = file_hashes(root);
            let output = std::process::Command::new(old_reader())
                .arg(path)
                .arg(mode)
                .arg("namespace-refusal")
                .output()
                .expect("old reader");
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                String::from_utf8(output.stdout).expect("JSON").trim(),
                "{\"namespace_refused\": true}"
            );
            assert_eq!(payloads(root), before, "old reader changed namespace data");
            let lock_changes = file_hashes(root)
                .into_iter()
                .filter(|(path, hash)| {
                    matches!(
                        path.file_name().and_then(|n| n.to_str()),
                        Some("writer.lock" | ".ze-readers.lock")
                    ) && all_before.get(path) != Some(hash)
                })
                .map(|(path, _)| path)
                .collect::<Vec<_>>();
            eprintln!("ZE-383/384 old reader {mode} lock-file changes: {lock_changes:?}");
        }
    };
    let conversion = tempfile::tempdir().expect("conversion");
    let source = conversion.path().join("source");
    copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/releases/v0.6.0-namespaces"),
        &source,
    );
    let source_before = file_hashes(&source);
    let converted = conversion.path().join("converted");
    zeppelin_embed::lifecycle::namespace_relocate(&source, &converted)
        .expect("explicit conversion");
    for name in ["a", "b"] {
        assert_refused(&converted, &converted.join(name));
    }
    namespace_delete_cascade(&converted, mutations(false)).expect("converted copy routes");
    for name in ["a", "b"] {
        assert_refused(&converted, &converted.join(name));
        let routed = std::fs::read_dir(&converted)
            .expect("entries")
            .map(|e| e.expect("entry").path())
            .find(|p| {
                p.file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.starts_with(".ze-batch-"))
            })
            .expect("route")
            .join(name);
        assert_refused(&converted, &routed);
    }
    assert_eq!(file_hashes(&source), source_before);
    let bootstrap = tempfile::tempdir().expect("bootstrap root");
    for name in ["a", "b"] {
        copy_tree(&fixture, &bootstrap.path().join(name));
    }
    std::fs::copy(
        root.path().join(".ze-namespaces"),
        bootstrap.path().join(".ze-namespaces"),
    )
    .expect("empty portable bootstrap descriptor");
    assert!(!bootstrap.path().join("a/.ze-namespace-root").exists());
    assert_refused(bootstrap.path(), &bootstrap.path().join("a"));
    for routed in [false, true] {
        if routed {
            namespace_delete_cascade(root.path(), mutations(false))
                .expect("portable copy publication");
        }
        let paths = if routed {
            std::fs::read_dir(root.path())
                .expect("root entries")
                .map(|entry| entry.expect("entry").path())
                .find(|path| {
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with(".ze-batch-"))
                })
                .map(|path| vec![root.path().join("a"), path.join("a")])
                .expect("routed participant")
        } else {
            vec![root.path().join("a")]
        };
        for path in paths {
            assert_refused(root.path(), &path);
        }
    }
}
