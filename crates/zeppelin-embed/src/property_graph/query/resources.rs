//! Actual query allocation owners and nested capacity reservations.
use super::MAX_QUERY_BYTES;
use crate::lifecycle::StoreError;
use crate::property_graph::resources::{GraphReservation, GraphResources};
use std::cell::Cell;
mod inputs;
pub use inputs::{QueryInputs, RetainedAllocation, RetentionInventory, RuntimePlan};

/// Failure before a query allocation or ownership transfer can succeed.
#[derive(Debug)]
pub enum MemoryError {
    /// Query cap, element arithmetic or fixed capacity was exceeded.
    Limit,
    /// System allocation failed after its capacity was reserved.
    Allocation,
    /// The shared lifecycle accounting rejected a reservation.
    Store(StoreError),
    /// A required span lacks an owner, or belongs to another query allowance.
    UnprovedInput,
    /// Cancellation or work exhaustion during bounded registration.
    Value(super::QueryError),
    /// The typed plan cannot be proved against actual retained allocations.
    Plan(super::plan::PlanError),
}
impl From<super::QueryError> for MemoryError {
    fn from(error: super::QueryError) -> Self {
        Self::Value(error)
    }
}
impl From<StoreError> for MemoryError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}
impl std::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Limit => f.write_str("graph query memory limit"),
            Self::Allocation => f.write_str("graph query allocation failed"),
            Self::Store(e) => e.fmt(f),
            Self::UnprovedInput => f.write_str("unproved query input ownership"),
            Self::Value(e) => e.fmt(f),
            Self::Plan(e) => e.fmt(f),
        }
    }
}
impl std::error::Error for MemoryError {}

/// One caller-thread query sublimit, always inside the shared store allowance.
/// Its own stack descriptor is reserved; no independent allocator is created.
pub struct QueryMemory<'g> {
    shared: &'g GraphResources,
    limit: usize,
    used: Cell<usize>,
    peak: Cell<usize>,
    _control: GraphReservation,
}
impl<'g> QueryMemory<'g> {
    /// Creates a tightened query allowance. Does not admit a view or query.
    pub fn new(shared: &'g GraphResources, limit: usize) -> Result<Self, MemoryError> {
        let bytes = std::mem::size_of::<Self>();
        if limit > MAX_QUERY_BYTES || bytes > limit {
            return Err(MemoryError::Limit);
        }
        let control = shared.reserve(bytes)?;
        Ok(Self {
            shared,
            limit,
            used: Cell::new(bytes),
            peak: Cell::new(bytes),
            _control: control,
        })
    }
    /// Consumes a participant's shared reservation into immutable joint ownership.
    /// Backing keeps one shared charge and gains the nested query charge. A
    /// failure returns the original reservation so no live backing loses its
    /// owner. The participant must keep this guard with its backing and free
    /// backing first. No resize/extraction permits an uncharged later growth.
    #[allow(
        clippy::result_large_err,
        reason = "allocation failure must return the original charge without allocating an error box"
    )]
    pub fn adopt_shared<'m>(
        &'m self,
        backing: GraphReservation,
    ) -> Result<QuerySharedReservation<'m, 'g>, (MemoryError, GraphReservation)> {
        let prepared = (|| {
            if !backing.belongs_to(self.shared) {
                return Err(MemoryError::UnprovedInput);
            }
            let bytes = usize::try_from(backing.bytes()).map_err(|_| MemoryError::Limit)?;
            let control = self.reserve(std::mem::size_of::<QuerySharedReservation<'_, '_>>())?;
            let total = self
                .used
                .get()
                .checked_add(bytes)
                .ok_or(MemoryError::Limit)?;
            if total > self.limit {
                return Err(MemoryError::Limit);
            }
            Ok((bytes, total, control))
        })();
        match prepared {
            Err(error) => Err((error, backing)),
            Ok((bytes, total, control)) => {
                self.used.set(total);
                self.peak.set(self.peak.get().max(total));
                Ok(QuerySharedReservation {
                    shared: backing,
                    memory: self,
                    bytes,
                    _control: control,
                })
            }
        }
    }
    /// Live heap capacities plus separately reserved control/scratch descriptors.
    pub fn reserved_bytes(&self) -> usize {
        self.used.get()
    }
    /// Monotone peak of real reservations, including overlapping replacements.
    pub fn peak_reserved_bytes(&self) -> usize {
        self.peak.get()
    }
    pub(crate) fn reserve(&self, bytes: usize) -> Result<QueryReservation<'_, 'g>, MemoryError> {
        let total = self
            .used
            .get()
            .checked_add(bytes)
            .ok_or(MemoryError::Limit)?;
        if total > self.limit {
            return Err(MemoryError::Limit);
        }
        let shared = self.shared.reserve(bytes)?;
        self.used.set(total);
        self.peak.set(self.peak.get().max(total));
        Ok(QueryReservation {
            memory: self,
            shared,
            bytes,
        })
    }
}

