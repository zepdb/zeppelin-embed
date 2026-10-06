use super::*;
use crate::property_graph::storage::tree::TreeKind;

/// Required participant payload interpretation, in the fixed ZGCP role namespace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum ParticipantRole {
    /// Complete graph interpretation/catalog participant.
    Catalog = 1,
    /// Exact immutable prepared object inventory.
    PreparedInventory = 2,
    /// Complete captured protected root set.
    ProtectedRoots = 3,
    /// Completed mark manifest with validated runs/counts/checksums.
    CompletedMark = 4,
    /// Retained pending intent/proof state.
    ReclaimState = 5,
    /// Vector or text base/delta state, interpreted by the search owner.
    RetrievalState = 6,
}
/// Required object role; semantic validators cannot reinterpret the same block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequiredRole {
    /// Exact tree comparator domain at a graph-root position.
    Tree(TreeKind),
    /// Required role/version-tagged commit participant.
    Participant(ParticipantRole),
    /// Complete lossless canonical image, following every indexed extent.
    Canonical,
}
/// Read-only participant validation, mandatory before a complete batch escapes.
///
/// There are no success defaults. Implementations must resolve full required
/// objects/extents, validate exact descriptors and role semantics, check complete
/// inventory/reclaim correlations, and propagate work/cancellation through `r`.
/// These hooks never apply changes, unlink files, or grant cleanup authority.
pub trait ReplayValidator {
    /// Resolve and validate required object bytes and every descendant extent.
    fn required(
        &mut self,
        reference: RequiredRef,
        role: RequiredRole,
        r: &mut WalResources<'_>,
    ) -> Result<(), WalError>;
    /// Validate the normalized mutation against the admitted base and other frames.
    fn mutation(
        &mut self,
        mutation: Mutation<'_>,
        r: &mut WalResources<'_>,
    ) -> Result<(), WalError>;
    /// Validate allocation/state correlation with the complete prepared inventory.
    fn inventory(
        &mut self,
        inventory: InventoryChange,
        r: &mut WalResources<'_>,
    ) -> Result<(), WalError>;
    /// Validate completed protected-root/mark/candidate proof, with no deletion.
    fn reclaim_intent(
        &mut self,
        intent: ReclaimIntent<'_>,
        r: &mut WalResources<'_>,
    ) -> Result<(), WalError>;
    /// Validate exact completed/remaining subsets and original committed proof.
    fn reclaim_complete(
        &mut self,
        complete: ReclaimComplete<'_>,
        r: &mut WalResources<'_>,
    ) -> Result<(), WalError>;
    /// Validate coherent catalog/search/root and inventory/proof state together.
    fn state(
        &mut self,
        kind: EnvelopeKind,
        base: CommitState<'_>,
        target: CommitState<'_>,
        changes: ChangeReader<'_>,
        r: &mut WalResources<'_>,
    ) -> Result<(), WalError>;
}
/// Complete batch returned only after frame and all participant checks succeed.
/// It belongs to private recovery state; a later log error forbids publication.
#[derive(Debug)]
pub struct ValidatedEnvelope<'a> {
    change_bytes: &'a [u8],
    change_count: u32,
    /// Complete envelope identity.
    pub batch: BatchId,
    /// Logical batch class.
    pub kind: EnvelopeKind,
    /// Complete coherent committed state.
    pub state: CommitState<'a>,
}
/// End of a valid log or proved incomplete terminal append.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplayEnd {
    /// File prefix ending immediately after the last complete envelope.
    pub complete_bytes: usize,
    /// Remaining bytes are a validated prefix of an uncommitted append.
    pub incomplete_tail: bool,
}
/// One complete private batch or terminal classification, never a partial change.
/// Fixed descriptors remain inline inside the declared stack reservation.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum ReplayStep<'a> {
    /// Fully validated coherent envelope.
    Envelope(ValidatedEnvelope<'a>),
    /// Terminal state; read-only replay has changed no input bytes.
    End(ReplayEnd),
}

