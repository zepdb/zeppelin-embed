//! Immutable borrowed typed plans. Validation allocates nothing and acquires no
//! store lease. The reservation owner attests actual retained capacities; this
//! component checks visible spans, not hidden Vec capacity or runtime budgets.
mod expression;
mod lineage;
mod mutation;
mod parameters;
mod search;
pub use search::{SearchBounds, SearchMode};
mod accounting;
mod validate;
use super::{Arithmetic, Comparison, QueryError, StringPredicate, ValueContext};
use crate::property_graph::{EntityKind, GraphName, NodeId, RelId};
pub use accounting::{PlanBacking, RetainedRegion};

/// Shared independent operator and expression arena limit.
pub const MAX_PLAN_NODES: usize = 4096;
/// Maximum dependency/expression nesting depth.
pub const MAX_PLAN_DEPTH: usize = 64;
/// Per-scope/output width, independent of the global slot identifier space.
pub const MAX_COLUMNS: usize = 256;
/// Conservative reserved stack envelope for bounded validator frames/scratch.
pub const VALIDATION_SCRATCH_BYTES: usize = 64 * 1024;

macro_rules! id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct $name(pub u32);
    };
}
id!(PlanNodeId, "Index into the operator arena.");
id!(ExprId, "Index into the expression arena.");
id!(SlotId, "Logical slot identity; it is not a row offset.");
id!(ParameterId, "Index into the declared parameter arena.");
id!(
    PatternId,
    "Relationship-uniqueness scope for one MATCH pattern."
);
id!(SearchCallId, "Source-order search call identity.");

