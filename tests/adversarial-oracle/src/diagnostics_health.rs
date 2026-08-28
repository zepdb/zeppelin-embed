//! Independent checks for the minimal diagnostics/health campaign.

pub const I63_CHECKER_ID: &str = "diagnostics.i63.health.v1";
pub const I64_CHECKER_ID: &str = "diagnostics.i64.self-check.v1";
pub const I65_CHECKER_ID: &str = "diagnostics.i65.recovery.v1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticsInput {
    pub expected_documents: u64,
    pub expect_corruption: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticsObserved {
    pub pending_documents: u64,
    pub returned_matches_candidates: bool,
    pub counter_delta_matches_one_row: bool,
    pub self_check_healthy: bool,
    pub corruption_attributed: bool,
    pub recovery_cleared_fault: bool,
}

fn require(checker: &str, condition: bool, detail: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(format!("{checker}: {detail}"))
    }
}

pub fn compare_i63(input: &DiagnosticsInput, observed: &DiagnosticsObserved) -> Result<(), String> {
    require(
        I63_CHECKER_ID,
        observed.pending_documents == input.expected_documents
            && observed.returned_matches_candidates
            && observed.counter_delta_matches_one_row,
        "health or query counters disagree with the public operation",
    )
}

pub fn compare_i64(input: &DiagnosticsInput, observed: &DiagnosticsObserved) -> Result<(), String> {
    require(
        I64_CHECKER_ID,
        if input.expect_corruption {
            observed.corruption_attributed
        } else {
            observed.self_check_healthy
        },
        "self-check did not report the expected healthy/corrupt state",
    )
}

pub fn compare_i65(_: &DiagnosticsInput, observed: &DiagnosticsObserved) -> Result<(), String> {
    require(
        I65_CHECKER_ID,
        observed.corruption_attributed && observed.recovery_cleared_fault,
        "revalidation did not clear the repaired artifact fault",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_diagnostics_checker_accepts_a_valid_observation() {
        let input = DiagnosticsInput {
            expected_documents: 3,
            expect_corruption: true,
        };
        let observed = DiagnosticsObserved {
            pending_documents: 3,
            returned_matches_candidates: true,
            counter_delta_matches_one_row: true,
            self_check_healthy: true,
            corruption_attributed: true,
            recovery_cleared_fault: true,
        };
        compare_i63(&input, &observed).unwrap();
        compare_i64(&input, &observed).unwrap();
        compare_i65(&input, &observed).unwrap();
    }

    #[test]
    fn every_diagnostics_checker_rejects_a_deliberate_plant() {
        let input = DiagnosticsInput {
            expected_documents: 3,
            expect_corruption: true,
        };
        let observed = DiagnosticsObserved {
            pending_documents: 3,
            returned_matches_candidates: true,
            counter_delta_matches_one_row: true,
            self_check_healthy: true,
            corruption_attributed: true,
            recovery_cleared_fault: true,
        };
        let mut plant = observed.clone();
        plant.counter_delta_matches_one_row = false;
        assert!(compare_i63(&input, &plant).is_err());
        let mut plant = observed.clone();
        plant.corruption_attributed = false;
        assert!(compare_i64(&input, &plant).is_err());
        let mut plant = observed;
        plant.recovery_cleared_fault = false;
        assert!(compare_i65(&input, &plant).is_err());
    }
}
