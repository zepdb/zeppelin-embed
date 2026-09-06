mod common;

use std::fmt::Write as _;
use std::mem::size_of;

use zeppelin_embed_ffi::*;

const SEED: u64 = 25_172_023;
const DIMENSION: usize = 4;
const TEXTS: [&str; 4] = [
    "zeppelin airship over the harbour",
    "harbour lights at dusk",
    "quantized vectors and postings",
    "epoch aligned harbour search",
];

#[derive(Clone, Copy)]
struct FixtureHit {
    doc_id: ZeDocId,
    score: f64,
    vector_squared_l2: Option<f64>,
    lexical_bm25: Option<f64>,
}

struct FixtureQuery {
    generation: u64,
    mode: i32,
    embedding_epoch: Option<u64>,
    tokenizer_epoch: Option<u64>,
    hits: Vec<FixtureHit>,
}

fn seeded_vectors() -> Vec<Vec<f32>> {
    let mut state = SEED;
    (0..4)
        .map(|_| {
            (0..DIMENSION)
                .map(|_| {
                    state = state
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    (((state >> 32) % 16) + 1) as f32 / 16.0
                })
                .collect()
        })
        .collect()
}

fn code_name(code: ZeErrorCode) -> &'static str {
    let pointer = ze_error_code_name(code as i32);
    unsafe { std::ffi::CStr::from_ptr(pointer) }
        .to_str()
        .expect("error code name is ASCII")
}

fn run_query(
    handle: ZeHandle,
    vector: Option<&[f32]>,
    text: Option<&str>,
    rules_enabled: bool,
) -> (ZeErrorCode, Option<FixtureQuery>) {
    let vector = vector.unwrap_or(&[]);
    let text = text.unwrap_or("");
    let mut request = common::valid_query_request(vector);
    request.text = if text.is_empty() {
        std::ptr::null()
    } else {
        text.as_ptr()
    };
    request.text_len = text.len();
    request.k = 4;
    request.thread_budget = 1;
    request.rules_enabled = u32::from(rules_enabled);
    let mut result: ZeQueryResult = common::sized_zeroed();
    let code = ze_query(handle, &request, &mut result);
    if code != ZeErrorCode::ZeOk {
        return (code, None);
    }
    let hits = unsafe { std::slice::from_raw_parts(result.hits, result.hit_count) }
        .iter()
        .map(|hit| FixtureHit {
            doc_id: hit.doc_id,
            score: hit.score,
            vector_squared_l2: (hit.has_vector_score == 1).then_some(hit.vector_squared_l2),
            lexical_bm25: (hit.has_lexical_score == 1).then_some(hit.lexical_bm25),
        })
        .collect();
    let fixture = FixtureQuery {
        generation: result.generation,
        mode: result.mode,
        embedding_epoch: (result.has_embedding_epoch == 1).then_some(result.embedding_epoch),
        tokenizer_epoch: (result.has_tokenizer_epoch == 1).then_some(result.tokenizer_epoch),
        hits,
    };
    assert_eq!(ze_query_result_free(&mut result), ZeErrorCode::ZeOk);
    (code, Some(fixture))
}

fn optional_u64(value: Option<u64>) -> String {
    value.map_or_else(|| "null".to_owned(), |number| number.to_string())
}

fn optional_score(value: Option<f64>) -> String {
    value.map_or_else(|| "null".to_owned(), |number| format!("{number:.6}"))
}

