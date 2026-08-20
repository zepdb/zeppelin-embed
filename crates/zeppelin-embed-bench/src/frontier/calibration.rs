//! Persisted compute ceilings with upward-only revision enforcement.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::roofline::ComputeTier;

const SCHEMA: &str = "zeppelin-compute-calibration-v1";
const REQUIRED_TIERS: [ComputeTier; 3] = [
    ComputeTier::NeonFma,
    ComputeTier::NeonSdot,
    ComputeTier::NeonFp16ConvertFma,
];

/// Repository-shipped calibration for the measured Apple M3 Max machine.
#[must_use]
pub fn default_calibration_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("calibrations/apple-m3-max-mac15-9.json")
}

/// Machine state recorded with a denominator calibration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CalibrationMachineContext {
    /// Human-readable hardware model.
    pub model_name: String,
    /// Stable machine model identifier.
    pub model_identifier: String,
    /// Processor name.
    pub chip: String,
    /// Operating-system product version.
    pub os_product_version: String,
    /// Operating-system build identifier.
    pub os_build: String,
    /// Confirmed power source during calibration.
    pub power_state: String,
    /// Confirmed nominal thermal response during calibration.
    pub thermal_state: String,
}

/// One independent execution of a tier saturation calibration.
#[derive(Clone, Debug, PartialEq)]
pub struct CalibrationRun {
    /// One-based operator-assigned run number.
    pub run: u64,
    /// Sustained rate observed by this run in decimal GMAC/s.
    pub gmac_per_second: f64,
    /// Accepted min-of-medians inputs printed by the harness.
    pub raw_medians_ns: Vec<f64>,
    /// Deterministic saturation-loop checksum.
    pub checksum: u64,
}

/// Persisted best-known ceiling and its raw runs for one instruction tier.
#[derive(Clone, Debug, PartialEq)]
pub struct CalibrationTier {
    /// Calibrated instruction tier.
    pub tier: ComputeTier,
    /// Maximum sustained rate adopted as the ceiling, in decimal GMAC/s.
    pub adopted_gmac_per_second: f64,
    /// Independent raw measurements supporting the adopted value.
    pub runs: Vec<CalibrationRun>,
}

impl CalibrationTier {
    /// Creates a validated tier whose adopted ceiling appears in its raw runs.
    pub fn new(
        tier: ComputeTier,
        adopted_gmac_per_second: f64,
        runs: Vec<CalibrationRun>,
    ) -> Result<Self, CalibrationError> {
        let value = Self {
            tier,
            adopted_gmac_per_second,
            runs,
        };
        validate_tier(&value)?;
        Ok(value)
    }
}

/// One tier's old and newly adopted values in revision history.
#[derive(Clone, Debug, PartialEq)]
pub struct CalibrationChange {
    /// Revised instruction tier.
    pub tier: ComputeTier,
    /// Previously persisted ceiling, absent for an initial write.
    pub previous_gmac_per_second: Option<f64>,
    /// Newly adopted ceiling.
    pub adopted_gmac_per_second: f64,
}

/// Audit record appended whenever a calibration artifact is persisted.
#[derive(Clone, Debug, PartialEq)]
pub struct CalibrationRevision {
    /// Caller-supplied measurement date.
    pub date: String,
    /// Whether an explicit lower-ceiling override was actually used.
    pub override_used: bool,
    /// Human-readable reason for the write or override.
    pub reason: String,
    /// Per-tier values changed by this write.
    pub changes: Vec<CalibrationChange>,
}

/// Versioned compute-denominator artifact consumed by roofline scoring.
#[derive(Clone, Debug, PartialEq)]
pub struct CalibrationArtifact {
    /// Hardware and machine-state provenance.
    pub machine: CalibrationMachineContext,
    /// Caller-supplied measurement date.
    pub measured_date: String,
    /// Exact command that produced the raw measurements.
    pub command: String,
    /// Complete set of supported compute tiers.
    pub tiers: Vec<CalibrationTier>,
    /// Append-style history of persisted revisions.
    pub revision_history: Vec<CalibrationRevision>,
}

