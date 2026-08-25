#![allow(clippy::expect_used, clippy::indexing_slicing)]

use std::path::Path;

use tempfile::tempdir;
use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, EpochIdentity, EpochMismatch,
    Normalization, StoreEpoch,
};
use zeppelin_embed::fts::tokenizer::TokenizerConfig;
use zeppelin_embed::ingest::{
    DocId, DocumentVersion, IngestBatch, IngestDocument, IngestError, Revision, SearchOutcome,
    SearchRequest,
};
use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store, StoreError};
use zeppelin_embed::manifest::io::commit_manifest;
use zeppelin_embed::manifest::{EpochMeta, Manifest};
use zeppelin_embed::meta::Schema;
use zeppelin_embed::scan::ScanOptions;
use zeppelin_embed::vfs::StdVfs;

#[test]
fn an_ingest_with_a_matching_dimension_but_different_declared_epoch_is_a_typed_epoch_mismatch() {
    let directory = tempdir().expect("store directory");
    let declared = store_epoch("model-a", "1", TokenizerConfig::text_default());
    let conflicting = store_epoch("model-b", "1", TokenizerConfig::text_default());
    assert_eq!(
        declared.embedding.document.dims,
        conflicting.embedding.document.dims
    );
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_epoch(declared.clone()),
    )
    .expect("open epoch store");

    let error = store
        .ingest(batch(1).with_epoch(conflicting.identity()))
        .expect_err("conflicting epoch must fail");

    assert!(matches!(error, IngestError::EpochMismatch(_)));
    assert!(!matches!(error, IngestError::Vector(_)));
}

#[test]
fn opening_a_store_with_a_conflicting_declared_epoch_is_a_typed_error_before_any_write() {
    let directory = tempdir().expect("store directory");
    let expected = store_epoch("model-a", "1", TokenizerConfig::text_default());
    let declared = store_epoch("model-b", "1", TokenizerConfig::text_default());
    write_epoch_manifest(directory.path(), &expected);
    std::fs::write(directory.path().join("writer.lock"), []).expect("seed writer lock path");
    let before = directory_bytes(directory.path());
    assert!(!directory.path().join("wal.ze").exists());

    let error = Store::open(directory.path(), OpenOptions::new().with_epoch(declared))
        .err()
        .expect("conflicting open must fail");

    assert!(matches!(error, StoreError::EpochMismatch(_)));
    assert_eq!(directory_bytes(directory.path()), before);
    assert!(!directory.path().join("wal.ze").exists());
}

#[test]
fn opening_an_epoch_stamped_store_without_declaring_an_epoch_is_a_typed_error() {
    let directory = tempdir().expect("store directory");
    let declared = store_epoch("model-a", "1", TokenizerConfig::text_default());
    let store = stamped_store(directory.path(), &declared);
    store.close().expect("close stamped store");

    let error = Store::open(directory.path(), OpenOptions::new())
        .err()
        .expect("undeclared open must fail");

    assert!(matches!(error, StoreError::EpochUndeclared));
}

#[test]
fn opening_a_pre_epoch_store_with_a_declared_epoch_is_a_typed_error() {
    let directory = tempdir().expect("store directory");
    let legacy = Store::open(directory.path(), OpenOptions::new()).expect("open legacy store");
    legacy.ingest(batch(1)).expect("legacy ingest");
    legacy.seal().expect("legacy seal");
    legacy.close().expect("close legacy store");
    let declared = store_epoch("model-a", "1", TokenizerConfig::text_default());

    let error = Store::open(directory.path(), OpenOptions::new().with_epoch(declared))
        .err()
        .expect("pre-epoch open must fail");

    assert!(matches!(error, StoreError::EpochUnstamped));
}

#[test]
fn an_ingest_without_a_declared_epoch_into_an_epoch_stamped_store_is_a_typed_error() {
    let directory = tempdir().expect("store directory");
    let declared = store_epoch("model-a", "1", TokenizerConfig::text_default());
    let store = stamped_store(directory.path(), &declared);

    let error = store
        .ingest(batch(2))
        .expect_err("undeclared ingest must fail");

    assert!(matches!(error, IngestError::EpochUndeclared));
}

