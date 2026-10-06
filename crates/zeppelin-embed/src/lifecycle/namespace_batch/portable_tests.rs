#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
use super::*;
use crate::ingest::{DocumentVersion, IngestDocument, Revision, wal_payload};

const PARTICIPANT_TRANSCRIPT: &[u8] =
    b"ZENSPID1\x11\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x01\x00a";

fn checksum(mut bytes: Vec<u8>) -> Vec<u8> {
    bytes.extend_from_slice(&xxh3_64(&bytes).to_le_bytes());
    bytes
}

fn root_record(id: u128, descriptor: &[u8]) -> Vec<u8> {
    let mut bytes = b"ZENS0003".to_vec();
    bytes.extend_from_slice(&id.to_le_bytes());
    bytes.extend_from_slice(&(descriptor.len() as u32).to_le_bytes());
    bytes.extend_from_slice(descriptor);
    checksum(bytes)
}

fn reference(id: u128, depth: u8, name: &str) -> Vec<u8> {
    let mut bytes = b"ZENR0002".to_vec();
    bytes.extend_from_slice(&id.to_le_bytes());
    bytes.extend_from_slice(&[depth, 0]);
    bytes.extend_from_slice(&(name.len() as u16).to_le_bytes());
    bytes.extend_from_slice(name.as_bytes());
    checksum(bytes)
}

