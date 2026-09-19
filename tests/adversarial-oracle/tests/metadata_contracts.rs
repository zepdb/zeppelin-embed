use std::collections::BTreeMap;
use zeppelin_embed_adversarial_oracle::metadata_filter_planner::*;

fn put32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}
fn blob(bytes: &mut Vec<u8>, value: &[u8]) {
    bytes.extend_from_slice(&(value.len() as u32).to_le_bytes());
    bytes.extend_from_slice(value);
}
fn definition(bytes: &mut Vec<u8>, id: u32, kind: u16, nullable: bool, name: &[u8]) {
    bytes.extend_from_slice(&id.to_le_bytes());
    bytes.extend_from_slice(&kind.to_le_bytes());
    bytes.extend_from_slice(&u16::from(nullable).to_le_bytes());
    blob(bytes, name);
}
fn columns() -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&6u32.to_le_bytes());
    for (id, kind, name) in [
        (0, 2, "ts"),
        (1, 1, "u"),
        (2, 3, "f"),
        (3, 4, "b"),
        (4, 5, "d"),
        (5, 6, "r"),
    ] {
        definition(&mut bytes, id, kind, id != 0, name.as_bytes());
    }
    for payload in [
        (-7i64).to_le_bytes().to_vec(),
        42u64.to_le_bytes().to_vec(),
        0x7ff8000000000042u64.to_le_bytes().to_vec(),
        vec![1],
    ] {
        blob(&mut bytes, &[1]);
        bytes.extend_from_slice(&payload);
    }
    blob(&mut bytes, &[1]);
    bytes.extend_from_slice(&1u32.to_le_bytes());
    blob(&mut bytes, b"cat");
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    blob(&mut bytes, &[1]);
    blob(&mut bytes, b"raw");
    bytes
}

#[test]
fn column_parser_rejects_malformed_definitions_presence_and_physical_cells() {
    let valid = columns();
    let parsed = parse_columns(&valid).unwrap();
    assert_eq!(parsed.cells[&(0, 0)].logical, ScalarCell::I64(-7));
    assert_eq!(
        parsed.cells[&(0, 2)].logical,
        ScalarCell::F64Bits(0x7ff8000000000042)
    );
    assert_eq!(
        parsed.cells[&(0, 4)].logical,
        ScalarCell::Utf8(b"cat".to_vec())
    );
    for end in 0..valid.len() {
        assert!(
            parse_columns(&valid[..end]).is_err(),
            "accepted truncation {end}"
        );
    }
    for (offset, value, message) in [
        (4, 0, "timestamp definition is absent"),
        (12, 9, "unknown column type"),
        (14, 2, "invalid nullable flag"),
        (20, 255, "column name UTF-8"),
        (8, 1, "duplicate column id"),
        (20, b'x', "timestamp definition is not canonical"),
    ] {
        let mut bad = valid.clone();
        bad[offset] = value;
        assert!(
            parse_columns(&bad).unwrap_err().contains(message),
            "{message}"
        );
    }
    let mut bad = valid.clone();
    bad.push(0);
    assert!(parse_columns(&bad).unwrap_err().contains("trailing"));
    let d = &parsed.definition_spans[&2];
    let mut bad = valid.clone();
    bad[d.name.payload.start] = b'u';
    assert!(
        parse_columns(&bad)
            .unwrap_err()
            .contains("duplicate column name")
    );
    let p = &parsed.presence_spans[&1];
    let mut bad = valid.clone();
    put32(&mut bad, p.length.start, 0);
    assert!(parse_columns(&bad).unwrap_err().contains("presence length"));
    let mut bad = valid.clone();
    bad[p.bitmap.start] = 128;
    assert!(parse_columns(&bad).unwrap_err().contains("tail padding"));
    for column in [1, 2, 3, 4, 5] {
        let mut bad = valid.clone();
        bad[parsed.presence_spans[&column].bitmap.start] = 0;
        if column == 4 {
            bad[parsed.cells[&(0, column)].span.start] = 1;
        }
        assert!(
            parse_columns(&bad).unwrap_err().contains("noncanonical"),
            "column {column}"
        );
    }
    let mut bad = valid.clone();
    bad[parsed.cells[&(0, 3)].span.start] = 2;
    assert!(parse_columns(&bad).unwrap_err().contains("invalid Boolean"));
    let dict = &parsed.dictionary_spans[&4];
    for (offset, value, message) in [
        (dict.reserved.start, 1, "reserved field"),
        (dict.width.start, 1, "dictionary width"),
        (parsed.cells[&(0, 4)].span.start, 1, "out of range"),
        (dict.entries[0].payload.start, 255, "UTF-8"),
    ] {
        let mut bad = valid.clone();
        bad[offset] = value;
        assert!(
            parse_columns(&bad).unwrap_err().contains(message),
            "{message}"
        );
    }
    let mut alive = vec![1, 0, 0, 0, 1, 0, 0, 0, 1];
    assert_eq!(parse_alive(&alive).unwrap().live, [0].into());
    alive[4] = 2;
    assert!(parse_alive(&alive).unwrap_err().contains("bitmap length"));
    alive[4] = 1;
    alive[8] = 2;
    assert!(parse_alive(&alive).unwrap_err().contains("tail padding"));
}

