//! Canonical compact range descriptors. Decoding these bytes establishes shape,
//! not admission: the owning reader must still resolve and validate every run.
mod edit;
use super::{
    Direction, MAX_BASE_ENTRIES, MAX_DELTA_RUNS, MAX_PENDING_ENTRIES, RangeKey, UpperBound,
};
use super::{Edge, Error, MAX_MERGED_ENTRIES, MERGE_STATE_BYTES, Merged, Work};
use crate::property_graph::query::resources::{QueryArena, QueryMemory, QueryReservation};
use crate::property_graph::query::runtime::{RuntimeError, WorkKind};
use crate::property_graph::storage::artifact::{self, BlockKind, PhysicalRef};
use crate::property_graph::storage::memory::{StorageBuffer, StorageMemory, StorageReservation};
use crate::property_graph::storage::tree::Key;
use crate::property_graph::storage::tree::directory::{
    BlockSource, DirectoryEntry, DirectoryRoot, TreeResources,
};
use crate::property_graph::storage::tree::directory::{NativeReadEvent, QueryOwner};
use crate::property_graph::storage::tree::{TreeKind, directory::TreeError};
use crate::property_graph::{GraphGeneration, StoreInstanceId};
use crate::property_graph::{NodeId, RelId, catalog::RelTypeId};
pub use edit::{RangeEditContext, put_range, remove_range};

/// Version-one leaf value, independent of the tree's inline key threshold.
pub const RANGE_DESCRIPTOR_BYTES: usize = 328;

/// Shape-checked descriptor only. Its private fields cannot bypass reference or
/// reserved-slot checks. Physical resolution and complete merge remain required.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RangeDescriptor {
    key: RangeKey,
    watermark: u64,
    pending: u32,
    base_count: u32,
    base: PhysicalRef,
    deltas: [Option<PhysicalRef>; MAX_DELTA_RUNS],
}

