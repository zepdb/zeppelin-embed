//! Two-bit Extended-RaBitQ row encoding.
//!
//! The codec is training-free and data-oblivious. Its conceptual codebook is
//! the normalized half-integer grid `{-1.5, -0.5, 0.5, 1.5}^d`; v1 applies no
//! random rotation. Rotation remains an optional future query/model setting,
//! off by default, whose recall effect must be validated per embedding model.
//!
//! For input `v`, the encoder selects the grid vector `y` maximizing
//! `<y, v> / ||y||`. Extended-RaBitQ Algorithm 1 observes that an optimum is
//! produced by rounding `t * v` for some positive rescale `t`. At two bits,
//! every coordinate starts at signed magnitude `0.5` and has one critical
//! value `t = 1 / |v_i|` where it changes to `1.5`. Sorting those critical
//! values and evaluating each tied group is therefore an exact `O(d log d)`
//! search, not per-coordinate scalar rounding and not a trained codebook.
//!
//! The stored correction follows the RaBitQ estimator. With
//! `c = ||v||^2 / <y, v>`, scoring returns `c * <y, q_hat>`. Normalizing `y`
//! in both the numerator and denominator cancels its norm, so storing `||v||`
//! and `c` is equivalent to the paper's
//! `<y/||y||, q> / <y/||y||, v>` form. A zero row stores zero correction and
//! scores as exactly zero.

use crate::kernels::{MAX_DOT_I8_DIMENSION, dot_bit2};

use super::QuantError;

const CODES_PER_BYTE: usize = 4;
const MAX_MAGNITUDE_LEVEL: u8 = 1;

/// Per-row scalars required by the two-bit unbiased dot estimator.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bit2Factors {
    scale: f32,
    normalized_norm: f32,
    normalized_correction: f32,
}

impl Bit2Factors {
    /// Returns the original row's Euclidean norm.
    ///
    /// The norm is stored as a finite `f32` scale times a finite normalized
    /// `f32` norm, then reconstructed in `f64`. This keeps the three-scalar
    /// factor record usable even when the full row norm exceeds `f32::MAX`.
    #[must_use]
    pub const fn norm(self) -> f64 {
        self.scale as f64 * self.normalized_norm as f64
    }

    /// Returns `||v||^2 / <y, v>` for the selected grid direction.
    ///
    /// As with [`Self::norm`], the persisted finite `f32` value is normalized
    /// by the maximum absolute row coordinate and expanded in `f64` here.
    #[must_use]
    pub const fn correction(self) -> f64 {
        self.scale as f64 * self.normalized_correction as f64
    }

    /// Returns the L2 error bound for norm-scaled code reconstruction.
    ///
    /// The selected grid coordinate always has the sign of its source
    /// coordinate, so its cosine with the row is non-negative. Consequently
    /// `||v - ||v|| y/||y|||| <= sqrt(2) ||v||`. The actual encoder minimizes
    /// the angular error within the grid; this public bound is the conservative
    /// deterministic guarantee requiring no fourth per-row scalar.
    #[must_use]
    pub fn reconstruction_error_bound(self) -> f64 {
        std::f64::consts::SQRT_2 * self.norm()
    }
}

/// Query-side signed-byte representation prepared once and reused per row.
///
/// Coordinates are symmetrically quantized to `[-127, 127]`. Seeded
/// stochastic rounding makes every reconstructed coordinate unbiased in
/// expectation while keeping the query deterministic for a given seed.
#[derive(Clone, Debug, PartialEq)]
pub struct Bit2Query {
    codes: Vec<i8>,
    scale: f64,
}

