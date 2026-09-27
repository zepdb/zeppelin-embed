#![allow(clippy::expect_used)]
use tempfile::tempdir;
use zeppelin_embed::ingest::{DocId, DocumentVersion, IngestBatch, IngestDocument, Revision};
use zeppelin_embed::lifecycle::{OpenOptions, Store};
use zeppelin_embed::segment::layout::RegionKind;

#[test]
fn reindex_text_replaces_postings_preserves_other_regions_and_reopens() {
    let dir = tempdir().expect("directory");
    let store = Store::open(dir.path(), OpenOptions::default()).expect("open");
    for id in 1..=2 {
        store
            .ingest(IngestBatch::new(vec![
                IngestDocument::new(
                    DocumentVersion::new(DocId::new(id), Revision::new(1)),
                    vec![1.0, 0.0],
                )
                .with_text("searchable meeting notes"),
            ]))
            .expect("ingest");
        store.seal().expect("seal");
    }
    let before = store.snapshot().expect("snapshot");
    let generation = store.reindex_text().expect("reindex");
    let after = store.snapshot().expect("snapshot");
    assert!(generation > before.generation());
    assert_eq!(after.segments().len(), 2);
    for (old, new) in before.segments().iter().zip(after.segments()) {
        assert_ne!(old.meta().id, new.meta().id);
        for entry in old.directory() {
            if let Some(kind) = RegionKind::from_id(entry.kind) {
                if kind != RegionKind::ChecksumTable {
                    assert_eq!(
                        old.region(kind).expect("old region"),
                        new.region(kind).expect("new region")
                    );
                }
            }
        }
    }
    drop(after);
    drop(before);
    store.close().expect("close");
    let reopened = Store::open(dir.path(), OpenOptions::default()).expect("reopen");
    assert_eq!(
        reopened.snapshot().expect("snapshot").generation(),
        generation
    );
}

#[test]
#[allow(clippy::indexing_slicing)]
fn newer_segment_region_retains_typed_version_range() {
    let dir = tempdir().expect("directory");
    let store = Store::open(dir.path(), OpenOptions::default()).expect("open");
    store
        .ingest(IngestBatch::new(vec![
            IngestDocument::new(
                DocumentVersion::new(DocId::new(1), Revision::new(1)),
                vec![1.0, 0.0],
            )
            .with_text("text"),
        ]))
        .expect("ingest");
    store.seal().expect("seal");
    let snapshot = store.snapshot().expect("snapshot");
    let path = dir
        .path()
        .join(snapshot.segments()[0].meta().id.file_name());
    drop(snapshot);
    store.close().expect("close");
    let mut bytes = std::fs::read(&path).expect("segment bytes");
    let header_len = u64::from_le_bytes(bytes[16..24].try_into().expect("header width")) as usize;
    bytes[66..68].copy_from_slice(&99_u16.to_le_bytes());
    let checksum = xxhash_rust::xxh3::xxh3_64(&bytes[..header_len - 8]);
    bytes[header_len - 8..header_len].copy_from_slice(&checksum.to_le_bytes());
    let tail = bytes.len() - 8;
    let checksum = xxhash_rust::xxh3::xxh3_64(&bytes[..tail]);
    bytes[tail..].copy_from_slice(&checksum.to_le_bytes());
    std::fs::write(&path, bytes).expect("future region fixture");
    let error = match Store::open(dir.path(), OpenOptions::default()) {
        Err(error) => error,
        Ok(_) => panic!("future region accepted"),
    };
    let zeppelin_embed::lifecycle::StoreError::Segment(
        zeppelin_embed::segment::SegmentError::Format(error),
    ) = error
    else {
        panic!("unexpected error: {error}")
    };
    assert_eq!(error.version_range(), Some((99, 1, 1)));
}
