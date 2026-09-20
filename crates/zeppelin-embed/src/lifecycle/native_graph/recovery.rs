use super::persistence::{
    NativeStoreClassification, NativeWal, ROOT_SELECTOR, ROOT_SELECTOR_BYTES, artifact_descriptor,
    decode_root_selector,
};
use super::{NativeGraphBundleInput, NativeGraphError};
use crate::epoch::EmbeddingTower;
use crate::lifecycle::{AccessMode, CancelToken, MonotonicClock, OpenOptions, QueryControl, Store};
use crate::property_graph::catalog::{
    CatalogError, CatalogImage, GraphInterpretation, Symbol, SymbolEntry, SymbolHighWaters,
    SymbolKind,
};
use crate::property_graph::resources::{GraphReservation, GraphResources};
use crate::property_graph::staging::{BaseIdentity, WriteLimits, WriteMemory};
use crate::property_graph::storage::NativeReadonlyMapping;
use crate::property_graph::storage::adjacency::{
    Direction, RangeScratch, RelationshipRow, validate_range,
};
use crate::property_graph::storage::artifact::{
    self, ArtifactControlError, ArtifactId, ArtifactIdentity, BlockKind, ContainerKind,
    FramedBlock, MAX_ARTIFACT_BYTES, PhysicalRef, ValidatedArtifact,
};
use crate::property_graph::storage::memory::{StorageBuffer, StorageMemory, StorageReservation};
use crate::property_graph::storage::payload::PayloadRef;
use crate::property_graph::storage::records::{
    FenceView, NativeDirectoryValues, NodeRecordState, RecordCatalog, RecordShape, RecordView,
    StoredKey, StoredProvenance, verify_fence_entry, verify_node_state, verify_record,
};
use crate::property_graph::storage::stream::PayloadSlice;
use crate::property_graph::storage::tree::{
    TreeKind,
    directory::{
        BlockSource, DirectoryCursor, FenceKey, GraphRoots, LeafValidator, TreeError,
        TreeResources, lookup_entry, lookup_fence_entry, verify_directory,
    },
};
use crate::property_graph::wal::{
    ArtifactDescriptor, Change, ChangeReader, CommitState, InventoryChange, InventoryState,
    MAX_ENVELOPE_BYTES, Mutation, ParticipantRole, ReclaimComplete, ReclaimIntent, Replay,
    ReplayStep, ReplayValidator, RequiredRef, RequiredRole, STACK_RESERVATION_BYTES, WalError,
    WalResources, decode_checkpoint, validate_required_block,
};
use crate::property_graph::{
    ApplicationKey, CanonicalError, CanonicalFingerprint, CanonicalRecord, CurrentEntity,
    CypherEdit, EntityId, EntityShape, ExpectedGraphState, GraphOperation, KeyDecision, KeyRequest,
    KeyState, MAX_GRAPH_CHANGES, OperationProvenance, classify_cypher, classify_key,
};
use crate::vfs::Vfs;
use std::cell::{Cell, OnceCell, RefCell};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3;

const MAX_RECOVERED_DESCRIPTORS: usize = 8_192;
const RECOVERY_TREE_WORK: u64 = 512 * 1024 * 1024;

fn canonical_payload<S: BlockSource>(
    source: &S,
    required: RequiredRef,
    resources: &mut TreeResources<'_>,
) -> Result<PayloadRef, TreeError> {
    let block = source.resolve(required.block, resources)?;
    let length = match required.block.kind {
        BlockKind::CanonicalImage => block.payload().len() as u64,
        BlockKind::ExtentList => {
            let payload = block.payload();
            if payload.get(..4) != Some(b"ZGEX".as_slice())
                || payload.get(4..6) != Some(&1_u16.to_le_bytes())
                || payload.get(6..8) != Some(&(BlockKind::CanonicalImage as u16).to_le_bytes())
            {
                return Err(TreeError::Invalid("recovery canonical extent header"));
            }
            u64::from_le_bytes(
                *payload
                    .get(8..16)
                    .and_then(|bytes| bytes.first_chunk::<8>())
                    .ok_or(TreeError::Invalid("recovery canonical extent length"))?,
            )
        }
        _ => return Err(TreeError::Invalid("recovery canonical role")),
    };
    PayloadRef::new(BlockKind::CanonicalImage, length, required.block)
}

struct PreparedInventory<'a> {
    descriptors: &'a [u8],
    count: usize,
}

impl PreparedInventory<'_> {
    fn descriptor(&self, index: usize) -> Result<ArtifactDescriptor, TreeError> {
        let start = index.checked_mul(64).ok_or(TreeError::Memory)?;
        let row = self
            .descriptors
            .get(start..start + 64)
            .ok_or(TreeError::Invalid("prepared inventory descriptor extent"))?;
        let read_u16 = |offset| {
            row.get(offset..offset + 2)
                .and_then(|bytes| bytes.first_chunk::<2>())
                .copied()
                .map(u16::from_le_bytes)
                .ok_or(TreeError::Invalid("prepared inventory descriptor field"))
        };
        let read_u32 = |offset| {
            row.get(offset..offset + 4)
                .and_then(|bytes| bytes.first_chunk::<4>())
                .copied()
                .map(u32::from_le_bytes)
                .ok_or(TreeError::Invalid("prepared inventory descriptor field"))
        };
        let read_u64 = |offset| {
            row.get(offset..offset + 8)
                .and_then(|bytes| bytes.first_chunk::<8>())
                .copied()
                .map(u64::from_le_bytes)
                .ok_or(TreeError::Invalid("prepared inventory descriptor field"))
        };
        let read_u128 = |offset| {
            row.get(offset..offset + 16)
                .and_then(|bytes| bytes.first_chunk::<16>())
                .copied()
                .map(u128::from_le_bytes)
                .ok_or(TreeError::Invalid("prepared inventory descriptor field"))
        };
        Ok(ArtifactDescriptor {
            store: crate::property_graph::StoreInstanceId::new(read_u128(0)?)
                .map_err(|_| TreeError::Invalid("prepared inventory store"))?,
            artifact: ArtifactId::new(read_u128(16)?)
                .map_err(|_| TreeError::Invalid("prepared inventory artifact"))?,
            generation: crate::property_graph::GraphGeneration::new(read_u64(32)?),
            serial: read_u64(40)?,
            bytes: read_u32(48)?,
            family: read_u16(52)?,
            version: read_u16(54)?,
            checksum: read_u64(56)?,
        })
    }
}

fn prepared_inventory<'a>(
    source: &'a impl BlockSource,
    required: RequiredRef,
    state: CommitState<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<PreparedInventory<'a>, TreeError> {
    let block = source.resolve(required.block, resources)?;
    let identity = block.identity();
    if identity.store != required.object.store
        || identity.artifact != required.object.artifact
        || identity.generation != required.object.generation
        || identity.creation_serial != required.object.serial
        || block.reference() != required.block
        || block.file_length() != required.object.bytes as usize
        || block.file_checksum() != required.object.checksum
    {
        return Err(TreeError::Invalid("prepared inventory required descriptor"));
    }
    let payload = block.payload();
    if payload.get(..4) != Some(b"ZGCP".as_slice())
        || payload.get(4..6) != Some(&2_u16.to_le_bytes())
        || payload.get(6..8) != Some(&1_u16.to_le_bytes())
        || payload.get(12..16) != Some([0_u8; 4].as_slice())
    {
        return Err(TreeError::Invalid("prepared inventory role or reserved"));
    }
    let count = usize::try_from(u32::from_le_bytes(
        *payload
            .get(8..12)
            .and_then(|bytes| bytes.first_chunk::<4>())
            .ok_or(TreeError::Invalid("prepared inventory count"))?,
    ))
    .map_err(|_| TreeError::Memory)?;
    if count > MAX_RECOVERED_DESCRIPTORS
        || payload.len()
            != count
                .checked_mul(64)
                .and_then(|bytes| bytes.checked_add(16))
                .ok_or(TreeError::Memory)?
    {
        return Err(TreeError::Invalid("prepared inventory length"));
    }
    let inventory = PreparedInventory {
        descriptors: payload
            .get(16..)
            .ok_or(TreeError::Invalid("prepared inventory descriptors"))?,
        count,
    };
    for index in 0..count {
        resources.step(1)?;
        let descriptor = inventory.descriptor(index)?;
        if descriptor.store != state.store
            || descriptor.generation > state.generation
            || descriptor.serial == 0
            || descriptor.serial > state.high_waters.creation_serial
            || descriptor.bytes as usize > MAX_ARTIFACT_BYTES
            || descriptor.family != crate::format::FormatFamily::NativeGraphObject.id()
            || descriptor.version != 1
        {
            return Err(TreeError::Invalid("prepared inventory descriptor domain"));
        }
        for prior in 0..index {
            resources.step(1)?;
            let previous = inventory.descriptor(prior)?;
            if previous.artifact == descriptor.artifact || previous.serial == descriptor.serial {
                return Err(TreeError::Invalid(
                    "duplicate prepared inventory descriptor",
                ));
            }
        }
    }
    Ok(inventory)
}

fn map_file(
    store: &Store,
    path: &Path,
    maximum: usize,
) -> Result<NativeReadonlyMapping, NativeGraphError> {
    let file = store
        .vfs
        .open_for_map(path)
        .map_err(|source| NativeGraphError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    NativeReadonlyMapping::open_recovery(file, path, &store.native_graph, maximum)
}

struct RecoveryMappedArtifact {
    artifact: ArtifactId,
    mapping: NativeReadonlyMapping,
    validation: ValidatedArtifact,
}

struct RecoverySource<'a, 'm> {
    store: &'a Store,
    directory: &'a Path,
    expected_store: crate::property_graph::StoreInstanceId,
    generation: crate::property_graph::GraphGeneration,
    creation_serial: u64,
    memory: &'m StorageMemory<'m>,
    slots: StorageBuffer<'m, OnceCell<RecoveryMappedArtifact>>,
    source_error: RefCell<Option<NativeGraphError>>,
}

impl<'a, 'm> RecoverySource<'a, 'm> {
    fn new(
        store: &'a Store,
        directory: &'a Path,
        state: CommitState<'_>,
        memory: &'m StorageMemory<'m>,
    ) -> Result<Self, TreeError> {
        let mut slots = StorageBuffer::new(memory, MAX_RECOVERED_DESCRIPTORS)?;
        for _ in 0..MAX_RECOVERED_DESCRIPTORS {
            slots.push(OnceCell::new())?;
        }
        Ok(Self {
            store,
            directory,
            expected_store: state.store,
            generation: state.generation,
            creation_serial: state.high_waters.creation_serial,
            memory,
            slots,
            source_error: RefCell::new(None),
        })
    }

    fn resources(&self) -> Result<TreeResources<'m>, TreeError> {
        TreeResources::for_prepare(self.memory, RECOVERY_TREE_WORK)
    }

    fn charged_path(
        &self,
        artifact: ArtifactId,
    ) -> Result<(PathBuf, StorageReservation<'m>), TreeError> {
        const NAME_BYTES: usize = b"graph-00000000000000000000000000000000.zgraph".len();
        let path_upper = self
            .directory
            .as_os_str()
            .len()
            .checked_add(1 + NAME_BYTES)
            .ok_or(TreeError::Memory)?;
        let mut charge = self.memory.reserve(
            path_upper
                .checked_add(NAME_BYTES)
                .ok_or(TreeError::Memory)?,
        )?;
        let mut name = String::new();
        name.try_reserve_exact(NAME_BYTES)
            .map_err(|_| TreeError::Memory)?;
        write!(&mut name, "graph-{:032x}.zgraph", artifact.get()).map_err(|_| TreeError::Memory)?;
        let mut raw = OsString::with_capacity(path_upper);
        raw.push(self.directory.as_os_str());
        let mut path = PathBuf::from(raw);
        path.push(&name);
        let raw = path.into_os_string();
        let path_capacity = raw.capacity();
        charge.resize(
            name.capacity()
                .checked_add(path_capacity)
                .ok_or(TreeError::Memory)?,
        )?;
        drop(name);
        charge.resize(path_capacity)?;
        Ok((PathBuf::from(raw), charge))
    }

    fn latch_source(&self, error: NativeGraphError) -> TreeError {
        let mut slot = self.source_error.borrow_mut();
        if slot.is_none() {
            *slot = Some(error);
        }
        TreeError::Invalid("native recovery artifact source")
    }

    fn take_source_error(&self) -> Option<NativeGraphError> {
        self.source_error.borrow_mut().take()
    }

    fn decode<'s>(
        &'s self,
        mapped: &'s RecoveryMappedArtifact,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'s>, TreeError> {
        resources.require_preparation(self.memory)?;
        resources.step(1)?;
        mapped
            .validation
            .framed_block(mapped.mapping.as_bytes(), reference)
            .map_err(TreeError::Format)
    }

    fn validate_descriptor(
        &self,
        descriptor: ArtifactDescriptor,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        if descriptor.store != self.expected_store
            || descriptor.generation > self.generation
            || descriptor.serial == 0
            || descriptor.serial > self.creation_serial
            || descriptor.bytes as usize > MAX_ARTIFACT_BYTES
            || descriptor.family != crate::format::FormatFamily::NativeGraphObject.id()
            || descriptor.version != 1
        {
            return Err(TreeError::Invalid("recovery artifact descriptor domain"));
        }
        let (path, path_charge) = self.charged_path(descriptor.artifact)?;
        let mapping = map_file(self.store, &path, MAX_ARTIFACT_BYTES)
            .map_err(|error| self.latch_source(error))?;
        drop(path);
        drop(path_charge);
        let frame = artifact::decode_with_control(
            ContainerKind::Object,
            Some((descriptor.store, descriptor.artifact)),
            mapping.as_bytes(),
            &mut |bytes| resources.step(bytes as u64),
        )
        .map_err(|error| match error {
            ArtifactControlError::Format(error) => TreeError::Format(error),
            ArtifactControlError::Control(error) => error,
        })?;
        let identity = frame.identity();
        let checksum = u64::from_le_bytes(
            *mapping
                .as_bytes()
                .last_chunk::<8>()
                .ok_or(TreeError::Invalid("recovery artifact checksum trailer"))?,
        );
        if identity.generation != descriptor.generation
            || identity.creation_serial != descriptor.serial
            || mapping.as_bytes().len() != descriptor.bytes as usize
            || checksum != descriptor.checksum
        {
            return Err(TreeError::Invalid("recovery artifact descriptor mismatch"));
        }
        Ok(())
    }
}

