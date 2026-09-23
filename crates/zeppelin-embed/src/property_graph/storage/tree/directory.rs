//! Immutable ordered-directory operations. These private preparation components
//! neither acquire a store lease nor publish/sync/delete artifacts.

use super::super::artifact::{ArtifactIdentity, BlockKind, FramedBlock, PhysicalRef};
use super::super::memory::{StorageMemory, StorageReservation};
use super::{Cell, Key, PAGE_BYTES, PageHeader, TreeKind, decode_page, encode_page};
use crate::format::frame::FormatError;
use crate::lifecycle::{QueryControl, QueryError};
use crate::property_graph::query::resources::{QueryMemory, QueryReservation};
use crate::property_graph::query::runtime::{
    RuntimeContext, RuntimeError, RuntimeInstanceId, WorkKind,
};
use crate::property_graph::resources::{GraphReservation, GraphResources};
use crate::property_graph::{GraphGeneration, StoreInstanceId};

/// Typed framing, resource, lifecycle or source failure; no missing-on-corruption fallback.
#[derive(Debug)]
pub enum TreeError {
    /// Required referenced object is absent.
    Missing,
    /// Invalid inner tree semantics, source substitution, or unsupported cell shape.
    Invalid(&'static str),
    /// Outer or page framing failed.
    Format(FormatError),
    /// Retained WAL root metadata failed its existing typed format contract.
    WalMetadata(crate::property_graph::wal::WalError),
    /// Mandatory cancellation or deadline check failed.
    Control(QueryError),
    /// A physical artifact read failed without admitting partial bytes.
    Io(std::io::Error),
    /// Caller reservation or fallible allocation was insufficient.
    Memory,
    /// Checked work budget was exhausted.
    Work,
    /// Exact query runtime rejection, retaining its limit/value/memory kind.
    Runtime(RuntimeError),
}
impl From<FormatError> for TreeError {
    fn from(error: FormatError) -> Self {
        Self::Format(error)
    }
}
impl std::fmt::Display for TreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => f.write_str("missing graph directory artifact"),
            Self::Invalid(reason) => write!(f, "invalid graph directory: {reason}"),
            Self::Format(error) => error.fmt(f),
            Self::WalMetadata(error) => error.fmt(f),
            Self::Control(error) => error.fmt(f),
            Self::Io(error) => error.fmt(f),
            Self::Memory => f.write_str("graph directory reservation exhausted"),
            Self::Work => f.write_str("graph directory work exhausted"),
            Self::Runtime(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for TreeError {}

/// Free mapping slots a scoped source keeps available for a consumer that
/// already observed `scoped_blocks() == false` and is part-way through one
/// bounded operation. The largest such operation compares two overflow keys,
/// and each side pins one root block plus one chunk, so four is the worst case.
pub(crate) const RESERVED_PINNED_SLOTS: usize = 4;

/// Read-only immutable source. The owner retains its coherent base lease and
/// charges mapping/cache capacity. A successful callback must return an exact
/// framed block; tree consumers independently reject substituted references.
pub trait BlockSource {
    /// Resolve only immutable admitted-base or current-preparation objects.
    fn resolve<'a>(
        &'a self,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'a>, TreeError>;

    /// Run one checked read while its backing is scoped to the callback. The
    /// owned result cannot retain the framed block's mapping lifetime.
    /// `Self: Sized` keeps the trait usable as `dyn BlockSource`.
    fn with_block<R>(
        &self,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
        callback: impl for<'a, 'r> FnOnce(
            FramedBlock<'a>,
            &'r mut TreeResources<'_>,
        ) -> Result<R, TreeError>,
    ) -> Result<R, TreeError>
    where
        Self: Sized,
    {
        let block = self.resolve(reference, resources)?;
        callback(block, resources)
    }

    /// Whether trace-native consumers must use scoped copied spans rather than
    /// retaining a borrowed mapping in their cursor cache.
    fn scoped_blocks(&self) -> bool {
        false
    }

    /// Run one bounded traversal whose incidental reads are validated and then
    /// released rather than pinned for this source's whole lifetime. Sources
    /// that already release every mapping keep the no-op default.
    /// `Self: Sized` keeps the trait usable as `dyn BlockSource`.
    fn with_scoped_reads<R>(&self, body: impl FnOnce() -> R) -> R
    where
        Self: Sized,
    {
        body()
    }
}
/// Private append-only sink. The owner reserves retained capacity before append
/// and keeps all new artifacts in an explicit abort inventory, including on error.
/// This trait has no publication, synchronization, overwrite, or unlink operation.
pub trait BlockSink: BlockSource {
    /// Return the exact immutable block reference created for these bytes.
    fn append(
        &mut self,
        kind: BlockKind,
        generation: GraphGeneration,
        bytes: &[u8],
        resources: &mut TreeResources<'_>,
    ) -> Result<PhysicalRef, TreeError>;
}

enum CapacityOwner<'a> {
    Shared(GraphResources),
    Preparation(&'a StorageMemory<'a>),
    Query(&'a QueryMemory<'a>),
}
impl<'a> CapacityOwner<'a> {
    fn reserve(&self, bytes: usize) -> Result<CapacityReservation<'a>, TreeError> {
        match self {
            Self::Shared(shared) => Ok(CapacityReservation::Shared(
                shared.reserve(bytes).map_err(|_| TreeError::Memory)?,
            )),
            Self::Preparation(memory) => {
                Ok(CapacityReservation::Preparation(memory.reserve(bytes)?))
            }
            Self::Query(memory) => Ok(CapacityReservation::Query(
                memory
                    .reserve(bytes)
                    .map_err(RuntimeError::Memory)
                    .map_err(TreeError::Runtime)?,
            )),
        }
    }
}
enum CapacityReservation<'a> {
    Shared(GraphReservation),
    Preparation(StorageReservation<'a>),
    Query(QueryReservation<'a, 'a>),
}

/// Fixed cursor-state charge bound to the same operation owner on every resume.
pub(crate) struct TreeTraceReservation<'a> {
    reservation: CapacityReservation<'a>,
    owner: CursorOwner<'a>,
}

/// One lazily requested copied-span cache charged to the same active owner as
/// the cursor. Backing is declared before its reservation so it drops first.
pub(crate) struct TreeReadBuffer<'a> {
    output: Vec<u8>,
    reservation: CapacityReservation<'a>,
}
impl TreeReadBuffer<'_> {
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.output
    }
    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.output
    }
}
impl TreeTraceReservation<'_> {
    pub(crate) fn require(&self, resources: &TreeResources<'_>) -> Result<(), TreeError> {
        if self.reservation.bytes() == 0 {
            return Err(TreeError::Invalid("empty trace reservation"));
        }
        resources.require_cursor_owner(self.owner)
    }
}
impl CapacityReservation<'_> {
    fn bytes(&self) -> usize {
        match self {
            Self::Shared(charge) => charge.bytes() as usize,
            Self::Preparation(charge) => charge.bytes(),
            Self::Query(charge) => charge.bytes(),
        }
    }
    fn resize(&mut self, bytes: usize) -> Result<(), TreeError> {
        match self {
            Self::Shared(charge) => charge.resize(bytes).map_err(|_| TreeError::Memory),
            Self::Preparation(charge) => charge.resize(bytes),
            Self::Query(charge) => charge
                .resize(bytes)
                .map_err(RuntimeError::Memory)
                .map_err(TreeError::Runtime),
        }
    }
}

trait QueryRuntime {
    fn checkpoint(&self) -> Result<(), RuntimeError>;
    fn charge(&mut self, kind: WorkKind, units: u64) -> Result<(), RuntimeError>;
    #[cfg(feature = "graph-cypher")]
    fn decode_graph_lexical<'m>(
        &mut self,
        bytes: &[u8],
        epoch: crate::fts::tokenizer::TokenizerEpoch,
        memory: &'m QueryMemory<'m>,
    ) -> Result<
        crate::fts::graph_build::DecodedGraphLexical<'m>,
        crate::fts::graph_build::GraphLexicalError,
    >;
}
impl QueryRuntime for RuntimeContext<'_, '_, '_> {
    fn checkpoint(&self) -> Result<(), RuntimeError> {
        RuntimeContext::checkpoint(self)
    }
    fn charge(&mut self, kind: WorkKind, units: u64) -> Result<(), RuntimeError> {
        RuntimeContext::charge(self, kind, units)
    }
    #[cfg(feature = "graph-cypher")]
    fn decode_graph_lexical<'m>(
        &mut self,
        bytes: &[u8],
        epoch: crate::fts::tokenizer::TokenizerEpoch,
        memory: &'m QueryMemory<'m>,
    ) -> Result<
        crate::fts::graph_build::DecodedGraphLexical<'m>,
        crate::fts::graph_build::GraphLexicalError,
    > {
        crate::fts::graph_build::DecodedGraphLexical::decode_query(bytes, epoch, memory, self)
    }
}

#[derive(Clone, Copy)]
pub(crate) enum NativeReadEvent {
    Lookup,
    Scan,
    AdjacencyEntry,
    CopiedBytes(u64),
}
impl NativeReadEvent {
    const fn work(self) -> (WorkKind, u64) {
        match self {
            Self::Lookup => (WorkKind::Lookups, 1),
            Self::Scan => (WorkKind::Scans, 1),
            Self::AdjacencyEntry => (WorkKind::AdjacencyEntries, 1),
            Self::CopiedBytes(bytes) => (WorkKind::CopiedBytes, bytes),
        }
    }
}

enum TreeControl<'a> {
    Direct {
        control: &'a QueryControl,
        limit: u64,
    },
    Query {
        context: &'a mut dyn QueryRuntime,
        identity: RuntimeInstanceId,
    },
}

#[derive(Clone, Copy)]
pub(crate) struct QueryOwner<'m, 'g> {
    memory: &'m QueryMemory<'g>,
    context: RuntimeInstanceId,
}
impl QueryOwner<'_, '_> {
    fn same_owner(self, other: QueryOwner<'_, '_>) -> bool {
        std::ptr::eq(self.memory, other.memory) && self.context == other.context
    }
}

#[derive(Clone, Copy)]
enum CursorOwner<'a> {
    Direct,
    Preparation(&'a StorageMemory<'a>),
    Query(QueryOwner<'a, 'a>),
}

