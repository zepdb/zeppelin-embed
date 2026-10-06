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
    assert!(
        current
            .get_documents_with_generation(&[], DocumentFields::NONE)
            .expect("generation")
            .0
            >= *generations.first().expect("first participant generation"),
        "purge recovery must preserve the acknowledged generation"
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

#[cfg(feature = "test-seams")]
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
    let preparation = root.path().join("a");
    std::fs::remove_file(preparation.join("manifest.ze")).expect("remove accepted manifest");
    std::fs::remove_file(preparation.join("wal.ze")).expect("remove accepted WAL");
    assert!(
        Store::open(root.path().join("a"), OpenOptions::read_only()).is_err(),
        "a missing accepted store is not an empty namespace"
    );
}

#[cfg(feature = "test-seams")]
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
#[cfg(feature = "test-seams")]
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

#[cfg(feature = "test-seams")]
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

#[test]
fn live_writers_preserve_before_batch_and_after_batch_writes() {
    use zeppelin_embed::lifecycle::{LiveNamespaceMutation, namespace_batch_live};
    let root = tempfile::tempdir().expect("root");
    let a = Store::open(root.path().join("a"), OpenOptions::new()).expect("a");
    let b = Store::open(root.path().join("b"), OpenOptions::new()).expect("b");
    for store in [&a, &b] {
        store
            .ingest(IngestBatch::new(vec![doc(1, 1), doc(4, 1)]))
            .expect("before");
    }
    let mut a_mutation = mutation("a", vec![doc(2, 1)]);
    a_mutation.deletes = vec![DocId::new(4)];
    let mut b_mutation = mutation("b", vec![doc(2, 1)]);
    b_mutation.deletes = vec![DocId::new(4)];
    let generations = namespace_batch_live(
        root.path(),
        vec![
            LiveNamespaceMutation {
                store: &b,
                mutation: b_mutation,
            },
            LiveNamespaceMutation {
                store: &a,
                mutation: a_mutation,
            },
        ],
    )
    .expect("live commit");
    assert_eq!(generations, [4, 4]);
    for store in [&a, &b] {
        store
            .ingest(IngestBatch::new(vec![doc(3, 1)]))
            .expect("after");
        assert!(
            store
                .get_documents(
                    &[DocId::new(1), DocId::new(2), DocId::new(3)],
                    DocumentFields::NONE
                )
                .expect("get")
                .iter()
                .all(Option::is_some)
        );
        assert!(
            store
                .get_documents(&[DocId::new(4)], DocumentFields::NONE)
                .expect("deleted active row")
                .iter()
                .all(Option::is_none)
        );
        store.seal().expect("seal accepted batch");
        store.close().expect("close");
    }
    for name in ["b", "a"] {
        let store = Store::open(root.path().join(name), OpenOptions::new()).expect("reopen");
        assert!(
            store
                .get_documents(
                    &[DocId::new(1), DocId::new(2), DocId::new(3)],
                    DocumentFields::NONE
                )
                .expect("recovered")
                .iter()
                .all(Option::is_some)
        );
    }
}

#[cfg(feature = "test-seams")]
#[test]
fn committed_frames_survive_root_retirement() {
    use zeppelin_embed::lifecycle::{LiveNamespaceMutation, namespace_batch_live_with_steps};
    let root = tempfile::tempdir().expect("root");
    let a = Store::open(root.path().join("a"), OpenOptions::new()).expect("a");
    let b = Store::open(root.path().join("b"), OpenOptions::new()).expect("b");
    for store in [&a, &b] {
        store
            .ingest(IngestBatch::new(vec![doc(1, 1)]))
            .expect("before");
    }
    let mut renames = 0;
    let result = namespace_batch_live_with_steps(
        root.path(),
        vec![
            LiveNamespaceMutation {
                store: &a,
                mutation: mutation("a", vec![doc(2, 1), doc(4, 1)]),
            },
            LiveNamespaceMutation {
                store: &b,
                mutation: mutation("b", vec![doc(2, 1), doc(4, 1)]),
            },
        ],
        &mut |step| {
            if step == "accept binding rename" {
                renames += 1;
                if renames == 2 {
                    return Err(std::io::Error::other("acceptance interrupted"));
                }
            }
            Ok(())
        },
    );
    assert!(result.is_err());
    for store in [&a, &b] {
        assert!(
            store.ingest(IngestBatch::new(vec![doc(5, 1)])).is_err(),
            "queued writes fenced"
        );
        assert!(store.seal().is_err(), "checkpoint fenced");
        store.close().expect("close fenced handle");
    }
    // Recover b without opening a. A's durable receipt already outlives its
    // prepared range; b must use the still-published root decision.
    let b = Store::open(root.path().join("b"), OpenOptions::new()).expect("adopt b alone");
    b.ingest(IngestBatch::new(vec![doc(3, 1)]))
        .expect("later b");
    let recovery_vfs = std::sync::Arc::new(zeppelin_embed::vfs::CountingVfs::new(
        zeppelin_embed::vfs::StdVfs,
    ));
    let a = Store::open_with_test_dependencies(
        root.path().join("a"),
        OpenOptions::new(),
        zeppelin_embed::lifecycle::StoreTestDependencies::new(
            recovery_vfs.clone(),
            std::sync::Arc::new(zeppelin_embed::lifecycle::SystemMonotonicClock),
        ),
    )
    .expect("accepted a");
    assert!(
        recovery_vfs.full_sync_calls() > 0,
        "surviving acceptance must be made durable before ordinary writes"
    );
    a.ingest(IngestBatch::new(vec![doc(3, 1)]))
        .expect("later a");
    zeppelin_embed::lifecycle::namespace_batch_live(
        root.path(),
        vec![
            LiveNamespaceMutation {
                store: &b,
                mutation: mutation("b", vec![doc(6, 1)]),
            },
            LiveNamespaceMutation {
                store: &a,
                mutation: mutation("a", vec![doc(6, 1)]),
            },
        ],
    )
    .expect("normalize previous root and commit next");
    assert_eq!(
        &std::fs::read(root.path().join(".ze-namespaces")).expect("root")[..8],
        b"ZENS0001"
    );
    for store in [&b, &a] {
        store.close().expect("close");
    }
    for name in ["a", "b"] {
        let store =
            Store::open(root.path().join(name), OpenOptions::new()).expect("receipt recovery");
        assert!(
            store
                .get_documents(
                    &[
                        DocId::new(1),
                        DocId::new(2),
                        DocId::new(3),
                        DocId::new(4),
                        DocId::new(6)
                    ],
                    DocumentFields::NONE
                )
                .expect("committed rows")
                .iter()
                .all(Option::is_some)
        );
        store.seal().expect("seal after retirement");
        store.close().expect("close seal");
        let store = Store::open(root.path().join(name), OpenOptions::read_only())
            .expect("repeated recovery");
        assert!(
            store
                .get_documents(&[DocId::new(2), DocId::new(6)], DocumentFields::NONE)
                .expect("sealed committed rows")
                .iter()
                .all(Option::is_some)
        );
    }
}

