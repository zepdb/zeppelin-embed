//! ZE-52 slice D4: plain DELETE items in the `Mutate` occurrence.
//!
//! These reuse slice D2's plan specifications, stores and public-path
//! observations. Plain DELETE stages a `Restrict` tombstone through the
//! statement overlay; whether a deleted node still has a live incident
//! relationship is decided once, when the statement is finalized, by the
//! same overlay checks every Cypher statement uses. Every rejection is
//! checked to leave the published generation and the reopened graph exactly
//! as they were.

use super::d3::{node_id, structured_node};
use super::*;
use crate::lifecycle::native_graph::NativeGraphError;
use crate::property_graph::query::expression::{ExpressionError, ExpressionFailure};

/// Two linked nodes `a -[LINKS]-> b` and an unlinked node `lonely`, carrying
/// `p` = 1, 2 and 3.
struct Linked {
    a: NodeId,
    b: NodeId,
    lonely: NodeId,
    r: RelId,
}

fn linked(store: &D2Store) -> Linked {
    let receipts = crate::property_graph::with_local_refs(|refs| {
        let p = GraphName::new("p").unwrap();
        let value = |v| {
            [GraphProperty::new(
                p,
                PropertyValue::new(PropertyData::I64(v)).unwrap(),
            )]
        };
        let (mut one, mut two, mut three) = (value(1), value(2), value(3));
        let a = CanonicalContents::node(&mut [], &mut one, None, None).unwrap();
        let b = CanonicalContents::node(&mut [], &mut two, None, None).unwrap();
        let lonely = CanonicalContents::node(&mut [], &mut three, None, None).unwrap();
        let properties = [GraphProperty::new(
            GraphName::new("w").unwrap(),
            PropertyValue::new(PropertyData::I64(1)).unwrap(),
        )];
        let node = |key, image| StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "d4", key).unwrap(),
            revision: GraphRevision::new(1).unwrap(),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(image)),
        };
        let requests = [
            node("a", &a),
            node("b", &b),
            node("lonely", &lonely),
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "d4", "ab").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &properties,
                }),
            },
        ];
        store
            .store
            .apply_native_graph(&requests, &control())
            .expect("publish linked fixture")
    });
    let EntityId::Relationship(r) = receipts[3].entity else {
        panic!("relationship receipt");
    };
    Linked {
        a: node(&receipts[0]),
        b: node(&receipts[1]),
        lonely: node(&receipts[2]),
        r,
    }
}

/// `MATCH ()-[r]->() WHERE id(r) = <id> RETURN r, 0`, read-only: one row
/// while the relationship is live, none once it is deleted.
fn relationship_rows(store: &D2Store, r: RelId) -> Vec<(u128, i64)> {
    read(
        store,
        &Spec {
            operators: vec![
                (vec![], K::Unit),
                (vec![0], K::LookupRelationship(0, r)),
                (vec![1], K::Project(vec![(100, 0), (101, 1)])),
                (vec![2], K::Collect),
            ],
            expressions: vec![E::Slot(0), E::I64(0)],
        },
    )
}

/// Asserts `outcome` is the typed rejection `expected` names and that the
/// statement reached execution rather than being refused at build time.
fn rejected(
    label: &str,
    outcome: (
        Result<(EntityValueRows, NativeMutationReport), NativeMutationError>,
        bool,
    ),
    expected: fn(&NativeMutationError) -> bool,
) {
    match outcome {
        (Err(error), false) if expected(&error) => {}
        (Err(error), refused) => panic!("{label}: wrong rejection {error:?} (refused {refused})"),
        (Ok((_, report)), _) => panic!("{label}: committed {:?}", report.changed),
    }
}

fn incident(error: &NativeMutationError) -> bool {
    matches!(
        error,
        NativeMutationError::Graph(NativeGraphError::Stage(StageError::IncidentRelationship))
    )
}

/// A write item that reaches a deleted entity.
fn deleted(error: &NativeMutationError) -> bool {
    matches!(
        error,
        NativeMutationError::Execution(NativeExecutionError::Stage(StageError::DeletedEntity))
    )
}

