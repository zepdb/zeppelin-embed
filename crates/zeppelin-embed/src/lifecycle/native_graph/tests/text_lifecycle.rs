//! ZE-170: `StoredText(NodeRef)` across a maintenance generation, a concurrent
//! publication and store close.
//!
//! ZE-145 proved `StoredText` evaluation on a freshly written graph, and
//! `graph_query_storage.rs` proves payload charging. Nothing proved the value a
//! caller actually receives when ZE-46 bounded consolidation relocates the node
//! record underneath a retained read view, when the retired pack is physically
//! unlinked, when a concurrent publication replaces the text, or when the store
//! closes while a copy is in flight.
//!
//! Four text shapes are carried through every stage, because they take
//! different paths through `storage/view.rs::stored_text` and
//! `query/expression.rs::copy_text_reader`:
//!
//! - `A`: present, multi-chunk (more than two `CHUNK_BYTES` reads).
//! - `E`: present and empty, which is not absence.
//! - `Z`: present, short, and containing an interior zero byte.
//! - `N`: absent, which is `Null`.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use super::*;
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::native_graph::maintenance::NativeMaintenanceReport;
use crate::lifecycle::native_graph::tests::consolidation::node_directory_value;
use crate::property_graph::GraphDeleteMode;
use crate::property_graph::query::expression::{
    ExpressionCapacity, ExpressionFailure, NativeExpressionEvaluator,
};
use crate::property_graph::query::plan::{
    ExprId, Expression, NodeFacts, Operator, OperatorKind, PlanBacking, PlanDescription,
    PlanFootprint, PlanNodeId, Projection, RetainedRegion, SlotId, UnaryExpression,
    VALIDATION_SCRATCH_BYTES,
};
use crate::property_graph::query::relational::Schema;
use crate::property_graph::query::resources::{
    QueryArena, QueryInputs, RetainedAllocation, RetentionInventory,
};
use crate::property_graph::query::runtime::{RowBatch, RuntimeError};
use crate::property_graph::query::{QueryError, QueryValue};
use crate::property_graph::storage::payload::{CHUNK_BYTES, PayloadRef};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// `"text-猫"` is eight bytes, so this repeat count is just over two chunks and
/// forces `copy_text_reader` to loop three times.
fn multi_chunk_text() -> String {
    "text-猫".repeat(2 * CHUNK_BYTES / 7 + 17)
}

/// Short text with an interior zero byte: a reader that stopped at a zero
/// terminator would return `"!!"` instead of the whole five bytes.
const ZERO_TERM_TEXT: &str = "!!\u{0}!!";

const ANCHOR_TEXT: &str = "anchor";

fn fixture_options() -> OpenOptions {
    OpenOptions::new()
        .with_durability(DurabilityMode::Durable, CommitTier::Durable)
        .with_max_resident_bytes(256 * 1024 * 1024)
}

fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}

/// One committed store holding the four text shapes in the first pack and one
/// anchor node in a second pack, so a read view can map one pack without
/// mapping the other.
struct TextFixture {
    _directory: super::tempfile::TempDir,
    path: PathBuf,
    store: Arc<Store>,
    a: NodeId,
    e: NodeId,
    z: NodeId,
    n: NodeId,
    b: NodeId,
    text: String,
}

impl TextFixture {
    /// The five node ids in commit order; every one of them is relocatable.
    fn nodes(&self) -> [NodeId; 5] {
        [self.a, self.e, self.z, self.n, self.b]
    }

    fn reopen(&mut self) {
        self.store.close().expect("close ze170 store");
        assert_eq!(
            Arc::strong_count(&self.store),
            1,
            "reopen requires the fixture to hold the only store handle"
        );
        self.store = Arc::new(
            Store::open_native_graph(&self.path, fixture_options(), None)
                .expect("reopen ze170 store"),
        );
    }
}

fn node_id(receipt: &crate::property_graph::staging::ItemReceipt) -> NodeId {
    match receipt.entity {
        EntityId::Node(id) => id,
        EntityId::Relationship(_) => panic!("ze170 fixture receipt is not a node"),
    }
}

