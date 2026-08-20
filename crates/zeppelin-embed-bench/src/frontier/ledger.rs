//! Append-only JSON-lines optimization ledger.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::attestation::MachineStateProvenance;
use super::pmu::{AttributionEvidence, CounterReading};
use super::tune::CampaignStop;

/// Keep/revert decision stored for every hypothesis.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LedgerDecision {
    /// Candidate survived correctness and the full workload matrix.
    Keep,
    /// Candidate regressed, failed correctness, or overfit a workload.
    Revert,
}

impl LedgerDecision {
    fn as_str(self) -> &'static str {
        match self {
            Self::Keep => "keep",
            Self::Revert => "revert",
        }
    }
}

/// Attribution payload mandatory for a completed ledger row.
#[derive(Clone, Debug, PartialEq)]
pub struct LedgerAttribution {
    /// Counters and what each showed.
    pub counters: Vec<CounterReading>,
    /// PMU artifact path.
    pub evidence_path: String,
    /// Irreducible-gap conclusion.
    pub conclusion: String,
}

impl From<&AttributionEvidence> for LedgerAttribution {
    fn from(evidence: &AttributionEvidence) -> Self {
        Self {
            counters: evidence.counters().to_vec(),
            evidence_path: evidence.evidence_path().to_owned(),
            conclusion: evidence.conclusion().to_owned(),
        }
    }
}

/// Attribution-only completion or explicitly open frontier.
#[derive(Clone, Debug, PartialEq)]
pub enum LedgerStatus {
    /// PMU-attributed proof is the only completion status.
    Complete {
        /// Required counter attribution and evidence path.
        attribution: LedgerAttribution,
    },
    /// No irreducible proof exists and follow-up hypotheses remain.
    FrontierOpen {
        /// Why completion authority is absent.
        reason: String,
        /// Ranked next hypotheses.
        hypotheses: Vec<String>,
    },
}

impl LedgerStatus {
    /// Converts the type-safe campaign decision into its ledger schema.
    #[must_use]
    pub fn from_campaign_stop(stop: &CampaignStop) -> Self {
        match stop {
            CampaignStop::Complete { attribution } => Self::Complete {
                attribution: LedgerAttribution::from(attribution),
            },
            CampaignStop::FrontierOpen { reason, hypotheses } => Self::FrontierOpen {
                reason: reason.clone(),
                hypotheses: hypotheses.clone(),
            },
        }
    }
}

/// One immutable optimization experiment row.
#[derive(Clone, Debug, PartialEq)]
pub struct LedgerRow {
    /// Measurement date.
    pub date: String,
    /// Falsifiable change hypothesis.
    pub hypothesis: String,
    /// Predicted performance mechanism.
    pub mechanism: String,
    /// Percent change from incumbent.
    pub delta_percent: f64,
    /// Achieved percent of the workload-selected roofline.
    pub roofline_percent: f64,
    /// Keep or revert decision.
    pub decision: LedgerDecision,
    /// Exact workload name.
    pub workload: String,
    /// Synthetic-only evidence marker.
    pub provisional: bool,
    /// Measurement/correctness evidence path.
    pub evidence_path: String,
    /// How safe machine state was established for the measurement.
    pub machine_state: MachineStateProvenance,
    /// Completion or open-frontier record.
    pub status: LedgerStatus,
}

/// Open append-only ledger bound to the exact prior-byte snapshot.
#[derive(Debug)]
pub struct Ledger {
    path: PathBuf,
    observed_length: usize,
    observed_hash: u64,
}

impl Ledger {
    /// Opens or creates a ledger without truncate/write capability.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LedgerError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|error| LedgerError::Io {
                path: parent.to_path_buf(),
                reason: error.to_string(),
            })?;
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)
            .map_err(|error| LedgerError::Io {
                path: path.clone(),
                reason: error.to_string(),
            })?;
        let bytes = read_and_validate(&path)?;
        Ok(Self {
            path,
            observed_length: bytes.len(),
            observed_hash: stable_hash(&bytes),
        })
    }

    /// Appends exactly one row after proving all prior bytes are unchanged.
    pub fn append(&mut self, row: &LedgerRow) -> Result<(), LedgerError> {
        validate_row(row)?;
        let prior = read_bytes(&self.path)?;
        if prior.len() != self.observed_length || stable_hash(&prior) != self.observed_hash {
            return Err(LedgerError::PriorContentChanged);
        }
        let _rows = parse_rows(&prior)?;
        let mut encoded = serde_json::to_vec(&row_to_value(row)).map_err(|error| {
            LedgerError::InvalidRow(format!("row JSON serialization failed: {error}"))
        })?;
        encoded.push(b'\n');
        let mut file = OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(|error| LedgerError::Io {
                path: self.path.clone(),
                reason: error.to_string(),
            })?;
        file.write_all(&encoded).map_err(|error| LedgerError::Io {
            path: self.path.clone(),
            reason: error.to_string(),
        })?;
        file.sync_data().map_err(|error| LedgerError::Io {
            path: self.path.clone(),
            reason: error.to_string(),
        })?;
        let mut combined = prior;
        combined.extend_from_slice(&encoded);
        self.observed_length = combined.len();
        self.observed_hash = stable_hash(&combined);
        Ok(())
    }

    /// Reads all rows only if the observed history remains byte-identical.
    pub fn rows(&self) -> Result<Vec<LedgerRow>, LedgerError> {
        let bytes = read_bytes(&self.path)?;
        if bytes.len() != self.observed_length || stable_hash(&bytes) != self.observed_hash {
            return Err(LedgerError::PriorContentChanged);
        }
        parse_rows(&bytes)
    }

    /// Path to the append-only ledger.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Aggregate report over any collection of ledger rows.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LedgerSummary {
    /// Total experiment rows.
    pub rows: usize,
    /// Kept variants.
    pub keeps: usize,
    /// Reverted variants.
    pub reverts: usize,
    /// PMU-attributed completion rows.
    pub complete: usize,
    /// Open-frontier rows.
    pub frontier_open: usize,
    /// Synthetic-only rows awaiting real-workload validation.
    pub provisional: usize,
}

