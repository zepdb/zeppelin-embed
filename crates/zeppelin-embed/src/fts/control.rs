//! Cooperative checkpoints shared by bounded lexical work loops.

use std::marker::PhantomData;

/// A work unit is a cursor, posting, or metadata probe, including probes that
/// do not produce a match. The caller also checks at stage entry and exit.
/// Generic infallible callbacks let uncontrolled callers erase these checks.
pub(crate) struct WorkCheck<F> {
    check: F,
    remaining: u8,
}

/// Allocation and work policy for lexical build owners. Implementations must
/// reserve before allocating, reconcile the allocator's actual capacity, and
/// keep the returned charge live until after its backing is dropped.
pub(crate) trait BuildPolicy<'m> {
    type Error;
    type Charge;

    fn checkpoint(&mut self) -> Result<(), Self::Error>;
    fn step(&mut self, units: u64) -> Result<(), Self::Error>;
    fn lexical_block(&mut self) -> Result<(), Self::Error> {
        self.step(1)
    }
    fn lexical_posting(&mut self) -> Result<(), Self::Error> {
        self.step(1)
    }
    /// Accounts one actual initialized-memory copy immediately before it is
    /// performed. The caller has already proved the destination capacity.
    fn copy_step(&mut self, bytes: usize) -> Result<(), Self::Error>;
    fn allocate_vec<T>(&mut self, capacity: usize) -> Result<(Vec<T>, Self::Charge), Self::Error>;
    fn allocate_string(&mut self, capacity: usize) -> Result<(String, Self::Charge), Self::Error>;
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TestStage {
    Work,
    Analysis,
    Sort,
    PositionsEncode,
    RegionEncode,
    PositionsDecode,
    RegionDecode,
}

#[cfg(test)]
std::thread_local! {
    static TEST_STAGE_HOOK: std::cell::RefCell<Option<Box<dyn FnMut(TestStage)>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn test_stage_probe(stage: TestStage) {
    TEST_STAGE_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().as_mut() {
            hook(stage);
        }
    });
}

#[cfg(test)]
pub(crate) fn with_test_stage_hook<R>(
    hook: impl FnMut(TestStage) + 'static,
    action: impl FnOnce() -> R,
) -> R {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_STAGE_HOOK.with(|slot| {
                let _ = slot.borrow_mut().take();
            });
        }
    }

    TEST_STAGE_HOOK.with(|slot| {
        assert!(
            slot.borrow().is_none(),
            "test stage hook is already installed"
        );
        *slot.borrow_mut() = Some(Box::new(hook));
    });
    let reset = Reset;
    let result = action();
    drop(reset);
    result
}

pub(crate) struct LegacyPolicy;

impl BuildPolicy<'static> for LegacyPolicy {
    type Error = std::convert::Infallible;
    type Charge = ();

    fn checkpoint(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
    fn step(&mut self, _units: u64) -> Result<(), Self::Error> {
        Ok(())
    }
    fn copy_step(&mut self, _bytes: usize) -> Result<(), Self::Error> {
        Ok(())
    }
    fn allocate_vec<T>(&mut self, capacity: usize) -> Result<(Vec<T>, Self::Charge), Self::Error> {
        Ok((Vec::with_capacity(capacity), ()))
    }
    fn allocate_string(&mut self, capacity: usize) -> Result<(String, Self::Charge), Self::Error> {
        Ok((String::with_capacity(capacity), ()))
    }
}

/// A Vec whose complete actual backing remains paired with its authentic
/// capacity charge. Field order is intentional: backing drops before charge.
pub(crate) struct GuardedVec<'m, T, C> {
    values: Vec<T>,
    charge: C,
    marker: PhantomData<&'m ()>,
}

impl<'m, T, C> GuardedVec<'m, T, C> {
    pub(crate) fn with_capacity<P>(policy: &mut P, capacity: usize) -> Result<Self, P::Error>
    where
        P: BuildPolicy<'m, Charge = C>,
    {
        policy.checkpoint()?;
        let (values, charge) = policy.allocate_vec(capacity)?;
        Ok(Self {
            values,
            charge,
            marker: PhantomData,
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.values.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn actual_capacity_bytes(&self) -> usize {
        self.values
            .capacity()
            .saturating_mul(std::mem::size_of::<T>())
    }

    pub(crate) fn as_slice(&self) -> &[T] {
        &self.values
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.values
    }

    pub(crate) fn first(&self) -> Option<&T> {
        self.values.first()
    }

    pub(crate) fn get(&self, index: usize) -> Option<&T> {
        self.values.get(index)
    }

    pub(crate) fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        self.values.get_mut(index)
    }

    pub(crate) fn push<P>(&mut self, value: T, policy: &mut P) -> Result<(), P::Error>
    where
        P: BuildPolicy<'m, Charge = C>,
    {
        if self.values.len() == self.values.capacity() {
            self.grow(policy)?;
        }
        policy.copy_step(std::mem::size_of::<T>())?;
        self.values.push(value);
        Ok(())
    }

    fn grow<P>(&mut self, policy: &mut P) -> Result<(), P::Error>
    where
        P: BuildPolicy<'m, Charge = C>,
    {
        let next = self.values.capacity().max(1).saturating_mul(2);
        let mut replacement = Self::with_capacity(policy, next)?;
        for value in self.values.drain(..) {
            policy.step(1)?;
            policy.copy_step(std::mem::size_of::<T>())?;
            replacement.values.push(value);
        }
        std::mem::swap(self, &mut replacement);
        Ok(())
    }

    pub(crate) fn into_parts(self) -> (Vec<T>, C) {
        (self.values, self.charge)
    }

    pub(crate) fn extend_from_slice<P>(
        &mut self,
        values: &[T],
        policy: &mut P,
    ) -> Result<(), P::Error>
    where
        T: Copy,
        P: BuildPolicy<'m, Charge = C>,
    {
        for chunk in values.chunks(
            8_192_usize
                .saturating_div(std::mem::size_of::<T>().max(1))
                .max(1),
        ) {
            for value in chunk {
                self.push(*value, policy)?;
            }
        }
        Ok(())
    }

    pub(crate) fn remove_first<P>(&mut self, policy: &mut P) -> Result<Option<T>, P::Error>
    where
        P: BuildPolicy<'m, Charge = C>,
    {
        if self.values.is_empty() {
            return Ok(None);
        }
        let shifted = self.values.len().saturating_sub(1);
        policy.copy_step(shifted.saturating_mul(std::mem::size_of::<T>()))?;
        Ok(Some(self.values.remove(0)))
    }
}

pub(crate) trait CapacityCharge {
    fn bytes(&self) -> usize;
}

impl CapacityCharge for () {
    fn bytes(&self) -> usize {
        0
    }
}

/// A String whose allocator capacity is paired with its authentic charge.
pub(crate) struct GuardedString<'m, C> {
    value: String,
    charge: C,
    marker: PhantomData<&'m ()>,
}

impl<'m, C> GuardedString<'m, C> {
    pub(crate) fn with_capacity<P>(policy: &mut P, capacity: usize) -> Result<Self, P::Error>
    where
        P: BuildPolicy<'m, Charge = C>,
    {
        policy.checkpoint()?;
        let (owned, charge) = policy.allocate_string(capacity)?;
        Ok(Self {
            value: owned,
            charge,
            marker: PhantomData,
        })
    }

