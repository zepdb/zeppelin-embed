//! Vector preparation retained only for one admitted, immutable query.

use std::sync::Arc;

use super::stats::{Accounted, AccountedCounter, Accounting, AllocationComponent};
use super::{QueryError, StoreError};
use crate::epoch::EpochIdentity;
use crate::graph::search::{GraphSearchError, PreparedGraphQuery};
use crate::ingest::{DocumentVersion, GlobalRowId, RowSource};
use crate::quant::{ExactScoreReuse, RescoreCheckError, rescore_top_k_reusing};
use crate::segment::reader::SegmentReader;

#[derive(Clone, Copy, Eq, PartialEq)]
enum Transform {
    UnrotatedBit4Blocks,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct Geometry {
    dimensions: usize,
    padded_dimensions: usize,
    transform: Transform,
    seed: u64,
    space: Option<EpochIdentity>,
}

struct PreparedLayout {
    geometry: Geometry,
    prepared: PreparedGraphQuery,
    _codes: AccountedCounter,
}

pub(crate) struct PreparedVectorQuery<'a> {
    query: &'a [f32],
    space: Option<EpochIdentity>,
    accounting: &'a Arc<Accounting>,
    layouts: Accounted<Vec<PreparedLayout>>,
    graph_scores: Accounted<Vec<GraphExactScore>>,
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum GraphScorePolicy {
    NegativeSquaredL2F64,
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct GraphScoreKey {
    row: GlobalRowId,
    document: Option<DocumentVersion>,
    epoch: Option<EpochIdentity>,
    policy: GraphScorePolicy,
}

struct GraphExactScore {
    key: GraphScoreKey,
    score: f64,
}

/// One immutable segment binding into the admission's f64-only score cache.
struct GraphScoreReuse<'a> {
    entries: &'a mut Accounted<Vec<GraphExactScore>>,
    sorted: usize,
    segment: &'a SegmentReader,
    source: RowSource,
    epoch: Option<EpochIdentity>,
    accounting: &'a Arc<Accounting>,
}

impl GraphScoreReuse<'_> {
    fn key(&self, row: usize) -> Result<GraphScoreKey, QueryError> {
        Ok(GraphScoreKey {
            row: GlobalRowId::new(self.source, u32::try_from(row).map_err(|_| overflow())?),
            document: self
                .segment
                .document_version(row)
                .map_err(|error| QueryError::Store(StoreError::Segment(error)))?,
            epoch: self.epoch,
            policy: GraphScorePolicy::NegativeSquaredL2F64,
        })
    }
}

impl ExactScoreReuse<QueryError> for GraphScoreReuse<'_> {
    fn get(&mut self, row: usize) -> Result<Option<f64>, QueryError> {
        let key = self.key(row)?;
        let sorted = self.entries.get(..self.sorted).ok_or_else(overflow)?;
        Ok(sorted
            .binary_search_by_key(&key, |entry| entry.key)
            .ok()
            .and_then(|index| sorted.get(index))
            .map(|entry| entry.score))
    }

    fn insert(&mut self, row: usize, score: f64) -> Result<(), QueryError> {
        let key = self.key(row)?;
        if self.entries.len() == self.entries.capacity() {
            let capacity = self
                .entries
                .len()
                .checked_mul(2)
                .ok_or_else(overflow)?
                .max(16);
            self.entries
                .try_reserve_total(self.accounting, capacity, AllocationComponent::Temporary)
                .map_err(QueryError::Store)?;
        }
        self.entries
            .push(GraphExactScore { key, score })
            .map_err(QueryError::Store)
    }
}

impl<'a> PreparedVectorQuery<'a> {
    pub(crate) fn new(
        query: &'a [f32],
        space: Option<EpochIdentity>,
        accounting: &'a Arc<Accounting>,
    ) -> Self {
        Self {
            query,
            space,
            accounting,
            layouts: Accounted::unaccounted_empty(),
            graph_scores: Accounted::unaccounted_empty(),
        }
    }

