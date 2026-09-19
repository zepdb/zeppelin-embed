use super::*;

impl<'m, 'g, 'c> Builder<'m, 'g, 'c> {
    pub(super) fn search(
        &mut self,
        bound: &BoundQuery<'_>,
        syntax_id: AstId,
        current: PlanNodeId,
        has_prior: bool,
    ) -> Result<PlanNodeId, ParseError> {
        let clause = syntax(bound, syntax_id)?;
        let call = *bound
            .calls()
            .iter()
            .find(|call| call.syntax == syntax_id)
            .ok_or_else(|| invariant(clause.span, "bound search call"))?;
        let (request, independent) = match call.request {
            BoundSearchRequest::Vector {
                vector,
                k,
                mode,
                eligible,
            } => {
                let (eligible, independent) =
                    self.search_eligibility(bound, eligible, clause.span)?;
                (
                    SearchRequest::Vector {
                        vector: self.lower_invariant_expression(bound, vector, clause.span)?,
                        k: self.lower_invariant_expression(bound, k, clause.span)?,
                        mode: search_mode(mode),
                        eligible,
                    },
                    independent,
                )
            }
            BoundSearchRequest::Text { query, k, eligible } => {
                let (eligible, independent) =
                    self.search_eligibility(bound, eligible, clause.span)?;
                (
                    SearchRequest::Text {
                        query: self.lower_invariant_expression(bound, query, clause.span)?,
                        k: self.lower_invariant_expression(bound, k, clause.span)?,
                        eligible,
                    },
                    independent,
                )
            }
            BoundSearchRequest::Hybrid {
                vector,
                text,
                k,
                mode,
                eligible,
            } => {
                let (eligible, independent) =
                    self.search_eligibility(bound, eligible, clause.span)?;
                (
                    SearchRequest::Hybrid {
                        vector: self.lower_invariant_expression(bound, vector, clause.span)?,
                        text: self.lower_invariant_expression(bound, text, clause.span)?,
                        k: self.lower_invariant_expression(bound, k, clause.span)?,
                        mode: search_mode(mode),
                        eligible,
                    },
                    independent,
                )
            }
        };
        let source = if independent && has_prior {
            self.operator(DraftOp::Unit, &[], clause.span)?
        } else {
            current
        };
        let search = self.operator(
            DraftOp::Search {
                call: call.id,
                request,
                outputs: call.outputs,
            },
            &[source],
            clause.span,
        )?;
        if usize::try_from(call.id.0).map_err(|_| limit(clause.span))? != self.eager_searches.len()
        {
            return Err(invariant(clause.span, "search call order"));
        }
        self.eager_searches
            .push(search, self.memory, self.control)?;
        for slot in [
            call.outputs.node,
            call.outputs.distance,
            call.outputs.score,
            call.outputs.vector_distance,
            call.outputs.lexical_score,
        ]
        .into_iter()
        .flatten()
        {
            self.scope.push(slot, self.memory, self.control)?;
        }
        if independent && has_prior {
            self.operator(DraftOp::Join, &[current, search], clause.span)
        } else {
            Ok(search)
        }
    }

    fn search_eligibility(
        &mut self,
        bound: &BoundQuery<'_>,
        eligibility: BoundEligibility,
        span: Span,
    ) -> Result<(Option<ExprId>, bool), ParseError> {
        match eligibility {
            BoundEligibility::AllIndexed => Ok((None, true)),
            BoundEligibility::Materialized {
                expression,
                provenance: BoundEligibilityProvenance::LiteralEmpty,
            } => Ok((
                Some(self.lower_invariant_expression(bound, expression, span)?),
                true,
            )),
            BoundEligibility::Materialized {
                expression,
                provenance: BoundEligibilityProvenance::GlobalDistinctNodes,
            } => Ok((Some(self.lower_expression(bound, expression, span)?), false)),
        }
    }
}

const fn search_mode(mode: BoundSearchMode) -> SearchMode {
    match mode {
        BoundSearchMode::Default => SearchMode::Default,
        BoundSearchMode::Auto => SearchMode::Auto,
        BoundSearchMode::Exact => SearchMode::Exact,
        BoundSearchMode::Scan => SearchMode::Scan,
    }
}
