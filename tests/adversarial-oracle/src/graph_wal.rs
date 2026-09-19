//! PG7: primitive commit-boundary/state observations, independent of wire codecs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct State {
    pub store: u128,
    pub generation: u64,
    pub sequence: u64,
    /// Node, relationship, four symbol counters, physical creation serial.
    pub high_waters: [u128; 7],
}
pub fn check_transition(base: &State, next: &State, accepted: bool) -> Result<(), String> {
    let valid = base.store != 0
        && base.store == next.store
        && base.sequence.checked_add(1) == Some(next.sequence)
        && base.generation.checked_add(1) == Some(next.generation)
        && base
            .high_waters
            .iter()
            .zip(next.high_waters)
            .all(|(a, b)| *a <= b);
    if accepted == valid {
        Ok(())
    } else {
        Err(format!(
            "PG7 transition expected={valid}, observed={accepted}"
        ))
    }
}
/// The driver supplies primitive end offsets from its construction, not from the
/// production decoder. A known completely corrupt predecessor forbids success.
pub fn check_prefix(
    commits: &[(usize, State)],
    available: usize,
    corrupt_complete_at: Option<usize>,
    observed: &Result<Vec<State>, ()>,
) -> Result<(), String> {
    let expected = if corrupt_complete_at.is_some_and(|end| end <= available) {
        Err(())
    } else {
        Ok(commits
            .iter()
            .filter(|(end, _)| *end <= available)
            .map(|(_, state)| state.clone())
            .collect::<Vec<_>>())
    };
    if *observed == expected {
        Ok(())
    } else {
        Err(format!(
            "PG7 prefix available={available}, expected={expected:?}, observed={observed:?}"
        ))
    }
}
/// Fault evidence requires an observed fire and the same-seed clean states.
pub fn check_fault(
    clean: &[State],
    armed: bool,
    fires: usize,
    observed: &Result<Vec<State>, ()>,
) -> Result<(), String> {
    let valid = if armed {
        fires == 1 && observed.is_err()
    } else {
        fires == 0 && observed.as_ref().is_ok_and(|states| states == clean)
    };
    if valid {
        Ok(())
    } else {
        Err(format!(
            "PG7 fault armed={armed}, fires={fires}, observed={observed:?}"
        ))
    }
}
