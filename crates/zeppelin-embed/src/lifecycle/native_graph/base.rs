//! Authentic admitted-base adaptation for the retained native storage view.

use super::{NativeGraphError, NativeReadLease};
use crate::property_graph::catalog::{GraphInterpretation, Symbol, SymbolKind};
use crate::property_graph::staging::{
    AdmittedBase, BaseEntity, BaseIdentity, BaseKeyState, CanonicalSource, HighWaters, Membership,
    StageError, StructuredOperation, StructuredWrite, WriteControl, WriteImage,
};
use crate::property_graph::storage::adjacency::{NativeGraphReader, RangeScratch};
use crate::property_graph::storage::memory::{StorageBuffer, StorageMemory};
use crate::property_graph::storage::records::{
    CanonicalShape, NodeRecordState, RecordView, StoredProvenance, verify_fence_entry,
    verify_node_state, verify_record,
};
use crate::property_graph::storage::search::{Modality, SparseRoots, SparseView};
use crate::property_graph::storage::stream::{PayloadCursor, PayloadSlice};
use crate::property_graph::storage::tree::TreeKind;
use crate::property_graph::storage::tree::directory::{
    FenceKey, TreeError, TreeResources, lookup_entry, lookup_fence_entry,
};
use crate::property_graph::storage::{NativePreparationCatalog, NativePreparationSource};
use crate::property_graph::{
    ApplicationKey, CanonicalFingerprint, EntityId, EntityKind, EntityShape, ExpectedGraphState,
    GraphDeleteMode, GraphName, GraphOperation, GraphRevision, NodeId, NodeRef, OperationFields,
    OperationProvenance, PropertyValue, RelId,
};
use std::cell::{Cell, RefCell, RefMut};
use xxhash_rust::xxh3::Xxh3;

struct ChargedText<'m> {
    bytes: StorageBuffer<'m, u8>,
}

impl ChargedText<'_> {
    fn as_str(&self) -> Result<&str, TreeError> {
        std::str::from_utf8(self.bytes.as_slice())
            .map_err(|_| TreeError::Invalid("invalid admitted UTF-8"))
    }
}

struct CachedCanonical<'source, 'resources, 'm> {
    source: &'source NativePreparationSource<'source, 'm>,
    store: crate::property_graph::StoreInstanceId,
    generation: crate::property_graph::GraphGeneration,
    payload: crate::property_graph::storage::payload::PayloadRef,
    resources: &'resources RefCell<&'resources mut TreeResources<'m>>,
    first_error: &'resources Cell<Option<TreeError>>,
}

impl CanonicalSource for CachedCanonical<'_, '_, '_> {
    fn read_at(&self, offset: u64, output: &mut [u8]) -> std::io::Result<usize> {
        let mut resources = self
            .resources
            .try_borrow_mut()
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::WouldBlock))?;
        let slice = PayloadSlice::new(self.source, self.store, self.generation, self.payload);
        slice
            .read_at(offset, output, &mut resources)
            .map_err(|error| {
                let first = self.first_error.take().unwrap_or(error);
                self.first_error.set(Some(first));
                std::io::Error::from(std::io::ErrorKind::Other)
            })
    }
}

enum CachedShape<'m> {
    Node,
    Relationship {
        source: NodeId,
        target: NodeId,
        relationship_type: ChargedText<'m>,
    },
}

enum CachedPropertyValue<'m> {
    String(ChargedText<'m>),
    Bool(bool),
    I64(i64),
    F64(f64),
    EmptyList,
    Strings {
        _owners: StorageBuffer<'m, ChargedText<'m>>,
        views: StorageBuffer<'m, &'m str>,
    },
    Bools(StorageBuffer<'m, bool>),
    Integers(StorageBuffer<'m, i64>),
    Floats(StorageBuffer<'m, f64>),
}

impl CachedPropertyValue<'_> {
    fn value(&self) -> Result<PropertyValue<'_>, StageError> {
        let data = match self {
            Self::String(value) => crate::property_graph::PropertyData::String(value.as_str()?),
            Self::Bool(value) => crate::property_graph::PropertyData::Bool(*value),
            Self::I64(value) => crate::property_graph::PropertyData::I64(*value),
            Self::F64(value) => crate::property_graph::PropertyData::F64(*value),
            Self::EmptyList => crate::property_graph::PropertyData::EmptyList { count: 0 },
            Self::Strings { views, .. } => {
                crate::property_graph::PropertyData::Strings(views.as_slice())
            }
            Self::Bools(values) => crate::property_graph::PropertyData::Bools(values.as_slice()),
            Self::Integers(values) => {
                crate::property_graph::PropertyData::Integers(values.as_slice())
            }
            Self::Floats(values) => crate::property_graph::PropertyData::Floats(values.as_slice()),
        };
        PropertyValue::new(data).map_err(|_| StageError::InvalidInput)
    }
}

struct CachedProperty<'m> {
    name: ChargedText<'m>,
    value: CachedPropertyValue<'m>,
}

struct CachedEntity<'source, 'resources, 'm> {
    kind: EntityKind,
    namespace: Option<ChargedText<'m>>,
    key: Option<ChargedText<'m>>,
    operation: GraphOperation,
    requested_revision: GraphRevision,
    installed_revision: GraphRevision,
    expected: ExpectedGraphState,
    incarnation: EntityId,
    delete_mode: Option<GraphDeleteMode>,
    original_generation: crate::property_graph::GraphGeneration,
    shape: Option<CachedShape<'m>>,
    canonical: Option<CachedCanonical<'source, 'resources, 'm>>,
    fingerprint: Option<CanonicalFingerprint>,
    membership: Membership,
    evidence_only: bool,
    text: Option<ChargedText<'m>>,
    properties: StorageBuffer<'m, CachedProperty<'m>>,
}

