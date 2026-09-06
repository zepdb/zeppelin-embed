mod common;

use std::mem::size_of;

use zeppelin_embed_ffi::*;

fn open_settings() -> ZeOpenRequest {
    ZeOpenRequest {
        abi_size: size_of::<ZeOpenRequest>() as u32,
        abi_reserved: 0,
        path: std::ptr::null(),
        path_len: 0,
        access_mode: 0,
        durability_mode: 0,
        commit_tier: 1,
        reader_drain_timeout_ms: 250,
        max_resident_bytes: u64::MAX,
        max_temp_bytes: u64::MAX,
    }
}

fn open_namespace(
    root: &std::path::Path,
    name: &[u8],
    attributes: &[ZeAttributeDefinition],
    has_vector_space: u32,
    dimensions: u32,
) -> ZeHandle {
    let root = root.to_string_lossy().into_owned().into_bytes();
    let spec = ZeNamespaceSpec {
        abi_size: size_of::<ZeNamespaceSpec>() as u32,
        abi_reserved: 0,
        attributes: attributes.as_ptr(),
        attribute_count: attributes.len(),
        has_vector_space,
        dimensions,
        normalization: 0,
        epoch: std::ptr::null(),
    };
    let request = ZeNamespaceOpenRequest {
        abi_size: size_of::<ZeNamespaceOpenRequest>() as u32,
        abi_reserved: 0,
        root: root.as_ptr(),
        root_len: root.len(),
        name: name.as_ptr(),
        name_len: name.len(),
        open: open_settings(),
        spec: &spec,
    };
    let mut handle = 0;
    assert_eq!(ze_namespace_open(&request, &mut handle), ZeErrorCode::ZeOk);
    handle
}

#[allow(clippy::too_many_arguments)]
fn upsert(
    handle: ZeHandle,
    id: ZeDocId,
    revision: u64,
    vector: Option<&[f32]>,
    dimension: usize,
    text: &[u8],
    metadata: &[u8],
    attributes: &[ZeAttributeValue],
) -> ZeMutationReport {
    let (vector_pointer, vector_len) = vector.map_or((std::ptr::null(), 0), |vector| {
        (vector.as_ptr(), vector.len())
    });
    let document = ZeUpsertDocument {
        abi_size: size_of::<ZeUpsertDocument>() as u32,
        abi_reserved: 0,
        document: ZeIngestDocument {
            abi_size: size_of::<ZeIngestDocument>() as u32,
            abi_reserved: 0,
            doc_id: id,
            revision,
            timestamp: revision as i64,
            vector: vector_pointer,
            vector_len,
            metadata: metadata.as_ptr(),
            metadata_len: metadata.len(),
            text: text.as_ptr(),
            text_len: text.len(),
        },
        attributes: attributes.as_ptr(),
        attribute_count: attributes.len(),
    };
    let request = ZeUpsertRequest {
        abi_size: size_of::<ZeUpsertRequest>() as u32,
        abi_reserved: 0,
        documents: &document,
        document_count: 1,
        dimension,
    };
    let mut report: ZeMutationReport = common::sized_zeroed();
    assert_eq!(ze_upsert(handle, &request, &mut report), ZeErrorCode::ZeOk);
    report
}

fn get(handle: ZeHandle, ids: &[ZeDocId], flags: [u32; 4]) -> ZeGetResult {
    let request = ZeGetRequest {
        abi_size: size_of::<ZeGetRequest>() as u32,
        abi_reserved: 0,
        ids: ids.as_ptr(),
        id_count: ids.len(),
        include_vector: flags[0],
        include_text: flags[1],
        include_metadata: flags[2],
        include_attributes: flags[3],
    };
    let mut result: ZeGetResult = common::sized_zeroed();
    assert_eq!(ze_get(handle, &request, &mut result), ZeErrorCode::ZeOk);
    result
}

