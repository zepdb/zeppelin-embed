//! Fail-closed machine preflight, disciplined sampling, and workloads.

use std::fmt;
use std::hint::black_box;
use std::io;
use std::process::Command;
use std::time::Instant;

use zeppelin_embed::kernels::KernelVariant;

use super::attestation::MachineStateProvenance;
use super::roofline::{BindingBound, ComputeTier};
use super::variants::RegisteredVariant;

/// Required measurement configuration.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeasurementConfig {
    /// Untimed executions before collection.
    pub warmup_repetitions: usize,
    /// Samples in each independently variance-checked run; must be at least 30.
    pub repetitions_per_run: usize,
    /// Low-variance run medians required for the final min-of-medians.
    pub accepted_runs: usize,
    /// Total run attempts before returning a typed failure.
    pub maximum_attempts: usize,
    /// Maximum accepted relative standard deviation in percent; at most 2%.
    pub maximum_rsd_percent: f64,
}

impl MeasurementConfig {
    /// Strict default used by frontier measurements.
    #[must_use]
    pub const fn strict() -> Self {
        Self {
            warmup_repetitions: 5,
            repetitions_per_run: 30,
            accepted_runs: 3,
            maximum_attempts: 12,
            maximum_rsd_percent: 2.0,
        }
    }
}

/// Source of warmed, independently timed samples.
pub trait SampleSource {
    /// Typed sampling failure.
    type Error;

    /// Executes one untimed warm-up operation.
    fn warm_up(&mut self) -> Result<(), Self::Error>;

    /// Returns one elapsed observation in nanoseconds.
    fn sample_ns(&mut self) -> Result<f64, Self::Error>;
}

/// Accepted low-variance runs and the final min-of-medians statistic.
#[derive(Clone, Debug, PartialEq)]
pub struct MeasurementResult {
    /// Median from every accepted run.
    pub accepted_run_medians_ns: Vec<f64>,
    /// Relative standard deviation from every accepted run.
    pub accepted_run_rsd_percent: Vec<f64>,
    /// Number of high-variance runs discarded in full.
    pub discarded_runs: usize,
    /// Minimum accepted run median.
    pub min_of_medians_ns: f64,
    /// How safe machine state was established before this measurement.
    pub machine_state: MachineStateProvenance,
}

