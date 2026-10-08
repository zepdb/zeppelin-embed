//! ZE-66 S1: the public graph store lifecycle and `apply_batch` contract.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    reason = "tests fail loudly on the first broken contract"
)]

use super::{GraphStoreError, GraphStoreErrorKind, GraphWriteOutcome, GraphWriteResult};
use crate::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, Store, StoreError, StoreErrorKind};
use crate::property_graph::staging::{
    StageError, StructuredOperation, StructuredWrite, WriteImage,
};
use crate::property_graph::{
    ApplicationKey, CanonicalContents, EntityId, EntityKind, GraphDeleteMode, GraphGeneration,
    GraphName, GraphProperty, GraphRevision, KeyLifecycleError, NodeId, NodeRef, RelId,
    with_local_refs,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}

fn options() -> OpenOptions {
    OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024)
}

fn revision(value: u64) -> GraphRevision {
    GraphRevision::new(value).expect("positive revision")
}

fn generation(value: u64) -> GraphGeneration {
    GraphGeneration::new(value)
}

fn node_key(key: &str) -> ApplicationKey<'_> {
    ApplicationKey::new(EntityKind::Node, "app", key).expect("node key")
}

fn snapshot(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    std::fs::read_dir(path)
        .expect("store directory")
        .map(|entry| {
            let path = entry.expect("store entry").path();
            let bytes = std::fs::read(&path).expect("store file");
            (path, bytes)
        })
        .collect()
}

fn create_node(store: &Store, key: &str, at: u64) -> GraphWriteResult {
    let image = CanonicalContents::node(&mut [], &mut [], Some("text"), None).expect("node image");
    store
        .graph_apply(
            &[StructuredWrite {
                key: node_key(key),
                revision: revision(at),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &control(),
        )
        .expect("create node")
}

fn node_id(result: &GraphWriteResult, index: usize) -> NodeId {
    match result.receipts()[index].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("receipt {index} names a relationship"),
    }
}

fn rel_id(result: &GraphWriteResult, index: usize) -> RelId {
    match result.receipts()[index].entity {
        EntityId::Relationship(relationship) => relationship,
        EntityId::Node(_) => panic!("receipt {index} names a node"),
    }
}

#[test]
fn graph_store_creates_a_graph_only_store_with_forced_durable_writes() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("graph");
    // The caller's options ask for the default Derived durability; the facade
    // replaces it rather than creating a weaker graph store.
    let store = Store::create_graph(&path, options(), None).expect("graph-only store");
    let durable = DurabilityPolicy::new(DurabilityMode::Durable, CommitTier::Durable)
        .expect("durable policy");
    assert_eq!(store.store_for_test().durability_policy, durable);

    let first = create_node(&store, "first", 1);
    // Graph creation publishes manifest generation 1; the first write advances it to 2.
    assert_eq!(first.admitted_generation(), generation(1));
    assert_eq!(
        first.outcome(),
        GraphWriteOutcome::Committed {
            generation: generation(2)
        }
    );
    assert_eq!(first.receipts().len(), 1);
    assert!(!first.receipts()[0].replayed);
    assert_eq!(first.receipts()[0].generation, generation(2));
    store.close_graph().expect("close graph store");
    drop(store);

    // The graph store uses the unified manifest and WAL, with no legacy selector.
    for file in snapshot(&path).keys() {
        let name = file.file_name().and_then(|name| name.to_str()).unwrap();
        assert!(
            name.starts_with("graph-")
                || matches!(
                    name,
                    "writer.lock" | ".ze-readers.lock" | "manifest.ze" | "wal.ze"
                ),
            "unexpected file in a graph-only store: {name}"
        );
    }

    assert!(!path.join("graph-root.ze").exists());
    let reopened = Store::open_graph(&path, options(), None).expect("reopen graph store");
    assert_eq!(reopened.store_for_test().durability_policy, durable);
    let second = create_node(&reopened, "second", 1);
    let committed = generation(second.admitted_generation().get() + 1);
    assert_eq!(
        second.outcome(),
        GraphWriteOutcome::Committed {
            generation: committed
        }
    );
    assert_eq!(second.receipts()[0].generation, committed);
    reopened.close_graph().expect("close reopened store");
    drop(reopened);

    let reader = Store::open_graph_read_only(&path, options(), None).expect("read-only open");
    let before = snapshot(&path);
    let image = CanonicalContents::node(&mut [], &mut [], None, None).expect("node image");
    let refused = reader
        .graph_apply(
            &[StructuredWrite {
                key: node_key("third"),
                revision: revision(1),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &control(),
        )
        .expect_err("a read-only graph store refuses writes");
    assert_eq!(refused.kind(), GraphStoreErrorKind::ReadOnly);
    assert!(refused.nothing_committed());
    assert_eq!(snapshot(&path), before);
    reader.close_graph().expect("close reader");
}

#[test]
fn legacy_open_refuses_a_graph_store_directory_with_a_typed_error() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("graph");
    std::fs::create_dir(&path).expect("legacy graph directory");
    std::fs::write(path.join("graph-root.ze"), b"legacy graph selector").expect("legacy selector");

    let before = snapshot(&path);
    for legacy_options in [OpenOptions::new(), OpenOptions::read_only()] {
        let Err(error) = Store::open(&path, legacy_options) else {
            panic!("legacy open must refuse a graph store directory");
        };
        match &error {
            StoreError::NativeGraphDirectory { path: refused } => assert_eq!(refused, &path),
            other => panic!("expected a typed native graph refusal, observed {other}"),
        }
        assert_eq!(error.kind(), StoreErrorKind::InvalidArgument);
        assert_eq!(snapshot(&path), before);
    }
}

mod legacy_graph_dir {
    use super::*;

    fn hashes(path: &Path) -> BTreeMap<PathBuf, u64> {
        snapshot(path)
            .into_iter()
            .map(|(path, bytes)| (path, xxhash_rust::xxh3::xxh3_64(&bytes)))
            .collect()
    }

    #[test]
    fn a_0_6_0_graph_directory_is_refused_before_any_write() {
        let parent = tempfile::tempdir().expect("temporary parent");
        let path = parent.path().join("graph");
        std::fs::create_dir(&path).expect("legacy graph directory");
        std::fs::write(path.join("graph-root.ze"), b"legacy graph selector")
            .expect("legacy selector");

        // Also cover a crash leaving only the selector's real temporary name:
        // other graph artifacts would mask a broken temporary-file guard.
        let temporary = parent.path().join("temporary");
        std::fs::create_dir(&temporary).expect("temporary directory");
        std::fs::write(
            temporary.join("graph-root-00000000000000000000000000000001.tmp"),
            b"unpublished selector",
        )
        .expect("temporary selector");
        for directory in [&path, &temporary] {
            let before = hashes(directory);
            for options in [OpenOptions::new(), OpenOptions::read_only()] {
                let Err(error) = Store::open(directory, options) else {
                    panic!("legacy open must refuse graph artifacts");
                };
                assert!(
                    matches!(error, StoreError::NativeGraphDirectory { .. }),
                    "{error}"
                );
                assert_eq!(
                    hashes(directory),
                    before,
                    "refusal must not change any file"
                );
            }
        }
    }
}

#[test]
fn graph_open_refuses_a_legacy_store_directory_with_a_typed_error() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("legacy");
    let legacy = Store::open(&path, OpenOptions::new()).expect("legacy store");
    legacy.close_graph().expect("close legacy store");
    drop(legacy);

    let before = snapshot(&path);
    let refusals = [
        Store::open_graph(&path, options(), None)
            .err()
            .expect("graph open must refuse"),
        Store::open_graph_read_only(&path, options(), None)
            .err()
            .expect("read-only graph open must refuse"),
        Store::create_graph(&path, options(), None)
            .err()
            .expect("graph create must refuse"),
    ];
    for error in refusals {
        assert_eq!(error.kind(), GraphStoreErrorKind::LegacyStore, "{error}");
        assert_eq!(error.legacy_store_path(), Some(path.as_path()));
        assert!(error.nothing_committed());
        assert_eq!(snapshot(&path), before);
    }

    // The legacy store is still intact and opens as itself.
    let legacy = Store::open(&path, OpenOptions::new()).expect("legacy store reopens");
    legacy.close_graph().expect("close legacy store");
}

