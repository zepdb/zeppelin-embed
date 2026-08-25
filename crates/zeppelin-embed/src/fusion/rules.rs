use super::{DEFAULT_ALPHA, FusionError};

/// Version of the rule data interpreted by fusion reports.
pub const ALPHA_POLICY_VERSION: u16 = 1;

/// PLACEHOLDER -- NOT YET MEASURED.
///
/// Alpha selected when any exact-match rule shifts weight toward lexical.
pub const LEXICAL_RULE_ALPHA: f64 = 0.4;

/// PLACEHOLDER -- NOT YET MEASURED.
///
/// A query token at or below this document frequency is considered rare.
pub const RARE_DOCUMENT_FREQUENCY_THRESHOLD: u64 = 5;

/// Stable rule names emitted by the fusion report.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum FusionRule {
    /// Query contains a quoted phrase.
    QuotedPhrase,
    /// Query contains an exact token at or below the rarity threshold.
    RareExactToken,
    /// Query contains an identifier-class token.
    IdentifierToken,
}

/// Caller-computed deterministic query-shape facts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RuleSignals {
    /// Whether the structured query contains a quoted phrase.
    pub quoted_phrase: bool,
    /// Lowest exact-token document frequency, absent when no exact token exists.
    pub rarest_exact_document_frequency: Option<u64>,
    /// Whether token classification found an identifier.
    pub identifier_token: bool,
}

impl RuleSignals {
    /// Constructs a signal set with no matching rules.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            quoted_phrase: false,
            rarest_exact_document_frequency: None,
            identifier_token: false,
        }
    }
}

pub(crate) fn effective_alpha(
    query: &super::HybridQuery,
) -> Result<(f64, Vec<FusionRule>), FusionError> {
    if let Some(alpha) = query.alpha {
        validate(alpha)?;
        return Ok((alpha, Vec::new()));
    }
    validate(DEFAULT_ALPHA)?;
    if !query.rules_enabled {
        return Ok((DEFAULT_ALPHA, Vec::new()));
    }
    let mut applied = Vec::new();
    if query.rule_signals.quoted_phrase {
        applied.push(FusionRule::QuotedPhrase);
    }
    if query
        .rule_signals
        .rarest_exact_document_frequency
        .is_some_and(|frequency| frequency <= RARE_DOCUMENT_FREQUENCY_THRESHOLD)
    {
        applied.push(FusionRule::RareExactToken);
    }
    if query.rule_signals.identifier_token {
        applied.push(FusionRule::IdentifierToken);
    }
    if applied.is_empty() {
        Ok((DEFAULT_ALPHA, applied))
    } else {
        Ok((LEXICAL_RULE_ALPHA, applied))
    }
}

fn validate(alpha: f64) -> Result<(), FusionError> {
    if alpha.is_finite() && (0.0..=1.0).contains(&alpha) {
        Ok(())
    } else {
        Err(FusionError::InvalidAlpha(alpha))
    }
}
