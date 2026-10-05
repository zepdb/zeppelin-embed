//! Integrated corpus search after actual artifact/WAL/checkpoint failures.
#[path = "../support/graph_search_apps.rs"]
mod apps;
#[path = "../support/graph_search.rs"]
mod graph_search;
#[path = "../support/graph_search_retained.rs"]
mod retained;
use super::coverage::CoverageRegistry;
use super::fault_vfs::{
    FaultEvent, FaultMode, FaultSchedule, FaultSite, Layer, ScheduledVfs, SimulatedCrashVfs,
};
use graph_search::*;
use rand::Rng;
use std::sync::Arc;
use zeppelin_embed::graph_commit_recovery_test_support::{ProbeStore, document};
use zeppelin_embed::property_graph::staging::*;
use zeppelin_embed::property_graph::*;
use zeppelin_embed::vfs::StdVfs;
use zeppelin_embed_adversarial_oracle::graph_fixture as oracle;
pub const REQUIRED_COVERAGE: &[&str] = &[
    "property-graph.search-qualification.membership",
    "property-graph.search-qualification.artifact.fire",
    "property-graph.search-qualification.wal.fire",
    "property-graph.search-qualification.checkpoint.fire",
    "property-graph.search-qualification.replay",
    "property-graph.search-qualification.materialization",
    "property-graph.search-qualification.same-seed-control",
    "property-graph.search-qualification.comparator",
    "property-graph.search-qualification.release",
    "property-graph.search-qualification.retained",
];
pub fn schedule(seed: u64) -> [(&'static str, FaultSite, Option<&'static str>); 3] {
    let mut rng = super::test_support::seeded_rng("ze65-search-schedule", seed);
    let mut sites = [
        ("artifact", FaultSite::Sync, Some(".zgraph")),
        ("wal", FaultSite::Sync, Some("graph-wal-")),
        ("checkpoint", FaultSite::Rename, None),
    ];
    if rng.random::<bool>() {
        sites.reverse();
    }
    sites
}
fn check(c: &Corpus) -> Result<(), String> {
    for (shape, query) in [
        oracle::Query::ProjectEvidence {
            project: 1,
            limit: 20,
        },
        oracle::Query::SemanticContext {
            vector: vec![0.0; 2],
            k: 20,
        },
        oracle::Query::AliceProjectRanking {
            person: 2,
            project: 1,
            vector: vec![0.0; 2],
            k: 20,
        },
    ]
    .iter()
    .enumerate()
    {
        let expected = oracle::query(&c.snapshot(), query)?;
        let actual = c.run(apps::APPLICATIONS[shape]);
        oracle::compare_scored_rows(&expected, &observe(&actual), ABSOLUTE, RELATIVE)?;
        for report in actual.pools().reports {
            if report.generation != actual.metadata().generation {
                return Err("mixed source/report generation".into());
            }
        }
    }
    check_search_snapshot(c)
}
pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    for (name, site, needle) in schedule(seed) {
        for fault in [true, false] {
            let crash = SimulatedCrashVfs::new(StdVfs);
            let event = FaultEvent {
                id: format!("ZE65-{seed}-{name}"),
                op_index: 1,
                layer: Layer::Io,
                site,
                mode: FaultMode::Eio,
                nth_match: 1,
                expected_matches: None,
                deadline_budget_seconds: None,
                path_contains: needle.map(str::to_owned),
                fired: false,
                fire_count: 0,
                path: None,
            };
            let vfs = Arc::new(ScheduledVfs::new(
                crash.clone(),
                if fault {
                    FaultSchedule::single(event)
                } else {
                    FaultSchedule::default()
                },
            ));
            let mut c = Corpus::with_vfs(vfs.clone());
            c.store
                .as_ref()
                .unwrap()
                .checkpoint()
                .map_err(|e| e.to_string())?;
            // Create/replace/remove membership before the injected boundary. These
            // committed deltas survive either failure and modeled power loss.
            c.node("a", "Eligible", None, None, 2, oracle::Operation::Put);
            c.node(
                "empty",
                "Eligible",
                Some("amber"),
                Some([4.0, 0.0]),
                2,
                oracle::Operation::Put,
            );
            let key = oracle::Key {
                kind: oracle::Kind::Node,
                namespace: "ze65".into(),
                value: "outside".into(),
            };
            c.apply(&oracle::Mutation {
                key,
                operation: oracle::Operation::Delete,
                revision: 2,
                expected: oracle::Expectation::Entity(8),
                detach: false,
                image: None,
            });
            c.node(
                "outside",
                "Outside",
                Some("amber"),
                Some([0.0, 0.0]),
                3,
                oracle::Operation::Recreate,
            );
            let key = oracle::Key {
                kind: oracle::Kind::Node,
                namespace: "ze65".into(),
                value: "a".into(),
            };
            c.apply(&oracle::Mutation {
                key,
                operation: oracle::Operation::Delete,
                revision: 3,
                expected: oracle::Expectation::Entity(6),
                detach: true,
                image: None,
            });
            c.store
                .as_ref()
                .unwrap()
                .checkpoint()
                .map_err(|e| e.to_string())?;
            let before = observe(&c.run(apps::APPLICATIONS[2]));
            let mut rng = super::test_support::seeded_rng("ze65-membership-value", seed);
            let point = [rng.random_range(0.125..0.5), 0.0];
            if name == "checkpoint" {
                c.node(
                    "b",
                    "Eligible",
                    Some("cedar"),
                    Some(point),
                    2,
                    oracle::Operation::Put,
                );
            }
            vfs.set_operation(1);
            let result = if name == "checkpoint" {
                c.store.as_ref().unwrap().checkpoint().map(|_| ())
            } else {
                let tower = document();
                let mut labels = [GraphName::new("Eligible").unwrap()];
                let mut props = [
                    GraphProperty::new(
                        GraphName::new("excerpt").unwrap(),
                        PropertyValue::new(PropertyData::String("b")).unwrap(),
                    ),
                    GraphProperty::new(
                        GraphName::new("name").unwrap(),
                        PropertyValue::new(PropertyData::String("b")).unwrap(),
                    ),
                    GraphProperty::new(
                        GraphName::new("timestamp").unwrap(),
                        PropertyValue::new(PropertyData::I64(65)).unwrap(),
                    ),
                ];
                let image = CanonicalContents::node(
                    &mut labels,
                    &mut props,
                    Some("cedar"),
                    Some(CanonicalEmbedding::new(&tower, &point).unwrap()),
                )
                .unwrap();
                c.graph()
                    .apply_batch(
                        &[StructuredWrite {
                            key: ApplicationKey::new(EntityKind::Node, "ze65", "b").unwrap(),
                            revision: GraphRevision::new(2).unwrap(),
                            operation: StructuredOperation::Put(EntityId::Node(
                                NodeId::new(7).unwrap(),
                            )),
                            image: Some(WriteImage::Node(&image)),
                        }],
                        &control(),
                    )
                    .map(|_| ())
            };
            if fault {
                result.map_or_else(
                    |_| Ok(()),
                    |_| Err("injected failure returned success".to_owned()),
                )?;
                let events = vfs.events();
                if events.len() != 1 || events[0].fire_count != 1 || events[0].path.is_none() {
                    return Err(format!("unreached {name} receipt: {events:?}"));
                }
                println!("ZE65 seed={seed} {}", events[0].json_line());
            } else {
                result.map_err(|e| e.to_string())?;
            }
            // Match the independently maintained model only to acknowledged commits.
            if name != "checkpoint" && !fault {
                let key = oracle::Key {
                    kind: oracle::Kind::Node,
                    namespace: "ze65".into(),
                    value: "b".into(),
                };
                let mut m = c.model.history[&key].last.clone();
                m.operation = oracle::Operation::Put;
                m.revision = 2;
                m.expected = oracle::Expectation::Entity(7);
                if let Some(oracle::Image::Node { vector, text, .. }) = &mut m.image {
                    *vector = Some(point.map(f32::to_bits).to_vec());
                    *text = Some("cedar".into());
                }
                c.model.apply(&[m]).map_err(|e| format!("model {e:?}"))?;
            }
            let path = c.dir.path().join("graph");
            if c.store.take().unwrap().release() != 0 {
                return Err("query/writer owner retained".into());
            }
            crash.crash().map_err(|e| e.to_string())?;
            c.store = Some(ProbeStore::open(&path).map_err(|e| e.to_string())?);
            check(&c)?;
            let after = observe(&c.run(apps::APPLICATIONS[2]));
            if fault && name != "checkpoint" {
                oracle::compare_rows(&before, &after, true)?;
            }
            let mut dropped = after.clone();
            dropped.pop();
            if oracle::compare_rows(&after, &dropped, true).is_ok() {
                return Err("comparator accepted dropped membership".into());
            }
            coverage.hit("property-graph.search-qualification.membership");
            coverage.hit("property-graph.search-qualification.replay");
            coverage.hit("property-graph.search-qualification.materialization");
            coverage.hit("property-graph.search-qualification.comparator");
            if fault {
                coverage.hit(match name {
                    "artifact" => REQUIRED_COVERAGE[1],
                    "wal" => REQUIRED_COVERAGE[2],
                    _ => REQUIRED_COVERAGE[3],
                });
            } else {
                coverage.hit("property-graph.search-qualification.same-seed-control");
            }
            if c.store.take().unwrap().close() != 0 {
                return Err("recovered owner retained".into());
            }
            coverage.hit("property-graph.search-qualification.release");
        }
    }
    // Existing instance-scoped scheduled clocks qualify deadline/cancel/close
    // and execution row-budget release; retain their own distinct receipt keys.
    retained::retained(seed);
    coverage.hit("property-graph.search-qualification.retained");
    super::graph_cypher_search::probe(seed, coverage)?;
    Ok(())
}
