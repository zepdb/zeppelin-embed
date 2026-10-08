mod common;
use common::graph::*;
use common::sized_zeroed;
use zeppelin_embed_ffi::*;

#[test]
fn ze241_get_preserves_missing_order_duplicates_and_optional_tags() {
    let mut store = GraphTestStore::create();
    let mut builder = PoolBuilder::new();
    let ns = builder.text("docs");
    let key = builder.text("empty");
    let image = builder.node_image(&[], 0..0, Some(""));
    let pool = builder.pool();
    let items = [create_node_item(ns, key, 1, image)];
    let mut applied = empty_response();
    assert_eq!(
        ze_graph_apply(store.handle, &batch_request(&items, &pool), &mut applied),
        ZeErrorCode::ZeOk
    );
    let id = receipts(&applied)[0].node;
    let ids = [
        id,
        ZeNodeId {
            high: u64::MAX,
            low: u64::MAX,
        },
        id,
    ];
    let mut request: ZeGraphGetNodesRequest = sized_zeroed();
    request.ids = ids.as_ptr();
    request.id_count = ids.len();
    request.include_text = 1;
    let mut response = empty_response();
    assert_eq!(
        ze_graph_get_nodes(store.handle, &request, &mut response),
        ZeErrorCode::ZeOk,
        "{}",
        last_error(store.handle.token)
    );
    let values = rows(&response);
    assert_eq!(
        values.iter().map(|r| r[0].tag).collect::<Vec<_>>(),
        [5, 0, 5]
    );
    assert_eq!(values[0][0].entity_index, values[2][0].entity_index);
    let nodes =
        unsafe { std::slice::from_raw_parts(response.pool.nodes, response.pool.node_count) };
    assert_eq!(
        (nodes[0].has_text, nodes[0].text.count, nodes[0].has_vector),
        (1, 0, 0)
    );
    assert_eq!(store.close(), ZeErrorCode::ZeOk);
    let (code, reopened) = graph_open(&store.path, MODE_READ_WRITE);
    assert_eq!(code, ZeErrorCode::ZeOk);
    assert_eq!(ze_graph_response_free(&mut applied), ZeErrorCode::ZeOk);
    assert_eq!(ze_graph_response_free(&mut response), ZeErrorCode::ZeOk);
    assert_eq!(ze_graph_close(reopened), ZeErrorCode::ZeOk);
    let request: ZeGraphGetRelsRequest = sized_zeroed();
    assert_eq!(
        ze_graph_get_relationships(reopened, &request, &mut response),
        ZeErrorCode::ZeErrClosed
    );
}

#[test]
fn ze241_query_reads_and_mutates_through_c() {
    let store = GraphTestStore::create();
    let mut pool: ZeGraphValuePool = sized_zeroed();
    let mut unit: ZeGraphOperator = sized_zeroed();
    unit.kind = 0;
    let mut eager: ZeGraphOperator = sized_zeroed();
    eager.kind = 5;
    eager.inputs = ZeGraphRange { start: 0, count: 1 };
    let mut mutate: ZeGraphOperator = sized_zeroed();
    mutate.kind = 6;
    mutate.inputs = ZeGraphRange { start: 1, count: 1 };
    mutate.mutations = ZeGraphRange { start: 0, count: 1 };
    let mut mutation: ZeGraphMutation = sized_zeroed();
    mutation.output = 42;
    let mut project: ZeGraphOperator = sized_zeroed();
    project.kind = 16;
    project.inputs = ZeGraphRange { start: 2, count: 1 };
    project.projections = ZeGraphRange { start: 0, count: 1 };
    let mut expression: ZeGraphExpression = sized_zeroed();
    expression.kind = 1;
    expression.value = 42;
    let mut projection: ZeGraphProjection = sized_zeroed();
    projection.slot = 42;
    let operators = [unit, eager, mutate, project];
    let inputs = [0, 1, 2];
    let mut plan: ZeGraphPlan = sized_zeroed();
    plan.root = 3;
    plan.expressions = &expression;
    plan.expression_count = 1;
    plan.projections = &projection;
    plan.projection_count = 1;
    plan.operators = operators.as_ptr();
    plan.operator_count = operators.len();
    plan.inputs = inputs.as_ptr();
    plan.input_count = inputs.len();
    plan.mutations = &mutation;
    plan.mutation_count = 1;
    plan.pool = &pool;
    let mut request: ZeGraphQueryRequest = sized_zeroed();
    request.plan = &plan;
    let mut response = empty_response();
    assert_eq!(
        ze_graph_query(store.handle, &request, &mut response),
        ZeErrorCode::ZeOk,
        "{}",
        last_error(store.handle.token)
    );
    assert_eq!(response.disposition, 2);
    assert_eq!(response.changed_generation, 2);
    assert_eq!(rows(&response)[0][0].tag, 5);
    let column = unsafe { &*response.columns };
    let bytes =
        unsafe { std::slice::from_raw_parts(response.pool.bytes, response.pool.byte_count) };
    assert_eq!(
        &bytes[column.name.start as usize..(column.name.start + column.name.count) as usize],
        b"slot_42"
    );
    assert_eq!(ze_graph_response_free(&mut response), ZeErrorCode::ZeOk);
    pool.abi_reserved = 1;
    plan.pool = &pool;
    request.plan = &plan;
    assert_eq!(
        ze_graph_query(store.handle, &request, &mut response),
        ZeErrorCode::ZeErrInvalidArgument
    );
}

#[test]
fn ze241_cypher_vector_parameter_and_options_execute() {
    let store = GraphTestStore::create();
    let mut values: [ZeGraphValue; 3] = [sized_zeroed(); 3];
    values[0].tag = 2;
    values[0].integer = 3;
    values[1].tag = 2;
    values[1].integer = 4;
    values[2].tag = 7;
    values[2].range = ZeGraphRange { start: 0, count: 2 };
    let children = [0, 1];
    let mut pool: ZeGraphValuePool = sized_zeroed();
    pool.values = values.as_ptr();
    pool.value_count = values.len();
    pool.children = children.as_ptr();
    pool.child_count = children.len();
    pool.bytes = b"v".as_ptr();
    pool.byte_count = 1;
    let mut parameter: ZeGraphParameterValue = sized_zeroed();
    parameter.name = ZeGraphRange { start: 0, count: 1 };
    parameter.value = 2;
    let options: ZeGraphQueryOptions = sized_zeroed();
    let mut request = cypher_request(
        b"RETURN size($v) AS length, $v[1] AS second",
        &[parameter],
        Some(&pool),
    );
    // Keep the honest binding buffer alive throughout the synchronous call.
    request.parameters = &parameter;
    request.options = &options;
    let mut response = empty_response();
    assert_eq!(
        ze_graph_cypher(store.handle, &request, &mut response),
        ZeErrorCode::ZeOk,
        "{}",
        last_error(store.handle.token)
    );
    let result = rows(&response);
    assert_eq!((result[0][0].integer, result[0][1].integer), (2, 4));
    assert_eq!(ze_graph_response_free(&mut response), ZeErrorCode::ZeOk);
}

#[cfg(feature = "graph-result-test-support")]
#[test]
fn ze241_response_capacity_failure_commits_nothing() {
    use zeppelin_embed_ffi::graph_result::test_support::AllocationFaultScope;
    for entry in 0..2 {
        let store = GraphTestStore::create();
        let mut response = empty_response();
        let fault = AllocationFaultScope::arm(1);
        let code = if entry == 0 {
            ze_graph_cypher(
                store.handle,
                &cypher_request(b"CREATE (:BeforeCommit) RETURN 1 AS x", &[], None),
                &mut response,
            )
        } else {
            let mut builder = PoolBuilder::new();
            let item = single_node(&mut builder, "before-commit");
            let pool = builder.pool();
            ze_graph_apply(store.handle, &batch_request(&[item], &pool), &mut response)
        };
        assert_eq!(fault.receipt().fires, 1);
        drop(fault);
        assert_eq!(code, ZeErrorCode::ZeErrOutOfMemory);
        assert_eq!(
            response.disposition, 1,
            "entry {entry} committed before result capacity was reserved"
        );
        let mut read = cypher_ok(store.handle, "MATCH (n) RETURN count(n) AS count");
        assert_eq!(rows(&read)[0][0].integer, 0);
        assert_eq!(ze_graph_response_free(&mut read), ZeErrorCode::ZeOk);
    }
}

