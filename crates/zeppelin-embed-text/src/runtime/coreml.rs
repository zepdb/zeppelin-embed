//! CoreML query-tower runtime.
//!
//! At batch one the query tower is bound by Metal dispatch count, not by
//! arithmetic: a six-layer encoder issues roughly a hundred kernel
//! launches whose fixed overhead dominates the work inside them. CoreML
//! compiles the whole graph into one unit, so the Apple Neural Engine
//! evaluates it about ten times faster than the MLX GPU path and with a
//! far tighter tail.
//!
//! This runtime owns evaluation only. Tokenization, pooling policy and
//! normalisation stay with the bundle, exactly as they do for MLX, so
//! swapping the backend cannot change what a vector means.

use std::ffi::{CString, c_char};
use std::path::Path;

use super::{EmbeddingBatch, ModelRuntime, RuntimeError, RuntimeIdentity};
use crate::ObservedComputeUnits;
use crate::tower::TokenBatch;

/// Which processors CoreML may schedule the model on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComputeUnits {
    /// CPU only.
    Cpu,
    /// CPU and GPU.
    CpuAndGpu,
    /// CPU and Apple Neural Engine.
    CpuAndNeuralEngine,
    /// Every available processor.
    All,
}

impl ComputeUnits {
    const fn code(self) -> i32 {
        match self {
            Self::Cpu => 0,
            Self::CpuAndGpu => 1,
            Self::CpuAndNeuralEngine => 2,
            Self::All => 3,
        }
    }

    const fn identity(self) -> &'static str {
        match self {
            Self::Cpu => "coreml-cpu",
            Self::CpuAndGpu => "coreml-cpu-gpu",
            Self::CpuAndNeuralEngine => "coreml-cpu-ane",
            Self::All => "coreml-all",
        }
    }
}

#[repr(C)]
struct Handle {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn ze_coreml_open(
        path: *const c_char,
        compute_units: i32,
        observed: *mut ObservedComputeUnits,
        has_observed: *mut i32,
        error: *mut *mut c_char,
    ) -> *mut Handle;
    fn ze_coreml_predict(
        handle: *mut Handle,
        ids: *const i32,
        mask: *const i32,
        sequence: usize,
        out: *mut f32,
        out_len: usize,
        error: *mut *mut c_char,
    ) -> i32;
    fn ze_coreml_close(handle: *mut Handle);
    fn ze_coreml_sequence(handle: *mut Handle) -> usize;
    fn ze_coreml_string_free(text: *mut c_char);
}

/// Takes ownership of a shim-allocated message and renders it.
fn take_error(raw: *mut c_char, fallback: &str) -> String {
    if raw.is_null() {
        return fallback.to_owned();
    }
    // SAFETY: the shim returns a NUL-terminated buffer it allocated with
    // malloc, and hands ownership to exactly one caller.
    let message = unsafe { std::ffi::CStr::from_ptr(raw) }
        .to_string_lossy()
        .into_owned();
    unsafe { ze_coreml_string_free(raw) };
    message
}

pub(crate) fn pad_query(tokens: &TokenBatch, sequence: usize) -> Result<TokenBatch, RuntimeError> {
    if tokens.tokens_per_row == 0 || tokens.tokens_per_row > sequence {
        return Err(RuntimeError::Shape(format!(
            "CoreML query has {} tokens, expected 1..={sequence}",
            tokens.tokens_per_row
        )));
    }
    tokens.padded_to(sequence)
}

/// A loaded CoreML query tower with one model-declared fixed sequence length.
pub struct CoreMlRuntime {
    handle: *mut Handle,
    sequence: usize,
    dims: usize,
    compute_units: ComputeUnits,
    observed_compute_units: Option<ObservedComputeUnits>,
}

// SAFETY: the handle is owned exclusively by this value, every call goes
// through `&mut self`, and the shim performs no shared mutation of its
// own. `MLModel` is documented as safe to call from any one thread at a
// time, which `&mut self` guarantees.
unsafe impl Send for CoreMlRuntime {}

impl CoreMlRuntime {
    /// Loads a compiled CoreML model and reads its fixed input length.
    ///
    /// Inputs must declare matching fixed [1, sequence] Int32
    /// shapes. `dims` is the expected embedding width.
    pub fn load(
        model: &Path,
        dims: usize,
        compute_units: ComputeUnits,
    ) -> Result<Self, RuntimeError> {
        if dims == 0 {
            return Err(RuntimeError::Shape(
                "CoreML dims must be non-zero".to_owned(),
            ));
        }
        let path = model
            .to_str()
            .ok_or_else(|| RuntimeError::Mlx("CoreML model path is not valid UTF-8".to_owned()))?;
        let path = CString::new(path)
            .map_err(|_| RuntimeError::Mlx("CoreML model path contains NUL".to_owned()))?;
        let mut error: *mut c_char = std::ptr::null_mut();
        let mut observed = ObservedComputeUnits::default();
        let mut has_observed = 0;
        // SAFETY: `path` is NUL-terminated and outlives the call; the shim
        // writes the valid output pointers and writes `error` only on failure.
        let handle = unsafe {
            ze_coreml_open(
                path.as_ptr(),
                compute_units.code(),
                &mut observed,
                &mut has_observed,
                &mut error,
            )
        };
        if handle.is_null() {
            return Err(RuntimeError::Mlx(take_error(
                error,
                "CoreML model failed to load",
            )));
        }
        // SAFETY: the handle is live; the accessor reads its validated input width.
        let sequence = unsafe { ze_coreml_sequence(handle) };
        if sequence == 0 {
            unsafe { ze_coreml_close(handle) };
            return Err(RuntimeError::Shape(
                "CoreML sequence must be non-zero".to_owned(),
            ));
        }
        Ok(Self {
            handle,
            sequence,
            dims,
            compute_units,
            observed_compute_units: (has_observed != 0).then_some(observed),
        })
    }

