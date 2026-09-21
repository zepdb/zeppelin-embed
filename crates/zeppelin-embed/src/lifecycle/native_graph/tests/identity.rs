//! ZE-36 directed public identity, incarnation, fence and retry observations.
//!
//! Every body asserts one boundary of the stable-identity contract through the
//! real writer, storage and reopen paths. The trailing script observation holds
//! primitive facts only: the independent comparator lives in the oracle crate.

use super::consolidation::commit_maintenance;
use super::publication::{
    FaultPoint, RecordingVfs, record_verified_fault, reset_verified_faults, rows_for_lease,
    take_verified_faults,
};
use super::recovery::{native_options, observe_node, relationship_is_visible};
use super::tempfile;
use crate::graph_identity_test_support::{
    IdentityProbeReport, IdentityState, ObservedAdjacency, ObservedOutcome, ObservedWatch,
};
use crate::lifecycle::{CancelToken, QueryControl, Store};
use crate::property_graph::staging::{
    ItemReceipt, StageError, StructuredOperation, StructuredWrite, WriteImage,
};
use crate::property_graph::storage::DirectionSelection;
use crate::property_graph::{
    ApplicationKey, CanonicalContents, EntityId, EntityKind, GraphDeleteMode, GraphName,
    GraphProperty, GraphRevision, KeyLifecycleError, NodeId, NodeRef, PropertyData, PropertyValue,
    RelId,
};
use crate::vfs::Vfs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::super::NativeGraphError;

/// Key ordinals shared with the independent ZE-36 sequence description.
const KEY_FIRST: u32 = 1;
const KEY_PEER: u32 = 2;
const KEY_EDGE: u32 = 3;
const KEY_SECOND_EDGE: u32 = 4;

fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}

fn key(kind: EntityKind, name: &'static str) -> ApplicationKey<'static> {
    ApplicationKey::new(kind, "ze36", name).expect("application key")
}

fn revision(value: u64) -> GraphRevision {
    GraphRevision::new(value).expect("positive revision")
}

fn value(bits: u64) -> [GraphProperty<'static>; 1] {
    [GraphProperty::new(
        GraphName::new("value").expect("property name"),
        PropertyValue::new(PropertyData::F64(f64::from_bits(bits))).expect("all original bits"),
    )]
}

fn node(store: &Store, receipt: &ItemReceipt) -> NodeId {
    match receipt.entity {
        EntityId::Node(node) => {
            assert!(observe_node(store, node).is_some() || receipt.replayed);
            node
        }
        EntityId::Relationship(_) => panic!("node receipt domain"),
    }
}

fn relationship(receipt: &ItemReceipt) -> RelId {
    match receipt.entity {
        EntityId::Relationship(relationship) => relationship,
        EntityId::Node(_) => panic!("relationship receipt domain"),
    }
}

/// Primitive rejection name, matching the independent oracle's vocabulary.
fn rejection(error: &NativeGraphError) -> &'static str {
    let NativeGraphError::Stage(StageError::Lifecycle(lifecycle)) = error else {
        panic!("ZE-36 expected a key lifecycle rejection, observed {error}");
    };
    match lifecycle {
        KeyLifecycleError::MissingKey => "Missing",
        KeyLifecycleError::Stale { .. } => "Stale",
        KeyLifecycleError::RevisionConflict => "Conflict",
        KeyLifecycleError::AlreadyExists => "Exists",
        KeyLifecycleError::DeletedKey => "Deleted",
        KeyLifecycleError::NotDeleted => "NotDeleted",
        KeyLifecycleError::IncarnationConflict => "Incarnation",
        KeyLifecycleError::DeletionRevisionConflict => "DeletionRevision",
        other => panic!("ZE-36 unexpected key lifecycle rejection {other}"),
    }
}

/// Applies `requests` and requires the named rejection, counting the refusal.
fn refuse(store: &Store, requests: &[StructuredWrite<'_, '_>], expected: &'static str) {
    let error = store
        .apply_native_graph(requests, &control())
        .err()
        .unwrap_or_else(|| panic!("ZE-36 required a {expected} rejection"));
    assert_eq!(rejection(&error), expected);
    record_verified_fault();
}

/// Requires a refusal without naming its type, for a case whose classification
/// is owned by ZE-174. The caller proves the unchanged state separately.
fn refuse_untyped(store: &Store, requests: &[StructuredWrite<'_, '_>]) {
    let error = store
        .apply_native_graph(requests, &control())
        .err()
        .expect("ZE-36 required a refusal");
    assert!(!matches!(
        error,
        NativeGraphError::CommitIndeterminate { .. }
    ));
    record_verified_fault();
}

