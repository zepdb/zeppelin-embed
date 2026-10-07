#![allow(clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
#[allow(dead_code)]
#[path = "../../../scripts/fixtures/common.rs"]
mod common;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use zeppelin_embed::ingest::DocId;
use zeppelin_embed::ingest::wal_payload::{self, TransactionBinding};
use zeppelin_embed::lifecycle::{
    DocumentFields, Store, namespace_relocate, namespace_relocate_with_steps,
};
use zeppelin_embed::vfs::StdVfs;
use zeppelin_embed::wal::{LogSeq, WalReader};

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("directory");
    for entry in std::fs::read_dir(from).expect("entries") {
        let entry = entry.expect("entry");
        if entry.file_type().expect("type").is_dir() {
            copy_tree(&entry.path(), &to.join(entry.file_name()));
        } else {
            std::fs::copy(entry.path(), to.join(entry.file_name())).expect("copy");
        }
    }
}
fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/releases/v0.6.0-namespaces")
}
fn bytes(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, directory: &Path, output: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(directory).expect("entries") {
            let entry = entry.expect("entry");
            if entry.file_type().expect("type").is_dir() {
                visit(root, &entry.path(), output);
            } else {
                output.insert(
                    entry
                        .path()
                        .strip_prefix(root)
                        .expect("relative")
                        .to_owned(),
                    std::fs::read(entry.path()).expect("bytes"),
                );
            }
        }
    }
    let mut output = BTreeMap::new();
    visit(root, root, &mut output);
    output
}
fn envelope(body: &[u8]) -> Vec<u8> {
    let mut out = b"ZENS0001".to_vec();
    out.extend_from_slice(body);
    out.extend_from_slice(&xxhash_rust::xxh3::xxh3_64(&out).to_le_bytes());
    out
}
fn binding(root: &Path, name: &str) -> TransactionBinding {
    let marker = std::fs::read_dir(root.join(name))
        .expect("entries")
        .map(|e| e.expect("entry").path())
        .find(|p| {
            p.file_name()
                .expect("name")
                .to_string_lossy()
                .starts_with(".ze-accepted-")
        })
        .expect("acceptance");
    let bytes = std::fs::read(marker).expect("marker");
    TransactionBinding::decode(&bytes[8..bytes.len() - 8]).expect("binding")
}
fn add_undecided(directory: &Path, mut binding: TransactionBinding) {
    let path = directory.join("wal.ze");
    let reader = WalReader::open(&StdVfs, &path).expect("WAL");
    let mut out = std::fs::read(&path).expect("WAL bytes");
    let first = reader.records().last().expect("record").seq.get() + 1;
    for (transaction, start, count) in [
        (binding.transaction + 1, first, 2),
        (binding.transaction + 2, first + 2, 1),
    ] {
        binding.transaction = transaction;
        binding.first_seq = start;
        binding.last_seq = start + 1;
        for index in 0..count {
            let inner =
                wal_payload::encode_delete(&[DocId::new(1 + u128::from(index))]).expect("delete");
            let payload =
                wal_payload::encode_prepared(binding, index, 2, wal_payload::DELETE_V1, &inner)
                    .expect("prepared");
            out.extend_from_slice(
                &zeppelin_embed::wal::record::encode_record(
                    zeppelin_embed::wal::record::WalRecord {
                        seq: LogSeq::new(start + u64::from(index)),
                        op: 9,
                        payload: &payload,
                    },
                )
                .expect("record"),
            );
        }
    }
    std::fs::write(path, out).expect("undecided WAL");
}
fn staged(root: &Path, routed: bool) {
    let mut out = b"ZENS0002".to_vec();
    out.extend_from_slice(&2_u32.to_le_bytes());
    let mut routes = String::new();
    for name in ["a", "b"] {
        let binding = binding(root, name);
        let manifest = format!(".ze-manifest-{}", binding.transaction);
        for text in [name, &manifest] {
            out.extend_from_slice(&(text.len() as u16).to_le_bytes());
            out.extend_from_slice(text.as_bytes());
        }
        out.extend_from_slice(&binding.encode().expect("binding"));
        let directory = if routed {
            let route = format!(".ze-batch-conversion/{name}");
            let target = root.join(&route);
            copy_tree(&root.join(name), &target);
            std::fs::remove_file(target.join(".ze-namespace-root"))
                .expect("0.6.0 copy protocol retains references only in logical directories");
            std::fs::write(target.join(".ze-prepared"), envelope(route.as_bytes()))
                .expect("preparation");
            routes.push_str(&format!("{name}\t{route}\n"));
            target
        } else {
            root.join(name)
        };
        std::fs::remove_file(directory.join(format!(".ze-accepted-{}", binding.transaction)))
            .expect("remove acceptance");
    }
    let routes = envelope(routes.as_bytes());
    out.extend_from_slice(&(routes.len() as u32).to_le_bytes());
    out.extend_from_slice(&routes);
    out.extend_from_slice(&xxhash_rust::xxh3::xxh3_64(&out).to_le_bytes());
    if routed {
        std::fs::write(root.join(".ze-batch-conversion/intent.ze"), routes).expect("intent");
    }
    std::fs::write(root.join(".ze-namespaces"), out).expect("staged root");
}
fn observe(root: &Path) {
    for name in ["a", "b"] {
        let store = Store::open(root.join(name), common::options(true)).expect("converted reader");
        for id in [1, 2, 4] {
            let documents = store
                .get_documents(&[DocId::new(id)], DocumentFields::ALL)
                .expect("get");
            let document = documents[0]
                .as_ref()
                .expect("undecided deletes did not commit");
            assert_eq!(document.vector, Some(common::vector(id)));
        }
        assert!(
            store
                .get_documents(&[DocId::new(3)], DocumentFields::NONE)
                .expect("get")[0]
                .is_none()
        );
        assert_eq!(
            store.count_documents(None, None).expect("count").generation,
            6
        );
        store.close().expect("close");
    }
}
#[test]
#[ignore = "builds v0.6.0; run with ZE_FORMAT_COMPAT=1 in the format-compat job"]
fn conversion_accepts_the_routed_layout_written_by_0_6_0() {
    static WRITER: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    let writer = WRITER.get_or_init(|| {
        let scratch = tempfile::tempdir().expect("writer scratch").keep();
        let binary = scratch.join("old-writer");
        let status = std::process::Command::new("bash")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../scripts/fixtures/build-old-writer.sh"
            ))
            .arg(&binary)
            .status()
            .expect("build real 0.6.0 writer");
        assert!(status.success(), "old-writer build failed");
        binary
    });
    let temp = tempfile::tempdir().expect("scratch");
    let original = temp.path().join("original");
    std::fs::create_dir(&original).expect("original root");
    for mode in ["namespace-seed", "namespace-commit", "namespace-cascade"] {
        let output = std::process::Command::new(writer)
            .arg(mode)
            .arg(&original)
            .output()
            .expect("release writer");
        assert!(
            output.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if mode == "namespace-cascade" {
            assert_eq!(
                String::from_utf8(output.stdout).expect("release oracle"),
                "[10, 10]\n"
            );
        }
    }
    let routed = std::fs::read_dir(&original)
        .expect("root entries")
        .map(|entry| entry.expect("entry").path())
        .find(|path| {
            path.file_name()
                .expect("name")
                .to_string_lossy()
                .starts_with(".ze-batch-")
        })
        .expect("release copy-protocol route");
    for name in ["a", "b"] {
        assert!(original.join(name).join(".ze-namespace-root").is_file());
        assert!(
            !routed.join(name).join(".ze-namespace-root").exists(),
            "0.6.0 references are logical only"
        );
        let store = Store::open(original.join(name), common::options(true))
            .expect("unmoved 0.6.0 store opens");
        assert_eq!(
            store
                .count_documents(None, None)
                .expect("release generation")
                .generation,
            10
        );
        store.close().expect("close");
    }
    let source = temp.path().join("source");
    std::fs::rename(&original, &source).expect("move complete legacy root");
    let before = bytes(&source);
    let error = Store::open(source.join("a"), common::options(true))
        .err()
        .expect("raw legacy relocation refuses");
    assert!(
        error
            .to_string()
            .contains("namespace requires its original transaction root")
    );
    let destination = temp.path().join("converted");
    namespace_relocate(&source, &destination).expect("convert real release routed layout");
    for name in ["a", "b"] {
        let reference = std::fs::read(
            destination
                .join(routed.strip_prefix(&original).expect("relative route"))
                .join(name)
                .join(".ze-namespace-root"),
        )
        .expect("converted physical reference");
        assert!(reference.starts_with(b"ZENR0002"));
        for read_only in [true, false] {
            let store = Store::open(destination.join(name), common::options(read_only))
                .expect("converted routed store");
            let documents = store
                .get_documents(
                    &[DocId::new(1), DocId::new(2), DocId::new(3), DocId::new(4)],
                    DocumentFields::ALL,
                )
                .expect("get");
            assert!(documents[0].is_some() && documents[3].is_some());
            assert!(documents[1].is_none() && documents[2].is_none());
            assert_eq!(
                store
                    .count_documents(None, None)
                    .expect("generation")
                    .generation,
                10
            );
            store.close().expect("close");
        }
    }
    assert_eq!(bytes(&source), before, "conversion preserves source bytes");
}

