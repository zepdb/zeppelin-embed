//! Directed ZE-36 identity, incarnation and retry observations against the
//! composed independent oracle.
//!
//! The production probe reports primitive facts only: the identity, revision
//! and generation of every receipt, the rejection name of every refusal, and
//! the visible rows of two expansions after every batch. This module restates
//! the same script independently, derives the expected answer from the oracle
//! crate, and compares the two without repairing either side.

use super::coverage::CoverageRegistry;
use std::collections::BTreeSet;
use zeppelin_embed::graph_identity_test_support::{
    IdentityProbeReport, IdentityState, ObservedAdjacency,
};
use zeppelin_embed_adversarial_oracle::graph_adjacency_store::{AdjacencyRow, Direction};
use zeppelin_embed_adversarial_oracle::graph_identity::{
    Batch, Expectation, Framing, Sequence, Step, Watch, WatchedRows,
};
use zeppelin_embed_adversarial_oracle::graph_key_lifecycle::{
    Action, Expected, Observation, Origin, Record, Rejection,
};

/// Key ordinals shared with the production script.
const KEY_FIRST: u32 = 1;
const KEY_PEER: u32 = 2;
const KEY_EDGE: u32 = 3;
const KEY_SECOND_EDGE: u32 = 4;

/// Relationship type ordinal expected for the script's single type name.
const LINKS: u64 = 1;

/// Keys whose body must have refused at least one scheduled failure.
const FIRED: &[&str] = &[
    "property-graph.identity.incarnation",
    "property-graph.identity.revision",
    "property-graph.identity.relationship",
    "property-graph.identity.indeterminate",
];

/// Batch holding the pre-deletion generation a retained reader keeps.
const RETAINED_BATCH: usize = 2;
/// Final batch of the script.
const FINAL_BATCH: usize = 6;

fn node_step(key: u32, action: Action, fresh_id: u128) -> Step {
    Step {
        key,
        action,
        fresh_id,
        framing: Framing::Node,
    }
}

fn edge_step(key: u32, action: Action, fresh_id: u128) -> Step {
    Step {
        key,
        action,
        fresh_id,
        framing: Framing::Relationship {
            source: KEY_FIRST,
            target: KEY_PEER,
            relationship_type: LINKS,
        },
    }
}

/// The independent description of the script the probe runs. Only the
/// identities the engine allocated are taken from the observation.
fn sequence(state: &IdentityState) -> Result<Sequence, String> {
    let identity = |index: usize| {
        state
            .outcomes
            .get(index)
            .map(|outcome| outcome.entity)
            .ok_or_else(|| format!("ZE-36 probe reported no outcome {index}"))
    };
    let first = identity(0)?;
    let peer = identity(1)?;
    let edge = identity(2)?;
    let recreated = identity(8)?;
    let second_edge = identity(9)?;
    let install = |first: u128, peer: u128, edge: u128| {
        vec![
            node_step(
                KEY_FIRST,
                Action::Create {
                    revision: 1,
                    bits: 0xA1,
                },
                first,
            ),
            node_step(
                KEY_PEER,
                Action::Create {
                    revision: 1,
                    bits: 0xB1,
                },
                peer,
            ),
            edge_step(
                KEY_EDGE,
                Action::Create {
                    revision: 1,
                    bits: 0,
                },
                edge,
            ),
        ]
    };
    Ok(Sequence {
        batches: vec![
            Batch {
                generation: 2,
                steps: install(first, peer, edge),
            },
            Batch {
                generation: 2,
                steps: install(first, peer, edge),
            },
            Batch {
                generation: 3,
                steps: vec![node_step(
                    KEY_FIRST,
                    Action::Put {
                        revision: 2,
                        expected: first,
                        bits: 0xA2,
                    },
                    0,
                )],
            },
            Batch {
                generation: 4,
                steps: vec![node_step(
                    KEY_FIRST,
                    Action::Delete {
                        revision: 3,
                        expected: first,
                        detach: true,
                    },
                    0,
                )],
            },
            Batch {
                generation: 5,
                steps: vec![node_step(
                    KEY_FIRST,
                    Action::Recreate {
                        revision: 4,
                        deleted: 3,
                        bits: 0xA4,
                    },
                    recreated,
                )],
            },
            Batch {
                generation: 6,
                steps: vec![edge_step(
                    KEY_SECOND_EDGE,
                    Action::Create {
                        revision: 1,
                        bits: 0,
                    },
                    second_edge,
                )],
            },
            Batch {
                generation: 6,
                steps: vec![node_step(
                    KEY_FIRST,
                    Action::Put {
                        revision: 1,
                        expected: recreated,
                        bits: 0xA4,
                    },
                    0,
                )],
            },
        ],
        watches: vec![
            Watch {
                key: KEY_FIRST,
                direction: Direction::Outgoing,
            },
            Watch {
                key: KEY_PEER,
                direction: Direction::Incoming,
            },
        ],
    })
}

