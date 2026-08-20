//! Error-only fallback for unavailable machine-state probes.

use std::cell::RefCell;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use super::measure::{
    MachineProbe, PreflightOutcome, ProbeOutput, explicitly_on_ac_power,
    explicitly_thermal_nominal, preflight,
};

/// Maximum age of an operator attestation.
///
/// Twenty minutes is long enough to cross the sandbox handoff and compile a
/// campaign binary while keeping the accepted machine-state observation local
/// to the current operator session.
pub const ATTESTATION_TTL: Duration = Duration::from_secs(20 * 60);

/// Failure to create a live operator attestation.
#[derive(Debug)]
pub enum AttestationWriteError {
    /// Live `pmset` output did not establish a safe machine state.
    UnsafeMachineState(Vec<String>),
    /// The live machine identifier was blank.
    InvalidMachineIdentifier,
    /// The system clock could not be represented by the strict UTC schema.
    InvalidClock(String),
    /// Attestation persistence failed.
    Io {
        /// Affected path.
        path: PathBuf,
        /// Human-readable I/O failure.
        reason: String,
    },
}

impl fmt::Display for AttestationWriteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsafeMachineState(reasons) => {
                write!(
                    formatter,
                    "live machine state is not attestable: {}",
                    reasons.join("; ")
                )
            }
            Self::InvalidMachineIdentifier => {
                formatter.write_str("live machine identifier is blank")
            }
            Self::InvalidClock(reason) => {
                write!(formatter, "attestation clock is invalid: {reason}")
            }
            Self::Io { path, reason } => {
                write!(formatter, "attestation {} failed: {reason}", path.display())
            }
        }
    }
}

impl std::error::Error for AttestationWriteError {}

/// Provenance identifying how safe machine state was established.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MachineStateProvenance {
    /// Both live `pmset` probes succeeded with favorable readings.
    DirectProbe,
    /// An unavailable live probe was substituted by a validated attestation.
    OperatorAttestation {
        /// UTC issuance time from the attestation.
        timestamp: String,
        /// Machine identifier bound into the attestation.
        machine_identifier: String,
    },
}

/// Lazy source used only after the direct probe cannot establish state.
pub trait AttestationSource {
    /// Reads the attestation bytes.
    fn read_attestation(&self) -> io::Result<Vec<u8>>;

    /// Returns the current machine identifier.
    fn current_machine_identifier(&self) -> io::Result<String>;

    /// Returns the current wall clock for TTL validation.
    fn now(&self) -> SystemTime;
}

/// Filesystem-backed attestation source used by the frontier CLI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileAttestationSource {
    path: PathBuf,
}

impl FileAttestationSource {
    /// Binds a source to the repository-local ephemeral attestation path.
    #[must_use]
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }
}

impl AttestationSource for FileAttestationSource {
    fn read_attestation(&self) -> io::Result<Vec<u8>> {
        fs::read(&self.path)
    }