#[test]
fn conversion_preserves_committed_and_undecided_prepared_runs() {
    for (selected, routed) in [(false, false), (true, false), (true, true)] {
        let temp = tempfile::tempdir().expect("scratch");
        let source = temp.path().join("source");
        copy_tree(&fixture(), &source);
        for name in ["a", "b"] {
            add_undecided(&source.join(name), binding(&source, name));
        }
        if selected {
            staged(&source, routed);
        }
        // Also retain an unselected preparation, which must remain private.
        copy_tree(&source.join("a"), &source.join(".ze-batch-undecided/a"));
        std::fs::remove_file(source.join(".ze-batch-undecided/a/.ze-namespace-root"))
            .expect("unselected legacy copy has no reference");
        std::fs::write(
            source.join(".ze-batch-undecided/intent.ze"),
            envelope(b"a\t.ze-batch-undecided/a\n"),
        )
        .expect("intent");
        std::fs::write(
            source.join(".ze-batch-undecided/a/.ze-prepared"),
            envelope(b".ze-batch-undecided/a"),
        )
        .expect("preparation");
        let before = bytes(&source);
        let destination = temp.path().join("converted");
        namespace_relocate(&source, &destination).expect("convert prepared state");
        observe(&destination);
        assert_eq!(bytes(&source), before, "conversion rewrote source");
        for (relative, original) in &before {
            let name = relative.file_name().expect("filename").to_string_lossy();
            if !matches!(
                name.as_ref(),
                ".ze-namespaces" | ".ze-namespace-root" | "wal.ze"
            ) && !name.starts_with(".ze-accepted-")
            {
                assert_eq!(
                    std::fs::read(destination.join(relative)).expect("converted artifact"),
                    *original,
                    "conversion changed data, manifest digest, or relative route: {relative:?}"
                );
            }
        }
        for relative in before
            .keys()
            .filter(|p| p.file_name().and_then(|s| s.to_str()) == Some("wal.ze"))
        {
            let old = WalReader::open(&StdVfs, &source.join(relative)).expect("old WAL");
            let new = WalReader::open(&StdVfs, &destination.join(relative)).expect("new WAL");
            assert_eq!(old.records().len(), new.records().len());
            for (old, new) in old.records().iter().zip(new.records()) {
                assert_eq!((old.seq, old.op), (new.seq, new.op));
                let old = old.payload().expect("old payload");
                let new = new.payload().expect("new payload");
                if old != new {
                    assert_eq!(&old[..20], &new[..20]); // header and transaction
                    assert_eq!(&old[36..], &new[36..]); // only participant u128 changes
                }
            }
        }
        let unselected = Store::open(
            destination.join(".ze-batch-undecided/a"),
            common::options(true),
        );
        assert!(unselected.is_err(), "unselected preparation became public");
    }
}

