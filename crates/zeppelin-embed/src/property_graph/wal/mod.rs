//! Complete native graph WAL framing and private replay validation.
//!
//! This participant does not publish a graph, mutate files, or authorize cleanup.

use super::storage::artifact::{ArtifactId, PhysicalRef};
use super::{GraphGeneration, OperationFields, StoreInstanceId};

/// Maximum complete encoded Begin/Change/Commit envelope.
pub const MAX_ENVELOPE_BYTES: usize = 16 * 1024 * 1024;
/// Conservative fixed codec/replay stack reservation; backing stays caller-owned.
pub const STACK_RESERVATION_BYTES: usize = 64 * 1024;
/// Graph WAL file header width.
pub const HEADER_BYTES: usize = 64;

/// Typed framing, resource, or participant validation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalError {
    /// Required family, role, operation, or version is unsupported.
    Unsupported,
    /// Complete framing is malformed.
    Malformed,
    /// A persisted checksum or aggregate digest differs.
    Checksum,
    /// Full store identity differs.
    Store,
    /// Sequence, batch identity, frame order, or generation chain differs.
    Sequence,
    /// Retained logical or physical allocation high-water regressed.
    HighWater,
    /// Input/output or fixed scratch reservation exceeds an admitted bound.
    Capacity,
    /// A caller's work allowance is exhausted.
    WorkLimit,
    /// Cancellation was observed during bounded work.
    Cancelled,
    /// A required committed artifact or logical extent is missing.
    MissingArtifact,
    /// Required participant semantics or proof validation failed.
    Participant,
    /// Replay has already failed; no later record may be returned.
    Failed,
}
impl std::fmt::Display for WalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "graph WAL: {self:?}")
    }
}
impl std::error::Error for WalError {}

/// Work/cancellation context shared with required replay participants.
pub struct WalResources<'a> {
    remaining: u64,
    consumed: u64,
    accounting: Option<super::resources::GraphResources>,
    cancelled: &'a mut dyn FnMut() -> bool,
}
impl<'a> WalResources<'a> {
    /// Reserves codec stack from an already-held caller allowance. Borrowed byte
    /// buffers and descriptor capacities remain charged once to their owner.
    pub fn new(
        work: u64,
        stack_allowance: usize,
        cancelled: &'a mut dyn FnMut() -> bool,
    ) -> Result<Self, WalError> {
        if stack_allowance < STACK_RESERVATION_BYTES {
            return Err(WalError::Capacity);
        }
        let mut value = Self {
            remaining: work,
            consumed: 0,
            accounting: None,
            cancelled,
        };
        value.charge(0)?;
        Ok(value)
    }
    /// Attaches the actual native writer owner; standalone codecs keep their
    /// explicit local consumed counter without claiming store-bound work.
    pub(crate) fn with_accounting(mut self, resources: &super::resources::GraphResources) -> Self {
        self.accounting = Some(resources.clone());
        self
    }
    /// Checks cancellation and charges before the next byte/descriptor unit.
    pub fn charge(&mut self, units: u64) -> Result<(), WalError> {
        if (self.cancelled)() {
            return Err(WalError::Cancelled);
        }
        self.remaining = self
            .remaining
            .checked_sub(units)
            .ok_or(WalError::WorkLimit)?;
        self.consumed = self
            .consumed
            .checked_add(units)
            .ok_or(WalError::WorkLimit)?;
        if let Some(accounting) = &self.accounting {
            accounting.record_work(crate::lifecycle::stats::GraphWorkKind::WalCodecUnits, units);
        }
        Ok(())
    }
    pub(crate) const fn remaining(&self) -> u64 {
        self.remaining
    }

    /// Exact units successfully admitted by this context.
    pub const fn consumed(&self) -> u64 {
        self.consumed
    }
}

