//! Lexical and hybrid ranking restricted to a complete eligibility set.
//!
//! Score domains are full-live, never eligible-subset: BM25 `N`, lengths and
//! document frequencies count every live indexed text member `T`; the fixed
//! lexical anchor is the best BM25 over all of `T`; the vector anchor is the
//! Store norm enclosure over every live vector member `V`. Only candidate
//! generation is restricted. Fusion is Store policy v1, applied through the
//! fusion module's own per-candidate scorer with query-level weights.
//!
//! Components keep absence distinct from zero: a node without a vector has
//! no vector component (zero contribution, never distance zero), and a live
//! indexed text with no query match scores exactly zero, distinct from a node
//! with no indexed text. Every retained candidate is cross-scored in every
//! modality it has, so leaving one producer's window never means absence.
use super::rank::{Eligible, Route, memory_error, route};
use super::{NativeRetrievalContext, PreparedEligibility, PreparedNativeVector, RetrievalError};
use crate::fts::bm25::{Bm25Params, CorpusStats, Df, DocLen, TermScorer, Tf};
use crate::fts::graph_build::{GraphQueryTerms, analyze_query};
use crate::fts::index::DEFAULT_FIELD;
use crate::fts::sealed::{SealedSegment, TermStream};
use crate::fts::search::FieldWeights;
use crate::fts::tokenizer::Analyzer;
use crate::fusion::{
    ALPHA_POLICY_VERSION, HYBRID_NORMALIZATION_POLICY_VERSION, HYBRID_WINDOW_FLOOR,
    HYBRID_WINDOW_PER_K, HybridQuery, RuleSignals, StorePolicyScorer,
};
use crate::graph::search::GraphSegmentNormRange;
use crate::lifecycle::SearchTier;
use crate::property_graph::query::completed::{
    ActualTier, CandidateCoverage, LegState, ScorePrecision,
};
use crate::property_graph::query::eligibility::Eligibility;
use crate::property_graph::query::plan::SearchBounds;
use crate::property_graph::query::resources::{QueryArena, QueryMemory, QueryReservation};
use crate::property_graph::query::runtime::{RuntimeContext, WorkKind};
use crate::property_graph::storage::search::Modality;
use crate::property_graph::storage::tree::directory::{TreeError, TreeResources};
use crate::property_graph::{GraphRevision, NodeId};

/// ZE-103 shared limit on analyzed terms in one lexical argument.
pub(crate) const MAX_QUERY_TERMS: usize = 64;
/// Largest producer window a nested vector ranking may request.
const MAX_PRODUCER_WINDOW: usize = 4096;

/// One lexical query analyzed against this view's text interpretation.
pub(crate) struct PreparedNativeText<'e, 'm> {
    view: *const crate::property_graph::query::QueryView,
    terms: GraphQueryTerms<'m>,
    exact_terms: usize,
    quoted_phrase: bool,
    identifier_token: bool,
    expanded: Option<(Vec<String>, QueryReservation<'m, 'm>)>,
    eligibility: PreparedEligibility<'e>,
}

impl PreparedNativeText<'_, '_> {
    pub(crate) fn term_count(&self) -> usize {
        self.expanded
            .as_ref()
            .map_or(self.terms.len(), |(terms, _)| terms.len())
    }
    fn term(&self, index: usize) -> Option<&str> {
        match &self.expanded {
            Some((terms, _)) => terms.get(index).map(String::as_str),
            None => self.terms.term(index),
        }
    }
}

/// One ranked lexical node. BM25 is higher-first; ties order by full ID.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TextHit {
    pub(crate) node: NodeId,
    pub(crate) revision: GraphRevision,
    pub(crate) bm25: f64,
}

/// One fused node with both components; `None` is an absent modality.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct HybridHit {
    pub(crate) node: NodeId,
    pub(crate) revision: GraphRevision,
    pub(crate) fused: f64,
    /// Exact original-domain squared-L2; absent when the node has no vector.
    pub(crate) vector: Option<f64>,
    /// Exact BM25; zero for present nonmatching text, absent without text.
    pub(crate) lexical: Option<f64>,
}

/// Full-live lexical statistics and membership established by one query.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TextDomainReport {
    pub(crate) leg: LegState,
    /// Live indexed text members `T`, the BM25 `N`.
    pub(crate) live_members: u64,
    /// Analyzed length of every member of `T`.
    pub(crate) total_tokens: u64,
    pub(crate) eligible_members: u64,
    pub(crate) eligible_matches: u64,
    /// Best BM25 over all of `T`; absent when `T` has no match.
    pub(crate) maximum: Option<f64>,
    pub(crate) terms: u64,
    pub(crate) postings: u64,
}

/// Lexical ranking is exhaustive over the eligible domain.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TextRankReport {
    pub(crate) domain: TextDomainReport,
    pub(crate) coverage: CandidateCoverage,
}

/// Retained provenance for one hybrid invocation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct HybridRankReport {
    pub(crate) requested_tier: Option<SearchTier>,
    /// Actual vector route; absent when no eligible vector member existed.
    pub(crate) actual_tier: Option<ActualTier>,
    pub(crate) precision: ScorePrecision,
    pub(crate) coverage: CandidateCoverage,
    pub(crate) vector_leg: LegState,
    pub(crate) lexical_leg: LegState,
    pub(crate) text: TextDomainReport,
    pub(crate) live_vector_members: u64,
    pub(crate) eligible_vector_members: u64,
    /// Full-live `V` norm enclosure; absent when `V` is empty.
    pub(crate) vector_ceiling: Option<f64>,
    /// Query-level vector weight after the empty-eligible-leg rule.
    pub(crate) effective_alpha: f64,
    pub(crate) normalization_version: u16,
    pub(crate) rules_version: u16,
    pub(crate) candidate_count: u64,
    pub(crate) cross_scored_count: u64,
    pub(crate) cross_score_complete: bool,
    pub(crate) traversed_sources: u64,
    pub(crate) fallback_count: u64,
}

