//! Typed graph query semantics, separate from lossless replay identity.

mod context;
mod grouping;
mod id_text;
mod list;
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
