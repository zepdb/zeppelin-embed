//! Bounded exact top-k selection.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use super::ScanCandidate;
use crate::lifecycle::{StoreError, stats::AccountedCounter};

#[derive(Clone, Copy, Debug, PartialEq)]
struct HeapCandidate(ScanCandidate);

impl Eq for HeapCandidate {}

impl Ord for HeapCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .0
            .score
            .total_cmp(&self.0.score)
            .then_with(|| self.0.row_id.cmp(&other.0.row_id))
    }
}

impl PartialOrd for HeapCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A bounded score collector whose capacity is fixed at construction.
///
/// The competitive heap stays fixed-capacity. Candidates tied with its score
/// boundary are retained separately and may allocate so a caller with document
/// identity can perform the only tie-discarding cut. Physical exact-k callers
/// still receive equal scores in ascending row-id order.
pub(crate) struct BoundedTopK {
    limit: usize,
    heap: BinaryHeap<HeapCandidate>,
    boundary: Vec<ScanCandidate>,
}

impl BoundedTopK {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            limit,
            heap: BinaryHeap::with_capacity(limit),
            boundary: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, candidate: ScanCandidate) {
        let result = self.push_with_reserve(candidate, |_| Ok::<_, std::convert::Infallible>(()));
        match result {
            Ok(()) => {}
            Err(never) => match never {},
        }
    }

    /// The caller owns the charge through consumption of the returned candidates.
    pub(crate) fn try_new(limit: usize, memory: &mut AccountedCounter) -> Result<Self, StoreError> {
        memory.set(candidate_bytes(limit)?)?;
        let mut heap = BinaryHeap::new();
        reserve_allocation(|| heap.try_reserve_exact(limit))
            .map_err(|_| allocation_error(limit))?;
        Ok(Self {
            limit,
            heap,
            boundary: Vec::new(),
        })
    }

    // Exact scans update this heap for every eligible row, including rejects.
    #[inline(always)]
    pub(crate) fn try_push(
        &mut self,
        candidate: ScanCandidate,
        memory: &mut AccountedCounter,
    ) -> Result<(), StoreError> {
        let heap_capacity = self.heap.capacity();
        self.push_with_reserve(candidate, |boundary| {
            if boundary.len() == boundary.capacity() {
                let capacity = boundary
                    .capacity()
                    .max(1)
                    .checked_mul(2)
                    .ok_or_else(|| allocation_error(usize::MAX))?;
                let total = heap_capacity
                    .checked_add(capacity)
                    .ok_or_else(|| allocation_error(usize::MAX))?;
                memory.set(candidate_bytes(total)?)?;
                reserve_allocation(|| boundary.try_reserve_exact(capacity - boundary.len()))
                    .map_err(|_| allocation_error(capacity))?;
            }
            Ok(())
        })
    }

    pub(crate) fn capacity(&self) -> usize {
        self.heap
            .capacity()
            .saturating_add(self.boundary.capacity())
    }

    pub(crate) fn try_into_sorted_with_ties(
        mut self,
        memory: &mut AccountedCounter,
    ) -> Result<(Vec<ScanCandidate>, usize), StoreError> {
        let len = self
            .heap
            .len()
            .checked_add(self.boundary.len())
            .ok_or_else(|| allocation_error(usize::MAX))?;
        // Reuse the tie allocation as the output; large tie groups commonly
        // already have room for the heap. Charge any growth before reserving.
        if len > self.boundary.capacity() {
            let capacity = self
                .heap
                .capacity()
                .checked_add(len)
                .ok_or_else(|| allocation_error(usize::MAX))?;
            memory.set(candidate_bytes(capacity)?)?;
            reserve_allocation(|| self.boundary.try_reserve_exact(len - self.boundary.len()))
                .map_err(|_| allocation_error(len))?;
        }
        let peak_capacity = self.capacity();
        let Self {
            heap, mut boundary, ..
        } = self;
        boundary.extend(heap.iter().map(|candidate| candidate.0));
        drop(heap);
        memory.set(candidate_bytes(boundary.capacity())?)?;
        sort_candidates(&mut boundary);
        Ok((boundary, peak_capacity))
    }

    #[inline(always)]
    fn push_with_reserve<E>(
        &mut self,
        candidate: ScanCandidate,
        mut reserve: impl FnMut(&mut Vec<ScanCandidate>) -> Result<(), E>,
    ) -> Result<(), E> {
        if self.limit == 0 {
            return Ok(());
        }
        let candidate = HeapCandidate(candidate);
        if self.heap.len() < self.limit {
            self.heap.push(candidate);
            return Ok(());
        }
        let Some(worst_score) = self.heap.peek().map(|worst| worst.0.score) else {
            self.heap.push(candidate);
            return Ok(());
        };
        match candidate.0.score.total_cmp(&worst_score) {
            Ordering::Less => {}
            Ordering::Equal => {
                reserve(&mut self.boundary)?;
                self.boundary.push(candidate.0);
            }
            Ordering::Greater => {
                let Some(evicted) = self.heap.pop() else {
                    self.heap.push(candidate);
                    return Ok(());
                };
                self.heap.push(candidate);
                let Some(new_worst_score) = self.heap.peek().map(|worst| worst.0.score) else {
                    self.boundary.clear();
                    return Ok(());
                };
                if evicted.0.score.total_cmp(&new_worst_score).is_eq() {
                    reserve(&mut self.boundary)?;
                    self.boundary.push(evicted.0);
                }
                self.boundary
                    .retain(|tied| tied.score.total_cmp(&new_worst_score).is_eq());
            }
        }
        Ok(())
    }

    pub(crate) fn into_sorted(self) -> Vec<ScanCandidate> {
        let limit = self.limit;
        let mut candidates = self.into_sorted_with_ties();
        candidates.truncate(limit);
        candidates
    }

    pub(crate) fn into_sorted_with_ties(self) -> Vec<ScanCandidate> {
        let mut candidates = self
            .heap
            .into_vec()
            .into_iter()
            .map(|candidate| candidate.0)
            .collect::<Vec<_>>();
        candidates.extend(self.boundary);
        candidates.sort_unstable_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.row_id.cmp(&right.row_id))
        });
        candidates
    }
}

