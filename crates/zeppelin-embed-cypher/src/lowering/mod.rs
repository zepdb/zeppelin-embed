//! Complete symbolic read preparation. No graph admission or execution occurs.
use crate::*;
use std::mem::size_of;
use zeppelin_embed::lifecycle::{QueryControl, QueryError as ControlError};
use zeppelin_embed::property_graph::{
    GraphName,
    query::{ValueContext, plan::*, resources::*},
};
mod context;
mod expression;
mod owned;
mod parameters;
mod pattern;
mod projection;
pub use context::{PreparationControl, ReadContext};
use owned::*;
use parameters::ParameterValue;

/// Immutable output metadata, copied independently of compiler/caller storage.
#[derive(Clone, Copy, Debug)]
pub struct ReadColumn<'a> {
    pub name: &'a str,
    pub slot: SlotId,
    pub kinds: ValueKinds,
}

/// Scoped complete native plan and its source/parameter ownership. The HRTB
/// consumer prevents any of these borrowed plan/fact owners from escaping.
pub struct LoweredRead<'plan, 'facts> {
    plan: GraphPlan<'plan, 'facts>,
    columns: &'plan [ReadColumn<'plan>],
    source: &'plan str,
    expression_spans: &'plan [Span],
    operator_spans: &'plan [Span],
    parameters: &'plan [ParameterBinding<'plan>],
    owners: &'plan [RetainedAllocation<'plan>],
}
impl<'p, 'f> LoweredRead<'p, 'f> {
    pub fn plan(&self) -> &GraphPlan<'p, 'f> {
        &self.plan
    }
    pub fn columns(&self) -> &[ReadColumn<'p>] {
        self.columns
    }
    pub fn source(&self) -> &'p str {
        self.source
    }
    pub fn expression_spans(&self) -> &'p [Span] {
        self.expression_spans
    }
    pub fn operator_spans(&self) -> &'p [Span] {
        self.operator_spans
    }
    pub fn parameters(&self) -> &'p [ParameterBinding<'p>] {
        self.parameters
    }
    /// Complete actual plan and fact arena capabilities under the same query.
    /// No caller numeric span, logical length or prepayment proves ownership.
    pub fn owners(&self) -> &[RetainedAllocation<'p>] {
        self.owners
    }
}