fn store_at(path: &Path) -> Store {
    Store::create_native_graph(path, native_options(), None).expect("fresh identity store")
}

fn adjacency(
    rows: &[crate::property_graph::storage::adjacency::RelationshipRow],
    bound: NodeId,
    outgoing: bool,
) -> Vec<ObservedAdjacency> {
    rows.iter()
        .map(|row| ObservedAdjacency {
            bound_node: bound.get(),
            relationship_type: row.relationship_type.get(),
            rel: row.rel.get(),
            neighbor: if outgoing {
                row.target.get()
            } else {
                row.source.get()
            },
        })
        .collect()
}

fn outcome(key: u32, receipt: &ItemReceipt) -> ObservedOutcome {
    ObservedOutcome {
        key,
        rejection: None,
        entity: match receipt.entity {
            EntityId::Node(node) => node.get(),
            EntityId::Relationship(relationship) => relationship.get(),
        },
        revision: receipt.revision.get(),
        generation: receipt.generation.get(),
        replayed: receipt.replayed,
    }
}

// ---------------------------------------------------------------------------
// property-graph.identity.replay
// ---------------------------------------------------------------------------

/// The exact retry of an installing request returns its original identity,
/// revision and generation across a reopen and a real reclamation cycle.
fn run_ze36_identity_replay_survives_reopen_and_reclamation() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("replay");
    let store = store_at(&path);
    let mut properties = value(0xA1);
    let mut peer_properties = value(0xB1);
    let installed = crate::property_graph::with_local_refs(|refs| {
        let first = CanonicalContents::node(&mut [], &mut properties, None, None).expect("first");
        let peer =
            CanonicalContents::node(&mut [], &mut peer_properties, None, None).expect("peer");
        store
            .apply_native_graph(&script_batch(&first, &peer, refs), &control())
            .expect("install identity script")
    });
    let first = node(&store, &installed[0]);
    let peer = node(&store, &installed[1]);
    let edge = relationship(&installed[2]);
    let original: Vec<_> = installed.iter().copied().collect();
    assert!(original.iter().all(|receipt| !receipt.replayed));
    assert!(original.iter().all(|receipt| receipt.generation.get() == 1));
    drop(installed);

    let replay = |store: &Store| {
        let mut properties = value(0xA1);
        let mut peer_properties = value(0xB1);
        crate::property_graph::with_local_refs(|refs| {
            let first =
                CanonicalContents::node(&mut [], &mut properties, None, None).expect("first");
            let peer =
                CanonicalContents::node(&mut [], &mut peer_properties, None, None).expect("peer");
            let receipts = store
                .apply_native_graph(&script_batch(&first, &peer, refs), &control())
                .expect("exact replay");
            // An exact retry differs from its installing commit in exactly one
            // field: it is classified as a replay.
            assert!(receipts.iter().all(|receipt| receipt.replayed));
            receipts
                .iter()
                .map(|receipt| ItemReceipt {
                    replayed: false,
                    ..*receipt
                })
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(replay(&store), original);

    store.close().expect("close installed store");
    drop(store);
    let reopened = Store::open_native_graph(&path, native_options(), None).expect("reopen");
    assert_eq!(replay(&reopened), original);
    assert_eq!(observe_node(&reopened, first).map(|node| node.2), Some(1));
    assert!(relationship_is_visible(&reopened, edge));

    commit_maintenance(&reopened).expect("first reclamation cycle");
    commit_maintenance(&reopened).expect("second reclamation cycle");
    reopened
        .checkpoint_native_graph(&control())
        .expect("checkpoint after reclamation");
    assert_eq!(replay(&reopened), original);
    reopened.close().expect("close reclaimed store");
    drop(reopened);

    let again = Store::open_native_graph(&path, native_options(), None).expect("reopen reclaimed");
    assert_eq!(replay(&again), original);
    let lease = again.admit_native_read().expect("reader");
    assert_eq!(
        rows_for_lease(&again, &lease, first, peer, DirectionSelection::Out)
            .iter()
            .map(|row| row.rel)
            .collect::<Vec<_>>(),
        vec![edge]
    );
    drop(lease);
    again.close().expect("close replayed store");
}

/// The one installing batch of the identity script.
fn script_batch<'a, 'batch>(
    first: &'a CanonicalContents<'a>,
    peer: &'a CanonicalContents<'a>,
    refs: crate::property_graph::LocalRefs<'batch>,
) -> [StructuredWrite<'a, 'batch>; 3] {
    [
        StructuredWrite {
            key: key(EntityKind::Node, "first"),
            revision: revision(1),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(first)),
        },
        StructuredWrite {
            key: key(EntityKind::Node, "peer"),
            revision: revision(1),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(peer)),
        },
        StructuredWrite {
            key: key(EntityKind::Relationship, "edge"),
            revision: revision(1),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Relationship {
                source: NodeRef::Local(refs.node(0).expect("first local slot")),
                target: NodeRef::Local(refs.node(1).expect("peer local slot")),
                relationship_type: GraphName::new("LINKS").expect("relationship type"),
                properties: &[],
            }),
        },
    ]
}

