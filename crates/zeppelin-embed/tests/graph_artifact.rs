#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use zeppelin_embed::property_graph::storage::artifact::{
    self, ArtifactId, ArtifactIdentity, Block, BlockKind, ContainerKind,
};
use zeppelin_embed::property_graph::{GraphGeneration, StoreInstanceId};
use zeppelin_embed::vfs::{StdVfs, Vfs};

fn identity() -> ArtifactIdentity {
    ArtifactIdentity {
        store: StoreInstanceId::new((1_u128 << 100) + 11).unwrap(),
        artifact: ArtifactId::new((1_u128 << 96) + 23).unwrap(),
        generation: GraphGeneration::new(7),
        creation_serial: 31,
    }
}

#[test]
fn tree_page_frames_keep_full_width_inline_overflow_and_infinite_keys_distinct() {
    use zeppelin_embed::property_graph::storage::tree::{
        self, Cell, Key, PAGE_BYTES, PageHeader, TreeKind,
    };
    let header = PageHeader {
        kind: TreeKind::Nodes,
        level: 0,
        generation: GraphGeneration::new(7),
    };
    let first = 255_u128.to_le_bytes();
    let second = ((1_u128 << 64) + 1).to_le_bytes();
    let cells = [
        Cell::Leaf {
            key: Key::Inline(&first),
            value: b"first",
        },
        Cell::Leaf {
            key: Key::Inline(&second),
            value: b"second",
        },
    ];
    let mut bytes = [0; PAGE_BYTES];
    tree::encode_page(header, &cells, &mut bytes).unwrap();
    let page = tree::decode_page(TreeKind::Nodes, &bytes).unwrap();
    assert_eq!(page.header(), header);
    assert_eq!(page.cell(0).unwrap(), cells[0]);
    assert_eq!(page.cell(1).unwrap(), cells[1]);
    assert_eq!(
        tree::compare_inline_keys(TreeKind::Nodes, &first, &second).unwrap(),
        std::cmp::Ordering::Less
    );
    let overflow = artifact::PhysicalRef {
        artifact: identity().artifact,
        offset: 96,
        length: 100,
        kind: BlockKind::OverflowKey,
        version: 1,
    };
    let child = artifact::PhysicalRef {
        artifact: identity().artifact,
        offset: 196,
        length: (PAGE_BYTES + 24) as u32,
        kind: BlockKind::TreePage,
        version: 1,
    };
    let header = PageHeader {
        kind: TreeKind::KeyFences,
        level: 1,
        ..header
    };
    let cells = [
        Cell::Branch {
            upper: Some(Key::Overflow {
                logical_length: 8 * 1024 * 1024,
                reference: overflow,
            }),
            child,
        },
        Cell::Branch { upper: None, child },
    ];
    tree::encode_page(header, &cells, &mut bytes).unwrap();
    let page = tree::decode_page(TreeKind::KeyFences, &bytes).unwrap();
    assert_eq!(page.cell(0).unwrap(), cells[0]);
    assert_eq!(page.cell(1).unwrap(), cells[1]);
}

