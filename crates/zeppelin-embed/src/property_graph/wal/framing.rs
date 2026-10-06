use super::codec::*;
use super::*;
use crate::property_graph::storage::artifact::BlockKind;
use xxhash_rust::xxh3::xxh3_64;

pub(super) fn validate_transition(
    base: CommitState<'_>,
    next: CommitState<'_>,
) -> Result<(), WalError> {
    if base.store != next.store {
        return Err(WalError::Store);
    }
    if base.generation.get().checked_add(1) != Some(next.generation.get())
        || base.sequence.checked_add(1) != Some(next.sequence)
    {
        return Err(WalError::Sequence);
    }
    if base.high_waters.node > next.high_waters.node
        || base.high_waters.relationship > next.high_waters.relationship
        || base.high_waters.creation_serial > next.high_waters.creation_serial
        || base
            .high_waters
            .symbols
            .iter()
            .zip(next.high_waters.symbols)
            .any(|(a, b)| *a > b)
    {
        return Err(WalError::HighWater);
    }
    Ok(())
}
pub(super) fn validate_ref(
    v: RequiredRef,
    state: CommitState<'_>,
    kind: BlockKind,
) -> Result<(), WalError> {
    descriptor(v.object)?;
    if v.object.store != state.store {
        return Err(WalError::Store);
    }
    if v.object.generation > state.generation || v.object.serial > state.high_waters.creation_serial
    {
        return Err(WalError::HighWater);
    }
    if v.block.kind != kind || v.block.version != 1 {
        return Err(WalError::Participant);
    }
    Ok(())
}
pub(super) fn state_write(
    v: CommitState<'_>,
    w: &mut Writer<'_>,
    r: &mut WalResources<'_>,
) -> Result<(), WalError> {
    w.u128(v.store.get(), r)?;
    w.u64(v.generation.get(), r)?;
    w.u64(v.sequence, r)?;
    w.u128(v.high_waters.node, r)?;
    w.u128(v.high_waters.relationship, r)?;
    for high in v.high_waters.symbols {
        w.u64(high, r)?;
    }
    w.u64(v.high_waters.creation_serial, r)?;
    for root in v.graph.slots {
        if let Some(root) = root {
            validate_ref(root, v, BlockKind::TreePage)?;
        }
        put_optional(root, w, r)?;
    }
    validate_ref(v.catalog, v, BlockKind::CommitParticipant)?;
    put_ref(v.catalog, w, r)?;
    for root in [v.vector, v.text, v.reclaim] {
        if let Some(root) = root {
            validate_ref(root, v, BlockKind::CommitParticipant)?;
        }
        put_optional(root, w, r)?;
    }
    let count = v.prepared_inventories.len()?;
    w.u32(u32::try_from(count).map_err(|_| WalError::Capacity)?, r)?;
    w.u32(0, r)?;
    for i in 0..count {
        let root = v.prepared_inventories.get(i, r)?;
        validate_ref(root, v, BlockKind::CommitParticipant)?;
        put_ref(root, w, r)?;
    }
    Ok(())
}
pub(super) fn state_read<'a>(
    rd: &mut Reader<'a>,
    r: &mut WalResources<'_>,
) -> Result<CommitState<'a>, WalError> {
    let store = StoreInstanceId::new(rd.u128(r)?).map_err(|_| WalError::Malformed)?;
    let generation = GraphGeneration::new(rd.u64(r)?);
    let sequence = rd.u64(r)?;
    let node = rd.u128(r)?;
    let relationship = rd.u128(r)?;
    let mut symbols = [0; 4];
    for symbol in &mut symbols {
        *symbol = rd.u64(r)?;
    }
    let creation_serial = rd.u64(r)?;
    let mut graph = WalGraphRoots::default();
    for root in &mut graph.slots {
        *root = get_optional(rd, r)?;
    }
    let catalog = get_ref(rd, r)?;
    let vector = get_optional(rd, r)?;
    let text = get_optional(rd, r)?;
    let reclaim = get_optional(rd, r)?;
    let count = rd.u32(r)? as usize;
    rd.zero(4, r)?;
    let prepared_inventories =
        ReferenceList::Encoded(rd.take(count.checked_mul(96).ok_or(WalError::Malformed)?, r)?);
    let state = CommitState {
        store,
        generation,
        sequence,
        graph,
        catalog,
        vector,
        text,
        reclaim,
        high_waters: HighWaters {
            node,
            relationship,
            symbols,
            creation_serial,
        },
        prepared_inventories,
    };
    state_write(
        state,
        &mut Writer {
            bytes: None,
            pos: 0,
        },
        r,
    )?;
    Ok(state)
}

