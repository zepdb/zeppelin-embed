//! ZE-52 slice D3: CREATE items in the `Mutate` occurrence.
//!
//! These reuse slice D2's plan specifications, stores and public-path
//! observations. Every fresh identity is checked against an oracle that does
//! not read the engine's allocator: the store's allocation fence is the
//! largest identity the fixture committed, and each create takes the next
//! integer in the order the statement ran it.

use super::*;

/// The exact canonical bytes of one node image with only `i64` properties.
fn node_bytes(labels: &[&str], properties: &[(&str, i64)]) -> Vec<u8> {
    let mut labels: Vec<GraphName<'_>> = labels
        .iter()
        .map(|label| GraphName::new(label).unwrap())
        .collect();
    let mut properties: Vec<GraphProperty<'_>> = properties
        .iter()
        .map(|(name, value)| {
            GraphProperty::new(
                GraphName::new(name).unwrap(),
                PropertyValue::new(PropertyData::I64(*value)).unwrap(),
            )
        })
        .collect();
    let image = CanonicalContents::node(&mut labels, &mut properties, None, None).unwrap();
    let mut bytes = Vec::new();
    image.write_to(&mut bytes, &mut || Ok(())).unwrap();
    bytes
}

/// The exact canonical bytes of one relationship with one `i64` property.
fn relationship_bytes(source: NodeId, target: NodeId, kind: &str, w: i64) -> Vec<u8> {
    let mut properties = [GraphProperty::new(
        GraphName::new("w").unwrap(),
        PropertyValue::new(PropertyData::I64(w)).unwrap(),
    )];
    let image = CanonicalContents::relationship(
        source,
        target,
        GraphName::new(kind).unwrap(),
        &mut properties,
    )
    .unwrap();
    let mut bytes = Vec::new();
    image.write_to(&mut bytes, &mut || Ok(())).unwrap();
    bytes
}

pub(super) fn node_id(value: u128) -> NodeId {
    NodeId::new(value).unwrap()
}

/// One keyed structured create, returning the identity its receipt names.
pub(super) fn structured_node(store: &D2Store, key: &str) -> NodeId {
    let receipts = crate::property_graph::with_local_refs(|_| {
        let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
        store
            .store
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "d3", key).unwrap(),
                    revision: GraphRevision::new(1).unwrap(),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                }],
                &control(),
            )
            .expect("publish structured node")
    });
    node(&receipts[0])
}

/// `CREATE (n:B:A) SET n.p = 5, n.q = n.p + 1 RETURN n, n.q`. The node takes
/// the next identity after the store's fence, carries exactly its labels and
/// both properties, is visible to the later item and to the projection, and
/// reopens byte for byte. A structured create afterwards takes the identity
/// after it, so both paths advance one allocator.
#[test]
fn ze52_slice_d3_create_node_with_labels_and_properties_reopens() {
    let store = D2Store::create(None);
    let nodes = three_nodes(&store);
    let fence = nodes.iter().map(|node| node.get()).max().unwrap();
    let before = store.generation();
    let spec = Spec {
        operators: vec![
            (vec![], K::Unit),
            (vec![0], K::Eager),
            (
                vec![1],
                K::Mutate(vec![
                    M::CreateNode(0, &["B", "A"]),
                    M::Set(0, "p", 1),
                    M::Set(0, "q", 4),
                ]),
            ),
            (vec![2], K::Project(vec![(100, 0), (101, 5)])),
            (vec![3], K::Collect),
        ],
        expressions: vec![
            E::Slot(0),
            E::I64(5),
            E::Property(0, "p"),
            E::I64(1),
            E::Arithmetic(Arithmetic::Add, 2, 3),
            E::Property(0, "q"),
        ],
    };

    let (rows, report) = committed(mutate(&store, &spec, IMAGES));
    let created = fence + 1;
    assert_eq!(rows, vec![(created, 6)]);
    assert_eq!(report.disposition, BatchDisposition::Changed);
    assert_eq!(report.admitted.get(), before);
    assert_eq!(report.changed.map(GraphGeneration::get), Some(before + 1));

    let store = store.reopen();
    assert_eq!(store.generation(), before + 1);
    assert_eq!(
        records(&store, &[node_id(created)], &[]),
        vec![(1, node_bytes(&["A", "B"], &[("p", 5), ("q", 6)]))]
    );
    let mut expected = pairs(&nodes, &[1, 2, 3]);
    expected.push((created, 5));
    assert_eq!(sorted(read(&store, &scan_p())), sorted(expected));
    assert_eq!(structured_node(&store, "after").get(), created + 1);
    store.store.close().expect("close d3 store");
}

