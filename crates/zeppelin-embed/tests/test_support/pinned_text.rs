use zeppelin_embed::fts::index::DEFAULT_FIELD;
use zeppelin_embed::fts::search::TermQuery;
use zeppelin_embed::fusion::{FusionError, FusionLeg, HybridQuery, LegFailureKind};
use zeppelin_embed::ingest::{SearchRequest, StoreLexicalError};
use zeppelin_embed::lifecycle::{
    CancelToken, QueryControl, QueryMaterializer, SearchOptions, SearchTier, Store,
};

#[derive(Clone, Copy, Debug)]
pub enum Leg {
    Dense,
    Lexical,
    Hybrid,
}
pub const LEGS: [Leg; 3] = [Leg::Dense, Leg::Lexical, Leg::Hybrid];

pub fn run<R>(
    store: &Store,
    leg: Leg,
    k: usize,
    finish: impl FnOnce(&QueryMaterializer<'_>) -> R,
) -> Result<(u64, R), FusionError> {
    let query = SearchRequest::new(&[1.0, 0.0]);
    let analyzer = zeppelin_embed::fts::tokenizer::Analyzer::new(
        zeppelin_embed::fts::tokenizer::TokenizerConfig::text_default(),
    )
    .map_err(|error| FusionError::Leg {
        leg: FusionLeg::Lexical,
        kind: LegFailureKind::Lexical,
        detail: error.to_string(),
    })?;
    let terms = TermQuery::flat(
        analyzer
            .analyze("bronze")
            .into_iter()
            .map(|token| token.term.into_bytes())
            .collect(),
        &[DEFAULT_FIELD],
    );
    let options = SearchOptions::default().with_tier(SearchTier::Exact);
    let control = QueryControl::Cancel(CancelToken::new());
    match leg {
        Leg::Dense => store
            .search_with_text(query, k, options, control, |_, materializer| {
                finish(materializer)
            })
            .map(|(outcome, value)| (outcome.generation, value))
            .map_err(FusionError::from),
        Leg::Lexical => store
            .search_lexical_with_text(&terms, k, control, |_, materializer| finish(materializer))
            .map(|(outcome, value)| (outcome.generation, value))
            .map_err(|error| match error {
                StoreLexicalError::Query(error) => FusionError::from(error),
                error => FusionError::Leg {
                    leg: FusionLeg::Lexical,
                    kind: LegFailureKind::Lexical,
                    detail: error.to_string(),
                },
            }),
        Leg::Hybrid => store
            .search_hybrid_with_text(
                query,
                &terms,
                &HybridQuery::new(k),
                options,
                control,
                |_, materializer| finish(materializer),
            )
            .map(|(outcome, value)| (outcome.generation, value)),
    }
}
