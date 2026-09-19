use zeppelin_embed_adversarial_oracle::graph_adjacency_store::{
    AdjacencyRange, AdjacencyRow, DegreeQuery, Direction, EntityKind, Model, ObservationPlan,
    Operation, RelationshipRange, RelationshipRow,
};

#[test]
fn same_batch_new_endpoints_create_exact_raw_and_visible_rows() {
    let source = (1_u128 << 100) | 7;
    let target = u128::MAX;
    let rel = (1_u128 << 127) | 9;
    let mut model = Model::new();
    model
        .apply(
            3,
            &[
                Operation::CreateNode { id: source },
                Operation::CreateNode { id: target },
                Operation::CreateRelationship {
                    rel,
                    source,
                    target,
                    relationship_type: u64::MAX,
                },
            ],
        )
        .unwrap();

    let observation = model
        .snapshot()
        .observation(&ObservationPlan::default())
        .unwrap();
    let relationship = RelationshipRow {
        rel,
        source,
        target,
        relationship_type: u64::MAX,
    };
    assert_eq!(observation.raw_relationships, [relationship]);
    assert_eq!(observation.visible_relationships, [relationship]);
    assert_eq!(
        observation.raw_outgoing,
        [AdjacencyRow {
            bound_node: source,
            relationship_type: u64::MAX,
            rel,
            neighbor: target,
        }]
    );
    assert_eq!(observation.raw_outgoing, observation.visible_outgoing);
    assert_eq!(
        observation.raw_incoming,
        [AdjacencyRow {
            bound_node: target,
            relationship_type: u64::MAX,
            rel,
            neighbor: source,
        }]
    );
    assert_eq!(observation.raw_incoming, observation.visible_incoming);
}

#[test]
fn restrict_delete_requires_every_live_self_and_parallel_edge_to_be_deleted() {
    let a = (1_u128 << 100) | 1;
    let b = (1_u128 << 100) | 2;
    let self_loop = (1_u128 << 120) | 1;
    let parallel_one = (1_u128 << 120) | 2;
    let parallel_two = (1_u128 << 120) | 3;
    let mut model = Model::new();
    model
        .apply(
            1,
            &[
                Operation::CreateNode { id: a },
                Operation::CreateNode { id: b },
                Operation::CreateRelationship {
                    rel: self_loop,
                    source: a,
                    target: a,
                    relationship_type: 7,
                },
                Operation::CreateRelationship {
                    rel: parallel_one,
                    source: a,
                    target: b,
                    relationship_type: 7,
                },
                Operation::CreateRelationship {
                    rel: parallel_two,
                    source: a,
                    target: b,
                    relationship_type: 7,
                },
            ],
        )
        .unwrap();
    let before = model
        .snapshot()
        .observation(&ObservationPlan::default())
        .unwrap();

    assert_eq!(
        model.apply(
            2,
            &[
                Operation::DeleteNode {
                    id: a,
                    detach: false,
                },
                Operation::DeleteNode {
                    id: b,
                    detach: false,
                },
            ],
        ),
        Err(
            zeppelin_embed_adversarial_oracle::graph_adjacency_store::Error::State(
                "incident_relationship"
            )
        )
    );
    assert_eq!(
        model
            .snapshot()
            .observation(&ObservationPlan::default())
            .unwrap(),
        before
    );
    assert!(
        model
            .apply(
                2,
                &[
                    Operation::DeleteRelationship { rel: self_loop },
                    Operation::DeleteNode {
                        id: a,
                        detach: false,
                    },
                ],
            )
            .is_err()
    );

    model
        .apply(
            2,
            &[
                Operation::DeleteRelationship { rel: self_loop },
                Operation::DeleteRelationship { rel: parallel_one },
                Operation::DeleteRelationship { rel: parallel_two },
                Operation::DeleteNode {
                    id: a,
                    detach: false,
                },
            ],
        )
        .unwrap();
    let after = model
        .snapshot()
        .observation(&ObservationPlan::default())
        .unwrap();
    assert!(after.raw_relationships.is_empty());
    assert!(!after.nodes[0].live);
    assert!(after.nodes[1].live);
}