    fn current_machine_identifier(&self) -> io::Result<String> {
        system_machine_identifier()
    }

    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Returns the gitignored, machine-local attestation path below `target/`.
#[must_use]
pub fn default_attestation_path(repository_root: &Path) -> PathBuf {
    repository_root.join("target/frontier/operator-attestation.json")
}

/// Command seam used to resolve the current machine identifier.
pub trait MachineIdentifierProbe {
    /// Runs one identifier command and captures its literal result.
    fn command(&self, program: &str, arguments: &[&str]) -> io::Result<ProbeOutput>;
}

struct SystemMachineIdentifierProbe;

impl MachineIdentifierProbe for SystemMachineIdentifierProbe {
    fn command(&self, program: &str, arguments: &[&str]) -> io::Result<ProbeOutput> {
        let output = Command::new(program).args(arguments).output()?;
        Ok(ProbeOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Reads the current Darwin hardware model identifier.
pub fn system_machine_identifier() -> io::Result<String> {
    machine_identifier_with_probe(&SystemMachineIdentifierProbe)
}

/// Resolves the current machine identifier through the ordered command probes.
pub fn machine_identifier_with_probe(probe: &impl MachineIdentifierProbe) -> io::Result<String> {
    let sysctl = probe.command("sysctl", &["-n", "hw.model"]);
    if let Ok(output) = &sysctl
        && output.success
    {
        let identifier = output.stdout.trim();
        if !identifier.is_empty() {
            return Ok(identifier.to_owned());
        }
    }
    let profiler = probe.command("system_profiler", &["SPHardwareDataType"]);
    if let Ok(output) = &profiler
        && output.success
        && let Some(identifier) = output.stdout.lines().find_map(|line| {
            line.trim()
                .strip_prefix("Model Identifier:")
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
    {
        return Ok(identifier.to_owned());
    }
    Err(io::Error::other(format!(
        "machine identifier unavailable: {}; {}",
        command_failure("sysctl -n hw.model", &sysctl),
        command_failure("system_profiler SPHardwareDataType", &profiler)
    )))
}

fn command_failure(label: &str, result: &io::Result<ProbeOutput>) -> String {
    match result {
        Ok(output) => format!("{label} returned no identifier: {}", output.stderr.trim()),
        Err(error) => format!("{label} could not start: {error}"),
    }
}

/// Preflight result carrying durable machine-state provenance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CampaignPreflightOutcome {
    /// Safe state was established directly or through a valid attestation.
    Ready {
        /// Literal power evidence.
        power_evidence: String,
        /// Literal thermal evidence.
        thermal_evidence: String,
        /// How the evidence was established.
        provenance: MachineStateProvenance,
    },
    /// Measurement must idle; no timing may be collected.
    Idle {
        /// Every state that could not be proven safe.
        reasons: Vec<String>,
    },
}

/// Runs the unchanged direct preflight before considering an attestation.
#[must_use]
pub fn preflight_with_attestation(
    probe: &impl MachineProbe,
    source: &impl AttestationSource,
) -> CampaignPreflightOutcome {
    let recording_probe = RecordingProbe::new(probe);
    match preflight(&recording_probe) {
        PreflightOutcome::Ready {
            power_evidence,
            thermal_evidence,
        } => CampaignPreflightOutcome::Ready {
            power_evidence,
            thermal_evidence,
            provenance: MachineStateProvenance::DirectProbe,
        },
        PreflightOutcome::Idle { reasons } if !recording_probe.attestation_is_eligible() => {
            CampaignPreflightOutcome::Idle { reasons }
        }
        PreflightOutcome::Idle { mut reasons } => match source.read_attestation() {
            Ok(bytes) => match validate_attestation_identity(&bytes, source) {
                Ok(attestation) => CampaignPreflightOutcome::Ready {
                    power_evidence: attestation.power_raw,
                    thermal_evidence: attestation.thermal_raw,
                    provenance: MachineStateProvenance::OperatorAttestation {
                        timestamp: attestation.timestamp,
                        machine_identifier: attestation.machine_identifier,
                    },
                },
                Err(reason) => {
                    reasons.push(format!("operator attestation rejected: {reason}"));
                    CampaignPreflightOutcome::Idle { reasons }
                }
            },
            Err(error) => {
                reasons.push(format!("operator attestation unavailable: {error}"));
                CampaignPreflightOutcome::Idle { reasons }
            }
        },
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct RecordedProbeState {
    unavailable: bool,
    unfavorable: bool,
}

struct RecordingProbe<'a, P> {
    inner: &'a P,
    state: RefCell<RecordedProbeState>,
}

impl<'a, P> RecordingProbe<'a, P> {
    fn new(inner: &'a P) -> Self {
        Self {
            inner,
            state: RefCell::new(RecordedProbeState::default()),
        }
    }

    fn attestation_is_eligible(&self) -> bool {
        let state = *self.state.borrow();
        state.unavailable && !state.unfavorable
    }
}

impl<P: MachineProbe> MachineProbe for RecordingProbe<'_, P> {
    fn pmset(&self, arguments: &[&str]) -> io::Result<super::measure::ProbeOutput> {
        let result = self.inner.pmset(arguments);
        let mut state = self.state.borrow_mut();
        match &result {
            Err(_) => state.unavailable = true,
            Ok(output) if !output.success => state.unavailable = true,
            Ok(output) => {
                let favorable = match arguments {
                    ["-g", "ps"] => explicitly_on_ac_power(&output.stdout),
                    ["-g", "therm"] => explicitly_thermal_nominal(&output.stdout),
                    _ => false,
                };
                if !favorable && probe_output_is_unavailable(output) {
                    state.unavailable = true;
                } else if !favorable {
                    state.unfavorable = true;
                }
            }
        }
        result
    }
}

fn probe_output_is_unavailable(output: &super::measure::ProbeOutput) -> bool {
    let mut lines = output
        .stdout
        .lines()
        .chain(output.stderr.lines())
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .peekable();
    lines.peek().is_some()
        && lines.all(|line| {
            let lowercase = line.to_ascii_lowercase();
            lowercase.contains("error") || lowercase.contains("failed to get")
        })
}

/// Executes live `pmset`, validates favorable raw output, and atomically writes
/// a short-lived machine-bound attestation.
pub fn write_operator_attestation(
    probe: &impl MachineProbe,
    machine_identifier: &str,
    now: SystemTime,
    path: impl AsRef<Path>,
) -> Result<(), AttestationWriteError> {
    let power = probe.pmset(&["-g", "ps"]);
    let thermal = probe.pmset(&["-g", "therm"]);
    let mut reasons = Vec::new();
    let power_raw = favorable_live_output(
        power,
        "AC power not established by live pmset -g ps",
        explicitly_on_ac_power,
        &mut reasons,
    );
    let thermal_raw = favorable_live_output(
        thermal,
        "nominal thermal state not established by live pmset -g therm",
        explicitly_thermal_nominal,
        &mut reasons,
    );
    let (Some(power_raw), Some(thermal_raw)) = (power_raw, thermal_raw) else {
        return Err(AttestationWriteError::UnsafeMachineState(reasons));
    };
    let machine_identifier = machine_identifier.trim();
    if machine_identifier.is_empty() {
        return Err(AttestationWriteError::InvalidMachineIdentifier);
    }
    let timestamp = format_iso8601_utc(now).map_err(AttestationWriteError::InvalidClock)?;
    let bytes = serde_json::to_vec_pretty(&json!({
        "schema": 1,
        "timestamp": timestamp,
        "machine_identifier": machine_identifier,
        "pmset_ps_raw": power_raw,
        "pmset_therm_raw": thermal_raw,
    }))
    .map_err(|error| AttestationWriteError::InvalidClock(error.to_string()))?;
    persist_attestation(path.as_ref(), &bytes)
}

fn favorable_live_output(
    result: io::Result<super::measure::ProbeOutput>,
    label: &str,
    favorable: fn(&str) -> bool,
    reasons: &mut Vec<String>,
) -> Option<String> {
    match result {
        Ok(output) if output.success && favorable(&output.stdout) => Some(output.stdout),
        Ok(output) => {
            reasons.push(format!(
                "{label}: {}{}",
                output.stdout.trim(),
                output.stderr.trim()
            ));
            None
        }
        Err(error) => {
            reasons.push(format!("{label}: {error}"));
            None
        }
    }
}

fn persist_attestation(path: &Path, bytes: &[u8]) -> Result<(), AttestationWriteError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|error| AttestationWriteError::Io {
            path: parent.to_path_buf(),
            reason: error.to_string(),
        })?;
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| AttestationWriteError::Io {
            path: temporary.clone(),
            reason: error.to_string(),
        })?;
    file.write_all(bytes)
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all())
        .map_err(|error| AttestationWriteError::Io {
            path: temporary.clone(),
            reason: error.to_string(),
        })?;
    fs::rename(&temporary, path).map_err(|error| AttestationWriteError::Io {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })
}

struct ValidatedAttestation {
    timestamp: String,
    machine_identifier: String,
    power_raw: String,
    thermal_raw: String,
}

fn validate_attestation_identity(
    bytes: &[u8],
    source: &impl AttestationSource,
) -> Result<ValidatedAttestation, String> {
    let value: Value = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    if value.get("schema").and_then(Value::as_u64) != Some(1) {
        return Err("schema must be 1".to_owned());
    }
    let timestamp = string_field(&value, "timestamp")?.to_owned();
    let machine_identifier = string_field(&value, "machine_identifier")?.to_owned();
    let power_raw = string_field(&value, "pmset_ps_raw")?.to_owned();
    let thermal_raw = string_field(&value, "pmset_therm_raw")?.to_owned();
    if !explicitly_on_ac_power(&power_raw) {
        return Err("raw pmset -g ps output does not establish AC power".to_owned());
    }
    if !explicitly_thermal_nominal(&thermal_raw) {
        return Err(
            "raw pmset -g therm output does not establish nominal thermal state".to_owned(),
        );
    }
    let current_machine = source
        .current_machine_identifier()
        .map_err(|error| format!("current machine identifier unavailable: {error}"))?;
    if machine_identifier != current_machine.trim() {
        return Err(format!(
            "machine identifier mismatch: attested {machine_identifier:?}, current {:?}",
            current_machine.trim()
        ));
    }
    if timestamp.trim().is_empty() {
        return Err("timestamp must not be blank".to_owned());
    }
    let issued_at = parse_iso8601_utc(&timestamp)?;
    let now = source
        .now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "current clock is before the Unix epoch".to_owned())?
        .as_secs();
    if issued_at > now {
        return Err("timestamp is in the future".to_owned());
    }
    if now - issued_at > ATTESTATION_TTL.as_secs() {
        return Err(format!(
            "attestation expired: age {} seconds exceeds {} second TTL",
            now - issued_at,
            ATTESTATION_TTL.as_secs()
        ));
    }
    Ok(ValidatedAttestation {
        timestamp,
        machine_identifier,
        power_raw,
        thermal_raw,
    })
}

