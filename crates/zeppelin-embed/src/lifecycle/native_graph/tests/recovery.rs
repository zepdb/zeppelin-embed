use super::publication::{FaultPoint, RecordingVfs};
use super::tempfile;
use crate::epoch::{ComputeUnits, EmbeddingRuntime, EmbeddingTower, Normalization};
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use crate::property_graph::catalog::{Symbol, SymbolKind};
use crate::property_graph::query::resources::QueryMemory;
use crate::property_graph::query::runtime::{RuntimeContext, RuntimeLimits};
use crate::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use crate::property_graph::storage::adjacency::RelationshipRow;
use crate::property_graph::storage::search::Modality;
use crate::property_graph::storage::tree::directory::{TreeError, TreeResources};
use crate::property_graph::storage::{
    CursorState, DirectionSelection, GraphReadView, NativeCatalog, NativeQuerySource,
    NativeReadCapability, RelationshipTypeSelection,
};
use crate::property_graph::{
    ApplicationKey, CanonicalContents, CanonicalEmbedding, EntityId, EntityKind, GraphDeleteMode,
    GraphGeneration, GraphName, GraphProperty, GraphRevision, NodeId, NodeRef, PropertyData,
    PropertyValue, RelId, StoreInstanceId,
};
use crate::vfs::{StdVfs, Vfs};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

#[derive(Clone, Debug, Eq, PartialEq)]
struct MixedObservation {
    store: StoreInstanceId,
    generation: GraphGeneration,
    sequence: u64,
    first_revision: u64,
    second_revision: u64,
    rank_property: Vec<u8>,
    text: Vec<u8>,
    vector_bits: Vec<u32>,
    sparse_vector_bits: Vec<u32>,
    text_count: u64,
    vector_count: u64,
    text_membership: [bool; 2],
    vector_membership: [bool; 2],
    relationship: RelationshipRow,
    outgoing: Vec<RelationshipRow>,
    incoming: Vec<RelationshipRow>,
}

struct ObserveRecoveredMixed {
    first: NodeId,
    second: NodeId,
    relationship: RelId,
}

impl super::super::NativeReadConsumer<MixedObservation> for ObserveRecoveredMixed {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<MixedObservation, TreeError> {
        let mut resources = TreeResources::for_query(runtime)?;
        let first = view
            .lookup_node(self.first, &mut resources)?
            .ok_or(TreeError::Invalid("missing recovered first node"))?;
        let second = view
            .lookup_node(self.second, &mut resources)?
            .ok_or(TreeError::Invalid("missing recovered second node"))?;
        let property = match view.expression_symbol(
            SymbolKind::Property,
            GraphName::new("rank").map_err(|_| TreeError::Invalid("property name"))?,
            &mut resources,
        )? {
            Some(Symbol::Property(property)) => property,
            _ => return Err(TreeError::Invalid("missing recovered property symbol")),
        };
        let rank = view
            .node_property(&first, property, &mut resources)?
            .ok_or(TreeError::Invalid("missing recovered property"))?;
        let mut rank_property =
            vec![0_u8; usize::try_from(rank.len()).map_err(|_| TreeError::Memory)?];
        let rank_bytes = rank_property.len();
        if rank.read_at(0, &mut rank_property, &mut resources)? != rank_bytes {
            return Err(TreeError::Invalid("short recovered property"));
        }
        let text = view
            .stored_text(self.first, &mut resources)?
            .ok_or(TreeError::Invalid("missing recovered text"))?;
        let mut text_bytes =
            vec![0_u8; usize::try_from(text.len()).map_err(|_| TreeError::Memory)?];
        let text_length = text_bytes.len();
        if text.read_at(0, &mut text_bytes, &mut resources)? != text_length {
            return Err(TreeError::Invalid("short recovered text"));
        }
        let vector = view
            .vector_payload(self.second, &mut resources)?
            .ok_or(TreeError::Invalid("missing recovered vector"))?;
        let mut vector_bits = Vec::new();
        for index in 0..vector.dimensions() {
            vector_bits.push(vector.coordinate(index, &mut resources)?.to_bits());
        }
        let relationship = view
            .lookup_relationship(self.relationship, &mut resources)?
            .ok_or(TreeError::Invalid("missing recovered relationship"))?
            .row();
        let first_revision = first.record().revision().get();
        let second_revision = second.record().revision().get();
        drop(resources);

        let sparse = view.sparse_view(runtime)?;
        let mut resources = TreeResources::for_query(runtime)?;
        let text_membership = [
            sparse
                .lookup(Modality::Text, self.first, &mut resources)?
                .is_some(),
            sparse
                .lookup(Modality::Text, self.second, &mut resources)?
                .is_some(),
        ];
        let first_vector = sparse
            .lookup(Modality::Vector, self.first, &mut resources)?
            .is_some();
        let vector_member = sparse
            .lookup(Modality::Vector, self.second, &mut resources)?
            .ok_or(TreeError::Invalid("missing recovered sparse vector"))?;
        let stored = vector_member.vector.ok_or(TreeError::Invalid(
            "missing recovered sparse vector payload",
        ))?;
        let mut sparse_vector_bits = Vec::new();
        for index in 0..stored.dimensions() {
            sparse_vector_bits.push(stored.coordinate(index, &mut resources)?.to_bits());
        }
        let text_count = sparse.text_count();
        let vector_count = sparse.vector_count();
        drop(resources);
        drop(sparse);

        let mut outgoing = Vec::new();
        let mut incoming = Vec::new();
        for (node, direction, output) in [
            (self.first, DirectionSelection::Out, &mut outgoing),
            (self.second, DirectionSelection::In, &mut incoming),
        ] {
            let mut cursor =
                view.expansion_cursor(node, direction, RelationshipTypeSelection::All, runtime)?;
            let mut rows = [relationship; 2];
            loop {
                let (count, state) = view.expand(&mut cursor, &mut rows, runtime)?;
                output.extend_from_slice(
                    rows.get(..count)
                        .ok_or(TreeError::Invalid("recovered expansion extent"))?,
                );
                if state == CursorState::Done {
                    break;
                }
            }
        }
        Ok(MixedObservation {
            store: view.store_instance_id(),
            generation: view.generation(),
            sequence: view.sequence(),
            first_revision,
            second_revision,
            rank_property,
            text: text_bytes,
            vector_bits,
            sparse_vector_bits,
            text_count,
            vector_count,
            text_membership,
            vector_membership: [first_vector, true],
            relationship,
            outgoing,
            incoming,
        })
    }
}

fn compare_mixed(
    expected: &MixedObservation,
    actual: &MixedObservation,
) -> Result<(), &'static str> {
    if expected != actual {
        Err("recovered mixed graph differs from committed observation")
    } else {
        Ok(())
    }
}

struct RecoveryPathReceipt {
    key: &'static str,
    fires: u64,
    clean_controls: u64,
}

fn begin_recovery_path() {
    super::publication::reset_verified_faults();
}

fn finish_recovery_path(key: &'static str) -> RecoveryPathReceipt {
    RecoveryPathReceipt {
        key,
        fires: super::publication::take_verified_faults(),
        // Returning proves the case's real clean control and assertions completed.
        clean_controls: 1,
    }
}

fn wal_path(directory: &Path) -> PathBuf {
    std::fs::read_dir(directory)
        .expect("read native graph directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("graph-wal-") && name.ends_with(".ze"))
        })
        .expect("native graph WAL")
}

pub(super) fn file_snapshot(directory: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    std::fs::read_dir(directory)
        .expect("read native graph directory")
        .map(|entry| {
            let path = entry.expect("directory entry").path();
            let bytes = std::fs::read(&path).expect("read native graph file");
            (path, bytes)
        })
        .collect()
}

fn assert_refused_without_vfs_mutation(
    path: &Path,
    options: OpenOptions,
    document: Option<EmbeddingTower>,
    vfs: &Arc<RecordingVfs>,
) {
    vfs.take();
    let before = file_snapshot(path);
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let result = Store::open_native_graph_with_infrastructure(
        path,
        options,
        document,
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
    );
    if let Ok(store) = result {
        store.close().expect("close unexpectedly admitted store");
        panic!("invalid native store was admitted");
    }
    assert_eq!(file_snapshot(path), before);
    assert!(
        vfs.take().is_empty(),
        "refused recovery must issue no VFS mutation call"
    );
}

fn remove_required_and_assert_refused(
    directory: &Path,
    required: crate::property_graph::wal::RequiredRef,
    options: OpenOptions,
    document: Option<EmbeddingTower>,
    vfs: &Arc<RecordingVfs>,
) {
    let path = crate::property_graph::storage::allocation::artifact_path(
        directory,
        required.object.artifact,
    );
    let bytes = std::fs::read(&path).expect("required recovery artifact");
    std::fs::remove_file(&path).expect("remove required recovery artifact");
    assert_refused_without_vfs_mutation(directory, options, document, vfs);
    std::fs::write(path, bytes).expect("restore required recovery artifact");
}

pub(super) fn native_options() -> OpenOptions {
    OpenOptions::new()
        .with_durability(DurabilityMode::Durable, CommitTier::Durable)
        .with_max_resident_bytes(256 * 1024 * 1024)
}

struct ObserveNode {
    node: NodeId,
}

impl super::super::NativeReadConsumer<Option<(u64, u64, u64)>> for ObserveNode {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<Option<(u64, u64, u64)>, TreeError> {
        let mut resources = TreeResources::for_query(runtime)?;
        Ok(view.lookup_node(self.node, &mut resources)?.map(|node| {
            (
                view.generation().get(),
                view.sequence(),
                node.record().revision().get(),
            )
        }))
    }
}

