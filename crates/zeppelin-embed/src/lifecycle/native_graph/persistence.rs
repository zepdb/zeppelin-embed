use super::NativeGraphError;
use crate::epoch::EmbeddingTower;
use crate::format::FormatFamily;
use crate::fts::tokenizer::TokenizerEpoch;
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::{CancelToken, QueryControl};
use crate::lifecycle::{MonotonicClock, OpenOptions, Store, SystemMonotonicClock};
use crate::property_graph::catalog::{
    CatalogDeclaration, CatalogImage, GraphInterpretation, SymbolCatalog, SymbolHighWaters,
};
use crate::property_graph::staging::{WriteLimits, WriteMemory};
use crate::property_graph::storage::allocation::{
    EntropyProvider, OsEntropy, artifact_path, fresh_store_identity,
};
use crate::property_graph::storage::artifact::{
    self, ArtifactId, ArtifactIdentity, Block, BlockKind, ContainerKind,
};
use crate::property_graph::storage::memory::{StorageBuffer, StorageMemory};
use crate::property_graph::wal::{
    ArtifactDescriptor, CommitState, HighWaters, ReferenceList, RequiredRef, WalGraphRoots,
};
use crate::vfs::{SyncKind, Vfs};
use std::path::Path;
use std::sync::Arc;
fn io(path: &Path, source: std::io::Error) -> NativeGraphError {
    NativeGraphError::Io {
        path: path.to_path_buf(),
        source,
    }
}

pub(super) fn artifact_descriptor(
    identity: ArtifactIdentity,
    kind: ContainerKind,
    bytes: &[u8],
) -> Result<ArtifactDescriptor, NativeGraphError> {
    let trailer = bytes
        .len()
        .checked_sub(8)
        .and_then(|offset| bytes.get(offset..))
        .and_then(|value| value.first_chunk::<8>())
        .copied()
        .ok_or(NativeGraphError::Invalid(
            "native artifact checksum trailer",
        ))?;
    Ok(ArtifactDescriptor {
        store: identity.store,
        artifact: identity.artifact,
        generation: identity.generation,
        serial: identity.creation_serial,
        bytes: u32::try_from(bytes.len())
            .map_err(|_| NativeGraphError::Invalid("native artifact length"))?,
        family: match kind {
            ContainerKind::Object => FormatFamily::NativeGraphObject.id(),
            ContainerKind::RootEnvelope => FormatFamily::NativeGraphRoot.id(),
        },
        version: 1,
        checksum: u64::from_le_bytes(trailer),
    })
}

pub(super) fn zeroed<'a>(
    memory: &'a StorageMemory<'a>,
    control: &QueryControl,
    length: usize,
) -> Result<StorageBuffer<'a, u8>, NativeGraphError> {
    let mut bytes = StorageBuffer::new(memory, length)?;
    let zeros = [0_u8; 64 * 1024];
    while bytes.as_slice().len() < length {
        control.checkpoint().map_err(|error| {
            NativeGraphError::Read(
                crate::property_graph::storage::tree::directory::TreeError::Control(error),
            )
        })?;
        let count = (length - bytes.as_slice().len()).min(zeros.len());
        bytes.extend_from_slice(
            zeros
                .get(..count)
                .ok_or(NativeGraphError::Invalid("buffer extent"))?,
        )?;
    }
    Ok(bytes)
}

pub(super) fn encode_framed<'a>(
    memory: &'a StorageMemory<'a>,
    control: &QueryControl,
    kind: ContainerKind,
    identity: ArtifactIdentity,
    blocks: &[Block<'_>],
) -> Result<(StorageBuffer<'a, u8>, RequiredRef), NativeGraphError> {
    let length = artifact::encoded_len(kind, blocks)
        .map_err(|_| NativeGraphError::Invalid("native artifact framing"))?;
    let mut bytes = zeroed(memory, control, length)?;
    let mut poll = |_| {
        control.checkpoint().map_err(|error| {
            NativeGraphError::Read(
                crate::property_graph::storage::tree::directory::TreeError::Control(error),
            )
        })
    };
    let map_error = |error| match error {
        artifact::ArtifactControlError::Control(error) => error,
        artifact::ArtifactControlError::Format(error) => NativeGraphError::Read(
            crate::property_graph::storage::tree::directory::TreeError::Format(error),
        ),
    };
    artifact::encode_into_with_control(kind, identity, blocks, bytes.as_mut_slice(), &mut poll)
        .map_err(map_error)?;
    let frame = artifact::decode_with_control(
        kind,
        Some((identity.store, identity.artifact)),
        bytes.as_slice(),
        &mut poll,
    )
    .map_err(map_error)?;
    let block = frame
        .reference(0)
        .map_err(|_| NativeGraphError::Invalid("native artifact block"))?;
    let object = artifact_descriptor(identity, kind, bytes.as_slice())?;
    Ok((bytes, RequiredRef { object, block }))
}

