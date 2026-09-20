//! Storage-backed source and catalog for one admitted native preparation.

use super::mapping::NativeReadonlyMapping;
use crate::lifecycle::native_graph::NativeReadLease;
use crate::property_graph::catalog::{
    CatalogError, CatalogImage, GraphInterpretation, Symbol, SymbolEntry, SymbolHighWaters,
    SymbolKind,
};
use crate::property_graph::query::runtime::RuntimeError;
use crate::property_graph::staging::BaseIdentity;
use crate::property_graph::storage::artifact::{
    self, ArtifactControlError, ArtifactId, BlockKind, ContainerKind, FramedBlock, PhysicalRef,
    ValidatedArtifact,
};
use crate::property_graph::storage::memory::{StorageBuffer, StorageMemory, StorageReservation};
use crate::property_graph::storage::participant::PreparationCatalog;
use crate::property_graph::storage::records::RecordCatalog;
use crate::property_graph::storage::stream::PayloadSlice;
use crate::property_graph::storage::tree::directory::{BlockSource, TreeError, TreeResources};
use std::{cell::OnceCell, ffi::OsString, fmt::Write as _, path::PathBuf};

struct PreparationMappedArtifact {
    artifact: ArtifactId,
    mapping: NativeReadonlyMapping,
    validation: ValidatedArtifact,
}

fn charged_artifact_path<'m>(
    memory: &'m StorageMemory<'m>,
    directory: &std::path::Path,
    artifact: ArtifactId,
) -> Result<(PathBuf, StorageReservation<'m>), TreeError> {
    const FILENAME_BYTES: usize = b"graph-00000000000000000000000000000000.zgraph".len();
    let path_upper = directory
        .as_os_str()
        .len()
        .checked_add(1)
        .and_then(|bytes| bytes.checked_add(FILENAME_BYTES))
        .ok_or(TreeError::Memory)?;
    let mut charge = memory.reserve(
        path_upper
            .checked_add(FILENAME_BYTES)
            .ok_or(TreeError::Memory)?,
    )?;
    let mut filename = String::new();
    filename
        .try_reserve_exact(FILENAME_BYTES)
        .map_err(|_| TreeError::Memory)?;
    write!(&mut filename, "graph-{:032x}.zgraph", artifact.get()).map_err(|_| TreeError::Memory)?;
    let mut raw = OsString::with_capacity(path_upper);
    raw.push(directory.as_os_str());
    let mut path = PathBuf::from(raw);
    path.push(&filename);
    let raw = path.into_os_string();
    let path_capacity = raw.capacity();
    charge.resize(
        filename
            .capacity()
            .checked_add(path_capacity)
            .ok_or(TreeError::Memory)?,
    )?;
    drop(filename);
    charge.resize(path_capacity)?;
    Ok((PathBuf::from(raw), charge))
}

/// Immutable artifact source bound to one active lease and the same store-owned
/// storage allowance used by preparation.
pub(crate) struct NativePreparationSource<'lease, 'm> {
    lease: &'lease NativeReadLease,
    memory: &'m StorageMemory<'m>,
    slots: StorageBuffer<'m, OnceCell<PreparationMappedArtifact>>,
}

impl<'lease, 'm> NativePreparationSource<'lease, 'm> {
    pub(crate) fn new(
        lease: &'lease NativeReadLease,
        memory: &'m StorageMemory<'m>,
        capacity: usize,
    ) -> Result<Self, TreeError> {
        lease
            .check_active()
            .map_err(RuntimeError::Value)
            .map_err(TreeError::Runtime)?;
        if capacity == 0 || !lease.belongs_to(memory.resources()) {
            return Err(TreeError::Invalid(
                "native preparation source accounting owner mismatch",
            ));
        }
        let mut slots = StorageBuffer::new(memory, capacity)?;
        for _ in 0..capacity {
            slots.push(OnceCell::new())?;
        }
        Ok(Self {
            lease,
            memory,
            slots,
        })
    }