/// Framing-only immutable capture evidence. This proves one complete encoded
/// envelope boundary and exposes its typed changes for protected-root tracing;
/// it is not semantic replay admission or cleanup authority.
pub(crate) struct FramedCaptureEnvelope<'a> {
    change_bytes: &'a [u8],
    change_count: u32,
    batch: BatchId,
    pub(crate) kind: EnvelopeKind,
    pub(crate) state: CommitState<'a>,
    pub(crate) complete_bytes: usize,
}

#[allow(
    clippy::large_enum_variant,
    reason = "framed WAL capture preserves inline terminal evidence"
)]
pub(crate) enum FramedCaptureStep<'a> {
    Envelope(FramedCaptureEnvelope<'a>),
    #[allow(
        dead_code,
        reason = "retain typed terminal WAL evidence beside the framed envelope"
    )]
    End(ReplayEnd),
}
/// Latched private recovery scanner borrowing immutable WAL and checkpoint state.
pub struct Replay<'a> {
    bytes: &'a [u8],
    state: CommitState<'a>,
    offset: usize,
    failed: bool,
    end: Option<ReplayEnd>,
}

/// Checks exactly one headerless envelope without admitting artifacts or graph
/// state. Unified WAL payloads carry these bytes inside the family-11 record.
pub(crate) fn validate_envelope_framing(
    bytes: &[u8],
    r: &mut WalResources<'_>,
) -> Result<(), WalError> {
    use super::codec::Reader;
    use super::framing::{read_record, state_read};
    if bytes.len() > MAX_ENVELOPE_BYTES {
        return Err(WalError::Capacity);
    }
    let sequence = u64::from_le_bytes(
        bytes
            .get(16..24)
            .ok_or(WalError::Malformed)?
            .try_into()
            .map_err(|_| WalError::Malformed)?,
    );
    let begin = read_record(bytes, 0, None, sequence, Some(1), r)?.ok_or(WalError::Malformed)?;
    let mut rd = Reader {
        bytes: begin.payload,
        pos: 0,
    };
    let store = StoreInstanceId::new(rd.u128(r)?).map_err(|_| WalError::Store)?;
    rd.take(8, r)?;
    let generation = GraphGeneration::new(rd.u64(r)?);
    rd.u64(r)?;
    let count = rd.u32(r)?;
    rd.zero(4, r)?;
    if rd.u64(r)? != bytes.len() as u64 || u64::from(count) * 72 > bytes.len() as u64 {
        return Err(WalError::Malformed);
    }
    rd.end()?;
    let mut offset = begin.bytes;
    for index in 1..=count {
        let change = read_record(
            bytes.get(offset..).ok_or(WalError::Malformed)?,
            index,
            Some(begin.batch),
            sequence,
            None,
            r,
        )?
        .ok_or(WalError::Malformed)?;
        offset = offset
            .checked_add(change.bytes)
            .ok_or(WalError::Malformed)?;
    }
    let commit = read_record(
        bytes.get(offset..).ok_or(WalError::Malformed)?,
        count.checked_add(1).ok_or(WalError::Malformed)?,
        Some(begin.batch),
        sequence,
        Some(6),
        r,
    )?
    .ok_or(WalError::Malformed)?;
    let mut rd = Reader {
        bytes: commit.payload,
        pos: 16,
    };
    let state = state_read(&mut rd, r)?;
    rd.end()?;
    let mut replay = Replay {
        bytes,
        state: CommitState {
            store,
            generation,
            sequence: sequence.checked_sub(1).ok_or(WalError::Sequence)?,
            high_waters: HighWaters::default(),
            ..state
        },
        offset: 0,
        failed: false,
        end: None,
    };
    match replay.next_inner(None, r)? {
        ReplayStep::Envelope(_) if replay.offset == bytes.len() => Ok(()),
        _ => Err(WalError::Malformed),
    }
}

