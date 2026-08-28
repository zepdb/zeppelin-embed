//! Independent, std-only expected-value oracles for feature campaigns.
//!
//! The package consumes primitive fixture descriptions and observed facts. It
//! must never depend on engine crates or call production helpers.

#![forbid(unsafe_code)]
#![deny(warnings)]

pub mod fts;
pub mod ingest_retention;
pub mod metadata_filter_planner;
pub mod storage_durability;
pub mod vamana_graph;
pub mod vector_execution;

/// Append-only contract version attested by episode and campaign evidence.
pub const ORACLE_CONTRACT_VERSION: u32 = 1;

/// Stable structured description of the first exact comparator disagreement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OracleFirstDifference {
    pub checker_id: &'static str,
    pub path: String,
    pub kind: String,
    pub row: Option<u64>,
    pub expected: String,
    pub observed: String,
}

impl OracleFirstDifference {
    fn json(&self) -> String {
        let row = self
            .row
            .map_or_else(|| "null".to_owned(), |row| row.to_string());
        format!(
            "{{\"checker_id\":\"{}\",\"path\":\"{}\",\"kind\":\"{}\",\"row\":{row},\"expected\":\"{}\",\"observed\":\"{}\"}}",
            escape(self.checker_id),
            escape(&self.path),
            escape(&self.kind),
            escape(&self.expected),
            escape(&self.observed),
        )
    }
}

/// Stable result emitted by one exact invariant checker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OracleRecord {
    pub invariant: u8,
    pub checker_id: &'static str,
    pub operation: &'static str,
    /// Stable family-owned case identity when one operation emits a matrix.
    pub case_identity: Option<String>,
    /// Canonical JSON value encoding of the complete independent expectation.
    pub expected: String,
    /// Canonical JSON value encoding of the complete production observation.
    pub observed: String,
    /// Digest of the canonical independent input/expectation JSON bytes.
    pub input_digest: String,
    /// Digest of the canonical production-observation JSON bytes.
    pub observed_digest: String,
    /// Family-owned canonical primitive encoding version.
    pub canonical_version: u32,
    /// Digest produced by the independent family oracle over its primitive input.
    pub oracle_input_digest: String,
    /// Digest produced by the independent family oracle over its primitive observation.
    pub oracle_observed_digest: String,
    /// Hex-encoded family canonical input bytes retained for strict digest replay.
    pub oracle_input_bytes: String,
    /// Hex-encoded family canonical observation bytes retained for strict digest replay.
    pub oracle_observed_bytes: String,
    pub provenance: String,
    pub passed: bool,
    /// Exact first comparator difference, absent only when the comparison passed.
    pub first_difference: Option<OracleFirstDifference>,
    pub detail: String,
}

impl OracleRecord {
    #[must_use]
    pub fn json_line(&self) -> String {
        let first_difference = self
            .first_difference
            .as_ref()
            .map_or_else(|| "null".to_owned(), OracleFirstDifference::json);
        let case_identity = self.case_identity.as_ref().map_or_else(
            || "null".to_owned(),
            |identity| format!("\"{}\"", escape(identity)),
        );
        format!(
            "{{\"invariant\":\"I{}\",\"checker_id\":\"{}\",\"operation\":\"{}\",\"case_identity\":{case_identity},\"expected\":{},\"observed\":{},\"input_digest\":\"{}\",\"observed_digest\":\"{}\",\"canonical_version\":{},\"oracle_input_digest\":\"{}\",\"oracle_observed_digest\":\"{}\",\"oracle_input_bytes\":\"{}\",\"oracle_observed_bytes\":\"{}\",\"provenance\":\"{}\",\"passed\":{},\"first_difference\":{},\"detail\":\"{}\"}}",
            self.invariant,
            escape(self.checker_id),
            escape(self.operation),
            self.expected,
            self.observed,
            escape(&self.input_digest),
            escape(&self.observed_digest),
            self.canonical_version,
            escape(&self.oracle_input_digest),
            escape(&self.oracle_observed_digest),
            escape(&self.oracle_input_bytes),
            escape(&self.oracle_observed_bytes),
            escape(&self.provenance),
            self.passed,
            first_difference,
            escape(&self.detail),
        )
    }
}

fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}