impl CalibrationArtifact {
    /// Creates a complete artifact before its first persisted revision record.
    pub fn new(
        machine: CalibrationMachineContext,
        measured_date: impl Into<String>,
        command: impl Into<String>,
        tiers: Vec<CalibrationTier>,
    ) -> Result<Self, CalibrationError> {
        let artifact = Self {
            machine,
            measured_date: measured_date.into(),
            command: command.into(),
            tiers,
            revision_history: Vec::new(),
        };
        validate_artifact(&artifact)?;
        Ok(artifact)
    }

    /// Finds the adopted calibration for one instruction tier.
    #[must_use]
    pub fn tier(&self, tier: ComputeTier) -> Option<&CalibrationTier> {
        self.tiers.iter().find(|candidate| candidate.tier == tier)
    }
}

/// Policy controlling writes to an existing calibration artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CalibrationWritePolicy {
    /// Reject every downward ceiling revision.
    UpwardOnly,
    /// Explicitly permit a downward calibration-only revision.
    AllowLower {
        /// Operator-supplied reason retained in revision history.
        reason: String,
    },
}

/// Typed calibration parse, validation, or persistence failure.
#[derive(Debug)]
pub enum CalibrationError {
    /// Filesystem access failed.
    Io(std::io::Error),
    /// JSON syntax was invalid.
    Json(serde_json::Error),
    /// Required schema content was absent or invalid.
    InvalidArtifact(String),
    /// An upward-only write proposed a lower ceiling.
    DownwardRevision {
        /// Tier whose lower value was rejected.
        tier: ComputeTier,
        /// Best-known persisted ceiling.
        persisted_gmac_per_second: f64,
        /// Lower proposed reading.
        proposed_gmac_per_second: f64,
    },
}

impl fmt::Display for CalibrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "calibration I/O failed: {error}"),
            Self::Json(error) => write!(formatter, "calibration JSON failed: {error}"),
            Self::InvalidArtifact(reason) => {
                write!(formatter, "invalid calibration artifact: {reason}")
            }
            Self::DownwardRevision {
                tier,
                persisted_gmac_per_second,
                proposed_gmac_per_second,
            } => write!(
                formatter,
                "REFUSED LOWER COMPUTE CEILING for {}: proposed {:.6} GMAC/s is below persisted {:.6} GMAC/s; a lower reading usually means a degraded machine state, not a lower physical ceiling",
                tier.as_str(),
                proposed_gmac_per_second,
                persisted_gmac_per_second
            ),
        }
    }
}

impl std::error::Error for CalibrationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::InvalidArtifact(_) | Self::DownwardRevision { .. } => None,
        }
    }
}

impl From<std::io::Error> for CalibrationError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for CalibrationError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

/// Loads and fully validates a persisted calibration artifact.
pub fn load_calibration(path: impl AsRef<Path>) -> Result<CalibrationArtifact, CalibrationError> {
    let bytes = fs::read(path)?;
    let value = serde_json::from_slice::<Value>(&bytes)?;
    parse_artifact(&value)
}

/// Persists a calibration atomically under an upward-only default policy.
///
/// A rejected lower reading leaves the existing bytes untouched. The
/// `AllowLower` policy applies only to this artifact write; it has no path to
/// the measurement preflight.
pub fn persist_calibration(
    path: impl AsRef<Path>,
    proposed: &CalibrationArtifact,
    policy: CalibrationWritePolicy,
) -> Result<(), CalibrationError> {
    validate_artifact(proposed)?;
    let path = path.as_ref();
    let previous = if path.exists() {
        Some(load_calibration(path)?)
    } else {
        None
    };
    let mut lower_revision = false;
    if let Some(persisted) = &previous {
        for tier in REQUIRED_TIERS {
            let old = required_tier(persisted, tier)?;
            let new = required_tier(proposed, tier)?;
            if new.adopted_gmac_per_second < old.adopted_gmac_per_second {
                lower_revision = true;
                if matches!(policy, CalibrationWritePolicy::UpwardOnly) {
                    return Err(CalibrationError::DownwardRevision {
                        tier,
                        persisted_gmac_per_second: old.adopted_gmac_per_second,
                        proposed_gmac_per_second: new.adopted_gmac_per_second,
                    });
                }
            }
        }
    }
    let (override_used, reason) = match &policy {
        CalibrationWritePolicy::UpwardOnly => {
            (false, "initial or upward-only calibration".to_owned())
        }
        CalibrationWritePolicy::AllowLower { reason } => {
            require_nonblank("override reason", reason)?;
            (lower_revision, reason.clone())
        }
    };
    let mut artifact = proposed.clone();
    artifact.revision_history = previous
        .as_ref()
        .map_or_else(Vec::new, |value| value.revision_history.clone());
    let changes = REQUIRED_TIERS
        .into_iter()
        .map(|tier| {
            let previous_gmac_per_second = previous
                .as_ref()
                .and_then(|value| value.tier(tier))
                .map(|value| value.adopted_gmac_per_second);
            let adopted_gmac_per_second = proposed
                .tier(tier)
                .map(|value| value.adopted_gmac_per_second)
                .ok_or_else(|| invalid(format!("missing tier {}", tier.as_str())))?;
            Ok(CalibrationChange {
                tier,
                previous_gmac_per_second,
                adopted_gmac_per_second,
            })
        })
        .collect::<Result<Vec<_>, CalibrationError>>()?;
    artifact.revision_history.push(CalibrationRevision {
        date: artifact.measured_date.clone(),
        override_used,
        reason,
        changes,
    });
    write_atomic(path, &serialize_artifact(&artifact)?)
}

