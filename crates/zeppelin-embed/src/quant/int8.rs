//! Per-vector affine signed-byte quantization.
//!
//! Each row stores one signed byte per coordinate plus an eight-byte
//! `(scale, offset)` tail. A code reconstructs as `scale * code + offset`,
//! with the row minimum and maximum mapped across `[-127, 127]`. The mapping
//! is training-free: every row derives its own two scalars and depends on no
//! corpus calibration or learned state.
//!
//! A future quality option may replace extrema with clipped quantiles. Apache
//! Lucene's [`ScalarQuantizer`](https://lucene.apache.org/core/10_4_0/core/org/apache/lucene/util/quantization/ScalarQuantizer.html)
//! is the reference for confidence-interval quantiles. That policy is not part
//! of this v1 codec because clipping changes the deterministic error contract.

use crate::kernels::{MAX_DOT_I8_DIMENSION, dot_i8};

use super::QuantError;

/// Borrowed signed-byte row and its per-vector affine reconstruction factors.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Int8Vec<'a> {
    /// One signed code per source coordinate.
    pub codes: &'a [i8],
    /// Reconstruction step between adjacent signed codes.
    pub scale: f32,
    /// Reconstructed value at code zero.
    pub offset: f32,
}

/// Query-side signed-byte representation prepared once and reused per row.
///
/// The query uses a symmetric zero-centered map, so a per-row affine candidate
/// can be scored from one native i8 dot product plus the candidate offset times
/// the precomputed query-code sum. No query quantization or expansion occurs
/// inside the row loop.
#[derive(Clone, Debug, PartialEq)]
pub struct Int8Query {
    codes: Vec<i8>,
    scale: f64,
    code_sum: i32,
}

impl Int8Query {
    /// Returns the query coordinate count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.codes.len()
    }

    /// Returns whether the query has zero coordinates.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.codes.is_empty()
    }

    /// Returns the prepared code bytes, scale, and code sum exactly as
    /// consumed by the affine dot-product scorer.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    #[must_use]
    pub fn observation_parts(&self) -> (&[i8], f64, i32) {
        (&self.codes, self.scale, self.code_sum)
    }

    #[cfg(test)]
    pub(crate) fn codes(&self) -> &[i8] {
        &self.codes
    }

    pub(crate) fn score_integer_dot(
        &self,
        integer_dot: i32,
        row_scale: f32,
        row_offset: f32,
    ) -> f32 {
        (self.scale
            * (f64::from(row_scale) * f64::from(integer_dot)
                + f64::from(row_offset) * f64::from(self.code_sum))) as f32
    }
}

/// Quantizes one finite row to signed bytes using a per-vector affine map.
///
/// A non-constant row reconstructs coordinate `i` as
/// `scale * out[i] + offset`, where `scale = (max - min) / 254`. A constant
/// row uses `scale = 0`, zero codes, and the constant as its offset, so the
/// edge case is exact. Positive and negative zero are accepted and reconstruct
/// to numerical zero.
///
/// # Errors
///
/// Returns [`QuantError::EmptyVector`] for an empty input,
/// [`QuantError::DimensionTooLarge`] above the signed-byte kernel limit,
/// [`QuantError::NonFinite`] for the first NaN or infinity, and
/// [`QuantError::OutputLength`] unless `out.len() == v.len()`. Validation
/// completes before `out` is modified.
pub fn quantize_int8(v: &[f32], out: &mut [i8]) -> Result<(f32, f32), QuantError> {
    validate_vector(v)?;
    if out.len() != v.len() {
        return Err(QuantError::OutputLength {
            expected: v.len(),
            actual: out.len(),
        });
    }

    let (minimum, maximum) = extrema(v);
    if minimum == maximum {
        out.fill(0);
        return Ok((0.0, minimum));
    }

    let range = f64::from(maximum) - f64::from(minimum);
    let mut scale = (range / 254.0) as f32;
    if scale == 0.0 {
        scale = f32::from_bits(1);
    }
    let offset = ((f64::from(maximum) + f64::from(minimum)) * 0.5) as f32;
    let scale_f64 = f64::from(scale);
    let offset_f64 = f64::from(offset);
    for (&value, code) in v.iter().zip(out.iter_mut()) {
        let mapped = ((f64::from(value) - offset_f64) / scale_f64)
            .round()
            .clamp(-127.0, 127.0);
        *code = mapped as i8;
    }
    Ok((scale, offset))
}