#[test]
fn conversion_refuses_pending_namespace_cleanup_without_modifying_data() {
    let temp = tempfile::tempdir().expect("scratch");
    let source = temp.path().join("source");
    copy_tree(&fixture(), &source);
    std::fs::write(
        source.join(".ze-cleanup"),
        b"pending cleanup must not be removed or repaired",
    )
    .expect("cleanup");
    let before = bytes(&source);
    let destination = temp.path().join("converted");
    let error = namespace_relocate(&source, &destination).expect_err("pending cleanup refused");
    assert!(
        error
            .to_string()
            .contains("conversion refuses pending namespace cleanup")
    );
    assert!(!destination.exists());
    assert_eq!(bytes(&source), before);
}

#[test]
fn interrupted_conversion_syncs_destination_parent_before_acknowledged_writes() {
    use std::sync::Arc;
    use zeppelin_embed::lifecycle::{StoreTestDependencies, SystemMonotonicClock};
    use zeppelin_embed::vfs::{
        SyncKind,
        crash::{CrashOperation, RecordingVfs},
    };

    let temp = tempfile::tempdir().expect("scratch");
    let parent = std::fs::canonicalize(temp.path()).expect("parent");
    let source = parent.join("source");
    let destination = parent.join("converted");
    copy_tree(&fixture(), &source);
    let before = bytes(&source);
    let error = namespace_relocate_with_steps(&source, &destination, &mut |step| {
        if step == "conversion publish" {
            return Err(std::io::Error::other("process died before parent sync"));
        }
        Ok(())
    })
    .expect_err("interrupted publication");
    assert!(error.to_string().contains("publication is indeterminate"));
    assert!(
        destination.is_dir(),
        "rename is visible without a parent sync"
    );
    namespace_relocate(&source, &destination)
        .expect_err("existing destination must not be overwritten");

    let recording = Arc::new(RecordingVfs::new(StdVfs));
    let store = Store::open_with_test_dependencies(
        destination.join("a"),
        common::options(true),
        StoreTestDependencies::new(recording.clone(), Arc::new(SystemMonotonicClock)),
    )
    .expect("read-only open of visible destination");
    store.close().expect("close reader");
    assert!(recording.operations().expect("operations").is_empty());
    let store = Store::open_with_test_dependencies(
        destination.join("a"),
        common::options(false),
        StoreTestDependencies::new(recording.clone(), Arc::new(SystemMonotonicClock)),
    )
    .expect("writable open completes publication");
    store
        .ingest(common::batch(vec![common::document(
            5,
            "acknowledged after relocation",
        )]))
        .expect("durable acknowledged write");
    let operations = recording.operations().expect("operations");
    let parent_sync = operations
        .iter()
        .position(|op| {
            matches!(op,
                CrashOperation::Sync { path, kind: SyncKind::Full } if path == &parent
            )
        })
        .expect("destination parent must be synced before any write can be acknowledged");
    let first_write = operations
        .iter()
        .position(|op| {
            matches!(
                op,
                CrashOperation::Write { .. }
                    | CrashOperation::Append { .. }
                    | CrashOperation::Rename { .. }
            )
        })
        .expect("acknowledged mutation");
    assert!(
        parent_sync < first_write,
        "publication sync must precede recovery and writes"
    );
    store.close().expect("close writer");
    let reopened = Store::open(destination.join("a"), common::options(true)).expect("reopen");
    assert!(
        reopened
            .get_documents(&[DocId::new(5)], DocumentFields::ALL)
            .expect("acknowledged row")[0]
            .is_some()
    );
    assert_eq!(bytes(&source), before, "source must stay unchanged");
}

