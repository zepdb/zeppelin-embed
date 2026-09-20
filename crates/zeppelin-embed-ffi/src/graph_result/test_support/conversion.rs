//! Opt-in primitive bridge through the actual private native converter.

use super::{AllocationFaultReceipt, current_allocation_fault_receipt};
use crate::graph_result::conversion::{
    ConversionError, PreparedNativeResponse, finalize_native, prepare_native,
};
use crate::graph_result::{GraphResultRegistry, OwnerError};
use crate::{ZeGraphResponse, ZeGraphWorkKind};
use std::cell::{Cell, RefCell};
use std::mem::{size_of, size_of_val};
use zeppelin_embed::property_graph::query::QueryError;
use zeppelin_embed::property_graph::query::completed::{
    ActualTier, CandidateCoverage, CompletedError, LegState, ListKind, Node, Outcome, Pools,
    Relationship, ResultInput, ResultSource, ScorePrecision, SearchKind, SearchReport, SourceError,
    Span, Value,
};
use zeppelin_embed::property_graph::query::plan::{
    NodeFacts, Operator, OperatorKind, PlanBacking, PlanDescription, PlanFootprint, PlanNodeId,
    RetainedRegion, SearchCallId, VALIDATION_SCRATCH_BYTES,
};
use zeppelin_embed::property_graph::query::resources::{
    MemoryError, QueryArena, QueryInputs, RetainedAllocation, RetentionInventory, RuntimePlan,
};
use zeppelin_embed::property_graph::query::runtime::{
    Completion, ExecutionCapacity, FrozenOutput, PreparedRows, PullOperator, PullState, RowBatch,
    RuntimeContext, RuntimeError, RuntimeFailure, WorkKind, execute_in,
};
use zeppelin_embed::property_graph::{GraphGeneration, GraphRevision, NodeId, RelId};

static REGISTRY: GraphResultRegistry = GraphResultRegistry::new(32);

/// Successful native outcome selected by the primitive fixture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeConversionOutcome {
    /// Read-only result.
    Read,
    /// Known commit at admitted generation plus one.
    Committed,
    /// Exact replay with no new generation.
    Replayed,
    /// Successful effect-free operation.
    NoOp,
}

/// Primitive input for one authentic conversion and driver finalization.
#[derive(Clone, Copy, Debug)]
pub struct NativeConversionCase<'a> {
    /// Full node identity.
    pub node_id: u128,
    /// Full relationship identity.
    pub relationship_id: u128,
    /// Exact IEEE binary64 bits.
    pub scalar_bits: u64,
    /// Borrowed UTF-8 bytes copied through native and C owners.
    pub payload: &'a str,
    /// Successful result disposition.
    pub outcome: NativeConversionOutcome,
    /// Whether one vector report is present.
    pub include_report: bool,
}

/// Boundary reached before a typed refusal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeConversionStage {
    /// Native source validation or owned copy.
    Native,
    /// C geometry, reservation, allocation, mapping or registration.
    C,
    /// Driver work/final checkpoint after successful completion.
    Driver,
}

/// Primitive refusal classification without exposing private error types.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeConversionRefusal {
    /// Exact source/view identity rejection.
    Context,
    /// Malformed native shape, bytes or report.
    Shape,
    /// Represented or padded capacity limit.
    Limit,
    /// Actual allocation site refused.
    Allocation,
    /// Query or shared memory refused.
    Memory,
    /// Retained view or caller control cancelled.
    Cancel,
    /// Cumulative copied-work allowance refused.
    Work,
    /// Registry admission/gate/token refused.
    Registry,
    /// A driver-only shape or other refusal.
    Other,
}

/// Exact primitive failure and real fault/cleanup receipts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeConversionFailure {
    /// Boundary at which the refusal occurred.
    pub stage: NativeConversionStage,
    /// Stable primitive classification.
    pub refusal: NativeConversionRefusal,
    /// Actual C allocation sites reached, including the refused site.
    pub allocation_matching_sites: usize,
    /// Actual C allocation faults fired.
    pub allocation_fires: usize,
    /// Whether every query charge returned to the entry baseline.
    pub query_charge_restored: bool,
}