pub(super) fn observe_node(store: &Store, node: NodeId) -> Option<(u64, u64, u64)> {
    store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            8 * 1024 * 1024,
            32,
            ObserveNode { node },
        )
        .expect("observe native node")
}

fn observe_node_with_lease(
    store: &Store,
    lease: &super::super::NativeReadLease,
    node: NodeId,
) -> Option<(u64, u64, u64)> {
    let shared = crate::property_graph::resources::GraphResources::from_store(store)
        .expect("shared graph resources");
    let memory = QueryMemory::new(&shared, 8 * 1024 * 1024).expect("query memory");
    let control = QueryControl::Cancel(CancelToken::new());
    let mut runtime = RuntimeContext::new(lease, &control, &memory, RuntimeLimits::default())
        .expect("retained runtime");
    let capability = NativeReadCapability::admit(lease, &runtime).expect("retained capability");
    let mut resources = TreeResources::for_query(&mut runtime).expect("retained resources");
    let source = NativeQuerySource::new(capability, &resources, 16).expect("retained source");
    let catalog = NativeCatalog::open(&source, &mut resources).expect("retained catalog");
    drop(resources);
    let view = GraphReadView::new(&source, &catalog).expect("retained graph view");
    let mut resources = TreeResources::for_query(&mut runtime).expect("retained node resources");
    view.lookup_node(node, &mut resources)
        .expect("retained node lookup")
        .map(|record| {
            (
                view.generation().get(),
                view.sequence(),
                record.record().revision().get(),
            )
        })
}

struct ObserveRelationship {
    relationship: RelId,
}

impl super::super::NativeReadConsumer<bool> for ObserveRelationship {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<bool, TreeError> {
        let mut resources = TreeResources::for_query(runtime)?;
        Ok(view
            .lookup_relationship(self.relationship, &mut resources)?
            .is_some())
    }
}

pub(super) fn relationship_is_visible(store: &Store, relationship: RelId) -> bool {
    store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            8 * 1024 * 1024,
            32,
            ObserveRelationship { relationship },
        )
        .expect("observe native relationship")
}

