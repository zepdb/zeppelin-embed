//! Aligned, pre-registered graph result ownership (internal Rust component).
//! Real native conversion and public C entry points remain separate integration.

use crate::*;
use std::alloc::Layout;
use std::alloc::{alloc, dealloc};
use std::ptr::NonNull;
use zeppelin_embed::property_graph::query::resources::{MemoryError, QueryExternalReservation};
use zeppelin_embed::property_graph::query::runtime::{RuntimeContext, RuntimeError, WorkKind};
mod outcome;
pub use outcome::{OperationOutcome, OutcomeCell, OutcomeTransitionError};
pub(crate) mod conversion;
mod coordinator;
pub use coordinator::{GuardedWrite, WriteAttempt, WriteInterrupted, run_potential_write};
mod registration;
/// Scoped allocation-site injection for the canonical opt-in test runner.
#[cfg(feature = "graph-result-test-support")]
pub mod test_support;
pub use registration::{FreeReport, GraphResultRegistry, PendingResponse, PreparedResponse};

/// A failure before publication or a rejected free. No partial result escapes.
#[derive(Debug)]
pub enum OwnerError {
    /// Checked size or completed ABI allowance exceeded.
    Limit,
    /// Actual system allocation returned null after reservation.
    Allocation,
    /// Authentic query/shared reservation failed.
    Memory(MemoryError),
    /// Real retained-view, cancellation or cumulative work check failed.
    Runtime(RuntimeError),
    /// Registry lock was poisoned; only private abort cleanup may recover it.
    Poisoned,
    /// Another caller owns the registry gate; retry preserves the descriptor.
    Busy,
    /// Configured outstanding-result admission bound reached.
    RegistryFull,
    /// Monotone owner tokens cannot be reused after exhaustion.
    TokenExhausted,
    /// Descriptor is private, stale, foreign, or altered.
    InvalidOwner,
    /// Root rows, columns, cells, or global work range disagree.
    InvalidShape,
}
impl From<MemoryError> for OwnerError {
    fn from(error: MemoryError) -> Self {
        Self::Memory(error)
    }
}
impl From<RuntimeError> for OwnerError {
    fn from(error: RuntimeError) -> Self {
        Self::Runtime(error)
    }
}
impl std::fmt::Display for OwnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Memory(e) => e.fmt(f),
            Self::Runtime(e) => e.fmt(f),
            Self::Limit => f.write_str("graph response capacity limit"),
            Self::Allocation => f.write_str("graph response allocation failed"),
            Self::Busy => f.write_str("graph response registry busy"),
            Self::Poisoned => f.write_str("graph response registry poisoned"),
            Self::RegistryFull => f.write_str("graph response registry full"),
            Self::TokenExhausted => f.write_str("graph response owner tokens exhausted"),
            Self::InvalidOwner => f.write_str("invalid graph response owner"),
            Self::InvalidShape => f.write_str("invalid graph response shape"),
        }
    }
}
impl std::error::Error for OwnerError {}

#[derive(Clone, Copy, Default)]
struct PoolCounts {
    values: usize,
    children: usize,
    bytes: usize,
    nodes: usize,
    relationships: usize,
    properties: usize,
    names: usize,
    vectors: usize,
    columns: usize,
    cells: usize,
    receipts: usize,
    reports: usize,
    diagnostics: usize,
    work: usize,
}
impl PoolCounts {
    const fn ordered(self) -> [usize; 14] {
        [
            self.values,
            self.children,
            self.bytes,
            self.nodes,
            self.relationships,
            self.properties,
            self.names,
            self.vectors,
            self.columns,
            self.cells,
            self.receipts,
            self.reports,
            self.diagnostics,
            self.work,
        ]
    }
}

struct ArenaLayout {
    layout: Layout,
    offsets: [usize; 14],
}
impl ArenaLayout {
    fn new(counts: PoolCounts) -> Result<Self, OwnerError> {
        let elements = [
            Layout::new::<ZeGraphValue>(),
            Layout::new::<u32>(),
            Layout::new::<u8>(),
            Layout::new::<ZeGraphNode>(),
            Layout::new::<ZeGraphRelationship>(),
            Layout::new::<ZeGraphProperty>(),
            Layout::new::<ZeGraphRange>(),
            Layout::new::<f32>(),
            Layout::new::<ZeGraphColumn>(),
            Layout::new::<u32>(),
            Layout::new::<ZeGraphReceipt>(),
            Layout::new::<ZeGraphSearchReport>(),
            Layout::new::<ZeGraphDiagnostic>(),
            Layout::new::<ZeGraphWorkCounter>(),
        ];
        let mut layout = Layout::from_size_align(0, 1).map_err(|_| OwnerError::Limit)?;
        let mut offsets = [0; 14];
        for ((element, count), offset) in
            elements.into_iter().zip(counts.ordered()).zip(&mut offsets)
        {
            let size = element.size().checked_mul(count).ok_or(OwnerError::Limit)?;
            let array =
                Layout::from_size_align(size, element.align()).map_err(|_| OwnerError::Limit)?;
            let (extended, at) = layout.extend(array).map_err(|_| OwnerError::Limit)?;
            layout = extended;
            *offset = at;
        }
        let layout = layout.pad_to_align();
        if layout.size() > 4 * 1024 * 1024 {
            return Err(OwnerError::Limit);
        }
        Ok(Self { layout, offsets })
    }
}