    pub(crate) fn copy_from<P>(policy: &mut P, value: &str) -> Result<Self, P::Error>
    where
        P: BuildPolicy<'m, Charge = C>,
    {
        let mut guarded = Self::with_capacity(policy, value.len())?;
        for character in value.chars() {
            let bytes = character.len_utf8();
            policy.copy_step(bytes)?;
            guarded.value.push(character);
            #[cfg(test)]
            test_stage_probe(TestStage::Analysis);
        }
        Ok(guarded)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.value
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn actual_capacity_bytes(&self) -> usize {
        self.value.capacity()
    }

    pub(crate) fn push_str<P>(&mut self, policy: &mut P, value: &str) -> Result<(), P::Error>
    where
        P: BuildPolicy<'m, Charge = C>,
    {
        let needed = self.value.len().saturating_add(value.len());
        if needed > self.value.capacity() {
            self.grow(policy, needed)?;
        }
        for character in value.chars() {
            let bytes = character.len_utf8();
            policy.copy_step(bytes)?;
            self.value.push(character);
        }
        Ok(())
    }

    pub(crate) fn push_char<P>(&mut self, policy: &mut P, value: char) -> Result<(), P::Error>
    where
        P: BuildPolicy<'m, Charge = C>,
    {
        let mut encoded = [0_u8; 4];
        self.push_str(policy, value.encode_utf8(&mut encoded))
    }

    fn grow<P>(&mut self, policy: &mut P, needed: usize) -> Result<(), P::Error>
    where
        P: BuildPolicy<'m, Charge = C>,
    {
        let next = self.value.capacity().max(1).saturating_mul(2).max(needed);
        let mut replacement = Self::with_capacity(policy, next)?;
        for character in self.value.chars() {
            let bytes = character.len_utf8();
            policy.copy_step(bytes)?;
            replacement.value.push(character);
        }
        std::mem::swap(self, &mut replacement);
        Ok(())
    }

    pub(crate) fn into_parts(self) -> (String, C) {
        (self.value, self.charge)
    }
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

/// Allocation-free heapsort whose comparator can itself fail while polling a
/// long byte comparison through the same policy.
pub(crate) fn sort_by_policy<'m, T, P: BuildPolicy<'m>>(
    values: &mut [T],
    policy: &mut P,
    mut compare: impl FnMut(&T, &T, &mut P) -> Result<std::cmp::Ordering, P::Error>,
) -> Result<(), P::Error> {
    policy.checkpoint()?;
    for root in (0..values.len() / 2).rev() {
        sift_down_policy(values, root, policy, &mut compare)?;
    }
    for end in (1..values.len()).rev() {
        policy.step(1)?;
        values.swap(0, end);
        sift_down_policy(values.split_at_mut(end).0, 0, policy, &mut compare)?;
    }
    policy.checkpoint()
}

fn sift_down_policy<'m, T, P: BuildPolicy<'m>>(
    values: &mut [T],
    mut root: usize,
    policy: &mut P,
    compare: &mut impl FnMut(&T, &T, &mut P) -> Result<std::cmp::Ordering, P::Error>,
) -> Result<(), P::Error> {
    loop {
        let mut child = root.saturating_mul(2).saturating_add(1);
        if child >= values.len() {
            return Ok(());
        }
        policy.step(1)?;
        if let (Some(left), Some(right)) = (values.get(child), values.get(child + 1)) {
            let order = compare(left, right, policy)?;
            #[cfg(test)]
            test_stage_probe(TestStage::Sort);
            if order.is_lt() {
                child = child.saturating_add(1);
            }
        }
        policy.step(1)?;
        let Some(parent) = values.get(root) else {
            return Ok(());
        };
        let Some(selected) = values.get(child) else {
            return Ok(());
        };
        let order = compare(parent, selected, policy)?;
        #[cfg(test)]
        test_stage_probe(TestStage::Sort);
        if !order.is_lt() {
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
