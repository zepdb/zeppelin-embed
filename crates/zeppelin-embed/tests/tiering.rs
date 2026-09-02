#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use tempfile::{TempDir, tempdir};
use xxhash_rust::xxh3::xxh3_64;
use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, Normalization, StoreEpoch,
};
use zeppelin_embed::format::frame::{FILE_HEADER_LEN, FILE_TRAILER_LEN};
use zeppelin_embed::fts::index::DEFAULT_FIELD;
use zeppelin_embed::fts::search::TermQuery;
use zeppelin_embed::fts::tokenizer::TokenizerConfig;
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::{CancelToken, QueryControl, SearchOptions};
use zeppelin_embed::lifecycle::{InMemorySegment, InMemorySegmentFactors, OpenOptions, Store};
use zeppelin_embed::meta::{AliveSet, ColumnStore, ColumnStoreBuilder, Schema};
use zeppelin_embed::quant::{Bit4Factors, quantize_bit4};
use zeppelin_embed::segment::SegmentId;
use zeppelin_embed::segment::layout::{REGION_ENTRY_LEN, RegionKind, SEGMENT_PREFIX_LEN};
use zeppelin_embed::tier::maintain::UncarriedRegionKind;
use zeppelin_embed::tier::{
    MaintenanceBudget, MaintenanceReport, MaintenanceStatus, TierThresholds,
};

const DIMS: usize = 128;
const GRAPH_TEST_ROWS: usize = 128;
const MAINTENANCE_TEST_BUDGET: Duration = Duration::from_secs(600);
const UNKNOWN_REGION_KIND: u16 = 65_000;
const DIRECTORY_START: usize = FILE_HEADER_LEN + SEGMENT_PREFIX_LEN;

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
    let store = open_sift_store(directory.path(), "open tiering store");
    let vectors = (0..rows)
        .flat_map(|row| fixture_vector(row as f32))
        .collect::<Vec<_>>();
    let row_bytes = DIMS.div_ceil(2);
    let mut codes = vec![0_u8; rows * row_bytes];
    let mut factors = Vec::<Bit4Factors>::with_capacity(rows);
    for (vector, encoded) in vectors
        .chunks_exact(DIMS)
        .zip(codes.chunks_exact_mut(row_bytes))
    {
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

fn sift_epoch() -> StoreEpoch {
    let document = EmbeddingTower {
        model_id: "tiering-sift-fixture".to_owned(),
        model_version: "1".to_owned(),
        weights_digest: vec![0x16],
        dims: DIMS as u32,
        normalization: Normalization::None,
        prompt_prefix: String::new(),
        max_tokens: 512,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    StoreEpoch {
        embedding: EmbeddingEpoch {
            query: document.clone(),
            document,
            alignment_digest: Vec::new(),
        },
        tokenizer: TokenizerConfig::text_default().epoch(),
    }
}

fn open_sift_store(path: &std::path::Path, context: &str) -> Store {
    Store::open(path, OpenOptions::default().with_epoch(sift_epoch()))
        .unwrap_or_else(|error| panic!("{context}: {error}"))
}

fn fixture_vector(amplitude: f32) -> Vec<f32> {
    (0..DIMS)
        .map(|dimension| {
            if dimension.is_multiple_of(2) {
                amplitude
            } else {
                -amplitude
            }
        })
        .collect()
}

fn maintain_test(store: &Store, budget: MaintenanceBudget) -> MaintenanceReport {
    store.maintain_with_test_thresholds(
        budget,
        TierThresholds {
            graph_min_rows: GRAPH_TEST_ROWS as u32,
        },
    )
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

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(
        bytes[offset..offset + 2]
            .try_into()
            .expect("u16 fixture bytes"),
    )
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("u32 fixture bytes"),
    )
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("u64 fixture bytes"),
    )
}

fn region_entry_offset(bytes: &[u8], kind: u16) -> usize {
    let region_count = usize::from(read_u16(bytes, FILE_HEADER_LEN + 20));
    (0..region_count)
        .map(|index| DIRECTORY_START + index * REGION_ENTRY_LEN)
        .find(|offset| read_u16(bytes, *offset) == kind)
        .expect("fixture region entry")
}

