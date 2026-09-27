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
    _reservation: CapacityReservation<'a>,
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
            _reservation: reservation,
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
mod batch;
pub(crate) use batch::DirectoryBatch;
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
/// One edit in a strictly comparator-ordered, duplicate-free bulk mutation.
#[derive(Clone, Copy)]
pub enum DirectoryOp<'a> {
    /// Insert or replace the exact key's value.
    Insert {
        /// Logical key bytes in this tree's comparator domain.
        key: &'a [u8],
        /// Opaque value admitted by the owning role.
        value: &'a [u8],
    },
    /// Remove the key if present.
    Remove {
        /// Logical key bytes in this tree's comparator domain.
        key: &'a [u8],
    },
}
impl DirectoryOp<'_> {
    fn key(&self) -> Key<'_> {
        match self {
            Self::Insert { key, .. } | Self::Remove { key } => Key::Inline(key),
        }
    }
    fn cell(&self) -> Option<Cell<'_>> {
        match self {
            Self::Insert { key, value } => Some(Cell::Leaf {
                key: Key::Inline(key),
                value,
            }),
            Self::Remove { .. } => None,
        }
    }
}

// Bulk descriptors outlive recursive calls, so charge their actual heap backing
// to the same owner as the existing tree workspace. Capacity never grows.
pub(crate) struct BulkBuffer<'a, T> {
    pub(crate) values: Vec<T>,
    _reservation: CapacityReservation<'a>,
    limit: usize,
}
impl<'a, T> BulkBuffer<'a, T> {
    pub(crate) fn new(
        capacity: usize,
        resources: &mut TreeResources<'a>,
    ) -> Result<Self, TreeError> {
        resources.step(0)?;
        let bytes = capacity
            .checked_mul(std::mem::size_of::<T>())
            .ok_or(TreeError::Memory)?;
        let mut reservation = resources.owner.reserve(bytes)?;
        let mut values = Vec::new();
        #[cfg(feature = "allocation-audit")]
        let allocation = crate::allocation_audit::attributed(|| values.try_reserve_exact(capacity));
        #[cfg(not(feature = "allocation-audit"))]
        let allocation = values.try_reserve_exact(capacity);
        allocation.map_err(|_| TreeError::Memory)?;
        reservation.resize(
            values
                .capacity()
                .checked_mul(std::mem::size_of::<T>())
                .ok_or(TreeError::Memory)?,
        )?;
        Ok(Self {
            values,
            _reservation: reservation,
            limit: capacity,
        })
    }
    pub(crate) fn push(&mut self, value: T) -> Result<(), TreeError> {
        if self.values.len() == self.limit {
            return Err(TreeError::Memory);
        }
        self.values.push(value);
        Ok(())
    }
}
struct BulkPage {
    upper: Option<Separator>,
    reference: PhysicalRef,
}

fn retained_bulk_page<'a>(
    reference: Option<PhysicalRef>,
    resources: &mut TreeResources<'a>,
) -> Result<BulkBuffer<'a, BulkPage>, TreeError> {
    let mut pages = BulkBuffer::new(usize::from(reference.is_some()), resources)?;
    if let Some(reference) = reference {
        pages.push(BulkPage {
            upper: None,
            reference,
        })?;
    }
    Ok(pages)
}

fn emit_many<'a>(
    store: &mut impl BlockSink,
    root: DirectoryRoot,
    header: PageHeader,
    cells: &mut [Cell<'_>],
    scratch: &mut TreeScratch<'_>,
    resources: &mut TreeResources<'a>,
) -> Result<BulkBuffer<'a, BulkPage>, TreeError> {
    normalize_cells(store, root, header.generation, cells, resources)?;
    let mut cuts = BulkBuffer::new(cells.len(), resources)?;
    let mut used = 64usize;
    let mut start = 0;
    for (index, cell) in cells.iter().enumerate() {
        resources.step(1)?;
        let bytes = super::cell_len(*cell)?
            .checked_add(8)
            .ok_or(TreeError::Memory)?;
        if 64 + bytes > PAGE_BYTES {
            return Err(TreeError::Invalid("no bounded page split"));
        }
        if used + bytes > PAGE_BYTES || index - start == MAX_CELLS {
            cuts.push(index)?;
            used = 64;
            start = index;
        }
        used += bytes;
    }
    if !cells.is_empty() {
        cuts.push(cells.len())?;
    }
    let mut pages = BulkBuffer::new(cuts.values.len(), resources)?;
    let mut start = 0;
    for end in cuts.values {
        resources.step(1)?;
        let upper = if end == cells.len() {
            None
        } else if header.level == 0 {
            let Cell::Leaf { key, .. } = *cells.get(end).ok_or(TreeError::Memory)? else {
                return Err(TreeError::Invalid("leaf split"));
            };
            Some(Separator::new(key)?)
        } else {
            let last = cells.get_mut(end - 1).ok_or(TreeError::Memory)?;
            let Cell::Branch {
                upper: Some(key),
                child,
            } = *last
            else {
                return Err(TreeError::Invalid("branch split bound"));
            };
            let upper = Some(Separator::new(key)?);
            *last = Cell::Branch { upper: None, child };
            upper
        };
        let reference = append_page(
            store,
            root,
            header,
            cells.get(start..end).ok_or(TreeError::Memory)?,
            scratch,
            resources,
        )?;
        pages.push(BulkPage { upper, reference })?;
        start = end;
    }
    Ok(pages)
}