/// Possible runtime kinds, including nullability, with no physical layout.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ValueKinds(u16);
impl ValueKinds {
    /// Null alone.
    pub const NULL: Self = Self(1);
    /// Boolean alone.
    pub const BOOL: Self = Self(2);
    /// Signed integer alone.
    pub const I64: Self = Self(4);
    /// IEEE binary64 alone.
    pub const F64: Self = Self(8);
    /// UTF-8 text alone.
    pub const STRING: Self = Self(16);
    /// Same-view node reference alone.
    pub const NODE: Self = Self(32);
    /// Same-view relationship reference alone.
    pub const REL: Self = Self(64);
    /// Validated query list alone.
    pub const LIST: Self = Self(128);
    /// Any supported runtime kind.
    pub const ANY: Self = Self(255);
    /// Union, including null when either input permits it.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
    /// Whether this set allows all kinds in the other set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
    pub(super) const fn overlaps(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }
}
/// Lease-free scalar literal; entity refs cannot be embedded in a reusable plan.
#[derive(Clone, Copy, Debug)]
pub enum Literal<'a> {
    /// Unknown value.
    Null,
    /// Boolean.
    Bool(bool),
    /// Exact signed integer.
    I64(i64),
    /// Exact IEEE bits, including nonfinite values.
    F64(f64),
    /// Borrowed immutable text, whose backing capacity must be charged.
    String(&'a str),
}
/// Structured expression, never a string DSL.
#[derive(Clone, Copy, Debug)]
pub enum Expression<'a> {
    /// Top-level aggregate output only; nested aggregates are rejected.
    Aggregate {
        /// Aggregate semantics.
        operation: AggregateExpression,
        /// None only for count(*).
        operand: Option<ExprId>,
    },
    /// Bounded list construction; value geometry is rechecked during execution.
    List(&'a [ExprId]),
    /// Static property access, yielding null for missing/null receivers.
    Property {
        /// Node or relationship expression.
        entity: ExprId,
        /// Symbolic property name resolved at admission.
        name: GraphName<'a>,
    },
    /// Static label predicate on a node or null receiver.
    HasLabel {
        /// Node expression.
        entity: ExprId,
        /// Symbolic label.
        label: GraphName<'a>,
    },
    /// Typed binary expression.
    Binary {
        /// Operation.
        operation: BinaryExpression,
        /// Left operand.
        left: ExprId,
        /// Right operand.
        right: ExprId,
    },
    /// Unary expression evaluated under the same input scope.
    Unary {
        /// Typed operation.
        operation: UnaryExpression,
        /// Input expression.
        operand: ExprId,
    },
    /// Scalar constant.
    Literal(Literal<'a>),
    /// Read a slot in the scope at this use site.
    Slot(SlotId),
    /// Read a declared runtime parameter.
    Parameter(ParameterId),
}
/// Typed unary operation; no frontend spelling enters core.
#[derive(Clone, Copy, Debug)]
pub enum UnaryExpression {
    /// Three-valued logical negation.
    Not,
    /// Checked numeric unary plus.
    Positive,
    /// Checked numeric negation.
    Negate,
    /// Exact null test.
    IsNull,
    /// Exact non-null test.
    IsNotNull,
    /// Unicode scalar/string or list element count.
    Size,
    /// Same-view label-name list.
    Labels,
    /// Same-view relationship type name.
    RelType,
    /// Explicit same-view/overlay optional source text; storage fetch is later-owned.
    StoredText,
    /// Full-width lowercase node identity text.
    NodeIdText,
    /// Full-width lowercase relationship identity text.
    RelIdText,
}
/// Typed binary value operations with runtime errors retained.
#[derive(Clone, Copy, Debug)]
pub enum BinaryExpression {
    /// Three-valued AND.
    And,
    /// Three-valued OR.
    Or,
    /// Three-valued XOR.
    Xor,
    /// Query comparison, distinct from total ordering and grouping.
    Comparison(Comparison),
    /// Checked numeric operation.
    Arithmetic(Arithmetic),
    /// Exact string predicate.
    String(StringPredicate),
    /// Three-valued membership in a query list.
    In,
    /// I64 list indexing, including negative indices.
    Index,
}
/// Supported aggregate forms, distinct from scalar functions.
#[derive(Clone, Copy, Debug)]
pub enum AggregateExpression {
    /// Count rows or nonnull values.
    Count {
        /// Group equivalence deduplication.
        distinct: bool,
    },
    /// Collect nonnull values under shared list/resource limits.
    Collect {
        /// Group equivalence deduplication.
        distinct: bool,
    },
}
/// Typed search source request. Dynamic request bounds are checked by the
/// later retrieval/runtime owner before invocation, never inferred from LIMIT.
#[derive(Clone, Copy, Debug)]
pub enum SearchRequest {
    /// Vector source; evaluated numeric list and view eligibility are retrieval-validated.
    Vector {
        /// Numeric vector expression.
        vector: ExprId,
        /// Positive I64, at most 4096.
        k: ExprId,
        /// Explicit search mode.
        mode: SearchMode,
        /// Optional same-view node-list domain.
        eligible: Option<ExprId>,
    },
    /// Hybrid source, retaining independent vector/text interpretation.
    Hybrid {
        /// Numeric vector expression.
        vector: ExprId,
        /// String query expression.
        text: ExprId,
        /// Positive I64, at most 4096.
        k: ExprId,
        /// Explicit search mode.
        mode: SearchMode,
        /// Optional same-view node-list domain.
        eligible: Option<ExprId>,
    },
    /// Exact lexical query data and optional same-view node-list eligibility.
    Text {
        /// String query expression.
        query: ExprId,
        /// Positive I64, at most 4096.
        k: ExprId,
        /// Optional list from the singleton input.
        eligible: Option<ExprId>,
    },
}
/// Exact parameter name and borrowed runtime value.
#[derive(Clone, Copy, Debug)]
pub struct ParameterBinding<'a> {
    /// Exact declared name.
    pub name: &'a str,
    /// Nonentity value; retained ownership stays with the caller.
    pub value: super::QueryValue<'a>,
}
/// One projected cell in a new row scope.
#[derive(Clone, Copy, Debug)]
pub struct Projection {
    /// Logical destination.
    pub slot: SlotId,
    /// Expression in the input scope.
    pub expression: ExprId,
}
/// Parameter name and permitted top-level value kinds.
#[derive(Clone, Copy, Debug)]
pub struct Parameter<'a> {
    /// Exact symbolic name.
    pub name: &'a str,
    /// Nonentity kinds; nested entities are also rejected at binding.
    pub kinds: ValueKinds,
}
/// Semantic operator, with arity independently checked by the validator.
#[derive(Clone, Copy, Debug)]
pub enum OperatorKind<'a> {
    /// One empty input row, no dependency.
    Unit,
    /// Shared slots are equality keys (null never joins); disjoint scopes form
    /// a cross product. The residual predicate sees the combined scope.
    Join {
        /// Optional join predicate.
        predicate: Option<ExprId>,
    },
    /// Equivalence-based whole-row deduplication; no output order promised.
    Distinct,
    /// Explicit stable ordering under the query total order.
    Sort(&'a [SortKey]),
    /// Base-view node scan with optional symbolic label.
    ScanNodes {
        /// New node slot.
        output: SlotId,
        /// Optional label constraint.
        label: Option<GraphName<'a>>,
    },
    /// Drain and freeze the complete upstream input before a mutation clause.
    Eager,
    /// Ordered mutation items; requires an immediate Eager input.
    Mutate(&'a [Mutation<'a>]),
    /// Global aggregation is exactly one row; grouped aggregation is not.
    Aggregate {
        /// Group key projections.
        keys: &'a [Projection],
        /// Top-level count/collect projections.
        aggregates: &'a [Projection],
    },
    /// Eager once-per-query source, never a per-row callback.
    Search {
        /// Textual/dependency order identity.
        call: SearchCallId,
        /// Structured search request.
        request: SearchRequest,
        /// Node result slot.
        node: SlotId,
        /// Distance for vector, score for text/hybrid. Component projection is retrieval-owned.
        score: SlotId,
    },
    /// Projection-stage bounds; never suppresses eager calls or mutations.
    OffsetLimit {
        /// Nonnegative skipped row count.
        offset: u64,
        /// Explicit optional row maximum.
        limit: Option<u64>,
    },
    /// Full-width structured identity lookup in the admitted base view.
    LookupNode {
        /// New node slot.
        output: SlotId,
        /// Full store-local identity.
        id: NodeId,
    },
    /// Distinct full-width relationship identity lookup.
    LookupRelationship {
        /// New relationship slot.
        output: SlotId,
        /// Full store-local relationship identity.
        id: RelId,
    },
    /// Kind/namespace-scoped application key, resolved in the admitted view.
    LookupKey {
        /// New entity binding.
        output: SlotId,
        /// Exact symbolic namespace.
        namespace: GraphName<'a>,
        /// String expression; runtime validates full key budget.
        key: ExprId,
        /// Distinct node/relationship domain.
        kind: EntityKind,
    },
    /// Single relationship expansion, retaining its original endpoints.
    Expand {
        /// Bound starting node.
        source: SlotId,
        /// New neighboring node slot.
        node: SlotId,
        /// New relationship slot.
        relationship: SlotId,
        /// Traversal direction.
        direction: Direction,
        /// Exact-name OR alternatives; empty means unrestricted. Duplicate names
        /// never multiply runtime rows. Resolution uses the same admitted catalog.
        relationship_types: &'a [GraphName<'a>],
        /// MATCH uniqueness scope.
        pattern: PatternId,
    },
    /// Finite expansion; the relationship binding is always a list.
    BoundedExpand {
        /// Bound starting node.
        source: SlotId,
        /// New neighboring node slot.
        node: SlotId,
        /// New relationship-list slot, including for one hop.
        relationships: SlotId,
        /// Predicate over each candidate edge before it enters a path. Null does
        /// not match; zero-hop paths evaluate no edges. The private slot never
        /// appears in this operator's output schema.
        edge_predicate: Option<EdgePredicate>,
        /// Inclusive lower bound.
        min: u8,
        /// Inclusive upper bound, at most 16.
        max: u8,
        /// Traversal direction.
        direction: Direction,
        /// Exact-name OR alternatives; empty means unrestricted. Duplicate names
        /// never multiply runtime rows. Resolution uses the same admitted catalog.
        relationship_types: &'a [GraphName<'a>],
        /// MATCH uniqueness scope.
        pattern: PatternId,
    },
    /// Correlated optional input, predicate applied before null extension.
    /// If the right sub-DAG contains the left input, execution substitutes each
    /// left row at that anchor. An independent right DAG instead matches shared
    /// slots by equality (null never matches), like Join. Shared bindings retain
    /// their left values; only right-only slots become null if the complete right
    /// candidate and attached predicate produce no match. A same-named scan
    /// alone is not an implicit correlated anchor.
    OptionalApply {
        /// Predicate in the combined scope.
        predicate: Option<ExprId>,
    },
    /// One input; exactly these output slots, preserving the input bag.
    Project(&'a [Projection]),
    /// Projection plus explicit scope barrier.
    With(&'a [Projection]),
    /// One input; only boolean/null predicate values are permitted.
    Filter(ExprId),
    /// One input; completed-result copying remains a later runtime responsibility.
    Collect,
}
/// Candidate-edge scope for bounded traversal, separate from its public list.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EdgePredicate {
    /// Fresh nonnullable relationship slot visible only while evaluating this
    /// predicate, alongside the traversal's input bindings. It must differ from
    /// every input slot and both new traversal output slots.
    pub current_edge: SlotId,
    /// Boolean/null expression in the private candidate-edge scope.
    pub expression: ExprId,
}
/// One ORDER BY key; ties remain ties unless a later key distinguishes them.
#[derive(Clone, Copy, Debug)]
pub struct SortKey {
    /// Expression in the current row scope.
    pub expression: ExprId,
    /// Reverse the total order.
    pub descending: bool,
}
/// Semantic boundaries an optimizer must preserve.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum Barrier {
    /// Optional null extension and its attached predicate.
    Optional = 1,
    /// Explicit WITH scope.
    Scope = 2,
    /// Grouping/global aggregation.
    Aggregate = 4,
    /// Equivalence deduplication.
    Distinct = 8,
    /// Explicit ORDER BY.
    Sort = 16,
    /// Projection-stage row bound.
    Limit = 32,
    /// Frozen mutation input.
    Eager = 64,
    /// Once-per-query source domain.
    Search = 128,
}
/// Structured mutation items; execution stages them privately in listed order.
#[derive(Clone, Copy, Debug)]
pub enum Mutation<'a> {
    /// Allocate a fresh node during execution; identities are never plan constants.
    CreateNode {
        /// New binding.
        output: SlotId,
        /// Exact symbolic label set, checked by staging.
        labels: &'a [GraphName<'a>],
    },
    /// Allocate a fresh relationship with preserved source/target orientation.
    CreateRelationship {
        /// New binding.
        output: SlotId,
        /// Source node expression.
        source: ExprId,
        /// Target node expression.
        target: ExprId,
        /// Exact relationship type.
        relationship_type: GraphName<'a>,
    },
    /// Remove one property.
    RemoveProperty {
        /// Node/relationship expression.
        entity: ExprId,
        /// Property name.
        name: GraphName<'a>,
    },
    /// Add/remove one node label in textual item order.
    SetLabel {
        /// Node expression.
        entity: ExprId,
        /// Exact label.
        label: GraphName<'a>,
        /// True adds, false removes.
        present: bool,
    },
    /// Delete an entity; DETACH is node-only, with atomic limits owned by staging.
    Delete {
        /// Entity expression.
        entity: ExprId,
        /// Include incident relationships for a node.
        detach: bool,
    },
    /// Null value removes a property; list conversion validates fully.
    SetProperty {
        /// Node/relationship expression in the progressive overlay.
        entity: ExprId,
        /// Exact property name.
        name: GraphName<'a>,
        /// RHS evaluated during mutation, not by the eager freeze.
        value: ExprId,
    },
}
/// Traversal direction does not rewrite stored endpoint identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    /// From stored source to target.
    Outgoing,
    /// From target to source.
    Incoming,
    /// Both, with runtime self-edge deduplication.
    Either,
}
/// Required runtime behavior, propagated through all dependencies.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Classification(u8);
impl Classification {
    /// Requires admitted base-view reads.
    pub const fn reads(self) -> bool {
        self.0 & 1 != 0
    }
    /// Requires private mutation staging.
    pub const fn writes(self) -> bool {
        self.0 & 2 != 0
    }
    /// Requires query-level eager search reports.
    pub const fn searches(self) -> bool {
        self.0 & 4 != 0
    }
}
/// Semantic barriers accumulated from dependencies; not optimizer hints.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Barriers(u16);
impl Barriers {
    /// Whether a semantic boundary occurs in this dependency subgraph.
    pub const fn contains(self, barrier: Barrier) -> bool {
        self.0 & barrier as u16 != 0
    }
    /// Optional predicate/null extension must remain attached.
    pub const fn optional(self) -> bool {
        self.0 & 1 != 0
    }
    /// Explicit projection scope must not be bypassed.
    pub const fn scope(self) -> bool {
        self.0 & 2 != 0
    }
}
/// One DAG node. Dependencies are explicit and are not inferred from text.
#[derive(Clone, Copy, Debug)]
pub struct Operator<'a> {
    /// Ordered input edges.
    pub inputs: &'a [PlanNodeId],
    /// Typed semantics.
    pub kind: OperatorKind<'a>,
}
/// Immutable caller-owned arenas; all backing stays borrowed for the plan life.
#[derive(Clone, Copy, Debug)]
pub struct PlanDescription<'a> {
    /// Operator arena.
    pub operators: &'a [Operator<'a>],
    /// Expression arena.
    pub expressions: &'a [Expression<'a>],
    /// Exact parameter declarations.
    pub parameters: &'a [Parameter<'a>],
    /// Row-producing root.
    pub root: PlanNodeId,
    /// Eager obligations in source order, independent of row short-circuiting.
    pub eager_searches: &'a [PlanNodeId],
}
/// Owner attestation for actual retained capacity, including all arenas,
/// backing strings/lists, compiler state, facts and validation scratch. This
/// validator can verify exposed spans only. ZE-49 supplies authoritative
/// reservations and accounts concurrent runtime/result ownership.
#[derive(Clone, Copy, Debug)]
pub struct PlanFootprint {
    retained_bytes: usize,
}
impl PlanFootprint {
    /// Declares actual charged bytes, not just slice lengths.
    pub const fn declared(retained_bytes: usize) -> Self {
        Self { retained_bytes }
    }
    /// The caller's complete retained-capacity declaration.
    pub const fn retained_bytes(self) -> usize {
        self.retained_bytes
    }
}
#[derive(Clone, Copy, Debug, Default)]
struct Slot {
    id: u32,
    kinds: ValueKinds,
}
/// Caller-owned validation output. Private fields cannot forge admitted facts.
#[derive(Clone, Debug)]
pub struct NodeFacts {
    state: u8,
    slots: [Slot; MAX_COLUMNS],
    width: usize,
    depth: usize,
    classification: Classification,
    barriers: Barriers,
    singleton: bool,
    ordered: bool,
    search_calls: u8,
}
impl Default for NodeFacts {
    fn default() -> Self {
        Self {
            state: 0,
            slots: [Slot::default(); MAX_COLUMNS],
            width: 0,
            depth: 0,
            classification: Classification::default(),
            barriers: Barriers::default(),
            singleton: false,
            ordered: false,
            search_calls: 0,
        }
    }
}
impl NodeFacts {
    /// Explicit order preserved from a validated Sort through stable operators.
    pub const fn ordered(&self) -> bool {
        self.ordered
    }
    /// Proven exactly one row; grouped or row-correlated inputs are excluded.
    pub const fn singleton(&self) -> bool {
        self.singleton
    }
    /// Full dependency classification.
    pub const fn classification(&self) -> Classification {
        self.classification
    }
    /// Accumulated semantic barriers.
    pub const fn barriers(&self) -> Barriers {
        self.barriers
    }
    /// Output width, independent of numeric slot IDs.
    pub const fn width(&self) -> usize {
        self.width
    }
    /// Read-only ordinal schema access; logical SlotId is never a cell offset.
    pub fn slot_at(&self, ordinal: usize) -> Option<(SlotId, ValueKinds)> {
        let slot = self.slots.get(..self.width)?.get(ordinal)?;
        Some((SlotId(slot.id), slot.kinds))
    }
    /// Possible value kinds for a slot in this exact scope.
    pub fn slot(&self, id: SlotId) -> Option<ValueKinds> {
        self.slots
            .get(..self.width)?
            .iter()
            .find(|slot| slot.id == id.0)
            .map(|slot| slot.kinds)
    }
}
/// Validated immutable borrowed plan, with no lease or executable pointers.
pub struct GraphPlan<'a, 'facts> {
    description: PlanDescription<'a>,
    facts: &'facts [NodeFacts],
    footprint: PlanFootprint,
    fact_owner: Option<RetainedRegion>,
}
impl<'a, 'facts> GraphPlan<'a, 'facts> {
    /// Validates references, cycles, every expression use scope and reservations.
    pub fn validate(
        description: PlanDescription<'a>,
        facts: &'facts mut [NodeFacts],
        footprint: PlanFootprint,
        backing: PlanBacking<'_>,
        context: &mut ValueContext<'_>,
    ) -> Result<Self, PlanError> {
        validate::validate(description, facts, footprint, backing, context)?;
        Ok(Self {
            description,
            facts,
            footprint,
            fact_owner: None,
        })
    }
    /// Validates while retaining a private certificate of the complete facts Vec
    /// capacity. Runtime admission requires this actual owner, not a raw-slice
    /// visible-length declaration. The retained loan prevents growth or release.
    pub fn validate_with_fact_vec(
        description: PlanDescription<'a>,
        facts: &'facts mut Vec<NodeFacts>,
        footprint: PlanFootprint,
        backing: PlanBacking<'_>,
        context: &mut ValueContext<'_>,
    ) -> Result<Self, PlanError> {
        let owner = RetainedRegion::vector(facts)?;
        let mut result = Self::validate(description, facts, footprint, backing, context)?;
        result.fact_owner = Some(owner);
        Ok(result)
    }
    pub(crate) const fn fact_owner(&self) -> Option<RetainedRegion> {
        self.fact_owner
    }
    pub(crate) fn verify_runtime_backing(
        &self,
        footprint: PlanFootprint,
        backing: PlanBacking<'_>,
        context: &mut ValueContext<'_>,
    ) -> Result<(), PlanError> {
        if self.fact_owner.is_none() {
            return Err(PlanError::Footprint);
        }
        validate::preflight(self.description, self.facts, footprint, backing, context)
    }
    /// Checks exact bindings on every execution; entities, including those
    /// nested in lists, are never accepted through the parameter seam.
    pub fn validate_parameters(
        &self,
        bindings: &[ParameterBinding<'_>],
        context: &mut ValueContext<'_>,
    ) -> Result<(), PlanError> {
        parameters::validate(self.description.parameters, bindings, context)
    }
    /// The same immutable typed description that was validated.
    pub const fn description(&self) -> PlanDescription<'a> {
        self.description
    }
    /// Validated facts for an operator.
    pub fn facts(&self, id: PlanNodeId) -> Option<&NodeFacts> {
        self.facts
            .get(..self.description.operators.len())?
            .get(id.0 as usize)
    }
    /// Includes row root and every eager source obligation.
    pub fn classification(&self) -> Classification {
        let mut bits = self
            .facts
            .get(self.description.root.0 as usize)
            .map_or(0, |fact| fact.classification.0);
        for source in self.description.eager_searches {
            bits |= self
                .facts
                .get(source.0 as usize)
                .map_or(0, |fact| fact.classification.0);
        }
        Classification(bits)
    }
    /// Actual retained capacity attested by the reservation owner.
    pub const fn footprint(&self) -> PlanFootprint {
        self.footprint
    }
}
/// Validation failure; no partial plan is returned.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanError {
    /// Arena/depth/width exceeds a hard bound.
    Limit,
    /// Visible spans exceed the declaration, or total declaration exceeds 24MiB.
    Footprint,
    /// Missing arena reference.
    Reference,
    /// Dependency or expression cycle.
    Cycle,
    /// Unreachable unused arena node.
    Unreachable,
    /// Wrong number of dependency edges.
    Arity,
    /// Slot is absent from this exact input scope or is multiply defined.
    Scope,
    /// Statically incompatible expression kind.
    Type,
    /// Invalid/duplicate declaration or missing/wrong/surplus binding.
    Parameter,
    /// Mutation input is not an explicit eager barrier.
    Barrier,
    /// A base-view reading clause follows a write.
    ReadAfterWrite,
    /// Search and mutations appear in one statement.
    ReadWriteSearch,
    /// Invalid aggregate nesting or use context.
    Aggregate,
    /// Invalid eager call inventory, source context, order or request bounds.
    Search,
    /// Invalid finite inclusive path interval.
    PathBound,
    /// Existing control/work limit aborted validation.
    Control(QueryError),
}
impl From<QueryError> for PlanError {
    fn from(error: QueryError) -> Self {
        Self::Control(error)
    }
}
impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "graph plan validation: {self:?}")
    }
}
impl std::error::Error for PlanError {}
