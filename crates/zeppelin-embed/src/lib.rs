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
    not(any(
        target_os = "macos",
        all(target_os = "windows", target_arch = "x86_64")
    ))
))]
compile_error!("graph-cypher supports macOS (arm64, x86_64) and Windows x64 only");

#[cfg(feature = "allocation-audit")]
mod allocation_audit;

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod adversarial_test_support;

/// Fixed keyed fixture for integration tests of the Cypher writer seam.
#[cfg(all(feature = "graph-cypher", feature = "test-support"))]
#[doc(hidden)]
pub mod graph_structured_write_test_support {
    use crate::lifecycle::{CancelToken, QueryControl, Store};
    use crate::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
    use crate::property_graph::{
        ApplicationKey, CanonicalContents, EntityId, EntityKind, GraphGeneration, GraphName,
        GraphProperty, GraphRevision, NodeId, PropertyData, PropertyValue,
    };

    /// Creates one application-keyed `:A {v: 1}` through the real writer.
    pub fn create_keyed_node(store: &Store) -> Result<(NodeId, GraphGeneration), String> {
        let mut labels = [GraphName::new("A").map_err(|e| format!("label: {e:?}"))?];
        let mut properties = [GraphProperty::new(
            GraphName::new("v").map_err(|e| format!("property name: {e:?}"))?,
            PropertyValue::new(PropertyData::I64(1))
                .map_err(|e| format!("property value: {e:?}"))?,
        )];
        let image = CanonicalContents::node(&mut labels, &mut properties, None, None)
            .map_err(|e| format!("node image: {e:?}"))?;
        let receipts = store
            .apply_native_graph(
                &[StructuredWrite {
                    key: ApplicationKey::new(EntityKind::Node, "app", "ze214")
                        .map_err(|e| format!("application key: {e:?}"))?,
                    revision: GraphRevision::new(1).map_err(|e| format!("revision: {e:?}"))?,
                    operation: StructuredOperation::Create,
                    image: Some(WriteImage::Node(&image)),
                }],
                &QueryControl::Cancel(CancelToken::new()),
            )
            .map_err(|e| format!("structured create: {e:?}"))?;
        if receipts.len() != 1 {
            return Err("structured create did not return exactly one receipt".to_owned());
        }
        let receipt = receipts
            .first()
            .ok_or_else(|| "structured create returned no receipt".to_owned())?;
        match receipt.entity {
            EntityId::Node(node) => Ok((node, receipt.generation)),
            EntityId::Relationship(_) => {
                Err("structured create returned a relationship".to_owned())
            }
        }
    }
}

#[cfg(all(feature = "graph-cypher", feature = "test-support"))]
#[doc(hidden)]
pub mod graph_native_vector_index_test_support {
    pub use crate::property_graph::storage::search::{
        ActualProbeReport, CleanPreparationObservation, CloseFailureObservation,
        ControlProbeReport, IdentityProbeReport, KernelProbeReport, LimitProbeReport,
        NativeFailureObservation, OracleControlObservation, OracleProbeReport, PhysicalReadReceipt,
        PhysicalReadReport, ReopenIndexObservation, ReopenProbeReport, SmallWriteSourceObservation,
        SmallWritesProbeReport, TraceBatchObservation, TraceProbeReport, TraceSourceObservation,
        run_actual_probe, run_identity_probe, run_kernel_probe, run_oracle_probe,
        run_preparation_schedule_probe, run_reopen_probe, run_small_writes_probe, run_trace_probe,
    };
}

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

#[cfg(all(feature = "graph-cypher", feature = "test-support"))]
#[doc(hidden)]
pub mod graph_identity_test_support {
    use crate::graph_read_view_test_support::PathReceipt;

