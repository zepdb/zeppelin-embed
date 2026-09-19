//! Fallible in-place ordering with a checkpoint at every heap operation and
//! every 64 KiB of exact name bytes. Cancellation may leave scratch reordered.
use super::{ApplicationKey, CanonicalError, EntityShape, KeyLifecycleError};
use std::cmp::Ordering;

type Checkpoint<'a> = dyn FnMut() -> Result<(), CanonicalError> + 'a;

fn bytes(
    left: &[u8],
    right: &[u8],
    checkpoint: &mut Checkpoint<'_>,
) -> Result<Ordering, CanonicalError> {
    for (left, right) in left.chunks(64 * 1024).zip(right.chunks(64 * 1024)) {
        checkpoint()?;
        let order = left.cmp(right);
        if order != Ordering::Equal {
            return Ok(order);
        }
    }
    Ok(left.len().cmp(&right.len()))
}

pub(super) fn keys(
    left: Option<ApplicationKey<'_>>,
    right: Option<ApplicationKey<'_>>,
    checkpoint: &mut Checkpoint<'_>,
) -> Result<Ordering, CanonicalError> {
    checkpoint()?;
    let (left, right) = match (left, right) {
        (None, None) => return Ok(Ordering::Equal),
        (None, Some(_)) => return Ok(Ordering::Less),
        (Some(_), None) => return Ok(Ordering::Greater),
        (Some(left), Some(right)) => (left, right),
    };
    let kind = left.kind().cmp(&right.kind());
    if kind != Ordering::Equal {
        return Ok(kind);
    }
    let namespace = bytes(
        left.namespace().as_str().as_bytes(),
        right.namespace().as_str().as_bytes(),
        checkpoint,
    )?;
    if namespace != Ordering::Equal {
        return Ok(namespace);
    }
    bytes(
        left.key().as_str().as_bytes(),
        right.key().as_str().as_bytes(),
        checkpoint,
    )
}

pub(super) fn shapes(
    left: EntityShape<'_>,
    right: EntityShape<'_>,
    checkpoint: &mut Checkpoint<'_>,
) -> Result<bool, CanonicalError> {
    checkpoint()?;
    match (left, right) {
        (EntityShape::Node, EntityShape::Node) => Ok(true),
        (
            EntityShape::Relationship {
                source: ls,
                target: lt,
                relationship_type: ln,
            },
            EntityShape::Relationship {
                source: rs,
                target: rt,
                relationship_type: rn,
            },
        ) => Ok(ls == rs
            && lt == rt
            && bytes(ln.as_str().as_bytes(), rn.as_str().as_bytes(), checkpoint)?
                == Ordering::Equal),
        _ => Ok(false),
    }
}

pub(super) fn sort<T>(
    values: &mut [T],
    checkpoint: &mut Checkpoint<'_>,
    mut compare: impl FnMut(&T, &T, &mut Checkpoint<'_>) -> Result<Ordering, CanonicalError>,
) -> Result<(), KeyLifecycleError> {
    for root in (0..values.len() / 2).rev() {
        sift(values, root, values.len(), checkpoint, &mut compare)?;
    }
    for end in (1..values.len()).rev() {
        checkpoint()?;
        values.swap(0, end);
        sift(values, 0, end, checkpoint, &mut compare)?;
    }
    Ok(())
}

fn sift<T>(
    values: &mut [T],
    mut root: usize,
    end: usize,
    checkpoint: &mut Checkpoint<'_>,
    compare: &mut impl FnMut(&T, &T, &mut Checkpoint<'_>) -> Result<Ordering, CanonicalError>,
) -> Result<(), KeyLifecycleError> {
    // root < end <= the admitted 16,384 descriptors; multiplication cannot wrap.
    while root < end / 2 {
        checkpoint()?;
        let mut child = root * 2 + 1;
        let get = |index| values.get(index).ok_or(KeyLifecycleError::InvalidState);
        if child + 1 < end && compare(get(child)?, get(child + 1)?, checkpoint)? == Ordering::Less {
            child += 1;
        }
        if compare(get(root)?, get(child)?, checkpoint)? != Ordering::Less {
            break;
        }
        checkpoint()?;
        values.swap(root, child);
        root = child;
    }
    Ok(())
}
