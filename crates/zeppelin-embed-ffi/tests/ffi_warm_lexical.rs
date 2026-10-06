mod common;
use common::sized_zeroed;
use zeppelin_embed_ffi::*;

fn hits(handle: ZeHandle, prefix: bool) -> Vec<(u64, u64, f64)> {
    let text = if prefix {
        b"common pai".as_slice()
    } else {
        b"common pair".as_slice()
    };
    let mut request: ZeQueryRequest = sized_zeroed();
    request.text = text.as_ptr();
    request.text_len = text.len();
    request.k = 10;
    request.lexical_flags = u32::from(prefix);
    let mut result: ZeQueryResult = sized_zeroed();
    assert_eq!(ze_query(handle, &request, &mut result), ZeErrorCode::ZeOk);
    let hits = unsafe { std::slice::from_raw_parts(result.hits, result.hit_count) }
        .iter()
        .map(|hit| (hit.doc_id.high, hit.doc_id.low, hit.score))
        .collect();
    assert_eq!(ze_query_result_free(&mut result), ZeErrorCode::ZeOk);
    hits
}

#[test]
fn ze_265_reopen_warm_exact_and_prefix_parity() {
    let mut store = common::TestStore::new();
    let vector = [1.0, 0.0];
    let text = b"common pair";
    let mut doc: ZeIngestDocument = sized_zeroed();
    doc.doc_id.low = 1;
    doc.revision = 1;
    doc.vector = vector.as_ptr();
    doc.vector_len = 2;
    doc.text = text.as_ptr();
    doc.text_len = text.len();
    let mut ingest: ZeIngestRequest = sized_zeroed();
    ingest.documents = &doc;
    ingest.document_count = 1;
    ingest.dimension = 2;
    let mut report: ZeMutationReport = sized_zeroed();
    assert_eq!(
        ze_ingest(store.handle, &ingest, &mut report),
        ZeErrorCode::ZeOk
    );
    let seal: ZeSealRequest = sized_zeroed();
    let mut generation: ZeGenerationReport = sized_zeroed();
    assert_eq!(
        ze_seal(store.handle, &seal, &mut generation),
        ZeErrorCode::ZeOk
    );
    let expected = hits(store.handle, false);
    assert_eq!(expected.len(), 1);
    let expected_prefix = hits(store.handle, true);
    assert_eq!(expected_prefix.len(), 1);
    assert_eq!(store.close(), ZeErrorCode::ZeOk);
    let (code, handle) = common::open_path(&store.path);
    assert_eq!(code, ZeErrorCode::ZeOk);
    store.handle = handle;
    let request: ZeWarmLexicalRequest = sized_zeroed();
    for _ in 0..2 {
        assert_eq!(ze_warm_lexical(handle, &request), ZeErrorCode::ZeOk);
    }
    assert_eq!(hits(handle, false), expected);
    assert_eq!(hits(handle, true), expected_prefix);
}

#[test]
fn ze_265_warm_controls_and_request_validation() {
    let mut store = common::TestStore::new();
    let mut request: ZeWarmLexicalRequest = sized_zeroed();
    let mut token = 0;
    assert_eq!(ze_cancel_token_create(&mut token), ZeErrorCode::ZeOk);
    assert_eq!(ze_cancel_token_cancel(token), ZeErrorCode::ZeOk);
    request.cancel_token = token;
    assert_eq!(
        ze_warm_lexical(store.handle, &request),
        ZeErrorCode::ZeErrCancelled
    );
    request.deadline_ns = 1;
    assert_eq!(
        ze_warm_lexical(store.handle, &request),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(ze_cancel_token_free(token), ZeErrorCode::ZeOk);
    request.deadline_ns = 0;
    assert_eq!(
        ze_warm_lexical(store.handle, &request),
        ZeErrorCode::ZeErrClosed
    );
    request.cancel_token = 0;
    assert_eq!(
        ze_warm_lexical(store.handle, std::ptr::null()),
        ZeErrorCode::ZeErrInvalidArgument
    );
    request.abi_reserved = 1;
    assert_eq!(
        ze_warm_lexical(store.handle, &request),
        ZeErrorCode::ZeErrInvalidArgument
    );
    request.abi_reserved = 0;
    request.abi_size = 8;
    assert_eq!(
        ze_warm_lexical(store.handle, &request),
        ZeErrorCode::ZeErrInvalidArgument
    );
    request.abi_size = 65_537;
    assert_eq!(
        ze_warm_lexical(store.handle, &request),
        ZeErrorCode::ZeErrInvalidArgument
    );
    request = sized_zeroed();
    request.deadline_ns = 1;
    assert_eq!(
        ze_warm_lexical(store.handle, &request),
        ZeErrorCode::ZeErrTimeout
    );
    request.deadline_ns = 0;
    assert_eq!(
        ze_warm_lexical(0, &request),
        ZeErrorCode::ZeErrInvalidHandle
    );
    let handle = store.handle;
    assert_eq!(store.close(), ZeErrorCode::ZeOk);
    assert_eq!(ze_warm_lexical(handle, &request), ZeErrorCode::ZeErrClosed);
}