#[cfg_attr(test, test)]
fn ze36_identity_replay_survives_reopen_and_reclamation() {
    run_ze36_identity_replay_survives_reopen_and_reclamation();
}

// ---------------------------------------------------------------------------
// property-graph.identity.incarnation
// ---------------------------------------------------------------------------

/// A tombstone and a later incarnation fence every stale request: only an
/// explicit recreate naming the exact deletion revision installs a new ID.
fn run_ze36_identity_incarnation_fences_refuse_stale_requests() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("incarnation");
    let store = store_at(&path);
    let mut properties = value(0xC1);
    let image = CanonicalContents::node(&mut [], &mut properties, None, None).expect("image");
    let created = store
        .apply_native_graph(
            &[StructuredWrite {
                key: key(EntityKind::Node, "fenced"),
                revision: revision(1),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &control(),
        )
        .expect("create fenced key");
    let first = node(&store, &created[0]);
    drop(created);

    let delete = [StructuredWrite {
        key: key(EntityKind::Node, "fenced"),
        revision: revision(2),
        operation: StructuredOperation::Delete(EntityId::Node(first), GraphDeleteMode::Restrict),
        image: None,
    }];
    let deleted = store
        .apply_native_graph(&delete, &control())
        .expect("delete fenced key");
    assert_eq!(deleted[0].entity, EntityId::Node(first));
    drop(deleted);
    assert_eq!(observe_node(&store, first), None);

    // The deletion fence answers its own exact retry, and nothing else.
    let replayed = store
        .apply_native_graph(&delete, &control())
        .expect("deletion replay");
    assert!(replayed[0].replayed);
    assert_eq!(replayed[0].entity, EntityId::Node(first));
    assert_eq!(replayed[0].revision.get(), 2);
    let fence_generation = replayed[0].generation.get();
    drop(replayed);

    refuse(
        &store,
        &[StructuredWrite {
            key: key(EntityKind::Node, "fenced"),
            revision: revision(3),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(&image)),
        }],
        "Deleted",
    );
    refuse(
        &store,
        &[StructuredWrite {
            key: key(EntityKind::Node, "fenced"),
            revision: revision(3),
            operation: StructuredOperation::Put(EntityId::Node(first)),
            image: Some(WriteImage::Node(&image)),
        }],
        "Deleted",
    );
    refuse(
        &store,
        &[StructuredWrite {
            key: key(EntityKind::Node, "fenced"),
            revision: revision(3),
            operation: StructuredOperation::Recreate(revision(1)),
            image: Some(WriteImage::Node(&image)),
        }],
        "DeletionRevision",
    );

    let recreated = store
        .apply_native_graph(
            &[StructuredWrite {
                key: key(EntityKind::Node, "fenced"),
                revision: revision(3),
                operation: StructuredOperation::Recreate(revision(2)),
                image: Some(WriteImage::Node(&image)),
            }],
            &control(),
        )
        .expect("explicit recreation");
    let second = node(&store, &recreated[0]);
    drop(recreated);
    assert!(second.get() > first.get());
    assert_eq!(observe_node(&store, first), None);
    assert_eq!(observe_node(&store, second).map(|node| node.2), Some(3));

    // The retired incarnation can neither be written nor deleted again, and
    // the old delete no longer reaches the fence it once installed. ZE-174
    // owns the rejection type here: the retired incarnation's tombstone is
    // decoded as a live record, so the refusal carries an internal storage
    // error instead of IncarnationConflict. The refusal itself, and the
    // untouched replacement, are the identity invariant and are proven now.
    refuse_untyped(
        &store,
        &[StructuredWrite {
            key: key(EntityKind::Node, "fenced"),
            revision: revision(4),
            operation: StructuredOperation::Put(EntityId::Node(first)),
            image: Some(WriteImage::Node(&image)),
        }],
    );
    refuse_untyped(
        &store,
        &[StructuredWrite {
            key: key(EntityKind::Node, "fenced"),
            revision: revision(4),
            operation: StructuredOperation::Delete(
                EntityId::Node(first),
                GraphDeleteMode::Restrict,
            ),
            image: None,
        }],
    );
    refuse_untyped(&store, &delete);
    assert_eq!(observe_node(&store, first), None);
    assert_eq!(observe_node(&store, second).map(|node| node.2), Some(3));
    assert!(fence_generation >= 1);

    // The replacement still accepts its own next revision, so the refusals
    // above changed nothing.
    let advanced = store
        .apply_native_graph(
            &[StructuredWrite {
                key: key(EntityKind::Node, "fenced"),
                revision: revision(4),
                operation: StructuredOperation::Put(EntityId::Node(second)),
                image: Some(WriteImage::Node(&image)),
            }],
            &control(),
        )
        .expect("the replacement accepts its own revision");
    assert_eq!(advanced[0].entity, EntityId::Node(second));
    drop(advanced);
    store.close().expect("close fenced store");
}