/// Complete bounded ranking owned by the query memory that admitted it.
pub(crate) struct Ranked<'m, 'g, T, R> {
    hits: Option<QueryArena<'m, 'g, T>>,
    report: R,
}

impl<T, R: Copy> Ranked<'_, '_, T, R> {
    pub(crate) fn hits(&self) -> &[T] {
        self.hits.as_ref().map_or(&[], QueryArena::as_slice)
    }

    pub(crate) const fn report(&self) -> R {
        self.report
    }
}

pub(crate) type RankedText<'m, 'g> = Ranked<'m, 'g, TextHit, TextRankReport>;
pub(crate) type RankedHybrid<'m, 'g> = Ranked<'m, 'g, HybridHit, HybridRankReport>;

fn text_better(left: &TextHit, right: &TextHit) -> bool {
    match right.bm25.total_cmp(&left.bm25) {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Greater => false,
        std::cmp::Ordering::Equal => left.node < right.node,
    }
}

fn hybrid_better(left: &HybridHit, right: &HybridHit) -> bool {
    match right.fused.total_cmp(&left.fused) {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Greater => false,
        std::cmp::Ordering::Equal => left.node < right.node,
    }
}

/// Bounded best-first selection in one fixed-capacity charged arena.
struct Best<'m, 'g, T> {
    k: usize,
    hits: QueryArena<'m, 'g, T>,
    better: fn(&T, &T) -> bool,
}

impl<'m, 'g, T: Copy> Best<'m, 'g, T> {
    fn new(
        memory: &'m QueryMemory<'g>,
        k: usize,
        better: fn(&T, &T) -> bool,
    ) -> Result<Self, RetrievalError> {
        Ok(Self {
            k,
            hits: QueryArena::new(memory, k).map_err(memory_error)?,
            better,
        })
    }

    fn offer(&mut self, hit: T, resources: &mut TreeResources<'_>) -> Result<(), RetrievalError> {
        if self.k == 0 {
            return Ok(());
        }
        if self.hits.len() < self.k {
            self.hits.push(hit).map_err(memory_error)?;
        } else {
            let worst = self
                .hits
                .as_mut_slice()
                .last_mut()
                .ok_or(RetrievalError::Invariant("empty full best-k"))?;
            if !(self.better)(&hit, worst) {
                return Ok(());
            }
            *worst = hit;
        }
        let better = self.better;
        let hits = self.hits.as_mut_slice();
        let mut position = hits.len().saturating_sub(1);
        while position > 0 {
            resources.step(1)?;
            let previous = position - 1;
            let (Some(left), Some(right)) = (hits.get(previous), hits.get(position)) else {
                return Err(RetrievalError::Invariant("best-k position"));
            };
            if !better(right, left) {
                break;
            }
            hits.swap(previous, position);
            position = previous;
        }
        Ok(())
    }
}

/// One eligible live text member and its exact BM25 (zero: no match).
#[derive(Clone, Copy, Debug)]
struct TextMember {
    node: NodeId,
    revision: GraphRevision,
    bm25: f64,
}

/// Full-live lexical domain plus every eligible member, ascending by node.
struct TextDomain<'m, 'g> {
    report: TextDomainReport,
    members: QueryArena<'m, 'g, TextMember>,
    rarest_document_frequency: Option<u64>,
}

impl TextDomain<'_, '_> {
    /// Present modality returns the exact score, including zero.
    fn component(&self, node: NodeId) -> Option<&TextMember> {
        let members = self.members.as_slice();
        members
            .binary_search_by(|member| member.node.cmp(&node))
            .ok()
            .and_then(|index| members.get(index))
    }
}

fn revision(value: u64) -> Result<GraphRevision, RetrievalError> {
    GraphRevision::new(value).map_err(|_| RetrievalError::Invariant("zero revision"))
}

fn fusion(error: crate::fusion::FusionError) -> RetrievalError {
    RetrievalError::Fusion(error)
}

fn leg_for(live: u64, eligible: u64) -> LegState {
    if live == 0 {
        LegState::NoIndexedPopulation
    } else if eligible == 0 {
        LegState::NoEligibleMembers
    } else {
        LegState::Nonempty
    }
}

/// Opens one query term's postings with its exact stream capacity charged.
fn open_term<'s, 'm, 'g>(
    memory: &'m QueryMemory<'g>,
    segment: &'s SealedSegment,
    term: &str,
    weights: &FieldWeights,
) -> Result<Option<(TermStream<'s>, QueryReservation<'m, 'g>)>, RetrievalError> {
    let bytes = TermStream::allocation_bytes(segment, term.as_bytes(), weights)
        .ok_or(RetrievalError::Memory)?;
    let charge = memory.reserve(bytes).map_err(memory_error)?;
    let mut work = crate::fts::control::WorkCheck::new(|| Ok::<(), std::convert::Infallible>(()));
    let stream =
        match TermStream::open_sized_controlled(segment, term.as_bytes(), weights, &mut work) {
            Ok(stream) => stream,
            Err(never) => match never {},
        };
    Ok(stream.map(|stream| (stream, charge)))
}

/// Streams live vector members, reading coordinates of `selected` ones into
/// one charged scratch and handing them to `visit`.
struct VectorStream<'q> {
    query: &'q [f32],
    live: u64,
    eligible: u64,
}