#[test]
fn detach_keeps_high_degree_raw_rows_and_property_edit_uses_admitted_visibility() {
    let hub = (1_u128 << 112) | 1;
    let first_leaf = (1_u128 << 96) | 10;
    let first_rel = (1_u128 << 120) | 10;
    let degree = 4097_u128;
    let mut operations = Vec::new();
    operations.push(Operation::CreateNode { id: hub });
    for offset in 0..degree {
        operations.push(Operation::CreateNode {
            id: first_leaf + offset,
        });
        operations.push(Operation::CreateRelationship {
            rel: first_rel + offset,
            source: hub,
            target: first_leaf + offset,
            relationship_type: 9,
        });
    }
    let mut model = Model::new();
    model.apply(10, &operations).unwrap();
    model
        .apply(
            99,
            &[
                Operation::PropertyOnly {
                    entity_kind: EntityKind::Relationship,
                    id: first_rel,
                },
                Operation::DeleteNode {
                    id: hub,
                    detach: true,
                },
            ],
        )
        .unwrap();

    let detached = model
        .snapshot()
        .observation(&ObservationPlan::default())
        .unwrap();
    assert_eq!(detached.generation, 99);
    assert_eq!(detached.raw_relationships.len(), degree as usize);
    assert_eq!(detached.raw_outgoing.len(), degree as usize);
    assert_eq!(detached.raw_incoming.len(), degree as usize);
    assert!(detached.visible_relationships.is_empty());
    assert!(detached.visible_outgoing.is_empty());
    assert!(detached.visible_incoming.is_empty());

    model
        .apply(100, &[Operation::DeleteRelationship { rel: first_rel }])
        .unwrap();
    model
        .apply(
            101,
            &[Operation::DeleteNode {
                id: first_leaf + 1,
                detach: false,
            }],
        )
        .unwrap();
    assert_eq!(
        model
            .snapshot()
            .observation(&ObservationPlan::default())
            .unwrap()
            .raw_relationships
            .len(),
        degree as usize - 1
    );
}

#[test]
fn visible_ranges_filter_both_endpoints_before_capacity_and_count_degrees() {
    let a = (1_u128 << 96) | 1;
    let b = (1_u128 << 96) | 2;
    let c = (1_u128 << 96) | 3;
    let d = (1_u128 << 96) | 4;
    let first_rel = (1_u128 << 112) | 1;
    let visible_rel = first_rel + 2;
    let mut model = Model::new();
    model
        .apply(
            1,
            &[
                Operation::CreateNode { id: a },
                Operation::CreateNode { id: b },
                Operation::CreateNode { id: c },
                Operation::CreateNode { id: d },
                Operation::CreateRelationship {
                    rel: first_rel,
                    source: a,
                    target: b,
                    relationship_type: 7,
                },
                Operation::CreateRelationship {
                    rel: first_rel + 1,
                    source: a,
                    target: c,
                    relationship_type: 7,
                },
                Operation::CreateRelationship {
                    rel: visible_rel,
                    source: a,
                    target: d,
                    relationship_type: 7,
                },
            ],
        )
        .unwrap();
    model
        .apply(
            2,
            &[
                Operation::DeleteNode {
                    id: b,
                    detach: true,
                },
                Operation::DeleteNode {
                    id: c,
                    detach: true,
                },
            ],
        )
        .unwrap();
    let plan = ObservationPlan {
        relationship_ranges: vec![RelationshipRange {
            start: first_rel,
            end: None,
            capacity: 1,
        }],
        adjacency_ranges: vec![AdjacencyRange {
            direction: Direction::Outgoing,
            start: AdjacencyRow {
                bound_node: a,
                relationship_type: 0,
                rel: 0,
                neighbor: 0,
            },
            end: Some(AdjacencyRow {
                bound_node: a + 1,
                relationship_type: 0,
                rel: 0,
                neighbor: 0,
            }),
            capacity: 1,
        }],
        degrees: vec![
            DegreeQuery {
                node: a,
                direction: Direction::Outgoing,
                relationship_type: Some(7),
            },
            DegreeQuery {
                node: d,
                direction: Direction::Incoming,
                relationship_type: None,
            },
        ],
    };
    let observation = model.snapshot().observation(&plan).unwrap();
    let relationship = RelationshipRow {
        rel: visible_rel,
        source: a,
        target: d,
        relationship_type: 7,
    };
    assert_eq!(observation.visible_relationship_count, 1);
    assert_eq!(observation.visible_relationships, [relationship]);
    assert_eq!(observation.relationship_ranges[0].rows, [relationship]);
    assert_eq!(observation.adjacency_ranges[0].rows[0].rel, visible_rel);
    assert_eq!(observation.degrees[0].degree, 1);
    assert_eq!(observation.degrees[1].degree, 1);
}

