mod common;

use std::mem::size_of;
use zeppelin_embed_ffi::*;

fn global_error_guard() -> std::sync::MutexGuard<'static, ()> {
    static GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
    GUARD.lock().expect("global-error fixture guard")
}

fn last_error(handle: ZeHandle) -> String {
    let mut length = 0;
    assert_eq!(
        ze_last_error_message(handle, std::ptr::null_mut(), 0, &mut length),
        ZeErrorCode::ZeOk
    );
    let mut bytes = vec![0_u8; length + 1];
    assert_eq!(
        ze_last_error_message(handle, bytes.as_mut_ptr().cast(), bytes.len(), &mut length),
        ZeErrorCode::ZeOk
    );
    assert_eq!(bytes.pop(), Some(0));
    String::from_utf8(bytes).expect("UTF-8 error")
}

fn open_request(path: &[u8]) -> ZeOpenRequest {
    ZeOpenRequest {
        abi_size: size_of::<ZeOpenRequest>() as u32,
        abi_reserved: 0,
        path: path.as_ptr(),
        path_len: path.len(),
        access_mode: 0,
        durability_mode: 0,
        commit_tier: 1,
        reader_drain_timeout_ms: 100,
        max_resident_bytes: u64::MAX,
        max_temp_bytes: u64::MAX,
    }
}

#[test]
fn open_rejects_unknown_options_and_read_only_handles_reject_writes() {
    let _guard = global_error_guard();
    let directory = tempfile::tempdir().expect("open fixture");
    let path = directory.path().join("store");
    let bytes = path.to_string_lossy().into_owned().into_bytes();
    for (access_mode, durability_mode, commit_tier, message) in [
        (7, 0, 1, "access_mode"),
        (0, 7, 1, "durability_mode"),
        (0, 0, 7, "commit_tier"),
    ] {
        let request = ZeOpenRequest {
            access_mode,
            durability_mode,
            commit_tier,
            ..open_request(&bytes)
        };
        let mut handle = 0;
        assert_eq!(
            ze_open(&request, &mut handle),
            ZeErrorCode::ZeErrInvalidArgument
        );
        assert_eq!(handle, 0);
        assert!(last_error(0).contains(message));
        assert!(!path.exists(), "invalid options must not create a store");
    }
    for (mode, tier) in [(0, 0), (1, 2), (2, 1)] {
        let path = directory.path().join(format!("mode-{mode}"));
        let bytes = path.to_string_lossy().into_owned().into_bytes();
        let mut request = ZeOpenRequest {
            durability_mode: mode,
            commit_tier: tier,
            ..open_request(&bytes)
        };
        let mut handle = 0;
        if mode == 2 {
            assert_eq!(
                ze_open(&request, &mut handle),
                ZeErrorCode::ZeErrUnsupported
            );
            assert_eq!(handle, 0);
            assert!(last_error(0).contains("attached durability mode is not yet supported"));
            continue;
        }
        assert_eq!(ze_open(&request, &mut handle), ZeErrorCode::ZeOk);
        assert_eq!(common::ingest_rows(handle, 1, 1), ZeErrorCode::ZeOk);
        assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
        request.access_mode = 1;
        assert_eq!(ze_open(&request, &mut handle), ZeErrorCode::ZeOk);
        assert_eq!(
            common::ingest_rows(handle, 1, 1),
            ZeErrorCode::ZeErrAccessMode
        );
        assert!(last_error(handle).contains("read-only"));
        assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
    }
}

