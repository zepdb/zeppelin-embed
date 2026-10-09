//! Exact logical-key interpretation and chunked overflow comparison.
use super::*;
use crate::property_graph::storage::payload::{CHUNK_BYTES, PayloadRef};
use std::cmp::Ordering;

pub(super) fn length(key: Key<'_>) -> Result<usize, TreeError> {
    match key {
        Key::Inline(bytes) => Ok(bytes.len()),
        Key::Overflow { logical_length, .. } => {
            usize::try_from(logical_length).map_err(|_| TreeError::Invalid("key length overflow"))
        }
    }
}
pub(super) fn span<'a>(
    source: &'a impl BlockSource,
    root: DirectoryRoot,
    key: Key<'a>,
    offset: usize,
    resources: &mut TreeResources<'_>,
) -> Result<&'a [u8], TreeError> {
    match key {
        Key::Inline(bytes) => {
            let bytes = bytes
                .get(offset..)
                .ok_or(TreeError::Invalid("key read extent"))?;
            let length = bytes.len().min(CHUNK_BYTES);
            resources.step(length as u64)?;
            bytes.get(..length).ok_or(TreeError::Invalid("key span"))
        }
        Key::Overflow {
            logical_length,
            reference,
        } => PayloadRef::new(BlockKind::OverflowKey, logical_length, reference)?.span_at(
            source,
            root.store,
            root.generation,
            offset as u64,
            resources,
        ),
    }
}
pub(super) fn prefix(
    source: &impl BlockSource,
    root: DirectoryRoot,
    key: Key<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<(u8, u64), TreeError> {
    let bytes = span(source, root, key, 0, resources)?;
    let kind = bytes
        .first()
        .copied()
        .ok_or(TreeError::Invalid("missing key kind"))?;
    let namespace = u64::from_le_bytes(
        bytes
            .get(1..9)
            .and_then(|part| part.try_into().ok())
            .ok_or(TreeError::Invalid("missing namespace symbol"))?,
    );
    if !(1..=2).contains(&kind) || namespace == 0 {
        return Err(TreeError::Invalid("key kind or namespace"));
    }
    Ok((kind, namespace))
}
pub(super) fn validate_key(
    source: &impl BlockSource,
    root: DirectoryRoot,
    key: Key<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    resources.step(1)?;
    if root.kind != TreeKind::KeyFences {
        return validate_numeric_key(root.kind, key);
    }
    let length = length(key)?;
    if !(9..=crate::property_graph::MAX_GRAPH_INPUT_BYTES).contains(&length) {
        return Err(TreeError::Invalid("key logical length"));
    }
    let key_prefix = if source.scoped_blocks() {
        scoped_prefix(source, root, key, resources)?
    } else {
        prefix(source, root, key, resources)?
    };
    if source.scoped_blocks() {
        let mut buffer = [0_u8; CHUNK_BYTES];
        validate_fence_key(length, key_prefix, |position, utf8| {
            let maximum = (length - position).min(CHUNK_BYTES);
            let count = copy_scoped_span(
                source,
                root,
                key,
                position,
                buffer.get_mut(..maximum).ok_or(TreeError::Memory)?,
                resources,
            )?;
            utf8.feed(buffer.get(..count).ok_or(TreeError::Memory)?)?;
            Ok(count)
        })
    } else {
        validate_fence_key(length, key_prefix, |position, utf8| {
            let bytes = span(source, root, key, position, resources)?;
            utf8.feed(bytes)?;
            Ok(bytes.len())
        })
    }
}
pub(super) fn compare(
    source: &impl BlockSource,
    root: DirectoryRoot,
    left: Key<'_>,
    right: Key<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<Ordering, TreeError> {
    resources.step(1)?;
    if root.kind != TreeKind::KeyFences {
        let (Key::Inline(left), Key::Inline(right)) = (left, right) else {
            return Err(TreeError::Invalid("overflow numeric comparator"));
        };
        return Ok(super::super::compare_inline_keys(root.kind, left, right)?);
    }
    let left_prefix = if source.scoped_blocks() {
        scoped_prefix(source, root, left, resources)?
    } else {
        prefix(source, root, left, resources)?
    };
    let right_prefix = if source.scoped_blocks() {
        scoped_prefix(source, root, right, resources)?
    } else {
        prefix(source, root, right, resources)?
    };
    let order = left_prefix.cmp(&right_prefix);
    if !order.is_eq() {
        return Ok(order);
    }
    let left_length = length(left)?;
    let right_length = length(right)?;
    if source.scoped_blocks() {
        let mut left_bytes = [0_u8; CHUNK_BYTES];
        let mut right_bytes = [0_u8; CHUNK_BYTES];
        compare_fence_key(left_length, right_length, |position, maximum| {
            let left_count = copy_scoped_span(
                source,
                root,
                left,
                position,
                left_bytes.get_mut(..maximum).ok_or(TreeError::Memory)?,
                resources,
            )?;
            let right_count = copy_scoped_span(
                source,
                root,
                right,
                position,
                right_bytes.get_mut(..maximum).ok_or(TreeError::Memory)?,
                resources,
            )?;
            let count = left_count.min(right_count).min(maximum);
            Ok((
                count,
                left_bytes
                    .get(..count)
                    .ok_or(TreeError::Memory)?
                    .cmp(right_bytes.get(..count).ok_or(TreeError::Memory)?),
            ))
        })
    } else {
        compare_fence_key(left_length, right_length, |position, maximum| {
            let a = span(source, root, left, position, resources)?;
            let b = span(source, root, right, position, resources)?;
            let count = a.len().min(b.len()).min(maximum);
            Ok((
                count,
                a.get(..count)
                    .ok_or(TreeError::Invalid("left comparator extent"))?
                    .cmp(
                        b.get(..count)
                            .ok_or(TreeError::Invalid("right comparator extent"))?,
                    ),
            ))
        })
    }
}

pub(super) fn validate_numeric_key(kind: TreeKind, key: Key<'_>) -> Result<(), TreeError> {
    let Key::Inline(bytes) = key else {
        return Err(TreeError::Invalid("overflow numeric key"));
    };
    super::super::compare_inline_keys(kind, bytes, bytes)?;
    let nonzero = |start: usize, length: usize| {
        bytes
            .get(start..start + length)
            .is_some_and(|part| part.iter().any(|byte| *byte != 0))
    };
    let valid = match kind {
        TreeKind::Nodes | TreeKind::SparseMembership => true,
        TreeKind::Relationships | TreeKind::ObjectInventory => nonzero(0, 16),
        TreeKind::SparseSources => {
            let reference = crate::property_graph::storage::artifact::decode_reference(bytes)?;
            reference.kind == BlockKind::CommitParticipant && reference.version == 1
        }
        TreeKind::Labels => nonzero(0, 8),
        TreeKind::RelationshipTypes => nonzero(0, 8) && nonzero(8, 16),
        TreeKind::OutRanges | TreeKind::InRanges => nonzero(16, 8) && nonzero(24, 16),
        TreeKind::KeyFences => false,
    };
    if valid {
        Ok(())
    } else {
        Err(TreeError::Invalid("zero numeric key component"))
    }
}

fn validate_fence_key(
    length: usize,
    _prefix: (u8, u64),
    mut consume: impl FnMut(
        usize,
        &mut crate::property_graph::storage::stream::Utf8State,
    ) -> Result<usize, TreeError>,
) -> Result<(), TreeError> {
    let mut utf8 = crate::property_graph::storage::stream::Utf8State::default();
    let mut position = 9;
    while position < length {
        let count = consume(position, &mut utf8)?;
        if count == 0 || count > length - position {
            return Err(TreeError::Invalid("short key stream"));
        }
        position = position.checked_add(count).ok_or(TreeError::Work)?;
    }
    utf8.finish()?;
    if position != length {
        return Err(TreeError::Invalid("key stream length"));
    }
    Ok(())
}

fn compare_fence_key(
    left_length: usize,
    right_length: usize,
    mut compare_span: impl FnMut(usize, usize) -> Result<(usize, Ordering), TreeError>,
) -> Result<Ordering, TreeError> {
    let end = left_length.min(right_length);
    let mut position = 9;
    while position < end {
        let (count, order) = compare_span(position, end - position)?;
        if count == 0 || count > end - position {
            return Err(TreeError::Invalid("short comparator stream"));
        }
        if !order.is_eq() {
            return Ok(order);
        }
        position += count;
    }
    Ok(left_length.cmp(&right_length))
}

pub(super) fn copy_scoped_span(
    source: &impl BlockSource,
    root: DirectoryRoot,
    key: Key<'_>,
    offset: usize,
    output: &mut [u8],
    resources: &mut TreeResources<'_>,
) -> Result<usize, TreeError> {
    match key {
        Key::Inline(bytes) => {
            let bytes = bytes
                .get(offset..)
                .ok_or(TreeError::Invalid("key read extent"))?;
            let count = bytes.len().min(output.len()).min(CHUNK_BYTES);
            resources.step(count as u64)?;
            resources.read_event(NativeReadEvent::CopiedBytes(count as u64))?;
            output
                .get_mut(..count)
                .ok_or(TreeError::Memory)?
                .copy_from_slice(bytes.get(..count).ok_or(TreeError::Invalid("key span"))?);
            Ok(count)
        }
        Key::Overflow {
            logical_length,
            reference,
        } => PayloadRef::new(BlockKind::OverflowKey, logical_length, reference)?.with_span_at(
            source,
            root.store,
            root.generation,
            offset as u64,
            output.len(),
            resources,
            |bytes, resources| {
                let count = bytes.len().min(output.len());
                output
                    .get_mut(..count)
                    .ok_or(TreeError::Memory)?
                    .copy_from_slice(bytes.get(..count).ok_or(TreeError::Invalid("key span"))?);
                resources.read_event(NativeReadEvent::CopiedBytes(count as u64))?;
                Ok(count)
            },
        ),
    }
}

pub(super) fn scoped_prefix(
    source: &impl BlockSource,
    root: DirectoryRoot,
    key: Key<'_>,
    resources: &mut TreeResources<'_>,
) -> Result<(u8, u64), TreeError> {
    let mut prefix = [0_u8; 9];
    if copy_scoped_span(source, root, key, 0, &mut prefix, resources)? != prefix.len() {
        return Err(TreeError::Invalid("missing key prefix"));
    }
    let kind = prefix[0];
    let namespace = u64::from_le_bytes(
        prefix
            .get(1..9)
            .and_then(|part| part.try_into().ok())
            .ok_or(TreeError::Invalid("missing namespace symbol"))?,
    );
    if !(1..=2).contains(&kind) || namespace == 0 {
        return Err(TreeError::Invalid("key kind or namespace"));
    }
    Ok((kind, namespace))
}

pub(super) fn copy(
    source: &impl BlockSource,
    root: DirectoryRoot,
    key: Key<'_>,
    output: &mut [u8],
    resources: &mut TreeResources<'_>,
) -> Result<(), TreeError> {
    let length = length(key)?;
    if length > output.len() {
        return Err(TreeError::Memory);
    }
    let mut position = 0;
    while position < length {
        let bytes = span(source, root, key, position, resources)?;
        if bytes.is_empty() {
            return Err(TreeError::Invalid("short copied key stream"));
        }
        let end = position.checked_add(bytes.len()).ok_or(TreeError::Memory)?;
        let target = output.get_mut(position..end).ok_or(TreeError::Memory)?;
        resources.read_event(NativeReadEvent::CopiedBytes(bytes.len() as u64))?;
        target.copy_from_slice(bytes);
        position = end;
    }
    Ok(())
}