#[test]
fn graph_store_ids_above_u64_round_trip_through_apply_batch() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("wide");
    let first_node = NodeId::new((1_u128 << 64) + 5).expect("wide node seed");
    let first_relationship = RelId::new((1_u128 << 100) + 7).expect("wide relationship seed");
    let store = Store::create_graph_with_allocator_seed_for_test(
        &path,
        options(),
        first_node,
        first_relationship,
    )
    .expect("seeded graph store");

    let source = CanonicalContents::node(&mut [], &mut [], Some("source"), None).expect("source");
    let target = CanonicalContents::node(&mut [], &mut [], Some("target"), None).expect("target");
    static NO_PROPERTIES: [GraphProperty<'static>; 0] = [];
    let created = with_local_refs(|refs| {
        let batch = [
            StructuredWrite {
                key: node_key("source"),
                revision: revision(1),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&source)),
            },
            StructuredWrite {
                key: node_key("target"),
                revision: revision(1),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&target)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "app", "edge")
                    .expect("relationship key"),
                revision: revision(1),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).expect("source slot")),
                    target: NodeRef::Local(refs.node(1).expect("target slot")),
                    relationship_type: GraphName::new("LINKS").expect("type"),
                    properties: &NO_PROPERTIES,
                }),
            },
        ];
        store.graph_apply(&batch, &control())
    })
    .expect("wide batch");
    // Local references resolve to the identity at their request index; every
    // identity keeps its bits above u64.
    let source_id = node_id(&created, 0);
    let target_id = node_id(&created, 1);
    let edge_id = rel_id(&created, 2);
    assert!(source_id.get() > u128::from(u64::MAX));
    assert!(edge_id.get() > u128::from(u64::MAX));
    assert_eq!(source_id, first_node);
    assert_eq!(target_id.get(), first_node.get() + 1);
    assert_eq!(edge_id, first_relationship);
    store.close_graph().expect("close seeded store");
    drop(store);

    // After reopen the store resolves the full 128-bit identities it returned:
    // a Put names each incarnation by ID, and a truncated ID would conflict.
    let reopened = Store::open_graph(&path, options(), None).expect("reopen seeded store");
    let replacement =
        CanonicalContents::node(&mut [], &mut [], Some("replaced"), None).expect("replacement");
    let replaced = reopened
        .graph_apply(
            &[
                StructuredWrite {
                    key: node_key("source"),
                    revision: revision(2),
                    operation: StructuredOperation::Put(EntityId::Node(source_id)),
                    image: Some(WriteImage::Node(&replacement)),
                },
                StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Relationship, "app", "edge")
                        .expect("relationship key"),
                    revision: revision(2),
                    operation: StructuredOperation::Delete(
                        EntityId::Relationship(edge_id),
                        GraphDeleteMode::Restrict,
                    ),
                    image: None,
                },
            ],
            &control(),
        )
        .expect("wide identities resolve after reopen");
    assert_eq!(replaced.receipts()[0].entity, EntityId::Node(source_id));
    assert_eq!(
        replaced.receipts()[1].entity,
        EntityId::Relationship(edge_id)
    );
    assert!(matches!(
        replaced.outcome(),
        GraphWriteOutcome::Committed { .. }
    ));
    reopened.close_graph().expect("close reopened store");
}

#[test]
fn graph_store_exact_retry_replays_with_its_original_generation() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let store =
        Store::create_graph(parent.path().join("retry"), options(), None).expect("graph store");
    let first = create_node(&store, "first", 1);
    let installed = first.receipts()[0];

    // No automatic retry happened: the caller resubmits and gets a replay at
    // the original generation, and nothing new is written.
    let replayed = create_node(&store, "first", 1);
    assert_eq!(replayed.outcome(), GraphWriteOutcome::Replayed);
    assert_eq!(replayed.admitted_generation(), generation(2));
    assert!(replayed.receipts()[0].replayed);
    assert_eq!(replayed.receipts()[0].entity, installed.entity);
    assert_eq!(replayed.receipts()[0].generation, generation(2));

    // A mixed batch commits once; each receipt carries its own record.
    let image = CanonicalContents::node(&mut [], &mut [], Some("text"), None).expect("image");
    let mixed = store
        .graph_apply(
            &[
                StructuredWrite {
                    key: node_key("first"),
                    revision: revision(1),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                },
                StructuredWrite {
                    key: node_key("second"),
                    revision: revision(1),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                },
            ],
            &control(),
        )
        .expect("mixed batch");
    assert_eq!(
        mixed.outcome(),
        GraphWriteOutcome::Committed {
            generation: generation(3)
        }
    );
    assert_eq!(mixed.admitted_generation(), generation(2));
    assert!(mixed.receipts()[0].replayed);
    assert_eq!(mixed.receipts()[0].generation, generation(2));
    assert!(!mixed.receipts()[1].replayed);
    assert_eq!(mixed.receipts()[1].generation, generation(3));

    // An empty batch changes nothing.
    let empty = store.graph_apply(&[], &control()).expect("empty batch");
    assert_eq!(empty.outcome(), GraphWriteOutcome::NoOp);
    assert_eq!(empty.admitted_generation(), generation(3));
    assert!(empty.receipts().is_empty());
    store.close_graph().expect("close graph store");
}