#[test]
fn exact_comparator_detects_direction_identity_topology_type_and_multiplicity() {
    let a = (1_u128 << 104) | 1;
    let b = (1_u128 << 104) | 2;
    let first_rel = (1_u128 << 120) | 1;
    let mut model = Model::new();
    model
        .apply(
            1,
            &[
                Operation::CreateNode { id: a },
                Operation::CreateNode { id: b },
                Operation::CreateRelationship {
                    rel: first_rel,
                    source: a,
                    target: a,
                    relationship_type: 11,
                },
                Operation::CreateRelationship {
                    rel: first_rel + 1,
                    source: a,
                    target: b,
                    relationship_type: 11,
                },
                Operation::CreateRelationship {
                    rel: first_rel + 2,
                    source: a,
                    target: b,
                    relationship_type: 11,
                },
            ],
        )
        .unwrap();
    let snapshot = model.snapshot();
    let plan = ObservationPlan::default();
    let observed = snapshot.observation(&plan).unwrap();
    snapshot.check(&plan, &observed).unwrap();

    let mut missing_reverse = observed.clone();
    missing_reverse.raw_incoming.pop();
    assert_eq!(
        snapshot.check(&plan, &missing_reverse).unwrap_err().path,
        "raw_incoming.length"
    );

    let mut narrowed = observed.clone();
    narrowed.raw_relationships[0].rel &= u64::MAX as u128;
    assert_eq!(
        snapshot.check(&plan, &narrowed).unwrap_err().path,
        "raw_relationships[0].rel"
    );

    let mut topology = observed.clone();
    topology.raw_relationships[0].source = b;
    assert_eq!(
        snapshot.check(&plan, &topology).unwrap_err().path,
        "raw_relationships[0].source"
    );

    let mut relationship_type = observed.clone();
    relationship_type.raw_relationships[0].relationship_type = 12;
    assert_eq!(
        snapshot.check(&plan, &relationship_type).unwrap_err().path,
        "raw_relationships[0].relationship_type"
    );

    let mut missing_parallel = observed.clone();
    missing_parallel.visible_relationships.pop();
    assert_eq!(
        snapshot.check(&plan, &missing_parallel).unwrap_err().path,
        "visible_relationships.length"
    );

    let mut missing_self_loop = observed.clone();
    missing_self_loop.visible_relationships.remove(0);
    assert_eq!(
        snapshot.check(&plan, &missing_self_loop).unwrap_err().path,
        "visible_relationships.length"
    );

    let mut reordered = observed;
    reordered.raw_relationships.swap(0, 1);
    assert_eq!(
        snapshot.check(&plan, &reordered).unwrap_err().path,
        "raw_relationships[0].rel"
    );
}

