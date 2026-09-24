//! ZE-217: `ze_delete_where` deletes every document matching a filter in one
//! mutation and reports the count and the generation.

mod common;

use std::mem::size_of;
use std::path::Path;

use zeppelin_embed_ffi::*;

fn value_u64(attribute_id: u32, value: u64) -> ZeAttributeValue {
    ZeAttributeValue {
        attribute_id,
        value_type: 1,
        u64_value: value,
        i64_value: 0,
        f64_value: 0.0,
        bool_value: 0,
        string_value: std::ptr::null(),
        string_len: 0,
    }
}

fn eq_node(attribute_id: u32, value: &ZeAttributeValue) -> ZeFilterNode {
    ZeFilterNode {
        op: 1,
        attribute_id,
        values: value,
        value_count: 1,
        has_lower: 0,
        lower: value_u64(0, 0),
        lower_inclusive: 0,
        has_upper: 0,
        upper: value_u64(0, 0),
        upper_inclusive: 0,
        children_start: 0,
        children_count: 0,
    }
}

fn filter(nodes: &[ZeFilterNode]) -> ZeFilter {
    ZeFilter {
        abi_size: size_of::<ZeFilter>() as u32,
        abi_reserved: 0,
        nodes: nodes.as_ptr(),
        node_count: nodes.len(),
        root: 0,
    }
}

fn open_notes(root: &Path, access_mode: i32) -> (ZeErrorCode, ZeHandle) {
    let root = root.to_string_lossy().into_owned().into_bytes();
    let name = b"noteId";
    let attributes = [ZeAttributeDefinition {
        attribute_id: 1,
        name: name.as_ptr(),
        name_len: name.len(),
        attribute_type: 1,
        nullable: 0,
    }];
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
    let namespace = b"notes";
    let request = ZeNamespaceOpenRequest {
        abi_size: size_of::<ZeNamespaceOpenRequest>() as u32,
        abi_reserved: 0,
        root: root.as_ptr(),
        root_len: root.len(),
        name: namespace.as_ptr(),
        name_len: namespace.len(),
        open: ZeOpenRequest {
            abi_size: size_of::<ZeOpenRequest>() as u32,
            abi_reserved: 0,
            path: std::ptr::null(),
            path_len: 0,
            access_mode,
            durability_mode: 0,
            commit_tier: 1,
            reader_drain_timeout_ms: 250,
            max_resident_bytes: u64::MAX,
            max_temp_bytes: u64::MAX,
        },
        spec: &spec,
    };
    let mut handle = 0;
    let code = ze_namespace_open(&request, &mut handle);
    (code, handle)
}