/// Two `apply_native_graph` batches, so `A`, `E`, `Z` and `N` live in one pack
/// and the anchor `B` lives in another.
fn fixture(namespace: &str) -> TextFixture {
    let directory = super::tempfile::tempdir().expect("ze170 store directory");
    let path = directory.path().join("native");
    let store = Arc::new(
        Store::create_native_graph(&path, fixture_options(), None)
            .expect("create ze170 native graph"),
    );
    let text = multi_chunk_text();

    let first = with_local_refs(|_refs| {
        let mut labels_a = [GraphName::new("Text").expect("ze170 label")];
        let mut labels_e = [GraphName::new("Text").expect("ze170 label")];
        let mut labels_z = [GraphName::new("Text").expect("ze170 label")];
        let mut labels_n = [GraphName::new("Text").expect("ze170 label")];
        let node_a = CanonicalContents::node(&mut labels_a, &mut [], Some(text.as_str()), None)
            .expect("ze170 multi-chunk node");
        let node_e = CanonicalContents::node(&mut labels_e, &mut [], Some(""), None)
            .expect("ze170 present-empty node");
        let node_z = CanonicalContents::node(&mut labels_z, &mut [], Some(ZERO_TERM_TEXT), None)
            .expect("ze170 zero-byte node");
        let node_n =
            CanonicalContents::node(&mut labels_n, &mut [], None, None).expect("ze170 absent node");
        let images = [
            ("a", &node_a),
            ("e", &node_e),
            ("z", &node_z),
            ("n", &node_n),
        ];
        let writes = images
            .iter()
            .map(|(key, contents)| StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, namespace, key).expect("ze170 key"),
                revision: GraphRevision::new(1).expect("ze170 revision"),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(contents)),
            })
            .collect::<Vec<StructuredWrite<'_, '_>>>();
        store
            .apply_native_graph(&writes, &control())
            .expect("publish ze170 text shapes")
    });

    let second = with_local_refs(|_refs| {
        let mut labels_b = [GraphName::new("Anchor").expect("ze170 anchor label")];
        let node_b = CanonicalContents::node(&mut labels_b, &mut [], Some(ANCHOR_TEXT), None)
            .expect("ze170 anchor node");
        store
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, namespace, "b")
                        .expect("ze170 anchor key"),
                    revision: GraphRevision::new(1).expect("ze170 anchor revision"),
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&node_b)),
                }],
                &control(),
            )
            .expect("publish ze170 anchor")
    });

    assert_eq!(first.len(), 4, "ze170 first batch receipts");
    assert_eq!(second.len(), 1, "ze170 second batch receipts");
    TextFixture {
        _directory: directory,
        path,
        store,
        a: node_id(&first[0]),
        e: node_id(&first[1]),
        z: node_id(&first[2]),
        n: node_id(&first[3]),
        b: node_id(&second[0]),
        text,
    }
}

// ---------------------------------------------------------------------------
// Maintenance driver and physical record identity
// ---------------------------------------------------------------------------

/// Commits one maintenance generation and returns the whole report. A
/// `StalePreparation` refusal means a concurrent change invalidated the
/// admitted base, which is a re-admit and retry; every other error is a real
/// failure. Bounded to two attempts, exactly as ZE-169 does.
fn maintain_once(store: &Store, call: usize) -> NativeMaintenanceReport {
    for attempt in 0..2 {
        let admission = store
            .admit_native_graph_maintenance()
            .expect("ze170 maintenance admission");
        match store.commit_native_graph_maintenance(&admission, &control()) {
            Ok(report) => return report,
            Err(NativeGraphError::StalePreparation) => {
                assert!(attempt == 0, "call {call} stayed stale across two attempts");
            }
            Err(error) => panic!("ze170 maintenance call {call} failed: {error:?}"),
        }
    }
    panic!("ze170 maintenance call {call} never committed")
}

/// The raw node-directory value of every id, read through ZE-46's own helper.
fn node_records(store: &Store, nodes: &[NodeId]) -> Vec<Vec<u8>> {
    let lease = store.admit_native_read().expect("ze170 record reader");
    nodes
        .iter()
        .map(|node| node_directory_value(store, &lease, *node))
        .collect()
}

/// The file that currently holds `node`'s physical record. Maintenance
/// relocates the record into a new pack and eventually unlinks the old file, so
/// a path captured before maintenance names the file whose disappearance proves
/// the reclaim actually ran.
fn record_artifact_path(
    store: &Store,
    lease: &super::super::NativeReadLease,
    directory: &Path,
    node: NodeId,
) -> PathBuf {
    let value = node_directory_value(store, lease, node);
    let payload = PayloadRef::decode(&value).expect("ze170 node record descriptor");
    artifact_path(directory, payload.reference().artifact)
}

// ---------------------------------------------------------------------------
// StoredText evaluation
// ---------------------------------------------------------------------------

/// The operand handed to `StoredText`. `Null` and `Number` exist because the
/// plan validator only sees a NODE-kind slot; the runtime value decides.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Operand {
    Node(NodeId),
    Null,
    Number,
}

/// A `StoredText` result in a form that can be compared across stages. The
/// failure text is kept verbatim so an unexpected error is diagnosable rather
/// than merely unequal.
#[derive(Clone, Debug, Eq, PartialEq)]
enum TextOutcome {
    /// `Some` is present text (possibly empty); `None` is SQL-style `Null`.
    Text(Option<String>),
    /// `StoredText` of a non-node, non-null value.
    TypeError,
    /// The node is absent or tombstoned.
    Absent,
    /// The read view was cancelled, typically by `close()`.
    Cancelled,
    Other(String),
}

impl TextOutcome {
    fn text(value: &str) -> Self {
        Self::Text(Some(value.to_string()))
    }
}

fn outcome(result: Result<Option<String>, ExpressionFailure>) -> TextOutcome {
    match result {
        Ok(text) => TextOutcome::Text(text),
        Err(ExpressionFailure::Runtime(RuntimeError::Value(QueryError::Type))) => {
            TextOutcome::TypeError
        }
        Err(ExpressionFailure::Tree(TreeError::Invalid(
            "stored text entity is absent or deleted",
        ))) => TextOutcome::Absent,
        Err(ExpressionFailure::Tree(TreeError::Runtime(RuntimeError::Value(
            QueryError::ReadCancelled,
        )))) => TextOutcome::Cancelled,
        Err(other) => TextOutcome::Other(format!("{other:?}")),
    }
}

