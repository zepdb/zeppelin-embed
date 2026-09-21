//! ZE-36 keyed identity sequences over the two existing independent models.
//!
//! Per-key history is predicted by `graph_key_lifecycle::predict` and visible
//! adjacency by `graph_adjacency_store::Model`. This module composes those two
//! answers for a whole sequence of keyed actions; it classifies nothing itself,
//! repairs no observation and has no engine dependency.

use std::collections::BTreeMap;

use crate::graph_adjacency_store::{
    AdjacencyRow, Direction, EntityKind, Model, Observation as StoreObservation, ObservationPlan,
    Operation,
};
use crate::graph_key_lifecycle::{Action, Observation, Record, predict};

/// Stable identity of the composed ZE-36 comparator.
pub const IDENTITY_COMPARATOR_ID: &str = "ZE-36/keyed-identity-sequence-v1";

/// Application key, named by a small ordinal instead of engine key bytes.
pub type KeyId = u32;

/// Fixed shape of a key's entity domain, supplied by the caller's script.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Framing {
    /// The key names nodes.
    Node,
    /// The key names relationships with immutable endpoints and type.
    Relationship {
        /// Key whose current incarnation is the source.
        source: KeyId,
        /// Key whose current incarnation is the target.
        target: KeyId,
        /// Exact relationship type ordinal.
        relationship_type: u64,
    },
}

/// One keyed action, with the identity the engine reported for a fresh install.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Step {
    /// Application key this action names.
    pub key: KeyId,
    /// Complete requested action.
    pub action: Action,
    /// Identity observed when this action installs a new incarnation.
    pub fresh_id: u128,
    /// Entity domain and, for relationships, the requested endpoints.
    pub framing: Framing,
}

/// Actions submitted together and, when any of them changes state, the
/// generation the engine reported for that change.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Batch {
    /// Generation of this batch's changed work.
    pub generation: u64,
    /// Ordered actions.
    pub steps: Vec<Step>,
}

/// One adjacency question asked after every batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Watch {
    /// Key whose current incarnation is the bound node.
    pub key: KeyId,
    /// Expansion direction.
    pub direction: Direction,
}

/// Visible rows of one watch after one batch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WatchedRows {
    /// Index of the batch this answer follows.
    pub batch: usize,
    /// Generation label of that batch.
    pub generation: u64,
    /// Question that was asked.
    pub watch: Watch,
    /// Complete visible rows, in model order.
    pub rows: Vec<AdjacencyRow>,
}

/// Complete independent expectation, or the complete production observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Expectation {
    /// One entry per step, in submission order.
    pub observations: Vec<Observation>,
    /// One entry per watch per batch, in batch then watch order.
    pub watched: Vec<WatchedRows>,
}

/// A complete keyed script plus the adjacency questions asked after each batch.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Sequence {
    /// Ordered batches.
    pub batches: Vec<Batch>,
    /// Questions asked after every batch.
    pub watches: Vec<Watch>,
}

impl Sequence {
    /// Derives the complete expected observations and visible rows.
    ///
    /// # Errors
    /// Returns the model's own rejection when the supplied identities cannot
    /// describe a legal history, which includes a reused entity identity.
    pub fn expect(&self) -> Result<Expectation, String> {
        let mut history: BTreeMap<KeyId, Record> = BTreeMap::new();
        let mut model = Model::new();
        let mut observations = Vec::new();
        let mut watched = Vec::new();
        for (index, batch) in self.batches.iter().enumerate() {
            let mut operations = Vec::new();
            for step in &batch.steps {
                let prior = history.get(&step.key).copied();
                let observation = predict(prior, step.action, step.fresh_id, batch.generation);
                if let Observation::Changed(record) = observation {
                    operations.push(operation(*step, prior, record, &history)?);
                    history.insert(step.key, record);
                }
                observations.push(observation);
            }
            if !operations.is_empty() {
                model
                    .apply(batch.generation, &operations)
                    .map_err(|error| format!("ZE-36 batch {index} adjacency input: {error:?}"))?;
            }
            let store = model
                .snapshot()
                .observation(&ObservationPlan::default())
                .map_err(|error| format!("ZE-36 batch {index} adjacency model: {error:?}"))?;
            for watch in &self.watches {
                let bound = history.get(&watch.key).map_or(0, |record| record.id);
                watched.push(WatchedRows {
                    batch: index,
                    generation: batch.generation,
                    watch: *watch,
                    rows: rows(&store, watch.direction, bound),
                });
            }
        }
        Ok(Expectation {
            observations,
            watched,
        })
    }

