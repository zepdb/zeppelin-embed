//! Memory and compute roofline calculations with revisable provenance.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use super::attestation::{
    AttestationSource, CampaignPreflightOutcome, MachineStateProvenance, preflight_with_attestation,
};
use super::calibration::{CalibrationError, default_calibration_path, load_calibration};
use super::measure::{
    MachineProbe, MeasurementConfig, MeasurementError, MeasurementResult, PreflightOutcome,
    preflight,
};
#[cfg(target_arch = "aarch64")]
use super::measure::{SampleSource, measure_source_with_provenance};

/// Adopted BL-013 one-core wide-load denominator in decimal GB/s.
pub const WIDE_LOAD_SINGLE_CORE_GBPS: f64 = 80.689_179;

/// Which physical bound governs a workload's score.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BindingBound {
    /// Bytes moved against a measured sustained-memory rate.
    Memory,
    /// Operations issued against a measured instruction-path rate.
    Compute,
}

/// Measured compute paths used by current distance kernels.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ComputeTier {
    /// AArch64 four-lane binary32 fused multiply-add.
    NeonFma,
    /// AArch64 sixteen-lane signed-byte dot product.
    NeonSdot,
    /// AArch64 binary16-to-binary32 conversion followed by FMA.
    NeonFp16ConvertFma,
}

impl ComputeTier {
    /// Stable command-line and ledger name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NeonFma => "neon-fma-f32",
            Self::NeonSdot => "neon-sdot-i8",
            Self::NeonFp16ConvertFma => "neon-fp16-convert-fma",
        }
    }
}

/// Provenance attached to every adopted denominator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DenominatorProvenance {
    /// Human-readable measurement method.
    pub method: String,
    /// Exact command that produced the measurement.
    pub command: String,
    /// Measurement date in `YYYY-MM-DD` form.
    pub measured_date: String,
}

impl DenominatorProvenance {
    /// Creates provenance only when all required evidence fields are present.
    pub fn measured(
        method: impl Into<String>,
        command: impl Into<String>,
        measured_date: impl Into<String>,
    ) -> Result<Self, RooflineError> {
        let provenance = Self {
            method: method.into(),
            command: command.into(),
            measured_date: measured_date.into(),
        };
        if provenance.method.trim().is_empty()
            || provenance.command.trim().is_empty()
            || provenance.measured_date.trim().is_empty()
        {
            return Err(RooflineError::MissingProvenance);
        }
        Ok(provenance)
    }
}

/// Sustained compute ceiling for one instruction path.
#[derive(Clone, Debug, PartialEq)]
pub struct ComputeCeiling {
    /// Measured instruction path.
    pub tier: ComputeTier,
    /// Sustained multiply-accumulate operations per second.
    pub operations_per_second: f64,
    /// Measurement provenance.
    pub provenance: DenominatorProvenance,
}

