//! Fixed typed native result ownership. Producers remain responsible for actual
//! admitted entity reads; this module validates and owns their copied description.
pub use super::plan::ValueKinds;
use super::resources::{MemoryError, QueryArena, QueryReservation};
use super::runtime::{RuntimeContext, RuntimeError, WorkCounters, WorkKind};
use super::{QueryError, QueryView};
use crate::property_graph::{EntityId, GraphGeneration};
mod records;
pub use records::*;
mod validate;

/// Checked span into the named typed pool, never a byte pointer.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Span {
    /// First element or byte offset in the explicitly named pool.
    pub start: u32,
    /// Number of elements or bytes, including zero for a present empty span.
    pub len: u32,
}
impl Span {
    /// Constructs untrusted geometry, validated during preparation.
    pub const fn new(start: u32, len: u32) -> Self {
        Self { start, len }
    }
    fn range(self) -> Option<std::ops::Range<usize>> {
        let start = usize::try_from(self.start).ok()?;
        Some(start..start.checked_add(usize::try_from(self.len).ok()?)?)
    }
}
/// One checked value-pool index.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ValueIndex(pub u32);
/// Scalar bits and owned-pool references. No source lifetime can enter this type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Value {
    /// Explicit query null, separate from empty strings/lists.
    Null,
    /// Boolean scalar or homogeneous boolean-list element tag.
    Bool(bool),
    /// Exact signed 64-bit integer or homogeneous integer-list element tag.
    I64(i64),
    /// Exact IEEE binary64 bits or homogeneous floating-list element tag.
    F64(u64),
    /// Exact UTF-8 range or homogeneous string-list element tag.
    String(Span),
    /// Index into copied native nodes, never a source pointer.
    Node(u32),
    /// Index into copied native relationships, never a source pointer.
    Relationship(u32),
    /// Ordered bounded child range with an exact list interpretation.
    List {
        /// Ordered range in the child-index pool.
        children: Span,
        /// Query-list or exact stored scalar-list interpretation.
        element: ListKind,
    },
}
/// One output column's copied name and validated possible value kinds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Column {
    /// Exact UTF-8 name range in the byte pool.
    pub name: Span,
    /// Nonempty set of supported kinds allowed in this output column.
    pub kinds: ValueKinds,
}
/// Typed initialized pool description. Its producer retains its own backing.
#[derive(Clone, Copy, Default)]
pub struct Pools<'a> {
    /// Postorder typed values; a list child always precedes its parent.
    pub values: &'a [Value],
    /// Exact owned UTF-8 string/name/key bytes; ranges preserve embedded NUL.
    pub bytes: &'a [u8],
    /// Ordered column descriptors; row order is independent of entity-pool order.
    pub columns: &'a [Column],
    /// Row-major indices into the value pool, preserving every duplicate.
    pub cells: &'a [ValueIndex],
    /// Ordered list child indices; repeated indices preserve list multiplicity.
    pub children: &'a [ValueIndex],
    /// UTF-8 name ranges used by strictly ordered node label sets.
    pub names: &'a [Span],
    /// Named scalar or typed homogeneous scalar-list properties.
    pub properties: &'a [Property],
    /// Copied nodes sorted uniquely by their full NodeId.
    pub nodes: &'a [Node],
    /// Copied relationships sorted uniquely by their full RelId.
    pub relationships: &'a [Relationship],
    /// Original finite f32 coordinate bits for explicitly selected vectors.
    pub vectors: &'a [u32],
    /// Complete eager reports in source-call order, including zero-row calls.
    pub reports: &'a [SearchReport],
    /// Original per-item generations/revisions supplied by the write coordinator.
    pub receipts: &'a [Receipt],
}
/// Coordinator-supplied outcome. Storage copying cannot establish a commit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// Read result; no write disposition or fresh generation is fabricated.
    Read,
    /// Known coordinator commit with its changed generation.
    Committed {
        /// Changed generation authenticated by the coordinator.
        changed: GraphGeneration,
    },
    /// Only exact replay receipts, retaining each original changed generation.
    Replayed,
    /// Successful effect-free operation at the admitted generation.
    NoOp,
}
/// Complete input bound to the actual query token, not only a generation number.
#[derive(Clone, Copy)]
pub struct ResultInput<'a> {
    /// Exact admitted token; matching store/generation numbers alone are insufficient.
    pub view: &'a QueryView,
    /// Complete initialized typed representation retained by its producer.
    pub pools: Pools<'a>,
    /// Complete row count, including zero-column rows and bag duplicates.
    pub rows: u32,
    /// Coordinator-authenticated disposition; copying establishes no durable result.
    pub outcome: Outcome,
}
/// Required real producer errors; missing/deleted records never become empty values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceError {
    /// The actual producer could not resolve the requested identity.
    Missing(EntityId),
    /// The actual producer refuses returning a deleted entity object.
    Deleted(EntityId),
    /// The producer/token belongs to another actual admission.
    ForeignView,
    /// The actual producer reported a storage failure.
    Storage,
}
/// An internal producer must resolve entities from the admitted base/allowed
/// overlay and retain its input backing until the synchronous copy finishes.
pub trait ResultSource {
    /// Returns an already resolved description or a typed producer failure.
    fn result_input(&self) -> Result<ResultInput<'_>, SourceError>;
}
/// Typed preparation rejection without any partial application-owned result.
#[derive(Debug)]
pub enum CompletedError {
    /// Explicit entity producer failure, preserved separately from shape errors.
    Source(SourceError),
    /// Real query control, work or memory rejection.
    Runtime(RuntimeError),
    /// Malformed typed index, range, kind, ordering or metadata.
    Shape,
    /// A referenced byte span is not complete valid UTF-8.
    Utf8,
    /// The complete represented result or list geometry exceeds a hard cap.
    Limit,
}
impl From<RuntimeError> for CompletedError {
    fn from(e: RuntimeError) -> Self {
        Self::Runtime(e)
    }
}
impl From<MemoryError> for CompletedError {
    fn from(e: MemoryError) -> Self {
        Self::Runtime(e.into())
    }
}
impl From<QueryError> for CompletedError {
    fn from(e: QueryError) -> Self {
        Self::Runtime(e.into())
    }
}
/// Retained metadata; work counters are supplied by the single runtime authority.
#[derive(Clone, Copy, Debug)]
pub struct Metadata {
    /// Exact logical generation associated with this record or admitted view.
    pub generation: GraphGeneration,
    /// Coordinator-authenticated disposition; copying establishes no durable result.
    pub outcome: Outcome,
    /// Complete row count, including zero-column rows and bag duplicates.
    pub rows: u32,
    /// Final actual work from the one runtime counter authority.
    pub counters: WorkCounters,
    /// Measured peak of real query reservations, including overlap.
    pub peak_query_bytes: usize,
}

