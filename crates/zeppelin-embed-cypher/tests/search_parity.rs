//! Independent structured plans and model answers for local ZE-58 extensions.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::result_large_err
)]
#[path = "support/search.rs"]
mod search;
mod support;
#[path = "support/tck.rs"]
mod tck;
use search::SearchFixture;
use zeppelin_embed::property_graph::query::completed::{CompletedGraphResult, GraphQueryOptions};
use zeppelin_embed::property_graph::query::plan::*;
use zeppelin_embed::property_graph::{GraphName, GraphPlanBacking, GraphQueryPlan};

/// Authored directly from application semantics, with no compiler inspection.
fn structured(f: &SearchFixture, shape: u8) -> CompletedGraphResult {
    let names = [
        String::from("Project"),
        String::from("Person"),
        String::from("ABOUT"),
        String::from("ATTENDED"),
        String::from("FROM_MEETING"),
    ];
    let n = |i: usize| GraphName::new(&names[i]).unwrap();
    let about = vec![n(2)];
    let attended = vec![n(3)];
    let from = vec![n(4)];
    let coordinates = vec![ExprId(0), ExprId(1)];
    let mut expressions = vec![
        Expression::Literal(Literal::F64(0.0)),
        Expression::Literal(Literal::F64(0.0)),
        Expression::List(&coordinates),
        Expression::Literal(Literal::I64(20)),
    ];
    if shape == 0 {
        expressions.clear();
    }
    // Each edge has its own MATCH scope, preserving parallel relationship paths.
    let expand = |source, node, relationship, types, direction, pattern| OperatorKind::Expand {
        source: SlotId(source),
        node: SlotId(node),
        relationship: SlotId(relationship),
        relationship_types: types,
        direction,
        pattern: PatternId(pattern),
    };
    let mut kinds = vec![OperatorKind::Unit];
    let mut aggregate = Vec::new();
    let mut eager = Vec::new();
    if shape == 0 {
        kinds.push(OperatorKind::ScanNodes {
            output: SlotId(0),
            label: Some(n(0)),
        });
        kinds.push(expand(0, 1, 2, about.as_slice(), Direction::Incoming, 0));
        kinds.push(expand(1, 3, 4, from.as_slice(), Direction::Incoming, 1));
    } else {
        let eligible = if shape == 2 {
            kinds.push(OperatorKind::ScanNodes {
                output: SlotId(5),
                label: Some(n(1)),
            });
            kinds.push(expand(5, 6, 7, attended.as_slice(), Direction::Outgoing, 0));
            kinds.push(expand(6, 8, 9, about.as_slice(), Direction::Outgoing, 1));
            kinds.push(expand(6, 10, 11, from.as_slice(), Direction::Incoming, 2));
            expressions.push(Expression::Slot(SlotId(10)));
            expressions.push(Expression::Aggregate {
                operation: AggregateExpression::Collect { distinct: true },
                operand: Some(ExprId(4)),
            });
            aggregate.push(Projection {
                slot: SlotId(12),
                expression: ExprId(5),
            });
            kinds.push(OperatorKind::Aggregate {
                keys: &[],
                aggregates: &aggregate,
            });
            expressions.push(Expression::Slot(SlotId(12)));
            Some(ExprId(6))
        } else {
            None
        };
        eager.push(PlanNodeId(kinds.len() as u32));
        kinds.push(OperatorKind::Search {
            call: SearchCallId(0),
            request: SearchRequest::Vector {
                vector: ExprId(2),
                k: ExprId(3),
                mode: SearchMode::Exact,
                eligible,
            },
            outputs: SearchOutputs {
                node: Some(SlotId(0)),
                distance: Some(SlotId(1)),
                ..Default::default()
            },
        });
        if shape == 1 {
            kinds.push(expand(0, 2, 3, from.as_slice(), Direction::Outgoing, 0));
        }
    }
    let slots: Vec<u32> = match shape {
        0 => vec![3, 1],
        1 => vec![0, 2, 1],
        _ => vec![0, 1],
    };
    let mut projections = Vec::new();
    for (i, slot) in slots.iter().enumerate() {
        let id = ExprId(expressions.len() as u32);
        expressions.push(Expression::Slot(SlotId(*slot)));
        projections.push(Projection {
            slot: SlotId(20 + i as u32),
            expression: id,
        });
    }
    if shape < 2 {
        let operand = ExprId(expressions.len() as u32);
        expressions.push(Expression::Slot(SlotId(if shape == 0 { 3 } else { 0 })));
        let id = ExprId(expressions.len() as u32);
        expressions.push(Expression::Unary {
            operation: UnaryExpression::StoredText,
            operand,
        });
        projections.push(Projection {
            slot: SlotId(24),
            expression: id,
        });
    }
    kinds.push(OperatorKind::Project(&projections));
    let edges: Vec<Vec<PlanNodeId>> = (0..kinds.len())
        .map(|i| {
            if i == 0 {
                vec![]
            } else {
                vec![PlanNodeId(i as u32 - 1)]
            }
        })
        .collect();
    let operators: Vec<_> = kinds
        .into_iter()
        .enumerate()
        .map(|(i, kind)| Operator {
            inputs: &edges[i],
            kind,
        })
        .collect();
    let mut backing = GraphPlanBacking::default();
    for name in &names {
        backing.string(name).unwrap();
    }
    for edge in &edges {
        backing.vec(edge).unwrap();
    }
    backing.vec(&coordinates).unwrap();
    backing.vec(&projections).unwrap();
    backing.vec(&aggregate).unwrap();
    backing.vec(&about).unwrap();
    backing.vec(&attended).unwrap();
    backing.vec(&from).unwrap();
    let parameters = Vec::new();
    let columns: &[&str] = match shape {
        0 => &["chunk", "meeting", "source_text"],
        1 => &["chunk", "meeting", "distance", "source_text"],
        _ => &["node", "distance"],
    };
    f.store()
        .query(
            &search::control(),
            &GraphQueryOptions::default(),
            &GraphQueryPlan {
                operators: &operators,
                expressions: &expressions,
                parameters: &parameters,
                eager_searches: &eager,
                root: PlanNodeId(operators.len() as u32 - 1),
                backing: &backing,
                bindings: &[],
                columns,
            },
        )
        .unwrap()
}
fn bag(r: &CompletedGraphResult) -> Vec<String> {
    let mut rows: Vec<_> = tck::actual_table(r)
        .1
        .iter()
        .map(|row| format!("{row:?}"))
        .collect();
    rows.sort();
    rows
}
#[test]
fn ze58_three_application_shapes_match_structured() {
    let f = SearchFixture::create();
    for (shape, q, count) in [
        (
            0,
            "MATCH (project:Project {key:'X'})<-[:ABOUT]-(meeting) MATCH (chunk)-[:FROM_MEETING]->(meeting) RETURN chunk,meeting,ze.stored_text(chunk) AS source_text",
            3,
        ),
        (
            1,
            "CALL ze.vector_search([0,0],20,'exact') YIELD node AS chunk,distance MATCH (chunk)-[:FROM_MEETING]->(meeting) RETURN chunk,meeting,distance,ze.stored_text(chunk) AS source_text",
            3,
        ),
        (
            2,
            "MATCH (person:Person {key:'Alice'})-[:ATTENDED]->(meeting) MATCH (meeting)-[:ABOUT]->(project:Project {key:'X'}) MATCH (chunk:Chunk)-[:FROM_MEETING]->(meeting) WITH collect(DISTINCT chunk) AS eligible CALL ze.vector_search([0,0],20,'exact',eligible) YIELD node,distance RETURN node,distance",
            2,
        ),
    ] {
        let cypher = f.run(q);
        let native = structured(&f, shape);
        assert_eq!(cypher.metadata().rows, count);
        assert_eq!(bag(&cypher), bag(&native));
        assert_eq!(cypher.metadata().generation, native.metadata().generation);
        assert_eq!(cypher.pools().reports, native.pools().reports);
        assert_eq!(
            cypher
                .pools()
                .columns
                .iter()
                .map(|c| c.kinds)
                .collect::<Vec<_>>(),
            native
                .pools()
                .columns
                .iter()
                .map(|c| c.kinds)
                .collect::<Vec<_>>()
        );
        if shape == 2 {
            let distances: Vec<_> = (0..2)
                .map(|i| match cypher.cell(i, 1) {
                    Some(zeppelin_embed::property_graph::query::completed::Value::F64(bits)) => {
                        f64::from_bits(*bits)
                    }
                    _ => panic!("distance type"),
                })
                .collect();
            assert_eq!(distances, [2.0, 50.0]);
        }
    }
}