/// An expression, in an item or a later clause, that reads a deleted entity.
fn deleted_read(error: &NativeMutationError) -> bool {
    matches!(
        error,
        NativeMutationError::Execution(NativeExecutionError::Expression(ExpressionError {
            failure: ExpressionFailure::Stage(StageError::DeletedEntity),
            ..
        }))
    )
}

/// `MATCH (a), ()-[r]->() DELETE a, r RETURN a, 0`. The node is named before
/// its incident relationship, yet the statement commits: incident liveness
/// is decided when the statement is finalized, after `r` is deleted too. The
/// deleted node's reference is still returned. After reopen `a` and `r` are
/// gone and `b`, the relationship's other endpoint, is byte-for-byte as it
/// was.
#[test]
fn ze52_slice_d4_delete_node_and_its_relationship_reopens() {
    let store = D2Store::create(None);
    let fixture = linked(&store);
    let before = store.generation();
    let b_before = records(&store, &[fixture.b], &[]);
    let spec = Spec {
        operators: vec![
            (vec![], K::Unit),
            (vec![0], K::LookupNode(0, fixture.a)),
            (vec![], K::Unit),
            (vec![2], K::LookupRelationship(1, fixture.r)),
            (vec![1, 3], K::Join),
            (vec![4], K::Eager),
            (vec![5], K::Mutate(vec![M::Delete(0), M::Delete(1)])),
            (vec![6], K::Project(vec![(100, 0), (101, 2)])),
            (vec![7], K::Collect),
        ],
        expressions: vec![E::Slot(0), E::Slot(1), E::I64(0)],
    };

    let (rows, report) = committed(mutate(&store, &spec, IMAGES));
    assert_eq!(rows, vec![(fixture.a.get(), 0)]);
    assert_eq!(report.disposition, BatchDisposition::Changed);
    assert_eq!(report.admitted.get(), before);
    assert_eq!(report.changed.map(GraphGeneration::get), Some(before + 1));

    let store = store.reopen();
    assert_eq!(store.generation(), before + 1);
    assert_eq!(
        sorted(read(&store, &scan_p())),
        pairs(&[fixture.b, fixture.lonely], &[2, 3])
    );
    assert_eq!(relationship_rows(&store, fixture.r), vec![]);
    assert_eq!(records(&store, &[fixture.b], &[]), b_before);
    store.store.close().expect("close d4 store");
}

