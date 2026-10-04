//! Directed ZE-46 consolidation and reclamation observations with an
//! independent graph oracle.

use super::coverage::CoverageRegistry;
use std::collections::BTreeSet;
use zeppelin_embed_adversarial_oracle::graph_adjacency_store::{
    Operation, RelationshipRow, compare_read_view_expansion,
};

/// Keys whose body must have fired at least one scheduled fault or refusal.
const FIRED: &[&str] = &[
    "property-graph.reclaim.stale-recheck",
    "property-graph.reclaim.inventory-fold",
    "property-graph.reclaim.spill-refusal",
    "property-graph.reclaim.orphan",
    "property-graph.reclaim.intent-unlink-completion",
    "property-graph.reclaim.page-relocation",
    "property-graph.reclaim.superseded-history",
    "property-graph.reclaim.read-only-retirement",
];

fn compare_replay(expected: (u64, bool), actual: (u64, bool)) -> Result<(), &'static str> {
    if expected == actual {
        Ok(())
    } else {
        Err("replayed receipt differs from the original installing commit")
    }
}

fn compare_bytes(removed: u64, unlinked: u64) -> Result<(), &'static str> {
    if removed != 0 && removed == unlinked {
        Ok(())
    } else {
        Err("reclaimed byte accounting differs from the unlinked files")
    }
}

pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    let report = zeppelin_embed::graph_reclaim_test_support::run_actual_probe(seed);
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
        .state
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
        .map_err(|difference| format!("post-reclaim adjacency mismatch: {difference:?}"))?;
    if compare_read_view_expansion(seed, &expected, 1, &[]).is_ok() {
        return Err("reclaim comparator accepted a missing committed relationship".into());
    }
    if (report.state.first_node, report.state.second_node) != (1, 2) {
        return Err("reclaim cycle changed a node identity".into());
    }
    let replay = (report.state.replay_generation, report.state.replayed);
    compare_replay((1, true), replay).map_err(str::to_owned)?;
    if compare_replay((2, true), replay).is_ok() {
        return Err("reclaim comparator accepted a wrong original replay generation".into());
    }
    compare_bytes(report.state.removed_bytes, report.state.unlinked_file_bytes)
        .map_err(str::to_owned)?;
    if compare_bytes(
        report.state.removed_bytes,
        report.state.unlinked_file_bytes + 1,
    )
    .is_ok()
    {
        return Err("reclaim comparator accepted wrong byte accounting".into());
    }

    let mut seen = BTreeSet::new();
    for receipt in report.receipts {
        if !receipt.key.starts_with("property-graph.reclaim.")
            || !seen.insert(receipt.key)
            || receipt.clean_controls == 0
        {
            return Err(format!("invalid reclaim receipt {}", receipt.key));
        }
        if FIRED.contains(&receipt.key) && receipt.fires == 0 {
            return Err(format!("unfired reclaim boundary {}", receipt.key));
        }
        coverage.hit(receipt.key);
    }
    let required: BTreeSet<_> = super::coverage::REQUIRED_GRAPH_SMOKE_COVERAGE
        .iter()
        .copied()
        .filter(|key| key.starts_with("property-graph.reclaim."))
        .collect();
    if seen != required {
        return Err("missing reclaim boundary receipts".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// Binds the real probe to the comparator without running the campaign.
    #[test]
    fn reclaim_probe_binds_actual_receipts_to_the_independent_comparator() {
        let mut coverage = super::CoverageRegistry::default();
        super::probe(0x5a45_0046, &mut coverage).expect("reclaim probe");
        assert_eq!(coverage.count("property-graph.reclaim.wal-only"), 1);
        assert_eq!(coverage.count("property-graph.reclaim.superseded-history"), 1);
        assert_eq!(
            coverage.count("property-graph.reclaim.maintenance-output"),
            1
        );
    }
}
