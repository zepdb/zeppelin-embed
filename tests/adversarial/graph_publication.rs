//! Directed ZE-39 coordinator observations, independent adjacency comparison.

use super::coverage::CoverageRegistry;
use std::collections::BTreeSet;
use zeppelin_embed_adversarial_oracle::graph_adjacency_store::{
    Operation, RelationshipRow, compare_read_view_expansion,
};

pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    let report = zeppelin_embed::graph_publication_test_support::run_actual_probe(seed);
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
    let actual: Vec<_> = report
        .relationships
        .iter()
        .map(|row| RelationshipRow {
            rel: row.rel,
            source: row.source,
            target: row.target,
            relationship_type: row.relationship_type,
        })
        .collect();
    compare_read_view_expansion(seed, &expected, 1, &actual)
        .map_err(|difference| format!("publication adjacency mismatch: {difference:?}"))?;
    let mut missing = actual.clone();
    missing.pop();
    if compare_read_view_expansion(seed, &expected, 1, &missing).is_ok() {
        return Err("publication comparator accepted a missing committed edge".into());
    }
    let mut seen = BTreeSet::new();
    for receipt in report.receipts {
        if !receipt.key.starts_with("property-graph.publication.")
            || !seen.insert(receipt.key)
            || receipt.clean_controls == 0
        {
            return Err(format!("invalid publication receipt {}", receipt.key));
        }
        if [
            "property-graph.publication.precommit",
            "property-graph.publication.uncertain",
            "property-graph.publication.checkpoint",
            "property-graph.publication.creation",
        ]
        .contains(&receipt.key)
            && receipt.fires == 0
        {
            return Err(format!("unfired publication boundary {}", receipt.key));
        }
        coverage.hit(receipt.key);
    }
    if seen.len() != 9 {
        return Err("missing publication boundary receipts".into());
    }
    coverage.hit("property-graph.publication.oracle.can-fire");
    Ok(())
}
