#![cfg(test)]
//! ZE-62: constrained ranking against an independent exhaustive oracle.
//!
//! The oracle never touches the store: it scores the exact coordinates the
//! test wrote, over the liveness the test itself tracked, with its own
//! squared-L2 formula. Coordinates are small integers so every distance is an
//! exact integer in f64 and ties are deliberate. A separate postfilter
//! (unrestricted top-k, then filter) is kept only to prove the selective cases
//! discriminate constrained ranking from filtering after the fact.

use crate::epoch::{ComputeUnits, EmbeddingRuntime, EmbeddingTower, Normalization};
use crate::lifecycle::durability::{CommitTier, DurabilityMode};
use crate::lifecycle::native_graph::NativeReadConsumer;
use crate::lifecycle::{CancelToken, OpenOptions, QueryControl, SearchTier, Store};
use crate::property_graph::query::completed::{
    ActualTier, CandidateCoverage, LegState, ScorePrecision,
};
use crate::property_graph::query::eligibility::{Eligibility, EligibleNodeSet};
use crate::property_graph::query::plan::{SearchBounds, SearchMode};
use crate::property_graph::query::runtime::{
    RuntimeContext, RuntimeError, RuntimeLimits, WorkKind,
};
use crate::property_graph::retrieval::rank::VectorRankReport;
use crate::property_graph::retrieval::{NativeRetrievalContext, RetrievalError};
use crate::property_graph::staging::{StructuredOperation, StructuredWrite, WriteImage};
use crate::property_graph::storage::GraphReadView;
use crate::property_graph::storage::tree::directory::TreeError;
use crate::property_graph::{
    ApplicationKey, CanonicalContents, CanonicalEmbedding, EntityId, EntityKind, GraphDeleteMode,
    GraphRevision, NodeId,
};
use rand::Rng;
use rand::seq::SliceRandom;
use std::collections::{BTreeMap, BTreeSet};

type Hit = (NodeId, u64, f64);

fn tower() -> EmbeddingTower {
    EmbeddingTower {
        model_id: "ze62-document".into(),
        model_version: "1".into(),
        weights_digest: vec![0x06, 0x2a],
        dims: 2,
        normalization: Normalization::None,
        prompt_prefix: "doc: ".into(),
        max_tokens: 32,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    }
}

/// A real native store plus the test's own record of what it wrote.
struct Corpus {
    store: Store,
    _directory: tempfile::TempDir,
    document: EmbeddingTower,
    /// Live vector members: full ID -> (revision, coordinates).
    live: BTreeMap<NodeId, (u64, [f32; 2])>,
    /// Live graph-only nodes and deleted nodes; never vector members.
    outside: Vec<NodeId>,
    next_key: usize,
    keys: BTreeMap<NodeId, String>,
}

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
            outside: Vec::new(),
            next_key: 0,
            keys: BTreeMap::new(),
        }
    }

    fn key(index: usize) -> String {
        format!("n{index:05}")
    }

    /// One apply call, so one new native vector source for the vector rows.
    fn create(&mut self, points: &[Option<[f32; 2]>]) -> Vec<NodeId> {
        let contents = points
            .iter()
            .map(|point| {
                let embedding = point
                    .as_ref()
                    .map(|point| CanonicalEmbedding::new(&self.document, point).unwrap());
                CanonicalContents::node(&mut [], &mut [], None, embedding).unwrap()
            })
            .collect::<Vec<_>>();
        let keys = (0..points.len())
            .map(|offset| Self::key(self.next_key + offset))
            .collect::<Vec<_>>();
        self.next_key += points.len();
        let requests = keys
            .iter()
            .zip(&contents)
            .map(|(key, contents)| StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", key).unwrap(),
                revision: GraphRevision::new(1).unwrap(),
                operation: StructuredOperation::Create,
                image: Some(WriteImage::Node(contents)),
            })
            .collect::<Vec<_>>();
        let receipts = self
            .store
            .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
            .unwrap();
        assert_eq!(receipts.len(), points.len());
        let mut nodes = Vec::new();
        for ((receipt, point), key) in receipts.iter().zip(points).zip(keys) {
            let EntityId::Node(node) = receipt.entity else {
                panic!("node write changed identity domain");
            };
            self.keys.insert(node, key);
            match point {
                Some(point) => {
                    self.live.insert(node, (1, *point));
                }
                None => self.outside.push(node),
            }
            nodes.push(node);
        }
        nodes
    }

    fn key_of(&self, node: NodeId) -> String {
        self.keys[&node].clone()
    }

    fn delete(&mut self, nodes: &[NodeId]) {
        let keys = nodes
            .iter()
            .map(|node| self.key_of(*node))
            .collect::<Vec<_>>();
        let requests = nodes
            .iter()
            .zip(&keys)
            .map(|(node, key)| StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", key).unwrap(),
                revision: GraphRevision::new(self.live[node].0 + 1).unwrap(),
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
            self.outside.push(*node);
        }
    }

    fn replace(&mut self, changes: &[(NodeId, [f32; 2])]) {
        let keys = changes
            .iter()
            .map(|(node, _)| self.key_of(*node))
            .collect::<Vec<_>>();
        let contents = changes
            .iter()
            .map(|(_, point)| {
                let embedding = CanonicalEmbedding::new(&self.document, point).unwrap();
                CanonicalContents::node(&mut [], &mut [], None, Some(embedding)).unwrap()
            })
            .collect::<Vec<_>>();
        let requests = changes
            .iter()
            .zip(&keys)
            .zip(&contents)
            .map(|(((node, _), key), contents)| StructuredWrite {
                key: ApplicationKey::new(EntityKind::Node, "app", key).unwrap(),
                revision: GraphRevision::new(self.live[node].0 + 1).unwrap(),
                operation: StructuredOperation::Put(EntityId::Node(*node)),
                image: Some(WriteImage::Node(contents)),
            })
            .collect::<Vec<_>>();
        self.store
            .apply_native_graph(&requests, &QueryControl::Cancel(CancelToken::new()))
            .unwrap();
        for (node, point) in changes {
            let entry = self.live.get_mut(node).unwrap();
            *entry = (entry.0 + 1, *point);
        }
    }

    fn all_nodes(&self) -> Vec<NodeId> {
        self.live.keys().chain(&self.outside).copied().collect()
    }
}