#[test]
fn ze241_structured_search_preserves_intent_and_reports() {
    let store = GraphTestStore::create();
    let mut builder = PoolBuilder::new();
    let item = single_node(&mut builder, "search");
    let pool = builder.pool();
    let mut applied = empty_response();
    assert_eq!(
        ze_graph_apply(store.handle, &batch_request(&[item], &pool), &mut applied),
        ZeErrorCode::ZeOk
    );
    assert_eq!(ze_graph_response_free(&mut applied), ZeErrorCode::ZeOk);
    let mut values: [ZeGraphValue; 2] = [sized_zeroed(); 2];
    values[0].tag = 4;
    values[0].range = ZeGraphRange { start: 0, count: 6 };
    values[1].tag = 2;
    values[1].integer = 1;
    let mut pool: ZeGraphValuePool = sized_zeroed();
    pool.bytes = b"search".as_ptr();
    pool.byte_count = 6;
    pool.values = values.as_ptr();
    pool.value_count = 2;
    let mut expressions: [ZeGraphExpression; 2] = [sized_zeroed(); 2];
    expressions[1].value = 1;
    let mut search: ZeGraphSearch = sized_zeroed();
    search.kind = 1;
    search.text = ZeGraphOptionalIndex {
        present: 1,
        index: 0,
    };
    search.k = 1;
    search.node_slot = 5;
    search.score_slot = 9;
    let mut operator: ZeGraphOperator = sized_zeroed();
    operator.kind = 8;
    let eager = [0];
    let mut plan: ZeGraphPlan = sized_zeroed();
    plan.operators = &operator;
    plan.operator_count = 1;
    plan.expressions = expressions.as_ptr();
    plan.expression_count = 2;
    plan.searches = &search;
    plan.search_count = 1;
    plan.eager_searches = eager.as_ptr();
    plan.eager_search_count = 1;
    plan.pool = &pool;
    let mut request: ZeGraphQueryRequest = sized_zeroed();
    request.plan = &plan;
    let mut response = empty_response();
    assert_eq!(
        ze_graph_query(store.handle, &request, &mut response),
        ZeErrorCode::ZeOk,
        "{}",
        last_error(store.handle.token)
    );
    assert_eq!(response.report_count, 1);
    let report = unsafe { &*response.reports };
    assert_eq!(report.kind, 1);
    assert_eq!(report.has_requested_tier, 0);
    assert_eq!(ze_graph_response_free(&mut response), ZeErrorCode::ZeOk);
    // A row dependency is legal only when the descriptor names eligibility.
    operator.inputs.count = 1;
    let operators = [sized_zeroed(), operator];
    let inputs = [0];
    let eager = [1];
    plan.operators = operators.as_ptr();
    plan.operator_count = 2;
    plan.root = 1;
    plan.inputs = inputs.as_ptr();
    plan.input_count = 1;
    plan.eager_searches = eager.as_ptr();
    request.plan = &plan;
    assert_eq!(
        ze_graph_query(store.handle, &request, &mut response),
        ZeErrorCode::ZeErrInvalidArgument
    );
}

#[cfg(feature = "abi-panic-probe")]
#[test]
fn ze241_concurrent_writers_are_busy() {
    use std::sync::{Arc, Barrier};
    use zeppelin_embed::vfs::file_test_support::{FileEvent, FileOperationScope};
    let store = GraphTestStore::create();
    let handle = store.handle;
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let (thread_entered, thread_release) = (entered.clone(), release.clone());
    let writer = std::thread::spawn(move || {
        let mut fired = false;
        let _scope = FileOperationScope::install(move |event| {
            if event == FileEvent::BeforeAppend && !fired {
                fired = true;
                thread_entered.wait();
                thread_release.wait();
            }
            Ok(())
        });
        let mut response = empty_response();
        let code = ze_graph_cypher(
            handle,
            &cypher_request(b"CREATE (:First)", &[], None),
            &mut response,
        );
        let disposition = response.disposition;
        ze_graph_response_free(&mut response);
        (code, disposition)
    });
    entered.wait();
    let mut response = empty_response();
    let code = ze_graph_cypher(
        handle,
        &cypher_request(b"CREATE (:Second)", &[], None),
        &mut response,
    );
    release.wait();
    assert_eq!(writer.join().unwrap(), (ZeErrorCode::ZeOk, 2));
    assert_eq!(code, ZeErrorCode::ZeErrBusy);
    assert_eq!(response.disposition, 1);
    let mut read = cypher_ok(handle, "MATCH (n) RETURN count(n) AS count");
    assert_eq!(rows(&read)[0][0].integer, 1);
    ze_graph_response_free(&mut read);
}

#[cfg(feature = "abi-panic-probe")]
#[test]
fn ze241_postcommit_cancel_preserves_commit() {
    use zeppelin_embed::vfs::file_test_support::{FileEvent, FileOperationScope};
    let store = GraphTestStore::create();
    let mut cancel = 0;
    assert_eq!(ze_cancel_token_create(&mut cancel), ZeErrorCode::ZeOk);
    let mut append_seen = false;
    let scope = FileOperationScope::install(move |event| {
        if event == FileEvent::BeforeAppend {
            append_seen = true;
        }
        if event == FileEvent::AfterSync && append_seen {
            assert_eq!(ze_cancel_token_cancel(cancel), ZeErrorCode::ZeOk);
        }
        Ok(())
    });
    let mut control: ZeGraphControl = sized_zeroed();
    control.cancel_token = cancel;
    let mut request = cypher_request(b"CREATE (:AfterSync)", &[], None);
    request.control = &control;
    let mut response = empty_response();
    assert_eq!(
        ze_graph_cypher(store.handle, &request, &mut response),
        ZeErrorCode::ZeOk,
        "{}",
        last_error(store.handle.token)
    );
    // One store counter: enable_graph commits generation 1; this first graph write commits 2.
    assert_eq!((response.disposition, response.changed_generation), (2, 2));
    ze_graph_response_free(&mut response);
    drop(scope);
    ze_cancel_token_free(cancel);
    let mut read = cypher_ok(store.handle, "MATCH (n:AfterSync) RETURN count(n) AS count");
    assert_eq!(rows(&read)[0][0].integer, 1);
    ze_graph_response_free(&mut read);
}

#[cfg(feature = "abi-panic-probe")]
#[test]
fn ze241_io_uncertainty_requires_reopen() {
    use zeppelin_embed::vfs::file_test_support::{FileEvent, FileOperationScope};
    let mut store = GraphTestStore::create();
    let mut append_seen = false;
    let scope = FileOperationScope::install(move |event| {
        if event == FileEvent::BeforeAppend {
            append_seen = true;
        }
        if event == FileEvent::BeforeSync && append_seen {
            return Err(std::io::Error::other(
                "ZE-241 WAL sync refusal after real append",
            ));
        }
        Ok(())
    });
    let mut response = empty_response();
    let request = cypher_request(b"CREATE (:Uncertain)", &[], None);
    assert_eq!(
        ze_graph_cypher(store.handle, &request, &mut response),
        ZeErrorCode::ZeErrIndeterminateCommit
    );
    assert_eq!(response.disposition, 5);
    drop(scope);
    assert!(matches!(
        ze_graph_cypher(store.handle, &request, &mut response),
        ZeErrorCode::ZeErrAccessMode | ZeErrorCode::ZeErrIndeterminateCommit
    ));
    let path = store.path.clone();
    let _ = store.close();
    let (code, handle) = graph_open(&path, MODE_READ_WRITE);
    assert_eq!(code, ZeErrorCode::ZeOk);
    let mut read = cypher_ok(handle, "MATCH (n:Uncertain) RETURN count(n) AS count");
    assert_eq!(rows(&read)[0][0].integer, 1);
    ze_graph_response_free(&mut read);
    ze_graph_close(handle);
}

