use zeppelin_embed_adversarial_oracle::graph_directory::{
    Entity, Expected, Image, Key, Kind, Limits, Model, Operation, OperationKind, Outcome, Shape,
};

fn node(id: u128) -> Entity {
    Entity {
        kind: Kind::Node,
        id,
    }
}
fn key(namespace: &[u8], key: &[u8]) -> Key {
    Key {
        kind: Kind::Node,
        namespace: namespace.to_vec(),
        key: key.to_vec(),
    }
}
fn create(id: u128, key: Option<Key>, labels: &[u64], canonical: &[u8]) -> Operation {
    Operation {
        kind: if key.is_some() {
            OperationKind::Create
        } else {
            OperationKind::Cypher
        },
        key,
        expected: Expected::Absent,
        incarnation: node(id),
        revision: 1,
        delete_mode: None,
        image: Some(Image {
            canonical: canonical.to_vec(),
            shape: Shape::Node {
                labels: labels.to_vec(),
            },
        }),
    }
}

#[test]
fn graph_directory_preserves_full_ids_and_independent_kind_domains() {
    let mut model = Model::new(Limits::default());
    let high = (1_u128 << 100) | 7;
    assert!(matches!(
        model
            .apply(
                1,
                create(high, Some(key(b"n", b"high")), &[2, 9], &[0x80, 7])
            )
            .unwrap(),
        Outcome::Changed(_)
    ));
    model
        .apply(2, create(7, Some(key(b"n", b"low")), &[3], &[0, 7]))
        .unwrap();
    let snapshot = model.snapshot();
    assert_eq!(snapshot.lookup(node(7)).unwrap().image.canonical, [0, 7]);
    assert_eq!(
        snapshot.lookup(node(high)).unwrap().image.canonical,
        [0x80, 7]
    );
    assert_eq!(
        snapshot
            .node_range(0, None, 10)
            .unwrap()
            .iter()
            .map(|r| r.provenance.incarnation.id)
            .collect::<Vec<_>>(),
        [7, high]
    );
    assert!(snapshot.lookup(node(8)).is_none());
    assert_eq!(
        snapshot.observation().labels,
        [(2, high), (3, 7), (9, high)]
    );
}

