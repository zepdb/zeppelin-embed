//! Four-bit Extended-RaBitQ row encoding.
//!
//! The conceptual codebook is the normalized half-integer grid
//! `{-7.5, -6.5, ..., -0.5, 0.5, ..., 6.5, 7.5}^d`. It is fixed by the bit
//! width: there is no learned codebook, training set, or first-query setup.
//! Random rotation is optional in Extended-RaBitQ and is off by default in v1;
//! the recall harness must validate that identity-rotation assumption for each
//! embedding model.
//!
//! For input `v`, Algorithm 1 selects `y` maximizing `<y,v>/||y||`. At scale
//! zero each signed magnitude is `0.5`. Coordinate `i` then crosses its seven
//! rounding boundaries at `t = level / |v_i|`, `level=1..=7`. Sorting all
//! critical values, applying equal thresholds as one group, and retaining the
//! best normalized projection enumerates every realizable rounded direction
//! exactly in `O(2^B d log d)` time.
//!
//! The stored RaBitQ correction is `c = ||v||^2 / <y,v>`. The paper writes the
//! estimator with normalized `y`; because `||y||` occurs in numerator and
//! denominator it cancels, leaving `c * <y,q_hat>`. The companion norm is kept
//! for reconstruction/error bounds and future distance bookkeeping.

use crate::kernels::{MAX_DOT_I8_DIMENSION, dot_bit4};

use super::QuantError;

const CODES_PER_BYTE: usize = 2;
const MAX_MAGNITUDE_LEVEL: u8 = 7;

/// Per-row scalars required by the four-bit unbiased dot estimator.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bit4Factors {
    scale: f32,
    normalized_norm: f32,
    normalized_correction: f32,
}

impl Bit4Factors {
    /// Returns the original row's Euclidean norm.
    ///
    /// A finite `f32` row scale and finite normalized `f32` norm are expanded
    /// in `f64`, so the logical norm remains usable above `f32::MAX` without
    /// enlarging the three-scalar factor record.
    #[must_use]
    pub const fn norm(self) -> f64 {
        self.scale as f64 * self.normalized_norm as f64
    }

    /// Returns `||v||^2 / <y, v>` for the selected grid direction.
    ///
    /// The stored normalized correction is multiplied by the finite row scale
    /// in `f64`, avoiding overflow in persisted metadata.
    #[must_use]
    pub const fn correction(self) -> f64 {
        self.scale as f64 * self.normalized_correction as f64
    }

    /// Returns the L2 error bound for norm-scaled code reconstruction.
    ///
    /// Every selected grid coordinate is sign-aligned with the input, hence
    /// the direction cosine is non-negative and the deterministic bound is
    /// `sqrt(2) ||v||`. The exact grid search normally gives a tighter error;
    /// retaining this conservative bound avoids a fourth per-row scalar.
    #[must_use]
    pub fn reconstruction_error_bound(self) -> f64 {
        std::f64::consts::SQRT_2 * self.norm()
    }
}

/// Query-side signed-byte representation prepared once and reused per row.
///
/// Coordinates are symmetrically quantized to `[-127, 127]` with seeded
/// stochastic rounding, so their reconstruction is unbiased in expectation.
#[derive(Clone, Debug, PartialEq)]
pub struct Bit4Query {
    codes: Vec<i8>,
    scale: f64,
}

