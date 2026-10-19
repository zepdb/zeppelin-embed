#![cfg(all(feature = "text", target_os = "macos"))]
mod common;
use zeppelin_embed_ffi::*;

#[test]
fn text_open_model_offline_miss_is_typed_without_starting_mlx() {
    let cache = tempfile::tempdir().unwrap();
    let cache_path = cache.path().to_str().unwrap();
    let store_path = cache.path().join("store");
    let store_path = store_path.to_str().unwrap();
    let repo = "fixture/tiny";
    let revision = "0123456789abcdef0123456789abcdef01234567";
    let pin = "00".repeat(32);
    let mut request: ZeTextOpenModelRequest = common::sized_zeroed();
    request.store = common::sized_zeroed();
    request.store.path = store_path.as_ptr();
    request.store.path_len = store_path.len();
    request.store.commit_tier = 1;
    request.store.max_resident_bytes = u64::MAX;
    request.store.max_temp_bytes = u64::MAX;
    request.repo = repo.as_ptr();
    request.repo_len = repo.len();
    request.revision = revision.as_ptr();
    request.revision_len = revision.len();
    request.manifest_sha256 = pin.as_ptr();
    request.manifest_sha256_len = pin.len();
    request.cache_dir = cache_path.as_ptr();
    request.cache_dir_len = cache_path.len();
    request.offline = 1;
    let mut handle = 0;
    assert_eq!(
        ze_text_open_model(&request, &mut handle),
        ZeErrorCode::ZeErrNotFound
    );
    assert_eq!(handle, 0);
    assert!(!std::path::Path::new(store_path).exists());
    for flags in [
        ZE_MODEL_ALLOW_HTTP,
        ZE_MODEL_SEND_TOKEN_TO_ENDPOINT,
        ZE_MODEL_ALLOW_HTTP | ZE_MODEL_SEND_TOKEN_TO_ENDPOINT,
    ] {
        request.flags = flags;
        assert_eq!(
            ze_text_open_model(&request, &mut handle),
            ZeErrorCode::ZeErrNotFound
        );
    }
    request.flags = 4;
    assert_eq!(
        ze_text_open_model(&request, &mut handle),
        ZeErrorCode::ZeErrInvalidArgument
    );
    request.flags = 0;
    request.offline = 2;
    assert_eq!(
        ze_text_open_model(&request, &mut handle),
        ZeErrorCode::ZeErrInvalidArgument
    );
}