fn roundtrip() -> (I36Input, I36Observed) {
    let raw = parse_columns(&columns()).unwrap();
    let row: BTreeMap<_, _> = [
        (0, ScalarCell::I64(-7)),
        (1, ScalarCell::U64(42)),
        (2, ScalarCell::F64Bits(0x7ff8000000000042)),
        (3, ScalarCell::Bool(true)),
        (4, ScalarCell::Utf8(b"cat".to_vec())),
        (5, ScalarCell::Utf8(b"raw".to_vec())),
    ]
    .into();
    let input = I36Input {
        source: "sealed-7".into(),
        definitions: raw.definitions.clone(),
        rows: vec![row],
    };
    let observed = I36Observed {
        active_rows: input.rows.clone(),
        reader_rows: input.rows.clone(),
        public_rows: input.rows.clone(),
        raw,
    };
    (input, observed)
}

#[test]
fn column_roundtrip_checks_each_independent_observation_surface() {
    let (input, clean) = roundtrip();
    compare_i36(&input, &clean).unwrap();
    for (field, message) in [
        (0, "schema"),
        (1, "row-count"),
        (2, "active-public"),
        (3, "sealed-reader"),
        (4, "reopened-public"),
        (5, "Missing"),
        (6, "observed=U64(99)"),
        (7, "expected_present"),
        (8, "expected_physical"),
    ] {
        let mut bad = clean.clone();
        match field {
            0 => bad.raw.definitions[1].name = b"changed".to_vec(),
            1 => bad.raw.row_count = 2,
            2 => bad.active_rows.clear(),
            3 => {
                bad.reader_rows[0].insert(1, ScalarCell::U64(99));
            }
            4 => {
                bad.public_rows[0].insert(1, ScalarCell::Null);
            }
            5 => {
                bad.raw.cells.remove(&(0, 1));
            }
            6 => bad.raw.cells.get_mut(&(0, 1)).unwrap().logical = ScalarCell::U64(99),
            7 => bad.raw.cells.get_mut(&(0, 1)).unwrap().present = false,
            _ => bad.raw.cells.get_mut(&(0, 1)).unwrap().physical = PhysicalCell::I64(42),
        }
        let error = compare_i36(&input, &bad).unwrap_err();
        assert!(error.contains(message), "{message}: {error}");
        let replay = replay_canonical_comparison(
            I36_CHECKER_ID,
            &canonical_i36_input_bytes(&input),
            &canonical_i36_observed_bytes(&bad),
        )
        .unwrap();
        assert!(replay.first_difference.is_some());
    }
    compare_i36(&input, &clean).unwrap();
}

#[test]
fn canonical_metadata_replay_rejects_truncation_domain_and_suffix_changes() {
    let (input, observed) = roundtrip();
    let encoded = canonical_i36_input_bytes(&input);
    let observation = canonical_i36_observed_bytes(&observed);
    assert!(
        replay_canonical_comparison(I36_CHECKER_ID, &encoded, &observation)
            .unwrap()
            .first_difference
            .is_none()
    );
    for end in 0..encoded.len() {
        assert!(
            replay_canonical_comparison(I36_CHECKER_ID, &encoded[..end], &observation).is_err()
        );
    }
    for end in 0..observation.len() {
        assert!(
            replay_canonical_comparison(I36_CHECKER_ID, &encoded, &observation[..end]).is_err()
        );
    }
    let mut bad = encoded.clone();
    bad.push(0);
    assert!(
        replay_canonical_comparison(I36_CHECKER_ID, &bad, &observation)
            .unwrap_err()
            .contains("trailing")
    );
    let mut bad = encoded.clone();
    bad[8] = b'X';
    assert!(
        replay_canonical_comparison(I36_CHECKER_ID, &bad, &observation)
            .unwrap_err()
            .contains("domain mismatch")
    );
    assert!(
        replay_canonical_comparison("unknown", &encoded, &observation)
            .unwrap_err()
            .contains("unknown metadata canonical checker")
    );
}

