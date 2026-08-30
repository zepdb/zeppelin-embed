#![allow(clippy::expect_used, clippy::indexing_slicing)]

use tempfile::tempdir;
use zeppelin_embed::fts::index::{DEFAULT_FIELD, Document, SegmentIndex};
use zeppelin_embed::fts::sealed::SealedSegment;
use zeppelin_embed::fts::search::TermQuery;
use zeppelin_embed::fts::tokenizer::{Analyzer, TokenizerConfig};
use zeppelin_embed::graph::block::{GraphNodeBlockBuild, GraphNodeBlockInput, GraphNodeLayout};
use zeppelin_embed::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::{
    CancelToken, GraphSearchOptions, OpenOptions, QueryControl, QueryError, SearchOptions,
    SearchTier, Store, StoreError,
};
use zeppelin_embed::manifest::Manifest;
use zeppelin_embed::manifest::io::commit_manifest;
use zeppelin_embed::meta::{
    AliveSet, ColumnStoreBuilder, Predicate, PredicateValue, Schema, TIMESTAMP_COLUMN,
};
use zeppelin_embed::quant::{Bit4Factors, quantize_bit4};
use zeppelin_embed::scan::ScanOptions;
use zeppelin_embed::segment::layout::RegionKind;
use zeppelin_embed::segment::reader::{SegmentCostAudit, SegmentReader};
use zeppelin_embed::segment::writer::{
    SegmentBuild, SegmentDocumentVersions, SegmentFactors, SegmentPostings,
    write_segment_with_documents, write_segment_with_documents_and_postings,
    write_segment_with_graph_and_documents,
};
use zeppelin_embed::segment::{SegmentError, SegmentId};
use zeppelin_embed::vfs::StdVfs;

const ROWS: usize = 256;
const MERGE_K: usize = 100;
const GRAPH_ROWS: usize = 12;
const GRAPH_DIMS: usize = 128;

fn identity_segment() -> (tempfile::TempDir, SegmentId) {
    let directory = tempdir().expect("segment directory");
    let id = SegmentId::new(0x0203_0405_0607, [0x52; 10]);
    let mut columns = ColumnStoreBuilder::new(Schema::timestamp_only());
    for row in 0..ROWS {
        columns.push_row(row as i64, &[]).expect("timestamp row");
    }
    let columns = columns.finish().expect("columns");
    let alive = AliveSet::new(ROWS as u32);
    let codes = vec![0x88_u8; ROWS];
    let factors = vec![Bit4Factors::from_persisted(1.0, 1.0, 1.0); ROWS];
    let rescore = vec![0.0_f32; ROWS * 2];
    let versions = (0..ROWS)
        .map(|row| {
            DocumentVersion::new(
                DocId::new(0x1000_u128 + row as u128),
                Revision::new(row as u64 + 1),
            )
        })
        .collect::<Vec<_>>();
    let doc_ids = versions
        .iter()
        .map(|version| version.doc_id())
        .collect::<Vec<_>>();
    let revisions = versions
        .iter()
        .map(|version| version.revision())
        .collect::<Vec<_>>();
    write_segment_with_documents(
        &StdVfs,
        directory.path(),
        SegmentBuild {
            id,
            scheme: 4,
            dims: 2,
            codes: &codes,
            factors: SegmentFactors::Bit4(&factors),
            rescore: &rescore,
            columns: &columns,
            alive: &alive,
        },
        SegmentDocumentVersions {
            doc_ids: &doc_ids,
            revisions: &revisions,
        },
        DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None).expect("derived policy"),
    )
    .expect("write identity segment");
    (directory, id)
}