impl Bit4Query {
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

#[derive(Clone, Copy, Debug)]
struct CriticalValue {
    threshold: f64,
    coordinate: usize,
    level: u8,
    magnitude: f64,
}

/// Quantizes one row to four-bit Extended-RaBitQ codes.
///
/// Two coordinates occupy each byte in coordinate order, high nibble first.
/// Unsigned code `u` represents `u - 7.5`. For an odd dimension, the unused
/// low nibble in the final byte is canonical zero padding.
///
/// # Errors
///
/// Returns [`QuantError::EmptyVector`] for an empty input,
/// [`QuantError::DimensionTooLarge`] above the i8 scoring-kernel limit,
/// [`QuantError::NonFinite`] for NaN or infinity, and
/// [`QuantError::OutputLength`] unless `out.len() == ceil(v.len() / 2)`.
/// Validation completes before `out` is modified.
pub fn quantize_bit4(v: &[f32], out: &mut [u8]) -> Result<Bit4Factors, QuantError> {
    validate_input(v, out.len())?;

    let mut norm_squared = 0.0_f64;
    let mut absolute_sum = 0.0_f64;
    let mut row_scale = 0.0_f64;
    let mut critical_values = Vec::with_capacity(v.len().saturating_mul(7));
    for (coordinate, &value) in v.iter().enumerate() {
        let value = f64::from(value);
        let magnitude = value.abs();
        norm_squared += value * value;
        absolute_sum += magnitude;
        row_scale = row_scale.max(magnitude);
        if magnitude > 0.0 {
            for level in 1..=MAX_MAGNITUDE_LEVEL {
                critical_values.push(CriticalValue {
                    threshold: f64::from(level) / magnitude,
                    coordinate,
                    level,
                    magnitude,
                });
            }
        }
    }
    critical_values.sort_unstable_by(|left, right| {
        left.threshold
            .total_cmp(&right.threshold)
            .then_with(|| left.coordinate.cmp(&right.coordinate))
            .then_with(|| left.level.cmp(&right.level))
    });

    let mut numerator = 0.5 * absolute_sum;
    let mut grid_norm_squared = 0.25 * v.len() as f64;
    let mut best_numerator = numerator;
    let mut best_score_squared = normalized_score_squared(numerator, grid_norm_squared);
    let mut best_event_count = 0_usize;
    let mut event_count = 0_usize;
    let mut events = critical_values.iter().peekable();
    while let Some(event) = events.next() {
        let threshold = event.threshold;
        apply_event(event, &mut numerator, &mut grid_norm_squared);
        event_count += 1;
        while events
            .peek()
            .is_some_and(|next| next.threshold == threshold)
        {
            if let Some(tied) = events.next() {
                apply_event(tied, &mut numerator, &mut grid_norm_squared);
                event_count += 1;
            }
        }
        let score_squared = normalized_score_squared(numerator, grid_norm_squared);
        if score_squared > best_score_squared {
            best_score_squared = score_squared;
            best_numerator = numerator;
            best_event_count = event_count;
        }
    }

    let mut magnitudes = vec![0_u8; v.len()];
    for event in critical_values.iter().take(best_event_count) {
        if let Some(level) = magnitudes.get_mut(event.coordinate) {
            *level = event.level;
        }
    }
    pack(v, &magnitudes, out);

    let (normalized_norm, normalized_correction) = if norm_squared == 0.0 {
        (0.0, 0.0)
    } else {
        (
            (norm_squared.sqrt() / row_scale) as f32,
            (norm_squared / (row_scale * best_numerator)) as f32,
        )
    };
    Ok(Bit4Factors {
        scale: row_scale as f32,
        normalized_norm,
        normalized_correction,
    })
}

/// Reconstructs the norm-scaled selected four-bit code direction.
///
/// The result is `||v|| * y / ||y||`. Serving uses [`est_dot_bit4`] directly;
/// reconstruction exists to specify and verify the codec's geometric error.
///
/// # Errors
///
/// Returns the code-length and padding errors documented by
/// [`est_dot_bit4`]. The output length defines the logical dimension of the
/// final partial byte.
pub fn dequantize_bit4(
    codes: &[u8],
    factors: Bit4Factors,
    out: &mut [f32],
) -> Result<(), QuantError> {
    let unpacked = unpack(codes, out.len())?;
    if factors.scale == 0.0 {
        out.fill(0.0);
        return Ok(());
    }
    let doubled_code_norm = unpacked
        .iter()
        .map(|&value| {
            let value = f64::from(value);
            value * value
        })
        .sum::<f64>()
        .sqrt();
    for (output, &value) in out.iter_mut().zip(&unpacked) {
        *output = finite_f32(factors.norm() * f64::from(value) / doubled_code_norm);
    }
    Ok(())
}

/// Prepares one query for repeated four-bit row scoring.
///
/// The seed controls reproducible stochastic rounding only. Random rotation is
/// not performed by this v1 API. A zero query receives scale zero and all-zero
/// signed codes.
///
/// # Errors
///
/// Returns [`QuantError::EmptyVector`], [`QuantError::DimensionTooLarge`], or
/// [`QuantError::NonFinite`] under the same input policy as
/// [`quantize_bit4`].
pub fn prepare_bit4_query(q: &[f32], seed: u64) -> Result<Bit4Query, QuantError> {
    validate_vector(q)?;
    let max_absolute = q.iter().map(|value| value.abs()).fold(0.0_f32, f32::max);
    if max_absolute == 0.0 {
        return Ok(Bit4Query {
            codes: vec![0_i8; q.len()],
            scale: 0.0,
        });
    }

    let scale = f64::from(max_absolute) / 127.0;
    let mut random = SplitMix64::new(seed);
    let mut codes = Vec::with_capacity(q.len());
    for &value in q {
        let scaled = f64::from(value) / scale;
        let lower = scaled.floor();
        let probability_up = scaled - lower;
        let rounded = if random.next_open_unit_f64() < probability_up {
            lower + 1.0
        } else {
            lower
        };
        codes.push(rounded.clamp(-127.0, 127.0) as i8);
    }
    Ok(Bit4Query { codes, scale })
}

/// Estimates a dot product directly from one packed four-bit row.
///
/// The native packed kernel expands each nibble `u` to `2*u - 15` in scalar
/// variables or SIMD registers, representing twice the selected half-integer
/// grid value. It accumulates immediately and never materializes an expanded
/// row. If the prepared query reconstructs as `scale*z`, the integer kernel
/// gives `<y,q_hat> = scale * dot(2*y,z) / 2`; the stored correction then yields
/// the Extended-RaBitQ estimate.
///
/// For an odd dimension, the unused low nibble must be canonical zero padding.
///
/// # Errors
///
/// Returns [`QuantError::CodeLength`] for a dimension mismatch and
/// [`QuantError::NonZeroPadding`] for non-canonical trailing fields.
pub fn est_dot_bit4(
    query: &Bit4Query,
    codes: &[u8],
    factors: Bit4Factors,
) -> Result<f32, QuantError> {
    validate_code(codes, query.codes.len())?;
    if factors.scale == 0.0 {
        return Ok(0.0);
    }
    let integer_dot = dot_bit4(&query.codes, codes);
    Ok((factors.correction() * query.scale * 0.5 * f64::from(integer_dot)) as f32)
}

fn validate_input(v: &[f32], output_len: usize) -> Result<(), QuantError> {
    validate_vector(v)?;
    let expected = v.len().div_ceil(CODES_PER_BYTE);
    if output_len != expected {
        return Err(QuantError::OutputLength {
            expected,
            actual: output_len,
        });
    }
    Ok(())
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

fn unpack(codes: &[u8], dimension: usize) -> Result<Vec<i8>, QuantError> {
    validate_code(codes, dimension)?;

    let mut unpacked = Vec::with_capacity(dimension);
    for &byte in codes {
        for shift in [4_u32, 0] {
            if unpacked.len() == dimension {
                break;
            }
            let unsigned = (byte >> shift) & 0b1111;
            unpacked.push((i16::from(unsigned) * 2 - 15) as i8);
        }
    }
    Ok(unpacked)
}

fn validate_code(codes: &[u8], dimension: usize) -> Result<(), QuantError> {
    let expected = dimension.div_ceil(CODES_PER_BYTE);
    if codes.len() != expected {
        return Err(QuantError::CodeLength {
            expected,
            actual: codes.len(),
        });
    }
    validate_padding(codes, dimension)?;
    Ok(())
}

fn validate_padding(codes: &[u8], dimension: usize) -> Result<(), QuantError> {
    if dimension.is_multiple_of(CODES_PER_BYTE) {
        return Ok(());
    }
    let mask = 0x0f;
    if let Some(&byte) = codes.last()
        && byte & mask != 0
    {
        return Err(QuantError::NonZeroPadding { byte, mask });
    }
    Ok(())
}

fn normalized_score_squared(numerator: f64, norm_squared: f64) -> f64 {
    numerator * numerator / norm_squared
}

fn finite_f32(value: f64) -> f32 {
    value.clamp(f64::from(f32::MIN), f64::from(f32::MAX)) as f32
}

fn apply_event(event: &CriticalValue, numerator: &mut f64, norm_squared: &mut f64) {
    *numerator += event.magnitude;
    *norm_squared += 2.0 * f64::from(event.level);
}

fn pack(v: &[f32], magnitudes: &[u8], out: &mut [u8]) {
    for ((values, levels), byte) in v
        .chunks(CODES_PER_BYTE)
        .zip(magnitudes.chunks(CODES_PER_BYTE))
        .zip(out.iter_mut())
    {
        let mut packed = 0_u8;
        for (field, (&value, &level)) in values.iter().zip(levels).enumerate() {
            let unsigned = if value < 0.0 {
                MAX_MAGNITUDE_LEVEL - level
            } else {
                MAX_MAGNITUDE_LEVEL + 1 + level
            };
            let shift = 4_u32.saturating_sub((field as u32) * 4);
            packed |= unsigned << shift;
        }
        *byte = packed;
    }
}
