//! ZE-52 slice D1: one writer lease that admits both a read view and a
//! writer overlay.
//!
//! The structured-write path admits a writer lease, builds an admitted base
//! from the caller's request list, stages it and commits. A query-driven
//! mutation needs the same lease to also carry a live `GraphReadView`, so a
//! MATCH clause can discover its targets before the overlay stages them.
//! These tests pin the second admission shape, `Store::with_native_mutation`,
//! and pin that the structured path it was extracted from did not move.

#![cfg(test)]
#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use super::super::base::NativeAdmittedBase;
use super::super::mutate::{NativeMutationConsumer, NativeMutationError, NativeMutationReport};
use super::*;
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::property_graph::query::runtime::{NativeExecutionError, RuntimeError, WorkKind};
use crate::property_graph::staging::{BatchEntityRef, GraphBatchReadView, StatementImages};
use crate::property_graph::storage::NativePreparationSource;
use crate::property_graph::{BatchDisposition, GraphDeleteMode};

const DOCUMENT_TEXT: &str = "mutation admission text";

fn fixture_options() -> OpenOptions {
    OpenOptions::new()
        .with_durability(DurabilityMode::Durable, CommitTier::Durable)
        .with_max_resident_bytes(256 * 1024 * 1024)
}

fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}

/// Three committed `Document` nodes, exactly slice B's shape.
struct MutationFixture {
    directory: super::tempfile::TempDir,
    path: std::path::PathBuf,
    store: Store,
    nodes: [NodeId; 3],
}

impl MutationFixture {
    fn commit() -> Self {
        let directory = super::tempfile::tempdir().expect("temporary parent");
        let path = directory.path().join("mutation-admission");
        let store = Store::create_native_graph(&path, fixture_options(), None)
            .expect("create native store");
        let first = commit_node(&store, "first");
        let second = commit_node(&store, "second");
        let third = commit_node(&store, "third");
        assert_ne!(first, second);
        assert_ne!(second, third);
        Self {
            directory,
            path,
            store,
            nodes: [first, second, third],
        }
    }

    /// Closes the fixture store and reopens the same directory, keeping the
    /// temporary directory alive alongside the new handle.
    fn reopen(self) -> (super::tempfile::TempDir, Store) {
        let Self {
            directory,
            path,
            store,
            nodes: _,
        } = self;
        store.close().expect("close native store");
        let reopened =
            Store::open_native_graph(&path, fixture_options(), None).expect("reopen native store");
        (directory, reopened)
    }
}

/// One committed node carrying the shared property fixture and its own text.
fn commit_node(store: &Store, key: &str) -> NodeId {
    let mut labels = [GraphName::new("Document").expect("label")];
    let mut properties = super::publication::property_fixture();
    let image = CanonicalContents::node(&mut labels, &mut properties, Some(DOCUMENT_TEXT), None)
        .expect("node image");
    let receipts = store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", key).expect("node key"),
                revision: GraphRevision::new(1).expect("node revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(&image)),
            }],
            &control(),
        )
        .unwrap_or_else(|error| panic!("commit {key}: {error:?}"));
    match receipts[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("commit {key} returned a relationship"),
    }
}

/// The current published graph generation.
fn published_generation(store: &Store) -> GraphGeneration {
    store
        .admit_native_read()
        .expect("native read lease")
        .bundle()
        .base()
        .generation
}

/// Every live node the admitted read view reports, in scan order.
struct ScanNodes;

impl NativeReadConsumer<Vec<NodeId>> for ScanNodes {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<Vec<NodeId>, TreeError> {
        scan_all(view, runtime)
    }
}

/// Drains one label-free node cursor to exhaustion.
fn scan_all<'s, 'lease, 'm, 'g>(
    view: &GraphReadView<'s, 'lease, 'm, 'g>,
    runtime: &mut RuntimeContext<'lease, 'm, 'g>,
) -> Result<Vec<NodeId>, TreeError> {
    let placeholder = NodeId::new(1).map_err(|_| TreeError::Invalid("placeholder node"))?;
    let mut cursor = view.node_cursor(LabelSelection::All, runtime)?;
    let mut observed = Vec::new();
    loop {
        let mut batch = [placeholder; 4];
        let (count, state) = view.scan_nodes(&mut cursor, &mut batch, runtime)?;
        observed.extend_from_slice(batch.get(..count).unwrap_or(&[]));
        if state == CursorState::Done {
            break;
        }
    }
    observed.sort_by_key(|node| node.get());
    Ok(observed)
}