fn branch_fixture() -> (I39ExpectedCase, I39Observed) {
    let key = QuerySourceKey {
        query_id: 7,
        source: "sealed-7".into(),
    };
    let expected = I39ExpectedCase {
        key: key.clone(),
        mode: I39ExecutionModeDto::ExactScan {
            source_may_match: true,
        },
        row_count: 2,
        filter_cardinality: 1,
        allow_list_threshold: 4,
        rows_examined: 1,
        allowed_rows_examined: 1,
        vectors_scored: 1,
        graph_nodes_visited: 0,
        exact_fallback_rows_examined: 0,
        returned_candidates: 1,
        ef_effective: None,
        visited_budget: None,
        sealed: true,
    };
    let report = BranchReportDto {
        key: key.clone(),
        branch: ExecutionBranchDto::ExactAllowList,
        fallback: FallbackReasonDto::None,
        filter_cardinality: 1,
    };
    let receipt = ExecutionReceiptDto {
        key,
        branch: ExecutionBranchDto::ExactAllowList,
        fallback: FallbackReasonDto::None,
        row_count: 2,
        filter_cardinality: 1,
        rows_examined: 1,
        allowed_rows_examined: 1,
        vectors_scored: 1,
        graph_nodes_visited: 0,
        exact_fallback_rows_examined: 0,
        returned_candidates: 1,
        ef_effective: None,
        visited_budget: None,
        sealed: true,
    };
    (
        expected,
        I39Observed {
            reports: vec![report.clone()],
            diagnostics_reports: vec![report],
            receipts: vec![receipt],
            allow_list_threshold: 4,
        },
    )
}

#[test]
fn execution_receipts_require_exact_correlations_counters_and_lifecycle() {
    let (expected, clean) = branch_fixture();
    compare_i39_expected(std::slice::from_ref(&expected), &clean).unwrap();
    for (field, message) in [
        (0, "count mismatch"),
        (1, "missing public report"),
        (2, "missing diagnostics report"),
        (3, "missing production execution receipt"),
        (4, "diagnostics="),
        (5, "observed_branch="),
        (6, "executed="),
        (7, "expected threshold="),
        (8, "expected row_count/cardinality"),
        (9, "expected rows_examined="),
        (10, "expected allowed_rows_examined="),
        (11, "expected vectors_scored="),
        (12, "expected graph_nodes_visited="),
        (13, "expected exact_fallback_rows_examined="),
        (14, "expected returned_candidates="),
        (15, "expected ef/budget="),
        (16, "expected sealed="),
        (17, "duplicate public report"),
        (18, "duplicate production execution receipt"),
    ] {
        let mut bad = clean.clone();
        match field {
            0 => bad.reports.clear(),
            1 => bad.reports[0].key.query_id = 8,
            2 => bad.diagnostics_reports[0].key.query_id = 8,
            3 => bad.receipts[0].key.query_id = 8,
            4 => bad.diagnostics_reports[0].filter_cardinality = 2,
            5 => {
                bad.reports[0].branch = ExecutionBranchDto::MaskedScan;
                bad.diagnostics_reports = bad.reports.clone();
            }
            6 => bad.receipts[0].branch = ExecutionBranchDto::MaskedScan,
            7 => bad.allow_list_threshold = 3,
            8 => bad.receipts[0].row_count = 3,
            9 => bad.receipts[0].rows_examined = 2,
            10 => bad.receipts[0].allowed_rows_examined = 2,
            11 => bad.receipts[0].vectors_scored = 2,
            12 => bad.receipts[0].graph_nodes_visited = 2,
            13 => bad.receipts[0].exact_fallback_rows_examined = 2,
            14 => bad.receipts[0].returned_candidates = 2,
            15 => bad.receipts[0].ef_effective = Some(1),
            16 => bad.receipts[0].sealed = false,
            17 => bad.reports.push(bad.reports[0].clone()),
            _ => bad.receipts.push(bad.receipts[0].clone()),
        }
        let error = compare_i39_expected(std::slice::from_ref(&expected), &bad).unwrap_err();
        assert!(error.contains(message), "{message}: {error}");
        let replay = replay_canonical_comparison(
            I39_CHECKER_ID,
            &canonical_i39_input_bytes(std::slice::from_ref(&expected)),
            &canonical_i39_observed_bytes(&bad),
        )
        .unwrap();
        assert!(replay.first_difference.is_some());
    }
    assert!(
        compare_i39_expected(&[expected.clone(), expected], &clean)
            .unwrap_err()
            .contains("duplicate expected case")
    );
}

