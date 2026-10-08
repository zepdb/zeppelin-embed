use super::*;
use memory::{Arena, WriteReservation};

/// Exact capacities needed for precommit result copies and registration.
#[derive(Clone, Copy, Debug)]
pub struct ResultLayout {
    /// Complete output row count, including rows without mutations.
    pub rows: usize,
    /// Complete core result arena capacity; receipts remain charged metadata.
    pub core_bytes: usize,
    /// Complete binding-owned result representation.
    pub abi_bytes: usize,
    /// Actual separately owned registry/descriptor allocation capacity.
    pub registry_bytes: usize,
}
/// A concrete retained binding registration owner, released on abort/exposure.
/// Capacity reports actual backing, not a logical entry count or estimate.
pub trait ResultRegistration {
    /// Exact anonymous capacity owned by this token.
    fn capacity_bytes(&self) -> usize;
}
/// Mandatory capability for callers that need binding copies/registration.
/// A Rust-only staging call has no ABI claim. Implementations write the complete
/// representations and acquire their real registration token before returning.
pub trait ResultMaterializer {
    /// Concrete registry reservation lifetime/ownership; no permissive default.
    type Registration: ResultRegistration;
    /// Measures complete capacities from the actual pending receipt count,
    /// before any generation or fresh ID exists. Any ID representation has its
    /// fixed domain width; implementations must not need fabricated receipts.
    fn layout(
        &mut self,
        receipt_count: usize,
        control: &mut WriteControl<'_>,
    ) -> Result<ResultLayout, StageError>;
    /// Fills preallocated buffers and obtains a charged registration token. Any
    /// failure must undo private registration; this never runs after commit.
    fn materialize(
        &mut self,
        receipts: &[ItemReceipt],
        core: &mut [u8],
        abi: &mut [u8],
        control: &mut WriteControl<'_>,
    ) -> Result<Self::Registration, StageError>;
}
/// Fully prepared optional output. Dropping it releases registry/backing before
/// the corresponding shared reservation; no postcommit materialization remains.
pub struct MaterializedBatch<'a, R> {
    batch: StagedBatch<'a>,
    registration: R,
    core: Arena<'a, u8>,
    abi: Arena<'a, u8>,
    _registration_charge: WriteReservation<'a>,
}
impl<'a, R> MaterializedBatch<'a, R> {
    /// A changed document participant commits the assigned generation even
    /// when every graph item is an exact replay. Graph receipts stay exact.
    pub(crate) fn include_document_change(&mut self) {
        self.batch.disposition = crate::property_graph::BatchDisposition::Changed;
    }

    pub(crate) fn include_document_versions(
        &mut self,
        base: &'a dyn AdmittedBase,
        versions: &[crate::ingest::DocumentVersion],
        memory: &'a WriteMemory<'a>,
        control: &mut WriteControl<'_>,
    ) -> Result<(), StageError> {
        self.batch
            .bind_updated_documents(base, versions, memory, control)
    }