#[test]
fn late_participant_validation_publishes_nothing() {
    use zeppelin_embed::lifecycle::{LiveNamespaceMutation, namespace_batch_live};
    let root = tempfile::tempdir().expect("root");
    let a = Store::open(root.path().join("a"), OpenOptions::new()).expect("a");
    let b = Store::open(root.path().join("b"), OpenOptions::new()).expect("b");
    for store in [&a, &b] {
        store
            .ingest(IngestBatch::new(vec![doc(1, 2)]))
            .expect("before");
        store.seal().expect("sealed");
    }
    let before = ["a", "b"].map(|name| {
        ["wal.ze", "manifest.ze"]
            .map(|file| std::fs::read(root.path().join(name).join(file)).expect("before bytes"))
    });
    for invalid in 0..5 {
        let mut bad = mutation("b", vec![doc(1, 1)]);
        if invalid == 1 {
            bad.upserts = vec![IngestDocument::new(
                DocumentVersion::new(DocId::new(2), Revision::new(1)),
                vec![f32::NAN, 0.0],
            )];
        }
        if invalid == 2 {
            bad.upserts = vec![doc(2, 1)];
            bad.delete_where = Some(zeppelin_embed::meta::Predicate::Eq {
                column: zeppelin_embed::meta::ColumnId::new(999),
                value: zeppelin_embed::meta::PredicateValue::I64(1),
            });
        }
        if invalid == 3 {
            bad.upserts = vec![doc(2, 1)];
            bad.options = bad.options.with_schema(
                zeppelin_embed::meta::Schema::new(vec![
                    zeppelin_embed::meta::ColumnDefinition::new(
                        zeppelin_embed::meta::ColumnId::new(1),
                        "new",
                        zeppelin_embed::meta::ColumnType::U64,
                        true,
                    ),
                ])
                .expect("declared schema"),
            );
        }
        if invalid == 4 {
            use zeppelin_embed::epoch::{
                ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, Normalization,
                StoreEpoch,
            };
            let tower = EmbeddingTower {
                model_id: "test".into(),
                model_version: "1".into(),
                weights_digest: vec![1],
                dims: 2,
                normalization: Normalization::None,
                prompt_prefix: String::new(),
                max_tokens: 32,
                runtime: EmbeddingRuntime::CpuReference,
                compute_units: ComputeUnits::Cpu,
                os_build: None,
            };
            bad.upserts = vec![doc(2, 1)];
            bad.options = bad.options.with_epoch(StoreEpoch {
                embedding: EmbeddingEpoch {
                    query: tower.clone(),
                    document: tower,
                    alignment_digest: vec![],
                },
                tokenizer: zeppelin_embed::fts::tokenizer::TokenizerConfig::text_default().epoch(),
            });
        }
        assert!(
            namespace_batch_live(
                root.path(),
                vec![
                    LiveNamespaceMutation {
                        store: &b,
                        mutation: bad
                    },
                    LiveNamespaceMutation {
                        store: &a,
                        mutation: mutation("a", vec![doc(1, 3)])
                    },
                ]
            )
            .is_err()
        );
        let after = ["a", "b"].map(|name| {
            ["wal.ze", "manifest.ze"]
                .map(|file| std::fs::read(root.path().join(name).join(file)).expect("after bytes"))
        });
        assert_eq!(before, after);
        assert!(!root.path().join(".ze-namespaces").exists());
    }
    namespace_batch_live(
        root.path(),
        vec![
            LiveNamespaceMutation {
                store: &a,
                mutation: mutation("a", vec![doc(7, 1)]),
            },
            LiveNamespaceMutation {
                store: &b,
                mutation: mutation("b", vec![doc(7, 1)]),
            },
        ],
    )
    .expect("valid batch after refusals");
    #[cfg(feature = "test-seams")]
    {
        let before = ["a", "b"].map(|name| {
            std::fs::read(root.path().join(name).join("wal.ze")).expect("accepted WAL")
        });
        assert!(
            zeppelin_embed::lifecycle::namespace_batch_live_with_steps(
                root.path(),
                vec![
                    LiveNamespaceMutation {
                        store: &a,
                        mutation: mutation("a", vec![doc(9, 1)])
                    },
                    LiveNamespaceMutation {
                        store: &b,
                        mutation: mutation("b", vec![doc(9, 1)])
                    },
                ],
                &mut |step| if step == "prepared append" {
                    Err(std::io::Error::other("precommit abort"))
                } else {
                    Ok(())
                }
            )
            .is_err()
        );
        let after = ["a", "b"].map(|name| {
            std::fs::read(root.path().join(name).join("wal.ze")).expect("restored WAL")
        });
        assert_eq!(before, after, "abort truncates only the prepared suffix");
        namespace_batch_live(
            root.path(),
            vec![
                LiveNamespaceMutation {
                    store: &b,
                    mutation: mutation("b", vec![doc(9, 1)]),
                },
                LiveNamespaceMutation {
                    store: &a,
                    mutation: mutation("a", vec![doc(9, 1)]),
                },
            ],
        )
        .expect("restored WAL bookkeeping supports the next batch");
    }
    for store in [&a, &b] {
        store
            .ingest(IngestBatch::new(vec![doc(8, 1)]))
            .expect("still writable");
    }
}

#[test]
fn incremental_prepare_never_copies_untouched_payloads() {
    use std::sync::Arc;
    use zeppelin_embed::lifecycle::{
        LiveNamespaceMutation, StoreTestDependencies, SystemMonotonicClock, namespace_batch_live,
    };
    use zeppelin_embed::vfs::{CountingVfs, StdVfs};
    for history in [1, 64] {
        let root = tempfile::tempdir().expect("root");
        let vfs = Arc::new(CountingVfs::new(StdVfs));
        let a = Store::open_with_test_dependencies(
            root.path().join("a"),
            OpenOptions::new(),
            StoreTestDependencies::new(vfs.clone(), Arc::new(SystemMonotonicClock)),
        )
        .expect("a");
        let b = Store::open_with_test_dependencies(
            root.path().join("b"),
            OpenOptions::new(),
            StoreTestDependencies::new(vfs.clone(), Arc::new(SystemMonotonicClock)),
        )
        .expect("b");
        for store in [&a, &b] {
            store
                .ingest(IngestBatch::new(
                    (1..=history)
                        .map(|id| doc(id, 1).with_metadata(vec![42; 4096]))
                        .collect(),
                ))
                .expect("sealed baseline");
            store.seal().expect("seal untouched");
            for revision in 1..=history as u64 {
                store
                    .ingest(IngestBatch::new(vec![doc(1000, revision)]))
                    .expect("WAL history");
            }
        }
        let payloads = ["a", "b"].map(|name| {
            std::fs::read_dir(root.path().join(name))
                .expect("segments")
                .map(|entry| entry.expect("entry").path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "zseg"))
                .map(|path| (path.clone(), std::fs::read(path).expect("payload")))
                .collect::<Vec<_>>()
        });
        let before = vfs.bytes_written();
        let appended = vfs.bytes_appended();
        namespace_batch_live(
            root.path(),
            vec![
                LiveNamespaceMutation {
                    store: &a,
                    mutation: mutation("a", vec![doc(2000, 1)]),
                },
                LiveNamespaceMutation {
                    store: &b,
                    mutation: mutation("b", vec![doc(2000, 1)]),
                },
            ],
        )
        .expect("incremental batch");
        eprintln!(
            "ZE-256 incremental history={history}: manifest/receipt bytes={}, prepared WAL bytes={}",
            vfs.bytes_written() - before,
            vfs.bytes_appended() - appended
        );
        assert!(
            vfs.bytes_written() - before < 4096,
            "only bounded manifest metadata written: {}",
            vfs.bytes_written() - before
        );
        assert!(
            vfs.bytes_appended() - appended < 512,
            "only changed prepared frames appended"
        );
        for (path, bytes) in payloads.into_iter().flatten() {
            assert_eq!(std::fs::read(path).expect("same segment"), bytes);
        }
        assert!(
            !std::fs::read_dir(root.path())
                .expect("root entries")
                .any(|entry| entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".ze-batch-"))
        );
    }
}

