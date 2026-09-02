//! End-to-end contracts for 19-M8 graph-segment consolidation.

#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::fs;
use std::path::Path;
use std::time::Duration;

use tempfile::{TempDir, tempdir};
use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, Normalization, StoreEpoch,
};
use zeppelin_embed::fts::index::DEFAULT_FIELD;
use zeppelin_embed::fts::search::TermQuery;
use zeppelin_embed::fts::tokenizer::TokenizerConfig;
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::{
    CancelToken, OpenOptions, QueryControl, SearchOptions, SearchTier, Store,
};
use zeppelin_embed::segment::layout::RegionKind;
use zeppelin_embed::tier::{
    MaintenanceBudget, MaintenanceReport, MaintenanceStatus, TierThresholds,
};

const DIMS: usize = 128;
const MAINTENANCE_TEST_BUDGET: Duration = Duration::from_secs(600);
const CONSOLIDATION_CHECKPOINT: &str = ".consolidate.checkpoint";

fn sift_epoch() -> StoreEpoch {
    let document = EmbeddingTower {
        model_id: "consolidation-sift-fixture".to_owned(),
        model_version: "1".to_owned(),
        weights_digest: vec![0x19],
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

fn open_store(path: &Path, context: &str) -> Store {
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

fn ingest_batch(store: &Store, first_doc: u128, rows: usize) {
    let documents = (0..rows)
        .map(|row| {
            let doc = first_doc + row as u128;
            let mut document = IngestDocument::new(
                DocumentVersion::new(DocId::new(doc), Revision::new(1)),
                fixture_vector(doc as f32),
            )
            .with_timestamp(doc as i64);
            if doc.is_multiple_of(2) {
                document = document.with_text(format!("shared token doc{doc}"));
            }
            if doc.is_multiple_of(3) {
                document = document.with_metadata(format!("meta-{doc}").into_bytes());
            }
            document
        })
        .collect::<Vec<_>>();
    store
        .ingest(IngestBatch::new(documents).with_epoch(sift_epoch().identity()))
        .expect("ingest consolidation batch");
}

fn maintain(store: &Store, bytes: u64, graph_min_rows: u32) -> MaintenanceReport {
    store.maintain_with_test_thresholds(
        MaintenanceBudget {
            wall_time: MAINTENANCE_TEST_BUDGET,
            bytes,
        },
        TierThresholds { graph_min_rows },
    )
}

fn segment_files(directory: &Path) -> Vec<String> {
    let mut names = fs::read_dir(directory)
        .expect("list store directory")
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with("segment-") && name.ends_with(".zseg"))
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn vector_results(store: &Store, amplitude: f32, k: usize) -> Vec<(DocumentVersion, u32)> {
    store
        .search(
            SearchRequest::new(&fixture_vector(amplitude)),
            k,
            SearchOptions::default().with_tier(SearchTier::Exact),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("vector search")
        .candidates
        .iter()
        .map(|candidate| {
            (
                candidate.document().expect("document identity"),
                candidate.score().to_bits(),
            )
        })
        .collect()
}

fn lexical_results(store: &Store, k: usize) -> Vec<(DocId, f64)> {
    store
        .search_lexical(
            &TermQuery::flat(vec![b"shared".to_vec()], &[DEFAULT_FIELD]),
            k,
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("lexical search")
        .candidates
        .into_iter()
        .map(|candidate| (candidate.document.doc_id(), candidate.score))
        .collect()
}

fn published_segment_count(store: &Store) -> usize {
    store
        .snapshot()
        .expect("published snapshot")
        .segments()
        .len()
}

/// Seals three 64-row batches and returns the store with 3 sealed segments.
fn three_sealed_batches(directory: &Path) -> Store {
    let store = open_store(directory, "open consolidation store");
    for batch in 0..3_u128 {
        ingest_batch(&store, batch * 64 + 1, 64);
        store.seal().expect("seal consolidation batch");
    }
    store
}

/// Stages exactly three published graph segments without consolidating.
///
/// The first segment dominates (256 of 320 rows is 80%), so the two-segment
/// state stays put; the third promotion runs under a byte budget that is
/// spent before the merge pass is admitted.
fn three_graph_segments(directory: &Path) -> Store {
    let store = open_store(directory, "open staged consolidation store");
    ingest_batch(&store, 1, 256);
    store.seal().expect("seal dominant batch");
    let first = maintain(&store, u64::MAX, 32);
    assert_eq!(first.graphs_built, 1);
    assert_eq!(first.consolidations, 0);
    ingest_batch(&store, 257, 32);
    store.seal().expect("seal second batch");
    let second = maintain(&store, u64::MAX, 32);
    assert_eq!(second.graphs_built, 1);
    assert_eq!(
        second.consolidations, 0,
        "a dominant segment pair must not consolidate"
    );
    ingest_batch(&store, 289, 32);
    store.seal().expect("seal third batch");
    let third = maintain(&store, 32_000, 32);
    assert_eq!(third.graphs_built, 1, "third promotion fits 32 KB");
    assert_eq!(third.consolidations, 0);
    assert!(matches!(third.status, MaintenanceStatus::BudgetExhausted));
    assert_eq!(published_segment_count(&store), 3);
    assert!(!directory.join(CONSOLIDATION_CHECKPOINT).exists());
    store
}

fn merge_charge(store: &Store) -> u64 {
    store
        .snapshot()
        .expect("charge snapshot")
        .segments()
        .iter()
        .map(|segment| segment.meta().file_size)
        .sum()
}

#[test]
fn one_maintain_call_promotes_three_batches_and_consolidates_them() {
    let directory = tempdir().expect("one-call directory");
    let store = three_sealed_batches(directory.path());

    let report = maintain(&store, u64::MAX, 64);
    assert!(matches!(report.status, MaintenanceStatus::Complete));
    assert_eq!(report.graphs_built, 3, "all three sealed batches promote");
    assert_eq!(report.consolidations, 1, "the graph trio merges into one");
    assert!(report.consolidation_deferrals.is_empty());
    assert_eq!(report.graph_profiles.len(), 4);
    assert_eq!(published_segment_count(&store), 1);
    assert_eq!(segment_files(directory.path()).len(), 1);
}

#[test]
fn consolidation_merges_every_graph_segment_into_one_and_preserves_results() {
    let directory = tempdir().expect("consolidation directory");
    let store = open_store(directory.path(), "open preservation store");
    ingest_batch(&store, 1, 256);
    store
        .delete(DeleteBatch::new(vec![DocId::new(5)]))
        .expect("tombstone one active doc");
    store.seal().expect("seal dominant batch");
    assert_eq!(maintain(&store, u64::MAX, 32).graphs_built, 1);
    ingest_batch(&store, 257, 32);
    store.seal().expect("seal second batch");
    assert_eq!(maintain(&store, u64::MAX, 32).graphs_built, 1);
    ingest_batch(&store, 289, 32);
    store.seal().expect("seal third batch");
    let staged = maintain(&store, 32_000, 32);
    assert_eq!(staged.graphs_built, 1);
    assert_eq!(staged.consolidations, 0);
    assert_eq!(
        published_segment_count(&store),
        3,
        "the staging protocol leaves three published graph segments"
    );

    let queries = [1.0_f32, 63.5, 100.2, 319.9];
    let vector_before = queries
        .map(|amplitude| vector_results(&store, amplitude, 3))
        .to_vec();
    let lexical_before = lexical_results(&store, 5);
    let generation_before = store
        .snapshot()
        .expect("pre-consolidation snapshot")
        .generation();

    let report = maintain(&store, u64::MAX, 32);
    assert!(matches!(report.status, MaintenanceStatus::Complete));
    assert_eq!(report.graphs_built, 0, "every input already has its graph");
    assert_eq!(report.consolidations, 1, "the graph trio merges into one");
    assert!(report.consolidation_deferrals.is_empty());
    assert_eq!(report.graph_profiles.len(), 1);

    let snapshot = store.snapshot().expect("post-consolidation snapshot");
    assert_eq!(snapshot.segments().len(), 1);
    let merged = snapshot.segments().first().expect("merged segment");
    assert_eq!(
        merged.meta().row_count,
        320 - 1,
        "only live rows are carried"
    );
    assert!(
        merged
            .directory()
            .iter()
            .any(|entry| entry.kind == RegionKind::GraphNodeBlocks.id()),
        "the merged segment carries one graph"
    );
    assert_eq!(
        report.consolidation_generation,
        Some(snapshot.generation()),
        "the mutation returns the generation it changed"
    );
    assert!(snapshot.generation() > generation_before);
    drop(snapshot);

    assert_eq!(
        segment_files(directory.path()).len(),
        1,
        "inputs and the merge intermediate are unlinked after the commit"
    );
    assert!(!directory.path().join(CONSOLIDATION_CHECKPOINT).exists());

    for (amplitude, before) in queries.iter().zip(&vector_before) {
        let after = vector_results(&store, *amplitude, 3);
        assert_eq!(&after, before, "top-k changed at amplitude {amplitude}");
    }
    assert_eq!(
        lexical_results(&store, 5),
        lexical_before,
        "BM25 scores must survive the df re-aggregation"
    );
    let deleted = DocId::new(5);
    assert!(
        !vector_results(&store, 5.0, 3)
            .iter()
            .any(|(document, _)| document.doc_id() == deleted),
        "a tombstoned doc must not resurrect"
    );

    let second = maintain(&store, u64::MAX, 64);
    assert!(matches!(second.status, MaintenanceStatus::Complete));
    assert_eq!(second.graphs_built, 0);
    assert_eq!(second.consolidations, 0, "consolidation is idempotent");
}

#[test]
fn interrupted_consolidation_leaves_old_segments_published_then_resumes() {
    let directory = tempdir().expect("interruption directory");
    let store = three_graph_segments(directory.path());
    let charge = merge_charge(&store);

    let interrupted = maintain(&store, charge + 4_000, 32);
    assert!(matches!(
        interrupted.status,
        MaintenanceStatus::BudgetExhausted
    ));
    assert_eq!(interrupted.consolidations, 0);
    assert_eq!(
        published_segment_count(&store),
        3,
        "an interrupted merge publishes nothing"
    );
    assert!(directory.path().join(CONSOLIDATION_CHECKPOINT).exists());
    assert_eq!(
        segment_files(directory.path()).len(),
        4,
        "the merge intermediate exists beside the three published inputs"
    );
    let acked = vector_results(&store, 300.0, 1);
    assert_eq!(
        acked[0].0,
        DocumentVersion::new(DocId::new(300), Revision::new(1))
    );

    let resumed = maintain(&store, u64::MAX, 32);
    assert!(matches!(resumed.status, MaintenanceStatus::Complete));
    assert_eq!(resumed.consolidations, 1);
    assert_eq!(
        resumed.checkpoints_resumed, 1,
        "the merge checkpoint is resumed instead of re-merging"
    );
    assert_eq!(published_segment_count(&store), 1);
    assert_eq!(segment_files(directory.path()).len(), 1);
    assert!(!directory.path().join(CONSOLIDATION_CHECKPOINT).exists());
}

#[test]
fn a_corrupted_consolidation_checkpoint_is_refused_and_cleared() {
    let directory = tempdir().expect("corruption directory");
    let store = three_graph_segments(directory.path());
    let charge = merge_charge(&store);

    let interrupted = maintain(&store, charge + 4_000, 32);
    assert!(matches!(
        interrupted.status,
        MaintenanceStatus::BudgetExhausted
    ));
    let checkpoint_path = directory.path().join(CONSOLIDATION_CHECKPOINT);
    let mut bytes = fs::read(&checkpoint_path).expect("checkpoint bytes");
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xff;
    fs::write(&checkpoint_path, bytes).expect("corrupt checkpoint");

    let recovered = maintain(&store, u64::MAX, 32);
    assert!(matches!(recovered.status, MaintenanceStatus::Complete));
    assert_eq!(recovered.consolidations, 1);
    assert_eq!(
        recovered.checkpoints_resumed, 0,
        "a corrupt checkpoint restarts the merge instead of resuming"
    );
    assert_eq!(published_segment_count(&store), 1);
    assert!(!checkpoint_path.exists());
}

#[test]
fn consolidation_is_deterministic_across_runs_and_interruption() {
    let directory = tempdir().expect("determinism directory");
    let store = three_graph_segments(directory.path());
    let charge = merge_charge(&store);
    store.close().expect("close staged store");

    let copy_a = copy_store(directory.path());
    let copy_b = copy_store(directory.path());
    let copy_c = copy_store(directory.path());

    let store_a = open_store(copy_a.path(), "reopen copy A");
    let complete_a = maintain(&store_a, u64::MAX, 32);
    assert_eq!(complete_a.consolidations, 1);
    store_a.close().expect("close copy A");

    let store_b = open_store(copy_b.path(), "reopen copy B");
    let complete_b = maintain(&store_b, u64::MAX, 32);
    assert_eq!(complete_b.consolidations, 1);
    store_b.close().expect("close copy B");

    let store_c = open_store(copy_c.path(), "reopen copy C");
    let interrupted = maintain(&store_c, charge + 4_000, 32);
    assert!(matches!(
        interrupted.status,
        MaintenanceStatus::BudgetExhausted
    ));
    let resumed = maintain(&store_c, u64::MAX, 32);
    assert_eq!(resumed.consolidations, 1);
    store_c.close().expect("close copy C");

    let names_a = segment_files(copy_a.path());
    assert_eq!(names_a.len(), 1);
    assert_eq!(names_a, segment_files(copy_b.path()));
    assert_eq!(names_a, segment_files(copy_c.path()));
    let bytes_a = fs::read(copy_a.path().join(&names_a[0])).expect("copy A segment");
    let bytes_b = fs::read(copy_b.path().join(&names_a[0])).expect("copy B segment");
    let bytes_c = fs::read(copy_c.path().join(&names_a[0])).expect("copy C segment");
    assert_eq!(bytes_a, bytes_b, "two clean runs must be byte-identical");
    assert_eq!(
        bytes_a, bytes_c,
        "an interrupted and resumed run must be byte-identical"
    );
}

fn copy_store(source: &Path) -> TempDir {
    let target = tempdir().expect("copy directory");
    for entry in fs::read_dir(source).expect("list source store") {
        let entry = entry.expect("source entry");
        if entry.file_type().expect("entry type").is_file() {
            fs::copy(entry.path(), target.path().join(entry.file_name())).expect("copy store file");
        }
    }
    target
}

#[test]
fn consolidation_defers_when_an_input_region_cannot_be_carried() {
    let directory = tempdir().expect("deferral directory");
    let store = three_graph_segments(directory.path());
    let victim = store
        .snapshot()
        .expect("deferral snapshot")
        .segments()
        .iter()
        .find(|segment| {
            segment
                .directory()
                .iter()
                .any(|entry| entry.kind == RegionKind::StoredMetadata.id())
        })
        .map(|segment| segment.meta().id)
        .expect("a metadata-bearing graph segment");
    store.close().expect("close before relabel");

    let victim_path = directory.path().join(victim.file_name());
    let mut bytes = fs::read(&victim_path).expect("read victim segment");
    relabel_region(&mut bytes, RegionKind::StoredMetadata, 65_000);
    fs::write(&victim_path, bytes).expect("write relabeled segment");

    let store = open_store(directory.path(), "reopen relabeled store");
    let report = maintain(&store, u64::MAX, 32);
    assert!(matches!(report.status, MaintenanceStatus::Complete));
    assert_eq!(report.consolidations, 0);
    assert_eq!(report.consolidation_deferrals.len(), 1);
    let deferral = &report.consolidation_deferrals[0];
    assert_eq!(deferral.segment_id, victim);
    assert_eq!(
        deferral.uncarried_regions,
        vec![zeppelin_embed::tier::maintain::UncarriedRegionKind::Unknown(65_000)]
    );
    assert_eq!(
        published_segment_count(&store),
        3,
        "a deferred consolidation publishes nothing"
    );
}

#[test]
fn drop_partition_behaves_identically_before_and_after_consolidation() {
    // Store A consolidates first; store B keeps its three graph segments.
    let dir_a = tempdir().expect("drop-after directory");
    let store_a = three_graph_segments(dir_a.path());
    let merged = maintain(&store_a, u64::MAX, 32);
    assert_eq!(merged.consolidations, 1);

    // Timestamps span 1..=320, so 0..100 straddles the merged segment and
    // must be skipped with the merged segment named, proving the union
    // clustering range was recomputed and stamped.
    let straddle = store_a.drop_partition(0..100).expect("straddling drop");
    assert!(straddle.segments_dropped().is_empty());
    assert_eq!(straddle.straddlers_skipped().len(), 1);
    assert_eq!(published_segment_count(&store_a), 1);

    // A non-straddling range drops everything on both stores.
    let full_a = store_a.drop_partition(0..1000).expect("full drop after");
    assert_eq!(full_a.segments_dropped().len(), 1);
    assert_eq!(published_segment_count(&store_a), 0);

    let dir_b = tempdir().expect("drop-before directory");
    let store_b = three_graph_segments(dir_b.path());
    let full_b = store_b.drop_partition(0..1000).expect("full drop before");
    assert_eq!(full_b.segments_dropped().len(), 3);
    assert_eq!(published_segment_count(&store_b), 0);
}

#[test]
fn consolidation_skips_a_store_whose_graph_rows_are_all_tombstoned() {
    let directory = tempdir().expect("tombstoned directory");
    let store = open_store(directory.path(), "open tombstoned store");
    for batch in 0..3_u128 {
        let first = batch * 64 + 1;
        ingest_batch(&store, first, 64);
        store
            .delete(DeleteBatch::new(
                (0..64).map(|row| DocId::new(first + row)).collect(),
            ))
            .expect("tombstone the whole batch");
        store.seal().expect("seal tombstoned batch");
    }
    let report = maintain(&store, u64::MAX, 64);
    assert!(matches!(report.status, MaintenanceStatus::Complete));
    assert_eq!(
        report.consolidations, 0,
        "an all-tombstoned store has nothing to carry"
    );
    assert!(!directory.path().join(CONSOLIDATION_CHECKPOINT).exists());
}

// Byte-surgery helpers shared with `tiering.rs`: relabel one region's kind
// and re-seal every dependent checksum so the segment stays valid.
fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().expect("u16 bytes"))
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("u32 bytes"))
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("u64 bytes"))
}

fn region_entry_offset(bytes: &[u8], kind: u16) -> usize {
    use zeppelin_embed::format::frame::FILE_HEADER_LEN;
    use zeppelin_embed::segment::layout::{REGION_ENTRY_LEN, SEGMENT_PREFIX_LEN};
    let directory_start = FILE_HEADER_LEN + SEGMENT_PREFIX_LEN;
    let region_count = usize::from(read_u16(bytes, FILE_HEADER_LEN + 20));
    (0..region_count)
        .map(|index| directory_start + index * REGION_ENTRY_LEN)
        .find(|offset| read_u16(bytes, *offset) == kind)
        .expect("region entry")
}

fn relabel_region(bytes: &mut [u8], from: RegionKind, to: u16) {
    use xxhash_rust::xxh3::xxh3_64;
    use zeppelin_embed::format::frame::FILE_TRAILER_LEN;
    let entry = region_entry_offset(bytes, from.id());
    bytes[entry..entry + 2].copy_from_slice(&to.to_le_bytes());
    let checksum_entry = region_entry_offset(bytes, RegionKind::ChecksumTable.id());
    let checksum_start = usize::try_from(read_u64(bytes, checksum_entry + 8)).expect("offset");
    let checksum_length = usize::try_from(read_u64(bytes, checksum_entry + 16)).expect("length");
    let checksum_end = checksum_start + checksum_length;
    let chunk_count = usize::try_from(read_u32(bytes, checksum_start)).expect("chunks");
    for chunk in 0..chunk_count {
        let chunk_entry = checksum_start + 8 + chunk * 16;
        if read_u16(bytes, chunk_entry) == from.id() {
            bytes[chunk_entry..chunk_entry + 2].copy_from_slice(&to.to_le_bytes());
        }
    }
    let table_checksum = xxh3_64(&bytes[checksum_start..checksum_end]).to_le_bytes();
    bytes[checksum_entry + 24..checksum_entry + 32].copy_from_slice(&table_checksum);
    let header_length = usize::try_from(read_u64(bytes, 16)).expect("header length");
    let header_checksum_offset = header_length - 8;
    let header_checksum = xxh3_64(&bytes[..header_checksum_offset]).to_le_bytes();
    bytes[header_checksum_offset..header_length].copy_from_slice(&header_checksum);
    let trailer = bytes.len() - FILE_TRAILER_LEN;
    let file_checksum = xxh3_64(&bytes[..trailer]).to_le_bytes();
    bytes[trailer..].copy_from_slice(&file_checksum);
}