/// Typed statistical measurement failure.
#[derive(Debug)]
pub enum MeasurementError<E> {
    /// The requested statistical policy weakens a fixed harness invariant.
    InvalidConfiguration(&'static str),
    /// The source could not produce a sample.
    Source(E),
    /// A source returned a non-positive or non-finite duration.
    InvalidSample(f64),
    /// Too many entire runs exceeded the variance cap.
    VarianceBudgetExhausted {
        /// Discarded run count.
        discarded_runs: usize,
        /// Required accepted run count.
        accepted_runs_required: usize,
    },
}

impl<E: fmt::Display> fmt::Display for MeasurementError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration(reason) => formatter.write_str(reason),
            Self::Source(error) => write!(formatter, "measurement source failed: {error}"),
            Self::InvalidSample(value) => {
                write!(formatter, "measurement returned invalid {value} ns sample")
            }
            Self::VarianceBudgetExhausted {
                discarded_runs,
                accepted_runs_required,
            } => write!(
                formatter,
                "discarded {discarded_runs} noisy runs before collecting {accepted_runs_required} accepted runs"
            ),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for MeasurementError<E> {}

/// Measures a source with whole-run variance rejection and min-of-medians.
pub fn measure_source<S: SampleSource>(
    source: &mut S,
    config: MeasurementConfig,
) -> Result<MeasurementResult, MeasurementError<S::Error>> {
    measure_source_with_provenance(source, config, MachineStateProvenance::DirectProbe)
}

/// Measures a source while carrying its established machine-state provenance.
pub fn measure_source_with_provenance<S: SampleSource>(
    source: &mut S,
    config: MeasurementConfig,
    machine_state: MachineStateProvenance,
) -> Result<MeasurementResult, MeasurementError<S::Error>> {
    validate_measurement_config(config)?;
    for _ in 0..config.warmup_repetitions {
        source.warm_up().map_err(MeasurementError::Source)?;
    }
    let mut accepted_run_medians_ns = Vec::with_capacity(config.accepted_runs);
    let mut accepted_run_rsd_percent = Vec::with_capacity(config.accepted_runs);
    let mut discarded_runs = 0;
    for _ in 0..config.maximum_attempts {
        let mut samples = Vec::with_capacity(config.repetitions_per_run);
        for _ in 0..config.repetitions_per_run {
            let sample = source.sample_ns().map_err(MeasurementError::Source)?;
            if !sample.is_finite() || sample <= 0.0 {
                return Err(MeasurementError::InvalidSample(sample));
            }
            samples.push(sample);
        }
        let rsd_percent = relative_standard_deviation_percent(&samples);
        if rsd_percent > config.maximum_rsd_percent {
            discarded_runs += 1;
            continue;
        }
        samples.sort_by(f64::total_cmp);
        let median = median_of_sorted(&samples);
        accepted_run_medians_ns.push(median);
        accepted_run_rsd_percent.push(rsd_percent);
        if accepted_run_medians_ns.len() == config.accepted_runs {
            let min_of_medians_ns = accepted_run_medians_ns
                .iter()
                .copied()
                .min_by(f64::total_cmp)
                .ok_or(MeasurementError::InvalidConfiguration(
                    "accepted run count unexpectedly became empty",
                ))?;
            return Ok(MeasurementResult {
                accepted_run_medians_ns,
                accepted_run_rsd_percent,
                discarded_runs,
                min_of_medians_ns,
                machine_state,
            });
        }
    }
    Err(MeasurementError::VarianceBudgetExhausted {
        discarded_runs,
        accepted_runs_required: config.accepted_runs,
    })
}

fn validate_measurement_config<E>(config: MeasurementConfig) -> Result<(), MeasurementError<E>> {
    if config.repetitions_per_run < 30 {
        return Err(MeasurementError::InvalidConfiguration(
            "frontier measurement requires at least 30 repetitions per run",
        ));
    }
    if config.accepted_runs == 0 || config.maximum_attempts < config.accepted_runs {
        return Err(MeasurementError::InvalidConfiguration(
            "frontier measurement needs accepted runs and enough attempts",
        ));
    }
    if !config.maximum_rsd_percent.is_finite()
        || config.maximum_rsd_percent <= 0.0
        || config.maximum_rsd_percent > 2.0
    {
        return Err(MeasurementError::InvalidConfiguration(
            "frontier measurement RSD cap must be positive and no greater than 2%",
        ));
    }
    Ok(())
}

fn relative_standard_deviation_percent(samples: &[f64]) -> f64 {
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    if mean == 0.0 {
        return f64::INFINITY;
    }
    let variance = samples
        .iter()
        .map(|sample| {
            let difference = sample - mean;
            difference * difference
        })
        .sum::<f64>()
        / samples.len() as f64;
    variance.sqrt() / mean * 100.0
}

fn median_of_sorted(samples: &[f64]) -> f64 {
    let middle = samples.len() / 2;
    if samples.len().is_multiple_of(2) {
        (samples[middle - 1] + samples[middle]) / 2.0
    } else {
        samples[middle]
    }
}

/// Captured result of one `pmset` command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProbeOutput {
    /// Whether the command exited successfully.
    pub success: bool,
    /// Standard output.
    pub stdout: String,
    /// Standard error.
    pub stderr: String,
}

