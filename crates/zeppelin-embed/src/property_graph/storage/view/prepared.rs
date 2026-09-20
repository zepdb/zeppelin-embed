//! Scoped handoff for one actual native preparation and its admitted base.

use super::preparation_source::{NativePreparationCatalog, NativePreparationSource};
use crate::format::FormatFamily;
use crate::lifecycle::native_graph::{NativePreparedRegistration, NativeReadLease};
use crate::property_graph::storage::adjacency::{
    NativeGraphBase, NativeGraphCandidate, prepare_native_graph,
};
use crate::property_graph::storage::memory::StorageBuffer;
use crate::property_graph::storage::participant::DirectoryBase;
use crate::property_graph::storage::prepared::{PackLimits, PreparedObjects};
use crate::property_graph::storage::search::{
    PreparedMembershipChange, PreparedSparseCandidate, SparseRoots, prepare_sparse,
};
use crate::property_graph::storage::tree::directory::{BlockSource, TreeError, TreeResources};
use crate::property_graph::wal::{
    ArtifactDescriptor, CommitState, InventoryChange, InventoryState, ReferenceList,
};

fn same_admitted_bundle(left: &NativeReadLease, right: &NativeReadLease) -> bool {
    let left = left.bundle();
    let right = right.bundle();
    left.base() == right.base()
        && left.root_envelope() == right.root_envelope()
        && left.roots().store() == right.roots().store()
        && left.roots().generation() == right.roots().generation()
        && left.roots().references() == right.roots().references()
        && left.wal_roots().slots == right.wal_roots().slots
        && left.sequence() == right.sequence()
        && left.catalog() == right.catalog()
        && left.vector() == right.vector()
        && left.text() == right.text()
        && left.reclaim() == right.reclaim()
        && left.high_waters() == right.high_waters()
        && left.prepared_inventories() == right.prepared_inventories()
        && left.lexical() == right.lexical()
        && left.document() == right.document()
}

/// Sole scoped owner of the actual native prepare/finalize sequence. It binds
/// source admission to one cloned complete base lease and returns every pack on
/// any failure; it has no publication, sync, unlink, or retry authority.
pub(crate) struct GraphPreparation<'source, 'lease, 'm, F> {
    objects: PreparedObjects<'m, 'source, NativePreparationSource<'lease, 'm>, F>,
    catalog: NativePreparationCatalog<'source, 'lease, 'm>,
    source: &'source NativePreparationSource<'lease, 'm>,
    base: NativeReadLease,
    analyzer: &'source crate::fts::tokenizer::Analyzer,
}

impl<
    'source,
    'lease,
    'm,
    F: FnMut() -> Result<crate::property_graph::storage::artifact::ArtifactIdentity, TreeError>,
> GraphPreparation<'source, 'lease, 'm, F>
{
    pub(crate) fn new(
        source: &'source NativePreparationSource<'lease, 'm>,
        generation: crate::property_graph::GraphGeneration,
        identity_source: F,
        limits: PackLimits,
        analyzer: &'source crate::fts::tokenizer::Analyzer,
        resources: &mut TreeResources<'_>,
    ) -> Result<Self, TreeError> {
        resources.require_preparation(source.memory())?;
        let base_generation = source.lease().bundle().base().generation;
        if generation.get()
            != base_generation
                .get()
                .checked_add(1)
                .ok_or(TreeError::Invalid("native preparation generation overflow"))?
        {
            return Err(TreeError::Invalid(
                "native preparation target generation mismatch",
            ));
        }
        let catalog = NativePreparationCatalog::open(source, resources)?;
        let base = source.lease().clone();
        if analyzer.epoch() != base.bundle().lexical() {
            return Err(TreeError::Invalid(
                "native preparation analyzer epoch mismatch",
            ));
        }
        let objects = PreparedObjects::new(
            source,
            identity_source,
            base.bundle().base().store,
            generation,
            limits,
            source.memory(),
            resources,
        )?;
        Ok(Self {
            objects,
            catalog,
            source,
            base,
            analyzer,
        })
    }

    #[allow(
        clippy::result_large_err,
        reason = "failure returns the complete owned packs and retained base lease"
    )]
    pub(crate) fn prepare(
        mut self,
        batch: &crate::property_graph::staging::StagedBatch<'_>,
        resources: &mut TreeResources<'_>,
    ) -> Result<
        PreparedGraphArtifacts<'source, 'm, 'source, NativePreparationSource<'lease, 'm>, F>,
        PreparedGraphFailure<'m, 'source, NativePreparationSource<'lease, 'm>, F>,
    > {
        let memory = self.source.memory();
        if !std::ptr::eq(self.objects.memory(), memory) || !self.catalog.owns(self.source) {
            return Err(PreparedGraphFailure::from_preparation(
                TreeError::Invalid("prepared native graph memory owner mismatch"),
                self.objects,
                self.base,
            ));
        }
        let bundle = self.base.bundle();
        let committed = CommitState {
            store: bundle.base().store,
            generation: bundle.base().generation,
            sequence: bundle.sequence(),
            graph: bundle.wal_roots(),
            catalog: bundle.catalog(),
            vector: bundle.vector(),
            text: bundle.text(),
            reclaim: bundle.reclaim(),
            high_waters: bundle.high_waters(),
            prepared_inventories: ReferenceList::Values(bundle.prepared_inventories()),
        };
        let candidate = match prepare_native_graph(
            &mut self.objects,
            batch,
            NativeGraphBase {
                directories: DirectoryBase {
                    identity: bundle.base(),
                    roots: bundle.roots(),
                },
                committed,
            },
            &self.catalog,
            bundle.document(),
            memory,
            resources,
        ) {
            Ok(candidate) => candidate,
            Err(error) => {
                return Err(PreparedGraphFailure::from_preparation(
                    error,
                    self.objects,
                    self.base,
                ));
            }
        };
        let batch_catalog = crate::property_graph::storage::participant::BatchCatalog {
            base: &self.catalog,
            additions: batch.symbols(),
        };
        let sparse = match prepare_sparse(
            &mut self.objects,
            batch,
            &candidate,
            &batch_catalog,
            self.analyzer,
            &self.base,
            memory,
            resources,
        ) {
            Ok(candidate) => candidate,
            Err(error) => {
                return Err(PreparedGraphFailure::from_preparation(
                    error,
                    self.objects,
                    self.base,
                ));
            }
        };
        if let Err(error) = self.objects.finish(resources) {
            return Err(PreparedGraphFailure::from_preparation(
                error,
                self.objects,
                self.base,
            ));
        }
        PreparedGraphArtifacts::new(
            candidate,
            sparse,
            self.objects,
            self.source.lease(),
            self.base,
        )
    }
}

