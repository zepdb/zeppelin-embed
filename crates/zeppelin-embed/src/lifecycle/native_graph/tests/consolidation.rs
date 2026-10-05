use super::publication::{
    DurabilityEvent, RecordingVfs, property_fixture, snapshot_for_lease, sparse_physical_for_lease,
};
use crate::epoch::{ComputeUnits, EmbeddingRuntime, EmbeddingTower, Normalization};
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use crate::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use crate::property_graph::staging::{WriteLimits, WriteMemory};
use crate::property_graph::storage::NativePreparationSource;
use crate::property_graph::storage::inventory::{
    force_next_contradictory_inventory_addition, force_next_incomplete_inventory_retirement,
    inventory_resume_after, validate_fold_conservation, verify_inventory_entry,
};
use crate::property_graph::storage::memory::StorageMemory;
use crate::property_graph::storage::reclaim::{
    DurableRunReader, ProtectedClass, ProtectedRecord, ProtectedValue, omit_mark_artifact_for_test,
    take_omitted_mark_emissions_for_test, validate_protected_stream,
};
use crate::property_graph::storage::search::Modality;
use crate::property_graph::storage::tree::TreeKind;
use crate::property_graph::storage::tree::directory::{BlockSource, DirectoryCursor};
use crate::property_graph::wal::{ArtifactDescriptor, InventoryChange, InventoryState};
use crate::property_graph::{
    ApplicationKey, CanonicalContents, CanonicalEmbedding, EntityId, EntityKind, GraphName,
    GraphRevision, NodeRef,
};
use crate::vfs::Vfs;
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Barrier};

fn directory_image(path: &Path) -> BTreeMap<std::ffi::OsString, Vec<u8>> {
    std::fs::read_dir(path)
        .expect("read native directory")
        .map(|entry| {
            let entry = entry.expect("native directory entry");
            let name = entry.file_name();
            let bytes = std::fs::read(entry.path()).expect("read native file");
            (name, bytes)
        })
        .collect()
}

fn decode_persisted_manifest(payload: &[u8], owner: ArtifactDescriptor) -> Vec<InventoryChange> {
    assert_eq!(
        payload.get(..8),
        Some([b'Z', b'G', b'C', b'P', 2, 0, 1, 0].as_slice())
    );
    let count = u32::from_le_bytes(payload[8..12].try_into().expect("manifest count"));
    assert_eq!(payload.get(12..16), Some([0_u8; 4].as_slice()));
    assert_eq!(payload.len(), 16 + count as usize * 64);
    let mut output = Vec::new();
    for row in payload[16..].chunks_exact(64) {
        output.push(InventoryChange {
            object: ArtifactDescriptor {
                store: crate::property_graph::StoreInstanceId::new(u128::from_le_bytes(
                    row[0..16].try_into().expect("descriptor store"),
                ))
                .expect("nonzero descriptor store"),
                artifact: crate::property_graph::storage::artifact::ArtifactId::new(
                    u128::from_le_bytes(row[16..32].try_into().expect("descriptor artifact")),
                )
                .expect("nonzero descriptor artifact"),
                generation: crate::property_graph::GraphGeneration::new(u64::from_le_bytes(
                    row[32..40].try_into().expect("descriptor generation"),
                )),
                serial: u64::from_le_bytes(row[40..48].try_into().expect("descriptor serial")),
                bytes: u32::from_le_bytes(row[48..52].try_into().expect("descriptor bytes")),
                family: u16::from_le_bytes(row[52..54].try_into().expect("descriptor family")),
                version: u16::from_le_bytes(row[54..56].try_into().expect("descriptor version")),
                checksum: u64::from_le_bytes(row[56..64].try_into().expect("descriptor checksum")),
            },
            state: InventoryState::Retained,
        });
    }
    output.push(InventoryChange {
        object: owner,
        state: InventoryState::Retained,
    });
    output
}

fn prepared_union_for_lease(
    store: &Store,
    lease: &super::super::NativeReadLease,
) -> Vec<InventoryChange> {
    let shared = crate::property_graph::resources::GraphResources::from_store(store)
        .expect("shared resources");
    let control = QueryControl::Cancel(CancelToken::new());
    let writer = WriteMemory::new(&shared, WriteLimits::default()).expect("write memory");
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).expect("storage memory");
    let source = NativePreparationSource::new(lease, &memory, 64).expect("preparation source");
    let mut resources = source.resources(128 * 1024 * 1024).expect("tree resources");
    let mut output = Vec::new();
    for required in lease.bundle().prepared_inventories() {
        let block = source
            .resolve(required.block, &mut resources)
            .expect("resolve persisted prepared inventory");
        assert_eq!(block.identity().artifact, required.object.artifact);
        assert_eq!(block.reference(), required.block);
        assert_eq!(block.file_length(), required.object.bytes as usize);
        assert_eq!(block.file_checksum(), required.object.checksum);
        output.extend(decode_persisted_manifest(block.payload(), required.object));
    }
    output.sort_unstable_by_key(|change| change.object.artifact);
    output.dedup_by(|right, left| {
        if left.object.artifact != right.object.artifact {
            return false;
        }
        assert_eq!(left.object, right.object, "contradictory persisted union");
        true
    });
    output
}

fn prepared_manifest_for_lease(
    store: &Store,
    lease: &super::super::NativeReadLease,
    required: crate::property_graph::wal::RequiredRef,
) -> Vec<InventoryChange> {
    let shared = crate::property_graph::resources::GraphResources::from_store(store)
        .expect("shared resources");
    let control = QueryControl::Cancel(CancelToken::new());
    let writer = WriteMemory::new(&shared, WriteLimits::default()).expect("write memory");
    let memory = StorageMemory::new(&writer, &control, 8 * 1024 * 1024).expect("storage memory");
    let source = NativePreparationSource::new(lease, &memory, 8).expect("preparation source");
    let mut resources = source.resources(16 * 1024 * 1024).expect("tree resources");
    let block = source
        .resolve(required.block, &mut resources)
        .expect("resolve selected prepared inventory");
    assert_eq!(block.reference(), required.block);
    assert_eq!(block.file_checksum(), required.object.checksum);
    decode_persisted_manifest(block.payload(), required.object)
}

fn complete_inventory_union_for_lease(
    store: &Store,
    lease: &super::super::NativeReadLease,
) -> Vec<InventoryChange> {
    let mut output = rooted_inventory_for_lease(store, lease);
    output.extend(prepared_union_for_lease(store, lease));
    output.sort_unstable_by_key(|change| change.object.artifact);
    output.dedup_by(|right, left| {
        if left.object.artifact != right.object.artifact {
            return false;
        }
        assert_eq!(left.object, right.object, "contradictory complete union");
        true
    });
    let mut by_serial = output.clone();
    by_serial.sort_unstable_by_key(|change| change.object.serial);
    for pair in by_serial.windows(2) {
        assert!(
            pair[0].object.serial != pair[1].object.serial
                || pair[0].object.artifact == pair[1].object.artifact,
            "duplicate complete-union serial"
        );
    }
    output
}

fn rooted_inventory_for_lease(
    store: &Store,
    lease: &super::super::NativeReadLease,
) -> Vec<InventoryChange> {
    let shared = crate::property_graph::resources::GraphResources::from_store(store)
        .expect("shared resources");
    let control = QueryControl::Cancel(CancelToken::new());
    let writer = WriteMemory::new(&shared, WriteLimits::default()).expect("write memory");
    let memory = StorageMemory::new(&writer, &control, 8 * 1024 * 1024).expect("storage memory");
    let source = NativePreparationSource::new(lease, &memory, 32).expect("preparation source");
    let mut resources = source.resources(128 * 1024 * 1024).expect("tree resources");
    let root = lease
        .bundle()
        .roots()
        .directory(TreeKind::ObjectInventory)
        .expect("inventory root");
    let mut cursor =
        DirectoryCursor::seek(&source, root, None, &mut resources).expect("inventory cursor");
    let mut output = Vec::new();
    while let Some(entry) = cursor.next_entry(&mut resources).expect("inventory entry") {
        output.push(verify_inventory_entry(root, entry, &mut resources).expect("inventory row"));
    }
    output
}

fn options() -> OpenOptions {
    OpenOptions::new()
        .with_durability(DurabilityMode::Durable, CommitTier::Durable)
        .with_max_resident_bytes(256 * 1024 * 1024)
}

pub(super) fn pending_reclaim_candidates(
    store: &Store,
    lease: &super::super::NativeReadLease,
) -> Vec<ArtifactDescriptor> {
    pending_reclaim_proof_for_lease(store, lease).1
}

pub(super) fn pending_reclaim_proof_for_lease(
    store: &Store,
    lease: &super::super::NativeReadLease,
) -> (
    crate::property_graph::storage::reclaim::PendingIntentManifest,
    Vec<ArtifactDescriptor>,
) {
    let Some(required) = lease.bundle().reclaim() else {
        panic!("pending reclaim root");
    };
    let shared = crate::property_graph::resources::GraphResources::from_store(store)
        .expect("shared resources");
    let control = QueryControl::Cancel(CancelToken::new());
    let writer = WriteMemory::new(&shared, WriteLimits::default()).expect("write memory");
    let memory = StorageMemory::new(&writer, &control, 8 * 1024 * 1024).expect("storage memory");
    let source = NativePreparationSource::new(lease, &memory, 1).expect("reclaim source");
    let mut resources = source.resources(32 * 1024 * 1024).expect("tree resources");
    let block = source
        .resolve(required.block, &mut resources)
        .expect("resolve reclaim state");
    let manifest =
        crate::property_graph::storage::reclaim::decode_pending_intent_manifest(block.payload())
            .expect("pending reclaim manifest");
    let candidates = (0..manifest.candidate_count)
        .map(|index| {
            crate::property_graph::storage::reclaim::pending_intent_candidate_at(
                block.payload(),
                index,
            )
            .expect("pending reclaim candidate")
        })
        .collect();
    (manifest, candidates)
}

fn create_reclaim_test_store(path: &Path, vfs: &Arc<RecordingVfs>) -> Store {
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    Store::create_native_graph_with_infrastructure(
        path,
        options(),
        None,
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh reclaim proof store")
}

pub(super) fn seed_reclaimable_manifest(store: &Store, name: &str) {
    let image = CanonicalContents::node(&mut [], &mut [], Some("reclaim proof"), None)
        .expect("reclaim proof node image");
    store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "reclaim-proof", name)
                    .expect("reclaim proof key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("seed reclaim proof object");
    let first = store
        .admit_native_graph_maintenance()
        .expect("first reclaim proof admission");
    store
        .commit_native_graph_maintenance(&first, &QueryControl::Cancel(CancelToken::new()))
        .expect("first reclaim proof replacement");
    drop(first);
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .expect("checkpoint reclaim proof history");
}

fn cap_one_inventory_addition() -> super::super::maintenance::MaintenanceLimits {
    super::super::maintenance::MaintenanceLimits {
        inventory_additions: 1,
        ..Default::default()
    }
}

fn assert_live_files_unchanged(path: &Path, before: &BTreeMap<std::ffi::OsString, Vec<u8>>) {
    let after = directory_image(path);
    for (name, bytes) in before {
        assert_eq!(after.get(name), Some(bytes), "live file changed: {name:?}");
    }
}

fn durable_proof_for_lease(
    store: &Store,
    lease: &super::super::NativeReadLease,
) -> (
    Vec<ProtectedRecord>,
    Vec<crate::property_graph::storage::artifact::ArtifactId>,
) {
    let capture = store
        .capture_native_read_roots()
        .expect("capture durable proof roots");
    let proof = *capture.proofs().last().expect("latest durable proof");
    let shared = crate::property_graph::resources::GraphResources::from_store(store)
        .expect("durable proof resources");
    let control = QueryControl::Cancel(CancelToken::new());
    let writer = WriteMemory::new(&shared, WriteLimits::default()).expect("durable proof memory");
    let memory =
        StorageMemory::new(&writer, &control, 8 * 1024 * 1024).expect("durable proof storage");
    let mut resources =
        crate::property_graph::storage::tree::directory::TreeResources::for_prepare(
            &memory,
            64 * 1024 * 1024,
        )
        .expect("durable proof tree resources");
    let reader = super::super::maintenance::spill::NativeSpillReader::new(
        lease,
        &memory,
        proof.protected.binding.target_generation,
    )
    .expect("durable proof reader");
    let mut records = Vec::new();
    validate_protected_stream(
        proof.protected,
        &reader,
        &memory,
        &mut resources,
        |record, _| {
            records.push(record);
            Ok(())
        },
    )
    .expect("decode protected stream");
    let mut mark = DurableRunReader::new(proof.mark, &memory).expect("durable mark reader");
    let mut artifacts = Vec::new();
    while let Some(artifact) = mark
        .next(&reader, &mut resources)
        .expect("decode completed mark")
    {
        artifacts.push(artifact);
    }
    drop(capture);
    (records, artifacts)
}

fn maintenance_allocations(
    store: &Store,
    lease: &super::super::NativeReadLease,
    node: crate::property_graph::NodeId,
) -> Vec<ArtifactDescriptor> {
    let generation = lease.bundle().base().generation;
    let record_artifact = sparse_physical_for_lease(store, lease, node, Modality::Text)
        .record
        .reference()
        .artifact;
    let complete = complete_inventory_union_for_lease(store, lease);
    let mut matching_pack = complete
        .iter()
        .filter(|change| change.object.artifact == record_artifact);
    let pack = matching_pack
        .next()
        .expect("current relocated pack descriptor")
        .object;
    assert!(
        matching_pack.next().is_none(),
        "duplicate relocated pack descriptor"
    );
    assert_eq!(pack.generation, generation);

    let mut matching_manifest = lease
        .bundle()
        .prepared_inventories()
        .iter()
        .copied()
        .filter(|required| required.object.generation == generation);
    let manifest = matching_manifest
        .next()
        .expect("current maintenance manifest");
    assert!(
        matching_manifest.next().is_none(),
        "multiple current-generation maintenance manifests"
    );
    assert!(
        prepared_manifest_for_lease(store, lease, manifest)
            .iter()
            .any(|change| change.object == pack),
        "maintenance manifest does not authenticate relocated pack"
    );
    assert_ne!(pack, manifest.object);
    // A maintenance manifest now authenticates multiple lifetime objects.
    // Preserve exact accounting of every allocation, not just the record pack.
    let authenticated = prepared_manifest_for_lease(store, lease, manifest);
    let mut allocations: Vec<_> = complete
        .iter()
        .filter(|change| change.object.generation == generation)
        .map(|change| change.object)
        .collect();
    allocations.sort_unstable_by_key(|object| object.artifact);
    let mut expected: Vec<_> = authenticated.iter().map(|change| change.object).collect();
    expected.sort_unstable_by_key(|object| object.artifact);
    assert_eq!(allocations, expected, "unaccounted maintenance allocation");
    allocations
}

fn assert_semantic_snapshot_advanced(
    before: &super::publication::GenerationSnapshot,
    after: &super::publication::GenerationSnapshot,
) {
    assert_eq!(after.generation, before.generation + 1);
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.original_generation, before.original_generation);
    assert_eq!(after.canonical, before.canonical);
    assert_eq!(after.text, before.text);
    assert_eq!(after.vector, before.vector);
    assert_eq!(after.old_relationship, before.old_relationship);
    assert_eq!(after.new_relationship, before.new_relationship);
    assert_eq!(after.out, before.out);
    assert_eq!(after.incoming, before.incoming);
    assert_eq!(after.sparse_text, before.sparse_text);
    assert_eq!(after.sparse_vector, before.sparse_vector);
}

#[test]
fn ze46_real_consolidation_preserves_exact_state_and_reopens() {
    run_ze46_real_consolidation_preserves_exact_state_and_reopens();
}

fn run_ze46_real_consolidation_preserves_exact_state_and_reopens() {
    let parent = super::tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let document = EmbeddingTower {
        model_id: "ze46-document".into(),
        model_version: "1".into(),
        weights_digest: vec![0x46, 0xa5],
        dims: 2,
        normalization: Normalization::None,
        prompt_prefix: "doc: ".into(),
        max_tokens: 32,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    let store = Store::create_native_graph(&path, options(), Some(document.clone()))
        .expect("fresh native store");
    let coordinates = [f32::from_bits(0x3f80_0046), f32::from_bits(0x8000_0000)];
    let peer_coordinates = [f32::from_bits(0x4000_0046), f32::from_bits(0x3f00_0000)];
    let receipts = crate::property_graph::with_local_refs(|refs| {
        let label = GraphName::new("Document").expect("label");
        let mut labels = [label];
        let embedding = CanonicalEmbedding::new(&document, &coordinates).expect("embedding");
        let mut properties = property_fixture();
        let first = CanonicalContents::node(
            &mut labels,
            &mut properties,
            Some("exact ze46 text"),
            Some(embedding),
        )
        .expect("first node");
        let peer_embedding =
            CanonicalEmbedding::new(&document, &peer_coordinates).expect("peer embedding");
        let second = CanonicalContents::node(
            &mut [],
            &mut [],
            Some("exact ze46 peer text"),
            Some(peer_embedding),
        )
        .expect("second node");
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
                    source: NodeRef::Local(refs.node(0).expect("node slot")),
                    target: NodeRef::Local(refs.node(1).expect("node slot")),
                    relationship_type: GraphName::new("LINKS").expect("relationship type"),
                    properties: &[],
                }),
            },
        ];
        store
            .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
            .expect("mixed graph commit")
    });
    let first = match receipts[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("first receipt identity"),
    };
    let second = match receipts[1].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("second receipt identity"),
    };
    let relationship = match receipts[2].entity {
        EntityId::Relationship(rel) => rel,
        EntityId::Node(_) => panic!("relationship receipt identity"),
    };

    let old_lazy = store.admit_native_read().expect("unmapped retained reader");
    let before_lease = store.admit_native_read().expect("before reader");
    let before_roots = before_lease.bundle().roots().references();
    let before_text_root = before_lease.bundle().text();
    let before_vector_root = before_lease.bundle().vector();
    let before = snapshot_for_lease(&store, &before_lease, first, second, relationship, None);
    let peer_before = snapshot_for_lease(&store, &before_lease, second, first, relationship, None);
    let before_text_physical =
        sparse_physical_for_lease(&store, &before_lease, first, Modality::Text);
    let before_peer_text_physical =
        sparse_physical_for_lease(&store, &before_lease, second, Modality::Text);
    let before_vector_physical =
        sparse_physical_for_lease(&store, &before_lease, first, Modality::Vector);
    let before_peer_vector_physical =
        sparse_physical_for_lease(&store, &before_lease, second, Modality::Vector);
    assert_eq!(
        before_text_physical.source,
        before_peer_text_physical.source
    );
    assert_eq!(
        before_vector_physical.source,
        before_peer_vector_physical.source
    );
    drop(before_lease);
    crate::property_graph::storage::search::miss_next_maintenance_peer_retarget();
    let rejected = store
        .admit_native_graph_maintenance()
        .expect("refusal maintenance admission");
    let error = store
        .commit_native_graph_maintenance(&rejected, &QueryControl::Cancel(CancelToken::new()))
        .expect_err("missed sparse peer retarget must refuse");
    assert!(
        matches!(
            &error,
            super::super::NativeGraphError::Read(
                crate::property_graph::storage::tree::directory::TreeError::Invalid(_)
            )
        ),
        "unexpected refusal error: {error:?}"
    );
    let after_refusal = store.admit_native_read().expect("reader after refusal");
    assert_eq!(after_refusal.bundle().roots().references(), before_roots);
    assert_eq!(
        after_refusal.bundle().base().generation.get(),
        before.generation
    );
    drop(after_refusal);
    drop(rejected);
    let admitted = store
        .admit_native_graph_maintenance()
        .expect("maintenance admission");
    let report = store
        .commit_native_graph_maintenance(&admitted, &QueryControl::Cancel(CancelToken::new()))
        .expect("maintenance commit");
    assert!(report.replaced_physical_refs > 0);
    assert!(report.new_pack_bytes > 0);
    assert_eq!(report.generation.get(), before.generation + 1);

    let current = store.admit_native_read().expect("current reader");
    let after_roots = current.bundle().roots().references();
    assert_ne!(current.bundle().text(), before_text_root);
    assert_ne!(current.bundle().vector(), before_vector_root);
    assert_ne!(
        after_roots[0], before_roots[0],
        "node directory did not move"
    );
    assert_ne!(after_roots[5], before_roots[5], "OUT range did not move");
    assert_ne!(after_roots[6], before_roots[6], "IN range did not move");
    let after = snapshot_for_lease(&store, &current, first, second, relationship, None);
    let peer_after = snapshot_for_lease(&store, &current, second, first, relationship, None);
    let after_text_physical = sparse_physical_for_lease(&store, &current, first, Modality::Text);
    let after_peer_text_physical =
        sparse_physical_for_lease(&store, &current, second, Modality::Text);
    let after_vector_physical =
        sparse_physical_for_lease(&store, &current, first, Modality::Vector);
    let after_peer_vector_physical =
        sparse_physical_for_lease(&store, &current, second, Modality::Vector);
    assert_eq!(after.generation, before.generation + 1);
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.original_generation, before.original_generation);
    assert_eq!(after.canonical, before.canonical);
    assert_eq!(after.text, before.text);
    assert_eq!(after.vector, coordinates.map(f32::to_bits));
    assert_eq!(after.out, before.out);
    assert_eq!(after.incoming, before.incoming);
    assert_eq!(after.sparse_text, before.sparse_text);
    assert_eq!(after.sparse_vector, before.sparse_vector);
    assert_eq!(peer_after.canonical, peer_before.canonical);
    assert_eq!(peer_after.text, peer_before.text);
    assert_eq!(peer_after.vector, peer_coordinates.map(f32::to_bits));
    assert_eq!(peer_after.sparse_text, peer_before.sparse_text);
    assert_eq!(peer_after.sparse_vector, peer_before.sparse_vector);
    for (old, new, old_peer, new_peer) in [
        (
            before_text_physical,
            after_text_physical,
            before_peer_text_physical,
            after_peer_text_physical,
        ),
        (
            before_vector_physical,
            after_vector_physical,
            before_peer_vector_physical,
            after_peer_vector_physical,
        ),
    ] {
        assert_ne!(new.source, old.source);
        assert_ne!(new.row_table, old.row_table);
        assert_ne!(new.record, old.record);
        assert_eq!(new.source, new_peer.source);
        assert_eq!(old.mask, new.mask);
        assert_eq!(old.lexical, new.lexical);
        assert_eq!(old.ordinal, new.ordinal);
        assert_eq!(old_peer.ordinal, new_peer.ordinal);
        assert_ne!(old_peer.record, new_peer.record);
    }
    drop(current);

    let old_after = snapshot_for_lease(&store, &old_lazy, first, second, relationship, None);
    assert_eq!(old_after, before);
    let old_peer_after = snapshot_for_lease(&store, &old_lazy, second, first, relationship, None);
    assert_eq!(old_peer_after, peer_before);
    drop(old_lazy);
    drop(admitted);
    store.close().expect("close before reopen");

    let reopened = Store::open_native_graph(&path, options(), Some(document.clone()))
        .expect("reopen maintenance state");
    let reopened_lease = reopened.admit_native_read().expect("reopened reader");
    let reopened_snapshot = snapshot_for_lease(
        &reopened,
        &reopened_lease,
        first,
        second,
        relationship,
        None,
    );
    assert_eq!(reopened_snapshot, after);
    drop(reopened_lease);

    let replay = crate::property_graph::with_local_refs(|refs| {
        let label = GraphName::new("Document").expect("label");
        let mut labels = [label];
        let embedding = CanonicalEmbedding::new(&document, &coordinates).expect("embedding");
        let mut properties = property_fixture();
        let first_image = CanonicalContents::node(
            &mut labels,
            &mut properties,
            Some("exact ze46 text"),
            Some(embedding),
        )
        .expect("first node");
        let peer_embedding =
            CanonicalEmbedding::new(&document, &peer_coordinates).expect("peer embedding");
        let second_image = CanonicalContents::node(
            &mut [],
            &mut [],
            Some("exact ze46 peer text"),
            Some(peer_embedding),
        )
        .expect("second node");
        let requests = [
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", "a").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&first_image)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", "b").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&second_image)),
            },
            StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "app", "ab").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &[],
                }),
            },
        ];
        reopened
            .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
            .expect("exact keyed replay")
    });
    assert!(replay.iter().all(|receipt| receipt.replayed));
    assert!(
        replay
            .iter()
            .all(|receipt| receipt.generation.get() == before.original_generation)
    );
    reopened.close().expect("close reopened store");
}

/// The node directory's raw value for `node`: its physical record reference.
pub(crate) fn node_directory_value(
    store: &Store,
    lease: &super::super::NativeReadLease,
    node: crate::property_graph::NodeId,
) -> Vec<u8> {
    let shared = crate::property_graph::resources::GraphResources::from_store(store)
        .expect("shared resources");
    let control = QueryControl::Cancel(CancelToken::new());
    let writer = WriteMemory::new(&shared, WriteLimits::default()).expect("write memory");
    let memory = StorageMemory::new(&writer, &control, 8 * 1024 * 1024).expect("storage memory");
    let source = NativePreparationSource::new(lease, &memory, 8).expect("directory source");
    let mut resources = source.resources(32 * 1024 * 1024).expect("tree resources");
    let root = lease
        .bundle()
        .roots()
        .directory(TreeKind::Nodes)
        .expect("node directory root");
    crate::property_graph::storage::tree::directory::lookup_entry(
        &source,
        root,
        &node.get().to_le_bytes(),
        &mut resources,
    )
    .expect("node directory lookup")
    .expect("node directory entry")
    .value()
    .to_vec()
}

#[test]
fn ze260_maintenance_moves_an_oversized_oldest_record_alone() {
    use crate::property_graph::{GraphProperty, PropertyData, PropertyValue};
    let parent = super::tempfile::tempdir().unwrap();
    let store = Store::create_native_graph(parent.path().join("native"), options(), None).unwrap();
    let large = "x".repeat(1024 * 1024 + 1);
    let mut properties = [GraphProperty::new(
        GraphName::new("large").unwrap(),
        PropertyValue::new(PropertyData::String(&large)).unwrap(),
    )];
    let image = CanonicalContents::node(&mut [], &mut properties, None, None).unwrap();
    let small = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let garbage = "g".repeat(2 * 1024 * 1024);
    let mut garbage_properties = [GraphProperty::new(
        GraphName::new("garbage").unwrap(),
        PropertyValue::new(PropertyData::String(&garbage)).unwrap(),
    )];
    let garbage_image =
        CanonicalContents::node(&mut [], &mut garbage_properties, None, None).unwrap();
    let writes = [
        (&image, "large"),
        (&small, "small"),
        (&garbage_image, "garbage"),
    ]
    .map(|(image, name)| StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "oversized", name).unwrap(),
        revision: GraphRevision::new(1).unwrap(),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(image)),
    });
    let receipts = store
        .apply_native_graph(&writes, &QueryControl::Cancel(CancelToken::new()))
        .unwrap();
    let nodes = receipts
        .iter()
        .map(|receipt| match receipt.entity {
            EntityId::Node(node) => node,
            EntityId::Relationship(_) => panic!("expected node"),
        })
        .collect::<Vec<_>>();
    store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "oversized", "garbage").unwrap(),
                revision: GraphRevision::new(2).unwrap(),
                operation: StructuredOperation::Put(receipts[2].entity),
                image: Some(WriteImage::Node(&small)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap();
    let maintenance = || {
        let admission = store.admit_native_graph_maintenance().unwrap();
        store
            .commit_native_graph_maintenance_with_limits(
                &admission,
                &QueryControl::Cancel(CancelToken::new()),
                super::super::maintenance::MaintenanceLimits {
                    relocation_bytes: 1024 * 1024,
                    ..Default::default()
                },
            )
            .unwrap()
    };
    let records = || {
        let lease = store.admit_native_read().unwrap();
        nodes
            .iter()
            .map(|node| node_directory_value(&store, &lease, *node))
            .collect::<Vec<_>>()
    };
    let before = records();
    let first = maintenance();
    eprintln!(
        "oversized cycle 1: drained={}, copied={}",
        first.drained_packs, first.relocated_bytes
    );
    let after = records();
    assert_ne!(after[0], before[0], "oversized oldest record must move");
    assert_eq!(after[1], before[1], "oversized record must move alone");
    let second = maintenance();
    eprintln!(
        "oversized cycle 2: drained={}, copied={}",
        second.drained_packs, second.relocated_bytes
    );
    assert_ne!(records()[1], after[1], "remaining small record must move");
    store.close().unwrap();
}

#[test]
fn ze260_small_packs_merge_only_in_groups() {
    let parent = super::tempfile::tempdir().unwrap();
    let store = Store::create_native_graph(parent.path().join("native"), options(), None).unwrap();
    let mut nodes = Vec::new();
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    for batch in 0..15 {
        let receipts = store
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "small-packs", &batch.to_string())
                        .unwrap(),
                    revision: GraphRevision::new(1).unwrap(),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .unwrap();
        let EntityId::Node(node) = receipts[0].entity else {
            panic!("node");
        };
        nodes.push(node);
        if batch == 11 {
            store
                .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
                .unwrap();
            let report = commit_maintenance(&store).unwrap();
            eprintln!(
                "small cycle 1: drained={}, copied={}",
                report.drained_packs, report.relocated_bytes
            );
            assert_eq!(report.drained_packs, 12);
        }
    }
    let records = || {
        let lease = store.admit_native_read().unwrap();
        nodes[..11]
            .iter()
            .map(|node| node_directory_value(&store, &lease, *node))
            .collect::<Vec<_>>()
    };
    let before = records();
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .unwrap();
    // S6c's first preparation can leave an intent. Finish it before
    // measuring a second preparation; completion itself never drains packs.
    for _ in 0..4 {
        let pending = store
            .admit_native_read()
            .unwrap()
            .bundle()
            .reclaim()
            .is_some();
        if !pending {
            break;
        }
        match commit_maintenance(&store) {
            Ok(_) | Err(super::super::NativeGraphError::StalePreparation) => {}
            Err(error) => panic!("finish prior intent: {error:?}"),
        }
    }
    assert!(
        store
            .admit_native_read()
            .unwrap()
            .bundle()
            .reclaim()
            .is_none()
    );
    let report = commit_maintenance(&store).unwrap();
    eprintln!(
        "small cycle 2: drained={}, copied={}",
        report.drained_packs, report.relocated_bytes
    );
    assert_ne!(
        records(),
        before,
        "the fixture's leaf pages died, so the quarter-dead rule applies"
    );
    // Fully live small packs are different: seven wait, eight merge.
    {
        use crate::property_graph::storage::artifact::ArtifactId;
        use crate::property_graph::storage::consolidation::{PackCensus, select_drain};
        let shared = crate::property_graph::resources::GraphResources::from_store(&store).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
        let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
        let mut census: Vec<_> = (1..=8)
            .map(|id| PackCensus {
                artifact: ArtifactId::new(id).unwrap(),
                serial: id as u64,
                bytes: 4096,
                live: 4000,
                live_pages: 0,
                live_records: 1,
                graph_live: true,
            })
            .collect();
        for count in 1..8 {
            assert!(
                select_drain(&mut census[..count], 8 * 1024 * 1024, &memory)
                    .unwrap()
                    .as_slice()
                    .is_empty()
            );
        }
        assert_eq!(
            select_drain(&mut census, 8 * 1024 * 1024, &memory)
                .unwrap()
                .as_slice()
                .len(),
            8
        );
    }
    store.close().unwrap();
}

#[test]
fn ze260_drain_never_recopies_a_maintenance_pack() {
    let parent = super::tempfile::tempdir().unwrap();
    let path = parent.path().join("native");
    let store = Store::create_native_graph(&path, options(), None).unwrap();
    for batch in 0..20 {
        crate::property_graph::with_local_refs(|refs| {
            let names: Vec<_> = (0..10).map(|n| format!("{batch}-{n}")).collect();
            let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
            let mut writes: Vec<_> = names
                .iter()
                .map(|name| StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "drain", name).unwrap(),
                    revision: GraphRevision::new(1).unwrap(),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                })
                .collect();
            writes.push(StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "drain", &names[0]).unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &[],
                }),
            });
            store
                .apply_native_graph(&writes, &QueryControl::Cancel(CancelToken::new()))
                .unwrap();
        });
    }

    let values = || {
        let lease = store.admit_native_read().unwrap();
        let shared = crate::property_graph::resources::GraphResources::from_store(&store).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
        let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
        let source = NativePreparationSource::new(&lease, &memory, 512).unwrap();
        let mut r = source.resources(128 * 1024 * 1024).unwrap();
        let mut values = Vec::new();
        for kind in [TreeKind::Nodes, TreeKind::Relationships] {
            let root = lease.bundle().roots().directory(kind).unwrap();
            let mut cursor = DirectoryCursor::seek(&source, root, None, &mut r).unwrap();
            while let Some(entry) = cursor.next_entry(&mut r).unwrap() {
                values.push(entry.value().to_vec());
            }
        }
        let mut ranges = Vec::new();
        for kind in [TreeKind::OutRanges, TreeKind::InRanges] {
            let root = lease.bundle().roots().directory(kind).unwrap();
            let mut cursor = DirectoryCursor::seek(&source, root, None, &mut r).unwrap();
            while let Some(entry) = cursor.next_entry(&mut r).unwrap() {
                let crate::property_graph::storage::tree::Key::Inline(key) = entry.key() else {
                    panic!("range key");
                };
                let descriptor =
                    crate::property_graph::storage::adjacency::RangeDescriptor::decode(
                        kind,
                        key,
                        entry.value(),
                    )
                    .unwrap();
                ranges.extend(
                    std::iter::once(descriptor.base())
                        .chain(descriptor.deltas())
                        .map(|reference| reference.artifact),
                );
            }
        }
        (values, ranges)
    };
    let original = values().0;
    let old_packs: std::collections::BTreeSet<_> = original[..190]
        .iter()
        .map(|value| {
            crate::property_graph::storage::payload::PayloadRef::decode(value)
                .unwrap()
                .reference()
                .artifact
        })
        .collect();
    assert_eq!(old_packs.len(), 19);
    let mut after_first = Vec::new();
    for cycle in 1..=4 {
        // Count preparations, not completion/retirement or checkpoint retries.
        for _ in 0..4 {
            let pending = store
                .admit_native_read()
                .unwrap()
                .bundle()
                .reclaim()
                .is_some();
            if !pending {
                break;
            }
            match commit_maintenance(&store) {
                Ok(_) | Err(super::super::NativeGraphError::StalePreparation) => {}
                Err(error) => panic!("finish prior intent: {error:?}"),
            }
        }
        assert!(
            store
                .admit_native_read()
                .unwrap()
                .bundle()
                .reclaim()
                .is_none()
        );
        store
            .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
            .unwrap();
        let report = commit_maintenance(&store).unwrap();
        let (current, ranges) = values();
        eprintln!(
            "cycle {cycle}: drained={}, copied={}, refs={}",
            report.drained_packs, report.relocated_bytes, report.replaced_physical_refs
        );
        assert_eq!(report.drained_packs, [19, 1, 0, 0][cycle - 1]);
        if cycle >= 3 {
            assert_eq!(report.relocated_bytes, 0);
        }
        if cycle == 1 {
            for value in current[..190].iter().chain(&current[200..219]) {
                assert!(
                    !old_packs.contains(
                        &crate::property_graph::storage::payload::PayloadRef::decode(value)
                            .unwrap()
                            .reference()
                            .artifact
                    )
                );
            }
            assert!(
                ranges.iter().all(|artifact| !old_packs.contains(artifact)),
                "range payload still pins a drained pack"
            );
            after_first = current;
        } else if cycle >= 3 {
            assert!(
                current[..190] == after_first[..190] && current[200..219] == after_first[200..219],
                "maintenance must not recopy drained records"
            );
        }
    }
    store.close().unwrap();
}

#[test]
fn ze260_maintenance_drains_the_oldest_pack_in_one_call() {
    let parent = super::tempfile::tempdir().unwrap();
    let path = parent.path().join("native");
    let store = Store::create_native_graph(&path, options(), None).unwrap();
    for batch in 0..20 {
        crate::property_graph::with_local_refs(|refs| {
            let names: Vec<_> = (0..10).map(|n| format!("{batch}-{n}")).collect();
            let image = CanonicalContents::node(&mut [], &mut [], Some("payload"), None).unwrap();
            let mut writes: Vec<_> = names
                .iter()
                .map(|name| StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "drain", name).unwrap(),
                    revision: GraphRevision::new(1).unwrap(),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                })
                .collect();
            writes.push(StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "drain", &names[0]).unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &[],
                }),
            });
            store
                .apply_native_graph(&writes, &QueryControl::Cancel(CancelToken::new()))
                .unwrap();
        });
    }
    let records = |lease: &super::super::NativeReadLease, newer_than: Option<u64>| {
        use crate::property_graph::storage::{
            NativePreparationCatalog, payload::PayloadRef, records::verify_record,
            stream::PayloadSlice, tree::Key,
        };
        let shared = crate::property_graph::resources::GraphResources::from_store(&store).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
        let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
        let source = NativePreparationSource::new(lease, &memory, 512).unwrap();
        let mut resources = source.resources(128 * 1024 * 1024).unwrap();
        let catalog = NativePreparationCatalog::open(&source, &mut resources).unwrap();
        let mut records = BTreeMap::new();
        let mut counts = [0; 2];
        for (index, kind) in [TreeKind::Nodes, TreeKind::Relationships]
            .into_iter()
            .enumerate()
        {
            let root = lease.bundle().roots().directory(kind).unwrap();
            let mut cursor = DirectoryCursor::seek(&source, root, None, &mut resources).unwrap();
            while let Some(entry) = cursor.next_entry(&mut resources).unwrap() {
                let Key::Inline(key) = entry.key() else {
                    panic!("entity key must be inline");
                };
                let id = u128::from_le_bytes(key.try_into().unwrap());
                let entity = if kind == TreeKind::Nodes {
                    EntityId::Node(crate::property_graph::NodeId::new(id).unwrap())
                } else {
                    EntityId::Relationship(crate::property_graph::RelId::new(id).unwrap())
                };
                let reference = PayloadRef::decode(entry.value()).unwrap();
                if let Some(oldest) = newer_than {
                    let block = source
                        .resolve(reference.reference(), &mut resources)
                        .unwrap();
                    assert!(
                        block.identity().creation_serial > oldest,
                        "record pack must be newer than oldest"
                    );
                }
                let record = verify_record(
                    PayloadSlice::new(
                        &source,
                        root.store(),
                        entry.creation_generation(),
                        reference,
                    ),
                    entity,
                    &catalog,
                    None,
                    &mut resources,
                )
                .unwrap();
                let mut canonical = vec![0; record.canonical_bytes().len() as usize];
                assert_eq!(
                    record
                        .canonical_bytes()
                        .read_at(0, &mut canonical, &mut resources)
                        .unwrap(),
                    canonical.len()
                );
                assert!(
                    records
                        .insert(entity, (record.revision().get(), canonical))
                        .is_none()
                );
                counts[index] += 1;
            }
        }
        assert_eq!(counts, [200, 20]);
        records
    };
    let lease = store.admit_native_read().unwrap();
    let oldest = prepared_union_for_lease(&store, &lease)
        .into_iter()
        .min_by_key(|change| change.object.serial)
        .unwrap()
        .object
        .serial;
    let before = records(&lease, None);
    drop(lease);
    let report = commit_maintenance(&store).unwrap();
    let lease = store.admit_native_read().unwrap();
    assert_eq!(records(&lease, Some(oldest)), before);
    assert!(report.replaced_physical_refs >= 33);
    drop(lease);
    store.close().unwrap();
    Store::open_native_graph(&path, options(), None)
        .unwrap()
        .close()
        .unwrap();
}