#[cfg_attr(test, test)]
fn ze36_identity_incarnation_fences_refuse_stale_requests() {
    run_ze36_identity_incarnation_fences_refuse_stale_requests();
}

// ---------------------------------------------------------------------------
// property-graph.identity.revision
// ---------------------------------------------------------------------------

/// Revisions advance monotonically through the whole lifecycle, and fences and
/// identity high-waters survive a reopen, a checkpoint and an empty-store
/// reclamation cycle without ever reusing an identity.
fn run_ze36_identity_revisions_and_high_waters_survive_reopen() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("revisions");
    let store = store_at(&path);
    let mut first_properties = value(0xD1);
    let mut second_properties = value(0xD2);
    let image =
        CanonicalContents::node(&mut [], &mut first_properties, None, None).expect("first image");
    let altered =
        CanonicalContents::node(&mut [], &mut second_properties, None, None).expect("second image");
    let node_key = key(EntityKind::Node, "monotone");
    let created = store
        .apply_native_graph(
            &[StructuredWrite {
                key: node_key,
                revision: revision(1),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &control(),
        )
        .expect("create monotone key");
    let first = node(&store, &created[0]);
    drop(created);

    let put = [StructuredWrite {
        key: node_key,
        revision: revision(2),
        operation: StructuredOperation::Put(EntityId::Node(first)),
        image: Some(WriteImage::Node(&altered)),
    }];
    let updated = store.apply_native_graph(&put, &control()).expect("put");
    assert_eq!(updated[0].entity, EntityId::Node(first));
    assert_eq!(updated[0].revision.get(), 2);
    drop(updated);

    // A lower revision is stale, and the same revision with different bytes is
    // a conflict rather than a replay.
    refuse(
        &store,
        &[StructuredWrite {
            key: node_key,
            revision: revision(1),
            operation: StructuredOperation::Put(EntityId::Node(first)),
            image: Some(WriteImage::Node(&altered)),
        }],
        "Stale",
    );
    refuse(
        &store,
        &[StructuredWrite {
            key: node_key,
            revision: revision(2),
            operation: StructuredOperation::Put(EntityId::Node(first)),
            image: Some(WriteImage::Node(&image)),
        }],
        "Conflict",
    );
    let replayed = store.apply_native_graph(&put, &control()).expect("replay");
    assert!(replayed[0].replayed);
    drop(replayed);

    store
        .apply_native_graph(
            &[StructuredWrite {
                key: node_key,
                revision: revision(3),
                operation: StructuredOperation::Delete(
                    EntityId::Node(first),
                    GraphDeleteMode::Restrict,
                ),
                image: None,
            }],
            &control(),
        )
        .expect("delete monotone key");
    store
        .checkpoint_native_graph(&control())
        .expect("checkpoint the empty graph");
    commit_maintenance(&store).expect("empty-store reclamation cycle");
    store.close().expect("close empty store");
    drop(store);

    let reopened = Store::open_native_graph(&path, native_options(), None).expect("reopen");
    // The fence survived every boundary: only the exact deletion revision
    // recreates, and the new identity is never a reused one.
    refuse(
        &reopened,
        &[StructuredWrite {
            key: node_key,
            revision: revision(4),
            operation: StructuredOperation::Recreate(revision(2)),
            image: Some(WriteImage::Node(&image)),
        }],
        "DeletionRevision",
    );
    let recreated = reopened
        .apply_native_graph(
            &[StructuredWrite {
                key: node_key,
                revision: revision(4),
                operation: StructuredOperation::Recreate(revision(3)),
                image: Some(WriteImage::Node(&image)),
            }],
            &control(),
        )
        .expect("recreate after reclamation");
    let second = node(&reopened, &recreated[0]);
    assert_eq!(recreated[0].revision.get(), 4);
    drop(recreated);
    assert!(second.get() > first.get());
    assert_eq!(observe_node(&reopened, first), None);
    refuse(
        &reopened,
        &[StructuredWrite {
            key: node_key,
            revision: revision(5),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(&image)),
        }],
        "Exists",
    );
    reopened.close().expect("close recreated store");
}

