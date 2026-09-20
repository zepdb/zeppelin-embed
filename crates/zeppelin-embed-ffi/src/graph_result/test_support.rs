//! Explicit real allocation-site fault scheduling, not a system-allocator audit.
//! This module is absent from default and graph-cypher-only builds.
use std::cell::Cell;
use std::marker::PhantomData;

mod conversion;
pub use conversion::{
    NativeConversionCase, NativeConversionFailure, NativeConversionObservation,
    NativeConversionOutcome, NativeConversionRefusal, NativeConversionStage,
    run_native_conversion_case,
};

#[derive(Clone, Copy)]
struct State {
    enabled: bool,
    fail_at: usize,
    attempts: usize,
    fires: usize,
}
thread_local! {
    static STATE: Cell<State> = const { Cell::new(State { enabled: false, fail_at: 0, attempts: 0, fires: 0 }) };
}
/// Exact receipts from the two actual allocation sites reached in this scope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AllocationFaultReceipt {
    /// Real owner arena/node allocation sites reached (including refused site).
    pub matching_sites: usize,
    /// Refused sites; zero or one for a nonrepeating ordinal schedule.
    pub fires: usize,
}
/// Calling-thread-only controller. Drop restores the previous state, including
/// through unwind. No heap allocation, hidden mutex or runtime C export exists.
pub struct AllocationFaultScope {
    previous: State,
    _thread: PhantomData<*mut ()>,
}
impl AllocationFaultScope {
    /// Refuses one ordinal, or observes the same clean operation when zero.
    pub fn arm(fail_at: usize) -> Self {
        let previous = STATE.with(|state| {
            state.replace(State {
                enabled: true,
                fail_at,
                attempts: 0,
                fires: 0,
            })
        });
        Self {
            previous,
            _thread: PhantomData,
        }
    }
    /// Actual matching/fire receipts, not a requested schedule interpreted as proof.
    pub fn receipt(&self) -> AllocationFaultReceipt {
        STATE.with(|state| {
            let state = state.get();
            AllocationFaultReceipt {
                matching_sites: state.attempts,
                fires: state.fires,
            }
        })
    }
}
impl Drop for AllocationFaultScope {
    fn drop(&mut self) {
        STATE.with(|state| state.set(self.previous));
    }
}
pub(super) fn refuse_allocation() -> bool {
    STATE.with(|cell| {
        let mut state = cell.get();
        if !state.enabled {
            return false;
        }
        state.attempts = state.attempts.saturating_add(1);
        let refuse = state.attempts == state.fail_at;
        if refuse {
            state.fires = state.fires.saturating_add(1);
            state.fail_at = 0;
        }
        cell.set(state);
        refuse
    })
}

pub(super) fn current_allocation_fault_receipt() -> AllocationFaultReceipt {
    STATE.with(|state| {
        let state = state.get();
        AllocationFaultReceipt {
            matching_sites: state.attempts,
            fires: state.fires,
        }
    })
}
