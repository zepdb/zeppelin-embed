//! Independent literal plan oracles for ZE-138 search CALL lowering.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod support;

use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use zeppelin_embed::property_graph::{
    GraphGeneration, StoreInstanceId,
    query::{QueryView, ValueContext, plan::*, resources::*},
    resources::GraphResources,
};
use zeppelin_embed_cypher::{CompileLimits, compile_read_in};

fn with_memory(run: impl FnOnce(&QueryMemory<'_>, &mut ValueContext<'_>)) {
    let path = support::unique_temp_dir("ze138-search-lowering");
    std::fs::create_dir(&path).unwrap();
    let store = Store::open(
        &path,
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let memory = QueryMemory::new(&shared, 24 * 1024 * 1024).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    let baseline = memory.reserved_bytes();
    run(&memory, &mut context);
    assert_eq!(memory.reserved_bytes(), baseline);
    drop(memory);
    drop(shared);
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn text_call_lowers_to_one_typed_eager_source() {
    with_memory(|memory, context| {
        compile_read_in(
            "CALL ze.text_search('needle', 3) YIELD node AS hit, score AS rank RETURN hit, rank",
            &[],
            CompileLimits::default(),
            memory,
            context,
            |read, _| {
                let plan = read.plan().description();
                assert_eq!(plan.operators.len(), 3);
                assert_eq!(plan.eager_searches, &[PlanNodeId(1)]);
                assert!(matches!(plan.operators[0].kind, OperatorKind::Unit));
                let OperatorKind::Search {
                    call,
                    request:
                        SearchRequest::Text {
                            query,
                            k,
                            eligible,
                            options: _,
                        },
                    outputs,
                } = plan.operators[1].kind
                else {
                    panic!("expected typed text search")
                };
                assert_eq!(call, SearchCallId(0));
                assert_eq!(eligible, None);
                assert!(matches!(
                    plan.expressions[query.0 as usize],
                    Expression::Literal(Literal::String("needle"))
                ));
                assert!(matches!(
                    plan.expressions[k.0 as usize],
                    Expression::Literal(Literal::I64(3))
                ));
                assert_eq!(
                    outputs,
                    SearchOutputs {
                        node: Some(SlotId(0)),
                        distance: None,
                        score: Some(SlotId(1)),
                        vector_distance: None,
                        lexical_score: None,
                    }
                );
                let OperatorKind::Project(columns) = plan.operators[2].kind else {
                    panic!("expected final projection")
                };
                assert_eq!(columns.len(), 2);
                assert_eq!(read.columns()[0].name, "hit");
                assert_eq!(read.columns()[1].name, "rank");
                Ok(())
            },
        )
        .unwrap();
    });
}

fn operator_depends_on(
    description: PlanDescription<'_>,
    root: PlanNodeId,
    wanted: PlanNodeId,
) -> bool {
    if root == wanted {
        return true;
    }
    description
        .operators
        .get(root.0 as usize)
        .is_some_and(|operator| {
            operator
                .inputs
                .iter()
                .any(|input| operator_depends_on(description, *input, wanted))
        })
}

#[test]
fn direct_vector_search_yield_feeds_following_match_in_one_validated_plan() {
    with_memory(|memory, context| {
        compile_read_in(
            "CALL ze.vector_search([1,2],20,'auto') YIELD node AS chunk,distance MATCH (chunk)-[:FROM_MEETING]->(meeting) RETURN chunk,meeting,distance ORDER BY distance,ze.node_id(chunk)",
            &[],
            CompileLimits::default(),
            memory,
            context,
            |read, _| {
                let description = read.plan().description();
                assert_eq!(description.eager_searches, &[PlanNodeId(1)]);
                let search_id = description.eager_searches[0];
                let OperatorKind::Search {
                    request: SearchRequest::Vector { mode, .. },
                    outputs,
                    ..
                } = description.operators[search_id.0 as usize].kind
                else {
                    panic!("expected vector search")
                };
                assert_eq!(mode, SearchMode::Auto);
                assert_eq!(outputs.node, Some(SlotId(0)));
                assert_eq!(outputs.distance, Some(SlotId(1)));
                let chunk = outputs.node.unwrap();
                let expand = description
                    .operators
                    .iter()
                    .enumerate()
                    .find_map(|(index, operator)| match operator.kind {
                        OperatorKind::Expand { source, node, .. } if source == chunk => {
                            Some((PlanNodeId(index as u32), node))
                        }
                        _ => None,
                    })
                    .unwrap();
                assert!(operator_depends_on(description, expand.0, search_id));
                assert!(operator_depends_on(description, description.root, expand.0));
                assert_eq!(
                    read.columns()
                        .iter()
                        .map(|column| column.name)
                        .collect::<Vec<_>>(),
                    ["chunk", "meeting", "distance"]
                );
                let root = read.plan().facts(description.root).unwrap();
                assert!(root.ordered());
                assert!(read
                    .columns()
                    .iter()
                    .any(|column| column.name == "meeting" && column.kinds == ValueKinds::NODE));
                assert!(root.slot(expand.1).is_none());
                Ok(())
            },
        )
        .unwrap();
    });
}

fn expression_contains_slot(description: PlanDescription<'_>, root: ExprId) -> bool {
    let Some(expression) = description.expressions.get(root.0 as usize) else {
        return true;
    };
    match expression {
        Expression::Slot(_) => true,
        Expression::Unary { operand, .. }
        | Expression::Property {
            entity: operand, ..
        }
        | Expression::HasLabel {
            entity: operand, ..
        } => expression_contains_slot(description, *operand),
        Expression::Binary { left, right, .. } => {
            expression_contains_slot(description, *left)
                || expression_contains_slot(description, *right)
        }
        Expression::List(items) => items
            .iter()
            .any(|item| expression_contains_slot(description, *item)),
        Expression::Aggregate { .. } => true,
        Expression::Literal(_) | Expression::Parameter(_) => false,
    }
}

#[test]
fn independent_search_arguments_substitute_complete_invariant_alias_dags() {
    with_memory(|memory, context| {
        for (query, expected_kind) in [
            (
                "WITH 1+1 AS k CALL ze.text_search('x', k) YIELD node RETURN node",
                "binary-k",
            ),
            (
                "WITH 1 AS x CALL ze.vector_search([x,2], 1, 'exact') YIELD node RETURN node",
                "list-vector",
            ),
            (
                "WITH size(([1,2])) AS k CALL ze.text_search('x', k) YIELD node RETURN node",
                "scalar-k",
            ),
            (
                "WITH 1 AS one WITH one AS first WITH first AS k LIMIT 0 CALL ze.text_search('x', k) YIELD node RETURN node",
                "empty-prior",
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
                    assert_eq!(plan.eager_searches.len(), 1, "{expected_kind}");
                    let search_id = plan.eager_searches[0];
                    let search = plan.operators[search_id.0 as usize];
                    assert_eq!(search.inputs.len(), 1, "{expected_kind}");
                    assert!(matches!(
                        plan.operators[search.inputs[0].0 as usize].kind,
                        OperatorKind::Unit
                    ));
                    let OperatorKind::Search { request, .. } = search.kind else {
                        panic!("{expected_kind}: expected search")
                    };
                    let arguments: &[ExprId] = match request {
                        SearchRequest::Text { query, k, .. } => &[query, k],
                        SearchRequest::Vector { vector, k, .. } => &[vector, k],
                        SearchRequest::Hybrid { .. } => panic!("unexpected hybrid"),
                    };
                    for argument in arguments {
                        assert!(
                            !expression_contains_slot(plan, *argument),
                            "{expected_kind}: independent request reached a row slot"
                        );
                    }
                    assert!(plan.operators.iter().any(|operator| {
                        matches!(operator.kind, OperatorKind::Join { predicate: None })
                            && operator.inputs.contains(&search_id)
                    }));
                    Ok(())
                },
            )
            .unwrap_or_else(|error| panic!("{expected_kind}: {error:?}"));
        }
    });
}

#[test]
fn omitted_empty_and_global_eligibility_remain_distinct_plans() {
    with_memory(|memory, context| {
        for (query, expected) in [
            (
                "CALL ze.text_search('x',1) YIELD node RETURN node",
                "omitted",
            ),
            (
                "CALL ze.text_search('x',1,[]) YIELD node RETURN node",
                "empty",
            ),
            (
                "MATCH (n) WITH collect(DISTINCT n) AS eligible CALL ze.text_search('x',1,eligible) YIELD node RETURN node",
                "global",
            ),
            (
                "MATCH (n) WITH collect(DISTINCT n) AS eligible WITH eligible AS domain CALL ze.text_search('x',1,domain) YIELD node RETURN node",
                "global-alias",
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
                    let search_id = plan.eager_searches[0];
                    let search = plan.operators[search_id.0 as usize];
                    let OperatorKind::Search {
                        request: SearchRequest::Text { eligible, .. },
                        ..
                    } = search.kind
                    else {
                        panic!("expected text search")
                    };
                    match expected {
                        "omitted" => assert_eq!(eligible, None),
                        "empty" => {
                            let eligible = eligible.unwrap();
                            assert!(matches!(
                                plan.expressions[eligible.0 as usize],
                                Expression::List(items) if items.is_empty()
                            ));
                            assert!(matches!(
                                plan.operators[search.inputs[0].0 as usize].kind,
                                OperatorKind::Unit
                            ));
                        }
                        "global" | "global-alias" => {
                            let eligible = eligible.unwrap();
                            let Expression::Slot(slot) = plan.expressions[eligible.0 as usize]
                            else {
                                panic!("global eligibility must retain its current slot")
                            };
                            let input = search.inputs[0];
                            assert!(!matches!(
                                plan.operators[input.0 as usize].kind,
                                OperatorKind::Unit
                            ));
                            let input_facts = read.plan().facts(input).unwrap();
                            assert!(input_facts.singleton());
                            assert_eq!(input_facts.slot(slot), Some(ValueKinds::LIST));
                            if expected == "global-alias" {
                                assert!(matches!(
                                    plan.operators[input.0 as usize].kind,
                                    OperatorKind::With(_)
                                ));
                            }
                        }
                        _ => panic!("unknown eligibility case"),
                    }
                    Ok(())
                },
            )
            .unwrap_or_else(|error| panic!("{expected}: {error:?}"));
        }
    });
}