/// Required operation control and checked work accounting.
pub struct TreeResources<'a> {
    control: TreeControl<'a>,
    work: u64,
    owner: CapacityOwner<'a>,
    workspace: CapacityReservation<'a>,
    preparation_checkpoint: Option<&'a dyn Fn() -> Result<(), TreeError>>,
}
impl<'a> TreeResources<'a> {
    /// A caller retains the same control across every component of its operation.
    pub fn new(
        control: &'a QueryControl,
        shared: &GraphResources,
        work_limit: u64,
    ) -> Result<Self, TreeError> {
        control.checkpoint().map_err(TreeError::Control)?;
        let workspace = shared.reserve(STACK_BYTES).map_err(|_| TreeError::Memory)?;
        Ok(Self {
            control: TreeControl::Direct {
                control,
                limit: work_limit,
            },
            work: 0,
            owner: CapacityOwner::Shared(shared.clone()),
            workspace: CapacityReservation::Shared(workspace),
            preparation_checkpoint: None,
        })
    }
    /// Reserves the full tree workspace inside the complete storage preparation.
    pub fn for_prepare(memory: &'a StorageMemory<'a>, work_limit: u64) -> Result<Self, TreeError> {
        let owner = CapacityOwner::Preparation(memory);
        let workspace = owner.reserve(STACK_BYTES)?;
        Ok(Self {
            control: TreeControl::Direct {
                control: memory.control(),
                limit: work_limit,
            },
            work: 0,
            owner,
            workspace,
            preparation_checkpoint: None,
        })
    }
    /// Reserves tree workspace through the exact query memory and retains the
    /// same cumulative runtime context for close-first checkpoints.
    pub fn for_query<'v, 'm, 'g>(
        context: &'a mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<Self, TreeError>
    where
        'm: 'a,
        'g: 'a,
    {
        context.checkpoint().map_err(TreeError::Runtime)?;
        let memory: &'a QueryMemory<'a> = context.memory();
        let identity = context.identity();
        let owner = CapacityOwner::Query(memory);
        let workspace = owner.reserve(STACK_BYTES)?;
        Ok(Self {
            control: TreeControl::Query { context, identity },
            work: 0,
            owner,
            workspace,
            preparation_checkpoint: None,
        })
    }
    pub(crate) fn with_preparation_checkpoint(
        mut self,
        checkpoint: &'a dyn Fn() -> Result<(), TreeError>,
    ) -> Result<Self, TreeError> {
        if !matches!(self.owner, CapacityOwner::Preparation(_)) {
            return Err(TreeError::Invalid(
                "preparation checkpoint requires preparation resources",
            ));
        }
        if self.preparation_checkpoint.is_some() {
            return Err(TreeError::Invalid("duplicate preparation checkpoint"));
        }
        checkpoint()?;
        self.preparation_checkpoint = Some(checkpoint);
        Ok(self)
    }
    pub(crate) fn require_preparation(&self, memory: &StorageMemory<'_>) -> Result<(), TreeError> {
        match &self.owner {
            CapacityOwner::Preparation(owner) if std::ptr::eq(*owner, memory) => Ok(()),
            _ => Err(TreeError::Invalid("storage preparation owner mismatch")),
        }
    }
    pub(crate) fn require_query(&self, memory: &QueryMemory<'_>) -> Result<(), TreeError> {
        match &self.owner {
            CapacityOwner::Query(owner) if std::ptr::eq(*owner, memory) => Ok(()),
            _ => Err(TreeError::Invalid("query memory owner mismatch")),
        }
    }
    pub(crate) fn query_owner<'m, 'g>(
        &self,
        memory: &'m QueryMemory<'g>,
    ) -> Result<QueryOwner<'m, 'g>, TreeError> {
        self.require_query(memory)?;
        match self.cursor_owner()? {
            CursorOwner::Query(owner) => Ok(QueryOwner {
                memory,
                context: owner.context,
            }),
            _ => Err(TreeError::Invalid("query memory owner mismatch")),
        }
    }
    pub(crate) fn require_query_owner(
        &self,
        expected: QueryOwner<'_, '_>,
    ) -> Result<(), TreeError> {
        match self.cursor_owner()? {
            CursorOwner::Query(actual) if expected.same_owner(actual) => Ok(()),
            _ => Err(TreeError::Invalid("query range scratch owner mismatch")),
        }
    }
    pub(crate) fn reserve_trace(
        &mut self,
        bytes: usize,
    ) -> Result<TreeTraceReservation<'a>, TreeError> {
        if bytes == 0 {
            return Err(TreeError::Memory);
        }
        self.step(0)?;
        let owner = self.cursor_owner()?;
        let reservation = self.owner.reserve(bytes)?;
        Ok(TreeTraceReservation { reservation, owner })
    }
    pub(crate) fn copied_span_buffer(
        &mut self,
        bytes: usize,
    ) -> Result<TreeReadBuffer<'a>, TreeError> {
        if bytes == 0 || bytes > super::super::payload::CHUNK_BYTES {
            return Err(TreeError::Memory);
        }
        self.step(0)?;
        let mut reservation = self.owner.reserve(bytes)?;
        let mut output = Vec::new();
        #[cfg(feature = "allocation-audit")]
        let allocation = crate::allocation_audit::attributed(|| output.try_reserve_exact(bytes));
        #[cfg(not(feature = "allocation-audit"))]
        let allocation = output.try_reserve_exact(bytes);
        allocation.map_err(|_| TreeError::Memory)?;
        reservation.resize(output.capacity())?;
        output.resize(bytes, 0);
        Ok(TreeReadBuffer {
            output,
            reservation,
        })
    }
    #[cfg(feature = "graph-cypher")]
    pub(crate) fn decode_graph_lexical<'m>(
        &mut self,
        bytes: &[u8],
        epoch: crate::fts::tokenizer::TokenizerEpoch,
        memory: &'m QueryMemory<'m>,
    ) -> Result<
        crate::fts::graph_build::DecodedGraphLexical<'m>,
        crate::fts::graph_build::GraphLexicalError,
    > {
        use crate::fts::graph_build::GraphLexicalError;
        self.step(0).map_err(GraphLexicalError::Resource)?;
        self.require_query(memory)
            .map_err(GraphLexicalError::Resource)?;
        match &mut self.control {
            TreeControl::Query { context, .. } => {
                context.decode_graph_lexical(bytes, epoch, memory)
            }
            TreeControl::Direct { .. } => Err(GraphLexicalError::Resource(TreeError::Invalid(
                "graph lexical query runtime required",
            ))),
        }
    }
    fn cursor_owner(&self) -> Result<CursorOwner<'a>, TreeError> {
        match (&self.owner, &self.control) {
            (CapacityOwner::Shared(_), TreeControl::Direct { .. }) => Ok(CursorOwner::Direct),
            (CapacityOwner::Preparation(memory), TreeControl::Direct { .. }) => {
                Ok(CursorOwner::Preparation(memory))
            }
            (
                CapacityOwner::Query(memory),
                TreeControl::Query {
                    identity: context, ..
                },
            ) => Ok(CursorOwner::Query(QueryOwner {
                memory,
                context: *context,
            })),
            _ => Err(TreeError::Invalid("tree resource owner mismatch")),
        }
    }
    fn require_cursor_owner(&self, expected: CursorOwner<'_>) -> Result<(), TreeError> {
        let actual = self.cursor_owner()?;
        match (expected, actual) {
            (CursorOwner::Direct, CursorOwner::Direct) => Ok(()),
            (CursorOwner::Preparation(expected), CursorOwner::Preparation(actual))
                if std::ptr::eq(expected, actual) =>
            {
                Ok(())
            }
            (CursorOwner::Query(expected), CursorOwner::Query(actual))
                if expected.same_owner(actual) =>
            {
                Ok(())
            }
            (CursorOwner::Query(_), _) => Err(TreeError::Invalid("query cursor owner mismatch")),
            _ => Err(TreeError::Invalid("storage cursor owner mismatch")),
        }
    }
    /// Conservative fixed operation workspace, separately from retained heap.
    pub fn reserved_bytes(&self) -> u64 {
        self.workspace.bytes() as u64
    }
    /// Poll before work and charge its complete checked amount.
    pub fn step(&mut self, units: u64) -> Result<(), TreeError> {
        if let Some(checkpoint) = self.preparation_checkpoint {
            checkpoint()?;
        }
        match &mut self.control {
            TreeControl::Direct { control, .. } => {
                control.checkpoint().map_err(TreeError::Control)?;
            }
            TreeControl::Query { context, .. } => {
                context.checkpoint().map_err(TreeError::Runtime)?;
            }
        }
        let next = self.work.checked_add(units).ok_or(TreeError::Work)?;
        if matches!(
            self.control,
            TreeControl::Direct { limit, .. } if next > limit
        ) {
            return Err(TreeError::Work);
        }
        self.work = next;
        Ok(())
    }
    pub(crate) fn read_event(&mut self, event: NativeReadEvent) -> Result<(), TreeError> {
        if let TreeControl::Query { context, .. } = &mut self.control {
            let (kind, units) = event.work();
            context.charge(kind, units).map_err(TreeError::Runtime)?;
        }
        Ok(())
    }
    /// Charges query-owned retrieval work to the same cumulative runtime. There
    /// is no silent direct-control path: scoring requires a query runtime.
    #[cfg(feature = "graph-cypher")]
    pub(crate) fn charge_query_work(
        &mut self,
        kind: WorkKind,
        units: u64,
    ) -> Result<(), TreeError> {
        match &mut self.control {
            TreeControl::Query { context, .. } => {
                context.charge(kind, units).map_err(TreeError::Runtime)
            }
            TreeControl::Direct { .. } => {
                Err(TreeError::Invalid("query work requires a query runtime"))
            }
        }
    }
    /// Exact charged work, including work completed before an error.
    pub const fn work(&self) -> u64 {
        self.work
    }
}

/// One physical directory root; this is not a coherent GraphStore admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DirectoryRoot {
    store: StoreInstanceId,
    kind: TreeKind,
    generation: GraphGeneration,
    reference: Option<PhysicalRef>,
}
impl DirectoryRoot {
    /// Explicit empty tree with no invented entity/artifact sentinel.
    pub const fn empty(
        store: StoreInstanceId,
        kind: TreeKind,
        generation: GraphGeneration,
    ) -> Self {
        Self {
            store,
            kind,
            generation,
            reference: None,
        }
    }
    /// Reconstruct a typed root descriptor after decoding a publication bundle.
    /// This validates reference shape only. Reads check visited paths; admission
    /// still requires the owning verifier and coherent retained view.
    pub fn from_reference(
        store: StoreInstanceId,
        kind: TreeKind,
        generation: GraphGeneration,
        reference: Option<PhysicalRef>,
    ) -> Result<Self, TreeError> {
        if let Some(reference) = reference {
            if reference.kind != BlockKind::TreePage || reference.version != 1 {
                return Err(TreeError::Invalid("root physical role/version"));
            }
            super::super::artifact::encode_reference(reference, &mut [0; 32])?;
        }
        Ok(Self {
            store,
            kind,
            generation,
            reference,
        })
    }
    /// Exact physical root, absent for an empty tree.
    pub const fn reference(self) -> Option<PhysicalRef> {
        self.reference
    }
    /// Store identity retained by every referenced artifact.
    pub const fn store(self) -> StoreInstanceId {
        self.store
    }
    /// Comparator domain owned by this root.
    pub const fn kind(self) -> TreeKind {
        self.kind
    }
    /// Admitted/prepared upper generation, not every shared page's creation time.
    pub const fn generation(self) -> GraphGeneration {
        self.generation
    }
}

