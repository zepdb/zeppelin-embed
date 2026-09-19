//! PG3: graph artifact preparation/reopen facts, independent of engine code.
//! This does not model GraphStore commit, checkpoint admission or durability.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Case {
    Clean,
    Collision,
    Entropy,
    BeforeCreate,
    AfterCreate,
    Torn,
    BitFlip,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    Created,
    Collision,
    Entropy,
    CreateFailed,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Observation {
    pub outcome: Outcome,
    pub attempted: Option<u128>,
    pub decoded: Option<(u128, u128, Vec<u8>)>,
    pub candidate_present: bool,
    pub protected_unchanged: bool,
    pub inventory_count: usize,
    pub entropy_calls: usize,
    pub fault_fires: usize,
    pub write_calls: u64,
    pub bytes_written: u64,
}

pub fn check(
    case: Case,
    store: u128,
    artifact: u128,
    payload: &[u8],
    observed: &Observation,
) -> Result<(), String> {
    let created = matches!(case, Case::Clean | Case::Torn | Case::BitFlip);
    let present = !matches!(case, Case::Entropy | Case::BeforeCreate);
    let expected = Observation {
        outcome: match case {
            Case::Collision => Outcome::Collision,
            Case::Entropy => Outcome::Entropy,
            Case::BeforeCreate | Case::AfterCreate => Outcome::CreateFailed,
            _ => Outcome::Created,
        },
        attempted: if case == Case::Entropy {
            None
        } else {
            Some(artifact)
        },
        decoded: if matches!(case, Case::Clean | Case::AfterCreate) {
            Some((store, artifact, payload.to_vec()))
        } else {
            None
        },
        candidate_present: present,
        protected_unchanged: true,
        inventory_count: 2 + usize::from(present),
        entropy_calls: 1,
        fault_fires: usize::from(matches!(
            case,
            Case::BeforeCreate | Case::AfterCreate | Case::Torn | Case::BitFlip
        )),
        write_calls: u64::from(created),
        // Independent handwritten single-block framing: 96 + 24 + payload + 24 + 8.
        bytes_written: if created {
            152 + payload.len() as u64
        } else {
            0
        },
    };
    if observed == &expected {
        Ok(())
    } else {
        Err(format!(
            "PG3 case={case:?} expected={expected:?} observed={observed:?}"
        ))
    }
}