#[test]
fn every_permitted_yield_subset_keeps_exact_alias_slots_and_types() {
    with_memory(|memory, context| {
        let cases: &[(&str, &[&str])] = &[
            ("ze.vector_search([1,2],1,'exact')", &["node", "distance"]),
            ("ze.text_search('x',1)", &["node", "score"]),
            (
                "ze.hybrid_search([1,2],'x',1,'exact')",
                &["node", "score", "vector_distance", "lexical_score"],
            ),
        ];
        let mut checked = 0;
        for (procedure, fields) in cases {
            for mask in 1_u32..(1_u32 << fields.len()) {
                let selected = fields
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| mask & (1 << index) != 0)
                    .map(|(_, field)| *field)
                    .collect::<Vec<_>>();
                let yields = selected
                    .iter()
                    .enumerate()
                    .map(|(index, field)| format!("{field} AS a{index}"))
                    .collect::<Vec<_>>()
                    .join(",");
                let returns = (0..selected.len())
                    .map(|index| format!("a{index}"))
                    .collect::<Vec<_>>()
                    .join(",");
                let query = format!("CALL {procedure} YIELD {yields} RETURN {returns}");
                compile_read_in(
                    &query,
                    &[],
                    CompileLimits::default(),
                    memory,
                    context,
                    |read, _| {
                        let search_id = read.plan().description().eager_searches[0];
                        let OperatorKind::Search { outputs, .. } =
                            read.plan().description().operators[search_id.0 as usize].kind
                        else {
                            panic!("expected search")
                        };
                        let mut expected = SearchOutputs::default();
                        for (slot, field) in selected.iter().enumerate() {
                            let slot = Some(SlotId(slot as u32));
                            match *field {
                                "node" => expected.node = slot,
                                "distance" => expected.distance = slot,
                                "score" => expected.score = slot,
                                "vector_distance" => expected.vector_distance = slot,
                                "lexical_score" => expected.lexical_score = slot,
                                _ => panic!("unexpected expected field"),
                            }
                        }
                        assert_eq!(outputs, expected, "{query}");
                        let facts = read.plan().facts(search_id).unwrap();
                        for slot in [outputs.vector_distance, outputs.lexical_score]
                            .into_iter()
                            .flatten()
                        {
                            assert_eq!(
                                facts.slot(slot),
                                Some(ValueKinds::F64.union(ValueKinds::NULL)),
                                "{query}"
                            );
                        }
                        Ok(())
                    },
                )
                .unwrap_or_else(|error| panic!("{query}: {error:?}"));
                checked += 1;
            }
        }
        assert_eq!(checked, 21);
    });
}

