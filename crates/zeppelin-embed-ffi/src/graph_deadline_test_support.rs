//! Deterministic clock injection for scoped C graph deadline qualification.
use std::cell::{Cell, RefCell};
use std::sync::Arc;
use zeppelin_embed::lifecycle::MonotonicClock;
thread_local! {
    static CLASSIFIED: Cell<bool> = const { Cell::new(false) };
    static CLOCK: RefCell<Option<Arc<dyn MonotonicClock>>> = const { RefCell::new(None) };
}
/// Restores the calling thread's prior test clock on drop.
pub struct ClockScope(
    Option<Arc<dyn MonotonicClock>>,
    std::marker::PhantomData<std::rc::Rc<()>>,
    bool,
);
impl ClockScope {
    /// Uses this clock only for relative deadlines constructed on this thread.
    pub fn install(clock: Arc<dyn MonotonicClock>) -> Self {
        Self(
            CLOCK.with(|slot| slot.replace(Some(clock))),
            std::marker::PhantomData,
            CLASSIFIED.with(|phase| phase.replace(false)),
        )
    }
}
impl Drop for ClockScope {
    fn drop(&mut self) {
        CLASSIFIED.with(|phase| phase.set(self.2));
        CLOCK.with(|slot| {
            slot.replace(self.0.take());
        });
    }
}
pub(crate) fn clock() -> Option<Arc<dyn MonotonicClock>> {
    CLOCK.with(|slot| slot.borrow().clone())
}

/// Whether core validation and binding admission have completed on this thread.
pub fn query_classified() -> bool {
    CLASSIFIED.with(Cell::get)
}
pub(crate) fn classified() {
    CLASSIFIED.with(|phase| phase.set(true));
}
