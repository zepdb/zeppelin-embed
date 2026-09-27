#![allow(clippy::expect_used)]
use zeppelin_embed::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
use zeppelin_embed::lifecycle::{
    DocumentFields, NamespaceMutation, OpenOptions, Store, namespace_batch,
};
fn doc(id: u128, rev: u64) -> IngestDocument {
    IngestDocument::new(
        DocumentVersion::new(DocId::new(id), Revision::new(rev)),
        vec![1.0, 2.0],
    )
    .with_timestamp(id as i64)
}
fn mutation(name: &str, upserts: Vec<IngestDocument>) -> NamespaceMutation {
    NamespaceMutation {
        name: name.into(),
        options: OpenOptions::new(),
        upserts,
        deletes: vec![],
        delete_where: None,
    }
}
#[test]
fn cross_namespace_validation_changes_nothing() {
    let root = tempfile::tempdir().expect("root");
    for name in ["a", "b"] {
        let store = Store::open(root.path().join(name), OpenOptions::new()).expect("store");
        store
            .ingest(IngestBatch::new(vec![doc(1, 1)]))
            .expect("seed");
    }
    let result = namespace_batch(
        root.path(),
        vec![
            mutation("a", vec![doc(2, 1)]),
            mutation(
                "b",
                vec![IngestDocument::new(
                    DocumentVersion::new(DocId::new(2), Revision::new(1)),
                    vec![f32::NAN, 0.0],
                )],
            ),
        ],
    );
    assert!(result.is_err());
    let a = Store::open(root.path().join("a"), OpenOptions::read_only()).expect("open a alone");
    assert!(
        a.get_documents(&[DocId::new(2)], DocumentFields::NONE)
            .expect("get")
            .iter()
            .all(Option::is_none),
        "a failed cross-namespace batch must not publish its first participant"
    );
}

fn seed(root: &std::path::Path) {
    for name in ["a", "b"] {
        let store = Store::open(root.join(name), OpenOptions::new()).expect("seed open");
        store
            .ingest(IngestBatch::new(vec![doc(1, 1), doc(3, 1)]))
            .expect("seed");
        store.seal().expect("seal seed");
    }
}
fn mixed() -> Vec<NamespaceMutation> {
    ["a", "b"]
        .into_iter()
        .map(|name| NamespaceMutation {
            name: name.into(),
            options: OpenOptions::new(),
            upserts: vec![doc(2, 1)],
            deletes: vec![DocId::new(1)],
            delete_where: Some(zeppelin_embed::meta::Predicate::Eq {
                column: zeppelin_embed::meta::TIMESTAMP_COLUMN,
                value: zeppelin_embed::meta::PredicateValue::I64(3),
            }),
        })
        .collect()
}
fn state(root: &std::path::Path, name: &str) -> Vec<bool> {
    let store =
        Store::open(root.join(name), OpenOptions::read_only()).expect("standalone read-only open");
    store
        .get_documents(
            &[DocId::new(1), DocId::new(2), DocId::new(3)],
            DocumentFields::NONE,
        )
        .expect("get")
        .iter()
        .map(Option::is_some)
        .collect()
}
#[test]
fn standalone_reader_resolves_commit_before_sibling_recovery() {
    let root = tempfile::tempdir().expect("root");
    seed(root.path());
    let old = Store::open(root.path().join("a"), OpenOptions::read_only()).expect("old reader");
    let generations = namespace_batch(root.path(), mixed()).expect("commit");
    assert_eq!(generations.len(), 2);
    assert_eq!(state(root.path(), "a"), [false, true, false]);
    let current =
        Store::open(root.path().join("a"), OpenOptions::read_only()).expect("decided generation");
    assert_eq!(
        current
            .get_documents_with_generation(&[], DocumentFields::NONE)
            .expect("generation")
            .0,
        *generations.first().expect("first participant generation")
    );
    assert!(
        old.get_documents(&[DocId::new(1)], DocumentFields::NONE)
            .expect("old read")
            .iter()
            .all(Option::is_some)
    );
    assert_eq!(state(root.path(), "b"), [false, true, false]);
    let writer = Store::open(root.path().join("a"), OpenOptions::new()).expect("writer");
    writer
        .ingest(IngestBatch::new(vec![doc(4, 1)]))
        .expect("ordinary write");
    assert!(
        namespace_batch(root.path(), mixed()).is_err(),
        "open writers exclude coordinator"
    );
    writer.close().expect("close releases both locks");
    namespace_batch(root.path(), mixed()).expect("second transaction");
    let reopened = Store::open(root.path().join("a"), OpenOptions::read_only()).expect("reopen");
    assert!(
        reopened
            .get_documents(&[DocId::new(4)], DocumentFields::NONE)
            .expect("get ordinary write")
            .iter()
            .all(Option::is_some)
    );
}