impl<'view, 's, 'lease, 'm, 'g> NativeRetrievalContext<'view, 's, 'lease, 'm, 'g> {
    /// Analyzes one lexical argument with this view's own text analyzer.
    pub(crate) fn prepare_text<'e, 'v, 'em, 'eg>(
        &self,
        analyzer: &Analyzer,
        text: &str,
        eligibility: Eligibility<'e, 'v, 'em, 'eg>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<PreparedNativeText<'e, 'm>, RetrievalError>
    where
        'g: 'm,
    {
        self.validate_runtime(runtime)?;
        if analyzer.epoch() != self.interpretation.lexical() {
            return Err(RetrievalError::AnalyzerMismatch);
        }
        let eligibility = match eligibility {
            Eligibility::AllIndexed => PreparedEligibility::AllIndexed,
            Eligibility::Set(set) => PreparedEligibility::Set(
                set.ids_for(self.query_view)
                    .map_err(RetrievalError::Eligibility)?,
            ),
        };
        let memory: &'m QueryMemory<'m> = self.memory;
        let terms =
            analyze_query(analyzer, text, memory, runtime).map_err(RetrievalError::Lexical)?;
        if terms.len() > MAX_QUERY_TERMS {
            return Err(RetrievalError::LexicalTerms {
                count: terms.len(),
                limit: MAX_QUERY_TERMS,
            });
        }
        let exact_terms = terms.len();
        let identifier_token = terms.has_identifier();
        Ok(PreparedNativeText {
            view: self.query_view,
            terms,
            expanded: None,
            quoted_phrase: text.contains('"'),
            identifier_token,
            exact_terms,
            eligibility,
        })
    }

    pub(crate) fn prepare_text_with_options<'e, 'v, 'em, 'eg>(
        &self,
        analyzer: &Analyzer,
        text: &str,
        eligibility: Eligibility<'e, 'v, 'em, 'eg>,
        options: crate::property_graph::query::plan::SearchOptions,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<PreparedNativeText<'e, 'm>, RetrievalError>
    where
        'g: 'm,
    {
        let mut prepared = self.prepare_text(analyzer, text, eligibility, runtime)?;
        if !options.last_as_prefix || prepared.terms.len() == 0 {
            return Ok(prepared);
        }
        let prefix = prepared
            .terms
            .term(prepared.terms.len() - 1)
            .ok_or(RetrievalError::Invariant("prefix term"))?;
        let memory: &'m QueryMemory<'m> = self.memory;
        let mut charge = memory.reserve(0).map_err(memory_error)?;
        let mut terms: Vec<String> = Vec::new();
        let mut copy = |term: &str| -> Result<(), RetrievalError> {
            if terms.len() >= MAX_QUERY_TERMS {
                return Err(RetrievalError::LexicalTerms {
                    count: terms.len() + 1,
                    limit: MAX_QUERY_TERMS,
                });
            }
            let mut text = String::new();
            charge
                .resize(
                    charge
                        .bytes()
                        .checked_add(term.len() + std::mem::size_of::<String>())
                        .ok_or(RetrievalError::Memory)?,
                )
                .map_err(memory_error)?;
            text.try_reserve_exact(term.len())
                .map_err(|_| RetrievalError::Memory)?;
            text.push_str(term);
            terms
                .try_reserve_exact(1)
                .map_err(|_| RetrievalError::Memory)?;
            terms.push(text);
            Ok(())
        };
        for index in 0..prepared.terms.len() - 1 {
            copy(
                prepared
                    .terms
                    .term(index)
                    .ok_or(RetrievalError::Invariant("prefix leading term"))?,
            )?;
        }

        let leading = terms.len();
        prepared.exact_terms = leading;
        let sparse = self.view.sparse_view(runtime)?;
        let mut resources = TreeResources::for_query(runtime)?;
        let mut sources = sparse.sources(Modality::Text, &mut resources)?;
        while let Some(source) = sources.next(&mut resources)? {
            let segment = source
                .lexical(&mut resources)?
                .ok_or(RetrievalError::Invariant("prefix text source"))?;
            for candidate in segment.terms() {
                resources.step(1)?;
                if !candidate.starts_with(prefix.as_bytes()) {
                    continue;
                }
                let candidate = std::str::from_utf8(candidate)
                    .map_err(|_| RetrievalError::Invariant("indexed term UTF-8"))?;
                if terms
                    .get(leading..)
                    .ok_or(RetrievalError::Invariant("prefix terms"))?
                    .iter()
                    .any(|term| term == candidate)
                {
                    continue;
                }
                if terms.len() >= MAX_QUERY_TERMS {
                    return Err(RetrievalError::LexicalTerms {
                        count: terms.len() + 1,
                        limit: MAX_QUERY_TERMS,
                    });
                }
                let mut text = String::new();
                charge
                    .resize(
                        charge
                            .bytes()
                            .checked_add(candidate.len() + std::mem::size_of::<String>())
                            .ok_or(RetrievalError::Memory)?,
                    )
                    .map_err(memory_error)?;
                text.try_reserve_exact(candidate.len())
                    .map_err(|_| RetrievalError::Memory)?;
                text.push_str(candidate);
                terms
                    .try_reserve_exact(1)
                    .map_err(|_| RetrievalError::Memory)?;
                terms.push(text);
            }
        }
        // Input token order remains fixed; expansion order is bytewise deterministic.
        terms
            .get_mut(leading..)
            .ok_or(RetrievalError::Invariant("prefix sorting"))?
            .sort();
        prepared.expanded = Some((terms, charge));
        Ok(prepared)
    }

    fn validate_text(&self, prepared: &PreparedNativeText<'_, '_>) -> Result<(), RetrievalError> {
        if !std::ptr::eq(prepared.view, self.query_view) {
            return Err(RetrievalError::Storage(TreeError::Invalid(
                "foreign prepared native text",
            )));
        }
        Ok(())
    }

    /// Full-live BM25 statistics, the fixed lexical anchor and the exact
    /// score of every eligible live text member.
    fn text_domain(
        &self,
        prepared: &PreparedNativeText<'_, '_>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<TextDomain<'m, 'g>, RetrievalError> {
        let eligible = match prepared.eligibility {
            PreparedEligibility::AllIndexed => Eligible::All,
            PreparedEligibility::Set(ids) => Eligible::Set(ids),
        };
        let terms = prepared;
        let weights = FieldWeights::flat(&[DEFAULT_FIELD]);
        let _weights = self
            .memory
            .reserve(weights.allocation_bytes().ok_or(RetrievalError::Memory)?)
            .map_err(memory_error)?;
        let mut frequencies =
            QueryArena::<u32>::new(self.memory, terms.term_count()).map_err(memory_error)?;
        for _ in 0..terms.term_count() {
            frequencies.push(0).map_err(memory_error)?;
        }
        let mut report = TextDomainReport {
            leg: LegState::NoIndexedPopulation,
            live_members: 0,
            total_tokens: 0,
            eligible_members: 0,
            eligible_matches: 0,
            maximum: None,
            terms: terms.term_count() as u64,
            postings: 0,
        };
        let sparse = self.view.sparse_view(runtime)?;
        let mut resources = TreeResources::for_query(runtime)?;

        // Pass 1: membership, lengths and live document frequencies over T.
        let mut sources = sparse.sources(Modality::Text, &mut resources)?;
        while let Some(source) = sources.next(&mut resources)? {
            for ordinal in 0..source.row_count() {
                resources.step(1)?;
                let Some(member) = source.resolve_row(ordinal, &mut resources)? else {
                    continue;
                };
                report.live_members += 1;
                report.total_tokens = report
                    .total_tokens
                    .checked_add(u64::from(member.analyzed_length))
                    .ok_or(RetrievalError::Memory)?;
                if eligible.contains(member.node, &mut resources)? {
                    report.eligible_members += 1;
                }
            }
            let segment = source
                .lexical(&mut resources)?
                .ok_or(RetrievalError::Invariant(
                    "text source lacks its lexical region",
                ))?;
            for slot in 0..terms.term_count() {
                let term = terms
                    .term(slot)
                    .ok_or(RetrievalError::Invariant("query term"))?;
                let Some((mut stream, _charge)) = open_term(self.memory, segment, term, &weights)?
                else {
                    continue;
                };
                while let Some(row) = stream.current_row() {
                    resources.step(1)?;
                    resources.charge_query_work(WorkKind::LexicalPostings, 1)?;
                    report.postings += 1;
                    if source.is_live(row, &mut resources)? {
                        let frequency = frequencies
                            .as_mut_slice()
                            .get_mut(slot)
                            .ok_or(RetrievalError::Invariant("term frequency slot"))?;
                        *frequency = frequency.checked_add(1).ok_or(RetrievalError::Memory)?;
                    }
                    stream.advance();
                }
            }
        }
        drop(sources);

        let mut members = QueryArena::<TextMember>::new(
            self.memory,
            usize::try_from(report.eligible_members).map_err(|_| RetrievalError::Memory)?,
        )
        .map_err(memory_error)?;
        if report.live_members == 0 {
            // No live indexed text: BM25 statistics are undefined, not zero.
            resources.step(0)?;
            drop(resources);
            runtime.checkpoint().map_err(RetrievalError::Control)?;
            return Ok(TextDomain {
                report,
                members,
                rarest_document_frequency: (terms.exact_terms > 0).then_some(0),
            });
        }
        let statistics = CorpusStats::new(report.live_members, report.total_tokens)
            .map_err(|_| RetrievalError::Invariant("live text without analyzed tokens"))?;
        let mut scorers = QueryArena::<Option<TermScorer>>::new(self.memory, terms.term_count())
            .map_err(memory_error)?;
        for frequency in frequencies.as_slice() {
            scorers
                .push(
                    (*frequency > 0)
                        .then(|| TermScorer::new(Df(*frequency), &statistics, Bm25Params::beir())),
                )
                .map_err(memory_error)?;
        }
        let scoring = scorers.as_slice().iter().any(Option::is_some);

        // Pass 2: exact BM25 of every live row, summed in query-term order.
        let mut maximum = 0.0_f64;
        let mut sources = sparse.sources(Modality::Text, &mut resources)?;
        while let Some(source) = sources.next(&mut resources)? {
            let rows = source.row_count() as usize;
            let mut scores = QueryArena::<f64>::new(self.memory, if scoring { rows } else { 0 })
                .map_err(memory_error)?;
            if scoring {
                for _ in 0..rows {
                    scores.push(0.0).map_err(memory_error)?;
                }
                let segment = source
                    .lexical(&mut resources)?
                    .ok_or(RetrievalError::Invariant(
                        "text source lacks its lexical region",
                    ))?;
                let lengths = segment
                    .field_lengths(DEFAULT_FIELD)
                    .ok_or(RetrievalError::Invariant("text source lacks row lengths"))?;
                for (slot, scorer) in scorers.as_slice().iter().enumerate() {
                    let Some(scorer) = scorer else {
                        continue;
                    };
                    let term = terms
                        .term(slot)
                        .ok_or(RetrievalError::Invariant("query term"))?;
                    let Some((mut stream, _charge)) =
                        open_term(self.memory, segment, term, &weights)?
                    else {
                        continue;
                    };
                    while let Some(row) = stream.current_row() {
                        resources.step(1)?;
                        resources.charge_query_work(WorkKind::LexicalPostings, 1)?;
                        report.postings += 1;
                        let tf = stream.current_tf().unwrap_or(0);
                        let length = lengths
                            .get(row as usize)
                            .copied()
                            .ok_or(RetrievalError::Invariant("posting row beyond lengths"))?;
                        let score = scorer.score(Tf(tf), DocLen(length));
                        let entry = scores
                            .as_mut_slice()
                            .get_mut(row as usize)
                            .ok_or(RetrievalError::Invariant("posting row beyond source"))?;
                        *entry += score;
                        stream.advance();
                    }
                }
            }
            for ordinal in 0..source.row_count() {
                resources.step(1)?;
                let Some(member) = source.resolve_row(ordinal, &mut resources)? else {
                    continue;
                };
                let bm25 = scores
                    .as_slice()
                    .get(ordinal as usize)
                    .copied()
                    .unwrap_or(0.0);
                if !bm25.is_finite() || bm25 < 0.0 {
                    return Err(RetrievalError::Invariant("nonfinite BM25"));
                }
                maximum = maximum.max(bm25);
                if eligible.contains(member.node, &mut resources)? {
                    if bm25 > 0.0 {
                        report.eligible_matches += 1;
                    }
                    members
                        .push(TextMember {
                            node: member.node,
                            revision: revision(member.revision)?,
                            bm25,
                        })
                        .map_err(memory_error)?;
                }
            }
        }
        drop(sources);
        if members.len() as u64 != report.eligible_members {
            return Err(RetrievalError::Invariant(
                "text membership changed within one view",
            ));
        }
        members
            .as_mut_slice()
            .sort_unstable_by(|left, right| left.node.cmp(&right.node));
        for pair in members.as_slice().windows(2) {
            resources.step(1)?;
            if let [left, right] = pair
                && left.node == right.node
            {
                return Err(RetrievalError::Invariant(
                    "node has two live text memberships",
                ));
            }
        }
        resources.step(0)?;
        drop(resources);
        report.maximum = (maximum > 0.0).then_some(maximum);
        report.leg = if report.eligible_members == 0 {
            LegState::NoEligibleMembers
        } else if report.eligible_matches == 0 {
            LegState::NoQueryMatches
        } else {
            LegState::Nonempty
        };
        runtime.checkpoint().map_err(RetrievalError::Control)?;
        Ok(TextDomain {
            report,
            members,
            rarest_document_frequency: frequencies
                .as_slice()
                .iter()
                .take(terms.exact_terms)
                .copied()
                .min()
                .map(u64::from),
        })
    }

    /// Ranks the complete eligible lexical population of this admitted view
    /// against full-live BM25 statistics.
    pub(crate) fn rank_text(
        &self,
        prepared: &PreparedNativeText<'_, '_>,
        bounds: SearchBounds,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<RankedText<'m, 'g>, RetrievalError> {
        self.validate_runtime(runtime)?;
        self.validate_text(prepared)?;
        let domain = self.text_domain(prepared, runtime)?;
        let k = bounds.k() as usize;
        let mut hits = None;
        if domain.report.eligible_matches > 0 {
            if k > bounds.candidate_window() as usize {
                return Err(RetrievalError::CandidateWindow {
                    required: k,
                    window: bounds.candidate_window() as usize,
                });
            }
            let mut resources = TreeResources::for_query(runtime)?;
            let mut best = Best::new(self.memory, k, text_better)?;
            for member in domain.members.as_slice() {
                resources.step(1)?;
                if member.bm25 > 0.0 {
                    best.offer(
                        TextHit {
                            node: member.node,
                            revision: member.revision,
                            bm25: member.bm25,
                        },
                        &mut resources,
                    )?;
                }
            }
            drop(resources);
            hits = Some(best.hits);
        }
        runtime.checkpoint().map_err(RetrievalError::Control)?;
        Ok(Ranked {
            hits,
            report: TextRankReport {
                domain: domain.report,
                coverage: CandidateCoverage::Exact,
            },
        })
    }

    /// Visits every live vector member; `selected` members' original
    /// coordinates are streamed into one charged scratch for `visit`.
    fn stream_vectors(
        &self,
        stream: &mut VectorStream<'_>,
        eligible: &Eligible<'_>,
        only_eligible: bool,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
        mut visit: impl FnMut(
            NodeId,
            GraphRevision,
            bool,
            &[f32],
            &mut TreeResources<'_>,
        ) -> Result<(), RetrievalError>,
    ) -> Result<(), RetrievalError> {
        let dimensions = stream.query.len();
        let coordinates = u64::try_from(dimensions).map_err(|_| RetrievalError::Memory)?;
        let mut scratch = QueryArena::<f32>::new(self.memory, dimensions).map_err(memory_error)?;
        let sparse = self.view.sparse_view(runtime)?;
        let mut resources = TreeResources::for_query(runtime)?;
        let mut sources = sparse.sources(Modality::Vector, &mut resources)?;
        while let Some(source) = sources.next(&mut resources)? {
            for ordinal in 0..source.row_count() {
                resources.step(1)?;
                let Some(member) = source.resolve_row(ordinal, &mut resources)? else {
                    continue;
                };
                stream.live += 1;
                let is_eligible = eligible.contains(member.node, &mut resources)?;
                if is_eligible {
                    stream.eligible += 1;
                }
                if only_eligible && !is_eligible {
                    continue;
                }
                let vector = member
                    .vector
                    .ok_or(RetrievalError::Invariant("vector member lacks payload"))?;
                if vector.dimensions() as usize != dimensions {
                    return Err(RetrievalError::Invariant("stored vector dimensions"));
                }
                resources.charge_query_work(WorkKind::VectorCoordinates, coordinates)?;
                resources.charge_query_work(
                    WorkKind::VectorBytes,
                    coordinates.checked_mul(4).ok_or(RetrievalError::Memory)?,
                )?;
                scratch.clear();
                for dimension in 0..vector.dimensions() {
                    scratch
                        .push(vector.coordinate(dimension, &mut resources)?)
                        .map_err(memory_error)?;
                }
                visit(
                    member.node,
                    revision(member.revision)?,
                    is_eligible,
                    scratch.as_slice(),
                    &mut resources,
                )?;
            }
        }
        drop(sources);
        resources.step(0)?;
        drop(resources);
        runtime.checkpoint().map_err(RetrievalError::Control)?;
        Ok(())
    }

    /// Full-live `V` norm enclosure: the Store v1 upper vector anchor. It
    /// depends on live membership only, never on the eligible subset.
    fn vector_ceiling<'q>(
        &self,
        query: &'q [f32],
        eligible: &Eligible<'_>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<(VectorStream<'q>, Option<f64>), RetrievalError> {
        let mut stream = VectorStream {
            query,
            live: 0,
            eligible: 0,
        };
        let mut ceiling: Option<f64> = None;
        self.stream_vectors(&mut stream, eligible, false, runtime, |_, _, _, row, _| {
            let bound = GraphSegmentNormRange::from_exact_rows(row, query.len())
                .squared_l2_upper_bound(query);
            if !bound.is_finite() {
                return Err(RetrievalError::Invariant(
                    "vector norm enclosure is not a finite squared-L2 ceiling",
                ));
            }
            ceiling = Some(ceiling.map_or(bound, |current| current.max(bound)));
            Ok(())
        })?;
        Ok((stream, ceiling))
    }

    /// Exact original-domain squared-L2 for one node, absent without a vector.
    fn cross_vector<S, C>(
        node: NodeId,
        query: &[f32],
        scratch: &mut QueryArena<'m, 'g, f32>,
        sparse: &crate::property_graph::storage::search::SparseView<'_, '_, S, C>,
        resources: &mut TreeResources<'_>,
    ) -> Result<Option<(GraphRevision, f64)>, RetrievalError>
    where
        S: crate::property_graph::storage::tree::directory::BlockSource,
        C: crate::property_graph::storage::records::RecordCatalog<S>,
    {
        let Some(member) = sparse.lookup(Modality::Vector, node, resources)? else {
            return Ok(None);
        };
        let vector = member
            .vector
            .ok_or(RetrievalError::Invariant("vector member lacks payload"))?;
        if vector.dimensions() as usize != query.len() {
            return Err(RetrievalError::Invariant("stored vector dimensions"));
        }
        let coordinates = u64::try_from(query.len()).map_err(|_| RetrievalError::Memory)?;
        resources.charge_query_work(WorkKind::VectorCoordinates, coordinates)?;
        resources.charge_query_work(
            WorkKind::VectorBytes,
            coordinates.checked_mul(4).ok_or(RetrievalError::Memory)?,
        )?;
        scratch.clear();
        for dimension in 0..vector.dimensions() {
            scratch
                .push(vector.coordinate(dimension, resources)?)
                .map_err(memory_error)?;
        }
        Ok(Some((
            revision(member.revision)?,
            crate::quant::squared_l2_f64(query, scratch.as_slice()),
        )))
    }

    /// Ranks the eligible hybrid population under Store policy v1 with
    /// full-live anchors. Exact scores the entire eligible union; the other
    /// modes cross-score bounded producer windows and certify coverage only
    /// from an exact producer and a strict omitted-candidate score bound.
    pub(crate) fn rank_hybrid(
        &self,
        vector: &PreparedNativeVector<'_, '_>,
        text: &PreparedNativeText<'_, '_>,
        bounds: SearchBounds,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<RankedHybrid<'m, 'g>, RetrievalError> {
        self.validate_runtime(runtime)?;
        self.validate_text(text)?;
        if !std::ptr::eq(vector.view, self.query_view) {
            return Err(RetrievalError::Storage(TreeError::Invalid(
                "foreign prepared native vector",
            )));
        }
        let eligible = match (vector.eligibility(), &text.eligibility) {
            (PreparedEligibility::AllIndexed, PreparedEligibility::AllIndexed) => Eligible::All,
            (PreparedEligibility::Set(left), PreparedEligibility::Set(right))
                if std::ptr::eq(*left, *right) =>
            {
                Eligible::Set(left)
            }
            _ => return Err(RetrievalError::EligibilityMismatch),
        };
        let query = vector.coordinates();
        let k = bounds.k() as usize;
        let window = bounds.candidate_window() as usize;
        let (route, requested_tier) = route(vector.mode());

        let domain = self.text_domain(text, runtime)?;
        let (vectors, ceiling) = self.vector_ceiling(query, &eligible, runtime)?;
        let (live_vectors, eligible_vectors) = (vectors.live, vectors.eligible);
        let vector_leg = leg_for(live_vectors, eligible_vectors);
        let lexical_leg = domain.report.leg;
        let vector_anchor = if vector_leg == LegState::Nonempty {
            Some(ceiling.ok_or(RetrievalError::Invariant("live vectors without a ceiling"))?)
        } else {
            None
        };
        let lexical_anchor = if lexical_leg == LegState::Nonempty {
            Some(domain.report.maximum.ok_or(RetrievalError::Invariant(
                "eligible match without an anchor",
            ))?)
        } else {
            None
        };
        let mut fusion_query = HybridQuery::new(k);
        fusion_query.alpha = vector.options.alpha;
        fusion_query.rules_enabled = vector.options.rules_enabled;
        fusion_query.rule_signals = RuleSignals {
            quoted_phrase: text.quoted_phrase,
            identifier_token: text.identifier_token,
            rarest_exact_document_frequency: domain.rarest_document_frequency,
        };
        if let Some(rounds) = vector.options.max_rounds {
            fusion_query.max_rounds =
                usize::try_from(rounds).map_err(|_| RetrievalError::Memory)?;
        }
        let policy =
            StorePolicyScorer::new(&fusion_query, vector_anchor, lexical_anchor).map_err(fusion)?;
        let mut report = HybridRankReport {
            requested_tier,
            actual_tier: None,
            precision: ScorePrecision::NotApplicable,
            coverage: CandidateCoverage::Exact,
            vector_leg,
            lexical_leg,
            text: domain.report,
            live_vector_members: live_vectors,
            eligible_vector_members: eligible_vectors,
            vector_ceiling: ceiling,
            effective_alpha: policy.alpha(),
            normalization_version: HYBRID_NORMALIZATION_POLICY_VERSION,
            rules_version: ALPHA_POLICY_VERSION,
            candidate_count: 0,
            cross_scored_count: 0,
            cross_score_complete: true,
            traversed_sources: 0,
            fallback_count: 0,
        };
        let has_candidates = vector_leg == LegState::Nonempty || lexical_leg == LegState::Nonempty;
        if !has_candidates {
            // Both eligible legs are empty: zero rows with valid metadata.
            runtime.checkpoint().map_err(RetrievalError::Control)?;
            return Ok(Ranked { hits: None, report });
        }
        let windowed = route != Route::Exact
            && vector_leg == LegState::Nonempty
            && vector.options.max_rounds != Some(0);
        let hits = if windowed {
            let rounds = vector.options.max_rounds.unwrap_or(1);
            let mut width = HYBRID_WINDOW_PER_K
                .checked_mul(k)
                .ok_or(RetrievalError::Memory)?
                .max(HYBRID_WINDOW_FLOOR)
                .max(k)
                .min(MAX_PRODUCER_WINDOW);
            let mut result = None;
            for _ in 0..rounds {
                drop(result.take());
                result = Some(self.hybrid_windows(
                    vector,
                    &domain,
                    &policy,
                    bounds,
                    width,
                    &mut report,
                    runtime,
                )?);
                if report.coverage == CandidateCoverage::Exact || width == MAX_PRODUCER_WINDOW {
                    break;
                }
                width = width
                    .checked_mul(2)
                    .ok_or(RetrievalError::Memory)?
                    .min(MAX_PRODUCER_WINDOW);
            }
            result.ok_or(RetrievalError::Invariant("zero windowed hybrid rounds"))?
        } else {
            if k > window {
                return Err(RetrievalError::CandidateWindow {
                    required: k,
                    window,
                });
            }
            self.hybrid_exhaustive(query, &domain, &eligible, &policy, k, &mut report, runtime)?
        };
        runtime.checkpoint().map_err(RetrievalError::Control)?;
        Ok(Ranked {
            hits: Some(hits),
            report,
        })
    }

    /// Scores the full eligible union `E ∩ (V ∪ matching T)` exactly.
    #[allow(
        clippy::too_many_arguments,
        reason = "one exhaustive pass names both domains, the policy and the report"
    )]
    fn hybrid_exhaustive(
        &self,
        query: &[f32],
        domain: &TextDomain<'m, 'g>,
        eligible: &Eligible<'_>,
        policy: &StorePolicyScorer,
        k: usize,
        report: &mut HybridRankReport,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<QueryArena<'m, 'g, HybridHit>, RetrievalError> {
        let members = domain.members.as_slice();
        let mut covered =
            QueryArena::<bool>::new(self.memory, members.len()).map_err(memory_error)?;
        for _ in members {
            covered.push(false).map_err(memory_error)?;
        }
        let mut best = Best::new(self.memory, k, hybrid_better)?;
        let mut candidates = 0_u64;
        if report.vector_leg == LegState::Nonempty {
            let mut stream = VectorStream {
                query,
                live: 0,
                eligible: 0,
            };
            self.stream_vectors(
                &mut stream,
                eligible,
                true,
                runtime,
                |node, revision, _, row, resources| {
                    let distance = crate::quant::squared_l2_f64(query, row);
                    let index = members
                        .binary_search_by(|member| member.node.cmp(&node))
                        .ok();
                    let lexical = match index {
                        Some(index) => {
                            let member = members
                                .get(index)
                                .ok_or(RetrievalError::Invariant("text member index"))?;
                            if member.revision != revision {
                                return Err(RetrievalError::Invariant(
                                    "vector and text memberships disagree on revision",
                                ));
                            }
                            *covered
                                .as_mut_slice()
                                .get_mut(index)
                                .ok_or(RetrievalError::Invariant("covered index"))? = true;
                            Some(member.bm25)
                        }
                        None => None,
                    };
                    candidates += 1;
                    best.offer(
                        HybridHit {
                            node,
                            revision,
                            fused: policy.score(Some(distance), lexical).map_err(fusion)?,
                            vector: Some(distance),
                            lexical,
                        },
                        resources,
                    )
                },
            )?;
            if stream.eligible != report.eligible_vector_members
                || stream.live != report.live_vector_members
            {
                return Err(RetrievalError::Invariant(
                    "vector membership changed within one view",
                ));
            }
            report.actual_tier = Some(ActualTier::Exact);
            report.precision = ScorePrecision::Original;
        }
        if report.lexical_leg == LegState::Nonempty {
            let mut resources = TreeResources::for_query(runtime)?;
            for (member, covered) in members.iter().zip(covered.as_slice()) {
                resources.step(1)?;
                // Uncovered eligible members have no vector: every eligible
                // vector member was streamed above and marked its text.
                if *covered || member.bm25 <= 0.0 {
                    continue;
                }
                candidates += 1;
                best.offer(
                    HybridHit {
                        node: member.node,
                        revision: member.revision,
                        fused: policy.score(None, Some(member.bm25)).map_err(fusion)?,
                        vector: None,
                        lexical: Some(member.bm25),
                    },
                    &mut resources,
                )?;
            }
        }
        report.candidate_count = candidates;
        report.cross_scored_count = candidates;
        report.coverage = CandidateCoverage::Exact;
        Ok(best.hits)
    }

    /// Cross-scores the union of the eligible vector producer window and the
    /// exact lexical window. An exact vector producer can certify the result.
    #[allow(
        clippy::too_many_arguments,
        reason = "one windowed round names both producers, the policy and the report"
    )]
    fn hybrid_windows(
        &self,
        vector: &PreparedNativeVector<'_, '_>,
        domain: &TextDomain<'m, 'g>,
        policy: &StorePolicyScorer,
        bounds: SearchBounds,
        width: usize,
        report: &mut HybridRankReport,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<QueryArena<'m, 'g, HybridHit>, RetrievalError> {
        let k = bounds.k() as usize;
        let producer = SearchBounds::new(
            i64::try_from(width).map_err(|_| RetrievalError::Memory)?,
            u64::from(bounds.candidate_window()),
        )
        .map_err(|_| RetrievalError::Invariant("hybrid producer bounds"))?;
        let ranked = self.rank_vector(vector, producer, runtime)?;
        let vector_report = ranked.report();
        if vector_report.eligible_members != report.eligible_vector_members {
            return Err(RetrievalError::Invariant(
                "vector membership changed within one view",
            ));
        }
        report.actual_tier = vector_report.actual_tier;
        report.traversed_sources = vector_report.traversed_sources;
        report.fallback_count = vector_report.fallback_count;
        // Rescoring fixes every retained vector distance in the original
        // domain; it never proves omitted candidates cannot win.
        report.precision = ScorePrecision::Original;
        let rescore = vector_report.precision != ScorePrecision::Original;

        let members = domain.members.as_slice();
        let query = vector.coordinates();
        let mut lexical_window = Best::new(self.memory, width.min(members.len()), text_better)?;
        let union_capacity = ranked
            .hits()
            .len()
            .checked_add(lexical_window.k)
            .ok_or(RetrievalError::Memory)?;
        if union_capacity > bounds.candidate_window() as usize {
            return Err(RetrievalError::CandidateWindow {
                required: union_capacity,
                window: bounds.candidate_window() as usize,
            });
        }
        let mut seen =
            QueryArena::<NodeId>::new(self.memory, union_capacity).map_err(memory_error)?;
        let mut scratch = QueryArena::<f32>::new(self.memory, query.len()).map_err(memory_error)?;
        let mut best = Best::new(self.memory, k, hybrid_better)?;
        let sparse = self.view.sparse_view(runtime)?;
        let mut resources = TreeResources::for_query(runtime)?;
        for member in members {
            resources.step(1)?;
            if member.bm25 > 0.0 {
                lexical_window.offer(
                    TextHit {
                        node: member.node,
                        revision: member.revision,
                        bm25: member.bm25,
                    },
                    &mut resources,
                )?;
            }
        }
        let mut candidates = 0_u64;
        for hit in ranked.hits() {
            resources.step(1)?;
            let distance = if rescore {
                let (revision, distance) =
                    Self::cross_vector(hit.node, query, &mut scratch, &sparse, &mut resources)?
                        .ok_or(RetrievalError::Invariant(
                            "ranked vector node lost its vector",
                        ))?;
                if revision != hit.revision {
                    return Err(RetrievalError::Invariant("rescored vector revision"));
                }
                distance
            } else {
                hit.distance
            };
            let lexical = domain.component(hit.node).map(|member| member.bm25);
            seen.push(hit.node).map_err(memory_error)?;
            candidates += 1;
            best.offer(
                HybridHit {
                    node: hit.node,
                    revision: hit.revision,
                    fused: policy.score(Some(distance), lexical).map_err(fusion)?,
                    vector: Some(distance),
                    lexical,
                },
                &mut resources,
            )?;
        }
        seen.as_mut_slice().sort_unstable();
        for hit in lexical_window.hits.as_slice() {
            resources.step(1)?;
            if seen.as_slice().binary_search(&hit.node).is_ok() {
                continue;
            }
            // Outside the vector window is not absence: cross-score the
            // node's own vector whenever it has one.
            let distance =
                match Self::cross_vector(hit.node, query, &mut scratch, &sparse, &mut resources)? {
                    Some((revision, distance)) => {
                        if revision != hit.revision {
                            return Err(RetrievalError::Invariant("cross-scored vector revision"));
                        }
                        Some(distance)
                    }
                    None => None,
                };
            candidates += 1;
            best.offer(
                HybridHit {
                    node: hit.node,
                    revision: hit.revision,
                    fused: policy.score(distance, Some(hit.bm25)).map_err(fusion)?,
                    vector: distance,
                    lexical: Some(hit.bm25),
                },
                &mut resources,
            )?;
        }
        // A candidate omitted by the union is outside both windows. For
        // exact producers its vector distance is at least the last retained
        // distance and its BM25 is at most the last retained score. An
        // exhausted leg contributes zero: omitted nodes lack that component.
        // Equality cannot certify the final node-ID tie break.
        let certified = if vector_report.coverage == CandidateCoverage::Exact
            && vector_report.precision == ScorePrecision::Original
        {
            let vector_exhausted = ranked.hits().len() as u64 == report.eligible_vector_members;
            let lexical_exhausted =
                lexical_window.hits.len() as u64 == domain.report.eligible_matches;
            if vector_exhausted && lexical_exhausted {
                true
            } else {
                let distance = if vector_exhausted {
                    None
                } else {
                    Some(
                        ranked
                            .hits()
                            .last()
                            .ok_or(RetrievalError::Invariant(
                                "unexhausted vector window is empty",
                            ))?
                            .distance,
                    )
                };
                let lexical = if lexical_exhausted {
                    None
                } else {
                    Some(
                        lexical_window
                            .hits
                            .as_slice()
                            .last()
                            .ok_or(RetrievalError::Invariant(
                                "unexhausted lexical window is empty",
                            ))?
                            .bm25,
                    )
                };
                let omitted = policy.score(distance, lexical).map_err(fusion)?;
                best.hits.len() == k
                    && best
                        .hits
                        .as_slice()
                        .last()
                        .is_some_and(|hit| hit.fused > omitted)
            }
        } else {
            false
        };
        resources.step(0)?;
        drop(resources);
        drop(ranked);
        report.candidate_count = candidates;
        report.cross_scored_count = candidates;
        report.coverage = if certified {
            CandidateCoverage::Exact
        } else if report.lexical_leg == LegState::Nonempty {
            CandidateCoverage::Approximate
        } else {
            // With no eligible lexical match the fused order is the vector
            // order, so the vector producer's own coverage carries over.
            vector_report.coverage
        };
        Ok(best.hits)
    }
}
