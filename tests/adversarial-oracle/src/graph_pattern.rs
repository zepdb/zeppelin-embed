//! Independent, std-only tiny-graph oracle for native pattern matching.
//!
//! The module consumes a primitive graph description plus a primitive pattern
//! description and produces the exact expected multiset of result rows. It never
//! depends on engine crates, never reads engine state, and never calls a
//! production helper: every rule below is written from the ZE-50 contract.
//!
//! Contract implemented here:
//!
//! * Nodes are `(id, labels)`; relationships are `(rel, source, target, type)`.
//! * `Expand` in `Out`/`In`/`Undirected` visits an undirected self-loop exactly
//!   once and visits parallel relationships separately.
//! * `BoundedExpand` uses an inclusive `[min, max]` bound with `max <= 16`,
//!   walks depth first, and never reuses a relationship inside one path. Depth
//!   zero emits `[start, start, []]`.
//! * `Join` is the multiset product restricted to shared-slot equality; a null
//!   never joins.
//! * `Optional` evaluates the right side once per left row, including that
//!   side's own predicate, and emits one row with null new slots when the right
//!   side produced nothing for that left row.
//! * Relationship uniqueness is enforced per `PatternId` across joins inside one
//!   pattern; a later independent pattern gets a fresh uniqueness set.

use std::collections::BTreeMap;

/// Stored node identity.
pub type NodeId = u128;
/// Stored relationship identity.
pub type RelId = u128;
/// Result column identity.
pub type SlotId = u32;
/// Syntactic MATCH identity that scopes relationship uniqueness.
pub type PatternId = u32;

/// Primitive node fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Node {
    /// Stored identity.
    pub id: NodeId,
    /// Exact label names.
    pub labels: Vec<String>,
}

impl Node {
    /// Builds one node fact from borrowed primitives.
    #[must_use]
    pub fn new(id: NodeId, labels: &[&str]) -> Self {
        Self {
            id,
            labels: labels.iter().map(|label| (*label).to_owned()).collect(),
        }
    }
}

/// Primitive relationship fact with directed endpoints.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Edge {
    /// Stored identity.
    pub rel: RelId,
    /// Directed source endpoint.
    pub source: NodeId,
    /// Directed target endpoint.
    pub target: NodeId,
    /// Exact relationship type name.
    pub relationship_type: String,
}

impl Edge {
    /// Builds one relationship fact from borrowed primitives.
    #[must_use]
    pub fn new(rel: RelId, source: NodeId, target: NodeId, relationship_type: &str) -> Self {
        Self {
            rel,
            source,
            target,
            relationship_type: relationship_type.to_owned(),
        }
    }
}

/// Complete tiny graph. Order is the fixture order; results are bag compared.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Graph {
    /// Every live node.
    pub nodes: Vec<Node>,
    /// Every live relationship, including parallel and self relationships.
    pub edges: Vec<Edge>,
}

/// Expansion direction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    /// Source to target only.
    Out,
    /// Target to source only.
    In,
    /// Either endpoint, with a self-loop visited exactly once.
    Undirected,
}

/// One result cell.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Cell {
    /// Unbound optional slot.
    Null,
    /// Bound node.
    Node(NodeId),
    /// Bound relationship.
    Relationship(RelId),
    /// Bound ordered relationship path.
    Relationships(Vec<RelId>),
}

/// One result row, ordered by slot so plan column order cannot change it.
pub type Row = Vec<(SlotId, Cell)>;

/// Sorted multiset of result rows; duplicates are preserved.
pub type Bag = Vec<Row>;

/// Sorts observed rows into the comparable multiset form used by both sides.
#[must_use]
pub fn bag(mut rows: Vec<Row>) -> Bag {
    for row in &mut rows {
        row.sort();
    }
    rows.sort();
    rows
}

/// Tiny three-valued predicate over already bound slots.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Predicate {
    /// True when the slot holds a node carrying the label.
    HasLabel {
        /// Node slot.
        slot: SlotId,
        /// Exact label name.
        label: String,
    },
    /// True when both slots hold the same non-null value.
    Same {
        /// Left slot.
        left: SlotId,
        /// Right slot.
        right: SlotId,
    },
    /// Three-valued negation; a null operand stays unretained.
    Not(Box<Predicate>),
}