#[test]
fn all_four_request_modes_survive_binding_and_lowering() {
    with_memory(|memory, context| {
        for (spelling, expected) in [
            ("default", SearchMode::Default),
            ("auto", SearchMode::Auto),
            ("exact", SearchMode::Exact),
            ("scan", SearchMode::Scan),
        ] {
            let query =
                format!("CALL ze.vector_search([1,2],1,'{spelling}') YIELD distance AS d RETURN d");
            compile_read_in(
                &query,
                &[],
                CompileLimits::default(),
                memory,
                context,
                |read, _| {
                    let search = read.plan().description().eager_searches[0];
                    assert!(matches!(
                        read.plan().description().operators[search.0 as usize].kind,
                        OperatorKind::Search {
                            request: SearchRequest::Vector { mode, .. },
                            ..
                        } if mode == expected
                    ));
                    Ok(())
                },
            )
            .unwrap();
        }
    });
}

#[test]
fn independent_sources_after_match_and_prior_search_keep_cartesian_joins_and_eager_order() {
    with_memory(|memory, context| {
        let query = "MATCH (n) CALL ze.text_search('a',1) YIELD node AS a, score AS discarded CALL ze.hybrid_search([1,2],'b',2,'auto') YIELD node AS b, score AS also_discarded, vector_distance AS component RETURN n,a,b LIMIT 0";
        compile_read_in(
            query,
            &[],
            CompileLimits::default(),
            memory,
            context,
            |read, _| {
                let plan = read.plan().description();
                assert_eq!(plan.eager_searches.len(), 2);
                for (index, search_id) in plan.eager_searches.iter().enumerate() {
                    let search = plan.operators[search_id.0 as usize];
                    assert!(matches!(
                        plan.operators[search.inputs[0].0 as usize].kind,
                        OperatorKind::Unit
                    ));
                    assert!(matches!(
                        search.kind,
                        OperatorKind::Search { call, .. } if call == SearchCallId(index as u32)
                    ));
                    assert!(plan.operators.iter().any(|operator| {
                        matches!(operator.kind, OperatorKind::Join { predicate: None })
                            && operator.inputs.contains(search_id)
                    }));
                }
                let projected = read
                    .columns()
                    .iter()
                    .map(|column| column.name)
                    .collect::<Vec<_>>();
                assert_eq!(projected, ["n", "a", "b"]);
                assert!(matches!(
                    plan.operators[plan.root.0 as usize].kind,
                    OperatorKind::OffsetLimit { limit: Some(0), .. }
                ));
                Ok(())
            },
        )
        .unwrap();
    });
}