fn checkpoint_fault_reopens(parent: &Path, name: &str, point: FaultPoint) {
    let path = parent.join(name);
    let vfs = Arc::new(RecordingVfs::default());
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let store = Store::create_native_graph_with_infrastructure(
        &path,
        native_options(),
        None,
        Arc::clone(&infrastructure),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh checkpoint fault store");
    let image =
        CanonicalContents::node(&mut [], &mut [], None, None).expect("checkpoint fault image");
    let receipt = store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", name)
                    .expect("checkpoint fault key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("checkpoint fault commit");
    let node = match receipt[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("checkpoint fault node domain"),
    };
    vfs.take();
    vfs.arm_fault(point);
    assert!(
        store
            .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
            .is_err()
    );
    vfs.assert_fired_once();
    let selected = checkpoint_from_selected(&path);
    store.close().expect("close checkpoint fault store");
    drop(store);

    vfs.take();
    let reopened = Store::open_native_graph_with_infrastructure(
        &path,
        native_options(),
        None,
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
    )
    .expect("reopen surviving checkpoint authority");
    assert_eq!(observe_node(&reopened, node), Some((1, 1, 1)));
    let admission = reopened
        .admit_native_read()
        .expect("checkpoint fault admission");
    assert_eq!(
        admission.bundle().root_envelope().object.generation,
        selected.state.generation
    );
    drop(admission);
    assert!(
        vfs.take()
            .iter()
            .all(|event| matches!(event, super::publication::DurabilityEvent::OpenAppend(_)))
    );
    reopened
        .close()
        .expect("close recovered checkpoint fault store");
}

fn checkpoint_from_selected(
    directory: &Path,
) -> crate::property_graph::wal::NativeCheckpoint<'static> {
    let selector = std::fs::read(directory.join("graph-root.ze")).expect("selected root");
    let required = super::super::persistence::decode_root_selector(&selector)
        .expect("selected root descriptor");
    let bytes = std::fs::read(crate::property_graph::storage::allocation::artifact_path(
        directory,
        required.object.artifact,
    ))
    .expect("selected checkpoint object");
    let leaked = Box::leak(bytes.into_boxed_slice());
    let frame = crate::property_graph::storage::artifact::decode(
        crate::property_graph::storage::artifact::ContainerKind::RootEnvelope,
        Some((required.object.store, required.object.artifact)),
        leaked,
    )
    .expect("selected checkpoint frame");
    let payload = frame
        .framed_block(required.block)
        .expect("selected checkpoint block")
        .payload();
    let mut cancelled = || false;
    let mut resources = crate::property_graph::wal::WalResources::new(
        16 * 1024 * 1024,
        crate::property_graph::wal::STACK_RESERVATION_BYTES,
        &mut cancelled,
    )
    .expect("checkpoint resources");
    crate::property_graph::wal::decode_checkpoint(payload, &mut resources)
        .expect("selected checkpoint payload")
}

fn historical_checkpoint_root(
    store: &Store,
    directory: &Path,
    wal_identity: u128,
    first_sequence: u64,
) -> (PathBuf, Vec<u8>, crate::property_graph::wal::RequiredRef) {
    let selector = std::fs::read(directory.join("graph-root.ze")).expect("selected root");
    let selected = super::super::persistence::decode_root_selector(&selector)
        .expect("selected root descriptor");
    let checkpoint = checkpoint_from_selected(directory);
    let shared = crate::property_graph::resources::GraphResources::from_store(store)
        .expect("historical checkpoint resources");
    let write = crate::property_graph::staging::WriteMemory::new(
        &shared,
        crate::property_graph::staging::WriteLimits::default(),
    )
    .expect("historical checkpoint write memory");
    let control = QueryControl::Cancel(CancelToken::new());
    let storage = crate::property_graph::storage::memory::StorageMemory::new(
        &write,
        &control,
        32 * 1024 * 1024,
    )
    .expect("historical checkpoint storage memory");
    let capacity = checkpoint
        .state
        .prepared_inventories
        .len()
        .expect("historical inventory count")
        .checked_mul(128)
        .and_then(|bytes| bytes.checked_add(16 * 1024))
        .expect("historical checkpoint capacity");
    let mut payload = super::super::persistence::zeroed(&storage, &control, capacity)
        .expect("historical checkpoint payload");
    let mut cancelled = || false;
    let mut resources = crate::property_graph::wal::WalResources::new(
        u64::try_from(capacity).expect("historical checkpoint work") * 4,
        crate::property_graph::wal::STACK_RESERVATION_BYTES,
        &mut cancelled,
    )
    .expect("historical checkpoint WAL resources");
    let payload_bytes = crate::property_graph::wal::encode_checkpoint(
        crate::property_graph::wal::NativeCheckpoint {
            wal_identity,
            first_sequence,
            applied_sequence: checkpoint.applied_sequence,
            state: checkpoint.state,
        },
        payload.as_mut_slice(),
        &mut resources,
    )
    .expect("encode historical checkpoint");
    let identity = crate::property_graph::storage::artifact::ArtifactIdentity {
        store: selected.object.store,
        artifact: selected.object.artifact,
        generation: selected.object.generation,
        creation_serial: selected.object.serial,
    };
    let (bytes, required) = super::super::persistence::encode_framed(
        &storage,
        &control,
        crate::property_graph::storage::artifact::ContainerKind::RootEnvelope,
        identity,
        &[crate::property_graph::storage::artifact::Block {
            kind: crate::property_graph::storage::artifact::BlockKind::CheckpointPayload,
            payload: payload
                .as_slice()
                .get(..payload_bytes)
                .expect("historical checkpoint payload extent"),
        }],
    )
    .expect("encode historical root");
    (
        crate::property_graph::storage::allocation::artifact_path(
            directory,
            selected.object.artifact,
        ),
        bytes.as_slice().to_vec(),
        required,
    )
}

fn corrupt_first_membership_with_valid_checksums(path: &Path) {
    let mut bytes = std::fs::read(path).expect("read native graph WAL");
    let envelope = crate::property_graph::wal::HEADER_BYTES;
    let payload_length = |at: usize, bytes: &[u8]| {
        u32::from_le_bytes(
            bytes[at + 8..at + 12]
                .try_into()
                .expect("WAL payload length"),
        ) as usize
    };
    let begin_length = payload_length(envelope, &bytes);
    let mutation = envelope + 72 + begin_length;
    assert_eq!(
        u16::from_le_bytes(bytes[mutation + 4..mutation + 6].try_into().unwrap()),
        2
    );
    let mutation_length = payload_length(mutation, &bytes);
    bytes[mutation + 65] ^= 1;
    let checksum = xxhash_rust::xxh3::xxh3_64(&bytes[mutation..mutation + 64 + mutation_length]);
    bytes[mutation + 64 + mutation_length..mutation + 72 + mutation_length]
        .copy_from_slice(&checksum.to_le_bytes());

    let mut commit = mutation + 72 + mutation_length;
    while u16::from_le_bytes(bytes[commit + 4..commit + 6].try_into().unwrap()) != 6 {
        commit += 72 + payload_length(commit, &bytes);
    }
    let digest = xxhash_rust::xxh3::xxh3_64(&bytes[envelope..commit]);
    bytes[commit + 72..commit + 80].copy_from_slice(&digest.to_le_bytes());
    let commit_length = payload_length(commit, &bytes);
    let checksum = xxhash_rust::xxh3::xxh3_64(&bytes[commit..commit + 64 + commit_length]);
    bytes[commit + 64 + commit_length..commit + 72 + commit_length]
        .copy_from_slice(&checksum.to_le_bytes());
    std::fs::write(path, bytes).expect("write semantic WAL corruption");
}

fn repair_wal_record(bytes: &mut [u8], offset: usize) {
    let payload_length = u32::from_le_bytes(
        bytes[offset + 8..offset + 12]
            .try_into()
            .expect("WAL payload length"),
    ) as usize;
    let header_checksum = xxhash_rust::xxh3::xxh3_64(&bytes[offset..offset + 56]);
    bytes[offset + 56..offset + 64].copy_from_slice(&header_checksum.to_le_bytes());
    let checksum = xxhash_rust::xxh3::xxh3_64(&bytes[offset..offset + 64 + payload_length]);
    bytes[offset + 64 + payload_length..offset + 72 + payload_length]
        .copy_from_slice(&checksum.to_le_bytes());
}

fn omit_second_mutation_with_valid_framing(path: &Path) {
    let mut bytes = std::fs::read(path).expect("read native graph WAL");
    let envelope = crate::property_graph::wal::HEADER_BYTES;
    let payload_length = |at: usize, bytes: &[u8]| {
        u32::from_le_bytes(
            bytes[at + 8..at + 12]
                .try_into()
                .expect("WAL payload length"),
        ) as usize
    };
    let mut offset = envelope;
    let mut mutations = Vec::new();
    loop {
        let kind = u16::from_le_bytes(bytes[offset + 4..offset + 6].try_into().unwrap());
        if kind == 2 {
            mutations.push(offset);
        }
        if kind == 6 {
            break;
        }
        offset += 72 + payload_length(offset, &bytes);
    }
    assert_eq!(mutations.len(), 2, "two-node control has two mutations");
    let removed = mutations[1];
    let removed_bytes = 72 + payload_length(removed, &bytes);
    bytes.drain(removed..removed + removed_bytes);

    let begin_payload = envelope + 64;
    let old_count = u32::from_le_bytes(
        bytes[begin_payload + 40..begin_payload + 44]
            .try_into()
            .expect("begin change count"),
    );
    bytes[begin_payload + 40..begin_payload + 44].copy_from_slice(&(old_count - 1).to_le_bytes());
    let old_size = u64::from_le_bytes(
        bytes[begin_payload + 48..begin_payload + 56]
            .try_into()
            .expect("begin envelope size"),
    );
    bytes[begin_payload + 48..begin_payload + 56]
        .copy_from_slice(&(old_size - removed_bytes as u64).to_le_bytes());
    repair_wal_record(&mut bytes, envelope);

    offset = envelope + 72 + payload_length(envelope, &bytes);
    loop {
        let kind = u16::from_le_bytes(bytes[offset + 4..offset + 6].try_into().unwrap());
        if offset >= removed {
            let index = u32::from_le_bytes(bytes[offset + 12..offset + 16].try_into().unwrap());
            bytes[offset + 12..offset + 16].copy_from_slice(&(index - 1).to_le_bytes());
        }
        repair_wal_record(&mut bytes, offset);
        if kind == 6 {
            break;
        }
        offset += 72 + payload_length(offset, &bytes);
    }
    let commit = offset;
    let commit_payload = commit + 64;
    let commit_count = u32::from_le_bytes(
        bytes[commit_payload..commit_payload + 4]
            .try_into()
            .expect("commit change count"),
    );
    bytes[commit_payload..commit_payload + 4].copy_from_slice(&(commit_count - 1).to_le_bytes());
    let aggregate = xxhash_rust::xxh3::xxh3_64(&bytes[envelope..commit]);
    bytes[commit_payload + 8..commit_payload + 16].copy_from_slice(&aggregate.to_le_bytes());
    repair_wal_record(&mut bytes, commit);
    std::fs::write(path, bytes).expect("write omitted-mutation WAL");
}

fn run_ze40_complete_mixed_commit_close_reopen_is_coherent()
-> (MixedObservation, RecoveryPathReceipt) {
    begin_recovery_path();
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let document = EmbeddingTower {
        model_id: "ze40-document".into(),
        model_version: "1".into(),
        weights_digest: vec![0x40, 0xa5],
        dims: 2,
        normalization: Normalization::None,
        prompt_prefix: "doc: ".into(),
        max_tokens: 32,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    let infrastructure: Arc<dyn Vfs> = Arc::new(StdVfs);
    let options = OpenOptions::new()
        .with_durability(DurabilityMode::Durable, CommitTier::Durable)
        .with_max_resident_bytes(256 * 1024 * 1024);
    let store = Store::create_native_graph_with_infrastructure(
        &path,
        options.clone(),
        Some(document.clone()),
        Arc::clone(&infrastructure),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh native store");
    let coordinates = [f32::from_bits(0x3f80_0001), f32::from_bits(0x8000_0000)];
    let receipts = crate::property_graph::with_local_refs(|refs| {
        let embedding = CanonicalEmbedding::new(&document, &coordinates).expect("embedding");
        let mut properties = [GraphProperty::new(
            GraphName::new("rank").expect("property name"),
            PropertyValue::new(PropertyData::I64(7)).expect("property value"),
        )];
        let mut labels = [GraphName::new("Document").expect("label")];
        let first = CanonicalContents::node(&mut labels, &mut properties, Some("first text"), None)
            .expect("first node");
        let second =
            CanonicalContents::node(&mut [], &mut [], None, Some(embedding)).expect("second node");
        let requests = [
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", "a").expect("node key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&first)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", "b").expect("node key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&second)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "app", "ab")
                    .expect("relationship key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).expect("first local node")),
                    target: NodeRef::Local(refs.node(1).expect("second local node")),
                    relationship_type: GraphName::new("LINKS").expect("relationship type"),
                    properties: &[],
                }),
            },
        ];
        store
            .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
            .expect("mixed commit")
    });
    let first = match receipts[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("first receipt domain"),
    };
    let second = match receipts[1].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("second receipt domain"),
    };
    let relationship = match receipts[2].entity {
        EntityId::Relationship(relationship) => relationship,
        EntityId::Node(_) => panic!("relationship receipt domain"),
    };
    let expected = store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            8 * 1024 * 1024,
            32,
            ObserveRecoveredMixed {
                first,
                second,
                relationship,
            },
        )
        .expect("pre-close mixed observation");
    assert_eq!(expected.rank_property, [3_u8, 7, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(expected.text, b"first text");
    assert_eq!(expected.vector_bits, coordinates.map(f32::to_bits));
    assert_eq!(expected.sparse_vector_bits, coordinates.map(f32::to_bits));
    assert_eq!((expected.text_count, expected.vector_count), (1, 1));
    assert_eq!(expected.text_membership, [true, false]);
    assert_eq!(expected.vector_membership, [false, true]);
    assert_eq!(expected.relationship.source, first);
    assert_eq!(expected.relationship.target, second);
    assert_eq!(expected.outgoing, [expected.relationship]);
    assert_eq!(expected.incoming, [expected.relationship]);
    assert_eq!((expected.generation.get(), expected.sequence), (1, 1));
    store.close().expect("close native store");
    drop(store);

    let reopened = Store::open_native_graph_with_infrastructure(
        &path,
        options,
        Some(document),
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
    )
    .expect("reopen native store");
    isolate_recovery_from_foreground_reclaim(&reopened);
    let actual = reopened
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            8 * 1024 * 1024,
            32,
            ObserveRecoveredMixed {
                first,
                second,
                relationship,
            },
        )
        .expect("post-reopen mixed observation");
    let mut missing_reverse = expected.clone();
    missing_reverse.incoming.clear();
    assert!(compare_mixed(&missing_reverse, &actual).is_err());
    let mut wrong_generation = expected.clone();
    wrong_generation.generation = GraphGeneration::new(2);
    assert!(compare_mixed(&wrong_generation, &actual).is_err());
    compare_mixed(&expected, &actual).expect("coherent recovered mixed graph");

    let third = CanonicalContents::node(&mut [], &mut [], None, None).expect("third node");
    let next = reopened
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", "c").expect("third key"),
                revision: GraphRevision::new(1).expect("third revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&third)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("post-recovery write");
    let third_id = match next[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("third receipt domain"),
    };
    assert!(third_id.get() > second.get());
    assert_eq!(next[0].generation.get(), 2);
    reopened.close().expect("close recovered store");
    (
        actual,
        finish_recovery_path("property-graph.recovery.complete-reopen"),
    )
}

#[cfg_attr(test, test)]
fn ze40_complete_mixed_commit_close_reopen_is_coherent() {
    let _ = run_ze40_complete_mixed_commit_close_reopen_is_coherent();
}

fn run_ze40_committed_artifact_and_framing_damage_fail_without_partial_admission()
-> RecoveryPathReceipt {
    begin_recovery_path();
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let document = EmbeddingTower {
        model_id: "ze40-document".into(),
        model_version: "1".into(),
        weights_digest: vec![0x40, 0xa5],
        dims: 2,
        normalization: Normalization::None,
        prompt_prefix: "doc: ".into(),
        max_tokens: 32,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    let options = OpenOptions::new()
        .with_durability(DurabilityMode::Durable, CommitTier::Durable)
        .with_max_resident_bytes(256 * 1024 * 1024);
    let store = Store::create_native_graph_with_infrastructure(
        &path,
        options.clone(),
        Some(document.clone()),
        Arc::new(StdVfs),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh native store");
    let first =
        CanonicalContents::node(&mut [], &mut [], Some("first text"), None).expect("text node");
    let coordinates = [f32::from_bits(0x3f80_0001), f32::from_bits(0x8000_0000)];
    let embedding = CanonicalEmbedding::new(&document, &coordinates).expect("embedding");
    let second =
        CanonicalContents::node(&mut [], &mut [], None, Some(embedding)).expect("vector node");
    crate::property_graph::with_local_refs(|refs| {
        store
            .apply_native_graph(
                &[
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "app", "a").expect("node key"),
                        revision: GraphRevision::new(1).expect("revision"),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&first)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "app", "b").expect("node key"),
                        revision: GraphRevision::new(1).expect("revision"),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&second)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Relationship, "app", "ab")
                            .expect("relationship key"),
                        revision: GraphRevision::new(1).expect("revision"),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Relationship {
                            source: NodeRef::Local(refs.node(0).expect("first local node")),
                            target: NodeRef::Local(refs.node(1).expect("second local node")),
                            relationship_type: GraphName::new("LINKS").expect("relationship type"),
                            properties: &[],
                        }),
                    },
                ],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("mixed commit")
    });
    store.close().expect("close native store");
    drop(store);
    corrupt_first_membership_with_valid_checksums(&wal_path(&path));
    let vfs = Arc::new(RecordingVfs::default());
    assert_refused_without_vfs_mutation(&path, options.clone(), Some(document.clone()), &vfs);

    let omitted_path = parent.path().join("omitted-mutation");
    let store = Store::create_native_graph(&omitted_path, options.clone(), None)
        .expect("fresh omitted-mutation store");
    let image = CanonicalContents::node(&mut [], &mut [], None, None).expect("node image");
    let receipts = store
        .apply_native_graph(
            &[
                StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "app", "omitted-a")
                        .expect("first node key"),
                    revision: GraphRevision::new(1).expect("revision"),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                },
                StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "app", "omitted-b")
                        .expect("second node key"),
                    revision: GraphRevision::new(1).expect("revision"),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                },
            ],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("two-node graph-only commit");
    let omitted_nodes = receipts
        .iter()
        .map(|receipt| match receipt.entity {
            EntityId::Node(node) => node,
            EntityId::Relationship(_) => panic!("omitted control node domain"),
        })
        .collect::<Vec<_>>();
    store.close().expect("close omitted-mutation store");
    drop(store);
    let omitted_wal = wal_path(&omitted_path);
    let clean_wal = std::fs::read(&omitted_wal).expect("clean omitted-mutation WAL");
    omit_second_mutation_with_valid_framing(&omitted_wal);
    assert_refused_without_vfs_mutation(&omitted_path, options.clone(), None, &vfs);
    std::fs::write(&omitted_wal, clean_wal).expect("restore omitted-mutation WAL");
    let clean = Store::open_native_graph(&omitted_path, options.clone(), None)
        .expect("clean omitted-mutation control");
    for node in omitted_nodes {
        assert_eq!(observe_node(&clean, node), Some((1, 1, 1)));
    }
    clean.close().expect("close clean omitted-mutation control");

    let checkpoint_path = parent.path().join("checkpoint-only");
    let store = Store::create_native_graph_with_infrastructure(
        &checkpoint_path,
        options.clone(),
        Some(document.clone()),
        Arc::new(StdVfs),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh checkpoint damage store");
    let checkpoint_image = CanonicalContents::node(&mut [], &mut [], Some("checkpoint text"), None)
        .expect("checkpoint text node");
    store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", "checkpoint-damage")
                    .expect("checkpoint key"),
                revision: GraphRevision::new(1).expect("checkpoint revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&checkpoint_image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("checkpoint damage commit");
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .expect("checkpoint damage cutoff");
    store.close().expect("close checkpoint damage store");
    drop(store);
    let checkpoint = checkpoint_from_selected(&checkpoint_path);
    let mut cancelled = || false;
    let mut resources = crate::property_graph::wal::WalResources::new(
        16 * 1024 * 1024,
        crate::property_graph::wal::STACK_RESERVATION_BYTES,
        &mut cancelled,
    )
    .expect("checkpoint inventory resources");
    let prepared = checkpoint
        .state
        .prepared_inventories
        .get(0, &mut resources)
        .expect("checkpoint prepared inventory");
    let graph = checkpoint
        .state
        .graph
        .slots
        .into_iter()
        .flatten()
        .next()
        .expect("checkpoint native tree");
    let text = checkpoint.state.text.expect("checkpoint sparse text");
    for required in [checkpoint.state.catalog, graph, text, prepared] {
        remove_required_and_assert_refused(
            &checkpoint_path,
            required,
            options.clone(),
            Some(document.clone()),
            &vfs,
        );
    }

    let later_path = parent.path().join("later-envelope");
    let store = Store::create_native_graph(&later_path, options.clone(), None)
        .expect("fresh later-envelope store");
    let image = CanonicalContents::node(&mut [], &mut [], None, None).expect("node image");
    for key in ["first", "second"] {
        store
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "app", key).expect("node key"),
                    revision: GraphRevision::new(1).expect("revision"),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("complete envelope");
    }
    store.close().expect("close later-envelope store");
    drop(store);
    let later_wal = wal_path(&later_path);
    let mut bytes = std::fs::read(&later_wal).expect("later WAL bytes");
    let last = bytes.len().checked_sub(1).expect("complete WAL trailer");
    bytes[last] ^= 1;
    std::fs::write(later_wal, bytes).expect("corrupt later complete envelope");
    assert_refused_without_vfs_mutation(&later_path, options, None, &vfs);
    finish_recovery_path("property-graph.recovery.corrupt-missing-artifact")
}