impl BlockSource for RecoverySource<'_, '_> {
    fn resolve<'a>(
        &'a self,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'a>, TreeError> {
        resources.require_preparation(self.memory)?;
        resources.step(0)?;
        for slot in self.slots.as_slice() {
            if let Some(mapped) = slot.get()
                && mapped.artifact == reference.artifact
            {
                return self.decode(mapped, reference, resources);
            }
        }
        let slot = self
            .slots
            .as_slice()
            .iter()
            .find(|slot| slot.get().is_none())
            .ok_or(TreeError::Memory)?;
        let (path, path_charge) = self.charged_path(reference.artifact)?;
        let mapping = map_file(self.store, &path, MAX_ARTIFACT_BYTES)
            .map_err(|error| self.latch_source(error))?;
        drop(path);
        drop(path_charge);
        let frame = artifact::decode_with_control(
            ContainerKind::Object,
            Some((self.expected_store, reference.artifact)),
            mapping.as_bytes(),
            &mut |bytes| resources.step(bytes as u64),
        )
        .map_err(|error| match error {
            ArtifactControlError::Format(error) => TreeError::Format(error),
            ArtifactControlError::Control(error) => error,
        })?;
        let identity = frame.identity();
        if identity.generation > self.generation || identity.creation_serial > self.creation_serial
        {
            return Err(TreeError::Invalid(
                "native recovery artifact is newer than committed cutoff",
            ));
        }
        frame.framed_block(reference).map_err(TreeError::Format)?;
        let mapped = RecoveryMappedArtifact {
            artifact: reference.artifact,
            validation: frame.validation(),
            mapping,
        };
        slot.set(mapped)
            .map_err(|_| TreeError::Invalid("native recovery source slot initialized twice"))?;
        self.decode(
            slot.get().ok_or(TreeError::Invalid(
                "native recovery source slot remained empty",
            ))?,
            reference,
            resources,
        )
    }
}

struct RecoveryCatalog<'source, 'm> {
    image: CatalogImage<'source>,
    _descriptors: StorageReservation<'m>,
}

impl<'source, 'm> RecoveryCatalog<'source, 'm> {
    fn open(
        source: &'source RecoverySource<'_, 'm>,
        required: RequiredRef,
        expected: GraphInterpretation<'_>,
        high: crate::property_graph::wal::HighWaters,
        resources: &mut TreeResources<'_>,
    ) -> Result<Self, TreeError> {
        let block = source.resolve(required.block, resources)?;
        let identity = block.identity();
        if identity.store != required.object.store
            || identity.artifact != required.object.artifact
            || identity.generation != required.object.generation
            || identity.creation_serial != required.object.serial
            || block.reference() != required.block
            || block.file_length() != required.object.bytes as usize
            || block.file_checksum() != required.object.checksum
        {
            return Err(TreeError::Invalid("recovery catalog descriptor mismatch"));
        }
        let payload = block.payload();
        if payload.get(..4) != Some(b"ZGCP".as_slice())
            || payload.get(4..6) != Some(&1_u16.to_le_bytes())
            || payload.get(6..8) != Some(&1_u16.to_le_bytes())
        {
            return Err(TreeError::Invalid("recovery catalog role or version"));
        }
        let encoded = payload
            .get(8..)
            .ok_or(TreeError::Invalid("recovery catalog payload"))?;
        let count = usize::try_from(u64::from_le_bytes(
            *encoded
                .get(104..112)
                .and_then(|bytes| bytes.first_chunk::<8>())
                .ok_or(TreeError::Invalid("recovery catalog symbol count"))?,
        ))
        .map_err(|_| TreeError::Memory)?;
        let allowance = count
            .checked_mul(std::mem::size_of::<SymbolEntry<'_>>())
            .ok_or(TreeError::Memory)?;
        let mut descriptors = source.memory.reserve(allowance)?;
        let mut callback_error = None;
        let image = CatalogImage::decode(encoded, allowance, &mut || {
            resources.step(1).map_err(|error| {
                if callback_error.is_none() {
                    callback_error = Some(error);
                }
                CatalogError::Cancelled
            })
        })
        .map_err(|error| match error {
            CatalogError::Cancelled => callback_error
                .take()
                .unwrap_or(TreeError::Invalid("recovery catalog cancelled")),
            CatalogError::Capacity | CatalogError::Allocation => TreeError::Memory,
            _ => TreeError::Invalid("invalid recovery catalog"),
        })?;
        descriptors.resize(image.symbols.allocated_bytes())?;
        let mut validation_error = None;
        image
            .declaration
            .validate_for(required.object.store, expected, &mut || {
                resources.step(1).map_err(|error| {
                    if validation_error.is_none() {
                        validation_error = Some(error);
                    }
                    CatalogError::Cancelled
                })
            })
            .map_err(|error| match error {
                CatalogError::Cancelled => validation_error
                    .take()
                    .unwrap_or(TreeError::Invalid("recovery catalog cancelled")),
                CatalogError::Capacity | CatalogError::Allocation => TreeError::Memory,
                _ => TreeError::Invalid("recovery catalog interpretation"),
            })?;
        let [label, relationship_type, property, namespace] = high.symbols;
        if image.declaration.node_high_water != high.node
            || image.declaration.relationship_high_water != high.relationship
            || image.symbols.high_waters()
                != (SymbolHighWaters {
                    label,
                    relationship_type,
                    property,
                    namespace,
                })
        {
            return Err(TreeError::Invalid("recovery catalog high-water mismatch"));
        }
        Ok(Self {
            image,
            _descriptors: descriptors,
        })
    }

    fn lookup_symbol(
        &self,
        kind: SymbolKind,
        name: crate::property_graph::GraphName<'_>,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<Symbol>, TreeError> {
        let mut callback_error = None;
        self.image
            .symbols
            .lookup(kind, name, &mut || {
                resources.step(1).map_err(|error| {
                    if callback_error.is_none() {
                        callback_error = Some(error);
                    }
                    CatalogError::Cancelled
                })
            })
            .map_err(|error| match error {
                CatalogError::Cancelled => callback_error
                    .take()
                    .unwrap_or(TreeError::Invalid("recovery catalog cancelled")),
                CatalogError::Capacity | CatalogError::Allocation => TreeError::Memory,
                _ => TreeError::Invalid("recovery catalog symbol lookup"),
            })
    }

    fn symbol_name(
        &self,
        symbol: Symbol,
        resources: &mut TreeResources<'_>,
    ) -> Result<crate::property_graph::GraphName<'source>, TreeError> {
        for entry in self.image.symbols.entries() {
            resources.step(1)?;
            if entry.symbol == symbol {
                return Ok(entry.name);
            }
        }
        Err(TreeError::Invalid(
            "recovery catalog symbol identity is absent",
        ))
    }

    fn validate_retains(
        &self,
        base: &RecoveryCatalog<'_, '_>,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        for entry in base.image.symbols.entries() {
            resources.step(1)?;
            if self.lookup_symbol(entry.symbol.kind(), entry.name, resources)? != Some(entry.symbol)
            {
                return Err(TreeError::Invalid(
                    "recovery catalog changed an existing symbol mapping",
                ));
            }
        }
        Ok(())
    }
}

impl<S: BlockSource> RecordCatalog<S> for RecoveryCatalog<'_, '_> {
    fn resolve(
        &self,
        kind: SymbolKind,
        name: PayloadSlice<'_, S>,
        resources: &mut TreeResources<'_>,
    ) -> Result<Symbol, TreeError> {
        name.validate_utf8(resources)?;
        for entry in self.image.symbols.entries() {
            resources.step(1)?;
            if entry.symbol.kind() == kind
                && name
                    .compare_bytes(entry.name.as_str().as_bytes(), resources)?
                    .is_eq()
            {
                return Ok(entry.symbol);
            }
        }
        Err(TreeError::Invalid(
            "record name is absent from recovery catalog",
        ))
    }
}

fn exact_relationship_membership(
    source: &RecoverySource<'_, '_>,
    roots: GraphRoots,
    sequence: u64,
    row: crate::property_graph::storage::adjacency::RelationshipRow,
    present: bool,
    memory: &StorageMemory<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    for (kind, node, direction, neighbor) in [
        (TreeKind::OutRanges, row.source, Direction::Out, row.target),
        (TreeKind::InRanges, row.target, Direction::In, row.source),
    ] {
        let root = roots.directory(kind)?;
        let mut scratch = RangeScratch::for_prepare(memory, resources)?;
        let mut count = 0_u64;
        verify_directory(source, root, resources, &mut |entry, resources| {
            let range = validate_range(source, root, entry, sequence, &mut scratch, resources)?;
            let key = range.descriptor().key();
            for edge in range.edges() {
                resources.step(1)?;
                if edge.rel == row.rel {
                    if key.node != node
                        || key.direction != direction
                        || key.rel_type != row.relationship_type
                        || edge.neighbor != neighbor
                    {
                        return Err(TreeError::Invalid(
                            "recovery relationship adjacency topology mismatch",
                        ));
                    }
                    count = count.checked_add(1).ok_or(TreeError::Work)?;
                }
            }
            Ok(())
        })?;
        if (present && count != 1) || (!present && count != 0) {
            return Err(TreeError::Invalid(
                "recovery relationship adjacency mismatch",
            ));
        }
    }
    Ok(())
}

fn authoritative_relationship(
    source: &RecoverySource<'_, '_>,
    catalog: &RecoveryCatalog<'_, '_>,
    document: Option<&EmbeddingTower>,
    roots: GraphRoots,
    relationship: crate::property_graph::RelId,
    resources: &mut TreeResources<'_>,
) -> Result<Option<RelationshipRow>, TreeError> {
    let root = roots.directory(TreeKind::Relationships)?;
    let Some(entry) = lookup_entry(source, root, &relationship.get().to_le_bytes(), resources)?
    else {
        return Ok(None);
    };
    let payload = PayloadRef::decode(entry.value())?;
    let record = verify_record(
        PayloadSlice::new(source, roots.store(), entry.creation_generation(), payload),
        crate::property_graph::EntityId::Relationship(relationship),
        catalog,
        document,
        resources,
    )?;
    let RecordShape::Relationship {
        id,
        source: relationship_source,
        target: relationship_target,
        relationship_type,
    } = record.shape()
    else {
        return Err(TreeError::Invalid("recovery relationship directory role"));
    };
    Ok(Some(RelationshipRow {
        rel: id,
        source: relationship_source,
        target: relationship_target,
        relationship_type,
    }))
}

fn fence_matches_record(
    fence: &FenceView<'_, RecoverySource<'_, '_>>,
    record: &RecordView<'_, RecoverySource<'_, '_>>,
    resources: &mut TreeResources<'_>,
) -> Result<bool, TreeError> {
    Ok(fence.incarnation() == record.incarnation()
        && fence.revision() == record.revision()
        && stored_provenance_equal(fence.provenance(), record.provenance(), resources)?
        && fence
            .canonical_bytes()
            .ok_or(TreeError::Invalid(
                "live recovery fence lacks canonical bytes",
            ))?
            .compare(record.canonical_bytes(), resources)?
            .is_eq())
}

