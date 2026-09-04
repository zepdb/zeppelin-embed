#![allow(clippy::expect_used)]

use tempfile::tempdir;
use zeppelin_embed_text::{
    ChunkPolicy, IngestOptions, Legs, MaintenanceBudget, MaintenanceStatus, QueryOptions,
    SearchTier, SegmentTier, TextDocument, TextStore,
};

#[test]
fn default_ingest_seals_at_4096_rows() {
    assert_eq!(IngestOptions::default().seal_every, 4_096);
}

mod common;

#[test]
fn ingest_text_then_query_text_returns_the_ingested_text_with_the_declared_epoch() {
    let directory = tempdir().expect("store directory");
    let bundle_path = directory.path().join("fixture.zem");
    let declared_epoch = common::write_symmetric_fixture_bundle(&bundle_path);
    let store = TextStore::open(
        directory.path().join("store"),
        &bundle_path,
        Default::default(),
    )
    .expect("open text store");

    store
        .ingest_text(
            &[TextDocument::new(7, 1, "the bronze zeppelin")],
            IngestOptions::default(),
        )
        .expect("ingest text");
    let hits = store
        .query_text(
            "bronze zeppelin",
            QueryOptions::new(1).with_legs(Legs::Dense),
        )
        .expect("query text");

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].doc_id, 7);
    assert_eq!(hits[0].revision, 1);
    assert_eq!(hits[0].chunk, 0);
    assert_eq!(hits[0].text, "the bronze zeppelin");
    assert_eq!(hits[0].epoch, declared_epoch);
    for legs in [Legs::Lexical, Legs::Hybrid] {
        let hits = store
            .query_text("bronze zeppelin", QueryOptions::new(1).with_legs(legs))
            .expect("query text leg");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].doc_id, 7);
        assert_eq!(hits[0].text, "the bronze zeppelin");
        assert_eq!(hits[0].epoch, declared_epoch);
    }
}

#[test]
fn deleting_a_caller_id_deletes_every_chunk_row() {
    let directory = tempdir().expect("store directory");
    let bundle_path = directory.path().join("fixture.zem");
    common::write_symmetric_fixture_bundle(&bundle_path);
    let store = TextStore::open(
        directory.path().join("store"),
        bundle_path,
        Default::default(),
    )
    .expect("open text store");
    store
        .ingest_text(
            &[TextDocument::new(41, 1, "bronze zeppelin bronze zeppelin")],
            IngestOptions {
                embed_batch_size: 1,
                seal_every: 8,
                chunk_policy: ChunkPolicy::Tokens { max: 2, overlap: 0 },
                ..Default::default()
            },
        )
        .expect("ingest chunks");
    let before = store
        .query_text(
            "bronze zeppelin",
            QueryOptions::new(8).with_legs(Legs::Dense),
        )
        .expect("query before delete");
    assert_eq!(before.len(), 2);

    store.delete_text(&[41]).expect("delete every chunk");

    let after = store
        .query_text(
            "bronze zeppelin",
            QueryOptions::new(8).with_legs(Legs::Dense),
        )
        .expect("query after delete");
    assert!(after.is_empty());
}