/// Primitive observation made after actual finalize/expose and before free.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeConversionObservation {
    /// Copied full node identity halves.
    pub node_id: (u64, u64),
    /// Copied full relationship identity halves.
    pub relationship_id: (u64, u64),
    /// Exact scalar bits after the C mapping.
    pub scalar_bits: u64,
    /// Exact list tag for the native Empty sentinel.
    pub list_tag: u32,
    /// Explicit text presence, start and count.
    pub text: (u32, u32, u32),
    /// Byte-identical copied payload.
    pub payload: Vec<u8>,
    /// Per-report work start/count, or zero/zero when absent.
    pub report_work: (u32, u32),
    /// Final global work count and copied-byte value.
    pub global_work: (u32, u64),
    /// Actual peak charged query bytes captured by the driver.
    pub peak_query_bytes: u64,
    /// Source observations; exactly one is required.
    pub source_calls: usize,
    /// Actual C allocation sites/fires seen by the active scope.
    pub allocation: AllocationFaultReceipt,
    /// Whether every query charge returned to the entry baseline.
    pub query_charge_restored: bool,
}

struct Source<'a> {
    input: ResultInput<'a>,
    calls: Cell<usize>,
}
impl ResultSource for Source<'_> {
    fn result_input(&self) -> Result<ResultInput<'_>, SourceError> {
        self.calls.set(self.calls.get().saturating_add(1));
        Ok(self.input)
    }
}

struct OneRow(bool);
impl<'v, 'm, 'g> PullOperator<'v, 'm, 'g> for OneRow {
    fn node(&self) -> PlanNodeId {
        PlanNodeId(0)
    }
    fn prepare_search(
        &mut self,
        _: PlanNodeId,
        _: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<(), RuntimeError> {
        Err(RuntimeError::Batch)
    }
    fn pull(
        &mut self,
        context: &mut RuntimeContext<'v, 'm, 'g>,
        output: &mut RowBatch<'v, 'm, 'g>,
    ) -> Result<PullState, RuntimeError> {
        if self.0 {
            return Err(RuntimeError::Batch);
        }
        self.0 = true;
        output.push_row(&[], context)?;
        Ok(PullState::Done)
    }
}

struct Complete<'a> {
    source: &'a Source<'a>,
    error: &'a RefCell<Option<ConversionError>>,
}

struct ExposedRoot(ZeGraphResponse);
impl Drop for ExposedRoot {
    fn drop(&mut self) {
        let _ = REGISTRY.free(&mut self.0);
    }
}
impl<'m, 'g: 'm> Completion<'m, 'g> for Complete<'_> {
    type Output = PreparedNativeResponse<'m, 'g>;
    fn complete<'v>(
        &mut self,
        _: &PreparedRows<'v, 'm, 'g>,
        context: &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<FrozenOutput<Self::Output>, RuntimeError> {
        prepare_native(&REGISTRY, self.source, context).map_err(|error| {
            *self.error.borrow_mut() = Some(error);
            RuntimeError::Batch
        })
    }
}

/// Executes the actual private converter, driver finalizer, exposure and free.
pub fn run_native_conversion_case(
    context: &mut RuntimeContext<'_, '_, '_>,
    case: NativeConversionCase<'_>,
) -> Result<NativeConversionObservation, NativeConversionFailure> {
    let baseline = context.memory().reserved_bytes();
    let result = run_case(context, case);
    let restored = context.memory().reserved_bytes() == baseline;
    match result {
        Ok(mut observation) => {
            observation.query_charge_restored = restored;
            Ok(observation)
        }
        Err(mut failure) => {
            failure.query_charge_restored = restored;
            Err(failure)
        }
    }
}

