use super::super::resources::{GraphReservation, GraphResources};
use super::{StageError, WriteControl, WriteLimits, WritePhase};
use std::{cell::Cell, ops::Deref};

/// One writer admission's overlapping capacity, backed by the store's accounting.
pub struct WriteMemory<'a> {
    resources: &'a GraphResources,
    pub(super) limits: WriteLimits,
    used: Cell<usize>,
}
impl<'a> WriteMemory<'a> {
    /// Borrows the same store owner used by graph, search and result participants.
    pub fn new(resources: &'a GraphResources, limits: WriteLimits) -> Result<Self, StageError> {
        limits.validate()?;
        Ok(Self {
            resources,
            limits,
            used: Cell::new(0),
        })
    }
    /// Live charged writer capacity, including temporary overlap.
    pub fn reserved_bytes(&self) -> usize {
        self.used.get()
    }

    pub(crate) const fn resources(&self) -> &'a GraphResources {
        self.resources
    }
}
pub(super) struct Arena<'a, T> {
    values: Vec<T>,
    limit: usize,
    charge: WriteReservation<'a>,
}
impl<'a, T> Arena<'a, T> {
    pub(super) fn uses_memory(&self, memory: &WriteMemory<'_>) -> bool {
        std::ptr::eq(self.charge.writer.memory, memory)
    }
    pub(super) fn new(
        memory: &'a WriteMemory<'a>,
        capacity: usize,
        control: &mut WriteControl<'_>,
    ) -> Result<Self, StageError> {
        let bytes = capacity
            .checked_mul(std::mem::size_of::<T>())
            .ok_or(StageError::Limit)?;
        let mut charge = memory.reserve(bytes, control)?;
        let mut values = Vec::new();
        #[cfg(feature = "allocation-audit")]
        let result = crate::allocation_audit::attributed(|| values.try_reserve_exact(capacity));
        #[cfg(not(feature = "allocation-audit"))]
        let result = values.try_reserve_exact(capacity);
        result.map_err(|_| {
            StageError::Memory(crate::lifecycle::StoreError::AllocationFailed {
                needed: bytes as u64,
                component: "graph write staging",
            })
        })?;
        let actual = values
            .capacity()
            .checked_mul(std::mem::size_of::<T>())
            .ok_or(StageError::Limit)?;
        charge.resize(actual)?;
        Ok(Self {
            values,
            charge,
            limit: capacity,
        })
    }
    pub(super) fn into_parts(self) -> (Vec<T>, WriteReservation<'a>) {
        (self.values, self.charge)
    }
    pub(super) fn allocated_bytes(&self) -> usize {
        self.charge.bytes() as usize
    }
    pub(super) fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.values
    }
    pub(super) fn push(&mut self, value: T) -> Result<(), StageError> {
        if self.values.len() == self.limit {
            return Err(StageError::Limit);
        }
        self.values.push(value);
        Ok(())
    }
}
impl<T> Deref for Arena<'_, T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.values
    }
}
impl std::io::Write for Arena<'_, u8> {
    fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
        let remaining = self.limit.saturating_sub(self.values.len());
        if input.len() > remaining {
            return Err(std::io::ErrorKind::OutOfMemory.into());
        }
        self.values.extend_from_slice(input);
        Ok(input.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Failed transfer preserves the original shared reservation without allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteAdoptionError {
    /// The reservation belongs to another store's accounting owner.
    ForeignOwner,
    /// The writer-local envelope cannot retain this capacity.
    Limit,
}
impl From<WriteAdoptionError> for StageError {
    fn from(error: WriteAdoptionError) -> Self {
        match error {
            WriteAdoptionError::ForeignOwner => Self::ViewMismatch,
            WriteAdoptionError::Limit => Self::Limit,
        }
    }
}
/// Writer and shared aggregate reservation for a participant-owned allocation.
/// Keep backing ownership before this token in drop order; report exact capacity.
pub struct WriteReservation<'a> {
    charge: GraphReservation,
    writer: WriterCapacity<'a>,
}
/// Writer-local half of a consumed shared reservation. The backing owner keeps
/// this guard alongside the adopted aggregate/query reservation until drop.
pub(super) struct WriterCapacity<'a> {
    memory: &'a WriteMemory<'a>,
    bytes: usize,
}
impl WriteMemory<'_> {
    /// Reserves participant capacity before allocation without creating backing.
    pub fn reserve(
        &self,
        bytes: usize,
        control: &mut WriteControl<'_>,
    ) -> Result<WriteReservation<'_>, StageError> {
        control(WritePhase::Allocate)?;
        let total = self
            .used
            .get()
            .checked_add(bytes)
            .ok_or(StageError::Limit)?;
        if total > self.limits.writer_bytes {
            return Err(StageError::Limit);
        }
        let charge = self.resources.reserve(bytes)?;
        self.adopt(charge).map_err(|(error, _charge)| error.into())
    }
    /// Adds writer-local ownership to an existing shared reservation without
    /// charging aggregate bytes twice. The immutable backing must remain beside
    /// the returned owner. Failure returns the original reservation unchanged.
    pub fn adopt(
        &self,
        charge: GraphReservation,
    ) -> Result<WriteReservation<'_>, (WriteAdoptionError, GraphReservation)> {
        if !charge.belongs_to(self.resources) {
            return Err((WriteAdoptionError::ForeignOwner, charge));
        }
        let Ok(bytes) = usize::try_from(charge.bytes()) else {
            return Err((WriteAdoptionError::Limit, charge));
        };
        let Some(total) = self.used.get().checked_add(bytes) else {
            return Err((WriteAdoptionError::Limit, charge));
        };
        if total > self.limits.writer_bytes {
            return Err((WriteAdoptionError::Limit, charge));
        }
        self.used.set(total);
        Ok(WriteReservation {
            charge,
            writer: WriterCapacity {
                memory: self,
                bytes,
            },
        })
    }
}
impl<'a> WriteReservation<'a> {
    /// Exact currently retained participant capacity.
    pub fn bytes(&self) -> u64 {
        self.charge.bytes()
    }
    pub(crate) fn resize(&mut self, bytes: usize) -> Result<(), StageError> {
        let total = self
            .writer
            .memory
            .used
            .get()
            .checked_sub(self.writer.bytes)
            .and_then(|n| n.checked_add(bytes))
            .ok_or(StageError::Limit)?;
        if total > self.writer.memory.limits.writer_bytes {
            return Err(StageError::Limit);
        }
        self.charge.resize(bytes)?;
        self.writer.memory.used.set(total);
        self.writer.bytes = bytes;
        Ok(())
    }
    pub(super) fn split(self) -> (GraphReservation, WriterCapacity<'a>) {
        (self.charge, self.writer)
    }
    pub(super) fn from_parts(charge: GraphReservation, writer: WriterCapacity<'a>) -> Self {
        Self { charge, writer }
    }
}
impl Drop for WriterCapacity<'_> {
    fn drop(&mut self) {
        self.memory
            .used
            .set(self.memory.used.get().saturating_sub(self.bytes));
    }
}
impl Arena<'_, u8> {
    pub(super) fn zeroed(
        &mut self,
        control: &mut WriteControl<'_>,
        phase: WritePhase,
    ) -> Result<(), StageError> {
        let zeroes = [0u8; 65536];
        while self.values.len() < self.limit {
            control(phase)?;
            let count = (self.limit - self.values.len()).min(zeroes.len());
            self.values
                .extend_from_slice(zeroes.get(..count).ok_or(StageError::InvalidInput)?);
        }
        Ok(())
    }
}
