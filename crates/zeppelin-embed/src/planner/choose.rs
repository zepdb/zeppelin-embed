//! Small exact-cardinality branch table.

use super::SegmentBranch;

/// Maximum allow-list cardinality that drives row gathering.
///
/// PLACEHOLDER -- NOT YET MEASURED. Both sides are exact; a poor value can
/// only make a query slower. The control-armed calibration protocol is
/// recorded in `tasks/evidence/16-planner-thresholds.md`.
pub const ALLOW_LIST_ROWS_THRESHOLD: u64 = 64;

/// Chooses between the two exact scan branches from exact cardinality.
#[must_use]
pub const fn choose_scan_branch(cardinality: u64) -> SegmentBranch {
    if cardinality <= ALLOW_LIST_ROWS_THRESHOLD {
        SegmentBranch::ExactAllowList
    } else {
        SegmentBranch::MaskedScan
    }
}