impl ProbeOutput {
    /// Creates a successful mocked or captured command result.
    #[must_use]
    pub fn success(stdout: impl Into<String>) -> Self {
        Self {
            success: true,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    /// Creates a failed mocked or captured command result.
    #[must_use]
    pub fn failure(stderr: impl Into<String>) -> Self {
        Self {
            success: false,
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }
}

/// Injectable machine-state seam used by the fail-closed preflight.
pub trait MachineProbe {
    /// Runs `pmset` with the supplied arguments.
    fn pmset(&self, arguments: &[&str]) -> io::Result<ProbeOutput>;
}

/// Real process-based `pmset` probe.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemMachineProbe;

impl MachineProbe for SystemMachineProbe {
    fn pmset(&self, arguments: &[&str]) -> io::Result<ProbeOutput> {
        let output = Command::new("pmset").args(arguments).output()?;
        Ok(ProbeOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Result of the mandatory AC-power and thermal preflight.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PreflightOutcome {
    /// AC power and nominal thermal state were both established.
    Ready {
        /// Literal power-source evidence.
        power_evidence: String,
        /// Literal thermal evidence.
        thermal_evidence: String,
    },
    /// Measurement must idle; no timing may be collected.
    Idle {
        /// Every state that could not be proven safe.
        reasons: Vec<String>,
    },
}

/// Checks AC power and thermal state, failing closed on ambiguity or errors.
#[must_use]
pub fn preflight(probe: &impl MachineProbe) -> PreflightOutcome {
    let mut reasons = Vec::new();
    let power = probe.pmset(&["-g", "ps"]);
    let thermal = probe.pmset(&["-g", "therm"]);
    let power_evidence = match power {
        Ok(output) if output.success && explicitly_on_ac_power(&output.stdout) => {
            Some(output.stdout)
        }
        Ok(output) => {
            reasons.push(format!(
                "AC power not established by pmset -g ps: {}{}",
                output.stdout.trim(),
                output.stderr.trim()
            ));
            None
        }
        Err(error) => {
            reasons.push(format!("AC power probe failed: {error}"));
            None
        }
    };
    let thermal_evidence = match thermal {
        Ok(output) if output.success && explicitly_thermal_nominal(&output.stdout) => {
            Some(output.stdout)
        }
        Ok(output) => {
            reasons.push(format!(
                "nominal thermal state not established by pmset -g therm: {}{}",
                output.stdout.trim(),
                output.stderr.trim()
            ));
            None
        }
        Err(error) => {
            reasons.push(format!("thermal probe failed: {error}"));
            None
        }
    };
    match (power_evidence, thermal_evidence) {
        (Some(power_evidence), Some(thermal_evidence)) if reasons.is_empty() => {
            PreflightOutcome::Ready {
                power_evidence,
                thermal_evidence,
            }
        }
        _ => PreflightOutcome::Idle { reasons },
    }
}

pub(crate) fn explicitly_on_ac_power(output: &str) -> bool {
    let lowercase = output.to_ascii_lowercase();
    lowercase.contains("drawing from 'ac power'")
        || (lowercase.contains("ac attached") && !lowercase.contains("battery power"))
}

pub(crate) fn explicitly_thermal_nominal(output: &str) -> bool {
    let lowercase = output.to_ascii_lowercase();
    let no_warnings =
        lowercase.contains("no thermal warning") && lowercase.contains("no performance warning");
    let explicit_zero = (lowercase.contains("thermal warning level = 0")
        || lowercase.contains("thermal_warning_level = 0"))
        && (lowercase.contains("performance warning level = 0")
            || lowercase.contains("performance_warning_level = 0"));
    let speed_unconstrained =
        !lowercase.contains("cpu_speed_limit") || lowercase.contains("cpu_speed_limit = 100");
    (no_warnings || explicit_zero) && speed_unconstrained
}

/// Workload metadata carried into every measurement and ledger row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkloadDescriptor {
    /// Stable workload name.
    pub name: String,
    /// Whether this workload lacks real production data/access provenance.
    pub provisional: bool,
    /// Workload-selected binding denominator.
    pub binding: BindingBound,
    /// Compute path used when computing the companion compute bound.
    pub compute_tier: ComputeTier,
    /// Bytes touched per execution.
    pub bytes_touched: u64,
    /// Multiply-accumulate operations per execution.
    pub operation_count: u64,
}

/// One workload execution observed by the harness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkloadObservation {
    /// Whether all results matched the scalar oracle.
    pub correct: bool,
    /// Output checksum retaining all scored rows.
    pub checksum: i64,
    /// Access-order fingerprint distinguishing contiguous and gathered calls.
    pub access_fingerprint: u64,
}

/// Pluggable workload seam for synthetic and future real datasets.
pub trait Workload {
    /// Typed workload construction/execution failure.
    type Error;

    /// Returns workload identity, provenance, work, and bound selection.
    fn descriptor(&self) -> &WorkloadDescriptor;

    /// Executes one full workload against a concrete registered variant.
    fn execute(&mut self, variant: &RegisteredVariant) -> Result<WorkloadObservation, Self::Error>;

    /// Executes the timed hot path without running the scalar oracle inside
    /// the measurement interval. Callers verify through [`Workload::execute`]
    /// before constructing a sampler.
    fn execute_timed(
        &mut self,
        variant: &RegisteredVariant,
    ) -> Result<WorkloadObservation, Self::Error> {
        self.execute(variant)
    }
}

/// Invalid or unexecutable i8 frontier workload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkloadError {
    /// Rows, dimensions, names, or stride did not define a workload.
    InvalidShape,
    /// Derived fixture allocation size overflowed.
    SizeOverflow,
}

impl fmt::Display for WorkloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidShape => formatter.write_str("workload shape and name must be nonzero"),
            Self::SizeOverflow => formatter.write_str("workload fixture size overflowed"),
        }
    }
}

