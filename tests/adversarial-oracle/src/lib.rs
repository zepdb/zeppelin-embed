//! Independent, std-only expected-value oracles for feature campaigns.
//!
//! The package consumes primitive fixture descriptions and observed facts. It
//! must never depend on engine crates or call production helpers.

#![forbid(unsafe_code)]

/// Append-only contract version attested by episode and campaign evidence.
pub const ORACLE_CONTRACT_VERSION: u32 = 1;

/// Stable result emitted by one exact invariant checker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OracleRecord {
    pub invariant: u8,
    pub checker_id: &'static str,
    pub operation: &'static str,
    pub expected: String,
    pub observed: String,
    pub provenance: String,
    pub passed: bool,
    pub detail: String,
}

impl OracleRecord {
    #[must_use]
    pub fn json_line(&self) -> String {
        format!(
            "{{\"invariant\":\"I{}\",\"checker_id\":\"{}\",\"operation\":\"{}\",\"expected\":\"{}\",\"observed\":\"{}\",\"provenance\":\"{}\",\"passed\":{},\"detail\":\"{}\"}}",
            self.invariant,
            escape(self.checker_id),
            escape(self.operation),
            escape(&self.expected),
            escape(&self.observed),
            escape(&self.provenance),
            self.passed,
            escape(&self.detail),
        )
    }
}

fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}