impl<'a> Replay<'a> {
    /// Authenticates the selected WAL header and returns its declared first sequence.
    pub(crate) fn checked_first_sequence(
        bytes: &[u8],
        store: StoreInstanceId,
        r: &mut WalResources<'_>,
    ) -> Result<u64, WalError> {
        read_header(bytes, store, r)
    }

    /// Locates the exact complete envelope boundary for a retained checkpoint
    /// state while validating every scalar frame in the historical prefix.
    pub(crate) fn checked_checkpoint_watermark(
        bytes: &'a [u8],
        checkpoint: CommitState<'a>,
        r: &mut WalResources<'_>,
    ) -> Result<usize, WalError> {
        use super::codec::*;
        let first = read_header(bytes, checkpoint.store, r)?;
        if first
            == checkpoint
                .sequence
                .checked_add(1)
                .ok_or(WalError::Sequence)?
        {
            return Ok(HEADER_BYTES);
        }
        if first > checkpoint.sequence {
            return Err(WalError::Sequence);
        }
        let begin = super::framing::read_record(
            bytes.get(HEADER_BYTES..).ok_or(WalError::Malformed)?,
            0,
            None,
            first,
            Some(1),
            r,
        )?
        .ok_or(WalError::Malformed)?;
        let mut rd = Reader {
            bytes: begin.payload,
            pos: 24,
        };
        let generation = GraphGeneration::new(rd.u64(r)?);
        let seed = CommitState {
            generation,
            sequence: first.checked_sub(1).ok_or(WalError::Sequence)?,
            high_waters: HighWaters::default(),
            graph: WalGraphRoots::default(),
            vector: None,
            text: None,
            reclaim: None,
            prepared_inventories: ReferenceList::Values(&[]),
            ..checkpoint
        };
        let mut replay = Self {
            bytes,
            state: seed,
            offset: HEADER_BYTES,
            failed: false,
            end: None,
        };
        loop {
            match replay.next_inner(None, r)? {
                ReplayStep::Envelope(_) if replay.state.sequence < checkpoint.sequence => {}
                ReplayStep::Envelope(_) if replay.state.sequence == checkpoint.sequence => {
                    if !same_commit_state(replay.state, checkpoint, r)? {
                        return Err(WalError::Participant);
                    }
                    return Ok(replay.offset);
                }
                ReplayStep::Envelope(_) => return Err(WalError::Sequence),
                ReplayStep::End(_) => return Err(WalError::Sequence),
            }
        }
    }

    /// Checks the exact complete-envelope byte watermark of an admitted
    /// checkpoint, including any still-present historical WAL prefix.
    pub fn at_watermark(
        bytes: &'a [u8],
        checkpoint: CommitState<'a>,
        watermark: usize,
        r: &mut WalResources<'_>,
    ) -> Result<Self, WalError> {
        use super::codec::*;
        if watermark == HEADER_BYTES {
            return Self::new(bytes, checkpoint, r);
        }
        if watermark < HEADER_BYTES || watermark > bytes.len() {
            return Err(WalError::Malformed);
        }
        let first = read_header(bytes, checkpoint.store, r)?;
        if first > checkpoint.sequence {
            return Err(WalError::Sequence);
        }
        super::framing::state_write(
            checkpoint,
            &mut Writer {
                bytes: None,
                pos: 0,
            },
            r,
        )?;
        let begin = super::framing::read_record(
            bytes
                .get(HEADER_BYTES..watermark)
                .ok_or(WalError::Malformed)?,
            0,
            None,
            first,
            Some(1),
            r,
        )?
        .ok_or(WalError::Malformed)?;
        let mut rd = Reader {
            bytes: begin.payload,
            pos: 24,
        };
        let generation = GraphGeneration::new(rd.u64(r)?);
        // Before the first retained Commit only the Begin base generation/sequence
        // is available. Zero lower bounds seed scalar monotonicity, not an admitted
        // graph or inferred checkpoint. This private state is never returned.
        let seed = CommitState {
            generation,
            sequence: first.checked_sub(1).ok_or(WalError::Sequence)?,
            high_waters: HighWaters::default(),
            graph: WalGraphRoots::default(),
            vector: None,
            text: None,
            reclaim: None,
            prepared_inventories: ReferenceList::Values(&[]),
            ..checkpoint
        };
        let mut replay = Self {
            bytes,
            state: seed,
            offset: HEADER_BYTES,
            failed: false,
            end: None,
        };
        while replay.offset < watermark {
            // Retired historical objects need not exist. The complete scalar
            // framing is still checked, and no historical batch escapes this scan.
            if !matches!(replay.next_inner(None, r)?, ReplayStep::Envelope(_)) {
                return Err(WalError::Malformed);
            }
        }
        if replay.offset != watermark || !same_commit_state(replay.state, checkpoint, r)? {
            return Err(WalError::Participant);
        }
        replay.state = checkpoint;
        Ok(replay)
    }

