//! Embedded hybrid-search engine primitives.
//!
//! # Durability
//!
//! [`lifecycle::durability::DurabilityMode`] defaults to `Derived`. The default
//! issues no synchronization primitive at any commit tier; under it,
//! [`lifecycle::durability::CommitTier`] is ignored entirely. An application
//! crash or process kill does not lose acknowledged writes because the
//! operating-system page cache retains them and writes them out afterward. A
//! power cut or kernel panic can lose recently acknowledged writes. Recovery
//! truncates the log at the first record whose checksum fails, leaving a
//! structurally valid store with a missing recent tail rather than silently
//! wrong data.
//!
//! `Derived` asserts that another store is authoritative and this store can be
//! rebuilt from it. If this store is the only copy of the data, select
//! [`lifecycle::durability::DurabilityMode::Durable`] explicitly.

#![deny(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented,
    clippy::unwrap_used,
    unsafe_op_in_unsafe_fn
)]
#![warn(missing_docs)]

#[cfg(all(
    feature = "graph-cypher",
    not(all(target_os = "macos", target_arch = "aarch64"))
))]
compile_error!("graph-cypher requires macOS arm64; graph packaging requires macOS 14+");

#[cfg(feature = "allocation-audit")]
mod allocation_audit;

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod adversarial_test_support;

#[cfg(all(feature = "graph-cypher", feature = "test-support"))]
#[doc(hidden)]
pub mod graph_read_view_test_support {
    /// One completed production boundary, emitted only after its exact direct
    /// assertion and any paired clean control returned successfully.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct PathReceipt {
        /// Registry key owned by the completed boundary.
        pub key: &'static str,
        /// Actual scheduled failures observed at that boundary.
        pub fires: u64,
        /// Same-seed clean executions observed after or beside the failure.
        pub clean_controls: u64,
    }

    /// Primitive actual relationship observation for the independent ZE-129
    /// oracle crate; this type contains no expected answer or comparator.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct ObservedRelationship {
        pub rel: u128,
        pub source: u128,
        pub target: u128,
        pub relationship_type: u64,
    }

    /// Exact receipts and actual rows from one controlled probe execution.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct ActualProbeReport {
        pub receipts: Vec<PathReceipt>,
        pub relationships: Vec<ObservedRelationship>,
    }
    /// Confirms that the controlled fixture boundary is linked only into a
    /// test-support build. Behavioral acceptance remains on the internal scoped
    /// adapter so no installer or read capability becomes a shipping API.
    pub fn controlled_boundary_is_available() -> bool {
        std::mem::size_of::<crate::lifecycle::native_graph::NativeReadLease>() > 0
            && std::mem::size_of::<crate::property_graph::storage::GraphReadView<'_, '_, '_, '_>>()
                > 0
    }

    /// Runs the real controlled native adapter, fault, lifetime and release paths.
    /// Panics are retained as test failures; the shipping graph feature has no
    /// installer or fixture surface because this module requires test-support.
    pub fn run_actual_probe(seed: u64) -> ActualProbeReport {
        crate::lifecycle::native_graph::tests::run_adversarial_probe(seed)
    }
}

#[cfg(all(feature = "graph-cypher", feature = "test-support"))]
#[doc(hidden)]
pub mod graph_publication_test_support {
    /// Runs only the directed production coordinator paths and returns observed receipts.
    pub fn run_actual_probe(seed: u64) -> crate::graph_read_view_test_support::ActualProbeReport {
        crate::lifecycle::native_graph::tests::publication::run_actual_probe(seed)
    }
}

/// Query diagnostics and health reporting.
pub mod diag;
/// Epoch identity and migration.
pub mod epoch;
/// Persisted-format framing, versions, and golden-fixture support.
pub mod format;
/// Full-text indexing and retrieval.
pub mod fts;
/// Hybrid result fusion.
pub mod fusion;
/// Per-segment vector graphs.
pub mod graph;
/// Ingest and mutation coordination.
pub mod ingest;
/// Runtime-dispatched compute kernels.
pub mod kernels;
/// Store lifecycle and memory accounting.
pub mod lifecycle;
/// Persistent manifest coordination.
pub mod manifest;
/// Columnar metadata and filters.
pub mod meta;
/// Query planning and selectivity decisions.
pub mod planner;
/// Native property-graph identity and values, independent of vector indexes.
#[cfg(feature = "graph-cypher")]
pub mod property_graph;
/// Training-free vector quantization.
pub mod quant;
/// Exact vector scanning.
pub mod scan;
/// Immutable segment representation.
pub mod segment;
/// Operating-system integration wrappers.
pub mod sys;
/// Adaptive storage tiers.
pub mod tier;
/// Virtual filesystem and platform I/O.
pub mod vfs;
/// Write-ahead logging and durability.
pub mod wal;

#[cfg(test)]
mod test_support;

/// The current crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use rand::RngCore;

    #[test]
    #[cfg_attr(miri, ignore = "environment-dependent RNG is outside the Miri subset")]
    fn seeded_rng_is_deterministic_per_name_and_env() {
        let mut first =
            crate::test_support::seeded_rng("tests::seeded_rng_is_deterministic_per_name_and_env");
        let mut second =
            crate::test_support::seeded_rng("tests::seeded_rng_is_deterministic_per_name_and_env");
        let mut other_name = crate::test_support::seeded_rng("tests::another_test");

        let first_draw = first.next_u64();
        let second_draw = second.next_u64();
        let other_name_draw = other_name.next_u64();

        let environment_seed = std::env::var("ZE_TEST_SEED").unwrap_or_else(|_| String::from("0"));
        let other_environment_seed = format!("{environment_seed}-different");
        let mut other_env = crate::test_support::seeded_rng_from(
            "tests::seeded_rng_is_deterministic_per_name_and_env",
            &other_environment_seed,
        );
        let other_env_draw = other_env.next_u64();

        assert_eq!(first_draw, second_draw);
        assert_ne!(first_draw, other_name_draw);
        assert_ne!(first_draw, other_env_draw);
    }

    #[test]
    fn version_constant_is_current() {
        assert_eq!(crate::VERSION, "0.4.2");
    }
}
