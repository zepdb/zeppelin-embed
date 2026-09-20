//! Exact shared reservation observations over one declared interval.

use super::Accounting;
use std::sync::Arc;

#[derive(Clone, Copy)]
pub(super) struct AllocationIntervalState {
    pub(super) owner: u64,
    pub(super) start_reserved_bytes: u64,
    pub(super) peak_reserved_bytes: u64,
}

/// Exact shared managed-byte reservations observed over one declared interval.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AllocationIntervalSnapshot {
    /// Shared reservations present when the interval began.
    pub start_reserved_bytes: u64,
    /// Shared reservations present when this snapshot linearized.
    pub current_reserved_bytes: u64,
    /// Largest shared reservation total reached during this interval.
    pub peak_reserved_bytes: u64,
}

/// A shared allocation interval could not be observed faithfully.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AllocationIntervalError {
    /// Another interval already owns this Accounting observation slot.
    Busy,
    /// The existing accounting mutex is poisoned.
    Synchronization,
    /// The guard no longer owns an active observation slot.
    Inactive,
}

impl std::fmt::Display for AllocationIntervalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => formatter.write_str("allocation interval observation is busy"),
            Self::Synchronization => {
                formatter.write_str("allocation interval accounting synchronization failed")
            }
            Self::Inactive => formatter.write_str("allocation interval observation is inactive"),
        }
    }
}

impl std::error::Error for AllocationIntervalError {}

/// One non-clone observation guard bound only to the store's Accounting owner.
pub struct AllocationInterval {
    accounting: Arc<Accounting>,
    owner: u64,
    active: bool,
}

impl Accounting {
    pub(crate) fn begin_allocation_interval(
        self: &Arc<Self>,
    ) -> Result<AllocationInterval, AllocationIntervalError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AllocationIntervalError::Synchronization)?;
        if state.allocation_interval.is_some() {
            return Err(AllocationIntervalError::Busy);
        }
        let owner = state.allocation_interval_owner.wrapping_add(1);
        state.allocation_interval_owner = owner;
        let current = state.resident_owned_bytes;
        state.allocation_interval = Some(AllocationIntervalState {
            owner,
            start_reserved_bytes: current,
            peak_reserved_bytes: current,
        });
        drop(state);
        Ok(AllocationInterval {
            accounting: Arc::clone(self),
            owner,
            active: true,
        })
    }
}

impl AllocationInterval {
    /// Returns an exact snapshot without ending this interval.
    pub fn snapshot(&self) -> Result<AllocationIntervalSnapshot, AllocationIntervalError> {
        let state = self
            .accounting
            .state
            .lock()
            .map_err(|_| AllocationIntervalError::Synchronization)?;
        snapshot_for(&state, self.owner)
    }

    /// Returns the final exact snapshot and releases the observation slot.
    pub fn finish(mut self) -> Result<AllocationIntervalSnapshot, AllocationIntervalError> {
        let snapshot = {
            let mut state = self
                .accounting
                .state
                .lock()
                .map_err(|_| AllocationIntervalError::Synchronization)?;
            let snapshot = snapshot_for(&state, self.owner)?;
            state.allocation_interval = None;
            snapshot
        };
        self.active = false;
        Ok(snapshot)
    }
}

fn snapshot_for(
    state: &super::AccountingState,
    owner: u64,
) -> Result<AllocationIntervalSnapshot, AllocationIntervalError> {
    let interval = state
        .allocation_interval
        .filter(|interval| interval.owner == owner)
        .ok_or(AllocationIntervalError::Inactive)?;
    Ok(AllocationIntervalSnapshot {
        start_reserved_bytes: interval.start_reserved_bytes,
        current_reserved_bytes: state.resident_owned_bytes,
        peak_reserved_bytes: interval.peak_reserved_bytes,
    })
}

impl Drop for AllocationInterval {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut state = match self.accounting.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if state
            .allocation_interval
            .is_some_and(|interval| interval.owner == self.owner)
        {
            state.allocation_interval = None;
        }
        self.active = false;
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn graph_memory_interval_poison_returns_error_and_drop_releases_slot() {
        let accounting = Arc::new(Accounting::new(1024, 1024));
        let interval = accounting
            .begin_allocation_interval()
            .expect("begin observation");
        let poisoned = Arc::clone(&accounting);
        let result = std::panic::catch_unwind(move || {
            let _state = poisoned.state.lock().unwrap();
            panic!("poison interval accounting");
        });
        assert!(result.is_err());
        assert_eq!(
            interval.snapshot(),
            Err(AllocationIntervalError::Synchronization)
        );
        drop(interval);
        let state = match accounting.state.lock() {
            Ok(_) => panic!("accounting poison unexpectedly cleared"),
            Err(poisoned) => poisoned.into_inner(),
        };
        assert!(state.allocation_interval.is_none());
        drop(state);
        assert!(matches!(
            accounting.begin_allocation_interval(),
            Err(AllocationIntervalError::Synchronization)
        ));
    }
}
