//! Registry bridging task-03 knob declarations to concrete kernel builds.

use std::fmt;

use zeppelin_embed::kernels::{
    BASELINE_KERNEL_CONFIG, InstructionTier, KERNEL_KNOB_SPACE, KernelVariant,
};

/// One point in the task-03 kernel tuning grid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KernelPoint {
    /// Loop-unroll depth.
    pub unroll: usize,
    /// Independent accumulator count.
    pub accumulators: usize,
    /// Rows processed together.
    pub rows_per_block: usize,
    /// Software-prefetch distance in rows.
    pub prefetch_dist: usize,
    /// Runtime instruction tier.
    pub tier: InstructionTier,
}

impl KernelPoint {
    /// Stable identifier used in trajectories and ledgers.
    #[must_use]
    pub fn stable_id(self) -> String {
        format!(
            "u{}-a{}-r{}-p{}-{}",
            self.unroll,
            self.accumulators,
            self.rows_per_block,
            self.prefetch_dist,
            tier_name(self.tier)
        )
    }
}

/// One executable production kernel with an honest baseline-shape label.
#[derive(Clone, Copy)]
pub struct RegisteredVariant {
    point: KernelPoint,
    kernel: KernelVariant,
}

impl fmt::Debug for RegisteredVariant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RegisteredVariant")
            .field("point", &self.point)
            .field("kernel", &self.kernel)
            .finish()
    }
}

impl RegisteredVariant {
    /// Returns this build's tuning point.
    #[must_use]
    pub const fn point(&self) -> KernelPoint {
        self.point
    }

    /// Executes an i8 dot product through this concrete production table.
    #[must_use]
    pub fn dot_i8(&self, left: &[i8], right: &[i8]) -> i32 {
        self.kernel.dot_i8(left, right)
    }

    /// Executes a contiguous row-major i8 batch through the production table.
    pub fn dot_i8_batch(&self, query: &[i8], rows: &[i8], d: usize, out: &mut [i32]) {
        self.kernel.dot_i8_batch(query, rows, d, out);
    }
}

/// Full declared grid plus the task-03 points already backed by code.
#[derive(Clone, Debug)]
pub struct VariantRegistry {
    declared: Vec<KernelPoint>,
    materialized: Vec<RegisteredVariant>,
}

impl VariantRegistry {
    /// Consumes `KERNEL_KNOB_SPACE` and materializes the shipped baseline shape.
    ///
    /// Task 03 has one compiled micro-shape per executable instruction tier.
    /// Other declared points remain explicit registry candidates for later B1
    /// variant additions; the harness never pretends an ignored knob is built.
    pub fn from_kernel_knob_space() -> Result<Self, VariantRegistryError> {
        validate_space()?;
        let mut declared = Vec::new();
        for &unroll in KERNEL_KNOB_SPACE.unroll {
            for &accumulators in KERNEL_KNOB_SPACE.accumulators {
                for &rows_per_block in KERNEL_KNOB_SPACE.rows_per_block {
                    for &prefetch_dist in KERNEL_KNOB_SPACE.prefetch_dist {
                        for &tier in KERNEL_KNOB_SPACE.tier {
                            declared.push(KernelPoint {
                                unroll,
                                accumulators,
                                rows_per_block,
                                prefetch_dist,
                                tier,
                            });
                        }
                    }
                }
            }
        }
        let mut materialized: Vec<RegisteredVariant> = Vec::new();
        for kernel in KernelVariant::available()
            .filter(|variant| KERNEL_KNOB_SPACE.tier.contains(&variant.tier()))
        {
            if materialized
                .iter()
                .any(|variant| variant.point.tier == kernel.tier())
            {
                continue;
            }
            materialized.push(materialize_task03_baseline(kernel));
        }
        Ok(Self {
            declared,
            materialized,
        })
    }

    /// All points declared by task 03, including reserved tiers and shapes.
    #[must_use]
    pub fn declared_points(&self) -> &[KernelPoint] {
        &self.declared
    }

    /// Points with a real callable monomorphized task-03 build today.
    #[must_use]
    pub fn materialized(&self) -> &[RegisteredVariant] {
        &self.materialized
    }
}

/// Invalid task-03 knob declaration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VariantRegistryError {
    /// One dimension of the grid was empty.
    EmptyDimension(&'static str),
    /// The shipped baseline was absent from its declared dimension.
    BaselineOutsideSpace(&'static str),
}

impl fmt::Display for VariantRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyDimension(name) => {
                write!(formatter, "kernel knob dimension {name} is empty")
            }
            Self::BaselineOutsideSpace(name) => {
                write!(
                    formatter,
                    "task-03 baseline {name} is outside KERNEL_KNOB_SPACE"
                )
            }
        }
    }
}

impl std::error::Error for VariantRegistryError {}

fn validate_space() -> Result<(), VariantRegistryError> {
    for (name, values) in [
        ("unroll", KERNEL_KNOB_SPACE.unroll),
        ("accumulators", KERNEL_KNOB_SPACE.accumulators),
        ("rows_per_block", KERNEL_KNOB_SPACE.rows_per_block),
        ("prefetch_dist", KERNEL_KNOB_SPACE.prefetch_dist),
    ] {
        if values.is_empty() {
            return Err(VariantRegistryError::EmptyDimension(name));
        }
    }
    if KERNEL_KNOB_SPACE.tier.is_empty() {
        return Err(VariantRegistryError::EmptyDimension("tier"));
    }
    for (name, value, values) in [
        (
            "unroll",
            BASELINE_KERNEL_CONFIG.unroll,
            KERNEL_KNOB_SPACE.unroll,
        ),
        (
            "accumulators",
            BASELINE_KERNEL_CONFIG.accumulators,
            KERNEL_KNOB_SPACE.accumulators,
        ),
        (
            "rows_per_block",
            BASELINE_KERNEL_CONFIG.rows_per_block,
            KERNEL_KNOB_SPACE.rows_per_block,
        ),
        (
            "prefetch_dist",
            BASELINE_KERNEL_CONFIG.prefetch_dist,
            KERNEL_KNOB_SPACE.prefetch_dist,
        ),
    ] {
        if !values.contains(&value) {
            return Err(VariantRegistryError::BaselineOutsideSpace(name));
        }
    }
    Ok(())
}

fn materialize_task03_baseline(kernel: KernelVariant) -> RegisteredVariant {
    RegisteredVariant {
        point: KernelPoint {
            unroll: BASELINE_KERNEL_CONFIG.unroll,
            accumulators: BASELINE_KERNEL_CONFIG.accumulators,
            rows_per_block: BASELINE_KERNEL_CONFIG.rows_per_block,
            prefetch_dist: BASELINE_KERNEL_CONFIG.prefetch_dist,
            tier: kernel.tier(),
        },
        kernel,
    }
}

fn tier_name(tier: InstructionTier) -> &'static str {
    match tier {
        InstructionTier::Scalar => "scalar",
        InstructionTier::NeonWiden => "neon-widen",
        InstructionTier::NeonDotprod => "neon-dotprod",
        InstructionTier::Avx2 => "avx2",
        InstructionTier::NeonI8mmReserved => "neon-i8mm-smmla-r2",
        InstructionTier::Sme2Reserved => "sme2-reserved",
    }
}
