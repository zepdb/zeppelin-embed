//! Adoption of complete, unregistered native objects into the allocation
//! inventory.
//!
//! A crash between object creation and its WAL envelope leaves a complete
//! object file that no inventory names. Retired spill and proof pages are the
//! same kind of file: complete native objects with no inventory row. Neither
//! can ever become a reclaim candidate, because candidates come only from the
//! inventory.
//!
//! Adoption repairs the bookkeeping and nothing else. It never deletes. An
//! adopted row is ordinary allocation bookkeeping; a later maintenance selects
//! it through the normal mark, intent, unlink and completion path, with the
//! same proof and recovery validation as every other object.
//!
//! A file is adopted only when all of this holds:
//!
//! * nothing durable outside the traced roots can name it: the captured WAL
//!   is header-only, no captured bundle roots a reclaim state, and no proof or
//!   spill is protected;
//! * its name is a canonical native object name and its complete 96-byte
//!   header says: supported object family and version, this store, the
//!   artifact of its own name, a generation and creation serial at or below
//!   the captured cutoffs, and a declared length equal to its actual length;
//! * it is absent from the completed mark, from the rooted inventory and from
//!   every admitted prepared manifest;
//! * the whole file validates against the descriptor read from it.
//!
//! Everything else stays exactly where it is: unknown names, unknown families,
//! other stores' objects, newer or in-flight objects, short or headerless
//! prefixes, and files that fail validation. Filename shape alone grants
//! nothing.

use super::super::write::io;
use super::spill::NativeSpillWriter;
use super::{NativeGraphError, NativeMaintenanceAdmission};
use crate::lifecycle::native_graph::NativeProtectedRoots;
use crate::property_graph::storage::NativePreparationSource;
use crate::property_graph::storage::artifact::{ArtifactId, HEADER_BYTES, MAX_ARTIFACT_BYTES};
use crate::property_graph::storage::inventory::for_each_prepared_descriptor;
use crate::property_graph::storage::memory::{StorageBuffer, StorageMemory};
use crate::property_graph::storage::reclaim::{DurableRun, DurableRunReader};
use crate::property_graph::storage::tree::TreeKind;
use crate::property_graph::storage::tree::directory::{TreeError, TreeResources, lookup_entry};
use crate::property_graph::wal::{ArtifactDescriptor, InventoryChange, InventoryState};
use std::path::Path;

use crate::property_graph::storage::inventory::INVENTORY_ADOPTION_LIMIT as ADOPTION_LIMIT;

const MAGIC: &[u8; 8] = b"ZEPEMBED";

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static ADOPTION_SUSPENDED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Suspend adoption on this thread until the guard drops. A fixture that
/// asserts an exact candidate set uses it; production maintenance always
/// adopts when quiescent.
#[cfg(any(test, feature = "test-support"))]
#[must_use]
pub(in crate::lifecycle::native_graph) fn suspend_adoption_for_test() -> AdoptionSuspension {
    ADOPTION_SUSPENDED.with(|flag| flag.set(true));
    AdoptionSuspension
}

#[cfg(any(test, feature = "test-support"))]
pub(in crate::lifecycle::native_graph) struct AdoptionSuspension;

#[cfg(any(test, feature = "test-support"))]
impl Drop for AdoptionSuspension {
    fn drop(&mut self) {
        ADOPTION_SUSPENDED.with(|flag| flag.set(false));
    }
}

fn artifact_of(path: &Path) -> Option<ArtifactId> {
    let name = path.file_name()?.to_str()?;
    let digits = name.strip_prefix("graph-")?.strip_suffix(".zgraph")?;
    if digits.len() != 32 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    ArtifactId::new(u128::from_str_radix(digits, 16).ok()?).ok()
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        *bytes.get(offset..offset.checked_add(8)?)?.first_chunk()?,
    ))
}

fn read_u128(bytes: &[u8], offset: usize) -> Option<u128> {
    Some(u128::from_le_bytes(
        *bytes.get(offset..offset.checked_add(16)?)?.first_chunk()?,
    ))
}

/// The descriptor a complete object file claims for itself, or `None` when
/// any header fact disqualifies it. The checksum is only claimed here; the
/// caller validates the whole file against it.
fn claimed_descriptor(
    header: &[u8],
    trailer: &[u8],
    actual_length: u64,
    named: ArtifactId,
    admission: &NativeMaintenanceAdmission,
    serial_fence: u64,
) -> Option<ArtifactDescriptor> {
    let base = admission.lease.bundle();
    let family = u16::from_le_bytes(*header.get(8..10)?.first_chunk()?);
    let version = u16::from_le_bytes(*header.get(10..12)?.first_chunk()?);
    let flags = u32::from_le_bytes(*header.get(12..16)?.first_chunk()?);
    let generation = read_u64(header, 64)?;
    let serial = read_u64(header, 88)?;
    if header.len() != HEADER_BYTES
        || header.get(..8)? != MAGIC
        || family != crate::format::FormatFamily::NativeGraphObject.id()
        || version != 1
        || flags != 0
        || read_u64(header, 16)? != HEADER_BYTES as u64
        || read_u64(header, 24)? != actual_length
        || read_u128(header, 32)? != base.base().store.get()
        || read_u128(header, 48)? != named.get()
        || generation > base.base().generation.get()
        || serial == 0
        || serial > serial_fence
        || serial > base.high_waters().creation_serial
    {
        return None;
    }
    Some(ArtifactDescriptor {
        store: base.base().store,
        artifact: named,
        generation: crate::property_graph::GraphGeneration::new(generation),
        serial,
        bytes: u32::try_from(actual_length).ok()?,
        family,
        version,
        checksum: u64::from_le_bytes(*trailer.first_chunk()?),
    })
}