#[test]
fn portable_namespace_records_round_trip_and_reject_corruption() {
    let root = tempfile::tempdir().expect("root");
    let path = root.path().join(RECORD);
    let routes = Routes::from([("a".into(), ".ze-batch-1/a".into())]);
    let legacy = encode(&routes);
    let bytes = root_record(17, &legacy);
    std::fs::write(&path, &bytes).expect("portable root");
    let RootDescriptor::Legacy(decoded) = root_descriptor(root.path()).expect("portable routes")
    else {
        panic!("routes descriptor expected");
    };
    assert_eq!(encode(&decoded), legacy);
    assert_eq!(
        root_record_vfs(&StdVfs, root.path())
            .expect("portable identity")
            .0,
        Some(NamespaceRootId::new(17).expect("id"))
    );
    assert!(
        body(&path, &bytes).is_err(),
        "v0.6.0 route reader refuses v3"
    );
    assert!(
        decode_staged(&path, &bytes).is_err(),
        "v0.6.0 staged reader refuses v3"
    );
    for end in 0..bytes.len() {
        std::fs::write(&path, &bytes[..end]).expect("truncated root");
        assert!(root_descriptor(root.path()).is_err(), "truncation {end}");
    }
    let mut corrupt = bytes.clone();
    corrupt[24] ^= 1;
    let mut trailing = bytes.clone();
    trailing.push(0);
    let mut wrong_length = bytes[..bytes.len() - 8].to_vec();
    wrong_length[24..28].copy_from_slice(&u32::MAX.to_le_bytes());
    let mut extra = bytes[..bytes.len() - 8].to_vec();
    extra.push(0);
    let mut bad_descriptor = legacy.clone();
    bad_descriptor[8] ^= 1;
    for bad in [
        corrupt,
        trailing,
        checksum(wrong_length),
        checksum(extra),
        root_record(17, &bad_descriptor),
        root_record(0, &legacy),
        root_record(17, &bytes),
        root_record(17, &envelope(b"a\t../a\n")),
        vec![0; MAX_RECORD + 1],
    ] {
        std::fs::write(&path, bad).expect("invalid root");
        assert!(root_descriptor(root.path()).is_err());
    }
    let participant = reference(17, 1, "a");
    assert!(
        body(Path::new(REFERENCE), &participant).is_err(),
        "v0.6.0 reference reader refuses v2"
    );
    let decoded =
        portable::decode_reference(Path::new(REFERENCE), &participant).expect("reference");
    assert_eq!(
        portable::encode_reference(&decoded).expect("encode reference"),
        participant
    );
    assert_eq!(decoded.root_id.get(), 17);
    assert_eq!(decoded.parent_depth, 1);
    assert_eq!(decoded.name, "a");
    for end in 0..participant.len() {
        assert!(portable::decode_reference(Path::new(REFERENCE), &participant[..end]).is_err());
    }
    let mut reserved = participant[..participant.len() - 8].to_vec();
    reserved[25] = 1;
    let mut length = participant[..participant.len() - 8].to_vec();
    length[26] = 2;
    let mut extra = participant[..participant.len() - 8].to_vec();
    extra.push(0);
    let mut corrupt = participant.clone();
    corrupt[8] ^= 1;
    for bad in [
        checksum(reserved),
        checksum(length),
        checksum(extra),
        corrupt,
        reference(0, 1, "a"),
        reference(17, 0, "a"),
        reference(17, 3, "a"),
        reference(17, 1, ""),
        reference(17, 1, "../a"),
        reference(17, 1, "é"),
        reference(17, 1, &"a".repeat(256)),
    ] {
        assert!(portable::decode_reference(Path::new(REFERENCE), &bad).is_err());
    }
    assert!(NamespaceRootId::new(0).is_err());
    assert!(NamespaceRootId::from_entropy(|_| Ok(())).is_err());
    let entropy_error = NamespaceRootId::from_entropy(|_| {
        Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
    })
    .expect_err("entropy error");
    assert_eq!(entropy_error.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(
        NamespaceRootId::from_entropy(|bytes| {
            *bytes = 17_u128.to_le_bytes();
            Ok(())
        })
        .expect("entropy")
        .get(),
        17
    );
    assert_ne!(NamespaceRootId::generate().expect("OS randomness").get(), 0);
    let id = NamespaceRootId::new(17).expect("id");
    let identity = xxhash_rust::xxh3::xxh3_128(PARTICIPANT_TRANSCRIPT);
    assert_eq!(
        portable::participant_id(id, "a").expect("identity"),
        identity
    );
    let descriptor = RootDescriptor::Staged(StagedDescriptor(
        BTreeMap::from([(
            "a".into(),
            StagedSelection {
                manifest: ".ze-manifest-1".into(),
                binding: crate::ingest::wal_payload::TransactionBinding {
                    transaction: 1,
                    participant: identity,
                    first_seq: 1,
                    last_seq: 1,
                    manifest_digest: 1,
                    final_generation: 1,
                },
            },
        )]),
        routes,
    ));
    let RootDescriptor::Staged(staged) = &descriptor else {
        panic!("staged");
    };
    let bytes = root_record(17, &encode_staged(staged).expect("staged bytes"));
    let (decoded_id, decoded) = portable::decode_root(&path, &bytes).expect("staged root");
    assert_eq!(decoded, descriptor);
    assert_eq!(
        portable::encode_root(decoded_id, &decoded).expect("encode"),
        bytes
    );
    let mut wrong = staged.clone();
    wrong.0.get_mut("a").expect("a").binding.participant = 2;
    assert!(
        portable::decode_root(
            &path,
            &root_record(17, &encode_staged(&wrong).expect("foreign identity"))
        )
        .is_err()
    );
    wrong
        .0
        .insert("b".into(), wrong.0.get("a").expect("a").clone());
    assert!(encode_staged(&wrong).is_err(), "duplicate identities");
}

fn tree_bytes(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, directory: &Path, result: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(directory).expect("directory") {
            let entry = entry.expect("entry");
            if entry.file_type().expect("type").is_dir() {
                visit(root, &entry.path(), result);
            } else {
                result.insert(
                    entry
                        .path()
                        .strip_prefix(root)
                        .expect("relative")
                        .to_owned(),
                    std::fs::read(entry.path()).expect("bytes"),
                );
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result);
    result
}

fn prepared_wal(directory: &Path, binding: wal_payload::TransactionBinding) {
    let document = IngestDocument::new(
        DocumentVersion::new(DocId::new(7), Revision::new(1)),
        vec![1.0, 2.0],
    )
    .with_text("portable");
    let inner = wal_payload::encode_upsert_v2(&document).expect("upsert");
    let payload = wal_payload::encode_prepared(binding, 0, 1, wal_payload::UPSERT_V2, &inner)
        .expect("prepared");
    let record = crate::wal::record::encode_record(crate::wal::record::WalRecord {
        seq: crate::wal::LogSeq::new(binding.first_seq),
        op: wal_payload::PREPARED_MUTATION_V1,
        payload: &payload,
    })
    .expect("record");
    let path = directory.join("wal.ze");
    let mut bytes =
        std::fs::read(&path).expect("WAL")[..crate::wal::header::WAL_HEADER_LEN].to_vec();
    bytes.extend_from_slice(&record);
    std::fs::write(path, bytes).expect("prepared WAL");
}

// Hand-built portable authority around a sealed store, not a root writer or converter.
fn portable_fixture(
    root: &Path,
    routed: bool,
    accepted_locally: bool,
) -> wal_payload::TransactionBinding {
    let destination = if routed { ".ze-batch-1/a" } else { "a" };
    let directory = root.join(destination);
    std::fs::create_dir_all(root).expect("root");
    let store = Store::open(&directory, OpenOptions::new()).expect("empty participant");
    store
        .ingest(crate::ingest::IngestBatch::new(vec![IngestDocument::new(
            DocumentVersion::new(DocId::new(8), Revision::new(1)),
            vec![1.0, 2.0],
        )]))
        .expect("seed");
    store.seal().expect("persist manifest");
    store.close().expect("close");
    let path = directory.join("manifest.ze");
    let mut manifest =
        crate::manifest::decode_manifest("participant", &std::fs::read(&path).expect("manifest"))
            .expect("decode manifest");
    manifest.generation += 1;
    let generation = manifest.generation;
    let first_seq = manifest.log_seq + 1;
    let manifest = crate::manifest::encode_manifest(&manifest).expect("staged manifest");
    let binding = wal_payload::TransactionBinding {
        transaction: 1,
        participant: xxhash_rust::xxh3::xxh3_128(PARTICIPANT_TRANSCRIPT),
        first_seq,
        last_seq: first_seq,
        manifest_digest: xxh3_64(&manifest),
        final_generation: generation,
    };
    prepared_wal(&directory, binding);
    std::fs::write(directory.join(".ze-manifest-1"), &manifest).expect("staged");
    std::fs::write(
        directory.join(REFERENCE),
        reference(17, if routed { 2 } else { 1 }, "a"),
    )
    .expect("reference");
    let routes = if routed {
        std::fs::create_dir(root.join("a")).expect("logical participant");
        std::fs::write(root.join("a").join(REFERENCE), reference(17, 1, "a"))
            .expect("logical reference");
        let routes = Routes::from([("a".into(), destination.into())]);
        std::fs::write(root.join(".ze-batch-1/intent.ze"), encode(&routes)).expect("intent");
        std::fs::write(directory.join(PREPARED), envelope(destination.as_bytes()))
            .expect("preparation");
        routes
    } else {
        Routes::new()
    };
    let descriptor = if accepted_locally {
        std::fs::write(path, &manifest).expect("accepted manifest");
        std::fs::write(
            acceptance_path(&directory, binding),
            envelope(&binding.encode().expect("binding")),
        )
        .expect("acceptance");
        encode(&routes)
    } else {
        encode_staged(&StagedDescriptor(
            BTreeMap::from([(
                "a".into(),
                StagedSelection {
                    manifest: ".ze-manifest-1".into(),
                    binding,
                },
            )]),
            routes,
        ))
        .expect("selection")
    };
    std::fs::write(root.join(RECORD), root_record(17, &descriptor)).expect("portable root");
    binding
}

fn refuses_without_writing(root: &Path, directory: &Path) {
    let before = tree_bytes(root);
    let error = match Store::open(directory, OpenOptions::read_only()) {
        Ok(_) => panic!(
            "invalid portable participant opened: {}",
            directory.display()
        ),
        Err(error) => error,
    };
    match error {
        StoreError::Io { source, .. } => assert_eq!(source.kind(), std::io::ErrorKind::InvalidData),
        StoreError::WalMutation {
            op: 9,
            source: wal_payload::PayloadError::TransactionBinding,
            ..
        } => {}
        error => panic!("unexpected refusal: {error:?}"),
    }
    assert_eq!(tree_bytes(root), before, "refusal rewrote data");
}

#[test]
fn a_portable_namespace_requires_its_matching_root() {
    for routed in [false, true] {
        for accepted_locally in [false, true] {
            let scratch = tempfile::tempdir().expect("scratch");
            let original = scratch.path().join("original");
            let binding = portable_fixture(&original, routed, accepted_locally);
            // A root may itself have a transaction-looking basename: portable
            // ancestry is prescribed by the reference, never inferred from that name.
            let root = scratch.path().join(".ze-batch-relocated");
            std::fs::rename(original, &root).expect("move entire root");
            let directory = root.join(if routed { ".ze-batch-1/a" } else { "a" });
            let before = tree_bytes(&root);
            for path in [root.join("a"), directory.clone()] {
                let store = Store::open(&path, OpenOptions::read_only())
                    .expect("matching portable root opens after relocation");
                let (generation, rows) = store
                    .get_documents_with_generation(
                        &[DocId::new(7)],
                        super::super::DocumentFields::ALL,
                    )
                    .expect("prepared document");
                assert_eq!(generation, binding.final_generation);
                let row = rows
                    .first()
                    .expect("row")
                    .as_ref()
                    .expect("committed prepared row");
                assert_eq!(row.text.as_deref(), Some("portable"));
                assert_eq!(row.vector, Some(vec![1.0, 2.0]));
                store.close().expect("close reader");
            }
            assert_eq!(
                tree_bytes(&root),
                before,
                "read-only open upgraded or rewrote records"
            );
            let ref_path = directory.join(REFERENCE);
            let good_reference = std::fs::read(&ref_path).expect("reference");
            for bad in [
                reference(18, if routed { 2 } else { 1 }, "a"),
                reference(17, if routed { 1 } else { 2 }, "a"),
                reference(17, if routed { 2 } else { 1 }, "b"),
                envelope(
                    std::fs::canonicalize(&root)
                        .expect("root")
                        .to_str()
                        .expect("UTF8")
                        .as_bytes(),
                ),
            ] {
                std::fs::write(&ref_path, bad).expect("bad reference");
                refuses_without_writing(&root, &directory);
                if routed {
                    refuses_without_writing(&root, &root.join("a"));
                }
            }
            std::fs::remove_file(&ref_path).expect("remove reference");
            refuses_without_writing(&root, &directory);
            if routed {
                refuses_without_writing(&root, &root.join("a"));
            }
            std::fs::write(&ref_path, &good_reference).expect("restore reference");
            let root_path = root.join(RECORD);
            let good_root = std::fs::read(&root_path).expect("root bytes");
            std::fs::remove_file(&root_path).expect("detach root");
            refuses_without_writing(&root, &directory);
            std::fs::write(&root_path, &good_root).expect("restore root");
            if routed && accepted_locally {
                let foreign_routes = Routes::from([("a".into(), ".ze-batch-2/a".into())]);
                std::fs::write(&root_path, root_record(17, &encode(&foreign_routes)))
                    .expect("foreign route");
                refuses_without_writing(&root, &directory);
                refuses_without_writing(&root, &root.join("a"));
                std::fs::write(&root_path, &good_root).expect("restore route");
            }
            let renamed = directory.with_file_name("b");
            std::fs::rename(&directory, &renamed).expect("rename namespace");
            refuses_without_writing(&root, &renamed);
            std::fs::rename(&renamed, &directory).expect("restore namespace");
            if accepted_locally {
                // A valid legacy hash plus matching local acceptance is never a fallback.
                let foreign = wal_payload::TransactionBinding {
                    participant: participant_identity(&root, "a").expect("legacy hash"),
                    ..binding
                };
                prepared_wal(&directory, foreign);
                std::fs::write(
                    acceptance_path(&directory, foreign),
                    envelope(&foreign.encode().expect("foreign binding")),
                )
                .expect("foreign acceptance");
                refuses_without_writing(&root, &directory);
            }
        }
    }
}

fn private_fixture(root: &Path) -> PathBuf {
    portable_fixture(root, true, true);
    std::fs::write(root.join(RECORD), root_record(17, &encode(&Routes::new())))
        .expect("leave preparation unpublished");
    root.join(".ze-batch-1/a")
}

#[test]
fn an_ordinary_open_of_a_private_preparation_is_refused() {
    let root = tempfile::tempdir().expect("root");
    let directory = private_fixture(root.path());
    for options in [OpenOptions::read_only(), OpenOptions::new()] {
        let error = match Store::open(&directory, options) {
            Ok(_) => panic!("unpublished preparation opened"),
            Err(error) => error,
        };
        match error {
            StoreError::Io { source, .. } => assert_eq!(
                source.to_string(),
                "portable participant is outside its selected route"
            ),
            error => panic!("unexpected refusal: {error:?}"),
        }
    }
}

#[test]
fn the_coordinator_opens_its_private_preparation() {
    let root = tempfile::tempdir().expect("root");
    let directory = private_fixture(root.path());
    let coordinator = StoreLock::acquire(root.path()).expect("coordinator");
    let authority = PrivatePreparation::new(
        NamespaceRootId::new(17).expect("id"),
        "a",
        &directory,
        &coordinator,
    );
    let store = Store::open_private_preparation(&directory, OpenOptions::new(), authority)
        .expect("coordinator opens unpublished copy with acceptance");
    let rows = store
        .get_documents(&[DocId::new(7)], super::super::DocumentFields::ALL)
        .expect("prepared document");
    assert_eq!(
        rows[0].as_ref().expect("committed row").text.as_deref(),
        Some("portable")
    );
    store.seal().expect("checkpoint private participant");
    store.close().expect("close private participant");
}