fn rejection(name: &str) -> Result<Rejection, String> {
    Ok(match name {
        "Missing" => Rejection::Missing,
        "Stale" => Rejection::Stale,
        "Conflict" => Rejection::Conflict,
        "Exists" => Rejection::Exists,
        "Deleted" => Rejection::Deleted,
        "NotDeleted" => Rejection::NotDeleted,
        "Incarnation" => Rejection::Incarnation,
        "DeletionRevision" => Rejection::DeletionRevision,
        other => return Err(format!("ZE-36 unknown observed rejection {other}")),
    })
}

/// Restates one observed receipt as a primitive record. The engine supplies the
/// identity, revision, generation and replay classification; the remaining
/// fields are the request's own, echoed from this module's script.
fn record(action: Action, entity: u128, revision: u64, generation: u64) -> Result<Record, String> {
    let (origin, expected, bits, detach) = match action {
        Action::Create { bits, .. } => (Origin::Create, Expected::Absent, Some(bits), None),
        Action::Put { expected, bits, .. } => {
            (Origin::Put, Expected::Entity(expected), Some(bits), None)
        }
        Action::Delete {
            expected, detach, ..
        } => (
            Origin::Delete,
            Expected::Entity(expected),
            None,
            Some(detach),
        ),
        Action::Recreate { deleted, bits, .. } => (
            Origin::Recreate,
            Expected::Deletion(deleted),
            Some(bits),
            None,
        ),
        Action::CypherPut { .. } | Action::CypherDelete { .. } => {
            return Err("ZE-36 script has no Cypher step".to_owned());
        }
    };
    Ok(Record {
        id: entity,
        revision,
        origin,
        expected,
        detach,
        generation,
        bits,
    })
}

fn row(observed: &ObservedAdjacency) -> AdjacencyRow {
    AdjacencyRow {
        bound_node: observed.bound_node,
        relationship_type: observed.relationship_type,
        rel: observed.rel,
        neighbor: observed.neighbor,
    }
}

fn rows(observed: &[ObservedAdjacency]) -> Vec<AdjacencyRow> {
    observed.iter().map(row).collect()
}

/// Restates the complete production observation in the oracle's vocabulary.
fn observation(sequence: &Sequence, state: &IdentityState) -> Result<Expectation, String> {
    let steps: Vec<Step> = sequence
        .batches
        .iter()
        .flat_map(|batch| batch.steps.iter().copied())
        .collect();
    if steps.len() != state.outcomes.len() {
        return Err(format!(
            "ZE-36 script has {} steps and the probe reported {} outcomes",
            steps.len(),
            state.outcomes.len()
        ));
    }
    let mut observations = Vec::with_capacity(steps.len());
    for (step, outcome) in steps.iter().zip(&state.outcomes) {
        if step.key != outcome.key {
            return Err(format!(
                "ZE-36 script key {} does not name the observed key {}",
                step.key, outcome.key
            ));
        }
        observations.push(match outcome.rejection {
            Some(name) => Observation::Rejected(rejection(name)?),
            None => {
                let record = record(
                    step.action,
                    outcome.entity,
                    outcome.revision,
                    outcome.generation,
                )?;
                if outcome.replayed {
                    Observation::Replay(record)
                } else {
                    Observation::Changed(record)
                }
            }
        });
    }
    let watched = state
        .watched
        .iter()
        .map(|watch| WatchedRows {
            batch: watch.batch,
            generation: watch.generation,
            watch: Watch {
                key: watch.key,
                direction: if watch.outgoing {
                    Direction::Outgoing
                } else {
                    Direction::Incoming
                },
            },
            rows: rows(&watch.rows),
        })
        .collect();
    Ok(Expectation {
        observations,
        watched,
    })
}