/// Nonzero full-width identity binding all frames of one envelope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BatchId(u128);
impl BatchId {
    /// Checks a decoded or privately allocated identity.
    pub fn new(value: u128) -> Result<Self, WalError> {
        if value == 0 {
            Err(WalError::Malformed)
        } else {
            Ok(Self(value))
        }
    }
    /// All identity bits.
    pub const fn get(self) -> u128 {
        self.0
    }
}
/// Logical envelope class; reclaim records never masquerade as entity edits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnvelopeKind {
    /// Normalized entity changes.
    Mutation,
    /// Physical replacement, inventory, or reclamation bookkeeping.
    Maintenance,
}
/// Complete immutable object descriptor, independent of reachability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtifactDescriptor {
    /// Persisted storage incarnation.
    pub store: StoreInstanceId,
    /// Physical nonce.
    pub artifact: ArtifactId,
    /// Object creation generation.
    pub generation: GraphGeneration,
    /// Monotone allocation serial.
    pub serial: u64,
    /// Complete immutable file length.
    pub bytes: u32,
    /// Required container family.
    pub family: u16,
    /// Required container version.
    pub version: u16,
    /// Whole-file trailer checksum.
    pub checksum: u64,
}
/// Required-live framed block, with complete owning-object identity/checksum.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RequiredRef {
    /// Owning immutable object.
    pub object: ArtifactDescriptor,
    /// Exact entire framed block within that object.
    pub block: PhysicalRef,
}
/// Fixed ordered wire carrier. ZE43 owns semantic GraphRoots and tree access.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WalGraphRoots {
    /// Nodes, relationships, fences, labels, types, OUT, IN, inventory (slots0..7).
    pub slots: [Option<RequiredRef>; 8],
}
/// Inclusive retained high-waters; zero means never allocated.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HighWaters {
    /// Largest consumed NodeId.
    pub node: u128,
    /// Largest consumed RelId.
    pub relationship: u128,
    /// Label, relationship-type, property, namespace symbol high-waters.
    pub symbols: [u64; 4],
    /// Largest consumed physical creation serial.
    pub creation_serial: u64,
}
/// Borrowed required-reference list; decoding retains wire bytes without copying.
#[derive(Clone, Copy, Debug)]
pub enum ReferenceList<'a> {
    /// Caller-owned typed descriptors.
    Values(&'a [RequiredRef]),
    /// Validated wire descriptors, internal decoder form.
    Encoded(&'a [u8]),
}
/// Borrowed complete-artifact descriptors; membership alone grants no liveness.
#[derive(Clone, Copy, Debug)]
pub enum DescriptorList<'a> {
    /// Caller-owned descriptors.
    Values(&'a [ArtifactDescriptor]),
    /// Validated wire descriptors.
    Encoded(&'a [u8]),
}
/// Complete admitted base or committed state; no participant is inferred.
#[derive(Clone, Copy, Debug)]
pub struct CommitState<'a> {
    /// Storage-owned identity, unchanged across replay.
    pub store: StoreInstanceId,
    /// Coherent graph/search cutoff.
    pub generation: GraphGeneration,
    /// Last complete committed envelope sequence.
    pub sequence: u64,
    /// All eight graph root positions.
    pub graph: WalGraphRoots,
    /// Required graph catalog participant.
    pub catalog: RequiredRef,
    /// Optional vector retrieval participant.
    pub vector: Option<RequiredRef>,
    /// Optional lexical retrieval participant.
    pub text: Option<RequiredRef>,
    /// Optional retained reclaim-state/proof participant.
    pub reclaim: Option<RequiredRef>,
    /// Every logical and physical allocator high-water.
    pub high_waters: HighWaters,
    /// Durable prepared-inventory participants required to reconstruct this commit.
    pub prepared_inventories: ReferenceList<'a>,
}
/// Explicit before/after optional node membership; relations have no membership.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Membership {
    /// Text membership before the normalized change.
    pub text_before: bool,
    /// Text membership after the change.
    pub text_after: bool,
    /// Vector membership before the change.
    pub vector_before: bool,
    /// Vector membership after the change.
    pub vector_after: bool,
}
/// Full normalized durable entity outcome.
#[derive(Clone, Copy, Debug)]
pub struct Mutation<'a> {
    /// Explicit supported provenance version; no inferred default.
    pub provenance_version: u16,
    /// Every identity-owned installing-operation field.
    pub provenance: OperationFields<'a>,
    /// True for a live installed record, false for a deletion fence/tombstone.
    pub live: bool,
    /// Required lossless image or its complete extent-list root for a live record.
    pub canonical: Option<RequiredRef>,
    /// Search membership transition, independent of node existence.
    pub membership: Membership,
}
/// Inventory state, distinct from a live-reference declaration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InventoryState {
    /// Newly prepared allocation.
    Prepared,
    /// Retained allocation, not necessarily reachable.
    Retained,
    /// Candidate of the named committed intent.
    ReclaimPending(BatchId),
    /// Completed candidate of the named intent.
    Reclaimed(BatchId),
}
/// One allocation/state transition.
#[derive(Clone, Copy, Debug)]
pub struct InventoryChange {
    /// Complete object identity.
    pub object: ArtifactDescriptor,
    /// Intended inventory state.
    pub state: InventoryState,
}
/// Durable completed-mark claim; semantic proof validation remains mandatory.
#[derive(Clone, Copy, Debug)]
pub struct ReclaimIntent<'a> {
    /// Unique full-width intent identity.
    pub id: BatchId,
    /// Captured graph generation.
    pub capture_generation: GraphGeneration,
    /// Captured complete-envelope sequence.
    pub capture_sequence: u64,
    /// Allocation serial fence captured atomically with protected roots.
    pub serial_fence: u64,
    /// Required immutable protected-root-set participant.
    pub protected_roots: RequiredRef,
    /// Digest binding the complete protected set.
    pub protected_digest: u64,
    /// Required immutable completed-mark manifest participant.
    pub completed_mark: RequiredRef,
    /// Digest binding mark runs/counts/checksums, validated by its owner.
    pub mark_digest: u64,
    /// Validated deletion targets, never required-live references.
    pub candidates: DescriptorList<'a>,
}
/// Durable completion claim; no unlink is executed or authorized by this codec.
#[derive(Clone, Copy, Debug)]
pub struct ReclaimComplete<'a> {
    /// Original committed intent.
    pub id: BatchId,
    /// Required retained original intent/proof participant.
    pub intent: RequiredRef,
    /// Exact candidate subset unlinked and directory-synced before this commit.
    pub completed: DescriptorList<'a>,
    /// Exact candidate subset still pending.
    pub remaining: DescriptorList<'a>,
}
/// Ordered normalized change. Only whole envelopes escape replay validation.
#[derive(Clone, Copy, Debug)]
pub enum Change<'a> {
    /// Entity mutation.
    Mutation(Mutation<'a>),
    /// Allocation/state record.
    Inventory(InventoryChange),
    /// New complete reclamation proof claim.
    ReclaimIntent(ReclaimIntent<'a>),
    /// Durable reclamation progress claim.
    ReclaimComplete(ReclaimComplete<'a>),
}
/// Private complete envelope supplied by the sole future coordinator.
#[derive(Clone, Copy, Debug)]
pub struct Envelope<'a> {
    /// Full envelope identity.
    pub batch: BatchId,
    /// Mutation versus maintenance.
    pub kind: EnvelopeKind,
    /// Ordered complete normalized frames.
    pub changes: &'a [Change<'a>],
    /// Complete target state, including all roots and high-waters.
    pub state: CommitState<'a>,
}
mod codec;
mod framing;
pub(crate) use framing::envelope_size;
pub use framing::{encode_envelope, encode_header};
mod checkpoint;
pub(crate) use checkpoint::{NativeCheckpoint, decode_checkpoint, encode_checkpoint};
pub(crate) use replay::{FramedCaptureStep, same_commit_state};
/// Metadata-only binding reused by the native storage participant. The retained
/// artifact owner still admits complete bytes/checksum and the coherent lease.
pub(crate) fn validate_graph_root_reference(
    state: CommitState<'_>,
    reference: RequiredRef,
) -> Result<(), WalError> {
    framing::validate_ref(
        reference,
        state,
        super::storage::artifact::BlockKind::TreePage,
    )?;
    codec::validate_reference_geometry(reference)
}
mod replay;
pub use replay::{
    ChangeReader, ParticipantRole, Replay, ReplayEnd, ReplayStep, ReplayValidator, RequiredRole,
    ValidatedEnvelope,
};

mod change;
mod maintenance;
pub use replay::validate_required_block;

#[cfg(all(test, feature = "allocation-audit"))]
mod allocation_tests;
