//! Independent feature-campaign oracle over primitive observations only.
//!
//! This module deliberately has no engine imports. Production code reports
//! facts; the oracle decides whether those facts satisfy a literal contract.

use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckerKind {
    ExactSequence,
    ExactSet,
    ExactScalar,
    Prefix,
    Bounded,
    Finite,
    TypedRefusal,
    Attribution,
    NoPartial,
    StableBits,
    Monotonic,
    Range,
    Parity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrimitiveObservation {
    pub expected_sequence: Vec<u128>,
    pub observed_sequence: Vec<u128>,
    pub expected_set: Vec<u128>,
    pub observed_set: Vec<u128>,
    pub expected_scalar: u64,
    pub observed_scalar: u64,
    pub maximum: u64,
    pub score_bits: Vec<u64>,
    pub expected_status: String,
    pub observed_status: String,
    pub expected_artifact: String,
    pub observed_artifact: String,
}

impl PrimitiveObservation {
    #[must_use]
    pub fn clean(sequence: Vec<u128>, score_bits: Vec<u64>, scalar: u64) -> Self {
        let mut set = sequence.clone();
        set.sort_unstable();
        set.dedup();
        Self {
            expected_sequence: sequence.clone(),
            observed_sequence: sequence,
            expected_set: set.clone(),
            observed_set: set,
            expected_scalar: scalar,
            observed_scalar: scalar,
            maximum: scalar,
            score_bits,
            expected_status: "typed-refusal".to_owned(),
            observed_status: "typed-refusal".to_owned(),
            expected_artifact: "observed-artifact".to_owned(),
            observed_artifact: "observed-artifact".to_owned(),
        }
    }
}

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

#[must_use]
pub fn compare(
    invariant: u8,
    checker_id: &'static str,
    operation: &'static str,
    checker: CheckerKind,
    facts: &PrimitiveObservation,
    provenance: impl Into<String>,
) -> OracleRecord {
    let (passed, expected, observed) = match checker {
        CheckerKind::ExactSequence | CheckerKind::StableBits | CheckerKind::Parity => (
            facts.expected_sequence == facts.observed_sequence,
            format!("sequence:{:?}", facts.expected_sequence),
            format!("sequence:{:?}", facts.observed_sequence),
        ),
        CheckerKind::ExactSet => {
            let expected = facts.expected_set.iter().copied().collect::<BTreeSet<_>>();
            let observed = facts.observed_set.iter().copied().collect::<BTreeSet<_>>();
            (
                expected == observed,
                format!("set:{expected:?}"),
                format!("set:{observed:?}"),
            )
        }
        CheckerKind::ExactScalar | CheckerKind::Monotonic => (
            facts.expected_scalar == facts.observed_scalar,
            format!("scalar:{}", facts.expected_scalar),
            format!("scalar:{}", facts.observed_scalar),
        ),
        CheckerKind::Prefix => (
            facts
                .expected_sequence
                .starts_with(&facts.observed_sequence),
            format!("legal-prefix-of:{:?}", facts.expected_sequence),
            format!("sequence:{:?}", facts.observed_sequence),
        ),
        CheckerKind::Bounded | CheckerKind::Range | CheckerKind::NoPartial => (
            facts.observed_scalar <= facts.maximum,
            format!("maximum:{}", facts.maximum),
            format!("scalar:{}", facts.observed_scalar),
        ),
        CheckerKind::Finite => {
            let finite = facts
                .score_bits
                .iter()
                .all(|bits| f64::from_bits(*bits).is_finite());
            (
                finite,
                "all-score-bits-finite".to_owned(),
                format!("score-bits:{:?}", facts.score_bits),
            )
        }
        CheckerKind::TypedRefusal => (
            facts.expected_status == facts.observed_status,
            format!("status:{}", facts.expected_status),
            format!("status:{}", facts.observed_status),
        ),
        CheckerKind::Attribution => (
            facts.expected_artifact == facts.observed_artifact,
            format!("artifact:{}", facts.expected_artifact),
            format!("artifact:{}", facts.observed_artifact),
        ),
    };
    OracleRecord {
        invariant,
        checker_id,
        operation,
        expected,
        observed,
        provenance: provenance.into(),
        passed,
        detail: if passed {
            "comparison succeeded".to_owned()
        } else {
            format!("I{invariant} {checker_id} comparison failed")
        },
    }
}

#[must_use]
pub fn planted(mut facts: PrimitiveObservation, checker: CheckerKind) -> PrimitiveObservation {
    match checker {
        CheckerKind::ExactSequence | CheckerKind::StableBits | CheckerKind::Parity => {
            facts.observed_sequence.push(u128::MAX);
        }
        CheckerKind::ExactSet => {
            facts.observed_set.push(u128::MAX);
        }
        CheckerKind::ExactScalar | CheckerKind::Monotonic => {
            facts.observed_scalar = facts.expected_scalar.saturating_add(1);
        }
        CheckerKind::Prefix => {
            facts.observed_sequence = vec![u128::MAX];
        }
        CheckerKind::Bounded | CheckerKind::Range | CheckerKind::NoPartial => {
            facts.observed_scalar = facts.maximum.saturating_add(1);
        }
        CheckerKind::Finite => facts.score_bits.push(f64::NAN.to_bits()),
        CheckerKind::TypedRefusal => facts.observed_status = "success".to_owned(),
        CheckerKind::Attribution => facts.observed_artifact = "sibling-artifact".to_owned(),
    }
    facts
}

fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}
