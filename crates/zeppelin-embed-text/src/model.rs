//! Pinned Hugging Face model resolution and verified, process-safe caching.

use std::path::PathBuf;

/// Immutable model identity. No moving branch names or tags are accepted.
#[derive(Clone, Debug)]
pub struct ModelRef {
    /// Hugging Face `owner/repository`.
    pub repo: String,
    /// Full immutable 40-hex commit SHA.
    pub revision: String,
    /// SHA-256 of the exact `zeppelin-model.json` bytes, as 64 hex characters.
    pub manifest_sha256: String,
}

/// Download/cache policy plus the existing text-store open policy.
#[derive(Default)]
pub struct ModelOpenOptions<'a> {
    /// Existing explicit-path store options.
    pub text: crate::TextOpenOptions,
    /// Cache root; wins over `ZE_MODEL_CACHE` and the macOS default.
    pub cache_dir: Option<PathBuf>,
    /// Never use the network. `ZE_MODEL_OFFLINE=1` also enables this.
    pub offline: bool,
    /// Endpoint override; otherwise `ZE_MODEL_ENDPOINT` or HF HTTPS.
    /// Plain HTTP additionally requires `allow_http` or `ZE_MODEL_ALLOW_HTTP=1`.
    pub endpoint: Option<String>,
    /// Permit plain HTTP for local test fixtures. `ZE_MODEL_ALLOW_HTTP=1` also
    /// enables this. Tokens are never sent over HTTP.
    pub allow_http: bool,
    /// Authorize HF_TOKEN on HTTPS requests to the endpoint's exact host.
    /// Default false: only HTTPS huggingface.co requests may receive the token.
    /// This opt-in never authorizes HTTP or other redirect hosts.
    pub send_token_to_endpoint: bool,
    /// Synchronous status at file boundaries: verified bytes done / total.
    /// Called with zero bytes before downloading the manifest (total unknown: zero).
    pub progress: Option<&'a dyn Fn(u64, u64)>,
}

/// Typed model resolution failure; invalid bytes are never opened as a model.
#[derive(Debug)]
pub enum ModelError {
    /// No complete entry was found while offline.
    OfflineMiss,
    /// A reference, option, or manifest violates the model contract.
    Invalid(String),
    /// A file's size or SHA-256 does not match its pin.
    Verification(String),
    /// HTTP or transport failure.
    Download(String),
    /// Cache filesystem operation failed.
    Io(std::io::Error),
}

impl std::fmt::Display for ModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OfflineMiss => f.write_str("model is not cached (offline)"),
            Self::Invalid(s) => write!(f, "invalid model: {s}"),
            Self::Verification(s) => write!(f, "model verification failed: {s}"),
            Self::Download(s) => write!(f, "model download failed: {s}"),
            Self::Io(e) => e.fmt(f),
        }
    }
}
impl std::error::Error for ModelError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}
impl From<std::io::Error> for ModelError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

use crate::model_manifest::Manifest;
use std::ffi::{CStr, CString, c_char};
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};
use std::path::Path;

const MANIFEST: &str = "zeppelin-model.json";
const MAX_MANIFEST: u64 = 1024 * 1024;

unsafe extern "C" {
    fn ze_model_download(
        url: *const c_char,
        path: *const c_char,
        token: *const c_char,
        authorized_endpoint: *const c_char,
        allow_http: i32,
        limit: u64,
        digest: *mut u8,
        bytes: *mut u64,
        error: *mut c_char,
        error_cap: usize,
    ) -> i32;
    fn ze_model_file_sha256(path: *const c_char, digest: *mut u8, bytes: *mut u64) -> i32;
}

pub(crate) fn valid_sha(text: &str, length: usize) -> bool {
    text.len() == length && text.bytes().all(|b| b.is_ascii_hexdigit())
}
pub(crate) fn valid_path(text: &str) -> bool {
    text.split('/').all(|part| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    })
}
fn invalid(detail: &str) -> ModelError {
    ModelError::Invalid(detail.into())
}
fn env_string(name: &str) -> Result<Option<String>, ModelError> {
    std::env::var_os(name)
        .map(|value| {
            value
                .into_string()
                .map_err(|_| invalid("environment override is not UTF-8"))
        })
        .transpose()
}
fn cstring(bytes: &[u8]) -> Result<CString, ModelError> {
    CString::new(bytes).map_err(|_| invalid("NUL in path, endpoint, or token"))
}
fn digest_hex(digest: &[u8]) -> String {
    use std::fmt::Write;
    let mut hex = String::with_capacity(64);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}