#[test]
fn entropy_and_collisions_fail_without_replacing_published_or_orphan_objects() {
    use zeppelin_embed::property_graph::storage::allocation::{
        AllocationError, ArtifactAllocator, EntropyProvider, OsEntropy, artifact_path,
        fresh_store_identity,
    };
    struct Fixed(u128);
    impl EntropyProvider for Fixed {
        fn fill_nonce(&mut self, output: &mut [u8; 16]) -> std::io::Result<()> {
            *output = self.0.to_le_bytes();
            Ok(())
        }
    }
    struct Failed;
    impl EntropyProvider for Failed {
        fn fill_nonce(&mut self, _: &mut [u8; 16]) -> std::io::Result<()> {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "entropy probe",
            ))
        }
    }
    assert_eq!(
        fresh_store_identity(&mut Fixed(identity().store.get())).unwrap(),
        identity().store
    );
    assert_eq!(
        fresh_store_identity(&mut Fixed(0)).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
    assert_eq!(
        fresh_store_identity(&mut Failed).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    assert_ne!(fresh_store_identity(&mut OsEntropy).unwrap().get(), 0);
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    assert_eq!(
        fresh_store_identity(&mut OsEntropy).unwrap_err().kind(),
        std::io::ErrorKind::Unsupported
    );
    let directory = tempfile::tempdir().unwrap();
    let blocks = [Block {
        kind: BlockKind::CanonicalImage,
        payload: b"retained content",
    }];
    let mut scratch = [0xa5; 256];
    let mut fixed = Fixed(identity().artifact.get());
    let mut allocator = ArtifactAllocator {
        filesystem: &StdVfs,
        directory: directory.path(),
        store: identity().store,
        entropy: &mut fixed,
    };
    let made = allocator
        .create(GraphGeneration::new(7), 31, &blocks, &mut scratch)
        .unwrap();
    assert_eq!(made, identity());
    let path = artifact_path(directory.path(), made.artifact);
    let published = StdVfs.read(&path).unwrap();
    for original in [published.as_slice(), b"interrupted orphan bytes"] {
        // Explicit fixture setup, never the allocator's collision path.
        StdVfs.write(&path, original).unwrap();
        let error = allocator
            .create(GraphGeneration::new(8), 32, &blocks, &mut scratch)
            .unwrap_err();
        assert!(
            matches!(error, AllocationError::Collision { artifact, ref source } if artifact == identity().artifact && source.kind() == std::io::ErrorKind::AlreadyExists)
        );
        assert!(error.to_string().contains("collision; not owned"));
        assert!(
            error
                .to_string()
                .contains(&format!("{:032x}", identity().artifact.get()))
        );
        assert_eq!(StdVfs.read(&path).unwrap(), original);
    }
    let before = StdVfs.list(directory.path()).unwrap();
    let bytes_before = StdVfs.read(&path).unwrap();
    let mut failed = Failed;
    allocator.entropy = &mut failed;
    let error = allocator
        .create(GraphGeneration::new(8), 32, &blocks, &mut scratch)
        .unwrap_err();
    assert!(
        matches!(error, AllocationError::Entropy(ref e) if e.kind() == std::io::ErrorKind::PermissionDenied)
    );
    assert_eq!(error.to_string(), "graph entropy: entropy probe");
    assert_eq!(StdVfs.list(directory.path()).unwrap(), before);
    assert_eq!(StdVfs.read(&path).unwrap(), bytes_before);
}

#[test]
fn native_graph_only_object_reopens_full_width_identity_and_exact_payload() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph-only.object");
    let blocks = [
        Block {
            kind: BlockKind::CanonicalImage,
            payload: b"graph-only\0no vectors",
        },
        Block {
            kind: BlockKind::StoredText,
            payload: b"",
        },
    ];
    let mut bytes = [0_u8; 256];
    let used =
        artifact::encode_into(ContainerKind::Object, identity(), &blocks, &mut bytes).unwrap();
    StdVfs.create_new(&path, &bytes[..used]).unwrap();
    let reopened = StdVfs.read(&path).unwrap();
    let frame = artifact::decode(
        ContainerKind::Object,
        Some((identity().store, identity().artifact)),
        &reopened,
    )
    .unwrap();
    assert_eq!(frame.identity(), identity());
    for (index, block) in blocks.iter().enumerate() {
        let reference = frame.reference(index).unwrap();
        assert_eq!(reference.artifact.get(), (1_u128 << 96) + 23);
        assert_eq!(
            frame.resolve_framed_block(reference).unwrap(),
            block.payload
        );
    }
}

#[test]
fn native_graph_formats_are_required_and_append_only() {
    use zeppelin_embed::format::{FormatRegistry, RegistryError};
    for family in [17, 18] {
        assert!(FormatRegistry::require(family, 1).is_ok());
        assert!(matches!(
            FormatRegistry::require(family, 2),
            Err(RegistryError::UnsupportedVersion { .. })
        ));
    }
    assert_eq!(
        FormatRegistry::require(19, 1),
        Err(RegistryError::UnknownFamily(19))
    );
}