#[cfg(feature = "test-seams")]
#[test]
fn batch_serializes_with_ingest_seal_merge_and_close() {
    use std::sync::mpsc;
    use zeppelin_embed::lifecycle::{LiveNamespaceMutation, namespace_batch_live_with_steps};
    for reversed in [false, true] {
        let root = tempfile::tempdir().expect("root");
        seed(root.path());
        let a = Store::open(root.path().join("a"), OpenOptions::new()).expect("a");
        let b = Store::open(root.path().join("b"), OpenOptions::new()).expect("b");
        let (prepared_tx, prepared_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let (a, b, root) = (&a, &b, root.path());
            let batch = scope.spawn(move || {
                let mut participants = vec![
                    LiveNamespaceMutation {
                        store: a,
                        mutation: mutation("a", vec![doc(2, 1)]),
                    },
                    LiveNamespaceMutation {
                        store: b,
                        mutation: mutation("b", vec![doc(2, 1)]),
                    },
                ];
                if reversed {
                    participants.reverse();
                }
                let mut stopped = false;
                namespace_batch_live_with_steps(root, participants, &mut |step| {
                    if step == "prepared append" && !stopped {
                        stopped = true;
                        prepared_tx.send(()).expect("prepared signal");
                        release_rx.recv().expect("release coordinator");
                    }
                    Ok(())
                })
                .expect("serialized batch")
            });
            prepared_rx.recv().expect("batch holds participant locks");
            let (started_tx, started_rx) = mpsc::channel();
            let writers = [a, b].map(|store| {
                let started = started_tx.clone();
                scope.spawn(move || {
                    started.send(()).expect("writer admission");
                    store
                        .ingest(IngestBatch::new(vec![doc(4, 1)]))
                        .expect("queued acknowledged write");
                    store.seal().expect("queued seal");
                    store.merge_sealed().expect("queued merge");
                    store.close().expect("queued close");
                })
            });
            for _ in 0..2 {
                started_rx.recv().expect("both queued");
            }
            release_tx.send(()).expect("publish and release");
            assert_eq!(batch.join().expect("batch thread").len(), 2);
            for writer in writers {
                writer.join().expect("writer thread");
            }
        });
        for name in ["b", "a"] {
            let store =
                Store::open(root.path().join(name), OpenOptions::read_only()).expect("reopen");
            assert!(
                store
                    .get_documents(
                        &[DocId::new(1), DocId::new(2), DocId::new(3), DocId::new(4)],
                        DocumentFields::NONE
                    )
                    .expect("all acknowledged writes")
                    .iter()
                    .all(Option::is_some)
            );
        }
    }
}

#[cfg(feature = "test-seams")]
#[test]
fn crash_at_every_prepare_publish_adopt_step() {
    use std::sync::Arc;
    use zeppelin_embed::lifecycle::{
        LiveNamespaceMutation, StoreTestDependencies, SystemMonotonicClock,
        namespace_batch_live_on_vfs,
    };
    use zeppelin_embed::vfs::crash::{CrashStateKind, CrashVfs, MemoryVfs};
    let root = tempfile::tempdir().expect("root");
    // Real directories carry writer locks; all engine bytes use CrashVfs.
    for name in ["a", "b"] {
        std::fs::create_dir(root.path().join(name)).expect("directory");
    }
    let root_path = std::fs::canonicalize(root.path()).expect("canonical root");
    let initial = Arc::new(MemoryVfs::new());
    for name in ["a", "b"] {
        let store = Store::open_with_test_dependencies(
            root_path.join(name),
            OpenOptions::new().with_schema(zeppelin_embed::meta::Schema::timestamp_only()),
            StoreTestDependencies::new(initial.clone(), Arc::new(SystemMonotonicClock)),
        )
        .expect("seed open");
        store
            .ingest(IngestBatch::new(vec![doc(1, 1)]))
            .expect("seed active");
        store.close().expect("seed close");
    }
    let crash = Arc::new(CrashVfs::new(initial.snapshot().expect("baseline")).expect("recorder"));
    let a = Store::open_with_test_dependencies(
        root_path.join("a"),
        OpenOptions::new(),
        StoreTestDependencies::new(crash.clone(), Arc::new(SystemMonotonicClock)),
    )
    .expect("a");
    let b = Store::open_with_test_dependencies(
        root_path.join("b"),
        OpenOptions::new(),
        StoreTestDependencies::new(crash.clone(), Arc::new(SystemMonotonicClock)),
    )
    .expect("b");
    namespace_batch_live_on_vfs(
        &root_path,
        vec![
            LiveNamespaceMutation {
                store: &a,
                mutation: mutation("a", vec![doc(2, 1)]),
            },
            LiveNamespaceMutation {
                store: &b,
                mutation: mutation("b", vec![doc(2, 1)]),
            },
        ],
        crash.as_ref(),
    )
    .expect("record protocol");
    drop(a);
    drop(b);
    let states = crash.crash_states().expect("enumeration");
    assert!(!states.was_capped(), "crash enumeration must be uncapped");
    let total = states.len();
    let mut successes = 0;
    for state in states.iter() {
        let vfs = Arc::new(state.vfs().snapshot().expect("crash image"));
        let before = vfs.files().expect("read-only image");
        let opened = ["b", "a"].map(|name| {
            Store::open_with_test_dependencies(
                root_path.join(name),
                OpenOptions::read_only(),
                StoreTestDependencies::new(vfs.clone(), Arc::new(SystemMonotonicClock)),
            )
            .map(|store| {
                store
                    .get_documents(&[DocId::new(1), DocId::new(2)], DocumentFields::NONE)
                    .expect("read recovered")
                    .iter()
                    .map(Option::is_some)
                    .collect::<Vec<_>>()
            })
        });
        assert_eq!(before, vfs.files().expect("read-only unchanged"));
        if let [Ok(b), Ok(a)] = &opened {
            assert_eq!(a, b, "{:?}", state.kind());
            assert!(
                a == &[true, false] || a == &[true, true],
                "{:?}",
                state.kind()
            );
            successes += 1;
            for name in ["b", "a"] {
                let store = Store::open_with_test_dependencies(
                    root_path.join(name),
                    OpenOptions::new(),
                    StoreTestDependencies::new(vfs.clone(), Arc::new(SystemMonotonicClock)),
                )
                .expect("writable recovery");
                assert_eq!(
                    store
                        .get_documents(&[DocId::new(1), DocId::new(2)], DocumentFields::NONE)
                        .expect("adopt")
                        .iter()
                        .map(Option::is_some)
                        .collect::<Vec<_>>(),
                    *a
                );
                store
                    .ingest(IngestBatch::new(vec![doc(3, 1)]))
                    .expect("write after recover");
                store.close().expect("close recovered");
            }
        } else if matches!(state.kind(), CrashStateKind::Prefix { .. }) {
            panic!(
                "valid operation prefix must recover: {:?}: {:?}",
                state.kind(),
                opened
            );
        } else {
            // Torn/corrupt bytes must fail loudly; no partial state is accepted.
            assert!(
                !matches!(&opened, [Ok(rows), _] | [_, Ok(rows)] if rows != &[true, false] && rows != &[true, true])
            );
        }
    }
    assert!(successes > 0);
    eprintln!(
        "ZE-256 uncapped prepare/publish/adopt crash states: {total}, clean recoveries: {successes}"
    );
}