/// Exact scans know the complete eligible count. When it fits in 2k slots,
/// one unordered buffer avoids heap maintenance and still uses O(k) capacity.
pub(crate) enum ExactTopK {
    Heap(BoundedTopK),
    Unordered {
        limit: usize,
        candidates: Vec<ScanCandidate>,
    },
}

impl ExactTopK {
    pub(crate) fn try_new(
        limit: usize,
        eligible: usize,
        memory: &mut AccountedCounter,
    ) -> Result<Self, StoreError> {
        let limit = limit.min(eligible);
        if eligible <= limit.saturating_mul(2) {
            memory.set(candidate_bytes(eligible)?)?;
            let mut candidates = Vec::new();
            reserve_allocation(|| candidates.try_reserve_exact(eligible))
                .map_err(|_| allocation_error(eligible))?;
            Ok(Self::Unordered { limit, candidates })
        } else {
            Ok(Self::Heap(BoundedTopK::try_new(limit, memory)?))
        }
    }

    // Keep the per-row update inline when serial scans, worker partitions and
    // the final merge share this collector. Outlining adds a call to every row.
    #[inline(always)]
    pub(crate) fn try_push(
        &mut self,
        candidate: ScanCandidate,
        memory: &mut AccountedCounter,
    ) -> Result<(), StoreError> {
        match self {
            Self::Heap(heap) => heap.try_push(candidate, memory),
            Self::Unordered { candidates, .. } => {
                if candidates.len() == candidates.capacity() {
                    return Err(allocation_error(candidates.len().saturating_add(1)));
                }
                candidates.push(candidate);
                Ok(())
            }
        }
    }

    pub(crate) fn try_into_sorted_with_ties(
        self,
        memory: &mut AccountedCounter,
    ) -> Result<(Vec<ScanCandidate>, usize), StoreError> {
        match self {
            Self::Heap(heap) => heap.try_into_sorted_with_ties(memory),
            Self::Unordered {
                limit,
                mut candidates,
            } => {
                if limit == 0 {
                    candidates.clear();
                } else if candidates.len() > limit {
                    let (_, boundary, _) = candidates
                        .select_nth_unstable_by(limit - 1, |left, right| {
                            right.score.total_cmp(&left.score)
                        });
                    let score = boundary.score;
                    candidates.retain(|candidate| !candidate.score.total_cmp(&score).is_lt());
                }
                sort_candidates(&mut candidates);
                let capacity = candidates.capacity();
                Ok((candidates, capacity))
            }
        }
    }
}