/// Plain DELETE of a node that still has a live incident relationship at the
/// end of the statement fails loudly and publishes nothing, whether that
/// relationship is committed, created in this statement to an existing node,
/// or created in this statement between two fresh nodes.
#[test]
fn ze52_slice_d4_delete_rejects_live_incident_relationships() {
    let store = D2Store::create(None);
    let fixture = linked(&store);
    let before = store.generation();
    let unit_then = |operators: Vec<(Vec<u32>, K)>, items: Vec<M>, expressions: Vec<E>| {
        let mut all = operators;
        let last = u32::try_from(all.len() - 1).unwrap();
        all.push((vec![last], K::Eager));
        all.push((vec![last + 1], K::Mutate(items)));
        all.push((vec![last + 2], K::Project(vec![(100, 0), (101, 1)])));
        all.push((vec![last + 3], K::Collect));
        Spec {
            operators: all,
            expressions,
        }
    };
    let lookup = |id| vec![(vec![], K::Unit), (vec![0], K::LookupNode(0, id))];

    // MATCH (a) DELETE a: `a -[LINKS]-> b` is committed and live.
    let committed_edge = unit_then(
        lookup(fixture.a),
        vec![M::Delete(0)],
        vec![E::Slot(0), E::I64(0)],
    );
    rejected(
        "committed relationship",
        mutate(&store, &committed_edge, IMAGES),
        incident,
    );
    // MATCH (b) DELETE b: the committed relationship's target is incident too.
    let committed_target = unit_then(
        lookup(fixture.b),
        vec![M::Delete(0)],
        vec![E::Slot(0), E::I64(0)],
    );
    rejected(
        "committed relationship target",
        mutate(&store, &committed_target, IMAGES),
        incident,
    );
    // MATCH (n) CREATE (n)-[:R]->(m) DELETE n on the unlinked node.
    let fresh_edge = unit_then(
        lookup(fixture.lonely),
        vec![
            M::CreateNode(1, &[]),
            M::CreateRelationship(2, 0, 2, "R"),
            M::Delete(0),
        ],
        vec![E::Slot(0), E::I64(0), E::Slot(1)],
    );
    rejected(
        "fresh relationship to an existing node",
        mutate(&store, &fresh_edge, IMAGES),
        incident,
    );
    // CREATE (m)-[:R]->(k) DELETE m: both nodes and the edge are fresh.
    let all_fresh = unit_then(
        vec![(vec![], K::Unit)],
        vec![
            M::CreateNode(0, &[]),
            M::CreateNode(1, &[]),
            M::CreateRelationship(2, 0, 2, "R"),
            M::Delete(0),
        ],
        vec![E::Slot(0), E::I64(0), E::Slot(1)],
    );
    rejected(
        "fresh relationship between fresh nodes",
        mutate(&store, &all_fresh, IMAGES),
        incident,
    );
    assert_eq!(store.generation(), before);

    let store = store.reopen();
    assert_eq!(store.generation(), before);
    assert_eq!(
        sorted(read(&store, &scan_p())),
        pairs(&[fixture.a, fixture.b, fixture.lonely], &[1, 2, 3])
    );
    assert_eq!(
        relationship_rows(&store, fixture.r),
        vec![(fixture.r.get(), 0)]
    );
    // No rejected statement consumed a node identity.
    let fence = [fixture.a, fixture.b, fixture.lonely]
        .iter()
        .map(|id| id.get())
        .max()
        .unwrap();
    assert_eq!(structured_node(&store, "after").get(), fence + 1);
    store.store.close().expect("close d4 store");
}

/// `CREATE (m)-[r:R]->(m) RETURN r, 0`: the identity the next relationship
/// create takes, which shows how far the relationship fence has moved.
fn next_relationship(store: &D2Store) -> u128 {
    let spec = Spec {
        operators: vec![
            (vec![], K::Unit),
            (vec![0], K::Eager),
            (
                vec![1],
                K::Mutate(vec![
                    M::CreateNode(0, &[]),
                    M::CreateRelationship(1, 0, 0, "R"),
                ]),
            ),
            (vec![2], K::Project(vec![(100, 1), (101, 2)])),
            (vec![3], K::Collect),
        ],
        expressions: vec![E::Slot(0), E::Slot(1), E::I64(0)],
    };
    let (rows, _) = committed(mutate(store, &spec, IMAGES));
    assert_eq!(rows.len(), 1);
    rows[0].0
}

/// `MATCH (a) SET a.p = 9 CREATE (m:T)-[r:R]->(k) SET m.p = 5
/// DELETE m, k, r RETURN a, a.p`. Every entity the statement created is
/// deleted again before it ends, so only `a`'s change publishes. The fresh
/// entries are suppressed by the same net-empty rule the overlay applies to
/// a local create and delete, yet the identities they bound stay consumed:
/// later node and relationship creates take the identities after them.
#[test]
fn ze52_slice_d4_deleted_creates_burn_identities_beside_a_change() {
    let store = D2Store::create(None);
    let nodes = three_nodes(&store);
    let fence = nodes.iter().map(|node| node.get()).max().unwrap();
    let before = store.generation();
    let spec = Spec {
        operators: vec![
            (vec![], K::Unit),
            (vec![0], K::LookupNode(0, nodes[0])),
            (vec![1], K::Eager),
            (
                vec![2],
                K::Mutate(vec![
                    M::Set(0, "p", 1),
                    M::CreateNode(1, &["T"]),
                    M::Set(2, "p", 3),
                    M::CreateNode(2, &[]),
                    M::CreateRelationship(3, 2, 4, "R"),
                    M::Delete(2),
                    M::Delete(4),
                    M::Delete(5),
                ]),
            ),
            (vec![3], K::Project(vec![(100, 0), (101, 6)])),
            (vec![4], K::Collect),
        ],
        expressions: vec![
            E::Slot(0),
            E::I64(9),
            E::Slot(1),
            E::I64(5),
            E::Slot(2),
            E::Slot(3),
            E::Property(0, "p"),
        ],
    };

    let (rows, report) = committed(mutate(&store, &spec, IMAGES));
    assert_eq!(rows, vec![(nodes[0].get(), 9)]);
    assert_eq!(report.disposition, BatchDisposition::Changed);
    assert_eq!(report.changed.map(GraphGeneration::get), Some(before + 1));

    let store = store.reopen();
    assert_eq!(store.generation(), before + 1);
    assert_eq!(sorted(read(&store, &scan_p())), pairs(&nodes, &[9, 2, 3]));
    assert_eq!(revisions(&store, &nodes), vec![2, 1, 1]);
    assert_eq!(relationship_rows(&store, RelId::new(1).unwrap()), vec![]);
    // Both node identities and the relationship identity stay burned.
    assert_eq!(structured_node(&store, "after").get(), fence + 3);
    assert_eq!(next_relationship(&store), 2);
    store.store.close().expect("close d4 store");
}