#[test]
fn query_options_carry_an_explicit_search_tier_through_dense_and_hybrid_legs() {
    let directory = tempdir().expect("store directory");
    let bundle_path = directory.path().join("fixture.zem");
    common::write_symmetric_fixture_bundle(&bundle_path);
    let store = TextStore::open(
        directory.path().join("store"),
        bundle_path,
        Default::default(),
    )
    .expect("open text store");
    let documents = (0_u128..8)
        .map(|id| TextDocument::new(id + 1, 1, format!("bronze zeppelin document {id}")))
        .collect::<Vec<_>>();
    store
        .ingest_text(
            &documents,
            IngestOptions {
                seal_every: 4,
                maintenance_wall_time: std::time::Duration::ZERO,
                ..Default::default()
            },
        )
        .expect("ingest text");

    // Dense and lexical honour every explicit tier and must agree, because
    // the tier changes how a score is obtained, never which rows match.
    for legs in [Legs::Dense, Legs::Lexical] {
        let mut expected = store
            .query_text(
                "bronze zeppelin",
                QueryOptions::new(8)
                    .with_legs(legs)
                    .with_tier(SearchTier::Auto),
            )
            .expect("auto query")
            .into_iter()
            .map(|hit| hit.doc_id)
            .collect::<Vec<_>>();
        expected.sort_unstable();
        for tier in [SearchTier::Exact, SearchTier::Scan] {
            let mut actual = store
                .query_text(
                    "bronze zeppelin",
                    QueryOptions::new(8).with_legs(legs).with_tier(tier),
                )
                .expect("explicit-tier query")
                .into_iter()
                .map(|hit| hit.doc_id)
                .collect::<Vec<_>>();
            actual.sort_unstable();
            assert_eq!(actual, expected, "{legs:?} must preserve the result ids");
        }
    }

    // Hybrid is different, and deliberately so. Fusion may only fuse vector
    // scores that were exactly rescored, so the store selects
    // `SearchTier::Exact` for itself when no graph exists and the caller
    // states no preference. In this scan-only store, leaving the tier unset
    // must therefore equal asking for Exact. An explicit estimating tier
    // must fail loudly rather than silently fusing estimates.
    let mut unset = store
        .query_text(
            "bronze zeppelin",
            QueryOptions::new(8).with_legs(Legs::Hybrid),
        )
        .expect("hybrid with no tier preference")
        .into_iter()
        .map(|hit| hit.doc_id)
        .collect::<Vec<_>>();
    unset.sort_unstable();
    let mut exact = store
        .query_text(
            "bronze zeppelin",
            QueryOptions::new(8)
                .with_legs(Legs::Hybrid)
                .with_tier(SearchTier::Exact),
        )
        .expect("hybrid at explicit Exact")
        .into_iter()
        .map(|hit| hit.doc_id)
        .collect::<Vec<_>>();
    exact.sort_unstable();
    assert_eq!(
        unset, exact,
        "an unset tier must resolve to Exact for the hybrid leg"
    );

    for tier in [SearchTier::Auto, SearchTier::Scan] {
        let error = store
            .query_text(
                "bronze zeppelin",
                QueryOptions::new(8).with_legs(Legs::Hybrid).with_tier(tier),
            )
            .expect_err("hybrid must refuse an estimating tier");
        assert!(
            error.to_string().contains("not exactly rescored"),
            "{tier:?} must fail loudly on unrescored fusion input, got {error}"
        );
    }
}

#[test]
fn health_reports_sealed_scan_segments_and_maintain_reports_complete_below_the_graph_threshold() {
    let directory = tempdir().expect("store directory");
    let bundle_path = directory.path().join("fixture.zem");
    common::write_symmetric_fixture_bundle(&bundle_path);
    let store = TextStore::open(
        directory.path().join("store"),
        bundle_path,
        Default::default(),
    )
    .expect("open text store");
    let documents = (0_u128..8)
        .map(|id| TextDocument::new(id + 1, 1, format!("bronze zeppelin document {id}")))
        .collect::<Vec<_>>();
    store
        .ingest_text(
            &documents,
            IngestOptions {
                seal_every: 4,
                maintenance_wall_time: std::time::Duration::ZERO,
                ..Default::default()
            },
        )
        .expect("ingest text");

    let health = store.health().expect("read health");
    assert!(!health.segments.is_empty());
    assert!(
        health
            .segments
            .iter()
            .all(|segment| segment.tier == SegmentTier::SealedScan)
    );
    let report = store
        .maintain(MaintenanceBudget {
            wall_time: std::time::Duration::from_millis(25),
            bytes: 8 * 1024 * 1024,
        })
        .expect("maintain below graph threshold");
    assert!(matches!(report.status, MaintenanceStatus::Complete));
    assert_eq!(report.graphs_built, 0);
    assert!(report.promotion_deferrals.is_empty());
}

#[test]
#[ignore = "requires an unsandboxed MLX runtime"]
fn default_text_store_over_30000_documents_maintains_and_serves_a_graph() {
    let directory = tempdir().expect("store directory");
    let bundle_path = directory.path().join("fixture.zem");
    common::write_symmetric_fixture_bundle(&bundle_path);
    let store = TextStore::open(
        directory.path().join("store"),
        bundle_path,
        Default::default(),
    )
    .expect("open text store");
    let documents = (0_u128..30_001)
        .map(|id| TextDocument::new(id + 1, 1, format!("bronze zeppelin document {id}")))
        .collect::<Vec<_>>();

    store
        .ingest_text(&documents, IngestOptions::default())
        .expect("default ingest");
    let final_maintenance = store
        .maintain_to_completion(MaintenanceBudget {
            wall_time: IngestOptions::default().maintenance_wall_time,
            bytes: IngestOptions::default().maintenance_bytes,
        })
        .expect("explicit completion is idempotent");
    assert!(matches!(
        final_maintenance.status,
        MaintenanceStatus::Complete
    ));
    let before = store.health().expect("health before query");
    assert!(
        before
            .segments
            .iter()
            .any(|segment| segment.tier == SegmentTier::SealedGraph),
        "default ingest must publish a graph once scan segments cross the threshold"
    );
    assert!(before.graph_coverage > 0.0);

    store
        .query_text("bronze zeppelin", QueryOptions::default())
        .expect("default hybrid query");
    let after = store.health().expect("health after query");
    assert!(
        after.stats.graph_segments_served > before.stats.graph_segments_served,
        "the default hybrid query must serve the published graph"
    );
}
