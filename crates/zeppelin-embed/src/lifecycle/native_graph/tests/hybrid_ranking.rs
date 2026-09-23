#![cfg(test)]
//! ZE-63: lexical and hybrid ranking over sparse populations against an
//! independent full-eligible-union oracle.
//!
//! The oracle never reads the store. It keeps its own record of every live
//! node's text and vector, derives full-live BM25 statistics (`N`, lengths,
//! `df` over every live indexed text), the full-live anchors, and scores the
//! entire eligible union with its own BM25 and Store policy v1 arithmetic.
//! Only query/text analysis reuses the store's analyzer, because analysis is
//! not the contract under test.

use crate::epoch::{ComputeUnits, EmbeddingRuntime, EmbeddingTower, Normalization};
use crate::fts::tokenizer::Analyzer;
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::native_graph::NativeReadConsumer;
use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, SearchTier, Store};
use crate::property_graph::query::completed::{
    ActualTier, CandidateCoverage, LegState, ScorePrecision,
};
use crate::property_graph::query::eligibility::{Eligibility, EligibleNodeSet};
use crate::property_graph::query::plan::{SearchBounds, SearchMode};
use crate::property_graph::query::runtime::{RuntimeContext, RuntimeLimits};
use crate::property_graph::retrieval::hybrid::{HybridRankReport, TextRankReport};
use crate::property_graph::retrieval::{NativeRetrievalContext, RetrievalError};
use crate::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use crate::property_graph::storage::GraphReadView;
use crate::property_graph::storage::tree::directory::TreeError;
use crate::property_graph::{
    ApplicationKey, CanonicalContents, CanonicalEmbedding, EntityId, EntityKind, GraphDeleteMode,
    GraphRevision, NodeId,
};
use rand::Rng;
use std::collections::{BTreeMap, BTreeSet};

const ALPHA: f64 = crate::fusion::DEFAULT_ALPHA;
const K1: f64 = 1.2;
const B: f64 = 0.75;
const WORDS: [&str; 6] = ["amber", "birch", "cedar", "dune", "ember", "fjord"];

fn tower() -> EmbeddingTower {
    EmbeddingTower {
        model_id: "ze63-document".into(),
        model_version: "1".into(),
        weights_digest: vec![0x06, 0x3a],
        dims: 2,
        normalization: Normalization::None,
        prompt_prefix: "doc: ".into(),
        max_tokens: 32,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    }
}

#[derive(Clone, Debug)]
struct Entry {
    revision: u64,
    point: Option<[f32; 2]>,
    text: Option<String>,
}

/// A real native store plus the test's own record of what it wrote.
struct Corpus {
    store: Store,
    _directory: tempfile::TempDir,
    document: EmbeddingTower,
    live: BTreeMap<NodeId, Entry>,
    dead: Vec<NodeId>,
    next_key: usize,
    keys: BTreeMap<NodeId, String>,
}

type Item<'a> = (Option<[f32; 2]>, Option<&'a str>);

