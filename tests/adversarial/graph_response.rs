//! PG16 actual internal C-owner operations. Synthetic typed pools exercise the
//! compiled component, not authentic native conversion/admission or a C export.
use super::coverage::CoverageRegistry;
use rand::RngCore;
use std::cell::Cell;
use std::num::NonZeroU64;
use std::sync::{Arc, Barrier};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, SnapshotLease, Store};
use zeppelin_embed::property_graph::query::resources::{MemoryError, QueryMemory};
use zeppelin_embed::property_graph::query::runtime::{
    RetainedView, RuntimeContext, RuntimeError, RuntimeLimits, WorkKind,
};
use zeppelin_embed::property_graph::query::{QueryError, QueryView};
use zeppelin_embed::property_graph::resources::GraphResources;
use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};
use zeppelin_embed_adversarial_oracle::graph_response::{
    self as oracle, Fault, Observation, Refusal,
};
use zeppelin_embed_ffi::graph_result::{test_support::AllocationFaultScope, *};
use zeppelin_embed_ffi::*;

pub const REQUIRED_COVERAGE: &[&str] = &[
    "property-graph.response.aligned-owner",
    "property-graph.response.private-forged-stale",
    "property-graph.response.abort-cleanup",
    "property-graph.response.concurrent-single-owner",
    "property-graph.response.known-outcome",
    "property-graph.response.allocation.fire",
    "property-graph.response.cancel.fire",
    "property-graph.response.memory.fire",
    "property-graph.response.work.fire",
    "property-graph.response.registry.fire",
    "property-graph.response.same-seed-control",
    "property-graph.response.oracle.can-fire",
];
#[derive(Debug, Default)]
pub struct ProbeReport {
    pub cases: usize,
    pub fault_fires: usize,
    pub clean_controls: usize,
}
struct View {
    token: QueryView,
    lease: SnapshotLease,
    cancellation: CancelToken,
    cancel_after: Cell<usize>,
    polls: Cell<usize>,
    fires: Cell<usize>,
}
impl RetainedView for View {
    fn query_view(&self) -> &QueryView {
        &self.token
    }
    fn check_active(&self) -> Result<(), QueryError> {
        self.lease
            .check_active()
            .map_err(|_| QueryError::ReadCancelled)?;
        self.polls.set(self.polls.get() + 1);
        if self.cancel_after.get() != 0 && self.polls.get() == self.cancel_after.get() {
            self.fires.set(self.fires.get() + 1);
            self.cancellation.cancel();
        }
        Ok(())
    }
}
fn context<T>(
    memory_limit: usize,
    work_limit: u64,
    cancel_after: usize,
    run: impl FnOnce(&mut RuntimeContext<'_, '_, '_>, &View) -> Result<T, String>,
) -> Result<T, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let store = Store::open(
        dir.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .map_err(|e| e.to_string())?;
    let resources = GraphResources::from_store(&store).map_err(|e| e.to_string())?;
    let memory = QueryMemory::new(&resources, memory_limit).map_err(|e| e.to_string())?;
    let token = CancelToken::new();
    let view = View {
        token: QueryView::new(
            StoreInstanceId::new(128).map_err(|e| e.to_string())?,
            GraphGeneration::new(0),
        ),
        lease: store.snapshot().map_err(|e| e.to_string())?,
        cancellation: token.clone(),
        cancel_after: Cell::new(0),
        polls: Cell::new(0),
        fires: Cell::new(0),
    };
    let control = QueryControl::Cancel(token);
    let limits = RuntimeLimits::default()
        .with_limit(WorkKind::CopiedBytes, work_limit)
        .map_err(|e| e.to_string())?;
    let mut runtime =
        RuntimeContext::new(&view, &control, &memory, limits).map_err(|e| e.to_string())?;
    view.polls.set(0);
    view.cancel_after.set(cancel_after);
    run(&mut runtime, &view)
}
fn refusal(result: &Result<PreparedResponse<'_, '_>, OwnerError>) -> Refusal {
    match result {
        Ok(_) => Refusal::Accepted,
        Err(OwnerError::Allocation) => Refusal::Allocation,
        Err(OwnerError::Runtime(RuntimeError::Value(QueryError::Cancelled))) => Refusal::Cancel,
        Err(OwnerError::Runtime(RuntimeError::Limit(WorkKind::CopiedBytes))) => Refusal::Work,
        Err(OwnerError::Memory(MemoryError::Limit)) => Refusal::Memory,
        Err(OwnerError::RegistryFull) => Refusal::Registry,
        _ => Refusal::Other,
    }
}
fn clean(
    context: &mut RuntimeContext<'_, '_, '_>,
    parts: ResponseParts<'_>,
) -> Result<bool, String> {
    static REGISTRY: GraphResultRegistry = GraphResultRegistry::new(4);
    let base = context.memory().reserved_bytes();
    let scope = AllocationFaultScope::arm(0);
    let prepared = REGISTRY
        .prepare(context, parts, ResponseMetadata::new(0, Some(0)))
        .map_err(|e| e.to_string())?;
    let receipt = scope.receipt();
    drop(scope);
    let mut root = prepared.expose(SuccessfulOutcome::Read);
    let equal =
        unsafe { std::slice::from_raw_parts(root.pool.bytes, root.pool.byte_count) } == parts.bytes;
    REGISTRY.free(&mut root).map_err(|e| e.to_string())?;
    Ok(equal
        && receipt.matching_sites == 2
        && receipt.fires == 0
        && context.memory().reserved_bytes() == base)
}
struct SentRoot(ZeGraphResponse);
// Only copied descriptor values cross threads, for registry validation. No
// competing thread dereferences payload while another may free it.
unsafe impl Send for SentRoot {}
impl SentRoot {
    fn release(mut self, registry: &GraphResultRegistry) -> Result<FreeReport, OwnerError> {
        registry.free(&mut self.0)
    }
}

pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<ProbeReport, String> {
    let mut rng = super::test_support::seeded_rng("property_graph::response_probe", seed);
    let integer = (rng.next_u64() as i64) | (1_i64 << 54);
    let payload = format!("PG16\0λ-{:016x}", rng.next_u64()).into_bytes();
    let mut report = ProbeReport::default();
    static OWNER: GraphResultRegistry = GraphResultRegistry::new(32);
    context(1024 * 1024, u64::MAX, 0, |context, _| {
        let base = context.memory().reserved_bytes();
        // Known C scalar-only records: every zero bit pattern is valid Rust.
        let mut value: ZeGraphValue = unsafe { std::mem::zeroed() };
        value.abi_size = std::mem::size_of::<ZeGraphValue>() as u32;
        value.tag = ZeGraphValueTag::ZeGraphValueI64 as u32;
        value.integer = integer;
        let mut column: ZeGraphColumn = unsafe { std::mem::zeroed() };
        column.abi_size = std::mem::size_of::<ZeGraphColumn>() as u32;
        column.kinds = 4; // fixed C I64 kind bit, independent of Rust tag value
        let parts = ResponseParts {
            values: &[value],
            bytes: &payload,
            columns: &[column],
            cells: &[0],
            ..ResponseParts::default()
        };
        let prepared = OWNER
            .prepare(context, parts, ResponseMetadata::new(1, Some(0)))
            .map_err(|e| e.to_string())?;
        let mut private = prepared.descriptor();
        let private_rejected = matches!(OWNER.free(&mut private), Err(OwnerError::InvalidOwner));
        let mut root = prepared.expose(SuccessfulOutcome::Read);
        let mut forged = root;
        forged.pool.byte_count += 1;
        let forged_rejected = matches!(OWNER.free(&mut forged), Err(OwnerError::InvalidOwner));
        let mut observed = Observation {
            integer: unsafe { (*root.pool.values).integer },
            bytes: unsafe { std::slice::from_raw_parts(root.pool.bytes, root.pool.byte_count) }
                .to_vec(),
            rows: root.row_count,
            columns: root.column_count,
            cells: root.cell_count,
            disposition: root.disposition,
            admitted: (root.has_admitted_generation == 1).then_some(root.admitted_generation),
            changed: (root.has_changed_generation == 1).then_some(root.changed_generation),
            aligned: (root.pool.values as usize).is_multiple_of(8)
                && (root.columns as usize).is_multiple_of(4)
                && (root.cells as usize).is_multiple_of(4),
            private_rejected,
            forged_rejected,
            stale_rejected: false,
            empty_after_free: false,
            second_free_succeeded: false,
            query_charge_released: context.memory().reserved_bytes() == base,
        };
        let mut stale = root;
        OWNER.free(&mut root).map_err(|e| e.to_string())?;
        observed.empty_after_free =
            root.owner_token == 0 && root.pool.bytes.is_null() && root.row_count == 0;
        observed.second_free_succeeded = OWNER.free(&mut root).is_ok();
        observed.stale_rejected = matches!(OWNER.free(&mut stale), Err(OwnerError::InvalidOwner));
        oracle::check_owner(integer, &payload, &observed)?;
        let mut wrong = observed.clone();
        wrong.query_charge_released = false;
        if oracle::check_owner(integer, &payload, &wrong).is_ok()
            || oracle::check_race(2, 0, true).is_ok()
            || oracle::check_fault(Fault::Allocation(1), Refusal::Allocation, 1, 0, true, true)
                .is_ok()
        {
            return Err("PG16 oracle negative control did not fire".into());
        }
        let prepared = OWNER
            .prepare(context, parts, ResponseMetadata::new(1, Some(0)))
            .map_err(|e| e.to_string())?;
        let mut aborted = prepared.descriptor();
        drop(prepared);
        if context.memory().reserved_bytes() != base
            || !matches!(OWNER.free(&mut aborted), Err(OwnerError::InvalidOwner))
        {
            return Err("PG16 abort leaked ownership/charge".into());
        }
        Ok(())
    })?;
    for key in [
        REQUIRED_COVERAGE[0],
        REQUIRED_COVERAGE[1],
        REQUIRED_COVERAGE[2],
        REQUIRED_COVERAGE[11],
    ] {
        coverage.hit(key);
    }
    report.cases += 2;

    // Complete clean operations with identical source bytes follow every fault.
    for fault in [
        Fault::Allocation(1),
        Fault::Allocation(2),
        Fault::Cancel,
        Fault::Memory,
        Fault::Work,
        Fault::Registry,
    ] {
        let memory_limit = if fault == Fault::Memory {
            1024
        } else {
            1024 * 1024
        };
        let work_limit = if fault == Fault::Work { 0 } else { u64::MAX };
        let cancel_after = if fault == Fault::Cancel { 2 } else { 0 };
        let bytes = if fault == Fault::Memory {
            vec![7_u8; 8192]
        } else {
            payload.clone()
        };
        let (status, matching, fires, restored) =
            context(memory_limit, work_limit, cancel_after, |context, view| {
                static REGISTRY: GraphResultRegistry = GraphResultRegistry::new(1);
                let base = context.memory().reserved_bytes();
                let blocker = if fault == Fault::Registry {
                    Some(
                        REGISTRY
                            .prepare(
                                context,
                                ResponseParts::default(),
                                ResponseMetadata::new(0, Some(0)),
                            )
                            .map_err(|e| e.to_string())?,
                    )
                } else {
                    None
                };
                let scope = AllocationFaultScope::arm(match fault {
                    Fault::Allocation(n) => n,
                    _ => 0,
                });
                let result = REGISTRY.prepare(
                    context,
                    ResponseParts {
                        bytes: &bytes,
                        ..ResponseParts::default()
                    },
                    ResponseMetadata::new(0, Some(0)),
                );
                let status = refusal(&result);
                drop(result);
                let receipt = scope.receipt();
                drop(scope);
                drop(blocker);
                let (matching, fires) = match fault {
                    Fault::Allocation(_) => (receipt.matching_sites, receipt.fires),
                    Fault::Cancel => (view.fires.get(), view.fires.get()),
                    _ => (
                        usize::from(status != Refusal::Accepted),
                        usize::from(status != Refusal::Accepted),
                    ),
                };
                Ok((
                    status,
                    matching,
                    fires,
                    context.memory().reserved_bytes() == base,
                ))
            })?;
        let clean_succeeded = context(1024 * 1024, u64::MAX, 0, |context, _| {
            clean(
                context,
                ResponseParts {
                    bytes: &bytes,
                    ..ResponseParts::default()
                },
            )
        })?;
        oracle::check_fault(fault, status, matching, fires, restored, clean_succeeded)?;
        coverage.hit(match fault {
            Fault::Allocation(_) => REQUIRED_COVERAGE[5],
            Fault::Cancel => REQUIRED_COVERAGE[6],
            Fault::Memory => REQUIRED_COVERAGE[7],
            Fault::Work => REQUIRED_COVERAGE[8],
            Fault::Registry => REQUIRED_COVERAGE[9],
        });
        coverage.hit(REQUIRED_COVERAGE[10]);
        report.fault_fires += fires;
        report.clean_controls += 1;
        report.cases += 2;
    }

    context(1024 * 1024, u64::MAX, 0, |context, _| {
        let prepared = OWNER
            .prepare(
                context,
                ResponseParts {
                    bytes: &payload,
                    ..ResponseParts::default()
                },
                ResponseMetadata::new(0, Some(0)),
            )
            .map_err(|e| e.to_string())?;
        let private = SentRoot(prepared.descriptor());
        let barrier = Arc::new(Barrier::new(2));
        let other = Arc::clone(&barrier);
        let contender = std::thread::spawn(move || {
            other.wait();
            private.release(&OWNER)
        });
        barrier.wait();
        let root = prepared.expose(SuccessfulOutcome::Read);
        let first = contender
            .join()
            .map_err(|_| "PG16 owner contender panicked")?;
        let second = SentRoot(root).release(&OWNER);
        let winners = usize::from(first.is_ok()) + usize::from(second.is_ok());
        let losers = usize::from(matches!(
            first,
            Err(OwnerError::Busy | OwnerError::InvalidOwner)
        )) + usize::from(matches!(
            second,
            Err(OwnerError::Busy | OwnerError::InvalidOwner)
        ));
        let mut stale = root;
        let stale_rejected = matches!(OWNER.free(&mut stale), Err(OwnerError::InvalidOwner));
        oracle::check_race(winners, losers, stale_rejected)?;
        // Two actual free contenders start together against one published owner.
        let root = OWNER
            .prepare(
                context,
                ResponseParts {
                    bytes: &payload,
                    ..ResponseParts::default()
                },
                ResponseMetadata::new(0, Some(0)),
            )
            .map_err(|e| e.to_string())?
            .expose(SuccessfulOutcome::Read);
        let first = SentRoot(root);
        let second = SentRoot(root);
        let barrier = Arc::new(Barrier::new(3));
        let one_barrier = Arc::clone(&barrier);
        let two_barrier = Arc::clone(&barrier);
        let one = std::thread::spawn(move || {
            one_barrier.wait();
            first.release(&OWNER)
        });
        let two = std::thread::spawn(move || {
            two_barrier.wait();
            second.release(&OWNER)
        });
        barrier.wait();
        let results = [
            one.join().map_err(|_| "PG16 first free panicked")?,
            two.join().map_err(|_| "PG16 second free panicked")?,
        ];
        let winners = results.iter().filter(|result| result.is_ok()).count();
        let losers = results
            .iter()
            .filter(|result| matches!(result, Err(OwnerError::Busy | OwnerError::InvalidOwner)))
            .count();
        let mut stale = root;
        oracle::check_race(
            winners,
            losers,
            matches!(OWNER.free(&mut stale), Err(OwnerError::InvalidOwner)),
        )
    })?;
    coverage.hit(REQUIRED_COVERAGE[3]);
    report.cases += 1;

    let state = OutcomeCell::write();
    let read = match OutcomeCell::read().get() {
        OperationOutcome::Success(SuccessfulOutcome::Read) => 0,
        _ => 255,
    };
    state
        .begin_attempt()
        .map_err(|_| "PG16 failed to record attempted write")?;
    let attempted = match state.get() {
        OperationOutcome::Indeterminate => 5,
        _ => 255,
    };
    state
        .record_success(SuccessfulOutcome::Committed(
            NonZeroU64::new(73).ok_or("zero generation")?,
        ))
        .map_err(|_| "PG16 failed to retain known outcome")?;
    // A fixed integer unwind payload avoids formatting or a fake C entrypoint.
    let unwind = std::panic::catch_unwind(|| std::panic::resume_unwind(Box::new(128_u32)));
    let (committed, generation) = match state.get() {
        OperationOutcome::Success(SuccessfulOutcome::Committed(value)) => (2, Some(value.get())),
        _ => (255, None),
    };
    oracle::check_outcome(
        read,
        attempted,
        committed,
        generation,
        state.record_not_committed().is_err(),
        unwind.is_err(),
    )?;
    // Scope reset on unwind: the still-armed ordinal must not leak to next work.
    let _ = std::panic::catch_unwind(|| {
        let _scope = AllocationFaultScope::arm(1);
        std::panic::resume_unwind(Box::new(128_u32));
    });
    context(1024 * 1024, u64::MAX, 0, |context, _| {
        let prepared = OWNER
            .prepare(
                context,
                ResponseParts {
                    bytes: &payload,
                    ..ResponseParts::default()
                },
                ResponseMetadata::new(0, Some(0)),
            )
            .map_err(|e| e.to_string())?;
        drop(prepared);
        Ok(())
    })?;
    coverage.hit(REQUIRED_COVERAGE[4]);
    report.cases += 1;
    Ok(report)
}
