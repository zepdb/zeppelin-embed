mod common;

use std::mem::size_of;

use zeppelin_embed::epoch::{
    ComputeUnits, EmbeddingEpoch, EmbeddingRuntime, EmbeddingTower, Normalization, StoreEpoch,
};
use zeppelin_embed::fts::tokenizer::TokenizerConfig;
use zeppelin_embed::ingest::{DocId, SearchRequest};
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, SearchOptions, Store};
use zeppelin_embed::meta::{ColumnId, Predicate, PredicateValue};
use zeppelin_embed_ffi::*;

fn open_settings() -> ZeOpenRequest {
    ZeOpenRequest {
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
    }
}

fn open_namespace(
    root: &std::path::Path,
    name: &[u8],
    attributes: &[ZeAttributeDefinition],
    has_vector_space: u32,
    dimensions: u32,
) -> ZeHandle {
    let root = root.to_string_lossy().into_owned().into_bytes();
    let spec = ZeNamespaceSpec {
        abi_size: size_of::<ZeNamespaceSpec>() as u32,
        abi_reserved: 0,
        attributes: attributes.as_ptr(),
        attribute_count: attributes.len(),
        has_vector_space,
        dimensions,
        normalization: 0,
        epoch: std::ptr::null(),
    };
    let request = ZeNamespaceOpenRequest {
        abi_size: size_of::<ZeNamespaceOpenRequest>() as u32,
        abi_reserved: 0,
        root: root.as_ptr(),
        root_len: root.len(),
        name: name.as_ptr(),
        name_len: name.len(),
        open: open_settings(),
        spec: &spec,
    };
    let mut handle = 0;
    assert_eq!(ze_namespace_open(&request, &mut handle), ZeErrorCode::ZeOk);
    handle
}

fn attribute_definition(
    attribute_id: u32,
    name: &[u8],
    attribute_type: i32,
    nullable: u32,
) -> ZeAttributeDefinition {
    ZeAttributeDefinition {
        attribute_id,
        name: name.as_ptr(),
        name_len: name.len(),
        attribute_type,
        nullable,
    }
}

fn canonical_vector_epoch(dimensions: u32) -> StoreEpoch {
    let tower = EmbeddingTower {
        model_id: "zeppelin.vector-space".to_owned(),
        model_version: "1".to_owned(),
        weights_digest: Vec::new(),
        dims: dimensions,
        normalization: Normalization::None,
        prompt_prefix: String::new(),
        max_tokens: 0,
        runtime: EmbeddingRuntime::CpuReference,
        compute_units: ComputeUnits::Cpu,
        os_build: None,
    };
    StoreEpoch {
        embedding: EmbeddingEpoch {
            document: tower.clone(),
            query: tower,
            alignment_digest: Vec::new(),
        },
        tokenizer: TokenizerConfig::text_default().epoch(),
    }
}

fn attribute_value(attribute_id: u32, value_type: i32) -> ZeAttributeValue {
    ZeAttributeValue {
        attribute_id,
        value_type,
        u64_value: 0,
        i64_value: 0,
        f64_value: 0.0,
        bool_value: 0,
        string_value: std::ptr::null(),
        string_len: 0,
    }
}

fn upsert_code(
    handle: ZeHandle,
    doc_id: u64,
    revision: u64,
    vector: Option<&[f32]>,
    dimension: usize,
    attributes: &[ZeAttributeValue],
) -> ZeErrorCode {
    let (vector, vector_len) = vector.map_or((std::ptr::null(), 0), |vector| {
        (vector.as_ptr(), vector.len())
    });
    let attributes_pointer = if attributes.is_empty() {
        std::ptr::null()
    } else {
        attributes.as_ptr()
    };
    let document = ZeUpsertDocument {
        abi_size: size_of::<ZeUpsertDocument>() as u32,
        abi_reserved: 0,
        document: ZeIngestDocument {
            abi_size: size_of::<ZeIngestDocument>() as u32,
            abi_reserved: 0,
            doc_id: ZeDocId {
                high: 0,
                low: doc_id,
            },
            revision,
            timestamp: 0,
            vector,
            vector_len,
            metadata: std::ptr::null(),
            metadata_len: 0,
            text: std::ptr::null(),
            text_len: 0,
        },
        attributes: attributes_pointer,
        attribute_count: attributes.len(),
    };
    let request = ZeUpsertRequest {
        abi_size: size_of::<ZeUpsertRequest>() as u32,
        abi_reserved: 0,
        documents: &document,
        document_count: 1,
        dimension,
    };
    let mut report: ZeMutationReport = common::sized_zeroed();
    ze_upsert(handle, &request, &mut report)
}

