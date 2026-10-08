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
/// Opens through the ordinary Store API; graph enable is explicit on creation.
pub fn store_open(request: &ZeGraphOpenRequest, out: &mut ZeHandle) -> ZeErrorCode {
    let open = ZeOpenRequest {
        abi_size: size_of::<ZeOpenRequest>() as u32,
        abi_reserved: 0,
        path: request.path.data,
        path_len: request.path.count,
        access_mode: i32::from(request.mode == MODE_READ_ONLY),
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
            abi_size: size_of::<ZeEpochRequest>() as u32,
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
    if code != ZeErrorCode::ZeOk || request.mode != MODE_CREATE {
        return code;
    }
    let mut report = sized_zeroed();
    ze_store_enable_graph(*out, &mut report)
}

pub fn graph_open(path: &Path, mode: u32) -> (ZeErrorCode, ZeHandle) {
    let path = path.to_str().expect("UTF-8 path").as_bytes();
    let mut handle = 0;
    (store_open(&open_request(path, mode), &mut handle), handle)
}
pub struct GraphTestStore {
    pub handle: ZeHandle,
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
        let code = ze_close(self.handle);
        self.handle = 0;
        code
    }
}
impl Drop for GraphTestStore {
    fn drop(&mut self) {
        if self.handle != 0 {
            let _ = ze_close(self.handle);
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
    pub fn node_vector(&mut self, node: u32, vector: &[f32]) {
        let image = self.nodes.get_mut(node as usize).expect("node image");
        image.has_vector = 1;
        image.vector = ZeGraphRange {
            start: self.vectors.len() as u32,
            count: vector.len() as u32,
        };
        self.vectors.extend_from_slice(vector);
    }
    pub fn empty_list(&mut self, kind: u32) -> u32 {
        let mut value: ZeGraphValue = sized_zeroed();
        value.tag = 7;
        value.list_kind = kind;
        let index = self.values.len() as u32;
        self.values.push(value);
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

pub fn cypher_request(
    text: &[u8],
    params: &[ZeGraphParameterValue],
    pool: Option<&ZeGraphValuePool>,
) -> ZeGraphCypherRequest {
    let mut r: ZeGraphCypherRequest = sized_zeroed();
    r.query = ZeGraphBytes {
        data: text.as_ptr(),
        count: text.len(),
    };
    r.parameters = params.as_ptr();
    r.parameter_count = params.len();
    r.parameter_pool = pool.map_or(std::ptr::null(), |p| p);
    r
}
pub fn parameter(pool: &mut PoolBuilder, name: &str, value: u32) -> ZeGraphParameterValue {
    let mut p: ZeGraphParameterValue = sized_zeroed();
    p.name = pool.text(name);
    p.value = value;
    p
}
fn response_slice<T>(_response: &ZeGraphResponse, p: *const T, n: usize) -> &[T] {
    if n == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(p, n) }
    }
}
pub fn rows(r: &ZeGraphResponse) -> Vec<Vec<ZeGraphValue>> {
    if r.column_count == 0 {
        return vec![];
    }
    let values = response_slice(r, r.pool.values, r.pool.value_count);
    response_slice(r, r.cells, r.cell_count)
        .chunks(r.column_count)
        .map(|row| row.iter().map(|i| values[*i as usize]).collect())
        .collect()
}
fn range_string(r: &ZeGraphResponse, range: ZeGraphRange) -> String {
    let bytes = response_slice(r, r.pool.bytes, r.pool.byte_count);
    String::from_utf8(bytes[range.start as usize..(range.start + range.count) as usize].to_vec())
        .unwrap()
}
pub fn string_of(r: &ZeGraphResponse, v: &ZeGraphValue) -> String {
    range_string(r, v.range)
}
pub fn column_names(r: &ZeGraphResponse) -> Vec<String> {
    response_slice(r, r.columns, r.column_count)
        .iter()
        .map(|c| range_string(r, c.name))
        .collect()
}
pub fn cypher_ok(handle: ZeHandle, text: &str) -> ZeGraphResponse {
    let mut r = empty_response();
    assert_eq!(
        ze_store_cypher(handle, &cypher_request(text.as_bytes(), &[], None), &mut r),
        ZeErrorCode::ZeOk,
        "{}",
        last_error(handle)
    );
    r
}

impl PoolBuilder {
    pub fn tagged_value(&mut self, tag: u32) -> u32 {
        let mut v: ZeGraphValue = sized_zeroed();
        v.tag = tag;
        let index = self.values.len() as u32;
        self.values.push(v);
        index
    }
}