impl CachedEntity<'_, '_, '_> {
    /// Application key, when this entity has one. Absence (a Cypher-created
    /// entity) is distinct from an empty namespace/key and is never forced
    /// into one: `namespace` and `key` are always both present or both
    /// absent, by construction in `cached_from_parts`.
    fn key(&self) -> Result<Option<ApplicationKey<'_>>, StageError> {
        match (&self.namespace, &self.key) {
            (Some(namespace), Some(key)) => Ok(Some(
                ApplicationKey::new(self.kind, namespace.as_str()?, key.as_str()?)
                    .map_err(|_| StageError::InvalidInput)?,
            )),
            (None, None) => Ok(None),
            _ => Err(StageError::InvalidInput),
        }
    }

    fn provenance(&self) -> Result<OperationProvenance<'_>, StageError> {
        OperationProvenance::from_fields(
            Some(1),
            OperationFields {
                operation: self.operation,
                key: self.key()?,
                requested_revision: self.requested_revision,
                installed_revision: self.installed_revision,
                expected: self.expected,
                incarnation: self.incarnation,
                delete_mode: self.delete_mode,
                original_generation: self.original_generation,
            },
        )
        .map_err(StageError::Canonical)
    }

    fn live(&self, view: BaseIdentity) -> Result<Option<BaseEntity<'_>>, StageError> {
        let (Some(shape), Some(canonical), Some(fingerprint)) =
            (&self.shape, &self.canonical, self.fingerprint)
        else {
            return Ok(None);
        };
        let shape = match shape {
            CachedShape::Node => EntityShape::Node,
            CachedShape::Relationship {
                source,
                target,
                relationship_type,
            } => EntityShape::Relationship {
                source: *source,
                target: *target,
                relationship_type: GraphName::new(relationship_type.as_str()?)
                    .map_err(|_| StageError::InvalidInput)?,
            },
        };
        Ok(Some(BaseEntity {
            view,
            provenance: self.provenance()?,
            shape,
            fingerprint,
            source: canonical,
            membership: self.membership,
        }))
    }
}

/// Largest lazy target arena a caller may request. A query-driven executor
/// discovers its targets while it runs, so the arena is admitted once at
/// construction and never grows; a caller that asks for more is refused.
pub(super) const MAX_LAZY_TARGETS: usize = 4096;

/// One lazily resolved target lives in its own single-element buffer. A later
/// arrival therefore appends to the slot array without touching the bytes an
/// earlier caller is still borrowing.
type LazySlot<'source, 'resources, 'm> = StorageBuffer<'m, CachedEntity<'source, 'resources, 'm>>;

/// Bounded interior-mutable cache for targets no structured-write list named.
struct LazyTargets<'source, 'resources, 'm> {
    slots: RefCell<StorageBuffer<'m, LazySlot<'source, 'resources, 'm>>>,
}

impl<'source, 'resources, 'm> LazyTargets<'source, 'resources, 'm> {
    fn new(memory: &'m StorageMemory<'m>, capacity: usize) -> Result<Self, NativeGraphError> {
        if capacity > MAX_LAZY_TARGETS {
            return Err(NativeGraphError::Invalid(
                "native base lazy target capacity",
            ));
        }
        Ok(Self {
            slots: RefCell::new(StorageBuffer::new(memory, capacity)?),
        })
    }

    fn borrowed() -> StageError {
        StageError::NativeStorage(TreeError::Invalid(
            "native base lazy targets already borrowed",
        ))
    }

    fn find(
        &self,
        id: EntityId,
    ) -> Result<Option<&CachedEntity<'source, 'resources, 'm>>, StageError> {
        let slots = self.slots.try_borrow().map_err(|_| Self::borrowed())?;
        for slot in slots.as_slice() {
            let Some(entry) = slot.as_slice().first() else {
                continue;
            };
            if entry.incarnation == id {
                let pointer: *const CachedEntity<'source, 'resources, 'm> = entry;
                // SAFETY: the entry lives in its own fixed-capacity slot buffer,
                // whose allocation is made once and is never reallocated, moved
                // into, or handed out mutably. The slot buffer is owned by
                // `self.slots` for the whole life of `self`, and appending
                // another slot only writes the outer array. Returning the
                // reference bounded by `&self` therefore cannot outlive or
                // alias the bytes it names.
                return Ok(Some(unsafe { &*pointer }));
            }
        }
        Ok(None)
    }

    fn insert(
        &self,
        memory: &'m StorageMemory<'m>,
        entity: CachedEntity<'source, 'resources, 'm>,
    ) -> Result<&CachedEntity<'source, 'resources, 'm>, StageError> {
        let mut slot = LazySlot::new(memory, 1)?;
        slot.push(entity)?;
        let pointer: *const CachedEntity<'source, 'resources, 'm> = slot
            .as_slice()
            .first()
            .ok_or(StageError::NativeStorage(TreeError::Memory))?;
        // A full arena is a loud refusal: the rejected slot drops here and
        // releases every byte it charged, so no partial target survives.
        self.slots
            .try_borrow_mut()
            .map_err(|_| Self::borrowed())?
            .push(slot)?;
        // SAFETY: as in `find`, plus the slot buffer was moved into `self.slots`
        // without touching its heap allocation, so `pointer` still names the
        // element just written and is now owned for the whole life of `self`.
        Ok(unsafe { &*pointer })
    }
}

pub(super) struct NativeAdmittedBase<'source, 'lease, 'resources, 'm> {
    lease: &'lease NativeReadLease,
    interpretation: GraphInterpretation<'lease>,
    relationship_rules: crate::property_graph::catalog::RelationshipRules<'source>,
    source: &'source NativePreparationSource<'lease, 'm>,
    memory: &'m StorageMemory<'m>,
    cache: StorageBuffer<'m, CachedEntity<'source, 'resources, 'm>>,
    lazy: Option<LazyTargets<'source, 'resources, 'm>>,
    resources: &'resources RefCell<&'resources mut TreeResources<'m>>,
    first_error: &'resources Cell<Option<TreeError>>,
}

