//! Bounded producers for `Store::search_hybrid`.
//!
//! The hybrid legs are windowed: each produces its own top-W plus the exact
//! extreme its normalization range needs, and fusion proves the window with
//! the (W+1)-th element. Nothing here scales with the corpus except the
//! clamp that keeps a window from exceeding it.

use crate::fusion::{FusionError, HYBRID_WINDOW_FLOOR, HYBRID_WINDOW_PER_K};
use crate::lifecycle::QueryError;

/// Requested per-leg window. Both legs share one width so the stability
/// bound has exactly one (W+1)-th element per leg.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HybridWindow {
    /// Rows each producer is asked for, never above the corpus.
    pub(crate) width: usize,
}

/// Resolves the per-leg window for one hybrid round.
///
/// The bounded producers that consume it land in the next commit; the clamp
/// ships with its own test first so the window contract is pinned before a
/// caller can depend on it.
///
/// The width is `max(HYBRID_WINDOW_FLOOR, HYBRID_WINDOW_PER_K * k)` raised to
/// `k` and then clamped to the corpus, so a producer is never asked for more
/// rows than exist and never for fewer than the caller's own `k`. A `k` of
/// zero requests nothing: fusion already returns an empty result.
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "the bounded producers that call it land in the next commit"
    )
)]
pub(crate) fn hybrid_window(k: usize, corpus_rows: usize) -> Result<HybridWindow, FusionError> {
    if k == 0 {
        return Ok(HybridWindow { width: 0 });
    }
    let per_k = HYBRID_WINDOW_PER_K.checked_mul(k).ok_or_else(overflow)?;
    let width = per_k.max(HYBRID_WINDOW_FLOOR).max(k).min(corpus_rows);
    Ok(HybridWindow { width })
}

fn overflow() -> FusionError {
    FusionError::from(QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use super::{HYBRID_WINDOW_FLOOR, HybridWindow, hybrid_window};
    use crate::fusion::{FusionError, LegFailureKind};

    #[test]
    fn hybrid_window_is_clamped_between_k_and_the_corpus() {
        assert_eq!(
            hybrid_window(10, 4_096).expect("window"),
            HybridWindow {
                width: HYBRID_WINDOW_FLOOR
            },
            "the floor dominates a small k on a large corpus"
        );
        assert_eq!(
            hybrid_window(40, 4_096).expect("window").width,
            200,
            "the per-k term dominates once it passes the floor"
        );
        assert_eq!(
            hybrid_window(3, 2).expect("window").width,
            2,
            "the corpus clamps the floor down"
        );
        assert_eq!(
            hybrid_window(4_096, 4_096).expect("window").width,
            4_096,
            "a k at the corpus size still fits inside the corpus"
        );
        assert_eq!(
            hybrid_window(0, 9).expect("window").width,
            0,
            "k of zero asks the producers for nothing"
        );
        assert!(
            matches!(
                hybrid_window(usize::MAX, 9),
                Err(FusionError::Leg {
                    kind: LegFailureKind::Scan,
                    ..
                })
            ),
            "an unrepresentable window is typed, never a panic"
        );
    }
}