impl Bit2Query {
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

/// Quantizes one row to two-bit Extended-RaBitQ codes.
///
/// Four coordinates occupy each byte in coordinate order, most-significant
/// field first. Unsigned fields `0, 1, 2, 3` denote grid values `-1.5, -0.5,
/// 0.5, 1.5`. If the dimension is not divisible by four, unused low-order
/// fields in the final byte are canonical zero padding.
///
/// Random rotation is optional in the algorithm and deliberately absent from
/// this v1 entry point. The identity rotation keeps encoding training-free and
/// deterministic; the recall harness must validate that assumption per model.
///
/// # Errors
///
/// Returns [`QuantError::EmptyVector`] for an empty input,
/// [`QuantError::DimensionTooLarge`] above the i8 scoring-kernel limit,
/// [`QuantError::NonFinite`] for NaN or infinity, and
/// [`QuantError::OutputLength`] unless `out.len() == ceil(v.len() / 4)`.
/// Validation completes before `out` is modified.
pub fn quantize_bit2(v: &[f32], out: &mut [u8]) -> Result<Bit2Factors, QuantError> {
    validate_input(v, out.len())?;

    let mut norm_squared = 0.0_f64;
    let mut absolute_sum = 0.0_f64;
    let mut row_scale = 0.0_f64;
    let mut critical_values = Vec::with_capacity(v.len());
    for (coordinate, &value) in v.iter().enumerate() {
        let value = f64::from(value);
        let magnitude = value.abs();
        norm_squared += value * value;
        absolute_sum += magnitude;
        row_scale = row_scale.max(magnitude);
        if magnitude > 0.0 {
            critical_values.push(CriticalValue {
                threshold: 1.0 / magnitude,
                coordinate,
                level: 1,
                magnitude,
            });
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
    Ok(Bit2Factors {
        scale: row_scale as f32,
        normalized_norm,
        normalized_correction,
    })
}

/// Reconstructs the norm-scaled selected two-bit code direction.
///
/// This returns `||v|| * y / ||y||`. Extended-RaBitQ scoring uses the stored
/// correction instead of this reconstruction, but exposing the direction
/// makes codec round-trip error and golden fixtures independently testable.
///
/// # Errors
///
/// Returns the code-length and padding errors documented by [`est_dot_bit2`].
/// The output length defines the logical dimension of the final partial byte.
pub fn dequantize_bit2(
    codes: &[u8],
    factors: Bit2Factors,
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

/// Prepares one query for repeated two-bit row scoring.
///
/// The seed controls only reproducible stochastic rounding; it is not a
/// learned parameter or a persisted rotation. A zero query receives scale zero
/// and all-zero signed codes.
///
/// # Errors
///
/// Returns [`QuantError::EmptyVector`], [`QuantError::DimensionTooLarge`], or
/// [`QuantError::NonFinite`] under the same input policy as
/// [`quantize_bit2`].
pub fn prepare_bit2_query(q: &[f32], seed: u64) -> Result<Bit2Query, QuantError> {
    validate_vector(q)?;
    let max_absolute = q.iter().map(|value| value.abs()).fold(0.0_f32, f32::max);
    if max_absolute == 0.0 {
        return Ok(Bit2Query {
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
    Ok(Bit2Query { codes, scale })
}

/// Estimates a dot product directly from one packed two-bit row.
///
/// The native packed kernel expands each two-bit field `u` to the odd signed
/// byte `2*u - 3` in scalar variables or SIMD registers, representing twice
/// the grid value `y = u - 1.5`. It accumulates immediately and never
/// materializes an expanded row. The prepared query approximates `q` as
/// `scale*z`, so the runtime-dispatched kernel yields
/// `<y,q_hat> = scale * dot(2*y,z) / 2`. Multiplying by the stored correction
/// produces the Extended-RaBitQ estimator.
///
/// The final partial byte is canonical: unused low-order fields must be zero
/// and are never scored.
///
/// # Errors
///
/// Returns [`QuantError::CodeLength`] for a dimension mismatch and
/// [`QuantError::NonZeroPadding`] for non-canonical trailing fields.
pub fn est_dot_bit2(
    query: &Bit2Query,
    codes: &[u8],
    factors: Bit2Factors,
) -> Result<f32, QuantError> {
    validate_code(codes, query.codes.len())?;
    if factors.scale == 0.0 {
        return Ok(0.0);
    }
    let integer_dot = dot_bit2(&query.codes, codes);
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
        for shift in [6_u32, 4, 2, 0] {
            if unpacked.len() == dimension {
                break;
            }
            let unsigned = (byte >> shift) & 0b11;
            unpacked.push((i16::from(unsigned) * 2 - 3) as i8);
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
    let used_fields = dimension % CODES_PER_BYTE;
    if used_fields == 0 {
        return Ok(());
    }
    let unused_bits = (CODES_PER_BYTE - used_fields) * 2;
    let mask = u8::MAX >> (8 - unused_bits);
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
            let shift = 6_u32.saturating_sub((field as u32) * 2);
            packed |= unsigned << shift;
        }
        *byte = packed;
    }
}
