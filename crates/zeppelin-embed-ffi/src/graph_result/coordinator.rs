//! Potential-write outcome guard. The outcome cell lives in this frame,
//! outside the unwind-catching scope, and reads Indeterminate before the body
//! can reach a commit. Only the body's unique attempt can resolve it: a real
//! settle records the decided outcome and then publishes, and a definite
//! no-effect result records NotCommitted. An error return or a caught panic
//! that resolves neither leaves Indeterminate, never a stale definite outcome.
use super::*;
use std::panic::{AssertUnwindSafe, catch_unwind};
use zeppelin_embed::property_graph::staging::ItemReceipt;

/// Why a guarded potential write returned no value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteInterrupted {
    /// A panic unwound out of the body and was caught at this guard.
    Panicked,
}

/// Outcome metadata and the body's value. The outcome is valid in both arms,
/// including after a caught panic.
#[derive(Debug)]
pub struct GuardedWrite<T> {
    /// Final outcome state; Indeterminate unless the attempt resolved it.
    pub outcome: OperationOutcome,
    /// The body's value, or the interruption that replaced it.
    pub value: Result<T, WriteInterrupted>,
}

/// The unique, single-use right to resolve this write's outcome. It cannot
/// escape the body; dropping it unresolved leaves Indeterminate.
pub struct WriteAttempt<'a> {
    cell: &'a OutcomeCell,
}
impl WriteAttempt<'_> {
    /// The coordinator already decided this outcome; only delivering the C
    /// response failed. Records the known outcome so it is never reported
    /// as Indeterminate. Nothing is published.
    pub fn delivery_failed(self, settlement: WriteSettlement) {
        self.cell
            .resolve_attempt(OperationOutcome::Success(settlement.outcome()));
    }

    /// Post-commit settle: records the decided outcome in the guard's cell
    /// first, then stamps and publishes the pending result. Infallible and
    /// allocation-free; after it returns, free accepts the descriptor.
    pub fn settle(
        self,
        pending: PendingResponse,
        receipts: &[ItemReceipt],
        settlement: WriteSettlement,
    ) -> ZeGraphResponse {
        self.cell
            .resolve_attempt(OperationOutcome::Success(settlement.outcome()));
        pending.settle(receipts, settlement)
    }
    /// The coordinator proved the attempt had no durable effect. Never call
    /// this merely because cancellation, panic, or delivery failed.
    pub fn no_effect(self) {
        self.cell.resolve_attempt(OperationOutcome::NotCommitted);
    }
}

/// Runs one potential write. The outcome reads Indeterminate before `body`
/// starts, so any interruption before a resolution is reported as unknown.
/// A known outcome recorded by settle survives a later panic in the body.
pub fn run_potential_write<T>(body: impl FnOnce(WriteAttempt<'_>) -> T) -> GuardedWrite<T> {
    let cell = OutcomeCell::attempting();
    let value = catch_unwind(AssertUnwindSafe(|| body(WriteAttempt { cell: &cell })))
        .map_err(|_| WriteInterrupted::Panicked);
    GuardedWrite {
        outcome: cell.get(),
        value,
    }
}

#[cfg(test)]
mod tests;