#[test]
fn ze260_maintenance_emits_each_tree_root_once_per_call() {
    run_ze260_maintenance_emits_each_tree_root_once_per_call();
}

fn run_ze260_maintenance_emits_each_tree_root_once_per_call() {
    let parent = super::tempfile::tempdir().unwrap();
    let path = parent.path().join("native");
    let store = Store::create_native_graph(&path, options(), None).unwrap();
    for batch in 0..20 {
        crate::property_graph::with_local_refs(|refs| {
            let names: Vec<_> = (0..10).map(|n| format!("{batch}-{n}")).collect();
            let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
            let mut writes: Vec<_> = names
                .iter()
                .map(|name| StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "drain", name).unwrap(),
                    revision: GraphRevision::new(1).unwrap(),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                })
                .collect();
            writes.push(StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "drain", &names[0]).unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &[],
                }),
            });
            store
                .apply_native_graph(&writes, &QueryControl::Cancel(CancelToken::new()))
                .unwrap();
        });
    }
    use crate::property_graph::storage::artifact::{self, BlockKind, ContainerKind};
    use crate::property_graph::storage::tree::{Cell, decode_page};
    let before = directory_image(&path);
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .unwrap();
    let report = commit_maintenance(&store).unwrap();
    let mut emitted = BTreeMap::<u16, usize>::new();
    let mut all_pages = BTreeMap::<u16, usize>::new();
    let mut mixed = 0;
    let mut emitted_pages = std::collections::BTreeSet::new();
    for (name, bytes) in directory_image(&path) {
        if before.contains_key(&name) || !name.to_string_lossy().ends_with(".zgraph") {
            continue;
        }
        let family = u16::from_le_bytes(bytes[8..10].try_into().unwrap());
        if family == 18 {
            artifact::decode(ContainerKind::RootEnvelope, None, &bytes).unwrap();
            continue;
        }
        let frame = artifact::decode(ContainerKind::Object, None, &bytes).unwrap();
        if frame.identity().generation != report.generation {
            continue;
        }
        let mut inventory = false;
        let mut other = false;
        let count = u32::from_le_bytes(bytes[80..84].try_into().unwrap());
        for i in 0..count as usize {
            let reference = frame.reference(i).unwrap();
            let block = frame.framed_block(reference).unwrap();
            if reference.kind == BlockKind::TreePage {
                let kind = u16::from_le_bytes(block.payload()[6..8].try_into().unwrap());
                let level = u16::from_le_bytes(block.payload()[16..18].try_into().unwrap());
                *all_pages.entry(kind).or_default() += 1;
                if kind == TreeKind::ObjectInventory as u16 {
                    inventory = true;
                } else {
                    other = true;
                    emitted_pages.insert((reference.artifact.get(), reference.offset));
                }
                if level > 0 {
                    *emitted.entry(kind).or_default() += 1;
                }
            } else {
                other = true;
            }
        }
        mixed += usize::from(inventory && other);
    }
    let lease = store.admit_native_read().unwrap();
    let shared = crate::property_graph::resources::GraphResources::from_store(&store).unwrap();
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
    let source = NativePreparationSource::new(&lease, &memory, 128).unwrap();
    let mut resources = source.resources(u64::MAX).unwrap();
    let mut counts = Vec::new();
    let mut reachable_pages = std::collections::BTreeSet::new();
    for kind in [
        TreeKind::Nodes,
        TreeKind::Relationships,
        TreeKind::KeyFences,
        TreeKind::Labels,
        TreeKind::RelationshipTypes,
        TreeKind::OutRanges,
        TreeKind::InRanges,
    ] {
        let mut pending: Vec<_> = lease
            .bundle()
            .roots()
            .directory(kind)
            .unwrap()
            .reference()
            .into_iter()
            .collect();
        let mut seen = std::collections::BTreeSet::new();
        let mut reachable = 0;
        while let Some(reference) = pending.pop() {
            if !seen.insert((reference.artifact.get(), reference.offset)) {
                continue;
            }
            reachable_pages.insert((reference.artifact.get(), reference.offset));
            let block = source.resolve(reference, &mut resources).unwrap();
            let page = decode_page(kind, block.payload()).unwrap();
            if page.header().level == 0 {
                continue;
            }
            reachable += 1;
            let count = u32::from_le_bytes(block.payload()[12..16].try_into().unwrap());
            for i in 0..count as usize {
                if let Cell::Branch { child, .. } = page.cell(i).unwrap() {
                    pending.push(child);
                }
            }
        }
        let emitted = emitted.get(&(kind as u16)).copied().unwrap_or(0);
        eprintln!(
            "{kind:?}: pages={} emitted_branches={emitted} reachable_branches={reachable}",
            all_pages.get(&(kind as u16)).copied().unwrap_or(0)
        );
        counts.push((kind, emitted, reachable));
    }
    eprintln!("mixed_inventory_objects={mixed}");
    let dead_pages = emitted_pages.difference(&reachable_pages).count();
    eprintln!("dead_graph_pages={dead_pages}");
    assert_eq!(dead_pages, 0, "graph pages must not be dead at birth");
    assert_eq!(mixed, 0, "inventory must have its own lifetime stream");
    for (kind, emitted, reachable) in counts {
        assert_eq!(emitted, reachable, "dead-at-birth branch in {kind:?}");
    }
}

#[test]
fn ze260_idle_maintenance_reaches_a_fixed_point() {
    let parent = super::tempfile::tempdir().unwrap();
    let path = parent.path().join("native");
    let store = Store::create_native_graph(&path, options(), None).unwrap();
    for batch in 0..20 {
        crate::property_graph::with_local_refs(|refs| {
            let names: Vec<_> = (0..10).map(|n| format!("{batch}-{n}")).collect();
            let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
            let mut writes: Vec<_> = names
                .iter()
                .map(|name| StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "drain", name).unwrap(),
                    revision: GraphRevision::new(1).unwrap(),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                })
                .collect();
            writes.push(StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "drain", &names[0]).unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &[],
                }),
            });
            store
                .apply_native_graph(&writes, &QueryControl::Cancel(CancelToken::new()))
                .unwrap();
        });
    }
    let records = |store: &Store, lease: &super::super::NativeReadLease| {
        use crate::property_graph::query::resources::QueryMemory;
        use crate::property_graph::query::runtime::{RuntimeContext, RuntimeLimits};
        use crate::property_graph::storage::tree::directory::TreeResources;
        use crate::property_graph::storage::{
            GraphReadView, NativeCatalog, NativeQuerySource, NativeReadCapability,
        };
        let shared = crate::property_graph::resources::GraphResources::from_store(store).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let mut result = BTreeMap::new();
        for entity in
            (1..=200)
                .map(|id| EntityId::Node(crate::property_graph::NodeId::new(id).unwrap()))
                .chain((1..=20).map(|id| {
                    EntityId::Relationship(crate::property_graph::RelId::new(id).unwrap())
                }))
        {
            let memory = QueryMemory::new(&shared, 8 * 1024 * 1024).unwrap();
            let mut runtime =
                RuntimeContext::new(lease, &control, &memory, RuntimeLimits::default()).unwrap();
            let capability = NativeReadCapability::admit(lease, &runtime).unwrap();
            let mut resources = TreeResources::for_query(&mut runtime).unwrap();
            let source = NativeQuerySource::new(capability, &resources, 16).unwrap();
            let catalog = NativeCatalog::open(&source, &mut resources).unwrap();
            drop(resources);
            let view = GraphReadView::new(&source, &catalog).unwrap();
            let mut resources = TreeResources::for_query(&mut runtime).unwrap();
            let mut capture =
                |record: &crate::property_graph::storage::records::RecordView<'_, _>,
                 resources: &mut TreeResources<'_>| {
                    let mut canonical = vec![0; record.canonical_bytes().len() as usize];
                    assert_eq!(
                        record
                            .canonical_bytes()
                            .read_at(0, &mut canonical, resources)
                            .unwrap(),
                        canonical.len()
                    );
                    assert!(
                        result
                            .insert(record.incarnation(), (record.revision().get(), canonical))
                            .is_none()
                    );
                };
            match entity {
                EntityId::Node(id) => {
                    let node = view.lookup_node(id, &mut resources).unwrap().unwrap();
                    capture(node.record(), &mut resources);
                }
                EntityId::Relationship(id) => {
                    let relationship = view
                        .lookup_relationship(id, &mut resources)
                        .unwrap()
                        .unwrap();
                    capture(relationship.record(), &mut resources);
                }
            }
        }
        result
    };
    let bytes = || {
        std::fs::read_dir(&path)
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum::<u64>()
    };
    let lease = store.admit_native_read().unwrap();
    let oldest = prepared_union_for_lease(&store, &lease)
        .into_iter()
        .min_by_key(|change| change.object.serial)
        .unwrap()
        .object
        .artifact;
    let oldest_path = crate::property_graph::storage::allocation::artifact_path(&path, oldest);
    let before = records(&store, &lease);
    drop(lease);
    let before_bytes = bytes();
    let mut removed = 0;
    let mut subtypes = Vec::new();
    const MAX_COMPLETED_CYCLES: usize = 8;
    let mut completed_cycles = 0;
    let mut first_cycle_bytes = None;
    let mut sizes = Vec::new();
    let mut previous_removed = 0;
    // One preparation call plus intent, completion and retirement per cycle.
    for _ in 0..MAX_COMPLETED_CYCLES * 4 {
        // Historical WAL roots protect original packs until checkpointed.
        store
            .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
            .unwrap();
        removed += commit_maintenance(&store).unwrap().removed_bytes;
        let admission = store.admit_native_graph_maintenance().unwrap();
        if admission.lease.bundle().reclaim().is_some() {
            subtypes.push(
                super::super::maintenance::active_reclaim_subtype(
                    &store,
                    &admission,
                    &QueryControl::Cancel(CancelToken::new()),
                )
                .unwrap(),
            );
        } else if subtypes.last() == Some(&3) {
            subtypes.push(0);
            completed_cycles += 1;
            eprintln!(
                "S6_MUST_MAKE_IDLE_CYCLES_NET_NEGATIVE cycle={completed_cycles} bytes={} growth={} files={} removed_bytes={}",
                bytes(),
                i128::from(bytes()) - i128::from(before_bytes),
                std::fs::read_dir(&path).unwrap().count(),
                removed - previous_removed,
            );
            sizes.push((bytes(), std::fs::read_dir(&path).unwrap().count()));
            previous_removed = removed;
            first_cycle_bytes.get_or_insert_with(bytes);
            if completed_cycles == MAX_COMPLETED_CYCLES {
                break;
            }
        }
    }
    assert_eq!(completed_cycles, 8);
    assert!(sizes[1].0 <= sizes[0].0 - 1_500_000);
    assert!(sizes[1].1 < sizes[0].1);
    // Compare completed cycles 5 through 8 (sizes is zero-indexed).
    for cycle in 5..8 {
        assert_eq!(sizes[cycle].1, sizes[cycle - 1].1);
        assert!(sizes[cycle].0.abs_diff(sizes[cycle - 1].0) <= 8_192);
    }
    assert!(sizes[7].0 <= 810_000);
    assert!(sizes[7].1 <= 55);
    eprintln!(
        "ZE260_S5 before_bytes={before_bytes} after_bytes={} removed_bytes={removed} completed_cycles={completed_cycles} max_completed_cycles={MAX_COMPLETED_CYCLES} first_cycle_bytes={first_cycle_bytes:?} subtypes={subtypes:?}",
        bytes()
    );
    assert!(
        (1..=MAX_COMPLETED_CYCLES).contains(&completed_cycles),
        "must finish within the completed-cycle bound"
    );
    assert!(
        subtypes.windows(3).any(|states| states == [2, 3, 0]),
        "intent, completion, retirement must complete"
    );
    assert_eq!(subtypes.last(), Some(&0), "final cycle must be retired");
    assert!(removed > 0, "full cycles removed no bytes");
    assert!(
        !oldest_path.exists(),
        "oldest append-only pack remains live"
    );
    store.close().unwrap();
    let reopened = Store::open_native_graph(&path, options(), None).unwrap();
    let lease = reopened.admit_native_read().unwrap();
    assert_eq!(records(&reopened, &lease), before);
    drop(lease);
    reopened.close().unwrap();
}

#[test]
fn ze46_consolidation_rotates_through_every_node() {
    run_ze46_consolidation_rotates_through_every_node();
}

fn run_ze46_consolidation_rotates_through_every_node() {
    // Relocation must move every node before moving one again. A bounded
    // call may move several nodes; this three-node store needs at most three.
    let parent = super::tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let store = Store::create_native_graph(&path, options(), None).expect("fresh native store");
    let mut nodes = Vec::new();
    for name in ["a", "b", "c"] {
        let image = CanonicalContents::node(&mut [], &mut [], Some(name), None).expect("node");
        let receipt = store
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "rotate", name).expect("key"),
                    revision: GraphRevision::new(1).expect("revision"),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("create node");
        match receipt[0].entity {
            EntityId::Node(node) => nodes.push(node),
            EntityId::Relationship(_) => panic!("node receipt identity"),
        }
    }
    let records = |store: &Store| {
        let lease = store.admit_native_read().expect("record reader");
        nodes
            .iter()
            .map(|node| node_directory_value(store, &lease, *node))
            .collect::<Vec<_>>()
    };
    let original = records(&store);
    let mut previous = original.clone();
    let mut moved = vec![false; nodes.len()];
    for call in 0..nodes.len() {
        let report = commit_maintenance(&store).expect("rotating maintenance");
        assert!(report.replaced_physical_refs > 0);
        let current = records(&store);
        let changed: Vec<_> = (0..nodes.len())
            .filter(|index| current[*index] != previous[*index])
            .collect();
        assert!(!changed.is_empty(), "call {call} moved no nodes");
        for index in changed {
            assert!(
                !moved[index],
                "call {call} moved node {index} again before the others"
            );
            moved[index] = true;
        }
        previous = current;
        if moved.iter().all(|moved| *moved) {
            break;
        }
    }
    assert!(moved.iter().all(|moved| *moved));
    assert!(
        records(&store)
            .iter()
            .zip(&original)
            .all(|(now, before)| now != before)
    );
    store.close().expect("close rotating store");
    let reopened = Store::open_native_graph(&path, options(), None).expect("reopen");
    assert_eq!(records(&reopened), previous);
    reopened.close().expect("close reopened store");
}

#[test]
fn ze46_range_consolidation_keeps_self_loops_and_parallel_edges() {
    run_ze46_range_consolidation_keeps_self_loops_and_parallel_edges();
}

/// A self-loop and two parallel edges of one type, plus an edge of a second
/// type, written in separate commits so their ranges carry pending deltas.
/// Consolidation turns pending deltas into bounded bases one range per call;
/// OUT and IN expansions must stay exactly equal throughout and after reopen.
fn run_ze46_range_consolidation_keeps_self_loops_and_parallel_edges() {
    let parent = super::tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let store = Store::create_native_graph(&path, options(), None).expect("fresh native store");
    let mut nodes = Vec::new();
    for name in ["hub", "leaf"] {
        let image = CanonicalContents::node(&mut [], &mut [], None, None).expect("node");
        let receipt = store
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "edges", name).expect("key"),
                    revision: GraphRevision::new(1).expect("revision"),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("create node");
        match receipt[0].entity {
            EntityId::Node(node) => nodes.push(node),
            EntityId::Relationship(_) => panic!("node receipt identity"),
        }
    }
    let (hub, leaf) = (nodes[0], nodes[1]);
    for (name, source, target, relationship_type) in [
        ("self", hub, hub, "LINKS"),
        ("parallel-a", hub, leaf, "LINKS"),
        ("parallel-b", hub, leaf, "LINKS"),
        ("other-type", leaf, hub, "OWNS"),
    ] {
        store
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Relationship, "edges", name).expect("key"),
                    revision: GraphRevision::new(1).expect("revision"),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Relationship {
                        source: NodeRef::Existing(source),
                        target: NodeRef::Existing(target),
                        relationship_type: GraphName::new(relationship_type).expect("type"),
                        properties: &[],
                    }),
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("create relationship");
    }
    let expansions = |store: &Store| {
        let lease = store.admit_native_read().expect("expansion reader");
        [hub, leaf].map(|node| {
            let mut rows = super::publication::out_rows_for_lease(store, &lease, node, hub);
            rows.sort_by_key(|row| row.rel);
            rows
        })
    };
    let before = expansions(&store);
    assert_eq!(before[0].len(), 3, "self-loop and both parallel edges");
    assert_eq!(before[1].len(), 1);
    assert!(
        before[0]
            .iter()
            .any(|row| row.source == hub && row.target == hub)
    );
    assert_eq!(
        before[0].iter().filter(|row| row.target == leaf).count(),
        2,
        "parallel relationships stay distinct"
    );
    for round in 0..6 {
        commit_maintenance(&store)
            .unwrap_or_else(|error| panic!("range round {round} failed: {error:?}"));
        assert_eq!(
            expansions(&store),
            before,
            "round {round} changed an expansion"
        );
    }
    store.close().expect("close edge store");
    let reopened = Store::open_native_graph(&path, options(), None).expect("reopen edge store");
    assert_eq!(expansions(&reopened), before);
    reopened.close().expect("close reopened edge store");
}

#[test]
fn ze46_maintenance_accepts_empty_and_deleted_first_node_states() {
    run_ze46_maintenance_accepts_empty_and_deleted_first_node_states();
}

fn run_ze46_maintenance_accepts_empty_and_deleted_first_node_states() {
    // A never-written store is a valid maintenance base with nothing to do:
    // success, no publication, no byte changed.
    let parent = super::tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let store = Store::create_native_graph(&path, options(), None).expect("fresh native store");
    let before = directory_image(&path);
    let report = commit_maintenance(&store).expect("maintenance over the empty graph");
    assert_eq!(report.generation.get(), 0);
    assert_eq!(report.replaced_physical_refs, 0);
    assert_eq!(report.removed_bytes, 0);
    assert_eq!(directory_image(&path), before);

    // The lowest node id is deleted; the next live node is the one to move.
    fn key(name: &str) -> ApplicationKey<'_> {
        ApplicationKey::new(EntityKind::Node, "first", name).expect("key")
    }
    let image = CanonicalContents::node(&mut [], &mut [], Some("kept"), None).expect("node");
    let mut created = Vec::new();
    for name in ["deleted", "kept"] {
        let receipt = store
            .apply_native_graph(
                &[StructuredWrite {
                    key: key(name),
                    revision: GraphRevision::new(1).expect("revision"),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("create node");
        created.push(receipt[0].entity);
    }
    assert!(created[0] < created[1], "the deleted node sorts first");
    store
        .apply_native_graph(
            &[StructuredWrite {
                key: key("deleted"),
                revision: GraphRevision::new(2).expect("revision"),
                operation: StructuredOperation::Delete(
                    created[0],
                    crate::property_graph::GraphDeleteMode::Restrict,
                ),
                image: None,
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("delete the first node");
    let report = commit_maintenance(&store).expect("maintenance past a deleted first node");
    assert!(report.replaced_physical_refs > 0);
    store.close().expect("close first-node store");
    let reopened = Store::open_native_graph(&path, options(), None).expect("reopen");
    reopened.close().expect("close reopened store");
}

#[test]
fn ze46_repeated_maintenance_releases_durable_proof_protection() {
    run_ze46_repeated_maintenance_releases_durable_proof_protection();
}

fn run_ze46_repeated_maintenance_releases_durable_proof_protection() {
    // Each maintenance commit records its proof in the writer's fixed
    // 64-entry protection table. A checkpoint cuts the WAL envelope that named
    // the proof, so the entry must go unless a reclaim cycle still roots it.
    // Without the release, long-running maintenance fills the table for good.
    let parent = super::tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let vfs = Arc::new(RecordingVfs::default());
    let store = create_reclaim_test_store(&path, &vfs);
    seed_reclaimable_manifest(&store, "repeat");
    let protections = |store: &Store| {
        store
            .native_graph
            .writer
            .lock()
            .expect("native writer")
            .as_ref()
            .expect("installed writer")
            .durable_protected
            .iter()
            .map(|proof| proof.intent.is_some())
            .collect::<Vec<_>>()
    };
    let mut committed = 0_usize;
    for round in 0..48 {
        match commit_maintenance(&store) {
            Ok(_) => committed += 1,
            Err(super::super::NativeGraphError::StalePreparation) => {}
            Err(error) => panic!("maintenance round {round} failed: {error:?}"),
        }
        store
            .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
            .expect("checkpoint after maintenance");
        let open_cycle = store
            .admit_native_read()
            .expect("round reader")
            .bundle()
            .reclaim()
            .is_some();
        let held = protections(&store);
        if open_cycle {
            assert!(
                held.iter().all(|intent| *intent),
                "round {round}: a proof without an intent outlived its WAL envelope"
            );
        } else {
            assert!(
                held.is_empty(),
                "round {round}: {} proofs protected with no open reclaim cycle",
                held.len()
            );
        }
        assert!(
            held.len() <= 2,
            "round {round}: {} proofs protected",
            held.len()
        );
    }
    assert!(
        committed > 24,
        "only {committed} maintenance commits succeeded"
    );
    store.close().expect("close repeated-maintenance store");
    let reopened = Store::open_native_graph(&path, options(), None).expect("reopen");
    reopened.close().expect("close reopened store");
}

#[test]
fn ze46_stale_preparation_rejects_without_publication_or_foreign_cleanup() {
    run_ze46_stale_preparation_rejects_without_publication_or_foreign_cleanup();
}

fn run_ze46_stale_preparation_rejects_without_publication_or_foreign_cleanup() {
    let parent = super::tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let store = Store::create_native_graph(&path, options(), None).expect("fresh native store");
    let first = CanonicalContents::node(&mut [], &mut [], Some("base"), None).expect("node");
    let first_request = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "stale", "base").expect("base key"),
        revision: GraphRevision::new(1).expect("revision"),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&first)),
    }];
    store
        .apply_native_graph(&first_request, &QueryControl::Cancel(CancelToken::new()))
        .expect("base write");
    let admission = store
        .admit_native_graph_maintenance()
        .expect("maintenance admission");

    let second = CanonicalContents::node(&mut [], &mut [], Some("foreground"), None)
        .expect("foreground node");
    let second_request = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "stale", "foreground").expect("foreground key"),
        revision: GraphRevision::new(1).expect("revision"),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&second)),
    }];
    store
        .apply_native_graph(&second_request, &QueryControl::Cancel(CancelToken::new()))
        .expect("foreground changed write");
    let foreign = path.join("graph-object-ffffffffffffffffffffffffffffffff.zgraph");
    std::fs::write(&foreign, b"foreign-collision-bytes").expect("foreign collision");
    let before = directory_image(&path);
    let generation = store
        .admit_native_read()
        .expect("reader before stale commit")
        .bundle()
        .base()
        .generation;

    let error = store
        .commit_native_graph_maintenance(&admission, &QueryControl::Cancel(CancelToken::new()))
        .expect_err("changed base must make preparation stale");
    assert!(
        matches!(error, super::super::NativeGraphError::StalePreparation),
        "changed base returned {error:?}"
    );
    super::publication::record_verified_fault();
    assert_eq!(directory_image(&path), before);
    assert_eq!(
        std::fs::read(&foreign).expect("foreign collision remains"),
        b"foreign-collision-bytes"
    );
    assert_eq!(
        store
            .admit_native_read()
            .expect("reader after stale refusal")
            .bundle()
            .base()
            .generation,
        generation
    );
    drop(admission);
    store.close().expect("close stale store");

    // Late branch: the foreground commit lands after this preparation has
    // created its first private file, so only the recheck under the writer
    // lock can refuse it. Then a checkpoint alone replaces the admitted bundle.
    let late_parent = super::tempfile::tempdir().expect("late stale parent");
    let path = late_parent.path().join("native");
    let vfs = Arc::new(RecordingVfs::default());
    let store = Arc::new(create_reclaim_test_store(&path, &vfs));
    store
        .apply_native_graph(&first_request, &QueryControl::Cancel(CancelToken::new()))
        .expect("late base write");
    let serial_before = store
        .capture_native_read_roots()
        .expect("pre-preparation capture")
        .serial_fence();
    let admission = store
        .admit_native_graph_maintenance()
        .expect("late maintenance admission");
    let before = directory_image(&path);
    let foreground = Arc::clone(&store);
    vfs.after_next_create(move || {
        let second = CanonicalContents::node(&mut [], &mut [], Some("foreground"), None)
            .expect("foreground node");
        let receipt = foreground
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "stale", "foreground")
                        .expect("foreground key"),
                    revision: GraphRevision::new(1).expect("revision"),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&second)),
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("foreground commit during paused preparation");
        assert_eq!(receipt[0].generation.get(), 2);
    });
    vfs.take();
    let error = store
        .commit_native_graph_maintenance(&admission, &QueryControl::Cancel(CancelToken::new()))
        .expect_err("base changed after private allocation");
    assert!(
        matches!(error, super::super::NativeGraphError::StalePreparation),
        "late changed base returned {error:?}"
    );
    super::publication::record_verified_fault();
    assert!(!vfs.after_create_is_armed(), "foreground commit never ran");
    drop(admission);
    let events = vfs.take();
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, DurabilityEvent::Delete(_))),
        "stale preparation deleted a file: {events:?}"
    );
    let capture = store
        .capture_native_read_roots()
        .expect("post-stale capture");
    assert!(
        capture.serial_fence() > serial_before,
        "the preparation burned its own serials before the recheck"
    );
    assert_eq!(capture.bundle_count(), 1);
    assert!(
        capture.leases().is_empty(),
        "stale preparation kept a lease"
    );
    assert!(
        capture.spills().is_empty(),
        "stale preparation kept a spill registration"
    );
    drop(capture);
    let current = store.admit_native_read().expect("reader after late stale");
    assert_eq!(current.bundle().base().generation.get(), 2);
    assert!(current.bundle().reclaim().is_none());
    let checkpoint_base = current.bundle().root_envelope();
    drop(current);
    for (name, bytes) in &before {
        let after = std::fs::read(path.join(name)).expect("preexisting file");
        if name.to_string_lossy().starts_with("graph-wal-") {
            assert!(after.starts_with(bytes), "WAL prefix changed");
        } else {
            assert_eq!(&after, bytes, "stale preparation changed {name:?}");
        }
    }

    let admission = store
        .admit_native_graph_maintenance()
        .expect("checkpoint-branch admission");
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .expect("checkpoint replaces the admitted bundle");
    let before = directory_image(&path);
    let error = store
        .commit_native_graph_maintenance(&admission, &QueryControl::Cancel(CancelToken::new()))
        .expect_err("checkpoint replacement must make preparation stale");
    assert!(
        matches!(error, super::super::NativeGraphError::StalePreparation),
        "checkpoint replacement returned {error:?}"
    );
    super::publication::record_verified_fault();
    drop(admission);
    assert_eq!(directory_image(&path), before);
    let current = store
        .admit_native_read()
        .expect("reader after checkpoint stale");
    assert_ne!(current.bundle().root_envelope(), checkpoint_base);
    assert_eq!(current.bundle().base().generation.get(), 2);
    drop(current);

    // Own serial burns alone never make a preparation stale.
    let admission = store
        .admit_native_graph_maintenance()
        .expect("clean admission");
    let report = store
        .commit_native_graph_maintenance(&admission, &QueryControl::Cancel(CancelToken::new()))
        .expect("own allocations do not stale the preparation");
    assert_eq!(report.generation.get(), 3);
    drop(admission);
    let store = Arc::into_inner(store).expect("sole late store owner");
    store.close().expect("close late stale store");
}

#[test]
fn ze46_atomic_capture_excludes_new_and_inflight_allocations() {
    run_ze46_atomic_capture_excludes_new_and_inflight_allocations();
}

fn run_ze46_atomic_capture_excludes_new_and_inflight_allocations() {
    let parent = super::tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let store =
        Arc::new(Store::create_native_graph(&path, options(), None).expect("fresh native store"));
    let image =
        CanonicalContents::node(&mut [], &mut [], Some("capture"), None).expect("capture node");
    store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "capture", "base").expect("capture key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("capture seed write");
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .expect("checkpoint capture seed");

    let base = store.admit_native_read().expect("capture base lease");
    let descriptor = complete_inventory_union_for_lease(&store, &base)
        .first()
        .copied()
        .expect("authentic finalized allocation")
        .object;
    let registered = [InventoryChange {
        object: descriptor,
        state: InventoryState::Prepared,
    }];
    let prepared = base
        .register_prepared(&registered)
        .expect("register finalized preparation");
    let spill = base.register_spill().expect("register precreate spill");
    spill
        .begin_data(descriptor, None)
        .expect("publish exact in-flight descriptor before create");

    let initial = store
        .capture_native_read_roots()
        .expect("capture prepared and in-flight owners");
    assert!(initial.contains_lease(&base));
    assert!(initial.contains_prepared(descriptor));
    assert!(initial.spills().iter().any(|entry| {
        entry.admission_token == base.token()
            && entry.head.is_none()
            && entry.pending == [Some(descriptor), None]
    }));

    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    store
        .native_graph
        .state
        .lock()
        .expect("publication state")
        .admission_hook = Some((Arc::clone(&entered), Arc::clone(&release)));
    std::thread::scope(|scope| {
        let read_store = Arc::clone(&store);
        let racing = scope.spawn(move || {
            read_store
                .admit_native_read()
                .expect("barrier-controlled read admission")
        });
        entered.wait();
        let capture_store = Arc::clone(&store);
        let capture = scope.spawn(move || {
            capture_store
                .capture_native_read_roots()
                .expect("capture racing admission")
        });
        release.wait();
        let racing = racing.join().expect("admission thread");
        let captured = capture.join().expect("capture thread");
        assert!(
            captured.contains_lease(&racing),
            "the read is registered before capture can pass publication exclusion"
        );
        assert!(captured.contains_prepared(descriptor));
        assert!(captured.spills().iter().any(|entry| {
            entry.admission_token == base.token() && entry.pending == [Some(descriptor), None]
        }));
        drop(racing);
        assert!(
            captured
                .leases()
                .iter()
                .any(|lease| lease.token() != base.token() && lease.check_active().is_ok()),
            "capture retains the racing owner through sweep completion"
        );
    });

    drop(initial);
    drop(spill);
    drop(prepared);
    let released = store
        .capture_native_read_roots()
        .expect("capture after preparation abort");
    assert!(!released.contains_prepared(descriptor));
    assert!(released.spills().is_empty());
    drop(released);
    drop(base);
    Arc::try_unwrap(store)
        .unwrap_or_else(|_| panic!("capture store Arc leaked"))
        .close()
        .expect("close capture store");
}

#[test]
fn ze46_inventory_fold_conserves_complete_allocation_union() {
    crate::property_graph::storage::tree::directory::tests::inventory_fold_page_count();
    run_ze46_inventory_fold_conserves_complete_allocation_union();
}

fn run_ze46_inventory_fold_conserves_complete_allocation_union() {
    assert_eq!(
        inventory_resume_after(255_u128.to_le_bytes()),
        Some(256_u128.to_le_bytes())
    );
    assert_eq!(inventory_resume_after(u128::MAX.to_le_bytes()), None);
    let parent = super::tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let store = Store::create_native_graph(&path, options(), None).expect("fresh native store");
    for (key, text) in [("one", "first manifest"), ("two", "second manifest")] {
        let image = CanonicalContents::node(&mut [], &mut [], Some(text), None).expect("node");
        let request = [StructuredWrite {
            key: ApplicationKey::new(EntityKind::Node, "inventory", key).expect("key"),
            revision: GraphRevision::new(1).expect("revision"),
            operation: StructuredOperation::Create,
            image: Some(WriteImage::Node(&image)),
        }];
        store
            .apply_native_graph(&request, &QueryControl::Cancel(CancelToken::new()))
            .expect("manifest-producing write");
    }
    let before = store.admit_native_read().expect("reader before fold");
    assert_eq!(before.bundle().prepared_inventories().len(), 2);
    let original_manifests = before.bundle().prepared_inventories().to_vec();
    let first_manifest = prepared_manifest_for_lease(&store, &before, original_manifests[0]);
    let expected = complete_inventory_union_for_lease(&store, &before);
    assert!(
        first_manifest.len() >= 2,
        "first manifest has an allocation and its owner"
    );
    assert!(expected.len() >= 4, "two manifests plus their allocations");
    drop(before);

    for (arm, label) in [
        (
            force_next_incomplete_inventory_retirement as fn(),
            "incomplete retirement",
        ),
        (
            force_next_contradictory_inventory_addition as fn(),
            "contradictory addition",
        ),
    ] {
        let before_refusal = directory_image(&path);
        let admitted = store
            .admit_native_graph_maintenance()
            .expect("negative-control maintenance admission");
        arm();
        assert!(
            store
                .commit_native_graph_maintenance_with_limits(
                    &admitted,
                    &QueryControl::Cancel(CancelToken::new()),
                    cap_one_inventory_addition(),
                )
                .is_err(),
            "{label} must fail the bound producer before commit"
        );
        super::publication::record_verified_fault();
        drop(admitted);
        assert_eq!(
            directory_image(&path),
            before_refusal,
            "{label} refusal cannot publish files or WAL"
        );
    }

    let admitted = store
        .admit_native_graph_maintenance()
        .expect("maintenance admission");
    store
        .commit_native_graph_maintenance_with_limits(
            &admitted,
            &QueryControl::Cancel(CancelToken::new()),
            cap_one_inventory_addition(),
        )
        .expect("inventory-folding maintenance");
    drop(admitted);
    let after = store.admit_native_read().expect("reader after fold");
    assert_eq!(
        after
            .bundle()
            .prepared_inventories()
            .get(..original_manifests.len()),
        Some(original_manifests.as_slice()),
        "a partial slice retains the selected and unrelated manifests"
    );
    let actual = rooted_inventory_for_lease(&store, &after);
    assert_eq!(actual.len(), 1, "cap-one slice adds one rooted descriptor");
    let current_manifest_union = prepared_union_for_lease(&store, &after);
    let inventory_page = after.bundle().roots().references()[7].expect("inventory page");
    assert!(
        current_manifest_union
            .iter()
            .any(|change| change.object.artifact == inventory_page.artifact),
        "new inventory pages remain in the current prepared manifest"
    );
    assert!(current_manifest_union.iter().any(|change| {
        after
            .bundle()
            .prepared_inventories()
            .iter()
            .any(|manifest| manifest.object == change.object)
    }));
    let first_union = complete_inventory_union_for_lease(&store, &after);
    for descriptor in &expected {
        assert!(
            first_union
                .iter()
                .any(|current| current.object == descriptor.object),
            "partial fold conserves the admitted allocation union"
        );
    }
    drop(after);
    store.close().expect("close partially folded store");

    let store = Store::open_native_graph(&path, options(), None).expect("replay partial fold");
    let replayed = store
        .admit_native_read()
        .expect("reader after partial replay");
    assert_eq!(
        replayed
            .bundle()
            .prepared_inventories()
            .get(..original_manifests.len()),
        Some(original_manifests.as_slice())
    );
    assert_eq!(rooted_inventory_for_lease(&store, &replayed).len(), 1);
    drop(replayed);

    let mut slices = 1_usize;
    let mut previous_covered = 1_usize;
    let mut previous_union = first_union;
    loop {
        let admitted = store
            .admit_native_graph_maintenance()
            .expect("resumed maintenance admission");
        store
            .commit_native_graph_maintenance_with_limits(
                &admitted,
                &QueryControl::Cancel(CancelToken::new()),
                cap_one_inventory_addition(),
            )
            .expect("resumed cap-one inventory fold");
        drop(admitted);
        slices += 1;
        let lease = store
            .admit_native_read()
            .expect("reader after resumed fold");
        let rooted = rooted_inventory_for_lease(&store, &lease);
        let covered = first_manifest
            .iter()
            .filter(|expected| rooted.iter().any(|actual| actual.object == expected.object))
            .count();
        assert_eq!(
            covered,
            previous_covered + 1,
            "each cap-one slice adds one uncovered selected descriptor"
        );
        previous_covered = covered;
        let current_union = complete_inventory_union_for_lease(&store, &lease);
        for descriptor in &previous_union {
            assert!(
                current_union
                    .iter()
                    .any(|current| current.object == descriptor.object),
                "each partial fold conserves the complete prior union"
            );
        }
        previous_union = current_union;
        let retired = !lease
            .bundle()
            .prepared_inventories()
            .contains(&original_manifests[0]);
        assert!(
            lease
                .bundle()
                .prepared_inventories()
                .contains(&original_manifests[1]),
            "unrelated manifest remains referenced"
        );
        drop(lease);
        if retired {
            break;
        }
        assert!(
            slices <= first_manifest.len(),
            "bounded fold must make durable progress"
        );
    }
    assert!(slices >= 2, "fold required multiple bounded slices");
    let complete = store.admit_native_read().expect("reader after retirement");
    let rooted = rooted_inventory_for_lease(&store, &complete);
    let mut folded_first: Vec<_> = first_manifest
        .iter()
        .filter_map(|expected| {
            rooted
                .iter()
                .find(|actual| actual.object.artifact == expected.object.artifact)
                .copied()
        })
        .collect();
    folded_first.sort_unstable_by_key(|change| change.object.artifact);
    let mut expected_first = first_manifest.clone();
    expected_first.sort_unstable_by_key(|change| change.object.artifact);
    validate_fold_conservation(&expected_first, &folded_first)
        .expect("retirement retained every selected descriptor and owner");
    let before_checkpoint = complete_inventory_union_for_lease(&store, &complete);
    drop(complete);
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .expect("checkpoint folded inventory");
    store.close().expect("close folded inventory store");

    let reopened = Store::open_native_graph(&path, options(), None).expect("reopen folded store");
    let lease = reopened.admit_native_read().expect("reopened reader");
    assert_eq!(
        complete_inventory_union_for_lease(&reopened, &lease)
            .iter()
            .map(|change| change.object)
            .collect::<Vec<_>>(),
        before_checkpoint
            .iter()
            .map(|change| change.object)
            .collect::<Vec<_>>(),
        "checkpoint/reopen preserves complete allocation union"
    );
    drop(lease);
    reopened.close().expect("close reopened inventory store");
}