// Cell descriptors and the copied current page have a bounded stack footprint.
// This conservative envelope is counted alongside the actual heap capacity.
const STACK_BYTES: usize = 256 * 1024;
const MAX_CELLS: usize = PAGE_BYTES / 28 + 1;
const MAX_PREPARE_BYTES: usize = 32 * 1024 * 1024;
/// Fixed-capacity reusable page output. The caller reserves this capacity inside
/// its writer/query and shared-store budgets before construction.
pub struct TreeScratch<'a> {
    // Free backing before releasing its shared reservation (field drop order).
    output: Vec<u8>,
    reservation: CapacityReservation<'a>,
}
impl<'a> TreeScratch<'a> {
    /// Fallibly allocate a single page and account its actual capacity. The
    /// operation context separately owns bounded stack/descriptor workspace.
    pub fn new(shared: &GraphResources, reserved_bytes: usize) -> Result<Self, TreeError> {
        if !(PAGE_BYTES..=MAX_PREPARE_BYTES).contains(&reserved_bytes) {
            return Err(TreeError::Memory);
        }
        let mut reservation = shared.reserve(PAGE_BYTES).map_err(|_| TreeError::Memory)?;
        let mut output = Vec::new();
        output
            .try_reserve_exact(PAGE_BYTES)
            .map_err(|_| TreeError::Memory)?;
        if output.capacity() > reserved_bytes {
            return Err(TreeError::Memory);
        }
        reservation
            .resize(output.capacity())
            .map_err(|_| TreeError::Memory)?;
        output.resize(PAGE_BYTES, 0);
        Ok(Self {
            output,
            reservation: CapacityReservation::Shared(reservation),
        })
    }
    /// Allocate fixed page backing inside the one complete storage allowance.
    pub fn for_prepare(memory: &'a StorageMemory<'a>) -> Result<Self, TreeError> {
        let mut reservation = memory.reserve(PAGE_BYTES)?;
        let mut output = Vec::new();
        #[cfg(feature = "allocation-audit")]
        let allocation =
            crate::allocation_audit::attributed(|| output.try_reserve_exact(PAGE_BYTES));
        #[cfg(not(feature = "allocation-audit"))]
        let allocation = output.try_reserve_exact(PAGE_BYTES);
        allocation.map_err(|_| TreeError::Memory)?;
        reservation.resize(output.capacity())?;
        memory.control().checkpoint().map_err(TreeError::Control)?;
        output.resize(PAGE_BYTES, 0);
        Ok(Self {
            output,
            reservation: CapacityReservation::Preparation(reservation),
        })
    }
    /// Complete actual owned heap reservation; never grows during an update.
    pub fn owned_bytes(&self) -> usize {
        self.reservation.bytes()
    }
    /// Actual heap backing capacity, separate from the conservative stack envelope.
    pub fn heap_bytes(&self) -> usize {
        self.output.capacity()
    }
}

fn checked_block<'a>(
    source: &'a impl BlockSource,
    root: DirectoryRoot,
    reference: PhysicalRef,
    resources: &mut TreeResources<'_>,
) -> Result<FramedBlock<'a>, TreeError> {
    resources.step(1)?;
    let block = source.resolve(reference, resources)?;
    resources.step(0)?;
    check_block(root, reference, block)
}
/// The same checked read as `checked_block`, with the block's backing scoped to
/// the callback. The owned result cannot retain that mapping's lifetime.
fn with_checked_block<S: BlockSource, R>(
    source: &S,
    root: DirectoryRoot,
    reference: PhysicalRef,
    resources: &mut TreeResources<'_>,
    callback: impl for<'a, 'r> FnOnce(
        FramedBlock<'a>,
        &'r mut TreeResources<'_>,
    ) -> Result<R, TreeError>,
) -> Result<R, TreeError> {
    resources.step(1)?;
    source.with_block(reference, resources, |block, resources| {
        resources.step(0)?;
        callback(check_block(root, reference, block)?, resources)
    })
}
fn check_block<'a>(
    root: DirectoryRoot,
    reference: PhysicalRef,
    block: FramedBlock<'a>,
) -> Result<FramedBlock<'a>, TreeError> {
    let identity = block.identity();
    if block.reference() != reference
        || identity.store != root.store
        || identity.artifact != reference.artifact
        || identity.generation.get() > root.generation.get()
        || reference.kind != BlockKind::TreePage
        || reference.version != 1
    {
        return Err(TreeError::Invalid(
            "substituted, future, or wrong-kind page",
        ));
    }
    Ok(block)
}
pub(crate) const MAX_DEPTH: usize = 64;
const INLINE_BYTES: usize = 512;

fn count(bytes: &[u8]) -> Result<usize, TreeError> {
    Ok(crate::format::frame::read_u32("graph directory", bytes, 12)? as usize)
}
mod fence_input;
mod keys;
use fence_input::ProbeKey;
pub use fence_input::{
    FenceKey, insert_fence, insert_fence_checked, lookup_fence, lookup_fence_entry,
};
use keys::{compare, validate_key};
fn checked_page<'a>(
    source: &impl BlockSource,
    root: DirectoryRoot,
    identity: ArtifactIdentity,
    bytes: &'a [u8],
    lower: Option<Key<'_>>,
    upper: Option<Key<'_>>,
    resources: &mut TreeResources<'_>,
) -> Result<super::FramedPage<'a>, TreeError> {
    resources.step(1)?;
    let page = decode_page(root.kind, bytes)?;
    if page.header().generation != identity.generation || page.header().level as usize >= MAX_DEPTH
    {
        return Err(TreeError::Invalid("page creation generation or depth"));
    }
    let mut previous = None;
    for index in 0..count(bytes)? {
        resources.step(1)?;
        let key = match owned_page_cell(bytes, page.header(), index)? {
            Cell::Leaf { key, .. } => Some(key),
            Cell::Branch { upper, .. } => upper,
        };
        if let Some(key) = key {
            validate_key(
                source,
                DirectoryRoot {
                    generation: identity.generation,
                    ..root
                },
                key,
                resources,
            )?;
            if let Some(old) = previous
                && !compare(source, root, old, key, resources)?.is_lt()
            {
                return Err(TreeError::Invalid("duplicate or unordered directory key"));
            }
            if let Some(lower) = lower {
                let order = compare(source, root, key, lower, resources)?;
                if order.is_lt() || (page.header().level > 0 && order.is_eq()) {
                    return Err(TreeError::Invalid("key below ancestor lower bound"));
                }
            }
            if let Some(upper) = upper
                && !compare(source, root, key, upper, resources)?.is_lt()
            {
                return Err(TreeError::Invalid("key exceeds ancestor upper bound"));
            }
            previous = Some(key);
        }
    }
    Ok(page)
}

/// Resolve and fully validate one directory page for a bounded descendant trace.
pub(crate) fn trace_page<'s>(
    source: &'s impl BlockSource,
    root: DirectoryRoot,
    reference: PhysicalRef,
    lower: Option<Key<'_>>,
    upper: Option<Key<'_>>,
    resources: &mut TreeResources<'_>,
) -> Result<(super::FramedPage<'s>, usize), TreeError> {
    let block = checked_block(source, root, reference, resources)?;
    let bytes = block.payload();
    let page = checked_page(
        source,
        root,
        block.identity(),
        bytes,
        lower,
        upper,
        resources,
    )?;
    let cells = count(bytes)?;
    Ok((page, cells))
}

/// Fully validate one directory page inside a scoped source callback and
/// return only caller-owned state. Captured trace sources release the mapping
/// before this function returns.
pub(crate) fn trace_page_scoped<R>(
    source: &impl BlockSource,
    root: DirectoryRoot,
    reference: PhysicalRef,
    lower: Option<Key<'_>>,
    upper: Option<Key<'_>>,
    resources: &mut TreeResources<'_>,
    callback: impl for<'a, 'r> FnOnce(
        super::FramedPage<'a>,
        ArtifactIdentity,
        usize,
        &'r mut TreeResources<'_>,
    ) -> Result<R, TreeError>,
) -> Result<R, TreeError> {
    resources.step(1)?;
    source.with_block(reference, resources, |block, resources| {
        resources.step(0)?;
        let block = check_block(root, reference, block)?;
        let identity = block.identity();
        let page = checked_page(
            source,
            root,
            identity,
            block.payload(),
            lower,
            upper,
            resources,
        )?;
        let cells = count(block.payload())?;
        callback(page, identity, cells, resources)
    })
}

#[derive(Clone, Copy)]
struct FixedLookupBound {
    bytes: [u8; 32],
    length: u8,
}

impl FixedLookupBound {
    fn copy(key: Key<'_>, width: usize) -> Result<Self, TreeError> {
        let Key::Inline(bytes) = key else {
            return Err(TreeError::Invalid("fixed lookup overflow key"));
        };
        if bytes.len() != width {
            return Err(TreeError::Invalid("fixed lookup key width"));
        }
        let mut output = [0_u8; 32];
        output
            .get_mut(..width)
            .ok_or(TreeError::Invalid("fixed lookup key extent"))?
            .copy_from_slice(bytes);
        Ok(Self {
            bytes: output,
            length: u8::try_from(width).map_err(|_| TreeError::Memory)?,
        })
    }

    fn key(&self) -> Result<Key<'_>, TreeError> {
        Ok(Key::Inline(
            self.bytes
                .get(..usize::from(self.length))
                .ok_or(TreeError::Invalid("fixed lookup bound extent"))?,
        ))
    }
}

enum FixedLookupStep {
    Found(usize),
    Absent,
    Child {
        reference: PhysicalRef,
        expected_level: u16,
        generation_bound: GraphGeneration,
        lower: Option<FixedLookupBound>,
        upper: Option<FixedLookupBound>,
    },
}

/// Copy one fixed-width trace-only value while releasing every page mapping
/// before following its next route. Ordinary borrowed lookups retain their
/// existing API and ownership semantics.
pub(crate) fn lookup_fixed_scoped(
    source: &impl BlockSource,
    root: DirectoryRoot,
    key: &[u8],
    output: &mut [u8],
    resources: &mut TreeResources<'_>,
) -> Result<Option<usize>, TreeError> {
    let width = match root.kind {
        TreeKind::Nodes | TreeKind::SparseMembership => 16,
        TreeKind::SparseSources => 32,
        _ => return Err(TreeError::Invalid("fixed lookup tree kind")),
    };
    if key.len() != width {
        return Err(TreeError::Invalid("fixed lookup probe width"));
    }
    resources.step(1)?;
    resources.read_event(NativeReadEvent::Lookup)?;
    validate_key(source, root, Key::Inline(key), resources)?;
    let Some(mut reference) = root.reference else {
        return Ok(None);
    };
    let mut expected_level = None;
    let mut generation_bound = root.generation;
    let mut lower = None;
    let mut upper = None;
    let mut ancestors = [None; MAX_DEPTH];
    let mut depth = 0_usize;
    loop {
        if ancestors
            .get(..depth)
            .is_some_and(|path| path.contains(&Some(reference)))
        {
            return Err(TreeError::Invalid("fixed lookup directory cycle"));
        }
        *ancestors
            .get_mut(depth)
            .ok_or(TreeError::Invalid("fixed lookup directory depth"))? = Some(reference);
        depth = depth.checked_add(1).ok_or(TreeError::Work)?;
        let lower_key = lower.as_ref().map(FixedLookupBound::key).transpose()?;
        let upper_key = upper.as_ref().map(FixedLookupBound::key).transpose()?;
        let step = trace_page_scoped(
            source,
            root,
            reference,
            lower_key,
            upper_key,
            resources,
            |page, _identity, cells, resources| {
                if expected_level.is_some_and(|level| level != page.header().level)
                    || page.header().generation > generation_bound
                {
                    return Err(TreeError::Invalid("fixed lookup child level or generation"));
                }
                if page.header().level == 0 {
                    for index in 0..cells {
                        resources.step(1)?;
                        let Cell::Leaf { key: stored, value } = page.cell(index)? else {
                            return Err(TreeError::Invalid("fixed lookup leaf cell"));
                        };
                        let Key::Inline(stored_bytes) = stored else {
                            return Err(TreeError::Invalid("fixed lookup overflow key"));
                        };
                        if stored_bytes.len() != width {
                            return Err(TreeError::Invalid("fixed lookup key width"));
                        }
                        match compare(source, root, Key::Inline(key), stored, resources)? {
                            std::cmp::Ordering::Equal => {
                                let destination =
                                    output.get_mut(..value.len()).ok_or(TreeError::Memory)?;
                                resources.step(value.len() as u64)?;
                                resources
                                    .read_event(NativeReadEvent::CopiedBytes(value.len() as u64))?;
                                destination.copy_from_slice(value);
                                return Ok(FixedLookupStep::Found(value.len()));
                            }
                            std::cmp::Ordering::Less => return Ok(FixedLookupStep::Absent),
                            std::cmp::Ordering::Greater => {}
                        }
                    }
                    return Ok(FixedLookupStep::Absent);
                }
                let mut selected = None;
                let mut prior = lower;
                for index in 0..cells {
                    resources.step(1)?;
                    let Cell::Branch {
                        upper: bound,
                        child,
                    } = page.cell(index)?
                    else {
                        return Err(TreeError::Invalid("fixed lookup branch cell"));
                    };
                    let copied_bound = bound
                        .map(|bound| FixedLookupBound::copy(bound, width))
                        .transpose()?;
                    if selected.is_none()
                        && (bound.is_none()
                            || compare(
                                source,
                                root,
                                Key::Inline(key),
                                bound.ok_or(TreeError::Invalid("fixed lookup bound"))?,
                                resources,
                            )?
                            .is_lt())
                    {
                        selected = Some((child, prior, copied_bound.or(upper)));
                        break;
                    }
                    prior = copied_bound;
                }
                let (child, child_lower, child_upper) =
                    selected.ok_or(TreeError::Invalid("fixed lookup missing branch route"))?;
                Ok(FixedLookupStep::Child {
                    reference: child,
                    expected_level: page
                        .header()
                        .level
                        .checked_sub(1)
                        .ok_or(TreeError::Invalid("fixed lookup branch level"))?,
                    generation_bound: page.header().generation,
                    lower: child_lower,
                    upper: child_upper,
                })
            },
        )?;
        match step {
            FixedLookupStep::Found(length) => return Ok(Some(length)),
            FixedLookupStep::Absent => return Ok(None),
            FixedLookupStep::Child {
                reference: child,
                expected_level: child_level,
                generation_bound: child_generation,
                lower: child_lower,
                upper: child_upper,
            } => {
                reference = child;
                expected_level = Some(child_level);
                generation_bound = child_generation;
                lower = child_lower;
                upper = child_upper;
            }
        }
    }
}

