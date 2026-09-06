mod common;

use std::mem::size_of;

use zeppelin_embed_ffi::*;

const DIMENSION: usize = 4;

fn vector(seed: usize) -> Vec<f32> {
    (0..DIMENSION)
        .map(|index| ((index + seed) as f32 + 1.0) / 8.0)
        .collect()
}

/// Ingests documents with text through the C surface (BL-162).
fn ingest_text(handle: ZeHandle, texts: &[&str]) -> ZeErrorCode {
    let vectors = (0..texts.len()).map(vector).collect::<Vec<_>>();
    let documents = texts
        .iter()
        .zip(&vectors)
        .enumerate()
        .map(|(index, (text, vector))| ZeIngestDocument {
            abi_size: size_of::<ZeIngestDocument>() as u32,
            abi_reserved: 0,
            doc_id: ZeDocId {
                high: 0,
                low: index as u64 + 1,
            },
            revision: 1,
            timestamp: index as i64,
            vector: vector.as_ptr(),
            vector_len: vector.len(),
            metadata: std::ptr::null(),
            metadata_len: 0,
            text: text.as_ptr(),
            text_len: text.len(),
        })
        .collect::<Vec<_>>();
    let request = ZeIngestRequest {
        abi_size: size_of::<ZeIngestRequest>() as u32,
        abi_reserved: 0,
        documents: documents.as_ptr(),
        document_count: documents.len(),
        dimension: DIMENSION,
    };
    let mut report: ZeMutationReport = common::sized_zeroed();
    ze_ingest(handle, &request, &mut report)
}

fn query(handle: ZeHandle, request: &ZeQueryRequest) -> (ZeErrorCode, ZeQueryResult) {
    let mut result: ZeQueryResult = common::sized_zeroed();
    let code = ze_query(handle, request, &mut result);
    (code, result)
}

#[test]
fn a_vector_only_query_returns_the_same_hits_as_ze_search_with_no_tier_preference() {
    let store = common::TestStore::new();
    assert_eq!(
        common::ingest_rows(store.handle, 3, DIMENSION),
        ZeErrorCode::ZeOk
    );
    let probe = vector(0);
    let mut request = common::valid_query_request(&probe);
    request.k = 3;
    let (code, mut result) = query(store.handle, &request);
    assert_eq!(code, ZeErrorCode::ZeOk);
    assert_eq!(result.mode, 0);
    assert_eq!(result.hit_count, 3);
    assert_eq!(result.approximate, 0);
    assert_eq!(result.has_fusion, 0);
    let hits = unsafe { std::slice::from_raw_parts(result.hits, result.hit_count) };
    let mut search_request = common::valid_search_request(&probe);
    search_request.k = 3;
    let mut search: ZeSearchResult = common::sized_zeroed();
    assert_eq!(
        ze_search(store.handle, &search_request, &mut search),
        ZeErrorCode::ZeOk
    );
    let search_hits = unsafe { std::slice::from_raw_parts(search.hits, search.hit_count) };
    for (hit, expected) in hits.iter().zip(search_hits) {
        assert_eq!(hit.has_document, 1);
        assert_eq!(hit.has_revision, 1);
        assert_eq!(hit.doc_id, expected.doc_id);
        assert_eq!(hit.revision, expected.revision);
        assert_eq!(hit.score, f64::from(expected.score));
        assert_eq!(hit.has_vector_score, 1);
        assert_eq!(hit.has_lexical_score, 0);
        assert_eq!(hit.vector_squared_l2, -f64::from(expected.score));
    }
    assert_eq!(result.generation, search.generation);
    assert_eq!(result.dims_touched, search.dims_touched);
    assert_eq!(ze_search_result_free(&mut search), ZeErrorCode::ZeOk);
    assert_eq!(ze_query_result_free(&mut result), ZeErrorCode::ZeOk);
    assert!(result.hits.is_null());
    assert_eq!(ze_query_result_free(&mut result), ZeErrorCode::ZeOk);
    let mut zeroed: ZeQueryResult = unsafe { std::mem::zeroed() };
    assert_eq!(ze_query_result_free(&mut zeroed), ZeErrorCode::ZeOk);
}

