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
use zeppelin_embed::segment::reader::{SegmentCostAudit, SegmentReader};
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
fn int8_scan_results_are_bit_identical_with_factor_cache() {
    let source = tempdir().expect("Int8 result fixture");
    let (segment, frozen) = sealed_int8_fixture(source.path());
    let directory = frozen.materialize();
    let store =
        Store::open(directory.path(), OpenOptions::default()).expect("open Int8 result Store");
    let outcome = search(&store, SearchTier::Scan, 3).expect("public Int8 result scan");
    let observed = outcome
        .candidates
        .iter()
        .map(|candidate| {
            assert_eq!(candidate.row_id().source(), RowSource::Sealed(segment));
            (candidate.row_id().local_row(), candidate.score().to_bits())
        })
        .collect::<Vec<_>>();

    assert_eq!(
        observed,
        vec![(0, 0x4010_40c2), (1, 0x3f90_40c2), (2, 0xc010_40c2)]
    );
    store.close().expect("close Int8 result Store");
}

#[test]
fn int8_factor_validation_runs_once_per_segment() {
    let source = tempdir().expect("Int8 factor-cache fixture");
    let (_segment, frozen) = sealed_int8_fixture(source.path());
    let directory = frozen.materialize();
    let store = Store::open(directory.path(), OpenOptions::default())
        .expect("open Int8 factor-cache Store");
    let before = store.stats().expect("stats before Int8 factor validation");
    let audit = SegmentCostAudit::new();

    audit.measure(|| {
        search(&store, SearchTier::Scan, 3).expect("first public Int8 factor-cache scan");
    });
    let first = audit.snapshot();
    assert_eq!(
        first.int8_factor_decode_bytes,
        3 * std::mem::size_of::<Int8Factors>() as u64
    );

    audit.measure(|| {
        search(&store, SearchTier::Scan, 3).expect("second public Int8 factor-cache scan");
    });
    assert_eq!(
        audit.snapshot().int8_factor_decode_bytes,
        first.int8_factor_decode_bytes,
        "the identical segment must not revalidate Int8 factors"
    );

    let snapshot = store.snapshot().expect("Int8 factor-cache snapshot");
    let retained = snapshot
        .segments()
        .iter()
        .map(SegmentReader::retained_query_view_bytes)
        .sum::<u64>();
    drop(snapshot);
    let after = store.stats().expect("stats after Int8 factor validation");
    assert_eq!(
        after.snapshot_bytes - before.snapshot_bytes,
        retained,
        "every retained Int8 factor-cache byte must reach snapshot accounting"
    );
    store.close().expect("close Int8 factor-cache Store");
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
fn exact_tier_scores_are_bit_identical_to_the_previous_loop() {
    let directory = tempdir().expect("exact score golden fixture");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open exact fixture");
    ingest_rows(
        &store,
        &[
            (61, [0.1, -0.2, 0.3]),
            (62, [1.25, -1.5, 0.75]),
            (63, [3.0, -2.0, 1.0]),
            (64, [-0.7, 0.8, -0.9]),
            (65, QUERY),
        ],
    );

    let active = search(&store, SearchTier::Exact, 5).expect("active exact search");
    store.seal().expect("seal exact fixture");
    let sealed = search(&store, SearchTier::Exact, 5).expect("sealed exact search");

    assert!(
        active
            .candidates
            .iter()
            .all(|candidate| candidate.row_id().source() == RowSource::Active)
    );
    assert!(
        sealed
            .candidates
            .iter()
            .all(|candidate| matches!(candidate.row_id().source(), RowSource::Sealed(_)))
    );
    let signature = |outcome: &SearchOutcome| {
        (
            outcome
                .candidates
                .iter()
                .map(|candidate| {
                    (
                        candidate
                            .document()
                            .expect("fixture candidate has a document")
                            .doc_id()
                            .get(),
                        candidate.row_id().local_row(),
                        candidate.score().to_bits(),
                    )
                })
                .collect::<Vec<_>>(),
            outcome.stats.dims_touched,
            outcome.stats.bytes_read,
            outcome.stats.threads_used,
            outcome.stats.worker_thread_ids.len(),
        )
    };
    let actual = (signature(&active), signature(&sealed));

    let expected = (
        (
            vec![
                (65, 4, 2_147_483_648),
                (62, 1, 3_200_253_952),
                (61, 0, 3_216_947_282),
                (63, 2, 3_232_235_520),
                (64, 3, 3_238_097_060),
            ],
            15,
            60,
            1,
            1,
        ),
        (
            vec![
                (65, 4, 2_147_483_648),
                (62, 1, 3_200_253_952),
                (61, 0, 3_216_947_282),
                (63, 2, 3_232_235_520),
                (64, 3, 3_238_097_060),
            ],
            15,
            60,
            1,
            1,
        ),
    );
    assert_eq!(actual, expected);
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

#[test]
fn astra_04_exact_scan_does_not_materialize_corpus_results() {
    for n in [129_usize, 1025] {
        let directory = tempdir().expect("exact scratch fixture");
        let controller = VectorFaultController::observe_only(11);
        let store = Store::open_with_test_dependencies(
            directory.path(),
            OpenOptions::default(),
            vector_dependencies(controller.clone()),
        )
        .expect("open exact scratch fixture");
        let rows = (0..n)
            .map(|row| (row as u128 + 1, [row as f32 + 2.0, -1.0, 0.5]))
            .collect::<Vec<_>>();
        ingest_rows(&store, &rows);
        let outcome = search(&store, SearchTier::Exact, 7).expect("bounded exact query");
        assert_eq!(outcome.candidates.len(), 7);
        assert_eq!(outcome.stats.dims_touched, n as u64 * 3);
        assert_eq!(outcome.stats.bytes_read, n as u64 * 12);
        let work = controller.take_exact_scan_work();
        assert_eq!(work.len(), 1);
        eprintln!("exact scratch n={n} k=7: {:?}", work[0]);
        assert_eq!(work[0].scored_rows, n);
        assert_eq!(work[0].row_indices_capacity, 0, "full alive-row buffer");
        assert_eq!(work[0].coarse_scores_capacity, 0, "dummy scores");
        assert_eq!(work[0].exact_scores_capacity, 0, "full f64 result buffer");
        assert_eq!(
            work[0].converted_candidates_capacity, 0,
            "full converted result buffer"
        );
        assert_eq!(work[0].sorted_items, 7);
        assert!(work[0].collector_capacity <= 14);
    }
}

fn astra_04_run(store: &Store, query: &[f32], k: usize) -> Result<SearchOutcome, QueryError> {
    store.search(
        SearchRequest::new(query),
        k,
        search_options(SearchTier::Exact),
        QueryControl::Cancel(CancelToken::new()),
    )
}

#[test]
fn astra_04_exact_scan_boundary_ties_survive_identity_join() {
    let directory = tempdir().expect("tie fixture");
    let controller = VectorFaultController::observe_only(11);
    let store = Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::default(),
        vector_dependencies(controller.clone()),
    )
    .expect("open tie fixture");
    // f64 distinguishes 1 and 1 + 2^-26; both distances narrow to f32 1.
    let high = [1.0, 0.0001220703125, 0.0];
    assert_ne!(
        1_f64.to_bits(),
        (1.0 + f64::from(high[1]).powi(2)).to_bits()
    );
    assert_eq!(
        1_f32.to_bits(),
        ((1.0 + f64::from(high[1]).powi(2)) as f32).to_bits()
    );
    for base in [200_u128, 100, 0] {
        let rows = (1..=80)
            .rev()
            .map(|id| (base + id, if id % 2 == 0 { high } else { [1.0, 0.0, 0.0] }))
            .collect::<Vec<_>>();
        ingest_rows(&store, &rows);
        if base != 0 {
            store.seal().expect("seal tie segment");
        }
    }
    let outcome = astra_04_run(&store, &[0.0; 3], 7).expect("identity tie query");
    assert_eq!(
        result_documents(&outcome)
            .iter()
            .map(|v| v.doc_id().get())
            .collect::<Vec<_>>(),
        (1..=7).collect::<Vec<_>>()
    );
    assert!(
        outcome
            .candidates
            .iter()
            .all(|c| c.score().to_bits() == (-1_f32).to_bits())
    );
    assert_eq!(outcome.stats.dims_touched, 240 * 3);
    let work = controller.take_exact_scan_work();
    assert_eq!(work.len(), 3);
    assert!(work.iter().all(|w| w.scored_rows == 80));
    eprintln!("valid O(N) boundary ties: {work:?}");
}

#[test]
fn astra_04_exact_scan_matches_scalar_bits_and_worst_score() {
    for dim in [1, 3, 4, 7, 129] {
        let directory = tempdir().expect("scalar fixture");
        let controller = VectorFaultController::observe_only(11);
        let store = Store::open_with_test_dependencies(
            directory.path(),
            OpenOptions::default(),
            vector_dependencies(controller.clone()),
        )
        .expect("open scalar fixture");
        let query = (0..dim)
            .map(|j| (j as f32 - 1.5) * 0.003)
            .collect::<Vec<_>>();
        let mut rows = Vec::new();
        for group in 0..3 {
            let mut documents = Vec::new();
            for i in 0..9 {
                let id = (group * 9 + i + 1) as u128;
                let vector = (0..dim)
                    .map(|j| (((id as usize * 13 + j * 7) % 47) as f32 - 23.0) * 0.03125)
                    .collect::<Vec<_>>();
                rows.push((id, vector.clone(), group));
                documents.push(IngestDocument::new(
                    DocumentVersion::new(DocId::new(id), Revision::new(1)),
                    vector,
                ));
            }
            store
                .ingest(IngestBatch::new(documents))
                .expect("scalar rows");
            if group < 2 {
                store.seal().expect("scalar seal");
            }
        }
        store
            .delete(zeppelin_embed::ingest::DeleteBatch::new(vec![
                DocId::new(2),
                DocId::new(12),
                DocId::new(25),
            ]))
            .expect("scalar tombstones");
        rows.retain(|(id, _, _)| ![2, 12, 25].contains(id));
        let score = |row: &[f32]| {
            let mut sum = 0_f64;
            for (&q, &v) in query.iter().zip(row) {
                let d = f64::from(q) - f64::from(v);
                sum += d * d;
            }
            -sum as f32
        };
        let mut expected = rows
            .iter()
            .map(|(id, v, _)| (*id, score(v)))
            .collect::<Vec<_>>();
        expected.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        for k in [1, 7, 23, 24, 30] {
            let actual = astra_04_run(&store, &query, k).expect("scalar exact");
            assert_eq!(
                actual
                    .candidates
                    .iter()
                    .map(|c| (
                        c.document().expect("identity").doc_id().get(),
                        c.score().to_bits()
                    ))
                    .collect::<Vec<_>>(),
                expected
                    .iter()
                    .take(k)
                    .map(|(id, s)| (*id, s.to_bits()))
                    .collect::<Vec<_>>()
            );
            assert_eq!(actual.stats.dims_touched, 24 * dim as u64);
            assert_eq!(actual.stats.bytes_read, 24 * dim as u64 * 4);
            let mut worst = controller
                .take_exact_scan_work()
                .iter()
                .filter_map(|w| w.worst_score)
                .map(f32::to_bits)
                .collect::<Vec<_>>();
            let mut expected_worst = (0..3)
                .map(|group| {
                    rows.iter()
                        .filter(|r| r.2 == group)
                        .map(|r| score(&r.1))
                        .min_by(f32::total_cmp)
                        .expect("live group")
                        .to_bits()
                })
                .collect::<Vec<_>>();
            worst.sort_unstable();
            expected_worst.sort_unstable();
            assert_eq!(worst, expected_worst);
        }
        store
            .delete(zeppelin_embed::ingest::DeleteBatch::new(
                rows.iter().map(|r| DocId::new(r.0)).collect(),
            ))
            .expect("delete all");
        let empty = astra_04_run(&store, &query, 7).expect("empty alive query");
        assert!(empty.candidates.is_empty());
        assert_eq!(empty.stats.dims_touched, 0);
    }
}

#[test]
fn astra_04_exact_scan_preserves_cancel_and_nonfinite_precedence() {
    let directory = tempdir().expect("precedence fixture");
    let controller = VectorFaultController::armed(
        VectorFault::CancelAfterRows {
            source: VectorRowSource::Active,
            requested_rows: 2,
            tier: VectorSearchTier::Exact,
        },
        11,
    );
    let store = Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::default(),
        vector_dependencies(controller.clone()),
    )
    .expect("open precedence fixture");
    ingest_rows(&store, &[(1, [0.0; 3]), (2, [1.0; 3]), (3, [2.0; 3])]);
    let huge = [1.0e20_f32; 3];
    assert!(matches!(
        astra_04_run(&store, &huge, 1),
        Err(QueryError::Cancelled { partial: false })
    ));
    let receipts = controller.take_typed_receipts();
    assert_eq!(receipts.len(), 1);
    assert!(!receipts[0].result_published());
    assert!(matches!(
        receipts[0].effect(),
        VectorFaultEffect::CancelledAfterRows {
            requested_rows: 2,
            observed_rows: 2,
            ..
        }
    ));
    assert!(matches!(
        astra_04_run(&store, &huge, 1),
        Err(QueryError::Scan(ScanError::NonFiniteScore { row_id: 0 }))
    ));
    let retry = astra_04_run(&store, &[0.0; 3], 3).expect("finite retry");
    let clean_dir = tempdir().expect("same-seed clean");
    let clean = Store::open_with_test_dependencies(
        clean_dir.path(),
        OpenOptions::default(),
        vector_dependencies(VectorFaultController::observe_only(11)),
    )
    .expect("clean");
    ingest_rows(&clean, &[(1, [0.0; 3]), (2, [1.0; 3]), (3, [2.0; 3])]);
    assert_same_result(
        &astra_04_run(&clean, &[0.0; 3], 3).expect("clean result"),
        &retry,
    );
}

