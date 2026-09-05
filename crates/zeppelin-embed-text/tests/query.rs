#![allow(clippy::expect_used)]

use tempfile::tempdir;
use zeppelin_embed_text::bundle::Bundle;
use zeppelin_embed_text::{IngestOptions, Legs, QueryOptions, TextDocument, TextStore};

mod common;

#[test]
fn astra_07_result_materialization_uses_one_admission() {
    let directory = tempdir().expect("text hydration fixture");
    let path = directory.path().join("fixture.zem");
    common::write_symmetric_fixture_bundle(&path);
    let store =
        TextStore::open(directory.path().join("store"), &path, Default::default()).expect("open");
    let documents: Vec<_> = (1..=100)
        .map(|id| TextDocument::new(id, 7, "bronze zeppelin"))
        .collect();
    store
        .ingest_text(&documents, IngestOptions::default())
        .expect("ingest");
    for k in [1, 10, 100] {
        for legs in [Legs::Dense, Legs::Lexical, Legs::Hybrid] {
            let outcome = store
                .query_text_with_diagnostics(
                    "bronze zeppelin",
                    QueryOptions::new(k).with_legs(legs),
                )
                .expect("text query");
            let work = outcome
                .diagnostics
                .expect("core query")
                .materialization
                .expect("text must be materialized within retrieval admission");
            assert_eq!(work.row_lookups, k as u64);
            assert_eq!(work.text_copies, k as u64);
            assert_eq!(work.text_bytes, (k * "bronze zeppelin".len()) as u64);
            assert_eq!(outcome.hits.len(), k);
            assert!(
                outcome
                    .hits
                    .iter()
                    .all(|hit| hit.revision == 7 && hit.text == "bronze zeppelin")
            );
        }
    }
    store.close().expect("close");
}

#[test]
fn astra_06_text_query_exposes_explicit_rescore_and_preserves_tier_controls() {
    use zeppelin_embed::lifecycle::{ScanRescoreOptions, SearchTier};
    let directory = tempdir().expect("tempdir");
    let path = directory.path().join("fixture.zem");
    common::write_symmetric_fixture_bundle(&path);
    let store = TextStore::open(directory.path().join("store"), &path, Default::default())
        .expect("open native fixture");
    store
        .ingest_text(
            &[TextDocument::new(1, 1, "bronze zeppelin")],
            IngestOptions::default(),
        )
        .expect("ingest");
    let config = ScanRescoreOptions::new(2, 128).expect("controls");
    for legs in [Legs::Dense, Legs::Hybrid, Legs::Lexical] {
        let base = QueryOptions::new(1).with_legs(legs);
        let report = store
            .query_text_with_diagnostics("bronze zeppelin", base.with_scan_rescore(config))
            .expect("rescore text query");
        assert_eq!(report.embedding_calls, usize::from(legs != Legs::Lexical));
        let diagnostics = report.diagnostics.expect("admitted");
        if legs == Legs::Lexical {
            assert!(diagnostics.scan_rescore.is_none());
        } else {
            let counters = diagnostics
                .scan_rescore
                .expect("text option must reach vector producer");
            assert_eq!(counters.candidates_rescored, 1);
            assert_eq!(counters.rescore_bytes, 8);
            assert!(diagnostics.exact_rescore);
            assert!(
                !diagnostics.approximate,
                "all eligible rows rescored in tiny fixture"
            );
        }
        let control = store
            .query_text("bronze zeppelin", base.with_tier(SearchTier::Exact))
            .expect("exact");
        assert_eq!(report.hits, control);
        for tier in [
            None,
            Some(SearchTier::Exact),
            Some(SearchTier::Auto),
            Some(SearchTier::Scan),
        ] {
            let reset = store.query_text_with_diagnostics(
                "bronze zeppelin",
                base.with_scan_rescore(config).with_optional_tier(tier),
            );
            let ordinary = store.query_text("bronze zeppelin", base.with_optional_tier(tier));
            if legs == Legs::Hybrid && matches!(tier, Some(SearchTier::Auto | SearchTier::Scan)) {
                // Explicit ordinary coarse tiers retain their existing refusal
                // to supply estimated scores to exact-score fusion.
                assert!(matches!(
                    reset,
                    Err(zeppelin_embed_text::TextError::Hybrid(
                        zeppelin_embed::fusion::FusionError::EstimatedVectorScore { rank: 0 }
                    ))
                ));
                assert!(matches!(
                    ordinary,
                    Err(zeppelin_embed_text::TextError::Hybrid(
                        zeppelin_embed::fusion::FusionError::EstimatedVectorScore { rank: 0 }
                    ))
                ));
                continue;
            }
            let reset = reset.expect("ordinary tier");
            let ordinary = ordinary.expect("control");
            assert_eq!(reset.hits, ordinary);
            assert!(reset.diagnostics.expect("admitted").scan_rescore.is_none());
        }
    }
    store.close().expect("close");
}

