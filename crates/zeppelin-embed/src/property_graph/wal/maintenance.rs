use super::codec::*;
use super::*;
use crate::property_graph::storage::artifact::BlockKind;
fn object(v: ArtifactDescriptor, state: CommitState<'_>) -> Result<(), WalError> {
    descriptor(v)?;
    if v.store != state.store {
        return Err(WalError::Store);
    }
    if v.generation > state.generation || v.serial > state.high_waters.creation_serial {
        return Err(WalError::HighWater);
    }
    Ok(())
}
fn candidates(
    v: DescriptorList<'_>,
    state: CommitState<'_>,
    fence: u64,
    r: &mut WalResources<'_>,
) -> Result<(), WalError> {
    let count = v.len()?;
    if count > MAX_ENVELOPE_BYTES / 64 {
        return Err(WalError::Capacity);
    }
    let mut previous = None;
    for index in 0..count {
        let candidate = v.get(index, r)?;
        candidate_descriptor(candidate)?;
        if candidate.store != state.store {
            return Err(WalError::Store);
        }
        if candidate.generation > state.generation
            || candidate.serial > state.high_waters.creation_serial
        {
            return Err(WalError::HighWater);
        }
        if candidate.serial > fence {
            return Err(WalError::HighWater);
        }
        if previous.is_some_and(|id| id >= candidate.artifact) {
            return Err(WalError::Malformed);
        }
        previous = Some(candidate.artifact);
    }
    Ok(())
}
fn put_candidates(
    v: DescriptorList<'_>,
    w: &mut Writer<'_>,
    r: &mut WalResources<'_>,
) -> Result<(), WalError> {
    w.u32(u32::try_from(v.len()?).map_err(|_| WalError::Capacity)?, r)?;
    w.u32(0, r)?;
    for index in 0..v.len()? {
        put_candidate_descriptor(v.get(index, r)?, w, r)?;
    }
    Ok(())
}
fn get_candidates<'a>(
    rd: &mut Reader<'a>,
    r: &mut WalResources<'_>,
) -> Result<DescriptorList<'a>, WalError> {
    let n = rd.u32(r)? as usize;
    rd.zero(4, r)?;
    Ok(DescriptorList::Encoded(
        rd.take(n.checked_mul(64).ok_or(WalError::Capacity)?, r)?,
    ))
}
pub(super) fn validate(
    v: Change<'_>,
    state: CommitState<'_>,
    r: &mut WalResources<'_>,
) -> Result<(), WalError> {
    match v {
        Change::Inventory(v) => object(v.object, state),
        Change::ReclaimIntent(v) => {
            if v.capture_generation >= state.generation
                || v.capture_sequence >= state.sequence
                || v.serial_fence > state.high_waters.creation_serial
            {
                return Err(WalError::Sequence);
            }
            super::framing::validate_ref(v.protected_roots, state, BlockKind::CommitParticipant)?;
            super::framing::validate_ref(v.completed_mark, state, BlockKind::CommitParticipant)?;
            candidates(v.candidates, state, v.serial_fence, r)
        }
        Change::ReclaimComplete(v) => {
            super::framing::validate_ref(v.intent, state, BlockKind::CommitParticipant)?;
            candidates(v.completed, state, state.high_waters.creation_serial, r)?;
            candidates(v.remaining, state, state.high_waters.creation_serial, r)?;
            let (mut left, mut right) = (0, 0);
            while left < v.completed.len()? && right < v.remaining.len()? {
                let a = v.completed.get(left, r)?.artifact;
                let b = v.remaining.get(right, r)?.artifact;
                match a.cmp(&b) {
                    std::cmp::Ordering::Less => left += 1,
                    std::cmp::Ordering::Greater => right += 1,
                    std::cmp::Ordering::Equal => return Err(WalError::Malformed),
                }
            }
            Ok(())
        }
        Change::Mutation(_) => Err(WalError::Participant),
    }
}
pub(super) fn write(
    v: Change<'_>,
    state: CommitState<'_>,
    w: &mut Writer<'_>,
    r: &mut WalResources<'_>,
) -> Result<(), WalError> {
    validate(v, state, r)?;
    match v {
        Change::Inventory(v) => {
            put_descriptor(v.object, w, r)?;
            let (tag, id) = match v.state {
                InventoryState::Prepared => (1, 0),
                InventoryState::Retained => (2, 0),
                InventoryState::ReclaimPending(id) => (3, id.get()),
                InventoryState::Reclaimed(id) => (4, id.get()),
            };
            w.u8(tag, r)?;
            w.put(&[0; 7], r)?;
            w.u128(id, r)
        }
        Change::ReclaimIntent(v) => {
            w.u128(v.id.get(), r)?;
            w.u64(v.capture_generation.get(), r)?;
            w.u64(v.capture_sequence, r)?;
            w.u64(v.serial_fence, r)?;
            w.u8(1, r)?;
            w.put(&[0; 7], r)?; // Explicit complete-mark state, never inferred.
            put_ref(v.protected_roots, w, r)?;
            w.u64(v.protected_digest, r)?;
            put_ref(v.completed_mark, w, r)?;
            w.u64(v.mark_digest, r)?;
            put_candidates(v.candidates, w, r)
        }
        Change::ReclaimComplete(v) => {
            w.u128(v.id.get(), r)?;
            put_ref(v.intent, w, r)?;
            w.u8(3, r)?;
            w.put(&[0; 7], r)?; // Both completed unlink and durable directory sync.
            put_candidates(v.completed, w, r)?;
            put_candidates(v.remaining, w, r)
        }
        Change::Mutation(_) => Err(WalError::Participant),
    }
}
pub(super) fn read<'a>(
    tag: u16,
    rd: &mut Reader<'a>,
    state: CommitState<'_>,
    r: &mut WalResources<'_>,
) -> Result<Change<'a>, WalError> {
    let v = match tag {
        3 => {
            let object = get_descriptor(rd, r)?;
            let tag = rd.u8(r)?;
            rd.zero(7, r)?;
            let id = rd.u128(r)?;
            let state = match (tag, id) {
                (1, 0) => InventoryState::Prepared,
                (2, 0) => InventoryState::Retained,
                (3, _) => InventoryState::ReclaimPending(BatchId::new(id)?),
                (4, _) => InventoryState::Reclaimed(BatchId::new(id)?),
                _ => return Err(WalError::Malformed),
            };
            Change::Inventory(InventoryChange { object, state })
        }
        4 => {
            let id = BatchId::new(rd.u128(r)?)?;
            let capture_generation = GraphGeneration::new(rd.u64(r)?);
            let capture_sequence = rd.u64(r)?;
            let serial_fence = rd.u64(r)?;
            if rd.u8(r)? != 1 {
                return Err(WalError::Participant);
            }
            rd.zero(7, r)?;
            let protected_roots = get_ref(rd, r)?;
            let protected_digest = rd.u64(r)?;
            let completed_mark = get_ref(rd, r)?;
            let mark_digest = rd.u64(r)?;
            let candidates = get_candidates(rd, r)?;
            Change::ReclaimIntent(ReclaimIntent {
                id,
                capture_generation,
                capture_sequence,
                serial_fence,
                protected_roots,
                protected_digest,
                completed_mark,
                mark_digest,
                candidates,
            })
        }
        5 => {
            let id = BatchId::new(rd.u128(r)?)?;
            let intent = get_ref(rd, r)?;
            if rd.u8(r)? != 3 {
                return Err(WalError::Participant);
            }
            rd.zero(7, r)?;
            let completed = get_candidates(rd, r)?;
            let remaining = get_candidates(rd, r)?;
            Change::ReclaimComplete(ReclaimComplete {
                id,
                intent,
                completed,
                remaining,
            })
        }
        _ => return Err(WalError::Unsupported),
    };
    validate(v, state, r)?;
    Ok(v)
}