#[test]
fn graph_store_refuses_a_stale_incarnation_with_nothing_committed() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let store =
        Store::create_graph(parent.path().join("stale"), options(), None).expect("graph store");
    let image = CanonicalContents::node(&mut [], &mut [], Some("text"), None).expect("image");
    let first = node_id(&create_node(&store, "fenced", 1), 0);
    store
        .graph_apply(
            &[StructuredWrite {
                key: node_key("fenced"),
                revision: revision(2),
                operation: StructuredOperation::Delete(
                    EntityId::Node(first),
                    GraphDeleteMode::Restrict,
                ),
                image: None,
            }],
            &control(),
        )
        .expect("delete first incarnation");
    let recreated = store
        .graph_apply(
            &[StructuredWrite {
                key: node_key("fenced"),
                revision: revision(3),
                operation: StructuredOperation::Recreate(revision(2)),
                image: Some(WriteImage::Node(&image)),
            }],
            &control(),
        )
        .expect("recreate the key");
    assert_eq!(
        recreated.outcome(),
        GraphWriteOutcome::Committed {
            generation: generation(4)
        }
    );
    assert_ne!(node_id(&recreated, 0), first);

    let stale = store
        .graph_apply(
            &[StructuredWrite {
                key: node_key("fenced"),
                revision: revision(4),
                operation: StructuredOperation::Put(EntityId::Node(first)),
                image: Some(WriteImage::Node(&image)),
            }],
            &control(),
        )
        .expect_err("the retired incarnation is refused");
    assert_eq!(stale.kind(), GraphStoreErrorKind::Constraint, "{stale}");
    assert!(stale.nothing_committed());
    assert!(matches!(
        stale.stage_error(),
        Some(StageError::Lifecycle(
            KeyLifecycleError::IncarnationConflict
        ))
    ));

    // Nothing committed: the next change is admitted at the same generation
    // the refused batch saw, and publishes the one after it.
    let next = create_node(&store, "after", 1);
    assert_eq!(next.admitted_generation(), generation(4));
    assert_eq!(
        next.outcome(),
        GraphWriteOutcome::Committed {
            generation: generation(5)
        }
    );
    store.close_graph().expect("close graph store");
}

#[test]
fn graph_write_results_stay_readable_after_close() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let store =
        Store::create_graph(parent.path().join("closed"), options(), None).expect("graph store");
    let result = create_node(&store, "kept", 1);
    let expected = result.clone();
    store.close_graph().expect("close graph store");
    // Closing twice is harmless.
    store.close_graph().expect("close again");

    let late = store
        .graph_apply(&[], &control())
        .expect_err("a closed store admits nothing");
    assert_eq!(late.kind(), GraphStoreErrorKind::Closed);
    assert!(late.nothing_committed(), "{late}");
    drop(store);

    assert_eq!(result, expected);
    assert_eq!(
        result.outcome(),
        GraphWriteOutcome::Committed {
            generation: generation(2)
        }
    );
    let receipts = result.into_receipts();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].revision, revision(1));
    assert_eq!(receipts[0].generation, generation(2));
}

#[test]
fn macos_thirteen_is_refused_and_fourteen_admitted() {
    assert!(!super::macos_admits_graph((13, 6)));
    assert!(super::macos_admits_graph((14, 0)));
    assert!(super::macos_admits_graph((15, 1)));
}

#[test]
fn unsupported_platform_error_reports_kind_and_versions() {
    let error = super::GraphStoreError {
        cause: super::Cause::UnsupportedPlatform {
            required: (14, 0),
            observed: Some((13, 6)),
            probe_error: String::new(),
        },
    };
    assert_eq!(error.kind(), GraphStoreErrorKind::Unsupported);
    assert!(error.nothing_committed());
    assert_eq!(error.operator(), None);
    assert!(error.counters().is_none());
    assert!(error.legacy_store_path().is_none());
    assert!(
        error
            .to_string()
            .contains("graph store requires macOS 14.0 or newer; this host reports 13.6")
    );
    let error = super::GraphStoreError {
        cause: super::Cause::UnsupportedPlatform {
            required: (14, 0),
            observed: None,
            probe_error: "probe failed".to_owned(),
        },
    };
    assert_eq!(error.kind(), GraphStoreErrorKind::Unsupported);
    assert!(error.nothing_committed());
    assert!(
        error
            .to_string()
            .contains("could not determine the macOS version: probe failed")
    );
}

#[test]
fn relationship_rules_enforce_restrict_and_transitive_cascade_after_reopen() {
    use crate::property_graph::catalog::{OnDelete, RelationshipRule};
    for policy in [OnDelete::Restrict, OnDelete::Cascade] {
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("rules");
        let rules = [RelationshipRule {
            relationship_type: GraphName::new("IN").unwrap(),
            on_delete: policy,
        }];
        let store =
            Store::create_graph_with_relationship_types(&path, options(), None, &rules).unwrap();
        let a = node_id(&create_node(&store, "a", 1), 0);
        let b = node_id(&create_node(&store, "b", 1), 0);
        let c = node_id(&create_node(&store, "c", 1), 0);
        for (key, source, target) in [("ab", a, b), ("bc", b, c)] {
            store
                .graph_apply(
                    &[StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Relationship, "app", key).unwrap(),
                        revision: revision(1),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Relationship {
                            source: NodeRef::Existing(source),
                            target: NodeRef::Existing(target),
                            relationship_type: GraphName::new("IN").unwrap(),
                            properties: &[],
                        }),
                    }],
                    &control(),
                )
                .unwrap();
        }
        store.close_graph().unwrap();
        let store = Store::open_graph(&path, options(), None).unwrap();
        let result = store.graph_apply(
            &[StructuredWrite {
                key: node_key("c"),
                revision: revision(2),
                operation: StructuredOperation::Delete(
                    EntityId::Node(c),
                    if policy == OnDelete::Cascade {
                        GraphDeleteMode::Restrict
                    } else {
                        GraphDeleteMode::Detach
                    },
                ),
                image: None,
            }],
            &control(),
        );
        if policy == OnDelete::Restrict {
            assert!(result.unwrap_err().nothing_committed());
        } else {
            assert_eq!(result.unwrap().receipts().len(), 1);
        }
        store.close_graph().unwrap();
        let store = Store::open_graph(&path, options(), None).unwrap();
        let nodes = store
            .get_nodes(&[a, b, c], super::GraphGetOptions::default(), &control())
            .unwrap();
        assert!(
            nodes
                .nodes()
                .iter()
                .all(|n| n.is_some() == (policy == OnDelete::Restrict))
        );
        store.close_graph().unwrap();
    }
}

