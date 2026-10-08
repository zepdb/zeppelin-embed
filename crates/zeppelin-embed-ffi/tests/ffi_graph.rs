#![cfg(feature = "graph-cypher")]
mod common;
use common::graph::{cypher_request, rows};
use zeppelin_embed_ffi::*;

#[test]
fn a_store_handle_runs_cypher() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = dir.path().to_str().unwrap().as_bytes();
    let open = ZeOpenRequest {
        abi_size: std::mem::size_of::<ZeOpenRequest>() as u32,
        abi_reserved: 0,
        path: bytes.as_ptr(),
        path_len: bytes.len(),
        access_mode: 0,
        durability_mode: 0,
        commit_tier: 1,
        reader_drain_timeout_ms: 250,
        max_resident_bytes: 256 << 20,
        max_temp_bytes: u64::MAX,
    };
    let fixture = common::EpochFixture::new(2);
    let epoch = fixture.request();
    let mut handle = 0;
    assert_eq!(
        ze_open_with_epoch(&open, &epoch, &mut handle),
        ZeErrorCode::ZeOk
    );
    assert_eq!(common::ingest_rows(handle, 1, 2), ZeErrorCode::ZeOk);
    let mut generation = common::sized_zeroed();
    assert_eq!(
        ze_store_enable_graph(handle, &mut generation),
        ZeErrorCode::ZeOk
    );
    let mut response = common::sized_zeroed();
    assert_eq!(
        ze_store_cypher(
            handle,
            &cypher_request(b"MATCH (n) RETURN n", &[], None),
            &mut response
        ),
        ZeErrorCode::ZeOk,
        "{}",
        common::graph::last_error(handle)
    );
    assert_eq!(response.row_count, 1);
    assert_eq!(rows(&response)[0][0].tag, 5);
    let node = unsafe { *response.pool.nodes };
    assert_eq!(node.id, ZeNodeId { high: 0, low: 1 });
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
    assert_eq!(ze_graph_response_free(&mut response), ZeErrorCode::ZeOk);
}

#[test]
fn store_graph_results_survive_close_and_free_without_growth() {
    use common::graph::*;
    let mut store = GraphTestStore::create();
    let mut builder = PoolBuilder::new();
    let item = single_node(&mut builder, "soak");
    let pool = builder.pool();
    let items = [item];
    let request = batch_request(&items, &pool);
    let mut response = empty_response();
    assert_eq!(
        ze_store_graph_apply(store.handle, &request, &mut response),
        ZeErrorCode::ZeOk
    );
    assert_eq!(ze_graph_response_free(&mut response), ZeErrorCode::ZeOk);
    let mut baseline = common::sized_zeroed();
    assert_eq!(
        ze_store_graph_resources(store.handle, &mut baseline),
        ZeErrorCode::ZeOk
    );
    for _ in 0..10_000 {
        assert_eq!(
            ze_store_graph_apply(store.handle, &request, &mut response),
            ZeErrorCode::ZeOk
        );
        assert_eq!(
            response.disposition,
            ZeGraphDisposition::ZeGraphDispositionReplayed as u32
        );
        assert_eq!(response.receipt_count, 1);
        assert_eq!(ze_graph_response_free(&mut response), ZeErrorCode::ZeOk);
        assert_eq!(response.owner_token, 0);
    }
    let mut after = common::sized_zeroed();
    assert_eq!(
        ze_store_graph_resources(store.handle, &mut after),
        ZeErrorCode::ZeOk
    );
    eprintln!(
        "ZE360_APPLY_FREE rounds=10000 baseline_engine_bytes={} final_engine_bytes={}",
        baseline.engine_bytes, after.engine_bytes
    );
    assert_eq!(after.engine_bytes, baseline.engine_bytes);
    assert_eq!(
        ze_store_graph_apply(store.handle, &request, &mut response),
        ZeErrorCode::ZeOk
    );
    assert_eq!(store.close(), ZeErrorCode::ZeOk);
    assert_eq!(unsafe { (*response.receipts).revision }, 1);
    assert_eq!(ze_graph_response_free(&mut response), ZeErrorCode::ZeOk);
    assert_eq!(ze_graph_response_free(&mut response), ZeErrorCode::ZeOk);
}

