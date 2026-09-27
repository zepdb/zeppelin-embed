#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use std::io::Read;
use zeppelin_embed::lifecycle::{CancelToken, OpenOptions, QueryControl, Store};
use zeppelin_embed::property_graph::{
    GraphGeneration, StoreInstanceId,
    resources::GraphResources,
    staging::{WriteLimits, WriteMemory},
    storage::{
        artifact::*,
        memory::StorageMemory,
        tree::directory::{BlockSource, TreeError, TreeResources},
    },
};

#[test]
fn physical_artifact_reopen_bounds_reads_and_polls_between_interrupted_attempts() {
    let directory = tempfile::tempdir().unwrap();
    let owner = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&owner).unwrap();
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let token = CancelToken::new();
    let control = QueryControl::Cancel(token.clone());
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
    let mut r = TreeResources::for_prepare(&memory, 10_000_000).unwrap();
    let identity = ArtifactIdentity {
        store: StoreInstanceId::new(1).unwrap(),
        artifact: ArtifactId::new(19).unwrap(),
        generation: GraphGeneration::new(3),
        creation_serial: 7,
    };
    let payload = vec![0x5a; 180_000];
    let mut private = PrivateArtifact::new(identity, 256 * 1024, 1, &memory, &mut r).unwrap();
    let reference = private
        .append(BlockKind::StoredText, &payload, &mut r)
        .unwrap();
    private.seal(&mut r).unwrap();
    let bytes = private.sealed_bytes().unwrap();
    let path = directory.path().join("reopened.graph");
    std::fs::write(&path, bytes).unwrap();
    struct Reader {
        file: std::fs::File,
        calls: usize,
        largest: usize,
        interrupt: bool,
        token: CancelToken,
    }
    impl Read for Reader {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            self.calls += 1;
            self.largest = self.largest.max(output.len());
            if self.interrupt && self.calls == 2 {
                self.token.cancel();
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            if self.interrupt && self.calls > 2 {
                return Err(std::io::ErrorKind::Other.into());
            }
            self.file.read(output)
        }
    }
    let mut reader = Reader {
        file: std::fs::File::open(&path).unwrap(),
        calls: 0,
        largest: 0,
        interrupt: false,
        token: token.clone(),
    };
    let before = memory.reserved_bytes();
    let admitted = OwnedArtifact::read_from(
        &mut reader,
        bytes.len(),
        (identity.store, identity.artifact),
        &memory,
        &mut r,
    )
    .unwrap();
    assert_eq!(reader.calls, 4, "three bounded reads plus exact EOF check");
    assert_eq!(reader.largest, 65_536);
    assert_eq!(
        admitted.resolve(reference, &mut r).unwrap().payload(),
        &payload
    );
    assert_eq!(memory.reserved_bytes() - before, admitted.owned_bytes());
    drop(admitted);
    assert_eq!(memory.reserved_bytes(), before);
    let mut cancelled = Reader {
        file: std::fs::File::open(&path).unwrap(),
        calls: 0,
        largest: 0,
        interrupt: true,
        token,
    };
    let result = OwnedArtifact::read_from(
        &mut cancelled,
        bytes.len(),
        (identity.store, identity.artifact),
        &memory,
        &mut r,
    );
    assert!(
        matches!(result, Err(TreeError::Control(_))),
        "cancellation must be polled between interrupted read attempts"
    );
    drop(result);
    assert_eq!(
        cancelled.calls, 2,
        "no retry I/O after observed cancellation"
    );
    assert_eq!(
        memory.reserved_bytes(),
        before,
        "failed admission releases actual buffer"
    );
}

