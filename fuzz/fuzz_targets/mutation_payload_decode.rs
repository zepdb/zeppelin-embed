#![no_main]

use libfuzzer_sys::fuzz_target;
use zeppelin_embed::ingest::wal_payload::{
    DELETE_V1, METADATA_EDIT_V1, MutationPayload, UPSERT_V1, decode_mutation, encode_delete,
    encode_metadata_edit, encode_upsert,
};

fuzz_target!(|data: &[u8]| {
    let Some((&selector, payload)) = data.split_first() else {
        return;
    };
    let op = match selector % 4 {
        0 => u16::MAX,
        1 => UPSERT_V1,
        2 => DELETE_V1,
        _ => METADATA_EDIT_V1,
    };
    if let Ok(decoded) = decode_mutation(op, payload) {
        let reencoded = match &decoded {
            MutationPayload::Upsert(document) => encode_upsert(document),
            MutationPayload::Delete(doc_ids) => encode_delete(doc_ids),
            MutationPayload::MetadataEdit(edit) => encode_metadata_edit(edit),
        }
        .expect("every accepted mutation payload must remain encodable");
        assert_eq!(reencoded, payload, "accepted payloads have one encoding");
    }
});
