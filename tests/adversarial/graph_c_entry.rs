//! Directed C boundary proof; the oracle consumes primitive observations only.
use super::coverage::CoverageRegistry;
use zeppelin_embed_adversarial_oracle::graph_c_entry::{self as oracle, Observed};
use zeppelin_embed_ffi::*;
#[path = "../support/graph_bindings.rs"]
mod bindings;
pub const REQUIRED_COVERAGE: [&str; 7] = [
    "property-graph.c-entry.shared-semantics",
    "property-graph.c-entry.response-after-close",
    "property-graph.c-entry.null-bag-comparator",
    "property-graph.c-entry.query",
    "property-graph.c-entry.get",
    "property-graph.c-entry.oracle.can-fire",
    "property-graph.c-entry.same-seed-control",
];
fn sized<T>() -> T {
    let mut value: T = unsafe { std::mem::zeroed() };
    unsafe {
        (&mut value as *mut T)
            .cast::<u32>()
            .write(std::mem::size_of::<T>() as u32);
    }
    value
}
fn status(code: ZeErrorCode) -> Result<(), String> {
    if code == ZeErrorCode::ZeOk {
        Ok(())
    } else {
        Err(format!("C entry status {code:?}"))
    }
}
struct Response(ZeGraphResponse);
impl Response {
    fn new() -> Self {
        Self(sized())
    }
    fn values(&self) -> Result<Vec<ZeGraphValue>, String> {
        let root = &self.0;
        if root.cell_count != root.row_count * root.column_count
            || (root.cell_count != 0 && root.cells.is_null())
            || (root.pool.value_count != 0 && root.pool.values.is_null())
        {
            return Err("C entry malformed owned geometry".into());
        }
        let cells = if root.cell_count == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(root.cells, root.cell_count) }
        };
        let values = if root.pool.value_count == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(root.pool.values, root.pool.value_count) }
        };
        cells
            .iter()
            .map(|index| {
                values
                    .get(*index as usize)
                    .copied()
                    .ok_or_else(|| "C entry owned cell index".into())
            })
            .collect()
    }
    fn free(&mut self) -> Result<(), String> {
        status(ze_graph_response_free(&mut self.0))
    }
}
impl Drop for Response {
    fn drop(&mut self) {
        let _ = ze_graph_response_free(&mut self.0);
    }
}
struct Store(ZeHandle);
impl Drop for Store {
    fn drop(&mut self) {
        let _ = ze_close(self.0);
    }
}
fn retained_parameters(handle: ZeHandle) -> Result<Response, String> {
    let mut bytes = *b"shellov";
    let mut values: [ZeGraphValue; 4] = [sized(); 4];
    values[0].tag = 4;
    values[0].range = ZeGraphRange { start: 1, count: 5 };
    values[1].tag = 2;
    values[1].integer = 3;
    values[2].tag = 2;
    values[2].integer = 4;
    values[3].tag = 7;
    values[3].range.count = 2;
    let mut children = [1, 2];
    let mut pool: ZeGraphValuePool = sized();
    pool.bytes = bytes.as_ptr();
    pool.byte_count = bytes.len();
    pool.values = values.as_ptr();
    pool.value_count = values.len();
    pool.children = children.as_ptr();
    pool.child_count = children.len();
    let mut bindings: [ZeGraphParameterValue; 2] = [sized(); 2];
    bindings[0].name.count = 1;
    bindings[1].name = ZeGraphRange { start: 6, count: 1 };
    bindings[1].value = 3;
    let mut plan_pool: ZeGraphValuePool = sized();
    plan_pool.bytes = b"sv".as_ptr();
    plan_pool.byte_count = 2;
    let mut declarations: [ZeGraphParameter; 2] = [sized(); 2];
    declarations[0].name.count = 1;
    declarations[0].kinds = 16;
    declarations[1].name = ZeGraphRange { start: 1, count: 1 };
    declarations[1].kinds = 128;
    let mut expressions: [ZeGraphExpression; 4] = [sized(); 4];
    for (index, expression) in expressions.iter_mut().enumerate() {
        expression.kind = if index < 2 { 2 } else { 1 };
        expression.value = if index < 2 {
            index as u32
        } else {
            index as u32 + 7
        };
    }
    let mut projections: [ZeGraphProjection; 4] = [sized(); 4];
    for (index, projection) in projections.iter_mut().enumerate() {
        projection.slot = (index % 2) as u32 + 9;
        projection.expression = index as u32;
    }
    let mut operators: [ZeGraphOperator; 3] = [sized(); 3];
    operators[1].kind = 17;
    operators[1].inputs.count = 1;
    operators[1].projections.count = 2;
    operators[2].kind = 16;
    operators[2].inputs = ZeGraphRange { start: 1, count: 1 };
    operators[2].projections = ZeGraphRange { start: 2, count: 2 };
    let inputs = [0, 1];
    let mut plan: ZeGraphPlan = sized();
    plan.root = 2;
    plan.pool = &plan_pool;
    plan.parameters = declarations.as_ptr();
    plan.parameter_count = 2;
    plan.expressions = expressions.as_ptr();
    plan.expression_count = 4;
    plan.operators = operators.as_ptr();
    plan.operator_count = 3;
    plan.projections = projections.as_ptr();
    plan.projection_count = 4;
    plan.inputs = inputs.as_ptr();
    plan.input_count = 2;
    let mut query: ZeGraphQueryRequest = sized();
    query.plan = &plan;
    query.parameters = bindings.as_ptr();
    query.parameter_count = 2;
    query.parameter_pool = &pool;
    let mut response = Response::new();
    status(ze_store_graph_query(handle, &query, &mut response.0))?;
    bytes.fill(0);
    values.fill(sized());
    children.fill(0);
    Ok(response)
}
fn run(seed: u64) -> Result<Observed, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path = dir.path().join("graph");
    let bytes = path.to_string_lossy().into_owned().into_bytes();
    let mut open: ZeGraphOpenRequest = sized();
    open.path = ZeGraphBytes {
        data: bytes.as_ptr(),
        count: bytes.len(),
    };
    open.max_resident_bytes = 256 << 20;
    let mut store = Store(0);
    status(store_open(&open, &mut store.0))?;
    let mut value: ZeGraphValue = sized();
    value.tag = 2;
    value.integer = (seed % 31) as i64 + 17;
    let mut pool: ZeGraphValuePool = sized();
    pool.bytes = b"x".as_ptr();
    pool.byte_count = 1;
    pool.values = &value;
    pool.value_count = 1;
    let mut binding: ZeGraphParameterValue = sized();
    binding.name.count = 1;
    let text = b"CREATE (n:CEntry {v:$x}) RETURN n";
    let mut cypher: ZeGraphCypherRequest = sized();
    cypher.query = ZeGraphBytes {
        data: text.as_ptr(),
        count: text.len(),
    };
    cypher.parameters = &binding;
    cypher.parameter_count = 1;
    cypher.parameter_pool = &pool;
    let mut write = Response::new();
    status(ze_store_cypher(store.0, &cypher, &mut write.0))?;
    if write.0.pool.node_count != 1 || write.0.pool.nodes.is_null() {
        return Err("C entry write node geometry".into());
    }
    let node = unsafe { (*write.0.pool.nodes).id };
    let (changed, disposition) = (write.0.changed_generation, write.0.disposition);
    write.free()?;
    pool.values = std::ptr::null();
    pool.value_count = 0;
    pool.bytes = b"v".as_ptr();
    let mut expressions: [ZeGraphExpression; 2] = [sized(); 2];
    expressions[0].kind = 1;
    expressions[0].value = 5;
    expressions[1].kind = 5;
    expressions[1].name.count = 1;
    let mut projection: ZeGraphProjection = sized();
    projection.slot = 9;
    projection.expression = 1;
    let mut operators: [ZeGraphOperator; 2] = [sized(); 2];
    operators[0].kind = 4;
    operators[0].node_slot = 5;
    operators[1].kind = 16;
    operators[1].inputs.count = 1;
    operators[1].projections.count = 1;
    let input = [0];
    let mut plan: ZeGraphPlan = sized();
    plan.root = 1;
    plan.operators = operators.as_ptr();
    plan.operator_count = 2;
    plan.expressions = expressions.as_ptr();
    plan.expression_count = 2;
    plan.projections = &projection;
    plan.projection_count = 1;
    plan.inputs = input.as_ptr();
    plan.input_count = 1;
    plan.pool = &pool;
    let mut query: ZeGraphQueryRequest = sized();
    query.plan = &plan;
    let mut read = Response::new();
    status(ze_store_graph_query(store.0, &query, &mut read.0))?;
    let values = read.values()?;
    if values.len() != 1 || values.first().map(|v| v.tag) != Some(2) {
        return Err("C entry scalar shape".into());
    }
    let scalar = values
        .first()
        .ok_or_else(|| "C scalar absent".to_string())?
        .integer;
    let admitted = read.0.admitted_generation;
    read.free()?;
    let ids = [
        node,
        ZeNodeId {
            high: u64::MAX,
            low: node.low,
        },
        node,
    ];
    let mut get: ZeGraphGetNodesRequest = sized();
    get.ids = ids.as_ptr();
    get.id_count = ids.len();
    status(ze_store_get_nodes(store.0, &get, &mut read.0))?;
    let tags = read.values()?.iter().map(|v| v.tag).collect();
    read.free()?;
    let relationship_ids = [ZeRelId {
        high: u64::MAX,
        low: node.low,
    }];
    let mut relationships: ZeGraphGetRelsRequest = sized();
    relationships.ids = relationship_ids.as_ptr();
    relationships.id_count = 1;
    status(ze_store_get_relationships(
        store.0,
        &relationships,
        &mut read.0,
    ))?;
    if read.values()?.iter().map(|v| v.tag).collect::<Vec<_>>() != [0] {
        return Err("C entry foreign relationship was present".into());
    }
    let mut retained = retained_parameters(store.0)?;
    // Ownership must survive closing the producing handle.
    status(ze_close(store.0))?;
    store.0 = 0;
    read.free()?;
    let parameter_values = retained.values()?;
    let string = parameter_values.first().ok_or("retained string absent")?;
    let list = parameter_values.get(1).ok_or("retained list absent")?;
    let pool = &retained.0.pool;
    if parameter_values.len() != 2
        || string.tag != 4
        || list.tag != 7
        || pool.bytes.is_null()
        || pool.children.is_null()
    {
        return Err("retained parameter geometry".into());
    }
    let bytes = unsafe { std::slice::from_raw_parts(pool.bytes, pool.byte_count) };
    let parameter_string = bytes
        .get(string.range.start as usize..(string.range.start + string.range.count) as usize)
        .ok_or("retained string span")?
        .to_vec();
    let children = unsafe { std::slice::from_raw_parts(pool.children, pool.child_count) };
    let values = unsafe { std::slice::from_raw_parts(pool.values, pool.value_count) };
    let mut parameter_numbers = Vec::new();
    for index in children
        .get(list.range.start as usize..(list.range.start + list.range.count) as usize)
        .ok_or("retained list span")?
    {
        let value = values.get(*index as usize).ok_or("retained list child")?;
        if value.tag != 2 {
            return Err("retained integer tag".into());
        }
        parameter_numbers.push(value.integer);
    }
    let parameter_tags = parameter_values.iter().map(|v| v.tag).collect();
    retained.free()?;
    Ok(Observed {
        parameter_string,
        parameter_numbers,
        parameter_tags,
        scalar,
        tags,
        admitted,
        changed,
        disposition,
    })
}
pub fn probe(seed: u64, coverage: &mut CoverageRegistry) -> Result<(), String> {
    bindings::semantics();
    for (_, _, expected) in bindings::cases() {
        if !expected.is_empty()
            && bindings::compare("planted", &expected, &format!("{expected}N")).is_ok()
        {
            return Err("ZE-72 shared comparator missed planted discrepancy".into());
        }
    }
    #[cfg(feature = "graph-result-test-support")]
    binding_faults(coverage)?;
    let observed = run(seed)?;
    oracle::compare(seed, &observed)?;
    let mut planted = observed.clone();
    planted.scalar ^= 1;
    if oracle::compare(seed, &planted).is_ok() {
        return Err("C oracle did not catch planted scalar discrepancy".into());
    }
    let mut planted = observed.clone();
    planted.parameter_numbers.push(0);
    if oracle::compare(seed, &planted).is_ok() {
        return Err("C oracle did not catch planted parameter discrepancy".into());
    }
    let control = run(seed)?;
    oracle::compare(seed, &control)?;
    if observed != control {
        return Err("C entry same-seed control changed".into());
    }
    for key in REQUIRED_COVERAGE {
        coverage.hit(key);
    }
    Ok(())
}

