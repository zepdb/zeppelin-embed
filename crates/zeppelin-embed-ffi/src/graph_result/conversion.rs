//! Private native completed-result conversion into the registered C arena.
//! ZE-68/ZE-69 retain the production coordinator and public ABI integration.
#![allow(
    dead_code,
    reason = "ZE-141 builds the private seam consumed by ZE-68/ZE-69"
)]

use super::*;
use std::mem::size_of;
use std::ptr::NonNull;
use zeppelin_embed::lifecycle::SearchTier;
use zeppelin_embed::property_graph::EntityId;
use zeppelin_embed::property_graph::query::completed::{
    ActualTier, CandidateCoverage, CompletedError, LegState, ListKind, Outcome, Pools,
    PreparedGraphResult, ResultSource, ScorePrecision, SearchKind, SearchReport, Span, Value,
    ValueKinds,
};
use zeppelin_embed::property_graph::query::runtime::{Execution, FrozenOutput, WorkCounters};

const GLOBAL_WORK_COUNT: usize = 23;
const REPORT_WORK_COUNT: usize = 22;
const MAX_REPORTS: usize = 8;

const WORK_KINDS: [(WorkKind, u32); REPORT_WORK_COUNT] = [
    (
        WorkKind::OperatorRows,
        ZeGraphWorkKind::ZeGraphWorkOperatorRows as u32,
    ),
    (
        WorkKind::AdjacencyEntries,
        ZeGraphWorkKind::ZeGraphWorkAdjacencyEntries as u32,
    ),
    (
        WorkKind::Expressions,
        ZeGraphWorkKind::ZeGraphWorkExpressions as u32,
    ),
    (
        WorkKind::HashProbes,
        ZeGraphWorkKind::ZeGraphWorkHashProbes as u32,
    ),
    (
        WorkKind::CompletedRows,
        ZeGraphWorkKind::ZeGraphWorkCompletedRows as u32,
    ),
    (
        WorkKind::CompletedBytes,
        ZeGraphWorkKind::ZeGraphWorkCompletedBytes as u32,
    ),
    (
        WorkKind::PreparedPayloadBytes,
        ZeGraphWorkKind::ZeGraphWorkPreparedPayloadBytes as u32,
    ),
    (
        WorkKind::CompletedAbiBytes,
        ZeGraphWorkKind::ZeGraphWorkCompletedAbiBytes as u32,
    ),
    (
        WorkKind::VectorCoordinates,
        ZeGraphWorkKind::ZeGraphWorkVectorCoordinates as u32,
    ),
    (
        WorkKind::VectorBytes,
        ZeGraphWorkKind::ZeGraphWorkVectorBytes as u32,
    ),
    (
        WorkKind::LexicalPostings,
        ZeGraphWorkKind::ZeGraphWorkLexicalPostings as u32,
    ),
    (
        WorkKind::LexicalBlocks,
        ZeGraphWorkKind::ZeGraphWorkLexicalBlocks as u32,
    ),
    (
        WorkKind::SearchInvocations,
        ZeGraphWorkKind::ZeGraphWorkSearchInvocations as u32,
    ),
    (
        WorkKind::Lookups,
        ZeGraphWorkKind::ZeGraphWorkLookups as u32,
    ),
    (WorkKind::Scans, ZeGraphWorkKind::ZeGraphWorkScans as u32),
    (WorkKind::Paths, ZeGraphWorkKind::ZeGraphWorkPaths as u32),
    (WorkKind::RowsIn, ZeGraphWorkKind::ZeGraphWorkRowsIn as u32),
    (
        WorkKind::RowsOut,
        ZeGraphWorkKind::ZeGraphWorkRowsOut as u32,
    ),
    (
        WorkKind::JoinProbes,
        ZeGraphWorkKind::ZeGraphWorkJoinProbes as u32,
    ),
    (
        WorkKind::GroupKeys,
        ZeGraphWorkKind::ZeGraphWorkGroupKeys as u32,
    ),
    (
        WorkKind::EligibilityEntries,
        ZeGraphWorkKind::ZeGraphWorkEligibilityEntries as u32,
    ),
    (
        WorkKind::CopiedBytes,
        ZeGraphWorkKind::ZeGraphWorkCopiedBytes as u32,
    ),
];

