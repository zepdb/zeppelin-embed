#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use super::*;
use crate::allocation_audit::audit_engine_path;
use crate::property_graph::storage::artifact::BlockKind;
const GOLDEN: &[u8] = include_bytes!("../../../tests/fixtures/graph-wal/complete-v1.bin");
fn initial() -> CommitState<'static> {
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
struct Carriage;
impl ReplayValidator for Carriage {
    fn required(
        &mut self,
        _: RequiredRef,
        _: RequiredRole,
        _: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        Ok(())
    }
    fn mutation(&mut self, _: Mutation<'_>, _: &mut WalResources<'_>) -> Result<(), WalError> {
        Ok(())
    }
    fn inventory(&mut self, _: InventoryChange, _: &mut WalResources<'_>) -> Result<(), WalError> {
        Ok(())
    }
    fn reclaim_intent(
        &mut self,
        _: ReclaimIntent<'_>,
        _: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        Ok(())
    }
    fn reclaim_complete(
        &mut self,
        _: ReclaimComplete<'_>,
        _: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        Ok(())
    }
    fn state(
        &mut self,
        _: EnvelopeKind,
        _: CommitState<'_>,
        _: CommitState<'_>,
        _: ChangeReader<'_>,
        _: &mut WalResources<'_>,
    ) -> Result<(), WalError> {
        Ok(())
    }
}
#[test]
fn wal_complete_replay_and_encode_make_zero_heap_allocations() {
    let base = initial();
    let mut output = vec![0; 4096];
    let (result, audit) = audit_engine_path(|| -> Result<usize, WalError> {
        let mut cancel = || false;
        let mut r = WalResources::new(100_000_000, STACK_RESERVATION_BYTES, &mut cancel)?;
        let mut replay = Replay::new(GOLDEN, base, &mut r)?;
        let mut count = 0;
        while let ReplayStep::Envelope(v) = replay.next_envelope(&mut Carriage, &mut r)? {
            let mut changes = v.changes();
            while changes.next_change(&mut r)?.is_some() {}
            count += 1;
        }
        encode_envelope(
            base,
            Envelope {
                batch: BatchId::new(1)?,
                kind: EnvelopeKind::Mutation,
                changes: &[],
                state: CommitState {
                    generation: GraphGeneration::new(1),
                    sequence: 1,
                    ..base
                },
            },
            &mut output,
            &mut r,
        )?;
        Ok(count)
    });
    assert_eq!(result, Ok(3));
    assert_eq!(
        (
            audit.allocations,
            audit.attributed_bytes,
            audit.unattributed_bytes
        ),
        (0, 0, 0)
    );
}
#[test]
fn wal_repaired_malformed_descriptor_rejection_allocates_zero_bytes() {
    // Commit's catalog ArtifactId is independently located after full graph roots.
    let mut bytes = GOLDEN.to_vec();
    let mut cursor = 64usize;
    loop {
        let length =
            u32::from_le_bytes(bytes[cursor + 8..cursor + 12].try_into().unwrap()) as usize;
        if bytes[cursor + 4] == 6 {
            break;
        }
        cursor += 72 + length;
    }
    let length = u32::from_le_bytes(bytes[cursor + 8..cursor + 12].try_into().unwrap()) as usize;
    let catalog_id = cursor + 64 + 16 + 104 + 8 * 104 + 16;
    bytes[catalog_id..catalog_id + 16].fill(0);
    let sum = xxhash_rust::xxh3::xxh3_64(&bytes[cursor..cursor + 64 + length]);
    bytes[cursor + 64 + length..cursor + 72 + length].copy_from_slice(&sum.to_le_bytes());
    let base = initial();
    let (result, audit) = audit_engine_path(|| -> Result<(), WalError> {
        let mut cancel = || false;
        let mut r = WalResources::new(100_000_000, STACK_RESERVATION_BYTES, &mut cancel)?;
        Replay::new(&bytes, base, &mut r)?.next_envelope(&mut Carriage, &mut r)?;
        Ok(())
    });
    assert_eq!(result, Err(WalError::Malformed));
    assert_eq!(
        (
            audit.allocations,
            audit.attributed_bytes,
            audit.unattributed_bytes
        ),
        (0, 0, 0)
    );
}
