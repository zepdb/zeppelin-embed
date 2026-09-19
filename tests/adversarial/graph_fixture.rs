//! PG13 tooling comparator probes. These are deliberate wrong observations,
//! not claimed VFS/product faults or graph implementation qualification.
use super::coverage::CoverageRegistry;
use rand::Rng;
use std::collections::{BTreeMap, BTreeSet};
use zeppelin_embed_adversarial_oracle::graph_fixture::*;
pub const REQUIRED_COVERAGE: &[&str] = &[
    "property-graph.fixture.missing-edge.fire",
    "property-graph.fixture.missing-edge.clean",
    "property-graph.fixture.narrowed-id.fire",
    "property-graph.fixture.narrowed-id.clean",
    "property-graph.fixture.resurrection.fire",
    "property-graph.fixture.resurrection.clean",
    "property-graph.fixture.multiplicity.fire",
    "property-graph.fixture.multiplicity.clean",
];
pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    let mut rng = super::test_support::seeded_rng("property-graph.fixture", seed);
    let low = u128::from(rng.random::<u64>()) | 1;
    let a = (1_u128 << 100) | low;
    let b = (2_u128 << 100) | low;
    let node = |id| Node {
        id,
        key: None,
        revision: 1,
        generation: 1,
        labels: BTreeSet::new(),
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
        relationship_type: "edge".into(),
        properties: BTreeMap::new(),
    };
    let expected = Snapshot {
        nodes: vec![node(a), node(b)],
        relationships: vec![edge(1, a, b), edge(2, a, b), edge(3, a, a)],
    };
    let mut missing = expected.clone();
    missing.relationships.remove(1);
    paired(
        compare(&expected, &missing),
        compare(&expected, &expected),
        "missing-edge",
        coverage,
    )?;
    let mut narrowed = expected.clone();
    for node in &mut narrowed.nodes {
        node.id = u128::from(node.id as u64);
    }
    for edge in &mut narrowed.relationships {
        edge.source = u128::from(edge.source as u64);
        edge.target = u128::from(edge.target as u64);
    }
    paired(
        compare(&expected, &narrowed),
        compare(&expected, &expected),
        "narrowed-id",
        coverage,
    )?;
    let mut graph = Graph::default();
    let create = Mutation {
        key: Key {
            kind: Kind::Node,
            namespace: "fixture".into(),
            value: "deleted".into(),
        },
        operation: Operation::Create,
        revision: 1,
        expected: Expectation::Absent,
        detach: false,
        image: Some(Image::Node {
            labels: BTreeSet::new(),
            properties: BTreeMap::new(),
            text: None,
            vector: None,
        }),
    };
    let id = graph
        .apply(std::slice::from_ref(&create))
        .map_err(|e| format!("create {e:?}"))?
        .receipts[0]
        .id;
    graph
        .apply(&[Mutation {
            operation: Operation::Delete,
            revision: 2,
            expected: Expectation::Entity(id),
            image: None,
            ..create
        }])
        .map_err(|e| format!("delete {e:?}"))?;
    let deleted = graph.snapshot();
    let mut resurrected = deleted.clone();
    resurrected.nodes.push(node(id));
    paired(
        compare(&deleted, &resurrected),
        compare(&deleted, &deleted),
        "resurrection",
        coverage,
    )?;
    let row = vec![
        Cell::Node(a),
        Cell::Null,
        Cell::List(vec![Cell::Relationship(1)]),
    ];
    let rows = vec![row.clone(), row.clone()];
    paired(
        compare_rows(&rows, &[row], false),
        compare_rows(&rows, &rows, false),
        "multiplicity",
        coverage,
    )
}
fn paired(
    fault: Result<(), String>,
    clean: Result<(), String>,
    name: &str,
    coverage: &mut CoverageRegistry,
) -> Result<(), String> {
    if fault.is_ok() {
        return Err(format!("PG13 {name} comparator did not fire"));
    }
    coverage.hit(format!("property-graph.fixture.{name}.fire"));
    clean?;
    coverage.hit(format!("property-graph.fixture.{name}.clean"));
    Ok(())
}