impl Corpus {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let document = tower();
        let store = Store::create_native_graph(
            directory.path().join("native"),
            OpenOptions::new()
                .with_durability(DurabilityMode::Durable, CommitTier::Durable)
                .with_max_resident_bytes(256 * 1024 * 1024),
            Some(document.clone()),
        )
        .unwrap();
        Self {
            store,
            _directory: directory,
            document,
            live: BTreeMap::new(),
            dead: Vec::new(),
            next_key: 0,
            keys: BTreeMap::new(),
        }
    }

    fn analyzer(&self) -> Analyzer {
        self.store.tokenizer.clone()
    }

    fn write(&mut self, items: &[Item<'_>], targets: &[Option<NodeId>]) -> Vec<NodeId> {
        let contents = items
            .iter()
            .map(|(point, text)| {
                let embedding = point
                    .as_ref()
                    .map(|point| CanonicalEmbedding::new(&self.document, point).unwrap());
                CanonicalContents::node(&mut [], &mut [], *text, embedding).unwrap()
            })
            .collect::<Vec<_>>();
        let keys = targets
            .iter()
            .map(|target| match target {
                Some(node) => self.keys[node].clone(),
                None => {
                    self.next_key += 1;
                    format!("n{:05}", self.next_key)
                }
            })
            .collect::<Vec<_>>();
        let requests = keys
            .iter()
            .zip(&contents)
            .zip(targets)
            .map(|((key, contents), target)| StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", key).unwrap(),
                revision: GraphRevision::new(
                    target.map_or(1, |node| self.live[&node].revision + 1),
                )
                .unwrap(),
                operation: match target {
                    Some(node) => StructuredOperation::Put(EntityId::Node(*node)),
                    None => StructuredOperation::Create,
                },
                image: Some(WriteImage::Node(contents)),
            })
            .collect::<Vec<_>>();
        let receipts = self
            .store
            .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
            .unwrap();
        let mut nodes = Vec::new();
        for ((receipt, (point, text)), key) in receipts.iter().zip(items).zip(keys) {
            let EntityId::Node(node) = receipt.entity else {
                panic!("node write changed identity domain");
            };
            self.keys.insert(node, key);
            let revision = self.live.get(&node).map_or(1, |entry| entry.revision + 1);
            self.live.insert(
                node,
                Entry {
                    revision,
                    point: *point,
                    text: text.map(str::to_owned),
                },
            );
            nodes.push(node);
        }
        nodes
    }

    /// One apply call, so one new sparse source per present modality.
    fn create(&mut self, items: &[Item<'_>]) -> Vec<NodeId> {
        self.write(items, &vec![None; items.len()])
    }

    fn replace(&mut self, changes: &[(NodeId, Item<'_>)]) {
        let items = changes.iter().map(|(_, item)| *item).collect::<Vec<_>>();
        let targets = changes
            .iter()
            .map(|(node, _)| Some(*node))
            .collect::<Vec<_>>();
        self.write(&items, &targets);
    }

    fn delete(&mut self, nodes: &[NodeId]) {
        let keys = nodes
            .iter()
            .map(|node| self.keys[node].clone())
            .collect::<Vec<_>>();
        let requests = nodes
            .iter()
            .zip(&keys)
            .map(|(node, key)| StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", key).unwrap(),
                revision: GraphRevision::new(self.live[node].revision + 1).unwrap(),
                operation: StructuredOperation::Delete(
                    EntityId::Node(*node),
                    GraphDeleteMode::Detach,
                ),
                image: None,
            })
            .collect::<Vec<_>>();
        self.store
            .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
            .unwrap();
        for node in nodes {
            self.live.remove(node);
            self.dead.push(*node);
        }
    }

    fn all_nodes(&self) -> Vec<NodeId> {
        self.live.keys().chain(&self.dead).copied().collect()
    }
}

/// Independent full-live oracle over the test's own records.
struct Oracle<'c> {
    corpus: &'c Corpus,
    /// Live indexed text members `T`: (length, term frequencies).
    text: BTreeMap<NodeId, (u32, BTreeMap<String, u32>)>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct OracleHit {
    node: NodeId,
    revision: u64,
    fused: f64,
    vector: Option<f64>,
    lexical: Option<f64>,
}

impl<'c> Oracle<'c> {
    fn new(corpus: &'c Corpus) -> Self {
        let analyzer = corpus.analyzer();
        let mut text = BTreeMap::new();
        for (node, entry) in &corpus.live {
            let Some(value) = &entry.text else {
                continue;
            };
            let tokens = analyzer.analyze(value);
            if tokens.is_empty() {
                continue;
            }
            let length = tokens.iter().map(|token| token.position + 1).max().unwrap();
            let mut frequencies = BTreeMap::new();
            for token in tokens {
                *frequencies.entry(token.term).or_insert(0) += 1;
            }
            text.insert(*node, (length, frequencies));
        }
        Self { corpus, text }
    }

    fn terms(&self, query: &str) -> Vec<String> {
        self.corpus
            .analyzer()
            .analyze(query)
            .into_iter()
            .map(|token| token.term)
            .collect()
    }

    /// BM25 of every member of `domain` against statistics of `domain`.
    fn bm25_over(
        domain: &BTreeMap<NodeId, (u32, BTreeMap<String, u32>)>,
        terms: &[String],
    ) -> BTreeMap<NodeId, f64> {
        let n = domain.len() as u64;
        let total = domain
            .values()
            .map(|(length, _)| u64::from(*length))
            .sum::<u64>();
        let average = total as f64 / n as f64;
        let mut scores = domain
            .keys()
            .map(|node| (*node, 0.0_f64))
            .collect::<BTreeMap<_, _>>();
        for term in terms {
            let df = domain
                .values()
                .filter(|(_, frequencies)| frequencies.contains_key(term))
                .count() as u32;
            if df == 0 {
                continue;
            }
            let documents = n as f64;
            let df = f64::from(df).min(documents);
            let idf = (1.0 + (documents - df + 0.5) / (df + 0.5)).ln();
            for (node, (length, frequencies)) in domain {
                let Some(tf) = frequencies.get(term) else {
                    continue;
                };
                let frequency = f64::from(*tf);
                let normalization = 1.0 - B + B * f64::from(*length) / average;
                let score = idf * (frequency * (K1 + 1.0)) / (frequency + K1 * normalization);
                *scores.get_mut(node).unwrap() += score;
            }
        }
        scores
    }

    fn bm25(&self, terms: &[String]) -> BTreeMap<NodeId, f64> {
        if self.text.is_empty() {
            return BTreeMap::new();
        }
        Self::bm25_over(&self.text, terms)
    }

    fn distance(query: [f32; 2], point: [f32; 2]) -> f64 {
        crate::quant::squared_l2_f64(&query, &point)
    }

    fn ceiling(&self, query: [f32; 2]) -> Option<f64> {
        self.corpus
            .live
            .values()
            .filter_map(|entry| entry.point)
            .map(|point| {
                crate::graph::search::GraphSegmentNormRange::from_exact_rows(&point, 2)
                    .squared_l2_upper_bound(&query)
            })
            .reduce(f64::max)
    }

    fn text_hits(
        &self,
        text: &str,
        eligible: Option<&BTreeSet<NodeId>>,
        k: usize,
    ) -> Vec<(NodeId, u64, f64)> {
        let mut hits = self
            .bm25(&self.terms(text))
            .into_iter()
            .filter(|(node, score)| *score > 0.0 && eligible.is_none_or(|set| set.contains(node)))
            .map(|(node, score)| (node, self.corpus.live[&node].revision, score))
            .collect::<Vec<_>>();
        hits.sort_by(|left, right| right.2.total_cmp(&left.2).then(left.0.cmp(&right.0)));
        hits.truncate(k);
        hits
    }

    /// Scores the entire eligible union against full-live anchors.
    fn hybrid(
        &self,
        query: [f32; 2],
        text: &str,
        eligible: Option<&BTreeSet<NodeId>>,
        k: usize,
    ) -> (Vec<OracleHit>, f64) {
        let bm25 = self.bm25(&self.terms(text));
        let maximum = bm25.values().copied().fold(0.0, f64::max);
        let ceiling = self.ceiling(query);
        let allowed = |node: &NodeId| eligible.is_none_or(|set| set.contains(node));
        let vector_leg = self
            .corpus
            .live
            .iter()
            .any(|(node, entry)| entry.point.is_some() && allowed(node));
        let lexical_leg = bm25
            .iter()
            .any(|(node, score)| *score > 0.0 && allowed(node));
        let alpha = if !vector_leg {
            0.0
        } else if !lexical_leg {
            1.0
        } else {
            ALPHA
        };
        let mut hits = Vec::new();
        for (node, entry) in &self.corpus.live {
            if !allowed(node) {
                continue;
            }
            let vector = entry.point.map(|point| Self::distance(query, point));
            let lexical = bm25.get(node).copied();
            if vector.is_none() && lexical.is_none_or(|score| score == 0.0) {
                continue;
            }
            let mut fused = 0.0;
            if let Some(distance) = vector {
                let ceiling = ceiling.unwrap();
                fused += alpha * ((ceiling - distance) / ceiling);
            }
            if lexical_leg && let Some(score) = lexical {
                fused += (1.0 - alpha) * (score / maximum);
            }
            hits.push(OracleHit {
                node: *node,
                revision: entry.revision,
                fused,
                vector,
                lexical,
            });
        }
        hits.sort_by(|left, right| {
            right
                .fused
                .total_cmp(&left.fused)
                .then(left.node.cmp(&right.node))
        });
        hits.truncate(k);
        (hits, alpha)
    }
}

fn hit_bits(hits: &[OracleHit]) -> Vec<(u128, u64, u64, Option<u64>, Option<u64>)> {
    hits.iter()
        .map(|hit| {
            (
                hit.node.get(),
                hit.revision,
                hit.fused.to_bits(),
                hit.vector.map(f64::to_bits),
                hit.lexical.map(f64::to_bits),
            )
        })
        .collect()
}

#[derive(Clone, Copy)]
enum Operation {
    Text,
    Hybrid,
}

enum Outcome {
    Text(Vec<(NodeId, u64, f64)>, TextRankReport),
    Hybrid(Vec<OracleHit>, HybridRankReport),
}

struct Request<'a> {
    operation: Operation,
    query: [f32; 2],
    text: &'a str,
    mode: SearchMode,
    eligible: Option<&'a [NodeId]>,
    k: i64,
    window: u64,
    analyzer: Analyzer,
}

