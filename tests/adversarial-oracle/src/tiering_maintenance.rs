//! Independent checks for the minimal tiering-maintenance campaign.

pub const I50_CHECKER_ID: &str = "tier.i50.policy.v1";
pub const I51_CHECKER_ID: &str = "tier.i51.transition.v1";
pub const I52_CHECKER_ID: &str = "tier.i52.progress.v1";
pub const I53_CHECKER_ID: &str = "tier.i53.publication.v1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TierInput {
    pub threshold: u32,
    pub expected_documents: Vec<u128>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TierObserved {
    pub below_stays_scan: bool,
    pub at_threshold_transitions: bool,
    pub before_documents: Vec<u128>,
    pub after_documents: Vec<u128>,
    pub reopened_documents: Vec<u128>,
    pub budget_exhausted: bool,
    pub graphs_built: u64,
}

fn fail(checker: &str, detail: &str) -> Result<(), String> {
    Err(format!("{checker}: {detail}"))
}

pub fn compare_i50(_input: &TierInput, observed: &TierObserved) -> Result<(), String> {
    if !observed.below_stays_scan || !observed.at_threshold_transitions {
        return fail(I50_CHECKER_ID, "tier threshold decision differs");
    }
    Ok(())
}

pub fn compare_i51(input: &TierInput, observed: &TierObserved) -> Result<(), String> {
    if observed.before_documents != input.expected_documents
        || observed.after_documents != input.expected_documents
    {
        return fail(I51_CHECKER_ID, "tier transition changed query results");
    }
    Ok(())
}

pub fn compare_i52(_input: &TierInput, observed: &TierObserved) -> Result<(), String> {
    if !observed.budget_exhausted || observed.graphs_built != 1 {
        return fail(
            I52_CHECKER_ID,
            "budgeted transition did not defer then complete",
        );
    }
    Ok(())
}

pub fn compare_i53(_input: &TierInput, observed: &TierObserved) -> Result<(), String> {
    if observed.after_documents != observed.reopened_documents {
        return fail(I53_CHECKER_ID, "published graph changed after reopen");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (TierInput, TierObserved) {
        (
            TierInput {
                threshold: 4,
                expected_documents: vec![1, 2],
            },
            TierObserved {
                below_stays_scan: true,
                at_threshold_transitions: true,
                before_documents: vec![1, 2],
                after_documents: vec![1, 2],
                reopened_documents: vec![1, 2],
                budget_exhausted: true,
                graphs_built: 1,
            },
        )
    }

    #[test]
    fn every_tier_checker_accepts_a_valid_observation() {
        let (input, observed) = pair();
        compare_i50(&input, &observed).unwrap();
        compare_i51(&input, &observed).unwrap();
        compare_i52(&input, &observed).unwrap();
        compare_i53(&input, &observed).unwrap();
    }

    #[test]
    fn every_tier_checker_rejects_a_deliberate_plant() {
        let (input, observed) = pair();
        let mut plant = observed.clone();
        plant.below_stays_scan = false;
        assert!(compare_i50(&input, &plant).is_err());
        let mut plant = observed.clone();
        plant.after_documents.reverse();
        assert!(compare_i51(&input, &plant).is_err());
        let mut plant = observed.clone();
        plant.budget_exhausted = false;
        assert!(compare_i52(&input, &plant).is_err());
        let mut plant = observed;
        plant.reopened_documents.reverse();
        assert!(compare_i53(&input, &plant).is_err());
    }
}
