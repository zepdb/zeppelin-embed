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

fn compare_detach_sweep(actual: (bool, u64)) -> bool {
    actual == (false, 2)
}

fn compare_bytes(removed: u64, unlinked: u64) -> Result<(), &'static str> {
    if removed != 0 && removed == unlinked {
        Ok(())
    } else {
        Err("reclaimed byte accounting differs from the unlinked files")
    }
}

fn compare_race(
    report: &zeppelin_embed::graph_reclaim_test_support::RaceProbeReport,
) -> Result<(), String> {
    if report.observation != report.control {
        return Err(String::from(
            "reader race differs from same-seed serialized control",
        ));
    }
    Ok(())
}

pub fn race_probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    race_observation(seed, coverage).map(|_| ())
}
pub fn race_observation(
    seed: u64,
    coverage: &mut CoverageRegistry,
) -> Result<zeppelin_embed_bench::harness_json::Value, String> {
    let report = zeppelin_embed::graph_reclaim_test_support::run_ze176_race_probe(seed);
    compare_race(&report)?;
    let mut perturbed = report.clone();
    perturbed.observation.0 += 1;
    if compare_race(&perturbed).is_ok() {
        return Err(String::from(
            "reader race comparator accepted altered generation",
        ));
    }
    for key in [
        "property-graph.reclaim.capture-race",
        "property-graph.reclaim.publication-race",
        "property-graph.reclaim.lazy-after-sweep",
        "property-graph.reclaim.release-unlink",
    ] {
        coverage.hit(key);
    }
    Ok(zeppelin_embed_bench::harness_json::json!({
        "observation": report.observation, "control": report.control
    }))
}

pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    observe(seed, coverage).map(|_| ())
}
pub fn observe(
    seed: u64,
    coverage: &mut CoverageRegistry,
) -> Result<zeppelin_embed_bench::harness_json::Value, String> {
    let report = zeppelin_embed::graph_reclaim_test_support::run_actual_probe(seed);
    if !compare_detach_sweep(report.detach_sweep) {
        return Err("DETACH sweep changed visibility or original installing generation".into());
    }
    if compare_detach_sweep((true, 2)) || compare_detach_sweep((false, 3)) {
        return Err("DETACH sweep comparator accepted wrong visibility/generation".into());
    }
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
    // The expected history installs the edge in generation 1; the seed is
    // only a probe input, not an oracle generation.
    compare_read_view_expansion(1, &expected, 1, &actual)
        .map_err(|difference| format!("post-reclaim adjacency mismatch: {difference:?}"))?;
    if compare_read_view_expansion(1, &expected, 1, &[]).is_ok() {
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

    let race_keys = [
        "property-graph.reclaim.capture-race",
        "property-graph.reclaim.publication-race",
        "property-graph.reclaim.lazy-after-sweep",
        "property-graph.reclaim.release-unlink",
    ];
    for key in race_keys {
        coverage.hit(key);
    }
    let mut seen = BTreeSet::new();
    for receipt in &report.receipts {
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
        .filter(|key| key.starts_with("property-graph.reclaim.") && !race_keys.contains(key))
        .collect();
    if seen != required {
        return Err("missing reclaim boundary receipts".into());
    }
    Ok(zeppelin_embed_bench::harness_json::json!({
        "state": format!("{:?}", report.state), "detach_sweep": report.detach_sweep,
        "receipts": report.receipts.iter().map(|r| zeppelin_embed_bench::harness_json::json!({"key": r.key, "fires": r.fires, "controls": r.clean_controls})).collect::<Vec<_>>()
    }))
}

#[cfg(test)]
mod tests {
    #[test]
    fn reclaim_probe_seed_zero_validates_committed_adjacency() {
        super::probe(0, &mut super::CoverageRegistry::default()).unwrap();
    }

    /// Binds the real probe to the comparator without running the campaign.
    #[test]
    fn reclaim_probe_binds_actual_receipts_to_the_independent_comparator() {
        let mut coverage = super::CoverageRegistry::default();
        super::probe(0x5a45_0046, &mut coverage).expect("reclaim probe");
        assert_eq!(coverage.count("property-graph.reclaim.wal-only"), 1);
        assert_eq!(
            coverage.count("property-graph.reclaim.superseded-history"),
            1
        );
        assert_eq!(
            coverage.count("property-graph.reclaim.maintenance-output"),
            1
        );
    }
}