#[test]
fn graph_directory_retains_complete_fences_replays_and_old_snapshots() {
    use zeppelin_embed_adversarial_oracle::graph_directory::{DeleteMode, Fence, Provenance};
    let mut model = Model::new(Limits::default());
    let id = (1_u128 << 120) | 9;
    let request = create(id, Some(key(b"", b"\0")), &[3], &[0, 0, 0, 0, 0, 0, 0, 128]);
    let installed = Provenance {
        version: 1,
        operation: OperationKind::Create,
        key: Some(key(b"", b"\0")),
        requested_revision: 1,
        installed_revision: 1,
        expected: Expected::Absent,
        incarnation: node(id),
        delete_mode: None,
        original_generation: 10,
    };
    assert_eq!(
        model.apply(10, request.clone()).unwrap(),
        Outcome::Changed(installed.clone())
    );
    let old = model.snapshot();
    assert_eq!(
        model.apply(99, request).unwrap(),
        Outcome::Replay(installed)
    );
    assert_eq!(model.snapshot().generation(), 10);
    let replacement = Operation {
        kind: OperationKind::Put,
        key: Some(key(b"", b"\0")),
        expected: Expected::Entity(node(id)),
        incarnation: node(id),
        revision: 3,
        delete_mode: None,
        image: Some(Image {
            canonical: vec![1, 0, 0, 0, 0, 0, 248, 127],
            shape: Shape::Node { labels: vec![9] },
        }),
    };
    model.apply(11, replacement).unwrap();
    let deletion = Operation {
        kind: OperationKind::Delete,
        key: Some(key(b"", b"\0")),
        expected: Expected::Entity(node(id)),
        incarnation: node(id),
        revision: 4,
        delete_mode: Some(DeleteMode::Detach),
        image: None,
    };
    let dead = Provenance {
        version: 1,
        operation: OperationKind::Delete,
        key: Some(key(b"", b"\0")),
        requested_revision: 4,
        installed_revision: 4,
        expected: Expected::Entity(node(id)),
        incarnation: node(id),
        delete_mode: Some(DeleteMode::Detach),
        original_generation: 12,
    };
    model.apply(12, deletion.clone()).unwrap();
    let deleted = model.snapshot();
    assert!(deleted.observation().nodes.is_empty());
    assert!(deleted.observation().labels.is_empty());
    assert_eq!(
        deleted.fence(&key(b"", b"\0")),
        Some(&Fence {
            provenance: dead.clone(),
            canonical: None
        })
    );
    assert_eq!(
        deleted.observation().node_tombstones.as_slice(),
        std::slice::from_ref(&dead)
    );
    assert_eq!(model.apply(13, deletion).unwrap(), Outcome::Replay(dead));
    let fresh = (1_u128 << 121) | 9;
    let recreation = Operation {
        kind: OperationKind::Recreate,
        key: Some(key(b"", b"\0")),
        expected: Expected::Deletion(4),
        incarnation: node(fresh),
        revision: 7,
        delete_mode: None,
        image: Some(Image {
            canonical: vec![0x55],
            shape: Shape::Node { labels: vec![2] },
        }),
    };
    model.apply(14, recreation.clone()).unwrap();
    assert!(matches!(
        model.apply(15, recreation).unwrap(),
        Outcome::Replay(_)
    ));
    let current = model.snapshot();
    assert!(current.lookup(node(id)).is_none());
    assert_eq!(current.lookup(node(fresh)).unwrap().image.canonical, [0x55]);
    assert_eq!(
        old.lookup(node(id)).unwrap().image.canonical,
        [0, 0, 0, 0, 0, 0, 0, 128]
    );
    assert_eq!(old.observation().labels, [(3, id)]);
    assert_eq!(deleted.generation(), 12);
    assert_eq!(current.generation(), 14);
}

fn relationship(id: u128, source: u128, target: u128, rel_type: u64) -> Operation {
    Operation {
        kind: OperationKind::Create,
        key: Some(Key {
            kind: Kind::Relationship,
            namespace: b"n".to_vec(),
            key: b"high".to_vec(),
        }),
        expected: Expected::Absent,
        incarnation: Entity {
            kind: Kind::Relationship,
            id,
        },
        revision: 1,
        delete_mode: None,
        image: Some(Image {
            canonical: vec![0xa0, 0xb1],
            shape: Shape::Relationship {
                source,
                target,
                rel_type,
            },
        }),
    }
}
fn delete_node(id: u128, key: Option<Key>, revision: u64, detach: bool) -> Operation {
    use zeppelin_embed_adversarial_oracle::graph_directory::DeleteMode;
    Operation {
        kind: if key.is_some() {
            OperationKind::Delete
        } else {
            OperationKind::Cypher
        },
        key,
        expected: Expected::Entity(node(id)),
        incarnation: node(id),
        revision,
        delete_mode: Some(if detach {
            DeleteMode::Detach
        } else {
            DeleteMode::Restrict
        }),
        image: None,
    }
}