impl ComputeCeiling {
    /// Creates a positive, finite measured compute ceiling.
    pub fn new(
        tier: ComputeTier,
        operations_per_second: f64,
        provenance: DenominatorProvenance,
    ) -> Result<Self, RooflineError> {
        validate_positive(operations_per_second)?;
        Ok(Self {
            tier,
            operations_per_second,
            provenance,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
struct MemoryCeiling {
    gigabytes_per_second: f64,
    provenance: DenominatorProvenance,
}

/// Upward-only collection of measured roofline denominators.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RooflineModel {
    memory: BTreeMap<usize, MemoryCeiling>,
    compute: BTreeMap<ComputeTier, ComputeCeiling>,
}

impl RooflineModel {
    /// Creates an empty denominator model.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Loads the tracked machine calibration and the adopted BL-013 memory ceiling.
    pub fn from_default_calibration() -> Result<Self, RooflineLoadError> {
        Self::from_persisted_calibration(default_calibration_path())
    }

    /// Loads compute ceilings from a persisted artifact for subsequent scoring.
    ///
    /// The memory side is always the owner-adopted one-core BL-013 wide-load
    /// ceiling. Every compute ceiling retains the artifact's machine, command,
    /// and caller-supplied date as score provenance.
    pub fn from_persisted_calibration(path: impl AsRef<Path>) -> Result<Self, RooflineLoadError> {
        let artifact = load_calibration(path).map_err(RooflineLoadError::Calibration)?;
        let memory_provenance = DenominatorProvenance::measured(
            "BL-013 four-accumulator NEON wide-load",
            "cargo run --release -p zeppelin-embed-bench --bin platform-truth -- bandwidth-compare",
            "2026-08-20",
        )
        .map_err(RooflineLoadError::Roofline)?;
        let mut model = Self::new();
        model
            .revise_memory_ceiling(1, WIDE_LOAD_SINGLE_CORE_GBPS, memory_provenance)
            .map_err(RooflineLoadError::Roofline)?;
        for tier in &artifact.tiers {
            let method = format!(
                "{} sustained saturation calibration on {} {} ({}, {}; {}; {})",
                tier.tier.as_str(),
                artifact.machine.model_name,
                artifact.machine.model_identifier,
                artifact.machine.chip,
                artifact.machine.os_build,
                artifact.machine.power_state,
                artifact.machine.thermal_state
            );
            let provenance = DenominatorProvenance::measured(
                method,
                artifact.command.clone(),
                artifact.measured_date.clone(),
            )
            .map_err(RooflineLoadError::Roofline)?;
            let ceiling = ComputeCeiling::new(
                tier.tier,
                tier.adopted_gmac_per_second * 1_000_000_000.0,
                provenance,
            )
            .map_err(RooflineLoadError::Roofline)?;
            model
                .revise_compute_ceiling(ceiling)
                .map_err(RooflineLoadError::Roofline)?;
        }
        Ok(model)
    }

    /// Installs a memory denominator, rejecting downward revisions.
    pub fn revise_memory_ceiling(
        &mut self,
        cores: usize,
        gigabytes_per_second: f64,
        provenance: DenominatorProvenance,
    ) -> Result<(), RooflineError> {
        if cores == 0 {
            return Err(RooflineError::ZeroCoreCount);
        }
        validate_positive(gigabytes_per_second)?;
        if let Some(previous) = self.memory.get(&cores)
            && gigabytes_per_second < previous.gigabytes_per_second
        {
            return Err(RooflineError::DownwardRevision {
                previous: previous.gigabytes_per_second,
                proposed: gigabytes_per_second,
            });
        }
        self.memory.insert(
            cores,
            MemoryCeiling {
                gigabytes_per_second,
                provenance,
            },
        );
        Ok(())
    }

    /// Installs a compute denominator, rejecting downward revisions.
    pub fn revise_compute_ceiling(&mut self, ceiling: ComputeCeiling) -> Result<(), RooflineError> {
        if let Some(previous) = self.compute.get(&ceiling.tier)
            && ceiling.operations_per_second < previous.operations_per_second
        {
            return Err(RooflineError::DownwardRevision {
                previous: previous.operations_per_second,
                proposed: ceiling.operations_per_second,
            });
        }
        self.compute.insert(ceiling.tier, ceiling);
        Ok(())
    }

    /// Scores one run against both denominators and the declared binding one.
    pub fn score(&self, input: RooflineInput) -> Result<RooflineScore, RooflineError> {
        validate_positive(input.elapsed_seconds)?;
        let memory = self
            .memory
            .get(&input.cores)
            .ok_or(RooflineError::MissingMemoryCeiling { cores: input.cores })?;
        let compute =
            self.compute
                .get(&input.compute_tier)
                .ok_or(RooflineError::MissingComputeCeiling {
                    tier: input.compute_tier,
                })?;
        let memory_bound_seconds =
            input.bytes_touched as f64 / (memory.gigabytes_per_second * 1_000_000_000.0);
        let compute_bound_seconds = input.operation_count as f64 / compute.operations_per_second;
        let binding_seconds = match input.binding {
            BindingBound::Memory => memory_bound_seconds,
            BindingBound::Compute => compute_bound_seconds,
        };
        let achieved_percent = binding_seconds / input.elapsed_seconds * 100.0;
        let diagnostic = if achieved_percent > 100.0 {
            RooflineDiagnostic::DenominatorStale {
                achieved_percent,
                binding: input.binding,
            }
        } else {
            RooflineDiagnostic::WithinDenominator
        };
        Ok(RooflineScore {
            memory_bound_seconds,
            compute_bound_seconds,
            binding: input.binding,
            achieved_percent,
            diagnostic,
            memory_provenance: memory.provenance.clone(),
            compute_provenance: compute.provenance.clone(),
        })
    }
}

/// Work and elapsed-time inputs for a roofline score.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RooflineInput {
    /// Total bytes touched by the measured workload.
    pub bytes_touched: u64,
    /// Total multiply-accumulate or equivalent operations.
    pub operation_count: u64,
    /// Measured elapsed wall time in seconds.
    pub elapsed_seconds: f64,
    /// Active worker/core count selecting the memory denominator.
    pub cores: usize,
    /// Instruction path selecting the compute denominator.
    pub compute_tier: ComputeTier,
    /// Workload-declared binding bound.
    pub binding: BindingBound,
}

/// Explicit diagnostic attached to every roofline score.
#[derive(Clone, Debug, PartialEq)]
pub enum RooflineDiagnostic {
    /// The run did not exceed the adopted binding denominator.
    WithinDenominator,
    /// The adopted denominator was exceeded and must be re-measured upward.
    DenominatorStale {
        /// Observed percent of the stale denominator.
        achieved_percent: f64,
        /// Denominator family that was exceeded.
        binding: BindingBound,
    },
}

/// Both physical bounds plus the selected workload score.
#[derive(Clone, Debug, PartialEq)]
pub struct RooflineScore {
    /// Lower-bound time implied by sustained memory throughput.
    pub memory_bound_seconds: f64,
    /// Lower-bound time implied by sustained instruction throughput.
    pub compute_bound_seconds: f64,
    /// Workload-selected denominator family.
    pub binding: BindingBound,
    /// Binding-bound time divided by observed time.
    pub achieved_percent: f64,
    /// Staleness diagnostic, loud when the score exceeds 100%.
    pub diagnostic: RooflineDiagnostic,
    /// Provenance for the reported memory bound.
    pub memory_provenance: DenominatorProvenance,
    /// Provenance for the reported compute bound.
    pub compute_provenance: DenominatorProvenance,
}

impl RooflineScore {
    /// Produces the mandatory human-readable staleness warning.
    #[must_use]
    pub fn loud_message(&self) -> String {
        match self.diagnostic {
            RooflineDiagnostic::WithinDenominator => format!(
                "roofline {:.3}% ({:?} binding)",
                self.achieved_percent, self.binding
            ),
            RooflineDiagnostic::DenominatorStale {
                achieved_percent,
                binding,
            } => format!(
                "DENOMINATOR STALE: achieved {achieved_percent:.3}% of {binding:?} bound; re-measure and revise upward before interpreting this run"
            ),
        }
    }
}

/// Typed roofline model error.
#[derive(Clone, Debug, PartialEq)]
pub enum RooflineError {
    /// A rate or elapsed time was zero, negative, NaN, or infinite.
    InvalidPositiveValue(f64),
    /// A memory ceiling was requested for zero workers.
    ZeroCoreCount,
    /// Required provenance was blank.
    MissingProvenance,
    /// The requested core count has no measured memory denominator.
    MissingMemoryCeiling {
        /// Requested worker count.
        cores: usize,
    },
    /// The requested instruction tier has no measured compute denominator.
    MissingComputeCeiling {
        /// Requested tier.
        tier: ComputeTier,
    },
    /// Denominators may only be revised upward.
    DownwardRevision {
        /// Previously adopted rate.
        previous: f64,
        /// Rejected proposed rate.
        proposed: f64,
    },
}

impl fmt::Display for RooflineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPositiveValue(value) => {
                write!(formatter, "expected a positive finite value, got {value}")
            }
            Self::ZeroCoreCount => formatter.write_str("memory ceiling requires at least one core"),
            Self::MissingProvenance => {
                formatter.write_str("denominator provenance fields must not be blank")
            }
            Self::MissingMemoryCeiling { cores } => {
                write!(formatter, "no measured memory ceiling for {cores} core(s)")
            }
            Self::MissingComputeCeiling { tier } => {
                write!(formatter, "no measured compute ceiling for {tier:?}")
            }
            Self::DownwardRevision { previous, proposed } => write!(
                formatter,
                "denominator revisions are upward-only: {proposed} is below {previous}"
            ),
        }
    }
}

