//! Runtime-dispatched dot-product and packed-bit Hamming kernels.
//!
//! Callers validate dimensions once at the engine boundary. Every hot kernel
//! therefore uses `debug_assert!` for the equal-length and row-shape contracts
//! instead of repeating release-mode validation in the scan loop. Call
//! [`crate::kernels::initialize`] during engine initialization to apply the optional
//! `ZE_KERNEL=scalar|neon|avx2` override and surface unsupported requests as a
//! typed [`crate::kernels::KernelInitError`]. If no explicit initialization occurs, the first
//! kernel call safely caches the best runtime-detected arm.
//!
//! The scalar implementation is the behavioral oracle. [`crate::kernels::KernelVariant`]
//! exposes every runtime-supported table to property tests, fuzzing, and the
//! future Task 27 frontier harness without moving selection to build time.

mod dispatch;
mod scalar;

#[cfg(target_arch = "x86_64")]
mod avx2;
#[cfg(target_arch = "aarch64")]
mod neon;

/// Largest supported dimension for an i8 dot product.
///
/// The worst product is `(-128) * (-128) = 16_384`; at this dimension the
/// worst sum is `2^30`, which remains representable by [`i32`].
pub const MAX_DOT_I8_DIMENSION: usize = 65_536;

const _: [(); 1] = [(); (MAX_DOT_I8_DIMENSION * (i8::MIN as i16 * i8::MIN as i16) as usize
    <= i32::MAX as usize) as usize];

/// User-selectable runtime kernel families.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KernelArm {
    /// Portable scalar oracle and fallback.
    Scalar,
    /// AArch64 NEON, using SDOT at runtime when available.
    Neon,
    /// x86-64 AVX2 plus scalar POPCNT.
    Avx2,
}

/// Instruction tiers enumerated for the Task 27 B1 frontier campaign.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstructionTier {
    /// Portable scalar instructions.
    Scalar,
    /// Baseline AArch64 widening multiply.
    NeonWiden,
    /// AArch64 FEAT_DotProd SDOT.
    NeonDotprod,
    /// x86-64 AVX2 plus POPCNT.
    Avx2,
    /// AArch64 FEAT_I8MM SMMLA batch tier.
    NeonI8mmReserved,
    /// Reserved, unimplemented FEAT_SME2 campaign tier.
    Sme2Reserved,
}

/// Runtime CPU capabilities relevant to current and reserved kernel tiers.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KernelFeatures {
    /// AArch64 Advanced SIMD is executable.
    pub neon: bool,
    /// AArch64 FEAT_DotProd is executable.
    pub dotprod: bool,
    /// AArch64 half-precision conversion/arithmetic is executable.
    pub fp16: bool,
    /// AArch64 FEAT_I8MM is executable.
    pub i8mm: bool,
    /// AArch64 FEAT_SME2 was detected; implementation is deferred to B1.
    pub sme2: bool,
    /// x86-64 AVX2 is executable.
    pub avx2: bool,
    /// x86 POPCNT is executable.
    pub popcnt: bool,
}

/// Typed failures produced while applying a runtime kernel override.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KernelInitError {
    /// `ZE_KERNEL` contained a Unicode value outside the supported set.
    UnknownOverride {
        /// Rejected environment value.
        value: String,
    },
    /// `ZE_KERNEL` was not valid Unicode.
    NonUnicodeOverride,
    /// The requested family cannot execute on this CPU/architecture.
    UnsupportedArm {
        /// Requested runtime family.
        requested: KernelArm,
    },
    /// A kernel call selected an arm before a conflicting explicit override.
    AlreadyInitialized {
        /// Arm already cached in the process.
        selected: KernelArm,
        /// Conflicting arm requested by `ZE_KERNEL`.
        requested: KernelArm,
    },
}

impl std::fmt::Display for KernelInitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownOverride { value } => write!(
                formatter,
                "unknown ZE_KERNEL value {value:?}; expected scalar, neon, or avx2"
            ),
            Self::NonUnicodeOverride => formatter.write_str("ZE_KERNEL is not valid Unicode"),
            Self::UnsupportedArm { requested } => {
                write!(
                    formatter,
                    "forced kernel arm {requested:?} is unsupported on this CPU"
                )
            }
            Self::AlreadyInitialized {
                selected,
                requested,
            } => write!(
                formatter,
                "kernel dispatch already selected {selected:?}; cannot force {requested:?}"
            ),
        }
    }
}

impl std::error::Error for KernelInitError {}