/// Consumed identities commit without changing surviving entities.
#[test]
fn ze190_fence_only_statement_commits_and_reopens() {
    for checkpoint in [false, true] {
        let store = D2Store::create(Some(document()));
        let nodes = three_nodes(&store);
        let mut properties = [GraphProperty::new(
            GraphName::new("p").unwrap(),
            PropertyValue::new(PropertyData::I64(1)).unwrap(),
        )];
        let coordinates = COORDINATES.map(f32::from_bits);
        let embedding =
            CanonicalEmbedding::new(store.document.as_ref().unwrap(), &coordinates).unwrap();
        let image =
            CanonicalContents::node(&mut [], &mut properties, Some(RICH_TEXT), Some(embedding))
                .unwrap();
        store
            .store
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "d2", "one").unwrap(),
                    revision: GraphRevision::new(2).unwrap(),
                    operation: StructuredOperation::Put(EntityId::Node(nodes[0])),
                    image: Some(WriteImage::Node(&image)),
                }],
                &control(),
            )
            .unwrap();

        let fence = nodes.iter().map(|node| node.get()).max().unwrap();
        let before = store.generation();
        store.store.checkpoint_native_graph(&control()).unwrap();
        assert_eq!(
            store.store.snapshot().unwrap().generation(),
            before + 1,
            "checkpoint publishes a manifest generation"
        );
        let lease = store.store.admit_native_read().unwrap();
        let roots = lease.bundle().roots().references();
        let high = lease.bundle().high_waters();
        let symbols = catalog_symbols(&store.path, lease.bundle().catalog());
        let membership = search_membership(&store.path, lease.bundle().text());
        let vectors = search_membership(&store.path, lease.bundle().vector());
        assert!(membership.is_none() && vectors.is_none());
        drop(lease);

        let spec = Spec {
            operators: vec![
                (vec![], K::Unit),
                (vec![0], K::Eager),
                (
                    vec![1],
                    K::Mutate(vec![
                        M::CreateNode(0, &["T"]),
                        M::Set(0, "p", 3),
                        M::CreateNode(1, &[]),
                        M::CreateRelationship(2, 0, 1, "R"),
                        M::Delete(0),
                        M::Delete(1),
                        M::Delete(2),
                    ]),
                ),
                (vec![2], K::Project(vec![(100, 0), (101, 4)])),
                (vec![3], K::Collect),
            ],
            expressions: vec![E::Slot(0), E::Slot(1), E::Slot(2), E::I64(5), E::I64(9)],
        };

        let (rows, report) = committed(mutate(&store, &spec, IMAGES));
        assert_eq!(rows, vec![(fence + 1, 9)]);
        assert_eq!(report.disposition, BatchDisposition::Changed);
        assert_eq!(report.changed.map(GraphGeneration::get), Some(before + 2));
        assert_eq!(store.generation(), before + 2);
        let lease = store.store.admit_native_read().unwrap();
        assert_eq!(lease.bundle().roots().references(), roots);
        assert_eq!(lease.bundle().high_waters().node, high.node + 2);
        assert_eq!(
            lease.bundle().high_waters().relationship,
            high.relationship + 1
        );
        assert_eq!(lease.bundle().high_waters().symbols, high.symbols);
        assert_eq!(
            catalog_symbols(&store.path, lease.bundle().catalog()),
            symbols
        );
        assert_eq!(
            search_membership(&store.path, lease.bundle().text()),
            membership
        );
        assert_eq!(
            search_membership(&store.path, lease.bundle().vector()),
            vectors
        );
        drop(lease);
        assert_fence_only_wal(&store.path);

        let store = if checkpoint {
            store.reopen()
        } else {
            let D2Store {
                _directory,
                path,
                store,
                document,
            } = store;
            drop(store);
            let store = Store::open_native_graph(&path, options(), document.clone()).unwrap();
            D2Store {
                _directory,
                path,
                store,
                document,
            }
        };
        assert_eq!(store.generation(), before + 2);
        assert_eq!(sorted(read(&store, &scan_p())), pairs(&nodes, &[1, 2, 3]));
        assert_eq!(revisions(&store, &nodes), vec![2, 1, 1]);
        let lease = store.store.admit_native_read().unwrap();
        assert_eq!(lease.bundle().roots().references(), roots);
        assert_eq!(
            catalog_symbols(&store.path, lease.bundle().catalog()),
            symbols
        );
        assert_eq!(
            search_membership(&store.path, lease.bundle().text()),
            membership
        );
        assert_eq!(
            search_membership(&store.path, lease.bundle().vector()),
            vectors
        );
        drop(lease);
        assert_eq!(structured_node(&store, "after").get(), fence + 3);
        assert_eq!(next_relationship(&store), 2);
        store.store.close().expect("close d4 store");
    }
}