pub(crate) fn commit_state_size(
    state: CommitState<'_>,
    resources: &mut WalResources<'_>,
) -> Result<usize, WalError> {
    let mut writer = Writer {
        bytes: None,
        pos: 0,
    };
    state_write(state, &mut writer, resources)?;
    Ok(writer.pos)
}

pub(crate) fn encode_commit_state(
    state: CommitState<'_>,
    output: &mut [u8],
    resources: &mut WalResources<'_>,
) -> Result<usize, WalError> {
    let mut measured = Writer {
        bytes: None,
        pos: 0,
    };
    state_write(state, &mut measured, resources)?;
    if output.len() < measured.pos {
        return Err(WalError::Capacity);
    }
    let mut writer = Writer {
        bytes: Some(output),
        pos: 0,
    };
    state_write(state, &mut writer, resources)?;
    Ok(writer.pos)
}

pub(crate) fn decode_commit_state<'a>(
    input: &'a [u8],
    resources: &mut WalResources<'_>,
) -> Result<CommitState<'a>, WalError> {
    let mut reader = Reader {
        bytes: input,
        pos: 0,
    };
    let state = state_read(&mut reader, resources)?;
    if reader.pos != input.len() {
        return Err(WalError::Malformed);
    }
    Ok(state)
}
/// Writes the required graph WAL file header. Existing family11 bytes are unchanged.
pub fn encode_header(
    store: StoreInstanceId,
    first_sequence: u64,
    output: &mut [u8],
) -> Result<usize, WalError> {
    if first_sequence == 0 {
        return Err(WalError::Sequence);
    }
    let output = output.get_mut(..HEADER_BYTES).ok_or(WalError::Capacity)?;
    let mut no_cancel = || false;
    let mut r = WalResources::new(1024, STACK_RESERVATION_BYTES, &mut no_cancel)?;
    let mut w = Writer {
        bytes: Some(output),
        pos: 0,
    };
    w.put(b"ZEPEMBED", &mut r)?;
    w.u16(crate::format::FormatFamily::NativeGraphWal.id(), &mut r)?;
    w.u16(1, &mut r)?;
    w.u32(0, &mut r)?;
    w.u64(64, &mut r)?;
    w.u64(0, &mut r)?;
    w.u128(store.get(), &mut r)?;
    w.u64(first_sequence, &mut r)?;
    let sum = xxh3_64(
        w.bytes
            .as_deref()
            .and_then(|b| b.get(..56))
            .ok_or(WalError::Capacity)?,
    );
    w.u64(sum, &mut r)?;
    Ok(HEADER_BYTES)
}
pub(super) fn record(
    kind: u16,
    index: u32,
    batch: BatchId,
    seq: u64,
    payload: impl Fn(&mut Writer<'_>, &mut WalResources<'_>) -> Result<(), WalError>,
    w: &mut Writer<'_>,
    r: &mut WalResources<'_>,
) -> Result<(), WalError> {
    let mut measure = Writer {
        bytes: None,
        pos: 0,
    };
    payload(&mut measure, r)?;
    let mut header = [0; 64];
    let mut hw = Writer {
        bytes: Some(&mut header),
        pos: 0,
    };
    hw.put(b"ZGWF", r)?;
    hw.u16(kind, r)?;
    hw.u16(1, r)?;
    hw.u32(
        u32::try_from(measure.pos).map_err(|_| WalError::Capacity)?,
        r,
    )?;
    hw.u32(index, r)?;
    hw.u64(seq, r)?;
    hw.u128(batch.get(), r)?;
    hw.put(&[0; 16], r)?;
    let sum = hash(
        hw.bytes
            .as_deref()
            .and_then(|b| b.get(..56))
            .ok_or(WalError::Capacity)?,
        r,
    )?;
    hw.u64(sum, r)?;
    let start = w.pos;
    w.put(&header, r)?;
    payload(w, r)?;
    let checksum = if let Some(bytes) = w.bytes.as_deref() {
        hash(bytes.get(start..w.pos).ok_or(WalError::Capacity)?, r)?
    } else {
        0
    };
    w.u64(checksum, r)
}
/// Encodes one complete envelope into already reserved caller storage. Cancellation
/// may leave private output bytes unfinished; they must never be appended on error.
pub(crate) fn envelope_size(
    base: CommitState<'_>,
    envelope: Envelope<'_>,
    r: &mut WalResources<'_>,
) -> Result<usize, WalError> {
    r.charge(0)?;
    validate_transition(base, envelope.state)?;
    if envelope.changes.len() > MAX_ENVELOPE_BYTES / 72 {
        return Err(WalError::Capacity);
    }
    let mut mutations = 0usize;
    for change in envelope.changes {
        r.charge(1)?;
        match (envelope.kind, change) {
            (EnvelopeKind::Maintenance, Change::Mutation(_))
            | (EnvelopeKind::Mutation, Change::ReclaimIntent(_) | Change::ReclaimComplete(_)) => {
                return Err(WalError::Participant);
            }
            (_, Change::Mutation(_)) => {
                mutations += 1;
            }
            _ => {}
        }
    }
    if mutations > super::super::MAX_GRAPH_CHANGES {
        return Err(WalError::Capacity);
    }
    let mut measured = Writer {
        bytes: None,
        pos: 0,
    };
    envelope_write(base, envelope, 0, &mut measured, r)?;
    Ok(measured.pos)
}

