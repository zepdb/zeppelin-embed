#![allow(clippy::expect_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, mpsc};

use tempfile::tempdir;
use zeppelin_embed::graph::block::{GraphNodeBlockBuild, GraphNodeBlockInput, GraphNodeLayout};
use zeppelin_embed::ingest::{
    DeleteBatch, DocId, DocumentVersion, IngestBatch, IngestDocument, PurgeError, Revision,
    SearchRequest,
};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use zeppelin_embed::manifest::Manifest;
use zeppelin_embed::manifest::io::commit_manifest;
use zeppelin_embed::meta::{
    AliveSet, Column, ColumnDefinition, ColumnId, ColumnInput, ColumnStoreBuilder, ColumnType,
    ColumnValue, Schema,
};
use zeppelin_embed::quant::Bit4Factors;
use zeppelin_embed::scan::ScanOptions;
use zeppelin_embed::segment::layout::{Int8Factors, RegionKind};
use zeppelin_embed::segment::writer::{
    SegmentBuild, SegmentDocumentVersions, SegmentFactors, write_segment_with_documents,
    write_segment_with_graph_and_documents,
};
use zeppelin_embed::segment::{ClusteringKeyRange, SegmentId};
use zeppelin_embed::vfs::{StdVfs, SyncKind, Vfs, VfsFile};

const METADATA_SENTINEL: &[u8] = b"ZE_PURGE_METADATA_7f4a91c2";
const VECTOR_SENTINEL_BITS: [u32; 4] = [0x3f12_34ab, 0x3e56_78cd, 0xbf23_45de, 0x3d67_89ef];

#[test]
fn purged_sentinels_are_absent_from_every_file_under_the_store() {
    let directory = tempdir().expect("store directory");
    let purged = DocId::new(0x7f4a_91c2);
    let vector = VECTOR_SENTINEL_BITS.map(f32::from_bits).to_vec();
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(DocumentVersion::new(purged, Revision::new(1)), vector)
                .with_metadata(METADATA_SENTINEL.to_vec()),
        ]))
        .expect("ingest sentinel document");
    store.seal().expect("seal sentinel document");

    let snapshot = store.snapshot().expect("metadata snapshot");
    let metadata = snapshot.segments()[0]
        .stored_metadata()
        .expect("metadata region")
        .expect("metadata rows");
    assert_eq!(metadata.row_count(), 1);
    assert_eq!(metadata.row(0), Some(METADATA_SENTINEL));
    assert_eq!(metadata.row(1), None);
    drop(snapshot);

    let before = sentinel_hits(directory.path()).expect("scan pre-purge artifacts");
    assert!(
        before.iter().any(|hit| hit.kind == SentinelKind::Metadata),
        "metadata sentinel was not findable before purge: {}",
        describe_hits(&before)
    );
    for bits in VECTOR_SENTINEL_BITS {
        assert!(
            before
                .iter()
                .any(|hit| hit.kind == SentinelKind::Vector(bits)),
            "vector sentinel {bits:#010x} was not findable before purge: {}",
            describe_hits(&before)
        );
    }

    let token = store.purge(&[purged]).expect("schedule physical purge");
    store
        .await_physical_purge(token)
        .expect("await physical purge");

    let after = sentinel_hits(directory.path()).expect("scan post-purge artifacts");
    assert!(
        after.is_empty(),
        "purged sentinel bytes remain: {}",
        describe_hits(&after)
    );
}