fn graph_store() -> (tempfile::TempDir, Vec<f32>, u64) {
    let directory = tempdir().expect("graph store directory");
    let id = SegmentId::new(0x0304_0506_0708, [0x53; 10]);
    let mut columns = ColumnStoreBuilder::new(Schema::timestamp_only());
    for row in 0..GRAPH_ROWS {
        columns
            .push_row(row as i64, &[])
            .expect("graph timestamp row");
    }
    let columns = columns.finish().expect("graph columns");
    let alive = AliveSet::new(GRAPH_ROWS as u32);
    let vectors = (0..GRAPH_ROWS)
        .flat_map(|row| {
            (0..GRAPH_DIMS).map(move |dimension| {
                let sign = if dimension.is_multiple_of(2) {
                    1.0
                } else {
                    -1.0
                };
                row as f32 * sign
            })
        })
        .collect::<Vec<_>>();
    let row_bytes = GRAPH_DIMS.div_ceil(2);
    let mut codes = vec![0_u8; GRAPH_ROWS * row_bytes];
    let mut factors = Vec::with_capacity(GRAPH_ROWS);
    for (row, encoded) in vectors
        .chunks_exact(GRAPH_DIMS)
        .zip(codes.chunks_exact_mut(row_bytes))
    {
        factors.push(quantize_bit4(row, encoded).expect("quantize graph row"));
    }
    let neighbors = (0..GRAPH_ROWS)
        .map(|row| {
            (0..GRAPH_ROWS)
                .filter(|neighbor| *neighbor != row)
                .map(|neighbor| neighbor as u32)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let nodes = codes
        .chunks_exact(row_bytes)
        .zip(&factors)
        .zip(&neighbors)
        .enumerate()
        .map(|(row, ((codes, factors), neighbors))| GraphNodeBlockInput {
            codes,
            factors: *factors,
            flags: u8::from(row < 4),
            neighbors,
        })
        .collect::<Vec<_>>();
    let versions = (0..GRAPH_ROWS)
        .map(|row| DocumentVersion::new(DocId::new(0x2000_u128 + row as u128), Revision::new(1)))
        .collect::<Vec<_>>();
    let doc_ids = versions
        .iter()
        .map(|version| version.doc_id())
        .collect::<Vec<_>>();
    let revisions = versions
        .iter()
        .map(|version| version.revision())
        .collect::<Vec<_>>();
    let policy =
        DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None).expect("graph policy");
    let meta = write_segment_with_graph_and_documents(
        &StdVfs,
        directory.path(),
        SegmentBuild {
            id,
            scheme: 4,
            dims: GRAPH_DIMS as u32,
            codes: &codes,
            factors: SegmentFactors::Bit4(&factors),
            rescore: &vectors,
            columns: &columns,
            alive: &alive,
        },
        GraphNodeBlockBuild {
            layout: GraphNodeLayout::new(
                GRAPH_DIMS as u32,
                GRAPH_DIMS as u32,
                (GRAPH_ROWS - 1) as u8,
            )
            .expect("graph layout"),
            nodes: &nodes,
        },
        SegmentDocumentVersions {
            doc_ids: &doc_ids,
            revisions: &revisions,
        },
        policy,
    )
    .expect("write graph segment");
    let rescore_bytes = meta.file_size;
    commit_manifest(
        &StdVfs,
        directory.path(),
        &Manifest {
            generation: 1,
            log_seq: 0,
            segments: vec![meta],
            epochs: Vec::new(),
            epoch_alias: None,
            schema: columns.schema().clone(),
        },
        policy,
    )
    .expect("commit graph manifest");
    (directory, vectors, rescore_bytes)
}

fn lexical_store() -> tempfile::TempDir {
    const LEXICAL_ROWS: usize = 3;
    let directory = tempdir().expect("lexical store directory");
    let id = SegmentId::new(0x0405_0607_0809, [0x54; 10]);
    let mut columns = ColumnStoreBuilder::new(Schema::timestamp_only());
    for row in 0..LEXICAL_ROWS {
        columns
            .push_row(row as i64, &[])
            .expect("lexical timestamp row");
    }
    let columns = columns.finish().expect("lexical columns");
    let alive = AliveSet::new(LEXICAL_ROWS as u32);
    let analyzer = Analyzer::new(TokenizerConfig::text_default()).expect("lexical analyzer");
    let mut index = SegmentIndex::new();
    for text in ["bronze zeppelin", "silver zeppelin", "bronze balloon"] {
        index
            .push_document(&analyzer, &Document::with_text(text))
            .expect("index lexical row");
    }
    let postings = SealedSegment::seal(&index)
        .expect("seal postings")
        .encode_region()
        .expect("encode postings");
    let vectors = [1.0_f32, 0.0, 0.0, 1.0, 0.5, 0.5];
    let codes = vectors
        .iter()
        .flat_map(|value| value.to_bits().to_le_bytes())
        .collect::<Vec<_>>();
    let versions = (0..LEXICAL_ROWS)
        .map(|row| DocumentVersion::new(DocId::new(0x3000_u128 + row as u128), Revision::new(1)))
        .collect::<Vec<_>>();
    let doc_ids = versions
        .iter()
        .map(|version| version.doc_id())
        .collect::<Vec<_>>();
    let revisions = versions
        .iter()
        .map(|version| version.revision())
        .collect::<Vec<_>>();
    let policy =
        DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None).expect("lexical policy");
    let meta = write_segment_with_documents_and_postings(
        &StdVfs,
        directory.path(),
        SegmentBuild {
            id,
            scheme: 0,
            dims: 2,
            codes: &codes,
            factors: SegmentFactors::F32,
            rescore: &vectors,
            columns: &columns,
            alive: &alive,
        },
        SegmentDocumentVersions {
            doc_ids: &doc_ids,
            revisions: &revisions,
        },
        SegmentPostings { bytes: &postings },
        policy,
    )
    .expect("write lexical segment");
    commit_manifest(
        &StdVfs,
        directory.path(),
        &Manifest {
            generation: 1,
            log_seq: 0,
            segments: vec![meta],
            epochs: Vec::new(),
            epoch_alias: None,
            schema: columns.schema().clone(),
        },
        policy,
    )
    .expect("commit lexical manifest");
    directory
}