#[test]
fn an_ingest_declaring_an_epoch_into_an_unstamped_store_is_a_typed_error() {
    let directory = tempdir().expect("store directory");
    let declared = store_epoch("model-a", "1", TokenizerConfig::text_default());
    let store = Store::open(directory.path(), OpenOptions::new()).expect("open legacy store");

    let error = store
        .ingest(batch(1).with_epoch(declared.identity()))
        .expect_err("declared ingest into unstamped store must fail");

    assert!(matches!(error, IngestError::EpochUnstamped));
}

#[test]
fn a_rejected_epoch_mismatch_leaves_the_generation_and_every_search_result_unchanged() {
    let directory = tempdir().expect("store directory");
    let declared = store_epoch("model-a", "1", TokenizerConfig::text_default());
    let conflicting = store_epoch("model-b", "1", TokenizerConfig::text_default());
    let store = stamped_store(directory.path(), &declared);
    let before = search(&store, 4);

    let error = store
        .ingest(batch(2).with_epoch(conflicting.identity()))
        .expect_err("conflicting ingest must fail");
    let after = search(&store, 4);

    assert!(matches!(error, IngestError::EpochMismatch(_)));
    assert_eq!(after.generation, before.generation);
    assert_eq!(after, before);
}

#[test]
fn a_tokenizer_epoch_change_alone_is_a_typed_epoch_mismatch() {
    let directory = tempdir().expect("store directory");
    let declared = store_epoch("model-a", "1", TokenizerConfig::text_default());
    let conflicting = StoreEpoch {
        embedding: declared.embedding.clone(),
        tokenizer: TokenizerConfig::code().epoch(),
    };
    assert_eq!(
        declared.identity().embedding,
        conflicting.identity().embedding
    );
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_epoch(declared.clone()),
    )
    .expect("open epoch store");

    let error = store
        .ingest(batch(1).with_epoch(conflicting.identity()))
        .expect_err("tokenizer conflict must fail");

    let IngestError::EpochMismatch(EpochMismatch {
        expected,
        declared: actual,
    }) = error
    else {
        panic!("wrong ingest error")
    };
    assert_eq!(expected.embedding, actual.embedding);
    assert_ne!(expected.tokenizer, actual.tokenizer);
}

#[test]
fn reopening_a_store_declaring_a_different_tokenizer_epoch_alone_is_a_typed_error() {
    let directory = tempdir().expect("store directory");
    let declared = store_epoch("model-a", "1", TokenizerConfig::text_default());
    let conflicting = StoreEpoch {
        embedding: declared.embedding.clone(),
        tokenizer: TokenizerConfig::code().epoch(),
    };
    assert_eq!(
        declared.identity().embedding,
        conflicting.identity().embedding,
        "only the tokenizer half may differ"
    );

    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_epoch(declared.clone()),
    )
    .expect("open epoch store");
    store
        .ingest(batch(1).with_epoch(declared.identity()))
        .expect("ingest declared epoch");
    store.close().expect("close store");

    let error = Store::open(directory.path(), OpenOptions::new().with_epoch(conflicting))
        .err()
        .expect("reopen under a conflicting tokenizer must fail");

    let StoreError::EpochMismatch(EpochMismatch {
        expected,
        declared: actual,
    }) = error
    else {
        panic!("wrong open error")
    };
    assert_eq!(expected.embedding, actual.embedding);
    assert_ne!(expected.tokenizer, actual.tokenizer);
}