fn run_case(
    context: &mut RuntimeContext<'_, '_, '_>,
    case: NativeConversionCase<'_>,
) -> Result<NativeConversionObservation, NativeConversionFailure> {
    let node_id = NodeId::new(case.node_id).map_err(|_| {
        failure(
            NativeConversionStage::Native,
            NativeConversionRefusal::Shape,
        )
    })?;
    let relationship_id = RelId::new(case.relationship_id).map_err(|_| {
        failure(
            NativeConversionStage::Native,
            NativeConversionRefusal::Shape,
        )
    })?;
    let payload_len = u32::try_from(case.payload.len()).map_err(|_| {
        failure(
            NativeConversionStage::Native,
            NativeConversionRefusal::Limit,
        )
    })?;
    let mut bytes = QueryArena::new(
        context.memory(),
        case.payload.len().checked_add(4096).ok_or_else(|| {
            failure(
                NativeConversionStage::Native,
                NativeConversionRefusal::Limit,
            )
        })?,
    )
    .map_err(memory_failure)?;
    for byte in case.payload.as_bytes() {
        bytes.push(*byte).map_err(memory_failure)?;
    }
    let mut values = QueryArena::new(context.memory(), 5).map_err(memory_failure)?;
    for value in [
        Value::F64(case.scalar_bits),
        Value::Node(0),
        Value::Relationship(0),
        Value::List {
            children: Span::new(0, 0),
            element: ListKind::Empty,
        },
        Value::String(Span::new(0, payload_len)),
    ] {
        values.push(value).map_err(memory_failure)?;
    }
    let mut vectors = QueryArena::new(context.memory(), 1).map_err(memory_failure)?;
    vectors.push(0x8000_0000).map_err(memory_failure)?;
    let mut nodes = QueryArena::new(context.memory(), 1).map_err(memory_failure)?;
    nodes
        .push(Node {
            id: node_id,
            revision: GraphRevision::new(1).map_err(|_| {
                failure(
                    NativeConversionStage::Native,
                    NativeConversionRefusal::Shape,
                )
            })?,
            generation: context.view().generation(),
            key: None,
            labels: Span::new(0, 0),
            properties: Span::new(0, 0),
            text: Some(Span::new(payload_len, 0)),
            vector: Some(Span::new(0, 1)),
        })
        .map_err(memory_failure)?;
    let mut relationships = QueryArena::new(context.memory(), 1).map_err(memory_failure)?;
    relationships
        .push(Relationship {
            id: relationship_id,
            revision: GraphRevision::new(2).map_err(|_| {
                failure(
                    NativeConversionStage::Native,
                    NativeConversionRefusal::Shape,
                )
            })?,
            generation: context.view().generation(),
            key: None,
            source: node_id,
            target: NodeId::new(case.node_id ^ (1_u128 << 63)).map_err(|_| {
                failure(
                    NativeConversionStage::Native,
                    NativeConversionRefusal::Shape,
                )
            })?,
            relationship_type: Span::new(0, payload_len),
            properties: Span::new(0, 0),
        })
        .map_err(memory_failure)?;
    let mut reports = QueryArena::new(context.memory(), usize::from(case.include_report))
        .map_err(memory_failure)?;
    if case.include_report {
        reports
            .push(SearchReport {
                call: SearchCallId(0),
                generation: context.view().generation(),
                kind: SearchKind::Vector,
                requested_tier: None,
                actual_tier: Some(ActualTier::Exact),
                precision: ScorePrecision::Original,
                coverage: CandidateCoverage::Exact,
                vector_leg: LegState::Nonempty,
                lexical_leg: LegState::NotRequested,
                document_epoch: Some(0),
                query_epoch: Some(17),
                tokenizer_epoch: None,
                effective_alpha_bits: 0.5_f64.to_bits(),
                normalization_version: 3,
                rules_version: 5,
                candidate_count: 1,
                cross_scored_count: 1,
                fallback_count: 0,
                cross_score_complete: true,
                work: context.counters(),
            })
            .map_err(memory_failure)?;
    }
    let outcome = match case.outcome {
        NativeConversionOutcome::Read => Outcome::Read,
        NativeConversionOutcome::Committed => Outcome::Committed {
            changed: GraphGeneration::new(
                context
                    .view()
                    .generation()
                    .get()
                    .checked_add(1)
                    .ok_or_else(|| {
                        failure(
                            NativeConversionStage::Native,
                            NativeConversionRefusal::Shape,
                        )
                    })?,
            ),
        },
        NativeConversionOutcome::Replayed => Outcome::Replayed,
        NativeConversionOutcome::NoOp => Outcome::NoOp,
    };
    let source = Source {
        input: ResultInput {
            view: context.view(),
            pools: Pools {
                values: values.as_slice(),
                bytes: bytes.as_slice(),
                nodes: nodes.as_slice(),
                relationships: relationships.as_slice(),
                vectors: vectors.as_slice(),
                reports: reports.as_slice(),
                ..Pools::default()
            },
            rows: 1,
            outcome,
        },
        calls: Cell::new(0),
    };
    with_plan(context, |plan, context| {
        let error = RefCell::new(None);
        let mut completion = Complete {
            source: &source,
            error: &error,
        };
        let mut pull = OneRow(false);
        let execution = execute_in(
            context,
            plan,
            &mut pull,
            &mut completion,
            ExecutionCapacity {
                batch_rows: 1,
                result_rows: 1,
                ..ExecutionCapacity::default()
            },
        )
        .map_err(|driver| map_driver_failure(driver, error.into_inner()))?;
        let peak_query_bytes = execution.peak_query_bytes as u64;
        let finalized = finalize_native(execution);
        let (owner, successful) = finalized.into_parts();
        let mut root = ExposedRoot(owner.expose(successful));
        let observed = observe(&root.0, case.payload, peak_query_bytes, source.calls.get())?;
        REGISTRY
            .free(&mut root.0)
            .map_err(|error| owner_failure(&error))?;
        Ok(observed)
    })
}