#[test]
fn relationship_cascade_revision_overflow_refuses_the_entire_mutation() {
    use crate::property_graph::catalog::{OnDelete, RelationshipRule};
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("rules");
    let rules = [RelationshipRule {
        relationship_type: GraphName::new("IN").unwrap(),
        on_delete: OnDelete::Cascade,
    }];
    let store =
        Store::create_graph_with_relationship_types(&path, options(), None, &rules).unwrap();
    let child = node_id(&create_node(&store, "child", u64::MAX), 0);
    let target = node_id(&create_node(&store, "parent", 1), 0);
    store
        .graph_apply(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "app", "in").unwrap(),
                revision: revision(1),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Existing(child),
                    target: NodeRef::Existing(target),
                    relationship_type: GraphName::new("IN").unwrap(),
                    properties: &[],
                }),
            }],
            &control(),
        )
        .unwrap();
    let error = store
        .graph_apply(
            &[StructuredWrite {
                key: node_key("parent"),
                revision: revision(2),
                operation: StructuredOperation::Delete(
                    EntityId::Node(target),
                    GraphDeleteMode::Detach,
                ),
                image: None,
            }],
            &control(),
        )
        .unwrap_err();
    assert!(error.nothing_committed());
    store.close_graph().unwrap();
    let store = Store::open_graph(&path, options(), None).unwrap();
    assert!(
        store
            .get_nodes(
                &[child, target],
                super::GraphGetOptions::default(),
                &control()
            )
            .unwrap()
            .nodes()
            .iter()
            .all(Option::is_some)
    );
    store.close_graph().unwrap();
}

#[test]
fn ze257_small_batch_does_not_reserve_maximum_wal_envelope() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let store =
        Store::create_graph(parent.path().join("graph"), options(), None).expect("graph store");
    let _schedule = crate::property_graph::storage::search::native_vector_index_test_schedule(
        Some(8 * 1024 * 1024),
        None,
        |_| {},
    );
    create_node(&store, "small", 1);
    store.close_graph().expect("close graph store");
}

#[test]
fn ze194_single_batch_400_nodes_with_and_without_vectors() {
    use crate::epoch::{ComputeUnits, EmbeddingRuntime, EmbeddingTower, Normalization};
    use crate::property_graph::CanonicalEmbedding;

    for with_vectors in [false, true] {
        let parent = tempfile::tempdir().expect("temporary parent");
        let tower = EmbeddingTower {
            model_id: "ze194-document".into(),
            model_version: "1".into(),
            weights_digest: vec![0x19, 0x4],
            dims: 2,
            normalization: Normalization::None,
            prompt_prefix: String::new(),
            max_tokens: 32,
            runtime: EmbeddingRuntime::CpuReference,
            compute_units: ComputeUnits::Cpu,
            os_build: None,
        };
        let store = Store::create_graph(
            parent.path().join("graph"),
            options(),
            with_vectors.then(|| tower.clone()),
        )
        .expect("graph store");
        let embedding = with_vectors
            .then(|| CanonicalEmbedding::new(&tower, &[0.25, 0.75]).expect("embedding"));
        let image = CanonicalContents::node(&mut [], &mut [], None, embedding).expect("node image");
        let keys: Vec<_> = (0..400).map(|index| format!("ze194-{index}")).collect();
        let requests: Vec<_> = keys
            .iter()
            .map(|key| StructuredWrite {
                key: node_key(key),
                revision: revision(1),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            })
            .collect();
        let result = store
            .graph_apply(&requests, &control())
            .expect("400-node single batch under production preparation allowance");
        assert_eq!(result.admitted_generation(), generation(1));
        assert_eq!(
            result.outcome(),
            GraphWriteOutcome::Committed {
                generation: generation(2),
            }
        );
        assert_eq!(result.receipts().len(), 400);
        assert!(
            result
                .receipts()
                .iter()
                .all(|receipt| receipt.generation == generation(2) && !receipt.replayed)
        );
        let nodes: Vec<_> = (0..400).map(|index| node_id(&result, index)).collect();
        assert_eq!(
            nodes
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            400
        );
        let read = store
            .get_nodes(&nodes, super::GraphGetOptions::default(), &control())
            .expect("read every committed node");
        assert_eq!(read.nodes().len(), 400);
        assert!(read.nodes().iter().all(Option::is_some));
        drop(read);
        store.close_graph().expect("close graph store");
    }
}

#[test]
fn ze257_batch_work_is_admitted_per_mutation() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let store =
        Store::create_graph(parent.path().join("graph"), options(), None).expect("graph store");
    let keys: Vec<_> = (0..100).map(|index| index.to_string()).collect();
    let mut labels = [GraphName::new("Doc").expect("label")];
    let mut properties = [GraphProperty::new(
        GraphName::new("rank").expect("property"),
        crate::property_graph::PropertyValue::new(crate::property_graph::PropertyData::I64(7))
            .expect("value"),
    )];
    let image =
        CanonicalContents::node(&mut labels, &mut properties, None, None).expect("node image");
    let requests: Vec<_> = keys
        .iter()
        .map(|key| StructuredWrite {
            key: node_key(key),
            revision: revision(1),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(&image)),
        })
        .collect();
    store
        .graph_apply(&requests, &control())
        .expect("100-node batch under defaults");
    store.close_graph().expect("close graph store");
}