#[test]
fn ze241_boundary_refusals_leave_no_effects() {
    let store = GraphTestStore::create();
    let mut response = empty_response();
    let mut cancel = 0;
    assert_eq!(ze_cancel_token_create(&mut cancel), ZeErrorCode::ZeOk);
    assert_eq!(ze_cancel_token_cancel(cancel), ZeErrorCode::ZeOk);
    let mut control: ZeGraphControl = sized_zeroed();
    control.cancel_token = cancel;
    let mut request = cypher_request(b"CREATE (:Cancelled)", &[], None);
    request.control = &control;
    assert_eq!(
        ze_graph_cypher(store.handle, &request, &mut response),
        ZeErrorCode::ZeErrCancelled
    );
    ze_cancel_token_free(cancel);
    let request = cypher_request(b"CREATE (:Unsupported) RETURN sin(1)", &[], None);
    assert_eq!(
        ze_graph_cypher(store.handle, &request, &mut response),
        ZeErrorCode::ZeErrQueryUnsupported
    );
    let mut pool: ZeGraphValuePool = sized_zeroed();
    let mut operator: ZeGraphOperator = sized_zeroed();
    operator.kind = 5;
    operator.inputs.count = 1;
    let mut inputs = [0];
    let mut plan: ZeGraphPlan = sized_zeroed();
    plan.pool = &pool;
    plan.operators = &operator;
    plan.operator_count = 1;
    plan.inputs = inputs.as_ptr();
    plan.input_count = 1;
    let mut query: ZeGraphQueryRequest = sized_zeroed();
    query.plan = &plan;
    assert_ne!(
        ze_graph_query(store.handle, &query, &mut response),
        ZeErrorCode::ZeOk
    );
    inputs[0] = 7;
    plan.inputs = inputs.as_ptr();
    query.plan = &plan;
    assert_ne!(
        ze_graph_query(store.handle, &query, &mut response),
        ZeErrorCode::ZeOk
    );
    pool.abi_reserved = 1;
    plan.pool = &pool;
    query.plan = &plan;
    assert_eq!(
        ze_graph_query(store.handle, &query, &mut response),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let mut get: ZeGraphGetNodesRequest = sized_zeroed();
    for flag in [2, u32::MAX] {
        get.include_text = flag;
        assert_eq!(
            ze_graph_get_nodes(store.handle, &get, &mut response),
            ZeErrorCode::ZeErrInvalidArgument
        );
    }
    get.include_text = 0;
    get.id_count = 1;
    assert_eq!(
        ze_graph_get_nodes(store.handle, &get, &mut response),
        ZeErrorCode::ZeErrInvalidArgument
    );
    get.id_count = usize::MAX;
    assert_eq!(
        ze_graph_get_nodes(store.handle, &get, &mut response),
        ZeErrorCode::ZeErrInvalidArgument
    );
    get.id_count = 0;
    let mut invalid_control: ZeGraphControl = sized_zeroed();
    invalid_control.abi_reserved = 1;
    get.control = &invalid_control;
    assert_eq!(
        ze_graph_get_nodes(store.handle, &get, &mut response),
        ZeErrorCode::ZeErrInvalidArgument
    );
    get.control = std::ptr::null();
    let legacy = common::TestStore::new();
    assert_eq!(
        ze_graph_get_nodes(
            ZeGraphHandle {
                token: legacy.handle
            },
            &get,
            &mut response
        ),
        ZeErrorCode::ZeErrInvalidHandle
    );
    let mut read = cypher_ok(store.handle, "MATCH (n) RETURN count(n) AS count");
    assert_eq!(rows(&read)[0][0].integer, 0);
    ze_graph_response_free(&mut read);
}

#[cfg(feature = "abi-panic-probe")]
#[test]
fn ze241_get_full_width_nodes_and_relationships_survive_reopen() {
    use zeppelin_embed::lifecycle::Store;
    use zeppelin_embed::property_graph::{NodeId, RelId};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("seeded");
    let node_seed = (1u128 << 127) + 7;
    let rel_seed = (1u128 << 126) + 19;
    let bootstrap = Store::create_graph_with_allocator_seed_for_test(
        &path,
        zeppelin_embed::lifecycle::OpenOptions::new().with_max_resident_bytes(256 << 20),
        NodeId::new(node_seed).unwrap(),
        RelId::new(rel_seed).unwrap(),
    )
    .unwrap();
    bootstrap.close().unwrap();
    let (code, handle) = graph_open(&path, MODE_READ_WRITE);
    assert_eq!(code, ZeErrorCode::ZeOk);
    let mut builder = PoolBuilder::new();
    let first = single_node(&mut builder, "first");
    let second = single_node(&mut builder, "second");
    let ns = builder.text("rels");
    let key = builder.text("link");
    let image = builder.relationship_image("LINK", 0..0);
    let rel = create_rel_item(ns, key, 1, image, local_endpoint(0), local_endpoint(1));
    let pool = builder.pool();
    let mut applied = empty_response();
    assert_eq!(
        ze_graph_apply(
            handle,
            &batch_request(&[first, second, rel], &pool),
            &mut applied
        ),
        ZeErrorCode::ZeOk,
        "{}",
        last_error(handle.token)
    );
    let receipts = receipts(&applied);
    assert_eq!(
        receipts[0].node,
        ZeNodeId {
            high: 1 << 63,
            low: 7
        }
    );
    assert_eq!(
        receipts[2].relationship,
        ZeRelId {
            high: 1 << 62,
            low: 19
        }
    );
    let nodes = [receipts[0].node, receipts[1].node];
    let rels = [
        receipts[2].relationship,
        ZeRelId {
            high: u64::MAX,
            low: 19,
        },
        receipts[2].relationship,
    ];
    assert_eq!(ze_graph_close(handle), ZeErrorCode::ZeOk);
    let (code, handle) = graph_open(&path, MODE_READ_WRITE);
    assert_eq!(code, ZeErrorCode::ZeOk);
    let mut get: ZeGraphGetRelsRequest = sized_zeroed();
    get.ids = rels.as_ptr();
    get.id_count = rels.len();
    let mut response = empty_response();
    assert_eq!(
        ze_graph_get_relationships(handle, &get, &mut response),
        ZeErrorCode::ZeOk
    );
    assert_eq!(
        rows(&response)
            .iter()
            .map(|row| row[0].tag)
            .collect::<Vec<_>>(),
        [6, 0, 6]
    );
    let relationships = unsafe {
        std::slice::from_raw_parts(
            response.pool.relationships,
            response.pool.relationship_count,
        )
    };
    assert_eq!(
        (relationships[0].source, relationships[0].target),
        (nodes[0], nodes[1])
    );
    assert_eq!(ze_graph_close(handle), ZeErrorCode::ZeOk);
    assert_eq!(ze_graph_response_free(&mut response), ZeErrorCode::ZeOk);
    assert_eq!(ze_graph_response_free(&mut applied), ZeErrorCode::ZeOk);
}

#[test]
fn ze241_search_acceptance_has_worked_rows_and_reports() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("search");
    let epoch = common::EpochFixture::new(2);
    let declaration = epoch.request().embedding.document;
    let bytes = path.to_str().unwrap().as_bytes();
    let mut open = open_request(bytes, MODE_CREATE);
    open.document_tower = &declaration;
    let mut handle = ZeGraphHandle { token: 0 };
    assert_eq!(ze_graph_open(&open, &mut handle), ZeErrorCode::ZeOk);
    let mut builder = PoolBuilder::new();
    let ns = builder.text("docs");
    for (kind, name) in [
        (1, "booleans"),
        (2, "integers"),
        (3, "floats"),
        (4, "strings"),
        (5, "empty"),
    ] {
        let value = builder.empty_list(kind);
        builder.property(name, value);
    }
    let first = builder.node_image(&[], 0..5, Some("amber"));
    builder.node_vector(first, &[0.0, 0.0]);
    let far = builder.node_image(&[], 0..0, None);
    builder.node_vector(far, &[3.0, 4.0]);
    let text = builder.node_image(&[], 0..0, Some("amber amber"));
    let empty = builder.node_image(&[], 0..0, Some(""));
    let mut items = Vec::new();
    for (name, image) in [
        ("origin", first),
        ("far", far),
        ("text", text),
        ("empty", empty),
    ] {
        let key = builder.text(name);
        items.push(create_node_item(ns, key, 1, image));
    }
    let pool = builder.pool();
    let mut response = empty_response();
    assert_eq!(
        ze_graph_apply(handle, &batch_request(&items, &pool), &mut response),
        ZeErrorCode::ZeOk,
        "{}",
        last_error(handle.token)
    );
    let ids = receipts(&response)
        .iter()
        .map(|r| r.node)
        .collect::<Vec<_>>();
    ze_graph_response_free(&mut response);
    let mut get: ZeGraphGetNodesRequest = sized_zeroed();
    get.ids = ids.as_ptr();
    get.id_count = ids.len();
    get.include_text = 1;
    get.include_vector = 1;
    assert_eq!(
        ze_graph_get_nodes(handle, &get, &mut response),
        ZeErrorCode::ZeOk
    );
    let nodes =
        unsafe { std::slice::from_raw_parts(response.pool.nodes, response.pool.node_count) };
    assert_eq!((nodes[0].has_vector, nodes[0].vector.count), (1, 2));
    assert_eq!(
        (nodes[2].has_vector, nodes[3].has_text, nodes[3].text.count),
        (0, 1, 0)
    );
    let properties = unsafe {
        std::slice::from_raw_parts(response.pool.properties, response.pool.property_count)
    };
    let values =
        unsafe { std::slice::from_raw_parts(response.pool.values, response.pool.value_count) };
    let kinds = properties
        .iter()
        .map(|p| values[p.value as usize].list_kind)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(kinds, [1, 2, 3, 4, 5].into_iter().collect());
    ze_graph_response_free(&mut response);
    let mut values: [ZeGraphValue; 4] = [sized_zeroed(); 4];
    values[0].tag = 2;
    values[1].tag = 2;
    values[1].integer = 3;
    values[2].tag = 4;
    values[2].range.count = 5;
    values[3].tag = 2;
    let mut pool: ZeGraphValuePool = sized_zeroed();
    pool.values = values.as_ptr();
    pool.value_count = values.len();
    pool.bytes = b"amberMISSING".as_ptr();
    pool.byte_count = 12;
    let children = [0, 0];
    let mut expressions: [ZeGraphExpression; 5] = [sized_zeroed(); 5];
    expressions[1].value = 1;
    expressions[2].kind = 7;
    expressions[2].children.count = 2;
    expressions[3].value = 2;
    expressions[4].value = 3;
    let mut search: ZeGraphSearch = sized_zeroed();
    search.vector = ZeGraphOptionalIndex {
        present: 1,
        index: 2,
    };
    search.k = 1;
    search.node_slot = 10;
    search.score_slot = 20;
    let mut operator: ZeGraphOperator = sized_zeroed();
    operator.kind = 8;
    let eager = [0];
    let mut plan: ZeGraphPlan = sized_zeroed();
    plan.operators = &operator;
    plan.operator_count = 1;
    plan.expressions = expressions.as_ptr();
    plan.expression_count = 3;
    plan.expression_children = children.as_ptr();
    plan.expression_child_count = 2;
    plan.searches = &search;
    plan.search_count = 1;
    plan.eager_searches = eager.as_ptr();
    plan.eager_search_count = 1;
    plan.pool = &pool;
    let mut request: ZeGraphQueryRequest = sized_zeroed();
    request.plan = &plan;
    let mut invalid_width: ZeGraphSearchOptions = sized_zeroed();
    invalid_width.graph_ef = 16;
    search.options = &invalid_width;
    plan.searches = &search;
    request.plan = &plan;
    assert_eq!(
        ze_graph_query(handle, &request, &mut response),
        ZeErrorCode::ZeErrInvalidArgument
    );
    for (has_tier, tier) in [(0, 0), (1, 0), (1, 1), (1, 2), (1, 3)] {
        let mut options: ZeGraphSearchOptions = sized_zeroed();
        options.graph_seed = 7;
        options.graph_ef = 2;
        options.rescore = 1;
        search.has_tier = has_tier;
        search.tier = tier;
        search.options = &options;
        plan.searches = &search;
        request.plan = &plan;
        assert_eq!(
            ze_graph_query(handle, &request, &mut response),
            ZeErrorCode::ZeOk,
            "tier {tier}: {}",
            last_error(handle.token)
        );
        let result = rows(&response);
        assert_eq!(result.len(), 2);
        assert_eq!((result[0][1].floating, result[1][1].floating), (0.0, 25.0));
        let report = unsafe { &*response.reports };
        assert_eq!(
            (report.has_requested_tier, report.requested_tier),
            (has_tier, tier)
        );
        ze_graph_response_free(&mut response);
    }
    search.options = std::ptr::null();
    search.has_tier = 1;
    search.tier = 1;
    expressions[3].value = 3;
    plan.expressions = expressions.as_ptr();
    plan.expression_count = 4;
    search.window = ZeGraphOptionalIndex {
        present: 1,
        index: 3,
    };
    plan.searches = &search;
    request.plan = &plan;
    assert_ne!(
        ze_graph_query(handle, &request, &mut response),
        ZeErrorCode::ZeOk
    );
    search.window = ZeGraphOptionalIndex {
        present: 0,
        index: 0,
    };
    search.kind = 1;
    search.has_tier = 0;
    search.tier = 0;
    search.vector = ZeGraphOptionalIndex {
        present: 0,
        index: 0,
    };
    search.text = ZeGraphOptionalIndex {
        present: 1,
        index: 0,
    };
    let mut text_expressions = [expressions[0], expressions[1]];
    text_expressions[0].value = 2;
    plan.expressions = text_expressions.as_ptr();
    plan.expression_count = 2;
    plan.searches = &search;
    request.plan = &plan;
    assert_eq!(
        ze_graph_query(handle, &request, &mut response),
        ZeErrorCode::ZeOk,
        "{}",
        last_error(handle.token)
    );
    let result = rows(&response);
    assert_eq!(result.len(), 2);
    // Only the two nonempty analyzed texts are indexed: N=2, df=2, avglen=1.5.
    let idf = 1.2_f64.ln();
    assert!((result[0][1].floating - idf * 4.4 / 3.5).abs() < 1e-6);
    assert!((result[1][1].floating - idf * 2.2 / 1.9).abs() < 1e-6);
    ze_graph_response_free(&mut response);
    let mut prefix: ZeGraphSearchOptions = sized_zeroed();
    prefix.lexical_flags = 1;
    values[2].range.count = 2;
    pool.values = values.as_ptr();
    plan.pool = &pool;
    search.options = &prefix;
    plan.searches = &search;
    request.plan = &plan;
    assert_eq!(
        ze_graph_query(handle, &request, &mut response),
        ZeErrorCode::ZeOk
    );
    assert_eq!(response.row_count, 2);
    ze_graph_response_free(&mut response);
    values[2].range.count = 5;
    pool.values = values.as_ptr();
    plan.pool = &pool;
    let mut options: ZeGraphSearchOptions = sized_zeroed();
    options.has_alpha = 1;
    options.alpha = 0.25;
    options.has_max_rounds = 1;
    expressions[3].value = 2;
    plan.expressions = expressions.as_ptr();
    plan.expression_count = 4;
    search.text = ZeGraphOptionalIndex {
        present: 1,
        index: 3,
    };
    search.kind = 2;
    search.has_tier = 1;
    search.tier = 1;
    search.vector = ZeGraphOptionalIndex {
        present: 1,
        index: 2,
    };
    search.vector_distance_slot = ZeGraphOptionalIndex {
        present: 1,
        index: 30,
    };
    search.lexical_score_slot = ZeGraphOptionalIndex {
        present: 1,
        index: 40,
    };
    search.options = &options;
    plan.searches = &search;
    request.plan = &plan;
    assert_eq!(
        ze_graph_query(handle, &request, &mut response),
        ZeErrorCode::ZeOk,
        "{}",
        last_error(handle.token)
    );
    let result = rows(&response);
    assert_eq!(result.len(), 3);
    assert!((result[0][1].floating - (0.25 + 0.75 * 3.5 / 3.8)).abs() < 1e-6);
    assert!((result[1][1].floating - 0.75).abs() < 1e-6);
    assert_eq!(result[1][2].tag, 0);
    assert!(result[2][1].floating.abs() < 1e-12);
    assert_eq!(result[2][3].tag, 0);
    assert_eq!(unsafe { (*response.reports).effective_alpha }, 0.25);
    ze_graph_response_free(&mut response);
    options.has_alpha = 0;
    options.alpha = 0.0;
    options.rules_enabled = 1;
    search.options = &options;
    plan.searches = &search;
    request.plan = &plan;
    assert_eq!(
        ze_graph_query(handle, &request, &mut response),
        ZeErrorCode::ZeOk
    );
    assert_eq!(unsafe { (*response.reports).effective_alpha }, 0.4);
    ze_graph_response_free(&mut response);
    // The eager call survives a root LIMIT 0 and still validates its window.
    let mut limit: ZeGraphOperator = sized_zeroed();
    limit.kind = 9;
    limit.inputs.count = 1;
    limit.has_limit = 1;
    let operators = [operator, limit];
    let input = [0];
    plan.operators = operators.as_ptr();
    plan.operator_count = 2;
    plan.inputs = input.as_ptr();
    plan.input_count = 1;
    plan.root = 1;
    request.plan = &plan;
    assert_eq!(
        ze_graph_query(handle, &request, &mut response),
        ZeErrorCode::ZeOk
    );
    assert_eq!((response.row_count, response.report_count), (0, 1));
    ze_graph_response_free(&mut response);
    plan.expression_count = 5;
    search.window = ZeGraphOptionalIndex {
        present: 1,
        index: 4,
    };
    plan.searches = &search;
    request.plan = &plan;
    assert_ne!(
        ze_graph_query(handle, &request, &mut response),
        ZeErrorCode::ZeOk
    );
    search.window = ZeGraphOptionalIndex {
        present: 0,
        index: 0,
    };
    search.eligible_set = ZeGraphOptionalIndex {
        present: 1,
        index: 77,
    };
    plan.searches = &search;
    plan.expression_count = 4;
    let mut scan: ZeGraphOperator = sized_zeroed();
    scan.kind = 4;
    scan.node_slot = 55;
    scan.has_name = 1;
    scan.name = ZeGraphRange { start: 5, count: 7 };
    let mut eligible: ZeGraphOperator = sized_zeroed();
    eligible.kind = 20;
    eligible.inputs.count = 1;
    eligible.source_slot = 55;
    eligible.set_slot = 77;
    let mut source = operator;
    source.inputs = ZeGraphRange { start: 1, count: 1 };
    let operators = [scan, eligible, source];
    let inputs = [0, 1];
    let eager = [2];
    plan.operators = operators.as_ptr();
    plan.operator_count = 3;
    plan.inputs = inputs.as_ptr();
    plan.input_count = 2;
    plan.root = 2;
    plan.eager_searches = eager.as_ptr();
    request.plan = &plan;
    assert_eq!(
        ze_graph_query(handle, &request, &mut response),
        ZeErrorCode::ZeOk,
        "{}",
        last_error(handle.token)
    );
    assert_eq!((response.row_count, response.report_count), (0, 1));
    assert_eq!(response.column_count, 4);
    ze_graph_response_free(&mut response);
    for (query, count) in [
        (
            "CALL ze.text_search('amber',3) YIELD node RETURN count(node)",
            2,
        ),
        (
            "CALL ze.vector_search([0,0],3,'exact') YIELD node RETURN count(node)",
            2,
        ),
        (
            "CALL ze.hybrid_search([0,0],'amber',3,'exact') YIELD node RETURN count(node)",
            3,
        ),
    ] {
        let mut response = cypher_ok(handle, query);
        assert_eq!(rows(&response)[0][0].integer, count);
        assert_eq!(response.report_count, 1);
        ze_graph_response_free(&mut response);
    }
    assert_eq!(ze_graph_close(handle), ZeErrorCode::ZeOk);
}