fn with_plan<'v, 'm, 'g, T>(
    context: &mut RuntimeContext<'v, 'm, 'g>,
    run: impl FnOnce(
        &RuntimePlan<'_, '_, '_, '_, '_, '_>,
        &mut RuntimeContext<'v, 'm, 'g>,
    ) -> Result<T, NativeConversionFailure>,
) -> Result<T, NativeConversionFailure> {
    let memory = context.memory();
    let mut scratch = memory.reserve_external_capacity().map_err(memory_failure)?;
    scratch
        .reserve_additional(
            VALIDATION_SCRATCH_BYTES
                .checked_add(size_of::<[RetainedRegion; 2]>())
                .and_then(|n| n.checked_add(size_of::<PlanDescription<'_>>()))
                .ok_or_else(|| {
                    failure(
                        NativeConversionStage::Driver,
                        NativeConversionRefusal::Limit,
                    )
                })?,
        )
        .map_err(memory_failure)?;
    let mut operators = QueryArena::new(memory, 1).map_err(memory_failure)?;
    operators
        .push(Operator {
            inputs: &[],
            kind: OperatorKind::Unit,
        })
        .map_err(memory_failure)?;
    let mut facts = QueryArena::new(memory, 16).map_err(memory_failure)?;
    facts.push(NodeFacts::default()).map_err(memory_failure)?;
    let mut regions = [
        RetainedRegion::declared(
            operators.as_slice().as_ptr() as usize,
            operators.heap_bytes(),
        )
        .map_err(|_| {
            failure(
                NativeConversionStage::Driver,
                NativeConversionRefusal::Shape,
            )
        })?,
        RetainedRegion::declared(facts.as_slice().as_ptr() as usize, facts.heap_bytes()).map_err(
            |_| {
                failure(
                    NativeConversionStage::Driver,
                    NativeConversionRefusal::Shape,
                )
            },
        )?,
    ];
    regions.sort();
    let (plan, facts_owner) = facts
        .validate_plan(
            PlanDescription {
                operators: operators.as_slice(),
                expressions: &[],
                parameters: &[],
                root: PlanNodeId(0),
                eager_searches: &[],
            },
            PlanFootprint::declared(memory.reserved_bytes()),
            PlanBacking::new(&regions, size_of_val(&regions)).map_err(|_| {
                failure(
                    NativeConversionStage::Driver,
                    NativeConversionRefusal::Shape,
                )
            })?,
            context.values(),
        )
        .map_err(|_| {
            failure(
                NativeConversionStage::Driver,
                NativeConversionRefusal::Shape,
            )
        })?;
    let owners = [
        RetainedAllocation::arena(&operators).map_err(|_| {
            failure(
                NativeConversionStage::Driver,
                NativeConversionRefusal::Shape,
            )
        })?,
        facts_owner,
    ];
    let inputs = QueryInputs::reserve(memory, RetentionInventory::array(&owners), context.values())
        .map_err(memory_failure)?;
    let admitted = inputs.admit_plan(&plan, context.values()).map_err(|_| {
        failure(
            NativeConversionStage::Driver,
            NativeConversionRefusal::Shape,
        )
    })?;
    run(&admitted, context)
}

fn observe(
    root: &ZeGraphResponse,
    expected_payload: &str,
    peak_query_bytes: u64,
    source_calls: usize,
) -> Result<NativeConversionObservation, NativeConversionFailure> {
    let values = checked_slice(root.pool.values, root.pool.value_count)?;
    let nodes = checked_slice(root.pool.nodes, root.pool.node_count)?;
    let relationships = checked_slice(root.pool.relationships, root.pool.relationship_count)?;
    let reports = checked_slice(root.reports, root.report_count)?;
    let work = checked_slice(root.work, root.work_count)?;
    let scalar = values.first().ok_or_else(observation_shape)?;
    let list = values.get(3).ok_or_else(observation_shape)?;
    let node = nodes.first().ok_or_else(observation_shape)?;
    let relationship = relationships.first().ok_or_else(observation_shape)?;
    let report_work = reports
        .first()
        .map_or((0, 0), |report| (report.work.start, report.work.count));
    let copied = work
        .iter()
        .find(|row| row.kind == ZeGraphWorkKind::ZeGraphWorkCopiedBytes as u32)
        .ok_or_else(observation_shape)?;
    let payload = checked_slice(root.pool.bytes, root.pool.byte_count)?.to_vec();
    if payload != expected_payload.as_bytes() {
        return Err(observation_shape());
    }
    Ok(NativeConversionObservation {
        node_id: (node.id.high, node.id.low),
        relationship_id: (relationship.id.high, relationship.id.low),
        scalar_bits: scalar.floating.to_bits(),
        list_tag: list.list_kind,
        text: (node.has_text, node.text.start, node.text.count),
        payload,
        report_work,
        global_work: (root.global_work.count, copied.value),
        peak_query_bytes,
        source_calls,
        allocation: current_allocation_fault_receipt(),
        query_charge_restored: false,
    })
}

fn checked_slice<'a, T>(
    pointer: *const T,
    count: usize,
) -> Result<&'a [T], NativeConversionFailure> {
    if count == 0 {
        return Ok(&[]);
    }
    if pointer.is_null() || pointer.align_offset(std::mem::align_of::<T>()) != 0 {
        return Err(observation_shape());
    }
    // The registered root owns this exact validated pool through observe/free.
    Ok(unsafe { std::slice::from_raw_parts(pointer, count) })
}

