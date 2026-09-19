//! PG11 independent primitive binding observations. No parser, AST, engine
//! value/type predicate, scope helper, or runtime admission is imported.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Kind {
    Integer,
    Float,
    Text,
    Node,
    NullableNode,
    List,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Column {
    pub name: String,
    pub kind: Kind,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Outcome {
    Bound,
    Unknown,
    Duplicate,
    Type,
    Unsupported,
    Search,
    Deleted,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Observation {
    pub outcome: Outcome,
    pub columns: Vec<Column>,
    pub modes: Vec<u8>,
    pub parameter_bits: Vec<u64>,
    pub consumer_calls: usize,
}
#[derive(Clone, Copy, Debug)]
pub enum Case {
    Alias,
    Optional,
    Path,
    Writes,
    Search,
    Parameter,
    Unknown,
    Duplicate,
    WrongType,
    Unsupported,
    BadVector,
    Deleted,
}
pub const CASES: &[Case] = &[
    Case::Alias,
    Case::Optional,
    Case::Path,
    Case::Writes,
    Case::Search,
    Case::Parameter,
    Case::Unknown,
    Case::Duplicate,
    Case::WrongType,
    Case::Unsupported,
    Case::BadVector,
    Case::Deleted,
];
/// The fixture model explicitly states output declaration order, exact types,
/// mode distinction and phase; it never parses or calls the product binder.
pub fn check(
    case: Case,
    name: &str,
    bits: u64,
    mode: u8,
    observed: &Observation,
) -> Result<(), String> {
    let (outcome, columns, modes, parameter_bits) = match case {
        Case::Alias => (
            Outcome::Bound,
            vec![(name, Kind::Integer), ("text", Kind::Text)],
            vec![],
            vec![],
        ),
        Case::Optional => (
            Outcome::Bound,
            vec![("base", Kind::Node), ("maybe", Kind::NullableNode)],
            vec![],
            vec![],
        ),
        Case::Path => (Outcome::Bound, vec![("edges", Kind::List)], vec![], vec![]),
        Case::Writes => (
            Outcome::Bound,
            vec![("total", Kind::Integer)],
            vec![],
            vec![],
        ),
        Case::Search => (
            Outcome::Bound,
            vec![("hit", Kind::Node), ("score", Kind::Float)],
            vec![mode],
            vec![],
        ),
        Case::Parameter => (
            Outcome::Bound,
            vec![(name, Kind::Float)],
            vec![],
            vec![bits],
        ),
        Case::Unknown => (Outcome::Unknown, vec![], vec![], vec![]),
        Case::Duplicate => (Outcome::Duplicate, vec![], vec![], vec![]),
        Case::WrongType => (Outcome::Type, vec![], vec![], vec![]),
        Case::Unsupported => (Outcome::Unsupported, vec![], vec![], vec![]),
        Case::BadVector => (Outcome::Search, vec![], vec![], vec![]),
        Case::Deleted => (Outcome::Deleted, vec![], vec![], vec![]),
    };
    let consumer_calls = usize::from(outcome == Outcome::Bound);
    let expected = Observation {
        outcome,
        columns: columns
            .into_iter()
            .map(|(name, kind)| Column {
                name: name.into(),
                kind,
            })
            .collect(),
        modes,
        parameter_bits,
        consumer_calls,
    };
    if observed != &expected {
        return Err(format!(
            "PG11 {case:?}: expected {expected:?}, observed {observed:?}"
        ));
    }
    Ok(())
}
pub fn check_fault(
    cancelled: bool,
    expected_cancelled: bool,
    fires: usize,
    consumer_calls: usize,
) -> Result<(), String> {
    if cancelled != expected_cancelled || fires != 1 || consumer_calls != 0 {
        return Err(format!(
            "PG11 unproved fault: cancel={cancelled} expected={expected_cancelled} fires={fires} calls={consumer_calls}"
        ));
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn binding_oracle_rejects_wrong_order_types_bits_modes_and_admission() {
        let correct = Observation {
            outcome: Outcome::Bound,
            columns: vec![
                Column {
                    name: "x".into(),
                    kind: Kind::Integer,
                },
                Column {
                    name: "text".into(),
                    kind: Kind::Text,
                },
            ],
            modes: vec![],
            parameter_bits: vec![],
            consumer_calls: 1,
        };
        assert!(check(Case::Alias, "x", 0, 0, &correct).is_ok());
        let mut bad = correct.clone();
        bad.columns.reverse();
        assert!(check(Case::Alias, "x", 0, 0, &bad).is_err());
        bad = correct.clone();
        bad.columns[0].kind = Kind::Float;
        assert!(check(Case::Alias, "x", 0, 0, &bad).is_err());
        bad = correct;
        bad.consumer_calls = 0;
        assert!(check(Case::Alias, "x", 0, 0, &bad).is_err());
        let mut parameter = Observation {
            outcome: Outcome::Bound,
            columns: vec![Column {
                name: "x".into(),
                kind: Kind::Float,
            }],
            modes: vec![],
            parameter_bits: vec![0x7ff8000000000055],
            consumer_calls: 1,
        };
        assert!(check(Case::Parameter, "x", 0x7ff8000000000055, 0, &parameter).is_ok());
        parameter.parameter_bits[0] = 0x7ff8000000000000;
        assert!(check(Case::Parameter, "x", 0x7ff8000000000055, 0, &parameter).is_err());
        let mut search = Observation {
            outcome: Outcome::Bound,
            columns: vec![
                Column {
                    name: "hit".into(),
                    kind: Kind::Node,
                },
                Column {
                    name: "score".into(),
                    kind: Kind::Float,
                },
            ],
            modes: vec![0],
            parameter_bits: vec![],
            consumer_calls: 1,
        };
        assert!(check(Case::Search, "x", 0, 0, &search).is_ok());
        search.modes[0] = 1;
        assert!(check(Case::Search, "x", 0, 0, &search).is_err());
        assert!(check_fault(true, true, 1, 0).is_ok());
        assert!(check_fault(false, true, 1, 0).is_err());
        assert!(check_fault(true, true, 0, 0).is_err());
        assert!(check_fault(true, true, 1, 1).is_err());
    }
}