/// Immutable joint ownership of one aggregate reservation and query-local charge.
/// Move it with the actual buffer; free backing before this guard drops. It has
/// no resize or extraction path, and is not itself an allocation capability.
pub struct QuerySharedReservation<'m, 'g> {
    shared: GraphReservation,
    memory: &'m QueryMemory<'g>,
    bytes: usize,
    _control: QueryReservation<'m, 'g>,
}
impl QuerySharedReservation<'_, '_> {
    /// Exact retained shared backing reservation.
    pub fn bytes(&self) -> u64 {
        self.shared.bytes()
    }
}
impl Drop for QuerySharedReservation<'_, '_> {
    fn drop(&mut self) {
        self.memory
            .used
            .set(self.memory.used.get().saturating_sub(self.bytes));
    }
}

pub(crate) struct QueryReservation<'m, 'g> {
    memory: &'m QueryMemory<'g>,
    shared: GraphReservation,
    bytes: usize,
}
impl<'m, 'g> QueryReservation<'m, 'g> {
    pub(crate) const fn owner(&self) -> &'m QueryMemory<'g> {
        self.memory
    }
    fn resize(&mut self, bytes: usize) -> Result<(), MemoryError> {
        let remaining = self
            .memory
            .used
            .get()
            .checked_sub(self.bytes)
            .ok_or(MemoryError::Limit)?;
        let total = remaining.checked_add(bytes).ok_or(MemoryError::Limit)?;
        if total > self.memory.limit {
            return Err(MemoryError::Limit);
        }
        self.shared.resize(bytes)?;
        self.bytes = bytes;
        self.memory.used.set(total);
        self.memory.peak.set(self.memory.peak.get().max(total));
        Ok(())
    }
}
impl Drop for QueryReservation<'_, '_> {
    fn drop(&mut self) {
        self.memory
            .used
            .set(self.memory.used.get().saturating_sub(self.bytes));
    }
}

/// Fixed-capacity typed query storage; all backing is freed before its charge.
/// Values may themselves borrow backing, whose owner must remain separately charged.
pub struct QueryArena<'m, 'g, T> {
    values: Vec<T>,
    charge: QueryReservation<'m, 'g>,
}
impl<'m, 'g, T> QueryArena<'m, 'g, T> {
    /// Reserves capacity and this arena's control descriptor before allocation.
    pub fn new(memory: &'m QueryMemory<'g>, capacity: usize) -> Result<Self, MemoryError> {
        if std::mem::size_of::<T>() == 0 {
            return Err(MemoryError::Limit);
        }
        let heap = capacity
            .checked_mul(std::mem::size_of::<T>())
            .ok_or(MemoryError::Limit)?;
        let mut charge = memory.reserve(
            heap.checked_add(std::mem::size_of::<Self>())
                .ok_or(MemoryError::Limit)?,
        )?;
        let mut values = Vec::new();
        #[cfg(feature = "allocation-audit")]
        let allocation = crate::allocation_audit::attributed(|| values.try_reserve_exact(capacity));
        #[cfg(not(feature = "allocation-audit"))]
        let allocation = values.try_reserve_exact(capacity);
        allocation.map_err(|_| MemoryError::Allocation)?;
        let heap = values
            .capacity()
            .checked_mul(std::mem::size_of::<T>())
            .ok_or(MemoryError::Limit)?;
        charge.resize(
            heap.checked_add(std::mem::size_of::<Self>())
                .ok_or(MemoryError::Limit)?,
        )?;
        Ok(Self { values, charge })
    }
    /// Actual retained element capacity, including unused slots.
    pub fn capacity(&self) -> usize {
        self.values.capacity()
    }
    /// Exact Vec capacity bytes; excludes separately charged control descriptors.
    pub fn heap_bytes(&self) -> usize {
        self.values.capacity() * std::mem::size_of::<T>()
    }
    /// Actual capacity plus this arena's separately charged control descriptor.
    pub const fn reserved_bytes(&self) -> usize {
        self.charge.bytes
    }
    /// Initialized element count.
    pub fn len(&self) -> usize {
        self.values.len()
    }
    /// Whether no values are initialized.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
    /// Borrows initialized values only, never spare capacity.
    pub fn as_slice(&self) -> &[T] {
        &self.values
    }
    /// Borrows initialized values only, never spare capacity.
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.values
    }
    /// Adds one element without ever growing implicitly.
    pub fn push(&mut self, value: T) -> Result<(), MemoryError> {
        if self.values.len() == self.values.capacity() {
            return Err(MemoryError::Limit);
        }
        self.values.push(value);
        Ok(())
    }
    /// Clears initialized values, retaining and charging the full capacity.
    pub fn clear(&mut self) {
        self.values.clear();
    }
    pub(crate) fn extend_copy(&mut self, values: &[T]) -> Result<(), MemoryError>
    where
        T: Copy,
    {
        if self
            .values
            .len()
            .checked_add(values.len())
            .is_none_or(|n| n > self.values.capacity())
        {
            return Err(MemoryError::Limit);
        }
        self.values.extend_from_slice(values);
        Ok(())
    }
    pub(crate) fn truncate(&mut self, length: usize) {
        self.values.truncate(length);
    }
}