/// Reusable, fully charged merge output and fixed kernel state for one range.
/// This preparation owner does not establish a query view or source admission.
enum RangeEdges<'a> {
    Preparation(StorageBuffer<'a, Edge>),
    Query(QueryArena<'a, 'a, Edge>),
}
impl RangeEdges<'_> {
    fn as_mut_slice(&mut self) -> &mut [Edge] {
        match self {
            Self::Preparation(edges) => edges.as_mut_slice(),
            Self::Query(edges) => edges.as_mut_slice(),
        }
    }
    fn owned_bytes(&self) -> usize {
        match self {
            Self::Preparation(edges) => edges.owned_bytes(),
            Self::Query(edges) => edges.reserved_bytes(),
        }
    }
}
enum RangeOwner<'a> {
    Preparation(&'a StorageMemory<'a>),
    Query(QueryOwner<'a>),
}
enum RangeCharge<'a> {
    Preparation(StorageReservation<'a>),
    Query(QueryReservation<'a, 'a>),
}
impl RangeCharge<'_> {
    fn bytes(&self) -> usize {
        match self {
            Self::Preparation(charge) => charge.bytes(),
            Self::Query(charge) => charge.bytes(),
        }
    }
}
/// Reusable, fully charged merge output and fixed kernel state for one range.
pub struct RangeScratch<'a> {
    // Free backing before its capacity reservation (field drop order).
    edges: RangeEdges<'a>,
    owner: RangeOwner<'a>,
    charge: RangeCharge<'a>,
}
impl<'a> RangeScratch<'a> {
    /// Admit all backing before initializing it, with bounded control polls.
    pub fn for_prepare(
        memory: &'a StorageMemory<'a>,
        r: &mut TreeResources<'_>,
    ) -> Result<Self, TreeError> {
        r.require_preparation(memory)?;
        let charge = RangeCharge::Preparation(
            memory.reserve(std::mem::size_of::<Self>() + MERGE_STATE_BYTES)?,
        );
        let mut edges = StorageBuffer::new(memory, MAX_MERGED_ENTRIES)?;
        let empty = Edge {
            rel: RelId::new(1).map_err(|_| invalid("adjacency scratch identity"))?,
            neighbor: NodeId::new(1).map_err(|_| invalid("adjacency scratch identity"))?,
        };
        for _ in 0..MAX_MERGED_ENTRIES {
            r.step(std::mem::size_of::<Edge>() as u64)?;
            edges.push(empty)?;
        }
        r.step(0)?;
        Ok(Self {
            edges: RangeEdges::Preparation(edges),
            owner: RangeOwner::Preparation(memory),
            charge,
        })
    }
    /// Reserve fixed query backing through the exact runtime memory owner before
    /// initializing any edge slot. A failure drops every acquired reservation.
    pub fn for_query<'g>(
        memory: &'a QueryMemory<'g>,
        r: &mut TreeResources<'a>,
    ) -> Result<Self, TreeError>
    where
        'g: 'a,
    {
        r.step(0)?;
        let memory: &'a QueryMemory<'a> = memory;
        let owner = r.query_owner(memory)?;
        let control_bytes = std::mem::size_of::<Self>()
            .checked_sub(std::mem::size_of::<QueryArena<'_, '_, Edge>>())
            .and_then(|bytes| bytes.checked_add(MERGE_STATE_BYTES))
            .ok_or(TreeError::Runtime(RuntimeError::Memory(
                crate::property_graph::query::resources::MemoryError::Limit,
            )))?;
        let charge = RangeCharge::Query(
            memory
                .reserve(control_bytes)
                .map_err(RuntimeError::Memory)
                .map_err(TreeError::Runtime)?,
        );
        let mut edges = QueryArena::new(memory, MAX_MERGED_ENTRIES)
            .map_err(RuntimeError::Memory)
            .map_err(TreeError::Runtime)?;
        let empty = Edge {
            rel: RelId::new(1).map_err(|_| invalid("adjacency scratch identity"))?,
            neighbor: NodeId::new(1).map_err(|_| invalid("adjacency scratch identity"))?,
        };
        for _ in 0..MAX_MERGED_ENTRIES {
            r.step(std::mem::size_of::<Edge>() as u64)?;
            edges
                .push(empty)
                .map_err(RuntimeError::Memory)
                .map_err(TreeError::Runtime)?;
        }
        r.step(0)?;
        Ok(Self {
            edges: RangeEdges::Query(edges),
            owner: RangeOwner::Query(owner),
            charge,
        })
    }
    /// Complete actual backing, descriptor and kernel-state reservation.
    pub fn owned_bytes(&self) -> usize {
        self.edges.owned_bytes() + self.charge.bytes()
    }
    pub(super) fn require_owner(&self, r: &TreeResources<'_>) -> Result<(), TreeError> {
        match self.owner {
            RangeOwner::Preparation(memory) => r.require_preparation(memory),
            RangeOwner::Query(owner) => r.require_query_owner(owner),
        }
    }
}

/// Complete physical and inner validation at one exact containing generation
/// and committed WAL cutoff. Only the returned edge prefix is usable.
pub struct ValidatedRange<'a> {
    descriptor: RangeDescriptor,
    merged: Merged<'a>,
}
impl ValidatedRange<'_> {
    /// Descriptor whose exact active references and all inner counts were checked.
    pub const fn descriptor(&self) -> RangeDescriptor {
        self.descriptor
    }
    /// Fully merged raw rows. Authoritative topology and endpoint liveness are
    /// checked by the graph reader before these become observable edges.
    pub fn edges(&self) -> &[Edge] {
        self.merged.edges()
    }
    /// Existing bounded kernel's zero, one or two nonempty partitions.
    pub fn partitions(&self) -> &[super::Partition] {
        self.merged.partitions()
    }
}

/// Validate an actual entry from this exact root under its original leaf
/// generation. The root's possibly newer generation never widens child bounds.
pub fn validate_range<'a>(
    source: &impl BlockSource,
    root: DirectoryRoot,
    entry: DirectoryEntry<'_>,
    cutoff: u64,
    scratch: &'a mut RangeScratch<'_>,
    r: &mut TreeResources<'_>,
) -> Result<ValidatedRange<'a>, TreeError> {
    scratch.require_owner(r)?;
    entry.require_root(root)?;
    r.step((40 + RANGE_DESCRIPTOR_BYTES) as u64)?;
    let Key::Inline(key) = entry.key() else {
        return Err(invalid("overflow adjacency key"));
    };
    let descriptor = RangeDescriptor::decode(root.kind(), key, entry.value())?;
    let range = validate_descriptor(
        source,
        root.store(),
        entry.creation_generation(),
        descriptor,
        cutoff,
        scratch,
        r,
    )?;
    if range.edges().is_empty() {
        return Err(invalid("empty persisted adjacency range"));
    }
    Ok(range)
}