struct BulkEdit<'a, 's, V> {
    mutation: DirectoryMutation<V>,
    scratch: &'s mut TreeScratch<'a>,
}
impl<V> BulkEdit<'_, '_, V> {
    // The expected header bounds every child before recursion; levels strictly
    // decrease. Page backing is heap charged, not PAGE_BYTES per stack frame.
    #[allow(clippy::too_many_arguments)]
    fn page<'a, S: BlockSink>(
        &mut self,
        store: &mut S,
        reference: Option<PhysicalRef>,
        expected: PageHeader,
        lower: Option<Key<'_>>,
        upper: Option<Key<'_>>,
        ops: &[DirectoryOp<'_>],
        resources: &mut TreeResources<'a>,
    ) -> Result<BulkBuffer<'a, BulkPage>, TreeError>
    where
        V: LeafValidator<S>,
    {
        resources.step(1)?;
        let root = self.mutation.root;
        let mut old = resources.copied_span_buffer(PAGE_BYTES)?;
        let header = if let Some(reference) = reference {
            let header = store.with_scoped_reads(|| {
                trace_page_scoped(
                    store,
                    root,
                    reference,
                    lower,
                    upper,
                    resources,
                    |page, _, _, resources| {
                        resources.step(PAGE_BYTES as u64)?;
                        old.as_mut_slice().copy_from_slice(page.bytes);
                        Ok(page.header())
                    },
                )
            })?;
            if header.level != expected.level || header.generation > expected.generation {
                return Err(TreeError::Invalid("child level or creation generation"));
            }
            header
        } else {
            expected
        };
        let old_count = if reference.is_some() {
            count(old.as_slice())?
        } else {
            0
        };
        let mut cells = BulkBuffer::new(old_count + 2 * ops.len(), resources)?;
        let mut changed = false;
        if header.level == 0 {
            // Validate each old value exactly once, including replaced/removed
            // entries, matching the checked single-key mutation contract.
            store.with_scoped_reads(|| -> Result<(), TreeError> {
                for index in 0..old_count {
                    resources.step(1)?;
                    let Cell::Leaf { key, value } = owned_page_cell(old.as_slice(), header, index)?
                    else {
                        return Err(TreeError::Invalid("leaf required"));
                    };
                    self.mutation.validator.verify(
                        store,
                        root,
                        DirectoryEntry {
                            root,
                            generation: header.generation,
                            key,
                            value,
                        },
                        resources,
                    )?;
                }
                let mut next = 0;
                for index in 0..old_count {
                    resources.step(1)?;
                    let cell = owned_page_cell(old.as_slice(), header, index)?;
                    let Cell::Leaf { key, .. } = cell else {
                        return Err(TreeError::Invalid("leaf required"));
                    };
                    while let Some(op) = ops.get(next) {
                        resources.step(1)?;
                        if !compare(store, root, op.key(), key, resources)?.is_lt() {
                            break;
                        }
                        if let Some(cell) = op.cell() {
                            cells.push(cell)?;
                            changed = true;
                        }
                        next += 1;
                    }
                    if let Some(op) = ops.get(next)
                        && compare(store, root, op.key(), key, resources)?.is_eq()
                    {
                        changed = true;
                        if let Some(cell) = op.cell() {
                            cells.push(cell)?;
                        }
                        next += 1;
                    } else {
                        cells.push(cell)?;
                    }
                }
                for op in ops.get(next..).ok_or(TreeError::Memory)? {
                    resources.step(1)?;
                    if let Some(cell) = op.cell() {
                        cells.push(cell)?;
                        changed = true;
                    }
                }
                Ok(())
            })?;
            if !changed {
                return retained_bulk_page(reference, resources);
            }
            return emit_many(
                store,
                root,
                PageHeader {
                    generation: self.mutation.generation,
                    ..header
                },
                &mut cells.values,
                self.scratch,
                resources,
            );
        }
        store.with_scoped_reads(|| -> Result<(), TreeError> {
            let mut prior = lower;
            for index in 0..old_count {
                resources.step(1)?;
                let Cell::Branch {
                    upper: bound,
                    child,
                } = owned_page_cell(old.as_slice(), header, index)?
                else {
                    return Err(TreeError::Invalid("branch required"));
                };
                let child_root = DirectoryRoot {
                    generation: header.generation,
                    ..root
                };
                with_checked_block(store, child_root, child, resources, |block, resources| {
                    let page = checked_page(
                        store,
                        child_root,
                        block.identity(),
                        block.payload(),
                        prior,
                        bound.or(upper),
                        resources,
                    )?;
                    if page.header().level.checked_add(1) != Some(header.level) {
                        return Err(TreeError::Invalid("child level before directory rewrite"));
                    }
                    Ok(())
                })?;
                prior = bound;
            }
            Ok(())
        })?;
        // Own separators until all child replacements have been collected.
        let mut children = BulkBuffer::new(old_count + 2 * ops.len(), resources)?;
        let mut prior = lower;
        let mut next = 0;
        for index in 0..old_count {
            resources.step(1)?;
            let Cell::Branch {
                upper: bound,
                child,
            } = owned_page_cell(old.as_slice(), header, index)?
            else {
                return Err(TreeError::Invalid("branch required"));
            };
            let start = next;
            while let Some(op) = ops.get(next) {
                resources.step(1)?;
                if let Some(bound) = bound
                    && !store
                        .with_scoped_reads(|| compare(store, root, op.key(), bound, resources))?
                        .is_lt()
                {
                    break;
                }
                next += 1;
            }
            if start == next {
                children.push(BulkPage {
                    upper: bound.map(Separator::new).transpose()?,
                    reference: child,
                })?;
            } else {
                let mut replacement = self.page(
                    store,
                    Some(child),
                    PageHeader {
                        level: header
                            .level
                            .checked_sub(1)
                            .ok_or(TreeError::Invalid("bulk child level underflow"))?,
                        ..header
                    },
                    prior,
                    bound.or(upper),
                    ops.get(start..next).ok_or(TreeError::Memory)?,
                    resources,
                )?;
                changed |= replacement.values.len() != 1
                    || replacement
                        .values
                        .first()
                        .is_none_or(|page| page.reference != child);
                if let Some(last) = replacement.values.last_mut() {
                    last.upper = bound.map(Separator::new).transpose()?;
                }
                for page in replacement.values {
                    resources.step(1)?;
                    children.push(page)?;
                }
            }
            prior = bound;
        }
        if !changed {
            return retained_bulk_page(reference, resources);
        }
        if let Some(last) = children.values.last_mut() {
            last.upper = None;
        }
        for child in &children.values {
            resources.step(1)?;
            cells.push(Cell::Branch {
                upper: child.upper.as_ref().map(Separator::key).transpose()?,
                child: child.reference,
            })?;
        }
        emit_many(
            store,
            root,
            PageHeader {
                generation: self.mutation.generation,
                ..header
            },
            &mut cells.values,
            self.scratch,
            resources,
        )
    }
}

