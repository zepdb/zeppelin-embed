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

/// Store-wide inputs reserved for workload-aware policy without adding I/O.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StoreStats;

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
    use super::{SegmentStats, StoreStats, TierPlan, decide};
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
            decide(below, StoreStats),
            TierPlan::Stay(SegmentTier::SealedScan)
        );
        assert_eq!(
            decide(above, StoreStats),
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
            decide(segment, StoreStats),
            TierPlan::Stay(SegmentTier::SealedGraph)
        );
    }
}