#[test]
fn graph_directory_preserves_raw_incident_rows_but_validates_live_endpoints() {
    use zeppelin_embed_adversarial_oracle::graph_directory::{DeleteMode, Error};
    let mut model = Model::new(Limits::default());
    let high = (1_u128 << 100) | 7;
    model
        .apply(1, create(high, Some(key(b"n", b"high")), &[2], &[0x11]))
        .unwrap();
    model.apply(2, create(7, None, &[3], &[0x22])).unwrap();
    let before = model.snapshot().observation();
    assert_eq!(
        model.apply(3, relationship(high, high, 8, 42)),
        Err(Error::State("missing_endpoint"))
    );
    assert_eq!(model.snapshot().observation(), before);
    let edge = relationship(high, high, 7, 42);
    model.apply(3, edge.clone()).unwrap();
    let old = model.snapshot();
    assert_eq!(old.relationship_range(0, None, 4).unwrap().len(), 1);
    assert_eq!(old.observation().types, [(42, high)]);
    assert_eq!(old.observation().fences.len(), 2);
    assert_eq!(old.lookup(node(high)).unwrap().image.canonical, [0x11]);
    assert_eq!(
        old.lookup(edge.incarnation).unwrap().image.canonical,
        [0xa0, 0xb1]
    );
    let protected = model.snapshot().observation();
    assert_eq!(
        model.apply(4, delete_node(high, Some(key(b"n", b"high")), 2, false)),
        Err(Error::State("incident_relationship"))
    );
    assert_eq!(model.snapshot().observation(), protected);
    model
        .apply(4, delete_node(high, Some(key(b"n", b"high")), 2, true))
        .unwrap();
    assert!(model.snapshot().lookup(node(high)).is_none());
    assert!(model.snapshot().lookup(edge.incarnation).is_some());
    assert_eq!(model.snapshot().observation().types, [(42, high)]);
    assert_eq!(model.snapshot().observation().node_tombstones.len(), 1);
    // This raw edge has a dead source, so it cannot block plain deletion.
    model.apply(5, delete_node(7, None, 2, false)).unwrap();
    let mut delete_edge = edge;
    delete_edge.kind = OperationKind::Delete;
    delete_edge.expected = Expected::Entity(delete_edge.incarnation);
    delete_edge.revision = 2;
    delete_edge.delete_mode = Some(DeleteMode::Restrict);
    delete_edge.image = None;
    model.apply(6, delete_edge).unwrap();
    let empty = model.snapshot().observation();
    assert!(empty.nodes.is_empty() && empty.relationships.is_empty());
    assert!(empty.labels.is_empty() && empty.types.is_empty());
    assert_eq!(empty.fences.len(), 2);
    assert_eq!(empty.node_tombstones.len(), 2);
    assert_eq!(empty.node_tombstones[0].key, None);
    assert_eq!(empty.node_tombstones[0].original_generation, 5);
    assert_eq!(old.observation().nodes.len(), 2);
    assert_eq!(old.observation().types, [(42, high)]);
}