fn validate_fence_record_agreement(
    source: &RecoverySource<'_, '_>,
    catalog: &RecoveryCatalog<'_, '_>,
    document: Option<&EmbeddingTower>,
    roots: GraphRoots,
    fence: &FenceView<'_, RecoverySource<'_, '_>>,
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    match fence.incarnation() {
        EntityId::Node(node) => {
            let root = roots.directory(TreeKind::Nodes)?;
            let entry = lookup_entry(source, root, &node.get().to_le_bytes(), resources)?
                .ok_or(TreeError::Invalid("recovery fence node is absent"))?;
            let payload = PayloadRef::decode(entry.value())?;
            match verify_node_state(
                PayloadSlice::new(source, roots.store(), entry.creation_generation(), payload),
                node,
                catalog,
                document,
                resources,
            )? {
                NodeRecordState::Live(record) => {
                    if fence.is_deleted() || !fence_matches_record(fence, &record, resources)? {
                        return Err(TreeError::Invalid("recovery live node fence mismatch"));
                    }
                }
                NodeRecordState::Tombstone(tombstone) => {
                    if !fence.is_deleted()
                        || fence.revision() != tombstone.revision()
                        || !stored_provenance_equal(
                            fence.provenance(),
                            tombstone.provenance(),
                            resources,
                        )?
                    {
                        return Err(TreeError::Invalid("recovery deleted node fence mismatch"));
                    }
                }
            }
        }
        EntityId::Relationship(relationship) => {
            let root = roots.directory(TreeKind::Relationships)?;
            let entry = lookup_entry(source, root, &relationship.get().to_le_bytes(), resources)?;
            match (fence.is_deleted(), entry) {
                (true, None) => {}
                (true, Some(_)) => {
                    return Err(TreeError::Invalid(
                        "recovery deleted relationship fence has a record",
                    ));
                }
                (false, None) => {
                    return Err(TreeError::Invalid(
                        "recovery live relationship fence lacks a record",
                    ));
                }
                (false, Some(entry)) => {
                    let payload = PayloadRef::decode(entry.value())?;
                    let record = verify_record(
                        PayloadSlice::new(
                            source,
                            roots.store(),
                            entry.creation_generation(),
                            payload,
                        ),
                        EntityId::Relationship(relationship),
                        catalog,
                        document,
                        resources,
                    )?;
                    if !fence_matches_record(fence, &record, resources)? {
                        return Err(TreeError::Invalid(
                            "recovery live relationship fence mismatch",
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn require_live_record_fence(
    source: &RecoverySource<'_, '_>,
    catalog: &RecoveryCatalog<'_, '_>,
    document: Option<&EmbeddingTower>,
    roots: GraphRoots,
    record: &RecordView<'_, RecoverySource<'_, '_>>,
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    let Some(key) = record.provenance().key() else {
        return Ok(());
    };
    let fence = find_fence_by_key(source, catalog, document, roots, key, resources)?.ok_or(
        TreeError::Invalid("recovery keyed live record lacks a fence"),
    )?;
    if !fence_matches_record(&fence, record, resources)? {
        return Err(TreeError::Invalid(
            "recovery keyed live record differs from its fence",
        ));
    }
    Ok(())
}

fn validate_native_checkpoint(
    source: &RecoverySource<'_, '_>,
    catalog: &RecoveryCatalog<'_, '_>,
    document: Option<&EmbeddingTower>,
    memory: &StorageMemory<'_>,
    roots: GraphRoots,
    sequence: u64,
    high_waters: crate::property_graph::wal::HighWaters,
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    for kind in [
        TreeKind::Nodes,
        TreeKind::Labels,
        TreeKind::RelationshipTypes,
    ] {
        let root = roots.directory(kind)?;
        let mut values = NativeDirectoryValues::new(catalog, document);
        verify_directory(source, root, resources, &mut |entry, resources| {
            if kind == TreeKind::Nodes {
                let crate::property_graph::storage::tree::Key::Inline(key) = entry.key() else {
                    return Err(TreeError::Invalid("recovery node key role"));
                };
                let id = u128::from_le_bytes(
                    key.try_into()
                        .map_err(|_| TreeError::Invalid("recovery node key width"))?,
                );
                if id == 0 || id > high_waters.node {
                    return Err(TreeError::Invalid("recovery node high-water"));
                }
            }
            values.verify(source, root, entry, resources)
        })?;
    }

    let fence_root = roots.directory(TreeKind::KeyFences)?;
    verify_directory(source, fence_root, resources, &mut |entry, resources| {
        let fence = verify_fence_entry(source, fence_root, entry, catalog, document, resources)?;
        validate_fence_record_agreement(source, catalog, document, roots, &fence, resources)
    })?;

    let node_root = roots.directory(TreeKind::Nodes)?;
    let label_root = roots.directory(TreeKind::Labels)?;
    verify_directory(source, node_root, resources, &mut |entry, resources| {
        let crate::property_graph::storage::tree::Key::Inline(key) = entry.key() else {
            return Err(TreeError::Invalid("recovery node key role"));
        };
        let node = crate::property_graph::NodeId::new(u128::from_le_bytes(
            key.try_into()
                .map_err(|_| TreeError::Invalid("recovery node key width"))?,
        ))
        .map_err(|_| TreeError::Invalid("recovery node identity"))?;
        let payload = PayloadRef::decode(entry.value())?;
        if let NodeRecordState::Live(record) = verify_node_state(
            PayloadSlice::new(source, roots.store(), entry.creation_generation(), payload),
            node,
            catalog,
            document,
            resources,
        )? {
            require_live_record_fence(source, catalog, document, roots, &record, resources)?;
            let RecordShape::Node { labels, .. } = record.shape() else {
                return Err(TreeError::Invalid("recovery node record role"));
            };
            for index in 0..labels {
                resources.step(1)?;
                let label = record.label(index, resources)?;
                let mut membership = [0_u8; 24];
                membership
                    .get_mut(..8)
                    .ok_or(TreeError::Memory)?
                    .copy_from_slice(&label.get().to_le_bytes());
                membership
                    .get_mut(8..)
                    .ok_or(TreeError::Memory)?
                    .copy_from_slice(&node.get().to_le_bytes());
                let member = lookup_entry(source, label_root, &membership, resources)?.ok_or(
                    TreeError::Invalid("recovery node label membership is absent"),
                )?;
                if !member.value().is_empty() {
                    return Err(TreeError::Invalid("recovery node label membership value"));
                }
            }
        }
        Ok(())
    })?;
    verify_directory(source, label_root, resources, &mut |entry, resources| {
        let crate::property_graph::storage::tree::Key::Inline(key) = entry.key() else {
            return Err(TreeError::Invalid("recovery label key role"));
        };
        let label = crate::property_graph::catalog::LabelId::new(u64::from_le_bytes(
            key.get(..8)
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or(TreeError::Invalid("recovery label key width"))?,
        ))
        .map_err(|_| TreeError::Invalid("recovery label identity"))?;
        let node = crate::property_graph::NodeId::new(u128::from_le_bytes(
            key.get(8..)
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or(TreeError::Invalid("recovery label node width"))?,
        ))
        .map_err(|_| TreeError::Invalid("recovery label node identity"))?;
        let node_entry = lookup_entry(source, node_root, &node.get().to_le_bytes(), resources)?
            .ok_or(TreeError::Invalid("recovery label node is absent"))?;
        let payload = PayloadRef::decode(node_entry.value())?;
        let NodeRecordState::Live(record) = verify_node_state(
            PayloadSlice::new(
                source,
                roots.store(),
                node_entry.creation_generation(),
                payload,
            ),
            node,
            catalog,
            document,
            resources,
        )?
        else {
            return Err(TreeError::Invalid("recovery label node is deleted"));
        };
        let RecordShape::Node { labels, .. } = record.shape() else {
            return Err(TreeError::Invalid("recovery label record role"));
        };
        let mut found = false;
        for index in 0..labels {
            resources.step(1)?;
            found |= record.label(index, resources)? == label;
        }
        if !found {
            return Err(TreeError::Invalid(
                "recovery label is absent from node record",
            ));
        }
        Ok(())
    })?;

    let relationship_root = roots.directory(TreeKind::Relationships)?;
    let mut values = NativeDirectoryValues::new(catalog, document);
    verify_directory(
        source,
        relationship_root,
        resources,
        &mut |entry, resources| {
            values.verify(source, relationship_root, entry, resources)?;
            let crate::property_graph::storage::tree::Key::Inline(key) = entry.key() else {
                return Err(TreeError::Invalid("recovery relationship key role"));
            };
            let id = u128::from_le_bytes(
                key.try_into()
                    .map_err(|_| TreeError::Invalid("recovery relationship key width"))?,
            );
            if id == 0 || id > high_waters.relationship {
                return Err(TreeError::Invalid("recovery relationship high-water"));
            }
            let relationship = crate::property_graph::RelId::new(id)
                .map_err(|_| TreeError::Invalid("recovery relationship identity"))?;
            let row = authoritative_relationship(
                source,
                catalog,
                document,
                roots,
                relationship,
                resources,
            )?
            .ok_or(TreeError::Invalid("recovery relationship disappeared"))?;
            let payload = PayloadRef::decode(entry.value())?;
            let record = verify_record(
                PayloadSlice::new(source, roots.store(), entry.creation_generation(), payload),
                EntityId::Relationship(relationship),
                catalog,
                document,
                resources,
            )?;
            require_live_record_fence(source, catalog, document, roots, &record, resources)?;
            exact_relationship_membership(source, roots, sequence, row, true, memory, resources)
        },
    )?;

    let type_root = roots.directory(TreeKind::RelationshipTypes)?;
    verify_directory(
        source,
        relationship_root,
        resources,
        &mut |entry, resources| {
            let crate::property_graph::storage::tree::Key::Inline(key) = entry.key() else {
                return Err(TreeError::Invalid("recovery relationship key role"));
            };
            let rel = crate::property_graph::RelId::new(u128::from_le_bytes(
                key.try_into()
                    .map_err(|_| TreeError::Invalid("recovery relationship key width"))?,
            ))
            .map_err(|_| TreeError::Invalid("recovery relationship identity"))?;
            let row = authoritative_relationship(source, catalog, document, roots, rel, resources)?
                .ok_or(TreeError::Invalid("recovery relationship disappeared"))?;
            let mut membership = [0_u8; 24];
            membership
                .get_mut(..8)
                .ok_or(TreeError::Memory)?
                .copy_from_slice(&row.relationship_type.get().to_le_bytes());
            membership
                .get_mut(8..)
                .ok_or(TreeError::Memory)?
                .copy_from_slice(&rel.get().to_le_bytes());
            let member = lookup_entry(source, type_root, &membership, resources)?.ok_or(
                TreeError::Invalid("recovery relationship type membership is absent"),
            )?;
            if !member.value().is_empty() {
                return Err(TreeError::Invalid(
                    "recovery relationship type membership value",
                ));
            }
            Ok(())
        },
    )?;
    verify_directory(source, type_root, resources, &mut |entry, resources| {
        let crate::property_graph::storage::tree::Key::Inline(key) = entry.key() else {
            return Err(TreeError::Invalid("recovery relationship type key role"));
        };
        let relationship_type = crate::property_graph::catalog::RelTypeId::new(u64::from_le_bytes(
            key.get(..8)
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or(TreeError::Invalid("recovery relationship type key width"))?,
        ))
        .map_err(|_| TreeError::Invalid("recovery relationship type identity"))?;
        let rel = crate::property_graph::RelId::new(u128::from_le_bytes(
            key.get(8..)
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or(TreeError::Invalid(
                    "recovery relationship type entity width",
                ))?,
        ))
        .map_err(|_| TreeError::Invalid("recovery relationship type entity"))?;
        let row = authoritative_relationship(source, catalog, document, roots, rel, resources)?
            .ok_or(TreeError::Invalid(
                "recovery relationship type entity is absent",
            ))?;
        if row.relationship_type != relationship_type {
            return Err(TreeError::Invalid(
                "recovery relationship type differs from record",
            ));
        }
        Ok(())
    })?;

    let mut scratch = RangeScratch::for_prepare(memory, resources)?;
    for kind in [TreeKind::OutRanges, TreeKind::InRanges] {
        let root = roots.directory(kind)?;
        verify_directory(source, root, resources, &mut |entry, resources| {
            let range = validate_range(source, root, entry, sequence, &mut scratch, resources)?;
            let key = range.descriptor().key();
            for edge in range.edges() {
                resources.step(1)?;
                let row = authoritative_relationship(
                    source, catalog, document, roots, edge.rel, resources,
                )?
                .ok_or(TreeError::Invalid(
                    "recovery adjacency relationship is absent",
                ))?;
                let matches = match key.direction {
                    Direction::Out => {
                        row.source == key.node
                            && row.target == edge.neighbor
                            && row.relationship_type == key.rel_type
                    }
                    Direction::In => {
                        row.target == key.node
                            && row.source == edge.neighbor
                            && row.relationship_type == key.rel_type
                    }
                };
                if !matches {
                    return Err(TreeError::Invalid("recovery adjacency topology mismatch"));
                }
            }
            Ok(())
        })?;
    }

    let inventory_root = roots.directory(TreeKind::ObjectInventory)?;
    verify_directory(
        source,
        inventory_root,
        resources,
        &mut |entry, resources| {
            let change = crate::property_graph::storage::inventory::verify_inventory_entry(
                inventory_root,
                entry,
                resources,
            )?;
            if matches!(
                change.state,
                InventoryState::ReclaimPending(_) | InventoryState::Reclaimed(_)
            ) {
                return Err(TreeError::Invalid("unsupported recovery inventory proof"));
            }
            source.validate_descriptor(change.object, resources)
        },
    )?;
    Ok(())
}

fn stored_keys_equal<S: BlockSource>(
    left: StoredKey<'_, S>,
    right: StoredKey<'_, S>,
    resources: &mut TreeResources<'_>,
) -> Result<bool, TreeError> {
    Ok(left.kind() == right.kind()
        && left
            .namespace()
            .compare(right.namespace(), resources)?
            .is_eq()
        && left.key().compare(right.key(), resources)?.is_eq())
}

fn stored_key_matches<S: BlockSource>(
    stored: StoredKey<'_, S>,
    key: ApplicationKey<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<bool, TreeError> {
    Ok(stored.kind() == key.kind()
        && stored
            .namespace()
            .compare_bytes(key.namespace().as_str().as_bytes(), resources)?
            .is_eq()
        && stored
            .key()
            .compare_bytes(key.key().as_str().as_bytes(), resources)?
            .is_eq())
}

fn stored_provenance_equal<S: BlockSource>(
    left: &StoredProvenance<'_, S>,
    right: &StoredProvenance<'_, S>,
    resources: &mut TreeResources<'_>,
) -> Result<bool, TreeError> {
    if left.operation() != right.operation()
        || left.requested_revision() != right.requested_revision()
        || left.installed_revision() != right.installed_revision()
        || left.expected() != right.expected()
        || left.incarnation() != right.incarnation()
        || left.delete_mode() != right.delete_mode()
        || left.original_generation() != right.original_generation()
    {
        return Ok(false);
    }
    match (left.key(), right.key()) {
        (None, None) => Ok(true),
        (Some(left), Some(right)) => stored_keys_equal(left, right, resources),
        _ => Ok(false),
    }
}

fn records_equal<S: BlockSource>(
    left: &RecordView<'_, S>,
    right: &RecordView<'_, S>,
    resources: &mut TreeResources<'_>,
) -> Result<bool, TreeError> {
    Ok(left.shape() == right.shape()
        && left.revision() == right.revision()
        && stored_provenance_equal(left.provenance(), right.provenance(), resources)?
        && left
            .canonical_bytes()
            .compare(right.canonical_bytes(), resources)?
            .is_eq())
}

#[allow(
    clippy::too_many_arguments,
    reason = "recovery compares one entity across two complete immutable roots"
)]
fn entity_logically_equal(
    source: &RecoverySource<'_, '_>,
    catalog: &RecoveryCatalog<'_, '_>,
    document: Option<&EmbeddingTower>,
    base: GraphRoots,
    target: GraphRoots,
    entity: EntityId,
    resources: &mut TreeResources<'_>,
) -> Result<bool, TreeError> {
    match entity {
        EntityId::Node(node) => {
            let load = |roots: GraphRoots, resources: &mut TreeResources<'_>| {
                let root = roots.directory(TreeKind::Nodes)?;
                let entry = lookup_entry(source, root, &node.get().to_le_bytes(), resources)?
                    .ok_or(TreeError::Invalid(
                        "recovery node transition entry is absent",
                    ))?;
                let payload = PayloadRef::decode(entry.value())?;
                verify_node_state(
                    PayloadSlice::new(source, roots.store(), entry.creation_generation(), payload),
                    node,
                    catalog,
                    document,
                    resources,
                )
            };
            let left = load(base, resources)?;
            let right = load(target, resources)?;
            match (left, right) {
                (NodeRecordState::Live(left), NodeRecordState::Live(right)) => {
                    records_equal(&left, &right, resources)
                }
                (NodeRecordState::Tombstone(left), NodeRecordState::Tombstone(right)) => Ok(left
                    .revision()
                    == right.revision()
                    && stored_provenance_equal(left.provenance(), right.provenance(), resources)?),
                _ => Ok(false),
            }
        }
        EntityId::Relationship(rel) => {
            let load = |roots: GraphRoots, resources: &mut TreeResources<'_>| {
                let root = roots.directory(TreeKind::Relationships)?;
                let entry = lookup_entry(source, root, &rel.get().to_le_bytes(), resources)?
                    .ok_or(TreeError::Invalid(
                        "recovery relationship transition entry is absent",
                    ))?;
                let payload = PayloadRef::decode(entry.value())?;
                verify_record(
                    PayloadSlice::new(source, roots.store(), entry.creation_generation(), payload),
                    entity,
                    catalog,
                    document,
                    resources,
                )
            };
            let left = load(base, resources)?;
            let right = load(target, resources)?;
            records_equal(&left, &right, resources)
        }
    }
}

fn mutation_count_for_entity(mutations: &[Mutation<'_>], entity: EntityId) -> usize {
    mutations
        .iter()
        .filter(|mutation| mutation.provenance.incarnation == entity)
        .count()
}

fn mutation_count_for_stored_key(
    mutations: &[Mutation<'_>],
    stored: StoredKey<'_, RecoverySource<'_, '_>>,
    resources: &mut TreeResources<'_>,
) -> Result<usize, TreeError> {
    let mut count = 0_usize;
    for mutation in mutations {
        resources.step(1)?;
        if let Some(key) = mutation.provenance.key
            && stored_key_matches(stored, key, resources)?
        {
            count = count.checked_add(1).ok_or(TreeError::Work)?;
        }
    }
    Ok(count)
}

#[allow(
    clippy::too_many_arguments,
    reason = "recovery streams exact base and target entity directories"
)]
fn reconcile_entity_directory(
    source: &RecoverySource<'_, '_>,
    catalog: &RecoveryCatalog<'_, '_>,
    document: Option<&EmbeddingTower>,
    base: GraphRoots,
    target: GraphRoots,
    kind: TreeKind,
    mutations: &[Mutation<'_>],
    base_high: u128,
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    let base_root = base.directory(kind)?;
    let target_root = target.directory(kind)?;
    let mut left = DirectoryCursor::seek(source, base_root, None, resources)?;
    let mut right = DirectoryCursor::seek(source, target_root, None, resources)?;
    let mut left_key = [0_u8; 16];
    let mut right_key = [0_u8; 16];
    let mut left_value = [0_u8; 48];
    let mut right_value = [0_u8; 48];
    let mut left_row = left.next(&mut left_key, &mut left_value, resources)?;
    let mut right_row = right.next(&mut right_key, &mut right_value, resources)?;
    while left_row.is_some() || right_row.is_some() {
        resources.step(1)?;
        let left_id = left_row
            .map(|(key, value)| {
                if key != 16 || value != 48 {
                    return Err(TreeError::Invalid("recovery entity directory row width"));
                }
                Ok(u128::from_le_bytes(left_key))
            })
            .transpose()?;
        let right_id = right_row
            .map(|(key, value)| {
                if key != 16 || value != 48 {
                    return Err(TreeError::Invalid("recovery entity directory row width"));
                }
                Ok(u128::from_le_bytes(right_key))
            })
            .transpose()?;
        let (id, changed, advance_left, advance_right) = match (left_id, right_id) {
            (Some(left), Some(right)) if left < right => (left, true, true, false),
            (Some(left), Some(right)) if left > right => (right, true, false, true),
            (Some(id), Some(_)) => {
                let entity = match kind {
                    TreeKind::Nodes => crate::property_graph::NodeId::new(id)
                        .map(EntityId::Node)
                        .map_err(|_| TreeError::Invalid("recovery node identity"))?,
                    TreeKind::Relationships => crate::property_graph::RelId::new(id)
                        .map(EntityId::Relationship)
                        .map_err(|_| TreeError::Invalid("recovery relationship identity"))?,
                    _ => return Err(TreeError::Invalid("recovery entity directory kind")),
                };
                (
                    id,
                    !entity_logically_equal(
                        source, catalog, document, base, target, entity, resources,
                    )?,
                    true,
                    true,
                )
            }
            (Some(left), None) => (left, true, true, false),
            (None, Some(right)) => (right, true, false, true),
            (None, None) => break,
        };
        let entity = match kind {
            TreeKind::Nodes => crate::property_graph::NodeId::new(id)
                .map(EntityId::Node)
                .map_err(|_| TreeError::Invalid("recovery node identity"))?,
            TreeKind::Relationships => crate::property_graph::RelId::new(id)
                .map(EntityId::Relationship)
                .map_err(|_| TreeError::Invalid("recovery relationship identity"))?,
            _ => return Err(TreeError::Invalid("recovery entity directory kind")),
        };
        let count = mutation_count_for_entity(mutations, entity);
        if (changed && count != 1) || (!changed && count != 0) {
            return Err(TreeError::Invalid(
                "recovery entity transition differs from mutation set",
            ));
        }
        if advance_right && !advance_left && id <= base_high {
            return Err(TreeError::Invalid(
                "recovery fresh identity below high-water",
            ));
        }
        if advance_left {
            left_row = left.next(&mut left_key, &mut left_value, resources)?;
        }
        if advance_right {
            right_row = right.next(&mut right_key, &mut right_value, resources)?;
        }
    }
    Ok(())
}

fn find_fence_by_key<'s, 'store, 'm, 'catalog_memory>(
    source: &'s RecoverySource<'store, 'm>,
    catalog: &RecoveryCatalog<'s, 'catalog_memory>,
    document: Option<&EmbeddingTower>,
    roots: GraphRoots,
    wanted: StoredKey<'s, RecoverySource<'store, 'm>>,
    resources: &mut TreeResources<'_>,
) -> Result<Option<FenceView<'s, RecoverySource<'store, 'm>>>, TreeError> {
    let root = roots.directory(TreeKind::KeyFences)?;
    let mut cursor = DirectoryCursor::seek(source, root, None, resources)?;
    while let Some(entry) = cursor.next_entry(resources)? {
        let fence = verify_fence_entry(source, root, entry, catalog, document, resources)?;
        let key = fence
            .provenance()
            .key()
            .ok_or(TreeError::Invalid("recovery fence is unkeyed"))?;
        if stored_keys_equal(wanted, key, resources)? {
            return Ok(Some(fence));
        }
    }
    Ok(None)
}

fn fences_equal(
    left: &FenceView<'_, RecoverySource<'_, '_>>,
    right: &FenceView<'_, RecoverySource<'_, '_>>,
    resources: &mut TreeResources<'_>,
) -> Result<bool, TreeError> {
    if left.incarnation() != right.incarnation()
        || left.revision() != right.revision()
        || left.is_deleted() != right.is_deleted()
        || !stored_provenance_equal(left.provenance(), right.provenance(), resources)?
    {
        return Ok(false);
    }
    match (left.canonical_bytes(), right.canonical_bytes()) {
        (None, None) => Ok(true),
        (Some(left), Some(right)) => Ok(left.compare(right, resources)?.is_eq()),
        _ => Ok(false),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "recovery streams the exact permanent key ledger in both directions"
)]
fn reconcile_fence_directory(
    source: &RecoverySource<'_, '_>,
    catalog: &RecoveryCatalog<'_, '_>,
    document: Option<&EmbeddingTower>,
    base: GraphRoots,
    target: GraphRoots,
    mutations: &[Mutation<'_>],
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    for (outer_roots, inner_roots) in [(base, target), (target, base)] {
        let root = outer_roots.directory(TreeKind::KeyFences)?;
        let mut cursor = DirectoryCursor::seek(source, root, None, resources)?;
        while let Some(entry) = cursor.next_entry(resources)? {
            let fence = verify_fence_entry(source, root, entry, catalog, document, resources)?;
            let key = fence
                .provenance()
                .key()
                .ok_or(TreeError::Invalid("recovery fence is unkeyed"))?;
            let counterpart =
                find_fence_by_key(source, catalog, document, inner_roots, key, resources)?;
            let changed = match counterpart.as_ref() {
                None => true,
                Some(other) => !fences_equal(&fence, other, resources)?,
            };
            let count = mutation_count_for_stored_key(mutations, key, resources)?;
            if (changed && count != 1) || (!changed && count != 0) {
                return Err(TreeError::Invalid(
                    "recovery key-fence transition differs from mutation set",
                ));
            }
        }
    }
    Ok(())
}

struct RecoveryPayloadReader<'source, 'resources, 'store, 'source_memory, 'tree_memory> {
    slice: PayloadSlice<'source, RecoverySource<'store, 'source_memory>>,
    offset: u64,
    resources: &'resources RefCell<&'resources mut TreeResources<'tree_memory>>,
    first_error: &'resources Cell<Option<TreeError>>,
}

impl Read for RecoveryPayloadReader<'_, '_, '_, '_, '_> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let mut resources = self
            .resources
            .try_borrow_mut()
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::WouldBlock))?;
        let remaining = usize::try_from(self.slice.len().saturating_sub(self.offset))
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::OutOfMemory))?;
        let target = output
            .get_mut(..remaining.min(output.len()))
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        let read = self
            .slice
            .read_at(self.offset, target, &mut resources)
            .map_err(|error| {
                let previous = self.first_error.take();
                self.first_error.set(previous.or(Some(error)));
                std::io::Error::from(std::io::ErrorKind::Other)
            })?;
        self.offset = self
            .offset
            .checked_add(read as u64)
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidData))?;
        Ok(read)
    }
}

