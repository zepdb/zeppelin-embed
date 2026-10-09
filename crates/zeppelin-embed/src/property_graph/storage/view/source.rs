//! Fixed-capacity caller-thread source for one admitted immutable bundle.

use super::mapping::NativeReadonlyMapping;
use crate::lifecycle::native_graph::NativeReadLease;
use crate::property_graph::query::resources::{QueryArena, QueryMemory, QueryReservation};
use crate::property_graph::query::runtime::{RetainedView, RuntimeContext, RuntimeInstanceId};
use crate::property_graph::storage::artifact::{
    self, ArtifactControlError, ArtifactId, ContainerKind, FramedBlock, PhysicalRef,
    ValidatedArtifact,
};
use crate::property_graph::storage::tree::directory::{
    BlockSource, QueryOwner, TreeError, TreeResources,
};
use std::{
    cell::{Cell, OnceCell},
    ffi::OsString,
    fmt::Write as _,
    path::PathBuf,
};

struct MappedArtifact<'m, 'g> {
    mapping: NativeReadonlyMapping,
    validation: ValidatedArtifact,
    // QueryArena charges its heap and control descriptor. Boxing the descriptor
    // keeps its charge separate from the mapping table's capacity reservation.
    validated_pages: Box<[QueryArena<'m, 'g, Cell<u8>>; 1]>,
}

fn charged_artifact_path<'m, 'g>(
    memory: &'m QueryMemory<'g>,
    directory: &std::path::Path,
    artifact: ArtifactId,
) -> Result<(PathBuf, QueryReservation<'m, 'g>), TreeError> {
    const FILENAME_BYTES: usize = b"graph-00000000000000000000000000000000.zgraph".len();
    let path_upper = directory
        .as_os_str()
        .len()
        .checked_add(1)
        .and_then(|bytes| bytes.checked_add(FILENAME_BYTES))
        .ok_or(TreeError::Memory)?;
    let mut charge = memory
        .reserve(
            path_upper
                .checked_add(FILENAME_BYTES)
                .ok_or(TreeError::Memory)?,
        )
        .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
        .map_err(TreeError::Runtime)?;
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
    charge
        .resize(
            filename
                .capacity()
                .checked_add(path_capacity)
                .ok_or(TreeError::Memory)?,
        )
        .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
        .map_err(TreeError::Runtime)?;
    drop(filename);
    charge
        .resize(path_capacity)
        .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
        .map_err(TreeError::Runtime)?;
    Ok((PathBuf::from(raw), charge))
}

pub(crate) struct NativeQuerySource<'a, 'm, 'g> {
    lease: &'a NativeReadLease,
    memory: &'m QueryMemory<'g>,
    owner: QueryOwner<'m, 'g>,
    runtime: RuntimeInstanceId,
    slots: QueryArena<'m, 'g, OnceCell<MappedArtifact<'m, 'g>>>,
}

/// Unforgeable proof that one retained lease, query-memory owner and runtime
/// view were admitted together before any native source or catalog is opened.
pub(crate) struct NativeReadCapability<'a, 'm, 'g> {
    lease: &'a NativeReadLease,
    memory: &'m QueryMemory<'g>,
    runtime: RuntimeInstanceId,
}

impl<'a, 'm, 'g> NativeReadCapability<'a, 'm, 'g> {
    pub(crate) fn admit(
        lease: &'a NativeReadLease,
        runtime: &RuntimeContext<'a, 'm, 'g>,
    ) -> Result<Self, TreeError> {
        lease
            .check_active()
            .map_err(crate::property_graph::query::runtime::RuntimeError::Value)
            .map_err(TreeError::Runtime)?;
        if !std::ptr::eq(runtime.view(), lease.query_view()) {
            return Err(TreeError::Invalid("foreign native read capability"));
        }
        Ok(Self {
            lease,
            memory: runtime.memory(),
            runtime: runtime.identity(),
        })
    }
}

impl<'a, 'm, 'g> NativeQuerySource<'a, 'm, 'g> {
    pub(crate) fn new(
        capability: NativeReadCapability<'a, 'm, 'g>,
        resources: &TreeResources<'_>,
        capacity: usize,
    ) -> Result<Self, TreeError> {
        if capacity == 0 {
            return Err(TreeError::Memory);
        }
        let owner = resources.query_owner(capability.memory)?;
        let mut slots = QueryArena::new(capability.memory, capacity)
            .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
            .map_err(TreeError::Runtime)?;
        for _ in 0..capacity {
            slots
                .push(OnceCell::new())
                .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
                .map_err(TreeError::Runtime)?;
        }
        Ok(Self {
            lease: capability.lease,
            memory: capability.memory,
            owner,
            runtime: capability.runtime,
            slots,
        })
    }

    #[cfg(any(test, feature = "test-seams"))]
    pub(super) fn retained_validation_bytes(&self) -> usize {
        self.slots
            .as_slice()
            .iter()
            .filter_map(OnceCell::get)
            .flat_map(|mapped| mapped.validated_pages.iter())
            .map(QueryArena::reserved_bytes)
            .sum()
    }