/// `MATCH (n) CREATE (m) SET m.p = n.p + 10 WITH m WHERE m.p > 11 RETURN m,
/// m.p ORDER BY m.p DESC`. The scan feeds exactly its three admitted nodes:
/// the nodes this statement creates are never scanned, so the statement
/// creates three nodes, not an unbounded stream. The clauses after the
/// `Mutate` read every created node's staged value.
#[test]
fn ze52_slice_d3_scans_do_not_consume_their_own_creates() {
    let store = D2Store::create(None);
    let nodes = three_nodes(&store);
    let fence = nodes.iter().map(|node| node.get()).max().unwrap();
    let spec = Spec {
        operators: vec![
            (vec![], K::Unit),
            (vec![0], K::Scan(0)),
            (vec![1], K::Eager),
            (
                vec![2],
                K::Mutate(vec![M::CreateNode(1, &[]), M::Set(4, "p", 3)]),
            ),
            (vec![3], K::Filter(7)),
            (vec![4], K::Sort(vec![(5, true)])),
            (vec![5], K::Project(vec![(100, 4), (101, 5)])),
            (vec![6], K::Collect),
        ],
        expressions: vec![
            E::Slot(0),
            E::Property(0, "p"),
            E::I64(10),
            E::Arithmetic(Arithmetic::Add, 1, 2),
            E::Slot(1),
            E::Property(4, "p"),
            E::I64(11),
            E::Comparison(Comparison::Greater, 5, 6),
        ],
    };

    let (rows, report) = committed(mutate(&store, &spec, IMAGES));
    assert_eq!(report.disposition, BatchDisposition::Changed);
    assert_eq!(rows.iter().map(|row| row.1).collect::<Vec<_>>(), [13, 12]);
    let fresh: Vec<u128> = (1..=3).map(|offset| fence + offset).collect();
    for (id, _) in &rows {
        assert!(
            fresh.contains(id),
            "row entity {id} is not a fresh identity"
        );
    }
    assert_ne!(rows[0].0, rows[1].0);

    let store = store.reopen();
    let all = read(&store, &scan_p());
    assert_eq!(all.len(), 6, "exactly three nodes were created: {all:?}");
    let created: Vec<(u128, i64)> = all.iter().copied().filter(|(id, _)| *id > fence).collect();
    assert_eq!(
        sorted(created.iter().map(|row| row.0).map(|id| (id, 0)).collect()),
        fresh.iter().map(|id| (*id, 0)).collect::<Vec<_>>()
    );
    let mut values: Vec<i64> = created.iter().map(|row| row.1).collect();
    values.sort_unstable();
    assert_eq!(values, [11, 12, 13]);
    for row in &rows {
        assert!(created.contains(row), "returned {row:?} was not published");
    }
    store.store.close().expect("close d3 store");
}

/// `MATCH (a) CREATE (a)-[r1:LINKS]->(b:F)-[r2:NEXT]->(c) SET r1.w = 7,
/// r2.w = r1.w + 1 RETURN r2, r2.w`. One relationship joins an existing node
/// to a fresh one, the other joins two fresh nodes; a later item reads a
/// created relationship's staged property, and both endpoints of both
/// relationships reopen exactly.
#[test]
fn ze52_slice_d3_create_relationships_between_existing_and_fresh_nodes() {
    let store = D2Store::create(None);
    let nodes = three_nodes(&store);
    let fence = nodes.iter().map(|node| node.get()).max().unwrap();
    let existing = nodes[0];
    let before = records(&store, &[existing], &[]);
    let spec = Spec {
        operators: vec![
            (vec![], K::Unit),
            (vec![0], K::LookupNode(0, existing)),
            (vec![1], K::Eager),
            (
                vec![2],
                K::Mutate(vec![
                    M::CreateNode(1, &["F"]),
                    M::CreateRelationship(2, 0, 1, "LINKS"),
                    M::CreateNode(3, &[]),
                    M::CreateRelationship(4, 1, 3, "NEXT"),
                    M::Set(2, "w", 4),
                    M::Set(5, "w", 8),
                ]),
            ),
            (vec![3], K::Project(vec![(100, 5), (101, 9)])),
            (vec![4], K::Collect),
        ],
        expressions: vec![
            E::Slot(0),
            E::Slot(1),
            E::Slot(2),
            E::Slot(3),
            E::I64(7),
            E::Slot(4),
            E::Property(2, "w"),
            E::I64(1),
            E::Arithmetic(Arithmetic::Add, 6, 7),
            E::Property(5, "w"),
        ],
    };

    let (rows, report) = committed(mutate(&store, &spec, IMAGES));
    // The store holds no relationship yet, so its relationship fence is zero.
    assert_eq!(rows, vec![(2, 8)]);
    assert_eq!(report.disposition, BatchDisposition::Changed);

    let store = store.reopen();
    let (b, c) = (node_id(fence + 1), node_id(fence + 2));
    let (r1, r2) = (RelId::new(1).unwrap(), RelId::new(2).unwrap());
    assert_eq!(
        records(&store, &[b, c], &[r1, r2]),
        vec![
            (1, node_bytes(&["F"], &[])),
            (1, node_bytes(&[], &[])),
            (1, relationship_bytes(existing, b, "LINKS", 7)),
            (1, relationship_bytes(b, c, "NEXT", 8)),
        ]
    );
    // The existing endpoint is referenced, never rewritten.
    assert_eq!(records(&store, &[existing], &[]), before);
    store.store.close().expect("close d3 store");
}