struct Missing;
impl BlockSource for Missing {
    fn resolve<'a>(
        &'a self,
        _: PhysicalRef,
        _: &mut TreeResources<'_>,
    ) -> Result<FramedBlock<'a>, TreeError> {
        Err(TreeError::Missing)
    }
}
#[test]
fn failed_private_append_seal_and_finish_keep_inventory_without_exposing_artifacts() {
    use zeppelin_embed::property_graph::storage::{
        prepared::{PackLimits, PreparedObjects},
        tree::directory::BlockSink,
    };
    let directory = tempfile::tempdir().unwrap();
    let owner = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&owner).unwrap();
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
    let mut r = TreeResources::for_prepare(&memory, 100_000_000).unwrap();
    let identity = ArtifactIdentity {
        store: StoreInstanceId::new(1).unwrap(),
        artifact: ArtifactId::new(19).unwrap(),
        generation: GraphGeneration::new(3),
        creation_serial: 7,
    };
    let payload = vec![0x5a; 128_000];
    let before = memory.reserved_bytes();
    let (append_work, seal_work, admitted_reference) = {
        let mut clean = PrivateArtifact::new(identity, 256 * 1024, 2, &memory, &mut r).unwrap();
        let start = r.work();
        let admitted_reference = clean
            .append(BlockKind::StoredText, &payload, &mut r)
            .unwrap();
        let append = r.work() - start;
        let start = r.work();
        clean.seal(&mut r).unwrap();
        assert!(clean.sealed_bytes().is_some());
        (append, r.work() - start, admitted_reference)
    };
    assert_eq!(memory.reserved_bytes(), before);
    for limit in [payload.len() as u64 + 1, append_work - 1] {
        let mut pack = PrivateArtifact::new(identity, 256 * 1024, 2, &memory, &mut r).unwrap();
        let mut limited = TreeResources::for_prepare(&memory, limit).unwrap();
        let result = pack.append(BlockKind::StoredText, &payload, &mut limited);
        assert!(
            matches!(result, Err(TreeError::Work)),
            "append work refusal {limit}"
        );
        assert_eq!(pack.identity(), identity);
        assert!(
            pack.framed_block(admitted_reference, &mut r).is_err(),
            "failed validation cannot grant a reusable entry"
        );
        assert!(pack.sealed_bytes().is_none());
        assert!(
            pack.seal(&mut r).is_err(),
            "failed mutation cannot be finalized on retry"
        );
    }
    for limit in [payload.len() as u64 + 1, append_work - 1] {
        let mut pack = PrivateArtifact::new(identity, 256 * 1024, 2, &memory, &mut r).unwrap();
        let first = pack
            .append(BlockKind::StoredText, b"previously admitted", &mut r)
            .unwrap();
        assert_eq!(
            pack.framed_block(first, &mut r).unwrap().payload(),
            b"previously admitted"
        );
        let mut limited = TreeResources::for_prepare(&memory, limit).unwrap();
        assert!(matches!(
            pack.append(BlockKind::StoredText, &payload, &mut limited),
            Err(TreeError::Work)
        ));
        let mut fresh = TreeResources::for_prepare(&memory, 100_000_000).unwrap();
        assert!(
            matches!(
                pack.framed_block(first, &mut fresh),
                Err(TreeError::Invalid(_))
            ),
            "later failed append revokes prior admitted entries with fresh control"
        );
        assert!(pack.sealed_bytes().is_none());
    }
    for limit in [0, 24, 120, seal_work / 2, seal_work - 1] {
        let mut pack = PrivateArtifact::new(identity, 256 * 1024, 2, &memory, &mut r).unwrap();
        pack.append(BlockKind::StoredText, &payload, &mut r)
            .unwrap();
        let mut limited = TreeResources::for_prepare(&memory, limit).unwrap();
        assert!(
            matches!(pack.seal(&mut limited), Err(TreeError::Work)),
            "seal work refusal {limit}"
        );
        assert_eq!(pack.identity(), identity);
        assert!(
            pack.sealed_bytes().is_none(),
            "no bytes after failed complete framing"
        );
        assert!(pack.seal(&mut r).is_err());
    }
    assert_eq!(memory.reserved_bytes(), before);
    for limit in [0, 1, 120, 250] {
        let mut serial = 0u64;
        let mut prepared = PreparedObjects::new(
            &Missing,
            || {
                serial += 1;
                Ok(ArtifactIdentity {
                    artifact: ArtifactId::new(serial as u128).unwrap(),
                    creation_serial: serial,
                    ..identity
                })
            },
            identity.store,
            identity.generation,
            PackLimits {
                artifact_bytes: 1024,
                blocks: 1,
                ..PackLimits::default()
            },
            &memory,
            &mut r,
        )
        .unwrap();
        for _ in 0..3 {
            prepared
                .append(
                    BlockKind::StoredText,
                    identity.generation,
                    b"bounded",
                    &mut r,
                )
                .unwrap();
        }
        let inventory: Vec<_> = prepared.abort_inventory().collect();
        assert_eq!(inventory.len(), 3);
        let retained = memory.reserved_bytes();
        let mut limited = TreeResources::for_prepare(&memory, limit).unwrap();
        assert!(
            matches!(prepared.finish(&mut limited), Err(TreeError::Work)),
            "final participant refuses {limit}"
        );
        assert_eq!(prepared.abort_inventory().collect::<Vec<_>>(), inventory);
        for index in 0..3 {
            assert!(
                prepared.artifact(index).is_err(),
                "even previously sealed packs stay private when finalization fails"
            );
        }
        assert!(prepared.finish(&mut r).is_err());
        drop(limited);
        assert_eq!(
            memory.reserved_bytes(),
            retained,
            "failed finalization retains owned abort backing without new allocation"
        );
    }
    assert_eq!(memory.reserved_bytes(), before);
}

