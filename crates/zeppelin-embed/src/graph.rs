//! Per-segment vector graphs.

/// Persisted fixed-stride graph node blocks.
pub mod block;
/// Deterministic, checkpointable flat Vamana construction.
pub mod build;
/// Copy-on-write consolidation of sealed graph segments (Task 19-M8).
pub mod consolidate;
/// Copy-on-write post-consolidation graph refinement passes.
pub mod refine;
/// Single-core fixed-stride graph traversal.
pub mod search;

/// Compatibility name for the tier policy's single provisional threshold.
pub const MIN_GRAPH_ROWS: u32 = crate::tier::PROVISIONAL_TIER_THRESHOLDS.graph_min_rows;

/// Validated construction controls for one flat per-segment graph.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GraphParams {
    r_target: u8,
    r_max: u8,
    alpha_build: f32,
    alpha_refine: f32,
    l_build: u16,
    checkpoint_batch_rows: u32,
}

impl GraphParams {
    /// Creates validated Vamana construction controls.
    pub fn new(
        r_target: u8,
        r_max: u8,
        alpha_build: f32,
        alpha_refine: f32,
        l_build: u16,
        checkpoint_batch_rows: u32,
    ) -> Result<Self, GraphParamsError> {
        if r_target == 0 || r_target > r_max {
            return Err(GraphParamsError::DegreeOrder { r_target, r_max });
        }
        if !alpha_build.is_finite()
            || !alpha_refine.is_finite()
            || alpha_build < 1.0
            || alpha_refine < alpha_build
        {
            return Err(GraphParamsError::AlphaOrder {
                alpha_build,
                alpha_refine,
            });
        }
        if l_build == 0 || usize::from(l_build) < usize::from(r_target) {
            return Err(GraphParamsError::ConstructionWidth { l_build, r_target });
        }
        if checkpoint_batch_rows == 0 {
            return Err(GraphParamsError::CheckpointBatchRows);
        }
        Ok(Self {
            r_target,
            r_max,
            alpha_build,
            alpha_refine,
            l_build,
            checkpoint_batch_rows,
        })
    }

    /// Returns the Task 19-M3 SIFT-1M construction contract.
    #[must_use]
    pub const fn sift_1m() -> Self {
        Self {
            // M3/M5 SIFT measurements: `docs/19-m5-defaults.md`.
            r_target: 32,
            // M3/M5 SIFT measurements: `docs/19-m5-defaults.md`.
            r_max: 44,
            // Shipped one-pass alpha measured in `docs/19-m5-defaults.md`.
            alpha_build: 1.0,
            // Explicit two-pass arm only; rejected as a default by the matched
            // measurement in `docs/19-m5-defaults.md`.
            alpha_refine: 1.2,
            // M3/M5 SIFT measurements: `docs/19-m5-defaults.md`.
            l_build: 100,
            // Operational M3 choice, NOT MEASURED; documented plainly in
            // `docs/19-m5-defaults.md`.
            checkpoint_batch_rows: 65_536,
        }
    }

    /// Selects a different positive checkpoint batch size without changing graph quality.
    pub fn with_checkpoint_batch_rows(
        self,
        checkpoint_batch_rows: u32,
    ) -> Result<Self, GraphParamsError> {
        Self::new(
            self.r_target,
            self.r_max,
            self.alpha_build,
            self.alpha_refine,
            self.l_build,
            checkpoint_batch_rows,
        )
    }

    /// Returns whether a sealed segment crosses Amendment A1's graph threshold.
    #[must_use]
    pub const fn should_build(self, row_count: u32) -> bool {
        row_count >= MIN_GRAPH_ROWS
    }

    /// Returns the normal pruned out-degree.
    #[must_use]
    pub const fn r_target(self) -> u8 {
        self.r_target
    }

    /// Returns the persisted hard degree cap.
    #[must_use]
    pub const fn r_max(self) -> u8 {
        self.r_max
    }

    /// Returns the first-pass occlusion factor.
    #[must_use]
    pub const fn alpha_build(self) -> f32 {
        self.alpha_build
    }

    /// Returns the second-pass occlusion factor.
    #[must_use]
    pub const fn alpha_refine(self) -> f32 {
        self.alpha_refine
    }

    /// Returns the construction beam width.
    #[must_use]
    pub const fn l_build(self) -> u16 {
        self.l_build
    }

    /// Returns rows processed between resumable checkpoints.
    #[must_use]
    pub const fn checkpoint_batch_rows(self) -> u32 {
        self.checkpoint_batch_rows
    }
}

/// Rejection of loose or contradictory graph-construction controls.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GraphParamsError {
    /// The target degree was zero or exceeded the hard persisted cap.
    DegreeOrder {
        /// Normal pruned degree.
        r_target: u8,
        /// Hard persisted cap.
        r_max: u8,
    },
    /// Alpha values were non-finite, below one, or decreased on refinement.
    AlphaOrder {
        /// First-pass alpha.
        alpha_build: f32,
        /// Second-pass alpha.
        alpha_refine: f32,
    },
    /// The construction beam was zero or narrower than the target degree.
    ConstructionWidth {
        /// Construction beam width.
        l_build: u16,
        /// Normal pruned degree.
        r_target: u8,
    },
    /// A checkpoint batch must contain at least one row.
    CheckpointBatchRows,
}