/// Tiny pattern tree mirroring the accepted native operator set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TinyPattern {
    /// One empty row.
    Unit,
    /// The enclosing optional's current left row.
    Anchor,
    /// One node by stored identity, dropped when that node is not live.
    LookupNode {
        /// Upstream rows.
        input: Box<TinyPattern>,
        /// New node slot.
        output: SlotId,
        /// Stored identity.
        id: NodeId,
    },
    /// Every live node, optionally restricted to one label.
    ScanNodes {
        /// Upstream rows.
        input: Box<TinyPattern>,
        /// New node slot.
        output: SlotId,
        /// Optional label restriction.
        label: Option<String>,
    },
    /// One relationship step.
    Expand {
        /// Upstream rows.
        input: Box<TinyPattern>,
        /// Bound start node slot.
        source: SlotId,
        /// New endpoint node slot.
        node: SlotId,
        /// New relationship slot.
        relationship: SlotId,
        /// Step direction.
        direction: Direction,
        /// Empty accepts every type.
        relationship_types: Vec<String>,
        /// Uniqueness scope.
        pattern: PatternId,
    },
    /// Inclusive `[min, max]` variable-length step.
    BoundedExpand {
        /// Upstream rows.
        input: Box<TinyPattern>,
        /// Bound start node slot.
        source: SlotId,
        /// New endpoint node slot.
        node: SlotId,
        /// New relationship-list slot.
        relationships: SlotId,
        /// Inclusive lower bound.
        min: u8,
        /// Inclusive upper bound, at most 16.
        max: u8,
        /// Step direction.
        direction: Direction,
        /// Empty accepts every type.
        relationship_types: Vec<String>,
        /// Uniqueness scope.
        pattern: PatternId,
    },
    /// Multiset product on shared-slot equality.
    Join {
        /// Left input.
        left: Box<TinyPattern>,
        /// Right input.
        right: Box<TinyPattern>,
    },
    /// Correlated optional apply; the right side reads the left row.
    Optional {
        /// Left input.
        left: Box<TinyPattern>,
        /// Right input, whose leaf is `Anchor`.
        right: Box<TinyPattern>,
        /// Attached WHERE evaluated with the combined row.
        predicate: Option<Predicate>,
    },
    /// Retains rows whose predicate is true.
    Filter {
        /// Upstream rows.
        input: Box<TinyPattern>,
        /// Retained-when-true predicate.
        predicate: Predicate,
    },
}

/// Rejected tiny pattern or fixture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PatternError {
    /// A step read a slot the upstream rows do not bind.
    UnboundSlot,
    /// A step read a slot that does not hold a node.
    NotANode,
    /// `max` exceeded the accepted bound of 16, or `min` exceeded `max`.
    Bound,
    /// A step bound a slot that is already bound.
    SlotConflict,
}

/// One relationship binding site, scoped by pattern and by binding origin.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Use {
    pattern: PatternId,
    origin: u32,
    relationship: RelId,
}

/// One partial result row and the relationship uses it carries.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Binding {
    cells: BTreeMap<SlotId, Cell>,
    uses: Vec<Use>,
}

impl Binding {
    fn empty() -> Self {
        Self {
            cells: BTreeMap::new(),
            uses: Vec::new(),
        }
    }

    fn node(&self, slot: SlotId) -> Result<NodeId, PatternError> {
        match self.cells.get(&slot) {
            Some(Cell::Node(id)) => Ok(*id),
            Some(_) => Err(PatternError::NotANode),
            None => Err(PatternError::UnboundSlot),
        }
    }

    fn bind(&mut self, slot: SlotId, cell: Cell) -> Result<(), PatternError> {
        if self.cells.insert(slot, cell).is_some() {
            return Err(PatternError::SlotConflict);
        }
        Ok(())
    }

    fn add_use(&mut self, value: Use) {
        if !self.uses.contains(&value) {
            self.uses.push(value);
        }
    }

    /// A relationship already bound by another site in the same pattern is
    /// unavailable; the same site rebinding it is not a reuse.
    fn blocked(&self, pattern: PatternId, origin: u32, relationship: RelId) -> bool {
        self.uses.iter().any(|used| {
            used.pattern == pattern && used.relationship == relationship && used.origin != origin
        })
    }

