//! Literal compiler-to-plan oracles; these do not execute graph queries.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use zeppelin_embed::property_graph::{
    GraphGeneration, StoreInstanceId,
    query::{QueryView, ValueContext, plan::*, resources::*},
    resources::GraphResources,
};
use zeppelin_embed_cypher::{CompileLimits, compile_read_in};

fn with_memory(run: impl FnOnce(&QueryMemory<'_>, &mut ValueContext<'_>)) {
    let path = std::env::temp_dir().join(format!("ze126-lowering-{}", std::process::id()));
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
fn read_lowering_remaps_nested_lists_arithmetic_and_scalar_functions() {
    with_memory(|memory, context| {
        compile_read_in(
            "RETURN [1, 2+3, ['x',null]][-1] AS v, NOT false XOR true AS b, size('é') AS n",
            &[],
            CompileLimits::default(),
            memory,
            context,
            |read, _context| {
                let plan = read.plan().description();
                let OperatorKind::Project(columns) = plan.operators[plan.root.0 as usize].kind
                else {
                    unreachable!()
                };
                let Expression::Binary {
                    operation: BinaryExpression::Index,
                    left,
                    right,
                } = plan.expressions[columns[0].expression.0 as usize]
                else {
                    unreachable!()
                };
                let Expression::Unary {
                    operation: UnaryExpression::Negate,
                    operand,
                } = plan.expressions[right.0 as usize]
                else {
                    unreachable!()
                };
                assert!(matches!(
                    plan.expressions[operand.0 as usize],
                    Expression::Literal(Literal::I64(1))
                ));
                let Expression::List(items) = plan.expressions[left.0 as usize] else {
                    unreachable!()
                };
                assert_eq!(items.len(), 3);
                assert!(matches!(
                    plan.expressions[items[1].0 as usize],
                    Expression::Binary {
                        operation: BinaryExpression::Arithmetic(
                            zeppelin_embed::property_graph::query::Arithmetic::Add
                        ),
                        ..
                    }
                ));
                assert!(matches!(
                    plan.expressions[columns[1].expression.0 as usize],
                    Expression::Binary {
                        operation: BinaryExpression::Xor,
                        ..
                    }
                ));
                assert!(matches!(
                    plan.expressions[columns[2].expression.0 as usize],
                    Expression::Unary {
                        operation: UnaryExpression::Size,
                        ..
                    }
                ));
                assert_eq!(read.expression_spans().len(), plan.expressions.len());
                for span in read.expression_spans() {
                    assert!(read.source().get(span.start..span.end).is_some());
                }
                assert!(
                    plan.expressions.len() < 25,
                    "syntax holes must not enter the native arena"
                );
                Ok(())
            },
        )
        .unwrap();
    });
}

#[test]
fn read_lowering_copies_scalar_projection_and_exact_source_spans() {
    with_memory(|memory, context| {
        let text = "RETURN 7 AS seven, 'é' AS text";
        compile_read_in(
            text,
            &[],
            CompileLimits::default(),
            memory,
            context,
            |read, _context| {
                let description = read.plan().description();
                assert_eq!(description.operators.len(), 2);
                assert!(matches!(description.operators[0].kind, OperatorKind::Unit));
                let OperatorKind::Project(columns) = description.operators[1].kind else {
                    unreachable!()
                };
                assert_eq!(columns.len(), 2);
                assert_eq!(read.columns()[0].name, "seven");
                assert_eq!(read.columns()[1].name, "text");
                assert!(matches!(
                    description.expressions[columns[0].expression.0 as usize],
                    Expression::Literal(Literal::I64(7))
                ));
                let Expression::Literal(Literal::String(value)) =
                    description.expressions[columns[1].expression.0 as usize]
                else {
                    unreachable!()
                };
                assert_eq!(value, "é");
                assert_ne!(read.source().as_ptr(), text.as_ptr());
                let span = read.expression_spans()[columns[1].expression.0 as usize];
                assert_eq!(&read.source()[span.start..span.end], "'é'");
                let facts = read.plan().facts(description.root).unwrap();
                assert_eq!(facts.slot_at(0), Some((SlotId(0), ValueKinds::I64)));
                assert_eq!(facts.slot_at(1), Some((SlotId(1), ValueKinds::STRING)));
                Ok(())
            },
        )
        .unwrap();
    });
}

#[test]
fn read_lowering_real_owner_inventory_admits_without_a_second_context() {
    with_memory(|memory, context| {
        let expected_control = context.control() as *const QueryControl;
        compile_read_in(
            "RETURN 'owned' AS text",
            &[],
            CompileLimits::default(),
            memory,
            context,
            |read, context| {
                assert_eq!(context.control() as *const QueryControl, expected_control);
                let before = context.work();
                let mut inventory_charge = memory.reserve_external_capacity().unwrap();
                inventory_charge
                    .reserve_additional(
                        std::mem::size_of_val(read.owners())
                            + std::mem::size_of::<Vec<RetainedAllocation<'_>>>(),
                    )
                    .unwrap();
                let mut inventory = Vec::new();
                inventory.try_reserve_exact(read.owners().len()).unwrap();
                inventory.extend_from_slice(read.owners());
                let inputs = QueryInputs::reserve(
                    memory,
                    RetentionInventory::vector(&inventory).unwrap(),
                    context,
                )
                .unwrap();
                inputs
                    .verify_span(read.source().as_bytes(), context)
                    .unwrap();
                inputs.verify_span(read.columns(), context).unwrap();
                let runtime = inputs.admit_plan(read.plan(), context).unwrap();
                assert!(context.work() > before);
                assert_eq!(
                    runtime
                        .plan()
                        .facts(runtime.plan().description().root)
                        .unwrap()
                        .width(),
                    1
                );
                assert!(runtime.backing_bytes() > 0);
                Ok(())
            },
        )
        .unwrap();
    });
}

#[test]
fn read_lowering_complete_optional_path_keeps_or_edge_predicate_and_correlation() {
    with_memory(|memory, context| {
        let query = "MATCH (a:A:B {key:7})-[r:BASE|ALT]->(m) OPTIONAL MATCH (m)-[p:FIRST|SECOND*0..2 {weight:7}]->(b) WHERE b.ok=true RETURN a,r,m,p,b";
        compile_read_in(query, &[], CompileLimits::default(), memory, context, |read, _| {
            let plan = read.plan().description();
            let fixed = plan.operators.iter().find_map(|op| if let OperatorKind::Expand { relationship_types, pattern, .. } = op.kind {Some((relationship_types,pattern))} else {None}).unwrap();
            assert_eq!(fixed.0.iter().map(|n| n.as_str()).collect::<Vec<_>>(), ["BASE", "ALT"]);
            assert_eq!(fixed.1, PatternId(0));
            let (path_index, path) = plan.operators.iter().enumerate().find(|(_,op)| matches!(op.kind, OperatorKind::BoundedExpand { .. })).unwrap();
            let OperatorKind::BoundedExpand { source, node, relationships, edge_predicate, min, max, relationship_types, pattern, .. } = path.kind else { unreachable!() };
            assert_eq!((source,node,relationships,min,max,pattern), (SlotId(2),SlotId(4),SlotId(3),0,2,PatternId(1)));
            assert_eq!(relationship_types.iter().map(|n| n.as_str()).collect::<Vec<_>>(), ["FIRST","SECOND"]);
            let edge = edge_predicate.unwrap();
            assert_eq!(read.plan().facts(PlanNodeId(path_index as u32)).unwrap().slot(edge.current_edge), None);
            let Expression::Binary { operation: BinaryExpression::Comparison(zeppelin_embed::property_graph::query::Comparison::Equal), left, right } = plan.expressions[edge.expression.0 as usize] else { unreachable!() };
            assert!(matches!(plan.expressions[right.0 as usize], Expression::Literal(Literal::I64(7))));
            let Expression::Property { entity, name } = plan.expressions[left.0 as usize] else { unreachable!() };
            assert_eq!(name.as_str(), "weight");
            assert!(matches!(plan.expressions[entity.0 as usize], Expression::Slot(slot) if slot==edge.current_edge));
            let optional = plan.operators.iter().find(|op| matches!(op.kind, OperatorKind::OptionalApply {..})).unwrap();
            assert!(matches!(optional.kind, OperatorKind::OptionalApply { predicate: Some(_) }));
            let right = &plan.operators[optional.inputs[1].0 as usize];
            let mut anchor = right.inputs[0];
            while anchor != optional.inputs[0] { anchor = plan.operators[anchor.0 as usize].inputs[0]; }
            assert_eq!(anchor, optional.inputs[0]);
            let facts = read.plan().facts(plan.root).unwrap();
            assert_eq!(facts.width(),5);
            for (i, kinds) in [ValueKinds::NODE,ValueKinds::REL,ValueKinds::NODE,ValueKinds::LIST.union(ValueKinds::NULL),ValueKinds::NODE.union(ValueKinds::NULL)].into_iter().enumerate() {
                assert_eq!(facts.slot_at(i), Some((SlotId(i as u32+5), kinds)));
            }
            Ok(())
        }).unwrap();
    });
}

#[test]
fn read_lowering_stages_grouping_distinct_hidden_order_and_bounds() {
    with_memory(|memory, context| {
        for query in [
            "MATCH (n) RETURN n.p AS x ORDER BY n.q DESC LIMIT 3",
            "MATCH (n) WITH n.p AS p, count(*) AS c ORDER BY p DESC SKIP 1 LIMIT 2 WHERE c > 0 RETURN p,c",
            "MATCH (n) RETURN DISTINCT n.p AS x ORDER BY x",
            "MATCH (n) RETURN count(n.p) AS c, collect(DISTINCT n.p) AS ps",
        ] {
            compile_read_in(query, &[], CompileLimits::default(), memory, context, |read, _| {
                let plan = read.plan().description();
                let facts = read.plan().facts(plan.root).unwrap();
                if query.contains("n.q") {
                    assert_eq!(facts.width(),1);
                    let sort = plan.operators.iter().find(|op| matches!(op.kind,OperatorKind::Sort(_))).unwrap();
                    assert_eq!(read.plan().facts(sort.inputs[0]).unwrap().width(),2);
                    let OperatorKind::Sort(keys) = sort.kind else { unreachable!() };
                    assert!(keys[0].descending);
                    assert!(matches!(plan.expressions[keys[0].expression.0 as usize],Expression::Property {name,..} if name.as_str()=="q"));
                    assert!(plan.operators.iter().any(|op| matches!(op.kind,OperatorKind::OffsetLimit {offset:0,limit:Some(3)})));
                } else if query.contains("WHERE") {
                    assert!(facts.barriers().scope());
                    assert!(facts.barriers().contains(Barrier::Aggregate));
                    let aggregate = plan.operators.iter().find(|op| matches!(op.kind,OperatorKind::Aggregate {..})).unwrap();
                    let OperatorKind::Aggregate {keys,aggregates} = aggregate.kind else {unreachable!()};
                    assert_eq!((keys.len(),aggregates.len()),(1,1));
                    assert!(plan.operators.iter().any(|op| matches!(op.kind,OperatorKind::OffsetLimit {offset:1,limit:Some(2)})));
                } else if query.contains("RETURN DISTINCT") {
                    assert!(facts.barriers().contains(Barrier::Distinct));
                    assert!(facts.ordered());
                } else {
                    assert!(facts.singleton());
                    let OperatorKind::Aggregate {keys,aggregates} = plan.operators.iter().find(|op| matches!(op.kind,OperatorKind::Aggregate {..})).unwrap().kind else {unreachable!()};
                    assert!(keys.is_empty());
                    assert_eq!(aggregates.len(),2);
                    assert!(matches!(plan.expressions[aggregates[1].expression.0 as usize],Expression::Aggregate {operation:AggregateExpression::Collect {distinct:true},operand:Some(_)}));
                }
                Ok(())
            }).unwrap();
        }
    });
}

#[test]
fn read_lowering_copies_parameter_names_strings_nested_lists_and_exact_bits() {
    use zeppelin_embed::property_graph::query::{QueryList, QueryValue};
    with_memory(|memory, context| {
        let text = String::from("hé");
        let nested = [
            QueryValue::I64(i64::MAX),
            QueryValue::F64(f64::from_bits(0x7ff8_0000_0000_0042)),
        ];
        let list = QueryList::new(&nested, context).unwrap();
        let payload = [QueryValue::String(&text), QueryValue::List(list)];
        let bindings = [
            ParameterBinding {
                name: "payload",
                value: QueryValue::List(QueryList::new(&payload, context).unwrap()),
            },
            ParameterBinding {
                name: "cap",
                value: QueryValue::I64(2),
            },
        ];
        compile_read_in("RETURN $payload[0] AS first, $payload AS copied LIMIT $cap", &bindings, CompileLimits::default(), memory, context, |read, _| {
            assert_eq!(read.parameters().len(),2);
            assert_eq!(read.parameters()[0].name,"payload");
            assert_ne!(read.parameters()[0].name.as_ptr(),bindings[0].name.as_ptr());
            let QueryValue::List(copy)=read.parameters()[0].value else {unreachable!()};
            let QueryValue::String(copied_text)=copy.get(0).unwrap() else {unreachable!()};
            assert_eq!(copied_text,"hé");
            assert_ne!(copied_text.as_ptr(),text.as_ptr());
            let QueryValue::List(inner)=copy.get(1).unwrap() else {unreachable!()};
            assert!(matches!(inner.get(0),Some(QueryValue::I64(i64::MAX))));
            assert!(matches!(inner.get(1),Some(QueryValue::F64(v)) if v.to_bits()==0x7ff8_0000_0000_0042));
            let description=read.plan().description();
            assert_eq!(description.parameters.len(),2);
            assert!(description.operators.iter().any(|op|matches!(op.kind,OperatorKind::OffsetLimit {offset:0,limit:Some(2)})));
            Ok(())
        }).unwrap();
    });
}

#[test]
fn read_lowering_completed_path_predicate_keeps_self_list_dependencies_and_spans() {
    with_memory(|memory, context| {
        for query in [
            "MATCH (a)-[r*1..2 {x:size(r)}]->(b) RETURN r",
            "OPTIONAL MATCH (a)-[r:R*0..0 {x:size(r)+1,y:7}]->(b) RETURN r",
        ] {
            compile_read_in(query,&[],CompileLimits::default(),memory,context,|read,_| {
                let plan=read.plan().description();
                let (index,op)=plan.operators.iter().enumerate().find(|(_,op)|matches!(op.kind,OperatorKind::BoundedExpand {..})).unwrap();
                let OperatorKind::BoundedExpand {relationships,edge_predicate,completed_edge_predicate,..}=op.kind else {unreachable!()};
                assert!(edge_predicate.is_none(),"the complete property bag is evaluated at one consistent stage");
                let completed=completed_edge_predicate.unwrap();
                assert_ne!(completed.current_edge,relationships);
                assert!(read.plan().facts(PlanNodeId(index as u32)).unwrap().slot(completed.current_edge).is_none());
                let (size_index,operand)=plan.expressions.iter().enumerate().find_map(|(i,e)|if let Expression::Unary {operation:UnaryExpression::Size,operand}=e {Some((i,*operand))}else{None}).unwrap();
                assert!(matches!(plan.expressions[operand.0 as usize],Expression::Slot(slot) if slot==relationships));
                let span=read.expression_spans()[size_index];assert_eq!(&read.source()[span.start..span.end],"size(r)");
                if query.contains("y:7") {assert!(matches!(plan.expressions[completed.expression.0 as usize],Expression::Binary {operation:BinaryExpression::And,..}));}
                Ok(())
            }).unwrap();
        }
    });
}

#[test]
fn read_lowering_large_copied_parameter_preserves_split_four_byte_utf8() {
    use zeppelin_embed::property_graph::query::QueryValue;
    with_memory(|memory, context| {
        let text = format!("{}😀{}", "a".repeat(65535), "λ".repeat(32769));
        let parameters = [ParameterBinding {
            name: "text",
            value: QueryValue::String(&text),
        }];
        compile_read_in(
            "RETURN $text AS copied",
            &parameters,
            CompileLimits::default(),
            memory,
            context,
            |read, _| {
                let QueryValue::String(copied) = read.parameters()[0].value else {
                    unreachable!()
                };
                assert_eq!(copied, text);
                assert_ne!(copied.as_ptr(), text.as_ptr());
                assert_eq!(copied.len(), 131077);
                Ok(())
            },
        )
        .unwrap();
    });
}

#[test]
fn read_lowering_reused_bindings_use_fresh_candidates_and_exact_equalities() {
    use zeppelin_embed::property_graph::query::Comparison;
    with_memory(|memory, context| {
        compile_read_in(
            "MATCH (a)-[r:R]->(b), (b)-[s:S]->(a) MATCH (a)-[r:R]->(b) RETURN a,r,b,s",
            &[],
            CompileLimits::default(),
            memory,
            context,
            |read, _| {
                let plan = read.plan().description();
                let expansions = plan
                    .operators
                    .iter()
                    .filter_map(|op| {
                        if let OperatorKind::Expand {
                            source,
                            node,
                            relationship,
                            pattern,
                            ..
                        } = op.kind
                        {
                            Some((source.0, node.0, relationship.0, pattern.0))
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>();
                assert_eq!(expansions, [(0, 2, 1, 0), (2, 8, 3, 0), (0, 10, 9, 1)]);
                let equalities = plan
                    .operators
                    .iter()
                    .filter_map(|op| {
                        let OperatorKind::Filter(id) = op.kind else {
                            return None;
                        };
                        let Expression::Binary {
                            operation: BinaryExpression::Comparison(Comparison::Equal),
                            left,
                            right,
                        } = plan.expressions[id.0 as usize]
                        else {
                            return None;
                        };
                        let (Expression::Slot(left), Expression::Slot(right)) = (
                            plan.expressions[left.0 as usize],
                            plan.expressions[right.0 as usize],
                        ) else {
                            return None;
                        };
                        Some((left.0, right.0))
                    })
                    .collect::<Vec<_>>();
                assert_eq!(equalities, [(8, 0), (9, 1), (10, 2)]);
                let facts = read.plan().facts(plan.root).unwrap();
                assert_eq!(facts.width(), 4);
                for (i, kind) in [
                    ValueKinds::NODE,
                    ValueKinds::REL,
                    ValueKinds::NODE,
                    ValueKinds::REL,
                ]
                .into_iter()
                .enumerate()
                {
                    assert_eq!(facts.slot_at(i), Some((SlotId(i as u32 + 4), kind)));
                }
                Ok(())
            },
        )
        .unwrap();
    });
}
#[test]
fn read_lowering_zero_hop_optional_wildcard_preserves_direction_and_nullable_list() {
    with_memory(|memory, context| {
        compile_read_in(
            "OPTIONAL MATCH (a)<-[p:R|R*0..0]-(b) WHERE false RETURN *",
            &[],
            CompileLimits::default(),
            memory,
            context,
            |read, _| {
                let plan = read.plan().description();
                let path = plan
                    .operators
                    .iter()
                    .find(|op| matches!(op.kind, OperatorKind::BoundedExpand { .. }))
                    .unwrap();
                let OperatorKind::BoundedExpand {
                    direction,
                    min,
                    max,
                    relationship_types,
                    edge_predicate,
                    completed_edge_predicate,
                    ..
                } = path.kind
                else {
                    unreachable!()
                };
                assert_eq!((direction, min, max), (Direction::Incoming, 0, 0));
                assert_eq!(relationship_types.len(), 2);
                assert_eq!(relationship_types[0], relationship_types[1]);
                assert!(edge_predicate.is_none() && completed_edge_predicate.is_none());
                let optional = plan
                    .operators
                    .iter()
                    .find(|op| matches!(op.kind, OperatorKind::OptionalApply { .. }))
                    .unwrap();
                let OperatorKind::OptionalApply {
                    predicate: Some(id),
                } = optional.kind
                else {
                    unreachable!()
                };
                assert!(matches!(
                    plan.expressions[id.0 as usize],
                    Expression::Literal(Literal::Bool(false))
                ));
                assert_eq!(optional.inputs[0], PlanNodeId(0));
                let facts = read.plan().facts(plan.root).unwrap();
                for (i, kind) in [ValueKinds::NODE, ValueKinds::LIST, ValueKinds::NODE]
                    .into_iter()
                    .enumerate()
                {
                    assert_eq!(
                        facts.slot_at(i),
                        Some((SlotId(i as u32 + 3), kind.union(ValueKinds::NULL)))
                    );
                }
                assert_eq!(
                    read.columns().iter().map(|c| c.name).collect::<Vec<_>>(),
                    ["a", "p", "b"]
                );
                Ok(())
            },
        )
        .unwrap();
    });
}
#[test]
fn read_lowering_alias_priority_and_hidden_keys_leave_only_projected_scope() {
    with_memory(|memory, context| {
        compile_read_in("WITH 1 AS x MATCH (n) RETURN n AS x ORDER BY x.p, n.q DESC",&[],CompileLimits::default(),memory,context,|read,_| {
            let plan=read.plan().description();
            let sort=plan.operators.iter().find(|op|matches!(op.kind,OperatorKind::Sort(_))).unwrap();
            let OperatorKind::Sort(keys)=sort.kind else {unreachable!()};assert_eq!(keys.len(),2);
            for (key,wanted,property) in [(keys[0],2,"p"),(keys[1],1,"q")] {
                let Expression::Property {entity,name}=plan.expressions[key.expression.0 as usize] else {unreachable!()};assert_eq!(name.as_str(),property);
                assert!(matches!(plan.expressions[entity.0 as usize],Expression::Slot(slot) if slot==SlotId(wanted)));
            }
            assert!(!keys[0].descending);assert!(keys[1].descending);
            let sort_scope=read.plan().facts(sort.inputs[0]).unwrap();assert_eq!(sort_scope.width(),2);assert!(sort_scope.slot(SlotId(0)).is_none());
            assert_eq!(read.plan().facts(plan.root).unwrap().slot_at(0),Some((SlotId(2),ValueKinds::NODE)));
            assert_eq!(read.plan().facts(plan.root).unwrap().width(),1);
            Ok(())
        }).unwrap();
    });
}
#[test]
fn read_lowering_preserves_typed_profile_errors_and_explicit_call_refusal() {
    use zeppelin_embed::property_graph::query::QueryValue;
    use zeppelin_embed_cypher::ErrorKind;
    with_memory(|memory, context| {
        for (query, kind) in [
            ("RETURN missing", ErrorKind::UnknownVariable),
            ("RETURN 1 AS x,2 AS x", ErrorKind::DuplicateVariable),
            (
                "MATCH (n) RETURN DISTINCT n.p AS x ORDER BY n.q",
                ErrorKind::UnknownVariable,
            ),
            ("MATCH (n) RETURN count(*)+1 AS c", ErrorKind::Unsupported),
            ("RETURN 1 SKIP 1+1", ErrorKind::Unsupported),
            ("CREATE (n) RETURN n", ErrorKind::Unsupported),
            (
                "CALL ze.vector_search([1,2], 1, 'exact') YIELD node RETURN node",
                ErrorKind::SearchContext,
            ),
        ] {
            let mut entered = false;
            let baseline = memory.reserved_bytes();
            let error = compile_read_in(
                query,
                &[],
                CompileLimits::default(),
                memory,
                context,
                |_, _| {
                    entered = true;
                    Ok(())
                },
            )
            .unwrap_err();
            assert_eq!(error.kind, kind, "{query}");
            assert!(!entered);
            assert!(error.span.end > error.span.start, "{query}: {error:?}");
            assert_eq!(memory.reserved_bytes(), baseline);
        }
        for value in [QueryValue::I64(-1), QueryValue::F64(2.0), QueryValue::Null] {
            let params = [ParameterBinding {
                name: "limit",
                value,
            }];
            let error = compile_read_in(
                "RETURN 1 LIMIT $limit",
                &params,
                CompileLimits::default(),
                memory,
                context,
                |_, _| Ok(()),
            )
            .unwrap_err();
            assert_eq!(error.kind, ErrorKind::InvalidRange);
        }
        let params = [ParameterBinding {
            name: "limit",
            value: QueryValue::I64(i64::MAX),
        }];
        compile_read_in("RETURN 1 SKIP $limit LIMIT 0",&params,CompileLimits::default(),memory,context,|read,_| {
            assert!(read.plan().description().operators.iter().any(|op|matches!(op.kind,OperatorKind::OffsetLimit {offset,limit:Some(0)} if offset==i64::MAX as u64)));Ok(())
        }).unwrap();
    });
}

#[test]
fn read_lowering_copies_all_accepted_scalar_operation_families_and_label_synthesis() {
    with_memory(|memory, context| {
        let query = "MATCH (n)-[r:R]->() WHERE n:A:B RETURN labels(n) AS a,type(r) AS b,size([1]) AS c,ze.node_id(n) AS d,ze.relationship_id(r) AS e,ze.stored_text(n) AS f,NOT false AS g,+1 AS h,-1 AS i,null IS NULL AS j,null IS NOT NULL AS k,true AND false AS l,true OR false AS m,true XOR false AS n1,1=1 AS o,1<>2 AS p,1<2 AS q,1<=2 AS rr,2>1 AS s,2>=1 AS t,1+2 AS u,3-2 AS v,2*3 AS w,4/2 AS x,5%2 AS y,'x' STARTS WITH 'x' AS z1,'x' ENDS WITH 'x' AS z2,'x' CONTAINS 'x' AS z3,1 IN [1] AS z4,[1][0] AS z5";
        compile_read_in(
            query,
            &[],
            CompileLimits::default(),
            memory,
            context,
            |read, _| {
                let plan = read.plan().description();
                let unary = plan
                    .expressions
                    .iter()
                    .filter_map(|e| {
                        if let Expression::Unary { operation, .. } = e {
                            Some(format!("{operation:?}"))
                        } else {
                            None
                        }
                    })
                    .collect::<std::collections::BTreeSet<_>>();
                let wanted = [
                    "IsNotNull",
                    "IsNull",
                    "Labels",
                    "Negate",
                    "NodeIdText",
                    "Not",
                    "Positive",
                    "RelIdText",
                    "RelType",
                    "Size",
                    "StoredText",
                ]
                .into_iter()
                .map(str::to_string)
                .collect::<std::collections::BTreeSet<_>>();
                assert_eq!(unary, wanted);
                let binary = plan
                    .expressions
                    .iter()
                    .filter_map(|e| {
                        if let Expression::Binary { operation, .. } = e {
                            Some(format!("{operation:?}"))
                        } else {
                            None
                        }
                    })
                    .collect::<std::collections::BTreeSet<_>>();
                assert_eq!(binary.len(), 19);
                let labels = plan
                    .expressions
                    .iter()
                    .enumerate()
                    .filter_map(|(i, e)| {
                        if let Expression::HasLabel { label, .. } = e {
                            Some((i, label.as_str()))
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    labels.iter().map(|(_, s)| *s).collect::<Vec<_>>(),
                    ["A", "B"]
                );
                for (i, label) in labels {
                    let span = read.expression_spans()[i];
                    assert_eq!(&read.source()[span.start..span.end], label);
                }
                assert_eq!(read.columns().len(), 30);
                assert_eq!(read.operator_spans().len(), plan.operators.len());
                for span in read.operator_spans() {
                    assert!(span.end > span.start);
                    assert!(read.source().get(span.start..span.end).is_some());
                }
                Ok(())
            },
        )
        .unwrap();
    });
}