#[test]
#[ignore = "paired process exact selection measurement"]
fn astra_04_exact_scan_measurement() {
    for (n, k, tied) in [
        (1024, 10, false),
        (8192, 10, false),
        (60000, 10, false),
        (8192, 4096, false),
        (8192, 8192, false),
        (8192, 10, true),
    ] {
        let dir = tempdir().expect("measurement fixture");
        let store = Store::open(dir.path(), OpenOptions::default()).expect("measurement Store");
        let documents = (0..n)
            .map(|row| {
                IngestDocument::new(
                    DocumentVersion::new(DocId::new((n - row) as u128), Revision::new(1)),
                    (0..16)
                        .map(|j| {
                            if tied {
                                1.0
                            } else {
                                ((row * 31 + j * 17) % 65521) as f32 * 0.001
                            }
                        })
                        .collect(),
                )
            })
            .collect();
        store
            .ingest(IngestBatch::new(documents))
            .expect("measurement ingest");
        let query = [0.125_f32; 16];
        let mut oracle = (0..n)
            .map(|row| {
                let mut distance = 0_f64;
                for j in 0..16 {
                    let v = if tied {
                        1.0
                    } else {
                        ((row * 31 + j * 17) % 65521) as f32 * 0.001
                    };
                    let delta = f64::from(query[j]) - f64::from(v);
                    distance += delta * delta;
                }
                ((n - row) as u128, (-distance as f32).to_bits())
            })
            .collect::<Vec<_>>();
        oracle.sort_unstable_by(|a, b| {
            f32::from_bits(b.1)
                .total_cmp(&f32::from_bits(a.1))
                .then(a.0.cmp(&b.0))
        });
        oracle.truncate(k);
        let mut timings = Vec::new();
        for iteration in 0..84 {
            let started = std::time::Instant::now();
            let actual = astra_04_run(&store, &query, k).expect("measured exact query");
            let micros = started.elapsed().as_secs_f64() * 1e6;
            assert_eq!(actual.stats.dims_touched, n as u64 * 16);
            assert_eq!(actual.stats.bytes_read, n as u64 * 64);
            assert_eq!(
                actual
                    .candidates
                    .iter()
                    .map(|c| (
                        c.document().expect("identity").doc_id().get(),
                        c.score().to_bits()
                    ))
                    .collect::<Vec<_>>(),
                oracle
            );
            if iteration >= 20 {
                timings.push(micros);
            }
        }
        timings.sort_by(f64::total_cmp);
        eprintln!(
            "ASTRA04 n={n} k={k} tied={tied} p50_us={} p95_us={} samples={timings:?}",
            timings[31], timings[60]
        );
    }
}