    /// One observed adjacency row, bound to the node the question named.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct ObservedAdjacency {
        /// Node the expansion was bound to.
        pub bound_node: u128,
        /// Exact relationship type identity.
        pub relationship_type: u64,
        /// Exact relationship identity.
        pub rel: u128,
        /// Other endpoint of that row.
        pub neighbor: u128,
    }

    /// Exact outcome of one keyed request. It holds no expected answer.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct ObservedOutcome {
        /// Key ordinal the request named.
        pub key: u32,
        /// Primitive rejection name, absent when the request was admitted.
        pub rejection: Option<&'static str>,
        /// Installed or replayed identity; zero on a rejection.
        pub entity: u128,
        /// Installed or replayed revision; zero on a rejection.
        pub revision: u64,
        /// Original generation for a replay, changed generation otherwise.
        pub generation: u64,
        /// Exact replay classification.
        pub replayed: bool,
    }

    /// Complete visible rows of one watched expansion after one batch.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct ObservedWatch {
        /// Batch index this answer follows.
        pub batch: usize,
        /// Published generation the reader observed.
        pub generation: u64,
        /// Key ordinal whose current incarnation was bound.
        pub key: u32,
        /// Whether the expansion was outgoing.
        pub outgoing: bool,
        /// Complete rows, in the order the engine returned them.
        pub rows: Vec<ObservedAdjacency>,
    }

    /// Actual state observed from one real keyed identity script.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct IdentityState {
        /// Ordered receipts and rejections of the script.
        pub outcomes: Vec<ObservedOutcome>,
        /// Watched expansions after every batch.
        pub watched: Vec<ObservedWatch>,
        /// Generation held by a reader admitted before the deletion.
        pub retained_generation: u64,
        /// Rows that retained reader still observes after the deletion,
        /// the recreation and a real reclamation cycle.
        pub retained_rows: Vec<ObservedAdjacency>,
        /// Generation observed by a reader admitted after those changes.
        pub fresh_generation: u64,
        /// Rows that newly admitted reader observes.
        pub fresh_rows: Vec<ObservedAdjacency>,
    }

    /// Receipts emitted only after each directed identity body completes.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct IdentityProbeReport {
        /// Completed path receipts with actual fired and clean counts.
        pub receipts: Vec<PathReceipt>,
        /// Actual state observed from one real script.
        pub state: IdentityState,
    }

    /// Runs the directed production identity paths used by ZE-36 acceptance.
    pub fn run_actual_probe(seed: u64) -> IdentityProbeReport {
        crate::lifecycle::native_graph::tests::run_identity_probe(seed)
    }
}

#[cfg(all(feature = "graph-cypher", feature = "test-support"))]
#[doc(hidden)]
pub mod graph_reclaim_test_support {
    use crate::graph_read_view_test_support::{ObservedRelationship, PathReceipt};

    /// Actual state observed after one real reclaim cycle and a reopen. It
    /// holds no expected answer and no comparator.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct ReclaimState {
        /// First created node identity.
        pub first_node: u128,
        /// Second created node identity.
        pub second_node: u128,
        /// OUT rows of the first node after the cycle and the reopen.
        pub relationships: Vec<ObservedRelationship>,
        /// Generation replayed for the original installing request.
        pub replay_generation: u64,
        /// Whether that request was classified as an exact replay.
        pub replayed: bool,
        /// Bytes physically unlinked by the cycle.
        pub removed_bytes: u64,
        /// Summed on-disk lengths of the files the cycle unlinked.
        pub unlinked_file_bytes: u64,
    }

    /// Receipts emitted only after each directed reclamation body completes.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct ReclaimProbeReport {
        /// Completed path receipts with actual fired and clean counts.
        pub receipts: Vec<PathReceipt>,
        /// Actual state after a real cycle.
        pub state: ReclaimState,
    }

    /// Narrow measured reader-race observations and serialized controls.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct RaceProbeReport {
        /// Measured barriers, lazy reads and unlinks.
        pub receipts: Vec<PathReceipt>,
        /// Old generation, canonical bytes, and root/WAL unlink results.
        pub observation: (u64, Vec<u8>, bool, bool),
        /// Identical seed with serialized scheduling.
        pub control: (u64, Vec<u8>, bool, bool),
    }

    /// Runs only ZE-176's same-coordinator reader races.
    pub fn run_ze176_race_probe(seed: u64) -> RaceProbeReport {
        crate::lifecycle::native_graph::tests::run_ze176_race_probe(seed)
    }

    /// Runs the directed production consolidation and reclamation paths used
    /// by ZE-46 acceptance.
    pub fn run_actual_probe(seed: u64) -> ReclaimProbeReport {
        crate::lifecycle::native_graph::tests::run_reclaim_probe(seed)
    }
}

