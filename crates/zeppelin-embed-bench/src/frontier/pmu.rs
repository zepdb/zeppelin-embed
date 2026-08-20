//! Optional `xctrace` CPU-counter capture and typed gap attribution.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// One counter and the observation it contributes to attribution.
#[derive(Clone, Debug, PartialEq)]
pub struct CounterReading {
    /// Canonical counter family (`ipc`, `stall`, or `bandwidth`).
    pub name: String,
    /// Parsed numeric value.
    pub value: f64,
    /// Counter unit from the export.
    pub unit: String,
    /// Plain-language statement of what the value showed.
    pub interpretation: String,
}

impl CounterReading {
    /// Creates a finite reading with nonempty identity and interpretation.
    pub fn new(
        name: impl Into<String>,
        value: f64,
        unit: impl Into<String>,
        interpretation: impl Into<String>,
    ) -> Result<Self, PmuError> {
        let reading = Self {
            name: name.into(),
            value,
            unit: unit.into(),
            interpretation: interpretation.into(),
        };
        if reading.name.trim().is_empty()
            || reading.unit.trim().is_empty()
            || reading.interpretation.trim().is_empty()
            || !reading.value.is_finite()
        {
            return Err(PmuError::InvalidCounter);
        }
        Ok(reading)
    }
}

/// Whether measured counters prove the remaining gap is reducible.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AttributionClass {
    /// Counters show a remaining optimization opportunity.
    Reducible {
        /// PMU-backed reason the frontier remains open.
        reason: String,
    },
    /// Counters prove the remaining gap comes from an irreducible cause.
    Irreducible {
        /// PMU-backed cause that cannot be optimized away.
        cause: String,
    },
    /// Counters were captured but do not prove either conclusion.
    Unattributed {
        /// Missing link in the attribution.
        reason: String,
    },
}

/// Parsed CPU-counter evidence for one measured run.
#[derive(Clone, Debug, PartialEq)]
pub struct PmuReport {
    counters: Vec<CounterReading>,
    evidence_path: String,
    class: AttributionClass,
}

impl PmuReport {
    /// Creates a report only with counters, an evidence path, and a conclusion.
    pub fn new(
        counters: Vec<CounterReading>,
        evidence_path: impl Into<String>,
        class: AttributionClass,
    ) -> Result<Self, PmuError> {
        let evidence_path = evidence_path.into();
        if counters.is_empty() || evidence_path.trim().is_empty() {
            return Err(PmuError::MissingAttributionEvidence);
        }
        let conclusion = match &class {
            AttributionClass::Reducible { reason } | AttributionClass::Unattributed { reason } => {
                reason
            }
            AttributionClass::Irreducible { cause } => cause,
        };
        if conclusion.trim().is_empty() {
            return Err(PmuError::MissingAttributionEvidence);
        }
        Ok(Self {
            counters,
            evidence_path,
            class,
        })
    }

    /// Returns parsed counters and their interpretations.
    #[must_use]
    pub fn counters(&self) -> &[CounterReading] {
        &self.counters
    }

    /// Returns the attribution classification.
    #[must_use]
    pub const fn class(&self) -> &AttributionClass {
        &self.class
    }

    /// Returns the immutable evidence artifact path.
    #[must_use]
    pub fn evidence_path(&self) -> &str {
        &self.evidence_path
    }

    /// Converts only an irreducible report into completion authority.
    #[must_use]
    pub(crate) fn into_irreducible_evidence(self) -> Option<AttributionEvidence> {
        let AttributionClass::Irreducible { cause } = self.class else {
            return None;
        };
        Some(AttributionEvidence {
            counters: self.counters,
            evidence_path: self.evidence_path,
            conclusion: cause,
        })
    }
}

/// Evidence required by the type-level `CampaignStop::Complete` authority.
///
/// Fields are private and the only constructor consumes a nonempty,
/// irreducible [`PmuReport`]. Percentage and stagnation signals cannot create
/// this value.
#[derive(Clone, Debug, PartialEq)]
pub struct AttributionEvidence {
    counters: Vec<CounterReading>,
    evidence_path: String,
    conclusion: String,
}

impl AttributionEvidence {
    /// Counters and the text describing what each showed.
    #[must_use]
    pub fn counters(&self) -> &[CounterReading] {
        &self.counters
    }

    /// Path to the captured PMU artifact.
    #[must_use]
    pub fn evidence_path(&self) -> &str {
        &self.evidence_path
    }

    /// PMU-backed irreducible-gap conclusion.
    #[must_use]
    pub fn conclusion(&self) -> &str {
        &self.conclusion
    }
}

/// Optional PMU result; unavailability is explicit and never completion.
#[derive(Clone, Debug, PartialEq)]
pub enum PmuOutcome {
    /// CPU counters were captured and parsed.
    Measured(PmuReport),
    /// CI, permissions, the template, or export prevented attribution.
    Unavailable {
        /// Literal reason no attribution is available.
        reason: String,
    },
}

