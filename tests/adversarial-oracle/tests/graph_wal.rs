use zeppelin_embed_adversarial_oracle::graph_wal::*;
fn state(seq: u64) -> State {
    State {
        store: (1u128 << 100) + 3,
        generation: seq,
        sequence: seq,
        high_waters: [5, 7, 1, 2, 3, 4, 9],
    }
}
#[test]
fn graph_wal_oracle_rejects_invented_partial_commits_and_fault_successes() {
    let commits = [(200, state(1)), (400, state(2))];
    assert!(check_prefix(&commits, 199, None, &Ok(vec![state(1)])).is_err());
    assert!(check_prefix(&commits, 400, None, &Ok(vec![state(1)])).is_err());
    assert!(check_prefix(&commits, 400, Some(300), &Ok(vec![state(1), state(2)])).is_err());
    assert!(check_prefix(&commits, 199, None, &Err(())).is_err());
    assert!(check_fault(&[state(1)], true, 0, &Err(())).is_err());
    assert!(check_fault(&[state(1)], true, 1, &Ok(vec![state(1)])).is_err());
    assert!(check_fault(&[state(1)], false, 1, &Ok(vec![state(1)])).is_err());
    assert!(check_fault(&[state(1)], false, 0, &Ok(vec![])).is_err());
}
#[test]
fn graph_wal_oracle_rejects_regression_wrap_and_wrong_store_acceptance() {
    let a = state(1);
    let mut b = state(2);
    b.high_waters[4] = 0;
    assert!(check_transition(&a, &b, true).is_err());
    b = state(2);
    b.store ^= 1u128 << 96;
    assert!(check_transition(&a, &b, true).is_err());
    let mut a = state(u64::MAX);
    a.generation = 1;
    let b = state(0);
    assert!(check_transition(&a, &b, true).is_err());
    assert!(check_transition(&state(1), &state(2), false).is_err());
}
#[test]
fn graph_wal_oracle_accepts_exact_complete_prefixes_and_observed_faults() {
    let commits = [(200, state(1)), (400, state(2))];
    for (available, values) in [
        (199, vec![]),
        (200, vec![state(1)]),
        (399, vec![state(1)]),
        (400, vec![state(1), state(2)]),
    ] {
        check_prefix(&commits, available, None, &Ok(values)).unwrap();
    }
    check_prefix(&commits, 400, Some(300), &Err(())).unwrap();
    check_transition(&state(1), &state(2), true).unwrap();
    let mut bad = state(2);
    bad.high_waters[0] = 0;
    check_transition(&state(1), &bad, false).unwrap();
    check_fault(&[state(1)], true, 1, &Err(())).unwrap();
    check_fault(&[state(1)], false, 0, &Ok(vec![state(1)])).unwrap();
}
