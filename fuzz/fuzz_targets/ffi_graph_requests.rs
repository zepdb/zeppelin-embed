#![no_main]
//! Every pointer/count pair names honest allocated storage. A caught panic is a bug.
use libfuzzer_sys::fuzz_target;
use std::mem::size_of;
use std::sync::OnceLock;
use zeppelin_embed_ffi::*;
fn sized<T>() -> T {
    let mut value: T = unsafe { std::mem::zeroed() };
    unsafe {
        (&mut value as *mut T)
            .cast::<u32>()
            .write(size_of::<T>() as u32);
    }
    value
}
fn checked(code: ZeErrorCode) {
    assert_ne!(code, ZeErrorCode::ZeErrPanic);
}
struct Fixture {
    handle: ZeGraphHandle,
    path: Vec<u8>,
}
fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let path =
            std::env::temp_dir().join(format!("ze241-ffi-graph-fuzz-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        let path = path.to_string_lossy().into_owned().into_bytes();
        let mut request: ZeGraphOpenRequest = sized();
        request.path = ZeGraphBytes {
            data: path.as_ptr(),
            count: path.len(),
        };
        request.max_resident_bytes = 256 << 20;
        let mut handle = ZeGraphHandle { token: 0 };
        let code = ze_graph_open(&request, &mut handle);
        checked(code);
        assert_eq!(code, ZeErrorCode::ZeOk,
            "ZE-72 graph fuzz requires a supported native graph host; unavailable target waits for ZE-108");
        Fixture { handle, path }
    })
}
fuzz_target!(|data: &[u8]| {
    let fixture = fixture();
    let byte = |index| data.get(index).copied().unwrap_or(0);
    let bytes = data.get(..data.len().min(65_536)).unwrap_or(&[]);
    let mut pool: ZeGraphValuePool = sized();
    let mut pooled_bytes = Vec::with_capacity(bytes.len() + 1);
    pooled_bytes.push(b'p');
    pooled_bytes.extend_from_slice(bytes);
    pool.bytes = pooled_bytes.as_ptr();
    pool.byte_count = pooled_bytes.len();
    let mut values: [ZeGraphValue; 16] = std::array::from_fn(|_| sized());
    let children: [u32; 16] = std::array::from_fn(|index| u32::from(byte(index)) % 20);
    for (index, value) in values.iter_mut().enumerate() {
        value.tag = u32::from(byte(index)) % 9;
        if value.tag == 1 {
            value.boolean = u32::from(byte(index + 16));
        }
        if value.tag == 2 {
            value.integer = i64::from(byte(index + 16));
        }
        if value.tag == 4 || value.tag == 7 {
            value.range.count = u32::from(byte(index + 16));
        }
    }
    let coordinates = [f32::from_bits(u32::from(byte(19))), 0.0];
    let mut node: ZeGraphNode = sized();
    node.has_text = u32::from(byte(20) % 3);
    node.text.count = u32::from(byte(21));
    node.has_vector = u32::from(byte(22) % 3);
    node.vector.count = u32::from(byte(23) % 4);
    pool.nodes = &node;
    pool.node_count = 1;
    pool.vectors = coordinates.as_ptr();
    pool.vector_count = coordinates.len();
    pool.values = values.as_ptr();
    pool.value_count = values.len();
    pool.children = children.as_ptr();
    pool.child_count = children.len();
    let mut response: ZeGraphResponse = sized();
    let handle = fixture.handle;
    if byte(24) & 1 != 0 {
        let text = b"RETURN null, [], [1,null,['nested']]";
        let mut completed: ZeGraphCypherRequest = sized();
        completed.query = ZeGraphBytes {
            data: text.as_ptr(),
            count: text.len(),
        };
        assert_eq!(
            ze_graph_cypher(handle, &completed, &mut response),
            ZeErrorCode::ZeOk
        );
        assert_eq!(response.row_count, 1);
        checked(ze_graph_response_free(&mut response));
    }
    let mut cypher: ZeGraphCypherRequest = sized();
    cypher.query = ZeGraphBytes {
        data: bytes.as_ptr(),
        count: bytes.len(),
    };
    let mut binding: ZeGraphParameterValue = sized();
    binding.name.count = 1;
    binding.value = u32::from(byte(9)) % 16;
    cypher.parameters = &binding;
    cypher.parameter_count = 1;
    if byte(10) & 1 != 0 {
        cypher.query = ZeGraphBytes {
            data: b"RETURN $p".as_ptr(),
            count: 9,
        };
    }
    cypher.parameter_pool = &pool;
    checked(ze_graph_cypher(handle, &cypher, &mut response));
    checked(ze_graph_response_free(&mut response));
    checked(ze_graph_cypher_with_row_limit(
        handle,
        &cypher,
        u32::from(byte(0)),
        &mut response,
    ));
    checked(ze_graph_response_free(&mut response));
    let mut operator: ZeGraphOperator = sized();
    operator.kind = u32::from(byte(1)) % 22;
    let inputs = [u32::from(byte(2))];
    operator.inputs = ZeGraphRange {
        start: 0,
        count: u32::from(byte(3) % 2),
    };
    let mut plan: ZeGraphPlan = sized();
    plan.operators = &operator;
    plan.operator_count = 1;
    plan.inputs = inputs.as_ptr();
    plan.input_count = 1;
    let mut expression: ZeGraphExpression = sized();
    expression.kind = u32::from(byte(11)) % 9;
    expression.value = u32::from(byte(12)) % 16;
    expression.children.count = u32::from(byte(13)) % 17;
    plan.expressions = &expression;
    plan.expression_count = usize::from(byte(14) % 2);
    plan.expression_children = children.as_ptr();
    plan.expression_child_count = 16;
    let mut search: ZeGraphSearch = sized();
    search.kind = u32::from(byte(15)) % 4;
    search.vector = ZeGraphOptionalIndex {
        present: 1,
        index: 0,
    };
    search.node_slot = 1;
    search.score_slot = 2;
    plan.searches = &search;
    plan.search_count = usize::from(byte(16) % 2);
    let mut mutation: ZeGraphMutation = sized();
    mutation.kind = u32::from(byte(17)) % 7;
    mutation.output = 1;
    plan.mutations = &mutation;
    plan.mutation_count = usize::from(byte(18) % 2);
    plan.pool = &pool;
    let mut query: ZeGraphQueryRequest = sized();
    query.plan = &plan;
    checked(ze_graph_query(handle, &query, &mut response));
    checked(ze_graph_response_free(&mut response));
    let mut item: ZeGraphBatchItem = sized();
    item.operation = u32::from(byte(4));
    let mut batch: ZeGraphBatchRequest = sized();
    batch.items = &item;
    batch.item_count = 1;
    batch.pool = &pool;
    checked(ze_graph_apply(handle, &batch, &mut response));
    checked(ze_graph_response_free(&mut response));
    let ids = [ZeNodeId {
        high: u64::from(byte(5)),
        low: u64::from(byte(6)),
    }];
    let mut nodes: ZeGraphGetNodesRequest = sized();
    nodes.ids = ids.as_ptr();
    nodes.id_count = 1;
    nodes.include_text = u32::from(byte(7));
    checked(ze_graph_get_nodes(handle, &nodes, &mut response));
    checked(ze_graph_response_free(&mut response));
    let ids = [ZeRelId {
        high: u64::from(byte(5)),
        low: u64::from(byte(6)),
    }];
    let mut rels: ZeGraphGetRelsRequest = sized();
    rels.ids = ids.as_ptr();
    rels.id_count = 1;
    checked(ze_graph_get_relationships(handle, &rels, &mut response));
    checked(ze_graph_response_free(&mut response));
    let mut report: ZeGraphMaintainReport = sized();
    checked(ze_graph_maintain(handle, std::ptr::null(), &mut report));
    let mut policy: ZeGraphMaintenancePolicy = sized();
    policy.automatic = u32::from(byte(8));
    policy.reclaim_after_bytes = 1 << 20;
    checked(ze_graph_set_maintenance_policy(handle, &policy));
    let mut open: ZeGraphOpenRequest = sized();
    open.mode = 2;
    open.path = ZeGraphBytes {
        data: fixture.path.as_ptr(),
        count: fixture.path.len(),
    };
    open.max_resident_bytes = 256 << 20;
    let mut opened = ZeGraphHandle { token: 0 };
    checked(ze_graph_open(&open, &mut opened));
    if opened.token != 0 {
        let text = b"RETURN null, [], [1,null,['nested']]";
        let mut completed: ZeGraphCypherRequest = sized();
        completed.query = ZeGraphBytes {
            data: text.as_ptr(),
            count: text.len(),
        };
        checked(ze_graph_cypher(opened, &completed, &mut response));
        // Callee ownership is independent of the producing handle's lifetime.
        checked(ze_graph_close(opened));
        checked(ze_graph_response_free(&mut response));
        checked(ze_graph_response_free(&mut response));
        checked(ze_graph_cypher(opened, &completed, &mut response));
        checked(ze_graph_response_free(&mut response));
    }
    opened.token = 0;
    checked(ze_graph_open_with_relationship_types(
        &open,
        std::ptr::null(),
        0,
        &mut opened,
    ));
    checked(ze_graph_close(opened));
});