#[test]
fn ze46_inventory_fold_drains_a_manifest_backlog() {
    run_ze46_inventory_fold_drains_a_manifest_backlog();
}

fn run_ze46_inventory_fold_drains_a_manifest_backlog() {
    // Every commit appends one prepared manifest. Maintenance adds one of
    // its own, so it must retire several per call or the list never shrinks.
    let parent = super::tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let store = Store::create_native_graph(&path, options(), None).expect("fresh native store");
    const WRITES: usize = 40;
    for index in 0..WRITES {
        let image = CanonicalContents::node(&mut [], &mut [], None, None).expect("node image");
        store
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "backlog", &index.to_string())
                        .expect("backlog key"),
                    revision: GraphRevision::new(1).expect("revision"),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("backlog write");
    }
    let manifests = |store: &Store| {
        store
            .admit_native_read()
            .expect("manifest reader")
            .bundle()
            .prepared_inventories()
            .len()
    };
    assert_eq!(manifests(&store), WRITES);
    // Cut the WAL first: the proof retraces every uncheckpointed state
    // (ZE-163), which would dominate this case without changing the fold.
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .expect("checkpoint the backlog");
    assert_eq!(manifests(&store), WRITES);
    let before = {
        let lease = store.admit_native_read().expect("union reader");
        complete_inventory_union_for_lease(&store, &lease)
    };

    // Released WAL protection also starts reclaim cycles here. Their resume
    // and clear steps each add a manifest without folding, so the count is
    // not monotonic; the fold still has to win over the whole run.
    let mut calls = 0_usize;
    let mut peak = WRITES;
    while manifests(&store) > 4 {
        match commit_maintenance(&store) {
            Ok(_) | Err(super::super::NativeGraphError::StalePreparation) => {}
            Err(error) => panic!("draining maintenance call {calls} failed: {error:?}"),
        }
        calls += 1;
        peak = peak.max(manifests(&store));
        assert!(
            calls <= WRITES,
            "{} manifests remain after {calls} calls: the backlog does not drain",
            manifests(&store)
        );
    }
    assert!(peak <= WRITES + 1, "the backlog grew to {peak} manifests");
    assert_eq!(calls, 1, "one S6c fold must drain forty commit manifests");

    // Retirement moves bookkeeping into the rooted tree; it never loses it.
    // A row may leave only with its file, through a completed reclaim cycle.
    let lease = store.admit_native_read().expect("drained union reader");
    let after = complete_inventory_union_for_lease(&store, &lease);
    let mut reclaimed = 0_usize;
    for change in &before {
        let kept = after.iter().any(|kept| kept.object == change.object);
        let on_disk = crate::property_graph::storage::allocation::artifact_path(
            &path,
            change.object.artifact,
        )
        .exists();
        assert!(
            kept || !on_disk,
            "drain lost the bookkeeping of a file that still exists: {:?}",
            change.object
        );
        reclaimed += usize::from(!kept);
    }
    assert!(reclaimed < before.len(), "every allocation was reclaimed");
    drop(lease);
    store.close().expect("close drained store");
    let reopened = Store::open_native_graph(&path, options(), None).expect("reopen drained store");
    assert!(manifests(&reopened) <= 4);
    reopened.close().expect("close reopened drained store");
}

#[test]
fn ze46_checkpoint_during_an_open_reclaim_cycle_reopens() {
    run_ze46_checkpoint_during_an_open_reclaim_cycle_reopens();
}

fn run_ze46_checkpoint_during_an_open_reclaim_cycle_reopens() {
    // A foreground checkpoint (explicit, or the writer's 64-envelope policy)
    // can land at any point of a reclaim cycle. Every such state must reopen.
    for stage in ["pending intent", "completion"] {
        let (history, store) = seed_crash_history();
        let oracle = history.oracle(&store);
        commit_maintenance(&store).expect("durable reclaim intent");
        if stage == "completion" {
            commit_maintenance(&store).expect("in-process resume");
        }
        store
            .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
            .expect("checkpoint inside the reclaim cycle");
        store.close().expect("close mid-cycle store");
        let reopened = history
            .open(options())
            .unwrap_or_else(|error| panic!("reopen after checkpoint at {stage}: {error:?}"));
        assert_same_logical_state(&oracle, &history.oracle(&reopened));
        reopened.close().expect("close reopened mid-cycle store");
    }

    // A foreground write may also land between the intent and its resume.
    // The cycle must still finish, in this process and after a reopen.
    for reopen_first in [false, true] {
        let (history, mut store) = seed_crash_history();
        commit_maintenance(&store).expect("durable reclaim intent");
        let pending = store.admit_native_read().expect("pending reader");
        let candidates = pending_reclaim_candidates(&store, &pending);
        drop(pending);
        let image = CanonicalContents::node(&mut [], &mut [], Some("late"), None).expect("node");
        store
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "crash", "late").expect("key"),
                    revision: GraphRevision::new(1).expect("revision"),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("foreground write inside the reclaim cycle");
        if reopen_first {
            store
                .close()
                .expect("close store with a write after the intent");
            store = history
                .open(options())
                .unwrap_or_else(|error| panic!("reopen with a write after the intent: {error:?}"));
        } else {
            commit_maintenance(&store).expect("resume after a foreground write");
        }
        for candidate in &candidates {
            assert!(
                !reclaim_candidate_path(&history.path, candidate).exists(),
                "reopen_first={reopen_first}: pending target survived"
            );
        }
        store.close().expect("close finished cycle store");
    }
}

#[test]
fn ze46_reconciles_real_prewal_orphans_without_touching_unknown_files() {
    run_ze46_reconciles_real_prewal_orphans_without_touching_unknown_files();
}

fn run_ze46_reconciles_real_prewal_orphans_without_touching_unknown_files() {
    use super::publication::FaultPoint;
    let parent = super::tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let vfs = Arc::new(RecordingVfs::default());
    let store = create_reclaim_test_store(&path, &vfs);
    let write = |name: &str| {
        let image = CanonicalContents::node(&mut [], &mut [], Some(name), None).expect("node");
        store.apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "orphans", name).expect("key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
    };
    write("first").expect("first commit");

    // Real crash debris: each fault fires inside a real write, before any WAL
    // inventory names the new files.
    let mut debris = Vec::new();
    for (point, label) in [
        (FaultPoint::ObjectSync, "complete"),
        (FaultPoint::PartialCreate, "partial"),
        (FaultPoint::Create, "headerless"),
    ] {
        let before = directory_image(&path);
        vfs.arm_fault(point);
        let Err(error) = write("second") else {
            panic!("{label} fault did not fail the write");
        };
        assert!(
            matches!(error, super::super::NativeGraphError::Io { .. }),
            "{label}: {error:?}"
        );
        vfs.assert_fired_once();
        let created: Vec<_> = directory_image(&path)
            .into_iter()
            .filter(|(name, _)| !before.contains_key(name))
            .collect();
        assert!(!created.is_empty(), "{label} fault left no file");
        debris.push((label, created));
    }
    write("second").expect("clean retry");
    // ZE-46 reclaims the complete orphan; ZE-165 reclaims the interrupted
    // create beside it. The headerless prefix is retained by both.
    let complete: Vec<_> = debris[0].1.iter().chain(&debris[1].1).cloned().collect();
    let mut retained: BTreeMap<std::ffi::OsString, Vec<u8>> = debris[2].1.iter().cloned().collect();
    assert!(
        debris[1].1.iter().all(|(_, bytes)| bytes.len() >= 96),
        "the partial create keeps an intact header"
    );
    assert!(
        debris[2]
            .1
            .iter()
            .all(|(_, bytes)| bytes == b"foreign-owner")
    );

    // Files cleanup must never touch.
    let (orphan_name, orphan_bytes) = complete.first().cloned().expect("complete orphan");
    retained.insert("notes.txt".into(), b"not a native name".to_vec());
    let mut unknown_family = orphan_bytes.clone();
    unknown_family[8] = 99;
    retained.insert(
        "graph-0000000000000000000000000000f00d.zgraph".into(),
        unknown_family,
    );
    let foreign_parent = super::tempfile::tempdir().expect("foreign parent");
    let foreign_path = foreign_parent.path().join("native");
    let foreign_store =
        Store::create_native_graph(&foreign_path, options(), None).expect("foreign store");
    let image = CanonicalContents::node(&mut [], &mut [], None, None).expect("foreign node");
    foreign_store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "foreign", "node").expect("key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("foreign commit");
    foreign_store.close().expect("close foreign store");
    let (foreign_name, foreign_bytes) = directory_image(&foreign_path)
        .into_iter()
        .find(|(name, _)| name.to_string_lossy().ends_with(".zgraph"))
        .expect("foreign object");
    retained.insert(foreign_name, foreign_bytes);
    for (name, bytes) in &retained {
        if !path.join(name).exists() {
            std::fs::write(path.join(name), bytes).expect("plant retained file");
        }
    }

    // Maintenance with checkpoints between calls. Every unlink must name a
    // target of the pending intent that was durable before it.
    let (lists_before, children_before) = vfs.enumeration_calls();
    let mut removed_bytes = 0_u64;
    let mut deleted: BTreeMap<std::ffi::OsString, u64> = BTreeMap::new();
    let mut first_spill_files: Option<Vec<std::ffi::OsString>> = None;
    for round in 0..40 {
        let sizes: BTreeMap<_, _> = directory_image(&path)
            .into_iter()
            .map(|(name, bytes)| (name, bytes.len() as u64))
            .collect();
        let pending: Vec<_> = {
            let lease = store.admit_native_read().expect("round reader");
            pending_reclaim_targets_or_empty_root(&store, &lease)
        };
        vfs.take();
        match commit_maintenance(&store) {
            Ok(report) => removed_bytes += report.removed_bytes,
            Err(super::super::NativeGraphError::StalePreparation) => {}
            Err(error) => panic!("orphan round {round} failed: {error:?}"),
        }
        for unlinked in delete_events(&vfs.take()) {
            let name = unlinked.file_name().expect("deleted name").to_os_string();
            assert!(
                pending.contains(&unlinked),
                "round {round} unlinked {name:?} without a durable intent naming it"
            );
            let size = *sizes.get(&name).expect("deleted file existed");
            assert!(deleted.insert(name, size).is_none(), "double unlink");
        }
        if first_spill_files.is_none() {
            first_spill_files = Some(
                directory_image(&path)
                    .into_keys()
                    .filter(|name| !sizes.contains_key(name))
                    .collect(),
            );
        }
        store
            .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
            .expect("checkpoint between orphan rounds");
        if complete.iter().all(|(name, _)| deleted.contains_key(name)) {
            break;
        }
    }
    for (name, bytes) in &complete {
        assert_eq!(
            deleted.get(name),
            Some(&(bytes.len() as u64)),
            "complete pre-WAL orphan {name:?} was not reclaimed"
        );
    }
    let first_spill_files = first_spill_files.expect("first round private files");
    assert!(!first_spill_files.is_empty());
    assert!(
        first_spill_files
            .iter()
            .any(|name| deleted.contains_key(name)),
        "no retired spill or proof file of the first round was ever reclaimed"
    );
    assert_eq!(removed_bytes, deleted.values().sum::<u64>());
    for (name, bytes) in &retained {
        assert_eq!(
            std::fs::read(path.join(name)).ok().as_ref(),
            Some(bytes),
            "cleanup touched {name:?}"
        );
    }
    let (lists_after, children_after) = vfs.enumeration_calls();
    assert_eq!(
        lists_after, lists_before,
        "orphan selection loaded a directory list"
    );
    assert!(children_after > children_before);
    let _ = orphan_name;

    // Both committed nodes survive, and the store reopens once the two files
    // that recovery itself refuses (foreign store, unknown family) are gone.
    store.close().expect("close orphan store");
    for name in retained.keys() {
        let bytes = &retained[name];
        if bytes.len() >= 96 && bytes != b"foreign-owner" {
            std::fs::remove_file(path.join(name)).expect("remove refused fixture file");
        }
    }
    let reopened = Store::open_native_graph(&path, options(), None).expect("reopen orphan store");
    assert_eq!(
        reopened
            .admit_native_read()
            .expect("reopened reader")
            .bundle()
            .high_waters()
            .node,
        2
    );
    reopened.close().expect("close reopened orphan store");
}

#[test]
fn ze46_orphan_adoption_waits_for_a_quiescent_history() {
    run_ze46_orphan_adoption_waits_for_a_quiescent_history();
}

fn run_ze46_orphan_adoption_waits_for_a_quiescent_history() {
    // While WAL envelopes still name earlier proofs, their private pages look
    // exactly like orphans. Nothing may adopt or unlink them until a
    // checkpoint has cut that history; the store must reopen at every point.
    let parent = super::tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let vfs = Arc::new(RecordingVfs::default());
    let store = create_reclaim_test_store(&path, &vfs);
    let image = CanonicalContents::node(&mut [], &mut [], Some("quiescent"), None).expect("node");
    store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "quiescent", "node").expect("key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("seed write");
    // No checkpoint ever happens here, so every proof stays named by the WAL.
    let before = directory_image(&path);
    vfs.take();
    for round in 0..6 {
        let report = commit_maintenance(&store)
            .unwrap_or_else(|error| panic!("round {round} failed: {error:?}"));
        assert_eq!(
            report.removed_bytes, 0,
            "round {round} unlinked without a checkpoint"
        );
        let lease = store.admit_native_read().expect("round reader");
        assert!(
            lease.bundle().reclaim().is_none(),
            "round {round} selected candidates while the WAL protects every allocation"
        );
    }
    assert!(delete_events(&vfs.take()).is_empty());
    let after = directory_image(&path);
    for (name, bytes) in &before {
        let current = after.get(name).expect("preexisting file survives");
        if name.to_string_lossy().starts_with("graph-wal-") {
            assert!(current.starts_with(bytes), "WAL prefix changed: {name:?}");
        } else {
            assert_eq!(current, bytes, "live file changed: {name:?}");
        }
    }
    assert!(after.len() > before.len());
    store.close().expect("close uncheckpointed store");
    let reopened = Store::open_native_graph(&path, options(), None)
        .expect("reopen with every proof still named by the WAL");
    reopened.close().expect("close reopened store");
}

