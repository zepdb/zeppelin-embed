//! Sparse retrieval membership, immutable sources, and checked descriptors.

mod checkpoint;
mod codec;
mod prepare;
mod trace;
mod vector_index;
mod view;

pub(crate) use checkpoint::{
    PreparedSparseCheckpoint, SparseCheckpoint, prepare_sparse_checkpoint,
    prepare_sparse_maintenance, validate_checkpoint, validate_persisted_maintenance_transition,
    validate_persisted_replay_transition, validate_replay_transition,
};
pub(crate) use codec::{Modality, SparseRoots};
#[cfg(any(test, feature = "test-support"))]
pub(crate) use prepare::miss_next_maintenance_peer_retarget;
pub(crate) use prepare::{PreparedMembershipChange, PreparedSparseCandidate, prepare_sparse};
pub(crate) use trace::{
    SearchTraceCursor, SearchTraceResult, SearchTraceState, SparseTraceRecordFacts,
    verify_sparse_trace_record,
};
#[cfg(any(test, feature = "test-support"))]
pub(crate) use vector_index::test_support::limits as native_vector_index_test_limits;
#[cfg(all(feature = "graph-cypher", feature = "test-support"))]
pub use vector_index::test_support::{
    ActualProbeReport, CleanPreparationObservation, CloseFailureObservation, ControlProbeReport,
    IdentityProbeReport, KernelProbeReport, LimitProbeReport, NativeFailureObservation,
    OracleControlObservation, OracleProbeReport, PhysicalReadReceipt, PhysicalReadReport,
    ReopenIndexObservation, ReopenProbeReport, SmallWriteSourceObservation, SmallWritesProbeReport,
    TraceBatchObservation, TraceProbeReport, TraceSourceObservation, run_actual_probe,
    run_identity_probe, run_kernel_probe, run_oracle_probe, run_preparation_schedule_probe,
    run_reopen_probe, run_small_writes_probe, run_trace_probe,
};
#[cfg(any(test, feature = "test-support"))]
pub(crate) use vector_index::test_support::{
    NativePrepareStage as NativeVectorValidationStage,
    validation_phase as native_vector_validation_phase,
};
#[cfg(any(test, feature = "test-support"))]
pub(crate) use vector_index::test_support::{
    PhysicalReadOrigin, observe_physical_read as observe_native_vector_physical_read,
};
pub(crate) use vector_index::{
    NativeVectorIndex, validate_vector_index_row, validate_vector_index_rows_with,
};
#[cfg(any(test, feature = "test-support"))]
pub(crate) use view::SparsePhysicalSnapshot;
pub(crate) use view::{SparseMember, SparseSource, SparseSources, SparseView};

#[cfg(any(test, feature = "test-support"))]
mod tests;
