//! PG17 primitive whole-plan observations. This crate imports no engine or
//! compiler types, and constructs expectations from five fixed query recipes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Observation {
    pub operators: Vec<String>,
    pub columns: Vec<(String, u32, u16)>,
    pub parameters: Vec<(String, i64)>,
    pub root: u32,
    pub ordered: bool,
}
fn lines(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).into()).collect()
}
pub fn expected(case: usize, alias: &str, value: i64) -> Observation {
    match case {
        0 => Observation {
            operators: lines(&["[] unit", "[0] project 0=$0,1=[1,\"λ\"]"]),
            columns: vec![(alias.into(), 0, 4), ("list".into(), 1, 128)],
            parameters: vec![("value".into(), value)],
            root: 1,
            ordered: false,
        },
        1 => Observation {
            operators: lines(&[
                "[] unit",
                "[0] scan 0",
                "[1] filter label(s0,\"A\")",
                "[2] expand 0->2 rel1 Outgoing types[\"R\", \"S\"] pattern0",
                "[3] filter IsNotNull(s2)",
                "[4] path 2->4 rels3 Outgoing types[\"P\", \"Q\"] pattern1 0..2 edge10=Comparison(Equal)(prop(s10,\"weight\"),$0)",
                "[3, 5] optional Comparison(Equal)(prop(s4,\"ok\"),true)",
                "[6] project 5=s0,6=s1,7=s2,8=s3,9=s4",
            ]),
            columns: vec![
                ("a".into(), 5, 32),
                ("r".into(), 6, 64),
                ("b".into(), 7, 32),
                ("p".into(), 8, 129),
                ("c".into(), 9, 33),
            ],
            parameters: vec![("v".into(), value)],
            root: 7,
            ordered: false,
        },
        2 => Observation {
            operators: lines(&[
                "[] unit",
                "[0] scan 0",
                "[1] project 1=prop(s0,\"p\"),0=s0",
                "[2] sort prop(s0,\"q\"):desc",
                "[3] bound 1 3",
                "[4] project 1=s1",
            ]),
            columns: vec![("x".into(), 1, 159)],
            parameters: vec![("limit".into(), 3)],
            root: 5,
            ordered: true,
        },
        3 => Observation {
            operators: lines(&[
                "[] unit",
                "[0] scan 0",
                "[1] aggregate keys2=prop(s0,\"p\") values1=Count { distinct: false }(*)",
                "[2] project 1=s1,2=s2",
                "[3] sort s2:asc",
                "[4] with 1=s1,2=s2",
                "[5] project 3=s1,4=s2",
            ]),
            columns: vec![("c".into(), 3, 4), ("p".into(), 4, 159)],
            parameters: vec![],
            root: 6,
            ordered: true,
        },
        4 => Observation {
            operators: lines(&[
                "[] unit",
                "[0] scan 0",
                "[1] path 0->2 rels1 Outgoing types[\"R\"] pattern0 0..2 none completed4=And(Comparison(Equal)(prop(s4,\"x\"),Size(s1)),Comparison(Equal)(prop(s4,\"y\"),7))",
                "[2] project 3=s1",
            ]),
            columns: vec![("r".into(), 3, 128)],
            parameters: vec![],
            root: 3,
            ordered: false,
        },
        _ => unreachable!("PG17 fixed primitive query recipe"),
    }
}
pub fn check(case: usize, alias: &str, value: i64, observed: &Observation) -> Result<(), String> {
    let expected = expected(case, alias, value);
    if expected != *observed {
        return Err(format!(
            "PG17 case{case} expected={expected:#?}\nobserved={observed:#?}"
        ));
    }
    Ok(())
}
pub fn check_failure(
    success: bool,
    fires: usize,
    calls: usize,
    retained_delta: usize,
    final_handoff: bool,
) -> Result<(), String> {
    if success || fires != 1 || calls != usize::from(final_handoff) || retained_delta != 0 {
        return Err(format!(
            "PG17 failure: success={success} fires={fires} calls={calls} retained={retained_delta}"
        ));
    }
    Ok(())
}