/// Independent exhaustive-domain oracle over the test's own records.
fn oracle(
    live: &BTreeMap<NodeId, (u64, [f32; 2])>,
    eligible: Option<&BTreeSet<NodeId>>,
    query: [f32; 2],
    k: usize,
) -> Vec<Hit> {
    let mut all = live
        .iter()
        .filter(|(node, _)| eligible.is_none_or(|set| set.contains(node)))
        .map(|(node, (revision, point))| {
            let dx = f64::from(query[0]) - f64::from(point[0]);
            let dy = f64::from(query[1]) - f64::from(point[1]);
            (*node, *revision, dx * dx + dy * dy)
        })
        .collect::<Vec<_>>();
    all.sort_by(|left, right| left.2.total_cmp(&right.2).then(left.0.cmp(&right.0)));
    all.truncate(k);
    all
}

/// Deliberately wrong comparison: rank everything, then drop ineligible hits.
fn postfilter(
    live: &BTreeMap<NodeId, (u64, [f32; 2])>,
    eligible: &BTreeSet<NodeId>,
    query: [f32; 2],
    k: usize,
) -> Vec<Hit> {
    oracle(live, None, query, k)
        .into_iter()
        .filter(|(node, _, _)| eligible.contains(node))
        .collect()
}

type RankOutcome = Result<(Vec<Hit>, VectorRankReport), RetrievalError>;

struct Rank<'a> {
    query: [f32; 2],
    mode: SearchMode,
    /// None is an omitted restriction; Some(list) may repeat and be empty.
    eligible: Option<&'a [NodeId]>,
    k: i64,
    window: u64,
}

impl NativeReadConsumer<RankOutcome> for Rank<'_> {
    fn consume<'s, 'lease, 'm, 'g>(
        &mut self,
        view: &GraphReadView<'s, 'lease, 'm, 'g>,
        runtime: &mut RuntimeContext<'lease, 'm, 'g>,
    ) -> Result<RankOutcome, TreeError> {
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
        let eligibility = set
            .as_ref()
            .map_or(Eligibility::AllIndexed, Eligibility::Set);
        let prepared = context
            .prepare_vector(&self.query, self.mode, eligibility, runtime)
            .map_err(|_| TreeError::Invalid("prepared vector"))?;
        let bounds = SearchBounds::new(self.k, self.window).unwrap();
        Ok(context
            .rank_vector(&prepared, bounds, runtime)
            .map(|ranked| {
                let hits = ranked
                    .hits()
                    .iter()
                    .map(|hit| (hit.node, hit.revision.get(), hit.distance))
                    .collect();
                (hits, ranked.report())
            }))
    }
}

fn try_rank_with(
    corpus: &Corpus,
    rank: Rank<'_>,
    limits: RuntimeLimits,
    memory: usize,
) -> Result<RankOutcome, String> {
    corpus
        .store
        .with_native_read(
            &QueryControl::Cancel(CancelToken::new()),
            limits,
            memory,
            32,
            rank,
        )
        .map_err(|error| format!("{error:?}"))
}

fn rank_with(corpus: &Corpus, rank: Rank<'_>, limits: RuntimeLimits, memory: usize) -> RankOutcome {
    try_rank_with(corpus, rank, limits, memory).unwrap()
}

fn rank(
    corpus: &Corpus,
    query: [f32; 2],
    mode: SearchMode,
    eligible: Option<&[NodeId]>,
    k: i64,
) -> RankOutcome {
    rank_with(
        corpus,
        Rank {
            query,
            mode,
            eligible,
            k,
            window: 65_536,
        },
        RuntimeLimits::default(),
        16 * 1024 * 1024,
    )
}

fn bits(hits: &[Hit]) -> Vec<(u128, u64, u64)> {
    hits.iter()
        .map(|(node, revision, distance)| (node.get(), *revision, distance.to_bits()))
        .collect()
}