/// Encode into caller storage after measuring with the same bounded codec.
pub fn encode_envelope(
    base: CommitState<'_>,
    envelope: Envelope<'_>,
    output: &mut [u8],
    r: &mut WalResources<'_>,
) -> Result<usize, WalError> {
    let size = envelope_size(base, envelope, r)?;
    if output.len() < size {
        return Err(WalError::Capacity);
    }
    let mut writer = Writer {
        bytes: Some(output),
        pos: 0,
    };
    envelope_write(base, envelope, size, &mut writer, r)?;
    r.charge(0)?;
    Ok(writer.pos)
}

fn envelope_write(
    base: CommitState<'_>,
    v: Envelope<'_>,
    size: usize,
    w: &mut Writer<'_>,
    r: &mut WalResources<'_>,
) -> Result<(), WalError> {
    record(
        1,
        0,
        v.batch,
        v.state.sequence,
        |w, r| {
            w.u128(v.state.store.get(), r)?;
            w.u8(
                match v.kind {
                    EnvelopeKind::Mutation => 1,
                    EnvelopeKind::Maintenance => 2,
                },
                r,
            )?;
            w.put(&[0; 7], r)?;
            w.u64(base.generation.get(), r)?;
            w.u64(v.state.generation.get(), r)?;
            w.u32(v.changes.len() as u32, r)?;
            w.u32(0, r)?;
            w.u64(size as u64, r)
        },
        w,
        r,
    )?;
    for (i, change) in v.changes.iter().enumerate() {
        record(
            super::change::tag(*change),
            (i + 1) as u32,
            v.batch,
            v.state.sequence,
            |w, r| super::change::write(*change, v.state, w, r),
            w,
            r,
        )?;
    }
    let digest = if let Some(bytes) = w.bytes.as_deref() {
        hash(bytes.get(..w.pos).ok_or(WalError::Capacity)?, r)?
    } else {
        0
    };
    record(
        6,
        (v.changes.len() + 1) as u32,
        v.batch,
        v.state.sequence,
        |w, r| {
            w.u32(v.changes.len() as u32, r)?;
            w.u32(0, r)?;
            w.u64(digest, r)?;
            state_write(v.state, w, r)
        },
        w,
        r,
    )
}

