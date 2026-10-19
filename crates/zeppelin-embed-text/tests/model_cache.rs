#![cfg(target_os = "macos")]

use zeppelin_embed_text::{ModelError, ModelOpenOptions, ModelRef, resolve_model};

#[test]
fn offline_miss_is_typed_and_never_downloads() {
    let cache = tempfile::tempdir().unwrap();
    let model = ModelRef {
        repo: "fixture/tiny".into(),
        revision: "0123456789abcdef0123456789abcdef01234567".into(),
        manifest_sha256: "00".repeat(32),
    };
    let options = ModelOpenOptions {
        cache_dir: Some(cache.path().into()),
        offline: true,
        endpoint: Some("http://127.0.0.1:1".into()),
        ..Default::default()
    };
    assert!(matches!(
        resolve_model(&model, &options),
        Err(ModelError::OfflineMiss)
    ));
}

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

const SHA_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

fn sha(bytes: &[u8]) -> String {
    // Independent system utility supplies the manifest pin, not resolver code.
    let mut child = std::process::Command::new("shasum")
        .arg("-a")
        .arg("256")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(bytes).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .into()
}

fn manifest(path: &str, digest: &str, size: u64) -> Vec<u8> {
    format!(r#"{{"format":1,"name":"tiny","bundle":"tiny.zem","query_coreml":null,"files":[{{"path":"{path}","size":{size},"sha256":"{digest}"}}]}}"#).into_bytes()
}

struct Fixture {
    endpoint: String,
    requests: Arc<AtomicUsize>,
    authorizations: Arc<AtomicUsize>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}
impl Fixture {
    fn new(manifest: Vec<u8>, body: Vec<u8>, declared_length: usize, status: u16) -> Self {
        Self::with_redirect(manifest, body, declared_length, status, None)
    }
    fn with_redirect(
        manifest: Vec<u8>,
        body: Vec<u8>,
        declared_length: usize,
        status: u16,
        redirect: Option<String>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let authorizations = Arc::new(AtomicUsize::new(0));
        let auth_count = authorizations.clone();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (count, stopped) = (requests.clone(), stop.clone());
        let join = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    if stream.read(&mut byte).unwrap() == 0 {
                        break;
                    }
                    request.push(byte[0]);
                }
                count.fetch_add(1, Ordering::SeqCst);
                if String::from_utf8_lossy(&request)
                    .lines()
                    .any(|line| line.to_ascii_lowercase().starts_with("authorization:"))
                {
                    auth_count.fetch_add(1, Ordering::SeqCst);
                }
                let is_manifest =
                    String::from_utf8_lossy(&request).contains("/zeppelin-model.json ");
                if is_manifest && let Some(location) = &redirect {
                    write!(stream, "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    continue;
                }
                let (data, length, code) = if is_manifest {
                    (&manifest, manifest.len(), 200)
                } else {
                    (&body, declared_length, status)
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {code} Fixture\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
                );
                if !is_manifest && data.len() > 64 * 1024 {
                    for chunk in data.chunks(64 * 1024) {
                        if stream.write_all(chunk).is_err() {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(110));
                    }
                } else {
                    let _ = stream.write_all(data);
                }
            }
        });
        Self {
            endpoint: format!("http://{addr}"),
            requests,
            authorizations,
            stop,
            join: Some(join),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.endpoint.trim_start_matches("http://"));
        self.join.take().unwrap().join().unwrap();
    }
}
fn model(manifest: &[u8]) -> ModelRef {
    ModelRef {
        repo: "fixture/tiny".into(),
        revision: "0123456789abcdef0123456789abcdef01234567".into(),
        manifest_sha256: sha(manifest),
    }
}