#[test]
#[ignore = "opens the explicitly supplied Node scale fixture"]
fn ze257_recovery_of_node_scale_fixture() {
    let path = std::env::var("ZE_GRAPH_SCALE_FIXTURE").expect("Node scale fixture path");
    let store = Store::open_graph(&path, options(), None).expect("reopen Node scale fixture");
    let metrics = crate::lifecycle::native_graph::open_metrics_for_test();
    let serial_probes = crate::lifecycle::native_graph::serial_probes_for_test();
    let mut files = 0_u64;
    let mut bytes = 0_u64;
    for entry in std::fs::read_dir(&path).unwrap() {
        files += 1;
        bytes += entry.unwrap().metadata().unwrap().len();
    }
    eprintln!(
        "ZE257 recovery work/peak: {metrics:?}; serial_probes={serial_probes}; files={files}; bytes={bytes}"
    );
    assert!(metrics.1 <= 32 * 1024 * 1024);
    assert!(metrics.3 <= 256 * 1024 * 1024);
    assert!(serial_probes <= files * 8);
    store.close_graph().expect("close recovered scale fixture");
}

#[test]
fn ze257_checkpoint_recovery_work_scales_linearly() {
    // S2 measures 23,511 / 24,036 / 24,304 work per node (rounded up)
    // at 128 / 256 / 512 nodes. Bound logical size, not amplified file bytes.
    const WORK_PER_NODE_LIMIT: u64 = 25_000;
    let mut measurements = Vec::new();
    for count in [128, 256, 512] {
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("graph");
        let store = Store::create_graph(&path, options(), None).unwrap();
        let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        for first in (0..count).step_by(128) {
            let keys: Vec<_> = (first..first + 128)
                .map(|index| index.to_string())
                .collect();
            let requests: Vec<_> = keys
                .iter()
                .map(|key| StructuredWrite {
                    key: node_key(key),
                    revision: revision(1),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                })
                .collect();
            store.graph_apply(&requests, &control()).unwrap();
        }
        store
            .store_for_test()
            .checkpoint_native_graph(&control())
            .unwrap();
        store.close_graph().unwrap();
        let store = Store::open_graph(&path, options(), None).unwrap();
        let (work, peak, candidates, resident_peak) =
            crate::lifecycle::native_graph::open_metrics_for_test();
        let bytes: u64 = std::fs::read_dir(&path)
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum();
        eprintln!(
            "ZE257 checkpoint nodes={count} work={work} peak={peak} bytes={bytes} fence_candidates={candidates} resident_peak={resident_peak}"
        );
        assert!(work > 0);
        assert!(
            work <= count as u64 * WORK_PER_NODE_LIMIT,
            "recovery work {work} exceeds {WORK_PER_NODE_LIMIT} per node at {count} nodes"
        );
        measurements.push((count as u64, work));
        assert!(peak <= 32 * 1024 * 1024);
        assert!(resident_peak <= 256 * 1024 * 1024);
        assert_eq!(
            candidates, count as u64,
            "one indexed fence candidate per keyed node"
        );
        store.close_graph().unwrap();
    }
    // Check both doublings and the complete 4x span with 5% tolerance.
    for (index, &(small_nodes, small_work)) in measurements.iter().enumerate() {
        for &(large_nodes, large_work) in measurements.iter().skip(index + 1) {
            assert!(
                large_work * small_nodes * 20 <= small_work * large_nodes * 21,
                "recovery work grew superlinearly: {small_nodes} nodes/{small_work} work -> {large_nodes} nodes/{large_work} work"
            );
        }
    }
}

#[test]
fn ze257_close_checkpoints_the_graph_replay_tail() {
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("graph");
    let store = Store::create_graph(&path, options(), None).unwrap();
    for key in ["one", "two", "three"] {
        create_node(&store, key, 1);
    }
    store.close_graph().unwrap();
    store.close_graph().unwrap();
    let store = Store::open_graph(&path, options(), None).unwrap();
    let (_, _, fence_candidates, _) = crate::lifecycle::native_graph::open_metrics_for_test();
    assert_eq!(
        fence_candidates, 3,
        "clean close leaves only the checkpoint to validate"
    );
    store.close_graph().unwrap();
}