/// Three vector sources with dead rows, one replacement source and graph-only
/// nodes. Integer coordinates in [-6, 6] repeat, so distance ties are common.
fn corpus(name: &str) -> Corpus {
    let mut random = crate::test_support::seeded_rng(name);
    let point = |random: &mut rand_chacha::ChaCha8Rng| {
        [
            random.random_range(-6_i32..=6) as f32,
            random.random_range(-6_i32..=6) as f32,
        ]
    };
    let mut corpus = Corpus::new();
    let first = (0..48)
        .map(|_| Some(point(&mut random)))
        .collect::<Vec<_>>();
    let first = corpus.create(&first);
    let mut second = (0..40)
        .map(|_| Some(point(&mut random)))
        .collect::<Vec<_>>();
    // A deliberate tie group at the origin spanning the second and third sources.
    for slot in second.iter_mut().take(6) {
        *slot = Some([0.0, 0.0]);
    }
    let second = corpus.create(&second);
    let third = corpus.create(&[Some([0.0, 0.0]), Some([0.0, 0.0]), Some(point(&mut random))]);
    corpus.create(&[None, None, None]);
    corpus.delete(&[first[1], first[7], first[20], second[30], third[2]]);
    corpus.replace(&[
        (first[3], [0.0, 0.0]),
        (second[10], [6.0, -6.0]),
        (second[11], [1.0, 1.0]),
    ]);
    corpus
}

fn report_for_exact(report: VectorRankReport, eligible: u64) {
    assert_eq!(report.eligible_members, eligible);
    if eligible == 0 {
        assert_eq!(report.actual_tier, None);
        assert_eq!(report.precision, ScorePrecision::NotApplicable);
        assert_eq!(report.leg, LegState::NoEligibleMembers);
    } else {
        assert_eq!(report.actual_tier, Some(ActualTier::Exact));
        assert_eq!(report.precision, ScorePrecision::Original);
        assert_eq!(report.leg, LegState::Nonempty);
    }
    assert_eq!(report.coverage, CandidateCoverage::Exact);
}

#[test]
fn ze62_exact_constrained_top_k_matches_independent_oracle() {
    let name = "lifecycle::native_graph::tests::ranking::ze62_exact_constrained_top_k_matches_independent_oracle";
    let corpus = corpus(name);
    let mut random = crate::test_support::seeded_rng(&format!("{name}::queries"));
    let everyone = corpus.all_nodes();
    let live = corpus.live.keys().copied().collect::<Vec<_>>();
    let mut postfilter_differs = 0;
    for case in 0..24 {
        let query = [
            random.random_range(-7_i32..=7) as f32,
            random.random_range(-7_i32..=7) as f32,
        ];
        let k = [1_i64, 3, 5, 17, 200][case % 5];
        // Selective subsets draw from every node, including dead and
        // graph-only IDs, which must never become candidates.
        let fraction = [0.05, 0.2, 0.5, 1.0][case % 4];
        let chosen = everyone
            .iter()
            .copied()
            .filter(|_| random.random_bool(fraction))
            .collect::<Vec<_>>();
        let set = chosen.iter().copied().collect::<BTreeSet<_>>();
        let expected = oracle(&corpus.live, Some(&set), query, k as usize);
        let (hits, report) = rank(&corpus, query, SearchMode::Exact, Some(&chosen), k).unwrap();
        assert_eq!(
            bits(&hits),
            bits(&expected),
            "case {case} query {query:?} k {k}"
        );
        let eligible = live.iter().filter(|node| set.contains(node)).count() as u64;
        report_for_exact(report, eligible);
        assert_eq!(report.live_members, corpus.live.len() as u64);
        if bits(&postfilter(&corpus.live, &set, query, k as usize)) != bits(&expected) {
            postfilter_differs += 1;
        }
    }
    assert!(
        postfilter_differs >= 3,
        "selective cases must discriminate a postfilter, saw {postfilter_differs}"
    );
    // The complete domain: omitted restriction equals the full-live oracle.
    for query in [[0.0, 0.0], [5.0, -3.0], [-7.0, 7.0]] {
        let expected = oracle(&corpus.live, None, query, 4096);
        let (hits, report) = rank(&corpus, query, SearchMode::Exact, None, 4096).unwrap();
        assert_eq!(bits(&hits), bits(&expected));
        assert_eq!(hits.len(), corpus.live.len());
        report_for_exact(report, corpus.live.len() as u64);
    }
}