/// Pending-intent candidates, or nothing when no intent is rooted.
fn pending_reclaim_candidates_or_empty_root(
    store: &Store,
    lease: &super::super::NativeReadLease,
) -> Vec<ArtifactDescriptor> {
    let Some(required) = lease.bundle().reclaim() else {
        return Vec::new();
    };
    let shared = crate::property_graph::resources::GraphResources::from_store(store)
        .expect("shared resources");
    let control = QueryControl::Cancel(CancelToken::new());
    let writer = WriteMemory::new(&shared, WriteLimits::default()).expect("write memory");
    let memory = StorageMemory::new(&writer, &control, 8 * 1024 * 1024).expect("storage memory");
    let source = NativePreparationSource::new(lease, &memory, 1).expect("reclaim source");
    let mut resources = source.resources(32 * 1024 * 1024).expect("tree resources");
    let block = source
        .resolve(required.block, &mut resources)
        .expect("resolve reclaim state");
    match crate::property_graph::storage::reclaim::decode_pending_intent_manifest(block.payload()) {
        Ok(manifest) => (0..manifest.candidate_count)
            .map(|index| {
                crate::property_graph::storage::reclaim::pending_intent_candidate_at(
                    block.payload(),
                    index,
                )
                .expect("pending candidate")
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

pub(super) fn reclaim_candidate_path(
    directory: &Path,
    candidate: &ArtifactDescriptor,
) -> std::path::PathBuf {
    if candidate.family == 19 {
        directory.join(format!("graph-wal-{:032x}.ze", candidate.artifact.get()))
    } else {
        crate::property_graph::storage::allocation::artifact_path(directory, candidate.artifact)
    }
}

/// Every artifact the rooted pending intent authorizes an unlink for: the
/// genuine object candidates plus ZE-165's tagged partial-target partition.
fn pending_reclaim_targets_or_empty_root(
    store: &Store,
    lease: &super::super::NativeReadLease,
) -> Vec<std::path::PathBuf> {
    let Some(required) = lease.bundle().reclaim() else {
        return Vec::new();
    };
    let shared = crate::property_graph::resources::GraphResources::from_store(store)
        .expect("shared resources");
    let control = QueryControl::Cancel(CancelToken::new());
    let writer = WriteMemory::new(&shared, WriteLimits::default()).expect("write memory");
    let memory = StorageMemory::new(&writer, &control, 8 * 1024 * 1024).expect("storage memory");
    let source = NativePreparationSource::new(lease, &memory, 1).expect("reclaim source");
    let mut resources = source.resources(32 * 1024 * 1024).expect("tree resources");
    let block = source
        .resolve(required.block, &mut resources)
        .expect("resolve reclaim state");
    let payload = block.payload();
    match crate::property_graph::storage::reclaim::decode_pending_intent_manifest(payload) {
        Ok(manifest) => (0..manifest.candidate_count)
            .map(|index| {
                reclaim_candidate_path(
                    lease.bundle().directory(),
                    &crate::property_graph::storage::reclaim::pending_intent_candidate_at(
                        payload, index,
                    )
                    .expect("pending candidate"),
                )
            })
            .chain((0..manifest.partial_count).map(|index| {
                crate::property_graph::storage::allocation::artifact_path(
                    lease.bundle().directory(),
                    crate::property_graph::storage::reclaim::pending_intent_partial_at(
                        payload, index,
                    )
                    .expect("pending partial target")
                    .artifact,
                )
            }))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// The tagged partial partition of the rooted pending intent, or nothing when
/// no intent is rooted.
fn pending_reclaim_partials_or_empty_root(
    store: &Store,
    lease: &super::super::NativeReadLease,
) -> Vec<crate::property_graph::storage::reclaim::PartialTarget> {
    let Some(required) = lease.bundle().reclaim() else {
        return Vec::new();
    };
    let shared = crate::property_graph::resources::GraphResources::from_store(store)
        .expect("shared resources");
    let control = QueryControl::Cancel(CancelToken::new());
    let writer = WriteMemory::new(&shared, WriteLimits::default()).expect("write memory");
    let memory = StorageMemory::new(&writer, &control, 8 * 1024 * 1024).expect("storage memory");
    let source = NativePreparationSource::new(lease, &memory, 1).expect("reclaim source");
    let mut resources = source.resources(32 * 1024 * 1024).expect("tree resources");
    let block = source
        .resolve(required.block, &mut resources)
        .expect("resolve reclaim state");
    let payload = block.payload();
    match crate::property_graph::storage::reclaim::decode_pending_intent_manifest(payload) {
        Ok(manifest) => (0..manifest.partial_count)
            .map(|index| {
                crate::property_graph::storage::reclaim::pending_intent_partial_at(payload, index)
                    .expect("pending partial target")
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[test]
fn ze165_reclaims_a_partial_create_and_retains_every_other_prefix() {
    run_ze165_reclaims_a_partial_create_and_retains_every_other_prefix();
}

/// ZE-165. A real `PartialCreate` fault leaves a file whose 96-byte header is
/// intact but whose body is not. It can never carry a whole-file checksum, so
/// ZE-46 could not adopt it and retained it. Here a later completed proof
/// selects it into the intent's tagged partial partition, and the unlink
/// follows that durable intent.
///
/// Three prefixes that look similar must survive byte-identical: a headerless
/// one from a `Create` collision, a foreign store's *interrupted* create whose
/// header shape is identical but whose store id is not, and a non-native name.
fn run_ze165_reclaims_a_partial_create_and_retains_every_other_prefix() {
    use super::publication::FaultPoint;
    let parent = super::tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let vfs = Arc::new(RecordingVfs::default());
    let store = create_reclaim_test_store(&path, &vfs);
    let write = |name: &str| {
        let image = CanonicalContents::node(&mut [], &mut [], Some(name), None).expect("node");
        store.apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "partials", name).expect("key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
    };
    write("first").expect("first commit");

    // Real crash debris, produced by the fault VFS inside a real write.
    let mut debris = Vec::new();
    for (point, label) in [
        (FaultPoint::PartialCreate, "partial"),
        (FaultPoint::Create, "headerless"),
    ] {
        let before = directory_image(&path);
        vfs.arm_fault(point);
        let Err(error) = write("second") else {
            panic!("{label} fault did not fail the write");
        };
        assert!(
            matches!(error, super::super::NativeGraphError::Io { .. }),
            "{label}: {error:?}"
        );
        vfs.assert_fired_once();
        let created: Vec<_> = directory_image(&path)
            .into_iter()
            .filter(|(name, _)| !before.contains_key(name))
            .collect();
        assert_eq!(created.len(), 1, "{label} fault left {created:?}");
        debris.push(created.into_iter().next().expect("one debris file"));
    }
    write("second").expect("clean retry");
    let (partial_name, partial_bytes) = debris.first().cloned().expect("partial debris");
    let (headerless_name, headerless_bytes) = debris.get(1).cloned().expect("headerless debris");
    assert!(
        partial_bytes.len() >= 96,
        "the partial create must keep an intact header"
    );
    assert!(
        headerless_bytes.len() < 96,
        "the headerless prefix must be shorter than one header"
    );
    let partial_artifact = artifact_of_name(&partial_name);

    // Files reclamation must never touch. The foreign partial has the exact
    // header shape of the reclaimable one and differs only in its store id.
    let mut retained: BTreeMap<std::ffi::OsString, Vec<u8>> =
        [(headerless_name.clone(), headerless_bytes)]
            .into_iter()
            .collect();
    retained.insert("notes.txt".into(), b"not a native name".to_vec());
    let foreign_parent = super::tempfile::tempdir().expect("foreign parent");
    let foreign_path = foreign_parent.path().join("native");
    let foreign_store =
        Store::create_native_graph(&foreign_path, options(), None).expect("foreign store");
    let image = CanonicalContents::node(&mut [], &mut [], None, None).expect("foreign node");
    foreign_store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "foreign", "node").expect("key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("foreign commit");
    foreign_store.close().expect("close foreign store");
    let (foreign_name, foreign_bytes) = directory_image(&foreign_path)
        .into_iter()
        .find(|(name, bytes)| name.to_string_lossy().ends_with(".zgraph") && bytes.len() > 200)
        .expect("foreign object");
    let foreign_partial = foreign_bytes
        .get(..foreign_bytes.len() / 2)
        .expect("foreign prefix")
        .to_vec();
    assert!(foreign_partial.len() >= 96);
    retained.insert(foreign_name, foreign_partial);
    for (name, bytes) in &retained {
        // The headerless prefix is already on disk: the fault left it there.
        if !path.join(name).exists() {
            std::fs::write(path.join(name), bytes).expect("plant retained file");
        }
    }

    // Maintenance with checkpoints between calls. Every unlink must name a
    // target of the pending intent that was durable before it ran.
    let mut removed_bytes = 0_u64;
    let mut deleted: BTreeMap<std::ffi::OsString, u64> = BTreeMap::new();
    let mut authorizing: Option<crate::property_graph::storage::reclaim::PartialTarget> = None;
    for round in 0..40 {
        let sizes: BTreeMap<_, _> = directory_image(&path)
            .into_iter()
            .map(|(name, bytes)| (name, bytes.len() as u64))
            .collect();
        let (targets, partials) = {
            let lease = store.admit_native_read().expect("round reader");
            (
                pending_reclaim_targets_or_empty_root(&store, &lease),
                pending_reclaim_partials_or_empty_root(&store, &lease),
            )
        };
        vfs.take();
        match commit_maintenance(&store) {
            Ok(report) => removed_bytes += report.removed_bytes,
            Err(super::super::NativeGraphError::StalePreparation) => {}
            Err(error) => panic!("partial round {round} failed: {error:?}"),
        }
        for unlinked in delete_events(&vfs.take()) {
            let name = unlinked.file_name().expect("deleted name").to_os_string();
            assert!(
                targets.contains(&unlinked),
                "round {round} unlinked {name:?} without a durable intent naming it"
            );
            if name == partial_name {
                authorizing = partials
                    .iter()
                    .copied()
                    .find(|target| target.artifact == partial_artifact);
            }
            let size = *sizes.get(&name).expect("deleted file existed");
            assert!(deleted.insert(name, size).is_none(), "double unlink");
        }
        store
            .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
            .expect("checkpoint between partial rounds");
        if deleted.contains_key(&partial_name) {
            break;
        }
    }

    // The interrupted create was reclaimed, and the intent that authorized it
    // recorded the bytes actually observed rather than any declared length.
    assert_eq!(
        deleted.get(&partial_name),
        Some(&(partial_bytes.len() as u64)),
        "the partial create was not reclaimed"
    );
    let authorizing = authorizing.expect("no partial target authorized the unlink");
    assert_eq!(authorizing.artifact, partial_artifact);
    assert_eq!(authorizing.observed, partial_bytes.len() as u64);
    assert!(
        authorizing.declared > authorizing.observed,
        "an interrupted prefix declares more than it holds"
    );
    assert_eq!(
        authorizing.digest,
        xxhash_rust::xxh3::xxh3_64(&partial_bytes),
        "the intent digested bytes other than the ones on disk"
    );
    assert_eq!(removed_bytes, deleted.values().sum::<u64>());

    // Every look-alike prefix is still there, byte for byte.
    for (name, bytes) in &retained {
        assert_eq!(
            std::fs::read(path.join(name)).ok().as_ref(),
            Some(bytes),
            "reclamation touched {name:?}"
        );
    }

    store.close().expect("close partial store");
    for name in retained.keys() {
        if retained[name].len() >= 96 {
            std::fs::remove_file(path.join(name)).expect("remove refused fixture file");
        }
    }
    let reopened = Store::open_native_graph(&path, options(), None).expect("reopen partial store");
    assert_eq!(
        reopened
            .admit_native_read()
            .expect("reopened reader")
            .bundle()
            .high_waters()
            .node,
        2
    );
    reopened.close().expect("close reopened partial store");
}

/// The artifact id a canonical `graph-<32 hex>.zgraph` name encodes.
fn artifact_of_name(
    name: &std::ffi::OsStr,
) -> crate::property_graph::storage::artifact::ArtifactId {
    let text = name.to_string_lossy();
    let digits = text
        .strip_prefix("graph-")
        .and_then(|rest| rest.strip_suffix(".zgraph"))
        .expect("canonical native object name");
    crate::property_graph::storage::artifact::ArtifactId::new(
        u128::from_str_radix(digits, 16).expect("artifact digits"),
    )
    .expect("artifact id")
}

#[test]
fn ze46_spill_merge_and_incomplete_mark_never_delete_candidates() {
    run_ze46_spill_merge_and_incomplete_mark_never_delete_candidates();
}

fn run_ze46_spill_merge_and_incomplete_mark_never_delete_candidates() {
    let parent = super::tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let store = Store::create_native_graph(&path, options(), None).expect("fresh native store");
    for (key, text) in [
        ("multi-a", "first physical object"),
        ("multi-b", "second physical object"),
        ("multi-c", "third physical object"),
    ] {
        let image =
            CanonicalContents::node(&mut [], &mut [], Some(text), None).expect("node image");
        store
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "spill", key)
                        .expect("application key"),
                    revision: GraphRevision::new(1).expect("revision"),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("separate physical graph commit");
    }
    let graph_admission = store
        .admit_native_graph_maintenance()
        .expect("multi-object maintenance admission");
    let graph_report = store
        .commit_native_graph_maintenance(
            &graph_admission,
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("multi-object bounded trace maintenance");
    assert!(graph_report.replaced_physical_refs > 0);
    drop(graph_admission);
    let admission = store
        .admit_native_graph_maintenance()
        .expect("maintenance admission");
    let before = directory_image(&path);

    let report =
        super::super::maintenance::run_spill_probe(&store, &admission, &[9, 2, 7, 2, 5], 2)
            .expect("durable spill/merge probe");
    assert_eq!(report.ordered, vec![2, 5, 7, 9]);
    assert!(report.spill_runs >= 2, "test cap forced multiple disk runs");
    assert!(report.merges >= 1, "multiple disk runs forced a real merge");
    assert!(
        report.max_batch <= 2,
        "sort memory obeyed the requested cap"
    );
    assert!(report.created_objects >= 4);
    assert_eq!(report.created_objects % 2, 0);
    assert!(report.disk_bytes > 0);
    assert!(report.maximum_encoded_backing < 8 * 1024);
    assert!(report.charged_peak_bytes <= 4 * 1024 * 1024);
    assert!(report.read_windows > 0);
    assert!(report.maximum_mapped_window < 8 * 1024);
    assert!(report.released_each_read_window);
    assert!(
        report.mapped_bytes_released,
        "isolated probe released every scoped mapping before return"
    );
    assert_eq!(report.captured_head, report.allocation_head);
    assert_eq!(report.captured_pending, [None; 2]);
    let after = directory_image(&path);
    let created: Vec<_> = after
        .iter()
        .filter(|(name, _)| !before.contains_key(*name))
        .collect();
    assert_eq!(created.len() as u64, report.created_objects);
    assert_eq!(
        created
            .iter()
            .map(|(_, bytes)| bytes.len() as u64)
            .sum::<u64>(),
        report.disk_bytes
    );

    let run_name = format!(
        "graph-{:032x}.zgraph",
        report.run.root.object.artifact.get()
    );
    let run_bytes = after
        .get(&std::ffi::OsString::from(run_name))
        .expect("durable run root bytes");
    let frame = crate::property_graph::storage::artifact::decode(
        crate::property_graph::storage::artifact::ContainerKind::Object,
        Some((
            report.run.root.object.store,
            report.run.root.object.artifact,
        )),
        run_bytes,
    )
    .expect("reopen framed run root");
    let block = frame
        .framed_block(report.run.root.block)
        .expect("reopen exact run block");
    assert_eq!(
        block.payload().get(..8),
        Some(b"ZGCP\x04\0\x01\0".as_slice())
    );

    let allocation = report.allocation_head.expect("allocation head");
    let allocation_name = format!("graph-{:032x}.zgraph", allocation.object.artifact.get());
    let allocation_bytes = after
        .get(&std::ffi::OsString::from(allocation_name))
        .expect("durable allocation root bytes");
    let allocation_frame = crate::property_graph::storage::artifact::decode(
        crate::property_graph::storage::artifact::ContainerKind::Object,
        Some((allocation.object.store, allocation.object.artifact)),
        allocation_bytes,
    )
    .expect("reopen framed allocation root");
    assert_eq!(
        allocation_frame
            .framed_block(allocation.block)
            .expect("reopen exact allocation block")
            .payload()
            .get(..8),
        Some(b"ZGCP\x05\0\x01\0".as_slice())
    );

    drop(admission);
    store.close().expect("close spill store");

    // Each refusal fires on the real maintenance path while one eligible
    // reclaim candidate waits, so a refusal that deleted or published
    // anything would be visible. The clean counterpart then selects it.
    let refusal_parent = super::tempfile::tempdir().expect("refusal parent");
    let path = refusal_parent.path().join("native");
    let vfs = Arc::new(RecordingVfs::default());
    let store = create_reclaim_test_store(&path, &vfs);
    seed_reclaimable_manifest(&store, "refusal");
    let (base_generation, sparse_path, sparse_offset) = {
        let lease = store.admit_native_read().expect("refusal base reader");
        assert!(lease.bundle().reclaim().is_none());
        let text = lease.bundle().text().expect("actual sparse text root");
        (
            lease.bundle().base().generation,
            path.join(format!("graph-{:032x}.zgraph", text.object.artifact.get())),
            text.block.offset + u64::from(text.block.length) / 2,
        )
    };
    // Work budgets come from the measured stage boundaries of this fixture:
    // the fold ends near 1.6M, the graph mark runs 1.8M..2.7M and the sparse
    // trace 2.7M..3.3M. Each budget lands mid-stage with >0.2M on each side.
    for refusal in [
        "memory",
        "work before the mark",
        "work during the graph mark",
        "work during the sparse trace",
        "cancel after the first spill create",
        "spill create collision",
        "spill disk-full partial create",
        "corrupt sparse root artifact",
    ] {
        let before = directory_image(&path);
        let token = CancelToken::new();
        let control = QueryControl::Cancel(token.clone());
        let mut limits = super::super::maintenance::MaintenanceLimits::default();
        let mut restore = None;
        match refusal {
            "memory" => limits.storage_bytes = 256 * 1024,
            "work before the mark" => limits.work = 4 * 1024,
            "work during the graph mark" => limits.work = 2_000_000,
            "work during the sparse trace" => limits.work = 3_000_000,
            "cancel after the first spill create" => {
                let token = token.clone();
                vfs.after_next_create(move || token.cancel());
            }
            "spill create collision" => vfs.arm_fault(super::publication::FaultPoint::Create),
            "spill disk-full partial create" => {
                vfs.arm_fault(super::publication::FaultPoint::PartialCreate);
            }
            _ => {
                let mut file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&sparse_path)
                    .expect("open sparse root artifact");
                let mut byte = [0_u8; 1];
                file.seek(SeekFrom::Start(sparse_offset)).expect("seek");
                file.read_exact(&mut byte).expect("read sparse byte");
                file.seek(SeekFrom::Start(sparse_offset)).expect("seek");
                file.write_all(&[byte[0] ^ 0xff])
                    .expect("corrupt sparse byte");
                restore = Some(byte[0]);
            }
        }
        vfs.take();
        let admission = store
            .admit_native_graph_maintenance()
            .expect("refusal maintenance admission");
        let error = store
            .commit_native_graph_maintenance_with_limits(&admission, &control, limits)
            .expect_err(refusal);
        drop(admission);
        if let Some(byte) = restore {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(&sparse_path)
                .expect("reopen sparse root artifact");
            file.seek(SeekFrom::Start(sparse_offset)).expect("seek");
            file.write_all(&[byte]).expect("restore sparse byte");
        }
        use super::super::NativeGraphError as E;
        use crate::property_graph::storage::tree::directory::TreeError as T;
        let typed = match refusal {
            "memory" => matches!(error, E::Read(T::Memory)),
            "work before the mark"
            | "work during the graph mark"
            | "work during the sparse trace" => matches!(error, E::Read(T::Work)),
            "cancel after the first spill create" => {
                assert!(!vfs.after_create_is_armed(), "cancel hook never fired");
                matches!(error, E::Read(T::Control(_)))
            }
            "spill create collision" | "spill disk-full partial create" => {
                vfs.assert_fired_once();
                matches!(error, E::Io { .. })
            }
            _ => matches!(error, E::Read(T::Format(_))),
        };
        assert!(typed, "{refusal} returned {error:?}");
        if !matches!(
            refusal,
            "spill create collision" | "spill disk-full partial create"
        ) {
            super::publication::record_verified_fault();
        }
        let events = vfs.take();
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, DurabilityEvent::Delete(_))),
            "{refusal} refusal deleted a file: {events:?}"
        );
        assert_live_files_unchanged(&path, &before);
        let lease = store.admit_native_read().expect("reader after refusal");
        assert_eq!(
            lease.bundle().base().generation,
            base_generation,
            "{refusal} published a generation"
        );
        assert!(
            lease.bundle().reclaim().is_none(),
            "{refusal} published a reclaim intent"
        );
    }

    // Clean counterpart: same store, default budgets, no fault. The candidate
    // that every refusal left alone is now selected, and still not deleted
    // before its intent is durable.
    let before = directory_image(&path);
    vfs.take();
    let admission = store
        .admit_native_graph_maintenance()
        .expect("clean counterpart admission");
    let report = store
        .commit_native_graph_maintenance(&admission, &QueryControl::Cancel(CancelToken::new()))
        .expect("clean counterpart maintenance");
    drop(admission);
    assert_eq!(report.generation.get(), base_generation.get() + 1);
    assert_eq!(report.removed_bytes, 0);
    assert!(
        vfs.take()
            .iter()
            .all(|event| !matches!(event, DurabilityEvent::Delete(_)))
    );
    // The committed Maintenance envelope extends the active WAL; every other
    // preexisting byte stays where it was.
    let after = directory_image(&path);
    let mut grown = 0_usize;
    for (name, bytes) in &before {
        let current = after.get(name).expect("preexisting file survives");
        if name.to_string_lossy().starts_with("graph-wal-") {
            assert!(current.starts_with(bytes), "WAL prefix changed: {name:?}");
            grown += usize::from(current.len() > bytes.len());
        } else {
            assert_eq!(current, bytes, "live file changed: {name:?}");
        }
    }
    assert_eq!(grown, 1, "exactly the active WAL takes the envelope");
    let pending = store.admit_native_read().expect("clean pending reader");
    let candidates = pending_reclaim_candidates(&store, &pending);
    assert!(
        !candidates.is_empty(),
        "the refusals ran without an eligible candidate"
    );
    for candidate in &candidates {
        let name = reclaim_candidate_path(&path, candidate)
            .file_name()
            .unwrap()
            .to_os_string();
        assert!(
            before.contains_key(&name),
            "candidate postdates the refusals"
        );
    }
    drop(pending);
    store.close().expect("close refusal store");
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CrashCell {
    Control,
    BeforeIntent,
    BeforeFirstUnlink,
    AfterOneUnlink,
    DirectorySync,
    BeforeCompletion,
    LostCompletionAck,
}

struct CrashHistory {
    provenance_phases: Vec<Ze166FenceEvidence>,
    _parent: super::tempfile::TempDir,
    path: std::path::PathBuf,
    vfs: Arc<RecordingVfs>,
    document: EmbeddingTower,
    first: crate::property_graph::NodeId,
    peer: crate::property_graph::NodeId,
    relationship: crate::property_graph::RelId,
}

impl CrashHistory {
    fn open(&self, options: OpenOptions) -> Result<Store, super::super::NativeGraphError> {
        let infrastructure: Arc<dyn Vfs> = self.vfs.clone();
        Store::open_native_graph_with_infrastructure(
            &self.path,
            options,
            Some(self.document.clone()),
            infrastructure,
            Arc::new(crate::lifecycle::SystemMonotonicClock),
        )
    }

    fn oracle(&self, store: &Store) -> [super::publication::GenerationSnapshot; 2] {
        let lease = store.admit_native_read().expect("oracle reader");
        [
            snapshot_for_lease(
                store,
                &lease,
                self.first,
                self.peer,
                self.relationship,
                None,
            ),
            snapshot_for_lease(
                store,
                &lease,
                self.peer,
                self.first,
                self.relationship,
                None,
            ),
        ]
    }
}

fn assert_same_logical_state(
    before: &[super::publication::GenerationSnapshot; 2],
    after: &[super::publication::GenerationSnapshot; 2],
) {
    for (before, after) in before.iter().zip(after) {
        assert!(after.generation >= before.generation);
        assert_eq!(after.revision, before.revision);
        assert_eq!(after.original_generation, before.original_generation);
        assert_eq!(after.canonical, before.canonical);
        assert_eq!(after.text, before.text);
        assert_eq!(after.vector, before.vector);
        assert_eq!(after.old_relationship, before.old_relationship);
        assert_eq!(after.out, before.out);
        assert_eq!(after.incoming, before.incoming);
        assert_eq!(after.sparse_text, before.sparse_text);
        assert_eq!(after.sparse_vector, before.sparse_vector);
    }
}

/// The one keyed request of the crash-table history. Applying it again after
/// any maintenance must replay the original receipts.
fn apply_crash_history_writes(
    store: &Store,
    document: &EmbeddingTower,
) -> super::super::write::NativePreparedResult<super::super::write::ReceiptRegistration> {
    crate::property_graph::with_local_refs(|refs| {
        let first_embedding =
            CanonicalEmbedding::new(document, &[0.25_f32, 0.75_f32]).expect("embedding");
        let first =
            CanonicalContents::node(&mut [], &mut [], Some("crash first"), Some(first_embedding))
                .expect("first node");
        let peer_embedding =
            CanonicalEmbedding::new(document, &[0.5_f32, 1.0_f32]).expect("peer embedding");
        let peer =
            CanonicalContents::node(&mut [], &mut [], Some("crash peer"), Some(peer_embedding))
                .expect("peer node");
        store
            .apply_native_graph(
                &[
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "crash", "first")
                            .expect("first key"),
                        revision: GraphRevision::new(1).expect("revision"),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&first)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "crash", "peer")
                            .expect("peer key"),
                        revision: GraphRevision::new(1).expect("revision"),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&peer)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Relationship, "crash", "edge")
                            .expect("relationship key"),
                        revision: GraphRevision::new(1).expect("revision"),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Relationship {
                            source: NodeRef::Local(refs.node(0).expect("first local node")),
                            target: NodeRef::Local(refs.node(1).expect("peer local node")),
                            relationship_type: GraphName::new("LINKS").expect("type"),
                            properties: &[],
                        }),
                    },
                ],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("seed crash-table history")
    })
}

/// Two embedded nodes and one edge, two physical replacements, then a
/// checkpoint: the next maintenance has several dead whole objects to select.
fn seed_crash_history() -> (CrashHistory, Store) {
    let parent = super::tempfile::tempdir().expect("crash parent");
    let path = parent.path().join("native");
    let vfs = Arc::new(RecordingVfs::default());
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let document = EmbeddingTower {
        model_id: "ze46-crash-table".into(),
        model_version: "1".into(),
        weights_digest: vec![0x46, 0x09],
        dims: 2,
        normalization: Normalization::None,
        prompt_prefix: "doc: ".into(),
        max_tokens: 32,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    let store = Store::create_native_graph_with_infrastructure(
        &path,
        options(),
        Some(document.clone()),
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh crash-table store");
    let receipts = apply_crash_history_writes(&store, &document);
    let mut provenance_phases = vec![ze166_fence_evidence(&store)];
    let node = |index: usize| match receipts[index].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("node receipt identity"),
    };
    let relationship = match receipts[2].entity {
        EntityId::Relationship(relationship) => relationship,
        EntityId::Node(_) => panic!("relationship receipt identity"),
    };
    for _ in 0..2 {
        let admission = store
            .admit_native_graph_maintenance()
            .expect("crash-table replacement admission");
        store
            .commit_native_graph_maintenance(&admission, &QueryControl::Cancel(CancelToken::new()))
            .expect("crash-table physical replacement");
    }
    provenance_phases.push(ze166_fence_evidence(&store));
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .expect("checkpoint releases crash-table WAL protection");
    provenance_phases.push(ze166_fence_evidence(&store));
    (
        CrashHistory {
            provenance_phases,
            _parent: parent,
            path,
            vfs,
            document,
            first: node(0),
            peer: node(1),
            relationship,
        },
        store,
    )
}

pub(super) fn commit_maintenance(
    store: &Store,
) -> Result<super::super::maintenance::NativeMaintenanceReport, super::super::NativeGraphError> {
    let admission = store
        .admit_native_graph_maintenance()
        .expect("crash-table maintenance admission");
    store.commit_native_graph_maintenance(&admission, &QueryControl::Cancel(CancelToken::new()))
}

fn delete_events(events: &[DurabilityEvent]) -> Vec<std::path::PathBuf> {
    events
        .iter()
        .filter_map(|event| match event {
            DurabilityEvent::Delete(path) => Some(path.clone()),
            _ => None,
        })
        .collect()
}

fn run_reclaim_crash_cell(cell: CrashCell) {
    let _ = run_reclaim_crash_cell_observed(cell, false);
}
fn run_reclaim_crash_cell_observed(
    cell: CrashCell,
    retain: bool,
) -> crate::graph_commit_recovery_test_support::ReclaimEvidence {
    use super::publication::FaultPoint;
    let (history, mut store) = seed_crash_history();
    let vfs = Arc::clone(&history.vfs);
    let oracle = history.oracle(&store);
    let mut error_observation = None;
    let input_image = directory_image(&history.path);

    if cell == CrashCell::BeforeIntent {
        let before = directory_image(&history.path);
        vfs.take();
        vfs.arm_fault(FaultPoint::Append);
        let error = commit_maintenance(&store).expect_err("intent WAL append fault");
        error_observation = Some(
            format!("{error:?}").replace(&history.path.to_string_lossy().to_string(), "<store>"),
        );
        assert!(
            matches!(
                error,
                super::super::NativeGraphError::CommitIndeterminate { .. }
            ),
            "{error:?}"
        );
        vfs.assert_fired_once();
        assert!(delete_events(&vfs.take()).is_empty());
        assert_live_files_unchanged(&history.path, &before);
        store.close().expect("close store stopped before intent");
        store = history
            .open(options())
            .expect("reopen without durable intent");
        let lease = store.admit_native_read().expect("no-intent reader");
        assert!(lease.bundle().reclaim().is_none(), "intent was not durable");
        drop(lease);
        assert!(delete_events(&vfs.take()).is_empty());
        assert_live_files_unchanged(&history.path, &before);
    }

    vfs.take();
    let intent = commit_maintenance(&store).expect("durable reclaim intent");
    assert_eq!(intent.removed_bytes, 0);
    assert!(delete_events(&vfs.take()).is_empty());
    let pending = store.admit_native_read().expect("pending reader");
    let candidates = pending_reclaim_candidates(&store, &pending);
    let inventory_pending = format!(
        "{:?}",
        complete_inventory_union_for_lease(&store, &pending)
            .into_iter()
            .filter(|r| candidates.iter().any(|c| c.artifact == r.object.artifact))
            .collect::<Vec<_>>()
    );
    let pending_proof = format!("{:?}", pending_reclaim_proof_for_lease(&store, &pending));
    drop(pending);
    if retain {
        store
            .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
            .expect("checkpoint pending proof");
    }
    let checkpoint = store.admit_native_read().unwrap();
    let replayed_proof = format!("{:?}", pending_reclaim_proof_for_lease(&store, &checkpoint));
    let inventory_replayed = format!(
        "{:?}",
        complete_inventory_union_for_lease(&store, &checkpoint)
            .into_iter()
            .filter(|r| candidates.iter().any(|c| c.artifact == r.object.artifact))
            .collect::<Vec<_>>()
    );
    drop(checkpoint);
    assert!(
        candidates.len() >= 2,
        "one unlink must leave work: {candidates:?}"
    );
    let targets: Vec<_> = candidates
        .iter()
        .map(|candidate| reclaim_candidate_path(&history.path, candidate))
        .collect();
    let candidate_bytes: u64 = candidates
        .iter()
        .map(|candidate| u64::from(candidate.bytes))
        .sum();
    for (target, candidate) in targets.iter().zip(&candidates) {
        assert_eq!(
            std::fs::metadata(target).expect("pending target").len(),
            u64::from(candidate.bytes),
            "intent names the exact on-disk length"
        );
    }

    // The fault fires inside the real resume operation of the open store.
    match cell {
        CrashCell::Control | CrashCell::BeforeIntent => {}
        CrashCell::BeforeFirstUnlink => vfs.arm_fault(FaultPoint::Delete),
        CrashCell::AfterOneUnlink => vfs.arm_fault_after(FaultPoint::Delete, 1),
        CrashCell::DirectorySync => vfs.arm_fault(FaultPoint::DirectorySync),
        CrashCell::BeforeCompletion => vfs.arm_fault(FaultPoint::Append),
        CrashCell::LostCompletionAck => vfs.arm_fault(FaultPoint::WalSync),
    }
    let faulted = !matches!(cell, CrashCell::Control | CrashCell::BeforeIntent);
    let resumed = commit_maintenance(&store);
    let first_events = vfs.take();
    let first_deletes = delete_events(&first_events);
    if faulted {
        let error = resumed.expect_err("armed resume fault");
        error_observation = Some(
            format!("{error:?}").replace(&history.path.to_string_lossy().to_string(), "<store>"),
        );
        vfs.assert_fired_once();
        let expected_deletes = match cell {
            CrashCell::BeforeFirstUnlink => 0,
            CrashCell::AfterOneUnlink => 1,
            _ => targets.len(),
        };
        assert_eq!(first_deletes.len(), expected_deletes, "{cell:?}: {error:?}");
    } else {
        let report = resumed.expect("clean in-process resume");
        assert_eq!(report.reclaimed_bytes, candidate_bytes);
        assert_eq!(report.removed_bytes, candidate_bytes);
        assert_eq!(report.already_missing, 0);
        assert_eq!(first_deletes.len(), targets.len());
    }
    assert_unlink_order(&first_events, cell);
    store.close().expect("close crashed reclaim store");

    // Read-only recovery adopts whatever partition the crash left and
    // performs no VFS mutation at all, not even a same-byte rewrite.
    let before_read_only = directory_image(&history.path);
    let read_only = history
        .open(
            OpenOptions::read_only()
                .with_durability(DurabilityMode::Durable, CommitTier::Durable)
                .with_max_resident_bytes(256 * 1024 * 1024),
        )
        .expect("read-only recovery over the crashed partition");
    assert_same_logical_state(&oracle, &history.oracle(&read_only));
    read_only.close().expect("close read-only crashed store");
    let read_only_events = vfs.take();
    assert!(
        read_only_events.is_empty(),
        "{cell:?}: read-only recovery mutated the VFS: {read_only_events:?}"
    );
    assert_eq!(directory_image(&history.path), before_read_only);

    // Writable recovery resumes exactly the unlinks that are still owed.
    let writable = history.open(options()).expect("writable resume");
    let resume_events = vfs.take();
    let resume_deletes = delete_events(&resume_events);
    let mut all_deletes = first_deletes.clone();
    all_deletes.extend(resume_deletes.iter().cloned());
    all_deletes.sort();
    let mut expected = targets.clone();
    expected.sort();
    assert_eq!(
        all_deletes, expected,
        "{cell:?}: each target unlinks exactly once"
    );
    assert_unlink_order(&resume_events, cell);
    for target in &targets {
        assert!(!target.exists(), "{cell:?}: completed target survives");
    }
    let completed = writable.admit_native_read().expect("completion reader");
    let (completion, completed_candidates) = completed_reclaim_for_lease(&writable, &completed);
    assert_eq!(completion.completed_count, candidates.len());
    assert_eq!(completion.remaining_count, 0);
    assert_eq!(completed_candidates, candidates);
    drop(completed);
    assert_same_logical_state(&oracle, &history.oracle(&writable));
    let mut provenance_phases = history.provenance_phases.clone();
    provenance_phases.push(ze166_fence_evidence(&writable));
    let retry = apply_crash_history_writes(&writable, &history.document);
    assert!(retry.iter().all(|r| r.replayed));
    let retry_receipts = format!("{:?}", retry.iter().collect::<Vec<_>>());
    let completion_observation = format!("{completion:?} {completed_candidates:?}");
    writable.close().expect("close resumed reclaim store");
    let final_image = directory_image(&history.path);
    let reopened = history.open(options()).expect("idempotent reopen");
    assert!(delete_events(&vfs.take()).is_empty());
    reopened.close().expect("close idempotent reopen");
    for (name, bytes) in &final_image {
        if !name.to_string_lossy().starts_with("graph-wal-") {
            assert_eq!(directory_image(&history.path).get(name), Some(bytes));
        }
    }
    crate::graph_commit_recovery_test_support::ReclaimEvidence {
        cell: format!("{cell:?}"),
        provenance_phases,
        inventory_pending,
        inventory_replayed,
        pending_proof,
        replayed_proof,
        completion: completion_observation,
        retry_receipts,
        error: error_observation,
        input_image: input_image
            .into_iter()
            .map(|(n, b)| (n.to_string_lossy().into_owned(), b))
            .collect(),
        durable_image: final_image
            .into_iter()
            .map(|(n, b)| (n.to_string_lossy().into_owned(), b))
            .collect(),
        fires: u64::from(cell != CrashCell::Control),
        controls: u64::from(cell == CrashCell::Control),
    }
}

/// The event right after the last unlink is the directory Full-sync: no
/// completion object or WAL byte may be written while an unlink is volatile.
/// The completion commit issues directory syncs of its own, so only
/// adjacency distinguishes the required sync from those.
fn assert_unlink_order(events: &[DurabilityEvent], cell: CrashCell) {
    let Some(last_delete) = events
        .iter()
        .rposition(|event| matches!(event, DurabilityEvent::Delete(_)))
    else {
        return;
    };
    let after = events.get(last_delete + 1..).unwrap_or_default();
    if let Some(next) = after.first() {
        assert!(
            matches!(next, DurabilityEvent::Sync(path, crate::vfs::SyncKind::Full) if path.is_dir()),
            "{cell:?}: {next:?} follows the last unlink before a directory Full-sync"
        );
    }
    let first_delete = events
        .iter()
        .position(|event| matches!(event, DurabilityEvent::Delete(_)))
        .unwrap_or(last_delete);
    assert!(
        events[first_delete..=last_delete]
            .iter()
            .all(|event| matches!(event, DurabilityEvent::Delete(_))),
        "{cell:?}: unlinks interleave with other durable work: {events:?}"
    );
}

fn completed_reclaim_for_lease(
    store: &Store,
    lease: &super::super::NativeReadLease,
) -> (
    crate::property_graph::storage::reclaim::CompletedIntentManifest,
    Vec<ArtifactDescriptor>,
) {
    let required = lease
        .bundle()
        .reclaim()
        .expect("durable reclaim completion");
    let shared = crate::property_graph::resources::GraphResources::from_store(store)
        .expect("completion resources");
    let control = QueryControl::Cancel(CancelToken::new());
    let writer = WriteMemory::new(&shared, WriteLimits::default()).expect("completion memory");
    let memory = StorageMemory::new(&writer, &control, 8 * 1024 * 1024).expect("storage memory");
    let source = NativePreparationSource::new(lease, &memory, 1).expect("completion source");
    let mut resources = source.resources(32 * 1024 * 1024).expect("tree resources");
    let block = source
        .resolve(required.block, &mut resources)
        .expect("resolve reclaim completion");
    let manifest =
        crate::property_graph::storage::reclaim::decode_completed_intent_manifest(block.payload())
            .expect("decode reclaim completion");
    let candidates = (0..manifest.completed_count)
        .map(|index| {
            crate::property_graph::storage::reclaim::completed_intent_candidate_at(
                block.payload(),
                index,
            )
            .expect("completed candidate")
        })
        .collect();
    (manifest, candidates)
}

#[test]
fn ze46_intent_unlink_sync_completion_crashes_resume_idempotently() {
    run_ze46_intent_unlink_sync_completion_crashes_resume_idempotently();
}

fn run_ze46_intent_unlink_sync_completion_crashes_resume_idempotently() {
    let parent = super::tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let store = Store::create_native_graph(&path, options(), None).expect("fresh native store");
    let image =
        CanonicalContents::node(&mut [], &mut [], Some("reclaim me"), None).expect("node image");
    store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "reclaim", "node")
                    .expect("application key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("seed reclaim object");
    let seeded = store.admit_native_read().expect("seeded reclaim reader");
    let superseded_manifest = seeded
        .bundle()
        .prepared_inventories()
        .first()
        .copied()
        .expect("seeded prepared manifest")
        .object;
    drop(seeded);
    let first = store
        .admit_native_graph_maintenance()
        .expect("first maintenance admission");
    store
        .commit_native_graph_maintenance(&first, &QueryControl::Cancel(CancelToken::new()))
        .expect("first physical replacement");
    drop(first);
    let replaced = store.admit_native_read().expect("replaced graph reader");
    assert!(
        !replaced
            .bundle()
            .prepared_inventories()
            .iter()
            .any(|required| required.object == superseded_manifest),
        "completed fold retires the superseded manifest root"
    );
    assert!(
        rooted_inventory_for_lease(&store, &replaced)
            .iter()
            .any(|change| change.object == superseded_manifest),
        "retired manifest remains in the allocation inventory"
    );
    drop(replaced);
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .expect("checkpoint releases historical WAL protection");

    let second = store
        .admit_native_graph_maintenance()
        .expect("intent maintenance admission");
    let intent_report = store
        .commit_native_graph_maintenance(&second, &QueryControl::Cancel(CancelToken::new()))
        .expect("durable reclaim intent");
    drop(second);
    assert_eq!(intent_report.removed_bytes, 0);
    let pending = store.admit_native_read().expect("pending reclaim reader");
    let candidates = pending_reclaim_candidates(&store, &pending);
    assert!(
        !candidates.is_empty(),
        "real dead objects entered the intent"
    );
    assert!(
        candidates.contains(&superseded_manifest),
        "superseded manifest is the named whole dead allocation"
    );
    for candidate in &candidates {
        assert!(
            reclaim_candidate_path(&path, candidate).exists(),
            "intent commit precedes unlink"
        );
    }
    drop(pending);
    store.close().expect("close pending reclaim store");

    let read_options = OpenOptions::read_only()
        .with_durability(DurabilityMode::Durable, CommitTier::Durable)
        .with_max_resident_bytes(256 * 1024 * 1024);
    let before_read_only = directory_image(&path);
    let read_only = Store::open_native_graph(&path, read_options, None)
        .expect("read-only pending reclaim recovery");
    assert_eq!(directory_image(&path), before_read_only);
    read_only.close().expect("close read-only pending reclaim");

    let writable = Store::open_native_graph(&path, options(), None)
        .expect("writable pending reclaim recovery");
    for candidate in &candidates {
        assert!(
            !reclaim_candidate_path(&path, candidate).exists(),
            "completed target is physically absent"
        );
    }
    let completed = writable
        .admit_native_read()
        .expect("automatically resumed reclaim reader");
    let completion = completed
        .bundle()
        .reclaim()
        .expect("durable reclaim completion");
    let shared = crate::property_graph::resources::GraphResources::from_store(&writable)
        .expect("shared completion resources");
    let control = QueryControl::Cancel(CancelToken::new());
    let writer = WriteMemory::new(&shared, WriteLimits::default()).expect("completion memory");
    let memory =
        StorageMemory::new(&writer, &control, 8 * 1024 * 1024).expect("completion storage memory");
    let source = NativePreparationSource::new(&completed, &memory, 1).expect("completion source");
    let mut resources = source
        .resources(32 * 1024 * 1024)
        .expect("completion resources");
    let block = source
        .resolve(completion.block, &mut resources)
        .expect("resolve reclaim completion");
    let manifest =
        crate::property_graph::storage::reclaim::decode_completed_intent_manifest(block.payload())
            .expect("decode reclaim completion");
    assert_eq!(manifest.completed_count, candidates.len());
    assert_eq!(manifest.remaining_count, 0);
    drop(completed);
    let checkpoint_admission = writable
        .admit_native_graph_maintenance()
        .expect("completed reclaim checkpoint admission");
    let error = writable
        .commit_native_graph_maintenance(
            &checkpoint_admission,
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect_err("completion checkpoint replaces its admission");
    assert!(matches!(
        error,
        super::super::NativeGraphError::StalePreparation
    ));
    drop(checkpoint_admission);
    let checkpointed = writable
        .admit_native_read()
        .expect("checkpointed completion reader");
    assert_eq!(
        checkpointed.bundle().root_envelope().object.generation,
        checkpointed.bundle().base().generation
    );
    assert!(checkpointed.bundle().reclaim().is_some());
    drop(checkpointed);
    let clear_admission = writable
        .admit_native_graph_maintenance()
        .expect("completed reclaim clear admission");
    writable
        .commit_native_graph_maintenance(
            &clear_admission,
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("clear and checkpoint completed reclaim");
    drop(clear_admission);
    let cleared = writable
        .admit_native_read()
        .expect("cleared reclaim reader");
    assert!(cleared.bundle().reclaim().is_none());
    assert_eq!(
        cleared.bundle().root_envelope().object.generation,
        cleared.bundle().base().generation
    );
    drop(cleared);
    writable.close().expect("close completed reclaim store");
    let reopened =
        Store::open_native_graph(&path, options(), None).expect("reopen completed reclaim store");
    reopened.close().expect("close reopened reclaim store");

    for cell in [
        CrashCell::Control,
        CrashCell::BeforeIntent,
        CrashCell::BeforeFirstUnlink,
        CrashCell::AfterOneUnlink,
        CrashCell::DirectorySync,
        CrashCell::BeforeCompletion,
        CrashCell::LostCompletionAck,
    ] {
        run_reclaim_crash_cell(cell);
    }
}

#[test]
fn ze46_readonly_pending_reclaim_and_checkpoint_retirement_are_exact() {
    run_ze46_readonly_pending_reclaim_and_checkpoint_retirement_are_exact();
}

fn run_ze46_readonly_pending_reclaim_and_checkpoint_retirement_are_exact() {
    // The partly unlinked read-only cell lives in the crash table, which
    // asserts zero VFS events and identical bytes for that partition.
    run_reclaim_crash_cell(CrashCell::AfterOneUnlink);

    let (history, store) = seed_crash_history();
    let oracle = history.oracle(&store);
    let held = store.admit_native_read().expect("held pre-intent reader");
    let held_snapshot = |store: &Store| {
        snapshot_for_lease(
            store,
            &held,
            history.first,
            history.peer,
            history.relationship,
            None,
        )
    };
    let held_before = held_snapshot(&store);
    let proof_path = |required: crate::property_graph::wal::RequiredRef| {
        crate::property_graph::storage::allocation::artifact_path(
            &history.path,
            required.object.artifact,
        )
    };

    commit_maintenance(&store).expect("durable reclaim intent");
    let pending = store.admit_native_read().expect("pending reader");
    let intent = pending.bundle().reclaim().expect("pending intent root");
    drop(pending);
    let resumed = commit_maintenance(&store).expect("in-process resume");
    assert!(resumed.removed_bytes > 0);
    assert_eq!(
        held_snapshot(&store),
        held_before,
        "unlink reached a held reader"
    );
    let completed = store.admit_native_read().expect("completed reader");
    let completion = completed.bundle().reclaim().expect("completion root");
    assert_ne!(completion, intent);
    assert_ne!(
        completed.bundle().root_envelope().object.generation,
        completed.bundle().base().generation,
        "completion is WAL-only before its checkpoint"
    );
    drop(completed);

    // Retirement step one checkpoints the completion and replaces its own
    // admission. The proof stays rooted and on disk.
    let error = commit_maintenance(&store).expect_err("completion checkpoint");
    assert!(
        matches!(error, super::super::NativeGraphError::StalePreparation),
        "{error:?}"
    );
    let checkpointed = store
        .admit_native_read()
        .expect("checkpointed completion reader");
    assert_eq!(checkpointed.bundle().reclaim(), Some(completion));
    assert_eq!(
        checkpointed.bundle().root_envelope().object.generation,
        checkpointed.bundle().base().generation
    );
    drop(checkpointed);
    for required in [intent, completion] {
        assert!(
            proof_path(required).exists(),
            "proof vanished before retirement"
        );
    }
    assert_eq!(held_snapshot(&store), held_before);

    // Step two clears the checkpointed completion. Only now is the proof
    // unrooted; the held reader still reads its original bytes.
    let cleared_report = commit_maintenance(&store).expect("clear completed reclaim");
    assert_eq!(cleared_report.removed_bytes, 0);
    let cleared = store.admit_native_read().expect("cleared reader");
    assert!(cleared.bundle().reclaim().is_none());
    assert_eq!(
        cleared.bundle().root_envelope().object.generation,
        cleared.bundle().base().generation
    );
    drop(cleared);
    assert_eq!(held_snapshot(&store), held_before);
    assert_same_logical_state(&oracle, &history.oracle(&store));

    // The retired intent page is unrooted garbage with no inventory row. A
    // quiescent maintenance adopts it as bookkeeping; the next cycle unlinks
    // it through a durable intent like any other object.
    let retired_intent = proof_path(intent);
    assert!(retired_intent.exists());
    for _ in 0..24 {
        if !retired_intent.exists() {
            break;
        }
        store
            .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
            .expect("checkpoint between retirement rounds");
        match commit_maintenance(&store) {
            Ok(_) | Err(super::super::NativeGraphError::StalePreparation) => {}
            Err(error) => panic!("post-retirement maintenance failed: {error:?}"),
        }
        assert_eq!(held_snapshot(&store), held_before);
    }
    assert!(
        !retired_intent.exists(),
        "the retired reclaim intent page was never reclaimed"
    );
    assert_same_logical_state(&oracle, &history.oracle(&store));
    drop(held);
    store.close().expect("close retirement store");

    // Maintenance obeys the writer's 64-envelope checkpoint policy exactly:
    // the 64th envelope commits, the next admission checkpoints instead.
    let parent = super::tempfile::tempdir().expect("boundary parent");
    let path = parent.path().join("native");
    let vfs = Arc::new(RecordingVfs::default());
    let store = create_reclaim_test_store(&path, &vfs);
    let envelopes = |store: &Store| {
        store
            .native_graph
            .writer
            .lock()
            .expect("native writer")
            .as_ref()
            .expect("installed writer")
            .complete_envelopes
    };
    for index in 0..62 {
        let image = CanonicalContents::node(&mut [], &mut [], None, None).expect("node image");
        store
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "boundary", &index.to_string())
                        .expect("boundary key"),
                    revision: GraphRevision::new(1).expect("revision"),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("boundary write");
    }
    assert_eq!(envelopes(&store), 62);
    for expected in [63, 64] {
        commit_maintenance(&store).expect("maintenance below the envelope limit");
        assert_eq!(envelopes(&store), expected);
    }
    let before = store.admit_native_read().expect("pre-boundary reader");
    let sequence = before.bundle().sequence();
    let generation = before.bundle().base().generation;
    drop(before);
    let error = commit_maintenance(&store).expect_err("maintenance at the envelope limit");
    assert!(
        matches!(error, super::super::NativeGraphError::StalePreparation),
        "{error:?}"
    );
    assert_eq!(envelopes(&store), 0);
    let capture = store
        .capture_native_read_roots()
        .expect("post-boundary capture");
    let wal = capture.wal().expect("post-boundary WAL");
    assert_eq!(wal.bytes(), crate::property_graph::wal::HEADER_BYTES);
    assert_eq!(wal.first_sequence(), sequence + 1);
    drop(capture);
    let after = store.admit_native_read().expect("post-boundary reader");
    assert_eq!(after.bundle().sequence(), sequence);
    assert_eq!(after.bundle().base().generation, generation);
    drop(after);
    commit_maintenance(&store).expect("fresh admission after the boundary checkpoint");
    assert_eq!(envelopes(&store), 1);
    store.close().expect("close boundary store");
}

#[test]
fn ze46_corrupt_or_incomplete_reclaim_proof_refuses_before_mutation() {
    run_ze46_corrupt_or_incomplete_reclaim_proof_refuses_before_mutation();
}

fn run_ze46_corrupt_or_incomplete_reclaim_proof_refuses_before_mutation() {
    let parent = super::tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("omitted-live-root");
    let vfs = Arc::new(RecordingVfs::default());
    let store = create_reclaim_test_store(&path, &vfs);
    seed_reclaimable_manifest(&store, "omitted-live-root");

    let admission = store
        .admit_native_graph_maintenance()
        .expect("omitted-root maintenance admission");
    let live = admission.lease.bundle().catalog().block;
    assert!(
        complete_inventory_union_for_lease(&store, &admission.lease)
            .iter()
            .any(|change| change.object.artifact == live.artifact),
        "selected live catalog is an authentic inventory descriptor"
    );
    let before = directory_image(&path);
    vfs.take();
    omit_mark_artifact_for_test(live.artifact);
    let result = store
        .commit_native_graph_maintenance(&admission, &QueryControl::Cancel(CancelToken::new()));
    let omitted = take_omitted_mark_emissions_for_test();
    assert!(omitted > 0, "directed omission reached the live mark edge");
    assert!(
        matches!(
            &result,
            Err(super::super::NativeGraphError::Read(
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "protected root is absent from completed mark"
                )
            ))
        ),
        "a protected live root omitted from the completed mark must refuse: {result:?}"
    );
    assert_live_files_unchanged(&path, &before);
    assert!(
        vfs.take()
            .iter()
            .all(|event| !matches!(event, DurabilityEvent::Delete(_))),
        "invalid proof must never unlink a candidate"
    );
    drop(admission);
    store.close().expect("close refused reclaim proof store");

    let missing_path = parent.path().join("missing-mark-stream");
    let missing_vfs = Arc::new(RecordingVfs::default());
    let missing_store = create_reclaim_test_store(&missing_path, &missing_vfs);
    seed_reclaimable_manifest(&missing_store, "missing-mark-stream");
    let pending_admission = missing_store
        .admit_native_graph_maintenance()
        .expect("missing-stream intent admission");
    missing_store
        .commit_native_graph_maintenance(
            &pending_admission,
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("real pending intent for missing-stream control");
    drop(pending_admission);
    let pending = missing_store
        .admit_native_read()
        .expect("missing-stream pending reader");
    let (manifest, candidates) = pending_reclaim_proof_for_lease(&missing_store, &pending);
    assert!(
        !candidates.is_empty(),
        "real pending intent has a candidate"
    );
    drop(pending);
    missing_store
        .close()
        .expect("close missing-stream pending store");

    let mark_path = crate::property_graph::storage::allocation::artifact_path(
        &missing_path,
        manifest.mark.root.object.artifact,
    );
    let mark_bytes = std::fs::read(&mark_path).expect("completed mark root bytes");
    std::fs::remove_file(&mark_path).expect("remove completed mark root");
    let missing_before = directory_image(&missing_path);
    missing_vfs.take();
    let infrastructure: Arc<dyn Vfs> = missing_vfs.clone();
    let reopened = Store::open_native_graph_with_infrastructure(
        &missing_path,
        options(),
        None,
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
    );
    if let Ok(store) = reopened {
        store.close().expect("close unexpectedly admitted store");
        panic!("missing completed mark stream was admitted");
    }
    assert_eq!(directory_image(&missing_path), missing_before);
    assert!(
        missing_vfs.take().is_empty(),
        "missing proof stream must refuse before any VFS mutation"
    );
    std::fs::write(mark_path, mark_bytes).expect("restore completed mark root");
}

#[test]
fn ze46_protected_union_keeps_partial_packs_and_actual_sparse_refs() {
    run_ze46_protected_union_keeps_partial_packs_and_actual_sparse_refs();
}

fn run_ze46_protected_union_keeps_partial_packs_and_actual_sparse_refs() {
    let parent = super::tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let vfs = Arc::new(RecordingVfs::default());
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let document = EmbeddingTower {
        model_id: "ze46-protected-union".into(),
        model_version: "1".into(),
        weights_digest: vec![0x46, 0x05],
        dims: 2,
        normalization: Normalization::None,
        prompt_prefix: "doc: ".into(),
        max_tokens: 32,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    let store = Store::create_native_graph_with_infrastructure(
        &path,
        options(),
        Some(document.clone()),
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("fresh protected-union store");
    let coordinates = [0.25_f32, 0.75_f32];
    let peer_coordinates = [0.5_f32, 1.0_f32];
    let receipts = crate::property_graph::with_local_refs(|refs| {
        let embedding = CanonicalEmbedding::new(&document, &coordinates).expect("embedding");
        let first = CanonicalContents::node(
            &mut [],
            &mut [],
            Some("protected union first"),
            Some(embedding),
        )
        .expect("first node");
        let peer_embedding =
            CanonicalEmbedding::new(&document, &peer_coordinates).expect("peer embedding");
        let second = CanonicalContents::node(
            &mut [],
            &mut [],
            Some("protected union peer"),
            Some(peer_embedding),
        )
        .expect("second node");
        store
            .apply_native_graph(
                &[
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "protected-union", "first")
                            .expect("first key"),
                        revision: GraphRevision::new(1).expect("revision"),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&first)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "protected-union", "peer")
                            .expect("peer key"),
                        revision: GraphRevision::new(1).expect("revision"),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&second)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(
                            EntityKind::Relationship,
                            "protected-union",
                            "edge",
                        )
                        .expect("relationship key"),
                        revision: GraphRevision::new(1).expect("revision"),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Relationship {
                            source: NodeRef::Local(refs.node(0).expect("first local node")),
                            target: NodeRef::Local(refs.node(1).expect("peer local node")),
                            relationship_type: GraphName::new("LINKS").expect("type"),
                            properties: &[],
                        }),
                    },
                ],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .expect("seed protected-union history")
    });
    let first = match receipts[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("first receipt identity"),
    };
    let peer = match receipts[1].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("peer receipt identity"),
    };
    let relationship = match receipts[2].entity {
        EntityId::Relationship(relationship) => relationship,
        EntityId::Node(_) => panic!("relationship receipt identity"),
    };

    // This fixture keeps the original pack partly live through `peer`, so
    // every maintenance here relocates `first`. Production selection rotates.
    let _pin = crate::property_graph::storage::consolidation::pin_selection_for_test(first);
    // It also asserts exact candidate sets, so retired proof pages must not
    // join them; adoption has its own cases.
    let _suspension = super::super::maintenance::orphans::suspend_adoption_for_test();

    let observed = store
        .admit_native_read()
        .expect("checkpoint witness reader");
    let checkpoint_manifest = observed
        .bundle()
        .prepared_inventories()
        .first()
        .copied()
        .expect("authentic checkpoint prepared manifest");
    let manifest_path = crate::property_graph::storage::allocation::artifact_path(
        &path,
        checkpoint_manifest.object.artifact,
    );
    let manifest_bytes = std::fs::read(&manifest_path).expect("checkpoint manifest bytes");
    let text = sparse_physical_for_lease(&store, &observed, first, Modality::Text);
    let peer_text = sparse_physical_for_lease(&store, &observed, peer, Modality::Text);
    let vector = sparse_physical_for_lease(&store, &observed, first, Modality::Vector);
    let peer_vector = sparse_physical_for_lease(&store, &observed, peer, Modality::Vector);
    assert_eq!(text.source, peer_text.source);
    assert_eq!(vector.source, peer_vector.source);
    assert_eq!(text.record, vector.record);
    assert_eq!(peer_text.record, peer_vector.record);
    let superseded_block = text.record.reference();
    let surviving_block = peer_text.record.reference();
    assert_eq!(superseded_block.artifact, surviving_block.artifact);
    assert_ne!(superseded_block, surviving_block);
    let partial_pack_artifact = superseded_block.artifact;
    let mut matching_pack = complete_inventory_union_for_lease(&store, &observed)
        .into_iter()
        .filter(|change| change.object.artifact == partial_pack_artifact);
    let partial_pack = matching_pack
        .next()
        .expect("partly live pack descriptor")
        .object;
    assert!(
        matching_pack.next().is_none(),
        "partly live pack descriptor is not unique"
    );
    assert_ne!(partial_pack.artifact, checkpoint_manifest.object.artifact);
    let partial_pack_path =
        crate::property_graph::storage::allocation::artifact_path(&path, partial_pack_artifact);
    let partial_pack_bytes = std::fs::read(&partial_pack_path).expect("partly live pack bytes");
    assert_eq!(
        u64::try_from(partial_pack_bytes.len()).expect("partly live pack byte count"),
        u64::from(partial_pack.bytes)
    );
    let partial_pack_frame = crate::property_graph::storage::artifact::decode(
        crate::property_graph::storage::artifact::ContainerKind::Object,
        Some((partial_pack.store, partial_pack.artifact)),
        &partial_pack_bytes,
    )
    .expect("authenticate partly live pack");
    let superseded = partial_pack_frame
        .framed_block(superseded_block)
        .expect("superseded record in partly live pack");
    let surviving = partial_pack_frame
        .framed_block(surviving_block)
        .expect("surviving record in partly live pack");
    for block in [superseded, surviving] {
        let identity = block.identity();
        assert_eq!(identity.store, partial_pack.store);
        assert_eq!(identity.artifact, partial_pack.artifact);
        assert_eq!(identity.generation, partial_pack.generation);
        assert_eq!(identity.creation_serial, partial_pack.serial);
        assert_eq!(block.file_length(), partial_pack_bytes.len());
        assert_eq!(block.file_checksum(), partial_pack.checksum);
    }
    let before_first = snapshot_for_lease(&store, &observed, first, peer, relationship, None);
    let before_peer = snapshot_for_lease(&store, &observed, peer, first, relationship, None);
    assert_eq!(
        vector.vector_index.expect("real V2 vector index").role(),
        crate::property_graph::storage::artifact::BlockKind::RetrievalVectorIndex
    );
    assert!(vector.vector_index_catalog.is_some());
    drop(observed);

    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .expect("checkpoint current prepared manifest");
    let checkpointed = store
        .admit_native_read()
        .expect("selected checkpoint reader");
    assert!(
        checkpointed
            .bundle()
            .prepared_inventories()
            .contains(&checkpoint_manifest)
    );
    let checkpoint = checkpointed.bundle().root_envelope();
    let checkpoint_sequence = checkpointed.bundle().sequence();
    drop(checkpointed);

    let fold = store
        .admit_native_graph_maintenance()
        .expect("checkpoint-manifest fold admission");
    store
        .commit_native_graph_maintenance(&fold, &QueryControl::Cancel(CancelToken::new()))
        .expect("fold checkpoint manifest");
    drop(fold);
    let retired = store.admit_native_read().expect("retired manifest reader");
    assert!(
        !retired
            .bundle()
            .prepared_inventories()
            .contains(&checkpoint_manifest)
    );
    assert_eq!(retired.bundle().root_envelope(), checkpoint);
    assert!(
        retired.bundle().reclaim().is_none(),
        "unexpected reclaim after fold"
    );
    assert!(
        rooted_inventory_for_lease(&store, &retired)
            .iter()
            .any(|change| change.object == checkpoint_manifest.object
                && change.state == InventoryState::Retained),
        "checkpoint manifest is absent from rooted inventory"
    );
    assert_eq!(
        std::fs::read(&manifest_path).expect("retired manifest bytes"),
        manifest_bytes
    );
    let fold_text = sparse_physical_for_lease(&store, &retired, first, Modality::Text);
    let fold_peer_text = sparse_physical_for_lease(&store, &retired, peer, Modality::Text);
    let fold_vector = sparse_physical_for_lease(&store, &retired, first, Modality::Vector);
    let fold_peer_vector = sparse_physical_for_lease(&store, &retired, peer, Modality::Vector);
    assert_eq!(fold_text.record, fold_vector.record);
    assert_eq!(fold_peer_text.record, fold_peer_vector.record);
    for (old, new, old_peer, new_peer) in [
        (text, fold_text, peer_text, fold_peer_text),
        (vector, fold_vector, peer_vector, fold_peer_vector),
    ] {
        assert_ne!(new.record, old.record);
        assert_ne!(new.record.reference(), superseded_block);
        assert_ne!(new.source, old.source);
        assert_ne!(new.row_table, old.row_table);
        assert_eq!(new.source, new_peer.source);
        assert_eq!(new_peer.record, old_peer.record);
        assert_eq!(new_peer.record.reference(), surviving_block);
        assert_eq!(new.ordinal, old.ordinal);
        assert_eq!(new_peer.ordinal, old_peer.ordinal);
        assert_eq!(new.mask, old.mask);
        assert_eq!(new.lexical, old.lexical);
        assert_eq!(new_peer.mask, old_peer.mask);
        assert_eq!(new_peer.lexical, old_peer.lexical);
    }
    assert_eq!(fold_vector.vector_index, vector.vector_index);
    assert_eq!(
        fold_vector.vector_index_catalog,
        vector.vector_index_catalog
    );
    assert_eq!(fold_peer_vector.vector_index, peer_vector.vector_index);
    assert_eq!(
        fold_peer_vector.vector_index_catalog,
        peer_vector.vector_index_catalog
    );
    assert_eq!(
        fold_vector
            .vector_index
            .expect("first-fold V2 vector index")
            .role(),
        crate::property_graph::storage::artifact::BlockKind::RetrievalVectorIndex
    );
    assert!(fold_vector.vector_index_catalog.is_some());
    assert_ne!(
        fold_text.record.reference().artifact,
        partial_pack_artifact,
        "selected record remained in the partly live pack"
    );
    assert_eq!(
        fold_peer_text.record.reference().artifact,
        partial_pack_artifact,
        "peer record left the partly live pack"
    );
    assert_eq!(
        std::fs::read(&partial_pack_path).expect("partly live pack after first fold"),
        partial_pack_bytes
    );
    assert!(
        rooted_inventory_for_lease(&store, &retired)
            .iter()
            .any(|change| change.object == partial_pack
                && change.state == InventoryState::Retained),
        "partly live pack is absent from rooted retained inventory"
    );
    let fold_allocations = maintenance_allocations(&store, &retired, first);
    let after_first = snapshot_for_lease(&store, &retired, first, peer, relationship, None);
    let after_peer = snapshot_for_lease(&store, &retired, peer, first, relationship, None);
    assert_eq!(after_first.generation, after_peer.generation);
    for (before, after) in [(&before_first, &after_first), (&before_peer, &after_peer)] {
        assert!(after.generation > before.generation);
        assert_eq!(after.revision, before.revision);
        assert_eq!(after.original_generation, before.original_generation);
        assert_eq!(after.canonical, before.canonical);
        assert_eq!(after.text, before.text);
        assert_eq!(after.vector, before.vector);
        assert_eq!(after.old_relationship, before.old_relationship);
        assert_eq!(after.new_relationship, before.new_relationship);
        assert_eq!(after.out, before.out);
        assert_eq!(after.incoming, before.incoming);
        assert!(after.sparse_text);
        assert!(after.sparse_vector);
    }
    drop(retired);

    let mut visited_checkpoint = false;
    let mut found_manifest = false;
    super::super::recovery::visit_captured_state(
        &store,
        &path,
        checkpoint,
        checkpoint_sequence,
        None,
        &QueryControl::Cancel(CancelToken::new()),
        |visit, resources| {
            match visit {
                super::super::recovery::CapturedStateVisit::Checkpoint { state, .. } => {
                    visited_checkpoint = true;
                    for index in 0..state.prepared_inventories.len()? {
                        found_manifest |= state.prepared_inventories.get(index, resources)?
                            == checkpoint_manifest;
                    }
                }
                super::super::recovery::CapturedStateVisit::Envelope { .. } => {
                    panic!("checkpoint-own sequence unexpectedly visited a WAL envelope");
                }
            }
            Ok(())
        },
    )
    .expect("authenticate saved checkpoint state");
    assert!(visited_checkpoint && found_manifest);

    {
        let capture = store
            .capture_native_read_roots()
            .expect("exclusive historical witness capture");
        assert!(!capture.contains_prepared(checkpoint_manifest.object));
        assert!(
            capture
                .bundles()
                .iter()
                .all(|bundle| { !bundle.prepared_inventories().contains(&checkpoint_manifest) })
        );
    }

    let before_second = directory_image(&path);
    vfs.take();
    let second = store
        .admit_native_graph_maintenance()
        .expect("historical protection admission");
    let result =
        store.commit_native_graph_maintenance(&second, &QueryControl::Cancel(CancelToken::new()));
    let historical_allocations = match result {
        Err(error) => {
            assert_live_files_unchanged(&path, &before_second);
            assert_eq!(
                std::fs::read(&manifest_path).expect("historical manifest retained"),
                manifest_bytes
            );
            let events = vfs.take();
            assert!(
                events
                    .iter()
                    .all(|event| !matches!(event, DurabilityEvent::Delete(_))),
                "historical protection refusal deleted a file: {events:?}"
            );
            drop(second);
            panic!(
                "historical checkpoint requirement prevented maintenance before mutation: {error:?}; checkpoint={:?} sequence={} manifest={:?}",
                checkpoint.object.artifact,
                checkpoint_sequence,
                checkpoint_manifest.object.artifact
            );
        }
        Ok(report) => {
            assert_eq!(report.removed_bytes, 0);
            drop(second);
            let current = store.admit_native_read().expect("historical proof reader");
            let candidates = if current.bundle().reclaim().is_some() {
                pending_reclaim_candidates(&store, &current)
            } else {
                Vec::new()
            };
            assert!(
                !candidates.contains(&checkpoint_manifest.object),
                "historical checkpoint manifest entered reclaim candidates: {candidates:?}"
            );
            assert!(
                candidates
                    .iter()
                    .all(|candidate| candidate.artifact != partial_pack_artifact),
                "partly live pack entered reclaim candidates: pack={partial_pack:?} candidates={candidates:?}"
            );
            let (_, mark) = durable_proof_for_lease(&store, &current);
            assert!(
                mark.contains(&checkpoint_manifest.object.artifact),
                "historical checkpoint manifest is absent from completed mark"
            );
            assert!(
                mark.contains(&partial_pack_artifact),
                "partly live pack is absent from completed mark: pack={partial_pack:?} surviving={surviving_block:?} first_fold_peer={:?}",
                fold_peer_text.record.reference()
            );
            assert_eq!(
                std::fs::read(&manifest_path).expect("marked historical manifest bytes"),
                manifest_bytes
            );
            let marked_partial_pack =
                std::fs::read(&partial_pack_path).expect("marked partly live pack bytes");
            assert_eq!(marked_partial_pack, partial_pack_bytes);
            assert_eq!(
                u64::try_from(marked_partial_pack.len()).expect("marked pack byte count"),
                u64::from(partial_pack.bytes)
            );
            let current_peer_text =
                sparse_physical_for_lease(&store, &current, peer, Modality::Text);
            assert_eq!(current_peer_text.record, fold_peer_text.record);
            let allocations = maintenance_allocations(&store, &current, first);
            assert!(
                vfs.take()
                    .iter()
                    .all(|event| !matches!(event, DurabilityEvent::Delete(_)))
            );
            drop(current);
            allocations
        }
    };

    let old_lazy = store
        .admit_native_read()
        .expect("unmapped old reader admission");
    assert!(
        old_lazy.bundle().reclaim().is_none(),
        "unexpected reclaim before reader-only checkpoint"
    );
    assert_eq!(old_lazy.bundle().root_envelope(), checkpoint);
    let old_lazy_sequence = old_lazy.bundle().sequence();
    let old_lazy_generation = old_lazy.bundle().base().generation;
    assert!(
        !old_lazy
            .bundle()
            .prepared_inventories()
            .contains(&checkpoint_manifest)
    );
    let observation = store
        .admit_native_read()
        .expect("reader-only baseline observation");
    let reader_before_first =
        snapshot_for_lease(&store, &observation, first, peer, relationship, None);
    let reader_before_peer =
        snapshot_for_lease(&store, &observation, peer, first, relationship, None);
    drop(observation);

    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .expect("replace current checkpoint while retaining old reader");
    let checkpointed_current = store
        .admit_native_read()
        .expect("reader-only new-current observation");
    let current_checkpoint = checkpointed_current.bundle().root_envelope();
    assert_ne!(current_checkpoint, checkpoint);
    assert_eq!(checkpointed_current.bundle().sequence(), old_lazy_sequence);
    assert_eq!(
        checkpointed_current.bundle().base().generation,
        old_lazy_generation
    );
    assert!(checkpointed_current.bundle().reclaim().is_none());
    assert!(
        !checkpointed_current
            .bundle()
            .prepared_inventories()
            .contains(&checkpoint_manifest)
    );
    assert!(
        rooted_inventory_for_lease(&store, &checkpointed_current)
            .iter()
            .any(|change| change.object == checkpoint_manifest.object
                && change.state == InventoryState::Retained),
        "reader-only manifest is absent from rooted inventory"
    );
    assert_eq!(
        std::fs::read(&manifest_path).expect("reader-only manifest after checkpoint"),
        manifest_bytes
    );
    drop(checkpointed_current);

    {
        let capture = store
            .capture_native_read_roots()
            .expect("reader-only ownership capture");
        assert!(capture.contains_lease(&old_lazy));
        assert!(!capture.contains_prepared(checkpoint_manifest.object));
        assert_eq!(capture.bundles().len(), 2);
        assert!(capture.bundles().iter().all(|bundle| {
            bundle.root_envelope() == checkpoint || bundle.root_envelope() == current_checkpoint
        }));
        let wal = capture.wal().expect("new current WAL capture");
        assert_eq!(wal.first_sequence(), old_lazy_sequence + 1);
        assert_eq!(wal.bytes(), crate::property_graph::wal::HEADER_BYTES);

        let mut visited_current_checkpoint = false;
        super::super::recovery::visit_captured_state(
            &store,
            &path,
            current_checkpoint,
            old_lazy_sequence,
            None,
            &QueryControl::Cancel(CancelToken::new()),
            |visit, resources| {
                match visit {
                    super::super::recovery::CapturedStateVisit::Checkpoint { state, .. } => {
                        visited_current_checkpoint = true;
                        for index in 0..state.prepared_inventories.len()? {
                            assert_ne!(
                                state.prepared_inventories.get(index, resources)?,
                                checkpoint_manifest
                            );
                        }
                        assert!(
                            state
                                .graph
                                .slots
                                .into_iter()
                                .flatten()
                                .chain(
                                    [state.text, state.vector, state.reclaim]
                                        .into_iter()
                                        .flatten(),
                                )
                                .chain(std::iter::once(state.catalog))
                                .all(|required| {
                                    required.object.artifact != checkpoint_manifest.object.artifact
                                        && required.block.artifact
                                            != checkpoint_manifest.object.artifact
                                }),
                            "new current checkpoint directly names reader-only manifest"
                        );
                    }
                    super::super::recovery::CapturedStateVisit::Envelope { .. } => {
                        panic!("new current checkpoint unexpectedly visited a WAL envelope");
                    }
                }
                Ok(())
            },
        )
        .expect("authenticate reader-only current checkpoint");
        assert!(visited_current_checkpoint);
        assert_eq!(old_lazy.bundle().root_envelope(), checkpoint);
        assert_eq!(old_lazy.bundle().sequence(), old_lazy_sequence);
    }

    vfs.take();
    let reader_protection = store
        .admit_native_graph_maintenance()
        .expect("reader-only protection admission");
    let reader_report = store
        .commit_native_graph_maintenance(
            &reader_protection,
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("reader-only protection maintenance");
    assert_eq!(reader_report.removed_bytes, 0);
    drop(reader_protection);
    let reader_proof = store
        .admit_native_read()
        .expect("reader-only proof observation");
    let reader_allocations = maintenance_allocations(&store, &reader_proof, first);
    let candidates = if reader_proof.bundle().reclaim().is_some() {
        pending_reclaim_candidates(&store, &reader_proof)
    } else {
        Vec::new()
    };
    assert!(
        !candidates.contains(&checkpoint_manifest.object),
        "reader-only manifest entered reclaim candidates: {candidates:?}"
    );
    let (records, mark) = durable_proof_for_lease(&store, &reader_proof);
    assert!(
        records.iter().any(|record| {
            *record
                == ProtectedRecord::captured_state(
                    ProtectedClass::Reader,
                    checkpoint,
                    old_lazy_sequence,
                )
        }),
        "old reader's exact captured-state locator is absent"
    );
    assert!(
        mark.contains(&checkpoint_manifest.object.artifact),
        "reader-only manifest is absent from completed mark"
    );
    assert!(
        records.iter().all(|record| {
            !(record.class == ProtectedClass::PreparedAllocation
                && matches!(
                    record.value,
                    ProtectedValue::Descriptor(descriptor)
                        if descriptor == checkpoint_manifest.object
                ))
        }),
        "reader-only manifest is masked by a prepared allocation"
    );
    assert!(
        records.iter().all(|record| {
            !matches!(
                (record.class, record.value),
                (
                    ProtectedClass::Current | ProtectedClass::PreparedBase,
                    ProtectedValue::Required(required),
                ) if required.object.artifact == checkpoint_manifest.object.artifact
            )
        }),
        "current or prepared-base direct root names reader-only manifest"
    );
    assert_eq!(
        std::fs::read(&manifest_path).expect("reader-only marked manifest bytes"),
        manifest_bytes
    );
    assert_eq!(
        std::fs::read(&partial_pack_path).expect("reader-only partly live pack bytes"),
        partial_pack_bytes
    );
    let reader_events = vfs.take();
    assert!(
        reader_events
            .iter()
            .all(|event| !matches!(event, DurabilityEvent::Delete(_))),
        "reader-only protection deleted a file: {reader_events:?}"
    );
    drop(reader_proof);

    assert_eq!(
        snapshot_for_lease(&store, &old_lazy, first, peer, relationship, None),
        reader_before_first
    );
    assert_eq!(
        snapshot_for_lease(&store, &old_lazy, peer, first, relationship, None),
        reader_before_peer
    );

    assert_eq!(
        historical_allocations[0].generation.get(),
        fold_allocations[0].generation.get() + 1
    );
    assert_eq!(
        reader_allocations[0].generation.get(),
        historical_allocations[0].generation.get() + 1
    );
    let replacement_descriptors: Vec<_> = fold_allocations
        .iter()
        .chain(&historical_allocations)
        .chain(&reader_allocations)
        .copied()
        .collect();
    let mut replacement_artifacts: Vec<_> = replacement_descriptors
        .iter()
        .map(|descriptor| descriptor.artifact)
        .collect();
    replacement_artifacts.sort_unstable();
    replacement_artifacts.dedup();
    assert_eq!(replacement_artifacts.len(), replacement_descriptors.len());
    for descriptor in replacement_descriptors.iter().copied() {
        assert_eq!(descriptor.store, checkpoint_manifest.object.store);
        assert_ne!(descriptor, checkpoint_manifest.object);
        assert_ne!(descriptor.artifact, partial_pack_artifact);
    }
    eprintln!(
        "ZE46_PRIMARY_M object={:?} block={:?} fold={:?} historical={:?} reader={:?}",
        checkpoint_manifest.object,
        checkpoint_manifest.block,
        fold_allocations,
        historical_allocations,
        reader_allocations,
    );
    let replacement_paths: Vec<_> = replacement_descriptors
        .iter()
        .map(|descriptor| {
            crate::property_graph::storage::allocation::artifact_path(&path, descriptor.artifact)
        })
        .collect();
    let replacement_bytes: Vec<_> = replacement_paths
        .iter()
        .map(|path| std::fs::read(path).expect("replacement allocation bytes"))
        .collect();

    let registration_base = store
        .admit_native_read()
        .expect("prepared-only registration base");
    assert!(
        registration_base.bundle().reclaim().is_none(),
        "unexpected pending reclaim before prepared-only registration"
    );
    assert!(
        rooted_inventory_for_lease(&store, &registration_base)
            .iter()
            .any(|change| change.object == checkpoint_manifest.object
                && change.state == InventoryState::Retained),
        "prepared-only manifest is absent from rooted inventory"
    );
    assert!(
        !registration_base
            .bundle()
            .prepared_inventories()
            .contains(&checkpoint_manifest)
    );
    assert_eq!(
        std::fs::read(&manifest_path).expect("prepared-only manifest before registration"),
        manifest_bytes
    );
    let prepared_before_first =
        snapshot_for_lease(&store, &registration_base, first, peer, relationship, None);
    let prepared_before_peer =
        snapshot_for_lease(&store, &registration_base, peer, first, relationship, None);
    let registered = [InventoryChange {
        object: checkpoint_manifest.object,
        state: InventoryState::Prepared,
    }];
    let prepared = registration_base
        .register_prepared(&registered)
        .expect("register exact prepared-only manifest");
    let replacement_allocations: Vec<_> = replacement_descriptors
        .iter()
        .copied()
        .map(|object| InventoryChange {
            object,
            state: InventoryState::Prepared,
        })
        .collect();
    let replacement_registration = registration_base
        .register_prepared(&replacement_allocations)
        .expect("register exact maintenance allocations");
    drop(registration_base);
    drop(old_lazy);

    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .expect("checkpoint prepared-only current state");
    let prepared_current = store
        .admit_native_read()
        .expect("prepared-only current observation");
    let prepared_checkpoint = prepared_current.bundle().root_envelope();
    let prepared_sequence = prepared_current.bundle().sequence();
    let prepared_generation = prepared_current.bundle().base().generation;
    assert_ne!(prepared_checkpoint, current_checkpoint);
    assert!(prepared_current.bundle().reclaim().is_none());
    assert!(
        !prepared_current
            .bundle()
            .prepared_inventories()
            .contains(&checkpoint_manifest)
    );
    assert!(
        rooted_inventory_for_lease(&store, &prepared_current)
            .iter()
            .any(|change| change.object == checkpoint_manifest.object
                && change.state == InventoryState::Retained),
        "prepared-only checkpoint lost the manifest inventory descriptor"
    );
    assert_eq!(prepared_before_first.generation, prepared_generation.get());
    assert_eq!(prepared_before_peer.generation, prepared_generation.get());
    assert_eq!(
        std::fs::read(&manifest_path).expect("prepared-only checkpoint manifest bytes"),
        manifest_bytes
    );
    drop(prepared_current);

    {
        let capture = store
            .capture_native_read_roots()
            .expect("prepared-only ownership capture");
        assert_eq!(capture.bundles().len(), 1);
        assert!(capture.leases().is_empty());
        assert!(capture.contains_prepared(checkpoint_manifest.object));
        let mut captured_prepared = capture.prepared().to_vec();
        captured_prepared.sort_unstable_by_key(|descriptor| descriptor.artifact);
        let mut expected_prepared = replacement_descriptors.to_vec();
        expected_prepared.push(checkpoint_manifest.object);
        expected_prepared.sort_unstable_by_key(|descriptor| descriptor.artifact);
        assert_eq!(captured_prepared, expected_prepared);
        assert!(capture.spills().is_empty());
        let wal = capture.wal().expect("prepared-only current WAL");
        assert_eq!(wal.bytes(), crate::property_graph::wal::HEADER_BYTES);
        assert_eq!(wal.first_sequence(), prepared_sequence + 1);
        let current = capture
            .bundles()
            .first()
            .expect("prepared-only current bundle");
        assert_eq!(current.root_envelope(), prepared_checkpoint);
        assert_eq!(current.sequence(), prepared_sequence);

        let mut checkpoint_visits = 0_usize;
        super::super::recovery::visit_captured_state(
            &store,
            &path,
            prepared_checkpoint,
            prepared_sequence,
            None,
            &QueryControl::Cancel(CancelToken::new()),
            |visit, resources| {
                match visit {
                    super::super::recovery::CapturedStateVisit::Checkpoint { state, .. } => {
                        checkpoint_visits += 1;
                        for index in 0..state.prepared_inventories.len()? {
                            assert_ne!(
                                state.prepared_inventories.get(index, resources)?,
                                checkpoint_manifest
                            );
                        }
                        assert!(
                            state
                                .graph
                                .slots
                                .into_iter()
                                .flatten()
                                .chain(
                                    [state.text, state.vector, state.reclaim]
                                        .into_iter()
                                        .flatten(),
                                )
                                .chain(std::iter::once(state.catalog))
                                .all(|required| {
                                    required.object.artifact != checkpoint_manifest.object.artifact
                                        && required.block.artifact
                                            != checkpoint_manifest.object.artifact
                                }),
                            "prepared-only current checkpoint directly names the manifest"
                        );
                    }
                    super::super::recovery::CapturedStateVisit::Envelope { .. } => {
                        panic!("prepared-only checkpoint unexpectedly visited a WAL envelope");
                    }
                }
                Ok(())
            },
        )
        .expect("authenticate prepared-only current checkpoint");
        assert_eq!(checkpoint_visits, 1);
    }

    vfs.take();
    let prepared_admission = store
        .admit_native_graph_maintenance()
        .expect("prepared-only maintenance admission");
    let prepared_result = store.commit_native_graph_maintenance(
        &prepared_admission,
        &QueryControl::Cancel(CancelToken::new()),
    );
    let prepared_events = vfs.take();
    assert!(
        prepared_events
            .iter()
            .all(|event| !matches!(event, DurabilityEvent::Delete(_))),
        "prepared-only maintenance deleted a file: {prepared_events:?}"
    );
    let prepared_report = prepared_result.expect("prepared-only protection maintenance");
    assert_eq!(prepared_report.removed_bytes, 0);
    assert_eq!(
        prepared_report.generation.get(),
        prepared_generation.get() + 1
    );
    drop(prepared_admission);

    let prepared_proof = store
        .admit_native_read()
        .expect("prepared-only proof observation");
    let (prepared_records, prepared_mark) = durable_proof_for_lease(&store, &prepared_proof);
    assert!(prepared_records.contains(&ProtectedRecord::descriptor(
        ProtectedClass::PreparedAllocation,
        checkpoint_manifest.object,
    )));
    assert!(
        prepared_records
            .iter()
            .all(|record| record.class != ProtectedClass::Reader),
        "prepared-only proof retained a reader record: {prepared_records:?}"
    );
    assert!(prepared_records.contains(&ProtectedRecord::captured_state(
        ProtectedClass::Current,
        prepared_checkpoint,
        prepared_sequence,
    )));
    assert!(prepared_records.contains(&ProtectedRecord::captured_state(
        ProtectedClass::PreparedBase,
        prepared_checkpoint,
        prepared_sequence,
    )));
    assert!(
        prepared_mark.contains(&checkpoint_manifest.object.artifact),
        "prepared-only manifest is absent from completed mark"
    );
    for descriptor in replacement_descriptors.iter().copied() {
        assert!(prepared_records.contains(&ProtectedRecord::descriptor(
            ProtectedClass::PreparedAllocation,
            descriptor,
        )));
        assert!(
            prepared_mark.contains(&descriptor.artifact),
            "registered replacement allocation is absent from completed mark: {descriptor:?}"
        );
    }
    let prepared_candidates = if prepared_proof.bundle().reclaim().is_some() {
        pending_reclaim_candidates(&store, &prepared_proof)
    } else {
        Vec::new()
    };
    assert!(
        !prepared_candidates.contains(&checkpoint_manifest.object),
        "prepared-only manifest entered reclaim candidates: {prepared_candidates:?}"
    );
    assert!(
        prepared_candidates
            .iter()
            .all(|candidate| candidate.artifact != partial_pack_artifact),
        "partly live pack entered prepared-only candidates: {prepared_candidates:?}"
    );
    assert!(
        prepared_candidates.is_empty(),
        "prepared-only maintenance left unrelated pending candidates: {prepared_candidates:?}"
    );
    assert_eq!(
        std::fs::read(&manifest_path).expect("prepared-only marked manifest bytes"),
        manifest_bytes
    );
    assert_eq!(
        std::fs::read(&partial_pack_path).expect("prepared-only partly live pack bytes"),
        partial_pack_bytes
    );
    let prepared_after_first =
        snapshot_for_lease(&store, &prepared_proof, first, peer, relationship, None);
    let prepared_after_peer =
        snapshot_for_lease(&store, &prepared_proof, peer, first, relationship, None);
    assert_eq!(
        prepared_after_first.generation,
        prepared_report.generation.get()
    );
    assert_eq!(
        prepared_after_peer.generation,
        prepared_report.generation.get()
    );
    assert_semantic_snapshot_advanced(&prepared_before_first, &prepared_after_first);
    assert_semantic_snapshot_advanced(&prepared_before_peer, &prepared_after_peer);
    let prepared_after_sequence = prepared_proof.bundle().sequence();
    let prepared_after_generation = prepared_proof.bundle().base().generation;
    drop(prepared_proof);
    drop(prepared);

    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .expect("checkpoint released primary manifest");
    let released_current = store
        .admit_native_read()
        .expect("released primary observation");
    let released_checkpoint = released_current.bundle().root_envelope();
    let released_sequence = released_current.bundle().sequence();
    let released_generation = released_current.bundle().base().generation;
    assert_ne!(released_checkpoint, prepared_checkpoint);
    assert_eq!(released_sequence, prepared_after_sequence);
    assert_eq!(released_generation, prepared_after_generation);
    assert!(released_current.bundle().reclaim().is_none());
    assert!(!released_current.bundle().contains(checkpoint_manifest));
    assert!(
        !released_current
            .bundle()
            .prepared_inventories()
            .contains(&checkpoint_manifest)
    );
    assert!(
        rooted_inventory_for_lease(&store, &released_current)
            .iter()
            .any(|change| change.object == checkpoint_manifest.object
                && change.state == InventoryState::Retained),
        "released primary manifest is absent from rooted inventory"
    );
    assert_eq!(
        std::fs::read(&manifest_path).expect("released primary manifest bytes"),
        manifest_bytes
    );
    drop(released_current);

    {
        let released = store
            .capture_native_read_roots()
            .expect("capture released prepared-only registration");
        assert_eq!(released.bundles().len(), 1);
        assert!(released.leases().is_empty());
        assert!(released.spills().is_empty());
        assert!(!released.contains_prepared(checkpoint_manifest.object));
        let mut captured_prepared = released.prepared().to_vec();
        captured_prepared.sort_unstable_by_key(|descriptor| descriptor.artifact);
        let mut expected_prepared = replacement_descriptors.to_vec();
        expected_prepared.sort_unstable_by_key(|descriptor| descriptor.artifact);
        assert_eq!(captured_prepared, expected_prepared);
        let wal = released.wal().expect("released primary WAL");
        assert_eq!(wal.bytes(), crate::property_graph::wal::HEADER_BYTES);
        assert_eq!(wal.first_sequence(), released_sequence + 1);
        let current = released.bundles().first().expect("released current bundle");
        assert_eq!(current.root_envelope(), released_checkpoint);
        assert_eq!(current.sequence(), released_sequence);

        let mut checkpoint_visits = 0_usize;
        super::super::recovery::visit_captured_state(
            &store,
            &path,
            released_checkpoint,
            released_sequence,
            None,
            &QueryControl::Cancel(CancelToken::new()),
            |visit, resources| {
                match visit {
                    super::super::recovery::CapturedStateVisit::Checkpoint { state, .. } => {
                        checkpoint_visits += 1;
                        assert_eq!(state.sequence, released_sequence);
                        for index in 0..state.prepared_inventories.len()? {
                            assert_ne!(
                                state.prepared_inventories.get(index, resources)?,
                                checkpoint_manifest
                            );
                        }
                        assert!(
                            state
                                .graph
                                .slots
                                .into_iter()
                                .flatten()
                                .chain(
                                    [state.text, state.vector, state.reclaim]
                                        .into_iter()
                                        .flatten(),
                                )
                                .chain(std::iter::once(state.catalog))
                                .all(|required| {
                                    required.object.artifact != checkpoint_manifest.object.artifact
                                        && required.block.artifact
                                            != checkpoint_manifest.object.artifact
                                }),
                            "released checkpoint directly names primary manifest"
                        );
                    }
                    super::super::recovery::CapturedStateVisit::Envelope { .. } => {
                        panic!("released checkpoint unexpectedly visited a WAL envelope");
                    }
                }
                Ok(())
            },
        )
        .expect("authenticate released primary checkpoint");
        assert_eq!(checkpoint_visits, 1);
    }

    vfs.take();
    let release_admission = store
        .admit_native_graph_maintenance()
        .expect("released primary maintenance admission");
    let release_report = store
        .commit_native_graph_maintenance(
            &release_admission,
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("create exact primary reclaim intent");
    assert_eq!(release_report.removed_bytes, 0);
    let release_events = vfs.take();
    assert!(
        release_events
            .iter()
            .all(|event| !matches!(event, DurabilityEvent::Delete(_))),
        "primary intent commit deleted a file: {release_events:?}"
    );
    drop(release_admission);

    let pending = store
        .admit_native_read()
        .expect("exact primary pending reader");
    let pending_required = pending
        .bundle()
        .reclaim()
        .expect("real primary pending root");
    let (pending_manifest, candidates) = pending_reclaim_proof_for_lease(&store, &pending);
    assert_eq!(pending_manifest.candidate_count, 1);
    assert_eq!(candidates, vec![checkpoint_manifest.object]);
    let (_, pending_mark) = durable_proof_for_lease(&store, &pending);
    assert!(
        !pending_mark.contains(&checkpoint_manifest.object.artifact),
        "released primary manifest remained in completed mark"
    );
    for descriptor in replacement_descriptors.iter().copied() {
        assert!(
            pending_mark.contains(&descriptor.artifact),
            "registered replacement allocation left completed mark: {descriptor:?}"
        );
    }
    assert!(pending_mark.contains(&partial_pack_artifact));
    assert_eq!(
        std::fs::read(&manifest_path).expect("pending primary manifest bytes"),
        manifest_bytes
    );
    let pending_first = snapshot_for_lease(&store, &pending, first, peer, relationship, None);
    let pending_peer = snapshot_for_lease(&store, &pending, peer, first, relationship, None);
    assert_semantic_snapshot_advanced(&prepared_after_first, &pending_first);
    assert_semantic_snapshot_advanced(&prepared_after_peer, &pending_peer);
    let pending_generation = pending.bundle().base().generation;
    assert_eq!(pending_first.generation, pending_generation.get());
    assert_eq!(pending_peer.generation, pending_generation.get());
    eprintln!(
        "ZE46_PRIMARY_PENDING required={pending_required:?} binding={:?} candidate={:?} replacement={replacement_descriptors:?}",
        pending_manifest.binding, checkpoint_manifest.object,
    );
    drop(pending);

    drop(replacement_registration);
    store.close().expect("close exact primary pending store");
    drop(store);
    assert!(
        manifest_path.exists(),
        "primary manifest disappeared before resume"
    );

    vfs.take();
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let writable = Store::open_native_graph_with_infrastructure(
        &path,
        options(),
        Some(document.clone()),
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
    )
    .expect("resume exact primary reclaim intent");
    assert!(
        !manifest_path.exists(),
        "primary manifest survived writable resume"
    );
    let resume_events = vfs.take();
    let deletes: Vec<_> = resume_events
        .iter()
        .filter_map(|event| match event {
            DurabilityEvent::Delete(path) => Some(path.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(deletes, vec![manifest_path.clone()]);
    for (path, bytes) in replacement_paths.iter().zip(replacement_bytes.iter()) {
        let actual = std::fs::read(path).expect("replacement allocation after resume");
        assert_eq!(actual.as_slice(), bytes.as_slice());
    }
    assert_eq!(
        std::fs::read(&partial_pack_path).expect("partly live pack after resume"),
        partial_pack_bytes
    );

    let completed = writable
        .admit_native_read()
        .expect("primary completion observation");
    let completion_required = completed
        .bundle()
        .reclaim()
        .expect("durable primary completion root");
    let (completion_manifest, completed_candidate) = {
        let shared = crate::property_graph::resources::GraphResources::from_store(&writable)
            .expect("primary completion resources");
        let control = QueryControl::Cancel(CancelToken::new());
        let writer =
            WriteMemory::new(&shared, WriteLimits::default()).expect("primary completion memory");
        let memory = StorageMemory::new(&writer, &control, 8 * 1024 * 1024)
            .expect("primary completion storage memory");
        let source = NativePreparationSource::new(&completed, &memory, 1)
            .expect("primary completion source");
        let mut resources = source
            .resources(32 * 1024 * 1024)
            .expect("primary completion tree resources");
        let block = source
            .resolve(completion_required.block, &mut resources)
            .expect("resolve primary reclaim completion");
        let manifest = crate::property_graph::storage::reclaim::decode_completed_intent_manifest(
            block.payload(),
        )
        .expect("decode primary reclaim completion");
        let candidate = crate::property_graph::storage::reclaim::completed_intent_candidate_at(
            block.payload(),
            0,
        )
        .expect("decode completed primary candidate");
        (manifest, candidate)
    };
    assert_eq!(completion_manifest.intent, pending_required);
    assert_eq!(completion_manifest.completed_count, 1);
    assert_eq!(completion_manifest.remaining_count, 0);
    assert_eq!(completed_candidate, checkpoint_manifest.object);
    assert_eq!(
        completed.bundle().base().generation.get(),
        pending_generation.get() + 1
    );
    let completed_first =
        snapshot_for_lease(&writable, &completed, first, peer, relationship, None);
    let completed_peer = snapshot_for_lease(&writable, &completed, peer, first, relationship, None);
    assert_semantic_snapshot_advanced(&pending_first, &completed_first);
    assert_semantic_snapshot_advanced(&pending_peer, &completed_peer);
    eprintln!(
        "ZE46_PRIMARY_COMPLETED root={completion_required:?} intent={:?} candidate={completed_candidate:?} deletes={deletes:?}",
        completion_manifest.intent,
    );
    drop(completed);
    writable.close().expect("close completed primary store");
}

#[test]
fn older_wal_manifest_retention_without_explicit_registration() {
    run_older_wal_manifest_retention_without_explicit_registration();
}

fn run_older_wal_manifest_retention_without_explicit_registration() {
    let parent = super::tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let vfs = Arc::new(RecordingVfs::default());
    let store = create_reclaim_test_store(&path, &vfs);

    let initial = store
        .admit_native_read()
        .expect("initial WAL witness reader");
    let initial_checkpoint = initial.bundle().root_envelope();
    let initial_sequence = initial.bundle().sequence();
    drop(initial);

    let image = CanonicalContents::node(&mut [], &mut [], Some("older WAL manifest witness"), None)
        .expect("older WAL node image");
    let request = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "ze46-older-wal", "witness")
            .expect("older WAL application key"),
        revision: GraphRevision::new(1).expect("older WAL revision"),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&image)),
    }];
    let receipt = store
        .apply_native_graph(&request, &QueryControl::Cancel(CancelToken::new()))
        .expect("create older WAL witness")[0];
    assert!(!receipt.replayed);

    let seeded = store
        .admit_native_read()
        .expect("seeded WAL witness reader");
    let wal_manifest = seeded
        .bundle()
        .prepared_inventories()
        .first()
        .copied()
        .expect("authentic older WAL manifest");
    let seed_sequence = seeded.bundle().sequence();
    let wal_manifest_path = crate::property_graph::storage::allocation::artifact_path(
        &path,
        wal_manifest.object.artifact,
    );
    let wal_manifest_bytes = std::fs::read(&wal_manifest_path).expect("older WAL manifest bytes");
    assert!(wal_manifest.object.serial > initial_checkpoint.object.serial);
    eprintln!(
        "ZE46_OLDER_WAL_WITNESS object={:?} block={:?} initial_checkpoint={:?} initial_sequence={} seed_sequence={} byte_length={} descriptor_checksum={}",
        wal_manifest.object,
        wal_manifest.block,
        initial_checkpoint,
        initial_sequence,
        seed_sequence,
        wal_manifest_bytes.len(),
        wal_manifest.object.checksum,
    );
    drop(seeded);

    let first = store
        .admit_native_graph_maintenance()
        .expect("first older WAL maintenance admission");
    let first_report = store
        .commit_native_graph_maintenance(&first, &QueryControl::Cancel(CancelToken::new()))
        .expect("fold older WAL manifest");
    assert_eq!(first_report.removed_bytes, 0);
    drop(first);

    let observed = store
        .admit_native_read()
        .expect("observe folded older WAL manifest");
    assert!(
        !observed
            .bundle()
            .prepared_inventories()
            .contains(&wal_manifest)
    );
    assert!(
        rooted_inventory_for_lease(&store, &observed)
            .iter()
            .any(|change| change.object == wal_manifest.object
                && change.state == InventoryState::Retained),
        "older WAL manifest is absent from rooted inventory"
    );
    assert_eq!(
        std::fs::read(&wal_manifest_path).expect("folded older WAL manifest bytes"),
        wal_manifest_bytes
    );
    assert!(observed.bundle().reclaim().is_none());
    let target_checkpoint = observed.bundle().root_envelope();
    let target_sequence = observed.bundle().sequence();
    let target_generation = observed.bundle().base().generation;
    assert_eq!(target_checkpoint, initial_checkpoint);
    assert!(target_sequence > seed_sequence);
    assert_eq!(target_sequence, seed_sequence + 1);
    drop(observed);

    {
        let state = store
            .native_graph
            .state
            .lock()
            .expect("older WAL publication state");
        assert!(
            state.preparations.iter().all(Option::is_none),
            "older WAL witness has an explicit prepared registration"
        );
    }

    let (wal_identity, wal_first_sequence, wal_bytes) = {
        let capture = store
            .capture_native_read_roots()
            .expect("capture older WAL authority");
        assert_eq!(capture.bundles().len(), 1);
        assert!(capture.leases().is_empty());
        assert!(capture.spills().is_empty());
        assert!(capture.contains_prepared(wal_manifest.object));
        let bundle = capture.bundles().first().expect("captured current bundle");
        assert_eq!(bundle.root_envelope(), target_checkpoint);
        assert_eq!(bundle.sequence(), target_sequence);
        let wal = capture.wal().expect("captured older WAL");
        assert!(wal.bytes() > crate::property_graph::wal::HEADER_BYTES);
        assert_eq!(wal.first_sequence(), initial_sequence + 1);

        let state_names_manifest = |state: &crate::property_graph::wal::CommitState<'_>| {
            state
                .graph
                .slots
                .into_iter()
                .flatten()
                .chain(
                    [state.text, state.vector, state.reclaim]
                        .into_iter()
                        .flatten(),
                )
                .chain(std::iter::once(state.catalog))
                .any(|required| {
                    required.object.artifact == wal_manifest.object.artifact
                        || required.block.artifact == wal_manifest.object.artifact
                })
        };
        let mut visited_sequences = Vec::new();
        let mut checkpoint_visits = 0_usize;
        let mut seed_visit = false;
        let mut target_visit = false;
        let mut seed_complete_bytes = None;
        super::super::recovery::visit_captured_state(
            &store,
            &path,
            initial_checkpoint,
            target_sequence,
            Some(super::super::recovery::CapturedWalCutoff {
                identity: wal.identity(),
                first_sequence: wal.first_sequence(),
                bytes: wal.bytes(),
            }),
            &QueryControl::Cancel(CancelToken::new()),
            |visit, resources| {
                match visit {
                    super::super::recovery::CapturedStateVisit::Checkpoint { state, .. } => {
                        checkpoint_visits += 1;
                        visited_sequences.push(state.sequence);
                        assert_eq!(state.sequence, initial_sequence);
                        assert_eq!(state.prepared_inventories.len()?, 0);
                        assert!(!state_names_manifest(&state));
                    }
                    super::super::recovery::CapturedStateVisit::Envelope {
                        state,
                        complete_bytes,
                        ..
                    } => {
                        visited_sequences.push(state.sequence);
                        assert!(state.sequence <= target_sequence);
                        if state.sequence == seed_sequence {
                            seed_visit = true;
                            seed_complete_bytes = Some(complete_bytes);
                            assert_eq!(state.prepared_inventories.len()?, 1);
                            assert_eq!(state.prepared_inventories.get(0, resources)?, wal_manifest);
                        } else if state.sequence == target_sequence {
                            target_visit = true;
                            assert_eq!(complete_bytes, wal.bytes());
                            for index in 0..state.prepared_inventories.len()? {
                                assert_ne!(
                                    state.prepared_inventories.get(index, resources)?,
                                    wal_manifest
                                );
                            }
                            assert!(!state_names_manifest(&state));
                        } else {
                            panic!("unexpected older WAL envelope sequence {}", state.sequence);
                        }
                    }
                }
                Ok(())
            },
        )
        .expect("authenticate older WAL captured state");
        assert_eq!(checkpoint_visits, 1);
        assert!(seed_visit && target_visit);
        assert_eq!(
            visited_sequences,
            vec![initial_sequence, seed_sequence, target_sequence]
        );
        assert!(seed_complete_bytes.is_some_and(|bytes| bytes < wal.bytes()));
        eprintln!(
            "ZE46_OLDER_WAL_CUTOFF identity={} first_sequence={} bytes={} sequences={visited_sequences:?} writer_prepared={}",
            wal.identity(),
            wal.first_sequence(),
            wal.bytes(),
            capture.contains_prepared(wal_manifest.object),
        );
        (wal.identity(), wal.first_sequence(), wal.bytes())
    };

    vfs.take();
    let second = store
        .admit_native_graph_maintenance()
        .expect("older WAL protection admission");
    let result =
        store.commit_native_graph_maintenance(&second, &QueryControl::Cancel(CancelToken::new()));
    let events = vfs.take();
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, DurabilityEvent::Delete(_))),
        "older WAL protection deleted a file: {events:?}"
    );
    let report = result.expect("older WAL protection maintenance");
    assert_eq!(report.removed_bytes, 0);
    drop(second);

    let proof = store
        .admit_native_read()
        .expect("older WAL proof observation");
    let (records, mark) = durable_proof_for_lease(&store, &proof);
    assert!(
        records.iter().any(|record| {
            record.class == ProtectedClass::Wal
                && matches!(
                    record.value,
                    ProtectedValue::WalAuthority {
                        identity,
                        first_sequence,
                        bytes,
                    } if identity == wal_identity
                        && first_sequence == wal_first_sequence
                        && bytes == u64::try_from(wal_bytes).expect("WAL cutoff byte count")
                )
        }),
        "exact older WAL authority is absent: {records:?}"
    );
    assert!(records.contains(&ProtectedRecord::captured_state(
        ProtectedClass::Current,
        initial_checkpoint,
        target_sequence,
    )));
    assert!(records.contains(&ProtectedRecord::captured_state(
        ProtectedClass::PreparedBase,
        initial_checkpoint,
        target_sequence,
    )));
    assert!(
        records
            .iter()
            .all(|record| record.class != ProtectedClass::Reader),
        "older WAL proof retained a reader: {records:?}"
    );
    assert!(
        mark.contains(&wal_manifest.object.artifact),
        "older WAL manifest is absent from completed mark"
    );
    let candidates = if proof.bundle().reclaim().is_some() {
        pending_reclaim_candidates(&store, &proof)
    } else {
        Vec::new()
    };
    assert!(
        !candidates.contains(&wal_manifest.object),
        "older WAL manifest entered reclaim candidates: {candidates:?}"
    );
    assert!(
        candidates.is_empty(),
        "isolated older WAL fixture has pending candidates: {candidates:?}"
    );
    assert_eq!(
        std::fs::read(&wal_manifest_path).expect("protected older WAL manifest bytes"),
        wal_manifest_bytes
    );
    let proof_sequence = proof.bundle().sequence();
    let proof_generation = proof.bundle().base().generation;
    assert!(proof_sequence > target_sequence);
    assert!(proof_generation > target_generation);
    drop(proof);

    let replay = store
        .apply_native_graph(&request, &QueryControl::Cancel(CancelToken::new()))
        .expect("replay older WAL request");
    assert_eq!(replay.len(), 1);
    assert!(replay[0].replayed);
    assert_eq!(replay[0].entity, receipt.entity);
    assert_eq!(replay[0].generation, receipt.generation);
    let after_replay = store
        .admit_native_read()
        .expect("observe idempotent older WAL replay");
    assert_eq!(after_replay.bundle().sequence(), proof_sequence);
    assert_eq!(after_replay.bundle().base().generation, proof_generation);
    drop(after_replay);
    store.close().expect("close older WAL witness store");
}

