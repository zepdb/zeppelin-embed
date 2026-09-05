//! Real-Store adapter for the hybrid-fusion adversarial campaign.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use tempfile::tempdir;
use zeppelin_embed::fts::index::DEFAULT_FIELD;
use zeppelin_embed::fts::search::TermQuery;
use zeppelin_embed::fusion::{
    FusionError, FusionLeg, FusionMethod, FusionTermination, HybridQuery, LegFailureKind,
    LexicalCandidate, VectorCandidate, execute_hybrid,
};
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::{
    CancelToken, HybridLegTestFault, OpenOptions, QueryControl, SearchOptions, SearchTier, Store,
    StoreTestDependencies, SystemMonotonicClock,
};
use zeppelin_embed::vfs::StdVfs;
use zeppelin_embed_adversarial_oracle::hybrid_fusion::{
    FusedHitFact, FusionCaseInput, FusionCaseObserved, FusionLegFact, FusionMethodFact,
    FusionReportFact, FusionTerminationFact, HybridInput, HybridObserved, LegFailureFact,
    LegFaultObserved, RankedScore, StorePolicyFact,
};

use super::runner::FrozenStoreFixture;

const MAIN_TERM: &[u8] = b"zeppelin";
const RRF_TERM: &[u8] = b"airship";
const SINGLE_TERM: &[u8] = b"bronz";
const EMPTY_TERM: &[u8] = b"absent";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HybridOperationKind {
    Provenance,
    Normalization,
    BoundedFusion,
    Rrf,
    Legs,
}

