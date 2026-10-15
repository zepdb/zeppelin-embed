#![allow(clippy::expect_used, clippy::indexing_slicing)]
#[cfg(all(feature = "test-seams", unix))]
mod test_support;
use std::path::Path;
use zeppelin_embed::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
use zeppelin_embed::lifecycle::{
    CascadeRule, DocumentFields, NamespaceMutation, OpenOptions, Store, StoreError,
    namespace_declare_cascade, namespace_delete_cascade,
};
use zeppelin_embed::meta::{ColumnDefinition, ColumnId, ColumnType, PredicateValue, Schema};
fn options() -> OpenOptions {
    OpenOptions::new().with_schema(
        Schema::new(vec![ColumnDefinition::new(
            ColumnId::new(1),
            "parent",
            ColumnType::Id128,
            true,
        )])
        .expect("schema"),
    )
}
fn participants(names: &[&str]) -> Vec<NamespaceMutation> {
    names
        .iter()
        .map(|name| NamespaceMutation {
            name: (*name).into(),
            options: options(),
            upserts: vec![],
            deletes: vec![],
            delete_where: None,
        })
        .collect()
}
fn rule(parent: &str, child: &str) -> CascadeRule {
    CascadeRule {
        parent: parent.into(),
        child: child.into(),
        attribute: ColumnId::new(1),
    }
}
fn seed(root: &Path) {
    for (name, offset, parent) in [("notes", 0, 0), ("segments", 10, 0), ("words", 20, 10)] {
        let store = Store::open(root.join(name), options()).expect("open");
        let docs = (1..=2)
            .map(|id| {
                IngestDocument::new(
                    DocumentVersion::new(DocId::new(offset + id), Revision::new(1)),
                    vec![1.0, 2.0],
                )
                .with_timestamp(0)
                .with_text("private text")
                .with_columns(vec![(
                    ColumnId::new(1),
                    PredicateValue::Id128(DocId::new(parent + id)),
                )])
            })
            .collect();
        store.ingest(IngestBatch::new(docs)).expect("seed");
        store.seal().expect("seal");
    }
    namespace_declare_cascade(
        root,
        participants(&["notes", "segments"]),
        rule("notes", "segments"),
    )
    .expect("declare segments");
    namespace_declare_cascade(
        root,
        participants(&["segments", "words"]),
        rule("segments", "words"),
    )
    .expect("declare words");
}
fn deletion() -> Vec<NamespaceMutation> {
    let mut result = participants(&["notes", "segments", "words"]);
    result[0].deletes = vec![DocId::new(1)];
    result
}
fn state(root: &Path) -> Vec<Vec<bool>> {
    [("words", 20), ("segments", 10), ("notes", 0)]
        .into_iter()
        .map(|(name, offset)| {
            let store = Store::open(
                root.join(name),
                OpenOptions::read_only().with_schema(
                    Schema::new(vec![ColumnDefinition::new(
                        ColumnId::new(1),
                        "parent",
                        ColumnType::Id128,
                        true,
                    )])
                    .expect("schema"),
                ),
            )
            .expect("reopen child first");
            store
                .get_documents(
                    &[DocId::new(offset + 1), DocId::new(offset + 2)],
                    DocumentFields::ALL,
                )
                .expect("get")
                .iter()
                .map(Option::is_some)
                .collect()
        })
        .collect()
}
#[test]
fn cascade_uses_live_publication() {
    use zeppelin_embed::lifecycle::namespace_delete_cascade_with_steps;

    let root = tempfile::tempdir().expect("root");
    seed(root.path());
    let mut steps = Vec::new();
    let generations = namespace_delete_cascade_with_steps(root.path(), deletion(), &mut |step| {
        steps.push(step.to_owned());
        Ok(())
    })
    .expect("cascade");
    assert!(steps.iter().any(|step| step == "live states installed"));
    assert!(!steps.iter().any(|step| step == "copy chunk"));
    assert_eq!(generations, vec![3; 3]);
    assert_eq!(state(root.path()), vec![vec![false, true]; 3]);
}

#[test]
fn cascade_preserves_singleton_and_checkpoint_generations() {
    for deletes in [vec![], vec![DocId::new(1)], vec![DocId::new(99)]] {
        let root = tempfile::tempdir().expect("root");
        let store = Store::open(root.path().join("notes"), options()).expect("open");
        store
            .ingest(IngestBatch::new(vec![IngestDocument::new(
                DocumentVersion::new(DocId::new(1), Revision::new(1)),
                vec![1.0, 2.0],
            )]))
            .expect("unsealed row");
        store.close().expect("close");
        let expected = if deletes.is_empty() { 2 } else { 3 };
        let mut mutations = participants(&["notes"]);
        mutations[0].deletes = deletes;
        assert_eq!(
            namespace_delete_cascade(root.path(), mutations).expect("singleton cascade"),
            vec![expected]
        );
        let reopened = Store::open(root.path().join("notes"), options()).expect("reopen");
        assert_eq!(reopened.seal().expect("already checkpointed"), expected);
    }
}