#[cfg(test)]
mod tests;

/// Already converted typed input slices, copied while their actual producer
/// owners remain live. This descriptor is not a capacity/ownership proof.
/// ZE-68 must retain and charge those real source owners throughout conversion.
#[derive(Clone, Copy, Default)]
pub struct ResponseParts<'a> {
    /// Initialized values in the fixed C schema.
    pub values: &'a [ZeGraphValue],
    /// Initialized children in the fixed C schema.
    pub children: &'a [u32],
    /// Initialized bytes in the fixed C schema.
    pub bytes: &'a [u8],
    /// Initialized nodes in the fixed C schema.
    pub nodes: &'a [ZeGraphNode],
    /// Initialized relationships in the fixed C schema.
    pub relationships: &'a [ZeGraphRelationship],
    /// Initialized properties in the fixed C schema.
    pub properties: &'a [ZeGraphProperty],
    /// Initialized names in the fixed C schema.
    pub names: &'a [ZeGraphRange],
    /// Initialized vectors in the fixed C schema.
    pub vectors: &'a [f32],
    /// Initialized columns in the fixed C schema.
    pub columns: &'a [ZeGraphColumn],
    /// Initialized cells in the fixed C schema.
    pub cells: &'a [u32],
    /// Initialized receipts in the fixed C schema.
    pub receipts: &'a [ZeGraphReceipt],
    /// Initialized reports in the fixed C schema.
    pub reports: &'a [ZeGraphSearchReport],
    /// Initialized diagnostics in the fixed C schema.
    pub diagnostics: &'a [ZeGraphDiagnostic],
    /// Initialized work in the fixed C schema.
    pub work: &'a [ZeGraphWorkCounter],
}
impl ResponseParts<'_> {
    fn counts(&self) -> PoolCounts {
        PoolCounts {
            values: self.values.len(),
            children: self.children.len(),
            bytes: self.bytes.len(),
            nodes: self.nodes.len(),
            relationships: self.relationships.len(),
            properties: self.properties.len(),
            names: self.names.len(),
            vectors: self.vectors.len(),
            columns: self.columns.len(),
            cells: self.cells.len(),
            receipts: self.receipts.len(),
            reports: self.reports.len(),
            diagnostics: self.diagnostics.len(),
            work: self.work.len(),
        }
    }
}
/// Fixed response metadata. Coordinator/admission authenticity is external.
#[derive(Clone, Copy)]
pub struct ResponseMetadata {
    /// Complete row count; must match row-major cells and columns.
    pub row_count: usize,
    /// Genuine admitted generation when one exists, including generation zero.
    pub admitted_generation: Option<u64>,
    /// Cumulative global counter range, separate from invocation ranges.
    pub global_work: ZeGraphRange,
}
impl ResponseMetadata {
    /// Creates metadata without inventing an admitted generation.
    pub const fn new(row_count: usize, admitted_generation: Option<u64>) -> Self {
        Self {
            row_count,
            admitted_generation,
            global_work: ZeGraphRange { start: 0, count: 0 },
        }
    }
}
/// Only successful payload outcomes can publish an owned result. Unknown write
/// outcomes discard prepared payloads; error metadata is delivered separately.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SuccessfulOutcome {
    /// A read never attempted a mutation.
    Read,
    /// Coordinator-established durable changed generation.
    Committed(std::num::NonZeroU64),
    /// All operations replayed existing durable receipts.
    Replayed,
    /// No durable change.
    NoOp,
}
impl SuccessfulOutcome {
    fn apply(self, root: &mut ZeGraphResponse) {
        let (disposition, generation) = match self {
            Self::Read => (ZeGraphDisposition::ZeGraphDispositionNotApplicable, None),
            Self::Committed(g) => (
                ZeGraphDisposition::ZeGraphDispositionCommitted,
                Some(g.get()),
            ),
            Self::Replayed => (ZeGraphDisposition::ZeGraphDispositionReplayed, None),
            Self::NoOp => (ZeGraphDisposition::ZeGraphDispositionNoOp, None),
        };
        root.disposition = disposition as u32;
        root.has_changed_generation = u32::from(generation.is_some());
        root.changed_generation = generation.unwrap_or(0);
    }
}
/// The decided outcome of a potential write, known only after its commit
/// tail has run. A read or an unknown outcome never settles a result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteSettlement {
    /// The commit published this durable changed generation.
    Committed(std::num::NonZeroU64),
    /// All operations replayed existing durable receipts.
    Replayed,
    /// No durable change.
    NoOp,
}
impl WriteSettlement {
    const fn outcome(self) -> SuccessfulOutcome {
        match self {
            Self::Committed(changed) => SuccessfulOutcome::Committed(changed),
            Self::Replayed => SuccessfulOutcome::Replayed,
            Self::NoOp => SuccessfulOutcome::NoOp,
        }
    }
    const fn changed(self) -> Option<u64> {
        match self {
            Self::Committed(changed) => Some(changed.get()),
            Self::Replayed | Self::NoOp => None,
        }
    }
}
/// Canonical empty descriptor; no backing and no registry ownership.
pub fn empty_response() -> ZeGraphResponse {
    // Every field is a scalar, raw pointer, or recursively such a C struct.
    let mut root: ZeGraphResponse = unsafe { std::mem::zeroed() };
    root.abi_size = std::mem::size_of::<ZeGraphResponse>() as u32;
    root.pool.abi_size = std::mem::size_of::<ZeGraphValuePool>() as u32;
    root
}