/// Exact private conversion failure; no public ABI mapping is chosen here.
#[derive(Debug)]
pub(crate) enum ConversionError {
    /// Native source/copy/validation rejection.
    Completed(CompletedError),
    /// C arena/registry ownership rejection.
    Owner(OwnerError),
    /// Frozen-output represented-limit rejection.
    Runtime(RuntimeError),
}
impl From<CompletedError> for ConversionError {
    fn from(error: CompletedError) -> Self {
        Self::Completed(error)
    }
}
impl From<OwnerError> for ConversionError {
    fn from(error: OwnerError) -> Self {
        Self::Owner(error)
    }
}
impl From<RuntimeError> for ConversionError {
    fn from(error: RuntimeError) -> Self {
        Self::Runtime(error)
    }
}

#[derive(Clone, Copy)]
struct NativeGeometry {
    counts: PoolCounts,
    report_work_starts: [u32; MAX_REPORTS],
}
impl NativeGeometry {
    fn new(pools: Pools<'_>) -> Result<Self, OwnerError> {
        let report_rows = REPORT_WORK_COUNT
            .checked_mul(pools.reports.len())
            .ok_or(OwnerError::Limit)?;
        let work_count = GLOBAL_WORK_COUNT
            .checked_add(report_rows)
            .ok_or(OwnerError::Limit)?;
        let _ = u32::try_from(work_count).map_err(|_| OwnerError::Limit)?;
        let mut report_work_starts = [0; MAX_REPORTS];
        for (ordinal, start) in report_work_starts
            .iter_mut()
            .take(pools.reports.len())
            .enumerate()
        {
            let value = GLOBAL_WORK_COUNT
                .checked_add(
                    REPORT_WORK_COUNT
                        .checked_mul(ordinal)
                        .ok_or(OwnerError::Limit)?,
                )
                .ok_or(OwnerError::Limit)?;
            *start = u32::try_from(value).map_err(|_| OwnerError::Limit)?;
        }
        Ok(Self {
            counts: PoolCounts {
                values: pools.values.len(),
                children: pools.children.len(),
                bytes: pools.bytes.len(),
                nodes: pools.nodes.len(),
                relationships: pools.relationships.len(),
                properties: pools.properties.len(),
                names: pools.names.len(),
                vectors: pools.vectors.len(),
                columns: pools.columns.len(),
                cells: pools.cells.len(),
                receipts: pools.receipts.len(),
                reports: pools.reports.len(),
                diagnostics: 0,
                work: work_count,
            },
            report_work_starts,
        })
    }
}

struct NativeInitializer<'a, 'm, 'g> {
    native: &'a PreparedGraphResult<'m, 'g>,
    geometry: NativeGeometry,
}

struct GlobalWorkSlots {
    pointer: NonNull<[ZeGraphWorkCounter; GLOBAL_WORK_COUNT]>,
}
impl GlobalWorkSlots {
    fn new(response: &PreparedResponse<'_, '_>) -> Result<Self, OwnerError> {
        let root = response.descriptor();
        if root.global_work.start != 0
            || root.global_work.count as usize != GLOBAL_WORK_COUNT
            || root.work_count < GLOBAL_WORK_COUNT
        {
            return Err(OwnerError::InvalidShape);
        }
        let pointer = NonNull::new(
            root.work
                .cast_mut()
                .cast::<[ZeGraphWorkCounter; GLOBAL_WORK_COUNT]>(),
        )
        .ok_or(OwnerError::InvalidShape)?;
        Ok(Self { pointer })
    }
    fn finalize(self, counters: WorkCounters, peak_query_bytes: usize) {
        // The capability was created from the still-private registered arena;
        // PreparedNativeResponse uniquely owns it until this consuming call.
        unsafe {
            let slots = &mut *self.pointer.as_ptr();
            for (slot, (kind, _)) in slots.iter_mut().zip(WORK_KINDS) {
                slot.value = counters.get(kind);
            }
            (*self.pointer.as_ptr().cast::<ZeGraphWorkCounter>().add(22)).value =
                peak_query_bytes as u64;
        }
    }
}

