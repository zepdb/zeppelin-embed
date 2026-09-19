//! Independent logical directory and persistent key-fence model for PG8.
//!
//! Fixture operations supply full identities and lossless logical bytes. This
//! module never imports the engine or decodes a physical storage representation.
//! It is a bounded sequential model, not a publication or durability simulator.

use std::collections::{BTreeMap, BTreeSet};

mod compare;
pub use compare::Difference;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Kind {
    Node,
    Relationship,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Entity {
    pub kind: Kind,
    pub id: u128,
}

/// Logical ordering is kind, exact namespace bytes, then exact key bytes.
/// Physical namespace-symbol ordering is a separate production adapter check.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Key {
    pub kind: Kind,
    pub namespace: Vec<u8>,
    pub key: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Shape {
    Node {
        labels: Vec<u64>,
    },
    Relationship {
        source: u128,
        target: u128,
        rel_type: u64,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Image {
    pub canonical: Vec<u8>,
    pub shape: Shape,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationKind {
    Create,
    Put,
    Delete,
    Recreate,
    Cypher,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Expected {
    Absent,
    Entity(Entity),
    Deletion(u64),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeleteMode {
    Restrict,
    Detach,
}

/// An independently authored fixture request, never an observed engine result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Operation {
    pub kind: OperationKind,
    pub key: Option<Key>,
    pub expected: Expected,
    pub incarnation: Entity,
    pub revision: u64,
    pub delete_mode: Option<DeleteMode>,
    pub image: Option<Image>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Provenance {
    pub version: u16,
    pub operation: OperationKind,
    pub key: Option<Key>,
    pub requested_revision: u64,
    pub installed_revision: u64,
    pub expected: Expected,
    pub incarnation: Entity,
    pub delete_mode: Option<DeleteMode>,
    pub original_generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Record {
    pub image: Image,
    pub provenance: Provenance,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Fence {
    pub provenance: Provenance,
    pub canonical: Option<Vec<u8>>,
}

/// Test-model bounds, independent of the engine's actual-allocation accounting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub max_steps: usize,
    pub max_entities: usize,
    pub max_fences: usize,
    pub max_key_bytes: usize,
    pub max_image_bytes: usize,
    pub max_labels_per_node: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_steps: 4096,
            max_entities: 4096,
            max_fences: 4096,
            max_key_bytes: 64 * 1024,
            max_image_bytes: 64 * 1024,
            max_labels_per_node: 256,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid(&'static str),
    Limit(&'static str),
    State(&'static str),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Outcome {
    Changed(Provenance),
    Replay(Provenance),
    NoOp,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Observation {
    pub generation: u64,
    pub nodes: Vec<Record>,
    pub relationships: Vec<Record>,
    pub fences: Vec<Fence>,
    pub labels: Vec<(u64, u128)>,
    /// Raw type index entries; dead endpoints do not erase stored rows.
    pub types: Vec<(u64, u128)>,
    /// Full deletion provenance, also for unkeyed Cypher nodes.
    pub node_tombstones: Vec<Provenance>,
}

/// Immutable owned logical root. Caller-retained snapshot count is not an
/// engine budget claim; each snapshot is bounded by its model's limits.
#[derive(Clone, Debug)]
pub struct Snapshot {
    generation: u64,
    records: BTreeMap<Entity, Record>,
    fences: BTreeMap<Key, Fence>,
    tombstones: BTreeMap<u128, Provenance>,
    allocated: BTreeSet<Entity>,
    limits: Limits,
}

pub struct Model {
    state: Snapshot,
    steps: usize,
}

impl Model {
    pub fn new(limits: Limits) -> Self {
        Self {
            state: Snapshot {
                generation: 0,
                records: BTreeMap::new(),
                fences: BTreeMap::new(),
                tombstones: BTreeMap::new(),
                allocated: BTreeSet::new(),
                limits,
            },
            steps: 0,
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.state.clone()
    }

    /// Relocation changes a view generation, never logical bytes or provenance.
    pub fn relocate(&mut self, generation: u64) -> Result<(), Error> {
        self.maintenance_admission(generation)?;
        self.state.generation = generation;
        self.steps += 1;
        Ok(())
    }

    /// A node tombstone may disappear only after every stored incident row.
    /// Its identity remains allocated, independent of object inventory or fences.
    pub fn drop_node_tombstone(&mut self, generation: u64, id: u128) -> Result<(), Error> {
        self.maintenance_admission(generation)?;
        if !self.state.tombstones.contains_key(&id) {
            return Err(Error::State("missing_tombstone"));
        }
        if self.state.records.values().any(|record| matches!(record.image.shape, Shape::Relationship { source, target, .. } if source == id || target == id)) {
            return Err(Error::State("retained_incident_relationship"));
        }
        self.state.tombstones.remove(&id);
        self.state.generation = generation;
        self.steps += 1;
        Ok(())
    }

    fn maintenance_admission(&self, generation: u64) -> Result<(), Error> {
        if self.steps >= self.state.limits.max_steps {
            return Err(Error::Limit("steps"));
        }
        if generation <= self.state.generation {
            return Err(Error::State("generation"));
        }
        Ok(())
    }

    /// One operation is atomic on error. This does not model mixed batches.
    pub fn apply(&mut self, generation: u64, operation: Operation) -> Result<Outcome, Error> {
        if self.steps >= self.state.limits.max_steps {
            return Err(Error::Limit("steps"));
        }
        validate(&operation, self.state.limits)?;
        let mut candidate = self.state.clone();
        let outcome = candidate.transition(generation, operation)?;
        self.state = candidate;
        self.steps += 1;
        Ok(outcome)
    }
}

impl Snapshot {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Raw directory lookup. Relationship liveness also requires both endpoints.
    pub fn lookup(&self, entity: Entity) -> Option<&Record> {
        self.records.get(&entity)
    }

    pub fn fence(&self, key: &Key) -> Option<&Fence> {
        self.fences.get(key)
    }

    pub fn node_range(
        &self,
        start: u128,
        end: Option<u128>,
        capacity: usize,
    ) -> Result<Vec<&Record>, Error> {
        self.record_range(Kind::Node, start, end, capacity)
    }

    pub fn relationship_range(
        &self,
        start: u128,
        end: Option<u128>,
        capacity: usize,
    ) -> Result<Vec<&Record>, Error> {
        self.record_range(Kind::Relationship, start, end, capacity)
    }

    pub fn key_range(
        &self,
        start: &Key,
        end: Option<&Key>,
        capacity: usize,
    ) -> Result<Vec<(&Key, &Fence)>, Error> {
        if end.is_some_and(|end| end < start) {
            return Err(Error::Invalid("range"));
        }
        if capacity > self.limits.max_fences {
            return Err(Error::Limit("query_capacity"));
        }
        Ok(self
            .fences
            .iter()
            .filter(|(key, _)| *key >= start && end.is_none_or(|upper| *key < upper))
            .take(capacity)
            .collect())
    }

    pub fn label_members(
        &self,
        label: u64,
        start: u128,
        end: Option<u128>,
        capacity: usize,
    ) -> Result<Vec<u128>, Error> {
        self.members(Kind::Node, label, start, end, capacity)
    }

    /// Raw membership, including rows hidden by an endpoint tombstone.
    pub fn type_members(
        &self,
        rel_type: u64,
        start: u128,
        end: Option<u128>,
        capacity: usize,
    ) -> Result<Vec<u128>, Error> {
        self.members(Kind::Relationship, rel_type, start, end, capacity)
    }

    fn members(
        &self,
        kind: Kind,
        symbol: u64,
        start: u128,
        end: Option<u128>,
        capacity: usize,
    ) -> Result<Vec<u128>, Error> {
        query_bounds(start, end, capacity, self.limits.max_entities)?;
        Ok(self
            .records
            .iter()
            .filter(|(entity, record)| {
                entity.kind == kind
                    && entity.id >= start
                    && end.is_none_or(|upper| entity.id < upper)
                    && match &record.image.shape {
                        Shape::Node { labels } => labels.contains(&symbol),
                        Shape::Relationship { rel_type, .. } => *rel_type == symbol,
                    }
            })
            .take(capacity)
            .map(|(entity, _)| entity.id)
            .collect())
    }

    /// Endpoint visibility is evaluated before applying the output capacity.
    pub fn observable_relationships(
        &self,
        start: u128,
        end: Option<u128>,
        capacity: usize,
    ) -> Result<Vec<&Record>, Error> {
        query_bounds(start, end, capacity, self.limits.max_entities)?;
        Ok(self.records.iter().filter(|(entity, record)| {
            entity.kind == Kind::Relationship && entity.id >= start && end.is_none_or(|upper| entity.id < upper)
                && matches!(record.image.shape, Shape::Relationship { source, target, .. } if self.node_live(source) && self.node_live(target))
        }).take(capacity).map(|(_, record)| record).collect())
    }

    fn record_range(
        &self,
        kind: Kind,
        start: u128,
        end: Option<u128>,
        capacity: usize,
    ) -> Result<Vec<&Record>, Error> {
        query_bounds(start, end, capacity, self.limits.max_entities)?;
        Ok(self
            .records
            .iter()
            .filter(|(entity, _)| {
                entity.kind == kind
                    && entity.id >= start
                    && end.is_none_or(|upper| entity.id < upper)
            })
            .take(capacity)
            .map(|(_, record)| record)
            .collect())
    }

    /// Compare exact logical rows, including order and every provenance field.
    /// Observations are never sorted, deduplicated or completed by this oracle.
    pub fn check(&self, observed: &Observation) -> Result<(), Difference> {
        compare::check(&self.observation(), observed)
    }

    pub fn observation(&self) -> Observation {
        let mut nodes = Vec::new();
        let mut relationships = Vec::new();
        let mut labels = BTreeSet::new();
        let mut types = BTreeSet::new();
        for (entity, record) in &self.records {
            match &record.image.shape {
                Shape::Node { labels: ids } => {
                    nodes.push(record.clone());
                    labels.extend(ids.iter().map(|label| (*label, entity.id)));
                }
                Shape::Relationship { rel_type, .. } => {
                    relationships.push(record.clone());
                    types.insert((*rel_type, entity.id));
                }
            }
        }
        Observation {
            generation: self.generation,
            nodes,
            relationships,
            fences: self.fences.values().cloned().collect(),
            labels: labels.into_iter().collect(),
            types: types.into_iter().collect(),
            node_tombstones: self.tombstones.values().cloned().collect(),
        }
    }

    fn node_live(&self, id: u128) -> bool {
        self.records.contains_key(&Entity {
            kind: Kind::Node,
            id,
        })
    }

    fn transition(&mut self, generation: u64, operation: Operation) -> Result<Outcome, Error> {
        let old_fence = operation.key.as_ref().and_then(|key| self.fences.get(key));
        let old_record = self.records.get(&operation.incarnation);
        if operation.kind != OperationKind::Cypher {
            if let Some(fence) = old_fence {
                if matches!(operation.expected, Expected::Entity(entity) if entity != fence.provenance.incarnation)
                {
                    return Err(Error::State("incarnation"));
                }
                if operation.revision < fence.provenance.installed_revision {
                    return Err(Error::State("stale"));
                }
                if operation.revision == fence.provenance.installed_revision {
                    let expected = provenance(&operation, fence.provenance.original_generation);
                    let same_image = match (&operation.image, old_record) {
                        (Some(image), Some(record)) => image == &record.image,
                        (None, None) => true,
                        _ => false,
                    };
                    return if fence.provenance == expected
                        && same_image
                        && fence.canonical.as_ref()
                            == operation.image.as_ref().map(|image| &image.canonical)
                    {
                        Ok(Outcome::Replay(fence.provenance.clone()))
                    } else {
                        Err(Error::State("replay_conflict"))
                    };
                }
                match operation.kind {
                    OperationKind::Create => return Err(Error::State("used_key")),
                    OperationKind::Put | OperationKind::Delete if fence.canonical.is_none() => {
                        return Err(Error::State("deleted_key"));
                    }
                    OperationKind::Recreate if fence.canonical.is_some() => {
                        return Err(Error::State("not_deleted"));
                    }
                    OperationKind::Recreate
                        if operation.expected
                            != Expected::Deletion(fence.provenance.installed_revision) =>
                    {
                        return Err(Error::State("deletion_revision"));
                    }
                    _ => {}
                }
            } else if operation.kind != OperationKind::Create {
                return Err(Error::State("missing_key"));
            }
        }
        let fresh = matches!(
            operation.kind,
            OperationKind::Create | OperationKind::Recreate
        ) || operation.expected == Expected::Absent;
        if fresh {
            if self.allocated.contains(&operation.incarnation) {
                return Err(Error::State("reused_identity"));
            }
            if self.allocated.len() >= self.limits.max_entities {
                return Err(Error::Limit("entities"));
            }
            if operation.kind == OperationKind::Cypher && old_fence.is_some() {
                return Err(Error::State("used_key"));
            }
        } else {
            let old = old_record.ok_or(Error::State("missing_entity"))?;
            if old.provenance.key != operation.key {
                return Err(Error::State("entity_key"));
            }
            if operation.kind == OperationKind::Cypher {
                if operation.image.as_ref() == Some(&old.image) {
                    return Ok(Outcome::NoOp);
                }
                let revision = old
                    .provenance
                    .installed_revision
                    .checked_add(1)
                    .ok_or(Error::State("revision_overflow"))?;
                if operation.revision != revision {
                    return Err(Error::State("cypher_revision"));
                }
            }
            if let (
                Shape::Relationship {
                    source,
                    target,
                    rel_type,
                },
                Some(Image {
                    shape:
                        Shape::Relationship {
                            source: next_source,
                            target: next_target,
                            rel_type: next_type,
                        },
                    ..
                }),
            ) = (&old.image.shape, &operation.image)
                && (source, target, rel_type) != (next_source, next_target, next_type)
            {
                return Err(Error::State("relationship_topology"));
            }
        }
        if let Some(Image {
            shape: Shape::Relationship { source, target, .. },
            ..
        }) = &operation.image
            && (!self.node_live(*source) || !self.node_live(*target))
        {
            return Err(Error::State("missing_endpoint"));
        }
        if operation.image.is_none()
            && operation.incarnation.kind == Kind::Node
            && operation.delete_mode == Some(DeleteMode::Restrict)
        {
            let blocked = self
                .records
                .values()
                .any(|record| match record.image.shape {
                    Shape::Relationship { source, target, .. } => {
                        (source == operation.incarnation.id || target == operation.incarnation.id)
                            && self.node_live(source)
                            && self.node_live(target)
                    }
                    _ => false,
                });
            if blocked {
                return Err(Error::State("incident_relationship"));
            }
        }
        if generation <= self.generation {
            return Err(Error::State("generation"));
        }
        if operation.key.is_some()
            && old_fence.is_none()
            && self.fences.len() >= self.limits.max_fences
        {
            return Err(Error::Limit("fences"));
        }
        let provenance = provenance(&operation, generation);
        if let Some(key) = operation.key {
            self.fences.insert(
                key,
                Fence {
                    provenance: provenance.clone(),
                    canonical: operation
                        .image
                        .as_ref()
                        .map(|image| image.canonical.clone()),
                },
            );
        }
        if let Some(image) = operation.image {
            self.allocated.insert(operation.incarnation);
            self.records.insert(
                operation.incarnation,
                Record {
                    image,
                    provenance: provenance.clone(),
                },
            );
        } else {
            self.records.remove(&operation.incarnation);
            if operation.incarnation.kind == Kind::Node {
                self.tombstones
                    .insert(operation.incarnation.id, provenance.clone());
            }
        }
        self.generation = generation;
        Ok(Outcome::Changed(provenance))
    }
}

fn provenance(operation: &Operation, generation: u64) -> Provenance {
    Provenance {
        version: 1,
        operation: operation.kind,
        key: operation.key.clone(),
        requested_revision: operation.revision,
        installed_revision: operation.revision,
        expected: operation.expected,
        incarnation: operation.incarnation,
        delete_mode: operation.delete_mode,
        original_generation: generation,
    }
}

fn validate(operation: &Operation, limits: Limits) -> Result<(), Error> {
    if operation.incarnation.id == 0 || operation.revision == 0 {
        return Err(Error::Invalid("identity_or_revision"));
    }
    if let Some(key) = &operation.key {
        if key.kind != operation.incarnation.kind
            || std::str::from_utf8(&key.namespace).is_err()
            || std::str::from_utf8(&key.key).is_err()
        {
            return Err(Error::Invalid("key"));
        }
        if key
            .namespace
            .len()
            .checked_add(key.key.len())
            .is_none_or(|bytes| bytes > limits.max_key_bytes)
        {
            return Err(Error::Limit("key_bytes"));
        }
    } else if operation.kind != OperationKind::Cypher {
        return Err(Error::Invalid("structured_key"));
    }
    let image_present = operation.image.is_some();
    if image_present == operation.delete_mode.is_some() {
        return Err(Error::Invalid("delete_mode"));
    }
    let expected_valid = match operation.kind {
        OperationKind::Create => operation.expected == Expected::Absent && image_present,
        OperationKind::Put => {
            operation.expected == Expected::Entity(operation.incarnation) && image_present
        }
        OperationKind::Delete => {
            operation.expected == Expected::Entity(operation.incarnation) && !image_present
        }
        OperationKind::Recreate => {
            matches!(operation.expected, Expected::Deletion(revision) if revision > 0)
                && image_present
        }
        OperationKind::Cypher => {
            operation.expected == Expected::Entity(operation.incarnation)
                || (operation.expected == Expected::Absent
                    && image_present
                    && operation.revision == 1)
        }
    };
    if !expected_valid {
        return Err(Error::Invalid("operation_precondition"));
    }
    if let Some(image) = &operation.image {
        if image.canonical.len() > limits.max_image_bytes {
            return Err(Error::Limit("image_bytes"));
        }
        match &image.shape {
            Shape::Node { labels } => {
                if operation.incarnation.kind != Kind::Node
                    || labels.contains(&0)
                    || !labels.windows(2).all(|pair| pair.first() < pair.get(1))
                {
                    return Err(Error::Invalid("node_shape"));
                }
                if labels.len() > limits.max_labels_per_node {
                    return Err(Error::Limit("labels"));
                }
            }
            Shape::Relationship {
                source,
                target,
                rel_type,
            } => {
                if operation.incarnation.kind != Kind::Relationship
                    || *source == 0
                    || *target == 0
                    || *rel_type == 0
                {
                    return Err(Error::Invalid("relationship_shape"));
                }
            }
        }
    }
    Ok(())
}

fn query_bounds(
    start: u128,
    end: Option<u128>,
    capacity: usize,
    limit: usize,
) -> Result<(), Error> {
    if end.is_some_and(|end| end < start) {
        return Err(Error::Invalid("range"));
    }
    if capacity > limit {
        return Err(Error::Limit("query_capacity"));
    }
    Ok(())
}
