//! PG10: atomic private staging outcomes, using primitive independent history.
//! This does not model durable publication, a WAL, storage or physical search.
use crate::graph_key_lifecycle::{Action, Observation, Record, Rejection};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Request {
    pub key: u16,
    pub action: Action,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refusal {
    Duplicate,
    Lifecycle(Rejection),
    Allocator,
    Generation,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Disposition {
    NoOp,
    Replay,
    Changed,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Accepted {
    pub disposition: Disposition,
    pub high_water: u128,
    pub receipts: Vec<(u16, Record, bool)>,
    pub changes: Vec<(u16, Record)>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Outcome {
    Rejected(Refusal),
    Accepted(Accepted),
}

pub fn predict(
    base: &[(u16, Record)],
    requests: &[Request],
    high: u128,
    generation: u64,
) -> Outcome {
    use crate::graph_key_lifecycle::predict;
    let mut targets = std::collections::BTreeSet::new();
    let mut identities = std::collections::BTreeSet::new();
    for request in requests {
        let current = base
            .iter()
            .find(|(key, _)| *key == request.key)
            .map(|(_, r)| *r);
        let expected = match request.action {
            Action::Put { expected, .. } | Action::Delete { expected, .. } => Some(expected),
            _ => current.map(|r| r.id),
        };
        if !targets.insert(request.key) || expected.is_some_and(|id| !identities.insert(id)) {
            return Outcome::Rejected(Refusal::Duplicate);
        }
    }
    let mut decisions = Vec::new();
    for request in requests {
        let current = base
            .iter()
            .find(|(key, _)| *key == request.key)
            .map(|(_, r)| *r);
        // Placeholder identity/generation permit full primitive classification;
        // they are replaced only after all requests pass their base conditions.
        let decision = predict(current, request.action, 1, 1);
        if let Observation::Rejected(reason) = decision {
            return Outcome::Rejected(Refusal::Lifecycle(reason));
        }
        decisions.push(decision);
    }
    let changed = decisions
        .iter()
        .any(|d| matches!(d, Observation::Changed(_)));
    let generation = if changed {
        match generation.checked_add(1) {
            Some(value) => value,
            None => return Outcome::Rejected(Refusal::Generation),
        }
    } else {
        generation
    };
    let mut accepted = Accepted {
        disposition: if changed {
            Disposition::Changed
        } else if requests.is_empty() {
            Disposition::NoOp
        } else {
            Disposition::Replay
        },
        high_water: high,
        receipts: Vec::new(),
        changes: Vec::new(),
    };
    for (request, decision) in requests.iter().zip(decisions) {
        match decision {
            Observation::Changed(mut record) => {
                if matches!(
                    request.action,
                    Action::Create { .. } | Action::Recreate { .. }
                ) {
                    let Some(next) = accepted.high_water.checked_add(1) else {
                        return Outcome::Rejected(Refusal::Allocator);
                    };
                    accepted.high_water = next;
                    record.id = next;
                }
                record.generation = generation;
                accepted.receipts.push((request.key, record, false));
                accepted.changes.push((request.key, record));
            }
            Observation::Replay(record) => accepted.receipts.push((request.key, record, true)),
            Observation::NoOp => {}
            Observation::Rejected(reason) => return Outcome::Rejected(Refusal::Lifecycle(reason)),
        }
    }
    Outcome::Accepted(accepted)
}

pub fn check(
    base: &[(u16, Record)],
    requests: &[Request],
    high: u128,
    generation: u64,
    observed: &Outcome,
) -> Result<(), String> {
    let expected = predict(base, requests, high, generation);
    if expected == *observed {
        Ok(())
    } else {
        Err(format!(
            "PG10 private staging expected={expected:?} observed={observed:?} requests={requests:?}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph_key_lifecycle::{Expected, Origin};
    #[test]
    fn primitive_staging_oracle_refuses_erased_changes_and_rewritten_replay_generation() {
        let old = Record {
            id: (1u128 << 100) + 9,
            revision: 4,
            origin: Origin::Create,
            expected: Expected::Absent,
            detach: None,
            generation: 5,
            bits: Some(0x8000000000000000),
        };
        let base = [(1, old)];
        let requests = [
            Request {
                key: 1,
                action: Action::Create {
                    revision: 4,
                    bits: old.bits.unwrap(),
                },
            },
            Request {
                key: 2,
                action: Action::Create {
                    revision: 1,
                    bits: 0x7ff8000000001234,
                },
            },
        ];
        let new = Record {
            id: old.id + 1,
            revision: 1,
            origin: Origin::Create,
            expected: Expected::Absent,
            detach: None,
            generation: 8,
            bits: Some(0x7ff8000000001234),
        };
        let accepted = Accepted {
            disposition: Disposition::Changed,
            high_water: new.id,
            receipts: vec![(1, old, true), (2, new, false)],
            changes: vec![(2, new)],
        };
        check(
            &base,
            &requests,
            old.id,
            7,
            &Outcome::Accepted(accepted.clone()),
        )
        .unwrap();
        let mut corrupt = accepted.clone();
        corrupt.changes.clear();
        assert!(
            check(&base, &requests, old.id, 7, &Outcome::Accepted(corrupt)).is_err(),
            "PG10 CAN FIRE erased participant"
        );
        let mut corrupt = accepted;
        corrupt.receipts[0].1.generation = 8;
        assert!(
            check(&base, &requests, old.id, 7, &Outcome::Accepted(corrupt)).is_err(),
            "PG10 CAN FIRE rewritten replay generation"
        );
    }
}