#[test]
fn downloads_verified_bundle_then_reuses_cache_and_offline_hit_without_requests() {
    let bytes = manifest("tiny.zem", SHA_ABC, 3);
    let model = model(&bytes);
    let server = Fixture::new(bytes, b"abc".to_vec(), 3, 200);
    let cache = tempfile::tempdir().unwrap();
    let progress = std::cell::RefCell::new(Vec::new());
    let callback = |done, total| progress.borrow_mut().push((done, total));
    let mut options = ModelOpenOptions {
        cache_dir: Some(cache.path().into()),
        endpoint: Some(server.endpoint.clone()),
        allow_http: true,
        progress: Some(&callback),
        ..Default::default()
    };
    let bundle = resolve_model(&model, &options).unwrap();
    assert_eq!(std::fs::read(&bundle).unwrap(), b"abc");
    assert!(
        bundle
            .parent()
            .unwrap()
            .join(".zeppelin-verified")
            .is_file()
    );
    assert_eq!(server.requests.load(Ordering::SeqCst), 2);
    assert_eq!(resolve_model(&model, &options).unwrap(), bundle);
    options.offline = true;
    assert_eq!(resolve_model(&model, &options).unwrap(), bundle);
    assert_eq!(server.requests.load(Ordering::SeqCst), 2);
    assert_eq!(progress.borrow().last(), Some(&(3, 3)));
}

fn cached(cache: &std::path::Path, model: &ModelRef, bytes: &[u8]) -> std::path::PathBuf {
    let entry = cache.join("fixture%2Ftiny").join(&model.revision);
    std::fs::create_dir_all(&entry).unwrap();
    std::fs::write(entry.join("zeppelin-model.json"), bytes).unwrap();
    std::fs::write(entry.join("tiny.zem"), b"abc").unwrap();
    entry
}

#[test]
fn offline_hit_verifies_cached_bundle() {
    let bytes = manifest("tiny.zem", SHA_ABC, 3);
    let model = model(&bytes);
    let cache = tempfile::tempdir().unwrap();
    let entry = cached(cache.path(), &model, &bytes);
    let options = ModelOpenOptions {
        cache_dir: Some(cache.path().into()),
        offline: true,
        ..Default::default()
    };
    assert_eq!(
        resolve_model(&model, &options).unwrap(),
        entry.join("tiny.zem")
    );
}

#[test]
fn ctime_change_rejects_corruption_even_when_mtime_is_restored() {
    use std::os::unix::fs::MetadataExt;

    let bytes = manifest("tiny.zem", SHA_ABC, 3);
    let model = model(&bytes);
    let cache = tempfile::tempdir().unwrap();
    cached(cache.path(), &model, &bytes);
    let options = ModelOpenOptions {
        cache_dir: Some(cache.path().into()),
        offline: true,
        ..Default::default()
    };
    let bundle = resolve_model(&model, &options).unwrap();
    let before = std::fs::metadata(&bundle).unwrap();
    // Separate the ctime updates even on filesystems with coarse timestamps.
    std::thread::sleep(std::time::Duration::from_secs(1));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(&bundle)
        .unwrap();
    file.write_all(b"x").unwrap();
    file.set_modified(before.modified().unwrap()).unwrap();
    let after = file.metadata().unwrap();
    assert_eq!(before.ino(), after.ino());
    assert_eq!(before.len(), after.len());
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
    assert_ne!(
        (before.ctime(), before.ctime_nsec()),
        (after.ctime(), after.ctime_nsec())
    );
    assert!(matches!(
        resolve_model(&model, &options),
        Err(ModelError::Verification(_))
    ));
}

#[test]
fn wrong_length_marker_is_replaced_without_reading_it() {
    use std::os::unix::fs::PermissionsExt;

    let bytes = manifest("tiny.zem", SHA_ABC, 3);
    let model = model(&bytes);
    let cache = tempfile::tempdir().unwrap();
    let entry = cached(cache.path(), &model, &bytes);
    let options = ModelOpenOptions {
        cache_dir: Some(cache.path().into()),
        offline: true,
        ..Default::default()
    };
    resolve_model(&model, &options).unwrap();
    let marker = entry.join(".zeppelin-verified");
    let expected = std::fs::read(&marker).unwrap();
    std::fs::write(&marker, b"x").unwrap();
    assert_ne!(expected.len(), 1);
    std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o000)).unwrap();
    assert_eq!(
        std::fs::File::open(&marker).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        resolve_model(&model, &options).unwrap(),
        entry.join("tiny.zem")
    );
    assert_eq!(std::fs::read(&marker).unwrap(), expected);
}

