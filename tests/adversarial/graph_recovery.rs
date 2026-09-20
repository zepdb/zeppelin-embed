//! Directed ZE-40 recovery observations with an independent graph oracle.

use super::coverage::CoverageRegistry;
use std::collections::BTreeSet;
use zeppelin_embed_adversarial_oracle::graph_adjacency_store::{
    Operation, RelationshipRow, compare_read_view_expansion,
};

fn compare_state(
    expected: (u64, u64, u128, u128),
    actual: (u64, u64, u128, u128),
) -> Result<(), &'static str> {
    if expected == actual {
        Ok(())
    } else {
        Err("reopened recovery state differs from input history")
    }
}

pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    let report = zeppelin_embed::graph_recovery_test_support::run_actual_probe(seed);
    let expected = [
        Operation::CreateNode { id: 1 },
        Operation::CreateNode { id: 2 },
        Operation::CreateRelationship {
            rel: 1,
            source: 1,
            target: 2,
            relationship_type: 1,
        },
    ];
    let actual = [RelationshipRow {
        rel: report.state.relationship.rel,
        source: report.state.relationship.source,
        target: report.state.relationship.target,
        relationship_type: report.state.relationship.relationship_type,
    }];
    compare_read_view_expansion(seed, &expected, 1, &actual)
        .map_err(|difference| format!("recovery adjacency mismatch: {difference:?}"))?;
    if compare_read_view_expansion(seed, &expected, 1, &[]).is_ok() {
        return Err("recovery comparator accepted a missing committed edge".into());
    }
    let state = (
        report.state.generation,
        report.state.sequence,
        report.state.first_node,
        report.state.second_node,
    );
    compare_state((1, 1, 1, 2), state).map_err(str::to_owned)?;
    if compare_state((2, 1, 1, 2), state).is_ok() {
        return Err("recovery comparator accepted an incorrect generation".into());
    }

    let mut seen = BTreeSet::new();
    for receipt in report.receipts {
        if !receipt.key.starts_with("property-graph.recovery.")
            || !seen.insert(receipt.key)
            || receipt.clean_controls == 0
        {
            return Err(format!("invalid recovery receipt {}", receipt.key));
        }
        if [
            "property-graph.recovery.lost-ack",
            "property-graph.recovery.torn-tail",
            "property-graph.recovery.serial-orphan",
            "property-graph.recovery.checkpoint",
            "property-graph.recovery.read-only",
        ]
        .contains(&receipt.key)
            && receipt.fires == 0
        {
            return Err(format!("unfired recovery boundary {}", receipt.key));
        }
        coverage.hit(receipt.key);
    }
    if seen.len() != 9 {
        return Err("missing recovery boundary receipts".into());
    }
    Ok(())
}
