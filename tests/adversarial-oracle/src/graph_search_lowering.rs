//! PG20 primitive search-plan observations. This crate imports no engine or
//! compiler types and derives no expectation from production lowering helpers.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Eligibility {
    AllIndexed,
    LiteralEmpty,
    GlobalDistinctNodes,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchObservation {
    pub mode: u8,
    pub eligibility: Eligibility,
    pub output_masks: [Option<u16>; 5],
    pub unit_input: bool,
    pub request_slots: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Observation {
    pub searches: Vec<SearchObservation>,
    pub eager: Vec<u32>,
    pub cartesian_joins: usize,
    pub limit_zero: bool,
}

pub fn expected_pair(mode: u8) -> Observation {
    Observation {
        searches: vec![
            SearchObservation {
                mode,
                eligibility: Eligibility::LiteralEmpty,
                output_masks: [Some(32), Some(8), None, None, None],
                unit_input: true,
                request_slots: 0,
            },
            SearchObservation {
                mode: 1,
                eligibility: Eligibility::AllIndexed,
                output_masks: [Some(32), None, Some(8), Some(9), Some(9)],
                unit_input: true,
                request_slots: 0,
            },
        ],
        eager: vec![0, 1],
        cartesian_joins: 2,
        limit_zero: true,
    }
}

pub fn expected_global() -> Observation {
    Observation {
        searches: vec![SearchObservation {
            mode: 255,
            eligibility: Eligibility::GlobalDistinctNodes,
            output_masks: [Some(32), None, Some(8), None, None],
            unit_input: false,
            request_slots: 0,
        }],
        eager: vec![0],
        cartesian_joins: 0,
        limit_zero: true,
    }
}

fn check(expected: Observation, observed: &Observation) -> Result<(), String> {
    if expected != *observed {
        return Err(format!(
            "PG20 expected={expected:#?}\nobserved={observed:#?}"
        ));
    }
    Ok(())
}

pub fn check_pair(mode: u8, observed: &Observation) -> Result<(), String> {
    check(expected_pair(mode), observed)
}

pub fn check_global(observed: &Observation) -> Result<(), String> {
    check(expected_global(), observed)
}