fn validate_artifact(artifact: &CalibrationArtifact) -> Result<(), CalibrationError> {
    require_nonblank("measured_date", &artifact.measured_date)?;
    require_nonblank("command", &artifact.command)?;
    for (name, value) in [
        ("machine.model_name", &artifact.machine.model_name),
        (
            "machine.model_identifier",
            &artifact.machine.model_identifier,
        ),
        ("machine.chip", &artifact.machine.chip),
        (
            "machine.os_product_version",
            &artifact.machine.os_product_version,
        ),
        ("machine.os_build", &artifact.machine.os_build),
        ("machine.power_state", &artifact.machine.power_state),
        ("machine.thermal_state", &artifact.machine.thermal_state),
    ] {
        require_nonblank(name, value)?;
    }
    let mut tiers = BTreeSet::new();
    for tier in &artifact.tiers {
        validate_tier(tier)?;
        if !tiers.insert(tier.tier) {
            return Err(invalid(format!("duplicate tier {}", tier.tier.as_str())));
        }
    }
    for required in REQUIRED_TIERS {
        if !tiers.contains(&required) {
            return Err(invalid(format!("missing tier {}", required.as_str())));
        }
    }
    if tiers.len() != REQUIRED_TIERS.len() {
        return Err(invalid("unknown compute tier"));
    }
    for revision in &artifact.revision_history {
        require_nonblank("revision.date", &revision.date)?;
        require_nonblank("revision.reason", &revision.reason)?;
        if revision.changes.len() != REQUIRED_TIERS.len() {
            return Err(invalid("revision must contain every compute tier"));
        }
    }
    Ok(())
}

fn validate_tier(tier: &CalibrationTier) -> Result<(), CalibrationError> {
    validate_positive("adopted_gmac_per_second", tier.adopted_gmac_per_second)?;
    if tier.runs.is_empty() {
        return Err(invalid(format!("{} has no raw runs", tier.tier.as_str())));
    }
    let mut run_numbers = BTreeSet::new();
    let mut checksum = None;
    let mut adopted_is_observed = false;
    for run in &tier.runs {
        if run.run == 0 || !run_numbers.insert(run.run) {
            return Err(invalid(format!(
                "{} has invalid run numbers",
                tier.tier.as_str()
            )));
        }
        validate_positive("run.gmac_per_second", run.gmac_per_second)?;
        if run.raw_medians_ns.is_empty() {
            return Err(invalid(format!(
                "{} run has no raw medians",
                tier.tier.as_str()
            )));
        }
        for median in &run.raw_medians_ns {
            validate_positive("run.raw_medians_ns", *median)?;
        }
        match checksum {
            Some(previous) if previous != run.checksum => {
                return Err(invalid(format!(
                    "{} checksums differ across runs",
                    tier.tier.as_str()
                )));
            }
            None => checksum = Some(run.checksum),
            Some(_) => {}
        }
        if run.gmac_per_second > tier.adopted_gmac_per_second {
            return Err(invalid(format!(
                "{} adopted ceiling is below a raw measurement",
                tier.tier.as_str()
            )));
        }
        adopted_is_observed |= run.gmac_per_second == tier.adopted_gmac_per_second;
    }
    if !adopted_is_observed {
        return Err(invalid(format!(
            "{} adopted ceiling does not match a raw measurement",
            tier.tier.as_str()
        )));
    }
    Ok(())
}