fn last_error(handle: ZeHandle) -> String {
    let mut required = 0;
    assert_eq!(
        ze_last_error_message(handle, std::ptr::null_mut(), 0, &mut required),
        ZeErrorCode::ZeOk
    );
    let mut bytes = vec![0_i8; required + 1];
    assert_eq!(
        ze_last_error_message(handle, bytes.as_mut_ptr(), bytes.len(), &mut required),
        ZeErrorCode::ZeOk
    );
    String::from_utf8(
        bytes
            .into_iter()
            .take(required)
            .map(|byte| byte as u8)
            .collect(),
    )
    .expect("last error UTF-8")
}

#[test]
fn upsert_with_every_attribute_type_round_trips_through_core_filters() {
    let root = tempfile::tempdir().expect("temporary namespace root");
    let namespace = b"all-types";
    let names: [&[u8]; 6] = [b"u64", b"i64", b"f64", b"bool", b"dict", b"raw"];
    let attributes = [
        attribute_definition(1, names[0], 1, 0),
        attribute_definition(2, names[1], 2, 0),
        attribute_definition(3, names[2], 3, 0),
        attribute_definition(4, names[3], 4, 0),
        attribute_definition(5, names[4], 5, 0),
        attribute_definition(6, names[5], 6, 0),
    ];
    let handle = open_namespace(root.path(), namespace, &attributes, 1, 2);
    let vector = [1.0_f32, 0.0];
    let dictionary = b"alpha";
    let raw = b"bravo";
    let values = [
        ZeAttributeValue {
            attribute_id: 1,
            value_type: 1,
            u64_value: 42,
            i64_value: 0,
            f64_value: 0.0,
            bool_value: 0,
            string_value: std::ptr::null(),
            string_len: 0,
        },
        ZeAttributeValue {
            attribute_id: 2,
            value_type: 2,
            u64_value: 0,
            i64_value: -17,
            f64_value: 0.0,
            bool_value: 0,
            string_value: std::ptr::null(),
            string_len: 0,
        },
        ZeAttributeValue {
            attribute_id: 3,
            value_type: 3,
            u64_value: 0,
            i64_value: 0,
            f64_value: 3.5,
            bool_value: 0,
            string_value: std::ptr::null(),
            string_len: 0,
        },
        ZeAttributeValue {
            attribute_id: 4,
            value_type: 4,
            u64_value: 0,
            i64_value: 0,
            f64_value: 0.0,
            bool_value: 1,
            string_value: std::ptr::null(),
            string_len: 0,
        },
        ZeAttributeValue {
            attribute_id: 5,
            value_type: 5,
            u64_value: 0,
            i64_value: 0,
            f64_value: 0.0,
            bool_value: 0,
            string_value: dictionary.as_ptr(),
            string_len: dictionary.len(),
        },
        ZeAttributeValue {
            attribute_id: 6,
            value_type: 5,
            u64_value: 0,
            i64_value: 0,
            f64_value: 0.0,
            bool_value: 0,
            string_value: raw.as_ptr(),
            string_len: raw.len(),
        },
    ];
    let document = ZeUpsertDocument {
        abi_size: size_of::<ZeUpsertDocument>() as u32,
        abi_reserved: 0,
        document: ZeIngestDocument {
            abi_size: size_of::<ZeIngestDocument>() as u32,
            abi_reserved: 0,
            doc_id: ZeDocId { high: 0, low: 7 },
            revision: 1,
            timestamp: 99,
            vector: vector.as_ptr(),
            vector_len: vector.len(),
            metadata: std::ptr::null(),
            metadata_len: 0,
            text: std::ptr::null(),
            text_len: 0,
        },
        attributes: values.as_ptr(),
        attribute_count: values.len(),
    };
    let request = ZeUpsertRequest {
        abi_size: size_of::<ZeUpsertRequest>() as u32,
        abi_reserved: 0,
        documents: &document,
        document_count: 1,
        dimension: vector.len(),
    };
    let mut report: ZeMutationReport = common::sized_zeroed();
    assert_eq!(ze_upsert(handle, &request, &mut report), ZeErrorCode::ZeOk);
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);

    let store = Store::open(
        root.path().join("all-types"),
        OpenOptions::read_only().with_epoch(canonical_vector_epoch(2)),
    )
    .expect("open through core API");
    let predicates = [
        Predicate::Eq {
            column: ColumnId::new(1),
            value: PredicateValue::U64(42),
        },
        Predicate::Eq {
            column: ColumnId::new(2),
            value: PredicateValue::I64(-17),
        },
        Predicate::Eq {
            column: ColumnId::new(3),
            value: PredicateValue::F64(3.5),
        },
        Predicate::Eq {
            column: ColumnId::new(4),
            value: PredicateValue::Bool(true),
        },
        Predicate::Eq {
            column: ColumnId::new(5),
            value: PredicateValue::String("alpha".to_owned()),
        },
        Predicate::Eq {
            column: ColumnId::new(6),
            value: PredicateValue::String("bravo".to_owned()),
        },
    ];
    for predicate in predicates {
        let outcome = store
            .search_filtered(
                SearchRequest::new(&vector),
                &predicate,
                1,
                SearchOptions::default(),
                QueryControl::Cancel(CancelToken::new()),
            )
            .expect("filter through core API");
        assert_eq!(outcome.candidates.len(), 1, "predicate {predicate:?}");
        assert_eq!(
            outcome.candidates[0]
                .document()
                .map(|version| version.doc_id()),
            Some(DocId::new(7))
        );
    }
}

