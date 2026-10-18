//! Errors from native graph retrieval through store search.

use crate::property_graph::query::QueryError;
use crate::property_graph::query::runtime::RuntimeError;
use crate::property_graph::storage::tree::directory::TreeError;
#[allow(
    dead_code,
    reason = "error payloads are retained for Debug diagnostics"
)]
#[derive(Debug)]
pub(crate) enum RetrievalError {
    Storage(TreeError),
    Control(RuntimeError),
    Eligibility(QueryError),
    NoVectorSpace,
    Dimension {
        expected: usize,
        actual: usize,
    },
    Memory,
    /// The selected method must retain more candidates than the caller allowed.
    CandidateWindow {
        required: usize,
        window: usize,
    },
    /// Store fusion policy v1 rejected a component or anchor.
    Fusion(crate::fusion::FusionError),
    /// A retrieval contract was violated by admitted state.
    Invariant(&'static str),
}

impl From<TreeError> for RetrievalError {
    fn from(error: TreeError) -> Self {
        Self::Storage(error)
    }
}