#[test]
fn ze62_absent_restriction_differs_from_explicit_empty_set() {
    let corpus = corpus(
        "lifecycle::native_graph::tests::ranking::ze62_absent_restriction_differs_from_explicit_empty_set",
    );
    let query = [1.0, -1.0];
    let live = corpus.live.keys().copied().collect::<Vec<_>>();
    for mode in [
        SearchMode::Default,
        SearchMode::Auto,
        SearchMode::Exact,
        SearchMode::Scan,
    ] {
        let (all, all_report) = rank(&corpus, query, mode, None, 10).unwrap();
        assert_eq!(all.len(), 10, "{mode:?}");
        assert_eq!(all_report.leg, LegState::Nonempty);
        assert_eq!(all_report.eligible_members, corpus.live.len() as u64);

        let (none, none_report) = rank(&corpus, query, mode, Some(&[]), 10).unwrap();
        assert!(none.is_empty(), "{mode:?}");
        assert_eq!(none_report.leg, LegState::NoEligibleMembers);
        assert_eq!(none_report.eligible_members, 0);
        assert_eq!(none_report.live_members, corpus.live.len() as u64);
        assert_eq!(none_report.actual_tier, None);
        assert_eq!(none_report.coverage, CandidateCoverage::Exact);

        // Only dead and graph-only IDs: a nonempty set with no vector members.
        let (outside, outside_report) =
            rank(&corpus, query, mode, Some(&corpus.outside), 10).unwrap();
        assert!(outside.is_empty(), "{mode:?}");
        assert_eq!(outside_report.leg, LegState::NoEligibleMembers);

        // Every live member explicitly listed ranks like the omitted restriction.
        let (listed, listed_report) = rank(&corpus, query, mode, Some(&live), 10).unwrap();
        assert_eq!(bits(&listed), bits(&all), "{mode:?}");
        assert_eq!(listed_report, all_report, "{mode:?}");
    }
}

#[test]
fn ze62_duplicate_eligibility_inputs_do_not_change_candidates() {
    let name = "lifecycle::native_graph::tests::ranking::ze62_duplicate_eligibility_inputs_do_not_change_candidates";
    let corpus = corpus(name);
    let mut random = crate::test_support::seeded_rng(&format!("{name}::subset"));
    let chosen = corpus
        .all_nodes()
        .into_iter()
        .filter(|_| random.random_bool(0.3))
        .collect::<Vec<_>>();
    let mut repeated = chosen
        .iter()
        .flat_map(|node| [*node, *node, *node])
        .collect::<Vec<_>>();
    repeated.shuffle(&mut random);
    let set = chosen.iter().copied().collect::<BTreeSet<_>>();
    for mode in [SearchMode::Default, SearchMode::Exact, SearchMode::Scan] {
        let query = [2.0, 2.0];
        let unique = rank(&corpus, query, mode, Some(&chosen), 8).unwrap();
        let duplicated = rank(&corpus, query, mode, Some(&repeated), 8).unwrap();
        assert_eq!(bits(&unique.0), bits(&duplicated.0), "{mode:?}");
        assert_eq!(unique.1, duplicated.1, "{mode:?}");
        assert!(unique.0.iter().all(|(node, _, _)| set.contains(node)));
    }
    let exact = rank(&corpus, [2.0, 2.0], SearchMode::Exact, Some(&repeated), 8).unwrap();
    assert_eq!(
        bits(&exact.0),
        bits(&oracle(&corpus.live, Some(&set), [2.0, 2.0], 8))
    );
}

#[test]
fn ze62_full_id_ties_order_by_ascending_node_across_sources() {
    let corpus = corpus(
        "lifecycle::native_graph::tests::ranking::ze62_full_id_ties_order_by_ascending_node_across_sources",
    );
    let origin = corpus
        .live
        .iter()
        .filter(|(_, (_, point))| *point == [0.0, 0.0])
        .map(|(node, _)| *node)
        .collect::<Vec<_>>();
    assert!(origin.len() >= 8, "tie group spans the sources");
    for k in [1_usize, 2, 5, origin.len() - 1] {
        let (hits, _) = rank(&corpus, [0.0, 0.0], SearchMode::Exact, None, k as i64).unwrap();
        let expected = origin.iter().take(k).copied().collect::<Vec<_>>();
        assert_eq!(
            hits.iter().map(|hit| hit.0).collect::<Vec<_>>(),
            expected,
            "k {k}"
        );
        assert!(hits.iter().all(|hit| hit.2.to_bits() == 0.0_f64.to_bits()));
    }
    // A restriction that omits the smallest tied IDs selects the next ones.
    let tail = origin.iter().skip(3).copied().collect::<Vec<_>>();
    let (hits, _) = rank(&corpus, [0.0, 0.0], SearchMode::Exact, Some(&tail), 2).unwrap();
    assert_eq!(hits.iter().map(|hit| hit.0).collect::<Vec<_>>(), tail[..2]);
}