#[test]
fn ze241_empty_gets_accept_null_zero_and_validate_controls() {
    let store = GraphTestStore::create();
    let mut response = empty_response();
    let mut get: ZeGraphGetNodesRequest = sized_zeroed();
    assert_eq!(
        ze_graph_get_nodes(store.handle, &get, &mut response),
        ZeErrorCode::ZeOk,
        "{}",
        last_error(store.handle.token)
    );
    assert_eq!((response.row_count, response.column_count), (0, 1));
    ze_graph_response_free(&mut response);
    let mut control: ZeGraphControl = sized_zeroed();
    control.abi_reserved = 1;
    get.control = &control;
    assert_eq!(
        ze_graph_get_nodes(store.handle, &get, &mut response),
        ZeErrorCode::ZeErrInvalidArgument
    );
    let rels: ZeGraphGetRelsRequest = sized_zeroed();
    assert_eq!(
        ze_graph_get_relationships(store.handle, &rels, &mut response),
        ZeErrorCode::ZeOk,
        "{}",
        last_error(store.handle.token)
    );
    assert_eq!((response.row_count, response.column_count), (0, 1));
    ze_graph_response_free(&mut response);
}

#[cfg(feature = "abi-panic-probe")]
#[test]
fn ze241_mid_query_deadline_returns_no_partial_rows() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::{Duration, Instant};
    struct Clock {
        base: Instant,
        calls: AtomicUsize,
    }
    impl zeppelin_embed::lifecycle::MonotonicClock for Clock {
        fn now(&self) -> Instant {
            if !zeppelin_embed_ffi::graph_deadline_test_support::query_classified() {
                return self.base;
            }
            let calls = self.calls.fetch_add(1, Ordering::SeqCst);
            self.base
                + if calls < 20 {
                    Duration::ZERO
                } else {
                    Duration::from_nanos(2)
                }
        }
    }
    let store = GraphTestStore::create();
    for _ in 0..32 {
        let mut response = cypher_ok(store.handle, "CREATE (:Deadline)");
        ze_graph_response_free(&mut response);
    }
    let clock = Arc::new(Clock {
        base: Instant::now(),
        calls: AtomicUsize::new(0),
    });
    let _scope =
        zeppelin_embed_ffi::graph_deadline_test_support::ClockScope::install(clock.clone());
    let mut control: ZeGraphControl = sized_zeroed();
    control.deadline_ns = 1;
    let mut operator: ZeGraphOperator = sized_zeroed();
    operator.kind = 4;
    operator.node_slot = 3;
    let pool: ZeGraphValuePool = sized_zeroed();
    let mut plan: ZeGraphPlan = sized_zeroed();
    plan.operators = &operator;
    plan.operator_count = 1;
    plan.pool = &pool;
    let mut request: ZeGraphQueryRequest = sized_zeroed();
    request.plan = &plan;
    request.control = &control;
    let mut response = empty_response();
    assert_eq!(
        ze_graph_query(store.handle, &request, &mut response),
        ZeErrorCode::ZeErrTimeout
    );
    assert!(clock.calls.load(Ordering::SeqCst) > 20);
    assert_eq!((response.row_count, response.owner_token), (0, 0));
    ze_graph_response_free(&mut response);
    let mut read = cypher_ok(store.handle, "MATCH (n:Deadline) RETURN count(n)");
    assert_eq!(rows(&read)[0][0].integer, 32);
    ze_graph_response_free(&mut read);
}

