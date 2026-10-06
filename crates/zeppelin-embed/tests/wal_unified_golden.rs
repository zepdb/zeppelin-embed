#![cfg(feature = "graph-cypher")]
#![allow(clippy::expect_used)]

use zeppelin_embed::format::golden::decode_hex;
use zeppelin_embed::ingest::wal_payload::{
    DELETE_V1, GRAPH_COMMIT_V1, MIXED_BATCH_MEMBER_V1, MutationPayload, PREPARED_MUTATION_V1,
    PayloadError, UPSERT_V2, UPSERT_V2_BATCH_MEMBER, decode_mixed_batch_member, decode_mutation,
    encode_delete, encode_graph_commit, encode_mixed_batch_member, encode_upsert_v2,
};
use zeppelin_embed::wal::LogSeq;
use zeppelin_embed::wal::record::{WalRecord, decode_record, encode_record};

fn graph_envelope() -> &'static [u8] {
    // The first complete envelope of the independently minted ZE-38 fixture;
    // exclude its family-19 file header and the following two envelopes.
    include_bytes!("fixtures/graph-wal/complete-v1.bin")
        .get(64..2105)
        .expect("first envelope")
}

#[test]
fn wal_mixed_member_v1_is_byte_exact() {
    let payload =
        encode_mixed_batch_member(1, 2, GRAPH_COMMIT_V1, graph_envelope()).expect("mixed payload");
    let encoded = encode_record(WalRecord {
        seq: LogSeq::new(0x0102_0304_0506_0708),
        op: MIXED_BATCH_MEMBER_V1,
        payload: &payload,
    })
    .expect("record");
    assert_eq!(
        encoded,
        decode_hex(include_str!("fixtures/format/wal_mixed_member_v1.hex")).expect("golden")
    );
    let record = decode_record(&encoded).expect("record").record;
    assert_eq!(
        decode_mutation(record.op, record.payload),
        Ok(MutationPayload::MixedBatchMember {
            index: 1,
            count: 2,
            op: GRAPH_COMMIT_V1,
            mutation: Box::new(MutationPayload::GraphCommit(graph_envelope().to_vec())),
        })
    );
    for (index, count) in [(0, 0), (0, 1), (2, 2)] {
        assert_eq!(
            encode_mixed_batch_member(index, count, GRAPH_COMMIT_V1, graph_envelope()),
            Err(PayloadError::BatchPosition { index, count })
        );
        let mut forged = payload.clone();
        forged
            .get_mut(..4)
            .expect("index")
            .copy_from_slice(&index.to_le_bytes());
        forged
            .get_mut(4..8)
            .expect("count")
            .copy_from_slice(&count.to_le_bytes());
        assert_eq!(
            decode_mixed_batch_member(&forged),
            Err(PayloadError::BatchPosition { index, count })
        );
    }
    for op in [
        0,
        1,
        3,
        UPSERT_V2_BATCH_MEMBER,
        PREPARED_MUTATION_V1,
        MIXED_BATCH_MEMBER_V1,
        u16::MAX,
    ] {
        assert_eq!(
            encode_mixed_batch_member(0, 2, op, &[]),
            Err(PayloadError::UnknownOperation(op))
        );
        let mut forged = payload.clone();
        forged
            .get_mut(8..10)
            .expect("op")
            .copy_from_slice(&op.to_le_bytes());
        assert_eq!(
            decode_mixed_batch_member(&forged),
            Err(PayloadError::UnknownOperation(op))
        );
    }
    let document = zeppelin_embed::ingest::IngestDocument::new(
        zeppelin_embed::ingest::DocumentVersion::new(
            zeppelin_embed::ingest::DocId::new(9),
            zeppelin_embed::ingest::Revision::new(1),
        ),
        vec![1.0],
    );
    for (op, inner, mutation) in [
        (
            DELETE_V1,
            encode_delete(&[zeppelin_embed::ingest::DocId::new(9)]).expect("delete"),
            MutationPayload::Delete(vec![zeppelin_embed::ingest::DocId::new(9)]),
        ),
        (
            UPSERT_V2,
            encode_upsert_v2(&document).expect("upsert"),
            MutationPayload::Upsert(document),
        ),
    ] {
        let bytes = encode_mixed_batch_member(0, 2, op, &inner).expect("member");
        assert_eq!(
            decode_mixed_batch_member(&bytes),
            Ok(MutationPayload::MixedBatchMember {
                index: 0,
                count: 2,
                op,
                mutation: Box::new(mutation),
            })
        );
    }
    for length in 0..payload.len() {
        assert!(decode_mixed_batch_member(payload.get(..length).expect("prefix")).is_err());
    }
}

#[test]
fn wal_graph_commit_v1_is_byte_exact() {
    let payload = encode_graph_commit(graph_envelope()).expect("graph payload");
    let encoded = encode_record(WalRecord {
        seq: LogSeq::new(0x0102_0304_0506_0708),
        op: GRAPH_COMMIT_V1,
        payload: &payload,
    })
    .expect("record");
    assert_eq!(
        encoded,
        decode_hex(include_str!("fixtures/format/wal_graph_commit_v1.hex")).expect("golden")
    );
    let record = decode_record(&encoded).expect("record").record;
    assert_eq!(
        decode_mutation(record.op, record.payload),
        Ok(MutationPayload::GraphCommit(graph_envelope().to_vec()))
    );
    assert_eq!(
        (PREPARED_MUTATION_V1, GRAPH_COMMIT_V1, MIXED_BATCH_MEMBER_V1),
        (9, 10, 11)
    );
    for length in 0..payload.len() {
        assert!(decode_mutation(GRAPH_COMMIT_V1, payload.get(..length).expect("prefix")).is_err());
    }
    let mut bad = payload.clone();
    *bad.get_mut(128).expect("first change") ^= 1;
    assert!(decode_mutation(GRAPH_COMMIT_V1, &bad).is_err());
    let mut trailing = payload;
    trailing.push(0);
    assert!(decode_mutation(GRAPH_COMMIT_V1, &trailing).is_err());
    assert_eq!(
        decode_mutation(u16::MAX, &[]),
        Err(PayloadError::UnknownOperation(u16::MAX))
    );
}
