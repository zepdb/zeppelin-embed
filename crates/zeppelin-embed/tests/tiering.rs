#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use tempfile::{TempDir, tempdir};
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::{CancelToken, QueryControl, SearchOptions};
use zeppelin_embed::lifecycle::{InMemorySegment, InMemorySegmentFactors, OpenOptions, Store};
use zeppelin_embed::meta::{AliveSet, ColumnStore, ColumnStoreBuilder, Schema};
use zeppelin_embed::quant::{Bit4Factors, quantize_bit4};
use zeppelin_embed::segment::SegmentId;
use zeppelin_embed::segment::layout::RegionKind;
use zeppelin_embed::tier::maintain::UncarriedRegionKind;
use zeppelin_embed::tier::{MaintenanceBudget, MaintenanceStatus, PROVISIONAL_TIER_THRESHOLDS};

const DIMS: usize = 1;
const MAINTENANCE_TEST_BUDGET: Duration = Duration::from_secs(600);

struct SealedFixture {
    _directory: TempDir,
    store: Store,
}

fn columns(rows: usize) -> ColumnStore {
    let schema = Schema::new(Vec::new()).expect("timestamp-only schema");
    let mut builder = ColumnStoreBuilder::new(schema);
    for row in 0..rows {
        builder
            .push_row(row as i64, &[])
            .expect("fixture metadata row");
    }
    builder.finish().expect("fixture columns")
}

fn sealed_fixture(rows: usize) -> SealedFixture {
    let directory = tempdir().expect("tiering store directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open tiering store");
    let vectors = (0..rows).map(|row| row as f32).collect::<Vec<_>>();
    let mut codes = vec![0_u8; rows];
    let mut factors = Vec::<Bit4Factors>::with_capacity(rows);
    for (vector, encoded) in vectors.chunks_exact(DIMS).zip(codes.chunks_exact_mut(1)) {
        factors.push(quantize_bit4(vector, encoded).expect("finite fixture vector"));
    }
    let columns = columns(rows);
    let alive = AliveSet::new(rows as u32);
    let prepared = store
        .prepare_segment(InMemorySegment {
            id: SegmentId::new(0x0001_9000_2000, [0x20; 10]),
            scheme: 4,
            dims: DIMS as u32,
            codes,
            factors: InMemorySegmentFactors::Bit4(factors),
            rescore: vectors,
            columns: &columns,
            alive: &alive,
        })
        .expect("prepare sealed tiering segment");
    store
        .seal_snapshot(prepared)
        .expect("publish sealed tiering segment");
    SealedFixture {
        _directory: directory,
        store,
    }
}

fn has_graph(store: &Store) -> bool {
    let snapshot = store.snapshot().expect("tiering snapshot");
    snapshot.segments().iter().any(|segment| {
        segment
            .directory()
            .iter()
            .any(|entry| entry.kind == RegionKind::GraphNodeBlocks.id())
    })
}

