use super::{DegenerateKind, DegenerateLeg, FusionLeg, JoinedLexical, JoinedVector};

#[derive(Clone, Copy)]
pub(crate) struct ScoreRange {
    minimum: f64,
    maximum: f64,
    constant: Option<f64>,
}

impl ScoreRange {
    /// Builds a range from exact producer-supplied extremes. Equal extremes
    /// are not a range: the leg is degenerate and falls back to rank fusion,
    /// exactly as an all-equal complete list does.
    pub(crate) fn explicit(minimum: f64, maximum: f64) -> Option<Self> {
        (minimum != maximum).then_some(Self {
            minimum,
            maximum,
            constant: None,
        })
    }

    pub(crate) fn fixed_zero(maximum: f64, zero_width_score: f64) -> Self {
        Self {
            minimum: 0.0,
            maximum,
            constant: (maximum == 0.0).then_some(zero_width_score),
        }
    }

    pub(crate) fn vector(self, squared_l2: f64) -> f64 {
        self.constant
            .unwrap_or_else(|| (self.maximum - squared_l2) / (self.maximum - self.minimum))
    }

    pub(crate) fn lexical(self, bm25: f64) -> f64 {
        self.constant
            .unwrap_or_else(|| (bm25 - self.minimum) / (self.maximum - self.minimum))
    }
}

pub(crate) fn vector_range<K>(hits: &[JoinedVector<K>]) -> Option<ScoreRange> {
    let first = hits.first()?.squared_l2;
    let last = hits.last()?.squared_l2;
    (first != last).then_some(ScoreRange {
        minimum: first,
        maximum: last,
        constant: None,
    })
}

pub(crate) fn lexical_range<K>(hits: &[JoinedLexical<K>]) -> Option<ScoreRange> {
    let first = hits.first()?.bm25;
    let last = hits.last()?.bm25;
    (first != last).then_some(ScoreRange {
        minimum: last,
        maximum: first,
        constant: None,
    })
}

pub(crate) fn degeneracies<K>(
    vector: &[JoinedVector<K>],
    lexical: &[JoinedLexical<K>],
) -> Vec<DegenerateLeg> {
    let mut reasons = Vec::new();
    if let Some(kind) = degenerate_vector(vector) {
        reasons.push(DegenerateLeg {
            leg: FusionLeg::Vector,
            kind,
        });
    }
    if let Some(kind) = degenerate_lexical(lexical) {
        reasons.push(DegenerateLeg {
            leg: FusionLeg::Lexical,
            kind,
        });
    }
    reasons
}

fn degenerate_vector<K>(hits: &[JoinedVector<K>]) -> Option<DegenerateKind> {
    match hits {
        [] => Some(DegenerateKind::Empty),
        [_] => Some(DegenerateKind::SingleHit),
        many if many.first().map(|hit| hit.squared_l2) == many.last().map(|hit| hit.squared_l2) => {
            Some(DegenerateKind::AllScoresEqual)
        }
        _ => None,
    }
}

fn degenerate_lexical<K>(hits: &[JoinedLexical<K>]) -> Option<DegenerateKind> {
    match hits {
        [] => Some(DegenerateKind::Empty),
        [_] => Some(DegenerateKind::SingleHit),
        many if many.first().map(|hit| hit.bm25) == many.last().map(|hit| hit.bm25) => {
            Some(DegenerateKind::AllScoresEqual)
        }
        _ => None,
    }
}