#[test]
fn document_version_hashes_the_region_once_per_reader() {
    let (directory, id) = identity_segment();
    let path = directory.path().join(id.file_name());
    let region_bytes = (ROWS * 24) as u64;

    let full_reader = SegmentReader::open(&StdVfs, &path, id).expect("open full-iteration reader");
    assert_eq!(
        full_reader
            .directory()
            .iter()
            .find(|entry| entry.kind == RegionKind::DocumentVersions.id())
            .expect("document-version region")
            .length,
        region_bytes
    );
    let full_audit = SegmentCostAudit::new();
    full_audit.measure(|| {
        for row in 0..ROWS {
            assert!(
                full_reader
                    .document_version(row)
                    .expect("full-iteration identity")
                    .is_some()
            );
        }
    });
    assert_eq!(
        full_audit.snapshot().identity_hash_bytes,
        region_bytes,
        "full iteration must hash the identity region once"
    );

    let merge_reader = SegmentReader::open(&StdVfs, &path, id).expect("open merge reader");
    let merge_audit = SegmentCostAudit::new();
    merge_audit.measure(|| {
        for row in 0..MERGE_K {
            assert!(
                merge_reader
                    .document_version(row)
                    .expect("merge identity")
                    .is_some()
            );
        }
    });
    assert_eq!(
        merge_audit.snapshot().identity_hash_bytes,
        region_bytes,
        "k=100 merge must hash the identity region once"
    );
}

