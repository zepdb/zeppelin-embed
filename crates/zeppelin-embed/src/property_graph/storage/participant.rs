//! Native directory preparation from the identity-owned normalized batch. The
//! returned candidate cannot publish itself; writes retains/rechecks the base
//! lease/token and owns artifact creation, abort inventory, WAL and publication.
use super::artifact::BlockKind;
use super::memory::{StorageBuffer, StorageMemory, StorageReservation};
use super::payload::{PayloadRef, prepare_payload};
use super::records::*;
use super::stream::PayloadSlice;
use super::tree::TreeKind;
use super::tree::directory::*;
use crate::epoch::EmbeddingTower;
use crate::property_graph::catalog::{LabelId, Symbol, SymbolEntry, SymbolKind};
use crate::property_graph::staging::{BaseIdentity, NormalizedDelta, StagedBatch};
use crate::property_graph::{EntityId, ExpectedGraphState, GraphGeneration, GraphRevision};

/// Metadata borrowed from one coordinator-retained base. Constructing this does
/// not admit a GraphReadView: the admission owner binds identity to these roots.
#[derive(Clone, Copy, Debug)]
pub struct DirectoryBase {
    /// Exact coherent graph/search envelope token retained by staging.
    pub identity: BaseIdentity,
    /// Native roots from that same retained envelope.
    pub roots: GraphRoots,
}
/// Base catalog adapter bound to the same retained publication token. The actual
/// adapter must retain its admitted lease, not synthesize a numeric generation.
/// This module merges the batch's authentic new symbol assignments itself.
pub trait PreparationCatalog<S: BlockSource>: RecordCatalog<S> {
    /// Complete store/generation/root-envelope identity of the retained catalog.
    fn base_identity(&self) -> BaseIdentity;
}
/// Private successful native-directory candidate. OUT/IN are retained unchanged
/// for ZE44's required adjacency integration; this is not a publishable graph.
pub struct NativeDirectoryCandidate<'a> {
    expected: BaseIdentity,
    roots: GraphRoots,
    _charge: StorageReservation<'a>,
}
impl NativeDirectoryCandidate<'_> {
    /// Exact base the sole coordinator must recheck before complete publication.
    pub const fn expected_base(&self) -> BaseIdentity {
        self.expected
    }
    /// Proposed native roots; remaining participant roots stay explicitly owned.
    pub const fn roots(&self) -> GraphRoots {
        self.roots
    }
}
/// Prepare all normalized changes against the retained exact base. Any error
/// returns no candidate. The caller still owns every partial private sink object
/// and must preserve its abort inventory; an error grants no cleanup/publication
/// authority and does not assert that physical writes were rolled back.
pub fn prepare_directories<'a, S: BlockSink>(
    sink: &mut S,
    batch: &StagedBatch<'_>,
    base: DirectoryBase,
    catalog: &impl PreparationCatalog<S>,
    document: Option<&EmbeddingTower>,
    memory: &'a StorageMemory<'a>,
    r: &mut TreeResources<'_>,
) -> Result<NativeDirectoryCandidate<'a>, TreeError> {
    r.require_preparation(memory)?;
    memory.require_batch(batch)?;
    r.step(1)?;
    if batch.base() != base.identity
        || catalog.base_identity() != base.identity
        || base.roots.store() != base.identity.store
        || base.roots.generation() != base.identity.generation
        || (base.identity.roots.is_none()
            && (base.identity.generation.get() != 0
                || base.roots.references().iter().any(Option::is_some)))
    {
        return Err(TreeError::Invalid(
            "native preparation base identity mismatch",
        ));
    }
    let charge = memory.reserve(std::mem::size_of::<NativeDirectoryCandidate<'_>>())?;
    if batch.deltas().is_empty() {
        r.step(0)?;
        return Ok(NativeDirectoryCandidate {
            expected: base.identity,
            roots: base.roots,
            _charge: charge,
        });
    }
    let generation = GraphGeneration::new(
        base.identity
            .generation
            .get()
            .checked_add(1)
            .ok_or(TreeError::Invalid("generation overflow"))?,
    );
    for delta in batch.deltas() {
        r.step(1)?;
        if delta.provenance().fields().original_generation != generation
            || delta.canonical().is_some() != delta.shape().is_some()
        {
            return Err(TreeError::Invalid("normalized change generation/shape"));
        }
    }
    let catalog = BatchCatalog {
        base: catalog,
        additions: batch.symbols(),
    };
    let mut state = PrepareState {
        roots: base.roots.for_generation(generation)?,
        catalog: &catalog,
        document,
        memory,
        scratch: TreeScratch::for_prepare(memory)?,
    };
    for delta in batch.deltas() {
        state.apply(sink, delta, r)?;
    }
    r.step(0)?;
    Ok(NativeDirectoryCandidate {
        expected: base.identity,
        roots: state.roots,
        _charge: charge,
    })
}
pub(crate) struct BatchCatalog<'a, C> {
    pub(crate) base: &'a C,
    pub(crate) additions: &'a [SymbolEntry<'a>],
}
impl<S: BlockSource, C: RecordCatalog<S>> RecordCatalog<S> for BatchCatalog<'_, C> {
    fn resolve(
        &self,
        kind: SymbolKind,
        name: PayloadSlice<'_, S>,
        r: &mut TreeResources<'_>,
    ) -> Result<Symbol, TreeError> {
        // StagedBatch owns these immutable, kind/name-sorted assignments. Its
        // private constructor and exact writer identity prevent caller forgery.
        let (mut low, mut high) = (0, self.additions.len());
        while low < high {
            r.step(1)?;
            let mid = low + (high - low) / 2;
            let entry = self
                .additions
                .get(mid)
                .ok_or(TreeError::Invalid("staged symbol index"))?;
            let mut order = (entry.symbol.kind() as u8).cmp(&(kind as u8));
            if order.is_eq() {
                order = name
                    .compare_bytes(entry.name.as_str().as_bytes(), r)?
                    .reverse();
            }
            match order {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => return Ok(entry.symbol),
            }
        }
        self.base.resolve(kind, name, r)
    }
}
struct Snapshot<'a> {
    shape: RecordShape,
    revision: GraphRevision,
    labels: StorageBuffer<'a, LabelId>,
}
struct PrepareState<'a, 'm, C> {
    roots: GraphRoots,
    catalog: &'a C,
    document: Option<&'a EmbeddingTower>,
    memory: &'m StorageMemory<'m>,
    scratch: TreeScratch<'m>,
}
impl<C> PrepareState<'_, '_, C> {
    fn snapshot<'m, S: BlockSource>(
        &self,
        source: &S,
        entity: EntityId,
        reference: PayloadRef,
        generation: GraphGeneration,
        memory: &'m StorageMemory<'m>,
        r: &mut TreeResources<'_>,
    ) -> Result<Snapshot<'m>, TreeError>
    where
        C: RecordCatalog<S>,
    {
        let record = verify_record(
            PayloadSlice::new(source, self.roots.store(), generation, reference),
            entity,
            self.catalog,
            self.document,
            r,
        )?;
        let count = match record.shape() {
            RecordShape::Node { labels, .. } => labels as usize,
            RecordShape::Relationship { .. } => 0,
        };
        let mut labels = StorageBuffer::new(memory, count)?;
        for index in 0..count {
            r.step(1)?;
            labels.push(record.label(index as u32, r)?)?;
        }
        Ok(Snapshot {
            shape: record.shape(),
            revision: record.revision(),
            labels,
        })
    }
    fn apply<S: BlockSink>(
        &mut self,
        sink: &mut S,
        delta: &NormalizedDelta<'_>,
        r: &mut TreeResources<'_>,
    ) -> Result<(), TreeError>
    where
        C: RecordCatalog<S>,
    {
        r.step(1)?;
        let fields = delta.provenance().fields();
        let (kind, id) = entity_key(fields.incarnation);
        let directory = self.roots.directory(kind)?;
        let old = match lookup_entry(sink, directory, &id.to_le_bytes(), r)? {
            Some(entry) => {
                let reference = PayloadRef::decode(entry.value())?;
                Some(self.snapshot(
                    sink,
                    fields.incarnation,
                    reference,
                    entry.creation_generation(),
                    self.memory,
                    r,
                )?)
            }
            None => None,
        };
        match (fields.expected, &old) {
            (ExpectedGraphState::Entity(expected), Some(old))
                if expected == old.shape.incarnation()
                    && old.revision < fields.installed_revision => {}
            (ExpectedGraphState::Absent | ExpectedGraphState::Deletion(_), None) => {}
            _ => return Err(TreeError::Invalid("normalized change/base record mismatch")),
        }
        let generation = self.roots.generation();
        let store = self.roots.store();
        let provenance =
            prepare_provenance(sink, store, generation, delta.provenance(), self.memory, r)?;
        let canonical = delta
            .canonical()
            .map(|bytes| {
                prepare_payload(sink, store, generation, BlockKind::CanonicalImage, bytes, r)
            })
            .transpose()?;
        let (record, new) = if let Some(canonical) = canonical {
            let reference = prepare_record(
                sink,
                RecordInput {
                    store,
                    generation,
                    entity: fields.incarnation,
                    canonical,
                    provenance,
                },
                self.catalog,
                self.document,
                self.memory,
                r,
            )?;
            let new = self.snapshot(
                sink,
                fields.incarnation,
                reference,
                generation,
                self.memory,
                r,
            )?;
            if let (Some(old), RecordShape::Relationship { .. }) = (&old, new.shape)
                && old.shape != new.shape
            {
                return Err(TreeError::Invalid("relationship topology changed"));
            }
            (Some(reference), Some(new))
        } else {
            let reference = match fields.incarnation {
                EntityId::Node(id) => Some(prepare_node_tombstone(
                    sink, store, generation, id, provenance, r,
                )?),
                EntityId::Relationship(_) => None,
            };
            (reference, None)
        };
        self.memberships(sink, old.as_ref(), new.as_ref(), r)?;
        let updated = if let Some(record) = record {
            let mut bytes = [0; 48];
            record.encode_into(&mut bytes)?;
            insert_checked(
                sink,
                DirectoryMutation::new(
                    directory,
                    generation,
                    NativeDirectoryValues::new(self.catalog, self.document),
                ),
                &id.to_le_bytes(),
                &bytes,
                &mut self.scratch,
                r,
            )?
        } else {
            remove_checked(
                sink,
                DirectoryMutation::new(
                    directory,
                    generation,
                    NativeDirectoryValues::new(self.catalog, self.document),
                ),
                &id.to_le_bytes(),
                &mut self.scratch,
                r,
            )?
        };
        self.roots.replace(updated)?;
        if let Some(key) = fields.key {
            let stored =
                verify_provenance(PayloadSlice::new(sink, store, generation, provenance), r)?;
            let stored_key = stored
                .key()
                .ok_or(TreeError::Invalid("normalized key disappeared"))?;
            let Symbol::Namespace(namespace) =
                self.catalog
                    .resolve(SymbolKind::Namespace, stored_key.namespace(), r)?
            else {
                return Err(TreeError::Invalid("normalized namespace domain"));
            };
            let probe = FenceKey::new(key.kind(), namespace, key.key().as_str())?;
            let value = prepare_fence(
                sink,
                FenceInput {
                    store,
                    generation,
                    key: probe,
                    provenance,
                    canonical,
                },
                self.catalog,
                self.document,
                r,
            )?;
            let root = insert_fence_checked(
                sink,
                DirectoryMutation::new(
                    self.roots.directory(TreeKind::KeyFences)?,
                    generation,
                    NativeDirectoryValues::new(self.catalog, self.document),
                ),
                probe,
                &value,
                &mut self.scratch,
                r,
            )?;
            self.roots.replace(root)?;
        }
        r.step(0)
    }
    fn memberships<S: BlockSink>(
        &mut self,
        sink: &mut S,
        old: Option<&Snapshot<'_>>,
        new: Option<&Snapshot<'_>>,
        r: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        let before = old.map_or(&[][..], |old| old.labels.as_slice());
        let after = new.map_or(&[][..], |new| new.labels.as_slice());
        let (mut left, mut right) = (0, 0);
        while left < before.len() || right < after.len() {
            r.step(1)?;
            let a = before.get(left).copied();
            let b = after.get(right).copied();
            match (a, b) {
                (Some(a), Some(b)) if a == b => {
                    left += 1;
                    right += 1;
                }
                (Some(a), b) if b.is_none_or(|b| a < b) => {
                    let old = old.ok_or(TreeError::Invalid("missing old membership record"))?;
                    self.membership(
                        sink,
                        TreeKind::Labels,
                        a.get(),
                        old.shape.incarnation(),
                        false,
                        r,
                    )?;
                    left += 1;
                }
                (_, Some(b)) => {
                    let new = new.ok_or(TreeError::Invalid("missing new membership record"))?;
                    self.membership(
                        sink,
                        TreeKind::Labels,
                        b.get(),
                        new.shape.incarnation(),
                        true,
                        r,
                    )?;
                    right += 1;
                }
                _ => return Err(TreeError::Invalid("membership merge did not progress")),
            }
        }
        let type_id = |snapshot: &Snapshot<'_>| match snapshot.shape {
            RecordShape::Relationship {
                relationship_type, ..
            } => Some(relationship_type),
            _ => None,
        };
        let a = old.and_then(type_id);
        let b = new.and_then(type_id);
        if a != b {
            if let (Some(id), Some(old)) = (a, old) {
                self.membership(
                    sink,
                    TreeKind::RelationshipTypes,
                    id.get(),
                    old.shape.incarnation(),
                    false,
                    r,
                )?;
            }
            if let (Some(id), Some(new)) = (b, new) {
                self.membership(
                    sink,
                    TreeKind::RelationshipTypes,
                    id.get(),
                    new.shape.incarnation(),
                    true,
                    r,
                )?;
            }
        }
        Ok(())
    }
    fn membership(
        &mut self,
        sink: &mut impl BlockSink,
        kind: TreeKind,
        symbol: u64,
        entity: EntityId,
        add: bool,
        r: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        let (_, id) = entity_key(entity);
        let mut key = [0; 24];
        super::artifact::put(&mut key, 0, &symbol.to_le_bytes())?;
        super::artifact::put(&mut key, 8, &id.to_le_bytes())?;
        let root = self.roots.directory(kind)?;
        let existing = lookup_entry(sink, root, &key, r)?;
        if existing.is_some_and(|entry| !entry.value().is_empty()) || existing.is_some() == add {
            return Err(TreeError::Invalid("membership index/base mismatch"));
        }
        let root = if add {
            insert_checked(
                sink,
                DirectoryMutation::new(root, self.roots.generation(), MembershipValues),
                &key,
                &[],
                &mut self.scratch,
                r,
            )?
        } else {
            remove_checked(
                sink,
                DirectoryMutation::new(root, self.roots.generation(), MembershipValues),
                &key,
                &mut self.scratch,
                r,
            )?
        };
        self.roots.replace(root)
    }
}
fn entity_key(entity: EntityId) -> (TreeKind, u128) {
    match entity {
        EntityId::Node(id) => (TreeKind::Nodes, id.get()),
        EntityId::Relationship(id) => (TreeKind::Relationships, id.get()),
    }
}

struct MembershipValues;
impl<S: BlockSource> LeafValidator<S> for MembershipValues {
    fn verify(
        &mut self,
        _: &S,
        root: DirectoryRoot,
        entry: DirectoryEntry<'_>,
        r: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        entry.require_root(root)?;
        if !matches!(root.kind(), TreeKind::Labels | TreeKind::RelationshipTypes)
            || !entry.value().is_empty()
        {
            return Err(TreeError::Invalid("membership leaf role/value"));
        }
        r.step(0)
    }
}
