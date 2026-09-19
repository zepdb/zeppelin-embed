//! PG13: independent primitive fixture graph, lifecycle and query expectations.
//! No engine or generator type/helper enters this module.
use std::collections::{BTreeMap, BTreeSet};

/// Original typed scalar bits; canonical equality is deliberately not numeric equality.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Scalar {
    Bool(bool),
    I64(i64),
    F64(u64),
    String(String),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Element {
    Empty,
    Bool,
    I64,
    F64,
    String,
}
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Property {
    Scalar(Scalar),
    List(Element, Vec<Scalar>),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Kind {
    Node,
    Relationship,
}
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct Key {
    pub kind: Kind,
    pub namespace: String,
    pub value: String,
}
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct Node {
    pub id: u128,
    pub key: Option<Key>,
    pub revision: u64,
    pub generation: u64,
    pub labels: BTreeSet<String>,
    pub properties: BTreeMap<String, Property>,
    pub text: Option<String>,
    pub vector: Option<Vec<u32>>,
}
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct Relationship {
    pub id: u128,
    pub key: Option<Key>,
    pub revision: u64,
    pub generation: u64,
    pub source: u128,
    pub target: u128,
    pub relationship_type: String,
    pub properties: BTreeMap<String, Property>,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Snapshot {
    pub nodes: Vec<Node>,
    pub relationships: Vec<Relationship>,
}
/// Exact comparison of complete public primitive graph observations.
pub fn compare(expected: &Snapshot, observed: &Snapshot) -> Result<(), String> {
    let sorted = |snapshot: &Snapshot| {
        let mut nodes = snapshot.nodes.clone();
        let mut relationships = snapshot.relationships.clone();
        nodes.sort();
        relationships.sort();
        (nodes, relationships)
    };
    if sorted(expected) == sorted(observed) {
        Ok(())
    } else {
        Err(format!(
            "PG13 graph snapshot differs expected={expected:?} observed={observed:?}"
        ))
    }
}

mod model;
pub use model::*;

mod query;
pub use query::*;
