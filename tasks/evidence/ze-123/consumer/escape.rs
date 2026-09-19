//! One controlled compiler/core ownership tracer, not a general lowerer or TCK.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::unreachable
)]
use std::mem::size_of;
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use zeppelin_embed::property_graph::{
    GraphGeneration, GraphName, StoreInstanceId,
    query::{Comparison, QueryView, ValueContext, plan::*, resources::*},
    resources::GraphResources,
};
use zeppelin_embed_cypher::{
    AstId, BoundQuery, CompileLimits, ErrorKind, NodeKind, ParseError, ResourceError, Span,
    compile_in,
};

const QUERY: &str = "MATCH (a)-[r:BASE|ALT]->(m) OPTIONAL MATCH (m)-[p:FIRST|SECOND*0..2 {weight:7}]->(b) WHERE b.ok=true RETURN a,r,m,p,b";
const CURRENT_EDGE: SlotId = SlotId(u32::MAX);
const OP_COUNT: usize = 6;
const QUERY_BYTES: usize = 1024 * 1024;
// Separately reserved bounded remapping frames and small stack descriptors.
const LOWERING_SCRATCH_BYTES: usize =
    size_of::<[bool; MAX_PLAN_NODES]>() + size_of::<[Option<ExprId>; MAX_PLAN_NODES]>() + 4096;

