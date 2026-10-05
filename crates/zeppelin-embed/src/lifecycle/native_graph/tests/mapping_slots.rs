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
/// mapping table without scoped reads. The default table now has 8,192 slots.
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

#[cfg(test)]
fn check_slot_reports(
    reports: &[crate::property_graph::storage::mapping_slot_capture::Report],
    context: &str,
) -> Result<(), String> {
    use crate::property_graph::storage::tree::directory::RESERVED_PINNED_SLOTS;
    if reports.is_empty() {
        return Err(format!("{context}: missing source observations"));
    }
    for report in reports {
        if report.filled > report.capacity - RESERVED_PINNED_SLOTS
            || report.post_exhaustion_resolves > RESERVED_PINNED_SLOTS
        {
            return Err(format!("{context}: {report:?}"));
        }
    }
    Ok(())
}

#[test]
fn ze168_mapping_slots_per_commit() {
    use crate::property_graph::storage::mapping_slot_capture::{Capture, Kind};
    let capture = Capture::start();
    let parent = super::tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("counter-slots");
    let store = Store::create_native_graph(&path, durable_options(), None).expect("create store");
    capture.take();
    let mut nodes = Vec::new();
    let mut all = Vec::new();
    for index in 0..320 {
        nodes.push(commit_probe_node(&store, index));
        let reports = capture.take();
        let preparation: Vec<_> = reports
            .iter()
            .copied()
            .filter(|r| r.kind == Kind::Preparation)
            .collect();
        check_slot_reports(&preparation, &format!("commit {index}")).unwrap();
        check_slot_reports(&reports, &format!("commit {index}")).unwrap();
        all.extend(reports);
    }
    let sampled: Vec<_> = nodes.iter().step_by(37).copied().collect();
    let before = logical_state(&store, &sampled);
    store.close().expect("close");
    // Match the regression's default mapping budget: recovery also retains
    // unscoped reads, so the write-side 64-slot gate does not apply to reopen.
    capture.restore_default_capacity();
    let reopened = Store::open_native_graph(&path, durable_options(), None).expect("reopen");
    let reports = capture.take();
    let recovery: Vec<_> = reports
        .iter()
        .copied()
        .filter(|r| r.kind == Kind::Recovery)
        .collect();
    assert!(
        !recovery.is_empty(),
        "reopen: missing recovery observations"
    );
    for report in &recovery {
        assert!(report.filled <= report.capacity, "reopen: {report:?}");
    }
    all.extend(reports);
    for kind in [Kind::Preparation, Kind::Recovery] {
        let reports: Vec<_> = all.iter().filter(|r| r.kind == kind).collect();
        if kind == Kind::Preparation {
            assert!(
                reports.iter().any(|r| r.filled == 60),
                "{kind:?}: threshold never reached"
            );
        }
        println!(
            "{kind:?}: sources={}, max fills={}, max post-exhaustion resolves={}, generations={:?}",
            reports.len(),
            reports.iter().map(|r| r.filled).max().unwrap(),
            reports
                .iter()
                .map(|r| r.post_exhaustion_resolves)
                .max()
                .unwrap(),
            reports
                .iter()
                .map(|r| r.generation)
                .min()
                .zip(reports.iter().map(|r| r.generation).max())
        );
    }
    assert_eq!(before, logical_state(&reopened, &sampled));
    reopened.close().expect("close reopened");
}

#[test]
fn ze168_mapping_slot_gate_rejects_missing_and_exceeded_bounds() {
    use crate::property_graph::storage::mapping_slot_capture::{Kind, Report};
    let report = Report {
        kind: Kind::Preparation,
        generation: 1,
        capacity: 64,
        filled: 60,
        post_exhaustion_resolves: 4,
    };
    assert!(check_slot_reports(&[], "missing").is_err());
    assert!(check_slot_reports(&[report], "boundary").is_ok());
    assert!(
        check_slot_reports(
            &[Report {
                filled: 61,
                ..report
            }],
            "fills"
        )
        .is_err()
    );
    assert!(
        check_slot_reports(
            &[Report {
                post_exhaustion_resolves: 5,
                ..report
            }],
            "resolves"
        )
        .is_err()
    );
}

#[test]
fn ze168_post_exhaustion_hit_and_fill_are_distinct() {
    use crate::property_graph::resources::GraphResources;
    use crate::property_graph::staging::{WriteLimits, WriteMemory};
    use crate::property_graph::storage::NativePreparationSource;
    use crate::property_graph::storage::memory::StorageMemory;
    use crate::property_graph::storage::tree::directory::BlockSource;
    let parent = super::tempfile::tempdir().expect("temporary parent");
    let path = parent.path().join("boundary");
    let store = Store::create_native_graph(&path, durable_options(), None).expect("create");
    commit_probe_node(&store, 0);
    let old_catalog = store.admit_native_read().unwrap().bundle().catalog().block;
    commit_probe_node(&store, 1);
    let lease = store.admit_native_read().unwrap();
    let catalog = lease.bundle().catalog().block;
    assert_ne!(old_catalog.artifact, catalog.artifact);
    let shared = GraphResources::from_store(&store).unwrap();
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
    let source = NativePreparationSource::new(&lease, &memory, 5).unwrap();
    let mut resources = source.resources(u64::MAX).unwrap();
    source.resolve(catalog, &mut resources).unwrap();
    let first = source.mapping_slot_report();
    assert_eq!((first.filled, first.post_exhaustion_resolves), (1, 0));
    source.resolve(catalog, &mut resources).unwrap();
    let hit = source.mapping_slot_report();
    assert_eq!((hit.filled, hit.post_exhaustion_resolves), (1, 1));
    source.resolve(old_catalog, &mut resources).unwrap();
    let fill = source.mapping_slot_report();
    assert_eq!((fill.filled, fill.post_exhaustion_resolves), (2, 2));
    if let QueryControl::Cancel(token) = &control {
        token.cancel();
    }
    assert!(source.resolve(catalog, &mut resources).is_err());
    assert_eq!(source.mapping_slot_report().post_exhaustion_resolves, 3);
}