    /// Admits a complete checkpoint, including every retained allocation high-water.
    pub fn new(
        bytes: &'a [u8],
        state: CommitState<'a>,
        r: &mut WalResources<'_>,
    ) -> Result<Self, WalError> {
        use super::codec::*;
        if state.sequence.checked_add(1) != Some(read_header(bytes, state.store, r)?) {
            return Err(WalError::Sequence);
        }
        super::framing::state_write(
            state,
            &mut Writer {
                bytes: None,
                pos: 0,
            },
            r,
        )?;
        Ok(Self {
            bytes,
            state,
            offset: 64,
            failed: false,
            end: None,
        })
    }
    /// Validates the next whole batch. Any error permanently latches this reader.
    pub fn next_envelope(
        &mut self,
        validator: &mut dyn ReplayValidator,
        r: &mut WalResources<'_>,
    ) -> Result<ReplayStep<'a>, WalError> {
        if self.failed {
            return Err(WalError::Failed);
        }
        if let Some(end) = self.end {
            return Ok(ReplayStep::End(end));
        }
        let result = self.next_inner(Some(validator), r);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    pub(crate) fn next_framed_capture(
        &mut self,
        r: &mut WalResources<'_>,
    ) -> Result<FramedCaptureStep<'a>, WalError> {
        if self.failed {
            return Err(WalError::Failed);
        }
        if let Some(end) = self.end {
            return Ok(FramedCaptureStep::End(end));
        }
        let result = self.next_inner(None, r);
        match result {
            Ok(ReplayStep::Envelope(envelope)) => {
                Ok(FramedCaptureStep::Envelope(FramedCaptureEnvelope {
                    change_bytes: envelope.change_bytes,
                    change_count: envelope.change_count,
                    batch: envelope.batch,
                    kind: envelope.kind,
                    state: envelope.state,
                    complete_bytes: self.offset,
                }))
            }
            Ok(ReplayStep::End(end)) => Ok(FramedCaptureStep::End(end)),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }
    fn finish(&mut self, incomplete_tail: bool) -> ReplayStep<'a> {
        let end = ReplayEnd {
            complete_bytes: self.offset,
            incomplete_tail,
        };
        self.end = Some(end);
        ReplayStep::End(end)
    }
    fn next_inner(
        &mut self,
        validator: Option<&mut dyn ReplayValidator>,
        r: &mut WalResources<'_>,
    ) -> Result<ReplayStep<'a>, WalError> {
        use super::codec::*;
        use super::framing::*;
        r.charge(0)?;
        let bytes = self.bytes.get(self.offset..).ok_or(WalError::Malformed)?;
        if bytes.is_empty() {
            return Ok(self.finish(false));
        }
        let seq = self
            .state
            .sequence
            .checked_add(1)
            .ok_or(WalError::Sequence)?;
        let Some(begin) = read_record(bytes, 0, None, seq, Some(1), r)? else {
            return Ok(self.finish(true));
        };
        let mut rd = Reader {
            bytes: begin.payload,
            pos: 0,
        };
        if rd.u128(r)? != self.state.store.get() {
            return Err(WalError::Store);
        }
        let kind = match rd.u8(r)? {
            1 => EnvelopeKind::Mutation,
            2 => EnvelopeKind::Maintenance,
            _ => return Err(WalError::Unsupported),
        };
        rd.zero(7, r)?;
        if rd.u64(r)? != self.state.generation.get() {
            return Err(WalError::Sequence);
        }
        let target = rd.u64(r)?;
        if self.state.generation.get().checked_add(1) != Some(target) {
            return Err(WalError::Sequence);
        }
        let count = rd.u32(r)?;
        rd.zero(4, r)?;
        let declared = usize::try_from(rd.u64(r)?).map_err(|_| WalError::Capacity)?;
        rd.end()?;
        if declared > MAX_ENVELOPE_BYTES
            || declared < begin.bytes + 72
            || u64::from(count) * 72 > declared as u64
        {
            return Err(WalError::Capacity);
        }
        let bounded = bytes
            .get(..bytes.len().min(declared))
            .ok_or(WalError::Malformed)?;
        // Fully present malformed changes are corruption even when Commit is torn.
        // The final committed high-waters are not available yet; their tighter
        // checks run after Commit, while structure/provenance are checked now.
        let provisional = CommitState {
            generation: GraphGeneration::new(target),
            sequence: seq,
            high_waters: HighWaters {
                node: u128::MAX,
                relationship: u128::MAX,
                symbols: [u64::MAX; 4],
                creation_serial: u64::MAX,
            },
            ..self.state
        };
        let mut cursor = begin.bytes;
        let mut mutations = 0usize;
        for index in 1..=count {
            let Some(change) = read_record(
                bounded.get(cursor..).ok_or(WalError::Malformed)?,
                index,
                Some(begin.batch),
                seq,
                None,
                r,
            )?
            else {
                if bytes.len() >= declared {
                    return Err(WalError::Malformed);
                }
                return Ok(self.finish(true));
            };
            if !(2..=5).contains(&change.kind) {
                return Err(WalError::Malformed);
            }
            if (kind == EnvelopeKind::Maintenance && change.kind == 2)
                || (kind == EnvelopeKind::Mutation && change.kind >= 4)
            {
                return Err(WalError::Participant);
            }
            if change.kind == 2 {
                mutations += 1;
                if mutations > super::super::MAX_GRAPH_CHANGES {
                    return Err(WalError::Capacity);
                }
            }
            super::change::read(change.kind, change.payload, provisional, r)?;
            cursor = cursor
                .checked_add(change.bytes)
                .ok_or(WalError::Malformed)?;
        }
        let Some(commit) = read_record(
            bounded.get(cursor..).ok_or(WalError::Malformed)?,
            count.checked_add(1).ok_or(WalError::Malformed)?,
            Some(begin.batch),
            seq,
            Some(6),
            r,
        )?
        else {
            if bytes.len() >= declared {
                return Err(WalError::Malformed);
            }
            return Ok(self.finish(true));
        };
        if cursor + commit.bytes != declared {
            return Err(WalError::Malformed);
        }
        let mut rd = Reader {
            bytes: commit.payload,
            pos: 0,
        };
        if rd.u32(r)? != count {
            return Err(WalError::Malformed);
        }
        rd.zero(4, r)?;
        if rd.u64(r)? != hash(bytes.get(..cursor).ok_or(WalError::Malformed)?, r)? {
            return Err(WalError::Checksum);
        }
        let state = state_read(&mut rd, r)?;
        rd.end()?;
        validate_transition(self.state, state)?;
        if state.generation.get() != target || state.sequence != seq {
            return Err(WalError::Sequence);
        }
        if validator.is_none() {
            let mut changes = ChangeReader {
                bytes: bounded
                    .get(begin.bytes..cursor)
                    .ok_or(WalError::Malformed)?,
                state,
                batch: begin.batch,
                offset: 0,
                index: 1,
                remaining: count,
                failed: false,
            };
            while changes.next_change(r)?.is_some() {}
        }
        if let Some(validator) = validator {
            let mut change_offset = begin.bytes;
            for index in 1..=count {
                let record = read_record(
                    bounded
                        .get(change_offset..cursor)
                        .ok_or(WalError::Malformed)?,
                    index,
                    Some(begin.batch),
                    seq,
                    None,
                    r,
                )?
                .ok_or(WalError::Malformed)?;
                let change = super::change::read(record.kind, record.payload, state, r)?;
                match change {
                    Change::Mutation(v) => {
                        if let Some(reference) = v.canonical {
                            validator.required(reference, RequiredRole::Canonical, r)?;
                        }
                        validator.mutation(v, r)?;
                    }
                    Change::Inventory(v) => validator.inventory(v, r)?,
                    Change::ReclaimIntent(v) => {
                        validator.required(
                            v.protected_roots,
                            RequiredRole::Participant(ParticipantRole::ProtectedRoots),
                            r,
                        )?;
                        validator.required(
                            v.completed_mark,
                            RequiredRole::Participant(ParticipantRole::CompletedMark),
                            r,
                        )?;
                        validator.reclaim_intent(v, r)?;
                    }
                    Change::ReclaimComplete(v) => {
                        validator.required(
                            v.intent,
                            RequiredRole::Participant(ParticipantRole::ReclaimState),
                            r,
                        )?;
                        validator.reclaim_complete(v, r)?;
                    }
                }
                change_offset += record.bytes;
            }
            for (slot, root) in state.graph.slots.iter().enumerate() {
                if let Some(root) = root {
                    let tree = match slot {
                        0 => TreeKind::Nodes,
                        1 => TreeKind::Relationships,
                        2 => TreeKind::KeyFences,
                        3 => TreeKind::Labels,
                        4 => TreeKind::RelationshipTypes,
                        5 => TreeKind::OutRanges,
                        6 => TreeKind::InRanges,
                        7 => TreeKind::ObjectInventory,
                        _ => return Err(WalError::Malformed),
                    };
                    validator.required(*root, RequiredRole::Tree(tree), r)?;
                }
            }
            validator.required(
                state.catalog,
                RequiredRole::Participant(ParticipantRole::Catalog),
                r,
            )?;
            for (root, role) in [
                (state.vector, ParticipantRole::RetrievalState),
                (state.text, ParticipantRole::RetrievalState),
                (state.reclaim, ParticipantRole::ReclaimState),
            ] {
                if let Some(root) = root {
                    validator.required(root, RequiredRole::Participant(role), r)?;
                }
            }
            for i in 0..state.prepared_inventories.len()? {
                validator.required(
                    state.prepared_inventories.get(i, r)?,
                    RequiredRole::Participant(ParticipantRole::PreparedInventory),
                    r,
                )?;
            }
            let changes = ChangeReader {
                bytes: bounded
                    .get(begin.bytes..cursor)
                    .ok_or(WalError::Malformed)?,
                state,
                batch: begin.batch,
                offset: 0,
                index: 1,
                remaining: count,
                failed: false,
            };
            validator.state(kind, self.state, state, changes, r)?;
        }
        r.charge(0)?;
        self.state = state;
        self.offset = self
            .offset
            .checked_add(declared)
            .ok_or(WalError::Malformed)?;
        Ok(ReplayStep::Envelope(ValidatedEnvelope {
            batch: begin.batch,
            kind,
            state,
            change_bytes: bounded
                .get(begin.bytes..cursor)
                .ok_or(WalError::Malformed)?,
            change_count: count,
        }))
    }
}

