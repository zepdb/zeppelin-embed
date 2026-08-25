//! Measured Task-03 hot-kernel performance contracts.

use std::fmt;

use crate::frontier::roofline::WIDE_LOAD_SINGLE_CORE_GBPS;

const DIMENSION: f64 = 768.0;
const U1_BYTES: f64 = DIMENSION / 8.0;
const NEON_FMA_GMAC_PER_SECOND: f64 = 33.294_029;
const NEON_FP16_CONVERT_FMA_GMAC_PER_SECOND: f64 = 21.569_397;

// Evidence: tasks/evidence/03-kernels.md, single-vector latency row i8 d=768
// = 10.004013 ns, against the 80.689179 GB/s BL-013 denominator, measured
// 2026-08-20 on Apple M3 Max (Mac15,9): 95.141868% roofline. The 19% floor
// retains 5.01x headroom.
pub const I8_ROOFLINE_FLOOR_PERCENT: f64 = 19.0;
// Evidence: tasks/evidence/03-kernels.md, single-vector latency row u1 d=768
// = 2.919274 ns for 96 bytes, against the 80.689179 GB/s BL-013 denominator,
// measured 2026-08-20 on Apple M3 Max (Mac15,9): 40.755017% roofline. The 8%
// floor retains 5.09x headroom.
pub const U1_ROOFLINE_FLOOR_PERCENT: f64 = 8.0;
// Evidence: tasks/evidence/03-kernels.md, single-vector latency row f32 d=768
// = 43.288878 ns, against the 33.294029 GMAC/s persisted compute denominator,
// measured 2026-08-20 on Apple M3 Max (Mac15,9): 53.286665% roofline. The 10%
// floor retains 5.33x headroom.
pub const F32_ROOFLINE_FLOOR_PERCENT: f64 = 10.0;
// Evidence: tasks/evidence/03-kernels.md, single-vector latency row f16 d=768
// = 44.348737 ns, against the 21.569397 GMAC/s persisted compute denominator,
// measured 2026-08-20 on Apple M3 Max (Mac15,9): 80.286395% roofline. The 16%
// floor retains 5.02x headroom.
pub const F16_ROOFLINE_FLOOR_PERCENT: f64 = 16.0;

// At equal d, f32 touches four times i8's row bytes. The task-03 evidence row
// measures 43.288878 / 10.004013 = 4.327151x; the owner-selected 8x ceiling
// leaves room for platform noise while still catching the historical 10x loss.
pub const F32_OVER_I8_CEILING: f64 = 8.0;
// At equal d, f16 touches twice i8's row bytes. The task-03 evidence row
// measures 44.348737 / 10.004013 = 4.433095x; the owner-selected 6x ceiling
// automates the physical ratio check that exposed the historical regression.
pub const F16_OVER_I8_CEILING: f64 = 6.0;

/// Median single-vector latency for every Task-03 hot kernel at d=768.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KernelMeasurements {
    /// Signed-byte dot-product latency.
    pub i8_ns: f64,
    /// Packed-bit Hamming latency.
    pub u1_ns: f64,
    /// Binary16-to-binary32 dot-product latency.
    pub f16_ns: f64,
    /// Binary32 dot-product latency.
    pub f32_ns: f64,
}

/// Derived roofline percentages and cross-kernel latency ratios.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KernelGateReport {
    /// i8 percent of the adopted one-core memory denominator.
    pub i8_roofline_percent: f64,
    /// u1 percent of the adopted one-core memory denominator.
    pub u1_roofline_percent: f64,
    /// f16 percent of the persisted conversion-plus-FMA denominator.
    pub f16_roofline_percent: f64,
    /// f32 percent of the persisted FMA denominator.
    pub f32_roofline_percent: f64,
    /// f32 latency divided by i8 latency at equal dimension.
    pub f32_over_i8: f64,
    /// f16 latency divided by i8 latency at equal dimension.
    pub f16_over_i8: f64,
}