// Private sealed set: neither arbitrary QueryValue nor borrowed user types can detach.
pub(super) trait OwnedElement: Copy + 'static {}
impl OwnedElement for u8 {}
impl OwnedElement for Value {}
impl OwnedElement for Column {}
impl OwnedElement for ValueIndex {}
impl OwnedElement for Span {}
impl OwnedElement for Property {}
impl OwnedElement for Node {}
impl OwnedElement for Relationship {}
impl OwnedElement for u32 {}
impl OwnedElement for SearchReport {}
impl OwnedElement for Receipt {}

/// Prepared genuine owned arrays, still charged to the query and aggregate.
pub struct PreparedGraphResult<'m, 'g> {
    values: QueryArena<'m, 'g, Value>,
    bytes: QueryArena<'m, 'g, u8>,
    columns: QueryArena<'m, 'g, Column>,
    cells: QueryArena<'m, 'g, ValueIndex>,
    children: QueryArena<'m, 'g, ValueIndex>,
    names: QueryArena<'m, 'g, Span>,
    properties: QueryArena<'m, 'g, Property>,
    nodes: QueryArena<'m, 'g, Node>,
    relationships: QueryArena<'m, 'g, Relationship>,
    vectors: QueryArena<'m, 'g, u32>,
    reports: QueryArena<'m, 'g, SearchReport>,
    receipts: QueryArena<'m, 'g, Receipt>,
    metadata: Metadata,
    represented: usize,
    _control: QueryReservation<'m, 'g>,
}
/// Completed independent storage. No query, caller, view or store lifetime exists.
pub struct CompletedGraphResult {
    values: Vec<Value>,
    bytes: Vec<u8>,
    columns: Vec<Column>,
    cells: Vec<ValueIndex>,
    children: Vec<ValueIndex>,
    names: Vec<Span>,
    properties: Vec<Property>,
    nodes: Vec<Node>,
    relationships: Vec<Relationship>,
    vectors: Vec<u32>,
    reports: Vec<SearchReport>,
    receipts: Vec<Receipt>,
    metadata: Metadata,
}
fn text(pools: Pools<'_>, span: Span) -> Result<&str, CompletedError> {
    let bytes = pools
        .bytes
        .get(span.range().ok_or(CompletedError::Shape)?)
        .ok_or(CompletedError::Shape)?;
    std::str::from_utf8(bytes).map_err(|_| CompletedError::Utf8)
}
fn copy<'m, 'g, T: OwnedElement>(
    input: &[T],
    context: &mut RuntimeContext<'_, 'm, 'g>,
) -> Result<QueryArena<'m, 'g, T>, CompletedError> {
    let mut arena = QueryArena::new(context.memory(), input.len())?;
    for part in input.chunks((65536 / std::mem::size_of::<T>()).max(1)) {
        context.charge(WorkKind::CopiedBytes, std::mem::size_of_val(part) as u64)?;
        arena.extend_copy(part)?;
    }
    Ok(arena)
}
impl<'m, 'g> PreparedGraphResult<'m, 'g> {
    /// Validates and copies under one real context; no storage read is fabricated.
    pub fn copy_from(
        source: &impl ResultSource,
        context: &mut RuntimeContext<'_, 'm, 'g>,
    ) -> Result<Self, CompletedError> {
        context.checkpoint()?;
        let input = source.result_input().map_err(CompletedError::Source)?;
        if !std::ptr::eq(input.view, context.view()) {
            return Err(CompletedError::Source(SourceError::ForeignView));
        }
        let pools = input.pools;
        if input.rows > 65536
            || pools.columns.len() > super::plan::MAX_COLUMNS
            || usize::try_from(input.rows)
                .ok()
                .and_then(|n| n.checked_mul(pools.columns.len()))
                != Some(pools.cells.len())
        {
            return Err(CompletedError::Shape);
        }
        let represented = [
            std::mem::size_of::<CompletedGraphResult>(),
            std::mem::size_of_val(pools.values),
            std::mem::size_of_val(pools.bytes),
            std::mem::size_of_val(pools.columns),
            std::mem::size_of_val(pools.cells),
            std::mem::size_of_val(pools.children),
            std::mem::size_of_val(pools.names),
            std::mem::size_of_val(pools.properties),
            std::mem::size_of_val(pools.nodes),
            std::mem::size_of_val(pools.relationships),
            std::mem::size_of_val(pools.vectors),
            std::mem::size_of_val(pools.reports),
            std::mem::size_of_val(pools.receipts),
        ]
        .into_iter()
        .try_fold(0_usize, |n, b| n.checked_add(b))
        .ok_or(CompletedError::Limit)?;
        if represented > 4 * 1024 * 1024 {
            return Err(CompletedError::Limit);
        }
        let control = context.memory().reserve(std::mem::size_of::<Self>())?;
        validate::validate(pools, input.outcome, context)?;
        Ok(Self {
            values: copy(pools.values, context)?,
            bytes: copy(pools.bytes, context)?,
            columns: copy(pools.columns, context)?,
            cells: copy(pools.cells, context)?,
            children: copy(pools.children, context)?,
            names: copy(pools.names, context)?,
            properties: copy(pools.properties, context)?,
            nodes: copy(pools.nodes, context)?,
            relationships: copy(pools.relationships, context)?,
            vectors: copy(pools.vectors, context)?,
            reports: copy(pools.reports, context)?,
            receipts: copy(pools.receipts, context)?,
            metadata: Metadata {
                generation: input.view.generation(),
                outcome: input.outcome,
                rows: input.rows,
                counters: WorkCounters::default(),
                peak_query_bytes: 0,
            },
            represented,
            _control: control,
        })
    }
    /// Read-only initialized pools while preparation still owns all charges.
    pub fn pools(&self) -> Pools<'_> {
        Pools {
            values: self.values.as_slice(),
            bytes: self.bytes.as_slice(),
            columns: self.columns.as_slice(),
            cells: self.cells.as_slice(),
            children: self.children.as_slice(),
            names: self.names.as_slice(),
            properties: self.properties.as_slice(),
            nodes: self.nodes.as_slice(),
            relationships: self.relationships.as_slice(),
            vectors: self.vectors.as_slice(),
            reports: self.reports.as_slice(),
            receipts: self.receipts.as_slice(),
        }
    }
    /// Actual initialized native representation; full capacities remain separately charged.
    pub const fn represented_bytes(&self) -> usize {
        self.represented
    }
    /// Stable admitted generation, outcome and row count captured from the
    /// producer once. Counters and peak are unfinished here; the final driver
    /// supplies them at detach. C preparation reads this owner, never the source.
    pub const fn metadata(&self) -> &Metadata {
        &self.metadata
    }
    /// Infallibly moves every existing allocation and relinquishes real temporary
    /// guards exactly once. Call only after the producer's final admission check.
    pub fn detach(self, counters: WorkCounters, peak_query_bytes: usize) -> CompletedGraphResult {
        CompletedGraphResult {
            values: self.values.detach_owned(),
            bytes: self.bytes.detach_owned(),
            columns: self.columns.detach_owned(),
            cells: self.cells.detach_owned(),
            children: self.children.detach_owned(),
            names: self.names.detach_owned(),
            properties: self.properties.detach_owned(),
            nodes: self.nodes.detach_owned(),
            relationships: self.relationships.detach_owned(),
            vectors: self.vectors.detach_owned(),
            reports: self.reports.detach_owned(),
            receipts: self.receipts.detach_owned(),
            metadata: Metadata {
                counters,
                peak_query_bytes,
                ..self.metadata
            },
        }
    }
}
impl CompletedGraphResult {
    /// Read-only initialized typed arrays; nothing is fetched from a store.
    pub fn pools(&self) -> Pools<'_> {
        Pools {
            values: &self.values,
            bytes: &self.bytes,
            columns: &self.columns,
            cells: &self.cells,
            children: &self.children,
            names: &self.names,
            properties: &self.properties,
            nodes: &self.nodes,
            relationships: &self.relationships,
            vectors: &self.vectors,
            reports: &self.reports,
            receipts: &self.receipts,
        }
    }
    /// Complete generation/outcome and final actual counters.
    pub const fn metadata(&self) -> &Metadata {
        &self.metadata
    }
    /// Checked row-major cell access, retaining duplicate rows/cells.
    pub fn cell(&self, row: usize, column: usize) -> Option<&Value> {
        if row >= self.metadata.rows as usize || column >= self.columns.len() {
            return None;
        }
        let cell = self
            .cells
            .get(row.checked_mul(self.columns.len())?.checked_add(column)?)?;
        self.values.get(cell.0 as usize)
    }
    /// Checked owned UTF-8 access; an empty span remains an empty string.
    pub fn string(&self, span: Span) -> Option<&str> {
        text(self.pools(), span).ok()
    }
}