#[test]
fn astra_05_large_exact_scan_executes_disjoint_worker_partitions() {
    let capacity = zeppelin_embed::scan::physical_thread_capacity().expect("worker capacity");
    let workers = capacity.min(4);
    assert!(
        workers >= 2,
        "this directed worker test needs a multi-core host"
    );
    let n = 16_385_usize;
    let dim = 129_usize;
    let directory = tempdir().expect("exact worker fixture");
    let controller = VectorFaultController::trace_exact_partitions(11);
    let store = Store::open_with_test_dependencies(
        directory.path(),
        OpenOptions::default(),
        vector_dependencies(controller.clone()),
    )
    .expect("open exact worker fixture");
    store
        .ingest(IngestBatch::new(
            (0..n)
                .map(|row| {
                    let mut vector = vec![0.03125; dim];
                    vector[0] = row as f32 * 0.0009765625;
                    IngestDocument::new(
                        DocumentVersion::new(DocId::new(row as u128 + 1), Revision::new(1)),
                        vector,
                    )
                })
                .collect(),
        ))
        .expect("worker rows");
    let deleted = (0..n)
        .step_by(4096)
        .map(|row| DocId::new(row as u128 + 1))
        .collect::<Vec<_>>();
    let live = n - deleted.len();
    store
        .delete(zeppelin_embed::ingest::DeleteBatch::new(deleted))
        .expect("worker tombstones");
    let query = vec![0.0_f32; dim];
    let outcome = store
        .search(
            SearchRequest::new(&query),
            10,
            SearchOptions::new(ScanOptions {
                thread_budget: workers,
            })
            .with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("large exact query");
    assert_eq!(outcome.stats.dims_touched, (live * dim) as u64);
    assert_eq!(outcome.stats.bytes_read, (live * dim * 4) as u64);
    let work = controller.take_exact_scan_work();
    assert_eq!(
        work.iter().map(|work| work.scored_rows).sum::<usize>(),
        live
    );
    assert!(
        store.stats().expect("pool stats").query_pool_bytes > 0,
        "active admission already creates a pool"
    );
    assert_eq!(
        outcome.stats.threads_used, workers,
        "large Exact still ran on the caller despite its admitted query pool"
    );
    assert_eq!(outcome.stats.worker_thread_ids.len(), workers);
    assert!(
        !outcome
            .stats
            .worker_thread_ids
            .contains(&std::thread::current().id())
    );
    assert_eq!(work.len(), workers);
    let partitions = controller.take_exact_partitions();
    assert_eq!(partitions.len(), workers);
    let mut seen = BTreeSet::new();
    for partition in partitions {
        assert!(
            outcome
                .stats
                .worker_thread_ids
                .contains(&partition.thread_id)
        );
        for row in partition.checked_rows {
            assert!(partition.range.contains(&row));
            assert_ne!(row % 4096, 0, "tombstone scored");
            assert!(seen.insert(row), "eligible row scored by two workers");
        }
    }
    assert_eq!(seen, (0..n).filter(|row| row % 4096 != 0).collect());
}

fn astra_05_query(
    store: &Store,
    query: &[f32],
    k: usize,
    workers: usize,
    token: CancelToken,
) -> Result<SearchOutcome, QueryError> {
    store.search(
        SearchRequest::new(query),
        k,
        SearchOptions::new(ScanOptions {
            thread_budget: workers,
        })
        .with_tier(SearchTier::Exact),
        QueryControl::Cancel(token),
    )
}

fn astra_05_fixture(controller: VectorFaultController) -> (TempDir, Arc<Store>, Vec<f32>) {
    astra_05_fixture_with_clock(controller, Arc::new(SystemMonotonicClock))
}

#[test]
fn astra_05_exact_pool_capacity_is_shared_across_callers() {
    let (_directory, store, query) = astra_05_fixture(VectorFaultController::observe_only(11));
    let expected =
        astra_05_query(&store, &query, 7, 1, CancelToken::new()).expect("serial capacity oracle");
    let start = std::sync::Barrier::new(4);
    let outcomes = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let (store, query, start) = (&store, &query, &start);
                scope.spawn(move || {
                    start.wait();
                    astra_05_query(store, query, 7, 4, CancelToken::new())
                        .expect("concurrent exact query")
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("caller joins"))
            .collect::<Vec<_>>()
    });
    let mut threads = std::collections::HashSet::new();
    for outcome in outcomes {
        assert_same_result(&expected, &outcome);
        assert_eq!(outcome.stats.threads_used, 4);
        threads.extend(outcome.stats.worker_thread_ids);
    }
    assert_eq!(
        threads.len(),
        4,
        "Exact callers share the measured four-worker capacity"
    );
    assert_eq!(
        store
            .stats()
            .expect("all query scratch released")
            .temporary_bytes,
        0
    );
}