/// Proves the comparator still catches a wrong identity, a wrong generation, a
/// dropped incoming row and a reused identity.
fn controls(sequence: &Sequence, observed: &Expectation) -> Result<(), String> {
    let mut wrong_identity = observed.clone();
    let target = wrong_identity
        .observations
        .get_mut(8)
        .ok_or("ZE-36 control needs the recreation observation")?;
    if let Observation::Changed(record) = target {
        record.id = record.id.wrapping_add(1);
    } else {
        return Err("ZE-36 recreation must be a changed observation".into());
    }
    if sequence.check(&wrong_identity).is_ok() {
        return Err("ZE-36 comparator accepted a wrong recreated identity".into());
    }

    let mut wrong_generation = observed.clone();
    let target = wrong_generation
        .observations
        .get_mut(6)
        .ok_or("ZE-36 control needs the replacement observation")?;
    if let Observation::Changed(record) = target {
        record.generation = record.generation.wrapping_add(1);
    } else {
        return Err("ZE-36 replacement must be a changed observation".into());
    }
    if sequence.check(&wrong_generation).is_ok() {
        return Err("ZE-36 comparator accepted a wrong changed generation".into());
    }

    let mut dropped_incoming = observed.clone();
    let target = dropped_incoming
        .watched
        .iter_mut()
        .find(|watch| watch.watch.direction == Direction::Incoming && !watch.rows.is_empty())
        .ok_or("ZE-36 control needs a visible incoming row")?;
    target.rows.clear();
    if sequence.check(&dropped_incoming).is_ok() {
        return Err("ZE-36 comparator accepted a missing incoming row".into());
    }

    let mut reused = sequence.clone();
    let retired = match reused
        .batches
        .first()
        .and_then(|batch| batch.steps.first())
        .map(|step| step.fresh_id)
    {
        Some(identity) => identity,
        None => return Err("ZE-36 control needs the first installed identity".into()),
    };
    match reused
        .batches
        .get_mut(4)
        .and_then(|batch| batch.steps.first_mut())
    {
        Some(step) => step.fresh_id = retired,
        None => return Err("ZE-36 control needs the recreation step".into()),
    }
    if reused.expect().is_ok() {
        return Err("ZE-36 comparator accepted a reused retired identity".into());
    }
    Ok(())
}

/// Compares what a reader admitted before the deletion still observes with the
/// generation it holds, and what a later reader observes.
fn leases(sequence: &Sequence, state: &IdentityState) -> Result<(), String> {
    let watch = Watch {
        key: KEY_FIRST,
        direction: Direction::Outgoing,
    };
    let retained = sequence.watched_rows(RETAINED_BATCH, watch)?;
    if state.retained_generation != 3 {
        return Err(format!(
            "ZE-36 retained reader holds generation {} instead of 3",
            state.retained_generation
        ));
    }
    if rows(&state.retained_rows) != retained {
        return Err(format!(
            "ZE-36 retained reader rows expected={retained:?} observed={:?}",
            state.retained_rows
        ));
    }
    if state.fresh_generation <= state.retained_generation {
        return Err("ZE-36 later reader did not observe a newer generation".into());
    }
    let fresh = sequence.watched_rows(FINAL_BATCH, watch)?;
    if rows(&state.fresh_rows) != fresh {
        return Err(format!(
            "ZE-36 later reader rows expected={fresh:?} observed={:?}",
            state.fresh_rows
        ));
    }
    if state.retained_rows == state.fresh_rows {
        return Err("ZE-36 the two readers observed the same incarnation".into());
    }
    Ok(())
}

fn receipts(report: &IdentityProbeReport, coverage: &mut CoverageRegistry) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for receipt in &report.receipts {
        if !receipt.key.starts_with("property-graph.identity.")
            || !seen.insert(receipt.key)
            || receipt.clean_controls == 0
        {
            return Err(format!("invalid identity receipt {}", receipt.key));
        }
        if FIRED.contains(&receipt.key) && receipt.fires == 0 {
            return Err(format!("unrefused identity boundary {}", receipt.key));
        }
        coverage.hit(receipt.key);
    }
    if seen.len() != 5 {
        return Err("missing identity boundary receipts".into());
    }
    Ok(())
}

pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    let report = zeppelin_embed::graph_identity_test_support::run_actual_probe(seed);
    let sequence = sequence(&report.state)?;
    let observed = observation(&sequence, &report.state)?;
    sequence
        .check(&observed)
        .map_err(|difference| format!("identity sequence mismatch: {difference}"))?;
    controls(&sequence, &observed)?;
    leases(&sequence, &report.state)?;
    receipts(&report, coverage)?;
    coverage.hit("property-graph.identity.oracle.can-fire");
    Ok(())
}

#[cfg(test)]
mod tests {
    /// Binds the real probe to the composed comparator without running the
    /// whole campaign.
    #[test]
    fn identity_probe_binds_actual_receipts_to_the_composed_comparator() {
        let mut coverage = super::CoverageRegistry::default();
        super::probe(0x5a45_0036, &mut coverage).expect("identity probe");
        assert_eq!(coverage.count("property-graph.identity.replay"), 1);
        assert_eq!(coverage.count("property-graph.identity.oracle.can-fire"), 1);
    }
}