#[test]
fn exact_comparator_detects_ignored_delete_late_liveness_and_old_view_changes() {
    let a = (1_u128 << 92) | 1;
    let b = (1_u128 << 92) | 2;
    let c = (1_u128 << 92) | 3;
    let first_rel = (1_u128 << 118) | 1;
    let second_rel = first_rel + 1;
    let mut model = Model::new();
    model
        .apply(
            1,
            &[
                Operation::CreateNode { id: a },
                Operation::CreateNode { id: b },
                Operation::CreateNode { id: c },
                Operation::CreateRelationship {
                    rel: first_rel,
                    source: a,
                    target: b,
                    relationship_type: 5,
                },
                Operation::CreateRelationship {
                    rel: second_rel,
                    source: a,
                    target: c,
                    relationship_type: 5,
                },
            ],
        )
        .unwrap();
    let plan = ObservationPlan {
        relationship_ranges: vec![RelationshipRange {
            start: first_rel,
            end: None,
            capacity: 1,
        }],
        adjacency_ranges: vec![AdjacencyRange {
            direction: Direction::Outgoing,
            start: AdjacencyRow {
                bound_node: a,
                relationship_type: 0,
                rel: 0,
                neighbor: 0,
            },
            end: Some(AdjacencyRow {
                bound_node: a + 1,
                relationship_type: 0,
                rel: 0,
                neighbor: 0,
            }),
            capacity: 1,
        }],
        degrees: vec![DegreeQuery {
            node: a,
            direction: Direction::Outgoing,
            relationship_type: Some(5),
        }],
    };
    let old = model.snapshot();
    let old_observation = old.observation(&plan).unwrap();
    model
        .apply(
            2,
            &[Operation::DeleteNode {
                id: b,
                detach: true,
            }],
        )
        .unwrap();
    let detached = model.snapshot();
    let observed = detached.observation(&plan).unwrap();
    assert_eq!(observed.relationship_ranges[0].rows[0].rel, second_rel);
    assert_eq!(observed.adjacency_ranges[0].rows[0].rel, second_rel);
    assert_eq!(observed.degrees[0].degree, 1);

    let mut filtered_after_capacity = observed.clone();
    filtered_after_capacity.relationship_ranges[0].rows[0] = RelationshipRow {
        rel: first_rel,
        source: a,
        target: b,
        relationship_type: 5,
    };
    filtered_after_capacity.adjacency_ranges[0].rows[0] = AdjacencyRow {
        bound_node: a,
        relationship_type: 5,
        rel: first_rel,
        neighbor: b,
    };
    assert_eq!(
        detached
            .check(&plan, &filtered_after_capacity)
            .unwrap_err()
            .path,
        "relationship_ranges[0].rows[0].rel"
    );

    model
        .apply(3, &[Operation::DeleteRelationship { rel: first_rel }])
        .unwrap();
    let after_delete = model.snapshot();
    let after_delete_observation = after_delete.observation(&plan).unwrap();
    let mut ignored_delete = observed;
    ignored_delete.generation = 3;
    assert_eq!(
        after_delete.check(&plan, &ignored_delete).unwrap_err().path,
        "raw_relationships.length"
    );
    let mut ignored_out_delete = after_delete_observation.clone();
    ignored_out_delete.raw_outgoing.insert(
        0,
        AdjacencyRow {
            bound_node: a,
            relationship_type: 5,
            rel: first_rel,
            neighbor: b,
        },
    );
    assert_eq!(
        after_delete
            .check(&plan, &ignored_out_delete)
            .unwrap_err()
            .path,
        "raw_outgoing.length"
    );
    let mut ignored_in_delete = after_delete_observation;
    ignored_in_delete.raw_incoming.insert(
        0,
        AdjacencyRow {
            bound_node: b,
            relationship_type: 5,
            rel: first_rel,
            neighbor: a,
        },
    );
    assert_eq!(
        after_delete
            .check(&plan, &ignored_in_delete)
            .unwrap_err()
            .path,
        "raw_incoming.length"
    );

    model
        .apply(
            4,
            &[
                Operation::DeleteNode {
                    id: a,
                    detach: true,
                },
                Operation::DeleteNode {
                    id: c,
                    detach: true,
                },
            ],
        )
        .unwrap();
    let all_deleted = model.snapshot().observation(&plan).unwrap();
    assert!(all_deleted.nodes.iter().all(|node| !node.live));
    assert_eq!(all_deleted.raw_relationships.len(), 1);
    assert!(all_deleted.visible_relationships.is_empty());
    assert!(all_deleted.relationship_ranges[0].rows.is_empty());
    assert_eq!(all_deleted.visible_relationship_count, 0);
    assert_eq!(all_deleted.degrees[0].degree, 0);

    assert_eq!(old.observation(&plan).unwrap(), old_observation);
    old.check(&plan, &old_observation).unwrap();
    assert_eq!(
        old.check(&plan, &all_deleted).unwrap_err().path,
        "generation"
    );
}

