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
        if self.limit == 0 {
            return;
        }
        let candidate = HeapCandidate(candidate);
        if self.heap.len() < self.limit {
            self.heap.push(candidate);
            return;
        }
        let Some(worst_score) = self.heap.peek().map(|worst| worst.0.score) else {
            self.heap.push(candidate);
            return;
        };
        match candidate.0.score.total_cmp(&worst_score) {
            Ordering::Less => {}
            Ordering::Equal => self.boundary.push(candidate.0),
            Ordering::Greater => {
                let Some(evicted) = self.heap.pop() else {
                    self.heap.push(candidate);
                    return;
                };
                self.heap.push(candidate);
                let Some(new_worst_score) = self.heap.peek().map(|worst| worst.0.score) else {
                    self.boundary.clear();
                    return;
                };
                if evicted.0.score.total_cmp(&new_worst_score).is_eq() {
                    self.boundary.push(evicted.0);
                }
                self.boundary
                    .retain(|tied| tied.score.total_cmp(&new_worst_score).is_eq());
            }
        }
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

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use rand::seq::SliceRandom;

    use super::BoundedTopK;
    use crate::scan::ScanCandidate;

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