impl std::error::Error for WorkloadError {}

/// Deterministic contiguous row-major synthetic i8 workload.
#[derive(Clone, Debug)]
pub struct SyntheticI8Workload {
    descriptor: WorkloadDescriptor,
    dimension: usize,
    query: Vec<i8>,
    rows: Vec<i8>,
}

impl SyntheticI8Workload {
    /// Builds a seeded contiguous synthetic fixture.
    pub fn new(
        name: impl Into<String>,
        row_count: usize,
        dimension: usize,
        seed: u64,
    ) -> Result<Self, WorkloadError> {
        let name = name.into();
        if name.trim().is_empty() || row_count == 0 || dimension == 0 {
            return Err(WorkloadError::InvalidShape);
        }
        let row_values = row_count
            .checked_mul(dimension)
            .ok_or(WorkloadError::SizeOverflow)?;
        let mut rng = SeededXorShift::new(seed);
        let query = generated_i8(&mut rng, dimension);
        let rows = generated_i8(&mut rng, row_values);
        Ok(Self {
            descriptor: WorkloadDescriptor {
                name,
                provisional: true,
                binding: BindingBound::Memory,
                compute_tier: ComputeTier::NeonSdot,
                bytes_touched: u64::try_from(row_values).unwrap_or(u64::MAX),
                operation_count: u64::try_from(row_values).unwrap_or(u64::MAX),
            },
            dimension,
            query,
            rows,
        })
    }
}

impl Workload for SyntheticI8Workload {
    type Error = WorkloadError;

    fn descriptor(&self) -> &WorkloadDescriptor {
        &self.descriptor
    }

    fn execute(&mut self, variant: &RegisteredVariant) -> Result<WorkloadObservation, Self::Error> {
        execute_rows(
            variant,
            &self.query,
            &self.rows,
            self.dimension,
            1,
            self.rows.len() / self.dimension,
            true,
        )
    }

    fn execute_timed(
        &mut self,
        variant: &RegisteredVariant,
    ) -> Result<WorkloadObservation, Self::Error> {
        execute_rows(
            variant,
            &self.query,
            &self.rows,
            self.dimension,
            1,
            self.rows.len() / self.dimension,
            false,
        )
    }
}

/// Deterministic synthetic i8 workload with gathered, non-contiguous rows.
#[derive(Clone, Debug)]
pub struct StridedI8Workload {
    descriptor: WorkloadDescriptor,
    dimension: usize,
    stride: usize,
    logical_rows: usize,
    query: Vec<i8>,
    rows: Vec<i8>,
}

impl StridedI8Workload {
    /// Builds a seeded fixture whose candidate walk skips physical rows.
    pub fn new(
        name: impl Into<String>,
        logical_rows: usize,
        dimension: usize,
        stride: usize,
        seed: u64,
    ) -> Result<Self, WorkloadError> {
        let name = name.into();
        if name.trim().is_empty() || logical_rows == 0 || dimension == 0 || stride < 2 {
            return Err(WorkloadError::InvalidShape);
        }
        let physical_rows = logical_rows
            .checked_mul(stride)
            .ok_or(WorkloadError::SizeOverflow)?;
        let row_values = physical_rows
            .checked_mul(dimension)
            .ok_or(WorkloadError::SizeOverflow)?;
        let touched_values = logical_rows
            .checked_mul(dimension)
            .ok_or(WorkloadError::SizeOverflow)?;
        let mut rng = SeededXorShift::new(seed);
        let query = generated_i8(&mut rng, dimension);
        let rows = generated_i8(&mut rng, row_values);
        Ok(Self {
            descriptor: WorkloadDescriptor {
                name,
                provisional: true,
                binding: BindingBound::Memory,
                compute_tier: ComputeTier::NeonSdot,
                bytes_touched: u64::try_from(touched_values).unwrap_or(u64::MAX),
                operation_count: u64::try_from(touched_values).unwrap_or(u64::MAX),
            },
            dimension,
            stride,
            logical_rows,
            query,
            rows,
        })
    }
}