/// One statement: scan every node through the read view, then optionally
/// stage one deletion through the writer overlay. Nothing here builds an
/// image; D1 admits the two halves, and D2 onward supplies `Mutate`.
///
/// `invocations` is borrowed from the caller because the admission consumes
/// the consumer, and a retried or rejected attempt must still report how many
/// times it ran.
struct ScanThenStage<'a> {
    /// The node to tombstone, or `None` to stage nothing at all.
    target: Option<NodeId>,
    /// Cancelled after the scan, before anything is staged.
    cancel: Option<&'a CancelToken>,
    /// How many times the admission actually ran this consumer.
    invocations: &'a std::cell::Cell<u64>,
}

impl<'a> ScanThenStage<'a> {
    const fn reading(invocations: &'a std::cell::Cell<u64>) -> Self {
        Self {
            target: None,
            cancel: None,
            invocations,
        }
    }

    const fn deleting(invocations: &'a std::cell::Cell<u64>, target: NodeId) -> Self {
        Self {
            target: Some(target),
            cancel: None,
            invocations,
        }
    }

    const fn cancelling(invocations: &'a std::cell::Cell<u64>, token: &'a CancelToken) -> Self {
        Self {
            target: None,
            cancel: Some(token),
            invocations,
        }
    }
}

impl NativeMutationConsumer<Vec<NodeId>> for ScanThenStage<'_> {
    fn consume<'lease, 'm, 'g, 'w, 'i>(
        &mut self,
        view: &'w GraphReadView<'w, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
        mut overlay: GraphBatchReadView<'w, 'static>,
        _images: &'w StatementImages<'i>,
        control: &mut WriteControl<'_>,
    ) -> Result<(Vec<NodeId>, GraphBatchReadView<'w, 'static>), NativeExecutionError> {
        self.invocations.set(self.invocations.get() + 1);
        // The read view has to be live under the writer lease: this is a real
        // cursor scan against the admitted generation, not a replay of a
        // caller-supplied request list.
        let observed = scan_all(view, runtime)?;
        if let Some(token) = self.cancel {
            token.cancel();
            // A cancelled statement stops here, before it stages anything.
            runtime.checkpoint()?;
        }
        if let Some(target) = self.target {
            overlay.delete(
                BatchEntityRef::Node(NodeRef::Existing(target)),
                GraphDeleteMode::Restrict,
                control,
            )?;
        }
        Ok((observed, overlay))
    }
}

/// Runs one consumer through the admission under this module's defaults.
fn admit(
    store: &Store,
    control: &QueryControl,
    consumer: ScanThenStage<'_>,
) -> Result<(Vec<NodeId>, NativeMutationReport), NativeMutationError> {
    store.with_native_mutation(
        control,
        RuntimeLimits::default(),
        4 * 1024 * 1024,
        16,
        8,
        8,
        8,
        consumer,
    )
}

/// Sorted live node ids as the public read path reports them.
fn live_nodes(store: &Store) -> Vec<NodeId> {
    store
        .with_native_read(
            &control(),
            RuntimeLimits::default(),
            4 * 1024 * 1024,
            16,
            ScanNodes,
        )
        .expect("scoped native read")
}

/// Builds a read-only lazy admitted base over `store` and asks it whether
/// `node` is still a live entity. This never goes through the writer, so it
/// answers from the published roots alone.
fn lazily_admitted_entity_exists(store: &Store, node: NodeId) -> bool {
    let lease = store.admit_native_read().expect("native read lease");
    let shared = GraphResources::from_store(store).expect("graph resources");
    let writer = WriteMemory::new(&shared, WriteLimits::default()).expect("write memory");
    let control = control();
    let storage = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).expect("storage memory");
    let source = NativePreparationSource::new(&lease, &storage, 32).expect("preparation source");
    let mut resources = source.resources(64 * 1024 * 1024).expect("tree resources");
    let cell = std::cell::RefCell::new(&mut resources);
    let error = std::cell::Cell::new(None);
    let base =
        NativeAdmittedBase::with_lazy_targets(&lease, &source, &storage, &[], &cell, &error, 4)
            .expect("lazy admitted base");
    base.entity(EntityId::Node(node), &mut |_| Ok(()))
        .expect("lazy entity lookup")
        .is_some()
}