#[test]
fn purge_token_resolves_only_after_the_bytes_are_gone() {
    let directory = tempdir().expect("store directory");
    let purged = DocId::new(201);
    let store = Arc::new(sealed_store_with_sentinel(directory.path(), purged));
    let old_segment = store.snapshot().expect("snapshot").segments()[0].meta().id;
    let old_path = directory.path().join(old_segment.file_name());
    let token = store.purge(&[purged]).expect("schedule purge");
    let blocking = BlockingDeleteVfs::default();
    let (result_tx, result_rx) = mpsc::channel();
    let worker_store = Arc::clone(&store);
    let worker_vfs = blocking.clone();
    let worker = std::thread::spawn(move || {
        result_tx
            .send(worker_store.await_physical_purge_on_vfs(token, &worker_vfs))
            .expect("return purge result");
    });
    blocking.wait_until_delete_blocked();

    assert!(
        old_path.exists(),
        "old segment was unlinked before blocked delete"
    );
    assert!(
        !sentinel_hits(directory.path())
            .expect("scan while await is blocked")
            .is_empty(),
        "bytes disappeared before the old artifact unlink"
    );
    assert!(
        matches!(result_rx.try_recv(), Err(mpsc::TryRecvError::Empty)),
        "purge token resolved while old bytes were still linked"
    );

    blocking.release_delete();
    result_rx
        .recv()
        .expect("purge result channel")
        .expect("physical purge");
    worker.join().expect("purge worker");
    assert!(
        sentinel_hits(directory.path())
            .expect("scan after token resolution")
            .is_empty()
    );
}

#[test]
fn purge_refuses_without_temp_space_and_leaves_the_store_intact() {
    let directory = tempdir().expect("store directory");
    let purged = DocId::new(301);
    let store = sealed_store_with_sentinel(directory.path(), purged);
    let snapshot = store.snapshot().expect("snapshot before refusal");
    let generation = snapshot.generation();
    let segment_bytes = snapshot.segments()[0].meta().file_size;
    let required_bytes = segment_bytes * 6 / 5;
    let before = file_bytes(directory.path()).expect("snapshot store bytes");

    let error = store
        .purge_with_available_space(&[purged], required_bytes)
        .expect_err("free bytes equal to the strict threshold must refuse purge");

    assert!(
        matches!(
            error,
            PurgeError::InsufficientTempSpace {
                segment_bytes: observed_segment_bytes,
                available_bytes,
                required_bytes: observed_required_bytes,
            }
            if observed_segment_bytes == segment_bytes
                && available_bytes == required_bytes
                && observed_required_bytes == required_bytes
        ),
        "wrong refusal: {error:?}"
    );
    assert_eq!(
        file_bytes(directory.path()).expect("resnapshot store bytes"),
        before
    );
    assert_eq!(
        store
            .snapshot()
            .expect("snapshot after refusal")
            .generation(),
        generation
    );
}

#[test]
fn purge_renumbers_surviving_rows_consistently() {
    let directory = tempdir().expect("store directory");
    let first = DocumentVersion::new(DocId::new(401), Revision::new(1));
    let removed = DocumentVersion::new(DocId::new(402), Revision::new(1));
    let last = DocumentVersion::new(DocId::new(403), Revision::new(1));
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(first, vec![1.0, 0.0]),
            IngestDocument::new(removed, vec![0.0, 1.0]),
            IngestDocument::new(last, vec![-1.0, 0.0]),
        ]))
        .expect("ingest rows");
    store.seal().expect("seal rows");

    let token = store.purge(&[removed.doc_id()]).expect("schedule purge");
    store.await_physical_purge(token).expect("await purge");

    let first_result = search_one(&store, &[1.0, 0.0]);
    let last_result = search_one(&store, &[-1.0, 0.0]);
    assert_eq!(first_result.document(), Some(first));
    assert_eq!(first_result.row_id().local_row(), 0);
    assert_eq!(last_result.document(), Some(last));
    assert_eq!(last_result.row_id().local_row(), 1);
}

#[test]
fn purge_does_not_leave_a_graph_pointing_at_stale_row_ids() {
    let directory = tempdir().expect("store directory");
    let removed = DocId::new(502);
    publish_graph_segment(directory.path(), removed);
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open graph store");
    assert!(
        store.snapshot().expect("before snapshot").segments()[0]
            .directory()
            .iter()
            .any(|entry| entry.kind == RegionKind::GraphNodeBlocks.id()),
        "fixture does not contain a graph"
    );

    let token = store.purge(&[removed]).expect("schedule graph purge");
    store
        .await_physical_purge(token)
        .expect("await graph purge");

    let snapshot = store.snapshot().expect("after snapshot");
    let rewritten = snapshot.segments().first().expect("rewritten segment");
    assert_eq!(rewritten.meta().row_count, 2);
    assert!(
        rewritten
            .directory()
            .iter()
            .all(|entry| entry.kind != RegionKind::GraphNodeBlocks.id()),
        "rewritten segment retained stale graph row ids"
    );
}

