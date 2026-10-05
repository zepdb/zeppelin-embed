use zeppelin_embed_adversarial_oracle::graph_profile::*;
#[test]
fn ze74_detects_declared_semantic_mismatches() {
    let expected = Observation {
        columns: vec![("n".into(), "integer".into())],
        rows: vec![vec![Cell::Integer(7)], vec![Cell::Integer(7)]],
        ordered: false,
        error: None,
        disposition: "unchanged".into(),
        generation: 3,
        provenance: vec![],
        effects: [0; 8],
    };
    let mut bad = expected.clone();
    bad.rows.pop();
    assert!(
        compare_observation(&expected, &bad).is_err(),
        "lost duplicate"
    );
    bad = expected.clone();
    bad.rows[0][0] = Cell::Float(7_f64.to_bits());
    assert!(
        compare_observation(&expected, &bad).is_err(),
        "integer to float"
    );
    bad = expected.clone();
    bad.columns[0].1 = "float".into();
    assert!(
        compare_observation(&expected, &bad).is_err(),
        "wrong column kind"
    );
    bad = expected.clone();
    bad.error = Some(("unsupported".into(), "execution".into()));
    assert!(
        compare_observation(&expected, &bad).is_err(),
        "wrong error/stage"
    );
    let mut rejected = expected.clone();
    rejected.error = Some(("unsupported".into(), "compile".into()));
    let mut wrong_stage = rejected.clone();
    wrong_stage.error = Some(("unsupported".into(), "runtime".into()));
    assert!(
        compare_observation(&rejected, &wrong_stage).is_err(),
        "wrong stage"
    );
    bad = expected.clone();
    bad.effects[0] = 1;
    assert!(
        compare_observation(&expected, &bad).is_err(),
        "wrong final effect"
    );
    assert!(compare_observation(&expected, &expected).is_ok());
    let mut bag = expected.clone();
    bag.rows.push(vec![Cell::Integer(8)]);
    let mut reordered = bag.clone();
    reordered.rows.reverse();
    assert!(compare_observation(&bag, &reordered).is_ok());
    bag.ordered = true;
    reordered.ordered = true;
    assert!(compare_observation(&bag, &reordered).is_err());
}
