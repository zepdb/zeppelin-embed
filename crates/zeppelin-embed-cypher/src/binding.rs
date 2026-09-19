//! Private one-shot compiler boundary; no reusable prepared query is returned.
use crate::resources::{poll, push};
use crate::*;
use zeppelin_embed::property_graph::query::{QueryValue, plan::*};
mod expressions;
mod mutation;
mod parameters;
mod patterns;
mod projection;
mod search;
pub use search::{
    BoundCall, BoundEligibility, BoundEligibilityProvenance, BoundSearchMode, BoundSearchRequest,
};

/// A column borrowed only during the private compiler callback.
#[derive(Clone, Copy, Debug)]
pub struct BoundColumn<'a> {
    pub name: &'a str,
    pub kinds: ValueKinds,
    pub expression: ExprId,
    pub slot: SlotId,
}
/// Bound facts keyed by the exact source AST identity.
#[derive(Clone, Copy, Debug)]
pub struct BoundFact {
    pub kinds: ValueKinds,
    pub slot: Option<SlotId>,
    pub row_dependent: bool,
    pub entity_origin: Option<SlotId>,
    pub global_distinct_nodes: bool,
}
/// Projection outputs retain declaration order and strong symbolic slots.
pub struct BoundProjection<'a> {
    syntax: AstId,
    columns: Vec<BoundColumn<'a>>,
}
impl<'a> BoundProjection<'a> {
    pub fn syntax(&self) -> AstId {
        self.syntax
    }
    pub fn columns(&self) -> &[BoundColumn<'a>] {
        &self.columns
    }
}
/// Ephemeral compiler view. It grants no graph admission or execution authority.
pub struct BoundQuery<'a> {
    ast: &'a Ast,
    columns: &'a [BoundColumn<'a>],
    expressions: &'a [Expression<'a>],
    calls: &'a [BoundCall],
    facts: &'a [Option<Info>],
    parameters: &'a [ParameterBinding<'a>],
    projections: &'a [BoundProjection<'a>],
    runtime_deleted_checks: bool,
}
impl<'a> BoundQuery<'a> {
    /// Later write lowering must retain dynamic deleted-access validation.
    pub fn requires_deleted_runtime_validation(&self) -> bool {
        self.runtime_deleted_checks
    }
    pub fn parameters(&self) -> &[ParameterBinding<'a>] {
        self.parameters
    }
    pub fn projections(&self) -> &[BoundProjection<'a>] {
        self.projections
    }
    pub fn fact(&self, id: AstId) -> Option<BoundFact> {
        let info = self.facts.get(id.0).copied().flatten()?;
        let slot = match self.expressions.get(id.0)? {
            Expression::Slot(slot) => Some(*slot),
            _ => None,
        };
        Some(BoundFact {
            kinds: info.kinds,
            slot,
            row_dependent: info.row,
            entity_origin: info.origin,
            global_distinct_nodes: info.eligible,
        })
    }
    pub fn calls(&self) -> &[BoundCall] {
        self.calls
    }
    pub fn syntax(&self) -> &Ast {
        self.ast
    }
    pub fn columns(&self) -> &[BoundColumn<'a>] {
        self.columns
    }
    /// Source-indexed scalar IR, including non-expression syntax holes.
    /// Lowering must select/remap reachable expressions and validate its complete
    /// operator plan; this arena alone is not an executable GraphPlan.
    pub fn expressions(&self) -> &[Expression<'a>] {
        self.expressions
    }
    /// Canonical row-independent backing for an expression, when one exists.
    pub fn invariant_expression(&self, id: ExprId) -> Option<ExprId> {
        let index = usize::try_from(id.0).ok()?;
        self.facts
            .get(index)
            .copied()
            .flatten()?
            .invariant
            .and_then(|id| u32::try_from(id.0).ok())
            .map(ExprId)
    }
}
#[derive(Clone, Copy)]
struct Info {
    kinds: ValueKinds,
    row: bool,
    origin: Option<SlotId>,
    eligible: bool,
    // Alias provenance for literals, parameters and list/collect sources.
    // A source may be row-dependent; this is not a constant-folding assertion.
    constant: Option<AstId>,
    // Canonical expression reconstructable without a row slot. Unlike constant,
    // this covers all accepted scalar operations and recursively substituted aliases.
    invariant: Option<AstId>,
}
#[derive(Clone, Copy)]
struct Symbol<'a> {
    name: &'a str,
    slot: SlotId,
    info: Info,
}
struct Binder<'a, 'r> {
    ast: &'a Ast,
    parameters: &'a [ParameterBinding<'a>],
    links: &'a [Vec<ExprId>],
    resources: &'r mut dyn Resources,
    limits: CompileLimits,
    expressions: Vec<Expression<'a>>,
    facts: Vec<Option<Info>>,
    scope: Vec<Symbol<'a>>,
    projections: Vec<BoundProjection<'a>>,
    deleted: Vec<SlotId>,
    has_deletions: bool,
    singleton: bool,
    runtime_deleted_checks: bool,
    calls: Vec<BoundCall>,
    next_slot: u32,
    order_aliases: Vec<(AstId, Symbol<'a>)>,
}
/// Complete parse/bind followed by one lifetime-scoped internal consumer.
/// The higher-ranked callback cannot return compiler-borrowed backing.
///
/// ```compile_fail
/// use zeppelin_embed_cypher::{compile_with, Budget, CompileLimits};
/// let escaped = compile_with("RETURN 1", &[], CompileLimits::default(),
///     &mut Budget::default(), |bound| Ok(bound));
/// ```
pub fn compile_with<T>(
    text: &str,
    parameters: &[ParameterBinding<'_>],
    limits: CompileLimits,
    resources: &mut dyn Resources,
    consume: impl for<'query> FnOnce(BoundQuery<'query>) -> Result<T, ParseError>,
) -> Result<T, ParseError> {
    let ast = parse_with(text, limits, resources)?;
    let mut links = Vec::new();
    for node in ast.nodes() {
        poll(resources, node.span)?;
        let mut children = Vec::new();
        for id in node.children() {
            push(&mut children, expr_id(*id)?, resources, node.span)?;
        }
        push(&mut links, children, resources, node.span)?;
    }
    let mut binder = Binder {
        ast: &ast,
        parameters,
        links: &links,
        resources,
        limits,
        expressions: Vec::new(),
        facts: Vec::new(),
        scope: Vec::new(),
        projections: Vec::new(),
        deleted: Vec::new(),
        has_deletions: false,
        singleton: true,
        runtime_deleted_checks: false,
        calls: Vec::new(),
        next_slot: 0,
        order_aliases: Vec::new(),
    };
    for node in ast.nodes() {
        push(
            &mut binder.expressions,
            Expression::Literal(Literal::Null),
            binder.resources,
            node.span,
        )?;
        push(&mut binder.facts, None, binder.resources, node.span)?;
    }
    binder.parameters()?;
    for clause in node(&ast, ast.root())?.children() {
        let clause_id = *clause;
        let clause = node(&ast, clause_id)?;
        match clause.kind {
            NodeKind::Projection { .. } => binder.projection(clause_id, clause)?,
            NodeKind::Match { optional } => binder.patterns(clause, optional, false)?,
            NodeKind::Create => binder.patterns(clause, false, true)?,
            NodeKind::Call(procedure) => binder.call(clause_id, procedure)?,
            NodeKind::Set | NodeKind::Remove | NodeKind::Delete { .. } => {
                binder.mutation(clause)?
            }
            _ => return Err(error(clause.span, "binding clause not implemented")),
        }
    }
    poll(binder.resources, node(&ast, ast.root())?.span)?;
    let columns = if node(&ast, ast.root())?
        .children()
        .last()
        .and_then(|id| ast.node(*id))
        .is_some_and(|node| matches!(node.kind, NodeKind::Projection { with: false, .. }))
    {
        binder
            .projections
            .last()
            .map_or(&[][..], |projection| projection.columns.as_slice())
    } else {
        &[]
    };
    let result = consume(BoundQuery {
        ast: &ast,
        columns,
        expressions: &binder.expressions,
        calls: &binder.calls,
        facts: &binder.facts,
        parameters,
        projections: &binder.projections,
        runtime_deleted_checks: binder.runtime_deleted_checks,
    })?;
    poll(binder.resources, node(&ast, ast.root())?.span)?;
    Ok(result)
}
impl<'a> Binder<'a, '_> {
    fn lookup(&mut self, name: &str, span: Span) -> Result<Symbol<'a>, ParseError> {
        if let Some(symbol) = self.lookup_optional(name, span)? {
            return Ok(symbol);
        }
        Err(ParseError::new(
            ErrorKind::UnknownVariable,
            span,
            "unknown or out-of-scope variable",
        ))
    }
    fn lookup_optional(
        &mut self,
        name: &str,
        span: Span,
    ) -> Result<Option<Symbol<'a>>, ParseError> {
        for symbol in &self.scope {
            poll(self.resources, span)?;
            if symbol.name == name {
                return Ok(Some(*symbol));
            }
        }
        Ok(None)
    }
    fn parameter(&mut self, name: &str, span: Span) -> Result<QueryValue<'a>, ParseError> {
        for parameter in self.parameters {
            poll(self.resources, span)?;
            if parameter.name == name {
                return Ok(parameter.value);
            }
        }
        Err(error(span, "missing checked parameter"))
    }
    fn new_slot(&mut self, span: Span) -> Result<SlotId, ParseError> {
        let slot = SlotId(self.next_slot);
        self.next_slot = self
            .next_slot
            .checked_add(1)
            .ok_or_else(|| error(span, "slot identity overflow"))?;
        Ok(slot)
    }
}
fn expr_id(id: AstId) -> Result<ExprId, ParseError> {
    u32::try_from(id.0)
        .map(ExprId)
        .map_err(|_| error(Span::default(), "expression identity overflow"))
}
fn node(ast: &Ast, id: AstId) -> Result<&Node, ParseError> {
    ast.node(id)
        .ok_or_else(|| error(Span::default(), "invalid syntax identity"))
}
fn child(node: &Node, index: usize) -> Result<AstId, ParseError> {
    node.children()
        .get(index)
        .copied()
        .ok_or_else(|| error(node.span, "missing expression"))
}
fn error(span: Span, message: &'static str) -> ParseError {
    ParseError::new(ErrorKind::BindingInvariant, span, message)
}
fn require(kinds: ValueKinds, allowed: ValueKinds, span: Span) -> Result<(), ParseError> {
    for kind in [
        ValueKinds::NULL,
        ValueKinds::BOOL,
        ValueKinds::I64,
        ValueKinds::F64,
        ValueKinds::STRING,
        ValueKinds::NODE,
        ValueKinds::REL,
        ValueKinds::LIST,
    ] {
        if kinds.contains(kind) && allowed.union(ValueKinds::NULL).contains(kind) {
            return Ok(());
        }
    }
    Err(ParseError::new(
        ErrorKind::Type,
        span,
        "expression has the wrong type",
    ))
}