/// Enumerable tuning declarations consumed directly by Task 27-H.
///
/// This is data only. Task 03 intentionally does not implement search,
/// measurement ledgers, roofline calculation, or variant code generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KnobSpace {
    /// Candidate loop-unroll depths.
    pub unroll: &'static [usize],
    /// Candidate independent-accumulator counts.
    pub accumulators: &'static [usize],
    /// Candidate rows grouped per scan block.
    pub rows_per_block: &'static [usize],
    /// Candidate software-prefetch distances in rows; zero disables prefetch.
    pub prefetch_dist: &'static [usize],
    /// Current and reserved instruction tiers.
    pub tier: &'static [InstructionTier],
}

/// Baseline micro-shape selected before the Task 27 B1 campaign.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BaselineKernelConfig {
    /// Loop-unroll depth.
    pub unroll: usize,
    /// Number of independent accumulators.
    pub accumulators: usize,
    /// Rows handled per row-major batch iteration.
    pub rows_per_block: usize,
    /// Software-prefetch distance in rows.
    pub prefetch_dist: usize,
}

/// Shipped baseline configuration; B1 may replace every value with evidence.
pub const BASELINE_KERNEL_CONFIG: BaselineKernelConfig = BaselineKernelConfig {
    // Retained by tasks/evidence/opt-ledger/B1.md iterations 1–5.
    unroll: 4,
    accumulators: 4,
    rows_per_block: 1,
    prefetch_dist: 0,
};

// Search provenance: tasks/evidence/opt-ledger/B1.md.
const UNROLL_KNOBS: [usize; 4] = [2, 4, 6, 8];
const ACCUMULATOR_KNOBS: [usize; 4] = [2, 4, 6, 8];
const ROW_BLOCK_KNOBS: [usize; 5] = [1, 2, 4, 8, 16];
const PREFETCH_KNOBS: [usize; 5] = [0, 1, 2, 4, 8];
const TIER_KNOBS: [InstructionTier; 6] = [
    InstructionTier::Scalar,
    InstructionTier::NeonWiden,
    InstructionTier::NeonDotprod,
    InstructionTier::Avx2,
    InstructionTier::NeonI8mmReserved,
    InstructionTier::Sme2Reserved,
]; // Search provenance: tasks/evidence/opt-ledger/B1.md.

/// Registered Task 27-H kernel tuning space.
pub const KERNEL_KNOB_SPACE: KnobSpace = KnobSpace {
    unroll: &UNROLL_KNOBS,
    accumulators: &ACCUMULATOR_KNOBS,
    rows_per_block: &ROW_BLOCK_KNOBS,
    prefetch_dist: &PREFETCH_KNOBS,
    tier: &TIER_KNOBS,
};

type DotI8Fn = fn(&[i8], &[i8]) -> i32;
type DotF32Fn = fn(&[f32], &[f32]) -> f32;
type DotF16Fn = fn(&[u16], &[u16]) -> f32;
type HammingU1Fn = fn(&[u8], &[u8]) -> u32;
type DotI8BatchFn = fn(&[i8], &[i8], usize, &mut [i32]);
type HammingU1BatchFn = fn(&[u8], &[u8], usize, &mut [u32]);
type DotPackedFn = fn(&[i8], &[u8]) -> i32;
type DotPackedBatchFn = fn(&[i8], &[u8], usize, &mut [i32]);

#[derive(Clone, Copy)]
struct KernelTable {
    arm: KernelArm,
    tier: InstructionTier,
    dot_i8: DotI8Fn,
    dot_f32: DotF32Fn,
    dot_f16: DotF16Fn,
    hamming_u1: HammingU1Fn,
    dot_i8_batch: DotI8BatchFn,
    hamming_u1_batch: HammingU1BatchFn,
    dot_bit2: DotPackedFn,
    dot_bit4: DotPackedFn,
    dot_bit2_batch: DotPackedBatchFn,
    dot_bit4_batch: DotPackedBatchFn,
}

/// One concrete runtime dispatch table.
///
/// Variants are exposed so the scalar oracle, property suite, fuzzer, and
/// future frontier harness can evaluate every arm independently of the global
/// once-selected default.
#[derive(Clone, Copy)]
pub struct KernelVariant {
    table: KernelTable,
}

impl std::fmt::Debug for KernelVariant {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("KernelVariant")
            .field("arm", &self.table.arm)
            .field("tier", &self.table.tier)
            .finish()
    }
}

impl KernelVariant {
    /// Returns the scalar oracle variant.
    #[must_use]
    pub fn scalar() -> Self {
        Self {
            table: scalar::table(),
        }
    }