#[cfg(all(feature = "graph-cypher", feature = "test-support"))]
#[doc(hidden)]
pub mod graph_storage_fault_test_support {
    use crate::graph_read_view_test_support::{ObservedRelationship, PathReceipt};
    use crate::lifecycle::{OpenOptions, Store};
    use crate::vfs::Vfs;
    use std::path::Path;
    use std::sync::Arc;

    /// One keyed node observed in actual directory order. It holds no expected
    /// answer and no comparator.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct ObservedKeyedNode {
        /// Application-key bytes exactly as the fixture supplied them.
        pub key: Vec<u8>,
        /// Node identity the store resolved for that key.
        pub node: u128,
        /// Installed revision of that node record.
        pub revision: u64,
    }

    /// Seeded per-class fault selection. The adversarial family owns the
    /// schedule; this crate only executes the schedule it is handed.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct StorageFaultSchedule {
        /// Byte position, modulo the damaged region, of the class-1 bit flip.
        pub artifact_ref_offset: u64,
        /// Upper bound on the keyed nodes class 2 commits while it discovers
        /// which chunk splits the node directory.
        pub split_keys: u32,
        /// Which pre-split key class 2 resolves first through the old root.
        pub split_probe: u32,
        /// How many class-2 artifact creates succeed before the fault fires.
        pub split_skip: u32,
        /// Which class-3 adjacency append fails after the OUT append succeeds.
        pub out_in_append: u32,
        /// Selects the class-4 root-replacement variant order.
        pub root_variant: u8,
    }

    /// Actual state observed by one complete ZE-47 storage-fault execution.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct StorageFaultState {
        /// Typed classification of every class-1 damaged-artifact refusal.
        pub artifact_refusals: Vec<String>,
        /// Nodes observed after each class-1 damage was reverted byte-exactly.
        pub surviving_nodes: Vec<ObservedKeyedNode>,
        /// Actual node-directory root level after the splitting commit
        /// published, observed through a fresh lease.
        pub split_level: u16,
        /// Root level the retained pre-split lease still reports after that
        /// same publication. A pre-split oracle must still read level zero.
        pub retained_split_level: u16,
        /// Keyed nodes committed before the splitting chunk, so the family
        /// can model exactly the population both leases must answer.
        pub pre_split_keys: u32,
        /// Pre-split keys observed through a fresh post-split lease.
        pub committed_keys: Vec<ObservedKeyedNode>,
        /// The same keys resolved through the retained pre-split root.
        pub old_root_keys: Vec<ObservedKeyedNode>,
        /// Published generations observed before and after the class-2 faults
        /// on the splitting commit, each through its own fresh lease.
        pub split_generations: (u64, u64),
        /// Xxh3-64 over the retained pre-split root page, before the faults
        /// and after the splitting chunk published cleanly.
        pub retained_root_digests: (u64, u64),
        /// Generations before and after the WAL-envelope fault on the
        /// splitting commit, the second one read through a reopen because
        /// that refusal stops read admissions.
        pub unsplit_generations: (u64, u64),
        /// Node-directory root level that same reopen exposes. A split whose
        /// WAL envelope never landed must not survive recovery.
        pub unsplit_reopen_level: u16,
        /// Typed classification of every class-2 refusal.
        pub split_refusals: Vec<String>,
        /// OUT rows of the class-3 relationship after a reopen.
        pub out_rows: Vec<ObservedRelationship>,
        /// IN rows of the same relationship after the same reopen.
        pub in_rows: Vec<ObservedRelationship>,
        /// Typed classification of every class-3 half-prepared refusal.
        pub out_in_refusals: Vec<String>,
        /// Typed classification of every class-4 root-replacement refusal.
        pub root_refusals: Vec<String>,
        /// Generation exposed by the class-4 reopen.
        pub reopened_generation: u64,
        /// Generation acknowledged before the class-4 fault.
        pub previous_generation: u64,
    }

    /// Receipts emitted only after each directed storage-fault body completes.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct StorageFaultProbeReport {
        /// Completed path receipts with actual fired and clean counts.
        pub receipts: Vec<PathReceipt>,
        /// Actual state observed by the four fault classes.
        pub state: StorageFaultState,
    }

    /// Runs the four directed ZE-47 native storage fault classes against a
    /// real native store under a faulty VFS, following the supplied schedule.
    pub fn run_actual_probe(seed: u64, schedule: StorageFaultSchedule) -> StorageFaultProbeReport {
        crate::lifecycle::native_graph::tests::run_storage_fault_probe(seed, schedule)
    }

    /// Forwards the crate-private native-store constructor so an external
    /// test crate can place its own [`Vfs`] under a real native store.
    /// `lifecycle::native_graph` is `pub(crate)`, so `NativeGraphError` cannot
    /// cross this seam; the refusal is rendered through its `Display`.
    pub fn create_native_graph_with_infrastructure(
        path: &Path,
        options: OpenOptions,
        vfs: Arc<dyn Vfs>,
    ) -> Result<Store, String> {
        Store::create_native_graph_with_infrastructure(
            path,
            options,
            None,
            vfs,
            Arc::new(crate::lifecycle::SystemMonotonicClock),
            &mut crate::property_graph::storage::allocation::OsEntropy,
        )
        .map_err(|error| error.to_string())
    }

    /// Forwards the crate-private native-store reopen constructor. See
    /// [`create_native_graph_with_infrastructure`] for the error contract.
    pub fn open_native_graph_with_infrastructure(
        path: &Path,
        options: OpenOptions,
        vfs: Arc<dyn Vfs>,
    ) -> Result<Store, String> {
        Store::open_native_graph_with_infrastructure(
            path,
            options,
            None,
            vfs,
            Arc::new(crate::lifecycle::SystemMonotonicClock),
        )
        .map_err(|error| error.to_string())
    }
}