#[test]
fn purge_scrubs_wal_records_that_carried_purged_payloads() {
    let directory = tempdir().expect("store directory");
    let purged = DocId::new(601);
    let store = sealed_store_with_sentinel(directory.path(), purged);
    let wal_path = directory.path().join("wal.ze");
    let before = fs::read(&wal_path).expect("read WAL before purge");
    assert!(contains(&before, METADATA_SENTINEL));
    assert!(
        VECTOR_SENTINEL_BITS
            .iter()
            .all(|bits| contains(&before, &bits.to_le_bytes()))
    );

    let token = store.purge(&[purged]).expect("schedule purge");
    store.await_physical_purge(token).expect("await purge");

    let after = fs::read(&wal_path).expect("read WAL after purge");
    assert!(!contains(&after, METADATA_SENTINEL));
    assert!(
        VECTOR_SENTINEL_BITS
            .iter()
            .all(|bits| !contains(&after, &bits.to_le_bytes()))
    );
    store.close().expect("close scrubbed store");
    Store::open(directory.path(), OpenOptions::default()).expect("reopen scrubbed WAL");
}

#[test]
fn purge_of_an_unknown_id_is_a_no_op_and_reports_it() {
    let directory = tempdir().expect("store directory");
    let store = sealed_store_with_sentinel(directory.path(), DocId::new(701));
    let unknown = DocId::new(799);
    let generation = store.snapshot().expect("before snapshot").generation();
    let before = file_bytes(directory.path()).expect("snapshot store bytes");

    let token = store.purge(&[unknown]).expect("schedule unknown purge");
    assert!(token.is_no_op());
    assert_eq!(token.unknown_ids(), &[unknown]);
    let report = store
        .await_physical_purge(token)
        .expect("await no-op purge");

    assert!(report.is_no_op());
    assert_eq!(report.generation(), generation);
    assert_eq!(report.unknown_ids(), &[unknown]);
    assert_eq!(
        file_bytes(directory.path()).expect("resnapshot store"),
        before
    );
}

#[test]
fn active_only_purge_compacts_rows_preserves_tombstones_and_reopens() {
    let directory = tempdir().expect("active purge store directory");
    let first = DocumentVersion::new(DocId::new(801), Revision::new(1));
    let removed = DocumentVersion::new(DocId::new(802), Revision::new(1));
    let deleted = DocumentVersion::new(DocId::new(803), Revision::new(1));
    let last = DocumentVersion::new(DocId::new(804), Revision::new(1));
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open active store");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(first, vec![4.0, 0.0])
                .with_timestamp(11)
                .with_metadata(b"first".to_vec()),
            IngestDocument::new(removed, vec![0.0, 4.0])
                .with_timestamp(22)
                .with_metadata(b"remove-me".to_vec()),
            IngestDocument::new(deleted, vec![-4.0, 0.0])
                .with_timestamp(33)
                .with_metadata(b"deleted-survivor".to_vec()),
            IngestDocument::new(last, vec![0.0, -4.0])
                .with_timestamp(44)
                .with_metadata(b"last".to_vec()),
        ]))
        .expect("ingest active purge rows");
    store
        .delete(DeleteBatch::new(vec![deleted.doc_id()]))
        .expect("tombstone survivor before purge");

    let token = store
        .purge(&[removed.doc_id()])
        .expect("schedule active-only purge");
    let report = store
        .await_physical_purge(token)
        .expect("complete active-only purge");
    assert_eq!(report.segments_rewritten(), 0);
    assert!(report.wal_rewritten());

    assert_eq!(search_one(&store, &[4.0, 0.0]).document(), Some(first));
    assert_eq!(search_one(&store, &[0.0, -4.0]).document(), Some(last));
    let all = store
        .search(
            SearchRequest::new(&[1.0, 1.0]),
            8,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search compacted active rows");
    let visible = all
        .candidates
        .iter()
        .filter_map(|candidate| candidate.document())
        .collect::<Vec<_>>();
    assert_eq!(visible.len(), 2);
    assert!(visible.contains(&first));
    assert!(visible.contains(&last));
    assert!(!visible.contains(&removed));
    assert!(!visible.contains(&deleted));

    store.close().expect("close active purge store");
    let reopened = Store::open(directory.path(), OpenOptions::default()).expect("reopen store");
    assert_eq!(search_one(&reopened, &[4.0, 0.0]).document(), Some(first));
    assert_eq!(search_one(&reopened, &[0.0, -4.0]).document(), Some(last));
}

