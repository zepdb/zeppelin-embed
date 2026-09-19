//! Bounded cancellable comparisons and allocation-free descriptor sorting.
use super::CatalogError;
use std::cmp::Ordering;

pub(super) const CHUNK: usize = 64 * 1024;
pub(super) type Checkpoint<'a> = &'a mut dyn FnMut() -> Result<(), CatalogError>;

pub(super) fn compare_bytes(
    left: &[u8],
    right: &[u8],
    checkpoint: Checkpoint<'_>,
) -> Result<Ordering, CatalogError> {
    checkpoint()?;
    for (left, right) in left.chunks(CHUNK).zip(right.chunks(CHUNK)) {
        checkpoint()?;
        let order = left.cmp(right);
        if order != Ordering::Equal {
            return Ok(order);
        }
    }
    Ok(left.len().cmp(&right.len()))
}

pub(super) fn sort<T: Copy>(
    values: &mut [T],
    checkpoint: Checkpoint<'_>,
    compare: impl Fn(&T, &T, Checkpoint<'_>) -> Result<Ordering, CatalogError>,
) -> Result<(), CatalogError> {
    for root in (0..values.len() / 2).rev() {
        sift(values, root, values.len(), checkpoint, &compare)?;
    }
    for end in (1..values.len()).rev() {
        checkpoint()?;
        swap(values, 0, end)?;
        sift(values, 0, end, checkpoint, &compare)?;
    }
    Ok(())
}
fn sift<T: Copy>(
    values: &mut [T],
    mut root: usize,
    end: usize,
    checkpoint: Checkpoint<'_>,
    compare: &impl Fn(&T, &T, Checkpoint<'_>) -> Result<Ordering, CatalogError>,
) -> Result<(), CatalogError> {
    loop {
        checkpoint()?;
        let Some(mut child) = root
            .checked_mul(2)
            .and_then(|v| v.checked_add(1))
            .filter(|child| *child < end)
        else {
            return Ok(());
        };
        if child + 1 < end
            && compare(
                values.get(child).ok_or(CatalogError::Malformed)?,
                values.get(child + 1).ok_or(CatalogError::Malformed)?,
                checkpoint,
            )? == Ordering::Less
        {
            child += 1;
        }
        if compare(
            values.get(root).ok_or(CatalogError::Malformed)?,
            values.get(child).ok_or(CatalogError::Malformed)?,
            checkpoint,
        )? != Ordering::Less
        {
            return Ok(());
        }
        swap(values, root, child)?;
        root = child;
    }
}
fn swap<T>(values: &mut [T], left: usize, right: usize) -> Result<(), CatalogError> {
    if left == right {
        return Ok(());
    }
    let range = values
        .get_mut(left.min(right)..=left.max(right))
        .ok_or(CatalogError::Malformed)?;
    let (first, tail) = range.split_first_mut().ok_or(CatalogError::Malformed)?;
    let last = tail.last_mut().ok_or(CatalogError::Malformed)?;
    std::mem::swap(first, last);
    Ok(())
}

pub(super) fn utf8<'a>(
    bytes: &'a [u8],
    checkpoint: Checkpoint<'_>,
) -> Result<&'a str, CatalogError> {
    let mut remaining = bytes;
    while !remaining.is_empty() {
        checkpoint()?;
        let length = remaining.len().min(CHUNK);
        let part = remaining.get(..length).ok_or(CatalogError::Malformed)?;
        let consumed = match std::str::from_utf8(part) {
            Ok(_) => length,
            Err(error) if error.error_len().is_none() && length < remaining.len() => {
                error.valid_up_to()
            }
            Err(_) => return Err(CatalogError::Malformed),
        };
        if consumed == 0 {
            return Err(CatalogError::Malformed);
        }
        remaining = remaining.get(consumed..).ok_or(CatalogError::Malformed)?;
    }
    // SAFETY: every byte was validated in complete UTF-8 spans above. A partial
    // code point at a chunk boundary remains in `remaining` for the next check.
    // The immutable borrowed bytes cannot change between validation and this cast.
    Ok(unsafe { std::str::from_utf8_unchecked(bytes) })
}