#[test]
fn malformed_truncated_trailing_and_wrong_store_files_release_owned_admission_buffers() {
    let directory = tempfile::tempdir().unwrap();
    let owner = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&owner).unwrap();
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
    let mut r = TreeResources::for_prepare(&memory, 10_000_000).unwrap();
    let identity = ArtifactIdentity {
        store: StoreInstanceId::new(1).unwrap(),
        artifact: ArtifactId::new(19).unwrap(),
        generation: GraphGeneration::new(3),
        creation_serial: 7,
    };
    let mut private = PrivateArtifact::new(identity, 1024, 1, &memory, &mut r).unwrap();
    private
        .append(BlockKind::StoredText, b"complete", &mut r)
        .unwrap();
    private.seal(&mut r).unwrap();
    let bytes = private.sealed_bytes().unwrap();
    let before = memory.reserved_bytes();
    for case in 0..4 {
        let mut fixture = bytes.to_vec();
        let mut expected = (identity.store, identity.artifact);
        match case {
            0 => {
                fixture.pop();
            }
            1 => fixture.push(0),
            2 => fixture[120] ^= 0x80,
            3 => expected.0 = StoreInstanceId::new(2).unwrap(),
            _ => unreachable!(),
        }
        let path = directory.path().join(format!("refused-{case}.graph"));
        std::fs::write(&path, &fixture).unwrap();
        let result = OwnedArtifact::read_from(
            &mut std::fs::File::open(path).unwrap(),
            bytes.len(),
            expected,
            &memory,
            &mut r,
        );
        assert!(result.is_err(), "failed file case {case}");
        drop(result);
        assert_eq!(
            memory.reserved_bytes(),
            before,
            "failure {case} releases actual admission capacity"
        );
    }
    assert!(matches!(
        OwnedArtifact::read_from(
            &mut std::fs::File::open(directory.path().join("refused-0.graph")).unwrap(),
            0,
            (identity.store, identity.artifact),
            &memory,
            &mut r
        ),
        Err(TreeError::Invalid(_))
    ));
    assert_eq!(memory.reserved_bytes(), before);
}