#[cfg_attr(test, test)]
fn ze36_identity_revisions_and_high_waters_survive_reopen() {
    run_ze36_identity_revisions_and_high_waters_survive_reopen();
}

// ---------------------------------------------------------------------------
// property-graph.identity.relationship
// ---------------------------------------------------------------------------

/// Relationship endpoints and type are immutable for one incarnation, and an
/// explicit recreate installs a new relationship identity.
fn run_ze36_identity_relationship_endpoints_are_immutable() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("relationship");
    let store = store_at(&path);
    let mut properties = value(0xE1);
    let nodes = crate::property_graph::with_local_refs(|_| {
        let image = CanonicalContents::node(&mut [], &mut properties, None, None).expect("image");
        let receipts = store
            .apply_native_graph(
                &[
                    StructuredWrite {
                        key: key(EntityKind::Node, "source"),
                        revision: revision(1),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&image)),
                    },
                    StructuredWrite {
                        key: key(EntityKind::Node, "target"),
                        revision: revision(1),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&image)),
                    },
                    StructuredWrite {
                        key: key(EntityKind::Node, "other"),
                        revision: revision(1),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&image)),
                    },
                ],
                &control(),
            )
            .expect("create endpoints");
        [
            node(&store, &receipts[0]),
            node(&store, &receipts[1]),
            node(&store, &receipts[2]),
        ]
    });
    let [source, target, other] = nodes;
    let edge_key = key(EntityKind::Relationship, "edge");
    let edge = |revision_value: u64,
                operation: StructuredOperation,
                to: NodeId,
                relationship_type: &'static str,
                properties: &'static [GraphProperty<'static>]| {
        [StructuredWrite {
            key: edge_key,
            revision: revision(revision_value),
            operation,
            image: Some(WriteImage::Relationship {
                source: NodeRef::Existing(source),
                target: NodeRef::Existing(to),
                relationship_type: GraphName::new(relationship_type).expect("type"),
                properties,
            }),
        }]
    };
    let created = store
        .apply_native_graph(
            &edge(1, StructuredOperation::Create, target, "LINKS", &[]),
            &control(),
        )
        .expect("create relationship");
    let first = relationship(&created[0]);
    drop(created);

    static CHANGED: [GraphProperty<'static>; 0] = [];
    let updated = store
        .apply_native_graph(
            &edge(
                2,
                StructuredOperation::Put(EntityId::Relationship(first)),
                target,
                "LINKS",
                &CHANGED,
            ),
            &control(),
        )
        .expect("replace relationship properties");
    assert_eq!(updated[0].entity, EntityId::Relationship(first));
    drop(updated);

    for (to, relationship_type) in [(other, "LINKS"), (target, "OWNS")] {
        let error = store
            .apply_native_graph(
                &edge(
                    3,
                    StructuredOperation::Put(EntityId::Relationship(first)),
                    to,
                    relationship_type,
                    &[],
                ),
                &control(),
            )
            .err()
            .expect("immutable relationship identity");
        assert!(matches!(
            error,
            NativeGraphError::Stage(StageError::Lifecycle(
                KeyLifecycleError::RelationshipIdentityChange
            ))
        ));
        record_verified_fault();
    }

    let lease = store.admit_native_read().expect("reader");
    let rows = rows_for_lease(&store, &lease, source, target, DirectionSelection::Out);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].rel, first);
    assert_eq!(rows[0].source, source);
    assert_eq!(rows[0].target, target);
    drop(lease);

    store
        .apply_native_graph(
            &[StructuredWrite {
                key: edge_key,
                revision: revision(3),
                operation: StructuredOperation::Delete(
                    EntityId::Relationship(first),
                    GraphDeleteMode::Restrict,
                ),
                image: None,
            }],
            &control(),
        )
        .expect("delete relationship");
    assert!(!relationship_is_visible(&store, first));
    let recreated = store
        .apply_native_graph(
            &edge(
                4,
                StructuredOperation::Recreate(revision(3)),
                target,
                "LINKS",
                &[],
            ),
            &control(),
        )
        .expect("recreate relationship");
    let second = relationship(&recreated[0]);
    drop(recreated);
    assert!(second.get() > first.get());
    assert!(relationship_is_visible(&store, second));
    let lease = store.admit_native_read().expect("reader");
    let rows = rows_for_lease(&store, &lease, source, target, DirectionSelection::Out);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].rel, second);
    drop(lease);
    store.close().expect("close relationship store");
}