#[test]
fn bl_135_purge_persists_acked_generation_with_nonempty_wal() {
    let directory = tempdir().expect("BL-135 purge directory");
    let removed = DocId::new(851);
    let retained = DocumentVersion::new(DocId::new(852), Revision::new(1));
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(removed, Revision::new(1)),
                vec![1.0, 0.0],
            ),
            IngestDocument::new(retained, vec![0.0, 1.0]),
        ]))
        .expect("ingest active rows");

    let token = store.purge(&[removed]).expect("schedule active purge");
    let acknowledged = store
        .await_physical_purge(token)
        .expect("complete active purge")
        .generation();
    store.close().expect("close purged store");

    let reopened = Store::open(directory.path(), OpenOptions::default()).expect("reopen store");
    assert!(
        reopened.snapshot().expect("reopened snapshot").generation() >= acknowledged,
        "reopen regressed below the acknowledged purge generation"
    );
    assert_eq!(
        search_one(&reopened, &[0.0, 1.0]).document(),
        Some(retained)
    );
}

#[test]
fn int8_physical_purge_preserves_surviving_codes_and_tombstones() {
    let directory = tempdir().expect("Int8 purge directory");
    let (first, removed, deleted) = publish_int8_document_segment(directory.path());
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open Int8 store");

    let token = store
        .purge(&[removed.doc_id()])
        .expect("schedule Int8 purge");
    let report = store
        .await_physical_purge(token)
        .expect("complete Int8 purge");
    assert_eq!(report.segments_rewritten(), 1);

    let snapshot = store.snapshot().expect("Int8 purge snapshot");
    let segment = snapshot.segments().first().expect("rewritten Int8 segment");
    assert_eq!(segment.meta().scheme, 2);
    assert_eq!(segment.meta().row_count, 2);
    assert_eq!(
        segment.document_version(0).expect("first version"),
        Some(first)
    );
    assert_eq!(
        segment.document_version(1).expect("last version"),
        Some(deleted)
    );
    assert!(segment.alive().expect("rewritten alive set").is_alive(0));
    assert!(!segment.alive().expect("rewritten alive set").is_alive(1));
    drop(snapshot);

    let result = search_one(&store, &[1.0, 0.0]);
    assert_eq!(result.document(), Some(first));
    let all = store
        .search(
            SearchRequest::new(&[-1.0, 0.0]),
            4,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search rewritten Int8 segment");
    assert!(
        all.candidates
            .iter()
            .all(|candidate| candidate.document() != Some(removed))
    );
    assert!(
        all.candidates
            .iter()
            .all(|candidate| candidate.document() != Some(deleted))
    );
}

#[test]
fn physical_purge_preserves_every_typed_metadata_column_and_null() {
    let directory = tempdir().expect("typed-column purge directory");
    let (first, removed, last) = publish_all_column_document_segment(directory.path());
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open typed store");

    let token = store
        .purge(&[removed.doc_id()])
        .expect("schedule typed-column purge");
    store
        .await_physical_purge(token)
        .expect("rewrite typed-column segment");

    let snapshot = store.snapshot().expect("typed-column snapshot");
    let segment = snapshot.segments().first().expect("rewritten segment");
    assert_eq!(segment.meta().row_count, 2);
    assert_eq!(
        segment.document_version(0).expect("first version"),
        Some(first)
    );
    assert_eq!(
        segment.document_version(1).expect("last version"),
        Some(last)
    );
    let columns = segment.columns().expect("rewritten columns");
    assert_eq!(columns.timestamps(), &[10, 30]);
    match columns.column(ColumnId::new(1)) {
        Some(Column::U64(values)) => assert_eq!((values.get(0), values.get(1)), (Some(11), None)),
        actual => panic!("unexpected u64 column: {actual:?}"),
    }
    match columns.column(ColumnId::new(2)) {
        Some(Column::I64(values)) => assert_eq!((values.get(0), values.get(1)), (Some(-12), None)),
        actual => panic!("unexpected i64 column: {actual:?}"),
    }
    match columns.column(ColumnId::new(3)) {
        Some(Column::F64(values)) => {
            assert_eq!(values.get(0).map(f64::to_bits), Some((-0.0_f64).to_bits()));
            assert_eq!(values.get(1), None);
        }
        actual => panic!("unexpected f64 column: {actual:?}"),
    }
    match columns.column(ColumnId::new(4)) {
        Some(Column::Bool(values)) => {
            assert_eq!((values.get(0), values.get(1)), (Some(true), None))
        }
        actual => panic!("unexpected bool column: {actual:?}"),
    }
    match columns.column(ColumnId::new(5)) {
        Some(Column::DictionaryString(values)) => {
            assert_eq!((values.get(0), values.get(1)), (Some("alpha"), None));
        }
        actual => panic!("unexpected dictionary column: {actual:?}"),
    }
    match columns.column(ColumnId::new(6)) {
        Some(Column::RawString(values)) => {
            assert_eq!((values.get(0), values.get(1)), (Some("raw-alpha"), None));
        }
        actual => panic!("unexpected raw column: {actual:?}"),
    }
}

#[test]
fn pending_physical_purge_is_completed_during_reopen() {
    let directory = tempdir().expect("purge recovery directory");
    let removed = DocumentVersion::new(DocId::new(951), Revision::new(1));
    let retained = DocumentVersion::new(DocId::new(952), Revision::new(1));
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open store");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(removed, vec![1.0, 0.0]),
            IngestDocument::new(retained, vec![0.0, 1.0]),
        ]))
        .expect("ingest recovery rows");
    store.seal().expect("seal recovery rows");
    let token = store
        .purge(&[removed.doc_id()])
        .expect("persist purge intent");
    assert!(!token.is_no_op());
    store.close().expect("close with pending intent");

    let reopened = Store::open(directory.path(), OpenOptions::default())
        .expect("open completes pending physical purge");
    let snapshot = reopened.snapshot().expect("recovered snapshot");
    assert_eq!(snapshot.segments().len(), 1);
    assert_eq!(snapshot.segments()[0].meta().row_count, 1);
    assert_eq!(
        snapshot.segments()[0]
            .document_version(0)
            .expect("retained version"),
        Some(retained)
    );
    drop(snapshot);
    assert_eq!(
        search_one(&reopened, &[0.0, 1.0]).document(),
        Some(retained)
    );
    assert!(
        !directory.path().join("purge.ze").exists(),
        "recovery left the committed intent behind"
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SentinelKind {
    Metadata,
    Vector(u32),
}

#[derive(Debug)]
struct SentinelHit {
    path: PathBuf,
    kind: SentinelKind,
    offset: usize,
}

fn describe_hits(hits: &[SentinelHit]) -> String {
    hits.iter()
        .map(|hit| format!("{}:{:?}@{}", hit.path.display(), hit.kind, hit.offset))
        .collect::<Vec<_>>()
        .join(", ")
}

fn sentinel_hits(root: &Path) -> std::io::Result<Vec<SentinelHit>> {
    let vector_patterns = VECTOR_SENTINEL_BITS.map(u32::to_le_bytes);
    let mut hits = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            for entry in fs::read_dir(&path)? {
                pending.push(entry?.path());
            }
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        let bytes = fs::read(&path)?;
        for offset in find_all(&bytes, METADATA_SENTINEL) {
            hits.push(SentinelHit {
                path: path.clone(),
                kind: SentinelKind::Metadata,
                offset,
            });
        }
        for (bits, pattern) in VECTOR_SENTINEL_BITS.iter().zip(&vector_patterns) {
            for offset in find_all(&bytes, pattern) {
                hits.push(SentinelHit {
                    path: path.clone(),
                    kind: SentinelKind::Vector(*bits),
                    offset,
                });
            }
        }
    }
    Ok(hits)
}

