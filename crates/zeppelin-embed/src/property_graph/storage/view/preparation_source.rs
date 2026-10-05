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
use crate::property_graph::storage::tree::directory::{
    BlockSource, RESERVED_PINNED_SLOTS, TreeError, TreeResources,
};
use std::{
    cell::{Cell, OnceCell, RefCell},
    ffi::OsString,
    fmt::Write as _,
    path::PathBuf,
};

struct PreparationMappedArtifact {
    artifact: ArtifactId,
    mapping: NativeReadonlyMapping,
    validation: ValidatedArtifact,
}

/// One authenticated artifact mapping retained only for a caller-owned bounded
/// semantic window. Nested reads in the same artifact reuse its completed
/// framing proof; reads in another artifact remain scoped through the parent.
pub(crate) struct NativeArtifactWindow<'source, 'lease, 'm> {
    source: &'source NativePreparationSource<'lease, 'm>,
    artifact: ArtifactId,
    mapping: NativeReadonlyMapping,
    validation: ValidatedArtifact,
}

pub(crate) fn charged_artifact_path<'m>(
    memory: &'m StorageMemory<'m>,
    directory: &std::path::Path,
    artifact: ArtifactId,
) -> Result<(PathBuf, StorageReservation<'m>), TreeError> {
    charged_container_path(memory, directory, artifact, false)
}
fn charged_container_path<'m>(
    memory: &'m StorageMemory<'m>,
    directory: &std::path::Path,
    artifact: ArtifactId,
    wal: bool,
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
    if wal {
        write!(&mut filename, "graph-wal-{:032x}.ze", artifact.get())
    } else {
        write!(&mut filename, "graph-{:032x}.zgraph", artifact.get())
    }
    .map_err(|_| TreeError::Memory)?;
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
    scoped: Cell<bool>,
    /// At most one authenticated mapping retained for the current scoped
    /// traversal. Every read of one leaf entry names the same object, so this
    /// keeps a scoped traversal's open/authenticate cost at parity with the
    /// retained slot table without retaining one mapping per artifact.
    window: RefCell<Option<PreparationMappedArtifact>>,
    retain_window: Cell<bool>,
    filled: Cell<usize>,
}

impl<'lease, 'm> NativePreparationSource<'lease, 'm> {
    pub(crate) fn charged_path(
        memory: &'m StorageMemory<'m>,
        directory: &std::path::Path,
        artifact: ArtifactId,
    ) -> Result<(PathBuf, StorageReservation<'m>), TreeError> {
        charged_artifact_path(memory, directory, artifact)
    }

    pub(crate) fn charged_candidate_path(
        memory: &'m StorageMemory<'m>,
        directory: &std::path::Path,
        descriptor: crate::property_graph::wal::ArtifactDescriptor,
    ) -> Result<(PathBuf, StorageReservation<'m>), TreeError> {
        charged_container_path(
            memory,
            directory,
            descriptor.artifact,
            descriptor.family == 19,
        )
    }

