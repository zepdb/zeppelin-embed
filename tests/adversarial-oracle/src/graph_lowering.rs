//! PG17 primitive whole-plan observations. This crate imports no engine or
//! compiler types, and constructs expectations from six fixed query recipes.
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
            operators: lines(&[
                "[] unit",
                "[0] project 0=$0,1=[1,\"λ\"],2=case($0;0,coalesce(null,1),1,2;case(none;true,coalesce(null,$0);3))",
            ]),
            columns: vec![
                (alias.into(), 0, 4),
                ("list".into(), 1, 128),
                ("choice".into(), 2, 5),
            ],
            parameters: vec![("value".into(), value)],
            root: 1,
            ordered: false,
        },
        1 => Observation {
            operators: lines(&[
                "[] unit",
                "[0] scan 0 label\"A\"",
                "[1] expand 0->2 rel1 Outgoing types[\"R\", \"S\"] pattern0",
                "[2] filter IsNotNull(s2)",
                "[3] path 2->4 rels3 Outgoing types[\"P\", \"Q\"] pattern1 0..2 edge10=Comparison(Equal)(prop(s10,\"weight\"),$0)",
                "[2, 4] optional Comparison(Equal)(prop(s4,\"ok\"),true)",
                "[5] project 5=s0,6=s1,7=s2,8=s3,9=s4",
            ]),
            columns: vec![
                ("a".into(), 5, 32),
                ("r".into(), 6, 64),
                ("b".into(), 7, 32),
                ("p".into(), 8, 129),
                ("c".into(), 9, 33),
            ],
            parameters: vec![("v".into(), value)],
            root: 6,
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
        5 => Observation {
            operators: lines(&[
                "[] unit",
                "[0] scan 0",
                "[1] scan 1",
                "[2] filter IsNotNull(s0)",
                "[3] expand 0->6 rel2 Outgoing types[\"T\"] pattern1",
                "[4] filter Comparison(Equal)(s6,s1)",
                "[5] project 0=s0,1=s1,2=s2",
                "[6] bound 0 1",
                "[7] project 0=s0,1=s1,3=true",
                "[2, 8] optional none",
                "[9] filter Not(IsNotNull(s3))",
                "[10] project 4=s0,5=s1",
            ]),
            columns: vec![("a".into(), 4, 32), ("b".into(), 5, 32)],
            parameters: vec![],
            root: 11,
            ordered: false,
        },
        6 => Observation {
            operators: lines(&[
                "[] unit",
                "[0] project 1=comprehension(0;[1,2,null];Comparison(Greater)(local0,1);map(\"value\",local0))",
            ]),
            columns: vec![("values".into(), 1, 129)],
            parameters: vec![],
            root: 1,
            ordered: false,
        },
        7 => Observation {
            operators: lines(&[
                "[] unit",
                "[0] project 1=comprehension(0;[1,2];Comparison(Greater)(local0,1);none),3=comprehension(2;[1,2];none;Arithmetic(Multiply)(local2,2))",
            ]),
            columns: vec![("filtered".into(), 1, 129), ("mapped".into(), 3, 129)],
            parameters: vec![],
            root: 1,
            ordered: false,
        },
        8 => Observation {
            operators: lines(&[
                "[] unit",
                "[0] scan 0",
                "[1] aggregate keys values1=Avg { distinct: false }(prop(s0,\"v\"))",
                "[2] project 1=s1",
            ]),
            columns: vec![("mean".into(), 1, 9)],
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