    pub(crate) fn resources(&self, work_limit: u64) -> Result<TreeResources<'m>, TreeError> {
        TreeResources::for_prepare(self.memory, work_limit)
    }

    pub(crate) const fn lease(&self) -> &NativeReadLease {
        self.lease
    }

    pub(crate) const fn memory(&self) -> &'m StorageMemory<'m> {
        self.memory
    }

    fn check_owner(&self, resources: &mut TreeResources<'_>) -> Result<(), TreeError> {
        self.lease
            .check_active()
            .map_err(RuntimeError::Value)
            .map_err(TreeError::Runtime)?;
        resources.require_preparation(self.memory)?;
        resources.step(0)
    }

    fn decode<'a>(
        &'a self,
        mapped: &'a PreparationMappedArtifact,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'a>, TreeError> {
        resources.step(1)?;
        let block = mapped
            .validation
            .framed_block(mapped.mapping.as_bytes(), reference)
            .map_err(TreeError::Format)?;
        self.check_required(block)
    }

    fn admit_mapping(
        &self,
        mapping: &NativeReadonlyMapping,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<ValidatedArtifact, TreeError> {
        let bundle = self.lease.bundle();
        let frame = artifact::decode_with_control(
            ContainerKind::Object,
            Some((bundle.base().store, reference.artifact)),
            mapping.as_bytes(),
            &mut |bytes| resources.step(bytes as u64),
        )
        .map_err(|error| match error {
            ArtifactControlError::Format(error) => TreeError::Format(error),
            ArtifactControlError::Control(error) => error,
        })?;
        let identity = frame.identity();
        if identity.generation > bundle.base().generation
            || identity.creation_serial > bundle.high_waters().creation_serial
        {
            return Err(TreeError::Invalid(
                "native preparation artifact is newer than admitted cutoff",
            ));
        }
        let block = frame.framed_block(reference).map_err(TreeError::Format)?;
        let validation = frame.validation();
        let _ = self.check_required(block)?;
        Ok(validation)
    }

    fn check_required<'a>(&self, block: FramedBlock<'a>) -> Result<FramedBlock<'a>, TreeError> {
        let bundle = self.lease.bundle();
        if let Some(required) = bundle.required_object(block.reference()) {
            let expected = required.object;
            let identity = block.identity();
            if identity.store != expected.store
                || identity.artifact != expected.artifact
                || identity.generation != expected.generation
                || identity.creation_serial != expected.serial
                || block.file_length() != expected.bytes as usize
                || block.file_checksum() != expected.checksum
            {
                return Err(TreeError::Invalid(
                    "native preparation required descriptor mismatch",
                ));
            }
        }
        Ok(block)
    }
}

impl BlockSource for NativePreparationSource<'_, '_> {
    fn resolve<'a>(
        &'a self,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'a>, TreeError> {
        self.check_owner(resources)?;
        for cell in self.slots.as_slice() {
            if let Some(mapped) = cell.get()
                && mapped.artifact == reference.artifact
            {
                return self.decode(mapped, reference, resources);
            }
        }
        let cell = self
            .slots
            .as_slice()
            .iter()
            .find(|cell| cell.get().is_none())
            .ok_or(TreeError::Memory)?;
        let (path, path_charge) = charged_artifact_path(
            self.memory,
            self.lease.bundle().directory(),
            reference.artifact,
        )?;
        let file = self
            .lease
            .bundle()
            .vfs()
            .open_for_map(&path)
            .map_err(TreeError::Io)?;
        let mapping = NativeReadonlyMapping::open(file, &path, self.lease)?;
        drop(path);
        drop(path_charge);
        let validation = self.admit_mapping(&mapping, reference, resources)?;
        let mapped = PreparationMappedArtifact {
            artifact: reference.artifact,
            mapping,
            validation,
        };
        cell.set(mapped)
            .map_err(|_| TreeError::Invalid("native preparation source slot initialized twice"))?;
        let mapped = cell.get().ok_or(TreeError::Invalid(
            "native preparation source slot remained empty",
        ))?;
        self.decode(mapped, reference, resources)
    }
}

/// Decoded base catalog whose bytes and admission are owned by one preparation
/// source and whose descriptor allocation is charged to the same StorageMemory.
pub(crate) struct NativePreparationCatalog<'source, 'lease, 'm> {
    image: CatalogImage<'source>,
    base: BaseIdentity,
    source: &'source NativePreparationSource<'lease, 'm>,
    _descriptors: StorageReservation<'m>,
}