    /// Iterates over every arm executable on the current CPU.
    pub fn available() -> impl Iterator<Item = Self> {
        dispatch::variant_tables()
            .into_iter()
            .flatten()
            .map(|table| Self { table })
    }

    /// Returns this variant's runtime family.
    #[must_use]
    pub fn arm(self) -> KernelArm {
        self.table.arm
    }

    /// Returns this variant's precise instruction tier.
    #[must_use]
    pub fn tier(self) -> InstructionTier {
        self.table.tier
    }

    /// Computes an i8 dot product through this concrete table.
    #[must_use]
    pub fn dot_i8(self, a: &[i8], b: &[i8]) -> i32 {
        (self.table.dot_i8)(a, b)
    }

    /// Computes an f32 dot product through this concrete table.
    #[must_use]
    pub fn dot_f32(self, a: &[f32], b: &[f32]) -> f32 {
        (self.table.dot_f32)(a, b)
    }

    /// Computes an IEEE-f16-bits dot product through this concrete table.
    #[must_use]
    pub fn dot_f16(self, a: &[u16], b: &[u16]) -> f32 {
        (self.table.dot_f16)(a, b)
    }

    /// Computes packed-byte Hamming distance through this concrete table.
    #[must_use]
    pub fn hamming_u1(self, a: &[u8], b: &[u8]) -> u32 {
        (self.table.hamming_u1)(a, b)
    }

    /// Scores contiguous i8 rows through this concrete table.
    pub fn dot_i8_batch(self, q: &[i8], rows: &[i8], d: usize, out: &mut [i32]) {
        (self.table.dot_i8_batch)(q, rows, d, out);
    }

    /// Scores contiguous packed-bit rows through this concrete table.
    pub fn hamming_u1_batch(self, q: &[u8], rows: &[u8], d_bytes: usize, out: &mut [u32]) {
        (self.table.hamming_u1_batch)(q, rows, d_bytes, out);
    }

    /// Scores a signed-byte query against one packed two-bit row.
    #[must_use]
    pub fn dot_bit2(self, q: &[i8], codes: &[u8]) -> i32 {
        (self.table.dot_bit2)(q, codes)
    }

    /// Scores a signed-byte query against one packed four-bit row.
    #[must_use]
    pub fn dot_bit4(self, q: &[i8], codes: &[u8]) -> i32 {
        (self.table.dot_bit4)(q, codes)
    }

    /// Scores a signed-byte query against contiguous packed two-bit rows.
    pub fn dot_bit2_batch(self, q: &[i8], rows: &[u8], d: usize, out: &mut [i32]) {
        (self.table.dot_bit2_batch)(q, rows, d, out);
    }

    /// Scores a signed-byte query against contiguous packed four-bit rows.
    pub fn dot_bit4_batch(self, q: &[i8], rows: &[u8], d: usize, out: &mut [i32]) {
        (self.table.dot_bit4_batch)(q, rows, d, out);
    }
}

/// Applies runtime feature detection and the optional `ZE_KERNEL` override.
///
/// Call this once at engine initialization, before any kernel call. Unknown or
/// unsupported forced arms return a typed error and are never executed.
pub fn initialize() -> Result<KernelArm, KernelInitError> {
    dispatch::initialize()
}

/// Returns the cached arm, selecting the best safe runtime arm if necessary.
#[must_use]
pub fn selected_arm() -> KernelArm {
    dispatch::active_table().arm
}

/// Reports whether a user-selectable arm is executable on this CPU.
#[must_use]
pub fn is_arm_supported(arm: KernelArm) -> bool {
    dispatch::table_for_arm(arm).is_some()
}

/// Returns cached runtime feature detection, including reserved B1 features.
#[must_use]
pub fn detected_features() -> KernelFeatures {
    dispatch::features()
}

/// Streams bytes through four independent wide-load accumulators for the
/// platform-truth bandwidth harness.
///
/// This measurement-only hook returns `None` when an AArch64 NEON
/// implementation is unavailable. It is not part of the distance-kernel API.
#[doc(hidden)]
#[must_use]
pub fn platform_wide_stream_checksum(bytes: &[u8]) -> Option<u64> {
    #[cfg(target_arch = "aarch64")]
    {
        dispatch::features()
            .neon
            .then(|| neon::wide_stream_checksum(bytes))
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        let _ = bytes;
        None
    }
}

/// Computes the dot product of equal-length signed-byte vectors.
///
/// Callers must pre-validate equal lengths and `a.len() <=
/// MAX_DOT_I8_DIMENSION`; the hot path performs `debug_assert!` checks only.
#[must_use]
pub fn dot_i8(a: &[i8], b: &[i8]) -> i32 {
    (dispatch::active_table().dot_i8)(a, b)
}

