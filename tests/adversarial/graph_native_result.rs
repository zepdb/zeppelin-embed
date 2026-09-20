//! Directed ZE-156 actual-native completed-result proof.

use super::coverage::CoverageRegistry;
use std::collections::BTreeSet;
use zeppelin_embed::property_graph::query::native_result_test_support::run_actual_probe;

const RECEIPTS: [&str; 8] = [
    "copy",
    "identity",
    "same-view",
    "limit.fire",
    "control.fire",
    "late-error.fire",
    "release",
    "oracle.can-fire",
];

const KEYS: [&str; 8] = [
    "property-graph.native-result.copy",
    "property-graph.native-result.identity",
    "property-graph.native-result.same-view",
    "property-graph.native-result.limit.fire",
    "property-graph.native-result.control.fire",
    "property-graph.native-result.late-error.fire",
    "property-graph.native-result.release",
    "property-graph.native-result.oracle.can-fire",
];

pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    let report = run_actual_probe(seed)?;
    if report.observations != report.expected {
        return Err(String::from("native result independent oracle mismatch"));
    }
    if report.receipts.len() != RECEIPTS.len() {
        return Err(String::from("native result receipt count mismatch"));
    }
    for (expected, (actual, count)) in RECEIPTS.into_iter().zip(&report.receipts) {
        if *actual != expected {
            return Err(format!(
                "native result receipt mismatch: expected {expected}, got {actual}"
            ));
        }
        if *count == 0 {
            return Err(format!("native result receipt did not fire: {actual}"));
        }
    }
    let mut perturbed = report.observations.clone();
    let Some(first) = perturbed.first_mut() else {
        return Err(String::from("native result probe returned no observation"));
    };
    first.1 ^= 1;
    if perturbed == report.expected {
        return Err(String::from(
            "native result oracle accepted perturbed scalar",
        ));
    }
    let paired = run_actual_probe(seed)?;
    if paired != report {
        return Err(String::from("native result paired clean mismatch"));
    }
    if KEYS.into_iter().collect::<BTreeSet<_>>().len() != KEYS.len() {
        return Err(String::from("duplicate native result coverage key"));
    }
    for key in KEYS {
        coverage.hit(key);
    }
    Ok(())
}
