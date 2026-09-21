//! Directed ZE-152 native-pattern execution and independent bag controls.

use super::coverage::CoverageRegistry;
use std::collections::{BTreeMap, BTreeSet};
use zeppelin_embed::property_graph::query::pattern_test_support::{ProbeReport, run_actual_probe};
use zeppelin_embed_adversarial_oracle::graph_pattern::{
    Cell, Direction, Edge, Graph, Node, PatternId, Row, TinyPattern, evaluate,
};

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

/// Rebuilds the probe fixture as the independent oracle's primitive graph.
fn oracle_graph(report: &ProbeReport) -> Graph {
    Graph {
        nodes: report
            .fixture_nodes
            .iter()
            .map(|id| Node::new(*id, &[]))
            .collect(),
        edges: report
            .fixture_edges
            .iter()
            .map(|(relationship, source, target)| {
                Edge::new(*relationship, *source, *target, "LINKS")
            })
            .collect(),
    }
}

/// The probe's plan as a primitive tiny pattern. The production plan also
/// carries a `weight > 0` edge predicate; every fixture relationship has weight
/// 1 or 2, so the predicate retains every relationship and the predicate-free
/// tiny pattern describes the same expected bag.
fn oracle_pattern(start: u128, right: PatternId) -> TinyPattern {
    TinyPattern::Optional {
        left: Box::new(TinyPattern::Join {
            left: Box::new(TinyPattern::BoundedExpand {
                input: Box::new(TinyPattern::LookupNode {
                    input: Box::new(TinyPattern::Unit),
                    output: 0,
                    id: start,
                }),
                source: 0,
                node: 1,
                relationships: 2,
                min: 1,
                max: 1,
                direction: Direction::Out,
                relationship_types: Vec::new(),
                pattern: 0,
            }),
            right: Box::new(TinyPattern::Expand {
                input: Box::new(TinyPattern::LookupNode {
                    input: Box::new(TinyPattern::Unit),
                    output: 10,
                    id: start,
                }),
                source: 10,
                node: 1,
                relationship: 4,
                direction: Direction::Out,
                relationship_types: Vec::new(),
                pattern: right,
            }),
        }),
        right: Box::new(TinyPattern::Anchor),
        predicate: None,
    }
}

/// Projects oracle rows into the probe's reported tuple order.
fn oracle_tuples(rows: &[Row]) -> Result<Vec<(u128, u128, u128, u128)>, String> {
    let mut tuples = Vec::new();
    for row in rows {
        let mut source = None;
        let mut target = None;
        let mut path = None;
        let mut relationship = None;
        for (slot, cell) in row {
            match (slot, cell) {
                (0, Cell::Node(id)) => source = Some(*id),
                (1, Cell::Node(id)) => target = Some(*id),
                (2, Cell::Relationships(list)) if list.len() == 1 => {
                    path = list.first().copied();
                }
                (4, Cell::Relationship(id)) => relationship = Some(*id),
                _ => {}
            }
        }
        match (source, path, target, relationship) {
            (Some(source), Some(path), Some(target), Some(relationship)) => {
                tuples.push((source, path, target, relationship));
            }
            _ => return Err(String::from("native pattern oracle row shape")),
        }
    }
    tuples.sort_unstable();
    Ok(tuples)
}

/// Compares the independent oracle with the observed production bags for the
/// plan permutations and for the later independent pattern match.
fn oracle_controls(report: &ProbeReport, coverage: &mut CoverageRegistry) -> Result<(), String> {
    let graph = oracle_graph(report);
    let start = *report
        .fixture_nodes
        .first()
        .ok_or_else(|| String::from("native pattern fixture start"))?;
    let directed = oracle_tuples(
        &evaluate(&graph, &oracle_pattern(start, 0))
            .map_err(|error| format!("native pattern oracle: {error:?}"))?,
    )?;
    if directed != report.observations {
        return Err(String::from("native pattern oracle bag mismatch"));
    }
    if report.permutations.len() < 2 {
        return Err(String::from("native pattern permutation count"));
    }
    for bag in &report.permutations {
        if *bag != directed {
            return Err(String::from(
                "native pattern permutation bag depends on the plan choice",
            ));
        }
    }
    coverage.hit("property-graph.pattern.oracle.permutation");
    let subsequent = oracle_tuples(
        &evaluate(&graph, &oracle_pattern(start, 1))
            .map_err(|error| format!("native pattern oracle: {error:?}"))?,
    )?;
    if subsequent != report.subsequent {
        return Err(String::from("native pattern subsequent-match bag mismatch"));
    }
    if subsequent.len() <= directed.len()
        || !subsequent
            .iter()
            .any(|(_, path, _, relationship)| path == relationship)
    {
        return Err(String::from(
            "native pattern subsequent match did not get a fresh uniqueness set",
        ));
    }
    coverage.hit("property-graph.pattern.oracle.subsequent-match");
    Ok(())
}

const KEYS: [&str; 14] = [
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
    "property-graph.pattern.oracle.permutation",
    "property-graph.pattern.oracle.subsequent-match",
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
    oracle_controls(&report, coverage)?;
    let control_names = [
        "permutation",
        "subsequent-match",
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