impl LedgerSummary {
    /// Summarizes rows without changing their order or bytes.
    #[must_use]
    pub fn from_rows(rows: &[LedgerRow]) -> Self {
        let mut summary = Self::default();
        for row in rows {
            summary.rows += 1;
            match row.decision {
                LedgerDecision::Keep => summary.keeps += 1,
                LedgerDecision::Revert => summary.reverts += 1,
            }
            match row.status {
                LedgerStatus::Complete { .. } => summary.complete += 1,
                LedgerStatus::FrontierOpen { .. } => summary.frontier_open += 1,
            }
            summary.provisional += usize::from(row.provisional);
        }
        summary
    }
}

/// Typed append-only ledger failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LedgerError {
    /// A filesystem operation failed.
    Io {
        /// Affected path.
        path: PathBuf,
        /// Human-readable I/O failure.
        reason: String,
    },
    /// Existing bytes did not parse as valid ledger rows.
    CorruptHistory(String),
    /// A proposed row omitted required fields or finite values.
    InvalidRow(String),
    /// Bytes observed on open or the preceding append were modified.
    PriorContentChanged,
}

impl fmt::Display for LedgerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, reason } => {
                write!(formatter, "ledger {} failed: {reason}", path.display())
            }
            Self::CorruptHistory(reason) => {
                write!(formatter, "ledger history is corrupt: {reason}")
            }
            Self::InvalidRow(reason) => write!(formatter, "ledger row is invalid: {reason}"),
            Self::PriorContentChanged => formatter
                .write_str("append refused because previously observed ledger bytes were changed"),
        }
    }
}

impl std::error::Error for LedgerError {}

fn validate_row(row: &LedgerRow) -> Result<(), LedgerError> {
    for (name, value) in [
        ("date", row.date.as_str()),
        ("hypothesis", row.hypothesis.as_str()),
        ("mechanism", row.mechanism.as_str()),
        ("workload", row.workload.as_str()),
        ("evidence_path", row.evidence_path.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(LedgerError::InvalidRow(format!("{name} must not be blank")));
        }
    }
    if !row.delta_percent.is_finite() || !row.roofline_percent.is_finite() {
        return Err(LedgerError::InvalidRow(
            "delta and roofline percentages must be finite".to_owned(),
        ));
    }
    if let MachineStateProvenance::OperatorAttestation {
        timestamp,
        machine_identifier,
    } = &row.machine_state
        && (timestamp.trim().is_empty() || machine_identifier.trim().is_empty())
    {
        return Err(LedgerError::InvalidRow(
            "attested machine state requires timestamp and machine identifier".to_owned(),
        ));
    }
    match &row.status {
        LedgerStatus::Complete { attribution } => {
            if attribution.counters.is_empty()
                || attribution.evidence_path.trim().is_empty()
                || attribution.conclusion.trim().is_empty()
            {
                return Err(LedgerError::InvalidRow(
                    "complete status requires counter attribution".to_owned(),
                ));
            }
        }
        LedgerStatus::FrontierOpen { reason, hypotheses } => {
            if reason.trim().is_empty() || hypotheses.is_empty() {
                return Err(LedgerError::InvalidRow(
                    "frontier-open status requires a reason and hypotheses".to_owned(),
                ));
            }
        }
    }
    Ok(())
}

fn row_to_value(row: &LedgerRow) -> Value {
    let status = match &row.status {
        LedgerStatus::Complete { attribution } => json!({
            "kind": "complete",
            "attribution": {
                "evidence_path": attribution.evidence_path,
                "conclusion": attribution.conclusion,
                "counters": attribution.counters.iter().map(|counter| json!({
                    "name": counter.name,
                    "value": counter.value,
                    "unit": counter.unit,
                    "showed": counter.interpretation,
                })).collect::<Vec<_>>(),
            }
        }),
        LedgerStatus::FrontierOpen { reason, hypotheses } => json!({
            "kind": "frontier-open",
            "reason": reason,
            "hypotheses": hypotheses,
        }),
    };
    json!({
        "date": row.date,
        "hypothesis": row.hypothesis,
        "mechanism": row.mechanism,
        "delta": row.delta_percent,
        "roofline-%": row.roofline_percent,
        "keep/revert": row.decision.as_str(),
        "workload": row.workload,
        "provisional?": row.provisional,
        "evidence path": row.evidence_path,
        "machine-state": machine_state_to_value(&row.machine_state),
        "status": status,
    })
}