#[test]
fn ze257_close_checkpoint_failure_is_reported_and_releases_the_writer() {
    use crate::lifecycle::native_graph::tests::publication::{FaultPoint, RecordingVfs};
    use std::sync::Arc;
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("graph");
    let vfs = Arc::new(RecordingVfs::default());
    let store = Store::create_native_graph_with_infrastructure(
        &path,
        options().with_durability(DurabilityMode::Durable, CommitTier::Durable),
        None,
        vfs.clone(),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .unwrap();
    create_node(&store, "durable", 1);
    // Unified close publishes the manifest instead of creating a legacy root.
    vfs.arm_fault(FaultPoint::ManifestSync);
    assert_eq!(
        store.close_graph().unwrap_err().kind(),
        GraphStoreErrorKind::Storage
    );
    vfs.assert_fired_once();
    store.close_graph().unwrap();
    let reopened = Store::open_graph(&path, options(), None).unwrap();
    assert_eq!(
        reopened
            .store_for_test()
            .admit_native_read()
            .unwrap()
            .bundle()
            .base()
            .generation,
        generation(2)
    );
    assert!(
        reopened
            .get_nodes(
                &[NodeId::new(1).unwrap()],
                super::GraphGetOptions::default(),
                &control()
            )
            .unwrap()
            .nodes()[0]
            .is_some()
    );
    let next = create_node(&reopened, "next", 1);
    assert_eq!(
        next.outcome(),
        GraphWriteOutcome::Committed {
            generation: generation(next.admitted_generation().get() + 1),
        }
    );
    reopened.close_graph().unwrap();
}

#[test]
fn ze397_manifest_io_and_fenced_writer_keep_storage_class() {
    use crate::lifecycle::native_graph::tests::publication::{FaultPoint, RecordingVfs};
    use std::sync::Arc;
    let parent = tempfile::tempdir().unwrap();
    let vfs = Arc::new(RecordingVfs::default());
    let store = Store::create_native_graph_with_infrastructure(
        parent.path().join("graph"),
        options().with_durability(DurabilityMode::Durable, CommitTier::Durable),
        None,
        vfs.clone(),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .unwrap();
    create_node(&store, "durable", 1);
    vfs.arm_fault(FaultPoint::ManifestSync);
    let error = store.checkpoint_native_graph(&control()).unwrap_err();
    vfs.assert_fired_once();
    assert_eq!(
        GraphStoreError::graph(error).kind(),
        GraphStoreErrorKind::Storage
    );
    let fenced = store.checkpoint_native_graph(&control()).unwrap_err();
    assert!(
        fenced
            .to_string()
            .contains("writer publication did not complete")
    );
    assert_eq!(
        GraphStoreError::graph(fenced).kind(),
        GraphStoreErrorKind::Storage
    );
}

#[test]
fn ze257_recovery_sizes_serial_inventory_from_actual_artifacts() {
    use crate::property_graph::storage::artifact::{
        self, ArtifactId, ArtifactIdentity, ContainerKind,
    };
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("graph");
    let store = Store::create_graph(&path, options(), None).unwrap();
    let lease = store.store_for_test().admit_native_read().unwrap();
    let store_id = lease.bundle().base().store;
    drop(lease);
    store.close_graph().unwrap();
    // Valid pre-WAL orphan objects still consume serials. Their descriptors
    // fit comfortably in the existing budget even beyond the old 8192 cap.
    let mut bytes = vec![0; artifact::encoded_len(ContainerKind::Object, &[]).unwrap()];
    for serial in 64..64 + 8192 {
        let id = ArtifactId::new(0x25700000000000000000000000000000 + u128::from(serial)).unwrap();
        artifact::encode_into(
            ContainerKind::Object,
            ArtifactIdentity {
                store: store_id,
                artifact: id,
                generation: generation(0),
                creation_serial: serial,
            },
            &[],
            &mut bytes,
        )
        .unwrap();
        std::fs::write(path.join(format!("graph-{:032x}.zgraph", id.get())), &bytes).unwrap();
    }
    let store = Store::open_graph(&path, options(), None)
        .expect("actual descriptor count fits default memory");
    let probes = crate::lifecycle::native_graph::serial_probes_for_test();
    eprintln!("ZE257 serial inventory probes={probes}");
    assert!(probes < 8192 * 8, "serial inventory probes: {probes}");
    create_node(&store, "after-orphans", 1);
    store.close_graph().unwrap();
}

#[test]
fn ze260_maintenance_policy_and_public_step() {
    use super::GraphMaintenancePolicy;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph");
    let store = Store::create_graph(&path, options(), None).unwrap();
    let policy = GraphMaintenancePolicy::default();
    assert!(policy.automatic);
    assert_eq!(policy.reclaim_after_bytes, 64 * 1024 * 1024);
    assert_eq!(
        store
            .set_graph_maintenance_policy(GraphMaintenancePolicy {
                automatic: false,
                reclaim_after_bytes: 0
            })
            .unwrap_err()
            .kind(),
        GraphStoreErrorKind::InvalidRequest
    );
    store
        .set_graph_maintenance_policy(GraphMaintenancePolicy {
            automatic: false,
            reclaim_after_bytes: 1024 * 1024,
        })
        .unwrap();
    let report = store.graph_maintain_step(&control()).unwrap();
    assert!(report.cycle_complete);
    assert_eq!(report.new_pack_bytes, 0);
    store.close_graph().unwrap();
    let reader = Store::open_graph_read_only(&path, options(), None).unwrap();
    assert_eq!(
        reader
            .set_graph_maintenance_policy(policy)
            .unwrap_err()
            .kind(),
        GraphStoreErrorKind::ReadOnly
    );
    assert_eq!(
        reader.graph_maintain_step(&control()).unwrap_err().kind(),
        GraphStoreErrorKind::ReadOnly
    );
}

#[test]
fn ze260_automatic_reclaim_failure_commits_nothing() {
    use super::GraphMaintenancePolicy;
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::create_graph(dir.path().join("graph"), options(), None).unwrap();
    create_node(&store, "seed", 1);
    store
        .set_graph_maintenance_policy(GraphMaintenancePolicy {
            automatic: true,
            reclaim_after_bytes: 1024 * 1024,
        })
        .unwrap();
    let native = store.store_for_test();
    let written = native
        .native_graph
        .pack_bytes_since_reclaim
        .load(Ordering::Relaxed);
    assert!(written > 0);
    native
        .native_graph
        .pack_bytes_since_reclaim
        .store(1024 * 1024, Ordering::Relaxed);
    let count_debt = native
        .native_graph
        .commits_since_reclaim
        .load(Ordering::Relaxed);
    let generation = native
        .admit_native_read()
        .unwrap()
        .bundle()
        .base()
        .generation;
    crate::lifecycle::native_graph::automatic::PARTIAL_FOLD.with(|limit| limit.set(true));
    crate::property_graph::storage::inventory::force_next_incomplete_inventory_retirement();
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let error = store
        .graph_apply(
            &[StructuredWrite {
                key: node_key("changed"),
                revision: revision(1),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &control(),
        )
        .unwrap_err();
    crate::lifecycle::native_graph::automatic::PARTIAL_FOLD.with(|limit| limit.set(false));
    assert!(error.nothing_committed());
    assert_eq!(
        native
            .native_graph
            .commits_since_reclaim
            .load(Ordering::Relaxed),
        count_debt
    );
    assert_eq!(
        native
            .admit_native_read()
            .unwrap()
            .bundle()
            .base()
            .generation,
        generation
    );
    store
        .set_graph_maintenance_policy(GraphMaintenancePolicy {
            automatic: false,
            reclaim_after_bytes: 1024 * 1024,
        })
        .unwrap();
    store.graph_apply(&[], &control()).unwrap();
    assert!(
        store
            .graph_maintain_cycle(&control())
            .unwrap()
            .cycle_complete
    );
}

#[test]
#[ignore = "slow: ~200 s in release; run explicitly"]
fn ze260_automatic_reclaim_keeps_an_append_only_store_bounded() {
    use super::GraphMaintenancePolicy;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph");
    let store = Store::create_graph(&path, options(), None).unwrap();
    store
        .set_graph_maintenance_policy(GraphMaintenancePolicy {
            automatic: true,
            reclaim_after_bytes: 4 * 1024 * 1024,
        })
        .unwrap();
    let image = CanonicalContents::node(&mut [], &mut [], Some("text"), None).unwrap();
    let mut ids = Vec::new();
    for batch in 0..20 {
        let keys: Vec<_> = (0..100).map(|i| format!("n{}", batch * 100 + i)).collect();
        let edge_key = format!("edge{batch}");
        let result = with_local_refs(|refs| {
            let mut writes: Vec<_> = keys
                .iter()
                .map(|key| StructuredWrite {
                    key: node_key(key),
                    revision: revision(1),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                })
                .collect();
            writes.push(StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "app", &edge_key).unwrap(),
                revision: revision(1),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("LINK").unwrap(),
                    properties: &[],
                }),
            });
            store.graph_apply(&writes, &control())
        })
        .unwrap();
        ids.extend(result.receipts().iter().take(100).map(|r| r.entity));
    }
    for batch in 0..10 {
        let keys: Vec<_> = (0..10).map(|i| format!("n{}", batch * 10 + i)).collect();
        let writes: Vec<_> = keys
            .iter()
            .enumerate()
            .map(|(i, key)| StructuredWrite {
                key: node_key(key),
                revision: revision(2),
                operation: StructuredOperation::Put(ids[batch * 10 + i]),
                image: Some(WriteImage::Node(&image)),
            })
            .collect();
        store.graph_apply(&writes, &control()).unwrap();
    }
    let files = snapshot(&path);
    assert!(files.values().map(Vec::len).sum::<usize>() <= 8_000_000);
    assert!(files.len() <= 120);
    store.close_graph().unwrap();
    let reopened = Store::open_graph(&path, options(), None).unwrap();
    let nodes: Vec<_> = ids
        .into_iter()
        .map(|id| match id {
            EntityId::Node(id) => id,
            _ => panic!("node"),
        })
        .collect();
    assert_eq!(
        reopened
            .get_nodes(&nodes, super::GraphGetOptions::default(), &control())
            .unwrap()
            .nodes()
            .iter()
            .filter(|node| node.is_some())
            .count(),
        2000
    );
}