/// Binds a resolver's already validated immutable object to its WAL descriptor,
/// exact directory block and required role. This allocates nothing. Storage owns
/// artifact admission; participant semantics and complete extent traversal remain
/// mandatory in the resolver hook. This helper never grants deletion authority.
pub fn validate_required_block<'a>(
    reference: RequiredRef,
    role: RequiredRole,
    frame: &'a crate::property_graph::storage::artifact::ArtifactFrame<'_>,
    r: &mut WalResources<'_>,
) -> Result<&'a [u8], WalError> {
    use super::codec::*;
    use crate::property_graph::storage::artifact::{BlockKind, ContainerKind};
    r.charge(1)?;
    descriptor(reference.object)?;
    let bytes = frame.bytes();
    let identity = frame.identity();
    if frame.kind() != ContainerKind::Object
        || identity.store != reference.object.store
        || identity.artifact != reference.object.artifact
        || identity.artifact != reference.block.artifact
    {
        return Err(WalError::Store);
    }
    if identity.generation != reference.object.generation
        || identity.creation_serial != reference.object.serial
        || bytes.len() != reference.object.bytes as usize
    {
        return Err(WalError::Participant);
    }
    let trailer = bytes.len().checked_sub(8).ok_or(WalError::Malformed)?;
    let mut rd = Reader {
        bytes: bytes.get(trailer..).ok_or(WalError::Malformed)?,
        pos: 0,
    };
    if rd.u64(r)? != reference.object.checksum {
        return Err(WalError::Checksum);
    }
    let mut rd = Reader {
        bytes: bytes.get(80..84).ok_or(WalError::Malformed)?,
        pos: 0,
    };
    let count = rd.u32(r)? as usize;
    let mut found = false;
    for index in 0..count {
        r.charge(1)?;
        // ArtifactFrame guarantees every in-range directory entry is valid;
        // reference() therefore cannot construct an owned FormatError here.
        if frame.reference(index).map_err(|_| WalError::Malformed)? == reference.block {
            found = true;
            break;
        }
    }
    if !found {
        return Err(WalError::Participant);
    }
    let offset = usize::try_from(reference.block.offset).map_err(|_| WalError::Malformed)?;
    let end = offset
        .checked_add(reference.block.length as usize)
        .ok_or(WalError::Malformed)?;
    let payload = bytes
        .get(offset.checked_add(24).ok_or(WalError::Malformed)?..end)
        .ok_or(WalError::Malformed)?;
    match role {
        RequiredRole::Tree(tree) => {
            if reference.block.kind != BlockKind::TreePage
                || payload.len() != crate::property_graph::storage::tree::PAGE_BYTES
                || payload.get(..4) != Some(b"ZGTP".as_slice())
            {
                return Err(WalError::Participant);
            }
            let mut rd = Reader {
                bytes: payload,
                pos: 4,
            };
            if rd.u16(r)? != 1 {
                return Err(WalError::Unsupported);
            }
            if rd.u16(r)? != tree as u16 {
                return Err(WalError::Participant);
            }
            Ok(payload)
        }
        RequiredRole::Participant(role) => {
            if reference.block.kind != BlockKind::CommitParticipant
                || payload.get(..4) != Some(b"ZGCP".as_slice())
            {
                return Err(WalError::Participant);
            }
            let mut rd = Reader {
                bytes: payload,
                pos: 4,
            };
            let actual = rd.u16(r)?;
            let version = rd.u16(r)?;
            if !(1..=6).contains(&actual) || version != 1 {
                return Err(WalError::Unsupported);
            }
            if actual != role as u16 {
                return Err(WalError::Participant);
            }
            payload.get(8..).ok_or(WalError::Malformed)
        }
        RequiredRole::Canonical => {
            if !matches!(
                reference.block.kind,
                BlockKind::CanonicalImage | BlockKind::ExtentList
            ) {
                return Err(WalError::Participant);
            }
            Ok(payload)
        }
    }
}