/// Apply at most 16,384 strictly ascending edits, copying each touched page once.
/// Only private immutable blocks are appended; the caller owns publication.
pub fn apply_sorted_checked<S: BlockSink>(
    store: &mut S,
    mutation: DirectoryMutation<impl LeafValidator<S>>,
    ops: &[DirectoryOp<'_>],
    scratch: &mut TreeScratch<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<DirectoryRoot, TreeError> {
    resources.step(1)?;
    if ops.len() > 16_384 {
        return Err(TreeError::Invalid("too many bulk ops"));
    }
    apply_prepared_sorted_checked(store, mutation, ops, scratch, resources)
}

// One entity may produce many label/range edits. The participant buffer has
// already admitted every descriptor and byte against StorageMemory; its bound
// is not the structured batch's 16,384-entity limit.
fn apply_prepared_sorted_checked<S: BlockSink>(
    store: &mut S,
    mutation: DirectoryMutation<impl LeafValidator<S>>,
    ops: &[DirectoryOp<'_>],
    scratch: &mut TreeScratch<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<DirectoryRoot, TreeError> {
    let (root, generation) = (mutation.root, mutation.generation);
    if generation < root.generation {
        return Err(TreeError::Invalid("generation regressed"));
    }
    let mut prior = None;
    for op in ops {
        resources.step(1)?;
        validate_key(store, root, op.key(), resources)?;
        if let Some(prior) = prior
            && !compare(store, root, prior, op.key(), resources)?.is_lt()
        {
            return Err(TreeError::Invalid("bulk ops out of order"));
        }
        prior = Some(op.key());
    }
    if ops.is_empty() {
        return Ok(root);
    }
    let mut level = if let Some(reference) = root.reference {
        store.with_scoped_reads(|| {
            trace_page_scoped(
                store,
                root,
                reference,
                None,
                None,
                resources,
                |page, _, _, _| Ok(page.header().level),
            )
        })?
    } else {
        0
    };
    let mut edit = BulkEdit { mutation, scratch };
    let mut pages = edit.page(
        store,
        root.reference,
        PageHeader {
            kind: root.kind,
            level,
            generation: root.generation,
        },
        None,
        None,
        ops,
        resources,
    )?;
    while pages.values.len() > 1 {
        resources.step(1)?;
        level = level
            .checked_add(1)
            .ok_or(TreeError::Invalid("root level overflow"))?;
        if level as usize >= MAX_DEPTH {
            return Err(TreeError::Invalid("directory depth limit"));
        }
        let mut cells = BulkBuffer::new(pages.values.len(), resources)?;
        for page in &pages.values {
            resources.step(1)?;
            cells.push(Cell::Branch {
                upper: page.upper.as_ref().map(Separator::key).transpose()?,
                child: page.reference,
            })?;
        }
        let next = emit_many(
            store,
            root,
            PageHeader {
                kind: root.kind,
                level,
                generation,
            },
            &mut cells.values,
            edit.scratch,
            resources,
        )?;
        drop(cells);
        pages = next;
    }
    resources.step(0)?;
    let reference = pages.values.first().map(|page| page.reference);
    if reference == root.reference {
        return Ok(root);
    }
    collapse_root(
        store,
        DirectoryRoot {
            generation,
            reference,
            ..root
        },
        (level, generation),
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
    collapse_root(
        store,
        DirectoryRoot {
            generation,
            reference: replacement,
            ..root
        },
        collapse_expected,
        resources,
    )
}

// Repeated singleton roots can remain after sparse subtree removal. Both edit
// paths preserve the old parent's level/generation bounds while collapsing.
fn collapse_root(
    source: &impl BlockSource,
    mut root: DirectoryRoot,
    mut expected: (u16, GraphGeneration),
    resources: &mut TreeResources<'_>,
) -> Result<DirectoryRoot, TreeError> {
    source.with_scoped_reads(|| {
        let mut depth = 0usize;
        while let Some(reference) = root.reference {
            resources.step(1)?;
            if depth >= MAX_DEPTH {
                return Err(TreeError::Invalid("collapse depth limit"));
            }
            depth += 1;
            let next = trace_page_scoped(
                source,
                root,
                reference,
                None,
                None,
                resources,
                |page, _, cells, _| {
                    let header = page.header();
                    if header.level != expected.0 || header.generation > expected.1 {
                        return Err(TreeError::Invalid("collapse child level/generation"));
                    }
                    if header.level == 0 || cells != 1 {
                        return Ok(None);
                    }
                    let Cell::Branch { upper: None, child } = page.cell(0)? else {
                        return Err(TreeError::Invalid("singleton root"));
                    };
                    let level = header
                        .level
                        .checked_sub(1)
                        .ok_or(TreeError::Invalid("collapse level underflow"))?;
                    Ok(Some((child, level, header.generation)))
                },
            )?;
            let Some((child, level, generation)) = next else {
                break;
            };
            root.reference = Some(child);
            expected = (level, generation);
        }
        resources.step(0)?;
        Ok(root)
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
pub(crate) mod tests {
    use super::*;
    use crate::lifecycle::{CancelToken, OpenOptions, Store};
    use crate::property_graph::storage::artifact::{self, ArtifactId, Block, ContainerKind};
    use std::collections::BTreeMap;
    #[derive(Clone)]
    struct Objects {
        store: StoreInstanceId,
        next: u128,
        tree_pages: usize,
        objects: BTreeMap<u128, Vec<u8>>,
    }
    impl Objects {
        fn new() -> Self {
            Self {
                store: StoreInstanceId::new(1u128 << 100).expect("source store identity"),
                next: 1,
                tree_pages: 0,
                objects: BTreeMap::new(),
            }
        }
    }
    impl BlockSource for Objects {
        fn resolve<'a>(
            &'a self,
            reference: PhysicalRef,
            resources: &mut TreeResources<'_>,
        ) -> Result<FramedBlock<'a>, TreeError> {
            resources.step(1)?;
            let bytes = self
                .objects
                .get(&reference.artifact.get())
                .ok_or(TreeError::Missing)?;
            let frame = artifact::decode(
                ContainerKind::Object,
                Some((self.store, reference.artifact)),
                bytes,
            )?;
            Ok(frame.framed_block(reference)?)
        }
    }
    impl BlockSink for Objects {
        fn append(
            &mut self,
            kind: BlockKind,
            generation: GraphGeneration,
            bytes: &[u8],
            resources: &mut TreeResources<'_>,
        ) -> Result<PhysicalRef, TreeError> {
            resources.step(1)?;
            let artifact = ArtifactId::new(self.next)?;
            let identity = ArtifactIdentity {
                store: self.store,
                artifact,
                generation,
                creation_serial: self.next as u64,
            };
            let blocks = [Block {
                kind,
                payload: bytes,
            }];
            let mut output = vec![0; artifact::encoded_len(ContainerKind::Object, &blocks)?];
            artifact::encode_into(ContainerKind::Object, identity, &blocks, &mut output)?;
            let reference =
                artifact::decode(ContainerKind::Object, Some((self.store, artifact)), &output)?
                    .reference(0)?;
            if self.objects.insert(self.next, output).is_some() {
                return Err(TreeError::Invalid("duplicate fixture artifact"));
            }
            self.tree_pages += usize::from(kind == BlockKind::TreePage);
            self.next += 1;
            Ok(reference)
        }
    }

    fn fixture() -> (tempfile::TempDir, Store, GraphResources, Objects) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path(),
            OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
        )
        .unwrap();
        let shared = GraphResources::from_store(&store).unwrap();
        (dir, store, shared, Objects::new())
    }
    pub(crate) fn inventory_fold_page_count() {
        use crate::property_graph::storage::inventory::{
            apply_inventory, retire_reclaimed_inventory,
        };
        use crate::property_graph::wal::{
            ArtifactDescriptor, BatchId, InventoryChange, InventoryState,
        };
        let (_dir, _store, shared, mut objects) = fixture();
        let control = QueryControl::Cancel(CancelToken::new());
        let mut resources = TreeResources::new(&control, &shared, u64::MAX).unwrap();
        let mut scratch = TreeScratch::new(&shared, PAGE_BYTES).unwrap();
        let root = DirectoryRoot::empty(
            objects.store,
            TreeKind::ObjectInventory,
            GraphGeneration::new(0),
        );
        let mut changes: Vec<_> = (1..=32)
            .map(|id| InventoryChange {
                object: ArtifactDescriptor {
                    store: objects.store,
                    artifact: ArtifactId::new(id).unwrap(),
                    generation: GraphGeneration::new(0),
                    serial: id as u64,
                    bytes: 104,
                    family: 17,
                    version: 1,
                    checksum: 0,
                },
                state: InventoryState::Retained,
            })
            .collect();
        let folded = apply_inventory(
            &mut objects,
            root,
            &changes,
            GraphGeneration::new(1),
            &mut scratch,
            &mut resources,
        )
        .unwrap();
        assert_eq!(entries(&objects, folded, &mut resources).len(), 32);
        eprintln!(
            "32-change fold: {} pages, {} sink bytes",
            objects.tree_pages,
            objects.objects.values().map(Vec::len).sum::<usize>()
        );
        assert!(
            objects.tree_pages <= 4,
            "32-change inventory fold emitted {} TreePage blocks",
            objects.tree_pages
        );
        for invalid in [[changes[1], changes[0]], [changes[0], changes[0]]] {
            let before = objects.tree_pages;
            assert!(matches!(
                apply_inventory(
                    &mut objects,
                    folded,
                    &invalid,
                    GraphGeneration::new(2),
                    &mut scratch,
                    &mut resources,
                ),
                Err(TreeError::Invalid("duplicate/unordered inventory change"))
            ));
            assert_eq!(objects.tree_pages, before);
        }
        for change in &mut changes {
            change.state = InventoryState::Reclaimed(BatchId::new(1).unwrap());
        }
        let reclaimed = apply_inventory(
            &mut objects,
            folded,
            &changes,
            GraphGeneration::new(2),
            &mut scratch,
            &mut resources,
        )
        .unwrap();
        let before = objects.tree_pages;
        let retired = retire_reclaimed_inventory(
            &mut objects,
            reclaimed,
            &changes,
            GraphGeneration::new(3),
            &mut scratch,
            &mut resources,
        )
        .unwrap();
        assert!(entries(&objects, retired, &mut resources).is_empty());
        assert!(objects.tree_pages - before <= 4);
    }

    #[test]
    fn ze260_label_edits_are_not_limited_to_the_entity_count() {
        use crate::property_graph::staging::{WriteLimits, WriteMemory};
        let (_dir, _store, shared, mut objects) = fixture();
        let control = QueryControl::Cancel(CancelToken::new());
        let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
        let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
        let mut resources = TreeResources::for_prepare(&memory, u64::MAX).unwrap();
        let mut scratch = TreeScratch::for_prepare(&memory).unwrap();
        let mut batch = DirectoryBatch::new(&memory, 1).unwrap();
        for label in (1u64..=16_385).rev() {
            let mut key = [0; 24];
            key[..8].copy_from_slice(&label.to_le_bytes());
            key[8..].copy_from_slice(&1u128.to_le_bytes());
            batch.push(&key, Some(&[]), &mut resources).unwrap();
        }
        let root = DirectoryRoot::empty(objects.store, TreeKind::Labels, GraphGeneration::new(0));
        let root = batch
            .flush(
                &mut objects,
                DirectoryMutation::new(root, GraphGeneration::new(1), OpaqueValues),
                &mut scratch,
                &mut resources,
            )
            .unwrap();
        let mut cursor = DirectoryCursor::seek(&objects, root, None, &mut resources).unwrap();
        let mut count = 0;
        while cursor.next_entry(&mut resources).unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 16_385);
    }

    #[test]
    fn bulk_update_emits_each_touched_page_once() {
        let (_dir, _store, shared, mut objects) = fixture();
        let control = QueryControl::Cancel(CancelToken::new());
        let mut resources = TreeResources::new(&control, &shared, u64::MAX).unwrap();
        let mut scratch = TreeScratch::new(&shared, PAGE_BYTES).unwrap();
        let root = DirectoryRoot::empty(objects.store, TreeKind::Nodes, GraphGeneration::new(0));
        let keys: Vec<_> = (1u128..=100).map(u128::to_le_bytes).collect();
        let ops: Vec<_> = keys
            .iter()
            .map(|key| DirectoryOp::Insert {
                key,
                value: &[7; 64],
            })
            .collect();
        let result = apply_sorted_checked(
            &mut objects,
            DirectoryMutation::new(root, GraphGeneration::new(1), OpaqueValues),
            &ops,
            &mut scratch,
            &mut resources,
        )
        .unwrap();
        assert_eq!(objects.tree_pages, 1);
        assert_eq!(entries(&objects, result, &mut resources).len(), 100);
        let absent = 101u128.to_le_bytes();
        let before = objects.tree_pages;
        let unchanged = apply_sorted_checked(
            &mut objects,
            DirectoryMutation::new(result, GraphGeneration::new(2), OpaqueValues),
            &[DirectoryOp::Remove { key: &absent }],
            &mut scratch,
            &mut resources,
        )
        .unwrap();
        assert_eq!(unchanged, result);
        assert_eq!(objects.tree_pages, before);

        // Construct exactly ten leaves with spare capacity; each receives ten
        // new keys, so the only ancestor is emitted once.
        let mut children = Vec::new();
        let old_keys: Vec<_> = (0u128..10)
            .map(|leaf| {
                (1u128..=10)
                    .map(|n| (leaf * 1000 + n).to_le_bytes())
                    .collect::<Vec<_>>()
            })
            .collect();
        for keys in &old_keys {
            let cells: Vec<_> = keys
                .iter()
                .map(|key| Cell::Leaf {
                    key: Key::Inline(key),
                    value: &[3; 64],
                })
                .collect();
            children.push(
                append_page(
                    &mut objects,
                    root,
                    PageHeader {
                        kind: root.kind,
                        level: 0,
                        generation: GraphGeneration::new(1),
                    },
                    &cells,
                    &mut scratch,
                    &mut resources,
                )
                .unwrap(),
            );
        }
        let cells: Vec<_> = children
            .iter()
            .enumerate()
            .map(|(index, child)| Cell::Branch {
                upper: old_keys.get(index + 1).map(|keys| Key::Inline(&keys[0])),
                child: *child,
            })
            .collect();
        let reference = append_page(
            &mut objects,
            root,
            PageHeader {
                kind: root.kind,
                level: 1,
                generation: GraphGeneration::new(1),
            },
            &cells,
            &mut scratch,
            &mut resources,
        )
        .unwrap();
        let root = DirectoryRoot {
            reference: Some(reference),
            generation: GraphGeneration::new(1),
            ..root
        };
        let keys: Vec<_> = (0u128..10)
            .flat_map(|leaf| (11u128..=20).map(move |n| (leaf * 1000 + n).to_le_bytes()))
            .collect();
        let ops: Vec<_> = keys
            .iter()
            .map(|key| DirectoryOp::Insert {
                key,
                value: &[7; 64],
            })
            .collect();
        struct CountValues<'a>(&'a mut usize);
        impl LeafValidator<Objects> for CountValues<'_> {
            fn verify(
                &mut self,
                _: &Objects,
                _: DirectoryRoot,
                _: DirectoryEntry<'_>,
                _: &mut TreeResources<'_>,
            ) -> Result<(), TreeError> {
                *self.0 += 1;
                Ok(())
            }
        }
        let mut validated = 0;
        let before = objects.tree_pages;
        let result = apply_sorted_checked(
            &mut objects,
            DirectoryMutation::new(root, GraphGeneration::new(2), CountValues(&mut validated)),
            &ops,
            &mut scratch,
            &mut resources,
        )
        .unwrap();
        assert_eq!(objects.tree_pages - before, 11);
        assert_eq!(validated, 100);
        assert_eq!(entries(&objects, result, &mut resources).len(), 200);
    }
    #[test]
    fn bulk_update_rejects_unsorted_and_duplicate_keys() {
        let (_dir, _store, shared, mut objects) = fixture();
        let control = QueryControl::Cancel(CancelToken::new());
        let mut resources = TreeResources::new(&control, &shared, u64::MAX).unwrap();
        let mut scratch = TreeScratch::new(&shared, PAGE_BYTES).unwrap();
        let root = DirectoryRoot::empty(objects.store, TreeKind::Nodes, GraphGeneration::new(0));
        for keys in [[256u128, 255], [7, 7]] {
            let keys = keys.map(u128::to_le_bytes);
            let ops = [
                DirectoryOp::Insert {
                    key: &keys[0],
                    value: &[1],
                },
                DirectoryOp::Remove { key: &keys[1] },
            ];
            assert!(matches!(
                apply_sorted_checked(
                    &mut objects,
                    DirectoryMutation::new(root, GraphGeneration::new(1), OpaqueValues),
                    &ops,
                    &mut scratch,
                    &mut resources
                ),
                Err(TreeError::Invalid("bulk ops out of order"))
            ));
        }
        let keys: Vec<_> = (1u128..=16_385).map(u128::to_le_bytes).collect();
        let ops: Vec<_> = keys.iter().map(|key| DirectoryOp::Remove { key }).collect();
        assert!(matches!(
            apply_sorted_checked(
                &mut objects,
                DirectoryMutation::new(root, GraphGeneration::new(1), OpaqueValues),
                &ops,
                &mut scratch,
                &mut resources
            ),
            Err(TreeError::Invalid("too many bulk ops"))
        ));
        assert_eq!(objects.tree_pages, 0);
    }

    #[test]
    fn bulk_update_splits_a_leaf_into_many_pages() {
        let (_dir, _store, shared, mut objects) = fixture();
        let control = QueryControl::Cancel(CancelToken::new());
        let mut resources = TreeResources::new(&control, &shared, u64::MAX).unwrap();
        let mut scratch = TreeScratch::new(&shared, PAGE_BYTES).unwrap();
        let mut root =
            DirectoryRoot::empty(objects.store, TreeKind::Nodes, GraphGeneration::new(0));
        let keys: Vec<_> = (1u128..=16_384).map(u128::to_le_bytes).collect();
        root = insert(
            &mut objects,
            root,
            &keys[0],
            &[9; 64],
            GraphGeneration::new(1),
            &mut scratch,
            &mut resources,
        )
        .unwrap();
        let old_root = root;
        let ops: Vec<_> = keys
            .iter()
            .map(|key| DirectoryOp::Insert {
                key,
                value: &[7; 64],
            })
            .collect();
        let before = objects.tree_pages;
        root = apply_sorted_checked(
            &mut objects,
            DirectoryMutation::new(root, GraphGeneration::new(2), OpaqueValues),
            &ops,
            &mut scratch,
            &mut resources,
        )
        .unwrap();
        // 64 header bytes; each cell has an 8-byte slot, 12-byte key
        // descriptor, 16-byte key, 8-byte leaf prefix and 64-byte value.
        let per_leaf = (PAGE_BYTES - 64) / 108;
        assert_eq!(
            objects.tree_pages - before,
            keys.len().div_ceil(per_leaf) + 1
        );
        let actual = entries(&objects, root, &mut resources);
        assert_eq!(
            actual,
            keys.iter()
                .map(|key| (key.to_vec(), vec![7; 64]))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            entries(&objects, old_root, &mut resources),
            vec![(keys[0].to_vec(), vec![9; 64])]
        );
        let removes: Vec<_> = keys.iter().map(|key| DirectoryOp::Remove { key }).collect();
        root = apply_sorted_checked(
            &mut objects,
            DirectoryMutation::new(root, GraphGeneration::new(3), OpaqueValues),
            &removes,
            &mut scratch,
            &mut resources,
        )
        .unwrap();
        assert!(root.reference.is_none());
        // Wider values force more children than one branch page can hold,
        // exercising branch cuts and repeated root growth as well as leaf cuts.
        let keys = &keys[..512];
        let ops: Vec<_> = keys
            .iter()
            .map(|key| DirectoryOp::Insert {
                key,
                value: &[5; 8000],
            })
            .collect();
        root = apply_sorted_checked(
            &mut objects,
            DirectoryMutation::new(root, GraphGeneration::new(4), OpaqueValues),
            &ops,
            &mut scratch,
            &mut resources,
        )
        .unwrap();
        let block = checked_block(&objects, root, root.reference.unwrap(), &mut resources).unwrap();
        assert_eq!(
            decode_page(root.kind, block.payload())
                .unwrap()
                .header()
                .level,
            2
        );
        assert_eq!(
            entries(&objects, root, &mut resources),
            keys.iter()
                .map(|key| (key.to_vec(), vec![5; 8000]))
                .collect::<Vec<_>>()
        );
        // Touch both internal branches, then remove a complete subtree.
        let ops: Vec<_> = keys
            .iter()
            .map(|key| DirectoryOp::Insert {
                key,
                value: &[6; 8000],
            })
            .collect();
        root = apply_sorted_checked(
            &mut objects,
            DirectoryMutation::new(root, GraphGeneration::new(5), OpaqueValues),
            &ops,
            &mut scratch,
            &mut resources,
        )
        .unwrap();
        let sequential_base = root;
        let removes: Vec<_> = keys[..500]
            .iter()
            .map(|key| DirectoryOp::Remove { key })
            .collect();
        root = apply_sorted_checked(
            &mut objects,
            DirectoryMutation::new(root, GraphGeneration::new(6), OpaqueValues),
            &removes,
            &mut scratch,
            &mut resources,
        )
        .unwrap();
        assert_eq!(
            entries(&objects, root, &mut resources),
            keys[500..]
                .iter()
                .map(|key| (key.to_vec(), vec![6; 8000]))
                .collect::<Vec<_>>()
        );
        assert!(root_is_collapsed(&objects, root, &mut resources));
        let mut sequential = sequential_base;
        for key in &keys[..500] {
            sequential = remove(
                &mut objects,
                sequential,
                key,
                GraphGeneration::new(6),
                &mut scratch,
                &mut resources,
            )
            .unwrap();
        }
        assert_eq!(
            tree_shape(&objects, root, &mut resources),
            tree_shape(&objects, sequential, &mut resources)
        );
    }

    #[test]
    fn bulk_update_matches_sequential_inserts_and_removes() {
        use proptest::prelude::*;
        use proptest::test_runner::{Config, RngSeed, TestRunner};
        use rand::RngCore;
        let mut rng = crate::test_support::seeded_rng(concat!(
            module_path!(),
            "::bulk_update_matches_sequential_inserts_and_removes"
        ));
        let config = Config {
            cases: 32,
            rng_seed: RngSeed::Fixed(rng.next_u64()),
            failure_persistence: None,
            ..Config::default()
        };
        let mut runner = TestRunner::new(config);
        let strategy = proptest::collection::btree_map(
            1u128..600,
            proptest::option::of(proptest::collection::vec(any::<u8>(), 1..128)),
            1..180,
        );
        let (_dir, _store, shared, mut objects) = fixture();
        let control = QueryControl::Cancel(CancelToken::new());
        let mut resources = TreeResources::new(&control, &shared, u64::MAX).unwrap();
        let mut scratch = TreeScratch::new(&shared, PAGE_BYTES).unwrap();
        let mut base =
            DirectoryRoot::empty(objects.store, TreeKind::Nodes, GraphGeneration::new(0));
        for key in 1u128..=300 {
            base = insert(
                &mut objects,
                base,
                &key.to_le_bytes(),
                &[9; 64],
                GraphGeneration::new(1),
                &mut scratch,
                &mut resources,
            )
            .unwrap();
        }
        runner
            .run(&strategy, |edits| {
                let mut objects = objects.clone();
                let mut resources = TreeResources::new(&control, &shared, u64::MAX).unwrap();
                let mut scratch = TreeScratch::new(&shared, PAGE_BYTES).unwrap();
                let data: Vec<_> = edits
                    .iter()
                    .map(|(key, value)| (key.to_le_bytes(), value))
                    .collect();
                let ops: Vec<_> = data
                    .iter()
                    .map(|(key, value)| match value {
                        Some(value) => DirectoryOp::Insert { key, value },
                        None => DirectoryOp::Remove { key },
                    })
                    .collect();
                let bulk = apply_sorted_checked(
                    &mut objects,
                    DirectoryMutation::new(base, GraphGeneration::new(2), OpaqueValues),
                    &ops,
                    &mut scratch,
                    &mut resources,
                )
                .unwrap();
                let mut sequential = base;
                for op in &ops {
                    sequential = match op {
                        DirectoryOp::Insert { key, value } => insert(
                            &mut objects,
                            sequential,
                            key,
                            value,
                            GraphGeneration::new(2),
                            &mut scratch,
                            &mut resources,
                        ),
                        DirectoryOp::Remove { key } => remove(
                            &mut objects,
                            sequential,
                            key,
                            GraphGeneration::new(2),
                            &mut scratch,
                            &mut resources,
                        ),
                    }
                    .unwrap();
                }
                prop_assert!(root_is_collapsed(&objects, bulk, &mut resources));
                prop_assert_eq!(
                    tree_shape(&objects, bulk, &mut resources),
                    tree_shape(&objects, sequential, &mut resources)
                );
                prop_assert_eq!(
                    entries(&objects, bulk, &mut resources),
                    entries(&objects, sequential, &mut resources)
                );
                Ok(())
            })
            .unwrap();
    }

    fn root_is_collapsed(
        objects: &Objects,
        root: DirectoryRoot,
        resources: &mut TreeResources<'_>,
    ) -> bool {
        let Some(reference) = root.reference else {
            return true;
        };
        let block = checked_block(objects, root, reference, resources).unwrap();
        let page = decode_page(root.kind, block.payload()).unwrap();
        count(block.payload()).unwrap() > 1 || page.header().level == 0
    }

    fn tree_shape(
        objects: &Objects,
        root: DirectoryRoot,
        resources: &mut TreeResources<'_>,
    ) -> (Option<u16>, usize) {
        let mut pending: Vec<_> = root.reference.into_iter().collect();
        let mut level = None;
        let mut pages = 0;
        while let Some(reference) = pending.pop() {
            let block = checked_block(objects, root, reference, resources).unwrap();
            let page = decode_page(root.kind, block.payload()).unwrap();
            level.get_or_insert(page.header().level);
            pages += 1;
            if page.header().level > 0 {
                for index in 0..count(block.payload()).unwrap() {
                    let Cell::Branch { child, .. } = page.cell(index).unwrap() else {
                        unreachable!()
                    };
                    pending.push(child);
                }
            }
        }
        (level, pages)
    }

    // Like NativePreparationSource: resolve pins old artifacts; with_block can
    // release incidental reads only inside with_scoped_reads. New sink blocks
    // are resident preparation data and consume no base-source slots.
    struct LimitedObjects {
        objects: Objects,
        base_end: u128,
        pins: std::cell::RefCell<std::collections::BTreeSet<u128>>,
        scoped: std::cell::Cell<bool>,
        scoped_reads: std::cell::Cell<usize>,
    }
    impl BlockSource for LimitedObjects {
        fn resolve<'a>(
            &'a self,
            reference: PhysicalRef,
            resources: &mut TreeResources<'_>,
        ) -> Result<FramedBlock<'a>, TreeError> {
            if reference.artifact.get() < self.base_end {
                let mut pins = self.pins.borrow_mut();
                if !pins.contains(&reference.artifact.get()) && pins.len() == 4 {
                    return Err(TreeError::Memory);
                }
                pins.insert(reference.artifact.get());
            }
            self.objects.resolve(reference, resources)
        }
        fn with_block<R>(
            &self,
            reference: PhysicalRef,
            resources: &mut TreeResources<'_>,
            callback: impl for<'a, 'r> FnOnce(
                FramedBlock<'a>,
                &'r mut TreeResources<'_>,
            ) -> Result<R, TreeError>,
        ) -> Result<R, TreeError> {
            if self.scoped.get() {
                self.scoped_reads.set(self.scoped_reads.get() + 1);
                callback(self.objects.resolve(reference, resources)?, resources)
            } else {
                callback(self.resolve(reference, resources)?, resources)
            }
        }
        fn scoped_blocks(&self) -> bool {
            self.scoped.get()
        }
        fn with_scoped_reads<R>(&self, body: impl FnOnce() -> R) -> R {
            let old = self.scoped.replace(true);
            let result = body();
            self.scoped.set(old);
            result
        }
    }
    impl BlockSink for LimitedObjects {
        fn append(
            &mut self,
            kind: BlockKind,
            generation: GraphGeneration,
            bytes: &[u8],
            resources: &mut TreeResources<'_>,
        ) -> Result<PhysicalRef, TreeError> {
            self.objects.append(kind, generation, bytes, resources)
        }
    }
    struct ReadValues<'a> {
        records: &'a BTreeMap<u128, PhysicalRef>,
        verified: usize,
    }
    impl LeafValidator<LimitedObjects> for ReadValues<'_> {
        fn verify(
            &mut self,
            source: &LimitedObjects,
            _: DirectoryRoot,
            entry: DirectoryEntry<'_>,
            resources: &mut TreeResources<'_>,
        ) -> Result<(), TreeError> {
            let Key::Inline(key) = entry.key() else {
                unreachable!()
            };
            let key = u128::from_le_bytes(key.try_into().unwrap());
            self.verified += 1;
            source.with_block(self.records[&key], resources, |block, _| {
                assert_eq!(block.payload(), key.to_le_bytes());
                Ok(())
            })
        }
    }
    impl LeafValidator<LimitedObjects> for &mut ReadValues<'_> {
        fn verify(
            &mut self,
            source: &LimitedObjects,
            root: DirectoryRoot,
            entry: DirectoryEntry<'_>,
            resources: &mut TreeResources<'_>,
        ) -> Result<(), TreeError> {
            (**self).verify(source, root, entry, resources)
        }
    }
    fn limited_slots_bulk(leaves: usize) {
        let (_dir, _store, shared, mut objects) = fixture();
        let control = QueryControl::Cancel(CancelToken::new());
        let mut resources = TreeResources::new(&control, &shared, u64::MAX).unwrap();
        let mut scratch = TreeScratch::new(&shared, PAGE_BYTES).unwrap();
        let mut root =
            DirectoryRoot::empty(objects.store, TreeKind::Nodes, GraphGeneration::new(1));
        let keys: Vec<_> = (0..leaves)
            .map(|leaf| {
                (1..=8)
                    .map(|n| ((leaf * 1000 + n) as u128).to_le_bytes())
                    .collect::<Vec<_>>()
            })
            .collect();
        let mut records = BTreeMap::new();
        let mut children = Vec::new();
        for keys in &keys {
            for key in keys {
                let reference = objects
                    .append(BlockKind::NodeRecord, root.generation, key, &mut resources)
                    .unwrap();
                records.insert(u128::from_le_bytes(*key), reference);
            }
            let cells: Vec<_> = keys
                .iter()
                .map(|key| Cell::Leaf {
                    key: Key::Inline(key),
                    value: &[],
                })
                .collect();
            children.push(
                append_page(
                    &mut objects,
                    root,
                    PageHeader {
                        kind: root.kind,
                        level: 0,
                        generation: root.generation,
                    },
                    &cells,
                    &mut scratch,
                    &mut resources,
                )
                .unwrap(),
            );
        }
        root.reference = Some(if leaves == 1 {
            children[0]
        } else {
            let cells: Vec<_> = children
                .iter()
                .enumerate()
                .map(|(i, child)| Cell::Branch {
                    upper: keys.get(i + 1).map(|keys| Key::Inline(&keys[0])),
                    child: *child,
                })
                .collect();
            append_page(
                &mut objects,
                root,
                PageHeader {
                    kind: root.kind,
                    level: 1,
                    generation: root.generation,
                },
                &cells,
                &mut scratch,
                &mut resources,
            )
            .unwrap()
        });
        let mut source = LimitedObjects {
            base_end: objects.next,
            objects,
            pins: Default::default(),
            scoped: Default::default(),
            scoped_reads: Default::default(),
        };
        let ops: Vec<_> = keys
            .iter()
            .map(|keys| DirectoryOp::Insert {
                key: &keys[0],
                value: &[7],
            })
            .collect();
        let mut validator = ReadValues {
            records: &records,
            verified: 0,
        };
        let result = apply_sorted_checked(
            &mut source,
            DirectoryMutation::new(root, GraphGeneration::new(2), &mut validator),
            &ops,
            &mut scratch,
            &mut resources,
        )
        .unwrap();
        assert_eq!(validator.verified, leaves * 8);
        assert!(source.pins.borrow().len() <= 4);
        assert!(source.scoped_reads.get() >= leaves * 8);
        let actual = entries(&source.objects, result, &mut resources);
        assert_eq!(actual.len(), leaves * 8);
        for (index, (_, value)) in actual.iter().enumerate() {
            assert_eq!(
                value.as_slice(),
                if index % 8 == 0 { &[7][..] } else { &[] }
            );
        }
    }
    #[test]
    fn bulk_update_scopes_leaf_validation() {
        limited_slots_bulk(1);
    }
    #[test]
    fn bulk_update_scopes_child_validation() {
        limited_slots_bulk(70);
    }

    #[test]
    fn bulk_update_splits_overflow_fence_keys() {
        let (_dir, _store, shared, mut objects) = fixture();
        let control = QueryControl::Cancel(CancelToken::new());
        let mut resources = TreeResources::new(&control, &shared, u64::MAX).unwrap();
        let mut scratch = TreeScratch::new(&shared, PAGE_BYTES).unwrap();
        let root =
            DirectoryRoot::empty(objects.store, TreeKind::KeyFences, GraphGeneration::new(0));
        let keys: Vec<_> = (0..300)
            .map(|n| {
                let mut key = vec![1];
                key.extend_from_slice(&1u64.to_le_bytes());
                key.extend_from_slice(format!("{n:04}{}", "x".repeat(600)).as_bytes());
                key
            })
            .collect();
        let ops: Vec<_> = keys
            .iter()
            .map(|key| DirectoryOp::Insert {
                key,
                value: &[7; 64],
            })
            .collect();
        let root = apply_sorted_checked(
            &mut objects,
            DirectoryMutation::new(root, GraphGeneration::new(1), OpaqueValues),
            &ops,
            &mut scratch,
            &mut resources,
        )
        .unwrap();
        assert_eq!(tree_shape(&objects, root, &mut resources).0, Some(1));
        let block = checked_block(&objects, root, root.reference.unwrap(), &mut resources).unwrap();
        let page = decode_page(root.kind, block.payload()).unwrap();
        assert!(matches!(
            page.cell(0).unwrap(),
            Cell::Branch {
                upper: Some(Key::Overflow { .. }),
                ..
            }
        ));
        let mut cursor = DirectoryCursor::seek(&objects, root, None, &mut resources).unwrap();
        for key in &keys {
            let entry = cursor.next_entry(&mut resources).unwrap().unwrap();
            assert!(matches!(entry.key(), Key::Overflow { .. }));
            assert_eq!(
                compare(
                    &objects,
                    root,
                    entry.key(),
                    Key::Inline(key),
                    &mut resources
                )
                .unwrap(),
                std::cmp::Ordering::Equal
            );
            assert_eq!(entry.value(), &[7; 64]);
        }
        assert!(cursor.next_entry(&mut resources).unwrap().is_none());
    }
    fn entries(
        objects: &Objects,
        root: DirectoryRoot,
        resources: &mut TreeResources<'_>,
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut cursor = DirectoryCursor::seek(objects, root, None, resources).unwrap();
        let mut result = Vec::new();
        while let Some(entry) = cursor.next_entry(resources).unwrap() {
            let Key::Inline(key) = entry.key() else {
                unreachable!()
            };
            result.push((key.to_vec(), entry.value().to_vec()));
        }
        result
    }
}
