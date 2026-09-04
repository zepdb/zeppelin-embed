#![allow(clippy::expect_used)]

mod test_support;

use std::collections::BTreeSet;

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestRunner};
use rand::RngCore;
use tempfile::tempdir;
use zeppelin_embed::diag::HybridTierResolution;
use zeppelin_embed::fts::index::DEFAULT_FIELD;
use zeppelin_embed::fts::search::TermQuery;
use zeppelin_embed::fusion::HybridQuery;
use zeppelin_embed::graph::search::QueryQosClass;
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::{
    CancelToken, GraphSearchOptions, OpenOptions, QueryControl, SearchOptions, SearchTier, Store,
};
use zeppelin_embed::meta::{
    Predicate, PredicateValue, RangeBound, RangePredicate, TIMESTAMP_COLUMN,
};
use zeppelin_embed::planner::{ExplicitScanTier, ScanReason, ScanReasonCounter};

fn diagnostics_v1_text(diagnostics: &zeppelin_embed::diag::QueryDiagnostics) -> String {
    use std::fmt::Write as _;

    let mut text = String::new();
    let _ = writeln!(
        text,
        "snapshot_generation = {}",
        diagnostics.snapshot_generation
    );
    let _ = writeln!(
        text,
        "indexed_through_seq = {}",
        diagnostics.indexed_through_seq.get()
    );
    let _ = writeln!(text, "plan.count = {}", diagnostics.plan.len());
    for (index, plan) in diagnostics.plan.iter().enumerate() {
        let _ = writeln!(text, "plan.{index}.tier = {:?}", plan.tier);
        let _ = writeln!(text, "plan.{index}.branch = {:?}", plan.branch);
        let _ = writeln!(text, "plan.{index}.scan_reason = {:?}", plan.scan_reason);
        let _ = writeln!(text, "plan.{index}.filter_mode = {:?}", plan.filter_mode);
        let _ = writeln!(
            text,
            "plan.{index}.filter_cardinality = {}",
            plan.filter_cardinality
        );
    }
    let _ = writeln!(text, "approximate = {}", diagnostics.approximate);
    let _ = writeln!(text, "exact_rescore = {}", diagnostics.exact_rescore);
    let _ = writeln!(
        text,
        "fusion = {}",
        if diagnostics.fusion.is_some() {
            "some"
        } else {
            "none"
        }
    );
    let _ = writeln!(
        text,
        "hybrid_tier_resolution = {:?}",
        diagnostics.hybrid_tier_resolution
    );
    let _ = writeln!(text, "requested_k = {}", diagnostics.requested_k);
    let _ = writeln!(text, "returned = {}", diagnostics.returned);
    let _ = writeln!(text, "budget_exhausted = {}", diagnostics.budget_exhausted);
    let _ = writeln!(
        text,
        "counters.scan.dims_touched = {}",
        diagnostics.counters.scan.dims_touched
    );
    let _ = writeln!(
        text,
        "counters.scan.bytes_read = {}",
        diagnostics.counters.scan.bytes_read
    );
    let _ = writeln!(text, "counters.scan.threads_used = <runtime>");
    let _ = writeln!(text, "counters.scan.worker_thread_ids.count = <runtime>");
    let _ = writeln!(text, "counters.graph = {:?}", diagnostics.counters.graph);
    let _ = writeln!(
        text,
        "counters.lexical = {:?}",
        diagnostics.counters.lexical
    );
    let _ = writeln!(
        text,
        "embedding_epoch = {}",
        if diagnostics.embedding_epoch.is_some() {
            "some"
        } else {
            "none"
        }
    );
    let _ = writeln!(
        text,
        "tokenizer_epoch = {}",
        if diagnostics.tokenizer_epoch.is_some() {
            "some"
        } else {
            "none"
        }
    );
    let _ = writeln!(text, "observed_qos.class = <runtime>");
    let _ = writeln!(text, "observed_qos.relative_priority = <runtime>");
    let _ = writeln!(text, "elapsed = <duration>");
    text
}

fn populated_store() -> (tempfile::TempDir, Store) {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(1), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_text("bronze zeppelin"),
            IngestDocument::new(
                DocumentVersion::new(DocId::new(2), Revision::new(1)),
                vec![0.0, 1.0],
            )
            .with_text("silver airship"),
        ]))
        .expect("ingest corpus");
    (directory, store)
}

