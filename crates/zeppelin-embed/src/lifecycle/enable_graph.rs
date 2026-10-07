//! Explicit, durable manifest-version barrier for unified graph writes.

use std::path::Path;
use std::sync::Arc;

use super::{PublishedSnapshot, Store, StoreError, StoreState};
use crate::manifest::{GraphManifest, GraphObject, ManifestError};
use crate::property_graph::GraphGeneration;
use crate::property_graph::catalog::{
    CatalogDeclaration, CatalogImage, GraphInterpretation, RelationshipRules, SymbolCatalog,
    SymbolHighWaters,
};
use crate::property_graph::storage::allocation::{OsEntropy, artifact_path, fresh_store_identity};
use crate::property_graph::storage::artifact::{
    self, ArtifactId, ArtifactIdentity, Block, BlockKind, ContainerKind,
};
use crate::property_graph::wal::{
    ArtifactDescriptor, CommitState, HighWaters, ReferenceList, RequiredRef, WalGraphRoots,
};
use crate::vfs::{SyncKind, Vfs};

fn invalid(error: impl std::fmt::Display) -> StoreError {
    StoreError::Manifest(ManifestError::Decode(error.to_string()))
}

fn io(path: &Path, source: std::io::Error) -> StoreError {
    StoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

impl Store {
    /// Enables graph storage and returns the resulting visible generation.
    ///
    /// This irreversible format upgrade durably commits manifest v3 before
    /// any graph WAL append is permitted. Older binaries refuse the upgraded
    /// store. Repeated calls are idempotent; read-only handles refuse even an
    /// already enabled store. No document or WAL bytes are changed.
    #[cfg(feature = "graph-cypher")]
    pub fn enable_graph(&self) -> Result<u64, StoreError> {
        let state = self
            .state
            .lock()
            .map_err(|_| StoreError::Synchronization { component: "state" })?;
        match *state {
            StoreState::Open => {}
            StoreState::Closing => return Err(StoreError::Closing),
            StoreState::Closed => return Err(StoreError::Closed),
        }
        let lock = self
            .writer_lock
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "writer lock",
            })?;
        if lock.is_none() {
            return Err(StoreError::ReadOnly);
        }
        let mut wal = self
            .wal_writer
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "WAL writer",
            })?;
        let writer = wal.as_mut().ok_or(StoreError::ReadOnly)?;
        let mut active = self
            .active
            .lock()
            .map_err(|_| StoreError::Synchronization {
                component: "active segment",
            })?;
        let current = active.as_mut().ok_or(StoreError::Closed)?;
        let mut manifest = crate::ingest::load_current_manifest(
            self.vfs.as_ref(),
            &self.directory,
            writer.durable_end(),
            0,
            &self.schema,
        )?;
        if manifest.graph.is_some() {
            if self
                .graph_enable_pending
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                // Only this handle's unfinished upgrade may repair its fence.
                // No WAL mutation was admitted after the failed publication.
                self.vfs
                    .sync(&self.directory, SyncKind::Full)
                    .map_err(|source| io(&self.directory, source))?;
                let path = self.directory.join("wal.ze");
                *writer = if self.vfs.open(&path).map_err(|source| io(&path, source))? == 0 {
                    // The unopened WAL header is pending until the first append,
                    // exactly as on ordinary writable open of an empty store.
                    crate::ingest::StoreWal::create(
                        self.vfs.clone(),
                        &self.directory,
                        &path,
                        self.durability_policy,
                        &self.accounting,
                    )?
                } else {
                    let recovered = crate::wal::WalReader::open(self.vfs.as_ref(), &path)
                        .map_err(StoreError::Wal)?
                        .into_clean()
                        .map_err(StoreError::WalRecovery)?;
                    crate::ingest::StoreWal::resume(
                        self.vfs.as_ref(),
                        &path,
                        recovered,
                        self.durability_policy,
                        manifest.log_seq,
                        &self.accounting,
                    )?
                };
                let publication = writer.manifest_publication()?.arm();
                self.finish_graph_enable(&manifest, current)?;
                publication.complete();
                self.graph_enable_pending
                    .store(false, std::sync::atomic::Ordering::Relaxed);
            } else {
                // Idempotence must not turn a previous publication failure into
                // an acknowledgement on a stale writer.
                let publication = writer.manifest_publication()?;
                self.vfs
                    .sync(&self.directory, SyncKind::Full)
                    .map_err(|source| io(&self.directory, source))?;
                self.native_graph.enable_registries()?;
                if !self.native_graph.is_installed()? {
                    return Err(invalid(
                        "graph enable has no installed bundle; reopen required",
                    ));
                }
                publication.complete();
            }
            return Ok(current.generation);
        }
        self.accounting.enable_graph_ceiling()?;
        let generation = current
            .generation
            .checked_add(1)
            .ok_or(StoreError::GenerationOverflow)?;
        manifest.generation = generation;
        manifest.epochs = self.epoch_registry(&manifest.epochs);
        manifest.epoch_alias = self.epoch_identity();
        manifest.graph = Some(self.empty_graph(generation, writer.durable_end())?);
        manifest
            .record_generation_bump(writer.durable_end())
            .map_err(StoreError::Manifest)?;
        // The version barrier must survive before a future writer can append
        // an op an older binary cannot read, including in Derived stores.
        let barrier = super::durability::DurabilityPolicy::new(
            super::durability::DurabilityMode::Durable,
            super::durability::CommitTier::Durable,
        )
        .map_err(StoreError::Durability)?;
        let mut publication = writer.manifest_publication()?;
        self.graph_enable_pending
            .store(true, std::sync::atomic::Ordering::Relaxed);
        publication
            .commit_manifest(self.vfs.as_ref(), &self.directory, &manifest, barrier)
            .map_err(StoreError::Manifest)?;
        self.finish_graph_enable(&manifest, current)?;
        publication.complete();
        self.graph_enable_pending
            .store(false, std::sync::atomic::Ordering::Relaxed);
        Ok(generation)
    }

    fn finish_graph_enable(
        &self,
        manifest: &crate::manifest::Manifest,
        current: &mut crate::ingest::ActiveState,
    ) -> Result<(), StoreError> {
        self.native_graph.enable_registries()?;
        let remapped = PublishedSnapshot::from_manifest(
            self.vfs.as_ref(),
            &self.directory,
            manifest,
            &self.accounting,
        )?;
        *self
            .snapshot
            .write()
            .map_err(|_| StoreError::Synchronization {
                component: "published snapshot",
            })? = Some(Arc::new(remapped));
        current.generation = manifest.generation;
        let graph = manifest
            .graph
            .as_ref()
            .ok_or_else(|| invalid("graph upgrade section is absent"))?;
        super::native_graph::recovery::install_unified(
            self,
            graph,
            manifest.generation,
            &[],
            super::AccessMode::ReadWrite,
            None,
        )?;
        Ok(())
    }

    fn empty_graph(&self, generation: u64, log_seq: u64) -> Result<GraphManifest, StoreError> {
        let mut entropy = OsEntropy;
        let store =
            fresh_store_identity(&mut entropy).map_err(|source| io(&self.directory, source))?;
        let artifact = ArtifactId::new(
            fresh_store_identity(&mut entropy)
                .map_err(|source| io(&self.directory, source))?
                .get(),
        )
        .map_err(invalid)?;
        let mut checkpoint = || Ok(());
        let image = CatalogImage {
            relationship_rules: RelationshipRules::EMPTY,
            declaration: CatalogDeclaration {
                store,
                node_high_water: 0,
                relationship_high_water: 0,
                interpretation: GraphInterpretation::new(
                    self.tokenizer.epoch(),
                    self.epoch.as_ref().map(|epoch| &epoch.embedding.document),
                )
                .map_err(invalid)?,
            },
            symbols: SymbolCatalog::reconstruct(
                &[],
                SymbolHighWaters::default(),
                0,
                0,
                &mut checkpoint,
            )
            .map_err(invalid)?,
        };
        let length = image.encoded_len(&mut checkpoint).map_err(invalid)?;
        let mut payload = vec![
            0;
            length
                .checked_add(8)
                .ok_or(StoreError::GenerationOverflow)?
        ];
        payload
            .get_mut(..8)
            .ok_or_else(|| invalid("catalog prefix"))?
            .copy_from_slice(b"ZGCP\x01\x00\x01\x00");
        image
            .encode_into(
                payload
                    .get_mut(8..)
                    .ok_or_else(|| invalid("catalog extent"))?,
                &mut checkpoint,
            )
            .map_err(invalid)?;
        let blocks = [Block {
            kind: BlockKind::CommitParticipant,
            payload: &payload,
        }];
        let identity = ArtifactIdentity {
            store,
            artifact,
            generation: GraphGeneration::new(generation),
            creation_serial: 1,
        };
        let mut bytes =
            vec![0; artifact::encoded_len(ContainerKind::Object, &blocks).map_err(invalid)?];
        artifact::encode_into(ContainerKind::Object, identity, &blocks, &mut bytes)
            .map_err(invalid)?;
        let frame = artifact::decode(ContainerKind::Object, Some((store, artifact)), &bytes)
            .map_err(invalid)?;
        let checksum = bytes
            .len()
            .checked_sub(8)
            .and_then(|start| bytes.get(start..))
            .and_then(|bytes| bytes.first_chunk::<8>())
            .copied()
            .map(u64::from_le_bytes)
            .ok_or_else(|| invalid("catalog checksum extent"))?;
        let catalog = RequiredRef {
            object: ArtifactDescriptor {
                store,
                artifact,
                generation: identity.generation,
                serial: 1,
                bytes: u32::try_from(bytes.len()).map_err(invalid)?,
                family: crate::format::FormatFamily::NativeGraphObject.id(),
                version: 1,
                checksum,
            },
            block: frame.reference(0).map_err(invalid)?,
        };
        let graph = GraphManifest::new(
            CommitState {
                store,
                generation: identity.generation,
                sequence: 0,
                graph: WalGraphRoots::default(),
                catalog,
                vector: None,
                text: None,
                reclaim: None,
                // The ZE-38 codec stores consumed high-waters, so zero allocates
                // the first logical id at 1; the catalog consumes physical serial 1.
                high_waters: HighWaters {
                    creation_serial: 1,
                    ..HighWaters::default()
                },
                prepared_inventories: ReferenceList::Values(&[]),
            },
            log_seq,
            vec![GraphObject {
                artifact,
                length: bytes.len() as u64,
                checksum,
            }],
        )
        .map_err(StoreError::Manifest)?;
        let path = artifact_path(&self.directory, artifact);
        self.vfs
            .create_new(&path, &bytes)
            .map_err(|source| io(&path, source))?;
        self.vfs
            .sync(&path, SyncKind::Full)
            .map_err(|source| io(&path, source))?;
        self.vfs
            .sync(&self.directory, SyncKind::Full)
            .map_err(|source| io(&self.directory, source))?;
        Ok(graph)
    }
}