#[test]
fn ze62_graph_route_never_emits_ineligible_nodes_or_upgrades_coverage() {
    let name = "lifecycle::native_graph::tests::ranking::ze62_graph_route_never_emits_ineligible_nodes_or_upgrades_coverage";
    let corpus = corpus(name);
    let mut random = crate::test_support::seeded_rng(&format!("{name}::queries"));
    let everyone = corpus.all_nodes();
    let mut traversed_cases = 0;
    let mut exact_matches_under_approximate = 0;
    for case in 0..40 {
        let query = [
            random.random_range(-7_i32..=7) as f32,
            random.random_range(-7_i32..=7) as f32,
        ];
        let k = [1_i64, 4, 10][case % 3];
        let fraction = [0.1, 0.4, 0.8, 1.0][case % 4];
        let chosen = everyone
            .iter()
            .copied()
            .filter(|_| random.random_bool(fraction))
            .collect::<Vec<_>>();
        let set = chosen.iter().copied().collect::<BTreeSet<_>>();
        let eligible = corpus.live.keys().filter(|node| set.contains(node)).count();
        let (hits, report) = rank(&corpus, query, SearchMode::Default, Some(&chosen), k).unwrap();
        assert_eq!(hits.len(), eligible.min(k as usize), "case {case}");
        for (node, revision, distance) in &hits {
            assert!(set.contains(node), "case {case}: ineligible {node:?}");
            let (live_revision, point) = corpus.live.get(node).expect("dead node emitted");
            assert_eq!(revision, live_revision);
            // Retained hits are exactly rescored in the original domain.
            let exact = oracle(&corpus.live, Some(&BTreeSet::from([*node])), query, 1);
            assert_eq!(distance.to_bits(), exact[0].2.to_bits(), "{point:?}");
        }
        assert!(hits.windows(2).all(|pair| pair[0].2 <= pair[1].2));
        assert_eq!(report.requested_tier, None);
        if eligible == 0 {
            assert_eq!(report.leg, LegState::NoEligibleMembers);
            continue;
        }
        assert_eq!(report.precision, ScorePrecision::Original);
        if report.traversed_sources > 0 {
            traversed_cases += 1;
            assert_eq!(report.actual_tier, Some(ActualTier::Graph));
            assert_eq!(report.coverage, CandidateCoverage::Approximate);
            if bits(&hits) == bits(&oracle(&corpus.live, Some(&set), query, k as usize)) {
                exact_matches_under_approximate += 1;
            }
        } else {
            assert!(report.fallback_count > 0);
            assert_eq!(report.actual_tier, Some(ActualTier::Exact));
            assert_eq!(report.coverage, CandidateCoverage::Exact);
            assert_eq!(
                bits(&hits),
                bits(&oracle(&corpus.live, Some(&set), query, k as usize))
            );
        }
    }
    assert!(
        traversed_cases >= 10,
        "graph traversal exercised {traversed_cases} times"
    );
    assert!(
        exact_matches_under_approximate > 0,
        "an exact-looking rescored answer still reports approximate coverage"
    );
}

#[test]
fn ze62_graph_selectivity_fallback_scores_every_eligible_row_exactly() {
    let name = "lifecycle::native_graph::tests::ranking::ze62_graph_selectivity_fallback_scores_every_eligible_row_exactly";
    let corpus = corpus(name);
    let mut random = crate::test_support::seeded_rng(&format!("{name}::queries"));
    let everyone = corpus.all_nodes();
    // Authentic sources are at most a few dozen rows, below the production
    // selectivity budget, so the test tightens it (as the legacy planner
    // tests do) to drive every traversal into the exhaustive fallback.
    crate::property_graph::retrieval::rank::with_visited_budget_override(1, || {
        for case in 0..16 {
            let query = [
                random.random_range(-7_i32..=7) as f32,
                random.random_range(-7_i32..=7) as f32,
            ];
            let k = [1_i64, 3, 9, 40][case % 4];
            let chosen = everyone
                .iter()
                .copied()
                .filter(|_| random.random_bool([0.1, 0.5, 1.0][case % 3]))
                .collect::<Vec<_>>();
            let set = chosen.iter().copied().collect::<BTreeSet<_>>();
            let expected = oracle(&corpus.live, Some(&set), query, k as usize);
            let (hits, report) =
                rank(&corpus, query, SearchMode::Default, Some(&chosen), k).unwrap();
            assert_eq!(bits(&hits), bits(&expected), "case {case}");
            if expected.is_empty() {
                assert_eq!(report.leg, LegState::NoEligibleMembers);
                continue;
            }
            // Every eligible source fell back and scored its complete mask
            // exactly, so exact coverage is established, not inferred.
            assert!(report.fallback_count > 0, "case {case}");
            assert_eq!(report.traversed_sources, 0, "case {case}");
            assert_eq!(report.actual_tier, Some(ActualTier::Exact));
            assert_eq!(report.precision, ScorePrecision::Original);
            assert_eq!(report.coverage, CandidateCoverage::Exact);
        }
    });
}

#[test]
fn ze62_omitted_tier_differs_from_explicit_auto() {
    let corpus = corpus(
        "lifecycle::native_graph::tests::ranking::ze62_omitted_tier_differs_from_explicit_auto",
    );
    let chosen = corpus.live.keys().step_by(2).copied().collect::<Vec<_>>();
    let omitted = rank(&corpus, [3.0, 1.0], SearchMode::Default, Some(&chosen), 6).unwrap();
    let auto = rank(&corpus, [3.0, 1.0], SearchMode::Auto, Some(&chosen), 6).unwrap();
    assert_eq!(bits(&omitted.0), bits(&auto.0));
    assert_eq!(omitted.1.requested_tier, None);
    assert_eq!(auto.1.requested_tier, Some(SearchTier::Auto));
    assert_eq!(
        VectorRankReport {
            requested_tier: None,
            ..auto.1
        },
        omitted.1
    );
    let exact = rank(&corpus, [3.0, 1.0], SearchMode::Exact, Some(&chosen), 6).unwrap();
    assert_eq!(exact.1.requested_tier, Some(SearchTier::Exact));
    let scan = rank(&corpus, [3.0, 1.0], SearchMode::Scan, Some(&chosen), 6).unwrap();
    assert_eq!(scan.1.requested_tier, Some(SearchTier::Scan));
}

