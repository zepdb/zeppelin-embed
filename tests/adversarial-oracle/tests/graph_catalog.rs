use zeppelin_embed_adversarial_oracle::graph_catalog::*;

#[test]
fn primitive_catalog_oracle_rejects_malformed_or_mismatched_observations() {
    let rows = [(1, 7, "é\0"), (2, 1, "")];
    let waters = [9, 1, 0, 0];
    let correct = Dictionary {
        rows: rows.to_vec(),
        high_waters: waters,
    };
    assert!(check_dictionary(&rows, waters, Some(correct.clone())).is_ok());
    assert!(check_dictionary(&rows, waters, None).is_err());
    for bad in [
        Dictionary {
            rows: vec![],
            ..correct.clone()
        },
        Dictionary {
            rows: vec![(1, 7, "e\u{301}\0"), (2, 1, "")],
            ..correct.clone()
        },
        Dictionary {
            high_waters: [7, 1, 0, 0],
            ..correct.clone()
        },
    ] {
        assert!(check_dictionary(&rows, waters, Some(bad)).is_err());
    }
    for rows in [
        vec![(1, 0, "a")],
        vec![(1, 10, "a")],
        vec![(0, 1, "a")],
        vec![(5, 1, "a")],
        vec![(1, 1, "a"), (1, 1, "b")],
        vec![(1, 1, "a"), (1, 2, "a")],
    ] {
        assert!(check_dictionary(&rows, waters, None).is_ok());
        assert!(check_dictionary(&rows, waters, Some(correct.clone())).is_err());
    }
    let a = Interpretation {
        lexical: 7,
        document: Some(("document", 2)),
    };
    assert!(check_admission(a, a, true).is_ok());
    assert!(check_admission(a, a, false).is_err());
    for b in [
        Interpretation { lexical: 8, ..a },
        Interpretation {
            document: None,
            ..a
        },
        Interpretation {
            document: Some(("Document", 2)),
            ..a
        },
        Interpretation {
            document: Some(("document", 3)),
            ..a
        },
    ] {
        assert!(check_admission(a, b, false).is_ok());
        assert!(check_admission(a, b, true).is_err());
    }
    let requests = [
        (1, "a"),
        (1, "b"),
        (1, "a"),
        (2, "a"),
        (0, "bad"),
        (5, "bad"),
    ];
    let observations = [Some(u64::MAX), None, Some(u64::MAX), Some(1), None, None];
    let initial = [u64::MAX - 1, 0, 0, 0];
    let final_waters = [u64::MAX, 1, 0, 0];
    assert!(check_interning(&requests, initial, &observations, final_waters).is_ok());
    assert!(check_interning(&requests, initial, &[], final_waters).is_err());
    assert!(check_interning(&requests, initial, &observations, initial).is_err());
}
