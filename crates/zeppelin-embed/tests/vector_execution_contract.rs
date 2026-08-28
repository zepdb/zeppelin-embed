#![allow(clippy::expect_used, clippy::indexing_slicing)]

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use tempfile::{TempDir, tempdir};
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, IngestError, Revision, RowSource,
    SearchCandidate, SearchOutcome, SearchRequest,
};
use zeppelin_embed::kernels::vector_fault::{KernelFaultController, KernelOperationId};
use zeppelin_embed::kernels::{KernelBackendId, KernelVariant};
use zeppelin_embed::lifecycle::{
    CancelToken, InMemorySegment, InMemorySegmentFactors, OpenOptions, QueryControl, QueryError,
    SearchOptions, SearchTier, Store, StoreError, StoreTestDependencies, SystemMonotonicClock,
};
use zeppelin_embed::meta::{AliveSet, ColumnStoreBuilder, Schema};
use zeppelin_embed::quant::{
    Int8Vec, QuantError, QuantScheme, dot_int8_query, est_dot_bit4, prepare_bit4_query,
    prepare_int8_query, quantize_bit4, quantize_int8,
};
use zeppelin_embed::scan::vector_fault::{
    MissingRescoreSite, VectorAllocationSite, VectorCampaign, VectorFault, VectorFaultController,
    VectorFaultEffect, VectorFaultKind, VectorFaultSite, VectorOperation, VectorQuantField,
    VectorQuantScheme, VectorRowSource, VectorSearchTier,
};
use zeppelin_embed::scan::{ScanError, ScanOptions};
use zeppelin_embed::segment::SegmentId;
use zeppelin_embed::segment::layout::Int8Factors;
use zeppelin_embed::vfs::StdVfs;

const QUERY: [f32; 3] = [1.0, -1.0, 0.5];

fn search_options(tier: SearchTier) -> SearchOptions {
    SearchOptions::new(ScanOptions { thread_budget: 1 }).with_tier(tier)
}

fn search(store: &Store, tier: SearchTier, k: usize) -> Result<SearchOutcome, QueryError> {
    store.search(
        SearchRequest::new(&QUERY),
        k,
        search_options(tier),
        QueryControl::Cancel(CancelToken::new()),
    )
}

fn vector_dependencies(controller: VectorFaultController) -> StoreTestDependencies {
    StoreTestDependencies::new(Arc::new(StdVfs), Arc::new(SystemMonotonicClock))
        .with_vector_fault_controller(controller)
}

fn kernel_dependencies(controller: KernelFaultController) -> StoreTestDependencies {
    StoreTestDependencies::new(Arc::new(StdVfs), Arc::new(SystemMonotonicClock))
        .with_kernel_fault_controller(controller)
}

fn ingest_rows(store: &Store, documents: &[(u128, [f32; 3])]) {
    store
        .ingest(IngestBatch::new(
            documents
                .iter()
                .map(|(document, vector)| {
                    IngestDocument::new(
                        DocumentVersion::new(DocId::new(*document), Revision::new(1)),
                        vector.to_vec(),
                    )
                })
                .collect(),
        ))
        .expect("ingest vector fixture");
}

fn ingest_versioned_rows(store: &Store, documents: &[(u128, u64, [f32; 3])]) {
    store
        .ingest(IngestBatch::new(
            documents
                .iter()
                .map(|(document, revision, vector)| {
                    IngestDocument::new(
                        DocumentVersion::new(DocId::new(*document), Revision::new(*revision)),
                        vector.to_vec(),
                    )
                })
                .collect(),
        ))
        .expect("ingest versioned vector fixture");
}

fn result_documents(outcome: &SearchOutcome) -> Vec<DocumentVersion> {
    outcome
        .candidates
        .iter()
        .filter_map(|candidate| candidate.document())
        .collect()
}