/// Private driver output retaining unique prepared registry ownership.
pub(crate) struct PreparedNativeResponse<'m, 'g> {
    response: PreparedResponse<'m, 'g>,
    global_work: GlobalWorkSlots,
    outcome: SuccessfulOutcome,
}

/// Finalized private owner; only the coordinator-facing parts can be consumed.
pub(crate) struct FinalizedNativeResponse<'m, 'g> {
    response: PreparedResponse<'m, 'g>,
    outcome: SuccessfulOutcome,
}
impl<'m, 'g> FinalizedNativeResponse<'m, 'g> {
    /// Returns the existing owner and authenticated successful outcome.
    pub(crate) fn into_parts(self) -> (PreparedResponse<'m, 'g>, SuccessfulOutcome) {
        (self.response, self.outcome)
    }
    /// Write path: detaches the finalized owner before commit and discards
    /// the provisional copy-time outcome; only the post-commit settle
    /// decides the published one.
    pub(crate) fn into_pending(self) -> PendingResponse {
        self.response.detach()
    }
}

/// Copies one authentic native result and converts it under the same context.
pub(crate) fn prepare_native<'v, 'm, 'g>(
    registry: &'static GraphResultRegistry,
    source: &impl ResultSource,
    context: &mut RuntimeContext<'v, 'm, 'g>,
) -> Result<FrozenOutput<PreparedNativeResponse<'m, 'g>>, ConversionError> {
    let native = PreparedGraphResult::copy_from(source, context)?;
    let geometry = NativeGeometry::new(native.pools())?;
    let outcome = map_outcome(native.metadata().outcome)?;
    let metadata = ResponseMetadata {
        row_count: native.metadata().rows as usize,
        admitted_generation: Some(native.metadata().generation.get()),
        global_work: ZeGraphRange {
            start: 0,
            count: u32::try_from(GLOBAL_WORK_COUNT).map_err(|_| OwnerError::Limit)?,
        },
    };
    let native_bytes = native.represented_bytes();
    let initializer = NativeInitializer {
        native: &native,
        geometry,
    };
    let largest_wrapper = size_of::<FrozenOutput<PreparedNativeResponse<'m, 'g>>>()
        .max(size_of::<Execution<PreparedNativeResponse<'m, 'g>>>())
        .max(size_of::<FinalizedNativeResponse<'m, 'g>>());
    let wrapper_extra = largest_wrapper.saturating_sub(size_of::<PreparedResponse<'m, 'g>>());
    let initializer_controls = size_of::<NativeInitializer<'_, 'm, 'g>>()
        .checked_add(wrapper_extra)
        .ok_or(OwnerError::Limit)?;
    let response = registry.prepare_with(
        context,
        geometry.counts,
        metadata,
        initializer_controls,
        move |arena, plan, context| fill_native(arena, plan, initializer, metadata, context),
    )?;
    let c_bytes = response.represented_bytes();
    let global_work = GlobalWorkSlots::new(&response)?;
    let output = PreparedNativeResponse {
        response,
        global_work,
        outcome,
    };
    Ok(FrozenOutput::new(
        output,
        metadata.row_count,
        native_bytes,
        c_bytes,
    )?)
}

/// Fills the 23 fixed final rows from the one completed driver snapshot.
pub(crate) fn finalize_native<'m, 'g>(
    execution: Execution<PreparedNativeResponse<'m, 'g>>,
) -> FinalizedNativeResponse<'m, 'g> {
    let PreparedNativeResponse {
        response,
        global_work,
        outcome,
    } = execution.output;
    global_work.finalize(execution.counters, execution.peak_query_bytes);
    FinalizedNativeResponse { response, outcome }
}

