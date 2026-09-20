//! Sparse retrieval membership, immutable sources, and checked descriptors.

mod checkpoint;
mod codec;
mod prepare;
mod trace;
mod view;

pub(crate) use checkpoint::{
    SparseCheckpoint, prepare_sparse_checkpoint, validate_checkpoint,
    validate_persisted_replay_transition, validate_replay_transition,
};
pub(crate) use codec::{Modality, SparseRoots};
pub(crate) use prepare::{PreparedMembershipChange, PreparedSparseCandidate, prepare_sparse};
pub(crate) use trace::{SearchTraceCursor, SearchTraceResult};
pub(crate) use view::{SparseMember, SparseSource, SparseSources, SparseView};

#[cfg(test)]
mod tests;