impl std::error::Error for RooflineError {}

/// Typed failure while installing a persisted calibration into a roofline model.
#[derive(Debug)]
pub enum RooflineLoadError {
    /// Calibration artifact loading or validation failed.
    Calibration(CalibrationError),
    /// A loaded denominator violated the roofline model invariants.
    Roofline(RooflineError),
}

impl fmt::Display for RooflineLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Calibration(error) => write!(formatter, "persisted calibration failed: {error}"),
            Self::Roofline(error) => write!(formatter, "persisted roofline failed: {error}"),
        }
    }
}

impl std::error::Error for RooflineLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Calibration(error) => Some(error),
            Self::Roofline(error) => Some(error),
        }
    }
}

fn validate_positive(value: f64) -> Result<(), RooflineError> {
    if value.is_finite() && value > 0.0 {
        Ok(())
    } else {
        Err(RooflineError::InvalidPositiveValue(value))
    }
}

/// Saturation-loop configuration for measured compute denominators.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ComputeCalibrationConfig {
    /// Loop iterations in each timed sample.
    pub iterations_per_sample: u64,
    /// Shared N>=30, <=2% RSD measurement discipline.
    pub measurement: MeasurementConfig,
}

impl ComputeCalibrationConfig {
    /// Evidence-sized saturation loops and strict statistics.
    #[must_use]
    pub const fn evidence() -> Self {
        Self {
            iterations_per_sample: 5_000_000,
            measurement: MeasurementConfig::strict(),
        }
    }
}

