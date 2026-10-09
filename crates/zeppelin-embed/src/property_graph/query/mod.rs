//! Typed graph query semantics, separate from lossless replay identity.

mod context;
#[cfg(feature = "graph-cypher")]
pub(crate) mod expression;
mod grouping;
mod id_text;
mod list;
#[cfg(feature = "graph-cypher")]
mod pattern;

#[cfg(all(feature = "graph-cypher", feature = "test-seams"))]
/// Tooling-only native pattern directed probes.
pub mod pattern_test_support {
    pub use super::pattern::test_support::observe_expand_scratch;
    pub use super::pattern::test_support::{
        ProbeReport, observe_document_visits, run_actual_probe, with_original_node_sources,
    };
}

#[cfg(all(feature = "graph-cypher", any(test, feature = "test-seams")))]
pub(crate) use pattern::test_support::{note_document_visit, note_expand_scratch};

#[cfg(all(feature = "graph-cypher", feature = "test-seams"))]
/// Tooling-only native relational directed probes.
pub mod native_relational_test_support {
    pub use super::pattern::relational::test_support::{
        NativeRelationalProbeReport, run_actual_probe, seed_capacity_store,
    };
    pub(crate) use super::pattern::relational::test_support::{
        capacity_fixture_active, capacity_fixture_work,
    };
}
/// Typed DAG validation, separate from execution and admission.
pub mod plan;
mod property;
mod scalar;
mod value;
pub use context::{
    MAX_QUERY_BYTES, MAX_VALUE_WORK, QueryNodeRef, QueryRelRef, QueryView, ValueContext,
};
pub use list::{MAX_LIST_DEPTH, MAX_LIST_ELEMENTS, QueryList};
pub use property::{PropertyAssignment, PropertyScratch};
pub use scalar::StringPredicate;

pub use value::{Arithmetic, Comparison, QueryValue, Truth};

/// Explicit query rejection; no error carries a partial result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryError {
    /// The value has a kind unsupported by the requested expression.
    Type,
    /// Property payload cannot fit the complete graph input bound.
    PropertyLimit,
    /// Caller-reserved typed output is too small.
    BufferTooSmall,
    /// An entity-bearing value belongs to another admission token.
    ForeignView,
    /// Work must be reserved before consuming another value/chunk unit.
    WorkLimit,
    /// Nested list geometry exceeds the shared element/depth/byte limits.
    ListLimit,
    /// One borrowed string exceeds the complete query byte envelope.
    ValueTooLarge,
    /// Existing caller cancellation stopped the operation.
    Cancelled,
    /// The retained lifecycle view was cancelled by close.
    ReadCancelled,
    /// Existing absolute deadline expired.
    Timeout,
    /// Unexpected failure from the mandatory control interface.
    Control,
    /// Checked integer or finite floating arithmetic overflowed.
    ArithmeticOverflow,
    /// An arithmetic operand is nonfinite.
    ArithmeticDomain,
    /// Integer or floating arithmetic divided by zero.
    DivisionByZero,
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ArithmeticOverflow => formatter.write_str("graph arithmetic overflow"),
            Self::ArithmeticDomain => formatter.write_str("nonfinite graph arithmetic operand"),
            Self::DivisionByZero => formatter.write_str("graph arithmetic division by zero"),
            Self::WorkLimit => formatter.write_str("graph value work limit"),
            Self::ListLimit => formatter.write_str("graph query list limit"),
            Self::ValueTooLarge => formatter.write_str("graph query value exceeds byte envelope"),
            Self::Cancelled => formatter.write_str("graph query cancelled"),
            Self::ReadCancelled => formatter.write_str("store close cancelled graph query"),
            Self::Timeout => formatter.write_str("graph query timed out"),
            Self::Control => formatter.write_str("graph query control failed"),
            Self::ForeignView => formatter.write_str("graph value belongs to another query view"),
            Self::PropertyLimit => formatter.write_str("graph property payload limit"),
            Self::BufferTooSmall => formatter.write_str("graph query output buffer too small"),
            Self::Type => formatter.write_str("graph query value has the wrong type"),
        }
    }
}

impl std::error::Error for QueryError {}

/// Query-local capacities charged to the same store accounting authority.
pub mod resources;

/// Bounded flat execution interfaces and cumulative query work.
pub mod runtime;

/// Immutable same-view graph eligibility with owned packed full-width IDs.
pub mod eligibility;
/// Bounded relational kernels over explicit pre-evaluated typed columns.
pub mod relational;

/// Owned typed completed-result storage, independent of query and store lifetime.
pub mod completed;

#[cfg(all(feature = "graph-cypher", feature = "test-seams"))]
/// Tooling-only native completed-result directed probes.
pub mod native_result_test_support {
    pub use super::completed::native::test_support::{ProbeReport, run_actual_probe};
}

#[cfg(all(feature = "graph-cypher", feature = "test-seams"))]
/// Tooling-only directed probes of the structured execution seam's write path.
pub mod query_entry_test_support {
    pub use super::completed::native::entry_probe::{ProbeReport, run_actual_probe};
}

#[cfg(all(test, feature = "graph-cypher"))]
pub(crate) use completed::native::entry_probe;