#[test]
fn ze201_maintain_after_close_is_closed() {
    let parent = tempfile::tempdir().expect("parent");
    let store = Store::create_graph(parent.path().join("native"), options(), None).expect("store");
    store.close_graph().expect("close");
    let error = store.graph_maintain_step(&control()).expect_err("closed");
    assert_eq!(error.kind(), GraphStoreErrorKind::Closed);
    assert!(error.nothing_committed());
}

#[test]
fn ze201_maintain_cycle_after_close_is_closed() {
    let parent = tempfile::tempdir().expect("parent");
    let store = Store::create_graph(parent.path().join("native"), options(), None).expect("store");
    store.close_graph().expect("close");
    let error = store.graph_maintain_cycle(&control()).expect_err("closed");
    assert_eq!(error.kind(), GraphStoreErrorKind::Closed);
    assert!(error.nothing_committed());
}

#[test]
fn ze200_facade_preserves_internal_and_limit_kinds() {
    use crate::lifecycle::native_graph::NativeGraphError;
    use crate::property_graph::query::completed::GraphQueryError;
    for cause in [
        NativeGraphError::PreparedBaseChanged,
        NativeGraphError::WriterAbsent,
        NativeGraphError::Stage(StageError::ViewMismatch),
    ] {
        let error = super::GraphStoreError::from(cause);
        assert_eq!(error.kind(), GraphStoreErrorKind::Internal);
        assert!(error.nothing_committed());
    }
    let error = super::GraphStoreError::from(GraphQueryError::from(NativeGraphError::Stage(
        StageError::ViewMismatch,
    )));
    assert_eq!(error.kind(), GraphStoreErrorKind::Internal);
    assert!(error.nothing_committed());
    let error = super::GraphStoreError::from(NativeGraphError::WalTailBoundExceeded);
    assert_eq!(error.kind(), GraphStoreErrorKind::Limit);
    assert!(error.nothing_committed());
}
#[test]
#[ignore = "slow: ~19 min in debug, ~50 s in release; the Node graph-scale test gates it in CI; run with --release --ignored"]
fn ze329_node_scale_batches_use_default_budgets() {
    use crate::property_graph::storage::preparation_work_capture as capture;
    use crate::property_graph::{PropertyData, PropertyValue};
    use std::sync::atomic::Ordering;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph");
    let store = Store::create_graph(&path, options(), None).unwrap();
    for batch in 0..200 {
        let start = batch * 100;
        let keys: Vec<_> = (start..start + 100).map(|i| i.to_string()).collect();
        let edge_key = start.to_string();
        let mut labels = vec![[GraphName::new("Segment").unwrap()]; 100];
        let mut properties: Vec<_> = (start..start + 100)
            .map(|i| {
                [GraphProperty::new(
                    GraphName::new("startMs").unwrap(),
                    PropertyValue::new(PropertyData::I64(i)).unwrap(),
                )]
            })
            .collect();
        let images: Vec<_> = labels
            .iter_mut()
            .zip(properties.iter_mut())
            .map(|(labels, properties)| {
                CanonicalContents::node(labels, properties, None, None).unwrap()
            })
            .collect();
        capture::start();
        let result = with_local_refs(|refs| {
            let mut writes: Vec<_> = keys
                .iter()
                .zip(&images)
                .map(|(key, image)| StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "docs", key).unwrap(),
                    revision: revision(1),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(image)),
                })
                .collect();
            writes.push(StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "edges", &edge_key).unwrap(),
                revision: revision(1),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("NEXT").unwrap(),
                    properties: &[],
                }),
            });
            store.graph_apply(&writes, &control())
        });
        let report = capture::take();
        if result.is_err() || batch == 32 || batch == 199 {
            let native = store.store_for_test();
            let lease = native.admit_native_read().unwrap();
            eprintln!(
                "ZE329 batch={batch} start={start} error={:?} phases={:?} rejected={:?} census_rows={} manifest_backlog={} reclaim={:?} count_debt={} byte_debt={} origins={:?}",
                result.as_ref().err().map(ToString::to_string),
                report.phases,
                report.rejected,
                report.census_rows,
                lease.bundle().prepared_inventories().len(),
                lease.bundle().reclaim(),
                native
                    .native_graph
                    .commits_since_reclaim
                    .load(Ordering::Relaxed),
                native
                    .native_graph
                    .pack_bytes_since_reclaim
                    .load(Ordering::Relaxed),
                report.origins
            );
        }
        let result = result.unwrap();
        assert!(matches!(
            result.outcome(),
            GraphWriteOutcome::Committed { .. }
        ));
        assert_eq!(result.receipts().len(), 101);
    }
    store.close_graph().unwrap();
    drop(store);
    let reopened = Store::open_graph(&path, options(), None).unwrap();
    ze329_check_scale_queries(&reopened);
    reopened.close_graph().unwrap();
}

