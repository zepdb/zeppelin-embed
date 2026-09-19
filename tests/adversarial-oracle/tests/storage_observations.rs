use zeppelin_embed_adversarial_oracle::storage_durability::*;

fn refusal(result: Result<(), CheckFailure>, checker: &str, detail: &str) {
    let error = result.unwrap_err();
    assert_eq!((error.checker_id, error.detail), (checker, detail));
    assert_eq!(error.to_string(), format!("{checker}: {detail}"));
}

#[test]
fn wal_prefix_checker_rejects_changed_acknowledgements_and_public_refusals() {
    let record = WalRecordFact {
        seq: 1,
        op: 7,
        payload: vec![42],
    };
    let clean_public = WalPublicOutcome::Opened {
        live_versions: vec![(7, 1)],
    };
    let expected = WalPrefixExpected {
        first_seq: 1,
        acknowledged: vec![record.clone()],
        optional_unacknowledged_tail: vec![],
        terminator: WalTerminator::CleanEnd,
        live_versions: vec![(7, 1)],
        ack_boundaries: vec![],
    };
    let clean = WalPrefixObserved {
        first_seq: 1,
        records: vec![record.clone()],
        terminator: WalTerminator::CleanEnd,
        clean_public: clean_public.clone(),
        public: clean_public,
        reopened_ack_boundaries: vec![],
    };
    check_i16(&expected, &clean).unwrap();
    for (field, detail) in [
        (0, "same-seed clean logical versions differ"),
        (1, "same-seed clean WAL was refused"),
        (2, "first sequence differs"),
        (3, "record sequence is not a prefix"),
        (4, "unacknowledged records are not a legal tail"),
        (5, "WAL terminator differs"),
        (6, "reopened logical versions differ"),
        (7, "clean WAL prefix was refused"),
    ] {
        let mut bad = clean.clone();
        let missing = WalTerminator::InvalidHeader {
            artifact: "wal.ze".into(),
            reason: WalHeaderFailure::Missing,
        };
        match field {
            0 => {
                bad.clean_public = WalPublicOutcome::Opened {
                    live_versions: vec![],
                }
            }
            1 => {
                bad.clean_public = WalPublicOutcome::Refused {
                    terminator: missing,
                }
            }
            2 => bad.first_seq = 2,
            3 => bad.records[0].seq = 3,
            4 => bad.records.push(WalRecordFact {
                seq: 2,
                ..record.clone()
            }),
            5 => bad.terminator = missing,
            6 => {
                bad.public = WalPublicOutcome::Opened {
                    live_versions: vec![],
                }
            }
            _ => {
                bad.public = WalPublicOutcome::Refused {
                    terminator: missing,
                }
            }
        }
        refusal(check_i16(&expected, &bad), I16_CHECKER_ID, detail);
    }
    let missing = WalTerminator::InvalidHeader {
        artifact: "wal.ze".into(),
        reason: WalHeaderFailure::Missing,
    };
    let mut damaged_expected = expected.clone();
    damaged_expected.terminator = missing.clone();
    let mut damaged = clean.clone();
    damaged.records.clear();
    damaged.terminator = missing.clone();
    damaged.public = WalPublicOutcome::Refused {
        terminator: missing,
    };
    check_i16(&damaged_expected, &damaged).unwrap();
    let mut bad = damaged.clone();
    bad.records.push(record.clone());
    refusal(
        check_i16(&damaged_expected, &bad),
        I16_CHECKER_ID,
        "invalid WAL header exposed trusted records",
    );
    let mut bad = damaged.clone();
    bad.public = clean.public.clone();
    refusal(
        check_i16(&damaged_expected, &bad),
        I16_CHECKER_ID,
        "damaged WAL was accepted",
    );
    let mut bad = damaged.clone();
    bad.public = WalPublicOutcome::Refused {
        terminator: WalTerminator::CleanEnd,
    };
    refusal(
        check_i16(&damaged_expected, &bad),
        I16_CHECKER_ID,
        "public WAL refusal differs",
    );
    let corrupt = WalTerminator::CorruptAt {
        artifact: "wal.ze".into(),
        offset: 63,
        location: CorruptionLocation::Tail,
        reason: WalRecordFailure::HeaderTruncated {
            needed: 14,
            available: 1,
        },
    };
    damaged_expected.terminator = corrupt.clone();
    damaged.terminator = corrupt.clone();
    damaged.public = WalPublicOutcome::Refused {
        terminator: corrupt,
    };
    refusal(
        check_i16(&damaged_expected, &damaged),
        I16_CHECKER_ID,
        "trusted records before corrupt tail differ from acknowledged model",
    );
    damaged.records.push(record);
    check_i16(&damaged_expected, &damaged).unwrap();
}