fn machine_state_to_value(provenance: &MachineStateProvenance) -> Value {
    match provenance {
        MachineStateProvenance::DirectProbe => json!({"kind": "direct-probe"}),
        MachineStateProvenance::OperatorAttestation {
            timestamp,
            machine_identifier,
        } => json!({
            "kind": "operator-attestation",
            "timestamp": timestamp,
            "machine_identifier": machine_identifier,
        }),
    }
}

fn read_and_validate(path: &Path) -> Result<Vec<u8>, LedgerError> {
    let bytes = read_bytes(path)?;
    let _rows = parse_rows(&bytes)?;
    Ok(bytes)
}

fn read_bytes(path: &Path) -> Result<Vec<u8>, LedgerError> {
    fs::read(path).map_err(|error| LedgerError::Io {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })
}

fn parse_rows(bytes: &[u8]) -> Result<Vec<LedgerRow>, LedgerError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|error| LedgerError::CorruptHistory(error.to_string()))?;
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            let value: Value = serde_json::from_str(line).map_err(|error| {
                LedgerError::CorruptHistory(format!("line {}: {error}", index + 1))
            })?;
            value_to_row(&value).map_err(|error| {
                LedgerError::CorruptHistory(format!("line {}: {error}", index + 1))
            })
        })
        .collect()
}

fn value_to_row(value: &Value) -> Result<LedgerRow, String> {
    let decision = match string_field(value, "keep/revert")? {
        "keep" => LedgerDecision::Keep,
        "revert" => LedgerDecision::Revert,
        other => return Err(format!("unknown decision {other}")),
    };
    let status_value = value.get("status").ok_or("missing status")?;
    let status = match string_field(status_value, "kind")? {
        "frontier-open" => LedgerStatus::FrontierOpen {
            reason: string_field(status_value, "reason")?.to_owned(),
            hypotheses: string_array(status_value, "hypotheses")?,
        },
        "complete" => {
            let attribution = status_value
                .get("attribution")
                .ok_or("missing attribution")?;
            let counter_values = attribution
                .get("counters")
                .and_then(Value::as_array)
                .ok_or("missing counters")?;
            let mut counters = Vec::new();
            for counter in counter_values {
                counters.push(
                    CounterReading::new(
                        string_field(counter, "name")?,
                        number_field(counter, "value")?,
                        string_field(counter, "unit")?,
                        string_field(counter, "showed")?,
                    )
                    .map_err(|error| error.to_string())?,
                );
            }
            LedgerStatus::Complete {
                attribution: LedgerAttribution {
                    counters,
                    evidence_path: string_field(attribution, "evidence_path")?.to_owned(),
                    conclusion: string_field(attribution, "conclusion")?.to_owned(),
                },
            }
        }
        other => return Err(format!("unknown status {other}")),
    };
    let machine_state_value = value.get("machine-state").ok_or("missing machine-state")?;
    let machine_state = match string_field(machine_state_value, "kind")? {
        "direct-probe" => MachineStateProvenance::DirectProbe,
        "operator-attestation" => MachineStateProvenance::OperatorAttestation {
            timestamp: string_field(machine_state_value, "timestamp")?.to_owned(),
            machine_identifier: string_field(machine_state_value, "machine_identifier")?.to_owned(),
        },
        other => return Err(format!("unknown machine-state kind {other}")),
    };
    let row = LedgerRow {
        date: string_field(value, "date")?.to_owned(),
        hypothesis: string_field(value, "hypothesis")?.to_owned(),
        mechanism: string_field(value, "mechanism")?.to_owned(),
        delta_percent: number_field(value, "delta")?,
        roofline_percent: number_field(value, "roofline-%")?,
        decision,
        workload: string_field(value, "workload")?.to_owned(),
        provisional: value
            .get("provisional?")
            .and_then(Value::as_bool)
            .ok_or("missing provisional?")?,
        evidence_path: string_field(value, "evidence path")?.to_owned(),
        machine_state,
        status,
    };
    validate_row(&row).map_err(|error| error.to_string())?;
    Ok(row)
}

fn string_field<'a>(value: &'a Value, name: &str) -> Result<&'a str, String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string field {name}"))
}

fn number_field(value: &Value, name: &str) -> Result<f64, String> {
    value
        .get(name)
        .and_then(Value::as_f64)
        .ok_or_else(|| format!("missing number field {name}"))
}

fn string_array(value: &Value, name: &str) -> Result<Vec<String>, String> {
    value
        .get(name)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("missing string array {name}"))?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("non-string item in {name}"))
        })
        .collect()
}

fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}
