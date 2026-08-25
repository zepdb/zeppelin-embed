//! Automatic per-segment storage-tier policy and host-invoked maintenance.

pub mod maintain;
pub mod policy;
pub mod thresholds;

pub use maintain::{
    GraphBuildProfileReport, MaintenanceBudget, MaintenanceError, MaintenanceReport,
    MaintenanceStatus,
};
pub use policy::{SegmentStats, StoreStats, TierPlan, decide};
pub use thresholds::{PROVISIONAL_TIER_THRESHOLDS, TierThresholds};

/// The query structure currently available for one segment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SegmentTier {
    /// Mutable rows are always searched exhaustively.
    ActiveScan,
    /// An immutable segment without a published graph is searched exhaustively.
    SealedScan,
    /// An immutable segment has a published graph accelerator.
    SealedGraph,
}