impl HybridOperationKind {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Provenance => "provenance",
            Self::Normalization => "normalization",
            Self::BoundedFusion => "bounded-fusion",
            Self::Rrf => "rrf-fallback",
            Self::Legs => "legs",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HybridFaultKind {
    VectorLegError,
    LexicalLegError,
    DualFailureOrder,
    LegPanic,
    EstimatedScore,
    NonfiniteScore,
    CancelClose,
}

impl HybridFaultKind {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::VectorLegError => "vector-leg-error",
            Self::LexicalLegError => "lexical-leg-error",
            Self::DualFailureOrder => "dual-failure-order",
            Self::LegPanic => "leg-panic",
            Self::EstimatedScore => "estimated-score",
            Self::NonfiniteScore => "nonfinite-score",
            Self::CancelClose => "cancel-close",
        }
    }

    #[must_use]
    pub const fn operation(self) -> HybridOperationKind {
        match self {
            Self::EstimatedScore => HybridOperationKind::Provenance,
            Self::NonfiniteScore => HybridOperationKind::Normalization,
            Self::VectorLegError
            | Self::LexicalLegError
            | Self::DualFailureOrder
            | Self::LegPanic
            | Self::CancelClose => HybridOperationKind::Legs,
        }
    }

    #[must_use]
    pub const fn site(self) -> &'static str {
        match self {
            Self::VectorLegError => "hybrid.vector-leg",
            Self::LexicalLegError => "hybrid.lexical-leg",
            Self::DualFailureOrder => "hybrid.dual-leg",
            Self::LegPanic => "hybrid.leg-panic",
            Self::EstimatedScore => "hybrid.vector-provenance",
            Self::NonfiniteScore => "hybrid.normalize",
            Self::CancelClose => "hybrid.cancel-close",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HybridFaultReceipt {
    pub fault: HybridFaultKind,
    pub operation: HybridOperationKind,
    pub site: &'static str,
    pub cardinality: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HybridInvariantEvidence {
    I45 {
        input: HybridInput,
        observed: HybridObserved,
    },
    I46 {
        input: HybridInput,
        observed: HybridObserved,
    },
    I47 {
        input: HybridInput,
        observed: HybridObserved,
    },
    I48 {
        input: HybridInput,
        observed: HybridObserved,
    },
    I49 {
        input: HybridInput,
        observed: HybridObserved,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HybridOperationEvidence {
    pub invariants: Vec<HybridInvariantEvidence>,
    pub receipts: Vec<HybridFaultReceipt>,
    pub clean_control_passed: bool,
}

#[derive(Clone, Debug)]
struct FixtureDocument {
    id: u64,
    vector: Vec<f32>,
    text: String,
    deleted: bool,
}

#[derive(Clone, Copy, Debug)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next() as usize) % bound
        }
    }
}

pub struct HybridEpisode {
    input: HybridInput,
    observed: HybridObserved,
    frozen: FrozenStoreFixture,
    vector_query: Vec<f32>,
}

fn component(rng: &mut SplitMix64) -> f32 {
    let numerator = i32::try_from(rng.next() % 2_001).unwrap_or(0) - 1_000;
    numerator as f32 / 137.0
}

fn derive_documents(seed: u64) -> (Vec<FixtureDocument>, Vec<f32>, usize, usize, usize) {
    let mut rng = SplitMix64::new(seed ^ 0x4859_4252_4944_4655);
    let count = 40 + rng.below(161);
    let dimensions = if rng.next() & 1 == 0 { 8 } else { 32 };
    let active_docs = 3 + rng.below(6);
    let minimum_deleted = count.saturating_mul(5).div_ceil(100);
    let maximum_deleted = count.saturating_mul(10) / 100;
    let deleted_docs = minimum_deleted
        + rng.below(
            maximum_deleted
                .saturating_sub(minimum_deleted)
                .saturating_add(1),
        );
    let query = (0..dimensions)
        .map(|_| component(&mut rng))
        .collect::<Vec<_>>();
    let id_limit = (1_u64 << 52).saturating_sub(count as u64).saturating_sub(1);
    let id_base = 1 + rng.next() % id_limit;
    let mut documents = Vec::with_capacity(count);
    for index in 0..count {
        let vector = if index == 1 {
            documents
                .first()
                .map_or_else(Vec::new, |document: &FixtureDocument| {
                    document.vector.clone()
                })
        } else {
            (0..dimensions)
                .map(|_| component(&mut rng))
                .collect::<Vec<_>>()
        };
        let text = match index {
            0 | 1 => "zeppelin airship amber cobalt".to_owned(),
            2 => "zeppelin zeppelin bronze amber".to_owned(),
            _ => match index % 4 {
                0 => "zeppelin amber".to_owned(),
                1 => "zeppelin zeppelin cobalt".to_owned(),
                2 => "amber cobalt delta".to_owned(),
                _ => "zeppelin delta echo cobalt amber".to_owned(),
            },
        };
        documents.push(FixtureDocument {
            id: id_base + (count - index) as u64,
            vector,
            text,
            deleted: false,
        });
    }
    let sealed_count = count - active_docs;
    let mut deleted = BTreeSet::new();
    while deleted.len() < deleted_docs {
        deleted.insert(4 + rng.below(sealed_count - 4));
    }
    for index in deleted {
        if let Some(document) = documents.get_mut(index) {
            document.deleted = true;
        }
    }
    (documents, query, dimensions, active_docs, deleted_docs)
}

fn ingest_documents(store: &Store, documents: &[FixtureDocument]) -> Result<(), String> {
    let batch = documents
        .iter()
        .map(|document| {
            IngestDocument::new(
                DocumentVersion::new(DocId::new(u128::from(document.id)), Revision::new(1)),
                document.vector.clone(),
            )
            .with_text(document.text.clone())
        })
        .collect::<Vec<_>>();
    store
        .ingest(IngestBatch::new(batch))
        .map_err(|error| format!("ingest hybrid fixture: {error}"))?;
    Ok(())
}

fn vector_scores(
    store: &Store,
    documents: &[FixtureDocument],
    query: &[f32],
    k: usize,
) -> Result<(Vec<RankedScore>, f64), String> {
    let independent = documents
        .iter()
        .filter(|document| !document.deleted)
        .map(|document| {
            let squared_l2 = document
                .vector
                .iter()
                .zip(query)
                .map(|(left, right)| {
                    let delta = f64::from(*left) - f64::from(*right);
                    delta * delta
                })
                .sum::<f64>();
            (document.id, f64::from(squared_l2 as f32).to_bits())
        })
        .collect::<BTreeMap<_, _>>();
    let outcome = store
        .search(
            SearchRequest::new(query),
            k,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| format!("observe public exact vector leg: {error}"))?;
    let ceiling = outcome
        .vector_ceiling
        .ok_or_else(|| "missing validated vector ceiling".to_owned())?;
    let mut scores = outcome.candidates
        .into_iter()
        .map(|candidate| {
            let document = candidate
                .document()
                .ok_or_else(|| "public exact vector leg omitted document identity".to_owned())?;
            let id = doc_id_u64(document.doc_id())?;
            let score_bits = independent
                .get(&id)
                .copied()
                .ok_or_else(|| format!("public exact vector leg returned unknown document {id}"))?;
            let observed_bits = (-f64::from(candidate.score())).to_bits();
            if observed_bits != score_bits {
                return Err(format!(
                    "independent squared-L2 differs for document {id}: expected bits {score_bits} observed {observed_bits}"
                ));
            }
            Ok(RankedScore::new(id, score_bits))
        })
        .collect::<Result<Vec<_>, String>>()?;
    scores.sort_by(|left, right| {
        f64::from_bits(left.score_bits)
            .total_cmp(&f64::from_bits(right.score_bits))
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok((scores, ceiling))
}

fn term_query(term: &[u8]) -> TermQuery {
    TermQuery::flat(vec![term.to_vec()], &[DEFAULT_FIELD])
}

fn doc_id_u64(id: DocId) -> Result<u64, String> {
    u64::try_from(id.get()).map_err(|_| format!("hybrid document id {} exceeds u64", id.get()))
}

fn lexical_scores(store: &Store, term: &[u8], k: usize) -> Result<Vec<RankedScore>, String> {
    let mut scores = store
        .search_lexical(
            &term_query(term),
            k,
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| format!("observe public lexical leg: {error}"))?
        .candidates
        .into_iter()
        .map(|candidate| {
            Ok(RankedScore::new(
                doc_id_u64(candidate.document.doc_id())?,
                candidate.score.to_bits(),
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    scores.sort_by(|left, right| {
        f64::from_bits(right.score_bits)
            .total_cmp(&f64::from_bits(left.score_bits))
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(scores)
}

fn report_fact(report: &zeppelin_embed::fusion::FusionReport, version: u16) -> FusionReportFact {
    FusionReportFact {
        normalization_policy_version: Some(version),
        method: match report.method {
            FusionMethod::ConvexCombination => FusionMethodFact::ConvexCombination,
            FusionMethod::ReciprocalRankFusion => FusionMethodFact::ReciprocalRankFusion,
        },
        rounds: report.rounds,
        budget_exhausted: report.budget_exhausted,
        termination: match report.termination {
            FusionTermination::StableBound => FusionTerminationFact::StableBound,
            FusionTermination::ListsExhausted => FusionTerminationFact::ListsExhausted,
            FusionTermination::BudgetFullMaterialization => {
                FusionTerminationFact::BudgetFullMaterialization
            }
            FusionTermination::WindowUnproven => FusionTerminationFact::WindowUnproven,
            FusionTermination::ApproximateCandidates => {
                FusionTerminationFact::ApproximateCandidates
            }
        },
    }
}

fn observe_case(
    store: &Store,
    vector_query: &[f32],
    lexical_term: &[u8],
    query: &HybridQuery,
) -> Result<FusionCaseObserved, String> {
    let outcome = store
        .search_hybrid(
            SearchRequest::new(vector_query),
            &term_query(lexical_term),
            query,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .map_err(|error| format!("observe public hybrid result: {error}"))?;
    let report = outcome
        .diagnostics
        .fusion
        .as_ref()
        .ok_or_else(|| "public hybrid diagnostics omitted fusion report".to_owned())?;
    let hits = outcome
        .hits
        .into_iter()
        .map(|hit| {
            Ok(FusedHitFact {
                id: doc_id_u64(hit.key)?,
                vector_squared_l2_bits: hit.vector_squared_l2.map(f64::to_bits),
                lexical_bm25_bits: hit.lexical_bm25.map(f64::to_bits),
                fused_score_bits: hit.fused_score.to_bits(),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(FusionCaseObserved {
        hits,
        report: report_fact(
            report,
            outcome
                .diagnostics
                .hybrid
                .as_ref()
                .ok_or_else(|| "missing Store policy report".to_owned())?
                .normalization_policy_version,
        ),
    })
}

fn case_input(
    query: &HybridQuery,
    vector: &[RankedScore],
    lexical: Vec<RankedScore>,
    policy: StorePolicyFact,
) -> FusionCaseInput {
    FusionCaseInput {
        store_policy: Some(policy),
        k: query.k,
        alpha_bits: query
            .alpha
            .unwrap_or(zeppelin_embed::fusion::DEFAULT_ALPHA)
            .to_bits(),
        max_rounds: query.max_rounds,
        vector: vector.to_vec(),
        lexical,
    }
}

pub fn build_hybrid_episode(seed: u64) -> Result<HybridEpisode, String> {
    let (documents, vector_query, dimensions, active_docs, deleted_docs) = derive_documents(seed);
    let generated_docs = documents.len();
    let sealed_count = generated_docs - active_docs;
    let directory = tempdir().map_err(|error| format!("hybrid Store tempdir: {error}"))?;
    let store = Store::open(directory.path(), OpenOptions::default())
        .map_err(|error| format!("open hybrid Store: {error}"))?;
    ingest_documents(&store, &documents[..sealed_count])?;
    store
        .seal()
        .map_err(|error| format!("seal hybrid fixture: {error}"))?;
    let deleted = documents[..sealed_count]
        .iter()
        .filter(|document| document.deleted)
        .map(|document| DocId::new(u128::from(document.id)))
        .collect::<Vec<_>>();
    store
        .delete(DeleteBatch::new(deleted))
        .map_err(|error| format!("delete hybrid fixture rows: {error}"))?;
    ingest_documents(&store, &documents[sealed_count..])?;

    let live_count = generated_docs - deleted_docs;
    let (vector, ceiling) = vector_scores(&store, &documents, &vector_query, live_count)?;
    let policy = StorePolicyFact {
        version: 1,
        vector_ceiling_bits: ceiling.to_bits(),
        corpus_rows: generated_docs,
    };
    let k = 5 + (seed as usize % 11);
    let alpha = [
        0.0,
        0.5,
        1.0,
        std::f64::consts::FRAC_1_SQRT_2,
        0.618_033_988_749_894_8,
    ][seed as usize % 5];
    let query = HybridQuery::new(k).with_alpha(alpha);
    let main = case_input(
        &query,
        &vector,
        lexical_scores(&store, MAIN_TERM, live_count)?,
        policy,
    );
    let rrf = case_input(
        &query,
        &vector,
        lexical_scores(&store, RRF_TERM, live_count)?,
        policy,
    );
    let single = case_input(
        &query,
        &vector,
        lexical_scores(&store, SINGLE_TERM, live_count)?,
        policy,
    );
    let empty = case_input(
        &query,
        &vector,
        lexical_scores(&store, EMPTY_TERM, live_count)?,
        policy,
    );
    let observed = HybridObserved {
        main: observe_case(&store, &vector_query, MAIN_TERM, &query)?,
        rrf: observe_case(&store, &vector_query, RRF_TERM, &query)?,
        single: observe_case(&store, &vector_query, SINGLE_TERM, &query)?,
        empty: observe_case(&store, &vector_query, EMPTY_TERM, &query)?,
        leg_faults: Vec::new(),
        same_seed_control_passed: false,
    };
    store
        .close()
        .map_err(|error| format!("close hybrid fixture Store: {error}"))?;
    let frozen = FrozenStoreFixture::capture(directory.path())?;
    let control = frozen.materialize()?;
    let same_seed_control_passed = control.path() != directory.path()
        && FrozenStoreFixture::capture(control.path())? == frozen;

    Ok(HybridEpisode {
        input: HybridInput {
            generated_docs,
            deleted_docs,
            active_docs,
            dimensions,
            fixture_ids: documents.iter().map(|document| document.id).collect(),
            main,
            rrf,
            single,
            empty,
        },
        observed: HybridObserved {
            same_seed_control_passed,
            ..observed
        },
        frozen,
        vector_query,
    })
}

fn dependencies(leg: FusionLeg) -> StoreTestDependencies {
    StoreTestDependencies::new(Arc::new(StdVfs), Arc::new(SystemMonotonicClock))
        .with_hybrid_leg_fault(HybridLegTestFault::Panic(leg))
}

fn observe_leg_fault(episode: &HybridEpisode, leg: FusionLeg) -> Result<LegFaultObserved, String> {
    let directory = episode.frozen.materialize()?;
    let store = Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::default(),
        dependencies(leg),
    )
    .map_err(|error| format!("open hybrid leg-fault Store: {error}"))?;
    let query = HybridQuery::new(episode.input.main.k)
        .with_alpha(f64::from_bits(episode.input.main.alpha_bits));
    let error = match store.search_hybrid(
        SearchRequest::new(&episode.vector_query),
        &term_query(MAIN_TERM),
        &query,
        SearchOptions::default(),
        QueryControl::Cancel(CancelToken::new()),
    ) {
        Err(error) => error,
        Ok(_) => return Err(format!("{leg:?} hybrid leg panic returned partial success")),
    };
    let (observed_leg, detail) = match error {
        FusionError::LegPanic { leg, detail } => (leg, detail),
        other => return Err(format!("{leg:?} hybrid leg panic returned {other:?}")),
    };
    if observed_leg != leg {
        return Err(format!(
            "armed {leg:?} hybrid leg panic reported {observed_leg:?}"
        ));
    }
    let receipt = store
        .take_hybrid_execution_receipt()
        .ok_or_else(|| format!("{leg:?} hybrid leg panic omitted execution receipt"))?;
    let retry = observe_case(&store, &episode.vector_query, MAIN_TERM, &query)?;
    store
        .close()
        .map_err(|error| format!("close hybrid leg-fault Store: {error}"))?;
    Ok(LegFaultObserved {
        leg: match leg {
            FusionLeg::Vector => FusionLegFact::Vector,
            FusionLeg::Lexical => FusionLegFact::Lexical,
        },
        kind: LegFailureFact::Panic,
        detail: detail.to_owned(),
        no_partial: true,
        both_legs_completed: receipt.vector_completed && receipt.lexical_completed,
        retry,
    })
}

fn observed_for_operation(
    episode: &HybridEpisode,
    operation: HybridOperationKind,
) -> Result<HybridObserved, String> {
    let mut observed = episode.observed.clone();
    if operation == HybridOperationKind::Legs {
        observed.leg_faults = vec![
            observe_leg_fault(episode, FusionLeg::Vector)?,
            observe_leg_fault(episode, FusionLeg::Lexical)?,
        ];
    }
    Ok(observed)
}

fn leg_error(leg: FusionLeg, detail: &str) -> FusionError {
    FusionError::Leg {
        leg,
        kind: LegFailureKind::Caller,
        detail: detail.to_owned(),
    }
}

fn exercise_fault(fault: HybridFaultKind, observed: &HybridObserved) -> Result<(), String> {
    if fault == HybridFaultKind::LegPanic {
        if observed.leg_faults.len() == 2
            && observed
                .leg_faults
                .iter()
                .all(|fact| fact.kind == LegFailureFact::Panic && fact.no_partial)
        {
            return Ok(());
        }
        return Err("production hybrid leg panic seam did not emit both typed failures".to_owned());
    }
    let query = HybridQuery::new(1);
    let (result, expected) = match fault {
        HybridFaultKind::VectorLegError => (
            execute_hybrid(
                &query,
                || Err(leg_error(FusionLeg::Vector, "vector fault")),
                || Ok(vec![LexicalCandidate::new(1_u32, 1.0)]),
                |id| Some(*id),
                |id| Some(*id),
            ),
            leg_error(FusionLeg::Vector, "vector fault"),
        ),
        HybridFaultKind::LexicalLegError => (
            execute_hybrid(
                &query,
                || Ok(vec![VectorCandidate::exact(1_u32, 0.0)]),
                || Err(leg_error(FusionLeg::Lexical, "lexical fault")),
                |id| Some(*id),
                |id| Some(*id),
            ),
            leg_error(FusionLeg::Lexical, "lexical fault"),
        ),
        HybridFaultKind::DualFailureOrder => (
            execute_hybrid(
                &query,
                || Err(leg_error(FusionLeg::Vector, "vector fault")),
                || Err(leg_error(FusionLeg::Lexical, "lexical fault")),
                |id: &u32| Some(*id),
                |id: &u32| Some(*id),
            ),
            leg_error(FusionLeg::Vector, "vector fault"),
        ),
        HybridFaultKind::EstimatedScore => (
            execute_hybrid(
                &query,
                || Ok(vec![VectorCandidate::estimated(1_u32, 0.5)]),
                || Ok(vec![LexicalCandidate::new(1_u32, 1.0)]),
                |id| Some(*id),
                |id| Some(*id),
            ),
            FusionError::EstimatedVectorScore { rank: 0 },
        ),
        HybridFaultKind::NonfiniteScore => (
            execute_hybrid(
                &query,
                || Ok(vec![VectorCandidate::exact(1_u32, f64::NAN)]),
                || Ok(vec![LexicalCandidate::new(1_u32, 1.0)]),
                |id| Some(*id),
                |id| Some(*id),
            ),
            FusionError::NonFiniteScore {
                leg: FusionLeg::Vector,
                rank: 0,
            },
        ),
        HybridFaultKind::CancelClose => (
            execute_hybrid(
                &query,
                || Err(FusionError::Cancelled { partial: false }),
                || Ok(vec![LexicalCandidate::new(1_u32, 1.0)]),
                |id| Some(*id),
                |id| Some(*id),
            ),
            FusionError::Cancelled { partial: false },
        ),
        HybridFaultKind::LegPanic => {
            return Err("leg panic escaped its production-seam branch".to_owned());
        }
    };
    match result {
        Err(actual) if actual == expected => Ok(()),
        Err(actual) => Err(format!(
            "hybrid fault {} returned {actual:?}, expected {expected:?}",
            fault.key()
        )),
        Ok(_) => Err(format!("hybrid fault {} was accepted", fault.key())),
    }
}

pub fn run_hybrid_operation(
    episode: &HybridEpisode,
    operation: HybridOperationKind,
    fault: Option<HybridFaultKind>,
) -> Result<HybridOperationEvidence, String> {
    if fault.is_some_and(|selected| selected.operation() != operation) {
        return Err(format!(
            "hybrid fault {fault:?} does not target {operation:?}"
        ));
    }
    let input = episode.input.clone();
    let observed = observed_for_operation(episode, operation)?;
    let invariants = match operation {
        HybridOperationKind::Provenance => vec![HybridInvariantEvidence::I45 { input, observed }],
        HybridOperationKind::Normalization => {
            vec![HybridInvariantEvidence::I46 { input, observed }]
        }
        HybridOperationKind::BoundedFusion => {
            vec![HybridInvariantEvidence::I47 { input, observed }]
        }
        HybridOperationKind::Rrf => vec![HybridInvariantEvidence::I48 { input, observed }],
        HybridOperationKind::Legs => vec![HybridInvariantEvidence::I49 { input, observed }],
    };
    let mut receipts = Vec::new();
    if let Some(fault) = fault {
        let observed = match invariants.first() {
            Some(HybridInvariantEvidence::I45 { observed, .. })
            | Some(HybridInvariantEvidence::I46 { observed, .. })
            | Some(HybridInvariantEvidence::I47 { observed, .. })
            | Some(HybridInvariantEvidence::I48 { observed, .. })
            | Some(HybridInvariantEvidence::I49 { observed, .. }) => observed,
            None => return Err("hybrid operation omitted invariant evidence".to_owned()),
        };
        exercise_fault(fault, observed)?;
        receipts.push(HybridFaultReceipt {
            fault,
            operation,
            site: fault.site(),
            cardinality: 1,
        });
    }
    Ok(HybridOperationEvidence {
        invariants,
        receipts,
        clean_control_passed: episode.observed.same_seed_control_passed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeppelin_embed_adversarial_oracle::hybrid_fusion as oracle;

    #[test]
    fn astra_03_fixed_policy_cross_fill_and_certificate_plants() {
        let episode = build_hybrid_episode(11).expect("seed-11 fixed-policy episode");
        for operation in [
            HybridOperationKind::Provenance,
            HybridOperationKind::Normalization,
            HybridOperationKind::BoundedFusion,
            HybridOperationKind::Rrf,
            HybridOperationKind::Legs,
        ] {
            let fault = match operation {
                HybridOperationKind::Provenance => Some(HybridFaultKind::EstimatedScore),
                HybridOperationKind::Normalization => Some(HybridFaultKind::NonfiniteScore),
                HybridOperationKind::Legs => Some(HybridFaultKind::LegPanic),
                _ => None,
            };
            let evidence =
                run_hybrid_operation(&episode, operation, fault).expect("directed policy boundary");
            assert!(evidence.clean_control_passed);
            assert_eq!(evidence.receipts.len(), usize::from(fault.is_some()));
            for invariant in evidence.invariants {
                match invariant {
                    HybridInvariantEvidence::I45 {
                        input,
                        mut observed,
                    } => {
                        oracle::compare_i45(&input, &observed).expect("complete raw-score control");
                        observed.main.hits[0].lexical_bm25_bits = None;
                        assert!(
                            oracle::compare_i45(&input, &observed).is_err(),
                            "omitted cross-score plant must fire"
                        );
                    }
                    HybridInvariantEvidence::I46 {
                        input,
                        mut observed,
                    } => {
                        oracle::compare_i46(&input, &observed).expect("fixed-anchor control");
                        observed.main.report.normalization_policy_version = Some(0);
                        assert!(
                            oracle::compare_i46(&input, &observed).is_err(),
                            "unsupported normalization policy must fire"
                        );
                    }
                    HybridInvariantEvidence::I47 {
                        input,
                        mut observed,
                    } => {
                        oracle::compare_i47(&input, &observed).expect("independent bounded replay");
                        observed.main.report.rounds += 1;
                        assert!(
                            oracle::compare_i47(&input, &observed).is_err(),
                            "false round receipt must fire"
                        );
                    }
                    HybridInvariantEvidence::I48 { input, observed } => {
                        oracle::compare_i48(&input, &observed)
                            .expect("Store degenerate policy control")
                    }
                    HybridInvariantEvidence::I49 { input, observed } => {
                        oracle::compare_i49(&input, &observed)
                            .expect("panic join and exact same-seed retry")
                    }
                }
            }
        }
    }

    #[test]
    fn every_hybrid_operation_runs_its_checker_on_one_shared_episode() {
        let episode = build_hybrid_episode(7).expect("hybrid episode");
        for operation in [
            HybridOperationKind::Provenance,
            HybridOperationKind::Normalization,
            HybridOperationKind::BoundedFusion,
            HybridOperationKind::Rrf,
            HybridOperationKind::Legs,
        ] {
            let evidence =
                run_hybrid_operation(&episode, operation, None).expect("hybrid operation");
            for invariant in evidence.invariants {
                match invariant {
                    HybridInvariantEvidence::I45 { input, observed } => {
                        oracle::compare_i45(&input, &observed)
                    }
                    HybridInvariantEvidence::I46 { input, observed } => {
                        oracle::compare_i46(&input, &observed)
                    }
                    HybridInvariantEvidence::I47 { input, observed } => {
                        oracle::compare_i47(&input, &observed)
                    }
                    HybridInvariantEvidence::I48 { input, observed } => {
                        oracle::compare_i48(&input, &observed)
                    }
                    HybridInvariantEvidence::I49 { input, observed } => {
                        oracle::compare_i49(&input, &observed)
                    }
                }
                .expect("hybrid checker");
            }
        }
    }

    #[test]
    fn astra_00_scoped_lexical_worker_panic_and_clean_control() {
        let episode = build_hybrid_episode(11).expect("hybrid episode");
        // The existing I49 full-score checker models the old k-wide producer
        // and fails before this change (ASTRA-ISSUE-002). This directed case
        // owns atomic leg failure and same-seed recovery, not fusion policy.
        let atomic = |facts: &[LegFaultObserved]| {
            facts.len() == 2
                && facts.iter().all(|fact| {
                    fact.kind == LegFailureFact::Panic
                        && fact.no_partial
                        && fact.both_legs_completed
                        && fact.detail
                            == match fact.leg {
                                FusionLegFact::Vector => "vector hybrid leg panicked",
                                FusionLegFact::Lexical => "lexical hybrid leg panicked",
                            }
                })
        };
        let evidence = run_hybrid_operation(
            &episode,
            HybridOperationKind::Legs,
            Some(HybridFaultKind::LegPanic),
        )
        .expect("directed leg panic");
        assert!(evidence.clean_control_passed);
        assert_eq!(evidence.receipts.len(), 1);
        assert_eq!(evidence.receipts[0].cardinality, 1);
        for invariant in evidence.invariants {
            let HybridInvariantEvidence::I49 { mut observed, .. } = invariant else {
                panic!("unexpected invariant at leg boundary");
            };
            assert!(atomic(&observed.leg_faults));
            for fault in &observed.leg_faults {
                assert_eq!(
                    fault.retry, episode.observed.main,
                    "post-panic retry must equal the same-seed clean query"
                );
            }
            observed.leg_faults[0].both_legs_completed = false;
            assert!(
                !atomic(&observed.leg_faults),
                "a missing join must trip the atomic-failure oracle"
            );
        }
    }

    #[test]
    fn every_declared_hybrid_fault_fires_once() {
        let episode = build_hybrid_episode(11).expect("hybrid episode");
        for fault in [
            HybridFaultKind::VectorLegError,
            HybridFaultKind::LexicalLegError,
            HybridFaultKind::DualFailureOrder,
            HybridFaultKind::LegPanic,
            HybridFaultKind::EstimatedScore,
            HybridFaultKind::NonfiniteScore,
            HybridFaultKind::CancelClose,
        ] {
            let evidence = run_hybrid_operation(&episode, fault.operation(), Some(fault))
                .unwrap_or_else(|error| panic!("{fault:?}: {error}"));
            assert_eq!(evidence.receipts.len(), 1);
            assert_eq!(evidence.receipts[0].fault, fault);
            assert_eq!(evidence.receipts[0].cardinality, 1);
        }
    }
}