#[path = "../../../tests/support/graph_bindings.rs"]
mod graph_bindings;
#[test]
fn ze72_shared_semantics_match_independent_oracle() {
    let mut store = GraphTestStore::create();
    let mut created = cypher_ok(store.handle, "CREATE (:Fixture), (:Fixture)");
    ze_graph_response_free(&mut created);
    let mut retained = Vec::new();
    for (name, query, expected) in graph_bindings::cases() {
        let response = cypher_ok(store.handle, query);
        let mut actual = graph_bindings::c_observe(&response);
        if std::env::var("ZE72_MUTATION").as_deref() == Ok("nested") && name == "nested" {
            actual = actual.replace("N", "L0[]");
        }
        graph_bindings::compare(name, &expected, &actual).unwrap();
        retained.push((name, expected, response));
    }
    assert_eq!(store.close(), ZeErrorCode::ZeOk);
    let (code, reopened) = graph_open(&store.path, MODE_READ_WRITE);
    assert_eq!(code, ZeErrorCode::ZeOk);
    for (name, expected, mut response) in retained {
        graph_bindings::compare(name, &expected, &graph_bindings::c_observe(&response)).unwrap();
        assert_eq!(ze_graph_response_free(&mut response), ZeErrorCode::ZeOk);
    }
    assert_eq!(ze_graph_close(reopened), ZeErrorCode::ZeOk);
}