fn map_outcome(outcome: Outcome) -> Result<SuccessfulOutcome, CompletedError> {
    match outcome {
        Outcome::Read => Ok(SuccessfulOutcome::Read),
        Outcome::Committed { changed } => std::num::NonZeroU64::new(changed.get())
            .map(SuccessfulOutcome::Committed)
            .ok_or(CompletedError::Shape),
        Outcome::Replayed => Ok(SuccessfulOutcome::Replayed),
        Outcome::NoOp => Ok(SuccessfulOutcome::NoOp),
    }
}

fn range(span: Span) -> ZeGraphRange {
    ZeGraphRange {
        start: span.start,
        count: span.len,
    }
}
fn node_id(value: u128) -> ZeNodeId {
    ZeNodeId {
        high: (value >> 64) as u64,
        low: value as u64,
    }
}
fn rel_id(value: u128) -> ZeRelId {
    ZeRelId {
        high: (value >> 64) as u64,
        low: value as u64,
    }
}
fn map_value(value: &Value) -> ZeGraphValue {
    // Every field is a scalar or scalar-only C range, and zero is the canonical
    // inactive representation for every tag.
    let mut output: ZeGraphValue = unsafe { std::mem::zeroed() };
    output.abi_size = size_of::<ZeGraphValue>() as u32;
    match value {
        Value::Null => output.tag = ZeGraphValueTag::ZeGraphValueNull as u32,
        Value::Bool(boolean) => {
            output.tag = ZeGraphValueTag::ZeGraphValueBool as u32;
            output.boolean = u32::from(*boolean);
        }
        Value::I64(integer) => {
            output.tag = ZeGraphValueTag::ZeGraphValueI64 as u32;
            output.integer = *integer;
        }
        Value::F64(bits) => {
            output.tag = ZeGraphValueTag::ZeGraphValueF64 as u32;
            output.floating = f64::from_bits(*bits);
        }
        Value::String(span) => {
            output.tag = ZeGraphValueTag::ZeGraphValueString as u32;
            output.range = range(*span);
        }
        Value::Node(index) => {
            output.tag = ZeGraphValueTag::ZeGraphValueNode as u32;
            output.entity_index = *index;
        }
        Value::Relationship(index) => {
            output.tag = ZeGraphValueTag::ZeGraphValueRelationship as u32;
            output.entity_index = *index;
        }
        Value::List { children, element } => {
            output.tag = ZeGraphValueTag::ZeGraphValueList as u32;
            output.range = range(*children);
            output.list_kind = match element {
                ListKind::Query => ZeGraphListKind::ZeGraphListQuery,
                ListKind::Bool => ZeGraphListKind::ZeGraphListBool,
                ListKind::I64 => ZeGraphListKind::ZeGraphListI64,
                ListKind::F64 => ZeGraphListKind::ZeGraphListF64,
                ListKind::String => ZeGraphListKind::ZeGraphListString,
                ListKind::Empty => ZeGraphListKind::ZeGraphListEmpty,
            } as u32;
        }
    }
    output
}
fn property(value: &zeppelin_embed::property_graph::query::completed::Property) -> ZeGraphProperty {
    ZeGraphProperty {
        abi_size: size_of::<ZeGraphProperty>() as u32,
        abi_reserved: 0,
        name: range(value.name),
        value: value.value.0,
        reserved: 0,
    }
}
fn node(value: &zeppelin_embed::property_graph::query::completed::Node) -> ZeGraphNode {
    let (has_key, namespace_name, key) = match value.key {
        Some(key) => (1, range(key.namespace), range(key.value)),
        None => (0, ZeGraphRange::default(), ZeGraphRange::default()),
    };
    let (has_text, text) = match value.text {
        Some(text) => (1, range(text)),
        None => (0, ZeGraphRange::default()),
    };
    let (has_vector, vector) = match value.vector {
        Some(vector) => (1, range(vector)),
        None => (0, ZeGraphRange::default()),
    };
    ZeGraphNode {
        abi_size: size_of::<ZeGraphNode>() as u32,
        abi_reserved: 0,
        id: node_id(value.id.get()),
        has_key,
        has_text,
        has_vector,
        reserved: 0,
        namespace_name,
        key,
        revision: value.revision.get(),
        last_change_generation: value.generation.get(),
        properties: range(value.properties),
        text,
        vector,
        labels: range(value.labels),
    }
}
fn relationship(
    value: &zeppelin_embed::property_graph::query::completed::Relationship,
) -> ZeGraphRelationship {
    let (has_key, namespace_name, key) = match value.key {
        Some(key) => (1, range(key.namespace), range(key.value)),
        None => (0, ZeGraphRange::default(), ZeGraphRange::default()),
    };
    ZeGraphRelationship {
        abi_size: size_of::<ZeGraphRelationship>() as u32,
        abi_reserved: 0,
        id: rel_id(value.id.get()),
        source: node_id(value.source.get()),
        target: node_id(value.target.get()),
        has_key,
        reserved: 0,
        namespace_name,
        key,
        revision: value.revision.get(),
        last_change_generation: value.generation.get(),
        properties: range(value.properties),
        relationship_type: range(value.relationship_type),
    }
}
fn kinds(value: ValueKinds) -> u32 {
    let mut bits = 0;
    for (native, c) in [
        (ValueKinds::NULL, 1),
        (ValueKinds::BOOL, 2),
        (ValueKinds::I64, 4),
        (ValueKinds::F64, 8),
        (ValueKinds::STRING, 16),
        (ValueKinds::NODE, 32),
        (ValueKinds::REL, 64),
        (ValueKinds::LIST, 128),
    ] {
        if value.contains(native) {
            bits |= c;
        }
    }
    bits
}
fn column(value: &zeppelin_embed::property_graph::query::completed::Column) -> ZeGraphColumn {
    ZeGraphColumn {
        abi_size: size_of::<ZeGraphColumn>() as u32,
        abi_reserved: 0,
        name: range(value.name),
        kinds: kinds(value.kinds),
        reserved: 0,
    }
}
fn receipt(value: &zeppelin_embed::property_graph::query::completed::Receipt) -> ZeGraphReceipt {
    let (entity_kind, node, relationship) = match value.receipt.entity {
        EntityId::Node(id) => (
            ZeGraphEntityKind::ZeGraphEntityNode as u32,
            node_id(id.get()),
            ZeRelId::default(),
        ),
        EntityId::Relationship(id) => (
            ZeGraphEntityKind::ZeGraphEntityRelationship as u32,
            ZeNodeId::default(),
            rel_id(id.get()),
        ),
    };
    ZeGraphReceipt {
        abi_size: size_of::<ZeGraphReceipt>() as u32,
        abi_reserved: 0,
        item: value.item_index,
        entity_kind,
        disposition: if value.receipt.replayed {
            ZeGraphDisposition::ZeGraphDispositionReplayed as u32
        } else {
            ZeGraphDisposition::ZeGraphDispositionCommitted as u32
        },
        deleted: u32::from(value.deleted),
        node,
        relationship,
        revision: value.receipt.revision.get(),
        generation: value.receipt.generation.get(),
    }
}
fn report(value: &SearchReport, work_start: u32) -> ZeGraphSearchReport {
    let (has_requested_tier, requested_tier) = match value.requested_tier {
        None => (0, 0),
        Some(SearchTier::Auto) => (1, ZeGraphTier::ZeGraphTierAuto as u32),
        Some(SearchTier::Exact) => (1, ZeGraphTier::ZeGraphTierExact as u32),
        Some(SearchTier::Scan) => (1, ZeGraphTier::ZeGraphTierScan as u32),
        Some(SearchTier::Graph(_)) => (1, ZeGraphTier::ZeGraphTierGraph as u32),
    };
    let (has_actual_tier, actual_tier) = match value.actual_tier {
        None => (0, 0),
        Some(ActualTier::Exact) => (1, ZeGraphTier::ZeGraphTierExact as u32),
        Some(ActualTier::Scan) => (1, ZeGraphTier::ZeGraphTierScan as u32),
        Some(ActualTier::Graph) => (1, ZeGraphTier::ZeGraphTierGraph as u32),
    };
    let (has_document_epoch, document_epoch) = option_u64(value.document_epoch);
    let (has_query_epoch, query_epoch) = option_u64(value.query_epoch);
    let (has_tokenizer_epoch, tokenizer_epoch) = option_u64(value.tokenizer_epoch);
    ZeGraphSearchReport {
        abi_size: size_of::<ZeGraphSearchReport>() as u32,
        abi_reserved: 0,
        call_id: value.call.0,
        kind: match value.kind {
            SearchKind::Vector => ZeGraphSearchKind::ZeGraphSearchVector,
            SearchKind::Lexical => ZeGraphSearchKind::ZeGraphSearchText,
            SearchKind::Hybrid => ZeGraphSearchKind::ZeGraphSearchHybrid,
        } as u32,
        generation: value.generation.get(),
        has_requested_tier,
        requested_tier,
        has_actual_tier,
        actual_tier,
        precision: match value.precision {
            ScorePrecision::NotApplicable => ZeGraphScorePrecision::ZeGraphPrecisionNotApplicable,
            ScorePrecision::Original => ZeGraphScorePrecision::ZeGraphPrecisionOriginal,
            ScorePrecision::Quantized => ZeGraphScorePrecision::ZeGraphPrecisionQuantized,
            ScorePrecision::Mixed => ZeGraphScorePrecision::ZeGraphPrecisionMixed,
        } as u32,
        coverage: match value.coverage {
            CandidateCoverage::Exact => ZeGraphCandidateCoverage::ZeGraphCoverageExact,
            CandidateCoverage::Approximate => ZeGraphCandidateCoverage::ZeGraphCoverageApproximate,
        } as u32,
        vector_leg: leg(value.vector_leg),
        lexical_leg: leg(value.lexical_leg),
        has_document_epoch,
        has_query_epoch,
        has_tokenizer_epoch,
        cross_score_complete: u32::from(value.cross_score_complete),
        document_epoch,
        query_epoch,
        tokenizer_epoch,
        effective_alpha: f64::from_bits(value.effective_alpha_bits),
        normalization_version: value.normalization_version,
        rules_version: value.rules_version,
        candidate_count: value.candidate_count,
        cross_scored_count: value.cross_scored_count,
        fallback_count: value.fallback_count,
        work: ZeGraphRange {
            start: work_start,
            count: REPORT_WORK_COUNT as u32,
        },
    }
}
fn option_u64(value: Option<u64>) -> (u32, u64) {
    match value {
        Some(value) => (1, value),
        None => (0, 0),
    }
}
fn leg(value: LegState) -> u32 {
    (match value {
        LegState::NotRequested => ZeGraphLegState::ZeGraphLegNotRequested,
        LegState::Nonempty => ZeGraphLegState::ZeGraphLegNonempty,
        LegState::NoIndexedPopulation => ZeGraphLegState::ZeGraphLegNoIndexedPopulation,
        LegState::NoEligibleMembers => ZeGraphLegState::ZeGraphLegNoEligibleMembers,
        LegState::NoQueryMatches => ZeGraphLegState::ZeGraphLegNoQueryMatches,
    }) as u32
}
fn work_counter(kind: u32, value: u64) -> ZeGraphWorkCounter {
    ZeGraphWorkCounter {
        abi_size: size_of::<ZeGraphWorkCounter>() as u32,
        abi_reserved: 0,
        kind,
        reserved: 0,
        value,
    }
}
fn work_kind(index: usize) -> (WorkKind, u32) {
    // Callers prove index <22. A raw read avoids an impossible fallback or a
    // panic-capable index at this fixed, statically sized table.
    unsafe { *WORK_KINDS.as_ptr().add(index) }
}