#[test]
fn an_embedding_epoch_change_alone_is_a_typed_epoch_mismatch() {
    let directory = tempdir().expect("store directory");
    let declared = store_epoch("model-a", "1", TokenizerConfig::text_default());
    let conflicting = store_epoch("model-b", "1", TokenizerConfig::text_default());
    assert_eq!(declared.tokenizer, conflicting.tokenizer);
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_epoch(declared.clone()),
    )
    .expect("open epoch store");

    let error = store
        .ingest(batch(1).with_epoch(conflicting.identity()))
        .expect_err("embedding conflict must fail");

    let IngestError::EpochMismatch(EpochMismatch {
        expected,
        declared: actual,
    }) = error
    else {
        panic!("wrong ingest error")
    };
    assert_ne!(expected.embedding, actual.embedding);
    assert_eq!(expected.tokenizer, actual.tokenizer);
}

#[test]
fn every_search_outcome_from_an_epoch_stamped_store_names_that_store_epoch() {
    let directory = tempdir().expect("store directory");
    let declared = store_epoch("model-a", "1", TokenizerConfig::text_default());
    let store = stamped_store(directory.path(), &declared);

    for outcome in [search(&store, 1), search(&store, 4)] {
        assert_eq!(outcome.epoch, Some(declared.identity()));
        assert_eq!(
            outcome.diagnostics.embedding_epoch,
            Some(declared.identity().embedding)
        );
        assert_eq!(outcome.diagnostics.tokenizer_epoch, None);
    }
}

#[test]
fn the_declared_epoch_survives_ingest_seal_close_and_reopen() {
    let directory = tempdir().expect("store directory");
    let declared = store_epoch("model-a", "1", TokenizerConfig::text_default());
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_epoch(declared.clone()),
    )
    .expect("open epoch store");
    store
        .ingest(batch(1).with_epoch(declared.identity()))
        .expect("ingest declared epoch");
    store.seal().expect("seal declared epoch");
    store.close().expect("close declared epoch");

    let reopened = Store::open(
        directory.path(),
        OpenOptions::new().with_epoch(declared.clone()),
    )
    .expect("reopen matching epoch");
    assert_eq!(search(&reopened, 1).epoch, Some(declared.identity()));
    reopened.close().expect("close matching reopen");

    let conflicting = store_epoch("model-a", "2", TokenizerConfig::text_default());
    let error = Store::open(directory.path(), OpenOptions::new().with_epoch(conflicting))
        .err()
        .expect("reopen conflict must fail");
    assert!(matches!(error, StoreError::EpochMismatch(_)));
}

#[test]
fn a_store_created_under_one_epoch_cannot_be_reopened_under_another_without_ever_sealing() {
    let directory = tempdir().expect("store directory");
    let declared = store_epoch("model-a", "1", TokenizerConfig::text_default());
    let conflicting = store_epoch("model-b", "1", TokenizerConfig::text_default());
    let store = Store::open(
        directory.path(),
        OpenOptions::new().with_epoch(declared.clone()),
    )
    .expect("open epoch store");
    store
        .ingest(batch(1).with_epoch(declared.identity()))
        .expect("ingest declared epoch");
    store.close().expect("close without sealing");

    let error = Store::open(directory.path(), OpenOptions::new().with_epoch(conflicting))
        .err()
        .expect("reopen under a conflicting epoch must fail");
    assert!(matches!(error, StoreError::EpochMismatch(_)));

    let reopened = Store::open(
        directory.path(),
        OpenOptions::new().with_epoch(declared.clone()),
    )
    .expect("reopen matching epoch");
    let outcome = search(&reopened, 1);
    assert_eq!(outcome.epoch, Some(declared.identity()));
    assert_eq!(outcome.candidates.len(), 1);
    assert_eq!(
        outcome.candidates[0].document(),
        Some(DocumentVersion::new(DocId::new(1), Revision::new(1)))
    );
}

#[test]
fn a_read_only_open_declaring_an_epoch_against_an_unstamped_store_is_a_typed_error() {
    let directory = tempdir().expect("store directory");
    let declared = store_epoch("model-a", "1", TokenizerConfig::text_default());
    let before = directory_bytes(directory.path());

    let error = Store::open(
        directory.path(),
        OpenOptions::read_only().with_epoch(declared),
    )
    .err()
    .expect("read-only open cannot stamp an epoch");

    assert!(matches!(error, StoreError::EpochUnstamped));
    assert_eq!(directory_bytes(directory.path()), before);
}