fn streamed_fingerprint<S: BlockSource>(
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
            return Err(TreeError::Invalid("short recovery canonical"));
        }
        hash.update(output);
        offset = offset.checked_add(read as u64).ok_or(TreeError::Memory)?;
    }
    CanonicalFingerprint::new(slice.len(), hash.digest())
        .map_err(|_| TreeError::Invalid("invalid recovery canonical fingerprint"))
}

fn recovery_entity_shape<'catalog>(
    shape: RecordShape,
    catalog: &'catalog RecoveryCatalog<'catalog, '_>,
    resources: &mut TreeResources<'_>,
) -> Result<EntityShape<'catalog>, TreeError> {
    match shape {
        RecordShape::Node { .. } => Ok(EntityShape::Node),
        RecordShape::Relationship {
            source,
            target,
            relationship_type,
            ..
        } => Ok(EntityShape::Relationship {
            source,
            target,
            relationship_type: catalog
                .symbol_name(Symbol::RelationshipType(relationship_type), resources)?,
        }),
    }
}

fn live_record<'source, 'store, 'm>(
    source: &'source RecoverySource<'store, 'm>,
    catalog: &RecoveryCatalog<'source, '_>,
    document: Option<&EmbeddingTower>,
    roots: GraphRoots,
    entity: EntityId,
    resources: &mut TreeResources<'_>,
) -> Result<Option<RecordView<'source, RecoverySource<'store, 'm>>>, TreeError> {
    match entity {
        EntityId::Node(node) => {
            let root = roots.directory(TreeKind::Nodes)?;
            let Some(entry) = lookup_entry(source, root, &node.get().to_le_bytes(), resources)?
            else {
                return Ok(None);
            };
            let payload = PayloadRef::decode(entry.value())?;
            match verify_node_state(
                PayloadSlice::new(source, roots.store(), entry.creation_generation(), payload),
                node,
                catalog,
                document,
                resources,
            )? {
                NodeRecordState::Live(record) => Ok(Some(record)),
                NodeRecordState::Tombstone(_) => Ok(None),
            }
        }
        EntityId::Relationship(rel) => {
            let root = roots.directory(TreeKind::Relationships)?;
            let Some(entry) = lookup_entry(source, root, &rel.get().to_le_bytes(), resources)?
            else {
                return Ok(None);
            };
            let payload = PayloadRef::decode(entry.value())?;
            verify_record(
                PayloadSlice::new(source, roots.store(), entry.creation_generation(), payload),
                entity,
                catalog,
                document,
                resources,
            )
            .map(Some)
        }
    }
}