#[test]
fn fresh_relationship_requires_final_live_endpoints_but_property_only_preserves_topology() {
    let source = (1_u128 << 110) | 1;
    let target = (1_u128 << 110) | 2;
    let rel = (1_u128 << 125) | 1;
    let mut model = Model::new();
    model
        .apply(
            1,
            &[
                Operation::CreateNode { id: source },
                Operation::CreateNode { id: target },
            ],
        )
        .unwrap();
    let before = model
        .snapshot()
        .observation(&ObservationPlan::default())
        .unwrap();
    assert!(
        model
            .apply(
                2,
                &[
                    Operation::CreateRelationship {
                        rel,
                        source,
                        target,
                        relationship_type: 3,
                    },
                    Operation::DeleteNode {
                        id: target,
                        detach: true,
                    },
                ],
            )
            .is_err()
    );
    assert_eq!(
        model
            .snapshot()
            .observation(&ObservationPlan::default())
            .unwrap(),
        before
    );

    model
        .apply(
            2,
            &[Operation::CreateRelationship {
                rel,
                source,
                target,
                relationship_type: 3,
            }],
        )
        .unwrap();
    let topology = model
        .snapshot()
        .observation(&ObservationPlan::default())
        .unwrap();
    model
        .apply(
            9,
            &[
                Operation::PropertyOnly {
                    entity_kind: EntityKind::Node,
                    id: source,
                },
                Operation::PropertyOnly {
                    entity_kind: EntityKind::Relationship,
                    id: rel,
                },
            ],
        )
        .unwrap();
    let after = model
        .snapshot()
        .observation(&ObservationPlan::default())
        .unwrap();
    assert_eq!(after.generation, 9);
    assert_eq!(after.raw_relationships, topology.raw_relationships);
    assert_eq!(after.raw_outgoing, topology.raw_outgoing);
    assert_eq!(after.raw_incoming, topology.raw_incoming);
}

#[test]
fn half_open_ranges_and_capacities_preserve_full_u128_order() {
    let a = (1_u128 << 100) | 1;
    let b = (1_u128 << 100) | 2;
    let low_rel = 1;
    let high_rel = (1_u128 << 100) | 1;
    let max_rel = u128::MAX;
    let mut model = Model::new();
    model
        .apply(
            1,
            &[
                Operation::CreateNode { id: a },
                Operation::CreateNode { id: b },
                Operation::CreateRelationship {
                    rel: low_rel,
                    source: a,
                    target: b,
                    relationship_type: 1,
                },
                Operation::CreateRelationship {
                    rel: high_rel,
                    source: a,
                    target: b,
                    relationship_type: 1,
                },
                Operation::CreateRelationship {
                    rel: max_rel,
                    source: a,
                    target: b,
                    relationship_type: 2,
                },
            ],
        )
        .unwrap();
    let plan = ObservationPlan {
        relationship_ranges: vec![
            RelationshipRange {
                start: high_rel,
                end: Some(max_rel),
                capacity: 8,
            },
            RelationshipRange {
                start: high_rel,
                end: None,
                capacity: 2,
            },
            RelationshipRange {
                start: 0,
                end: None,
                capacity: 0,
            },
        ],
        adjacency_ranges: vec![AdjacencyRange {
            direction: Direction::Outgoing,
            start: AdjacencyRow {
                bound_node: a,
                relationship_type: 1,
                rel: high_rel,
                neighbor: 0,
            },
            end: Some(AdjacencyRow {
                bound_node: a,
                relationship_type: 2,
                rel: 0,
                neighbor: 0,
            }),
            capacity: 8,
        }],
        degrees: Vec::new(),
    };
    let observation = model.snapshot().observation(&plan).unwrap();
    assert_eq!(
        observation.relationship_ranges[0]
            .rows
            .iter()
            .map(|row| row.rel)
            .collect::<Vec<_>>(),
        [high_rel]
    );
    assert_eq!(
        observation.relationship_ranges[1]
            .rows
            .iter()
            .map(|row| row.rel)
            .collect::<Vec<_>>(),
        [high_rel, max_rel]
    );
    assert!(observation.relationship_ranges[2].rows.is_empty());
    assert_eq!(observation.adjacency_ranges[0].rows[0].rel, high_rel);

    let invalid = ObservationPlan {
        relationship_ranges: vec![RelationshipRange {
            start: max_rel,
            end: Some(high_rel),
            capacity: 1,
        }],
        ..ObservationPlan::default()
    };
    assert!(model.snapshot().observation(&invalid).is_err());
}
