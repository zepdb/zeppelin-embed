//! Frozen C plan descriptors to caller-owned native arenas.
use super::{Pool, invalid, read_exact, store_error};
use crate::{error::FfiError, marshal, *};
use zeppelin_embed::property_graph::query::plan::*;
use zeppelin_embed::property_graph::query::plan::{SearchOptions, SearchRequest};
use zeppelin_embed::property_graph::query::{Arithmetic as A, Comparison as C, StringPredicate};
use zeppelin_embed::property_graph::{EntityKind, GraphName, GraphPlanBacking, GraphQueryPlan};

fn slice<T>(pointer: *const T, count: usize) -> Result<Vec<T>, FfiError>
where
    T: Copy,
{
    if count > 524_288 {
        return Err(invalid("plan arena exceeds 524288 elements"));
    }
    let source = marshal::read_slice(pointer, count).map_err(|e| invalid(e.0))?;
    let mut result = Vec::new();
    result
        .try_reserve_exact(count)
        .map_err(|_| FfiError::new(ZeErrorCode::ZeErrOutOfMemory, "plan arena"))?;
    result.extend_from_slice(source);
    Ok(result)
}
fn span<T>(values: &[T], range: ZeGraphRange) -> Result<&[T], FfiError> {
    let range = range
        .checked_range(values.len())
        .map_err(|_| invalid("plan range out of bounds"))?;
    values
        .get(range)
        .ok_or_else(|| invalid("plan range out of bounds"))
}
fn optional(index: ZeGraphOptionalIndex) -> Result<Option<u32>, FfiError> {
    index
        .validate_shape()
        .map_err(|_| invalid("invalid optional plan index"))?;
    Ok((index.present == 1).then_some(index.index))
}
fn name<'a>(pool: &Pool<'a>, range: ZeGraphRange) -> Result<GraphName<'a>, FfiError> {
    GraphName::new(pool.text(range, "plan name")?).map_err(|_| invalid("invalid graph name"))
}
fn unary(tag: u32) -> Result<UnaryExpression, FfiError> {
    use UnaryExpression::*;
    Ok(match tag {
        0 => Not,
        1 => Positive,
        2 => Negate,
        3 => IsNull,
        4 => IsNotNull,
        5 => Size,
        6 => Labels,
        7 => RelType,
        8 => StoredText,
        9 => NodeIdText,
        10 => RelIdText,
        _ => return Err(invalid("unknown unary operation")),
    })
}
fn binary(tag: u32) -> Result<BinaryExpression, FfiError> {
    use BinaryExpression::*;
    Ok(match tag {
        0 => And,
        1 => Or,
        2 => Xor,
        3 => Comparison(C::Equal),
        4 => Comparison(C::NotEqual),
        5 => Comparison(C::Less),
        6 => Comparison(C::LessEqual),
        7 => Comparison(C::Greater),
        8 => Comparison(C::GreaterEqual),
        9 => Arithmetic(A::Add),
        10 => Arithmetic(A::Subtract),
        11 => Arithmetic(A::Multiply),
        12 => Arithmetic(A::Divide),
        13 => Arithmetic(A::Remainder),
        14 => String(StringPredicate::StartsWith),
        15 => String(StringPredicate::EndsWith),
        16 => String(StringPredicate::Contains),
        17 => In,
        18 => Index,
        _ => return Err(invalid("unknown binary operation")),
    })
}

fn expression_descriptor(kind: u32) -> ZeGraphExpression {
    // Every field is a scalar or range; all-zero inactive fields are the frozen convention.
    let mut value: ZeGraphExpression = unsafe { std::mem::zeroed() };
    value.abi_size = std::mem::size_of::<ZeGraphExpression>() as u32;
    value.kind = kind;
    value
}
fn list_literal(
    pool: &Pool<'_>,
    index: u32,
    depth: usize,
    ancestors: &mut Vec<u32>,
    expressions: &mut Vec<ZeGraphExpression>,
    children: &mut Vec<ExprId>,
) -> Result<u32, FfiError> {
    if depth > 16 || ancestors.contains(&index) || expressions.len() >= 4096 {
        return Err(invalid("cyclic, over-depth or oversized plan literal"));
    }
    let value = pool.value(index, "plan literal")?;
    let mut expression = expression_descriptor(0);
    expression.value = index;
    if value.tag == 7 {
        if depth >= 16 {
            return Err(invalid("over-depth plan literal"));
        }
        expression.kind = 7;
        ancestors.push(index);
        let mut ids = Vec::new();
        for child in pool.children(value.range)? {
            if value.list_kind != 0
                && value.list_kind != 5
                && pool.value(*child, "typed literal child")?.tag != value.list_kind
            {
                return Err(invalid("typed literal list kind mismatch"));
            }
            ids.push(ExprId(list_literal(
                pool,
                *child,
                depth + 1,
                ancestors,
                expressions,
                children,
            )?));
        }
        ancestors.pop();
        expression.children = ZeGraphRange {
            start: children.len() as u32,
            count: ids.len() as u32,
        };
        children.extend(ids);
    }
    let id = expressions.len() as u32;
    expressions.push(expression);
    Ok(id)
}