/// Once an entity is deleted, every later read or write of it in the same
/// statement fails with the typed `DeletedEntity`, and nothing publishes:
/// a later item of the same row, a later clause (a projection or a filter),
/// a later row that reaches the same node, and a CREATE that uses it as an
/// endpoint.
#[test]
fn ze52_slice_d4_reads_of_a_deleted_entity_fail_loudly() {
    let store = D2Store::create(None);
    let nodes = three_nodes(&store);
    let fence = nodes.iter().map(|node| node.get()).max().unwrap();
    let before = store.generation();
    let first = nodes[0];

    // MATCH (n) DELETE n SET n.p = 1
    let later_item = scan_mutate(
        vec![M::Delete(0), M::Set(0, "p", 1)],
        1,
        vec![E::Slot(0), E::I64(1)],
    );
    rejected("later item", mutate(&store, &later_item, IMAGES), deleted);

    // MATCH (n) DELETE n RETURN n, n.p
    let later_projection =
        scan_mutate(vec![M::Delete(0)], 1, vec![E::Slot(0), E::Property(0, "p")]);
    rejected(
        "later projection",
        mutate(&store, &later_projection, IMAGES),
        deleted_read,
    );

    // MATCH (n) DELETE n WITH n WHERE n.p > 0 RETURN n, 0
    let later_filter = Spec {
        operators: vec![
            (vec![], K::Unit),
            (vec![0], K::Scan(0)),
            (vec![1], K::Eager),
            (vec![2], K::Mutate(vec![M::Delete(0)])),
            (vec![3], K::Filter(3)),
            (vec![4], K::Project(vec![(100, 0), (101, 2)])),
            (vec![5], K::Collect),
        ],
        expressions: vec![
            E::Slot(0),
            E::Property(0, "p"),
            E::I64(0),
            E::Comparison(Comparison::Greater, 1, 2),
        ],
    };
    rejected(
        "later filter",
        mutate(&store, &later_filter, IMAGES),
        deleted_read,
    );

    // MATCH (n) WHERE id(n) = <first>, (m) SET n.p = 7 DELETE n: the first
    // row deletes `n`, and the second row's SET reaches the deleted node.
    let later_row = Spec {
        operators: vec![
            (vec![], K::Unit),
            (vec![0], K::LookupNode(0, first)),
            (vec![], K::Unit),
            (vec![2], K::Scan(1)),
            (vec![1, 3], K::Join),
            (vec![4], K::Eager),
            (vec![5], K::Mutate(vec![M::Set(0, "p", 2), M::Delete(0)])),
            (vec![6], K::Project(vec![(100, 1), (101, 2)])),
            (vec![7], K::Collect),
        ],
        expressions: vec![E::Slot(0), E::Slot(1), E::I64(7)],
    };
    rejected("later row", mutate(&store, &later_row, IMAGES), deleted);

    // MATCH (n) DELETE n CREATE (n)-[:R]->(m)
    let deleted_endpoint = Spec {
        operators: vec![
            (vec![], K::Unit),
            (vec![0], K::LookupNode(0, first)),
            (vec![1], K::Eager),
            (
                vec![2],
                K::Mutate(vec![
                    M::Delete(0),
                    M::CreateNode(1, &[]),
                    M::CreateRelationship(2, 0, 1, "R"),
                ]),
            ),
            (vec![3], K::Project(vec![(100, 1), (101, 2)])),
            (vec![4], K::Collect),
        ],
        expressions: vec![E::Slot(0), E::Slot(1), E::I64(0)],
    };
    rejected(
        "deleted endpoint",
        mutate(&store, &deleted_endpoint, IMAGES),
        deleted,
    );
    // The same CREATE with the endpoint's roles swapped fails the same way.
    let deleted_target = Spec {
        operators: vec![
            (vec![], K::Unit),
            (vec![0], K::LookupNode(0, first)),
            (vec![1], K::Eager),
            (
                vec![2],
                K::Mutate(vec![
                    M::Delete(0),
                    M::CreateNode(1, &[]),
                    M::CreateRelationship(2, 1, 0, "R"),
                ]),
            ),
            (vec![3], K::Project(vec![(100, 1), (101, 2)])),
            (vec![4], K::Collect),
        ],
        expressions: vec![E::Slot(0), E::Slot(1), E::I64(0)],
    };
    rejected(
        "deleted target endpoint",
        mutate(&store, &deleted_target, IMAGES),
        deleted,
    );
    assert_eq!(store.generation(), before);

    let store = store.reopen();
    assert_eq!(store.generation(), before);
    assert_eq!(sorted(read(&store, &scan_p())), pairs(&nodes, &[1, 2, 3]));
    assert_eq!(revisions(&store, &nodes), vec![1, 1, 1]);
    assert_eq!(structured_node(&store, "after").get(), fence + 1);
    store.store.close().expect("close d4 store");
}