fn astra_05_fixture_with_clock(
    controller: VectorFaultController,
    clock: Arc<dyn zeppelin_embed::lifecycle::MonotonicClock>,
) -> (TempDir, Arc<Store>, Vec<f32>) {
    let dir = tempdir().expect("exact lifecycle fixture");
    let store = Arc::new(
        Store::open_with_test_dependencies(
            dir.path(),
            OpenOptions::default(),
            StoreTestDependencies::new(Arc::new(StdVfs), clock)
                .with_vector_fault_controller(controller),
        )
        .expect("open exact lifecycle fixture"),
    );
    store
        .ingest(IngestBatch::new(
            (0..2049)
                .map(|row| {
                    let mut vector = vec![0.03125; 129];
                    vector[0] = row as f32 * 0.0009765625;
                    IngestDocument::new(
                        DocumentVersion::new(DocId::new(row as u128 + 1), Revision::new(1)),
                        vector,
                    )
                })
                .collect(),
        ))
        .expect("lifecycle rows");
    (dir, store, vec![0.0; 129])
}

#[test]
fn astra_05_parallel_exact_matches_serial_bits_and_identity_ties() {
    let dir = tempdir().expect("parallel ties");
    let controller = VectorFaultController::trace_exact_partitions(11);
    let store = Store::open_with_test_dependencies(
        dir.path(),
        OpenOptions::default(),
        vector_dependencies(controller.clone()),
    )
    .expect("parallel ties Store");
    for base in [20_000_u128, 10_000, 0] {
        store
            .ingest(IngestBatch::new(
                (1..=2049)
                    .rev()
                    .map(|id| {
                        let mut vector = vec![0.0_f32; 129];
                        vector[0] = 1.0;
                        if id % 2 == 0 {
                            vector[1] = 1.0_f32 / 8192.0;
                        }
                        IngestDocument::new(
                            DocumentVersion::new(DocId::new(base + id), Revision::new(1)),
                            vector,
                        )
                    })
                    .collect(),
            ))
            .expect("tie source");
        if base != 0 {
            store.seal().expect("tie seal");
        }
    }
    store
        .delete(zeppelin_embed::ingest::DeleteBatch::new(vec![
            DocId::new(1),
            DocId::new(10_001),
            DocId::new(20_001),
        ]))
        .expect("tie tombstones");
    let query = vec![0.0_f32; 129];
    let serial = astra_05_query(&store, &query, 7, 1, CancelToken::new()).expect("serial ties");
    let _ = controller.take_exact_partitions();
    let parallel = astra_05_query(&store, &query, 7, 4, CancelToken::new()).expect("parallel ties");
    assert_eq!(
        parallel.stats.threads_used, 4,
        "one worker lane must be reused across segments"
    );
    assert_same_result(&serial, &parallel);
    assert_eq!(serial.generation, parallel.generation);
    assert_eq!(
        result_documents(&parallel)
            .iter()
            .map(|v| v.doc_id().get())
            .collect::<Vec<_>>(),
        (2..=8).collect::<Vec<_>>()
    );
    assert!(
        parallel
            .candidates
            .iter()
            .all(|c| c.score().to_bits() == (-1.0_f32).to_bits())
    );
    assert_eq!(parallel.stats.dims_touched, 3 * 2048 * 129);
    let receipts = controller.take_exact_partitions();
    assert_eq!(receipts.len(), 12);
    assert_eq!(
        receipts.iter().map(|r| r.checked_rows.len()).sum::<usize>(),
        3 * 2048
    );
    assert_eq!(
        store
            .stats()
            .expect("released parallel scratch")
            .temporary_bytes,
        0
    );
}