#[test]
fn upsert_nullable_attribute_stores_null() {
    let root = tempfile::tempdir().expect("temporary namespace root");
    let name = b"nullable";
    let attribute_name = b"optional";
    let attributes = [attribute_definition(1, attribute_name, 1, 1)];
    let handle = open_namespace(root.path(), name, &attributes, 1, 2);
    let vector = [1.0_f32, 0.0];
    let values = [ZeAttributeValue {
        attribute_id: 1,
        value_type: 0,
        u64_value: 0,
        i64_value: 0,
        f64_value: 0.0,
        bool_value: 0,
        string_value: std::ptr::null(),
        string_len: 0,
    }];
    let document = ZeUpsertDocument {
        abi_size: size_of::<ZeUpsertDocument>() as u32,
        abi_reserved: 0,
        document: ZeIngestDocument {
            abi_size: size_of::<ZeIngestDocument>() as u32,
            abi_reserved: 0,
            doc_id: ZeDocId { high: 0, low: 8 },
            revision: 1,
            timestamp: 100,
            vector: vector.as_ptr(),
            vector_len: vector.len(),
            metadata: std::ptr::null(),
            metadata_len: 0,
            text: std::ptr::null(),
            text_len: 0,
        },
        attributes: values.as_ptr(),
        attribute_count: values.len(),
    };
    let request = ZeUpsertRequest {
        abi_size: size_of::<ZeUpsertRequest>() as u32,
        abi_reserved: 0,
        documents: &document,
        document_count: 1,
        dimension: vector.len(),
    };
    let mut report: ZeMutationReport = common::sized_zeroed();
    assert_eq!(ze_upsert(handle, &request, &mut report), ZeErrorCode::ZeOk);
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);

    let store = Store::open(
        root.path().join("nullable"),
        OpenOptions::read_only().with_epoch(canonical_vector_epoch(2)),
    )
    .expect("open through core API");
    let outcome = store
        .search_filtered(
            SearchRequest::new(&vector),
            &Predicate::IsNull(ColumnId::new(1)),
            1,
            SearchOptions::default(),
            QueryControl::Cancel(CancelToken::new()),
        )
        .expect("filter null through core API");
    assert_eq!(outcome.candidates.len(), 1);
    assert_eq!(
        outcome.candidates[0]
            .document()
            .map(|version| version.doc_id()),
        Some(DocId::new(8))
    );
}

#[test]
fn upsert_missing_non_nullable_attribute_is_invalid_argument() {
    let root = tempfile::tempdir().expect("temporary namespace root");
    let attribute_name = b"required";
    let attributes = [attribute_definition(1, attribute_name, 1, 0)];
    let handle = open_namespace(root.path(), b"missing", &attributes, 1, 2);
    let vector = [1.0_f32, 0.0];

    assert_eq!(
        upsert_code(handle, 1, 1, Some(&vector), vector.len(), &[]),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        last_error(handle),
        "ingest columns: missing required column 1"
    );
}

#[test]
fn upsert_attribute_type_mismatch_is_invalid_argument() {
    let root = tempfile::tempdir().expect("temporary namespace root");
    let attribute_name = b"count";
    let attributes = [attribute_definition(1, attribute_name, 1, 0)];
    let handle = open_namespace(root.path(), b"mismatch", &attributes, 1, 2);
    let vector = [1.0_f32, 0.0];
    let mut value = attribute_value(1, 2);
    value.i64_value = -1;

    assert_eq!(
        upsert_code(handle, 1, 1, Some(&vector), vector.len(), &[value]),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        last_error(handle),
        "ingest columns: column 1 expects U64, received I64"
    );
}

