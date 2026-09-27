mod common;
use common::graph::*;
use zeppelin_embed_ffi::*;

#[test]
fn graph_open_creates_reopens_and_closes_a_store() {
    let mut store = GraphTestStore::create();
    assert_eq!(store.close(), ZeErrorCode::ZeOk);
    for mode in [MODE_READ_WRITE, MODE_READ_ONLY] {
        let (code, handle) = graph_open(&store.path, mode);
        assert_eq!(code, ZeErrorCode::ZeOk);
        assert_eq!(ze_graph_close(handle), ZeErrorCode::ZeOk);
    }
}
#[test]
fn graph_open_refuses_a_legacy_store_directory_with_store_kind() {
    let mut store = common::TestStore::new();
    assert_eq!(store.close(), ZeErrorCode::ZeOk);
    assert_eq!(
        graph_open(&store.path, MODE_READ_WRITE).0,
        ZeErrorCode::ZeErrStoreKind
    );
    assert!(last_error(0).contains(store.path.to_str().unwrap()));
}
#[test]
fn graph_open_rejects_each_malformed_request_before_touching_the_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("untouched");
    let bytes = path.to_str().unwrap().as_bytes();
    for case in 0..11 {
        let mut request = open_request(bytes, MODE_CREATE);
        let mut handle = ZeGraphHandle { token: 0 };
        let control: ZeGraphControl = common::sized_zeroed();
        match case {
            0 => {
                request.path = ZeGraphBytes {
                    data: std::ptr::null(),
                    count: 0,
                }
            }
            1 => {
                request.path = ZeGraphBytes {
                    data: b"a\0b".as_ptr(),
                    count: 3,
                }
            }
            2 => {
                request.path = ZeGraphBytes {
                    data: b"\xff".as_ptr(),
                    count: 1,
                }
            }
            3 => request.tokenizer_profile = 1,
            4 => request.max_resident_bytes = 0,
            5 => request.max_resident_bytes += 1,
            6 => request.mode = 3,
            7 => request.abi_size += 8,
            8 => request.abi_reserved = 1,
            9 => {}
            10 => request.control = &control,
            _ => unreachable!(),
        }
        let out = if case == 9 {
            std::ptr::null_mut()
        } else {
            &mut handle
        };
        let expected = if case == 10 {
            ZeErrorCode::ZeErrUnsupported
        } else {
            ZeErrorCode::ZeErrInvalidArgument
        };
        assert_eq!(ze_graph_open(&request, out), expected, "case {case}");
        assert!(!path.exists(), "case {case} touched disk");
    }
}
#[test]
fn graph_handles_are_distinct_from_legacy_and_text_handles() {
    let graph = GraphTestStore::create();
    let legacy = common::TestStore::new();
    assert_eq!(
        ze_close(graph.handle.token),
        ZeErrorCode::ZeErrInvalidHandle
    );
    for token in [legacy.handle, 0, graph.handle.token | (1 << 31)] {
        assert_eq!(
            ze_graph_close(ZeGraphHandle { token }),
            ZeErrorCode::ZeErrInvalidHandle
        );
    }
}
#[test]
fn graph_close_twice_and_a_stale_generation_return_typed_errors() {
    let mut store = GraphTestStore::create();
    let old = store.handle;
    assert_eq!(store.close(), ZeErrorCode::ZeOk);
    assert_eq!(ze_graph_close(old), ZeErrorCode::ZeErrClosed);
    let (code, new) = graph_open(&store.path, MODE_READ_WRITE);
    assert_eq!(code, ZeErrorCode::ZeOk);
    assert_ne!(old.token, new.token);
    assert_eq!(ze_graph_close(old), ZeErrorCode::ZeErrClosed);
    assert_eq!(ze_graph_close(new), ZeErrorCode::ZeOk);
}
#[test]
fn graph_last_error_message_routes_by_graph_token() {
    let graph = GraphTestStore::create();
    let mut legacy = common::TestStore::new();
    assert_eq!(legacy.close(), ZeErrorCode::ZeOk);
    assert_eq!(
        graph_open(&legacy.path, MODE_READ_WRITE).0,
        ZeErrorCode::ZeErrStoreKind
    );
    let global = last_error(0);
    // A wrong-kind close fails while the graph token still identifies a live slot.
    assert_eq!(
        ze_close(graph.handle.token),
        ZeErrorCode::ZeErrInvalidHandle
    );
    assert!(!last_error(graph.handle.token).is_empty());
    assert_eq!(last_error(0), global);
}

