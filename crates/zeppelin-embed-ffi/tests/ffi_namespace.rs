mod common;

use std::mem::size_of;

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

fn canonical_epoch_identity(model_id: &[u8], dimensions: u32) -> ZeEpochIdentity {
    let version = b"1";
    let tower = ZeEmbeddingTower {
        model_id: model_id.as_ptr(),
        model_id_len: model_id.len(),
        model_version: version.as_ptr(),
        model_version_len: version.len(),
        weights_digest: std::ptr::null(),
        weights_digest_len: 0,
        dims: dimensions,
        normalization: 0,
        prompt_prefix: std::ptr::null(),
        prompt_prefix_len: 0,
        max_tokens: 0,
        runtime: 3,
        compute_units: 1,
        has_os_build: 0,
        os_build: std::ptr::null(),
        os_build_len: 0,
    };
    let request = ZeEpochRequest {
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
    let mut identity: ZeEpochIdentity = common::sized_zeroed();
    assert_eq!(
        ze_epoch_identity(&request, &mut identity),
        ZeErrorCode::ZeOk
    );
    identity
}

#[test]
fn namespace_create_reopen_identical_spec_is_usable_for_ingest_and_search() {
    let root = tempfile::tempdir().expect("temporary namespace root");
    let root_bytes = root.path().to_string_lossy().into_owned().into_bytes();
    let name = b"documents";
    let spec = ZeNamespaceSpec {
        abi_size: size_of::<ZeNamespaceSpec>() as u32,
        abi_reserved: 0,
        attributes: std::ptr::null(),
        attribute_count: 0,
        has_vector_space: 1,
        dimensions: 4,
        normalization: 0,
        epoch: std::ptr::null(),
    };
    let request = ZeNamespaceOpenRequest {
        abi_size: size_of::<ZeNamespaceOpenRequest>() as u32,
        abi_reserved: 0,
        root: root_bytes.as_ptr(),
        root_len: root_bytes.len(),
        name: name.as_ptr(),
        name_len: name.len(),
        open: open_settings(),
        spec: &spec,
    };

    let mut handle = 0;
    assert_eq!(ze_namespace_open(&request, &mut handle), ZeErrorCode::ZeOk);
    assert_ne!(handle, 0);
    let expected = canonical_epoch_identity(b"zeppelin.vector-space", 4);
    let mut current: ZeEpochIdentity = common::sized_zeroed();
    assert_eq!(ze_epoch_current(handle, &mut current), ZeErrorCode::ZeOk);
    assert_eq!(current.embedding_epoch, expected.embedding_epoch);
    assert_eq!(current.tokenizer_epoch, expected.tokenizer_epoch);
    assert_eq!(common::ingest_rows(handle, 1, 4), ZeErrorCode::ZeOk);
    let query = vec![0.25_f32, 0.5, 0.75, 1.0];
    let search = common::valid_search_request(&query);
    let mut result: ZeSearchResult = common::sized_zeroed();
    assert_eq!(ze_search(handle, &search, &mut result), ZeErrorCode::ZeOk);
    assert_eq!(result.hit_count, 1);
    assert_eq!(ze_search_result_free(&mut result), ZeErrorCode::ZeOk);
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);

    let mut reopened = 0;
    assert_eq!(
        ze_namespace_open(&request, &mut reopened),
        ZeErrorCode::ZeOk
    );
    assert_ne!(reopened, 0);
    assert_eq!(ze_close(reopened), ZeErrorCode::ZeOk);
}

#[test]
fn namespace_reopen_with_different_schema_returns_schema_mismatch() {
    let root = tempfile::tempdir().expect("temporary namespace root");
    let root_bytes = root.path().to_string_lossy().into_owned().into_bytes();
    let name = b"schema";
    let first_name = b"category";
    let first_attribute = ZeAttributeDefinition {
        attribute_id: 1,
        name: first_name.as_ptr(),
        name_len: first_name.len(),
        attribute_type: 5,
        nullable: 1,
    };
    let first_spec = ZeNamespaceSpec {
        abi_size: size_of::<ZeNamespaceSpec>() as u32,
        abi_reserved: 0,
        attributes: &first_attribute,
        attribute_count: 1,
        has_vector_space: 1,
        dimensions: 4,
        normalization: 0,
        epoch: std::ptr::null(),
    };
    let mut request = ZeNamespaceOpenRequest {
        abi_size: size_of::<ZeNamespaceOpenRequest>() as u32,
        abi_reserved: 0,
        root: root_bytes.as_ptr(),
        root_len: root_bytes.len(),
        name: name.as_ptr(),
        name_len: name.len(),
        open: open_settings(),
        spec: &first_spec,
    };
    let mut handle = 0;
    assert_eq!(ze_namespace_open(&request, &mut handle), ZeErrorCode::ZeOk);
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);

    let second_name = b"language";
    let second_attribute = ZeAttributeDefinition {
        name: second_name.as_ptr(),
        name_len: second_name.len(),
        ..first_attribute
    };
    let second_spec = ZeNamespaceSpec {
        attributes: &second_attribute,
        ..first_spec
    };
    request.spec = &second_spec;
    assert_eq!(
        ze_namespace_open(&request, &mut handle),
        ZeErrorCode::ZeErrSchemaMismatch
    );
}

