mod common;

use std::mem::size_of;

use zeppelin_embed_ffi::*;

const U: u32 = 1;
const I: u32 = 2;
const F: u32 = 3;
const FLAG: u32 = 4;

fn open_namespace(root: &std::path::Path) -> ZeHandle {
    let names: [&[u8]; 4] = [b"u", b"i", b"f", b"flag"];
    let attributes = names
        .iter()
        .zip(1_u32..)
        .map(|(name, id)| ZeAttributeDefinition {
            attribute_id: id,
            name: name.as_ptr(),
            name_len: name.len(),
            attribute_type: id as i32,
            nullable: 1,
        })
        .collect::<Vec<_>>();
    let spec = ZeNamespaceSpec {
        abi_size: size_of::<ZeNamespaceSpec>() as u32,
        abi_reserved: 0,
        attributes: attributes.as_ptr(),
        attribute_count: attributes.len(),
        has_vector_space: 0,
        dimensions: 0,
        normalization: 0,
        epoch: std::ptr::null(),
    };
    let root = root.to_string_lossy().into_owned().into_bytes();
    let name = b"ordered";
    let request = ZeNamespaceOpenRequest {
        abi_size: size_of::<ZeNamespaceOpenRequest>() as u32,
        abi_reserved: 0,
        root: root.as_ptr(),
        root_len: root.len(),
        name: name.as_ptr(),
        name_len: name.len(),
        open: ZeOpenRequest {
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
        },
        spec: &spec,
    };
    let mut handle = 0;
    assert_eq!(ze_namespace_open(&request, &mut handle), ZeErrorCode::ZeOk);
    handle
}

fn value(attribute_id: u32, value_type: i32) -> ZeAttributeValue {
    ZeAttributeValue {
        attribute_id,
        value_type,
        u64_value: 0,
        i64_value: 0,
        f64_value: 0.0,
        bool_value: 0,
        string_value: std::ptr::null(),
        string_len: 0,
    }
}