fn fill_native(
    arena: &AlignedArena,
    plan: &ArenaLayout,
    initializer: NativeInitializer<'_, '_, '_>,
    metadata: ResponseMetadata,
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<ZeGraphResponse, OwnerError> {
    let pools = initializer.native.pools();
    let mut root = empty_response();
    root.row_count = metadata.row_count;
    root.has_admitted_generation = 1;
    root.admitted_generation = metadata.admitted_generation.unwrap_or(0);
    root.global_work = metadata.global_work;
    let [
        off_values,
        off_children,
        off_bytes,
        off_nodes,
        off_relationships,
        off_properties,
        off_names,
        off_vectors,
        off_columns,
        off_cells,
        off_receipts,
        off_reports,
        _off_diagnostics,
        off_work,
    ] = plan.offsets;
    root.pool.values = unsafe {
        arena.map(off_values, pools.values, context, |_, value| {
            map_value(value)
        })?
    };
    root.pool.value_count = pools.values.len();
    root.pool.children =
        unsafe { arena.map(off_children, pools.children, context, |_, value| value.0)? };
    root.pool.child_count = pools.children.len();
    root.pool.bytes = unsafe { arena.copy(off_bytes, pools.bytes, context)? };
    root.pool.byte_count = pools.bytes.len();
    root.pool.nodes =
        unsafe { arena.map(off_nodes, pools.nodes, context, |_, value| node(value))? };
    root.pool.node_count = pools.nodes.len();
    root.pool.relationships = unsafe {
        arena.map(
            off_relationships,
            pools.relationships,
            context,
            |_, value| relationship(value),
        )?
    };
    root.pool.relationship_count = pools.relationships.len();
    root.pool.properties = unsafe {
        arena.map(off_properties, pools.properties, context, |_, value| {
            property(value)
        })?
    };
    root.pool.property_count = pools.properties.len();
    root.pool.names =
        unsafe { arena.map(off_names, pools.names, context, |_, value| range(*value))? };
    root.pool.name_count = pools.names.len();
    root.pool.vectors = unsafe {
        arena.map(off_vectors, pools.vectors, context, |_, bits| {
            f32::from_bits(*bits)
        })?
    };
    root.pool.vector_count = pools.vectors.len();
    root.columns = unsafe {
        arena.map(off_columns, pools.columns, context, |_, value| {
            column(value)
        })?
    };
    root.column_count = pools.columns.len();
    root.cells = unsafe { arena.map(off_cells, pools.cells, context, |_, value| value.0)? };
    root.cell_count = pools.cells.len();
    root.receipts = unsafe {
        arena.map(off_receipts, pools.receipts, context, |_, value| {
            receipt(value)
        })?
    };
    root.receipt_count = pools.receipts.len();
    let starts = initializer.geometry.report_work_starts;
    root.reports = unsafe {
        arena.map(off_reports, pools.reports, context, |ordinal, value| {
            // Geometry checked every ordinal and u32 conversion before allocation.
            report(value, *starts.as_ptr().add(ordinal))
        })?
    };
    root.report_count = pools.reports.len();
    root.diagnostics = std::ptr::null();
    root.diagnostic_count = 0;
    let reports = pools.reports;
    let work_count = initializer.geometry.counts.work;
    root.work = unsafe {
        arena.generate(off_work, work_count, context, |index| {
            if index < REPORT_WORK_COUNT {
                let (_, c_kind) = work_kind(index);
                work_counter(c_kind, 0)
            } else if index == REPORT_WORK_COUNT {
                work_counter(ZeGraphWorkKind::ZeGraphWorkPeakOwnedBytes as u32, 0)
            } else {
                let local = index - GLOBAL_WORK_COUNT;
                let report_ordinal = local / REPORT_WORK_COUNT;
                let work_ordinal = local % REPORT_WORK_COUNT;
                let report = &*reports.as_ptr().add(report_ordinal);
                let (kind, c_kind) = work_kind(work_ordinal);
                work_counter(c_kind, report.work.get(kind))
            }
        })?
    };
    root.work_count = work_count;
    Ok(root)
}

#[cfg(test)]
mod tests;
