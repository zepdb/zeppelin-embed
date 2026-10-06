//! Shared accounting adapter, not a store/view admission or allocator owner.
use crate::lifecycle::stats::{AccountedCounter, Accounting, AllocationComponent};
use crate::lifecycle::{Store, StoreError};
use std::sync::Arc;

pub use crate::lifecycle::stats::{
    AllocationInterval, AllocationIntervalError, AllocationIntervalSnapshot,
};

/// Hard aggregate graph ceiling; all participants share the store's allowance.
pub const MAX_GRAPH_RESIDENT_BYTES: u64 = 256 * 1024 * 1024;

/// A non-allocating clone of the existing store's shared accounting authority.
/// Construction does not admit a view or permit work after store close.
#[derive(Clone)]
pub struct GraphResources {
    accounting: Arc<Accounting>,
}
impl GraphResources {
    /// Reuses a store configured with an aggregate limit at most 256 MiB.
    /// No second budget or independent accounting instance is created. The
    /// retained-view owner separately enforces admission and close cancellation.
    pub fn from_store(store: &Store) -> Result<Self, StoreError> {
        let limit = store.accounting.resident_limit();
        if limit > MAX_GRAPH_RESIDENT_BYTES {
            return Err(StoreError::BudgetExceeded {
                needed: limit,
                budget: MAX_GRAPH_RESIDENT_BYTES,
                component: "graph aggregate configuration",
            });
        }
        Ok(Self {
            accounting: Arc::clone(&store.accounting),
        })
    }
    /// Current engine capacities; application-retention reporting is reserved for ZE-310.
    pub fn snapshot(&self) -> Result<GraphResourceSnapshot, StoreError> {
        let (engine_bytes, engine_peak_bytes) = self.accounting.graph_resource_bytes()?;
        Ok(GraphResourceSnapshot {
            engine_bytes,
            engine_peak_bytes,
            application_bytes: 0,
            application_peak_bytes: 0,
        })
    }
    /// Nonshipping snapshot of internal store-cumulative work for exact tests.
    #[cfg(any(test, feature = "test-seams"))]
    pub fn work_ledger(&self) -> Result<crate::lifecycle::stats::GraphWorkLedger, StoreError> {
        Ok(self.accounting.graph_work())
    }
    pub(crate) fn record_work(&self, kind: crate::lifecycle::stats::GraphWorkKind, units: u64) {
        work_batch::record(&self.accounting, kind, units);
    }
    pub(crate) fn record_work_delta(&self, delta: crate::lifecycle::stats::GraphWorkLedger) {
        work_batch::record_delta(&self.accounting, delta);
    }
    pub(crate) fn begin_work(&self) -> work_batch::WorkBatch {
        work_batch::WorkBatch::new(&self.accounting)
    }
    /// Current exact shared reservations, including all other store participants.
    pub fn reserved_bytes(&self) -> Result<u64, StoreError> {
        Ok(self.accounting.audit()?.resident_owned_bytes)
    }
    /// Monotone resident reservation peak since this store Accounting was created.
    /// This includes short-lived overlap and is not periodic process-RSS sampling.
    pub fn peak_reserved_bytes(&self) -> Result<u64, StoreError> {
        self.accounting.resident_peak_bytes()
    }
    /// Begins one exact shared-reservation observation on this store owner.
    /// Existing live reservations are included. The guard owns no Store or
    /// graph admission and a second observation returns a typed busy error.
    pub fn begin_allocation_interval(&self) -> Result<AllocationInterval, AllocationIntervalError> {
        self.accounting.begin_allocation_interval()
    }
    /// Reserves anonymous participant capacity before allocating or adopting it.
    /// The participant owns the backing and must reconcile actual capacity;
    /// reservation success alone proves no allocation or view ownership.
    pub fn reserve(&self, bytes: usize) -> Result<GraphReservation, StoreError> {
        let mut charge = AccountedCounter::new(&self.accounting, AllocationComponent::Temporary)?;
        charge.set(bytes)?;
        Ok(GraphReservation { charge })
    }
}

/// Owned shared capacity reservation. Drop releases precisely the current charge.
pub struct GraphReservation {
    charge: AccountedCounter,
}
impl GraphReservation {
    pub(crate) fn belongs_to(&self, resources: &GraphResources) -> bool {
        self.charge.belongs_to(&resources.accounting)
    }
    /// Changes the charge atomically against the shared budgets; failed growth
    /// preserves its prior value. Backing must be freed before shrinking.
    pub fn resize(&mut self, bytes: usize) -> Result<(), StoreError> {
        self.charge.set(bytes)
    }
    /// Exact currently reserved capacity, not a count of initialized elements.
    pub const fn bytes(&self) -> u64 {
        self.charge.bytes()
    }
}

/// Allocation observations exclude mappings, process footprint and caller/model buffers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphResourceSnapshot {
    /// Current engine-owned working capacities and controls.
    pub engine_bytes: u64,
    /// Lifetime engine capacity high-water, including replacement overlap.
    pub engine_peak_bytes: u64,
    /// Reserved for ZE-310; currently zero.
    pub application_bytes: u64,
    /// Reserved for ZE-310; currently zero.
    pub application_peak_bytes: u64,
}

pub(crate) mod work_batch;