#[test]
fn maintain_defers_promotion_of_segments_whose_regions_it_cannot_carry() {
    const METADATA: &[u8] = b"tier-maintenance-metadata";

    let directory = tempdir().expect("guarded tiering directory");
    let store =
        Store::open(directory.path(), OpenOptions::default()).expect("open guarded tiering store");
    let rows = PROVISIONAL_TIER_THRESHOLDS.graph_min_rows as usize;
    let documents = (0..rows)
        .map(|row| {
            let document = IngestDocument::new(
                DocumentVersion::new(DocId::new(row as u128 + 1), Revision::new(1)),
                vec![row as f32],
            );
            if row == 0 {
                document
                    .with_text("guarded lexical row")
                    .with_metadata(METADATA.to_vec())
            } else {
                document
            }
        })
        .collect::<Vec<_>>();
    store
        .ingest(IngestBatch::new(documents))
        .expect("ingest guarded fixture");
    store.seal().expect("seal guarded fixture");

    let report = store.maintain(MaintenanceBudget {
        wall_time: MAINTENANCE_TEST_BUDGET,
        bytes: u64::MAX,
    });

    let snapshot = store.snapshot().expect("guarded snapshot");
    let segment = snapshot.segments().first().expect("guarded sealed segment");
    let postings = segment.postings().expect("read postings region");
    assert!(
        postings.is_some(),
        "maintenance promotion dropped the postings region"
    );
    let metadata = segment.stored_metadata().expect("read metadata region");
    assert!(
        metadata.is_some(),
        "maintenance promotion dropped the stored-metadata region"
    );
    assert_eq!(
        metadata.and_then(|rows| rows.row(0)),
        Some(METADATA),
        "stored metadata changed while promotion was deferred"
    );
    assert!(matches!(report.status, MaintenanceStatus::Complete));
    assert_eq!(report.promotion_deferrals.len(), 1);
    let deferral = report
        .promotion_deferrals
        .first()
        .expect("typed promotion deferral");
    assert_eq!(deferral.segment_id, segment.meta().id);
    assert_eq!(deferral.uncarried_regions.len(), 2);
    assert!(
        deferral
            .uncarried_regions
            .contains(&UncarriedRegionKind::Known(RegionKind::Postings))
    );
    assert!(
        deferral
            .uncarried_regions
            .contains(&UncarriedRegionKind::Known(RegionKind::StoredMetadata))
    );
    assert_eq!(report.graphs_built, 0);
    assert!(!has_graph(&store));
}

#[test]
fn maintain_still_promotes_plain_vector_only_segments() {
    let fixture = sealed_fixture(PROVISIONAL_TIER_THRESHOLDS.graph_min_rows as usize);

    let report = fixture.store.maintain(MaintenanceBudget {
        wall_time: MAINTENANCE_TEST_BUDGET,
        bytes: u64::MAX,
    });

    assert!(matches!(report.status, MaintenanceStatus::Complete));
    assert!(report.promotion_deferrals.is_empty());
    assert_eq!(report.graphs_built, 1);
    assert!(has_graph(&fixture.store));
}

#[test]
fn maintain_builds_a_graph_for_a_sealed_segment_above_the_threshold() {
    let fixture = sealed_fixture(PROVISIONAL_TIER_THRESHOLDS.graph_min_rows as usize);

    let report = fixture.store.maintain(MaintenanceBudget {
        wall_time: MAINTENANCE_TEST_BUDGET,
        bytes: u64::MAX,
    });

    assert_eq!(report.graphs_built, 1);
    assert!(has_graph(&fixture.store));
}

#[test]
fn maintain_is_idempotent_and_a_second_call_does_no_work() {
    let fixture = sealed_fixture(PROVISIONAL_TIER_THRESHOLDS.graph_min_rows as usize);
    let budget = MaintenanceBudget {
        wall_time: MAINTENANCE_TEST_BUDGET,
        bytes: u64::MAX,
    };

    let first = fixture.store.maintain(budget);
    let second = fixture.store.maintain(budget);

    assert_eq!(first.graphs_built, 1);
    assert_eq!(second.graphs_built, 0);
}

#[test]
fn maintain_respects_its_budget_and_resumes_interrupted_work() {
    let fixture = sealed_fixture(PROVISIONAL_TIER_THRESHOLDS.graph_min_rows as usize);
    let byte_budget = 256_u64 * 64;

    let interrupted = fixture.store.maintain(MaintenanceBudget {
        wall_time: MAINTENANCE_TEST_BUDGET,
        bytes: byte_budget,
    });

    assert_eq!(interrupted.graphs_built, 0);
    assert!(interrupted.bytes_consumed > 0);
    assert!(interrupted.bytes_consumed <= byte_budget + byte_budget / 10);
    assert!(!has_graph(&fixture.store));

    let resumed = fixture.store.maintain(MaintenanceBudget {
        wall_time: MAINTENANCE_TEST_BUDGET,
        bytes: u64::MAX,
    });

    assert_eq!(resumed.checkpoints_resumed, 1);
    assert_eq!(resumed.graphs_built, 1);
    assert!(has_graph(&fixture.store));
}