fn error(kind: ErrorKind, message: &'static str) -> ParseError {
    ParseError {
        kind,
        span: Span::default(),
        message,
    }
}
fn invariant(message: &'static str) -> ParseError {
    error(ErrorKind::BindingInvariant, message)
}
fn memory_error(_: MemoryError) -> ParseError {
    error(
        ErrorKind::Resource(ResourceError::Memory),
        "pattern tracer allocation",
    )
}
fn poll(control: &QueryControl) -> Result<(), ParseError> {
    control.checkpoint().map_err(|_| {
        error(
            ErrorKind::Resource(ResourceError::Cancelled),
            "pattern tracer cancelled",
        )
    })
}
fn region<T>(arena: &QueryArena<'_, '_, T>) -> RetainedRegion {
    RetainedRegion::declared(arena.as_slice().as_ptr() as usize, arena.heap_bytes()).unwrap()
}
#[derive(Clone, Copy, Debug)]
struct Metrics {
    frontend: usize,
    plan: usize,
    overlap: usize,
    heap: usize,
}
struct Scratch(std::path::PathBuf);
impl Scratch {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("ze123-pattern-consumer-{}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn ordinal(bound: &BoundQuery<'_>, id: AstId) -> Result<usize, ParseError> {
    let target = bound
        .syntax()
        .node(id)
        .ok_or_else(|| invariant("missing syntax node"))?;
    bound
        .syntax()
        .nodes()
        .iter()
        .position(|node| std::ptr::eq(node, target))
        .ok_or_else(|| invariant("foreign syntax node"))
}
fn ast_expr(bound: &BoundQuery<'_>, id: AstId) -> Result<ExprId, ParseError> {
    Ok(ExprId(
        u32::try_from(ordinal(bound, id)?).map_err(|_| invariant("syntax index"))?,
    ))
}
fn slot(bound: &BoundQuery<'_>, id: AstId) -> Result<SlotId, ParseError> {
    bound
        .fact(id)
        .and_then(|fact| fact.slot)
        .ok_or_else(|| invariant("unbound pattern slot"))
}
fn mark(
    id: ExprId,
    expressions: &[Expression<'_>],
    seen: &mut [bool; MAX_PLAN_NODES],
    control: &QueryControl,
    depth: usize,
) -> Result<(), ParseError> {
    poll(control)?;
    if depth > MAX_PLAN_DEPTH {
        return Err(invariant("expression depth"));
    }
    let index = id.0 as usize;
    if *seen
        .get(index)
        .ok_or_else(|| invariant("expression index"))?
    {
        return Ok(());
    }
    seen[index] = true;
    match *expressions
        .get(index)
        .ok_or_else(|| invariant("missing expression"))?
    {
        Expression::Property { entity, .. } => mark(entity, expressions, seen, control, depth + 1)?,
        Expression::Binary { left, right, .. } => {
            mark(left, expressions, seen, control, depth + 1)?;
            mark(right, expressions, seen, control, depth + 1)?;
        }
        Expression::Literal(Literal::I64(_) | Literal::Bool(_)) | Expression::Slot(_) => {}
        _ => return Err(invariant("outside controlled pattern expression subset")),
    }
    Ok(())
}

/// The only escape is T, which cannot contain either plan/fact/source lifetime.
/// Every plan pointer below refers to an independently copied real allocation.
fn with_pattern_plan<T>(
    text: &str,
    memory: &QueryMemory<'_>,
    control: &QueryControl,
    view: &QueryView,
    consume: impl for<'plan, 'facts> FnOnce(
        GraphPlan<'plan, 'facts>,
        &'plan [Span],
        &'plan str,
        Metrics,
    ) -> Result<T, ParseError>,
) -> Result<T, ParseError> {
    compile_in(
        text,
        &[],
        CompileLimits::default(),
        memory,
        control,
        |bound| {
            let frontend = memory.reserved_bytes();
            let mut external = memory.reserve_external_capacity().map_err(memory_error)?;
            external
                .reserve_additional(
                    LOWERING_SCRATCH_BYTES
                        + VALIDATION_SCRATCH_BYTES
                        + size_of::<Vec<NodeFacts>>()
                        + size_of::<GraphPlan<'_, '_>>()
                        + size_of::<PlanDescription<'_>>()
                        + size_of::<ValueContext<'_>>()
                        + OP_COUNT * size_of::<NodeFacts>(),
                )
                .map_err(memory_error)?;
            let ast = bound.syntax();
            let root = ast.node(ast.root()).ok_or_else(|| invariant("statement"))?;
            if root.children().len() != 3
                || !bound.parameters().is_empty()
                || !bound.calls().is_empty()
            {
                return Err(invariant("tracer only accepts two patterns and RETURN"));
            }
            let first = ast.node(root.children()[0]).unwrap();
            let optional = ast.node(root.children()[1]).unwrap();
            let returning = ast.node(root.children()[2]).unwrap();
            if first.kind != (NodeKind::Match { optional: false })
                || optional.kind != (NodeKind::Match { optional: true })
                || returning.kind
                    != (NodeKind::Projection {
                        with: false,
                        distinct: false,
                    })
            {
                return Err(invariant("controlled clause shape"));
            }
            let first_pattern = ast.node(first.children()[0]).unwrap();
            let second_pattern = ast.node(optional.children()[0]).unwrap();
            if first_pattern.children().len() != 3
                || second_pattern.children().len() != 3
                || optional.children().len() != 2
                || bound.columns().len() != 5
            {
                return Err(invariant("controlled pattern/projection width"));
            }
            let [a, r, m] = <[AstId; 3]>::try_from(first_pattern.children()).unwrap();
            let [anchor, p, b] = <[AstId; 3]>::try_from(second_pattern.children()).unwrap();
            let (a, r, m, p_slot, b_slot) = (
                slot(&bound, a)?,
                slot(&bound, r)?,
                slot(&bound, m)?,
                slot(&bound, p)?,
                slot(&bound, b)?,
            );
            if slot(&bound, anchor)? != m {
                return Err(invariant("optional anchor lost its left binding"));
            }
            let fixed = ast.node(first_pattern.children()[1]).unwrap();
            let bounded = ast.node(p).unwrap();
            let NodeKind::RelationshipPattern {
                direction: fixed_direction,
                bounds: None,
                ..
            } = fixed.kind
            else {
                return Err(invariant("fixed pattern"));
            };
            let NodeKind::RelationshipPattern {
                direction,
                bounds: Some(bounds),
                ..
            } = bounded.kind
            else {
                return Err(invariant("bounded pattern"));
            };
            if fixed_direction != zeppelin_embed_cypher::Direction::Outgoing
                || direction != fixed_direction
                || bounds.upper > 16
            {
                return Err(invariant("controlled direction/bounds"));
            }
            let predicate_syntax = ast.node(optional.children()[1]).unwrap();
            if predicate_syntax.kind != NodeKind::Predicate {
                return Err(invariant("attached optional predicate"));
            }
            let predicate = ast_expr(&bound, predicate_syntax.children()[0])?;
            let property_map = bounded
                .children()
                .iter()
                .find_map(|id| {
                    let node = ast.node(*id)?;
                    (node.kind == NodeKind::Properties).then_some(node)
                })
                .ok_or_else(|| invariant("per-edge property map"))?;
            if property_map.children().len() != 1 {
                return Err(invariant("controlled one-property edge predicate"));
            }
            let property = ast.node(property_map.children()[0]).unwrap();
            let NodeKind::Property(property_name) = property.kind else {
                return Err(invariant("property syntax"));
            };
            let property_value = ast_expr(&bound, property.children()[0])?;
            let property_text = ast.text(property_name).unwrap();

            let mut seen = [false; MAX_PLAN_NODES];
            let mut remap = [None; MAX_PLAN_NODES];
            mark(predicate, bound.expressions(), &mut seen, control, 1)?;
            mark(property_value, bound.expressions(), &mut seen, control, 1)?;
            for column in bound.columns() {
                mark(
                    column.expression,
                    bound.expressions(),
                    &mut seen,
                    control,
                    1,
                )?;
            }
            let mut count = 0;
            let mut name_bytes = property_text.len();
            for (index, expression) in bound.expressions().iter().enumerate() {
                poll(control)?;
                if !seen[index] {
                    continue;
                }
                remap[index] = Some(ExprId(count));
                count += 1;
                if let Expression::Property { name, .. } = expression {
                    name_bytes += name.as_str().len();
                }
            }
            let type_count = fixed
                .children()
                .iter()
                .chain(bounded.children())
                .filter(|id| matches!(ast.node(**id).unwrap().kind, NodeKind::Name(_)))
                .count();
            for id in fixed.children().iter().chain(bounded.children()) {
                if let NodeKind::Name(name) = ast.node(*id).unwrap().kind {
                    name_bytes += ast.text(name).unwrap().len();
                }
            }
            let mut source =
                QueryArena::<u8>::new(memory, ast.source().len()).map_err(memory_error)?;
            for byte in ast.source().bytes() {
                poll(control)?;
                source.push(byte).map_err(memory_error)?;
            }
            let mut bytes = QueryArena::<u8>::new(memory, name_bytes).map_err(memory_error)?;
            let mut offsets =
                QueryArena::<(usize, usize)>::new(memory, type_count + count as usize + 1)
                    .map_err(memory_error)?;
            for id in fixed.children().iter().chain(bounded.children()) {
                if let NodeKind::Name(name) = ast.node(*id).unwrap().kind {
                    let name = ast.text(name).unwrap();
                    let start = bytes.len();
                    for byte in name.bytes() {
                        poll(control)?;
                        bytes.push(byte).map_err(memory_error)?;
                    }
                    offsets.push((start, name.len())).map_err(memory_error)?;
                }
            }
            let fixed_count = fixed
                .children()
                .iter()
                .filter(|id| matches!(ast.node(**id).unwrap().kind, NodeKind::Name(_)))
                .count();
            let edge_property_offset = bytes.len();
            for byte in property_text.bytes() {
                poll(control)?;
                bytes.push(byte).map_err(memory_error)?;
            }
            for (index, expression) in bound.expressions().iter().enumerate() {
                if seen[index]
                    && let Expression::Property { name, .. } = expression
                {
                    let start = bytes.len();
                    for byte in name.as_str().bytes() {
                        poll(control)?;
                        bytes.push(byte).map_err(memory_error)?;
                    }
                    offsets
                        .push((start, name.as_str().len()))
                        .map_err(memory_error)?;
                }
            }
            let copied_name = |start: usize, length: usize| {
                GraphName::new(
                    std::str::from_utf8(&bytes.as_slice()[start..start + length]).unwrap(),
                )
                .unwrap()
            };
            let mut types = QueryArena::new(memory, type_count).map_err(memory_error)?;
            for &(start, length) in &offsets.as_slice()[..type_count] {
                types
                    .push(copied_name(start, length))
                    .map_err(memory_error)?;
            }
            let mut expressions =
                QueryArena::new(memory, count as usize + 3).map_err(memory_error)?;
            let mut property_index = type_count;
            for (index, expression) in bound.expressions().iter().enumerate() {
                if !seen[index] {
                    continue;
                }
                poll(control)?;
                let copied = match *expression {
                    Expression::Property { entity, name } => {
                        let (start, length) = offsets.as_slice()[property_index];
                        property_index += 1;
                        let copy = copied_name(start, length);
                        assert_ne!(copy.as_str().as_ptr(), name.as_str().as_ptr());
                        Expression::Property {
                            entity: remap[entity.0 as usize].unwrap(),
                            name: copy,
                        }
                    }
                    Expression::Binary {
                        operation,
                        left,
                        right,
                    } => Expression::Binary {
                        operation,
                        left: remap[left.0 as usize].unwrap(),
                        right: remap[right.0 as usize].unwrap(),
                    },
                    Expression::Literal(Literal::I64(v)) => Expression::Literal(Literal::I64(v)),
                    Expression::Literal(Literal::Bool(v)) => Expression::Literal(Literal::Bool(v)),
                    Expression::Slot(slot) => Expression::Slot(slot),
                    _ => return Err(invariant("unhandled controlled expression")),
                };
                expressions.push(copied).map_err(memory_error)?;
            }
            let edge = ExprId(expressions.len() as u32);
            expressions
                .push(Expression::Slot(CURRENT_EDGE))
                .map_err(memory_error)?;
            let edge_property = ExprId(expressions.len() as u32);
            expressions
                .push(Expression::Property {
                    entity: edge,
                    name: copied_name(edge_property_offset, property_text.len()),
                })
                .map_err(memory_error)?;
            let edge_predicate = ExprId(expressions.len() as u32);
            expressions
                .push(Expression::Binary {
                    operation: BinaryExpression::Comparison(Comparison::Equal),
                    left: edge_property,
                    right: remap[property_value.0 as usize].unwrap(),
                })
                .map_err(memory_error)?;
            let mut projections =
                QueryArena::new(memory, bound.columns().len()).map_err(memory_error)?;
            for column in bound.columns() {
                projections
                    .push(Projection {
                        slot: column.slot,
                        expression: remap[column.expression.0 as usize].unwrap(),
                    })
                    .map_err(memory_error)?;
            }
            let mut spans =
                QueryArena::new(memory, OP_COUNT + expressions.len()).map_err(memory_error)?;
            for span in [
                first.span,
                first.span,
                fixed.span,
                bounded.span,
                optional.span,
                returning.span,
            ] {
                spans.push(span).map_err(memory_error)?;
            }
            // Source map order is every operator followed by every compact expression.
            for (index, present) in seen.iter().enumerate().take(bound.expressions().len()) {
                if *present {
                    spans.push(ast.nodes()[index].span).map_err(memory_error)?;
                }
            }
            for span in [bounded.span, property.span, property.span] {
                spans.push(span).map_err(memory_error)?;
            }
            let mut inputs = QueryArena::new(memory, 6).map_err(memory_error)?;
            for id in [0, 1, 2, 2, 3, 4] {
                inputs.push(PlanNodeId(id)).map_err(memory_error)?;
            }
            let mut operators = QueryArena::new(memory, OP_COUNT).map_err(memory_error)?;
            for operator in [
                Operator {
                    inputs: &[],
                    kind: OperatorKind::Unit,
                },
                Operator {
                    inputs: &inputs.as_slice()[0..1],
                    kind: OperatorKind::ScanNodes {
                        output: a,
                        label: None,
                    },
                },
                Operator {
                    inputs: &inputs.as_slice()[1..2],
                    kind: OperatorKind::Expand {
                        source: a,
                        node: m,
                        relationship: r,
                        direction: Direction::Outgoing,
                        relationship_types: &types.as_slice()[..fixed_count],
                        pattern: PatternId(0),
                    },
                },
                Operator {
                    inputs: &inputs.as_slice()[2..3],
                    kind: OperatorKind::BoundedExpand {
                        source: m,
                        node: b_slot,
                        relationships: p_slot,
                        min: bounds.lower as u8,
                        max: bounds.upper as u8,
                        direction: Direction::Outgoing,
                        relationship_types: &types.as_slice()[fixed_count..],
                        pattern: PatternId(1),
                        edge_predicate: Some(EdgePredicate {
                            current_edge: CURRENT_EDGE,
                            expression: edge_predicate,
                        }),
                    },
                },
                Operator {
                    inputs: &inputs.as_slice()[3..5],
                    kind: OperatorKind::OptionalApply {
                        predicate: Some(remap[predicate.0 as usize].unwrap()),
                    },
                },
                Operator {
                    inputs: &inputs.as_slice()[5..6],
                    kind: OperatorKind::Project(projections.as_slice()),
                },
            ] {
                operators.push(operator).map_err(memory_error)?;
            }
            let mut facts = Vec::new();
            facts
                .try_reserve_exact(OP_COUNT)
                .map_err(|_| memory_error(MemoryError::Allocation))?;
            if facts.capacity() > OP_COUNT {
                external
                    .reserve_additional((facts.capacity() - OP_COUNT) * size_of::<NodeFacts>())
                    .map_err(memory_error)?;
            }
            facts.resize_with(OP_COUNT, NodeFacts::default);
            let mut regions = QueryArena::new(memory, 10).map_err(memory_error)?;
            for region in [
                region(&source),
                region(&bytes),
                region(&offsets),
                region(&types),
                region(&expressions),
                region(&projections),
                region(&spans),
                region(&inputs),
                region(&operators),
                RetainedRegion::vector(&facts).unwrap(),
            ] {
                regions.push(region).map_err(memory_error)?;
            }
            regions.as_mut_slice().sort_unstable();
            let overlap = memory.reserved_bytes();
            let metrics = Metrics {
                frontend,
                plan: overlap - frontend,
                overlap,
                heap: regions
                    .as_slice()
                    .iter()
                    .map(|region| region.end() - region.start())
                    .sum::<usize>()
                    + regions.heap_bytes(),
            };
            let description = PlanDescription {
                operators: operators.as_slice(),
                expressions: expressions.as_slice(),
                parameters: &[],
                root: PlanNodeId(5),
                eager_searches: &[],
            };
            let mut context = ValueContext::new(view, control, 8_000_000)
                .map_err(|_| invariant("value context"))?;
            let plan = GraphPlan::validate_with_fact_vec(
                description,
                &mut facts,
                PlanFootprint::declared(metrics.plan),
                PlanBacking::new(regions.as_slice(), regions.heap_bytes()).unwrap(),
                &mut context,
            )
            .map_err(|failure| {
                eprintln!("plan failure: {failure:?}");
                invariant("copied plan rejected")
            })?;
            let owned_source = std::str::from_utf8(source.as_slice()).unwrap();
            assert_ne!(owned_source.as_ptr(), ast.source().as_ptr());
            poll(control)?;
            let result = consume(plan, spans.as_slice(), owned_source, metrics)?;
            poll(control)?;
            Ok(result)
        },
    )
}

#[test]
fn scoped_pattern_consumer_preserves_complete_projection_and_private_edge_scope() {
    let directory = Scratch::new();
    let store = Store::open(
        &directory.0,
        OpenOptions::new().with_max_resident_bytes(4 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let initial = shared.reserved_bytes().unwrap();
    {
        let memory = QueryMemory::new(&shared, QUERY_BYTES).unwrap();
        let baseline = memory.reserved_bytes();
        let control = QueryControl::Cancel(CancelToken::new());
        let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
        let width = with_pattern_plan(QUERY, &memory, &control, &view, |plan, spans, source, metrics| {
            let description = plan.description();
            let facts = plan.facts(description.root).unwrap();
            assert_eq!(facts.width(), 5, "ZE123 complete RETURN must survive the scoped consumer");
            assert!(facts.barriers().optional());
            assert_eq!(facts.slot(CURRENT_EDGE), None);
            for (index, kinds) in [ValueKinds::NODE, ValueKinds::REL, ValueKinds::NODE, ValueKinds::LIST.union(ValueKinds::NULL), ValueKinds::NODE.union(ValueKinds::NULL)].into_iter().enumerate() {
                assert_eq!(facts.slot_at(index), Some((SlotId(index as u32 + 5), kinds)));
            }
            assert_eq!(facts.slot_at(5), None);
            let OperatorKind::Expand { relationship_types, pattern, .. } = description.operators[2].kind else { unreachable!() };
            assert_eq!(relationship_types.iter().map(|n| n.as_str()).collect::<Vec<_>>(), ["BASE", "ALT"], "ZE123 fixed OR alternatives must be complete");
            assert_eq!(pattern, PatternId(0));
            let OperatorKind::BoundedExpand { relationship_types, source: anchor, node, relationships, min, max, pattern, edge_predicate, .. } = description.operators[3].kind else { unreachable!() };
            assert_eq!(relationship_types.iter().map(|n| n.as_str()).collect::<Vec<_>>(), ["FIRST", "SECOND"], "ZE123 bounded OR alternatives must be complete");
            assert_eq!((anchor, node, relationships, min, max, pattern), (SlotId(2), SlotId(4), SlotId(3), 0, 2, PatternId(1)));
            assert_eq!(description.operators[3].inputs, [PlanNodeId(2)], "right expansion is correlated to each left row");
            assert_eq!(description.operators[4].inputs, [PlanNodeId(2), PlanNodeId(3)]);
            let edge_predicate = edge_predicate.expect("ZE123 per-edge predicate must survive");
            assert_eq!(edge_predicate.current_edge, CURRENT_EDGE);
            assert_eq!(plan.facts(PlanNodeId(3)).unwrap().slot(CURRENT_EDGE), None);
            let Expression::Binary { operation: BinaryExpression::Comparison(Comparison::Equal), left, right } = description.expressions[edge_predicate.expression.0 as usize] else { unreachable!() };
            let Expression::Property { entity, name } = description.expressions[left.0 as usize] else { unreachable!() };
            assert_eq!(name.as_str(), "weight");
            assert!(matches!(description.expressions[entity.0 as usize], Expression::Slot(CURRENT_EDGE)));
            assert!(matches!(description.expressions[right.0 as usize], Expression::Literal(Literal::I64(7))));
            let OperatorKind::OptionalApply { predicate: Some(predicate) } = description.operators[4].kind else { unreachable!() };
            let Expression::Binary { operation: BinaryExpression::Comparison(Comparison::Equal), left, right } = description.expressions[predicate.0 as usize] else { unreachable!() };
            let Expression::Property { entity, name } = description.expressions[left.0 as usize] else { unreachable!() };
            assert_eq!(name.as_str(), "ok");
            assert!(matches!(description.expressions[entity.0 as usize], Expression::Slot(SlotId(4))));
            assert!(matches!(description.expressions[right.0 as usize], Expression::Literal(Literal::Bool(true))));
            for span in spans { assert!(source.get(span.start..span.end).is_some()); }

            assert_eq!(source, QUERY);
            assert_eq!(spans.len(), OP_COUNT + description.expressions.len());
            assert_eq!(&source[spans[2].start..spans[2].end], "-[r:BASE|ALT]->");
            assert_eq!(&source[spans[3].start..spans[3].end], "-[p:FIRST|SECOND*0..2 {weight:7}]->");
            assert!(source[spans[5].start..spans[5].end].starts_with("RETURN"));
            assert_eq!(memory.reserved_bytes(), metrics.overlap);
            assert_eq!(metrics.overlap, metrics.frontend + metrics.plan);
            assert!(metrics.frontend > baseline + 65536);
            assert!(metrics.plan >= metrics.heap + VALIDATION_SCRATCH_BYTES + LOWERING_SCRATCH_BYTES);
            assert_eq!(shared.reserved_bytes().unwrap(), initial + metrics.overlap as u64);
            eprintln!("pattern consumer frontend={} plan={} heap={} overlap={} peak={} query_limit={}", metrics.frontend, metrics.plan, metrics.heap, metrics.overlap, memory.peak_reserved_bytes(), QUERY_BYTES);
            Ok(facts.width())
        }).unwrap();
        assert_eq!(width, 5);
        assert_eq!(memory.reserved_bytes(), baseline);
    }
    assert_eq!(shared.reserved_bytes().unwrap(), initial);
    store.close().unwrap();
}

#[test]
fn scoped_pattern_consumer_releases_actual_owners_on_capacity_and_final_cancel() {
    let directory = Scratch::new();
    let store = Store::open(
        &directory.0,
        OpenOptions::new().with_max_resident_bytes(4 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&store).unwrap();
    let initial = shared.reserved_bytes().unwrap();
    let view = QueryView::new(StoreInstanceId::new(1).unwrap(), GraphGeneration::new(0));
    let needed = {
        let memory = QueryMemory::new(&shared, QUERY_BYTES).unwrap();
        let baseline = memory.reserved_bytes();
        let token = CancelToken::new();
        let control = QueryControl::Cancel(token.clone());
        let metrics = with_pattern_plan(QUERY, &memory, &control, &view, |_, _, _, metrics| {
            Ok(metrics)
        })
        .unwrap();
        assert_eq!(memory.reserved_bytes(), baseline);
        let mut consumed = false;
        let error = with_pattern_plan(QUERY, &memory, &control, &view, |_, _, _, _| {
            consumed = true;
            token.cancel();
            Ok(7)
        })
        .unwrap_err();
        assert!(
            consumed,
            "cancellation must fire after the new owner seam constructed its plan"
        );
        assert_eq!(error.kind, ErrorKind::Resource(ResourceError::Cancelled));
        assert_eq!(memory.reserved_bytes(), baseline);
        metrics.overlap
    };
    assert_eq!(shared.reserved_bytes().unwrap(), initial);
    {
        let memory = QueryMemory::new(&shared, needed - 1).unwrap();
        let baseline = memory.reserved_bytes();
        let control = QueryControl::Cancel(CancelToken::new());
        let mut consumed = false;
        let error = with_pattern_plan(QUERY, &memory, &control, &view, |_, _, _, _| {
            consumed = true;
            Ok(())
        })
        .unwrap_err();
        assert!(!consumed);
        assert_eq!(error.kind, ErrorKind::Resource(ResourceError::Memory));
        assert_eq!(memory.reserved_bytes(), baseline);
    }
    assert_eq!(shared.reserved_bytes().unwrap(), initial);
    store.close().unwrap();
}

fn ownership_probe<'escaped>(memory: &QueryMemory<'_>, control: &QueryControl, view: &QueryView) -> Result<GraphPlan<'escaped, 'escaped>, ParseError> {
    with_pattern_plan(QUERY, memory, control, view, |plan, _, _, _| Ok(plan))
}
