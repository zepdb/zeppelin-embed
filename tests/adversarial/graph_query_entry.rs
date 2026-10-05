//! Directed ZE-53 S3 / ZE-192 proof: the structured execution seam's write
//! path through its fault sites.

use super::coverage::CoverageRegistry;
use std::collections::BTreeSet;
use zeppelin_embed::property_graph::query::query_entry_test_support::run_actual_probe;

const RECEIPTS: [&str; 15] = [
    "incident.fire",
    "partial-append.fire",
    "wal-sync.fire",
    "publish.fire",
    "mid-drain.fire",
    "image-limit.fire",
    "fence-only.commit",
    "fence-only.recovery",
    "precommit-cancel.fire",
    "indeterminate.fire",
    "post-commit-cancel.commit",
    "oracle.can-fire",
    "search-preparation.fire",
    "search-report.retain",
    "search-publication.same-view",
];

const KEYS: [&str; 16] = [
    "property-graph.query-entry.incident.fire",
    "property-graph.query-entry.partial-append.fire",
    "property-graph.query-entry.wal-sync.fire",
    "property-graph.query-entry.publish.fire",
    "property-graph.query-entry.mid-drain.fire",
    "property-graph.query-entry.image-limit.fire",
    "property-graph.query-entry.fence-only.commit",
    "property-graph.query-entry.fence-only.recovery",
    "property-graph.query-entry.precommit-cancel.fire",
    "property-graph.query-entry.indeterminate.fire",
    "property-graph.query-entry.post-commit-cancel.commit",
    "property-graph.query-entry.oracle.can-fire",
    "property-graph.query-entry.same-seed-control",
    "property-graph.query-entry.search-preparation.fire",
    "property-graph.query-entry.search-report.retain",
    "property-graph.query-entry.search-publication.same-view",
];

pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    let report = run_actual_probe(seed)?;
    if report.observations != report.expected {
        return Err(String::from("query entry independent oracle mismatch"));
    }
    if report.receipts.len() != RECEIPTS.len() {
        return Err(String::from("query entry receipt count mismatch"));
    }
    for (expected, (actual, count)) in RECEIPTS.into_iter().zip(&report.receipts) {
        if *actual != expected {
            return Err(format!(
                "query entry receipt mismatch: expected {expected}, got {actual}"
            ));
        }
        if *count == 0 {
            return Err(format!("query entry receipt did not fire: {actual}"));
        }
    }
    let mut perturbed = report.observations.clone();
    let Some(fence) = perturbed.last_mut() else {
        return Err(String::from("query entry probe returned no observation"));
    };
    fence.0 ^= 1;
    if perturbed == report.expected {
        return Err(String::from(
            "query entry oracle accepted a perturbed fence identity",
        ));
    }
    let paired = run_actual_probe(seed)?;
    if paired != report {
        return Err(String::from("query entry paired clean mismatch"));
    }
    if KEYS.into_iter().collect::<BTreeSet<_>>().len() != KEYS.len() {
        return Err(String::from("duplicate query entry coverage key"));
    }
    for key in KEYS {
        coverage.hit(key);
    }
    Ok(())
}