#[test]
fn store_search_uses_the_graph_automatically_after_maintain() {
    let fixture = sealed_fixture(PROVISIONAL_TIER_THRESHOLDS.graph_min_rows as usize);
    let report = fixture.store.maintain(MaintenanceBudget {
        wall_time: MAINTENANCE_TEST_BUDGET,
        bytes: u64::MAX,
    });
    assert_eq!(report.graphs_built, 1);

    let outcome = fixture
        .store
        .search(
            SearchRequest::new(&[0.0]),
            1,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("automatic tier search");

    assert_eq!(outcome.graph_stats.segments_traversed, 1);
    assert!(outcome.graph_stats.candidates_rescored > 0);
}

#[test]
fn store_search_scans_a_segment_that_has_not_been_transitioned_yet() {
    let fixture = sealed_fixture(PROVISIONAL_TIER_THRESHOLDS.graph_min_rows as usize);

    let outcome = fixture
        .store
        .search(
            SearchRequest::new(&[0.0]),
            1,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("automatic pre-transition search");

    assert_eq!(outcome.graph_stats.segments_traversed, 0);
    assert_eq!(outcome.candidates.len(), 1);
}

#[test]
fn queries_never_error_or_miss_acked_docs_during_maintain() {
    let directory = tempdir().expect("concurrent tiering directory");
    let store = Arc::new(
        Store::open(directory.path(), OpenOptions::default()).expect("open concurrent store"),
    );
    let rows = PROVISIONAL_TIER_THRESHOLDS.graph_min_rows as usize;
    let documents = (0..rows)
        .map(|row| {
            let amplitude = row as f32 + 1.0;
            IngestDocument::new(
                DocumentVersion::new(DocId::new(row as u128 + 1), Revision::new(1)),
                vec![amplitude, -amplitude],
            )
        })
        .collect::<Vec<_>>();
    store
        .ingest(IngestBatch::new(documents))
        .expect("acknowledge concurrent fixture");
    store.seal().expect("seal concurrent fixture");

    let started = Arc::new(Barrier::new(2));
    let maintenance_done = Arc::new(AtomicBool::new(false));
    let query_store = Arc::clone(&store);
    let query_started = Arc::clone(&started);
    let query_done = Arc::clone(&maintenance_done);
    let query_thread = std::thread::spawn(move || -> Result<(u64, u64), String> {
        query_started.wait();
        let mut seed = 0x20_00c0_ffee_u64;
        let mut queries = 0_u64;
        let mut graph_queries = 0_u64;
        let mut after_publish = 0_u8;
        while !query_done.load(Ordering::Acquire) || after_publish < 16 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let offset = usize::try_from(seed % 128).map_err(|_| "seed row overflow".to_owned())?;
            let amplitude = rows as f32 + offset as f32;
            let query = [amplitude, -amplitude];
            let outcome = query_store
                .search(
                    SearchRequest::new(&query),
                    1,
                    SearchOptions::default(),
                    QueryControl::Cancel(CancelToken::new()),
                )
                .map_err(|error| format!("query {queries} errored: {error}"))?;
            let expected = DocumentVersion::new(DocId::new(rows as u128), Revision::new(1));
            let actual = outcome
                .candidates
                .first()
                .and_then(|candidate| candidate.document());
            if actual != Some(expected) {
                return Err(format!(
                    "query {queries} missed acknowledged {expected:?}, got {actual:?}"
                ));
            }
            queries = queries.saturating_add(1);
            if outcome.graph_stats.segments_traversed > 0 {
                graph_queries = graph_queries.saturating_add(1);
            }
            if query_done.load(Ordering::Acquire) {
                after_publish = after_publish.saturating_add(1);
            }
        }
        Ok((queries, graph_queries))
    });

    started.wait();
    let report = store.maintain(MaintenanceBudget {
        wall_time: MAINTENANCE_TEST_BUDGET,
        bytes: u64::MAX,
    });
    maintenance_done.store(true, Ordering::Release);
    let (queries, graph_queries) = query_thread
        .join()
        .expect("query workload thread")
        .expect("query workload stayed correct");

    assert_eq!(report.graphs_built, 1);
    assert!(queries > 16);
    assert!(graph_queries > 0);
}