#[cfg(feature = "test-support")]
#[test]
fn ze46_reclaim_oracle_catches_early_unlink_and_missing_wal_protection() {
    run_ze46_reclaim_oracle_catches_early_unlink_and_missing_wal_protection();
}

/// The two guards an unsafe reclaimer would break, then the observations the
/// independent runner comparator consumes. Planted production defects make
/// this case fail for the intended reason: an unlink before the durable
/// intent breaks the before-intent crash cell, and a mark that skips the
/// uncheckpointed WAL references breaks the WAL-only witness.
#[cfg(feature = "test-support")]
fn run_ze46_reclaim_oracle_catches_early_unlink_and_missing_wal_protection() {
    run_older_wal_manifest_retention_without_explicit_registration();
    run_reclaim_crash_cell(CrashCell::BeforeIntent);
    wal_only_witness_is_guarded_and_never_selected();
    let state = observe_reclaim_cycle();
    assert_eq!(
        state.relationships,
        vec![crate::graph_read_view_test_support::ObservedRelationship {
            rel: 1,
            source: state.first_node,
            target: state.second_node,
            relationship_type: 1,
        }],
        "the committed relationship did not survive the reclaim cycle"
    );
    assert!(
        state.replayed,
        "the original request was not an exact replay"
    );
    assert_eq!(
        state.replay_generation, 1,
        "replay lost its original generation"
    );
    assert!(state.removed_bytes > 0);
    assert_eq!(state.removed_bytes, state.unlinked_file_bytes);
}