pub(super) fn next_artifact(
    entropy: &mut dyn EntropyProvider,
) -> Result<ArtifactId, NativeGraphError> {
    let value = fresh_store_identity(entropy)
        .map_err(|source| io(Path::new("native graph entropy"), source))?
        .get();
    ArtifactId::new(value).map_err(|_| NativeGraphError::Invalid("reserved artifact identity"))
}

#[allow(
    clippy::too_many_arguments,
    reason = "independent resource owners and lifetimes are explicit at this private seam"
)]
pub(super) fn catalog_payload<'a>(
    memory: &'a StorageMemory<'a>,
    control: &QueryControl,
    store: crate::property_graph::StoreInstanceId,
    lexical: TokenizerEpoch,
    document: Option<&EmbeddingTower>,
    node_high_water: u128,
    relationship_high_water: u128,
    entries: &[crate::property_graph::catalog::SymbolEntry<'_>],
    symbol_high_waters: SymbolHighWaters,
    relationship_rules: crate::property_graph::catalog::RelationshipRules<'_>,
) -> Result<StorageBuffer<'a, u8>, NativeGraphError> {
    let mut checkpoint = || {
        control
            .checkpoint()
            .map_err(|_| crate::property_graph::catalog::CatalogError::Cancelled)
    };
    let descriptor_bytes = entries
        .len()
        .checked_mul(std::mem::size_of::<
            crate::property_graph::catalog::SymbolEntry<'_>,
        >())
        .ok_or(NativeGraphError::Invalid("catalog descriptor length"))?;
    let _descriptors = memory.reserve(descriptor_bytes)?;
    let symbols = SymbolCatalog::reconstruct(
        entries,
        symbol_high_waters,
        entries.len(),
        descriptor_bytes,
        &mut checkpoint,
    )?;
    let image = CatalogImage {
        relationship_rules,
        declaration: CatalogDeclaration {
            store,
            node_high_water,
            relationship_high_water,
            interpretation: GraphInterpretation::new(lexical, document)?,
        },
        symbols,
    };
    let image_bytes = image.encoded_len(&mut checkpoint)?;
    let total = image_bytes
        .checked_add(8)
        .ok_or(NativeGraphError::Invalid("catalog length"))?;
    let mut payload = zeroed(memory, control, total)?;
    payload
        .as_mut_slice()
        .get_mut(..4)
        .ok_or(NativeGraphError::Invalid("catalog header"))?
        .copy_from_slice(b"ZGCP");
    payload
        .as_mut_slice()
        .get_mut(4..6)
        .ok_or(NativeGraphError::Invalid("catalog role"))?
        .copy_from_slice(&1_u16.to_le_bytes());
    payload
        .as_mut_slice()
        .get_mut(6..8)
        .ok_or(NativeGraphError::Invalid("catalog version"))?
        .copy_from_slice(&1_u16.to_le_bytes());
    image.encode_into(
        payload
            .as_mut_slice()
            .get_mut(8..)
            .ok_or(NativeGraphError::Invalid("catalog payload"))?,
        &mut checkpoint,
    )?;
    Ok(payload)
}

#[allow(
    clippy::too_many_arguments,
    reason = "independent resource owners and lifetimes are explicit at this private seam"
)]
fn empty_catalog<'a>(
    memory: &'a StorageMemory<'a>,
    control: &QueryControl,
    store: crate::property_graph::StoreInstanceId,
    lexical: TokenizerEpoch,
    document: Option<&EmbeddingTower>,
    node_high_water: u128,
    relationship_high_water: u128,
    relationship_rules: crate::property_graph::catalog::RelationshipRules<'_>,
) -> Result<StorageBuffer<'a, u8>, NativeGraphError> {
    catalog_payload(
        memory,
        control,
        store,
        lexical,
        document,
        node_high_water,
        relationship_high_water,
        &[],
        SymbolHighWaters::default(),
        relationship_rules,
    )
}

pub(super) fn write_new(
    vfs: &dyn Vfs,
    directory: &Path,
    path: &Path,
    bytes: &[u8],
    policy: crate::lifecycle::durability::DurabilityPolicy,
) -> Result<(), NativeGraphError> {
    vfs.create_new(path, bytes)
        .map_err(|source| io(path, source))?;
    if let crate::lifecycle::durability::SyncRequirement::Sync(kind) = policy.data_file_sync() {
        vfs.sync(path, kind).map_err(|source| io(path, source))?;
    }
    if let crate::lifecycle::durability::SyncRequirement::Sync(kind) = policy.directory_sync() {
        vfs.sync(directory, kind)
            .map_err(|source| io(directory, source))?;
    }
    Ok(())
}

