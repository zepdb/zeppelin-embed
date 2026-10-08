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