    /// Private graph/search delta awaiting coordinator publication.
    pub const fn batch(&self) -> &StagedBatch<'_> {
        &self.batch
    }
    /// Completely materialized core representation.
    pub fn core_bytes(&self) -> &[u8] {
        &self.core
    }
    /// Completely materialized binding representation.
    pub fn abi_bytes(&self) -> &[u8] {
        &self.abi
    }
    /// Retained real registry token; its semantics belong to the binding adapter.
    pub const fn registration(&self) -> &R {
        &self.registration
    }

    /// Moves already materialized backing and its actual shared charges.
    /// This transfer performs no allocation, callback, or fallible work.
    #[allow(clippy::type_complexity)]
    pub(crate) fn into_prepared_parts(
        self,
    ) -> (
        StagedBatch<'a>,
        R,
        Vec<u8>,
        Vec<u8>,
        [super::super::resources::GraphReservation; 3],
    ) {
        let (core, core_charge) = self.core.into_parts();
        let (abi, abi_charge) = self.abi.into_parts();
        let (core_charge, _core_local) = core_charge.split();
        let (abi_charge, _abi_local) = abi_charge.split();
        let (registry_charge, _registry_local) = self._registration_charge.split();
        (
            self.batch,
            self.registration,
            core,
            abi,
            [core_charge, abi_charge, registry_charge],
        )
    }
}
/// Prepares both arenas and registration before returning any private receipt.
/// A definite failure returns neither the batch nor any newly allocated ID.
pub fn stage_structured_with_results<'a, M: ResultMaterializer>(
    base: &'a dyn AdmittedBase,
    requests: &[StructuredWrite<'a, '_>],
    memory: &'a WriteMemory<'a>,
    materializer: &mut M,
    control: &mut WriteControl<'_>,
) -> Result<MaterializedBatch<'a, M::Registration>, StageError> {
    stage_structured_with_results_for_target(
        base,
        base.identity()
            .generation
            .get()
            .checked_add(1)
            .map(GraphGeneration::new),
        requests,
        memory,
        materializer,
        control,
    )
}
/// Materializes a batch at the exact generation assigned by its committer.
pub fn stage_structured_with_results_at_generation<'a, M: ResultMaterializer>(
    base: &'a dyn AdmittedBase,
    target_generation: GraphGeneration,
    requests: &[StructuredWrite<'a, '_>],
    memory: &'a WriteMemory<'a>,
    materializer: &mut M,
    control: &mut WriteControl<'_>,
) -> Result<MaterializedBatch<'a, M::Registration>, StageError> {
    stage_structured_with_results_for_target(
        base,
        Some(target_generation),
        requests,
        memory,
        materializer,
        control,
    )
}
fn stage_structured_with_results_for_target<'a, M: ResultMaterializer>(
    base: &'a dyn AdmittedBase,
    target_generation: Option<GraphGeneration>,
    requests: &[StructuredWrite<'a, '_>],
    memory: &'a WriteMemory<'a>,
    materializer: &mut M,
    control: &mut WriteControl<'_>,
) -> Result<MaterializedBatch<'a, M::Registration>, StageError> {
    let mut layout = None;
    let batch = structured::stage_structured_with_preflight(
        base,
        target_generation,
        requests,
        memory,
        control,
        &mut |count, control| {
            layout = Some(admit_layout(count, memory, materializer, control)?);
            Ok(())
        },
    )?;
    materialize_batch(
        batch,
        base,
        memory,
        layout.ok_or(StageError::InvalidInput)?,
        materializer,
        control,
    )
}
pub(super) type ResultPreflight<'a> =
    dyn FnMut(usize, &mut WriteControl<'_>) -> Result<(), StageError> + 'a;
pub(super) fn admit_layout<M: ResultMaterializer>(
    count: usize,
    memory: &WriteMemory<'_>,
    materializer: &mut M,
    control: &mut WriteControl<'_>,
) -> Result<ResultLayout, StageError> {
    let layout = materializer.layout(count, control)?;
    if layout.rows > memory.limits.result_rows
        || layout.core_bytes > memory.limits.result_bytes
        || layout.abi_bytes > memory.limits.result_bytes
    {
        return Err(StageError::Limit);
    }
    let simultaneous = layout
        .core_bytes
        .checked_add(layout.abi_bytes)
        .and_then(|n| n.checked_add(layout.registry_bytes))
        .and_then(|n| n.checked_add(memory.reserved_bytes()))
        .ok_or(StageError::Limit)?;
    if simultaneous > memory.limits.writer_bytes {
        return Err(StageError::Limit);
    }
    Ok(layout)
}
pub(super) fn materialize_batch<'a, M: ResultMaterializer>(
    batch: StagedBatch<'a>,
    base: &dyn AdmittedBase,
    memory: &'a WriteMemory<'a>,
    layout: ResultLayout,
    materializer: &mut M,
    control: &mut WriteControl<'_>,
) -> Result<MaterializedBatch<'a, M::Registration>, StageError> {
    let mut core = Arena::new(memory, layout.core_bytes, control)?;
    core.zeroed(control, WritePhase::CoreResult)?;
    let mut abi = Arena::new(memory, layout.abi_bytes, control)?;
    abi.zeroed(control, WritePhase::AbiResult)?;
    if core.allocated_bytes() > memory.limits.result_bytes
        || abi.allocated_bytes() > memory.limits.result_bytes
    {
        return Err(StageError::Limit);
    }
    let registration_charge = memory.reserve(layout.registry_bytes, control)?;
    let registration = materializer.materialize(
        batch.receipts(),
        core.as_mut_slice(),
        abi.as_mut_slice(),
        control,
    )?;
    if registration.capacity_bytes() != layout.registry_bytes {
        return Err(StageError::InvalidInput);
    }
    control(WritePhase::Finalize)?;
    if base.identity() != batch.base() {
        return Err(StageError::ViewMismatch);
    }
    Ok(MaterializedBatch {
        batch,
        core,
        abi,
        registration,
        _registration_charge: registration_charge,
    })
}