    /// Compares the complete production observation without normalizing it.
    ///
    /// # Errors
    /// Returns the first disagreement, or the model rejection from `expect`.
    pub fn check(&self, observed: &Expectation) -> Result<(), String> {
        let expected = self.expect()?;
        if expected.observations.len() != observed.observations.len() {
            return Err(format!(
                "{IDENTITY_COMPARATOR_ID} observation count expected={} observed={}",
                expected.observations.len(),
                observed.observations.len()
            ));
        }
        for (index, (expected, observed)) in expected
            .observations
            .iter()
            .zip(&observed.observations)
            .enumerate()
        {
            if expected != observed {
                return Err(format!(
                    "{IDENTITY_COMPARATOR_ID} observation {index} expected={expected:?} observed={observed:?}"
                ));
            }
        }
        if expected.watched.len() != observed.watched.len() {
            return Err(format!(
                "{IDENTITY_COMPARATOR_ID} watch count expected={} observed={}",
                expected.watched.len(),
                observed.watched.len()
            ));
        }
        for (index, (expected, observed)) in
            expected.watched.iter().zip(&observed.watched).enumerate()
        {
            if expected != observed {
                return Err(format!(
                    "{IDENTITY_COMPARATOR_ID} watch {index} expected={expected:?} observed={observed:?}"
                ));
            }
        }
        Ok(())
    }

    /// Visible rows this sequence expects for one batch and one watch.
    ///
    /// # Errors
    /// Returns the model rejection from `expect`, or an unknown question.
    pub fn watched_rows(&self, batch: usize, watch: Watch) -> Result<Vec<AdjacencyRow>, String> {
        self.expect()?
            .watched
            .into_iter()
            .find(|entry| entry.batch == batch && entry.watch == watch)
            .map(|entry| entry.rows)
            .ok_or_else(|| format!("{IDENTITY_COMPARATOR_ID} has no watch {watch:?} at {batch}"))
    }
}

fn rows(store: &StoreObservation, direction: Direction, bound: u128) -> Vec<AdjacencyRow> {
    let all = match direction {
        Direction::Outgoing => &store.visible_outgoing,
        Direction::Incoming => &store.visible_incoming,
    };
    all.iter()
        .filter(|row| row.bound_node == bound)
        .copied()
        .collect()
}

fn operation(
    step: Step,
    prior: Option<Record>,
    record: Record,
    history: &BTreeMap<KeyId, Record>,
) -> Result<Operation, String> {
    let installed = prior.is_none_or(|old| old.bits.is_none());
    match step.framing {
        Framing::Node => Ok(if record.bits.is_none() {
            Operation::DeleteNode {
                id: record.id,
                detach: record.detach == Some(true),
            }
        } else if installed {
            Operation::CreateNode { id: record.id }
        } else {
            Operation::PropertyOnly {
                entity_kind: EntityKind::Node,
                id: record.id,
            }
        }),
        Framing::Relationship {
            source,
            target,
            relationship_type,
        } => {
            if record.bits.is_none() {
                return Ok(Operation::DeleteRelationship { rel: record.id });
            }
            if !installed {
                return Ok(Operation::PropertyOnly {
                    entity_kind: EntityKind::Relationship,
                    id: record.id,
                });
            }
            Ok(Operation::CreateRelationship {
                rel: record.id,
                source: endpoint(history, source)?,
                target: endpoint(history, target)?,
                relationship_type,
            })
        }
    }
}