#[cfg_attr(test, test)]
fn ze40_committed_artifact_and_framing_damage_fail_without_partial_admission() {
    let _ = run_ze40_committed_artifact_and_framing_damage_fail_without_partial_admission();
}

fn run_ze40_open_refuses_unsupported_corrupt_and_incomplete_stores_without_mutation()
-> RecoveryPathReceipt {
    begin_recovery_path();
    let options = native_options();
    let parent = tempfile::tempdir().expect("temporary parent");
    let vfs = Arc::new(RecordingVfs::default());

    let invalid = parent.path().join("invalid");
    std::fs::create_dir(&invalid).expect("invalid native directory");
    std::fs::write(invalid.join(crate::lifecycle::lock::STORE_LOCK_FILE), b"")
        .expect("existing writer lock");
    std::fs::write(invalid.join("graph-root.ze"), b"invalid selector").expect("invalid selector");
    assert_refused_without_vfs_mutation(&invalid, options.clone(), None, &vfs);

    let complete = parent.path().join("complete");
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let store = Store::create_native_graph_with_infrastructure(
        &complete,
        options.clone(),
        None,
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh native store");
    store.close().expect("close native store");
    drop(store);
    let checkpoint = checkpoint_from_selected(&complete);

    let selector = complete.join("graph-root.ze");
    let selector_bytes = std::fs::read(&selector).expect("selector bytes");
    std::fs::remove_file(&selector).expect("remove selected authority");
    assert_refused_without_vfs_mutation(&complete, options.clone(), None, &vfs);
    std::fs::write(&selector, &selector_bytes).expect("restore selected authority");

    let wal = complete.join(format!("graph-wal-{:032x}.ze", checkpoint.wal_identity));
    let wal_bytes = std::fs::read(&wal).expect("selected WAL bytes");
    std::fs::remove_file(&wal).expect("remove selected WAL");
    assert_refused_without_vfs_mutation(&complete, options.clone(), None, &vfs);
    std::fs::write(&wal, wal_bytes).expect("restore selected WAL");

    remove_required_and_assert_refused(
        &complete,
        checkpoint.state.catalog,
        options.clone(),
        None,
        &vfs,
    );

    let root_path = crate::property_graph::storage::allocation::artifact_path(
        &complete,
        super::super::persistence::decode_root_selector(&selector_bytes)
            .expect("selected root")
            .object
            .artifact,
    );
    let root_bytes = std::fs::read(&root_path).expect("selected root bytes");
    let mut corrupt_root = root_bytes.clone();
    let last = corrupt_root.len().checked_sub(1).expect("root trailer");
    corrupt_root[last] ^= 1;
    std::fs::write(&root_path, corrupt_root).expect("corrupt selected root");
    assert_refused_without_vfs_mutation(&complete, options.clone(), None, &vfs);
    std::fs::write(&root_path, root_bytes).expect("restore selected root");

    std::fs::remove_file(complete.join(crate::lifecycle::lock::STORE_LOCK_FILE))
        .expect("remove writer lock");
    assert_refused_without_vfs_mutation(&complete, options.clone(), None, &vfs);

    let documented = parent.path().join("documented");
    let document = EmbeddingTower {
        model_id: "ze40-document".into(),
        model_version: "1".into(),
        weights_digest: vec![0x40, 0xa5],
        dims: 2,
        normalization: Normalization::None,
        prompt_prefix: "doc: ".into(),
        max_tokens: 32,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let store = Store::create_native_graph_with_infrastructure(
        &documented,
        options.clone(),
        Some(document.clone()),
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("document native store");
    store.close().expect("close document native store");
    drop(store);
    let mut wrong_document = document;
    wrong_document.weights_digest = vec![0xff];
    assert_refused_without_vfs_mutation(&documented, options.clone(), Some(wrong_document), &vfs);

    let legacy = parent.path().join("legacy");
    let store = Store::open(&legacy, OpenOptions::new()).expect("create legacy store");
    store.close().expect("close legacy store");
    drop(store);
    assert_refused_without_vfs_mutation(&legacy, options, None, &vfs);
    finish_recovery_path("property-graph.recovery.refusal")
}

#[cfg_attr(test, test)]
fn ze40_open_refuses_unsupported_corrupt_and_incomplete_stores_without_mutation() {
    let _ = run_ze40_open_refuses_unsupported_corrupt_and_incomplete_stores_without_mutation();
}

fn run_ze40_lost_ack_and_stopped_writer_resolve_on_reopen() -> RecoveryPathReceipt {
    begin_recovery_path();
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("lost-ack");
    let vfs = Arc::new(RecordingVfs::default());
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let store = Store::create_native_graph_with_infrastructure(
        &path,
        native_options(),
        None,
        Arc::clone(&infrastructure),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh native store");
    let image =
        CanonicalContents::node(&mut [], &mut [], Some("durable"), None).expect("node image");
    let request = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "app", "lost").expect("node key"),
        revision: GraphRevision::new(1).expect("revision"),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&image)),
    }];
    store
        .native_graph
        .fail_next_publication
        .store(true, Ordering::Release);
    assert!(matches!(
        store.apply_native_graph(&request, &QueryControl::Cancel(CancelToken::new())),
        Err(super::super::NativeGraphError::CommitIndeterminate { .. })
    ));
    assert!(
        !store
            .native_graph
            .fail_next_publication
            .load(Ordering::Acquire)
    );
    assert!(matches!(
        store.apply_native_graph(&request, &QueryControl::Cancel(CancelToken::new())),
        Err(super::super::NativeGraphError::WritesStopped)
    ));
    store.close().expect("close stopped store");
    drop(store);

    let reopened = Store::open_native_graph_with_infrastructure(
        &path,
        native_options(),
        None,
        Arc::clone(&infrastructure),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
    )
    .expect("recover durable lost acknowledgement");
    isolate_recovery_from_foreground_reclaim(&reopened);
    let replay = reopened
        .apply_native_graph(&request, &QueryControl::Cancel(CancelToken::new()))
        .expect("exact retry");
    assert!(replay[0].replayed);
    assert_eq!(replay[0].generation.get(), 1);
    let node = match replay[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("node receipt domain"),
    };
    assert_eq!(observe_node(&reopened, node), Some((1, 1, 1)));
    let altered =
        CanonicalContents::node(&mut [], &mut [], Some("altered"), None).expect("altered node");
    assert!(
        reopened
            .apply_native_graph(
                &[StructuredWrite {
                    image: Some(WriteImage::Node(&altered)),
                    ..request[0]
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .is_err()
    );
    reopened.close().expect("close recovered store");
    drop(reopened);

    let old_path = parent.path().join("append-failed");
    let old = Store::create_native_graph_with_infrastructure(
        &old_path,
        native_options(),
        None,
        Arc::clone(&infrastructure),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh append-failure store");
    vfs.arm_fault(FaultPoint::Append);
    assert!(matches!(
        old.apply_native_graph(&request, &QueryControl::Cancel(CancelToken::new())),
        Err(super::super::NativeGraphError::CommitIndeterminate { .. })
    ));
    vfs.assert_fired_once();
    old.close().expect("close append-failed store");
    drop(old);
    let old = Store::open_native_graph_with_infrastructure(
        &old_path,
        native_options(),
        None,
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
    )
    .expect("reopen old state");
    let committed = old
        .apply_native_graph(&request, &QueryControl::Cancel(CancelToken::new()))
        .expect("clean control commit");
    assert!(!committed[0].replayed);
    assert_eq!(committed[0].generation.get(), 1);
    old.close().expect("close clean control");
    finish_recovery_path("property-graph.recovery.lost-ack")
}

#[cfg_attr(test, test)]
fn ze40_lost_ack_and_stopped_writer_resolve_on_reopen() {
    let _ = run_ze40_lost_ack_and_stopped_writer_resolve_on_reopen();
}

fn run_ze40_incomplete_terminal_append_is_ignored_and_writable_reopen_rotates_before_append()
-> RecoveryPathReceipt {
    begin_recovery_path();
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("torn");
    let vfs = Arc::new(RecordingVfs::default());
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let store = Store::create_native_graph_with_infrastructure(
        &path,
        native_options(),
        None,
        Arc::clone(&infrastructure),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh native store");
    let first_image =
        CanonicalContents::node(&mut [], &mut [], Some("first"), None).expect("first image");
    let first_request = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "app", "first").expect("first key"),
        revision: GraphRevision::new(1).expect("revision"),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&first_image)),
    }];
    let first = store
        .apply_native_graph(&first_request, &QueryControl::Cancel(CancelToken::new()))
        .expect("first durable commit");
    let first_node = match first[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("node receipt domain"),
    };
    let old_wal = wal_path(&path);
    let complete_prefix = std::fs::read(&old_wal).expect("complete WAL prefix");
    let second_image =
        CanonicalContents::node(&mut [], &mut [], Some("second"), None).expect("second image");
    let second_request = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "app", "second").expect("second key"),
        revision: GraphRevision::new(1).expect("revision"),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&second_image)),
    }];
    vfs.arm_fault(FaultPoint::PartialAppend);
    assert!(matches!(
        store.apply_native_graph(&second_request, &QueryControl::Cancel(CancelToken::new())),
        Err(super::super::NativeGraphError::CommitIndeterminate { .. })
    ));
    vfs.assert_fired_once();
    let torn_bytes = std::fs::read(&old_wal).expect("torn WAL");
    assert!(torn_bytes.starts_with(&complete_prefix));
    assert!(torn_bytes.len() > complete_prefix.len());
    store.close().expect("close torn store");
    drop(store);

    let reopened = Store::open_native_graph_with_infrastructure(
        &path,
        native_options(),
        None,
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
    )
    .expect("recover torn tail");
    isolate_recovery_from_foreground_reclaim(&reopened);
    assert_eq!(observe_node(&reopened, first_node), Some((1, 1, 1)));
    assert_eq!(
        std::fs::read(&old_wal).expect("preserved old WAL"),
        torn_bytes
    );
    let rotated = {
        let guard = reopened
            .native_graph
            .writer
            .lock()
            .expect("recovered writer");
        guard
            .as_ref()
            .expect("recovered writer state")
            .wal
            .path
            .clone()
    };
    assert_ne!(rotated, old_wal);
    let second = reopened
        .apply_native_graph(&second_request, &QueryControl::Cancel(CancelToken::new()))
        .expect("post-rotation append");
    assert!(!second[0].replayed);
    assert_eq!(second[0].generation.get(), 2);
    reopened.close().expect("close recovered store");
    finish_recovery_path("property-graph.recovery.torn-tail")
}