pub(super) fn write_new_full(
    vfs: &dyn Vfs,
    directory: &Path,
    path: &Path,
    bytes: &[u8],
) -> Result<(), NativeGraphError> {
    vfs.create_new(path, bytes)
        .map_err(|source| io(path, source))?;
    vfs.sync(path, SyncKind::Full)
        .map_err(|source| io(path, source))?;
    vfs.sync(directory, SyncKind::Full)
        .map_err(|source| io(directory, source))
}

pub(super) fn create(
    path: &Path,
    options: OpenOptions,
    document: Option<EmbeddingTower>,
    vfs: Arc<dyn Vfs>,
    clock: Arc<dyn MonotonicClock>,
    entropy: &mut dyn EntropyProvider,
) -> Result<Store, NativeGraphError> {
    create_with_high_waters(
        path,
        options,
        document,
        vfs,
        clock,
        entropy,
        0,
        0,
        crate::property_graph::catalog::RelationshipRules::EMPTY,
    )
}

#[allow(clippy::too_many_arguments)]
fn create_with_high_waters(
    path: &Path,
    options: OpenOptions,
    document: Option<EmbeddingTower>,
    vfs: Arc<dyn Vfs>,
    clock: Arc<dyn MonotonicClock>,
    entropy: &mut dyn EntropyProvider,
    node_high_water: u128,
    relationship_high_water: u128,
    relationship_rules: crate::property_graph::catalog::RelationshipRules<'_>,
) -> Result<Store, NativeGraphError> {
    if options.access_mode != crate::lifecycle::AccessMode::ReadWrite {
        return Err(NativeGraphError::Store(
            crate::lifecycle::StoreError::ReadOnly,
        ));
    }
    vfs.create_directory(path)
        .map_err(|source| io(path, source))?;
    let parent = path
        .parent()
        .ok_or(NativeGraphError::Invalid("store parent"))?;
    vfs.sync(parent, SyncKind::Full)
        .map_err(|source| io(parent, source))?;
    let store = Store::open_graph_with_infrastructure(
        path,
        options,
        document.clone(),
        Arc::clone(&vfs),
        clock,
    )?;
    let shared = crate::property_graph::resources::GraphResources::from_store(&store)?;
    let write_memory = WriteMemory::new(&shared, WriteLimits::default())?;
    let control = QueryControl::Cancel(CancelToken::new());
    let storage = StorageMemory::new(&write_memory, &control, 32 * 1024 * 1024)?;
    let identity = fresh_store_identity(entropy).map_err(|source| io(path, source))?;
    let generation = crate::property_graph::GraphGeneration::new(1);
    let lexical = store.tokenizer.epoch();

    let catalog_identity = ArtifactIdentity {
        store: identity,
        artifact: next_artifact(entropy)?,
        generation,
        creation_serial: 1,
    };
    let catalog_payload = empty_catalog(
        &storage,
        &control,
        identity,
        lexical,
        document.as_ref(),
        node_high_water,
        relationship_high_water,
        relationship_rules,
    )?;
    let (catalog_bytes, catalog) = encode_framed(
        &storage,
        &control,
        ContainerKind::Object,
        catalog_identity,
        &[Block {
            kind: BlockKind::CommitParticipant,
            payload: catalog_payload.as_slice(),
        }],
    )?;
    let catalog_path = artifact_path(path, catalog_identity.artifact);
    write_new_full(vfs.as_ref(), path, &catalog_path, catalog_bytes.as_slice())?;

    let state = CommitState {
        store: identity,
        generation,
        sequence: 0,
        graph: WalGraphRoots::default(),
        catalog,
        vector: None,
        text: None,
        reclaim: None,
        high_waters: HighWaters {
            node: node_high_water,
            relationship: relationship_high_water,
            symbols: [0; 4],
            creation_serial: 1,
        },
        prepared_inventories: ReferenceList::Values(&[]),
    };
    let mut manifest =
        crate::ingest::load_current_manifest(vfs.as_ref(), path, 0, 0, &store.schema)?;
    manifest.generation = generation.get();
    let graph = crate::manifest::GraphManifest::new(
        state,
        0,
        vec![crate::manifest::GraphObject {
            artifact: catalog.object.artifact,
            length: u64::from(catalog.object.bytes),
            checksum: catalog.object.checksum,
        }],
    )
    .map_err(crate::lifecycle::StoreError::Manifest)?;
    manifest.graph = Some(graph);
    manifest
        .record_generation_bump(0)
        .map_err(crate::lifecycle::StoreError::Manifest)?;
    let graph = manifest
        .graph
        .as_ref()
        .ok_or(NativeGraphError::Invalid("initial graph manifest"))?
        .clone();
    let barrier = crate::lifecycle::durability::DurabilityPolicy::new(
        DurabilityMode::Durable,
        CommitTier::Durable,
    )
    .map_err(crate::lifecycle::StoreError::Durability)?;
    let mut publication = store
        .wal_writer
        .lock()
        .map_err(|_| crate::lifecycle::StoreError::Synchronization {
            component: "WAL writer",
        })?
        .as_ref()
        .ok_or(crate::lifecycle::StoreError::ReadOnly)?
        .manifest_publication()?;
    publication
        .commit_manifest(vfs.as_ref(), path, &manifest, barrier)
        .map_err(crate::lifecycle::StoreError::Manifest)?;
    store.native_graph.enable_registries()?;
    let snapshot = crate::lifecycle::PublishedSnapshot::from_manifest(
        vfs.as_ref(),
        path,
        &manifest,
        &store.accounting,
    )?;
    store.publish_snapshot(snapshot)?;
    {
        let mut active =
            store
                .active
                .lock()
                .map_err(|_| crate::lifecycle::StoreError::Synchronization {
                    component: "active segment",
                })?;
        active
            .as_mut()
            .ok_or(crate::lifecycle::StoreError::Closed)?
            .generation = generation.get();
    }
    super::recovery::install_unified(
        &store,
        &graph,
        generation.get(),
        &[],
        crate::lifecycle::AccessMode::ReadWrite,
        document,
    )?;
    publication.complete();

    Ok(store)
}