fn lifecycle_tree_error(
    first_error: &Cell<Option<TreeError>>,
    _: crate::property_graph::KeyLifecycleError,
) -> TreeError {
    first_error
        .take()
        .unwrap_or(TreeError::Invalid("recovery key lifecycle transition"))
}

fn recovery_checkpoint(
    resources: &RefCell<&mut TreeResources<'_>>,
    first_error: &Cell<Option<TreeError>>,
) -> Result<(), CanonicalError> {
    let mut resources = resources
        .try_borrow_mut()
        .map_err(|_| CanonicalError::Cancelled)?;
    resources.step(1).map_err(|error| {
        let previous = first_error.take();
        first_error.set(previous.or(Some(error)));
        CanonicalError::Cancelled
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "recovery replays exact base lifecycle against the persisted target"
)]
fn validate_lifecycle_transition<'source, 'store, 'source_memory, 'catalog_memory, 'tree_memory>(
    source: &'source RecoverySource<'store, 'source_memory>,
    catalog: &'source RecoveryCatalog<'source, 'catalog_memory>,
    document: Option<&EmbeddingTower>,
    base_roots: GraphRoots,
    target_roots: GraphRoots,
    base_high_waters: crate::property_graph::wal::HighWaters,
    mutation: Mutation<'source>,
    resources: &mut TreeResources<'tree_memory>,
) -> Result<(), TreeError> {
    let fields = mutation.provenance;
    if mutation.provenance_version != 1
        || fields.requested_revision != fields.installed_revision
        || fields.original_generation != target_roots.generation()
    {
        return Err(TreeError::Invalid("recovery mutation provenance domain"));
    }
    let fresh_high = match fields.incarnation {
        EntityId::Node(node) => (node.get(), base_high_waters.node),
        EntityId::Relationship(rel) => (rel.get(), base_high_waters.relationship),
    };
    if fields.operation == GraphOperation::CypherEdit
        && fields.expected == ExpectedGraphState::Absent
    {
        if fields.key.is_some()
            || !mutation.live
            || fields.delete_mode.is_some()
            || fields.requested_revision.get() != 1
            || fresh_high.0 <= fresh_high.1
        {
            return Err(TreeError::Invalid("recovery fresh Cypher lifecycle"));
        }
        return Ok(());
    }
    if matches!(
        fields.operation,
        GraphOperation::StructuredCreate | GraphOperation::StructuredRecreate
    ) && fresh_high.0 <= fresh_high.1
    {
        return Err(TreeError::Invalid(
            "recovery fresh identity below high-water",
        ));
    }

    let target_record = if mutation.live {
        Some(
            live_record(
                source,
                catalog,
                document,
                target_roots,
                fields.incarnation,
                resources,
            )?
            .ok_or(TreeError::Invalid(
                "recovery target lifecycle record is absent",
            ))?,
        )
    } else {
        None
    };
    let mut base_provenance = None;
    let mut base_deleted = false;
    let base_record = if fields.operation == GraphOperation::CypherEdit {
        let ExpectedGraphState::Entity(expected) = fields.expected else {
            return Err(TreeError::Invalid("recovery Cypher base state"));
        };
        if expected != fields.incarnation {
            return Err(TreeError::Invalid("recovery Cypher incarnation"));
        }
        let record = live_record(source, catalog, document, base_roots, expected, resources)?
            .ok_or(TreeError::Invalid("recovery Cypher base record is absent"))?;
        base_provenance = Some(
            OperationProvenance::from_fields(
                Some(1),
                record.provenance().fields_with_key(fields.key, resources)?,
            )
            .map_err(|_| TreeError::Invalid("recovery Cypher base provenance"))?,
        );
        Some(record)
    } else {
        let key = fields
            .key
            .ok_or(TreeError::Invalid("recovery structured key is absent"))?;
        let namespace =
            match catalog.lookup_symbol(SymbolKind::Namespace, key.namespace(), resources)? {
                Some(Symbol::Namespace(namespace)) => namespace,
                _ => return Err(TreeError::Invalid("recovery base key namespace is absent")),
            };
        let root = base_roots.directory(TreeKind::KeyFences)?;
        let fence_key = FenceKey::new(key.kind(), namespace, key.key().as_str())?;
        let fence = lookup_fence_entry(source, root, fence_key, resources)?
            .map(|entry| verify_fence_entry(source, root, entry, catalog, document, resources))
            .transpose()?;
        match fence {
            None => None,
            Some(fence) => {
                base_deleted = fence.is_deleted();
                base_provenance = Some(
                    OperationProvenance::from_fields(
                        Some(1),
                        fence.provenance().fields_with_key(Some(key), resources)?,
                    )
                    .map_err(|_| TreeError::Invalid("recovery structured base provenance"))?,
                );
                if fence.is_deleted() {
                    None
                } else {
                    let record = live_record(
                        source,
                        catalog,
                        document,
                        base_roots,
                        fence.incarnation(),
                        resources,
                    )?
                    .ok_or(TreeError::Invalid("recovery live fence record is absent"))?;
                    if !record
                        .canonical_bytes()
                        .compare(
                            fence.canonical_bytes().ok_or(TreeError::Invalid(
                                "recovery live fence canonical is absent",
                            ))?,
                            resources,
                        )?
                        .is_eq()
                        || !stored_provenance_equal(
                            record.provenance(),
                            fence.provenance(),
                            resources,
                        )?
                    {
                        return Err(TreeError::Invalid("recovery live fence/entity mismatch"));
                    }
                    Some(record)
                }
            }
        }
    };

    let base_fingerprint = base_record
        .as_ref()
        .map(|record| streamed_fingerprint(record.canonical_bytes(), resources))
        .transpose()?;
    let target_fingerprint = target_record
        .as_ref()
        .map(|record| streamed_fingerprint(record.canonical_bytes(), resources))
        .transpose()?;
    let base_shape = base_record
        .as_ref()
        .map(|record| recovery_entity_shape(record.shape(), catalog, resources))
        .transpose()?;
    let target_shape = target_record
        .as_ref()
        .map(|record| recovery_entity_shape(record.shape(), catalog, resources))
        .transpose()?;

    let first_error = Cell::new(None);
    let resources_cell = RefCell::new(resources);
    let mut base_reader = base_record.as_ref().map(|record| RecoveryPayloadReader {
        slice: record.canonical_bytes(),
        offset: 0,
        resources: &resources_cell,
        first_error: &first_error,
    });
    let mut target_reader = target_record.as_ref().map(|record| RecoveryPayloadReader {
        slice: record.canonical_bytes(),
        offset: 0,
        resources: &resources_cell,
        first_error: &first_error,
    });
    let mut scratch = [0_u8; 4096];
    let mut checkpoint = || recovery_checkpoint(&resources_cell, &first_error);
    let decision = if fields.operation == GraphOperation::CypherEdit {
        let current = CurrentEntity {
            provenance: base_provenance
                .ok_or(TreeError::Invalid("recovery Cypher provenance is absent"))?,
            contents: CanonicalRecord::from_validated(
                base_shape.ok_or(TreeError::Invalid("recovery Cypher shape is absent"))?,
                base_fingerprint
                    .ok_or(TreeError::Invalid("recovery Cypher fingerprint is absent"))?,
                base_reader
                    .as_mut()
                    .ok_or(TreeError::Invalid("recovery Cypher source is absent"))?,
            ),
        };
        let edit = match (mutation.live, fields.delete_mode) {
            (true, None) => CypherEdit::Put(CanonicalRecord::from_validated(
                target_shape.ok_or(TreeError::Invalid("recovery Cypher target shape"))?,
                target_fingerprint
                    .ok_or(TreeError::Invalid("recovery Cypher target fingerprint"))?,
                target_reader
                    .as_mut()
                    .ok_or(TreeError::Invalid("recovery Cypher target source"))?,
            )),
            (false, Some(mode)) => CypherEdit::Delete(mode),
            _ => return Err(TreeError::Invalid("recovery Cypher target operation")),
        };
        classify_cypher(Some(current), edit, &mut scratch, &mut checkpoint)
            .map_err(|error| lifecycle_tree_error(&first_error, error))?
    } else {
        let key = fields
            .key
            .ok_or(TreeError::Invalid("recovery structured key is absent"))?;
        let state = match (base_provenance, base_deleted, base_reader.as_mut()) {
            (None, false, None) => KeyState::NeverUsed,
            (Some(provenance), true, None) => KeyState::Deleted(provenance),
            (Some(provenance), false, Some(reader)) => KeyState::Live(CurrentEntity {
                provenance,
                contents: CanonicalRecord::from_validated(
                    base_shape.ok_or(TreeError::Invalid("recovery structured base shape"))?,
                    base_fingerprint
                        .ok_or(TreeError::Invalid("recovery structured base fingerprint"))?,
                    reader,
                ),
            }),
            _ => return Err(TreeError::Invalid("recovery structured base state")),
        };
        let request = match fields.operation {
            GraphOperation::StructuredCreate => KeyRequest::Create {
                revision: fields.requested_revision,
                contents: CanonicalRecord::from_validated(
                    target_shape.ok_or(TreeError::Invalid("recovery structured target shape"))?,
                    target_fingerprint
                        .ok_or(TreeError::Invalid("recovery structured target fingerprint"))?,
                    target_reader
                        .as_mut()
                        .ok_or(TreeError::Invalid("recovery structured create source"))?,
                ),
            },
            GraphOperation::StructuredPut => KeyRequest::Put {
                revision: fields.requested_revision,
                expected: match fields.expected {
                    ExpectedGraphState::Entity(expected) => expected,
                    _ => return Err(TreeError::Invalid("recovery structured put precondition")),
                },
                contents: CanonicalRecord::from_validated(
                    target_shape.ok_or(TreeError::Invalid("recovery structured target shape"))?,
                    target_fingerprint
                        .ok_or(TreeError::Invalid("recovery structured target fingerprint"))?,
                    target_reader
                        .as_mut()
                        .ok_or(TreeError::Invalid("recovery structured put source"))?,
                ),
            },
            GraphOperation::StructuredDelete => KeyRequest::Delete {
                revision: fields.requested_revision,
                expected: match fields.expected {
                    ExpectedGraphState::Entity(expected) => expected,
                    _ => {
                        return Err(TreeError::Invalid(
                            "recovery structured delete precondition",
                        ));
                    }
                },
                mode: fields
                    .delete_mode
                    .ok_or(TreeError::Invalid("recovery structured delete mode"))?,
            },
            GraphOperation::StructuredRecreate => KeyRequest::Recreate {
                revision: fields.requested_revision,
                deleted_revision: match fields.expected {
                    ExpectedGraphState::Deletion(revision) => revision,
                    _ => {
                        return Err(TreeError::Invalid(
                            "recovery structured recreate precondition",
                        ));
                    }
                },
                contents: CanonicalRecord::from_validated(
                    target_shape.ok_or(TreeError::Invalid("recovery structured target shape"))?,
                    target_fingerprint
                        .ok_or(TreeError::Invalid("recovery structured target fingerprint"))?,
                    target_reader
                        .as_mut()
                        .ok_or(TreeError::Invalid("recovery structured recreate source"))?,
                ),
            },
            GraphOperation::CypherEdit => {
                return Err(TreeError::Invalid("recovery structured operation"));
            }
        };
        classify_key(key, state, request, &mut scratch, &mut checkpoint)
            .map_err(|error| lifecycle_tree_error(&first_error, error))?
    };
    let KeyDecision::Change(change) = decision else {
        return Err(TreeError::Invalid(
            "recovery mutation is not a lifecycle change",
        ));
    };
    let installed = change
        .install(
            fields.incarnation,
            target_roots.generation(),
            &mut checkpoint,
        )
        .map_err(|error| lifecycle_tree_error(&first_error, error))?;
    if installed.fields() != fields {
        return Err(TreeError::Invalid("recovery lifecycle provenance outcome"));
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "recovery mutation validation binds exact complete base and target owners"
)]
fn validate_mutation_state(
    source: &RecoverySource<'_, '_>,
    catalog: &RecoveryCatalog<'_, '_>,
    memory: &StorageMemory<'_>,
    document: Option<&EmbeddingTower>,
    base_roots: GraphRoots,
    target_roots: GraphRoots,
    base_high_waters: crate::property_graph::wal::HighWaters,
    base_sequence: u64,
    target_sequence: u64,
    mutation: Mutation<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    let fields = mutation.provenance;
    if mutation.provenance_version != 1
        || fields.requested_revision != fields.installed_revision
        || fields.original_generation != target_roots.generation()
        || target_sequence != base_sequence.checked_add(1).ok_or(TreeError::Work)?
    {
        return Err(TreeError::Invalid("recovery mutation provenance domain"));
    }
    validate_lifecycle_transition(
        source,
        catalog,
        document,
        base_roots,
        target_roots,
        base_high_waters,
        mutation,
        resources,
    )?;
    let canonical = mutation
        .canonical
        .map(|required| canonical_payload(source, required, resources))
        .transpose()?
        .map(|payload| {
            PayloadSlice::new(
                source,
                target_roots.store(),
                target_roots.generation(),
                payload,
            )
        });
    match fields.incarnation {
        crate::property_graph::EntityId::Node(node) => {
            let root = target_roots.directory(TreeKind::Nodes)?;
            let entry = lookup_entry(source, root, &node.get().to_le_bytes(), resources)?
                .ok_or(TreeError::Invalid("recovery target node is absent"))?;
            let payload = PayloadRef::decode(entry.value())?;
            let state = verify_node_state(
                PayloadSlice::new(
                    source,
                    target_roots.store(),
                    entry.creation_generation(),
                    payload,
                ),
                node,
                catalog,
                document,
                resources,
            )?;
            match state {
                NodeRecordState::Live(record) => {
                    if record.revision() != fields.installed_revision
                        || record.provenance().fields_with_key(fields.key, resources)? != fields
                        || !mutation.live
                        || canonical.is_none()
                        || !record
                            .canonical_bytes()
                            .compare(
                                canonical
                                    .ok_or(TreeError::Invalid("recovery node canonical outcome"))?,
                                resources,
                            )?
                            .is_eq()
                    {
                        return Err(TreeError::Invalid("recovery node mutation outcome"));
                    }
                }
                NodeRecordState::Tombstone(tombstone) => {
                    if tombstone.revision() != fields.installed_revision
                        || tombstone
                            .provenance()
                            .fields_with_key(fields.key, resources)?
                            != fields
                        || mutation.live
                        || canonical.is_some()
                    {
                        return Err(TreeError::Invalid("recovery node tombstone outcome"));
                    }
                }
            }
        }
        crate::property_graph::EntityId::Relationship(rel) => {
            if mutation.live {
                let root = target_roots.directory(TreeKind::Relationships)?;
                let entry = lookup_entry(source, root, &rel.get().to_le_bytes(), resources)?
                    .ok_or(TreeError::Invalid("recovery target relationship is absent"))?;
                let payload = PayloadRef::decode(entry.value())?;
                let record = verify_record(
                    PayloadSlice::new(
                        source,
                        target_roots.store(),
                        entry.creation_generation(),
                        payload,
                    ),
                    crate::property_graph::EntityId::Relationship(rel),
                    catalog,
                    document,
                    resources,
                )?;
                if record.revision() != fields.installed_revision
                    || record.provenance().fields_with_key(fields.key, resources)? != fields
                    || canonical.is_none()
                    || !record
                        .canonical_bytes()
                        .compare(
                            canonical.ok_or(TreeError::Invalid(
                                "recovery relationship canonical outcome",
                            ))?,
                            resources,
                        )?
                        .is_eq()
                {
                    return Err(TreeError::Invalid("recovery relationship mutation outcome"));
                }
                exact_relationship_membership(
                    source,
                    target_roots,
                    target_sequence,
                    authoritative_relationship(
                        source,
                        catalog,
                        document,
                        target_roots,
                        rel,
                        resources,
                    )?
                    .ok_or(TreeError::Invalid("recovery relationship is absent"))?,
                    true,
                    memory,
                    resources,
                )?;
            } else {
                let target_root = target_roots.directory(TreeKind::Relationships)?;
                if lookup_entry(source, target_root, &rel.get().to_le_bytes(), resources)?.is_some()
                    || canonical.is_some()
                {
                    return Err(TreeError::Invalid("recovery deleted relationship is live"));
                }
                let base_root = base_roots.directory(TreeKind::Relationships)?;
                let base_entry =
                    lookup_entry(source, base_root, &rel.get().to_le_bytes(), resources)?.ok_or(
                        TreeError::Invalid("recovery deleted relationship base is absent"),
                    )?;
                let base_payload = PayloadRef::decode(base_entry.value())?;
                let base_record = verify_record(
                    PayloadSlice::new(
                        source,
                        base_roots.store(),
                        base_entry.creation_generation(),
                        base_payload,
                    ),
                    crate::property_graph::EntityId::Relationship(rel),
                    catalog,
                    document,
                    resources,
                )?;
                let crate::property_graph::storage::records::RecordShape::Relationship {
                    id,
                    source: relationship_source,
                    target: relationship_target,
                    relationship_type,
                } = base_record.shape()
                else {
                    return Err(TreeError::Invalid("recovery relationship base role"));
                };
                let row = crate::property_graph::storage::adjacency::RelationshipRow {
                    rel: id,
                    source: relationship_source,
                    target: relationship_target,
                    relationship_type,
                };
                exact_relationship_membership(
                    source,
                    target_roots,
                    target_sequence,
                    row,
                    false,
                    memory,
                    resources,
                )?;
            }
        }
    }
    if let Some(key) = fields.key {
        let namespace =
            match catalog.lookup_symbol(SymbolKind::Namespace, key.namespace(), resources)? {
                Some(Symbol::Namespace(namespace)) => namespace,
                _ => return Err(TreeError::Invalid("recovery key namespace is absent")),
            };
        let root = target_roots.directory(TreeKind::KeyFences)?;
        let fence_key = FenceKey::new(key.kind(), namespace, key.key().as_str())?;
        let entry = lookup_fence_entry(source, root, fence_key, resources)?
            .ok_or(TreeError::Invalid("recovery key fence is absent"))?;
        let fence = verify_fence_entry(source, root, entry, catalog, document, resources)?;
        if fence.incarnation() != fields.incarnation
            || fence.revision() != fields.installed_revision
            || fence.provenance().fields_with_key(Some(key), resources)? != fields
            || fence.canonical_bytes().is_some() != mutation.live
        {
            return Err(TreeError::Invalid("recovery key fence outcome"));
        }
        if let (Some(stored), Some(expected)) = (fence.canonical_bytes(), canonical)
            && !stored.compare(expected, resources)?.is_eq()
        {
            return Err(TreeError::Invalid("recovery fence canonical outcome"));
        }
    }
    Ok(())
}

