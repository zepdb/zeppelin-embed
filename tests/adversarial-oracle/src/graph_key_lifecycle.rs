//! PG5: primitive application-key history, with no engine types or codecs.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Origin {
    Create,
    Put,
    Delete,
    Recreate,
    Cypher,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Expected {
    Absent,
    Entity(u128),
    Deletion(u64),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Record {
    pub id: u128,
    pub revision: u64,
    pub origin: Origin,
    pub expected: Expected,
    pub detach: Option<bool>,
    pub generation: u64,
    pub bits: Option<u64>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Create {
        revision: u64,
        bits: u64,
    },
    Put {
        revision: u64,
        expected: u128,
        bits: u64,
    },
    Delete {
        revision: u64,
        expected: u128,
        detach: bool,
    },
    Recreate {
        revision: u64,
        deleted: u64,
        bits: u64,
    },
    CypherPut {
        bits: u64,
    },
    CypherDelete {
        detach: bool,
    },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rejection {
    Missing,
    Stale,
    Conflict,
    Exists,
    Deleted,
    NotDeleted,
    Incarnation,
    DeletionRevision,
    Overflow,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Observation {
    NoOp,
    Replay(Record),
    Changed(Record),
    Rejected(Rejection),
}

/// Complete primitive state transition. The model compares IEEE payload integers,
/// never engine canonical hashes, encodings, domain types or helper functions.
pub fn predict(
    state: Option<Record>,
    action: Action,
    fresh_id: u128,
    generation: u64,
) -> Observation {
    use Observation::{Changed, Rejected, Replay};
    use Rejection::*;
    let (origin, revision, expected, bits, detach) = match action {
        Action::Create { revision, bits } => {
            (Origin::Create, revision, Expected::Absent, Some(bits), None)
        }
        Action::Put {
            revision,
            expected,
            bits,
        } => (
            Origin::Put,
            revision,
            Expected::Entity(expected),
            Some(bits),
            None,
        ),
        Action::Delete {
            revision,
            expected,
            detach,
        } => (
            Origin::Delete,
            revision,
            Expected::Entity(expected),
            None,
            Some(detach),
        ),
        Action::Recreate {
            revision,
            deleted,
            bits,
        } => (
            Origin::Recreate,
            revision,
            Expected::Deletion(deleted),
            Some(bits),
            None,
        ),
        Action::CypherPut { bits } => return cypher(state, Some(bits), None, generation),
        Action::CypherDelete { detach } => return cypher(state, None, Some(detach), generation),
    };
    let candidate = |id| Record {
        id,
        revision,
        origin,
        expected,
        bits,
        detach,
        generation,
    };
    let Some(old) = state else {
        return if origin == Origin::Create {
            Changed(candidate(fresh_id))
        } else {
            Rejected(Missing)
        };
    };
    if matches!(expected, Expected::Entity(id) if id != old.id) {
        return Rejected(Incarnation);
    }
    if revision < old.revision {
        return Rejected(Stale);
    }
    if revision == old.revision {
        let retry = Record {
            generation: old.generation,
            ..candidate(old.id)
        };
        return if retry == old {
            Replay(old)
        } else {
            Rejected(Conflict)
        };
    }
    let live = old.bits.is_some();
    let decision = match (live, origin) {
        (true, Origin::Create) => Some(Exists),
        (true, Origin::Recreate) => Some(NotDeleted),
        (false, Origin::Create | Origin::Put | Origin::Delete) => Some(Deleted),
        (false, Origin::Recreate) if expected != Expected::Deletion(old.revision) => {
            Some(DeletionRevision)
        }
        _ => None,
    };
    match decision {
        Some(reason) => Rejected(reason),
        None => Changed(candidate(if live { old.id } else { fresh_id })),
    }
}

fn cypher(
    state: Option<Record>,
    bits: Option<u64>,
    detach: Option<bool>,
    generation: u64,
) -> Observation {
    let Some(old) = state.filter(|old| old.bits.is_some()) else {
        return Observation::NoOp;
    };
    if bits == old.bits {
        return Observation::NoOp;
    }
    match old.revision.checked_add(1) {
        Some(revision) => Observation::Changed(Record {
            id: old.id,
            revision,
            origin: Origin::Cypher,
            expected: Expected::Entity(old.id),
            detach,
            generation,
            bits,
        }),
        None => Observation::Rejected(Rejection::Overflow),
    }
}

pub fn check(
    state: Option<Record>,
    action: Action,
    fresh_id: u128,
    generation: u64,
    observed: Observation,
) -> Result<(), String> {
    let expected = predict(state, action, fresh_id, generation);
    if observed == expected {
        Ok(())
    } else {
        Err(format!(
            "PG5 key lifecycle expected={expected:?} observed={observed:?} state={state:?} action={action:?}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn primitive_lifecycle_oracle_preserves_creation_replay_and_incarnation_fences() {
        let action = Action::Create {
            revision: 7,
            bits: 0x8000_0000_0000_0000,
        };
        let installed = Record {
            id: (1_u128 << 100) | 9,
            revision: 7,
            origin: Origin::Create,
            expected: Expected::Absent,
            detach: None,
            generation: 13,
            bits: Some(0x8000_0000_0000_0000),
        };
        assert_eq!(
            predict(None, action, installed.id, 13),
            Observation::Changed(installed)
        );
        assert_eq!(
            predict(Some(installed), action, 99, 14),
            Observation::Replay(installed)
        );
        assert_eq!(
            predict(
                Some(installed),
                Action::Delete {
                    revision: u64::MAX,
                    expected: 9,
                    detach: false
                },
                99,
                14
            ),
            Observation::Rejected(Rejection::Incarnation)
        );
        assert!(
            check(
                None,
                action,
                installed.id,
                13,
                Observation::Changed(installed)
            )
            .is_ok()
        );
        assert!(
            check(None, action, installed.id, 13, Observation::NoOp)
                .expect_err("CAN FIRE")
                .contains("PG5 key lifecycle")
        );
    }
}