#[test]
fn namespace_reopen_with_different_epoch_returns_epoch_mismatch() {
    let root = tempfile::tempdir().expect("temporary namespace root");
    let root_bytes = root.path().to_string_lossy().into_owned().into_bytes();
    let name = b"epochs";
    let first_epoch = common::EpochFixture::new(4);
    let first_epoch_request = first_epoch.request();
    let first_spec = ZeNamespaceSpec {
        abi_size: size_of::<ZeNamespaceSpec>() as u32,
        abi_reserved: 0,
        attributes: std::ptr::null(),
        attribute_count: 0,
        has_vector_space: 1,
        dimensions: 4,
        normalization: 0,
        epoch: &first_epoch_request,
    };
    let mut request = ZeNamespaceOpenRequest {
        abi_size: size_of::<ZeNamespaceOpenRequest>() as u32,
        abi_reserved: 0,
        root: root_bytes.as_ptr(),
        root_len: root_bytes.len(),
        name: name.as_ptr(),
        name_len: name.len(),
        open: open_settings(),
        spec: &first_spec,
    };
    let mut handle = 0;
    assert_eq!(ze_namespace_open(&request, &mut handle), ZeErrorCode::ZeOk);
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);

    let mut second_epoch = common::EpochFixture::new(4);
    second_epoch.model_version = b"2".to_vec();
    let second_epoch_request = second_epoch.request();
    let second_spec = ZeNamespaceSpec {
        epoch: &second_epoch_request,
        ..first_spec
    };
    request.spec = &second_spec;
    assert_eq!(
        ze_namespace_open(&request, &mut handle),
        ZeErrorCode::ZeErrEpochMismatch
    );
}

#[test]
fn record_only_namespace_creates_reopens_and_reports_canonical_epoch() {
    let root = tempfile::tempdir().expect("temporary namespace root");
    let root_bytes = root.path().to_string_lossy().into_owned().into_bytes();
    let name = b"records";
    let spec = ZeNamespaceSpec {
        abi_size: size_of::<ZeNamespaceSpec>() as u32,
        abi_reserved: 0,
        attributes: std::ptr::null(),
        attribute_count: 0,
        has_vector_space: 0,
        dimensions: 0,
        normalization: 0,
        epoch: std::ptr::null(),
    };
    let request = ZeNamespaceOpenRequest {
        abi_size: size_of::<ZeNamespaceOpenRequest>() as u32,
        abi_reserved: 0,
        root: root_bytes.as_ptr(),
        root_len: root_bytes.len(),
        name: name.as_ptr(),
        name_len: name.len(),
        open: open_settings(),
        spec: &spec,
    };
    let expected = canonical_epoch_identity(b"zeppelin.record-only", 1);
    let mut handle = 0;
    assert_eq!(ze_namespace_open(&request, &mut handle), ZeErrorCode::ZeOk);
    let mut current: ZeEpochIdentity = common::sized_zeroed();
    assert_eq!(ze_epoch_current(handle, &mut current), ZeErrorCode::ZeOk);
    assert_eq!(current.embedding_epoch, expected.embedding_epoch);
    assert_eq!(current.tokenizer_epoch, expected.tokenizer_epoch);
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
    assert_eq!(ze_namespace_open(&request, &mut handle), ZeErrorCode::ZeOk);
    assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
}

