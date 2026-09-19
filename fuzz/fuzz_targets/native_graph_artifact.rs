#![no_main]

use libfuzzer_sys::fuzz_target;
use xxhash_rust::xxh3::xxh3_64;
use zeppelin_embed::property_graph::storage::{
    artifact::{self, ContainerKind, MAX_ARTIFACT_BYTES},
    tree::{self, TreeKind},
};

fn inspect(data: &[u8]) {
    for kind in [ContainerKind::Object, ContainerKind::RootEnvelope] {
        if let Ok(frame) = artifact::decode(kind, None, data) {
            let mut index = 0;
            while let Ok(reference) = frame.reference(index) {
                assert!(frame.resolve_framed_block(reference).is_ok());
                index += 1;
            }
        }
    }
    let _ = artifact::decode_reference(data);
    for kind in [
        TreeKind::Nodes,
        TreeKind::Relationships,
        TreeKind::KeyFences,
        TreeKind::Labels,
        TreeKind::RelationshipTypes,
        TreeKind::OutRanges,
        TreeKind::InRanges,
        TreeKind::ObjectInventory,
    ] {
        if let Ok(page) = tree::decode_page(kind, data) {
            let mut index = 0;
            while page.cell(index).is_ok() {
                index += 1;
            }
        }
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_ARTIFACT_BYTES {
        return;
    }
    inspect(data);

    // Keep the raw corruption lane above. Repair only the enclosing checksum
    // here so mutations can also reach semantic framing validation; payload and
    // directory block checksums still have to agree independently.
    if let Some(trailer) = data.len().checked_sub(8) {
        let mut repaired = data.to_vec();
        let checksum = xxh3_64(&repaired[..trailer]);
        repaired[trailer..].copy_from_slice(&checksum.to_le_bytes());
        inspect(&repaired);
    }

    // Page checksums occupy bytes 56..64 and hash the complete page with that
    // field zeroed. This lane reaches slots, cell descriptors and key bounds.
    if data.len() == tree::PAGE_BYTES {
        let mut repaired = data.to_vec();
        repaired[56..64].fill(0);
        let checksum = xxh3_64(&repaired);
        repaired[56..64].copy_from_slice(&checksum.to_le_bytes());
        inspect(&repaired);
    }
});