/// A query-driven statement scans the live graph under the writer lease,
/// stages one deletion through the overlay, and reaches durability through
/// the same commit tail the structured path uses.
#[test]
fn ze52_slice_d1_query_admission_deletes_a_scanned_node_and_reopens() {
    let fixture = MutationFixture::commit();
    let [first, second, third] = fixture.nodes;
    let admitted_before = published_generation(&fixture.store);
    assert_eq!(admitted_before.get(), 3);

    let invocations = std::cell::Cell::new(0);
    let (scanned, report) = admit(
        &fixture.store,
        &control(),
        ScanThenStage::deleting(&invocations, first),
    )
    .expect("query-driven mutation");

    // The read view was live: the consumer saw every committed node, not an
    // empty base.
    assert_eq!(invocations.get(), 1);
    assert_eq!(scanned, sorted([first, second, third]));
    assert_eq!(report.disposition, BatchDisposition::Changed);
    assert_eq!(report.admitted, admitted_before);
    assert_eq!(
        report.changed.map(GraphGeneration::get),
        Some(admitted_before.get() + 1)
    );
    // The scan is real work under the writer lease, not a replayed list.
    assert!(report.counters.get(WorkKind::CopiedBytes) > 0);

    // The deletion is publicly visible before the reopen.
    assert_eq!(live_nodes(&fixture.store), sorted([second, third]));

    let (_directory, reopened) = fixture.reopen();
    assert_eq!(published_generation(&reopened).get(), 4);
    assert_eq!(live_nodes(&reopened), sorted([second, third]));
    assert!(!lazily_admitted_entity_exists(&reopened, first));
    assert!(lazily_admitted_entity_exists(&reopened, second));
    reopened.close().expect("close reopened store");
}

/// A statement that stages nothing is a successful NoOp: no generation, no
/// WAL envelope, and every capacity the admission charged is released.
#[test]
fn ze52_slice_d1_query_admission_without_changes_is_noop() {
    let fixture = MutationFixture::commit();
    let [first, second, third] = fixture.nodes;
    let admitted_before = published_generation(&fixture.store);
    let shared = GraphResources::from_store(&fixture.store).expect("graph resources");
    let baseline_reserved = shared.reserved_bytes().expect("baseline reservation");
    let baseline_queries = fixture
        .store
        .active_queries
        .load(std::sync::atomic::Ordering::Relaxed);

    let invocations = std::cell::Cell::new(0);
    let (scanned, report) = admit(
        &fixture.store,
        &control(),
        ScanThenStage::reading(&invocations),
    )
    .expect("read-only query-driven mutation");

    assert_eq!(invocations.get(), 1);
    assert_eq!(scanned, sorted([first, second, third]));
    assert_eq!(report.disposition, BatchDisposition::NoOp);
    assert_eq!(report.admitted, admitted_before);
    assert_eq!(report.changed, None);

    assert_eq!(published_generation(&fixture.store), admitted_before);
    assert_eq!(live_nodes(&fixture.store), sorted([first, second, third]));

    // Every admission-scoped charge returned to its baseline.
    assert_eq!(
        shared
            .reserved_bytes()
            .expect("reservation after the admission"),
        baseline_reserved
    );
    assert_eq!(
        fixture
            .store
            .active_queries
            .load(std::sync::atomic::Ordering::Relaxed),
        baseline_queries
    );

    let (_directory, reopened) = fixture.reopen();
    assert_eq!(published_generation(&reopened), admitted_before);
    assert_eq!(live_nodes(&reopened), sorted([first, second, third]));
    reopened.close().expect("close reopened store");
}