/// One-shot read lowerer, never a public reusable prepared query. The provided
/// ValueContext supplies the very same caller control used by compile_in.
/// CALL belongs to search composition (ZE-58), and rejects explicitly here.
pub fn compile_read_in<'v, T, C: ReadContext<'v>>(
    source: &str,
    parameters: &[ParameterBinding<'_>],
    limits: CompileLimits,
    memory: &QueryMemory<'_>,
    context: &mut C,
    consume: impl for<'plan, 'facts> FnOnce(LoweredRead<'plan, 'facts>, &mut C) -> Result<T, ParseError>,
) -> Result<T, ParseError> {
    if !context.matches_memory(memory) {
        return Err(memory_error(MemoryError::UnprovedInput));
    }
    let preparation = context.preparation_control();
    let control = &|| preparation.checkpoint();
    crate::shared_resources::compile_checked_in(
        source,
        parameters,
        limits,
        memory,
        control,
        |bound| {
            let mut lowering_control = memory.reserve_external_capacity().map_err(memory_error)?;
            lowering_control
                .reserve_additional(
                    65536
                        + VALIDATION_SCRATCH_BYTES
                        + size_of::<Builder<'_, '_, '_>>()
                        + size_of::<PreparationControl<'_>>()
                        + size_of::<PlanDescription<'_>>(),
                )
                .map_err(memory_error)?;
            let mut builder = Builder::new(memory, control)?;
            let source = builder.copy_text(bound.syntax().source())?;
            for _ in bound.expressions() {
                builder.remap.push(None, memory, control)?;
            }
            let root = bound
                .syntax()
                .node(bound.syntax().root())
                .ok_or_else(|| invariant(Span::default(), "statement"))?;
            for id in root.children() {
                let node = bound
                    .syntax()
                    .node(*id)
                    .ok_or_else(|| invariant(root.span, "clause"))?;
                if matches!(node.kind, NodeKind::Call(_)) {
                    return Err(ParseError::new(
                        ErrorKind::SearchContext,
                        node.span,
                        "CALL requires search lowering (ZE-58)",
                    ));
                }
                if !matches!(
                    node.kind,
                    NodeKind::Projection { .. } | NodeKind::Match { .. }
                ) {
                    return Err(ParseError::new(
                        ErrorKind::Unsupported,
                        node.span,
                        "read lowering clause",
                    ));
                }
            }
            builder.copy_parameters(bound.parameters())?;
            for expression in bound.expressions() {
                if let Expression::Slot(slot) = expression {
                    builder.next_slot = builder
                        .next_slot
                        .max(slot.0.checked_add(1).ok_or_else(|| limit(root.span))?);
                }
            }
            for projection in bound.projections() {
                for column in projection.columns() {
                    builder.next_slot = builder.next_slot.max(
                        column
                            .slot
                            .0
                            .checked_add(1)
                            .ok_or_else(|| limit(root.span))?,
                    );
                }
            }
            let mut current = builder.operator(DraftOp::Unit, &[], root.span)?;
            for id in root.children() {
                let clause = syntax(&bound, *id)?;
                current = match clause.kind {
                    NodeKind::Match { optional } => {
                        builder.pattern(&bound, clause, current, optional)?
                    }
                    NodeKind::Projection { .. } => builder.projection(&bound, *id, current)?,
                    _ => return Err(invariant(clause.span, "unexpected read clause")),
                };
            }
            builder.finish(&bound, current, source, memory, context, consume)
        },
    )
}

#[derive(Clone, Copy)]
enum DraftExpr {
    Literal(Literal<'static>),
    String(Range),
    Slot(SlotId),
    Parameter(ParameterId),
    List(Range),
    Property {
        entity: ExprId,
        name: Range,
    },
    HasLabel {
        entity: ExprId,
        label: Range,
    },
    Binary {
        operation: BinaryExpression,
        left: ExprId,
        right: ExprId,
    },
    Unary {
        operation: UnaryExpression,
        operand: ExprId,
    },
    Aggregate {
        operation: AggregateExpression,
        operand: Option<ExprId>,
    },
}
#[derive(Clone, Copy)]
enum DraftOp {
    Unit,
    Project(Range),
    With(Range),
    Filter(ExprId),
    Scan(SlotId),
    Aggregate {
        keys: Range,
        aggregates: Range,
    },
    Distinct,
    Sort(Range),
    OffsetLimit {
        offset: u64,
        limit: Option<u64>,
    },
    Expand {
        source: SlotId,
        node: SlotId,
        relationship: SlotId,
        direction: zeppelin_embed::property_graph::query::plan::Direction,
        types: Range,
        pattern: PatternId,
    },
    Bounded {
        source: SlotId,
        node: SlotId,
        relationships: SlotId,
        direction: zeppelin_embed::property_graph::query::plan::Direction,
        types: Range,
        pattern: PatternId,
        min: u8,
        max: u8,
        edge_predicate: Option<EdgePredicate>,
        completed_edge_predicate: Option<CompletedEdgePredicate>,
    },
    Optional(Option<ExprId>),
}
#[derive(Clone, Copy)]
struct DraftOperator {
    kind: DraftOp,
    inputs: Range,
}
struct Builder<'m, 'g, 'c> {
    memory: &'m QueryMemory<'g>,
    control: &'c dyn Fn() -> Result<(), ResourceError>,
    parameter_levels: [Option<Buffer<'m, 'g, ParameterValue>>; 17],
    parameter_names: Buffer<'m, 'g, Range>,
    next_slot: u32,
    pattern_id: u32,
    scope: Buffer<'m, 'g, SlotId>,
    names: Buffer<'m, 'g, Range>,
    remap: Buffer<'m, 'g, Option<ExprId>>,
    children: Buffer<'m, 'g, ExprId>,
    bytes: Buffer<'m, 'g, u8>,
    expressions: Buffer<'m, 'g, DraftExpr>,
    expression_spans: Buffer<'m, 'g, Span>,
    operators: Buffer<'m, 'g, DraftOperator>,
    operator_spans: Buffer<'m, 'g, Span>,
    sort_keys: Buffer<'m, 'g, SortKey>,
    inputs: Buffer<'m, 'g, PlanNodeId>,
    projections: Buffer<'m, 'g, Projection>,
}
impl<'m, 'g, 'c> Builder<'m, 'g, 'c> {
    fn new(
        memory: &'m QueryMemory<'g>,
        control: &'c dyn Fn() -> Result<(), ResourceError>,
    ) -> Result<Self, ParseError> {
        Ok(Self {
            memory,
            control,
            parameter_levels: std::array::from_fn(|_| None),
            parameter_names: Buffer::new(memory)?,
            next_slot: 0,
            pattern_id: 0,
            scope: Buffer::new(memory)?,
            names: Buffer::new(memory)?,
            remap: Buffer::new(memory)?,
            children: Buffer::new(memory)?,
            bytes: Buffer::new(memory)?,
            expressions: Buffer::new(memory)?,
            expression_spans: Buffer::new(memory)?,
            operators: Buffer::new(memory)?,
            operator_spans: Buffer::new(memory)?,
            sort_keys: Buffer::new(memory)?,
            inputs: Buffer::new(memory)?,
            projections: Buffer::new(memory)?,
        })
    }
    fn copy_text(&mut self, value: &str) -> Result<Range, ParseError> {
        let range = Range {
            start: self.bytes.len(),
            len: value.len(),
        };
        for byte in value.bytes() {
            self.bytes.push(byte, self.memory, self.control)?;
        }
        Ok(range)
    }
    fn expression(&mut self, kind: DraftExpr, span: Span) -> Result<ExprId, ParseError> {
        if self.expressions.len() >= MAX_PLAN_NODES {
            return Err(limit(span));
        }
        let id = ExprId(self.expressions.len() as u32);
        self.expressions.push(kind, self.memory, self.control)?;
        self.expression_spans
            .push(span, self.memory, self.control)?;
        Ok(id)
    }
    fn operator(
        &mut self,
        kind: DraftOp,
        inputs: &[PlanNodeId],
        span: Span,
    ) -> Result<PlanNodeId, ParseError> {
        if self.operators.len() >= MAX_PLAN_NODES {
            return Err(limit(span));
        }
        let id = PlanNodeId(self.operators.len() as u32);
        let range = Range {
            start: self.inputs.len(),
            len: inputs.len(),
        };
        for input in inputs {
            self.inputs.push(*input, self.memory, self.control)?;
        }
        self.operators.push(
            DraftOperator {
                kind,
                inputs: range,
            },
            self.memory,
            self.control,
        )?;
        self.operator_spans.push(span, self.memory, self.control)?;
        Ok(id)
    }
    fn finish<'v, T, C: ReadContext<'v>>(
        mut self,
        bound: &BoundQuery<'_>,
        root: PlanNodeId,
        source: Range,
        memory: &QueryMemory<'_>,
        context: &mut C,
        consume: impl for<'plan, 'facts> FnOnce(
            LoweredRead<'plan, 'facts>,
            &mut C,
        ) -> Result<T, ParseError>,
    ) -> Result<T, ParseError> {
        let mut control_charge = memory.reserve_external_capacity().map_err(memory_error)?;
        control_charge
            .reserve_additional(
                size_of::<[RetainedAllocation<'_>; 32]>() + size_of::<LoweredRead<'_, '_>>(),
            )
            .map_err(memory_error)?;
        let mut names = QueryArena::new(memory, bound.columns().len()).map_err(memory_error)?;
        for column in bound.columns() {
            names
                .push(self.copy_text(column.name)?)
                .map_err(memory_error)?;
        }
        // All byte copies finish before any final borrowed value is created.
        let level16 = self.freeze_level(16, &[], context.value_context())?;
        let level15 = self.freeze_level(15, level16.as_slice(), context.value_context())?;
        let level14 = self.freeze_level(14, level15.as_slice(), context.value_context())?;
        let level13 = self.freeze_level(13, level14.as_slice(), context.value_context())?;
        let level12 = self.freeze_level(12, level13.as_slice(), context.value_context())?;
        let level11 = self.freeze_level(11, level12.as_slice(), context.value_context())?;
        let level10 = self.freeze_level(10, level11.as_slice(), context.value_context())?;
        let level9 = self.freeze_level(9, level10.as_slice(), context.value_context())?;
        let level8 = self.freeze_level(8, level9.as_slice(), context.value_context())?;
        let level7 = self.freeze_level(7, level8.as_slice(), context.value_context())?;
        let level6 = self.freeze_level(6, level7.as_slice(), context.value_context())?;
        let level5 = self.freeze_level(5, level6.as_slice(), context.value_context())?;
        let level4 = self.freeze_level(4, level5.as_slice(), context.value_context())?;
        let level3 = self.freeze_level(3, level4.as_slice(), context.value_context())?;
        let level2 = self.freeze_level(2, level3.as_slice(), context.value_context())?;
        let level1 = self.freeze_level(1, level2.as_slice(), context.value_context())?;
        let level0 = self.freeze_level(0, level1.as_slice(), context.value_context())?;
        let mut parameter_bindings =
            QueryArena::new(memory, bound.parameters().len()).map_err(memory_error)?;
        let mut parameter_declarations =
            QueryArena::new(memory, bound.parameters().len()).map_err(memory_error)?;
        for (range, value) in self.parameter_names.slice().iter().zip(level0.as_slice()) {
            let name = text(self.bytes.slice(), *range, self.control)?;
            parameter_bindings
                .push(ParameterBinding {
                    name,
                    value: *value,
                })
                .map_err(memory_error)?;
            parameter_declarations
                .push(Parameter {
                    name,
                    kinds: parameters::kind(*value),
                })
                .map_err(memory_error)?;
        }
        let mut columns = QueryArena::new(memory, names.len()).map_err(memory_error)?;
        for (column, name) in bound.columns().iter().zip(names.as_slice()) {
            columns
                .push(ReadColumn {
                    name: text(self.bytes.slice(), *name, self.control)?,
                    slot: column.slot,
                    kinds: column.kinds,
                })
                .map_err(memory_error)?;
        }
        let mut expressions =
            QueryArena::new(memory, self.expressions.len()).map_err(memory_error)?;
        for expression in self.expressions.slice() {
            check(self.control, Span::default())?;
            expressions
                .push(match expression {
                    DraftExpr::Literal(v) => Expression::Literal(*v),
                    DraftExpr::String(range) => Expression::Literal(Literal::String(text(
                        self.bytes.slice(),
                        *range,
                        self.control,
                    )?)),
                    DraftExpr::Slot(slot) => Expression::Slot(*slot),
                    DraftExpr::Parameter(id) => Expression::Parameter(*id),
                    DraftExpr::List(range) => Expression::List(range.get(self.children.slice())?),
                    DraftExpr::Property { entity, name } => Expression::Property {
                        entity: *entity,
                        name: GraphName::new(text(self.bytes.slice(), *name, self.control)?)
                            .map_err(|_| invariant(Span::default(), "property name"))?,
                    },
                    DraftExpr::HasLabel { entity, label } => Expression::HasLabel {
                        entity: *entity,
                        label: GraphName::new(text(self.bytes.slice(), *label, self.control)?)
                            .map_err(|_| invariant(Span::default(), "label name"))?,
                    },
                    DraftExpr::Binary {
                        operation,
                        left,
                        right,
                    } => Expression::Binary {
                        operation: *operation,
                        left: *left,
                        right: *right,
                    },
                    DraftExpr::Unary { operation, operand } => Expression::Unary {
                        operation: *operation,
                        operand: *operand,
                    },
                    DraftExpr::Aggregate { operation, operand } => Expression::Aggregate {
                        operation: *operation,
                        operand: *operand,
                    },
                })
                .map_err(memory_error)?;
        }
        let mut types = QueryArena::new(memory, self.names.len()).map_err(memory_error)?;
        for name in self.names.slice() {
            types
                .push(
                    GraphName::new(text(self.bytes.slice(), *name, self.control)?)
                        .map_err(|_| invariant(Span::default(), "relationship type"))?,
                )
                .map_err(memory_error)?;
        }
        let mut operators = QueryArena::new(memory, self.operators.len()).map_err(memory_error)?;
        for operator in self.operators.slice() {
            check(self.control, Span::default())?;
            operators
                .push(Operator {
                    inputs: operator.inputs.get(self.inputs.slice())?,
                    kind: match operator.kind {
                        DraftOp::Unit => OperatorKind::Unit,
                        DraftOp::Project(range) => {
                            OperatorKind::Project(range.get(self.projections.slice())?)
                        }
                        DraftOp::With(range) => {
                            OperatorKind::With(range.get(self.projections.slice())?)
                        }
                        DraftOp::Aggregate { keys, aggregates } => OperatorKind::Aggregate {
                            keys: keys.get(self.projections.slice())?,
                            aggregates: aggregates.get(self.projections.slice())?,
                        },
                        DraftOp::Distinct => OperatorKind::Distinct,
                        DraftOp::Sort(range) => {
                            OperatorKind::Sort(range.get(self.sort_keys.slice())?)
                        }
                        DraftOp::OffsetLimit { offset, limit } => {
                            OperatorKind::OffsetLimit { offset, limit }
                        }
                        DraftOp::Filter(predicate) => OperatorKind::Filter(predicate),
                        DraftOp::Scan(output) => OperatorKind::ScanNodes {
                            output,
                            label: None,
                        },
                        DraftOp::Expand {
                            source,
                            node,
                            relationship,
                            direction,
                            types: alternatives,
                            pattern,
                        } => OperatorKind::Expand {
                            source,
                            node,
                            relationship,
                            direction,
                            relationship_types: alternatives.get(types.as_slice())?,
                            pattern,
                        },
                        DraftOp::Bounded {
                            source,
                            node,
                            relationships,
                            direction,
                            types: alternatives,
                            pattern,
                            min,
                            max,
                            edge_predicate,
                            completed_edge_predicate,
                        } => OperatorKind::BoundedExpand {
                            source,
                            node,
                            relationships,
                            direction,
                            relationship_types: alternatives.get(types.as_slice())?,
                            pattern,
                            min,
                            max,
                            edge_predicate,
                            completed_edge_predicate,
                        },
                        DraftOp::Optional(predicate) => OperatorKind::OptionalApply { predicate },
                    },
                })
                .map_err(memory_error)?;
        }
        let mut facts = QueryArena::new(memory, operators.len()).map_err(memory_error)?;
        for _ in 0..operators.len() {
            check(self.control, Span::default())?;
            facts.push(NodeFacts::default()).map_err(memory_error)?;
        }

        let mut regions = QueryArena::new(memory, 31).map_err(memory_error)?;
        for region in [
            region(&level0)?,
            region(&level1)?,
            region(&level2)?,
            region(&level3)?,
            region(&level4)?,
            region(&level5)?,
            region(&level6)?,
            region(&level7)?,
            region(&level8)?,
            region(&level9)?,
            region(&level10)?,
            region(&level11)?,
            region(&level12)?,
            region(&level13)?,
            region(&level14)?,
            region(&level15)?,
            region(&level16)?,
            region(&parameter_bindings)?,
            region(&parameter_declarations)?,
            region(&self.sort_keys.arena)?,
            region(&types)?,
            region(&self.children.arena)?,
            region(&self.bytes.arena)?,
            region(&self.inputs.arena)?,
            region(&self.projections.arena)?,
            region(&self.expression_spans.arena)?,
            region(&self.operator_spans.arena)?,
            region(&columns)?,
            region(&expressions)?,
            region(&operators)?,
            region(&facts)?,
        ] {
            if region.start() != region.end() {
                regions.push(region).map_err(memory_error)?;
            }
        }
        regions.as_mut_slice().sort_unstable();
        let description = PlanDescription {
            operators: operators.as_slice(),
            expressions: expressions.as_slice(),
            parameters: parameter_declarations.as_slice(),
            root,
            eager_searches: &[],
        };
        let (plan, facts_owner) = facts
            .validate_plan(
                description,
                PlanFootprint::declared(memory.reserved_bytes()),
                PlanBacking::new(regions.as_slice(), regions.heap_bytes()).map_err(plan_error)?,
                context.value_context(),
            )
            .map_err(memory_error)?;
        let owners = [
            facts_owner,
            RetainedAllocation::arena(&level0).map_err(memory_error)?,
            RetainedAllocation::arena(&level1).map_err(memory_error)?,
            RetainedAllocation::arena(&level2).map_err(memory_error)?,
            RetainedAllocation::arena(&level3).map_err(memory_error)?,
            RetainedAllocation::arena(&level4).map_err(memory_error)?,
            RetainedAllocation::arena(&level5).map_err(memory_error)?,
            RetainedAllocation::arena(&level6).map_err(memory_error)?,
            RetainedAllocation::arena(&level7).map_err(memory_error)?,
            RetainedAllocation::arena(&level8).map_err(memory_error)?,
            RetainedAllocation::arena(&level9).map_err(memory_error)?,
            RetainedAllocation::arena(&level10).map_err(memory_error)?,
            RetainedAllocation::arena(&level11).map_err(memory_error)?,
            RetainedAllocation::arena(&level12).map_err(memory_error)?,
            RetainedAllocation::arena(&level13).map_err(memory_error)?,
            RetainedAllocation::arena(&level14).map_err(memory_error)?,
            RetainedAllocation::arena(&level15).map_err(memory_error)?,
            RetainedAllocation::arena(&level16).map_err(memory_error)?,
            RetainedAllocation::arena(&parameter_bindings).map_err(memory_error)?,
            RetainedAllocation::arena(&parameter_declarations).map_err(memory_error)?,
            RetainedAllocation::arena(&self.sort_keys.arena).map_err(memory_error)?,
            RetainedAllocation::arena(&types).map_err(memory_error)?,
            RetainedAllocation::arena(&self.children.arena).map_err(memory_error)?,
            RetainedAllocation::arena(&self.bytes.arena).map_err(memory_error)?,
            RetainedAllocation::arena(&self.inputs.arena).map_err(memory_error)?,
            RetainedAllocation::arena(&self.projections.arena).map_err(memory_error)?,
            RetainedAllocation::arena(&self.expression_spans.arena).map_err(memory_error)?,
            RetainedAllocation::arena(&self.operator_spans.arena).map_err(memory_error)?,
            RetainedAllocation::arena(&columns).map_err(memory_error)?,
            RetainedAllocation::arena(&expressions).map_err(memory_error)?,
            RetainedAllocation::arena(&operators).map_err(memory_error)?,
            RetainedAllocation::arena(&names).map_err(memory_error)?,
        ];
        plan.validate_parameters(parameter_bindings.as_slice(), context.value_context())
            .map_err(plan_error)?;
        check(self.control, Span::default())?;
        let result = consume(
            LoweredRead {
                plan,
                source: text(self.bytes.slice(), source, self.control)?,
                columns: columns.as_slice(),
                expression_spans: self.expression_spans.slice(),
                operator_spans: self.operator_spans.slice(),
                parameters: parameter_bindings.as_slice(),
                owners: &owners,
            },
            context,
        )?;
        check(self.control, Span::default())?;
        Ok(result)
    }
}
fn check(control: &dyn Fn() -> Result<(), ResourceError>, span: Span) -> Result<(), ParseError> {
    control()
        .map_err(|error| ParseError::new(ErrorKind::Resource(error), span, "read lowering control"))
}
fn invariant(span: Span, message: &'static str) -> ParseError {
    ParseError::new(ErrorKind::BindingInvariant, span, message)
}
fn limit(span: Span) -> ParseError {
    ParseError::new(
        ErrorKind::Limit(LimitKind::AstNodes),
        span,
        "native plan node limit",
    )
}
fn memory_error(error: MemoryError) -> ParseError {
    match error {
        MemoryError::Allocation => ParseError::new(
            ErrorKind::Resource(ResourceError::Allocation),
            Span::default(),
            "read lowering allocation",
        ),
        MemoryError::Value(error) => ParseError::new(
            ErrorKind::Resource(context::resource_error(error)),
            Span::default(),
            "read lowering control",
        ),
        MemoryError::Plan(error) => plan_error(error),
        _ => ParseError::new(
            ErrorKind::Resource(ResourceError::Memory),
            Span::default(),
            "read lowering reservation",
        ),
    }
}
fn plan_error(error: PlanError) -> ParseError {
    match error {
        PlanError::Limit | PlanError::PathBound => limit(Span::default()),
        PlanError::Control(error) => ParseError::new(
            ErrorKind::Resource(context::resource_error(error)),
            Span::default(),
            "plan control",
        ),
        _ => ParseError::new(
            ErrorKind::Plan(error),
            Span::default(),
            "generated read plan rejected",
        ),
    }
}

fn syntax<'a>(bound: &'a BoundQuery<'_>, id: AstId) -> Result<&'a Node, ParseError> {
    bound
        .syntax()
        .node(id)
        .ok_or_else(|| invariant(Span::default(), "syntax reference"))
}
fn child(node: &Node, index: usize) -> Result<AstId, ParseError> {
    node.children()
        .get(index)
        .copied()
        .ok_or_else(|| invariant(node.span, "syntax child"))
}
fn slot(bound: &BoundQuery<'_>, id: AstId) -> Result<SlotId, ParseError> {
    bound
        .fact(id)
        .and_then(|fact| fact.slot)
        .ok_or_else(|| invariant(Span::default(), "pattern binding"))
}