#[cfg_attr(test, test)]
fn ze36_identity_relationship_endpoints_are_immutable() {
    run_ze36_identity_relationship_endpoints_are_immutable();
}

// ---------------------------------------------------------------------------
// property-graph.identity.indeterminate
// ---------------------------------------------------------------------------

/// An indeterminate commit resolves on reopen to exactly one outcome: the
/// durable identity replays, and a pre-durable failure allocates nothing.
fn run_ze36_identity_indeterminate_commit_resolves_once() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let vfs = Arc::new(RecordingVfs::default());
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let mut properties = value(0xF1);
    let image = CanonicalContents::node(&mut [], &mut properties, None, None).expect("image");
    let request = [StructuredWrite {
        key: key(EntityKind::Node, "uncertain"),
        revision: revision(1),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&image)),
    }];

    // A lost acknowledgement after a durable commit replays the same identity.
    let durable_path = parent.path().join("lost-ack");
    let store = Store::create_native_graph_with_infrastructure(
        &durable_path,
        native_options(),
        None,
        Arc::clone(&infrastructure),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh lost-acknowledgement store");
    store
        .native_graph
        .fail_next_publication
        .store(true, Ordering::Release);
    assert!(matches!(
        store.apply_native_graph(&request, &control()),
        Err(NativeGraphError::CommitIndeterminate { .. })
    ));
    record_verified_fault();
    assert!(matches!(
        store.apply_native_graph(&request, &control()),
        Err(NativeGraphError::WritesStopped)
    ));
    store.close().expect("close stopped store");
    drop(store);
    let reopened = Store::open_native_graph_with_infrastructure(
        &durable_path,
        native_options(),
        None,
        Arc::clone(&infrastructure),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
    )
    .expect("reopen durable history");
    let resolved = reopened
        .apply_native_graph(&request, &control())
        .expect("durable retry");
    assert!(resolved[0].replayed);
    assert_eq!(resolved[0].generation.get(), 1);
    let durable = node(&reopened, &resolved[0]);
    drop(resolved);
    let again = reopened
        .apply_native_graph(&request, &control())
        .expect("second durable retry");
    assert_eq!(again[0].entity, EntityId::Node(durable));
    assert!(again[0].replayed);
    drop(again);
    // Exactly one incarnation exists: the key answers that identity and only it.
    assert_eq!(observe_node(&reopened, durable).map(|node| node.2), Some(1));
    reopened.close().expect("close resolved store");
    drop(reopened);

    // A failure before the durable append allocates nothing at all.
    let lost_path = parent.path().join("pre-durable");
    let store = Store::create_native_graph_with_infrastructure(
        &lost_path,
        native_options(),
        None,
        Arc::clone(&infrastructure),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh pre-durable store");
    vfs.arm_fault(FaultPoint::Append);
    assert!(matches!(
        store.apply_native_graph(&request, &control()),
        Err(NativeGraphError::CommitIndeterminate { .. })
    ));
    vfs.assert_fired_once();
    store.close().expect("close pre-durable store");
    drop(store);
    let reopened = Store::open_native_graph_with_infrastructure(
        &lost_path,
        native_options(),
        None,
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
    )
    .expect("reopen pre-durable history");
    refuse(
        &reopened,
        &[StructuredWrite {
            key: key(EntityKind::Node, "uncertain"),
            revision: revision(2),
            operation: StructuredOperation::Recreate(revision(1)),
            image: Some(WriteImage::Node(&image)),
        }],
        "Missing",
    );
    let committed = reopened
        .apply_native_graph(&request, &control())
        .expect("fresh commit after a pre-durable failure");
    assert!(!committed[0].replayed);
    assert_eq!(committed[0].generation.get(), 1);
    let fresh = node(&reopened, &committed[0]);
    drop(committed);
    assert_eq!(observe_node(&reopened, fresh).map(|node| node.2), Some(1));
    reopened.close().expect("close fresh store");
}

#[cfg_attr(test, test)]
fn ze36_identity_indeterminate_commit_resolves_once() {
    run_ze36_identity_indeterminate_commit_resolves_once();
}

// ---------------------------------------------------------------------------
// The observed identity script
// ---------------------------------------------------------------------------

/// Runs one keyed script through the real writer and records primitive facts:
/// every receipt, the visible rows of two watches after every batch, and the
/// rows a reader admitted before the deletion still observes afterwards.
fn observe_identity_script() -> IdentityState {
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("script");
    let store = store_at(&path);
    let mut outcomes = Vec::new();
    let mut watched = Vec::new();

    let mut first_properties = value(0xA1);
    let mut peer_properties = value(0xB1);
    let installed = crate::property_graph::with_local_refs(|refs| {
        let first = CanonicalContents::node(&mut [], &mut first_properties, None, None)
            .expect("first image");
        let peer =
            CanonicalContents::node(&mut [], &mut peer_properties, None, None).expect("peer image");
        store
            .apply_native_graph(&script_batch(&first, &peer, refs), &control())
            .expect("install script")
    });
    let mut first = node(&store, &installed[0]);
    let peer = node(&store, &installed[1]);
    for (index, key) in [KEY_FIRST, KEY_PEER, KEY_EDGE].into_iter().enumerate() {
        outcomes.push(outcome(key, &installed[index]));
    }
    drop(installed);
    observe_watches(&store, &mut watched, 0, first, peer);

    let replayed = crate::property_graph::with_local_refs(|refs| {
        let mut first_properties = value(0xA1);
        let mut peer_properties = value(0xB1);
        let first_image = CanonicalContents::node(&mut [], &mut first_properties, None, None)
            .expect("first image");
        let peer_image =
            CanonicalContents::node(&mut [], &mut peer_properties, None, None).expect("peer image");
        store
            .apply_native_graph(&script_batch(&first_image, &peer_image, refs), &control())
            .expect("replay script")
    });
    for (index, key) in [KEY_FIRST, KEY_PEER, KEY_EDGE].into_iter().enumerate() {
        outcomes.push(outcome(key, &replayed[index]));
    }
    drop(replayed);
    observe_watches(&store, &mut watched, 1, first, peer);

    let mut changed_properties = value(0xA2);
    let changed =
        CanonicalContents::node(&mut [], &mut changed_properties, None, None).expect("changed");
    let updated = store
        .apply_native_graph(
            &[StructuredWrite {
                key: key(EntityKind::Node, "first"),
                revision: revision(2),
                operation: StructuredOperation::Put(EntityId::Node(first)),
                image: Some(WriteImage::Node(&changed)),
            }],
            &control(),
        )
        .expect("put first");
    outcomes.push(outcome(KEY_FIRST, &updated[0]));
    drop(updated);
    observe_watches(&store, &mut watched, 2, first, peer);

    // A reader admitted here keeps the pre-deletion generation for its lifetime.
    let retained = store.admit_native_read().expect("retained reader");
    let retained_generation = retained.bundle().base().generation.get();
    let retired = first;

    let detached = store
        .apply_native_graph(
            &[StructuredWrite {
                key: key(EntityKind::Node, "first"),
                revision: revision(3),
                operation: StructuredOperation::Delete(
                    EntityId::Node(first),
                    GraphDeleteMode::Detach,
                ),
                image: None,
            }],
            &control(),
        )
        .expect("detach first");
    outcomes.push(outcome(KEY_FIRST, &detached[0]));
    drop(detached);
    observe_watches(&store, &mut watched, 3, first, peer);

    let mut recreated_properties = value(0xA4);
    let recreated_image = CanonicalContents::node(&mut [], &mut recreated_properties, None, None)
        .expect("recreated image");
    let recreated = store
        .apply_native_graph(
            &[StructuredWrite {
                key: key(EntityKind::Node, "first"),
                revision: revision(4),
                operation: StructuredOperation::Recreate(revision(3)),
                image: Some(WriteImage::Node(&recreated_image)),
            }],
            &control(),
        )
        .expect("recreate first");
    outcomes.push(outcome(KEY_FIRST, &recreated[0]));
    first = node(&store, &recreated[0]);
    drop(recreated);
    observe_watches(&store, &mut watched, 4, first, peer);

    let second_edge = store
        .apply_native_graph(
            &[StructuredWrite {
                key: key(EntityKind::Relationship, "second-edge"),
                revision: revision(1),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Existing(first),
                    target: NodeRef::Existing(peer),
                    relationship_type: GraphName::new("LINKS").expect("relationship type"),
                    properties: &[],
                }),
            }],
            &control(),
        )
        .expect("create the second edge");
    outcomes.push(outcome(KEY_SECOND_EDGE, &second_edge[0]));
    drop(second_edge);
    observe_watches(&store, &mut watched, 5, first, peer);

    // A stale revision is refused, and the batch changes nothing.
    let error = store
        .apply_native_graph(
            &[StructuredWrite {
                key: key(EntityKind::Node, "first"),
                revision: revision(1),
                operation: StructuredOperation::Put(EntityId::Node(first)),
                image: Some(WriteImage::Node(&recreated_image)),
            }],
            &control(),
        )
        .err()
        .expect("stale revision refusal");
    outcomes.push(ObservedOutcome {
        key: KEY_FIRST,
        rejection: Some(rejection(&error)),
        entity: 0,
        revision: 0,
        generation: 0,
        replayed: false,
    });
    observe_watches(&store, &mut watched, 6, first, peer);

    commit_maintenance(&store).expect("script reclamation cycle");
    let retained_rows = adjacency(
        &rows_for_lease(&store, &retained, retired, peer, DirectionSelection::Out),
        retired,
        true,
    );
    drop(retained);
    let fresh = store.admit_native_read().expect("fresh reader");
    let fresh_generation = fresh.bundle().base().generation.get();
    let fresh_rows = adjacency(
        &rows_for_lease(&store, &fresh, first, peer, DirectionSelection::Out),
        first,
        true,
    );
    drop(fresh);
    store.close().expect("close script store");
    IdentityState {
        outcomes,
        watched,
        retained_generation,
        retained_rows,
        fresh_generation,
        fresh_rows,
    }
}

