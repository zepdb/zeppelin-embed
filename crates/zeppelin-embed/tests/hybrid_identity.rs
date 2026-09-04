#![allow(clippy::expect_used)]

use tempfile::tempdir;
use zeppelin_embed::fts::index::{DEFAULT_FIELD, Document, SegmentIndex};
use zeppelin_embed::fts::query::LexicalQuery;
use zeppelin_embed::fts::sealed::SealedSegment;
use zeppelin_embed::fts::search::TermQuery;
use zeppelin_embed::fts::tokenizer::{Analyzer, TokenizerConfig};
use zeppelin_embed::fusion::{FusionError, FusionLeg, HybridQuery};
use zeppelin_embed::ingest::{DocId, Revision, SearchRequest, StoreLexicalError};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, SearchOptions, Store};
use zeppelin_embed::manifest::Manifest;
use zeppelin_embed::manifest::io::commit_manifest;
use zeppelin_embed::meta::{AliveSet, ColumnStoreBuilder, Schema};
use zeppelin_embed::segment::SegmentId;
use zeppelin_embed::segment::writer::{
    SegmentBuild, SegmentDocumentVersions, SegmentFactors, SegmentPostings,
    write_segment_with_documents_and_postings, write_segment_with_postings,
};
use zeppelin_embed::vfs::StdVfs;

/// Publishes a single sealed segment that carries postings but no
/// document-identity region, and opens a store over it.
fn identity_free_store(directory: &std::path::Path) -> (Store, SegmentId) {
    let empty = Store::open(directory, OpenOptions::default()).expect("create store");
    empty.close().expect("close empty store");

    let schema = Schema::timestamp_only();
    let mut columns = ColumnStoreBuilder::new(schema.clone());
    columns.push_row(0, &[]).expect("timestamp row");
    let columns = columns.finish().expect("columns");
    let alive = AliveSet::new(1);
    let vector = [1.0_f32, 0.0];
    let codes = vector
        .iter()
        .flat_map(|value| value.to_bits().to_le_bytes())
        .collect::<Vec<_>>();

    let analyzer = Analyzer::new(TokenizerConfig::text_default()).expect("analyzer");
    let mut lexical = SegmentIndex::new();
    lexical
        .push_document(&analyzer, &Document::with_text("bronze zeppelin"))
        .expect("index text");
    let postings = SealedSegment::seal(&lexical)
        .expect("seal text")
        .encode_region()
        .expect("encode postings region");
    let id = SegmentId::new(17, [3; 10]);
    let policy =
        DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None).expect("test durability");
    let meta = write_segment_with_postings(
        &StdVfs,
        directory,
        SegmentBuild {
            id,
            scheme: 0,
            dims: 2,
            codes: &codes,
            factors: SegmentFactors::F32,
            rescore: &vector,
            columns: &columns,
            alive: &alive,
        },
        SegmentPostings { bytes: &postings },
        policy,
    )
    .expect("write identity-free postings segment");
    commit_manifest(
        &StdVfs,
        directory,
        &Manifest {
            generation: 1,
            log_seq: 0,
            segments: vec![meta],
            epochs: Vec::new(),
            epoch_alias: None,
            schema,
        },
        policy,
    )
    .expect("publish identity-free segment");

    let store = Store::open(directory, OpenOptions::default()).expect("open fixture store");
    (store, id)
}

/// The hybrid legs assemble the lexical index without requiring document
/// identity; the standalone lexical path requires it. Both share one
/// assembly cache, so this runs the hybrid query first and then the
/// standalone one over exactly the same snapshot and active segment. A cache
/// entry built without the identity proof must never be handed to the path
/// that demands it.
#[test]
fn a_hybrid_query_does_not_let_the_lexical_path_skip_the_identity_proof() {
    let directory = tempdir().expect("store directory");
    let (store, id) = identity_free_store(directory.path());
    let term = TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]);

    assert_eq!(
        store
            .search_hybrid(
                SearchRequest::new(&[0.0, 0.0]),
                &term,
                &HybridQuery::new(1),
                SearchOptions::default(),
                QueryControl::Cancel(CancelToken::new())
            )
            .expect_err("identity-free hybrid query must fail"),
        FusionError::MissingDocumentIdentity {
            leg: FusionLeg::Vector,
            rank: 0
        }
    );
    assert!(
        matches!(
            store.search_lexical(&term, 1, QueryControl::Cancel(CancelToken::new())),
            Err(StoreLexicalError::MissingDocumentIdentity { segment_id }) if segment_id == id
        ),
        "the standalone lexical path skipped the identity proof after a hybrid query"
    );
    assert!(
        matches!(
            store.search_lexical_structured(
                &LexicalQuery::term(term.clone()),
                1,
                64,
                QueryControl::Cancel(CancelToken::new())
            ),
            Err(StoreLexicalError::MissingDocumentIdentity { segment_id }) if segment_id == id
        ),
        "the structured lexical path skipped the identity proof after a hybrid query"
    );
    store.close().expect("close fixture store");
}

