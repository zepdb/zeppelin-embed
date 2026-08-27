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

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExpectedLexicalHit {
    pub doc_id: u32,
    pub revision: u64,
    pub score: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExpectedHybridHit {
    pub doc_id: u32,
    pub vector_squared_l2: Option<f64>,
    pub lexical_bm25: Option<f64>,
    pub fused_score: f64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ModelEpoch {
    #[default]
    A,
    B,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Model {
    live: BTreeMap<u32, ModelDoc>,
    deleted: BTreeSet<u32>,
    purged: BTreeSet<u32>,
    dropped: BTreeSet<u32>,
    active: BTreeSet<u32>,
    segments: Vec<ModelSegment>,
    published_epoch: ModelEpoch,
    epoch_b_prepared: bool,
    epoch_a_dropped: bool,
}

impl Model {
    pub fn prepare_epoch_b(&mut self) {
        self.epoch_b_prepared = true;
    }

    pub fn switch_epoch(&mut self, epoch: ModelEpoch) -> bool {
        let available = match epoch {
            ModelEpoch::A => !self.epoch_a_dropped,
            ModelEpoch::B => self.epoch_b_prepared,
        };
        if available {
            self.published_epoch = epoch;
        }
        available
    }

    pub fn drop_epoch_a(&mut self) -> bool {
        if self.published_epoch == ModelEpoch::A || self.epoch_a_dropped {
            return false;
        }
        self.epoch_a_dropped = true;
        true
    }

    #[must_use]
    pub const fn published_epoch(&self) -> ModelEpoch {
        self.published_epoch
    }

    #[must_use]
    pub const fn epoch_b_prepared(&self) -> bool {
        self.epoch_b_prepared
    }

    #[must_use]
    pub fn live_documents(&self) -> Vec<(u32, u64, i64)> {
        self.live
            .iter()
            .map(|(doc_id, document)| (*doc_id, document.revision, document.timestamp))
            .collect()
    }

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
    pub fn expected_exact(&self, query: &[f32], k: usize) -> Vec<ExpectedHit> {
        let mut hits = self
            .live
            .iter()
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
    pub fn expected_predicate(
        &self,
        query: &[f32],
        k: usize,
        predicate: program::PredicateKind,
    ) -> Vec<ExpectedHit> {
        let mut hits = self
            .live
            .iter()
            .filter(|(doc_id, _)| predicate_matches(**doc_id, predicate))
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
    pub fn lexical_documents(&self) -> Vec<(u32, u64)> {
        self.live
            .iter()
            .map(|(doc_id, document)| (*doc_id, document.revision))
            .collect()
    }

    /// Independently scores the harness's deliberately closed text vocabulary.
    ///
    /// This is a reference implementation of the documented Lucene BM25
    /// formula and the fixture's analyzed position counts. It intentionally
    /// does not call the production tokenizer, postings, or BM25 scorer.
    #[must_use]
    pub fn expected_lexical(&self, query_slot: u8, k: usize) -> Vec<ExpectedLexicalHit> {
        let matching = self
            .live
            .iter()
            .filter(|(doc_id, _)| (**doc_id % 4) == u32::from(query_slot % 4))
            .collect::<Vec<_>>();
        if matching.is_empty() || self.live.is_empty() {
            return Vec::new();
        }
        let document_count = self.live.len() as f64;
        let document_frequency = matching.len() as f64;
        let total_tokens = self
            .live
            .iter()
            .map(|(doc_id, document)| lexical_document_len(*doc_id, document.revision))
            .sum::<u32>();
        let average_document_length = f64::from(total_tokens) / document_count;
        let idf =
            (1.0 + (document_count - document_frequency + 0.5) / (document_frequency + 0.5)).ln();
        let mut hits = matching
            .into_iter()
            .map(|(doc_id, document)| {
                let term_frequency = f64::from(*doc_id % 3 + 1);
                let document_length = f64::from(lexical_document_len(*doc_id, document.revision));
                let denominator = term_frequency
                    + 1.2 * (1.0 - 0.75 + 0.75 * document_length / average_document_length);
                ExpectedLexicalHit {
                    doc_id: *doc_id,
                    revision: document.revision,
                    score: idf * (term_frequency * (1.2 + 1.0)) / denominator,
                }
            })
            .collect::<Vec<_>>();
        hits.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.doc_id.cmp(&right.doc_id))
        });
        hits.truncate(k.min(hits.len()));
        hits
    }

    /// Independently applies the default alpha contract (0.7) or the
    /// documented reciprocal-rank fallback (k=60) to complete model legs.
    #[must_use]
    pub fn expected_hybrid(
        &self,
        vector_query: &[f32],
        query_slot: u8,
        k: usize,
    ) -> Vec<ExpectedHybridHit> {
        let vector = self
            .expected_exact(vector_query, self.len())
            .into_iter()
            .map(|hit| (hit.doc_id, -f64::from(hit.score)))
            .collect::<Vec<_>>();
        let lexical = self.expected_lexical(query_slot, self.len());
        let vector_range = score_range(vector.iter().map(|(_, score)| *score));
        let lexical_range = score_range(lexical.iter().map(|hit| hit.score));
        let reciprocal_rank = vector_range.is_none() || lexical_range.is_none();
        let mut accumulated = BTreeMap::<u32, ExpectedHybridHit>::new();
        for (rank, (doc_id, squared_l2)) in vector.iter().enumerate() {
            let contribution = if reciprocal_rank {
                1.0 / (60.0 + rank.saturating_add(1) as f64)
            } else {
                let (minimum, maximum) = vector_range.expect("non-degenerate vector range");
                0.7 * (maximum - squared_l2) / (maximum - minimum)
            };
            let hit = accumulated.entry(*doc_id).or_insert(ExpectedHybridHit {
                doc_id: *doc_id,
                vector_squared_l2: None,
                lexical_bm25: None,
                fused_score: 0.0,
            });
            hit.vector_squared_l2 = Some(*squared_l2);
            hit.fused_score += contribution;
        }
        for (rank, lexical_hit) in lexical.iter().enumerate() {
            let contribution = if reciprocal_rank {
                1.0 / (60.0 + rank.saturating_add(1) as f64)
            } else {
                let (minimum, maximum) = lexical_range.expect("non-degenerate lexical range");
                0.3 * (lexical_hit.score - minimum) / (maximum - minimum)
            };
            let hit = accumulated
                .entry(lexical_hit.doc_id)
                .or_insert(ExpectedHybridHit {
                    doc_id: lexical_hit.doc_id,
                    vector_squared_l2: None,
                    lexical_bm25: None,
                    fused_score: 0.0,
                });
            hit.lexical_bm25 = Some(lexical_hit.score);
            hit.fused_score += contribution;
        }
        let mut hits = accumulated.into_values().collect::<Vec<_>>();
        hits.sort_by(|left, right| {
            right
                .fused_score
                .total_cmp(&left.fused_score)
                .then_with(|| left.doc_id.cmp(&right.doc_id))
        });
        hits.truncate(k.min(hits.len()));
        hits
    }

    #[must_use]
    pub fn sealed_document_count(&self) -> usize {
        self.live
            .values()
            .filter(|document| matches!(document.location, Location::Sealed(_)))
            .count()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }
}

fn lexical_document_len(doc_id: u32, _revision: u64) -> u32 {
    let hexadecimal = format!("{doc_id:08x}");
    let mut hexadecimal_runs = 0_u32;
    let mut previous_was_digit = None;
    for character in hexadecimal.chars() {
        let is_digit = character.is_ascii_digit();
        if previous_was_digit != Some(is_digit) {
            hexadecimal_runs = hexadecimal_runs.saturating_add(1);
            previous_was_digit = Some(is_digit);
        }
    }
    // term repetitions + `common` + the positions occupied by
    // `ze`, the hexadecimal runs, `r`, and the revision digits.
    (doc_id % 3 + 1)
        .saturating_add(4)
        .saturating_add(hexadecimal_runs)
}

fn score_range(scores: impl Iterator<Item = f64>) -> Option<(f64, f64)> {
    let mut scores = scores;
    let first = scores.next()?;
    let (minimum, maximum, count) = scores.fold((first, first, 1_usize), |state, score| {
        (state.0.min(score), state.1.max(score), state.2 + 1)
    });
    (count > 1 && minimum != maximum).then_some((minimum, maximum))
}

fn predicate_matches(doc_id: u32, predicate: program::PredicateKind) -> bool {
    let numeric = program::numeric_column(doc_id);
    let boolean = program::boolean_column(doc_id);
    let string = program::string_column(doc_id);
    match predicate {
        program::PredicateKind::Eq => numeric == 1,
        program::PredicateKind::In => matches!(numeric, 1 | 3),
        program::PredicateKind::RangeTwoSided => (1..=3).contains(&numeric),
        program::PredicateKind::RangeHalfOpen => numeric >= 2,
        program::PredicateKind::Bool => boolean == Some(true),
        program::PredicateKind::String => string == "even",
        program::PredicateKind::Exists => boolean.is_some(),
        program::PredicateKind::IsNull => boolean.is_none(),
        program::PredicateKind::And => numeric >= 1 && boolean.is_some(),
        program::PredicateKind::Or => numeric == 0 || string == "odd",
        program::PredicateKind::Not => string != "odd",
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