#[test]
fn astra_05_parallel_narrowing_error_uses_global_f64_order() {
    let dir = tempdir().expect("parallel overflow");
    let store = Store::open(dir.path(), OpenOptions::default()).expect("overflow Store");
    store
        .ingest(IngestBatch::new(
            (0..2049)
                .map(|row| {
                    let mut vector = vec![0.03125_f32; 129];
                    vector[0] = row as f32 * 1.0e9;
                    IngestDocument::new(
                        DocumentVersion::new(DocId::new(row as u128 + 1), Revision::new(1)),
                        vector,
                    )
                })
                .collect(),
        ))
        .expect("finite large rows");
    for workers in [1, 4] {
        let error = astra_05_query(&store, &[1.0e20_f32; 129], 7, workers, CancelToken::new())
            .expect_err("f32 overflow");
        assert!(
            matches!(
                error,
                QueryError::Scan(ScanError::NonFiniteScore { row_id: 2048 })
            ),
            "best f64 overflow is in the last partition: {error:?}"
        );
        assert_eq!(
            store
                .stats()
                .expect("overflow releases scratch")
                .temporary_bytes,
            0
        );
    }
}

#[test]
fn astra_05_exact_pool_reservation_failure_leaves_store_reusable() {
    let controller = VectorFaultController::observe_only(11);
    let (dir, store, query) = astra_05_fixture(controller.clone());
    let control = astra_05_query(&store, &query, 7, 1, CancelToken::new()).expect("clean serial");
    let before = store.stats().expect("before refusal");
    controller.deny_next_exact_reservation();
    assert!(matches!(
        astra_05_query(&store, &query, 7, 4, CancelToken::new()),
        Err(QueryError::Store(StoreError::AllocationFailed {
            component: "exact query execution",
            ..
        }))
    ));
    assert_eq!(controller.take_exact_reservation_denials(), 1);
    assert!(
        controller.take_exact_worker_timings().is_empty(),
        "refused execution must not submit workers"
    );
    let after = store.stats().expect("after refusal");
    assert_eq!(after.temporary_bytes, 0);
    assert_eq!(after.resident_owned_bytes, before.resident_owned_bytes);
    assert_eq!(after.active_queries, 0);
    let retry =
        astra_05_query(&store, &query, 7, 4, CancelToken::new()).expect("retry after refusal");
    assert_same_result(&control, &retry);
    assert_eq!(retry.stats.threads_used, 4);
    assert_eq!(store.stats().expect("retry scratch").temporary_bytes, 0);

    store.seal().expect("seal budget fixture");
    store.close().expect("close unlimited fixture");
    let limited = Store::open(dir.path(), OpenOptions::default().with_max_temp_bytes(1024))
        .expect("open under real temporary ceiling");
    let serial = astra_05_query(&limited, &query, 7, 1, CancelToken::new())
        .expect("serial collector fits ceiling");
    let before = limited.stats().expect("budget baseline");
    let result = astra_05_query(&limited, &query, 7, 4, CancelToken::new());
    assert!(
        matches!(
            result,
            Err(QueryError::Store(StoreError::BudgetExceeded {
                budget: 1024,
                component: "temporary",
                ..
            }))
        ),
        "parallel scratch must respect actual ceiling: {result:?}"
    );
    let after = limited.stats().expect("released refused query");
    assert_eq!(after.temporary_bytes, 0);
    assert_eq!(after.resident_owned_bytes, before.resident_owned_bytes);
    assert_eq!(after.active_queries, 0);
    assert_same_result(
        &serial,
        &astra_05_query(&limited, &query, 7, 1, CancelToken::new())
            .expect("serial retry after real budget refusal"),
    );
}

#[derive(Default)]
struct Astra05Gate {
    open: std::sync::Mutex<bool>,
    changed: std::sync::Condvar,
}
impl Astra05Gate {
    fn wait(&self) {
        let mut open = self.open.lock().expect("gate lock");
        while !*open {
            open = self.changed.wait(open).expect("gate wait");
        }
    }
    fn release(&self) {
        *self.open.lock().expect("release lock") = true;
        self.changed.notify_all();
    }
}
struct Astra05Gates([Arc<Astra05Gate>; 2]);
impl Drop for Astra05Gates {
    fn drop(&mut self) {
        for gate in &self.0 {
            gate.release();
        }
    }
}