    pub(crate) fn wal_first_sequence(
        header: &[u8],
        store: crate::property_graph::StoreInstanceId,
    ) -> Result<u64, TreeError> {
        let mut cancelled = || false;
        let mut resources = crate::property_graph::wal::WalResources::new(
            1024,
            crate::property_graph::wal::STACK_RESERVATION_BYTES,
            &mut cancelled,
        )
        .map_err(|_| TreeError::Memory)?;
        crate::property_graph::wal::Replay::checked_first_sequence(header, store, &mut resources)
            .map_err(|_| TreeError::Invalid("reclaim WAL header"))
    }

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
            scoped: Cell::new(false),
            window: RefCell::new(None),
            retain_window: Cell::new(false),
            filled: Cell::new(0),
        })
    }

    /// Construct the authenticated trace variant. Its scoped callbacks release
    /// each path, file and mapping before returning an owned result.
    pub(crate) fn new_scoped(
        lease: &'lease NativeReadLease,
        memory: &'m StorageMemory<'m>,
        capacity: usize,
    ) -> Result<Self, TreeError> {
        let source = Self::new(lease, memory, capacity)?;
        source.scoped.set(true);
        Ok(source)
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

    fn decode<'b>(
        &self,
        mapped: &'b PreparationMappedArtifact,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'b>, TreeError> {
        resources.step(1)?;
        let block = mapped
            .validation
            .framed_block(mapped.mapping.as_bytes(), reference)
            .map_err(TreeError::Format)?;
        let block = self.check_required(block)?;
        #[cfg(any(test, feature = "test-support"))]
        crate::property_graph::storage::search::observe_native_vector_physical_read(
            crate::property_graph::storage::search::PhysicalReadOrigin::Preparation,
            reference,
        );
        Ok(block)
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

    /// Authenticate one immutable artifact once, retain it for one bounded
    /// callback, then release its mapping before returning an owned result.
    pub(crate) fn with_artifact_window<R>(
        &self,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
        callback: impl for<'a, 'r> FnOnce(
            &'a NativeArtifactWindow<'_, 'lease, 'm>,
            &'r mut TreeResources<'_>,
        ) -> Result<R, TreeError>,
    ) -> Result<R, TreeError> {
        self.check_owner(resources)?;
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
        let validation = self.admit_mapping(&mapping, reference, resources)?;
        let window = NativeArtifactWindow {
            source: self,
            artifact: reference.artifact,
            mapping,
            validation,
        };
        let result = callback(&window, resources);
        drop(window);
        drop(path);
        drop(path_charge);
        result
    }

    /// Copies one exact maintenance spill payload into caller-owned charged
    /// scratch and releases its path, file and mapping before returning.
    pub(crate) fn copy_spill_page(
        &self,
        required: crate::property_graph::wal::RequiredRef,
        target_generation: crate::property_graph::GraphGeneration,
        output: &mut [u8],
        resources: &mut TreeResources<'_>,
    ) -> Result<usize, TreeError> {
        self.check_owner(resources)?;
        let bundle = self.lease.bundle();
        if required.object.store != bundle.base().store
            || required.object.generation != target_generation
            || required.object.family != crate::format::FormatFamily::NativeGraphObject.id()
            || required.object.version != 1
            || required.object.artifact != required.block.artifact
            || required.block.kind != BlockKind::CommitParticipant
            || required.block.version != 1
        {
            return Err(TreeError::Invalid("maintenance spill required reference"));
        }
        let (path, path_charge) =
            charged_artifact_path(self.memory, bundle.directory(), required.object.artifact)?;
        let file = bundle.vfs().open_for_map(&path).map_err(TreeError::Io)?;
        let mapping = NativeReadonlyMapping::open(file, &path, self.lease)?;
        let frame = artifact::decode_with_control(
            ContainerKind::Object,
            Some((required.object.store, required.object.artifact)),
            mapping.as_bytes(),
            &mut |bytes| resources.step(bytes as u64),
        )
        .map_err(|error| match error {
            ArtifactControlError::Format(error) => TreeError::Format(error),
            ArtifactControlError::Control(error) => error,
        })?;
        let identity = frame.identity();
        let block = frame
            .framed_block(required.block)
            .map_err(TreeError::Format)?;
        if identity.store != required.object.store
            || identity.artifact != required.object.artifact
            || identity.generation != required.object.generation
            || identity.creation_serial != required.object.serial
            || block.file_length() != required.object.bytes as usize
            || block.file_checksum() != required.object.checksum
            || block.reference() != required.block
        {
            return Err(TreeError::Invalid("maintenance spill descriptor mismatch"));
        }
        let payload = block.payload();
        let target = output
            .get_mut(..payload.len())
            .ok_or(TreeError::Invalid("maintenance spill page exceeds scratch"))?;
        resources.step(payload.len() as u64)?;
        target.copy_from_slice(payload);
        let length = payload.len();
        drop(mapping);
        drop(path);
        drop(path_charge);
        Ok(length)
    }

    /// Revalidates one complete inventory descriptor through a scoped mapping.
    /// The path, file and mapping are released before returning.
    pub(crate) fn validate_object_descriptor(
        &self,
        descriptor: crate::property_graph::wal::ArtifactDescriptor,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        self.check_owner(resources)?;
        let bundle = self.lease.bundle();
        if descriptor.store != bundle.base().store
            || !matches!(descriptor.family, 17..=19)
            || descriptor.version != 1
        {
            return Err(TreeError::Invalid("inventory object descriptor domain"));
        }
        let (path, path_charge) =
            Self::charged_candidate_path(self.memory, bundle.directory(), descriptor)?;
        if descriptor.family == 19 {
            let length = bundle.vfs().open(&path).map_err(TreeError::Io)?;
            if length != u64::from(descriptor.bytes) {
                return Err(TreeError::Invalid("reclaim WAL length"));
            }
            let header = bundle
                .vfs()
                .read_range(&path, 0, crate::property_graph::wal::HEADER_BYTES)
                .map_err(TreeError::Io)?;
            Self::wal_first_sequence(&header, descriptor.store)?;
            let _charge = self.memory.reserve(64 * 1024)?;
            let mut digest = xxhash_rust::xxh3::Xxh3::new();
            let mut offset = 0;
            while offset < length {
                let take = (length - offset).min(64 * 1024) as usize;
                resources.step(take as u64)?;
                let bytes = bundle
                    .vfs()
                    .read_range(&path, offset, take)
                    .map_err(TreeError::Io)?;
                if bytes.len() != take {
                    return Err(TreeError::Invalid("reclaim WAL short read"));
                }
                digest.update(&bytes);
                offset += take as u64;
            }
            if digest.digest() != descriptor.checksum {
                return Err(TreeError::Invalid("reclaim WAL digest"));
            }
            return Ok(());
        }
        let file = bundle.vfs().open_for_map(&path).map_err(TreeError::Io)?;
        let mapping = NativeReadonlyMapping::open(file, &path, self.lease)?;
        let frame = artifact::decode_with_control(
            if descriptor.family == 18 {
                ContainerKind::RootEnvelope
            } else {
                ContainerKind::Object
            },
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
                .ok_or(TreeError::Invalid("inventory object checksum"))?,
        );
        if identity.store != descriptor.store
            || identity.artifact != descriptor.artifact
            || identity.generation != descriptor.generation
            || identity.creation_serial != descriptor.serial
            || mapping.as_bytes().len() != descriptor.bytes as usize
            || checksum != descriptor.checksum
        {
            return Err(TreeError::Invalid("inventory object descriptor mismatch"));
        }
        drop(mapping);
        drop(path);
        drop(path_charge);
        Ok(())
    }

    pub(crate) fn validate_required_reference(
        &self,
        required: crate::property_graph::wal::RequiredRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<(), TreeError> {
        self.check_owner(resources)?;
        let bundle = self.lease.bundle();
        let kind = if required.object.family == crate::format::FormatFamily::NativeGraphObject.id()
        {
            ContainerKind::Object
        } else if required.object.family == crate::format::FormatFamily::NativeGraphRoot.id() {
            ContainerKind::RootEnvelope
        } else {
            return Err(TreeError::Invalid("required reference container family"));
        };
        if required.object.store != bundle.base().store
            || required.object.generation > bundle.base().generation
            || required.object.serial == 0
            || required.object.serial > bundle.high_waters().creation_serial
            || required.object.artifact != required.block.artifact
            || required.object.version != 1
        {
            return Err(TreeError::Invalid("required reference descriptor domain"));
        }
        let (path, path_charge) =
            charged_artifact_path(self.memory, bundle.directory(), required.object.artifact)?;
        let file = bundle.vfs().open_for_map(&path).map_err(TreeError::Io)?;
        let mapping = NativeReadonlyMapping::open(file, &path, self.lease)?;
        let frame = artifact::decode_with_control(
            kind,
            Some((required.object.store, required.object.artifact)),
            mapping.as_bytes(),
            &mut |bytes| resources.step(bytes as u64),
        )
        .map_err(|error| match error {
            ArtifactControlError::Format(error) => TreeError::Format(error),
            ArtifactControlError::Control(error) => error,
        })?;
        let block = frame
            .framed_block(required.block)
            .map_err(TreeError::Format)?;
        let identity = frame.identity();
        if identity.store != required.object.store
            || identity.artifact != required.object.artifact
            || identity.generation != required.object.generation
            || identity.creation_serial != required.object.serial
            || mapping.as_bytes().len() != required.object.bytes as usize
            || block.file_checksum() != required.object.checksum
            || block.reference() != required.block
        {
            return Err(TreeError::Invalid("required reference descriptor mismatch"));
        }
        drop(mapping);
        drop(path);
        drop(path_charge);
        Ok(())
    }
}

impl NativePreparationSource<'_, '_> {
    /// Open and authenticate one immutable artifact, releasing its path charge
    /// and file handle before the mapping is returned.
    fn open_mapping(
        &self,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<PreparationMappedArtifact, TreeError> {
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
        Ok(PreparationMappedArtifact {
            artifact: reference.artifact,
            mapping,
            validation,
        })
    }

    /// Serve one reference from an already retained slot, without taking a free
    /// one. `None` means this artifact is not in the table.
    fn slot_hit<'a>(
        &'a self,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<FramedBlock<'a>>, TreeError> {
        self.check_owner(resources)?;
        let slot = crate::property_graph::storage::mapping_slot(
            self.slots.as_slice(),
            reference.artifact,
            |mapped| mapped.artifact,
            || resources.step(1),
        )?;
        if let Some(mapped) = slot.and_then(OnceCell::get) {
            return self.decode(mapped, reference, resources).map(Some);
        }
        Ok(None)
    }

    /// Serve one reference from the retained slot table, filling a free slot when
    /// the artifact is new. `None` means every slot is already taken by another
    /// artifact; the caller decides whether that is fatal or falls back.
    /// Serve one reference from the retained slot table, filling a free slot when
    /// the artifact is new. `None` means every slot is already taken by another
    /// artifact; the caller decides whether that is fatal or falls back.
    fn slot_block<'a>(
        &'a self,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<FramedBlock<'a>>, TreeError> {
        if let Some(block) = self.slot_hit(reference, resources)? {
            return Ok(Some(block));
        }
        let Some(cell) = crate::property_graph::storage::mapping_slot(
            self.slots.as_slice(),
            reference.artifact,
            |mapped| mapped.artifact,
            || resources.step(1),
        )?
        else {
            return Ok(None);
        };
        cell.set(self.open_mapping(reference, resources)?)
            .map_err(|_| TreeError::Invalid("native preparation source slot initialized twice"))?;
        self.filled.set(self.filled.get().saturating_add(1));
        let mapped = cell.get().ok_or(TreeError::Invalid(
            "native preparation source slot remained empty",
        ))?;
        self.decode(mapped, reference, resources).map(Some)
    }

    /// Whether a scoped traversal must stop pinning. The reserve keeps `resolve`
    /// answerable for a consumer that already chose the pinning path.
    fn slots_exhausted(&self) -> bool {
        self.slots
            .as_slice()
            .len()
            .saturating_sub(self.filled.get())
            <= RESERVED_PINNED_SLOTS
    }
}

impl BlockSource for NativePreparationSource<'_, '_> {
    fn resolve<'a>(
        &'a self,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'a>, TreeError> {
        self.slot_block(reference, resources)?
            .ok_or(TreeError::Memory)
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
        if !self.scoped_blocks() {
            let block = self.resolve(reference, resources)?;
            return callback(block, resources);
        }
        self.check_owner(resources)?;
        if self.retain_window.get() {
            // Artifacts already pinned before the table filled stay free to read.
            // The reserve is never spent here; it belongs to `resolve`.
            if let Some(block) = self.slot_hit(reference, resources)? {
                return callback(block, resources);
            }
            if let Ok(window) = self.window.try_borrow()
                && let Some(mapped) = window.as_ref()
                && mapped.artifact == reference.artifact
            {
                let block = self.decode(mapped, reference, resources)?;
                return callback(block, resources);
            }
            if let Ok(mut window) = self.window.try_borrow_mut() {
                *window = Some(self.open_mapping(reference, resources)?);
                drop(window);
                let window = self
                    .window
                    .try_borrow()
                    .map_err(|_| TreeError::Invalid("scoped window is already borrowed"))?;
                let mapped = window
                    .as_ref()
                    .ok_or(TreeError::Invalid("scoped window remained empty"))?;
                let block = self.decode(mapped, reference, resources)?;
                return callback(block, resources);
            }
        }
        // A nested read naming another artifact keeps the retained window and
        // releases its own mapping before returning.
        let mapped = self.open_mapping(reference, resources)?;
        let block = self.decode(&mapped, reference, resources)?;
        let result = callback(block, resources);
        drop(mapped);
        result
    }

    /// A scoped traversal keeps pinning until its retained slot table is spent.
    /// Until then it reads exactly as an unscoped traversal does.
    fn scoped_blocks(&self) -> bool {
        self.scoped.get() || (self.retain_window.get() && self.slots_exhausted())
    }

    fn with_scoped_reads<R>(&self, body: impl FnOnce() -> R) -> R {
        let previous_retain = self.retain_window.replace(true);
        let result = body();
        self.retain_window.set(previous_retain);
        if !previous_retain && let Ok(mut window) = self.window.try_borrow_mut() {
            *window = None;
        }
        result
    }
}

impl BlockSource for NativeArtifactWindow<'_, '_, '_> {
    fn resolve<'a>(
        &'a self,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'a>, TreeError> {
        self.source.check_owner(resources)?;
        if reference.artifact != self.artifact {
            return Err(TreeError::Invalid("artifact window reference owner"));
        }
        resources.step(1)?;
        let block = self
            .validation
            .framed_block(self.mapping.as_bytes(), reference)
            .map_err(TreeError::Format)?;
        self.source.check_required(block)
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
        if reference.artifact == self.artifact {
            let block = self.resolve(reference, resources)?;
            callback(block, resources)
        } else {
            self.source.with_block(reference, resources, callback)
        }
    }

    fn scoped_blocks(&self) -> bool {
        true
    }
}

impl<'source, 'lease, 'm> NativeArtifactWindow<'source, 'lease, 'm> {
    /// Reuse this authenticated object or enter one nested bounded object
    /// window through the same admitted parent.
    pub(crate) fn with_artifact_window<R>(
        &self,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
        callback: impl for<'a, 'r> FnOnce(
            &'a NativeArtifactWindow<'_, 'lease, 'm>,
            &'r mut TreeResources<'_>,
        ) -> Result<R, TreeError>,
    ) -> Result<R, TreeError> {
        if reference.artifact == self.artifact {
            callback(self, resources)
        } else {
            self.source
                .with_artifact_window(reference, resources, callback)
        }
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

    pub(crate) fn relationship_rule(
        &self,
        id: crate::property_graph::catalog::RelTypeId,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<crate::property_graph::catalog::OnDelete>, TreeError> {
        for entry in self.image.symbols.entries() {
            resources.step(1)?;
            if entry.symbol == Symbol::RelationshipType(id) {
                let mut failure = None;
                let result = self.image.relationship_rules.lookup(entry.name, &mut || {
                    resources.step(1).map_err(|error| {
                        failure = Some(error);
                        CatalogError::Cancelled
                    })
                });
                if let Some(error) = failure {
                    return Err(error);
                }
                return result.map_err(|_| TreeError::Invalid("invalid relationship policy"));
            }
        }
        Err(TreeError::Invalid("relationship type absent from catalog"))
    }

    pub(crate) fn relationship_rules(
        &self,
    ) -> crate::property_graph::catalog::RelationshipRules<'source> {
        self.image.relationship_rules
    }

    pub(crate) fn symbol_entries(&self) -> &[SymbolEntry<'source>] {
        self.image.symbols.entries()
    }

    pub(crate) fn lookup_symbol(
        &self,
        kind: SymbolKind,
        name: crate::property_graph::GraphName<'_>,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<Symbol>, TreeError> {
        self.image
            .symbols
            .lookup(kind, name, &mut || {
                resources.step(1).map_err(|_| CatalogError::Cancelled)
            })
            .map_err(|error| catalog_error(error, resources))
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
    fn namespace_id(
        &self,
        name: crate::property_graph::GraphName<'_>,
        r: &mut TreeResources<'_>,
    ) -> Result<crate::property_graph::catalog::NamespaceId, TreeError> {
        match self.lookup_symbol(SymbolKind::Namespace, name, r)? {
            Some(Symbol::Namespace(id)) => Ok(id),
            _ => Err(TreeError::Invalid("structured namespace absent")),
        }
    }
    fn relationship_on_delete(
        &self,
        id: crate::property_graph::catalog::RelTypeId,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<crate::property_graph::catalog::OnDelete>, TreeError> {
        self.relationship_rule(id, resources)
    }

    fn base_identity(&self) -> BaseIdentity {
        self.base
    }
}