#[test]
fn postings_segment_without_document_identity_is_a_typed_error() {
    let directory = tempdir().expect("store directory");
    let empty = Store::open(directory.path(), OpenOptions::default()).expect("create store");
    empty.close().expect("close empty store");

    let schema = Schema::timestamp_only();
    let mut columns = ColumnStoreBuilder::new(schema.clone());
    columns.push_row(0, &[]).expect("timestamp row");
    let columns = columns.finish().expect("columns");
    let alive = AliveSet::new(1);
    let vector = [1.0_f32, 0.0];
    let codes = vector
        .iter()
        .flat_map(|value| value.to_bits().to_le_bytes())
        .collect::<Vec<_>>();

    let analyzer = Analyzer::new(TokenizerConfig::text_default()).expect("analyzer");
    let mut lexical = SegmentIndex::new();
    lexical
        .push_document(&analyzer, &Document::with_text("bronze zeppelin"))
        .expect("index text");
    let postings = SealedSegment::seal(&lexical)
        .expect("seal text")
        .encode_region()
        .expect("encode postings region");
    let id = SegmentId::new(17, [3; 10]);
    let policy =
        DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None).expect("test durability");
    let meta = write_segment_with_postings(
        &StdVfs,
        directory.path(),
        SegmentBuild {
            id,
            scheme: 0,
            dims: 2,
            codes: &codes,
            factors: SegmentFactors::F32,
            rescore: &vector,
            columns: &columns,
            alive: &alive,
        },
        SegmentPostings { bytes: &postings },
        policy,
    )
    .expect("write identity-free postings segment");
    commit_manifest(
        &StdVfs,
        directory.path(),
        &Manifest {
            generation: 1,
            log_seq: 0,
            segments: vec![meta],
            epochs: Vec::new(),
            epoch_alias: None,
            schema,
        },
        policy,
    )
    .expect("publish identity-free segment");

    let store = Store::open(directory.path(), OpenOptions::default()).expect("open fixture store");
    let term = TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]);
    assert!(matches!(
        store.search_lexical(
            &term,
            1,
            QueryControl::Cancel(CancelToken::new())
        ),
        Err(StoreLexicalError::MissingDocumentIdentity { segment_id }) if segment_id == id
    ));
    assert_eq!(
        store
            .search_hybrid(
                SearchRequest::new(&[0.0, 0.0]),
                &term,
                &HybridQuery::new(1),
                SearchOptions::default(),
                QueryControl::Cancel(CancelToken::new())
            )
            .expect_err("identity-free hybrid query must fail"),
        FusionError::MissingDocumentIdentity {
            leg: FusionLeg::Vector,
            rank: 0
        }
    );
    store.close().expect("close fixture store");
}

#[test]
fn matching_legacy_postings_row_without_stored_text_is_a_typed_snippet_error() {
    let directory = tempdir().expect("store directory");
    let empty = Store::open(directory.path(), OpenOptions::default()).expect("create store");
    empty.close().expect("close empty store");

    let schema = Schema::timestamp_only();
    let mut columns = ColumnStoreBuilder::new(schema.clone());
    columns.push_row(0, &[]).expect("timestamp row");
    let columns = columns.finish().expect("columns");
    let alive = AliveSet::new(1);
    let vector = [1.0_f32, 0.0];
    let codes = vector
        .iter()
        .flat_map(|value| value.to_bits().to_le_bytes())
        .collect::<Vec<_>>();
    let analyzer = Analyzer::new(TokenizerConfig::text_default()).expect("analyzer");
    let mut lexical = SegmentIndex::new();
    lexical
        .push_document(&analyzer, &Document::with_text("bronze zeppelin"))
        .expect("index legacy text");
    let postings = SealedSegment::seal(&lexical)
        .expect("seal legacy text")
        .encode_region()
        .expect("encode legacy postings");
    let id = SegmentId::new(18, [4; 10]);
    let document = [DocId::new(181)];
    let revision = [Revision::new(1)];
    let policy =
        DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::None).expect("test durability");
    let meta = write_segment_with_documents_and_postings(
        &StdVfs,
        directory.path(),
        SegmentBuild {
            id,
            scheme: 0,
            dims: 2,
            codes: &codes,
            factors: SegmentFactors::F32,
            rescore: &vector,
            columns: &columns,
            alive: &alive,
        },
        SegmentDocumentVersions {
            doc_ids: &document,
            revisions: &revision,
        },
        SegmentPostings { bytes: &postings },
        policy,
    )
    .expect("write legacy postings segment");
    commit_manifest(
        &StdVfs,
        directory.path(),
        &Manifest {
            generation: 1,
            log_seq: 0,
            segments: vec![meta],
            epochs: Vec::new(),
            epoch_alias: None,
            schema,
        },
        policy,
    )
    .expect("publish legacy segment");

    let store = Store::open(directory.path(), OpenOptions::default()).expect("open legacy store");
    let term = TermQuery::flat(vec![b"zeppelin".to_vec()], &[DEFAULT_FIELD]);
    assert_eq!(
        store
            .search_lexical(&term, 1, QueryControl::Cancel(CancelToken::new()))
            .expect("term-only compatibility wrapper remains readable")
            .candidates[0]
            .document
            .doc_id(),
        DocId::new(181)
    );
    assert!(matches!(
        store.search_lexical_structured(
            &LexicalQuery::term(TermQuery::flat(
                vec![b"zeppelin".to_vec()],
                &[DEFAULT_FIELD],
            )),
            1,
            32,
            QueryControl::Cancel(CancelToken::new()),
        ),
        Err(StoreLexicalError::MissingStoredText { segment_id, row: 0 }) if segment_id == id
    ));
    store.close().expect("close legacy store");
}