fn find_all(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return Vec::new();
    }
    haystack
        .windows(needle.len())
        .enumerate()
        .filter_map(|(offset, candidate)| (candidate == needle).then_some(offset))
        .collect()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !find_all(haystack, needle).is_empty()
}

fn sealed_store_with_sentinel(directory: &Path, doc_id: DocId) -> Store {
    let vector = VECTOR_SENTINEL_BITS.map(f32::from_bits).to_vec();
    let store = Store::open(directory, OpenOptions::default()).expect("open store");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(DocumentVersion::new(doc_id, Revision::new(1)), vector)
                .with_metadata(METADATA_SENTINEL.to_vec()),
        ]))
        .expect("ingest sentinel document");
    store.seal().expect("seal sentinel document");
    store
}

fn file_bytes(root: &Path) -> std::io::Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            for entry in fs::read_dir(&path)? {
                pending.push(entry?.path());
            }
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(std::io::Error::other)?
                .to_path_buf();
            files.insert(relative, fs::read(path)?);
        }
    }
    Ok(files)
}

fn search_one(store: &Store, vector: &[f32]) -> zeppelin_embed::ingest::SearchCandidate {
    store
        .search(
            SearchRequest::new(vector),
            1,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search survivor")
        .candidates
        .first()
        .copied()
        .expect("one survivor")
}

fn publish_graph_segment(directory: &Path, removed: DocId) {
    let id = SegmentId::new(900, [0x41; 10]);
    let dims = 128_u32;
    let rows = 3_usize;
    let codes = vec![0_u8; rows * dims as usize / 2];
    let factors = vec![Bit4Factors::from_persisted(1.0, 1.0, 0.0); rows];
    let mut rescore = vec![0.0_f32; rows * dims as usize];
    rescore[0] = 1.0;
    rescore[dims as usize + 1] = 1.0;
    rescore[2 * dims as usize] = -1.0;
    let mut columns = ColumnStoreBuilder::new(Schema::new(Vec::new()).expect("schema"));
    for timestamp in [1_i64, 2, 3] {
        columns.push_row(timestamp, &[]).expect("column row");
    }
    let columns = columns.finish().expect("columns");
    let alive = AliveSet::new(rows as u32);
    let doc_ids = [DocId::new(501), removed, DocId::new(503)];
    let revisions = [Revision::new(1); 3];
    let padded_codes = vec![0_u8; 64];
    let nodes = [
        GraphNodeBlockInput {
            codes: &padded_codes,
            factors: factors[0],
            flags: 1,
            neighbors: &[1, 2],
        },
        GraphNodeBlockInput {
            codes: &padded_codes,
            factors: factors[1],
            flags: 0,
            neighbors: &[0, 2],
        },
        GraphNodeBlockInput {
            codes: &padded_codes,
            factors: factors[2],
            flags: 0,
            neighbors: &[1, 0],
        },
    ];
    let policy = DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::Ordered)
        .expect("derived policy");
    let mut meta = write_segment_with_graph_and_documents(
        &StdVfs,
        directory,
        SegmentBuild {
            id,
            scheme: 4,
            dims,
            codes: &codes,
            factors: SegmentFactors::Bit4(&factors),
            rescore: &rescore,
            columns: &columns,
            alive: &alive,
        },
        GraphNodeBlockBuild {
            layout: GraphNodeLayout::new(dims, dims, 2).expect("graph layout"),
            nodes: &nodes,
        },
        SegmentDocumentVersions {
            doc_ids: &doc_ids,
            revisions: &revisions,
        },
        policy,
    )
    .expect("write graph segment");
    meta.clustering_key_range = ClusteringKeyRange::Bounded {
        min_ts: 1,
        max_ts: 3,
    };
    commit_manifest(
        &StdVfs,
        directory,
        &Manifest {
            generation: 1,
            log_seq: 0,
            segments: vec![meta],
            epochs: Vec::new(),
            schema: columns.schema().clone(),
        },
        policy,
    )
    .expect("commit graph manifest");
}

