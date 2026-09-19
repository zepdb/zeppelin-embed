//! Independent ZE-126 review probes by /root/ze55_binder; retained for regression.
//! Literal compiler-to-plan oracles; these do not execute graph queries.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod support;

use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use zeppelin_embed::property_graph::{
    GraphGeneration, StoreInstanceId,
    query::{QueryView, ValueContext, plan::*, resources::*},
    resources::GraphResources,
};
use zeppelin_embed_cypher::{CompileLimits, compile_read_in};

fn with_memory(run: impl FnOnce(&QueryMemory<'_>, &mut ValueContext<'_>)) {
    let path = support::unique_temp_dir("ze126-lowering");
    std::fs::create_dir(&path).unwrap();
    let options = OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024);
    let store = Store::open(&path, options).unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let memory = QueryMemory::new(&shared, 24 * 1024 * 1024).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    assert!(std::ptr::eq(context.control(), &control));
    let baseline = memory.reserved_bytes();
    run(&memory, &mut context);
    assert_eq!(memory.reserved_bytes(), baseline);
    drop(memory);
    drop(shared);
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn independent_patterns_are_lowered() {
    with_memory(|memory, context| {
        let mut failures = Vec::new();
        for query in [
            "MATCH (a), (b) RETURN a,b",
            "MATCH (a)-[r]->(b), (b)-[s]->(c) RETURN r,s",
            "MATCH (a)-[r]->(b) MATCH (c)-[r]->(d) RETURN a,b,c,d,r",
            "MATCH (a)-[r]->(a) RETURN a,r",
            "MATCH (a)-[r]->(b) WITH a,b,r MATCH (b)<-[r]-(a) RETURN r",
            "MATCH (a) OPTIONAL MATCH (b) WHERE a.x = b.x RETURN a,b",
            "OPTIONAL MATCH (a) WHERE false RETURN a",
            "MATCH (a) OPTIONAL MATCH (a) WHERE false RETURN a",
            "MATCH (a)-[r*0..2 {x:7}]->(b) RETURN r",
            "MATCH (a)-[r*0..2 {x:a.x}]->(b) RETURN r",
        ]
        .iter()
        {
            let binding = zeppelin_embed_cypher::compile_in(
                query,
                &[],
                CompileLimits::default(),
                memory,
                context.control(),
                |_| Ok(()),
            );
            let lowered = compile_read_in(
                query,
                &[],
                CompileLimits::default(),
                memory,
                context,
                |_, _| Ok(()),
            );
            if let Err(error) = lowered {
                failures.push(format!("{query}: binder={binding:?}, lower={error:?}"));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    });
}

#[test]
fn independent_projections_are_lowered() {
    with_memory(|memory, context| {
        let mut failures = Vec::new();
        for query in [
            "WITH 1 AS x RETURN DISTINCT -x AS x ORDER BY -x",
            "WITH 1 AS x, 2 AS y RETURN DISTINCT x AS y, y AS x ORDER BY x",
            "MATCH (n) RETURN DISTINCT n.name AS name ORDER BY n.name",
            "MATCH (n) RETURN count(*) AS c ORDER BY c",
            "MATCH (n) RETURN count(*) AS c, n.x AS x ORDER BY x",
            "MATCH (n) WITH n ORDER BY n.x DESC RETURN collect(n.x) AS xs",
            "MATCH (n) WITH n.x AS x ORDER BY n.y LIMIT 1 WHERE x>0 RETURN x",
            "MATCH(n) WITH n.x AS x, count(*) AS c WITH x,c RETURN x,c",
            "WITH [1,2] AS xs RETURN xs[-1], size(xs), 1 < 2 < 3",
            "MATCH(n) WITH * RETURN *",
            "MATCH(n) RETURN n.x AS x ORDER BY [n.y][0] LIMIT 0",
            "RETURN count(*) AS c",
        ]
        .iter()
        {
            let binding = zeppelin_embed_cypher::compile_in(
                query,
                &[],
                CompileLimits::default(),
                memory,
                context.control(),
                |_| Ok(()),
            );
            let lowered = compile_read_in(
                query,
                &[],
                CompileLimits::default(),
                memory,
                context,
                |_, _| Ok(()),
            );
            if let Err(error) = lowered {
                failures.push(format!("{query}: binder={binding:?}, lower={error:?}"));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    });
}

#[test]
fn independent_bound_path_self_reference_is_preserved() {
    with_memory(|memory, context| {
        let query = "MATCH (a)-[r*1..2 {x:size(r)}]->(b) RETURN r";
        zeppelin_embed_cypher::compile_in(
            query,
            &[],
            CompileLimits::default(),
            memory,
            context.control(),
            |_| Ok(()),
        )
        .unwrap();
        compile_read_in(
            query,
            &[],
            CompileLimits::default(),
            memory,
            context,
            |_, _| Ok(()),
        )
        .unwrap();
    });
}

#[test]
fn independent_order_alias_priority_and_pattern_identity() {
    with_memory(|memory, context| {
        let query = "WITH 1 AS x RETURN DISTINCT -x AS x ORDER BY -x";
        compile_read_in(query, &[], CompileLimits::default(), memory, context, |read, _| {
            let plan = read.plan().description();
            let sort = plan.operators.iter().find_map(|op| if let OperatorKind::Sort(keys) = op.kind { Some(keys) } else {None}).unwrap();
            let Expression::Unary { operation: UnaryExpression::Negate, operand } = plan.expressions[sort[0].expression.0 as usize] else { panic!("ORDER BY must negate projected alias") };
            assert!(matches!(plan.expressions[operand.0 as usize], Expression::Slot(slot) if slot==read.columns()[0].slot));
            Ok(())
        }).unwrap();
        for (query, wanted) in [
            ("MATCH (a)-[r]->(b), (b)-[s]->(c) RETURN r,s", [0, 0]),
            ("MATCH (a)-[r]->(b) MATCH (b)-[s]->(c) RETURN r,s", [0, 1]),
        ] {
            compile_read_in(
                query,
                &[],
                CompileLimits::default(),
                memory,
                context,
                |read, _| {
                    let got: Vec<_> = read
                        .plan()
                        .description()
                        .operators
                        .iter()
                        .filter_map(|op| {
                            if let OperatorKind::Expand { pattern, .. } = op.kind {
                                Some(pattern.0)
                            } else {
                                None
                            }
                        })
                        .collect();
                    assert_eq!(got, wanted);
                    Ok(())
                },
            )
            .unwrap();
        }
    });
}

#[test]
fn independent_reuse_and_projection_matrix() {
    with_memory(|memory, context| {
        let mut failures = Vec::new();
        for query in [
            "OPTIONAL MATCH (a) MATCH (a) RETURN a",
            "OPTIONAL MATCH (a)-[r]->(b) MATCH (b)<-[r]-(a) RETURN a,b,r",
            "MATCH (a)-[r]->(b) WITH a AS x, b AS y, r AS q MATCH (x)-[q]->(y) RETURN q",
            "MATCH (a)-[r]->(b) WITH a AS x, a AS y MATCH (x)-[s]->(y) RETURN x,y,s",
            "MATCH (a) OPTIONAL MATCH (a)-[r]->(b), (b)-[s]->(c) WHERE b.x=1 AND c.x=2 RETURN a,r,b,s,c",
            "MATCH (a) OPTIONAL MATCH (a)-[r]->(b) WITH a,r,b WHERE b IS NULL RETURN a,b",
            "MATCH (a)-[r]->(b) WITH a, b, collect(r) AS rs MATCH (a)-[s*0..2 {x:size(rs)}]->(b) RETURN s",
            "MATCH (n {x:n.x}) RETURN n",
            "MATCH (a)-[r {x:r.x}]->(b {x:b.x}) RETURN a,r,b",
            "MATCH (n) WITH n.x AS x, n.y AS y RETURN DISTINCT x+y AS x, x AS y ORDER BY x+y",
            "MATCH (n) RETURN n.x AS x, n.x AS y, count(*) AS c ORDER BY x,y",
            "MATCH (n) WITH count(*) AS c WHERE c>0 RETURN c",
            "MATCH (n) WITH DISTINCT n.x AS x WHERE x>0 RETURN x",
            "WITH 1 AS x RETURN x AS y ORDER BY x+y",
            "MATCH (n) RETURN n.x AS x ORDER BY x, n.x",
            "MATCH (n) RETURN collect(DISTINCT n) AS ns, count(DISTINCT n) AS c",
            "MATCH (n) WITH n AS x ORDER BY n.x RETURN x",
            "WITH 1 AS x WITH x AS y ORDER BY x+y WHERE y=1 RETURN y",
        ] {
            let binding = zeppelin_embed_cypher::compile_in(
                query,
                &[],
                CompileLimits::default(),
                memory,
                context.control(),
                |_| Ok(()),
            );
            let lowered = compile_read_in(
                query,
                &[],
                CompileLimits::default(),
                memory,
                context,
                |_, _| Ok(()),
            );
            if let Err(error) = lowered {
                failures.push(format!("{query}: binder={binding:?}, lower={error:?}"));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    });
}

#[test]
fn independent_reuse_reprojects_original_slots_and_optional_right_only_nulls() {
    with_memory(|memory, context| {
        compile_read_in(
            "MATCH (a)-[r]->(b) WITH a AS x, b AS y, r AS q MATCH (x)-[q]->(y) RETURN x,q,y",
            &[],
            CompileLimits::default(),
            memory,
            context,
            |read, _| {
                let plan = read.plan().description();
                let expands: Vec<_> = plan
                    .operators
                    .iter()
                    .enumerate()
                    .filter(|(_, op)| matches!(op.kind, OperatorKind::Expand { .. }))
                    .collect();
                assert_eq!(expands.len(), 2);
                let (second_index, second) = expands[1];
                let OperatorKind::Expand {
                    source,
                    node,
                    relationship,
                    ..
                } = second.kind
                else {
                    unreachable!()
                };
                assert_eq!(source, SlotId(3));
                assert!(node.0 >= 9 && relationship.0 >= 9 && node != relationship);
                let mut equality_pairs = Vec::new();
                for op in plan.operators.iter().skip(second_index + 1).take(2) {
                    let OperatorKind::Filter(predicate) = op.kind else {
                        panic!("reuse must compare before pruning")
                    };
                    let Expression::Binary {
                        operation:
                            BinaryExpression::Comparison(
                                zeppelin_embed::property_graph::query::Comparison::Equal,
                            ),
                        left,
                        right,
                    } = plan.expressions[predicate.0 as usize]
                    else {
                        unreachable!()
                    };
                    let (Expression::Slot(a), Expression::Slot(b)) = (
                        plan.expressions[left.0 as usize],
                        plan.expressions[right.0 as usize],
                    ) else {
                        unreachable!()
                    };
                    equality_pairs.push((a, b));
                }
                assert_eq!(
                    equality_pairs,
                    [(relationship, SlotId(5)), (node, SlotId(4))]
                );
                let OperatorKind::Project(prune) = plan.operators[second_index + 3].kind else {
                    unreachable!()
                };
                assert_eq!(
                    prune.iter().map(|p| p.slot).collect::<Vec<_>>(),
                    [SlotId(3), SlotId(4), SlotId(5)]
                );
                Ok(())
            },
        )
        .unwrap();
        compile_read_in("MATCH (a) OPTIONAL MATCH (a)-[r]->(b), (b)-[s]->(c) WHERE b.x=1 AND c.x=2 RETURN a,r,b,s,c", &[], CompileLimits::default(), memory, context, |read,_| {
            let plan=read.plan().description();
            let (index, optional)=plan.operators.iter().enumerate().find(|(_,op)|matches!(op.kind,OperatorKind::OptionalApply {..})).unwrap();
            let OperatorKind::OptionalApply {predicate:Some(predicate)}=optional.kind else {unreachable!()};
            assert!(matches!(plan.expressions[predicate.0 as usize],Expression::Binary {operation:BinaryExpression::And,..}));
            let left=optional.inputs[0];
            let mut right=optional.inputs[1];
            while right!=left { right=plan.operators[right.0 as usize].inputs[0]; }
            let facts=read.plan().facts(PlanNodeId(index as u32)).unwrap();
            assert_eq!(facts.slot(SlotId(0)),Some(ValueKinds::NODE));
            for (slot,kind) in [(1,ValueKinds::REL),(2,ValueKinds::NODE),(3,ValueKinds::REL),(4,ValueKinds::NODE)] {
                assert_eq!(facts.slot(SlotId(slot)),Some(kind.union(ValueKinds::NULL)));
            }
            Ok(())
        }).unwrap();
    });
}

#[test]
fn independent_parameter_order_and_source_mapped_errors() {
    use zeppelin_embed::property_graph::query::QueryValue;
    use zeppelin_embed_cypher::ErrorKind;
    with_memory(|memory, context| {
        let parameters = [
            ParameterBinding {
                name: "second",
                value: QueryValue::String("copied"),
            },
            ParameterBinding {
                name: "first",
                value: QueryValue::I64(9),
            },
        ];
        compile_read_in(
            "RETURN $first AS a, $second AS b",
            &parameters,
            CompileLimits::default(),
            memory,
            context,
            |read, _| {
                let plan = read.plan().description();
                let OperatorKind::Project(columns) = plan.operators[plan.root.0 as usize].kind
                else {
                    unreachable!()
                };
                assert!(matches!(
                    plan.expressions[columns[0].expression.0 as usize],
                    Expression::Parameter(ParameterId(1))
                ));
                assert!(matches!(
                    plan.expressions[columns[1].expression.0 as usize],
                    Expression::Parameter(ParameterId(0))
                ));
                assert_eq!(read.parameters()[0].name, "second");
                assert_eq!(read.parameters()[1].name, "first");
                for (projection, text) in columns.iter().zip(["$first", "$second"]) {
                    let span = read.expression_spans()[projection.expression.0 as usize];
                    assert_eq!(&read.source()[span.start..span.end], text);
                }
                Ok(())
            },
        )
        .unwrap();
        for (query, parameters, kind) in [
            ("RETURN $missing", Vec::new(), ErrorKind::Parameter),
            (
                "RETURN 1 LIMIT $cap",
                vec![ParameterBinding {
                    name: "cap",
                    value: QueryValue::I64(-1),
                }],
                ErrorKind::InvalidRange,
            ),
            ("CREATE (n) RETURN n", Vec::new(), ErrorKind::Unsupported),
        ] {
            let mut called = false;
            let error = compile_read_in(
                query,
                &parameters,
                CompileLimits::default(),
                memory,
                context,
                |_, _| {
                    called = true;
                    Ok(())
                },
            )
            .unwrap_err();
            assert!(!called);
            assert_eq!(error.kind, kind, "{query}: {error:?}");
            assert!(
                error.span.start < error.span.end && error.span.end <= query.len(),
                "{query}: {error:?}"
            );
        }
    });
}

#[test]
fn independent_completed_routing_walks_lists_index_and_whole_bag() {
    with_memory(|memory, context| {
        for (query, stages) in [
            (
                "MATCH (a)-[r*1..2 {x:size([r][0])+1,y:7}]->(b) RETURN r",
                &[2][..],
            ),
            (
                "MATCH (a)-[r*0..2]->(b)-[s*0..2 {x:size(r)}]->(c) RETURN s",
                &[0, 1][..],
            ),
            (
                "MATCH (a)-[r*0..2]->(b)-[s*0..2 {x:size(r)+size(s),y:7}]->(c) RETURN s",
                &[0, 2][..],
            ),
        ] {
            compile_read_in(
                query,
                &[],
                CompileLimits::default(),
                memory,
                context,
                |read, _| {
                    let plan = read.plan().description();
                    let paths: Vec<_> = plan
                        .operators
                        .iter()
                        .filter_map(|op| {
                            if let OperatorKind::BoundedExpand {
                                edge_predicate,
                                completed_edge_predicate,
                                ..
                            } = op.kind
                            {
                                Some((edge_predicate, completed_edge_predicate))
                            } else {
                                None
                            }
                        })
                        .collect();
                    assert_eq!(paths.len(), stages.len());
                    for ((prefix, completed), stage) in paths.into_iter().zip(stages) {
                        assert_eq!(prefix.is_some(), *stage == 1, "{query}");
                        assert_eq!(completed.is_some(), *stage == 2, "{query}");
                        if let Some(predicate) = completed {
                            assert!(
                                matches!(
                                    plan.expressions[predicate.expression.0 as usize],
                                    Expression::Binary {
                                        operation: BinaryExpression::And,
                                        ..
                                    }
                                ),
                                "whole bag must retain independent y constraint"
                            );
                        }
                    }
                    Ok(())
                },
            )
            .unwrap();
        }
    });
}