fn assert_same_result(control: &SearchOutcome, retry: &SearchOutcome) {
    assert_eq!(
        retry.candidates, control.candidates,
        "post-clear candidate membership, score, or order diverged from clean control"
    );
    assert_eq!(retry.stats.dims_touched, control.stats.dims_touched);
    assert_eq!(retry.stats.bytes_read, control.stats.bytes_read);
    assert_eq!(retry.graph_stats, control.graph_stats);
    assert_eq!(retry.epoch, control.epoch);
    assert_eq!(retry.diagnostics.plan, control.diagnostics.plan);
    assert_eq!(
        retry.diagnostics.exact_rescore,
        control.diagnostics.exact_rescore
    );
    assert_eq!(
        retry.diagnostics.approximate,
        control.diagnostics.approximate
    );
    assert_eq!(retry.diagnostics.returned, control.diagnostics.returned);
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FrozenFixtureFile {
    relative_path: String,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FrozenStoreFixture {
    files: Vec<FrozenFixtureFile>,
}

fn collect_fixture_files(root: &Path, directory: &Path, output: &mut Vec<FrozenFixtureFile>) {
    let mut entries = std::fs::read_dir(directory)
        .expect("read vector fixture directory")
        .collect::<Result<Vec<_>, _>>()
        .expect("enumerate vector fixture directory");
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let file_type = entry.file_type().expect("vector fixture file type");
        let path = entry.path();
        if file_type.is_dir() {
            collect_fixture_files(root, &path, output);
        } else {
            assert!(file_type.is_file(), "fixture contains non-file {path:?}");
            let relative_path = path
                .strip_prefix(root)
                .expect("relativize vector fixture path")
                .to_str()
                .expect("vector fixture path is UTF-8")
                .replace(std::path::MAIN_SEPARATOR, "/");
            output.push(FrozenFixtureFile {
                relative_path,
                bytes: std::fs::read(&path).expect("read vector fixture bytes"),
            });
        }
    }
}

impl FrozenStoreFixture {
    fn capture(directory: &Path) -> Self {
        let mut files = Vec::new();
        collect_fixture_files(directory, directory, &mut files);
        files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        assert!(!files.is_empty(), "vector fixture contains no files");
        Self { files }
    }

    fn materialize(&self) -> TempDir {
        let directory = tempdir().expect("materialized vector fixture");
        for file in &self.files {
            let path = directory.path().join(&file.relative_path);
            std::fs::create_dir_all(path.parent().expect("fixture file parent"))
                .expect("create vector fixture directory");
            std::fs::write(&path, &file.bytes).expect("write vector fixture bytes");
        }
        assert_eq!(Self::capture(directory.path()), *self);
        directory
    }

    fn isolated_pair(&self) -> (TempDir, TempDir) {
        let clean = self.materialize();
        let fault = self.materialize();
        assert_ne!(clean.path(), fault.path());
        assert_eq!(Self::capture(clean.path()), Self::capture(fault.path()));
        (clean, fault)
    }
}

fn sealed_bit4_fixture(
    path: &std::path::Path,
) -> (SegmentId, FrozenStoreFixture, Vec<u8>, Vec<[u32; 3]>) {
    let store = Store::open(path, OpenOptions::default()).expect("open Bit4 fixture");
    let rows = [
        (31, [1.0, -1.0, 0.5]),
        (32, [0.5, -0.5, 0.25]),
        (33, [-1.0, 1.0, -0.5]),
    ];
    ingest_rows(&store, &rows);
    store.seal().expect("seal Bit4 fixture");
    let snapshot = store.snapshot().expect("Bit4 snapshot");
    let segment = &snapshot.segments()[0];
    let segment_id = segment.meta().id;
    let codes = segment.bit4_codes().expect("persisted Bit4 codes").to_vec();
    let factors = segment
        .bit4_factors()
        .expect("persisted Bit4 factors")
        .iter()
        .map(|factor| factor.persisted_fields().map(f32::to_bits))
        .collect();
    drop(snapshot);
    store.close().expect("close Bit4 fixture");
    (
        segment_id,
        FrozenStoreFixture::capture(path),
        codes,
        factors,
    )
}

fn sealed_int8_fixture(path: &std::path::Path) -> (SegmentId, FrozenStoreFixture) {
    let store = Store::open(path, OpenOptions::default()).expect("open Int8 fixture");
    let rows = [[1.0, -1.0, 0.5], [0.5, -0.5, 0.25], [-1.0, 1.0, -0.5]];
    let mut codes = Vec::new();
    let mut factors = Vec::new();
    for row in rows {
        let mut encoded = [0_i8; 3];
        let (scale, offset) = quantize_int8(&row, &mut encoded).expect("finite Int8 row");
        codes.extend(encoded.into_iter().map(|value| value as u8));
        factors.push(Int8Factors { scale, offset });
    }
    let schema = Schema::new(Vec::new()).expect("timestamp-only schema");
    let mut columns = ColumnStoreBuilder::new(schema);
    for timestamp in 0_i64..3 {
        columns.push_row(timestamp, &[]).expect("Int8 metadata row");
    }
    let columns = columns.finish().expect("Int8 columns");
    let alive = AliveSet::new(3);
    let segment_id = SegmentId::new(0x0102_0304_0506, [0x8a; 10]);
    let prepared = store
        .prepare_segment(InMemorySegment {
            id: segment_id,
            scheme: 2,
            dims: 3,
            codes,
            factors: InMemorySegmentFactors::Int8(factors),
            rescore: rows.into_iter().flatten().collect(),
            columns: &columns,
            alive: &alive,
        })
        .expect("prepare Int8 fixture");
    store.seal_snapshot(prepared).expect("seal Int8 fixture");
    store.close().expect("close Int8 fixture");
    (segment_id, FrozenStoreFixture::capture(path))
}

#[test]
fn vector_backend_ids_are_unique_and_name_every_concrete_table() {
    let observed = KernelVariant::available()
        .map(KernelVariant::backend_id)
        .collect::<Vec<_>>();
    let unique = observed.iter().copied().collect::<BTreeSet<_>>();

    assert_eq!(
        observed.len(),
        unique.len(),
        "concrete backend IDs collided"
    );
    assert!(observed.contains(&KernelBackendId::Scalar));
    assert!(
        observed
            .into_iter()
            .all(|backend| !backend.as_str().is_empty())
    );
}

#[test]
fn default_store_backend_observation_requires_a_real_public_score() {
    let directory = tempdir().expect("default-backend Store");
    let controller = KernelFaultController::observing_store(2400);
    let store = Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::default(),
        kernel_dependencies(controller.clone()),
    )
    .expect("open observed Store");
    assert!(
        controller.take_observations().is_empty(),
        "Store initialization alone claimed scoring work"
    );
    ingest_rows(&store, &[(1, QUERY), (2, [0.5, -0.5, 0.25])]);
    let result = search(&store, SearchTier::Scan, 2).expect("observed public Scan");
    let observations = controller.take_observations();
    assert_eq!(
        observations.len(),
        1,
        "expected one real scoring observation"
    );
    assert_eq!(
        observations[0].backend(),
        KernelVariant::selected().backend_id()
    );
    assert!(observations[0].work_items() > 0);
    assert!(observations[0].result_published());
    assert_eq!(result.candidates.len(), 2);
    assert!(
        controller.take_typed_receipts().is_empty(),
        "neutral backend observation fabricated a feature-fault receipt"
    );
}