fn validate_positive(name: &str, value: f64) -> Result<(), CalibrationError> {
    if value.is_finite() && value > 0.0 {
        Ok(())
    } else {
        Err(invalid(format!("{name} must be positive and finite")))
    }
}

fn require_nonblank(name: &str, value: &str) -> Result<(), CalibrationError> {
    if value.trim().is_empty() {
        Err(invalid(format!("{name} must not be blank")))
    } else {
        Ok(())
    }
}

fn required_tier(
    artifact: &CalibrationArtifact,
    tier: ComputeTier,
) -> Result<&CalibrationTier, CalibrationError> {
    artifact
        .tier(tier)
        .ok_or_else(|| invalid(format!("missing tier {}", tier.as_str())))
}

fn invalid(reason: impl Into<String>) -> CalibrationError {
    CalibrationError::InvalidArtifact(reason.into())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), CalibrationError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("json");
    let temporary = path.with_extension(format!("{extension}.tmp-{}", std::process::id()));
    fs::write(&temporary, bytes)?;
    match fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(temporary);
            Err(CalibrationError::Io(error))
        }
    }
}

fn serialize_artifact(artifact: &CalibrationArtifact) -> Result<Vec<u8>, CalibrationError> {
    let value = json!({
        "schema": SCHEMA,
        "machine": {
            "model_name": artifact.machine.model_name,
            "model_identifier": artifact.machine.model_identifier,
            "chip": artifact.machine.chip,
            "os_product_version": artifact.machine.os_product_version,
            "os_build": artifact.machine.os_build,
            "power_state": artifact.machine.power_state,
            "thermal_state": artifact.machine.thermal_state,
        },
        "measured_date": artifact.measured_date,
        "command": artifact.command,
        "tiers": artifact.tiers.iter().map(serialize_tier).collect::<Vec<_>>(),
        "revision_history": artifact.revision_history.iter().map(serialize_revision).collect::<Vec<_>>(),
    });
    let mut bytes = serde_json::to_vec_pretty(&value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn serialize_tier(tier: &CalibrationTier) -> Value {
    json!({
        "tier": tier.tier.as_str(),
        "adopted_gmac_per_second": tier.adopted_gmac_per_second,
        "runs": tier.runs.iter().map(|run| json!({
            "run": run.run,
            "gmac_per_second": run.gmac_per_second,
            "raw_medians_ns": run.raw_medians_ns,
            "checksum": run.checksum,
        })).collect::<Vec<_>>(),
    })
}

fn serialize_revision(revision: &CalibrationRevision) -> Value {
    json!({
        "date": revision.date,
        "override_used": revision.override_used,
        "reason": revision.reason,
        "changes": revision.changes.iter().map(|change| json!({
            "tier": change.tier.as_str(),
            "previous_gmac_per_second": change.previous_gmac_per_second,
            "adopted_gmac_per_second": change.adopted_gmac_per_second,
        })).collect::<Vec<_>>(),
    })
}

fn parse_artifact(value: &Value) -> Result<CalibrationArtifact, CalibrationError> {
    let root = object(value, "root")?;
    if string(root, "schema")? != SCHEMA {
        return Err(invalid("unsupported schema"));
    }
    let machine_value = field(root, "machine")?;
    let machine = object(machine_value, "machine")?;
    let tiers = array(root, "tiers")?
        .iter()
        .map(parse_tier)
        .collect::<Result<Vec<_>, _>>()?;
    let revisions = array(root, "revision_history")?
        .iter()
        .map(parse_revision)
        .collect::<Result<Vec<_>, _>>()?;
    let artifact = CalibrationArtifact {
        machine: CalibrationMachineContext {
            model_name: string(machine, "model_name")?.to_owned(),
            model_identifier: string(machine, "model_identifier")?.to_owned(),
            chip: string(machine, "chip")?.to_owned(),
            os_product_version: string(machine, "os_product_version")?.to_owned(),
            os_build: string(machine, "os_build")?.to_owned(),
            power_state: string(machine, "power_state")?.to_owned(),
            thermal_state: string(machine, "thermal_state")?.to_owned(),
        },
        measured_date: string(root, "measured_date")?.to_owned(),
        command: string(root, "command")?.to_owned(),
        tiers,
        revision_history: revisions,
    };
    validate_artifact(&artifact)?;
    Ok(artifact)
}

fn parse_tier(value: &Value) -> Result<CalibrationTier, CalibrationError> {
    let map = object(value, "tier")?;
    CalibrationTier::new(
        parse_tier_name(string(map, "tier")?)?,
        number(map, "adopted_gmac_per_second")?,
        array(map, "runs")?
            .iter()
            .map(parse_run)
            .collect::<Result<Vec<_>, _>>()?,
    )
}

fn parse_run(value: &Value) -> Result<CalibrationRun, CalibrationError> {
    let map = object(value, "run")?;
    Ok(CalibrationRun {
        run: unsigned(map, "run")?,
        gmac_per_second: number(map, "gmac_per_second")?,
        raw_medians_ns: array(map, "raw_medians_ns")?
            .iter()
            .map(|median| {
                median
                    .as_f64()
                    .ok_or_else(|| invalid("raw median must be a number"))
            })
            .collect::<Result<Vec<_>, _>>()?,
        checksum: unsigned(map, "checksum")?,
    })
}

fn parse_revision(value: &Value) -> Result<CalibrationRevision, CalibrationError> {
    let map = object(value, "revision")?;
    Ok(CalibrationRevision {
        date: string(map, "date")?.to_owned(),
        override_used: boolean(map, "override_used")?,
        reason: string(map, "reason")?.to_owned(),
        changes: array(map, "changes")?
            .iter()
            .map(parse_change)
            .collect::<Result<Vec<_>, _>>()?,
    })
}

fn parse_change(value: &Value) -> Result<CalibrationChange, CalibrationError> {
    let map = object(value, "change")?;
    let previous = field(map, "previous_gmac_per_second")?;
    Ok(CalibrationChange {
        tier: parse_tier_name(string(map, "tier")?)?,
        previous_gmac_per_second: if previous.is_null() {
            None
        } else {
            Some(
                previous
                    .as_f64()
                    .ok_or_else(|| invalid("previous ceiling must be numeric or null"))?,
            )
        },
        adopted_gmac_per_second: number(map, "adopted_gmac_per_second")?,
    })
}

fn parse_tier_name(value: &str) -> Result<ComputeTier, CalibrationError> {
    match value {
        "neon-fma-f32" => Ok(ComputeTier::NeonFma),
        "neon-sdot-i8" => Ok(ComputeTier::NeonSdot),
        "neon-fp16-convert-fma" => Ok(ComputeTier::NeonFp16ConvertFma),
        other => Err(invalid(format!("unknown compute tier {other}"))),
    }
}

fn object<'a>(value: &'a Value, name: &str) -> Result<&'a Map<String, Value>, CalibrationError> {
    value
        .as_object()
        .ok_or_else(|| invalid(format!("{name} must be an object")))
}

