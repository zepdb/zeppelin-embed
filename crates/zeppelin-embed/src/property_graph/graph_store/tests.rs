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

#[test]
fn ze257_small_batch_does_not_reserve_maximum_wal_envelope() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let store =
        GraphStore::create(parent.path().join("graph"), options(), None).expect("graph store");
    let _schedule = crate::property_graph::storage::search::native_vector_index_test_schedule(
        Some(8 * 1024 * 1024),
        None,
        |_| {},
    );
    create_node(&store, "small", 1);
    store.close().expect("close graph store");
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
        let store = GraphStore::create(
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
            .apply_batch(&requests, &control())
            .expect("400-node single batch under production preparation allowance");
        assert_eq!(result.admitted_generation(), generation(0));
        assert_eq!(
            result.outcome(),
            GraphWriteOutcome::Committed {
                generation: generation(1),
            }
        );
        assert_eq!(result.receipts().len(), 400);
        assert!(
            result
                .receipts()
                .iter()
                .all(|receipt| receipt.generation == generation(1) && !receipt.replayed)
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
        store.close().expect("close graph store");
    }
}

#[test]
fn ze257_batch_work_is_admitted_per_mutation() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let store =
        GraphStore::create(parent.path().join("graph"), options(), None).expect("graph store");
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
        .apply_batch(&requests, &control())
        .expect("100-node batch under defaults");
    store.close().expect("close graph store");
}

#[test]
#[ignore = "opens the explicitly supplied Node scale fixture"]
fn ze257_recovery_of_node_scale_fixture() {
    let path = std::env::var("ZE_GRAPH_SCALE_FIXTURE").expect("Node scale fixture path");
    let store = GraphStore::open(&path, options(), None).expect("reopen Node scale fixture");
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
    store.close().expect("close recovered scale fixture");
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
        let store = GraphStore::create(&path, options(), None).unwrap();
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
            store.apply_batch(&requests, &control()).unwrap();
        }
        store
            .store_for_test()
            .checkpoint_native_graph(&control())
            .unwrap();
        store.close().unwrap();
        let store = GraphStore::open(&path, options(), None).unwrap();
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
        store.close().unwrap();
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
    let store = GraphStore::create(&path, options(), None).unwrap();
    for key in ["one", "two", "three"] {
        create_node(&store, key, 1);
    }
    store.close().unwrap();
    store.close().unwrap();
    let store = GraphStore::open(&path, options(), None).unwrap();
    let (_, _, fence_candidates, _) = crate::lifecycle::native_graph::open_metrics_for_test();
    assert_eq!(
        fence_candidates, 3,
        "clean close leaves only the checkpoint to validate"
    );
    store.close().unwrap();
}

#[test]
fn ze257_close_checkpoint_failure_is_reported_and_releases_the_writer() {
    use crate::lifecycle::native_graph::tests::publication::{FaultPoint, RecordingVfs};
    use std::sync::Arc;
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("graph");
    let vfs = Arc::new(RecordingVfs::default());
    let store = GraphStore {
        store: Store::create_native_graph_with_infrastructure(
            &path,
            options().with_durability(DurabilityMode::Durable, CommitTier::Durable),
            None,
            vfs.clone(),
            Arc::new(crate::lifecycle::SystemMonotonicClock),
            &mut crate::property_graph::storage::allocation::OsEntropy,
        )
        .unwrap(),
    };
    create_node(&store, "durable", 1);
    vfs.arm_fault(FaultPoint::Create);
    assert!(
        store.close().is_err(),
        "checkpoint failure must not be hidden"
    );
    vfs.assert_fired_once();
    store.close().unwrap();
    let reopened = GraphStore::open(&path, options(), None).unwrap();
    assert_eq!(
        create_node(&reopened, "next", 1).admitted_generation(),
        generation(1)
    );
    reopened.close().unwrap();
}

