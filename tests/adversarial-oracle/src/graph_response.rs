//! PG16 independent primitive owner observations. No engine, C descriptor,
//! allocation-layout helper, registry or outcome implementation is imported.
#[derive(Clone, Debug)]
pub struct Observation {
    pub integer: i64,
    pub bytes: Vec<u8>,
    pub rows: usize,
    pub columns: usize,
    pub cells: usize,
    pub disposition: u32,
    pub admitted: Option<u64>,
    pub changed: Option<u64>,
    pub aligned: bool,
    pub private_rejected: bool,
    pub forged_rejected: bool,
    pub stale_rejected: bool,
    pub empty_after_free: bool,
    pub second_free_succeeded: bool,
    pub query_charge_released: bool,
}
pub fn check_owner(integer: i64, bytes: &[u8], observed: &Observation) -> Result<(), String> {
    if observed.integer != integer
        || observed.bytes != bytes
        || (observed.rows, observed.columns, observed.cells) != (1, 1, 1)
        || observed.disposition != 0
        || observed.admitted != Some(0)
        || observed.changed.is_some()
        || !observed.aligned
        || !observed.private_rejected
        || !observed.forged_rejected
        || !observed.stale_rejected
        || !observed.empty_after_free
        || !observed.second_free_succeeded
        || !observed.query_charge_released
    {
        return Err(format!("PG16 owner observation mismatch: {observed:?}"));
    }
    Ok(())
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fault {
    Allocation(usize),
    Cancel,
    Memory,
    Work,
    Registry,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refusal {
    Allocation,
    Cancel,
    Memory,
    Work,
    Registry,
    Other,
    Accepted,
}
pub fn check_fault(
    kind: Fault,
    refusal: Refusal,
    matches: usize,
    fires: usize,
    charge_restored: bool,
    clean_succeeded: bool,
) -> Result<(), String> {
    let (expected, expected_matches) = match kind {
        Fault::Allocation(ordinal) => (Refusal::Allocation, ordinal),
        Fault::Cancel => (Refusal::Cancel, 1),
        Fault::Memory => (Refusal::Memory, 1),
        Fault::Work => (Refusal::Work, 1),
        Fault::Registry => (Refusal::Registry, 1),
    };
    if refusal != expected
        || matches != expected_matches
        || fires != 1
        || !charge_restored
        || !clean_succeeded
    {
        return Err(format!(
            "PG16 unproved {kind:?}: refusal={refusal:?} matches={matches} fires={fires} charge_restored={charge_restored} clean={clean_succeeded}"
        ));
    }
    Ok(())
}
pub fn check_race(winners: usize, losers: usize, stale_rejected: bool) -> Result<(), String> {
    if winners != 1 || losers != 1 || !stale_rejected {
        return Err(format!(
            "PG16 ownership race: winners={winners} losers={losers} stale={stale_rejected}"
        ));
    }
    Ok(())
}
pub fn check_outcome(
    read: u32,
    attempted: u32,
    committed: u32,
    generation: Option<u64>,
    downgrade_rejected: bool,
    survived_delivery_unwind: bool,
) -> Result<(), String> {
    if read != 0
        || attempted != 5
        || committed != 2
        || generation != Some(73)
        || !downgrade_rejected
        || !survived_delivery_unwind
    {
        return Err(format!(
            "PG16 outcome loss: read={read} attempted={attempted} committed={committed} generation={generation:?} downgrade={downgrade_rejected} unwind={survived_delivery_unwind}"
        ));
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn graph_response_oracle_rejects_missing_fires_extra_owner_and_changed_commit() {
        assert!(check_fault(Fault::Allocation(2), Refusal::Allocation, 2, 1, true, true).is_ok());
        assert!(check_fault(Fault::Allocation(2), Refusal::Allocation, 2, 0, true, true).is_err());
        assert!(check_fault(Fault::Cancel, Refusal::Accepted, 1, 1, true, true).is_err());
        assert!(check_race(1, 1, true).is_ok());
        assert!(check_race(2, 0, true).is_err());
        assert!(check_outcome(0, 5, 2, Some(73), true, true).is_ok());
        assert!(check_outcome(0, 5, 2, None, true, true).is_err());
        assert!(check_outcome(0, 5, 1, Some(73), true, true).is_err());
    }
}
