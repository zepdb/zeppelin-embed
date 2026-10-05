//! One private native-record/OUT/IN candidate from the authentic staged batch.
use super::*;
use crate::epoch::EmbeddingTower;
use crate::property_graph::storage::{
    memory::{StorageBuffer, StorageMemory, StorageReservation},
    participant::{
        BatchCatalog, DirectoryBase, NativeDirectoryCandidate, PreparationCatalog,
        prepare_directories,
    },
    records::sort_by_symbol,
    tree::{TreeKind, directory::*},
};
use crate::property_graph::{
    EntityId, GraphDeleteMode, GraphGeneration,
    staging::{BaseIdentity, StagedBatch},
    wal::{self, CommitState, WalGraphRoots},
};
mod ranges;
pub(crate) use ranges::relocate_ranges;

/// Exact metadata borrowed from the sole coordinator's retained base. The
/// catalog/root-envelope token and this WAL state must have one admission owner.
#[derive(Clone, Copy, Debug)]
pub struct NativeGraphBase<'a> {
    /// Coherent native roots and complete logical base identity used by staging.
    pub directories: DirectoryBase,
    /// Real committed sequence and exact required root descriptors from that view.
    pub committed: CommitState<'a>,
}
/// Successful whole native participant. All record and directional changes are
/// private until this one result exists; it has no publication/sync authority.
pub struct NativeGraphCandidate<'a> {
    directories: NativeDirectoryCandidate<'a>,
    roots: GraphRoots,
    expected_sequence: u64,
    expected_roots: WalGraphRoots,
    expected_catalog: wal::RequiredRef,
    expected_vector: Option<wal::RequiredRef>,
    expected_text: Option<wal::RequiredRef>,
    expected_reclaim: Option<wal::RequiredRef>,
    expected_high_waters: wal::HighWaters,
    expected_prepared_inventories: StorageBuffer<'a, wal::RequiredRef>,
    target_generation: GraphGeneration,
    sequence: u64,
    _charge: StorageReservation<'a>,
}
impl NativeGraphCandidate<'_> {
    /// Complete staging token retained for the coordinator's publication recheck.
    pub const fn expected_base(&self) -> BaseIdentity {
        self.directories.expected_base()
    }
    /// Independent committed WAL cutoff the coordinator must still observe.
    pub const fn expected_sequence(&self) -> u64 {
        self.expected_sequence
    }
    /// Every optional required root, including complete object metadata.
    pub const fn expected_roots(&self) -> WalGraphRoots {
        self.expected_roots
    }
    /// Exact admitted catalog descriptor required at handoff.
    pub const fn expected_catalog(&self) -> wal::RequiredRef {
        self.expected_catalog
    }
    /// Exact optional admitted vector descriptor required at handoff.
    pub const fn expected_vector(&self) -> Option<wal::RequiredRef> {
        self.expected_vector
    }
    /// Exact optional admitted text descriptor required at handoff.
    pub const fn expected_text(&self) -> Option<wal::RequiredRef> {
        self.expected_text
    }
    /// Exact optional admitted reclaim descriptor required at handoff.
    pub const fn expected_reclaim(&self) -> Option<wal::RequiredRef> {
        self.expected_reclaim
    }
    /// Complete admitted allocation high-water identities required at handoff.
    pub const fn expected_high_waters(&self) -> wal::HighWaters {
        self.expected_high_waters
    }
    /// Exact admitted prepared inventory descriptors required at handoff.
    pub fn expected_prepared_inventories(&self) -> &[wal::RequiredRef] {
        self.expected_prepared_inventories.as_slice()
    }
    /// Generation assigned to every newly prepared artifact.
    pub const fn target_generation(&self) -> GraphGeneration {
        self.target_generation
    }
    /// All eight proposed native roots, including both adjacency directions.
    pub const fn roots(&self) -> GraphRoots {
        self.roots
    }
    /// One target sequence, or the original cutoff for an empty/no-op batch.
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }
}