#[test]
fn get_returns_every_upserted_field_exactly() {
    let root = tempfile::tempdir().expect("temporary namespace root");
    let number_name = b"number";
    let label_name = b"label";
    let definitions = [
        ZeAttributeDefinition {
            attribute_id: 1,
            name: number_name.as_ptr(),
            name_len: number_name.len(),
            attribute_type: 1,
            nullable: 0,
        },
        ZeAttributeDefinition {
            attribute_id: 2,
            name: label_name.as_ptr(),
            name_len: label_name.len(),
            attribute_type: 6,
            nullable: 0,
        },
    ];
    let handle = open_namespace(root.path(), b"get-all", &definitions, 1, 2);
    let vector = [1.25_f32, -2.5];
    let text = b"stored text";
    let metadata = [7_u8, 8, 9];
    let label = b"blue";
    let attributes = [
        ZeAttributeValue {
            attribute_id: 1,
            value_type: 1,
            u64_value: 42,
            i64_value: 0,
            f64_value: 0.0,
            bool_value: 0,
            string_value: std::ptr::null(),
            string_len: 0,
        },
        ZeAttributeValue {
            attribute_id: 2,
            value_type: 5,
            u64_value: 0,
            i64_value: 0,
            f64_value: 0.0,
            bool_value: 0,
            string_value: label.as_ptr(),
            string_len: label.len(),
        },
    ];
    let document = ZeUpsertDocument {
        abi_size: size_of::<ZeUpsertDocument>() as u32,
        abi_reserved: 0,
        document: ZeIngestDocument {
            abi_size: size_of::<ZeIngestDocument>() as u32,
            abi_reserved: 0,
            doc_id: ZeDocId { high: 3, low: 5 },
            revision: 7,
            timestamp: -11,
            vector: vector.as_ptr(),
            vector_len: vector.len(),
            metadata: metadata.as_ptr(),
            metadata_len: metadata.len(),
            text: text.as_ptr(),
            text_len: text.len(),
        },
        attributes: attributes.as_ptr(),
        attribute_count: attributes.len(),
    };
    let upsert = ZeUpsertRequest {
        abi_size: size_of::<ZeUpsertRequest>() as u32,
        abi_reserved: 0,
        documents: &document,
        document_count: 1,
        dimension: vector.len(),
    };
    let mut mutation: ZeMutationReport = common::sized_zeroed();
    assert_eq!(ze_upsert(handle, &upsert, &mut mutation), ZeErrorCode::ZeOk);
    let ids = [ZeDocId { high: 3, low: 5 }];
    let request = ZeGetRequest {
        abi_size: size_of::<ZeGetRequest>() as u32,
        abi_reserved: 0,
        ids: ids.as_ptr(),
        id_count: ids.len(),
        include_vector: 1,
        include_text: 1,
        include_metadata: 1,
        include_attributes: 1,
    };
    let mut result: ZeGetResult = common::sized_zeroed();

    assert_eq!(ze_get(handle, &request, &mut result), ZeErrorCode::ZeOk);
    assert_eq!(result.document_count, 1);
    assert_eq!(result.missing_count, 0);
    assert_eq!(result.generation, mutation.generation);
    let returned = unsafe { &*result.documents };
    assert_eq!(returned.has_document, 1);
    assert_eq!(returned.doc_id, ids[0]);
    assert_eq!(returned.revision, 7);
    assert_eq!(returned.timestamp, -11);
    assert_eq!(
        unsafe { std::slice::from_raw_parts(returned.vector, returned.vector_len) },
        vector
    );
    assert_eq!(
        unsafe { std::slice::from_raw_parts(returned.text, returned.text_len) },
        text
    );
    assert_eq!(
        unsafe { std::slice::from_raw_parts(returned.metadata, returned.metadata_len) },
        metadata
    );
    let returned_attributes =
        unsafe { std::slice::from_raw_parts(returned.attributes, returned.attribute_count) };
    assert_eq!(returned_attributes.len(), 2);
    assert_eq!(returned_attributes[0].attribute_id, 1);
    assert_eq!(returned_attributes[0].value_type, 1);
    assert_eq!(returned_attributes[0].u64_value, 42);
    assert_eq!(returned_attributes[1].attribute_id, 2);
    assert_eq!(returned_attributes[1].value_type, 5);
    assert_eq!(
        unsafe {
            std::slice::from_raw_parts(
                returned_attributes[1].string_value,
                returned_attributes[1].string_len,
            )
        },
        label
    );
    assert_eq!(ze_get_result_free(&mut result), ZeErrorCode::ZeOk);
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}

#[test]
fn get_marks_never_written_id_missing() {
    let root = tempfile::tempdir().expect("temporary namespace root");
    let handle = open_namespace(root.path(), b"missing", &[], 1, 1);
    let ids = [ZeDocId { high: 0, low: 99 }];

    let mut result = get(handle, &ids, [1, 1, 1, 1]);

    assert_eq!(result.document_count, 1);
    assert_eq!(result.missing_count, 1);
    let document = unsafe { &*result.documents };
    assert_eq!(document.has_document, 0);
    assert_eq!(document.doc_id, ids[0]);
    assert!(document.vector.is_null());
    assert_eq!(document.vector_len, 0);
    assert!(document.text.is_null());
    assert_eq!(document.text_len, 0);
    assert!(document.metadata.is_null());
    assert_eq!(document.metadata_len, 0);
    assert!(document.attributes.is_null());
    assert_eq!(document.attribute_count, 0);
    assert_eq!(ze_get_result_free(&mut result), ZeErrorCode::ZeOk);
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}