#[test]
fn namespace_batch_after_torn_single_store_batch_reopens() {
    use zeppelin_embed::lifecycle::{LiveNamespaceMutation, namespace_batch_live};
    use zeppelin_embed::wal::replay::{ReplayTerminator, replay};
    let root = tempfile::tempdir().expect("root");
    let path = root.path().join("a");
    let store = Store::open(&path, OpenOptions::new()).expect("create");
    store
        .ingest(IngestBatch::new(vec![doc(1, 1)]))
        .expect("baseline");
    let baseline = std::fs::metadata(path.join("wal.ze"))
        .expect("metadata")
        .len() as usize;
    store
        .ingest(IngestBatch::new(vec![
            doc(1, 2),
            doc(3, 1),
            doc(4, 1),
            doc(5, 1),
        ]))
        .expect("batch to tear");
    store.close().expect("close source");
    let wal = std::fs::read(path.join("wal.ze")).expect("WAL");
    // Keep exactly members 0 and 1 of the unreturned four-document batch.
    let cut = (baseline + 1..wal.len())
        .find(|&cut| {
            let prefix = replay(&wal[..cut]);
            matches!(prefix.terminator, ReplayTerminator::CleanEnd) && prefix.records.len() == 3
        })
        .expect("inner record boundary");
    std::fs::write(path.join("wal.ze"), &wal[..cut]).expect("tear");
    let writer = Store::open(&path, OpenOptions::new()).expect("recover writer");
    let sibling = Store::open(root.path().join("b"), OpenOptions::new()).expect("sibling");
    namespace_batch_live(
        root.path(),
        vec![
            LiveNamespaceMutation {
                store: &writer,
                mutation: mutation("a", vec![doc(2, 1)]),
            },
            LiveNamespaceMutation {
                store: &sibling,
                mutation: mutation("b", vec![doc(2, 1)]),
            },
        ],
    )
    .expect("acknowledged live namespace batch");
    writer.close().expect("close writer");
    let reopened = Store::open(&path, OpenOptions::new()).expect("reopen acknowledged batch");
    let documents = reopened
        .get_documents(&[1, 2, 3, 4, 5].map(DocId::new), DocumentFields::NONE)
        .expect("documents");
    assert_eq!(
        documents
            .into_iter()
            .map(|d| d.map(|d| d.revision.get()))
            .collect::<Vec<_>>(),
        [Some(1), Some(1), None, None, None]
    );
}

#[test]
fn deleting_batch_refuses_snapshot_before_publication() {
    use zeppelin_embed::lifecycle::{LiveNamespaceMutation, StoreError, namespace_batch_live};
    let root = tempfile::tempdir().expect("root");
    for predicate in [false, true] {
        let a = Store::open(root.path().join("a"), OpenOptions::new()).expect("a");
        let b = Store::open(root.path().join("b"), OpenOptions::new()).expect("b");
        a.ingest(IngestBatch::new(vec![doc(1, 1)])).expect("seed");
        let view = a.open_snapshot().expect("retained view");
        let mut deletion = mutation("a", vec![]);
        if predicate {
            deletion.delete_where = Some(zeppelin_embed::meta::Predicate::Eq {
                column: zeppelin_embed::meta::TIMESTAMP_COLUMN,
                value: zeppelin_embed::meta::PredicateValue::I64(1),
            });
        } else {
            deletion.deletes = vec![DocId::new(1)];
        }
        let result = namespace_batch_live(
            root.path(),
            vec![
                LiveNamespaceMutation {
                    store: &b,
                    mutation: mutation("b", vec![doc(2, 1)]),
                },
                LiveNamespaceMutation {
                    store: &a,
                    mutation: deletion,
                },
            ],
        );
        assert!(
            matches!(result, Err(StoreError::SnapshotViewsOpen)),
            "{result:?}"
        );
        assert!(!root.path().join(".ze-namespaces").exists());
        assert!(
            a.get_documents(&[DocId::new(1)], DocumentFields::NONE)
                .expect("a unchanged")
                .first()
                .expect("row")
                .is_some()
        );
        assert!(
            b.get_documents(&[DocId::new(2)], DocumentFields::NONE)
                .expect("b unchanged")
                .first()
                .expect("row")
                .is_none()
        );
        drop(view);
        drop(a);
        drop(b);
    }
}

#[cfg(feature = "test-seams")]
#[test]
fn reclamation_preserves_current_routes_and_reader_pins() {
    use zeppelin_embed::lifecycle::namespace_reclaim;
    let root = tempfile::tempdir().expect("root");
    seed(root.path());
    namespace_batch(
        root.path(),
        vec![
            mutation("a", vec![doc(2, 1)]),
            mutation("b", vec![doc(2, 1)]),
        ],
    )
    .expect("batch");
    let stale = root.path().join("a/.ze-manifest-123");
    std::fs::copy(root.path().join("a/manifest.ze"), &stale).expect("stale preparation");
    let reader =
        Store::open(root.path().join("a"), OpenOptions::read_only()).expect("second handle reader");
    namespace_reclaim(root.path()).expect("reclaim pinned");
    assert!(
        stale.exists(),
        "reader lease must retain preparation artifacts"
    );
    reader.close().expect("reader close");
    namespace_reclaim(root.path()).expect("reclaim released");
    assert!(
        !stale.exists(),
        "normalized-away staged manifest must be reclaimed"
    );
    assert_eq!(state(root.path(), "a"), vec![true, true, true]);
    let writer = Store::open(root.path().join("a"), OpenOptions::new()).expect("snapshot source");
    let snapshot = writer.open_snapshot().expect("snapshot pin");
    std::fs::copy(root.path().join("a/manifest.ze"), &stale).expect("another stale preparation");
    writer.close().expect("source closed before its snapshot");
    namespace_reclaim(root.path()).expect("snapshot retains OS lease");
    assert!(stale.exists());
    snapshot.close().expect("release snapshot lease");
    let reopened =
        Store::open(root.path().join("a"), OpenOptions::new()).expect("recovery cleanup");
    assert!(
        !stale.exists(),
        "writable recovery must trigger root cleanup"
    );
    reopened.close().expect("close recovered writer");
}

#[cfg(feature = "test-seams")]
fn namespace_envelope(body: &[u8]) -> Vec<u8> {
    let mut bytes = b"ZENS0001".to_vec();
    bytes.extend_from_slice(body);
    bytes.extend_from_slice(&xxhash_rust::xxh3::xxh3_64(&bytes).to_le_bytes());
    bytes
}

#[cfg(feature = "test-seams")]
fn legacy_reclamation_fixture(root: &std::path::Path) {
    seed(root);
    legacy_routes_from_current(root);
}

#[cfg(feature = "test-seams")]
fn legacy_routes_from_current(root: &std::path::Path) {
    for (transaction, names) in [
        (".ze-batch-old", vec!["a", "b"]),
        (".ze-batch-new", vec!["a"]),
        (".ze-batch-aborted", vec!["a"]),
    ] {
        let directory = root.join(transaction);
        std::fs::create_dir(&directory).expect("transaction");
        let mut routes = String::new();
        for name in names {
            let child = directory.join(name);
            std::fs::create_dir(&child).expect("child");
            for entry in std::fs::read_dir(root.join(name)).expect("source files") {
                let entry = entry.expect("entry");
                let filename = entry.file_name();
                if filename == "writer.lock" || filename == ".ze-readers.lock" {
                    continue;
                }
                std::fs::copy(entry.path(), child.join(filename)).expect("legacy payload");
            }
            // Reader pins use an existing lease; ReadOnly must never create it.
            std::fs::write(child.join(".ze-readers.lock"), b"").expect("lease stub");
            let destination = format!("{transaction}/{name}");
            std::fs::write(
                child.join(".ze-prepared"),
                namespace_envelope(destination.as_bytes()),
            )
            .expect("prepared identity");
            routes.push_str(&format!("{name}\t{destination}\n"));
        }
        std::fs::write(
            directory.join("intent.ze"),
            namespace_envelope(routes.as_bytes()),
        )
        .expect("intent");
    }
    std::fs::write(
        root.join(".ze-namespaces"),
        namespace_envelope(b"a\t.ze-batch-new/a\nb\t.ze-batch-old/b\n"),
    )
    .expect("routes");
    std::fs::write(root.join(".ze-namespaces.tmp"), b"unpublished root")
        .expect("abandoned root generation");
    std::fs::write(root.join(".ze-cleanup.tmp"), b"unfinished intent")
        .expect("abandoned cleanup intent");
    let canonical = std::fs::canonicalize(root).expect("root");
    for name in ["a", "b"] {
        std::fs::write(
            root.join(name).join(".ze-namespace-root"),
            namespace_envelope(canonical.to_str().expect("UTF-8").as_bytes()),
        )
        .expect("reference");
    }
}

