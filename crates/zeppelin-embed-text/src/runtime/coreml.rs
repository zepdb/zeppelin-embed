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

/// A loaded CoreML query tower pinned to one padded sequence length.
pub struct CoreMlRuntime {
    handle: *mut Handle,
    sequence: usize,
    dims: usize,
    compute_units: ComputeUnits,
}

// SAFETY: the handle is owned exclusively by this value, every call goes
// through `&mut self`, and the shim performs no shared mutation of its
// own. `MLModel` is documented as safe to call from any one thread at a
// time, which `&mut self` guarantees.
unsafe impl Send for CoreMlRuntime {}

impl CoreMlRuntime {
    /// Loads a compiled or packaged CoreML model.
    ///
    /// `sequence` is the exact padded token count the model was exported
    /// with, and `dims` its embedding width. Both are contracts of the
    /// export, so a mismatch is rejected rather than reshaped.
    pub fn load(
        model: &Path,
        sequence: usize,
        dims: usize,
        compute_units: ComputeUnits,
    ) -> Result<Self, RuntimeError> {
        if sequence == 0 || dims == 0 {
            return Err(RuntimeError::Shape(
                "CoreML sequence and dims must be non-zero".to_owned(),
            ));
        }
        let path = model
            .to_str()
            .ok_or_else(|| RuntimeError::Mlx("CoreML model path is not valid UTF-8".to_owned()))?;
        let path = CString::new(path)
            .map_err(|_| RuntimeError::Mlx("CoreML model path contains NUL".to_owned()))?;
        let mut error: *mut c_char = std::ptr::null_mut();
        // SAFETY: `path` is NUL-terminated and outlives the call; the shim
        // writes `error` only when it returns null.
        let handle = unsafe { ze_coreml_open(path.as_ptr(), compute_units.code(), &mut error) };
        if handle.is_null() {
            return Err(RuntimeError::Mlx(take_error(
                error,
                "CoreML model failed to load",
            )));
        }
        Ok(Self {
            handle,
            sequence,
            dims,
            compute_units,
        })
    }

    /// Returns the padded sequence length this model was exported with.
    #[must_use]
    pub const fn sequence(&self) -> usize {
        self.sequence
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
        if tokens.tokens_per_row != self.sequence {
            return Err(RuntimeError::Shape(format!(
                "CoreML model expects {} tokens per row, batch carries {}",
                self.sequence, tokens.tokens_per_row
            )));
        }
        let rows = tokens.rows;
        let mut values = vec![
            0.0_f32;
            rows.checked_mul(self.dims).ok_or_else(|| {
                RuntimeError::Shape("CoreML batch size overflows".to_owned())
            })?
        ];
        let mut mask = vec![0_i32; self.sequence];
        for row in 0..rows {
            let start = row
                .checked_mul(self.sequence)
                .ok_or_else(|| RuntimeError::Shape("CoreML row offset overflows".to_owned()))?;
            let end = start
                .checked_add(self.sequence)
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
        let ids = vec![0_i32; self.sequence];
        let mask = vec![1_i32; self.sequence];
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
