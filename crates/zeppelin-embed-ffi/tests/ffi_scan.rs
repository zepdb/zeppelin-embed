mod common;

use std::mem::size_of;

use zeppelin_embed_ffi::*;

fn ingest(handle: ZeHandle, rows: &[(u64, i64)]) {
    let vector = [1.0_f32];
    let documents = rows
        .iter()
        .map(|(id, timestamp)| ZeIngestDocument {
            abi_size: size_of::<ZeIngestDocument>() as u32,
            abi_reserved: 0,
            doc_id: ZeDocId { high: 0, low: *id },
            revision: 1,
            timestamp: *timestamp,
            vector: vector.as_ptr(),
            vector_len: 1,
            metadata: std::ptr::null(),
            metadata_len: 0,
            text: std::ptr::null(),
            text_len: 0,
        })
        .collect::<Vec<_>>();
    let request = ZeIngestRequest {
        abi_size: size_of::<ZeIngestRequest>() as u32,
        abi_reserved: 0,
        documents: documents.as_ptr(),
        document_count: documents.len(),
        dimension: 1,
    };
    let mut report: ZeMutationReport = common::sized_zeroed();
    assert_eq!(ze_ingest(handle, &request, &mut report), ZeErrorCode::ZeOk);
}

fn seal(handle: ZeHandle) {
    let request = ZeSealRequest {
        abi_size: size_of::<ZeSealRequest>() as u32,
        abi_reserved: 0,
        cancel_token: 0,
    };
    let mut report: ZeGenerationReport = common::sized_zeroed();
    assert_eq!(ze_seal(handle, &request, &mut report), ZeErrorCode::ZeOk);
}