// Every caller has reserved the exact nonzero Layout before this site.
unsafe fn allocate_raw(layout: Layout) -> *mut u8 {
    #[cfg(feature = "graph-result-test-support")]
    if test_support::refuse_allocation() {
        return std::ptr::null_mut();
    }
    unsafe { alloc(layout) }
}

struct AlignedArena {
    pointer: NonNull<u8>,
    layout: Layout,
}
impl AlignedArena {
    fn allocate(layout: Layout) -> Result<Self, OwnerError> {
        // Layout size zero must not be passed to the allocator. Empty pools
        // always publish null pointers and never access this dangling sentinel.
        let pointer = if layout.size() == 0 {
            NonNull::dangling()
        } else {
            NonNull::new(unsafe { allocate_raw(layout) }).ok_or(OwnerError::Allocation)?
        };
        Ok(Self { pointer, layout })
    }
    // Caller supplies the corresponding checked typed layout offset. These
    // private calls are fixed below; no external caller selects T or offset.
    unsafe fn copy<T: Copy>(
        &self,
        offset: usize,
        source: &[T],
        context: &mut RuntimeContext<'_, '_, '_>,
    ) -> Result<*const T, OwnerError> {
        if source.is_empty() {
            return Ok(std::ptr::null());
        }
        let pointer = unsafe { self.pointer.as_ptr().add(offset).cast::<T>() };
        let chunk_elements = (65536 / std::mem::size_of::<T>()).max(1);
        let mut initialized = 0;
        for chunk in source.chunks(chunk_elements) {
            let bytes = std::mem::size_of_val(chunk) as u64;
            context.charge(WorkKind::CopiedBytes, bytes)?;
            // The checked units are consumed immediately, with no intervening
            // fallible operation. Coordinator owns CompletedAbiBytes once.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    chunk.as_ptr(),
                    pointer.add(initialized),
                    chunk.len(),
                );
            }
            initialized += chunk.len();
        }
        Ok(pointer)
    }

    // The checked ArenaLayout fixes the typed offset and total extent. Each
    // infallible writer consumes a charged chunk before the next fallible step.
    unsafe fn write_chunks<T>(
        &self,
        offset: usize,
        count: usize,
        context: &mut RuntimeContext<'_, '_, '_>,
        mut writer: impl FnMut(*mut T, std::ops::Range<usize>),
    ) -> Result<*const T, OwnerError> {
        if count == 0 {
            return Ok(std::ptr::null());
        }
        let element_size = std::mem::size_of::<T>();
        if element_size == 0 {
            return Err(OwnerError::Limit);
        }
        let bytes = element_size.checked_mul(count).ok_or(OwnerError::Limit)?;
        if offset
            .checked_add(bytes)
            .is_none_or(|end| end > self.layout.size())
            || unsafe { self.pointer.as_ptr().add(offset) }.align_offset(std::mem::align_of::<T>())
                != 0
        {
            return Err(OwnerError::Limit);
        }
        let pointer = unsafe { self.pointer.as_ptr().add(offset).cast::<T>() };
        let chunk_elements = (65536 / element_size).max(1);
        let mut initialized = 0;
        while initialized < count {
            let end = initialized.saturating_add(chunk_elements).min(count);
            let chunk_bytes = element_size
                .checked_mul(end - initialized)
                .ok_or(OwnerError::Limit)?;
            context.charge(WorkKind::CopiedBytes, chunk_bytes as u64)?;
            writer(pointer, initialized..end);
            initialized = end;
        }
        Ok(pointer)
    }

    // The mapper is infallible and allocation-free; source length is the exact
    // initialized range passed to the shared checked writer.
    unsafe fn map<T, U>(
        &self,
        offset: usize,
        source: &[U],
        context: &mut RuntimeContext<'_, '_, '_>,
        mapper: impl Fn(usize, &U) -> T,
    ) -> Result<*const T, OwnerError> {
        unsafe {
            self.write_chunks(offset, source.len(), context, |pointer: *mut T, range| {
                for index in range {
                    pointer
                        .add(index)
                        .write(mapper(index, source.get_unchecked(index)));
                }
            })
        }
    }

    // Same proof as map, for fixed metadata rows synthesized from checked
    // geometry rather than a source slice.
    unsafe fn generate<T>(
        &self,
        offset: usize,
        count: usize,
        context: &mut RuntimeContext<'_, '_, '_>,
        mapper: impl Fn(usize) -> T,
    ) -> Result<*const T, OwnerError> {
        unsafe {
            self.write_chunks(offset, count, context, |pointer: *mut T, range| {
                for index in range {
                    pointer.add(index).write(mapper(index));
                }
            })
        }
    }
}
impl Drop for AlignedArena {
    fn drop(&mut self) {
        if self.layout.size() != 0 {
            unsafe {
                dealloc(self.pointer.as_ptr(), self.layout);
            }
        }
    }
}