#[test]
fn bitmap_algebra_refuses_duplicate_dead_and_uncorrelated_observations() {
    let (_, branch) = branch_fixture();
    let row = MetadataRowDto {
        row_id: 0,
        cells: [(1, ScalarCell::I64(7))].into(),
    };
    let source = I37SourceInputDto {
        source: "sealed-7".into(),
        sealed: true,
        rows: vec![row.clone()],
        live: [0].into(),
    };
    let expected = I37Input {
        rows: vec![row.clone()],
        live: [0].into(),
        predicate: PredicateDto::Eq {
            column: 1,
            value: ScalarCell::I64(7),
        },
        sources: vec![source],
    };
    let mut receipt = branch.receipts[0].clone();
    receipt.row_count = 1;
    let observed_source = I37SourceObservedDto {
        source: "sealed-7".into(),
        sealed: true,
        row_count: 1,
        live: [0].into(),
        evaluator: [0].into(),
        public_results: vec![0],
        report: branch.reports[0].clone(),
        receipt,
    };
    let clean = I37Observed {
        evaluator: [0].into(),
        public_results: vec![0],
        sources: vec![observed_source],
        allow_list_threshold: 4,
    };
    compare_i37(&expected, &clean).unwrap();
    for (field, message) in [
        (0, "duplicate_row="),
        (1, "dead_row="),
        (2, "duplicate source observation"),
        (3, "source count"),
        (4, "missing source observation"),
        (5, "expected sealed="),
        (6, "expected row_count/live="),
        (7, "duplicate public row"),
        (8, "correlation mismatch"),
        (9, "execution mismatch"),
        (10, "disagrees with lifecycle"),
        (11, "filter cardinality expected="),
    ] {
        let mut bad = clean.clone();
        match field {
            0 => bad.public_results.push(0),
            1 => bad.public_results.push(1),
            2 => bad.sources.push(bad.sources[0].clone()),
            3 => bad.sources.clear(),
            4 => bad.sources[0].source = "other".into(),
            5 => bad.sources[0].sealed = false,
            6 => bad.sources[0].row_count = 2,
            7 => bad.sources[0].public_results.push(0),
            8 => bad.sources[0].receipt.key.query_id = 8,
            9 => bad.sources[0].receipt.branch = ExecutionBranchDto::MaskedScan,
            10 => bad.sources[0].receipt.sealed = false,
            _ => {
                bad.sources[0].receipt.filter_cardinality = 2;
                bad.sources[0].report.filter_cardinality = 2;
            }
        }
        let error = compare_i37(&expected, &bad).unwrap_err();
        assert!(error.contains(message), "{message}: {error}");
    }
    let mut bad = expected.clone();
    bad.rows.push(row);
    assert!(evaluate_i37(&bad).unwrap_err().contains("duplicate row id"));
    let mut bad = expected.clone();
    bad.live.insert(1);
    assert!(
        evaluate_i37(&bad)
            .unwrap_err()
            .contains("live row 1 is absent")
    );
    let mut bad = expected.clone();
    bad.sources.push(bad.sources[0].clone());
    assert!(
        compare_i37(&bad, &clean)
            .unwrap_err()
            .contains("duplicate source input")
    );
    let encoded = canonical_i37_input_bytes(&expected);
    assert_eq!(decode_canonical_i37_input(&encoded).unwrap(), expected);
    for end in 0..encoded.len() {
        assert!(decode_canonical_i37_input(&encoded[..end]).is_err());
    }
    compare_i37(&expected, &clean).unwrap();
}

