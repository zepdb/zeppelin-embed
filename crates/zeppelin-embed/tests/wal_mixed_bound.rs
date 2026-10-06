#![allow(clippy::expect_used)]

use zeppelin_embed::ingest::wal_payload::mixed_batch_group_bytes;
use zeppelin_embed::wal::WalWriteError;

#[test]
fn mixed_batch_group_bound_includes_header_and_all_member_framing() {
    // Two inner payloads of 24 and 36 bytes occupy 124 bytes in an existing
    // WAL, or 164 bytes when the 40-byte file header is in the same group.
    assert_eq!(
        mixed_batch_group_bytes(0, &[24, 36], 124).expect("exact fit"),
        124
    );
    assert_eq!(
        mixed_batch_group_bytes(40, &[24, 36], 164).expect("fresh fit"),
        164
    );
    assert!(matches!(
        mixed_batch_group_bytes(40, &[24, 36], 163),
        Err(WalWriteError::GroupTooLarge {
            encoded_bytes: 164,
            max_group_bytes: 163
        })
    ));
    assert!(matches!(
        mixed_batch_group_bytes(0, &[usize::MAX, 1], 16_777_216),
        Err(WalWriteError::GroupTooLarge { .. })
    ));
    assert!(matches!(
        mixed_batch_group_bytes(usize::MAX, &[1, 1], usize::MAX),
        Err(WalWriteError::GroupTooLarge { .. })
    ));
}

#[cfg(not(feature = "graph-cypher"))]
#[test]
fn graph_free_payload_decoder_refuses_graph_operations() {
    use zeppelin_embed::ingest::wal_payload::{
        GRAPH_COMMIT_V1, MIXED_BATCH_MEMBER_V1, PayloadError, decode_mutation,
        encode_mixed_batch_member,
    };
    assert_eq!(
        decode_mutation(GRAPH_COMMIT_V1, &[]),
        Err(PayloadError::UnknownOperation(GRAPH_COMMIT_V1))
    );
    let member = encode_mixed_batch_member(0, 2, GRAPH_COMMIT_V1, &[]).expect("framing");
    assert_eq!(
        decode_mutation(MIXED_BATCH_MEMBER_V1, &member),
        Err(PayloadError::UnknownOperation(GRAPH_COMMIT_V1))
    );
}