/// One truly measured sustained compute denominator.
#[derive(Clone, Debug, PartialEq)]
pub struct ComputeCalibration {
    /// Measured instruction path.
    pub tier: ComputeTier,
    /// Sustained multiply-accumulates per second.
    pub operations_per_second: f64,
    /// Operations issued in every sample.
    pub operations_per_sample: u64,
    /// Accepted raw statistical result.
    pub measurement: MeasurementResult,
    /// Output checksum preventing loop elimination.
    pub checksum: u64,
}

/// Preflight-aware calibration result.
#[derive(Clone, Debug, PartialEq)]
pub enum ComputeCalibrationOutcome {
    /// Calibration idled and collected no timings.
    Idle {
        /// Fail-closed preflight reasons.
        reasons: Vec<String>,
    },
    /// Every locally executable requested tier was measured.
    Measured {
        /// Real measured rates.
        calibrations: Vec<ComputeCalibration>,
        /// Unsupported tiers explicitly not measured.
        not_measured: Vec<(ComputeTier, String)>,
    },
}

/// Typed saturation-calibration failure after a successful preflight.
#[derive(Debug)]
pub enum ComputeCalibrationError {
    /// Iteration count was zero or overflowed its operation count.
    InvalidIterations,
    /// Strict statistical measurement did not complete.
    Measurement(MeasurementError<std::io::Error>),
}

impl fmt::Display for ComputeCalibrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIterations => {
                formatter.write_str("compute calibration iterations must be nonzero and bounded")
            }
            Self::Measurement(error) => write!(formatter, "compute calibration failed: {error}"),
        }
    }
}

impl std::error::Error for ComputeCalibrationError {}

/// Measures NEON FMA, SDOT, and FP16 conversion/FMA saturation paths.
///
/// The preflight is inside this API: callers cannot accidentally turn an
/// unreadable thermal state into a timing. In that case the result is `Idle`
/// and contains no measured or estimated denominator.
pub fn calibrate_compute_tiers(
    probe: &impl MachineProbe,
    config: ComputeCalibrationConfig,
) -> Result<ComputeCalibrationOutcome, ComputeCalibrationError> {
    if config.iterations_per_sample == 0 {
        return Err(ComputeCalibrationError::InvalidIterations);
    }
    match preflight(probe) {
        PreflightOutcome::Ready { .. } => {}
        PreflightOutcome::Idle { reasons } => {
            return Ok(ComputeCalibrationOutcome::Idle { reasons });
        }
    }
    calibrate_compute_tiers_ready(config, MachineStateProvenance::DirectProbe, probe)
}