fn selected_root(store: &Store, directory: &Path) -> Result<RequiredRef, NativeGraphError> {
    let path = directory.join(ROOT_SELECTOR);
    let length = store
        .vfs
        .open(&path)
        .map_err(|source| NativeGraphError::Io {
            path: path.clone(),
            source,
        })?;
    if length != ROOT_SELECTOR_BYTES as u64 {
        return Err(NativeGraphError::Invalid(
            "corrupt native graph root selector",
        ));
    }
    let mapping = map_file(store, &path, ROOT_SELECTOR_BYTES)?;
    decode_root_selector(mapping.as_bytes()).map_err(|classification| match classification {
        NativeStoreClassification::Incomplete => NativeGraphError::StoreInitializationIncomplete,
        NativeStoreClassification::Incompatible => {
            NativeGraphError::Invalid("incompatible native graph root")
        }
        NativeStoreClassification::Corrupt | NativeStoreClassification::Complete { .. } => {
            NativeGraphError::Invalid("corrupt native graph root")
        }
    })
}

fn artifact_name(path: &Path) -> Option<ArtifactId> {
    let name = path.file_name()?.to_str()?;
    let digits = name.strip_prefix("graph-")?.strip_suffix(".zgraph")?;
    if digits.len() != 32 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    ArtifactId::new(u128::from_str_radix(digits, 16).ok()?).ok()
}

fn scan_creation_serials(
    store: &Store,
    resources: &GraphResources,
    directory: &Path,
    expected_store: crate::property_graph::StoreInstanceId,
    committed_generation: crate::property_graph::GraphGeneration,
    initial: u64,
    control: &QueryControl,
) -> Result<u64, NativeGraphError> {
    let mut maximum = initial;
    let mut first_error = None;
    let _serial_charge = resources.reserve(
        MAX_RECOVERED_DESCRIPTORS
            .checked_mul(std::mem::size_of::<u64>())
            .ok_or(NativeGraphError::Invalid("recovery serial scan capacity"))?,
    )?;
    let mut serials = Vec::new();
    serials
        .try_reserve_exact(MAX_RECOVERED_DESCRIPTORS)
        .map_err(|_| NativeGraphError::Invalid("recovery serial scan allocation"))?;
    store
        .vfs
        .for_each_direct_child(directory, &mut |path| {
            if first_error.is_some() {
                return Ok(());
            }
            let result = (|| {
                control
                    .checkpoint()
                    .map_err(crate::property_graph::storage::tree::directory::TreeError::Control)
                    .map_err(NativeGraphError::Read)?;
                let Some(artifact) = artifact_name(path) else {
                    return Ok(());
                };
                let length = store
                    .vfs
                    .open(path)
                    .map_err(|source| NativeGraphError::Io {
                        path: path.to_path_buf(),
                        source,
                    })?;
                if length < artifact::HEADER_BYTES as u64 {
                    return Ok(());
                }
                let header = store
                    .vfs
                    .read_range(path, 0, artifact::HEADER_BYTES)
                    .map_err(|source| NativeGraphError::Io {
                        path: path.to_path_buf(),
                        source,
                    })?;
                let field = |range: std::ops::Range<usize>| {
                    header.get(range).ok_or(NativeGraphError::Invalid(
                        "truncated native artifact header",
                    ))
                };
                let family = u16::from_le_bytes(
                    field(8..10)?
                        .try_into()
                        .map_err(|_| NativeGraphError::Invalid("native artifact family"))?,
                );
                let declared = u64::from_le_bytes(
                    field(24..32)?
                        .try_into()
                        .map_err(|_| NativeGraphError::Invalid("native artifact length"))?,
                );
                let header_store = u128::from_le_bytes(
                    field(32..48)?
                        .try_into()
                        .map_err(|_| NativeGraphError::Invalid("native artifact store"))?,
                );
                let header_artifact = u128::from_le_bytes(
                    field(48..64)?
                        .try_into()
                        .map_err(|_| NativeGraphError::Invalid("native artifact identity"))?,
                );
                let serial = u64::from_le_bytes(
                    field(88..96)?
                        .try_into()
                        .map_err(|_| NativeGraphError::Invalid("native artifact serial"))?,
                );
                let generation = u64::from_le_bytes(
                    field(64..72)?
                        .try_into()
                        .map_err(|_| NativeGraphError::Invalid("native artifact generation"))?,
                );
                let header_length = u64::from_le_bytes(
                    field(16..24)?
                        .try_into()
                        .map_err(|_| NativeGraphError::Invalid("native artifact header length"))?,
                );
                if field(0..8)? != b"ZEPEMBED"
                    || !matches!(family, 17 | 18)
                    || field(10..12)? != 1_u16.to_le_bytes()
                    || field(12..16)? != [0_u8; 4]
                    || header_length != artifact::HEADER_BYTES as u64
                    || header_store != expected_store.get()
                    || header_artifact != artifact.get()
                    || generation
                        > committed_generation
                            .get()
                            .checked_add(1)
                            .ok_or(NativeGraphError::IdentityExhausted)?
                    || serial == 0
                    || declared < (artifact::HEADER_BYTES + 8) as u64
                    || declared > MAX_ARTIFACT_BYTES as u64
                {
                    return Err(NativeGraphError::Invalid(
                        "corrupt recognized native artifact header",
                    ));
                }
                if serials.len() == serials.capacity() {
                    return Err(NativeGraphError::Invalid("recovery serial scan capacity"));
                }
                for previous in &serials {
                    control
                        .checkpoint()
                        .map_err(
                            crate::property_graph::storage::tree::directory::TreeError::Control,
                        )
                        .map_err(NativeGraphError::Read)?;
                    if *previous == serial {
                        return Err(NativeGraphError::Invalid(
                            "duplicate native artifact creation serial",
                        ));
                    }
                }
                serials.push(serial);
                if length < declared {
                    maximum = maximum.max(serial);
                    return Ok(());
                }
                if length != declared {
                    return Err(NativeGraphError::Invalid(
                        "ambiguous recognized native artifact extent",
                    ));
                }
                let mapping = map_file(store, path, MAX_ARTIFACT_BYTES)?;
                let container = if family == crate::format::FormatFamily::NativeGraphObject.id() {
                    ContainerKind::Object
                } else {
                    ContainerKind::RootEnvelope
                };
                let frame = artifact::decode_with_control(
                    container,
                    Some((expected_store, artifact)),
                    mapping.as_bytes(),
                    &mut |_| {
                        control.checkpoint().map_err(
                            crate::property_graph::storage::tree::directory::TreeError::Control,
                        )
                    },
                )
                .map_err(|error| match error {
                    ArtifactControlError::Format(_) => {
                        NativeGraphError::Invalid("corrupt recognized native artifact")
                    }
                    ArtifactControlError::Control(error) => NativeGraphError::Read(error),
                })?;
                let identity = frame.identity();
                if identity.store != expected_store || identity.artifact != artifact {
                    return Err(NativeGraphError::Invalid(
                        "recognized native artifact identity mismatch",
                    ));
                }
                maximum = maximum.max(identity.creation_serial);
                Ok(())
            })();
            if let Err(error) = result {
                first_error = Some(error);
            }
            Ok(())
        })
        .map_err(|source| NativeGraphError::Io {
            path: directory.to_path_buf(),
            source,
        })?;
    if let Some(error) = first_error {
        return Err(error);
    }
    if maximum == u64::MAX {
        return Err(NativeGraphError::IdentityExhausted);
    }
    Ok(maximum)
}