/// Computes the dot product of equal-length f32 vectors.
///
/// Callers must pre-validate equal lengths; the hot path performs a
/// `debug_assert!` check only. SIMD accumulation may reassociate additions.
#[must_use]
pub fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
    (dispatch::active_table().dot_f32)(a, b)
}

/// Computes an f32 dot product from equal-length IEEE f16 bit vectors.
///
/// Callers must pre-validate equal lengths; the hot path performs a
/// `debug_assert!` check only. Subnormals, infinities, and NaNs follow IEEE
/// conversion and f32 arithmetic behavior.
#[must_use]
pub fn dot_f16(a: &[u16], b: &[u16]) -> f32 {
    (dispatch::active_table().dot_f16)(a, b)
}

/// Computes Hamming distance over equal-length packed-byte vectors.
///
/// Callers must pre-validate equal lengths; the hot path performs a
/// `debug_assert!` check only. Every bit of every supplied byte is significant,
/// including unused high bits in a caller's final partial logical byte.
#[must_use]
pub fn hamming_u1(a: &[u8], b: &[u8]) -> u32 {
    (dispatch::active_table().hamming_u1)(a, b)
}

/// Scores one i8 query against contiguous row-major vectors.
///
/// Callers must pre-validate `q.len() == d`, `rows.len() == d * out.len()`,
/// and `d <= MAX_DOT_I8_DIMENSION`; the hot path uses `debug_assert!` only.
/// A zero dimension deterministically fills `out` with zero.
pub fn dot_i8_batch(q: &[i8], rows: &[i8], d: usize, out: &mut [i32]) {
    (dispatch::active_table().dot_i8_batch)(q, rows, d, out);
}

/// Scores one packed-bit query against contiguous row-major byte vectors.
///
/// Callers must pre-validate `q.len() == d_bytes` and `rows.len() ==
/// d_bytes * out.len()`; the hot path uses `debug_assert!` only. Every bit in
/// the final byte is significant. A zero byte dimension fills `out` with zero.
pub fn hamming_u1_batch(q: &[u8], rows: &[u8], d_bytes: usize, out: &mut [u32]) {
    (dispatch::active_table().hamming_u1_batch)(q, rows, d_bytes, out);
}

/// Scores a signed-byte query against one packed two-bit row.
///
/// Four unsigned fields are stored most-significant first in each byte and
/// decoded to the doubled quantization grid `2 * code - 3`. Callers must
/// pre-validate `codes.len() == q.len().div_ceil(4)` and `q.len() <=
/// MAX_DOT_I8_DIMENSION`; unused trailing fields are ignored. The hot path
/// allocates no memory.
#[must_use]
pub fn dot_bit2(q: &[i8], codes: &[u8]) -> i32 {
    (dispatch::active_table().dot_bit2)(q, codes)
}

/// Scores a signed-byte query against one packed four-bit row.
///
/// Two unsigned fields are stored most-significant first in each byte and
/// decoded to the doubled quantization grid `2 * code - 15`. Callers must
/// pre-validate `codes.len() == q.len().div_ceil(2)` and `q.len() <=
/// MAX_DOT_I8_DIMENSION`; an unused trailing field is ignored. The hot path
/// allocates no memory.
#[must_use]
pub fn dot_bit4(q: &[i8], codes: &[u8]) -> i32 {
    (dispatch::active_table().dot_bit4)(q, codes)
}

/// Scores one signed-byte query against contiguous packed two-bit rows.
///
/// Callers must pre-validate `q.len() == d` and `rows.len() ==
/// d.div_ceil(4) * out.len()` and `d <= MAX_DOT_I8_DIMENSION`. A zero dimension
/// fills `out` with zero. No scratch storage is allocated.
pub fn dot_bit2_batch(q: &[i8], rows: &[u8], d: usize, out: &mut [i32]) {
    (dispatch::active_table().dot_bit2_batch)(q, rows, d, out);
}

/// Scores one signed-byte query against contiguous packed four-bit rows.
///
/// Callers must pre-validate `q.len() == d` and `rows.len() ==
/// d.div_ceil(2) * out.len()` and `d <= MAX_DOT_I8_DIMENSION`. A zero dimension
/// fills `out` with zero. No scratch storage is allocated.
pub fn dot_bit4_batch(q: &[i8], rows: &[u8], d: usize, out: &mut [i32]) {
    (dispatch::active_table().dot_bit4_batch)(q, rows, d, out);
}