fn document_batch() -> (PoolBuilder, Vec<ZeGraphBatchItem>) {
    let mut b = PoolBuilder::new();
    let ns = b.text("docs");
    let alpha = b.text("alpha");
    let beta = b.text("beta");
    let v = b.string_value("alpha");
    let p = b.property("title", v);
    let a = b.node_image(&["Doc"], p..p + 1, Some("hello"));
    let z = b.node_image(&[], 0..0, None);
    let edges = b.text("edges");
    let key = b.text("alpha-beta");
    let r = b.relationship_image("LINKS", 0..0);
    (
        b,
        vec![
            create_node_item(ns, alpha, 1, a),
            create_node_item(ns, beta, 1, z),
            create_rel_item(edges, key, 1, r, local_endpoint(0), local_endpoint(1)),
        ],
    )
}
fn apply(
    handle: ZeGraphHandle,
    b: &PoolBuilder,
    items: &[ZeGraphBatchItem],
) -> (ZeErrorCode, ZeGraphResponse) {
    let mut out = empty_response();
    let code = ze_graph_apply(handle, &batch_request(items, &b.pool()), &mut out);
    (code, out)
}
fn free(r: &mut ZeGraphResponse) {
    assert_eq!(ze_graph_response_free(r), ZeErrorCode::ZeOk);
}
fn not_committed(r: &ZeGraphResponse) {
    assert_eq!(r.disposition, 1);
    assert_eq!(r.has_changed_generation, 0);
    assert_eq!(r.owner_token, 0);
}
#[test]
fn graph_apply_commits_a_document_node_and_its_edge_atomically() {
    let s = GraphTestStore::create();
    let (b, items) = document_batch();
    let (code, mut r) = apply(s.handle, &b, &items);
    assert_eq!(code, ZeErrorCode::ZeOk);
    assert_eq!(r.disposition, 2);
    assert_eq!(r.has_changed_generation, 1);
    assert_eq!(r.changed_generation, 1);
    assert_eq!(r.receipt_count, 3);
    assert_ne!(r.owner_token, 0);
    for (i, receipt) in receipts(&r).iter().enumerate() {
        assert_eq!(receipt.item, i as u32);
        assert_eq!(receipt.entity_kind, u32::from(i == 2));
        assert_eq!(receipt.generation, 1);
        if i == 2 {
            assert_ne!(receipt.relationship, ZeRelId::default());
        } else {
            assert_ne!(receipt.node, ZeNodeId::default());
        }
    }
    free(&mut r);
    assert_eq!(r.owner_token, 0);
    assert_eq!(r.disposition, 0);
}
#[test]
fn graph_apply_replays_an_exact_retry_and_reports_replayed() {
    let s = GraphTestStore::create();
    let (b, items) = document_batch();
    let (code, mut first) = apply(s.handle, &b, &items);
    assert_eq!(code, ZeErrorCode::ZeOk);
    let (code, mut second) = apply(s.handle, &b, &items);
    assert_eq!(code, ZeErrorCode::ZeOk);
    assert_eq!(second.disposition, 3);
    assert_eq!(second.has_changed_generation, 0);
    for (a, b) in receipts(&first).iter().zip(receipts(&second)) {
        assert_eq!(a.node, b.node);
        assert_eq!(a.relationship, b.relationship);
    }
    free(&mut first);
    free(&mut second);
}
#[test]
fn graph_apply_refuses_a_malformed_item_with_no_effect() {
    let s = GraphTestStore::create();
    let (mut b, mut items) = document_batch();
    items[1].key.start = u32::MAX;
    let (code, r) = apply(s.handle, &b, &items);
    assert_eq!(code, ZeErrorCode::ZeErrInvalidArgument);
    not_committed(&r);
    let fresh = single_node(&mut b, "different");
    let (code, mut r) = apply(s.handle, &b, &[fresh]);
    assert_eq!(code, ZeErrorCode::ZeOk);
    assert_eq!(r.changed_generation, 1);
    free(&mut r);
}
#[test]
fn graph_apply_reports_a_constraint_refusal_as_not_committed() {
    let s = GraphTestStore::create();
    let mut b = PoolBuilder::new();
    let mut item = single_node(&mut b, "alpha");
    let (code, mut r) = apply(s.handle, &b, &[item]);
    assert_eq!(code, ZeErrorCode::ZeOk);
    free(&mut r);
    item.operation = 1;
    item.revision = 2;
    item.expected_node = ZeNodeId { high: 0, low: 1234 };
    let (code, r) = apply(s.handle, &b, &[item]);
    assert!(matches!(
        code,
        ZeErrorCode::ZeErrIncarnationConflict | ZeErrorCode::ZeErrKeyConflict
    ));
    not_committed(&r);
}
#[test]
fn graph_apply_on_a_read_only_store_is_access_mode() {
    let mut s = GraphTestStore::create();
    assert_eq!(s.close(), ZeErrorCode::ZeOk);
    let (code, h) = graph_open(&s.path, MODE_READ_ONLY);
    assert_eq!(code, ZeErrorCode::ZeOk);
    let mut b = PoolBuilder::new();
    let item = single_node(&mut b, "alpha");
    let (code, r) = apply(h, &b, &[item]);
    assert_eq!(code, ZeErrorCode::ZeErrAccessMode);
    not_committed(&r);
    assert_eq!(ze_graph_close(h), ZeErrorCode::ZeOk);
}
#[test]
fn graph_apply_rejects_bad_response_descriptors_before_any_work() {
    let s = GraphTestStore::create();
    let mut b = PoolBuilder::new();
    let items = [single_node(&mut b, "alpha")];
    let pool = b.pool();
    let request = batch_request(&items, &pool);
    assert_eq!(
        ze_graph_apply(s.handle, &request, std::ptr::null_mut()),
        ZeErrorCode::ZeErrInvalidArgument
    );
    for reserved in [false, true] {
        let mut r = empty_response();
        if reserved {
            r.abi_reserved = 1;
        } else {
            r.abi_size += 8;
        }
        assert_eq!(
            ze_graph_apply(s.handle, &request, &mut r),
            ZeErrorCode::ZeErrInvalidArgument
        );
    }
    let (code, mut r) = apply(s.handle, &b, &items);
    assert_eq!(code, ZeErrorCode::ZeOk);
    assert_eq!(r.changed_generation, 1);
    free(&mut r);
}
#[test]
fn graph_apply_with_an_invalid_handle_never_touches_a_store() {
    let mut s = GraphTestStore::create();
    let closed = s.handle;
    assert_eq!(s.close(), ZeErrorCode::ZeOk);
    let legacy = common::TestStore::new();
    let mut b = PoolBuilder::new();
    let items = [single_node(&mut b, "alpha")];
    for (token, expected) in [
        (0, ZeErrorCode::ZeErrInvalidHandle),
        (legacy.handle, ZeErrorCode::ZeErrInvalidHandle),
        (closed.token, ZeErrorCode::ZeErrClosed),
    ] {
        let (code, r) = apply(ZeGraphHandle { token }, &b, &items);
        assert_eq!(code, expected);
        not_committed(&r);
    }
}
#[test]
fn graph_response_free_accepts_empty_and_rejects_forged_descriptors() {
    free(&mut empty_response());
    let mut error = empty_response();
    error.disposition = 1;
    free(&mut error);
    let s = GraphTestStore::create();
    let (b, items) = document_batch();
    let (code, mut r) = apply(s.handle, &b, &items);
    assert_eq!(code, ZeErrorCode::ZeOk);
    let mut forged = r;
    forged.owner_token += 1;
    assert_eq!(
        ze_graph_response_free(&mut forged),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let mut forged = r;
    forged.receipt_count += 1;
    assert_eq!(
        ze_graph_response_free(&mut forged),
        ZeErrorCode::ZeErrInvalidArgument
    );
    free(&mut r);
    free(&mut r);
    r.abi_size += 8;
    assert_eq!(
        ze_graph_response_free(&mut r),
        ZeErrorCode::ZeErrInvalidArgument
    );
}
#[test]
fn graph_apply_responses_survive_store_close() {
    let mut s = GraphTestStore::create();
    let (b, items) = document_batch();
    let (code, mut r) = apply(s.handle, &b, &items);
    assert_eq!(code, ZeErrorCode::ZeOk);
    let before: Vec<String> = receipts(&r).iter().map(|r| format!("{r:?}")).collect();
    assert_eq!(s.close(), ZeErrorCode::ZeOk);
    let after: Vec<String> = receipts(&r).iter().map(|r| format!("{r:?}")).collect();
    assert_eq!(before, after);
    free(&mut r);
}

#[test]
fn graph_cypher_write_then_read_returns_typed_rows() {
    let s = GraphTestStore::create();
    let mut r = cypher_ok(s.handle, "CREATE (:Doc {title: 'alpha'})");
    assert_eq!(
        (r.disposition, r.changed_generation, r.row_count),
        (2, 1, 0)
    );
    assert_eq!(ze_graph_response_free(&mut r), ZeErrorCode::ZeOk);
    let mut r = cypher_ok(s.handle, "MATCH (n:Doc) RETURN n.title AS title");
    assert_eq!(
        (r.disposition, r.has_admitted_generation, r.row_count),
        (0, 1, 1)
    );
    assert_eq!(column_names(&r), ["title"]);
    assert_eq!(rows(&r)[0][0].tag, 4);
    assert_eq!(string_of(&r, &rows(&r)[0][0]), "alpha");
    assert_eq!(ze_graph_response_free(&mut r), ZeErrorCode::ZeOk);
}
#[test]
fn graph_cypher_reads_nodes_written_by_ze_graph_apply() {
    let s = GraphTestStore::create();
    let mut b = PoolBuilder::new();
    let ns = b.text("docs");
    let key = b.text("b");
    let v = b.string_value("beta");
    b.property("title", v);
    let image = b.node_image(&["Doc"], 0..1, None);
    let mut r = empty_response();
    assert_eq!(
        ze_graph_apply(
            s.handle,
            &batch_request(&[create_node_item(ns, key, 1, image)], &b.pool()),
            &mut r
        ),
        ZeErrorCode::ZeOk
    );
    ze_graph_response_free(&mut r);
    let mut r = cypher_ok(s.handle, "MATCH (n:Doc) RETURN n.title AS title");
    assert_eq!(string_of(&r, &rows(&r)[0][0]), "beta");
    ze_graph_response_free(&mut r);
}
#[test]
fn graph_cypher_binds_scalar_parameters() {
    let s = GraphTestStore::create();
    let mut r = cypher_ok(s.handle, "CREATE (:Doc {title:'alpha'})");
    ze_graph_response_free(&mut r);
    let mut b = PoolBuilder::new();
    let t = b.string_value("alpha");
    let k = b.i64_value(7);
    let params = [parameter(&mut b, "t", t), parameter(&mut b, "k", k)];
    assert_eq!(
        ze_graph_cypher(
            s.handle,
            &cypher_request(
                b"MATCH (n:Doc) WHERE n.title = $t RETURN $k AS k",
                &params,
                Some(&b.pool())
            ),
            &mut r
        ),
        ZeErrorCode::ZeOk
    );
    assert_eq!(r.row_count, 1);
    assert_eq!((rows(&r)[0][0].tag, rows(&r)[0][0].integer), (2, 7));
    ze_graph_response_free(&mut r);
}
#[test]
fn graph_cypher_compile_refusals_have_their_own_codes_and_run_nothing() {
    let s = GraphTestStore::create();
    for (text, code) in [
        ("CREATE (", ZeErrorCode::ZeErrQuerySyntax),
        ("MATCH (n) RETURN unknownVar", ZeErrorCode::ZeErrScope),
        (
            "MATCH (n) RETURN n UNION MATCH (m) RETURN m",
            ZeErrorCode::ZeErrQueryUnsupported,
        ),
    ] {
        let mut r = empty_response();
        assert_eq!(
            ze_graph_cypher(
                s.handle,
                &cypher_request(text.as_bytes(), &[], None),
                &mut r
            ),
            code
        );
        assert_eq!(r.disposition, 1);
        let mut r = cypher_ok(s.handle, "MATCH (n) RETURN n");
        assert_eq!(r.row_count, 0);
        ze_graph_response_free(&mut r);
    }
}
#[test]
fn graph_cypher_no_op_write_reports_no_op() {
    let s = GraphTestStore::create();
    let mut r = cypher_ok(s.handle, "MATCH (n:Nope) SET n.x = 1");
    assert_eq!((r.disposition, r.has_changed_generation), (4, 0));
    ze_graph_response_free(&mut r);
}
#[test]
fn graph_cypher_on_a_read_only_store_reads_but_refuses_writes() {
    let mut s = GraphTestStore::create();
    assert_eq!(s.close(), ZeErrorCode::ZeOk);
    let (code, h) = graph_open(&s.path, MODE_READ_ONLY);
    assert_eq!(code, ZeErrorCode::ZeOk);
    let mut r = cypher_ok(h, "MATCH (n) RETURN n");
    ze_graph_response_free(&mut r);
    assert_eq!(
        ze_graph_cypher(h, &cypher_request(b"CREATE (:Doc)", &[], None), &mut r),
        ZeErrorCode::ZeErrAccessMode
    );
    assert_eq!(r.disposition, 1);
    ze_graph_close(h);
}
#[test]
fn graph_cypher_rejects_invalid_utf8_text_and_non_null_options() {
    let s = GraphTestStore::create();
    let mut r = empty_response();
    assert_eq!(
        ze_graph_cypher(s.handle, &cypher_request(&[255], &[], None), &mut r),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let options = common::sized_zeroed();
    let mut q = cypher_request(b"CREATE (:Doc)", &[], None);
    q.options = &options;
    assert_eq!(
        ze_graph_cypher(s.handle, &q, &mut r),
        ZeErrorCode::ZeErrUnsupported
    );
    assert_eq!(r.disposition, 1);
}
#[test]
fn graph_cypher_compile_limits_are_exact_and_tightening_only() {
    let s = GraphTestStore::create();
    for case in 0..3 {
        let mut limits: ZeGraphCompileLimits = common::sized_zeroed();
        limits.text_bytes = 65536;
        limits.tokens = 8192;
        limits.ast_nodes = 4096;
        limits.depth = 64;
        limits.parameters = 256;
        limits.columns = 256;
        limits.list_depth = 16;
        limits.path_hops = 16;
        match case {
            0 => limits.text_bytes = 8,
            1 => limits.path_hops = 17,
            _ => limits.abi_size += 8,
        }
        let mut q = cypher_request(b"MATCH (n) RETURN n", &[], None);
        q.compile_limits = &limits;
        let mut r = empty_response();
        assert_eq!(
            ze_graph_cypher(s.handle, &q, &mut r),
            if case == 0 {
                ZeErrorCode::ZeErrBudgetExceeded
            } else {
                ZeErrorCode::ZeErrInvalidArgument
            }
        );
        assert_eq!(r.disposition, 1);
    }
}

#[test]
fn graph_cypher_refuses_list_and_entity_parameters_before_any_effect() {
    let s = GraphTestStore::create();
    for case in 0..4 {
        let mut b = PoolBuilder::new();
        let v = b.tagged_value(if case == 0 {
            7
        } else if case == 1 {
            5
        } else {
            0
        });
        let p = parameter(&mut b, "p", v);
        let params = if case == 2 { vec![p, p] } else { vec![p] };
        let pool = b.pool();
        let q = cypher_request(
            b"CREATE (:Doc)",
            &params,
            if case == 3 { None } else { Some(&pool) },
        );
        let mut r = empty_response();
        let expected = match case {
            0 => ZeErrorCode::ZeErrUnsupported,
            1 | 2 => ZeErrorCode::ZeErrParameter,
            _ => ZeErrorCode::ZeErrInvalidArgument,
        };
        assert_eq!(ze_graph_cypher(s.handle, &q, &mut r), expected);
        assert_eq!(r.disposition, 1);
        let mut r = cypher_ok(s.handle, "MATCH (n) RETURN n");
        assert_eq!(r.row_count, 0);
        ze_graph_response_free(&mut r);
    }
}

#[test]
fn graph_cypher_result_row_limit_is_configurable_and_bounded() {
    let s = GraphTestStore::create();
    let mut r = cypher_ok(s.handle, "CREATE (:Doc), (:Doc)");
    ze_graph_response_free(&mut r);
    let q = cypher_request(b"MATCH (n:Doc) RETURN 7 AS k", &[], None);
    for (limit, code) in [
        (1, ZeErrorCode::ZeErrBudgetExceeded),
        (2, ZeErrorCode::ZeOk),
        (0, ZeErrorCode::ZeOk),
        (65536, ZeErrorCode::ZeOk),
        (65537, ZeErrorCode::ZeErrInvalidArgument),
    ] {
        assert_eq!(
            ze_graph_cypher_with_row_limit(s.handle, &q, limit, &mut r),
            code,
            "{}",
            last_error(s.handle.token)
        );
        if code == ZeErrorCode::ZeOk {
            assert_eq!(r.row_count, 2);
        }
        ze_graph_response_free(&mut r);
    }
}

#[test]
fn graph_cypher_result_row_limit_can_exceed_default() {
    let s = GraphTestStore::create();
    let mut b = PoolBuilder::new();
    let items: Vec<_> = (0..33)
        .map(|i| single_node(&mut b, &format!("n{i}")))
        .collect();
    let mut r = empty_response();
    for chunk in items.chunks(64) {
        assert_eq!(
            ze_graph_apply(s.handle, &batch_request(chunk, &b.pool()), &mut r),
            ZeErrorCode::ZeOk,
            "{}",
            last_error(s.handle.token)
        );
        ze_graph_response_free(&mut r);
    }
    let q = cypher_request(b"MATCH (a), (b) RETURN 7 AS k", &[], None);
    assert_eq!(
        ze_graph_cypher(s.handle, &q, &mut r),
        ZeErrorCode::ZeErrBudgetExceeded
    );
    assert_eq!(
        ze_graph_cypher_with_row_limit(s.handle, &q, 1089, &mut r),
        ZeErrorCode::ZeOk,
        "{}",
        last_error(s.handle.token)
    );
    assert_eq!(r.row_count, 1089);
    ze_graph_response_free(&mut r);
}