#[test]
fn vector_forced_backend_requires_selected_scoring_receipt() {
    let source = tempdir().expect("forced-backend fixture");
    let fixture = Store::open(source.path(), OpenOptions::default()).expect("open fixture Store");
    ingest_rows(&fixture, &[(1, QUERY), (2, [0.5, -0.5, 0.25])]);
    fixture.close().expect("close fixture Store");
    let frozen = FrozenStoreFixture::capture(source.path());
    let (clean_directory, fault_directory) = frozen.isolated_pair();
    let clean =
        Store::open(clean_directory.path(), OpenOptions::default()).expect("open clean Store");
    let control = search(&clean, SearchTier::Scan, 2).expect("clean Store search");
    clean.close().expect("close clean Store");

    let available = KernelVariant::available()
        .map(KernelVariant::backend_id)
        .collect::<BTreeSet<_>>();
    let requested = if available.contains(&KernelBackendId::NeonDotprodU4) {
        KernelBackendId::NeonDotprodU4
    } else if available.contains(&KernelBackendId::NeonWiden) {
        KernelBackendId::NeonWiden
    } else if available.contains(&KernelBackendId::Avx2) {
        KernelBackendId::Avx2
    } else {
        KernelBackendId::Scalar
    };
    let controller = KernelFaultController::forced_backend(requested, 2401);
    let faulted = Store::open_with_test_dependencies(
        fault_directory.path(),
        OpenOptions::default(),
        kernel_dependencies(controller.clone()),
    )
    .expect("open forced-backend Store");
    let observed = search(&faulted, SearchTier::Scan, 2).expect("forced Store search");
    assert_same_result(&control, &observed);
    let receipts = controller.take_typed_receipts();
    assert_eq!(
        receipts.len(),
        1,
        "expected one KernelDispatchSelectedScoringTable receipt, observed zero"
    );
    let receipt = &receipts[0];
    assert_eq!(receipt.campaign(), VectorCampaign::VectorExecution);
    assert_eq!(receipt.operation(), VectorOperation::KernelParity);
    assert_eq!(receipt.fault(), VectorFaultKind::ForcedDispatchBackend);
    assert_eq!(
        receipt.site(),
        VectorFaultSite::KernelDispatchSelectedScoringTable
    );
    assert_eq!(receipt.seed_case_id(), 2401);
    assert!(receipt.result_published());
    assert!(matches!(
        receipt.effect(),
        VectorFaultEffect::ForcedBackend {
            requested: effect_requested,
            selected,
            kernel: KernelOperationId::ScoreBit4PreparedBatch,
            work_items,
        } if *effect_requested == requested && *selected == requested && *work_items > 0
    ));
    let retry = search(&faulted, SearchTier::Scan, 2).expect("forced backend retry");
    assert_same_result(&control, &retry);
    assert!(
        controller.take_typed_receipts().is_empty(),
        "fault fired twice"
    );
}