#[derive(Clone, Copy)]
struct CatalogWaters {
    node: u128,
    relationship: u128,
    symbols: SymbolHighWaters,
}

struct SemanticReplay<'a, 'm> {
    store: &'a Store,
    directory: &'a Path,
    expected: GraphInterpretation<'a>,
    document: Option<&'a EmbeddingTower>,
    resources: &'a GraphResources,
    memory: &'m StorageMemory<'m>,
    protected: Vec<ArtifactDescriptor>,
    inventory_start: usize,
    _charge: GraphReservation,
    catalog: Option<CatalogWaters>,
    first_error: Option<NativeGraphError>,
}

impl<'a, 'm> SemanticReplay<'a, 'm> {
    fn new(
        store: &'a Store,
        directory: &'a Path,
        expected: GraphInterpretation<'a>,
        document: Option<&'a EmbeddingTower>,
        resources: &'a GraphResources,
        memory: &'m StorageMemory<'m>,
    ) -> Result<Self, NativeGraphError> {
        let bytes = MAX_RECOVERED_DESCRIPTORS
            .checked_mul(std::mem::size_of::<ArtifactDescriptor>())
            .ok_or(NativeGraphError::Invalid("recovery descriptor capacity"))?;
        let charge = resources.reserve(bytes)?;
        let mut protected = Vec::new();
        protected
            .try_reserve_exact(MAX_RECOVERED_DESCRIPTORS)
            .map_err(|_| NativeGraphError::Invalid("recovery descriptor allocation"))?;
        Ok(Self {
            store,
            directory,
            expected,
            document,
            resources,
            memory,
            protected,
            inventory_start: 0,
            _charge: charge,
            catalog: None,
            first_error: None,
        })
    }

    fn fail(&mut self, error: NativeGraphError, reported: WalError) -> WalError {
        if self.first_error.is_none() {
            self.first_error = Some(error);
        }
        reported
    }

    fn frame(&mut self, reference: RequiredRef) -> Result<NativeReadonlyMapping, WalError> {
        let path = crate::property_graph::storage::allocation::artifact_path(
            self.directory,
            reference.object.artifact,
        );
        match map_file(self.store, &path, MAX_ARTIFACT_BYTES) {
            Ok(mapping) => Ok(mapping),
            Err(error) => Err(self.fail(error, WalError::MissingArtifact)),
        }
    }

    fn validate_reference(
        &mut self,
        reference: RequiredRef,
        role: RequiredRole,
        resources: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        if matches!(
            role,
            RequiredRole::Participant(
                ParticipantRole::ProtectedRoots
                    | ParticipantRole::CompletedMark
                    | ParticipantRole::ReclaimState
            )
        ) {
            return Err(WalError::Participant);
        }
        let mapping = self.frame(reference)?;
        let frame = artifact::decode_with_control(
            ContainerKind::Object,
            Some((reference.object.store, reference.object.artifact)),
            mapping.as_bytes(),
            &mut |bytes| resources.charge(bytes as u64),
        )
        .map_err(|error| match error {
            artifact::ArtifactControlError::Format(_) => WalError::Malformed,
            artifact::ArtifactControlError::Control(error) => error,
        })?;
        let payload = validate_required_block(reference, role, &frame, resources)?;
        if role == RequiredRole::Participant(ParticipantRole::Catalog) {
            let count = usize::try_from(
                payload
                    .get(104..112)
                    .and_then(|bytes| bytes.first_chunk::<8>())
                    .copied()
                    .map(u64::from_le_bytes)
                    .ok_or(WalError::Malformed)?,
            )
            .map_err(|_| WalError::Capacity)?;
            let allowance = count
                .checked_mul(std::mem::size_of::<SymbolEntry<'_>>())
                .ok_or(WalError::Capacity)?;
            let descriptor_charge = self
                .resources
                .reserve(allowance)
                .map_err(|error| self.fail(NativeGraphError::Store(error), WalError::Capacity))?;
            let image = CatalogImage::decode(payload, allowance, &mut || {
                resources.charge(1).map_err(|_| CatalogError::Cancelled)
            })
            .map_err(|error| match error {
                CatalogError::Cancelled => WalError::Cancelled,
                CatalogError::Capacity | CatalogError::Allocation => WalError::Capacity,
                _ => WalError::Participant,
            })?;
            image
                .declaration
                .validate_for(reference.object.store, self.expected, &mut || {
                    resources.charge(1).map_err(|_| CatalogError::Cancelled)
                })
                .map_err(|error| match error {
                    CatalogError::Cancelled => WalError::Cancelled,
                    _ => WalError::Participant,
                })?;
            self.catalog = Some(CatalogWaters {
                node: image.declaration.node_high_water,
                relationship: image.declaration.relationship_high_water,
                symbols: image.symbols.high_waters(),
            });
            drop(image);
            drop(descriptor_charge);
        }
        Ok(())
    }

    fn validate_state(
        &mut self,
        state: CommitState<'_>,
        resources: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        for (root, tree) in state.graph.slots.into_iter().zip([
            TreeKind::Nodes,
            TreeKind::Relationships,
            TreeKind::KeyFences,
            TreeKind::Labels,
            TreeKind::RelationshipTypes,
            TreeKind::OutRanges,
            TreeKind::InRanges,
            TreeKind::ObjectInventory,
        ]) {
            if let Some(root) = root {
                self.validate_reference(root, RequiredRole::Tree(tree), resources)?;
            }
        }
        self.validate_reference(
            state.catalog,
            RequiredRole::Participant(ParticipantRole::Catalog),
            resources,
        )?;
        for root in [state.vector, state.text].into_iter().flatten() {
            self.validate_reference(
                root,
                RequiredRole::Participant(ParticipantRole::RetrievalState),
                resources,
            )?;
        }
        if state.reclaim.is_some() {
            return Err(WalError::Participant);
        }
        for index in 0..state.prepared_inventories.len()? {
            let root = state.prepared_inventories.get(index, resources)?;
            self.validate_reference(
                root,
                RequiredRole::Participant(ParticipantRole::PreparedInventory),
                resources,
            )?;
        }
        let catalog = self.catalog.ok_or(WalError::Participant)?;
        let [label, relationship_type, property, namespace] = state.high_waters.symbols;
        if catalog.node != state.high_waters.node
            || catalog.relationship != state.high_waters.relationship
            || catalog.symbols
                != (SymbolHighWaters {
                    label,
                    relationship_type,
                    property,
                    namespace,
                })
        {
            return Err(WalError::HighWater);
        }
        Ok(())
    }

    fn validate_checkpoint_state(
        &mut self,
        state: CommitState<'_>,
        resources: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        self.validate_state(state, resources)?;
        let source = RecoverySource::new(self.store, self.directory, state, self.memory)
            .map_err(|error| self.fail(NativeGraphError::Read(error), WalError::Capacity))?;
        let result = (|| {
            let mut tree = source.resources()?;
            let catalog = RecoveryCatalog::open(
                &source,
                state.catalog,
                self.expected,
                state.high_waters,
                &mut tree,
            )?;
            let roots = GraphRoots::from_references(
                state.store,
                state.generation,
                state.graph.slots.map(|root| root.map(|value| value.block)),
            )?;
            validate_native_checkpoint(
                &source,
                &catalog,
                self.document,
                self.memory,
                roots,
                state.sequence,
                state.high_waters,
                &mut tree,
            )?;
            if state.sequence == 0 {
                if state.text.is_some() || state.vector.is_some() {
                    return Err(TreeError::Invalid("initial sparse checkpoint roots"));
                }
            } else {
                crate::property_graph::storage::search::validate_checkpoint(
                    &source,
                    crate::property_graph::storage::search::SparseCheckpoint {
                        cutoff: state.sequence,
                        roots: crate::property_graph::storage::search::SparseRoots {
                            text: state.text,
                            vector: state.vector,
                        },
                    },
                    roots,
                    state.catalog,
                    &catalog,
                    self.document,
                    self.store.tokenizer.epoch(),
                    self.memory,
                    &mut tree,
                )?;
            }
            let mut checkpoint_descriptors =
                StorageBuffer::<ArtifactDescriptor>::new(self.memory, MAX_RECOVERED_DESCRIPTORS)?;
            let inventory_count = state.prepared_inventories.len().map_err(|error| {
                self.fail(NativeGraphError::Wal(error), error);
                TreeError::Invalid("checkpoint prepared inventory list")
            })?;
            for index in 0..inventory_count {
                let required =
                    state
                        .prepared_inventories
                        .get(index, resources)
                        .map_err(|error| {
                            self.fail(NativeGraphError::Wal(error), error);
                            TreeError::Invalid("checkpoint prepared inventory reference")
                        })?;
                let inventory = prepared_inventory(&source, required, state, &mut tree)?;
                for position in 0..inventory.count {
                    tree.step(1)?;
                    let descriptor = inventory.descriptor(position)?;
                    if checkpoint_descriptors.as_slice().iter().any(|previous| {
                        previous.artifact == descriptor.artifact
                            || previous.serial == descriptor.serial
                    }) {
                        return Err(TreeError::Invalid(
                            "duplicate checkpoint prepared descriptor",
                        ));
                    }
                    source.validate_descriptor(descriptor, &mut tree)?;
                    checkpoint_descriptors.push(descriptor)?;
                }
                if checkpoint_descriptors.as_slice().iter().any(|previous| {
                    previous.artifact == required.object.artifact
                        || previous.serial == required.object.serial
                }) {
                    return Err(TreeError::Invalid(
                        "duplicate checkpoint prepared inventory",
                    ));
                }
                checkpoint_descriptors.push(required.object)?;
            }
            Ok::<(), TreeError>(())
        })();
        if let Err(error) = result {
            if let Some(source_error) = source.take_source_error() {
                return Err(self.fail(source_error, WalError::Participant));
            }
            if let Some(source_error) = self.first_error.take() {
                return Err(self.fail(source_error, WalError::Participant));
            }
            return Err(self.fail(NativeGraphError::Read(error), WalError::Participant));
        }
        self.inventory_start = self.protected.len();
        Ok(())
    }
}

impl ReplayValidator for SemanticReplay<'_, '_> {
    fn required(
        &mut self,
        reference: RequiredRef,
        role: RequiredRole,
        resources: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        self.validate_reference(reference, role, resources)
    }

    fn mutation(
        &mut self,
        mutation: crate::property_graph::wal::Mutation<'_>,
        resources: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        resources.charge(1)?;
        if mutation.live != mutation.canonical.is_some() {
            return Err(WalError::Participant);
        }
        Ok(())
    }

    fn inventory(
        &mut self,
        inventory: InventoryChange,
        resources: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        resources.charge(1)?;
        match inventory.state {
            InventoryState::Prepared | InventoryState::Retained => {
                if self.protected.len() == self.protected.capacity() {
                    return Err(WalError::Capacity);
                }
                if self
                    .protected
                    .get(self.inventory_start..)
                    .ok_or(WalError::Participant)?
                    .iter()
                    .any(|previous| {
                        previous.artifact == inventory.object.artifact
                            || previous.serial == inventory.object.serial
                    })
                {
                    return Err(WalError::Participant);
                }
                self.protected.push(inventory.object);
                Ok(())
            }
            InventoryState::ReclaimPending(_) | InventoryState::Reclaimed(_) => {
                Err(WalError::Participant)
            }
        }
    }

    fn reclaim_intent(
        &mut self,
        _: ReclaimIntent<'_>,
        _: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        Err(WalError::Participant)
    }

    fn reclaim_complete(
        &mut self,
        _: ReclaimComplete<'_>,
        _: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        Err(WalError::Participant)
    }