fn fill(
    arena: &AlignedArena,
    plan: &ArenaLayout,
    parts: ResponseParts<'_>,
    metadata: ResponseMetadata,
    context: &mut RuntimeContext<'_, '_, '_>,
) -> Result<ZeGraphResponse, OwnerError> {
    let mut root = empty_response();
    root.row_count = metadata.row_count;
    root.has_admitted_generation = u32::from(metadata.admitted_generation.is_some());
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
        off_diagnostics,
        off_work,
    ] = plan.offsets;
    root.pool.values = unsafe { arena.copy(off_values, parts.values, context)? };
    root.pool.value_count = parts.values.len();
    root.pool.children = unsafe { arena.copy(off_children, parts.children, context)? };
    root.pool.child_count = parts.children.len();
    root.pool.bytes = unsafe { arena.copy(off_bytes, parts.bytes, context)? };
    root.pool.byte_count = parts.bytes.len();
    root.pool.nodes = unsafe { arena.copy(off_nodes, parts.nodes, context)? };
    root.pool.node_count = parts.nodes.len();
    root.pool.relationships =
        unsafe { arena.copy(off_relationships, parts.relationships, context)? };
    root.pool.relationship_count = parts.relationships.len();
    root.pool.properties = unsafe { arena.copy(off_properties, parts.properties, context)? };
    root.pool.property_count = parts.properties.len();
    root.pool.names = unsafe { arena.copy(off_names, parts.names, context)? };
    root.pool.name_count = parts.names.len();
    root.pool.vectors = unsafe { arena.copy(off_vectors, parts.vectors, context)? };
    root.pool.vector_count = parts.vectors.len();
    root.columns = unsafe { arena.copy(off_columns, parts.columns, context)? };
    root.column_count = parts.columns.len();
    root.cells = unsafe { arena.copy(off_cells, parts.cells, context)? };
    root.cell_count = parts.cells.len();
    root.receipts = unsafe { arena.copy(off_receipts, parts.receipts, context)? };
    root.receipt_count = parts.receipts.len();
    root.reports = unsafe { arena.copy(off_reports, parts.reports, context)? };
    root.report_count = parts.reports.len();
    root.diagnostics = unsafe { arena.copy(off_diagnostics, parts.diagnostics, context)? };
    root.diagnostic_count = parts.diagnostics.len();
    root.work = unsafe { arena.copy(off_work, parts.work, context)? };
    root.work_count = parts.work.len();
    Ok(root)
}

#[cfg(test)]
mod audit;
