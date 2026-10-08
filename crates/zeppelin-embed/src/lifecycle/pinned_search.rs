//! Graph retrieval uses the document half of its existing statement lease.
use super::native_graph::documents::NativeDocuments;
use super::*;

impl Store {
    pub(crate) fn graph_text_in(
        &self,
        pin: &NativeDocuments,
        query: &crate::fts::query::LexicalQuery,
        k: usize,
        filter: Option<&QueryFilter>,
        control: QueryControl,
    ) -> Result<(crate::ingest::StoreLexicalSearchOutcome, u64), crate::fusion::FusionError> {
        let started = self.clock.now();
        let lease = SnapshotLease::new_at(Arc::clone(&pin.snapshot), pin.generation);
        let cancellation = QueryCancellation::new(&control, &lease);
        let inputs = LexicalInputs {
            filter,
            generation: pin.generation,
            cache: &self.lexical_index_cache,
            snapshot: &pin.snapshot,
            active: &pin.active,
            accounting: &self.accounting,
        };
        let lexical = match query {
            crate::fts::query::LexicalQuery::Term(term) => PinnedLexicalQuery::Term(term),
            query => PinnedLexicalQuery::Structured(query),
        };
        let mut preparation = prepared_lexical::PreparedLexicalQuery::new(lexical);
        let (hits, _, counters, _) = match lexical {
            PinnedLexicalQuery::Term(_) => {
                exact_lexical_leg(inputs, k, &cancellation, &mut preparation)?
            }
            PinnedLexicalQuery::Structured(query) => exact_structured_lexical_leg(
                inputs,
                &self.tokenizer,
                query,
                k,
                &cancellation,
                &mut preparation,
            )?,
        };
        // Count matching eligible posting membership before top-k. Scorer
        // work counters count evaluated rows and cannot stand in for this domain.
        let (assembly, scoring) = match lexical {
            PinnedLexicalQuery::Term(_) => {
                let (prepared, _) = preparation.prepare_term(inputs, &cancellation)?;
                (
                    &prepared.assembly,
                    if prepared.assembly.index.segments().is_empty() {
                        None
                    } else {
                        Some(prepared.scoring()?.query())
                    },
                )
            }
            PinnedLexicalQuery::Structured(_) => {
                let (prepared, _) = preparation.prepare_structured(inputs, &cancellation)?;
                (
                    &prepared.assembly,
                    prepared.query.as_ref().map(|query| query.scoring().query()),
                )
            }
        };
        let mut matching = 0;
        if let Some(scoring) = scoring {
            for (segment, alive) in assembly.index.segments().iter().zip(&assembly.alive_sets) {
                let mut rows = crate::meta::DocBitmap::new();
                for term in &scoring.terms {
                    let mut work =
                        crate::fts::control::WorkCheck::new(|| cancellation.check_graph());
                    let Some(mut stream) = crate::fts::sealed::TermStream::open_controlled(
                        segment,
                        term,
                        &scoring.fields,
                        &mut work,
                    )
                    .map_err(QueryError::Scan)?
                    else {
                        continue;
                    };
                    while let Some(row) = stream.current_row() {
                        work.check_now().map_err(QueryError::Scan)?;
                        if alive.alive_bitmap().contains(row) {
                            rows.insert(row);
                        }
                        stream
                            .advance_controlled(&mut work)
                            .map_err(QueryError::Scan)?;
                    }
                }
                matching += rows.cardinality();
            }
        }
        let candidates = hits
            .into_iter()
            .map(|hit| {
                let document = hit
                    .document
                    .ok_or_else(|| crate::fusion::FusionError::Leg {
                        leg: crate::fusion::FusionLeg::Lexical,
                        kind: crate::fusion::LegFailureKind::Lexical,
                        detail: "lexical hit has no document identity".into(),
                    })?;
                Ok(crate::ingest::LexicalCandidate {
                    document,
                    score: hit.bm25,
                })
            })
            .collect::<Result<Vec<_>, crate::fusion::FusionError>>()?;
        let mut diagnostics =
            crate::diag::QueryDiagnostics::lexical(crate::diag::LexicalDiagnostics {
                snapshot_generation: pin.generation,
                indexed_through_seq: pin
                    .active
                    .indexed_through_seq()
                    .max(crate::wal::LogSeq::new(pin.snapshot.absorbed_through())),
                requested_k: k,
                returned: candidates.len(),
                counters,
                elapsed: self.clock.now().saturating_duration_since(started),
            });
        diagnostics.tokenizer_epoch = Some(self.tokenizer.epoch());
        Ok((
            crate::ingest::StoreLexicalSearchOutcome {
                candidates,
                generation: pin.generation,
                diagnostics,
            },
            matching,
        ))
    }
}

impl Store {
    fn graph_search_pin(
        &self,
        pin: &NativeDocuments,
        options: SearchOptions,
    ) -> Result<AdmittedVectorSearch<'_>, QueryError> {
        Ok(AdmittedVectorSearch {
            execution_options: options,
            pool: Some(self.ensure_query_pool()?),
            snapshot: snapshot::ReadSnapshot::new(Arc::clone(&pin.snapshot)),
            active_segment: Arc::clone(&pin.active),
            generation: pin.generation,
            _active_query: None,
        })
    }

    pub(crate) fn graph_vector_in(
        &self,
        pin: &NativeDocuments,
        request: crate::ingest::SearchRequest<'_>,
        k: usize,
        options: SearchOptions,
        control: QueryControl,
    ) -> Result<crate::ingest::SearchOutcome, QueryError> {
        let eligible_filter = request.eligible.map(|ids| {
            request.filter.cloned().map_or_else(
                || QueryFilter::eligible(&self.schema, ids),
                |filter| filter.with_eligible(ids),
            )
        });
        let request = request.with_filter(eligible_filter.as_ref().or(request.filter));
        let admitted = self.graph_search_pin(pin, options)?;
        let epoch = pin.snapshot.epoch_alias();
        let mut preparation =
            prepared::PreparedVectorQuery::new(request.vector(), epoch, &self.accounting);
        search_pinned(
            admitted.pool.as_deref(),
            &pin.snapshot,
            &pin.active,
            &self.accounting,
            pin.generation,
            epoch,
            request,
            k,
            options,
            control,
            GraphBoundMode::Shared,
            options.explicit_tier(),
            None,
            self.clock.now(),
            self.clock.as_ref(),
            &mut preparation,
            #[cfg(any(test, feature = "test-seams"))]
            self.vector_fault_controller.as_ref(),
        )
    }

    pub(crate) fn graph_hybrid_in(
        &self,
        pin: &NativeDocuments,
        request: crate::ingest::SearchRequest<'_>,
        lexical: &crate::fts::query::LexicalQuery,
        hybrid: &crate::fusion::HybridQuery,
        options: SearchOptions,
        control: QueryControl,
    ) -> Result<crate::ingest::StoreHybridSearchOutcome, crate::fusion::FusionError> {
        let lexical = match lexical {
            crate::fts::query::LexicalQuery::Term(term) => PinnedLexicalQuery::Term(term),
            lexical => PinnedLexicalQuery::Structured(lexical),
        };
        self.search_hybrid_prepared_then(
            || Ok::<_, std::convert::Infallible>(request),
            request.filter,
            request.eligible,
            lexical,
            hybrid,
            options,
            control,
            false,
            Some(self.graph_search_pin(pin, options)?),
            |outcome, _, _, _, _| Ok(outcome),
        )
        .map_err(|error| match error {
            HybridPreparationError::Preparation(never) => match never {},
            HybridPreparationError::Search(error) => error,
        })
    }
}