#[test]
fn a_manifest_registry_id_must_match_the_complete_embedding_identity() {
    let declared = store_epoch("model-a", "1", TokenizerConfig::text_default());
    let mut meta = EpochMeta::from(&declared);
    meta.embedding.query.model_version = "different-query-tower".to_owned();

    let error = zeppelin_embed::manifest::encode_manifest(&Manifest {
        generation: 0,
        log_seq: 0,
        segments: Vec::new(),
        epochs: vec![meta],
        epoch_alias: Some(declared.identity()),
        schema: Schema::new(Vec::new()).expect("empty schema"),
    })
    .expect_err("registry id must authenticate the complete embedding epoch");

    assert!(matches!(
        error,
        zeppelin_embed::manifest::ManifestError::Decode(_)
    ));
}

fn embedding_epoch(model_id: &str, model_version: &str) -> EmbeddingEpoch {
    let document = EmbeddingTower {
        model_id: model_id.to_owned(),
        model_version: model_version.to_owned(),
        weights_digest: vec![0x01, 0x23, 0x45, 0x67],
        dims: 4,
        normalization: Normalization::L2,
        prompt_prefix: "search_document: ".to_owned(),
        max_tokens: 512,
        runtime: EmbeddingRuntime::CoreMl,
        compute_units: ComputeUnits::CpuAndNeuralEngine,
        os_build: Some("25A100".to_owned()),
    };
    let mut query = document.clone();
    query.prompt_prefix = "search_query: ".to_owned();
    EmbeddingEpoch {
        document,
        query,
        alignment_digest: Vec::new(),
    }
}

fn store_epoch(model_id: &str, model_version: &str, tokenizer: TokenizerConfig) -> StoreEpoch {
    StoreEpoch {
        embedding: embedding_epoch(model_id, model_version),
        tokenizer: tokenizer.epoch(),
    }
}

fn batch(doc_id: u128) -> IngestBatch {
    IngestBatch::new(vec![IngestDocument::new(
        DocumentVersion::new(DocId::new(doc_id), Revision::new(1)),
        vec![1.0, 0.0, 0.0, 0.0],
    )])
}

fn stamped_store(directory: &Path, declared: &StoreEpoch) -> Store {
    let store = Store::open(directory, OpenOptions::new().with_epoch(declared.clone()))
        .expect("open epoch store");
    store
        .ingest(batch(1).with_epoch(declared.identity()))
        .expect("ingest epoch row");
    store.seal().expect("stamp epoch manifest");
    store
}

fn search(store: &Store, k: usize) -> SearchOutcome {
    store
        .search(
            SearchRequest::new(&[1.0, 0.0, 0.0, 0.0]),
            k,
            ScanOptions { thread_budget: 1 },
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("search store")
}

fn write_epoch_manifest(directory: &Path, epoch: &StoreEpoch) {
    let policy = DurabilityPolicy::new(DurabilityMode::Derived, CommitTier::Ordered)
        .expect("derived policy");
    commit_manifest(
        &StdVfs,
        directory,
        &Manifest {
            generation: 0,
            log_seq: 0,
            segments: Vec::new(),
            epochs: vec![EpochMeta::from(epoch.clone())],
            epoch_alias: Some(epoch.identity()),
            schema: Schema::new(Vec::new()).expect("empty schema"),
        },
        policy,
    )
    .expect("write epoch manifest");
}

fn directory_bytes(directory: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files = std::fs::read_dir(directory)
        .expect("read store directory")
        .map(|entry| {
            let path = entry.expect("store entry").path();
            let name = path
                .file_name()
                .expect("store entry name")
                .to_string_lossy()
                .into_owned();
            let bytes = std::fs::read(path).expect("read store entry");
            (name, bytes)
        })
        .collect::<Vec<_>>();
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}

fn _identity_is_copy(identity: EpochIdentity) -> (EpochIdentity, EpochIdentity) {
    (identity, identity)
}
