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
                let _ = stream.write_all(data);
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
