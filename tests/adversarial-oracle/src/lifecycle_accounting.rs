//! Independent checks for the minimal lifecycle/accounting campaign.

pub const I54_CHECKER_ID: &str = "lifecycle.i54.deadline.v1";
pub const I55_CHECKER_ID: &str = "lifecycle.i55.cancellation.v1";
pub const I56_CHECKER_ID: &str = "lifecycle.i56.close-drain.v1";
pub const I57_CHECKER_ID: &str = "lifecycle.i57.locking.v1";
pub const I58_CHECKER_ID: &str = "lifecycle.i58.accounting.v1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LifecycleInput {
    pub expected_active_queries_after: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LifecycleObserved {
    pub deadline_timed_out_without_partial: bool,
    pub cancellation_without_partial: bool,
    pub close_cancelled_active_query: bool,
    pub post_close_refused: bool,
    pub second_writer_refused: bool,
    pub active_queries_after: u64,
    pub query_pool_bytes: u64,
    pub allocation_denied: bool,
}

fn require(checker: &str, condition: bool, detail: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(format!("{checker}: {detail}"))
    }
}

pub fn compare_i54(_: &LifecycleInput, observed: &LifecycleObserved) -> Result<(), String> {
    require(
        I54_CHECKER_ID,
        observed.deadline_timed_out_without_partial,
        "deadline did not return a typed no-partial timeout",
    )
}

pub fn compare_i55(input: &LifecycleInput, observed: &LifecycleObserved) -> Result<(), String> {
    require(
        I55_CHECKER_ID,
        observed.cancellation_without_partial
            && observed.active_queries_after == input.expected_active_queries_after,
        "cancellation returned partial results or retained an admitted query",
    )
}

pub fn compare_i56(_: &LifecycleInput, observed: &LifecycleObserved) -> Result<(), String> {
    require(
        I56_CHECKER_ID,
        observed.close_cancelled_active_query && observed.post_close_refused,
        "close did not drain/cancel active work and refuse later work",
    )
}

pub fn compare_i57(_: &LifecycleInput, observed: &LifecycleObserved) -> Result<(), String> {
    require(
        I57_CHECKER_ID,
        observed.second_writer_refused,
        "a second writer acquired the same store",
    )
}

pub fn compare_i58(input: &LifecycleInput, observed: &LifecycleObserved) -> Result<(), String> {
    require(
        I58_CHECKER_ID,
        observed.query_pool_bytes > 0
            && observed.active_queries_after == input.expected_active_queries_after
            && observed.allocation_denied,
        "accounting omitted pool bytes, leaked a query, or missed budget denial",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (LifecycleInput, LifecycleObserved) {
        (
            LifecycleInput {
                expected_active_queries_after: 0,
            },
            LifecycleObserved {
                deadline_timed_out_without_partial: true,
                cancellation_without_partial: true,
                close_cancelled_active_query: true,
                post_close_refused: true,
                second_writer_refused: true,
                active_queries_after: 0,
                query_pool_bytes: 64,
                allocation_denied: true,
            },
        )
    }

    #[test]
    fn every_lifecycle_checker_accepts_a_valid_observation() {
        let (input, observed) = pair();
        compare_i54(&input, &observed).unwrap();
        compare_i55(&input, &observed).unwrap();
        compare_i56(&input, &observed).unwrap();
        compare_i57(&input, &observed).unwrap();
        compare_i58(&input, &observed).unwrap();
    }

    #[test]
    fn every_lifecycle_checker_rejects_a_deliberate_plant() {
        let (input, observed) = pair();
        let mut plant = observed.clone();
        plant.deadline_timed_out_without_partial = false;
        assert!(compare_i54(&input, &plant).is_err());
        let mut plant = observed.clone();
        plant.cancellation_without_partial = false;
        assert!(compare_i55(&input, &plant).is_err());
        let mut plant = observed.clone();
        plant.post_close_refused = false;
        assert!(compare_i56(&input, &plant).is_err());
        let mut plant = observed.clone();
        plant.second_writer_refused = false;
        assert!(compare_i57(&input, &plant).is_err());
        let mut plant = observed;
        plant.allocation_denied = false;
        assert!(compare_i58(&input, &plant).is_err());
    }
}