#[test]
fn i25_public_store_persists_exact_bit4_bytes_and_rejects_non_finite_atomically() {
    let directory = tempdir().expect("I25 Store");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open I25 Store");
    let before = store.snapshot().expect("initial snapshot").generation();
    let error = store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(24), Revision::new(1)),
            vec![1.0, f32::NAN, 0.5],
        )]))
        .expect_err("non-finite ingest must fail");
    assert!(matches!(
        error,
        IngestError::Vector(QuantError::NonFinite { index: 1 })
    ));
    assert_eq!(
        store.snapshot().expect("unchanged snapshot").generation(),
        before
    );
    assert!(
        search(&store, SearchTier::Exact, 1)
            .expect("empty Store remains searchable")
            .candidates
            .is_empty()
    );

    let version = DocumentVersion::new(DocId::new(25), Revision::new(1));
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            version,
            QUERY.to_vec(),
        )]))
        .expect("finite ingest");
    store.seal().expect("seal I25 Store");
    let snapshot = store.snapshot().expect("I25 sealed snapshot");
    let segment = &snapshot.segments()[0];
    let mut expected_codes = vec![0_u8; QUERY.len().div_ceil(2)];
    let expected_factors = quantize_bit4(&QUERY, &mut expected_codes).expect("oracle encode");
    assert_eq!(
        segment.bit4_codes().expect("persisted codes"),
        expected_codes
    );
    assert_eq!(
        segment.bit4_factors().expect("persisted factors")[0].persisted_fields(),
        expected_factors.persisted_fields()
    );
    drop(snapshot);
    let outcome = search(&store, SearchTier::Scan, 1).expect("public sealed Scan");
    assert_eq!(result_documents(&outcome), vec![version]);
}

#[test]
fn quant_int8_public_store_ingest_document_leg_persists_and_reopens() {
    let directory = tempdir().expect("I25 Int8 public-ingest tempdir");
    let dependencies = StoreTestDependencies::new(Arc::new(StdVfs), Arc::new(SystemMonotonicClock))
        .with_vector_seal_scheme(QuantScheme::Int8);
    let store =
        Store::open_with_test_dependencies(directory.path(), OpenOptions::default(), dependencies)
            .expect("open I25 Int8 public-ingest Store");
    let version = DocumentVersion::new(DocId::new(0x25_08), Revision::new(7));
    store
        .ingest(IngestBatch::new(vec![IngestDocument::new(
            version,
            QUERY.to_vec(),
        )]))
        .expect("public ingest Int8 document");
    store.seal().expect("seal public-ingested Int8 document");
    let snapshot = store.snapshot().expect("Int8 public-ingest snapshot");
    let segment = &snapshot.segments()[0];
    assert_eq!(segment.meta().scheme, u16::from(QuantScheme::Int8.id()));
    let mut expected_codes = vec![0_i8; QUERY.len()];
    let (expected_scale, expected_offset) =
        quantize_int8(&QUERY, &mut expected_codes).expect("independent Int8 encode");
    assert_eq!(
        segment.int8_codes().expect("persisted Int8 codes"),
        expected_codes
    );
    let factors = segment.int8_factors().expect("persisted Int8 factors");
    assert_eq!(factors.len(), 1);
    assert_eq!(factors[0].scale.to_bits(), expected_scale.to_bits());
    assert_eq!(factors[0].offset.to_bits(), expected_offset.to_bits());
    drop(snapshot);
    store.close().expect("close Int8 public-ingest Store");

    let reopened = Store::open(directory.path(), OpenOptions::default())
        .expect("reopen I25 Int8 public-ingest Store");
    let outcome = search(&reopened, SearchTier::Scan, 1).expect("public Int8 Scan after reopen");
    assert_eq!(result_documents(&outcome), vec![version]);
    reopened.close().expect("close reopened Int8 Store");
}

