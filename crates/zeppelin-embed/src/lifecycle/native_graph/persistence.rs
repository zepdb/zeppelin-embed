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
#[cfg(any(test, feature = "test-seams"))]
use xxhash_rust::xxh3::xxh3_64;

#[cfg(any(test, feature = "test-seams"))]
pub(super) const ROOT_SELECTOR: &str = "graph-root.ze";
#[cfg(any(test, feature = "test-seams"))]
const ROOT_SELECTOR_MAGIC: &[u8; 8] = b"ZGROOT01";
#[cfg(any(test, feature = "test-seams"))]
pub(super) const ROOT_SELECTOR_BYTES: usize = 120;

#[cfg(any(test, feature = "test-seams"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeStoreClassification {
    Complete {
        store: crate::property_graph::StoreInstanceId,
        root: RequiredRef,
    },
    Incomplete,
    Incompatible,
    Corrupt,
}

fn io(path: &Path, source: std::io::Error) -> NativeGraphError {
    NativeGraphError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(any(test, feature = "test-seams"))]
fn put_descriptor(
    output: &mut [u8],
    descriptor: ArtifactDescriptor,
) -> Result<(), NativeGraphError> {
    if output.len() != 64 {
        return Err(NativeGraphError::Invalid("root selector descriptor length"));
    }
    output
        .get_mut(0..16)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(&descriptor.store.get().to_le_bytes());
    output
        .get_mut(16..32)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(&descriptor.artifact.get().to_le_bytes());
    output
        .get_mut(32..40)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(&descriptor.generation.get().to_le_bytes());
    output
        .get_mut(40..48)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(&descriptor.serial.to_le_bytes());
    output
        .get_mut(48..52)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(&descriptor.bytes.to_le_bytes());
    output
        .get_mut(52..54)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(&descriptor.family.to_le_bytes());
    output
        .get_mut(54..56)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(&descriptor.version.to_le_bytes());
    output
        .get_mut(56..64)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(&descriptor.checksum.to_le_bytes());
    Ok(())
}

#[cfg(any(test, feature = "test-seams"))]
fn encode_root_selector(root: RequiredRef) -> Result<[u8; ROOT_SELECTOR_BYTES], NativeGraphError> {
    let mut output = [0_u8; ROOT_SELECTOR_BYTES];
    output
        .get_mut(0..8)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(ROOT_SELECTOR_MAGIC);
    output
        .get_mut(8..10)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(&1_u16.to_le_bytes());
    output
        .get_mut(10..12)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(&FormatFamily::NativeGraphRoot.id().to_le_bytes());
    output
        .get_mut(12..16)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(&(ROOT_SELECTOR_BYTES as u32).to_le_bytes());
    put_descriptor(
        output
            .get_mut(16..80)
            .ok_or(NativeGraphError::Invalid("selector descriptor extent"))?,
        root.object,
    )?;
    output
        .get_mut(80..96)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(&root.block.artifact.get().to_le_bytes());
    output
        .get_mut(96..104)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(&root.block.offset.to_le_bytes());
    output
        .get_mut(104..108)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(&root.block.length.to_le_bytes());
    output
        .get_mut(108..110)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(&(root.block.kind as u16).to_le_bytes());
    output
        .get_mut(110..112)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(&root.block.version.to_le_bytes());
    let checksum = xxh3_64(
        output
            .get(..112)
            .ok_or(NativeGraphError::Invalid("selector checksum extent"))?,
    );
    output
        .get_mut(112..120)
        .ok_or(NativeGraphError::Invalid("encoded field extent"))?
        .copy_from_slice(&checksum.to_le_bytes());
    Ok(output)
}

#[cfg(any(test, feature = "test-seams"))]
fn u16_at(input: &[u8], start: usize) -> Option<u16> {
    input
        .get(start..start + 2)
        .and_then(|bytes| bytes.first_chunk())
        .copied()
        .map(u16::from_le_bytes)
}

#[cfg(any(test, feature = "test-seams"))]
fn u32_at(input: &[u8], start: usize) -> Option<u32> {
    input
        .get(start..start + 4)
        .and_then(|bytes| bytes.first_chunk())
        .copied()
        .map(u32::from_le_bytes)
}

#[cfg(any(test, feature = "test-seams"))]
fn u64_at(input: &[u8], start: usize) -> Option<u64> {
    input
        .get(start..start + 8)
        .and_then(|bytes| bytes.first_chunk())
        .copied()
        .map(u64::from_le_bytes)
}

#[cfg(any(test, feature = "test-seams"))]
fn u128_at(input: &[u8], start: usize) -> Option<u128> {
    input
        .get(start..start + 16)
        .and_then(|bytes| bytes.first_chunk())
        .copied()
        .map(u128::from_le_bytes)
}