impl NativeReadConsumer<Result<Outcome, RetrievalError>> for Request<'_> {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<Result<Outcome, RetrievalError>, TreeError> {
        let context = NativeRetrievalContext::new(view, runtime)
            .map_err(|_| TreeError::Invalid("ranking context"))?;
        let token = runtime.view();
        let set = match self.eligible {
            Some(ids) => Some(
                EligibleNodeSet::build(runtime, ids.len(), ids.iter().map(|id| token.node(*id)))
                    .map_err(|_| TreeError::Invalid("eligible set"))?,
            ),
            None => None,
        };
        let eligibility = || {
            set.as_ref()
                .map_or(Eligibility::AllIndexed, Eligibility::Set)
        };
        let bounds = SearchBounds::new(self.k, self.window).unwrap();
        let text = match context.prepare_text(&self.analyzer, self.text, eligibility(), runtime) {
            Ok(text) => text,
            Err(error) => return Ok(Err(error)),
        };
        Ok(match self.operation {
            Operation::Text => context.rank_text(&text, bounds, runtime).map(|ranked| {
                Outcome::Text(
                    ranked
                        .hits()
                        .iter()
                        .map(|hit| (hit.node, hit.revision.get(), hit.bm25))
                        .collect(),
                    ranked.report(),
                )
            }),
            Operation::Hybrid => {
                let vector = context
                    .prepare_vector(&self.query, self.mode, eligibility(), runtime)
                    .map_err(|_| TreeError::Invalid("prepared vector"))?;
                context
                    .rank_hybrid(&vector, &text, bounds, runtime)
                    .map(|ranked| {
                        Outcome::Hybrid(
                            ranked
                                .hits()
                                .iter()
                                .map(|hit| OracleHit {
                                    node: hit.node,
                                    revision: hit.revision.get(),
                                    fused: hit.fused,
                                    vector: hit.vector,
                                    lexical: hit.lexical,
                                })
                                .collect(),
                            ranked.report(),
                        )
                    })
            }
        })
    }
}

