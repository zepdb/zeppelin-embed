//! Expected-revision conditions through the C ABI (ZE-224).
mod common;

use std::mem::size_of;

use zeppelin_embed_ffi::*;

const VECTOR: [f32; 2] = [1.0, 0.0];

fn doc_id(low: u64) -> ZeDocId {
    ZeDocId { high: 0, low }
}

fn upsert_document(id: u64, revision: u64) -> ZeUpsertDocument {
    ZeUpsertDocument {
        abi_size: size_of::<ZeUpsertDocument>() as u32,
        abi_reserved: 0,
        document: ZeIngestDocument {
            abi_size: size_of::<ZeIngestDocument>() as u32,
            abi_reserved: 0,
            doc_id: doc_id(id),
            revision,
            timestamp: 0,
            vector: VECTOR.as_ptr(),
            vector_len: VECTOR.len(),
            metadata: std::ptr::null(),
            metadata_len: 0,
            text: std::ptr::null(),
            text_len: 0,
        },
        attributes: std::ptr::null(),
        attribute_count: 0,
    }
}

fn condition(kind: u32, revision: u64) -> ZeRevisionCondition {
    ZeRevisionCondition {
        kind,
        reserved: 0,
        revision,
    }
}

fn upsert_request(
    documents: &[ZeUpsertDocument],
    conditions: &[ZeRevisionCondition],
) -> ZeConditionalUpsertRequest {
    ZeConditionalUpsertRequest {
        abi_size: size_of::<ZeConditionalUpsertRequest>() as u32,
        abi_reserved: 0,
        batch: ZeUpsertRequest {
            abi_size: size_of::<ZeUpsertRequest>() as u32,
            abi_reserved: 0,
            documents: documents.as_ptr(),
            document_count: documents.len(),
            dimension: VECTOR.len(),
        },
        conditions: conditions.as_ptr(),
        condition_count: conditions.len(),
    }
}

fn delete_request(
    ids: &[ZeDocId],
    conditions: &[ZeRevisionCondition],
) -> ZeConditionalDeleteRequest {
    ZeConditionalDeleteRequest {
        abi_size: size_of::<ZeConditionalDeleteRequest>() as u32,
        abi_reserved: 0,
        batch: ZeDeleteRequest {
            abi_size: size_of::<ZeDeleteRequest>() as u32,
            abi_reserved: 0,
            doc_ids: ids.as_ptr(),
            doc_id_count: ids.len(),
        },
        conditions: conditions.as_ptr(),
        condition_count: conditions.len(),
    }
}

fn upsert(
    handle: ZeHandle,
    request: &ZeConditionalUpsertRequest,
) -> (ZeErrorCode, ZeRevisionConflict) {
    let mut report: ZeMutationReport = common::sized_zeroed();
    let mut conflict: ZeRevisionConflict = common::sized_zeroed();
    let code = ze_upsert_conditional(handle, request, &mut report, &mut conflict);
    (code, conflict)
}

fn delete(
    handle: ZeHandle,
    request: &ZeConditionalDeleteRequest,
) -> (ZeErrorCode, ZeRevisionConflict) {
    let mut report: ZeMutationReport = common::sized_zeroed();
    let mut conflict: ZeRevisionConflict = common::sized_zeroed();
    let code = ze_delete_conditional(handle, request, &mut report, &mut conflict);
    (code, conflict)
}

fn live_revision(handle: ZeHandle, id: u64) -> Option<u64> {
    let ids = [doc_id(id)];
    let request = ZeGetRequest {
        abi_size: size_of::<ZeGetRequest>() as u32,
        abi_reserved: 0,
        ids: ids.as_ptr(),
        id_count: 1,
        include_vector: 0,
        include_text: 0,
        include_metadata: 0,
        include_attributes: 0,
    };
    let mut result: ZeGetResult = common::sized_zeroed();
    assert_eq!(ze_get(handle, &request, &mut result), ZeErrorCode::ZeOk);
    let document = unsafe { *result.documents };
    assert_eq!(ze_get_result_free(&mut result), ZeErrorCode::ZeOk);
    (document.has_document == 1).then_some(document.revision)
}

fn state(handle: ZeHandle) -> (u64, u64) {
    let mut stats: ZeStatsReport = common::sized_zeroed();
    assert_eq!(ze_stats(handle, &mut stats), ZeErrorCode::ZeOk);
    let ids = [doc_id(u64::MAX)];
    let request = ZeGetRequest {
        abi_size: size_of::<ZeGetRequest>() as u32,
        abi_reserved: 0,
        ids: ids.as_ptr(),
        id_count: 1,
        include_vector: 0,
        include_text: 0,
        include_metadata: 0,
        include_attributes: 0,
    };
    let mut result: ZeGetResult = common::sized_zeroed();
    assert_eq!(ze_get(handle, &request, &mut result), ZeErrorCode::ZeOk);
    let generation = result.generation;
    assert_eq!(ze_get_result_free(&mut result), ZeErrorCode::ZeOk);
    (generation, stats.wal_bytes)
}