#[test]
fn conversion_power_cuts_publish_no_partial_root() {
    let clean = tempfile::tempdir().expect("scratch");
    let source = clean.path().join("source");
    copy_tree(&fixture(), &source);
    for name in ["a", "b"] {
        add_undecided(&source.join(name), binding(&source, name));
    }
    staged(&source, true);
    std::fs::copy(
        source.join(".ze-namespaces"),
        source.join(".ze-namespaces.tmp"),
    )
    .expect("retained staged descriptor");
    let mut steps = Vec::new();
    namespace_relocate_with_steps(&source, &clean.path().join("converted"), &mut |step| {
        steps.push(step.to_owned());
        Ok(())
    })
    .expect("clean conversion");
    assert_eq!(
        steps.iter().filter(|s| *s == "conversion publish").count(),
        1
    );
    observe(&clean.path().join("converted"));
    let template = source;
    eprintln!(
        "ZE-384 power-cut matrix: {} completed I/O cuts; routed root with committed and undecided runs",
        steps.len()
    );
    for (cut, step) in steps.iter().enumerate() {
        let temp = tempfile::tempdir().expect("cut scratch");
        let source = temp.path().join("source");
        copy_tree(&template, &source);
        let before = bytes(&source);
        let destination = temp.path().join("converted");
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "conversion_crash_child",
                "--ignored",
                "--exact",
                "--nocapture",
            ])
            .env("ZE_RELOCATE_SOURCE", &source)
            .env("ZE_RELOCATE_DESTINATION", &destination)
            .env("ZE_RELOCATE_CUT", cut.to_string())
            .output()
            .expect("crash child");
        assert_eq!(
            output.status.code(),
            Some(88),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(&format!("cut {cut}: {step}")),
            "actual cut receipt"
        );
        assert_eq!(bytes(&source), before, "cut {cut} changed source");
        if destination.exists() {
            assert!(matches!(
                step.as_str(),
                "conversion publish" | "conversion parent sync"
            ));
            observe(&destination);
            // Losing the unsynced parent rename may instead leave no destination.
            // Both allowed durable images preserve all source bytes.
            std::fs::rename(&destination, temp.path().join("unpublished"))
                .expect("lost parent rename image");
        }
        assert!(!destination.exists(), "no prefix publishes part of a root");
    }
}

