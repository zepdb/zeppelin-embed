//! Primitive crash/lifecycle comparisons. No engine codec or observation builds expectations.
use crate::graph_fixture::{Graph, Mutation, Snapshot};
use std::collections::BTreeMap;

pub const CONTRACT: &str = "graph-lifecycle-v1";
pub const COMPLETE_PREFIX: &str = "complete-prefix";
pub const IDENTITY_HISTORY: &str = "identity-history";
pub const PROTECTED_ARTIFACTS: &str = "protected-artifacts";
pub const OUTCOME: &str = "outcome";

/// Input batches are atomic; only the explicit admissible complete cutoffs may recover.
pub fn compare_complete_prefix(
    batches: &[Vec<Mutation>],
    admissible: &[usize],
    observed: &Snapshot,
) -> Result<(), String> {
    let mut graph = Graph::default();
    let mut snapshots = vec![graph.snapshot()];
    for batch in batches {
        graph
            .apply(batch)
            .map_err(|e| format!("{COMPLETE_PREFIX}: invalid input {e:?}"))?;
        snapshots.push(graph.snapshot());
    }
    if admissible
        .iter()
        .any(|cut| snapshots.get(*cut) == Some(observed))
    {
        Ok(())
    } else {
        Err(format!(
            "{COMPLETE_PREFIX}: state is not an admissible complete batch"
        ))
    }
}

/// Entity kind is part of identity; a node and relationship may share integer bits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Identity {
    pub relationship: bool,
    pub key: String,
    pub id: u128,
    pub revision: u64,
    pub generation: u64,
    pub replayed: bool,
}
pub fn compare_identity_history(
    expected: &[Identity],
    observed: &[Identity],
) -> Result<(), String> {
    if expected == observed {
        Ok(())
    } else {
        Err(format!("{IDENTITY_HISTORY}: receipt history differs"))
    }
}

/// The input protection set includes checkpoint, WAL, current, read and prepared references.
pub fn compare_protected_artifacts(
    protected: &BTreeMap<String, Vec<u8>>,
    observed: &BTreeMap<String, Vec<u8>>,
) -> Result<(), String> {
    for (name, bytes) in protected {
        if observed.get(name) != Some(bytes) {
            return Err(format!("{PROTECTED_ARTIFACTS}: {name} missing or changed"));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Outcome {
    pub error: Option<String>,
    pub nothing_committed: bool,
    pub stopped_error: Option<String>,
}
pub fn compare_outcome(expected: &Outcome, observed: &Outcome) -> Result<(), String> {
    if expected == observed {
        Ok(())
    } else {
        Err(format!(
            "{OUTCOME}: disposition/error/writer state differs expected={expected:?} observed={observed:?}"
        ))
    }
}