    fn row(&self) -> Row {
        self.cells
            .iter()
            .map(|(slot, cell)| (*slot, cell.clone()))
            .collect()
    }
}

fn compatible(left: &[Use], right: &[Use]) -> bool {
    !left.iter().any(|left| {
        right.iter().any(|right| {
            left.pattern == right.pattern
                && left.relationship == right.relationship
                && left.origin != right.origin
        })
    })
}

/// Evaluates one tiny pattern over one tiny graph into the exact expected bag.
///
/// # Errors
///
/// Returns [`PatternError`] when the pattern reads an unbound slot, reads a slot
/// that does not hold a node, rebinds a bound slot, or declares an unaccepted
/// variable-length bound.
pub fn evaluate(graph: &Graph, pattern: &TinyPattern) -> Result<Bag, PatternError> {
    let bindings = evaluate_node(graph, pattern, 0, &[Binding::empty()])?;
    Ok(bag(bindings.iter().map(Binding::row).collect()))
}

/// Pre-order node count, which also assigns each step its binding origin.
fn size(pattern: &TinyPattern) -> u32 {
    let children = match pattern {
        TinyPattern::Unit | TinyPattern::Anchor => 0,
        TinyPattern::LookupNode { input, .. }
        | TinyPattern::ScanNodes { input, .. }
        | TinyPattern::Expand { input, .. }
        | TinyPattern::BoundedExpand { input, .. }
        | TinyPattern::Filter { input, .. } => size(input),
        TinyPattern::Join { left, right } | TinyPattern::Optional { left, right, .. } => {
            size(left) + size(right)
        }
    };
    children + 1
}

/// Slots a subtree binds beyond the rows it consumes.
fn slots(pattern: &TinyPattern, into: &mut Vec<SlotId>) {
    match pattern {
        TinyPattern::Unit | TinyPattern::Anchor => {}
        TinyPattern::LookupNode { input, output, .. }
        | TinyPattern::ScanNodes { input, output, .. } => {
            slots(input, into);
            into.push(*output);
        }
        TinyPattern::Expand {
            input,
            node,
            relationship,
            ..
        } => {
            slots(input, into);
            into.push(*node);
            into.push(*relationship);
        }
        TinyPattern::BoundedExpand {
            input,
            node,
            relationships,
            ..
        } => {
            slots(input, into);
            into.push(*node);
            into.push(*relationships);
        }
        TinyPattern::Filter { input, .. } => slots(input, into),
        TinyPattern::Join { left, right } | TinyPattern::Optional { left, right, .. } => {
            slots(left, into);
            slots(right, into);
        }
    }
}

