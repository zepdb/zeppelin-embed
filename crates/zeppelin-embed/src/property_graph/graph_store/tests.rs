//! ZE-66 S1: the public graph store lifecycle and `apply_batch` contract.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    reason = "tests fail loudly on the first broken contract"
)]

use super::{GraphStore, GraphStoreErrorKind, GraphWriteOutcome, GraphWriteResult};
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

fn create_node(store: &GraphStore, key: &str, at: u64) -> GraphWriteResult {
    let image = CanonicalContents::node(&mut [], &mut [], Some("text"), None).expect("node image");
    store
        .apply_batch(
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
    let store = GraphStore::create(&path, options(), None).expect("graph-only store");
    let durable = DurabilityPolicy::new(DurabilityMode::Durable, CommitTier::Durable)
        .expect("durable policy");
    assert_eq!(store.store_for_test().durability_policy, durable);

    let first = create_node(&store, "first", 1);
    assert_eq!(first.admitted_generation(), generation(0));
    assert_eq!(
        first.outcome(),
        GraphWriteOutcome::Committed {
            generation: generation(1)
        }
    );
    assert_eq!(first.receipts().len(), 1);
    assert!(!first.receipts()[0].replayed);
    assert_eq!(first.receipts()[0].generation, generation(1));
    store.close().expect("close graph store");
    drop(store);

    // Only graph files exist: no legacy manifest or legacy WAL was created.
    for file in snapshot(&path).keys() {
        let name = file.file_name().and_then(|name| name.to_str()).unwrap();
        assert!(
            name.starts_with("graph-") || name == "writer.lock",
            "unexpected file in a graph-only store: {name}"
        );
    }

    let reopened = GraphStore::open(&path, options(), None).expect("reopen graph store");
    assert_eq!(reopened.store_for_test().durability_policy, durable);
    let second = create_node(&reopened, "second", 1);
    assert_eq!(second.admitted_generation(), generation(1));
    assert_eq!(
        second.outcome(),
        GraphWriteOutcome::Committed {
            generation: generation(2)
        }
    );
    reopened.close().expect("close reopened store");
    drop(reopened);

    let reader = GraphStore::open_read_only(&path, options(), None).expect("read-only open");
    let before = snapshot(&path);
    let image = CanonicalContents::node(&mut [], &mut [], None, None).expect("node image");
    let refused = reader
        .apply_batch(
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
    reader.close().expect("close reader");
}

#[test]
fn legacy_open_refuses_a_graph_store_directory_with_a_typed_error() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("graph");
    let store = GraphStore::create(&path, options(), None).expect("graph store");
    create_node(&store, "only", 1);
    store.close().expect("close graph store");
    drop(store);

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

#[test]
fn graph_open_refuses_a_legacy_store_directory_with_a_typed_error() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("legacy");
    let legacy = Store::open(&path, OpenOptions::new()).expect("legacy store");
    legacy.close().expect("close legacy store");
    drop(legacy);

    let before = snapshot(&path);
    let refusals = [
        GraphStore::open(&path, options(), None).expect_err("graph open must refuse"),
        GraphStore::open_read_only(&path, options(), None)
            .expect_err("read-only graph open must refuse"),
        GraphStore::create(&path, options(), None).expect_err("graph create must refuse"),
    ];
    for error in refusals {
        assert_eq!(error.kind(), GraphStoreErrorKind::LegacyStore, "{error}");
        assert_eq!(error.legacy_store_path(), Some(path.as_path()));
        assert!(error.nothing_committed());
        assert_eq!(snapshot(&path), before);
    }

    // The legacy store is still intact and opens as itself.
    let legacy = Store::open(&path, OpenOptions::new()).expect("legacy store reopens");
    legacy.close().expect("close legacy store");
}

#[test]
fn graph_store_ids_above_u64_round_trip_through_apply_batch() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("wide");
    let first_node = NodeId::new((1_u128 << 64) + 5).expect("wide node seed");
    let first_relationship = RelId::new((1_u128 << 100) + 7).expect("wide relationship seed");
    let store = GraphStore::create_with_allocator_seed_for_test(
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
        store.apply_batch(&batch, &control())
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
    store.close().expect("close seeded store");
    drop(store);

    // After reopen the store resolves the full 128-bit identities it returned:
    // a Put names each incarnation by ID, and a truncated ID would conflict.
    let reopened = GraphStore::open(&path, options(), None).expect("reopen seeded store");
    let replacement =
        CanonicalContents::node(&mut [], &mut [], Some("replaced"), None).expect("replacement");
    let replaced = reopened
        .apply_batch(
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
    reopened.close().expect("close reopened store");
}

#[test]
fn graph_store_exact_retry_replays_with_its_original_generation() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let store =
        GraphStore::create(parent.path().join("retry"), options(), None).expect("graph store");
    let first = create_node(&store, "first", 1);
    let installed = first.receipts()[0];

    // No automatic retry happened: the caller resubmits and gets a replay at
    // the original generation, and nothing new is written.
    let replayed = create_node(&store, "first", 1);
    assert_eq!(replayed.outcome(), GraphWriteOutcome::Replayed);
    assert_eq!(replayed.admitted_generation(), generation(1));
    assert!(replayed.receipts()[0].replayed);
    assert_eq!(replayed.receipts()[0].entity, installed.entity);
    assert_eq!(replayed.receipts()[0].generation, generation(1));

    // A mixed batch commits once; each receipt carries its own record.
    let image = CanonicalContents::node(&mut [], &mut [], Some("text"), None).expect("image");
    let mixed = store
        .apply_batch(
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
            generation: generation(2)
        }
    );
    assert_eq!(mixed.admitted_generation(), generation(1));
    assert!(mixed.receipts()[0].replayed);
    assert_eq!(mixed.receipts()[0].generation, generation(1));
    assert!(!mixed.receipts()[1].replayed);
    assert_eq!(mixed.receipts()[1].generation, generation(2));

    // An empty batch changes nothing.
    let empty = store.apply_batch(&[], &control()).expect("empty batch");
    assert_eq!(empty.outcome(), GraphWriteOutcome::NoOp);
    assert_eq!(empty.admitted_generation(), generation(2));
    assert!(empty.receipts().is_empty());
    store.close().expect("close graph store");
}