    /// Returns the model's fixed padded sequence length.
    #[must_use]
    pub const fn sequence(&self) -> usize {
        self.sequence
    }

    /// Returns preferred-device op counts from the load-time compute plan.
    ///
    /// macOS versions before 14.4 report `None` (unknown).
    #[must_use]
    pub const fn observed_compute_units(&self) -> Option<ObservedComputeUnits> {
        self.observed_compute_units
    }

    fn embed_row(
        &mut self,
        ids: &[i32],
        mask: &[i32],
        out: &mut [f32],
    ) -> Result<(), RuntimeError> {
        let mut error: *mut c_char = std::ptr::null_mut();
        // SAFETY: all three slices are exactly the lengths the shim reads
        // and writes, and `self.handle` is non-null for this value's life.
        let code = unsafe {
            ze_coreml_predict(
                self.handle,
                ids.as_ptr(),
                mask.as_ptr(),
                ids.len(),
                out.as_mut_ptr(),
                out.len(),
                &mut error,
            )
        };
        if code == 0 {
            return Ok(());
        }
        Err(RuntimeError::Mlx(take_error(
            error,
            "CoreML prediction failed",
        )))
    }
}

impl Drop for CoreMlRuntime {
    fn drop(&mut self) {
        // SAFETY: the handle was produced by `ze_coreml_open` and is freed
        // exactly once, here.
        unsafe { ze_coreml_close(self.handle) };
    }
}

impl ModelRuntime for CoreMlRuntime {
    fn embed_batch(&mut self, tokens: &TokenBatch) -> Result<EmbeddingBatch, RuntimeError> {
        let sequence = self.sequence;
        let rows = tokens.rows;
        let mut values = vec![
            0.0_f32;
            rows.checked_mul(self.dims).ok_or_else(|| {
                RuntimeError::Shape("CoreML batch size overflows".to_owned())
            })?
        ];
        let padded;
        let tokens = if tokens.tokens_per_row == sequence {
            tokens
        } else {
            padded = pad_query(tokens, sequence)?;
            &padded
        };
        let mut mask = vec![0_i32; sequence];
        for row in 0..rows {
            let start = row
                .checked_mul(sequence)
                .ok_or_else(|| RuntimeError::Shape("CoreML row offset overflows".to_owned()))?;
            let end = start
                .checked_add(sequence)
                .ok_or_else(|| RuntimeError::Shape("CoreML row end overflows".to_owned()))?;
            let ids = tokens
                .token_ids
                .get(start..end)
                .ok_or_else(|| RuntimeError::Shape("token id row is out of range".to_owned()))?;
            let source = tokens.attention_mask.get(start..end).ok_or_else(|| {
                RuntimeError::Shape("attention mask row is out of range".to_owned())
            })?;
            for (slot, value) in mask.iter_mut().zip(source) {
                *slot = if *value > 0.0 { 1 } else { 0 };
            }
            let out_start = row
                .checked_mul(self.dims)
                .ok_or_else(|| RuntimeError::Shape("CoreML output offset overflows".to_owned()))?;
            let out_end = out_start
                .checked_add(self.dims)
                .ok_or_else(|| RuntimeError::Shape("CoreML output end overflows".to_owned()))?;
            let mut scratch = vec![0.0_f32; self.dims];
            self.embed_row(ids, &mask, &mut scratch)?;
            let target = values.get_mut(out_start..out_end).ok_or_else(|| {
                RuntimeError::Shape("CoreML output row is out of range".to_owned())
            })?;
            target.copy_from_slice(&scratch);
        }
        EmbeddingBatch::new(values, rows, self.dims)
    }

    fn warm(&mut self) -> Result<(), RuntimeError> {
        let sequence = self.sequence;
        let ids = vec![0_i32; sequence];
        let mask = vec![1_i32; sequence];
        let mut out = vec![0.0_f32; self.dims];
        self.embed_row(&ids, &mask, &mut out)
    }

    fn identity(&self) -> RuntimeIdentity {
        RuntimeIdentity {
            name: self.compute_units.identity(),
            gpu: matches!(
                self.compute_units,
                ComputeUnits::CpuAndGpu | ComputeUnits::All
            ),
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    #[test]
    fn query_rows_pad_to_96_and_reject_empty_or_overlength() {
        for count in [1, 32, 33, 64, 65, 96] {
            let tokens = crate::tower::TokenBatch::new(vec![7; count], vec![1.; count], 1, count)
                .expect("row");
            let padded = super::pad_query(&tokens, 96).expect("pad query");
            assert_eq!(padded.tokens_per_row, 96);
            assert_eq!(
                padded.token_ids.iter().filter(|id| **id == 7).count(),
                count
            );
            assert_eq!(padded.attention_mask.iter().sum::<f32>(), count as f32);
        }
        for count in [0, 97] {
            let tokens = crate::tower::TokenBatch {
                token_ids: vec![7; count],
                attention_mask: vec![1.; count],
                rows: 1,
                tokens_per_row: count,
            };
            assert!(matches!(
                super::pad_query(&tokens, 96),
                Err(super::RuntimeError::Shape(_))
            ));
        }
    }
}