#[cfg(all(feature = "graph-result-test-support", feature = "abi-panic-probe"))]
#[test]
fn ze72_real_faults_match_independent_commit_boundaries() {
    use zeppelin_embed_adversarial_oracle::graph_c_entry::compare_binding;
    for mode in 0..=6 {
        let mut actual = graph_bindings::fault(mode);
        if std::env::var("ZE72_MUTATION").as_deref() == Ok("outcome") {
            actual.disposition ^= 1;
        }
        compare_binding(mode, &actual).unwrap();
        let mut planted = actual.clone();
        planted.recovered_nodes ^= 1;
        assert!(compare_binding(mode, &planted).is_err());
        let control = graph_bindings::fault(0);
        compare_binding(0, &control).unwrap();
    }
}

#[cfg(feature = "abi-panic-probe")]
#[test]
fn ze72_same_low_half_ids_remain_distinct_after_reopen() {
    use zeppelin_embed::lifecycle::Store;
    use zeppelin_embed::property_graph::{NodeId, RelId};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("twins");
    let first = (1u128 << 127) + 7;
    let second = first + (1u128 << 64);
    let graph = Store::create_graph_with_allocator_seed_for_test(
        &path,
        zeppelin_embed::lifecycle::OpenOptions::new().with_max_resident_bytes(256 << 20),
        NodeId::new(first).unwrap(),
        RelId::new(1).unwrap(),
    )
    .unwrap();
    let control = zeppelin_embed::lifecycle::QueryControl::Cancel(
        zeppelin_embed::lifecycle::CancelToken::new(),
    );
    let retained = zeppelin_embed_cypher::execute(
        &graph,
        &control,
        &Default::default(),
        "CREATE (n:Twin) RETURN n",
        &[],
        Default::default(),
    )
    .unwrap();
    graph
        .jump_allocators_for_test(
            NodeId::new(second).unwrap(),
            RelId::new(1).unwrap(),
            &control,
        )
        .unwrap();
    graph.close().unwrap();
    assert_eq!(retained.pools().nodes[0].id.get(), first);
    let (code, handle) = graph_open(&path, MODE_READ_WRITE);
    assert_eq!(code, ZeErrorCode::ZeOk);
    let mut created = cypher_ok(handle, "CREATE (n:Twin) RETURN n");
    let id = unsafe { (*created.pool.nodes).id };
    assert_eq!((id.high, id.low), ((second >> 64) as u64, 7));
    ze_graph_response_free(&mut created);
    let ids = [
        ZeNodeId {
            high: (first >> 64) as u64,
            low: 7,
        },
        id,
    ];
    let mut get: ZeGraphGetNodesRequest = sized_zeroed();
    get.ids = ids.as_ptr();
    get.id_count = 2;
    let mut result = empty_response();
    assert_eq!(
        ze_graph_get_nodes(handle, &get, &mut result),
        ZeErrorCode::ZeOk
    );
    assert_eq!(
        rows(&result).iter().map(|r| r[0].tag).collect::<Vec<_>>(),
        [5, 5]
    );
    let nodes = unsafe { std::slice::from_raw_parts(result.pool.nodes, result.pool.node_count) };
    let expected = format!("D{first:032x}\nD{second:032x}");
    let mut actual = graph_bindings::c_observe(&result);
    if std::env::var("ZE72_MUTATION").as_deref() == Ok("id") {
        actual = actual.replace(&format!("{:016x}", nodes[1].id.high), "0000000000000000");
    }
    graph_bindings::compare("same-low-half", &expected, &actual).unwrap();
    assert_ne!(nodes[0].id.high, nodes[1].id.high);
    assert_eq!(nodes[0].id.low, nodes[1].id.low);
    assert_eq!(ze_graph_close(handle), ZeErrorCode::ZeOk);
    assert_eq!(ze_graph_response_free(&mut result), ZeErrorCode::ZeOk);
    if let Ok(output) = std::env::var("ZE72_CORPUS_OUTPUT") {
        std::fs::write(format!("{output}.twins"), expected).unwrap();
        std::fs::write(format!("{output}.twins.path"), path.to_str().unwrap()).unwrap();
        let _ = dir.keep();
    }
}