#[cfg(feature = "test-support")]
#[test]
fn cross_namespace_crash_prefixes_share_one_decision() {
    use zeppelin_embed::lifecycle::namespace_batch_with_steps;
    let count_root = tempfile::tempdir().expect("root");
    seed(count_root.path());
    let mut steps = Vec::new();
    namespace_batch_with_steps(count_root.path(), mixed(), &mut |name| {
        steps.push(name.to_owned());
        Ok(())
    })
    .expect("count steps");
    assert!(steps.len() > 20);
    for cut in 0..steps.len() {
        let root = tempfile::tempdir().expect("root");
        seed(root.path());
        let mut index = 0;
        assert!(
            namespace_batch_with_steps(root.path(), mixed(), &mut |_| {
                let fail = index == cut;
                index += 1;
                if fail {
                    Err(std::io::Error::other("injected protocol interruption"))
                } else {
                    Ok(())
                }
            })
            .is_err()
        );
        let a = state(root.path(), "a");
        let b = state(root.path(), "b");
        assert_eq!(a, b, "cut {cut}: {}", steps.get(cut).expect("step"));
        assert!(a == [true, false, true] || a == [false, true, false]);
        // Repeated recovery in the opposite order leaves the decision unchanged.
        assert_eq!(state(root.path(), "b"), b);
        assert_eq!(state(root.path(), "a"), a);
    }
    eprintln!("ZE-239 fault cuts: {}", steps.len());
}

#[test]
fn missing_or_corrupt_root_never_falls_back_to_original_store() {
    let root = tempfile::tempdir().expect("root");
    seed(root.path());
    namespace_batch(root.path(), mixed()).expect("commit");
    let record = root.path().join(".ze-namespaces");
    let bytes = std::fs::read(&record).expect("root decision");
    std::fs::write(&record, b"bad").expect("corrupt");
    assert!(Store::open(root.path().join("a"), OpenOptions::read_only()).is_err());
    std::fs::remove_file(&record).expect("remove");
    assert!(Store::open(root.path().join("a"), OpenOptions::read_only()).is_err());
    std::fs::write(&record, bytes).expect("restore");
    assert_eq!(state(root.path(), "a"), [false, true, false]);
    let preparation = std::fs::read_dir(root.path())
        .expect("root entries")
        .map(|entry| entry.expect("entry").path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(".ze-batch-"))
        })
        .expect("preparation")
        .join("a");
    std::fs::remove_file(preparation.join("manifest.ze")).expect("remove prepared manifest");
    std::fs::remove_file(preparation.join("wal.ze")).expect("remove prepared WAL");
    assert!(
        Store::open(root.path().join("a"), OpenOptions::read_only()).is_err(),
        "a missing preparation is not an empty namespace"
    );
}

#[cfg(feature = "test-support")]
#[test]
fn namespace_batch_kill_child() {
    let Ok(root) = std::env::var("ZE239_KILL_ROOT") else {
        return;
    };
    let cut: usize = std::env::var("ZE239_KILL_STEP")
        .expect("cut")
        .parse()
        .expect("integer cut");
    let mut step = 0;
    zeppelin_embed::lifecycle::namespace_batch_with_steps(
        std::path::Path::new(&root),
        mixed(),
        &mut |_| {
            if step == cut {
                std::process::exit(77);
            }
            step += 1;
            Ok(())
        },
    )
    .expect("child protocol");
}
#[cfg(feature = "test-support")]
#[test]
fn process_death_at_every_protocol_step_recovers_one_decision() {
    let baseline = tempfile::tempdir().expect("root");
    seed(baseline.path());
    let mut count = 0;
    zeppelin_embed::lifecycle::namespace_batch_with_steps(baseline.path(), mixed(), &mut |_| {
        count += 1;
        Ok(())
    })
    .expect("count");
    for cut in 0..count {
        let root = tempfile::tempdir().expect("root");
        seed(root.path());
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", "namespace_batch_kill_child"])
            .env("ZE239_KILL_ROOT", root.path())
            .env("ZE239_KILL_STEP", cut.to_string())
            .output()
            .expect("kill child");
        assert_eq!(
            output.status.code(),
            Some(77),
            "cut {cut}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            state(root.path(), "b"),
            state(root.path(), "a"),
            "kill at {cut}"
        );
    }
    eprintln!("ZE-239 process deaths: {count}");
}

#[cfg(feature = "test-support")]
#[test]
fn readers_during_preparation_and_publication_select_complete_states() {
    let root = tempfile::tempdir().expect("root");
    seed(root.path());
    let mut observed_new = false;
    zeppelin_embed::lifecycle::namespace_batch_with_steps(root.path(), mixed(), &mut |_| {
        let a = state(root.path(), "a");
        let b = state(root.path(), "b");
        assert_eq!(a, b);
        assert!(a == [true, false, true] || a == [false, true, false]);
        observed_new |= a == [false, true, false];
        Ok(())
    })
    .expect("commit with interleaved reader opens");
    assert!(observed_new);
}
