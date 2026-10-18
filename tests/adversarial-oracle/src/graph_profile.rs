//! Primitive, engine-independent graph profile comparison.

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
