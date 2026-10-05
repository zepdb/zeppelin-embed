//! ZE-58 real-store Zeppelin search extensions (not original TCK).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
#[path = "support/search.rs"]
mod search;
mod support;
use search::SearchFixture;
use zeppelin_embed::property_graph::query::completed::Value;
#[test]
fn ze58_text_search_returns_real_rows() {
    let f = SearchFixture::create();
    let r = f.run("CALL ze.text_search('amber',2) YIELD node,score RETURN node,score");
    assert_eq!(r.metadata().rows, 2);
    assert_eq!(r.pools().reports.len(), 1);
    // Full-domain BM25: three indexed texts, lengths 2,1,1, df(amber)=2.
    let idf = (1.0_f64 + (3.0 - 2.0 + 0.5) / (2.0 + 0.5)).ln();
    for (row, len) in [(0, 1.0), (1, 2.0)] {
        let expected = idf * 2.2 / (1.0 + 1.2 * (0.25 + 0.75 * len / (4.0 / 3.0)));
        let Some(Value::F64(score)) = r.cell(row, 1) else {
            panic!("score type")
        };
        let score = f64::from_bits(*score);
        assert!((score - expected).abs() < 1e-6, "{score} != {expected}");
        assert!(matches!(r.cell(row, 0), Some(Value::Node(_))));
    }
}
use zeppelin_embed::property_graph::query::completed::{
    ActualTier, CandidateCoverage, GraphQueryErrorKind, GraphQueryOptions, LegState, ScorePrecision,
};
use zeppelin_embed::property_graph::query::runtime::WorkKind;
use zeppelin_embed_cypher::{ErrorKind, StatementError};
fn scalar(
    r: &zeppelin_embed::property_graph::query::completed::CompletedGraphResult,
    row: usize,
    col: usize,
) -> f64 {
    match r.cell(row, col) {
        Some(Value::F64(bits)) => f64::from_bits(*bits),
        other => panic!("expected f64: {other:?}"),
    }
}
#[test]
fn ze58_eligibility_domains_remain_distinct() {
    let f = SearchFixture::create();
    for (prefix, eligible, count, leg) in [
        ("", "", 3, LegState::Nonempty),
        ("", ", []", 0, LegState::NoEligibleMembers),
        (
            "MATCH (n:Chunk {key: 'a'}) WITH collect(DISTINCT n) AS e ",
            ", e",
            1,
            LegState::Nonempty,
        ),
        (
            "MATCH (n:Absent) WITH collect(DISTINCT n) AS e ",
            ", e",
            0,
            LegState::NoEligibleMembers,
        ),
        (
            "MATCH (n:Chunk {key: 'a'}) WITH collect(DISTINCT n) AS e WITH e AS forwarded ",
            ", forwarded",
            1,
            LegState::Nonempty,
        ),
    ] {
        let r=f.run(&format!("{prefix}CALL ze.vector_search([0,0],20,'exact'{eligible}) YIELD node,distance RETURN node,distance"));
        assert_eq!(r.metadata().rows, count);
        assert_eq!(r.pools().reports.len(), 1);
        assert_eq!(r.pools().reports[0].vector_leg, leg);
        if count == 1 {
            assert_eq!(scalar(&r, 0, 1), 2.0);
        }
    }
    let r=f.run("MATCH (p:Person)-[:ATTENDED]->(m) MATCH (c:Chunk)-[:FROM_MEETING]->(m) WITH collect(DISTINCT c) AS e CALL ze.vector_search([0,0],20,'exact',e) YIELD node,distance MATCH (node)-[:FROM_MEETING]->(m) RETURN distance");
    assert_eq!(r.metadata().rows, 3);
    assert_eq!(
        (0..3).map(|i| scalar(&r, i, 0)).collect::<Vec<_>>(),
        [2.0, 2.0, 50.0]
    );
}
#[test]
fn ze58_independent_calls_preserve_bags_and_eager_counts() {
    let f = SearchFixture::create();
    let calls = "CALL ze.text_search('amber',2) YIELD node AS a CALL ze.vector_search([0,0],3,'exact') YIELD node AS b";
    for (prefix, suffix, rows) in [
        ("", "", 6),
        ("MATCH (missing:Absent) ", "", 0),
        ("", " LIMIT 0", 0),
    ] {
        let r = f.run(&format!("{prefix}{calls} RETURN a,b{suffix}"));
        assert_eq!(r.metadata().rows, rows);
        if rows == 6 {
            let left = f.run("CALL ze.text_search('amber',2) YIELD node RETURN node");
            let right = f.run("CALL ze.vector_search([0,0],3,'exact') YIELD node RETURN node");
            let id=|result:&zeppelin_embed::property_graph::query::completed::CompletedGraphResult,row,col|match result.cell(row,col){Some(Value::Node(index))=>result.pools().nodes[*index as usize].id.get(),_=>panic!("node pair type")};
            let mut expected = Vec::new();
            for a in 0..2 {
                for b in 0..3 {
                    expected.push((id(&left, a, 0), id(&right, b, 0)));
                }
            }
            let mut actual: Vec<_> = (0..6).map(|row| (id(&r, row, 0), id(&r, row, 1))).collect();
            expected.sort();
            actual.sort();
            assert_eq!(actual, expected);
        }
        assert_eq!(r.pools().reports.len(), 2);
        assert_eq!(r.metadata().counters.get(WorkKind::SearchInvocations), 2);
        for (i, report) in r.pools().reports.iter().enumerate() {
            assert_eq!(report.call.0, i as u32);
            assert_eq!(report.generation, r.metadata().generation);
        }
    }
    let r=f.run("CALL ze.text_search('absent',2) YIELD node AS a CALL ze.vector_search([0,0],3,'exact') YIELD node AS b RETURN a,b");
    assert_eq!(r.metadata().rows, 0);
    assert_eq!(r.pools().reports.len(), 2);
    assert_eq!(r.metadata().counters.get(WorkKind::SearchInvocations), 2);
}
#[test]
fn ze58_reports_survive_projection_and_aggregation() {
    let f = SearchFixture::create();
    let base = f.run("CALL ze.text_search('amber',2) YIELD node,score RETURN node,score");
    for tail in [
        "RETURN node",
        "RETURN count(*)",
        "MATCH (node)-[:FROM_MEETING]->(m) RETURN m",
    ] {
        let r = f.run(&format!(
            "CALL ze.text_search('amber',2) YIELD node,score {tail}"
        ));
        assert_eq!(r.pools().reports, base.pools().reports);
    }
}
#[test]
fn ze58_modes_and_components_preserve_provenance() {
    let f = SearchFixture::create();
    for (mode, tier, precision, coverage) in [
        (
            "exact",
            ActualTier::Exact,
            ScorePrecision::Original,
            CandidateCoverage::Exact,
        ),
        (
            "scan",
            ActualTier::Scan,
            ScorePrecision::Quantized,
            CandidateCoverage::Approximate,
        ),
    ] {
        let r = f.run(&format!(
            "CALL ze.vector_search([0,0],3,'{mode}') YIELD node,distance RETURN node,distance"
        ));
        let report = r.pools().reports[0];
        assert_eq!(report.actual_tier, Some(tier));
        assert_eq!(report.precision, precision);
        assert_eq!(report.coverage, coverage);
        if mode == "exact" {
            assert_eq!(
                (0..3).map(|i| scalar(&r, i, 1)).collect::<Vec<_>>(),
                [0.0, 2.0, 50.0]
            );
        }
    }
    let default = f.run("CALL ze.vector_search([0,0],3,'default') YIELD node RETURN node");
    let auto = f.run("CALL ze.vector_search([0,0],3,'auto') YIELD node RETURN node");
    assert!(default.pools().reports[0].requested_tier.is_none());
    assert!(auto.pools().reports[0].requested_tier.is_some());
    let r=f.run("CALL ze.hybrid_search([0,0],'amber',4,'exact') YIELD node,score,vector_distance,lexical_score RETURN node,score,vector_distance,lexical_score");
    assert_eq!(r.metadata().rows, 3);
    // Independently calculated full-domain normalization: max squared norm 50,
    // max BM25 belongs to the length-one text. Small enclosure rounding is allowed.
    let idf = (1.0_f64 + 1.5 / 2.5).ln();
    let maximum = idf * 2.2 / (1.0 + 1.2 * (0.25 + 0.75 / (4.0 / 3.0)));
    assert_eq!(
        f64::from_bits(r.pools().reports[0].effective_alpha_bits),
        0.7
    );
    for row in 0..3 {
        let distance = scalar(&r, row, 2);
        let lexical = match r.cell(row, 3) {
            Some(Value::Null) => 0.0,
            Some(Value::F64(bits)) => f64::from_bits(*bits),
            other => panic!("lexical {other:?}"),
        };
        let expected = 0.7 * (1.0 - distance / 50.0) + 0.3 * lexical / maximum;
        assert!((scalar(&r, row, 1) - expected).abs() < 1e-6);
    }
    assert!((0..3).any(|i| matches!(r.cell(i, 3), Some(Value::Null))));
    let lexical_only=f.run("CALL ze.hybrid_search([0,0],'birch',4,'exact') YIELD node,vector_distance RETURN node,vector_distance");
    assert!(
        (0..lexical_only.metadata().rows as usize)
            .any(|i| matches!(lexical_only.cell(i, 1), Some(Value::Null)))
    );
    let zero=f.run("CALL ze.hybrid_search([0,0],'absent',4,'exact') YIELD node,lexical_score RETURN node,lexical_score");
    assert!(
        (0..3)
            .any(|i| matches!(zero.cell(i,1),Some(Value::F64(bits)) if f64::from_bits(*bits)==0.0))
    );
}
#[test]
fn ze58_search_results_outlive_close_and_reopen() {
    let mut f = SearchFixture::create();
    let q = "CALL ze.vector_search([0,0],3,'exact') YIELD node,distance RETURN node,ze.stored_text(node),distance";
    let before = f.run(q);
    let bytes = before.pools().bytes.to_vec();
    let nodes = before.pools().nodes.to_vec();
    let reports = before.pools().reports.to_vec();
    f.reopen();
    let after = f.run(q);
    assert_eq!(before.pools().bytes, bytes);
    assert_eq!(before.pools().nodes, nodes);
    assert_eq!(before.pools().reports, reports);
    assert_eq!(after.pools().bytes, bytes);
    assert_eq!(after.pools().nodes, nodes);
    assert_eq!(after.pools().reports, reports);
}
#[test]
fn ze58_search_compile_rejections_publish_nothing() {
    let mut f = SearchFixture::create();
    let generation = f.run("RETURN 1").metadata().generation;
    let spans = [
        (30, 35),
        (34, 37),
        (40, 45),
        (81, 82),
        (68, 69),
        (26, 29),
        (26, 30),
        (20, 22),
        (24, 27),
        (0, 35),
        (24, 25),
        (24, 28),
        (33, 41),
        (38, 51),
        (22, 27),
        (30, 43),
        (30, 37),
    ];
    for (q,(start,end)) in [
        "MATCH (n) CALL ze.text_search(n.key,1) YIELD node RETURN node",
        "MATCH (n) CALL ze.text_search('a',n.k) YIELD node RETURN node",
        "MATCH (n) CALL ze.vector_search([0,0],1,n.key) YIELD node RETURN node",
        "MATCH (n) WITH n.key AS group,collect(DISTINCT n) AS e CALL ze.text_search('a',1,e) YIELD node RETURN node",
        "MATCH (n) WITH n,collect(DISTINCT n) AS e CALL ze.text_search('a',1,e) YIELD node RETURN node",
        "CALL ze.text_search('a',1,[1]) YIELD node RETURN node",
        "CALL ze.text_search('a',1,null) YIELD node RETURN node",
        "CALL ze.text_search([],1) YIELD node RETURN node",
        "CALL ze.text_search('a','1') YIELD node RETURN node",
        "CALL ze.text_search('a') YIELD node RETURN node",
        "CALL ze.text_search('a',0) YIELD node RETURN node",
        "CALL ze.text_search('a',4097) YIELD node RETURN node",
        "CALL ze.text_search('a',1) YIELD distance RETURN distance",
        "CALL ze.text_search('a',1) YIELD node,node AS other RETURN node",
        "CALL ze.vector_search(['a'],1,'exact') YIELD node RETURN node",
        "CALL ze.vector_search([0,0],1,'approximate') YIELD node RETURN node",
        "CALL ze.vector_search([0,0],1,'EXACT') YIELD node RETURN node",
    ].into_iter().zip(spans) {
        let Err(StatementError::Compile(e)) = f.try_run(q, &[]) else {
            panic!("expected compile refusal {q}")
        };
        assert_eq!(e.kind, ErrorKind::SearchContext, "{q}: {e}");
        assert_eq!(e.span,zeppelin_embed_cypher::Span{start,end},"{q}");
        assert_eq!(f.run("RETURN 1").metadata().generation, generation);
    }
    let q = "WITH 1 AS node CALL ze.text_search('a',1) YIELD node RETURN node";
    let Err(StatementError::Compile(e)) = f.try_run(q, &[]) else {
        panic!("shadow accepted")
    };
    assert_eq!(e.kind, ErrorKind::DuplicateVariable);
    let calls = (0..8)
        .map(|i| format!("CALL ze.text_search('a',1) YIELD node AS n{i} "))
        .collect::<String>();
    assert_eq!(f.run(&format!("{calls}RETURN 1")).pools().reports.len(), 8);
    let q = format!("{calls}CALL ze.text_search('a',1) YIELD node AS n8 RETURN 1");
    let Err(StatementError::Compile(e)) = f.try_run(&q, &[]) else {
        panic!("ninth accepted")
    };
    assert_eq!(e.kind, ErrorKind::SearchContext);
    f.reopen();
    assert_eq!(f.run("RETURN 1").metadata().generation, generation);
}
#[test]
fn ze58_search_write_mixing_publish_nothing() {
    let mut f = SearchFixture::create();
    let before = f.run("MATCH (n) RETURN n ORDER BY ze.node_id(n)");
    for write in [
        "CREATE (:Forbidden)",
        "MATCH (n) SET n.x=1",
        "MATCH (n) REMOVE n.key",
        "MATCH (n) DELETE n",
    ] {
        for q in [
            format!("CALL ze.text_search('amber',1) YIELD node {write}"),
            format!("{write} CALL ze.text_search('amber',1) YIELD node RETURN node LIMIT 0"),
        ] {
            let Err(StatementError::Compile(e)) = f.try_run(&q, &[]) else {
                panic!("mix accepted {q}")
            };
            assert_eq!(e.kind, ErrorKind::Unsupported, "{e}");
            assert!(e.span.end <= q.len());
        }
    }
    f.reopen();
    let after = f.run("MATCH (n) RETURN n ORDER BY ze.node_id(n)");
    assert_eq!(before.metadata().generation, after.metadata().generation);
    assert_eq!(before.pools().nodes, after.pools().nodes);
    assert_eq!(before.pools().bytes, after.pools().bytes);
    assert_eq!(before.pools().properties, after.pools().properties);
}
#[test]
fn ze58_runtime_refusals_return_no_partial_result() {
    let f = SearchFixture::create();
    let generation = f.run("RETURN 1").metadata().generation;
    for q in [
        "CALL ze.vector_search([0],1,'exact') YIELD node RETURN node",
        "CALL ze.vector_search([1e300,0],1,'exact') YIELD node RETURN node",
    ] {
        let Err(StatementError::Query(e)) = f.try_run(q, &[]) else {
            panic!("runtime refusal missing {q}")
        };
        assert_eq!(e.kind(), GraphQueryErrorKind::Expression);
        assert!(e.nothing_committed());
    }
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1e300] {
        let bindings = [
            zeppelin_embed::property_graph::query::plan::ParameterBinding {
                name: "x",
                value: zeppelin_embed::property_graph::query::QueryValue::F64(value),
            },
        ];
        let error = f
            .try_run(
                "CALL ze.vector_search([$x,0],1,'exact') YIELD node RETURN node",
                &bindings,
            )
            .err()
            .unwrap();
        let StatementError::Query(e) = error else {
            panic!("parameter runtime refusal: {error}")
        };
        assert_eq!(e.kind(), GraphQueryErrorKind::Expression);
        assert!(e.nothing_committed());
    }
    let options = GraphQueryOptions::default()
        .with_result_row_limit(1)
        .unwrap();
    let error = zeppelin_embed_cypher::execute(
        f.store().statement_store(),
        &search::control(),
        &options,
        "CALL ze.text_search('amber',2) YIELD node RETURN node",
        &[],
        Default::default(),
    )
    .err()
    .unwrap();
    let StatementError::Query(e) = error else {
        panic!("runtime limit")
    };
    assert_eq!(e.kind(), GraphQueryErrorKind::Limit);
    assert!(e.nothing_committed());
    assert_eq!(
        f.run("CALL ze.text_search('amber',2) YIELD node RETURN node")
            .metadata()
            .rows,
        2
    );
    assert_eq!(f.run("RETURN 1").metadata().generation, generation);
}
#[test]
fn ze58_full_u128_ties_remain_ordered() {
    use zeppelin_embed::property_graph::staging::{
        StructuredOperation, StructuredWrite, WriteImage,
    };
    use zeppelin_embed::property_graph::{
        ApplicationKey, CanonicalContents, EntityKind, GraphRevision, GraphStore, NodeId, RelId,
    };
    let root = support::unique_temp_dir("ze58-high-id");
    let store = GraphStore::create_with_allocator_seed_for_test(
        &root,
        zeppelin_embed::lifecycle::OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        NodeId::new((1_u128 << 64) - 1).unwrap(),
        RelId::new(1).unwrap(),
    )
    .unwrap();
    for key in ["lo", "hi"] {
        let contents = CanonicalContents::node(&mut [], &mut [], Some("equal"), None).unwrap();
        store
            .apply_batch(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "ze58", key).unwrap(),
                    revision: GraphRevision::new(1).unwrap(),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&contents)),
                }],
                &search::control(),
            )
            .unwrap();
    }
    let mut observed = Vec::new();
    for tail in [
        "RETURN node,score",
        "RETURN node,score ORDER BY score DESC,ze.node_id(node)",
    ] {
        let r = zeppelin_embed_cypher::execute(
            store.statement_store(),
            &search::control(),
            &Default::default(),
            &format!("CALL ze.text_search('equal',2) YIELD node,score {tail}"),
            &[],
            Default::default(),
        )
        .unwrap();
        let ids: Vec<_> = (0..2)
            .map(|row| match r.cell(row, 0) {
                Some(Value::Node(index)) => r.pools().nodes[*index as usize].id.get(),
                _ => panic!("node"),
            })
            .collect();
        assert_eq!(ids, [(1_u128 << 64) - 1, 1_u128 << 64]);
        assert_eq!(scalar(&r, 0, 1), scalar(&r, 1, 1));
        observed.push(ids);
    }
    assert_eq!(observed[0], observed[1]);
    store.close().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn ze58_graph_coverage_requires_actual_traversal() {
    use zeppelin_embed::property_graph::staging::{
        StructuredOperation, StructuredWrite, WriteImage,
    };
    use zeppelin_embed::property_graph::{
        ApplicationKey, CanonicalContents, CanonicalEmbedding, EntityKind, GraphRevision,
    };
    let f = SearchFixture::create();
    let points: Vec<_> = (0..65).map(|i| [i as f32, 0.0]).collect();
    let keys: Vec<_> = (0..65).map(|i| format!("graph-{i}")).collect();
    let images: Vec<_> = points
        .iter()
        .map(|point| {
            CanonicalContents::node(
                &mut [],
                &mut [],
                Some("amber"),
                Some(CanonicalEmbedding::new(&f.tower, point).unwrap()),
            )
            .unwrap()
        })
        .collect();
    let writes: Vec<_> = keys
        .iter()
        .zip(&images)
        .map(|(key, image)| StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "ze58", key).unwrap(),
            revision: GraphRevision::new(1).unwrap(),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(image)),
        })
        .collect();
    f.apply(&writes);
    for mode in ["auto", "default"] {
        let r = f.run(&format!(
            "CALL ze.hybrid_search([0,0],'amber',1,'{mode}') YIELD node,score RETURN node,score"
        ));
        let report = r.pools().reports[0];
        assert_eq!(report.actual_tier, Some(ActualTier::Graph));
        assert_eq!(report.coverage, CandidateCoverage::Approximate);
        assert_eq!(report.precision, ScorePrecision::Original);
        assert!(report.candidate_count < 69);
        assert_eq!(report.cross_scored_count, report.candidate_count);
        assert!(report.cross_score_complete);
    }
}