#[test]
fn admitted_private_block_reuse_keeps_exact_reference_checks_with_bounded_work() {
    let directory = tempfile::tempdir().unwrap();
    let owner = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&owner).unwrap();
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
    let mut r = TreeResources::for_prepare(&memory, 10_000_000).unwrap();
    let identity = ArtifactIdentity {
        store: StoreInstanceId::new(1).unwrap(),
        artifact: ArtifactId::new(19).unwrap(),
        generation: GraphGeneration::new(3),
        creation_serial: 7,
    };
    let mut pack = PrivateArtifact::new(identity, 256 * 1024, 2, &memory, &mut r).unwrap();
    let reference = pack
        .append(BlockKind::StoredText, &vec![0x5a; 65_536], &mut r)
        .unwrap();
    let before = r.work();
    let capacity = memory.reserved_bytes();
    let repeated = || {
        for _ in 0..10 {
            let block = pack.framed_block(reference, &mut r).unwrap();
            assert_eq!(block.reference(), reference);
            assert_eq!(block.payload().len(), 65_536);
            assert_eq!(block.payload().first(), Some(&0x5a));
        }
    };
    #[cfg(feature = "allocation-audit")]
    {
        let (_, audit) = zeppelin_embed::adversarial_test_support::audit_engine_path(repeated);
        assert_eq!(audit.allocations, 0);
        assert_eq!(audit.attributed_bytes, 0);
        assert_eq!(audit.unattributed_bytes, 0);
    }
    #[cfg(not(feature = "allocation-audit"))]
    {
        let mut repeated = repeated;
        repeated();
    }
    assert_eq!(memory.reserved_bytes(), capacity);
    assert!(
        r.work() - before <= 2048,
        "exact repeated reference lookup must reuse complete admission, not rehash unchanged payload bytes"
    );
    pack.append(BlockKind::StoredText, b"second", &mut r)
        .unwrap();
    assert_eq!(
        pack.framed_block(reference, &mut r)
            .unwrap()
            .payload()
            .first(),
        Some(&0x5a)
    );
    for bad in [
        PhysicalRef {
            length: reference.length - 1,
            ..reference
        },
        PhysicalRef {
            kind: BlockKind::NodeRecord,
            ..reference
        },
        PhysicalRef {
            version: 2,
            ..reference
        },
        PhysicalRef {
            offset: reference.offset + 1,
            ..reference
        },
    ] {
        assert!(
            pack.framed_block(bad, &mut r).is_err(),
            "entire admitted reference must match"
        );
    }
    pack.seal(&mut r).unwrap();
    assert_eq!(
        pack.framed_block(reference, &mut r)
            .unwrap()
            .payload()
            .first(),
        Some(&0x5a)
    );
}

#[test]
fn payload_window_reads_charge_only_the_requested_field() {
    use zeppelin_embed::property_graph::storage::{payload::PayloadRef, stream::PayloadSlice};
    let directory = tempfile::tempdir().unwrap();
    let owner = Store::open(
        directory.path(),
        OpenOptions::new().with_max_resident_bytes(256 * 1024 * 1024),
    )
    .unwrap();
    let shared = GraphResources::from_store(&owner).unwrap();
    let writer = WriteMemory::new(&shared, WriteLimits::default()).unwrap();
    let control = QueryControl::Cancel(CancelToken::new());
    let memory = StorageMemory::new(&writer, &control, 32 * 1024 * 1024).unwrap();
    let mut r = TreeResources::for_prepare(&memory, 10_000_000).unwrap();
    let identity = ArtifactIdentity {
        store: StoreInstanceId::new(1).unwrap(),
        artifact: ArtifactId::new(19).unwrap(),
        generation: GraphGeneration::new(3),
        creation_serial: 7,
    };
    let mut pack = PrivateArtifact::new(identity, 128 * 1024, 1, &memory, &mut r).unwrap();
    let reference = pack
        .append(BlockKind::StoredText, &vec![0x5a; 65_536], &mut r)
        .unwrap();
    pack.seal(&mut r).unwrap();
    let bytes = pack.sealed_bytes().unwrap();
    let admitted = OwnedArtifact::read_from(
        &mut std::io::Cursor::new(bytes),
        bytes.len(),
        (identity.store, identity.artifact),
        &memory,
        &mut r,
    )
    .unwrap();
    let reference = PayloadRef::new(BlockKind::StoredText, 65_536, reference).unwrap();
    let slice = PayloadSlice::new(&admitted, identity.store, identity.generation, reference);
    let mut first_work = None;
    for offset in [0, 32_000, 65_528] {
        let before = r.work();
        let mut field = [0; 8];
        assert_eq!(
            slice
                .subslice(offset, 8)
                .unwrap()
                .read_at(0, &mut field, &mut r)
                .unwrap(),
            8
        );
        assert_eq!(field, [0x5a; 8]);
        let used = r.work() - before;
        assert!(
            used <= 128,
            "an eight-byte window must not charge unrelated trailing bytes: {used}"
        );
        assert_eq!(
            *first_work.get_or_insert(used),
            used,
            "equal windows have equal work regardless of unrelated tail length"
        );
    }
}