#[test]
fn get_marks_deleted_id_missing() {
    let root = tempfile::tempdir().expect("temporary namespace root");
    let handle = open_namespace(root.path(), b"deleted", &[], 1, 1);
    let id = ZeDocId { high: 0, low: 4 };
    let vector = [1.0_f32];
    upsert(handle, id, 1, Some(&vector), 1, b"", b"", &[]);
    let delete = ZeDeleteRequest {
        abi_size: size_of::<ZeDeleteRequest>() as u32,
        abi_reserved: 0,
        doc_ids: &id,
        doc_id_count: 1,
    };
    let mut mutation: ZeMutationReport = common::sized_zeroed();
    assert_eq!(ze_delete(handle, &delete, &mut mutation), ZeErrorCode::ZeOk);

    let mut result = get(handle, &[id], [1, 1, 1, 1]);

    assert_eq!(result.missing_count, 1);
    assert_eq!(unsafe { (*result.documents).has_document }, 0);
    assert_eq!(ze_get_result_free(&mut result), ZeErrorCode::ZeOk);
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}

#[test]
fn get_returns_only_live_revision_after_two_upserts() {
    let root = tempfile::tempdir().expect("temporary namespace root");
    let handle = open_namespace(root.path(), b"revision", &[], 1, 1);
    let id = ZeDocId { high: 0, low: 7 };
    upsert(handle, id, 1, Some(&[1.0]), 1, b"old", b"", &[]);
    let second = upsert(handle, id, 2, Some(&[2.0]), 1, b"new", b"", &[]);

    let mut result = get(handle, &[id], [1, 1, 0, 0]);
    let document = unsafe { &*result.documents };

    assert_eq!(result.generation, second.generation);
    assert_eq!(document.has_document, 1);
    assert_eq!(document.revision, 2);
    assert_eq!(
        unsafe { std::slice::from_raw_parts(document.vector, 1) },
        [2.0]
    );
    assert_eq!(
        unsafe { std::slice::from_raw_parts(document.text, document.text_len) },
        b"new"
    );
    assert_eq!(ze_get_result_free(&mut result), ZeErrorCode::ZeOk);
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}

#[test]
fn get_include_flags_select_exactly_their_fields() {
    let root = tempfile::tempdir().expect("temporary namespace root");
    let attribute_name = b"rank";
    let definitions = [ZeAttributeDefinition {
        attribute_id: 1,
        name: attribute_name.as_ptr(),
        name_len: attribute_name.len(),
        attribute_type: 1,
        nullable: 0,
    }];
    let handle = open_namespace(root.path(), b"flags", &definitions, 1, 2);
    let id = ZeDocId { high: 1, low: 2 };
    let attribute = ZeAttributeValue {
        attribute_id: 1,
        value_type: 1,
        u64_value: 6,
        i64_value: 0,
        f64_value: 0.0,
        bool_value: 0,
        string_value: std::ptr::null(),
        string_len: 0,
    };
    upsert(
        handle,
        id,
        1,
        Some(&[3.0, 4.0]),
        2,
        b"text",
        b"meta",
        &[attribute],
    );
    let cases = [
        ([0, 0, 0, 0], [false, false, false, false]),
        ([1, 0, 0, 0], [true, false, false, false]),
        ([0, 1, 0, 0], [false, true, false, false]),
        ([0, 0, 1, 0], [false, false, true, false]),
        ([0, 0, 0, 1], [false, false, false, true]),
    ];
    for (flags, expected) in cases {
        let mut result = get(handle, &[id], flags);
        let document = unsafe { &*result.documents };
        assert_eq!(
            [
                !document.vector.is_null() && document.vector_len != 0,
                !document.text.is_null() && document.text_len != 0,
                !document.metadata.is_null() && document.metadata_len != 0,
                !document.attributes.is_null() && document.attribute_count != 0,
            ],
            expected
        );
        assert_eq!(ze_get_result_free(&mut result), ZeErrorCode::ZeOk);
    }
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}