/// A statement that fails after it has created nodes publishes nothing and
/// consumes no identity: `MATCH (n) CREATE (m) SET m.p = n.p * (MAX / 2)`
/// overflows on the row whose `p` is 3, after other rows created nodes. A
/// null endpoint, reached through `OPTIONAL MATCH`, is a typed endpoint
/// failure on the same terms. The next successful create still takes the
/// identity after the fence.
#[test]
fn ze52_slice_d3_late_failure_after_creates_rejects_atomically() {
    let store = D2Store::create(None);
    let nodes = three_nodes(&store);
    let fence = nodes.iter().map(|node| node.get()).max().unwrap();
    let before = store.generation();
    let overflow = Spec {
        operators: vec![
            (vec![], K::Unit),
            (vec![0], K::Scan(0)),
            (vec![1], K::Eager),
            (
                vec![2],
                K::Mutate(vec![M::CreateNode(1, &[]), M::Set(4, "p", 3)]),
            ),
            (vec![3], K::Project(vec![(100, 4), (101, 5)])),
            (vec![4], K::Collect),
        ],
        expressions: vec![
            E::Slot(0),
            E::Property(0, "p"),
            E::I64(i64::MAX / 2),
            E::Arithmetic(Arithmetic::Multiply, 1, 2),
            E::Slot(1),
            E::Property(4, "p"),
        ],
    };
    match mutate(&store, &overflow, IMAGES) {
        (Err(NativeMutationError::Execution(NativeExecutionError::Expression(_))), false) => {}
        (Err(error), refused) => {
            panic!("expected a late expression failure, got {error} (refused {refused})")
        }
        (Ok((_, report)), _) => panic!("an overflowing statement committed {:?}", report.changed),
    }
    assert_eq!(store.generation(), before);

    let missing = node_id(fence + 1000);
    let null_endpoint = Spec {
        operators: vec![
            (vec![], K::Unit),
            (vec![0], K::Scan(0)),
            (vec![1], K::LookupNode(1, missing)),
            (vec![1, 2], K::Optional),
            (vec![3], K::Eager),
            (
                vec![4],
                K::Mutate(vec![
                    M::CreateNode(2, &[]),
                    M::CreateRelationship(3, 2, 1, "R"),
                ]),
            ),
            (vec![5], K::Project(vec![(100, 0), (101, 3)])),
            (vec![6], K::Collect),
        ],
        expressions: vec![E::Slot(0), E::Slot(1), E::Slot(2), E::Property(0, "p")],
    };
    match mutate(&store, &null_endpoint, IMAGES) {
        (
            Err(NativeMutationError::Execution(NativeExecutionError::Stage(StageError::Endpoint))),
            false,
        ) => {}
        (Err(error), refused) => {
            panic!("expected a typed endpoint failure, got {error} (refused {refused})")
        }
        (Ok((_, report)), _) => panic!("a null endpoint committed {:?}", report.changed),
    }
    assert_eq!(store.generation(), before);

    let store = store.reopen();
    assert_eq!(sorted(read(&store, &scan_p())), pairs(&nodes, &[1, 2, 3]));
    assert_eq!(structured_node(&store, "after").get(), fence + 1);
    store.store.close().expect("close d3 store");
}
