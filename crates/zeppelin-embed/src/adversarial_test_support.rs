//! Hidden receipt boundary for test-only production fault controllers.
//!
//! Product modules create receipts at the operation that consumes an armed
//! test fault. External harnesses can inspect and serialize facts but cannot
//! construct receipts or inject faults through the normal production API.

/// Fact-only proof that a production operation consumed one test fault.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeatureFaultReceipt {
    campaign: &'static str,
    operation: &'static str,
    fault: &'static str,
    site: &'static str,
    cardinality: u32,
    effect: String,
}

impl FeatureFaultReceipt {
    #[allow(dead_code, reason = "family production controllers land in later vertical slices")]
    pub(crate) fn new(
        campaign: &'static str,
        operation: &'static str,
        fault: &'static str,
        site: &'static str,
        cardinality: u32,
        effect: String,
    ) -> Self {
        Self {
            campaign,
            operation,
            fault,
            site,
            cardinality,
            effect,
        }
    }

    /// Campaign that owns the receipt.
    #[must_use]
    pub const fn campaign(&self) -> &'static str {
        self.campaign
    }

    /// Typed production operation that consumed the fault.
    #[must_use]
    pub const fn operation(&self) -> &'static str {
        self.operation
    }

    /// Exact selected fault variant.
    #[must_use]
    pub const fn fault(&self) -> &'static str {
        self.fault
    }

    /// Exact production checkpoint or subsite.
    #[must_use]
    pub const fn site(&self) -> &'static str {
        self.site
    }

    /// Number of times the selected fault was consumed.
    #[must_use]
    pub const fn cardinality(&self) -> u32 {
        self.cardinality
    }

    /// Canonical fact-only description of the observed effect.
    #[must_use]
    pub fn effect(&self) -> &str {
        &self.effect
    }
}

#[cfg(test)]
mod tests {
    use super::FeatureFaultReceipt;

    #[test]
    fn receipt_preserves_every_origin_fact() {
        let receipt = FeatureFaultReceipt::new(
            "storage-durability",
            "wal-prefix",
            "torn-wal-body",
            "wal.append.body",
            1,
            "written=7 requested=16".to_owned(),
        );
        assert_eq!(receipt.campaign(), "storage-durability");
        assert_eq!(receipt.operation(), "wal-prefix");
        assert_eq!(receipt.fault(), "torn-wal-body");
        assert_eq!(receipt.site(), "wal.append.body");
        assert_eq!(receipt.cardinality(), 1);
        assert_eq!(receipt.effect(), "written=7 requested=16");
    }
}