#[test]
fn relationship_creation_with_a_document_tower_reopens_as_a_store() {
    use common::graph::*;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let bytes = path.to_str().unwrap().as_bytes();
    let mut fixture = common::EpochFixture::new(2);
    fixture.query_prefix = fixture.document_prefix.clone();
    let epoch = fixture.request();
    let mut request = open_request(bytes, MODE_CREATE);
    request.document_tower = &epoch.embedding.document;
    let mut handle = 0;
    assert_eq!(
        ze_store_create_with_relationship_types(&request, std::ptr::null(), 0, &mut handle),
        ZeErrorCode::ZeOk
    );
    let mut builder = PoolBuilder::new();
    let item = single_node(&mut builder, "vector");
    builder.node_vector(item.image, &[1.0, 0.0]);
    let pool = builder.pool();
    let items = [item];
    let mut response = empty_response();
    assert_eq!(
        ze_store_graph_apply(handle, &batch_request(&items, &pool), &mut response),
        ZeErrorCode::ZeOk
    );
    assert_eq!(ze_graph_response_free(&mut response), ZeErrorCode::ZeOk);
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
    let open = ZeOpenRequest {
        abi_size: std::mem::size_of::<ZeOpenRequest>() as u32,
        abi_reserved: 0,
        path: bytes.as_ptr(),
        path_len: bytes.len(),
        access_mode: 0,
        durability_mode: 1,
        commit_tier: 2,
        reader_drain_timeout_ms: 250,
        max_resident_bytes: 256 << 20,
        max_temp_bytes: u64::MAX,
    };
    assert_eq!(
        ze_open_with_epoch(&open, &epoch, &mut handle),
        ZeErrorCode::ZeOk
    );
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}

#[test]
fn ze399_v2_document_node_is_searchable_visible_and_durable() {
    use common::graph::*;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let bytes = path.to_str().unwrap().as_bytes();
    let open = ZeOpenRequest {
        abi_size: std::mem::size_of::<ZeOpenRequest>() as u32,
        abi_reserved: 0,
        path: bytes.as_ptr(),
        path_len: bytes.len(),
        access_mode: 0,
        durability_mode: 0,
        commit_tier: 1,
        reader_drain_timeout_ms: 250,
        max_resident_bytes: 256 << 20,
        max_temp_bytes: u64::MAX,
    };
    let fixture = common::EpochFixture::new(2);
    let epoch = fixture.request();
    let mut handle = 0;
    assert_eq!(
        ze_open_with_epoch(&open, &epoch, &mut handle),
        ZeErrorCode::ZeOk
    );
    let mut enabled = common::sized_zeroed();
    assert_eq!(
        ze_store_enable_graph(handle, &mut enabled),
        ZeErrorCode::ZeOk
    );
    let mut builder = PoolBuilder::new();
    let ns = builder.text("docs");
    let key = builder.text("orchard");
    let image = builder.node_image(&[], 0..0, Some("orchard"));
    let vector = [1.0_f32, 0.0];
    builder.node_vector(image, &vector);
    let pool = builder.pool();
    let items = [create_node_item(ns, key, 1, image)];
    let graph = batch_request(&items, &pool);
    let mut document: ZeStoreGraphDocument = common::sized_zeroed();
    document.item_index = 0;
    document.has_id = 1;
    document.id = ZeDocId { high: 0, low: 91 };
    let mut request: ZeStoreGraphBatchRequestV2 = common::sized_zeroed();
    request.graph = &graph;
    request.documents = &document;
    request.document_count = 1;
    let mut applied = empty_response();
    assert_eq!(
        ze_store_graph_apply_v2(handle, &request, &mut applied),
        ZeErrorCode::ZeOk,
        "{}",
        last_error(handle)
    );
    assert_eq!(receipts(&applied)[0].node, ZeNodeId { high: 0, low: 91 });
    assert_eq!(applied.changed_generation, enabled.generation + 1);
    let query = common::valid_query_request(&vector);
    let mut result: ZeQueryResult = common::sized_zeroed();
    assert_eq!(
        ze_query(handle, &query, &mut result),
        ZeErrorCode::ZeOk,
        "{}",
        last_error(handle)
    );
    assert_eq!(result.hit_count, 1);
    assert_eq!(unsafe { (*result.hits).doc_id }, document.id);
    assert_eq!(ze_query_result_free(&mut result), ZeErrorCode::ZeOk);
    let mut response = empty_response();
    assert_eq!(
        ze_store_cypher(
            handle,
            &cypher_request(b"MATCH (n:Document) RETURN n", &[], None),
            &mut response
        ),
        ZeErrorCode::ZeOk,
        "{}",
        last_error(handle)
    );
    assert_eq!(response.row_count, 1);
    assert_eq!(
        unsafe { (*response.pool.nodes).id },
        ZeNodeId { high: 0, low: 91 }
    );
    assert_eq!(ze_graph_response_free(&mut response), ZeErrorCode::ZeOk);
    assert_eq!(ze_graph_response_free(&mut applied), ZeErrorCode::ZeOk);
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
    assert_eq!(
        ze_open_with_epoch(&open, &epoch, &mut handle),
        ZeErrorCode::ZeOk
    );
    assert_eq!(ze_query(handle, &query, &mut result), ZeErrorCode::ZeOk);
    assert_eq!(result.hit_count, 1);
    assert_eq!(ze_query_result_free(&mut result), ZeErrorCode::ZeOk);
    assert_eq!(
        ze_store_cypher(
            handle,
            &cypher_request(b"MATCH (n:Document) RETURN n", &[], None),
            &mut response
        ),
        ZeErrorCode::ZeOk
    );
    assert_eq!(response.row_count, 1);
    assert_eq!(ze_graph_response_free(&mut response), ZeErrorCode::ZeOk);
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}