fn field<'a>(map: &'a Map<String, Value>, name: &str) -> Result<&'a Value, CalibrationError> {
    map.get(name)
        .ok_or_else(|| invalid(format!("missing field {name}")))
}

fn string<'a>(map: &'a Map<String, Value>, name: &str) -> Result<&'a str, CalibrationError> {
    field(map, name)?
        .as_str()
        .ok_or_else(|| invalid(format!("{name} must be a string")))
}

fn number(map: &Map<String, Value>, name: &str) -> Result<f64, CalibrationError> {
    field(map, name)?
        .as_f64()
        .ok_or_else(|| invalid(format!("{name} must be a number")))
}

fn unsigned(map: &Map<String, Value>, name: &str) -> Result<u64, CalibrationError> {
    field(map, name)?
        .as_u64()
        .ok_or_else(|| invalid(format!("{name} must be an unsigned integer")))
}

fn boolean(map: &Map<String, Value>, name: &str) -> Result<bool, CalibrationError> {
    field(map, name)?
        .as_bool()
        .ok_or_else(|| invalid(format!("{name} must be a boolean")))
}

fn array<'a>(map: &'a Map<String, Value>, name: &str) -> Result<&'a [Value], CalibrationError> {
    field(map, name)?
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| invalid(format!("{name} must be an array")))
}