pub(super) fn validate_descriptor<'a>(
    source: &impl BlockSource,
    store: StoreInstanceId,
    generation: GraphGeneration,
    descriptor: RangeDescriptor,
    cutoff: u64,
    scratch: &'a mut RangeScratch<'_>,
    r: &mut TreeResources<'_>,
) -> Result<ValidatedRange<'a>, TreeError> {
    scratch.require_owner(r)?;
    let base = resolve(source, store, generation, descriptor.base, r)?;
    let mut runs: [&[u8]; MAX_DELTA_RUNS] = [&[]; MAX_DELTA_RUNS];
    let mut count = 0;
    for (slot, reference) in runs.iter_mut().zip(descriptor.deltas()) {
        *slot = resolve(source, store, generation, reference, r)?;
        count += 1;
    }
    let runs = runs.get(..count).ok_or(invalid("adjacency run extent"))?;
    let merged = super::merge(
        descriptor.key,
        descriptor.watermark,
        cutoff,
        base,
        runs,
        scratch.edges.as_mut_slice(),
        &mut |work| checkpoint(r, work),
    )
    .map_err(map_error)?;
    // The shared kernel has now checked complete inner framing and every entry.
    // Read only those checked count fields; do not add a permissive inner parser.
    r.step(4)?;
    if u32::from_le_bytes(read(base, 72)?) != descriptor.base_count {
        return Err(invalid("adjacency base count mismatch"));
    }
    let mut pending = 0u32;
    for bytes in runs {
        r.step(4)?;
        let entries = u32::from_le_bytes(read(bytes, 72)?);
        if entries == 0 {
            return Err(invalid("empty persisted adjacency run"));
        }
        pending = pending
            .checked_add(entries)
            .ok_or(invalid("adjacency pending overflow"))?;
    }
    if pending != descriptor.pending {
        return Err(invalid("adjacency pending count mismatch"));
    }
    r.step(0)?;
    Ok(ValidatedRange { descriptor, merged })
}

fn resolve<'a>(
    source: &'a impl BlockSource,
    store: StoreInstanceId,
    generation: GraphGeneration,
    reference: PhysicalRef,
    r: &mut TreeResources<'_>,
) -> Result<&'a [u8], TreeError> {
    r.step(1)?;
    let block = source.resolve(reference, r)?;
    if block.reference() != reference
        || block.identity().store != store
        || block.identity().artifact != reference.artifact
        || block.identity().generation.get() > generation.get()
    {
        return Err(invalid("adjacency reference identity/generation"));
    }
    Ok(block.payload())
}

pub(super) fn checkpoint(r: &mut TreeResources<'_>, work: Work) -> Result<(), TreeError> {
    let units = match work {
        Work::HeaderBytes(n) | Work::EntryBytes(n) | Work::CopyBytes(n) => n as u64,
        Work::Compare => 1,
        Work::Finish => 0,
    };
    r.step(units)?;
    match work {
        Work::EntryBytes(_) => r.read_event(NativeReadEvent::AdjacencyEntry),
        Work::CopyBytes(bytes) => r
            .read_event(NativeReadEvent::CopiedBytes(u64::try_from(bytes).map_err(
                |_| TreeError::Runtime(RuntimeError::Limit(WorkKind::CopiedBytes)),
            )?)),
        Work::HeaderBytes(_) | Work::Compare | Work::Finish => Ok(()),
    }
}
pub(super) fn map_error(error: Error<TreeError>) -> TreeError {
    match error {
        Error::Control(error) => error,
        Error::Format(_) => invalid("adjacency inner format"),
        Error::Limit(_) => invalid("adjacency inner limit"),
    }
}

