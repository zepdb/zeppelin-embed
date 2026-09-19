#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]
use zeppelin_embed::property_graph::storage::adjacency::{
    self as a, Action, Admission, DeltaEntry, Direction, Edge, Error, FormatIssue, LimitIssue,
    RangeKey, UpperBound, Work,
};
use zeppelin_embed::property_graph::{NodeId, RelId, catalog::RelTypeId};
fn key() -> RangeKey {
    RangeKey {
        node: NodeId::new(1 << 100).unwrap(),
        rel_type: RelTypeId::new(7).unwrap(),
        direction: Direction::Out,
        lower: RelId::new(1).unwrap(),
        upper: UpperBound::Infinity,
    }
}
fn edge(rel: u128, neighbor: u128) -> Edge {
    Edge {
        rel: RelId::new(rel).unwrap(),
        neighbor: NodeId::new(neighbor).unwrap(),
    }
}
fn clean(_: Work) -> Result<(), ()> {
    Ok(())
}
fn base(k: RangeKey, seq: u64, entries: &[Edge]) -> Vec<u8> {
    let mut bytes = vec![0; a::HEADER_BYTES + entries.len() * 32];
    a::encode_base(k, seq, entries, &mut bytes, &mut clean).unwrap();
    bytes
}
fn delta(k: RangeKey, seq: u64, entries: &[(u128, u128, Action)]) -> Vec<u8> {
    let entries: Vec<_> = entries
        .iter()
        .map(|&(rel, neighbor, action)| DeltaEntry {
            edge: edge(rel, neighbor),
            action,
        })
        .collect();
    let mut bytes = vec![0; a::HEADER_BYTES + entries.len() * 40];
    a::encode_delta(k, seq, &entries, &mut bytes, &mut clean).unwrap();
    bytes
}
#[test]
fn adjacency_goldens_and_numeric_merge_keep_maximum_identity() {
    let k = key();
    let b = base(k, 4, &[edge(255, 2), edge(1 << 64, 3)]);
    let mut expected = vec![0; 96];
    expected[..4].copy_from_slice(b"ZADJ");
    expected[4..6].copy_from_slice(&1_u16.to_le_bytes());
    expected[6..8].copy_from_slice(&13_u16.to_le_bytes());
    expected[8..24].copy_from_slice(&(1_u128 << 100).to_le_bytes());
    expected[24..32].copy_from_slice(&7_u64.to_le_bytes());
    expected[32..48].copy_from_slice(&1_u128.to_le_bytes());
    expected[64..72].copy_from_slice(&4_u64.to_le_bytes());
    expected[72..76].copy_from_slice(&2_u32.to_le_bytes());
    expected[76] = 1;
    expected[77] = 1;
    for (r, n) in [(255_u128, 2_u128), (1 << 64, 3)] {
        expected.extend_from_slice(&r.to_le_bytes());
        expected.extend_from_slice(&n.to_le_bytes());
    }
    assert_eq!(b, expected);
    assert_eq!(
        b,
        include_bytes!("fixtures/graph-adjacency/base-v1.bin").as_slice()
    );
    let d = delta(
        k,
        5,
        &[(255, 2, Action::Delete), (u128::MAX, 4, Action::Insert)],
    );
    assert_eq!(
        d,
        include_bytes!("fixtures/graph-adjacency/delta-v1.bin").as_slice()
    );
    let mut output = [edge(1, 1); 4];
    let result = a::merge(k, 4, 5, &b, &[&d], &mut output, &mut clean).unwrap();
    assert_eq!(result.edges(), &[edge(1 << 64, 3), edge(u128::MAX, 4)]);
    assert_eq!(result.watermark(), 5);
    assert_eq!(result.partitions().len(), 1);
    assert_eq!(result.partitions()[0].key, k);
}
#[test]
fn adjacency_append_limits_require_consolidation_without_property_runs() {
    assert_eq!(a::append_admission(8, 2048, 0), Ok(Admission::NoChange));
    assert_eq!(a::append_admission(7, 2047, 1), Ok(Admission::Append));
    assert_eq!(
        a::append_admission(8, 1, 1),
        Ok(Admission::ConsolidateFirst)
    );
    assert_eq!(
        a::append_admission(1, 2048, 1),
        Ok(Admission::ConsolidateFirst)
    );
    assert_eq!(
        a::append_admission(0, 0, 2049),
        Err(LimitIssue::PendingEntries)
    );
}
#[test]
fn adjacency_all_sequences_keep_neighbor_even_behind_delete() {
    let k = key();
    let b = base(k, 1, &[edge(1, 2)]);
    let d = delta(k, 2, &[(1, 3, Action::Delete)]);
    let mut out = [edge(1, 1); 1];
    assert_eq!(
        a::merge(k, 1, 2, &b, &[&d], &mut out, &mut clean).unwrap_err(),
        Error::Format(FormatIssue::Neighbor)
    );
    let d = delta(k, 2, &[(1, 2, Action::Insert)]);
    let e = delta(k, 2, &[(1, 2, Action::Delete)]);
    assert_eq!(
        a::merge(k, 1, 2, &b, &[&d, &e], &mut out, &mut clean).unwrap_err(),
        Error::Format(FormatIssue::SequenceConflict)
    );
}
#[test]
fn adjacency_reserved_outer_tags_roundtrip_without_payload_role_widening() {
    use zeppelin_embed::property_graph::storage::artifact::{
        self, ArtifactId, ArtifactIdentity, Block, BlockKind, ContainerKind,
    };
    use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};
    let identity = ArtifactIdentity {
        store: StoreInstanceId::new(1).unwrap(),
        artifact: ArtifactId::new(2).unwrap(),
        generation: GraphGeneration::new(3),
        creation_serial: 4,
    };
    let b = base(key(), 4, &[]);
    let d = delta(key(), 5, &[]);
    let blocks = [
        Block {
            kind: BlockKind::AdjacencyBase,
            payload: &b,
        },
        Block {
            kind: BlockKind::AdjacencyDelta,
            payload: &d,
        },
    ];
    let mut buffer = vec![0; 1024];
    let length =
        artifact::encode_into(ContainerKind::Object, identity, &blocks, &mut buffer).unwrap();
    let frame = artifact::decode(
        ContainerKind::Object,
        Some((identity.store, identity.artifact)),
        &buffer[..length],
    )
    .unwrap();
    assert_eq!(frame.identity(), identity);
}
#[test]
fn adjacency_split_4097_and_full_6144_output_keep_exact_intervals() {
    let k = key();
    let entries: Vec<_> = (1..=4096).map(|id| edge(id, 77)).collect();
    let b = base(k, 2, &entries);
    for added in [1, 2048] {
        let changes: Vec<_> = (4097..4097 + added)
            .map(|id| (id, 77, Action::Insert))
            .collect();
        let d = delta(k, 3, &changes);
        let mut output = vec![edge(1, 1); a::MAX_MERGED_ENTRIES];
        let merged = a::merge(k, 2, 3, &b, &[&d], &mut output, &mut clean).unwrap();
        assert_eq!(merged.edges().len(), 4096 + added as usize);
        assert_eq!(merged.partitions().len(), 2);
        let p = merged.partitions();
        assert_eq!(p[0].end, 4096);
        assert_eq!(p[1].start, 4096);
        assert_eq!(
            p[0].key.upper,
            UpperBound::Exclusive(RelId::new(4097).unwrap())
        );
        assert_eq!(p[1].key.lower, RelId::new(4097).unwrap());
        assert_eq!(p[1].key.upper, UpperBound::Infinity);
        for p in merged.partitions() {
            let bytes = base(p.key, merged.watermark(), &merged.edges()[p.start..p.end]);
            let mut again = vec![edge(1, 1); 4096];
            let restored = a::merge(p.key, 3, 3, &bytes, &[], &mut again, &mut clean).unwrap();
            assert_eq!(restored.edges(), &merged.edges()[p.start..p.end]);
        }
    }
}
#[test]
fn adjacency_every_truncation_reserved_field_and_late_corruption_fail_before_copy() {
    let k = key();
    let b = base(k, 1, &[edge(1, 2), edge(3, 4)]);
    let d = delta(k, 2, &[(2, 5, Action::Insert)]);
    let sentinel = edge(9, 9);
    let mut out = [sentinel; 4];
    for cut in 0..b.len() {
        assert!(a::merge(k, 1, 2, &b[..cut], &[&d], &mut out, &mut clean).is_err());
        assert_eq!(out, [sentinel; 4]);
    }
    for cut in 0..d.len() {
        assert!(a::merge(k, 1, 2, &b, &[&d[..cut]], &mut out, &mut clean).is_err());
        assert_eq!(out, [sentinel; 4]);
    }
    for offset in [0, 4, 6, 76, 77, 78, 95] {
        let mut bad = b.clone();
        bad[offset] = 255;
        assert!(a::merge(k, 1, 2, &bad, &[&d], &mut out, &mut clean).is_err());
        assert_eq!(out, [sentinel; 4]);
    }
    let mut trailing = d.clone();
    trailing.push(0);
    assert!(a::merge(k, 1, 2, &b, &[&trailing], &mut out, &mut clean).is_err());
    for offset in [128, 129, 135] {
        let mut bad = d.clone();
        bad[offset] = 255;
        assert!(a::merge(k, 1, 2, &b, &[&bad], &mut out, &mut clean).is_err());
        assert_eq!(out, [sentinel; 4]);
    }
    let mut bad = b.clone();
    bad[128..144].copy_from_slice(&1_u128.to_le_bytes());
    assert_eq!(
        a::merge(k, 1, 2, &bad, &[&d], &mut out, &mut clean).unwrap_err(),
        Error::Format(FormatIssue::Order)
    );
    let later = delta(k, 3, &[(3, 999, Action::Delete)]);
    assert_eq!(
        a::merge(k, 1, 3, &b, &[&d, &later], &mut out, &mut clean).unwrap_err(),
        Error::Format(FormatIssue::Neighbor)
    );
    assert_eq!(out, [sentinel; 4]);
    assert_eq!(
        a::merge(k, 1, 2, &b, &[&d], &mut [], &mut clean).unwrap_err(),
        Error::Limit(LimitIssue::Output)
    );
}
#[test]
fn adjacency_sequence_cutoff_equal_duplicates_and_all_sequence_topology() {
    let k = key();
    let b = base(k, 7, &[edge(1, 8)]);
    let mut out = [edge(1, 1); 4];
    for (watermark, cutoff, sequence) in [(8, 9, 9_u64), (7, 6, 8), (7, 8, 7), (7, 8, 9), (7, 8, 0)]
    {
        let mut d = delta(k, 8, &[(2, 9, Action::Insert)]);
        d[64..72].copy_from_slice(&sequence.to_le_bytes());
        assert_eq!(
            a::merge(k, watermark, cutoff, &b, &[&d], &mut out, &mut clean).unwrap_err(),
            Error::Format(FormatIssue::Sequence)
        );
    }
    let d = delta(k, 8, &[(1, 8, Action::Delete), (2, 9, Action::Insert)]);
    let e = delta(k, 9, &[(2, 9, Action::Delete)]);
    assert_eq!(
        a::merge(k, 7, 9, &b, &[&e, &d], &mut out, &mut clean).unwrap_err(),
        Error::Format(FormatIssue::Sequence)
    );
    assert_eq!(
        a::merge(k, 7, 8, &b, &[&d, &d], &mut out, &mut clean)
            .unwrap()
            .edges(),
        &[edge(2, 9)]
    );
    let wrong = delta(k, 8, &[(2, 10, Action::Insert)]);
    assert_eq!(
        a::merge(k, 7, 9, &b, &[&d, &wrong, &e], &mut out, &mut clean).unwrap_err(),
        Error::Format(FormatIssue::Neighbor)
    );
    let last = delta(k, u64::MAX, &[(1, 8, Action::Delete)]);
    assert!(
        a::merge(k, 7, u64::MAX, &b, &[&last], &mut out, &mut clean)
            .unwrap()
            .edges()
            .is_empty()
    );
}
#[test]
fn adjacency_empty_sparse_and_finite_ranges_validate_all_group_fields() {
    let k = RangeKey {
        lower: RelId::new(100).unwrap(),
        upper: UpperBound::Exclusive(RelId::new(200).unwrap()),
        ..key()
    };
    let b = base(k, 0, &[]);
    let mut out = [edge(1, 1); 1];
    let empty = a::merge(k, 0, 0, &b, &[], &mut out, &mut clean).unwrap();
    assert!(empty.edges().is_empty());
    assert!(empty.partitions().is_empty());
    let d = delta(k, 1, &[(150, 44, Action::Insert)]);
    assert_eq!(
        a::merge(k, 0, 1, &b, &[&d], &mut out, &mut clean)
            .unwrap()
            .edges(),
        &[edge(150, 44)]
    );
    for offset in [8, 24, 32, 48, 76] {
        let mut wrong = b.clone();
        wrong[offset] ^= 1;
        assert!(a::merge(k, 0, 1, &wrong, &[&d], &mut out, &mut clean).is_err());
    }
    for rel in [99, 200, u128::MAX] {
        let mut wrong = d.clone();
        wrong[96..112].copy_from_slice(&rel.to_le_bytes());
        assert_eq!(
            a::merge(k, 0, 1, &b, &[&wrong], &mut out, &mut clean).unwrap_err(),
            Error::Format(FormatIssue::Range)
        );
    }
    let invalid = RangeKey {
        upper: UpperBound::Exclusive(k.lower),
        ..k
    };
    let mut bytes = [0; 128];
    assert_eq!(
        a::encode_base(invalid, 0, &[], &mut bytes, &mut clean).unwrap_err(),
        Error::Format(FormatIssue::Range)
    );
}
#[test]
fn adjacency_limits_reject_ninth_run_and_2049_total_observations() {
    let k = key();
    let b = base(k, 0, &[]);
    let d = delta(k, 1, &[(1, 2, Action::Insert)]);
    let mut out = [edge(1, 1); 1];
    assert_eq!(
        a::merge(k, 0, 1, &b, &[d.as_slice(); 8], &mut out, &mut clean)
            .unwrap()
            .edges(),
        &[edge(1, 2)]
    );
    assert_eq!(
        a::merge(k, 0, 1, &b, &[d.as_slice(); 9], &mut out, &mut clean).unwrap_err(),
        Error::Limit(LimitIssue::DeltaRuns)
    );
    let entries: Vec<_> = (1..=2048).map(|r| (r, 2, Action::Insert)).collect();
    let full = delta(k, 1, &entries);
    assert_eq!(
        a::merge(k, 0, 1, &b, &[&full, &d], &mut out, &mut clean).unwrap_err(),
        Error::Limit(LimitIssue::PendingEntries)
    );
    let edges = vec![edge(1, 2); 4097];
    let mut bytes = vec![0; 200000];
    assert_eq!(
        a::encode_base(k, 0, &edges, &mut bytes, &mut clean).unwrap_err(),
        Error::Limit(LimitIssue::BaseEntries)
    );
}
#[test]
fn adjacency_every_actual_work_checkpoint_and_final_completion_can_refuse() {
    let k = key();
    let b = base(k, 0, &[edge(1, 2), edge(3, 4)]);
    let d = delta(k, 1, &[(1, 2, Action::Delete), (2, 5, Action::Insert)]);
    let mut output = [edge(1, 1); 4];
    let mut events = Vec::new();
    a::merge(k, 0, 1, &b, &[&d], &mut output, &mut |work| {
        events.push(work);
        Ok::<_, usize>(())
    })
    .unwrap();
    assert_eq!(events.last(), Some(&Work::Finish));
    assert!(events.iter().all(|w| match w {
        Work::HeaderBytes(n) | Work::EntryBytes(n) | Work::CopyBytes(n) => *n <= 96,
        _ => true,
    }));
    for fail in 1..=events.len() {
        let mut seen = 0;
        let result = a::merge(k, 0, 1, &b, &[&d], &mut output, &mut |_| {
            seen += 1;
            if seen == fail { Err(fail) } else { Ok(()) }
        });
        assert_eq!(result.unwrap_err(), Error::Control(fail));
        assert_eq!(seen, fail);
    }
    let mut bytes = [0; 160];
    let entries = [edge(1, 2), edge(3, 4)];
    let mut n = 0;
    a::encode_base(k, 0, &entries, &mut bytes, &mut |_| {
        n += 1;
        Ok::<_, usize>(())
    })
    .unwrap();
    for fail in 1..=n {
        let mut seen = 0;
        assert_eq!(
            a::encode_base(k, 0, &entries, &mut bytes, &mut |_| {
                seen += 1;
                if seen == fail { Err(fail) } else { Ok(()) }
            })
            .unwrap_err(),
            Error::Control(fail)
        );
    }
}
#[test]
fn adjacency_wal_reference_tags_are_append_only_through_public_decode() {
    use zeppelin_embed::property_graph::storage::artifact::BlockKind;
    use zeppelin_embed::property_graph::wal::{
        ReferenceList, STACK_RESERVATION_BYTES, WalError, WalResources,
    };
    let mut bytes = [0; 96];
    bytes[..16].copy_from_slice(&1_u128.to_le_bytes());
    bytes[16..32].copy_from_slice(&2_u128.to_le_bytes());
    bytes[40..48].copy_from_slice(&1_u64.to_le_bytes());
    bytes[48..52].copy_from_slice(&4096_u32.to_le_bytes());
    bytes[52..54].copy_from_slice(&17_u16.to_le_bytes());
    bytes[54..56].copy_from_slice(&1_u16.to_le_bytes());
    bytes[64..80].copy_from_slice(&2_u128.to_le_bytes());
    bytes[80..88].copy_from_slice(&96_u64.to_le_bytes());
    bytes[88..92].copy_from_slice(&128_u32.to_le_bytes());
    bytes[94..96].copy_from_slice(&1_u16.to_le_bytes());
    for (tag, kind) in [
        (13, BlockKind::AdjacencyBase),
        (14, BlockKind::AdjacencyDelta),
        (10, BlockKind::CommitParticipant),
    ] {
        bytes[92..94].copy_from_slice(&u16::to_le_bytes(tag));
        let mut cancel = || false;
        let mut resources = WalResources::new(10000, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
        assert_eq!(
            ReferenceList::Encoded(&bytes)
                .get(0, &mut resources)
                .unwrap()
                .block
                .kind,
            kind
        );
    }
    bytes[92..94].copy_from_slice(&15_u16.to_le_bytes());
    let mut cancel = || false;
    let mut resources = WalResources::new(10000, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
    assert_eq!(
        ReferenceList::Encoded(&bytes)
            .get(0, &mut resources)
            .unwrap_err(),
        WalError::Unsupported
    );
}
#[test]
fn adjacency_merge_comparisons_keep_high_bits_between_runs() {
    let k = key();
    let b = base(k, 0, &[edge(255, 2)]);
    let d = delta(k, 1, &[((1_u128 << 64) + 1, 3, Action::Insert)]);
    let mut output = [edge(1, 1); 2];
    assert_eq!(
        a::merge(k, 0, 1, &b, &[&d], &mut output, &mut clean)
            .unwrap()
            .edges(),
        &[edge(255, 2), edge((1_u128 << 64) + 1, 3)]
    );
}