#[test]
#[ignore = "child of conversion_power_cuts_publish_no_partial_root"]
fn conversion_crash_child() {
    let source = PathBuf::from(std::env::var_os("ZE_RELOCATE_SOURCE").expect("source"));
    let destination =
        PathBuf::from(std::env::var_os("ZE_RELOCATE_DESTINATION").expect("destination"));
    let cut: usize = std::env::var("ZE_RELOCATE_CUT")
        .expect("cut")
        .parse()
        .expect("cut number");
    let mut index = 0;
    namespace_relocate_with_steps(&source, &destination, &mut |step| {
        if index == cut {
            println!("cut {cut}: {step}");
            // No unwinding or cleanup: the OS releases the source locks.
            std::process::exit(88);
        }
        index += 1;
        Ok(())
    })
    .expect("child conversion");
    panic!("crash cut did not fire");
}

#[test]
fn conversion_rejects_damaged_evidence_busy_roots_and_existing_destinations() {
    for defect in [
        "tail",
        "binding",
        "missing-member",
        "missing-manifest",
        "missing-segment",
        "route",
        "missing-intent",
        "missing-intended-participant",
        "missing-route-reference",
        "acceptance-digest",
        "busy",
        "destination",
    ] {
        let temp = tempfile::tempdir().expect("scratch");
        let source = temp.path().join("source");
        copy_tree(&fixture(), &source);
        let destination = temp.path().join("converted");
        let mut lock = None;
        match defect {
            "tail" => {
                let path = source.join("a/wal.ze");
                let mut wal = std::fs::read(&path).expect("wal");
                wal.push(0);
                std::fs::write(path, wal).expect("torn tail");
            }
            "binding" => {
                let binding = binding(&source, "a");
                let path = source.join(format!("a/.ze-accepted-{}", binding.transaction));
                let mut bad = binding;
                bad.participant += 1;
                std::fs::write(path, envelope(&bad.encode().expect("binding")))
                    .expect("bad marker");
            }
            "missing-member" => {
                let path = source.join("a/wal.ze");
                let wal = WalReader::open(&StdVfs, &path).expect("wal");
                let bytes = std::fs::read(&path).expect("bytes");
                std::fs::write(
                    path,
                    &bytes[..bytes.len() - wal.records().last().expect("last").encoded_len()],
                )
                .expect("remove member");
            }
            "missing-manifest" => {
                std::fs::remove_file(source.join("a/manifest.ze")).expect("remove manifest");
            }
            "missing-segment" => {
                let segment = std::fs::read_dir(source.join("a"))
                    .expect("entries")
                    .map(|e| e.expect("entry").path())
                    .find(|p| p.extension().is_some_and(|s| s == "zseg"))
                    .expect("segment");
                std::fs::remove_file(segment).expect("remove segment");
            }
            "route" => {
                std::fs::write(
                    source.join(".ze-namespaces"),
                    envelope(b"a\t.ze-batch-missing/a\n"),
                )
                .expect("missing route");
            }
            "missing-intent" => {
                copy_tree(&source.join("a"), &source.join(".ze-batch-orphan/a"));
                std::fs::write(
                    source.join(".ze-batch-orphan/a/.ze-prepared"),
                    envelope(b".ze-batch-orphan/a"),
                )
                .expect("preparation");
            }
            "missing-intended-participant" => {
                std::fs::create_dir(source.join(".ze-batch-orphan"))
                    .expect("preparation directory");
                std::fs::write(
                    source.join(".ze-batch-orphan/intent.ze"),
                    envelope(b"a\t.ze-batch-orphan/a\n"),
                )
                .expect("intent");
            }
            "missing-route-reference" => {
                staged(&source, true);
                // Legacy routed authority lives in the logical directory.
                std::fs::remove_file(source.join("a/.ze-namespace-root"))
                    .expect("remove logical routed authority");
                // Remove op 9 to expose the route/reference requirement itself.
                let path = source.join(".ze-batch-conversion/a/wal.ze");
                let wal = WalReader::open(&StdVfs, &path).expect("WAL");
                let input = std::fs::read(&path).expect("bytes");
                let mut output = input[..zeppelin_embed::wal::header::WAL_HEADER_LEN].to_vec();
                for record in wal.records().iter().filter(|r| r.op != 9) {
                    output.extend_from_slice(
                        &zeppelin_embed::wal::record::encode_record(
                            zeppelin_embed::wal::record::WalRecord {
                                seq: record.seq,
                                op: record.op,
                                payload: record.payload().expect("payload"),
                            },
                        )
                        .expect("record"),
                    );
                }
                std::fs::write(path, output).expect("WAL");
                // Use a routed v1 root without staged selections, so missing
                // reference cannot be hidden behind the committed-range check.
                std::fs::write(
                    source.join(".ze-namespaces"),
                    envelope(b"a\t.ze-batch-conversion/a\nb\t.ze-batch-conversion/b\n"),
                )
                .expect("routes");
            }
            "acceptance-digest" => {
                let binding = binding(&source, "a");
                let path = source.join(format!("a/.ze-accepted-{}", binding.transaction));
                let mut bad = binding;
                bad.manifest_digest ^= 1;
                // Rewrite both the marker and WAL binding consistently: the
                // independent retained manifest must still reject the false digest.
                std::fs::write(path, envelope(&bad.encode().expect("binding"))).expect("marker");
                let path = source.join("a/wal.ze");
                let wal = WalReader::open(&StdVfs, &path).expect("WAL");
                let input = std::fs::read(&path).expect("bytes");
                let mut output = input[..zeppelin_embed::wal::header::WAL_HEADER_LEN].to_vec();
                for record in wal.records() {
                    let mut payload = record.payload().expect("payload").to_vec();
                    if record.op == 9 {
                        payload[4..68].copy_from_slice(&bad.encode().expect("binding"));
                    }
                    output.extend_from_slice(
                        &zeppelin_embed::wal::record::encode_record(
                            zeppelin_embed::wal::record::WalRecord {
                                seq: record.seq,
                                op: record.op,
                                payload: &payload,
                            },
                        )
                        .expect("record"),
                    );
                }
                std::fs::write(path, output).expect("false WAL evidence");
            }
            "busy" => {
                lock = Some(
                    zeppelin_embed::lifecycle::lock::StoreLock::acquire(&source.join("a"))
                        .expect("participant lock"),
                );
            }
            "destination" => {
                std::fs::create_dir(&destination).expect("existing empty destination");
            }
            _ => panic!("unknown defect"),
        }
        let before = bytes(&source);
        namespace_relocate(&source, &destination).expect_err(defect);
        assert_eq!(bytes(&source), before, "{defect} changed source");
        assert_eq!(destination.exists(), defect == "destination");
        drop(lock);
    }
}
