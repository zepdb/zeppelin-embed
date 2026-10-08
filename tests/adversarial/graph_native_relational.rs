//! Directed ZE-154 native relational receipts and negative controls.

use super::coverage::CoverageRegistry;
use std::collections::{BTreeMap, BTreeSet};
use zeppelin_embed::property_graph::query::native_relational_test_support::{
    NativeRelationalProbeReport, run_actual_probe,
};

const KEYS: [&str; 14] = [
    "property-graph.native-relational.pipeline",
    "property-graph.native-relational.representative",
    "property-graph.native-relational.group",
    "property-graph.native-relational.eligibility",
    "property-graph.native-relational.limit.fire",
    "property-graph.native-relational.cancel.fire",
    "property-graph.native-relational.late-error.fire",
    "property-graph.native-relational.release",
    "property-graph.native-relational.same-seed-control",
    "property-graph.native-relational.oracle.can-fire",
    "property-graph.native-relational.chunk-reservation.fire",
    "property-graph.native-relational.variable-reservation.fire",
    "property-graph.native-relational.row-cap.fire",
    "property-graph.native-relational.streaming-retention",
];

fn bag(rows: &[(u128, i64, u128, u128)]) -> BTreeMap<(u128, i64, u128, u128), usize> {
    rows.iter().copied().fold(BTreeMap::new(), |mut bag, row| {
        *bag.entry(row).or_default() += 1;
        bag
    })
}

fn accepted(
    observations: &[(u128, i64, u128, u128)],
    expected: &[(u128, i64, u128, u128)],
) -> bool {
    let expected = bag(expected);
    bag(observations) == expected && expected.values().all(|count| *count == 1)
}

pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    register(run_actual_probe(seed)?, coverage)
}

fn register(
    report: NativeRelationalProbeReport,
    coverage: &mut CoverageRegistry,
) -> Result<(), String> {
    if !accepted(&report.observations, &report.expected) {
        return Err(String::from("native relational receipt bag mismatch"));
    }
    let mut missing = report.observations.clone();
    let _ = missing.pop();
    if accepted(&missing, &report.expected) {
        return Err(String::from(
            "native relational oracle accepted missing row",
        ));
    }
    let mut duplicate = report.observations.clone();
    duplicate.extend(report.observations.iter().copied());
    if accepted(&duplicate, &report.expected) {
        return Err(String::from(
            "native relational oracle accepted duplicate row",
        ));
    }
    let mut wrong_representative = report.observations.clone();
    let row = wrong_representative
        .first_mut()
        .ok_or_else(|| String::from("native relational representative observation"))?;
    row.2 ^= 1_u128 << 96;
    if accepted(&wrong_representative, &report.expected) {
        return Err(String::from(
            "native relational oracle accepted wrong representative",
        ));
    }
    let expected_receipts = [
        "pipeline",
        "representative",
        "group",
        "eligibility",
        "limit",
        "cancel",
        "late-error",
        "release",
        "same-seed",
        "oracle",
        "chunk-reservation",
        "variable-reservation",
        "row-cap",
        "streaming-retention",
    ];
    if report.receipts.len() != expected_receipts.len() {
        return Err(String::from("native relational receipt count"));
    }
    let mut receipts = BTreeMap::new();
    for (name, count) in report.receipts {
        if receipts.insert(name, count).is_some() {
            return Err(format!("duplicate native relational receipt: {name}"));
        }
    }
    for name in expected_receipts {
        if receipts.get(name).copied().unwrap_or(0) == 0 {
            return Err(format!("native relational receipt did not fire: {name}"));
        }
    }
    if KEYS.into_iter().collect::<BTreeSet<_>>().len() != KEYS.len() {
        return Err(String::from("duplicate native relational coverage key"));
    }
    for key in KEYS {
        coverage.hit(key);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_relational_directed_probe_can_fire() {
        let seed = 0x5e15_4c01;
        let report = run_actual_probe(seed).expect("actual blocking faults and clean controls");
        for name in [
            "chunk-reservation",
            "variable-reservation",
            "row-cap",
            "streaming-retention",
        ] {
            let mut missed = report.clone();
            missed
                .receipts
                .iter_mut()
                .find(|(key, _)| *key == name)
                .unwrap()
                .1 = 0;
            let mut coverage = CoverageRegistry::default();
            let error = register(missed, &mut coverage)
                .expect_err("a missing capacity fault must be rejected");
            assert_eq!(
                error,
                format!("native relational receipt did not fire: {name}")
            );
            assert!(KEYS.iter().all(|key| coverage.count(key) == 0));
        }
        let mut coverage = CoverageRegistry::default();
        register(report.clone(), &mut coverage).expect("register all receipts");
        assert!(KEYS.iter().all(|key| coverage.count(key) > 0));
        assert_eq!(run_actual_probe(seed).expect("same seed control"), report);
    }
}