#[test]
fn i25_code_length_errors_are_exact_for_both_formats() {
    let bit4 = prepare_bit4_query(&QUERY, 7).expect("Bit4 query");
    assert_eq!(
        est_dot_bit4(
            &bit4,
            &[0xff],
            quantize_bit4(&QUERY, &mut [0_u8; 2]).expect("Bit4 factors")
        ),
        Err(QuantError::CodeLength {
            expected: 2,
            actual: 1,
        })
    );

    let int8 = prepare_int8_query(&QUERY).expect("Int8 query");
    assert_eq!(
        dot_int8_query(
            &int8,
            Int8Vec {
                codes: &[1, 2],
                scale: 1.0,
                offset: 0.0,
            }
        ),
        Err(QuantError::CodeLength {
            expected: 3,
            actual: 2,
        })
    );
}

#[test]
fn corrupt_bit4_padding_fires_only_in_public_store_scan_and_retry_matches_control() {
    let source = tempdir().expect("Bit4 padding fixture");
    let (segment, frozen, _, _) = sealed_bit4_fixture(source.path());
    let (clean_directory, fault_directory) = frozen.isolated_pair();
    let clean = Store::open(clean_directory.path(), OpenOptions::default())
        .expect("open Bit4 padding control Store");
    let control = search(&clean, SearchTier::Scan, 3).expect("clean Bit4 padding search");
    clean.close().expect("close Bit4 padding control Store");
    let controller = VectorFaultController::armed(
        VectorFault::CorruptBit4OddPadding {
            source: VectorRowSource::Sealed(*segment.as_bytes()),
            local_row: 0,
        },
        2501,
    );
    let store = Store::open_with_test_dependencies(
        fault_directory.path(),
        OpenOptions::default(),
        vector_dependencies(controller.clone()),
    )
    .expect("open faulted Bit4 Store");
    let error = search(&store, SearchTier::Scan, 3).expect_err("padding corruption must fail");
    assert!(matches!(
        &error,
        QueryError::Scan(ScanError::Quant(QuantError::NonZeroPadding {
            byte: 0xbf,
            mask: 0x0f,
        }))
    ));
    assert_eq!(
        error.to_string(),
        "scan quantization error: packed quantization code has non-zero padding: byte=0xbf, mask=0x0f"
    );
    let receipts = controller.take_typed_receipts();
    assert_eq!(receipts.len(), 1, "expected real ScanBit4CodeView receipt");
    assert_eq!(receipts[0].site(), VectorFaultSite::ScanBit4CodeView);
    assert!(!receipts[0].result_published());
    assert!(matches!(
        receipts[0].effect(),
        VectorFaultEffect::CorruptedPayload {
            scheme: VectorQuantScheme::Bit4,
            source: VectorRowSource::Sealed(source),
            tier: VectorSearchTier::Scan,
            local_row: 0,
            field: VectorQuantField::OddPadding,
            before_bits,
            after_bits,
            ..
        } if *source == *segment.as_bytes() && after_bits != before_bits
    ));
    let retry = search(&store, SearchTier::Scan, 3).expect("Bit4 padding retry");
    assert_same_result(&control, &retry);
}