#[cfg(feature = "test-seams")]
#[test]
fn reclamation_preserves_legacy_siblings_and_reclaims_abandoned_copies() {
    use zeppelin_embed::lifecycle::namespace_reclaim;
    let root = tempfile::tempdir().expect("root");
    legacy_reclamation_fixture(root.path());
    let retired_reader = Store::open(
        root.path().join(".ze-batch-old/a"),
        OpenOptions::read_only(),
    )
    .expect("retired route reader");
    namespace_reclaim(root.path()).expect("cleanup with pinned retired route");
    assert!(root.path().join(".ze-batch-old/a/manifest.ze").exists());
    assert!(root.path().join(".ze-batch-old/b/manifest.ze").exists());
    assert!(root.path().join(".ze-batch-old/intent.ze").exists());
    assert!(!root.path().join(".ze-batch-aborted/a/wal.ze").exists());
    assert!(
        !root.path().join(".ze-batch-aborted/intent.ze").exists(),
        "abandoned transaction metadata must be reclaimed"
    );
    retired_reader.close().expect("release retired reader");
    namespace_reclaim(root.path()).expect("finish cleanup");
    assert!(!root.path().join(".ze-batch-old/a/wal.ze").exists());
    assert!(
        Store::open(root.path().join(".ze-batch-old/a"), OpenOptions::new()).is_err(),
        "retired stubs must never become fallback stores"
    );
    assert_eq!(state(root.path(), "a"), vec![true, false, true]);
    assert_eq!(state(root.path(), "b"), vec![true, false, true]);
}

#[cfg(feature = "test-seams")]
#[test]
fn crash_at_every_namespace_cleanup_step() {
    use std::sync::Arc;
    use zeppelin_embed::lifecycle::namespace_reclaim_on_vfs;
    use zeppelin_embed::vfs::{
        Vfs,
        crash::{CrashStateKind, CrashVfs, MemoryVfs},
    };
    let root = tempfile::tempdir().expect("root");
    legacy_reclamation_fixture(root.path());
    let root_path = std::fs::canonicalize(root.path()).expect("canonical");
    let initial = MemoryVfs::new();
    fn load(vfs: &MemoryVfs, directory: &std::path::Path) {
        for entry in std::fs::read_dir(directory).expect("inventory") {
            let entry = entry.expect("entry");
            if entry.file_type().expect("type").is_dir() {
                load(vfs, &entry.path());
            } else {
                vfs.insert(entry.path(), std::fs::read(entry.path()).expect("bytes"))
                    .expect("insert");
            }
        }
    }
    load(&initial, &root_path);
    let baseline = initial.files().expect("baseline");
    let crash = Arc::new(CrashVfs::new(initial).expect("recorder"));
    let mut steps = std::collections::BTreeSet::new();
    namespace_reclaim_on_vfs(&root_path, crash.as_ref(), &mut |step| {
        steps.insert(step.to_owned());
        Ok(())
    })
    .expect("cleanup");
    assert!(steps.contains("cleanup unlink"));
    assert!(steps.contains("cleanup intent rename"));
    assert!(steps.contains("cleanup completion"));
    let states = crash.crash_states().expect("enumerate");
    assert!(!states.was_capped());
    let mut recovered = 0;
    for state in states.iter() {
        let image = state.vfs().snapshot().expect("image");
        let result = namespace_reclaim_on_vfs(&root_path, &image, &mut |_| Ok(()));
        for (path, bytes) in &baseline {
            if path.starts_with(root_path.join(".ze-batch-new/a"))
                || path.starts_with(root_path.join(".ze-batch-old/b"))
                || path == &root_path.join(".ze-namespaces")
                || path == &root_path.join(".ze-batch-old/intent.ze")
            {
                assert_eq!(
                    image.read(path).expect("reachable artifact"),
                    *bytes,
                    "{:?}: {}",
                    state.kind(),
                    path.display()
                );
            }
        }
        if result.is_ok() {
            assert!(image.open(&root_path.join(".ze-cleanup")).is_err());
            assert!(image.open(&root_path.join(".ze-cleanup.tmp")).is_err());
            assert!(image.open(&root_path.join(".ze-namespaces.tmp")).is_err());
            assert!(
                image
                    .open(&root_path.join(".ze-batch-aborted/a/wal.ze"))
                    .is_err()
            );
            assert!(
                image
                    .open(&root_path.join(".ze-batch-aborted/intent.ze"))
                    .is_err()
            );
            namespace_reclaim_on_vfs(&root_path, &image, &mut |_| Ok(())).expect("repeat recovery");
            recovered += 1;
        } else if matches!(state.kind(), CrashStateKind::Prefix { .. }) {
            panic!(
                "valid cleanup prefix must resume: {:?}: {result:?}",
                state.kind()
            );
        }
    }
    eprintln!(
        "ZE-256 uncapped cleanup crash states: {}, recovered: {recovered}",
        states.len()
    );
    assert!(recovered > 0);
}

#[cfg(feature = "test-seams")]
#[test]
#[ignore = "subprocess lease fixture"]
fn namespace_reader_lease_child() {
    use std::io::{Read, Write};
    let path = std::env::var_os("ZE_NAMESPACE_READER_PATH").expect("child path");
    if std::env::var_os("ZE270_EXPECT_BUSY").is_some() {
        assert!(
            matches!(
                Store::open(std::path::PathBuf::from(path), OpenOptions::read_only()),
                Err(zeppelin_embed::lifecycle::StoreError::StoreBusy { .. })
            ),
            "cleanup contention must be StoreBusy"
        );
        return;
    }
    let reader = Store::open(std::path::PathBuf::from(path), OpenOptions::read_only())
        .expect("child reader");
    println!("READER_ADMITTED");
    std::io::stdout().flush().expect("flush readiness");
    let mut release = [0];
    std::io::stdin()
        .read_exact(&mut release)
        .expect("parent release");
    assert!(
        reader
            .get_documents(&[DocId::new(1)], DocumentFields::NONE)
            .expect("retained read")
            .iter()
            .all(Option::is_some)
    );
    reader.close().expect("release lease");
}