fn publish_int8_document_segment(
    directory: &Path,
) -> (DocumentVersion, DocumentVersion, DocumentVersion) {
    let id = SegmentId::new(901, [0x42; 10]);
    let versions = [
        DocumentVersion::new(DocId::new(901), Revision::new(1)),
        DocumentVersion::new(DocId::new(902), Revision::new(1)),
        DocumentVersion::new(DocId::new(903), Revision::new(1)),
    ];
    let dims = 2_u32;
    let codes = [127_u8, 0, 0, 127, 129, 0];
    let factors = [Int8Factors {
        scale: 1.0 / 127.0,
        offset: 0.0,
    }; 3];
    let rescore = [1.0_f32, 0.0, 0.0, 1.0, -1.0, 0.0];
    let mut columns = ColumnStoreBuilder::new(Schema::new(Vec::new()).expect("Int8 schema"));
    for timestamp in [10_i64, 20, 30] {
        columns.push_row(timestamp, &[]).expect("Int8 column row");
    }
    let columns = columns.finish().expect("Int8 columns");
    let mut alive = AliveSet::new(3);
    alive.tombstone(2).expect("tombstone Int8 survivor");
    let doc_ids = versions.map(DocumentVersion::doc_id);
    let revisions = versions.map(DocumentVersion::revision);
    let policy =
        DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::Ordered).expect("Int8 policy");
    let meta = write_segment_with_documents(
        &StdVfs,
        directory,
        SegmentBuild {
            id,
            scheme: 2,
            dims,
            codes: &codes,
            factors: SegmentFactors::Int8(&factors),
            rescore: &rescore,
            columns: &columns,
            alive: &alive,
        },
        SegmentDocumentVersions {
            doc_ids: &doc_ids,
            revisions: &revisions,
        },
        policy,
    )
    .expect("write Int8 document segment");
    commit_manifest(
        &StdVfs,
        directory,
        &Manifest {
            generation: 1,
            log_seq: 0,
            segments: vec![meta],
            epochs: Vec::new(),
            schema: columns.schema().clone(),
        },
        policy,
    )
    .expect("commit Int8 manifest");
    (versions[0], versions[1], versions[2])
}