/// Builds one `Project(StoredText(Slot 0))` plan and evaluates it once per
/// operand against a hand-built input batch.
///
/// The operator tree exists only so plan validation can type slot 0 as a node;
/// it is never executed. `interlude(row, view)` runs immediately before the
/// evaluation of `operands[row]`, which is how the close-during-copy case
/// cancels the view between two reads that share one admitted plan.
fn stored_text_reads<'s, 'lease, 'm, 'g>(
    view: &GraphReadView<'s, 'lease, 'm, 'g>,
    runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    operands: &[Operand],
    mut interlude: impl FnMut(usize, &GraphReadView<'s, 'lease, 'm, 'g>),
) -> Vec<TextOutcome> {
    assert!(!operands.is_empty(), "ze170 reads at least one operand");
    let witness = operands
        .iter()
        .find_map(|operand| match operand {
            Operand::Node(node) => Some(*node),
            _ => None,
        })
        .unwrap_or_else(|| NodeId::new(1).expect("ze170 witness identity"));

    let expressions = [
        Expression::Slot(SlotId(0)),
        Expression::Unary {
            operation: UnaryExpression::StoredText,
            operand: ExprId(0),
        },
    ];
    let projections = [Projection {
        slot: SlotId(1),
        expression: ExprId(1),
    }];
    let lookup_input = [PlanNodeId(0)];
    let project_input = [PlanNodeId(1)];
    let operators = [
        Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        },
        Operator {
            inputs: &lookup_input,
            kind: OperatorKind::LookupNode {
                output: SlotId(0),
                id: witness,
            },
        },
        Operator {
            inputs: &project_input,
            kind: OperatorKind::Project(&projections),
        },
    ];
    let mut facts = QueryArena::new(runtime.memory(), operators.len()).expect("ze170 plan facts");
    for _ in &operators {
        facts
            .push(NodeFacts::default())
            .expect("ze170 plan facts row");
    }
    let mut regions = [
        RetainedRegion::slice(&operators).unwrap(),
        RetainedRegion::slice(&lookup_input).unwrap(),
        RetainedRegion::slice(&project_input).unwrap(),
        RetainedRegion::slice(&projections).unwrap(),
        RetainedRegion::slice(&expressions).unwrap(),
        RetainedRegion::declared(facts.as_slice().as_ptr() as usize, facts.heap_bytes()).unwrap(),
    ];
    regions.sort();
    let external_bytes = regions
        .iter()
        .map(|region| region.end() - region.start())
        .sum::<usize>()
        + std::mem::size_of_val(&regions)
        + std::mem::size_of::<PlanDescription<'_>>()
        + VALIDATION_SCRATCH_BYTES;
    let mut plan_capacity = runtime
        .memory()
        .reserve_external_capacity()
        .expect("ze170 plan capacity");
    plan_capacity
        .reserve_additional(external_bytes)
        .expect("ze170 plan capacity reserve");
    let description = PlanDescription {
        operators: &operators,
        expressions: &expressions,
        parameters: &[],
        root: PlanNodeId(2),
        eager_searches: &[],
    };
    let footprint = PlanFootprint::declared(runtime.memory().reserved_bytes());
    let (plan, facts_owner) = facts
        .validate_plan(
            description,
            footprint,
            PlanBacking::new(&regions, std::mem::size_of_val(&regions)).unwrap(),
            runtime.values(),
        )
        .expect("ze170 validate stored-text plan");
    let owners = [
        RetainedAllocation::array(&operators).unwrap(),
        RetainedAllocation::array(&lookup_input).unwrap(),
        RetainedAllocation::array(&project_input).unwrap(),
        RetainedAllocation::array(&projections).unwrap(),
        RetainedAllocation::array(&expressions).unwrap(),
        facts_owner,
    ];
    let runtime_plan = QueryInputs::reserve(
        runtime.memory(),
        RetentionInventory::array(&owners),
        runtime.values(),
    )
    .expect("ze170 reserve stored-text plan")
    .admit_plan(&plan, runtime.values())
    .expect("ze170 admit stored-text plan");

    let schema = Schema::new(runtime, &[SlotId(0)]).expect("ze170 stored-text schema");
    // Every operand is a scalar cell; a node reference costs sixteen bytes and
    // is the widest of them.
    let mut input = RowBatch::new(runtime, 1, operands.len(), 16 * operands.len())
        .expect("ze170 stored-text input batch");
    for operand in operands {
        let value = match operand {
            Operand::Node(node) => runtime.view().node(*node),
            Operand::Null => QueryValue::Null,
            Operand::Number => QueryValue::I64(7),
        };
        input
            .push_row(&[value], runtime)
            .expect("ze170 stored-text input row");
    }
    let mut evaluator = NativeExpressionEvaluator::new(
        &runtime_plan,
        &[],
        ExpressionCapacity {
            cells: 4,
            string_bytes: 3 * CHUNK_BYTES,
        },
        runtime,
    )
    .expect("ze170 stored-text evaluator");

    let mut results = Vec::with_capacity(operands.len());
    for row in 0..operands.len() {
        interlude(row, view);
        // The `&str` is borrowed from the evaluator's scratch arena, so it is
        // copied into an owned `String` before the next evaluation reuses it.
        let result = evaluator
            .evaluate(ExprId(1), &schema, &input, row, view, runtime)
            .map(|value| match value {
                QueryValue::Null => None,
                QueryValue::String(text) => Some(text.to_string()),
                _ => panic!("ze170 stored text produced a non-string, non-null value"),
            })
            .map_err(|error| error.failure);
        results.push(outcome(result));
    }
    results
}