fn region_bytes(bytes: &[u8], kind: u16) -> &[u8] {
    let entry = region_entry_offset(bytes, kind);
    let start = usize::try_from(read_u64(bytes, entry + 8)).expect("region offset");
    let length = usize::try_from(read_u64(bytes, entry + 16)).expect("region length");
    &bytes[start..start + length]
}

fn region_version(bytes: &[u8], kind: u16) -> u16 {
    read_u16(bytes, region_entry_offset(bytes, kind) + 2)
}

fn relabel_region(bytes: &mut [u8], from: RegionKind, to: u16) {
    let entry = region_entry_offset(bytes, from.id());
    bytes[entry..entry + 2].copy_from_slice(&to.to_le_bytes());
    let checksum_entry = region_entry_offset(bytes, RegionKind::ChecksumTable.id());
    let checksum_start =
        usize::try_from(read_u64(bytes, checksum_entry + 8)).expect("checksum-table offset");
    let checksum_length =
        usize::try_from(read_u64(bytes, checksum_entry + 16)).expect("checksum-table length");
    let checksum_end = checksum_start + checksum_length;
    let chunk_count = usize::try_from(read_u32(bytes, checksum_start)).expect("chunk count");
    for chunk in 0..chunk_count {
        let chunk_entry = checksum_start + 8 + chunk * 16;
        if read_u16(bytes, chunk_entry) == from.id() {
            bytes[chunk_entry..chunk_entry + 2].copy_from_slice(&to.to_le_bytes());
        }
    }
    let checksum_table_checksum = xxh3_64(&bytes[checksum_start..checksum_end]).to_le_bytes();
    bytes[checksum_entry + 24..checksum_entry + 32].copy_from_slice(&checksum_table_checksum);
    let header_length = usize::try_from(read_u64(bytes, 16)).expect("header length");
    let header_checksum_offset = header_length - 8;
    let header_checksum = xxh3_64(&bytes[..header_checksum_offset]).to_le_bytes();
    bytes[header_checksum_offset..header_length].copy_from_slice(&header_checksum);
    let trailer = bytes.len() - FILE_TRAILER_LEN;
    let file_checksum = xxh3_64(&bytes[..trailer]).to_le_bytes();
    bytes[trailer..].copy_from_slice(&file_checksum);
}

#[test]
fn maintain_defers_promotion_of_segments_whose_regions_it_cannot_carry() {
    const METADATA: &[u8] = b"tier-maintenance-metadata";

    let directory = tempdir().expect("guarded tiering directory");
    let store = open_sift_store(directory.path(), "open guarded tiering store");
    let rows = GRAPH_TEST_ROWS;
    let documents = (0..rows)
        .map(|row| {
            let document = IngestDocument::new(
                DocumentVersion::new(DocId::new(row as u128 + 1), Revision::new(1)),
                fixture_vector(row as f32),
            );
            if row == 0 {
                document.with_metadata(METADATA.to_vec())
            } else {
                document
            }
        })
        .collect::<Vec<_>>();
    store
        .ingest(IngestBatch::new(documents).with_epoch(sift_epoch().identity()))
        .expect("ingest guarded fixture");
    store.seal().expect("seal guarded fixture");
    let source_id = store
        .snapshot()
        .expect("guarded source snapshot")
        .segments()
        .first()
        .expect("guarded source segment")
        .meta()
        .id;
    let source_path = directory.path().join(source_id.file_name());
    store.close().expect("close before guarded relabel");
    let mut source_bytes = fs::read(&source_path).expect("read guarded source bytes");
    relabel_region(
        &mut source_bytes,
        RegionKind::StoredMetadata,
        RegionKind::GraphColocatedCodes.id(),
    );
    fs::write(&source_path, source_bytes).expect("write guarded source bytes");
    let store = open_sift_store(directory.path(), "reopen guarded tiering store");
    store
        .snapshot()
        .expect("guarded source snapshot")
        .segments()
        .first()
        .expect("guarded source segment")
        .validate_all()
        .expect("valid guarded source segment");

    let report = maintain_test(
        &store,
        MaintenanceBudget {
            wall_time: MAINTENANCE_TEST_BUDGET,
            bytes: u64::MAX,
        },
    );

    let snapshot = store.snapshot().expect("guarded snapshot");
    let segment = snapshot.segments().first().expect("guarded sealed segment");
    assert_eq!(segment.meta().id, source_id);
    assert!(matches!(report.status, MaintenanceStatus::Complete));
    assert_eq!(report.promotion_deferrals.len(), 1);
    let deferral = report
        .promotion_deferrals
        .first()
        .expect("typed promotion deferral");
    assert_eq!(deferral.segment_id, segment.meta().id);
    assert_eq!(
        deferral.uncarried_regions,
        vec![UncarriedRegionKind::Known(RegionKind::GraphColocatedCodes)]
    );
    assert_eq!(report.graphs_built, 0);
    assert!(!has_graph(&store));
}