#[test]
fn warm_graph_query_does_not_rehash_rescore_region() {
    let (directory, vectors, _segment_bytes) = graph_store();
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open graph store");
    let snapshot = store.snapshot().expect("graph snapshot");
    let rescore_region_bytes = snapshot.segments()[0]
        .directory()
        .iter()
        .find(|entry| entry.kind == RegionKind::VectorRescore.id())
        .expect("rescore region")
        .length;
    drop(snapshot);
    let query = vectors
        .get(5 * GRAPH_DIMS..6 * GRAPH_DIMS)
        .expect("query row");
    let options =
        SearchOptions::new(ScanOptions { thread_budget: 1 }).with_tier(SearchTier::Graph(
            GraphSearchOptions::new(zeppelin_embed::graph::search::GraphSearchProfile::SiftClass)
                .with_ef(GRAPH_ROWS),
        ));
    let audit = SegmentCostAudit::new();
    audit.measure(|| {
        store
            .search(
                zeppelin_embed::ingest::SearchRequest::new(query),
                4,
                options,
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect("first graph query");
    });
    let first = audit.snapshot();
    assert_eq!(first.rescore_hash_bytes, rescore_region_bytes);
    audit.measure(|| {
        store
            .search(
                zeppelin_embed::ingest::SearchRequest::new(query),
                4,
                options,
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect("second graph query");
    });
    let warm = audit.snapshot();
    assert_eq!(
        warm.rescore_hash_bytes, first.rescore_hash_bytes,
        "the identical warm query must not rehash exact-rescore bytes"
    );
    store.close().expect("close graph store");
}

#[test]
fn warm_lexical_query_does_not_redecode_postings() {
    let directory = lexical_store();
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open lexical store");
    let term = TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]);
    let audit = SegmentCostAudit::new();
    audit.measure(|| {
        store
            .search_lexical(&term, 2, QueryControl::Cancel(CancelToken::new()))
            .expect("first lexical query");
    });
    let first = audit.snapshot();
    assert_eq!(first.postings_hash_bytes, 480);
    assert_eq!(first.postings_decode_bytes, first.postings_hash_bytes);
    audit.measure(|| {
        store
            .search_lexical(&term, 2, QueryControl::Cancel(CancelToken::new()))
            .expect("second lexical query");
    });
    let warm = audit.snapshot();
    assert_eq!(
        warm.postings_hash_bytes, first.postings_hash_bytes,
        "the identical warm query must not rehash sealed postings"
    );
    assert_eq!(
        warm.postings_decode_bytes, first.postings_decode_bytes,
        "the identical warm query must not heap-decode sealed postings"
    );
    store.close().expect("close lexical store");
}

#[test]
fn warm_filtered_query_does_not_redecode_columns_or_alive_state() {
    let directory = lexical_store();
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open filtered store");
    let predicate = Predicate::Eq {
        column: TIMESTAMP_COLUMN,
        value: PredicateValue::I64(1),
    };
    let audit = SegmentCostAudit::new();
    let run = || {
        store
            .search_filtered(
                zeppelin_embed::ingest::SearchRequest::new(&[0.0, 1.0]),
                &predicate,
                2,
                SearchOptions::new(ScanOptions { thread_budget: 1 }).with_tier(SearchTier::Scan),
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect("filtered query");
    };
    audit.measure(run);
    let first = audit.snapshot();
    assert_eq!(first.columns_hash_bytes, 51);
    assert_eq!(first.columns_decode_bytes, first.columns_hash_bytes);
    assert_eq!(first.alive_hash_bytes, 9);
    assert_eq!(first.alive_decode_bytes, first.alive_hash_bytes);
    audit.measure(run);
    let warm = audit.snapshot();
    assert_eq!(
        warm.columns_hash_bytes, first.columns_hash_bytes,
        "the identical warm query must not rehash typed columns"
    );
    assert_eq!(
        warm.columns_decode_bytes, first.columns_decode_bytes,
        "the identical warm query must not rebuild typed columns"
    );
    assert_eq!(
        warm.alive_hash_bytes, first.alive_hash_bytes,
        "the identical warm query must not rehash alive state"
    );
    assert_eq!(
        warm.alive_decode_bytes, first.alive_decode_bytes,
        "the identical warm query must not rebuild alive state"
    );
    store.close().expect("close filtered store");
}

#[test]
fn snapshot_query_view_bytes_are_exactly_accounted() {
    let directory = lexical_store();
    let store =
        Store::open(directory.path(), OpenOptions::default()).expect("open accounting store");
    let before = store.stats().expect("stats before query-view decode");
    let term = TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]);
    store
        .search_lexical(&term, 2, QueryControl::Cancel(CancelToken::new()))
        .expect("populate postings and alive views");
    store
        .search_filtered(
            zeppelin_embed::ingest::SearchRequest::new(&[0.0, 1.0]),
            &Predicate::Eq {
                column: TIMESTAMP_COLUMN,
                value: PredicateValue::I64(1),
            },
            2,
            SearchOptions::new(ScanOptions { thread_budget: 1 }).with_tier(SearchTier::Scan),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("populate columns view");
    let snapshot = store.snapshot().expect("accounted snapshot");
    let retained = snapshot
        .segments()
        .iter()
        .map(SegmentReader::retained_query_view_bytes)
        .sum::<u64>();
    drop(snapshot);
    let after = store.stats().expect("stats after query-view decode");
    assert!(retained > 0);
    assert_eq!(
        after.snapshot_bytes - before.snapshot_bytes,
        retained,
        "every retained decoded query-view byte must reach snapshot accounting"
    );
    store.close().expect("close accounting store");
}