impl RangeDescriptor {
    /// Construct canonical shape from exact required references. Resolution is
    /// intentionally deferred to the mandatory checked edit/read operation.
    pub fn new(
        key: RangeKey,
        watermark: u64,
        base_count: usize,
        base: PhysicalRef,
        deltas: &[PhysicalRef],
        pending: usize,
    ) -> Result<Self, TreeError> {
        if deltas.len() > MAX_DELTA_RUNS {
            return Err(invalid("adjacency descriptor delta count"));
        }
        let mut references = [None; MAX_DELTA_RUNS];
        for (slot, reference) in references.iter_mut().zip(deltas) {
            *slot = Some(*reference);
        }
        let descriptor = Self {
            key,
            watermark,
            base,
            base_count: u32::try_from(base_count).map_err(|_| invalid("adjacency base count"))?,
            pending: u32::try_from(pending).map_err(|_| invalid("adjacency pending count"))?,
            deltas: references,
        };
        let mut bytes = [0; RANGE_DESCRIPTOR_BYTES];
        descriptor.encode(&mut bytes)?;
        Self::decode(
            tree_kind(key.direction),
            &descriptor.directory_key()?,
            &bytes,
        )
    }
    /// Fixed unsigned node/type/lower key; no upper-bound successor arithmetic.
    pub fn directory_key(self) -> Result<[u8; 40], TreeError> {
        directory_key(self.key)
    }
    /// Decode a fixed 40-byte numeric tree key and its 328-byte leaf value.
    /// The caller charges this bounded access and retains the containing context.
    pub fn decode(kind: TreeKind, key: &[u8], bytes: &[u8]) -> Result<Self, TreeError> {
        if key.len() != 40 || bytes.len() != RANGE_DESCRIPTOR_BYTES {
            return Err(invalid("adjacency descriptor length"));
        }
        let direction = direction(kind)?;
        if u16::from_le_bytes(read(bytes, 0)?) != 1
            || bytes.get(2).copied() != Some(direction as u8)
            || read::<3>(bytes, 5)? != [0; 3]
        {
            return Err(invalid("adjacency descriptor version/direction/reserved"));
        }
        let upper_bits = u128::from_le_bytes(read(bytes, 8)?);
        let upper = match bytes.get(3) {
            Some(0) => UpperBound::Exclusive(
                RelId::new(upper_bits).map_err(|_| invalid("adjacency upper identity"))?,
            ),
            Some(1) if upper_bits == 0 => UpperBound::Infinity,
            _ => return Err(invalid("adjacency upper tag/reserved")),
        };
        let key = RangeKey {
            node: NodeId::new(u128::from_le_bytes(read(key, 0)?))
                .map_err(|_| invalid("adjacency node identity"))?,
            rel_type: RelTypeId::new(u64::from_le_bytes(read(key, 16)?))
                .map_err(|_| invalid("adjacency type identity"))?,
            lower: RelId::new(u128::from_le_bytes(read(key, 24)?))
                .map_err(|_| invalid("adjacency lower identity"))?,
            direction,
            upper,
        };
        if matches!(upper, UpperBound::Exclusive(upper) if upper <= key.lower) {
            return Err(invalid("adjacency descriptor interval"));
        }
        let count = usize::from(*bytes.get(4).ok_or(invalid("adjacency delta count"))?);
        let pending = u32::from_le_bytes(read(bytes, 32)?);
        let base_count = u32::from_le_bytes(read(bytes, 36)?);
        if count > MAX_DELTA_RUNS
            || pending as usize > MAX_PENDING_ENTRIES
            || base_count as usize > MAX_BASE_ENTRIES
            || (count == 0) != (pending == 0)
            || (pending as usize) < count
            || (count == 0 && base_count == 0)
        {
            return Err(invalid("adjacency descriptor counts"));
        }
        let base = decode_ref(bytes, 40, BlockKind::AdjacencyBase)?;
        let mut deltas = [None; MAX_DELTA_RUNS];
        for (index, slot) in deltas.iter_mut().enumerate() {
            let offset = 72 + index * 32;
            if index < count {
                *slot = Some(decode_ref(bytes, offset, BlockKind::AdjacencyDelta)?);
            } else if read::<32>(bytes, offset)? != [0; 32] {
                return Err(invalid("adjacency inactive reference"));
            }
        }
        Ok(Self {
            key,
            watermark: u64::from_le_bytes(read(bytes, 24)?),
            pending,
            base_count,
            base,
            deltas,
        })
    }