#[test]
fn ze399_v2_combined_input_limit_refuses_before_any_write() {
    use common::graph::*;
    let store = GraphTestStore::create();
    let mut builder = PoolBuilder::new();
    let ns = builder.text("docs");
    let key = builder.text("large");
    let text = "a".repeat(2 * 1024 * 1024);
    let image = builder.node_image(&[], 0..0, Some(&text));
    let pool = builder.pool();
    let items = [create_node_item(ns, key, 1, image)];
    let graph = batch_request(&items, &pool);
    let metadata = vec![0_u8; 5 * 1024 * 1024];
    let mut document: ZeStoreGraphDocument = common::sized_zeroed();
    document.has_id = 1;
    document.id = ZeDocId { high: 0, low: 91 };
    document.metadata = metadata.as_ptr();
    document.metadata_len = metadata.len();
    let mut request: ZeStoreGraphBatchRequestV2 = common::sized_zeroed();
    request.graph = &graph;
    request.documents = &document;
    request.document_count = 1;
    let before = store_bytes(&store.path);
    let mut response = empty_response();
    assert_eq!(
        ze_store_graph_apply_v2(store.handle, &request, &mut response),
        ZeErrorCode::ZeErrInvalidArgument,
        "{}",
        last_error(store.handle)
    );
    assert_eq!(
        response.disposition,
        ZeGraphDisposition::ZeGraphDispositionNotCommitted as u32
    );
    assert_eq!(store_bytes(&store.path), before);
}

fn store_bytes(path: &std::path::Path) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    std::fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let bytes = std::fs::read(&path).unwrap();
            (path.file_name().unwrap().into(), bytes)
        })
        .collect()
}

#[test]
fn ze399_v2_missing_duplicate_and_malformed_associations_write_nothing() {
    use common::graph::*;
    let store = GraphTestStore::create();
    let mut builder = PoolBuilder::new();
    let first = single_node(&mut builder, "first");
    let second = single_node(&mut builder, "second");
    let pool = builder.pool();
    let items = [first, second];
    let graph = batch_request(&items, &pool);
    let mut first_doc: ZeStoreGraphDocument = common::sized_zeroed();
    first_doc.has_id = 1;
    first_doc.id = ZeDocId { high: 0, low: 91 };
    let mut second_doc = first_doc;
    second_doc.item_index = 1;
    let mut missing = first_doc;
    missing.has_id = 0;
    missing.id = ZeDocId { high: 0, low: 0 };
    let mut zero = first_doc;
    zero.id = missing.id;
    let mut invalid_index = first_doc;
    invalid_index.item_index = 2;
    let mut bad_flag = first_doc;
    bad_flag.has_id = 2;
    let mut bad_size = first_doc;
    bad_size.abi_size += 8;
    let mut reserved = first_doc;
    reserved.abi_reserved = 1;
    let mut duplicate_index = second_doc;
    duplicate_index.item_index = 0;
    duplicate_index.id.low = 92;
    for documents in [
        vec![missing],
        vec![zero],
        vec![first_doc, second_doc],
        vec![first_doc, duplicate_index],
        vec![invalid_index],
        vec![bad_flag],
        vec![bad_size],
        vec![reserved],
    ] {
        let mut request: ZeStoreGraphBatchRequestV2 = common::sized_zeroed();
        request.graph = &graph;
        request.documents = documents.as_ptr();
        request.document_count = documents.len();
        let before = store_bytes(&store.path);
        let mut response = empty_response();
        assert_eq!(
            ze_store_graph_apply_v2(store.handle, &request, &mut response),
            ZeErrorCode::ZeErrInvalidArgument,
            "{}",
            last_error(store.handle)
        );
        assert_eq!(
            response.disposition,
            ZeGraphDisposition::ZeGraphDispositionNotCommitted as u32
        );
        assert_eq!(response.receipt_count, 0);
        assert_eq!(store_bytes(&store.path), before);
        assert_eq!(ze_graph_response_free(&mut response), ZeErrorCode::ZeOk);
    }
    let mut request: ZeStoreGraphBatchRequestV2 = common::sized_zeroed();
    request.graph = &graph;
    request.documents = &first_doc;
    request.document_count = 1;
    let mut response = empty_response();
    assert_eq!(
        ze_store_graph_apply_v2(store.handle, &request, &mut response),
        ZeErrorCode::ZeOk,
        "{}",
        last_error(store.handle)
    );
    assert_eq!(ze_graph_response_free(&mut response), ZeErrorCode::ZeOk);
}