fn ze72_wal_bytes(path: &std::path::Path) -> Vec<(std::ffi::OsString, Vec<u8>)> {
    let mut files = std::fs::read_dir(path)
        .unwrap()
        .filter_map(|f| {
            let p = f.unwrap().path();
            let name = p.file_name().unwrap().to_owned();
            name.to_str()
                .unwrap()
                .contains("wal")
                .then(|| (name, std::fs::read(p).unwrap()))
        })
        .collect::<Vec<_>>();
    files.sort();
    assert!(!files.is_empty(), "WAL proof must observe actual log bytes");
    files
}
#[test]
fn ze72_noop_replay_mixed_generations_and_conflicts_preserve_wal() {
    let store = GraphTestStore::create();
    let mut b = PoolBuilder::new();
    let first = single_node(&mut b, "first");
    let second = single_node(&mut b, "second");
    let pool = b.pool();
    let mut r = empty_response();
    assert_eq!(
        ze_graph_apply(store.handle, &batch_request(&[first], &pool), &mut r),
        ZeErrorCode::ZeOk
    );
    let id = receipts(&r)[0].node;
    assert_eq!(receipts(&r)[0].generation, 2);
    ze_graph_response_free(&mut r);
    let before = ze72_wal_bytes(&store.path);
    for (items, disposition) in [(Vec::new(), 4), (vec![first], 3)] {
        assert_eq!(
            ze_graph_apply(store.handle, &batch_request(&items, &pool), &mut r),
            ZeErrorCode::ZeOk
        );
        assert_eq!(r.disposition, disposition);
        assert_eq!(r.has_changed_generation, 0);
        assert_eq!(ze72_wal_bytes(&store.path), before);
        ze_graph_response_free(&mut r);
    }
    assert_eq!(
        ze_graph_apply(
            store.handle,
            &batch_request(&[first, second], &pool),
            &mut r
        ),
        ZeErrorCode::ZeOk
    );
    assert_eq!(r.changed_generation, 3);
    assert_eq!(
        receipts(&r)
            .iter()
            .map(|x| (x.disposition, x.generation))
            .collect::<Vec<_>>(),
        [(3, 2), (2, 3)]
    );
    ze_graph_response_free(&mut r);
    let stable = ze72_wal_bytes(&store.path);
    assert_eq!(
        ze_graph_apply(store.handle, &batch_request(&[first, first], &pool), &mut r),
        ZeErrorCode::ZeErrDuplicateTarget
    );
    ze_graph_response_free(&mut r);
    let mut put = first;
    put.operation = 1;
    put.expected_node = id;
    put.revision = 2;
    assert_eq!(
        ze_graph_apply(store.handle, &batch_request(&[put], &pool), &mut r),
        ZeErrorCode::ZeOk
    );
    ze_graph_response_free(&mut r);
    let updated = ze72_wal_bytes(&store.path);
    assert_ne!(stable, updated);
    assert_eq!(
        ze_graph_apply(store.handle, &batch_request(&[first], &pool), &mut r),
        ZeErrorCode::ZeErrStaleRevision
    );
    ze_graph_response_free(&mut r);
    put.revision = 3;
    put.expected_node.high = u64::MAX;
    assert_eq!(
        ze_graph_apply(store.handle, &batch_request(&[put], &pool), &mut r),
        ZeErrorCode::ZeErrIncarnationConflict
    );
    ze_graph_response_free(&mut r);
    assert_eq!(ze72_wal_bytes(&store.path), updated);
    let get: ZeGraphGetNodesRequest = sized_zeroed();
    assert_eq!(
        ze_graph_get_nodes(store.handle, &get, &mut r),
        ZeErrorCode::ZeOk
    );
    assert_eq!(r.row_count, 0);
    ze_graph_response_free(&mut r);
    let rels: ZeGraphGetRelsRequest = sized_zeroed();
    assert_eq!(
        ze_graph_get_relationships(store.handle, &rels, &mut r),
        ZeErrorCode::ZeOk
    );
    assert_eq!(r.row_count, 0);
    ze_graph_response_free(&mut r);
}
#[test]
fn ze72_present_empty_vector_is_rejected_without_wal_or_generation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty-vector");
    let epoch = common::EpochFixture::new(2);
    let tower = epoch.request().embedding.document;
    let mut open = open_request(path.to_str().unwrap().as_bytes(), MODE_CREATE);
    open.document_tower = &tower;
    let mut handle = ZeGraphHandle { token: 0 };
    assert_eq!(ze_graph_open(&open, &mut handle), ZeErrorCode::ZeOk);
    let before = ze72_wal_bytes(&path);
    let mut b = PoolBuilder::new();
    let ns = b.text("ze72");
    let key = b.text("empty-vector");
    let image = b.node_image(&[], 0..0, Some(""));
    b.node_vector(image, &[]);
    let pool = b.pool();
    let mut r = empty_response();
    assert_eq!(
        ze_graph_apply(
            handle,
            &batch_request(&[create_node_item(ns, key, 1, image)], &pool),
            &mut r
        ),
        ZeErrorCode::ZeErrDimensionMismatch
    );
    assert_eq!(r.has_changed_generation, 0);
    assert_eq!(ze72_wal_bytes(&path), before);
    ze_graph_response_free(&mut r);
    assert_eq!(ze_graph_close(handle), ZeErrorCode::ZeOk);
}

#[cfg(feature = "abi-panic-probe")]
#[test]
fn ze72_close_racing_admitted_query_keeps_results_owned() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier};
    struct Clock {
        entered: Arc<Barrier>,
        release: Arc<Barrier>,
        fired: AtomicBool,
        now: std::time::Instant,
    }
    impl zeppelin_embed::lifecycle::MonotonicClock for Clock {
        fn now(&self) -> std::time::Instant {
            if zeppelin_embed_ffi::graph_deadline_test_support::query_classified()
                && !self.fired.swap(true, Ordering::SeqCst)
            {
                self.entered.wait();
                self.release.wait();
            }
            self.now
        }
    }
    let mut store = GraphTestStore::create();
    let mut created = cypher_ok(store.handle, "CREATE (:Race), (:Race)");
    ze_graph_response_free(&mut created);
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let finished = Arc::new(Barrier::new(2));
    let clock = Arc::new(Clock {
        entered: entered.clone(),
        release: release.clone(),
        fired: AtomicBool::new(false),
        now: std::time::Instant::now(),
    });
    let reader_clock = clock.clone();
    let reader_finished = finished.clone();
    let handle = store.handle;
    let reader = std::thread::spawn(move || {
        let _scope =
            zeppelin_embed_ffi::graph_deadline_test_support::ClockScope::install(reader_clock);
        let mut control: ZeGraphControl = sized_zeroed();
        control.deadline_ns = 1;
        let mut request = cypher_request(b"MATCH (n:Race) RETURN n,[n,null]", &[], None);
        request.control = &control;
        let mut response = empty_response();
        let code = ze_graph_cypher(handle, &request, &mut response);
        reader_finished.wait();
        if code == ZeErrorCode::ZeOk {
            assert_eq!(response.row_count, 2);
            assert!(
                rows(&response)
                    .iter()
                    .all(|r| r[0].tag == 5 && r[1].tag == 7)
            );
        } else {
            assert!(matches!(
                code,
                ZeErrorCode::ZeErrClosing | ZeErrorCode::ZeErrClosed | ZeErrorCode::ZeErrCancelled
            ));
            assert_eq!(response.row_count, 0);
        }
        assert_eq!(ze_graph_response_free(&mut response), ZeErrorCode::ZeOk);
        code
    });
    entered.wait();
    let closer = std::thread::spawn(move || ze_graph_close(handle));
    let start = std::time::Instant::now();
    loop {
        // Invalid output refuses before maintenance and does not acquire the
        // response ownership gate held by the paused query.
        let code = ze_graph_maintain(handle, std::ptr::null(), std::ptr::null_mut());
        if matches!(code, ZeErrorCode::ZeErrClosing | ZeErrorCode::ZeErrClosed) {
            break;
        }
        if start.elapsed() > std::time::Duration::from_secs(5) {
            release.wait();
            finished.wait();
            panic!("ZE-72 close did not reach handle closing boundary");
        }
        std::thread::yield_now();
    }
    release.wait();
    let closed = closer.join().unwrap();
    finished.wait();
    let status = reader.join().unwrap();
    assert_eq!(closed, ZeErrorCode::ZeOk);
    assert!(clock.fired.load(Ordering::SeqCst));
    store.handle.token = 0;
    println!(
        "ZE72 admitted-query close: actual classification hook fired, status={status:?}, retained free GREEN"
    );
}