#[cfg_attr(test, test)]
fn ze40_incomplete_terminal_append_is_ignored_and_writable_reopen_rotates_before_append() {
    let _ =
        run_ze40_incomplete_terminal_append_is_ignored_and_writable_reopen_rotates_before_append();
}

fn run_ze40_empty_graph_preserves_ids_fences_replays_and_allocation_serials() -> RecoveryPathReceipt
{
    begin_recovery_path();
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("empty-history");
    let store =
        Store::create_native_graph(&path, native_options(), None).expect("fresh native store");
    let node_image = CanonicalContents::node(&mut [], &mut [], None, None).expect("node image");
    let created = crate::property_graph::with_local_refs(|refs| {
        store
            .apply_native_graph(
                &[
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "app", "left")
                            .expect("left key"),
                        revision: GraphRevision::new(1).expect("revision"),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&node_image)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "app", "right")
                            .expect("right key"),
                        revision: GraphRevision::new(1).expect("revision"),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&node_image)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Relationship, "app", "edge")
                            .expect("edge key"),
                        revision: GraphRevision::new(1).expect("revision"),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Relationship {
                            source: NodeRef::Local(refs.node(0).expect("left local")),
                            target: NodeRef::Local(refs.node(1).expect("right local")),
                            relationship_type: GraphName::new("LINKS").expect("type"),
                            properties: &[],
                        }),
                    },
                ],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("create graph")
    });
    let left = match created[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("left domain"),
    };
    let right = match created[1].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("right domain"),
    };
    let edge = match created[2].entity {
        EntityId::Relationship(edge) => edge,
        EntityId::Node(_) => panic!("edge domain"),
    };
    let deleted = [
        StructuredWrite {
            key: ApplicationKey::new(EntityKind::Relationship, "app", "edge").expect("edge key"),
            revision: GraphRevision::new(2).expect("revision"),
            operation: StructuredOperation::Delete(
                EntityId::Relationship(edge),
                GraphDeleteMode::Restrict,
            ),
            image: None,
        },
        StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "app", "left").expect("left key"),
            revision: GraphRevision::new(2).expect("revision"),
            operation: StructuredOperation::Delete(EntityId::Node(left), GraphDeleteMode::Restrict),
            image: None,
        },
        StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "app", "right").expect("right key"),
            revision: GraphRevision::new(2).expect("revision"),
            operation: StructuredOperation::Delete(
                EntityId::Node(right),
                GraphDeleteMode::Restrict,
            ),
            image: None,
        },
    ];
    store
        .apply_native_graph(&deleted, &QueryControl::Cancel(CancelToken::new()))
        .expect("delete complete graph");
    store.close().expect("close empty graph");
    drop(store);

    let reopened = Store::open_native_graph(&path, native_options(), None)
        .expect("reopen empty graph history");
    isolate_recovery_from_foreground_reclaim(&reopened);
    assert_eq!(observe_node(&reopened, left), None);
    assert_eq!(observe_node(&reopened, right), None);
    let before = file_snapshot(&path);
    let replay = reopened
        .apply_native_graph(&deleted, &QueryControl::Cancel(CancelToken::new()))
        .expect("exact delete replay");
    assert!(replay.iter().all(|receipt| receipt.replayed));
    assert_eq!(file_snapshot(&path), before);
    let recreated = reopened
        .apply_native_graph(
            &[
                StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "app", "left").expect("left key"),
                    revision: GraphRevision::new(3).expect("revision"),
                    operation: StructuredOperation::Recreate(
                        GraphRevision::new(2).expect("deletion revision"),
                    ),
                    image: Some(WriteImage::Node(&node_image)),
                },
                StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "app", "right").expect("right key"),
                    revision: GraphRevision::new(3).expect("revision"),
                    operation: StructuredOperation::Recreate(
                        GraphRevision::new(2).expect("deletion revision"),
                    ),
                    image: Some(WriteImage::Node(&node_image)),
                },
            ],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("explicit recreation");
    let recreated_left = match recreated[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("recreated node domain"),
    };
    let recreated_right = match recreated[1].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("recreated node domain"),
    };
    assert!(recreated_left > left);
    assert!(recreated_right > right);
    reopened.close().expect("close recreated graph");

    let detach_path = parent.path().join("detach-checkpoint");
    let detached = Store::create_native_graph(&detach_path, native_options(), None)
        .expect("fresh detach store");
    let receipts = crate::property_graph::with_local_refs(|refs| {
        detached
            .apply_native_graph(
                &[
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "app", "detach-left")
                            .expect("left key"),
                        revision: GraphRevision::new(1).expect("revision"),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&node_image)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "app", "detach-right")
                            .expect("right key"),
                        revision: GraphRevision::new(1).expect("revision"),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&node_image)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Relationship, "app", "detach-edge")
                            .expect("edge key"),
                        revision: GraphRevision::new(1).expect("revision"),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Relationship {
                            source: NodeRef::Local(refs.node(0).expect("left local")),
                            target: NodeRef::Local(refs.node(1).expect("right local")),
                            relationship_type: GraphName::new("LINKS").expect("type"),
                            properties: &[],
                        }),
                    },
                ],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("create detach topology")
    });
    let detach_left = match receipts[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("detach left domain"),
    };
    let detach_right = match receipts[1].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("detach right domain"),
    };
    let detach_edge = match receipts[2].entity {
        EntityId::Relationship(relationship) => relationship,
        EntityId::Node(_) => panic!("detach edge domain"),
    };
    detached
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", "detach-left").expect("left key"),
                revision: GraphRevision::new(2).expect("revision"),
                operation: StructuredOperation::Delete(
                    EntityId::Node(detach_left),
                    GraphDeleteMode::Detach,
                ),
                image: None,
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("detach node");
    assert!(!relationship_is_visible(&detached, detach_edge));
    detached
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .expect("checkpoint detached topology");
    detached.close().expect("close detached topology");
    drop(detached);
    let detached = Store::open_native_graph(&detach_path, native_options(), None)
        .expect("reopen detached checkpoint");
    assert_eq!(observe_node(&detached, detach_left), None);
    assert_eq!(observe_node(&detached, detach_right), Some((2, 2, 1)));
    assert!(!relationship_is_visible(&detached, detach_edge));
    detached.close().expect("close recovered detached topology");
    finish_recovery_path("property-graph.recovery.empty-history")
}

