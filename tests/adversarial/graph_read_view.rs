//! ZE-45 runner registration for the controlled native read-view boundary.

use super::coverage::CoverageRegistry;
use rand::RngCore;

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
    "property-graph.read-view.preparation-abort",
    "property-graph.read-view.oracle.can-fire",
    "property-graph.read-view.release",
];

#[derive(Debug, Default, Eq, PartialEq)]
pub struct Report {
    pub seed_draw: u64,
    pub actual_paths: usize,
}

pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<Report, String> {
    let mut rng = super::test_support::seeded_rng("property_graph::read_view", seed);
    let seed_draw = rng.next_u64();
    if !zeppelin_embed::graph_read_view_test_support::controlled_boundary_is_available() {
        return Err("ZE-45 controlled boundary was not linked into test support".into());
    }
    let actual_paths = zeppelin_embed::graph_read_view_test_support::run_actual_probe(seed_draw);
    if actual_paths != 15 {
        return Err(format!(
            "ZE-45 actual probe returned {actual_paths} paths, expected 15"
        ));
    }
    for key in REQUIRED_COVERAGE {
        coverage.hit(*key);
    }
    Ok(Report {
        seed_draw,
        actual_paths,
    })
}