#[test]
fn get_hides_record_only_sentinel_vector() {
    let root = tempfile::tempdir().expect("temporary namespace root");
    let handle = open_namespace(root.path(), b"records", &[], 0, 0);
    let id = ZeDocId { high: 0, low: 8 };
    upsert(handle, id, 1, None, 0, b"record", b"", &[]);

    let mut result = get(handle, &[id], [1, 1, 0, 0]);
    let document = unsafe { &*result.documents };

    assert_eq!(document.has_document, 1);
    assert!(document.vector.is_null());
    assert_eq!(document.vector_len, 0);
    assert_eq!(
        unsafe { std::slice::from_raw_parts(document.text, document.text_len) },
        b"record"
    );
    assert_eq!(ze_get_result_free(&mut result), ZeErrorCode::ZeOk);
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}

#[test]
fn get_preserves_order_across_hits_and_misses() {
    let root = tempfile::tempdir().expect("temporary namespace root");
    let handle = open_namespace(root.path(), b"order", &[], 1, 1);
    let first = ZeDocId { high: 0, low: 1 };
    let second = ZeDocId { high: 0, low: 2 };
    let missing = ZeDocId { high: 0, low: 3 };
    upsert(handle, first, 1, Some(&[1.0]), 1, b"", b"", &[]);
    upsert(handle, second, 1, Some(&[2.0]), 1, b"", b"", &[]);
    let ids = [second, missing, first, second];

    let mut result = get(handle, &ids, [0, 0, 0, 0]);
    let documents = unsafe { std::slice::from_raw_parts(result.documents, result.document_count) };

    assert_eq!(documents.len(), ids.len());
    assert_eq!(documents[0].doc_id, second);
    assert_eq!(documents[0].has_document, 1);
    assert_eq!(documents[1].doc_id, missing);
    assert_eq!(documents[1].has_document, 0);
    assert_eq!(documents[2].doc_id, first);
    assert_eq!(documents[2].has_document, 1);
    assert_eq!(documents[3].doc_id, second);
    assert_eq!(documents[3].has_document, 1);
    assert_eq!(result.missing_count, 1);
    assert_eq!(ze_get_result_free(&mut result), ZeErrorCode::ZeOk);
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}

#[test]
fn get_rejects_invalid_pointers_sizes_and_empty_request() {
    let root = tempfile::tempdir().expect("temporary namespace root");
    let handle = open_namespace(root.path(), b"invalid", &[], 1, 1);
    let id = ZeDocId { high: 0, low: 1 };
    let valid = ZeGetRequest {
        abi_size: size_of::<ZeGetRequest>() as u32,
        abi_reserved: 0,
        ids: &id,
        id_count: 1,
        include_vector: 0,
        include_text: 0,
        include_metadata: 0,
        include_attributes: 0,
    };
    let mut result: ZeGetResult = common::sized_zeroed();

    assert_eq!(
        ze_get(handle, std::ptr::null(), &mut result),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        ze_get(handle, &valid, std::ptr::null_mut()),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let null_ids = ZeGetRequest {
        ids: std::ptr::null(),
        ..valid
    };
    assert_eq!(
        ze_get(handle, &null_ids, &mut result),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let zero_size = ZeGetRequest {
        abi_size: 0,
        ..valid
    };
    assert_eq!(
        ze_get(handle, &zero_size, &mut result),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let wrong_size = ZeGetRequest {
        abi_size: size_of::<ZeGetRequest>() as u32 - 1,
        ..valid
    };
    assert_eq!(
        ze_get(handle, &wrong_size, &mut result),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let empty = ZeGetRequest {
        ids: std::ptr::null(),
        id_count: 0,
        ..valid
    };
    assert_eq!(
        ze_get(handle, &empty, &mut result),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}

#[test]
fn get_result_free_rejects_double_free_and_foreign_pointer() {
    let root = tempfile::tempdir().expect("temporary namespace root");
    let handle = open_namespace(root.path(), b"free", &[], 1, 1);
    let id = ZeDocId { high: 0, low: 1 };
    upsert(handle, id, 1, Some(&[1.0]), 1, b"", b"", &[]);
    let mut result = get(handle, &[id], [1, 0, 0, 0]);

    assert_eq!(ze_get_result_free(&mut result), ZeErrorCode::ZeOk);
    assert_eq!(
        ze_get_result_free(&mut result),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let mut foreign_documents = [unsafe { std::mem::zeroed::<ZeStoredDocument>() }];
    let mut foreign: ZeGetResult = common::sized_zeroed();
    foreign.documents = foreign_documents.as_mut_ptr();
    foreign.document_count = foreign_documents.len();
    assert_eq!(
        ze_get_result_free(&mut foreign),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        ze_get_result_free(std::ptr::null_mut()),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}