#[test]
fn normal_store_search_result_and_deterministic_counters_are_unchanged() {
    let store = common::TestStore::new();
    assert_eq!(
        common::ingest_rows(store.handle, 3, DIMENSION),
        ZeErrorCode::ZeOk
    );
    let probe = vector(0);
    let mut request = common::valid_search_request(&probe);
    request.k = 3;
    let mut result: ZeSearchResult = common::sized_zeroed();
    assert_eq!(
        ze_search(store.handle, &request, &mut result),
        ZeErrorCode::ZeOk
    );
    let hits = unsafe { std::slice::from_raw_parts(result.hits, result.hit_count) }
        .iter()
        .map(|hit| (hit.doc_id.low, hit.score.to_bits()))
        .collect::<Vec<_>>();
    assert_eq!(
        hits,
        vec![(1, 1_064_347_424), (2, 1_064_347_424), (3, 1_064_347_424)]
    );
    assert_eq!(result.hit_count, 3);
    assert_eq!(result.generation, 1);
    assert_eq!(result.dims_touched, 12);
    assert_eq!(result.bytes_read, 6);
    assert_eq!(result.threads_used, 1);
    assert_eq!(result.graph_segments_traversed, 0);
    assert_eq!(result.graph_validations, 0);
    assert_eq!(result.graph_entry_seed_discoveries, 0);
    assert_eq!(result.graph_visited_epoch_clears, 0);
    assert_eq!(result.graph_candidates_scored, 0);
    assert_eq!(result.graph_candidates_rescored, 0);
    assert_eq!(result.graph_segments_pruned_by_bound, 0);
    assert_eq!(ze_search_result_free(&mut result), ZeErrorCode::ZeOk);
}

#[test]
fn lexical_and_hybrid_legs_execute_through_the_structured_query_surface() {
    let store = common::TestStore::new();
    let handle = store.handle;
    assert_eq!(
        ingest_text(
            handle,
            &[
                "zeppelin airship over the harbour",
                "harbour lights at dusk",
                "quantized vectors and postings",
            ],
        ),
        ZeErrorCode::ZeOk
    );
    let invalid = [0xff_u8];
    let mut bad = ZeIngestDocument {
        abi_size: size_of::<ZeIngestDocument>() as u32,
        abi_reserved: 0,
        doc_id: ZeDocId { high: 0, low: 9 },
        revision: 1,
        timestamp: 9,
        vector: std::ptr::null(),
        vector_len: 0,
        metadata: std::ptr::null(),
        metadata_len: 0,
        text: invalid.as_ptr(),
        text_len: invalid.len(),
    };
    let probe = vector(9);
    bad.vector = probe.as_ptr();
    bad.vector_len = probe.len();
    let request = ZeIngestRequest {
        abi_size: size_of::<ZeIngestRequest>() as u32,
        abi_reserved: 0,
        documents: &bad,
        document_count: 1,
        dimension: DIMENSION,
    };
    let mut report: ZeMutationReport = common::sized_zeroed();
    assert_eq!(
        ze_ingest(handle, &request, &mut report),
        ZeErrorCode::ZeErrInvalidArgument,
        "invalid UTF-8 text is a typed error"
    );

    let text = b"harbour";
    let mut request = common::valid_query_request(&[]);
    request.text = text.as_ptr();
    request.text_len = text.len();
    request.k = 5;
    let (code, mut lexical) = query(handle, &request);
    assert_eq!(code, ZeErrorCode::ZeOk);
    assert_eq!(lexical.mode, 1);
    assert_eq!(lexical.hit_count, 2);
    assert!(lexical.docs_evaluated >= 2);
    let hits = unsafe { std::slice::from_raw_parts(lexical.hits, lexical.hit_count) };
    for hit in hits {
        assert_eq!(hit.has_document, 1);
        assert_eq!(hit.has_revision, 1);
        assert_eq!(hit.revision, 1);
        assert_eq!(hit.has_lexical_score, 1);
        assert_eq!(hit.has_vector_score, 0);
        assert_eq!(hit.score, hit.lexical_bm25);
        assert!(hit.lexical_bm25 > 0.0);
        assert!(matches!(hit.doc_id.low, 1 | 2));
    }
    assert_eq!(ze_query_result_free(&mut lexical), ZeErrorCode::ZeOk);

    let probe = vector(1);
    let mut request = common::valid_query_request(&probe);
    request.text = text.as_ptr();
    request.text_len = text.len();
    request.k = 3;
    request.rules_enabled = 1;
    let (code, mut hybrid) = query(handle, &request);
    assert_eq!(code, ZeErrorCode::ZeOk);
    assert_eq!(hybrid.mode, 2);
    assert_eq!(hybrid.has_fusion, 1);
    assert_eq!(
        hybrid.exact_rescore, 1,
        "no preference selects exact scoring"
    );
    assert_eq!(hybrid.hit_count, 3);
    assert_eq!(
        hybrid.has_embedding_epoch, 0,
        "store carries no stamped epoch"
    );
    let hits = unsafe { std::slice::from_raw_parts(hybrid.hits, hybrid.hit_count) };
    assert!(hits.windows(2).all(|pair| pair[0].score >= pair[1].score));
    assert!(
        hits.iter()
            .all(|hit| hit.has_document == 1 && hit.has_revision == 0)
    );
    assert!(hits.iter().any(|hit| hit.has_lexical_score == 1));
    assert!(hits.iter().all(|hit| hit.has_vector_score == 1));
    assert_eq!(ze_query_result_free(&mut hybrid), ZeErrorCode::ZeOk);

    // Rules are opt-in (policy version 2): the same identifier signal moves
    // the effective alpha only when rules_enabled is one.
    let mut signalled = request;
    signalled.rules_enabled = 0;
    signalled.identifier_token = 1;
    let (code, mut result) = query(handle, &signalled);
    assert_eq!(code, ZeErrorCode::ZeOk);
    assert_eq!(result.has_fusion, 1);
    let default_alpha = result.effective_alpha;
    assert_eq!(ze_query_result_free(&mut result), ZeErrorCode::ZeOk);
    signalled.rules_enabled = 1;
    let (code, mut result) = query(handle, &signalled);
    assert_eq!(code, ZeErrorCode::ZeOk);
    assert_ne!(result.effective_alpha, default_alpha);
    assert_eq!(ze_query_result_free(&mut result), ZeErrorCode::ZeOk);

    let mut explicit = request;
    explicit.has_alpha = 1;
    explicit.alpha = 0.25;
    explicit.rules_enabled = 0;
    let (code, mut result) = query(handle, &explicit);
    assert_eq!(code, ZeErrorCode::ZeOk);
    assert!(
        result.has_fusion == 1 && (result.fusion_method == 1 || result.effective_alpha == 0.25)
    );
    assert_eq!(ze_query_result_free(&mut result), ZeErrorCode::ZeOk);

    explicit.alpha = 1.5;
    let (code, result) = query(handle, &explicit);
    assert_eq!(code, ZeErrorCode::ZeErrInvalidArgument);
    assert_eq!(result.hit_count, 0);
}