#[cfg(feature = "test-seams")]
#[test]
fn reclamation_reader_lease_excludes_another_process_and_unlink_race() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};
    use zeppelin_embed::lifecycle::{namespace_reclaim, namespace_reclaim_on_vfs};
    use zeppelin_embed::vfs::StdVfs;
    let root = tempfile::tempdir().expect("root");
    seed(root.path());
    namespace_batch(
        root.path(),
        vec![
            mutation("a", vec![doc(2, 1)]),
            mutation("b", vec![doc(2, 1)]),
        ],
    )
    .expect("batch");
    let stale = root.path().join("a/.ze-manifest-123");
    std::fs::copy(root.path().join("a/manifest.ze"), &stale).expect("staged manifest");
    let mut child = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "namespace_reader_lease_child",
            "--ignored",
            "--nocapture",
        ])
        .env("ZE_NAMESPACE_READER_PATH", root.path().join("a"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("child");
    let mut output = BufReader::new(child.stdout.take().expect("stdout"));
    let mut line = String::new();
    loop {
        line.clear();
        assert!(
            output.read_line(&mut line).expect("child ready") > 0,
            "child exited before reader admission"
        );
        if line.contains("READER_ADMITTED") {
            break;
        }
    }
    namespace_reclaim(root.path()).expect("reclaim while another process reads");
    assert!(
        stale.exists(),
        "OS lease must preserve independently admitted reader artifacts"
    );
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(&[1])
        .expect("release");
    assert!(child.wait().expect("child exit").success());
    let mut attempted = false;
    namespace_reclaim_on_vfs(root.path(), &StdVfs, &mut |step| {
        if step == "cleanup intent rename" {
            attempted = true;
            let second = Store::open(root.path().join("a"), OpenOptions::read_only());
            assert!(
                second.is_err(),
                "admission racing unlink must be excluded by the root lease"
            );
            assert!(stale.exists(), "admission was attempted before unlink");
        }
        Ok(())
    })
    .expect("reclaim released reader");
    assert!(attempted);
    assert!(!stale.exists());
    assert_eq!(state(root.path(), "a"), vec![true, true, true]);
}

#[cfg(feature = "test-seams")]
#[test]
fn namespace_cleanup_revalidates_root_reachability_before_unlink() {
    use zeppelin_embed::lifecycle::namespace_reclaim_on_vfs;
    use zeppelin_embed::vfs::StdVfs;
    let root = tempfile::tempdir().expect("root");
    legacy_reclamation_fixture(root.path());
    let selected = root.path().join(".ze-batch-old/a/manifest.ze");
    let before = std::fs::read(&selected).expect("old payload");
    let mut changed = false;
    let result = namespace_reclaim_on_vfs(root.path(), &StdVfs, &mut |step| {
        if step == "cleanup intent rename" {
            changed = true;
            // A stale mark must never grant unlink authority over a now-current
            // route, even when the cleanup intent itself remains well formed.
            std::fs::write(
                root.path().join(".ze-namespaces"),
                namespace_envelope(b"a\t.ze-batch-old/a\nb\t.ze-batch-old/b\n"),
            )?;
        }
        Ok(())
    });
    assert!(changed);
    assert!(
        result.is_err(),
        "changed root reachability must fail loudly"
    );
    assert_eq!(std::fs::read(selected).expect("selected retained"), before);
}

#[cfg(feature = "test-seams")]
#[test]
fn reclamation_preserves_pending_decisions_after_local_checkpoint() {
    use zeppelin_embed::lifecycle::{
        LiveNamespaceMutation, namespace_batch_live_with_steps, namespace_reclaim,
    };
    let root = tempfile::tempdir().expect("root");
    let a = Store::open(root.path().join("a"), OpenOptions::new()).expect("a");
    let b = Store::open(root.path().join("b"), OpenOptions::new()).expect("b");
    let mut acceptances = 0;
    assert!(
        namespace_batch_live_with_steps(
            root.path(),
            vec![
                LiveNamespaceMutation {
                    store: &a,
                    mutation: mutation("a", vec![doc(1, 1)])
                },
                LiveNamespaceMutation {
                    store: &b,
                    mutation: mutation("b", vec![doc(1, 1)])
                },
            ],
            &mut |step| {
                if step == "accept binding rename" {
                    acceptances += 1;
                    if acceptances == 2 {
                        return Err(std::io::Error::other("leave root pending"));
                    }
                }
                Ok(())
            }
        )
        .is_err()
    );
    a.close().expect("close a");
    b.close().expect("close b");
    let pending_root = std::fs::read(root.path().join(".ze-namespaces")).expect("pending root");
    let b = Store::open(root.path().join("b"), OpenOptions::new()).expect("accepted b");
    b.seal().expect("absorb prepared range locally");
    b.close().expect("release b");
    namespace_reclaim(root.path()).expect("accepted decision no longer needs retired WAL frames");
    assert_eq!(
        std::fs::read(root.path().join(".ze-namespaces")).expect("root preserved"),
        pending_root
    );
    for name in ["a", "b"] {
        assert!(
            std::fs::read_dir(root.path().join(name))
                .expect("files")
                .any(|entry| entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".ze-manifest-")),
            "pending manifest retained"
        );
        let reader =
            Store::open(root.path().join(name), OpenOptions::read_only()).expect("accepted reader");
        assert!(
            reader
                .get_documents(&[DocId::new(1)], DocumentFields::NONE)
                .expect("committed row")
                .iter()
                .all(Option::is_some)
        );
    }
}

#[cfg(feature = "test-seams")]
#[test]
fn namespace_cleanup_resume_syncs_authorities_before_unlink() {
    use zeppelin_embed::lifecycle::namespace_reclaim_on_vfs;
    use zeppelin_embed::vfs::{
        StdVfs, SyncKind,
        crash::{CrashOperation, CrashVfs, MemoryVfs},
    };
    for interrupted_step in ["cleanup intent rename", "file sync"] {
        let root = tempfile::tempdir().expect("root");
        legacy_reclamation_fixture(root.path());
        let root_path = std::fs::canonicalize(root.path()).expect("canonical root");
        let mut file_syncs = 0;
        let first = namespace_reclaim_on_vfs(&root_path, &StdVfs, &mut |step| {
            if step == "file sync" {
                file_syncs += 1;
            }
            if step == interrupted_step && (step != "file sync" || file_syncs == 2) {
                return Err(std::io::Error::other(
                    "interrupt before directory durability",
                ));
            }
            Ok(())
        });
        assert!(first.is_err());
        fn load(image: &MemoryVfs, directory: &std::path::Path) {
            for entry in std::fs::read_dir(directory).expect("inventory") {
                let entry = entry.expect("entry");
                if entry.file_type().expect("type").is_dir() {
                    load(image, &entry.path());
                } else {
                    image
                        .insert(entry.path(), std::fs::read(entry.path()).expect("bytes"))
                        .expect("insert");
                }
            }
        }
        let image = MemoryVfs::new();
        load(&image, &root_path);
        let resumed = CrashVfs::new(image).expect("resume recorder");
        namespace_reclaim_on_vfs(&root_path, &resumed, &mut |_| Ok(())).expect("resume");
        let operations = resumed.operations().expect("operations");
        let first_unlink = operations
            .iter()
            .position(|op| matches!(op, CrashOperation::Delete { .. }))
            .expect("unlinks");
        assert!(operations.iter().take(first_unlink).any(|op| matches!(op, CrashOperation::Sync { path, kind: SyncKind::Full } if path == &root_path)), "surviving intent rename must be durable before unlink");
        let directory = root_path.join(".ze-batch-aborted");
        let retired_unlink = operations.iter().position(|op| matches!(op, CrashOperation::Delete { path } if path == &directory.join("intent.ze"))).expect("retired transaction unlink");
        assert!(operations.iter().take(retired_unlink).any(|op| matches!(op, CrashOperation::Sync { path, kind: SyncKind::Full } if path == &directory)), "surviving retirement marker must be durable before unlink");
    }
}

fn assert_no_deleted_bytes(root: &std::path::Path, sentinel: &[u8]) {
    for entry in std::fs::read_dir(root).expect("list") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            assert_no_deleted_bytes(&path, sentinel);
        } else {
            let bytes = std::fs::read(&path).expect("read every file");
            assert!(
                !bytes.windows(sentinel.len()).any(|b| b == sentinel),
                "deleted bytes in {}",
                path.display()
            );
        }
    }
}

#[cfg(feature = "test-seams")]
#[test]
fn physical_delete_erases_current_original_retired_and_abandoned_files() {
    let root = tempfile::tempdir().expect("root");
    let sentinel = b"ZE256-deleted-metadata-and-text-sentinel";
    let vector = vec![13.125_f32, -27.75];
    let vector_bytes = vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect::<Vec<_>>();
    let deleted = |id, rev| {
        IngestDocument::new(
            DocumentVersion::new(DocId::new(id), Revision::new(rev)),
            vector.clone(),
        )
        .with_timestamp(id as i64)
        .with_metadata(sentinel.to_vec())
        .with_text("ZE256-unique-deleted-text")
    };
    for name in ["a", "b"] {
        let store = Store::open(root.path().join(name), OpenOptions::new()).expect("open");
        store
            .ingest(IngestBatch::new(vec![
                deleted(1, 1),
                deleted(3, 1),
                doc(4, 1),
            ]))
            .expect("seed");
        store.seal().expect("seal");
        store
            .ingest(IngestBatch::new(vec![deleted(1, 2)]))
            .expect("active");
    }
    legacy_routes_from_current(root.path());
    let retained = Store::open(
        root.path().join(".ze-batch-old/a"),
        OpenOptions::read_only(),
    )
    .expect("retained historical route");
    assert!(
        namespace_batch(root.path(), mixed()).is_err(),
        "retained bytes must refuse precommit"
    );
    assert_eq!(state(root.path(), "a"), [true, false, true]);
    retained.close().expect("release historical route");
    let mut changes = mixed();
    for change in &mut changes {
        change.deletes.push(DocId::new(999));
    }
    namespace_batch(root.path(), changes).expect("physical commit");
    assert_no_deleted_bytes(root.path(), sentinel);
    assert_no_deleted_bytes(root.path(), b"ZE256-unique-deleted-text");
    assert_no_deleted_bytes(root.path(), &vector_bytes);
    for name in ["a", "b"] {
        assert_eq!(state(root.path(), name), [false, true, false]);
    }
}