/// Reconstructs one affine signed-byte row into caller-owned storage.
///
/// # Errors
///
/// Returns [`QuantError::OutputLength`] when the output and code lengths
/// differ.
pub fn dequantize_int8(encoded: Int8Vec<'_>, out: &mut [f32]) -> Result<(), QuantError> {
    if out.len() != encoded.codes.len() {
        return Err(QuantError::OutputLength {
            expected: encoded.codes.len(),
            actual: out.len(),
        });
    }
    for (&code, value) in encoded.codes.iter().zip(out.iter_mut()) {
        *value = (f64::from(encoded.scale) * f64::from(code) + f64::from(encoded.offset)) as f32;
    }
    Ok(())
}

/// Prepares one full-precision query for repeated affine Int8 row scoring.
///
/// Coordinates are symmetrically rounded into `[-127, 127]`. This work and
/// the signed-code sum happen once per query; [`dot_int8_query`] performs no
/// allocation and no per-row query conversion.
///
/// # Errors
///
/// Returns [`QuantError::EmptyVector`], [`QuantError::DimensionTooLarge`], or
/// [`QuantError::NonFinite`] under the same policy as [`quantize_int8`].
pub fn prepare_int8_query(q: &[f32]) -> Result<Int8Query, QuantError> {
    validate_vector(q)?;
    #[cfg(any(test, feature = "test-support"))]
    super::QUERY_PREPARATIONS.with(|calls| {
        if let Some(calls) = calls.borrow_mut().as_mut() {
            calls.int8.push(q.len());
        }
    });
    let maximum = q.iter().map(|value| value.abs()).fold(0.0_f32, f32::max);
    if maximum == 0.0 {
        return Ok(Int8Query {
            codes: vec![0_i8; q.len()],
            scale: 0.0,
            code_sum: 0,
        });
    }

    let scale = f64::from(maximum) / 127.0;
    let mut code_sum = 0_i32;
    let codes = q
        .iter()
        .map(|&value| {
            let code = (f64::from(value) / scale).round().clamp(-127.0, 127.0) as i8;
            code_sum += i32::from(code);
            code
        })
        .collect();
    Ok(Int8Query {
        codes,
        scale,
        code_sum,
    })
}

/// Estimates a dot product against one affine Int8 row.
///
/// If `q_hat = query_scale * query_codes` and
/// `v_hat = row_scale * row_codes + row_offset`, expansion gives
/// `query_scale * (row_scale * dot(query_codes, row_codes) +
/// row_offset * sum(query_codes))`. The native task-03 i8 dot kernel supplies
/// the only dimension-wide operation.
///
/// # Errors
///
/// Returns [`QuantError::CodeLength`] when the prepared query and stored row
/// dimensions differ.
pub fn dot_int8_query(query: &Int8Query, row: Int8Vec<'_>) -> Result<f32, QuantError> {
    if row.codes.len() != query.codes.len() {
        return Err(QuantError::CodeLength {
            expected: query.codes.len(),
            actual: row.codes.len(),
        });
    }
    let integer_dot = dot_i8(&query.codes, row.codes);
    Ok(query.score_integer_dot(integer_dot, row.scale, row.offset))
}

fn validate_vector(v: &[f32]) -> Result<(), QuantError> {
    if v.is_empty() {
        return Err(QuantError::EmptyVector);
    }
    if v.len() > MAX_DOT_I8_DIMENSION {
        return Err(QuantError::DimensionTooLarge {
            actual: v.len(),
            maximum: MAX_DOT_I8_DIMENSION,
        });
    }
    if let Some((index, _)) = v.iter().enumerate().find(|(_, value)| !value.is_finite()) {
        return Err(QuantError::NonFinite { index });
    }
    Ok(())
}

fn extrema(v: &[f32]) -> (f32, f32) {
    v.iter().copied().fold(
        (f32::INFINITY, f32::NEG_INFINITY),
        |(minimum, maximum), value| (minimum.min(value), maximum.max(value)),
    )
}