fn ze329_check_scale_queries(store: &Store) {
    use super::{GraphPlanBacking, GraphQueryPlan};
    use crate::property_graph::query::completed::{GraphQueryOptions, Value};
    use crate::property_graph::query::plan::{
        AggregateExpression, Direction, ExprId, Expression, Operator, OperatorKind, PatternId,
        PlanNodeId, Projection, SlotId, SortKey,
    };
    let label = String::from("Segment");
    let property = String::from("startMs");
    let relationship_type = String::from("NEXT");
    // Each query has one complete reachable plan: count nodes, ordered last
    // ten values, then count outgoing NEXT relationships, matching Node.
    for query in 0..3 {
        let unit = vec![PlanNodeId(0)];
        let scan = vec![PlanNodeId(1)];
        let project = vec![PlanNodeId(2)];
        let sort = vec![PlanNodeId(3)];
        let keys = Vec::new();
        let projection = vec![Projection {
            slot: SlotId(10),
            expression: ExprId(if query == 1 { 1 } else { 0 }),
        }];
        let sort_keys = vec![SortKey {
            expression: ExprId(2),
            descending: true,
        }];
        let types = vec![GraphName::new(&relationship_type).unwrap()];
        let expressions = if query == 1 {
            vec![
                Expression::Slot(SlotId(0)),
                Expression::Property {
                    entity: ExprId(0),
                    name: GraphName::new(&property).unwrap(),
                },
                Expression::Slot(SlotId(10)),
            ]
        } else {
            vec![Expression::Aggregate {
                operation: AggregateExpression::Count { distinct: false },
                operand: None,
            }]
        };
        let mut operators = vec![
            Operator {
                inputs: &[],
                kind: OperatorKind::Unit,
            },
            Operator {
                inputs: &unit,
                kind: OperatorKind::ScanNodes {
                    output: SlotId(0),
                    label: Some(GraphName::new(&label).unwrap()),
                },
            },
        ];
        match query {
            0 => operators.push(Operator {
                inputs: &scan,
                kind: OperatorKind::Aggregate {
                    keys: &keys,
                    aggregates: &projection,
                },
            }),
            1 => {
                operators.push(Operator {
                    inputs: &scan,
                    kind: OperatorKind::Project(&projection),
                });
                operators.push(Operator {
                    inputs: &project,
                    kind: OperatorKind::Sort(&sort_keys),
                });
                operators.push(Operator {
                    inputs: &sort,
                    kind: OperatorKind::OffsetLimit {
                        offset: 0,
                        limit: Some(10),
                    },
                });
            }
            _ => {
                operators.push(Operator {
                    inputs: &scan,
                    kind: OperatorKind::Expand {
                        source: SlotId(0),
                        node: SlotId(1),
                        relationship: SlotId(2),
                        direction: Direction::Outgoing,
                        relationship_types: &types,
                        pattern: PatternId(0),
                    },
                });
                operators.push(Operator {
                    inputs: &project,
                    kind: OperatorKind::Aggregate {
                        keys: &keys,
                        aggregates: &projection,
                    },
                });
            }
        }
        let mut backing = GraphPlanBacking::default();
        backing.string(&label).unwrap();
        backing.string(&property).unwrap();
        backing.string(&relationship_type).unwrap();
        backing.vec(&unit).unwrap();
        backing.vec(&scan).unwrap();
        backing.vec(&project).unwrap();
        backing.vec(&sort).unwrap();
        backing.vec(&keys).unwrap();
        backing.vec(&projection).unwrap();
        backing.vec(&sort_keys).unwrap();
        backing.vec(&types).unwrap();
        let parameters = Vec::new();
        let eager_searches = Vec::new();
        let plan = GraphQueryPlan {
            operators: &operators,
            expressions: &expressions,
            parameters: &parameters,
            eager_searches: &eager_searches,
            root: PlanNodeId((operators.len() - 1) as u32),
            backing: &backing,
            bindings: &[],
            columns: &["t"],
        };
        let result = store
            .graph_query(&control(), &GraphQueryOptions::default(), &plan)
            .unwrap();
        if query == 1 {
            assert_eq!(result.metadata().rows, 10);
            for index in 0..10 {
                assert_eq!(
                    result.cell(index, 0),
                    Some(&Value::I64(19_999 - index as i64))
                );
            }
        } else {
            assert_eq!(result.metadata().rows, 1);
            assert_eq!(
                result.cell(0, 0),
                Some(&Value::I64(if query == 0 { 20_000 } else { 200 }))
            );
        }
    }
}

#[test]
fn a_unified_empty_graph_store_reopens_through_the_compatibility_handle() {
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("unified");
    let store = Store::create_graph(&path, options(), None).unwrap();
    store.close_graph().unwrap();
    for read_only in [true, false] {
        let reopened = if read_only {
            Store::open_graph_read_only(&path, options(), None)
        } else {
            Store::open_graph(&path, options(), None)
        }
        .unwrap();
        assert_eq!(reopened.admit_native_read().unwrap().bundle().sequence(), 0);
        reopened.close_graph().unwrap();
    }
}

mod store_graph {
    use super::*;

    #[test]
    fn graph_apply_returns_an_ingest_ack_and_receipts() {
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("unified");
        let store = Store::open(&path, options()).unwrap();
        store.enable_graph().unwrap();
        let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        let writes = [StructuredWrite {
            key: node_key("ack"),
            revision: revision(1),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(&image)),
        }];
        let documents = crate::ingest::IngestBatch::new(vec![
            crate::ingest::IngestDocument::new(
                crate::ingest::DocumentVersion::new(
                    crate::ingest::DocId::new(91),
                    crate::ingest::Revision::new(1),
                ),
                vec![1.0, 0.0],
            )
            .with_text("mixed document"),
        ]);
        let batch = || super::super::GraphBatch {
            documents: Some(&documents),
            writes: &writes,
        };
        let result = store.graph_apply(batch(), &control()).unwrap();
        assert_eq!(store.count_documents(None, None).unwrap().count, 1);
        let ack: crate::ingest::IngestAck = result.ack();
        assert_eq!(result.receipts().len(), 1);
        assert_eq!(result.receipts()[0].generation.get(), ack.generation());
        assert_eq!(
            result.outcome(),
            GraphWriteOutcome::Committed {
                generation: generation(ack.generation()),
            }
        );
        assert!(ack.seq().get() > 0);
        let replay = store.graph_apply(batch(), &control()).unwrap();
        assert_eq!(replay.outcome(), GraphWriteOutcome::Replayed);
        assert_eq!(replay.ack(), ack);
        assert!(replay.receipts()[0].replayed);
        drop(store);
        let reopened = Store::open(&path, options()).unwrap();
        assert_eq!(reopened.count_documents(None, None).unwrap().count, 1);
        assert!(
            reopened
                .get_nodes(
                    &[node_id(&result, 0)],
                    super::super::GraphGetOptions::default(),
                    &control()
                )
                .unwrap()
                .nodes()[0]
                .is_some()
        );
    }
}