/// Opens a fresh read view and evaluates `StoredText` for every operand.
struct PlainReads<'a> {
    operands: &'a [Operand],
}

impl NativeReadConsumer<Vec<TextOutcome>> for PlainReads<'_> {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<Vec<TextOutcome>, TreeError> {
        Ok(stored_text_reads(view, runtime, self.operands, |_, _| {}))
    }
}

fn read_fresh(store: &Store, operands: &[Operand]) -> Vec<TextOutcome> {
    store
        .with_native_read(
            &control(),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            16,
            PlainReads { operands },
        )
        .expect("ze170 stored-text read")
}

/// The four text shapes plus the two non-node operands, in a fixed order.
fn shape_operands(fixture: &TextFixture) -> Vec<Operand> {
    vec![
        Operand::Node(fixture.a),
        Operand::Node(fixture.e),
        Operand::Node(fixture.z),
        Operand::Node(fixture.n),
        Operand::Null,
        Operand::Number,
    ]
}

fn expected_shapes(fixture: &TextFixture) -> Vec<TextOutcome> {
    vec![
        TextOutcome::text(&fixture.text),
        TextOutcome::text(""),
        TextOutcome::text(ZERO_TERM_TEXT),
        TextOutcome::Text(None),
        TextOutcome::Text(None),
        TextOutcome::TypeError,
    ]
}

// ---------------------------------------------------------------------------
// Case 1: every shape survives maintenance, a checkpoint and a reopen
// ---------------------------------------------------------------------------

#[cfg_attr(test, test)]
fn ze170_stored_text_shapes_survive_maintenance_and_reopen() {
    run_ze170_stored_text_shapes_survive_maintenance_and_reopen();
}

pub(super) fn run_ze170_stored_text_shapes_survive_maintenance_and_reopen() {
    let mut fixture = fixture("ze170-shapes");
    let operands = shape_operands(&fixture);
    let expected = expected_shapes(&fixture);
    assert_eq!(
        read_fresh(&fixture.store, &operands),
        expected,
        "the four text shapes are wrong before any maintenance ran"
    );

    // `K` maintenance calls, one per node.
    let rounds = fixture.nodes().len();
    let relocatable = [fixture.a, fixture.e, fixture.z, fixture.n];
    let before = node_records(&fixture.store, &relocatable);
    let mut replaced = 0;
    for call in 0..rounds {
        replaced += maintain_once(&fixture.store, call).replaced_physical_refs;
        assert_eq!(read_fresh(&fixture.store, &operands), expected);
    }
    assert!(
        replaced > 0,
        "the maintenance cycle must move actual physical references"
    );
    // ZE-260 S6a drains only packs that are at least a quarter dead, so
    // this fully live fixture's records need not move; the shapes must
    // still read back exactly after the calls and after reopen.
    let _ = before;
    assert_eq!(
        read_fresh(&fixture.store, &operands),
        expected,
        "a text shape changed after K maintenance calls"
    );

    fixture
        .store
        .checkpoint_native_graph(&control())
        .expect("ze170 checkpoint");

    // Per-call is the wrong assertion for this window. ZE-169 measured that a
    // consolidation call which retires packs arms a reclaim root, and the next
    // two calls take the reclaim branch (`resume_pending_reclaim` and
    // `retire_completed_reclaim`), both of which construct their report with a
    // literal `replaced_physical_refs: 0` and never enter consolidation. A zero
    // here means "this call drained a reclaim", not "there is nothing left to
    // do": maintenance does not converge at this size. Only the total over the
    // window is guaranteed positive, because a reclaim drain is always preceded
    // by a consolidation call that armed it.
    let mut second_half = 0_u64;
    for call in rounds..2 * rounds {
        second_half += maintain_once(&fixture.store, call).replaced_physical_refs;
    }
    assert!(
        second_half > 0,
        "the K calls after the checkpoint replaced no physical reference in total"
    );
    assert_eq!(
        read_fresh(&fixture.store, &operands),
        expected,
        "a text shape changed after the checkpoint and K more maintenance calls"
    );

    fixture.reopen();
    assert_eq!(
        read_fresh(&fixture.store, &operands),
        expected,
        "a text shape changed across a close and reopen"
    );
    fixture.store.close().expect("close ze170 shapes store");
}

