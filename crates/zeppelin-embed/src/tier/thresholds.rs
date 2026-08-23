//! Provisional tier thresholds, shaped for later per-bucket Part-A evidence.

/// Thresholds selected for one `(dimensions, quantization scheme)` bucket.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TierThresholds {
    /// Minimum sealed-segment row count that earns graph construction.
    pub graph_min_rows: u32,
}

/// PROVISIONAL and pending Task 20 Part A's complete crossover matrix.
///
/// This is the single existing value from `graph::MIN_GRAPH_ROWS`; it is not
/// interpolated into invented dimension- or scheme-specific thresholds.
pub const PROVISIONAL_TIER_THRESHOLDS: TierThresholds = TierThresholds {
    graph_min_rows: 10_000,
};

/// Selects the threshold bucket for a segment's dimensions and scheme.
///
/// Part A will replace this single honest provisional arm with measured
/// `(dimensions, scheme)` buckets.
#[must_use]
pub const fn for_bucket(_dimensions: u32, _scheme: u16) -> TierThresholds {
    PROVISIONAL_TIER_THRESHOLDS
}