#[test]
fn rejects_corrupt_incomplete_and_unpinned_cached_files() {
    let bytes = manifest("tiny.zem", SHA_ABC, 3);
    let model = model(&bytes);
    for corruption in [
        "checksum",
        "size",
        "missing",
        "manifest",
        "symlink",
        "extra-coreml",
    ] {
        let cache = tempfile::tempdir().unwrap();
        let entry = cached(cache.path(), &model, &bytes);
        match corruption {
            "checksum" => std::fs::write(entry.join("tiny.zem"), b"xyz").unwrap(),
            "size" => std::fs::write(entry.join("tiny.zem"), b"ab").unwrap(),
            "missing" => std::fs::remove_file(entry.join("tiny.zem")).unwrap(),
            "manifest" => std::fs::write(entry.join("zeppelin-model.json"), b"{}").unwrap(),
            "symlink" => {
                std::fs::remove_file(entry.join("tiny.zem")).unwrap();
                std::os::unix::fs::symlink("/etc/hosts", entry.join("tiny.zem")).unwrap();
            }
            "extra-coreml" => std::fs::create_dir(entry.join("tiny.mlmodelc")).unwrap(),
            _ => unreachable!(),
        }
        let options = ModelOpenOptions {
            cache_dir: Some(cache.path().into()),
            offline: true,
            ..Default::default()
        };
        assert!(
            matches!(
                resolve_model(&model, &options),
                Err(ModelError::Verification(_))
            ),
            "{corruption}"
        );
    }
}