pub(super) fn with_plan<R>(
    pointer: *const ZeGraphPlan,
    bindings: &[ParameterBinding<'_>],
    backing: GraphPlanBacking<'_>,
    run: impl FnOnce(&GraphQueryPlan<'_>, bool) -> Result<R, FfiError>,
) -> Result<R, FfiError> {
    let raw = read_exact(pointer, |p| p.abi_size, "graph plan")?;
    raw.validate_header()
        .map_err(|_| invalid("graph plan header"))?;
    if raw.reserved != 0
        || raw.operator_count > 4096
        || raw.expression_count > 4096
        || raw.search_count > 8
        || raw.parameter_count > 256
    {
        return Err(invalid("graph plan bounds or reserved fields"));
    }
    let mut descriptor = read_exact(raw.pool, |p| p.abi_size, "plan pool")?;
    let bytes = slice(descriptor.bytes, descriptor.byte_count)?;
    descriptor.bytes = bytes.as_ptr();
    let pool = Pool::read(&descriptor, "plan pool")?;
    let raw_ops = slice(raw.operators, raw.operator_count)?;
    let mut raw_exprs = slice(raw.expressions, raw.expression_count)?;
    let mut inputs = slice(raw.inputs, raw.input_count)?
        .into_iter()
        .map(PlanNodeId)
        .collect::<Vec<_>>();
    let mut children = slice(raw.expression_children, raw.expression_child_count)?
        .into_iter()
        .map(ExprId)
        .collect::<Vec<_>>();
    let original_expressions = raw_exprs.len();
    for index in 0..original_expressions {
        let e = *raw_exprs
            .get(index)
            .ok_or_else(|| invalid("literal expression"))?;
        if e.kind == 0 && pool.value(e.value, "literal")?.tag == 7 {
            let id = list_literal(
                &pool,
                e.value,
                0,
                &mut Vec::new(),
                &mut raw_exprs,
                &mut children,
            )?;
            let expanded = *raw_exprs
                .get(id as usize)
                .ok_or_else(|| invalid("expanded literal"))?;
            raw_exprs.pop();
            *raw_exprs
                .get_mut(index)
                .ok_or_else(|| invalid("literal expression"))? = expanded;
        }
    }
    let mut eligible_groups = Vec::new();
    let mut eligible_expressions = Vec::new();
    for op in &raw_ops {
        if op.kind == 20 {
            if eligible_expressions
                .iter()
                .any(|(slot, _)| *slot == op.set_slot)
            {
                return Err(invalid("duplicate eligible set slot"));
            }
            let mut source = expression_descriptor(1);
            source.value = op.source_slot;
            let source_id = raw_exprs.len() as u32;
            raw_exprs.push(source);
            let mut aggregate = expression_descriptor(8);
            aggregate.operation = 1;
            aggregate.has_operand = 1;
            aggregate.left = source_id;
            aggregate.distinct = 1;
            let aggregate_id = raw_exprs.len() as u32;
            raw_exprs.push(aggregate);
            eligible_groups.push((
                op.set_slot,
                vec![Projection {
                    slot: SlotId(op.set_slot),
                    expression: ExprId(aggregate_id),
                }],
            ));
            let mut set = expression_descriptor(1);
            set.value = op.set_slot;
            let set_id = raw_exprs.len() as u32;
            raw_exprs.push(set);
            eligible_expressions.push((op.set_slot, ExprId(set_id)));
        }
    }
    if raw_exprs.len() > 4096 {
        return Err(invalid("expanded plan exceeds expression limit"));
    }
    let raw_projections = slice(raw.projections, raw.projection_count)?;
    let mut projections = Vec::new();
    for p in &raw_projections {
        p.validate_header()
            .map_err(|_| invalid("projection header"))?;
        projections.push(Projection {
            slot: SlotId(p.slot),
            expression: ExprId(p.expression),
        });
    }
    let raw_sorts = slice(raw.sort_keys, raw.sort_key_count)?;
    let mut sorts = Vec::new();
    for p in &raw_sorts {
        p.validate_header().map_err(|_| invalid("sort header"))?;
        if p.descending > 1 {
            return Err(invalid("sort descending flag"));
        }
        sorts.push(SortKey {
            expression: ExprId(p.expression),
            descending: p.descending == 1,
        });
    }
    let raw_mutations = slice(raw.mutations, raw.mutation_count)?;
    let mut label_groups = Vec::new();
    for m in &raw_mutations {
        let mut labels = Vec::new();
        for range in pool.names(m.labels)? {
            labels.push(name(&pool, *range)?);
        }
        label_groups.push(labels);
    }
    let mut mutations = Vec::new();
    for (m, labels) in raw_mutations.iter().zip(&label_groups) {
        m.validate_header()
            .map_err(|_| invalid("mutation header"))?;
        if m.present > 1 || m.detach > 1 {
            return Err(invalid("mutation flag"));
        }
        let mut inactive = *m;
        let mutation = match m.kind {
            0 => {
                inactive.output = 0;
                inactive.labels = ZeGraphRange { start: 0, count: 0 };
                Mutation::CreateNode {
                    output: SlotId(m.output),
                    labels,
                }
            }
            1 => {
                inactive.output = 0;
                inactive.source = 0;
                inactive.target = 0;
                inactive.name = ZeGraphRange { start: 0, count: 0 };
                Mutation::CreateRelationship {
                    output: SlotId(m.output),
                    source: ExprId(m.source),
                    target: ExprId(m.target),
                    relationship_type: name(&pool, m.name)?,
                }
            }
            2 => {
                inactive.entity = 0;
                inactive.name = ZeGraphRange { start: 0, count: 0 };
                Mutation::RemoveProperty {
                    entity: ExprId(m.entity),
                    name: name(&pool, m.name)?,
                }
            }
            3 => {
                inactive.entity = 0;
                inactive.name = ZeGraphRange { start: 0, count: 0 };
                inactive.present = 0;
                Mutation::SetLabel {
                    entity: ExprId(m.entity),
                    label: name(&pool, m.name)?,
                    present: m.present == 1,
                }
            }
            4 => {
                inactive.entity = 0;
                inactive.detach = 0;
                Mutation::Delete {
                    entity: ExprId(m.entity),
                    detach: m.detach == 1,
                }
            }
            5 => {
                inactive.entity = 0;
                inactive.value = 0;
                inactive.name = ZeGraphRange { start: 0, count: 0 };
                Mutation::SetProperty {
                    entity: ExprId(m.entity),
                    name: name(&pool, m.name)?,
                    value: ExprId(m.value),
                }
            }
            _ => return Err(invalid("unknown mutation kind")),
        };
        if inactive.output != 0
            || inactive.entity != 0
            || inactive.value != 0
            || inactive.source != 0
            || inactive.target != 0
            || inactive.present != 0
            || inactive.detach != 0
            || inactive.name.start != 0
            || inactive.name.count != 0
            || inactive.labels.start != 0
            || inactive.labels.count != 0
        {
            return Err(invalid("inactive mutation fields"));
        }
        mutations.push(mutation);
    }
    let mut expressions = Vec::new();
    for e in &raw_exprs {
        e.validate_header()
            .map_err(|_| invalid("expression header"))?;
        if e.reserved != 0 || e.has_operand > 1 || e.distinct > 1 {
            return Err(invalid("expression flags"));
        }
        let mut inactive = *e;
        match e.kind {
            0..=2 => inactive.value = 0,
            3 => {
                inactive.operation = 0;
                inactive.left = 0;
            }
            4 => {
                inactive.operation = 0;
                inactive.left = 0;
                inactive.right = 0;
            }
            5 | 6 => {
                inactive.left = 0;
                inactive.name = ZeGraphRange { start: 0, count: 0 };
            }
            7 => inactive.children = ZeGraphRange { start: 0, count: 0 },
            8 => {
                inactive.operation = 0;
                if inactive.has_operand == 1 {
                    inactive.left = 0;
                }
                inactive.has_operand = 0;
                inactive.distinct = 0;
            }
            _ => return Err(invalid("expression kind")),
        }
        if inactive.value != 0
            || inactive.operation != 0
            || inactive.left != 0
            || inactive.right != 0
            || inactive.name.start != 0
            || inactive.name.count != 0
            || inactive.children.start != 0
            || inactive.children.count != 0
            || inactive.has_operand != 0
            || inactive.distinct != 0
        {
            return Err(invalid("inactive expression fields"));
        }
        let value = match e.kind {
            0 => {
                let v = pool.value(e.value, "plan literal")?;
                Expression::Literal(match v.tag {
                    0 => Literal::Null,
                    1 => Literal::Bool(v.boolean == 1),
                    2 => Literal::I64(v.integer),
                    3 => Literal::F64(v.floating),
                    4 => Literal::String(pool.text(v.range, "literal")?),
                    _ => return Err(invalid("plan literal must be scalar")),
                })
            }
            1 => Expression::Slot(SlotId(e.value)),
            2 => Expression::Parameter(ParameterId(e.value)),
            3 => Expression::Unary {
                operation: unary(e.operation)?,
                operand: ExprId(e.left),
            },
            4 => Expression::Binary {
                operation: binary(e.operation)?,
                left: ExprId(e.left),
                right: ExprId(e.right),
            },
            5 => Expression::Property {
                entity: ExprId(e.left),
                name: name(&pool, e.name)?,
            },
            6 => Expression::HasLabel {
                entity: ExprId(e.left),
                label: name(&pool, e.name)?,
            },
            7 => Expression::List(span(&children, e.children)?),
            8 => Expression::Aggregate {
                operation: match e.operation {
                    0 => AggregateExpression::Count {
                        distinct: e.distinct == 1,
                    },
                    1 => AggregateExpression::Collect {
                        distinct: e.distinct == 1,
                    },
                    _ => return Err(invalid("aggregate operation")),
                },
                operand: (e.has_operand == 1).then_some(ExprId(e.left)),
            },
            _ => return Err(invalid("unknown expression kind")),
        };
        expressions.push(value);
    }
    let raw_parameters = slice(raw.parameters, raw.parameter_count)?;
    let mut parameters = Vec::new();
    for p in &raw_parameters {
        p.validate_header()
            .map_err(|_| invalid("parameter header"))?;
        if p.reserved != 0 || p.kinds == 0 || p.kinds & !159 != 0 {
            return Err(invalid("parameter kinds"));
        }
        let mut kinds = ValueKinds::default();
        for (bit, kind) in [
            (1, ValueKinds::NULL),
            (2, ValueKinds::BOOL),
            (4, ValueKinds::I64),
            (8, ValueKinds::F64),
            (16, ValueKinds::STRING),
            (128, ValueKinds::LIST),
        ] {
            if p.kinds & bit != 0 {
                kinds = kinds.union(kind);
            }
        }
        parameters.push(Parameter {
            name: pool.text(p.name, "parameter name")?,
            kinds,
        });
    }
    let eager = slice(raw.eager_searches, raw.eager_search_count)?
        .into_iter()
        .map(PlanNodeId)
        .collect::<Vec<_>>();
    let raw_searches = slice(raw.searches, raw.search_count)?;
    // Native sources take a singleton scope; frozen C sources have no row input.
    let mut raw_ops = raw_ops;
    for op in &raw_ops {
        let expected = match op.kind {
            0 | 4 | 10 | 11 | 12 => 0,
            1 | 15 => 2,
            8 => u32::from(
                raw_searches
                    .get(op.search as usize)
                    .ok_or_else(|| invalid("search index"))?
                    .eligible_set
                    .present
                    == 1,
            ),
            _ => 1,
        };
        if op.inputs.count != expected {
            return Err(invalid("operator input arity"));
        }
    }
    if raw_ops
        .iter()
        .any(|o| matches!(o.kind, 4 | 8 | 10 | 11 | 12) && o.inputs.count == 0)
    {
        let unit_index = raw_ops.len() as u32;
        for op in &mut raw_ops {
            if matches!(op.kind, 4 | 8 | 10 | 11 | 12) && op.inputs.count == 0 {
                op.inputs = ZeGraphRange {
                    start: inputs.len() as u32,
                    count: 1,
                };
                inputs.push(PlanNodeId(unit_index));
            }
        }
        raw_ops.push(ZeGraphOperator {
            abi_size: std::mem::size_of::<ZeGraphOperator>() as u32,
            ..unsafe { std::mem::zeroed() }
        });
    }
    let mut types = Vec::new();
    for op in &raw_ops {
        let mut names = Vec::new();
        for range in pool.names(op.relationship_types)? {
            names.push(name(&pool, *range)?);
        }
        types.push(names);
    }

    for index in 0..raw_searches.len() {
        if raw_ops
            .iter()
            .filter(|op| op.kind == 8 && op.search as usize == index)
            .count()
            != 1
        {
            return Err(invalid("search descriptors must each name one eager call"));
        }
    }
    let mut search_requests = Vec::new();
    for search in &raw_searches {
        search
            .validate_header()
            .map_err(|_| invalid("search header"))?;
        if search.reserved != 0 || search.has_tier > 1 || (search.has_tier == 0 && search.tier != 0)
        {
            return Err(invalid("search reserved/tier"));
        }
        let vector = optional(search.vector)?.map(ExprId);
        let text = optional(search.text)?.map(ExprId);
        let eligible = optional(search.eligible_set)?
            .map(|slot| {
                eligible_expressions
                    .iter()
                    .find(|(id, _)| *id == slot)
                    .map(|(_, expression)| *expression)
                    .ok_or_else(|| invalid("eligible set slot not produced"))
            })
            .transpose()?;
        let mode = if search.has_tier == 0 {
            SearchMode::Default
        } else {
            match search.tier {
                0 => SearchMode::Auto,
                1 => SearchMode::Exact,
                2 => SearchMode::Scan,
                3 => SearchMode::Graph,
                _ => return Err(invalid("unsupported search tier")),
            }
        };
        let mut options = SearchOptions {
            hide_input: true,
            window: optional(search.window)?.map(ExprId),
            ..Default::default()
        };
        if !search.options.is_null() {
            let raw = read_exact(search.options, |p| p.abi_size, "search options")?;
            raw.validate_header()
                .map_err(|_| invalid("search options header"))?;
            if raw.reserved != 0
                || raw.graph_profile > 1
                || raw.lexical_flags > 1
                || raw.rescore > 1
                || raw.has_alpha > 1
                || raw.rules_enabled > 1
                || raw.has_max_rounds > 1
                || (raw.has_alpha == 0 && raw.alpha.to_bits() != 0)
                || (raw.has_max_rounds == 0 && raw.max_rounds != 0)
                || !raw.alpha.is_finite()
                || !(0.0..=1.0).contains(&raw.alpha)
            {
                return Err(invalid("search options shape"));
            }
            if (search.kind == 0
                && (raw.lexical_flags != 0
                    || raw.has_alpha != 0
                    || raw.rules_enabled != 0
                    || raw.has_max_rounds != 0))
                || (search.kind == 1
                    && (raw.graph_profile != 0
                        || raw.graph_ef != 0
                        || raw.graph_seed != 0
                        || raw.rescore != 0
                        || raw.has_alpha != 0
                        || raw.rules_enabled != 0
                        || raw.has_max_rounds != 0))
            {
                return Err(invalid("irrelevant search options"));
            }
            options.graph_profile = (search.kind != 1).then_some(if raw.graph_profile == 0 {
                zeppelin_embed::graph::search::GraphSearchProfile::SiftClass
            } else {
                zeppelin_embed::graph::search::GraphSearchProfile::Angular
            });
            options.graph_ef = raw.graph_ef;
            options.graph_seed = raw.graph_seed;
            options.last_as_prefix = raw.lexical_flags == 1;
            options.rescore = raw.rescore == 1;
            options.alpha = (raw.has_alpha == 1).then_some(raw.alpha);
            options.rules_enabled = raw.rules_enabled == 1;
            options.max_rounds = (raw.has_max_rounds == 1).then_some(raw.max_rounds);
        }
        let request = match search.kind {
            0 if text.is_none() => SearchRequest::Vector {
                vector: vector.ok_or_else(|| invalid("vector expression required"))?,
                k: ExprId(search.k),
                mode,
                eligible,
                options,
            },
            1 if vector.is_none() && search.has_tier == 0 => SearchRequest::Text {
                query: text.ok_or_else(|| invalid("text expression required"))?,
                k: ExprId(search.k),
                eligible,
                options,
            },
            2 => SearchRequest::Hybrid {
                vector: vector.ok_or_else(|| invalid("vector expression required"))?,
                text: text.ok_or_else(|| invalid("text expression required"))?,
                k: ExprId(search.k),
                mode,
                eligible,
                options,
            },
            _ => return Err(invalid("search kind/arguments")),
        };
        let vd = optional(search.vector_distance_slot)?.map(SlotId);
        let ls = optional(search.lexical_score_slot)?.map(SlotId);
        if search.kind != 2 && (vd.is_some() || ls.is_some()) {
            return Err(invalid("inactive component yields"));
        }
        search_requests.push((
            SearchCallId(search.call_id),
            request,
            SearchOutputs {
                node: Some(SlotId(search.node_slot)),
                distance: (search.kind == 0).then_some(SlotId(search.score_slot)),
                score: (search.kind != 0).then_some(SlotId(search.score_slot)),
                vector_distance: vd,
                lexical_score: ls,
            },
        ));
    }
    let mut operators = Vec::new();
    for (o, names) in raw_ops.iter().zip(&types) {
        o.validate_header()
            .map_err(|_| invalid("operator header"))?;
        if o.reserved != 0 || o.has_name > 1 || o.has_limit > 1 {
            return Err(invalid("operator flags"));
        }
        let mut inactive = *o;
        match o.kind {
            0 => {}
            1 => {
                inactive.predicate = ZeGraphOptionalIndex {
                    present: 0,
                    index: 0,
                };
            }
            2 => {}
            3 => {
                inactive.sort_keys = ZeGraphRange { start: 0, count: 0 };
            }
            4 => {
                inactive.node_slot = 0;
                inactive.has_name = 0;
                inactive.name = ZeGraphRange { start: 0, count: 0 };
            }
            5 => {}
            6 => {
                inactive.mutations = ZeGraphRange { start: 0, count: 0 };
            }
            7 => {
                inactive.projections = ZeGraphRange { start: 0, count: 0 };
                inactive.aggregates = ZeGraphRange { start: 0, count: 0 };
            }
            8 => {
                inactive.search = 0;
            }
            9 => {
                inactive.offset = 0;
                inactive.limit = 0;
                inactive.has_limit = 0;
            }
            10 => {
                inactive.node_slot = 0;
                inactive.node_id = ZeNodeId { high: 0, low: 0 };
            }
            11 => {
                inactive.relationship_slot = 0;
                inactive.relationship_id = ZeRelId { high: 0, low: 0 };
            }
            12 => {
                inactive.node_slot = 0;
                inactive.entity_kind = 0;
                inactive.name = ZeGraphRange { start: 0, count: 0 };
                inactive.has_name = 0;
                inactive.key_expression = 0;
            }
            13 => {
                inactive.source_slot = 0;
                inactive.node_slot = 0;
                inactive.relationship_slot = 0;
                inactive.direction = 0;
                inactive.pattern = 0;
                inactive.relationship_types = ZeGraphRange { start: 0, count: 0 };
            }
            14 => {
                inactive.source_slot = 0;
                inactive.node_slot = 0;
                inactive.relationship_slot = 0;
                inactive.direction = 0;
                inactive.pattern = 0;
                inactive.relationship_types = ZeGraphRange { start: 0, count: 0 };
                inactive.path_min = 0;
                inactive.path_max = 0;
                inactive.edge_predicate = ZeGraphOptionalIndex {
                    present: 0,
                    index: 0,
                };
                inactive.edge_slot = 0;
            }
            15 => {
                inactive.predicate = ZeGraphOptionalIndex {
                    present: 0,
                    index: 0,
                };
            }
            16 => {
                inactive.projections = ZeGraphRange { start: 0, count: 0 };
            }
            17 => {
                inactive.projections = ZeGraphRange { start: 0, count: 0 };
            }
            18 => {
                inactive.predicate = ZeGraphOptionalIndex {
                    present: 0,
                    index: 0,
                };
            }
            19 => {}
            20 => {
                inactive.source_slot = 0;
                inactive.set_slot = 0;
            }
            _ => return Err(invalid("operator kind")),
        }
        if inactive.entity_kind != 0
            || inactive.predicate.present != 0
            || inactive.predicate.index != 0
            || inactive.source_slot != 0
            || inactive.node_slot != 0
            || inactive.relationship_slot != 0
            || inactive.set_slot != 0
            || inactive.direction != 0
            || inactive.pattern != 0
            || inactive.name.start != 0
            || inactive.name.count != 0
            || inactive.has_name != 0
            || inactive.key_expression != 0
            || inactive.node_id.high != 0
            || inactive.node_id.low != 0
            || inactive.relationship_id.high != 0
            || inactive.relationship_id.low != 0
            || inactive.relationship_types.start != 0
            || inactive.relationship_types.count != 0
            || inactive.path_min != 0
            || inactive.path_max != 0
            || inactive.edge_predicate.present != 0
            || inactive.edge_predicate.index != 0
            || inactive.edge_slot != 0
            || inactive.search != 0
            || inactive.projections.start != 0
            || inactive.projections.count != 0
            || inactive.aggregates.start != 0
            || inactive.aggregates.count != 0
            || inactive.sort_keys.start != 0
            || inactive.sort_keys.count != 0
            || inactive.mutations.start != 0
            || inactive.mutations.count != 0
            || inactive.offset != 0
            || inactive.limit != 0
            || inactive.has_limit != 0
        {
            return Err(invalid("inactive operator fields"));
        }
        let predicate = optional(o.predicate)?.map(ExprId);
        if (o.kind == 4 && o.has_name == 0 && (o.name.start != 0 || o.name.count != 0))
            || (o.kind == 12 && o.has_name != 1)
            || (o.kind == 9 && o.has_limit == 0 && o.limit != 0)
            || (o.kind == 14 && o.edge_predicate.present == 0 && o.edge_slot != 0)
        {
            return Err(invalid("inactive optional operator fields"));
        }
        let direction = match o.direction {
            0 => Direction::Outgoing,
            1 => Direction::Incoming,
            2 => Direction::Either,
            _ => return Err(invalid("direction")),
        };
        let kind = match o.kind {
            0 => OperatorKind::Unit,
            1 => OperatorKind::Join { predicate },
            2 => OperatorKind::Distinct,
            3 => OperatorKind::Sort(span(&sorts, o.sort_keys)?),
            4 => OperatorKind::ScanNodes {
                output: SlotId(o.node_slot),
                label: if o.has_name == 1 {
                    Some(name(&pool, o.name)?)
                } else {
                    None
                },
            },
            5 => OperatorKind::Eager,
            6 => OperatorKind::Mutate(span(&mutations, o.mutations)?),
            7 => OperatorKind::Aggregate {
                keys: span(&projections, o.projections)?,
                aggregates: span(&projections, o.aggregates)?,
            },
            8 => {
                let (call, request, outputs) = *search_requests
                    .get(o.search as usize)
                    .ok_or_else(|| invalid("search index"))?;
                OperatorKind::Search {
                    call,
                    request,
                    outputs,
                }
            }
            9 => OperatorKind::OffsetLimit {
                offset: o.offset,
                limit: (o.has_limit == 1).then_some(o.limit),
            },
            10 => OperatorKind::LookupNode {
                output: SlotId(o.node_slot),
                id: o.node_id.try_into().map_err(|_| invalid("node identity"))?,
            },
            11 => OperatorKind::LookupRelationship {
                output: SlotId(o.relationship_slot),
                id: o
                    .relationship_id
                    .try_into()
                    .map_err(|_| invalid("relationship identity"))?,
            },
            12 => OperatorKind::LookupKey {
                output: SlotId(o.node_slot),
                namespace: name(&pool, o.name)?,
                key: ExprId(o.key_expression),
                kind: match o.entity_kind {
                    0 => EntityKind::Node,
                    1 => EntityKind::Relationship,
                    _ => return Err(invalid("entity kind")),
                },
            },
            13 => OperatorKind::Expand {
                source: SlotId(o.source_slot),
                node: SlotId(o.node_slot),
                relationship: SlotId(o.relationship_slot),
                direction,
                relationship_types: names,
                pattern: PatternId(o.pattern),
            },
            14 => OperatorKind::BoundedExpand {
                source: SlotId(o.source_slot),
                node: SlotId(o.node_slot),
                relationships: SlotId(o.relationship_slot),
                direction,
                relationship_types: names,
                pattern: PatternId(o.pattern),
                min: u8::try_from(o.path_min).map_err(|_| invalid("path minimum"))?,
                max: u8::try_from(o.path_max).map_err(|_| invalid("path maximum"))?,
                edge_predicate: optional(o.edge_predicate)?.map(|index| EdgePredicate {
                    current_edge: SlotId(o.edge_slot),
                    expression: ExprId(index),
                }),
                completed_edge_predicate: None,
            },
            15 => OperatorKind::OptionalApply { predicate },
            16 => OperatorKind::Project(span(&projections, o.projections)?),
            17 => OperatorKind::With(span(&projections, o.projections)?),
            18 => {
                OperatorKind::Filter(predicate.ok_or_else(|| invalid("filter requires predicate"))?)
            }
            19 => OperatorKind::Collect,
            20 => OperatorKind::Aggregate {
                keys: &[],
                aggregates: eligible_groups
                    .iter()
                    .find(|(slot, _)| *slot == o.set_slot)
                    .map(|(_, projections)| projections.as_slice())
                    .ok_or_else(|| invalid("eligible set backing"))?,
            },
            _ => return Err(invalid("unknown operator kind")),
        };
        operators.push(Operator {
            inputs: span(&inputs, o.inputs)?,
            kind,
        });
    }
    let mut backing = super::values::retain(backing, &bytes)?;
    macro_rules! retain {
        ($value:expr) => {
            backing.vec($value).map_err(|e| store_error(&e, false))?
        };
    }
    retain!(&inputs);
    retain!(&children);
    retain!(&projections);
    retain!(&sorts);
    retain!(&mutations);
    retain!(&parameters);
    retain!(&raw_ops);
    retain!(&raw_exprs);
    retain!(&raw_projections);
    retain!(&raw_sorts);
    retain!(&raw_mutations);
    retain!(&raw_parameters);
    retain!(&raw_searches);
    retain!(&types);
    retain!(&label_groups);
    retain!(&eligible_groups);
    retain!(&eligible_expressions);
    retain!(&search_requests);
    for (_, group) in &eligible_groups {
        retain!(group);
    }
    for names in types.iter().chain(&label_groups) {
        retain!(names);
    }
    // Native names and scalar strings borrow only this copied byte owner.
    // Pool::read currently borrows the C byte pool; copy that owner before decoding.
    let writes = raw_ops.iter().any(|o| o.kind == 6);
    run(
        &GraphQueryPlan {
            operators: &operators,
            expressions: &expressions,
            parameters: &parameters,
            eager_searches: &eager,
            root: PlanNodeId(raw.root),
            backing: &backing,
            bindings,
            columns: &[],
        },
        writes,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[allow(clippy::unwrap_used)]
    fn ze241_raw_plan_rejects_inactive_fields_and_honest_bad_ranges() {
        let mut operator: ZeGraphOperator = unsafe { std::mem::zeroed() };
        operator.abi_size = std::mem::size_of::<ZeGraphOperator>() as u32;
        let mut pool: ZeGraphValuePool = unsafe { std::mem::zeroed() };
        pool.abi_size = std::mem::size_of::<ZeGraphValuePool>() as u32;
        let mut plan: ZeGraphPlan = unsafe { std::mem::zeroed() };
        plan.abi_size = std::mem::size_of::<ZeGraphPlan>() as u32;
        plan.pool = &pool;
        plan.operators = &operator;
        plan.operator_count = 1;
        with_plan(&plan, &[], GraphPlanBacking::default(), |_, writes| {
            assert!(!writes);
            Ok(())
        })
        .unwrap();
        operator.node_slot = 17;
        plan.operators = &operator;
        assert!(with_plan(&plan, &[], GraphPlanBacking::default(), |_, _| Ok(())).is_err());
        operator.node_slot = 0;
        operator.inputs.count = 1;
        plan.operators = &operator;
        assert!(with_plan(&plan, &[], GraphPlanBacking::default(), |_, _| Ok(())).is_err());
        operator.inputs.count = 0;
        plan.operators = &operator;
        plan.operator_count = 4097;
        assert!(with_plan(&plan, &[], GraphPlanBacking::default(), |_, _| Ok(())).is_err());
    }
}