#[test]
fn corrupt_bit4_factor_fires_only_in_public_store_scan_and_retry_matches_control() {
    let source = tempdir().expect("Bit4 factor fixture");
    let (segment, frozen, _, _) = sealed_bit4_fixture(source.path());
    let (clean_directory, fault_directory) = frozen.isolated_pair();
    let clean = Store::open(clean_directory.path(), OpenOptions::default())
        .expect("open Bit4 factor control Store");
    let control = search(&clean, SearchTier::Scan, 3).expect("clean Bit4 factor search");
    clean.close().expect("close Bit4 factor control Store");
    let controller = VectorFaultController::armed(
        VectorFault::CorruptBit4CorrectionNaN {
            source: VectorRowSource::Sealed(*segment.as_bytes()),
            local_row: 0,
        },
        2502,
    );
    let store = Store::open_with_test_dependencies(
        fault_directory.path(),
        OpenOptions::default(),
        vector_dependencies(controller.clone()),
    )
    .expect("open faulted Bit4 factor Store");
    let error = search(&store, SearchTier::Scan, 3).expect_err("NaN correction must fail");
    assert!(matches!(
        &error,
        QueryError::Scan(ScanError::NonFiniteScore { row_id: 0 })
    ));
    assert_eq!(error.to_string(), "scan score is non-finite at row 0");
    let receipts = controller.take_typed_receipts();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].site(), VectorFaultSite::ScanBit4FactorView);
    assert!(!receipts[0].result_published());
    assert!(matches!(
        receipts[0].effect(),
        VectorFaultEffect::CorruptedPayload {
            scheme: VectorQuantScheme::Bit4,
            field: VectorQuantField::Correction,
            byte_offset: 8,
            after_bits,
            ..
        } if *after_bits == u64::from(f32::NAN.to_bits())
    ));
    let retry = search(&store, SearchTier::Scan, 3).expect("Bit4 factor retry");
    assert_same_result(&control, &retry);
}

#[test]
fn corrupt_int8_factor_fires_only_in_public_store_scan_and_retry_matches_control() {
    let source = tempdir().expect("Int8 factor fixture");
    let (segment, frozen) = sealed_int8_fixture(source.path());
    let (clean_directory, fault_directory) = frozen.isolated_pair();
    let clean = Store::open(clean_directory.path(), OpenOptions::default())
        .expect("open Int8 factor control Store");
    let control = search(&clean, SearchTier::Scan, 3).expect("clean Int8 factor search");
    clean.close().expect("close Int8 factor control Store");
    let controller = VectorFaultController::armed(
        VectorFault::CorruptInt8ScaleNaN {
            source: VectorRowSource::Sealed(*segment.as_bytes()),
            local_row: 0,
        },
        2503,
    );
    let store = Store::open_with_test_dependencies(
        fault_directory.path(),
        OpenOptions::default(),
        vector_dependencies(controller.clone()),
    )
    .expect("open faulted Int8 Store");
    let error = search(&store, SearchTier::Scan, 3).expect_err("NaN Int8 scale must fail");
    assert!(matches!(
        &error,
        QueryError::Scan(ScanError::NonFiniteScore { row_id: 0 })
    ));
    assert_eq!(error.to_string(), "scan score is non-finite at row 0");
    let receipts = controller.take_typed_receipts();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].site(), VectorFaultSite::ScanInt8FactorView);
    assert!(!receipts[0].result_published());
    assert!(matches!(
        receipts[0].effect(),
        VectorFaultEffect::CorruptedPayload {
            scheme: VectorQuantScheme::Int8,
            source: VectorRowSource::Sealed(source),
            tier: VectorSearchTier::Scan,
            local_row: 0,
            field: VectorQuantField::Scale,
            byte_offset: 0,
            after_bits,
            ..
        } if *source == *segment.as_bytes() && *after_bits == u64::from(f32::NAN.to_bits())
    ));
    let retry = search(&store, SearchTier::Scan, 3).expect("Int8 factor retry");
    assert_same_result(&control, &retry);
}

