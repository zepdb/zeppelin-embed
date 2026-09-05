//! Call-site observations shared only by one explicitly armed test query.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Default)]
struct Counts {
    scorers: AtomicUsize,
    frequencies: AtomicUsize,
    expansions: AtomicUsize,
    structured_retained_rows: AtomicUsize,
    structured_bound_terms: AtomicUsize,
    corpus_statistics: AtomicUsize,
}

/// Opaque handle passed to the same query's worker; no global counting window.
#[derive(Clone, Default)]
pub struct Observer(Arc<Counts>);

thread_local! {
    static CURRENT: std::cell::RefCell<Option<Observer>> = const { std::cell::RefCell::new(None) };
}

/// Starts a fresh caller-thread observation window.
pub fn begin() {
    CURRENT.with(|current| *current.borrow_mut() = Some(Observer::default()));
}

/// Stops caller observation and returns actual (scorer, frequency) call counts.
pub fn take() -> (usize, usize) {
    let (scorers, frequencies, _) = take_with_expansions();
    (scorers, frequencies)
}

/// Stops observation and also returns actual structured expansion calls.
pub fn take_with_expansions() -> (usize, usize, usize) {
    let (scorers, frequencies, expansions, _) = take_with_collection();
    (scorers, frequencies, expansions)
}

/// Also returns the peak row count held by a structured result collector.
pub fn take_with_collection() -> (usize, usize, usize, usize) {
    let (scorers, frequencies, expansions, retained, _) = take_with_bounds();
    (scorers, frequencies, expansions, retained)
}

/// Also returns the number of term contributions examined by weighted bounds.
pub fn take_with_bounds() -> (usize, usize, usize, usize, usize) {
    CURRENT.with(|current| {
        current.take().map_or((0, 0, 0, 0, 0), |observer| {
            (
                observer.0.scorers.load(Ordering::Relaxed),
                observer.0.frequencies.load(Ordering::Relaxed),
                observer.0.expansions.load(Ordering::Relaxed),
                observer.0.structured_retained_rows.load(Ordering::Relaxed),
                observer.0.structured_bound_terms.load(Ordering::Relaxed),
            )
        })
    })
}

pub(crate) fn structured_bounds(terms: usize) {
    CURRENT.with(|current| {
        if let Some(observer) = current.borrow().as_ref() {
            observer
                .0
                .structured_bound_terms
                .fetch_add(terms, Ordering::Relaxed);
        }
    });
}

/// Reads corpus-statistics preparations in the current observation window.
pub fn corpus_statistics_calls() -> usize {
    CURRENT.with(|current| {
        current.borrow().as_ref().map_or(0, |observer| {
            observer.0.corpus_statistics.load(Ordering::Relaxed)
        })
    })
}

pub(crate) fn corpus_statistics() {
    CURRENT.with(|current| {
        if let Some(observer) = current.borrow().as_ref() {
            observer.0.corpus_statistics.fetch_add(1, Ordering::Relaxed);
        }
    });
}

pub(crate) fn structured_collection(rows: usize) {
    CURRENT.with(|current| {
        if let Some(observer) = current.borrow().as_ref() {
            observer
                .0
                .structured_retained_rows
                .fetch_max(rows, Ordering::Relaxed);
        }
    });
}

pub(crate) fn current() -> Option<Observer> {
    CURRENT.with(|current| current.borrow().clone())
}

pub(crate) struct Scope(Option<Observer>);
impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.with(|current| *current.borrow_mut() = self.0.take());
    }
}

pub(crate) fn enter(observer: Option<Observer>) -> Scope {
    Scope(CURRENT.with(|current| current.replace(observer)))
}

pub(crate) fn scorer() {
    CURRENT.with(|current| {
        if let Some(observer) = current.borrow().as_ref() {
            observer.0.scorers.fetch_add(1, Ordering::Relaxed);
        }
    });
}

pub(crate) fn frequency() {
    CURRENT.with(|current| {
        if let Some(observer) = current.borrow().as_ref() {
            observer.0.frequencies.fetch_add(1, Ordering::Relaxed);
        }
    });
}

pub(crate) fn expansion() {
    CURRENT.with(|current| {
        if let Some(observer) = current.borrow().as_ref() {
            observer.0.expansions.fetch_add(1, Ordering::Relaxed);
        }
    });
}