/// One independent reason a hot-kernel run is rejected.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GateFailure {
    /// A measured latency was not positive and finite.
    InvalidMeasurement {
        /// Stable kernel name.
        kernel: &'static str,
        /// Rejected nanoseconds per vector.
        ns: f64,
    },
    /// A kernel fell below its evidence-derived roofline floor.
    RooflineFloor {
        /// Stable kernel name.
        kernel: &'static str,
        /// Measured percent of roofline.
        actual_percent: f64,
        /// Required percent of roofline.
        floor_percent: f64,
    },
    /// A cross-kernel latency ratio exceeded its physical ceiling.
    RatioCeiling {
        /// Slower-width numerator kernel.
        numerator: &'static str,
        /// Narrow reference denominator kernel.
        denominator: &'static str,
        /// Measured latency ratio.
        actual: f64,
        /// Maximum permitted latency ratio.
        ceiling: f64,
    },
}

impl fmt::Display for GateFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMeasurement { kernel, ns } => {
                write!(
                    formatter,
                    "{kernel} returned invalid latency {ns} ns/vector"
                )
            }
            Self::RooflineFloor {
                kernel,
                actual_percent,
                floor_percent,
            } => write!(
                formatter,
                "{kernel} roofline {actual_percent:.6}% is below floor {floor_percent:.6}%"
            ),
            Self::RatioCeiling {
                numerator,
                denominator,
                actual,
                ceiling,
            } => write!(
                formatter,
                "{numerator}/{denominator} latency ratio {actual:.6}x exceeds ceiling {ceiling:.6}x"
            ),
        }
    }
}

/// Evaluates every floor and ratio without stopping at the first failure.
pub fn evaluate_kernel_measurements(
    measurements: KernelMeasurements,
) -> Result<KernelGateReport, Vec<GateFailure>> {
    let mut failures = Vec::new();
    for (kernel, ns) in [
        ("i8", measurements.i8_ns),
        ("u1", measurements.u1_ns),
        ("f16", measurements.f16_ns),
        ("f32", measurements.f32_ns),
    ] {
        if !ns.is_finite() || ns <= 0.0 {
            failures.push(GateFailure::InvalidMeasurement { kernel, ns });
        }
    }
    if !failures.is_empty() {
        return Err(failures);
    }

    let report = KernelGateReport {
        i8_roofline_percent: memory_roofline_percent(DIMENSION, measurements.i8_ns),
        u1_roofline_percent: memory_roofline_percent(U1_BYTES, measurements.u1_ns),
        f16_roofline_percent: compute_roofline_percent(
            DIMENSION,
            NEON_FP16_CONVERT_FMA_GMAC_PER_SECOND,
            measurements.f16_ns,
        ),
        f32_roofline_percent: compute_roofline_percent(
            DIMENSION,
            NEON_FMA_GMAC_PER_SECOND,
            measurements.f32_ns,
        ),
        f32_over_i8: measurements.f32_ns / measurements.i8_ns,
        f16_over_i8: measurements.f16_ns / measurements.i8_ns,
    };
    for (kernel, actual_percent, floor_percent) in [
        ("i8", report.i8_roofline_percent, I8_ROOFLINE_FLOOR_PERCENT),
        ("u1", report.u1_roofline_percent, U1_ROOFLINE_FLOOR_PERCENT),
        (
            "f16",
            report.f16_roofline_percent,
            F16_ROOFLINE_FLOOR_PERCENT,
        ),
        (
            "f32",
            report.f32_roofline_percent,
            F32_ROOFLINE_FLOOR_PERCENT,
        ),
    ] {
        if actual_percent < floor_percent {
            failures.push(GateFailure::RooflineFloor {
                kernel,
                actual_percent,
                floor_percent,
            });
        }
    }
    for (numerator, actual, ceiling) in [
        ("f32", report.f32_over_i8, F32_OVER_I8_CEILING),
        ("f16", report.f16_over_i8, F16_OVER_I8_CEILING),
    ] {
        if actual >= ceiling {
            failures.push(GateFailure::RatioCeiling {
                numerator,
                denominator: "i8",
                actual,
                ceiling,
            });
        }
    }
    if failures.is_empty() {
        Ok(report)
    } else {
        Err(failures)
    }
}

fn memory_roofline_percent(bytes: f64, elapsed_ns: f64) -> f64 {
    (bytes / WIDE_LOAD_SINGLE_CORE_GBPS) / elapsed_ns * 100.0
}

fn compute_roofline_percent(operations: f64, gmac_per_second: f64, elapsed_ns: f64) -> f64 {
    (operations / gmac_per_second) / elapsed_ns * 100.0
}