#[test]
fn graph_directory_key_ranges_are_byte_exact_and_capacity_bounded() {
    use zeppelin_embed_adversarial_oracle::graph_directory::Error;
    let mut model = Model::new(Limits::default());
    let cases = [
        (9, b"z".as_slice(), b"".as_slice()),
        (7, b"".as_slice(), b"\0".as_slice()),
        (4, b"".as_slice(), b"".as_slice()),
        (8, b"a".as_slice(), b"\0x".as_slice()),
        (6, b"\0".as_slice(), b"".as_slice()),
        (3, "é".as_bytes(), b"x".as_slice()),
        (2, "e\u{301}".as_bytes(), b"x".as_slice()),
    ];
    for (generation, (id, namespace, name)) in (1..).zip(cases) {
        model
            .apply(
                generation,
                create(id, Some(key(namespace, name)), &[5], &[id as u8]),
            )
            .unwrap();
    }
    let snapshot = model.snapshot();
    let ids = |rows: Vec<(
        &Key,
        &zeppelin_embed_adversarial_oracle::graph_directory::Fence,
    )>| {
        rows.iter()
            .map(|(_, fence)| fence.provenance.incarnation.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(snapshot.key_range(&key(b"", b""), None, 20).unwrap()),
        [4, 7, 6, 8, 2, 9, 3]
    );
    assert_eq!(
        ids(snapshot
            .key_range(&key(b"", b"\0"), Some(&key(b"z", b"")), 20)
            .unwrap()),
        [7, 6, 8, 2]
    );
    assert_eq!(
        ids(snapshot.key_range(&key(b"", b""), None, 2).unwrap()),
        [4, 7]
    );
    assert!(
        snapshot
            .key_range(&key(b"", b""), None, 0)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        snapshot.key_range(&key(b"z", b""), Some(&key(b"", b"")), 1),
        Err(Error::Invalid("range"))
    );
    assert_eq!(snapshot.label_members(5, 4, Some(9), 3).unwrap(), [4, 6, 7]);
    assert_eq!(
        snapshot.node_range(9, Some(4), 1),
        Err(Error::Invalid("range"))
    );
    assert_eq!(
        snapshot.node_range(0, None, Limits::default().max_entities + 1),
        Err(Error::Limit("query_capacity"))
    );
    assert!(snapshot.fence(&key(b"a", b"x")).is_none());
}

#[test]
fn graph_directory_snapshot_reads_filter_both_endpoints_before_capacity() {
    use zeppelin_embed_adversarial_oracle::graph_directory::{DeleteMode, Error};
    let mut model = Model::new(Limits::default());
    for id in 1..=3 {
        model
            .apply(id as u64, create(id, None, &[5], &[id as u8]))
            .unwrap();
    }
    let mut edges = Vec::new();
    for (id, source, target) in [(4, 1, 3), (5, 3, 2), (6, 3, 3), (7, 1, 1)] {
        let mut edge = relationship(id, source, target, 42);
        edge.kind = OperationKind::Cypher;
        edge.key = None;
        model.apply(id as u64, edge.clone()).unwrap();
        edges.push(edge);
    }
    let old = model.snapshot();
    model.apply(8, delete_node(1, None, 2, true)).unwrap();
    model.apply(9, delete_node(2, None, 2, true)).unwrap();
    let deleted = model.snapshot();
    assert_eq!(deleted.type_members(42, 0, None, 10).unwrap(), [4, 5, 6, 7]);
    assert_eq!(
        deleted
            .observable_relationships(0, None, 1)
            .unwrap()
            .iter()
            .map(|r| r.provenance.incarnation.id)
            .collect::<Vec<_>>(),
        [6]
    );
    assert_eq!(old.observable_relationships(0, None, 10).unwrap().len(), 4);
    assert_eq!(
        model.drop_node_tombstone(10, 1),
        Err(Error::State("retained_incident_relationship"))
    );
    for (generation, mut edge) in [10, 11, 12]
        .into_iter()
        .zip(edges.into_iter().filter(|e| e.incarnation.id != 6))
    {
        edge.expected = Expected::Entity(edge.incarnation);
        edge.revision = 2;
        edge.delete_mode = Some(DeleteMode::Restrict);
        edge.image = None;
        model.apply(generation, edge).unwrap();
    }
    model.drop_node_tombstone(13, 1).unwrap();
    model.drop_node_tombstone(14, 2).unwrap();
    assert!(model.snapshot().observation().node_tombstones.is_empty());
    assert_eq!(deleted.observation().node_tombstones.len(), 2);
    assert_eq!(
        model.apply(15, create(1, None, &[], &[0])),
        Err(Error::State("reused_identity"))
    );
    let before = model.snapshot().observation();
    model.relocate(15).unwrap();
    let mut after = model.snapshot().observation();
    assert_eq!(after.generation, 15);
    after.generation = before.generation;
    assert_eq!(after, before);
}

fn comparator_fixture() -> (
    Model,
    zeppelin_embed_adversarial_oracle::graph_directory::Observation,
) {
    use zeppelin_embed_adversarial_oracle::graph_directory::{
        DeleteMode, Fence, Observation, Provenance, Record,
    };
    let high = (1_u128 << 100) | 7;
    let mut model = Model::new(Limits::default());
    model
        .apply(
            1,
            create(
                7,
                Some(key(b"n", b"low")),
                &[3],
                &[1, 0, 0, 0, 0, 0, 248, 127],
            ),
        )
        .unwrap();
    model
        .apply(
            2,
            create(
                high,
                Some(key(b"n", b"high")),
                &[2, 9],
                &[0, 0, 0, 0, 0, 0, 0, 128],
            ),
        )
        .unwrap();
    model.apply(3, relationship(7, 7, high, 42)).unwrap();
    model
        .apply(4, create(9, Some(key(b"", b"")), &[], &[0xff]))
        .unwrap();
    model
        .apply(5, delete_node(9, Some(key(b"", b"")), 2, true))
        .unwrap();
    let low = Provenance {
        version: 1,
        operation: OperationKind::Create,
        key: Some(key(b"n", b"low")),
        requested_revision: 1,
        installed_revision: 1,
        expected: Expected::Absent,
        incarnation: node(7),
        delete_mode: None,
        original_generation: 1,
    };
    let upper = Provenance {
        version: 1,
        operation: OperationKind::Create,
        key: Some(key(b"n", b"high")),
        requested_revision: 1,
        installed_revision: 1,
        expected: Expected::Absent,
        incarnation: node(high),
        delete_mode: None,
        original_generation: 2,
    };
    let edge = Provenance {
        version: 1,
        operation: OperationKind::Create,
        key: Some(Key {
            kind: Kind::Relationship,
            namespace: b"n".to_vec(),
            key: b"high".to_vec(),
        }),
        requested_revision: 1,
        installed_revision: 1,
        expected: Expected::Absent,
        incarnation: Entity {
            kind: Kind::Relationship,
            id: 7,
        },
        delete_mode: None,
        original_generation: 3,
    };
    let dead = Provenance {
        version: 1,
        operation: OperationKind::Delete,
        key: Some(key(b"", b"")),
        requested_revision: 2,
        installed_revision: 2,
        expected: Expected::Entity(node(9)),
        incarnation: node(9),
        delete_mode: Some(DeleteMode::Detach),
        original_generation: 5,
    };
    let expected = Observation {
        generation: 5,
        nodes: vec![
            Record {
                image: Image {
                    canonical: vec![1, 0, 0, 0, 0, 0, 248, 127],
                    shape: Shape::Node { labels: vec![3] },
                },
                provenance: low.clone(),
            },
            Record {
                image: Image {
                    canonical: vec![0, 0, 0, 0, 0, 0, 0, 128],
                    shape: Shape::Node { labels: vec![2, 9] },
                },
                provenance: upper.clone(),
            },
        ],
        relationships: vec![Record {
            image: Image {
                canonical: vec![0xa0, 0xb1],
                shape: Shape::Relationship {
                    source: 7,
                    target: high,
                    rel_type: 42,
                },
            },
            provenance: edge.clone(),
        }],
        fences: vec![
            Fence {
                provenance: dead.clone(),
                canonical: None,
            },
            Fence {
                provenance: upper,
                canonical: Some(vec![0, 0, 0, 0, 0, 0, 0, 128]),
            },
            Fence {
                provenance: low,
                canonical: Some(vec![1, 0, 0, 0, 0, 0, 248, 127]),
            },
            Fence {
                provenance: edge,
                canonical: Some(vec![0xa0, 0xb1]),
            },
        ],
        labels: vec![(2, high), (3, 7), (9, high)],
        types: vec![(42, 7)],
        node_tombstones: vec![dead],
    };
    (model, expected)
}

#[test]
fn graph_directory_comparator_accepts_literal_complete_observation() {
    let (model, literal) = comparator_fixture();
    assert_eq!(model.snapshot().check(&literal), Ok(()));
    assert_eq!(model.snapshot().observation(), literal);
}

#[test]
fn graph_directory_comparator_detects_narrowed_ids_and_missing_dead_fences() {
    let (model, literal) = comparator_fixture();
    let expected = model.snapshot();
    let mut narrowed = literal.clone();
    narrowed.nodes[1].provenance.incarnation.id = 7;
    assert_eq!(
        expected.check(&narrowed).unwrap_err().path,
        "nodes[1].provenance.incarnation"
    );
    let mut missing = literal;
    missing.fences.remove(0);
    assert_eq!(expected.check(&missing).unwrap_err().path, "fences.length");
}

#[test]
fn graph_directory_comparator_detects_omitted_extra_and_reordered_membership() {
    let (model, literal) = comparator_fixture();
    let expected = model.snapshot();
    let mut omitted = literal.clone();
    omitted.labels.remove(0);
    assert_eq!(expected.check(&omitted).unwrap_err().path, "labels.length");
    let mut extra = literal.clone();
    extra.labels.push((5, 9));
    assert_eq!(expected.check(&extra).unwrap_err().path, "labels.length");
    let mut missing_type = literal.clone();
    missing_type.types.clear();
    assert_eq!(
        expected.check(&missing_type).unwrap_err().path,
        "types.length"
    );
    let mut extra_type = literal.clone();
    extra_type.types.push((42, 9));
    assert_eq!(
        expected.check(&extra_type).unwrap_err().path,
        "types.length"
    );
    let mut reordered = literal;
    reordered.labels.swap(0, 1);
    assert_eq!(expected.check(&reordered).unwrap_err().path, "labels[0]");
}

#[test]
fn graph_directory_comparator_detects_canonical_bits_and_every_provenance_field() {
    use zeppelin_embed_adversarial_oracle::graph_directory::DeleteMode;
    let (model, literal) = comparator_fixture();
    let expected = model.snapshot();
    let mut changed = literal.clone();
    changed.nodes[0].image.canonical[0] = 2;
    assert_eq!(
        expected.check(&changed).unwrap_err().path,
        "nodes[0].canonical[0]"
    );
    let mut changed = literal.clone();
    changed.fences[1].canonical.as_mut().unwrap()[7] = 0;
    assert_eq!(
        expected.check(&changed).unwrap_err().path,
        "fences[1].canonical[7]"
    );
    let changes = [
        "version",
        "operation",
        "key.kind",
        "key.namespace.length",
        "key.key.length",
        "requested_revision",
        "installed_revision",
        "expected",
        "incarnation",
        "delete_mode",
        "original_generation",
    ];
    for field in changes {
        let mut observed = literal.clone();
        let p = &mut observed.fences[0].provenance;
        match field {
            "version" => p.version = 2,
            "operation" => p.operation = OperationKind::Cypher,
            "key.kind" => p.key.as_mut().unwrap().kind = Kind::Relationship,
            "key.namespace.length" => p.key.as_mut().unwrap().namespace.push(0),
            "key.key.length" => p.key.as_mut().unwrap().key.push(0),
            "requested_revision" => p.requested_revision = 3,
            "installed_revision" => p.installed_revision = 3,
            "expected" => p.expected = Expected::Absent,
            "incarnation" => p.incarnation.id = (1_u128 << 100) | 9,
            "delete_mode" => p.delete_mode = Some(DeleteMode::Restrict),
            "original_generation" => p.original_generation = 4,
            _ => unreachable!(),
        }
        assert_eq!(
            expected.check(&observed).unwrap_err().path,
            format!("fences[0].provenance.{field}"),
            "CAN FIRE {field}"
        );
    }
}

#[test]
fn graph_directory_comparator_rejects_wrong_old_root_and_inventory_as_liveness() {
    let (mut model, literal) = comparator_fixture();
    let old = model.snapshot();
    let high = (1_u128 << 100) | 7;
    model
        .apply(
            6,
            Operation {
                kind: OperationKind::Put,
                key: Some(key(b"n", b"high")),
                expected: Expected::Entity(node(high)),
                incarnation: node(high),
                revision: 2,
                delete_mode: None,
                image: Some(Image {
                    canonical: vec![0x22],
                    shape: Shape::Node { labels: vec![6] },
                }),
            },
        )
        .unwrap();
    let mut wrong_old_root = model.snapshot().observation();
    wrong_old_root.generation = 5;
    assert_eq!(
        old.check(&wrong_old_root).unwrap_err().path,
        "nodes[1].provenance.operation"
    );
    assert!(old.check(&literal).is_ok());
    let mut phantom_from_inventory = literal.clone();
    let mut phantom = phantom_from_inventory.nodes[0].clone();
    phantom.provenance.incarnation.id = 1000;
    phantom_from_inventory.nodes.push(phantom);
    assert_eq!(
        old.check(&phantom_from_inventory).unwrap_err().path,
        "nodes.length"
    );
    let mut missing_tombstone = literal;
    missing_tombstone.node_tombstones.clear();
    assert_eq!(
        old.check(&missing_tombstone).unwrap_err().path,
        "node_tombstones.length"
    );
}

#[test]
fn graph_directory_numeric_ranges_reach_maximum_id_without_byte_ordering() {
    let mut model = Model::new(Limits::default());
    for (generation, id) in (1..).zip([u128::MAX, 256, 255, 65536]) {
        model
            .apply(generation, create(id, None, &[u64::MAX], &[0x1f]))
            .unwrap();
    }
    let snapshot = model.snapshot();
    let ids = |rows: Vec<&zeppelin_embed_adversarial_oracle::graph_directory::Record>| {
        rows.iter()
            .map(|r| r.provenance.incarnation.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(snapshot.node_range(0, None, 4).unwrap()),
        [255, 256, 65536, u128::MAX]
    );
    assert_eq!(
        ids(snapshot.node_range(256, Some(u128::MAX), 4).unwrap()),
        [256, 65536]
    );
    assert_eq!(
        ids(snapshot.node_range(u128::MAX, None, 4).unwrap()),
        [u128::MAX]
    );
    assert!(snapshot.node_range(256, Some(256), 4).unwrap().is_empty());
    assert_eq!(
        snapshot.label_members(u64::MAX, 256, None, 4).unwrap(),
        [256, 65536, u128::MAX]
    );
}

#[test]
fn graph_directory_rejects_limits_without_partial_logical_changes() {
    use zeppelin_embed_adversarial_oracle::graph_directory::Error;
    for (field, error) in [
        ("steps", Error::Limit("steps")),
        ("entities", Error::Limit("entities")),
        ("fences", Error::Limit("fences")),
        ("key_bytes", Error::Limit("key_bytes")),
        ("image_bytes", Error::Limit("image_bytes")),
        ("labels", Error::Limit("labels")),
    ] {
        let mut limits = Limits::default();
        match field {
            "steps" => limits.max_steps = 0,
            "entities" => limits.max_entities = 0,
            "fences" => limits.max_fences = 0,
            "key_bytes" => limits.max_key_bytes = 0,
            "image_bytes" => limits.max_image_bytes = 0,
            "labels" => limits.max_labels_per_node = 0,
            _ => unreachable!(),
        }
        let mut model = Model::new(limits);
        let before = model.snapshot().observation();
        assert_eq!(
            model.apply(1, create(1, Some(key(b"n", b"x")), &[1], &[1])),
            Err(error),
            "{field}"
        );
        assert_eq!(
            model.snapshot().observation(),
            before,
            "atomic refusal {field}"
        );
    }
    let mut model = Model::new(Limits {
        max_steps: 1,
        ..Limits::default()
    });
    model.apply(1, create(1, None, &[], &[1])).unwrap();
    let before = model.snapshot().observation();
    assert_eq!(model.relocate(2), Err(Error::Limit("steps")));
    assert_eq!(model.drop_node_tombstone(2, 1), Err(Error::Limit("steps")));
    assert_eq!(model.snapshot().observation(), before);
}

#[test]
fn graph_directory_rejects_invalid_requests_topology_and_bitwise_replay_changes() {
    use zeppelin_embed_adversarial_oracle::graph_directory::Error;
    let mut model = Model::new(Limits::default());
    for (operation, reason) in [
        (create(0, None, &[], &[1]), "identity_or_revision"),
        (create(1, None, &[0], &[1]), "node_shape"),
        (create(1, None, &[2, 1], &[1]), "node_shape"),
        (create(1, None, &[1, 1], &[1]), "node_shape"),
        (create(1, Some(key(&[0xff], b"x")), &[], &[1]), "key"),
    ] {
        assert_eq!(model.apply(1, operation), Err(Error::Invalid(reason)));
        assert!(model.snapshot().observation().nodes.is_empty());
    }
    let request = create(1, Some(key(b"", b"zero")), &[], &[0, 0, 0, 0, 0, 0, 0, 128]);
    model.apply(1, request.clone()).unwrap();
    let before = model.snapshot().observation();
    let mut plus_zero = request;
    plus_zero.image.as_mut().unwrap().canonical[7] = 0;
    assert_eq!(
        model.apply(2, plus_zero),
        Err(Error::State("replay_conflict"))
    );
    assert_eq!(model.snapshot().observation(), before);
    model.apply(2, create(2, None, &[], &[2])).unwrap();
    model.apply(3, relationship(9, 1, 2, 42)).unwrap();
    let mut moved = relationship(9, 2, 1, 42);
    moved.kind = OperationKind::Put;
    moved.expected = Expected::Entity(moved.incarnation);
    moved.revision = 2;
    let before = model.snapshot().observation();
    assert_eq!(
        model.apply(4, moved),
        Err(Error::State("relationship_topology"))
    );
    assert_eq!(model.snapshot().observation(), before);
}

#[test]
fn graph_directory_unkeyed_tombstone_comparator_checks_complete_deletion_provenance() {
    use zeppelin_embed_adversarial_oracle::graph_directory::{DeleteMode, Observation, Provenance};
    let mut model = Model::new(Limits::default());
    model.apply(1, create(99, None, &[], &[0])).unwrap();
    model.apply(2, delete_node(99, None, 2, true)).unwrap();
    let mut literal = Observation {
        generation: 2,
        nodes: vec![],
        relationships: vec![],
        fences: vec![],
        labels: vec![],
        types: vec![],
        node_tombstones: vec![Provenance {
            version: 1,
            operation: OperationKind::Cypher,
            key: None,
            requested_revision: 2,
            installed_revision: 2,
            expected: Expected::Entity(node(99)),
            incarnation: node(99),
            delete_mode: Some(DeleteMode::Detach),
            original_generation: 2,
        }],
    };
    assert!(model.snapshot().check(&literal).is_ok());
    literal.node_tombstones[0].delete_mode = Some(DeleteMode::Restrict);
    assert_eq!(
        model.snapshot().check(&literal).unwrap_err().path,
        "node_tombstones[0].delete_mode"
    );
    literal.node_tombstones[0].delete_mode = Some(DeleteMode::Detach);
    literal.node_tombstones[0].original_generation = 1;
    assert_eq!(
        model.snapshot().check(&literal).unwrap_err().path,
        "node_tombstones[0].original_generation"
    );
}

#[test]
fn graph_directory_cypher_noop_preserves_max_revision_without_increment() {
    use zeppelin_embed_adversarial_oracle::graph_directory::Error;
    let mut model = Model::new(Limits::default());
    let mut initial = create(7, Some(key(b"n", b"max")), &[3], &[0x80]);
    initial.revision = u64::MAX;
    model.apply(1, initial.clone()).unwrap();
    let before = model.snapshot().observation();
    let mut cypher = initial;
    cypher.kind = OperationKind::Cypher;
    cypher.expected = Expected::Entity(node(7));
    assert_eq!(model.apply(2, cypher.clone()), Ok(Outcome::NoOp));
    assert_eq!(model.snapshot().observation(), before);
    cypher.image.as_mut().unwrap().canonical[0] = 0;
    assert_eq!(
        model.apply(2, cypher),
        Err(Error::State("revision_overflow"))
    );
    assert_eq!(model.snapshot().observation(), before);
}