#[test]
fn rejects_unsafe_paths_and_inconsistent_manifest_before_using_bundle() {
    for path in [
        "../tiny.zem",
        "/tiny.zem",
        "a//tiny.zem",
        "a/./tiny.zem",
        "a/../tiny.zem",
        "",
        "tiny%2ezem",
        "tiny\\\\zem",
    ] {
        let bytes = manifest(path, SHA_ABC, 3);
        let model = model(&bytes);
        let cache = tempfile::tempdir().unwrap();
        cached(cache.path(), &model, &bytes);
        let options = ModelOpenOptions {
            cache_dir: Some(cache.path().into()),
            offline: true,
            ..Default::default()
        };
        assert!(
            matches!(resolve_model(&model, &options), Err(ModelError::Invalid(_))),
            "{path}"
        );
    }
    // Cache metadata must never be admitted as a manifest payload.
    for reserved in [".zeppelin-verified", ".zeppelin-verified.tmp"] {
        let valid = String::from_utf8(manifest("tiny.zem", SHA_ABC, 3)).unwrap();
        let bytes = valid.replace(
            "\"files\":[",
            &format!(r#""files":[{{"path":"{reserved}","size":3,"sha256":"{SHA_ABC}"}},"#),
        );
        let model = model(bytes.as_bytes());
        let cache = tempfile::tempdir().unwrap();
        cached(cache.path(), &model, bytes.as_bytes());
        assert!(
            matches!(
                resolve_model(
                    &model,
                    &ModelOpenOptions {
                        cache_dir: Some(cache.path().into()),
                        offline: true,
                        ..Default::default()
                    }
                ),
                Err(ModelError::Invalid(_))
            ),
            "{reserved}"
        );
    }
    let valid = String::from_utf8(manifest("tiny.zem", SHA_ABC, 3)).unwrap();
    for bytes in [
        valid.replace("\"format\":1", "\"format\":2"),
        valid.replace("\"format\":1", "\"format\":1,\"format\":1"),
        valid.replace(
            "\"query_coreml\":null",
            "\"query_coreml\":\"tiny.mlmodelc\"",
        ),
        valid.replace("\"bundle\":\"tiny.zem\"", "\"bundle\":\"other.zem\""),
        valid.replace("\"size\":3", "\"size\":03"),
        format!("{valid} trailing"),
    ] {
        let model = model(bytes.as_bytes());
        let cache = tempfile::tempdir().unwrap();
        cached(cache.path(), &model, bytes.as_bytes());
        assert!(matches!(
            resolve_model(
                &model,
                &ModelOpenOptions {
                    cache_dir: Some(cache.path().into()),
                    offline: true,
                    ..Default::default()
                }
            ),
            Err(ModelError::Invalid(_))
        ));
    }
}

#[test]
fn downloads_reject_checksum_truncation_manifest_pin_traversal_and_http_failure() {
    for case in [
        "checksum",
        "truncated",
        "size",
        "manifest-pin",
        "traversal",
        "http",
    ] {
        let bytes = manifest(
            if case == "traversal" {
                "../tiny.zem"
            } else {
                "tiny.zem"
            },
            SHA_ABC,
            3,
        );
        let mut model = model(&bytes);
        if case == "manifest-pin" {
            model.manifest_sha256 = "00".repeat(32);
        }
        let body = if case == "checksum" {
            b"xyz".to_vec()
        } else if case == "size" {
            b"ab".to_vec()
        } else {
            b"abc".to_vec()
        };
        let server = Fixture::new(
            bytes,
            body.clone(),
            if case == "truncated" { 10 } else { body.len() },
            if case == "http" { 403 } else { 200 },
        );
        let cache = tempfile::tempdir().unwrap();
        let options = ModelOpenOptions {
            cache_dir: Some(cache.path().into()),
            endpoint: Some(server.endpoint.clone()),
            allow_http: true,
            ..Default::default()
        };
        let error = resolve_model(&model, &options).unwrap_err();
        match case {
            "traversal" => assert!(matches!(error, ModelError::Invalid(_))),
            "truncated" | "http" => assert!(matches!(
                error,
                ModelError::Download(_) | ModelError::Verification(_)
            )),
            _ => assert!(matches!(error, ModelError::Verification(_))),
        }
        let parent = cache.path().join("fixture%2Ftiny");
        assert!(!parent.join(&model.revision).exists());
        assert!(!parent.join(format!("{}.partial", model.revision)).exists());
        if matches!(case, "manifest-pin" | "traversal") {
            assert_eq!(server.requests.load(Ordering::SeqCst), 1);
        }
    }
}

#[test]
fn concurrent_resolvers_publish_one_complete_entry_without_staging_leftovers() {
    let bytes = manifest("tiny.zem", SHA_ABC, 3);
    let model = model(&bytes);
    let server = Fixture::new(bytes, b"abc".to_vec(), 3, 200);
    let cache = tempfile::tempdir().unwrap();
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let resolve = || {
            barrier.wait();
            resolve_model(
                &model,
                &ModelOpenOptions {
                    cache_dir: Some(cache.path().into()),
                    endpoint: Some(server.endpoint.clone()),
                    allow_http: true,
                    ..Default::default()
                },
            )
            .unwrap()
        };
        let a = scope.spawn(resolve);
        let b = scope.spawn(resolve);
        assert_eq!(a.join().unwrap(), b.join().unwrap());
    });
    assert_eq!(server.requests.load(Ordering::SeqCst), 2);
    let mut children: Vec<_> = std::fs::read_dir(cache.path().join("fixture%2Ftiny"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    children.sort();
    assert_eq!(
        children,
        vec![
            std::ffi::OsString::from(&model.revision),
            std::ffi::OsString::from(format!("{}.lock", model.revision))
        ]
    );
}

#[test]
fn options_and_environment_select_cache_and_offline_policy() {
    let bytes = manifest("tiny.zem", SHA_ABC, 3);
    let model = model(&bytes);
    if let Ok(mode) = std::env::var("ZE501_OPTIONS_CHILD") {
        let options = ModelOpenOptions {
            cache_dir: (mode == "option")
                .then(|| std::env::var_os("ZE501_OPTION_CACHE").unwrap().into()),
            endpoint: Some("http://127.0.0.1:1".into()),
            offline: mode == "default",
            ..Default::default()
        };
        assert!(resolve_model(&model, &options).is_ok());
        return;
    }
    for mode in ["default", "env", "option"] {
        let home = tempfile::tempdir().unwrap();
        let env_cache = home.path().join("env-cache");
        let option_cache = home.path().join("option-cache");
        let selected = match mode {
            "default" => home.path().join("Library/Caches/zeppelin-embed/models"),
            "env" => env_cache.clone(),
            _ => option_cache.clone(),
        };
        cached(&selected, &model, &bytes);
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "options_and_environment_select_cache_and_offline_policy",
            ])
            .env("ZE501_OPTIONS_CHILD", mode)
            .env("HOME", home.path())
            .env("ZE501_OPTION_CACHE", option_cache)
            .env_remove("ZE_MODEL_CACHE")
            .env("ZE_MODEL_OFFLINE", "1");
        if mode != "default" {
            command.env("ZE_MODEL_CACHE", env_cache);
        }
        assert!(command.status().unwrap().success(), "{mode}");
    }
}

