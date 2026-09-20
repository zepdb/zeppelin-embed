//! Scoped handoff for one actual native preparation and its admitted base.

use crate::format::FormatFamily;
use crate::lifecycle::native_graph::{NativePreparedRegistration, NativeReadLease};
use crate::property_graph::storage::adjacency::NativeGraphCandidate;
use crate::property_graph::storage::memory::StorageBuffer;
use crate::property_graph::storage::prepared::PreparedObjects;
use crate::property_graph::storage::tree::directory::{BlockSource, TreeError};
use crate::property_graph::wal::{ArtifactDescriptor, InventoryChange, InventoryState};

/// Finished private objects and their complete candidate, retained with the
/// exact registered base lease. This participant cannot publish or delete.
pub(crate) struct PreparedGraphArtifacts<'a, 'b, S, F> {
    _registration: NativePreparedRegistration,
    inventory: StorageBuffer<'a, InventoryChange>,
    candidate: NativeGraphCandidate<'a>,
    objects: PreparedObjects<'a, 'b, S, F>,
    base: NativeReadLease,
}

impl<
    'a,
    'b,
    S: BlockSource,
    F: FnMut() -> Result<crate::property_graph::storage::artifact::ArtifactIdentity, TreeError>,
> PreparedGraphArtifacts<'a, 'b, S, F>
{
    #[allow(
        clippy::result_large_err,
        reason = "failure must retain the owned packs and lease without an unaccounted box"
    )]
    pub(crate) fn new(
        candidate: NativeGraphCandidate<'a>,
        objects: PreparedObjects<'a, 'b, S, F>,
        base: NativeReadLease,
    ) -> Result<Self, PreparedGraphFailure<'a, 'b, S, F>> {
        let admitted = base.bundle();
        let valid = objects.is_finished()
            && candidate.expected_base() == admitted.base()
            && candidate.expected_sequence() == admitted.sequence()
            && candidate.expected_roots() == admitted.wal_roots()
            && candidate.expected_catalog() == admitted.catalog()
            && candidate.expected_vector() == admitted.vector()
            && candidate.expected_text() == admitted.text()
            && candidate.expected_reclaim() == admitted.reclaim()
            && candidate.expected_high_waters() == admitted.high_waters()
            && candidate.expected_prepared_inventories() == admitted.prepared_inventories()
            && candidate.roots().store() == admitted.base().store
            && candidate.target_generation() == candidate.roots().generation()
            && candidate.target_generation() == objects.generation()
            && objects.store() == admitted.base().store;
        if !valid {
            return Err(PreparedGraphFailure {
                error: TreeError::Invalid("prepared native graph base mismatch"),
                objects,
                base,
            });
        }
        let mut inventory = match StorageBuffer::new(objects.memory(), objects.len()) {
            Ok(inventory) => inventory,
            Err(error) => {
                return Err(PreparedGraphFailure {
                    error,
                    objects,
                    base,
                });
            }
        };
        for index in 0..objects.len() {
            let artifact = match objects.artifact(index) {
                Ok(artifact) => artifact,
                Err(error) => {
                    return Err(PreparedGraphFailure {
                        error,
                        objects,
                        base,
                    });
                }
            };
            let bytes = artifact.bytes();
            let Some(checksum) = bytes
                .len()
                .checked_sub(8)
                .and_then(|offset| bytes.get(offset..))
                .and_then(|trailer| trailer.first_chunk::<8>())
                .copied()
            else {
                return Err(PreparedGraphFailure {
                    error: TreeError::Invalid("prepared object checksum trailer"),
                    objects,
                    base,
                });
            };
            let identity = artifact.identity();
            let descriptor = ArtifactDescriptor {
                store: identity.store,
                artifact: identity.artifact,
                generation: identity.generation,
                serial: identity.creation_serial,
                bytes: match u32::try_from(bytes.len()) {
                    Ok(bytes) => bytes,
                    Err(_) => {
                        return Err(PreparedGraphFailure {
                            error: TreeError::Invalid("prepared object length"),
                            objects,
                            base,
                        });
                    }
                },
                family: FormatFamily::NativeGraphObject.id(),
                version: 1,
                checksum: u64::from_le_bytes(checksum),
            };
            if let Err(error) = inventory.push(InventoryChange {
                object: descriptor,
                state: InventoryState::Prepared,
            }) {
                return Err(PreparedGraphFailure {
                    error,
                    objects,
                    base,
                });
            }
        }
        let registration = match base.register_prepared(inventory.as_slice()) {
            Ok(registration) => registration,
            Err(_) => {
                return Err(PreparedGraphFailure {
                    error: TreeError::Invalid("prepared native graph registration failed"),
                    objects,
                    base,
                });
            }
        };
        Ok(Self {
            _registration: registration,
            inventory,
            candidate,
            objects,
            base,
        })
    }

    pub(crate) const fn candidate(&self) -> &NativeGraphCandidate<'a> {
        &self.candidate
    }

    pub(crate) const fn objects(&self) -> &PreparedObjects<'a, 'b, S, F> {
        &self.objects
    }

    pub(crate) fn abort_inventory(
        &self,
    ) -> impl Iterator<Item = crate::property_graph::storage::artifact::ArtifactIdentity> + '_ {
        self.objects.abort_inventory()
    }

    pub(crate) fn inventory(&self) -> &[InventoryChange] {
        self.inventory.as_slice()
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        NativeGraphCandidate<'a>,
        PreparedObjects<'a, 'b, S, F>,
        NativeReadLease,
    ) {
        (self.candidate, self.objects, self.base)
    }
}

/// Failed handoff retains all private packs and the registered base owner so
/// the writes coordinator can classify and clean its own paths.
pub(crate) struct PreparedGraphFailure<'a, 'b, S, F> {
    error: TreeError,
    objects: PreparedObjects<'a, 'b, S, F>,
    base: NativeReadLease,
}

impl<'a, 'b, S, F> PreparedGraphFailure<'a, 'b, S, F> {
    pub(crate) fn from_preparation(
        error: TreeError,
        objects: PreparedObjects<'a, 'b, S, F>,
        base: NativeReadLease,
    ) -> Self {
        Self {
            error,
            objects,
            base,
        }
    }
    pub(crate) const fn error(&self) -> &TreeError {
        &self.error
    }

    pub(crate) fn into_parts(self) -> (TreeError, PreparedObjects<'a, 'b, S, F>, NativeReadLease) {
        (self.error, self.objects, self.base)
    }
}