#[test]
fn astra_05_exact_worker_cancel_close_and_dual_failure_join_cleanly() {
    use std::sync::mpsc;
    use std::time::Duration;
    use zeppelin_embed::scan::vector_fault::ExactWorkerFault;
    // Each release order is driven by actual worker completion notifications.
    for reverse in [false, true] {
        for case in [
            "cancel",
            "deadline",
            "close",
            "dual",
            "panic",
            "caller_unwind",
        ] {
            let controller = VectorFaultController::observe_only(11);
            let clock = Arc::new(zeppelin_embed::lifecycle::ManualMonotonicClock::new());
            let (dir, store, query) =
                astra_05_fixture_with_clock(controller.clone(), clock.clone());
            let expected = astra_05_query(&store, &query, 7, 1, CancelToken::new())
                .expect("same-seed clean control");
            let gates = Astra05Gates(std::array::from_fn(|_| Arc::new(Astra05Gate::default())));
            let worker_gates = gates.0.clone();
            let (started_tx, started_rx) = mpsc::channel();
            let (completed_tx, completed_rx) = mpsc::channel();
            controller.notify_exact_completions(completed_tx);
            controller.set_exact_worker_hook(move |slot| {
                started_tx.send(slot).expect("worker entered");
                worker_gates[slot].wait();
                match case {
                    "dual" => ExactWorkerFault::NonFinite {
                        row_id: if slot == 0 { 111 } else { 1500 },
                    },
                    "panic" if slot == 0 => ExactWorkerFault::Panic,
                    "panic" => ExactWorkerFault::NonFinite { row_id: 1500 },
                    _ => ExactWorkerFault::None,
                }
            });
            if case == "caller_unwind" {
                controller.panic_after_exact_submission();
            }
            let token = CancelToken::new();
            let control = if case == "deadline" {
                QueryControl::Deadline(
                    zeppelin_embed::lifecycle::Deadline::after_with_test_clock(
                        Duration::from_secs(60),
                        clock.clone(),
                    )
                    .expect("manual deadline"),
                )
            } else {
                QueryControl::Cancel(token.clone())
            };
            let querying_store = Arc::clone(&store);
            let querying_vector = query.clone();
            let (result_tx, result_rx) = mpsc::channel();
            let querying = std::thread::spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    querying_store.search(
                        SearchRequest::new(&querying_vector),
                        7,
                        SearchOptions::new(ScanOptions { thread_budget: 2 })
                            .with_tier(SearchTier::Exact),
                        control,
                    )
                }));
                result_tx.send(result).expect("deliver joined query result");
            });
            let entered = [
                started_rx
                    .recv_timeout(Duration::from_secs(10))
                    .expect("first worker enters"),
                started_rx
                    .recv_timeout(Duration::from_secs(10))
                    .expect("second worker enters"),
            ];
            assert_eq!(
                entered.into_iter().collect::<BTreeSet<_>>(),
                BTreeSet::from([0, 1])
            );
            let closing = if case == "close" {
                let lease = store.snapshot().expect("observe close cancellation");
                let closing_store = Arc::clone(&store);
                let closing = std::thread::spawn(move || closing_store.close());
                lease
                    .wait_for_close_cancellation()
                    .expect("close has cancelled the pinned snapshot");
                drop(lease);
                Some(closing)
            } else {
                None
            };
            if case == "cancel" {
                token.cancel();
            }
            if case == "deadline" {
                clock.advance(Duration::from_secs(120));
            }
            let order = if reverse { [1, 0] } else { [0, 1] };
            gates.0[order[0]].release();
            assert_eq!(
                completed_rx
                    .recv_timeout(Duration::from_secs(10))
                    .expect("first actual completion"),
                order[0]
            );
            assert!(
                matches!(result_rx.try_recv(), Err(mpsc::TryRecvError::Empty)),
                "{case}: returned while second worker still owns a borrowed request"
            );
            gates.0[order[1]].release();
            assert_eq!(
                completed_rx
                    .recv_timeout(Duration::from_secs(10))
                    .expect("second actual completion"),
                order[1]
            );
            let result = result_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("joined query returns");
            querying.join().expect("query driver joins");
            match case {
                "caller_unwind" => assert!(result.is_err(), "caller panic plant did not fire"),
                "cancel" => assert!(matches!(
                    result.expect("no caller panic"),
                    Err(QueryError::Cancelled { partial: false })
                )),
                "deadline" => assert!(matches!(
                    result.expect("no caller panic"),
                    Err(QueryError::Timeout { partial: false })
                )),
                "close" => assert!(matches!(
                    result.expect("no caller panic"),
                    Err(QueryError::ReadCancelled { partial: false })
                )),
                "dual" => assert!(matches!(
                    result.expect("no caller panic"),
                    Err(QueryError::Scan(ScanError::NonFiniteScore { row_id: 111 }))
                )),
                "panic" => assert!(matches!(
                    result.expect("no caller panic"),
                    Err(QueryError::Scan(ScanError::WorkerPanicked))
                )),
                _ => unreachable!(),
            }
            controller.clear_exact_worker_hook();
            if let Some(closing) = closing {
                closing
                    .join()
                    .expect("close thread joins")
                    .expect("close completes");
                assert!(
                    matches!(store.stats(), Err(StoreError::Closed)),
                    "closed handles reject statistics"
                );
                let reopened =
                    Store::open(dir.path(), OpenOptions::default()).expect("reopen after close");
                assert_same_result(
                    &expected,
                    &astra_05_query(&reopened, &query, 7, 2, CancelToken::new())
                        .expect("closed-case clean retry"),
                );
            } else {
                assert_eq!(
                    store.stats().expect("joined error scratch").temporary_bytes,
                    0
                );
                assert_eq!(store.stats().expect("query released").active_queries, 0);
                let retry = astra_05_query(&store, &query, 7, 2, CancelToken::new())
                    .expect("same-seed clean retry");
                assert_same_result(&expected, &retry);
                assert_eq!(retry.stats.threads_used, 2);
            }
        }
    }
}