/// Open/remap hook: a v3 manifest must have every immutable object it names.
/// This validates the committed immutable inventory before replay.
pub(super) fn validate_objects(
    vfs: &dyn Vfs,
    directory: &Path,
    graph: &GraphManifest,
) -> Result<(), StoreError> {
    let state = graph.state().map_err(StoreError::Manifest)?;
    for object in &graph.objects {
        let path = artifact_path(directory, object.artifact);
        let bytes = vfs.read(&path).map_err(|source| io(&path, source))?;
        let frame = artifact::decode(
            ContainerKind::Object,
            Some((state.store, object.artifact)),
            &bytes,
        )
        .map_err(invalid)?;
        let checksum = bytes
            .len()
            .checked_sub(8)
            .and_then(|start| bytes.get(start..))
            .and_then(|bytes| bytes.first_chunk::<8>())
            .copied()
            .map(u64::from_le_bytes)
            .ok_or_else(|| invalid("graph checksum extent"))?;
        if bytes.len() as u64 != object.length || checksum != object.checksum {
            return Err(invalid("manifest graph object length or checksum differs"));
        }
        for reference in state
            .graph
            .slots
            .into_iter()
            .flatten()
            .chain(Some(state.catalog))
            .chain(state.reclaim)
        {
            if reference.object.artifact == object.artifact {
                frame.framed_block(reference.block).map_err(invalid)?;
            }
        }
    }
    Ok(())
}