use zeppelin_embed::property_graph::query::completed::SearchKind;
fn search_with_eligibility(
    store: &zeppelin_embed::property_graph::GraphStore,
    kind: SearchKind,
    k: i64,
    empty: bool,
    mode: SearchMode,
) -> Result<CompletedGraphResult, zeppelin_embed::property_graph::GraphStoreError> {
    let unit = vec![PlanNodeId(0)];
    let source = vec![PlanNodeId(1)];
    let coordinates = vec![ExprId(0), ExprId(1)];
    let text = String::from("amber");
    let mut expressions = Vec::new();
    if kind != SearchKind::Lexical {
        expressions.extend([
            Expression::Literal(Literal::F64(0.0)),
            Expression::Literal(Literal::F64(0.0)),
            Expression::List(&coordinates),
        ]);
    }
    let query = ExprId(expressions.len() as u32);
    if kind != SearchKind::Vector {
        expressions.push(Expression::Literal(Literal::String(&text)));
    }
    let count = ExprId(expressions.len() as u32);
    expressions.push(Expression::Literal(Literal::I64(k)));
    let eligible = if empty {
        let id = ExprId(expressions.len() as u32);
        expressions.push(Expression::List(&[]));
        Some(id)
    } else {
        None
    };
    let request = match kind {
        SearchKind::Vector => SearchRequest::Vector {
            vector: ExprId(2),
            k: count,
            mode,
            eligible,
        },
        SearchKind::Lexical => SearchRequest::Text {
            query,
            k: count,
            eligible,
        },
        SearchKind::Hybrid => SearchRequest::Hybrid {
            vector: ExprId(2),
            text: query,
            k: count,
            mode,
            eligible,
        },
    };
    let mut projections = Vec::new();
    let width = if kind == SearchKind::Hybrid { 4 } else { 2 };
    for i in 0..width {
        let expression = ExprId(expressions.len() as u32);
        expressions.push(Expression::Slot(SlotId(i)));
        projections.push(Projection {
            slot: SlotId(10 + i),
            expression,
        });
    }
    let outputs = SearchOutputs {
        node: Some(SlotId(0)),
        distance: (kind == SearchKind::Vector).then_some(SlotId(1)),
        score: (kind != SearchKind::Vector).then_some(SlotId(1)),
        vector_distance: (kind == SearchKind::Hybrid).then_some(SlotId(2)),
        lexical_score: (kind == SearchKind::Hybrid).then_some(SlotId(3)),
    };
    let operators = vec![
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &unit,
            kind: OperatorKind::Search {
                call: SearchCallId(0),
                request,
                outputs,
            },
        },
        Operator {
            inputs: &source,
            kind: OperatorKind::Project(&projections),
        },
    ];
    let eager = vec![PlanNodeId(1)];
    let parameters = Vec::new();
    let mut backing = GraphPlanBacking::default();
    backing.vec(&unit).unwrap();
    backing.vec(&source).unwrap();
    backing.vec(&coordinates).unwrap();
    backing.vec(&projections).unwrap();
    backing.string(&text).unwrap();
    store.query(
        &search::control(),
        &GraphQueryOptions::default(),
        &GraphQueryPlan {
            operators: &operators,
            expressions: &expressions,
            parameters: &parameters,
            eager_searches: &eager,
            root: PlanNodeId(2),
            backing: &backing,
            bindings: &[],
            columns: if width == 4 {
                &["n", "score", "distance", "lexical"]
            } else {
                &["n", "score"]
            },
        },
    )
}
#[test]
fn ze58_modality_values_and_reports_match_independent_plans() {
    let f = SearchFixture::create();
    for (kind, q) in [
        (
            SearchKind::Vector,
            "CALL ze.vector_search([0,0],4,'exact') YIELD node AS n,distance AS score RETURN n,score",
        ),
        (
            SearchKind::Lexical,
            "CALL ze.text_search('amber',4) YIELD node AS n,score RETURN n,score",
        ),
        (
            SearchKind::Hybrid,
            "CALL ze.hybrid_search([0,0],'amber',4,'exact') YIELD node AS n,score,vector_distance AS distance,lexical_score AS lexical RETURN n,score,distance,lexical",
        ),
    ] {
        let actual = f.run(q);
        let expected =
            search_with_eligibility(f.store(), kind, 4, false, SearchMode::Exact).unwrap();
        assert_eq!(bag(&actual), bag(&expected));
        assert_eq!(actual.pools().reports, expected.pools().reports);
        assert_eq!(actual.metadata().generation, expected.metadata().generation);
    }
}