#[test]
fn vector_missing_rescore_rows_fires_in_store_search() {
    let source = tempdir().expect("missing-rescore fixture");
    let (segment, frozen, _, _) = sealed_bit4_fixture(source.path());
    let (clean_directory, fault_directory) = frozen.isolated_pair();
    let clean =
        Store::open(clean_directory.path(), OpenOptions::default()).expect("open exact control");
    let control = search(&clean, SearchTier::Exact, 3).expect("exact control");
    clean.close().expect("close exact control");
    let controller = VectorFaultController::armed(
        VectorFault::MissingRescoreRows {
            source: VectorRowSource::Sealed(*segment.as_bytes()),
            site: MissingRescoreSite::ExactRescoreRows,
            expected_rows: 3,
            available_rows: 2,
            tier: VectorSearchTier::Exact,
        },
        2601,
    );
    let store = Store::open_with_test_dependencies(
        fault_directory.path(),
        OpenOptions::default(),
        vector_dependencies(controller.clone()),
    )
    .expect("open missing-rescore Store");
    let error = search(&store, SearchTier::Exact, 3).expect_err("missing rescore row must fail");
    let expected_detail =
        format!("exact scores unavailable for segment {segment}: expected 3 rows, got 2");
    assert!(matches!(
        &error,
        QueryError::Store(StoreError::Segment(zeppelin_embed::segment::SegmentError::Geometry(
            detail
        ))) if detail == &expected_detail
    ));
    assert_eq!(
        error.to_string(),
        format!("segment geometry is invalid: {expected_detail}")
    );
    let receipts = controller.take_typed_receipts();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].site(), VectorFaultSite::ExactRescoreRows);
    assert!(!receipts[0].result_published());
    assert!(matches!(
        receipts[0].effect(),
        VectorFaultEffect::MissingRescoreRows {
            segment: effect_segment,
            expected_rows: 3,
            available_rows: 2,
            requested_tier: VectorSearchTier::Exact,
        } if *effect_segment == *segment.as_bytes()
    ));
    let retry = search(&store, SearchTier::Exact, 3).expect("missing-rescore retry");
    assert_same_result(&control, &retry);
}

#[test]
fn cancel_after_exact_active_rows_has_no_partial_result_and_retry_matches_control() {
    let source = tempdir().expect("cancellation fixture");
    let fixture = Store::open(source.path(), OpenOptions::default()).expect("open active fixture");
    ingest_rows(
        &fixture,
        &[
            (41, QUERY),
            (42, [0.5, -0.5, 0.25]),
            (43, [-1.0, 1.0, -0.5]),
        ],
    );
    fixture.close().expect("close active fixture");
    let frozen = FrozenStoreFixture::capture(source.path());
    let (clean_directory, fault_directory) = frozen.isolated_pair();
    let clean =
        Store::open(clean_directory.path(), OpenOptions::default()).expect("open active control");
    let control = search(&clean, SearchTier::Exact, 3).expect("active exact control");
    clean.close().expect("close active control");
    let controller = VectorFaultController::armed(
        VectorFault::CancelAfterRows {
            source: VectorRowSource::Active,
            requested_rows: 2,
            tier: VectorSearchTier::Exact,
        },
        2701,
    );
    let store = Store::open_with_test_dependencies(
        fault_directory.path(),
        OpenOptions::default(),
        vector_dependencies(controller.clone()),
    )
    .expect("open cancellation Store");
    let fault_generation = store
        .snapshot()
        .expect("pre-cancellation snapshot")
        .generation();
    assert!(matches!(
        search(&store, SearchTier::Exact, 3).expect_err("second scored row must cancel"),
        QueryError::Cancelled { partial: false }
    ));
    assert_eq!(
        store.snapshot().expect("post-cancel snapshot").generation(),
        fault_generation
    );
    let receipts = controller.take_typed_receipts();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].site(), VectorFaultSite::ScoredVectorRow);
    assert!(!receipts[0].result_published());
    assert!(matches!(
        receipts[0].effect(),
        VectorFaultEffect::CancelledAfterRows {
            source: VectorRowSource::Active,
            requested_tier: VectorSearchTier::Exact,
            requested_rows: 2,
            observed_rows: 2,
            ..
        }
    ));
    let retry = search(&store, SearchTier::Exact, 3).expect("cancellation retry");
    assert_eq!(retry.generation, fault_generation);
    assert_same_result(&control, &retry);
}

