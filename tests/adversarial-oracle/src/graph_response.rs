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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeObservation {
    pub node_id: (u64, u64),
    pub relationship_id: (u64, u64),
    pub scalar_bits: u64,
    pub list_tag: u32,
    pub text: (u32, u32, u32),
    pub payload: Vec<u8>,
    pub report_work: (u32, u32),
    pub global_work: (u32, u64),
    pub peak_query_bytes: u64,
    pub source_calls: usize,
    pub allocation_matching_sites: usize,
    pub allocation_fires: usize,
    pub query_charge_restored: bool,
}

pub fn check_native(
    node_id: u128,
    relationship_id: u128,
    scalar_bits: u64,
    payload: &[u8],
    include_report: bool,
    observed: &NativeObservation,
) -> Result<(), String> {
    let expected_report = if include_report { (23, 22) } else { (0, 0) };
    if observed.node_id != ((node_id >> 64) as u64, node_id as u64)
        || observed.relationship_id != ((relationship_id >> 64) as u64, relationship_id as u64)
        || observed.scalar_bits != scalar_bits
        || observed.list_tag != 5
        || observed.text != (1, payload.len() as u32, 0)
        || observed.payload != payload
        || observed.report_work != expected_report
        || observed.global_work.0 != 23
        || observed.global_work.1 == 0
        || observed.peak_query_bytes == 0
        || observed.source_calls != 1
        || observed.allocation_matching_sites != 2
        || observed.allocation_fires != 0
        || !observed.query_charge_restored
    {
        return Err(format!("PG16 native conversion mismatch: {observed:?}"));
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "the independent refusal oracle compares every expected primitive directly"
)]
pub fn check_native_failure(
    expected_stage: u32,
    expected_refusal: u32,
    expected_matches: usize,
    observed_stage: u32,
    observed_refusal: u32,
    matching_sites: usize,
    fires: usize,
    charge_restored: bool,
    clean_succeeded: bool,
) -> Result<(), String> {
    if observed_stage != expected_stage
        || observed_refusal != expected_refusal
        || matching_sites != expected_matches
        || fires != 1
        || !charge_restored
        || !clean_succeeded
    {
        return Err(format!(
            "PG16 native refusal mismatch: stage={observed_stage}/{expected_stage} refusal={observed_refusal}/{expected_refusal} matches={matching_sites}/{expected_matches} fires={fires} restored={charge_restored} clean={clean_succeeded}"
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
        let observation = NativeObservation {
            node_id: (1, 7),
            relationship_id: (2, 9),
            scalar_bits: 0x8000_0000_0000_0000,
            list_tag: 5,
            text: (1, 4, 0),
            payload: b"four".to_vec(),
            report_work: (23, 22),
            global_work: (23, 91),
            peak_query_bytes: 4096,
            source_calls: 1,
            allocation_matching_sites: 2,
            allocation_fires: 0,
            query_charge_restored: true,
        };
        assert!(
            check_native(
                (1_u128 << 64) | 7,
                (2_u128 << 64) | 9,
                0x8000_0000_0000_0000,
                b"four",
                true,
                &observation,
            )
            .is_ok()
        );
        for mutation in 0..8 {
            let mut wrong = observation.clone();
            match mutation {
                0 => wrong.node_id.0 ^= 1,
                1 => wrong.scalar_bits ^= 1,
                2 => wrong.text.0 = 0,
                3 => wrong.list_tag = 0,
                4 => wrong.report_work.0 = 22,
                5 => wrong.global_work.0 = 22,
                6 => wrong.query_charge_restored = false,
                _ => wrong.allocation_matching_sites = 1,
            }
            assert!(
                check_native(
                    (1_u128 << 64) | 7,
                    (2_u128 << 64) | 9,
                    0x8000_0000_0000_0000,
                    b"four",
                    true,
                    &wrong,
                )
                .is_err()
            );
        }
        assert!(check_native_failure(1, 3, 2, 1, 3, 2, 1, true, true).is_ok());
        assert!(check_native_failure(1, 3, 2, 1, 3, 2, 0, true, true).is_err());
    }
}
