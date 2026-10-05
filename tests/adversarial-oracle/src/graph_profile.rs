//! Primitive, engine-independent graph profile comparison.
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Cell {
    Null,
    Boolean(bool),
    Integer(i64),
    Float(u64),
    String(String),
    Node(u128),
    Relationship(u128),
    List(Vec<Cell>),
    NodeValue(Vec<String>, Vec<(String, Cell)>),
    RelationshipValue(String, Vec<(String, Cell)>),
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TckState {
    pub nodes: BTreeSet<u128>,
    pub relationships: BTreeSet<u128>,
    pub labels: BTreeSet<String>,
    pub properties: BTreeSet<(String, u128, String, Cell)>,
}
/// Eight TCK counts: created/deleted nodes, relationships, labels, properties.
#[must_use]
pub fn diff_tck_state(before: &TckState, after: &TckState) -> [usize; 8] {
    [
        after.nodes.difference(&before.nodes).count(),
        before.nodes.difference(&after.nodes).count(),
        after
            .relationships
            .difference(&before.relationships)
            .count(),
        before
            .relationships
            .difference(&after.relationships)
            .count(),
        after.labels.difference(&before.labels).count(),
        before.labels.difference(&after.labels).count(),
        after.properties.difference(&before.properties).count(),
        before.properties.difference(&after.properties).count(),
    ]
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    pub columns: Vec<(String, String)>,
    pub rows: Vec<Vec<Cell>>,
    pub ordered: bool,
    pub error: Option<(String, String)>,
    pub disposition: String,
    pub generation: u64,
    pub provenance: Vec<String>,
    pub effects: [usize; 8],
}
pub fn compare_observation(expected: &Observation, observed: &Observation) -> Result<(), String> {
    let mut expected = expected.clone();
    let mut observed = observed.clone();
    if !expected.ordered {
        expected.rows.sort();
        observed.rows.sort();
    }
    if expected == observed {
        Ok(())
    } else {
        Err(format!(
            "declared observation mismatch: expected {expected:?}; observed {observed:?}"
        ))
    }
}
