#![cfg(feature = "graph-cypher")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
#[allow(dead_code)]
#[path = "../../../scripts/fixtures/common.rs"]
mod common;

#[allow(clippy::indexing_slicing)]
mod enable_graph {
    use super::common;
    use zeppelin_embed::lifecycle::Store;
    use zeppelin_embed::manifest::decode_manifest;

    #[test]
    fn commits_v3_before_any_graph_record() {
        let scratch = tempfile::tempdir().unwrap();
        let store = Store::open(
            scratch.path(),
            common::options(false).with_schema(common::schema()),
        )
        .unwrap();
        let old = std::fs::read(scratch.path().join("manifest.ze")).unwrap();
        let ack = store
            .ingest(common::batch(vec![common::document(1, "orchard")]))
            .unwrap();
        let wal = std::fs::read(scratch.path().join("wal.ze")).unwrap();
        assert_eq!(store.enable_graph().unwrap(), 2);
        let bytes = std::fs::read(scratch.path().join("manifest.ze")).unwrap();
        assert_eq!(u16::from_le_bytes([bytes[10], bytes[11]]), 3);
        let before = decode_manifest("before", &old).unwrap();
        let after = decode_manifest("after", &bytes).unwrap();
        assert_eq!(after.generation, ack.generation() + 1);
        assert_eq!(after.log_seq, before.log_seq);
        assert_eq!(after.segments, before.segments);
        let graph = after.graph.as_ref().unwrap();
        let state = graph.state().unwrap();
        assert_eq!(state.sequence, 0);
        assert!(state.graph.slots.iter().all(Option::is_none));
        assert_eq!(graph.graph_absorbed_through, ack.seq().get());
        assert_eq!(std::fs::read(scratch.path().join("wal.ze")).unwrap(), wal);
        assert_eq!(store.enable_graph().unwrap(), 2);
        assert_eq!(
            std::fs::read(scratch.path().join("manifest.ze")).unwrap(),
            bytes
        );
        store.close().unwrap();
        let reopened = Store::open(scratch.path(), common::options(true)).unwrap();
        assert_eq!(reopened.count_documents(None, None).unwrap().generation, 2);
        assert_eq!(common::text_hits(&reopened, "orchard"), vec![1]);
        reopened.close().unwrap();
    }
    #[test]
    fn a_read_only_open_refuses() {
        let scratch = tempfile::tempdir().unwrap();
        let store = Store::open(scratch.path(), common::options(false)).unwrap();
        store.close().unwrap();
        for enabled in [false, true] {
            if enabled {
                let writer = Store::open(scratch.path(), common::options(false)).unwrap();
                writer.enable_graph().unwrap();
                writer.close().unwrap();
            }
            let before = std::fs::read(scratch.path().join("manifest.ze")).unwrap();
            let reader = Store::open(scratch.path(), common::options(true)).unwrap();
            assert!(matches!(
                reader.enable_graph(),
                Err(zeppelin_embed::lifecycle::StoreError::ReadOnly)
            ));
            assert_eq!(
                std::fs::read(scratch.path().join("manifest.ze")).unwrap(),
                before
            );
            reader.close().unwrap();
        }
    }

    #[test]
    fn an_enabled_store_seals_after_document_writes() {
        let scratch = tempfile::tempdir().unwrap();
        let store = Store::open(
            scratch.path(),
            common::options(false).with_schema(common::schema()),
        )
        .unwrap();
        store.enable_graph().unwrap();
        let enabled = decode_manifest(
            "enabled",
            &std::fs::read(scratch.path().join("manifest.ze")).unwrap(),
        )
        .unwrap();
        for (id, text) in [(1, "orchard"), (2, "harbor")] {
            let ack = store
                .ingest(common::batch(vec![common::document(id, text)]))
                .unwrap();
            let generation = store
                .seal()
                .expect("seal document-only WAL on enabled store");
            let manifest = decode_manifest(
                "sealed",
                &std::fs::read(scratch.path().join("manifest.ze")).unwrap(),
            )
            .unwrap();
            assert_eq!(manifest.generation, generation);
            assert_eq!(generation, ack.generation() + 1);
            assert_eq!(manifest.log_seq, ack.seq().get());
            assert_eq!(manifest.segments.len(), id as usize);
            let mut expected = enabled.graph.clone().unwrap();
            expected.graph_absorbed_through = ack.seq().get();
            assert_eq!(manifest.graph, Some(expected));
        }
        store.close().unwrap();
        for read_only in [true, false] {
            let reopened = Store::open(scratch.path(), common::options(read_only)).unwrap();
            assert_eq!(common::text_hits(&reopened, "orchard"), vec![1]);
            assert_eq!(common::text_hits(&reopened, "harbor"), vec![2]);
            reopened.close().unwrap();
        }
    }

