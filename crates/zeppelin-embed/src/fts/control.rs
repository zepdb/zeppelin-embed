//! Cooperative checkpoints shared by bounded lexical work loops.

/// A work unit is a cursor, posting, or metadata probe, including probes that
/// do not produce a match. The caller also checks at stage entry and exit.
/// Generic infallible callbacks let uncontrolled callers erase these checks.
pub(crate) struct WorkCheck<F> {
    check: F,
    remaining: u8,
}

impl<E, F: FnMut() -> Result<(), E>> WorkCheck<F> {
    pub(crate) fn new(check: F) -> Self {
        Self {
            check,
            remaining: 64,
        }
    }

    #[inline]
    pub(crate) fn step(&mut self) -> Result<(), E> {
        self.remaining -= 1;
        if self.remaining == 0 {
            self.remaining = 64;
            (self.check)()?;
        }
        Ok(())
    }

    #[inline]
    pub(crate) fn check_now(&mut self) -> Result<(), E> {
        (self.check)()
    }
}

/// Bound individual host copies while keeping the caller's allocation policy.
pub(crate) fn extend_bytes<E>(
    output: &mut Vec<u8>,
    bytes: &[u8],
    work: &mut WorkCheck<impl FnMut() -> Result<(), E>>,
) -> Result<(), E> {
    work.check_now()?;
    output.reserve(bytes.len());
    for chunk in bytes.chunks(8_192) {
        work.check_now()?;
        output.extend_from_slice(chunk);
    }
    work.check_now()
}

/// In-place fallible heapsort: cancellation leaves a valid permutation and
/// requires no additional retained/scratch allocation. Private heap indices
/// are derived only from this slice's length. Ordering need not be stable.
pub(crate) fn sort_by<T, E>(
    values: &mut [T],
    mut compare: impl FnMut(&T, &T) -> std::cmp::Ordering,
    work: &mut WorkCheck<impl FnMut() -> Result<(), E>>,
) -> Result<(), E> {
    work.check_now()?;
    for root in (0..values.len() / 2).rev() {
        work.step()?;
        sift_down(values, root, &mut compare, work)?;
    }
    for end in (1..values.len()).rev() {
        work.step()?;
        values.swap(0, end);
        sift_down(values.split_at_mut(end).0, 0, &mut compare, work)?;
    }
    work.check_now()
}

fn sift_down<T, E>(
    values: &mut [T],
    mut root: usize,
    compare: &mut impl FnMut(&T, &T) -> std::cmp::Ordering,
    work: &mut WorkCheck<impl FnMut() -> Result<(), E>>,
) -> Result<(), E> {
    loop {
        let mut child = root.saturating_mul(2).saturating_add(1);
        if child >= values.len() {
            return Ok(());
        }
        work.step()?;
        if values
            .get(child)
            .zip(values.get(child + 1))
            .is_some_and(|(left, right)| compare(left, right).is_lt())
        {
            child += 1;
        }
        work.step()?;
        if !values
            .get(root)
            .zip(values.get(child))
            .is_some_and(|(parent, child)| compare(parent, child).is_lt())
        {
            return Ok(());
        }
        values.swap(root, child);
        root = child;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn astra_18_preparation_sort_cancels_inside_comparisons() {
        let input = (0..4_096).map(|i| (i * 7_919) % 4_096).collect::<Vec<_>>();
        let mut values = input.clone();
        let comparisons = std::cell::Cell::new(0);
        let mut work = WorkCheck::new(|| {
            if comparisons.get() >= 64 {
                Err("cancelled")
            } else {
                Ok(())
            }
        });
        let result = sort_by(
            &mut values,
            |a, b| {
                comparisons.set(comparisons.get() + 1);
                a.cmp(b)
            },
            &mut work,
        );
        println!("comparisons={}", comparisons.get());
        assert_eq!(result, Err("cancelled"));
        assert!(
            comparisons.get() <= 128,
            "sort must stop within a bounded number of comparisons"
        );
        // Cancellation leaves every owned value intact, even in a partial order.
        values.sort_unstable();
        assert_eq!(values, (0..4_096).collect::<Vec<_>>());
        let mut work = WorkCheck::new(|| Ok::<(), ()>(()));
        let mut clean = input;
        assert_eq!(sort_by(&mut clean, Ord::cmp, &mut work), Ok(()));
        assert_eq!(clean, values);
    }
}
