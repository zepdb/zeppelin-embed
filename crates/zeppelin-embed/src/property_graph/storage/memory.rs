//! One storage participant allowance nested in the existing writer/store owner.
use super::tree::directory::TreeError;
use crate::lifecycle::QueryControl;
use crate::property_graph::staging::{WriteMemory, WriteReservation};
use std::cell::Cell;

/// Maximum simultaneous storage preparation, inside the writer and aggregate caps.
pub const MAX_STORAGE_PREPARE_BYTES: usize = 32 * 1024 * 1024;

/// Caller-owned control object for one complete storage preparation. Every
/// scratch/index/artifact/inventory owner borrows this same allowance. This is
/// capacity admission only: it does not acquire a graph view or publish roots.
pub struct StorageMemory<'a> {
    writer: &'a WriteMemory<'a>,
    control: &'a QueryControl,
    limit: usize,
    used: Cell<usize>,
    peak: Cell<usize>,
    _reservation: WriteReservation<'a>,
}
impl<'a> StorageMemory<'a> {
    /// Tightens the 32 MiB participant ceiling while retaining the real writer.
    pub fn new(
        writer: &'a WriteMemory<'a>,
        control: &'a QueryControl,
        limit: usize,
    ) -> Result<Self, TreeError> {
        control.checkpoint().map_err(TreeError::Control)?;
        let bytes = std::mem::size_of::<Self>();
        if limit > MAX_STORAGE_PREPARE_BYTES || bytes > limit {
            return Err(TreeError::Memory);
        }
        let reservation = writer
            .reserve(bytes, &mut |_| Ok(()))
            .map_err(|_| TreeError::Memory)?;
        Ok(Self {
            writer,
            control,
            limit,
            used: Cell::new(bytes),
            peak: Cell::new(bytes),
            _reservation: reservation,
        })
    }
    pub(super) fn require_batch(
        &self,
        batch: &crate::property_graph::staging::StagedBatch<'_>,
    ) -> Result<(), TreeError> {
        if !batch.uses_memory(self.writer) {
            return Err(TreeError::Invalid("staging/storage writer owner mismatch"));
        }
        Ok(())
    }
    /// Complete current participant reservations, including its control object.
    pub fn reserved_bytes(&self) -> usize {
        self.used.get()
    }
    /// Monotone peak including simultaneously retained replacement buffers.
    pub fn peak_reserved_bytes(&self) -> usize {
        self.peak.get()
    }
    pub(super) fn control(&self) -> &'a QueryControl {
        self.control
    }
    pub(super) const fn resources(&self) -> &'a crate::property_graph::resources::GraphResources {
        self.writer.resources()
    }
    pub(crate) fn reserve(&self, bytes: usize) -> Result<StorageReservation<'_>, TreeError> {
        self.control.checkpoint().map_err(TreeError::Control)?;
        let next = self
            .used
            .get()
            .checked_add(bytes)
            .ok_or(TreeError::Memory)?;
        if next > self.limit {
            return Err(TreeError::Memory);
        }
        let writer = self
            .writer
            .reserve(bytes, &mut |_| Ok(()))
            .map_err(|_| TreeError::Memory)?;
        self.used.set(next);
        self.peak.set(self.peak.get().max(next));
        Ok(StorageReservation {
            memory: self,
            writer,
            bytes,
        })
    }
}

/// Non-extractable participant guard; backing must precede it in drop order.
pub(crate) struct StorageReservation<'a> {
    memory: &'a StorageMemory<'a>,
    writer: WriteReservation<'a>,
    bytes: usize,
}
impl StorageReservation<'_> {
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }
    pub(crate) fn resize(&mut self, bytes: usize) -> Result<(), TreeError> {
        self.memory
            .control
            .checkpoint()
            .map_err(TreeError::Control)?;
        let next = self
            .memory
            .used
            .get()
            .checked_sub(self.bytes)
            .and_then(|n| n.checked_add(bytes))
            .ok_or(TreeError::Memory)?;
        if next > self.memory.limit {
            return Err(TreeError::Memory);
        }
        self.writer.resize(bytes).map_err(|_| TreeError::Memory)?;
        self.memory.used.set(next);
        self.memory.peak.set(self.memory.peak.get().max(next));
        self.bytes = bytes;
        Ok(())
    }
}
impl Drop for StorageReservation<'_> {
    fn drop(&mut self) {
        self.memory
            .used
            .set(self.memory.used.get().saturating_sub(self.bytes));
    }
}

/// Fixed-capacity, fallibly allocated participant backing. No implicit growth;
/// replacing a buffer requires a second complete reservation while both live.
/// The surrounding preparation workspace/container owns descriptor capacity.
pub struct StorageBuffer<'a, T> {
    values: Vec<T>,
    reservation: StorageReservation<'a>,
    limit: usize,
}
impl<'a, T> StorageBuffer<'a, T> {
    /// Reserves logical capacity before asking the allocator, then reconciles the
    /// allocator's complete actual backing capacity through the same owners.
    pub fn new(memory: &'a StorageMemory<'a>, capacity: usize) -> Result<Self, TreeError> {
        let bytes = capacity
            .checked_mul(std::mem::size_of::<T>())
            .ok_or(TreeError::Memory)?;
        let mut reservation = memory.reserve(bytes)?;
        let mut values = Vec::new();
        #[cfg(feature = "allocation-audit")]
        let allocation = crate::allocation_audit::attributed(|| values.try_reserve_exact(capacity));
        #[cfg(not(feature = "allocation-audit"))]
        let allocation = values.try_reserve_exact(capacity);
        allocation.map_err(|_| TreeError::Memory)?;
        let actual = values
            .capacity()
            .checked_mul(std::mem::size_of::<T>())
            .ok_or(TreeError::Memory)?;
        reservation.resize(actual)?;
        Ok(Self {
            values,
            reservation,
            limit: capacity,
        })
    }
    pub(crate) fn drain(&mut self) -> impl Iterator<Item = T> + '_ {
        self.values.drain(..)
    }
    /// Complete allocator backing capacity in bytes.
    pub fn owned_bytes(&self) -> usize {
        self.reservation.bytes()
    }
    /// Requested element capacity; allocation rounding does not widen admission.
    pub const fn capacity(&self) -> usize {
        self.limit
    }
    /// Borrow initialized contents without copying.
    pub fn as_slice(&self) -> &[T] {
        &self.values
    }
    /// Borrow initialized private contents without granting growth authority.
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.values
    }
    /// Add within the already admitted capacity only.
    pub fn push(&mut self, value: T) -> Result<(), TreeError> {
        if self.values.len() == self.limit {
            return Err(TreeError::Memory);
        }
        self.values.push(value);
        Ok(())
    }
}

impl<T: Copy> StorageBuffer<'_, T> {
    pub(crate) fn extend_from_slice(&mut self, input: &[T]) -> Result<(), TreeError> {
        if self
            .values
            .len()
            .checked_add(input.len())
            .is_none_or(|n| n > self.limit)
        {
            return Err(TreeError::Memory);
        }
        self.values.extend_from_slice(input);
        Ok(())
    }
}
