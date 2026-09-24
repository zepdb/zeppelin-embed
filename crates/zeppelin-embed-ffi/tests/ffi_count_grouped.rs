mod common;

use std::mem::size_of;

use zeppelin_embed_ffi::*;

const FOLDER: u32 = 1;
const DAY: u32 = 2;
const SCORE: u32 = 3;

fn last_error(handle: ZeHandle) -> String {
    let mut length = 0;
    assert_eq!(
        ze_last_error_message(handle, std::ptr::null_mut(), 0, &mut length),
        ZeErrorCode::ZeOk
    );
    let mut bytes = vec![0_u8; length + 1];
    assert_eq!(
        ze_last_error_message(handle, bytes.as_mut_ptr().cast(), bytes.len(), &mut length),
        ZeErrorCode::ZeOk
    );
    assert_eq!(bytes.pop(), Some(0));
    String::from_utf8(bytes).expect("UTF-8 error")
}

fn open(root: &std::path::Path) -> ZeHandle {
    let definition =
        |attribute_id: u32, name: &'static [u8], attribute_type: i32| ZeAttributeDefinition {
            attribute_id,
            name: name.as_ptr(),
            name_len: name.len(),
            attribute_type,
            nullable: 1,
        };
    let attributes = [
        definition(FOLDER, b"folder", 5),
        definition(DAY, b"day", 2),
        definition(SCORE, b"score", 3),
    ];
    let root = root.to_string_lossy().into_owned().into_bytes();
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
    let name = b"notes";
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

fn attribute(attribute_id: u32) -> ZeAttributeValue {
    ZeAttributeValue {
        attribute_id,
        value_type: 0,
        u64_value: 0,
        i64_value: 0,
        f64_value: 0.0,
        bool_value: 0,
        string_value: std::ptr::null(),
        string_len: 0,
    }
}