#[derive(Clone, Copy)]
struct TraceBound {
    page: PhysicalRef,
    cell: usize,
    generation: GraphGeneration,
}

#[derive(Clone, Copy)]
struct DirectoryTraceFrame {
    reference: PhysicalRef,
    expected_level: Option<u16>,
    generation_bound: GraphGeneration,
    next_cell: usize,
    cells: usize,
    emitted: bool,
    lower: Option<TraceBound>,
    upper: Option<TraceBound>,
}

#[derive(Clone, Copy)]
struct PayloadTraceState {
    payload: super::super::payload::PayloadRef,
    generation: GraphGeneration,
    next: usize,
}

/// Owned locator for one already validated leaf. It names the page and cell
/// only, so reopening it cannot switch roots or keep mapped bytes.
#[derive(Clone, Copy)]
pub(crate) struct DirectoryTraceLeaf {
    page: PhysicalRef,
    cell: usize,
    generation: GraphGeneration,
}

/// One resumable all-cell trace event.
pub(crate) enum DirectoryTraceEvent {
    Reference(PhysicalRef),
    Leaf(DirectoryTraceLeaf),
    Done,
}

/// Value-owned depth-first trace state. It retains no source, mapping, page,
/// key or directory entry between calls.
pub(crate) struct DirectoryTraceState<'m> {
    root: DirectoryRoot,
    frames: [Option<DirectoryTraceFrame>; MAX_DEPTH],
    depth: usize,
    payload: Option<PayloadTraceState>,
    leaf: Option<DirectoryTraceLeaf>,
    page: [u8; PAGE_BYTES],
    page_reference: Option<PhysicalRef>,
    page_header: Option<PageHeader>,
    failed: bool,
    done: bool,
    reservation: TreeTraceReservation<'m>,
}

impl<'m> DirectoryTraceState<'m> {
    pub(crate) fn new(
        root: DirectoryRoot,
        resources: &mut TreeResources<'m>,
    ) -> Result<Self, TreeError> {
        let reservation = resources.reserve_trace(std::mem::size_of::<Self>())?;
        let mut frames = [None; MAX_DEPTH];
        let depth = if let Some(reference) = root.reference {
            frames[0] = Some(DirectoryTraceFrame {
                reference,
                expected_level: None,
                generation_bound: root.generation,
                next_cell: 0,
                cells: 0,
                emitted: false,
                lower: None,
                upper: None,
            });
            1
        } else {
            0
        };
        Ok(Self {
            root,
            frames,
            depth,
            payload: None,
            leaf: None,
            page: [0_u8; PAGE_BYTES],
            page_reference: None,
            page_header: None,
            failed: false,
            done: depth == 0,
            reservation,
        })
    }