#[test]
fn namespace_batch_does_not_expand_cascade_declarations() {
    let root = tempfile::tempdir().expect("root");
    seed(root.path());
    zeppelin_embed::lifecycle::namespace_batch(root.path(), deletion()).expect("explicit batch");
    assert_eq!(
        state(root.path()),
        vec![vec![true, true], vec![true, true], vec![false, true]]
    );
}

#[test]
fn cascade_refuses_pinned_writer_without_deletion() {
    let root = tempfile::tempdir().expect("root");
    seed(root.path());
    let store = Store::open(root.path().join("notes"), options()).expect("source");
    let view = store.open_snapshot().expect("pin");
    store.close().expect("closed writer");
    assert!(matches!(
        namespace_delete_cascade(root.path(), deletion()),
        Err(StoreError::StoreBusy { .. })
    ));
    assert!(
        view.get_documents(&[DocId::new(1)], DocumentFields::ALL)
            .expect("old snapshot")
            .iter()
            .all(Option::is_some)
    );
    view.close().expect("release pin");
    assert_eq!(state(root.path()), vec![vec![true, true]; 3]);
}

#[test]
fn durable_cascade_deletes_transitive_dependants_and_rejects_cycles() {
    let root = tempfile::tempdir().expect("root");
    seed(root.path());
    let error = namespace_declare_cascade(
        root.path(),
        participants(&["words", "notes"]),
        rule("words", "notes"),
    )
    .expect_err("cycle");
    assert!(matches!(error, StoreError::CascadeCycle { .. }));
    for name in ["notes", "segments", "words"] {
        assert!(error.to_string().contains(name));
    }
    assert!(namespace_delete_cascade(root.path(), participants(&["notes", "segments"])).is_err());
    let generations = namespace_delete_cascade(root.path(), deletion()).expect("cascade");
    assert_eq!(generations.len(), 3);
    assert_eq!(state(root.path()), vec![vec![false, true]; 3]);
}

#[cfg(all(feature = "test-seams", unix))]
mod kills {
    use super::test_support;
    use super::*;
    use rand::seq::SliceRandom;
    use std::os::unix::process::ExitStatusExt;
    use zeppelin_embed::lifecycle::namespace_delete_cascade_with_steps;

    #[test]
    fn cascade_sigkill_child() {
        let Ok(root) = std::env::var("ZE225_KILL_ROOT") else {
            return;
        };
        let cut: usize = std::env::var("ZE225_KILL_STEP")
            .expect("cut")
            .parse()
            .expect("integer");
        let mut step = 0;
        namespace_delete_cascade_with_steps(Path::new(&root), deletion(), &mut |_| {
            if step == cut {
                // Actual SIGKILL: no destructors or close/flush cleanup runs.
                unsafe {
                    libc::kill(libc::getpid(), libc::SIGKILL);
                }
            }
            step += 1;
            Ok(())
        })
        .expect("delete");
    }

    #[test]
    fn seeded_sigkill_preserves_whole_families_without_orphans() {
        let baseline = tempfile::tempdir().expect("baseline");
        seed(baseline.path());
        let mut steps = Vec::new();
        namespace_delete_cascade_with_steps(baseline.path(), deletion(), &mut |step| {
            steps.push(step.to_owned());
            Ok(())
        })
        .expect("count steps");
        for required in [
            "prepared append",
            "prepared WAL sync",
            "commit rename",
            "live states installed",
            "accept manifest rename",
            "accept binding rename",
            "cascade checkpoint",
        ] {
            assert!(
                steps.iter().any(|step| step == required),
                "missing {required}"
            );
        }
        let count = steps.len();
        let mut cuts: Vec<_> = (0..count).collect();
        cuts.shuffle(&mut test_support::seeded_rng("ZE225 cascade SIGKILL"));
        let mut observed = std::collections::BTreeSet::new();
        for cut in cuts {
            let root = tempfile::tempdir().expect("root");
            seed(root.path());
            let output = std::process::Command::new(std::env::current_exe().expect("binary"))
                .args(["--exact", "kills::cascade_sigkill_child"])
                .env("ZE225_KILL_ROOT", root.path())
                .env("ZE225_KILL_STEP", cut.to_string())
                .output()
                .expect("child");
            assert_eq!(
                output.status.signal(),
                Some(libc::SIGKILL),
                "cut {cut}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let recovered = state(root.path());
            assert!(
                recovered == vec![vec![true, true]; 3] || recovered == vec![vec![false, true]; 3],
                "orphan or partial family at cut {cut}: {recovered:?}"
            );
            observed.insert(recovered[0][0]);
        }
        assert_eq!(observed.len(), 2, "must observe both publication outcomes");
        eprintln!("ZE-383 cascade SIGKILL cuts: {count}");
    }
}