/// Captures a command with Instruments' CPU Counters template.
///
/// Tool absence, permissions, template absence, record failure, export
/// failure, or unrecognized counter output degrades to `Unavailable`. This is
/// graceful for CI, but callers must keep the campaign frontier open.
#[must_use]
pub fn capture_cpu_counters(program: &str, arguments: &[String], trace_path: &Path) -> PmuOutcome {
    let mut record_arguments = vec![
        "record".to_owned(),
        "--template".to_owned(),
        "CPU Counters".to_owned(),
        "--output".to_owned(),
        trace_path.display().to_string(),
        "--launch".to_owned(),
        "--".to_owned(),
        program.to_owned(),
    ];
    record_arguments.extend(arguments.iter().cloned());
    let recorded = match Command::new("xctrace").args(&record_arguments).output() {
        Ok(output) => output,
        Err(error) => return unavailable(format!("xctrace could not start: {error}")),
    };
    if !recorded.status.success() {
        return unavailable(format!(
            "xctrace CPU Counters record failed: {}",
            String::from_utf8_lossy(&recorded.stderr).trim()
        ));
    }
    let exported = match Command::new("xctrace")
        .args(["export", "--input"])
        .arg(trace_path)
        .args(["--xpath", "/trace-toc/run/data/table"])
        .output()
    {
        Ok(output) => output,
        Err(error) => return unavailable(format!("xctrace export could not start: {error}")),
    };
    if !exported.status.success() {
        return unavailable(format!(
            "xctrace CPU Counters export failed: {}",
            String::from_utf8_lossy(&exported.stderr).trim()
        ));
    }
    let export_text = String::from_utf8_lossy(&exported.stdout);
    match parse_counter_export(&export_text, trace_path.display().to_string()) {
        Ok(report) => PmuOutcome::Measured(report),
        Err(error) => unavailable(format!("xctrace counter export was unusable: {error}")),
    }
}

/// Parses comma-separated CPU-counter rows for IPC, stalls, and bandwidth.
pub fn parse_counter_export(
    export: &str,
    evidence_path: impl Into<String>,
) -> Result<PmuReport, PmuError> {
    let mut counters = Vec::new();
    for line in export.lines() {
        let mut fields = line.split(',').map(str::trim);
        let Some(raw_name) = fields.next() else {
            continue;
        };
        let canonical = canonical_counter_name(raw_name);
        let Some(name) = canonical else {
            continue;
        };
        let Some(raw_value) = fields.next() else {
            continue;
        };
        let Ok(value) = raw_value.parse::<f64>() else {
            continue;
        };
        let unit = fields.next().unwrap_or("value");
        counters.push(CounterReading::new(
            name,
            value,
            unit,
            format!("{raw_name} measured {value} {unit}"),
        )?);
    }
    for required in ["ipc", "stall", "bandwidth"] {
        if !counters.iter().any(|counter| counter.name == required) {
            return Err(PmuError::MissingCounterFamily(required));
        }
    }
    PmuReport::new(
        counters,
        evidence_path,
        AttributionClass::Unattributed {
            reason: "counters captured; an agent must relate them to the remaining gap".to_owned(),
        },
    )
}

fn canonical_counter_name(name: &str) -> Option<&'static str> {
    let lowercase = name.to_ascii_lowercase();
    if lowercase.contains("instructions per cycle") || lowercase.trim() == "ipc" {
        Some("ipc")
    } else if lowercase.contains("stall") {
        Some("stall")
    } else if lowercase.contains("bandwidth") {
        Some("bandwidth")
    } else {
        None
    }
}

fn unavailable(reason: String) -> PmuOutcome {
    PmuOutcome::Unavailable { reason }
}

/// Typed PMU evidence failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PmuError {
    /// A counter lacked identity, unit, interpretation, or a finite value.
    InvalidCounter,
    /// A report lacked counters, evidence path, or conclusion.
    MissingAttributionEvidence,
    /// A required counter family was absent from the export.
    MissingCounterFamily(&'static str),
    /// A filesystem operation for a future export path failed.
    Io {
        /// Path involved in the failed operation.
        path: PathBuf,
        /// Human-readable I/O failure.
        reason: String,
    },
}

impl fmt::Display for PmuError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCounter => formatter.write_str("PMU counter reading is incomplete"),
            Self::MissingAttributionEvidence => {
                formatter.write_str("PMU attribution needs counters, evidence path, and conclusion")
            }
            Self::MissingCounterFamily(name) => {
                write!(
                    formatter,
                    "PMU export did not contain required {name} counter"
                )
            }
            Self::Io { path, reason } => {
                write!(
                    formatter,
                    "PMU artifact {} failed: {reason}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for PmuError {}

impl From<io::Error> for PmuError {
    fn from(error: io::Error) -> Self {
        Self::Io {
            path: PathBuf::new(),
            reason: error.to_string(),
        }
    }
}
