use super::{StageError, WriteControl, WritePhase};
use std::cmp::Ordering;
pub(super) fn bytes(
    left: &[u8],
    right: &[u8],
    control: &mut WriteControl<'_>,
) -> Result<Ordering, StageError> {
    for (left, right) in left.chunks(64 * 1024).zip(right.chunks(64 * 1024)) {
        control(WritePhase::Validate)?;
        let order = left.cmp(right);
        if order != Ordering::Equal {
            return Ok(order);
        }
    }
    Ok(left.len().cmp(&right.len()))
}
pub(super) fn sort<T>(
    values: &mut [T],
    control: &mut WriteControl<'_>,
    mut compare: impl FnMut(&T, &T, &mut WriteControl<'_>) -> Result<Ordering, StageError>,
) -> Result<(), StageError> {
    for root in (0..values.len() / 2).rev() {
        sift(values, root, values.len(), control, &mut compare)?;
    }
    for end in (1..values.len()).rev() {
        control(WritePhase::Validate)?;
        values.swap(0, end);
        sift(values, 0, end, control, &mut compare)?;
    }
    Ok(())
}
fn sift<T>(
    values: &mut [T],
    mut root: usize,
    end: usize,
    control: &mut WriteControl<'_>,
    compare: &mut impl FnMut(&T, &T, &mut WriteControl<'_>) -> Result<Ordering, StageError>,
) -> Result<(), StageError> {
    while root < end / 2 {
        control(WritePhase::Validate)?;
        let mut child = root
            .checked_mul(2)
            .and_then(|v| v.checked_add(1))
            .ok_or(StageError::Limit)?;
        let get = |n| values.get(n).ok_or(StageError::InvalidInput);
        if child + 1 < end && compare(get(child)?, get(child + 1)?, control)? == Ordering::Less {
            child += 1;
        }
        if compare(get(root)?, get(child)?, control)? != Ordering::Less {
            break;
        }
        values.swap(root, child);
        root = child;
    }
    Ok(())
}