fn publish_all_column_document_segment(
    directory: &Path,
) -> (DocumentVersion, DocumentVersion, DocumentVersion) {
    let versions = [
        DocumentVersion::new(DocId::new(911), Revision::new(1)),
        DocumentVersion::new(DocId::new(912), Revision::new(1)),
        DocumentVersion::new(DocId::new(913), Revision::new(1)),
    ];
    let schema = Schema::new(vec![
        ColumnDefinition::new(ColumnId::new(1), "u", ColumnType::U64, true),
        ColumnDefinition::new(ColumnId::new(2), "i", ColumnType::I64, true),
        ColumnDefinition::new(ColumnId::new(3), "f", ColumnType::F64, true),
        ColumnDefinition::new(ColumnId::new(4), "b", ColumnType::Bool, true),
        ColumnDefinition::new(ColumnId::new(5), "d", ColumnType::DictionaryString, true),
        ColumnDefinition::new(ColumnId::new(6), "r", ColumnType::RawString, true),
    ])
    .expect("typed purge schema");
    let mut builder = ColumnStoreBuilder::new(schema);
    builder
        .push_row(
            10,
            &[
                ColumnInput {
                    column: ColumnId::new(1),
                    value: ColumnValue::U64(11),
                },
                ColumnInput {
                    column: ColumnId::new(2),
                    value: ColumnValue::I64(-12),
                },
                ColumnInput {
                    column: ColumnId::new(3),
                    value: ColumnValue::F64(-0.0),
                },
                ColumnInput {
                    column: ColumnId::new(4),
                    value: ColumnValue::Bool(true),
                },
                ColumnInput {
                    column: ColumnId::new(5),
                    value: ColumnValue::String("alpha"),
                },
                ColumnInput {
                    column: ColumnId::new(6),
                    value: ColumnValue::String("raw-alpha"),
                },
            ],
        )
        .expect("first typed row");
    builder
        .push_row(
            20,
            &[
                ColumnInput {
                    column: ColumnId::new(1),
                    value: ColumnValue::U64(21),
                },
                ColumnInput {
                    column: ColumnId::new(2),
                    value: ColumnValue::I64(-22),
                },
                ColumnInput {
                    column: ColumnId::new(3),
                    value: ColumnValue::F64(2.5),
                },
                ColumnInput {
                    column: ColumnId::new(4),
                    value: ColumnValue::Bool(false),
                },
                ColumnInput {
                    column: ColumnId::new(5),
                    value: ColumnValue::String("remove"),
                },
                ColumnInput {
                    column: ColumnId::new(6),
                    value: ColumnValue::String("raw-remove"),
                },
            ],
        )
        .expect("removed typed row");
    builder.push_row(30, &[]).expect("nullable retained row");
    let columns = builder.finish().expect("typed purge columns");
    let alive = AliveSet::new(3);
    let factors = [Bit4Factors::from_persisted(1.0, 1.0, 0.0); 3];
    let doc_ids = versions.map(DocumentVersion::doc_id);
    let revisions = versions.map(DocumentVersion::revision);
    let policy = DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::Ordered)
        .expect("typed purge policy");
    let meta = write_segment_with_documents(
        &StdVfs,
        directory,
        SegmentBuild {
            id: SegmentId::new(902, [0x43; 10]),
            scheme: 4,
            dims: 2,
            codes: &[0x80, 0x08, 0x77],
            factors: SegmentFactors::Bit4(&factors),
            rescore: &[1.0, 0.0, 0.0, 1.0, -1.0, 0.0],
            columns: &columns,
            alive: &alive,
        },
        SegmentDocumentVersions {
            doc_ids: &doc_ids,
            revisions: &revisions,
        },
        policy,
    )
    .expect("write typed purge segment");
    commit_manifest(
        &StdVfs,
        directory,
        &Manifest {
            generation: 1,
            log_seq: 0,
            segments: vec![meta],
            epochs: Vec::new(),
            schema: columns.schema().clone(),
        },
        policy,
    )
    .expect("commit typed purge manifest");
    (versions[0], versions[1], versions[2])
}

