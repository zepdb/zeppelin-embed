//! State retained outside an unwind-catching scope; authenticity is supplied by
//! the real coordinator, never inferred from an exception or a response buffer.
use super::SuccessfulOutcome;
use std::cell::Cell;

/// Durable outcome metadata contains no tentative/fresh entity identities.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationOutcome {
    /// No possible mutation attempt has begun, or coordinator proved no effect.
    NotCommitted,
    /// Attempt began and durable outcome has not been established.
    Indeterminate,
    /// Known successful outcome; committed generation is never discarded.
    Success(SuccessfulOutcome),
}
/// Invalid state transition, including attempts to overwrite known outcomes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutcomeTransitionError;

/// Caller-thread outcome state. Keep this outside catch_unwind and record the
/// real coordinator result immediately, before delivery/cancellation checks.
/// This component cannot authenticate a commit or recover a durable outcome.
pub struct OutcomeCell {
    state: Cell<OperationOutcome>,
}
impl OutcomeCell {
    /// A read remains NotApplicable even if execution or delivery fails.
    pub const fn read() -> Self {
        Self {
            state: Cell::new(OperationOutcome::Success(SuccessfulOutcome::Read)),
        }
    }
    /// A possible write starts with definite no attempted effects.
    pub const fn write() -> Self {
        Self {
            state: Cell::new(OperationOutcome::NotCommitted),
        }
    }
    /// Current metadata, readable after unwind or delivery failure.
    pub fn get(&self) -> OperationOutcome {
        self.state.get()
    }
    /// Set before crossing the first possibly durable coordinator operation.
    pub fn begin_attempt(&self) -> Result<(), OutcomeTransitionError> {
        if self.get() != OperationOutcome::NotCommitted {
            return Err(OutcomeTransitionError);
        }
        self.state.set(OperationOutcome::Indeterminate);
        Ok(())
    }
    /// Record an authenticated successful coordinator outcome. Known success is
    /// terminal; repeating exactly the same result is harmless and allocation-free.
    pub fn record_success(&self, outcome: SuccessfulOutcome) -> Result<(), OutcomeTransitionError> {
        let next = OperationOutcome::Success(outcome);
        match (self.get(), outcome) {
            (previous, _) if previous == next => return Ok(()),
            (
                OperationOutcome::Indeterminate,
                SuccessfulOutcome::Committed(_)
                | SuccessfulOutcome::Replayed
                | SuccessfulOutcome::NoOp,
            )
            | (
                OperationOutcome::NotCommitted,
                SuccessfulOutcome::Replayed | SuccessfulOutcome::NoOp,
            ) => {}
            _ => return Err(OutcomeTransitionError),
        }
        self.state.set(next);
        Ok(())
    }
    /// Only a coordinator's definite no-effect result permits this transition.
    /// Never call it merely because cancellation, panic, or delivery failed.
    pub fn record_not_committed(&self) -> Result<(), OutcomeTransitionError> {
        if matches!(self.get(), OperationOutcome::Success(_)) {
            return Err(OutcomeTransitionError);
        }
        self.state.set(OperationOutcome::NotCommitted);
        Ok(())
    }
}
