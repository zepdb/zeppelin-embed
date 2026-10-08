use super::*;

#[derive(Clone, Copy)]
pub(super) struct Run<'a> {
    pub bytes: &'a [u8],
    pub sequence: u64,
    pub count: usize,
    pub delta: bool,
}
impl Run<'_> {
    pub fn entry<E>(
        self,
        index: usize,
        c: &mut impl FnMut(Work) -> Result<(), E>,
    ) -> Result<Option<DeltaEntry>, Error<E>> {
        if index >= self.count {
            return Ok(None);
        }
        let width = if self.delta { 40 } else { 32 };
        step(c, Work::EntryBytes(width))?;
        let offset = index
            .checked_mul(width)
            .and_then(|n| n.checked_add(HEADER_BYTES))
            .ok_or(Error::Format(FormatIssue::Length))?;
        let rel = RelId::new(u128::from_le_bytes(read(self.bytes, offset)?))
            .map_err(|_| Error::Format(FormatIssue::Identity))?;
        let neighbor = NodeId::from(crate::ingest::DocId::new(u128::from_le_bytes(read(
            self.bytes,
            offset + 16,
        )?)));
        let action = if self.delta {
            let tail = read::<8, E>(self.bytes, offset + 32)?;
            if tail.iter().skip(1).any(|&b| b != 0) {
                return Err(Error::Format(FormatIssue::Reserved));
            }
            match tail.first() {
                Some(1) => Action::Insert,
                Some(2) => Action::Delete,
                _ => return Err(Error::Format(FormatIssue::Tag)),
            }
        } else {
            Action::Insert
        };
        Ok(Some(DeltaEntry {
            edge: Edge { rel, neighbor },
            action,
        }))
    }
}

pub(super) fn decode<'a, E>(
    key: RangeKey,
    delta: bool,
    bytes: &'a [u8],
    c: &mut impl FnMut(Work) -> Result<(), E>,
) -> Result<Run<'a>, Error<E>> {
    step(c, Work::HeaderBytes(bytes.len().min(HEADER_BYTES)))?;
    let header = read::<HEADER_BYTES, E>(bytes, 0)?;
    if read::<4, E>(&header, 0)? != *b"ZADJ"
        || u16::from_le_bytes(read(&header, 4)?) != 1
        || u16::from_le_bytes(read(&header, 6)?) != if delta { 14 } else { 13 }
    {
        return Err(Error::Format(FormatIssue::Tag));
    }
    if header.iter().skip(78).any(|&b| b != 0) {
        return Err(Error::Format(FormatIssue::Reserved));
    }
    let node = NodeId::from(crate::ingest::DocId::new(u128::from_le_bytes(read(
        &header, 8,
    )?)));
    let rel_type = RelTypeId::new(u64::from_le_bytes(read(&header, 24)?))
        .map_err(|_| Error::Format(FormatIssue::Identity))?;
    let lower = RelId::new(u128::from_le_bytes(read(&header, 32)?))
        .map_err(|_| Error::Format(FormatIssue::Identity))?;
    let upper_bits = u128::from_le_bytes(read(&header, 48)?);
    let upper = match header.get(77) {
        Some(0) => UpperBound::Exclusive(
            RelId::new(upper_bits).map_err(|_| Error::Format(FormatIssue::Identity))?,
        ),
        Some(1) if upper_bits == 0 => UpperBound::Infinity,
        Some(1) => return Err(Error::Format(FormatIssue::Reserved)),
        _ => return Err(Error::Format(FormatIssue::Tag)),
    };
    let direction = match header.get(76) {
        Some(1) => Direction::Out,
        Some(2) => Direction::In,
        _ => return Err(Error::Format(FormatIssue::Tag)),
    };
    let decoded = RangeKey {
        node,
        rel_type,
        direction,
        lower,
        upper,
    };
    valid_range(decoded)?;
    step(c, Work::Compare)?;
    if decoded != key {
        return Err(Error::Format(FormatIssue::Group));
    }
    let count = usize::try_from(u32::from_le_bytes(read(&header, 72)?))
        .map_err(|_| Error::Format(FormatIssue::Length))?;
    limit_count(delta, count)?;
    if bytes.len() != payload_len(delta, count)? {
        return Err(Error::Format(FormatIssue::Length));
    }
    let run = Run {
        bytes,
        sequence: u64::from_le_bytes(read(&header, 64)?),
        count,
        delta,
    };
    let mut previous = None;
    for index in 0..count {
        let entry = run
            .entry(index, c)?
            .ok_or(Error::Format(FormatIssue::Length))?;
        valid_edge(key, entry.edge, previous, c)?;
        previous = Some(entry.edge.rel);
    }
    Ok(run)
}