fn upsert(handle: ZeHandle, id: u64, revision: u64, folder: Option<&str>, day: Option<i64>) {
    let mut attributes = Vec::new();
    if let Some(folder) = folder {
        attributes.push(ZeAttributeValue {
            value_type: 5,
            string_value: folder.as_ptr(),
            string_len: folder.len(),
            ..attribute(FOLDER)
        });
    }
    if let Some(day) = day {
        attributes.push(ZeAttributeValue {
            value_type: 2,
            i64_value: day,
            ..attribute(DAY)
        });
    }
    let document = ZeUpsertDocument {
        abi_size: size_of::<ZeUpsertDocument>() as u32,
        abi_reserved: 0,
        document: ZeIngestDocument {
            abi_size: size_of::<ZeIngestDocument>() as u32,
            abi_reserved: 0,
            doc_id: ZeDocId { high: 0, low: id },
            revision,
            timestamp: id as i64,
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

fn delete(handle: ZeHandle, id: u64) {
    let doc_id = ZeDocId { high: 0, low: id };
    let request = ZeDeleteRequest {
        abi_size: size_of::<ZeDeleteRequest>() as u32,
        abi_reserved: 0,
        doc_ids: &doc_id,
        doc_id_count: 1,
    };
    let mut report: ZeMutationReport = common::sized_zeroed();
    assert_eq!(ze_delete(handle, &request, &mut report), ZeErrorCode::ZeOk);
}

fn seal(handle: ZeHandle) {
    let request = ZeSealRequest {
        abi_size: size_of::<ZeSealRequest>() as u32,
        abi_reserved: 0,
        cancel_token: 0,
    };
    let mut result: ZeGenerationReport = common::sized_zeroed();
    assert_eq!(ze_seal(handle, &request, &mut result), ZeErrorCode::ZeOk);
}

fn count_request() -> ZeCountRequest {
    ZeCountRequest {
        abi_size: size_of::<ZeCountRequest>() as u32,
        abi_reserved: 0,
        filter: std::ptr::null(),
        has_timestamp_range: 0,
        start_ts: 0,
        end_ts: 0,
    }
}

fn grouped_request(group_attribute_id: u32, group_limit: usize) -> ZeCountGroupedRequest {
    ZeCountGroupedRequest {
        abi_size: size_of::<ZeCountGroupedRequest>() as u32,
        abi_reserved: 0,
        count: count_request(),
        group_attribute_id,
        reserved: 0,
        group_limit,
    }
}

#[derive(Debug, PartialEq)]
enum Value {
    I64(i64),
    String(String),
}

fn groups(result: &ZeCountGroupedResult) -> Vec<(Value, u64)> {
    unsafe { std::slice::from_raw_parts(result.groups, result.group_count) }
        .iter()
        .map(|group| {
            let value = match group.value.value_type {
                2 => Value::I64(group.value.i64_value),
                5 if group.value.string_len == 0 => Value::String(String::new()),
                5 => Value::String(
                    String::from_utf8(
                        unsafe {
                            std::slice::from_raw_parts(
                                group.value.string_value,
                                group.value.string_len,
                            )
                        }
                        .to_vec(),
                    )
                    .expect("UTF-8 group"),
                ),
                other => panic!("unexpected group value type {other}"),
            };
            (value, group.count)
        })
        .collect()
}

fn failed_grouped(handle: ZeHandle, request: &ZeCountGroupedRequest) -> (ZeErrorCode, String) {
    let mut result: ZeCountGroupedResult = common::sized_zeroed();
    let code = ze_count_grouped(handle, request, &mut result);
    assert!(result.groups.is_null());
    assert_eq!((result.group_count, result.count), (0, 0));
    (code, last_error(handle))
}

#[test]
fn count_grouped_orders_groups_counts_missing_and_pins_one_generation() {
    let directory = tempfile::tempdir().expect("directory");
    let handle = open(directory.path());
    upsert(handle, 1, 1, Some("work"), Some(3));
    upsert(handle, 2, 1, Some("home"), Some(-1));
    upsert(handle, 3, 1, None, None);
    seal(handle);
    upsert(handle, 1, 2, Some("archive"), Some(3));
    upsert(handle, 4, 1, Some("work"), None);
    upsert(handle, 5, 1, Some(""), Some(3));
    delete(handle, 2);

    let mut result: ZeCountGroupedResult = common::sized_zeroed();
    assert_eq!(
        ze_count_grouped(handle, &grouped_request(FOLDER, 8), &mut result),
        ZeErrorCode::ZeOk
    );
    let mut plain: ZeCountResult = common::sized_zeroed();
    assert_eq!(
        ze_count(handle, &count_request(), &mut plain),
        ZeErrorCode::ZeOk
    );

    assert_eq!(
        groups(&result),
        vec![
            (Value::String(String::new()), 1),
            (Value::String("archive".to_owned()), 1),
            (Value::String("work".to_owned()), 1),
        ]
    );
    assert!(
        unsafe { std::slice::from_raw_parts(result.groups, result.group_count) }
            .iter()
            .all(|group| group.value.attribute_id == FOLDER)
    );
    assert_eq!(
        (result.missing_count, result.count, result.generation),
        (1, plain.count, plain.generation)
    );
    assert_eq!(plain.count, 4);
    assert_eq!(ze_count_grouped_result_free(&mut result), ZeErrorCode::ZeOk);
    assert!(result.groups.is_null());
    assert_eq!(
        ze_count_grouped_result_free(&mut result),
        ZeErrorCode::ZeErrInvalidArgument
    );

    let bounds = [ZeFilterNode {
        op: 5,
        attribute_id: 0,
        values: std::ptr::null(),
        value_count: 0,
        has_lower: 1,
        lower: ZeAttributeValue {
            value_type: 2,
            i64_value: 4,
            ..attribute(0)
        },
        lower_inclusive: 1,
        has_upper: 0,
        upper: attribute(0),
        upper_inclusive: 0,
        children_start: 0,
        children_count: 0,
    }];
    let filter = ZeFilter {
        abi_size: size_of::<ZeFilter>() as u32,
        abi_reserved: 0,
        nodes: bounds.as_ptr(),
        node_count: bounds.len(),
        root: 0,
    };
    let mut by_day = grouped_request(DAY, 8);
    by_day.count.filter = &filter;
    let mut result: ZeCountGroupedResult = common::sized_zeroed();
    assert_eq!(
        ze_count_grouped(handle, &by_day, &mut result),
        ZeErrorCode::ZeOk
    );
    assert_eq!(groups(&result), vec![(Value::I64(3), 1)]);
    assert_eq!((result.missing_count, result.count), (1, 2));
    assert_eq!(ze_count_grouped_result_free(&mut result), ZeErrorCode::ZeOk);

    let mut empty_range = grouped_request(DAY, 8);
    empty_range.count.has_timestamp_range = 1;
    empty_range.count.start_ts = 100;
    empty_range.count.end_ts = 200;
    let mut result: ZeCountGroupedResult = common::sized_zeroed();
    assert_eq!(
        ze_count_grouped(handle, &empty_range, &mut result),
        ZeErrorCode::ZeOk
    );
    assert!(!result.groups.is_null());
    assert_eq!((result.group_count, result.count), (0, 0));
    assert_eq!(ze_count_grouped_result_free(&mut result), ZeErrorCode::ZeOk);
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}

#[test]
fn count_grouped_rejects_bad_requests_with_precise_errors() {
    let directory = tempfile::tempdir().expect("directory");
    let handle = open(directory.path());
    upsert(handle, 1, 1, Some("a"), Some(1));
    upsert(handle, 2, 1, Some("b"), Some(1));
    upsert(handle, 3, 1, Some("c"), Some(2));

    assert_eq!(
        failed_grouped(handle, &grouped_request(FOLDER, 2)),
        (
            ZeErrorCode::ZeErrBudgetExceeded,
            "grouped count found more than 2 distinct values; raise the group limit".to_owned()
        )
    );
    assert_eq!(
        failed_grouped(handle, &grouped_request(DAY, 1)).0,
        ZeErrorCode::ZeErrBudgetExceeded
    );
    for limit in [0, ZE_MAX_COUNT_GROUPS + 1] {
        assert_eq!(
            failed_grouped(handle, &grouped_request(FOLDER, limit)),
            (
                ZeErrorCode::ZeErrInvalidArgument,
                "group_limit must be in 1..=ZE_MAX_COUNT_GROUPS".to_owned()
            )
        );
    }
    let (code, message) = failed_grouped(handle, &grouped_request(SCORE, 4));
    assert_eq!(code, ZeErrorCode::ZeErrInvalidArgument);
    assert!(
        message.contains("group-by attribute 3 has type F64"),
        "{message}"
    );
    let (code, message) = failed_grouped(handle, &grouped_request(9, 4));
    assert_eq!(code, ZeErrorCode::ZeErrInvalidArgument);
    assert!(
        message.contains("group-by attribute 9 is not in the schema"),
        "{message}"
    );

    let mut reserved = grouped_request(FOLDER, 4);
    reserved.reserved = 1;
    assert_eq!(
        failed_grouped(handle, &reserved),
        (
            ZeErrorCode::ZeErrInvalidArgument,
            "grouped count reserved field must be zero".to_owned()
        )
    );
    let mut stray_bounds = grouped_request(FOLDER, 4);
    stray_bounds.count.end_ts = 5;
    assert_eq!(
        failed_grouped(handle, &stray_bounds).0,
        ZeErrorCode::ZeErrInvalidArgument
    );
    let mut embedded_size = grouped_request(FOLDER, 4);
    embedded_size.count.abi_size = 8;
    assert_eq!(
        failed_grouped(handle, &embedded_size).0,
        ZeErrorCode::ZeErrInvalidArgument
    );

    let mut result: ZeCountGroupedResult = common::sized_zeroed();
    assert_eq!(
        ze_count_grouped(handle, std::ptr::null(), &mut result),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        ze_count_grouped(handle, &grouped_request(FOLDER, 4), std::ptr::null_mut()),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        ze_count_grouped_result_free(std::ptr::null_mut()),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}