/// A manifest that only the uncheckpointed WAL still protects. With its mark
/// edge omitted the independent guard refuses before any mutation; with the
/// real mark it is never a candidate and its bytes never change.
#[cfg(feature = "test-support")]
fn wal_only_witness_is_guarded_and_never_selected() {
    let parent = super::tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("native");
    let vfs = Arc::new(RecordingVfs::default());
    let store = create_reclaim_test_store(&path, &vfs);
    let image = CanonicalContents::node(&mut [], &mut [], Some("witness"), None).expect("node");
    store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "witness", "node").expect("key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("witness write");
    let witness = store
        .admit_native_read()
        .expect("witness reader")
        .bundle()
        .prepared_inventories()
        .first()
        .copied()
        .expect("first prepared manifest");
    commit_maintenance(&store).expect("fold retires the witness manifest root");
    let folded = store.admit_native_read().expect("folded reader");
    assert!(!folded.bundle().prepared_inventories().contains(&witness));
    assert!(
        rooted_inventory_for_lease(&store, &folded)
            .iter()
            .any(|change| change.object == witness.object),
        "the retired manifest stays allocation bookkeeping"
    );
    drop(folded);
    let witness_path =
        crate::property_graph::storage::allocation::artifact_path(&path, witness.object.artifact);
    let witness_bytes = std::fs::read(&witness_path).expect("witness bytes");

    let before = directory_image(&path);
    vfs.take();
    omit_mark_artifact_for_test(witness.object.artifact);
    let error = commit_maintenance(&store).expect_err("omitted WAL-only protection");
    assert!(
        take_omitted_mark_emissions_for_test() > 0,
        "the omission never fired"
    );
    assert!(
        matches!(
            &error,
            super::super::NativeGraphError::Read(
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "protected root is absent from completed mark"
                )
            )
        ),
        "{error:?}"
    );
    assert!(delete_events(&vfs.take()).is_empty());
    assert_live_files_unchanged(&path, &before);
    super::publication::record_verified_fault();

    for _ in 0..3 {
        commit_maintenance(&store).expect("maintenance with the real mark");
        let lease = store.admit_native_read().expect("round reader");
        assert!(
            !pending_reclaim_candidates_or_empty_root(&store, &lease).contains(&witness.object),
            "the WAL-only witness became a reclaim candidate"
        );
        assert_eq!(
            std::fs::read(&witness_path).expect("witness survives"),
            witness_bytes
        );
    }
    store.close().expect("close witness store");
    let reopened = Store::open_native_graph(&path, options(), None).expect("reopen witness store");
    reopened.close().expect("close reopened witness store");
}

/// One single-node create, keyed by group and index, so a ZE-163 history is
/// a run of one-envelope commits over an otherwise fixed store.
fn ze163_base_write(store: &Store, group: &str, index: usize) {
    let text = format!("ze163 {group} row {index}");
    let image =
        CanonicalContents::node(&mut [], &mut [], Some(&text), None).expect("ze163 node image");
    store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, group, &index.to_string())
                    .expect("ze163 key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("ze163 write");
}

/// ZE-163: one durable proof walks one captured state completely per bundle,
/// the bundle's checkpoint, plus the bundle's own target roots. Every
/// uncheckpointed envelope between them contributes only its change
/// references and the roots replay maps before it replays that envelope, so
/// proof cost follows the changed paths and not the history length, on the
/// producer and again on every reopen over the pending reclaim.
#[test]
fn ze163_proof_work_grows_with_changed_paths_not_with_history_length() {
    run_ze163_proof_work_grows_with_changed_paths_not_with_history_length();
}

fn run_ze163_proof_work_grows_with_changed_paths_not_with_history_length() {
    const HISTORY: [usize; 3] = [1, 4, 16];
    const BASE_ROWS: usize = 40;
    let mut producer_traces = Vec::new();
    let mut recovery_traces = Vec::new();
    let mut proof_work = Vec::new();
    for envelopes in HISTORY {
        let parent = super::tempfile::tempdir().expect("ze163 parent");
        let path = parent.path().join("native");
        let vfs = Arc::new(RecordingVfs::default());
        let store = create_reclaim_test_store(&path, &vfs);
        // A fixed base, large enough that one full traversal costs far more
        // than one single-node envelope's own rewritten root paths.
        for index in 0..BASE_ROWS {
            ze163_base_write(&store, "ze163-base", index);
        }
        // One replacement before the checkpoint leaves reclaimable packs, so
        // the measured maintenance publishes a real pending intent and the
        // reopen below has an authority to revalidate.
        commit_maintenance(&store).expect("ze163 base replacement");
        store
            .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
            .expect("ze163 base checkpoint");
        for index in 0..envelopes {
            ze163_base_write(&store, "ze163-tail", index);
        }
        let _ = crate::lifecycle::native_graph::recovery::take_state_trace_count_for_test();
        commit_maintenance(&store).expect("ze163 maintenance");
        producer_traces
            .push(crate::lifecycle::native_graph::recovery::take_state_trace_count_for_test());
        proof_work.push(
            store
                .native_graph
                .proof_work
                .load(std::sync::atomic::Ordering::Acquire),
        );
        {
            let lease = store.admit_native_read().expect("ze163 pending reader");
            assert!(
                lease.bundle().reclaim().is_some(),
                "the measured maintenance must publish a pending reclaim intent"
            );
            drop(lease);
        }
        store.close().expect("ze163 close");
        let _ = crate::lifecycle::native_graph::recovery::take_state_trace_count_for_test();
        let reopened =
            Store::open_native_graph(&path, options(), None).expect("ze163 reopen over reclaim");
        recovery_traces
            .push(crate::lifecycle::native_graph::recovery::take_state_trace_count_for_test());
        reopened.close().expect("ze163 close reopened");
    }
    let report = format!(
        "history={HISTORY:?} producer={producer_traces:?} \
         recovery={recovery_traces:?} work={proof_work:?}"
    );
    println!("ze163 proof work: {report}");
    assert_eq!(
        producer_traces,
        vec![1_u64, 1, 1],
        "one producer state trace per captured bundle: {report}"
    );
    // Two protected captured-state records survive the reopen, Current and
    // PreparedBase, and each retraces exactly its checkpoint and its target.
    assert_eq!(
        recovery_traces,
        vec![4_u64, 4, 4],
        "recovery retrace count must not follow history length: {report}"
    );
    let base = *proof_work.first().expect("ze163 base work");
    let longest = *proof_work.last().expect("ze163 longest work");
    assert!(
        longest.saturating_sub(base) <= base / 2,
        "marginal proof work must stay under half of one traversal: {report}"
    );
}

/// ZE-163: `validate_mark_reference` used to walk the whole completed mark
/// once per traced reference. `DurableRunReader::contains` answers the same
/// question with one page read per run level, so the cost of proving a
/// reference live no longer follows the mark's length.
#[test]
fn ze163_mark_membership_is_a_bounded_descent_not_a_run_scan() {
    run_ze163_mark_membership_is_a_bounded_descent_not_a_run_scan();
}

fn run_ze163_mark_membership_is_a_bounded_descent_not_a_run_scan() {
    let parent = super::tempfile::tempdir().expect("ze163 mark parent");
    let path = parent.path().join("native");
    let store = Store::create_native_graph(&path, options(), None).expect("ze163 mark store");
    ze163_base_write(&store, "ze163-mark", 0);
    let admission = store
        .admit_native_graph_maintenance()
        .expect("ze163 mark admission");
    // Enough ids to fill several leaf pages, so a run scan and a
    // root-to-leaf descent cost visibly different numbers of pages.
    let input: Vec<u128> = (1..=2048).collect();
    let report = super::super::maintenance::run_spill_probe(&store, &admission, &input, 256)
        .expect("ze163 membership probe");
    let descent = u64::from(report.run.height() + 1);
    let report_line = format!(
        "height={} descent={descent} walk_pages={} queries={} membership_pages={}",
        report.run.height(),
        report.walk_page_reads,
        report.membership_queries,
        report.membership_page_reads
    );
    println!("ze163 mark membership: {report_line}");
    assert_eq!(report.ordered.len(), input.len(), "{report_line}");
    assert!(report.membership_queries > 0, "{report_line}");
    assert!(
        report.run.height() >= 1,
        "the probe must build a multi-level run: {report_line}"
    );
    assert!(
        report.membership_page_reads <= report.membership_queries * descent,
        "each membership question reads at most one page per level: {report_line}"
    );
    assert!(
        report.walk_page_reads >= 3 * descent,
        "a run scan must cost several descents, or the gate proves nothing: \
         {report_line}"
    );
    drop(admission);
    store.close().expect("ze163 close mark store");
}

/// ZE-187: a plain store that checkpoints, writes uncheckpointed tail
/// envelopes, and then runs a complete reclaim cycle (one maintenance that
/// publishes the pending intent, one that completes it) must still reopen.
/// Those tail envelopes predate the intent, so their inventories still list
/// the candidates as `Retained`; that superseded claim may not make open
/// demand a file the completed reclaim legitimately unlinked. The control
/// below keeps the real claim loud: an artifact the published state still
/// inventories is a typed `NotFound` when it is absent.
#[test]
fn ze187_plain_store_reopens_after_a_completed_reclaim() {
    run_ze187_plain_store_reopens_after_a_completed_reclaim();
}

fn run_ze187_plain_store_reopens_after_a_completed_reclaim() {
    let parent = super::tempfile::tempdir().expect("ze187 parent");
    let path = parent.path().join("native");
    let vfs = Arc::new(RecordingVfs::default());
    let store = create_reclaim_test_store(&path, &vfs);
    for index in 0..24 {
        ze163_base_write(&store, "ze187-base", index);
    }
    commit_maintenance(&store).expect("ze187 base replacement");
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .expect("ze187 checkpoint");
    for index in 0..3 {
        ze163_base_write(&store, "ze187-tail", index);
    }
    vfs.take();
    commit_maintenance(&store).expect("ze187 pending maintenance");
    let pending_deletes = delete_events(&vfs.take());
    commit_maintenance(&store).expect("ze187 completing maintenance");
    let completing_deletes = delete_events(&vfs.take());
    println!(
        "ze187 deletes: pending={} completing={}",
        pending_deletes.len(),
        completing_deletes.len()
    );
    assert!(
        pending_deletes.is_empty(),
        "the first maintenance only publishes the intent: {pending_deletes:?}"
    );
    assert!(
        !completing_deletes.is_empty(),
        "the second maintenance must complete the reclaim"
    );
    store.close().expect("ze187 close");
    let reopened = Store::open_native_graph(&path, options(), None)
        .expect("ze187 reopen after a completed reclaim");
    // The liveness proof moved to the published state; it did not become a
    // best-effort skip. An artifact that state still inventories must stay a
    // loud typed error when it is absent.
    let victim = {
        let lease = reopened.admit_native_read().expect("ze187 witness lease");
        let inventory = rooted_inventory_for_lease(&reopened, &lease);
        let victim = inventory
            .iter()
            .find(|change| {
                change.state == InventoryState::Retained
                    && crate::property_graph::storage::allocation::artifact_path(
                        &path,
                        change.object.artifact,
                    )
                    .exists()
            })
            .map(|change| change.object.artifact)
            .expect("ze187 live inventoried artifact");
        drop(lease);
        victim
    };
    reopened.close().expect("ze187 close reopened");
    let victim_path = crate::property_graph::storage::allocation::artifact_path(&path, victim);
    std::fs::remove_file(&victim_path).expect("ze187 remove a live artifact");
    let Err(error) = Store::open_native_graph(&path, options(), None) else {
        panic!("ze187 a missing live artifact must refuse");
    };
    assert!(
        matches!(
            &error,
            super::super::NativeGraphError::Io { path: missing, source }
                if missing == &victim_path && source.kind() == std::io::ErrorKind::NotFound
        ),
        "{error:?}"
    );
}

fn ze163_document_tower() -> EmbeddingTower {
    EmbeddingTower {
        model_id: "ze163-witness".into(),
        model_version: "1".into(),
        weights_digest: vec![0x16, 0x03],
        dims: 2,
        normalization: Normalization::None,
        prompt_prefix: "doc: ".into(),
        max_tokens: 32,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    }
}

fn ze163_witness_store(path: &Path, vfs: &Arc<RecordingVfs>, document: &EmbeddingTower) -> Store {
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    Store::create_native_graph_with_infrastructure(
        path,
        options(),
        Some(document.clone()),
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
        &mut crate::property_graph::storage::allocation::OsEntropy,
    )
    .expect("ze163 witness store")
}

#[allow(
    clippy::too_many_arguments,
    reason = "one complete keyed write, spelled out so each fixture row is \
              readable at its call site"
)]
fn ze163_keyed_write(
    store: &Store,
    document: &EmbeddingTower,
    group: &str,
    key: &str,
    revision: u64,
    operation: StructuredOperation,
    text: &str,
    coordinates: [f32; 2],
) -> EntityId {
    let embedding = CanonicalEmbedding::new(document, &coordinates).expect("keyed embedding");
    let image = CanonicalContents::node(&mut [], &mut [], Some(text), Some(embedding))
        .expect("keyed image");
    let receipts = store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, group, key).expect("keyed key"),
                revision: GraphRevision::new(revision).expect("keyed revision"),
                operation,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .expect("keyed write");
    receipts[0].entity
}

/// One checkpoint, then at least three uncheckpointed one-node envelopes over
/// the same key, so a middle envelope's records are superseded before the
/// target state is reached.
fn ze163_seed_superseded_history(store: &Store, document: &EmbeddingTower) {
    for index in 0..24 {
        ze163_keyed_write(
            store,
            document,
            "ze163-base",
            &index.to_string(),
            1,
            StructuredOperation::Create,
            &format!("ze163 witness base row {index} with some words"),
            [index as f32 / 32.0, 1.0 - index as f32 / 32.0],
        );
    }
    // A replacement before the checkpoint leaves reclaimable packs, so the
    // measured maintenance publishes a real pending intent.
    commit_maintenance(store).expect("ze163 witness base replacement");
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .expect("ze163 witness checkpoint");
    let mid = match ze163_keyed_write(
        store,
        document,
        "ze163-mid",
        "mid",
        1,
        StructuredOperation::Create,
        "witness mid revision one alpha beta gamma",
        [0.11, 0.89],
    ) {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("ze163 witness mid identity"),
    };
    // Envelope two is the middle state. Envelope three rewrites the same key,
    // so every page envelope two wrote for it is superseded before the target.
    for (revision, word) in [(2_u64, "delta"), (3, "epsilon")] {
        ze163_keyed_write(
            store,
            document,
            "ze163-mid",
            "mid",
            revision,
            StructuredOperation::Put(EntityId::Node(mid)),
            &format!("witness mid revision {revision} {word} zeta eta theta"),
            [0.11 * revision as f32, 0.5],
        );
    }
}

/// What the removed per-intermediate-state retrace used to reach, measured
/// against what ZE-163 kept.
struct Ze163Witness {
    states: usize,
    /// Artifacts that neither the checkpoint's nor the target's complete
    /// traversal reaches, so only the intermediate history names them.
    intermediate_only: std::collections::BTreeSet<u128>,
    /// Of those, the ones the surviving cover does not reach either. The
    /// surviving cover is the checkpoint trace, the target trace, every
    /// state's `Roots` trace and every envelope's change references.
    uncovered: std::collections::BTreeSet<u128>,
}

/// Traces one captured bundle twice per state, at both `CapturedTraceDepth`
/// values, and reports which artifacts the deeper trace was the only witness
/// for. The measurement excludes `writer.protected`, the current bundle's own
/// trace and the durable proof roots, all of which the production mark also
/// emits, so an empty `uncovered` set is a conservative result.
fn ze163_witness_report(store: &Store, lease: &super::super::NativeReadLease) -> Ze163Witness {
    use std::collections::BTreeSet;
    type Ref = crate::property_graph::storage::artifact::PhysicalRef;
    type Res<'a> = crate::property_graph::storage::tree::directory::TreeResources<'a>;
    type Err = crate::property_graph::storage::tree::directory::TreeError;

    struct Captured {
        sequence: u64,
        edge: bool,
        roots: BTreeSet<u128>,
        complete: BTreeSet<u128>,
        changes: BTreeSet<u128>,
    }

    let bundle = lease.bundle();
    let control = QueryControl::Cancel(CancelToken::new());
    let shared = crate::property_graph::resources::GraphResources::from_store(store)
        .expect("ze163 witness shared");
    let write_memory =
        WriteMemory::new(&shared, WriteLimits::default()).expect("ze163 witness write memory");
    let memory = StorageMemory::new(&write_memory, &control, 32 * 1024 * 1024)
        .expect("ze163 witness memory");
    let mut resources =
        crate::property_graph::storage::tree::directory::TreeResources::for_prepare(
            &memory,
            crate::property_graph::storage::reclaim::MARK_WORK_LIMIT,
        )
        .expect("ze163 witness resources");
    let expected = crate::property_graph::catalog::GraphInterpretation::new(
        bundle.lexical(),
        bundle.document(),
    )
    .expect("ze163 witness interpretation");

    let mut per_state: Vec<Captured> = Vec::new();
    super::super::recovery::visit_captured_state(
        store,
        bundle.directory(),
        bundle.root_envelope(),
        bundle.sequence(),
        None,
        &control,
        |visit, wal_resources| {
            let (state, changes, is_checkpoint) = match visit {
                super::super::recovery::CapturedStateVisit::Checkpoint { state, .. } => {
                    (state, None, true)
                }
                super::super::recovery::CapturedStateVisit::Envelope { state, changes, .. } => {
                    (state, Some(changes), false)
                }
            };
            let mut change_set: BTreeSet<u128> = BTreeSet::new();
            if let Some(changes) = changes {
                let mut visit_change = |reference: Ref, _res: &mut Res<'_>| -> Result<(), Err> {
                    change_set.insert(reference.artifact.get());
                    Ok(())
                };
                super::super::recovery::trace_captured_change_references(
                    store,
                    bundle.directory(),
                    state,
                    changes,
                    wal_resources,
                    &memory,
                    &mut resources,
                    &mut visit_change,
                )?;
            }
            let mut roots: BTreeSet<u128> = BTreeSet::new();
            let mut complete: BTreeSet<u128> = BTreeSet::new();
            for (depth, sink) in [
                (
                    super::super::recovery::CapturedTraceDepth::Roots,
                    &mut roots,
                ),
                (
                    super::super::recovery::CapturedTraceDepth::Complete,
                    &mut complete,
                ),
            ] {
                let mut visit_depth = |reference: Ref, _res: &mut Res<'_>| -> Result<(), Err> {
                    sink.insert(reference.artifact.get());
                    Ok(())
                };
                super::super::recovery::trace_captured_state_references(
                    store,
                    bundle.directory(),
                    bundle.root_envelope(),
                    state,
                    expected,
                    bundle.document(),
                    &memory,
                    depth,
                    &mut resources,
                    &mut visit_depth,
                )?;
            }
            per_state.push(Captured {
                sequence: state.sequence,
                edge: is_checkpoint || state.sequence == bundle.sequence(),
                roots,
                complete,
                changes: change_set,
            });
            Ok(())
        },
    )
    .expect("ze163 witness captured walk");
    drop(resources);

    let mut edges: BTreeSet<u128> = BTreeSet::new();
    let mut surviving: BTreeSet<u128> = BTreeSet::new();
    for entry in &per_state {
        surviving.extend(entry.changes.iter().copied());
        surviving.extend(entry.roots.iter().copied());
        if entry.edge {
            edges.extend(entry.complete.iter().copied());
            surviving.extend(entry.complete.iter().copied());
        }
    }
    let mut intermediate_only: BTreeSet<u128> = BTreeSet::new();
    let mut uncovered: BTreeSet<u128> = BTreeSet::new();
    for entry in &per_state {
        if entry.edge {
            continue;
        }
        intermediate_only.extend(entry.complete.difference(&edges).copied());
        uncovered.extend(entry.complete.difference(&surviving).copied());
    }
    println!(
        "ze163 witness: states={} edges={} surviving={} intermediate_only={} uncovered={}",
        per_state.len(),
        edges.len(),
        surviving.len(),
        intermediate_only.len(),
        uncovered.len()
    );
    for entry in &per_state {
        println!(
            "ze163 witness state seq={} edge={} roots={} complete={} changes={}",
            entry.sequence,
            entry.edge,
            entry.roots.len(),
            entry.complete.len(),
            entry.changes.len()
        );
    }
    Ze163Witness {
        states: per_state.len(),
        intermediate_only,
        uncovered,
    }
}

/// Plants the mark omission for one artifact and runs maintenance over it.
/// The omission must fire and nothing may be unlinked; whether the producer
/// refuses or defers the refusal to the next open is the caller's assertion.
fn ze163_plant_omitted_mark(
    store: &Store,
    vfs: &Arc<RecordingVfs>,
    artifact: crate::property_graph::storage::artifact::ArtifactId,
) -> Option<super::super::NativeGraphError> {
    vfs.take();
    omit_mark_artifact_for_test(artifact);
    let result = commit_maintenance(store);
    let fired = take_omitted_mark_emissions_for_test();
    let error = match result {
        Ok(report) => {
            assert_eq!(report.removed_bytes, 0, "a planted omission removed bytes");
            None
        }
        Err(error) => Some(error),
    };
    println!(
        "ze163 witness omission: fired={fired} refused={}",
        error.is_some()
    );
    assert!(
        delete_events(&vfs.take()).is_empty(),
        "a planted omission deleted a file"
    );
    assert!(fired > 0, "the planted omission never fired");
    error
}

fn ze163_assert_absent_from_mark_refusal(error: &super::super::NativeGraphError) {
    let message = match error {
        super::super::NativeGraphError::Read(
            crate::property_graph::storage::tree::directory::TreeError::Invalid(message),
        ) => *message,
        other => panic!("unexpected refusal: {other:?}"),
    };
    assert!(
        message == "protected root is absent from completed mark"
            || message == "captured live reference absent from completed mark",
        "unexpected refusal message: {message}"
    );
    println!("ze163 witness refusal: {message}");
}

/// ZE-163 stopped retracing every uncheckpointed envelope state in full. This
/// is the negative control for the class that retrace was the only witness
/// for: an artifact a middle envelope allocated, whose pages a later envelope
/// supersedes before the target state. The fixture proves that class is
/// non-empty, that what ZE-163 kept still reaches every member of it, and
/// that a member reaches the completed mark and survives a real reclaim.
#[test]
fn ze163_intermediate_tree_artifact_keeps_a_current_witness() {
    let parent = super::tempfile::tempdir().expect("ze163 current parent");
    let path = parent.path().join("native");
    let vfs = Arc::new(RecordingVfs::default());
    let document = ze163_document_tower();
    let store = ze163_witness_store(&path, &vfs, &document);
    ze163_seed_superseded_history(&store, &document);

    let lease = store.admit_native_read().expect("ze163 current reader");
    let report = ze163_witness_report(&store, &lease);
    drop(lease);
    assert!(
        report.states >= 4,
        "the fixture needs a checkpoint and at least three uncheckpointed \
         envelopes: states={}",
        report.states
    );
    assert!(
        !report.intermediate_only.is_empty(),
        "the fixture never allocated an artifact only an intermediate state \
         reaches, so this control proves nothing"
    );
    assert!(
        report.uncovered.is_empty(),
        "an intermediate state reached an artifact that the checkpoint trace, \
         the target trace, the retained roots and the change references do \
         not: {:032x?}",
        report.uncovered
    );

    let witness = crate::property_graph::storage::artifact::ArtifactId::new(
        *report
            .intermediate_only
            .first()
            .expect("intermediate-only artifact"),
    )
    .expect("nonzero witness artifact");
    let witness_path = crate::property_graph::storage::allocation::artifact_path(&path, witness);
    let witness_bytes = std::fs::read(&witness_path).expect("ze163 witness bytes");

    let before = directory_image(&path);
    let error = ze163_plant_omitted_mark(&store, &vfs, witness)
        .expect("the omitted intermediate artifact must refuse before mutation");
    ze163_assert_absent_from_mark_refusal(&error);
    assert_live_files_unchanged(&path, &before);

    // With the real mark the same artifact reaches it, is never selected, and
    // keeps its bytes across a real reclaim and the reopen over its intent.
    ze163_survives_a_pending_reclaim_round(store, &path, &document, witness, &witness_bytes, None);
}

/// One real reclaim round over a witness artifact, ending at the reopen over
/// the published pending intent. `retained`, when present, is held across the
/// whole round and released only before the close.
///
/// The second, completing maintenance is deliberately not run. ZE-187 records
/// a pre-existing reopen failure after a *completed* reclaim on a
/// post-checkpoint envelope chain; it reproduces on unmodified main `39e740c`
/// with every ZE-163 production file reverted (probe
/// `ze163_baseline_probe_plain_store_reopens_after_two_maintenances`, nextest
/// run `f872df48`), so it is not this change's. Letting it run here would
/// replace this control's signal with that one.
fn ze163_survives_a_pending_reclaim_round(
    store: Store,
    path: &Path,
    document: &EmbeddingTower,
    witness: crate::property_graph::storage::artifact::ArtifactId,
    witness_bytes: &[u8],
    retained: Option<super::super::NativeReadLease>,
) {
    let witness_path = crate::property_graph::storage::allocation::artifact_path(path, witness);
    commit_maintenance(&store).expect("ze163 maintenance with the real mark");
    let lease = store.admit_native_read().expect("ze163 round reader");
    assert!(
        lease.bundle().reclaim().is_some(),
        "the maintenance must publish a real pending reclaim intent, or this \
         round proves nothing about deletion authority"
    );
    assert!(
        !pending_reclaim_candidates_or_empty_root(&store, &lease)
            .iter()
            .any(|candidate| candidate.artifact == witness),
        "the intermediate-envelope artifact became a reclaim candidate"
    );
    drop(lease);
    assert_eq!(
        std::fs::read(&witness_path).expect("ze163 witness survives"),
        witness_bytes
    );
    drop(retained);
    store.close().expect("ze163 close");
    let reopened = Store::open_native_graph(path, options(), Some(document.clone()))
        .expect("ze163 reopen over the pending intent");
    assert_eq!(
        std::fs::read(&witness_path).expect("ze163 witness after reopen"),
        witness_bytes
    );
    reopened.close().expect("ze163 close reopened");
}