    pub(crate) fn next(
        &mut self,
        source: &impl BlockSource,
        resources: &mut TreeResources<'_>,
    ) -> Result<DirectoryTraceEvent, TreeError> {
        if self.failed {
            return Err(TreeError::Invalid("directory trace previously failed"));
        }
        let result = self.next_inner(source, resources);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn next_inner(
        &mut self,
        source: &impl BlockSource,
        resources: &mut TreeResources<'_>,
    ) -> Result<DirectoryTraceEvent, TreeError> {
        self.reservation.require(resources)?;
        resources.step(0)?;
        loop {
            if let Some(mut payload) = self.payload {
                match payload.payload.physical_reference_at_scoped(
                    source,
                    self.root.store,
                    payload.generation,
                    payload.next,
                    resources,
                )? {
                    Some(reference) => {
                        payload.next = payload.next.checked_add(1).ok_or(TreeError::Work)?;
                        self.payload = Some(payload);
                        return Ok(DirectoryTraceEvent::Reference(reference));
                    }
                    None => self.payload = None,
                }
            }
            if let Some(leaf) = self.leaf.take() {
                return Ok(DirectoryTraceEvent::Leaf(leaf));
            }
            if self.depth == 0 {
                self.done = true;
                return Ok(DirectoryTraceEvent::Done);
            }
            let index = self.depth - 1;
            let mut frame = self
                .frames
                .get(index)
                .copied()
                .flatten()
                .ok_or(TreeError::Invalid("directory trace frame"))?;
            if self.page_reference != Some(frame.reference) {
                let (header, cells) =
                    copy_inspect_trace_page(source, self.root, frame, &mut self.page, resources)?;
                if frame.emitted && frame.cells != cells {
                    return Err(TreeError::Invalid("directory trace page changed"));
                }
                self.page_reference = Some(frame.reference);
                self.page_header = Some(header);
            }
            let header = self
                .page_header
                .ok_or(TreeError::Invalid("directory trace page header"))?;
            if !frame.emitted {
                let cells = count(&self.page)?;
                if frame
                    .expected_level
                    .is_some_and(|expected| expected != header.level)
                    || header.generation.get() > frame.generation_bound.get()
                {
                    return Err(TreeError::Invalid(
                        "directory trace child level or generation",
                    ));
                }
                frame.emitted = true;
                frame.cells = cells;
                *self.frames.get_mut(index).ok_or(TreeError::Memory)? = Some(frame);
                return Ok(DirectoryTraceEvent::Reference(frame.reference));
            }
            if frame.next_cell == frame.cells {
                *self.frames.get_mut(index).ok_or(TreeError::Memory)? = None;
                self.depth -= 1;
                continue;
            }
            let cell_index = frame.next_cell;
            frame.next_cell = frame.next_cell.checked_add(1).ok_or(TreeError::Work)?;
            *self.frames.get_mut(index).ok_or(TreeError::Memory)? = Some(frame);
            let cell = owned_page_cell(&self.page, header, cell_index)?;
            let generation = header.generation;
            let payload = match cell {
                Cell::Leaf { key, .. }
                | Cell::Branch {
                    upper: Some(key), ..
                } => overflow_key_payload(key)?,
                Cell::Branch { upper: None, .. } => None,
            };
            let action = match cell {
                Cell::Leaf { .. } => TraceCellAction::Leaf {
                    locator: DirectoryTraceLeaf {
                        page: frame.reference,
                        cell: cell_index,
                        generation,
                    },
                    payload,
                },
                Cell::Branch { upper, child } => {
                    let current = upper.map(|_| TraceBound {
                        page: frame.reference,
                        cell: cell_index,
                        generation,
                    });
                    let lower = if cell_index == 0 {
                        frame.lower
                    } else {
                        Some(TraceBound {
                            page: frame.reference,
                            cell: cell_index - 1,
                            generation,
                        })
                    };
                    TraceCellAction::Branch {
                        child,
                        expected_level: header
                            .level
                            .checked_sub(1)
                            .ok_or(TreeError::Invalid("leaf branch cell"))?,
                        generation_bound: generation,
                        lower,
                        upper: current.or(frame.upper),
                        payload,
                    }
                }
            };
            match action {
                TraceCellAction::Leaf { locator, payload } => {
                    self.leaf = Some(locator);
                    self.payload = payload.map(|payload| PayloadTraceState {
                        payload,
                        generation: locator.generation,
                        next: 0,
                    });
                }
                TraceCellAction::Branch {
                    child,
                    expected_level,
                    generation_bound,
                    lower,
                    upper,
                    payload,
                } => {
                    if self.frames.get(..self.depth).is_some_and(|frames| {
                        frames
                            .iter()
                            .flatten()
                            .any(|ancestor| ancestor.reference == child)
                    }) {
                        return Err(TreeError::Invalid("directory trace cycle"));
                    }
                    *self
                        .frames
                        .get_mut(self.depth)
                        .ok_or(TreeError::Invalid("directory trace depth"))? =
                        Some(DirectoryTraceFrame {
                            reference: child,
                            expected_level: Some(expected_level),
                            generation_bound,
                            next_cell: 0,
                            cells: 0,
                            emitted: false,
                            lower,
                            upper,
                        });
                    self.depth += 1;
                    self.payload = payload.map(|payload| PayloadTraceState {
                        payload,
                        generation: generation_bound,
                        next: 0,
                    });
                }
            }
        }
    }

    /// Borrow a selected leaf from the currently retained copied page. The
    /// source mapping was released before the page entered this state.
    pub(crate) fn with_leaf<R>(
        &self,
        leaf: DirectoryTraceLeaf,
        resources: &mut TreeResources<'_>,
        callback: impl FnOnce(DirectoryEntry<'_>, &mut TreeResources<'_>) -> Result<R, TreeError>,
    ) -> Result<R, TreeError> {
        self.reservation.require(resources)?;
        if self.page_reference != Some(leaf.page) {
            return Err(TreeError::Invalid("directory trace leaf page not retained"));
        }
        let header = self
            .page_header
            .ok_or(TreeError::Invalid("directory trace leaf header"))?;
        if header.level != 0 || header.generation != leaf.generation {
            return Err(TreeError::Invalid("directory trace leaf owner"));
        }
        let Cell::Leaf { key, value } = owned_page_cell(&self.page, header, leaf.cell)? else {
            return Err(TreeError::Invalid("directory trace leaf cell"));
        };
        callback(
            DirectoryEntry {
                root: self.root,
                generation: leaf.generation,
                key,
                value,
            },
            resources,
        )
    }
}

#[derive(Clone, Copy)]
enum TraceCellAction {
    Leaf {
        locator: DirectoryTraceLeaf,
        payload: Option<super::super::payload::PayloadRef>,
    },
    Branch {
        child: PhysicalRef,
        expected_level: u16,
        generation_bound: GraphGeneration,
        lower: Option<TraceBound>,
        upper: Option<TraceBound>,
        payload: Option<super::super::payload::PayloadRef>,
    },
}

fn overflow_key_payload(
    key: Key<'_>,
) -> Result<Option<super::super::payload::PayloadRef>, TreeError> {
    match key {
        Key::Inline(_) => Ok(None),
        Key::Overflow {
            logical_length,
            reference,
        } => Ok(Some(super::super::payload::PayloadRef::new(
            BlockKind::OverflowKey,
            logical_length,
            reference,
        )?)),
    }
}

fn with_trace_bound<R>(
    source: &impl BlockSource,
    root: DirectoryRoot,
    bound: TraceBound,
    resources: &mut TreeResources<'_>,
    callback: impl for<'a, 'r> FnOnce(Key<'a>, &'r mut TreeResources<'_>) -> Result<R, TreeError>,
) -> Result<R, TreeError> {
    source.with_block(bound.page, resources, |block, resources| {
        let block = check_block(root, bound.page, block)?;
        let page = decode_page(root.kind, block.payload())?;
        if page.header().generation != bound.generation || page.header().level == 0 {
            return Err(TreeError::Invalid("directory trace bound owner"));
        }
        let Cell::Branch {
            upper: Some(key), ..
        } = page.cell(bound.cell)?
        else {
            return Err(TreeError::Invalid("directory trace bound cell"));
        };
        validate_key(
            source,
            DirectoryRoot {
                generation: bound.generation,
                ..root
            },
            key,
            resources,
        )?;
        callback(key, resources)
    })
}

fn copy_inspect_trace_page(
    source: &impl BlockSource,
    root: DirectoryRoot,
    frame: DirectoryTraceFrame,
    output: &mut [u8; PAGE_BYTES],
    resources: &mut TreeResources<'_>,
) -> Result<(PageHeader, usize), TreeError> {
    let identity = source.with_block(frame.reference, resources, |block, resources| {
        let block = check_block(root, frame.reference, block)?;
        if block.payload().len() != PAGE_BYTES {
            return Err(TreeError::Invalid("directory trace page width"));
        }
        resources.step(PAGE_BYTES as u64)?;
        resources.read_event(NativeReadEvent::CopiedBytes(PAGE_BYTES as u64))?;
        output.copy_from_slice(block.payload());
        Ok(block.identity())
    })?;
    let page = checked_page(source, root, identity, output, None, None, resources)?;
    let cells = count(output)?;
    for index in 0..cells {
        let key = match page.cell(index)? {
            Cell::Leaf { key, .. } => Some(key),
            Cell::Branch { upper, .. } => upper,
        };
        let Some(key) = key else {
            continue;
        };
        if let Some(lower) = frame.lower {
            with_trace_bound(source, root, lower, resources, |bound, resources| {
                let order = compare(source, root, key, bound, resources)?;
                if order.is_lt() || (page.header().level > 0 && order.is_eq()) {
                    return Err(TreeError::Invalid("key below ancestor lower bound"));
                }
                Ok(())
            })?;
        }
        if let Some(upper) = frame.upper {
            with_trace_bound(source, root, upper, resources, |bound, resources| {
                if !compare(source, root, key, bound, resources)?.is_lt() {
                    return Err(TreeError::Invalid("key exceeds ancestor upper bound"));
                }
                Ok(())
            })?;
        }
    }
    Ok((page.header(), cells))
}

#[derive(Clone, Copy)]
struct PathEntry {
    reference: PhysicalRef,
    child: usize,
    level: u16,
}
struct Path {
    entries: [Option<PathEntry>; MAX_DEPTH],
    len: usize,
}
impl Path {
    fn new() -> Self {
        Self {
            entries: [None; MAX_DEPTH],
            len: 0,
        }
    }
    fn get(&self, index: usize) -> Result<PathEntry, TreeError> {
        if index >= self.len {
            return Err(TreeError::Invalid("path index exceeds live extent"));
        }
        self.entries
            .get(index)
            .copied()
            .flatten()
            .ok_or(TreeError::Invalid("path extent"))
    }
    fn push(&mut self, entry: PathEntry) -> Result<(), TreeError> {
        *self
            .entries
            .get_mut(self.len)
            .ok_or(TreeError::Invalid("directory depth limit"))? = Some(entry);
        self.len += 1;
        Ok(())
    }
}
/// Route one probe to its leaf, optionally verifying every leaf entry and every
/// retained branch child on the way. Validator mode reads far more artifacts
/// than the path itself, so its incidental reads are scoped and released.
fn find_path<S: BlockSource>(
    source: &S,
    root: DirectoryRoot,
    key: ProbeKey<'_>,
    path: &mut Path,
    validator: Option<&mut dyn LeafValidator<S>>,
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    if validator.is_some() {
        return source
            .with_scoped_reads(|| find_path_inner(source, root, key, path, validator, resources));
    }
    find_path_inner(source, root, key, path, validator, resources)
}
fn find_path_inner<S: BlockSource>(
    source: &S,
    root: DirectoryRoot,
    key: ProbeKey<'_>,
    path: &mut Path,
    mut validator: Option<&mut dyn LeafValidator<S>>,
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    let Some(mut reference) = root.reference else {
        return Ok(());
    };
    let mut lower = None;
    let mut upper = None;
    let mut expected_level = None;
    let mut generation_bound = root.generation;
    loop {
        let block = checked_block(source, root, reference, resources)?;
        let bytes = block.payload();
        let page = checked_page(
            source,
            root,
            block.identity(),
            bytes,
            lower,
            upper,
            resources,
        )?;
        if expected_level.is_some_and(|level| level != page.header().level)
            || page.header().generation.get() > generation_bound.get()
        {
            return Err(TreeError::Invalid("child level or creation generation"));
        }
        if page.header().level == 0 {
            if let Some(validator) = validator.as_mut() {
                for index in 0..count(bytes)? {
                    resources.step(1)?;
                    let Cell::Leaf { key, value } = owned_page_cell(bytes, page.header(), index)?
                    else {
                        return Err(TreeError::Invalid("leaf required"));
                    };
                    validator.verify(
                        source,
                        root,
                        DirectoryEntry {
                            root,
                            generation: page.header().generation,
                            key,
                            value,
                        },
                        resources,
                    )?;
                }
            }
            path.push(PathEntry {
                reference,
                child: 0,
                level: 0,
            })?;
            return Ok(());
        }
        let mut selected = None;
        let mut prior = lower;
        for index in 0..count(bytes)? {
            resources.step(1)?;
            let Cell::Branch {
                upper: bound,
                child,
            } = owned_page_cell(bytes, page.header(), index)?
            else {
                return Err(TreeError::Invalid("branch cell expected"));
            };
            if validator.is_some() {
                // Every retained child must be legal under the old parent before
                // COW gives that same link a newer containing generation.
                let child_root = DirectoryRoot {
                    generation: page.header().generation,
                    ..root
                };
                let child_upper = bound.or(upper);
                let level = page.header().level;
                // The checked child page is released here: only the pages this
                // probe actually descends into stay mapped for the caller.
                with_checked_block(source, child_root, child, resources, |block, resources| {
                    let child_page = checked_page(
                        source,
                        child_root,
                        block.identity(),
                        block.payload(),
                        prior,
                        child_upper,
                        resources,
                    )?;
                    if child_page.header().level.checked_add(1) != Some(level) {
                        return Err(TreeError::Invalid("child level before directory rewrite"));
                    }
                    Ok(())
                })?;
            }
            if selected.is_none()
                && (bound.is_none()
                    || key
                        .compare_stored(
                            source,
                            root,
                            bound.ok_or(TreeError::Invalid("bound"))?,
                            resources,
                        )?
                        .is_lt())
            {
                selected = Some((index, child, prior, bound.or(upper)));
                if validator.is_none() {
                    break;
                }
            }
            prior = bound;
        }
        let (child_index, child, child_lower, child_upper) =
            selected.ok_or(TreeError::Invalid("missing branch route"))?;
        path.push(PathEntry {
            reference,
            child: child_index,
            level: page.header().level,
        })?;
        lower = child_lower;
        upper = child_upper;
        expected_level = page.header().level.checked_sub(1);
        generation_bound = page.header().generation;
        reference = child;
    }
}

/// Copy one compact leaf value only after a fully checked lookup. None means
/// absent; corrupt/missing objects and insufficient output remain typed errors.
pub fn lookup(
    source: &impl BlockSource,
    root: DirectoryRoot,
    key: &[u8],
    output: &mut [u8],
    resources: &mut TreeResources<'_>,
) -> Result<Option<usize>, TreeError> {
    lookup_probe(
        source,
        root,
        ProbeKey::Stored(Key::Inline(key)),
        output,
        resources,
    )
}
fn lookup_probe(
    source: &impl BlockSource,
    root: DirectoryRoot,
    key: ProbeKey<'_>,
    output: &mut [u8],
    resources: &mut TreeResources<'_>,
) -> Result<Option<usize>, TreeError> {
    let Some(entry) = lookup_probe_entry(source, root, key, resources)? else {
        return Ok(None);
    };
    let target = output
        .get_mut(..entry.value.len())
        .ok_or(TreeError::Memory)?;
    resources.step(entry.value.len() as u64)?;
    resources.read_event(NativeReadEvent::CopiedBytes(entry.value.len() as u64))?;
    target.copy_from_slice(entry.value);
    resources.step(0)?;
    Ok(Some(entry.value.len()))
}
/// Borrow a checked numeric/encoded-key entry while retaining its leaf generation.
pub fn lookup_entry<'a>(
    source: &'a impl BlockSource,
    root: DirectoryRoot,
    key: &[u8],
    resources: &mut TreeResources<'_>,
) -> Result<Option<DirectoryEntry<'a>>, TreeError> {
    lookup_probe_entry(source, root, ProbeKey::Stored(Key::Inline(key)), resources)
}
fn lookup_probe_entry<'a>(
    source: &'a impl BlockSource,
    root: DirectoryRoot,
    key: ProbeKey<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<Option<DirectoryEntry<'a>>, TreeError> {
    resources.step(1)?;
    resources.read_event(NativeReadEvent::Lookup)?;
    key.validate(source, root, resources)?;
    let mut path = Path::new();
    find_path(source, root, key, &mut path, None, resources)?;
    if path.len == 0 {
        return Ok(None);
    }
    let entry = path.get(path.len - 1)?;
    let block = checked_block(source, root, entry.reference, resources)?;
    let page = checked_page(
        source,
        root,
        block.identity(),
        block.payload(),
        None,
        None,
        resources,
    )?;
    for index in 0..count(block.payload())? {
        resources.step(1)?;
        let Cell::Leaf { key: stored, value } =
            owned_page_cell(block.payload(), page.header(), index)?
        else {
            return Err(TreeError::Invalid("leaf required"));
        };
        match key
            .compare_stored(source, root, stored, resources)?
            .reverse()
        {
            std::cmp::Ordering::Equal => {
                resources.step(0)?;
                return Ok(Some(DirectoryEntry {
                    root,
                    key: stored,
                    value,
                    generation: page.header().generation,
                }));
            }
            std::cmp::Ordering::Greater => return Ok(None),
            std::cmp::Ordering::Less => {}
        }
    }
    Ok(None)
}

