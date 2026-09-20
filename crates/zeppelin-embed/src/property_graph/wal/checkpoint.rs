//! Checked native root-checkpoint payload shared by creation and later recovery.

use super::{CommitState, WalError, WalResources};

const MAGIC: &[u8; 8] = b"ZGCKPT01";
const PREFIX_BYTES: usize = 56;
const TRAILER_BYTES: usize = 8;

/// Complete logical checkpoint binding one exact native WAL stream and cutoff.
#[derive(Clone, Copy)]
pub(crate) struct NativeCheckpoint<'a> {
    pub(crate) wal_identity: u128,
    pub(crate) first_sequence: u64,
    pub(crate) applied_sequence: u64,
    pub(crate) state: CommitState<'a>,
}

/// Layout: magic[8], codec:u16, interpretation:u16, total:u32,
/// wal_identity:u128, first_sequence:u64, applied_sequence:u64,
/// state_bytes:u64, encoded CommitState, xxh3-64 over every preceding byte.
pub(crate) fn encode_checkpoint(
    checkpoint: NativeCheckpoint<'_>,
    output: &mut [u8],
    resources: &mut WalResources<'_>,
) -> Result<usize, WalError> {
    let state_output = output.get_mut(PREFIX_BYTES..).ok_or(WalError::Capacity)?;
    let state_bytes =
        super::framing::encode_commit_state(checkpoint.state, state_output, resources)?;
    let total = PREFIX_BYTES
        .checked_add(state_bytes)
        .and_then(|value| value.checked_add(TRAILER_BYTES))
        .ok_or(WalError::Capacity)?;
    let total_u32 = u32::try_from(total).map_err(|_| WalError::Capacity)?;
    let state_u64 = u64::try_from(state_bytes).map_err(|_| WalError::Capacity)?;
    let prefix = output.get_mut(..PREFIX_BYTES).ok_or(WalError::Capacity)?;
    prefix
        .get_mut(..8)
        .ok_or(WalError::Capacity)?
        .copy_from_slice(MAGIC);
    prefix
        .get_mut(8..10)
        .ok_or(WalError::Capacity)?
        .copy_from_slice(&1_u16.to_le_bytes());
    prefix
        .get_mut(10..12)
        .ok_or(WalError::Capacity)?
        .copy_from_slice(&1_u16.to_le_bytes());
    prefix
        .get_mut(12..16)
        .ok_or(WalError::Capacity)?
        .copy_from_slice(&total_u32.to_le_bytes());
    prefix
        .get_mut(16..32)
        .ok_or(WalError::Capacity)?
        .copy_from_slice(&checkpoint.wal_identity.to_le_bytes());
    prefix
        .get_mut(32..40)
        .ok_or(WalError::Capacity)?
        .copy_from_slice(&checkpoint.first_sequence.to_le_bytes());
    prefix
        .get_mut(40..48)
        .ok_or(WalError::Capacity)?
        .copy_from_slice(&checkpoint.applied_sequence.to_le_bytes());
    prefix
        .get_mut(48..56)
        .ok_or(WalError::Capacity)?
        .copy_from_slice(&state_u64.to_le_bytes());
    let checksum_offset = total.checked_sub(TRAILER_BYTES).ok_or(WalError::Capacity)?;
    let checksum = super::codec::hash(
        output.get(..checksum_offset).ok_or(WalError::Capacity)?,
        resources,
    )?;
    output
        .get_mut(checksum_offset..total)
        .ok_or(WalError::Capacity)?
        .copy_from_slice(&checksum.to_le_bytes());
    Ok(total)
}

pub(crate) fn decode_checkpoint<'a>(
    input: &'a [u8],
    resources: &mut WalResources<'_>,
) -> Result<NativeCheckpoint<'a>, WalError> {
    if input.len() < PREFIX_BYTES + TRAILER_BYTES || input.get(..8) != Some(MAGIC.as_slice()) {
        return Err(WalError::Malformed);
    }
    let u16_at = |range: std::ops::Range<usize>| {
        input
            .get(range)
            .and_then(|value| value.first_chunk::<2>())
            .copied()
            .map(u16::from_le_bytes)
    };
    let u32_at = |range: std::ops::Range<usize>| {
        input
            .get(range)
            .and_then(|value| value.first_chunk::<4>())
            .copied()
            .map(u32::from_le_bytes)
    };
    let u64_at = |range: std::ops::Range<usize>| {
        input
            .get(range)
            .and_then(|value| value.first_chunk::<8>())
            .copied()
            .map(u64::from_le_bytes)
    };
    let u128_at = |range: std::ops::Range<usize>| {
        input
            .get(range)
            .and_then(|value| value.first_chunk::<16>())
            .copied()
            .map(u128::from_le_bytes)
    };
    if u16_at(8..10) != Some(1) || u16_at(10..12) != Some(1) {
        return Err(WalError::Malformed);
    }
    let total = usize::try_from(u32_at(12..16).ok_or(WalError::Malformed)?)
        .map_err(|_| WalError::Malformed)?;
    let state_bytes = usize::try_from(u64_at(48..56).ok_or(WalError::Malformed)?)
        .map_err(|_| WalError::Malformed)?;
    if total != input.len()
        || PREFIX_BYTES
            .checked_add(state_bytes)
            .and_then(|value| value.checked_add(TRAILER_BYTES))
            != Some(total)
    {
        return Err(WalError::Malformed);
    }
    let checksum_offset = total - TRAILER_BYTES;
    if u64_at(checksum_offset..total)
        != Some(super::codec::hash(
            input.get(..checksum_offset).ok_or(WalError::Malformed)?,
            resources,
        )?)
    {
        return Err(WalError::Checksum);
    }
    let state = super::framing::decode_commit_state(
        input
            .get(PREFIX_BYTES..checksum_offset)
            .ok_or(WalError::Malformed)?,
        resources,
    )?;
    let first_sequence = u64_at(32..40).ok_or(WalError::Malformed)?;
    let applied_sequence = u64_at(40..48).ok_or(WalError::Malformed)?;
    let after_applied = applied_sequence.checked_add(1).ok_or(WalError::Sequence)?;
    if first_sequence == 0 || applied_sequence != state.sequence || first_sequence > after_applied {
        return Err(WalError::Sequence);
    }
    Ok(NativeCheckpoint {
        wal_identity: u128_at(16..32).ok_or(WalError::Malformed)?,
        first_sequence,
        applied_sequence,
        state,
    })
}