/// Result backing jointly retained by writer accounting and a consuming query
/// owner. The adapter owns the original shared reservations, never byte proofs.
pub struct ScopedMaterializedBatch<'a, R, Q> {
    batch: StagedBatch<'a>,
    registration: R,
    core: Vec<u8>,
    abi: Vec<u8>,
    owners: [Option<ResultOwner<'a, Q>>; 3],
}
enum ResultOwner<'a, Q> {
    Original(WriteReservation<'a>),
    Adopted {
        _writer: memory::WriterCapacity<'a>,
        _owner: Q,
    },
}
impl<R, Q> ScopedMaterializedBatch<'_, R, Q> {
    /// Fully prepared graph/search mutation participant.
    pub const fn batch(&self) -> &StagedBatch<'_> {
        &self.batch
    }
    /// Complete immutable core result bytes.
    pub fn core_bytes(&self) -> &[u8] {
        &self.core
    }
    /// Complete immutable ABI result bytes.
    pub fn abi_bytes(&self) -> &[u8] {
        &self.abi
    }
    /// Retained binding registration, released before its capacity ownership.
    pub const fn registration(&self) -> &R {
        &self.registration
    }
}
impl<'a, R> MaterializedBatch<'a, R> {
    /// Moves each sole shared charge into a query's consuming reservation owner
    /// while retaining its writer-local guard and immutable result backing.
    /// The adapter must return the original charge unchanged on failure. All
    /// callbacks and final view/control checks run before coordinator handoff.
    pub fn adopt_result_memory<Q>(
        self,
        base: &dyn AdmittedBase,
        adopt: &mut impl FnMut(
            super::super::resources::GraphReservation,
        )
            -> Result<Q, (StageError, super::super::resources::GraphReservation)>,
        control: &mut WriteControl<'_>,
    ) -> Result<ScopedMaterializedBatch<'a, R, Q>, StageError> {
        let (core, core_charge) = self.core.into_parts();
        let (abi, abi_charge) = self.abi.into_parts();
        let mut prepared = ScopedMaterializedBatch {
            batch: self.batch,
            core,
            abi,
            registration: self.registration,
            owners: [
                Some(ResultOwner::Original(core_charge)),
                Some(ResultOwner::Original(abi_charge)),
                Some(ResultOwner::Original(self._registration_charge)),
            ],
        };
        for slot in &mut prepared.owners {
            control(WritePhase::AbiResult)?;
            let Some(ResultOwner::Original(reservation)) = slot.take() else {
                return Err(StageError::InvalidInput);
            };
            let (shared, writer) = reservation.split();
            match adopt(shared) {
                Ok(owner) => {
                    *slot = Some(ResultOwner::Adopted {
                        _writer: writer,
                        _owner: owner,
                    })
                }
                Err((error, shared)) => {
                    *slot = Some(ResultOwner::Original(WriteReservation::from_parts(
                        shared, writer,
                    )));
                    return Err(error);
                }
            }
        }
        control(WritePhase::Finalize)?;
        if base.identity() != prepared.batch.base() {
            return Err(StageError::ViewMismatch);
        }
        Ok(prepared)
    }
}
