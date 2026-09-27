//! Temporary mutation lookup tables and test-only logical work accounting.
use std::collections::HashSet;
use std::hash::Hash;

pub(super) type LookupSet<T> = HashSet<T>;

pub(super) fn set<T: Copy + Eq + Hash>(values: &[T]) -> LookupSet<T> {
    work(values.len());
    values.iter().copied().collect()
}

pub(super) fn contains<T: Eq + Hash>(values: &LookupSet<T>, value: &T) -> bool {
    work(1);
    values.contains(value)
}

#[inline]
pub(super) fn work(amount: usize) {
    #[cfg(test)]
    WORK.with(|count| count.set(count.get() + amount));
    #[cfg(not(test))]
    let _ = amount;
}

#[cfg(test)]
thread_local! {
    static WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(super) fn take_work() -> usize {
    WORK.with(|count| count.replace(0))
}