#[test]
fn an_unstamped_store_reports_no_current_epoch_as_a_typed_error() {
    let store = common::TestStore::new();
    let mut current: ZeEpochIdentity = common::sized_zeroed();
    assert_eq!(
        ze_epoch_current(store.handle, &mut current),
        ZeErrorCode::ZeErrEpochUnstamped
    );
    assert_eq!(
        ze_epoch_current(store.handle, std::ptr::null_mut()),
        ZeErrorCode::ZeErrInvalidArgument
    );
}

#[test]
fn no_tier_preference_is_encoded_distinctly_from_an_explicit_tier() {
    let store = common::TestStore::new();
    assert_eq!(
        common::ingest_rows(store.handle, 2, DIMENSION),
        ZeErrorCode::ZeOk
    );
    let probe = vector(0);
    let base = common::valid_query_request(&probe);

    // has_tier = 0 with a nonzero tier is a contradiction, never a silent Auto.
    let mut contradiction = base;
    contradiction.tier = 2;
    assert_eq!(
        query(store.handle, &contradiction).0,
        ZeErrorCode::ZeErrInvalidArgument
    );

    // Every explicit tier discriminant is accepted, including explicit Auto (0)
    // and Exact (1), which the older ze_search enum cannot express.
    for tier in 0..=2 {
        let mut explicit = base;
        explicit.has_tier = 1;
        explicit.tier = tier;
        let (code, mut result) = query(store.handle, &explicit);
        assert_eq!(code, ZeErrorCode::ZeOk, "explicit tier {tier}");
        assert_eq!(result.hit_count, 1);
        assert_eq!(ze_query_result_free(&mut result), ZeErrorCode::ZeOk);
    }
    let mut graph = base;
    graph.has_tier = 1;
    graph.tier = 3;
    let (code, mut result) = query(store.handle, &graph);
    assert_eq!(
        code,
        ZeErrorCode::ZeOk,
        "explicit graph over an active-only store"
    );
    assert_eq!(ze_query_result_free(&mut result), ZeErrorCode::ZeOk);

    let mut bad_flag = base;
    bad_flag.has_tier = 2;
    assert_eq!(
        query(store.handle, &bad_flag).0,
        ZeErrorCode::ZeErrInvalidArgument
    );

    // A tier needs a vector leg, and fusion parameters need both legs.
    let text = b"anything";
    let mut lexical_with_tier = common::valid_query_request(&[]);
    lexical_with_tier.text = text.as_ptr();
    lexical_with_tier.text_len = text.len();
    lexical_with_tier.has_tier = 1;
    assert_eq!(
        query(store.handle, &lexical_with_tier).0,
        ZeErrorCode::ZeErrInvalidArgument
    );
    let mut vector_with_alpha = base;
    vector_with_alpha.has_alpha = 1;
    vector_with_alpha.alpha = 0.5;
    assert_eq!(
        query(store.handle, &vector_with_alpha).0,
        ZeErrorCode::ZeErrInvalidArgument
    );
    let empty = common::valid_query_request(&[]);
    assert_eq!(
        query(store.handle, &empty).0,
        ZeErrorCode::ZeErrInvalidArgument
    );
}