fn endpoint(history: &BTreeMap<KeyId, Record>, key: KeyId) -> Result<u128, String> {
    history
        .get(&key)
        .map(|record| record.id)
        .ok_or_else(|| format!("{IDENTITY_COMPARATOR_ID} endpoint key {key} has no incarnation"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script() -> Sequence {
        Sequence {
            batches: vec![
                Batch {
                    generation: 1,
                    steps: vec![
                        Step {
                            key: 1,
                            action: Action::Create {
                                revision: 1,
                                bits: 0x11,
                            },
                            fresh_id: 10,
                            framing: Framing::Node,
                        },
                        Step {
                            key: 2,
                            action: Action::Create {
                                revision: 1,
                                bits: 0x22,
                            },
                            fresh_id: 20,
                            framing: Framing::Node,
                        },
                        Step {
                            key: 3,
                            action: Action::Create {
                                revision: 1,
                                bits: 0x33,
                            },
                            fresh_id: 30,
                            framing: Framing::Relationship {
                                source: 1,
                                target: 2,
                                relationship_type: 7,
                            },
                        },
                    ],
                },
                Batch {
                    generation: 2,
                    steps: vec![Step {
                        key: 1,
                        action: Action::Delete {
                            revision: 2,
                            expected: 10,
                            detach: true,
                        },
                        fresh_id: 0,
                        framing: Framing::Node,
                    }],
                },
                Batch {
                    generation: 3,
                    steps: vec![Step {
                        key: 1,
                        action: Action::Recreate {
                            revision: 3,
                            deleted: 2,
                            bits: 0x44,
                        },
                        fresh_id: 11,
                        framing: Framing::Node,
                    }],
                },
            ],
            watches: vec![
                Watch {
                    key: 1,
                    direction: Direction::Outgoing,
                },
                Watch {
                    key: 2,
                    direction: Direction::Incoming,
                },
            ],
        }
    }

    #[test]
    fn identity_sequence_oracle_hides_detached_rows_and_never_reuses_an_identity() {
        let sequence = script();
        let expected = sequence.expect().expect("composed expectation");
        assert_eq!(expected.observations.len(), 5);
        assert_eq!(
            expected.watched[0].rows,
            vec![AdjacencyRow {
                bound_node: 10,
                relationship_type: 7,
                rel: 30,
                neighbor: 20,
            }]
        );
        assert_eq!(
            expected.watched[1].rows,
            vec![AdjacencyRow {
                bound_node: 20,
                relationship_type: 7,
                rel: 30,
                neighbor: 10,
            }]
        );
        // DETACH hides the row in both directions.
        assert!(expected.watched[2].rows.is_empty());
        assert!(expected.watched[3].rows.is_empty());
        // The recreated incarnation inherits no adjacency.
        assert!(expected.watched[4].rows.is_empty());
        assert!(sequence.check(&expected).is_ok());

        let mut wrong_identity = script();
        wrong_identity.batches[2].steps[0].fresh_id = 10;
        assert!(
            wrong_identity
                .expect()
                .expect_err("CAN FIRE")
                .contains("reused_node")
        );

        let mut observed = expected.clone();
        observed.watched[0].rows.clear();
        assert!(
            sequence
                .check(&observed)
                .expect_err("CAN FIRE")
                .contains("watch 0")
        );

        let mut observed = expected;
        observed.observations[4] = Observation::NoOp;
        assert!(
            sequence
                .check(&observed)
                .expect_err("CAN FIRE")
                .contains("observation 4")
        );
    }

    #[test]
    fn identity_sequence_oracle_answers_one_watch_by_batch() {
        let sequence = script();
        let watch = Watch {
            key: 1,
            direction: Direction::Outgoing,
        };
        assert_eq!(sequence.watched_rows(0, watch).expect("batch 0").len(), 1);
        assert!(sequence.watched_rows(1, watch).expect("batch 1").is_empty());
        assert!(
            sequence
                .watched_rows(9, watch)
                .expect_err("CAN FIRE")
                .contains("has no watch")
        );
    }
}
