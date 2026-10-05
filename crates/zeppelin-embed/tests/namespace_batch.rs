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
    let preparation = root.path().join("a");
    std::fs::remove_file(preparation.join("manifest.ze")).expect("remove accepted manifest");
    std::fs::remove_file(preparation.join("wal.ze")).expect("remove accepted WAL");
    assert!(
        Store::open(root.path().join("a"), OpenOptions::read_only()).is_err(),
        "a missing accepted store is not an empty namespace"
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
    assert_eq!(generations, [3, 3]);
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

#[cfg(feature = "test-support")]
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
    #[cfg(feature = "test-support")]
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

#[cfg(feature = "test-support")]
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

#[cfg(feature = "test-support")]
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