fn copy_slice<'m, S: crate::property_graph::storage::tree::directory::BlockSource>(
    slice: PayloadSlice<'_, S>,
    memory: &'m StorageMemory<'m>,
    resources: &mut TreeResources<'_>,
) -> Result<StorageBuffer<'m, u8>, TreeError> {
    let length = usize::try_from(slice.len()).map_err(|_| TreeError::Memory)?;
    let mut bytes = StorageBuffer::new(memory, length)?;
    let mut chunk = [0_u8; crate::property_graph::storage::payload::CHUNK_BYTES];
    let mut offset = 0usize;
    while offset < length {
        let end = offset
            .checked_add(crate::property_graph::storage::payload::CHUNK_BYTES)
            .map_or(length, |end| end.min(length));
        let output = chunk.get_mut(..end - offset).ok_or(TreeError::Memory)?;
        let read = slice.read_at(offset as u64, output, resources)?;
        if read != output.len() {
            return Err(TreeError::Invalid("short admitted payload"));
        }
        bytes.extend_from_slice(output)?;
        offset = end;
    }
    Ok(bytes)
}

fn copy_text<'m, S: crate::property_graph::storage::tree::directory::BlockSource>(
    slice: PayloadSlice<'_, S>,
    memory: &'m StorageMemory<'m>,
    resources: &mut TreeResources<'_>,
) -> Result<ChargedText<'m>, TreeError> {
    let bytes = copy_slice(slice, memory, resources)?;
    std::str::from_utf8(bytes.as_slice())
        .map_err(|_| TreeError::Invalid("invalid admitted UTF-8"))?;
    Ok(ChargedText { bytes })
}

fn copy_owned_text<'m>(
    value: &str,
    memory: &'m StorageMemory<'m>,
    resources: &mut TreeResources<'_>,
) -> Result<ChargedText<'m>, TreeError> {
    let mut bytes = StorageBuffer::new(memory, value.len())?;
    for chunk in value.as_bytes().chunks(64 * 1024) {
        resources.step(chunk.len() as u64)?;
        bytes.extend_from_slice(chunk)?;
    }
    Ok(ChargedText { bytes })
}

fn property_count<S: crate::property_graph::storage::tree::directory::BlockSource>(
    cursor: &mut PayloadCursor<'_, '_, S>,
    resources: &mut TreeResources<'_>,
) -> Result<usize, TreeError> {
    usize::try_from(u64::from_le_bytes(cursor.read_array(resources)?))
        .map_err(|_| TreeError::Memory)
}

fn decode_property<'m, S: crate::property_graph::storage::tree::directory::BlockSource>(
    encoded: PayloadSlice<'_, S>,
    memory: &'m StorageMemory<'m>,
    resources: &mut TreeResources<'_>,
) -> Result<CachedPropertyValue<'m>, TreeError> {
    let mut cursor = PayloadCursor::new(encoded);
    let tag = u8::from_le_bytes(cursor.read_array::<1>(resources)?);
    let value = match tag {
        1 => CachedPropertyValue::String(copy_text(cursor.blob(resources)?, memory, resources)?),
        2 => match u8::from_le_bytes(cursor.read_array::<1>(resources)?) {
            0 => CachedPropertyValue::Bool(false),
            1 => CachedPropertyValue::Bool(true),
            _ => return Err(TreeError::Invalid("canonical boolean")),
        },
        3 => CachedPropertyValue::I64(i64::from_le_bytes(cursor.read_array(resources)?)),
        4 => CachedPropertyValue::F64(f64::from_bits(u64::from_le_bytes(
            cursor.read_array(resources)?,
        ))),
        5 => {
            if property_count(&mut cursor, resources)? != 0 {
                return Err(TreeError::Invalid("nonempty untyped property list"));
            }
            CachedPropertyValue::EmptyList
        }
        6 => {
            let count = property_count(&mut cursor, resources)?;
            let mut owners = StorageBuffer::new(memory, count)?;
            for _ in 0..count {
                owners.push(copy_text(cursor.blob(resources)?, memory, resources)?)?;
            }
            let mut views = StorageBuffer::new(memory, count)?;
            for owner in owners.as_slice() {
                let view = owner.as_str()?;
                // SAFETY: every string byte buffer is heap-stable and owned by
                // `owners` in the same enum value. Returned PropertyValue
                // borrows the enum, so no view can outlive those owners.
                let retained: &'m str = unsafe { &*(view as *const str) };
                views.push(retained)?;
            }
            CachedPropertyValue::Strings {
                _owners: owners,
                views,
            }
        }
        7 => {
            let count = property_count(&mut cursor, resources)?;
            let mut values = StorageBuffer::new(memory, count)?;
            for _ in 0..count {
                values.push(
                    match u8::from_le_bytes(cursor.read_array::<1>(resources)?) {
                        0 => false,
                        1 => true,
                        _ => return Err(TreeError::Invalid("canonical list boolean")),
                    },
                )?;
            }
            CachedPropertyValue::Bools(values)
        }
        8 => {
            let count = property_count(&mut cursor, resources)?;
            let mut values = StorageBuffer::new(memory, count)?;
            for _ in 0..count {
                values.push(i64::from_le_bytes(cursor.read_array(resources)?))?;
            }
            CachedPropertyValue::Integers(values)
        }
        9 => {
            let count = property_count(&mut cursor, resources)?;
            let mut values = StorageBuffer::new(memory, count)?;
            for _ in 0..count {
                values.push(f64::from_bits(u64::from_le_bytes(
                    cursor.read_array(resources)?,
                )))?;
            }
            CachedPropertyValue::Floats(values)
        }
        _ => return Err(TreeError::Invalid("canonical property tag")),
    };
    cursor.finish(resources)?;
    Ok(value)
}