#[test]
fn active_lexical_view_is_sealed_once_per_active_generation() {
    let directory = tempdir().expect("active lexical directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open active store");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(0x4000), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_text("active zeppelin"),
        ]))
        .expect("ingest active text");
    let term = TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]);
    let audit = SegmentCostAudit::new();
    audit.measure(|| {
        store
            .search_lexical(&term, 1, QueryControl::Cancel(CancelToken::new()))
            .expect("first active lexical query");
    });
    let first = audit.snapshot();
    assert!(first.postings_decode_bytes > 0);
    audit.measure(|| {
        store
            .search_lexical(&term, 1, QueryControl::Cancel(CancelToken::new()))
            .expect("second active lexical query");
    });
    assert_eq!(
        audit.snapshot().postings_decode_bytes,
        first.postings_decode_bytes,
        "the identical active generation must not be resealed"
    );
    store
        .stats()
        .expect("active cache remains exactly accounted");
    store.close().expect("close active lexical store");
}

#[test]
fn touched_rescore_chunk_corruption_still_fails_loudly_and_restores() {
    let (directory, vectors, _segment_bytes) = graph_store();
    let segment_path = std::fs::read_dir(directory.path())
        .expect("list graph directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "zseg")
        })
        .expect("graph segment path");
    let id = SegmentId::new(0x0304_0506_0708, [0x53; 10]);
    let reader = SegmentReader::open(&StdVfs, &segment_path, id).expect("locate rescore region");
    let rescore_offset = reader
        .directory()
        .iter()
        .find(|entry| entry.kind == RegionKind::VectorRescore.id())
        .and_then(|entry| usize::try_from(entry.offset).ok())
        .expect("rescore offset");
    drop(reader);
    let original = std::fs::read(&segment_path).expect("read graph segment");
    let mut corrupted = original.clone();
    corrupted[rescore_offset + 64] ^= 0x80;
    std::fs::write(&segment_path, &corrupted).expect("install touched-chunk corruption");

    let store = Store::open(directory.path(), OpenOptions::default()).expect("open corrupt store");
    let query = vectors
        .get(5 * GRAPH_DIMS..6 * GRAPH_DIMS)
        .expect("corruption query row");
    let options =
        SearchOptions::new(ScanOptions { thread_budget: 1 }).with_tier(SearchTier::Graph(
            GraphSearchOptions::new(zeppelin_embed::graph::search::GraphSearchProfile::SiftClass)
                .with_ef(GRAPH_ROWS),
        ));
    let error = store
        .search(
            zeppelin_embed::ingest::SearchRequest::new(query),
            4,
            options,
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect_err("touched rescore corruption must fail");
    assert!(matches!(
        error,
        QueryError::Store(StoreError::Segment(SegmentError::Geometry(detail)))
            if detail.contains("BlockChecksum")
    ));
    store.close().expect("close corrupt store");

    std::fs::write(&segment_path, &original).expect("restore graph segment");
    let restored =
        Store::open(directory.path(), OpenOptions::default()).expect("open restored store");
    restored
        .search(
            zeppelin_embed::ingest::SearchRequest::new(query),
            4,
            options,
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("restored graph query");
    restored.close().expect("close restored store");
}