#[test]
fn ze62_scan_route_keeps_quantized_precision_and_approximate_coverage() {
    let name = "lifecycle::native_graph::tests::ranking::ze62_scan_route_keeps_quantized_precision_and_approximate_coverage";
    let corpus = corpus(name);
    let mut random = crate::test_support::seeded_rng(&format!("{name}::subset"));
    for case in 0..8 {
        let chosen = corpus
            .all_nodes()
            .into_iter()
            .filter(|_| random.random_bool(0.35))
            .collect::<Vec<_>>();
        let set = chosen.iter().copied().collect::<BTreeSet<_>>();
        let eligible = corpus.live.keys().filter(|node| set.contains(node)).count();
        let (hits, report) =
            rank(&corpus, [-2.0, 4.0], SearchMode::Scan, Some(&chosen), 7).unwrap();
        assert_eq!(hits.len(), eligible.min(7), "case {case}");
        assert!(hits.iter().all(|(node, revision, _)| {
            set.contains(node) && corpus.live.get(node).map(|entry| entry.0) == Some(*revision)
        }));
        assert!(
            hits.windows(2)
                .all(|pair| (pair[0].2, pair[0].0) < (pair[1].2, pair[1].0))
        );
        if eligible > 0 {
            assert_eq!(report.actual_tier, Some(ActualTier::Scan));
            assert_eq!(report.precision, ScorePrecision::Quantized);
            assert_eq!(report.coverage, CandidateCoverage::Approximate);
        }
    }
}

#[test]
fn ze62_empty_window_or_leg_cannot_assert_an_empty_population() {
    let corpus = corpus(
        "lifecycle::native_graph::tests::ranking::ze62_empty_window_or_leg_cannot_assert_an_empty_population",
    );
    let one = corpus.live.keys().take(1).copied().collect::<Vec<_>>();
    for mode in [
        SearchMode::Default,
        SearchMode::Auto,
        SearchMode::Exact,
        SearchMode::Scan,
    ] {
        for eligible in [None, Some(one.as_slice())] {
            let refused = rank_with(
                &corpus,
                Rank {
                    query: [0.0, 0.0],
                    mode,
                    eligible,
                    k: 1,
                    window: 0,
                },
                RuntimeLimits::default(),
                16 * 1024 * 1024,
            );
            assert!(
                matches!(
                    refused,
                    Err(RetrievalError::CandidateWindow { window: 0, .. })
                ),
                "{mode:?}: {refused:?}"
            );
        }
        // Nothing eligible needs no retained window, and says why it is empty.
        let (hits, report) = rank_with(
            &corpus,
            Rank {
                query: [0.0, 0.0],
                mode,
                eligible: Some(&corpus.outside),
                k: 1,
                window: 0,
            },
            RuntimeLimits::default(),
            16 * 1024 * 1024,
        )
        .unwrap();
        assert!(hits.is_empty());
        assert_eq!(report.leg, LegState::NoEligibleMembers);
    }

    // A declared vector space with no live members is a distinct state.
    let mut empty = Corpus::new();
    let graph_only = empty.create(&[None, None]);
    let vectors = empty.create(&[Some([1.0, 1.0]), Some([2.0, 2.0])]);
    empty.delete(&vectors);
    for eligible in [None, Some(graph_only.as_slice()), Some(vectors.as_slice())] {
        let (hits, report) = rank(&empty, [0.0, 0.0], SearchMode::Default, eligible, 3).unwrap();
        assert!(hits.is_empty());
        assert_eq!(report.leg, LegState::NoIndexedPopulation);
        assert_eq!(report.live_members, 0);
    }
}

