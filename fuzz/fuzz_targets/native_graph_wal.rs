#![no_main]
use libfuzzer_sys::fuzz_target;
use zeppelin_embed::property_graph::storage::artifact::{ArtifactId, BlockKind, PhysicalRef};
use zeppelin_embed::property_graph::wal::*;
use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};
// Framing fuzz intentionally models successful opaque semantic callbacks. It
// cannot establish actual catalog/mark/extent validity owned by other modules.
struct SyntacticOnly;
impl ReplayValidator for SyntacticOnly {
    fn required(
        &mut self,
        _: RequiredRef,
        _: RequiredRole,
        r: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        r.charge(1)
    }
    fn mutation(&mut self, _: Mutation<'_>, r: &mut WalResources<'_>) -> Result<(), WalError> {
        r.charge(1)
    }
    fn inventory(&mut self, _: InventoryChange, r: &mut WalResources<'_>) -> Result<(), WalError> {
        r.charge(1)
    }
    fn reclaim_intent(
        &mut self,
        _: ReclaimIntent<'_>,
        r: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        r.charge(1)
    }
    fn reclaim_complete(
        &mut self,
        _: ReclaimComplete<'_>,
        r: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        r.charge(1)
    }
    fn state(
        &mut self,
        _: EnvelopeKind,
        _: CommitState<'_>,
        _: CommitState<'_>,
        _: ChangeReader<'_>,
        r: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        r.charge(1)
    }
}
fn base() -> CommitState<'static> {
    let store = StoreInstanceId::new((1u128 << 100) + 3).unwrap();
    let artifact = ArtifactId::new((1u128 << 96) + 9).unwrap();
    let reference = RequiredRef {
        object: ArtifactDescriptor {
            store,
            artifact,
            generation: GraphGeneration::new(0),
            serial: 1,
            bytes: 200,
            family: 17,
            version: 1,
            checksum: 7,
        },
        block: PhysicalRef {
            artifact,
            offset: 96,
            length: 64,
            kind: BlockKind::CommitParticipant,
            version: 1,
        },
    };
    CommitState {
        store,
        generation: GraphGeneration::new(0),
        sequence: 0,
        graph: WalGraphRoots::default(),
        catalog: reference,
        vector: None,
        text: None,
        reclaim: None,
        high_waters: HighWaters {
            creation_serial: 1,
            ..HighWaters::default()
        },
        prepared_inventories: ReferenceList::Values(&[]),
    }
}
fn exercise(bytes: &[u8]) {
    let mut cancel = || false;
    let mut r = WalResources::new(64_000_000, STACK_RESERVATION_BYTES, &mut cancel).unwrap();
    if let Ok(mut replay) = Replay::new(bytes, base(), &mut r) {
        while let Ok(ReplayStep::Envelope(v)) = replay.next_envelope(&mut SyntacticOnly, &mut r) {
            let mut changes = v.changes();
            while let Ok(Some(_)) = changes.next_change(&mut r) {}
            let _ = Replay::at_watermark(bytes, v.state, bytes.len(), &mut r);
        }
    }
}
fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_ENVELOPE_BYTES + 64 {
        return;
    }
    exercise(data);
    // Keep the raw leg above; repairing framing checksums lets mutations reach
    // the scalar, UTF-8, provenance, and complete-state validators as well.
    let mut repaired = data.to_vec();
    if repaired.len() < 64 {
        return;
    }
    let sum = xxhash_rust::xxh3::xxh3_64(&repaired[..56]);
    repaired[56..64].copy_from_slice(&sum.to_le_bytes());
    let (mut at, mut begin) = (64usize, 64usize);
    while repaired.len().saturating_sub(at) >= 64 {
        let length = u32::from_le_bytes(repaired[at + 8..at + 12].try_into().unwrap()) as usize;
        let Some(end) = at
            .checked_add(72)
            .and_then(|v| v.checked_add(length))
            .filter(|end| *end <= repaired.len())
        else {
            break;
        };
        let kind = u16::from_le_bytes(repaired[at + 4..at + 6].try_into().unwrap());
        if kind == 1 {
            begin = at;
        }
        if kind == 6 && length >= 16 && begin <= at {
            let sum = xxhash_rust::xxh3::xxh3_64(&repaired[begin..at]);
            repaired[at + 72..at + 80].copy_from_slice(&sum.to_le_bytes());
        }
        let sum = xxhash_rust::xxh3::xxh3_64(&repaired[at..at + 56]);
        repaired[at + 56..at + 64].copy_from_slice(&sum.to_le_bytes());
        let sum = xxhash_rust::xxh3::xxh3_64(&repaired[at..end - 8]);
        repaired[end - 8..end].copy_from_slice(&sum.to_le_bytes());
        at = end;
    }
    exercise(&repaired);
});