#[cfg(feature = "test-support")]
#[test]
fn exclusive_creation_survives_vfs_decorators_and_fault_images() {
    use zeppelin_embed::vfs::CountingVfs;
    use zeppelin_embed::vfs::crash::{CrashVfs, MemoryVfs, RecordingVfs};
    use zeppelin_embed::vfs::fault::{BlockingVfs, FaultVfs};
    let filesystems: Vec<Box<dyn Vfs>> = vec![
        Box::new(MemoryVfs::new()),
        Box::new(FaultVfs::new()),
        Box::new(CountingVfs::new(MemoryVfs::new())),
        Box::new(RecordingVfs::new(MemoryVfs::new())),
        Box::new(CrashVfs::new(MemoryVfs::new()).expect("crash fixture")),
        Box::new(BlockingVfs::new(MemoryVfs::new())),
    ];
    for filesystem in filesystems {
        let path = std::path::Path::new("/objects/exclusive");
        filesystem.create_new(path, b"original").expect("new file");
        assert_eq!(
            filesystem
                .create_new(path, b"replacement")
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(filesystem.read(path).unwrap(), b"original");
    }
}

#[test]
fn exclusive_object_creation_preserves_published_and_orphan_bytes() {
    let directory = tempfile::tempdir().expect("artifact directory");
    let filesystem = StdVfs;
    for (name, original) in [
        ("published.zgraph", b"published immutable bytes".as_slice()),
        ("orphan.zgraph", b"uncommitted partial bytes".as_slice()),
    ] {
        let path = directory.path().join(name);
        filesystem
            .create_new(&path, original)
            .expect("fresh object");
        let error = filesystem
            .create_new(&path, b"collision must not replace")
            .expect_err("occupied object name");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(filesystem.read(&path).expect("retained bytes"), original);
    }
}

fn hex(text: &str) -> Vec<u8> {
    zeppelin_embed::format::golden::decode_hex(text).unwrap()
}
fn object_golden() -> Vec<u8> {
    hex(include_str!("fixtures/format/native_graph_object_v1.hex"))
}
fn root_golden() -> Vec<u8> {
    hex(include_str!(
        "fixtures/format/native_graph_root_envelope_v1.hex"
    ))
}
fn leaf_golden() -> Vec<u8> {
    hex(include_str!(
        "fixtures/format/native_graph_inline_page_v1.hex"
    ))
}
fn branch_golden() -> Vec<u8> {
    hex(include_str!(
        "fixtures/format/native_graph_overflow_branch_v1.hex"
    ))
}
fn repair_file(bytes: &mut [u8]) {
    let end = bytes.len() - 8;
    let checksum = xxhash_rust::xxh3::xxh3_64(&bytes[..end]);
    bytes[end..].copy_from_slice(&checksum.to_le_bytes());
}
fn repair_page(bytes: &mut [u8]) {
    bytes[56..64].fill(0);
    let checksum = xxhash_rust::xxh3::xxh3_64(bytes);
    bytes[56..64].copy_from_slice(&checksum.to_le_bytes());
}

#[test]
fn native_graph_artifact_reference_and_page_bytes_match_independent_goldens() {
    use zeppelin_embed::property_graph::storage::tree::{self, PAGE_BYTES, TreeKind};
    for (kind, golden) in [
        (ContainerKind::Object, object_golden()),
        (ContainerKind::RootEnvelope, root_golden()),
    ] {
        let frame =
            artifact::decode(kind, Some((identity().store, identity().artifact)), &golden).unwrap();
        let count = if kind == ContainerKind::Object { 2 } else { 1 };
        let blocks: Vec<_> = (0..count)
            .map(|index| {
                let reference = frame.reference(index).unwrap();
                Block {
                    kind: reference.kind,
                    payload: frame.resolve_framed_block(reference).unwrap(),
                }
            })
            .collect();
        let mut output = vec![0xa5; golden.len()];
        assert_eq!(
            artifact::encode_into(kind, identity(), &blocks, &mut output).unwrap(),
            golden.len()
        );
        assert_eq!(output, golden);
    }
    let reference_bytes = hex(include_str!(
        "fixtures/format/native_graph_reference_v1.hex"
    ));
    let reference = artifact::decode_reference(&reference_bytes).unwrap();
    assert_eq!(reference.offset, 96);
    assert_eq!(reference.length, 100);
    assert_eq!(reference.kind, BlockKind::OverflowKey);
    let mut output = [0; 32];
    artifact::encode_reference(reference, &mut output).unwrap();
    assert_eq!(output.as_slice(), reference_bytes);
    for (kind, golden) in [
        (TreeKind::Nodes, leaf_golden()),
        (TreeKind::KeyFences, branch_golden()),
    ] {
        let page = tree::decode_page(kind, &golden).unwrap();
        let cells = [page.cell(0).unwrap(), page.cell(1).unwrap()];
        let mut output = [0xa5; PAGE_BYTES];
        tree::encode_page(page.header(), &cells, &mut output).unwrap();
        assert_eq!(output.as_slice(), golden);
    }
}

#[test]
fn incompatible_root_validation_never_mutates_root_or_neighbor_artifacts() {
    let directory = tempfile::tempdir().unwrap();
    let root_path = directory.path().join("manifest.ze");
    let object_path = directory.path().join("graph-existing.zgraph");
    let orphan_path = directory.path().join("graph-orphan.zgraph");
    StdVfs.write(&object_path, &object_golden()).unwrap();
    StdVfs.write(&orphan_path, b"uncommitted orphan").unwrap();
    let good = root_golden();
    StdVfs.write(&root_path, &good).unwrap();
    let reopened = StdVfs.read(&root_path).unwrap();
    let root = artifact::decode(ContainerKind::RootEnvelope, None, &reopened).unwrap();
    assert_eq!(root.identity().store, identity().store);
    assert_eq!(
        root.resolve_framed_block(root.reference(0).unwrap())
            .unwrap(),
        b"opaque checkpoint bytes"
    );
    // This is envelope compatibility validation, not complete GraphStore admission.
    for (offset, replacement) in [
        (8, vec![99, 0]),
        (10, vec![2, 0]),
        (12, vec![1]),
        (16, vec![32]),
        (24, vec![0]),
        (96, vec![99, 0]),
    ] {
        let mut incompatible = good.clone();
        incompatible[offset..offset + replacement.len()].copy_from_slice(&replacement);
        repair_file(&mut incompatible);
        StdVfs.write(&root_path, &incompatible).unwrap();
        let mut before = StdVfs.list(directory.path()).unwrap();
        before.sort();
        let images: Vec<_> = before
            .iter()
            .map(|path| StdVfs.read(path).unwrap())
            .collect();
        let opened = StdVfs.read(&root_path).unwrap();
        assert!(
            artifact::decode(ContainerKind::RootEnvelope, None, &opened).is_err(),
            "offset {offset}"
        );
        let mut after = StdVfs.list(directory.path()).unwrap();
        after.sort();
        assert_eq!(after, before);
        assert_eq!(
            after
                .iter()
                .map(|path| StdVfs.read(path).unwrap())
                .collect::<Vec<_>>(),
            images
        );
    }
}

#[test]
fn graph_object_corruption_and_wrong_references_are_rejected_before_payload_access() {
    use zeppelin_embed::format::frame::FormatCheck;
    let good = object_golden();
    for end in 0..good.len() {
        assert!(
            artifact::decode(ContainerKind::Object, None, &good[..end]).is_err(),
            "truncation {end}"
        );
    }
    let directory = 96 + u64::from_le_bytes(good[72..80].try_into().unwrap()) as usize;
    let patches: Vec<(&str, usize, Vec<u8>, FormatCheck)> = vec![
        ("magic", 0, b"NO".to_vec(), FormatCheck::Magic),
        (
            "family",
            8,
            18_u16.to_le_bytes().to_vec(),
            FormatCheck::Family,
        ),
        (
            "version",
            10,
            2_u16.to_le_bytes().to_vec(),
            FormatCheck::Version,
        ),
        ("flags", 12, vec![1], FormatCheck::HeaderLength),
        (
            "header-length",
            16,
            32_u64.to_le_bytes().to_vec(),
            FormatCheck::HeaderLength,
        ),
        (
            "file-length",
            24,
            u64::MAX.to_le_bytes().to_vec(),
            FormatCheck::FileLength,
        ),
        ("store-zero", 32, vec![0; 16], FormatCheck::ObjectIdentity),
        (
            "artifact-zero",
            48,
            vec![0; 16],
            FormatCheck::ObjectIdentity,
        ),
        (
            "body-overflow",
            72,
            u64::MAX.to_le_bytes().to_vec(),
            FormatCheck::BlockLength,
        ),
        (
            "count-overflow",
            80,
            u32::MAX.to_le_bytes().to_vec(),
            FormatCheck::BlockLength,
        ),
        ("prefix-flags", 84, vec![1], FormatCheck::BlockLength),
        (
            "block-kind",
            96,
            99_u16.to_le_bytes().to_vec(),
            FormatCheck::Family,
        ),
        (
            "block-version-mismatch",
            98,
            2_u16.to_le_bytes().to_vec(),
            FormatCheck::Family,
        ),
        ("block-flags", 100, vec![1], FormatCheck::BlockLength),
        (
            "payload-length-overflow",
            104,
            u64::MAX.to_le_bytes().to_vec(),
            FormatCheck::Length,
        ),
        (
            "payload-checksum",
            112,
            vec![0; 8],
            FormatCheck::BlockChecksum,
        ),
        (
            "directory-offset-overflow",
            directory,
            u64::MAX.to_le_bytes().to_vec(),
            FormatCheck::BlockLength,
        ),
        (
            "directory-length-zero",
            directory + 8,
            vec![0; 4],
            FormatCheck::BlockLength,
        ),
        (
            "directory-kind",
            directory + 12,
            99_u16.to_le_bytes().to_vec(),
            FormatCheck::Family,
        ),
        (
            "directory-version",
            directory + 14,
            2_u16.to_le_bytes().to_vec(),
            FormatCheck::Version,
        ),
        (
            "directory-checksum",
            directory + 16,
            vec![0; 8],
            FormatCheck::BlockChecksum,
        ),
        (
            "overlap",
            directory + 24,
            96_u64.to_le_bytes().to_vec(),
            FormatCheck::BlockLength,
        ),
    ];
    for (label, offset, replacement, check) in patches {
        let mut corrupt = good.clone();
        corrupt[offset..offset + replacement.len()].copy_from_slice(&replacement);
        repair_file(&mut corrupt);
        let error = artifact::decode(ContainerKind::Object, None, &corrupt).unwrap_err();
        assert_eq!(error.check(), check, "{label}: {error}");
    }
    let mut corrupt = good.clone();
    corrupt[120] ^= 1;
    assert_eq!(
        artifact::decode(ContainerKind::Object, None, &corrupt)
            .unwrap_err()
            .check(),
        FormatCheck::FileChecksum
    );
    let mut corrupt = good.clone();
    corrupt[120] ^= 1;
    repair_file(&mut corrupt);
    assert_eq!(
        artifact::decode(ContainerKind::Object, None, &corrupt)
            .unwrap_err()
            .check(),
        FormatCheck::BlockChecksum
    );
    let other_store = StoreInstanceId::new(11).unwrap();
    let other_artifact = ArtifactId::new(23).unwrap();
    for expected in [
        (other_store, identity().artifact),
        (identity().store, other_artifact),
    ] {
        assert_eq!(
            artifact::decode(ContainerKind::Object, Some(expected), &good)
                .unwrap_err()
                .check(),
            FormatCheck::ObjectIdentity
        );
    }
    let frame = artifact::decode(ContainerKind::Object, None, &good).unwrap();
    assert!(frame.reference(2).is_err());
    let reference = frame.reference(0).unwrap();
    for wrong in [
        artifact::PhysicalRef {
            artifact: other_artifact,
            ..reference
        },
        artifact::PhysicalRef {
            offset: reference.offset + 1,
            ..reference
        },
        artifact::PhysicalRef {
            length: reference.length - 1,
            ..reference
        },
        artifact::PhysicalRef {
            kind: BlockKind::NodeRecord,
            ..reference
        },
        artifact::PhysicalRef {
            version: 2,
            ..reference
        },
    ] {
        assert!(frame.resolve_framed_block(wrong).is_err(), "{wrong:?}");
    }
}

#[test]
fn complete_artifact_cap_includes_framing_and_rejects_before_output_mutation() {
    let payload = vec![0x5a; 4 * 1024 * 1024 - 24];
    let blocks = [Block {
        kind: BlockKind::CanonicalImage,
        payload: &payload,
    }];
    let mut output = vec![0xa5; 4 * 1024 * 1024 + 128];
    assert!(
        artifact::encode_into(ContainerKind::Object, identity(), &blocks, &mut output).is_err()
    );
    assert!(output.iter().all(|byte| *byte == 0xa5));
}

#[test]
fn tree_page_corruption_cannot_be_hidden_by_valid_outer_checksums() {
    use zeppelin_embed::property_graph::storage::tree::{self, TreeKind};
    let leaf = leaf_golden();
    let branch = branch_golden();
    for (kind, good) in [(TreeKind::Nodes, &leaf), (TreeKind::KeyFences, &branch)] {
        for end in 0..good.len() {
            assert!(
                tree::decode_page(kind, &good[..end]).is_err(),
                "truncated {end}"
            );
        }
        let mut bad = good.clone();
        bad[100] ^= 1;
        assert!(tree::decode_page(kind, &bad).is_err());
        let common = [
            (0, b"FAIL".to_vec()),
            (4, 2_u16.to_le_bytes().to_vec()),
            (6, 99_u16.to_le_bytes().to_vec()),
            (8, 0_u32.to_le_bytes().to_vec()),
            (12, u32::MAX.to_le_bytes().to_vec()),
            (18, vec![1]),
            (20, 0_u32.to_le_bytes().to_vec()),
            (32, vec![1]),
            (64, 81_u32.to_le_bytes().to_vec()),
            (68, u32::MAX.to_le_bytes().to_vec()),
            (72, 80_u32.to_le_bytes().to_vec()),
            (16_383, vec![1]),
        ];
        for (offset, replacement) in common {
            let mut bad = good.clone();
            bad[offset..offset + replacement.len()].copy_from_slice(&replacement);
            repair_page(&mut bad);
            assert!(
                tree::decode_page(kind, &bad).is_err(),
                "{kind:?} offset {offset}"
            );
        }
    }
    for (offset, replacement) in [
        (80, u32::MAX.to_le_bytes().to_vec()),
        (84, 0_u32.to_le_bytes().to_vec()),
        (88, vec![99]),
        (89, vec![1]),
        (92, 15_u64.to_le_bytes().to_vec()),
        (92, u64::MAX.to_le_bytes().to_vec()),
    ] {
        let mut bad = leaf.clone();
        bad[offset..offset + replacement.len()].copy_from_slice(&replacement);
        repair_page(&mut bad);
        assert!(
            tree::decode_page(TreeKind::Nodes, &bad).is_err(),
            "leaf offset {offset}"
        );
    }
    for (offset, replacement) in [
        (80, vec![1]),
        (81, vec![1]),
        (84, 43_u32.to_le_bytes().to_vec()),
        (116, 2_u16.to_le_bytes().to_vec()),
        (118, 2_u16.to_le_bytes().to_vec()),
        (120, vec![99]),
        (121, vec![1]),
        (124, 0_u64.to_le_bytes().to_vec()),
        (124, (8_u64 * 1024 * 1024 + 1).to_le_bytes().to_vec()),
        (132, vec![0; 16]),
        (148, u64::MAX.to_le_bytes().to_vec()),
        (156, 0_u32.to_le_bytes().to_vec()),
        (160, 2_u16.to_le_bytes().to_vec()),
        (162, 2_u16.to_le_bytes().to_vec()),
        (164, vec![0]),
        (164, vec![99]),
    ] {
        let mut bad = branch.clone();
        bad[offset..offset + replacement.len()].copy_from_slice(&replacement);
        repair_page(&mut bad);
        assert!(
            tree::decode_page(TreeKind::KeyFences, &bad).is_err(),
            "branch offset {offset}"
        );
    }
    let page = tree::decode_page(TreeKind::Nodes, &leaf).unwrap();
    assert!(page.cell(2).is_err());
}

#[test]
fn page_comparators_use_declared_numeric_fields_and_exact_key_bytes() {
    use std::cmp::Ordering;
    use zeppelin_embed::property_graph::storage::tree::{TreeKind, compare_inline_keys};
    let low = 255_u128.to_le_bytes();
    let high = ((1_u128 << 64) + 1).to_le_bytes();
    for kind in [
        TreeKind::Nodes,
        TreeKind::Relationships,
        TreeKind::ObjectInventory,
    ] {
        assert_eq!(
            compare_inline_keys(kind, &low, &high).unwrap(),
            Ordering::Less
        );
        assert_eq!(
            compare_inline_keys(kind, &high, &low).unwrap(),
            Ordering::Greater
        );
        assert_eq!(
            compare_inline_keys(kind, &high, &high).unwrap(),
            Ordering::Equal
        );
        assert!(compare_inline_keys(kind, &low[..15], &high).is_err());
    }
    for kind in [TreeKind::Labels, TreeKind::RelationshipTypes] {
        let mut left = 255_u64.to_le_bytes().to_vec();
        left.extend_from_slice(&high);
        let mut right = 256_u64.to_le_bytes().to_vec();
        right.extend_from_slice(&low);
        assert_eq!(
            compare_inline_keys(kind, &left, &right).unwrap(),
            Ordering::Less
        );
        right[..8].copy_from_slice(&255_u64.to_le_bytes());
        assert_eq!(
            compare_inline_keys(kind, &left, &right).unwrap(),
            Ordering::Greater
        );
    }
    for kind in [TreeKind::OutRanges, TreeKind::InRanges] {
        let mut left = low.to_vec();
        left.extend_from_slice(&255_u64.to_le_bytes());
        left.extend_from_slice(&high);
        let mut right = high.to_vec();
        right.extend_from_slice(&1_u64.to_le_bytes());
        right.extend_from_slice(&low);
        assert_eq!(
            compare_inline_keys(kind, &left, &right).unwrap(),
            Ordering::Less
        );
        right[..16].copy_from_slice(&low);
        right[16..24].copy_from_slice(&256_u64.to_le_bytes());
        assert_eq!(
            compare_inline_keys(kind, &left, &right).unwrap(),
            Ordering::Less
        );
        right[16..24].copy_from_slice(&255_u64.to_le_bytes());
        assert_eq!(
            compare_inline_keys(kind, &left, &right).unwrap(),
            Ordering::Greater
        );
    }
    let mut left = vec![0];
    left.extend_from_slice(&255_u64.to_le_bytes());
    left.extend_from_slice(b"z\0bytes");
    let mut right = vec![0];
    right.extend_from_slice(&256_u64.to_le_bytes());
    right.extend_from_slice(b"a");
    assert_eq!(
        compare_inline_keys(TreeKind::KeyFences, &left, &right).unwrap(),
        Ordering::Less
    );
    right[1..9].copy_from_slice(&255_u64.to_le_bytes());
    assert_eq!(
        compare_inline_keys(TreeKind::KeyFences, &left, &right).unwrap(),
        Ordering::Greater
    );
}

#[test]
fn artifact_and_page_preflight_reject_invalid_shapes_without_touching_output() {
    use zeppelin_embed::property_graph::storage::tree::{
        self, Cell, Key, PAGE_BYTES, PageHeader, TreeKind,
    };
    let block = Block {
        kind: BlockKind::StoredText,
        payload: b"x",
    };
    let mut small = [0xa5; 100];
    assert!(
        artifact::encode_into(ContainerKind::Object, identity(), &[block], &mut small).is_err()
    );
    assert_eq!(small, [0xa5; 100]);
    for blocks in [
        vec![],
        vec![block],
        vec![Block {
            kind: BlockKind::CheckpointPayload,
            payload: b"",
        }],
        vec![
            Block {
                kind: BlockKind::CheckpointPayload,
                payload: b"x"
            };
            2
        ],
    ] {
        let mut output = [0xa5; 512];
        assert!(
            artifact::encode_into(
                ContainerKind::RootEnvelope,
                identity(),
                &blocks,
                &mut output
            )
            .is_err()
        );
        assert!(output.iter().all(|byte| *byte == 0xa5));
    }
    assert!(
        artifact::encoded_len(
            ContainerKind::Object,
            &[Block {
                kind: BlockKind::CheckpointPayload,
                payload: b"x"
            }]
        )
        .is_err()
    );
    assert!(
        artifact::encoded_len(
            ContainerKind::Object,
            &vec![
                Block {
                    kind: BlockKind::StoredText,
                    payload: &[]
                };
                artifact::MAX_ARTIFACT_BYTES / 48 + 1
            ]
        )
        .is_err()
    );
    let maximum = vec![7_u8; artifact::MAX_ARTIFACT_BYTES - 152];
    let mut exact = vec![0; artifact::MAX_ARTIFACT_BYTES];
    assert_eq!(
        artifact::encode_into(
            ContainerKind::Object,
            identity(),
            &[Block {
                kind: BlockKind::StoredText,
                payload: &maximum
            }],
            &mut exact
        )
        .unwrap(),
        artifact::MAX_ARTIFACT_BYTES
    );
    assert!(artifact::decode(ContainerKind::Object, None, &exact).is_ok());
    let mut empty = [0; 104];
    assert_eq!(
        artifact::encode_into(ContainerKind::Object, identity(), &[], &mut empty).unwrap(),
        104
    );
    assert!(artifact::decode(ContainerKind::Object, None, &empty).is_ok());
    let reference = artifact::decode_reference(&hex(include_str!(
        "fixtures/format/native_graph_reference_v1.hex"
    )))
    .unwrap();
    for length in [0, 31, 33] {
        assert!(artifact::decode_reference(&vec![0; length]).is_err());
        assert!(artifact::encode_reference(reference, &mut vec![0; length]).is_err());
    }
    let header = PageHeader {
        kind: TreeKind::Nodes,
        level: 0,
        generation: GraphGeneration::new(7),
    };
    let id = 1_u128.to_le_bytes();
    let child = artifact::PhysicalRef {
        kind: BlockKind::TreePage,
        ..reference
    };
    let too_large = vec![0; PAGE_BYTES];
    let invalid = [
        vec![Cell::Leaf {
            key: Key::Inline(&id[..15]),
            value: b"",
        }],
        vec![Cell::Leaf {
            key: Key::Inline(&id),
            value: &too_large,
        }],
        vec![Cell::Leaf {
            key: Key::Overflow {
                logical_length: 100,
                reference,
            },
            value: b"",
        }],
        vec![Cell::Branch { upper: None, child }],
    ];
    for cells in invalid {
        let mut output = [0xa5; PAGE_BYTES];
        assert!(tree::encode_page(header, &cells, &mut output).is_err());
        assert!(output.iter().all(|byte| *byte == 0xa5));
    }
    assert!(tree::encode_page(header, &[], &mut [0; 12]).is_err());
    let branch_header = PageHeader { level: 1, ..header };
    for cells in [
        vec![],
        vec![Cell::Branch {
            upper: Some(Key::Inline(&id)),
            child,
        }],
        vec![Cell::Branch { upper: None, child }; 2],
    ] {
        let mut output = [0xa5; PAGE_BYTES];
        assert!(tree::encode_page(branch_header, &cells, &mut output).is_err());
        assert!(output.iter().all(|byte| *byte == 0xa5));
    }
}

#[test]
fn failed_create_reports_its_nonce_without_granting_collision_cleanup_ownership() {
    use std::path::Path;
    use zeppelin_embed::property_graph::storage::allocation::{
        ArtifactAllocator, EntropyProvider, artifact_path,
    };
    use zeppelin_embed::vfs::{SyncKind, VfsFile};
    struct Nonce;
    impl EntropyProvider for Nonce {
        fn fill_nonce(&mut self, output: &mut [u8; 16]) -> std::io::Result<()> {
            *output = identity().artifact.get().to_le_bytes();
            Ok(())
        }
    }
    struct Partial;
    impl Vfs for Partial {
        fn create_new(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
            StdVfs.create_new(path, &bytes[..37])?;
            Err(std::io::Error::other("partial create fault"))
        }
        fn ensure_directory(&self, path: &Path, create: bool) -> std::io::Result<bool> {
            StdVfs.ensure_directory(path, create)
        }
        fn open(&self, path: &Path) -> std::io::Result<u64> {
            StdVfs.open(path)
        }
        fn open_for_map(&self, path: &Path) -> std::io::Result<std::fs::File> {
            StdVfs.open_for_map(path)
        }
        fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            StdVfs.read(path)
        }
        fn read_range(&self, path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
            StdVfs.read_range(path, offset, length)
        }
        fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
            StdVfs.write(path, bytes)
        }
        fn open_append(&self, path: &Path) -> std::io::Result<Box<dyn VfsFile>> {
            StdVfs.open_append(path)
        }
        fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
            StdVfs.rename(from, to)
        }
        fn sync(&self, path: &Path, kind: SyncKind) -> std::io::Result<()> {
            StdVfs.sync(path, kind)
        }
        fn list(&self, path: &Path) -> std::io::Result<Vec<std::path::PathBuf>> {
            StdVfs.list(path)
        }
        fn delete(&self, path: &Path) -> std::io::Result<()> {
            StdVfs.delete(path)
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let mut entropy = Nonce;
    let mut allocator = ArtifactAllocator {
        filesystem: &Partial,
        directory: directory.path(),
        store: identity().store,
        entropy: &mut entropy,
    };
    let blocks = [Block {
        kind: BlockKind::CanonicalImage,
        payload: b"new object",
    }];
    let mut output = [0; 256];
    let error = allocator
        .create(GraphGeneration::new(7), 31, &blocks, &mut output)
        .unwrap_err();
    assert_eq!(error.attempted_artifact(), Some(identity().artifact));
    assert!(matches!(
        error,
        zeppelin_embed::property_graph::storage::allocation::AllocationError::CreateFailed { .. }
    ));
    let path = artifact_path(directory.path(), identity().artifact);
    let partial = StdVfs.read(&path).unwrap();
    assert_eq!(partial, output[..37]);
    let error = allocator
        .create(GraphGeneration::new(8), 32, &blocks, &mut output)
        .unwrap_err();
    assert_eq!(error.attempted_artifact(), Some(identity().artifact));
    assert!(matches!(
        error,
        zeppelin_embed::property_graph::storage::allocation::AllocationError::Collision { .. }
    ));
    assert_eq!(StdVfs.read(&path).unwrap(), partial);
}

