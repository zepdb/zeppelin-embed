#![allow(clippy::expect_used)]

use tempfile::tempdir;
use zeppelin_embed::fts::{index::DEFAULT_FIELD, search::TermQuery};
use zeppelin_embed::fusion::HybridQuery;
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, Revision, SearchRequest,
};
use zeppelin_embed::lifecycle::{
    CancelToken, DocumentFields, OpenOptions, QueryControl, SearchOptions, Store,
};
use zeppelin_embed::segment::layout::RegionKind;
use zeppelin_embed::segment::reader::{SegmentCostAudit, SegmentReader};

fn control() -> QueryControl {
    QueryControl::Cancel(CancelToken::new())
}

fn terms() -> TermQuery {
    TermQuery::flat(vec![b"common".to_vec()], &[DEFAULT_FIELD])
}

fn ingest(store: &Store, first: u128, rows: u128) {
    store
        .ingest(IngestBatch::new(
            (first..first + rows)
                .map(|id| {
                    IngestDocument::new(
                        DocumentVersion::new(DocId::new(id), Revision::new(1)),
                        vec![id as f32, 1.0],
                    )
                    .with_text(if id.is_multiple_of(2) {
                        "common blue"
                    } else {
                        "common red red"
                    })
                })
                .collect(),
        ))
        .expect("ingest");
}

fn lexical(store: &Store) -> Vec<(DocId, u64)> {
    store
        .search_lexical(&terms(), 10, control())
        .expect("lexical")
        .candidates
        .into_iter()
        .map(|hit| (hit.document.doc_id(), hit.score.to_bits()))
        .collect()
}

fn queries(store: &Store) -> Vec<Vec<(DocId, u64)>> {
    // Ingest warms identity lookup indices. Exercise the same public lookup
    // after reopen so both accounting observations include those indices.
    assert_eq!(
        store
            .get_documents(&[DocId::new(u128::MAX)], DocumentFields::NONE)
            .expect("missing document lookup"),
        vec![None]
    );
    let dense = store
        .search(
            SearchRequest::new(&[1.0, 0.5]),
            10,
            SearchOptions::default(),
            control(),
        )
        .expect("dense")
        .candidates
        .into_iter()
        .map(|hit| {
            (
                hit.document().expect("identity").doc_id(),
                u64::from(hit.score().to_bits()),
            )
        })
        .collect();
    let hybrid = store
        .search_hybrid(
            SearchRequest::new(&[1.0, 0.5]),
            &terms(),
            &HybridQuery::new(10),
            SearchOptions::default(),
            control(),
        )
        .expect("hybrid")
        .hits
        .into_iter()
        .map(|hit| (hit.key, hit.fused_score.to_bits()))
        .collect();
    vec![lexical(store), dense, hybrid]
}

fn build(store: &Store, segments: u128) {
    for segment in 0..segments {
        ingest(store, segment * 4 + 1, 4);
        store.seal().expect("seal");
    }
    queries(store);
}

fn length(reader: &SegmentReader, kind: RegionKind) -> u64 {
    reader
        .directory()
        .iter()
        .find(|entry| entry.kind == kind.id())
        .expect("region")
        .length
}

#[test]
fn one_row_seal_decodes_only_the_new_segment() {
    let directory = tempdir().expect("directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    build(&store, 10);
    let audit = SegmentCostAudit::new();
    audit.measure(|| {
        ingest(&store, 41, 1);
        store.seal().expect("one-row seal");
        lexical(&store);
    });
    let snapshot = store.snapshot().expect("snapshot");
    let new = snapshot.segments().last().expect("eleventh segment");
    assert_eq!(snapshot.segments().len(), 11);
    let work = audit.snapshot();
    assert_eq!(work.postings_hash_bytes, length(new, RegionKind::Postings));
    assert_eq!(
        work.postings_decode_bytes,
        length(new, RegionKind::Postings)
    );
    assert_eq!(work.alive_decode_bytes, length(new, RegionKind::Alive));
    assert_eq!(
        work.stored_text_hash_bytes,
        length(new, RegionKind::StoredText)
    );
}

#[test]
fn shared_readers_are_accounted_once_and_match_fresh_open() {
    let directory = tempdir().expect("directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    build(&store, 10);
    let lease = store.snapshot().expect("pre-seal lease");
    ingest(&store, 41, 1);
    store.seal().expect("one-row seal");
    let distinct_bytes = store
        .snapshot()
        .expect("snapshot")
        .segments()
        .iter()
        .map(|reader| reader.meta().file_size)
        .sum::<u64>();
    assert_eq!(
        store.stats().expect("held lease stats").mapped_bytes,
        distinct_bytes
    );
    drop(lease);
    let expected = queries(&store);
    let before = store.stats().expect("stats");
    store.close().expect("close");
    let reopened = Store::open(directory.path(), OpenOptions::default()).expect("reopen");
    assert_eq!(queries(&reopened), expected);
    let after = reopened.stats().expect("fresh stats");
    assert_eq!(after.snapshot_bytes, before.snapshot_bytes);
    assert_eq!(after.mapped_bytes, before.mapped_bytes);
}

#[test]
fn purge_and_merge_replacements_are_read_fresh() {
    let directory = tempdir().expect("directory");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("open");
    build(&store, 3);
    let old_ids = store
        .snapshot()
        .expect("snapshot")
        .segments()
        .iter()
        .map(|reader| reader.meta().id)
        .collect::<Vec<_>>();
    let token = store.purge(&[DocId::new(1)]).expect("purge");
    store.await_physical_purge(token).expect("physical purge");
    let replacement_bytes = {
        let snapshot = store.snapshot().expect("purged snapshot");
        let replacements = snapshot
            .segments()
            .iter()
            .filter(|reader| !old_ids.contains(&reader.meta().id))
            .collect::<Vec<_>>();
        assert_eq!(replacements.len(), 1);
        length(
            replacements.first().expect("replacement"),
            RegionKind::Postings,
        )
    };
    let audit = SegmentCostAudit::new();
    let result = audit.measure(|| lexical(&store));
    assert!(!result.iter().any(|(id, _)| *id == DocId::new(1)));
    assert_eq!(audit.snapshot().postings_hash_bytes, replacement_bytes);
    let expected = queries(&store);
    store.close().expect("close purged store");
    let store = Store::open(directory.path(), OpenOptions::default()).expect("reopen purged store");
    assert_eq!(queries(&store), expected);
    store.merge_sealed().expect("merge");
    let merged_bytes = {
        let snapshot = store.snapshot().expect("merged snapshot");
        assert_eq!(snapshot.segments().len(), 1);
        length(
            snapshot.segments().first().expect("merged reader"),
            RegionKind::Postings,
        )
    };
    let audit = SegmentCostAudit::new();
    audit.measure(|| lexical(&store));
    assert_eq!(audit.snapshot().postings_hash_bytes, merged_bytes);
    let expected = queries(&store);
    store.close().expect("close merged store");
    let reopened =
        Store::open(directory.path(), OpenOptions::default()).expect("reopen merged store");
    assert_eq!(queries(&reopened), expected);
}
