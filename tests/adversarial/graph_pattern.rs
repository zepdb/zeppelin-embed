//! Directed ZE-152 native-pattern execution and independent bag controls.

use super::coverage::CoverageRegistry;
use std::collections::{BTreeMap, BTreeSet};
use zeppelin_embed::property_graph::query::pattern_test_support::run_actual_probe;

fn exact_receipts(
    entries: Vec<(&'static str, u64)>,
    expected: &[&'static str],
    class: &str,
) -> Result<BTreeMap<&'static str, u64>, String> {
    if entries.len() != expected.len() {
        return Err(format!("native pattern {class} receipt count mismatch"));
    }
    let mut receipts = BTreeMap::new();
    for (name, count) in entries {
        if receipts.insert(name, count).is_some() {
            return Err(format!("duplicate native pattern {class} receipt: {name}"));
        }
    }
    for name in expected {
        if !receipts.contains_key(name) {
            return Err(format!("missing native pattern {class} receipt: {name}"));
        }
    }
    Ok(receipts)
}

const KEYS: [&str; 12] = [
    "property-graph.pattern.native-source",
    "property-graph.pattern.path-predicates",
    "property-graph.pattern.uniqueness",
    "property-graph.pattern.join-optional",
    "property-graph.pattern.full-id",
    "property-graph.pattern.retained-view",
    "property-graph.pattern.cancel.fire",
    "property-graph.pattern.limit.fire",
    "property-graph.pattern.late-error.fire",
    "property-graph.pattern.same-seed-control",
    "property-graph.pattern.release",
    "property-graph.pattern.oracle.can-fire",
];

pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    let report = run_actual_probe(seed)?;
    let observed = report.observations.iter().copied().fold(
        BTreeMap::<(u128, u128, u128, u128), usize>::new(),
        |mut bag, row| {
            *bag.entry(row).or_default() += 1;
            bag
        },
    );
    let expected = report.expected.iter().copied().fold(
        BTreeMap::<(u128, u128, u128, u128), usize>::new(),
        |mut bag, row| {
            *bag.entry(row).or_default() += 1;
            bag
        },
    );
    if observed != expected || expected.values().any(|count| *count != 1) {
        return Err(String::from("native pattern input-history bag mismatch"));
    }
    let mut missing = report.observations.clone();
    let _ = missing.pop();
    if missing == report.expected {
        return Err(String::from(
            "native pattern oracle accepted a missing edge",
        ));
    }
    let control_names = [
        "native-source",
        "path-predicates",
        "uniqueness",
        "join-optional",
        "full-id",
        "retained-view",
        "late-error",
        "release",
        "oracle",
    ];
    let fault_names = ["cancel", "limit"];
    let clean_names = ["same-seed"];
    let controls = exact_receipts(report.controls, &control_names, "control")?;
    let faults = exact_receipts(report.faults, &fault_names, "fault")?;
    let clean = exact_receipts(report.clean_controls, &clean_names, "clean control")?;
    for name in control_names {
        if controls.get(name).copied().unwrap_or(0) == 0 {
            return Err(format!("native pattern control did not fire: {name}"));
        }
    }
    for name in fault_names {
        if faults.get(name).copied().unwrap_or(0) == 0 {
            return Err(format!("native pattern fault did not fire: {name}"));
        }
    }
    if clean.get("same-seed").copied().unwrap_or(0) == 0 {
        return Err(String::from("native pattern same-seed control failed"));
    }
    let unique = KEYS.into_iter().collect::<BTreeSet<_>>();
    if unique.len() != KEYS.len() {
        return Err(String::from("duplicate native pattern coverage key"));
    }
    for key in KEYS {
        coverage.hit(key);
    }
    Ok(())
}