#[test]
fn upsert_duplicate_attribute_id_is_invalid_argument() {
    let root = tempfile::tempdir().expect("temporary namespace root");
    let attribute_name = b"count";
    let attributes = [attribute_definition(1, attribute_name, 1, 1)];
    let handle = open_namespace(root.path(), b"duplicate", &attributes, 1, 2);
    let vector = [1.0_f32, 0.0];
    let value = attribute_value(1, 0);

    assert_eq!(
        upsert_code(handle, 1, 1, Some(&vector), vector.len(), &[value, value],),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(last_error(handle), "duplicate column 1");
}

#[test]
fn upsert_reserved_attribute_id_zero_is_invalid_argument() {
    let root = tempfile::tempdir().expect("temporary namespace root");
    let handle = open_namespace(root.path(), b"reserved", &[], 1, 2);
    let vector = [1.0_f32, 0.0];
    let value = attribute_value(0, 2);

    assert_eq!(
        upsert_code(handle, 1, 1, Some(&vector), vector.len(), &[value]),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(last_error(handle), "attribute_id zero is reserved for ts");
}

#[test]
fn upsert_higher_revision_supersedes_and_stale_revision_is_rejected() {
    let root = tempfile::tempdir().expect("temporary namespace root");
    let handle = open_namespace(root.path(), b"revisions", &[], 1, 2);
    let first = [1.0_f32, 0.0];
    let second = [0.0_f32, 1.0];
    assert_eq!(
        upsert_code(handle, 41, 1, Some(&first), first.len(), &[]),
        ZeErrorCode::ZeOk
    );
    assert_eq!(
        upsert_code(handle, 41, 2, Some(&second), second.len(), &[]),
        ZeErrorCode::ZeOk
    );

    let search = common::valid_search_request(&second);
    let mut result: ZeSearchResult = common::sized_zeroed();
    assert_eq!(ze_search(handle, &search, &mut result), ZeErrorCode::ZeOk);
    assert_eq!(result.hit_count, 1);
    let hits = unsafe { std::slice::from_raw_parts(result.hits, result.hit_count) };
    assert_eq!(hits[0].doc_id, ZeDocId { high: 0, low: 41 });
    assert_eq!(hits[0].revision, 2);
    assert_eq!(ze_search_result_free(&mut result), ZeErrorCode::ZeOk);

    assert_eq!(
        upsert_code(handle, 41, 1, Some(&first), first.len(), &[]),
        ZeErrorCode::ZeErrStaleRevision
    );
}

#[test]
fn upsert_record_only_namespace_supplies_sentinel_and_rejects_caller_vector() {
    let root = tempfile::tempdir().expect("temporary namespace root");
    let handle = open_namespace(root.path(), b"records", &[], 0, 0);
    assert_eq!(upsert_code(handle, 1, 1, None, 0, &[]), ZeErrorCode::ZeOk);

    let caller_vector = [1.0_f32];
    assert_eq!(
        upsert_code(handle, 2, 1, Some(&caller_vector), 1, &[]),
        ZeErrorCode::ZeErrNoVectorSpace
    );
    assert_eq!(
        upsert_code(handle, 2, 1, None, 2, &[]),
        ZeErrorCode::ZeErrInvalidArgument
    );
}

#[test]
fn upsert_invalid_pointers_sizes_and_empty_batch_return_typed_errors() {
    let root = tempfile::tempdir().expect("temporary namespace root");
    let handle = open_namespace(root.path(), b"invalid", &[], 1, 2);
    let mut report: ZeMutationReport = common::sized_zeroed();
    assert_eq!(
        ze_upsert(handle, std::ptr::null(), &mut report),
        ZeErrorCode::ZeErrInvalidArgument
    );

    let mut request = ZeUpsertRequest {
        abi_size: size_of::<ZeUpsertRequest>() as u32,
        abi_reserved: 0,
        documents: std::ptr::null(),
        document_count: 0,
        dimension: 2,
    };
    assert_eq!(
        ze_upsert(handle, &request, std::ptr::null_mut()),
        ZeErrorCode::ZeErrInvalidArgument
    );
    request.abi_size = 0;
    assert_eq!(
        ze_upsert(handle, &request, &mut report),
        ZeErrorCode::ZeErrInvalidArgument
    );
    request.abi_size = size_of::<ZeUpsertRequest>() as u32 - 1;
    assert_eq!(
        ze_upsert(handle, &request, &mut report),
        ZeErrorCode::ZeErrInvalidArgument
    );
    request.abi_size = size_of::<ZeUpsertRequest>() as u32;
    assert_eq!(
        ze_upsert(handle, &request, &mut report),
        ZeErrorCode::ZeErrEmptyBatch
    );
    request.document_count = 1;
    assert_eq!(
        ze_upsert(handle, &request, &mut report),
        ZeErrorCode::ZeErrInvalidArgument
    );
}
