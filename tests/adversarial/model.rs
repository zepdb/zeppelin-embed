use std::collections::{BTreeMap, BTreeSet};

use super::program;
use zeppelin_embed::quant::{est_dot_bit4, prepare_bit4_query, quantize_bit4};

#[derive(Clone, Debug, PartialEq)]
pub struct ModelDoc {
    pub revision: u64,
    pub vector: [f32; program::DIMENSIONS],
    pub timestamp: i64,
    pub metadata: Vec<u8>,
    pub location: Location,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Location {
    Active,
    Sealed(usize),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ModelSegment {
    ids: BTreeSet<u32>,
    minimum_timestamp: i64,
    maximum_timestamp: i64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExpectedHit {
    pub doc_id: u32,
    pub revision: u64,
    pub score: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Model {
    live: BTreeMap<u32, ModelDoc>,
    deleted: BTreeSet<u32>,
    purged: BTreeSet<u32>,
    dropped: BTreeSet<u32>,
    active: BTreeSet<u32>,
    segments: Vec<ModelSegment>,
}

impl Model {
    pub fn acknowledge(&mut self, doc_id: u32, revision: u64, timestamp: i64) {
        self.deleted.remove(&doc_id);
        self.purged.remove(&doc_id);
        self.dropped.remove(&doc_id);
        self.active.insert(doc_id);
        self.live.insert(
            doc_id,
            ModelDoc {
                revision,
                vector: program::vector(doc_id, revision),
                timestamp,
                metadata: program::sentinel(doc_id),
                location: Location::Active,
            },
        );
    }

    pub fn delete(&mut self, doc_id: u32) {
        self.live.remove(&doc_id);
        self.active.remove(&doc_id);
        self.deleted.insert(doc_id);
    }

    pub fn purge(&mut self, doc_id: u32) {
        self.live.remove(&doc_id);
        self.active.remove(&doc_id);
        self.deleted.remove(&doc_id);
        self.purged.insert(doc_id);
    }

    pub fn seal(&mut self) {
        if self.active.is_empty() {
            return;
        }
        let index = self.segments.len();
        let ids = std::mem::take(&mut self.active);
        let mut minimum_timestamp = i64::MAX;
        let mut maximum_timestamp = i64::MIN;
        for id in &ids {
            if let Some(doc) = self.live.get_mut(id) {
                doc.location = Location::Sealed(index);
                minimum_timestamp = minimum_timestamp.min(doc.timestamp);
                maximum_timestamp = maximum_timestamp.max(doc.timestamp);
            }
        }
        self.segments.push(ModelSegment {
            ids,
            minimum_timestamp,
            maximum_timestamp,
        });
    }

    pub fn drop_partition(&mut self, start: i64, end: i64) {
        for (index, segment) in self.segments.iter().enumerate() {
            if segment.minimum_timestamp >= start && segment.maximum_timestamp < end {
                for id in &segment.ids {
                    if self
                        .live
                        .get(id)
                        .is_some_and(|doc| doc.location == Location::Sealed(index))
                    {
                        self.live.remove(id);
                        self.dropped.insert(*id);
                    }
                }
            }
        }
    }

    #[must_use]
    pub fn expected(&self, query: &[f32], k: usize) -> Vec<ExpectedHit> {
        let mut hits = self
            .live
            .iter()
            .map(|(&doc_id, doc)| ExpectedHit {
                doc_id,
                revision: doc.revision,
                score: -squared_l2(&doc.vector, query),
            })
            .collect::<Vec<_>>();
        hits.sort_unstable_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.doc_id.cmp(&right.doc_id))
        });
        hits.truncate(k.min(hits.len()));
        hits
    }

    #[must_use]
    pub fn expected_scan(&self, query: &[f32], k: usize) -> Vec<ExpectedHit> {
        let prepared = prepare_bit4_query(query, 0).expect("closed-vocabulary scan query");
        let mut hits = self
            .live
            .iter()
            .map(|(&doc_id, doc)| {
                let mut codes = vec![0_u8; doc.vector.len().div_ceil(2)];
                let factors =
                    quantize_bit4(&doc.vector, &mut codes).expect("closed-vocabulary document");
                let score = est_dot_bit4(&prepared, &codes, factors)
                    .expect("matching closed-vocabulary Bit4 row");
                ExpectedHit {
                    doc_id,
                    revision: doc.revision,
                    score,
                }
            })
            .collect::<Vec<_>>();
        hits.sort_unstable_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.doc_id.cmp(&right.doc_id))
        });
        hits.truncate(k.min(hits.len()));
        hits
    }

    #[must_use]
    pub fn expected_filtered(
        &self,
        query: &[f32],
        k: usize,
        maximum_timestamp: i64,
    ) -> Vec<ExpectedHit> {
        let mut hits = self
            .live
            .iter()
            .filter(|(_, doc)| doc.timestamp <= maximum_timestamp)
            .map(|(&doc_id, doc)| ExpectedHit {
                doc_id,
                revision: doc.revision,
                score: -squared_l2_f64(&doc.vector, query),
            })
            .collect::<Vec<_>>();
        hits.sort_unstable_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.doc_id.cmp(&right.doc_id))
        });
        hits.truncate(k.min(hits.len()));
        hits
    }

    #[must_use]
    pub fn expected_filtered_scan(
        &self,
        query: &[f32],
        k: usize,
        maximum_timestamp: i64,
    ) -> Vec<ExpectedHit> {
        let prepared = prepare_bit4_query(query, 0).expect("closed-vocabulary scan query");
        let mut hits = self
            .live
            .iter()
            .filter(|(_, doc)| doc.timestamp <= maximum_timestamp)
            .map(|(&doc_id, doc)| {
                let mut codes = vec![0_u8; doc.vector.len().div_ceil(2)];
                let factors =
                    quantize_bit4(&doc.vector, &mut codes).expect("closed-vocabulary document");
                let score = est_dot_bit4(&prepared, &codes, factors)
                    .expect("matching closed-vocabulary Bit4 row");
                ExpectedHit {
                    doc_id,
                    revision: doc.revision,
                    score,
                }
            })
            .collect::<Vec<_>>();
        hits.sort_unstable_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.doc_id.cmp(&right.doc_id))
        });
        hits.truncate(k.min(hits.len()));
        hits
    }

    #[must_use]
    pub fn live_ids(&self) -> BTreeSet<u32> {
        self.live.keys().copied().collect()
    }

    #[must_use]
    pub fn forbidden_ids(&self) -> BTreeSet<u32> {
        self.deleted
            .union(&self.purged)
            .copied()
            .chain(self.dropped.iter().copied())
            .collect()
    }

    #[must_use]
    pub fn revision(&self, doc_id: u32) -> Option<u64> {
        self.live.get(&doc_id).map(|doc| doc.revision)
    }

    #[must_use]
    pub fn timestamp(&self, doc_id: u32) -> Option<i64> {
        self.live.get(&doc_id).map(|doc| doc.timestamp)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.live.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }
}

fn squared_l2(left: &[f32], right: &[f32]) -> f32 {
    left.iter()
        .zip(right)
        .map(|(left, right)| {
            let delta = *left - *right;
            delta * delta
        })
        .sum()
}

fn squared_l2_f64(left: &[f32], right: &[f32]) -> f32 {
    left.iter()
        .zip(right)
        .map(|(left, right)| {
            let delta = f64::from(*left) - f64::from(*right);
            delta * delta
        })
        .sum::<f64>() as f32
}