/// The same superseded-intermediate history, pinned by a retained reader
/// lease that is held across a checkpoint. `writer.protected` is rebuilt from
/// the uncheckpointed WAL, so after that checkpoint it no longer names this
/// history and cannot be the cover here.
fn ze163_reader_history_fixture(
    path: &Path,
    vfs: &Arc<RecordingVfs>,
    document: &EmbeddingTower,
) -> (Store, super::super::NativeReadLease) {
    let store = ze163_witness_store(path, vfs, document);
    ze163_seed_superseded_history(&store, document);
    let retained = store.admit_native_read().expect("ze163 retained reader");
    let retained_root = retained.bundle().root_envelope();
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .expect("ze163 checkpoint over the retained history");
    ze163_keyed_write(
        &store,
        document,
        "ze163-tail",
        "tail",
        1,
        StructuredOperation::Create,
        "witness tail row iota kappa lambda",
        [0.55, 0.45],
    );
    let current = store.admit_native_read().expect("ze163 current reader");
    assert_ne!(
        current.bundle().root_envelope(),
        retained_root,
        "the checkpoint did not move the writer past the retained history"
    );
    drop(current);
    (store, retained)
}

fn ze163_reader_witness(
    store: &Store,
    path: &Path,
    retained: &super::super::NativeReadLease,
) -> (
    crate::property_graph::storage::artifact::ArtifactId,
    Vec<u8>,
) {
    let report = ze163_witness_report(store, retained);
    assert!(
        report.states >= 4,
        "the retained history needs a checkpoint and at least three \
         uncheckpointed envelopes: states={}",
        report.states
    );
    assert!(
        !report.intermediate_only.is_empty(),
        "the retained history never allocated an artifact only an \
         intermediate state reaches, so this control proves nothing"
    );
    assert!(
        report.uncovered.is_empty(),
        "an intermediate reader state reached an artifact that the checkpoint \
         trace, the target trace, the retained roots and the change \
         references do not: {:032x?}",
        report.uncovered
    );
    let witness = crate::property_graph::storage::artifact::ArtifactId::new(
        *report
            .intermediate_only
            .first()
            .expect("intermediate-only artifact"),
    )
    .expect("nonzero witness artifact");
    let witness_path = crate::property_graph::storage::allocation::artifact_path(path, witness);
    let bytes = std::fs::read(&witness_path).expect("ze163 reader witness bytes");
    let writer_protected: Vec<crate::property_graph::storage::artifact::ArtifactId> = store
        .native_graph
        .writer
        .lock()
        .expect("ze163 writer state")
        .as_ref()
        .expect("ze163 writer")
        .protected
        .iter()
        .map(|descriptor| descriptor.artifact)
        .collect();
    assert!(
        !writer_protected.contains(&witness),
        "the checkpoint left this reader-history artifact in writer.protected, \
         so the reader-class leg would only retest the Current cover"
    );
    (witness, bytes)
}

#[test]
fn ze163_intermediate_tree_artifact_keeps_a_reader_witness() {
    let parent = super::tempfile::tempdir().expect("ze163 reader parent");
    let path = parent.path().join("native");
    let vfs = Arc::new(RecordingVfs::default());
    let document = ze163_document_tower();
    let (store, retained) = ze163_reader_history_fixture(&path, &vfs, &document);
    let (witness, witness_bytes) = ze163_reader_witness(&store, &path, &retained);

    // The lease stays held across the whole reclaim round: the point is that
    // this history is the reader's, not the writer's.
    ze163_survives_a_pending_reclaim_round(
        store,
        &path,
        &document,
        witness,
        &witness_bytes,
        Some(retained),
    );
}

/// The negative control for the reader-class leg. An independent store with
/// the same history has one intermediate-only artifact omitted from the
/// completed mark. The producer may accept it, because a reader bundle's
/// protected stream does not name that artifact; the next open over the
/// resulting manifest must not.
#[test]
fn ze163_omitted_reader_history_artifact_refuses_before_any_unlink() {
    let parent = super::tempfile::tempdir().expect("ze163 plant parent");
    let path = parent.path().join("native");
    let vfs = Arc::new(RecordingVfs::default());
    let document = ze163_document_tower();
    let (store, retained) = ze163_reader_history_fixture(&path, &vfs, &document);
    let (witness, witness_bytes) = ze163_reader_witness(&store, &path, &retained);
    let witness_path = crate::property_graph::storage::allocation::artifact_path(&path, witness);

    let before = directory_image(&path);
    let refusal = ze163_plant_omitted_mark(&store, &vfs, witness);
    match refusal {
        Some(error) => {
            ze163_assert_absent_from_mark_refusal(&error);
            assert_live_files_unchanged(&path, &before);
            drop(retained);
            store.close().expect("ze163 plant close");
        }
        None => {
            // The producer accepted the omission. The durable mark now lacks
            // a reference the retained reader history still names, so the
            // next open over it must refuse rather than reclaim anything.
            drop(retained);
            store.close().expect("ze163 plant close");
            match Store::open_native_graph(&path, options(), Some(document.clone())) {
                Ok(reopened) => {
                    reopened.close().expect("ze163 plant close reopened");
                    panic!(
                        "a mark missing a reader-history reference reopened \
                         without a refusal"
                    );
                }
                Err(error) => println!("ze163 reader reopen refusal: {error:?}"),
            }
        }
    }
    assert_eq!(
        std::fs::read(&witness_path).expect("ze163 plant witness survives"),
        witness_bytes
    );
}

/// One real reclaim cycle, a reopen and a replay of the original keyed
/// request, reported as plain observations for an independent comparator.
#[cfg(feature = "test-support")]
fn observe_reclaim_cycle() -> crate::graph_reclaim_test_support::ReclaimState {
    let (history, store) = seed_crash_history();
    let vfs = Arc::clone(&history.vfs);
    commit_maintenance(&store).expect("durable reclaim intent");
    let pending = store.admit_native_read().expect("pending reader");
    let candidates = pending_reclaim_candidates(&store, &pending);
    drop(pending);
    let unlinked_file_bytes = candidates
        .iter()
        .map(|candidate| {
            std::fs::metadata(reclaim_candidate_path(&history.path, candidate))
                .expect("pending target")
                .len()
        })
        .sum();
    vfs.take();
    let report = commit_maintenance(&store).expect("reclaim resume");
    assert_eq!(delete_events(&vfs.take()).len(), candidates.len());
    store.close().expect("close reclaimed store");
    let reopened = history.open(options()).expect("reopen reclaimed store");
    let lease = reopened.admit_native_read().expect("reopened reader");
    let relationships =
        super::publication::out_rows_for_lease(&reopened, &lease, history.first, history.peer)
            .into_iter()
            .map(
                |row| crate::graph_read_view_test_support::ObservedRelationship {
                    rel: row.rel.get(),
                    source: row.source.get(),
                    target: row.target.get(),
                    relationship_type: row.relationship_type.get(),
                },
            )
            .collect();
    drop(lease);
    let replay = apply_crash_history_writes(&reopened, &history.document);
    let state = crate::graph_reclaim_test_support::ReclaimState {
        first_node: history.first.get(),
        second_node: history.peer.get(),
        relationships,
        replay_generation: replay[0].generation.get(),
        replayed: replay.iter().all(|receipt| receipt.replayed),
        removed_bytes: report.removed_bytes,
        unlinked_file_bytes,
    };
    reopened.close().expect("close replayed store");
    state
}

/// The directed production paths behind ZE-46 acceptance. A receipt is pushed
/// only after its body returned, so a receipt proves its assertions passed.
#[cfg(feature = "test-support")]
pub(super) fn run_actual_probe(seed: u64) -> crate::graph_reclaim_test_support::ReclaimProbeReport {
    use super::publication::{reset_verified_faults, take_verified_faults};
    let bodies: [(&'static str, u64, fn()); 13] = [
        (
            "property-graph.reclaim.maintenance-output",
            1,
            run_ze260_maintenance_emits_each_tree_root_once_per_call,
        ),
        (
            "property-graph.reclaim.superseded-history",
            1,
            run_ze260_superseded_history_is_reclaimed_after_reader_drops,
        ),
        ("property-graph.reclaim.physical-replacement", 2, || {
            run_ze46_real_consolidation_preserves_exact_state_and_reopens();
            run_ze46_consolidation_rotates_through_every_node();
        }),
        (
            "property-graph.reclaim.stale-recheck",
            1,
            run_ze46_stale_preparation_rejects_without_publication_or_foreign_cleanup,
        ),
        ("property-graph.reclaim.inventory-fold", 2, || {
            run_ze46_inventory_fold_conserves_complete_allocation_union();
            run_ze46_inventory_fold_drains_a_manifest_backlog();
        }),
        (
            "property-graph.reclaim.protected-union",
            1,
            run_ze46_protected_union_keeps_partial_packs_and_actual_sparse_refs,
        ),
        (
            "property-graph.reclaim.wal-only",
            1,
            run_older_wal_manifest_retention_without_explicit_registration,
        ),
        (
            "property-graph.reclaim.spill-refusal",
            1,
            run_ze46_spill_merge_and_incomplete_mark_never_delete_candidates,
        ),
        ("property-graph.reclaim.orphan", 2, || {
            run_ze46_reconciles_real_prewal_orphans_without_touching_unknown_files();
            run_ze46_orphan_adoption_waits_for_a_quiescent_history();
        }),
        (
            "property-graph.reclaim.page-relocation",
            1,
            run_ze260_page_relocation_keeps_intent_before_unlink,
        ),
        ("property-graph.reclaim.intent-unlink-completion", 2, || {
            run_ze46_intent_unlink_sync_completion_crashes_resume_idempotently();
            run_ze46_checkpoint_during_an_open_reclaim_cycle_reopens();
        }),
        (
            "property-graph.reclaim.corrupt-proof",
            1,
            run_ze46_corrupt_or_incomplete_reclaim_proof_refuses_before_mutation,
        ),
        (
            "property-graph.reclaim.read-only-retirement",
            1,
            run_ze46_readonly_pending_reclaim_and_checkpoint_retirement_are_exact,
        ),
    ];
    let mut receipts = Vec::new();
    for (key, clean_controls, body) in bodies {
        reset_verified_faults();
        body();
        receipts.push(crate::graph_read_view_test_support::PathReceipt {
            key,
            fires: take_verified_faults(),
            clean_controls,
        });
    }
    receipts.extend(run_ze176_race_probe(seed).receipts);
    let detach_sweep = run_ze46_detach_sweeps_bounded_edges_and_preserves_fences();
    receipts.push(crate::graph_read_view_test_support::PathReceipt {
        key: "property-graph.reclaim.detach-sweep",
        fires: 0,
        clean_controls: 1,
    });
    crate::graph_reclaim_test_support::ReclaimProbeReport {
        receipts,
        state: observe_reclaim_cycle(),
        detach_sweep,
    }
}

#[test]
fn ze260_hundred_node_batches_write_bounded_pages() {
    use crate::property_graph::storage::artifact::{self, BlockKind, ContainerKind};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("store");
    let store = Store::create_native_graph(&path, options(), None).unwrap();
    let mut previous = directory_image(&path);
    for batch in 0..20 {
        let keys: Vec<_> = (0..100)
            .map(|n| format!("node-{:04}", batch * 100 + n))
            .collect();
        let edge = format!("edge-{batch}");
        let mut labels = [GraphName::new("Document").unwrap()];
        let mut properties = property_fixture();
        properties.truncate(1);
        let image = CanonicalContents::node(
            &mut labels,
            &mut properties,
            Some("bounded graph text"),
            None,
        )
        .unwrap();
        crate::property_graph::with_local_refs(|refs| {
            let mut requests: Vec<_> = keys
                .iter()
                .map(|key| StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "app", key).unwrap(),
                    revision: GraphRevision::new(1).unwrap(),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                })
                .collect();
            requests.push(StructuredWrite {
                key: ApplicationKey::new(EntityKind::Relationship, "app", &edge).unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Relationship {
                    source: NodeRef::Local(refs.node(0).unwrap()),
                    target: NodeRef::Local(refs.node(1).unwrap()),
                    relationship_type: GraphName::new("LINKS").unwrap(),
                    properties: &[],
                }),
            });
            store
                .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
                .unwrap();
        });
        let next = directory_image(&path);
        let mut pages = 0;
        let mut bytes = 0;
        let mut files = 0;
        for (name, data) in &next {
            if previous.contains_key(name) || !name.to_string_lossy().ends_with(".zgraph") {
                continue;
            }
            files += 1;
            bytes += data.len();
            let frame = artifact::decode(ContainerKind::Object, None, data).unwrap();
            let mut index = 0;
            while let Ok(reference) = frame.reference(index) {
                pages += usize::from(reference.kind == BlockKind::TreePage);
                index += 1;
            }
        }
        eprintln!(
            "ZE260 batch={} nodes=100 pages={pages} bytes={bytes} files={files}",
            batch + 1
        );
        assert!(
            pages <= 24,
            "batch {batch}: {pages} TreePage blocks, {bytes} bytes, {files} files"
        );
        assert!(bytes <= 512 * 1024, "batch {batch}: {bytes} bytes");
        previous = next;
    }
}

fn ze260_add_unlabelled_node(store: &Store, name: &str) {
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "s6a-garbage", name).unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap();
}

#[test]
fn ze260_maintenance_copies_an_unchanged_membership_page() {
    let parent = super::tempfile::tempdir().unwrap();
    let path = parent.path().join("native");
    let store = Store::create_native_graph(&path, options(), None).unwrap();
    let mut labels = [GraphName::new("AppendOnly").unwrap()];
    let image = CanonicalContents::node(&mut labels, &mut [], None, None).unwrap();
    let key = "unchanged".repeat(64);
    let writes = [StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "pages", &key).unwrap(),
        revision: GraphRevision::new(1).unwrap(),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Node(&image)),
    }];
    let control = QueryControl::Cancel(CancelToken::new());
    let original = store.apply_native_graph(&writes, &control).unwrap();
    let before = store
        .admit_native_read()
        .unwrap()
        .bundle()
        .roots()
        .directory(TreeKind::Labels)
        .unwrap()
        .reference();
    ze260_add_unlabelled_node(&store, "membership");
    commit_maintenance(&store).unwrap();
    let after = store
        .admit_native_read()
        .unwrap()
        .bundle()
        .roots()
        .directory(TreeKind::Labels)
        .unwrap()
        .reference();
    assert_ne!(
        before, after,
        "unchanged membership page must leave the old pack"
    );
    store.close().unwrap();
    let reopened = Store::open_native_graph(&path, options(), None).unwrap();
    let replay = reopened.apply_native_graph(&writes, &control).unwrap();
    assert!(replay[0].replayed);
    assert_eq!(replay[0].generation, original[0].generation);
    reopened.close().unwrap();
}

#[test]
fn ze260_maintenance_retargets_oldest_fence_payloads() {
    use crate::property_graph::storage::{NativePreparationCatalog, records::verify_fence_entry};
    let parent = super::tempfile::tempdir().unwrap();
    let store = Store::create_native_graph(parent.path().join("native"), options(), None).unwrap();
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "pages", "payload").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap();
    let lease = store.admit_native_read().unwrap();
    let oldest = prepared_union_for_lease(&store, &lease)
        .into_iter()
        .min_by_key(|change| change.object.serial)
        .unwrap()
        .object
        .artifact;
    drop(lease);
    ze260_add_unlabelled_node(&store, "fence");
    commit_maintenance(&store).unwrap();
    let lease = store.admit_native_read().unwrap();
    let shared = crate::property_graph::resources::GraphResources::from_store(&store).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
    let source = NativePreparationSource::new(&lease, &memory, 64).unwrap();
    let mut resources = source.resources(128 * 1024 * 1024).unwrap();
    let catalog = NativePreparationCatalog::open(&source, &mut resources).unwrap();
    let root = lease
        .bundle()
        .roots()
        .directory(TreeKind::KeyFences)
        .unwrap();
    let mut cursor = DirectoryCursor::seek(&source, root, None, &mut resources).unwrap();
    let mut count = 0;
    while let Some(entry) = cursor.next_entry(&mut resources).unwrap() {
        let fence =
            verify_fence_entry(&source, root, entry, &catalog, None, &mut resources).unwrap();
        let (provenance, canonical) = fence.required_payloads();
        assert_ne!(
            provenance.reference().artifact,
            oldest,
            "fence provenance still pins oldest pack"
        );
        assert_ne!(
            canonical.unwrap().reference().artifact,
            oldest,
            "fence canonical still pins oldest pack"
        );
        count += 1;
    }
    assert_eq!(count, 2);
}

#[test]
fn ze260_page_relocation_keeps_intent_before_unlink() {
    run_ze260_page_relocation_keeps_intent_before_unlink();
}

fn run_ze260_page_relocation_keeps_intent_before_unlink() {
    use super::publication::FaultPoint;
    let parent = super::tempfile::tempdir().unwrap();
    let path = parent.path().join("native");
    let vfs = Arc::new(RecordingVfs::default());
    let store = create_reclaim_test_store(&path, &vfs);
    let mut labels = [GraphName::new("AppendOnly").unwrap()];
    let image = CanonicalContents::node(&mut labels, &mut [], None, None).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "pages", "fault").unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &control,
        )
        .unwrap();
    commit_maintenance(&store).unwrap();
    store.checkpoint_native_graph(&control).unwrap();
    let before = store
        .admit_native_read()
        .unwrap()
        .bundle()
        .roots()
        .directory(TreeKind::Labels)
        .unwrap()
        .reference();
    ze260_add_unlabelled_node(&store, "intent");
    vfs.take();
    vfs.arm_fault(FaultPoint::Delete);
    let intent = commit_maintenance(&store).unwrap();
    assert_eq!(intent.removed_bytes, 0);
    assert!(
        delete_events(&vfs.take()).is_empty(),
        "unlink before durable intent"
    );
    let pending = store.admit_native_read().unwrap();
    assert!(pending.bundle().reclaim().is_some());
    assert_ne!(
        before,
        pending
            .bundle()
            .roots()
            .directory(TreeKind::Labels)
            .unwrap()
            .reference()
    );
    drop(pending);
    commit_maintenance(&store).expect_err("scheduled Delete must fire");
    vfs.assert_fired_once();
    assert!(delete_events(&vfs.take()).is_empty());
    store.close().unwrap();
    let infrastructure: Arc<dyn Vfs> = vfs.clone();
    let reopened = Store::open_native_graph_with_infrastructure(
        &path,
        options(),
        None,
        infrastructure,
        Arc::new(crate::lifecycle::SystemMonotonicClock),
    )
    .unwrap();
    assert!(
        !delete_events(&vfs.take()).is_empty(),
        "writable recovery must resume the authorized unlinks"
    );
    reopened
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .unwrap();
    commit_maintenance(&reopened).unwrap();
    reopened.close().unwrap();
}

#[test]
fn ze260_duplicate_heavy_mark_uses_one_run() {
    let parent = super::tempfile::tempdir().unwrap();
    let store = Store::create_native_graph(parent.path().join("native"), options(), None).unwrap();
    let admission = store.admit_native_graph_maintenance().unwrap();
    let input: Vec<u128> = (0..24).flat_map(|_| 1..=65).collect();
    let report =
        super::super::maintenance::run_spill_probe(&store, &admission, &input, 256).unwrap();
    assert_eq!(report.ordered, (1..=65).collect::<Vec<_>>());
    assert_eq!(report.spill_runs, 1);
    assert_eq!(report.merges, 0);
    assert_eq!(report.created_objects, 2);
}

#[test]
fn ze260_superseded_history_is_reclaimed_after_reader_drops() {
    run_ze260_superseded_history_is_reclaimed_after_reader_drops();
}

fn run_ze260_superseded_history_is_reclaimed_after_reader_drops() {
    let parent = super::tempfile::tempdir().unwrap();
    let path = parent.path().join("native");
    let vfs = Arc::new(RecordingVfs::default());
    let store = create_reclaim_test_store(&path, &vfs);
    ze163_base_write(&store, "history", 0);
    let old = store.admit_native_read().unwrap();
    let root = old.bundle().root_envelope().object;
    let wal_path = std::fs::read_dir(&path)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("graph-wal-")
        })
        .unwrap();
    let wal_identity = u128::from_str_radix(
        wal_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .strip_prefix("graph-wal-")
            .unwrap()
            .strip_suffix(".ze")
            .unwrap(),
        16,
    )
    .unwrap();
    let root_path = crate::property_graph::storage::allocation::artifact_path(&path, root.artifact);
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .unwrap();
    for _ in 0..3 {
        commit_maintenance(&store).unwrap();
        let lease = store.admit_native_read().unwrap();
        let candidates = pending_reclaim_candidates_or_empty_root(&store, &lease);
        assert!(
            !candidates
                .iter()
                .any(|c| c.artifact == root.artifact || c.artifact.get() == wal_identity)
        );
    }
    assert!(root_path.exists());
    assert!(wal_path.exists());
    drop(old);
    let mut selected = None;
    for _ in 0..6 {
        store
            .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
            .unwrap();
        vfs.take();
        commit_maintenance(&store).unwrap();
        let lease = store.admit_native_read().unwrap();
        let candidates = pending_reclaim_candidates_or_empty_root(&store, &lease);
        if let Some(index) = candidates
            .iter()
            .position(|c| c.family == 19 && c.artifact.get() == wal_identity)
        {
            assert!(
                candidates
                    .iter()
                    .any(|c| c.family == 18 && c.artifact == root.artifact)
            );
            assert!(
                delete_events(&vfs.take()).is_empty(),
                "history unlink before durable intent"
            );
            selected = Some(index);
            break;
        }
    }
    let wal_index = selected.expect("superseded root and WAL were never intent candidates");
    // The exact-length/digest validator must refuse a changed WAL, then the
    // same durable intent must resume after the original bytes are restored.
    let original = std::fs::read(&wal_path).unwrap();
    let mut changed = original.clone();
    *changed.last_mut().unwrap() ^= 1;
    std::fs::write(&wal_path, &changed).unwrap();
    let error = commit_maintenance(&store).expect_err("changed WAL digest must refuse unlink");
    assert!(
        matches!(
            error,
            super::super::NativeGraphError::Read(
                crate::property_graph::storage::tree::directory::TreeError::Invalid(
                    "reclaim WAL digest"
                )
            )
        ),
        "{error:?}"
    );
    assert!(wal_path.exists());
    std::fs::write(&wal_path, &original).unwrap();
    // Earlier candidates may already have been unlinked by the failed resume.
    let lease = store.admit_native_read().unwrap();
    let candidates = pending_reclaim_candidates(&store, &lease);
    let skip = candidates
        .iter()
        .take(wal_index)
        .filter(|c| {
            if c.family == 19 {
                path.join(format!("graph-wal-{:032x}.ze", c.artifact.get()))
                    .exists()
            } else {
                crate::property_graph::storage::allocation::artifact_path(&path, c.artifact)
                    .exists()
            }
        })
        .count();
    drop(lease);
    vfs.take();
    vfs.arm_fault_after(super::publication::FaultPoint::Delete, skip as u64);
    let error = commit_maintenance(&store).expect_err("scheduled WAL Delete refusal");
    assert!(
        matches!(&error, super::super::NativeGraphError::Io { path, .. } if path == &wal_path),
        "{error:?}"
    );
    vfs.assert_fired_once();
    assert!(wal_path.exists(), "the refused WAL must remain");
    store.close().unwrap();
    let reopened = Store::open_native_graph(&path, options(), None).unwrap();
    assert!(!root_path.exists(), "superseded root was never reclaimed");
    assert!(!wal_path.exists(), "superseded WAL was never reclaimed");
    // Exact replay still resolves the original revision and canonical image.
    ze163_base_write(&reopened, "history", 0);
    reopened.close().unwrap();
}

// Retain a real nonempty admission and its readable publication while
// reproducing close's drained writer at each maintenance branch.
fn ze201_retained_maintenance_drained_writer(phase: u8) {
    for state in [
        crate::lifecycle::StoreState::Closing,
        crate::lifecycle::StoreState::Closed,
    ] {
        let (_history, store) = seed_crash_history();
        let control = QueryControl::Cancel(CancelToken::new());
        if phase >= 1 {
            commit_maintenance(&store).expect("pending intent");
        }
        if phase >= 2 {
            commit_maintenance(&store).expect("completion");
        }
        if phase >= 3 {
            assert!(matches!(
                commit_maintenance(&store),
                Err(super::super::NativeGraphError::StalePreparation)
            ));
        }
        let admission = store
            .admit_native_graph_maintenance()
            .expect("retain admission");
        let before = Arc::clone(admission.lease.bundle());
        assert!(before.sequence() > 0);
        assert_eq!(before.reclaim().is_some(), phase > 0);
        if phase >= 2 {
            assert_eq!(
                before.root_envelope().object.generation == before.base().generation,
                phase == 3
            );
        }
        let error = if phase == 0 {
            let entered = Arc::new(std::sync::Barrier::new(2));
            let release = Arc::new(std::sync::Barrier::new(2));
            *store.state.lock().expect("state") = crate::lifecycle::StoreState::Open;
            store
                .native_graph
                .state
                .lock()
                .expect("publication")
                .maintenance_writer_hook = Some((Arc::clone(&entered), Arc::clone(&release)));
            std::thread::scope(|scope| {
                let attempt =
                    scope.spawn(|| store.commit_native_graph_maintenance(&admission, &control));
                entered.wait();
                store
                    .native_graph
                    .drain_writer_for_close()
                    .expect("drain writer");
                *store.state.lock().expect("state") = state;
                release.wait();
                attempt.join().expect("join").expect_err("drained writer")
            })
        } else {
            store
                .native_graph
                .drain_writer_for_close()
                .expect("drain writer");
            *store.state.lock().expect("state") = state;
            store
                .commit_native_graph_maintenance(&admission, &control)
                .expect_err("drained writer")
        };
        assert!(
            matches!(
                (&error, state),
                (
                    super::super::NativeGraphError::Store(crate::lifecycle::StoreError::Closing),
                    crate::lifecycle::StoreState::Closing
                ) | (
                    super::super::NativeGraphError::Store(crate::lifecycle::StoreError::Closed),
                    crate::lifecycle::StoreState::Closed
                )
            ),
            "{error:?}"
        );
        assert!(
            store
                .native_graph
                .is_current_bundle(&before)
                .expect("publication")
        );
        *store.state.lock().expect("state") = crate::lifecycle::StoreState::Open;
        drop(admission);
        store.close().expect("cleanup");
    }
}

#[test]
fn ze201_retained_maintenance_commit_is_closed() {
    ze201_retained_maintenance_drained_writer(0);
}
#[test]
fn ze201_pending_reclaim_completion_is_closed() {
    ze201_retained_maintenance_drained_writer(1);
}
#[test]
fn ze201_retirement_checkpoint_is_closed() {
    ze201_retained_maintenance_drained_writer(2);
}
#[test]
fn ze201_retirement_clear_is_closed() {
    ze201_retained_maintenance_drained_writer(3);
}

#[test]
fn ze188_retirement_reopens_after_trailing_checkpoint_failure() {
    use super::publication::FaultPoint;
    let (history, store) = seed_crash_history();
    let oracle = history.oracle(&store);
    commit_maintenance(&store).expect("publish reclaim intent");
    history.vfs.take();
    let completed = commit_maintenance(&store).expect("complete reclaim");
    assert!(completed.removed_bytes > 0);
    let deleted = delete_events(&history.vfs.take());
    assert!(!deleted.is_empty());
    assert!(deleted.iter().all(|path| !path.exists()));
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .expect("checkpoint completed reclaim");
    let before = directory_image(&history.path);
    history.vfs.arm_fault(FaultPoint::OpenAppend);
    commit_maintenance(&store).expect_err("trailing checkpoint must fail");
    history.vfs.assert_fired_once();
    let lease = store.admit_native_read().expect("cleared state");
    assert!(lease.bundle().reclaim().is_none());
    drop(lease);
    assert_same_logical_state(&oracle, &history.oracle(&store));
    let after = directory_image(&history.path);
    let selector = std::ffi::OsStr::new("graph-root.ze");
    assert_eq!(after.get(selector), before.get(selector));
    drop(store);
    let readonly = history
        .open(
            OpenOptions::read_only()
                .with_durability(DurabilityMode::Durable, CommitTier::Durable)
                .with_max_resident_bytes(256 * 1024 * 1024),
        )
        .expect("read-only reopen after interrupted retirement");
    assert_same_logical_state(&oracle, &history.oracle(&readonly));
    readonly.close().expect("close read-only");
    let writable = history
        .open(options())
        .expect("writable reopen after interrupted retirement");
    assert_same_logical_state(&oracle, &history.oracle(&writable));
    writable.close().expect("close writable");
}

fn graph_references_for_lease(
    store: &Store,
    lease: &super::super::NativeReadLease,
) -> Vec<crate::property_graph::storage::artifact::PhysicalRef> {
    let shared =
        crate::property_graph::resources::GraphResources::from_store(store).expect("resources");
    let control = QueryControl::Cancel(CancelToken::new());
    let writer = WriteMemory::new(&shared, WriteLimits::default()).expect("memory");
    let memory = StorageMemory::new(&writer, &control, 8 * 1024 * 1024).expect("storage");
    let source = NativePreparationSource::new(lease, &memory, 64).expect("source");
    let mut resources = source.resources(64 * 1024 * 1024).expect("work");
    let catalog =
        crate::property_graph::storage::NativePreparationCatalog::open(&source, &mut resources)
            .expect("catalog");
    let mut scratch = crate::property_graph::storage::adjacency::RangeScratch::for_prepare(
        &memory,
        &mut resources,
    )
    .expect("scratch");
    let mut references = Vec::new();
    crate::property_graph::storage::reclaim::trace_graph_state(
        &source, &catalog, lease.bundle().roots(), lease.bundle().sequence(),
        lease.bundle().document(), &mut scratch,
        &mut |reference, _: &mut crate::property_graph::storage::tree::directory::TreeResources<'_>| {
            references.push(reference);
            Ok(())
        }, &mut resources,
    ).expect("graph trace");
    references
}

fn sparse_references_for_lease(
    store: &Store,
    lease: &super::super::NativeReadLease,
) -> Vec<crate::property_graph::storage::artifact::PhysicalRef> {
    let shared =
        crate::property_graph::resources::GraphResources::from_store(store).expect("resources");
    let control = QueryControl::Cancel(CancelToken::new());
    let writer = WriteMemory::new(&shared, WriteLimits::default()).expect("memory");
    let memory = StorageMemory::new(&writer, &control, 8 * 1024 * 1024).expect("storage");
    let source = NativePreparationSource::new(lease, &memory, 64).expect("source");
    let mut resources = source.resources(64 * 1024 * 1024).expect("work");
    let catalog =
        crate::property_graph::storage::NativePreparationCatalog::open(&source, &mut resources)
            .expect("catalog");
    let mut state = crate::property_graph::storage::search::SearchTraceCursor::for_preparation(
        &source,
        &catalog,
        &mut resources,
    )
    .expect("sparse trace")
    .into_state();
    let mut output = [None; crate::property_graph::storage::reclaim::TRACE_OUTPUT_LIMIT];
    let mut references = Vec::new();
    loop {
        let result = state
            .trace_preparation(&source, &catalog, &mut output, &mut resources)
            .expect("sparse references");
        references.extend(output.iter().take(result.count).flatten().copied());
        if result.complete {
            break;
        }
    }
    references
}

#[test]
fn ze189_inventory_only_artifact_is_required_on_reopen() {
    use super::publication::FaultPoint;
    let parent = super::tempfile::tempdir().expect("parent");
    let path = parent.path().join("native");
    let vfs = Arc::new(RecordingVfs::default());
    let store = create_reclaim_test_store(&path, &vfs);
    ze163_base_write(&store, "ze189", 0);
    let before = directory_image(&path);
    vfs.arm_fault(FaultPoint::ObjectSync);
    let image = CanonicalContents::node(&mut [], &mut [], None, None).expect("node");
    assert!(
        store
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "ze189", "orphan").expect("key"),
                    revision: GraphRevision::new(1).expect("revision"),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                }],
                &QueryControl::Cancel(CancelToken::new())
            )
            .is_err(),
        "complete pre-WAL orphan fault"
    );
    vfs.assert_fired_once();
    let orphans: Vec<_> = directory_image(&path)
        .into_iter()
        .filter(|(name, _)| {
            !before.contains_key(name) && name.to_string_lossy().ends_with(".zgraph")
        })
        .collect();
    assert!(!orphans.is_empty());
    ze163_base_write(&store, "ze189", 1);
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .expect("advance durable fences before adoption");
    let checkpoint = store.admit_native_read().expect("checkpoint inventory");
    let checkpoint_inventory = complete_inventory_union_for_lease(&store, &checkpoint);
    drop(checkpoint);
    let selector = std::fs::read(path.join("graph-root.ze")).expect("selector");
    commit_maintenance(&store).expect("adopt orphan without checkpoint");
    assert_eq!(
        std::fs::read(path.join("graph-root.ze")).expect("selector after adoption"),
        selector
    );
    let lease = store.admit_native_read().expect("adopted inventory");
    let inventory = rooted_inventory_for_lease(&store, &lease);
    let victim = inventory
        .iter()
        .find(|change| {
            change.state == InventoryState::Retained
                && orphans.iter().any(|(name, _)| {
                    crate::property_graph::storage::allocation::artifact_path(
                        &path,
                        change.object.artifact,
                    ) == path.join(name)
                })
        })
        .expect("complete orphan adopted as Retained")
        .object;
    assert!(
        !checkpoint_inventory
            .iter()
            .any(|change| change.object == victim)
    );
    assert!(
        !graph_references_for_lease(&store, &lease)
            .iter()
            .any(|reference| reference.artifact == victim.artifact)
    );
    assert!(
        !sparse_references_for_lease(&store, &lease)
            .iter()
            .any(|reference| reference.artifact == victim.artifact)
    );
    if lease.bundle().reclaim().is_some() {
        assert!(!pending_reclaim_candidates(&store, &lease).contains(&victim));
    }
    let generation = lease.bundle().base().generation;
    drop(lease);
    drop(store);
    let intact = Store::open_native_graph(
        &path,
        OpenOptions::read_only()
            .with_durability(DurabilityMode::Durable, CommitTier::Durable)
            .with_max_resident_bytes(256 * 1024 * 1024),
        None,
    )
    .expect("intact orphan reopens");
    let lease = intact.admit_native_read().expect("intact generation");
    assert_eq!(lease.bundle().base().generation, generation);
    drop(lease);
    intact.close().expect("close intact read-only");
    let victim_path =
        crate::property_graph::storage::allocation::artifact_path(&path, victim.artifact);
    std::fs::remove_file(&victim_path).expect("remove inventory-only artifact");
    let Err(error) = Store::open_native_graph(&path, options(), None) else {
        panic!("inventory-only missing artifact incorrectly reopened");
    };
    assert!(
        matches!(&error, super::super::NativeGraphError::Io { path: missing, source }
        if missing == &victim_path && source.kind() == std::io::ErrorKind::NotFound),
        "{error:?}"
    );
}