fn run(corpus: &Corpus, request: Request<'_>) -> Result<Outcome, RetrievalError> {
    corpus
        .store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            RuntimeLimits::default(),
            16 * 1024 * 1024,
            32,
            request,
        )
        .unwrap()
}

fn hybrid(
    corpus: &Corpus,
    query: [f32; 2],
    text: &str,
    mode: SearchMode,
    eligible: Option<&[NodeId]>,
    k: i64,
) -> (Vec<OracleHit>, HybridRankReport) {
    match run(
        corpus,
        Request {
            operation: Operation::Hybrid,
            query,
            text,
            mode,
            eligible,
            k,
            window: 65_536,
            analyzer: corpus.analyzer(),
        },
    )
    .unwrap()
    {
        Outcome::Hybrid(hits, report) => (hits, report),
        Outcome::Text(..) => unreachable!(),
    }
}

fn text(
    corpus: &Corpus,
    query: &str,
    eligible: Option<&[NodeId]>,
    k: i64,
) -> (Vec<(NodeId, u64, f64)>, TextRankReport) {
    match run(
        corpus,
        Request {
            operation: Operation::Text,
            query: [0.0, 0.0],
            text: query,
            mode: SearchMode::Exact,
            eligible,
            k,
            window: 65_536,
            analyzer: corpus.analyzer(),
        },
    )
    .unwrap()
    {
        Outcome::Text(hits, report) => (hits, report),
        Outcome::Hybrid(..) => unreachable!(),
    }
}

fn phrase(random: &mut rand_chacha::ChaCha8Rng) -> String {
    let words = random.random_range(1..=6);
    (0..words)
        .map(|_| WORDS[random.random_range(0..WORDS.len())])
        .collect::<Vec<_>>()
        .join(" ")
}

/// Sparse populations across several sources: text-only, vector-only, both,
/// neither, text that analyzes to nothing, deletions and replacements.
fn corpus(name: &str) -> Corpus {
    let mut random = crate::test_support::seeded_rng(name);
    let mut corpus = Corpus::new();
    let mut batches = Vec::new();
    for _ in 0..4 {
        let owned = (0..24)
            .map(|_| {
                let point = random.random_bool(0.7).then(|| {
                    [
                        random.random_range(-6_i32..=6) as f32,
                        random.random_range(-6_i32..=6) as f32,
                    ]
                });
                let text = match random.random_range(0..10) {
                    0 => Some("!!! ...".to_owned()),
                    1..=6 => Some(phrase(&mut random)),
                    _ => None,
                };
                (point, text)
            })
            .collect::<Vec<_>>();
        let items = owned
            .iter()
            .map(|(point, text)| (*point, text.as_deref()))
            .collect::<Vec<_>>();
        batches.push(corpus.create(&items));
    }
    corpus.create(&[(None, None), (None, None)]);
    corpus.delete(&[
        batches[0][1],
        batches[0][5],
        batches[1][2],
        batches[2][9],
        batches[3][0],
    ]);
    corpus.replace(&[
        (batches[0][3], (Some([0.0, 0.0]), Some("amber amber fjord"))),
        (batches[1][4], (None, Some("cedar"))),
        (batches[2][6], (Some([6.0, -6.0]), None)),
        (batches[3][7], (Some([1.0, 1.0]), Some("..."))),
    ]);
    corpus
}

