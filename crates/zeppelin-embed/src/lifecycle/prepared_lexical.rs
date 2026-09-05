//! Lexical preparation owned by one pinned hybrid admission.

use std::sync::Arc;

use super::stats::{AccountedCounter, AllocationComponent};
use super::{
    LexicalAssembly, LexicalInputs, PinnedLexicalQuery, QueryCancellation, QueryError, StoreError,
};
use crate::fts::search::{PreparedTermQuery, TermQuery};
use crate::fusion::{FusionError, FusionLeg, LegFailureKind};

pub(super) struct PreparedLexicalQuery<'query> {
    query: PinnedLexicalQuery<'query>,
    term: Option<PreparedTermState>,
    structured: Option<PreparedStructuredState>,
}

pub(super) struct PreparedTermState {
    pub(super) assembly: Arc<LexicalAssembly>,
    scoring: Option<PreparedTermQuery>,
    _memory: AccountedCounter,
}

impl PreparedTermState {
    pub(super) fn scoring(&self) -> Result<&PreparedTermQuery, FusionError> {
        self.scoring
            .as_ref()
            .ok_or_else(|| invariant("nonempty lexical input has no prepared statistics"))
    }
}

pub(super) struct PreparedStructuredState {
    pub(super) assembly: Arc<LexicalAssembly>,
    pub(super) expansions: Vec<crate::fts::query::LexicalExpansion>,
    pub(super) terms: Vec<PreparedTermQuery>,
    _memory: AccountedCounter,
}