#[cfg(all(feature = "graph-cypher", feature = "test-support"))]
#[doc(hidden)]
pub mod graph_recovery_test_support {
    use crate::graph_read_view_test_support::{ObservedRelationship, PathReceipt};

    /// Actual state observed after reopening one real mixed native commit.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct RecoveryState {
        /// Actual persistent store identity.
        pub store: u128,
        /// Actual reopened graph generation.
        pub generation: u64,
        /// Actual reopened complete-envelope sequence.
        pub sequence: u64,
        /// First created node identity.
        pub first_node: u128,
        /// Second created node identity.
        pub second_node: u128,
        /// Actual relationship row observed in both directions.
        pub relationship: ObservedRelationship,
    }

    /// Receipts emitted only after each directed recovery body completes.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct RecoveryProbeReport {
        /// Completed path receipts with actual fired/clean counts.
        pub receipts: Vec<PathReceipt>,
        /// Actual reopened mixed state.
        pub state: RecoveryState,
    }

    /// Runs the nine directed production recovery paths used by ZE-40 acceptance.
    pub fn run_actual_probe(seed: u64) -> RecoveryProbeReport {
        crate::lifecycle::native_graph::tests::run_recovery_probe(seed)
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
/// Read-only end-to-end store verification.
pub mod verify;
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
        assert_eq!(crate::VERSION, "0.5.0");
    }
}