fn sort_candidates(candidates: &mut [ScanCandidate]) {
    candidates.sort_unstable_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.row_id.cmp(&right.row_id))
    });
}

fn reserve_allocation(
    operation: impl FnOnce() -> Result<(), std::collections::TryReserveError>,
) -> Result<(), std::collections::TryReserveError> {
    #[cfg(feature = "allocation-audit")]
    {
        crate::allocation_audit::attributed(operation)
    }
    #[cfg(not(feature = "allocation-audit"))]
    {
        operation()
    }
}

fn candidate_bytes(capacity: usize) -> Result<usize, StoreError> {
    capacity
        .checked_mul(std::mem::size_of::<ScanCandidate>())
        .ok_or_else(|| allocation_error(usize::MAX))
}

fn allocation_error(capacity: usize) -> StoreError {
    StoreError::AllocationFailed {
        needed: capacity.saturating_mul(std::mem::size_of::<ScanCandidate>()) as u64,
        component: "exact scan collector",
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use rand::seq::SliceRandom;

    use super::BoundedTopK;
    use crate::scan::ScanCandidate;

    #[test]
    fn astra_04_exact_collector_matches_sort_at_both_capacity_strategies() {
        use crate::lifecycle::stats::{AccountedCounter, Accounting, AllocationComponent};
        use std::sync::Arc;
        for k in [0_usize, 1, 8, 24, 47, 48, 64] {
            let mut oracle = (0..48)
                .map(|row_id| ScanCandidate {
                    row_id,
                    score: ((row_id * 17) % 7) as f32,
                })
                .collect::<Vec<_>>();
            oracle
                .sort_unstable_by(|a, b| b.score.total_cmp(&a.score).then(a.row_id.cmp(&b.row_id)));
            if k == 0 {
                oracle.clear();
            } else if let Some(score) = oracle.get(k - 1).map(|candidate| candidate.score) {
                oracle.retain(|candidate| !candidate.score.total_cmp(&score).is_lt());
            }
            for reverse in [false, true] {
                let accounting = Arc::new(Accounting::new(u64::MAX, u64::MAX));
                let mut memory = AccountedCounter::new(&accounting, AllocationComponent::Temporary)
                    .expect("counter");
                let mut collector =
                    super::ExactTopK::try_new(k, 48, &mut memory).expect("collector");
                for pos in 0..48 {
                    let row_id = if reverse { 47 - pos } else { pos };
                    collector
                        .try_push(
                            ScanCandidate {
                                row_id,
                                score: ((row_id * 17) % 7) as f32,
                            },
                            &mut memory,
                        )
                        .expect("candidate");
                }
                let (actual, _) = collector
                    .try_into_sorted_with_ties(&mut memory)
                    .expect("drain");
                assert_eq!(actual, oracle);
                assert_eq!(
                    memory.bytes(),
                    (actual.capacity() * std::mem::size_of::<ScanCandidate>()) as u64
                );
                drop(actual);
                drop(memory);
                assert_eq!(accounting.audit().expect("released").temporary_bytes, 0);
            }
        }
    }

    #[test]
    fn astra_04_collector_charges_capacity_before_allocation_and_through_drain() {
        use crate::lifecycle::{
            StoreError,
            stats::{AccountedCounter, Accounting, AllocationComponent},
        };
        use std::sync::Arc;
        let unit = std::mem::size_of::<ScanCandidate>() as u64;
        let accounting = Arc::new(Accounting::new(u64::MAX, 3 * unit));
        let mut memory =
            AccountedCounter::new(&accounting, AllocationComponent::Temporary).expect("counter");
        let mut collector = BoundedTopK::try_new(2, &mut memory).expect("heap");
        assert_eq!(accounting.audit().expect("audit").temporary_bytes, 2 * unit);
        for row_id in 0..2 {
            collector
                .try_push(ScanCandidate { row_id, score: 1.0 }, &mut memory)
                .expect("heap row");
        }
        let deny = || {
            collector.try_push(
                ScanCandidate {
                    row_id: 2,
                    score: 1.0,
                },
                &mut memory,
            )
        };
        #[cfg(feature = "allocation-audit")]
        let (error, audit) = crate::allocation_audit::audit_engine_path(deny);
        #[cfg(not(feature = "allocation-audit"))]
        let error = {
            let mut deny = deny;
            deny()
        };
        assert!(matches!(
            error,
            Err(StoreError::BudgetExceeded {
                component: "temporary",
                ..
            })
        ));
        #[cfg(feature = "allocation-audit")]
        assert_eq!(
            audit.allocations, 0,
            "deny tie capacity before calling allocator"
        );
        assert_eq!(collector.capacity(), 2);
        assert_eq!(memory.bytes(), 2 * unit);
        drop(collector);
        drop(memory);
        assert_eq!(accounting.audit().expect("released").temporary_bytes, 0);

        let accounting = Arc::new(Accounting::new(u64::MAX, u64::MAX));
        let mut memory =
            AccountedCounter::new(&accounting, AllocationComponent::Temporary).expect("counter");
        let build = || {
            let mut collector = BoundedTopK::try_new(2, &mut memory).expect("heap");
            for row_id in 0..19 {
                collector
                    .try_push(ScanCandidate { row_id, score: 1.0 }, &mut memory)
                    .expect("arbitrary ties");
                assert_eq!(memory.bytes(), collector.capacity() as u64 * unit);
            }
            collector
                .try_into_sorted_with_ties(&mut memory)
                .expect("drain")
        };
        #[cfg(feature = "allocation-audit")]
        let ((candidates, _), audit) = crate::allocation_audit::audit_engine_path(build);
        #[cfg(not(feature = "allocation-audit"))]
        let (candidates, _) = {
            let mut build = build;
            build()
        };
        #[cfg(feature = "allocation-audit")]
        {
            assert_eq!(audit.unattributed_bytes, 0);
            assert!(audit.attributed_bytes > 0);
        }
        assert_eq!(candidates.len(), 19);
        assert_eq!(memory.bytes(), candidates.capacity() as u64 * unit);
        assert_eq!(
            accounting.audit().expect("output alive").temporary_bytes,
            memory.bytes()
        );
        drop(candidates);
        drop(memory);
        assert_eq!(
            accounting.audit().expect("final release").temporary_bytes,
            0
        );
    }

    #[test]
    fn bounded_top_k_retains_every_candidate_tied_with_the_kth_score() {
        for order in [[0, 1, 2, 3, 4], [4, 3, 2, 1, 0], [2, 4, 0, 3, 1]] {
            let mut with_ties = BoundedTopK::new(2);
            let mut exact = BoundedTopK::new(2);
            for row_id in order {
                let score = [5.0, 4.0, 4.0, 4.0, 3.0][row_id];
                let candidate = ScanCandidate { row_id, score };
                with_ties.push(candidate);
                exact.push(candidate);
            }

            assert_eq!(
                with_ties
                    .into_sorted_with_ties()
                    .into_iter()
                    .map(|candidate| candidate.row_id)
                    .collect::<Vec<_>>(),
                [0, 1, 2, 3]
            );
            assert_eq!(
                exact
                    .into_sorted()
                    .into_iter()
                    .map(|candidate| candidate.row_id)
                    .collect::<Vec<_>>(),
                [0, 1]
            );
        }
    }

    #[test]
    fn seeded_boundary_tie_set_is_invariant_under_input_permutation() {
        let candidates = (0..48)
            .map(|row_id| ScanCandidate {
                row_id,
                score: ((row_id * 17) % 7) as f32,
            })
            .collect::<Vec<_>>();
        let k = 8;
        let mut expected_with_ties = candidates.clone();
        expected_with_ties.sort_unstable_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.row_id.cmp(&right.row_id))
        });
        crate::scan::truncate_to_k_with_score_ties(&mut expected_with_ties, k, |candidate| {
            candidate.score
        });
        let expected_exact = expected_with_ties
            .iter()
            .copied()
            .take(k)
            .collect::<Vec<_>>();
        let mut random = crate::test_support::seeded_rng(
            "scan::topk::tests::seeded_boundary_tie_set_is_invariant_under_input_permutation",
        );

        for _ in 0..128 {
            let mut input = candidates.clone();
            input.shuffle(&mut random);
            let mut with_ties = BoundedTopK::new(k);
            let mut exact = BoundedTopK::new(k);
            for candidate in input {
                with_ties.push(candidate);
                exact.push(candidate);
            }
            assert_eq!(with_ties.into_sorted_with_ties(), expected_with_ties);
            assert_eq!(exact.into_sorted(), expected_exact);
        }
    }
}