// ---------------------------------------------------------------------------
// Case 2: a retained lease suppresses every unlink, and the text stays exact
// ---------------------------------------------------------------------------

/// Reads the anchor first, so only the second pack is mapped, then checkpoints
/// and drives maintenance from inside the same thread that holds the lease,
/// then reads the four shapes out of the first pack for the first time.
///
/// The checkpoint matters. Measured on unmodified production: a pack cannot be
/// retired while the historical WAL still protects it, so without a checkpoint
/// no maintenance call ever enters the reclaim branch and there is no unlink to
/// be protected from. With the checkpoint, the only thing standing between
/// these objects and an unlink is `maintenance.rs::prepare_durable_proof`
/// tracing this lease's retained bundle as `ProtectedClass::Reader`.
struct RetainedAcrossReclaim {
    store: Arc<Store>,
    anchor: Operand,
    shapes: Vec<Operand>,
    calls: usize,
    /// The record artifact this lease's retained bundle reaches. The contract
    /// is checked from inside `consume`, while the lease is still held.
    original_a: PathBuf,
    report: mpsc::Sender<ReclaimObservations>,
}

struct ReclaimObservations {
    /// The anchor read, then the four shape reads.
    outcomes: Vec<TextOutcome>,
    /// Bytes maintenance unlinked while the lease was held. This is an
    /// observation about this fixture, not the contract. The contract is that
    /// a reader-retained bundle keeps every object it can reach on disk, which
    /// `consume` asserts directly against `original_a` while the lease is
    /// held. Whether the byte count reaches zero depends on where the
    /// checkpoint and the bounded reclaim land relative to the lease: this
    /// fixture checkpoints from inside the lease, so the only reclaimable
    /// objects are ones the bundle protects and the count stays zero, while a
    /// fixture that checkpoints before admission can legitimately unlink
    /// unprotected bytes under a lease.
    removed_under_lease: u64,
    /// Physical references replaced while the lease was held, which proves
    /// those calls did real work rather than refusing.
    replaced_under_lease: u64,
}

impl NativeReadConsumer<()> for RetainedAcrossReclaim {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<(), TreeError> {
        let start = view.sequence();
        // Only the anchor's pack is mapped at this point; the first pack is
        // still unmapped and is opened lazily after it has been superseded.
        let mut outcomes = stored_text_reads(view, runtime, &[self.anchor], |_, _| {});
        self.store
            .checkpoint_native_graph(&control())
            .expect("ze170 checkpoint under a retained lease");
        let mut removed_under_lease = 0_u64;
        let mut replaced_under_lease = 0_u64;
        for call in 0..self.calls {
            let report = maintain_once(&self.store, call);
            removed_under_lease += report.removed_bytes;
            replaced_under_lease += report.replaced_physical_refs;
        }
        // Still inside the lease: this is the contract under test, and it can
        // only be checked here, because the moment `consume` returns the lease
        // is dropped and nothing protects the artifact any more.
        assert!(
            self.original_a.exists(),
            "the original record artifact was unlinked while a reader still held it"
        );
        assert_eq!(
            view.sequence(),
            start,
            "the retained view followed the writer to a new sequence"
        );
        outcomes.extend(stored_text_reads(view, runtime, &self.shapes, |_, _| {}));
        self.report
            .send(ReclaimObservations {
                outcomes,
                removed_under_lease,
                replaced_under_lease,
            })
            .expect("ze170 retained-view report");
        Ok(())
    }
}

#[cfg_attr(test, test)]
fn ze170_retained_view_reads_exact_text_across_reclaim_unlink() {
    run_ze170_retained_view_reads_exact_text_across_reclaim_unlink();
}

