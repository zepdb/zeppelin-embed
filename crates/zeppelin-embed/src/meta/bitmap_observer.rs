//! Thread-local work observation for explicitly armed bitmap-bound tests.

use std::cell::Cell;

/// Actual iterator advances and ordered probes in one observation window.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Work {
    /// Values yielded by ordinary bitmap iteration.
    pub rows: usize,
    /// Ordered range lookups, independent of the size of the valid prefix.
    pub probes: usize,
}

thread_local! {
    static CURRENT: Cell<Option<Work>> = const { Cell::new(None) };
}

/// Starts an isolated observation window on this thread.
pub fn begin() {
    CURRENT.set(Some(Work::default()));
}

/// Ends observation and returns the recorded work.
pub fn take() -> Work {
    CURRENT.replace(None).unwrap_or_default()
}

pub(crate) fn row() {
    CURRENT.with(|current| {
        if let Some(mut work) = current.get() {
            work.rows += 1;
            current.set(Some(work));
        }
    });
}

pub(crate) fn probe() {
    CURRENT.with(|current| {
        if let Some(mut work) = current.get() {
            work.probes += 1;
            current.set(Some(work));
        }
    });
}