    pub(super) const fn lease(&self) -> &'a NativeReadLease {
        self.lease
    }

    pub(super) const fn memory(&self) -> &'m QueryMemory<'g> {
        self.memory
    }

    pub(super) const fn runtime(&self) -> RuntimeInstanceId {
        self.runtime
    }

    fn check_owner(&self, resources: &mut TreeResources<'_>) -> Result<(), TreeError> {
        self.lease
            .check_active()
            .map_err(crate::property_graph::query::runtime::RuntimeError::Value)
            .map_err(TreeError::Runtime)?;
        resources.require_query_owner(self.owner)?;
        resources.step(0)
    }

    fn page_bit(&self, reference: PhysicalRef) -> Result<(&Cell<u8>, u8), TreeError> {
        if reference.kind != artifact::BlockKind::TreePage {
            return Err(TreeError::Invalid("page memo requires a directory page"));
        }
        let mapped = self
            .slots
            .as_slice()
            .iter()
            .filter_map(OnceCell::get)
            .find(|mapped| mapped.validation.artifact() == reference.artifact)
            .ok_or(TreeError::Invalid("page memo mapping missing"))?;
        let bucket = usize::try_from(reference.offset).map_err(|_| TreeError::Memory)?
            / super::super::tree::PAGE_BYTES;
        let byte = mapped
            .validated_pages
            .first()
            .ok_or(TreeError::Memory)?
            .as_slice()
            .get(bucket / 8)
            .ok_or(TreeError::Invalid("page memo reference outside mapping"))?;
        Ok((byte, 1_u8 << (bucket % 8)))
    }

    fn decode<'s>(
        &'s self,
        mapped: &'s MappedArtifact<'m, 'g>,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'s>, TreeError> {
        resources.step(1)?;
        let block = mapped
            .validation
            .framed_block(mapped.mapping.as_bytes(), reference)
            .map_err(TreeError::Format)?;
        let block = self.check_required(block)?;
        Ok(block)
    }

    fn admit_mapping(
        &self,
        mapping: &NativeReadonlyMapping,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<ValidatedArtifact, TreeError> {
        let base = self.lease.bundle().base();
        let high_waters = self.lease.bundle().high_waters();
        let frame = artifact::decode_with_control(
            ContainerKind::Object,
            Some((base.store, reference.artifact)),
            mapping.as_bytes(),
            &mut |bytes| resources.step(bytes as u64),
        )
        .map_err(|error| match error {
            ArtifactControlError::Format(error) => TreeError::Format(error),
            ArtifactControlError::Control(error) => error,
        })?;
        let identity = frame.identity();
        if identity.generation > base.generation
            || identity.creation_serial > high_waters.creation_serial
        {
            return Err(TreeError::Invalid(
                "native graph artifact is newer than admitted cutoff",
            ));
        }
        let block = frame.framed_block(reference).map_err(TreeError::Format)?;
        let validation = frame.validation();
        let _ = self.check_required(block)?;
        Ok(validation)
    }

    fn check_required<'s>(&self, block: FramedBlock<'s>) -> Result<FramedBlock<'s>, TreeError> {
        if let Some(required) = self.lease.bundle().required_object(block.reference()) {
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
                    "native graph required descriptor mismatch",
                ));
            }
        }
        Ok(block)
    }
}

impl BlockSource for NativeQuerySource<'_, '_, '_> {
    fn page_validated(&self, reference: PhysicalRef) -> Result<bool, TreeError> {
        let (byte, mask) = self.page_bit(reference)?;
        Ok(byte.get() & mask != 0)
    }

    fn mark_page_validated(&self, reference: PhysicalRef) -> Result<(), TreeError> {
        let (byte, mask) = self.page_bit(reference)?;
        byte.set(byte.get() | mask);
        Ok(())
    }

    fn resolve<'a>(
        &'a self,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'a>, TreeError> {
        self.check_owner(resources)?;
        let cell = crate::property_graph::storage::mapping_slot(
            self.slots.as_slice(),
            reference.artifact,
            |mapped| mapped.validation.artifact(),
            || resources.step(1),
        )?
        .ok_or(TreeError::Memory)?;
        if let Some(mapped) = cell.get() {
            return self.decode(mapped, reference, resources);
        }
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
        // Exact admitted blocks never overlap. Every directory page spans
        // PAGE_BYTES plus its frame, so distinct pages cannot share a bucket.
        // Record blocks cannot consult or set these directory-page bits.
        let memo_bytes = mapping
            .as_bytes()
            .len()
            .div_ceil(super::super::tree::PAGE_BYTES)
            .div_ceil(8);
        let mut pages = QueryArena::new(self.memory, memo_bytes)
            .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
            .map_err(TreeError::Runtime)?;
        resources.step(memo_bytes as u64)?;
        for _ in 0..memo_bytes {
            pages
                .push(Cell::new(0))
                .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
                .map_err(TreeError::Runtime)?;
        }
        let mut descriptor = Vec::new();
        descriptor
            .try_reserve_exact(1)
            .map_err(|_| TreeError::Memory)?;
        if descriptor.capacity() != 1 {
            return Err(TreeError::Memory);
        }
        descriptor.push(pages);
        let validated_pages = descriptor
            .into_boxed_slice()
            .try_into()
            .map_err(|_| TreeError::Memory)?;
        let mapped = MappedArtifact {
            mapping,
            validation,
            validated_pages,
        };
        cell.set(mapped)
            .map_err(|_| TreeError::Invalid("native graph source slot initialized twice"))?;
        let mapped = cell.get().ok_or(TreeError::Invalid(
            "native graph source slot remained empty",
        ))?;
        self.decode(mapped, reference, resources)
    }
}
