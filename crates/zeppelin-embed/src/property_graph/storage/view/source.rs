//! Fixed-capacity caller-thread source for one admitted immutable bundle.

use super::mapping::NativeReadonlyMapping;
use crate::lifecycle::native_graph::NativeReadLease;
use crate::property_graph::query::resources::{QueryArena, QueryMemory, QueryReservation};
use crate::property_graph::query::runtime::{RetainedView, RuntimeContext, RuntimeInstanceId};
use crate::property_graph::storage::allocation::artifact_path;
use crate::property_graph::storage::artifact::{
    self, ArtifactControlError, ArtifactId, ContainerKind, FramedBlock, PhysicalRef,
};
use crate::property_graph::storage::tree::directory::{
    BlockSource, QueryOwner, TreeError, TreeResources,
};
use std::cell::OnceCell;

struct MappedArtifact<'m, 'g> {
    artifact: ArtifactId,
    mapping: NativeReadonlyMapping,
    _path_charge: QueryReservation<'m, 'g>,
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

    fn decode<'s>(
        &'s self,
        mapped: &'s MappedArtifact<'m, 'g>,
        reference: PhysicalRef,
        resources: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'s>, TreeError> {
        let base = self.lease.bundle().base();
        let high_waters = self.lease.bundle().high_waters();
        let frame = artifact::decode_with_control(
            ContainerKind::Object,
            Some((base.store, reference.artifact)),
            mapped.mapping.as_bytes(),
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
        if let Some(required) = self.lease.bundle().required_object(reference) {
            let expected = required.object;
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
        const ARTIFACT_PATH_SUFFIX_BYTES: usize =
            b"/graph-00000000000000000000000000000000.zgraph".len();
        let path_bytes = self
            .lease
            .bundle()
            .directory()
            .as_os_str()
            .len()
            .checked_add(ARTIFACT_PATH_SUFFIX_BYTES)
            .ok_or(TreeError::Memory)?;
        let path_charge = self
            .memory
            .reserve(path_bytes)
            .map_err(crate::property_graph::query::runtime::RuntimeError::Memory)
            .map_err(TreeError::Runtime)?;
        let path = artifact_path(self.lease.bundle().directory(), reference.artifact);
        let file = self
            .lease
            .bundle()
            .vfs()
            .open_for_map(&path)
            .map_err(TreeError::Io)?;
        let mapped = MappedArtifact {
            artifact: reference.artifact,
            mapping: NativeReadonlyMapping::open(file, &path, self.lease)?,
            _path_charge: path_charge,
        };
        // Validate the complete immutable file before exposing or caching it.
        let _ = self.decode(&mapped, reference, resources)?;
        cell.set(mapped)
            .map_err(|_| TreeError::Invalid("native graph source slot initialized twice"))?;
        let mapped = cell.get().ok_or(TreeError::Invalid(
            "native graph source slot remained empty",
        ))?;
        self.decode(mapped, reference, resources)
    }
}
