//! One-bit RaBitQ row encoding with identity rotation.
//!
//! For a finite row `v` with dimension `d`, let `h_i = sign(v_i)` in
//! `{-1, +1}` and pack the positive signs most-significant bit first. The
//! [RaBitQ paper](https://arxiv.org/abs/2405.12497) treats
//! `h_bar = h / sqrt(d)` as a unit code direction and estimates a dot product
//! with query `q` as
//!
//! ```text
//! ||v||^2 * <h_bar, q> / <h_bar, v>
//!   = ||v||^2 * <h, q> / <h, v>
//!   = correction * <h, q>,
//! correction = ||v||^2 / sum_i |v_i|.
//! ```
//!
//! The `sqrt(d)` normalization cancels between numerator and denominator.
//! Under the paper's randomized rotation, the residual error is unbiased and
//! concentrates with dimension. V1 deliberately uses the identity rotation:
//! rotation remains optional and off by default, and the recall harness tests
//! that near-isotropy assumption on every dataset, including anisotropic,
//! clustered, heavy-tailed, and correlated synthetic families.

use crate::kernels::MAX_DOT_I8_DIMENSION;

use super::QuantError;

const COORDINATES_PER_BYTE: usize = 8;

/// Three finite per-row scalars for the one-bit dot estimator.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bit1Factors {
    scale: f32,
    normalized_norm: f32,
    normalized_correction: f32,
}

impl Bit1Factors {
    /// Returns the source row's Euclidean norm.
    #[must_use]
    pub const fn norm(self) -> f64 {
        self.scale as f64 * self.normalized_norm as f64
    }

    /// Returns `||v||^2 / sum_i |v_i|`, the RaBitQ correction.
    #[must_use]
    pub const fn correction(self) -> f64 {
        self.scale as f64 * self.normalized_correction as f64
    }

    /// Returns the conservative norm-scaled sign reconstruction error bound.
    #[must_use]
    pub fn reconstruction_error_bound(self) -> f64 {
        std::f64::consts::SQRT_2 * self.norm()
    }
}

/// Query-side signed-byte representation prepared once and reused per row.
///
/// Seeded stochastic rounding is unbiased for every reconstructed query
/// coordinate. Row scoring reads these codes directly while extracting signs
/// from packed bytes; it allocates no expanded row.
#[derive(Clone, Debug, PartialEq)]
pub struct Bit1Query {
    codes: Vec<i8>,
    scale: f64,
}

impl Bit1Query {
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
}

#[derive(Clone, Copy, Debug)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_open_unit_f64(&mut self) -> f64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^= value >> 31;
        ((value >> 11) as f64 + 0.5) * (1.0 / 9_007_199_254_740_992.0)
    }
}

/// Quantizes one row to one-bit sign codes, packed most-significant bit first.
///
/// Bit `7 - (i % 8)` is one when coordinate `i` is non-negative and zero when
/// it is negative. Unused low bits in the final byte are canonical zero
/// padding. Positive and negative zero share the positive canonical code; a
/// zero row's correction is zero, so its estimate remains exactly zero.
///
/// # Errors
///
/// Returns [`QuantError::EmptyVector`], [`QuantError::DimensionTooLarge`],
/// [`QuantError::NonFinite`], or [`QuantError::OutputLength`]. Validation
/// completes before `out` is modified.
pub fn quantize_bit1(v: &[f32], out: &mut [u8]) -> Result<Bit1Factors, QuantError> {
    validate_vector(v)?;
    let expected = v.len().div_ceil(COORDINATES_PER_BYTE);
    if out.len() != expected {
        return Err(QuantError::OutputLength {
            expected,
            actual: out.len(),
        });
    }

    let mut norm_squared = 0.0_f64;
    let mut absolute_sum = 0.0_f64;
    let mut row_scale = 0.0_f64;
    out.fill(0);
    for (index, &value) in v.iter().enumerate() {
        let value_f64 = f64::from(value);
        let magnitude = value_f64.abs();
        norm_squared += value_f64 * value_f64;
        absolute_sum += magnitude;
        row_scale = row_scale.max(magnitude);
        if value >= 0.0
            && let Some(byte) = out.get_mut(index / COORDINATES_PER_BYTE)
        {
            *byte |= 1_u8 << (7 - index % COORDINATES_PER_BYTE);
        }
    }

    let (normalized_norm, normalized_correction) = if norm_squared == 0.0 {
        (0.0, 0.0)
    } else {
        (
            (norm_squared.sqrt() / row_scale) as f32,
            (norm_squared / (row_scale * absolute_sum)) as f32,
        )
    };
    Ok(Bit1Factors {
        scale: row_scale as f32,
        normalized_norm,
        normalized_correction,
    })
}