#[cfg_attr(test, test)]
fn ze40_empty_graph_preserves_ids_fences_replays_and_allocation_serials() {
    let _ = run_ze40_empty_graph_preserves_ids_fences_replays_and_allocation_serials();
}

fn run_ze40_checkpoint_cutoff_reopens_exactly_and_retains_required_inventories()
-> RecoveryPathReceipt {
    begin_recovery_path();
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("checkpoint");
    let store =
        Store::create_native_graph(&path, native_options(), None).expect("fresh native store");
    let first_image =
        CanonicalContents::node(&mut [], &mut [], Some("one"), None).expect("first image");
    let first_request = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "app", "checkpoint").expect("key"),
        revision: GraphRevision::new(1).expect("revision"),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&first_image)),
    }];
    let first = store
        .apply_native_graph(&first_request, &QueryControl::Cancel(CancelToken::new()))
        .expect("first commit");
    let node = match first[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("node domain"),
    };
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .expect("explicit checkpoint");
    let selected = checkpoint_from_selected(&path);
    assert_eq!(selected.state.sequence, 1);
    assert_eq!(selected.state.generation.get(), 1);
    assert_eq!(selected.first_sequence, 2);
    let checkpoint_inventories = selected
        .state
        .prepared_inventories
        .len()
        .expect("checkpoint inventory count");
    assert_eq!(checkpoint_inventories, 1);

    let second_image =
        CanonicalContents::node(&mut [], &mut [], Some("two"), None).expect("second image");
    let second_request = [StructuredWrite {
        key: first_request[0].key,
        revision: GraphRevision::new(2).expect("revision"),
        operation: StructuredOperation::Put(EntityId::Node(node)),
        image: Some(WriteImage::Node(&second_image)),
    }];
    store
        .apply_native_graph(&second_request, &QueryControl::Cancel(CancelToken::new()))
        .expect("post-checkpoint commit");
    let expected_protected = store
        .native_graph
        .writer
        .lock()
        .expect("writer state")
        .as_ref()
        .expect("writer")
        .protected
        .len();
    store.close().expect("close checkpoint store");
    drop(store);

    let reopened = Store::open_native_graph(&path, native_options(), None)
        .expect("reopen checkpoint and tail");
    assert_eq!(observe_node(&reopened, node), Some((2, 2, 2)));
    let admission = reopened.admit_native_read().expect("recovered admission");
    assert_eq!(
        admission.bundle().root_envelope().object.generation.get(),
        1
    );
    assert_eq!(admission.bundle().base().generation.get(), 2);
    assert_eq!(admission.bundle().prepared_inventories().len(), 2);
    drop(admission);
    {
        let writer_guard = reopened
            .native_graph
            .writer
            .lock()
            .expect("recovered writer");
        let writer = writer_guard.as_ref().expect("recovered writer state");
        assert_eq!(writer.complete_envelopes, 1);
        assert!(writer.wal.bytes > crate::property_graph::wal::HEADER_BYTES);
        assert_eq!(writer.protected.len(), expected_protected);
    }
    reopened.close().expect("close recovered checkpoint store");

    let threshold_path = parent.path().join("count-threshold");
    let vfs = Arc::new(RecordingVfs::default());
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let threshold = Store::create_native_graph_with_infrastructure(
        &threshold_path,
        native_options(),
        None,
        Arc::clone(&infrastructure),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh count-threshold store");
    isolate_recovery_from_foreground_reclaim(&threshold);
    let image = CanonicalContents::node(&mut [], &mut [], None, None).expect("threshold image");
    let key = ApplicationKey::new(EntityKind::Node, "app", "threshold").expect("threshold key");
    let first = threshold
        .apply_native_graph(
            &[StructuredWrite {
                key,
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("first threshold commit");
    let threshold_node = match first[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("threshold node domain"),
    };
    for revision in 2..=64 {
        threshold
            .apply_native_graph(
                &[StructuredWrite {
                    key,
                    revision: GraphRevision::new(revision).expect("revision"),
                    operation: StructuredOperation::Put(EntityId::Node(threshold_node)),
                    image: Some(WriteImage::Node(&image)),
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("count-threshold envelope");
    }
    let retained = threshold
        .admit_native_read()
        .expect("retained generation 64");
    assert_eq!(retained.bundle().prepared_inventories().len(), 64);
    vfs.take();
    assert!(
        threshold
            .apply_native_graph(&[], &QueryControl::Cancel(CancelToken::new()))
            .expect("threshold no-op")
            .is_empty()
    );
    assert!(
        threshold
            .apply_native_graph(
                &[StructuredWrite {
                    key,
                    revision: GraphRevision::new(64).expect("revision"),
                    operation: StructuredOperation::Put(EntityId::Node(threshold_node)),
                    image: Some(WriteImage::Node(&image)),
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("threshold replay")[0]
            .replayed
    );
    assert!(vfs.take().is_empty());
    threshold
        .apply_native_graph(
            &[StructuredWrite {
                key,
                revision: GraphRevision::new(65).expect("revision"),
                operation: StructuredOperation::Put(EntityId::Node(threshold_node)),
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("checkpoint and post-cutoff tail");
    assert_eq!(
        observe_node_with_lease(&threshold, &retained, threshold_node),
        Some((64, 64, 64))
    );
    drop(retained);
    let current = threshold
        .admit_native_read()
        .expect("generation 65 admission");
    assert_eq!(current.bundle().root_envelope().object.generation.get(), 64);
    assert_eq!(current.bundle().base().generation.get(), 65);
    assert_eq!(current.bundle().prepared_inventories().len(), 65);
    drop(current);
    let (threshold_count, threshold_bytes, threshold_protected) = {
        let guard = threshold
            .native_graph
            .writer
            .lock()
            .expect("threshold writer");
        let writer = guard.as_ref().expect("threshold writer state");
        (
            writer.complete_envelopes,
            writer.wal.bytes,
            writer.protected.len(),
        )
    };
    assert_eq!(threshold_count, 1);
    threshold.close().expect("close count-threshold store");
    drop(threshold);
    let threshold = Store::open_native_graph_with_infrastructure(
        &threshold_path,
        native_options(),
        None,
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
    )
    .expect("reopen count-threshold store");
    isolate_recovery_from_foreground_reclaim(&threshold);
    assert_eq!(observe_node(&threshold, threshold_node), Some((65, 65, 65)));
    {
        let guard = threshold
            .native_graph
            .writer
            .lock()
            .expect("recovered threshold writer");
        let writer = guard.as_ref().expect("recovered threshold writer state");
        assert_eq!(writer.complete_envelopes, threshold_count);
        assert_eq!(writer.wal.bytes, threshold_bytes);
        assert_eq!(writer.protected.len(), threshold_protected);
    }
    threshold
        .close()
        .expect("close recovered count-threshold store");

    let byte_path = parent.path().join("byte-threshold");
    let byte_store = Store::create_native_graph(&byte_path, native_options(), None)
        .expect("fresh byte-threshold store");
    let long_key = "k".repeat(512 * 1024);
    let byte_key =
        ApplicationKey::new(EntityKind::Node, "app", &long_key).expect("large threshold key");
    let mut byte_node = None;
    let mut expected_writer = None;
    for revision in 1..=40 {
        let old_identity = {
            let guard = byte_store.native_graph.writer.lock().expect("byte writer");
            guard.as_ref().expect("byte writer state").wal.identity
        };
        let receipt = byte_store
            .apply_native_graph(
                &[StructuredWrite {
                    key: byte_key,
                    revision: GraphRevision::new(revision).expect("revision"),
                    operation: byte_node.map_or(StructuredOperation::Create, |node| {
                        StructuredOperation::Put(EntityId::Node(node))
                    }),
                    image: Some(WriteImage::Node(&image)),
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("byte-threshold commit");
        if byte_node.is_none() {
            byte_node = match receipt[0].entity {
                EntityId::Node(node) => Some(node),
                EntityId::Relationship(_) => panic!("byte threshold node domain"),
            };
        }
        let guard = byte_store.native_graph.writer.lock().expect("byte writer");
        let writer = guard.as_ref().expect("byte writer state");
        if writer.wal.identity != old_identity {
            assert_eq!(writer.complete_envelopes, 1);
            expected_writer = Some((
                writer.complete_envelopes,
                writer.wal.bytes,
                writer.protected.len(),
            ));
            break;
        }
    }
    let expected_writer = expected_writer.expect("encoded-byte checkpoint threshold");
    let byte_node = byte_node.expect("byte threshold node");
    let byte_generation = byte_store
        .admit_native_read()
        .expect("byte threshold admission")
        .bundle()
        .base()
        .generation;
    byte_store.close().expect("close byte-threshold store");
    drop(byte_store);
    let byte_store = Store::open_native_graph(&byte_path, native_options(), None)
        .expect("reopen byte-threshold store");
    assert_eq!(
        observe_node(&byte_store, byte_node),
        Some((
            byte_generation.get(),
            byte_generation.get(),
            byte_generation.get()
        ))
    );
    {
        let guard = byte_store
            .native_graph
            .writer
            .lock()
            .expect("recovered byte writer");
        let writer = guard.as_ref().expect("recovered byte writer state");
        assert_eq!(
            (
                writer.complete_envelopes,
                writer.wal.bytes,
                writer.protected.len()
            ),
            expected_writer
        );
    }
    byte_store
        .close()
        .expect("close recovered byte-threshold store");

    let historical_path = parent.path().join("historical-prefix");
    let historical = Store::create_native_graph(&historical_path, native_options(), None)
        .expect("fresh historical-prefix store");
    let historical_wal = {
        let guard = historical
            .native_graph
            .writer
            .lock()
            .expect("historical writer");
        let writer = guard.as_ref().expect("historical writer state");
        (
            writer.wal.identity,
            writer.wal.first_sequence,
            writer.wal.path.clone(),
        )
    };
    let historical_image =
        CanonicalContents::node(&mut [], &mut [], Some("history"), None).expect("historical image");
    let historical_key =
        ApplicationKey::new(EntityKind::Node, "app", "historical").expect("historical key");
    let receipt = historical
        .apply_native_graph(
            &[StructuredWrite {
                key: historical_key,
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&historical_image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("historical-prefix commit");
    let historical_node = match receipt[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("historical node domain"),
    };
    historical
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .expect("historical-prefix checkpoint");
    let (root_path, root_bytes, root) = historical_checkpoint_root(
        &historical,
        &historical_path,
        historical_wal.0,
        historical_wal.1,
    );
    historical.close().expect("close historical-prefix store");
    drop(historical);
    assert!(
        std::fs::metadata(&historical_wal.2)
            .expect("historical WAL metadata")
            .len()
            > crate::property_graph::wal::HEADER_BYTES as u64
    );
    std::fs::write(&root_path, root_bytes).expect("install historical-prefix checkpoint root");
    super::super::persistence::publish_root_selector(&StdVfs, &historical_path, root)
        .expect("select historical-prefix checkpoint");
    let selected = checkpoint_from_selected(&historical_path);
    assert_eq!(selected.wal_identity, historical_wal.0);
    assert_eq!(selected.first_sequence, historical_wal.1);
    assert_eq!(selected.state.sequence, 1);

    let historical = Store::open_native_graph(&historical_path, native_options(), None)
        .expect("reopen retained historical WAL prefix");
    isolate_recovery_from_foreground_reclaim(&historical);
    assert_eq!(observe_node(&historical, historical_node), Some((1, 1, 1)));
    {
        let guard = historical
            .native_graph
            .writer
            .lock()
            .expect("recovered historical writer");
        let writer = guard.as_ref().expect("recovered historical writer state");
        assert_eq!(writer.wal.identity, historical_wal.0);
        assert_eq!(writer.wal.first_sequence, historical_wal.1);
        assert_eq!(writer.complete_envelopes, 0);
    }
    historical
        .apply_native_graph(
            &[StructuredWrite {
                key: historical_key,
                revision: GraphRevision::new(2).expect("revision"),
                operation: StructuredOperation::Put(EntityId::Node(historical_node)),
                image: Some(WriteImage::Node(&historical_image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("append after retained historical WAL prefix");
    assert_eq!(observe_node(&historical, historical_node), Some((2, 2, 2)));
    historical
        .close()
        .expect("close retained historical-prefix store");

    for (name, point) in [
        ("fault-object-sync", FaultPoint::ObjectSync),
        ("fault-selector-replace", FaultPoint::Rename),
        ("fault-selector-sync", FaultPoint::SelectorSync),
    ] {
        checkpoint_fault_reopens(parent.path(), name, point);
    }
    finish_recovery_path("property-graph.recovery.checkpoint")
}

#[cfg_attr(test, test)]
fn ze40_checkpoint_cutoff_reopens_exactly_and_retains_required_inventories() {
    let _ = run_ze40_checkpoint_cutoff_reopens_exactly_and_retains_required_inventories();
}

fn run_ze40_read_only_replay_preserves_tail_and_all_files() -> RecoveryPathReceipt {
    begin_recovery_path();
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("read-only");
    let vfs = Arc::new(RecordingVfs::default());
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let store = Store::create_native_graph_with_infrastructure(
        &path,
        native_options(),
        None,
        Arc::clone(&infrastructure),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh native store");
    let first_image = CanonicalContents::node(&mut [], &mut [], None, None).expect("first image");
    let first_request = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "app", "visible").expect("key"),
        revision: GraphRevision::new(1).expect("revision"),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&first_image)),
    }];
    let receipt = store
        .apply_native_graph(&first_request, &QueryControl::Cancel(CancelToken::new()))
        .expect("durable first commit");
    let node = match receipt[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("node domain"),
    };
    let torn_image = CanonicalContents::node(&mut [], &mut [], None, None).expect("torn image");
    let torn_request = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "app", "torn").expect("key"),
        revision: GraphRevision::new(1).expect("revision"),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&torn_image)),
    }];
    vfs.arm_fault(FaultPoint::PartialAppend);
    assert!(
        store
            .apply_native_graph(&torn_request, &QueryControl::Cancel(CancelToken::new()))
            .is_err()
    );
    vfs.assert_fired_once();
    store.close().expect("close torn store");
    drop(store);
    let before = file_snapshot(&path);
    vfs.take();
    let read_options = OpenOptions::read_only()
        .with_durability(DurabilityMode::Durable, CommitTier::Durable)
        .with_max_resident_bytes(256 * 1024 * 1024);
    let first = Store::open_native_graph_with_infrastructure(
        &path,
        read_options.clone(),
        None,
        Arc::clone(&infrastructure),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
    )
    .expect("first shared read-only recovery");
    let second = Store::open_native_graph_with_infrastructure(
        &path,
        read_options,
        None,
        Arc::clone(&infrastructure),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
    )
    .expect("second shared read-only recovery");
    assert_eq!(observe_node(&first, node), Some((1, 1, 1)));
    assert_eq!(observe_node(&second, node), Some((1, 1, 1)));
    assert!(matches!(
        first.apply_native_graph(&[], &QueryControl::Cancel(CancelToken::new())),
        Err(super::super::NativeGraphError::Store(
            crate::lifecycle::StoreError::ReadOnly
        ))
    ));
    assert!(matches!(
        first.checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new())),
        Err(super::super::NativeGraphError::Store(
            crate::lifecycle::StoreError::ReadOnly
        ))
    ));
    assert_eq!(file_snapshot(&path), before);
    first.close().expect("close first read-only store");
    second.close().expect("close second read-only store");
    drop(first);
    drop(second);
    assert_eq!(file_snapshot(&path), before);
    assert!(
        vfs.take().is_empty(),
        "read-only recovery must issue no VFS mutation call"
    );

    let writable = Store::open_native_graph_with_infrastructure(
        &path,
        native_options(),
        None,
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
    )
    .expect("writable rotation after read-only close");
    isolate_recovery_from_foreground_reclaim(&writable);
    assert_eq!(observe_node(&writable, node), Some((1, 1, 1)));
    assert_eq!(
        writable
            .apply_native_graph(&torn_request, &QueryControl::Cancel(CancelToken::new()))
            .expect("post-rotation write")[0]
            .generation
            .get(),
        2
    );
    writable.close().expect("close writable store");
    finish_recovery_path("property-graph.recovery.read-only")
}

#[cfg_attr(test, test)]
fn ze40_read_only_replay_preserves_tail_and_all_files() {
    let _ = run_ze40_read_only_replay_preserves_tail_and_all_files();
}

fn rewrite_artifact_identity(bytes: &mut [u8], artifact: u128, serial: u64) {
    bytes[48..64].copy_from_slice(&artifact.to_le_bytes());
    bytes[88..96].copy_from_slice(&serial.to_le_bytes());
    let trailer = bytes.len() - 8;
    let checksum = xxhash_rust::xxh3::xxh3_64(&bytes[..trailer]);
    bytes[trailer..].copy_from_slice(&checksum.to_le_bytes());
}

fn run_ze40_serial_scan_preserves_pre_wal_orphans_and_refuses_ambiguous_corruption()
-> RecoveryPathReceipt {
    begin_recovery_path();
    let parent = tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("serials");
    let store =
        Store::create_native_graph(&path, native_options(), None).expect("fresh native store");
    let image = CanonicalContents::node(&mut [], &mut [], None, None).expect("node image");
    let request = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "app", "serial").expect("key"),
        revision: GraphRevision::new(1).expect("revision"),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&image)),
    }];
    store
        .apply_native_graph(&request, &QueryControl::Cancel(CancelToken::new()))
        .expect("durable commit");
    store.close().expect("close native store");
    drop(store);
    let source = std::fs::read_dir(&path)
        .expect("native directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|candidate| {
            candidate
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("graph-") && name.ends_with(".zgraph"))
                && std::fs::read(candidate).is_ok_and(|bytes| {
                    bytes
                        .get(8..10)
                        .is_some_and(|family| family == 17_u16.to_le_bytes())
                })
        })
        .expect("native object artifact");
    let original = std::fs::read(&source).expect("source artifact");
    let high_artifact = (1_u128 << 124) + 0x40;
    let mut high = original.clone();
    rewrite_artifact_identity(&mut high, high_artifact, 10_000);
    let high_path = path.join(format!("graph-{high_artifact:032x}.zgraph"));
    std::fs::write(&high_path, high).expect("complete higher-serial orphan");
    let unknown = path.join("unknown.keep");
    std::fs::write(&unknown, b"keep").expect("unknown file");

    let vfs = Arc::new(RecordingVfs::default());
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let reopened = Store::open_native_graph_with_infrastructure(
        &path,
        native_options(),
        None,
        Arc::clone(&infrastructure),
        Arc::new(crate::lifecycle::SystemMonotonicClock),
    )
    .expect("recover with complete orphan");
    assert_eq!(
        reopened.native_graph.serial_fence().expect("serial fence"),
        10_000
    );
    assert_eq!(vfs.enumeration_calls().0, 0);
    assert!(vfs.enumeration_calls().1 >= 1);
    // Inject the interrupted object creation, after the new mandatory fold.
    reopened
        .set_native_graph_maintenance_policy(crate::property_graph::GraphMaintenancePolicy {
            automatic: false,
            ..Default::default()
        })
        .unwrap();
    reopened
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .unwrap();
    let before_partial = file_snapshot(&path);
    vfs.arm_fault(FaultPoint::PartialCreate);
    assert!(
        reopened
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "app", "partial-create")
                        .expect("partial key"),
                    ..request[0]
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .is_err()
    );
    vfs.assert_fired_once();
    reopened.close().expect("close orphan store");
    drop(reopened);
    assert_eq!(std::fs::read(&unknown).expect("unknown retained"), b"keep");
    assert!(high_path.exists());
    let after_partial = file_snapshot(&path);
    let (partial_path, partial_bytes) = after_partial
        .iter()
        .find(|(candidate, _)| !before_partial.contains_key(*candidate))
        .map(|(path, bytes)| (path.clone(), bytes.clone()))
        .expect("real partial create artifact");
    assert!(partial_bytes.len() >= crate::property_graph::storage::artifact::HEADER_BYTES);
    let partial_serial = u64::from_le_bytes(
        partial_bytes
            .get(88..96)
            .and_then(|bytes| bytes.first_chunk::<8>())
            .copied()
            .expect("partial creation serial"),
    );
    assert!(partial_serial > 10_000);

    let ambiguous_artifact = (1_u128 << 124) + 0x41;
    let ambiguous_path = path.join(format!("graph-{ambiguous_artifact:032x}.zgraph"));
    std::fs::write(&ambiguous_path, &original).expect("ambiguous complete object");
    assert!(
        Store::open_native_graph_with_infrastructure(
            &path,
            native_options(),
            None,
            Arc::clone(&infrastructure),
            Arc::new(crate::lifecycle::SystemMonotonicClock),
        )
        .is_err()
    );
    std::fs::remove_file(&ambiguous_path).expect("remove test corruption");

    let duplicate_artifact = (1_u128 << 124) + 0x42;
    let original_serial = u64::from_le_bytes(
        original
            .get(88..96)
            .and_then(|bytes| bytes.first_chunk::<8>())
            .copied()
            .expect("original creation serial"),
    );
    let mut duplicate = original.clone();
    rewrite_artifact_identity(&mut duplicate, duplicate_artifact, original_serial);
    let duplicate_path = path.join(format!("graph-{duplicate_artifact:032x}.zgraph"));
    std::fs::write(&duplicate_path, duplicate).expect("duplicate serial object");
    assert!(
        Store::open_native_graph_with_infrastructure(
            &path,
            native_options(),
            None,
            Arc::clone(&infrastructure),
            Arc::new(crate::lifecycle::SystemMonotonicClock),
        )
        .is_err()
    );
    std::fs::remove_file(&duplicate_path).expect("remove duplicate serial object");

    let overflow_artifact = (1_u128 << 124) + 0x43;
    let mut overflow = original;
    rewrite_artifact_identity(&mut overflow, overflow_artifact, u64::MAX);
    overflow.truncate(overflow.len() / 2);
    let overflow_path = path.join(format!("graph-{overflow_artifact:032x}.zgraph"));
    std::fs::write(&overflow_path, overflow).expect("overflow partial object");
    assert!(matches!(
        Store::open_native_graph_with_infrastructure(
            &path,
            native_options(),
            None,
            Arc::clone(&infrastructure),
            Arc::new(crate::lifecycle::SystemMonotonicClock),
        ),
        Err(super::super::NativeGraphError::IdentityExhausted)
    ));
    std::fs::remove_file(&overflow_path).expect("remove overflow object");

    let reopened = Store::open_native_graph_with_infrastructure(
        &path,
        native_options(),
        None,
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
    )
    .expect("classify partial pre-WAL object");
    isolate_recovery_from_foreground_reclaim(&reopened);
    assert_eq!(
        reopened.native_graph.serial_fence().expect("serial fence"),
        partial_serial
    );
    reopened
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", "after-orphan")
                    .expect("next key"),
                ..request[0]
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("write above partial serial");
    assert!(reopened.native_graph.serial_fence().expect("serial fence") > partial_serial);
    reopened.close().expect("close serial store");
    assert!(partial_path.exists());
    assert!(high_path.exists());
    assert!(unknown.exists());
    finish_recovery_path("property-graph.recovery.serial-orphan")
}

#[cfg_attr(test, test)]
fn ze40_serial_scan_preserves_pre_wal_orphans_and_refuses_ambiguous_corruption() {
    let _ = run_ze40_serial_scan_preserves_pre_wal_orphans_and_refuses_ambiguous_corruption();
}

#[cfg(feature = "test-seams")]
pub(super) fn run_actual_probe(
    _seed: u64,
) -> crate::graph_recovery_test_support::RecoveryProbeReport {
    use crate::graph_read_view_test_support::ObservedRelationship;
    use crate::graph_recovery_test_support::{RecoveryProbeReport, RecoveryState};

    let (mixed, complete) = run_ze40_complete_mixed_commit_close_reopen_is_coherent();
    let observed = [
        complete,
        run_ze40_open_refuses_unsupported_corrupt_and_incomplete_stores_without_mutation(),
        run_ze40_lost_ack_and_stopped_writer_resolve_on_reopen(),
        run_ze40_incomplete_terminal_append_is_ignored_and_writable_reopen_rotates_before_append(),
        run_ze40_committed_artifact_and_framing_damage_fail_without_partial_admission(),
        run_ze40_empty_graph_preserves_ids_fences_replays_and_allocation_serials(),
        run_ze40_serial_scan_preserves_pre_wal_orphans_and_refuses_ambiguous_corruption(),
        run_ze40_checkpoint_cutoff_reopens_exactly_and_retains_required_inventories(),
        run_ze40_read_only_replay_preserves_tail_and_all_files(),
    ];
    let receipts = observed
        .into_iter()
        .map(|receipt| crate::graph_read_view_test_support::PathReceipt {
            key: receipt.key,
            fires: receipt.fires,
            clean_controls: receipt.clean_controls,
        })
        .collect();
    let relationship = ObservedRelationship {
        rel: mixed.relationship.rel.get(),
        source: mixed.relationship.source.get(),
        target: mixed.relationship.target.get(),
        relationship_type: mixed.relationship.relationship_type.get(),
    };
    RecoveryProbeReport {
        receipts,
        state: RecoveryState {
            store: mixed.store.get(),
            generation: mixed.generation.get(),
            sequence: mixed.sequence,
            first_node: relationship.source,
            second_node: relationship.target,
            relationship,
        },
    }
}

// Recovery fixtures pin replay byte identity and exact WAL/checkpoint cuts.
// Foreground automatic reclamation is independently covered by ZE-316.
fn isolate_recovery_from_foreground_reclaim(store: &Store) {
    store
        .set_native_graph_maintenance_policy(crate::property_graph::GraphMaintenancePolicy {
            automatic: false,
            ..Default::default()
        })
        .expect("isolate recovery fixture from foreground reclaim");
}