    #[test]
    fn seal_and_purge_keep_the_graph_section() {
        let scratch = tempfile::tempdir().unwrap();
        let store = Store::open(
            scratch.path(),
            common::options(false).with_schema(common::schema()),
        )
        .unwrap();
        store.enable_graph().unwrap();
        let ack = store
            .ingest(common::batch(vec![
                common::document(1, "orchard"),
                common::document(2, "harbor"),
            ]))
            .unwrap();
        store.close().unwrap();
        // ZE-378 permits rotation only after both sides cover the WAL tail.
        // Place that completed graph fold on the real enabled manifest; its
        // catalog and inventory stay exactly as written by enable_graph.
        let mut manifest = zeppelin_embed::manifest::io::load_manifest(
            &zeppelin_embed::vfs::StdVfs,
            &scratch.path().join("manifest.ze"),
            ack.seq().get(),
        )
        .unwrap();
        manifest.generation = ack.generation();
        manifest.graph.as_mut().unwrap().graph_absorbed_through = ack.seq().get();
        zeppelin_embed::manifest::io::commit_manifest(
            &zeppelin_embed::vfs::StdVfs,
            scratch.path(),
            &manifest,
            zeppelin_embed::lifecycle::durability::DurabilityPolicy::new(
                zeppelin_embed::lifecycle::durability::DurabilityMode::Durable,
                zeppelin_embed::lifecycle::durability::CommitTier::Durable,
            )
            .unwrap(),
        )
        .unwrap();
        let graph = manifest.graph;
        let store = Store::open(scratch.path(), common::options(false)).unwrap();
        store.seal().unwrap();
        assert_eq!(
            decode_manifest(
                "sealed",
                &std::fs::read(scratch.path().join("manifest.ze")).unwrap()
            )
            .unwrap()
            .graph,
            graph
        );
        let token = store
            .purge(&[zeppelin_embed::ingest::DocId::new(1)])
            .unwrap();
        store.await_physical_purge(token).unwrap();
        let mut expected = graph.clone().unwrap();
        expected.generation_absorbed_through = Some(ack.seq().get());
        expected.generation_bumps = vec![(ack.seq().get(), 1)];
        assert_eq!(
            decode_manifest(
                "purged",
                &std::fs::read(scratch.path().join("manifest.ze")).unwrap()
            )
            .unwrap()
            .graph,
            Some(expected)
        );
        store.close().unwrap();
        let reopened = Store::open(scratch.path(), common::options(false)).unwrap();
        assert_eq!(common::text_hits(&reopened, "harbor"), vec![2]);
        assert!(common::text_hits(&reopened, "orchard").is_empty());
        reopened.close().unwrap();
    }