/// Records the OUT rows of the first key and the IN rows of the peer key.
fn observe_watches(
    store: &Store,
    watched: &mut Vec<ObservedWatch>,
    batch: usize,
    first: NodeId,
    peer: NodeId,
) {
    let lease = store.admit_native_read().expect("watch reader");
    let generation = lease.bundle().base().generation.get();
    for (key, bound, outgoing, direction) in [
        (KEY_FIRST, first, true, DirectionSelection::Out),
        (KEY_PEER, peer, false, DirectionSelection::In),
    ] {
        let rows = rows_for_lease(store, &lease, bound, peer, direction);
        watched.push(ObservedWatch {
            batch,
            generation,
            key,
            outgoing,
            rows: adjacency(&rows, bound, outgoing),
        });
    }
    drop(lease);
}

/// The directed production paths behind ZE-36 acceptance. A receipt is pushed
/// only after its body returned, so a receipt proves its assertions passed.
#[cfg(feature = "test-support")]
pub(super) fn run_actual_probe(_seed: u64) -> IdentityProbeReport {
    let bodies: [(&'static str, u64, fn()); 5] = [
        (
            "property-graph.identity.replay",
            1,
            run_ze36_identity_replay_survives_reopen_and_reclamation,
        ),
        (
            "property-graph.identity.incarnation",
            1,
            run_ze36_identity_incarnation_fences_refuse_stale_requests,
        ),
        (
            "property-graph.identity.revision",
            1,
            run_ze36_identity_revisions_and_high_waters_survive_reopen,
        ),
        (
            "property-graph.identity.relationship",
            1,
            run_ze36_identity_relationship_endpoints_are_immutable,
        ),
        (
            "property-graph.identity.indeterminate",
            1,
            run_ze36_identity_indeterminate_commit_resolves_once,
        ),
    ];
    let mut receipts = Vec::new();
    for (key, clean_controls, body) in bodies {
        reset_verified_faults();
        body();
        receipts.push(crate::graph_read_view_test_support::PathReceipt {
            key,
            fires: take_verified_faults(),
            clean_controls,
        });
    }
    IdentityProbeReport {
        receipts,
        state: observe_identity_script(),
    }
}

#[cfg_attr(test, test)]
fn ze36_identity_script_observes_every_watch() {
    let state = observe_identity_script();
    assert_eq!(state.outcomes.len(), 11);
    assert_eq!(state.watched.len(), 14);
    assert!(state.fresh_generation > state.retained_generation);
}