fn streamed_fingerprint<S: crate::property_graph::storage::tree::directory::BlockSource>(
    slice: PayloadSlice<'_, S>,
    resources: &mut TreeResources<'_>,
) -> Result<CanonicalFingerprint, TreeError> {
    let mut hash = Xxh3::new();
    let mut chunk = [0_u8; crate::property_graph::storage::payload::CHUNK_BYTES];
    let mut offset = 0_u64;
    while offset < slice.len() {
        let remaining = usize::try_from(slice.len() - offset).map_err(|_| TreeError::Memory)?;
        let chunk_length = chunk.len();
        let output = chunk
            .get_mut(..remaining.min(chunk_length))
            .ok_or(TreeError::Memory)?;
        let read = slice.read_at(offset, output, resources)?;
        if read != output.len() {
            return Err(TreeError::Invalid("short admitted canonical"));
        }
        hash.update(output);
        offset = offset.checked_add(read as u64).ok_or(TreeError::Memory)?;
    }
    CanonicalFingerprint::new(slice.len(), hash.digest())
        .map_err(|_| TreeError::Invalid("invalid canonical fingerprint"))
}

#[allow(
    clippy::too_many_arguments,
    reason = "independent resource owners and lifetimes are explicit at this private seam"
)]
fn cached_from_record<'source, 'resources, 'm>(
    source: &'source NativePreparationSource<'source, 'm>,
    record: &RecordView<'_, NativePreparationSource<'source, 'm>>,
    catalog: &NativePreparationCatalog<'_, 'source, 'm>,
    membership: Membership,
    memory: &'m StorageMemory<'m>,
    resources_cell: &'resources RefCell<&'resources mut TreeResources<'m>>,
    first_error: &'resources Cell<Option<TreeError>>,
    resources: &mut TreeResources<'m>,
) -> Result<CachedEntity<'source, 'resources, 'm>, TreeError> {
    let property_capacity = catalog
        .symbol_entries()
        .iter()
        .filter(|entry| matches!(entry.symbol, Symbol::Property(_)))
        .count();
    let mut properties = StorageBuffer::new(memory, property_capacity)?;
    for entry in catalog.symbol_entries() {
        let Symbol::Property(key) = entry.symbol else {
            continue;
        };
        if let Some(encoded) = record.property(key, resources)? {
            properties.push(CachedProperty {
                name: copy_owned_text(entry.name.as_str(), memory, resources)?,
                value: decode_property(encoded, memory, resources)?,
            })?;
        }
    }
    cached_from_parts(
        source,
        record.provenance(),
        Some(record.canonical()),
        Some(record.canonical_bytes()),
        Some(
            *record
                .required_payloads()
                .first()
                .ok_or(TreeError::Invalid("canonical payload"))?,
        ),
        membership,
        memory,
        resources_cell,
        first_error,
        resources,
        Some(properties),
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "independent resource owners and lifetimes are explicit at this private seam"
)]
fn cached_from_parts<'source, 'resources, 'm>(
    source: &'source NativePreparationSource<'source, 'm>,
    provenance: &StoredProvenance<'_, NativePreparationSource<'source, 'm>>,
    canonical: Option<
        &crate::property_graph::storage::records::CanonicalView<
            '_,
            NativePreparationSource<'source, 'm>,
        >,
    >,
    canonical_bytes: Option<PayloadSlice<'_, NativePreparationSource<'source, 'm>>>,
    canonical_ref: Option<crate::property_graph::storage::payload::PayloadRef>,
    membership: Membership,
    memory: &'m StorageMemory<'m>,
    resources_cell: &'resources RefCell<&'resources mut TreeResources<'m>>,
    first_error: &'resources Cell<Option<TreeError>>,
    resources: &mut TreeResources<'m>,
    properties: Option<StorageBuffer<'m, CachedProperty<'m>>>,
) -> Result<CachedEntity<'source, 'resources, 'm>, TreeError> {
    // A Cypher CREATE admits an entity with no application key at all; that
    // absence must survive here rather than being forced into a key or
    // refused as corruption. `provenance.key()` is already `Option` for
    // exactly this reason (ZE-32/ZE-34: absence is not an empty value).
    let stored_key = provenance.key();
    let namespace = stored_key
        .map(|stored_key| copy_text(stored_key.namespace(), memory, resources))
        .transpose()?;
    let key = stored_key
        .map(|stored_key| copy_text(stored_key.key(), memory, resources))
        .transpose()?;
    let application_key = match (stored_key, &namespace, &key) {
        (Some(stored_key), Some(namespace), Some(key)) => Some(
            ApplicationKey::new(stored_key.kind(), namespace.as_str()?, key.as_str()?)
                .map_err(|_| TreeError::Invalid("invalid admitted application key"))?,
        ),
        (None, None, None) => None,
        _ => return Err(TreeError::Invalid("incomplete admitted application key")),
    };
    let fields = provenance.fields_with_key(application_key, resources)?;
    let operation = fields.operation;
    let requested_revision = fields.requested_revision;
    let installed_revision = fields.installed_revision;
    let expected = fields.expected;
    let incarnation = fields.incarnation;
    let delete_mode = fields.delete_mode;
    let original_generation = fields.original_generation;
    let (shape, canonical, fingerprint, text) = match (canonical, canonical_bytes, canonical_ref) {
        (Some(canonical), Some(canonical_bytes), Some(canonical_ref)) => {
            let shape = match canonical.shape() {
                CanonicalShape::Node { .. } => CachedShape::Node,
                CanonicalShape::Relationship {
                    source,
                    target,
                    relationship_type,
                } => CachedShape::Relationship {
                    source: *source,
                    target: *target,
                    relationship_type: copy_text(*relationship_type, memory, resources)?,
                },
            };
            let fingerprint = streamed_fingerprint(canonical_bytes, resources)?;
            let text = canonical
                .stored_text()
                .map(|text| copy_text(text, memory, resources))
                .transpose()?;
            (
                Some(shape),
                Some(CachedCanonical {
                    source,
                    store: source.lease().bundle().base().store,
                    generation: source.lease().bundle().base().generation,
                    payload: canonical_ref,
                    resources: resources_cell,
                    first_error,
                }),
                Some(fingerprint),
                text,
            )
        }
        (None, None, None) => (None, None, None, None),
        _ => return Err(TreeError::Invalid("incomplete admitted canonical")),
    };
    Ok(CachedEntity {
        kind: incarnation.kind(),
        namespace,
        key,
        operation,
        requested_revision,
        installed_revision,
        expected,
        incarnation,
        delete_mode,
        original_generation,
        shape,
        canonical,
        fingerprint,
        membership,
        evidence_only: false,
        text,
        properties: match properties {
            Some(properties) => properties,
            None => StorageBuffer::new(memory, 0)?,
        },
    })
}