fn map_driver_failure(
    driver: RuntimeFailure,
    conversion: Option<ConversionError>,
) -> NativeConversionFailure {
    match conversion {
        Some(error) => conversion_failure(&error),
        None => runtime_failure(NativeConversionStage::Driver, &driver.error),
    }
}
fn conversion_failure(error: &ConversionError) -> NativeConversionFailure {
    match error {
        ConversionError::Completed(error) => match error {
            CompletedError::Source(_) => failure(
                NativeConversionStage::Native,
                NativeConversionRefusal::Context,
            ),
            CompletedError::Shape | CompletedError::Utf8 => failure(
                NativeConversionStage::Native,
                NativeConversionRefusal::Shape,
            ),
            CompletedError::Limit => failure(
                NativeConversionStage::Native,
                NativeConversionRefusal::Limit,
            ),
            CompletedError::Runtime(error) => runtime_failure(NativeConversionStage::Native, error),
        },
        ConversionError::Owner(error) => owner_failure(error),
        ConversionError::Runtime(error) => runtime_failure(NativeConversionStage::C, error),
    }
}
fn owner_failure(error: &OwnerError) -> NativeConversionFailure {
    let refusal = match error {
        OwnerError::Limit | OwnerError::InvalidShape => NativeConversionRefusal::Limit,
        OwnerError::Allocation => NativeConversionRefusal::Allocation,
        OwnerError::Memory(_) => NativeConversionRefusal::Memory,
        OwnerError::Runtime(error) => return runtime_failure(NativeConversionStage::C, error),
        OwnerError::Poisoned
        | OwnerError::Busy
        | OwnerError::RegistryFull
        | OwnerError::TokenExhausted
        | OwnerError::InvalidOwner => NativeConversionRefusal::Registry,
    };
    failure(NativeConversionStage::C, refusal)
}
fn runtime_failure(stage: NativeConversionStage, error: &RuntimeError) -> NativeConversionFailure {
    let refusal = match error {
        RuntimeError::Limit(WorkKind::CopiedBytes) => NativeConversionRefusal::Work,
        RuntimeError::Limit(_) => NativeConversionRefusal::Limit,
        RuntimeError::Value(
            QueryError::Cancelled | QueryError::ReadCancelled | QueryError::Timeout,
        ) => NativeConversionRefusal::Cancel,
        RuntimeError::Memory(_) => NativeConversionRefusal::Memory,
        RuntimeError::Value(_) | RuntimeError::Batch => NativeConversionRefusal::Other,
    };
    failure(stage, refusal)
}
fn memory_failure(error: MemoryError) -> NativeConversionFailure {
    let refusal = match error {
        MemoryError::Allocation => NativeConversionRefusal::Allocation,
        MemoryError::Limit
        | MemoryError::UnprovedInput
        | MemoryError::Store(_)
        | MemoryError::Plan(_) => NativeConversionRefusal::Memory,
        MemoryError::Value(error) => {
            return runtime_failure(NativeConversionStage::Native, &RuntimeError::Value(error));
        }
    };
    failure(NativeConversionStage::Native, refusal)
}
fn observation_shape() -> NativeConversionFailure {
    failure(NativeConversionStage::C, NativeConversionRefusal::Shape)
}
fn failure(
    stage: NativeConversionStage,
    refusal: NativeConversionRefusal,
) -> NativeConversionFailure {
    let allocation = current_allocation_fault_receipt();
    NativeConversionFailure {
        stage,
        refusal,
        allocation_matching_sites: allocation.matching_sites,
        allocation_fires: allocation.fires,
        query_charge_restored: false,
    }
}