/// Cancellation inside the consumer rejects the whole statement before the
/// commit tail runs: no generation, no value, and no retained charge.
#[test]
fn ze52_slice_d1_query_admission_rejects_before_commit_on_cancel() {
    let fixture = MutationFixture::commit();
    let [first, second, third] = fixture.nodes;
    let admitted_before = published_generation(&fixture.store);
    let shared = GraphResources::from_store(&fixture.store).expect("graph resources");
    let baseline_reserved = shared.reserved_bytes().expect("baseline reservation");
    let baseline_queries = fixture
        .store
        .active_queries
        .load(std::sync::atomic::Ordering::Relaxed);

    let token = CancelToken::new();
    let control = QueryControl::Cancel(token.clone());
    let invocations = std::cell::Cell::new(0);
    let rejected = admit(
        &fixture.store,
        &control,
        ScanThenStage::cancelling(&invocations, &token),
    );

    assert_eq!(invocations.get(), 1);
    match rejected {
        Err(NativeMutationError::Execution(NativeExecutionError::Runtime(
            RuntimeError::Value(crate::property_graph::query::QueryError::Cancelled),
        ))) => {}
        Ok((value, report)) => panic!(
            "cancelled mutation committed {value:?} at {:?}",
            report.changed
        ),
        Err(other) => panic!("cancelled mutation: {other:?}"),
    }

    // Nothing was published and nothing stayed charged.
    assert_eq!(published_generation(&fixture.store), admitted_before);
    assert_eq!(
        shared
            .reserved_bytes()
            .expect("reservation after the rejection"),
        baseline_reserved
    );
    assert_eq!(
        fixture
            .store
            .active_queries
            .load(std::sync::atomic::Ordering::Relaxed),
        baseline_queries
    );

    // A fresh control still works, so the rejection left the writer usable.
    assert_eq!(live_nodes(&fixture.store), sorted([first, second, third]));
    let (_directory, reopened) = fixture.reopen();
    assert_eq!(published_generation(&reopened), admitted_before);
    assert_eq!(live_nodes(&reopened), sorted([first, second, third]));
    reopened.close().expect("close reopened store");
}

/// The commit tail may checkpoint instead of committing. The admission then
/// rebuilds the whole attempt, so the consumer runs a second time and exactly
/// one deletion reaches durability.
#[test]
fn ze52_slice_d1_query_admission_reruns_consumer_after_checkpoint() {
    let fixture = MutationFixture::commit();
    let [first, second, third] = fixture.nodes;
    // Three creates already happened; drive the writer to exactly the 64
    // complete envelopes that make the next commit checkpoint first.
    for revision in 2..=62_u64 {
        let mut labels = [GraphName::new("Document").expect("label")];
        let mut properties = super::publication::property_fixture();
        let text = format!("revision {revision}");
        let image = CanonicalContents::node(&mut labels, &mut properties, Some(&text), None)
            .expect("checkpoint filler image");
        let receipt = fixture
            .store
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "app", "third").expect("filler key"),
                    revision: GraphRevision::new(revision).expect("filler revision"),
                    operation: StructuredOperation::Put(EntityId::Node(third)),
                    image: Some(WriteImage::Node(&image)),
                }],
                &control(),
            )
            .expect("filler commit");
        assert!(!receipt[0].replayed);
    }
    let admitted_before = published_generation(&fixture.store);
    assert_eq!(admitted_before.get(), 64);

    let invocations = std::cell::Cell::new(0);
    let (scanned, report) = admit(
        &fixture.store,
        &control(),
        ScanThenStage::deleting(&invocations, first),
    )
    .expect("query-driven mutation across a checkpoint");

    // The first attempt checkpointed instead of committing, so the whole
    // statement, including its scan, ran again.
    assert_eq!(invocations.get(), 2);
    assert_eq!(scanned, sorted([first, second, third]));
    assert_eq!(report.disposition, BatchDisposition::Changed);
    assert_eq!(report.admitted, admitted_before);
    assert_eq!(report.changed.map(GraphGeneration::get), Some(65));

    // Exactly one deletion committed.
    assert_eq!(live_nodes(&fixture.store), sorted([second, third]));
    let (_directory, reopened) = fixture.reopen();
    assert_eq!(published_generation(&reopened).get(), 65);
    assert_eq!(live_nodes(&reopened), sorted([second, third]));
    assert!(!lazily_admitted_entity_exists(&reopened, first));
    reopened.close().expect("close reopened store");
}