#[test]
fn epoch_identity_open_with_epoch_and_transitions_are_typed_through_the_boundary() {
    let fixture = common::EpochFixture::new(DIMENSION as u32);
    let request = fixture.request();
    let mut identity: ZeEpochIdentity = common::sized_zeroed();
    assert_eq!(
        ze_epoch_identity(&request, &mut identity),
        ZeErrorCode::ZeOk
    );
    let expected = fixture.core().identity();
    assert_eq!(identity.embedding_epoch, expected.embedding.value());
    assert_eq!(identity.tokenizer_epoch, expected.tokenizer.value());
    assert_eq!(
        ze_epoch_identity(std::ptr::null(), &mut identity),
        ZeErrorCode::ZeErrInvalidArgument
    );

    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("store");
    let (code, handle) = common::open_path_with_epoch(&path, &fixture);
    assert_eq!(code, ZeErrorCode::ZeOk);
    assert_eq!(common::ingest_rows(handle, 2, DIMENSION), ZeErrorCode::ZeOk);

    let mut current: ZeEpochIdentity = common::sized_zeroed();
    assert_eq!(ze_epoch_current(handle, &mut current), ZeErrorCode::ZeOk);
    assert_eq!(current.embedding_epoch, identity.embedding_epoch);
    assert_eq!(current.tokenizer_epoch, identity.tokenizer_epoch);

    let probe = vector(0);
    let (code, mut result) = query(handle, &common::valid_query_request(&probe));
    assert_eq!(code, ZeErrorCode::ZeOk);
    assert_eq!(result.has_embedding_epoch, 1);
    assert_eq!(result.embedding_epoch, identity.embedding_epoch);
    assert_eq!(ze_query_result_free(&mut result), ZeErrorCode::ZeOk);

    // Transitions before the active segment is sealed are refused, typed.
    let mut other = common::EpochFixture::new(DIMENSION as u32);
    other.model_version = b"2".to_vec();
    let other_request = other.request();
    let mut alias: ZeEpochAliasReport = common::sized_zeroed();
    let unsealed = ze_epoch_switch_alias(handle, &other_request, &mut alias);
    assert!(
        matches!(
            unsealed,
            ZeErrorCode::ZeErrUnsealedWrites | ZeErrorCode::ZeErrNotFound
        ),
        "{unsealed:?}"
    );
    let seal = ZeSealRequest {
        abi_size: size_of::<ZeSealRequest>() as u32,
        abi_reserved: 0,
        cancel_token: 0,
    };
    let mut generation: ZeGenerationReport = common::sized_zeroed();
    assert_eq!(ze_seal(handle, &seal, &mut generation), ZeErrorCode::ZeOk);

    // An unregistered alias target is not found; the published epoch cannot
    // be dropped; the declared epoch can be re-published as a no-op.
    assert_eq!(
        ze_epoch_switch_alias(handle, &other_request, &mut alias),
        ZeErrorCode::ZeErrNotFound
    );
    let mut drop: ZeEpochDropReport = common::sized_zeroed();
    assert_eq!(
        ze_epoch_drop(handle, &request, &mut drop),
        ZeErrorCode::ZeErrEpochPublished
    );
    assert_eq!(
        ze_epoch_drop(handle, &other_request, &mut drop),
        ZeErrorCode::ZeErrNotFound
    );
    assert_eq!(
        ze_epoch_switch_alias(handle, &request, &mut alias),
        ZeErrorCode::ZeOk
    );
    assert_eq!(alias.previous_embedding_epoch, identity.embedding_epoch);
    assert_eq!(alias.published_embedding_epoch, identity.embedding_epoch);
    assert_eq!(alias.published_tokenizer_epoch, identity.tokenizer_epoch);
    assert_eq!(alias.manifest_committed, 0);
    let mut message = vec![0_u8; 256];
    let mut written = 0;
    assert_eq!(
        ze_last_error_message(
            handle,
            message.as_mut_ptr().cast(),
            message.len(),
            &mut written
        ),
        ZeErrorCode::ZeOk
    );
    assert!(written > 0, "typed epoch errors carry a message");
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);

    // Reopening with a conflicting declaration is a typed mismatch, and a
    // plain reopen of a stamped store is a typed undeclared-epoch error.
    let (code, stale) = common::open_path_with_epoch(&path, &other);
    assert_eq!(code, ZeErrorCode::ZeErrEpochMismatch);
    assert_eq!(stale, 0);
    let (code, undeclared) = common::open_path(&path);
    assert_eq!(code, ZeErrorCode::ZeErrEpochUndeclared);
    assert_eq!(undeclared, 0);
    let (code, handle) = common::open_path_with_epoch(&path, &fixture);
    assert_eq!(code, ZeErrorCode::ZeOk);
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}