#[test]
fn format_checker_rejects_wrong_case_call_control_and_partial_results() {
    let expected = FormatExpected {
        case: FormatCase::WalHeader,
        call: FormatPublicCall::Open,
        clean: FormatCleanOutcome::Opened,
        refusal: FormatRefusal::WalInvalidHeader {
            artifact: ArtifactFact::Wal {
                path: "wal.ze".into(),
            },
            reason: WalHeaderFailure::Missing,
        },
    };
    let clean = FormatObserved {
        case: expected.case,
        call: expected.call,
        clean: expected.clean,
        refusal: Some(expected.refusal.clone()),
        partial_candidates: 0,
    };
    check_i18(&expected, &clean).unwrap();
    for (field, detail) in [
        (0, "format case differs"),
        (1, "public call differs"),
        (2, "same-seed clean public result differs"),
        (3, "partial candidates escaped refusal"),
        (4, "damaged artifact succeeded"),
        (5, "typed artifact refusal differs"),
    ] {
        let mut bad = clean.clone();
        match field {
            0 => bad.case = FormatCase::WalRecordBody,
            1 => bad.call = FormatPublicCall::ExactSearch,
            2 => bad.clean = FormatCleanOutcome::ExactSearch { candidates: 1 },
            3 => bad.partial_candidates = 1,
            4 => bad.refusal = None,
            _ => {
                bad.refusal = Some(FormatRefusal::WalInvalidHeader {
                    artifact: ArtifactFact::Wal {
                        path: "other.ze".into(),
                    },
                    reason: WalHeaderFailure::Missing,
                })
            }
        }
        refusal(check_i18(&expected, &bad), I18_CHECKER_ID, detail);
    }
}

fn file(path: &str, length: u64) -> FileFact {
    FileFact {
        path: path.into(),
        length,
        digest: [7; 32],
    }
}

#[test]
fn reachability_checker_rejects_incomplete_classification_and_changed_cleanup_evidence() {
    let preserved = file("manifest.ze", 32);
    let orphan = file(".manifest.ze.tmp", 9);
    let expected = ReachabilityExpected {
        baseline: vec![preserved.clone(), orphan.clone()],
        manifest_referenced: vec![],
        control_and_unknown: vec![preserved.clone()],
        preserved: vec![preserved.clone()],
        eligible_orphans: vec![orphan.clone()],
        reclaimed_bytes: 9,
        directory_sync_required: true,
        expected_directory_syncs: 1,
        committed_purge_read_only_required: false,
    };
    let clean = ReachabilityObserved {
        after_read_only: expected.baseline.clone(),
        final_inventory: expected.preserved.clone(),
        reclaimed_bytes: 9,
        directory_syncs: 1,
        committed_purge_read_only: None,
    };
    check_i19(&expected, &clean).unwrap();
    for (field, detail) in [
        (0, "unexpected committed purge read-only evidence"),
        (1, "read-only open mutated files"),
        (2, "reachable file removed"),
        (3, "preserved file bytes changed"),
        (4, "eligible orphan retained"),
        (5, "final inventory differs"),
        (6, "reclaimed byte count differs"),
        (7, "directory sync count differs"),
    ] {
        let mut bad = clean.clone();
        match field {
            0 => {
                bad.committed_purge_read_only = Some(CommittedPurgeReadOnlyObserved {
                    before: vec![],
                    after: vec![],
                    outcome: CommittedPurgeReadOnlyOutcome::RefusedPurgeRecoveryReadOnly,
                })
            }
            1 => bad.after_read_only.push(preserved.clone()),
            2 => bad.final_inventory.clear(),
            3 => bad.final_inventory[0].digest[0] ^= 1,
            4 => bad.final_inventory.push(orphan.clone()),
            5 => bad.final_inventory.push(file("surprise", 1)),
            6 => bad.reclaimed_bytes = 0,
            _ => bad.directory_syncs = 0,
        }
        refusal(check_i19(&expected, &bad), I19_CHECKER_ID, detail);
    }
    for (field, detail) in [
        (0, "manifest reachability classification overlaps"),
        (1, "expected inventory classification overlaps"),
        (2, "expected inventory classification is incomplete"),
        (3, "expected reclaimed byte count differs from inventory"),
    ] {
        let mut bad = expected.clone();
        match field {
            0 => bad.manifest_referenced.push(preserved.clone()),
            1 => bad.preserved.push(orphan.clone()),
            2 => bad.eligible_orphans.clear(),
            _ => bad.reclaimed_bytes = 0,
        }
        refusal(check_i19(&bad, &clean), I19_CHECKER_ID, detail);
    }
    let required = expected.clone().with_committed_purge_read_only();
    refusal(
        check_i19(&required, &clean),
        I19_CHECKER_ID,
        "committed purge read-only evidence missing",
    );
    let mut bad = clean.clone();
    bad.committed_purge_read_only = Some(CommittedPurgeReadOnlyObserved {
        before: vec![],
        after: vec![],
        outcome: CommittedPurgeReadOnlyOutcome::RefusedPurgeRecoveryReadOnly,
    });
    refusal(
        check_i19(&required, &bad),
        I19_CHECKER_ID,
        "committed purge intent evidence missing",
    );
    bad.committed_purge_read_only
        .as_mut()
        .unwrap()
        .before
        .push(file("purge.ze", 1));
    refusal(
        check_i19(&required, &bad),
        I19_CHECKER_ID,
        "read-only committed-purge open mutated files",
    );
    bad.committed_purge_read_only
        .as_mut()
        .unwrap()
        .after
        .push(file("purge.ze", 1));
    check_i19(&required, &bad).unwrap();
}