#[test]
fn epoch_identity_validates_tags_and_preserves_runtime_compute_and_os_identity() {
    let _guard = global_error_guard();
    let fixture = common::EpochFixture::new(8);
    let epoch = fixture.request();
    let mut output: ZeEpochIdentity = common::sized_zeroed();
    assert_eq!(ze_epoch_identity(&epoch, &mut output), ZeErrorCode::ZeOk);
    let initial = output.embedding_epoch;
    let os = b"26A5388g";
    let invalid_utf8 = [0xff];
    for (request, expected) in [
        (
            {
                let mut value = epoch;
                value.reserved = 1;
                value
            },
            "reserved",
        ),
        (
            {
                let mut value = epoch;
                value.tokenizer_profile = 99;
                value
            },
            "tokenizer_profile",
        ),
        (
            {
                let mut value = epoch;
                value.embedding.document.normalization = 99;
                value
            },
            "normalization",
        ),
        (
            {
                let mut value = epoch;
                value.embedding.document.runtime = 99;
                value
            },
            "runtime",
        ),
        (
            {
                let mut value = epoch;
                value.embedding.document.compute_units = 99;
                value
            },
            "compute_units",
        ),
        (
            {
                let mut value = epoch;
                value.embedding.document.has_os_build = 2;
                value
            },
            "has_os_build",
        ),
        (
            {
                let mut value = epoch;
                value.embedding.document.os_build = os.as_ptr();
                value.embedding.document.os_build_len = os.len();
                value
            },
            "without has_os_build",
        ),
        (
            {
                let mut value = epoch;
                value.embedding.document.model_version = invalid_utf8.as_ptr();
                value.embedding.document.model_version_len = 1;
                value
            },
            "model_version is not valid UTF-8",
        ),
    ] {
        assert_eq!(
            ze_epoch_identity(&request, &mut output),
            ZeErrorCode::ZeErrInvalidArgument
        );
        assert!(last_error(0).contains(expected), "expected {expected}");
    }
    let mut identities = std::collections::BTreeSet::new();
    for runtime in [1, 2, 3] {
        for compute_units in [1, 2, 3, 4] {
            let mut request = epoch;
            request.embedding.document.runtime = runtime;
            request.embedding.document.compute_units = compute_units;
            request.embedding.document.normalization = 1;
            request.embedding.document.has_os_build = 1;
            request.embedding.document.os_build = os.as_ptr();
            request.embedding.document.os_build_len = os.len();
            assert_eq!(ze_epoch_identity(&request, &mut output), ZeErrorCode::ZeOk);
            assert_ne!(output.embedding_epoch, initial);
            assert!(
                identities.insert(output.embedding_epoch),
                "runtime/compute distinctions must not collapse"
            );
        }
    }
}

fn namespace(root: &std::path::Path, definitions: &[ZeAttributeDefinition]) -> ZeHandle {
    let bytes = root.to_string_lossy().into_owned().into_bytes();
    let spec = ZeNamespaceSpec {
        attributes: definitions.as_ptr(),
        attribute_count: definitions.len(),
        has_vector_space: 1,
        dimensions: 1,
        ..common::sized_zeroed()
    };
    let request = ZeNamespaceOpenRequest {
        root: bytes.as_ptr(),
        root_len: bytes.len(),
        name: b"boundary".as_ptr(),
        name_len: 8,
        open: open_request(&[]),
        spec: &spec,
        ..common::sized_zeroed()
    };
    let mut handle = 0;
    assert_eq!(ze_namespace_open(&request, &mut handle), ZeErrorCode::ZeOk);
    handle
}

fn value(column: u32, kind: i32) -> ZeAttributeValue {
    ZeAttributeValue {
        attribute_id: column,
        value_type: kind,
        ..unsafe { std::mem::zeroed() }
    }
}

fn node(op: i32, column: u32, values: &[ZeAttributeValue]) -> ZeFilterNode {
    ZeFilterNode {
        op,
        attribute_id: column,
        values: values.as_ptr(),
        value_count: values.len(),
        ..unsafe { std::mem::zeroed() }
    }
}

fn count_nodes(handle: ZeHandle, nodes: &[ZeFilterNode]) -> (ZeErrorCode, ZeCountResult) {
    let filter = ZeFilter {
        nodes: nodes.as_ptr(),
        node_count: nodes.len(),
        root: 0,
        ..common::sized_zeroed()
    };
    let request = ZeCountRequest {
        filter: &filter,
        ..common::sized_zeroed()
    };
    let mut output: ZeCountResult = common::sized_zeroed();
    let code = ze_count(handle, &request, &mut output);
    (code, output)
}