#[test]
fn vector_allocation_denial_hits_candidate_reserve() {
    let source = tempdir().expect("allocation fixture");
    let fixture =
        Store::open(source.path(), OpenOptions::default()).expect("open allocation fixture");
    ingest_rows(
        &fixture,
        &[
            (51, QUERY),
            (52, [0.5, -0.5, 0.25]),
            (53, [-1.0, 1.0, -0.5]),
        ],
    );
    fixture.close().expect("close allocation fixture");
    let frozen = FrozenStoreFixture::capture(source.path());
    let (clean_directory, fault_directory) = frozen.isolated_pair();
    let clean = Store::open(clean_directory.path(), OpenOptions::default())
        .expect("open allocation control");
    let control = search(&clean, SearchTier::Exact, 3).expect("allocation control");
    clean.close().expect("close allocation control");
    let items = 3_u64;
    let candidate_bytes = u64::try_from(std::mem::size_of::<SearchCandidate>())
        .expect("SearchCandidate size fits u64");
    let bytes = items * candidate_bytes;
    let controller = VectorFaultController::armed(
        VectorFault::DenyGlobalCandidateAllocation { items, bytes },
        2702,
    );
    let store = Store::open_with_test_dependencies(
        fault_directory.path(),
        OpenOptions::default(),
        vector_dependencies(controller.clone()),
    )
    .expect("open allocation Store");
    let fault_generation = store.snapshot().expect("pre-denial snapshot").generation();
    let error = search(&store, SearchTier::Exact, 3).expect_err("candidate reserve must fail");
    assert!(matches!(
        error,
        QueryError::Store(StoreError::AllocationFailed {
            component: "vector search global candidates",
            needed,
        }) if needed == bytes
    ));
    assert_eq!(
        store.snapshot().expect("post-denial snapshot").generation(),
        fault_generation
    );
    let receipts = controller.take_typed_receipts();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].site(), VectorFaultSite::SearchGlobalCandidates);
    assert!(!receipts[0].result_published());
    assert!(matches!(
        receipts[0].effect(),
        VectorFaultEffect::AllocationDenied {
            component: VectorAllocationSite::SearchGlobalCandidates,
            requested_tier: VectorSearchTier::Exact,
            items: 3,
            bytes: effect_bytes,
        } if *effect_bytes == bytes
    ));
    let retry = search(&store, SearchTier::Exact, 3).expect("allocation retry");
    assert_eq!(retry.generation, fault_generation);
    assert_same_result(&control, &retry);
}

#[test]
fn identity_public_store_requires_nonempty_scores_document_ties_and_physical_identity() {
    let directory = tempdir().expect("identity Store");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open identity Store");
    ingest_versioned_rows(&store, &[(300, 4, QUERY)]);
    store.seal().expect("first seal");
    ingest_versioned_rows(&store, &[(200, 9, QUERY)]);
    store.seal().expect("second seal");
    ingest_versioned_rows(&store, &[(100, 2, QUERY)]);

    for tier in [SearchTier::Exact, SearchTier::Scan, SearchTier::Auto] {
        let outcome = search(&store, tier, 3).expect("identity tier search");
        assert_eq!(outcome.candidates.len(), 3, "Store membership was empty");
        assert!(
            outcome
                .candidates
                .iter()
                .all(|candidate| candidate.score().is_finite())
        );
        assert_eq!(
            result_documents(&outcome),
            vec![
                DocumentVersion::new(DocId::new(100), Revision::new(2)),
                DocumentVersion::new(DocId::new(200), Revision::new(9)),
                DocumentVersion::new(DocId::new(300), Revision::new(4)),
            ],
            "document/revision tie order did not override physical row order"
        );
        assert_eq!(
            result_documents(&outcome)
                .into_iter()
                .map(|document| document.doc_id())
                .collect::<Vec<_>>(),
            vec![DocId::new(100), DocId::new(200), DocId::new(300)],
            "document-ID tie order did not override physical row order"
        );
        let physical = outcome
            .candidates
            .iter()
            .map(|candidate| candidate.row_id())
            .collect::<BTreeSet<_>>();
        assert_eq!(physical.len(), 3, "physical rows collided");
        assert!(physical.iter().any(|row| row.source() == RowSource::Active));
        assert_eq!(
            physical
                .iter()
                .filter(|row| matches!(row.source(), RowSource::Sealed(_)))
                .count(),
            2
        );
        if tier == SearchTier::Exact {
            assert!(
                outcome.diagnostics.exact_rescore,
                "Exact tier did not report exhaustive exact scoring"
            );
        }
    }
}