/// PIN TEST. The structured-write path is the code slice D1 splits its commit
/// tail out of. This test never touches the new entry point: it drives
/// `apply_native_graph` through create, put, exact retry, delete and a
/// no-change statement, and pins the exact receipt shape and generation
/// sequence each one returns, plus the reopened public state.
///
/// Entity identities come from OS entropy, so the pin is over revisions,
/// generations, replay classification and the identities the create receipts
/// themselves reported, never over absolute id values.
#[test]
fn ze52_slice_d1_structured_path_is_unchanged() {
    let fixture = MutationFixture::commit();
    let [first, second, third] = fixture.nodes;
    let store = &fixture.store;

    // Three creates, one generation each.
    assert_eq!(published_generation(store).get(), 3);
    assert_eq!(live_nodes(store), sorted([first, second, third]));

    let key = |name: &'static str| {
        ApplicationKey::new(EntityKind::Node, "app", name).expect("structured key")
    };
    let mut labels = [GraphName::new("Document").expect("label")];
    let mut properties = super::publication::property_fixture();
    let replacement =
        CanonicalContents::node(&mut labels, &mut properties, Some("replaced text"), None)
            .expect("replacement image");

    // A changed PUT advances one generation and is not a replay.
    let put = [StructuredWrite {
        key: key("first"),
        revision: GraphRevision::new(2).expect("revision"),
        operation: StructuredOperation::Put(EntityId::Node(first)),
        image: Some(WriteImage::Node(&replacement)),
    }];
    let changed = store.apply_native_graph(&put, &control()).expect("put");
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].entity, EntityId::Node(first));
    assert_eq!(changed[0].revision.get(), 2);
    assert_eq!(changed[0].generation.get(), 4);
    assert!(!changed[0].replayed);

    // The identical PUT is an exact retry: same generation, replayed, and no
    // new published generation.
    let retried = store
        .apply_native_graph(&put, &control())
        .expect("exact retry");
    assert_eq!(retried.len(), 1);
    assert_eq!(retried[0].entity, EntityId::Node(first));
    assert_eq!(retried[0].revision.get(), 2);
    assert_eq!(retried[0].generation.get(), 4);
    assert!(retried[0].replayed);
    assert_eq!(published_generation(store).get(), 4);

    // A DELETE advances one generation and removes the node publicly.
    let delete = [StructuredWrite {
        key: key("second"),
        revision: GraphRevision::new(2).expect("revision"),
        operation: StructuredOperation::Delete(EntityId::Node(second), GraphDeleteMode::Restrict),
        image: None,
    }];
    let removed = store
        .apply_native_graph(&delete, &control())
        .expect("delete");
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].entity, EntityId::Node(second));
    assert_eq!(removed[0].revision.get(), 2);
    assert_eq!(removed[0].generation.get(), 5);
    assert!(!removed[0].replayed);

    // An empty statement is a no-op with no receipts and no generation.
    assert!(
        store
            .apply_native_graph(&[], &control())
            .expect("empty statement")
            .is_empty()
    );
    assert_eq!(published_generation(store).get(), 5);
    assert_eq!(live_nodes(store), sorted([first, third]));

    // The same state survives a reopen.
    let (_directory, reopened) = fixture.reopen();
    assert_eq!(published_generation(&reopened).get(), 5);
    assert_eq!(live_nodes(&reopened), sorted([first, third]));
    reopened.close().expect("close reopened store");
}

/// One committed node whose stored text is large enough that reading its
/// canonical image back dominates the work one statement charges.
const WIDE_TEXT_BYTES: usize = 64 * 1024;

/// Preparation work budgets that land inside the canonical stream comparison
/// `overlay.finish` runs, measured on this fixture: the statement completes at
/// and above 1,495,000 units, and fails inside the consumer's own base read at
/// and below 1,360,000. Every budget between those bounds stops in
/// `CachedCanonical::read_at`, which stashes the typed `TreeError` and returns
/// only an opaque `io::Error` to its caller. These five sit at least 15,000
/// units inside both edges of that window.
const CANONICAL_READ_WORK_BUDGETS: [u64; 5] =
    [1_380_000, 1_405_000, 1_430_000, 1_455_000, 1_480_000];

/// A budget the same statement completes under, proving the window above is a
/// storage failure and not an unconditional rejection.
const SUFFICIENT_WORK_BUDGET: u64 = 4 * 1024 * 1024;

/// A node image owned for the rest of the process. The admission hands its
/// consumer an overlay whose lifetime is chosen by the caller, so a replacement
/// image has to outlive every possible choice.
fn retained_wide_image() -> WriteImage<'static, 'static> {
    let text: &'static str = Box::leak("wide".repeat(WIDE_TEXT_BYTES / 4).into_boxed_str());
    let labels: &'static mut [GraphName<'static>] =
        Box::leak(Box::new([GraphName::new("Document").expect("wide label")]));
    let properties: &'static mut [GraphProperty<'static>] =
        Box::leak(super::publication::property_fixture().into_boxed_slice());
    let contents: &'static CanonicalContents<'static> = Box::leak(Box::new(
        CanonicalContents::node(labels, properties, Some(text), None).expect("wide node image"),
    ));
    WriteImage::Node(contents)
}

