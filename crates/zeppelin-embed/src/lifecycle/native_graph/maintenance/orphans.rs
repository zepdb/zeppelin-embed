//! Adoption of complete, unregistered native objects into the allocation
//! inventory.
//!
//! A crash between object creation and its WAL envelope leaves a complete
//! object file that no inventory names. Retired spill and proof pages are the
//! same kind of file: complete native objects with no inventory row. Neither
//! can become a reclaim candidate only after entering the inventory.
//!
//! Superseded root envelopes and WALs are different: the same quiescent scan
//! adds them directly to the existing intent candidate list when unmarked.
//! They are never adopted into ObjectInventory and use the same validated
//! intent/unlink/completion path as inventoried objects.
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
//!
//! # Interrupted creations (ZE-165)
//!
//! A crash between `create_new` writing its first bytes and writing its last
//! leaves a file whose header is complete but whose body is not. It has no
//! whole-file checksum, so it can never produce an `ArtifactDescriptor` and
//! can never be adopted by the path above. ZE-46 therefore retained it, which
//! its frozen plan allows as an explicit conservative limit.
//!
//! [`select_partial_targets`] reclaims it instead, under every exclusion
//! adoption already applies plus one more: the intact header must *declare* a
//! length strictly greater than the bytes the file actually holds. That
//! inequality is the interrupted-prefix classification, and a file that lacks
//! it is either a complete object (adoption's business) or unknown input.
//!
//! The result is not an object and never enters the inventory, the completed
//! mark or the WAL candidate lists. It travels as its own tagged partition of
//! the role-5 pending and completion records, carrying the exact bytes
//! observed and their digest, and the unlink re-observes both before it runs.

use super::super::write::io;
use super::spill::NativeSpillWriter;
use super::{NativeGraphError, NativeMaintenanceAdmission};
use crate::lifecycle::native_graph::NativeProtectedRoots;
use crate::property_graph::storage::NativePreparationSource;
use crate::property_graph::storage::artifact::{ArtifactId, HEADER_BYTES, MAX_ARTIFACT_BYTES};
use crate::property_graph::storage::inventory::for_each_prepared_descriptor;
use crate::property_graph::storage::memory::{StorageBuffer, StorageMemory};
use crate::property_graph::storage::reclaim::{DurableRun, DurableRunReader, PartialTarget};
use crate::property_graph::storage::tree::TreeKind;
use crate::property_graph::storage::tree::directory::{TreeError, TreeResources, lookup_entry};
use crate::property_graph::wal::{ArtifactDescriptor, InventoryChange, InventoryState};
use std::path::Path;

use crate::property_graph::storage::inventory::INVENTORY_ADOPTION_LIMIT as ADOPTION_LIMIT;

const MAGIC: &[u8; 8] = b"ZEPEMBED";

