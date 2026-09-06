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
