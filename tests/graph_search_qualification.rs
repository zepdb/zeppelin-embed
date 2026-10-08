#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::result_large_err
)]
#[path = "support/graph_search.rs"]
mod graph_search;
use graph_search::*;
use zeppelin_embed::lifecycle::Store;
use zeppelin_embed_adversarial_oracle::graph_fixture as oracle;
#[test]
fn ze65_exact_eligible_rankings_match_oracle() {
    let c = Corpus::new();
    let expected = vec![
        vec![oracle::Cell::Node(6), oracle::Cell::Score(1.0f64.to_bits())],
        vec![oracle::Cell::Node(7), oracle::Cell::Score(4.0f64.to_bits())],
    ];
    oracle::compare_scored_rows(
        &expected,
        &observe(&structured_plan(c.graph(), 2)),
        ABSOLUTE,
        RELATIVE,
    )
    .unwrap();
    let actual=c.run("MATCH (n:Eligible) WITH collect(DISTINCT n) AS eligible CALL ze.vector_search([0,0],2,'exact',eligible) YIELD node,distance RETURN node,distance");
    oracle::compare_scored_rows(&expected, &observe(&actual), ABSOLUTE, RELATIVE).unwrap();
}
#[path = "support/graph_search_apps.rs"]
mod apps;
fn expectations(c: &Corpus, shape: usize) -> Vec<oracle::Row> {
    let q = match shape {
        0 => oracle::Query::ProjectEvidence {
            project: 1,
            limit: 20,
        },
        1 => oracle::Query::SemanticContext {
            vector: vec![0.0; 2],
            k: 20,
        },
        _ => oracle::Query::AliceProjectRanking {
            person: 2,
            project: 1,
            vector: vec![0.0; 2],
            k: 20,
        },
    };
    oracle::query(&c.snapshot(), &q).unwrap()
}
#[test]
fn ze65_application_queries_match_oracle() {
    let c = Corpus::new();
    for shape in 0..3 {
        let expected = expectations(&c, shape);
        for actual in [
            apps::structured_application(c.graph(), shape),
            c.run(apps::APPLICATIONS[shape]),
        ] {
            oracle::compare_scored_rows(&expected, &observe(&actual), ABSOLUTE, RELATIVE).unwrap();
            for report in actual.pools().reports {
                assert_eq!(report.generation, actual.metadata().generation);
            }
        }
    }
}
#[test]
fn ze65_membership_checkpoint_replay() {
    let mut c = Corpus::new();
    c.checkpoint().unwrap();
    c.node("a", "Eligible", None, None, 2, oracle::Operation::Put);
    c.node(
        "b",
        "Eligible",
        Some("cedar"),
        Some([3.0, 0.0]),
        2,
        oracle::Operation::Put,
    );
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
        key: key.clone(),
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
    check_search_snapshot(&c).unwrap();
    let expected = expectations(&c, 2);
    let before = c.run(apps::APPLICATIONS[2]);
    let bits = observe(&before);
    oracle::compare_scored_rows(&expected, &bits, ABSOLUTE, RELATIVE).unwrap();
    let generation = before.metadata().generation;
    c.reopen();
    check_search_snapshot(&c).unwrap();
    let after = c.run(apps::APPLICATIONS[2]);
    assert_eq!(after.metadata().generation, generation);
    oracle::compare_rows(&bits, &observe(&after), true).unwrap();
    for node in after.pools().nodes {
        let truth = c
            .snapshot()
            .nodes
            .into_iter()
            .find(|n| n.id == node.id.get())
            .unwrap();
        assert_eq!(node.revision.get(), truth.revision);
        assert_eq!(node.generation.get(), truth.generation);
    }
}
#[test]
fn ze65_compaction_preserves_logical_search() {
    let mut c = Corpus::new();
    c.node(
        "b",
        "Eligible",
        Some("amber amber"),
        Some([2.0, 0.0]),
        2,
        oracle::Operation::Put,
    );
    let queries = [
        apps::APPLICATIONS[1],
        apps::APPLICATIONS[2],
        "CALL ze.text_search('amber',20) YIELD node,score RETURN node,score",
        "CALL ze.hybrid_search([0,0],'amber',20,'exact') YIELD node,score,vector_distance,lexical_score RETURN node,score,vector_distance,lexical_score",
    ];
    let before: Vec<_> = queries.iter().map(|q| observe(&c.run(q))).collect();
    let mut relocated = 0;
    for _ in 0..4 {
        let r = c.graph().graph_maintain_step(&control()).unwrap();
        relocated += r.replaced_physical_refs;
        if r.cycle_complete {
            break;
        }
    }
    assert!(
        relocated > 0,
        "compaction must physically relocate indexed records"
    );
    for (q, expected) in queries.iter().zip(&before) {
        oracle::compare_rows(expected, &observe(&c.run(q)), true).unwrap();
    }
    c.reopen();
    for (q, expected) in queries.iter().zip(&before) {
        oracle::compare_rows(expected, &observe(&c.run(q)), true).unwrap();
    }
    println!("ZE65 relocated_refs={relocated}");
}
use zeppelin_embed::property_graph::query::completed::{
    ActualTier, CandidateCoverage, LegState, ScorePrecision,
};
#[test]
fn ze65_modal_and_empty_matrix() {
    let c = Corpus::new();
    let expected = oracle::query(
        &c.snapshot(),
        &oracle::Query::LexicalEvidence {
            terms: vec!["amber".into()],
            phrase: false,
            k: 20,
        },
    )
    .unwrap();
    let lexical=c.run("CALL ze.text_search('amber',20) YIELD node,score RETURN node,node.name,ze.stored_text(node) IS NOT NULL,ze.stored_text(node),score");
    oracle::compare_scored_rows(&expected, &observe(&lexical), ABSOLUTE, RELATIVE).unwrap();
    let hybrid=c.run("CALL ze.hybrid_search([0,0],'amber',20,'exact') YIELD node,score,vector_distance,lexical_score RETURN node,score,vector_distance,lexical_score");
    let rows = observe(&hybrid);
    let row = |id| {
        rows.iter()
            .find(|r| r[0] == oracle::Cell::Node(id))
            .unwrap()
    };
    assert_eq!(row(4)[2], oracle::Cell::Null); // text only
    assert_eq!(row(10)[3], oracle::Cell::Null); // vector only
    assert!(matches!(row(6)[2], oracle::Cell::Score(_)));
    assert!(matches!(row(6)[3], oracle::Cell::Score(_)));
    assert!(!rows.iter().any(|r| {
        [1, 2, 5, 11, 12]
            .iter()
            .any(|id| r[0] == oracle::Cell::Node(*id))
    }));
    let zero=c.run("CALL ze.hybrid_search([0,0],'quartz',20,'exact') YIELD node,vector_distance,lexical_score RETURN node,vector_distance,lexical_score");
    assert_eq!(
        observe(&zero)
            .iter()
            .find(|r| r[0] == oracle::Cell::Node(6))
            .unwrap()[2],
        oracle::Cell::Score(0.0f64.to_bits())
    );
    assert_eq!(
        zero.pools().reports[0].lexical_leg,
        LegState::NoQueryMatches
    );
    for source in [
        "ze.vector_search([0,0],2,'exact'",
        "ze.text_search('amber',2",
        "ze.hybrid_search([0,0],'amber',2,'exact'",
    ] {
        let omitted = c.run(&format!("CALL {source}) YIELD node RETURN node"));
        assert!(omitted.metadata().rows > 0);
        let empty = c.run(&format!("CALL {source},[]) YIELD node RETURN node"));
        assert_eq!(empty.metadata().rows, 0);
        assert_eq!(empty.pools().reports.len(), 1);
        let projected = c.run(&format!(
            "CALL {source}) YIELD node RETURN count(*) LIMIT 0"
        ));
        assert_eq!(projected.metadata().rows, 0);
        assert_eq!(projected.pools().reports.len(), 1);
    }
    let two=c.run("CALL ze.text_search('amber',2) YIELD node AS a CALL ze.vector_search([0,0],3,'exact') YIELD node AS b RETURN a,b");
    assert_eq!(two.metadata().rows, 6);
    assert_eq!(two.pools().reports.len(), 2);
    let upstream = c.run("MATCH (n:Missing) CALL ze.text_search('amber',2) YIELD node RETURN node");
    assert_eq!(upstream.metadata().rows, 0);
    assert_eq!(upstream.pools().reports.len(), 1);
    let no_matches = c.run("CALL ze.text_search('quartz',2) YIELD node RETURN node");
    assert_eq!(no_matches.metadata().rows, 0);
    assert_eq!(
        no_matches.pools().reports[0].lexical_leg,
        LegState::NoQueryMatches
    );
    let independent_hybrid = oracle::query(
        &c.snapshot(),
        &oracle::Query::HybridProjectEvidence {
            project: 1,
            vector: vec![0.0; 2],
            terms: vec!["amber".into()],
            phrase: false,
            k: 20,
            alpha: 0.7,
        },
    )
    .unwrap();
    let actual_hybrid=c.run("MATCH (m:Meeting)-[:FOR_PROJECT]->(p:Project) MATCH (m)-[:HAS_CHUNK|HAS_ITEM*0..1]->(n) WITH collect(DISTINCT n) AS eligible CALL ze.hybrid_search([0,0],'amber',20,'exact',eligible) YIELD node,score,vector_distance,lexical_score RETURN node,node.name,score,vector_distance,lexical_score,vector_distance IS NOT NULL,lexical_score IS NOT NULL,ze.stored_text(node)");
    oracle::compare_scored_rows(
        &independent_hybrid,
        &observe(&actual_hybrid),
        ABSOLUTE,
        RELATIVE,
    )
    .unwrap();
    let bounded = c.run("MATCH (m:Meeting)-[path:HAS_CHUNK|MENTIONS*1..2]->(n) RETURN n,path");
    oracle::compare_rows(
        &oracle::query(
            &c.snapshot(),
            &oracle::Query::BoundedEvidence {
                meeting: 3,
                min: 1,
                max: 2,
            },
        )
        .unwrap(),
        &observe(&bounded),
        false,
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let empty = zeppelin_embed::lifecycle::Store::create_graph(
        dir.path().join("empty"),
        zeppelin_embed::lifecycle::OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        Some(zeppelin_embed::graph_commit_recovery_test_support::document()),
    )
    .unwrap();
    for q in [
        "CALL ze.vector_search([0,0],2,'exact') YIELD node RETURN node",
        "CALL ze.text_search('amber',2) YIELD node RETURN node",
        "CALL ze.hybrid_search([0,0],'amber',2,'exact') YIELD node RETURN node",
    ] {
        let r = zeppelin_embed_cypher::execute(
            &empty,
            &control(),
            &Default::default(),
            q,
            &[],
            Default::default(),
        )
        .unwrap();
        assert_eq!(r.metadata().rows, 0);
        assert_eq!(r.pools().reports.len(), 1);
    }
    empty.close_graph().unwrap();
}
#[test]
fn ze65_projection_preserves_ann_metadata() {
    let mut c = Corpus::new();
    c.ann_cohort();
    let exact=c.run("MATCH (n:Ann) WITH collect(DISTINCT n) AS eligible CALL ze.vector_search([0,0],20,'exact',eligible) YIELD node,distance RETURN node,distance");
    let approx=c.run("MATCH (n:Ann) WITH collect(DISTINCT n) AS eligible CALL ze.vector_search([0,0],20,'auto',eligible) YIELD node,distance RETURN node,distance");
    let report = approx.pools().reports[0];
    assert_eq!(report.actual_tier, Some(ActualTier::Graph));
    assert_eq!(report.coverage, CandidateCoverage::Approximate);
    assert_eq!(report.precision, ScorePrecision::Original);
    let truth = observe(&exact);
    let actual = observe(&approx);
    let recall = actual
        .iter()
        .filter(|r| truth.iter().any(|t| t[0] == r[0]))
        .count() as f64
        / 20.0;
    assert!(recall >= 0.95, "frozen ANN recall@20 threshold: {recall}");
    assert!(actual.iter().all(|r| match r[0] {
        oracle::Cell::Node(id) => id >= 13,
        _ => false,
    }));
    for r in &actual {
        let n = c
            .snapshot()
            .nodes
            .into_iter()
            .find(|n| Some(oracle::Cell::Node(n.id)) == r.first().cloned())
            .unwrap();
        let score = n
            .vector
            .unwrap()
            .iter()
            .map(|b| f64::from(f32::from_bits(*b)).powi(2))
            .sum::<f64>();
        oracle::compare_scored_rows(
            &[vec![r[0].clone(), oracle::Cell::Score(score.to_bits())]],
            std::slice::from_ref(r),
            ABSOLUTE,
            RELATIVE,
        )
        .unwrap();
    }
    for tail in [
        "RETURN node",
        "RETURN count(*)",
        "WITH node MATCH (node)-[*0..0]->(n) RETURN n",
    ] {
        let r=c.run(&format!("MATCH (n:Ann) WITH collect(DISTINCT n) AS eligible CALL ze.vector_search([0,0],20,'auto',eligible) YIELD node {tail}"));
        assert_eq!(r.pools().reports[0], report);
    }
    let two=c.run("CALL ze.vector_search([0,0],20,'auto') YIELD node AS a CALL ze.vector_search([1,1],20,'auto') YIELD node AS b RETURN count(*)");
    assert_eq!(two.metadata().rows, 1);
    assert_eq!(two.pools().reports.len(), 2);
    for (i, report) in two.pools().reports.iter().enumerate() {
        assert_eq!(report.call.0, i as u32);
        assert_eq!(report.actual_tier, Some(ActualTier::Graph));
        assert_eq!(report.coverage, CandidateCoverage::Approximate);
        assert_eq!(report.precision, ScorePrecision::Original);
    }
    println!(
        "ZE65 ANN rows=96 recall@20={recall} tier={:?} precision={:?} coverage={:?}",
        report.actual_tier, report.precision, report.coverage
    );
}
#[test]
fn ze65_configuration_preserves_graph_visibility() {
    use zeppelin_embed::lifecycle::OpenOptions;
    use zeppelin_embed::property_graph::*;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("no-vector");
    let store = Store::create_graph(
        &path,
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        None,
    )
    .unwrap();
    let content = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    store
        .graph_apply(
            &[zeppelin_embed::property_graph::staging::StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze65", "graph-only").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: zeppelin_embed::property_graph::staging::StructuredOperation::Create,
                image: Some(zeppelin_embed::property_graph::staging::WriteImage::Node(
                    &content,
                )),
            }],
            &control(),
        )
        .unwrap();
    let q = "MATCH (n) RETURN n";
    let r = zeppelin_embed_cypher::execute(
        &store,
        &control(),
        &Default::default(),
        q,
        &[],
        Default::default(),
    )
    .unwrap();
    assert_eq!(r.metadata().rows, 1);
    store.close_graph().unwrap();
    let store = Store::open_graph(
        &path,
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        None,
    )
    .unwrap();
    let r = zeppelin_embed_cypher::execute(
        &store,
        &control(),
        &Default::default(),
        q,
        &[],
        Default::default(),
    )
    .unwrap();
    assert_eq!(r.metadata().rows, 1);
    store.close_graph().unwrap();
    let mut c = Corpus::new();
    let before = observe(&c.run(q));
    assert_eq!(c.store.take().unwrap().close(), 0);
    let files = |path: &std::path::Path| -> std::collections::BTreeMap<_, _> {
        std::fs::read_dir(path)
            .unwrap()
            .map(|f| {
                let p = f.unwrap().path();
                (p.file_name().unwrap().to_owned(), std::fs::read(p).unwrap())
            })
            .collect()
    };
    let path = c.dir.path().join("graph");
    let original = files(&path);
    let mut incompatible = zeppelin_embed::graph_commit_recovery_test_support::document();
    incompatible.weights_digest.push(65);
    assert!(
        Store::open_graph(
            &path,
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
            Some(incompatible)
        )
        .is_err()
    );
    assert_eq!(files(&path), original);
    // Query model is supplied externally as coordinates; persisted document
    // declaration stays identical. Different external query coordinates are valid.
    c.store =
        Some(zeppelin_embed::graph_commit_recovery_test_support::ProbeStore::open(&path).unwrap());
    assert_eq!(observe(&c.run(q)), before);
    for vector in ["[0,0]", "[1,1]"] {
        let r = c.run(&format!(
            "CALL ze.vector_search({vector},2,'exact') YIELD node RETURN node"
        ));
        assert_eq!(r.metadata().rows, 2);
        assert_eq!(observe(&c.run(q)), before);
    }
}
#[path = "support/graph_search_retained.rs"]
mod retained;
#[test]
fn ze65_retained_view_materialization() {
    zeppelin_embed::graph_read_view_test_support::run_actual_probe(65);
    retained::retained(
        std::env::var("ZE_TEST_SEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
    );
}