#[cfg(feature = "test-seams")]
#[test]
fn interrupted_namespace_purge_resumes_after_reopen() {
    use zeppelin_embed::lifecycle::{LiveNamespaceMutation, namespace_batch_live_with_steps};
    for boundary in [
        "live states installed",
        "purge obligation adopted",
        "accept binding rename",
    ] {
        let root = tempfile::tempdir().expect("root");
        let sentinel = b"ZE256-interrupted-purge-sentinel";
        let stores = ["a", "b"].map(|name| {
            let store = Store::open(root.path().join(name), OpenOptions::new()).expect("open");
            store
                .ingest(IngestBatch::new(vec![
                    doc(1, 1).with_metadata(sentinel.to_vec()),
                    doc(4, 1),
                ]))
                .expect("seed");
            store.seal().expect("seal");
            store
        });
        let participants = stores
            .iter()
            .zip(["a", "b"])
            .map(|(store, name)| {
                let mut mutation = mutation(name, vec![]);
                mutation.deletes = vec![DocId::new(1)];
                LiveNamespaceMutation { store, mutation }
            })
            .collect();
        assert!(
            namespace_batch_live_with_steps(root.path(), participants, &mut |step| {
                if step == boundary {
                    Err(std::io::Error::other("interrupted purge"))
                } else {
                    Ok(())
                }
            })
            .is_err()
        );
        drop(stores);
        if boundary != "live states installed" {
            let reader = Store::open(root.path().join("a"), OpenOptions::read_only())
                .expect("accepted namespace purge permits a logical read");
            assert!(
                reader
                    .get_documents(&[DocId::new(1)], DocumentFields::NONE)
                    .expect("committed delete")
                    .iter()
                    .all(Option::is_none)
            );
            assert!(root.path().join("a/purge.ze").exists());
            reader.close().expect("close without purge recovery");
        }
        for name in ["b", "a"] {
            let store =
                Store::open(root.path().join(name), OpenOptions::new()).expect("resume purge");
            assert!(
                store
                    .get_documents(&[DocId::new(1)], DocumentFields::NONE)
                    .expect("deleted")
                    .iter()
                    .all(Option::is_none)
            );
            assert!(
                store
                    .get_documents(&[DocId::new(4)], DocumentFields::NONE)
                    .expect("survivor")
                    .iter()
                    .all(Option::is_some)
            );
        }
        assert_no_deleted_bytes(root.path(), sentinel);
    }
}

#[cfg(feature = "test-seams")]
#[test]
fn read_only_open_refuses_undecided_namespace_purge() {
    use zeppelin_embed::lifecycle::namespace_batch_with_steps;
    let root = tempfile::tempdir().expect("root");
    seed(root.path());
    assert!(
        namespace_batch_with_steps(root.path(), mixed(), &mut |step| {
            if step == "purge obligation adopted" {
                Err(std::io::Error::other("interrupt before acceptance"))
            } else {
                Ok(())
            }
        })
        .is_err()
    );
    assert!(root.path().join("a/purge.ze").exists());
    std::fs::remove_file(root.path().join(".ze-namespaces")).expect("remove root decision");
    assert!(Store::open(root.path().join("a"), OpenOptions::read_only()).is_err());
}

// CrashVfs owns the authoritative bytes. Anonymous native files provide only
// the mmap interface required by sealed-segment readers, without persisting
// anything outside the image or changing the recorded mutation stream.
#[cfg(feature = "test-seams")]
struct MappedCrashImage<V>(V);
#[cfg(feature = "test-seams")]
impl<V: zeppelin_embed::vfs::Vfs> zeppelin_embed::vfs::Vfs for MappedCrashImage<V> {
    fn ensure_directory(&self, p: &std::path::Path, c: bool) -> std::io::Result<bool> {
        self.0.ensure_directory(p, c)
    }
    fn open(&self, p: &std::path::Path) -> std::io::Result<u64> {
        self.0.open(p)
    }
    fn open_for_map(&self, p: &std::path::Path) -> std::io::Result<std::fs::File> {
        use std::io::Write;
        let mut file = tempfile::tempfile()?;
        file.write_all(&self.0.read(p)?)?;
        Ok(file)
    }
    fn read(&self, p: &std::path::Path) -> std::io::Result<Vec<u8>> {
        self.0.read(p)
    }
    fn read_range(&self, p: &std::path::Path, o: u64, n: usize) -> std::io::Result<Vec<u8>> {
        self.0.read_range(p, o, n)
    }
    fn write(&self, p: &std::path::Path, b: &[u8]) -> std::io::Result<()> {
        self.0.write(p, b)
    }
    fn open_append(
        &self,
        p: &std::path::Path,
    ) -> std::io::Result<Box<dyn zeppelin_embed::vfs::VfsFile>> {
        self.0.open_append(p)
    }
    fn rename(&self, a: &std::path::Path, b: &std::path::Path) -> std::io::Result<()> {
        self.0.rename(a, b)
    }
    fn sync(&self, p: &std::path::Path, k: zeppelin_embed::vfs::SyncKind) -> std::io::Result<()> {
        self.0.sync(p, k)
    }
    fn list(&self, p: &std::path::Path) -> std::io::Result<Vec<std::path::PathBuf>> {
        self.0.list(p)
    }
    fn for_each_direct_child(
        &self,
        p: &std::path::Path,
        v: &mut dyn FnMut(&std::path::Path) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        self.0.for_each_direct_child(p, v)
    }
    fn delete(&self, p: &std::path::Path) -> std::io::Result<()> {
        self.0.delete(p)
    }
}