#[test]
fn ze257_recovery_sizes_serial_inventory_from_actual_artifacts() {
    use crate::property_graph::storage::artifact::{
        self, ArtifactId, ArtifactIdentity, ContainerKind,
    };
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("graph");
    let store = GraphStore::create(&path, options(), None).unwrap();
    let lease = store.store_for_test().admit_native_read().unwrap();
    let store_id = lease.bundle().base().store;
    drop(lease);
    store.close().unwrap();
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
    let store = GraphStore::open(&path, options(), None)
        .expect("actual descriptor count fits default memory");
    let probes = crate::lifecycle::native_graph::serial_probes_for_test();
    eprintln!("ZE257 serial inventory probes={probes}");
    assert!(probes < 8192 * 8, "serial inventory probes: {probes}");
    create_node(&store, "after-orphans", 1);
    store.close().unwrap();
}

#[test]
fn ze260_maintenance_policy_and_public_step() {
    use super::GraphMaintenancePolicy;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph");
    let store = GraphStore::create(&path, options(), None).unwrap();
    let policy = GraphMaintenancePolicy::default();
    assert!(policy.automatic);
    assert_eq!(policy.reclaim_after_bytes, 64 * 1024 * 1024);
    assert_eq!(
        store
            .set_maintenance_policy(GraphMaintenancePolicy {
                automatic: false,
                reclaim_after_bytes: 0
            })
            .unwrap_err()
            .kind(),
        GraphStoreErrorKind::InvalidRequest
    );
    store
        .set_maintenance_policy(GraphMaintenancePolicy {
            automatic: false,
            reclaim_after_bytes: 1024 * 1024,
        })
        .unwrap();
    let report = store.maintain(&control()).unwrap();
    assert!(report.cycle_complete);
    assert_eq!(report.new_pack_bytes, 0);
    store.close().unwrap();
    let reader = GraphStore::open_read_only(&path, options(), None).unwrap();
    assert_eq!(
        reader.set_maintenance_policy(policy).unwrap_err().kind(),
        GraphStoreErrorKind::ReadOnly
    );
    assert_eq!(
        reader.maintain(&control()).unwrap_err().kind(),
        GraphStoreErrorKind::ReadOnly
    );
}

#[test]
fn ze260_automatic_reclaim_failure_commits_nothing() {
    use super::GraphMaintenancePolicy;
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir().unwrap();
    let store = GraphStore::create(dir.path().join("graph"), options(), None).unwrap();
    create_node(&store, "seed", 1);
    store
        .set_maintenance_policy(GraphMaintenancePolicy {
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
    let generation = native
        .admit_native_read()
        .unwrap()
        .bundle()
        .base()
        .generation;
    crate::lifecycle::native_graph::automatic::PARTIAL_FOLD.with(|limit| limit.set(true));
    crate::property_graph::storage::inventory::force_next_incomplete_inventory_retirement();
    let error = store.apply_batch(&[], &control()).unwrap_err();
    crate::lifecycle::native_graph::automatic::PARTIAL_FOLD.with(|limit| limit.set(false));
    assert!(error.nothing_committed());
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
        .set_maintenance_policy(GraphMaintenancePolicy {
            automatic: false,
            reclaim_after_bytes: 1024 * 1024,
        })
        .unwrap();
    store.apply_batch(&[], &control()).unwrap();
    assert!(store.maintain_cycle(&control()).unwrap().cycle_complete);
}

#[test]
#[ignore = "slow: ~200 s in release; run explicitly"]
fn ze260_automatic_reclaim_keeps_an_append_only_store_bounded() {
    use super::GraphMaintenancePolicy;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph");
    let store = GraphStore::create(&path, options(), None).unwrap();
    store
        .set_maintenance_policy(GraphMaintenancePolicy {
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
            store.apply_batch(&writes, &control())
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
        store.apply_batch(&writes, &control()).unwrap();
    }
    let files = snapshot(&path);
    assert!(files.values().map(Vec::len).sum::<usize>() <= 8_000_000);
    assert!(files.len() <= 120);
    store.close().unwrap();
    let reopened = GraphStore::open(&path, options(), None).unwrap();
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