pub(super) fn run_ze170_retained_view_reads_exact_text_across_reclaim_unlink() {
    let fixture = fixture("ze170-reclaim");
    let rounds = fixture.nodes().len();
    let original_a = {
        let lease = fixture
            .store
            .admit_native_read()
            .expect("ze170 original record reader");
        record_artifact_path(&fixture.store, &lease, &fixture.path, fixture.a)
    };
    assert!(
        original_a.exists(),
        "the original record artifact must exist before maintenance"
    );

    let shapes = vec![
        Operand::Node(fixture.a),
        Operand::Node(fixture.e),
        Operand::Node(fixture.z),
        Operand::Node(fixture.n),
    ];
    let expected = vec![
        TextOutcome::text(&fixture.text),
        TextOutcome::text(""),
        TextOutcome::text(ZERO_TERM_TEXT),
        TextOutcome::Text(None),
    ];

    let (report_tx, report_rx) = mpsc::channel();
    fixture
        .store
        .with_native_read(
            &control(),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            16,
            RetainedAcrossReclaim {
                store: Arc::clone(&fixture.store),
                anchor: Operand::Node(fixture.b),
                shapes,
                // One consolidating call relocates one record, and two of every
                // three post-checkpoint calls drain a reclaim instead, so `2K`
                // calls both relocate records and arm reclaims repeatedly.
                calls: 2 * rounds,
                original_a: original_a.clone(),
                report: report_tx,
            },
        )
        .expect("ze170 retained read across reclaim");
    let observed = report_rx.recv().expect("ze170 retained-view observations");
    assert!(
        observed.replaced_under_lease > 0,
        "maintenance did no work under the lease, so nothing was proved"
    );
    assert_eq!(
        observed.outcomes[0],
        TextOutcome::text(ANCHOR_TEXT),
        "the anchor read before maintenance is wrong"
    );
    assert_eq!(
        &observed.outcomes[1..],
        expected.as_slice(),
        "the retained view did not read the exact original text after maintenance"
    );
    // Mandatory folds also produce unprotected proof/control files. They can
    // be reclaimed while the exact retained text artifacts stay protected.
    assert!(observed.removed_under_lease > 0);

    // The lease is gone, so nothing protects those objects any more. The unlink
    // that now fires is a separate leg: it proves reclaim resumes once the
    // lease drops, rather than maintenance simply having nothing to reclaim.
    fixture
        .store
        .checkpoint_native_graph(&control())
        .expect("ze170 checkpoint after the lease dropped");
    let mut removed = 0_u64;
    for call in 0..6 {
        removed += maintain_once(&fixture.store, call).removed_bytes;
    }
    assert!(
        removed > 0,
        "no bytes were unlinked after the retaining lease dropped"
    );
    // Deliberately no `!original_a.exists()` here. Measured on unmodified
    // production: that file is still on disk after sixty post-lease calls,
    // because bounded reclaim retires roughly one object every third call
    // while each consolidating call creates new dead ones, so the backlog
    // never drains at this fixture size. The lease-scoped contract is proved
    // inside `consume` instead, where the artifact is still on disk with the
    // lease held; this leg only shows that reclaim resumes once it is gone.
    assert_eq!(
        read_fresh(&fixture.store, &[Operand::Node(fixture.a)]),
        vec![TextOutcome::text(&fixture.text)],
        "a fresh view lost the text after the reclaim unlinked dead objects"
    );
    fixture.store.close().expect("close ze170 reclaim store");
}

// ---------------------------------------------------------------------------
// Case 3: a retained view is stable across a concurrent publication
// ---------------------------------------------------------------------------

struct RetainedAcrossPublication {
    store: Arc<Store>,
    node: NodeId,
    new_text: String,
    report: mpsc::Sender<Vec<TextOutcome>>,
}

impl NativeReadConsumer<()> for RetainedAcrossPublication {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<(), TreeError> {
        let start = view.sequence();
        let mut observed = stored_text_reads(view, runtime, &[Operand::Node(self.node)], |_, _| {});
        let store = Arc::clone(&self.store);
        let node = self.node;
        let new_text = self.new_text.clone();
        let publisher = std::thread::spawn(move || {
            with_local_refs(|refs| {
                let mut replaced_labels = [GraphName::new("Text").expect("ze170 label")];
                let mut created_labels = [GraphName::new("Text").expect("ze170 label")];
                let replaced = CanonicalContents::node(
                    &mut replaced_labels,
                    &mut [],
                    Some(new_text.as_str()),
                    None,
                )
                .expect("ze170 replacement contents");
                let created =
                    CanonicalContents::node(&mut created_labels, &mut [], Some("created"), None)
                        .expect("ze170 created contents");
                let _ = refs;
                store
                    .apply_native_graph(
                        &[
                            StructuredWrite {
                                key: ApplicationKey::new(EntityKind::Node, "ze170-publish", "a")
                                    .expect("ze170 replacement key"),
                                revision: GraphRevision::new(2)
                                    .expect("ze170 replacement revision"),
                                operation: StructuredOperation::Put(EntityId::Node(node)),
                                image: Some(WriteImage::Node(&replaced)),
                            },
                            StructuredWrite {
                                key: ApplicationKey::new(
                                    EntityKind::Node,
                                    "ze170-publish",
                                    "created",
                                )
                                .expect("ze170 created key"),
                                revision: GraphRevision::new(1).expect("ze170 created revision"),
                                operation: StructuredOperation::Create,
                                image: Some(WriteImage::Node(&created)),
                            },
                        ],
                        &control(),
                    )
                    .expect("publish while an old view is retained")
            })
        });
        publisher.join().expect("ze170 publication thread");
        assert_eq!(
            view.sequence(),
            start,
            "the retained view followed the writer to a new sequence"
        );
        observed.extend(stored_text_reads(
            view,
            runtime,
            &[Operand::Node(self.node)],
            |_, _| {},
        ));
        self.report
            .send(observed)
            .expect("ze170 publication observations");
        Ok(())
    }
}

#[cfg_attr(test, test)]
fn ze170_retained_view_is_stable_across_concurrent_publication() {
    run_ze170_retained_view_is_stable_across_concurrent_publication();
}