#[test]
fn offline_coreml_directory_files_are_verified_individually() {
    let bytes = format!(r#"{{"files":[{{"sha256":"{SHA_ABC}","size":3,"path":"tiny.zem"}},{{"path":"tiny.mlmodelc/weights/data.bin","size":3,"sha256":"{SHA_ABC}"}}],"query_coreml":"tiny.mlmodelc","bundle":"tiny.zem","name":"tiny","format":1}}"#).into_bytes();
    let model = model(&bytes);
    let cache = tempfile::tempdir().unwrap();
    let entry = cached(cache.path(), &model, &bytes);
    std::fs::create_dir_all(entry.join("tiny.mlmodelc/weights")).unwrap();
    std::fs::write(entry.join("tiny.mlmodelc/weights/data.bin"), b"abc").unwrap();
    let options = ModelOpenOptions {
        cache_dir: Some(cache.path().into()),
        offline: true,
        ..Default::default()
    };
    assert!(resolve_model(&model, &options).is_ok());
    std::fs::write(entry.join("tiny.mlmodelc/weights/data.bin"), b"xyz").unwrap();
    assert!(matches!(
        resolve_model(&model, &options),
        Err(ModelError::Verification(_))
    ));
}

#[test]
fn token_predicate_requires_https_and_exact_authorized_host() {
    unsafe extern "C" {
        fn ze_model_token_allowed(
            url: *const std::ffi::c_char,
            authorized_host: *const std::ffi::c_char,
        ) -> i32;
    }
    for (url, endpoint_host, opt_in, expected) in [
        (
            "https://huggingface.co/org/model",
            "huggingface.co",
            false,
            true,
        ),
        ("https://cdn.example/model", "huggingface.co", false, false),
        (
            "http://huggingface.co/org/model",
            "huggingface.co",
            false,
            false,
        ),
        (
            "https://endpoint.example/model",
            "endpoint.example",
            true,
            true,
        ),
        (
            "https://endpoint.example/model",
            "endpoint.example",
            false,
            false,
        ),
        (
            "http://endpoint.example/model",
            "endpoint.example",
            true,
            false,
        ),
        (
            "https://huggingface.co.attacker.example/model",
            "huggingface.co",
            false,
            false,
        ),
    ] {
        let authorized_host = if opt_in {
            endpoint_host
        } else {
            "huggingface.co"
        };
        let url = std::ffi::CString::new(url).unwrap();
        let host = std::ffi::CString::new(authorized_host).unwrap();
        // SAFETY: both NUL-terminated strings remain valid for the call.
        assert_eq!(
            unsafe { ze_model_token_allowed(url.as_ptr(), host.as_ptr()) } != 0,
            expected
        );
    }
}

#[test]
fn http_endpoint_requires_explicit_allow_http() {
    if std::env::var_os("ZE501_HTTP_POLICY_CHILD").is_none() {
        assert!(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "http_endpoint_requires_explicit_allow_http"])
                .env("ZE501_HTTP_POLICY_CHILD", "1")
                .env_remove("ZE_MODEL_ALLOW_HTTP")
                .env_remove("ZE_MODEL_OFFLINE")
                .env("HF_TOKEN", "fixture-secret")
                .status()
                .unwrap()
                .success()
        );
        return;
    }
    let cache = tempfile::tempdir().unwrap();
    let model = model(&manifest("tiny.zem", SHA_ABC, 3));
    let no_request = |_, _| panic!("HTTP must be rejected before download status or request");
    let options = ModelOpenOptions {
        cache_dir: Some(cache.path().into()),
        endpoint: Some("http://127.0.0.1:1".into()),
        progress: Some(&no_request),
        ..Default::default()
    };
    assert!(matches!(
        resolve_model(&model, &options),
        Err(ModelError::Invalid(_))
    ));
}