#[cfg(feature = "graph-result-test-support")]
pub const BINDING_FAULT_COVERAGE: [&str; 7] = [
    "property-graph.c-entry.allocation.fire",
    "property-graph.c-entry.append.fire",
    "property-graph.c-entry.sync.fire",
    "property-graph.c-entry.cancel-after-sync.fire",
    "property-graph.c-entry.panic-after-sync.fire",
    "property-graph.c-entry.panic-known-commit.fire",
    "property-graph.c-entry.fault.same-seed-control",
];
#[cfg(feature = "graph-result-test-support")]
fn binding_faults(coverage: &mut CoverageRegistry) -> Result<(), String> {
    for mode in 1..=6 {
        let actual = bindings::fault(mode);
        oracle::compare_binding(mode, &actual)?;
        let mut planted = actual.clone();
        planted.disposition ^= 1;
        if oracle::compare_binding(mode, &planted).is_ok() {
            return Err(format!("ZE-72 comparator missed mode {mode}"));
        }
        oracle::compare_binding(0, &bindings::fault(0))?;
        println!(
            "ZE72 C mode={mode} fires={} recovered={} comparator=RED control=GREEN terminal=GREEN",
            actual.fires, actual.recovered_nodes
        );
    }
    for key in BINDING_FAULT_COVERAGE {
        coverage.hit(key);
    }
    Ok(())
}