/// Prepare one complete native participant. Any failure yields no candidate;
/// the caller retains every private pack and its abort inventory for writes.
pub fn prepare_native_graph<'a, S: BlockSink>(
    sink: &mut S,
    batch: &StagedBatch<'_>,
    base: NativeGraphBase<'_>,
    catalog: &impl PreparationCatalog<S>,
    document: Option<&EmbeddingTower>,
    memory: &'a StorageMemory<'a>,
    r: &mut TreeResources<'_>,
) -> Result<NativeGraphCandidate<'a>, TreeError> {
    r.require_preparation(memory)?;
    memory.require_batch(batch)?;
    bind_base(sink, base, r)?;
    let charge = memory.reserve(std::mem::size_of::<NativeGraphCandidate<'_>>())?;
    let inventory_count = base
        .committed
        .prepared_inventories
        .len()
        .map_err(TreeError::WalMetadata)?;
    let mut expected_prepared_inventories = StorageBuffer::new(memory, inventory_count)?;
    let mut no_cancel = || false;
    let mut wal_resources =
        wal::WalResources::new(u64::MAX, wal::STACK_RESERVATION_BYTES, &mut no_cancel)
            .map_err(TreeError::WalMetadata)?;
    for index in 0..inventory_count {
        expected_prepared_inventories.push(
            base.committed
                .prepared_inventories
                .get(index, &mut wal_resources)
                .map_err(TreeError::WalMetadata)?,
        )?;
    }
    #[cfg(any(test, feature = "test-support"))]
    let unchanged = batch.disposition() != crate::property_graph::BatchDisposition::Changed;
    #[cfg(not(any(test, feature = "test-support")))]
    let unchanged = batch.deltas().is_empty();
    let context = if unchanged {
        None
    } else {
        Some(RangeEditContext::new(
            base.directories.identity.generation,
            base.committed.sequence,
        )?)
    };
    // This uses the admitted pre-batch roots even when both nodes are pending
    // deletion. DETACH never calls the incident reader or enumerates its degree.
    check_restrict(sink, batch, base, catalog, document, memory, r)?;
    let directories =
        prepare_directories(sink, batch, base.directories, catalog, document, memory, r)?;
    let mut roots = directories.roots();
    if let Some(context) = context {
        let merged = BatchCatalog {
            base: catalog,
            additions: batch.symbols(),
        };
        let mut changes = collect_changes(
            sink,
            batch,
            base.directories.roots,
            roots,
            base.committed.sequence,
            context.target_sequence(),
            &merged,
            document,
            memory,
            r,
        )?;
        sort_by_symbol(changes.as_mut_slice(), Change::sort_key, r)?;
        for pair in changes.as_slice().windows(2) {
            r.step(1)?;
            if pair.first().map(Change::sort_key) == pair.last().map(Change::sort_key) {
                return Err(invalid("duplicate directed normalized change"));
            }
        }
        if !changes.as_slice().is_empty() {
            ranges::apply(
                sink,
                &mut roots,
                changes.as_slice(),
                base.committed.sequence,
                context,
                memory,
                r,
            )?;
        }
    }
    r.step(0)?;
    Ok(NativeGraphCandidate {
        directories,
        roots,
        expected_sequence: base.committed.sequence,
        expected_roots: base.committed.graph,
        expected_catalog: base.committed.catalog,
        expected_vector: base.committed.vector,
        expected_text: base.committed.text,
        expected_reclaim: base.committed.reclaim,
        expected_high_waters: base.committed.high_waters,
        expected_prepared_inventories,
        target_generation: roots.generation(),
        sequence: context.map_or(base.committed.sequence, RangeEditContext::target_sequence),
        _charge: charge,
    })
}

fn bind_base(
    source: &impl BlockSource,
    base: NativeGraphBase<'_>,
    r: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    r.step(1)?;
    let identity = base.directories.identity;
    let state = base.committed;
    if state.store != identity.store
        || state.generation != identity.generation
        || base.directories.roots.store() != identity.store
        || base.directories.roots.generation() != identity.generation
    {
        return Err(invalid("native WAL/base identity mismatch"));
    }
    for (reference, required) in base
        .directories
        .roots
        .references()
        .into_iter()
        .zip(state.graph.slots)
    {
        r.step(std::mem::size_of::<Option<wal::RequiredRef>>() as u64)?;
        if reference != required.map(|required| required.block) {
            return Err(invalid("native WAL root position mismatch"));
        }
        if let Some(required) = required {
            wal::validate_graph_root_reference(state, required).map_err(TreeError::WalMetadata)?;
            let block = source.resolve(required.block, r)?;
            let object = required.object;
            let identity = block.identity();
            if block.reference() != required.block
                || identity.store != object.store
                || identity.artifact != object.artifact
                || identity.generation != object.generation
                || identity.creation_serial != object.serial
                || block.file_length() != object.bytes as usize
                || block.file_checksum() != object.checksum
            {
                return Err(invalid("native WAL root source identity mismatch"));
            }
        }
    }
    r.step(0)
}

