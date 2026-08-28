//! Independent checks for the minimal full-text-search campaign.

pub const I40_CHECKER_ID: &str = "fts.i40.tokenizer.v1";
pub const I41_CHECKER_ID: &str = "fts.i41.regions.v1";
pub const I42_CHECKER_ID: &str = "fts.i42.bm25.v1";
pub const I43_CHECKER_ID: &str = "fts.i43.pruning.v1";
pub const I44_CHECKER_ID: &str = "fts.i44.extras.v1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TokenFact {
    pub term: String,
    pub position: u32,
    pub start: u32,
    pub end: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FtsInput {
    pub tokens: Vec<TokenFact>,
    pub row_count: u32,
    pub expected_rows: Vec<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FtsObserved {
    pub tokens: Vec<TokenFact>,
    pub row_count: u32,
    pub term_count: u32,
    pub exhaustive_rows: Vec<u32>,
    pub pruned_rows: Vec<u32>,
    pub finite_scores: bool,
    pub phrase_ok: bool,
    pub prefix_ok: bool,
    pub fuzzy_ok: bool,
    pub phonetic_ok: bool,
    pub snippet_ok: bool,
}

fn fail(checker: &str, detail: &str) -> Result<(), String> {
    Err(format!("{checker}: {detail}"))
}

pub fn compare_i40(input: &FtsInput, observed: &FtsObserved) -> Result<(), String> {
    if observed.tokens != input.tokens {
        return fail(I40_CHECKER_ID, "token sequence or UTF-8 offsets differ");
    }
    Ok(())
}

pub fn compare_i41(input: &FtsInput, observed: &FtsObserved) -> Result<(), String> {
    if observed.row_count != input.row_count || observed.term_count == 0 {
        return fail(I41_CHECKER_ID, "sealed lexical region facts differ");
    }
    Ok(())
}

pub fn compare_i42(input: &FtsInput, observed: &FtsObserved) -> Result<(), String> {
    if observed.exhaustive_rows != input.expected_rows || !observed.finite_scores {
        return fail(I42_CHECKER_ID, "BM25 order or score finiteness differs");
    }
    Ok(())
}

pub fn compare_i43(_input: &FtsInput, observed: &FtsObserved) -> Result<(), String> {
    if observed.pruned_rows != observed.exhaustive_rows {
        return fail(
            I43_CHECKER_ID,
            "pruned results differ from exhaustive results",
        );
    }
    Ok(())
}

pub fn compare_i44(_input: &FtsInput, observed: &FtsObserved) -> Result<(), String> {
    if !observed.phrase_ok
        || !observed.prefix_ok
        || !observed.fuzzy_ok
        || !observed.phonetic_ok
        || !observed.snippet_ok
    {
        return fail(
            I44_CHECKER_ID,
            "one lexical extra disagreed with its literal oracle",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (FtsInput, FtsObserved) {
        let tokens = vec![TokenFact {
            term: "alpha".to_owned(),
            position: 0,
            start: 0,
            end: 5,
        }];
        (
            FtsInput {
                tokens: tokens.clone(),
                row_count: 3,
                expected_rows: vec![0, 1],
            },
            FtsObserved {
                tokens,
                row_count: 3,
                term_count: 2,
                exhaustive_rows: vec![0, 1],
                pruned_rows: vec![0, 1],
                finite_scores: true,
                phrase_ok: true,
                prefix_ok: true,
                fuzzy_ok: true,
                phonetic_ok: true,
                snippet_ok: true,
            },
        )
    }

    #[test]
    fn every_fts_checker_accepts_a_valid_observation() {
        let (input, observed) = pair();
        compare_i40(&input, &observed).unwrap();
        compare_i41(&input, &observed).unwrap();
        compare_i42(&input, &observed).unwrap();
        compare_i43(&input, &observed).unwrap();
        compare_i44(&input, &observed).unwrap();
    }

    #[test]
    fn every_fts_checker_rejects_a_deliberate_plant() {
        let (input, observed) = pair();
        let mut plant = observed.clone();
        plant.tokens.clear();
        assert!(compare_i40(&input, &plant).is_err());
        let mut plant = observed.clone();
        plant.term_count = 0;
        assert!(compare_i41(&input, &plant).is_err());
        let mut plant = observed.clone();
        plant.exhaustive_rows.reverse();
        assert!(compare_i42(&input, &plant).is_err());
        let mut plant = observed.clone();
        plant.pruned_rows.clear();
        assert!(compare_i43(&input, &plant).is_err());
        let mut plant = observed;
        plant.snippet_ok = false;
        assert!(compare_i44(&input, &plant).is_err());
    }
}