pub fn store_open(request: &ZeGraphOpenRequest, out: &mut ZeHandle) -> ZeErrorCode {
    let open = ZeOpenRequest {
        abi_size: std::mem::size_of::<ZeOpenRequest>() as u32,
        abi_reserved: 0,
        path: request.path.data,
        path_len: request.path.count,
        access_mode: i32::from(request.mode == 2),
        durability_mode: 1,
        commit_tier: 2,
        reader_drain_timeout_ms: request.reader_drain_timeout_ms,
        max_resident_bytes: request.max_resident_bytes,
        max_temp_bytes: u64::MAX,
    };
    let code = if request.document_tower.is_null() {
        ze_open(&open, out)
    } else {
        let tower = unsafe { *request.document_tower };
        let epoch = ZeEpochRequest {
            abi_size: std::mem::size_of::<ZeEpochRequest>() as u32,
            abi_reserved: 0,
            embedding: ZeEmbeddingEpoch {
                document: tower,
                query: tower,
                alignment_digest: std::ptr::null(),
                alignment_digest_len: 0,
            },
            tokenizer_profile: 0,
            reserved: 0,
        };
        ze_open_with_epoch(&open, &epoch, out)
    };
    if code != ZeErrorCode::ZeOk || request.mode != 0 {
        return code;
    }
    let mut report = sized();
    ze_store_enable_graph(*out, &mut report)
}
