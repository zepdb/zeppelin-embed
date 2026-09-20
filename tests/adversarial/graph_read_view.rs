//! ZE-45 runner registration for the controlled native read-view boundary.

use super::coverage::CoverageRegistry;
use rand::RngCore;
use std::collections::BTreeSet;
use zeppelin_embed::graph_read_view_test_support::{ActualProbeReport, ObservedRelationship};

pub const REQUIRED_COVERAGE: &[&str] = &[
    "property-graph.read-view.admission-capture",
    "property-graph.read-view.old-lazy-open",
    "property-graph.read-view.coherent-reads",
    "property-graph.read-view.cursor-mismatch",
    "property-graph.read-view.source-identity-format",
    "property-graph.read-view.io.fire",
    "property-graph.read-view.io.clean",
    "property-graph.read-view.memory.fire",
    "property-graph.read-view.memory.clean",
    "property-graph.read-view.work.fire",
    "property-graph.read-view.work.clean",
    "property-graph.read-view.caller-cancel.fire",
    "property-graph.read-view.caller-cancel.clean",
    "property-graph.read-view.close-first-drain",
    "property-graph.read-view.close-drain-last-owner",
    "property-graph.read-view.close-drop-last-owner",
    "property-graph.read-view.preparation-abort",
    "property-graph.read-view.oracle.can-fire",
    "property-graph.read-view.release",
];

#[derive(Debug, Default, Eq, PartialEq)]
pub struct Report {
    pub seed_draw: u64,
    pub actual_paths: usize,
    pub comparator: &'static str,
}

fn expected_operations() -> Vec<zeppelin_embed_adversarial_oracle::graph_adjacency_store::Operation>
{
    use zeppelin_embed_adversarial_oracle::graph_adjacency_store::Operation;
    let node_a = (1_u128 << 100) + 1;
    let node_b = node_a + 1;
    let rel = 1_u128 << 110;
    vec![
        Operation::CreateNode { id: node_a },
        Operation::CreateNode { id: node_b },
        Operation::CreateRelationship {
            rel: rel + 1,
            source: node_a,
            target: node_b,
            relationship_type: 1,
        },
        Operation::CreateRelationship {
            rel: rel + 2,
            source: node_a,
            target: node_a,
            relationship_type: 2,
        },
        Operation::CreateRelationship {
            rel: rel + 3,
            source: node_a,
            target: node_b,
            relationship_type: 1,
        },
    ]
}

fn compare_actual_relationships(rows: &[ObservedRelationship]) -> Result<(), String> {
    use zeppelin_embed_adversarial_oracle::graph_adjacency_store::{
        RelationshipRow, compare_read_view_expansion,
    };
    let observed = rows
        .iter()
        .map(|row| RelationshipRow {
            rel: row.rel,
            source: row.source,
            target: row.target,
            relationship_type: row.relationship_type,
        })
        .collect::<Vec<_>>();
    compare_read_view_expansion(1, &expected_operations(), (1_u128 << 100) + 1, &observed)
        .map_err(|difference| format!("ZE-129 read-view difference: {difference:?}"))
}

pub(crate) fn record_report(
    report: &ActualProbeReport,
    coverage: &mut CoverageRegistry,
) -> Result<usize, String> {
    let mut seen = BTreeSet::new();
    for receipt in &report.receipts {
        if receipt.key == "property-graph.read-view.oracle.can-fire"
            || !REQUIRED_COVERAGE.contains(&receipt.key)
        {
            return Err(format!("unknown ZE-45 receipt {}", receipt.key));
        }
        if !seen.insert(receipt.key) {
            return Err(format!("duplicate ZE-45 receipt {}", receipt.key));
        }
        let observed = if receipt.key.ends_with(".fire") {
            receipt.fires > 0
        } else if receipt.key.ends_with(".clean") {
            receipt.clean_controls > 0
        } else {
            receipt.fires.saturating_add(receipt.clean_controls) > 0
        };
        if !observed {
            return Err(format!("unobserved ZE-45 receipt {}", receipt.key));
        }
        coverage.hit(receipt.key);
    }
    for key in REQUIRED_COVERAGE {
        if *key != "property-graph.read-view.oracle.can-fire" && !seen.contains(key) {
            return Err(format!("missing ZE-45 receipt {key}"));
        }
    }
    compare_actual_relationships(&report.relationships)?;
    coverage.hit("property-graph.read-view.oracle.can-fire");
    Ok(seen.len() + 1)
}

pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<Report, String> {
    let mut rng = super::test_support::seeded_rng("property_graph::read_view", seed);
    let seed_draw = rng.next_u64();
    if !zeppelin_embed::graph_read_view_test_support::controlled_boundary_is_available() {
        return Err("ZE-45 controlled boundary was not linked into test support".into());
    }
    let actual = zeppelin_embed::graph_read_view_test_support::run_actual_probe(seed_draw);
    let actual_paths = record_report(&actual, coverage)?;
    Ok(Report {
        seed_draw,
        actual_paths,
        comparator:
            zeppelin_embed_adversarial_oracle::graph_adjacency_store::READ_VIEW_COMPARATOR_ID,
    })
}