fn vector_json(vector: &[f32]) -> String {
    let values = vector
        .iter()
        .map(|value| format!("{value:.6}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{values}]")
}

fn query_json(
    name: &str,
    vector: Option<&[f32]>,
    text: Option<&str>,
    rules_enabled: bool,
    code: ZeErrorCode,
    query: &FixtureQuery,
) -> String {
    let vector = vector.map_or_else(|| "null".to_owned(), vector_json);
    let text = text.map_or_else(|| "null".to_owned(), |value| format!("\"{value}\""));
    let hits = query
        .hits
        .iter()
        .map(|hit| {
            format!(
                concat!(
                    "{{\"doc_id\": {{\"high\": {}, \"low\": {}}}, ",
                    "\"score\": {:.6}, \"vector_squared_l2\": {}, ",
                    "\"lexical_bm25\": {}}}"
                ),
                hit.doc_id.high,
                hit.doc_id.low,
                hit.score,
                optional_score(hit.vector_squared_l2),
                optional_score(hit.lexical_bm25),
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        concat!(
            "    {{\"kind\": \"query\", \"name\": \"{}\", ",
            "\"request\": {{\"vector\": {}, \"text\": {}, \"k\": 4, ",
            "\"tier\": null, \"rules_enabled\": {}}}, ",
            "\"expected\": {{\"error_code\": \"{}\", \"generation\": {}, ",
            "\"mode\": {}, \"embedding_epoch\": {}, \"tokenizer_epoch\": {}, ",
            "\"hits\": [{}]}}}}"
        ),
        name,
        vector,
        text,
        rules_enabled,
        code_name(code),
        query.generation,
        query.mode,
        optional_u64(query.embedding_epoch),
        optional_u64(query.tokenizer_epoch),
        hits,
    )
}

fn generate_fixture() -> String {
    let vectors = seeded_vectors();
    let epoch = common::EpochFixture::new(DIMENSION as u32);
    let epoch_request = epoch.request();
    let mut identity: ZeEpochIdentity = common::sized_zeroed();
    let identity_code = ze_epoch_identity(&epoch_request, &mut identity);
    assert_eq!(identity_code, ZeErrorCode::ZeOk);

    let directory = tempfile::tempdir().expect("parity temporary directory");
    let path = directory.path().join("store");
    let (open_code, handle) = common::open_path_with_epoch(&path, &epoch);
    assert_eq!(open_code, ZeErrorCode::ZeOk);

    let metadata = [b"alpha".as_slice(), b"bravo", b"charlie", b"delta"];
    let documents = vectors
        .iter()
        .zip(TEXTS)
        .zip(metadata)
        .enumerate()
        .map(|(index, ((vector, text), metadata))| ZeIngestDocument {
            abi_size: size_of::<ZeIngestDocument>() as u32,
            abi_reserved: 0,
            doc_id: ZeDocId {
                high: 0,
                low: index as u64 + 1,
            },
            revision: 1,
            timestamp: 100 + index as i64 * 10,
            vector: vector.as_ptr(),
            vector_len: vector.len(),
            metadata: metadata.as_ptr(),
            metadata_len: metadata.len(),
            text: text.as_ptr(),
            text_len: text.len(),
        })
        .collect::<Vec<_>>();
    let ingest = ZeIngestRequest {
        abi_size: size_of::<ZeIngestRequest>() as u32,
        abi_reserved: 0,
        documents: documents.as_ptr(),
        document_count: documents.len(),
        dimension: DIMENSION,
    };
    let mut mutation: ZeMutationReport = common::sized_zeroed();
    let ingest_code = ze_ingest(handle, &ingest, &mut mutation);
    assert_eq!(ingest_code, ZeErrorCode::ZeOk);

    let (vector_code, vector_query) = run_query(handle, Some(&vectors[1]), None, false);
    let vector_query = vector_query.expect("vector parity query");
    let (lexical_code, lexical_query) = run_query(handle, None, Some("harbour"), false);
    let lexical_query = lexical_query.expect("lexical parity query");
    let (hybrid_code, hybrid_query) = run_query(handle, Some(&vectors[1]), Some("harbour"), true);
    let hybrid_query = hybrid_query.expect("hybrid parity query");
    let (invalid_code, invalid_query) = run_query(handle, None, None, false);
    assert_eq!(invalid_code, ZeErrorCode::ZeErrInvalidArgument);
    assert!(invalid_query.is_none());
    let close_code = ze_close(handle);
    assert_eq!(close_code, ZeErrorCode::ZeOk);

    let namespace_root = directory.path().join("namespaces");
    std::fs::create_dir(&namespace_root).expect("create namespace root");
    let namespace_root_bytes = namespace_root.to_string_lossy().into_owned().into_bytes();
    let namespace_name = b"records";
    let attribute_name = b"rank";
    let attributes = [ZeAttributeDefinition {
        attribute_id: 1,
        name: attribute_name.as_ptr(),
        name_len: attribute_name.len(),
        attribute_type: 1,
        nullable: 0,
    }];
    let namespace_spec = ZeNamespaceSpec {
        abi_size: size_of::<ZeNamespaceSpec>() as u32,
        abi_reserved: 0,
        attributes: attributes.as_ptr(),
        attribute_count: attributes.len(),
        has_vector_space: 1,
        dimensions: 1,
        normalization: 0,
        epoch: std::ptr::null(),
    };
    let namespace_open = ZeNamespaceOpenRequest {
        abi_size: size_of::<ZeNamespaceOpenRequest>() as u32,
        abi_reserved: 0,
        root: namespace_root_bytes.as_ptr(),
        root_len: namespace_root_bytes.len(),
        name: namespace_name.as_ptr(),
        name_len: namespace_name.len(),
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
        spec: &namespace_spec,
    };
    let mut namespace_handle = 0;
    let namespace_open_code = ze_namespace_open(&namespace_open, &mut namespace_handle);
    assert_eq!(namespace_open_code, ZeErrorCode::ZeOk);

    let namespace_list = ZeNamespaceListRequest {
        abi_size: size_of::<ZeNamespaceListRequest>() as u32,
        abi_reserved: 0,
        root: namespace_root_bytes.as_ptr(),
        root_len: namespace_root_bytes.len(),
    };
    let mut namespace_list_result: ZeNamespaceListResult = common::sized_zeroed();
    let namespace_list_code = ze_namespace_list(&namespace_list, &mut namespace_list_result);
    assert_eq!(namespace_list_code, ZeErrorCode::ZeOk);
    let namespace_names = unsafe {
        std::slice::from_raw_parts(
            namespace_list_result.entries,
            namespace_list_result.entry_count,
        )
    }
    .iter()
    .map(|entry| {
        String::from_utf8(
            unsafe { std::slice::from_raw_parts(entry.name, entry.name_len) }.to_vec(),
        )
        .expect("namespace name UTF-8")
    })
    .collect::<Vec<_>>();
    assert_eq!(
        ze_namespace_list_result_free(&mut namespace_list_result),
        ZeErrorCode::ZeOk
    );

    let record_vector = [0.5_f32];
    let record_text = b"parity record";
    let record_metadata = [0xab_u8, 0xcd];
    let record_attributes = [ZeAttributeValue {
        attribute_id: 1,
        value_type: 1,
        u64_value: 7,
        i64_value: 0,
        f64_value: 0.0,
        bool_value: 0,
        string_value: std::ptr::null(),
        string_len: 0,
    }];
    let record = ZeUpsertDocument {
        abi_size: size_of::<ZeUpsertDocument>() as u32,
        abi_reserved: 0,
        document: ZeIngestDocument {
            abi_size: size_of::<ZeIngestDocument>() as u32,
            abi_reserved: 0,
            doc_id: ZeDocId { high: 0, low: 101 },
            revision: 2,
            timestamp: 321,
            vector: record_vector.as_ptr(),
            vector_len: record_vector.len(),
            metadata: record_metadata.as_ptr(),
            metadata_len: record_metadata.len(),
            text: record_text.as_ptr(),
            text_len: record_text.len(),
        },
        attributes: record_attributes.as_ptr(),
        attribute_count: record_attributes.len(),
    };
    let upsert = ZeUpsertRequest {
        abi_size: size_of::<ZeUpsertRequest>() as u32,
        abi_reserved: 0,
        documents: &record,
        document_count: 1,
        dimension: 1,
    };
    let mut upsert_report: ZeMutationReport = common::sized_zeroed();
    let upsert_code = ze_upsert(namespace_handle, &upsert, &mut upsert_report);
    assert_eq!(upsert_code, ZeErrorCode::ZeOk);

    let requested_ids = [ZeDocId { high: 0, low: 101 }, ZeDocId { high: 0, low: 999 }];
    let get = ZeGetRequest {
        abi_size: size_of::<ZeGetRequest>() as u32,
        abi_reserved: 0,
        ids: requested_ids.as_ptr(),
        id_count: requested_ids.len(),
        include_vector: 1,
        include_text: 1,
        include_metadata: 1,
        include_attributes: 1,
    };
    let mut get_result: ZeGetResult = common::sized_zeroed();
    let get_code = ze_get(namespace_handle, &get, &mut get_result);
    assert_eq!(get_code, ZeErrorCode::ZeOk);
    let returned = unsafe { &*get_result.documents };
    let returned_vector =
        unsafe { std::slice::from_raw_parts(returned.vector, returned.vector_len) }.to_vec();
    let returned_text = String::from_utf8(
        unsafe { std::slice::from_raw_parts(returned.text, returned.text_len) }.to_vec(),
    )
    .expect("record text UTF-8");
    let returned_metadata =
        unsafe { std::slice::from_raw_parts(returned.metadata, returned.metadata_len) }.to_vec();
    let returned_attribute = unsafe { *returned.attributes };
    let get_generation = get_result.generation;
    let get_missing_count = get_result.missing_count;
    assert_eq!(ze_get_result_free(&mut get_result), ZeErrorCode::ZeOk);

    let scan = ZeScanRequest {
        abi_size: size_of::<ZeScanRequest>() as u32,
        abi_reserved: 0,
        cursor_generation: 0,
        cursor_segment_id: [0; 16],
        cursor_next_row: 0,
        cursor_phase: 0,
        limit: 10,
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
    let mut scan_result: ZeScanResult = common::sized_zeroed();
    let scan_code = ze_scan(namespace_handle, &scan, &mut scan_result);
    assert_eq!(scan_code, ZeErrorCode::ZeOk);
    let scanned_ids =
        unsafe { std::slice::from_raw_parts(scan_result.documents, scan_result.document_count) }
            .iter()
            .map(|document| document.doc_id.low)
            .collect::<Vec<_>>();
    let scan_generation = scan_result.generation;
    let scan_has_more = scan_result.has_more;
    assert_eq!(ze_scan_result_free(&mut scan_result), ZeErrorCode::ZeOk);

    let filter_values = [record_attributes[0]];
    let filter_nodes = [ZeFilterNode {
        op: 1,
        attribute_id: 1,
        values: filter_values.as_ptr(),
        value_count: filter_values.len(),
        has_lower: 0,
        lower: unsafe { std::mem::zeroed() },
        lower_inclusive: 0,
        has_upper: 0,
        upper: unsafe { std::mem::zeroed() },
        upper_inclusive: 0,
        children_start: 0,
        children_count: 0,
    }];
    let filter = ZeFilter {
        abi_size: size_of::<ZeFilter>() as u32,
        abi_reserved: 0,
        nodes: filter_nodes.as_ptr(),
        node_count: filter_nodes.len(),
        root: 0,
    };
    let count = ZeCountRequest {
        abi_size: size_of::<ZeCountRequest>() as u32,
        abi_reserved: 0,
        filter: &filter,
        has_timestamp_range: 0,
        start_ts: 0,
        end_ts: 0,
    };
    let mut count_result: ZeCountResult = common::sized_zeroed();
    let count_code = ze_count(namespace_handle, &count, &mut count_result);
    assert_eq!(count_code, ZeErrorCode::ZeOk);

    let search = ZeSearchFilteredRequest {
        abi_size: size_of::<ZeSearchFilteredRequest>() as u32,
        abi_reserved: 0,
        search: ZeSearchRequest {
            k: 1,
            tier: 1,
            ..common::valid_search_request(&record_vector)
        },
        filter: &filter,
    };
    let mut filtered_result: ZeSearchResult = common::sized_zeroed();
    let search_filtered_code = ze_search_filtered(namespace_handle, &search, &mut filtered_result);
    assert_eq!(search_filtered_code, ZeErrorCode::ZeOk);
    let filtered_hit = unsafe { *filtered_result.hits };
    let filtered_generation = filtered_result.generation;
    assert_eq!(
        ze_search_result_free(&mut filtered_result),
        ZeErrorCode::ZeOk
    );
    assert_eq!(ze_close(namespace_handle), ZeErrorCode::ZeOk);

    let mut json = String::new();
    writeln!(json, "{{").expect("write JSON");
    writeln!(
        json,
        "  \"schema\": \"zeppelin-embed-cross-binding-parity\","
    )
    .expect("write JSON");
    writeln!(json, "  \"version\": 1,").expect("write JSON");
    writeln!(json, "  \"seed\": {SEED},").expect("write JSON");
    writeln!(json, "  \"score_precision\": 6,").expect("write JSON");
    writeln!(json, "  \"epoch\": {{").expect("write JSON");
    writeln!(json, "    \"document\": {{\"model_id\": \"ffi-embedding\", \"model_version\": \"1\", \"weights_digest_hex\": \"ad12\", \"dims\": 4, \"normalization\": 0, \"prompt_prefix\": \"search_document: \", \"max_tokens\": 64, \"runtime\": 3, \"compute_units\": 1, \"os_build\": null}},").expect("write JSON");
    writeln!(json, "    \"query\": {{\"model_id\": \"ffi-embedding\", \"model_version\": \"1\", \"weights_digest_hex\": \"ad12\", \"dims\": 4, \"normalization\": 0, \"prompt_prefix\": \"search_query: \", \"max_tokens\": 64, \"runtime\": 3, \"compute_units\": 1, \"os_build\": null}},").expect("write JSON");
    writeln!(json, "    \"alignment_digest_hex\": \"\",").expect("write JSON");
    writeln!(json, "    \"tokenizer_profile\": 0,").expect("write JSON");
    writeln!(
        json,
        "    \"expected_identity\": {{\"embedding_epoch\": {}, \"tokenizer_epoch\": {}}}",
        identity.embedding_epoch, identity.tokenizer_epoch
    )
    .expect("write JSON");
    writeln!(json, "  }},").expect("write JSON");
    writeln!(json, "  \"operations\": [").expect("write JSON");
    writeln!(
        json,
        "    {{\"kind\": \"open_with_epoch\", \"expected\": {{\"error_code\": \"{}\"}}}},",
        code_name(open_code)
    )
    .expect("write JSON");
    writeln!(json, "    {{\"kind\": \"ingest\", \"documents\": [").expect("write JSON");
    for (index, ((vector, text), metadata)) in vectors.iter().zip(TEXTS).zip(metadata).enumerate() {
        let comma = if index + 1 == vectors.len() { "" } else { "," };
        writeln!(json, "      {{\"doc_id\": {{\"high\": 0, \"low\": {}}}, \"revision\": 1, \"timestamp\": {}, \"vector\": {}, \"text\": \"{}\", \"metadata_hex\": \"{}\"}}{}", index + 1, 100 + index * 10, vector_json(vector), text, metadata.iter().map(|byte| format!("{byte:02x}")).collect::<String>(), comma).expect("write JSON");
    }
    writeln!(
        json,
        "    ], \"expected\": {{\"error_code\": \"{}\", \"sequence\": {}, \"generation\": {}}}}},",
        code_name(ingest_code),
        mutation.sequence,
        mutation.generation
    )
    .expect("write JSON");
    writeln!(
        json,
        "{},",
        query_json(
            "vector",
            Some(&vectors[1]),
            None,
            false,
            vector_code,
            &vector_query
        )
    )
    .expect("write JSON");
    writeln!(
        json,
        "{},",
        query_json(
            "lexical",
            None,
            Some("harbour"),
            false,
            lexical_code,
            &lexical_query
        )
    )
    .expect("write JSON");
    writeln!(
        json,
        "{},",
        query_json(
            "hybrid",
            Some(&vectors[1]),
            Some("harbour"),
            true,
            hybrid_code,
            &hybrid_query
        )
    )
    .expect("write JSON");
    writeln!(json, "    {{\"kind\": \"invalid_empty_query\", \"request\": {{\"vector\": null, \"text\": null}}, \"expected\": {{\"error_code\": \"{}\"}}}},", code_name(invalid_code)).expect("write JSON");
    writeln!(
        json,
        "    {{\"kind\": \"close\", \"expected\": {{\"error_code\": \"{}\"}}}}",
        code_name(close_code)
    )
    .expect("write JSON");
    writeln!(json, ",").expect("write JSON");
    writeln!(json, "    {{\"kind\": \"namespace_open\", \"root\": \"namespaces\", \"name\": \"records\", \"spec\": {{\"attributes\": [{{\"attribute_id\": 1, \"name\": \"rank\", \"attribute_type\": 1, \"nullable\": false}}], \"vector_space\": {{\"dimensions\": 1, \"normalization\": 0}}}}, \"expected\": {{\"error_code\": \"{}\"}}}},", code_name(namespace_open_code)).expect("write JSON");
    writeln!(json, "    {{\"kind\": \"namespace_list\", \"root\": \"namespaces\", \"expected\": {{\"error_code\": \"{}\", \"names\": [\"{}\"]}}}},", code_name(namespace_list_code), namespace_names.join("\", \"")).expect("write JSON");
    writeln!(json, "    {{\"kind\": \"upsert\", \"documents\": [{{\"doc_id\": {{\"high\": 0, \"low\": 101}}, \"revision\": 2, \"timestamp\": 321, \"vector\": {}, \"text\": \"{}\", \"metadata_hex\": \"{}\", \"attributes\": [{{\"attribute_id\": 1, \"attribute_type\": 1, \"value\": 7}}]}}], \"expected\": {{\"error_code\": \"{}\", \"sequence\": {}, \"generation\": {}}}}},", vector_json(&record_vector), String::from_utf8_lossy(record_text), record_metadata.iter().map(|byte| format!("{byte:02x}")).collect::<String>(), code_name(upsert_code), upsert_report.sequence, upsert_report.generation).expect("write JSON");
    writeln!(json, "    {{\"kind\": \"get\", \"ids\": [{{\"high\": 0, \"low\": 101}}, {{\"high\": 0, \"low\": 999}}], \"expected\": {{\"error_code\": \"{}\", \"generation\": {}, \"missing_count\": {}, \"documents\": [{{\"doc_id\": {{\"high\": 0, \"low\": 101}}, \"revision\": 2, \"timestamp\": 321, \"vector\": {}, \"text\": \"{}\", \"metadata_hex\": \"{}\", \"attributes\": [{{\"attribute_id\": 1, \"attribute_type\": 1, \"value\": {}}}]}}, null]}}}},", code_name(get_code), get_generation, get_missing_count, vector_json(&returned_vector), returned_text, returned_metadata.iter().map(|byte| format!("{byte:02x}")).collect::<String>(), returned_attribute.u64_value).expect("write JSON");
    writeln!(json, "    {{\"kind\": \"scan\", \"request\": {{\"order\": \"storage\", \"limit\": 10}}, \"expected\": {{\"error_code\": \"{}\", \"generation\": {}, \"has_more\": {}, \"doc_ids\": [{}]}}}},", code_name(scan_code), scan_generation, scan_has_more != 0, scanned_ids.iter().map(u64::to_string).collect::<Vec<_>>().join(", ")).expect("write JSON");
    writeln!(json, "    {{\"kind\": \"count\", \"filter\": {{\"op\": \"eq\", \"field\": \"rank\", \"value\": 7}}, \"expected\": {{\"error_code\": \"{}\", \"generation\": {}, \"count\": {}}}}},", code_name(count_code), count_result.generation, count_result.count).expect("write JSON");
    writeln!(json, "    {{\"kind\": \"search_filtered\", \"request\": {{\"vector\": {}, \"k\": 1, \"tier\": 1, \"filter\": {{\"op\": \"eq\", \"field\": \"rank\", \"value\": 7}}}}, \"expected\": {{\"error_code\": \"{}\", \"generation\": {}, \"hits\": [{{\"doc_id\": {{\"high\": {}, \"low\": {}}}, \"score\": {:.6}}}]}}}}", vector_json(&record_vector), code_name(search_filtered_code), filtered_generation, filtered_hit.doc_id.high, filtered_hit.doc_id.low, filtered_hit.score).expect("write JSON");
    writeln!(json, "  ]").expect("write JSON");
    writeln!(json, "}}").expect("write JSON");
    json
}

#[test]
fn parity_fixture_is_generated_from_the_ffi_and_matches_the_checked_in_json() {
    let generated = generate_fixture();
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root");
    let generated_path = workspace.join("target/cross_binding_parity_v1.json");
    std::fs::write(&generated_path, &generated).expect("write generated fixture copy");
    let checked_path = workspace.join("bindings/fixtures/cross_binding_parity_v1.json");
    let checked = std::fs::read_to_string(&checked_path).unwrap_or_default();
    assert_eq!(
        checked,
        generated,
        "checked fixture drifted; generated copy: {}",
        generated_path.display()
    );
}