#[allow(
    clippy::large_enum_variant,
    reason = "fixed charged stack workspace avoids a fallible heap allocation during path propagation"
)]
enum Separator {
    Inline {
        bytes: [u8; INLINE_BYTES],
        len: usize,
    },
    Overflow {
        logical_length: u64,
        reference: PhysicalRef,
    },
}
impl Separator {
    fn new(key: Key<'_>) -> Result<Self, TreeError> {
        match key {
            Key::Inline(raw) => {
                let mut bytes = [0; INLINE_BYTES];
                bytes
                    .get_mut(..raw.len())
                    .ok_or(TreeError::Memory)?
                    .copy_from_slice(raw);
                Ok(Self::Inline {
                    bytes,
                    len: raw.len(),
                })
            }
            Key::Overflow {
                logical_length,
                reference,
            } => Ok(Self::Overflow {
                logical_length,
                reference,
            }),
        }
    }
    fn key(&self) -> Result<Key<'_>, TreeError> {
        match self {
            Self::Inline { bytes, len } => {
                Ok(Key::Inline(bytes.get(..*len).ok_or(TreeError::Memory)?))
            }
            Self::Overflow {
                logical_length,
                reference,
            } => Ok(Key::Overflow {
                logical_length: *logical_length,
                reference: *reference,
            }),
        }
    }
}
fn normalize_cells(
    store: &mut impl BlockSink,
    root: DirectoryRoot,
    generation: GraphGeneration,
    cells: &mut [Cell<'_>],
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    for cell in cells {
        resources.step(1)?;
        let key = match cell {
            Cell::Leaf { key, .. } => key,
            Cell::Branch {
                upper: Some(key), ..
            } => key,
            Cell::Branch { upper: None, .. } => continue,
        };
        if let Key::Inline(bytes) = *key
            && bytes.len() > INLINE_BYTES
        {
            if root.kind != TreeKind::KeyFences {
                return Err(TreeError::Invalid("overflow comparator kind"));
            }
            let payload = super::super::payload::prepare_payload(
                store,
                root.store,
                generation,
                BlockKind::OverflowKey,
                bytes,
                resources,
            )?;
            *key = Key::Overflow {
                logical_length: payload.len(),
                reference: payload.reference(),
            };
        }
    }
    Ok(())
}
struct Replacement {
    left: PhysicalRef,
    split: Option<(Separator, PhysicalRef)>,
}
fn append_page(
    store: &mut impl BlockSink,
    root: DirectoryRoot,
    header: PageHeader,
    cells: &[Cell<'_>],
    scratch: &mut TreeScratch<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<PhysicalRef, TreeError> {
    resources.step(PAGE_BYTES as u64)?;
    let mut normalized = [Cell::Leaf {
        key: Key::Inline(&[]),
        value: &[],
    }; MAX_CELLS];
    let target = normalized.get_mut(..cells.len()).ok_or(TreeError::Memory)?;
    target.copy_from_slice(cells);
    normalize_cells(store, root, header.generation, target, resources)?;
    encode_page(header, target, &mut scratch.output)?;
    let reference = store.append(
        BlockKind::TreePage,
        header.generation,
        &scratch.output,
        resources,
    )?;
    let result = DirectoryRoot {
        generation: header.generation,
        reference: Some(reference),
        ..root
    };
    let block = checked_block(store, result, reference, resources)?;
    if block.identity().generation != header.generation || block.payload() != scratch.output {
        return Err(TreeError::Invalid("sink substituted prepared page"));
    }
    Ok(reference)
}
fn geometry(cells: &[Cell<'_>]) -> Result<usize, TreeError> {
    cells.iter().try_fold(64usize, |total, cell| {
        total
            .checked_add(
                super::cell_len(*cell)?
                    .checked_add(8)
                    .ok_or(TreeError::Memory)?,
            )
            .ok_or(TreeError::Memory)
    })
}
fn emit(
    store: &mut impl BlockSink,
    root: DirectoryRoot,
    header: PageHeader,
    cells: &mut [Cell<'_>],
    scratch: &mut TreeScratch<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<Replacement, TreeError> {
    normalize_cells(store, root, header.generation, cells, resources)?;
    if geometry(cells)? <= PAGE_BYTES {
        return Ok(Replacement {
            left: append_page(store, root, header, cells, scratch, resources)?,
            split: None,
        });
    }
    let mut best = None;
    let total = geometry(cells)? - 64;
    let mut used = 0usize;
    for split in 1..cells.len() {
        resources.step(1)?;
        let cell = *cells.get(split - 1).ok_or(TreeError::Memory)?;
        used = used
            .checked_add(super::cell_len(cell)? + 8)
            .ok_or(TreeError::Memory)?;
        let mut left = 64 + used;
        if let Cell::Branch {
            upper: Some(key), ..
        } = cell
        {
            left -= super::key_len(key)?;
        }
        let right = 64 + total - used;
        let difference = left.abs_diff(right);
        if left <= PAGE_BYTES && right <= PAGE_BYTES && best.is_none_or(|(_, old)| difference < old)
        {
            best = Some((split, difference));
        }
    }
    let split = best.ok_or(TreeError::Invalid("no bounded page split"))?.0;
    let separator = if header.level == 0 {
        let Cell::Leaf { key, .. } = *cells.get(split).ok_or(TreeError::Memory)? else {
            return Err(TreeError::Invalid("leaf split"));
        };
        Separator::new(key)?
    } else {
        let cell = cells.get_mut(split - 1).ok_or(TreeError::Memory)?;
        let Cell::Branch {
            upper: Some(key),
            child,
        } = *cell
        else {
            return Err(TreeError::Invalid("branch split bound"));
        };
        let separator = Separator::new(key)?;
        *cell = Cell::Branch { upper: None, child };
        separator
    };
    let left = append_page(
        store,
        root,
        header,
        cells.get(..split).ok_or(TreeError::Memory)?,
        scratch,
        resources,
    )?;
    let right = append_page(
        store,
        root,
        header,
        cells.get(split..).ok_or(TreeError::Memory)?,
        scratch,
        resources,
    )?;
    Ok(Replacement {
        left,
        split: Some((separator, right)),
    })
}
fn push<'a>(cells: &mut [Cell<'a>], count: &mut usize, cell: Cell<'a>) -> Result<(), TreeError> {
    *cells.get_mut(*count).ok_or(TreeError::Memory)? = cell;
    *count += 1;
    Ok(())
}
fn copy_page(
    source: &impl BlockSource,
    root: DirectoryRoot,
    reference: PhysicalRef,
    bytes: &mut [u8; PAGE_BYTES],
    resources: &mut TreeResources<'_>,
) -> Result<PageHeader, TreeError> {
    let block = checked_block(source, root, reference, resources)?;
    if block.payload().len() != PAGE_BYTES {
        return Err(TreeError::Invalid("page width"));
    }
    bytes.copy_from_slice(block.payload());
    let page = checked_page(source, root, block.identity(), bytes, None, None, resources)?;
    Ok(page.header())
}

/// Role-specific validation of every old entry copied by an immutable edit.
/// The entry retains its original root and leaf creation generation.
pub trait LeafValidator<S: BlockSource> {
    /// Reject any invalid old value before a replacement page is appended.
    fn verify(
        &mut self,
        source: &S,
        root: DirectoryRoot,
        entry: DirectoryEntry<'_>,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError>;
}
/// One private edit with its mandatory owning-role validator.
pub struct DirectoryMutation<V> {
    root: DirectoryRoot,
    generation: GraphGeneration,
    validator: V,
}
impl<V> DirectoryMutation<V> {
    /// Bind a validator to the exact original root and intended generation.
    pub const fn new(root: DirectoryRoot, generation: GraphGeneration, validator: V) -> Self {
        Self {
            root,
            generation,
            validator,
        }
    }
}
struct OpaqueValues;
impl<S: BlockSource> LeafValidator<S> for OpaqueValues {
    fn verify(
        &mut self,
        _: &S,
        _: DirectoryRoot,
        _: DirectoryEntry<'_>,
        _: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        Ok(())
    }
}
/// Prepare immutable replacement pages for opaque primitive values. Production
/// directory roles must use `insert_checked` with their complete value verifier.
pub fn insert(
    store: &mut impl BlockSink,
    root: DirectoryRoot,
    key: &[u8],
    value: &[u8],
    generation: GraphGeneration,
    scratch: &mut TreeScratch<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<DirectoryRoot, TreeError> {
    insert_checked(
        store,
        DirectoryMutation::new(root, generation, OpaqueValues),
        key,
        value,
        scratch,
        resources,
    )
}
/// Validate every copied old leaf value before preparing replacement pages.
pub fn insert_checked<S: BlockSink>(
    store: &mut S,
    mutation: DirectoryMutation<impl LeafValidator<S>>,
    key: &[u8],
    value: &[u8],
    scratch: &mut TreeScratch<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<DirectoryRoot, TreeError> {
    insert_key(
        store,
        mutation,
        InsertionKey {
            stored: Key::Inline(key),
            probe: ProbeKey::Stored(Key::Inline(key)),
        },
        value,
        scratch,
        resources,
    )
}
struct InsertionKey<'a> {
    stored: Key<'a>,
    probe: ProbeKey<'a>,
}
fn insert_key<S: BlockSink>(
    store: &mut S,
    mut mutation: DirectoryMutation<impl LeafValidator<S>>,
    key: InsertionKey<'_>,
    value: &[u8],
    scratch: &mut TreeScratch<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<DirectoryRoot, TreeError> {
    let (root, generation) = (mutation.root, mutation.generation);
    resources.step(1)?;
    key.probe.validate(store, root, resources)?;
    let prepared_root = DirectoryRoot { generation, ..root };
    validate_key(store, prepared_root, key.stored, resources)?;
    if !key
        .probe
        .compare_stored(store, prepared_root, key.stored, resources)?
        .is_eq()
    {
        return Err(TreeError::Invalid("prepared key differs from probe"));
    }
    if generation.get() < root.generation.get() {
        return Err(TreeError::Invalid("generation regressed"));
    }
    let mut path = Path::new();
    find_path(
        store,
        root,
        key.probe,
        &mut path,
        Some(&mut mutation.validator),
        resources,
    )?;
    let mut old_bytes = [0u8; PAGE_BYTES];
    let mut cells = [Cell::Leaf {
        key: Key::Inline(&[]),
        value: &[],
    }; MAX_CELLS];
    let mut count = 0usize;
    let mut inserted = false;
    if path.len > 0 {
        let entry = path.get(path.len - 1)?;
        let header = copy_page(store, root, entry.reference, &mut old_bytes, resources)?;
        for index in 0..self::count(&old_bytes)? {
            resources.step(1)?;
            let cell = owned_page_cell(&old_bytes, header, index)?;
            let Cell::Leaf { key: stored, .. } = cell else {
                return Err(TreeError::Invalid("leaf required"));
            };
            let order = key
                .probe
                .compare_stored(store, root, stored, resources)?
                .reverse();
            if !inserted && !order.is_lt() {
                push(
                    &mut cells,
                    &mut count,
                    Cell::Leaf {
                        key: key.stored,
                        value,
                    },
                )?;
                inserted = true;
            }
            if !order.is_eq() {
                push(&mut cells, &mut count, cell)?;
            }
        }
    }
    if !inserted {
        push(
            &mut cells,
            &mut count,
            Cell::Leaf {
                key: key.stored,
                value,
            },
        )?;
    }
    let header = PageHeader {
        kind: root.kind,
        level: 0,
        generation,
    };
    let mut replacement = emit(
        store,
        root,
        header,
        cells.get_mut(..count).ok_or(TreeError::Memory)?,
        scratch,
        resources,
    )?;
    for position in (0..path.len.saturating_sub(1)).rev() {
        let entry = path.get(position)?;
        let mut parent_bytes = [0u8; PAGE_BYTES];
        let old_header = copy_page(store, root, entry.reference, &mut parent_bytes, resources)?;
        let mut parent_cells = [Cell::Leaf {
            key: Key::Inline(&[]),
            value: &[],
        }; MAX_CELLS];
        let mut parent_count = 0usize;
        for index in 0..self::count(&parent_bytes)? {
            let cell = owned_page_cell(&parent_bytes, old_header, index)?;
            let Cell::Branch { upper, .. } = cell else {
                return Err(TreeError::Invalid("branch required"));
            };
            if index == entry.child {
                if let Some((separator, right)) = &replacement.split {
                    push(
                        &mut parent_cells,
                        &mut parent_count,
                        Cell::Branch {
                            upper: Some(separator.key()?),
                            child: replacement.left,
                        },
                    )?;
                    push(
                        &mut parent_cells,
                        &mut parent_count,
                        Cell::Branch {
                            upper,
                            child: *right,
                        },
                    )?;
                } else {
                    push(
                        &mut parent_cells,
                        &mut parent_count,
                        Cell::Branch {
                            upper,
                            child: replacement.left,
                        },
                    )?;
                }
            } else {
                push(&mut parent_cells, &mut parent_count, cell)?;
            }
        }
        replacement = emit(
            store,
            root,
            PageHeader {
                generation,
                ..old_header
            },
            parent_cells
                .get_mut(..parent_count)
                .ok_or(TreeError::Memory)?,
            scratch,
            resources,
        )?;
    }
    let reference = if let Some((separator, right)) = &replacement.split {
        let old_level = if path.len > 0 { path.get(0)?.level } else { 0 };
        let level = old_level
            .checked_add(1)
            .ok_or(TreeError::Invalid("root level overflow"))?;
        if level as usize >= MAX_DEPTH {
            return Err(TreeError::Invalid("directory depth limit"));
        }
        append_page(
            store,
            root,
            PageHeader {
                kind: root.kind,
                level,
                generation,
            },
            &[
                Cell::Branch {
                    upper: Some(separator.key()?),
                    child: replacement.left,
                },
                Cell::Branch {
                    upper: None,
                    child: *right,
                },
            ],
            scratch,
            resources,
        )?
    } else {
        replacement.left
    };
    resources.step(0)?;
    Ok(DirectoryRoot {
        generation,
        reference: Some(reference),
        ..root
    })
}

// Existing framing owns slot geometry; this helper preserves the copied bytes'
// lifetime after a temporary FramedPage has been discarded.
fn owned_page_cell(bytes: &[u8], header: PageHeader, index: usize) -> Result<Cell<'_>, TreeError> {
    let slot = index
        .checked_mul(8)
        .and_then(|n| n.checked_add(64))
        .ok_or(TreeError::Memory)?;
    let offset = crate::format::frame::read_u32("graph directory", bytes, slot)? as usize;
    let length = crate::format::frame::read_u32("graph directory", bytes, slot + 4)? as usize;
    let end = offset.checked_add(length).ok_or(TreeError::Memory)?;
    Ok(super::parse_cell(
        header,
        bytes
            .get(offset..end)
            .ok_or(TreeError::Invalid("cell extent"))?,
    )?)
}

/// Remove a compact entry from a private tree candidate. Identity-owned fence
/// retention and endpoint tombstones are enforced by the graph-record participant,
/// which must never translate logical entity deletion into key-ledger removal.
pub fn remove(
    store: &mut impl BlockSink,
    root: DirectoryRoot,
    key: &[u8],
    generation: GraphGeneration,
    scratch: &mut TreeScratch<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<DirectoryRoot, TreeError> {
    remove_checked(
        store,
        DirectoryMutation::new(root, generation, OpaqueValues),
        key,
        scratch,
        resources,
    )
}
/// Validate retained old leaf values before removing one entry from a private candidate.
pub fn remove_checked<S: BlockSink>(
    store: &mut S,
    mut mutation: DirectoryMutation<impl LeafValidator<S>>,
    key: &[u8],
    scratch: &mut TreeScratch<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<DirectoryRoot, TreeError> {
    let (root, generation) = (mutation.root, mutation.generation);
    resources.step(1)?;
    validate_key(store, root, Key::Inline(key), resources)?;
    if generation.get() < root.generation.get() {
        return Err(TreeError::Invalid("generation regressed"));
    }
    let mut path = Path::new();
    find_path(
        store,
        root,
        ProbeKey::Stored(Key::Inline(key)),
        &mut path,
        Some(&mut mutation.validator),
        resources,
    )?;
    if path.len == 0 {
        return Ok(root);
    }
    let mut old_bytes = [0u8; PAGE_BYTES];
    let leaf = path.get(path.len - 1)?;
    let header = copy_page(store, root, leaf.reference, &mut old_bytes, resources)?;
    let mut cells = [Cell::Leaf {
        key: Key::Inline(&[]),
        value: &[],
    }; MAX_CELLS];
    let mut length = 0usize;
    let mut found = false;
    for index in 0..count(&old_bytes)? {
        let cell = owned_page_cell(&old_bytes, header, index)?;
        let Cell::Leaf { key: stored, .. } = cell else {
            return Err(TreeError::Invalid("leaf required"));
        };
        if compare(store, root, stored, Key::Inline(key), resources)?.is_eq() {
            found = true;
        } else {
            push(&mut cells, &mut length, cell)?;
        }
    }
    if !found {
        return Ok(root);
    }
    let mut collapse_expected = (0u16, generation);
    let mut replacement = if length == 0 {
        None
    } else {
        Some(append_page(
            store,
            root,
            PageHeader {
                generation,
                ..header
            },
            cells.get(..length).ok_or(TreeError::Memory)?,
            scratch,
            resources,
        )?)
    };
    for position in (0..path.len.saturating_sub(1)).rev() {
        let entry = path.get(position)?;
        let mut parent_bytes = [0u8; PAGE_BYTES];
        let old_header = copy_page(store, root, entry.reference, &mut parent_bytes, resources)?;
        let mut parent_cells = [Cell::Leaf {
            key: Key::Inline(&[]),
            value: &[],
        }; MAX_CELLS];
        let mut parent_count = 0usize;
        for index in 0..count(&parent_bytes)? {
            resources.step(1)?;
            let cell = owned_page_cell(&parent_bytes, old_header, index)?;
            let Cell::Branch { upper, .. } = cell else {
                return Err(TreeError::Invalid("branch required"));
            };
            if index == entry.child {
                if let Some(child) = replacement {
                    push(
                        &mut parent_cells,
                        &mut parent_count,
                        Cell::Branch { upper, child },
                    )?;
                }
            } else {
                push(&mut parent_cells, &mut parent_count, cell)?;
            }
        }
        replacement = if parent_count == 0 {
            None
        } else {
            let last = parent_cells
                .get_mut(parent_count - 1)
                .ok_or(TreeError::Memory)?;
            let Cell::Branch { child, .. } = *last else {
                return Err(TreeError::Invalid("final child"));
            };
            *last = Cell::Branch { upper: None, child };
            if position == 0 && parent_count == 1 {
                // An untouched sibling retains the old parent's generation bound;
                // a privately replaced child may be from this preparation.
                let bound = if replacement == Some(child) {
                    generation
                } else {
                    old_header.generation
                };
                collapse_expected = (
                    old_header
                        .level
                        .checked_sub(1)
                        .ok_or(TreeError::Invalid("collapse leaf parent"))?,
                    bound,
                );
                Some(child)
            } else {
                if position == 0 {
                    collapse_expected = (old_header.level, generation);
                }
                Some(append_page(
                    store,
                    root,
                    PageHeader {
                        generation,
                        ..old_header
                    },
                    parent_cells.get(..parent_count).ok_or(TreeError::Memory)?,
                    scratch,
                    resources,
                )?)
            }
        };
    }
    // Repeated singleton roots can remain after sparse subtree removal.
    let mut collapse_depth = 0usize;
    while let Some(reference) = replacement {
        if collapse_depth >= MAX_DEPTH {
            return Err(TreeError::Invalid("collapse depth limit"));
        }
        collapse_depth += 1;
        let candidate = DirectoryRoot {
            generation,
            reference: Some(reference),
            ..root
        };
        let block = checked_block(store, candidate, reference, resources)?;
        let page = checked_page(
            store,
            candidate,
            block.identity(),
            block.payload(),
            None,
            None,
            resources,
        )?;
        if page.header().level != collapse_expected.0
            || page.header().generation > collapse_expected.1
        {
            return Err(TreeError::Invalid("collapse child level/generation"));
        }
        if page.header().level == 0 || count(block.payload())? != 1 {
            break;
        }
        let Cell::Branch { upper: None, child } = page.cell(0)? else {
            return Err(TreeError::Invalid("singleton root"));
        };
        collapse_expected = (
            page.header()
                .level
                .checked_sub(1)
                .ok_or(TreeError::Invalid("collapse level underflow"))?,
            page.header().generation,
        );
        replacement = Some(child);
        resources.step(1)?;
    }
    resources.step(0)?;
    Ok(DirectoryRoot {
        generation,
        reference: replacement,
        ..root
    })
}

/// Find the greatest key at or below the exact probe without scanning preceding
/// leaves. The returned entry retains its actual containing leaf generation.
pub fn lookup_predecessor<'a>(
    source: &'a impl BlockSource,
    root: DirectoryRoot,
    key: &[u8],
    resources: &mut TreeResources<'_>,
) -> Result<Option<DirectoryEntry<'a>>, TreeError> {
    resources.read_event(NativeReadEvent::Lookup)?;
    let mut cursor = DirectoryCursor::seek(source, root, Some(key), resources)?;
    if cursor.exhausted {
        resources.step(0)?;
        return Ok(None);
    }
    if cursor.index < cursor.leaf_count {
        let index = cursor.index;
        let entry = cursor
            .next_entry(resources)?
            .ok_or(TreeError::Invalid("predecessor selected leaf is empty"))?;
        if compare(source, root, entry.key(), Key::Inline(key), resources)?.is_eq() {
            resources.step(0)?;
            return Ok(Some(entry));
        }
        cursor.index = index;
    }
    if cursor.index == 0 && !cursor.retreat(resources)? {
        resources.step(0)?;
        return Ok(None);
    }
    cursor.index = cursor
        .index
        .checked_sub(1)
        .ok_or(TreeError::Invalid("predecessor index underflow"))?;
    cursor.next_entry(resources)
}

/// Generation-bound ancestor cursor. It borrows one immutable source for its
/// entire lifetime and never switches providers or follows mutable sibling links.
/// An error latches permanently; there is no continuation with partial success.
pub struct DirectoryCursor<'a, 'm, S> {
    source: &'a S,
    root: DirectoryRoot,
    path: Path,
    index: usize,
    leaf_count: usize,
    exhausted: bool,
    failed: bool,
    owner: CursorOwner<'m>,
    reservation: CapacityReservation<'m>,
}
impl<'a, 'm, S: BlockSource> DirectoryCursor<'a, 'm, S> {
    /// Seek the first key at or above lower, or the first entry when absent.
    /// The cursor owns its fixed-size shared reservation; the source owns its lease.
    pub fn seek(
        source: &'a S,
        root: DirectoryRoot,
        lower: Option<&[u8]>,
        resources: &mut TreeResources<'m>,
    ) -> Result<Self, TreeError> {
        Self::seek_stored(source, root, lower.map(Key::Inline), resources)
    }

    /// Seek from an already validated stored-key descriptor. This keeps a
    /// bounded overflow-key resume token between scoped mapping windows.
    pub(crate) fn seek_stored(
        source: &'a S,
        root: DirectoryRoot,
        lower: Option<Key<'_>>,
        resources: &mut TreeResources<'m>,
    ) -> Result<Self, TreeError> {
        resources.read_event(NativeReadEvent::Scan)?;
        resources.step(1)?;
        if let Some(key) = lower {
            validate_key(source, root, key, resources)?;
        }
        let owner = resources.cursor_owner()?;
        let reservation = resources.owner.reserve(std::mem::size_of::<Self>())?;
        let mut cursor = Self {
            source,
            root,
            path: Path::new(),
            index: 0,
            leaf_count: 0,
            exhausted: root.reference.is_none(),
            failed: false,
            owner,
            reservation,
        };
        if cursor.exhausted {
            return Ok(cursor);
        }
        if let Some(key) = lower {
            find_path(
                source,
                root,
                ProbeKey::Stored(key),
                &mut cursor.path,
                None,
                resources,
            )?;
        } else if let Some(reference) = root.reference {
            cursor.descend_left(reference, resources)?;
        }
        cursor.leaf_count = cursor.validate_path(resources)?;
        if let Some(lower) = lower {
            let entry = cursor.path.get(cursor.path.len - 1)?;
            let block = checked_block(source, root, entry.reference, resources)?;
            let page = decode_page(root.kind, block.payload())?;
            while cursor.index < cursor.leaf_count {
                let Cell::Leaf { key, .. } = page.cell(cursor.index)? else {
                    return Err(TreeError::Invalid("cursor leaf expected"));
                };
                if !compare(source, root, key, lower, resources)?.is_lt() {
                    break;
                }
                cursor.index += 1;
            }
        }
        Ok(cursor)
    }
    /// Fixed cursor-owned bytes, excluding the separately charged source lease.
    pub fn owned_bytes(&self) -> usize {
        self.reservation.bytes()
    }
    /// Copy one whole compact entry or return a typed error. Buffers remain
    /// caller scratch and no failed row is returned. Errors permanently latch.
    pub fn next(
        &mut self,
        key_output: &mut [u8],
        value_output: &mut [u8],
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<(usize, usize)>, TreeError> {
        if self.failed {
            return Err(TreeError::Invalid("cursor previously failed"));
        }
        let result = self.next_inner(key_output, value_output, resources);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn next_inner(
        &mut self,
        key_output: &mut [u8],
        value_output: &mut [u8],
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<(usize, usize)>, TreeError> {
        let Some(entry) = self.entry_inner(resources)? else {
            return Ok(None);
        };
        let key_length = keys::length(entry.key)?;
        if key_length > key_output.len() || entry.value.len() > value_output.len() {
            return Err(TreeError::Memory);
        }
        keys::copy(self.source, self.root, entry.key, key_output, resources)?;
        let target = value_output
            .get_mut(..entry.value.len())
            .ok_or(TreeError::Memory)?;
        resources.step(entry.value.len() as u64)?;
        resources.read_event(NativeReadEvent::CopiedBytes(entry.value.len() as u64))?;
        target.copy_from_slice(entry.value);
        resources.step(0)?;
        Ok(Some((key_length, entry.value.len())))
    }
    /// Yield one borrowed compact entry without materializing an overflow key.
    /// Its backing cannot outlive the one immutable source borrowed by this cursor.
    /// Required record/value interpretation still belongs to the leaf-role owner.
    pub fn next_entry(
        &mut self,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<DirectoryEntry<'a>>, TreeError> {
        if self.failed {
            return Err(TreeError::Invalid("cursor previously failed"));
        }
        let result = self.entry_inner(resources);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn entry_inner(
        &mut self,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<DirectoryEntry<'a>>, TreeError> {
        resources.require_cursor_owner(self.owner)?;
        resources.step(1)?;
        if self.exhausted {
            return Ok(None);
        }
        while self.index == self.leaf_count {
            if !self.advance(resources)? {
                self.exhausted = true;
                return Ok(None);
            }
        }
        let entry = self.path.get(self.path.len - 1)?;
        let block = checked_block(self.source, self.root, entry.reference, resources)?;
        let header = decode_page(self.root.kind, block.payload())?.header();
        let Cell::Leaf { key, value } = owned_page_cell(block.payload(), header, self.index)?
        else {
            return Err(TreeError::Invalid("cursor leaf expected"));
        };
        resources.step(0)?;
        self.index += 1;
        Ok(Some(DirectoryEntry {
            root: self.root,
            key,
            value,
            generation: header.generation,
        }))
    }
    fn descend_left(
        &mut self,
        mut reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        loop {
            let block = checked_block(self.source, self.root, reference, resources)?;
            let page = checked_page(
                self.source,
                self.root,
                block.identity(),
                block.payload(),
                None,
                None,
                resources,
            )?;
            self.path.push(PathEntry {
                reference,
                child: 0,
                level: page.header().level,
            })?;
            if page.header().level == 0 {
                return Ok(());
            }
            let Cell::Branch { child, .. } = page.cell(0)? else {
                return Err(TreeError::Invalid("cursor branch expected"));
            };
            reference = child;
        }
    }
    fn validate_path(&self, resources: &mut TreeResources<'_>) -> Result<usize, TreeError> {
        let mut lower = None;
        let mut upper = None;
        let mut generation_bound = self.root.generation;
        let mut expected_level = None;
        for depth in 0..self.path.len {
            let entry = self.path.get(depth)?;
            let block = checked_block(self.source, self.root, entry.reference, resources)?;
            let page = checked_page(
                self.source,
                self.root,
                block.identity(),
                block.payload(),
                lower,
                upper,
                resources,
            )?;
            if expected_level.is_some_and(|level| level != page.header().level)
                || page.header().level != entry.level
                || page.header().generation.get() > generation_bound.get()
            {
                return Err(TreeError::Invalid("cursor child level/generation"));
            }
            if page.header().level == 0 {
                if depth + 1 != self.path.len {
                    return Err(TreeError::Invalid("cursor leaf path suffix"));
                }
                return count(block.payload());
            }
            let Cell::Branch {
                upper: next_upper,
                child,
            } = owned_page_cell(block.payload(), page.header(), entry.child)?
            else {
                return Err(TreeError::Invalid("cursor branch child"));
            };
            if child != self.path.get(depth + 1)?.reference {
                return Err(TreeError::Invalid("cursor child path differs"));
            }
            if entry.child > 0 {
                let Cell::Branch { upper: prior, .. } =
                    owned_page_cell(block.payload(), page.header(), entry.child - 1)?
                else {
                    return Err(TreeError::Invalid("cursor predecessor"));
                };
                lower = prior;
            }
            upper = next_upper.or(upper);
            expected_level = page.header().level.checked_sub(1);
            generation_bound = page.header().generation;
        }
        Err(TreeError::Invalid("cursor has no leaf"))
    }
    fn descend_right(
        &mut self,
        mut reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        loop {
            let block = checked_block(self.source, self.root, reference, resources)?;
            let page = checked_page(
                self.source,
                self.root,
                block.identity(),
                block.payload(),
                None,
                None,
                resources,
            )?;
            let last = count(block.payload())?
                .checked_sub(1)
                .ok_or(TreeError::Invalid("cursor empty page"))?;
            self.path.push(PathEntry {
                reference,
                child: if page.header().level == 0 { 0 } else { last },
                level: page.header().level,
            })?;
            if page.header().level == 0 {
                return Ok(());
            }
            let Cell::Branch { child, .. } = page.cell(last)? else {
                return Err(TreeError::Invalid("cursor branch expected"));
            };
            reference = child;
        }
    }
    fn retreat(&mut self, resources: &mut TreeResources<'_>) -> Result<bool, TreeError> {
        for depth in (0..self.path.len.saturating_sub(1)).rev() {
            let mut entry = self.path.get(depth)?;
            let Some(prior) = entry.child.checked_sub(1) else {
                continue;
            };
            let block = checked_block(self.source, self.root, entry.reference, resources)?;
            let page = decode_page(self.root.kind, block.payload())?;
            let Cell::Branch { child, .. } = page.cell(prior)? else {
                return Err(TreeError::Invalid("cursor predecessor"));
            };
            entry.child = prior;
            *self
                .path
                .entries
                .get_mut(depth)
                .ok_or(TreeError::Invalid("path extent"))? = Some(entry);
            self.path.len = depth + 1;
            self.descend_right(child, resources)?;
            self.leaf_count = self.validate_path(resources)?;
            self.index = self.leaf_count;
            return Ok(true);
        }
        Ok(false)
    }
    fn advance(&mut self, resources: &mut TreeResources<'_>) -> Result<bool, TreeError> {
        for depth in (0..self.path.len.saturating_sub(1)).rev() {
            let mut entry = self.path.get(depth)?;
            let block = checked_block(self.source, self.root, entry.reference, resources)?;
            let page = decode_page(self.root.kind, block.payload())?;
            let next = entry
                .child
                .checked_add(1)
                .ok_or(TreeError::Invalid("child index overflow"))?;
            if next < count(block.payload())? {
                let Cell::Branch { child, .. } = page.cell(next)? else {
                    return Err(TreeError::Invalid("cursor successor"));
                };
                entry.child = next;
                *self
                    .path
                    .entries
                    .get_mut(depth)
                    .ok_or(TreeError::Invalid("path extent"))? = Some(entry);
                self.path.len = depth + 1;
                self.descend_left(child, resources)?;
                self.leaf_count = self.validate_path(resources)?;
                self.index = 0;
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// A compact immutable leaf entry; overflow bytes are a source-bound descriptor.
#[derive(Clone, Copy, Debug)]
pub struct DirectoryEntry<'a> {
    root: DirectoryRoot,
    generation: GraphGeneration,
    key: Key<'a>,
    value: &'a [u8],
}
impl<'a> DirectoryEntry<'a> {
    pub(crate) fn require_root(self, root: DirectoryRoot) -> Result<(), TreeError> {
        if self.root != root {
            return Err(TreeError::Invalid(
                "directory entry belongs to another root",
            ));
        }
        Ok(())
    }
    /// Actual containing leaf creation generation, bounding all descendant payloads.
    pub const fn creation_generation(self) -> GraphGeneration {
        self.generation
    }
    /// Exact inline bytes or validated overflow descriptor; no truncated key.
    pub const fn key(self) -> Key<'a> {
        self.key
    }
    /// Borrowed compact value. Its owning tree role must validate its semantics.
    pub const fn value(self) -> &'a [u8] {
        self.value
    }
}

/// Verify all paths and exact key streams, invoking the mandatory leaf-role
/// validator before accepting each entry. No directory-size map or key copy is
/// allocated. This validates one tree only; the coordinator owns coherent roots.
pub fn verify_directory<'a>(
    source: &'a impl BlockSource,
    root: DirectoryRoot,
    resources: &mut TreeResources<'_>,
    validator: &mut impl FnMut(DirectoryEntry<'a>, &mut TreeResources<'_>) -> Result<(), TreeError>,
) -> Result<u64, TreeError> {
    // The validator reads one record artifact per entry. Those reads are scoped
    // and released; only the leaf pages under the cursor stay mapped.
    source.with_scoped_reads(|| {
        let mut cursor = DirectoryCursor::seek(source, root, None, resources)?;
        let mut entries = 0u64;
        while let Some(entry) = cursor.next_entry(resources)? {
            validator(entry, resources)?;
            resources.step(1)?;
            entries = entries.checked_add(1).ok_or(TreeError::Work)?;
        }
        resources.step(0)?;
        Ok(entries)
    })
}

mod roots;
pub use roots::GraphRoots;