fn hash_file(path: &Path) -> Result<(String, u64), ModelError> {
    if !fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(ModelError::Verification(format!(
            "not a regular file: {}",
            path.display()
        )));
    }
    let cpath = cstring(path.as_os_str().as_bytes())?;
    let mut digest = [0; 32];
    let mut bytes = 0;
    // SAFETY: the path and the two fixed-size output buffers outlive this call.
    if unsafe { ze_model_file_sha256(cpath.as_ptr(), digest.as_mut_ptr(), &mut bytes) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok((digest_hex(&digest), bytes))
}
fn check_hash(actual: &str, expected: &str, path: &Path) -> Result<(), ModelError> {
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(ModelError::Verification(format!(
            "SHA-256 mismatch: {}",
            path.display()
        )));
    }
    Ok(())
}
fn load_manifest(entry: &Path, model: &ModelRef) -> Result<Manifest, ModelError> {
    let path = entry.join(MANIFEST);
    if fs::symlink_metadata(&path)?.len() > MAX_MANIFEST {
        return Err(invalid("manifest exceeds 1 MiB"));
    }
    let (hash, _) = hash_file(&path)?;
    check_hash(&hash, &model.manifest_sha256, &path)?;
    Manifest::parse(&fs::read(path)?)
}
fn verify_entry(entry: &Path, model: &ModelRef) -> Result<PathBuf, ModelError> {
    if !fs::symlink_metadata(entry)?.file_type().is_dir() {
        return Err(ModelError::Verification(
            "cache entry is not a directory".into(),
        ));
    }
    let manifest = load_manifest(entry, model)?;
    // Reject unlisted files, including symlinks or an unpinned CoreML tower.
    let mut remaining: std::collections::BTreeSet<PathBuf> = manifest
        .files
        .iter()
        .map(|f| PathBuf::from(&f.path))
        .collect();
    remaining.insert(MANIFEST.into());
    verify_tree(entry, entry, &mut remaining)?;
    if !remaining.is_empty() {
        return Err(ModelError::Verification("incomplete cache entry".into()));
    }
    for file in &manifest.files {
        let path = entry.join(&file.path);
        let (hash, size) = hash_file(&path)?;
        if size != file.size {
            return Err(ModelError::Verification(format!(
                "size mismatch: {}",
                file.path
            )));
        }
        check_hash(&hash, &file.sha256, &path)?;
    }
    Ok(entry.join(manifest.bundle))
}
fn verify_tree(
    root: &Path,
    dir: &Path,
    remaining: &mut std::collections::BTreeSet<PathBuf>,
) -> Result<(), ModelError> {
    for item in fs::read_dir(dir)? {
        let item = item?;
        let path = item.path();
        let kind = item.file_type()?;
        if kind.is_dir() {
            // Paths in the manifest are bounded by MAX_MANIFEST; real directory depth
            // must also stay within the listed tree (no recursion into extra trees).
            let relative = path.strip_prefix(root).map_err(|_| invalid("cache path"))?;
            if !remaining.iter().any(|p| p.starts_with(relative)) {
                return Err(ModelError::Verification("unlisted cache directory".into()));
            }
            verify_tree(root, &path, remaining)?;
        } else if !kind.is_file()
            || !remaining.remove(path.strip_prefix(root).map_err(|_| invalid("cache path"))?)
        {
            return Err(ModelError::Verification(
                "unlisted file or symlink in cache".into(),
            ));
        }
    }
    Ok(())
}
fn download(
    url: &str,
    path: &Path,
    limit: u64,
    allow_http: bool,
    token: Option<&CString>,
    authorized_endpoint: &CString,
) -> Result<(String, u64), ModelError> {
    let url = cstring(url.as_bytes())?;
    let path = cstring(path.as_os_str().as_bytes())?;
    let mut digest = [0; 32];
    let mut bytes = 0;
    let mut error = [0 as c_char; 1024];
    // SAFETY: all input strings and output buffers remain valid throughout the
    // synchronous call; the shim retains no Rust pointers after returning.
    let result = unsafe {
        ze_model_download(
            url.as_ptr(),
            path.as_ptr(),
            token.map_or(std::ptr::null(), |s| s.as_ptr()),
            authorized_endpoint.as_ptr(),
            i32::from(allow_http),
            limit,
            digest.as_mut_ptr(),
            &mut bytes,
            error.as_mut_ptr(),
            error.len(),
        )
    };
    if result != 0 {
        // The shim always NUL terminates this nonempty error buffer.
        let detail = unsafe { CStr::from_ptr(error.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        return Err(if result == -2 {
            ModelError::Verification(detail)
        } else {
            ModelError::Download(detail)
        });
    }
    Ok((digest_hex(&digest), bytes))
}

// Closing the descriptor releases flock, including on any error path.
struct CacheLock {
    _file: File,
}
impl CacheLock {
    fn acquire(path: &Path) -> Result<Self, ModelError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        loop {
            // SAFETY: file owns a live descriptor for the entire locked scope.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(Self { _file: file });
            }
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return Err(e.into());
            }
        }
    }
}
struct Staging(PathBuf);
impl Drop for Staging {
    fn drop(&mut self) {
        // Failure to clean up cannot publish partial bytes. The next lock holder
        // removes this same staging path before starting a fresh download.
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Resolves to a verified bundle without starting MLX or opening a store.
/// A corrupt existing entry fails validation; it is never silently repaired.
/// Repo components use `%2F` as the cache separator (unambiguous and path-safe).
pub fn resolve_model(
    model: &ModelRef,
    options: &ModelOpenOptions<'_>,
) -> Result<PathBuf, ModelError> {
    if !valid_path(&model.repo)
        || model.repo.split('/').count() != 2
        || !valid_sha(&model.revision, 40)
        || !valid_sha(&model.manifest_sha256, 64)
    {
        return Err(invalid(
            "expected owner/repo, immutable commit SHA and manifest SHA-256",
        ));
    }
    let root = options
        .cache_dir
        .clone()
        .or_else(|| std::env::var_os("ZE_MODEL_CACHE").map(PathBuf::from))
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|p| PathBuf::from(p).join("Library/Caches/zeppelin-embed/models"))
        })
        .ok_or_else(|| invalid("HOME and model cache directory are unset"))?;
    if root.as_os_str().is_empty() {
        return Err(invalid("empty cache root"));
    }
    let parent = root.join(model.repo.replace('/', "%2F"));
    let revision = model.revision.to_ascii_lowercase();
    let entry = parent.join(&revision);
    let offline = options.offline || std::env::var_os("ZE_MODEL_OFFLINE").is_some_and(|v| v == "1");
    if offline && !entry.try_exists()? {
        return Err(ModelError::OfflineMiss);
    }
    fs::create_dir_all(&parent)?;
    let _lock = CacheLock::acquire(&parent.join(format!("{revision}.lock")))?;
    match fs::symlink_metadata(&entry) {
        Ok(_) => return verify_entry(&entry, model),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    if offline {
        return Err(ModelError::OfflineMiss);
    }
    let endpoint = match &options.endpoint {
        Some(value) => Some(value.clone()),
        None => env_string("ZE_MODEL_ENDPOINT")?,
    };
    let allow_http =
        options.allow_http || std::env::var_os("ZE_MODEL_ALLOW_HTTP").is_some_and(|v| v == "1");
    let endpoint = endpoint
        .as_deref()
        .unwrap_or("https://huggingface.co")
        .trim_end_matches('/');
    if !(endpoint.starts_with("https://") || (allow_http && endpoint.starts_with("http://")))
        || endpoint.contains(['?', '#'])
    {
        return Err(invalid(
            "endpoint must use HTTPS; HTTP requires allow_http or ZE_MODEL_ALLOW_HTTP=1",
        ));
    }
    // Foundation parses the chosen URL's exact host; Rust chooses which host
    // is authorized without duplicating a URL parser here.
    let authorized_endpoint = cstring(if options.send_token_to_endpoint {
        endpoint.as_bytes()
    } else {
        b"https://huggingface.co"
    })?;
    let token = env_string("HF_TOKEN")?
        .map(|v| cstring(v.as_bytes()))
        .transpose()?;
    let staging_path = parent.join(format!("{revision}.partial"));
    if staging_path.try_exists()? {
        fs::remove_dir_all(&staging_path)?;
    }
    fs::create_dir(&staging_path)?;
    let staging = Staging(staging_path);
    let base = format!("{endpoint}/{}/resolve/{revision}", model.repo);
    if let Some(callback) = options.progress {
        callback(0, 0);
    }
    let (hash, _) = download(
        &format!("{base}/{MANIFEST}"),
        &staging.0.join(MANIFEST),
        MAX_MANIFEST,
        allow_http,
        token.as_ref(),
        &authorized_endpoint,
    )?;
    check_hash(&hash, &model.manifest_sha256, &staging.0.join(MANIFEST))?;
    let manifest = Manifest::parse(&fs::read(staging.0.join(MANIFEST))?)?;
    let total = manifest
        .files
        .iter()
        .try_fold(0u64, |sum, f| sum.checked_add(f.size))
        .ok_or_else(|| invalid("manifest total size overflows u64"))?;
    let mut done = 0;
    if let Some(callback) = options.progress {
        callback(done, total);
    }
    for file in &manifest.files {
        let path = staging.0.join(&file.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let (hash, size) = download(
            &format!("{base}/{}", file.path),
            &path,
            file.size,
            allow_http,
            token.as_ref(),
            &authorized_endpoint,
        )?;
        if size != file.size {
            return Err(ModelError::Verification(format!(
                "size mismatch: {}",
                file.path
            )));
        }
        check_hash(&hash, &file.sha256, &path)?;
        done += size; // total was checked above, and each size equals its manifest size.
        if let Some(callback) = options.progress {
            callback(done, total);
        }
    }
    fs::rename(&staging.0, &entry)?;
    File::open(&parent)?.sync_all()?;
    Ok(entry.join(manifest.bundle))
}
