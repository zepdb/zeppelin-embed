//! Independent checks for the minimal C-ABI bindings campaign.

pub const I66_CHECKER_ID: &str = "ffi.i66.validation.v1";
pub const I67_CHECKER_ID: &str = "ffi.i67.ownership.v1";
pub const I68_CHECKER_ID: &str = "ffi.i68.containment.v1";
pub const I69_CHECKER_ID: &str = "ffi.i69.control.v1";
pub const I70_CHECKER_ID: &str = "ffi.i70.parity.v1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FfiInput {
    pub expected_abi_version: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FfiObserved {
    pub null_pointer_rejected: bool,
    pub invalid_enum_rejected: bool,
    pub stale_handle_rejected: bool,
    pub double_destroy_rejected: bool,
    pub panic_caught: bool,
    pub poisoned_after_panic: bool,
    pub control_cancelled_without_hits: bool,
    pub abi_version: u32,
    pub error_name_matches: bool,
    pub malformed_sequence_rejected: bool,
}

fn require(checker: &str, condition: bool, detail: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(format!("{checker}: {detail}"))
    }
}

pub fn compare_i66(_: &FfiInput, observed: &FfiObserved) -> Result<(), String> {
    require(
        I66_CHECKER_ID,
        observed.null_pointer_rejected && observed.invalid_enum_rejected,
        "invalid pointer or enum crossed the ABI",
    )
}

pub fn compare_i67(_: &FfiInput, observed: &FfiObserved) -> Result<(), String> {
    require(
        I67_CHECKER_ID,
        observed.stale_handle_rejected && observed.double_destroy_rejected,
        "stale ownership state was accepted",
    )
}

pub fn compare_i68(_: &FfiInput, observed: &FfiObserved) -> Result<(), String> {
    require(
        I68_CHECKER_ID,
        observed.panic_caught && observed.poisoned_after_panic,
        "panic escaped or failed to poison the handle",
    )
}

pub fn compare_i69(_: &FfiInput, observed: &FfiObserved) -> Result<(), String> {
    require(
        I69_CHECKER_ID,
        observed.control_cancelled_without_hits,
        "FFI query control returned partial hits",
    )
}

pub fn compare_i70(input: &FfiInput, observed: &FfiObserved) -> Result<(), String> {
    require(
        I70_CHECKER_ID,
        observed.abi_version == input.expected_abi_version
            && observed.error_name_matches
            && observed.malformed_sequence_rejected,
        "binding constants, names, or call-sequence parity drifted",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (FfiInput, FfiObserved) {
        (
            FfiInput {
                expected_abi_version: 1,
            },
            FfiObserved {
                null_pointer_rejected: true,
                invalid_enum_rejected: true,
                stale_handle_rejected: true,
                double_destroy_rejected: true,
                panic_caught: true,
                poisoned_after_panic: true,
                control_cancelled_without_hits: true,
                abi_version: 1,
                error_name_matches: true,
                malformed_sequence_rejected: true,
            },
        )
    }

    #[test]
    fn every_ffi_checker_accepts_a_valid_observation() {
        let (input, observed) = pair();
        compare_i66(&input, &observed).unwrap();
        compare_i67(&input, &observed).unwrap();
        compare_i68(&input, &observed).unwrap();
        compare_i69(&input, &observed).unwrap();
        compare_i70(&input, &observed).unwrap();
    }

    #[test]
    fn every_ffi_checker_rejects_a_deliberate_plant() {
        let (input, observed) = pair();
        let mut plant = observed.clone();
        plant.invalid_enum_rejected = false;
        assert!(compare_i66(&input, &plant).is_err());
        let mut plant = observed.clone();
        plant.double_destroy_rejected = false;
        assert!(compare_i67(&input, &plant).is_err());
        let mut plant = observed.clone();
        plant.panic_caught = false;
        assert!(compare_i68(&input, &plant).is_err());
        let mut plant = observed.clone();
        plant.control_cancelled_without_hits = false;
        assert!(compare_i69(&input, &plant).is_err());
        let mut plant = observed;
        plant.error_name_matches = false;
        assert!(compare_i70(&input, &plant).is_err());
    }
}
