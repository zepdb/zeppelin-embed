//! Independent checks for the minimal hybrid-fusion campaign.

pub const I45_CHECKER_ID: &str = "hybrid.i45.provenance.v1";
pub const I46_CHECKER_ID: &str = "hybrid.i46.normalization.v1";
pub const I47_CHECKER_ID: &str = "hybrid.i47.bounded.v1";
pub const I48_CHECKER_ID: &str = "hybrid.i48.rrf.v1";
pub const I49_CHECKER_ID: &str = "hybrid.i49.legs.v1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HybridInput {
    pub k: usize,
    pub expected_ids: Vec<u32>,
    pub expected_rrf_ids: Vec<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HybridObserved {
    pub ids: Vec<u32>,
    pub rrf_ids: Vec<u32>,
    pub raw_scores_preserved: bool,
    pub finite_scores: bool,
    pub used_rrf: bool,
    pub no_partial: bool,
}

fn fail(checker: &str, detail: &str) -> Result<(), String> {
    Err(format!("{checker}: {detail}"))
}

pub fn compare_i45(_input: &HybridInput, observed: &HybridObserved) -> Result<(), String> {
    if !observed.raw_scores_preserved {
        return fail(I45_CHECKER_ID, "raw leg provenance was not retained");
    }
    Ok(())
}

pub fn compare_i46(_input: &HybridInput, observed: &HybridObserved) -> Result<(), String> {
    if !observed.finite_scores {
        return fail(I46_CHECKER_ID, "fusion produced a non-finite score");
    }
    Ok(())
}

pub fn compare_i47(input: &HybridInput, observed: &HybridObserved) -> Result<(), String> {
    if observed.ids.len() > input.k || observed.ids != input.expected_ids {
        return fail(I47_CHECKER_ID, "bounded fusion order differs");
    }
    Ok(())
}

pub fn compare_i48(input: &HybridInput, observed: &HybridObserved) -> Result<(), String> {
    if !observed.used_rrf || observed.rrf_ids != input.expected_rrf_ids {
        return fail(I48_CHECKER_ID, "RRF fallback order differs");
    }
    Ok(())
}

pub fn compare_i49(_input: &HybridInput, observed: &HybridObserved) -> Result<(), String> {
    if !observed.no_partial {
        return fail(I49_CHECKER_ID, "failed leg exposed partial output");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (HybridInput, HybridObserved) {
        (
            HybridInput {
                k: 2,
                expected_ids: vec![1, 2],
                expected_rrf_ids: vec![1, 2],
            },
            HybridObserved {
                ids: vec![1, 2],
                rrf_ids: vec![1, 2],
                raw_scores_preserved: true,
                finite_scores: true,
                used_rrf: true,
                no_partial: true,
            },
        )
    }

    #[test]
    fn every_hybrid_checker_accepts_a_valid_observation() {
        let (input, observed) = pair();
        compare_i45(&input, &observed).unwrap();
        compare_i46(&input, &observed).unwrap();
        compare_i47(&input, &observed).unwrap();
        compare_i48(&input, &observed).unwrap();
        compare_i49(&input, &observed).unwrap();
    }

    #[test]
    fn every_hybrid_checker_rejects_a_deliberate_plant() {
        let (input, observed) = pair();
        let mut plant = observed.clone();
        plant.raw_scores_preserved = false;
        assert!(compare_i45(&input, &plant).is_err());
        let mut plant = observed.clone();
        plant.finite_scores = false;
        assert!(compare_i46(&input, &plant).is_err());
        let mut plant = observed.clone();
        plant.ids.push(3);
        assert!(compare_i47(&input, &plant).is_err());
        let mut plant = observed.clone();
        plant.used_rrf = false;
        assert!(compare_i48(&input, &plant).is_err());
        let mut plant = observed;
        plant.no_partial = false;
        assert!(compare_i49(&input, &plant).is_err());
    }
}