#[test]
fn allocation_preflight_rejects_before_entropy_or_filesystem_access() {
    use zeppelin_embed::property_graph::storage::allocation::{
        AllocationError, ArtifactAllocator, EntropyProvider,
    };
    use zeppelin_embed::vfs::CountingVfs;
    struct Counted(usize);
    impl EntropyProvider for Counted {
        fn fill_nonce(&mut self, bytes: &mut [u8; 16]) -> std::io::Result<()> {
            self.0 += 1;
            *bytes = identity().artifact.get().to_le_bytes();
            Ok(())
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let fs = CountingVfs::new(StdVfs);
    let mut entropy = Counted(0);
    let blocks = [Block {
        kind: BlockKind::CanonicalImage,
        payload: b"content",
    }];
    let mut output = [0xa5; 158]; // One byte less than complete framed object.
    let error = ArtifactAllocator {
        filesystem: &fs,
        directory: directory.path(),
        store: identity().store,
        entropy: &mut entropy,
    }
    .create(GraphGeneration::new(0), 0, &blocks, &mut output)
    .unwrap_err();
    assert!(matches!(error, AllocationError::Format(_)));
    assert!(error.to_string().contains("output reservation too small"));
    assert_eq!(error.attempted_artifact(), None);
    assert_eq!(entropy.0, 0);
    assert_eq!(output, [0xa5; 158]);
    assert_eq!(fs.write_calls(), 0);
    assert!(
        std::fs::read_dir(directory.path())
            .unwrap()
            .next()
            .is_none()
    );
    let root_only = [Block {
        kind: BlockKind::CheckpointPayload,
        payload: b"checkpoint",
    }];
    let error = ArtifactAllocator {
        filesystem: &fs,
        directory: directory.path(),
        store: identity().store,
        entropy: &mut entropy,
    }
    .create(GraphGeneration::new(0), 0, &root_only, &mut output)
    .unwrap_err();
    assert!(matches!(error, AllocationError::Format(_)));
    assert_eq!(error.attempted_artifact(), None);
    assert_eq!(entropy.0, 0);
    assert_eq!(output, [0xa5; 158]);
    assert_eq!(fs.write_calls(), 0);
}
