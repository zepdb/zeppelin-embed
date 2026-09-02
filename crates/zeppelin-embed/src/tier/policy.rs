//! Pure per-segment tier decisions.

use super::{SegmentTier, TierThresholds};

/// Policy inputs derived from one segment and its currently published artifacts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SegmentStats {
    /// Tier the segment actually has now.
    pub tier: SegmentTier,
    /// Dense row count.
    pub row_count: u32,
    /// Logical vector dimensions.
    pub dimensions: u32,
    /// Permanent quantization scheme identifier.
    pub scheme: u16,
}

/// Store-wide inputs derived from manifest metadata without segment I/O.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StoreStats {
    /// Published-alias sealed segments that already carry a graph.
    pub graph_segment_count: u32,
    /// Total dense rows across those graph segments.
    pub total_graph_rows: u64,
    /// Dense rows of the largest single graph segment (manifest metadata only).
    pub largest_graph_segment_rows: u64,
}

/// Graph segment count at which consolidation is always due.
///
/// Owner policy A4 (2026-08-22 amendment recorded in
/// `tasks/19-vector-graph-tier.md`): multi-segment shared-bound search is a
/// transition state, not the steady state.
pub const CONSOLIDATE_MIN_SEGMENTS: u32 = 3;

/// At exactly two graph segments, consolidate unless one segment already
/// holds at least this many tenths of the total graph rows.
pub const CONSOLIDATE_DOMINANT_TENTHS: u128 = 7;

/// Pure store-level policy result for graph-segment consolidation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorePlan {
    /// Keep the published graph segment set.
    Stay,
    /// Merge every published graph segment into one new sealed graph segment.
    Consolidate,
}

/// Decides whether the store's sealed graph segments should merge into one.
///
/// Consolidation is due at [`CONSOLIDATE_MIN_SEGMENTS`] or more graph
/// segments, or at exactly two when neither segment holds at least 70% of
/// the total graph rows. The decision consults manifest metadata only.
#[must_use]
pub const fn decide_store(store: StoreStats) -> StorePlan {
    if store.graph_segment_count >= CONSOLIDATE_MIN_SEGMENTS {
        return StorePlan::Consolidate;
    }
    if store.graph_segment_count == 2 {
        let largest = store.largest_graph_segment_rows as u128;
        let total = store.total_graph_rows as u128;
        if largest * 10 < total * CONSOLIDATE_DOMINANT_TENTHS {
            return StorePlan::Consolidate;
        }
    }
    StorePlan::Stay
}

/// Pure policy result for one segment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TierPlan {
    /// Keep using the segment's published tier.
    Stay(SegmentTier),
    /// Build and atomically publish the named one-way transition.
    Transition {
        /// Published tier before maintenance.
        from: SegmentTier,
        /// Tier earned after maintenance.
        to: SegmentTier,
    },
}

/// Decides one segment's desired tier without consulting clocks or performing I/O.
#[must_use]
pub const fn decide(segment: SegmentStats, _store: StoreStats) -> TierPlan {
    let thresholds = super::thresholds::for_bucket(segment.dimensions, segment.scheme);
    decide_with_thresholds(segment, thresholds)
}

pub(crate) const fn decide_with_thresholds(
    segment: SegmentStats,
    thresholds: TierThresholds,
) -> TierPlan {
    match segment.tier {
        SegmentTier::ActiveScan => TierPlan::Stay(SegmentTier::ActiveScan),
        SegmentTier::SealedGraph => TierPlan::Stay(SegmentTier::SealedGraph),
        SegmentTier::SealedScan => {
            if segment.row_count >= thresholds.graph_min_rows {
                TierPlan::Transition {
                    from: SegmentTier::SealedScan,
                    to: SegmentTier::SealedGraph,
                }
            } else {
                TierPlan::Stay(SegmentTier::SealedScan)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SegmentStats, StorePlan, StoreStats, TierPlan, decide, decide_store};
    use crate::tier::{PROVISIONAL_TIER_THRESHOLDS, SegmentTier};

    #[test]
    fn policy_decides_graph_above_the_threshold_and_scan_below() {
        let threshold = PROVISIONAL_TIER_THRESHOLDS.graph_min_rows;
        let below = SegmentStats {
            tier: SegmentTier::SealedScan,
            row_count: threshold - 1,
            dimensions: 128,
            scheme: 4,
        };
        let above = SegmentStats {
            row_count: threshold,
            ..below
        };

        assert_eq!(
            decide(below, StoreStats::default()),
            TierPlan::Stay(SegmentTier::SealedScan)
        );
        assert_eq!(
            decide(above, StoreStats::default()),
            TierPlan::Transition {
                from: SegmentTier::SealedScan,
                to: SegmentTier::SealedGraph,
            }
        );
    }

    #[test]
    fn policy_never_demotes_a_segment_that_already_has_a_graph() {
        let segment = SegmentStats {
            tier: SegmentTier::SealedGraph,
            row_count: PROVISIONAL_TIER_THRESHOLDS.graph_min_rows - 1,
            dimensions: 128,
            scheme: 4,
        };

        assert_eq!(
            decide(segment, StoreStats::default()),
            TierPlan::Stay(SegmentTier::SealedGraph)
        );
    }

    #[test]
    fn store_policy_consolidates_at_three_or_more_graph_segments() {
        for (count, expected) in [
            (0, StorePlan::Stay),
            (1, StorePlan::Stay),
            (3, StorePlan::Consolidate),
            (4, StorePlan::Consolidate),
            (100, StorePlan::Consolidate),
        ] {
            assert_eq!(
                decide_store(StoreStats {
                    graph_segment_count: count,
                    total_graph_rows: 1_000_000,
                    largest_graph_segment_rows: 999_999,
                }),
                expected,
                "graph_segment_count={count}"
            );
        }
    }

    #[test]
    fn store_policy_at_two_segments_consolidates_only_without_a_dominant_segment() {
        // 50/50 split: no segment holds 70%, so the pair merges.
        assert_eq!(
            decide_store(StoreStats {
                graph_segment_count: 2,
                total_graph_rows: 1_000,
                largest_graph_segment_rows: 500,
            }),
            StorePlan::Consolidate
        );
        // 699/1000: still below the 70% dominance line.
        assert_eq!(
            decide_store(StoreStats {
                graph_segment_count: 2,
                total_graph_rows: 1_000,
                largest_graph_segment_rows: 699,
            }),
            StorePlan::Consolidate
        );
        // Exactly 70%: dominant, the pair stays.
        assert_eq!(
            decide_store(StoreStats {
                graph_segment_count: 2,
                total_graph_rows: 1_000,
                largest_graph_segment_rows: 700,
            }),
            StorePlan::Stay
        );
        // 95/5: a fresh small graph beside one consolidated graph stays.
        assert_eq!(
            decide_store(StoreStats {
                graph_segment_count: 2,
                total_graph_rows: 1_000,
                largest_graph_segment_rows: 950,
            }),
            StorePlan::Stay
        );
        // Row counts near u64::MAX must not overflow the tenths comparison.
        assert_eq!(
            decide_store(StoreStats {
                graph_segment_count: 2,
                total_graph_rows: u64::MAX,
                largest_graph_segment_rows: u64::MAX / 2,
            }),
            StorePlan::Consolidate
        );
    }
}