#[allow(clippy::too_many_arguments)]
fn load_record<'source, 'resources, 'm>(
    source: &'source NativePreparationSource<'source, 'm>,
    roots: crate::property_graph::storage::tree::directory::GraphRoots,
    entity: EntityId,
    catalog: &NativePreparationCatalog<'_, 'source, 'm>,
    document: Option<&crate::epoch::EmbeddingTower>,
    membership: Membership,
    memory: &'m StorageMemory<'m>,
    resources_cell: &'resources RefCell<&'resources mut TreeResources<'m>>,
    first_error: &'resources Cell<Option<TreeError>>,
    resources: &mut TreeResources<'m>,
) -> Result<Option<CachedEntity<'source, 'resources, 'm>>, TreeError> {
    let (kind, key) = match entity {
        EntityId::Node(node) => (TreeKind::Nodes, node.get()),
        EntityId::Relationship(relationship) => (TreeKind::Relationships, relationship.get()),
    };
    let Some(entry) = lookup_entry(
        source,
        roots.directory(kind)?,
        &key.to_le_bytes(),
        resources,
    )?
    else {
        return Ok(None);
    };
    let payload = crate::property_graph::storage::payload::PayloadRef::decode(entry.value())?;
    let slice = PayloadSlice::new(source, roots.store(), entry.creation_generation(), payload);
    // A deleted node retains a checked tombstone under its own identity. Verify
    // those bytes and their provenance in full, then report the node as not
    // live; only a live node record answers a request naming this entity.
    let record = match entity {
        EntityId::Node(node) => {
            match verify_node_state(slice, node, catalog, document, resources)? {
                NodeRecordState::Live(record) => record,
                NodeRecordState::Tombstone(_) => return Ok(None),
            }
        }
        EntityId::Relationship(_) => verify_record(slice, entity, catalog, document, resources)?,
    };
    Ok(Some(cached_from_record(
        source,
        &record,
        catalog,
        membership,
        memory,
        resources_cell,
        first_error,
        resources,
    )?))
}