/// Finished private objects and their complete candidate, retained with the
/// exact registered base lease. This participant cannot publish or delete.
pub(crate) struct PreparedGraphArtifacts<'source, 'a, 'b, S, F> {
    _registration: NativePreparedRegistration,
    inventory: StorageBuffer<'a, InventoryChange>,
    candidate: NativeGraphCandidate<'a>,
    sparse: PreparedSparseCandidate<'a>,
    sparse_roots: SparseRoots,
    objects: PreparedObjects<'a, 'b, S, F>,
    base: NativeReadLease,
    _source: std::marker::PhantomData<&'source NativeReadLease>,
}

impl<
    'source,
    'a,
    'b,
    S: BlockSource,
    F: FnMut() -> Result<crate::property_graph::storage::artifact::ArtifactIdentity, TreeError>,
> PreparedGraphArtifacts<'source, 'a, 'b, S, F>
{
    #[allow(
        clippy::result_large_err,
        reason = "failure must retain the owned packs and lease without an unaccounted box"
    )]
    pub(crate) fn new(
        candidate: NativeGraphCandidate<'a>,
        sparse: PreparedSparseCandidate<'a>,
        objects: PreparedObjects<'a, 'b, S, F>,
        source_lease: &'source NativeReadLease,
        base: NativeReadLease,
    ) -> Result<Self, PreparedGraphFailure<'a, 'b, S, F>> {
        let admitted = base.bundle();
        let valid = source_lease.token() == base.token()
            && same_admitted_bundle(source_lease, &base)
            && objects.is_finished()
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
            && objects.store() == admitted.base().store
            && sparse.matches(&candidate, admitted, base.token());
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
        let sparse_roots = match sparse.finalize(inventory.as_slice()) {
            Ok(roots) => roots,
            Err(error) => {
                return Err(PreparedGraphFailure {
                    error,
                    objects,
                    base,
                });
            }
        };
        Ok(Self {
            _registration: registration,
            inventory,
            candidate,
            sparse,
            sparse_roots,
            objects,
            base,
            _source: std::marker::PhantomData,
        })
    }

    pub(crate) const fn candidate(&self) -> &NativeGraphCandidate<'a> {
        &self.candidate
    }

    pub(crate) const fn objects(&self) -> &PreparedObjects<'a, 'b, S, F> {
        &self.objects
    }

    pub(crate) const fn sparse_roots(&self) -> SparseRoots {
        self.sparse_roots
    }

    pub(crate) fn membership_changes(&self) -> &[PreparedMembershipChange] {
        self.sparse.changes()
    }

    pub(crate) fn abort_inventory(
        &self,
    ) -> impl Iterator<Item = crate::property_graph::storage::artifact::ArtifactIdentity> + '_ {
        self.objects.abort_inventory()
    }

    pub(crate) fn inventory(&self) -> &[InventoryChange] {
        self.inventory.as_slice()
    }

    pub(crate) fn expected_root_envelope(&self) -> crate::property_graph::wal::RequiredRef {
        self.base.bundle().root_envelope()
    }

    pub(crate) fn matches_base(&self, lease: &NativeReadLease) -> bool {
        self.base.token() == lease.token() && same_admitted_bundle(&self.base, lease)
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        NativeGraphCandidate<'a>,
        SparseRoots,
        PreparedSparseCandidate<'a>,
        PreparedObjects<'a, 'b, S, F>,
        NativeReadLease,
    ) {
        (
            self.candidate,
            self.sparse_roots,
            self.sparse,
            self.objects,
            self.base,
        )
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