#[test]
fn pruning_requires_complete_live_baselines_and_exact_source_evidence() {
    let (_, branch) = branch_fixture();
    let hit = ExactHitDto {
        source: "sealed-7".into(),
        row_id: 0,
        document_id: (1u128 << 100) + 7,
        distance_bits: 0,
    };
    let row = SourceMetadataRowDto {
        source: "sealed-7".into(),
        row_id: 0,
        document_id: hit.document_id,
        cells: [(1, ScalarCell::I64(7))].into(),
    };
    let expected = I38Input {
        sources: vec![I38SourceDto {
            source: "sealed-7".into(),
            sealed: true,
            range: SourceRangeDto::Bounded { min: 1, max: 9 },
        }],
        rows: vec![row],
        live: [("sealed-7".into(), 0)].into(),
        predicate: PredicateDto::Eq {
            column: 1,
            value: ScalarCell::I64(7),
        },
        unfiltered_exact: vec![hit.clone()],
        expected_delete_records: 1,
    };
    let clean = I38Observed {
        filtered_exact: vec![hit.clone()],
        pruned_sources: Default::default(),
        reports: branch.reports,
        execution_receipts: branch.receipts,
        allow_list_threshold: 4,
        wal_delete_records: 1,
    };
    compare_i38(&expected, &clean).unwrap();
    for (field, message) in [
        (0, "duplicate source/row"),
        (1, "duplicate unfiltered exact hit"),
        (2, "not the complete live set"),
        (3, "missing metadata"),
        (4, "duplicate source lifecycle fact"),
        (5, "lacks lifecycle fact"),
        (6, "invalid bounds"),
        (7, "Empty range retains a live row"),
    ] {
        let mut bad = expected.clone();
        match field {
            0 => bad.rows.push(bad.rows[0].clone()),
            1 => bad.unfiltered_exact.push(hit.clone()),
            2 => bad.unfiltered_exact.clear(),
            3 => bad.rows.clear(),
            4 => bad.sources.push(bad.sources[0].clone()),
            5 => bad.sources.clear(),
            6 => bad.sources[0].range = SourceRangeDto::Bounded { min: 9, max: 1 },
            _ => bad.sources[0].range = SourceRangeDto::Empty,
        }
        let error = compare_i38(&bad, &clean).unwrap_err();
        assert!(error.contains(message), "{message}: {error}");
    }
    for (field, message) in [
        (0, "missing public branch report"),
        (1, "expected exactly one public branch report"),
        (2, "missing production execution receipt"),
        (3, "report/receipt mismatch"),
        (4, "disagrees with source lifecycle"),
        (5, "semantic="),
        (6, "orphan public branch report"),
        (7, "orphan production execution receipt"),
        (8, "pruned source ledger mismatch"),
        (9, "public delete WAL records"),
        (10, "unsound_prune"),
        (11, "orphan Pruned execution receipt"),
        (12, "missing_exact_hit"),
        (13, "extra_exact_hit"),
    ] {
        let mut bad = clean.clone();
        match field {
            0 => bad.reports.clear(),
            1 => {
                let mut extra = bad.reports[0].clone();
                extra.key.query_id = 8;
                bad.reports.push(extra);
            }
            2 => bad.execution_receipts.clear(),
            3 => bad.reports[0].filter_cardinality = 2,
            4 => bad.execution_receipts[0].sealed = false,
            5 => bad.execution_receipts[0].returned_candidates = 2,
            6 => bad.reports[0].key.source = "other".into(),
            7 => bad.execution_receipts[0].key.query_id = 8,
            8 => {
                bad.pruned_sources.insert("other".into());
            }
            9 => bad.wal_delete_records = 0,
            10 => {
                bad.pruned_sources.insert("sealed-7".into());
            }
            11 => bad.execution_receipts[0].branch = ExecutionBranchDto::Pruned,
            12 => bad.filtered_exact.clear(),
            _ => {
                let mut extra = hit.clone();
                extra.document_id += 1;
                bad.filtered_exact.push(extra);
            }
        }
        let error = compare_i38(&expected, &bad).unwrap_err();
        assert!(error.contains(message), "{message}: {error}");
    }
    let encoded = canonical_i38_input_bytes(&expected);
    assert_eq!(decode_canonical_i38_input(&encoded).unwrap(), expected);
    for end in 0..encoded.len() {
        assert!(decode_canonical_i38_input(&encoded[..end]).is_err());
    }
    compare_i38(&expected, &clean).unwrap();
}