impl<'query> PreparedLexicalQuery<'query> {
    pub(super) fn new(query: PinnedLexicalQuery<'query>) -> Self {
        Self {
            query,
            term: None,
            structured: None,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn reset_for_test(&mut self) {
        self.term = None;
        self.structured = None;
    }

    pub(super) fn prepare_term(
        &mut self,
        inputs: LexicalInputs<'_>,
        cancellation: &QueryCancellation<'_>,
    ) -> Result<(&PreparedTermState, bool), FusionError> {
        let PinnedLexicalQuery::Term(query) = self.query else {
            return Err(invariant("term preparation used for another query kind"));
        };
        let mut cache_hit = true;
        if self.term.is_none() {
            let receipt = super::assemble_lexical_index(inputs, false, Some(cancellation))
                .map_err(super::map_fusion_lexical_assembly_error)?;
            cache_hit = receipt.cache_hit;
            cancellation.check_graph().map_err(QueryError::Scan)?;
            let mut memory =
                AccountedCounter::new(inputs.accounting, AllocationComponent::Temporary)
                    .map_err(QueryError::Store)?;
            let scoring = if receipt.index.segments().is_empty() {
                None
            } else {
                let bytes = PreparedTermQuery::allocation_bytes(query).ok_or_else(overflow)?;
                memory.set(bytes).map_err(QueryError::Store)?;
                Some(
                    PreparedTermQuery::new(
                        &receipt.index,
                        query,
                        crate::fts::bm25::Bm25Params::beir(),
                    )
                    .map_err(|error| lexical_failure(&error.to_string()))?,
                )
            };
            self.term = Some(PreparedTermState {
                assembly: Arc::clone(&receipt.assembly),
                scoring,
                _memory: memory,
            });
        }
        Ok((
            self.term
                .as_ref()
                .ok_or_else(|| invariant("prepared term state is absent"))?,
            cache_hit,
        ))
    }

    pub(super) fn prepare_structured(
        &mut self,
        inputs: LexicalInputs<'_>,
        cancellation: &QueryCancellation<'_>,
    ) -> Result<(&PreparedStructuredState, bool), FusionError> {
        let PinnedLexicalQuery::Structured(query) = self.query else {
            return Err(invariant(
                "structured preparation used for another query kind",
            ));
        };
        let mut cache_hit = true;
        if self.structured.is_none() {
            let receipt = super::assemble_lexical_index(inputs, false, Some(cancellation))
                .map_err(super::map_fusion_lexical_assembly_error)?;
            let vocabulary = crate::fts::query::vocabulary(query, receipt.index.terms());
            let expansions = crate::fts::query::expand(query, &vocabulary)
                .map_err(|error| lexical_failure(&error.to_string()))?;
            drop(vocabulary);
            cache_hit = receipt.cache_hit;
            cancellation.check_graph().map_err(QueryError::Scan)?;
            let mut memory =
                AccountedCounter::new(inputs.accounting, AllocationComponent::Temporary)
                    .map_err(QueryError::Store)?;
            let expansion_bytes = expansions.iter().try_fold(
                expansions
                    .capacity()
                    .checked_mul(std::mem::size_of::<crate::fts::query::LexicalExpansion>())
                    .ok_or_else(overflow)?,
                |bytes, expansion| {
                    bytes
                        .checked_add(expansion.term.capacity())
                        .ok_or_else(overflow)
                },
            )?;
            let count = if receipt.index.segments().is_empty() {
                0
            } else {
                expansions.len()
            };
            let mut bytes = expansion_bytes
                .checked_add(
                    count
                        .checked_mul(std::mem::size_of::<PreparedTermQuery>())
                        .ok_or_else(overflow)?,
                )
                .ok_or_else(overflow)?;
            memory.set(bytes).map_err(QueryError::Store)?;
            let mut terms = Vec::with_capacity(count);
            let fields = query.fields();
            for expansion in expansions.iter().take(count) {
                cancellation.check_graph().map_err(QueryError::Scan)?;
                bytes = bytes
                    .checked_add(
                        PreparedTermQuery::single_allocation_bytes(&fields, expansion.term.len())
                            .ok_or_else(overflow)?,
                    )
                    .ok_or_else(overflow)?;
                memory.set(bytes).map_err(QueryError::Store)?;
                let term = TermQuery {
                    terms: vec![expansion.term.clone()],
                    fields: fields.clone(),
                };
                terms.push(
                    PreparedTermQuery::from_owned(
                        &receipt.index,
                        term,
                        crate::fts::bm25::Bm25Params::beir(),
                    )
                    .map_err(|error| lexical_failure(&error.to_string()))?,
                );
            }
            self.structured = Some(PreparedStructuredState {
                assembly: Arc::clone(&receipt.assembly),
                expansions,
                terms,
                _memory: memory,
            });
        }
        Ok((
            self.structured
                .as_ref()
                .ok_or_else(|| invariant("prepared structured state is absent"))?,
            cache_hit,
        ))
    }

    pub(super) fn has_expansions(&self) -> bool {
        match self.query {
            PinnedLexicalQuery::Term(query) => !query.terms.is_empty(),
            PinnedLexicalQuery::Structured(_) => self
                .structured
                .as_ref()
                .is_some_and(|state| !state.expansions.is_empty()),
        }
    }

    pub(super) fn take_expansions(
        &mut self,
    ) -> Result<Vec<crate::fts::query::LexicalExpansion>, FusionError> {
        Ok(match self.query {
            PinnedLexicalQuery::Term(query) => query
                .terms
                .iter()
                .cloned()
                .map(|term| crate::fts::query::LexicalExpansion {
                    term,
                    boost_thousandths: 1_000,
                    kind: crate::fts::query::LexicalMatchKind::Term,
                })
                .collect(),
            PinnedLexicalQuery::Structured(_) => std::mem::take(
                &mut self
                    .structured
                    .as_mut()
                    .ok_or_else(|| invariant("structured producer did not prepare expansions"))?
                    .expansions,
            ),
        })
    }

    pub(super) fn term_scoring(
        &self,
        assembly: &LexicalAssembly,
        query: &TermQuery,
    ) -> Result<&PreparedTermQuery, FusionError> {
        let PinnedLexicalQuery::Term(bound) = self.query else {
            return Err(invariant("term scoring used for another query kind"));
        };
        let state = self
            .term
            .as_ref()
            .ok_or_else(|| invariant("lexical producer did not prepare its query"))?;
        if !std::ptr::eq(bound, query) || !std::ptr::eq(Arc::as_ptr(&state.assembly), assembly) {
            return Err(invariant(
                "lexical preparation belongs to another query or pinned assembly",
            ));
        }
        state
            .scoring
            .as_ref()
            .ok_or_else(|| invariant("nonempty lexical candidates have no prepared statistics"))
    }
}

fn invariant(detail: &str) -> FusionError {
    FusionError::Leg {
        leg: FusionLeg::Lexical,
        kind: LegFailureKind::Invariant,
        detail: detail.to_owned(),
    }
}

fn lexical_failure(detail: &str) -> FusionError {
    FusionError::Leg {
        leg: FusionLeg::Lexical,
        kind: LegFailureKind::Lexical,
        detail: detail.to_owned(),
    }
}

fn overflow() -> FusionError {
    QueryError::Store(StoreError::BudgetExceeded {
        needed: u64::MAX,
        budget: u64::MAX,
        component: "temporary",
    })
    .into()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::fts::index::DEFAULT_FIELD;
    use crate::fts::query::LexicalQuery;
    use crate::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
    use crate::lifecycle::stats::Accounting;
    use crate::lifecycle::{
        CancelToken, OpenOptions, QueryControl, SearchOptions, SearchTier, Store, StoreErrorKind,
    };

    #[test]
    fn astra_09_lexical_preparation_refuses_foreign_bindings_and_releases_failed_budget() {
        let directory = tempfile::tempdir().expect("lexical preparation store");
        let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
        store
            .ingest(IngestBatch::new(vec![
                IngestDocument::new(
                    DocumentVersion::new(DocId::new(1), Revision::new(1)),
                    vec![1.0, 0.0],
                )
                .with_text("alpha beta"),
            ]))
            .expect("ingest");
        let admitted = store
            .admit_vector_search(SearchOptions::default().with_tier(SearchTier::Exact))
            .expect("pin");
        let lease = super::super::SnapshotLease::new_at(
            Arc::clone(&admitted.snapshot),
            admitted.generation,
        );
        let control = QueryControl::Cancel(CancelToken::new());
        let cancellation = QueryCancellation::new(&control, &lease);
        let inputs = LexicalInputs {
            cache: &store.lexical_index_cache,
            snapshot: &admitted.snapshot,
            active: &admitted.active_segment,
            accounting: &store.accounting,
        };
        let assembly = super::super::assemble_lexical_index(inputs, false, Some(&cancellation))
            .unwrap_or_else(|_| panic!("warm immutable assembly"));
        let query = TermQuery::flat(vec![b"alpha".to_vec(), b"beta".to_vec()], &[DEFAULT_FIELD]);
        let bytes =
            PreparedTermQuery::allocation_bytes(&query).expect("fixed query allocation") as u64;
        for limit in [bytes - 1, bytes] {
            let accounting = Arc::new(Accounting::new(u64::MAX, limit));
            let inputs = LexicalInputs {
                accounting: &accounting,
                ..inputs
            };
            let mut context = PreparedLexicalQuery::new(PinnedLexicalQuery::Term(&query));
            let result = context.prepare_term(inputs, &cancellation);
            if limit < bytes {
                assert!(matches!(
                    result,
                    Err(FusionError::Leg {
                        kind: LegFailureKind::Store(StoreErrorKind::BudgetExceeded),
                        ..
                    })
                ));
                assert!(
                    context.term.is_none(),
                    "no partial query preparation is published"
                );
                assert_eq!(
                    accounting
                        .audit()
                        .expect("failure released")
                        .temporary_bytes,
                    0
                );
            } else {
                assert!(result.is_ok());
                assert_eq!(
                    accounting.audit().expect("retained tables").temporary_bytes,
                    bytes
                );
                assert!(context.term_scoring(&assembly.assembly, &query).is_ok());
                assert!(matches!(
                    context.term_scoring(&assembly.assembly, &query.clone()),
                    Err(FusionError::Leg {
                        kind: LegFailureKind::Invariant,
                        ..
                    })
                ));
                let foreign_cache = super::super::LexicalIndexCache::new();
                let foreign_inputs = LexicalInputs {
                    cache: &foreign_cache,
                    accounting: &store.accounting,
                    ..inputs
                };
                let foreign = super::super::assemble_lexical_index(
                    foreign_inputs,
                    false,
                    Some(&cancellation),
                )
                .unwrap_or_else(|_| panic!("independent assembly owner"));
                assert!(matches!(
                    context.term_scoring(&foreign.assembly, &query),
                    Err(FusionError::Leg {
                        kind: LegFailureKind::Invariant,
                        ..
                    })
                ));
                let _ = context
                    .prepare_term(inputs, &cancellation)
                    .expect("same admission reuses preparation");
                assert_eq!(
                    accounting.audit().expect("no new capacity").temporary_bytes,
                    bytes
                );
            }
            drop(context);
            assert_eq!(
                accounting
                    .audit()
                    .expect("admission released")
                    .temporary_bytes,
                0
            );
        }
        let prefix = LexicalQuery::prefix(b"a".to_vec(), DEFAULT_FIELD);
        let accounting = Arc::new(Accounting::new(u64::MAX, 0));
        let mut context = PreparedLexicalQuery::new(PinnedLexicalQuery::Structured(&prefix));
        assert!(matches!(
            context.prepare_structured(
                LexicalInputs {
                    accounting: &accounting,
                    ..inputs
                },
                &cancellation
            ),
            Err(FusionError::Leg {
                kind: LegFailureKind::Store(StoreErrorKind::BudgetExceeded),
                ..
            })
        ));
        assert!(context.structured.is_none());
        drop(context);
        assert_eq!(
            accounting
                .audit()
                .expect("failed structured query released")
                .temporary_bytes,
            0
        );
        drop(assembly);
        drop(lease);
        drop(admitted);
        store.close().expect("close");
    }
}
