//! Package-energy measurement through privileged `powermetrics` sampling.

use std::fmt;
use std::io;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Package-energy result integrated over one `powermetrics` sampling interval.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EnergyMeasurement {
    /// Average combined CPU, GPU, and ANE package power in watts.
    pub average_package_watts: f64,
    /// Sampling interval in seconds.
    pub sample_seconds: f64,
    /// Integrated package energy in joules.
    pub package_joules: f64,
}

/// Typed energy-harness failure.
#[derive(Debug)]
pub enum EnergyError {
    /// `powermetrics` cannot be used without an operator-controlled root invocation.
    RequiresRoot,
    /// The sampling duration cannot be represented by `powermetrics`.
    InvalidDuration,
    /// The sampler process could not be started or observed.
    Powermetrics(io::Error),
    /// `powermetrics` exited unsuccessfully.
    PowermetricsFailed(String),
    /// The sampler output did not contain a recognized package-power line.
    MissingPackagePower,
    /// The bracketed workload failed.
    Workload(io::Error),
}

impl fmt::Display for EnergyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RequiresRoot => formatter.write_str(
                "powermetrics requires root; build first, then run: sudo target/release/platform-truth energy -- <workload> [args...]",
            ),
            Self::InvalidDuration => formatter.write_str("powermetrics duration must be at least 1 ms"),
            Self::Powermetrics(error) => write!(formatter, "could not run powermetrics: {error}"),
            Self::PowermetricsFailed(output) => {
                write!(formatter, "powermetrics failed: {}", output.trim())
            }
            Self::MissingPackagePower => formatter.write_str(
                "powermetrics output contained no combined/package power measurement",
            ),
            Self::Workload(error) => write!(formatter, "bracketed workload failed: {error}"),
        }
    }
}

impl std::error::Error for EnergyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Powermetrics(error) | Self::Workload(error) => Some(error),
            Self::RequiresRoot
            | Self::InvalidDuration
            | Self::PowermetricsFailed(_)
            | Self::MissingPackagePower => None,
        }
    }
}

/// Returns the exact privileged command an operator can use for a workload.
pub fn operator_invocation(workload: &str) -> String {
    format!("sudo target/release/platform-truth energy -- {workload}")
}

/// Parses average package power and integrates it over a sampling interval.
pub fn parse_package_energy(
    output: &str,
    sample_duration: Duration,
) -> Result<EnergyMeasurement, EnergyError> {
    let sample_seconds = sample_duration.as_secs_f64();
    if sample_seconds <= 0.0 {
        return Err(EnergyError::InvalidDuration);
    }
    for line in output.lines() {
        let lowercase = line.to_ascii_lowercase();
        if (lowercase.contains("combined power") || lowercase.contains("package power"))
            && let Some(milliwatts) = number_before_unit(line, "mW")
        {
            let average_package_watts = milliwatts / 1_000.0;
            return Ok(EnergyMeasurement {
                average_package_watts,
                sample_seconds,
                package_joules: average_package_watts * sample_seconds,
            });
        }
    }
    Err(EnergyError::MissingPackagePower)
}

fn number_before_unit(line: &str, unit: &str) -> Option<f64> {
    let fields = line.split_whitespace().collect::<Vec<_>>();
    fields.windows(2).find_map(|pair| {
        if pair[1].trim_matches(|character: char| !character.is_ascii_alphabetic()) == unit {
            pair[0]
                .trim_matches(|character: char| {
                    !character.is_ascii_digit() && character != '.' && character != '-'
                })
                .parse::<f64>()
                .ok()
        } else {
            None
        }
    })
}

/// Brackets a closure with a single privileged `powermetrics` package-power sample.
pub fn measure_with_powermetrics<F>(
    sample_duration: Duration,
    workload: F,
) -> Result<EnergyMeasurement, EnergyError>
where
    F: FnOnce() -> io::Result<()>,
{
    let effective_user_id = unsafe {
        // SAFETY: `geteuid` has no arguments or preconditions.
        libc::geteuid()
    };
    require_root(effective_user_id)?;
    let interval_ms =
        u64::try_from(sample_duration.as_millis()).map_err(|_| EnergyError::InvalidDuration)?;
    if interval_ms == 0 {
        return Err(EnergyError::InvalidDuration);
    }
    let sampler = Command::new("/usr/bin/powermetrics")
        .args([
            "--samplers",
            "cpu_power",
            "-i",
            &interval_ms.to_string(),
            "-n",
            "1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(EnergyError::Powermetrics)?;
    let workload_result = workload();
    let output = sampler
        .wait_with_output()
        .map_err(EnergyError::Powermetrics)?;
    workload_result.map_err(EnergyError::Workload)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}\n{stderr}");
    if !output.status.success() {
        return Err(EnergyError::PowermetricsFailed(combined));
    }
    parse_package_energy(&combined, sample_duration)
}

fn require_root(effective_user_id: libc::uid_t) -> Result<(), EnergyError> {
    if effective_user_id == 0 {
        Ok(())
    } else {
        Err(EnergyError::RequiresRoot)
    }
}

/// Measures a command as the bracketed workload.
pub fn measure_command(
    sample_duration: Duration,
    program: &str,
    arguments: &[String],
) -> Result<EnergyMeasurement, EnergyError> {
    measure_with_powermetrics(sample_duration, || {
        let status = Command::new(program).args(arguments).status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "workload exited with status {status}"
            )))
        }
    })
}

/// Prints a machine-readable measured energy record.
pub fn print_measurement(measurement: EnergyMeasurement) {
    println!(
        "average_package_watts: {:.9}",
        measurement.average_package_watts
    );
    println!("sample_seconds: {:.9}", measurement.sample_seconds);
    println!("package_joules: {:.9}", measurement.package_joules);
    println!(
        "JSON {}",
        serde_json::json!({
            "kind": "energy",
            "status": "measured",
            "average_package_watts": measurement.average_package_watts,
            "sample_seconds": measurement.sample_seconds,
            "package_joules": measurement.package_joules
        })
    );
}

/// Prints the expected non-root deferral and exact operator invocation.
pub fn print_not_measured(workload: &str) {
    let invocation = operator_invocation(workload);
    println!("NOT MEASURED — requires operator sudo run");
    println!("operator invocation: {invocation}");
    println!(
        "JSON {}",
        serde_json::json!({
            "kind": "energy",
            "status": "not_measured",
            "reason": "requires operator sudo run",
            "operator_invocation": invocation
        })
    );
}

#[cfg(test)]
mod tests {
    use super::{EnergyError, operator_invocation, parse_package_energy, require_root};
    use std::time::Duration;

    #[test]
    fn energy_parser_reports_positive_joules() -> Result<(), Box<dyn std::error::Error>> {
        let output = "Combined Power (CPU + GPU + ANE): 2500 mW";
        let energy = parse_package_energy(output, Duration::from_secs(2))?;
        assert_eq!(energy.package_joules, 5.0);
        Ok(())
    }

    #[test]
    fn energy_operator_command_requires_sudo() {
        assert!(operator_invocation("./workload").starts_with("sudo "));
        let error = require_root(501).expect_err("an unprivileged caller must be rejected");
        assert!(matches!(error, EnergyError::RequiresRoot));
        assert!(
            error
                .to_string()
                .contains("sudo target/release/platform-truth")
        );
    }
}