impl std::fmt::Display for GraphParamsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DegreeOrder { r_target, r_max } => write!(
                formatter,
                "graph target degree {r_target} must be positive and at most hard cap {r_max}"
            ),
            Self::AlphaOrder {
                alpha_build,
                alpha_refine,
            } => write!(
                formatter,
                "graph alpha must be finite and ordered 1 <= build <= refine, got {alpha_build} then {alpha_refine}"
            ),
            Self::ConstructionWidth { l_build, r_target } => write!(
                formatter,
                "graph construction width {l_build} must be at least target degree {r_target}"
            ),
            Self::CheckpointBatchRows => {
                formatter.write_str("graph checkpoint batch rows must be positive")
            }
        }
    }
}

impl std::error::Error for GraphParamsError {}

use crate::scan::{ScanCandidate, ScanError, ScanRequest};
use crate::segment::SegmentError;
use crate::segment::reader::SegmentReader;

/// Query-tier decision after validating a segment's derived graph accelerator.
#[derive(Debug)]
pub enum GraphLoadOutcome<'a> {
    /// The graph region is valid and may be consumed by the later traversal milestone.
    Graph(block::GraphNodeBlocks<'a>),
    /// The derived graph failed loudly and correctness was served by exact scan.
    ExactScanFallback {
        /// Typed graph/segment failure retained for diagnostics and callers.
        error: SegmentError,
        /// Exact candidates including every k-th-score boundary tie.
        candidates: Vec<ScanCandidate>,
    },
}

/// Failure of the correctness-preserving exact tier after graph-load failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GraphQueryError {
    /// The exact scan request itself was invalid or scoring failed.
    ExactScan(ScanError),
}

impl std::fmt::Display for GraphQueryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ExactScan(error) => {
                write!(formatter, "graph exact-scan fallback failed: {error}")
            }
        }
    }
}

impl std::error::Error for GraphQueryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ExactScan(error) => Some(error),
        }
    }
}

/// Loads a graph for a query, or loudly returns its typed error with exact results.
///
/// This is the single sanctioned recovery path for a corrupt derived accelerator.
/// No other segment contract is downgraded to best-effort behavior.
pub fn load_graph_or_exact_scan<'a>(
    reader: &'a SegmentReader,
    exact_request: ScanRequest<'_>,
    k: usize,
) -> Result<GraphLoadOutcome<'a>, GraphQueryError> {
    match reader.graph_node_blocks() {
        Ok(graph) => Ok(GraphLoadOutcome::Graph(graph)),
        Err(error) => crate::scan::top_k_with_ties(exact_request, k)
            .map(|candidates| GraphLoadOutcome::ExactScanFallback { error, candidates })
            .map_err(GraphQueryError::ExactScan),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod params_tests {
    use super::{GraphParams, GraphParamsError, MIN_GRAPH_ROWS};

    #[test]
    fn sift_params_and_graph_crossover_are_literal() {
        let params = GraphParams::sift_1m();
        assert_eq!(params.r_target(), 32);
        assert_eq!(params.r_max(), 44);
        assert_eq!(params.alpha_build(), 1.0);
        assert_eq!(params.alpha_refine(), 1.2);
        assert_eq!(params.l_build(), 100);
        assert_eq!(params.checkpoint_batch_rows(), 65_536);
        assert_eq!(
            params
                .with_checkpoint_batch_rows(17)
                .expect("positive checkpoint batch")
                .checkpoint_batch_rows(),
            17
        );
        assert!(!params.should_build(MIN_GRAPH_ROWS - 1));
        assert!(params.should_build(MIN_GRAPH_ROWS));
    }

    #[test]
    fn graph_params_reject_loose_invalid_controls() {
        assert!(GraphParams::new(0, 44, 1.0, 1.2, 100, 1).is_err());
        assert!(GraphParams::new(45, 44, 1.0, 1.2, 100, 1).is_err());
        assert!(GraphParams::new(32, 44, 0.9, 1.2, 100, 1).is_err());
        assert!(GraphParams::new(32, 44, 1.2, 1.1, 100, 1).is_err());
        assert!(GraphParams::new(32, 44, 1.0, 1.2, 31, 1).is_err());
        assert!(GraphParams::new(32, 44, 1.0, 1.2, 100, 0).is_err());

        let errors = [
            GraphParamsError::DegreeOrder {
                r_target: 0,
                r_max: 44,
            },
            GraphParamsError::AlphaOrder {
                alpha_build: 0.9,
                alpha_refine: 1.2,
            },
            GraphParamsError::ConstructionWidth {
                l_build: 31,
                r_target: 32,
            },
            GraphParamsError::CheckpointBatchRows,
        ];
        for error in errors {
            assert!(!error.to_string().is_empty());
        }
    }
}