/// Measures compute ceilings after the unchanged direct probe, using a valid
/// operator attestation only when that probe is unavailable.
pub fn calibrate_compute_tiers_with_attestation(
    probe: &impl MachineProbe,
    attestation: &impl AttestationSource,
    config: ComputeCalibrationConfig,
) -> Result<ComputeCalibrationOutcome, ComputeCalibrationError> {
    if config.iterations_per_sample == 0 {
        return Err(ComputeCalibrationError::InvalidIterations);
    }
    let provenance = match preflight_with_attestation(probe, attestation) {
        CampaignPreflightOutcome::Ready { provenance, .. } => provenance,
        CampaignPreflightOutcome::Idle { reasons } => {
            return Ok(ComputeCalibrationOutcome::Idle { reasons });
        }
    };
    calibrate_compute_tiers_ready(config, provenance, probe)
}

fn calibrate_compute_tiers_ready(
    config: ComputeCalibrationConfig,
    provenance: MachineStateProvenance,
    probe: &impl MachineProbe,
) -> Result<ComputeCalibrationOutcome, ComputeCalibrationError> {
    #[cfg(target_arch = "aarch64")]
    {
        calibrate_aarch64(config, provenance, probe)
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        let _ = config;
        let _ = provenance;
        let _ = probe;
        Ok(ComputeCalibrationOutcome::Measured {
            calibrations: Vec::new(),
            not_measured: vec![
                (
                    ComputeTier::NeonFma,
                    "NOT MEASURED: requires AArch64 NEON".to_owned(),
                ),
                (
                    ComputeTier::NeonSdot,
                    "NOT MEASURED: requires AArch64 FEAT_DotProd".to_owned(),
                ),
                (
                    ComputeTier::NeonFp16ConvertFma,
                    "NOT MEASURED: requires AArch64 FP16".to_owned(),
                ),
            ],
        })
    }
}

#[cfg(target_arch = "aarch64")]
fn calibrate_aarch64(
    config: ComputeCalibrationConfig,
    machine_state: MachineStateProvenance,
    probe: &impl MachineProbe,
) -> Result<ComputeCalibrationOutcome, ComputeCalibrationError> {
    let features = zeppelin_embed::kernels::detected_features();
    let mut calibrations = Vec::new();
    let mut not_measured = Vec::new();
    for (tier, supported, operations_per_iteration) in [
        (ComputeTier::NeonFma, features.neon, 8_u64 * 4),
        (ComputeTier::NeonSdot, features.dotprod, 8_u64 * 16),
        (ComputeTier::NeonFp16ConvertFma, features.fp16, 8_u64 * 4),
    ] {
        if !supported {
            not_measured.push((
                tier,
                format!(
                    "NOT MEASURED: runtime feature for {} is absent",
                    tier.as_str()
                ),
            ));
            continue;
        }
        let operations_per_sample = config
            .iterations_per_sample
            .checked_mul(operations_per_iteration)
            .ok_or(ComputeCalibrationError::InvalidIterations)?;
        let mut source = SaturationSource {
            tier,
            iterations: config.iterations_per_sample,
            checksum: 0,
            load_probe: probe,
        };
        let measurement =
            measure_source_with_provenance(&mut source, config.measurement, machine_state.clone())
                .map_err(ComputeCalibrationError::Measurement)?;
        let seconds = measurement.min_of_medians_ns / 1_000_000_000.0;
        calibrations.push(ComputeCalibration {
            tier,
            operations_per_second: operations_per_sample as f64 / seconds,
            operations_per_sample,
            measurement,
            checksum: source.checksum,
        });
    }
    Ok(ComputeCalibrationOutcome::Measured {
        calibrations,
        not_measured,
    })
}

#[cfg(target_arch = "aarch64")]
struct SaturationSource<'a, P> {
    tier: ComputeTier,
    iterations: u64,
    checksum: u64,
    load_probe: &'a P,
}