/// Reconstructs the norm-scaled selected sign direction.
///
/// The result is `||v|| * h / sqrt(d)`. Serving uses [`est_dot_bit1`]
/// directly; reconstruction exists to specify codec geometry and make golden
/// fixtures independently round-trippable.
///
/// # Errors
///
/// Returns [`QuantError::EmptyVector`] for an empty output and the code-length
/// or padding errors documented by [`est_dot_bit1`].
pub fn dequantize_bit1(
    codes: &[u8],
    factors: Bit1Factors,
    out: &mut [f32],
) -> Result<(), QuantError> {
    if out.is_empty() {
        return Err(QuantError::EmptyVector);
    }
    validate_code(codes, out.len())?;
    if factors.scale == 0.0 {
        out.fill(0.0);
        return Ok(());
    }
    let coordinate = factors.norm() / (out.len() as f64).sqrt();
    for (index, value) in out.iter_mut().enumerate() {
        let bit = codes
            .get(index / COORDINATES_PER_BYTE)
            .copied()
            .unwrap_or_default()
            >> (7 - index % COORDINATES_PER_BYTE)
            & 1;
        let sign = if bit == 0 { -1.0 } else { 1.0 };
        *value = (sign * coordinate).clamp(f64::from(f32::MIN), f64::from(f32::MAX)) as f32;
    }
    Ok(())
}

/// Prepares one query for repeated one-bit row scoring.
///
/// The seed affects reproducible stochastic rounding only; it is not a learned
/// rotation or persisted model parameter.
///
/// # Errors
///
/// Returns the same vector validation errors as [`quantize_bit1`].
pub fn prepare_bit1_query(q: &[f32], seed: u64) -> Result<Bit1Query, QuantError> {
    validate_vector(q)?;
    let maximum = q.iter().map(|value| value.abs()).fold(0.0_f32, f32::max);
    if maximum == 0.0 {
        return Ok(Bit1Query {
            codes: vec![0_i8; q.len()],
            scale: 0.0,
        });
    }

    let scale = f64::from(maximum) / 127.0;
    let mut random = SplitMix64::new(seed);
    let codes = q
        .iter()
        .map(|&value| {
            let scaled = f64::from(value) / scale;
            let lower = scaled.floor();
            let probability_up = scaled - lower;
            let rounded = if random.next_open_unit_f64() < probability_up {
                lower + 1.0
            } else {
                lower
            };
            rounded.clamp(-127.0, 127.0) as i8
        })
        .collect();
    Ok(Bit1Query { codes, scale })
}

/// Estimates a dot product directly from one packed one-bit row.
///
/// Each packed sign is extracted into a scalar register and immediately
/// multiplied by its prepared query code. No expanded row is allocated or
/// written, so unpack traffic is zero bytes even though this v1 path is not a
/// runtime-dispatched SIMD slot.
///
/// # Errors
///
/// Returns [`QuantError::CodeLength`] for a dimension mismatch and
/// [`QuantError::NonZeroPadding`] for non-canonical trailing bits.
pub fn est_dot_bit1(
    query: &Bit1Query,
    codes: &[u8],
    factors: Bit1Factors,
) -> Result<f32, QuantError> {
    validate_code(codes, query.codes.len())?;
    if factors.scale == 0.0 {
        return Ok(0.0);
    }
    let mut integer_dot = 0_i32;
    for (index, &query_code) in query.codes.iter().enumerate() {
        let bit = codes
            .get(index / COORDINATES_PER_BYTE)
            .copied()
            .unwrap_or_default()
            >> (7 - index % COORDINATES_PER_BYTE)
            & 1;
        let sign = if bit == 0 { -1_i32 } else { 1_i32 };
        integer_dot += sign * i32::from(query_code);
    }
    Ok((factors.correction() * query.scale * f64::from(integer_dot)) as f32)
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

fn validate_code(codes: &[u8], dimension: usize) -> Result<(), QuantError> {
    let expected = dimension.div_ceil(COORDINATES_PER_BYTE);
    if codes.len() != expected {
        return Err(QuantError::CodeLength {
            expected,
            actual: codes.len(),
        });
    }
    let used_bits = dimension % COORDINATES_PER_BYTE;
    if used_bits == 0 {
        return Ok(());
    }
    let mask = u8::MAX >> used_bits;
    if let Some(&byte) = codes.last()
        && byte & mask != 0
    {
        return Err(QuantError::NonZeroPadding { byte, mask });
    }
    Ok(())
}