#[test]
fn ze186_reader_graph_only_artifact_survives_reclaim() {
    let parent = tempfile::tempdir().expect("parent");
    let path = parent.path().join("native");
    let vfs = Arc::new(RecordingVfs::default());
    let store = create_reclaim_test_store(&path, &vfs);
    let control = QueryControl::Cancel(CancelToken::new());
    let image = CanonicalContents::node(&mut [], &mut [], None, None).expect("node");
    let names: Vec<_> = (0..64).map(|n| format!("node-{n:04}")).collect();
    let mut nodes = Vec::new();
    for batch in names.chunks(8) {
        let writes: Vec<_> = batch
            .iter()
            .map(|name| StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze186", name).expect("key"),
                revision: GraphRevision::new(1).expect("revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            })
            .collect();
        let receipts = store.apply_native_graph(&writes, &control).expect("seed");
        nodes.extend(receipts.iter().copied());
    }
    store
        .checkpoint_native_graph(&control)
        .expect("reader checkpoint");
    let reader = store.admit_native_read().expect("retained reader");
    let old = graph_references_for_lease(&store, &reader);
    let EntityId::Node(node) = nodes[0].entity else {
        panic!("node")
    };
    let lazy_value = node_directory_value(&store, &reader, node);
    for (batch, receipts) in names.chunks(8).zip(nodes.chunks(8)) {
        let writes: Vec<_> = batch
            .iter()
            .zip(receipts)
            .map(|(name, receipt)| StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze186", name).expect("key"),
                revision: GraphRevision::new(2).expect("revision"),
                operation: StructuredOperation::Put(receipt.entity),
                image: Some(WriteImage::Node(&image)),
            })
            .collect();
        store
            .apply_native_graph(&writes, &control)
            .expect("replace");
    }
    store
        .checkpoint_native_graph(&control)
        .expect("current checkpoint");
    let current = store.admit_native_read().expect("current");
    {
        let capture = store.capture_native_read_roots().expect("registrations");
        assert_eq!(capture.bundles().len(), 2);
        assert!(capture.prepared().is_empty());
    }
    let mut other: Vec<_> = graph_references_for_lease(&store, &current)
        .iter()
        .map(|r| r.artifact)
        .collect();
    for lease in [&reader, &current] {
        other.extend(
            sparse_references_for_lease(&store, lease)
                .iter()
                .map(|r| r.artifact),
        );
        other.extend(
            std::iter::once(lease.bundle().root_envelope())
                .chain(lease.bundle().wal_roots().slots.into_iter().flatten())
                .chain(
                    [
                        Some(lease.bundle().catalog()),
                        lease.bundle().text(),
                        lease.bundle().vector(),
                        lease.bundle().reclaim(),
                    ]
                    .into_iter()
                    .flatten(),
                )
                .chain(lease.bundle().prepared_inventories().iter().copied())
                .map(|r| r.object.artifact),
        );
    }
    // At each target checkpoint, history emits only its WAL identity:
    // the complete graph retrace is skipped at the target sequence.
    for lease in [&reader, &current] {
        let mut checkpoints = 0;
        super::super::recovery::visit_captured_state(
            &store,
            &path,
            lease.bundle().root_envelope(),
            lease.bundle().sequence(),
            None,
            &control,
            |visit, _| {
                match visit {
                    super::super::recovery::CapturedStateVisit::Checkpoint {
                        state,
                        wal_identity,
                        ..
                    } => {
                        assert_eq!(state.sequence, lease.bundle().sequence());
                        checkpoints += 1;
                        other.push(
                            crate::property_graph::storage::artifact::ArtifactId::new(wal_identity)
                                .expect("WAL identity"),
                        );
                    }
                    super::super::recovery::CapturedStateVisit::Envelope { .. } => {
                        panic!("unexpected history envelope")
                    }
                }
                Ok(())
            },
        )
        .expect("history isolation");
        assert_eq!(checkpoints, 1);
    }
    let mut witnesses: Vec<_> = old
        .iter()
        .filter(|r| !other.contains(&r.artifact))
        .map(|r| r.artifact)
        .collect();
    witnesses.sort_unstable();
    witnesses.dedup();
    let witness_bytes: Vec<_> = witnesses
        .iter()
        .map(|artifact| {
            let file = crate::property_graph::storage::allocation::artifact_path(&path, *artifact);
            let bytes = std::fs::read(&file).expect("witness bytes");
            (file, bytes)
        })
        .collect();
    assert!(
        !witnesses.is_empty(),
        "no Reader-only pack among {} graph references",
        old.len()
    );
    // The lighter witness is an old node-record pack. Its directory
    // reference is indirect; no direct root or sparse trace protects it.
    eprintln!("ZE186 witness packs: {}", witnesses.len());
    let before = directory_image(&path);
    drop(current);
    vfs.take();
    let mut removed = 0;
    for _ in 0..2 {
        removed += commit_maintenance(&store)
            .expect("reclaim under reader")
            .removed_bytes;
    }
    assert!(removed > 0, "reclaim must unlink unrelated packs");
    let proof = store.admit_native_read().expect("proof");
    let (_, mark) = durable_proof_for_lease(&store, &proof);
    for witness in witnesses {
        assert!(
            mark.contains(&witness),
            "Reader-only witness absent from mark: {witness:?}"
        );
    }
    for (file, bytes) in witness_bytes {
        assert_eq!(std::fs::read(file).expect("surviving witness"), bytes);
    }
    for deleted in delete_events(&vfs.take()) {
        assert!(!deleted.exists());
        assert!(before.contains_key(deleted.file_name().expect("filename")));
    }
    assert_eq!(node_directory_value(&store, &reader, node), lazy_value);
    assert_eq!(graph_references_for_lease(&store, &reader), old);
    eprintln!("ZE186 removed bytes: {removed}");
}

#[cfg(feature = "test-support")]
fn ze176_schedule(seed: u64, raced: bool) -> ((u64, Vec<u8>, bool, bool), [u64; 4]) {
    use crate::property_graph::query::resources::QueryMemory;
    use crate::property_graph::query::runtime::{RuntimeContext, RuntimeLimits};
    use crate::property_graph::storage::tree::directory::TreeResources;
    use crate::property_graph::storage::{
        GraphReadView, NativeCatalog, NativeQuerySource, NativeReadCapability,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    let parent = super::tempfile::tempdir().unwrap();
    let path = parent.path().join("native");
    let vfs = Arc::new(RecordingVfs::default());
    let store = Arc::new(create_reclaim_test_store(&path, &vfs));
    let index = (seed % 1000) as usize;
    ze163_base_write(&store, "ze176-old", index);
    let mut fires = [0; 4];
    let old = if raced {
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        store.native_graph.state.lock().unwrap().admission_hook =
            Some((entered.clone(), release.clone()));
        std::thread::scope(|scope| {
            let reader = scope.spawn(|| store.admit_native_read().unwrap());
            entered.wait();
            let capture = scope.spawn(|| store.capture_native_read_roots().unwrap());
            release.wait();
            let old = reader.join().unwrap();
            let captured = capture.join().unwrap();
            assert!(captured.contains_lease(&old));
            fires[0] += 1;
            old
        })
    } else {
        let old = store.admit_native_read().unwrap();
        assert!(
            store
                .capture_native_read_roots()
                .unwrap()
                .contains_lease(&old)
        );
        fires[0] += 1;
        old
    };
    let root = old.bundle().root_envelope();
    let root_path =
        crate::property_graph::storage::allocation::artifact_path(&path, root.object.artifact);
    let wal_path = std::fs::read_dir(&path)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("graph-wal-")
        })
        .unwrap();

    // Publish after the first private preparation artifact; force the real
    // writer recheck, and prove that this stale attempt cannot unlink.
    let creates = Arc::new(AtomicU64::new(0));
    let counter = creates.clone();
    if raced {
        let admission = store.admit_native_graph_maintenance().unwrap();
        let foreground = store.clone();
        vfs.after_next_create(move || {
            counter.fetch_add(1, Ordering::Relaxed);
            ze163_base_write(&foreground, "ze176-new", index);
        });
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        store
            .native_graph
            .state
            .lock()
            .unwrap()
            .maintenance_writer_hook = Some((entered.clone(), release.clone()));
        vfs.take();
        let error = std::thread::scope(|scope| {
            let attempt = scope.spawn(|| {
                store.commit_native_graph_maintenance(
                    &admission,
                    &QueryControl::Cancel(CancelToken::new()),
                )
            });
            entered.wait();
            release.wait();
            attempt
                .join()
                .unwrap()
                .expect_err("publication must invalidate preparation")
        });
        assert!(
            matches!(error, super::super::NativeGraphError::StalePreparation),
            "{error:?}"
        );
        assert_eq!(creates.load(Ordering::Relaxed), 1);
        assert!(
            delete_events(&vfs.take()).is_empty(),
            "stale attempt deleted an artifact"
        );
        fires[1] += 1;
    } else {
        ze163_base_write(&store, "ze176-new", index);
        vfs.after_next_create(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        });
        commit_maintenance(&store).unwrap();
        assert_eq!(creates.load(Ordering::Relaxed), 1);
        fires[1] += 1;
    }
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .unwrap();
    for _ in 0..3 {
        // Completed reclaim retirement requires a checkpoint at its generation.
        store
            .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
            .unwrap();
        commit_maintenance(&store).unwrap();
        let lease = store.admit_native_read().unwrap();
        assert!(
            !pending_reclaim_candidates_or_empty_root(&store, &lease)
                .iter()
                .any(|c| c.artifact == root.object.artifact)
        );
        assert!(root_path.exists() && wal_path.exists());
    }

    // No source has ever been constructed for `old`. Resolve its exact
    // node-tree block for the first time after maintenance, then read content.
    let generation = old.bundle().base().generation.get();
    let canonical = {
        let shared = crate::property_graph::resources::GraphResources::from_store(&store).unwrap();
        let memory = QueryMemory::new(&shared, 8 * 1024 * 1024).unwrap();
        let control = QueryControl::Cancel(CancelToken::new());
        let mut runtime =
            RuntimeContext::new(&old, &control, &memory, RuntimeLimits::default()).unwrap();
        let capability = NativeReadCapability::admit(&old, &runtime).unwrap();
        let mut resources = TreeResources::for_query(&mut runtime).unwrap();
        let source = NativeQuerySource::new(capability, &resources, 16).unwrap();
        let before = store.stats().unwrap().mapped_bytes;
        source
            .resolve(
                old.bundle().roots().references()[0].unwrap(),
                &mut resources,
            )
            .unwrap();
        assert!(
            store.stats().unwrap().mapped_bytes > before,
            "old root did not lazily map"
        );
        let catalog = NativeCatalog::open(&source, &mut resources).unwrap();
        drop(resources);
        let view = GraphReadView::new(&source, &catalog).unwrap();
        let mut resources = TreeResources::for_query(&mut runtime).unwrap();
        let node = crate::property_graph::NodeId::new(1).unwrap();
        let record = view.lookup_node(node, &mut resources).unwrap().unwrap();
        let mut canonical = vec![0; record.record().canonical_bytes().len() as usize];
        assert_eq!(
            record
                .record()
                .canonical_bytes()
                .read_at(0, &mut canonical, &mut resources)
                .unwrap(),
            canonical.len()
        );
        let text = view.stored_text(node, &mut resources).unwrap().unwrap();
        let mut bytes = vec![0; text.len() as usize];
        assert_eq!(
            text.read_at(0, &mut bytes, &mut resources).unwrap(),
            bytes.len()
        );
        assert_eq!(bytes, format!("ze163 ze176-old row {index}").as_bytes());
        assert_eq!(generation, 1);
        fires[2] += 1;
        canonical
    };
    drop(old);

    // Reuse ZE-260's bounded retirement fixture, without its unrelated
    // digest corruption and Delete-fault qualification.
    let mut selected = false;
    for _ in 0..6 {
        store
            .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
            .unwrap();
        vfs.take();
        commit_maintenance(&store).unwrap();
        let lease = store.admit_native_read().unwrap();
        let candidates = pending_reclaim_candidates_or_empty_root(&store, &lease);
        if candidates
            .iter()
            .any(|c| reclaim_candidate_path(&path, c) == wal_path)
        {
            assert!(
                candidates
                    .iter()
                    .any(|c| c.artifact == root.object.artifact)
            );
            assert!(
                delete_events(&vfs.take()).is_empty(),
                "unlink before intent"
            );
            selected = true;
            break;
        }
    }
    assert!(selected, "old root/WAL never became reclaim candidates");
    vfs.take();
    commit_maintenance(&store).unwrap();
    let deleted = delete_events(&vfs.take());
    assert!(deleted.contains(&root_path) && deleted.contains(&wal_path));
    assert!(!root_path.exists() && !wal_path.exists());
    fires[3] += 1;
    Arc::try_unwrap(store)
        .unwrap_or_else(|_| panic!("race store leaked"))
        .close()
        .unwrap();
    (
        (
            generation,
            canonical,
            !root_path.exists(),
            !wal_path.exists(),
        ),
        fires,
    )
}

#[cfg(feature = "test-support")]
pub(super) fn run_ze176_race_probe(
    seed: u64,
) -> crate::graph_reclaim_test_support::RaceProbeReport {
    let (observation, fires) = ze176_schedule(seed, true);
    let (control, controls) = ze176_schedule(seed, false);
    assert_eq!(observation, control, "same-seed serialized race control");
    let keys = [
        "property-graph.reclaim.capture-race",
        "property-graph.reclaim.publication-race",
        "property-graph.reclaim.lazy-after-sweep",
        "property-graph.reclaim.release-unlink",
    ];
    crate::graph_reclaim_test_support::RaceProbeReport {
        receipts: keys
            .into_iter()
            .zip(fires)
            .zip(controls)
            .map(|((key, fires), clean_controls)| {
                crate::graph_read_view_test_support::PathReceipt {
                    key,
                    fires,
                    clean_controls,
                }
            })
            .collect(),
        observation,
        control,
    }
}

#[cfg(feature = "test-support")]
#[test]
fn ze176_race_probe_requires_measured_controls() {
    let report = run_ze176_race_probe(7);
    for key in [
        "capture-race",
        "publication-race",
        "lazy-after-sweep",
        "release-unlink",
    ] {
        let receipt = report
            .receipts
            .iter()
            .find(|r| r.key.strip_prefix("property-graph.reclaim.") == Some(key))
            .expect("missing measured race receipt");
        assert_eq!(receipt.fires, 1, "{key}");
        assert_eq!(receipt.clean_controls, 1, "{key}");
    }
    assert_eq!(report.observation, report.control);
    assert_eq!(report.observation.0, 1);
    assert!(report.observation.2 && report.observation.3);
}

/// Reuse the existing real reclaim partitions, each beside a fresh clean control.
#[cfg(feature = "test-support")]
pub(crate) fn run_ze41_reclaim_boundaries() -> Vec<crate::graph_read_view_test_support::PathReceipt>
{
    use super::publication::{reset_verified_faults, take_verified_faults};
    [
        (
            "property-graph.recovery.commit.reclaim-unlink",
            CrashCell::BeforeFirstUnlink,
        ),
        (
            "property-graph.recovery.commit.reclaim-sync",
            CrashCell::DirectorySync,
        ),
        (
            "property-graph.recovery.commit.reclaim-completion",
            CrashCell::LostCompletionAck,
        ),
    ]
    .into_iter()
    .map(|(key, cell)| {
        reset_verified_faults();
        run_reclaim_crash_cell(cell);
        let fires = take_verified_faults();
        assert_eq!(fires, 1, "{key}");
        reset_verified_faults();
        run_reclaim_crash_cell(CrashCell::Control);
        assert_eq!(take_verified_faults(), 0);
        crate::graph_read_view_test_support::PathReceipt {
            key,
            fires,
            clean_controls: 1,
        }
    })
    .collect()
}

fn ze166_fixture(
    store: &Store,
) -> (
    crate::property_graph::NodeId,
    crate::property_graph::NodeId,
    Vec<crate::property_graph::staging::ItemReceipt>,
) {
    let control = QueryControl::Cancel(CancelToken::new());
    let image = CanonicalContents::node(&mut [], &mut [], None, None).expect("node");
    let nodes = store
        .apply_native_graph(
            &["a", "b"].map(|name| StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze166", name).unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }),
            &control,
        )
        .unwrap();
    let EntityId::Node(a) = nodes[0].entity else {
        panic!("node")
    };
    let EntityId::Node(b) = nodes[1].entity else {
        panic!("node")
    };
    let receipts = store
        .apply_native_graph(&[ze166_relationship(a, b)], &control)
        .unwrap();
    (a, b, receipts.to_vec())
}

fn ze166_relationship(
    a: crate::property_graph::NodeId,
    b: crate::property_graph::NodeId,
) -> StructuredWrite<'static, 'static> {
    StructuredWrite {
        key: ApplicationKey::new(EntityKind::Relationship, "ze166", "ab").unwrap(),
        revision: GraphRevision::new(1).unwrap(),
        operation: StructuredOperation::Create,
        image: Some(WriteImage::Relationship {
            source: NodeRef::Existing(a),
            target: NodeRef::Existing(b),
            relationship_type: GraphName::new("LINKS").unwrap(),
            properties: &[],
        }),
    }
}

fn ze166_detach(store: &Store, a: crate::property_graph::NodeId) {
    store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze166", "a").unwrap(),
                revision: GraphRevision::new(2).unwrap(),
                operation: StructuredOperation::Delete(
                    EntityId::Node(a),
                    crate::property_graph::GraphDeleteMode::Detach,
                ),
                image: None,
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap();
}

#[test]
fn ze166_detached_relationship_install_retry_preserves_receipt() {
    let parent = super::tempfile::tempdir().unwrap();
    let store = Store::create_native_graph(parent.path().join("native"), options(), None).unwrap();
    let (a, b, original) = ze166_fixture(&store);
    let control = QueryControl::Cancel(CancelToken::new());
    ze166_detach(&store, a);
    let replay = store
        .apply_native_graph(&[ze166_relationship(a, b)], &control)
        .expect("authenticated original install replay after DETACH");
    assert!(replay[0].replayed);
    assert_eq!(replay[0].entity, original[0].entity);
    assert_eq!(replay[0].generation, original[0].generation);
    assert_eq!(replay[0].revision, original[0].revision);
}

fn ze166_key_lookup(
    store: &Store,
    lease: &super::super::NativeReadLease,
    name: &str,
) -> Result<Option<EntityId>, crate::property_graph::storage::tree::directory::TreeError> {
    use crate::property_graph::query::{
        resources::QueryMemory,
        runtime::{RuntimeContext, RuntimeLimits},
    };
    use crate::property_graph::storage::tree::directory::TreeResources;
    use crate::property_graph::storage::{
        GraphReadView, NativeCatalog, NativeQuerySource, NativeReadCapability,
    };
    let shared = crate::property_graph::resources::GraphResources::from_store(store).unwrap();
    let memory = QueryMemory::new(&shared, 8 * 1024 * 1024).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let mut runtime =
        RuntimeContext::new(lease, &control, &memory, RuntimeLimits::default()).unwrap();
    let capability = NativeReadCapability::admit(lease, &runtime).unwrap();
    let mut resources = TreeResources::for_query(&mut runtime).unwrap();
    let source = NativeQuerySource::new(capability, &resources, 64).unwrap();
    let catalog = NativeCatalog::open(&source, &mut resources).unwrap();
    drop(resources);
    let view = GraphReadView::new(&source, &catalog).unwrap();
    let mut resources = TreeResources::for_query(&mut runtime).unwrap();
    view.lookup_application_key(
        ApplicationKey::new(EntityKind::Relationship, "ze166", name).unwrap(),
        &mut resources,
    )
}

type Ze166FenceEvidence = BTreeMap<Vec<u8>, (Vec<u8>, Option<Vec<u8>>)>;

fn ze166_fence_evidence(store: &Store) -> Ze166FenceEvidence {
    use crate::property_graph::storage::{
        records::verify_fence_entry, stream::PayloadSlice, tree::Key,
    };
    let lease = store.admit_native_read().unwrap();
    let shared = crate::property_graph::resources::GraphResources::from_store(store).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let memory = StorageMemory::new(&writer, &control, 16 * 1024 * 1024).unwrap();
    let source = NativePreparationSource::new(&lease, &memory, 64).unwrap();
    let mut r = source.resources(64 * 1024 * 1024).unwrap();
    let catalog =
        crate::property_graph::storage::NativePreparationCatalog::open(&source, &mut r).unwrap();
    let roots = lease.bundle().roots();
    let root = roots.directory(TreeKind::KeyFences).unwrap();
    let mut cursor = DirectoryCursor::seek(&source, root, None, &mut r).unwrap();
    let mut result = BTreeMap::new();
    while let Some(entry) = cursor.next_entry(&mut r).unwrap() {
        let Key::Inline(key) = entry.key() else {
            panic!("key")
        };
        let fence = verify_fence_entry(
            &source,
            root,
            entry,
            &catalog,
            lease.bundle().document(),
            &mut r,
        )
        .unwrap();
        let (provenance, canonical) = fence.required_payloads();
        let mut read = |payload: crate::property_graph::storage::payload::PayloadRef| {
            let slice =
                PayloadSlice::new(&source, roots.store(), entry.creation_generation(), payload);
            let mut bytes = vec![0; slice.len() as usize];
            assert_eq!(slice.read_at(0, &mut bytes, &mut r).unwrap(), bytes.len());
            bytes
        };
        result.insert(key.to_vec(), (read(provenance), canonical.map(read)));
    }
    result
}

fn ze166_physical(
    store: &Store,
    lease: &super::super::NativeReadLease,
    a: crate::property_graph::NodeId,
) -> (Vec<u128>, bool, usize, u64, u64) {
    use crate::property_graph::storage::{
        records::verify_fence_entry,
        tree::{Key, directory::lookup_entry},
    };
    let shared = crate::property_graph::resources::GraphResources::from_store(store).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let memory = StorageMemory::new(&writer, &control, 16 * 1024 * 1024).unwrap();
    let source = NativePreparationSource::new(lease, &memory, 64).unwrap();
    let mut r = source.resources(64 * 1024 * 1024).unwrap();
    let roots = lease.bundle().roots();
    let mut rels = Vec::new();
    let mut cursor = DirectoryCursor::seek(
        &source,
        roots.directory(TreeKind::Relationships).unwrap(),
        None,
        &mut r,
    )
    .unwrap();
    while let Some(entry) = cursor.next_entry(&mut r).unwrap() {
        let Key::Inline(key) = entry.key() else {
            panic!("key")
        };
        rels.push(u128::from_le_bytes(key.try_into().unwrap()));
    }
    for kind in [TreeKind::OutRanges, TreeKind::InRanges] {
        let root = roots.directory(kind).unwrap();
        let mut cursor = DirectoryCursor::seek(&source, root, None, &mut r).unwrap();
        let mut scratch =
            crate::property_graph::storage::adjacency::RangeScratch::for_prepare(&memory, &mut r)
                .unwrap();
        let mut edges = Vec::new();
        while let Some(entry) = cursor.next_entry(&mut r).unwrap() {
            let range = crate::property_graph::storage::adjacency::validate_range(
                &source,
                root,
                entry,
                lease.bundle().sequence(),
                &mut scratch,
                &mut r,
            )
            .unwrap();
            edges.extend(range.edges().iter().map(|edge| edge.rel.get()));
        }
        edges.sort_unstable();
        assert_eq!(
            edges, rels,
            "physical OUT/IN match exact surviving relationships"
        );
    }
    let tombstone = lookup_entry(
        &source,
        roots.directory(TreeKind::Nodes).unwrap(),
        &a.get().to_le_bytes(),
        &mut r,
    )
    .unwrap()
    .is_some();
    let mut types = 0;
    let mut cursor = DirectoryCursor::seek(
        &source,
        roots.directory(TreeKind::RelationshipTypes).unwrap(),
        None,
        &mut r,
    )
    .unwrap();
    while cursor.next_entry(&mut r).unwrap().is_some() {
        types += 1;
    }
    let catalog =
        crate::property_graph::storage::NativePreparationCatalog::open(&source, &mut r).unwrap();
    let root = roots.directory(TreeKind::KeyFences).unwrap();
    let mut cursor = DirectoryCursor::seek(&source, root, None, &mut r).unwrap();
    let mut payloads = std::collections::BTreeSet::new();
    let mut packs = std::collections::BTreeSet::new();
    let mut payload_bytes = 0;
    while let Some(entry) = cursor.next_entry(&mut r).unwrap() {
        let fence = verify_fence_entry(&source, root, entry, &catalog, None, &mut r).unwrap();
        let (kind, id) = match fence.incarnation() {
            EntityId::Node(id) => (TreeKind::Nodes, id.get()),
            EntityId::Relationship(id) => (TreeKind::Relationships, id.get()),
        };
        if lookup_entry(
            &source,
            roots.directory(kind).unwrap(),
            &id.to_le_bytes(),
            &mut r,
        )
        .unwrap()
        .is_some()
        {
            continue;
        }
        let (provenance, canonical) = fence.required_payloads();
        for payload in std::iter::once(provenance).chain(canonical) {
            if payloads.insert((payload.reference().artifact, payload.reference().offset)) {
                payload_bytes += payload.len();
            }
            packs.insert(payload.reference().artifact);
        }
    }
    let pack_bytes = packs
        .iter()
        .map(|id| {
            std::fs::metadata(
                store
                    .directory
                    .join(format!("graph-{:032x}.zgraph", id.get())),
            )
            .unwrap()
            .len()
        })
        .sum();
    (rels, tombstone, types, payload_bytes, pack_bytes)
}

#[test]
fn ze46_detach_sweeps_bounded_edges_and_preserves_fences() {
    run_ze46_detach_sweeps_bounded_edges_and_preserves_fences();
}

fn run_ze46_detach_sweeps_bounded_edges_and_preserves_fences() -> (bool, u64) {
    let parent = super::tempfile::tempdir().unwrap();
    let path = parent.path().join("native");
    let store = Store::create_native_graph(&path, options(), None).unwrap();
    let (a, b, original) = ze166_fixture(&store);
    let control = QueryControl::Cancel(CancelToken::new());
    let extras = [("parallel", a, b), ("loop", a, a), ("survivor", b, b)].map(|(key, from, to)| {
        let mut request = ze166_relationship(from, to);
        request.key = ApplicationKey::new(EntityKind::Relationship, "ze166", key).unwrap();
        request
    });
    let extra = store
        .apply_native_graph(&extras, &control)
        .unwrap()
        .to_vec();
    let old = store.admit_native_read().unwrap();
    assert_eq!(
        ze166_key_lookup(&store, &old, "ab").unwrap(),
        Some(original[0].entity)
    );
    ze166_detach(&store, a);
    let evidence = ze166_fence_evidence(&store);
    let mut changed = ze166_relationship(a, b);
    changed.operation = StructuredOperation::Put(original[0].entity);
    changed.revision = GraphRevision::new(2).unwrap();
    assert!(matches!(
        store.apply_native_graph(&[changed], &control),
        Err(super::super::NativeGraphError::Stage(
            crate::property_graph::staging::StageError::Endpoint
        ))
    ));
    for pass in 0..3 {
        let admission = store.admit_native_graph_maintenance().unwrap();
        store
            .commit_native_graph_maintenance_with_limits(
                &admission,
                &control,
                super::super::maintenance::MaintenanceLimits {
                    sweep_limit: 1,
                    ..Default::default()
                },
            )
            .unwrap();
        let lease = store.admit_native_read().unwrap();
        let (rels, tombstone, types, _, _) = ze166_physical(&store, &lease, a);
        assert_eq!(rels.len(), 3 - pass, "one hidden edge per slice");
        assert_eq!(types, rels.len());
        assert_eq!(
            ze166_fence_evidence(&store),
            evidence,
            "exact original canonical/provenance streams survive every slice"
        );
        assert_eq!(
            tombstone,
            pass < 2,
            "tombstone remains until last physical incident"
        );
        assert_eq!(ze166_key_lookup(&store, &lease, "ab").unwrap(), None);
        assert_eq!(
            ze166_key_lookup(&store, &lease, "survivor").unwrap(),
            Some(extra[2].entity)
        );
    }
    assert_eq!(
        ze166_key_lookup(&store, &old, "ab").unwrap(),
        Some(original[0].entity)
    );
    assert_eq!(
        ze166_key_lookup(&store, &old, "loop").unwrap(),
        Some(extra[1].entity)
    );
    drop(old);
    // Reopen once with maintenance in the WAL, then once from a checkpoint.
    store.close().unwrap();
    drop(store);
    let store = Store::open_native_graph(&path, options(), None).expect("replay bounded sweeps");
    store.checkpoint_native_graph(&control).unwrap();
    store.close().unwrap();
    drop(store);
    let store = Store::open_native_graph(&path, options(), None).expect("checkpoint swept graph");
    assert_eq!(ze166_fence_evidence(&store), evidence);
    let lease = store.admit_native_read().unwrap();
    let (_, _, _, payload, packs) = ze166_physical(&store, &lease, a);
    eprintln!(
        "ZE-166 after sweep evidence-only replay payload bytes={payload}; whole containing pack bytes={packs}"
    );
    drop(lease);
    let replay = store
        .apply_native_graph(&[ze166_relationship(a, b)], &control)
        .unwrap();
    assert_eq!(
        ze166_key_lookup(&store, &store.admit_native_read().unwrap(), "ab").unwrap(),
        None
    );
    assert!(replay[0].replayed);
    assert_eq!(replay[0].generation, original[0].generation);
    assert_eq!(replay[0].entity, original[0].entity);
    let sweep_observation = (
        ze166_key_lookup(&store, &store.admit_native_read().unwrap(), "ab")
            .unwrap()
            .is_some(),
        replay[0].generation.get(),
    );
    drop(replay);
    let deletion = StructuredWrite {
        key: ApplicationKey::new(EntityKind::Relationship, "ze166", "ab").unwrap(),
        revision: GraphRevision::new(2).unwrap(),
        operation: StructuredOperation::Delete(
            original[0].entity,
            crate::property_graph::GraphDeleteMode::Restrict,
        ),
        image: None,
    };
    let deleted = store
        .apply_native_graph(&[deletion], &control)
        .unwrap()
        .to_vec();
    let replay = store.apply_native_graph(&[deletion], &control).unwrap();
    assert!(replay[0].replayed);
    assert_eq!(replay[0].generation, deleted[0].generation);
    drop(replay);
    let node_delete = StructuredWrite {
        key: ApplicationKey::new(EntityKind::Node, "ze166", "a").unwrap(),
        revision: GraphRevision::new(2).unwrap(),
        operation: StructuredOperation::Delete(
            EntityId::Node(a),
            crate::property_graph::GraphDeleteMode::Detach,
        ),
        image: None,
    };
    let retry = store.apply_native_graph(&[node_delete], &control).unwrap();
    assert!(retry[0].replayed);
    assert_eq!(retry[0].generation.get(), 4);
    drop(retry);
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let recreated = store
        .apply_native_graph(
            &[StructuredWrite {
                key: node_delete.key,
                revision: GraphRevision::new(3).unwrap(),
                operation: StructuredOperation::Recreate(GraphRevision::new(2).unwrap()),
                image: Some(WriteImage::Node(&image)),
            }],
            &control,
        )
        .unwrap();
    let EntityId::Node(fresh) = recreated[0].entity else {
        panic!("node")
    };
    assert_ne!(a, fresh);
    drop(recreated);
    let mut recreate = ze166_relationship(fresh, b);
    recreate.revision = GraphRevision::new(3).unwrap();
    recreate.operation = StructuredOperation::Recreate(GraphRevision::new(2).unwrap());
    let recreated = store.apply_native_graph(&[recreate], &control).unwrap();
    assert_ne!(recreated[0].entity, original[0].entity);
    drop(recreated);
    let lease = store.admit_native_read().unwrap();
    let (_, _, _, payload, packs) = ze166_physical(&store, &lease, a);
    eprintln!(
        "ZE-166 evidence-only replay payload bytes={payload}; whole containing pack bytes={packs}"
    );
    drop(lease);
    store.close().unwrap();
    drop(store);
    let reopened = Store::open_native_graph(&path, options(), None)
        .expect("replay explicit swept Delete and Recreate");
    reopened.close().unwrap();
    ze166_both_live_missing_relationship_is_corruption();
    sweep_observation
}

fn ze166_both_live_missing_relationship_is_corruption() {
    use crate::property_graph::storage::tree::directory::DirectoryRoot;
    let parent = super::tempfile::tempdir().unwrap();
    let store = Store::create_native_graph(parent.path().join("corrupt"), options(), None).unwrap();
    let (a, b, _) = ze166_fixture(&store);
    store
        .checkpoint_native_graph(&QueryControl::Cancel(CancelToken::new()))
        .unwrap();
    let old = store.admit_native_read().unwrap();
    let bundle = old.bundle();
    let mut roots = bundle.roots();
    roots
        .replace(DirectoryRoot::empty(
            roots.store(),
            TreeKind::Relationships,
            roots.generation(),
        ))
        .unwrap();
    let mut wal_roots = bundle.wal_roots();
    wal_roots.slots[1] = None;
    let input = super::super::NativeGraphBundleInput {
        base: bundle.base(),
        root_envelope: bundle.root_envelope(),
        roots,
        wal_roots,
        sequence: bundle.sequence(),
        catalog: bundle.catalog(),
        vector: bundle.vector(),
        text: bundle.text(),
        reclaim: bundle.reclaim(),
        high_waters: bundle.high_waters(),
        prepared_inventories: bundle.prepared_inventories().to_vec(),
        lexical: bundle.lexical(),
        document: None,
    };
    drop(old);
    store.install_native_graph_for_test(input).unwrap();
    let lease = store.admit_native_read().unwrap();
    assert!(matches!(
        ze166_key_lookup(&store, &lease, "ab"),
        Err(
            crate::property_graph::storage::tree::directory::TreeError::Invalid(
                "missing relationship has two live endpoints"
            )
        )
    ));
    assert!(
        store
            .apply_native_graph(
                &[ze166_relationship(a, b)],
                &QueryControl::Cancel(CancelToken::new())
            )
            .is_err()
    );
}

#[test]
fn ze166_sweep_work_is_independent_of_unrelated_relationships() {
    use crate::property_graph::storage::consolidation::{
        SWEEP_WORK, select_finished_tombstones, select_hidden_relationships,
    };
    let measure = |relationships: usize| {
        let parent = super::tempfile::tempdir().unwrap();
        let path = parent.path().join("native");
        let store = Store::create_native_graph(&path, options(), None).unwrap();
        let (a, b, _) = ze166_fixture(&store);
        let control = QueryControl::Cancel(CancelToken::new());
        for chunk in (0..relationships).collect::<Vec<_>>().chunks(32) {
            let names: Vec<_> = chunk.iter().map(|id| format!("live-{id}")).collect();
            let requests: Vec<_> = names
                .iter()
                .map(|name| {
                    let mut request = ze166_relationship(b, b);
                    request.key =
                        ApplicationKey::new(EntityKind::Relationship, "ze166", name).unwrap();
                    request
                })
                .collect();
            store.apply_native_graph(&requests, &control).unwrap();
        }
        ze166_detach(&store, a);
        SWEEP_WORK.with(|counter| counter.set([0; 4]));
        {
            let lease = store.admit_native_read().unwrap();
            let shared =
                crate::property_graph::resources::GraphResources::from_store(&store).unwrap();
            let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
            let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
            let source = NativePreparationSource::new(&lease, &memory, 64).unwrap();
            let mut r = source.resources(256 * 1024 * 1024).unwrap();
            let catalog =
                crate::property_graph::storage::NativePreparationCatalog::open(&source, &mut r)
                    .unwrap();
            let roots = lease.bundle().roots();
            let mut resume = None;
            let swept = select_hidden_relationships(
                &source,
                roots,
                &catalog,
                None,
                128,
                &mut resume,
                &memory,
                &mut r,
            )
            .unwrap();
            select_finished_tombstones(
                &source,
                roots,
                &catalog,
                None,
                swept.as_slice(),
                128,
                lease.bundle().sequence(),
                &mut None,
                &memory,
                &mut r,
            )
            .unwrap();
            if relationships > 128 {
                let before = SWEEP_WORK.with(|counter| counter.get()[0]);
                assert!(resume.is_some());
                let rest = select_hidden_relationships(
                    &source,
                    roots,
                    &catalog,
                    None,
                    128,
                    &mut resume,
                    &memory,
                    &mut r,
                )
                .unwrap();
                assert!(rest.as_slice().is_empty());
                assert!(resume.is_none());
                let after = SWEEP_WORK.with(|counter| counter.get()[0]);
                assert_eq!(after - before, (relationships + 1 - 128) as u64);
                SWEEP_WORK.with(|counter| {
                    let mut counts = counter.get();
                    counts[0] = before;
                    counter.set(counts);
                });
            }
        }
        let selection = SWEEP_WORK.with(|counter| counter.get());
        for _ in 0..12 {
            store
                .commit_native_graph_maintenance(
                    &store.admit_native_graph_maintenance().unwrap(),
                    &control,
                )
                .unwrap();
            let lease = store.admit_native_read().unwrap();
            if !ze166_physical(&store, &lease, a).1 {
                break;
            }
        }
        assert!(!ze166_physical(&store, &store.admit_native_read().unwrap(), a).1);
        store.checkpoint_native_graph(&control).unwrap();
        store.close().unwrap();
        drop(store);
        SWEEP_WORK.with(|counter| counter.set([0; 4]));
        let store = Store::open_native_graph(&path, options(), None).unwrap();
        let open = SWEEP_WORK.with(|counter| counter.get()[2]);
        store.close().unwrap();
        (selection[0], selection[1], open)
    };
    let small = measure(16);
    let large = measure(160);
    eprintln!("ZE-166 work R=16 {small:?}; R=160 {large:?}");
    assert_eq!(
        small.1, large.1,
        "per-tombstone incident work scales with R"
    );
    assert_eq!(small.2, large.2, "swept-node open validation scales with R");
    assert!(large.0 <= 128, "hidden selection exceeds its visit budget");
    ze166_assert_blocked_tombstone_budget();
}

// Every visited tombstone here is blocked. Selection must still stop and
// resume, rather than chasing 128 successful removals through the whole tree.
#[cfg(test)]
fn ze166_assert_blocked_tombstone_budget() {
    use crate::property_graph::storage::consolidation::{SWEEP_WORK, select_finished_tombstones};
    let parent = super::tempfile::tempdir().unwrap();
    let store = Store::create_native_graph(parent.path().join("native"), options(), None).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let image = CanonicalContents::node(&mut [], &mut [], None, None).unwrap();
    let names: Vec<_> = (0..130).map(|id| format!("blocked-{id}")).collect();
    for chunk in names.chunks(32) {
        let nodes: Vec<_> = chunk
            .iter()
            .map(|name| StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze166", name).unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            })
            .collect();
        let receipts = store.apply_native_graph(&nodes, &control).unwrap();
        let edges: Vec<_> = chunk
            .iter()
            .zip(receipts.iter())
            .map(|(name, receipt)| {
                let EntityId::Node(node) = receipt.entity else {
                    panic!("node");
                };
                let mut edge = ze166_relationship(node, node);
                edge.key = ApplicationKey::new(EntityKind::Relationship, "ze166", name).unwrap();
                edge
            })
            .collect();
        store.apply_native_graph(&edges, &control).unwrap();
        let deletes: Vec<_> = chunk
            .iter()
            .zip(receipts.iter())
            .map(|(name, receipt)| StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze166", name).unwrap(),
                revision: GraphRevision::new(2).unwrap(),
                operation: StructuredOperation::Delete(
                    receipt.entity,
                    crate::property_graph::GraphDeleteMode::Detach,
                ),
                image: None,
            })
            .collect();
        store.apply_native_graph(&deletes, &control).unwrap();
    }
    let lease = store.admit_native_read().unwrap();
    let shared = crate::property_graph::resources::GraphResources::from_store(&store).unwrap();
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
    let source = NativePreparationSource::new(&lease, &memory, 64).unwrap();
    let mut r = source.resources(256 * 1024 * 1024).unwrap();
    let catalog =
        crate::property_graph::storage::NativePreparationCatalog::open(&source, &mut r).unwrap();
    let mut resume = None;
    for expected in [128, 2] {
        SWEEP_WORK.with(|counter| counter.set([0; 4]));
        let selected = select_finished_tombstones(
            &source,
            lease.bundle().roots(),
            &catalog,
            None,
            &[],
            128,
            lease.bundle().sequence(),
            &mut resume,
            &memory,
            &mut r,
        )
        .unwrap();
        assert!(selected.as_slice().is_empty());
        assert_eq!(SWEEP_WORK.with(|counter| counter.get()[3]), expected);
    }
    assert!(resume.is_none());
}

#[cfg(feature = "test-support")]
pub(crate) fn run_ze75_reclaim_evidence(
    seed: u64,
) -> Vec<crate::graph_commit_recovery_test_support::ReclaimEvidence> {
    use super::publication::{reset_verified_faults, take_verified_faults};
    [
        CrashCell::BeforeIntent,
        CrashCell::BeforeFirstUnlink,
        CrashCell::AfterOneUnlink,
        CrashCell::DirectorySync,
        CrashCell::BeforeCompletion,
        CrashCell::LostCompletionAck,
        CrashCell::Control,
    ]
    .into_iter()
    .map(|cell| {
        reset_verified_faults();
        let mut report =
            crate::graph_commit_recovery_test_support::with_qualification_nonces(seed, || {
                run_reclaim_crash_cell_observed(cell, true)
            });
        report.fires = take_verified_faults();
        report
    })
    .collect()
}