#[test]
fn the_query_prefix_is_applied_from_the_bundle_and_never_by_the_caller() {
    let directory = tempdir().expect("tempdir");
    let path = directory.path().join("pair.zem");
    common::write_fixture_bundle(
        &path,
        &[
            common::FixtureTower::document("document-fixture"),
            common::FixtureTower::query("query-fixture"),
        ],
        b"aligned",
    );
    let bundle = Bundle::open(&path).expect("open fixture");

    assert_eq!(
        bundle.query_input("bronze zeppelin"),
        "query: bronze zeppelin"
    );
}

#[test]
fn a_lexical_query_does_not_evaluate_the_query_tower() {
    let directory = tempdir().expect("tempdir");
    let path = directory.path().join("pair.zem");
    let mut unusable_query = common::FixtureTower::query("unused-query");
    unusable_query.word_vectors = [[0.0, 0.0]; 7];
    common::write_fixture_bundle(
        &path,
        &[common::FixtureTower::document("document"), unusable_query],
        b"aligned",
    );
    let store = TextStore::open(directory.path().join("store"), path, Default::default())
        .expect("open store");
    store
        .ingest_text(
            &[TextDocument::new(1, 1, "bronze zeppelin")],
            IngestOptions::default(),
        )
        .expect("ingest text");

    let hits = store
        .query_text(
            "bronze zeppelin",
            QueryOptions::new(1).with_legs(Legs::Lexical),
        )
        .expect("lexical query must not evaluate the unusable query tower");

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].doc_id, 1);
}

#[test]
fn astra_00_detailed_text_query_preserves_hits_and_reports_its_boundaries() {
    let directory = tempdir().expect("tempdir");
    let path = directory.path().join("fixture.zem");
    common::write_symmetric_fixture_bundle(&path);
    let store = TextStore::open(directory.path().join("store"), &path, Default::default())
        .expect("open tiny native fixture");
    store
        .ingest_text(
            &[TextDocument::new(1, 1, "bronze zeppelin")],
            IngestOptions::default(),
        )
        .expect("ingest");
    for legs in [Legs::Dense, Legs::Lexical, Legs::Hybrid] {
        let options = QueryOptions::new(1).with_legs(legs);
        let control = store
            .query_text("bronze zeppelin", options)
            .expect("hit-only control");
        let report = store
            .query_text_with_diagnostics("bronze zeppelin", options)
            .expect("detailed query");
        assert_eq!(report.hits, control);
        let hit = report.hits.first().expect("one hit");
        assert_eq!(hit.doc_id, 1);
        assert_eq!(hit.revision, 1);
        assert_eq!(hit.text, "bronze zeppelin");
        assert_eq!(report.embedding_calls, usize::from(legs != Legs::Lexical));
        assert_eq!(report.backend.is_some(), legs != Legs::Lexical);
        if let Some(backend) = report.backend {
            assert_eq!(backend.runtime.name, "mlx-c");
            assert_eq!(backend.observed_compute_units, None);
            assert!(report.query_tokens > 0);
        } else {
            assert_eq!(report.query_tokens, 0);
        }
        assert_eq!(
            report.timings.is_some(),
            zeppelin_embed::diag::QUERY_TIMING_ENABLED
        );
        let core = report.diagnostics.expect("admitted query");
        assert_eq!(core.returned, 1);
        assert_eq!(
            core.timings.is_some(),
            zeppelin_embed::diag::QUERY_TIMING_ENABLED
        );
        if let Some(timings) = report.timings {
            // Sequential outer spans only: core vector/lexical spans overlap
            // inside retrieval and deliberately are not added here.
            assert!(timings.end_to_end >= timings.retrieval + timings.materialization);
            if legs == Legs::Lexical {
                assert_eq!(timings.embedding_queue, std::time::Duration::ZERO);
                assert_eq!(timings.embedding_evaluation, std::time::Duration::ZERO);
            }
        }
    }
    let empty = store
        .query_text_with_diagnostics("ignored", QueryOptions::new(0))
        .expect("zero-k fast path");
    assert!(empty.hits.is_empty());
    assert!(empty.diagnostics.is_none());
    assert_eq!(empty.embedding_calls, 0);
    store.close().expect("close");
}

#[test]
fn astra_18_expired_text_entry_never_returns_partial_hits() {
    use std::time::Duration;
    use zeppelin_embed::lifecycle::{Deadline, QueryControl, QueryError};
    use zeppelin_embed_text::TextError;
    let directory = tempdir().expect("text deadline fixture");
    let path = directory.path().join("fixture.zem");
    common::write_symmetric_fixture_bundle(&path);
    let store =
        TextStore::open(directory.path().join("store"), &path, Default::default()).expect("open");
    for legs in [Legs::Lexical, Legs::Dense, Legs::Hybrid] {
        for k in [0, 1] {
            let expired =
                QueryControl::Deadline(Deadline::after(Duration::ZERO).expect("deadline"));
            assert!(
                matches!(
                    store.query_text_controlled(
                        "bronze",
                        QueryOptions::new(k).with_legs(legs),
                        expired
                    ),
                    Err(TextError::Query(QueryError::Timeout { partial: false }))
                ),
                "an already expired text call must stop before even zero-k work"
            );
        }
    }
    store.close().expect("close");
}
