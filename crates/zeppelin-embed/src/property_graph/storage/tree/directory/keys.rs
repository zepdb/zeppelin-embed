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
        let Key::Inline(bytes) = key else {
            return Err(TreeError::Invalid("overflow numeric key"));
        };
        super::super::compare_inline_keys(root.kind, bytes, bytes)?;
        let nonzero = |start: usize, length: usize| {
            bytes
                .get(start..start + length)
                .is_some_and(|part| part.iter().any(|byte| *byte != 0))
        };
        let valid = match root.kind {
            TreeKind::Nodes | TreeKind::Relationships | TreeKind::ObjectInventory => nonzero(0, 16),
            TreeKind::Labels | TreeKind::RelationshipTypes => nonzero(0, 8) && nonzero(8, 16),
            TreeKind::OutRanges | TreeKind::InRanges => {
                nonzero(0, 16) && nonzero(16, 8) && nonzero(24, 16)
            }
            TreeKind::KeyFences => false,
        };
        return if valid {
            Ok(())
        } else {
            Err(TreeError::Invalid("zero numeric key component"))
        };
    }
    let length = length(key)?;
    if !(9..=crate::property_graph::MAX_GRAPH_INPUT_BYTES).contains(&length) {
        return Err(TreeError::Invalid("key logical length"));
    }
    prefix(source, root, key, resources)?;
    let mut utf8 = crate::property_graph::storage::stream::Utf8State::default();
    let mut position = 9;
    while position < length {
        let bytes = span(source, root, key, position, resources)?;
        if bytes.is_empty() {
            return Err(TreeError::Invalid("short key stream"));
        }
        utf8.feed(bytes)?;
        position = position.checked_add(bytes.len()).ok_or(TreeError::Work)?;
    }
    utf8.finish()?;
    if position != length {
        return Err(TreeError::Invalid("key stream length"));
    }
    Ok(())
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
    let order =
        prefix(source, root, left, resources)?.cmp(&prefix(source, root, right, resources)?);
    if !order.is_eq() {
        return Ok(order);
    }
    let left_length = length(left)?;
    let right_length = length(right)?;
    let end = left_length.min(right_length);
    let mut position = 9;
    while position < end {
        let a = span(source, root, left, position, resources)?;
        let b = span(source, root, right, position, resources)?;
        let length = a.len().min(b.len()).min(end - position);
        if length == 0 {
            return Err(TreeError::Invalid("short comparator stream"));
        }
        let order = a
            .get(..length)
            .ok_or(TreeError::Invalid("left comparator extent"))?
            .cmp(
                b.get(..length)
                    .ok_or(TreeError::Invalid("right comparator extent"))?,
            );
        if !order.is_eq() {
            return Ok(order);
        }
        position += length;
    }
    Ok(left_length.cmp(&right_length))
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