#[test]
fn namespace_open_rejects_every_invalid_name() {
    let root = tempfile::tempdir().expect("temporary namespace root");
    let root_bytes = root.path().to_string_lossy().into_owned().into_bytes();
    let spec = ZeNamespaceSpec {
        abi_size: size_of::<ZeNamespaceSpec>() as u32,
        abi_reserved: 0,
        attributes: std::ptr::null(),
        attribute_count: 0,
        has_vector_space: 1,
        dimensions: 4,
        normalization: 0,
        epoch: std::ptr::null(),
    };
    let invalid_names = vec![
        Vec::new(),
        vec![b'a'; 256],
        b".leading-dot".to_vec(),
        b"-leading-dash".to_vec(),
        b"_leading-underscore".to_vec(),
        b"embedded/slash".to_vec(),
        b"..".to_vec(),
        b"embedded\0nul".to_vec(),
        "non-ascii-é".as_bytes().to_vec(),
    ];
    for name in invalid_names {
        let request = ZeNamespaceOpenRequest {
            abi_size: size_of::<ZeNamespaceOpenRequest>() as u32,
            abi_reserved: 0,
            root: root_bytes.as_ptr(),
            root_len: root_bytes.len(),
            name: name.as_ptr(),
            name_len: name.len(),
            open: open_settings(),
            spec: &spec,
        };
        let mut handle = 0;
        let code = ze_namespace_open(&request, &mut handle);
        if handle != 0 {
            assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
        }
        assert_eq!(
            code,
            ZeErrorCode::ZeErrInvalidArgument,
            "invalid namespace name {name:?}"
        );
    }
}

#[test]
fn namespace_list_returns_sorted_manifest_directories_and_empty_root() {
    let root = tempfile::tempdir().expect("temporary namespace root");
    let root_bytes = root.path().to_string_lossy().into_owned().into_bytes();
    let list_request = ZeNamespaceListRequest {
        abi_size: size_of::<ZeNamespaceListRequest>() as u32,
        abi_reserved: 0,
        root: root_bytes.as_ptr(),
        root_len: root_bytes.len(),
    };
    let missing = root.path().join("missing");
    let missing_bytes = missing.to_string_lossy().into_owned().into_bytes();
    let missing_request = ZeNamespaceListRequest {
        root: missing_bytes.as_ptr(),
        root_len: missing_bytes.len(),
        ..list_request
    };
    let mut result: ZeNamespaceListResult = common::sized_zeroed();
    assert_eq!(
        ze_namespace_list(&missing_request, &mut result),
        ZeErrorCode::ZeErrIo
    );
    result = common::sized_zeroed();
    assert_eq!(
        ze_namespace_list(&list_request, &mut result),
        ZeErrorCode::ZeOk
    );
    assert_eq!(result.entry_count, 0);
    assert!(result.entries.is_null());
    assert_eq!(
        ze_namespace_list_result_free(&mut result),
        ZeErrorCode::ZeOk
    );

    std::fs::create_dir(root.path().join("not-a-namespace"))
        .expect("create non-namespace directory");
    let spec = ZeNamespaceSpec {
        abi_size: size_of::<ZeNamespaceSpec>() as u32,
        abi_reserved: 0,
        attributes: std::ptr::null(),
        attribute_count: 0,
        has_vector_space: 1,
        dimensions: 4,
        normalization: 0,
        epoch: std::ptr::null(),
    };
    for name in [b"zeta".as_slice(), b"alpha".as_slice()] {
        let request = ZeNamespaceOpenRequest {
            abi_size: size_of::<ZeNamespaceOpenRequest>() as u32,
            abi_reserved: 0,
            root: root_bytes.as_ptr(),
            root_len: root_bytes.len(),
            name: name.as_ptr(),
            name_len: name.len(),
            open: open_settings(),
            spec: &spec,
        };
        let mut handle = 0;
        assert_eq!(ze_namespace_open(&request, &mut handle), ZeErrorCode::ZeOk);
        assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
    }

    result = common::sized_zeroed();
    assert_eq!(
        ze_namespace_list(&list_request, &mut result),
        ZeErrorCode::ZeOk
    );
    let entries = unsafe { std::slice::from_raw_parts(result.entries, result.entry_count) };
    let names = entries
        .iter()
        .map(|entry| unsafe { std::slice::from_raw_parts(entry.name, entry.name_len) })
        .collect::<Vec<_>>();
    assert_eq!(names, vec![b"alpha".as_slice(), b"zeta".as_slice()]);
    let mut forged = result;
    forged.entry_count += 1;
    assert_eq!(
        ze_namespace_list_result_free(&mut forged),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        ze_namespace_list_result_free(&mut result),
        ZeErrorCode::ZeOk
    );
    assert_eq!(
        ze_namespace_list_result_free(&mut result),
        ZeErrorCode::ZeOk
    );
}