pub(super) struct Record<'a> {
    pub kind: u16,
    pub batch: BatchId,
    pub payload: &'a [u8],
    pub bytes: usize,
}
fn prefix(bytes: &[u8], offset: usize, expected: &[u8]) -> Result<(), WalError> {
    if let Some(actual) = bytes.get(offset..) {
        let n = actual.len().min(expected.len());
        if actual.get(..n) != expected.get(..n) {
            return Err(WalError::Malformed);
        }
    }
    Ok(())
}
pub(super) fn read_record<'a>(
    bytes: &'a [u8],
    index: u32,
    batch: Option<BatchId>,
    seq: u64,
    kind: Option<u16>,
    r: &mut WalResources<'_>,
) -> Result<Option<Record<'a>>, WalError> {
    r.charge(1)?;
    prefix(bytes, 0, b"ZGWF")?;
    prefix(bytes, 6, &1u16.to_le_bytes())?;
    prefix(bytes, 12, &index.to_le_bytes())?;
    prefix(bytes, 16, &seq.to_le_bytes())?;
    prefix(bytes, 40, &[0; 16])?;
    if let Some(batch) = batch {
        prefix(bytes, 24, &batch.get().to_le_bytes())?;
    }
    if let Some(kind) = kind {
        prefix(bytes, 4, &kind.to_le_bytes())?;
    }
    if kind.is_none() {
        let observed = bytes.get(4..bytes.len().min(6)).unwrap_or(&[]);
        if ![2u16, 3, 4, 5]
            .iter()
            .any(|tag| tag.to_le_bytes().get(..observed.len()) == Some(observed))
        {
            return Err(WalError::Unsupported);
        }
    }
    // A partial length can already exceed every legal complete continuation.
    let mut partial_length = [0; 4];
    if let Some(part) = bytes.get(8..bytes.len().min(12)) {
        partial_length
            .get_mut(..part.len())
            .ok_or(WalError::Malformed)?
            .copy_from_slice(part);
    }
    if u32::from_le_bytes(partial_length) as usize > MAX_ENVELOPE_BYTES - 72 {
        return Err(WalError::Capacity);
    }
    if kind == Some(1) {
        prefix(bytes, 8, &56u32.to_le_bytes())?;
    }
    if batch.is_none()
        && bytes.len() >= 40
        && bytes.get(24..40).is_some_and(|v| v.iter().all(|b| *b == 0))
    {
        return Err(WalError::Malformed);
    }
    if bytes.len() < 64 {
        if bytes.len() >= 56 {
            let checksum = hash(bytes.get(..56).ok_or(WalError::Malformed)?, r)?;
            prefix(bytes, 56, &checksum.to_le_bytes())?;
        }
        return Ok(None);
    }
    let mut rd = Reader { bytes, pos: 4 };
    let observed_kind = rd.u16(r)?;
    let version = rd.u16(r)?;
    let length = rd.u32(r)? as usize;
    rd.u32(r)?;
    rd.u64(r)?;
    let observed_batch = BatchId::new(rd.u128(r)?)?;
    rd.zero(16, r)?;
    let expected = rd.u64(r)?;
    if hash(bytes.get(..56).ok_or(WalError::Malformed)?, r)? != expected {
        return Err(WalError::Checksum);
    }
    if !(1..=6).contains(&observed_kind) || version != 1 {
        return Err(WalError::Unsupported);
    }
    let total = length
        .checked_add(72)
        .filter(|v| *v <= MAX_ENVELOPE_BYTES)
        .ok_or(WalError::Capacity)?;
    if bytes.len() < total {
        return Ok(None);
    }
    let end = 64 + length;
    let checksum = u64::from_le_bytes(
        bytes
            .get(end..total)
            .ok_or(WalError::Malformed)?
            .try_into()
            .map_err(|_| WalError::Malformed)?,
    );
    if hash(bytes.get(..end).ok_or(WalError::Malformed)?, r)? != checksum {
        return Err(WalError::Checksum);
    }
    Ok(Some(Record {
        kind: observed_kind,
        batch: observed_batch,
        payload: bytes.get(64..end).ok_or(WalError::Malformed)?,
        bytes: total,
    }))
}