#[test]
fn http_cross_host_redirect_never_sends_token_and_still_downloads() {
    if std::env::var_os("ZE501_REDIRECT_CHILD").is_none() {
        assert!(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "http_cross_host_redirect_never_sends_token_and_still_downloads"
                ])
                .env("ZE501_REDIRECT_CHILD", "1")
                .env("HF_TOKEN", "fixture-secret")
                .env("ZE_MODEL_ALLOW_HTTP", "1")
                .env_remove("ZE_MODEL_OFFLINE")
                .status()
                .unwrap()
                .success()
        );
        return;
    }
    let bytes = manifest("tiny.zem", SHA_ABC, 3);
    let model = model(&bytes);
    let target = Fixture::new(bytes.clone(), b"abc".to_vec(), 3, 200);
    let redirect = format!(
        "{}/{}/resolve/{}/zeppelin-model.json",
        target.endpoint.replace("127.0.0.1", "localhost"),
        model.repo,
        model.revision
    );
    let source = Fixture::with_redirect(bytes, b"abc".to_vec(), 3, 200, Some(redirect));
    let cache = tempfile::tempdir().unwrap();
    let options = ModelOpenOptions {
        cache_dir: Some(cache.path().into()),
        endpoint: Some(source.endpoint.clone()),
        send_token_to_endpoint: true,
        ..Default::default()
    };
    let bundle = resolve_model(&model, &options).unwrap();
    assert_eq!(std::fs::read(bundle).unwrap(), b"abc");
    assert_eq!(source.requests.load(Ordering::SeqCst), 2);
    assert_eq!(target.requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        source.authorizations.load(Ordering::SeqCst),
        0,
        "no token on initial HTTP hop"
    );
    assert_eq!(
        target.authorizations.load(Ordering::SeqCst),
        0,
        "no token after redirect"
    );
}

#[test]
fn marker_is_published_and_stat_changes_force_verification() {
    let bytes = manifest("tiny.zem", SHA_ABC, 3);
    let model = model(&bytes);
    let cache = tempfile::tempdir().unwrap();
    cached(cache.path(), &model, &bytes);
    let options = ModelOpenOptions {
        cache_dir: Some(cache.path().into()),
        offline: true,
        ..Default::default()
    };
    let bundle = resolve_model(&model, &options).unwrap();
    let marker = bundle.parent().unwrap().join(".zeppelin-verified");
    assert!(marker.is_file(), "publish must write a verified marker");
    for changed in [b"xyz".as_slice(), b"ab".as_slice()] {
        std::fs::write(&bundle, changed).unwrap();
        let error = resolve_model(&model, &options).unwrap_err();
        assert!(matches!(error, ModelError::Verification(_)));
        assert!(
            error
                .to_string()
                .contains(bundle.parent().unwrap().to_str().unwrap())
        );
        std::fs::write(&bundle, b"abc").unwrap();
        resolve_model(&model, &options).unwrap();
    }
    // A missing/invalid marker requires full verification, not quarantine.
    std::fs::write(&marker, b"invalid marker").unwrap();
    resolve_model(&model, &options).unwrap();
    assert_ne!(std::fs::read(&marker).unwrap(), b"invalid marker");
}

#[test]
fn corrupt_and_partial_entries_are_repaired_online_and_report_path_offline() {
    let bytes = manifest("tiny.zem", SHA_ABC, 3);
    let model = model(&bytes);
    let server = Fixture::new(bytes.clone(), b"abc".to_vec(), 3, 200);
    for missing in [false, true] {
        let cache = tempfile::tempdir().unwrap();
        let entry = cached(cache.path(), &model, &bytes);
        if missing {
            std::fs::remove_file(entry.join("zeppelin-model.json")).unwrap();
        } else {
            std::fs::write(entry.join("tiny.zem"), b"xyz").unwrap();
        }
        let mut options = ModelOpenOptions {
            cache_dir: Some(cache.path().into()),
            endpoint: Some(server.endpoint.clone()),
            allow_http: true,
            offline: true,
            ..Default::default()
        };
        let error = resolve_model(&model, &options).unwrap_err();
        assert!(matches!(error, ModelError::Verification(_)), "{error}");
        assert!(error.to_string().contains(entry.to_str().unwrap()));
        options.offline = false;
        let callback = |_, _| {
            assert!(
                !entry.exists(),
                "old entry must be quarantined before download"
            );
            assert!(
                std::fs::read_dir(entry.parent().unwrap())
                    .unwrap()
                    .any(|e| e
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .contains(".corrupt-"))
            );
        };
        options.progress = Some(&callback);
        let bundle = resolve_model(&model, &options).unwrap();
        assert_eq!(std::fs::read(bundle).unwrap(), b"abc");
        assert!(
            !std::fs::read_dir(entry.parent().unwrap())
                .unwrap()
                .any(|e| e
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".corrupt-"))
        );
    }
}

