use std::ffi::OsStr;
use std::sync::OnceLock;

use super::RuntimeError;

/// Default process-global MLX buffer cache ceiling, initially 4 GiB from ZE-503.
/// The first text runtime to set the process-wide limit wins.
pub const DEFAULT_MLX_CACHE_LIMIT_BYTES: usize = 4 * 1024 * 1024 * 1024;

unsafe extern "C" {
    fn mlx_set_cache_limit(old: *mut usize, limit: usize) -> libc::c_int;
    fn mlx_clear_cache() -> libc::c_int;
}

// MLX's allocator is process-global, so later stores must not reset host policy.
static INITIALIZED: OnceLock<Result<(), RuntimeError>> = OnceLock::new();

pub(crate) fn initialize(option: Option<usize>) -> Result<(), RuntimeError> {
    if let Some(result) = INITIALIZED.get() {
        return result.clone();
    }
    let env = std::env::var_os("ZE_MLX_CACHE_LIMIT_BYTES");
    let limit = resolve_limit(option, env.as_deref())?;
    INITIALIZED
        .get_or_init(|| set_limit(limit).map(|_| ()))
        .clone()
}

fn set_limit(limit: usize) -> Result<usize, RuntimeError> {
    let _call = super::mlx::mlx_calls()
        .lock()
        .map_err(|_| RuntimeError::Mlx("MLX call mutex is poisoned".to_owned()))?;
    let mut old = 0;
    // SAFETY: MLX is linked through mlx-rs; old is a writable size_t, and
    // the process-wide MLX mutex serializes this call with runtime operations.
    let status = unsafe { mlx_set_cache_limit(&mut old, limit) };
    if status != 0 {
        return Err(RuntimeError::MlxCache {
            operation: "mlx_set_cache_limit",
            status,
        });
    }
    Ok(old)
}

pub(crate) fn clear() -> Result<(), RuntimeError> {
    let _call = super::mlx::mlx_calls()
        .lock()
        .map_err(|_| RuntimeError::Mlx("MLX call mutex is poisoned".to_owned()))?;
    // SAFETY: this argument-free MLX call is serialized with runtime operations.
    let status = unsafe { mlx_clear_cache() };
    if status != 0 {
        return Err(RuntimeError::MlxCache {
            operation: "mlx_clear_cache",
            status,
        });
    }
    Ok(())
}

fn resolve_limit(option: Option<usize>, env: Option<&OsStr>) -> Result<usize, RuntimeError> {
    if let Some(limit) = option {
        return Ok(limit);
    }
    match env {
        None => Ok(DEFAULT_MLX_CACHE_LIMIT_BYTES),
        Some(value) => value
            .to_str()
            .and_then(|value| value.parse().ok())
            .ok_or(RuntimeError::InvalidCacheLimit),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::bundle::Bundle;
    use crate::runtime::ModelRuntime;
    use crate::runtime::mlx::MlxRuntime;
    use crate::tower::TowerRole;
    use std::sync::Arc;

    #[test]
    fn cache_limit_resolution_uses_option_then_env_then_default_and_rejects_bad_env() {
        assert_eq!(
            resolve_limit(None, None).expect("default"),
            DEFAULT_MLX_CACHE_LIMIT_BYTES
        );
        assert_eq!(
            resolve_limit(Some(123), Some(OsStr::new("456"))).expect("option"),
            123
        );
        assert_eq!(
            resolve_limit(Some(0), Some(OsStr::new("bad"))).expect("explicit zero"),
            0
        );
        assert_eq!(
            resolve_limit(None, Some(OsStr::new("456"))).expect("env"),
            456
        );
        assert_eq!(
            resolve_limit(None, Some(OsStr::new("0"))).expect("zero cache"),
            0
        );
        for bad in ["", "bad", "-1", "1.5", "18446744073709551616"] {
            assert_eq!(
                resolve_limit(None, Some(OsStr::new(bad))),
                Err(RuntimeError::InvalidCacheLimit),
                "accepted {bad:?}"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            assert!(resolve_limit(None, Some(OsStr::from_bytes(&[0xff]))).is_err());
        }
    }

    #[test]
    #[ignore = "requires ZE_TEXT_BUNDLE and native Metal; run this test alone"]
    fn real_bundle_cache_limit_preserves_bit_identical_vectors_for_100_chunks() {
        let path = std::env::var_os("ZE_TEXT_BUNDLE").expect("set ZE_TEXT_BUNDLE");
        let bundle = Arc::new(Bundle::open(path).expect("real bundle"));
        let samples = [
            "A diversified stock portfolio spreads investment risk across companies and sectors.",
            "Paying down high interest credit card debt can reduce monthly household expenses.",
            "A retirement account offers tax advantages but early withdrawals may incur penalties.",
            "Mortgage payments include principal and interest; property taxes and insurance add costs.",
            "Bond prices generally fall when interest rates rise, and duration measures sensitivity.",
        ];
        let chunks = samples
            .iter()
            .cycle()
            .take(100)
            .enumerate()
            .map(|(n, sample)| format!("Financial example {n}. {}", sample.repeat(1 + n % 4)))
            .collect::<Vec<_>>();
        let tokens = bundle
            .tokenizer(TowerRole::Document)
            .encode_batch(
                &chunks,
                bundle.document_tower().embedding.max_tokens as usize,
            )
            .expect("100 tokenized chunks");
        let dims = bundle.document_tower().embedding.dims as usize;
        let mut runtime = MlxRuntime::load(bundle, TowerRole::Document).expect("document tower");
        let old = set_limit(usize::MAX).expect("uncapped cache");
        clear().expect("cold uncapped cache");
        let uncapped = runtime.embed_batch(&tokens).expect("uncapped embeddings");
        clear().expect("release uncapped buffers");
        set_limit(0).expect("no buffer cache");
        let capped = runtime.embed_batch(&tokens).expect("capped embeddings");
        clear().expect("release capped buffers");
        set_limit(old).expect("restore process cache policy");
        assert_eq!((uncapped.rows(), uncapped.dims()), (100, dims));
        assert_eq!((capped.rows(), capped.dims()), (100, dims));
        assert_eq!(
            uncapped
                .values()
                .iter()
                .map(|value| value.to_bits())
                .collect::<Vec<_>>(),
            capped
                .values()
                .iter()
                .map(|value| value.to_bits())
                .collect::<Vec<_>>(),
        );
    }
}