fn upsert(handle: ZeHandle, id: u64, attributes: &[ZeAttributeValue]) {
    let document = ZeUpsertDocument {
        abi_size: size_of::<ZeUpsertDocument>() as u32,
        abi_reserved: 0,
        document: ZeIngestDocument {
            abi_size: size_of::<ZeIngestDocument>() as u32,
            abi_reserved: 0,
            doc_id: ZeDocId { high: 0, low: id },
            revision: 1,
            timestamp: 0,
            vector: std::ptr::null(),
            vector_len: 0,
            metadata: std::ptr::null(),
            metadata_len: 0,
            text: std::ptr::null(),
            text_len: 0,
        },
        attributes: attributes.as_ptr(),
        attribute_count: attributes.len(),
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

fn upsert_numbers(handle: ZeHandle, id: u64, u: u64, i: i64, f: f64) {
    upsert(
        handle,
        id,
        &[
            ZeAttributeValue {
                u64_value: u,
                ..value(U, 1)
            },
            ZeAttributeValue {
                i64_value: i,
                ..value(I, 2)
            },
            ZeAttributeValue {
                f64_value: f,
                ..value(F, 3)
            },
        ],
    );
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

fn ordered(limit: usize, order: i32, attribute_id: u32) -> ZeScanOrderedRequest {
    ZeScanOrderedRequest {
        abi_size: size_of::<ZeScanOrderedRequest>() as u32,
        abi_reserved: 0,
        scan: ZeScanRequest {
            limit,
            order,
            ..common::sized_zeroed()
        },
        order_attribute_id: attribute_id,
        cursor_order: 0,
        cursor_order_attribute_id: 0,
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

/// Pages through `request` to the end, resuming with each returned cursor
/// tagged by the request's own order.
fn scan_all(handle: ZeHandle, mut request: ZeScanOrderedRequest) -> Vec<u64> {
    let mut ids = Vec::new();
    loop {
        let mut result: ZeScanResult = common::sized_zeroed();
        assert_eq!(
            ze_scan_ordered(handle, &request, &mut result),
            ZeErrorCode::ZeOk
        );
        ids.extend(page_ids(&result));
        let has_more = result.has_more;
        request.scan.cursor_generation = result.generation;
        request.scan.cursor_segment_id = result.next_segment_id;
        request.scan.cursor_next_row = result.next_row;
        request.scan.cursor_phase = result.next_phase;
        request.cursor_order = request.scan.order;
        request.cursor_order_attribute_id = request.order_attribute_id;
        assert_eq!(ze_scan_result_free(&mut result), ZeErrorCode::ZeOk);
        if has_more == 0 {
            return ids;
        }
    }
}

fn first_page_cursor(handle: ZeHandle, request: &ZeScanOrderedRequest) -> ZeScanOrderedRequest {
    let mut result: ZeScanResult = common::sized_zeroed();
    assert_eq!(
        ze_scan_ordered(handle, request, &mut result),
        ZeErrorCode::ZeOk
    );
    assert_eq!(result.has_more, 1);
    let mut next = *request;
    next.scan.cursor_generation = result.generation;
    next.scan.cursor_segment_id = result.next_segment_id;
    next.scan.cursor_next_row = result.next_row;
    next.scan.cursor_phase = result.next_phase;
    next.cursor_order = request.scan.order;
    next.cursor_order_attribute_id = request.order_attribute_id;
    assert_eq!(ze_scan_result_free(&mut result), ZeErrorCode::ZeOk);
    next
}

fn last_error(handle: ZeHandle) -> String {
    let mut required = 0;
    assert_eq!(
        ze_last_error_message(handle, std::ptr::null_mut(), 0, &mut required),
        ZeErrorCode::ZeOk
    );
    let mut bytes = vec![0_i8; required + 1];
    assert_eq!(
        ze_last_error_message(handle, bytes.as_mut_ptr(), bytes.len(), &mut required),
        ZeErrorCode::ZeOk
    );
    String::from_utf8(
        bytes
            .into_iter()
            .take(required)
            .map(|byte| byte as u8)
            .collect(),
    )
    .expect("last error UTF-8")
}

#[test]
fn scan_ordered_sorts_each_numeric_attribute_across_sealed_and_active_rows() {
    let root = tempfile::tempdir().expect("namespace root");
    let handle = open_namespace(root.path());
    upsert_numbers(handle, 5, u64::MAX, -7, -0.0);
    upsert_numbers(handle, 3, 2, i64::MIN, f64::NAN);
    seal(handle);
    upsert_numbers(handle, 4, 2, 9, 0.0);
    upsert(handle, 1, &[]);
    upsert_numbers(handle, 2, 0, -7, f64::NEG_INFINITY);

    let cases = [
        (3, U, vec![2, 3, 4, 5, 1]),
        (4, U, vec![5, 3, 4, 2, 1]),
        (3, I, vec![3, 2, 5, 4, 1]),
        (4, I, vec![4, 2, 5, 3, 1]),
        (3, F, vec![2, 4, 5, 1, 3]),
        (4, F, vec![4, 5, 2, 1, 3]),
    ];
    for (order, attribute_id, expected) in cases {
        for limit in [1, 2, 5] {
            assert_eq!(
                scan_all(handle, ordered(limit, order, attribute_id)),
                expected,
                "order {order} attribute {attribute_id} limit {limit}"
            );
        }
    }
    // ze_scan_ordered also serves the ze_scan orders with tagged cursors.
    assert_eq!(scan_all(handle, ordered(2, 0, 0)), vec![5, 3, 4, 1, 2]);
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}

#[test]
fn scan_ordered_rejects_invalid_orders_and_foreign_cursors_precisely() {
    let root = tempfile::tempdir().expect("namespace root");
    let handle = open_namespace(root.path());
    upsert_numbers(handle, 1, 1, 1, 1.0);
    upsert_numbers(handle, 2, 2, 2, 2.0);
    let by_u = first_page_cursor(handle, &ordered(1, 3, U));
    let cases = [
        (
            ordered(1, 5, U),
            "scan order discriminant is out of range".to_owned(),
        ),
        (
            ordered(1, 1, U),
            "scan order_attribute_id must be zero unless the order is 3 or 4".to_owned(),
        ),
        (
            ZeScanOrderedRequest {
                cursor_order: 3,
                ..ordered(1, 3, U)
            },
            "scan cursor order fields must be zero when cursor_generation is zero".to_owned(),
        ),
        (
            ZeScanOrderedRequest {
                cursor_order: 9,
                ..by_u
            },
            "scan cursor_order discriminant is out of range".to_owned(),
        ),
        (
            ZeScanOrderedRequest {
                cursor_order: 2,
                ..by_u
            },
            "scan cursor_order_attribute_id must be zero unless cursor_order is 3 or 4".to_owned(),
        ),
        (
            ordered(1, 3, 9),
            "invalid document scan: scan order attribute 9 is not a declared attribute".to_owned(),
        ),
        (
            ordered(1, 4, 0),
            "invalid document scan: scan order attribute 0 is the document timestamp; \
             use a timestamp order"
                .to_owned(),
        ),
        (
            ordered(1, 3, FLAG),
            "invalid document scan: scan order attribute 4 has type Bool; \
             only u64, i64 and f64 attributes are orderable"
                .to_owned(),
        ),
        (
            ZeScanOrderedRequest {
                scan: ZeScanRequest {
                    order: 4,
                    ..by_u.scan
                },
                ..by_u
            },
            "invalid document scan: scan cursor was issued for order Attribute { column: \
             ColumnId(1), direction: Ascending }, but the request orders by Attribute { \
             column: ColumnId(1), direction: Descending }"
                .to_owned(),
        ),
        (
            ZeScanOrderedRequest {
                order_attribute_id: I,
                ..by_u
            },
            "invalid document scan: scan cursor was issued for order Attribute { column: \
             ColumnId(1), direction: Ascending }, but the request orders by Attribute { \
             column: ColumnId(2), direction: Ascending }"
                .to_owned(),
        ),
        (
            ZeScanOrderedRequest {
                scan: ZeScanRequest {
                    order: 1,
                    ..by_u.scan
                },
                order_attribute_id: 0,
                ..by_u
            },
            "invalid document scan: scan cursor was issued for order Attribute { column: \
             ColumnId(1), direction: Ascending }, but the request orders by \
             TimestampAscending"
                .to_owned(),
        ),
    ];
    for (request, message) in cases {
        let mut result: ZeScanResult = common::sized_zeroed();
        assert_eq!(
            ze_scan_ordered(handle, &request, &mut result),
            ZeErrorCode::ZeErrInvalidArgument,
            "{message}"
        );
        assert_eq!(last_error(handle), message);
        assert!(result.documents.is_null());
    }

    let mut result: ZeScanResult = common::sized_zeroed();
    let legacy = ZeScanRequest {
        order: 3,
        ..by_u.scan
    };
    assert_eq!(
        ze_scan(handle, &legacy, &mut result),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        last_error(handle),
        "attribute scan orders 3 and 4 require ze_scan_ordered"
    );
    let wrong_size = ZeScanOrderedRequest {
        scan: ZeScanRequest {
            abi_size: 8,
            ..by_u.scan
        },
        ..by_u
    };
    assert_eq!(
        ze_scan_ordered(handle, &wrong_size, &mut result),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}

#[test]
fn scan_ordered_cursor_is_stale_after_a_write_between_pages() {
    let root = tempfile::tempdir().expect("namespace root");
    let handle = open_namespace(root.path());
    upsert_numbers(handle, 1, 1, 1, 1.0);
    upsert_numbers(handle, 2, 2, 2, 2.0);
    let resume = first_page_cursor(handle, &ordered(1, 4, F));
    upsert_numbers(handle, 3, 3, 3, 3.0);
    let mut result: ZeScanResult = common::sized_zeroed();
    assert_eq!(
        ze_scan_ordered(handle, &resume, &mut result),
        ZeErrorCode::ZeErrScanStale
    );
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}