#[test]
fn ze399_v2_put_replaces_document_fields_and_keeps_the_node_id() {
    use common::graph::*;
    let root = tempfile::tempdir().unwrap();
    let bytes = root.path().to_str().unwrap().as_bytes();
    let definition = ZeAttributeDefinition {
        attribute_id: 1,
        name: b"rank".as_ptr(),
        name_len: 4,
        attribute_type: 1,
        nullable: 1,
    };
    let mut spec: ZeNamespaceSpec = common::sized_zeroed();
    spec.attributes = &definition;
    spec.attribute_count = 1;
    let mut open: ZeNamespaceOpenRequest = common::sized_zeroed();
    open.root = bytes.as_ptr();
    open.root_len = bytes.len();
    open.name = b"documents".as_ptr();
    open.name_len = 9;
    open.open = common::sized_zeroed();
    open.open.commit_tier = 1;
    open.open.reader_drain_timeout_ms = 250;
    open.open.max_resident_bytes = 256 << 20;
    open.open.max_temp_bytes = u64::MAX;
    open.spec = &spec;
    let mut handle = 0;
    assert_eq!(
        ze_namespace_open(&open, &mut handle),
        ZeErrorCode::ZeOk,
        "{}",
        last_error(handle)
    );
    let mut enabled = common::sized_zeroed();
    assert_eq!(
        ze_store_enable_graph(handle, &mut enabled),
        ZeErrorCode::ZeOk
    );
    let id = ZeDocId { high: 1, low: 91 };
    let attribute = ZeAttributeValue {
        attribute_id: 1,
        value_type: 1,
        u64_value: 7,
        i64_value: 0,
        f64_value: 0.0,
        bool_value: 0,
        string_value: std::ptr::null(),
        string_len: 0,
    };
    for revision in [1, 2] {
        let mut builder = PoolBuilder::new();
        let ns = builder.text("docs");
        let key = builder.text("associated");
        let text = if revision == 1 { "orchard" } else { "updated" };
        let image = builder.node_image(&[], 0..0, Some(text));
        let mut item = create_node_item(ns, key, revision, image);
        let mut document: ZeStoreGraphDocument = common::sized_zeroed();
        document.timestamp = revision as i64;
        if revision == 1 {
            document.has_id = 1;
            document.id = id;
            document.attributes = &attribute;
            document.attribute_count = 1;
            document.metadata = b"opaque".as_ptr();
            document.metadata_len = 6;
        } else {
            item.operation = ZeGraphBatchOperation::ZeGraphBatchPut as u32;
            item.expected_node = ZeNodeId {
                high: id.high,
                low: id.low,
            };
        }
        let pool = builder.pool();
        let items = [item];
        let graph = batch_request(&items, &pool);
        let mut request: ZeStoreGraphBatchRequestV2 = common::sized_zeroed();
        request.graph = &graph;
        request.documents = &document;
        request.document_count = 1;
        let mut response = empty_response();
        assert_eq!(
            ze_store_graph_apply_v2(handle, &request, &mut response),
            ZeErrorCode::ZeOk,
            "{}",
            last_error(handle)
        );
        assert_eq!(response.changed_generation, enabled.generation + revision);
        assert_eq!(
            receipts(&response)[0].node,
            ZeNodeId {
                high: id.high,
                low: id.low
            }
        );
        assert_eq!(ze_graph_response_free(&mut response), ZeErrorCode::ZeOk);
        let mut get: ZeGetRequest = common::sized_zeroed();
        get.ids = &id;
        get.id_count = 1;
        get.include_text = 1;
        get.include_metadata = 1;
        get.include_attributes = 1;
        let mut result: ZeGetResult = common::sized_zeroed();
        assert_eq!(
            ze_get(handle, &get, &mut result),
            ZeErrorCode::ZeOk,
            "{}",
            last_error(handle)
        );
        let stored = unsafe { *result.documents };
        assert_eq!(stored.revision, revision);
        assert_eq!(stored.timestamp, revision as i64);
        assert_eq!(
            unsafe { std::slice::from_raw_parts(stored.text, stored.text_len) },
            text.as_bytes()
        );
        if revision == 1 {
            assert_eq!(stored.attribute_count, 1);
            assert_eq!(unsafe { (*stored.attributes).u64_value }, 7);
            assert_eq!(
                unsafe { std::slice::from_raw_parts(stored.metadata, stored.metadata_len) },
                b"opaque"
            );
        } else {
            assert_eq!(stored.metadata_len, 0);
            assert_eq!(stored.attribute_count, 0);
        }
        assert_eq!(ze_get_result_free(&mut result), ZeErrorCode::ZeOk);
    }
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}