    /// Encode exactly the canonical layout. This performs one bounded 328-byte
    /// touch; its surrounding operation owns cancellation and work accounting.
    pub fn encode(self, output: &mut [u8; RANGE_DESCRIPTOR_BYTES]) -> Result<(), TreeError> {
        output.fill(0);
        put(output, 0, &1u16.to_le_bytes())?;
        put(output, 2, &[self.key.direction as u8])?;
        match self.key.upper {
            UpperBound::Exclusive(upper) => put(output, 8, &upper.get().to_le_bytes())?,
            UpperBound::Infinity => put(output, 3, &[1])?,
        }
        put(output, 4, &[self.deltas().count() as u8])?;
        put(output, 24, &self.watermark.to_le_bytes())?;
        put(output, 32, &self.pending.to_le_bytes())?;
        put(output, 36, &self.base_count.to_le_bytes())?;
        encode_ref(output, 40, self.base)?;
        for (index, reference) in self.deltas().enumerate() {
            encode_ref(output, 72 + index * 32, reference)?;
        }
        Ok(())
    }

    /// Exact node/type/direction and half-open relationship interval.
    pub const fn key(self) -> RangeKey {
        self.key
    }
    /// Base consolidation cutoff in the independent WAL sequence domain.
    pub const fn watermark(self) -> u64 {
        self.watermark
    }
    /// Declared base entries; the validated inner header must agree.
    pub const fn base_count(self) -> usize {
        self.base_count as usize
    }
    /// Declared pending entries; all validated inner headers must sum to it.
    pub const fn pending_count(self) -> usize {
        self.pending as usize
    }
    /// Required base, including when its entry count is zero.
    pub const fn base(self) -> PhysicalRef {
        self.base
    }
    /// Exact active references in immutable sequence order.
    pub fn deltas(&self) -> impl Iterator<Item = PhysicalRef> + '_ {
        self.deltas.iter().flatten().copied()
    }
}

fn tree_kind(direction: Direction) -> TreeKind {
    match direction {
        Direction::Out => TreeKind::OutRanges,
        Direction::In => TreeKind::InRanges,
    }
}
pub(super) fn directory_key(key: RangeKey) -> Result<[u8; 40], TreeError> {
    let mut bytes = [0; 40];
    put(&mut bytes, 0, &key.node.get().to_le_bytes())?;
    put(&mut bytes, 16, &key.rel_type.get().to_le_bytes())?;
    put(&mut bytes, 24, &key.lower.get().to_le_bytes())?;
    Ok(bytes)
}

