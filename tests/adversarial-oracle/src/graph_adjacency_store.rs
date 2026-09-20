//! Independent native adjacency and endpoint-liveness expected-value model.
//!
//! Inputs are already-normalized changed batches. This module has no engine,
//! codec, physical-reference, publication or request-classification dependency.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum EntityKind {
    Node,
    Relationship,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    CreateNode {
        id: u128,
    },
    DeleteNode {
        id: u128,
        detach: bool,
    },
    CreateRelationship {
        rel: u128,
        source: u128,
        target: u128,
        relationship_type: u64,
    },
    DeleteRelationship {
        rel: u128,
    },
    PropertyOnly {
        entity_kind: EntityKind,
        id: u128,
    },
}

impl Operation {
    fn target(self) -> (EntityKind, u128) {
        match self {
            Self::CreateNode { id } | Self::DeleteNode { id, .. } => (EntityKind::Node, id),
            Self::CreateRelationship { rel, .. } | Self::DeleteRelationship { rel } => {
                (EntityKind::Relationship, rel)
            }
            Self::PropertyOnly { entity_kind, id } => (entity_kind, id),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid(&'static str),
    State(&'static str),
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RelationshipRow {
    pub rel: u128,
    pub source: u128,
    pub target: u128,
    pub relationship_type: u64,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct AdjacencyRow {
    pub bound_node: u128,
    pub relationship_type: u64,
    pub rel: u128,
    pub neighbor: u128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NodeLiveness {
    pub id: u128,
    pub live: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Outgoing,
    Incoming,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RelationshipRange {
    pub start: u128,
    pub end: Option<u128>,
    pub capacity: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdjacencyRange {
    pub direction: Direction,
    pub start: AdjacencyRow,
    pub end: Option<AdjacencyRow>,
    pub capacity: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DegreeQuery {
    pub node: u128,
    pub direction: Direction,
    pub relationship_type: Option<u64>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ObservationPlan {
    pub relationship_ranges: Vec<RelationshipRange>,
    pub adjacency_ranges: Vec<AdjacencyRange>,
    pub degrees: Vec<DegreeQuery>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RelationshipRangeResult {
    pub query: RelationshipRange,
    pub rows: Vec<RelationshipRow>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdjacencyRangeResult {
    pub query: AdjacencyRange,
    pub rows: Vec<AdjacencyRow>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DegreeResult {
    pub query: DegreeQuery,
    pub degree: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Observation {
    pub generation: u64,
    pub nodes: Vec<NodeLiveness>,
    pub raw_relationships: Vec<RelationshipRow>,
    pub raw_outgoing: Vec<AdjacencyRow>,
    pub raw_incoming: Vec<AdjacencyRow>,
    pub visible_relationships: Vec<RelationshipRow>,
    pub visible_outgoing: Vec<AdjacencyRow>,
    pub visible_incoming: Vec<AdjacencyRow>,
    pub visible_relationship_count: u64,
    pub degrees: Vec<DegreeResult>,
    pub relationship_ranges: Vec<RelationshipRangeResult>,
    pub adjacency_ranges: Vec<AdjacencyRangeResult>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Difference {
    pub path: String,
    pub expected: String,
    pub observed: String,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    generation: u64,
    allocated_nodes: BTreeSet<u128>,
    allocated_relationships: BTreeSet<u128>,
    live_nodes: BTreeSet<u128>,
    relationships: BTreeMap<u128, RelationshipRow>,
}

pub struct Model {
    state: Snapshot,
}

/// Stable identity for the independent ZE-45 visible-expansion comparator.
pub const READ_VIEW_COMPARATOR_ID: &str = "ZE-129/read-view-visible-outgoing-v1";

/// Derives the expected visible outgoing expansion from primitive graph
/// operations and compares exact type/relationship order without normalizing
/// the engine observation.
pub fn compare_read_view_expansion(
    generation: u64,
    operations: &[Operation],
    bound_node: u128,
    observed: &[RelationshipRow],
) -> Result<(), Difference> {
    let mut model = Model::new();
    model
        .apply(generation, operations)
        .map_err(|error| Difference {
            path: "read_view.operations".to_owned(),
            expected: "valid independent model input".to_owned(),
            observed: format!("{error:?}"),
        })?;
    let observation = model
        .snapshot()
        .observation(&ObservationPlan::default())
        .map_err(|error| Difference {
            path: "read_view.plan".to_owned(),
            expected: "valid independent observation".to_owned(),
            observed: format!("{error:?}"),
        })?;
    let expected = observation
        .visible_outgoing
        .into_iter()
        .filter(|row| row.bound_node == bound_node)
        .map(|row| RelationshipRow {
            rel: row.rel,
            source: row.bound_node,
            target: row.neighbor,
            relationship_type: row.relationship_type,
        })
        .collect::<Vec<_>>();
    sequence(
        "read_view.visible_outgoing",
        &expected,
        observed,
        relationship,
    )
}

impl Model {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Snapshot {
                generation: 0,
                allocated_nodes: BTreeSet::new(),
                allocated_relationships: BTreeSet::new(),
                live_nodes: BTreeSet::new(),
                relationships: BTreeMap::new(),
            },
        }
    }

    pub fn apply(&mut self, generation: u64, operations: &[Operation]) -> Result<(), Error> {
        if operations.is_empty() {
            return Err(Error::Invalid("empty_batch"));
        }
        if generation <= self.state.generation {
            return Err(Error::State("generation"));
        }
        let mut targets = BTreeSet::new();
        let mut created_nodes = BTreeSet::new();
        let mut deleted_nodes = BTreeMap::new();
        let mut deleted_relationships = BTreeSet::new();
        for operation in operations {
            if !targets.insert(operation.target()) {
                return Err(Error::Invalid("duplicate_target"));
            }
            match *operation {
                Operation::CreateNode { id } => {
                    created_nodes.insert(id);
                }
                Operation::DeleteNode { id, detach } => {
                    deleted_nodes.insert(id, detach);
                }
                Operation::DeleteRelationship { rel } => {
                    deleted_relationships.insert(rel);
                }
                Operation::CreateRelationship { .. } | Operation::PropertyOnly { .. } => {}
            }
        }
        let final_node_live = |id: &u128| {
            (self.state.live_nodes.contains(id) || created_nodes.contains(id))
                && !deleted_nodes.contains_key(id)
        };
        for operation in operations {
            match *operation {
                Operation::CreateNode { id } => {
                    if id == 0 {
                        return Err(Error::Invalid("zero_identity"));
                    }
                    if self.state.allocated_nodes.contains(&id) {
                        return Err(Error::State("reused_node"));
                    }
                }
                Operation::DeleteNode { id, .. } => {
                    if id == 0 {
                        return Err(Error::Invalid("zero_identity"));
                    }
                    if !self.state.live_nodes.contains(&id) {
                        return Err(Error::State("missing_node"));
                    }
                }
                Operation::CreateRelationship {
                    rel,
                    source,
                    target,
                    relationship_type: _,
                } => {
                    if rel == 0 || source == 0 || target == 0 {
                        return Err(Error::Invalid("zero_identity"));
                    }
                    if !final_node_live(&source) || !final_node_live(&target) {
                        return Err(Error::State("missing_endpoint"));
                    }
                    if self.state.allocated_relationships.contains(&rel) {
                        return Err(Error::State("reused_relationship"));
                    }
                }
                Operation::DeleteRelationship { rel } => {
                    if rel == 0 {
                        return Err(Error::Invalid("zero_identity"));
                    }
                    if !self.state.relationships.contains_key(&rel) {
                        return Err(Error::State("missing_relationship"));
                    }
                }
                Operation::PropertyOnly { entity_kind, id } => {
                    if id == 0 {
                        return Err(Error::Invalid("zero_identity"));
                    }
                    match entity_kind {
                        EntityKind::Node if !self.state.live_nodes.contains(&id) => {
                            return Err(Error::State("missing_node"));
                        }
                        EntityKind::Relationship => {
                            let row = self
                                .state
                                .relationships
                                .get(&id)
                                .ok_or(Error::State("missing_relationship"))?;
                            if !self.state.relationship_visible(row) {
                                return Err(Error::State("hidden_relationship"));
                            }
                        }
                        EntityKind::Node => {}
                    }
                }
            }
        }
        for (&id, &detach) in &deleted_nodes {
            if detach {
                continue;
            }
            let has_live_incident = self.state.relationships.values().any(|row| {
                (row.source == id || row.target == id)
                    && self.state.relationship_visible(row)
                    && !deleted_relationships.contains(&row.rel)
            });
            if has_live_incident {
                return Err(Error::State("incident_relationship"));
            }
        }
        let mut candidate = self.state.clone();
        for rel in deleted_relationships {
            candidate.relationships.remove(&rel);
        }
        for id in created_nodes {
            candidate.allocated_nodes.insert(id);
            candidate.live_nodes.insert(id);
        }
        for operation in operations {
            if let Operation::CreateRelationship {
                rel,
                source,
                target,
                relationship_type,
            } = *operation
            {
                candidate.allocated_relationships.insert(rel);
                candidate.relationships.insert(
                    rel,
                    RelationshipRow {
                        rel,
                        source,
                        target,
                        relationship_type,
                    },
                );
            }
        }
        for id in deleted_nodes.keys() {
            candidate.live_nodes.remove(id);
        }
        candidate.generation = generation;
        self.state = candidate;
        Ok(())
    }

    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        self.state.clone()
    }
}

impl Default for Model {
    fn default() -> Self {
        Self::new()
    }
}

impl Snapshot {
    fn relationship_visible(&self, row: &RelationshipRow) -> bool {
        self.live_nodes.contains(&row.source) && self.live_nodes.contains(&row.target)
    }

    /// Compares exact order and values without repairing the observation.
    pub fn check(&self, plan: &ObservationPlan, observed: &Observation) -> Result<(), Difference> {
        let expected = self.observation(plan).map_err(|error| Difference {
            path: "plan".to_owned(),
            expected: "valid observation plan".to_owned(),
            observed: format!("{error:?}"),
        })?;
        compare_observation(&expected, observed)
    }

    pub fn observation(&self, plan: &ObservationPlan) -> Result<Observation, Error> {
        let raw_relationships: Vec<_> = self.relationships.values().copied().collect();
        let mut raw_outgoing: Vec<_> = raw_relationships
            .iter()
            .map(|row| AdjacencyRow {
                bound_node: row.source,
                relationship_type: row.relationship_type,
                rel: row.rel,
                neighbor: row.target,
            })
            .collect();
        raw_outgoing.sort_unstable();
        let mut raw_incoming: Vec<_> = raw_relationships
            .iter()
            .map(|row| AdjacencyRow {
                bound_node: row.target,
                relationship_type: row.relationship_type,
                rel: row.rel,
                neighbor: row.source,
            })
            .collect();
        raw_incoming.sort_unstable();
        let visible_relationships: Vec<_> = raw_relationships
            .iter()
            .filter(|row| self.relationship_visible(row))
            .copied()
            .collect();
        let visible_outgoing: Vec<_> = raw_outgoing
            .iter()
            .filter(|row| {
                self.live_nodes.contains(&row.bound_node) && self.live_nodes.contains(&row.neighbor)
            })
            .copied()
            .collect();
        let visible_incoming: Vec<_> = raw_incoming
            .iter()
            .filter(|row| {
                self.live_nodes.contains(&row.bound_node) && self.live_nodes.contains(&row.neighbor)
            })
            .copied()
            .collect();
        let mut relationship_ranges = Vec::with_capacity(plan.relationship_ranges.len());
        for query in &plan.relationship_ranges {
            if query.end.is_some_and(|end| end < query.start) {
                return Err(Error::Invalid("relationship_range"));
            }
            relationship_ranges.push(RelationshipRangeResult {
                query: *query,
                rows: visible_relationships
                    .iter()
                    .filter(|row| {
                        row.rel >= query.start && query.end.is_none_or(|end| row.rel < end)
                    })
                    .take(query.capacity)
                    .copied()
                    .collect(),
            });
        }
        let mut adjacency_ranges = Vec::with_capacity(plan.adjacency_ranges.len());
        for query in &plan.adjacency_ranges {
            if query.end.is_some_and(|end| end < query.start) {
                return Err(Error::Invalid("adjacency_range"));
            }
            let rows = match query.direction {
                Direction::Outgoing => &visible_outgoing,
                Direction::Incoming => &visible_incoming,
            };
            adjacency_ranges.push(AdjacencyRangeResult {
                query: *query,
                rows: rows
                    .iter()
                    .filter(|row| **row >= query.start && query.end.is_none_or(|end| **row < end))
                    .take(query.capacity)
                    .copied()
                    .collect(),
            });
        }
        let mut degrees = Vec::with_capacity(plan.degrees.len());
        for query in &plan.degrees {
            let rows = match query.direction {
                Direction::Outgoing => &visible_outgoing,
                Direction::Incoming => &visible_incoming,
            };
            let count = rows
                .iter()
                .filter(|row| {
                    row.bound_node == query.node
                        && query.relationship_type.is_none_or(|relationship_type| {
                            row.relationship_type == relationship_type
                        })
                })
                .count();
            degrees.push(DegreeResult {
                query: *query,
                degree: u64::try_from(count).map_err(|_| Error::Invalid("degree"))?,
            });
        }
        Ok(Observation {
            generation: self.generation,
            nodes: self
                .allocated_nodes
                .iter()
                .map(|id| NodeLiveness {
                    id: *id,
                    live: self.live_nodes.contains(id),
                })
                .collect(),
            raw_relationships,
            raw_outgoing,
            raw_incoming,
            visible_relationship_count: visible_relationships.len() as u64,
            visible_relationships,
            visible_outgoing,
            visible_incoming,
            degrees,
            relationship_ranges,
            adjacency_ranges,
        })
    }
}

fn compare_observation(expected: &Observation, observed: &Observation) -> Result<(), Difference> {
    field("generation", &expected.generation, &observed.generation)?;
    sequence("nodes", &expected.nodes, &observed.nodes, node_liveness)?;
    sequence(
        "raw_relationships",
        &expected.raw_relationships,
        &observed.raw_relationships,
        relationship,
    )?;
    sequence(
        "raw_outgoing",
        &expected.raw_outgoing,
        &observed.raw_outgoing,
        adjacency,
    )?;
    sequence(
        "raw_incoming",
        &expected.raw_incoming,
        &observed.raw_incoming,
        adjacency,
    )?;
    sequence(
        "visible_relationships",
        &expected.visible_relationships,
        &observed.visible_relationships,
        relationship,
    )?;
    sequence(
        "visible_outgoing",
        &expected.visible_outgoing,
        &observed.visible_outgoing,
        adjacency,
    )?;
    sequence(
        "visible_incoming",
        &expected.visible_incoming,
        &observed.visible_incoming,
        adjacency,
    )?;
    field(
        "visible_relationship_count",
        &expected.visible_relationship_count,
        &observed.visible_relationship_count,
    )?;
    sequence("degrees", &expected.degrees, &observed.degrees, degree)?;
    sequence(
        "relationship_ranges",
        &expected.relationship_ranges,
        &observed.relationship_ranges,
        relationship_range,
    )?;
    sequence(
        "adjacency_ranges",
        &expected.adjacency_ranges,
        &observed.adjacency_ranges,
        adjacency_range,
    )
}

fn node_liveness(
    path: &str,
    expected: &NodeLiveness,
    observed: &NodeLiveness,
) -> Result<(), Difference> {
    field(&format!("{path}.id"), &expected.id, &observed.id)?;
    field(&format!("{path}.live"), &expected.live, &observed.live)
}

fn relationship(
    path: &str,
    expected: &RelationshipRow,
    observed: &RelationshipRow,
) -> Result<(), Difference> {
    field(&format!("{path}.rel"), &expected.rel, &observed.rel)?;
    field(
        &format!("{path}.source"),
        &expected.source,
        &observed.source,
    )?;
    field(
        &format!("{path}.target"),
        &expected.target,
        &observed.target,
    )?;
    field(
        &format!("{path}.relationship_type"),
        &expected.relationship_type,
        &observed.relationship_type,
    )
}

fn adjacency(
    path: &str,
    expected: &AdjacencyRow,
    observed: &AdjacencyRow,
) -> Result<(), Difference> {
    field(
        &format!("{path}.bound_node"),
        &expected.bound_node,
        &observed.bound_node,
    )?;
    field(
        &format!("{path}.relationship_type"),
        &expected.relationship_type,
        &observed.relationship_type,
    )?;
    field(&format!("{path}.rel"), &expected.rel, &observed.rel)?;
    field(
        &format!("{path}.neighbor"),
        &expected.neighbor,
        &observed.neighbor,
    )
}

fn degree(path: &str, expected: &DegreeResult, observed: &DegreeResult) -> Result<(), Difference> {
    field(&format!("{path}.query"), &expected.query, &observed.query)?;
    field(
        &format!("{path}.degree"),
        &expected.degree,
        &observed.degree,
    )
}

fn relationship_range(
    path: &str,
    expected: &RelationshipRangeResult,
    observed: &RelationshipRangeResult,
) -> Result<(), Difference> {
    field(&format!("{path}.query"), &expected.query, &observed.query)?;
    sequence(
        &format!("{path}.rows"),
        &expected.rows,
        &observed.rows,
        relationship,
    )
}

fn adjacency_range(
    path: &str,
    expected: &AdjacencyRangeResult,
    observed: &AdjacencyRangeResult,
) -> Result<(), Difference> {
    field(&format!("{path}.query"), &expected.query, &observed.query)?;
    sequence(
        &format!("{path}.rows"),
        &expected.rows,
        &observed.rows,
        adjacency,
    )
}

fn sequence<T>(
    path: &str,
    expected: &[T],
    observed: &[T],
    mut compare: impl FnMut(&str, &T, &T) -> Result<(), Difference>,
) -> Result<(), Difference> {
    field(&format!("{path}.length"), &expected.len(), &observed.len())?;
    for (index, (expected, observed)) in expected.iter().zip(observed).enumerate() {
        compare(&format!("{path}[{index}]"), expected, observed)?;
    }
    Ok(())
}

fn field<T: Debug + Eq>(path: &str, expected: &T, observed: &T) -> Result<(), Difference> {
    if expected == observed {
        Ok(())
    } else {
        Err(Difference {
            path: path.to_owned(),
            expected: format!("{expected:?}"),
            observed: format!("{observed:?}"),
        })
    }
}
