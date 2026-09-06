mod common;

use std::mem::size_of;

use zeppelin_embed_ffi::*;

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

fn count(handle: ZeHandle, request: &ZeCountRequest) -> ZeCountResult {
    let mut result: ZeCountResult = common::sized_zeroed();
    assert_eq!(ze_count(handle, request, &mut result), ZeErrorCode::ZeOk);
    result
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

fn scan_generation(handle: ZeHandle) -> u64 {
    let request = ZeScanRequest {
        abi_size: size_of::<ZeScanRequest>() as u32,
        abi_reserved: 0,
        cursor_generation: 0,
        cursor_segment_id: [0; 16],
        cursor_next_row: 0,
        cursor_phase: 0,
        limit: 1,
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
    };
    let mut result: ZeScanResult = common::sized_zeroed();
    assert_eq!(ze_scan(handle, &request, &mut result), ZeErrorCode::ZeOk);
    let generation = result.generation;
    assert_eq!(ze_scan_result_free(&mut result), ZeErrorCode::ZeOk);
    generation
}

fn open_filter_namespace(root: &std::path::Path) -> ZeHandle {
    let name = b"value";
    let attributes = [ZeAttributeDefinition {
        attribute_id: 1,
        name: name.as_ptr(),
        name_len: name.len(),
        attribute_type: 1,
        nullable: 1,
    }];
    open_namespace(root, b"count", &attributes, 1)
}

fn open_namespace(
    root: &std::path::Path,
    namespace: &[u8],
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
        name: namespace.as_ptr(),
        name_len: namespace.len(),
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

fn upsert_record_text(handle: ZeHandle, text: &[u8]) {
    let document = ZeUpsertDocument {
        abi_size: size_of::<ZeUpsertDocument>() as u32,
        abi_reserved: 0,
        document: ZeIngestDocument {
            abi_size: size_of::<ZeIngestDocument>() as u32,
            abi_reserved: 0,
            doc_id: ZeDocId { high: 0, low: 1 },
            revision: 1,
            timestamp: 1,
            vector: std::ptr::null(),
            vector_len: 0,
            metadata: std::ptr::null(),
            metadata_len: 0,
            text: text.as_ptr(),
            text_len: text.len(),
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
    let mut result: ZeMutationReport = common::sized_zeroed();
    assert_eq!(ze_upsert(handle, &request, &mut result), ZeErrorCode::ZeOk);
}

fn upsert_value(handle: ZeHandle, id: u64, timestamp: i64, value: Option<u64>) {
    let vector = [id as f32];
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

fn leaf(op: i32, values: &[ZeAttributeValue]) -> ZeFilterNode {
    ZeFilterNode {
        op,
        attribute_id: 1,
        values: values.as_ptr(),
        value_count: values.len(),
        has_lower: 0,
        lower: value_u64(0),
        lower_inclusive: 0,
        has_upper: 0,
        upper: value_u64(0),
        upper_inclusive: 0,
        children_start: 0,
        children_count: 0,
    }
}

fn logical(op: i32, children_start: u32, children_count: u32) -> ZeFilterNode {
    ZeFilterNode {
        op,
        attribute_id: 0,
        values: std::ptr::null(),
        value_count: 0,
        has_lower: 0,
        lower: value_u64(0),
        lower_inclusive: 0,
        has_upper: 0,
        upper: value_u64(0),
        upper_inclusive: 0,
        children_start,
        children_count,
    }
}

fn matching_scan_count(handle: ZeHandle, filter: &ZeFilter) -> usize {
    let request = ZeScanRequest {
        abi_size: size_of::<ZeScanRequest>() as u32,
        abi_reserved: 0,
        cursor_generation: 0,
        cursor_segment_id: [0; 16],
        cursor_next_row: 0,
        cursor_phase: 0,
        limit: 16,
        order: 0,
        include_vector: 0,
        include_text: 0,
        include_metadata: 0,
        include_attributes: 0,
        has_timestamp_range: 0,
        start_ts: 0,
        end_ts: 0,
        filter,
        cancel_token: 0,
        deadline_ns: 0,
    };
    let mut result: ZeScanResult = common::sized_zeroed();
    assert_eq!(ze_scan(handle, &request, &mut result), ZeErrorCode::ZeOk);
    let count = result.document_count;
    assert_eq!(result.has_more, 0);
    assert_eq!(ze_scan_result_free(&mut result), ZeErrorCode::ZeOk);
    count
}

fn assert_count_matches_scan(handle: ZeHandle, nodes: &[ZeFilterNode], root: u32) {
    let filter = ZeFilter {
        abi_size: size_of::<ZeFilter>() as u32,
        abi_reserved: 0,
        nodes: nodes.as_ptr(),
        node_count: nodes.len(),
        root,
    };
    let request = ZeCountRequest {
        filter: &filter,
        ..count_request()
    };
    assert_eq!(
        count(handle, &request).count as usize,
        matching_scan_count(handle, &filter)
    );
}

fn search_hits(handle: ZeHandle, request: &ZeSearchRequest) -> (ZeSearchResult, Vec<(u64, u32)>) {
    let mut result: ZeSearchResult = common::sized_zeroed();
    assert_eq!(ze_search(handle, request, &mut result), ZeErrorCode::ZeOk);
    let hits = unsafe { std::slice::from_raw_parts(result.hits, result.hit_count) }
        .iter()
        .map(|hit| (hit.doc_id.low, hit.score.to_bits()))
        .collect();
    (result, hits)
}

#[test]
fn count_without_filter_matches_live_rows_across_store_shapes() {
    for (sealed_rows, active_rows, expected) in [(0, 3, 3), (3, 0, 3), (2, 3, 5)] {
        let mut store = common::TestStore::new();
        if sealed_rows != 0 {
            assert_eq!(
                common::ingest_rows(store.handle, sealed_rows, 1),
                ZeErrorCode::ZeOk
            );
            seal(store.handle);
        }
        if active_rows != 0 {
            let first_id = sealed_rows + 1;
            let vector = [1.0_f32];
            let documents = (0..active_rows)
                .map(|offset| ZeIngestDocument {
                    abi_size: size_of::<ZeIngestDocument>() as u32,
                    abi_reserved: 0,
                    doc_id: ZeDocId {
                        high: 0,
                        low: (first_id + offset) as u64,
                    },
                    revision: 1,
                    timestamp: (first_id + offset) as i64,
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
            assert_eq!(
                ze_ingest(store.handle, &request, &mut report),
                ZeErrorCode::ZeOk
            );
        }
        let result = count(store.handle, &count_request());
        assert_eq!(result.count, expected);
        assert_eq!(result.generation, scan_generation(store.handle));
        assert_eq!(store.close(), ZeErrorCode::ZeOk);
    }
}

#[test]
fn count_filter_operators_match_scan_page_cardinality() {
    let root = tempfile::tempdir().expect("namespace root");
    let handle = open_filter_namespace(root.path());
    upsert_value(handle, 1, 10, Some(7));
    upsert_value(handle, 2, 20, Some(8));
    upsert_value(handle, 3, 30, None);
    let seven = [value_u64(7)];
    let seven_eight = [value_u64(7), value_u64(8)];

    for nodes in [
        vec![leaf(1, &seven)],
        vec![leaf(2, &seven)],
        vec![leaf(3, &seven_eight)],
        vec![leaf(4, &seven)],
        vec![leaf(6, &[])],
        vec![leaf(7, &[])],
        vec![logical(8, 1, 2), leaf(1, &seven), leaf(6, &[])],
        vec![logical(9, 1, 2), leaf(1, &seven), leaf(7, &[])],
        vec![logical(10, 1, 1), leaf(1, &seven)],
    ] {
        assert_count_matches_scan(handle, &nodes, 0);
    }
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
    assert_count_matches_scan(handle, &[range], 0);
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}

#[test]
fn count_timestamp_range_is_half_open_and_excludes_tombstones() {
    let mut store = common::TestStore::new();
    let vector = [1.0_f32];
    let documents = [10_i64, 20, 30, 40]
        .into_iter()
        .enumerate()
        .map(|(index, timestamp)| ZeIngestDocument {
            abi_size: size_of::<ZeIngestDocument>() as u32,
            abi_reserved: 0,
            doc_id: ZeDocId {
                high: 0,
                low: index as u64 + 1,
            },
            revision: 1,
            timestamp,
            vector: vector.as_ptr(),
            vector_len: 1,
            metadata: std::ptr::null(),
            metadata_len: 0,
            text: std::ptr::null(),
            text_len: 0,
        })
        .collect::<Vec<_>>();
    let ingest = ZeIngestRequest {
        abi_size: size_of::<ZeIngestRequest>() as u32,
        abi_reserved: 0,
        documents: documents.as_ptr(),
        document_count: 2,
        dimension: 1,
    };
    let mut mutation: ZeMutationReport = common::sized_zeroed();
    assert_eq!(
        ze_ingest(store.handle, &ingest, &mut mutation),
        ZeErrorCode::ZeOk
    );
    seal(store.handle);
    let active = ZeIngestRequest {
        documents: unsafe { documents.as_ptr().add(2) },
        ..ingest
    };
    assert_eq!(
        ze_ingest(store.handle, &active, &mut mutation),
        ZeErrorCode::ZeOk
    );
    let deleted = [ZeDocId { high: 0, low: 2 }, ZeDocId { high: 0, low: 4 }];
    let delete = ZeDeleteRequest {
        abi_size: size_of::<ZeDeleteRequest>() as u32,
        abi_reserved: 0,
        doc_ids: deleted.as_ptr(),
        doc_id_count: deleted.len(),
    };
    assert_eq!(
        ze_delete(store.handle, &delete, &mut mutation),
        ZeErrorCode::ZeOk
    );
    let request = ZeCountRequest {
        has_timestamp_range: 1,
        start_ts: 20,
        end_ts: 40,
        ..count_request()
    };
    assert_eq!(count(store.handle, &request).count, 1);
    assert_eq!(store.close(), ZeErrorCode::ZeOk);
}

#[test]
fn search_filtered_matches_post_filtered_exact_search_ids_and_score_bits() {
    let root = tempfile::tempdir().expect("namespace root");
    let handle = open_filter_namespace(root.path());
    upsert_value(handle, 1, 10, Some(7));
    upsert_value(handle, 2, 20, Some(8));
    upsert_value(handle, 3, 30, Some(7));
    upsert_value(handle, 4, 40, Some(8));
    let query = [0.0_f32];
    let search = ZeSearchRequest {
        k: 4,
        tier: 1,
        ..common::valid_search_request(&query)
    };
    let (mut unfiltered_result, unfiltered) = search_hits(handle, &search);
    let expected = unfiltered
        .into_iter()
        .filter(|(id, _)| id % 2 == 1)
        .collect::<Vec<_>>();

    let seven = [value_u64(7)];
    let nodes = [leaf(1, &seven)];
    let filter = ZeFilter {
        abi_size: size_of::<ZeFilter>() as u32,
        abi_reserved: 0,
        nodes: nodes.as_ptr(),
        node_count: nodes.len(),
        root: 0,
    };
    let request = ZeSearchFilteredRequest {
        abi_size: size_of::<ZeSearchFilteredRequest>() as u32,
        abi_reserved: 0,
        search,
        filter: &filter,
    };
    let mut filtered_result: ZeSearchResult = common::sized_zeroed();
    assert_eq!(
        ze_search_filtered(handle, &request, &mut filtered_result),
        ZeErrorCode::ZeOk
    );
    let actual =
        unsafe { std::slice::from_raw_parts(filtered_result.hits, filtered_result.hit_count) }
            .iter()
            .map(|hit| (hit.doc_id.low, hit.score.to_bits()))
            .collect::<Vec<_>>();
    assert_eq!(actual, expected);
    assert_eq!(filtered_result.generation, unfiltered_result.generation);
    assert_eq!(
        ze_search_result_free(&mut unfiltered_result),
        ZeErrorCode::ZeOk
    );
    assert_eq!(
        ze_search_result_free(&mut filtered_result),
        ZeErrorCode::ZeOk
    );
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}

#[test]
fn search_filtered_always_true_counters_match_unfiltered_search() {
    let root = tempfile::tempdir().expect("namespace root");
    let handle = open_filter_namespace(root.path());
    upsert_value(handle, 1, 10, Some(7));
    upsert_value(handle, 2, 20, Some(8));
    seal(handle);
    upsert_value(handle, 3, 30, Some(7));
    let query = [0.0_f32];
    let search = ZeSearchRequest {
        k: 3,
        tier: 1,
        ..common::valid_search_request(&query)
    };
    let (mut unfiltered, unfiltered_hits) = search_hits(handle, &search);
    let nodes = [logical(8, 0, 0)];
    let filter = ZeFilter {
        abi_size: size_of::<ZeFilter>() as u32,
        abi_reserved: 0,
        nodes: nodes.as_ptr(),
        node_count: nodes.len(),
        root: 0,
    };
    let request = ZeSearchFilteredRequest {
        abi_size: size_of::<ZeSearchFilteredRequest>() as u32,
        abi_reserved: 0,
        search,
        filter: &filter,
    };
    let mut filtered: ZeSearchResult = common::sized_zeroed();
    assert_eq!(
        ze_search_filtered(handle, &request, &mut filtered),
        ZeErrorCode::ZeOk
    );
    let filtered_hits = unsafe { std::slice::from_raw_parts(filtered.hits, filtered.hit_count) }
        .iter()
        .map(|hit| (hit.doc_id.low, hit.score.to_bits()))
        .collect::<Vec<_>>();
    assert_eq!(filtered_hits, unfiltered_hits);
    assert_eq!(filtered.generation, unfiltered.generation);
    assert_eq!(filtered.dims_touched, unfiltered.dims_touched);
    assert_eq!(filtered.bytes_read, unfiltered.bytes_read);
    assert_eq!(filtered.threads_used, unfiltered.threads_used);
    assert_eq!(
        filtered.graph_segments_traversed,
        unfiltered.graph_segments_traversed
    );
    assert_eq!(filtered.graph_validations, unfiltered.graph_validations);
    assert_eq!(
        filtered.graph_entry_seed_discoveries,
        unfiltered.graph_entry_seed_discoveries
    );
    assert_eq!(
        filtered.graph_visited_epoch_clears,
        unfiltered.graph_visited_epoch_clears
    );
    assert_eq!(
        filtered.graph_candidates_scored,
        unfiltered.graph_candidates_scored
    );
    assert_eq!(
        filtered.graph_candidates_rescored,
        unfiltered.graph_candidates_rescored
    );
    assert_eq!(
        filtered.graph_segments_pruned_by_bound,
        unfiltered.graph_segments_pruned_by_bound
    );
    assert_eq!(ze_search_result_free(&mut unfiltered), ZeErrorCode::ZeOk);
    assert_eq!(ze_search_result_free(&mut filtered), ZeErrorCode::ZeOk);
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}

#[test]
fn search_filtered_rejects_a_null_filter() {
    let mut store = common::TestStore::new();
    assert_eq!(common::ingest_rows(store.handle, 1, 1), ZeErrorCode::ZeOk);
    let query = [0.0_f32];
    let request = ZeSearchFilteredRequest {
        abi_size: size_of::<ZeSearchFilteredRequest>() as u32,
        abi_reserved: 0,
        search: common::valid_search_request(&query),
        filter: std::ptr::null(),
    };
    let mut result: ZeSearchResult = common::sized_zeroed();
    assert_eq!(
        ze_search_filtered(store.handle, &request, &mut result),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(store.close(), ZeErrorCode::ZeOk);
}

#[test]
fn malformed_filter_is_typed_invalid_for_count_and_filtered_search() {
    let mut store = common::TestStore::new();
    assert_eq!(common::ingest_rows(store.handle, 1, 1), ZeErrorCode::ZeOk);
    let nodes = [logical(8, 1, 1)];
    let filter = ZeFilter {
        abi_size: size_of::<ZeFilter>() as u32,
        abi_reserved: 0,
        nodes: nodes.as_ptr(),
        node_count: nodes.len(),
        root: 0,
    };
    let count_request = ZeCountRequest {
        filter: &filter,
        ..count_request()
    };
    let mut count_result: ZeCountResult = common::sized_zeroed();
    assert_eq!(
        ze_count(store.handle, &count_request, &mut count_result),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let query = [0.0_f32];
    let search_request = ZeSearchFilteredRequest {
        abi_size: size_of::<ZeSearchFilteredRequest>() as u32,
        abi_reserved: 0,
        search: common::valid_search_request(&query),
        filter: &filter,
    };
    let mut search_result: ZeSearchResult = common::sized_zeroed();
    assert_eq!(
        ze_search_filtered(store.handle, &search_request, &mut search_result),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(store.close(), ZeErrorCode::ZeOk);
}

#[test]
fn count_and_filtered_search_validate_pointers_and_abi_sizes() {
    let mut store = common::TestStore::new();
    assert_eq!(common::ingest_rows(store.handle, 1, 1), ZeErrorCode::ZeOk);
    let count_request = count_request();
    let mut count_result: ZeCountResult = common::sized_zeroed();
    assert_eq!(
        ze_count(store.handle, std::ptr::null(), &mut count_result),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        ze_count(store.handle, &count_request, std::ptr::null_mut()),
        ZeErrorCode::ZeErrInvalidArgument
    );
    for abi_size in [0, size_of::<ZeCountRequest>() as u32 - 1] {
        let request = ZeCountRequest {
            abi_size,
            ..count_request
        };
        assert_eq!(
            ze_count(store.handle, &request, &mut count_result),
            ZeErrorCode::ZeErrInvalidArgument
        );
    }

    let nodes = [logical(8, 0, 0)];
    let filter = ZeFilter {
        abi_size: size_of::<ZeFilter>() as u32,
        abi_reserved: 0,
        nodes: nodes.as_ptr(),
        node_count: nodes.len(),
        root: 0,
    };
    let query = [0.0_f32];
    let search_request = ZeSearchFilteredRequest {
        abi_size: size_of::<ZeSearchFilteredRequest>() as u32,
        abi_reserved: 0,
        search: common::valid_search_request(&query),
        filter: &filter,
    };
    let mut search_result: ZeSearchResult = common::sized_zeroed();
    assert_eq!(
        ze_search_filtered(store.handle, std::ptr::null(), &mut search_result),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        ze_search_filtered(store.handle, &search_request, std::ptr::null_mut()),
        ZeErrorCode::ZeErrInvalidArgument
    );
    for abi_size in [0, size_of::<ZeSearchFilteredRequest>() as u32 - 1] {
        let request = ZeSearchFilteredRequest {
            abi_size,
            ..search_request
        };
        assert_eq!(
            ze_search_filtered(store.handle, &request, &mut search_result),
            ZeErrorCode::ZeErrInvalidArgument
        );
    }
    assert_eq!(store.close(), ZeErrorCode::ZeOk);
}

#[test]
fn record_only_filtered_search_is_rejected_and_lexical_query_still_works() {
    let root = tempfile::tempdir().expect("namespace root");
    let handle = open_namespace(root.path(), b"records", &[], 0);
    let text = b"needle haystack";
    upsert_record_text(handle, text);

    let nodes = [logical(8, 0, 0)];
    let filter = ZeFilter {
        abi_size: size_of::<ZeFilter>() as u32,
        abi_reserved: 0,
        nodes: nodes.as_ptr(),
        node_count: nodes.len(),
        root: 0,
    };
    let vector = [1.0_f32];
    let filtered = ZeSearchFilteredRequest {
        abi_size: size_of::<ZeSearchFilteredRequest>() as u32,
        abi_reserved: 0,
        search: common::valid_search_request(&vector),
        filter: &filter,
    };
    let mut search_result: ZeSearchResult = common::sized_zeroed();
    assert_eq!(
        ze_search_filtered(handle, &filtered, &mut search_result),
        ZeErrorCode::ZeErrNoVectorSpace
    );

    let query_text = b"needle";
    let query = ZeQueryRequest {
        text: query_text.as_ptr(),
        text_len: query_text.len(),
        ..common::valid_query_request(&[])
    };
    let mut query_result: ZeQueryResult = common::sized_zeroed();
    assert_eq!(
        ze_query(handle, &query, &mut query_result),
        ZeErrorCode::ZeOk
    );
    assert_eq!(query_result.mode, 1);
    assert_eq!(query_result.hit_count, 1);
    let hit = unsafe { &*query_result.hits };
    assert_eq!(hit.doc_id.low, 1);
    assert_eq!(ze_query_result_free(&mut query_result), ZeErrorCode::ZeOk);
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}