fn direction(kind: TreeKind) -> Result<Direction, TreeError> {
    match kind {
        TreeKind::OutRanges => Ok(Direction::Out),
        TreeKind::InRanges => Ok(Direction::In),
        _ => Err(invalid("adjacency tree role")),
    }
}
fn decode_ref(bytes: &[u8], offset: usize, kind: BlockKind) -> Result<PhysicalRef, TreeError> {
    let reference = artifact::decode_reference(&read::<32>(bytes, offset)?)?;
    if reference.kind != kind {
        return Err(invalid("adjacency reference role"));
    }
    Ok(reference)
}
fn encode_ref(bytes: &mut [u8], offset: usize, reference: PhysicalRef) -> Result<(), TreeError> {
    artifact::encode_reference(
        reference,
        bytes
            .get_mut(offset..offset + 32)
            .ok_or(invalid("adjacency reference extent"))?,
    )?;
    Ok(())
}
fn read<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], TreeError> {
    let end = offset
        .checked_add(N)
        .ok_or(invalid("adjacency descriptor extent"))?;
    bytes
        .get(offset..end)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(invalid("adjacency descriptor extent"))
}
fn put(bytes: &mut [u8], offset: usize, value: &[u8]) -> Result<(), TreeError> {
    let end = offset
        .checked_add(value.len())
        .ok_or(invalid("adjacency descriptor extent"))?;
    bytes
        .get_mut(offset..end)
        .ok_or(invalid("adjacency descriptor extent"))?
        .copy_from_slice(value);
    Ok(())
}
fn invalid(message: &'static str) -> TreeError {
    TreeError::Invalid(message)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, SnapshotLease, Store};
    use crate::property_graph::query::runtime::{
        RetainedView, RuntimeContext, RuntimeLimits, WorkCounters,
    };
    use crate::property_graph::query::{QueryError, QueryView};
    use crate::property_graph::resources::GraphResources;
    use crate::property_graph::storage::artifact::{ArtifactId, FramedBlock};
    use std::cell::Cell;

    struct View {
        token: QueryView,
        lease: SnapshotLease,
    }
    impl RetainedView for View {
        fn query_view(&self) -> &QueryView {
            &self.token
        }
        fn check_active(&self) -> Result<(), QueryError> {
            self.lease
                .check_active()
                .map_err(|_| QueryError::ReadCancelled)
        }
    }

    struct CountingSource(Cell<usize>);
    impl BlockSource for CountingSource {
        fn resolve<'a>(
            &'a self,
            _: PhysicalRef,
            _: &mut TreeResources<'_>,
        ) -> Result<FramedBlock<'a>, TreeError> {
            self.0.set(self.0.get() + 1);
            Err(TreeError::Missing)
        }
    }

    #[test]
    fn query_range_scratch_rejects_same_memory_context_without_touching_backing() {
        let directory = tempfile::tempdir().expect("store fixture");
        let store = Store::open(
            directory.path(),
            OpenOptions::new().with_max_resident_bytes(4 * 1024 * 1024),
        )
        .expect("store");
        let shared = GraphResources::from_store(&store).expect("shared resources");
        let memory = QueryMemory::new(&shared, 1024 * 1024).expect("query allowance");
        let retained = View {
            token: QueryView::new(
                StoreInstanceId::new(1).expect("store identity"),
                GraphGeneration::new(0),
            ),
            lease: store.snapshot().expect("retained lease"),
        };
        let first_control = QueryControl::Cancel(CancelToken::new());
        let second_control = QueryControl::Cancel(CancelToken::new());
        let mut first =
            RuntimeContext::new(&retained, &first_control, &memory, RuntimeLimits::default())
                .expect("first runtime");
        let mut second = RuntimeContext::new(
            &retained,
            &second_control,
            &memory,
            RuntimeLimits::default(),
        )
        .expect("second runtime");
        let second_initial: WorkCounters = second.counters();
        {
            let mut first_resources =
                TreeResources::for_query(&mut first).expect("first resources");
            let mut second_resources =
                TreeResources::for_query(&mut second).expect("second resources");
            let mut scratch =
                RangeScratch::for_query(&memory, &mut first_resources).expect("first scratch");
            let initial_edges = scratch.edges.as_mut_slice().to_vec();
            let second_work = second_resources.work();
            let query_charge = memory.reserved_bytes();
            let shared_charge = shared.reserved_bytes().expect("shared charge");
            let source = CountingSource(Cell::new(0));
            let base = PhysicalRef {
                artifact: ArtifactId::new(1).expect("artifact identity"),
                offset: 96,
                length: 128,
                kind: BlockKind::AdjacencyBase,
                version: 1,
            };
            let descriptor = RangeDescriptor::new(
                RangeKey {
                    node: NodeId::new(1).expect("node identity"),
                    rel_type: RelTypeId::new(1).expect("relationship type"),
                    direction: Direction::Out,
                    lower: RelId::new(1).expect("relationship identity"),
                    upper: UpperBound::Infinity,
                },
                0,
                1,
                base,
                &[],
                0,
            )
            .expect("shape-valid descriptor");
            assert!(matches!(
                validate_descriptor(
                    &source,
                    StoreInstanceId::new(1).expect("store identity"),
                    GraphGeneration::new(0),
                    descriptor,
                    0,
                    &mut scratch,
                    &mut second_resources,
                ),
                Err(TreeError::Invalid("query range scratch owner mismatch"))
            ));
            assert_eq!(source.0.get(), 0);
            assert_eq!(second_resources.work(), second_work);
            assert_eq!(scratch.edges.as_mut_slice(), initial_edges);
            assert_eq!(memory.reserved_bytes(), query_charge);
            assert_eq!(
                shared.reserved_bytes().expect("shared charge"),
                shared_charge
            );
        }
        assert_eq!(second.counters(), second_initial);

        drop((first, second));
        drop(retained);
        store.close().expect("close");
    }
}