/// Select complete unregistered objects for adoption. Streams the directory
/// through one bounded state; the result never exceeds [`ADOPTION_LIMIT`].
pub(super) fn select_adoptions<'m>(
    admission: &NativeMaintenanceAdmission,
    capture: &NativeProtectedRoots,
    mark: DurableRun,
    writer: &NativeSpillWriter<'_, 'm>,
    storage: &'m StorageMemory<'m>,
    resources: &mut TreeResources<'_>,
) -> Result<StorageBuffer<'m, InventoryChange>, NativeGraphError> {
    let mut adoptions = StorageBuffer::new(storage, ADOPTION_LIMIT)?;
    let mut found: [Option<ArtifactDescriptor>; ADOPTION_LIMIT] = [None; ADOPTION_LIMIT];
    let mut found_len = 0_usize;
    let admitted = admission.lease.bundle();
    let quiescent = capture
        .wal()
        .is_some_and(|wal| wal.bytes() == crate::property_graph::wal::HEADER_BYTES)
        && capture.proofs().is_empty()
        && capture.spills().is_empty()
        && capture
            .bundles()
            .iter()
            .all(|bundle| bundle.reclaim().is_none());
    if !quiescent {
        return Ok(adoptions);
    }
    #[cfg(any(test, feature = "test-support"))]
    if ADOPTION_SUSPENDED.with(std::cell::Cell::get) {
        return Ok(adoptions);
    }
    let inventory_root = admitted.roots().directory(TreeKind::ObjectInventory)?;
    let mut live = DurableRunReader::new(mark, storage)?;
    let mut failure: Option<NativeGraphError> = None;
    let vfs = admitted.vfs();
    let streamed = vfs.for_each_direct_child(admitted.directory(), &mut |path| {
        if found_len == ADOPTION_LIMIT {
            return Ok(());
        }
        let Some(named) = artifact_of(path) else {
            return Ok(());
        };
        let mut inspect = || -> Result<Option<ArtifactDescriptor>, NativeGraphError> {
            resources.step(1)?;
            let length = vfs.open(path).map_err(|source| io(path, source))?;
            if length < (HEADER_BYTES + 8) as u64 || length > MAX_ARTIFACT_BYTES as u64 {
                return Ok(None);
            }
            let header = vfs
                .read_range(path, 0, HEADER_BYTES)
                .map_err(|source| io(path, source))?;
            let trailer = vfs
                .read_range(path, length - 8, 8)
                .map_err(|source| io(path, source))?;
            let Some(descriptor) = claimed_descriptor(
                &header,
                &trailer,
                length,
                named,
                admission,
                capture.serial_fence(),
            ) else {
                return Ok(None);
            };
            if live.contains(named, writer, resources)? {
                return Ok(None);
            }
            let source = NativePreparationSource::new(&admission.lease, storage, 8)?;
            if lookup_entry(
                &source,
                inventory_root,
                &named.get().to_le_bytes(),
                resources,
            )?
            .is_some()
            {
                return Ok(None);
            }
            drop(source);
            // A complete-looking file that fails validation is unknown input,
            // not an interrupted preparation: it is retained.
            match writer.validate_candidate(descriptor, resources) {
                Ok(()) => Ok(Some(descriptor)),
                Err(TreeError::Format(_) | TreeError::Invalid(_)) => Ok(None),
                Err(error) => Err(error.into()),
            }
        };
        match inspect() {
            Ok(Some(descriptor)) => {
                if let Some(slot) = found.get_mut(found_len) {
                    *slot = Some(descriptor);
                    found_len += 1;
                }
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(error) => {
                failure = Some(error);
                Err(std::io::Error::other("native orphan inspection failed"))
            }
        }
    });
    if let Some(error) = failure {
        return Err(error);
    }
    streamed.map_err(|source| io(admitted.directory(), source))?;

    // An object listed by an admitted prepared manifest is registered: the
    // fold will root it. Serial ownership must stay unique as well.
    for required in admitted.prepared_inventories().iter().copied() {
        let registered = |found: &mut [Option<ArtifactDescriptor>], listed: ArtifactDescriptor| {
            for slot in found.iter_mut() {
                if slot.is_some_and(|adoption| {
                    adoption.artifact == listed.artifact || adoption.serial == listed.serial
                }) {
                    *slot = None;
                }
            }
        };
        registered(&mut found, required.object);
        let scoped = NativePreparationSource::new_scoped(&admission.lease, storage, 1)?;
        for_each_prepared_descriptor(
            &scoped,
            required,
            admitted.base().store,
            admitted.base().generation,
            resources,
            |listed| {
                registered(&mut found, listed);
                Ok(())
            },
        )?;
    }
    for descriptor in found.into_iter().flatten() {
        adoptions.push(InventoryChange {
            object: descriptor,
            state: InventoryState::Retained,
        })?;
    }
    adoptions
        .as_mut_slice()
        .sort_unstable_by_key(|change| change.object.artifact);
    Ok(adoptions)
}