#[derive(Clone, Default)]
struct BlockingDeleteVfs {
    state: Arc<(Mutex<DeleteBlockState>, Condvar)>,
}

#[derive(Default)]
struct DeleteBlockState {
    blocked: bool,
    released: bool,
}

impl BlockingDeleteVfs {
    fn wait_until_delete_blocked(&self) {
        let mut state = self.state.0.lock().expect("delete-block mutex");
        while !state.blocked {
            state = self.state.1.wait(state).expect("delete-block wait");
        }
    }

    fn release_delete(&self) {
        let mut state = self.state.0.lock().expect("delete-block mutex");
        state.released = true;
        self.state.1.notify_all();
    }
}

impl Vfs for BlockingDeleteVfs {
    fn open(&self, path: &Path) -> std::io::Result<u64> {
        StdVfs.open(path)
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        StdVfs.read(path)
    }

    fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        StdVfs.read_range(path, offset, length)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        StdVfs.write(path, bytes)
    }

    fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
        StdVfs.open_append(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        StdVfs.rename(from, to)
    }

    fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
        StdVfs.sync(path, kind)
    }

    fn list(&self, directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        StdVfs.list(directory)
    }

    fn delete(&self, path: &Path) -> std::io::Result<()> {
        let is_segment = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("segment-") && name.ends_with(".zseg"));
        if is_segment {
            let mut state = self
                .state
                .0
                .lock()
                .map_err(|_| std::io::Error::other("delete-block mutex poisoned"))?;
            state.blocked = true;
            self.state.1.notify_all();
            while !state.released {
                state = self
                    .state
                    .1
                    .wait(state)
                    .map_err(|_| std::io::Error::other("delete-block wait poisoned"))?;
            }
        }
        StdVfs.delete(path)
    }
}