fn evaluate_node(
    graph: &Graph,
    pattern: &TinyPattern,
    origin: u32,
    input: &[Binding],
) -> Result<Vec<Binding>, PatternError> {
    match pattern {
        TinyPattern::Unit => Ok(vec![Binding::empty()]),
        TinyPattern::Anchor => Ok(input.to_vec()),
        TinyPattern::LookupNode {
            input: child,
            output,
            id,
        } => {
            let rows = evaluate_node(graph, child, origin + 1, input)?;
            if !graph.nodes.iter().any(|node| node.id == *id) {
                return Ok(Vec::new());
            }
            let mut output_rows = Vec::new();
            for row in &rows {
                let mut extended = row.clone();
                extended.bind(*output, Cell::Node(*id))?;
                output_rows.push(extended);
            }
            Ok(output_rows)
        }
        TinyPattern::ScanNodes {
            input: child,
            output,
            label,
        } => {
            let rows = evaluate_node(graph, child, origin + 1, input)?;
            let mut output_rows = Vec::new();
            for row in &rows {
                for node in &graph.nodes {
                    if label
                        .as_ref()
                        .is_some_and(|label| !node.labels.contains(label))
                    {
                        continue;
                    }
                    let mut extended = row.clone();
                    extended.bind(*output, Cell::Node(node.id))?;
                    output_rows.push(extended);
                }
            }
            Ok(output_rows)
        }
        TinyPattern::Expand {
            input: child,
            source,
            node,
            relationship,
            direction,
            relationship_types,
            pattern: pattern_id,
        } => {
            let rows = evaluate_node(graph, child, origin + 1, input)?;
            let mut output_rows = Vec::new();
            for row in &rows {
                let start = row.node(*source)?;
                for edge in &graph.edges {
                    let Some(neighbor) = step(edge, start, *direction, relationship_types) else {
                        continue;
                    };
                    if row.blocked(*pattern_id, origin, edge.rel) {
                        continue;
                    }
                    let mut extended = row.clone();
                    extended.bind(*node, Cell::Node(neighbor))?;
                    extended.bind(*relationship, Cell::Relationship(edge.rel))?;
                    extended.add_use(Use {
                        pattern: *pattern_id,
                        origin,
                        relationship: edge.rel,
                    });
                    output_rows.push(extended);
                }
            }
            Ok(output_rows)
        }
        TinyPattern::BoundedExpand {
            input: child,
            source,
            node,
            relationships,
            min,
            max,
            direction,
            relationship_types,
            pattern: pattern_id,
        } => {
            if *max > 16 || min > max {
                return Err(PatternError::Bound);
            }
            let rows = evaluate_node(graph, child, origin + 1, input)?;
            let mut output_rows = Vec::new();
            for row in &rows {
                let start = row.node(*source)?;
                let mut path = Vec::new();
                if *min == 0 {
                    output_rows.push(path_row(
                        row,
                        *node,
                        *relationships,
                        start,
                        &path,
                        *pattern_id,
                        origin,
                    )?);
                }
                walk(
                    graph,
                    row,
                    start,
                    &mut path,
                    WalkStep {
                        node: *node,
                        relationships: *relationships,
                        min: *min,
                        max: *max,
                        direction: *direction,
                        relationship_types,
                        pattern: *pattern_id,
                        origin,
                    },
                    &mut output_rows,
                )?;
            }
            Ok(output_rows)
        }
        TinyPattern::Join { left, right } => {
            let left_rows = evaluate_node(graph, left, origin + 1, input)?;
            let right_rows = evaluate_node(graph, right, origin + 1 + size(left), input)?;
            let mut output_rows = Vec::new();
            for left_row in &left_rows {
                for right_row in &right_rows {
                    if !joins(left_row, right_row) || !compatible(&left_row.uses, &right_row.uses) {
                        continue;
                    }
                    output_rows.push(merge(left_row, right_row));
                }
            }
            Ok(output_rows)
        }
        TinyPattern::Optional {
            left,
            right,
            predicate,
        } => {
            let left_rows = evaluate_node(graph, left, origin + 1, input)?;
            let right_origin = origin + 1 + size(left);
            let mut added = Vec::new();
            slots(right, &mut added);
            let mut output_rows = Vec::new();
            for left_row in &left_rows {
                let anchored =
                    evaluate_node(graph, right, right_origin, std::slice::from_ref(left_row))?;
                let mut matched = false;
                for row in anchored {
                    if !predicate
                        .as_ref()
                        .is_none_or(|predicate| retained(graph, &row, predicate))
                    {
                        continue;
                    }
                    matched = true;
                    output_rows.push(row);
                }
                if !matched {
                    let mut extended = left_row.clone();
                    for slot in &added {
                        if !extended.cells.contains_key(slot) {
                            extended.bind(*slot, Cell::Null)?;
                        }
                    }
                    output_rows.push(extended);
                }
            }
            Ok(output_rows)
        }
        TinyPattern::Filter {
            input: child,
            predicate,
        } => {
            let rows = evaluate_node(graph, child, origin + 1, input)?;
            Ok(rows
                .into_iter()
                .filter(|row| retained(graph, row, predicate))
                .collect())
        }
    }
}

/// Fixed facts of one variable-length walk.
struct WalkStep<'a> {
    node: SlotId,
    relationships: SlotId,
    min: u8,
    max: u8,
    direction: Direction,
    relationship_types: &'a [String],
    pattern: PatternId,
    origin: u32,
}