#[derive(Clone, Copy)]
struct Change {
    node: NodeId,
    relationship_type: RelTypeId,
    direction: Direction,
    delta: DeltaEntry,
}
impl Change {
    fn sort_key(&self) -> (u8, u128, u64, u128) {
        (
            self.direction as u8,
            self.node.get(),
            self.relationship_type.get(),
            self.delta.edge.rel.get(),
        )
    }
    fn group(&self) -> (u8, u128, u64) {
        (
            self.direction as u8,
            self.node.get(),
            self.relationship_type.get(),
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_changes<'a, S: BlockSource>(
    source: &S,
    batch: &StagedBatch<'_>,
    old: GraphRoots,
    next: GraphRoots,
    base_sequence: u64,
    sequence: u64,
    catalog: &impl crate::property_graph::storage::records::RecordCatalog<S>,
    document: Option<&EmbeddingTower>,
    memory: &'a StorageMemory<'a>,
    r: &mut TreeResources<'_>,
) -> Result<StorageBuffer<'a, Change>, TreeError> {
    let mut count = 0usize;
    for delta in batch.deltas() {
        r.step(1)?;
        if matches!(
            delta.provenance().fields().incarnation,
            EntityId::Relationship(_)
        ) {
            count = count.checked_add(2).ok_or(TreeError::Memory)?;
        }
    }
    let mut changes = StorageBuffer::new(memory, count)?;
    let previous = NativeGraphReader::new(source, old, base_sequence, catalog, document);
    let current = NativeGraphReader::new(source, next, sequence, catalog, document);
    for delta in batch.deltas() {
        r.step(1)?;
        let EntityId::Relationship(id) = delta.provenance().fields().incarnation else {
            continue;
        };
        let old = previous.raw_relationship(id, r)?;
        let new = current.raw_relationship(id, r)?;
        let (row, action) = match (old, new) {
            (Some(old), Some(new)) if old == new => continue,
            (None, Some(new)) if current.visible(new, r)? => (new, Action::Insert),
            (Some(old), None) => (old, Action::Delete),
            _ => return Err(invalid("normalized relationship topology/endpoints")),
        };
        for (direction, node, neighbor) in [
            (Direction::Out, row.source, row.target),
            (Direction::In, row.target, row.source),
        ] {
            r.step(std::mem::size_of::<Change>() as u64)?;
            changes.push(Change {
                node,
                relationship_type: row.relationship_type,
                direction,
                delta: DeltaEntry {
                    edge: Edge {
                        rel: row.rel,
                        neighbor,
                    },
                    action,
                },
            })?;
        }
    }
    r.step(0)?;
    Ok(changes)
}

fn check_restrict<S: BlockSource>(
    source: &S,
    batch: &StagedBatch<'_>,
    base: NativeGraphBase<'_>,
    catalog: &impl PreparationCatalog<S>,
    document: Option<&EmbeddingTower>,
    memory: &StorageMemory<'_>,
    r: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    let mut restrict = false;
    let mut removed_count = 0usize;
    for delta in batch.deltas() {
        r.step(1)?;
        let fields = delta.provenance().fields();
        if delta.canonical().is_none() {
            match fields.incarnation {
                EntityId::Node(_) if fields.delete_mode == Some(GraphDeleteMode::Restrict) => {
                    restrict = true
                }
                EntityId::Relationship(_) => {
                    removed_count = removed_count.checked_add(1).ok_or(TreeError::Memory)?
                }
                _ => {}
            }
        }
    }
    if !restrict {
        return r.step(0);
    }
    let mut removed = StorageBuffer::new(memory, removed_count)?;
    for delta in batch.deltas() {
        r.step(1)?;
        if delta.canonical().is_none()
            && let EntityId::Relationship(id) = delta.provenance().fields().incarnation
        {
            removed.push(id)?;
        }
    }
    sort_by_symbol(removed.as_mut_slice(), |id| *id, r)?;
    let mut scratch = RangeScratch::for_prepare(memory, r)?;
    let reader = NativeGraphReader::new(
        source,
        base.directories.roots,
        base.committed.sequence,
        catalog,
        document,
    );
    for delta in batch.deltas() {
        r.step(1)?;
        let fields = delta.provenance().fields();
        if delta.canonical().is_none()
            && fields.delete_mode == Some(GraphDeleteMode::Restrict)
            && let EntityId::Node(id) = fields.incarnation
        {
            for direction in [Direction::Out, Direction::In] {
                reader.visit_adjacency(
                    AdjacencyQuery {
                        node: id,
                        direction,
                        relationship_type: None,
                        relationships: RelationshipRange {
                            lower: RelId::new(1).map_err(|_| invalid("minimum relationship ID"))?,
                            upper: UpperBound::Infinity,
                        },
                    },
                    &mut scratch,
                    r,
                    &mut |row, r| {
                        if removed.as_slice().binary_search(&row.edge.rel).is_ok() {
                            return Ok(true);
                        }
                        if catalog
                            .relationship_on_delete(row.relationship_type, r)?
                            .is_some()
                        {
                            if direction == Direction::Out {
                                return Ok(true);
                            }
                            for change in batch.deltas() {
                                r.step(1)?;
                                if change.canonical().is_none()
                                    && change.provenance().fields().incarnation
                                        == EntityId::Node(row.edge.neighbor)
                                {
                                    return Ok(true);
                                }
                            }
                        }
                        Err(invalid("plain node deletion retains a live incident"))
                    },
                )?;
            }
        }
    }
    r.step(0)
}
fn invalid(message: &'static str) -> TreeError {
    TreeError::Invalid(message)
}