#[cfg(any(test, feature = "test-seams"))]
pub(super) fn decode_root_selector(input: &[u8]) -> Result<RequiredRef, NativeStoreClassification> {
    if input.len() != ROOT_SELECTOR_BYTES || input.get(..8) != Some(ROOT_SELECTOR_MAGIC.as_slice())
    {
        return Err(NativeStoreClassification::Corrupt);
    }
    if u16_at(input, 8) != Some(1) || u16_at(input, 10) != Some(FormatFamily::NativeGraphRoot.id())
    {
        return Err(NativeStoreClassification::Incompatible);
    }
    if u32_at(input, 12) != Some(ROOT_SELECTOR_BYTES as u32)
        || u64_at(input, 112)
            != Some(xxh3_64(
                input.get(..112).ok_or(NativeStoreClassification::Corrupt)?,
            ))
    {
        return Err(NativeStoreClassification::Corrupt);
    }
    let store = crate::property_graph::StoreInstanceId::new(
        u128_at(input, 16).ok_or(NativeStoreClassification::Corrupt)?,
    )
    .map_err(|_| NativeStoreClassification::Corrupt)?;
    let artifact = ArtifactId::new(u128_at(input, 32).ok_or(NativeStoreClassification::Corrupt)?)
        .map_err(|_| NativeStoreClassification::Corrupt)?;
    let generation = crate::property_graph::GraphGeneration::new(
        u64_at(input, 48).ok_or(NativeStoreClassification::Corrupt)?,
    );
    let object = ArtifactDescriptor {
        store,
        artifact,
        generation,
        serial: u64_at(input, 56).ok_or(NativeStoreClassification::Corrupt)?,
        bytes: u32_at(input, 64).ok_or(NativeStoreClassification::Corrupt)?,
        family: u16_at(input, 68).ok_or(NativeStoreClassification::Corrupt)?,
        version: u16_at(input, 70).ok_or(NativeStoreClassification::Corrupt)?,
        checksum: u64_at(input, 72).ok_or(NativeStoreClassification::Corrupt)?,
    };
    let block_artifact =
        ArtifactId::new(u128_at(input, 80).ok_or(NativeStoreClassification::Corrupt)?)
            .map_err(|_| NativeStoreClassification::Corrupt)?;
    let block = crate::property_graph::storage::artifact::PhysicalRef {
        artifact: block_artifact,
        offset: u64_at(input, 96).ok_or(NativeStoreClassification::Corrupt)?,
        length: u32_at(input, 104).ok_or(NativeStoreClassification::Corrupt)?,
        kind: match u16_at(input, 108) {
            Some(value) if value == BlockKind::CheckpointPayload as u16 => {
                BlockKind::CheckpointPayload
            }
            _ => return Err(NativeStoreClassification::Corrupt),
        },
        version: u16_at(input, 110).ok_or(NativeStoreClassification::Corrupt)?,
    };
    if object.family != FormatFamily::NativeGraphRoot.id()
        || object.version != 1
        || object.serial == 0
        || object.artifact != block.artifact
        || block.version != 1
    {
        return Err(NativeStoreClassification::Corrupt);
    }
    Ok(RequiredRef { object, block })
}

#[cfg(any(test, feature = "test-seams"))]
pub(crate) fn classify_native_graph(vfs: &dyn Vfs, path: &Path) -> NativeStoreClassification {
    let selector_path = path.join(ROOT_SELECTOR);
    let selector = match vfs.read(&selector_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return NativeStoreClassification::Incomplete;
        }
        Err(_) => return NativeStoreClassification::Corrupt,
    };
    let root = match decode_root_selector(&selector) {
        Ok(root) => root,
        Err(classification) => return classification,
    };
    let root_path = artifact_path(path, root.object.artifact);
    let bytes = match vfs.read(&root_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return NativeStoreClassification::Incomplete;
        }
        Err(_) => return NativeStoreClassification::Corrupt,
    };
    let descriptor_matches = artifact_descriptor(
        ArtifactIdentity {
            store: root.object.store,
            artifact: root.object.artifact,
            generation: root.object.generation,
            creation_serial: root.object.serial,
        },
        ContainerKind::RootEnvelope,
        &bytes,
    )
    .is_ok_and(|descriptor| descriptor == root.object);
    let block_matches = artifact::decode(
        ContainerKind::RootEnvelope,
        Some((root.object.store, root.object.artifact)),
        &bytes,
    )
    .and_then(|frame| frame.reference(0))
    .is_ok_and(|block| block == root.block);
    if !descriptor_matches || !block_matches {
        return NativeStoreClassification::Corrupt;
    }
    NativeStoreClassification::Complete {
        store: root.object.store,
        root,
    }
}

#[cfg(any(test, feature = "test-seams"))]
pub(super) fn publish_root_selector(
    vfs: &dyn Vfs,
    directory: &Path,
    root: RequiredRef,
) -> Result<(), NativeGraphError> {
    let bytes = encode_root_selector(root)?;
    let temporary = directory.join(format!(
        "graph-root-{:032x}.tmp",
        root.object.artifact.get()
    ));
    let selected = directory.join(ROOT_SELECTOR);
    vfs.create_new(&temporary, &bytes)
        .map_err(|source| io(&temporary, source))?;
    vfs.sync(&temporary, SyncKind::Full)
        .map_err(|source| io(&temporary, source))?;
    vfs.rename(&temporary, &selected)
        .map_err(|source| io(&selected, source))?;
    vfs.sync(directory, SyncKind::Full)
        .map_err(|source| io(directory, source))
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
