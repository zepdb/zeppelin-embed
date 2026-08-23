//! Per-segment vector graphs.

/// Persisted fixed-stride graph node blocks.
pub mod block;

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
        /// Complete exact-scan result; the failure never reduces result count silently.
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
        Err(error) => crate::scan::top_k(exact_request, k)
            .map(|candidates| GraphLoadOutcome::ExactScanFallback { error, candidates })
            .map_err(GraphQueryError::ExactScan),
    }
}