fn upsert(handle: ZeHandle, id: u64, note: u64) {
    let text = format!("segment {id} of note {note}").into_bytes();
    let attribute = value_u64(1, note);
    let document = ZeUpsertDocument {
        abi_size: size_of::<ZeUpsertDocument>() as u32,
        abi_reserved: 0,
        document: ZeIngestDocument {
            abi_size: size_of::<ZeIngestDocument>() as u32,
            abi_reserved: 0,
            doc_id: ZeDocId { high: 0, low: id },
            revision: 1,
            timestamp: 1,
            vector: std::ptr::null(),
            vector_len: 0,
            metadata: std::ptr::null(),
            metadata_len: 0,
            text: text.as_ptr(),
            text_len: text.len(),
        },
        attributes: &attribute,
        attribute_count: 1,
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

fn seal(handle: ZeHandle) {
    let request = ZeSealRequest {
        abi_size: size_of::<ZeSealRequest>() as u32,
        abi_reserved: 0,
        cancel_token: 0,
    };
    let mut report: ZeGenerationReport = common::sized_zeroed();
    assert_eq!(ze_seal(handle, &request, &mut report), ZeErrorCode::ZeOk);
}

/// Count of documents with `noteId == note`, or of every document.
fn count(handle: ZeHandle, note: Option<u64>) -> ZeCountResult {
    let value = value_u64(1, note.unwrap_or(0));
    let nodes = [eq_node(1, &value)];
    let filter = filter(&nodes);
    let request = ZeCountRequest {
        abi_size: size_of::<ZeCountRequest>() as u32,
        abi_reserved: 0,
        filter: if note.is_some() {
            &filter
        } else {
            std::ptr::null()
        },
        has_timestamp_range: 0,
        start_ts: 0,
        end_ts: 0,
    };
    let mut result: ZeCountResult = common::sized_zeroed();
    assert_eq!(ze_count(handle, &request, &mut result), ZeErrorCode::ZeOk);
    result
}

fn delete_where(handle: ZeHandle, note: u64) -> (ZeErrorCode, ZeDeleteWhereReport) {
    let value = value_u64(1, note);
    let nodes = [eq_node(1, &value)];
    let filter = filter(&nodes);
    let request = ZeDeleteWhereRequest {
        abi_size: size_of::<ZeDeleteWhereRequest>() as u32,
        abi_reserved: 0,
        filter: &filter,
    };
    let mut report: ZeDeleteWhereReport = common::sized_zeroed();
    let code = ze_delete_where(handle, &request, &mut report);
    (code, report)
}

/// Six documents: ids 1..=3 sealed, 4..=6 active; odd ids are note 1.
fn populated(root: &Path) -> ZeHandle {
    let (code, handle) = open_notes(root, 0);
    assert_eq!(code, ZeErrorCode::ZeOk);
    for id in 1..=6 {
        upsert(handle, id, id % 2);
        if id == 3 {
            seal(handle);
        }
    }
    handle
}

#[test]
fn delete_where_removes_every_match_and_reports_count_and_generation() {
    let root = tempfile::tempdir().expect("namespace root");
    let handle = populated(root.path());
    let before = count(handle, None).generation;

    let (code, report) = delete_where(handle, 1);

    assert_eq!(code, ZeErrorCode::ZeOk);
    assert_eq!(report.deleted_count, 3);
    assert!(report.generation > before);
    assert_eq!(count(handle, None).generation, report.generation);
    assert_eq!(count(handle, Some(1)).count, 0);
    assert_eq!(count(handle, Some(0)).count, 3);
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}

#[test]
fn delete_where_without_a_match_reports_zero_at_the_current_generation() {
    let root = tempfile::tempdir().expect("namespace root");
    let handle = populated(root.path());
    let before = count(handle, None).generation;

    let (code, report) = delete_where(handle, 9);

    assert_eq!(code, ZeErrorCode::ZeOk);
    assert_eq!(report.deleted_count, 0);
    assert_eq!(report.generation, before);
    assert_eq!(count(handle, None).count, 6);
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}

#[test]
fn delete_where_rejects_bad_requests_without_deleting() {
    let root = tempfile::tempdir().expect("namespace root");
    let handle = populated(root.path());
    let value = value_u64(1, 1);
    let nodes = [eq_node(1, &value)];
    let good_filter = filter(&nodes);
    let request = ZeDeleteWhereRequest {
        abi_size: size_of::<ZeDeleteWhereRequest>() as u32,
        abi_reserved: 0,
        filter: &good_filter,
    };
    let mut report: ZeDeleteWhereReport = common::sized_zeroed();

    let null_filter = ZeDeleteWhereRequest {
        filter: std::ptr::null(),
        ..request
    };
    assert_eq!(
        ze_delete_where(handle, &null_filter, &mut report),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        ze_delete_where(handle, std::ptr::null(), &mut report),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        ze_delete_where(handle, &request, std::ptr::null_mut()),
        ZeErrorCode::ZeErrInvalidArgument
    );
    for abi_size in [0, size_of::<ZeDeleteWhereRequest>() as u32 - 1] {
        let short = ZeDeleteWhereRequest {
            abi_size,
            ..request
        };
        assert_eq!(
            ze_delete_where(handle, &short, &mut report),
            ZeErrorCode::ZeErrInvalidArgument
        );
    }
    let reserved = ZeDeleteWhereRequest {
        abi_reserved: 1,
        ..request
    };
    assert_eq!(
        ze_delete_where(handle, &reserved, &mut report),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let unknown_value = value_u64(7, 1);
    let unknown_nodes = [eq_node(7, &unknown_value)];
    let unknown_filter = filter(&unknown_nodes);
    let unknown = ZeDeleteWhereRequest {
        filter: &unknown_filter,
        ..request
    };
    assert_eq!(
        ze_delete_where(handle, &unknown, &mut report),
        ZeErrorCode::ZeErrInvalidArgument
    );

    assert_eq!(count(handle, None).count, 6);
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}

#[test]
fn delete_where_on_a_read_only_handle_is_an_access_mode_error() {
    let root = tempfile::tempdir().expect("namespace root");
    let handle = populated(root.path());
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
    let (code, reader) = open_notes(root.path(), 1);
    assert_eq!(code, ZeErrorCode::ZeOk);

    let (code, _) = delete_where(reader, 1);

    assert_eq!(code, ZeErrorCode::ZeErrAccessMode);
    assert_eq!(count(reader, Some(1)).count, 3);
    assert_eq!(ze_close(reader), ZeErrorCode::ZeOk);
}