#[test]
fn parameter_and_mode_aliases_lower_to_canonical_backing() {
    use zeppelin_embed::property_graph::query::QueryValue;
    let parameters = [
        ParameterBinding {
            name: "query",
            value: QueryValue::String("term"),
        },
        ParameterBinding {
            name: "count",
            value: QueryValue::I64(2),
        },
    ];
    with_memory(|memory, context| {
        compile_read_in(
            "WITH $query AS q, $count AS k, 'scan' AS mode CALL ze.text_search(q,k) YIELD node RETURN node",
            &parameters,
            CompileLimits::default(),
            memory,
            context,
            |read, _| {
                let plan = read.plan().description();
                let search = plan.eager_searches[0];
                let OperatorKind::Search {
                    request: SearchRequest::Text { query, k, .. },
                    ..
                } = plan.operators[search.0 as usize].kind
                else {
                    panic!("expected text search")
                };
                assert!(matches!(
                    plan.expressions[query.0 as usize],
                    Expression::Parameter(ParameterId(0))
                ));
                assert!(matches!(
                    plan.expressions[k.0 as usize],
                    Expression::Parameter(ParameterId(1))
                ));
                Ok(())
            },
        )
        .unwrap();
        compile_read_in(
            "WITH 'scan' AS mode CALL ze.vector_search([1,2],1,mode) YIELD node RETURN node",
            &[],
            CompileLimits::default(),
            memory,
            context,
            |read, _| {
                let search = read.plan().description().eager_searches[0];
                assert!(matches!(
                    read.plan().description().operators[search.0 as usize].kind,
                    OperatorKind::Search {
                        request: SearchRequest::Vector {
                            mode: SearchMode::Scan,
                            ..
                        },
                        ..
                    }
                ));
                Ok(())
            },
        )
        .unwrap();
    });
}