#[test]
fn graph_store_refuses_a_stale_incarnation_with_nothing_committed() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let store =
        GraphStore::create(parent.path().join("stale"), options(), None).expect("graph store");
    let image = CanonicalContents::node(&mut [], &mut [], Some("text"), None).expect("image");
    let first = node_id(&create_node(&store, "fenced", 1), 0);
    store
        .apply_batch(
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
        .apply_batch(
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
            generation: generation(3)
        }
    );
    assert_ne!(node_id(&recreated, 0), first);

    let stale = store
        .apply_batch(
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
    assert_eq!(next.admitted_generation(), generation(3));
    assert_eq!(
        next.outcome(),
        GraphWriteOutcome::Committed {
            generation: generation(4)
        }
    );
    store.close().expect("close graph store");
}

#[test]
fn graph_write_results_stay_readable_after_close() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let store =
        GraphStore::create(parent.path().join("closed"), options(), None).expect("graph store");
    let result = create_node(&store, "kept", 1);
    let expected = result.clone();
    store.close().expect("close graph store");
    // Closing twice is harmless.
    store.close().expect("close again");

    // A closed store admits nothing. The writer admission still groups its
    // drained writer slot as `Corruption`, not `Closed`; ZE-201 owns that
    // fix for every writer path, so only the refusal is pinned here.
    let late = store
        .apply_batch(&[], &control())
        .expect_err("a closed store admits nothing");
    assert!(late.nothing_committed(), "{late}");
    drop(store);

    assert_eq!(result, expected);
    assert_eq!(
        result.outcome(),
        GraphWriteOutcome::Committed {
            generation: generation(1)
        }
    );
    let receipts = result.into_receipts();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].revision, revision(1));
    assert_eq!(receipts[0].generation, generation(1));
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
            GraphStore::create_with_relationship_types(&path, options(), None, &rules).unwrap();
        let a = node_id(&create_node(&store, "a", 1), 0);
        let b = node_id(&create_node(&store, "b", 1), 0);
        let c = node_id(&create_node(&store, "c", 1), 0);
        for (key, source, target) in [("ab", a, b), ("bc", b, c)] {
            store
                .apply_batch(
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
        store.close().unwrap();
        let store = GraphStore::open(&path, options(), None).unwrap();
        let result = store.apply_batch(
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
        store.close().unwrap();
        let store = GraphStore::open(&path, options(), None).unwrap();
        let nodes = store
            .get_nodes(&[a, b, c], super::GraphGetOptions::default(), &control())
            .unwrap();
        assert!(
            nodes
                .nodes()
                .iter()
                .all(|n| n.is_some() == (policy == OnDelete::Restrict))
        );
        store.close().unwrap();
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
    let store = GraphStore::create_with_relationship_types(&path, options(), None, &rules).unwrap();
    let child = node_id(&create_node(&store, "child", u64::MAX), 0);
    let target = node_id(&create_node(&store, "parent", 1), 0);
    store
        .apply_batch(
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
        .apply_batch(
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
    store.close().unwrap();
    let store = GraphStore::open(&path, options(), None).unwrap();
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
    store.close().unwrap();
}
