use super::*;
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Image {
    Node {
        labels: BTreeSet<String>,
        properties: BTreeMap<String, Property>,
        text: Option<String>,
        vector: Option<Vec<u32>>,
    },
    Relationship {
        source: u128,
        target: u128,
        relationship_type: String,
        properties: BTreeMap<String, Property>,
    },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    Create,
    Put,
    Delete,
    Recreate,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Expectation {
    Absent,
    Entity(u128),
    Deletion(u64),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mutation {
    pub key: Key,
    pub operation: Operation,
    pub revision: u64,
    pub expected: Expectation,
    pub detach: bool,
    pub image: Option<Image>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct History {
    pub id: u128,
    pub generation: u64,
    pub last: Mutation,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Rejection {
    Invalid,
    Duplicate,
    Missing,
    Stale,
    Conflict,
    Exists,
    Deleted,
    NotDeleted,
    Incarnation,
    DeletionRevision,
    Endpoint,
    Restrict,
    Overflow,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Receipt {
    pub id: u128,
    pub generation: u64,
    pub revision: u64,
    pub replayed: bool,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchOutcome {
    pub generation: u64,
    pub changed: bool,
    pub receipts: Vec<Receipt>,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Graph {
    pub generation: u64,
    pub node_high: u128,
    pub relationship_high: u128,
    pub history: BTreeMap<Key, History>,
}
impl Graph {
    /// Atomically evaluates every request against the pre-batch logical state.
    /// Private cloned state is discarded on any rejection.
    pub fn apply(&mut self, requests: &[Mutation]) -> Result<BatchOutcome, Rejection> {
        let mut keys = BTreeSet::new();
        let mut targets = BTreeSet::new();
        for request in requests {
            validate(request)?;
            if !keys.insert(request.key.clone()) {
                return Err(Rejection::Duplicate);
            }
            if let Some(old) = self.history.get(&request.key)
                && !targets.insert((request.key.kind, old.id))
            {
                return Err(Rejection::Duplicate);
            }
        }
        let mut prepared = self.clone();
        let mut receipts = Vec::new();
        let mut changed = Vec::new();
        for request in requests {
            let old = self.history.get(&request.key);
            let replay = if let Some(old) = old {
                if matches!(request.expected,Expectation::Entity(id) if id!=old.id) {
                    return Err(Rejection::Incarnation);
                }
                if request.revision < old.last.revision {
                    return Err(Rejection::Stale);
                }
                if request.revision == old.last.revision {
                    if request != &old.last {
                        return Err(Rejection::Conflict);
                    }
                    true
                } else {
                    match (old.last.image.is_some(), request.operation) {
                        (true, Operation::Create) => {
                            return Err(Rejection::Exists);
                        }
                        (true, Operation::Recreate) => return Err(Rejection::NotDeleted),
                        (false, Operation::Recreate)
                            if request.expected != Expectation::Deletion(old.last.revision) =>
                        {
                            return Err(Rejection::DeletionRevision);
                        }
                        (false, Operation::Create | Operation::Put | Operation::Delete) => {
                            return Err(Rejection::Deleted);
                        }
                        _ => {}
                    }
                    if request.operation == Operation::Put
                        && let (
                            Some(Image::Relationship {
                                source: a,
                                target: b,
                                relationship_type: c,
                                ..
                            }),
                            Some(Image::Relationship {
                                source: x,
                                target: y,
                                relationship_type: z,
                                ..
                            }),
                        ) = (&old.last.image, &request.image)
                        && (a, b, c) != (x, y, z)
                    {
                        return Err(Rejection::Endpoint);
                    }
                    false
                }
            } else {
                if request.operation != Operation::Create {
                    return Err(Rejection::Missing);
                }
                false
            };
            if replay {
                let old = old.ok_or(Rejection::Missing)?;
                receipts.push(Receipt {
                    id: old.id,
                    generation: old.generation,
                    revision: old.last.revision,
                    replayed: true,
                });
                continue;
            }
            let fresh = old.is_none() || request.operation == Operation::Recreate;
            let id = if fresh {
                let high = match request.key.kind {
                    Kind::Node => &mut prepared.node_high,
                    Kind::Relationship => &mut prepared.relationship_high,
                };
                *high = high.checked_add(1).ok_or(Rejection::Overflow)?;
                *high
            } else {
                old.ok_or(Rejection::Missing)?.id
            };
            prepared.history.insert(
                request.key.clone(),
                History {
                    id,
                    generation: 0,
                    last: request.clone(),
                },
            );
            changed.push(request.key.clone());
            receipts.push(Receipt {
                id,
                generation: 0,
                revision: request.revision,
                replayed: false,
            });
        }
        let live_nodes = prepared
            .history
            .iter()
            .filter_map(|(key, row)| {
                (key.kind == Kind::Node && row.last.image.is_some()).then_some(row.id)
            })
            .collect::<BTreeSet<_>>();
        for key in &changed {
            let row = prepared.history.get(key).ok_or(Rejection::Missing)?;
            if let Some(Image::Relationship { source, target, .. }) = &row.last.image
                && (!live_nodes.contains(source) || !live_nodes.contains(target))
            {
                return Err(Rejection::Endpoint);
            }
            if key.kind == Kind::Node && row.last.operation == Operation::Delete && !row.last.detach
            {
                let admitted_nodes = self
                    .history
                    .iter()
                    .filter_map(|(key, record)| {
                        (key.kind == Kind::Node && record.last.image.is_some()).then_some(record.id)
                    })
                    .collect::<BTreeSet<_>>();
                let incident=self.history.iter().any(|(key,edge)|matches!(&edge.last.image,Some(Image::Relationship{source,target,..}) if (*source==row.id||*target==row.id)&&admitted_nodes.contains(source)&&admitted_nodes.contains(target))&&prepared.history.get(key).is_some_and(|record|record.last.image.is_some()));
                if incident {
                    return Err(Rejection::Restrict);
                }
            }
        }
        if !changed.is_empty() {
            prepared.generation = self.generation.checked_add(1).ok_or(Rejection::Overflow)?;
            for key in changed {
                prepared
                    .history
                    .get_mut(&key)
                    .ok_or(Rejection::Missing)?
                    .generation = prepared.generation;
            }
            for receipt in &mut receipts {
                if !receipt.replayed {
                    receipt.generation = prepared.generation;
                }
            }
        }
        let result = BatchOutcome {
            generation: prepared.generation,
            changed: receipts.iter().any(|r| !r.replayed),
            receipts,
        };
        *self = prepared;
        Ok(result)
    }
    /// Complete logical view. Retained incident records never expose a dead endpoint.
    pub fn snapshot(&self) -> Snapshot {
        let mut out = Snapshot::default();
        for (key, row) in &self.history {
            if let Some(Image::Node {
                labels,
                properties,
                text,
                vector,
            }) = &row.last.image
            {
                out.nodes.push(Node {
                    id: row.id,
                    key: Some(key.clone()),
                    revision: row.last.revision,
                    generation: row.generation,
                    labels: labels.clone(),
                    properties: properties.clone(),
                    text: text.clone(),
                    vector: vector.clone(),
                });
            }
        }
        let ids = out.nodes.iter().map(|n| n.id).collect::<BTreeSet<_>>();
        for (key, row) in &self.history {
            if let Some(Image::Relationship {
                source,
                target,
                relationship_type,
                properties,
            }) = &row.last.image
                && ids.contains(source)
                && ids.contains(target)
            {
                out.relationships.push(Relationship {
                    id: row.id,
                    key: Some(key.clone()),
                    revision: row.last.revision,
                    generation: row.generation,
                    source: *source,
                    target: *target,
                    relationship_type: relationship_type.clone(),
                    properties: properties.clone(),
                });
            }
        }
        out
    }
}
fn validate(request: &Mutation) -> Result<(), Rejection> {
    if request.revision == 0 {
        return Err(Rejection::Invalid);
    }
    let form = match request.operation {
        Operation::Create => {
            request.expected == Expectation::Absent && request.image.is_some() && !request.detach
        }
        Operation::Put => {
            matches!(request.expected,Expectation::Entity(id) if id!=0)
                && request.image.is_some()
                && !request.detach
        }
        Operation::Delete => {
            matches!(request.expected,Expectation::Entity(id) if id!=0)
                && request.image.is_none()
                && (!request.detach || request.key.kind == Kind::Node)
        }
        Operation::Recreate => {
            matches!(request.expected,Expectation::Deletion(rev) if rev!=0)
                && request.image.is_some()
                && !request.detach
        }
    };
    if !form {
        return Err(Rejection::Invalid);
    }
    if let Some(image) = &request.image {
        let properties = match image {
            Image::Node {
                properties, vector, ..
            } => {
                if request.key.kind != Kind::Node
                    || vector.as_ref().is_some_and(|v| {
                        v.is_empty() || v.iter().any(|b| b & 0x7f80_0000 == 0x7f80_0000)
                    })
                {
                    return Err(Rejection::Invalid);
                }
                properties
            }
            Image::Relationship {
                source,
                target,
                properties,
                ..
            } => {
                if request.key.kind != Kind::Relationship || *source == 0 || *target == 0 {
                    return Err(Rejection::Invalid);
                }
                properties
            }
        };
        for value in properties.values() {
            if let Property::List(kind, values) = value {
                let valid = values.iter().all(|v| {
                    matches!(
                        (kind, v),
                        (Element::Bool, Scalar::Bool(_))
                            | (Element::I64, Scalar::I64(_))
                            | (Element::F64, Scalar::F64(_))
                            | (Element::String, Scalar::String(_))
                    )
                });
                if !valid || (*kind == Element::Empty && !values.is_empty()) {
                    return Err(Rejection::Invalid);
                }
            }
        }
    }
    Ok(())
}
