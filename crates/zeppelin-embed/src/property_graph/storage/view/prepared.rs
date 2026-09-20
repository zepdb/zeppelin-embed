//! Scoped handoff for one actual native preparation and its admitted base.

use crate::lifecycle::native_graph::NativeReadLease;
use crate::property_graph::storage::adjacency::NativeGraphCandidate;
use crate::property_graph::storage::prepared::PreparedObjects;
use crate::property_graph::storage::tree::directory::{BlockSource, TreeError};

/// Finished private objects and their complete candidate, retained with the
/// exact registered base lease. This participant cannot publish or delete.
pub(crate) struct PreparedGraphArtifacts<'a, 'b, S, F> {
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
            && candidate.roots().store() == admitted.base().store
            && candidate.roots().generation().get() >= admitted.base().generation.get();
        if !valid {
            return Err(PreparedGraphFailure {
                error: TreeError::Invalid("prepared native graph base mismatch"),
                objects,
                base,
            });
        }
        Ok(Self {
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
    pub(crate) const fn error(&self) -> &TreeError {
        &self.error
    }

    pub(crate) fn into_parts(self) -> (TreeError, PreparedObjects<'a, 'b, S, F>, NativeReadLease) {
        (self.error, self.objects, self.base)
    }
}