    fn state(
        &mut self,
        base: CommitState<'_>,
        target: CommitState<'_>,
        changes: ChangeReader<'_>,
        resources: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        let source = RecoverySource::new(self.store, self.directory, target, self.memory)
            .map_err(|error| self.fail(NativeGraphError::Read(error), WalError::Capacity))?;
        let mut replay_error = None;
        let result = (|| {
            let mut tree = source.resources()?;
            let catalog = RecoveryCatalog::open(
                &source,
                target.catalog,
                self.expected,
                target.high_waters,
                &mut tree,
            )?;
            let base_catalog = RecoveryCatalog::open(
                &source,
                base.catalog,
                self.expected,
                base.high_waters,
                &mut tree,
            )?;
            catalog.validate_retains(&base_catalog, &mut tree)?;
            let base_native = GraphRoots::from_references(
                base.store,
                base.generation,
                base.graph.slots.map(|root| root.map(|value| value.block)),
            )?;
            let target_native = GraphRoots::from_references(
                target.store,
                target.generation,
                target.graph.slots.map(|root| root.map(|value| value.block)),
            )?;
            validate_native_checkpoint(
                &source,
                &catalog,
                self.document,
                self.memory,
                target_native,
                target.sequence,
                target.high_waters,
                &mut tree,
            )?;
            let mut retained_mutations = StorageBuffer::new(self.memory, MAX_GRAPH_CHANGES)?;
            let mut semantic_changes = changes;
            while let Some(change) = semantic_changes.next_change(resources).map_err(|error| {
                if replay_error.is_none() {
                    replay_error = Some(error);
                }
                TreeError::Invalid("invalid recovery mutation stream")
            })? {
                match change {
                    Change::Mutation(mutation) => retained_mutations.push(mutation)?,
                    Change::Inventory(_) => {}
                    Change::ReclaimIntent(_) | Change::ReclaimComplete(_) => {
                        return Err(TreeError::Invalid("unsupported recovery reclaim proof"));
                    }
                }
            }
            for (index, mutation) in retained_mutations.as_slice().iter().enumerate() {
                tree.step(1)?;
                for previous in retained_mutations
                    .as_slice()
                    .get(..index)
                    .ok_or(TreeError::Invalid("recovery mutation prefix"))?
                {
                    tree.step(1)?;
                    let duplicate_key = mutation
                        .provenance
                        .key
                        .zip(previous.provenance.key)
                        .is_some_and(|(left, right)| left == right);
                    if mutation.provenance.incarnation == previous.provenance.incarnation
                        || duplicate_key
                    {
                        return Err(TreeError::Invalid("duplicate recovery mutation target"));
                    }
                }
                validate_mutation_state(
                    &source,
                    &catalog,
                    self.memory,
                    self.document,
                    base_native,
                    target_native,
                    base.high_waters,
                    base.sequence,
                    target.sequence,
                    *mutation,
                    &mut tree,
                )?;
            }
            reconcile_entity_directory(
                &source,
                &catalog,
                self.document,
                base_native,
                target_native,
                TreeKind::Nodes,
                retained_mutations.as_slice(),
                base.high_waters.node,
                &mut tree,
            )?;
            reconcile_entity_directory(
                &source,
                &catalog,
                self.document,
                base_native,
                target_native,
                TreeKind::Relationships,
                retained_mutations.as_slice(),
                base.high_waters.relationship,
                &mut tree,
            )?;
            reconcile_fence_directory(
                &source,
                &catalog,
                self.document,
                base_native,
                target_native,
                retained_mutations.as_slice(),
                &mut tree,
            )?;
            let base_count = base.prepared_inventories.len().map_err(|error| {
                if replay_error.is_none() {
                    replay_error = Some(error);
                }
                TreeError::Invalid("invalid base prepared inventory list")
            })?;
            let target_count = target.prepared_inventories.len().map_err(|error| {
                if replay_error.is_none() {
                    replay_error = Some(error);
                }
                TreeError::Invalid("invalid target prepared inventory list")
            })?;
            if target_count < base_count || target_count - base_count > 1 {
                return Err(TreeError::Invalid("prepared inventory history order"));
            }
            for index in 0..base_count {
                let old = base
                    .prepared_inventories
                    .get(index, resources)
                    .map_err(|error| {
                        if replay_error.is_none() {
                            replay_error = Some(error);
                        }
                        TreeError::Invalid("invalid base prepared inventory reference")
                    })?;
                let retained =
                    target
                        .prepared_inventories
                        .get(index, resources)
                        .map_err(|error| {
                            if replay_error.is_none() {
                                replay_error = Some(error);
                            }
                            TreeError::Invalid("invalid retained prepared inventory reference")
                        })?;
                if old != retained {
                    return Err(TreeError::Invalid("prepared inventory history changed"));
                }
            }
            for index in 0..target_count {
                let required =
                    target
                        .prepared_inventories
                        .get(index, resources)
                        .map_err(|error| {
                            if replay_error.is_none() {
                                replay_error = Some(error);
                            }
                            TreeError::Invalid("invalid prepared inventory reference")
                        })?;
                let inventory = prepared_inventory(&source, required, target, &mut tree)?;
                if index == target_count.saturating_sub(1) && target_count > base_count {
                    let current = self
                        .protected
                        .get(self.inventory_start..)
                        .ok_or(TreeError::Invalid("prepared inventory change range"))?;
                    if current.len() != inventory.count + 1
                        || current.last().copied() != Some(required.object)
                    {
                        return Err(TreeError::Invalid("prepared inventory envelope coverage"));
                    }
                    for (position, descriptor) in current
                        .get(..inventory.count)
                        .ok_or(TreeError::Invalid("prepared inventory coverage extent"))?
                        .iter()
                        .enumerate()
                    {
                        if inventory.descriptor(position)? != *descriptor {
                            return Err(TreeError::Invalid(
                                "prepared inventory envelope descriptor",
                            ));
                        }
                    }
                }
            }
            if target_count == base_count && self.protected.len() != self.inventory_start {
                return Err(TreeError::Invalid("unrooted prepared inventory changes"));
            }
            crate::property_graph::storage::search::validate_persisted_replay_transition(
                &source,
                crate::property_graph::storage::search::SparseCheckpoint {
                    cutoff: base.sequence,
                    roots: crate::property_graph::storage::search::SparseRoots {
                        text: base.text,
                        vector: base.vector,
                    },
                },
                base_native,
                crate::property_graph::storage::search::SparseRoots {
                    text: target.text,
                    vector: target.vector,
                },
                target_native,
                target.catalog,
                &catalog,
                self.document,
                self.store.tokenizer.epoch(),
                changes,
                resources,
                &mut replay_error,
                self.memory,
                &mut tree,
            )?;
            Ok::<(), TreeError>(())
        })();
        if let Err(error) = result {
            if let Some(error) = replay_error {
                return Err(error);
            }
            let source = source
                .take_source_error()
                .unwrap_or(NativeGraphError::Read(error));
            return Err(self.fail(source, WalError::Participant));
        }
        self.inventory_start = self.protected.len();
        self.validate_state(target, resources)
    }
}

pub(super) fn open(
    path: &Path,
    options: OpenOptions,
    document: Option<EmbeddingTower>,
    vfs: Arc<dyn Vfs>,
    clock: Arc<dyn MonotonicClock>,
) -> Result<Store, NativeGraphError> {
    let writable = options.access_mode == AccessMode::ReadWrite;
    let store = Store::new_native_graph_recovery_owner(path, options, vfs, clock)?;
    let shared = GraphResources::from_store(&store)?;
    let root_envelope = selected_root(&store, path)?;
    let root_path = crate::property_graph::storage::allocation::artifact_path(
        path,
        root_envelope.object.artifact,
    );
    let root_mapping = map_file(&store, &root_path, MAX_ARTIFACT_BYTES)?;
    let descriptor = artifact_descriptor(
        ArtifactIdentity {
            store: root_envelope.object.store,
            artifact: root_envelope.object.artifact,
            generation: root_envelope.object.generation,
            creation_serial: root_envelope.object.serial,
        },
        ContainerKind::RootEnvelope,
        root_mapping.as_bytes(),
    )?;
    if descriptor != root_envelope.object {
        return Err(NativeGraphError::Invalid(
            "root checkpoint descriptor mismatch",
        ));
    }
    let root_frame = artifact::decode(
        ContainerKind::RootEnvelope,
        Some((root_envelope.object.store, root_envelope.object.artifact)),
        root_mapping.as_bytes(),
    )
    .map_err(|_| NativeGraphError::Invalid("corrupt native graph root checkpoint"))?;
    let payload = root_frame
        .framed_block(root_envelope.block)
        .map_err(|_| NativeGraphError::Invalid("root checkpoint block mismatch"))?
        .payload();
    let control = QueryControl::Cancel(CancelToken::new());
    let mut cancelled = || control.checkpoint().is_err();
    let mut resources = WalResources::new(
        u64::try_from(payload.len())
            .ok()
            .and_then(|bytes| bytes.checked_mul(64))
            .unwrap_or(u64::MAX),
        STACK_RESERVATION_BYTES,
        &mut cancelled,
    )?;
    let checkpoint = decode_checkpoint(payload, &mut resources)?;
    if checkpoint.state.store != root_envelope.object.store
        || checkpoint.state.generation != root_envelope.object.generation
    {
        return Err(NativeGraphError::Invalid(
            "incoherent native graph checkpoint",
        ));
    }
    let wal_path = path.join(format!("graph-wal-{:032x}.ze", checkpoint.wal_identity));
    let wal_mapping = map_file(
        &store,
        &wal_path,
        crate::property_graph::wal::HEADER_BYTES + MAX_ENVELOPE_BYTES,
    )?;
    let mut cancelled = || control.checkpoint().is_err();
    let replay_work = u64::try_from(MAX_RECOVERED_DESCRIPTORS)
        .ok()
        .and_then(|count| count.checked_mul(MAX_ARTIFACT_BYTES as u64))
        .and_then(|bytes| bytes.checked_mul(8))
        .and_then(|object_work| {
            u64::try_from(wal_mapping.as_bytes().len())
                .ok()
                .and_then(|bytes| bytes.checked_mul(128))
                .and_then(|wal_work| object_work.checked_add(wal_work))
        })
        .ok_or(NativeGraphError::Invalid("WAL recovery work bound"))?;
    let mut resources = WalResources::new(replay_work, STACK_RESERVATION_BYTES, &mut cancelled)?;
    let first_sequence = Replay::checked_first_sequence(
        wal_mapping.as_bytes(),
        checkpoint.state.store,
        &mut resources,
    )?;
    if first_sequence != checkpoint.first_sequence {
        return Err(NativeGraphError::Wal(WalError::Sequence));
    }
    let expected = GraphInterpretation::new(store.tokenizer.epoch(), document.as_ref())?;
    let write_memory = WriteMemory::new(&shared, WriteLimits::default())?;
    let storage = StorageMemory::new(&write_memory, &control, 32 * 1024 * 1024)?;
    let mut validator =
        SemanticReplay::new(&store, path, expected, document.as_ref(), &shared, &storage)?;
    if let Err(error) = validator.validate_checkpoint_state(checkpoint.state, &mut resources) {
        if let Some(source) = validator.first_error.take() {
            return Err(source);
        }
        return Err(NativeGraphError::Wal(error));
    }
    let watermark = Replay::checked_checkpoint_watermark(
        wal_mapping.as_bytes(),
        checkpoint.state,
        &mut resources,
    )?;
    let mut replay = Replay::at_watermark(
        wal_mapping.as_bytes(),
        checkpoint.state,
        watermark,
        &mut resources,
    )?;
    let mut final_state = checkpoint.state;
    let mut envelope_count = 0_u64;
    let end = loop {
        match replay.next_envelope(&mut validator, &mut resources) {
            Ok(ReplayStep::Envelope(envelope)) => {
                final_state = envelope.state;
                envelope_count = envelope_count
                    .checked_add(1)
                    .ok_or(NativeGraphError::IdentityExhausted)?;
            }
            Ok(ReplayStep::End(end)) => break end,
            Err(error) => {
                if let Some(source) = validator.first_error.take() {
                    return Err(source);
                }
                return Err(NativeGraphError::Wal(error));
            }
        }
    };
    let SemanticReplay {
        protected,
        _charge: protected_charge,
        ..
    } = validator;
    let prepared_count = final_state.prepared_inventories.len()?;
    if prepared_count > MAX_RECOVERED_DESCRIPTORS {
        return Err(NativeGraphError::Invalid(
            "prepared inventory recovery capacity",
        ));
    }
    let prepared_charge = shared.reserve(
        prepared_count
            .checked_mul(std::mem::size_of::<RequiredRef>())
            .ok_or(NativeGraphError::Invalid(
                "prepared inventory recovery capacity",
            ))?,
    )?;
    let mut prepared_inventories = Vec::new();
    prepared_inventories
        .try_reserve_exact(prepared_count)
        .map_err(|_| NativeGraphError::Invalid("prepared inventory recovery allocation"))?;
    for index in 0..prepared_count {
        prepared_inventories.push(
            final_state
                .prepared_inventories
                .get(index, &mut resources)?,
        );
    }
    let roots = GraphRoots::from_references(
        final_state.store,
        final_state.generation,
        final_state
            .graph
            .slots
            .map(|root| root.map(|value| value.block)),
    )?;
    let serial_fence = if writable {
        scan_creation_serials(
            &store,
            &shared,
            path,
            final_state.store,
            final_state.generation,
            final_state.high_waters.creation_serial,
            &control,
        )?
    } else {
        final_state.high_waters.creation_serial
    };
    let bundle = super::NativeGraphBundle::install_recovered(
        &store,
        &shared,
        NativeGraphBundleInput {
            base: BaseIdentity {
                store: final_state.store,
                generation: final_state.generation,
                roots: Some(root_envelope.object.artifact),
            },
            root_envelope,
            roots,
            wal_roots: final_state.graph,
            sequence: final_state.sequence,
            catalog: final_state.catalog,
            vector: final_state.vector,
            text: final_state.text,
            reclaim: final_state.reclaim,
            high_waters: final_state.high_waters,
            prepared_inventories,
            lexical: store.tokenizer.epoch(),
            document,
        },
    )?;
    drop(prepared_charge);
    store.native_graph.install(bundle)?;
    if writable {
        let handle = store
            .vfs
            .open_append(&wal_path)
            .map_err(|source| NativeGraphError::Io {
                path: wal_path.clone(),
                source,
            })?;
        let writer = super::write::NativeWriter::resume(
            NativeWal {
                handle,
                path: wal_path,
                identity: checkpoint.wal_identity,
                first_sequence: checkpoint.first_sequence,
                bytes: end.complete_bytes,
            },
            &shared,
            envelope_count,
            &protected,
        )?;
        store.native_graph.initialize_writer(writer, serial_fence)?;
        if end.incomplete_tail {
            store.checkpoint_native_graph(&control)?;
        }
    } else {
        store.native_graph.mark_read_only();
    }
    drop(protected);
    drop(protected_charge);
    Ok(store)
}