    pub(crate) fn validate_binding(
        &self,
        query: &[f32],
        space: Option<EpochIdentity>,
    ) -> Result<(), QueryError> {
        if !std::ptr::eq(query, self.query) || space != self.space {
            return Err(QueryError::Graph(GraphSearchError::Geometry(
                "prepared vector context belongs to another query or epoch".to_owned(),
            )));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn rescore_graph(
        &mut self,
        query: &[f32],
        rows: &[f32],
        dimensions: usize,
        pool: crate::quant::RescorePool<'_>,
        k: usize,
        segment: &SegmentReader,
        source: RowSource,
    ) -> Result<crate::quant::RescoreResult, QueryError> {
        self.validate_binding(query, self.space)?;
        let sorted = self.graph_scores.len();
        let mut reuse = GraphScoreReuse {
            entries: &mut self.graph_scores,
            sorted,
            segment,
            source,
            epoch: self.space,
            accounting: self.accounting,
        };
        let result =
            rescore_top_k_reusing(query, rows, dimensions, pool, k, |_, _| Ok(()), &mut reuse);
        self.graph_scores
            .as_mut_slice()
            .sort_unstable_by_key(|entry| entry.key);
        result.map_err(|error| match error {
            RescoreCheckError::Rescore(error) => {
                super::map_graph_error(GraphSearchError::Rescore(error))
            }
            RescoreCheckError::Check(error) => error,
        })
    }

    pub(crate) fn graph(
        &mut self,
        query: &[f32],
        padded_dimensions: usize,
        seed: u64,
    ) -> Result<&PreparedGraphQuery, QueryError> {
        self.validate_binding(query, self.space)?;
        let geometry = Geometry {
            dimensions: self.query.len(),
            padded_dimensions,
            transform: Transform::UnrotatedBit4Blocks,
            seed,
            space: self.space,
        };
        let index = match self
            .layouts
            .iter()
            .position(|entry| entry.geometry == geometry)
        {
            Some(index) => index,
            None => {
                let index = self.layouts.len();
                let capacity = index.checked_add(1).ok_or_else(overflow)?;
                self.layouts
                    .try_reserve_total(self.accounting, capacity, AllocationComponent::Temporary)
                    .map_err(QueryError::Store)?;
                let mut codes =
                    AccountedCounter::new(self.accounting, AllocationComponent::Temporary)
                        .map_err(QueryError::Store)?;
                // Nonzero Bit4 preparation holds coordinate and interleaved
                // code buffers simultaneously. Padding, when needed, is f32.
                let code_buffers = if self.query.iter().all(|value| *value == 0.0) {
                    1
                } else {
                    2
                };
                let padding_buffers = usize::from(padded_dimensions != self.query.len()) * 4;
                let peak_bytes = padded_dimensions
                    .checked_mul(code_buffers + padding_buffers)
                    .ok_or_else(overflow)?;
                codes.set(peak_bytes).map_err(QueryError::Store)?;
                let prepared = PreparedGraphQuery::new(self.query, padded_dimensions, seed)
                    .map_err(super::map_graph_error)?;
                codes
                    .set(prepared.resident_bytes())
                    .map_err(QueryError::Store)?;
                self.layouts
                    .push(PreparedLayout {
                        geometry,
                        prepared,
                        _codes: codes,
                    })
                    .map_err(QueryError::Store)?;
                index
            }
        };
        self.layouts
            .get(index)
            .map(|entry| &entry.prepared)
            .ok_or(QueryError::Store(StoreError::Synchronization {
                component: "prepared vector layout",
            }))
    }
}

fn overflow() -> QueryError {
    QueryError::Scan(crate::scan::ScanError::ArithmeticOverflow)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::epoch::EpochId;
    use crate::fts::tokenizer::TokenizerConfig;
    use crate::graph::search::{
        begin_graph_query_test_observations, take_graph_query_test_observations,
    };

    fn query(dimensions: usize) -> Vec<f32> {
        (0..dimensions)
            .map(|n| (n % 17) as f32 * 0.013 + (n % 3) as f32 * 0.002)
            .collect()
    }

    #[test]
    fn astra_09_prepared_layouts_keep_padding_seed_and_query_space_distinct() {
        for dimensions in [128, 129] {
            let query = query(dimensions);
            let accounting = Arc::new(Accounting::new(u64::MAX, u64::MAX));
            let space = EpochIdentity {
                embedding: EpochId::from_value(1),
                tokenizer: TokenizerConfig::text_default().epoch(),
            };
            let mut context = PreparedVectorQuery::new(&query, Some(space), &accounting);
            begin_graph_query_test_observations();
            let mut first_seed_codes = None;
            for (padding, seed) in [(256, 0), (256, 71), (512, 71), (256, 0), (512, 71)] {
                let mut padded = query.clone();
                padded.resize(padding, 0.0);
                let independent = crate::quant::prepare_bit4_query(&padded, seed)
                    .expect("fresh independent layout");
                let actual = context
                    .graph(&query, padding, seed)
                    .expect("compatible layout");
                assert_eq!(actual.bit4(), &independent);
                if padding == 256 && seed == 0 {
                    first_seed_codes = Some(actual.bit4().observation_parts().0.to_vec());
                } else if padding == 256 {
                    assert_ne!(
                        Some(actual.bit4().observation_parts().0.to_vec()),
                        first_seed_codes,
                        "the two seeds must produce different prepared bytes"
                    );
                }
            }
            let observed = take_graph_query_test_observations();
            assert_eq!(
                observed.preparations,
                vec![
                    (dimensions, 256, 0),
                    (dimensions, 256, 71),
                    (dimensions, 512, 71),
                ]
            );
            let retained = context.layouts.resident_bytes()
                + context
                    .layouts
                    .iter()
                    .map(|layout| layout._codes.bytes())
                    .sum::<u64>();
            assert_eq!(
                accounting
                    .audit()
                    .expect("exact retained bytes")
                    .temporary_bytes,
                retained
            );
            assert!(
                context
                    .validate_binding(&query.clone(), Some(space))
                    .is_err()
            );
            let other_space = EpochIdentity {
                embedding: EpochId::from_value(2),
                ..space
            };
            assert!(context.validate_binding(&query, Some(other_space)).is_err());
            drop(context);
            assert_eq!(
                accounting
                    .audit()
                    .expect("released layouts")
                    .temporary_bytes,
                0
            );
        }
    }

    #[test]
    fn astra_09_graph_score_growth_accounts_overlap_and_releases_on_failure() {
        use crate::ingest::{DocId, IngestBatch, IngestDocument, Revision};
        use crate::lifecycle::{OpenOptions, Store};
        use crate::quant::{RescoreMetric, RescorePool};
        let directory = tempfile::tempdir().expect("score cache fixture");
        let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
        store
            .ingest(IngestBatch::new(
                (0..17)
                    .map(|row| {
                        IngestDocument::new(
                            DocumentVersion::new(DocId::new(row + 1), Revision::new(1)),
                            vec![row as f32],
                        )
                    })
                    .collect(),
            ))
            .expect("ingest");
        store.seal().expect("seal score rows");
        let snapshot = store.snapshot().expect("pin immutable rows");
        let segment = snapshot.segments().first().expect("one sealed source");
        let rows = segment.rescore_f32().expect("validated exact rows");
        let source = RowSource::Sealed(segment.meta().id);
        let query = [0.0_f32];
        let ids = (0..17_u32).collect::<Vec<_>>();
        let coarse = vec![0.0_f32; ids.len()];
        let pool = RescorePool::retained(&ids, &coarse, RescoreMetric::SquaredL2, ids.len(), 13);
        let entry_bytes = std::mem::size_of::<GraphExactScore>() as u64;
        // Sixteen old entries plus a 32-entry replacement need 48 slots,
        // even though the final allocation would fit in this 47-slot budget.
        let accounting = Arc::new(Accounting::new(u64::MAX, 47 * entry_bytes));
        let mut context = PreparedVectorQuery::new(&query, None, &accounting);
        let failure = context.rescore_graph(&query, rows, 1, pool, 1, segment, source);
        assert!(matches!(
            failure,
            Err(QueryError::Store(StoreError::BudgetExceeded {
                component: "temporary",
                ..
            }))
        ));
        assert_eq!(context.graph_scores.len(), 16);
        assert_eq!(
            accounting
                .audit()
                .expect("old allocation remains owned")
                .temporary_bytes,
            16 * entry_bytes
        );
        drop(context);
        assert_eq!(
            accounting
                .audit()
                .expect("failed query released")
                .temporary_bytes,
            0
        );
        let accounting = Arc::new(Accounting::new(u64::MAX, 48 * entry_bytes));
        let mut context = PreparedVectorQuery::new(&query, None, &accounting);
        let fresh = context
            .rescore_graph(&query, rows, 1, pool, 1, segment, source)
            .expect("exact growth allowance");
        let reused = context
            .rescore_graph(&query, rows, 1, pool, 1, segment, source)
            .expect("cached control");
        assert_eq!(fresh.candidates_rescored, 17);
        assert_eq!(reused.candidates_rescored, 0);
        assert_eq!(fresh.hits, reused.hits);
        assert_eq!(context.graph_scores.len(), 17);
        assert_eq!(
            accounting
                .audit()
                .expect("retained allocation")
                .temporary_bytes,
            32 * entry_bytes
        );
        drop(context);
        assert_eq!(
            accounting
                .audit()
                .expect("successful query released")
                .temporary_bytes,
            0
        );
        drop(snapshot);
        store.close().expect("close");
    }

    #[test]
    fn astra_09_preparation_budget_refusal_releases_partial_capacity() {
        let query = query(128);
        let header_bytes = std::mem::size_of::<PreparedLayout>() as u64;
        // Entry capacity fits; the simultaneous 128-byte coordinate and
        // interleaved buffers exceed the remaining allowance by one byte.
        let accounting = Arc::new(Accounting::new(u64::MAX, header_bytes + 255));
        let mut context = PreparedVectorQuery::new(&query, None, &accounting);
        begin_graph_query_test_observations();
        assert!(matches!(
            context.graph(&query, 128, 0),
            Err(QueryError::Store(StoreError::BudgetExceeded {
                component: "temporary",
                ..
            }))
        ));
        assert!(
            context.layouts.is_empty(),
            "no partial prepared entry is published"
        );
        assert!(
            take_graph_query_test_observations().preparations.is_empty(),
            "reservation must fail before preparation allocates"
        );
        assert_eq!(
            accounting
                .audit()
                .expect("retained empty header")
                .temporary_bytes,
            header_bytes
        );
        drop(context);
        assert_eq!(
            accounting
                .audit()
                .expect("failed admission released")
                .temporary_bytes,
            0
        );
        let accounting = Arc::new(Accounting::new(u64::MAX, u64::MAX));
        let mut retry = PreparedVectorQuery::new(&query, None, &accounting);
        assert!(retry.graph(&query, 128, 0).is_ok());
        drop(retry);
        assert_eq!(
            accounting
                .audit()
                .expect("clean retry released")
                .temporary_bytes,
            0
        );
    }
}