fn parse_iso8601_utc(timestamp: &str) -> Result<u64, String> {
    let bytes = timestamp.as_bytes();
    if bytes.len() != 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || bytes.get(10) != Some(&b'T')
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
        || bytes.get(19) != Some(&b'Z')
    {
        return Err("timestamp must use YYYY-MM-DDTHH:MM:SSZ".to_owned());
    }
    let year = parse_decimal(bytes, 0, 4)? as i64;
    let month = parse_decimal(bytes, 5, 2)?;
    let day = parse_decimal(bytes, 8, 2)?;
    let hour = parse_decimal(bytes, 11, 2)?;
    let minute = parse_decimal(bytes, 14, 2)?;
    let second = parse_decimal(bytes, 17, 2)?;
    if year < 1970 || !(1..=12).contains(&month) {
        return Err("timestamp date is outside the supported UTC range".to_owned());
    }
    let month_days = match month {
        2 if is_leap_year(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if day == 0 || day > month_days || hour > 23 || minute > 59 || second > 59 {
        return Err("timestamp contains an invalid UTC date or time".to_owned());
    }
    let days = days_from_civil(year, month, day);
    if days < 0 {
        return Err("timestamp predates the Unix epoch".to_owned());
    }
    let day_seconds = u64::try_from(days)
        .ok()
        .and_then(|value| value.checked_mul(86_400))
        .ok_or_else(|| "timestamp exceeds the supported UTC range".to_owned())?;
    day_seconds
        .checked_add(u64::from(hour) * 3_600)
        .and_then(|value| value.checked_add(u64::from(minute) * 60))
        .and_then(|value| value.checked_add(u64::from(second)))
        .ok_or_else(|| "timestamp exceeds the supported UTC range".to_owned())
}

fn parse_decimal(bytes: &[u8], start: usize, length: usize) -> Result<u32, String> {
    let digits = bytes
        .get(start..start + length)
        .ok_or_else(|| "timestamp field is truncated".to_owned())?;
    digits.iter().try_fold(0_u32, |value, digit| {
        if !digit.is_ascii_digit() {
            return Err("timestamp fields must be decimal digits".to_owned());
        }
        Ok(value * 10 + u32::from(*digit - b'0'))
    })
}

fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn format_iso8601_utc(time: SystemTime) -> Result<String, String> {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "time predates the Unix epoch".to_owned())?
        .as_secs();
    let days = i64::try_from(seconds / 86_400)
        .map_err(|_| "time exceeds the supported UTC range".to_owned())?;
    let second_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    if !(1970..=9999).contains(&year) {
        return Err("time exceeds four-digit ISO-8601 years".to_owned());
    }
    let hour = second_of_day / 3_600;
    let minute = second_of_day % 3_600 / 60;
    let second = second_of_day % 60;
    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
    ))
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn string_field<'a>(value: &'a Value, name: &str) -> Result<&'a str, String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .filter(|field| !field.trim().is_empty())
        .ok_or_else(|| format!("missing nonblank string field {name}"))
}