/// Sequential borrowed access to a validated batch's normalized changes.
#[derive(Clone, Copy, Debug)]
pub struct ChangeReader<'a> {
    bytes: &'a [u8],
    state: CommitState<'a>,
    batch: BatchId,
    offset: usize,
    index: u32,
    remaining: u32,
    failed: bool,
}
impl<'a> ValidatedEnvelope<'a> {
    /// Creates an allocation-free cursor over complete validated change frames.
    pub const fn changes(&self) -> ChangeReader<'a> {
        ChangeReader {
            bytes: self.change_bytes,
            state: self.state,
            batch: self.batch,
            offset: 0,
            index: 1,
            remaining: self.change_count,
            failed: false,
        }
    }
}
impl<'a> FramedCaptureEnvelope<'a> {
    pub(crate) const fn changes(&self) -> ChangeReader<'a> {
        ChangeReader {
            bytes: self.change_bytes,
            state: self.state,
            batch: self.batch,
            offset: 0,
            index: 1,
            remaining: self.change_count,
            failed: false,
        }
    }
}
impl<'a> ChangeReader<'a> {
    pub(crate) const fn remaining_count(&self) -> u32 {
        self.remaining
    }

    /// Returns the next complete typed change; errors latch this cursor. Callers
    /// mutate only private recovery state and discard it if later replay fails.
    pub fn next_change(
        &mut self,
        r: &mut WalResources<'_>,
    ) -> Result<Option<Change<'a>>, WalError> {
        if self.failed {
            return Err(WalError::Failed);
        }
        let result = self.next_inner(r);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn next_inner(&mut self, r: &mut WalResources<'_>) -> Result<Option<Change<'a>>, WalError> {
        r.charge(0)?;
        if self.remaining == 0 {
            return if self.offset == self.bytes.len() {
                Ok(None)
            } else {
                Err(WalError::Malformed)
            };
        }
        let record = super::framing::read_record(
            self.bytes.get(self.offset..).ok_or(WalError::Malformed)?,
            self.index,
            Some(self.batch),
            self.state.sequence,
            None,
            r,
        )?
        .ok_or(WalError::Malformed)?;
        let change = super::change::read(record.kind, record.payload, self.state, r)?;
        self.offset = self
            .offset
            .checked_add(record.bytes)
            .ok_or(WalError::Malformed)?;
        self.index = self.index.checked_add(1).ok_or(WalError::Malformed)?;
        self.remaining -= 1;
        Ok(Some(change))
    }
}

fn read_header(
    bytes: &[u8],
    store: StoreInstanceId,
    r: &mut WalResources<'_>,
) -> Result<u64, WalError> {
    use super::codec::*;
    r.charge(0)?;
    let header = bytes.get(..HEADER_BYTES).ok_or(WalError::Malformed)?;
    let mut rd = Reader {
        bytes: header,
        pos: 0,
    };
    if rd.take(8, r)? != b"ZEPEMBED"
        || rd.u16(r)? != crate::format::FormatFamily::NativeGraphWal.id()
        || rd.u16(r)? != 1
    {
        return Err(WalError::Unsupported);
    }
    rd.zero(4, r)?;
    if rd.u64(r)? != 64 || rd.u64(r)? != 0 {
        return Err(WalError::Malformed);
    }
    if rd.u128(r)? != store.get() {
        return Err(WalError::Store);
    }
    let first = rd.u64(r)?;
    if first == 0 {
        return Err(WalError::Sequence);
    }
    let expected = rd.u64(r)?;
    if hash(header.get(..56).ok_or(WalError::Malformed)?, r)? != expected {
        return Err(WalError::Checksum);
    }
    Ok(first)
}

pub(crate) fn same_commit_state(
    left: CommitState<'_>,
    right: CommitState<'_>,
    r: &mut WalResources<'_>,
) -> Result<bool, WalError> {
    r.charge(1)?;
    if left.store != right.store
        || left.generation != right.generation
        || left.sequence != right.sequence
        || left.high_waters != right.high_waters
        || left.graph != right.graph
        || left.catalog != right.catalog
        || left.vector != right.vector
        || left.text != right.text
        || left.reclaim != right.reclaim
        || left.prepared_inventories.len()? != right.prepared_inventories.len()?
    {
        return Ok(false);
    }
    for index in 0..left.prepared_inventories.len()? {
        if left.prepared_inventories.get(index, r)? != right.prepared_inventories.get(index, r)? {
            return Ok(false);
        }
    }
    Ok(true)
}