impl<'source, 'lease, 'resources, 'm> NativeAdmittedBase<'source, 'lease, 'resources, 'm>
where
    'm: 'source,
{
    /// Structured-write admission: every target is named by `requests` and is
    /// preloaded here, so a later cache miss is an absent entity, not an
    /// unasked question.
    pub(super) fn new(
        lease: &'lease NativeReadLease,
        source: &'source NativePreparationSource<'lease, 'm>,
        memory: &'m StorageMemory<'m>,
        requests: &[StructuredWrite<'_, '_>],
        resources_cell: &'resources RefCell<&'resources mut TreeResources<'m>>,
        first_error: &'resources Cell<Option<TreeError>>,
    ) -> Result<Self, NativeGraphError> {
        Self::build(
            lease,
            source,
            memory,
            requests,
            resources_cell,
            first_error,
            None,
        )
    }

    /// Query-driven admission: the same preload runs, and a miss on
    /// `entity`, `property` or `stored_text` then resolves the target from the
    /// admitted roots into a bounded arena of `lazy_capacity` entries. `key`
    /// keeps its structured-write-only meaning.
    pub(super) fn with_lazy_targets(
        lease: &'lease NativeReadLease,
        source: &'source NativePreparationSource<'lease, 'm>,
        memory: &'m StorageMemory<'m>,
        requests: &[StructuredWrite<'_, '_>],
        resources_cell: &'resources RefCell<&'resources mut TreeResources<'m>>,
        first_error: &'resources Cell<Option<TreeError>>,
        lazy_capacity: usize,
    ) -> Result<Self, NativeGraphError> {
        Self::build(
            lease,
            source,
            memory,
            requests,
            resources_cell,
            first_error,
            Some(lazy_capacity),
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "one admitted base carries its lease, source, arena and controls"
    )]
    fn build(
        lease: &'lease NativeReadLease,
        source: &'source NativePreparationSource<'lease, 'm>,
        memory: &'m StorageMemory<'m>,
        requests: &[StructuredWrite<'_, '_>],
        resources_cell: &'resources RefCell<&'resources mut TreeResources<'m>>,
        first_error: &'resources Cell<Option<TreeError>>,
        lazy_capacity: Option<usize>,
    ) -> Result<Self, NativeGraphError> {
        lease
            .check_active()
            .map_err(|_| NativeGraphError::Invalid("native admitted base is cancelled"))?;
        let interpretation =
            GraphInterpretation::new(lease.bundle().lexical(), lease.bundle().document())?;
        let relationship_rules = {
            let mut resources = resources_cell
                .try_borrow_mut()
                .map_err(|_| NativeGraphError::Invalid("native base resources already borrowed"))?;
            NativePreparationCatalog::open(source, &mut resources)?.relationship_rules()
        };
        let needs_cascade_targets = !relationship_rules.is_empty()
            && requests.iter().any(|request| {
                matches!(
                    request.operation,
                    StructuredOperation::Delete(EntityId::Node(_), _)
                )
            });
        let lazy_capacity = if needs_cascade_targets {
            Some(MAX_LAZY_TARGETS)
        } else {
            lazy_capacity
        };
        let capacity = requests
            .len()
            .checked_mul(3)
            .ok_or(NativeGraphError::Invalid("native base cache capacity"))?;
        let mut cache =
            StorageBuffer::<CachedEntity<'source, 'resources, 'm>>::new(memory, capacity)?;

        if lease.bundle().base().generation.get() != 0 {
            let mut resources_ref = resources_cell
                .try_borrow_mut()
                .map_err(|_| NativeGraphError::Invalid("native base resources already borrowed"))?;
            let resources = &mut **resources_ref;
            let catalog = NativePreparationCatalog::open(source, resources)?;
            let roots = lease.bundle().roots();
            let sparse = SparseView::open(
                source,
                SparseRoots {
                    text: lease.bundle().text(),
                    vector: lease.bundle().vector(),
                },
                roots,
                lease.bundle().catalog(),
                &catalog,
                lease.bundle().document(),
                lease.bundle().lexical(),
                memory,
                resources,
            )?;

            for request in requests {
                let mut cached = false;
                for entry in cache.as_slice() {
                    // The structured-write preload only ever caches keyed
                    // entities (every `request` here names a key), so
                    // `entry.namespace`/`entry.key` are always `Some` in
                    // this loop; `transpose()?` still surfaces a real UTF-8
                    // decode failure instead of masking it as a non-match.
                    if entry.kind == request.key.kind()
                        && entry
                            .namespace
                            .as_ref()
                            .map(ChargedText::as_str)
                            .transpose()?
                            == Some(request.key.namespace().as_str())
                        && entry.key.as_ref().map(ChargedText::as_str).transpose()?
                            == Some(request.key.key().as_str())
                    {
                        cached = true;
                        break;
                    }
                }
                if cached {
                    continue;
                }
                let namespace = match catalog.lookup_symbol(
                    SymbolKind::Namespace,
                    request.key.namespace(),
                    resources,
                )? {
                    Some(Symbol::Namespace(namespace)) => namespace,
                    Some(_) => return Err(NativeGraphError::Invalid("namespace symbol domain")),
                    None => continue,
                };
                let root = roots.directory(TreeKind::KeyFences)?;
                let key = FenceKey::new(request.key.kind(), namespace, request.key.key().as_str())?;
                if let Some(entry) = lookup_fence_entry(source, root, key, resources)? {
                    let fence = verify_fence_entry(
                        source,
                        root,
                        entry,
                        &catalog,
                        lease.bundle().document(),
                        resources,
                    )?;
                    let membership = match fence.incarnation() {
                        EntityId::Node(node) => Membership {
                            text: sparse.lookup(Modality::Text, node, resources)?.is_some(),
                            vector: sparse.lookup(Modality::Vector, node, resources)?.is_some(),
                        },
                        EntityId::Relationship(_) => Membership::default(),
                    };
                    let cached = if fence.canonical().is_some() {
                        let record = load_record(
                            source,
                            roots,
                            fence.incarnation(),
                            &catalog,
                            lease.bundle().document(),
                            membership,
                            memory,
                            resources_cell,
                            first_error,
                            resources,
                        )?;
                        match record {
                            Some(record) => record,
                            None => {
                                fence.hidden_relationship(
                                    source,
                                    roots,
                                    &catalog,
                                    lease.bundle().document(),
                                    resources,
                                )?;
                                let mut cached = cached_from_parts(
                                    source,
                                    fence.provenance(),
                                    fence.canonical(),
                                    fence.canonical_bytes(),
                                    fence.required_payloads().1,
                                    membership,
                                    memory,
                                    resources_cell,
                                    first_error,
                                    resources,
                                    None,
                                )?;
                                cached.evidence_only = true;
                                cached
                            }
                        }
                    } else {
                        cached_from_parts(
                            source,
                            fence.provenance(),
                            None,
                            None,
                            None,
                            membership,
                            memory,
                            resources_cell,
                            first_error,
                            resources,
                            None,
                        )?
                    };
                    cache.push(cached)?;
                }
            }

            let mut wanted = StorageBuffer::new(memory, capacity)?;
            for request in requests {
                match request.operation {
                    StructuredOperation::Put(entity) | StructuredOperation::Delete(entity, _) => {
                        wanted.push(entity)?
                    }
                    StructuredOperation::Create | StructuredOperation::Recreate(_) => {}
                }
                if let Some(WriteImage::Relationship { source, target, .. }) = request.image {
                    for endpoint in [source, target] {
                        if let NodeRef::Existing(node) = endpoint {
                            wanted.push(EntityId::Node(node))?;
                        }
                    }
                }
            }
            for entity in wanted.as_slice().iter().copied() {
                if cache
                    .as_slice()
                    .iter()
                    .any(|entry| entry.incarnation == entity)
                {
                    continue;
                }
                if let Some(cached) = load_record(
                    source,
                    roots,
                    entity,
                    &catalog,
                    lease.bundle().document(),
                    match entity {
                        EntityId::Node(node) => Membership {
                            text: sparse.lookup(Modality::Text, node, resources)?.is_some(),
                            vector: sparse.lookup(Modality::Vector, node, resources)?.is_some(),
                        },
                        EntityId::Relationship(_) => Membership::default(),
                    },
                    memory,
                    resources_cell,
                    first_error,
                    resources,
                )? {
                    cache.push(cached)?;
                }
            }
        }

        Ok(Self {
            lease,
            interpretation,
            relationship_rules,
            source,
            memory,
            cache,
            lazy: lazy_capacity
                .map(|capacity| LazyTargets::new(memory, capacity))
                .transpose()?,
            resources: resources_cell,
            first_error,
        })
    }

    pub(super) const fn source(&self) -> &NativePreparationSource<'lease, 'm> {
        self.source
    }

    pub(super) fn resources(
        &self,
    ) -> Result<RefMut<'_, &'resources mut TreeResources<'m>>, NativeGraphError> {
        self.resources
            .try_borrow_mut()
            .map_err(|_| NativeGraphError::Invalid("native base resources already borrowed"))
    }

    pub(super) fn take_error(&self) -> Option<TreeError> {
        self.first_error.take()
    }

    /// Resolve one target: the structured-write preload first, then, when this
    /// base was admitted with a lazy arena, the admitted roots themselves.
    fn cached_entity<'a>(
        &'a self,
        id: EntityId,
    ) -> Result<Option<&'a CachedEntity<'source, 'resources, 'm>>, StageError> {
        if let Some(entry) = self
            .cache
            .as_slice()
            .iter()
            .find(|entry| entry.incarnation == id)
        {
            return Ok(Some(entry));
        }
        let Some(lazy) = self.lazy.as_ref() else {
            return Ok(None);
        };
        if let Some(entry) = lazy.find(id)? {
            return Ok(Some(entry));
        }
        // An unpublished graph has no roots to read, exactly as the preload
        // reads none. An absent target is not cached: only a resolved one
        // consumes arena capacity.
        if self.lease.bundle().base().generation.get() == 0 {
            return Ok(None);
        }
        let Some(loaded) = self.load_target(id)? else {
            return Ok(None);
        };
        lazy.insert(self.memory, loaded).map(Some)
    }

    /// Read one entity from the admitted roots, with the same catalog, sparse
    /// membership and record verification the structured preload applies.
    fn load_target(
        &self,
        id: EntityId,
    ) -> Result<Option<CachedEntity<'source, 'resources, 'm>>, StageError> {
        let mut resources_ref = self.resources.try_borrow_mut().map_err(|_| {
            StageError::NativeStorage(TreeError::Invalid("native base resources already borrowed"))
        })?;
        let resources = &mut **resources_ref;
        let catalog = NativePreparationCatalog::open(self.source, resources)?;
        let roots = self.lease.bundle().roots();
        let sparse = SparseView::open(
            self.source,
            SparseRoots {
                text: self.lease.bundle().text(),
                vector: self.lease.bundle().vector(),
            },
            roots,
            self.lease.bundle().catalog(),
            &catalog,
            self.lease.bundle().document(),
            self.lease.bundle().lexical(),
            self.memory,
            resources,
        )?;
        let membership = match id {
            EntityId::Node(node) => Membership {
                text: sparse.lookup(Modality::Text, node, resources)?.is_some(),
                vector: sparse.lookup(Modality::Vector, node, resources)?.is_some(),
            },
            EntityId::Relationship(_) => Membership::default(),
        };
        Ok(load_record(
            self.source,
            roots,
            id,
            &catalog,
            self.lease.bundle().document(),
            membership,
            self.memory,
            self.resources,
            self.first_error,
            resources,
        )?)
    }

    fn cached_by_key<'a>(
        &'a self,
        key: ApplicationKey<'_>,
    ) -> Result<Option<&'a CachedEntity<'source, 'resources, 'm>>, StageError> {
        for entry in self.cache.as_slice() {
            // Same reasoning as the preload loop in `build`: `self.cache`
            // holds only structured-write (keyed) entries, so `Some` is the
            // only outcome reached here in practice, but a genuine decode
            // failure still surfaces through `?` rather than being hidden.
            if entry.kind == key.kind()
                && entry
                    .namespace
                    .as_ref()
                    .map(ChargedText::as_str)
                    .transpose()?
                    == Some(key.namespace().as_str())
                && entry.key.as_ref().map(ChargedText::as_str).transpose()?
                    == Some(key.key().as_str())
            {
                return Ok(Some(entry));
            }
        }
        Ok(None)
    }
}