#[test]
fn astra_05_exact_hybrid_widening_admits_required_workers() {
    use zeppelin_embed::epoch::{
        ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, Normalization, StoreEpoch,
    };
    use zeppelin_embed::fts::index::DEFAULT_FIELD;
    use zeppelin_embed::fts::search::TermQuery;
    use zeppelin_embed::fts::tokenizer::TokenizerConfig;
    use zeppelin_embed::fusion::HybridQuery;
    use zeppelin_embed::tier::{MaintenanceBudget, MaintenanceStatus, TierThresholds};
    for (n, dim, graph, traverses_first) in [
        (2049, 129, false, false),
        (256, 1024, true, false),
        (1024, 256, true, true),
    ] {
        let dir = tempdir().expect("hybrid worker fixture");
        let tower = EmbeddingTower {
            model_id: "astra-05-worker-fixture".to_owned(),
            model_version: "1".to_owned(),
            weights_digest: vec![11],
            dims: dim as u32,
            normalization: Normalization::None,
            prompt_prefix: String::new(),
            max_tokens: 32,
            runtime: EmbeddingRuntime::CpuReference,
            compute_units: ComputeUnits::Cpu,
            os_build: None,
        };
        let epoch = StoreEpoch {
            embedding: EmbeddingEpoch {
                query: tower.clone(),
                document: tower,
                alignment_digest: Vec::new(),
            },
            tokenizer: TokenizerConfig::text_default().epoch(),
        };
        let options = OpenOptions::default().with_epoch(epoch.clone());
        let fixture = Store::open(dir.path(), options.clone()).expect("fixture Store");
        fixture
            .ingest(
                IngestBatch::new(
                    (0..n)
                        .map(|row| {
                            let mut vector = vec![0.03125_f32; dim];
                            vector[0] = row as f32 * 0.00390625;
                            IngestDocument::new(
                                DocumentVersion::new(DocId::new(row as u128 + 1), Revision::new(1)),
                                vector,
                            )
                            .with_text("alpha")
                        })
                        .collect(),
                )
                .with_epoch(epoch.identity()),
            )
            .expect("hybrid fixture rows");
        fixture.seal().expect("sealed hybrid fixture");
        if graph {
            let report = fixture.maintain_with_test_thresholds(
                MaintenanceBudget {
                    wall_time: std::time::Duration::from_secs(120),
                    bytes: u64::MAX,
                },
                TierThresholds { graph_min_rows: 32 },
            );
            assert!(matches!(report.status, MaintenanceStatus::Complete));
            assert_eq!(report.graphs_built, 1);
        }
        fixture.close().expect("close prebuilt fixture");
        let controller = VectorFaultController::trace_exact_partitions(11);
        let store = Store::open_with_test_dependencies(
            dir.path(),
            options,
            vector_dependencies(controller.clone()),
        )
        .expect("fresh hybrid admission");
        assert_eq!(store.stats().expect("no prior pool").query_pool_bytes, 0);
        let result = store
            .search_hybrid(
                SearchRequest::new(&vec![0.0_f32; dim]),
                &TermQuery::flat(vec![b"absent".to_vec()], &[DEFAULT_FIELD]),
                &HybridQuery::new(1),
                SearchOptions::new(ScanOptions { thread_budget: 4 }),
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect("hybrid exact dispatch");
        if traverses_first {
            // Plan 01 ends ANN rounds as approximate; it does not widen them.
            assert_eq!(result.diagnostics.counters.scan.threads_used, 1);
            assert!(result.diagnostics.counters.scan.dims_touched < (n * dim) as u64);
            assert!(result.diagnostics.counters.scan.bytes_read < (n * dim * 4) as u64);
            assert_eq!(result.diagnostics.counters.graph.segments_traversed, 1);
            assert_eq!(
                result
                    .diagnostics
                    .fusion
                    .as_ref()
                    .expect("fusion report")
                    .rounds,
                1
            );
            assert_eq!(
                result
                    .diagnostics
                    .fusion
                    .as_ref()
                    .expect("fusion report")
                    .termination,
                zeppelin_embed::fusion::FusionTermination::ApproximateCandidates
            );
        } else {
            assert_eq!(result.diagnostics.counters.scan.threads_used, 4);
            assert_eq!(
                result.diagnostics.counters.scan.dims_touched,
                (n * dim) as u64
            );
            assert_eq!(
                result.diagnostics.counters.scan.bytes_read,
                (n * dim * 4) as u64
            );
        }
        assert!(store.stats().expect("pool admitted").query_pool_bytes > 0);
        assert_eq!(result.hits[0].key, DocId::new(1));
        let partitions = controller.take_exact_partitions();
        assert_eq!(partitions.len(), if traverses_first { 0 } else { 4 });
        assert_eq!(
            partitions
                .iter()
                .map(|p| p.checked_rows.len())
                .sum::<usize>(),
            if traverses_first { 0 } else { n }
        );
        if graph && !traverses_first {
            assert!(result.diagnostics.plan.iter().any(|plan| matches!(
                plan.scan_reason,
                Some(zeppelin_embed::planner::ScanReason::WideningCap { .. })
            )));
            if !traverses_first {
                // The smaller graph reaches its cap in the first request.
                assert_eq!(result.diagnostics.counters.graph.segments_traversed, 0);
            }
        }
        assert_eq!(
            store
                .stats()
                .expect("hybrid scratch released")
                .temporary_bytes,
            0
        );
    }
}

#[test]
#[ignore = "release-only FiQA worker-capacity screen; requires ASTRA05_STORE"]
fn astra_05_exact_pool_measurement() {
    use std::time::Instant;
    let path = std::env::var("ASTRA05_STORE").expect("explicit corrected store");
    let workers: usize = std::env::var("ASTRA05_WORKERS")
        .expect("workers")
        .parse()
        .expect("worker integer");
    let callers: usize = std::env::var("ASTRA05_CALLERS")
        .expect("callers")
        .parse()
        .expect("caller integer");
    assert!((1..=4).contains(&callers));
    let controller = VectorFaultController::observe_only(11);
    let manifest_path = Path::new(&path).join("manifest.ze");
    let manifest = zeppelin_embed::manifest::decode_manifest(
        &manifest_path.display().to_string(),
        &std::fs::read(&manifest_path).expect("read immutable epoch declaration"),
    )
    .expect("checked manifest");
    let alias = manifest.epoch_alias.expect("persisted epoch alias");
    assert_eq!(alias.embedding.value(), 7508391831206249002);
    assert_eq!(alias.tokenizer.value(), 1035315901113778624);
    let declared = manifest
        .epochs
        .iter()
        .find(|epoch| epoch.id == alias.embedding)
        .expect("full persisted embedding declaration");
    let declared = zeppelin_embed::epoch::StoreEpoch {
        embedding: declared.embedding.clone(),
        tokenizer: declared.tokenizer,
    };
    assert_eq!(declared.identity(), alias);
    let store = Store::open_with_test_dependencies(
        &path,
        OpenOptions::read_only()
            .with_epoch(declared)
            .with_tokenizer(zeppelin_embed::fts::tokenizer::TokenizerConfig::text_default()),
        vector_dependencies(controller.clone()),
    )
    .expect("open corrected FiQA");
    let epoch = store.epoch_identity().expect("stamped store");
    assert_eq!(epoch.embedding.value(), 7508391831206249002);
    assert_eq!(epoch.tokenizer.value(), 1035315901113778624);
    let snapshot = store.snapshot().expect("oracle snapshot");
    assert_eq!(snapshot.segments().len(), 1);
    let segment = &snapshot.segments()[0];
    assert_eq!(segment.meta().row_count, 58980);
    assert_eq!(segment.meta().dims, 768);
    assert!(!segment.directory().iter().any(
        |entry| entry.kind == zeppelin_embed::segment::layout::RegionKind::GraphNodeBlocks.id()
    ));
    let rows = segment
        .rescore_f32()
        .expect("checked full-precision vectors");
    assert_eq!(rows.len(), 58980 * 768);
    let alive = segment.alive().expect("live mask");
    assert!((0..58980).all(|row| alive.is_alive(row)));
    let queries: Vec<_> = (0..8)
        .map(|index| rows[index * 7372 * 768..(index * 7372 + 1) * 768].to_vec())
        .collect();
    let versions: Vec<_> = (0..58980)
        .map(|row| {
            segment
                .document_version(row)
                .expect("valid identity")
                .expect("present identity")
        })
        .collect();
    let expected: Vec<_> = queries
        .iter()
        .map(|query| {
            let mut scores: Vec<_> = rows
                .chunks_exact(768)
                .enumerate()
                .map(|(row, vector)| {
                    let mut distance = 0.0_f64;
                    for (q, v) in query.iter().zip(vector) {
                        let delta = f64::from(*q) - f64::from(*v);
                        distance += delta * delta;
                    }
                    (versions[row], (-distance as f32).to_bits())
                })
                .collect();
            scores.sort_unstable_by(|a, b| {
                f32::from_bits(b.1)
                    .total_cmp(&f32::from_bits(a.1))
                    .then(a.0.cmp(&b.0))
            });
            scores.truncate(10);
            scores
        })
        .collect();
    drop(snapshot);
    for iteration in 0..20 {
        astra_05_query(
            &store,
            &queries[iteration % 8],
            10,
            workers,
            CancelToken::new(),
        )
        .expect("warm query");
    }
    let _ = controller.take_exact_worker_timings();
    let _ = controller.take_exact_scan_work();
    let start = std::sync::Barrier::new(callers + 1);
    let (elapsed, outputs) = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for caller in 0..callers {
            let (store, queries, expected, start) = (&store, &queries, &expected, &start);
            handles.push(scope.spawn(move || {
                let mut samples = Vec::new();
                let mut ids = std::collections::HashSet::new();
                start.wait();
                for iteration in 0..64 {
                    let index = (iteration + caller) % 8;
                    let began = Instant::now();
                    let result =
                        astra_05_query(store, &queries[index], 10, workers, CancelToken::new())
                            .expect("measured exact query");
                    samples.push(began.elapsed().as_secs_f64() * 1e6);
                    assert_eq!(result.stats.dims_touched, 45_296_640);
                    assert_eq!(result.stats.bytes_read, 181_186_560);
                    assert_eq!(
                        result.stats.threads_used,
                        if workers == 0 { 4 } else { workers }
                    );
                    ids.extend(result.stats.worker_thread_ids);
                    let signature: Vec<_> = result
                        .candidates
                        .iter()
                        .map(|hit| {
                            (
                                hit.document().expect("returned identity"),
                                hit.score().to_bits(),
                            )
                        })
                        .collect();
                    assert_eq!(
                        signature, expected[index],
                        "independent scalar identity/bit oracle"
                    );
                }
                (samples, ids)
            }));
        }
        let began = Instant::now();
        start.wait();
        let outputs: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("measurement caller joins"))
            .collect();
        (began.elapsed().as_secs_f64(), outputs)
    });
    let mut samples = Vec::new();
    let mut ids = std::collections::HashSet::new();
    for (times, threads) in outputs {
        samples.extend(times);
        ids.extend(threads);
    }
    let timings = controller.take_exact_worker_timings();
    let work = controller.take_exact_scan_work();
    assert_eq!(
        work.iter().map(|work| work.scored_rows).sum::<usize>(),
        callers * 64 * 58980
    );
    assert_eq!(store.stats().expect("final accounting").temporary_bytes, 0);
    let queue_us: Vec<_> = timings
        .iter()
        .map(|t| t.queue_wait.as_secs_f64() * 1e6)
        .collect();
    let execution_us: Vec<_> = timings
        .iter()
        .map(|t| t.execution.as_secs_f64() * 1e6)
        .collect();
    let collector_bytes: Vec<_> = work
        .iter()
        .map(|w| w.collector_capacity * std::mem::size_of::<zeppelin_embed::scan::ScanCandidate>())
        .collect();
    eprintln!(
        "ASTRA05 workers={workers} callers={callers} elapsed_s={elapsed} unique_threads={} samples_us={samples:?} queue_us={queue_us:?} execution_us={execution_us:?} collector_bytes={collector_bytes:?}",
        ids.len()
    );
}

