//! Bounded exact top-k selection.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use super::ScanCandidate;

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
/// Once constructed, [`Self::push`] performs no allocation. Equal scores are
/// permanently ranked by ascending row id; every scan and future parallel
/// merge must preserve that parity contract.
pub(crate) struct BoundedTopK {
    limit: usize,
    heap: BinaryHeap<HeapCandidate>,
}

impl BoundedTopK {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            limit,
            heap: BinaryHeap::with_capacity(limit),
        }
    }

    pub(crate) fn push(&mut self, candidate: ScanCandidate) {
        if self.limit == 0 {
            return;
        }
        let candidate = HeapCandidate(candidate);
        if self.heap.len() < self.limit {
            self.heap.push(candidate);
            return;
        }
        if self
            .heap
            .peek()
            .is_some_and(|worst| candidate.cmp(worst).is_lt())
        {
            let _ = self.heap.pop();
            self.heap.push(candidate);
        }
    }

    pub(crate) fn is_full(&self) -> bool {
        self.heap.len() == self.limit && self.limit != 0
    }

    pub(crate) fn worst(&self) -> Option<ScanCandidate> {
        self.heap.peek().map(|candidate| candidate.0)
    }

    pub(crate) fn into_sorted(self) -> Vec<ScanCandidate> {
        let mut candidates = self
            .heap
            .into_vec()
            .into_iter()
            .map(|candidate| candidate.0)
            .collect::<Vec<_>>();
        candidates.sort_unstable_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.row_id.cmp(&right.row_id))
        });
        candidates
    }
}