#[test]
fn filter_grammar_rejects_irrelevant_children_bounds_and_values() {
    let store = common::TestStore::new();
    let values = [ZeAttributeValue {
        i64_value: 0,
        ..value(0, 2)
    }];
    let mut cases = Vec::new();
    for (op, name) in [
        (1, "equality"),
        (3, "membership"),
        (5, "range"),
        (6, "presence"),
    ] {
        cases.push((
            ZeFilterNode {
                children_count: 1,
                ..node(op, 0, &values)
            },
            format!("filter {name} cannot have children"),
        ));
    }
    for (op, name) in [
        (1, "equality"),
        (3, "membership"),
        (6, "presence"),
        (8, "logical operator"),
    ] {
        cases.push((
            ZeFilterNode {
                has_lower: 1,
                ..node(op, 0, &values)
            },
            format!("filter {name} cannot have range bounds"),
        ));
    }
    for (op, name) in [(5, "range"), (6, "presence"), (8, "logical operator")] {
        cases.push((
            node(op, 0, &values),
            format!("filter {name} cannot have values"),
        ));
    }
    cases.push((
        node(5, 0, &[]),
        "filter range requires at least one bound".into(),
    ));
    cases.push((node(6, 77, &[]), "unknown column 77".into()));
    cases.push((node(99, 0, &[]), "filter operator is out of range".into()));
    for (invalid, message) in cases {
        assert_eq!(
            count_nodes(store.handle, &[invalid]).0,
            ZeErrorCode::ZeErrInvalidArgument
        );
        assert_eq!(last_error(store.handle), message);
    }
    assert_eq!(
        count_nodes(store.handle, &[node(6, 0, &[])]).0,
        ZeErrorCode::ZeOk
    );
}

#[test]
fn filter_values_reject_null_unknown_tags_wrong_columns_and_invalid_bounds() {
    let store = common::TestStore::new();
    let cases = [
        (
            value(1, 2),
            "filter value column 1 differs from node column 0",
        ),
        (value(0, 0), "filter values cannot be null"),
        (
            value(0, 99),
            "filter value_type discriminant is out of range",
        ),
        (
            ZeAttributeValue {
                bool_value: 2,
                ..value(0, 4)
            },
            "filter bool value must be zero or one",
        ),
    ];
    for (invalid, message) in cases {
        assert_eq!(
            count_nodes(store.handle, &[node(1, 0, &[invalid])]).0,
            ZeErrorCode::ZeErrInvalidArgument
        );
        assert_eq!(last_error(store.handle), message);
    }
    let mut range = node(5, 0, &[]);
    range.has_lower = 1;
    range.lower = value(0, 2);
    range.lower_inclusive = 2;
    assert_eq!(
        count_nodes(store.handle, &[range]).0,
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        last_error(store.handle),
        "filter lower_inclusive must be zero or one"
    );
    range.has_lower = 0;
    range.has_upper = 1;
    range.upper = value(0, 2);
    range.upper_inclusive = 2;
    assert_eq!(
        count_nodes(store.handle, &[range]).0,
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        last_error(store.handle),
        "filter upper_inclusive must be zero or one"
    );
}

#[test]
fn typed_attributes_round_trip_through_get_and_match_exact_filters() {
    let directory = tempfile::tempdir().expect("typed namespace");
    let names: [&[u8]; 5] = [b"signed", b"float", b"flag", b"dict", b"raw"];
    let definitions: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(index, name)| ZeAttributeDefinition {
            attribute_id: index as u32 + 1,
            name: name.as_ptr(),
            name_len: name.len(),
            attribute_type: index as i32 + 2,
            nullable: 0,
        })
        .collect();
    let handle = namespace(directory.path(), &definitions);
    let string = b"exact\0bytes";
    let attributes = [
        ZeAttributeValue {
            i64_value: -9,
            ..value(1, 2)
        },
        ZeAttributeValue {
            f64_value: 1.25,
            ..value(2, 3)
        },
        ZeAttributeValue {
            bool_value: 1,
            ..value(3, 4)
        },
        ZeAttributeValue {
            string_value: string.as_ptr(),
            string_len: string.len(),
            ..value(4, 5)
        },
        ZeAttributeValue {
            string_value: string.as_ptr(),
            string_len: string.len(),
            ..value(5, 5)
        },
    ];
    let vector = [1.0_f32];
    let document = ZeUpsertDocument {
        document: ZeIngestDocument {
            doc_id: ZeDocId { high: 3, low: 7 },
            revision: 1,
            vector: vector.as_ptr(),
            vector_len: 1,
            ..common::sized_zeroed()
        },
        attributes: attributes.as_ptr(),
        attribute_count: attributes.len(),
        ..common::sized_zeroed()
    };
    let request = ZeUpsertRequest {
        documents: &document,
        document_count: 1,
        dimension: 1,
        ..common::sized_zeroed()
    };
    let mut mutation: ZeMutationReport = common::sized_zeroed();
    assert_eq!(
        ze_upsert(handle, &request, &mut mutation),
        ZeErrorCode::ZeOk
    );
    for attribute in attributes {
        let (code, count) = count_nodes(handle, &[node(1, attribute.attribute_id, &[attribute])]);
        assert_eq!(code, ZeErrorCode::ZeOk);
        assert_eq!(count.count, 1);
        assert_eq!(count.generation, mutation.generation);
    }
    let request = ZeGetRequest {
        ids: &document.document.doc_id,
        id_count: 1,
        include_attributes: 1,
        ..common::sized_zeroed()
    };
    let mut result: ZeGetResult = common::sized_zeroed();
    assert_eq!(ze_get(handle, &request, &mut result), ZeErrorCode::ZeOk);
    assert_eq!(result.document_count, 1);
    let returned = unsafe { &*result.documents };
    assert_eq!(returned.attribute_count, 5);
    let actual =
        unsafe { std::slice::from_raw_parts(returned.attributes, returned.attribute_count) };
    for expected in attributes {
        let actual = actual
            .iter()
            .find(|attribute| attribute.attribute_id == expected.attribute_id)
            .expect("stored attribute");
        assert_eq!(actual.value_type, expected.value_type);
        match expected.value_type {
            2 => assert_eq!(actual.i64_value, -9),
            3 => assert_eq!(actual.f64_value.to_bits(), 1.25_f64.to_bits()),
            4 => assert_eq!(actual.bool_value, 1),
            5 => assert_eq!(
                unsafe { std::slice::from_raw_parts(actual.string_value, actual.string_len) },
                string
            ),
            _ => unreachable!("fixture types"),
        }
    }
    assert_eq!(ze_get_result_free(&mut result), ZeErrorCode::ZeOk);
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}