#[test]
fn byte_progress_is_monotonic_throttled_and_finishes_at_total_on_caller_thread() {
    let body = vec![b'x'; 512 * 1024];
    let total = body.len() as u64;
    let bytes = manifest("tiny.zem", &sha(&body), total);
    let model = model(&bytes);
    let server = Fixture::new(bytes, body, total as usize, 200);
    let cache = tempfile::tempdir().unwrap();
    let reports = std::cell::RefCell::new(Vec::new());
    let caller = std::thread::current().id();
    let callback = |done, size| {
        assert_eq!(std::thread::current().id(), caller);
        reports
            .borrow_mut()
            .push((done, size, std::time::Instant::now()));
    };
    resolve_model(
        &model,
        &ModelOpenOptions {
            cache_dir: Some(cache.path().into()),
            endpoint: Some(server.endpoint.clone()),
            allow_http: true,
            progress: Some(&callback),
            ..Default::default()
        },
    )
    .unwrap();
    let reports = reports.borrow();
    assert_eq!(reports.last().map(|r| (r.0, r.1)), Some((total, total)));
    assert!(
        reports.iter().any(|r| r.0 > 0 && r.0 < total),
        "must report within a file"
    );
    for pair in reports.windows(2) {
        assert!(pair[0].0 <= pair[1].0);
    }
    let intermediate: Vec<_> = reports.iter().filter(|r| r.0 > 0 && r.0 < total).collect();
    for pair in intermediate.windows(2) {
        assert!(pair[1].0 - pair[0].0 >= 64 * 1024);
        assert!(pair[1].2.duration_since(pair[0].2) >= std::time::Duration::from_millis(100));
    }
}

#[test]
#[ignore = "manual 700 MiB cache-open measurement"]
fn measure_large_cache_open() {
    let cache = tempfile::tempdir().unwrap();
    let body = vec![0u8; 700 * 1024 * 1024];
    let bytes = manifest("tiny.zem", &sha(&body), body.len() as u64);
    let model = model(&bytes);
    let entry = cached(cache.path(), &model, &bytes);
    std::fs::write(entry.join("tiny.zem"), &body).unwrap();
    drop(body);
    let options = ModelOpenOptions {
        cache_dir: Some(cache.path().into()),
        offline: true,
        ..Default::default()
    };
    for pass in 0..3 {
        let _ = std::fs::remove_file(entry.join(".zeppelin-verified"));
        let start = std::time::Instant::now();
        resolve_model(&model, &options).unwrap();
        let full = start.elapsed();
        let start = std::time::Instant::now();
        resolve_model(&model, &options).unwrap();
        println!(
            "pass={pass} bytes={} full_rehash_us={} marker_open_us={}",
            700 * 1024 * 1024u64,
            full.as_micros(),
            start.elapsed().as_micros()
        );
    }
}