/// Encodes one bounded immutable base into caller-owned private scratch.
/// Returns only after full input validation and the final control check.
pub fn encode_base<'a, E>(
    key: RangeKey,
    watermark: u64,
    entries: &[Edge],
    output: &'a mut [u8],
    c: &mut impl FnMut(Work) -> Result<(), E>,
) -> Result<&'a [u8], Error<E>> {
    encode(
        key,
        watermark,
        entries.iter().map(|&edge| DeltaEntry {
            edge,
            action: Action::Insert,
        }),
        false,
        output,
        c,
    )
}
/// Encodes one bounded immutable delta; a delete retains its neighbor.
/// Sequence compatibility with its base/cutoff is checked by merge.
pub fn encode_delta<'a, E>(
    key: RangeKey,
    sequence: u64,
    entries: &[DeltaEntry],
    output: &'a mut [u8],
    c: &mut impl FnMut(Work) -> Result<(), E>,
) -> Result<&'a [u8], Error<E>> {
    if sequence == 0 {
        return Err(Error::Format(FormatIssue::Sequence));
    }
    encode(key, sequence, entries.iter().copied(), true, output, c)
}
fn encode<'a, E>(
    key: RangeKey,
    sequence: u64,
    entries: impl ExactSizeIterator<Item = DeltaEntry> + Clone,
    delta: bool,
    output: &'a mut [u8],
    c: &mut impl FnMut(Work) -> Result<(), E>,
) -> Result<&'a [u8], Error<E>> {
    step(c, Work::HeaderBytes(0))?;
    valid_range(key)?;
    let count = entries.len();
    limit_count(delta, count)?;
    let length = payload_len(delta, count)?;
    if output.len() < length {
        return Err(Error::Limit(LimitIssue::Output));
    }
    let mut previous = None;
    for entry in entries.clone() {
        step(c, Work::EntryBytes(if delta { 40 } else { 32 }))?;
        valid_edge(key, entry.edge, previous, c)?;
        previous = Some(entry.edge.rel);
    }
    let mut header = [0_u8; HEADER_BYTES];
    put(&mut header, 0, b"ZADJ")?;
    put(&mut header, 4, &1_u16.to_le_bytes())?;
    put(
        &mut header,
        6,
        &(if delta { 14_u16 } else { 13 }).to_le_bytes(),
    )?;
    put(&mut header, 8, &key.node.get().to_le_bytes())?;
    put(&mut header, 24, &key.rel_type.get().to_le_bytes())?;
    put(&mut header, 32, &key.lower.get().to_le_bytes())?;
    let (upper, tag) = match key.upper {
        UpperBound::Exclusive(id) => (id.get(), 0),
        UpperBound::Infinity => (0, 1),
    };
    put(&mut header, 48, &upper.to_le_bytes())?;
    put(&mut header, 64, &sequence.to_le_bytes())?;
    let count = u32::try_from(count).map_err(|_| Error::Format(FormatIssue::Length))?;
    put(&mut header, 72, &count.to_le_bytes())?;
    put(&mut header, 76, &[key.direction as u8, tag])?;
    step(c, Work::CopyBytes(HEADER_BYTES))?;
    put(output, 0, &header)?;
    let width = if delta { 40 } else { 32 };
    for (index, entry) in entries.enumerate() {
        step(c, Work::CopyBytes(width))?;
        let offset = HEADER_BYTES + index * width;
        put(output, offset, &entry.edge.rel.get().to_le_bytes())?;
        put(
            output,
            offset + 16,
            &entry.edge.neighbor.get().to_le_bytes(),
        )?;
        if delta {
            put(
                output,
                offset + 32,
                &[entry.action as u8, 0, 0, 0, 0, 0, 0, 0],
            )?;
        }
    }
    step(c, Work::Finish)?;
    output.get(..length).ok_or(Error::Limit(LimitIssue::Output))
}
fn limit_count<E>(delta: bool, count: usize) -> Result<(), Error<E>> {
    if delta && count > MAX_PENDING_ENTRIES {
        Err(Error::Limit(LimitIssue::PendingEntries))
    } else if !delta && count > MAX_BASE_ENTRIES {
        Err(Error::Limit(LimitIssue::BaseEntries))
    } else {
        Ok(())
    }
}
fn payload_len<E>(delta: bool, count: usize) -> Result<usize, Error<E>> {
    count
        .checked_mul(if delta { 40 } else { 32 })
        .and_then(|n| n.checked_add(HEADER_BYTES))
        .ok_or(Error::Format(FormatIssue::Length))
}
fn read<const N: usize, E>(bytes: &[u8], offset: usize) -> Result<[u8; N], Error<E>> {
    bytes
        .get(offset..)
        .and_then(|tail| tail.first_chunk::<N>())
        .copied()
        .ok_or(Error::Format(FormatIssue::Length))
}
fn put<E>(bytes: &mut [u8], offset: usize, value: &[u8]) -> Result<(), Error<E>> {
    let end = offset
        .checked_add(value.len())
        .ok_or(Error::Format(FormatIssue::Length))?;
    bytes
        .get_mut(offset..end)
        .ok_or(Error::Format(FormatIssue::Length))?
        .copy_from_slice(value);
    Ok(())
}