#[test]
fn graph_promotion_preserves_stored_metadata_and_postings() {
    const METADATA: &[u8] = b"promoted-tier-metadata";

    let directory = tempdir().expect("promotion preservation directory");
    let store = open_sift_store(directory.path(), "open promotion preservation store");
    let rows = GRAPH_TEST_ROWS;
    let documents = (0..rows)
        .map(|row| {
            let document = IngestDocument::new(
                DocumentVersion::new(DocId::new(row as u128 + 1), Revision::new(1)),
                fixture_vector(row as f32),
            );
            if row == 0 {
                document
                    .with_text("promoted lexical row")
                    .with_metadata(METADATA.to_vec())
            } else {
                document
            }
        })
        .collect::<Vec<_>>();
    store
        .ingest(IngestBatch::new(documents).with_epoch(sift_epoch().identity()))
        .expect("ingest promotion preservation fixture");
    store.seal().expect("seal promotion preservation fixture");

    let source = store.snapshot().expect("source snapshot");
    let source_segment = source.segments().first().expect("source segment");
    let source_id = source_segment.meta().id;
    drop(source);

    let report = maintain_test(
        &store,
        MaintenanceBudget {
            wall_time: MAINTENANCE_TEST_BUDGET,
            bytes: u64::MAX,
        },
    );

    assert!(matches!(report.status, MaintenanceStatus::Complete));
    assert_eq!(
        report.graphs_built, 1,
        "text-bearing segment was not promoted"
    );
    assert!(report.promotion_deferrals.is_empty());
    let promoted = store.snapshot().expect("promoted snapshot");
    let promoted_segment = promoted.segments().first().expect("promoted segment");
    assert_ne!(promoted_segment.meta().id, source_id);
    assert!(has_graph(&store));
    assert!(
        promoted_segment
            .postings()
            .expect("read promoted postings")
            .is_some()
    );
    let metadata = promoted_segment
        .stored_metadata()
        .expect("read promoted metadata")
        .expect("promoted metadata region");
    let metadata_row = (0..promoted_segment.meta().row_count as usize)
        .find(|row| {
            promoted_segment
                .document_version(*row)
                .expect("read promoted document mapping")
                .is_some_and(|document| document.doc_id() == DocId::new(1))
        })
        .expect("promoted metadata document");
    assert_eq!(metadata.row(metadata_row), Some(METADATA));
    let lexical = store
        .search_lexical(
            &TermQuery::flat(vec![b"promot".to_vec()], &[DEFAULT_FIELD]),
            1,
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search promoted postings");
    assert_eq!(lexical.candidates.len(), 1);
    assert_eq!(lexical.candidates[0].document.doc_id(), DocId::new(1));
}

#[test]
fn graph_promotion_copies_unknown_region_bytes_forward() {
    const UNKNOWN_BYTES: &[u8] = b"unknown promotion payload";

    let directory = tempdir().expect("unknown promotion directory");
    let store = open_sift_store(directory.path(), "open unknown promotion store");
    let rows = GRAPH_TEST_ROWS;
    let documents = (0..rows)
        .map(|row| {
            let document = IngestDocument::new(
                DocumentVersion::new(DocId::new(row as u128 + 1), Revision::new(1)),
                fixture_vector(row as f32),
            );
            if row == 0 {
                document.with_metadata(UNKNOWN_BYTES.to_vec())
            } else {
                document
            }
        })
        .collect::<Vec<_>>();
    store
        .ingest(IngestBatch::new(documents).with_epoch(sift_epoch().identity()))
        .expect("ingest unknown promotion fixture");
    store.seal().expect("seal unknown promotion fixture");
    let source_id = store
        .snapshot()
        .expect("unknown source snapshot")
        .segments()
        .first()
        .expect("unknown source segment")
        .meta()
        .id;
    let source_path = directory.path().join(source_id.file_name());
    store.close().expect("close before fixture relabel");

    let mut source_bytes = fs::read(&source_path).expect("read source segment bytes");
    relabel_region(
        &mut source_bytes,
        RegionKind::StoredMetadata,
        UNKNOWN_REGION_KIND,
    );
    let unknown_before = region_bytes(&source_bytes, UNKNOWN_REGION_KIND).to_vec();
    let version_before = region_version(&source_bytes, UNKNOWN_REGION_KIND);
    fs::write(&source_path, source_bytes).expect("write unknown-region fixture");

    let store = open_sift_store(directory.path(), "reopen unknown promotion store");
    store
        .snapshot()
        .expect("unknown source snapshot")
        .segments()
        .first()
        .expect("unknown source segment")
        .validate_all()
        .expect("valid unknown source segment");
    let report = maintain_test(
        &store,
        MaintenanceBudget {
            wall_time: MAINTENANCE_TEST_BUDGET,
            bytes: u64::MAX,
        },
    );

    let MaintenanceStatus::Failed(error) = &report.status else {
        panic!(
            "unexpected unknown-region maintenance status: {:?}",
            report.status
        );
    };
    assert_eq!(
        error.to_string(),
        "tier maintenance graph: graph build geometry: refinement geometry: \
         renumber cannot carry source region kind 65000"
    );
    let promoted = store.snapshot().expect("unknown promoted snapshot");
    let segment = promoted
        .segments()
        .first()
        .expect("unknown promoted segment");
    assert_eq!(segment.unknown_region_ids(), vec![UNKNOWN_REGION_KIND]);
    assert!(has_graph(&store));
    let promoted_bytes =
        fs::read(directory.path().join(segment.meta().id.file_name())).expect("promoted bytes");
    assert_eq!(
        region_bytes(&promoted_bytes, UNKNOWN_REGION_KIND),
        unknown_before
    );
    assert_eq!(
        region_version(&promoted_bytes, UNKNOWN_REGION_KIND),
        version_before
    );
}

#[test]
fn maintain_still_promotes_plain_vector_only_segments() {
    let fixture = sealed_fixture(GRAPH_TEST_ROWS);

    let report = maintain_test(
        &fixture.store,
        MaintenanceBudget {
            wall_time: MAINTENANCE_TEST_BUDGET,
            bytes: u64::MAX,
        },
    );

    assert!(matches!(report.status, MaintenanceStatus::Complete));
    assert!(report.promotion_deferrals.is_empty());
    assert_eq!(report.graphs_built, 1);
    assert!(has_graph(&fixture.store));
}

#[test]
fn publish_transition_unlinks_the_replaced_segment() {
    let fixture = sealed_fixture(GRAPH_TEST_ROWS);
    let source = fixture
        .store
        .snapshot()
        .expect("source snapshot")
        .segments()
        .first()
        .expect("source segment")
        .meta()
        .id;
    let source_path = fixture._directory.path().join(source.file_name());
    assert!(source_path.exists(), "source segment fixture is missing");

    let report = maintain_test(
        &fixture.store,
        MaintenanceBudget {
            wall_time: MAINTENANCE_TEST_BUDGET,
            bytes: u64::MAX,
        },
    );

    assert!(matches!(report.status, MaintenanceStatus::Complete));
    assert_eq!(report.graphs_built, 1);
    assert!(
        !source_path.exists(),
        "replaced segment file still exists after transition: {}",
        source_path.display()
    );
}

#[test]
fn maintain_builds_a_graph_for_a_sealed_segment_above_the_threshold() {
    let fixture = sealed_fixture(GRAPH_TEST_ROWS);

    let report = maintain_test(
        &fixture.store,
        MaintenanceBudget {
            wall_time: MAINTENANCE_TEST_BUDGET,
            bytes: u64::MAX,
        },
    );

    assert_eq!(report.graphs_built, 1);
    assert!(has_graph(&fixture.store));
}

#[test]
fn maintain_is_idempotent_and_a_second_call_does_no_work() {
    let fixture = sealed_fixture(GRAPH_TEST_ROWS);
    let budget = MaintenanceBudget {
        wall_time: MAINTENANCE_TEST_BUDGET,
        bytes: u64::MAX,
    };

    let first = maintain_test(&fixture.store, budget);
    let second = maintain_test(&fixture.store, budget);

    assert_eq!(first.graphs_built, 1);
    assert_eq!(second.graphs_built, 0);
}

#[test]
fn maintain_respects_its_budget_and_resumes_interrupted_work() {
    let fixture = sealed_fixture(GRAPH_TEST_ROWS);
    let byte_budget = 256_u64 * 64;

    let interrupted = maintain_test(
        &fixture.store,
        MaintenanceBudget {
            wall_time: MAINTENANCE_TEST_BUDGET,
            bytes: byte_budget,
        },
    );

    assert_eq!(interrupted.graphs_built, 0);
    assert!(interrupted.bytes_consumed > 0);
    assert!(interrupted.bytes_consumed <= byte_budget + byte_budget / 10);
    assert!(!has_graph(&fixture.store));

    let resumed = maintain_test(
        &fixture.store,
        MaintenanceBudget {
            wall_time: MAINTENANCE_TEST_BUDGET,
            bytes: u64::MAX,
        },
    );

    assert_eq!(resumed.checkpoints_resumed, 1);
    assert_eq!(resumed.graphs_built, 1);
    assert!(has_graph(&fixture.store));
}

#[test]
fn store_search_uses_the_graph_automatically_after_maintain() {
    let fixture = sealed_fixture(GRAPH_TEST_ROWS);
    let report = maintain_test(
        &fixture.store,
        MaintenanceBudget {
            wall_time: MAINTENANCE_TEST_BUDGET,
            bytes: u64::MAX,
        },
    );
    assert_eq!(report.graphs_built, 1);

    let outcome = fixture
        .store
        .search(
            SearchRequest::new(&[0.0_f32; DIMS]),
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
    let fixture = sealed_fixture(GRAPH_TEST_ROWS);

    let outcome = fixture
        .store
        .search(
            SearchRequest::new(&[0.0_f32; DIMS]),
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
    let store = Arc::new(open_sift_store(directory.path(), "open concurrent store"));
    let rows = GRAPH_TEST_ROWS;
    let documents = (0..rows)
        .map(|row| {
            let amplitude = row as f32 + 1.0;
            IngestDocument::new(
                DocumentVersion::new(DocId::new(row as u128 + 1), Revision::new(1)),
                fixture_vector(amplitude),
            )
        })
        .collect::<Vec<_>>();
    store
        .ingest(IngestBatch::new(documents).with_epoch(sift_epoch().identity()))
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
            let query = fixture_vector(amplitude);
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
    let report = maintain_test(
        &store,
        MaintenanceBudget {
            wall_time: MAINTENANCE_TEST_BUDGET,
            bytes: u64::MAX,
        },
    );
    maintenance_done.store(true, Ordering::Release);
    let (queries, graph_queries) = query_thread
        .join()
        .expect("query workload thread")
        .expect("query workload stayed correct");

    assert_eq!(report.graphs_built, 1);
    assert!(queries > 16);
    assert!(graph_queries > 0);
}