/// Three rows that all reach one node each DELETE it: deletion is
/// idempotent within a statement, so it commits once and removes exactly
/// that node. A DELETE whose target is null, reached through OPTIONAL
/// MATCH, is skipped, so a statement of only null deletes is a NoOp.
#[test]
fn ze52_slice_d4_repeated_and_null_deletes() {
    let store = D2Store::create(None);
    let nodes = three_nodes(&store);
    let fence = nodes.iter().map(|node| node.get()).max().unwrap();
    let before = store.generation();

    let null_delete = Spec {
        operators: vec![
            (vec![], K::Unit),
            (vec![0], K::Scan(0)),
            (vec![1], K::LookupNode(1, node_id(fence + 1000))),
            (vec![1, 2], K::Optional),
            (vec![3], K::Eager),
            (vec![4], K::Mutate(vec![M::Delete(1)])),
            (vec![5], K::Project(vec![(100, 0), (101, 2)])),
            (vec![6], K::Collect),
        ],
        expressions: vec![E::Slot(0), E::Slot(1), E::Property(0, "p")],
    };
    let (rows, report) = committed(mutate(&store, &null_delete, IMAGES));
    assert_eq!(sorted(rows), pairs(&nodes, &[1, 2, 3]));
    assert_eq!(report.disposition, BatchDisposition::NoOp);
    assert_eq!(report.changed, None);
    assert_eq!(store.generation(), before);

    let first = nodes[0];
    let repeated = Spec {
        operators: vec![
            (vec![], K::Unit),
            (vec![0], K::LookupNode(0, first)),
            (vec![], K::Unit),
            (vec![2], K::Scan(1)),
            (vec![1, 3], K::Join),
            (vec![4], K::Eager),
            (vec![5], K::Mutate(vec![M::Delete(0)])),
            (vec![6], K::Project(vec![(100, 0), (101, 1)])),
            (vec![7], K::Collect),
        ],
        expressions: vec![E::Slot(0), E::I64(0)],
    };
    let (rows, report) = committed(mutate(&store, &repeated, IMAGES));
    assert_eq!(rows, vec![(first.get(), 0); 3]);
    assert_eq!(report.disposition, BatchDisposition::Changed);
    assert_eq!(report.changed.map(GraphGeneration::get), Some(before + 1));

    let store = store.reopen();
    assert_eq!(store.generation(), before + 1);
    assert_eq!(sorted(read(&store, &scan_p())), pairs(&nodes[1..], &[2, 3]));
    store.store.close().expect("close d4 store");
}