#[test]
fn ze62_budget_exhaustion_returns_no_partial_ranking() {
    let corpus = corpus(
        "lifecycle::native_graph::tests::ranking::ze62_budget_exhaustion_returns_no_partial_ranking",
    );
    let request = |mode| Rank {
        query: [0.0, 0.0],
        mode,
        eligible: None,
        k: 5,
        window: 65_536,
    };
    for mode in [SearchMode::Exact, SearchMode::Default, SearchMode::Scan] {
        let complete = rank(&corpus, [0.0, 0.0], mode, None, 5).unwrap();
        // Preparation consumes the two query coordinates. Exact and Scan
        // charge two more per eligible member, so this allows about half.
        let half = 2 + corpus.live.len() as u64;
        let mut refusals = 0;
        for limit in (0..=4 * half).step_by(7).chain([half]) {
            let limits = RuntimeLimits::default()
                .with_limit(WorkKind::VectorCoordinates, limit)
                .unwrap();
            match try_rank_with(&corpus, request(mode), limits, 16 * 1024 * 1024) {
                // Any success is the complete ranking, never a prefix.
                Ok(Ok(ranked)) => {
                    assert_eq!(bits(&ranked.0), bits(&complete.0), "{mode:?} {limit}");
                    assert_eq!(ranked.1, complete.1, "{mode:?} {limit}");
                }
                Ok(Err(error)) => {
                    assert!(
                        matches!(
                            error,
                            RetrievalError::Storage(TreeError::Runtime(RuntimeError::Limit(
                                WorkKind::VectorCoordinates
                            )))
                        ),
                        "{mode:?} {limit}: {error:?}"
                    );
                    refusals += 1;
                }
                // Too little for query preparation itself.
                Err(_) => {
                    assert!(limit < 2, "{mode:?} {limit}");
                    refusals += 1;
                }
            }
        }
        assert!(refusals > 1, "{mode:?}");
        if mode != SearchMode::Default {
            let limits = RuntimeLimits::default()
                .with_limit(WorkKind::VectorCoordinates, half)
                .unwrap();
            assert!(matches!(
                try_rank_with(&corpus, request(mode), limits, 16 * 1024 * 1024),
                Ok(Err(RetrievalError::Storage(TreeError::Runtime(
                    RuntimeError::Limit(WorkKind::VectorCoordinates)
                ))))
            ));
        }

        // Shrinking the one query memory owner eventually refuses; every
        // success above that point is still the complete ranking.
        let mut memory_refusals = 0;
        for memory in [
            8 << 20,
            2 << 20,
            1 << 20,
            512 << 10,
            256 << 10,
            128 << 10,
            64 << 10,
        ] {
            match try_rank_with(&corpus, request(mode), RuntimeLimits::default(), memory) {
                Ok(Ok(ranked)) => {
                    assert_eq!(bits(&ranked.0), bits(&complete.0), "{mode:?} {memory}");
                }
                Ok(Err(_)) | Err(_) => memory_refusals += 1,
            }
        }
        assert!(memory_refusals > 0, "{mode:?}");
    }
}

#[test]
fn ze195_mixed_traversal_and_fallback_stays_approximate() {
    let mut corpus = Corpus::new();
    corpus.create(&[Some([0.0, 0.0]), Some([1.0, 0.0]), Some([2.0, 0.0])]);
    corpus.create(&(0..48).map(|i| Some([i as f32, 1.0])).collect::<Vec<_>>());
    crate::property_graph::retrieval::rank::with_visited_budget_override(3, || {
        let (_, report) = rank(&corpus, [0.0, 0.0], SearchMode::Default, None, 1).unwrap();
        assert_eq!(report.traversed_sources, 1);
        assert_eq!(report.fallback_count, 1);
        assert_eq!(report.actual_tier, Some(ActualTier::Graph));
        assert_eq!(report.precision, ScorePrecision::Original);
        assert_eq!(report.coverage, CandidateCoverage::Approximate);
    });
}

#[test]
fn ze195_foreign_prepared_vector_is_rejected() {
    use crate::property_graph::query::resources::QueryMemory;
    use crate::property_graph::resources::GraphResources;
    use crate::property_graph::storage::tree::directory::TreeResources;
    use crate::property_graph::storage::{NativeCatalog, NativeQuerySource, NativeReadCapability};
    let mut corpus = Corpus::new();
    corpus.create(&[Some([0.0, 0.0])]);
    let first = corpus.store.admit_native_read().unwrap();
    let second = corpus.store.admit_native_read().unwrap();
    assert_eq!(first.bundle().base(), second.bundle().base());
    let shared = GraphResources::from_store(&corpus.store).unwrap();
    let first_memory = QueryMemory::new(&shared, 8 * 1024 * 1024).unwrap();
    let second_memory = QueryMemory::new(&shared, 8 * 1024 * 1024).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let mut first_runtime =
        RuntimeContext::new(&first, &control, &first_memory, RuntimeLimits::default()).unwrap();
    let mut second_runtime =
        RuntimeContext::new(&second, &control, &second_memory, RuntimeLimits::default()).unwrap();
    let first_capability = NativeReadCapability::admit(&first, &first_runtime).unwrap();
    let mut resources = TreeResources::for_query(&mut first_runtime).unwrap();
    let first_source = NativeQuerySource::new(first_capability, &resources, 16).unwrap();
    let first_catalog = NativeCatalog::open(&first_source, &mut resources).unwrap();
    drop(resources);
    let first_view = GraphReadView::new(&first_source, &first_catalog).unwrap();
    let first_context = NativeRetrievalContext::new(&first_view, &mut first_runtime).unwrap();
    let prepared = first_context
        .prepare_vector(
            &[0.0, 0.0],
            SearchMode::Exact,
            Eligibility::AllIndexed,
            &mut first_runtime,
        )
        .unwrap();
    let second_capability = NativeReadCapability::admit(&second, &second_runtime).unwrap();
    let mut resources = TreeResources::for_query(&mut second_runtime).unwrap();
    let second_source = NativeQuerySource::new(second_capability, &resources, 16).unwrap();
    let second_catalog = NativeCatalog::open(&second_source, &mut resources).unwrap();
    drop(resources);
    let second_view = GraphReadView::new(&second_source, &second_catalog).unwrap();
    let second_context = NativeRetrievalContext::new(&second_view, &mut second_runtime).unwrap();
    assert!(matches!(
        second_context.rank_vector(
            &prepared,
            SearchBounds::new(1, 16).unwrap(),
            &mut second_runtime
        ),
        Err(RetrievalError::Storage(TreeError::Invalid(
            "foreign prepared native vector"
        )))
    ));
}