#[cfg(any(test, feature = "test-seams"))]
thread_local! {
    static ADOPTION_SUSPENDED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Suspend adoption on this thread until the guard drops. A fixture that
/// asserts an exact candidate set uses it; production maintenance always
/// adopts when quiescent.
#[cfg(any(test, feature = "test-seams"))]
#[must_use]
pub(in crate::lifecycle::native_graph) fn suspend_adoption_for_test() -> AdoptionSuspension {
    ADOPTION_SUSPENDED.with(|flag| flag.set(true));
    AdoptionSuspension
}

#[cfg(any(test, feature = "test-seams"))]
pub(in crate::lifecycle::native_graph) struct AdoptionSuspension;

#[cfg(any(test, feature = "test-seams"))]
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
        || !matches!(family, 17 | 18)
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

/// The identity an interrupted creation claims for itself, or `None` when any
/// header fact disqualifies it.
///
/// Every check matches [`claimed_descriptor`] except the length rule: this one
/// requires the declared length to be strictly greater than the actual length,
/// which is exactly what separates an interrupted prefix from a complete
/// object. `digest` is supplied by the caller because it costs a full read of
/// the observed bytes and must not be paid for a file that fails the header.
fn claimed_partial(
    header: &[u8],
    actual_length: u64,
    named: ArtifactId,
    admission: &NativeMaintenanceAdmission,
    serial_fence: u64,
    digest: u64,
) -> Option<PartialTarget> {
    let base = admission.lease.bundle();
    let family = u16::from_le_bytes(*header.get(8..10)?.first_chunk()?);
    let version = u16::from_le_bytes(*header.get(10..12)?.first_chunk()?);
    let flags = u32::from_le_bytes(*header.get(12..16)?.first_chunk()?);
    let generation = read_u64(header, 64)?;
    let serial = read_u64(header, 88)?;
    let declared = read_u64(header, 24)?;
    if header.len() != HEADER_BYTES
        || header.get(..8)? != MAGIC
        || family != crate::format::FormatFamily::NativeGraphObject.id()
        || version != 1
        || flags != 0
        || read_u64(header, 16)? != HEADER_BYTES as u64
        || declared <= actual_length
        || declared > MAX_ARTIFACT_BYTES as u64
        || read_u128(header, 32)? != base.base().store.get()
        || read_u128(header, 48)? != named.get()
        || generation > base.base().generation.get()
        || serial == 0
        || serial > serial_fence
        || serial > base.high_waters().creation_serial
    {
        return None;
    }
    Some(PartialTarget {
        store: base.base().store,
        artifact: named,
        generation: crate::property_graph::GraphGeneration::new(generation),
        serial,
        observed: actual_length,
        declared,
        digest,
        family,
        version,
    })
}

/// xxh3-64 over exactly the first `length` bytes of `path`, read in bounded
/// chunks so one 4 MiB artifact never becomes one 4 MiB allocation.
pub(in crate::lifecycle::native_graph) fn observe_digest(
    vfs: &dyn crate::vfs::Vfs,
    path: &Path,
    length: u64,
    resources: &mut TreeResources<'_>,
) -> Result<u64, NativeGraphError> {
    const CHUNK: usize = 64 * 1024;
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    let mut offset = 0_u64;
    while offset < length {
        resources.step(1)?;
        let remaining = length.saturating_sub(offset);
        let take = usize::try_from(remaining.min(CHUNK as u64))
            .map_err(|_| NativeGraphError::Invalid("partial target chunk"))?;
        let chunk = vfs
            .read_range(path, offset, take)
            .map_err(|source| io(path, source))?;
        if chunk.len() != take {
            return Err(NativeGraphError::Invalid("partial target short read"));
        }
        hasher.update(&chunk);
        offset = offset
            .checked_add(take as u64)
            .ok_or(NativeGraphError::Invalid("partial target extent"))?;
    }
    Ok(hasher.digest())
}

/// What one canonical native-object name in the store directory turned out to
/// be. Everything that is neither is retained and never appears here.
enum Classified {
    /// A complete unregistered object: ZE-46 adopts it as bookkeeping.
    Object(ArtifactDescriptor),
    /// Immutable root or superseded WAL: intent candidate, never adopted.
    History(ArtifactDescriptor),
    /// An interrupted creation with an intact header: ZE-165 unlinks it under
    /// a durable intent. It never becomes an object.
    Partial(PartialTarget),
}

/// One directory pass: adoptions, partial targets, and history candidates.
pub(super) struct OrphanSelection<'m> {
    /// Complete unregistered objects, as inventory rows to adopt.
    pub(super) adoptions: StorageBuffer<'m, InventoryChange>,
    /// Interrupted creations, sorted by artifact, as reclaim targets.
    pub(super) partials: StorageBuffer<'m, PartialTarget>,
    pub(super) history: StorageBuffer<'m, ArtifactDescriptor>,
}