#[test]
fn ze72_stored_list_kinds_match_shared_fixture() {
    let mut store = GraphTestStore::create();
    let mut b = PoolBuilder::new();
    let cases = graph_bindings::property_cases();
    for (name, kind, _) in &cases {
        let value = b.empty_list(*kind);
        b.property(name, value);
    }
    let image = b.node_image(&["Payload"], 0..cases.len() as u32, Some(""));
    let ns = b.text("ze72");
    let key = b.text("payload");
    let pool = b.pool();
    let mut response = empty_response();
    assert_eq!(
        ze_graph_apply(
            store.handle,
            &batch_request(&[create_node_item(ns, key, 1, image)], &pool),
            &mut response
        ),
        ZeErrorCode::ZeOk
    );
    let id = receipts(&response)[0].node;
    ze_graph_response_free(&mut response);
    let mut get: ZeGraphGetNodesRequest = sized_zeroed();
    get.ids = &id;
    get.id_count = 1;
    get.include_text = 1;
    get.include_vector = 1;
    assert_eq!(
        ze_graph_get_nodes(store.handle, &get, &mut response),
        ZeErrorCode::ZeOk
    );
    assert_eq!(store.close(), ZeErrorCode::ZeOk);
    let node = unsafe { *response.pool.nodes };
    assert_eq!((node.has_text, node.text.count, node.has_vector), (1, 0, 0));
    let props = unsafe {
        std::slice::from_raw_parts(response.pool.properties, response.pool.property_count)
    };
    let bytes =
        unsafe { std::slice::from_raw_parts(response.pool.bytes, response.pool.byte_count) };
    let values =
        unsafe { std::slice::from_raw_parts(response.pool.values, response.pool.value_count) };
    for (name, _, expected) in cases {
        let prop = props
            .iter()
            .find(|p| {
                &bytes[p.name.start as usize..(p.name.start + p.name.count) as usize]
                    == name.as_bytes()
            })
            .unwrap();
        let mut value = values[prop.value as usize];
        if std::env::var("ZE72_MUTATION").as_deref() == Ok("kind") {
            value.list_kind = 0;
        }
        graph_bindings::compare(name, expected, &graph_bindings::c_cell(&response, value)).unwrap();
    }
    assert_eq!(ze_graph_response_free(&mut response), ZeErrorCode::ZeOk);
}

fn ze311_vector_parameter_query(refuse: bool) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("parameter-search");
    let epoch = common::EpochFixture::new(2);
    let declaration = epoch.request().embedding.document;
    let mut open = open_request(path.to_str().unwrap().as_bytes(), MODE_CREATE);
    open.document_tower = &declaration;
    let mut handle = ZeGraphHandle { token: 0 };
    assert_eq!(ze_graph_open(&open, &mut handle), ZeErrorCode::ZeOk);
    let mut builder = PoolBuilder::new();
    let ns = builder.text("docs");
    let near_key = builder.text("near");
    let far_key = builder.text("far");
    let near = builder.node_image(&[], 0..0, None);
    builder.node_vector(near, &[0.0, 0.0]);
    let far = builder.node_image(&[], 0..0, None);
    builder.node_vector(far, &[3.0, 4.0]);
    let pool = builder.pool();
    let items = [
        create_node_item(ns, near_key, 1, near),
        create_node_item(ns, far_key, 1, far),
    ];
    let mut response = empty_response();
    assert_eq!(
        ze_graph_apply(handle, &batch_request(&items, &pool), &mut response),
        ZeErrorCode::ZeOk
    );
    let expected = receipts(&response)[1].node;
    assert_eq!(ze_graph_response_free(&mut response), ZeErrorCode::ZeOk);
    let mut values: [ZeGraphValue; 3] = [sized_zeroed(); 3];
    values[0].tag = 2;
    values[0].integer = 3;
    values[1].tag = 3;
    values[1].floating = 4.0;
    values[2].tag = 7;
    values[2].range.count = 2;
    let children = [0, 1];
    let mut parameter_pool: ZeGraphValuePool = sized_zeroed();
    parameter_pool.bytes = b"v".as_ptr();
    parameter_pool.byte_count = 1;
    parameter_pool.values = values.as_ptr();
    parameter_pool.value_count = 3;
    parameter_pool.children = children.as_ptr();
    parameter_pool.child_count = 2;
    let mut binding: ZeGraphParameterValue = sized_zeroed();
    binding.name.count = 1;
    binding.value = 2;
    let mut plan_pool: ZeGraphValuePool = sized_zeroed();
    plan_pool.bytes = b"v".as_ptr();
    plan_pool.byte_count = 1;
    let mut k_value: ZeGraphValue = sized_zeroed();
    k_value.tag = 2;
    k_value.integer = 1;
    plan_pool.values = &k_value;
    plan_pool.value_count = 1;
    let mut parameter: ZeGraphParameter = sized_zeroed();
    parameter.name.count = 1;
    parameter.kinds = 128;
    let mut expressions: [ZeGraphExpression; 2] = [sized_zeroed(); 2];
    expressions[0].kind = 2;
    let mut search: ZeGraphSearch = sized_zeroed();
    search.vector.present = 1;
    search.k = 1;
    search.has_tier = 1;
    search.tier = 1;
    search.node_slot = 10;
    search.score_slot = 20;
    let mut operator: ZeGraphOperator = sized_zeroed();
    operator.kind = 8;
    let eager = [0];
    let mut plan: ZeGraphPlan = sized_zeroed();
    plan.pool = &plan_pool;
    plan.parameters = &parameter;
    plan.parameter_count = 1;
    plan.operators = &operator;
    plan.operator_count = 1;
    plan.expressions = expressions.as_ptr();
    plan.expression_count = 2;
    plan.searches = &search;
    plan.search_count = 1;
    plan.eager_searches = eager.as_ptr();
    plan.eager_search_count = 1;
    let mut request: ZeGraphQueryRequest = sized_zeroed();
    request.plan = &plan;
    request.parameters = &binding;
    request.parameter_count = 1;
    request.parameter_pool = &parameter_pool;
    let mut limits: ZeGraphQueryLimits = sized_zeroed();
    limits.has_query_bytes = 1;
    limits.query_bytes = 1024;
    let mut options: ZeGraphQueryOptions = sized_zeroed();
    options.limits = &limits;
    if refuse {
        request.options = &options;
    }
    let code = ze_graph_query(handle, &request, &mut response);
    if refuse {
        assert_eq!(
            code,
            ZeErrorCode::ZeErrBudgetExceeded,
            "{}",
            last_error(handle.token)
        );
        assert_eq!(
            (
                response.row_count,
                response.cell_count,
                response.report_count
            ),
            (0, 0, 0)
        );
        assert!(response.cells.is_null() && response.pool.values.is_null());
        // The same handle and backing must remain usable after refusal.
        request.options = std::ptr::null();
        assert_eq!(
            ze_graph_query(handle, &request, &mut response),
            ZeErrorCode::ZeOk,
            "{}",
            last_error(handle.token)
        );
    } else {
        assert_eq!(code, ZeErrorCode::ZeOk, "{}", last_error(handle.token));
    }
    values.fill(sized_zeroed());
    assert_eq!(ze_graph_close(handle), ZeErrorCode::ZeOk);
    let result = rows(&response);
    assert_eq!(result.len(), 1);
    let node = unsafe { &*response.pool.nodes.add(result[0][0].entity_index as usize) };
    assert_eq!(node.id, expected);
    assert_eq!(result[0][1].floating, 0.0);
    assert_eq!(response.report_count, 1);
    let report = unsafe { &*response.reports };
    assert_eq!((report.call_id, report.generation), (0, 2));
    assert_eq!(ze_graph_response_free(&mut response), ZeErrorCode::ZeOk);
}

#[test]
fn ze311_structured_vector_search_parameter_is_retained() {
    ze311_vector_parameter_query(false);
}

#[test]
fn ze311_parameter_backing_limits_leave_no_partial_response() {
    ze311_vector_parameter_query(true);
}