#[test]
fn malformed_result_frees_preserve_caller_bytes_and_live_allocations() {
    let _guard = global_error_guard();
    let mut storage = [0_u64; 64];
    let unaligned = unsafe { storage.as_mut_ptr().cast::<u8>().add(1) };
    for (status, expected) in [
        (
            ze_search_result_free(unaligned.cast()),
            "search result pointer is misaligned",
        ),
        (
            ze_query_result_free(unaligned.cast()),
            "query result pointer is misaligned",
        ),
        (
            ze_get_result_free(unaligned.cast()),
            "get result pointer is misaligned",
        ),
        (
            ze_scan_result_free(unaligned.cast()),
            "scan result pointer is misaligned",
        ),
        (
            ze_namespace_list_result_free(unaligned.cast()),
            "namespace list result pointer is misaligned",
        ),
    ] {
        assert_eq!(status, ZeErrorCode::ZeErrInvalidArgument, "{expected}");
    }
    assert!(storage.iter().all(|value| *value == 0));
    let store = common::TestStore::new();
    assert_eq!(common::ingest_rows(store.handle, 2, 1), ZeErrorCode::ZeOk);
    let vector = [1.0];
    let request = common::valid_search_request(&vector);
    let mut result: ZeSearchResult = common::sized_zeroed();
    assert_eq!(
        ze_search(store.handle, &request, &mut result),
        ZeErrorCode::ZeOk
    );
    let original = result;
    for (size, length, generation, message) in [
        (
            0,
            original.hit_count,
            original.abi_reserved,
            "zero-sized search result contains an allocation",
        ),
        (4, original.hit_count, original.abi_reserved, "size"),
        (
            original.abi_size,
            0,
            original.abi_reserved,
            "result pointer and length disagree",
        ),
        (
            original.abi_size,
            original.hit_count,
            original.abi_reserved.wrapping_add(1),
            "not allocated by this ABI",
        ),
    ] {
        result.abi_size = size;
        result.hit_count = length;
        result.abi_reserved = generation;
        assert_eq!(
            ze_search_result_free(&mut result),
            ZeErrorCode::ZeErrInvalidArgument
        );
        assert!(last_error(0).contains(message));
        assert_eq!(result.hits, original.hits);
        assert_eq!(result.abi_size, size);
    }
    result = original;
    assert_eq!(unsafe { (*result.hits).has_document }, 1);
    assert_eq!(ze_search_result_free(&mut result), ZeErrorCode::ZeOk);
    assert!(result.hits.is_null());
    assert_eq!(result.hit_count, 0);
    let mut query: ZeQueryResult = unsafe { std::mem::zeroed() };
    query.hit_count = 1;
    assert_eq!(
        ze_query_result_free(&mut query),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert!(last_error(0).contains("zero-sized query result contains an allocation"));
    let mut namespaces: ZeNamespaceListResult = unsafe { std::mem::zeroed() };
    assert_eq!(
        ze_namespace_list_result_free(&mut namespaces),
        ZeErrorCode::ZeOk
    );
    namespaces.entry_count = 1;
    assert_eq!(
        ze_namespace_list_result_free(&mut namespaces),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert!(last_error(0).contains("zero-sized namespace list result contains an allocation"));
    namespaces.abi_size = size_of::<ZeNamespaceListResult>() as u32;
    assert_eq!(
        ze_namespace_list_result_free(&mut namespaces),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert!(last_error(0).contains("pointer and entry count disagree"));
}

#[test]
fn namespace_schema_rejects_invalid_columns_flags_and_epoch_geometry() {
    let _guard = global_error_guard();
    let directory = tempfile::tempdir().expect("invalid schema fixture");
    let bytes = directory.path().to_string_lossy().into_owned().into_bytes();
    let definition = ZeAttributeDefinition {
        attribute_id: 1,
        name: b"value".as_ptr(),
        name_len: 5,
        attribute_type: 1,
        nullable: 0,
    };
    let fixture = common::EpochFixture::new(2);
    let epoch = fixture.request();
    let spec = ZeNamespaceSpec {
        attributes: &definition,
        attribute_count: 1,
        has_vector_space: 1,
        dimensions: 1,
        ..common::sized_zeroed()
    };
    let request = ZeNamespaceOpenRequest {
        root: bytes.as_ptr(),
        root_len: bytes.len(),
        name: b"invalid".as_ptr(),
        name_len: 7,
        open: open_request(&[]),
        spec: &spec,
        ..common::sized_zeroed()
    };
    for (column, message) in [
        (
            ZeAttributeDefinition {
                attribute_id: 0,
                ..definition
            },
            "attribute_id zero is reserved for ts",
        ),
        (
            ZeAttributeDefinition {
                attribute_type: 99,
                ..definition
            },
            "attribute_type discriminant is out of range",
        ),
        (
            ZeAttributeDefinition {
                nullable: 2,
                ..definition
            },
            "nullable must be zero or one",
        ),
    ] {
        let spec = ZeNamespaceSpec {
            attributes: &column,
            ..spec
        };
        let request = ZeNamespaceOpenRequest {
            spec: &spec,
            ..request
        };
        let mut handle = 0;
        assert_eq!(
            ze_namespace_open(&request, &mut handle),
            ZeErrorCode::ZeErrInvalidArgument
        );
        assert_eq!(last_error(0), message);
        assert_eq!(handle, 0);
    }
    for (spec, message) in [
        (
            ZeNamespaceSpec {
                has_vector_space: 2,
                ..spec
            },
            "has_vector_space must be zero or one",
        ),
        (
            ZeNamespaceSpec {
                dimensions: 0,
                ..spec
            },
            "vector namespace dimensions must be nonzero",
        ),
        (
            ZeNamespaceSpec {
                epoch: &epoch,
                ..spec
            },
            "declared epoch dimensions do not match the namespace dimensions",
        ),
        (
            ZeNamespaceSpec {
                epoch: &epoch,
                dimensions: 2,
                normalization: 1,
                ..spec
            },
            "declared epoch normalization does not match the namespace normalization",
        ),
    ] {
        let request = ZeNamespaceOpenRequest {
            spec: &spec,
            ..request
        };
        let mut handle = 0;
        assert_eq!(
            ze_namespace_open(&request, &mut handle),
            ZeErrorCode::ZeErrInvalidArgument
        );
        assert_eq!(last_error(0), message);
        assert_eq!(handle, 0);
    }
    assert!(!directory.path().join("invalid").exists());
}

#[test]
fn error_copy_and_scalar_outputs_reject_invalid_buffers_without_writing_them() {
    let _guard = global_error_guard();
    let mut aligned = [0_u64; 4];
    let misaligned = unsafe { aligned.as_mut_ptr().cast::<u8>().add(1) };
    assert_eq!(
        ze_cancel_token_create(misaligned.cast()),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(last_error(0), "output pointer is misaligned");
    assert!(aligned.iter().all(|value| *value == 0));
    let mut length = 0;
    assert_eq!(
        ze_last_error_message(0, std::ptr::null_mut(), 1, &mut length),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        last_error(0),
        "last-error buffer is null with nonzero capacity"
    );
    let mut tiny = [0xa5_u8; 1];
    assert_eq!(
        ze_last_error_message(0, tiny.as_mut_ptr().cast(), tiny.len(), &mut length),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(tiny, [0xa5]);
    assert!(length > tiny.len());
    assert_eq!(last_error(0), "last-error buffer capacity is too small");
}

#[test]
fn wrong_result_type_or_missing_count_never_consumes_the_owned_allocation() {
    let _guard = global_error_guard();
    let store = common::TestStore::new();
    assert_eq!(common::ingest_rows(store.handle, 1, 1), ZeErrorCode::ZeOk);
    let vector = [1.0];
    let search = common::valid_search_request(&vector);
    let mut owned: ZeSearchResult = common::sized_zeroed();
    assert_eq!(
        ze_search(store.handle, &search, &mut owned),
        ZeErrorCode::ZeOk
    );
    let mut wrong: ZeQueryResult = common::sized_zeroed();
    wrong.hits = owned.hits.cast();
    wrong.hit_count = owned.hit_count;
    wrong.abi_reserved = owned.abi_reserved;
    assert_eq!(
        ze_query_result_free(&mut wrong),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert!(last_error(0).contains("not allocated by this ABI"));
    assert_eq!(unsafe { (*owned.hits).doc_id.low }, 1);
    assert_eq!(ze_search_result_free(&mut owned), ZeErrorCode::ZeOk);
    let mut empty: ZeQueryResult = common::sized_zeroed();
    empty.abi_reserved = 1;
    assert_eq!(
        ze_query_result_free(&mut empty),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        last_error(0),
        "empty result has a nonzero allocation generation"
    );
    let mut empty: ZeNamespaceListResult = common::sized_zeroed();
    empty.abi_reserved = 1;
    assert_eq!(
        ze_namespace_list_result_free(&mut empty),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        last_error(0),
        "empty result has a nonzero allocation generation"
    );
    let id = ZeDocId { high: 0, low: 1 };
    let request = ZeGetRequest {
        ids: &id,
        id_count: 1,
        ..common::sized_zeroed()
    };
    let mut result: ZeGetResult = common::sized_zeroed();
    assert_eq!(
        ze_get(store.handle, &request, &mut result),
        ZeErrorCode::ZeOk
    );
    result.missing_count = 2;
    assert_eq!(
        ze_get_result_free(&mut result),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        last_error(0),
        "get result missing count exceeds document count"
    );
    result.missing_count = 0;
    assert_eq!(unsafe { (*result.documents).doc_id }, id);
    assert_eq!(ze_get_result_free(&mut result), ZeErrorCode::ZeOk);
}

#[test]
fn purge_busy_and_consumed_tokens_are_typed_without_losing_the_first_request() {
    let store = common::TestStore::new();
    assert_eq!(common::ingest_rows(store.handle, 2, 1), ZeErrorCode::ZeOk);
    let id = ZeDocId { high: 0, low: 1 };
    let request = ZePurgeRequest {
        doc_ids: &id,
        doc_id_count: 1,
        ..common::sized_zeroed()
    };
    let mut token: ZePurgeTokenReport = common::sized_zeroed();
    assert_eq!(
        ze_purge(store.handle, &request, &mut token),
        ZeErrorCode::ZeOk
    );
    assert_eq!(token.is_no_op, 0);
    let mut duplicate: ZePurgeTokenReport = common::sized_zeroed();
    assert_eq!(
        ze_purge(store.handle, &request, &mut duplicate),
        ZeErrorCode::ZeErrBusy
    );
    assert!(last_error(store.handle).contains("physical purge is already pending"));
    let wait = ZeAwaitPurgeRequest {
        token_id: token.token_id,
        ..common::sized_zeroed()
    };
    let mut report: ZePurgeReport = common::sized_zeroed();
    assert_eq!(
        ze_await_physical_purge(store.handle, &wait, &mut report),
        ZeErrorCode::ZeOk
    );
    assert_eq!(report.is_no_op, 0);
    assert_eq!(
        ze_await_physical_purge(store.handle, &wait, &mut report),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        last_error(store.handle),
        "purge token is unknown for this handle"
    );
    let get = ZeGetRequest {
        ids: &id,
        id_count: 1,
        ..common::sized_zeroed()
    };
    let mut result: ZeGetResult = common::sized_zeroed();
    assert_eq!(ze_get(store.handle, &get, &mut result), ZeErrorCode::ZeOk);
    assert_eq!(result.missing_count, 1);
    assert_eq!(ze_get_result_free(&mut result), ZeErrorCode::ZeOk);
}

#[test]
fn read_only_maintenance_and_purge_retain_access_mode_errors() {
    let mut store = common::TestStore::new();
    assert_eq!(common::ingest_rows(store.handle, 1, 1), ZeErrorCode::ZeOk);
    assert_eq!(store.close(), ZeErrorCode::ZeOk);
    let bytes = store.path.to_string_lossy().into_owned().into_bytes();
    let request = ZeOpenRequest {
        access_mode: 1,
        ..open_request(&bytes)
    };
    let mut handle = 0;
    assert_eq!(ze_open(&request, &mut handle), ZeErrorCode::ZeOk);
    let maintain = ZeMaintainRequest {
        wall_time_ns: 1_000_000_000,
        bytes: u64::MAX,
        ..common::sized_zeroed()
    };
    let mut report: ZeMaintainReport = common::sized_zeroed();
    assert_eq!(
        ze_maintain(handle, &maintain, &mut report),
        ZeErrorCode::ZeErrAccessMode
    );
    assert!(last_error(handle).contains("read-only"));
    let id = ZeDocId { high: 0, low: 1 };
    let purge = ZePurgeRequest {
        doc_ids: &id,
        doc_id_count: 1,
        ..common::sized_zeroed()
    };
    let mut token: ZePurgeTokenReport = common::sized_zeroed();
    assert_eq!(
        ze_purge(handle, &purge, &mut token),
        ZeErrorCode::ZeErrAccessMode
    );
    assert!(last_error(handle).contains("read-only"));
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}

#[test]
fn filtered_search_reports_cancelled_and_invalid_vector_errors_with_empty_output() {
    let store = common::TestStore::new();
    assert_eq!(common::ingest_rows(store.handle, 32, 1), ZeErrorCode::ZeOk);
    let nodes = [node(6, 0, &[])];
    let filter = ZeFilter {
        nodes: nodes.as_ptr(),
        node_count: 1,
        ..common::sized_zeroed()
    };
    let vector = [1.0_f32];
    let mut request = ZeSearchFilteredRequest {
        search: common::valid_search_request(&vector),
        filter: &filter,
        ..common::sized_zeroed()
    };
    let mut token = 0;
    assert_eq!(ze_cancel_token_create(&mut token), ZeErrorCode::ZeOk);
    assert_eq!(ze_cancel_token_cancel(token), ZeErrorCode::ZeOk);
    request.search.cancel_token = token;
    let mut result: ZeSearchResult = common::sized_zeroed();
    assert_eq!(
        ze_search_filtered(store.handle, &request, &mut result),
        ZeErrorCode::ZeErrCancelled
    );
    assert!(result.hits.is_null());
    assert_eq!(result.hit_count, 0);
    assert!(last_error(store.handle).contains("cancel"));
    assert_eq!(ze_cancel_token_free(token), ZeErrorCode::ZeOk);
    request.search.cancel_token = 0;
    let invalid_vector = [f32::NAN];
    request.search.vector = invalid_vector.as_ptr();
    assert_eq!(
        ze_search_filtered(store.handle, &request, &mut result),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert!(result.hits.is_null());
    assert_eq!(result.hit_count, 0);
    assert!(last_error(store.handle).contains("finite"));
    assert_eq!(ze_search_result_free(&mut result), ZeErrorCode::ZeOk);
}

#[test]
fn empty_mutation_batches_and_zero_dimensions_never_advance_generation() {
    let store = common::TestStore::new();
    let generation = count_nodes(store.handle, &[node(6, 0, &[])]).1.generation;
    let mut report: ZeMutationReport = common::sized_zeroed();
    let mut ingest: ZeIngestRequest = common::sized_zeroed();
    ingest.dimension = 1;
    assert_eq!(
        ze_ingest(store.handle, &ingest, &mut report),
        ZeErrorCode::ZeErrEmptyBatch
    );
    assert_eq!(last_error(store.handle), "ingest batch is empty");
    ingest.dimension = 0;
    assert_eq!(
        ze_ingest(store.handle, &ingest, &mut report),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(last_error(store.handle), "ingest dimension must be nonzero");
    let mut upsert: ZeUpsertRequest = common::sized_zeroed();
    upsert.dimension = 1;
    assert_eq!(
        ze_upsert(store.handle, &upsert, &mut report),
        ZeErrorCode::ZeErrEmptyBatch
    );
    upsert.dimension = 0;
    assert_eq!(
        ze_upsert(store.handle, &upsert, &mut report),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(last_error(store.handle), "upsert dimension must be nonzero");
    let delete: ZeDeleteRequest = common::sized_zeroed();
    assert_eq!(
        ze_delete(store.handle, &delete, &mut report),
        ZeErrorCode::ZeErrEmptyBatch
    );
    assert_eq!(last_error(store.handle), "delete batch is empty");
    let purge: ZePurgeRequest = common::sized_zeroed();
    let mut token: ZePurgeTokenReport = common::sized_zeroed();
    assert_eq!(
        ze_purge(store.handle, &purge, &mut token),
        ZeErrorCode::ZeErrEmptyBatch
    );
    assert_eq!(last_error(store.handle), "purge id batch is empty");
    assert_eq!(
        count_nodes(store.handle, &[node(6, 0, &[])]).1.generation,
        generation
    );
}

#[test]
fn scan_cursor_and_timestamp_flags_reject_ambiguous_requests() {
    let store = common::TestStore::new();
    let base = ZeScanRequest {
        limit: 1,
        ..common::sized_zeroed()
    };
    let cases = [
        (
            ZeScanRequest { order: 99, ..base },
            "scan order discriminant is out of range",
        ),
        (
            ZeScanRequest {
                cursor_next_row: 1,
                ..base
            },
            "scan start cursor fields must be zero when cursor_generation is zero",
        ),
        (
            ZeScanRequest {
                cursor_generation: 1,
                cursor_phase: 1,
                cursor_segment_id: [1; 16],
                ..base
            },
            "active scan cursor segment id must be zero",
        ),
        (
            ZeScanRequest {
                cursor_generation: 1,
                cursor_phase: 99,
                ..base
            },
            "scan cursor_phase discriminant is out of range",
        ),
        (
            ZeScanRequest {
                start_ts: 1,
                ..base
            },
            "scan timestamp bounds require has_timestamp_range",
        ),
    ];
    for (request, message) in cases {
        let mut result: ZeScanResult = common::sized_zeroed();
        assert_eq!(
            ze_scan(store.handle, &request, &mut result),
            ZeErrorCode::ZeErrInvalidArgument
        );
        assert_eq!(last_error(store.handle), message);
        assert!(result.documents.is_null());
        assert_eq!(result.document_count, 0);
    }
    let request = ZeCountRequest {
        end_ts: 1,
        ..common::sized_zeroed()
    };
    let mut result: ZeCountResult = common::sized_zeroed();
    assert_eq!(
        ze_count(store.handle, &request, &mut result),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        last_error(store.handle),
        "count timestamp bounds require has_timestamp_range"
    );
}

#[test]
fn search_options_reject_reserved_bits_conflicting_controls_and_unknown_profiles() {
    let store = common::TestStore::new();
    assert_eq!(common::ingest_rows(store.handle, 1, 1), ZeErrorCode::ZeOk);
    let vector = [1.0];
    let base = common::valid_search_request(&vector);
    let mut token = 0;
    assert_eq!(ze_cancel_token_create(&mut token), ZeErrorCode::ZeOk);
    for (request, message) in [
        (
            ZeSearchRequest {
                reserved: 1,
                ..base
            },
            "search reserved field must be zero",
        ),
        (
            ZeSearchRequest {
                graph_profile: 99,
                ..base
            },
            "graph_profile discriminant is out of range",
        ),
        (
            ZeSearchRequest {
                cancel_token: token,
                deadline_ns: 1,
                ..base
            },
            "search accepts either a cancel token or a deadline, not both",
        ),
    ] {
        let mut result: ZeSearchResult = common::sized_zeroed();
        assert_eq!(
            ze_search(store.handle, &request, &mut result),
            ZeErrorCode::ZeErrInvalidArgument
        );
        assert_eq!(last_error(store.handle), message);
        assert!(result.hits.is_null());
        assert_eq!(result.hit_count, 0);
        assert_eq!(ze_search_result_free(&mut result), ZeErrorCode::ZeOk);
    }
    assert_eq!(ze_cancel_token_free(token), ZeErrorCode::ZeOk);
    let angular = ZeSearchRequest {
        graph_profile: 1,
        tier: 3,
        graph_ef: 2,
        ..base
    };
    let mut result: ZeSearchResult = common::sized_zeroed();
    assert_eq!(
        ze_search(store.handle, &angular, &mut result),
        ZeErrorCode::ZeOk
    );
    assert_eq!(result.hit_count, 1);
    assert_eq!(unsafe { (*result.hits).doc_id.low }, 1);
    assert_eq!(ze_search_result_free(&mut result), ZeErrorCode::ZeOk);
}