#[cfg(feature = "test-seams")]
#[test]
fn crash_at_every_namespace_purge_step() {
    use std::sync::Arc;
    use zeppelin_embed::lifecycle::{
        LiveNamespaceMutation, StoreTestDependencies, SystemMonotonicClock,
        namespace_batch_live_on_vfs,
    };
    use zeppelin_embed::vfs::crash::{CrashStateKind, CrashVfs, MemoryVfs};
    let root = tempfile::tempdir().expect("root");
    let root = std::fs::canonicalize(root.path()).expect("canonical");
    for name in ["a", "b"] {
        std::fs::create_dir(root.join(name)).expect("directory");
    }
    let initial = Arc::new(MappedCrashImage(MemoryVfs::new()));
    let sentinel = b"ZE256-purge-bytes";
    for name in ["a", "b"] {
        let store = Store::open_with_test_dependencies(
            root.join(name),
            OpenOptions::new().with_durability(
                zeppelin_embed::lifecycle::durability::DurabilityMode::Derived,
                zeppelin_embed::lifecycle::durability::CommitTier::None,
            ),
            StoreTestDependencies::new(initial.clone(), Arc::new(SystemMonotonicClock)),
        )
        .expect("seed open");
        store
            .ingest(IngestBatch::new(vec![
                doc(1, 1).with_metadata(sentinel.to_vec()),
                doc(4, 1),
            ]))
            .expect("seed");
        store.seal().expect("seal");
        store
            .ingest(IngestBatch::new(vec![
                doc(3, 1).with_metadata(sentinel.to_vec()),
            ]))
            .expect("active target");
    }
    let crash = Arc::new(MappedCrashImage(
        CrashVfs::new(initial.0.snapshot().expect("image")).expect("recorder"),
    ));
    let stores = ["a", "b"].map(|name| {
        Store::open_with_test_dependencies(
            root.join(name),
            OpenOptions::new().with_durability(
                zeppelin_embed::lifecycle::durability::DurabilityMode::Derived,
                zeppelin_embed::lifecycle::durability::CommitTier::None,
            ),
            StoreTestDependencies::new(crash.clone(), Arc::new(SystemMonotonicClock)),
        )
        .expect("participant")
    });
    let participants = stores
        .iter()
        .zip(["a", "b"])
        .map(|(store, name)| {
            let mut mutation = mutation(name, vec![]);
            mutation.deletes = vec![DocId::new(1)];
            mutation.delete_where = Some(zeppelin_embed::meta::Predicate::Eq {
                column: zeppelin_embed::meta::TIMESTAMP_COLUMN,
                value: zeppelin_embed::meta::PredicateValue::I64(3),
            });
            LiveNamespaceMutation { store, mutation }
        })
        .collect();
    namespace_batch_live_on_vfs(&root, participants, crash.as_ref())
        .expect("record deleting protocol");
    drop(stores);
    let operations = crash.0.operations().expect("purge operations");
    for name in ["a", "b"] {
        let temporary = root.join(name).join(".wal.ze.purge.tmp");
        assert!(operations.iter().any(|operation| matches!(operation,
            zeppelin_embed::vfs::crash::CrashOperation::Sync { path, kind: zeppelin_embed::vfs::SyncKind::Full }
                if path == &temporary)), "namespace purge must durably sync survivor WAL even for Derived/None");
        let completion = operations
            .iter()
            .position(|operation| {
                matches!(operation,
            zeppelin_embed::vfs::crash::CrashOperation::Delete { path }
                if path == &root.join(name).join("purge.ze"))
            })
            .expect("purge completion");
        let wal_rename = operations
            .iter()
            .position(|operation| {
                matches!(operation,
            zeppelin_embed::vfs::crash::CrashOperation::Rename { from, .. }
                if from == &temporary)
            })
            .expect("survivor WAL rename");
        assert!(operations.iter().skip(wal_rename + 1).take(completion - wal_rename - 1).any(|operation| matches!(operation,
            zeppelin_embed::vfs::crash::CrashOperation::Sync { path, kind: zeppelin_embed::vfs::SyncKind::Full }
                if path == &root.join(name))), "obligation removal follows durable survivor WAL directory sync");
    }
    let states = crash.0.crash_states().expect("enumerate");
    assert!(
        !states.was_capped(),
        "purge crash enumeration must be uncapped"
    );
    let mut recovered = 0;
    for state in states.iter() {
        let vfs = Arc::new(MappedCrashImage(
            state.vfs().snapshot().expect("crash image"),
        ));
        let mut rows = Vec::new();
        let mut error = None;
        for name in ["b", "a"] {
            match Store::open_with_test_dependencies(
                root.join(name),
                OpenOptions::new().with_durability(
                    zeppelin_embed::lifecycle::durability::DurabilityMode::Derived,
                    zeppelin_embed::lifecycle::durability::CommitTier::None,
                ),
                StoreTestDependencies::new(vfs.clone(), Arc::new(SystemMonotonicClock)),
            ) {
                Ok(store) => rows.push(
                    store
                        .get_documents(
                            &[DocId::new(1), DocId::new(3), DocId::new(4)],
                            DocumentFields::NONE,
                        )
                        .expect("recovered rows")
                        .iter()
                        .map(Option::is_some)
                        .collect::<Vec<_>>(),
                ),
                Err(failure) => {
                    error = Some(failure);
                    break;
                }
            }
        }
        if let Some(error) = error {
            assert!(
                !matches!(state.kind(), CrashStateKind::Prefix { .. }),
                "prefix {:?}: {error}",
                state.kind()
            );
            continue;
        }
        assert_eq!(rows[0], rows[1], "atomic decision {:?}", state.kind());
        assert!(
            rows[0] == [true, true, true] || rows[0] == [false, false, true],
            "{:?}: {rows:?}",
            state.kind()
        );
        if rows[0] == [false, false, true] {
            for (path, bytes) in vfs.0.files().expect("all engine files") {
                assert!(
                    !bytes.windows(sentinel.len()).any(|b| b == sentinel),
                    "{:?}: deleted bytes in {}",
                    state.kind(),
                    path.display()
                );
            }
        }
        recovered += 1;
    }
    eprintln!(
        "ZE-256 uncapped purge crash states: {}, recovered: {recovered}",
        states.len()
    );
    assert!(recovered > 0);
}

#[cfg(unix)]
#[test]
fn ze270_read_only_never_creates_or_writes_reader_lock() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().expect("root");
    let path = root.path().join("store");
    Store::open(&path, OpenOptions::new())
        .expect("create")
        .close()
        .expect("close");
    let lock = path.join(".ze-readers.lock");
    for existing in [false, true] {
        if existing {
            std::fs::write(&lock, b"lease sentinel").expect("lock");
            std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o444))
                .expect("permissions");
        } else {
            std::fs::remove_file(&lock).expect("remove lease");
        }
        let before: std::collections::BTreeSet<_> = std::fs::read_dir(&path)
            .expect("list")
            .map(|e| e.expect("entry").file_name())
            .collect();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o555)).expect("read only");
        let result = Store::open(&path, OpenOptions::read_only());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("restore");
        result
            .expect("read-only admission")
            .close()
            .expect("close reader");
        let after: std::collections::BTreeSet<_> = std::fs::read_dir(&path)
            .expect("list")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(before, after, "read-only must not create a lease");
        if existing {
            assert_eq!(std::fs::read(&lock).expect("contents"), b"lease sentinel");
        }
    }
}

#[cfg(unix)]
#[test]
fn ze270_lock_symlinks_fail_loudly() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().expect("root");
    let outside = root.path().join("outside");
    std::fs::write(&outside, b"untouched").expect("outside");
    for name in ["writer.lock", ".ze-readers.lock"] {
        let path = root.path().join(name.replace('.', "_"));
        Store::open(&path, OpenOptions::new())
            .expect("create")
            .close()
            .expect("close");
        std::fs::remove_file(path.join(name)).expect("remove lock");
        symlink(&outside, path.join(name)).expect("plant link");
        assert!(
            Store::open(&path, OpenOptions::new()).is_err(),
            "{name} must refuse symlink"
        );
        if name == ".ze-readers.lock" {
            assert!(Store::open(&path, OpenOptions::read_only()).is_err());
        }
    }
    assert_eq!(std::fs::read(outside).expect("contents"), b"untouched");
}

#[cfg(feature = "test-seams")]
#[test]
fn ze270_cleanup_reader_in_another_process_gets_store_busy() {
    use zeppelin_embed::lifecycle::namespace_reclaim_on_vfs;
    let root = tempfile::tempdir().expect("root");
    seed(root.path());
    namespace_batch(
        root.path(),
        vec![
            mutation("a", vec![doc(2, 1)]),
            mutation("b", vec![doc(2, 1)]),
        ],
    )
    .expect("batch");
    std::fs::copy(
        root.path().join("a/manifest.ze"),
        root.path().join("a/.ze-manifest-123"),
    )
    .expect("stale");
    let mut attempted = false;
    namespace_reclaim_on_vfs(root.path(), &zeppelin_embed::vfs::StdVfs, &mut |step| {
        if step == "cleanup intent rename" {
            attempted = true;
            let status = std::process::Command::new(std::env::current_exe().expect("binary"))
                .args([
                    "--exact",
                    "namespace_reader_lease_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("ZE_NAMESPACE_READER_PATH", root.path().join("a"))
                .env("ZE270_EXPECT_BUSY", "1")
                .status()
                .expect("child");
            assert!(status.success(), "reader child");
        }
        Ok(())
    })
    .expect("cleanup");
    assert!(attempted);
}