impl Workload for StridedI8Workload {
    type Error = WorkloadError;

    fn descriptor(&self) -> &WorkloadDescriptor {
        &self.descriptor
    }

    fn execute(&mut self, variant: &RegisteredVariant) -> Result<WorkloadObservation, Self::Error> {
        execute_rows(
            variant,
            &self.query,
            &self.rows,
            self.dimension,
            self.stride,
            self.logical_rows,
            true,
        )
    }

    fn execute_timed(
        &mut self,
        variant: &RegisteredVariant,
    ) -> Result<WorkloadObservation, Self::Error> {
        execute_rows(
            variant,
            &self.query,
            &self.rows,
            self.dimension,
            self.stride,
            self.logical_rows,
            false,
        )
    }
}

/// Adapts a pluggable workload into the statistical sampling seam.
pub struct WorkloadSampler<'a, W> {
    workload: &'a mut W,
    variant: &'a RegisteredVariant,
}

impl<'a, W> WorkloadSampler<'a, W> {
    /// Binds one workload to one concrete variant.
    #[must_use]
    pub const fn new(workload: &'a mut W, variant: &'a RegisteredVariant) -> Self {
        Self { workload, variant }
    }
}

impl<W: Workload> SampleSource for WorkloadSampler<'_, W> {
    type Error = W::Error;

    fn warm_up(&mut self) -> Result<(), Self::Error> {
        black_box(self.workload.execute_timed(self.variant)?);
        Ok(())
    }

    fn sample_ns(&mut self) -> Result<f64, Self::Error> {
        let started = Instant::now();
        black_box(self.workload.execute_timed(self.variant)?);
        Ok(started.elapsed().as_secs_f64() * 1_000_000_000.0)
    }
}

fn execute_rows(
    variant: &RegisteredVariant,
    query: &[i8],
    rows: &[i8],
    dimension: usize,
    stride: usize,
    logical_rows: usize,
    verify_oracle: bool,
) -> Result<WorkloadObservation, WorkloadError> {
    let scalar = KernelVariant::scalar();
    let mut checksum = 0_i64;
    let mut correct = true;
    let mut access_fingerprint = 0xcbf2_9ce4_8422_2325_u64;
    for logical_index in 0..logical_rows {
        let physical_index = logical_index
            .checked_mul(stride)
            .ok_or(WorkloadError::SizeOverflow)?;
        let start = physical_index
            .checked_mul(dimension)
            .ok_or(WorkloadError::SizeOverflow)?;
        let end = start
            .checked_add(dimension)
            .ok_or(WorkloadError::SizeOverflow)?;
        let row = rows.get(start..end).ok_or(WorkloadError::InvalidShape)?;
        let observed = variant.dot_i8(query, row);
        let expected = if verify_oracle {
            scalar.dot_i8(query, row)
        } else {
            observed
        };
        checksum = checksum.wrapping_add(i64::from(observed));
        correct &= observed == expected;
        access_fingerprint ^= physical_index as u64;
        access_fingerprint = access_fingerprint.wrapping_mul(0x100_0000_01b3);
    }
    black_box(checksum);
    Ok(WorkloadObservation {
        correct,
        checksum,
        access_fingerprint,
    })
}

fn generated_i8(rng: &mut SeededXorShift, length: usize) -> Vec<i8> {
    (0..length)
        .map(|_| rng.next_u64().to_le_bytes()[0] as i8)
        .collect()
}

struct SeededXorShift {
    state: u64,
}

impl SeededXorShift {
    fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x9e37_79b9_7f4a_7c15
            } else {
                seed
            },
        }
    }

    fn next_u64(&mut self) -> u64 {
        let mut value = self.state;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.state = value;
        value
    }
}