/// Stages one full-image replacement of an already committed node. The final
/// image equals the committed one, so finalization compares the two canonical
/// streams byte for byte and therefore reads the admitted base's stored image
/// back through `CachedCanonical::read_at`.
struct ReplaceWithSameImage {
    target: NodeId,
    image: WriteImage<'static, 'static>,
}

impl NativeMutationConsumer<()> for ReplaceWithSameImage {
    fn consume<'lease, 'm, 'g, 'w, 'i>(
        &mut self,
        _view: &'w GraphReadView<'w, 'lease, 'm, 'g>,
        _runtime: &mut RuntimeContext<'lease, 'm, 'g>,
        mut overlay: GraphBatchReadView<'w, 'static>,
        _images: &'w StatementImages<'i>,
        control: &mut WriteControl<'_>,
    ) -> Result<((), GraphBatchReadView<'w, 'static>), NativeExecutionError> {
        overlay.replace(
            BatchEntityRef::Node(NodeRef::Existing(self.target)),
            self.image,
            control,
        )?;
        Ok(((), overlay))
    }
}

/// A canonical read that fails inside the admitted base surfaces as the typed
/// lifecycle storage error, not as the opaque canonical I/O rejection it
/// caused.
///
/// `CachedCanonical::read_at` is the only writer of the base's error stash: it
/// records the real `TreeError` and hands its caller `io::ErrorKind::Other`,
/// which staging classifies as a canonical-comparison failure. The admission
/// must therefore read the stash before applying `?` to the call it guards.
/// Checking afterwards is unreachable for exactly the case the stash exists
/// for, and reports `Stage(Lifecycle(Canonical(Io(Other))))` with the root
/// cause discarded.
#[test]
fn ze52_slice_d1_canonical_read_failure_surfaces_as_the_typed_storage_error() {
    let directory = super::tempfile::tempdir().expect("temporary parent");
    let path = directory.path().join("canonical-read-failure");
    let store =
        Store::create_native_graph(&path, fixture_options(), None).expect("create native store");
    let image = retained_wide_image();
    let WriteImage::Node(contents) = image else {
        panic!("wide image is a node image");
    };
    let receipts = store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", "wide").expect("wide key"),
                revision: GraphRevision::new(1).expect("wide revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(contents)),
            }],
            &control(),
        )
        .expect("commit the wide node");
    let target = match receipts[0].entity {
        EntityId::Node(node) => node,
        EntityId::Relationship(_) => panic!("wide commit returned a relationship"),
    };
    let committed = published_generation(&store);

    let replace = |work_limit: u64| {
        let _schedule = crate::property_graph::storage::search::native_vector_index_test_schedule(
            None,
            Some(work_limit),
            |_| {},
        );
        store.with_native_mutation(
            &control(),
            RuntimeLimits::default(),
            4 * 1024 * 1024,
            16,
            8,
            8,
            8,
            ReplaceWithSameImage { target, image },
        )
    };

    // The same statement is a successful NoOp when the base can be read, so
    // the window below fails on storage and on nothing else.
    let (value, report) = replace(SUFFICIENT_WORK_BUDGET).expect("unbudgeted replacement");
    assert_eq!(value, ());
    assert_eq!(report.disposition, BatchDisposition::NoOp);
    assert_eq!(report.changed, None);

    for work_limit in CANONICAL_READ_WORK_BUDGETS {
        let rejected = replace(work_limit);
        match rejected {
            Err(NativeMutationError::Graph(NativeGraphError::Stage(
                StageError::NativeStorage(TreeError::Work),
            ))) => {}
            Ok((_, report)) => panic!(
                "a canonical read at {work_limit} units committed {:?}",
                report.changed
            ),
            Err(other) => panic!("a canonical read at {work_limit} units: {other:?}"),
        }
        // Every rejection leaves the writer usable and publishes nothing.
        assert_eq!(published_generation(&store), committed);
    }

    assert_eq!(live_nodes(&store), vec![target]);
    store.close().expect("close native store");
}

fn sorted<const N: usize>(mut nodes: [NodeId; N]) -> Vec<NodeId> {
    nodes.sort_by_key(|node| node.get());
    nodes.to_vec()
}