mod detach;

// Read the existing participant layouts: only metadata envelopes may change.
fn participant_payload(
    path: &std::path::Path,
    required: crate::property_graph::wal::RequiredRef,
) -> Vec<u8> {
    use crate::property_graph::storage::{allocation::artifact_path, artifact};
    let bytes = std::fs::read(artifact_path(path, required.object.artifact)).unwrap();
    let frame = artifact::decode(
        artifact::ContainerKind::Object,
        Some((required.object.store, required.object.artifact)),
        &bytes,
    )
    .unwrap();
    frame
        .framed_block(required.block)
        .unwrap()
        .payload()
        .to_vec()
}

fn catalog_symbols(
    path: &std::path::Path,
    required: crate::property_graph::wal::RequiredRef,
) -> Vec<(crate::property_graph::catalog::Symbol, String)> {
    let payload = participant_payload(path, required);
    let image = crate::property_graph::catalog::CatalogImage::decode(
        &payload[8..],
        usize::MAX,
        &mut || Ok(()),
    )
    .unwrap();
    image
        .symbols
        .entries()
        .iter()
        .map(|entry| (entry.symbol, entry.name.as_str().to_owned()))
        .collect()
}

fn search_membership(
    path: &std::path::Path,
    required: Option<crate::property_graph::wal::RequiredRef>,
) -> Option<Vec<u8>> {
    required.map(|required| {
        let payload = participant_payload(path, required);
        // RootDescriptor: membership and source roots, live rows and length.
        assert_eq!(payload.len(), 256);
        payload[160..256].to_vec()
    })
}

fn assert_fence_only_wal(path: &std::path::Path) {
    // Unified graph envelopes live in op 10 of wal.ze, with no ZE-38 header.
    let wal = crate::wal::WalReader::open(&crate::vfs::StdVfs, &path.join("wal.ze"))
        .unwrap()
        .into_clean()
        .unwrap();
    let record = wal
        .records()
        .iter()
        .rev()
        .find(|record| record.op == crate::ingest::wal_payload::GRAPH_COMMIT_V1)
        .unwrap();
    let bytes = crate::ingest::wal_payload::decode_graph_commit(record.payload().unwrap()).unwrap();
    let mut offset = 0;
    let mut commits = 0;
    let mut mutations = 0;
    let mut last_mutations = None;
    while offset < bytes.len() {
        let kind = u16::from_le_bytes(bytes[offset + 4..offset + 6].try_into().unwrap());
        if kind == 1 {
            mutations = 0;
        }
        mutations += usize::from(kind == 2);
        if kind == 6 {
            last_mutations = Some(mutations);
            commits += 1;
        }
        let length =
            u32::from_le_bytes(bytes[offset + 8..offset + 12].try_into().unwrap()) as usize;
        offset += 72 + length;
    }
    assert_eq!(offset, bytes.len());
    assert!(commits > 0);
    assert_eq!(
        last_mutations,
        Some(0),
        "fence-only WAL batch has entity mutations"
    );
}
