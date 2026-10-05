//! Bounded tooling adapter. The fixture expectations are authored separately.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use zeppelin_embed_ffi::*;
pub const FIXTURE: &str = include_str!("../fixtures/graph-bindings-v1/semantics.tsv");
pub fn cases() -> Vec<(&'static str, &'static str, String)> {
    FIXTURE
        .lines()
        .filter(|s| !s.starts_with('#'))
        .map(|s| {
            let fields: Vec<_> = s.split('\t').collect();
            assert_eq!(fields.len(), 3, "malformed ZE-72 fixture");
            (fields[0], fields[1], fields[2].replace("\\n", "\n"))
        })
        .collect()
}
pub fn compare(name: &str, expected: &str, actual: &str) -> Result<(), String> {
    if expected == actual {
        Ok(())
    } else {
        Err(format!(
            "ZE-72 graph-bindings-v1 {name}: expected {expected:?}, observed {actual:?}"
        ))
    }
}
unsafe fn span<'a, T>(p: *const T, n: usize) -> &'a [T] {
    if n == 0 {
        &[]
    } else {
        assert!(!p.is_null());
        unsafe { std::slice::from_raw_parts(p, n) }
    }
}
pub fn c_cell(r: &ZeGraphResponse, v: ZeGraphValue) -> String {
    let p = &r.pool;
    match v.tag {
        0 => "N".into(),
        1 => format!("B{}", v.boolean),
        2 => format!("I{}", v.integer),
        3 => format!("F{:016x}", v.floating.to_bits()),
        4 => {
            let bytes = unsafe { span(p.bytes, p.byte_count) };
            let s = std::str::from_utf8(
                &bytes[v.range.start as usize..(v.range.start + v.range.count) as usize],
            )
            .unwrap();
            format!("S{}:{s}", s.len())
        }
        5 => {
            let n = unsafe { span(p.nodes, p.node_count) }[v.entity_index as usize];
            format!("D{:016x}{:016x}", n.id.high, n.id.low)
        }
        6 => {
            let n = unsafe { span(p.relationships, p.relationship_count) }[v.entity_index as usize];
            format!("R{:016x}{:016x}", n.id.high, n.id.low)
        }
        7 => {
            let children = unsafe { span(p.children, p.child_count) };
            let values = unsafe { span(p.values, p.value_count) };
            let parts: Vec<_> = children
                [v.range.start as usize..(v.range.start + v.range.count) as usize]
                .iter()
                .map(|i| c_cell(r, values[*i as usize]))
                .collect();
            format!("L{}[{}]", v.list_kind, parts.join(","))
        }
        _ => panic!("unknown public value tag {}", v.tag),
    }
}
pub fn c_observe(r: &ZeGraphResponse) -> String {
    assert_eq!(r.cell_count, r.row_count * r.column_count);
    if r.cell_count == 0 {
        return String::new();
    }
    let cells = unsafe { span(r.cells, r.cell_count) };
    let values = unsafe { span(r.pool.values, r.pool.value_count) };
    cells
        .chunks(r.column_count)
        .map(|row| {
            row.iter()
                .map(|i| c_cell(r, values[*i as usize]))
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}
pub fn sized<T>() -> T {
    let mut v: T = unsafe { std::mem::zeroed() };
    unsafe {
        (&mut v as *mut T)
            .cast::<u32>()
            .write(std::mem::size_of::<T>() as u32);
    }
    v
}
pub fn query(handle: ZeGraphHandle, text: &str) -> ZeGraphResponse {
    let mut request: ZeGraphCypherRequest = sized();
    request.query = ZeGraphBytes {
        data: text.as_ptr(),
        count: text.len(),
    };
    let mut response = sized();
    assert_eq!(
        ze_graph_cypher(handle, &request, &mut response),
        ZeErrorCode::ZeOk
    );
    response
}
#[cfg(feature = "graph-result-test-support")]
pub fn fault(mode: u32) -> zeppelin_embed_adversarial_oracle::graph_c_entry::BindingOutcome {
    use zeppelin_embed_ffi::graph_bindings_test_support::ze72_test_cypher;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fault");
    let path = path.to_str().unwrap().as_bytes();
    let mut open: ZeGraphOpenRequest = sized();
    open.path = ZeGraphBytes {
        data: path.as_ptr(),
        count: path.len(),
    };
    open.max_resident_bytes = 256 << 20;
    let mut handle = ZeGraphHandle { token: 0 };
    assert_eq!(ze_graph_open(&open, &mut handle), ZeErrorCode::ZeOk);
    let mut request: ZeGraphCypherRequest = sized();
    let text = "CREATE (:Fault)";
    request.query = ZeGraphBytes {
        data: text.as_ptr(),
        count: text.len(),
    };
    let mut response: ZeGraphResponse = sized();
    let mut fires = 0;
    let status = unsafe { ze72_test_cypher(handle, &request, &mut response, mode, &mut fires) };
    let (disposition, changed) = (response.disposition, response.changed_generation);
    assert_eq!(ze_graph_response_free(&mut response), ZeErrorCode::ZeOk);
    if mode == 5 || mode == 6 {
        let mut read: ZeGraphResponse = sized();
        assert_eq!(
            ze_graph_cypher(handle, &request, &mut read),
            ZeErrorCode::ZeErrPoisoned
        );
        ze_graph_response_free(&mut read);
    }
    let code = ze_graph_close(handle);
    assert!(matches!(
        code,
        ZeErrorCode::ZeOk | ZeErrorCode::ZeErrPoisoned
    ));
    open.mode = 1;
    assert_eq!(ze_graph_open(&open, &mut handle), ZeErrorCode::ZeOk);
    let mut recovered = query(handle, "MATCH (n:Fault) RETURN count(n)");
    let value = unsafe { *recovered.pool.values.add(*recovered.cells as usize) }.integer;
    assert_eq!(ze_graph_close(handle), ZeErrorCode::ZeOk);
    assert_eq!(ze_graph_response_free(&mut recovered), ZeErrorCode::ZeOk);
    zeppelin_embed_adversarial_oracle::graph_c_entry::BindingOutcome {
        status: format!("{status:?}"),
        disposition,
        changed,
        fires,
        recovered_nodes: value,
    }
}
pub fn semantics() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("semantics");
    let bytes = path.to_str().unwrap().as_bytes();
    let mut open: ZeGraphOpenRequest = sized();
    open.path = ZeGraphBytes {
        data: bytes.as_ptr(),
        count: bytes.len(),
    };
    open.max_resident_bytes = 256 << 20;
    let mut handle = ZeGraphHandle { token: 0 };
    assert_eq!(ze_graph_open(&open, &mut handle), ZeErrorCode::ZeOk);
    let pool: ZeGraphValuePool = sized();
    let mut batch: ZeGraphBatchRequest = sized();
    batch.pool = &pool;
    let mut noop: ZeGraphResponse = sized();
    assert_eq!(ze_graph_apply(handle, &batch, &mut noop), ZeErrorCode::ZeOk);
    assert_eq!(noop.disposition, 4);
    assert_eq!(noop.has_changed_generation, 0);
    ze_graph_response_free(&mut noop);
    let mut created = query(handle, "CREATE (:Fixture), (:Fixture)");
    ze_graph_response_free(&mut created);
    let mut retained = Vec::new();
    for (name, q, expected) in cases() {
        let r = query(handle, q);
        compare(name, &expected, &c_observe(&r)).unwrap();
        retained.push((name, expected, r));
    }
    assert_eq!(ze_graph_close(handle), ZeErrorCode::ZeOk);
    for (name, expected, mut r) in retained {
        compare(name, &expected, &c_observe(&r)).unwrap();
        assert_eq!(ze_graph_response_free(&mut r), ZeErrorCode::ZeOk);
    }
}
pub fn property_cases() -> Vec<(&'static str, u32, &'static str)> {
    include_str!("../fixtures/graph-bindings-v1/stored-properties.tsv")
        .lines()
        .filter(|s| !s.starts_with('#'))
        .map(|s| {
            let fields = s.split('\t').collect::<Vec<_>>();
            assert_eq!(fields.len(), 3);
            (fields[0], fields[1].parse().unwrap(), fields[2])
        })
        .collect()
}
