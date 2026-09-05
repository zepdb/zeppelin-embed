#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use super::*;

#[path = "../tests/common/mod.rs"]
mod fixture;

fn fake_store(
    directory: &Path,
    mut evaluate: impl FnMut(&Store) -> Result<EmbeddingBatch, TextError> + Send + 'static,
) -> TextStore {
    let path = directory.join("fixture.zem");
    fixture::write_symmetric_fixture_bundle(&path);
    let bundle = Arc::new(Bundle::open(&path).expect("bundle"));
    let analyzer = Analyzer::new(TokenizerConfig::text_default()).expect("analyzer");
    let epoch = StoreEpoch {
        embedding: embedding_epoch(&bundle),
        tokenizer: analyzer.epoch(),
    }
    .identity();
    let store_epoch = StoreEpoch {
        embedding: document_epoch(&bundle),
        tokenizer: analyzer.epoch(),
    };
    let document_epoch = store_epoch.identity();
    let store = Arc::new(
        Store::open(
            directory.join("store"),
            OpenOptions::default().with_epoch(store_epoch),
        )
        .expect("core store"),
    );
    store
        .ingest(
            IngestBatch::new(vec![
                IngestDocument::new(
                    DocumentVersion::new(DocId::new(1_u128 << CHUNK_BITS), Revision::new(1)),
                    vec![1.0, 0.0],
                )
                .with_text("bronze zeppelin"),
            ])
            .with_epoch(document_epoch),
        )
        .expect("precomputed fixture document");
    let (sender, receiver) = mpsc::sync_channel(2);
    let closed = Arc::new(AtomicBool::new(false));
    let worker_closed = Arc::clone(&closed);
    let worker_store = Arc::clone(&store);
    let thread = std::thread::spawn(move || {
        run_embedding_commands(receiver, &worker_closed, |_, _, _| evaluate(&worker_store));
    });
    TextStore {
        store,
        bundle,
        analyzer,
        runtime: RuntimeClient {
            query_backend: QueryBackend {
                runtime: crate::runtime::RuntimeIdentity {
                    name: "astra19-fake",
                    gpu: false,
                },
                requested_compute_units: zeppelin_embed::epoch::ComputeUnits::Cpu,
                observed_compute_units: None,
                sequence_length: None,
            },
            sender,
            thread: Mutex::new(Some(thread)),
            closed,
        },
        epoch,
        document_epoch,
        versions: Mutex::new(BTreeMap::new()),
    }
}

#[test]
fn astra_19_text_hybrid_starts_lexical_before_embedding_completion() {
    let directory = tempfile::tempdir().expect("fixture");
    let started_before_completion = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&started_before_completion);
    let store = fake_store(directory.path(), move |core| {
        if core.query_materialization_test_counters().admissions == 1 {
            let watchdog = Instant::now() + Duration::from_secs(5);
            while core.lexical_index_cache_counters().1 == 0 {
                assert!(
                    Instant::now() < watchdog,
                    "watchdog: lexical preparation did not run"
                );
                std::thread::yield_now();
            }
            observed.store(true, Ordering::SeqCst);
        }
        EmbeddingBatch::new(vec![1.0, 0.0], 1, 2).map_err(TextError::Runtime)
    });
    let result = store
        .query_text("bronze", QueryOptions::new(1).with_legs(Legs::Hybrid))
        .expect("real TextStore query through fake embedding owner");
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].text, "bronze zeppelin");
    let started = started_before_completion.load(Ordering::SeqCst);
    store.close().expect("close and join");
    println!("public text lexical preparation started before embedding completed: {started}");
    assert!(started, "TextStore must use the deferred core query path");
}

#[test]
fn astra_19_single_leg_queries_execute_only_requested_leg() {
    let directory = tempfile::tempdir().expect("fixture");
    let calls = Arc::new(AtomicUsize::new(0));
    let evaluated = Arc::clone(&calls);
    let store = fake_store(directory.path(), move |_| {
        evaluated.fetch_add(1, Ordering::SeqCst);
        EmbeddingBatch::new(vec![1.0, 0.0], 1, 2).map_err(TextError::Runtime)
    });
    for legs in [Legs::Dense, Legs::Lexical, Legs::Hybrid] {
        let empty = store
            .query_text_with_diagnostics("bronze", QueryOptions::new(0).with_legs(legs))
            .expect("empty query");
        assert!(empty.hits.is_empty());
        assert_eq!(empty.embedding_calls, 0);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        store.store.query_materialization_test_counters().admissions,
        0
    );
    let dense = store
        .query_text_with_diagnostics("bronze", QueryOptions::new(1).with_legs(Legs::Dense))
        .expect("dense");
    assert_eq!(dense.hits.len(), 1);
    assert_eq!(dense.embedding_calls, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(store.store.lexical_index_cache_counters(), (0, 0));
    let lexical = store
        .query_text_with_diagnostics("bronze", QueryOptions::new(1).with_legs(Legs::Lexical))
        .expect("lexical");
    assert_eq!(lexical.hits.len(), 1);
    assert_eq!(lexical.embedding_calls, 0);
    assert_eq!(lexical.query_tokens, 0);
    assert!(lexical.backend.is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    store.close().expect("close");
}

#[test]
fn astra_19_text_embedding_error_keeps_original_type() {
    let directory = tempfile::tempdir().expect("fixture");
    let store = fake_store(directory.path(), |_| {
        Err(TextError::InvalidInput("fake embedding error"))
    });
    let result = store.query_text("bronze", QueryOptions::new(1).with_legs(Legs::Hybrid));
    assert!(matches!(
        result,
        Err(TextError::InvalidInput("fake embedding error"))
    ));
    assert_eq!(
        store.store.query_materialization_test_counters().admissions,
        1
    );
    store
        .close()
        .expect("failed query releases admission and joins workers");
}