#[test]
#[ignore = "release-only exact crossover and all-tie collector screen"]
fn astra_05_exact_crossover_measurement() {
    let workers: usize = std::env::var("ASTRA05_WORKERS")
        .expect("explicit measured worker count")
        .parse()
        .expect("worker integer");
    for (n, dim, tied) in [
        (1024, 16, false),
        (128, 2048, false),
        (256, 1024, false),
        (2048, 128, false),
        (4096, 128, false),
        (16384, 16, false),
        (8192, 16, true),
        (4096, 129, true),
    ] {
        let directory = tempdir().expect("crossover fixture");
        let controller = VectorFaultController::observe_only(11);
        let store = Store::open_with_test_dependencies(
            directory.path(),
            OpenOptions::default(),
            vector_dependencies(controller.clone()),
        )
        .expect("crossover store");
        let component = |row: usize, j: usize| {
            if tied {
                1.0_f32
            } else {
                ((row * 31 + j * 17) % 65521) as f32 * 0.001
            }
        };
        let documents = (0..n)
            .map(|row| {
                IngestDocument::new(
                    DocumentVersion::new(DocId::new((n - row) as u128), Revision::new(1)),
                    (0..dim).map(|j| component(row, j)).collect(),
                )
            })
            .collect();
        store
            .ingest(IngestBatch::new(documents))
            .expect("crossover ingest");
        let query = vec![0.125_f32; dim];
        let mut oracle: Vec<_> = (0..n)
            .map(|row| {
                let mut distance = 0.0_f64;
                for (j, q) in query.iter().enumerate() {
                    let delta = f64::from(*q) - f64::from(component(row, j));
                    distance += delta * delta;
                }
                ((n - row) as u128, (-distance as f32).to_bits())
            })
            .collect();
        oracle.sort_unstable_by(|a, b| {
            f32::from_bits(b.1)
                .total_cmp(&f32::from_bits(a.1))
                .then(a.0.cmp(&b.0))
        });
        oracle.truncate(10);
        let mut samples = Vec::new();
        let mut actual_workers = Vec::new();
        let mut collector_bytes = Vec::new();
        for iteration in 0..84 {
            let began = std::time::Instant::now();
            let result = astra_05_query(&store, &query, 10, workers, CancelToken::new())
                .expect("crossover query");
            let micros = began.elapsed().as_secs_f64() * 1e6;
            assert_eq!(result.stats.dims_touched, (n * dim) as u64);
            assert_eq!(result.stats.bytes_read, (n * dim * 4) as u64);
            let signature: Vec<_> = result
                .candidates
                .iter()
                .map(|candidate| {
                    (
                        candidate.document().expect("identity").doc_id().get(),
                        candidate.score().to_bits(),
                    )
                })
                .collect();
            assert_eq!(signature, oracle, "independent scalar crossover oracle");
            let work = controller.take_exact_scan_work();
            assert_eq!(work.iter().map(|w| w.scored_rows).sum::<usize>(), n);
            let _ = controller.take_exact_worker_timings();
            if iteration >= 20 {
                samples.push(micros);
                actual_workers.push(result.stats.threads_used);
                collector_bytes.extend(work.iter().map(|w| {
                    w.collector_capacity
                        * std::mem::size_of::<zeppelin_embed::scan::ScanCandidate>()
                }));
            }
        }
        assert_eq!(
            store
                .stats()
                .expect("released crossover scratch")
                .temporary_bytes,
            0
        );
        eprintln!(
            "ASTRA05_CROSSOVER workers={workers} n={n} dim={dim} tied={tied} samples_us={samples:?} actual_workers={actual_workers:?} collector_bytes={collector_bytes:?}"
        );
    }
}