    #[test]
    fn every_crash_state_of_enable_graph_reopens_as_v2_or_empty_v3() {
        use std::sync::Arc;
        use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode};
        use zeppelin_embed::lifecycle::{StoreTestDependencies, SystemMonotonicClock};
        use zeppelin_embed::vfs::crash::{CrashOperation, CrashVfs, MemoryVfs, RecordingVfs};
        // Derived and both durable tiers must establish the version barrier.
        for (mode, tier) in [
            (DurabilityMode::Derived, CommitTier::Ordered),
            (DurabilityMode::Durable, CommitTier::Ordered),
            (DurabilityMode::Durable, CommitTier::Durable),
        ] {
            let scratch = tempfile::tempdir().unwrap();
            let options = common::options(false).with_durability(mode, tier);
            let store = Store::open(scratch.path(), options.clone()).unwrap();
            store.close().unwrap();
            let initial = MemoryVfs::new();
            for entry in std::fs::read_dir(scratch.path()).unwrap() {
                let path = entry.unwrap().path();
                initial
                    .insert(&path, std::fs::read(&path).unwrap())
                    .unwrap();
            }
            let old = initial.files().unwrap();
            let recorder = Arc::new(RecordingVfs::new(zeppelin_embed::vfs::StdVfs));
            let store = Store::open_with_test_dependencies(
                scratch.path(),
                options,
                StoreTestDependencies::new(recorder.clone(), Arc::new(SystemMonotonicClock)),
            )
            .unwrap();
            assert_eq!(store.enable_graph().unwrap(), 1);
            store.close().unwrap();
            let operations = recorder.operations().unwrap();
            assert_eq!(operations.iter().filter(|op| matches!(op, CrashOperation::Rename { to, .. } if to.ends_with("manifest.ze"))).count(), 1);
            assert!(
                !operations
                    .iter()
                    .any(|op| matches!(op, CrashOperation::Append { .. }))
            );
            let states = CrashVfs::from_recorded(initial, operations.clone())
                .unwrap()
                .crash_states()
                .unwrap();
            assert!(!states.was_capped());
            assert!(
                states.len() > operations.len() + 1,
                "must cover more than prefixes"
            );
            let mut versions = std::collections::BTreeSet::new();
            for crash in states.iter() {
                let files = crash.vfs().files().unwrap();
                assert_eq!(
                    files.get(&scratch.path().join("wal.ze")),
                    old.get(&scratch.path().join("wal.ze"))
                );
                let bytes = files.get(&scratch.path().join("manifest.ze")).unwrap();
                let manifest = decode_manifest("crashed", bytes)
                    .unwrap_or_else(|error| panic!("{:?}: {error}", crash.kind()));
                let expected = if let Some(graph) = &manifest.graph {
                    versions.insert(3);
                    let state = graph.state().unwrap();
                    assert!(state.graph.slots.iter().all(Option::is_none));
                    assert_eq!(state.sequence, 0);
                    assert!(state.vector.is_none() && state.text.is_none());
                    1
                } else {
                    versions.insert(2);
                    0
                };
                for read_only in [true, false] {
                    // Reopen the exact materialized crash bytes through the normal
                    // file-backed mapping seam used by graph objects. MemoryVfs
                    // intentionally cannot provide mmap handles.
                    let image = tempfile::tempdir().unwrap();
                    for (path, bytes) in &files {
                        let relative = path.strip_prefix(scratch.path()).unwrap();
                        let target = image.path().join(relative);
                        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                        std::fs::write(target, bytes).unwrap();
                    }
                    let disk_image = || {
                        std::fs::read_dir(image.path())
                            .unwrap()
                            .map(|entry| {
                                let path = entry.unwrap().path();
                                (
                                    path.file_name().unwrap().to_owned(),
                                    std::fs::read(&path).unwrap(),
                                )
                            })
                            .collect::<std::collections::BTreeMap<_, _>>()
                    };
                    let before = disk_image();
                    let reopened = Store::open(image.path(), common::options(read_only))
                        .unwrap_or_else(|error| {
                            panic!("{:?}, read_only={read_only}: {error}", crash.kind())
                        });
                    assert_eq!(
                        reopened.count_documents(None, None).unwrap().generation,
                        expected
                    );
                    assert_eq!(reopened.count_documents(None, None).unwrap().count, 0);
                    reopened.close().unwrap();
                    if read_only {
                        assert_eq!(disk_image(), before);
                    }
                }
            }
            assert_eq!(versions, std::collections::BTreeSet::from([2, 3]));
            eprintln!(
                "{mode:?}/{tier:?}: {} crash states, {} operations",
                states.len(),
                operations.len()
            );
        }
    }

    #[test]
    fn replacement_snapshot_keeps_the_graph_section() {
        use zeppelin_embed::lifecycle::{InMemorySegment, InMemorySegmentFactors, OpenOptions};
        use zeppelin_embed::meta::{AliveSet, ColumnStoreBuilder, Schema};
        let scratch = tempfile::tempdir().unwrap();
        let store = Store::open(scratch.path(), OpenOptions::new()).unwrap();
        store.enable_graph().unwrap();
        let graph = decode_manifest(
            "enabled",
            &std::fs::read(scratch.path().join("manifest.ze")).unwrap(),
        )
        .unwrap()
        .graph;
        let columns = ColumnStoreBuilder::new(Schema::new(Vec::new()).unwrap())
            .finish()
            .unwrap();
        let alive = AliveSet::new(0);
        let prepared = store
            .prepare_segment(InMemorySegment {
                id: zeppelin_embed::segment::SegmentId::new(1, [2; 10]),
                scheme: 4,
                dims: 2,
                codes: Vec::new(),
                factors: InMemorySegmentFactors::Bit4(Vec::new()),
                rescore: Vec::new(),
                columns: &columns,
                alive: &alive,
            })
            .unwrap();
        store.seal_snapshot(prepared).unwrap();
        let mut expected = graph.unwrap();
        expected.generation_absorbed_through = Some(0);
        expected.generation_bumps = vec![(0, 2)];
        assert_eq!(
            decode_manifest(
                "replacement",
                &std::fs::read(scratch.path().join("manifest.ze")).unwrap()
            )
            .unwrap()
            .graph,
            Some(expected)
        );
        store.close().unwrap();
        let reader = Store::open(scratch.path(), OpenOptions::read_only()).unwrap();
        reader.close().unwrap();
    }

    #[test]
    fn a_shared_graph_lock_does_not_authorize_document_writes() {
        use zeppelin_embed::lifecycle::{
            InMemorySegment, InMemorySegmentFactors, OpenOptions, StoreError,
        };
        use zeppelin_embed::meta::{AliveSet, ColumnStoreBuilder, Schema};
        let scratch = tempfile::tempdir().unwrap();
        let writer = Store::open(scratch.path(), OpenOptions::new()).unwrap();
        writer.enable_graph().unwrap();
        writer.close().unwrap();
        let reader = Store::open(scratch.path(), OpenOptions::read_only()).unwrap();
        let before = std::fs::read(scratch.path().join("manifest.ze")).unwrap();
        let columns = ColumnStoreBuilder::new(Schema::new(Vec::new()).unwrap())
            .finish()
            .unwrap();
        let alive = AliveSet::new(0);
        let result = reader.prepare_segment(InMemorySegment {
            id: zeppelin_embed::segment::SegmentId::new(1, [2; 10]),
            scheme: 4,
            dims: 2,
            codes: Vec::new(),
            factors: InMemorySegmentFactors::Bit4(Vec::new()),
            rescore: Vec::new(),
            columns: &columns,
            alive: &alive,
        });
        assert!(matches!(result, Err(StoreError::ReadOnly)));
        assert_eq!(
            std::fs::read(scratch.path().join("manifest.ze")).unwrap(),
            before
        );
        reader.close().unwrap();
    }

    #[test]
    fn copied_enabled_snapshot_opens_at_a_different_path() {
        let scratch = tempfile::tempdir().unwrap();
        let source = scratch.path().join("source");
        let target = scratch.path().join("copy");
        let store = Store::open(
            &source,
            common::options(false).with_schema(common::schema()),
        )
        .unwrap();
        store
            .ingest(common::batch(vec![common::document(1, "orchard")]))
            .unwrap();
        store.enable_graph().unwrap();
        assert_eq!(store.write_snapshot(&target).unwrap(), 2);
        let copied = Store::open(&target, common::options(true)).unwrap();
        assert_eq!(copied.count_documents(None, None).unwrap().generation, 2);
        assert_eq!(common::text_hits(&copied, "orchard"), vec![1]);
        let manifest = std::fs::read(target.join("manifest.ze")).unwrap();
        assert_eq!(manifest, std::fs::read(source.join("manifest.ze")).unwrap());
        let graph = decode_manifest("copy", &manifest).unwrap().graph.unwrap();
        for object in graph.objects {
            let name = format!("graph-{:032x}.zgraph", object.artifact.get());
            assert_eq!(
                std::fs::read(target.join(&name)).unwrap(),
                std::fs::read(source.join(name)).unwrap()
            );
        }
        copied.close().unwrap();
        store.close().unwrap();
    }
}