#[test]
fn namespace_requests_reject_null_zero_and_wrong_sizes_without_crashing() {
    let root = tempfile::tempdir().expect("temporary namespace root");
    let root_bytes = root.path().to_string_lossy().into_owned().into_bytes();
    let name = b"validation";
    let spec = ZeNamespaceSpec {
        abi_size: size_of::<ZeNamespaceSpec>() as u32,
        abi_reserved: 0,
        attributes: std::ptr::null(),
        attribute_count: 0,
        has_vector_space: 1,
        dimensions: 4,
        normalization: 0,
        epoch: std::ptr::null(),
    };
    let valid_open = ZeNamespaceOpenRequest {
        abi_size: size_of::<ZeNamespaceOpenRequest>() as u32,
        abi_reserved: 0,
        root: root_bytes.as_ptr(),
        root_len: root_bytes.len(),
        name: name.as_ptr(),
        name_len: name.len(),
        open: open_settings(),
        spec: &spec,
    };
    let mut handle = 0;
    assert_eq!(
        ze_namespace_open(std::ptr::null(), &mut handle),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        ze_namespace_open(&valid_open, std::ptr::null_mut()),
        ZeErrorCode::ZeErrInvalidArgument
    );
    for abi_size in [0, size_of::<ZeNamespaceOpenRequest>() as u32 - 1] {
        let request = ZeNamespaceOpenRequest {
            abi_size,
            ..valid_open
        };
        assert_eq!(
            ze_namespace_open(&request, &mut handle),
            ZeErrorCode::ZeErrInvalidArgument
        );
    }

    let valid_list = ZeNamespaceListRequest {
        abi_size: size_of::<ZeNamespaceListRequest>() as u32,
        abi_reserved: 0,
        root: root_bytes.as_ptr(),
        root_len: root_bytes.len(),
    };
    let mut result: ZeNamespaceListResult = common::sized_zeroed();
    assert_eq!(
        ze_namespace_list(std::ptr::null(), &mut result),
        ZeErrorCode::ZeErrInvalidArgument
    );
    assert_eq!(
        ze_namespace_list(&valid_list, std::ptr::null_mut()),
        ZeErrorCode::ZeErrInvalidArgument
    );
    for abi_size in [0, size_of::<ZeNamespaceListRequest>() as u32 - 1] {
        let request = ZeNamespaceListRequest {
            abi_size,
            ..valid_list
        };
        result = common::sized_zeroed();
        assert_eq!(
            ze_namespace_list(&request, &mut result),
            ZeErrorCode::ZeErrInvalidArgument
        );
    }
}

#[test]
fn record_only_namespace_rejects_dimensions_above_one_and_a_caller_epoch() {
    let root = tempfile::tempdir().expect("temporary namespace root");
    let root_bytes = root.path().to_string_lossy().into_owned().into_bytes();
    let name = b"bad-records";
    let spec = ZeNamespaceSpec {
        abi_size: size_of::<ZeNamespaceSpec>() as u32,
        abi_reserved: 0,
        attributes: std::ptr::null(),
        attribute_count: 0,
        has_vector_space: 0,
        dimensions: 2,
        normalization: 0,
        epoch: std::ptr::null(),
    };
    let request = ZeNamespaceOpenRequest {
        abi_size: size_of::<ZeNamespaceOpenRequest>() as u32,
        abi_reserved: 0,
        root: root_bytes.as_ptr(),
        root_len: root_bytes.len(),
        name: name.as_ptr(),
        name_len: name.len(),
        open: open_settings(),
        spec: &spec,
    };
    let mut handle = 0;
    let code = ze_namespace_open(&request, &mut handle);
    if handle != 0 {
        assert_eq!(ze_close(handle), ZeErrorCode::ZeOk);
        handle = 0;
    }
    assert_eq!(code, ZeErrorCode::ZeErrInvalidArgument);

    let epoch = common::EpochFixture::new(1);
    let epoch_request = epoch.request();
    let epoch_spec = ZeNamespaceSpec {
        dimensions: 1,
        epoch: &epoch_request,
        ..spec
    };
    let epoch_request = ZeNamespaceOpenRequest {
        spec: &epoch_spec,
        ..request
    };
    assert_eq!(
        ze_namespace_open(&epoch_request, &mut handle),
        ZeErrorCode::ZeErrInvalidArgument
    );
}