#[cfg(target_arch = "aarch64")]
impl<P: MachineProbe> SampleSource for SaturationSource<'_, P> {
    type Error = std::io::Error;

    fn warm_up(&mut self) -> Result<(), Self::Error> {
        self.checksum ^= run_saturation(self.tier, self.iterations);
        Ok(())
    }

    fn sample_ns(&mut self) -> Result<f64, Self::Error> {
        let started = std::time::Instant::now();
        self.checksum ^= run_saturation(self.tier, self.iterations);
        Ok(started.elapsed().as_secs_f64() * 1_000_000_000.0)
    }

    fn concurrent_load_check(&mut self) -> Result<(), String> {
        self.load_probe.concurrent_load_check()
    }
}

#[cfg(target_arch = "aarch64")]
fn run_saturation(tier: ComputeTier, iterations: u64) -> u64 {
    match tier {
        ComputeTier::NeonFma => {
            // SAFETY: AArch64 user space has NEON and the caller checked it.
            unsafe { neon_fma_saturation(iterations) }
        }
        ComputeTier::NeonSdot => {
            // SAFETY: runtime feature detection established FEAT_DotProd.
            unsafe { neon_sdot_saturation(iterations) }
        }
        ComputeTier::NeonFp16ConvertFma => {
            // SAFETY: runtime feature detection established FP16 conversion.
            unsafe { neon_fp16_path_saturation(iterations) }
        }
    }
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn neon_fma_saturation(iterations: u64) -> u64 {
    use std::arch::aarch64::{vaddq_f32, vaddvq_f32, vdupq_n_f32, vfmaq_f32};
    let left = std::hint::black_box(vdupq_n_f32(1.000_1));
    let right = std::hint::black_box(vdupq_n_f32(0.999_9));
    let mut acc0 = vdupq_n_f32(0.0);
    let mut acc1 = vdupq_n_f32(1.0);
    let mut acc2 = vdupq_n_f32(2.0);
    let mut acc3 = vdupq_n_f32(3.0);
    let mut acc4 = vdupq_n_f32(4.0);
    let mut acc5 = vdupq_n_f32(5.0);
    let mut acc6 = vdupq_n_f32(6.0);
    let mut acc7 = vdupq_n_f32(7.0);
    for _ in 0..std::hint::black_box(iterations) {
        acc0 = vfmaq_f32(acc0, left, right);
        acc1 = vfmaq_f32(acc1, left, right);
        acc2 = vfmaq_f32(acc2, left, right);
        acc3 = vfmaq_f32(acc3, left, right);
        acc4 = vfmaq_f32(acc4, left, right);
        acc5 = vfmaq_f32(acc5, left, right);
        acc6 = vfmaq_f32(acc6, left, right);
        acc7 = vfmaq_f32(acc7, left, right);
    }
    let low = vaddq_f32(vaddq_f32(acc0, acc1), vaddq_f32(acc2, acc3));
    let high = vaddq_f32(vaddq_f32(acc4, acc5), vaddq_f32(acc6, acc7));
    u64::from(std::hint::black_box(vaddvq_f32(vaddq_f32(low, high))).to_bits())
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "dotprod")]
unsafe fn neon_sdot_saturation(iterations: u64) -> u64 {
    use std::arch::aarch64::{int32x4_t, vaddq_s32, vaddvq_s32, vdupq_n_s8, vdupq_n_s32};
    use std::arch::asm;
    let left = std::hint::black_box(vdupq_n_s8(1));
    let right = std::hint::black_box(vdupq_n_s8(1));
    let mut acc0: int32x4_t = vdupq_n_s32(0);
    let mut acc1: int32x4_t = vdupq_n_s32(1);
    let mut acc2: int32x4_t = vdupq_n_s32(2);
    let mut acc3: int32x4_t = vdupq_n_s32(3);
    let mut acc4: int32x4_t = vdupq_n_s32(4);
    let mut acc5: int32x4_t = vdupq_n_s32(5);
    let mut acc6: int32x4_t = vdupq_n_s32(6);
    let mut acc7: int32x4_t = vdupq_n_s32(7);
    for _ in 0..std::hint::black_box(iterations) {
        // SAFETY: FEAT_DotProd was checked, operands are initialized vector
        // registers, and the eight instructions touch no memory.
        unsafe {
            asm!(
                "sdot {acc0:v}.4s, {left:v}.16b, {right:v}.16b",
                "sdot {acc1:v}.4s, {left:v}.16b, {right:v}.16b",
                "sdot {acc2:v}.4s, {left:v}.16b, {right:v}.16b",
                "sdot {acc3:v}.4s, {left:v}.16b, {right:v}.16b",
                "sdot {acc4:v}.4s, {left:v}.16b, {right:v}.16b",
                "sdot {acc5:v}.4s, {left:v}.16b, {right:v}.16b",
                "sdot {acc6:v}.4s, {left:v}.16b, {right:v}.16b",
                "sdot {acc7:v}.4s, {left:v}.16b, {right:v}.16b",
                acc0 = inout(vreg) acc0,
                acc1 = inout(vreg) acc1,
                acc2 = inout(vreg) acc2,
                acc3 = inout(vreg) acc3,
                acc4 = inout(vreg) acc4,
                acc5 = inout(vreg) acc5,
                acc6 = inout(vreg) acc6,
                acc7 = inout(vreg) acc7,
                left = in(vreg) left,
                right = in(vreg) right,
                options(nomem, nostack)
            );
        }
    }
    let low = vaddq_s32(vaddq_s32(acc0, acc1), vaddq_s32(acc2, acc3));
    let high = vaddq_s32(vaddq_s32(acc4, acc5), vaddq_s32(acc6, acc7));
    std::hint::black_box(vaddvq_s32(vaddq_s32(low, high))) as u32 as u64
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon,fp16")]
unsafe fn neon_fp16_path_saturation(iterations: u64) -> u64 {
    use std::arch::aarch64::{vaddq_f32, vaddvq_f32, vdup_n_u16, vdupq_n_f32};
    let left = std::hint::black_box(vdup_n_u16(0x3c00));
    let right = std::hint::black_box(vdup_n_u16(0x3800));
    let mut acc0 = vdupq_n_f32(0.0);
    let mut acc1 = vdupq_n_f32(1.0);
    let mut acc2 = vdupq_n_f32(2.0);
    let mut acc3 = vdupq_n_f32(3.0);
    let mut acc4 = vdupq_n_f32(4.0);
    let mut acc5 = vdupq_n_f32(5.0);
    let mut acc6 = vdupq_n_f32(6.0);
    let mut acc7 = vdupq_n_f32(7.0);
    for _ in 0..std::hint::black_box(iterations) {
        // SAFETY: the enclosing function is runtime-gated for FP16 and all
        // values are initialized vector registers.
        unsafe {
            acc0 = fp16_convert_fma(acc0, left, right);
            acc1 = fp16_convert_fma(acc1, left, right);
            acc2 = fp16_convert_fma(acc2, left, right);
            acc3 = fp16_convert_fma(acc3, left, right);
            acc4 = fp16_convert_fma(acc4, left, right);
            acc5 = fp16_convert_fma(acc5, left, right);
            acc6 = fp16_convert_fma(acc6, left, right);
            acc7 = fp16_convert_fma(acc7, left, right);
        }
    }
    let low = vaddq_f32(vaddq_f32(acc0, acc1), vaddq_f32(acc2, acc3));
    let high = vaddq_f32(vaddq_f32(acc4, acc5), vaddq_f32(acc6, acc7));
    u64::from(std::hint::black_box(vaddvq_f32(vaddq_f32(low, high))).to_bits())
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon,fp16")]
unsafe fn fp16_convert_fma(
    accumulator: std::arch::aarch64::float32x4_t,
    left: std::arch::aarch64::uint16x4_t,
    right: std::arch::aarch64::uint16x4_t,
) -> std::arch::aarch64::float32x4_t {
    use std::arch::aarch64::{float32x4_t, vfmaq_f32};
    use std::arch::asm;
    let left_f32: float32x4_t;
    let right_f32: float32x4_t;
    // SAFETY: runtime FP16 detection permits FCVTL; the instruction touches
    // no memory. Omitting `pure` keeps each conversion in the saturation path.
    unsafe {
        asm!(
            "fcvtl {left_f32:v}.4s, {left:v}.4h",
            "fcvtl {right_f32:v}.4s, {right:v}.4h",
            left_f32 = out(vreg) left_f32,
            right_f32 = out(vreg) right_f32,
            left = in(vreg) left,
            right = in(vreg) right,
            options(nomem, nostack)
        );
    }
    vfmaq_f32(accumulator, left_f32, right_f32)
}