/// Select complete unregistered objects for adoption and interrupted
/// creations for reclamation. Streams the directory through one bounded
/// state; neither result ever exceeds [`ADOPTION_LIMIT`].
pub(super) fn select_adoptions<'m>(
    admission: &NativeMaintenanceAdmission,
    capture: &NativeProtectedRoots,
    mark: DurableRun,
    writer: &NativeSpillWriter<'_, 'm>,
    storage: &'m StorageMemory<'m>,
    resources: &mut TreeResources<'_>,
    candidate_capacity: usize,
) -> Result<OrphanSelection<'m>, NativeGraphError> {
    let mut adoptions = StorageBuffer::new(storage, ADOPTION_LIMIT)?;
    let mut partials = StorageBuffer::new(storage, ADOPTION_LIMIT)?;
    let mut found = StorageBuffer::new(storage, ADOPTION_LIMIT)?;
    let mut targets = StorageBuffer::new(storage, ADOPTION_LIMIT)?;
    let mut history = StorageBuffer::new(storage, candidate_capacity)?;
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
        return Ok(OrphanSelection {
            adoptions,
            partials,
            history,
        });
    }
    #[cfg(any(test, feature = "test-seams"))]
    if ADOPTION_SUSPENDED.with(std::cell::Cell::get) {
        return Ok(OrphanSelection {
            adoptions,
            partials,
            history,
        });
    }
    let inventory_root = admitted.roots().directory(TreeKind::ObjectInventory)?;
    let mut live = DurableRunReader::new(mark, storage)?;
    let mut failure: Option<NativeGraphError> = None;
    let vfs = admitted.vfs();
    let streamed = vfs.for_each_direct_child(admitted.directory(), &mut |path| {
        if found.as_slice().len() == ADOPTION_LIMIT
            && targets.as_slice().len() + history.as_slice().len() == candidate_capacity
        {
            return Ok(());
        }
        let wal_named = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("graph-wal-")?.strip_suffix(".ze"))
            .filter(|digits| {
                digits.len() == 32
                    && digits
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
            .and_then(|digits| u128::from_str_radix(digits, 16).ok())
            .and_then(|id| ArtifactId::new(id).ok());
        let Some(named) = wal_named.or_else(|| artifact_of(path)) else {
            return Ok(());
        };
        let mut inspect = || -> Result<Option<Classified>, NativeGraphError> {
            resources.step(1)?;
            let length = vfs.open(path).map_err(|source| io(path, source))?;
            if wal_named.is_some() {
                if targets.as_slice().len() + history.as_slice().len() == candidate_capacity
                    || length < crate::property_graph::wal::HEADER_BYTES as u64
                    || length > u32::MAX as u64
                    || live.contains(named, writer, resources)?
                {
                    return Ok(None);
                }
                let header = vfs
                    .read_range(path, 0, crate::property_graph::wal::HEADER_BYTES)
                    .map_err(|source| io(path, source))?;
                let first = match NativePreparationSource::wal_first_sequence(
                    &header,
                    admitted.base().store,
                ) {
                    Ok(first) => first,
                    Err(TreeError::Invalid(_)) => return Ok(None),
                    Err(error) => return Err(error.into()),
                };
                if capture.wal().is_none_or(|wal| first > wal.first_sequence()) {
                    return Ok(None);
                }
                // WAL headers have no allocation serial/generation. These are
                // capture fences; identity, exact length and digest bind the file.
                let descriptor = ArtifactDescriptor {
                    store: admitted.base().store,
                    artifact: named,
                    generation: admitted.base().generation,
                    serial: capture.serial_fence(),
                    bytes: length as u32,
                    family: 19,
                    version: 1,
                    checksum: observe_digest(vfs, path, length, resources)?,
                };
                return Ok(Some(Classified::History(descriptor)));
            }
            // Shorter than one header is not ownership: a name alone grants
            // nothing, and there is nothing to read the store out of.
            if length < HEADER_BYTES as u64 || length > MAX_ARTIFACT_BYTES as u64 {
                return Ok(None);
            }
            let header = vfs
                .read_range(path, 0, HEADER_BYTES)
                .map_err(|source| io(path, source))?;
            // Unreachability is the same question for both classes, so ask it
            // once, before either classification pays for a whole-file read.
            let mut unreachable = || -> Result<bool, NativeGraphError> {
                if live.contains(named, writer, resources)? {
                    return Ok(false);
                }
                let source = NativePreparationSource::new(&admission.lease, storage, 8)?;
                let listed = lookup_entry(
                    &source,
                    inventory_root,
                    &named.get().to_le_bytes(),
                    resources,
                )?
                .is_some();
                Ok(!listed)
            };
            if length >= (HEADER_BYTES + 8) as u64
                && let Some(descriptor) = claimed_descriptor(
                    &vfs.read_range(path, 0, HEADER_BYTES)
                        .map_err(|source| io(path, source))?,
                    &vfs.read_range(path, length - 8, 8)
                        .map_err(|source| io(path, source))?,
                    length,
                    named,
                    admission,
                    capture.serial_fence(),
                )
            {
                if !unreachable()? {
                    return Ok(None);
                }
                // A complete-looking file that fails validation is unknown
                // input, not an interrupted preparation: it is retained.
                return match writer.validate_candidate(descriptor, resources) {
                    Ok(()) => Ok(Some(if descriptor.family == 18 {
                        Classified::History(descriptor)
                    } else {
                        Classified::Object(descriptor)
                    })),
                    Err(TreeError::Format(_) | TreeError::Invalid(_)) => Ok(None),
                    Err(error) => Err(error.into()),
                };
            }
            // Not a complete object. It is an interrupted creation only if its
            // own intact header declares more bytes than the file holds.
            if claimed_partial(&header, length, named, admission, capture.serial_fence(), 0)
                .is_none()
                || !unreachable()?
            {
                return Ok(None);
            }
            let digest = observe_digest(vfs, path, length, resources)?;
            Ok(claimed_partial(
                &header,
                length,
                named,
                admission,
                capture.serial_fence(),
                digest,
            )
            .map(Classified::Partial))
        };
        match inspect() {
            Ok(Some(Classified::Object(descriptor))) => {
                if found.as_slice().len() < ADOPTION_LIMIT {
                    found
                        .push(Some(descriptor))
                        .map_err(std::io::Error::other)?;
                }
                Ok(())
            }
            Ok(Some(Classified::Partial(target))) => {
                if targets.as_slice().len() + history.as_slice().len() < candidate_capacity {
                    targets.push(Some(target)).map_err(std::io::Error::other)?;
                }
                Ok(())
            }
            Ok(Some(Classified::History(descriptor))) => {
                if targets.as_slice().len() + history.as_slice().len() < candidate_capacity {
                    history.push(descriptor).map_err(std::io::Error::other)?;
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
        registered(found.as_mut_slice(), required.object);
        registered_partial(targets.as_mut_slice(), required.object);
        let scoped = NativePreparationSource::new_scoped(&admission.lease, storage, 1)?;
        for_each_prepared_descriptor(
            &scoped,
            required,
            admitted.base().store,
            admitted.base().generation,
            resources,
            |listed| {
                registered(found.as_mut_slice(), listed);
                registered_partial(targets.as_mut_slice(), listed);
                Ok(())
            },
        )?;
    }
    for descriptor in found.as_slice().iter().copied().flatten() {
        adoptions.push(InventoryChange {
            object: descriptor,
            state: InventoryState::Retained,
        })?;
    }
    adoptions
        .as_mut_slice()
        .sort_unstable_by_key(|change| change.object.artifact);
    for target in targets.as_slice().iter().copied().flatten() {
        partials.push(target)?;
    }
    partials
        .as_mut_slice()
        .sort_unstable_by_key(|target| target.artifact);
    Ok(OrphanSelection {
        adoptions,
        partials,
        history,
    })
}

/// An interrupted creation whose artifact id or serial is claimed by an
/// admitted prepared manifest is a registered in-flight object, not debris.
fn registered_partial(targets: &mut [Option<PartialTarget>], listed: ArtifactDescriptor) {
    for slot in targets.iter_mut() {
        if slot.is_some_and(|target| {
            target.artifact == listed.artifact || target.serial == listed.serial
        }) {
            *slot = None;
        }
    }
}
