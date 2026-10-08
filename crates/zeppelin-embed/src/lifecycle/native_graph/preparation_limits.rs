//! Test-only tightening of the general graph preparation budgets.
use std::cell::Cell;
thread_local! { static LIMITS: Cell<(Option<usize>, Option<u64>)> = const { Cell::new((None, None)) }; }
pub(crate) struct Guard((Option<usize>, Option<u64>));
impl Drop for Guard {
    fn drop(&mut self) {
        LIMITS.with(|limits| limits.set(self.0));
    }
}
pub(crate) fn install(storage: Option<usize>, work: Option<u64>) -> Guard {
    Guard(LIMITS.with(|limits| limits.replace((storage, work))))
}
pub(crate) fn limits(storage: usize, work: u64) -> (usize, u64) {
    LIMITS.with(|limits| {
        let (bytes, units) = limits.get();
        (bytes.unwrap_or(storage), units.unwrap_or(work))
    })
}