fn query_text(random: &mut rand_chacha::ChaCha8Rng) -> String {
    let words = random.random_range(1..=3);
    (0..words)
        .map(|_| {
            if random.random_bool(0.1) {
                "zeppelin"
            } else {
                WORDS[random.random_range(0..WORDS.len())]
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn ze63_exact_hybrid_matches_full_eligible_union_oracle() {
    let name = "lifecycle::native_graph::tests::hybrid_ranking::ze63_exact_hybrid_matches_full_eligible_union_oracle";
    let corpus = corpus(name);
    let oracle = Oracle::new(&corpus);
    let mut random = crate::test_support::seeded_rng(&format!("{name}::queries"));
    let everyone = corpus.all_nodes();
    let mut text_only_hits = 0;
    let mut vector_only_hits = 0;
    let mut nonmatching_text_hits = 0;
    for case in 0..30 {
        let query = [
            random.random_range(-7_i32..=7) as f32,
            random.random_range(-7_i32..=7) as f32,
        ];
        let words = query_text(&mut random);
        let k = [1_i64, 3, 7, 20, 200][case % 5];
        let fraction = [0.1, 0.3, 0.6, 1.0][case % 4];
        let chosen = everyone
            .iter()
            .copied()
            .filter(|_| random.random_bool(fraction))
            .collect::<Vec<_>>();
        let set = chosen.iter().copied().collect::<BTreeSet<_>>();
        let (expected, alpha) = oracle.hybrid(query, &words, Some(&set), k as usize);
        let (hits, report) = hybrid(&corpus, query, &words, SearchMode::Exact, Some(&chosen), k);
        assert_eq!(
            hit_bits(&hits),
            hit_bits(&expected),
            "case {case} query {query:?} text {words:?} k {k}"
        );
        assert_eq!(
            report.effective_alpha.to_bits(),
            alpha.to_bits(),
            "case {case}"
        );
        assert_eq!(report.coverage, CandidateCoverage::Exact);
        assert!(report.cross_score_complete);
        assert_eq!(
            report.vector_ceiling.map(f64::to_bits),
            oracle.ceiling(query).map(f64::to_bits)
        );
        assert_eq!(report.text.live_members, oracle.text.len() as u64);
        for hit in &hits {
            match (hit.vector, hit.lexical) {
                (None, Some(_)) => text_only_hits += 1,
                (Some(_), None) => vector_only_hits += 1,
                (Some(_), Some(score)) if score == 0.0 => nonmatching_text_hits += 1,
                _ => {}
            }
        }
    }
    assert!(text_only_hits > 0 && vector_only_hits > 0 && nonmatching_text_hits > 0);
    // The omitted restriction equals the full-live union.
    for (query, words) in [
        ([0.0, 0.0], "amber"),
        ([5.0, -3.0], "cedar dune"),
        ([-7.0, 7.0], "fjord ember"),
    ] {
        let (expected, _) = oracle.hybrid(query, words, None, 4096);
        let (hits, report) = hybrid(&corpus, query, words, SearchMode::Exact, None, 4096);
        assert_eq!(hit_bits(&hits), hit_bits(&expected));
        assert_eq!(report.vector_leg, LegState::Nonempty);
        assert_eq!(report.lexical_leg, LegState::Nonempty);
        assert_eq!(report.actual_tier, Some(ActualTier::Exact));
        assert_eq!(report.precision, ScorePrecision::Original);
        assert_eq!(report.candidate_count, expected.len() as u64);
    }
}

#[test]
fn ze63_text_ranking_uses_full_live_statistics() {
    let name = "lifecycle::native_graph::tests::hybrid_ranking::ze63_text_ranking_uses_full_live_statistics";
    let corpus = corpus(name);
    let oracle = Oracle::new(&corpus);
    let mut random = crate::test_support::seeded_rng(&format!("{name}::queries"));
    let everyone = corpus.all_nodes();
    let mut subset_statistics_differ = 0;
    for case in 0..24 {
        let words = query_text(&mut random);
        let k = [1_i64, 4, 50][case % 3];
        let fraction = [0.2, 0.5, 1.0][case % 3];
        let chosen = everyone
            .iter()
            .copied()
            .filter(|_| random.random_bool(fraction))
            .collect::<Vec<_>>();
        let set = chosen.iter().copied().collect::<BTreeSet<_>>();
        let expected = oracle.text_hits(&words, Some(&set), k as usize);
        let (hits, report) = text(&corpus, &words, Some(&chosen), k);
        let bits = |hits: &[(NodeId, u64, f64)]| {
            hits.iter()
                .map(|(node, revision, score)| (node.get(), *revision, score.to_bits()))
                .collect::<Vec<_>>()
        };
        assert_eq!(bits(&hits), bits(&expected), "case {case} text {words:?}");
        assert_eq!(report.coverage, CandidateCoverage::Exact);
        assert_eq!(report.domain.live_members, oracle.text.len() as u64);
        // Deliberately wrong: statistics recomputed on the eligible subset.
        let subset = oracle
            .text
            .iter()
            .filter(|(node, _)| set.contains(node))
            .map(|(node, value)| (*node, value.clone()))
            .collect::<BTreeMap<_, _>>();
        if !subset.is_empty() {
            let local = Oracle::bm25_over(&subset, &oracle.terms(&words));
            if expected
                .iter()
                .any(|(node, _, score)| local[node].to_bits() != score.to_bits())
            {
                subset_statistics_differ += 1;
            }
        }
    }
    assert!(
        subset_statistics_differ >= 3,
        "saw {subset_statistics_differ}"
    );
}

#[test]
fn ze63_components_distinguish_absence_from_zero() {
    let mut corpus = Corpus::new();
    let nodes = corpus.create(&[
        (Some([2.0, 2.0]), None),               // vector only, distance zero
        (None, Some("amber birch")),            // text only, matching
        (Some([0.0, 0.0]), Some("cedar dune")), // present nonmatching text
        (Some([1.0, 2.0]), Some("amber")),      // both, matching
        (Some([2.0, 3.0]), Some("!!!")),        // text analyzes to nothing
        (None, Some("ember")),                  // text only, nonmatching
        (None, None),                           // graph only
    ]);
    let oracle = Oracle::new(&corpus);
    let (hits, report) = hybrid(&corpus, [2.0, 2.0], "amber", SearchMode::Exact, None, 10);
    let (expected, _) = oracle.hybrid([2.0, 2.0], "amber", None, 10);
    assert_eq!(hit_bits(&hits), hit_bits(&expected));
    let by_node = hits
        .iter()
        .map(|hit| (hit.node, *hit))
        .collect::<BTreeMap<_, _>>();
    let zero = by_node[&nodes[0]];
    assert_eq!(zero.vector, Some(0.0));
    assert_eq!(zero.lexical, None);
    let text_only = by_node[&nodes[1]];
    assert_eq!(text_only.vector, None, "absent vector is not distance zero");
    assert!(text_only.lexical.unwrap() > 0.0);
    assert_eq!(
        by_node[&nodes[2]].lexical,
        Some(0.0),
        "present nonmatching text"
    );
    assert!(by_node[&nodes[3]].vector.is_some() && by_node[&nodes[3]].lexical.unwrap() > 0.0);
    assert_eq!(
        by_node[&nodes[4]].lexical, None,
        "analyzed-to-empty text is outside T"
    );
    assert!(
        !by_node.contains_key(&nodes[5]),
        "nonmatching text-only node is no candidate"
    );
    assert!(!by_node.contains_key(&nodes[6]));
    assert_eq!(report.text.live_members, 4);
    assert_eq!(report.candidate_count, 5);
    assert_eq!(report.effective_alpha.to_bits(), ALPHA.to_bits());
}

#[test]
fn ze63_empty_and_absent_legs_report_why() {
    let mut vectors_only = Corpus::new();
    let v = vectors_only.create(&[(Some([1.0, 0.0]), None), (Some([3.0, 0.0]), None)]);
    let (hits, report) = hybrid(
        &vectors_only,
        [0.0, 0.0],
        "amber",
        SearchMode::Exact,
        None,
        5,
    );
    assert_eq!(report.lexical_leg, LegState::NoIndexedPopulation);
    assert_eq!(report.text.maximum, None);
    assert_eq!(report.effective_alpha, 1.0);
    assert_eq!(hits.iter().map(|hit| hit.node).collect::<Vec<_>>(), v);
    let (text_hits, text_report) = text(&vectors_only, "amber", None, 5);
    assert!(text_hits.is_empty());
    assert_eq!(text_report.domain.leg, LegState::NoIndexedPopulation);

    let mut corpus = Corpus::new();
    let nodes = corpus.create(&[
        (Some([0.0, 0.0]), Some("amber birch")),
        (None, Some("cedar")),
        (None, Some("amber")),
        (Some([4.0, 4.0]), None),
    ]);
    // Global vector population, eligible vector leg empty: text alone.
    let only_text = [nodes[1], nodes[2]];
    let (hits, report) = hybrid(
        &corpus,
        [0.0, 0.0],
        "amber",
        SearchMode::Exact,
        Some(&only_text),
        5,
    );
    assert_eq!(report.vector_leg, LegState::NoEligibleMembers);
    assert_eq!(report.live_vector_members, 2);
    assert_eq!(report.lexical_leg, LegState::Nonempty);
    assert_eq!(report.effective_alpha, 0.0);
    assert_eq!(report.actual_tier, None);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node, nodes[2]);
    assert_eq!(hits[0].vector, None);
    // A positive global lexical maximum does not prove eligible matches.
    let no_match = [nodes[1], nodes[3]];
    let (hits, report) = hybrid(
        &corpus,
        [0.0, 0.0],
        "amber",
        SearchMode::Exact,
        Some(&no_match),
        5,
    );
    assert!(report.text.maximum.is_some_and(|maximum| maximum > 0.0));
    assert_eq!(report.lexical_leg, LegState::NoQueryMatches);
    assert_eq!(report.effective_alpha, 1.0);
    assert_eq!(
        hits.iter().map(|hit| hit.node).collect::<Vec<_>>(),
        vec![nodes[3]]
    );
    // Eligible members exist but have no indexed text at all.
    let (_, report) = hybrid(
        &corpus,
        [0.0, 0.0],
        "amber",
        SearchMode::Exact,
        Some(&[nodes[3]]),
        5,
    );
    assert_eq!(report.lexical_leg, LegState::NoEligibleMembers);
    // A query that analyzes to no terms matches nothing but builds no stats.
    let (hits, report) = hybrid(&corpus, [0.0, 0.0], "!!! ...", SearchMode::Exact, None, 5);
    assert_eq!(report.text.terms, 0);
    assert_eq!(report.lexical_leg, LegState::NoQueryMatches);
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].lexical, Some(0.0));
    // Neither eligible leg: zero rows with valid metadata, in every mode.
    for mode in [
        SearchMode::Default,
        SearchMode::Auto,
        SearchMode::Exact,
        SearchMode::Scan,
    ] {
        let (hits, report) = hybrid(&corpus, [0.0, 0.0], "amber", mode, Some(&[]), 5);
        assert!(hits.is_empty());
        assert_eq!(report.vector_leg, LegState::NoEligibleMembers);
        assert_eq!(report.lexical_leg, LegState::NoEligibleMembers);
        assert_eq!(report.candidate_count, 0);
        assert_eq!(report.coverage, CandidateCoverage::Exact);
        assert_eq!(report.text.live_members, 3);
    }
}

#[test]
fn ze63_windowed_hybrid_cross_scores_and_never_claims_complete_coverage() {
    let name = "lifecycle::native_graph::tests::hybrid_ranking::ze63_windowed_hybrid_cross_scores_and_never_claims_complete_coverage";
    let corpus = corpus(name);
    let oracle = Oracle::new(&corpus);
    let mut random = crate::test_support::seeded_rng(&format!("{name}::queries"));
    let everyone = corpus.all_nodes();
    for case in 0..18 {
        let mode = [SearchMode::Default, SearchMode::Auto, SearchMode::Scan][case % 3];
        let query = [
            random.random_range(-7_i32..=7) as f32,
            random.random_range(-7_i32..=7) as f32,
        ];
        let words = query_text(&mut random);
        let chosen = everyone
            .iter()
            .copied()
            .filter(|_| random.random_bool([0.3, 1.0][case % 2]))
            .collect::<Vec<_>>();
        let set = chosen.iter().copied().collect::<BTreeSet<_>>();
        let (hits, report) = hybrid(&corpus, query, &words, mode, Some(&chosen), 4);
        // Every retained candidate carries its exact full-anchor fused score:
        // the same per-node value the exhaustive oracle assigns it.
        let (all, alpha) = oracle.hybrid(query, &words, Some(&set), usize::MAX);
        let exact = all
            .iter()
            .map(|hit| (hit.node, *hit))
            .collect::<BTreeMap<_, _>>();
        for hit in &hits {
            assert!(set.contains(&hit.node), "case {case}: ineligible hit");
            let expected = exact[&hit.node];
            assert_eq!(
                hit_bits(&[*hit]),
                hit_bits(&[expected]),
                "case {case} {mode:?}"
            );
        }
        assert!(hits.windows(2).all(|pair| pair[0].fused >= pair[1].fused));
        assert_eq!(report.effective_alpha.to_bits(), alpha.to_bits());
        assert_eq!(
            report.requested_tier,
            match mode {
                SearchMode::Default => None,
                SearchMode::Auto => Some(SearchTier::Auto),
                _ => Some(SearchTier::Scan),
            }
        );
        if report.vector_leg == LegState::Nonempty {
            assert_eq!(
                report.precision,
                ScorePrecision::Original,
                "rescored components"
            );
            if report.lexical_leg == LegState::Nonempty {
                assert_eq!(
                    report.coverage,
                    CandidateCoverage::Approximate,
                    "case {case}"
                );
            }
            if mode == SearchMode::Scan {
                assert_eq!(report.actual_tier, Some(ActualTier::Scan));
                assert_eq!(report.coverage, CandidateCoverage::Approximate);
            }
        }
    }
}

#[test]
fn ze63_candidate_outside_the_vector_window_keeps_its_vector() {
    // Sixty tied vectors fill the 50-wide producer window; the only text
    // match has a nearby vector outside it. Leaving the vector window is not
    // absence: its exact vector component must still be cross-scored.
    let mut corpus = Corpus::new();
    for _ in 0..2 {
        corpus.create(&[(Some([0.0, 0.0]), None); 30]);
    }
    let far = corpus.create(&[(Some([10.0, 0.0]), None)]);
    let target = corpus.create(&[(Some([1.0, 0.0]), Some("amber"))])[0];
    let oracle = Oracle::new(&corpus);
    let (expected, _) = oracle.hybrid([0.0, 0.0], "amber", None, 3);
    assert_eq!(expected[0].node, target);
    for mode in [SearchMode::Default, SearchMode::Auto, SearchMode::Scan] {
        let (hits, report) = hybrid(&corpus, [0.0, 0.0], "amber", mode, None, 3);
        assert_eq!(hits[0].node, target, "{mode:?}");
        assert_eq!(hits[0].vector, Some(1.0), "{mode:?}");
        assert_eq!(hit_bits(&hits[..1]), hit_bits(&expected[..1]), "{mode:?}");
        assert!(!hits.iter().any(|hit| hit.node == far[0]));
        assert_eq!(report.coverage, CandidateCoverage::Approximate);
    }
}

#[test]
fn ze63_zero_distance_ranges_score_finitely() {
    let mut corpus = Corpus::new();
    corpus.create(&[
        (Some([3.0, 3.0]), Some("amber")),
        (Some([3.0, 3.0]), None),
        (Some([3.0, 3.0]), Some("birch birch")),
        (None, Some("amber birch")),
    ]);
    let oracle = Oracle::new(&corpus);
    for mode in [SearchMode::Exact, SearchMode::Default] {
        let (hits, report) = hybrid(&corpus, [3.0, 3.0], "amber birch", mode, None, 10);
        let (expected, _) = oracle.hybrid([3.0, 3.0], "amber birch", None, 10);
        assert_eq!(hit_bits(&hits), hit_bits(&expected), "{mode:?}");
        assert!(hits.iter().all(|hit| hit.fused.is_finite()));
        assert!(report.vector_ceiling.is_some_and(|ceiling| ceiling > 0.0));
    }
}

#[test]
fn ze63_pure_compaction_preserves_every_score_and_report() {
    let name = "lifecycle::native_graph::tests::hybrid_ranking::ze63_pure_compaction_preserves_every_score_and_report";
    let corpus = corpus(name);
    let everyone = corpus.all_nodes();
    let selective = everyone.iter().copied().step_by(2).collect::<Vec<_>>();
    let observe = |corpus: &Corpus| {
        let mut observed = Vec::new();
        for (mode, eligible) in [
            (SearchMode::Exact, None),
            (SearchMode::Exact, Some(selective.as_slice())),
            (SearchMode::Default, Some(selective.as_slice())),
        ] {
            let (hits, report) = hybrid(corpus, [1.0, -2.0], "amber cedar", mode, eligible, 12);
            observed.push(format!("{:?} {report:?}", hit_bits(&hits)));
        }
        let (hits, report) = text(corpus, "birch ember", Some(&selective), 12);
        observed.push(format!("{hits:?} {report:?}"));
        observed
    };
    let before = observe(&corpus);
    let mut moved = 0;
    for _ in 0..3 {
        moved += super::consolidation::commit_maintenance(&corpus.store)
            .unwrap()
            .replaced_physical_refs;
    }
    assert!(moved > 0, "maintenance relocated physical references");
    assert_eq!(observe(&corpus), before);
}
