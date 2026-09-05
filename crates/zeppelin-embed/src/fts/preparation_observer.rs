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
    phrase_reanalyses: AtomicUsize,
    phrase_text_bytes: AtomicUsize,
    phrase_positions: AtomicUsize,
    phrase_position_bytes: AtomicUsize,
    vocabulary_builds: AtomicUsize,
    vocabulary_copied_bytes: AtomicUsize,
    vocabulary_terms_visited: AtomicUsize,
    vocabulary_seek_steps: AtomicUsize,
    vocabulary_group_checks: AtomicUsize,
    fuzzy_candidates: AtomicUsize,
    fuzzy_dp_cells: AtomicUsize,
    fuzzy_row_allocations: AtomicUsize,
    fuzzy_scratch_constructions: AtomicUsize,
}

/// Actual dictionary construction and prefix traversal in one query window.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VocabularyWork {
    /// Complete dictionary constructions.
    pub builds: usize,
    /// Term payload bytes copied while building the dictionary.
    pub copied_bytes: usize,
    /// Terms examined after locating the prefix range.
    pub terms_visited: usize,
    /// Lower-bound comparisons.
    pub seek_steps: usize,
}

/// Reads the armed query's vocabulary work without ending observation.
pub fn vocabulary_work() -> VocabularyWork {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .map_or(VocabularyWork::default(), |o| VocabularyWork {
                builds: o.0.vocabulary_builds.load(Ordering::Relaxed),
                copied_bytes: o.0.vocabulary_copied_bytes.load(Ordering::Relaxed),
                terms_visited: o.0.vocabulary_terms_visited.load(Ordering::Relaxed),
                seek_steps: o.0.vocabulary_seek_steps.load(Ordering::Relaxed),
            })
    })
}

pub(crate) fn vocabulary_build(bytes: usize) {
    CURRENT.with(|current| {
        if let Some(o) = current.borrow().as_ref() {
            o.0.vocabulary_builds.fetch_add(1, Ordering::Relaxed);
            o.0.vocabulary_copied_bytes
                .fetch_add(bytes, Ordering::Relaxed);
        }
    });
}

pub(crate) fn vocabulary_visit() {
    CURRENT.with(|current| {
        if let Some(o) = current.borrow().as_ref() {
            o.0.vocabulary_terms_visited.fetch_add(1, Ordering::Relaxed);
        }
    });
}

pub(crate) fn vocabulary_seek() {
    CURRENT.with(|current| {
        if let Some(o) = current.borrow().as_ref() {
            o.0.vocabulary_seek_steps.fetch_add(1, Ordering::Relaxed);
        }
    });
}

/// Comparisons used to group sorted term/field memberships during a build.
pub fn vocabulary_group_checks() -> usize {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .map_or(0, |o| o.0.vocabulary_group_checks.load(Ordering::Relaxed))
    })
}

pub(crate) fn record_vocabulary_group_checks(count: usize) {
    CURRENT.with(|current| {
        if let Some(o) = current.borrow().as_ref() {
            o.0.vocabulary_group_checks
                .fetch_add(count, Ordering::Relaxed);
        }
    });
}

/// Actual fuzzy candidate distance calls, DP cells and row-buffer allocations.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FuzzyWork {
    /// Candidates submitted to the edit-distance matcher.
    pub candidates: usize,
    /// Evaluated dynamic-programming cells.
    pub dp_cells: usize,
    /// Heap row buffers constructed in the matcher.
    pub row_allocations: usize,
    /// Query-local bounded scratch objects constructed.
    pub scratch_constructions: usize,
}

/// Reads the armed query's fuzzy matching work.
pub fn fuzzy_work() -> FuzzyWork {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .map_or(FuzzyWork::default(), |o| FuzzyWork {
                candidates: o.0.fuzzy_candidates.load(Ordering::Relaxed),
                dp_cells: o.0.fuzzy_dp_cells.load(Ordering::Relaxed),
                row_allocations: o.0.fuzzy_row_allocations.load(Ordering::Relaxed),
                scratch_constructions: o.0.fuzzy_scratch_constructions.load(Ordering::Relaxed),
            })
    })
}

pub(crate) fn fuzzy_scratch() {
    CURRENT.with(|current| {
        if let Some(o) = current.borrow().as_ref() {
            o.0.fuzzy_scratch_constructions
                .fetch_add(1, Ordering::Relaxed);
        }
    });
}

pub(crate) fn fuzzy_distance(cells: usize, allocations: usize) {
    CURRENT.with(|current| {
        if let Some(o) = current.borrow().as_ref() {
            o.0.fuzzy_candidates.fetch_add(1, Ordering::Relaxed);
            o.0.fuzzy_dp_cells.fetch_add(cells, Ordering::Relaxed);
            o.0.fuzzy_row_allocations
                .fetch_add(allocations, Ordering::Relaxed);
        }
    });
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

/// Actual phrase eligibility analyzer calls and their input bytes. Snippet
/// analysis is separate and deliberately excluded.
pub fn phrase_reanalysis_work() -> (usize, usize) {
    CURRENT.with(|current| {
        current.borrow().as_ref().map_or((0, 0), |observer| {
            (
                observer.0.phrase_reanalyses.load(Ordering::Relaxed),
                observer.0.phrase_text_bytes.load(Ordering::Relaxed),
            )
        })
    })
}

#[cfg(test)]
pub(crate) fn phrase_reanalysis(bytes: usize) {
    CURRENT.with(|current| {
        if let Some(observer) = current.borrow().as_ref() {
            observer.0.phrase_reanalyses.fetch_add(1, Ordering::Relaxed);
            observer
                .0
                .phrase_text_bytes
                .fetch_add(bytes, Ordering::Relaxed);
        }
    });
}

/// Requested-row positions and logical packed byte ranges decoded for phrase
/// eligibility; immutable index validation and snippets are separate work.
pub fn phrase_position_work() -> (usize, usize) {
    CURRENT.with(|current| {
        current.borrow().as_ref().map_or((0, 0), |observer| {
            (
                observer.0.phrase_positions.load(Ordering::Relaxed),
                observer.0.phrase_position_bytes.load(Ordering::Relaxed),
            )
        })
    })
}

pub(crate) fn phrase_positions(positions: usize, bytes: usize) {
    CURRENT.with(|current| {
        if let Some(observer) = current.borrow().as_ref() {
            observer
                .0
                .phrase_positions
                .fetch_add(positions, Ordering::Relaxed);
            observer
                .0
                .phrase_position_bytes
                .fetch_add(bytes, Ordering::Relaxed);
        }
    });
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