pub(super) fn run_ze170_retained_view_is_stable_across_concurrent_publication() {
    let fixture = fixture("ze170-publish");
    let new_text = format!("{}-replaced", multi_chunk_text());
    let (report_tx, report_rx) = mpsc::channel();
    fixture
        .store
        .with_native_read(
            &control(),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            16,
            RetainedAcrossPublication {
                store: Arc::clone(&fixture.store),
                node: fixture.a,
                new_text: new_text.clone(),
                report: report_tx,
            },
        )
        .expect("ze170 retained read across publication");
    let observed = report_rx.recv().expect("ze170 publication observations");
    assert_eq!(
        observed,
        vec![
            TextOutcome::text(&fixture.text),
            TextOutcome::text(&fixture.text),
        ],
        "the retained view saw the concurrent publication's text"
    );
    assert_eq!(
        read_fresh(&fixture.store, &[Operand::Node(fixture.a)]),
        vec![TextOutcome::text(&new_text)],
        "a fresh view did not see the published replacement text"
    );
    fixture.store.close().expect("close ze170 publish store");
}

// ---------------------------------------------------------------------------
// Case 4: a detached node's text is a typed error, not absence or empty text
// ---------------------------------------------------------------------------

#[cfg_attr(test, test)]
fn ze170_detached_node_text_is_a_typed_error_across_maintenance() {
    run_ze170_detached_node_text_is_a_typed_error_across_maintenance();
}

pub(super) fn run_ze170_detached_node_text_is_a_typed_error_across_maintenance() {
    let mut fixture = fixture("ze170-detach");
    let rounds = fixture.nodes().len();
    assert_eq!(
        read_fresh(&fixture.store, &[Operand::Node(fixture.e)]),
        vec![TextOutcome::text("")],
        "the present-empty node did not read as empty before DETACH"
    );

    fixture
        .store
        .apply_native_graph(
            &[StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "ze170-detach", "e")
                    .expect("ze170 detach key"),
                revision: GraphRevision::new(2).expect("ze170 detach revision"),
                operation: StructuredOperation::Delete(
                    EntityId::Node(fixture.e),
                    GraphDeleteMode::Detach,
                ),
                image: None,
            }],
            &control(),
        )
        .expect("ze170 detach the present-empty node");

    let operands = [Operand::Node(fixture.e)];
    let expected = vec![TextOutcome::Absent];
    assert_eq!(
        read_fresh(&fixture.store, &operands),
        expected,
        "a detached node's stored text is not a typed absence error"
    );
    for call in 0..rounds {
        maintain_once(&fixture.store, call);
    }
    assert_eq!(
        read_fresh(&fixture.store, &operands),
        expected,
        "a detached node's stored text changed across maintenance"
    );
    fixture.reopen();
    assert_eq!(
        read_fresh(&fixture.store, &operands),
        expected,
        "a detached node's stored text changed across a reopen"
    );
    // The live shapes are untouched by the tombstone.
    assert_eq!(
        read_fresh(
            &fixture.store,
            &[Operand::Node(fixture.a), Operand::Node(fixture.z)]
        ),
        vec![
            TextOutcome::text(&fixture.text),
            TextOutcome::text(ZERO_TERM_TEXT),
        ],
        "DETACH disturbed a node it did not name"
    );
    fixture.store.close().expect("close ze170 detach store");
}

// ---------------------------------------------------------------------------
// Case 5: a copied text value outlives the store
// ---------------------------------------------------------------------------

struct CopyOutReads {
    nodes: Vec<NodeId>,
}

impl NativeReadConsumer<Vec<(NodeId, Option<String>)>> for CopyOutReads {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<Vec<(NodeId, Option<String>)>, TreeError> {
        let operands = self
            .nodes
            .iter()
            .map(|node| Operand::Node(*node))
            .collect::<Vec<Operand>>();
        let results = stored_text_reads(view, runtime, &operands, |_, _| {});
        Ok(self
            .nodes
            .iter()
            .zip(results)
            .map(|(node, outcome)| match outcome {
                TextOutcome::Text(text) => (*node, text),
                other => panic!("ze170 copy-out read failed: {other:?}"),
            })
            .collect())
    }
}

#[cfg_attr(test, test)]
fn ze170_copied_text_survives_close() {
    run_ze170_copied_text_survives_close();
}

pub(super) fn run_ze170_copied_text_survives_close() {
    let fixture = fixture("ze170-copy-out");
    let expected_text = fixture.text.clone();
    let copied = fixture
        .store
        .with_native_read(
            &control(),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            16,
            CopyOutReads {
                nodes: vec![fixture.a, fixture.e, fixture.z, fixture.n, fixture.b],
            },
        )
        .expect("ze170 copy text out of the view");

    let TextFixture {
        _directory: directory,
        path: _path,
        store,
        a,
        e,
        z,
        n,
        b,
        text: _text,
    } = fixture;
    assert_eq!(
        Arc::strong_count(&store),
        1,
        "something other than this frame still holds the store open"
    );
    store.close().expect("ze170 close after copying text out");
    drop(store);

    // Every value below is asserted against a literal or against a `String`
    // built before the store existed, never against a second read.
    assert_eq!(copied.len(), 5);
    assert_eq!(copied[0].0, a);
    assert_eq!(copied[0].1.as_deref(), Some(expected_text.as_str()));
    assert_eq!(copied[0].1.as_deref().map(str::len), Some(149_928));
    assert_eq!(copied[1], (e, Some(String::new())));
    assert_eq!(copied[2], (z, Some(ZERO_TERM_TEXT.to_string())));
    assert_eq!(copied[3], (n, None));
    assert_eq!(copied[4], (b, Some(ANCHOR_TEXT.to_string())));
    // Present-and-empty is not absence, and the distinction survives the close.
    assert_ne!(copied[1].1, copied[3].1);
    drop(directory);
}