impl AdmittedBase for NativeAdmittedBase<'_, '_, '_, '_> {
    fn has_relationship_rules(&self) -> bool {
        !self.relationship_rules.is_empty()
    }

    fn visit_incoming_rules(
        &self,
        node: NodeId,
        visit: &mut dyn FnMut(
            RelId,
            NodeId,
            crate::property_graph::catalog::OnDelete,
        ) -> Result<(), StageError>,
        control: &mut WriteControl<'_>,
    ) -> Result<(), StageError> {
        use crate::property_graph::storage::adjacency::{
            AdjacencyQuery, Direction, RelationshipRange, UpperBound,
        };
        control(crate::property_graph::staging::WritePhase::Incident)?;
        let mut resources = self
            .resources
            .try_borrow_mut()
            .map_err(|_| StageError::InvalidInput)?;
        let catalog = NativePreparationCatalog::open(self.source, &mut resources)?;
        let reader = NativeGraphReader::new(
            self.source,
            self.lease.bundle().roots(),
            self.lease.bundle().sequence(),
            &catalog,
            self.lease.bundle().document(),
        );
        let mut scratch = RangeScratch::for_prepare(self.memory, &mut resources)?;
        let mut failure = None;
        let result = reader.visit_adjacency(
            AdjacencyQuery {
                node,
                direction: Direction::In,
                relationship_type: None,
                relationships: RelationshipRange {
                    lower: RelId::new(1).map_err(|_| StageError::InvalidInput)?,
                    upper: UpperBound::Infinity,
                },
            },
            &mut scratch,
            &mut resources,
            &mut |row, resources| {
                if let Some(policy) = catalog.relationship_rule(row.relationship_type, resources)? {
                    let result = control(crate::property_graph::staging::WritePhase::Incident)
                        .and_then(|()| visit(row.edge.rel, row.edge.neighbor, policy));
                    if let Err(error) = result {
                        failure = Some(error);
                        return Ok(false);
                    }
                }
                Ok(true)
            },
        );
        if let Some(error) = failure {
            return Err(error);
        }
        result.map_err(StageError::from)
    }

