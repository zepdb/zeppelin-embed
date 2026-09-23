#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use zeppelin_embed::property_graph::query::plan::{Expression, Literal, ValueKinds};
use zeppelin_embed_cypher::{Budget, CompileLimits, compile_with};

#[test]
fn scalar_return_binds_exact_column_and_core_literal() {
    let mut calls = 0;
    compile_with(
        "RETURN 42 AS answer",
        &[],
        CompileLimits::default(),
        &mut Budget::default(),
        |bound| {
            calls += 1;
            assert_eq!(bound.columns().len(), 1);
            let column = bound.columns()[0];
            assert_eq!(column.name, "answer");
            assert_eq!(column.kinds, ValueKinds::I64);
            assert!(matches!(
                bound.expressions()[column.expression.0 as usize],
                Expression::Literal(Literal::I64(42))
            ));
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(calls, 1);
}

#[test]
fn create_relationship_is_in_scope_for_right_endpoint_properties() {
    let mut calls = 0;
    compile_with(
        "CREATE (a)-[r:R]->(b {x:type(r)}) RETURN b.x",
        &[],
        CompileLimits::default(),
        &mut Budget::default(),
        |bound| {
            calls += 1;
            assert_eq!(bound.columns().len(), 1);
            assert_eq!(bound.columns()[0].name, "b.x");
            assert!(bound.columns()[0].kinds.contains(ValueKinds::STRING));
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(calls, 1);
}

#[test]
fn scalar_scope_parameters_and_operator_types_bind_before_consumption() {
    use zeppelin_embed::property_graph::query::{
        QueryValue,
        plan::{BinaryExpression, ParameterBinding},
    };
    let parameters = [ParameterBinding {
        name: "input",
        value: QueryValue::I64(7),
    }];
    compile_with(
        "WITH $input AS x RETURN x + 1 AS answer",
        &parameters,
        CompileLimits::default(),
        &mut Budget::default(),
        |bound| {
            assert_eq!(bound.columns()[0].name, "answer");
            assert_eq!(bound.columns()[0].kinds, ValueKinds::I64);
            assert!(matches!(
                bound.expressions()[bound.columns()[0].expression.0 as usize],
                Expression::Binary {
                    operation: BinaryExpression::Arithmetic(_),
                    ..
                }
            ));
            Ok(())
        },
    )
    .unwrap();
    for (text, expected) in [
        (
            "WITH 1 AS x RETURN y",
            zeppelin_embed_cypher::ErrorKind::UnknownVariable,
        ),
        (
            "RETURN 1 AS x, 2 AS x",
            zeppelin_embed_cypher::ErrorKind::DuplicateVariable,
        ),
        ("RETURN true + 1", zeppelin_embed_cypher::ErrorKind::Type),
        (
            "RETURN $missing",
            zeppelin_embed_cypher::ErrorKind::Parameter,
        ),
    ] {
        let mut consumed = false;
        let result = compile_with(
            text,
            &[],
            CompileLimits::default(),
            &mut Budget::default(),
            |_| {
                consumed = true;
                Ok(())
            },
        );
        assert_eq!(result.unwrap_err().kind, expected, "{text}");
        assert!(!consumed);
    }
}

#[test]
fn patterns_preserve_symbolic_names_optional_types_and_entity_functions() {
    use zeppelin_embed::property_graph::query::plan::UnaryExpression;
    let query = "MATCH (n:Unknown) OPTIONAL MATCH (n)-[r:R|S]->(m) RETURN ze.node_id(n) AS id, ze.relationship_id(r) AS rel, ze.stored_text(m) AS source, n.text AS ordinary, labels(n) AS labels, type(r) AS kind, size([n]) AS length, n:A:B AS labelled";
    compile_with(
        query,
        &[],
        CompileLimits::default(),
        &mut Budget::default(),
        |bound| {
            assert_eq!(bound.columns().len(), 8);
            assert_eq!(bound.columns()[0].kinds, ValueKinds::STRING);
            assert_eq!(
                bound.columns()[1].kinds,
                ValueKinds::STRING.union(ValueKinds::NULL)
            );
            assert_eq!(
                bound.columns()[2].kinds,
                ValueKinds::STRING.union(ValueKinds::NULL)
            );
            assert!(matches!(
                bound.expressions()[bound.columns()[0].expression.0 as usize],
                Expression::Unary {
                    operation: UnaryExpression::NodeIdText,
                    ..
                }
            ));
            assert!(matches!(
                bound.expressions()[bound.columns()[1].expression.0 as usize],
                Expression::Unary {
                    operation: UnaryExpression::RelIdText,
                    ..
                }
            ));
            assert!(matches!(
                bound.expressions()[bound.columns()[2].expression.0 as usize],
                Expression::Unary {
                    operation: UnaryExpression::StoredText,
                    ..
                }
            ));
            assert!(matches!(
                bound.expressions()[bound.columns()[3].expression.0 as usize],
                Expression::Property { .. }
            ));
            Ok(())
        },
    )
    .unwrap();
    for (query, expected) in [
        (
            "MATCH (a)-[r]->()-[r]->(a) RETURN r",
            zeppelin_embed_cypher::ErrorKind::RelationshipUniqueness,
        ),
        (
            "WITH 1 AS n MATCH (n) RETURN n",
            zeppelin_embed_cypher::ErrorKind::Type,
        ),
        (
            "MATCH (n) RETURN ze.relationship_id(n)",
            zeppelin_embed_cypher::ErrorKind::Type,
        ),
        (
            "MATCH ()-[rs*1..1]->() MATCH ()-[rs*1..1]->() RETURN rs",
            zeppelin_embed_cypher::ErrorKind::Type,
        ),
    ] {
        let result = compile_with(
            query,
            &[],
            CompileLimits::default(),
            &mut Budget::default(),
            |_| Ok(()),
        );
        assert_eq!(result.unwrap_err().kind, expected, "{query}");
    }
    compile_with(
        "MATCH ()-[r]->() MATCH ()-[r]->() RETURN r",
        &[],
        CompileLimits::default(),
        &mut Budget::default(),
        |_| Ok(()),
    )
    .unwrap();
}

#[test]
fn projections_expand_scope_and_preserve_grouped_order_references() {
    let positives = [
        "MATCH (a), (b) WITH b AS renamed, a RETURN *",
        "MATCH (n) WITH n.name AS name ORDER BY n.age WITH name WHERE name IS NOT NULL RETURN name",
        "MATCH (n) RETURN DISTINCT n.name AS name ORDER BY n.name",
        "MATCH (n) RETURN n.name AS name, count(*) AS total ORDER BY n.name, total DESC",
        "MATCH (n) WITH (n) RETURN n",
    ];
    for query in positives {
        compile_with(
            query,
            &[],
            CompileLimits::default(),
            &mut Budget::default(),
            |bound| {
                if query.ends_with("RETURN *") {
                    assert_eq!(
                        bound.columns().iter().map(|c| c.name).collect::<Vec<_>>(),
                        ["renamed", "a"]
                    );
                }
                Ok(())
            },
        )
        .unwrap_or_else(|e| panic!("{query}: {e:?}"));
    }
    for (query, kind) in [
        (
            "MATCH (n) WITH n.name AS name RETURN n",
            zeppelin_embed_cypher::ErrorKind::UnknownVariable,
        ),
        (
            "MATCH (n) RETURN DISTINCT n.name AS name ORDER BY n.age",
            zeppelin_embed_cypher::ErrorKind::UnknownVariable,
        ),
        (
            "MATCH (n) RETURN count(*) AS total ORDER BY n",
            zeppelin_embed_cypher::ErrorKind::UnknownVariable,
        ),
        (
            "MATCH (n) RETURN *, 1 AS n",
            zeppelin_embed_cypher::ErrorKind::DuplicateVariable,
        ),
        (
            "WITH 1 AS n RETURN n LIMIT $limit",
            zeppelin_embed_cypher::ErrorKind::Parameter,
        ),
    ] {
        let error = compile_with(
            query,
            &[],
            CompileLimits::default(),
            &mut Budget::default(),
            |_| Ok(()),
        )
        .unwrap_err();
        assert_eq!(error.kind, kind, "{query}");
    }
}

#[test]
fn mutations_bind_property_types_and_deleted_result_boundaries() {
    for query in [
        "CREATE (a:A:B {p: [], n: null}), (b), (a)-[r:R {p: [1,2]}]->(b) SET a.x=1, r.y=2, a:New REMOVE a.x, r.y, a:Old RETURN 1 AS result",
        "MATCH (n) WITH n, n.p AS old DELETE n RETURN old",
        "OPTIONAL MATCH (n) DELETE n RETURN n",
        "MATCH (n) DETACH DELETE n RETURN count(*) AS deleted LIMIT 0",
    ] {
        compile_with(
            query,
            &[],
            CompileLimits::default(),
            &mut Budget::default(),
            |_| Ok(()),
        )
        .unwrap_or_else(|e| panic!("{query}: {e:?}"));
    }
    for (query, kind) in [
        (
            "MATCH (n) CREATE (n:New)",
            zeppelin_embed_cypher::ErrorKind::DuplicateVariable,
        ),
        (
            "MATCH (n) SET n.x=[1, 2.0]",
            zeppelin_embed_cypher::ErrorKind::Type,
        ),
        (
            "MATCH (n) SET n.x=[null]",
            zeppelin_embed_cypher::ErrorKind::Type,
        ),
        (
            "MATCH (n) SET n.x=[[1]]",
            zeppelin_embed_cypher::ErrorKind::Type,
        ),
        (
            "MATCH ()-[r]->() SET r:Label",
            zeppelin_embed_cypher::ErrorKind::Type,
        ),
        (
            "MATCH (n) DELETE n RETURN n",
            zeppelin_embed_cypher::ErrorKind::DeletedEntity,
        ),
        (
            "MATCH (n) DELETE n RETURN n.p",
            zeppelin_embed_cypher::ErrorKind::DeletedEntity,
        ),
        (
            "MATCH (n) WITH n, n AS alias DELETE n RETURN alias",
            zeppelin_embed_cypher::ErrorKind::DeletedEntity,
        ),
        (
            "MATCH ()-[r]->() DELETE r RETURN collect(r)",
            zeppelin_embed_cypher::ErrorKind::DeletedEntity,
        ),
    ] {
        let mut calls = 0;
        let error = compile_with(
            query,
            &[],
            CompileLimits::default(),
            &mut Budget::default(),
            |_| {
                calls += 1;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.kind, kind, "{query}");
        assert_eq!(calls, 0);
    }
}

#[test]
fn parameters_reject_nested_entities_and_preserve_all_scalar_float_bits() {
    use zeppelin_embed::lifecycle::{CancelToken, QueryControl};
    use zeppelin_embed::property_graph::{
        GraphGeneration, NodeId, StoreInstanceId,
        query::{QueryList, QueryValue, QueryView, ValueContext, plan::ParameterBinding},
    };
    let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
    let control = QueryControl::Cancel(CancelToken::new());
    let mut context = ValueContext::new(&view, &control, 8_000_000).unwrap();
    let entity = [view.node(NodeId::new(1).unwrap())];
    let list = [QueryValue::List(
        QueryList::new(&entity, &mut context).unwrap(),
    )];
    let nested = QueryValue::List(QueryList::new(&list, &mut context).unwrap());
    let parameters = [ParameterBinding {
        name: "p",
        value: nested,
    }];
    let error = compile_with(
        "RETURN $p",
        &parameters,
        CompileLimits::default(),
        &mut Budget::default(),
        |_| Ok(()),
    )
    .unwrap_err();
    assert_eq!(error.kind, zeppelin_embed_cypher::ErrorKind::Parameter);
    for bits in [
        0x7ff8_0000_0000_0123,
        0xfff8_0000_0000_0456,
        f64::INFINITY.to_bits(),
        f64::NEG_INFINITY.to_bits(),
        (-0.0_f64).to_bits(),
    ] {
        let parameters = [ParameterBinding {
            name: "p",
            value: QueryValue::F64(f64::from_bits(bits)),
        }];
        compile_with(
            "RETURN $p + 0.0 AS value",
            &parameters,
            CompileLimits::default(),
            &mut Budget::default(),
            |bound| {
                assert_eq!(bound.columns()[0].kinds, ValueKinds::F64);
                Ok(())
            },
        )
        .unwrap();
        let QueryValue::F64(value) = parameters[0].value else {
            panic!("scalar changed")
        };
        assert_eq!(value.to_bits(), bits);
    }
}

#[test]
fn search_binding_preserves_each_call_and_singleton_eligibility_provenance() {
    for query in [
        "CALL ze.vector_search([1.0], 2, 'exact') YIELD node AS n, distance RETURN n, distance",
        "MATCH (n) WITH collect(DISTINCT n) AS eligible WITH eligible AS domain CALL ze.text_search('hello', 3, domain) YIELD node, score RETURN node, score",
        "CALL ze.hybrid_search([1.0], 'hello', 3, 'default', []) YIELD node, score, vector_distance, lexical_score RETURN *",
        "MATCH (n) CALL ze.text_search('a', 1) YIELD node AS a, score AS first CALL ze.text_search('b', 2) YIELD node AS b, score AS second RETURN a, b",
    ] {
        compile_with(
            query,
            &[],
            CompileLimits::default(),
            &mut Budget::default(),
            |_| Ok(()),
        )
        .unwrap_or_else(|e| panic!("{query}: {e:?}"));
    }
    for query in [
        "CALL ze.text_search('x', 1) YIELD distance RETURN distance",
        "CALL ze.vector_search([1.0], 1, 'approximate') YIELD node RETURN node",
        "MATCH (n) CALL ze.text_search(n.p, 1) YIELD node RETURN node",
        "MATCH (n) WITH n.p AS group, collect(DISTINCT n) AS eligible CALL ze.text_search('x', 1, eligible) YIELD node RETURN node",
        "WITH [] AS eligible CALL ze.text_search('x', 1, eligible) YIELD node RETURN node",
        "MATCH (n) WITH collect(n) AS eligible CALL ze.text_search('x', 1, eligible) YIELD node RETURN node",
    ] {
        let error = compile_with(
            query,
            &[],
            CompileLimits::default(),
            &mut Budget::default(),
            |_| Ok(()),
        )
        .unwrap_err();
        assert_eq!(
            error.kind,
            zeppelin_embed_cypher::ErrorKind::SearchContext,
            "{query}"
        );
    }
}

#[test]
fn selected_original_statements_bind_without_claiming_execution() {
    use zeppelin_embed::lifecycle::{CancelToken, QueryControl};
    use zeppelin_embed::property_graph::{
        GraphGeneration, StoreInstanceId,
        query::{QueryList, QueryValue, QueryView, ValueContext, plan::ParameterBinding},
    };
    let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
    let control = QueryControl::Cancel(CancelToken::new());
    let values = [QueryValue::String("Apa")];
    let list = QueryValue::List(
        QueryList::new(
            &values,
            &mut ValueContext::new(&view, &control, 8_000_000).unwrap(),
        )
        .unwrap(),
    );
    let mut accepted = 0;
    let mut rejected = 0;
    for entry in include_str!("fixtures/selected-tck-syntax.txt")
        .split("\n---\n")
        .skip(1)
    {
        let (label, query) = entry.split_once('\n').unwrap();
        let mut parameters = Vec::new();
        if label == "parse expressions/list/List1.feature [3] query" {
            parameters.push(ParameterBinding {
                name: "expr",
                value: list,
            });
        }
        if label == "parse expressions/list/List1.feature [3] query"
            || label == "parse expressions/list/List1.feature [4] query"
        {
            parameters.push(ParameterBinding {
                name: "idx",
                value: QueryValue::I64(0),
            });
        }
        let result = compile_with(
            query,
            &parameters,
            CompileLimits::default(),
            &mut Budget::default(),
            |_| Ok(()),
        );
        // The only `reject` rows are Match1[6] and Match2[8], whose original
        // expected error is SyntaxError InvalidParameterUse.
        if label.starts_with("reject ") {
            assert_eq!(
                result.unwrap_err().kind,
                zeppelin_embed_cypher::ErrorKind::InvalidParameterUse,
                "{label}"
            );
            rejected += 1;
        } else if label == "parse clauses/match/Match3.feature [29] query" {
            assert_eq!(
                result.unwrap_err().kind,
                zeppelin_embed_cypher::ErrorKind::RelationshipUniqueness,
                "{label}"
            );
            rejected += 1;
        } else {
            result.unwrap_or_else(|e| panic!("{label}: {e:?}\n{query}"));
            accepted += 1;
        }
    }
    assert_eq!((accepted, rejected), (152, 3));
}

#[test]
fn property_assignment_aliases_keep_exact_list_element_types() {
    for query in [
        "WITH [1, 2.0] AS values MATCH (n) SET n.p = values",
        "WITH [[1]] AS values MATCH (n) SET n.p = values",
        "WITH [null] AS values MATCH (n) SET n.p = values",
    ] {
        let error = compile_with(
            query,
            &[],
            CompileLimits::default(),
            &mut Budget::default(),
            |_| Ok(()),
        )
        .unwrap_err();
        assert_eq!(
            error.kind,
            zeppelin_embed_cypher::ErrorKind::Type,
            "{query}"
        );
    }
    compile_with(
        "WITH [] AS values MATCH (n) SET n.p = values",
        &[],
        CompileLimits::default(),
        &mut Budget::default(),
        |_| Ok(()),
    )
    .unwrap();
}

#[test]
fn deleted_counts_and_dynamic_identity_checks_remain_distinct_from_entity_results() {
    for query in [
        "MATCH (n) DELETE n RETURN count(n)",
        "MATCH (n) DELETE n RETURN ze.node_id(n)",
        "MATCH ()-[r]->() DELETE r RETURN ze.relationship_id(r)",
        "MATCH (n) WITH n, [n] AS refs DELETE n RETURN size(refs)",
    ] {
        compile_with(
            query,
            &[],
            CompileLimits::default(),
            &mut Budget::default(),
            |_| Ok(()),
        )
        .unwrap_or_else(|e| panic!("{query}: {e:?}"));
    }
    for query in [
        "MATCH (n) WITH n, [n] AS refs DELETE n RETURN refs",
        "MATCH (n) WITH n, [[n]] AS refs DELETE n RETURN refs",
        "MATCH (n) DELETE n RETURN collect(n)",
    ] {
        let error = compile_with(
            query,
            &[],
            CompileLimits::default(),
            &mut Budget::default(),
            |_| Ok(()),
        )
        .unwrap_err();
        assert_eq!(
            error.kind,
            zeppelin_embed_cypher::ErrorKind::DeletedEntity,
            "{query}"
        );
    }
}

#[test]
fn exact_syntax_node_budget_still_allows_wildcard_and_label_lowering() {
    for query in ["MATCH (n) RETURN *", "MATCH (n) RETURN n:A:B"] {
        let ast = zeppelin_embed_cypher::parse(query).unwrap();
        let limits = CompileLimits {
            ast_nodes: ast.nodes().len(),
            ..CompileLimits::default()
        };
        compile_with(query, &[], limits, &mut Budget::default(), |bound| {
            assert_eq!(bound.columns().len(), 1);
            Ok(())
        })
        .unwrap();
    }
}

#[test]
fn search_vectors_reject_known_nonnumeric_elements_before_lowering() {
    for vector in ["['x']", "[true]", "[null]", "[[1]]"] {
        let query = format!("CALL ze.vector_search({vector}, 1, 'exact') YIELD node RETURN node");
        let error = compile_with(
            &query,
            &[],
            CompileLimits::default(),
            &mut Budget::default(),
            |_| Ok(()),
        )
        .unwrap_err();
        assert_eq!(error.kind, zeppelin_embed_cypher::ErrorKind::SearchContext);
    }
    for vector in ["[1, 2.0]", "[1 + 2]", "[]"] {
        let query = format!(
            "WITH {vector} AS v CALL ze.vector_search(v, 1, 'exact') YIELD node RETURN node"
        );
        compile_with(
            &query,
            &[],
            CompileLimits::default(),
            &mut Budget::default(),
            |_| Ok(()),
        )
        .unwrap();
    }
}

#[test]
fn compile_errors_keep_exact_utf8_source_spans_and_never_consume() {
    use zeppelin_embed_cypher::ErrorKind;
    for (query, kind, fragment) in [
        (
            "WITH 'λ' AS text CREATE (n) RETURN missing",
            ErrorKind::UnknownVariable,
            "missing",
        ),
        ("RETURN $missing", ErrorKind::Parameter, "$missing"),
        ("RETURN 'x' + 1", ErrorKind::Type, "'x' + 1"),
        (
            "RETURN 1 AS x, 2 AS x",
            ErrorKind::DuplicateVariable,
            "2 AS x",
        ),
    ] {
        let mut consumed = 0;
        let error = compile_with(
            query,
            &[],
            CompileLimits::default(),
            &mut Budget::default(),
            |_| {
                consumed += 1;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.kind, kind, "{query}");
        assert_eq!(query.get(error.span.start..error.span.end), Some(fragment));
        assert_eq!(consumed, 0);
    }
}

#[test]
fn nullable_deleted_results_preserve_required_dynamic_validation() {
    for query in [
        "OPTIONAL MATCH (n) DELETE n RETURN n",
        "OPTIONAL MATCH (n) DELETE n RETURN n.p",
        "OPTIONAL MATCH (n) DELETE n RETURN collect(n)",
    ] {
        compile_with(
            query,
            &[],
            CompileLimits::default(),
            &mut Budget::default(),
            |bound| {
                assert!(bound.requires_deleted_runtime_validation(), "{query}");
                Ok(())
            },
        )
        .unwrap();
    }
    compile_with(
        "OPTIONAL MATCH (n) DELETE n RETURN count(n)",
        &[],
        CompileLimits::default(),
        &mut Budget::default(),
        |bound| {
            assert!(!bound.requires_deleted_runtime_validation());
            Ok(())
        },
    )
    .unwrap();
}

#[test]
fn computed_property_lists_preserve_possible_homogeneous_scalar_values() {
    for query in [
        "CREATE (n {p: [1 = 1]}) RETURN n",
        "MATCH (n) SET n.p = [n.x + 1]",
        "MATCH (n) SET n.p = [+n.x, 1]",
        "MATCH (n) SET n.p = [n.x + 1, 2.0]",
        "MATCH (n) SET n.p = [n.x = 1, true]",
        "MATCH ()-[r]->() SET r.p = [type(r)]",
        "WITH [1 = 1] AS values CREATE (n {p: values}) RETURN n",
    ] {
        let mut consumed = false;
        compile_with(
            query,
            &[],
            CompileLimits::default(),
            &mut Budget::default(),
            |bound| {
                assert_eq!(bound.syntax().source(), query);
                consumed = true;
                Ok(())
            },
        )
        .unwrap_or_else(|e| panic!("supported computed property list {query}: {e:?}"));
        assert!(consumed);
    }
    for query in [
        "CREATE (n {p: [null]})",
        "CREATE (n {p: [[1]]})",
        "MATCH (n) SET n.p = [n]",
        "MATCH ()-[r]->() SET r.p = [r]",
        "CREATE (n {p: [1 = 1, 2]})",
        "MATCH (n) SET n.p = [n.x + 1, 'string']",
        "MATCH (n) SET n.p = [n.x + 1, 2, 3.0]",
    ] {
        let mut consumed = false;
        let error = compile_with(
            query,
            &[],
            CompileLimits::default(),
            &mut Budget::default(),
            |_| {
                consumed = true;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(
            error.kind,
            zeppelin_embed_cypher::ErrorKind::Type,
            "{query}"
        );
        assert!(!consumed);
    }
}

#[test]
fn possibly_aliased_deleted_entities_keep_runtime_validation_after_projection() {
    for query in [
        "MATCH (n) WITH n, [n] AS refs DELETE n RETURN refs[0]",
        "MATCH (n) WITH n, [n][0] AS victim DELETE victim RETURN n",
        "MATCH (n) WITH n, [n][0] AS victim DELETE victim RETURN n.p",
        "MATCH (n), (m) DELETE n RETURN m",
        "MATCH (n), (m) DELETE n RETURN m.p",
        "MATCH (n), (m) DELETE n RETURN labels(m)",
        "MATCH (n), (m) DELETE n RETURN ze.stored_text(m)",
        "MATCH ()-[r]->(), ()-[s]->() DELETE r RETURN s",
        "MATCH ()-[r]->(), ()-[rs:R*1..2]->() DELETE r RETURN rs",
        "MATCH ()-[r]->() OPTIONAL MATCH ()-[rs:R*1..2]->() DELETE r RETURN rs",
        "MATCH (n), (m) WITH n, [m] AS refs DELETE n RETURN refs",
    ] {
        compile_with(
            query,
            &[],
            CompileLimits::default(),
            &mut Budget::default(),
            |bound| {
                assert!(bound.requires_deleted_runtime_validation(), "{query}");
                Ok(())
            },
        )
        .unwrap_or_else(|e| panic!("{query}: {e:?}"));
    }
    for query in [
        "MATCH (n) DELETE n RETURN count(n)",
        "MATCH (n) WITH n, n.p AS copied DELETE n RETURN copied",
        "MATCH (n) WITH n, ze.stored_text(n) AS copied DELETE n RETURN copied",
    ] {
        compile_with(
            query,
            &[],
            CompileLimits::default(),
            &mut Budget::default(),
            |bound| {
                assert!(!bound.requires_deleted_runtime_validation(), "{query}");
                Ok(())
            },
        )
        .unwrap_or_else(|e| panic!("{query}: {e:?}"));
    }
}

#[test]
fn order_expressions_resolve_shadowing_output_aliases_before_group_key_reuse() {
    use zeppelin_embed_cypher::NodeKind;
    for query in [
        "WITH 1 AS x RETURN DISTINCT -x AS x ORDER BY -x",
        "WITH 1 AS x RETURN -x AS x, count(*) AS total ORDER BY -x",
    ] {
        compile_with(
            query,
            &[],
            CompileLimits::default(),
            &mut Budget::default(),
            |bound| {
                let order = bound
                    .syntax()
                    .nodes()
                    .iter()
                    .find(|n| matches!(n.kind, NodeKind::Order { .. }))
                    .unwrap();
                let root = order.children()[0];
                assert_eq!(
                    bound.fact(root).unwrap().slot,
                    None,
                    "ORDER BY -x must evaluate over the projected x in {query}"
                );
                let operand = bound.syntax().node(root).unwrap().children()[0];
                assert_eq!(
                    bound.fact(operand).unwrap().slot,
                    Some(bound.columns()[0].slot)
                );
                Ok(())
            },
        )
        .unwrap();
    }
    for (query, index) in [
        (
            "MATCH (n) RETURN DISTINCT n.name AS name ORDER BY n.name",
            0,
        ),
        (
            "WITH 1 AS x, 2 AS y RETURN DISTINCT x AS y, y AS x ORDER BY x",
            1,
        ),
    ] {
        compile_with(
            query,
            &[],
            CompileLimits::default(),
            &mut Budget::default(),
            |bound| {
                let order = bound
                    .syntax()
                    .nodes()
                    .iter()
                    .find(|n| matches!(n.kind, NodeKind::Order { .. }))
                    .unwrap();
                assert_eq!(
                    bound.fact(order.children()[0]).unwrap().slot,
                    Some(bound.columns()[index].slot),
                    "{query}"
                );
                Ok(())
            },
        )
        .unwrap();
    }
}
