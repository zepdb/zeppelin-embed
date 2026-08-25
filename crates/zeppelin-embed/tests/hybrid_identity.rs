#![allow(clippy::expect_used)]

use tempfile::tempdir;
use zeppelin_embed::fts::index::{DEFAULT_FIELD, Document, SegmentIndex};
use zeppelin_embed::fts::sealed::SealedSegment;
use zeppelin_embed::fts::search::TermQuery;
use zeppelin_embed::fts::tokenizer::{Analyzer, TokenizerConfig};
use zeppelin_embed::fusion::{FusionError, FusionLeg, HybridQuery};
use zeppelin_embed::ingest::{SearchRequest, StoreLexicalError};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, SearchOptions, Store};
use zeppelin_embed::manifest::Manifest;
use zeppelin_embed::manifest::io::commit_manifest;
use zeppelin_embed::meta::{AliveSet, ColumnStoreBuilder, Schema};
use zeppelin_embed::segment::SegmentId;
use zeppelin_embed::segment::writer::{
    SegmentBuild, SegmentFactors, SegmentPostings, write_segment_with_postings,
};
use zeppelin_embed::vfs::StdVfs;

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