impl Store {
    pub(crate) fn create_native_graph_with_relationship_types(
        path: &Path,
        options: OpenOptions,
        document: Option<EmbeddingTower>,
        rules: crate::property_graph::catalog::RelationshipRules<'_>,
    ) -> Result<Self, NativeGraphError> {
        create_with_high_waters(
            path,
            options,
            document,
            Arc::new(crate::vfs::StdVfs),
            Arc::new(SystemMonotonicClock),
            &mut OsEntropy,
            0,
            0,
            rules,
        )
    }

    pub(crate) fn create_native_graph(
        path: impl AsRef<Path>,
        options: OpenOptions,
        document: Option<EmbeddingTower>,
    ) -> Result<Self, NativeGraphError> {
        create(
            path.as_ref(),
            options,
            document,
            Arc::new(crate::vfs::StdVfs),
            Arc::new(SystemMonotonicClock),
            &mut OsEntropy,
        )
    }

    #[cfg(any(test, feature = "test-seams"))]
    pub(crate) fn create_native_graph_with_allocator_seed_for_test(
        path: impl AsRef<Path>,
        options: OpenOptions,
        document: Option<EmbeddingTower>,
        first_node: crate::property_graph::NodeId,
        first_relationship: crate::property_graph::RelId,
    ) -> Result<Self, NativeGraphError> {
        let node_high_water = first_node
            .get()
            .checked_sub(1)
            .ok_or(NativeGraphError::Invalid("native node allocator seed"))?;
        let relationship_high_water =
            first_relationship
                .get()
                .checked_sub(1)
                .ok_or(NativeGraphError::Invalid(
                    "native relationship allocator seed",
                ))?;
        create_with_high_waters(
            path.as_ref(),
            options,
            document,
            Arc::new(crate::vfs::StdVfs),
            Arc::new(SystemMonotonicClock),
            &mut OsEntropy,
            node_high_water,
            relationship_high_water,
            crate::property_graph::catalog::RelationshipRules::EMPTY,
        )
    }

    pub(crate) fn open_native_graph(
        path: impl AsRef<Path>,
        options: OpenOptions,
        document: Option<EmbeddingTower>,
    ) -> Result<Self, NativeGraphError> {
        super::recovery::open(
            path.as_ref(),
            options,
            document,
            Arc::new(crate::vfs::StdVfs),
            Arc::new(SystemMonotonicClock),
        )
    }

    #[cfg(any(test, feature = "test-seams"))]
    pub(crate) fn create_native_graph_with_infrastructure(
        path: impl AsRef<Path>,
        options: OpenOptions,
        document: Option<EmbeddingTower>,
        vfs: Arc<dyn Vfs>,
        clock: Arc<dyn MonotonicClock>,
        entropy: &mut dyn EntropyProvider,
    ) -> Result<Self, NativeGraphError> {
        create(path.as_ref(), options, document, vfs, clock, entropy)
    }

    #[cfg(any(test, feature = "test-seams"))]
    pub(crate) fn open_native_graph_with_infrastructure(
        path: impl AsRef<Path>,
        options: OpenOptions,
        document: Option<EmbeddingTower>,
        vfs: Arc<dyn Vfs>,
        clock: Arc<dyn MonotonicClock>,
    ) -> Result<Self, NativeGraphError> {
        super::recovery::open(path.as_ref(), options, document, vfs, clock)
    }
}
