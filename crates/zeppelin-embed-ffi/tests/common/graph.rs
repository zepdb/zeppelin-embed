use super::*;
pub const MODE_CREATE: u32 = 0;
pub const MODE_READ_WRITE: u32 = 1;
pub const MODE_READ_ONLY: u32 = 2;

pub fn open_request(path: &[u8], mode: u32) -> ZeGraphOpenRequest {
    ZeGraphOpenRequest {
        abi_size: size_of::<ZeGraphOpenRequest>() as u32,
        abi_reserved: 0,
        path: ZeGraphBytes {
            data: path.as_ptr(),
            count: path.len(),
        },
        mode,
        tokenizer_profile: 0,
        document_tower: std::ptr::null(),
        reader_drain_timeout_ms: 250,
        max_resident_bytes: 256 << 20,
        control: std::ptr::null(),
    }
}
pub fn graph_open(path: &Path, mode: u32) -> (ZeErrorCode, ZeGraphHandle) {
    let path = path.to_str().expect("UTF-8 path").as_bytes();
    let mut handle = ZeGraphHandle { token: 0 };
    (
        ze_graph_open(&open_request(path, mode), &mut handle),
        handle,
    )
}
pub struct GraphTestStore {
    pub handle: ZeGraphHandle,
    pub path: PathBuf,
    dir: Option<TempDir>,
}
impl GraphTestStore {
    pub fn create() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph");
        let (code, handle) = graph_open(&path, MODE_CREATE);
        assert_eq!(code, ZeErrorCode::ZeOk);
        Self {
            handle,
            path,
            dir: Some(dir),
        }
    }
    pub fn close(&mut self) -> ZeErrorCode {
        let code = ze_graph_close(self.handle);
        self.handle.token = 0;
        code
    }
}
impl Drop for GraphTestStore {
    fn drop(&mut self) {
        if self.handle.token != 0 {
            let _ = ze_graph_close(self.handle);
        }
        let _ = self.dir.take();
    }
}
pub fn last_error(token: u64) -> String {
    let mut length = 0;
    assert_eq!(
        ze_last_error_message(token, std::ptr::null_mut(), 0, &mut length),
        ZeErrorCode::ZeOk
    );
    let mut bytes = vec![0u8; length + 1];
    assert_eq!(
        ze_last_error_message(token, bytes.as_mut_ptr().cast(), bytes.len(), &mut length),
        ZeErrorCode::ZeOk
    );
    bytes.truncate(length);
    String::from_utf8(bytes).unwrap()
}