// ---------------------------------------------------------------------------
// Case 6: close during a copy is a typed cancel, never torn text
// ---------------------------------------------------------------------------

struct CloseDuringCopy {
    store: Arc<Store>,
    anchor: NodeId,
    multi: NodeId,
    report: mpsc::Sender<Vec<TextOutcome>>,
    closed: mpsc::Sender<std::thread::JoinHandle<Result<(), StoreError>>>,
}

impl NativeReadConsumer<()> for CloseDuringCopy {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<(), TreeError> {
        let store = Arc::clone(&self.store);
        let closed = self.closed.clone();
        let observed = stored_text_reads(
            view,
            runtime,
            &[Operand::Node(self.anchor), Operand::Node(self.multi)],
            move |row, view| {
                if row != 1 {
                    return;
                }
                // The anchor's text is already copied into an owned `String`.
                // Close now, and do not evaluate again until the view has
                // actually observed the cancellation, so the second read is a
                // read against a cancelled view rather than a race.
                let closing = Arc::clone(&store);
                closed
                    .send(std::thread::spawn(move || closing.close()))
                    .expect("ze170 close handle");
                view.wait_until_cancelled_for_test()
                    .expect("ze170 cancellation observer");
            },
        );
        self.report
            .send(observed)
            .expect("ze170 close observations");
        Ok(())
    }
}

#[cfg_attr(test, test)]
fn ze170_close_during_text_copy_is_a_typed_cancel_not_torn_text() {
    run_ze170_close_during_text_copy_is_a_typed_cancel_not_torn_text();
}

pub(super) fn run_ze170_close_during_text_copy_is_a_typed_cancel_not_torn_text() {
    let directory = super::tempfile::tempdir().expect("ze170 close store directory");
    let path = directory.path().join("native");
    let store = Arc::new(
        Store::create_native_graph(
            &path,
            fixture_options().with_reader_drain_timeout(Duration::ZERO),
            None,
        )
        .expect("create ze170 close store"),
    );
    let text = multi_chunk_text();
    let receipts = with_local_refs(|_refs| {
        let mut labels_a = [GraphName::new("Text").expect("ze170 label")];
        let mut labels_b = [GraphName::new("Anchor").expect("ze170 anchor label")];
        let node_a = CanonicalContents::node(&mut labels_a, &mut [], Some(text.as_str()), None)
            .expect("ze170 multi-chunk node");
        let node_b = CanonicalContents::node(&mut labels_b, &mut [], Some(ANCHOR_TEXT), None)
            .expect("ze170 anchor node");
        store
            .apply_native_graph(
                &[
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "ze170-close", "a")
                            .expect("ze170 key"),
                        revision: GraphRevision::new(1).expect("ze170 revision"),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&node_a)),
                    },
                    StructuredWrite {
                        key: ApplicationKey::new(EntityKind::Node, "ze170-close", "b")
                            .expect("ze170 anchor key"),
                        revision: GraphRevision::new(1).expect("ze170 anchor revision"),
                        operation: StructuredOperation::Create,
                        image: Some(WriteImage::Node(&node_b)),
                    },
                ],
                &control(),
            )
            .expect("publish ze170 close fixture")
    });
    let multi = node_id(&receipts[0]);
    let anchor = node_id(&receipts[1]);

    let (report_tx, report_rx) = mpsc::channel();
    let (closed_tx, closed_rx) = mpsc::channel();
    // `with_native_read` runs a final `runtime.checkpoint()` after the consumer
    // returns, and that checkpoint sees the same cancellation. The observations
    // therefore come back through a channel, and the cancelled return is itself
    // required: a close that did not cancel this runtime would return `Ok`.
    let admitted = store.with_native_read(
        &control(),
        RuntimeLimits::default(),
        16 * 1024 * 1024,
        16,
        CloseDuringCopy {
            store: Arc::clone(&store),
            anchor,
            multi,
            report: report_tx,
            closed: closed_tx,
        },
    );
    assert!(
        matches!(
            admitted,
            Err(NativeGraphError::Read(TreeError::Runtime(
                RuntimeError::Value(QueryError::ReadCancelled)
            )))
        ),
        "the admitted read did not end in a typed cancellation"
    );

    let observed = report_rx.recv().expect("ze170 close observations");
    assert_eq!(
        observed[0],
        TextOutcome::text(ANCHOR_TEXT),
        "the text copied before close was torn or lost"
    );
    assert_eq!(
        observed[1],
        TextOutcome::Cancelled,
        "a read against a closing view is not a typed cancellation"
    );
    // The lease dropped when `with_native_read` returned, so close can finish.
    closed_rx
        .recv()
        .expect("ze170 close handle")
        .join()
        .expect("ze170 close thread")
        .expect("ze170 close result");
    drop(store);
    drop(directory);
}
