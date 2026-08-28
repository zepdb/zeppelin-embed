//! Independent, std-only checks for the minimal Vamana graph campaign.

use std::collections::BTreeSet;

pub const I28_CHECKER_ID: &str = "graph.i28.shape.v1";
pub const I29_CHECKER_ID: &str = "graph.i29.entry-points.v1";
pub const I30_CHECKER_ID: &str = "graph.i30.reachability.v1";
pub const I31_CHECKER_ID: &str = "graph.i31.result-soundness.v1";
pub const I32_CHECKER_ID: &str = "graph.i32.bounded-work.v1";
pub const I33_CHECKER_ID: &str = "graph.i33.atomic-publication.v1";
pub const I34_CHECKER_ID: &str = "graph.i34.segment-alignment.v1";
pub const I35_CHECKER_ID: &str = "graph.i35.filtered-soundness.v1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphInput {
    pub row_count: u32,
    pub top_documents: Vec<u128>,
    pub filtered_documents: Vec<u128>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphObserved {
    pub row_count: u32,
    pub graphs_built: u64,
    pub budget_exhausted: bool,
    pub graph_segments: u64,
    pub entry_discoveries: u64,
    pub documents: Vec<u128>,
    pub reopened_documents: Vec<u128>,
    pub filtered_documents: Vec<u128>,
    pub source_aligned: bool,
}

fn mismatch(checker: &str, detail: &str) -> Result<(), String> {
    Err(format!("{checker}: {detail}"))
}

pub fn compare_i28(input: &GraphInput, observed: &GraphObserved) -> Result<(), String> {
    if observed.row_count != input.row_count || observed.graphs_built != 1 {
        return mismatch(I28_CHECKER_ID, "published graph shape differs from fixture");
    }
    Ok(())
}

pub fn compare_i29(input: &GraphInput, observed: &GraphObserved) -> Result<(), String> {
    if input.row_count == 0 || observed.graph_segments == 0 || observed.entry_discoveries == 0 {
        return mismatch(I29_CHECKER_ID, "graph entry discovery was not observed");
    }
    Ok(())
}

pub fn compare_i30(input: &GraphInput, observed: &GraphObserved) -> Result<(), String> {
    let expected = input.top_documents.iter().copied().collect::<BTreeSet<_>>();
    let actual = observed.documents.iter().copied().collect::<BTreeSet<_>>();
    if actual != expected {
        return mismatch(
            I30_CHECKER_ID,
            "reachable result set differs from brute force",
        );
    }
    Ok(())
}

pub fn compare_i31(input: &GraphInput, observed: &GraphObserved) -> Result<(), String> {
    if observed.documents != input.top_documents {
        return mismatch(
            I31_CHECKER_ID,
            "ordered graph result differs from brute force",
        );
    }
    Ok(())
}

pub fn compare_i32(_input: &GraphInput, observed: &GraphObserved) -> Result<(), String> {
    if !observed.budget_exhausted || observed.graphs_built != 1 {
        return mismatch(I32_CHECKER_ID, "bounded build did not defer then complete");
    }
    Ok(())
}

pub fn compare_i33(_input: &GraphInput, observed: &GraphObserved) -> Result<(), String> {
    if observed.documents != observed.reopened_documents {
        return mismatch(I33_CHECKER_ID, "reopen changed the published graph result");
    }
    Ok(())
}

pub fn compare_i34(_input: &GraphInput, observed: &GraphObserved) -> Result<(), String> {
    if !observed.source_aligned {
        return mismatch(
            I34_CHECKER_ID,
            "graph candidates escaped the published segment",
        );
    }
    Ok(())
}

pub fn compare_i35(input: &GraphInput, observed: &GraphObserved) -> Result<(), String> {
    if observed.filtered_documents != input.filtered_documents {
        return mismatch(
            I35_CHECKER_ID,
            "filtered graph result differs from brute force",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (GraphInput, GraphObserved) {
        (
            GraphInput {
                row_count: 4,
                top_documents: vec![1, 2],
                filtered_documents: vec![1],
            },
            GraphObserved {
                row_count: 4,
                graphs_built: 1,
                budget_exhausted: true,
                graph_segments: 1,
                entry_discoveries: 1,
                documents: vec![1, 2],
                reopened_documents: vec![1, 2],
                filtered_documents: vec![1],
                source_aligned: true,
            },
        )
    }

    #[test]
    fn every_graph_invariant_accepts_a_valid_observation() {
        let (input, observed) = pair();
        for check in [
            compare_i28,
            compare_i29,
            compare_i30,
            compare_i31,
            compare_i32,
            compare_i33,
            compare_i34,
            compare_i35,
        ] {
            check(&input, &observed).expect("valid graph observation");
        }
    }

    #[test]
    fn every_graph_invariant_rejects_its_deliberate_plant() {
        let (input, observed) = pair();
        let mut plant = observed.clone();
        plant.row_count = 3;
        assert!(compare_i28(&input, &plant).is_err());
        let mut plant = observed.clone();
        plant.entry_discoveries = 0;
        assert!(compare_i29(&input, &plant).is_err());
        let mut plant = observed.clone();
        plant.documents = vec![1, 3];
        assert!(compare_i30(&input, &plant).is_err());
        let mut plant = observed.clone();
        plant.documents.reverse();
        assert!(compare_i31(&input, &plant).is_err());
        let mut plant = observed.clone();
        plant.budget_exhausted = false;
        assert!(compare_i32(&input, &plant).is_err());
        let mut plant = observed.clone();
        plant.reopened_documents.reverse();
        assert!(compare_i33(&input, &plant).is_err());
        let mut plant = observed.clone();
        plant.source_aligned = false;
        assert!(compare_i34(&input, &plant).is_err());
        let mut plant = observed.clone();
        plant.filtered_documents.push(2);
        assert!(compare_i35(&input, &plant).is_err());
    }
}