#[derive(Default)]
pub struct PoolBuilder {
    bytes: Vec<u8>,
    values: Vec<ZeGraphValue>,
    children: Vec<u32>,
    names: Vec<ZeGraphRange>,
    properties: Vec<ZeGraphProperty>,
    nodes: Vec<ZeGraphNode>,
    relationships: Vec<ZeGraphRelationship>,
    vectors: Vec<f32>,
}
impl PoolBuilder {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn text(&mut self, s: &str) -> ZeGraphRange {
        let start = self.bytes.len() as u32;
        self.bytes.extend_from_slice(s.as_bytes());
        ZeGraphRange {
            start,
            count: s.len() as u32,
        }
    }
    pub fn string_value(&mut self, s: &str) -> u32 {
        let mut v: ZeGraphValue = sized_zeroed();
        v.tag = 4;
        v.range = self.text(s);
        let index = self.values.len() as u32;
        self.values.push(v);
        index
    }
    pub fn i64_value(&mut self, integer: i64) -> u32 {
        let mut v: ZeGraphValue = sized_zeroed();
        v.tag = 2;
        v.integer = integer;
        let index = self.values.len() as u32;
        self.values.push(v);
        index
    }
    pub fn property(&mut self, name: &str, value: u32) -> u32 {
        let mut p: ZeGraphProperty = sized_zeroed();
        p.name = self.text(name);
        p.value = value;
        let index = self.properties.len() as u32;
        self.properties.push(p);
        index
    }
    pub fn node_image(
        &mut self,
        labels: &[&str],
        properties: std::ops::Range<u32>,
        text: Option<&str>,
    ) -> u32 {
        let mut n: ZeGraphNode = sized_zeroed();
        n.labels = ZeGraphRange {
            start: self.names.len() as u32,
            count: labels.len() as u32,
        };
        for label in labels {
            let name = self.text(label);
            self.names.push(name);
        }
        n.properties = span(properties);
        if let Some(text) = text {
            n.has_text = 1;
            n.text = self.text(text);
        }
        let index = self.nodes.len() as u32;
        self.nodes.push(n);
        index
    }
    pub fn relationship_image(&mut self, rel_type: &str, properties: std::ops::Range<u32>) -> u32 {
        let mut r: ZeGraphRelationship = sized_zeroed();
        r.relationship_type = self.text(rel_type);
        r.properties = span(properties);
        let index = self.relationships.len() as u32;
        self.relationships.push(r);
        index
    }
    /// Call last; this builder must outlive the FFI call.
    pub fn pool(&self) -> ZeGraphValuePool {
        ZeGraphValuePool {
            abi_size: size_of::<ZeGraphValuePool>() as u32,
            abi_reserved: 0,
            bytes: self.bytes.as_ptr(),
            byte_count: self.bytes.len(),
            values: self.values.as_ptr(),
            value_count: self.values.len(),
            children: self.children.as_ptr(),
            child_count: self.children.len(),
            names: self.names.as_ptr(),
            name_count: self.names.len(),
            properties: self.properties.as_ptr(),
            property_count: self.properties.len(),
            nodes: self.nodes.as_ptr(),
            node_count: self.nodes.len(),
            relationships: self.relationships.as_ptr(),
            relationship_count: self.relationships.len(),
            vectors: self.vectors.as_ptr(),
            vector_count: self.vectors.len(),
        }
    }
}
fn span(r: std::ops::Range<u32>) -> ZeGraphRange {
    ZeGraphRange {
        start: r.start,
        count: r.end - r.start,
    }
}
pub fn unused_endpoint() -> ZeGraphEndpoint {
    sized_zeroed()
}
pub fn local_endpoint(item: u32) -> ZeGraphEndpoint {
    let mut e = unused_endpoint();
    e.kind = 2;
    e.local_item = item;
    e
}
pub fn node_endpoint(id: ZeNodeId) -> ZeGraphEndpoint {
    let mut e = unused_endpoint();
    e.kind = 1;
    e.node = id;
    e
}
pub fn create_node_item(
    ns: ZeGraphRange,
    key: ZeGraphRange,
    revision: u64,
    image: u32,
) -> ZeGraphBatchItem {
    let mut i: ZeGraphBatchItem = sized_zeroed();
    i.namespace_name = ns;
    i.key = key;
    i.revision = revision;
    i.has_image = 1;
    i.image = image;
    i.source = unused_endpoint();
    i.target = unused_endpoint();
    i
}
pub fn create_rel_item(
    ns: ZeGraphRange,
    key: ZeGraphRange,
    revision: u64,
    image: u32,
    source: ZeGraphEndpoint,
    target: ZeGraphEndpoint,
) -> ZeGraphBatchItem {
    let mut i = create_node_item(ns, key, revision, image);
    i.entity_kind = 1;
    i.source = source;
    i.target = target;
    i
}
pub fn batch_request(items: &[ZeGraphBatchItem], pool: &ZeGraphValuePool) -> ZeGraphBatchRequest {
    let mut r: ZeGraphBatchRequest = sized_zeroed();
    r.items = items.as_ptr();
    r.item_count = items.len();
    r.pool = pool;
    r
}
pub fn empty_response() -> ZeGraphResponse {
    let mut r: ZeGraphResponse = sized_zeroed();
    r.pool.abi_size = 136;
    r
}
pub fn receipts(r: &ZeGraphResponse) -> &[ZeGraphReceipt] {
    if r.receipt_count == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(r.receipts, r.receipt_count) }
    }
}
pub fn single_node(builder: &mut PoolBuilder, key: &str) -> ZeGraphBatchItem {
    let ns = builder.text("docs");
    let key = builder.text(key);
    let image = builder.node_image(&[], 0..0, None);
    create_node_item(ns, key, 1, image)
}
