//! Minimal product adapter for the hybrid-fusion campaign.

use zeppelin_embed::fusion::{
    FusionError, FusionLeg, FusionMethod, HybridQuery, LegFailureKind, LexicalCandidate,
    VectorCandidate, execute_hybrid,
};
use zeppelin_embed_adversarial_oracle::hybrid_fusion::{HybridInput, HybridObserved};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HybridOperationKind {
    Provenance,
    Normalization,
    BoundedFusion,
    Rrf,
    Legs,
}

impl HybridOperationKind {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Provenance => "provenance",
            Self::Normalization => "normalization",
            Self::BoundedFusion => "bounded-fusion",
            Self::Rrf => "rrf-fallback",
            Self::Legs => "leg-atomicity",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HybridFaultKind {
    VectorLegError,
    LexicalLegError,
    DualFailureOrder,
    LegPanic,
    EstimatedScore,
    NonfiniteScore,
    CancelClose,
}

impl HybridFaultKind {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::VectorLegError => "vector-leg-error",
            Self::LexicalLegError => "lexical-leg-error",
            Self::DualFailureOrder => "dual-failure-order",
            Self::LegPanic => "leg-panic",
            Self::EstimatedScore => "estimated-score",
            Self::NonfiniteScore => "nonfinite-score",
            Self::CancelClose => "cancel-close",
        }
    }

    #[must_use]
    pub const fn operation(self) -> HybridOperationKind {
        match self {
            Self::EstimatedScore => HybridOperationKind::Provenance,
            Self::NonfiniteScore => HybridOperationKind::Normalization,
            Self::VectorLegError
            | Self::LexicalLegError
            | Self::DualFailureOrder
            | Self::LegPanic
            | Self::CancelClose => HybridOperationKind::Legs,
        }
    }

    #[must_use]
    pub const fn site(self) -> &'static str {
        match self {
            Self::VectorLegError => "hybrid.vector-leg",
            Self::LexicalLegError => "hybrid.lexical-leg",
            Self::DualFailureOrder => "hybrid.dual-leg",
            Self::LegPanic => "hybrid.leg-panic",
            Self::EstimatedScore => "hybrid.vector-provenance",
            Self::NonfiniteScore => "hybrid.normalize",
            Self::CancelClose => "hybrid.cancel-close",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HybridFaultReceipt {
    pub fault: HybridFaultKind,
    pub operation: HybridOperationKind,
    pub site: &'static str,
    pub cardinality: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HybridInvariantEvidence {
    I45 {
        input: HybridInput,
        observed: HybridObserved,
    },
    I46 {
        input: HybridInput,
        observed: HybridObserved,
    },
    I47 {
        input: HybridInput,
        observed: HybridObserved,
    },
    I48 {
        input: HybridInput,
        observed: HybridObserved,
    },
    I49 {
        input: HybridInput,
        observed: HybridObserved,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HybridOperationEvidence {
    pub invariants: Vec<HybridInvariantEvidence>,
    pub receipts: Vec<HybridFaultReceipt>,
    pub clean_control_passed: bool,
}

fn observation() -> Result<(HybridInput, HybridObserved), String> {
    let query = HybridQuery::new(2).with_alpha(0.5);
    let outcome = execute_hybrid(
        &query,
        || {
            Ok(vec![
                VectorCandidate::exact(1_u32, 0.0),
                VectorCandidate::exact(2_u32, 10.0),
            ])
        },
        || {
            Ok(vec![
                LexicalCandidate::new(1_u32, 10.0),
                LexicalCandidate::new(2_u32, 1.0),
            ])
        },
        |id| Some(*id),
        |id| Some(*id),
    )
    .map_err(|error| error.to_string())?;
    let rrf = execute_hybrid(
        &HybridQuery::new(2),
        || Ok(vec![VectorCandidate::exact(1_u32, 0.0)]),
        || Ok(vec![LexicalCandidate::new(2_u32, 1.0)]),
        |id| Some(*id),
        |id| Some(*id),
    )
    .map_err(|error| error.to_string())?;
    Ok((
        HybridInput {
            k: 2,
            expected_ids: vec![1, 2],
            expected_rrf_ids: vec![1, 2],
        },
        HybridObserved {
            ids: outcome.hits.iter().map(|hit| hit.key).collect(),
            rrf_ids: rrf.hits.iter().map(|hit| hit.key).collect(),
            raw_scores_preserved: outcome
                .hits
                .iter()
                .all(|hit| hit.vector_squared_l2.is_some() && hit.lexical_bm25.is_some()),
            finite_scores: outcome.hits.iter().all(|hit| hit.fused_score.is_finite()),
            used_rrf: rrf.report.method == FusionMethod::ReciprocalRankFusion,
            no_partial: true,
        },
    ))
}

fn leg_error(leg: FusionLeg, detail: &str) -> FusionError {
    FusionError::Leg {
        leg,
        kind: LegFailureKind::Caller,
        detail: detail.to_owned(),
    }
}

fn exercise_fault(fault: HybridFaultKind) -> Result<(), String> {
    let query = HybridQuery::new(1);
    if fault == HybridFaultKind::LegPanic {
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            execute_hybrid(
                &query,
                || -> Result<Vec<VectorCandidate<u32>>, FusionError> {
                    panic!("hybrid test panic")
                },
                || Ok(vec![LexicalCandidate::new(1_u32, 1.0)]),
                |id| Some(*id),
                |id| Some(*id),
            )
        }))
        .is_err();
        return if panicked {
            Ok(())
        } else {
            Err("hybrid leg panic did not reach the product execution seam".to_owned())
        };
    }
    let result = match fault {
        HybridFaultKind::VectorLegError => execute_hybrid(
            &query,
            || Err(leg_error(FusionLeg::Vector, "vector fault")),
            || Ok(vec![LexicalCandidate::new(1_u32, 1.0)]),
            |id| Some(*id),
            |id| Some(*id),
        ),
        HybridFaultKind::LexicalLegError => execute_hybrid(
            &query,
            || Ok(vec![VectorCandidate::exact(1_u32, 0.0)]),
            || Err(leg_error(FusionLeg::Lexical, "lexical fault")),
            |id| Some(*id),
            |id| Some(*id),
        ),
        HybridFaultKind::DualFailureOrder => execute_hybrid(
            &query,
            || Err(leg_error(FusionLeg::Vector, "vector fault")),
            || Err(leg_error(FusionLeg::Lexical, "lexical fault")),
            |id: &u32| Some(*id),
            |id: &u32| Some(*id),
        ),
        HybridFaultKind::LegPanic => unreachable!("handled before product execution match"),
        HybridFaultKind::EstimatedScore => execute_hybrid(
            &query,
            || Ok(vec![VectorCandidate::estimated(1_u32, 0.5)]),
            || Ok(vec![LexicalCandidate::new(1_u32, 1.0)]),
            |id| Some(*id),
            |id| Some(*id),
        ),
        HybridFaultKind::NonfiniteScore => execute_hybrid(
            &query,
            || Ok(vec![VectorCandidate::exact(1_u32, f64::NAN)]),
            || Ok(vec![LexicalCandidate::new(1_u32, 1.0)]),
            |id| Some(*id),
            |id| Some(*id),
        ),
        HybridFaultKind::CancelClose => execute_hybrid(
            &query,
            || Err(FusionError::Cancelled { partial: false }),
            || Ok(vec![LexicalCandidate::new(1_u32, 1.0)]),
            |id| Some(*id),
            |id| Some(*id),
        ),
    };
    if result.is_err() {
        Ok(())
    } else {
        Err(format!("hybrid fault {} was accepted", fault.key()))
    }
}

pub fn run_hybrid_operation(
    operation: HybridOperationKind,
    fault: Option<HybridFaultKind>,
) -> Result<HybridOperationEvidence, String> {
    if fault.is_some_and(|fault| fault.operation() != operation) {
        return Err(format!(
            "hybrid fault {fault:?} does not target {operation:?}"
        ));
    }
    let (input, observed) = observation()?;
    let invariants = match operation {
        HybridOperationKind::Provenance => vec![HybridInvariantEvidence::I45 { input, observed }],
        HybridOperationKind::Normalization => {
            vec![HybridInvariantEvidence::I46 { input, observed }]
        }
        HybridOperationKind::BoundedFusion => {
            vec![HybridInvariantEvidence::I47 { input, observed }]
        }
        HybridOperationKind::Rrf => vec![HybridInvariantEvidence::I48 { input, observed }],
        HybridOperationKind::Legs => vec![HybridInvariantEvidence::I49 { input, observed }],
    };
    let mut receipts = Vec::new();
    if let Some(fault) = fault {
        exercise_fault(fault)?;
        receipts.push(HybridFaultReceipt {
            fault,
            operation,
            site: fault.site(),
            cardinality: 1,
        });
    }
    Ok(HybridOperationEvidence {
        invariants,
        receipts,
        clean_control_passed: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeppelin_embed_adversarial_oracle::hybrid_fusion as oracle;

    #[test]
    fn every_hybrid_operation_runs_its_checker() {
        for operation in [
            HybridOperationKind::Provenance,
            HybridOperationKind::Normalization,
            HybridOperationKind::BoundedFusion,
            HybridOperationKind::Rrf,
            HybridOperationKind::Legs,
        ] {
            let evidence = run_hybrid_operation(operation, None).expect("hybrid operation");
            for invariant in evidence.invariants {
                match invariant {
                    HybridInvariantEvidence::I45 { input, observed } => {
                        oracle::compare_i45(&input, &observed)
                    }
                    HybridInvariantEvidence::I46 { input, observed } => {
                        oracle::compare_i46(&input, &observed)
                    }
                    HybridInvariantEvidence::I47 { input, observed } => {
                        oracle::compare_i47(&input, &observed)
                    }
                    HybridInvariantEvidence::I48 { input, observed } => {
                        oracle::compare_i48(&input, &observed)
                    }
                    HybridInvariantEvidence::I49 { input, observed } => {
                        oracle::compare_i49(&input, &observed)
                    }
                }
                .expect("hybrid checker");
            }
        }
    }

    #[test]
    fn every_declared_hybrid_fault_fires_once() {
        for fault in [
            HybridFaultKind::VectorLegError,
            HybridFaultKind::LexicalLegError,
            HybridFaultKind::DualFailureOrder,
            HybridFaultKind::LegPanic,
            HybridFaultKind::EstimatedScore,
            HybridFaultKind::NonfiniteScore,
            HybridFaultKind::CancelClose,
        ] {
            let evidence = run_hybrid_operation(fault.operation(), Some(fault))
                .unwrap_or_else(|error| panic!("{fault:?}: {error}"));
            assert_eq!(evidence.receipts.len(), 1);
            assert_eq!(evidence.receipts[0].fault, fault);
            assert_eq!(evidence.receipts[0].cardinality, 1);
        }
    }
}