#[test]
fn an_exact_flag_implies_equality_with_the_oracle() {
    let (_directory, store) = populated_store();
    let mut seed_rng = test_support::seeded_rng(
        "diagnostics_truthfulness::an_exact_flag_implies_equality_with_the_oracle",
    );
    let mut runner = TestRunner::new(Config {
        rng_seed: RngSeed::Fixed(seed_rng.next_u64()),
        ..Config::default()
    });
    runner
        .run(&(-100.0_f32..100.0, -100.0_f32..100.0), |query| {
            let vector = [query.0, query.1];
            let outcome = store
                .search(
                    SearchRequest::new(&vector),
                    2,
                    SearchOptions::default(),
                    QueryControl::Cancel(CancelToken::new()),
                )
                .expect("query store");
            let actual = outcome
                .candidates
                .iter()
                .map(|candidate| candidate.document().expect("document").doc_id())
                .collect::<BTreeSet<_>>();
            let expected = [DocId::new(1), DocId::new(2)]
                .into_iter()
                .collect::<BTreeSet<_>>();

            prop_assert!(!outcome.diagnostics.approximate);
            prop_assert!(!outcome.diagnostics.exact_rescore);
            prop_assert_eq!(actual, expected);
            Ok(())
        })
        .expect("seeded diagnostics property");
    store.close().expect("close store");
}

#[test]
fn the_reported_plan_equals_the_branch_the_executor_actually_took() {
    let (_directory, store) = populated_store();
    let predicate = Predicate::Range(RangePredicate {
        column: TIMESTAMP_COLUMN,
        lower: Some(RangeBound::inclusive(PredicateValue::I64(0))),
        upper: Some(RangeBound::inclusive(PredicateValue::I64(0))),
    });
    let outcome = store
        .search_filtered(
            SearchRequest::new(&[1.0, 0.0]),
            &predicate,
            2,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("filtered query");

    assert_eq!(outcome.diagnostics.plan, outcome.plans);
    assert!(!outcome.diagnostics.approximate);
    assert!(!outcome.diagnostics.exact_rescore);
    store.close().expect("close store");
}

#[test]
fn counters_are_consistent_and_move_by_known_deltas() {
    let (_directory, store) = populated_store();
    let query = || {
        store
            .search(
                SearchRequest::new(&[1.0, 0.0]),
                3,
                SearchOptions::default(),
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect("query store")
    };
    let before = query();
    assert!(before.diagnostics.counters.scan.dims_touched <= 2 * 2);

    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(3), Revision::new(1)),
            vec![-1.0, 0.0],
        )]))
        .expect("ingest one known row");
    let after = query();
    assert!(after.diagnostics.counters.scan.dims_touched <= 2 * 3);
    assert_eq!(
        after.diagnostics.counters.scan.dims_touched
            - before.diagnostics.counters.scan.dims_touched,
        2
    );
    assert_eq!(
        after.diagnostics.counters.scan.bytes_read - before.diagnostics.counters.scan.bytes_read,
        1
    );
    assert_eq!(
        after.diagnostics.counters.graph,
        before.diagnostics.counters.graph
    );
    assert_eq!(
        after.diagnostics.counters.lexical,
        before.diagnostics.counters.lexical
    );
    store.close().expect("close store");
}