    fn identity(&self) -> BaseIdentity {
        self.lease.bundle().base()
    }

    fn high_waters(&self) -> HighWaters {
        let high = self.lease.bundle().high_waters();
        let [label, relationship_type, property, namespace] = high.symbols;
        HighWaters {
            node: high.node,
            relationship: high.relationship,
            symbols: crate::property_graph::catalog::SymbolHighWaters {
                label,
                relationship_type,
                property,
                namespace,
            },
        }
    }

    fn interpretation(&self) -> GraphInterpretation<'_> {
        self.interpretation
    }

    fn key(
        &self,
        key: ApplicationKey<'_>,
        control: &mut WriteControl<'_>,
    ) -> Result<BaseKeyState<'_>, StageError> {
        control(crate::property_graph::staging::WritePhase::Validate)?;
        let Some(entry) = self.cached_by_key(key)? else {
            return Ok(BaseKeyState::NeverUsed);
        };
        match entry.live(self.identity())? {
            Some(entity) => Ok(BaseKeyState::Live(entity)),
            None => Ok(BaseKeyState::Deleted(self.identity(), entry.provenance()?)),
        }
    }

    fn entity(
        &self,
        id: EntityId,
        control: &mut WriteControl<'_>,
    ) -> Result<Option<BaseEntity<'_>>, StageError> {
        control(crate::property_graph::staging::WritePhase::Validate)?;
        self.cached_entity(id)?
            .filter(|entry| !entry.evidence_only)
            .map(|entry| entry.live(self.identity()))
            .transpose()
            .map(Option::flatten)
    }

    fn has_live_incident(
        &self,
        node: NodeId,
        removed: &[RelId],
        control: &mut WriteControl<'_>,
    ) -> Result<bool, StageError> {
        control(crate::property_graph::staging::WritePhase::Incident)?;
        let mut resources = self.resources.try_borrow_mut().map_err(|_| {
            StageError::NativeStorage(TreeError::Invalid("native base resources already borrowed"))
        })?;
        let catalog = NativePreparationCatalog::open(self.source, &mut resources)?;
        let reader = NativeGraphReader::new(
            self.source,
            self.lease.bundle().roots(),
            self.lease.bundle().sequence(),
            &catalog,
            self.lease.bundle().document(),
        );
        let mut scratch = RangeScratch::for_prepare(self.memory, &mut resources)?;
        if self.relationship_rules.is_empty() {
            return reader
                .has_live_incident(node, removed, &mut scratch, &mut resources)
                .map_err(StageError::from);
        }
        use crate::property_graph::storage::adjacency::{
            AdjacencyQuery, Direction, RelationshipRange, UpperBound,
        };
        let mut found = false;
        for direction in [Direction::Out, Direction::In] {
            reader.visit_adjacency(
                AdjacencyQuery {
                    node,
                    direction,
                    relationship_type: None,
                    relationships: RelationshipRange {
                        lower: RelId::new(1).map_err(|_| StageError::InvalidInput)?,
                        upper: UpperBound::Infinity,
                    },
                },
                &mut scratch,
                &mut resources,
                &mut |row, resources| {
                    if !removed.contains(&row.edge.rel)
                        && catalog
                            .relationship_rule(row.relationship_type, resources)?
                            .is_none()
                    {
                        found = true;
                    }
                    Ok(!found)
                },
            )?;
            if found {
                break;
            }
        }
        Ok(found)
    }

    fn property(
        &self,
        entity: EntityId,
        name: GraphName<'_>,
        control: &mut WriteControl<'_>,
    ) -> Result<Option<PropertyValue<'_>>, StageError> {
        control(crate::property_graph::staging::WritePhase::Overlay)?;
        let Some(entry) = self.cached_entity(entity)? else {
            return Ok(None);
        };
        for property in entry.properties.as_slice() {
            if property.name.as_str()? == name.as_str() {
                return property.value.value().map(Some);
            }
        }
        Ok(None)
    }

    fn stored_text(
        &self,
        node: NodeId,
        control: &mut WriteControl<'_>,
    ) -> Result<Option<&str>, StageError> {
        control(crate::property_graph::staging::WritePhase::Overlay)?;
        Ok(self
            .cached_entity(EntityId::Node(node))?
            .and_then(|entry| entry.text.as_ref())
            .map(ChargedText::as_str)
            .transpose()?)
    }

    fn symbol(
        &self,
        kind: SymbolKind,
        name: GraphName<'_>,
        control: &mut WriteControl<'_>,
    ) -> Result<Option<Symbol>, StageError> {
        control(crate::property_graph::staging::WritePhase::Validate)?;
        let mut resources = self.resources.try_borrow_mut().map_err(|_| {
            StageError::NativeStorage(TreeError::Invalid("native base resources already borrowed"))
        })?;
        let catalog = NativePreparationCatalog::open(self.source, &mut resources)?;
        catalog
            .lookup_symbol(kind, name, &mut resources)
            .map_err(StageError::from)
    }
}
