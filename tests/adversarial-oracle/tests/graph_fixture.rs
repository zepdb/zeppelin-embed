use std::collections::{BTreeMap, BTreeSet};
use zeppelin_embed_adversarial_oracle::graph_fixture::*;
fn small() -> Snapshot {
    let a = (1_u128 << 80) + 7;
    let b = (2_u128 << 80) + 7;
    let node = |id| Node {
        id,
        key: None,
        revision: 1,
        generation: 1,
        labels: BTreeSet::from(["Person".into()]),
        properties: BTreeMap::new(),
        text: None,
        vector: None,
    };
    let edge = |id, source, target| Relationship {
        id,
        key: None,
        revision: 1,
        generation: 1,
        source,
        target,
        relationship_type: "KNOWS".into(),
        properties: BTreeMap::new(),
    };
    Snapshot {
        nodes: vec![node(a), node(b)],
        relationships: vec![edge(1, a, b), edge(2, a, b), edge(3, a, a)],
    }
}
#[test]
fn primitive_fixture_comparator_rejects_a_missing_parallel_edge() {
    let expected = small();
    assert!(compare(&expected, &expected).is_ok());
    let mut missing = expected.clone();
    missing.relationships.remove(1);
    assert!(
        compare(&expected, &missing).is_err(),
        "PG13 missing edge must fire"
    );
}
fn keyed(name: &str) -> Key {
    Key {
        kind: Kind::Node,
        namespace: "person".into(),
        value: name.into(),
    }
}
fn node_image() -> Image {
    Image::Node {
        labels: BTreeSet::from(["Person".into()]),
        properties: BTreeMap::new(),
        text: Some(String::new()),
        vector: None,
    }
}
#[test]
fn independent_key_history_never_resurrects_deleted_keys() {
    let mut graph = Graph::default();
    let create = Mutation {
        key: keyed("alice"),
        operation: Operation::Create,
        revision: 1,
        expected: Expectation::Absent,
        detach: false,
        image: Some(node_image()),
    };
    let initial = graph.apply(std::slice::from_ref(&create)).unwrap();
    assert!(
        initial.changed,
        "PG13 creation must change independent state"
    );
    let id = initial.receipts[0].id;
    let delete = Mutation {
        operation: Operation::Delete,
        revision: 2,
        expected: Expectation::Entity(id),
        image: None,
        ..create.clone()
    };
    graph.apply(std::slice::from_ref(&delete)).unwrap();
    let before = graph.clone();
    assert_eq!(
        graph.apply(&[Mutation {
            revision: 3,
            ..create.clone()
        }]),
        Err(Rejection::Deleted)
    );
    assert_eq!(graph, before);
    let recreated = graph
        .apply(&[Mutation {
            operation: Operation::Recreate,
            revision: 3,
            expected: Expectation::Deletion(2),
            ..create.clone()
        }])
        .unwrap();
    assert_ne!(recreated.receipts[0].id, id);
    assert_eq!(
        graph.apply(&[Mutation {
            operation: Operation::Put,
            revision: 9,
            expected: Expectation::Entity(id),
            ..create
        }]),
        Err(Rejection::Incarnation)
    );
    assert_eq!(graph.apply(&[delete]), Err(Rejection::Incarnation));
}
fn named_graph() -> Snapshot {
    let mut graph = Snapshot::default();
    for (id, label, name, timestamp) in [
        (1, "Project", "P", 0),
        (2, "Person", "Alice", 0),
        (3, "Meeting", "M", 99),
        (4, "Chunk", "C", 0),
        (5, "Decision", "D", 0),
        (6, "Topic", "T", 0),
    ] {
        let mut properties =
            BTreeMap::from([("name".into(), Property::Scalar(Scalar::String(name.into())))]);
        if id == 3 {
            properties.insert("timestamp".into(), Property::Scalar(Scalar::I64(timestamp)));
        }
        if id == 4 {
            properties.insert(
                "excerpt".into(),
                Property::Scalar(Scalar::String("evidence".into())),
            );
        }
        graph.nodes.push(Node {
            id,
            key: None,
            revision: 1,
            generation: 1,
            labels: BTreeSet::from([label.into()]),
            properties,
            text: Some(if id == 4 { "amber cedar" } else { "amber" }.into()),
            vector: (id == 4).then_some(vec![1_f32.to_bits(), 0]),
        });
    }
    for (id, source, target, kind) in [
        (10, 3, 4, "HAS_CHUNK"),
        (11, 5, 4, "SUPPORTED_BY"),
        (12, 5, 1, "ABOUT"),
        (13, 2, 3, "PARTICIPATED_IN"),
        (14, 3, 1, "FOR_PROJECT"),
        (15, 4, 2, "MENTIONS"),
        (16, 4, 1, "MENTIONS"),
        (17, 4, 6, "MENTIONS"),
        (18, 3, 5, "HAS_ITEM"),
    ] {
        graph.relationships.push(Relationship {
            id,
            key: None,
            revision: 1,
            generation: 1,
            source,
            target,
            relationship_type: kind.into(),
            properties: BTreeMap::new(),
        });
    }
    graph
}
#[test]
fn project_evidence_preserves_full_literal_join_row_and_prelimit_order() {
    let graph = named_graph();
    let expected = vec![vec![
        Cell::Node(5),
        Cell::String("D".into()),
        Cell::Node(3),
        Cell::String("M".into()),
        Cell::Node(4),
        Cell::String("evidence".into()),
    ]];
    assert_eq!(
        query(
            &graph,
            &Query::ProjectEvidence {
                project: 1,
                limit: 100
            }
        )
        .unwrap(),
        expected,
        "PG13 project evidence literal row"
    );
}
#[test]
fn semantic_context_preserves_computed_scores_and_three_distinct_rows() {
    let rows = query(
        &named_graph(),
        &Query::SemanticContext {
            vector: vec![0.5, 0.0],
            k: 20,
        },
    )
    .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0],
        vec![
            Cell::Node(4),
            Cell::Score(0.25_f64.to_bits()),
            Cell::String("evidence".into()),
            Cell::Node(3),
            Cell::String("M".into()),
            Cell::Node(1),
            Cell::String("P".into())
        ]
    );
    assert_eq!(
        rows.iter().map(|r| r[5].clone()).collect::<Vec<_>>(),
        vec![Cell::Node(1), Cell::Node(2), Cell::Node(6)]
    );
}
fn add_context_chunk(graph: &mut Snapshot, id: u128, vector: [f32; 2]) {
    let mut chunk = graph
        .nodes
        .iter()
        .find(|node| node.id == 4)
        .unwrap()
        .clone();
    chunk.id = id;
    chunk.vector = Some(vector.iter().map(|v| v.to_bits()).collect());
    graph.nodes.push(chunk);
    for (source, target, kind) in [
        (3, id, "HAS_CHUNK"),
        (id, 1, "MENTIONS"),
        (id, 2, "MENTIONS"),
        (id, 6, "MENTIONS"),
    ] {
        let mut edge = graph.relationships[0].clone();
        edge.id = graph.relationships.len() as u128 + 100;
        edge.source = source;
        edge.target = target;
        edge.relationship_type = kind.into();
        graph.relationships.push(edge);
    }
}
#[test]
fn semantic_context_derives_nearest_full_live_seed_before_expansion() {
    let mut graph = named_graph();
    add_context_chunk(&mut graph, 99, [0.5, 0.0]);
    let rows = query(
        &graph,
        &Query::SemanticContext {
            vector: vec![0.5, 0.0],
            k: 1,
        },
    )
    .unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| (row[0].clone(), row[1].clone()))
            .collect::<Vec<_>>(),
        vec![(Cell::Node(99), Cell::Score(0_f64.to_bits())); 3],
        "PG13 semantic seed truth must select the nearer omitted live chunk"
    );
}
#[test]
fn semantic_context_top_twenty_uses_full_id_ties_and_keeps_parallel_rows() {
    let mut graph = named_graph();
    graph
        .nodes
        .iter_mut()
        .find(|node| node.id == 4)
        .unwrap()
        .vector = None;
    let ids = (1..=21_u128).map(|i| (i << 80) + 7).collect::<Vec<_>>();
    for id in ids.iter().rev() {
        add_context_chunk(&mut graph, *id, [1.0, 0.0]);
    }
    let mut parallel = graph
        .relationships
        .iter()
        .find(|edge| edge.source == ids[0] && edge.target == 1)
        .unwrap()
        .clone();
    parallel.id = 9999;
    graph.relationships.push(parallel);
    let rows = query(
        &graph,
        &Query::SemanticContext {
            vector: vec![0.5, 0.0],
            k: 20,
        },
    )
    .unwrap();
    let mut expected = Vec::new();
    for id in &ids[..20] {
        for (entity, name) in [(1, "P"), (2, "Alice"), (6, "T")] {
            let row = vec![
                Cell::Node(*id),
                Cell::Score(0.25_f64.to_bits()),
                Cell::String("evidence".into()),
                Cell::Node(3),
                Cell::String("M".into()),
                Cell::Node(entity),
                Cell::String(name.into()),
            ];
            if *id == ids[0] && entity == 1 {
                expected.push(row.clone());
            }
            expected.push(row);
        }
    }
    assert_eq!(
        rows, expected,
        "PG13 full-ID tied seeds must rank before the top-20 limit and preserve parallel evidence"
    );
    assert_eq!(rows.len(), 61);
    assert!(
        query(
            &graph,
            &Query::SemanticContext {
                vector: vec![0.5, 0.0],
                k: 0
            }
        )
        .unwrap()
        .is_empty()
    );
}
#[test]
fn semantic_context_does_not_refill_seeds_without_context() {
    let mut graph = named_graph();
    let mut unexpandable = graph
        .nodes
        .iter()
        .find(|node| node.id == 4)
        .unwrap()
        .clone();
    unexpandable.id = 99;
    unexpandable.vector = Some(vec![0.5_f32.to_bits(), 0]);
    graph.nodes.push(unexpandable);
    assert!(
        query(
            &graph,
            &Query::SemanticContext {
                vector: vec![0.5, 0.0],
                k: 1
            }
        )
        .unwrap()
        .is_empty(),
        "global k is applied before the context join"
    );
    for vector in [vec![], vec![f32::NAN, 0.0], vec![0.5]] {
        assert!(query(&graph, &Query::SemanticContext { vector, k: 20 }).is_err());
    }
    graph
        .nodes
        .iter_mut()
        .find(|node| node.id == 99)
        .unwrap()
        .vector = Some(vec![f32::INFINITY.to_bits(), 0]);
    assert!(
        query(
            &graph,
            &Query::SemanticContext {
                vector: vec![0.5, 0.0],
                k: 20
            }
        )
        .is_err()
    );
}
#[test]
fn eligible_vector_truth_never_filters_global_top_k() {
    let mut graph = named_graph();
    graph.nodes.iter_mut().find(|n| n.id == 4).unwrap().vector = Some(vec![0, 1_f32.to_bits()]);
    let mut outside = graph.nodes[3].clone();
    outside.id = 99;
    outside.vector = Some(vec![1_f32.to_bits(), 0]);
    graph.nodes.push(outside);
    assert_eq!(
        query(
            &graph,
            &Query::AliceProjectRanking {
                person: 2,
                project: 1,
                vector: vec![1.0, 0.0],
                k: 1
            }
        )
        .unwrap(),
        vec![vec![
            Cell::Node(4),
            Cell::Score(2_f64.to_bits()),
            Cell::String("evidence".into()),
            Cell::Node(3)
        ]]
    );
    assert!(
        query(
            &graph,
            &Query::AliceProjectRanking {
                person: 6,
                project: 1,
                vector: vec![1.0, 0.0],
                k: 20
            }
        )
        .unwrap()
        .is_empty()
    );
}
#[test]
fn lexical_reference_uses_full_live_statistics_and_phrase_positions() {
    let graph = named_graph();
    let rows = query(
        &graph,
        &Query::LexicalEvidence {
            terms: vec!["cedar".into()],
            phrase: false,
            k: 20,
        },
    )
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Cell::Node(4));
    let Cell::Score(bits) = rows[0][4] else {
        panic!("score")
    };
    // N=6, df=1, dl=2, total tokens=7: ln(14/3)*2.2/(1+1.2*(.25+.75*12/7)).
    assert!((f64::from_bits(bits) - 1.1921031975168894).abs() < 1e-12);
    assert!(
        query(
            &graph,
            &Query::LexicalEvidence {
                terms: vec!["cedar".into(), "amber".into()],
                phrase: true,
                k: 20
            }
        )
        .unwrap()
        .is_empty()
    );
    assert_eq!(
        query(
            &graph,
            &Query::LexicalEvidence {
                terms: vec!["amber".into(), "cedar".into()],
                phrase: true,
                k: 20
            }
        )
        .unwrap()
        .len(),
        1
    );
}
#[test]
fn hybrid_reference_cross_scores_optional_modalities_and_empty_eligible_legs() {
    let graph = named_graph();
    let rows = query(
        &graph,
        &Query::HybridProjectEvidence {
            project: 1,
            vector: vec![1.0, 0.0],
            terms: vec!["cedar".into()],
            phrase: false,
            k: 20,
            alpha: 0.75,
        },
    )
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Cell::Node(4));
    assert_eq!(rows[0][2], Cell::Score(1_f64.to_bits()));
    assert_eq!(rows[0][3], Cell::Score(0_f64.to_bits()));
    assert_eq!(&rows[0][5..7], &[Cell::Bool(true), Cell::Bool(true)]);
    let rows = query(
        &graph,
        &Query::HybridProjectEvidence {
            project: 1,
            vector: vec![1.0, 0.0],
            terms: vec!["unmatched".into()],
            phrase: false,
            k: 20,
            alpha: 0.75,
        },
    )
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][2], Cell::Score(1_f64.to_bits()));
    assert_eq!(rows[0][4], Cell::Score(0_f64.to_bits()));
}
#[test]
fn bounded_paths_preserve_parallel_edges_self_loops_and_relationship_uniqueness() {
    let mut graph = named_graph();
    graph.relationships.push(Relationship {
        id: 19,
        source: 4,
        target: 4,
        relationship_type: "MENTIONS".into(),
        ..graph.relationships[0].clone()
    });
    graph.relationships.push(Relationship {
        id: 20,
        source: 4,
        target: 2,
        relationship_type: "MENTIONS".into(),
        ..graph.relationships[0].clone()
    });
    let rows = query(
        &graph,
        &Query::BoundedEvidence {
            meeting: 3,
            min: 1,
            max: 2,
        },
    )
    .unwrap();
    assert_eq!(rows.len(), 6);
    assert_eq!(
        rows[0],
        vec![
            Cell::Node(1),
            Cell::List(vec![Cell::Relationship(10), Cell::Relationship(16)])
        ]
    );
    let longer = query(
        &graph,
        &Query::BoundedEvidence {
            meeting: 3,
            min: 1,
            max: 3,
        },
    )
    .unwrap();
    assert_eq!(longer.len(), 10);
    for row in longer {
        let Cell::List(path) = &row[1] else {
            panic!("path")
        };
        assert_eq!(path.iter().collect::<BTreeSet<_>>().len(), path.len());
    }
    assert!(
        query(
            &graph,
            &Query::BoundedEvidence {
                meeting: 3,
                min: 1,
                max: 17
            }
        )
        .is_err()
    );
}
#[test]
fn key_and_name_bytes_allow_empty_nul_and_original_list_float_bits() {
    let mut graph = Graph::default();
    let request = Mutation {
        key: Key {
            kind: Kind::Node,
            namespace: String::new(),
            value: "\0".into(),
        },
        operation: Operation::Create,
        revision: 1,
        expected: Expectation::Absent,
        detach: false,
        image: Some(Image::Node {
            labels: BTreeSet::from([String::new(), "\0".into()]),
            properties: BTreeMap::from([
                (String::new(), Property::List(Element::Empty, vec![])),
                ("typed".into(), Property::List(Element::String, vec![])),
                (
                    "bits".into(),
                    Property::Scalar(Scalar::F64(0x7ff8000000000007)),
                ),
            ]),
            text: Some(String::new()),
            vector: None,
        }),
    };
    graph
        .apply(std::slice::from_ref(&request))
        .expect("empty/NUL key/name are valid exact UTF-8 bytes");
    let snapshot = graph.snapshot();
    assert_ne!(
        snapshot.nodes[0].properties[""],
        snapshot.nodes[0].properties["typed"]
    );
    assert!(!graph.apply(&[request]).unwrap().changed);
}
#[test]
fn lifecycle_batches_pin_generation_replay_atomicity_and_tombstone_visibility() {
    let mut graph = Graph {
        node_high: 1_u128 << 100,
        ..Graph::default()
    };
    let mut a = Mutation {
        key: keyed("a"),
        operation: Operation::Create,
        revision: 1,
        expected: Expectation::Absent,
        detach: false,
        image: Some(node_image()),
    };
    let b = Mutation {
        key: keyed("b"),
        ..a.clone()
    };
    let first = graph.apply(&[a.clone(), b.clone()]).unwrap();
    assert_eq!(first.generation, 1);
    let aid = first.receipts[0].id;
    let bid = first.receipts[1].id;
    assert_eq!(aid, (1_u128 << 100) + 1);
    let edge = |key: &str, source, target| Mutation {
        key: Key {
            kind: Kind::Relationship,
            namespace: "r".into(),
            value: key.into(),
        },
        operation: Operation::Create,
        revision: 1,
        expected: Expectation::Absent,
        detach: false,
        image: Some(Image::Relationship {
            source,
            target,
            relationship_type: "KNOWS".into(),
            properties: BTreeMap::new(),
        }),
    };
    let rel = edge("parallel1", aid, bid);
    let mut requests = vec![
        a.clone(),
        rel.clone(),
        edge("parallel2", aid, bid),
        edge("self", aid, aid),
    ];
    let mixed = graph.apply(&requests).unwrap();
    assert_eq!(mixed.generation, 2);
    assert_eq!(mixed.receipts[0].generation, 1);
    assert!(mixed.receipts[0].replayed);
    assert_eq!(mixed.receipts[1].generation, 2);
    let before = graph.clone();
    assert!(!graph.apply(&requests).unwrap().changed);
    assert_eq!(graph, before);
    a.operation = Operation::Delete;
    a.revision = 2;
    a.expected = Expectation::Entity(aid);
    a.image = None;
    assert_eq!(graph.apply(&[a.clone()]), Err(Rejection::Restrict));
    assert_eq!(graph, before);
    requests = vec![
        Mutation {
            revision: 2,
            operation: Operation::Put,
            expected: Expectation::Entity(bid),
            ..b.clone()
        },
        Mutation { revision: 0, ..rel },
    ];
    assert_eq!(graph.apply(&requests), Err(Rejection::Invalid));
    assert_eq!(graph, before);
    a.detach = true;
    assert_eq!(graph.apply(&[a]).unwrap().generation, 3);
    assert_eq!(graph.snapshot().nodes.len(), 1);
    assert!(graph.snapshot().relationships.is_empty());
    assert_eq!(
        before.snapshot().relationships.len(),
        3,
        "old primitive view remains complete"
    );
    assert_eq!(
        graph.history.len(),
        5,
        "detached edges remain retained but invisible"
    );
}
#[test]
fn full_width_identity_and_row_bags_reject_narrowing_and_lost_multiplicity() {
    let expected = small();
    let mut narrowed = expected.clone();
    for node in &mut narrowed.nodes {
        node.id = node.id as u64 as u128;
    }
    for edge in &mut narrowed.relationships {
        edge.source = edge.source as u64 as u128;
        edge.target = edge.target as u64 as u128;
    }
    assert!(
        compare(&expected, &narrowed).is_err(),
        "PG13 narrowed ID must fire"
    );
    let row = vec![
        Cell::Node((1_u128 << 100) + 7),
        Cell::Null,
        Cell::List(vec![Cell::Null]),
    ];
    let rows = vec![row.clone(), row.clone()];
    assert!(compare_rows(&rows, &rows, false).is_ok());
    assert!(
        compare_rows(&rows, &[row], false).is_err(),
        "PG13 row multiplicity must fire"
    );
}
#[test]
fn score_comparison_tolerance_never_weakens_ids_order_or_multiplicity() {
    let expected = vec![vec![
        Cell::Node((1_u128 << 100) + 7),
        Cell::Score((1.0_f64 / 3.0).to_bits()),
    ]];
    let rounded = vec![vec![
        expected[0][0].clone(),
        Cell::Score(f64::from(1.0_f32 / 3.0).to_bits()),
    ]];
    assert!(compare_scored_rows(&expected, &rounded, 1e-6, 1e-6).is_ok());
    let mut wrong = rounded.clone();
    wrong[0][0] = Cell::Node(7);
    assert!(compare_scored_rows(&expected, &wrong, 1e-6, 1e-6).is_err());
    wrong = rounded;
    wrong[0][1] = Cell::Score(0.4_f64.to_bits());
    assert!(compare_scored_rows(&expected, &wrong, 1e-6, 1e-6).is_err());
    assert!(compare_scored_rows(&expected, &[], 1e-6, 1e-6).is_err());
    assert!(compare_scored_rows(&expected, &expected, f64::NAN, 0.0).is_err());
}
#[test]
fn recreating_a_live_key_has_its_own_typed_rejection() {
    let mut graph = Graph::default();
    let create = Mutation {
        key: keyed("live"),
        operation: Operation::Create,
        revision: 1,
        expected: Expectation::Absent,
        detach: false,
        image: Some(node_image()),
    };
    graph.apply(std::slice::from_ref(&create)).unwrap();
    let before = graph.clone();
    let error = graph
        .apply(&[Mutation {
            operation: Operation::Recreate,
            revision: 2,
            expected: Expectation::Deletion(1),
            ..create
        }])
        .unwrap_err();
    assert_eq!(error, Rejection::NotDeleted);
    assert_eq!(before, graph);
}
#[test]
fn hybrid_optional_modalities_keep_query_weights_and_absence_distinct() {
    let mut graph = named_graph();
    graph.nodes.iter_mut().find(|n| n.id == 4).unwrap().text = None;
    let rows = query(
        &graph,
        &Query::HybridProjectEvidence {
            project: 1,
            vector: vec![1.0, 0.0],
            terms: vec!["amber".into()],
            phrase: false,
            k: 20,
            alpha: 0.75,
        },
    )
    .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][0], Cell::Node(4));
    assert_eq!(rows[0][2], Cell::Score(0.75_f64.to_bits()));
    assert_eq!(rows[0][4], Cell::Null);
    assert_eq!(
        &rows[0][5..8],
        &[Cell::Bool(true), Cell::Bool(false), Cell::Null]
    );
    for row in &rows[1..] {
        assert_eq!(row[2], Cell::Score(0.25_f64.to_bits()));
        assert_eq!(row[3], Cell::Null);
        assert_eq!(&row[5..7], &[Cell::Bool(false), Cell::Bool(true)]);
    }
    graph.nodes.iter_mut().find(|n| n.id == 5).unwrap().text = Some("   ".into());
    let rows = query(
        &graph,
        &Query::HybridProjectEvidence {
            project: 1,
            vector: vec![1.0, 0.0],
            terms: vec!["amber".into()],
            phrase: false,
            k: 20,
            alpha: 0.75,
        },
    )
    .unwrap();
    assert_eq!(rows.len(), 2);
}
#[test]
fn project_join_order_and_limit_preserve_parallel_evidence_multiplicity() {
    let mut graph = named_graph();
    let high = (1_u128 << 100) + 5;
    let mut item = graph.nodes.iter().find(|n| n.id == 5).unwrap().clone();
    item.id = high;
    graph.nodes.push(item);
    graph.relationships.push(Relationship {
        id: 21,
        source: high,
        target: 1,
        relationship_type: "ABOUT".into(),
        ..graph.relationships[0].clone()
    });
    graph.relationships.push(Relationship {
        id: 22,
        source: high,
        target: 4,
        relationship_type: "SUPPORTED_BY".into(),
        ..graph.relationships[0].clone()
    });
    graph.relationships.push(Relationship {
        id: 23,
        source: 5,
        target: 1,
        relationship_type: "ABOUT".into(),
        ..graph.relationships[0].clone()
    });
    let all = query(
        &graph,
        &Query::ProjectEvidence {
            project: 1,
            limit: 100,
        },
    )
    .unwrap();
    assert_eq!(
        all.iter().map(|r| r[0].clone()).collect::<Vec<_>>(),
        [Cell::Node(5), Cell::Node(5), Cell::Node(high)]
    );
    assert_eq!(
        query(
            &graph,
            &Query::ProjectEvidence {
                project: 1,
                limit: 2
            }
        )
        .unwrap(),
        all[..2]
    );
}
#[test]
fn restrict_checks_admitted_incidents_unless_relationship_explicitly_deleted() {
    let mut graph = Graph::default();
    let create = |name| Mutation {
        key: keyed(name),
        operation: Operation::Create,
        revision: 1,
        expected: Expectation::Absent,
        detach: false,
        image: Some(node_image()),
    };
    let nodes = graph.apply(&[create("a"), create("b")]).unwrap();
    let a = nodes.receipts[0].id;
    let b = nodes.receipts[1].id;
    let edge = Mutation {
        key: Key {
            kind: Kind::Relationship,
            namespace: "r".into(),
            value: "edge".into(),
        },
        operation: Operation::Create,
        revision: 1,
        expected: Expectation::Absent,
        detach: false,
        image: Some(Image::Relationship {
            source: a,
            target: b,
            relationship_type: String::new(),
            properties: BTreeMap::new(),
        }),
    };
    let rel = graph.apply(std::slice::from_ref(&edge)).unwrap().receipts[0].id;
    let delete = |name, id| Mutation {
        key: keyed(name),
        operation: Operation::Delete,
        revision: 2,
        expected: Expectation::Entity(id),
        detach: false,
        image: None,
    };
    let before = graph.clone();
    assert_eq!(
        graph.apply(&[delete("a", a), delete("b", b)]),
        Err(Rejection::Restrict)
    );
    assert_eq!(graph, before);
    graph
        .apply(&[
            delete("a", a),
            delete("b", b),
            Mutation {
                operation: Operation::Delete,
                revision: 2,
                expected: Expectation::Entity(rel),
                image: None,
                ..edge
            },
        ])
        .unwrap();
    assert!(graph.snapshot().nodes.is_empty());
    assert!(graph.snapshot().relationships.is_empty());
}
#[test]
fn hybrid_enclosure_pins_directed_rounding_and_the_full_live_vector_population() {
    let mut graph = named_graph();
    graph.nodes.iter_mut().find(|n| n.id == 4).unwrap().vector = Some(vec![0, 1_f32.to_bits()]);
    let request = Query::HybridProjectEvidence {
        project: 1,
        vector: vec![1.0, 0.0],
        terms: vec!["unmatched".into()],
        phrase: false,
        k: 20,
        alpha: 0.75,
    };
    let rows = query(&graph, &request).unwrap();
    assert_eq!(
        rows[0][2],
        Cell::Score(0x3fe000000000000b),
        "Store-v1 enclosure includes directed rounding, not bare squared norms"
    );
    let mut outside = graph.nodes[3].clone();
    outside.id = 99;
    outside.vector = Some(vec![3_f32.to_bits(), 4_f32.to_bits()]);
    graph.nodes.push(outside);
    let rows = query(&graph, &request).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Cell::Node(4));
    assert_eq!(
        rows[0][2],
        Cell::Score(0x3fee38e38e38e38f),
        "outside-eligible live vectors still set the enclosure"
    );
}
