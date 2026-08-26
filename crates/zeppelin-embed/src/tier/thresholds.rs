//! Tier thresholds, measured on the single-tenant crossover ladder
//! (`tasks/evidence/20-crossover.md`, 27-B6b, 2026-08-26).

/// Thresholds selected for one `(dimensions, quantization scheme)` bucket.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TierThresholds {
    /// Minimum sealed-segment row count that earns graph construction.
    pub graph_min_rows: u32,
}

/// MEASURED (`tasks/evidence/20-crossover.md`): SIFT-128, one thread,
/// three-process medians, Bit4+rescore scan against a forced Vamana graph.
/// The task-20 rule (scan p50 >= 2x graph p50) first holds at 30,000 rows
/// (ratio 4.24 at k = 10, 3.43 at k = 100, graph recall 0.9985 / 0.9924);
/// at 10,000 rows the ratio is 1.82 / 1.75. The crossover therefore lies in
/// the measured bracket (10,000, 30,000] and this constant is the first
/// ladder point that satisfies the rule, not an interpolation. Only one
/// `(dimensions, scheme)` bucket is measured, so `for_bucket` still returns
/// this single arm for every bucket.
pub const MEASURED_TIER_THRESHOLDS: TierThresholds = TierThresholds {
    graph_min_rows: 30_000,
};

/// Kept as the name earlier tasks import; it now carries the measured value.
pub const PROVISIONAL_TIER_THRESHOLDS: TierThresholds = MEASURED_TIER_THRESHOLDS;

/// Selects the threshold bucket for a segment's dimensions and scheme.
///
/// One bucket (128-d, Bit4+rescore) is measured; other buckets reuse it
/// until they are measured on the same ladder.
#[must_use]
pub const fn for_bucket(_dimensions: u32, _scheme: u16) -> TierThresholds {
    MEASURED_TIER_THRESHOLDS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the measured crossover so a change is a policy decision with
    /// evidence, never a refactor.
    #[test]
    fn graph_min_rows_is_the_first_ladder_point_satisfying_the_two_x_rule() {
        assert_eq!(MEASURED_TIER_THRESHOLDS.graph_min_rows, 30_000);
        assert_eq!(for_bucket(128, 0), MEASURED_TIER_THRESHOLDS);
        assert_eq!(for_bucket(784, 1), MEASURED_TIER_THRESHOLDS);
    }
}