#[test]
fn partial_cache_offline_is_corrupt_with_entry_path() {
    let bytes = manifest("tiny.zem", SHA_ABC, 3);
    let model = model(&bytes);
    let cache = tempfile::tempdir().unwrap();
    let entry = cached(cache.path(), &model, &bytes);
    std::fs::remove_file(entry.join("zeppelin-model.json")).unwrap();
    let error = resolve_model(
        &model,
        &ModelOpenOptions {
            cache_dir: Some(cache.path().into()),
            offline: true,
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(matches!(error, ModelError::Verification(_)), "{error}");
    assert!(error.to_string().contains(entry.to_str().unwrap()));
}

#[test]
fn failed_repair_preserves_quarantine_and_does_not_publish_partial_bytes() {
    let bytes = manifest("tiny.zem", SHA_ABC, 3);
    let model = model(&bytes);
    let cache = tempfile::tempdir().unwrap();
    let entry = cached(cache.path(), &model, &bytes);
    std::fs::write(entry.join("tiny.zem"), b"xyz").unwrap();
    let callback = |_, _| {
        assert!(!entry.exists());
        let quarantined = std::fs::read_dir(entry.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.to_string_lossy().contains(".corrupt-"))
            .unwrap();
        assert_eq!(std::fs::read(quarantined.join("tiny.zem")).unwrap(), b"xyz");
    };
    let error = resolve_model(
        &model,
        &ModelOpenOptions {
            cache_dir: Some(cache.path().into()),
            endpoint: Some("http://127.0.0.1:1".into()),
            allow_http: true,
            progress: Some(&callback),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(matches!(error, ModelError::Download(_)), "{error}");
    assert!(!entry.exists());
    assert!(!entry.with_extension("partial").exists());

    let parent = entry.parent().unwrap();
    // Cleanup covers old files as well as the directory from the failed repair,
    // and must leave other revisions alone.
    std::fs::write(
        parent.join(format!("{}.corrupt-old-file", model.revision)),
        b"old",
    )
    .unwrap();
    let other_revision = parent.join("another-revision.corrupt-123");
    std::fs::create_dir(&other_revision).unwrap();
    let server = Fixture::new(bytes, b"abc".to_vec(), 3, 200);
    let bundle = resolve_model(
        &model,
        &ModelOpenOptions {
            cache_dir: Some(cache.path().into()),
            endpoint: Some(server.endpoint.clone()),
            allow_http: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(std::fs::read(bundle).unwrap(), b"abc");
    let prefix = format!("{}.corrupt-", model.revision);
    assert!(!std::fs::read_dir(parent).unwrap().any(|item| {
        item.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(&prefix)
    }));
    assert!(other_revision.is_dir());
}

#[test]
fn stale_marker_temp_is_replaced_when_opening_valid_cache_offline() {
    let bytes = manifest("tiny.zem", SHA_ABC, 3);
    let model = model(&bytes);
    let cache = tempfile::tempdir().unwrap();
    let entry = cached(cache.path(), &model, &bytes);
    let temporary = entry.join(".zeppelin-verified.tmp");
    std::fs::write(&temporary, b"interrupted marker write").unwrap();
    let bundle = resolve_model(
        &model,
        &ModelOpenOptions {
            cache_dir: Some(cache.path().into()),
            offline: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(bundle, entry.join("tiny.zem"));
    let marker = std::fs::read_to_string(entry.join(".zeppelin-verified")).unwrap();
    assert!(marker.starts_with(&format!("ze-verified-2\n{}\n", model.manifest_sha256)));
    assert!(!temporary.exists());
}

#[test]
fn read_only_cache_opens_without_marker_but_still_rejects_corruption() {
    use std::os::unix::fs::PermissionsExt;

    let bytes = manifest("tiny.zem", SHA_ABC, 3);
    let model = model(&bytes);
    let cache = tempfile::tempdir().unwrap();
    let entry = cached(cache.path(), &model, &bytes);
    let options = ModelOpenOptions {
        cache_dir: Some(cache.path().into()),
        offline: true,
        ..Default::default()
    };
    std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(0o555)).unwrap();
    let first = resolve_model(&model, &options);
    let second = resolve_model(&model, &options);
    let marker_exists = entry.join(".zeppelin-verified").exists();
    // The existing file remains writable so corruption can be checked under 0555.
    std::fs::write(entry.join("tiny.zem"), b"xyz").unwrap();
    let corrupt = resolve_model(&model, &options);
    std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(first.unwrap(), entry.join("tiny.zem"));
    assert_eq!(second.unwrap(), entry.join("tiny.zem"));
    assert!(!marker_exists);
    assert!(matches!(corrupt, Err(ModelError::Verification(_))));
}