#[test]
fn ze195_unindexed_v1_refuses_scan_and_graph() {
    use crate::property_graph::StoreInstanceId;
    use crate::property_graph::storage::allocation::artifact_path;
    use crate::property_graph::storage::artifact::{self, Block, BlockKind, ContainerKind};
    use xxhash_rust::xxh3::xxh3_64;
    // Build an unpublished fixture, then replace its V2 source with a raw V1
    // manifest. Retain original rows and liveness; no opened artifact is edited.
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let mut bundle = super::actual_producer_bundle_with_dimensions(
        &store,
        directory.path(),
        StoreInstanceId::new(195).unwrap(),
        0,
        true,
        0,
        false,
        2,
    );
    let mut objects = Vec::new();
    for entry in std::fs::read_dir(directory.path()).unwrap() {
        let path = entry.unwrap().path();
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(frame) = artifact::decode(ContainerKind::Object, None, &bytes) else {
            continue;
        };
        let mut payloads = Vec::new();
        let mut index = 0;
        while let Ok(reference) = frame.reference(index) {
            payloads.push((
                reference,
                frame.framed_block(reference).unwrap().payload().to_vec(),
            ));
            index += 1;
        }
        objects.push((frame.identity(), payloads));
    }
    let old = objects
        .iter()
        .flat_map(|(_, blocks)| blocks)
        .find(|(reference, payload)| {
            reference.kind == BlockKind::CommitParticipant
                && payload.len() == 200
                && payload[8] == 2
                && payload[9] == 2
        })
        .unwrap()
        .0;
    let new = artifact::PhysicalRef {
        length: 24 + 144,
        ..old
    };
    let mut old_bytes = [0; 32];
    let mut new_bytes = [0; 32];
    artifact::encode_reference(old, &mut old_bytes).unwrap();
    artifact::encode_reference(new, &mut new_bytes).unwrap();
    for (identity, payloads) in objects {
        let mut changed = false;
        let mut blocks = Vec::new();
        for (reference, mut payload) in payloads {
            if reference == old {
                payload.truncate(144);
                payload[6..8].copy_from_slice(&1_u16.to_le_bytes());
                blocks.push((reference.kind, payload));
                // Preserve following block offsets while filling the removed
                // 56 bytes with an unreferenced, valid framed block.
                blocks.push((BlockKind::RetrievalRows, vec![0; 32]));
                changed = true;
                continue;
            }
            let mut replaced = false;
            if matches!(reference.kind, BlockKind::TreePage) {
                for offset in 0..payload.len().saturating_sub(31) {
                    if payload[offset..offset + 32] == old_bytes {
                        payload[offset..offset + 32].copy_from_slice(&new_bytes);
                        replaced = true;
                    }
                }
                if replaced {
                    payload[56..64].fill(0);
                    let checksum = xxh3_64(&payload);
                    payload[56..64].copy_from_slice(&checksum.to_le_bytes());
                }
            }
            changed |= replaced;
            blocks.push((reference.kind, payload));
        }
        if !changed {
            continue;
        }
        let blocks = blocks
            .iter()
            .map(|(kind, payload)| Block {
                kind: *kind,
                payload,
            })
            .collect::<Vec<_>>();
        let mut bytes = vec![0; artifact::encoded_len(ContainerKind::Object, &blocks).unwrap()];
        artifact::encode_into(ContainerKind::Object, identity, &blocks, &mut bytes).unwrap();
        let frame = artifact::decode(ContainerKind::Object, None, &bytes).unwrap();
        let checksum = frame
            .framed_block(frame.reference(0).unwrap())
            .unwrap()
            .file_checksum();
        for required in bundle
            .wal_roots
            .slots
            .iter_mut()
            .flatten()
            .chain([&mut bundle.catalog])
            .chain(bundle.vector.iter_mut())
            .chain(bundle.text.iter_mut())
        {
            if required.object.artifact == identity.artifact {
                required.object.bytes = bytes.len() as u32;
                required.object.checksum = checksum;
            }
        }
        std::fs::write(artifact_path(directory.path(), identity.artifact), &bytes).unwrap();
    }
    store.install_native_graph_for_test(bundle).unwrap();
    let corpus = Corpus {
        store,
        _directory: directory,
        document: tower(),
        live: BTreeMap::new(),
        outside: vec![],
        next_key: 0,
        keys: BTreeMap::new(),
    };
    let (hits, report) = rank(&corpus, [0.0, 0.0], SearchMode::Exact, None, 1).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(report.live_members > 0);
    for mode in [SearchMode::Scan, SearchMode::Default, SearchMode::Auto] {
        assert!(
            matches!(
                rank(&corpus, [0.0, 0.0], mode, None, 1),
                Err(RetrievalError::UnindexedVectorSource)
            ),
            "mode {mode:?}"
        );
    }
}