impl<'source, 'lease, 'm> NativePreparationCatalog<'source, 'lease, 'm> {
    pub(crate) fn open(
        source: &'source NativePreparationSource<'lease, 'm>,
        resources: &mut TreeResources<'_>,
    ) -> Result<Self, TreeError> {
        let bundle = source.lease().bundle();
        let required = bundle.catalog();
        let block = source.resolve(required.block, resources)?;
        let identity = block.identity();
        if identity.store != required.object.store
            || identity.artifact != required.object.artifact
            || identity.generation != required.object.generation
            || identity.creation_serial != required.object.serial
            || block.reference() != required.block
            || block.file_length() != required.object.bytes as usize
            || block.file_checksum() != required.object.checksum
            || block.reference().kind != BlockKind::CommitParticipant
        {
            return Err(TreeError::Invalid("catalog required descriptor mismatch"));
        }
        let payload = block.payload();
        let header = payload
            .get(..8)
            .ok_or(TreeError::Invalid("catalog participant header"))?;
        if header.get(..4) != Some(b"ZGCP".as_slice())
            || u16::from_le_bytes(
                *header
                    .get(4..6)
                    .and_then(|bytes| bytes.first_chunk::<2>())
                    .ok_or(TreeError::Invalid("catalog participant role"))?,
            ) != 1
            || u16::from_le_bytes(
                *header
                    .get(6..8)
                    .and_then(|bytes| bytes.first_chunk::<2>())
                    .ok_or(TreeError::Invalid("catalog participant version"))?,
            ) != 1
        {
            return Err(TreeError::Invalid("catalog participant role or version"));
        }
        let encoded = payload
            .get(8..)
            .ok_or(TreeError::Invalid("catalog participant payload"))?;
        let count = usize::try_from(u64::from_le_bytes(
            *encoded
                .get(104..112)
                .and_then(|bytes| bytes.first_chunk::<8>())
                .ok_or(TreeError::Invalid("catalog symbol count"))?,
        ))
        .map_err(|_| TreeError::Memory)?;
        let allowance = count
            .checked_mul(std::mem::size_of::<SymbolEntry<'_>>())
            .ok_or(TreeError::Memory)?;
        let mut descriptors = source.memory().reserve(allowance)?;
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
                .unwrap_or(TreeError::Invalid("catalog cancelled")),
            CatalogError::Capacity | CatalogError::Allocation => TreeError::Memory,
            _ => TreeError::Invalid("invalid native graph catalog"),
        })?;
        descriptors.resize(image.symbols.allocated_bytes())?;
        let expected = GraphInterpretation::new(bundle.lexical(), bundle.document())
            .map_err(|_| TreeError::Invalid("invalid admitted catalog interpretation"))?;
        let mut callback_error = None;
        image
            .declaration
            .validate_for(bundle.base().store, expected, &mut || {
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
                    .unwrap_or(TreeError::Invalid("catalog cancelled")),
                CatalogError::Capacity | CatalogError::Allocation => TreeError::Memory,
                _ => TreeError::Invalid("invalid native graph catalog"),
            })?;
        let high = bundle.high_waters();
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
            return Err(TreeError::Invalid("catalog allocator high-water mismatch"));
        }
        Ok(Self {
            image,
            base: bundle.base(),
            source,
            _descriptors: descriptors,
        })
    }

    pub(crate) fn owns(&self, source: &NativePreparationSource<'_, '_>) -> bool {
        std::ptr::eq(self.source, source)
    }
}

fn catalog_error(error: CatalogError, resources: &mut TreeResources<'_>) -> TreeError {
    if error == CatalogError::Cancelled {
        resources
            .step(0)
            .err()
            .unwrap_or(TreeError::Invalid("catalog cancelled"))
    } else {
        TreeError::Invalid("invalid native graph catalog")
    }
}

impl<S: BlockSource> RecordCatalog<S> for NativePreparationCatalog<'_, '_, '_> {
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
                && name.compare_bytes(entry.name.as_str().as_bytes(), resources)?
                    == std::cmp::Ordering::Equal
            {
                return Ok(entry.symbol);
            }
        }
        Err(TreeError::Invalid(
            "record name is absent from admitted catalog",
        ))
    }
}

impl<S: BlockSource> PreparationCatalog<S> for NativePreparationCatalog<'_, '_, '_> {
    fn base_identity(&self) -> BaseIdentity {
        self.base
    }
}
