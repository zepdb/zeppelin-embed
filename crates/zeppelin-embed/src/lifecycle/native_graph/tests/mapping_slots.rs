//! Mapping-slot retention under long sequential commit histories (ZE-164).
//!
//! Validator-mode directory traversal must validate and release each artifact
//! mapping it touches instead of pinning it for the caller's whole lifetime.
//! One pack per commit otherwise exhausts the fixed mapping-slot table.

use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use crate::property_graph::query::resources::QueryMemory;
use crate::property_graph::query::runtime::{RuntimeContext, RuntimeLimits};
use crate::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use crate::property_graph::storage::tree::directory::TreeResources;
use crate::property_graph::storage::{
    GraphReadView, NativeCatalog, NativeQuerySource, NativeReadCapability,
};
use crate::property_graph::{
    ApplicationKey, CanonicalContents, EntityId, EntityKind, GraphRevision, NodeId,
};

fn durable_options() -> OpenOptions {
    OpenOptions::new()
        .with_durability(DurabilityMode::Durable, CommitTier::Durable)
        .with_max_resident_bytes(256 * 1024 * 1024)
}

/// One single-node create commit under its own application key.
fn commit_probe_node(store: &Store, index: usize) -> NodeId {
    let image = CanonicalContents::node(&mut [], &mut [], None, None).expect("probe image");
    let receipts = store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "probe", &index.to_string())
                    .expect("probe key"),
                revision: GraphRevision::new(1).expect("probe revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &QueryControl::Cancel(CancelToken::new()),
        )
        .unwrap_or_else(|error| panic!("commit {index}: {error:?}"));
    match receipts[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("commit {index} returned a relationship"),
    }
}

/// Exact per-node logical state read back through the public graph view.
#[derive(Debug, Eq, PartialEq)]
struct NodeState {
    node: NodeId,
    revision: u64,
    original_generation: u64,
    canonical: Vec<u8>,
}

/// Read every sampled node's canonical image through one admitted lease.
fn logical_state(store: &Store, nodes: &[NodeId]) -> Vec<NodeState> {
    let lease = store.admit_native_read().expect("oracle reader");
    let shared = crate::property_graph::resources::GraphResources::from_store(store)
        .expect("shared graph resources");
    let memory = QueryMemory::new(&shared, 8 * 1024 * 1024).expect("query memory");
    let control = QueryControl::Cancel(CancelToken::new());
    let mut runtime = RuntimeContext::new(&lease, &control, &memory, RuntimeLimits::default())
        .expect("retained runtime");
    let capability = NativeReadCapability::admit(&lease, &runtime).expect("retained capability");
    let mut resources = TreeResources::for_query(&mut runtime).expect("initial resources");
    let source = NativeQuerySource::new(capability, &resources, 16).expect("retained source");
    let catalog = NativeCatalog::open(&source, &mut resources).expect("retained catalog");
    drop(resources);
    let view = GraphReadView::new(&source, &catalog).expect("retained view");

    let mut output = Vec::new();
    for node in nodes {
        let mut resources = TreeResources::for_query(&mut runtime).expect("record resources");
        let record = view
            .lookup_node(*node, &mut resources)
            .expect("node lookup")
            .expect("live node");
        let mut canonical = vec![0_u8; record.record().canonical_bytes().len() as usize];
        let length = canonical.len();
        assert_eq!(
            record
                .record()
                .canonical_bytes()
                .read_at(0, &mut canonical, &mut resources)
                .expect("canonical read"),
            length
        );
        output.push(NodeState {
            node: *node,
            revision: record.record().revision().get(),
            original_generation: record.record().provenance().original_generation().get(),
            canonical,
        });
    }
    output
}

/// Each commit writes its own pack, so validator-mode leaf verification touches
/// one artifact per earlier record. Retaining every mapping exhausts the fixed
/// 64-slot preparation table at commit index 63; scoped reads must not.
#[test]
fn sequential_single_node_commits_outlive_the_mapping_slot_table() {
    let parent = super::tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("slots");
    let store = Store::create_native_graph(&path, durable_options(), None).expect("create store");
    let mut nodes = Vec::new();
    for index in 0..320 {
        nodes.push(commit_probe_node(&store, index));
    }
    assert_eq!(nodes.len(), 320);
    let sampled: Vec<NodeId> = nodes.iter().step_by(37).copied().collect();
    let state = logical_state(&store, &sampled);
    assert_eq!(state.len(), sampled.len());
    store.close().expect("close store");
}

/// The ticket's acceptance: ten thousand sequential small commits, automatic
/// checkpoints, a clean close and reopen, and unchanged read results.
///
/// Ignored by default on wall time only, never on outcome. Every commit
/// re-verifies its whole touched directory leaf, which holds about 220 entries
/// at 16 KiB pages, so a debug run costs tens of minutes. That cost is the
/// existing O(leaf) validator contract, not the scoped reads this ticket added:
/// the same shape is measurable before the fix once the slot table is widened.
/// `sequential_single_node_commits_outlive_the_mapping_slot_table` is the
/// always-on gate for the same failure.
#[test]
#[ignore = "ZE-164 acceptance: tens of minutes in a debug build"]
fn ten_thousand_small_commits_checkpoint_and_reopen() {
    let parent = super::tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("acceptance");
    let store = Store::create_native_graph(&path, durable_options(), None).expect("create store");
    let mut nodes = Vec::new();
    for index in 0..10_000 {
        nodes.push(commit_probe_node(&store, index));
    }
    let sampled: Vec<NodeId> = nodes.iter().step_by(311).copied().collect();
    let before = logical_state(&store, &sampled);
    store.close().expect("close store");

    let reopened =
        Store::open_native_graph(&path, durable_options(), None).expect("reopen native store");
    let after = logical_state(&reopened, &sampled);
    assert_eq!(before, after);
    reopened.close().expect("close reopened store");
}