#[test]
fn scanned_segment_plans_have_typed_reasons() {
    let (_directory, store) = populated_store();
    let active = store
        .search(
            SearchRequest::new(&[1.0, 0.0]),
            2,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("query active segment");
    assert_eq!(
        active.diagnostics.plan[0].scan_reason,
        Some(ScanReason::ActiveSegment)
    );

    store.seal().expect("seal below-threshold segment");
    let automatic = store
        .search(
            SearchRequest::new(&[1.0, 0.0]),
            2,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("query automatic sealed segment");
    assert_eq!(
        automatic.diagnostics.plan[0].scan_reason,
        Some(ScanReason::BelowGraphThreshold {
            rows: 2,
            min_rows: 30_000,
        })
    );

    let explicit = store
        .search(
            SearchRequest::new(&[1.0, 0.0]),
            2,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("query explicit exact tier");
    assert_eq!(
        explicit.diagnostics.plan[0].scan_reason,
        Some(ScanReason::ExplicitTier(ExplicitScanTier::Exact))
    );
    store.close().expect("close store");
}

#[test]
fn eligible_unpublished_graph_reports_graph_pending() {
    let directory = tempdir().expect("store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    let documents = (0_u128..30_000)
        .map(|id| {
            IngestDocument::new(
                DocumentVersion::new(DocId::new(id + 1), Revision::new(1)),
                vec![1.0],
            )
        })
        .collect::<Vec<_>>();
    store
        .ingest(IngestBatch::new(documents))
        .expect("ingest graph-eligible segment");
    store.seal().expect("seal graph-eligible segment");

    let outcome = store
        .search(
            SearchRequest::new(&[1.0]),
            1,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("query graph-pending segment");
    assert_eq!(
        outcome.diagnostics.plan[0].scan_reason,
        Some(ScanReason::GraphPending)
    );
    let stats = store.stats().expect("stats after graph-pending scan");
    assert_eq!(
        stats.scans_by_reason[ScanReasonCounter::GraphPending.index()],
        1
    );
    store.close().expect("close store");
}

#[test]
fn hybrid_without_a_tier_records_its_exact_resolution() {
    let (_directory, store) = populated_store();
    let outcome = store
        .search_hybrid(
            SearchRequest::new(&[1.0, 0.0]),
            &TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]),
            &HybridQuery::new(2),
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("query default hybrid tier");

    assert_eq!(
        outcome.diagnostics.hybrid_tier_resolution,
        Some(HybridTierResolution::Exact)
    );
    assert_eq!(
        outcome.diagnostics.plan[0].scan_reason,
        Some(ScanReason::ActiveSegment)
    );
    store.close().expect("close store");
}

#[test]
fn scan_reason_accounting_moves_by_executed_segment() {
    let (_directory, store) = populated_store();
    let before = store.stats().expect("stats before scans");

    store
        .search(
            SearchRequest::new(&[1.0, 0.0]),
            2,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("query active segment");
    let after_active = store.stats().expect("stats after active scan");
    assert_eq!(
        after_active.scans_by_reason[ScanReasonCounter::ActiveSegment.index()],
        before.scans_by_reason[ScanReasonCounter::ActiveSegment.index()] + 1
    );
    assert_eq!(after_active.graph_segments_served, 0);

    store.seal().expect("seal below-threshold segment");
    store
        .search(
            SearchRequest::new(&[1.0, 0.0]),
            2,
            SearchOptions::default().with_tier(SearchTier::Scan),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("query explicit scan tier");
    let after_explicit = store.stats().expect("stats after explicit scan");
    assert_eq!(
        after_explicit.scans_by_reason[ScanReasonCounter::ExplicitScan.index()],
        before.scans_by_reason[ScanReasonCounter::ExplicitScan.index()] + 1
    );
    store.close().expect("close store");
}

#[test]
fn a_short_result_list_is_always_explained() {
    let (_directory, store) = populated_store();
    let outcome = store
        .search(
            SearchRequest::new(&[1.0, 0.0]),
            40,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("query store");

    assert_eq!(outcome.diagnostics.returned, outcome.candidates.len());
    assert_eq!(outcome.diagnostics.requested_k, 40);
    assert_eq!(outcome.diagnostics.snapshot_generation, outcome.generation);
    assert_eq!(outcome.diagnostics.indexed_through_seq.get(), 2);
    assert!(
        outcome.diagnostics.returned == outcome.diagnostics.requested_k
            || 2 < outcome.diagnostics.requested_k
            || outcome.diagnostics.budget_exhausted
    );
    store.close().expect("close store");
}

#[test]
fn observed_qos_appears_in_every_diagnostics_value() {
    let (_directory, store) = populated_store();
    let vector = store
        .search(
            SearchRequest::new(&[1.0, 0.0]),
            2,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("vector query");
    let predicate = Predicate::Range(RangePredicate {
        column: TIMESTAMP_COLUMN,
        lower: Some(RangeBound::inclusive(PredicateValue::I64(0))),
        upper: Some(RangeBound::inclusive(PredicateValue::I64(0))),
    });
    let filtered = store
        .search_filtered(
            SearchRequest::new(&[1.0, 0.0]),
            &predicate,
            2,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("filtered query");

    assert_ne!(
        vector.diagnostics.observed_qos.class,
        QueryQosClass::Unavailable
    );
    assert_ne!(
        filtered.diagnostics.observed_qos.class,
        QueryQosClass::Unavailable
    );
    store.close().expect("close store");
}

#[test]
fn diagnostics_v1_shape_is_byte_identical() {
    let (_directory, store) = populated_store();
    let outcome = store
        .search_hybrid(
            SearchRequest::new(&[1.0, 0.0]),
            &TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]),
            &HybridQuery::new(2),
            SearchOptions::default().with_tier(SearchTier::Graph(GraphSearchOptions::default())),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("query hybrid store");
    assert_eq!(
        outcome.diagnostics.plan.len(),
        1,
        "hybrid diagnostics must report the active-segment vector plan"
    );
    assert_eq!(
        diagnostics_v1_text(&outcome.diagnostics),
        include_str!("fixtures/diagnostics_v1.txt")
    );
    store.close().expect("close store");
}