#[test]
fn conditional_upsert_reports_the_failed_document_and_writes_nothing() {
    let store = common::TestStore::new();
    let handle = store.handle;
    let seed = [upsert_document(1, 1)];
    assert_eq!(
        upsert(handle, &upsert_request(&seed, &[condition(2, 0)])).0,
        ZeErrorCode::ZeOk
    );
    let before = state(handle);

    let batch = [upsert_document(2, 1), upsert_document(1, 2)];
    let (code, conflict) = upsert(
        handle,
        &upsert_request(&batch, &[condition(0, 0), condition(1, 5)]),
    );
    assert_eq!(code, ZeErrorCode::ZeErrRevisionConflict);
    assert_eq!(
        (
            conflict.index,
            conflict.doc_id,
            conflict.expected_kind,
            conflict.expected_revision,
            conflict.has_current,
            conflict.current_revision,
        ),
        (1, doc_id(1), 1, 5, 1, 1)
    );
    assert_eq!(state(handle), before);
    assert_eq!(live_revision(handle, 2), None);

    let (code, conflict) = upsert(
        handle,
        &upsert_request(&batch, &[condition(2, 0), condition(1, 1)]),
    );
    assert_eq!(code, ZeErrorCode::ZeOk);
    assert_eq!((conflict.index, conflict.expected_kind), (0, 0));
    assert_eq!(live_revision(handle, 1), Some(2));
    assert_eq!(live_revision(handle, 2), Some(1));
}

#[test]
fn conditional_delete_reports_absent_and_mismatched_documents() {
    let store = common::TestStore::new();
    let handle = store.handle;
    let seed = [upsert_document(1, 3)];
    assert_eq!(
        upsert(handle, &upsert_request(&seed, &[condition(0, 0)])).0,
        ZeErrorCode::ZeOk
    );

    let ids = [doc_id(9), doc_id(1)];
    let (code, conflict) = delete(
        handle,
        &delete_request(&ids, &[condition(2, 0), condition(2, 0)]),
    );
    assert_eq!(code, ZeErrorCode::ZeErrRevisionConflict);
    assert_eq!(
        (conflict.index, conflict.doc_id, conflict.expected_kind),
        (1, doc_id(1), 2)
    );
    assert_eq!((conflict.has_current, conflict.current_revision), (1, 3));
    assert_eq!(live_revision(handle, 1), Some(3));

    assert_eq!(
        delete(
            handle,
            &delete_request(&ids, &[condition(2, 0), condition(1, 3)])
        )
        .0,
        ZeErrorCode::ZeOk
    );
    assert_eq!(live_revision(handle, 1), None);

    let (code, conflict) = delete(handle, &delete_request(&ids[1..], &[condition(1, 3)]));
    assert_eq!(code, ZeErrorCode::ZeErrRevisionConflict);
    assert_eq!((conflict.has_current, conflict.current_revision), (0, 0));
}

#[test]
fn malformed_conditions_are_rejected_before_any_write() {
    let store = common::TestStore::new();
    let handle = store.handle;
    let documents = [upsert_document(1, 1)];
    let ids = [doc_id(1)];
    let before = state(handle);
    for conditions in [
        vec![],
        vec![condition(0, 0), condition(0, 0)],
        vec![condition(3, 0)],
        vec![condition(0, 7)],
        vec![condition(2, 7)],
        vec![ZeRevisionCondition {
            kind: 1,
            reserved: 1,
            revision: 1,
        }],
    ] {
        assert_eq!(
            upsert(handle, &upsert_request(&documents, &conditions)).0,
            ZeErrorCode::ZeErrInvalidArgument,
            "upsert conditions {conditions:?}"
        );
        assert_eq!(
            delete(handle, &delete_request(&ids, &conditions)).0,
            ZeErrorCode::ZeErrInvalidArgument,
            "delete conditions {conditions:?}"
        );
    }

    let request = upsert_request(&documents, &[condition(0, 0)]);
    let mut report: ZeMutationReport = common::sized_zeroed();
    assert_eq!(
        ze_upsert_conditional(handle, &request, &mut report, std::ptr::null_mut()),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let mut conflict: ZeRevisionConflict = common::sized_zeroed();
    conflict.abi_size -= 8;
    assert_eq!(
        ze_upsert_conditional(handle, &request, &mut report, &mut conflict),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let delete_request = delete_request(&ids, &[condition(0, 0)]);
    assert_eq!(
        ze_delete_conditional(handle, &delete_request, &mut report, std::ptr::null_mut()),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(state(handle), before);
}