fn delete(handle: ZeHandle, id: u64) {
    let id = ZeDocId { high: 0, low: id };
    let request = ZeDeleteRequest {
        abi_size: size_of::<ZeDeleteRequest>() as u32,
        abi_reserved: 0,
        doc_ids: &id,
        doc_id_count: 1,
    };
    let mut report: ZeMutationReport = common::sized_zeroed();
    assert_eq!(ze_delete(handle, &request, &mut report), ZeErrorCode::ZeOk);
}

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
) -> ZeHandle {
    let root = root.to_string_lossy().into_owned().into_bytes();
    let spec = ZeNamespaceSpec {
        abi_size: size_of::<ZeNamespaceSpec>() as u32,
        abi_reserved: 0,
        attributes: attributes.as_ptr(),
        attribute_count: attributes.len(),
        has_vector_space,
        dimensions: has_vector_space,
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

fn upsert_u64(handle: ZeHandle, id: u64, timestamp: i64, value: Option<u64>) {
    let vector = [1.0_f32];
    let attribute = value.map(|value| ZeAttributeValue {
        attribute_id: 1,
        value_type: 1,
        u64_value: value,
        i64_value: 0,
        f64_value: 0.0,
        bool_value: 0,
        string_value: std::ptr::null(),
        string_len: 0,
    });
    let (attributes, attribute_count) = attribute
        .as_ref()
        .map_or((std::ptr::null(), 0), |attribute| (attribute, 1));
    let document = ZeUpsertDocument {
        abi_size: size_of::<ZeUpsertDocument>() as u32,
        abi_reserved: 0,
        document: ZeIngestDocument {
            abi_size: size_of::<ZeIngestDocument>() as u32,
            abi_reserved: 0,
            doc_id: ZeDocId { high: 0, low: id },
            revision: 1,
            timestamp,
            vector: vector.as_ptr(),
            vector_len: 1,
            metadata: std::ptr::null(),
            metadata_len: 0,
            text: std::ptr::null(),
            text_len: 0,
        },
        attributes,
        attribute_count,
    };
    let request = ZeUpsertRequest {
        abi_size: size_of::<ZeUpsertRequest>() as u32,
        abi_reserved: 0,
        documents: &document,
        document_count: 1,
        dimension: 1,
    };
    let mut report: ZeMutationReport = common::sized_zeroed();
    assert_eq!(ze_upsert(handle, &request, &mut report), ZeErrorCode::ZeOk);
}

fn upsert_record(handle: ZeHandle, id: u64, timestamp: i64) {
    let document = ZeUpsertDocument {
        abi_size: size_of::<ZeUpsertDocument>() as u32,
        abi_reserved: 0,
        document: ZeIngestDocument {
            abi_size: size_of::<ZeIngestDocument>() as u32,
            abi_reserved: 0,
            doc_id: ZeDocId { high: 0, low: id },
            revision: 1,
            timestamp,
            vector: std::ptr::null(),
            vector_len: 0,
            metadata: std::ptr::null(),
            metadata_len: 0,
            text: std::ptr::null(),
            text_len: 0,
        },
        attributes: std::ptr::null(),
        attribute_count: 0,
    };
    let request = ZeUpsertRequest {
        abi_size: size_of::<ZeUpsertRequest>() as u32,
        abi_reserved: 0,
        documents: &document,
        document_count: 1,
        dimension: 0,
    };
    let mut report: ZeMutationReport = common::sized_zeroed();
    assert_eq!(ze_upsert(handle, &request, &mut report), ZeErrorCode::ZeOk);
}

fn scan_request(limit: usize) -> ZeScanRequest {
    ZeScanRequest {
        abi_size: size_of::<ZeScanRequest>() as u32,
        abi_reserved: 0,
        cursor_generation: 0,
        cursor_segment_id: [0; 16],
        cursor_next_row: 0,
        cursor_phase: 0,
        limit,
        order: 0,
        include_vector: 0,
        include_text: 0,
        include_metadata: 0,
        include_attributes: 0,
        has_timestamp_range: 0,
        start_ts: 0,
        end_ts: 0,
        filter: std::ptr::null(),
        cancel_token: 0,
        deadline_ns: 0,
    }
}

fn page_ids(result: &ZeScanResult) -> Vec<u64> {
    if result.document_count == 0 {
        return Vec::new();
    }
    unsafe { std::slice::from_raw_parts(result.documents, result.document_count) }
        .iter()
        .map(|document| document.doc_id.low)
        .collect()
}

fn run_scan(handle: ZeHandle, request: &ZeScanRequest) -> ZeScanResult {
    let mut result: ZeScanResult = common::sized_zeroed();
    assert_eq!(ze_scan(handle, request, &mut result), ZeErrorCode::ZeOk);
    result
}

fn value_u64(value: u64) -> ZeAttributeValue {
    ZeAttributeValue {
        attribute_id: 1,
        value_type: 1,
        u64_value: value,
        i64_value: 0,
        f64_value: 0.0,
        bool_value: 0,
        string_value: std::ptr::null(),
        string_len: 0,
    }
}

fn empty_value() -> ZeAttributeValue {
    value_u64(0)
}

fn leaf_node(op: i32, values: &[ZeAttributeValue]) -> ZeFilterNode {
    ZeFilterNode {
        op,
        attribute_id: 1,
        values: values.as_ptr(),
        value_count: values.len(),
        has_lower: 0,
        lower: empty_value(),
        lower_inclusive: 0,
        has_upper: 0,
        upper: empty_value(),
        upper_inclusive: 0,
        children_start: 0,
        children_count: 0,
    }
}

fn logical_node(op: i32, children_start: u32, children_count: u32) -> ZeFilterNode {
    ZeFilterNode {
        op,
        attribute_id: 0,
        values: std::ptr::null(),
        value_count: 0,
        has_lower: 0,
        lower: empty_value(),
        lower_inclusive: 0,
        has_upper: 0,
        upper: empty_value(),
        upper_inclusive: 0,
        children_start,
        children_count,
    }
}

fn filtered_ids(handle: ZeHandle, nodes: &[ZeFilterNode], root: u32) -> Vec<u64> {
    let filter = ZeFilter {
        abi_size: size_of::<ZeFilter>() as u32,
        abi_reserved: 0,
        nodes: nodes.as_ptr(),
        node_count: nodes.len(),
        root,
    };
    let mut request = scan_request(16);
    request.filter = &filter;
    let mut result = run_scan(handle, &request);
    let ids = page_ids(&result);
    assert_eq!(ze_scan_result_free(&mut result), ZeErrorCode::ZeOk);
    ids
}

fn invalid_filter_code(handle: ZeHandle, nodes: &[ZeFilterNode], root: u32) -> ZeErrorCode {
    let filter = ZeFilter {
        abi_size: size_of::<ZeFilter>() as u32,
        abi_reserved: 0,
        nodes: nodes.as_ptr(),
        node_count: nodes.len(),
        root,
    };
    let mut request = scan_request(1);
    request.filter = &filter;
    let mut result: ZeScanResult = common::sized_zeroed();
    ze_scan(handle, &request, &mut result)
}

#[test]
fn scan_storage_pages_return_every_live_row_once_and_clear_has_more() {
    let mut store = common::TestStore::new();
    ingest(store.handle, &[(1, 30), (2, 10)]);
    seal(store.handle);
    ingest(store.handle, &[(3, 20), (4, 40)]);
    delete(store.handle, 4);
    let mut request = scan_request(2);
    let mut all = Vec::new();
    loop {
        let mut result: ZeScanResult = common::sized_zeroed();
        assert_eq!(
            ze_scan(store.handle, &request, &mut result),
            ZeErrorCode::ZeOk
        );
        all.extend(page_ids(&result));
        let has_more = result.has_more;
        if has_more != 0 {
            request.cursor_generation = result.generation;
            request.cursor_segment_id = result.next_segment_id;
            request.cursor_next_row = result.next_row;
            request.cursor_phase = result.next_phase;
        }
        assert_eq!(ze_scan_result_free(&mut result), ZeErrorCode::ZeOk);
        if has_more == 0 {
            break;
        }
    }
    assert_eq!(all, vec![1, 2, 3]);
    assert_eq!(store.close(), ZeErrorCode::ZeOk);
}

#[test]
fn scan_timestamp_orders_and_half_open_range_are_exact() {
    let mut store = common::TestStore::new();
    ingest(store.handle, &[(3, 20), (2, 10)]);
    seal(store.handle);
    ingest(store.handle, &[(1, 20), (4, 30)]);

    let mut ascending_request = scan_request(10);
    ascending_request.order = 1;
    let mut ascending = run_scan(store.handle, &ascending_request);
    assert_eq!(page_ids(&ascending), vec![2, 1, 3, 4]);
    assert_eq!(ze_scan_result_free(&mut ascending), ZeErrorCode::ZeOk);

    let mut descending_request = scan_request(10);
    descending_request.order = 2;
    let mut descending = run_scan(store.handle, &descending_request);
    assert_eq!(page_ids(&descending), vec![4, 1, 3, 2]);
    assert_eq!(ze_scan_result_free(&mut descending), ZeErrorCode::ZeOk);

    let mut range_request = scan_request(10);
    range_request.has_timestamp_range = 1;
    range_request.start_ts = 10;
    range_request.end_ts = 20;
    let mut range = run_scan(store.handle, &range_request);
    assert_eq!(page_ids(&range), vec![2]);
    assert_eq!(ze_scan_result_free(&mut range), ZeErrorCode::ZeOk);
    assert_eq!(store.close(), ZeErrorCode::ZeOk);
}

#[test]
fn scan_resume_after_write_and_seal_is_stale() {
    let mut store = common::TestStore::new();
    ingest(store.handle, &[(1, 1), (2, 2)]);
    let request = scan_request(1);
    let mut first = run_scan(store.handle, &request);
    let mut resume = request;
    resume.cursor_generation = first.generation;
    resume.cursor_segment_id = first.next_segment_id;
    resume.cursor_next_row = first.next_row;
    resume.cursor_phase = first.next_phase;
    assert_eq!(ze_scan_result_free(&mut first), ZeErrorCode::ZeOk);
    ingest(store.handle, &[(3, 3)]);
    seal(store.handle);

    let mut result: ZeScanResult = common::sized_zeroed();
    assert_eq!(
        ze_scan(store.handle, &resume, &mut result),
        ZeErrorCode::ZeErrScanStale
    );
    assert_eq!(store.close(), ZeErrorCode::ZeOk);
}

#[test]
fn scan_filter_operators_and_missing_field_semantics_match_the_core() {
    let root = tempfile::tempdir().expect("namespace root");
    let name = b"value";
    let definitions = [ZeAttributeDefinition {
        attribute_id: 1,
        name: name.as_ptr(),
        name_len: name.len(),
        attribute_type: 1,
        nullable: 1,
    }];
    let handle = open_namespace(root.path(), b"filters", &definitions, 1);
    upsert_u64(handle, 1, 10, Some(7));
    upsert_u64(handle, 2, 20, Some(8));
    upsert_u64(handle, 3, 30, None);

    let seven = [value_u64(7)];
    let seven_eight = [value_u64(7), value_u64(8)];
    assert_eq!(filtered_ids(handle, &[leaf_node(1, &seven)], 0), vec![1]);
    assert_eq!(filtered_ids(handle, &[leaf_node(2, &seven)], 0), vec![2, 3]);
    assert_eq!(
        filtered_ids(handle, &[leaf_node(3, &seven_eight)], 0),
        vec![1, 2]
    );
    assert_eq!(filtered_ids(handle, &[leaf_node(4, &seven)], 0), vec![2, 3]);
    let range = ZeFilterNode {
        op: 5,
        attribute_id: 1,
        values: std::ptr::null(),
        value_count: 0,
        has_lower: 1,
        lower: value_u64(7),
        lower_inclusive: 1,
        has_upper: 1,
        upper: value_u64(8),
        upper_inclusive: 0,
        children_start: 0,
        children_count: 0,
    };
    assert_eq!(filtered_ids(handle, &[range], 0), vec![1]);
    assert_eq!(filtered_ids(handle, &[leaf_node(6, &[])], 0), vec![1, 2]);
    assert_eq!(filtered_ids(handle, &[leaf_node(7, &[])], 0), vec![3]);

    let and_nodes = [
        logical_node(8, 1, 2),
        leaf_node(1, &seven),
        leaf_node(6, &[]),
    ];
    assert_eq!(filtered_ids(handle, &and_nodes, 0), vec![1]);
    let or_nodes = [
        logical_node(9, 1, 2),
        leaf_node(1, &seven),
        leaf_node(7, &[]),
    ];
    assert_eq!(filtered_ids(handle, &or_nodes, 0), vec![1, 3]);
    let not_nodes = [logical_node(10, 1, 1), leaf_node(1, &seven)];
    assert_eq!(filtered_ids(handle, &not_nodes, 0), vec![2, 3]);
    assert_eq!(
        filtered_ids(handle, &[logical_node(8, 0, 0)], 0),
        vec![1, 2, 3]
    );
    assert!(filtered_ids(handle, &[logical_node(9, 0, 0)], 0).is_empty());
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}

#[test]
fn scan_malformed_filters_are_typed_invalid_arguments() {
    let root = tempfile::tempdir().expect("namespace root");
    let name = b"value";
    let definitions = [ZeAttributeDefinition {
        attribute_id: 1,
        name: name.as_ptr(),
        name_len: name.len(),
        attribute_type: 1,
        nullable: 1,
    }];
    let handle = open_namespace(root.path(), b"malformed", &definitions, 1);
    upsert_u64(handle, 1, 1, Some(7));
    let seven = [value_u64(7)];
    let leaf = leaf_node(1, &seven);
    assert_eq!(
        invalid_filter_code(handle, &[leaf], 1),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        invalid_filter_code(handle, &[logical_node(8, 1, 1)], 0),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        invalid_filter_code(handle, &[logical_node(10, 0, 1)], 0),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        invalid_filter_code(
            handle,
            &[
                logical_node(10, 1, 2),
                leaf_node(1, &seven),
                leaf_node(1, &seven)
            ],
            0,
        ),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let two = [value_u64(7), value_u64(8)];
    assert_eq!(
        invalid_filter_code(handle, &[leaf_node(1, &two)], 0),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let wrong_type = [ZeAttributeValue {
        attribute_id: 1,
        value_type: 2,
        i64_value: 7,
        ..empty_value()
    }];
    assert_eq!(
        invalid_filter_code(handle, &[leaf_node(1, &wrong_type)], 0),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let mut depth = Vec::new();
    for index in 0..32_u32 {
        depth.push(logical_node(10, index + 1, 1));
    }
    depth.push(leaf_node(6, &[]));
    assert_eq!(
        invalid_filter_code(handle, &depth, 0),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}

#[test]
fn scan_cancellation_deadline_arguments_and_result_ownership_are_typed() {
    let mut store = common::TestStore::new();
    ingest(store.handle, &[(1, 1), (2, 2)]);
    let valid = scan_request(1);
    let mut result: ZeScanResult = common::sized_zeroed();
    assert_eq!(
        ze_scan(store.handle, std::ptr::null(), &mut result),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        ze_scan(store.handle, &valid, std::ptr::null_mut()),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let zero_size = ZeScanRequest {
        abi_size: 0,
        ..valid
    };
    assert_eq!(
        ze_scan(store.handle, &zero_size, &mut result),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let wrong_size = ZeScanRequest {
        abi_size: size_of::<ZeScanRequest>() as u32 - 1,
        ..valid
    };
    assert_eq!(
        ze_scan(store.handle, &wrong_size, &mut result),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let zero_limit = ZeScanRequest { limit: 0, ..valid };
    assert_eq!(
        ze_scan(store.handle, &zero_limit, &mut result),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let absurd_limit = ZeScanRequest {
        limit: ZE_MAX_K + 1,
        ..valid
    };
    assert_eq!(
        ze_scan(store.handle, &absurd_limit, &mut result),
        ZeErrorCode::ZeErrInvalidArgument
    );

    let mut token = 0;
    assert_eq!(ze_cancel_token_create(&mut token), ZeErrorCode::ZeOk);
    assert_eq!(ze_cancel_token_cancel(token), ZeErrorCode::ZeOk);
    let cancelled = ZeScanRequest {
        cancel_token: token,
        ..valid
    };
    assert_eq!(
        ze_scan(store.handle, &cancelled, &mut result),
        ZeErrorCode::ZeErrCancelled
    );
    assert_eq!(ze_cancel_token_free(token), ZeErrorCode::ZeOk);
    let deadline = ZeScanRequest {
        deadline_ns: 1,
        ..valid
    };
    assert_eq!(
        ze_scan(store.handle, &deadline, &mut result),
        ZeErrorCode::ZeErrTimeout
    );

    let mut owned = run_scan(store.handle, &valid);
    let documents = owned.documents;
    let document_count = owned.document_count;
    assert_eq!(ze_scan_result_free(&mut owned), ZeErrorCode::ZeOk);
    assert_eq!(
        ze_scan_result_free(&mut owned),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let mut stale: ZeScanResult = common::sized_zeroed();
    stale.documents = documents;
    stale.document_count = document_count;
    assert_eq!(
        ze_scan_result_free(&mut stale),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let mut foreign_buffer = [unsafe { std::mem::zeroed::<ZeStoredDocument>() }];
    let mut foreign: ZeScanResult = common::sized_zeroed();
    foreign.documents = foreign_buffer.as_mut_ptr();
    foreign.document_count = 1;
    assert_eq!(
        ze_scan_result_free(&mut foreign),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        ze_scan_result_free(std::ptr::null_mut()),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(store.close(), ZeErrorCode::ZeOk);
}

#[test]
fn scan_record_only_namespace_hides_the_sentinel_vector() {
    let root = tempfile::tempdir().expect("namespace root");
    let handle = open_namespace(root.path(), b"records", &[], 0);
    upsert_record(handle, 1, 1);
    let mut request = scan_request(1);
    request.include_vector = 1;
    let mut result = run_scan(handle, &request);
    let document = unsafe { &*result.documents };
    assert!(document.vector.is_null());
    assert_eq!(document.vector_len, 0);
    assert_eq!(ze_scan_result_free(&mut result), ZeErrorCode::ZeOk);
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}