fn walk(
    graph: &Graph,
    row: &Binding,
    current: NodeId,
    path: &mut Vec<RelId>,
    step_facts: WalkStep<'_>,
    output: &mut Vec<Binding>,
) -> Result<(), PatternError> {
    if u8::try_from(path.len()).map_err(|_| PatternError::Bound)? >= step_facts.max {
        return Ok(());
    }
    for edge in &graph.edges {
        let Some(next) = step(
            edge,
            current,
            step_facts.direction,
            step_facts.relationship_types,
        ) else {
            continue;
        };
        if path.contains(&edge.rel) || row.blocked(step_facts.pattern, step_facts.origin, edge.rel)
        {
            continue;
        }
        path.push(edge.rel);
        let depth = u8::try_from(path.len()).map_err(|_| PatternError::Bound)?;
        if depth >= step_facts.min {
            output.push(path_row(
                row,
                step_facts.node,
                step_facts.relationships,
                next,
                path,
                step_facts.pattern,
                step_facts.origin,
            )?);
        }
        walk(
            graph,
            row,
            next,
            path,
            WalkStep {
                node: step_facts.node,
                relationships: step_facts.relationships,
                min: step_facts.min,
                max: step_facts.max,
                direction: step_facts.direction,
                relationship_types: step_facts.relationship_types,
                pattern: step_facts.pattern,
                origin: step_facts.origin,
            },
            output,
        )?;
        path.pop();
    }
    Ok(())
}

fn path_row(
    row: &Binding,
    node: SlotId,
    relationships: SlotId,
    endpoint: NodeId,
    path: &[RelId],
    pattern: PatternId,
    origin: u32,
) -> Result<Binding, PatternError> {
    let mut extended = row.clone();
    extended.bind(node, Cell::Node(endpoint))?;
    extended.bind(relationships, Cell::Relationships(path.to_vec()))?;
    for relationship in path {
        extended.add_use(Use {
            pattern,
            origin,
            relationship: *relationship,
        });
    }
    Ok(extended)
}

/// One relationship step from `start`, or `None` when the relationship is not
/// reachable in that direction or carries an unaccepted type. An undirected
/// self-loop resolves once because the source endpoint is tested first.
fn step(edge: &Edge, start: NodeId, direction: Direction, types: &[String]) -> Option<NodeId> {
    if !types.is_empty() && !types.contains(&edge.relationship_type) {
        return None;
    }
    match direction {
        Direction::Out if edge.source == start => Some(edge.target),
        Direction::In if edge.target == start => Some(edge.source),
        Direction::Undirected if edge.source == start => Some(edge.target),
        Direction::Undirected if edge.target == start => Some(edge.source),
        _ => None,
    }
}

/// Shared slots must hold equal non-null values; a null never joins.
fn joins(left: &Binding, right: &Binding) -> bool {
    left.cells.iter().all(|(slot, value)| {
        right
            .cells
            .get(slot)
            .is_none_or(|other| *value != Cell::Null && *other != Cell::Null && value == other)
    })
}

fn merge(left: &Binding, right: &Binding) -> Binding {
    let mut merged = left.clone();
    for (slot, cell) in &right.cells {
        merged.cells.insert(*slot, cell.clone());
    }
    for value in &right.uses {
        merged.add_use(*value);
    }
    merged
}

/// Three-valued predicate result; only `True` retains a row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Truth {
    True,
    False,
    Unknown,
}

fn truth(graph: &Graph, row: &Binding, predicate: &Predicate) -> Truth {
    match predicate {
        Predicate::HasLabel { slot, label } => match row.cells.get(slot) {
            Some(Cell::Node(id)) => {
                let carries = graph
                    .nodes
                    .iter()
                    .any(|node| node.id == *id && node.labels.contains(label));
                if carries { Truth::True } else { Truth::False }
            }
            Some(Cell::Null) | None => Truth::Unknown,
            Some(_) => Truth::False,
        },
        Predicate::Same { left, right } => match (row.cells.get(left), row.cells.get(right)) {
            (Some(Cell::Null) | None, _) | (_, Some(Cell::Null) | None) => Truth::Unknown,
            (Some(left), Some(right)) => {
                if left == right {
                    Truth::True
                } else {
                    Truth::False
                }
            }
        },
        Predicate::Not(inner) => match truth(graph, row, inner) {
            Truth::True => Truth::False,
            Truth::False => Truth::True,
            Truth::Unknown => Truth::Unknown,
        },
    }
}

fn retained(graph: &Graph, row: &Binding, predicate: &Predicate) -> bool {
    truth(graph, row, predicate) == Truth::True
}
