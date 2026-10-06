#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use zeppelin_embed::format::golden::decode_hex;
use zeppelin_embed::manifest::decode_manifest;
#[cfg(feature = "graph-cypher")]
use zeppelin_embed::manifest::encode_manifest;

fn golden() -> Vec<u8> {
    decode_hex(include_str!("fixtures/format/manifest_v3.hex")).unwrap()
}

#[cfg(feature = "graph-cypher")]
#[test]
fn manifest_v3_round_trips_the_graph_state_and_inventory() {
    let bytes = golden();
    let manifest = decode_manifest("manifest_v3.hex", &bytes).expect("v3 graph manifest");
    assert_eq!(manifest.generation, 9);
    assert_eq!(manifest.log_seq, 41);
    let graph = manifest.graph.as_ref().unwrap();
    assert_eq!(graph.graph_absorbed_through, 37);
    let state = graph.state().unwrap();
    assert_eq!(state.store.get(), 11);
    assert_eq!(state.generation.get(), 9);
    assert_eq!(state.sequence, 7);
    assert_eq!(
        state.high_waters,
        zeppelin_embed::property_graph::wal::HighWaters {
            node: 23,
            relationship: 29,
            symbols: [31, 37, 41, 43],
            creation_serial: 47,
        }
    );
    assert!(state.vector.is_none());
    assert!(state.text.is_none());
    assert!(state.graph.slots.iter().all(Option::is_some));
    assert_eq!(state.reclaim, Some(state.catalog));
    assert_eq!(state.prepared_inventories.len().unwrap(), 2);
    assert_eq!(
        graph.objects,
        vec![13, 17]
            .into_iter()
            .map(|id| zeppelin_embed::manifest::GraphObject {
                artifact: zeppelin_embed::property_graph::storage::artifact::ArtifactId::new(id)
                    .unwrap(),
                length: 4096,
                checksum: 19,
            })
            .collect::<Vec<_>>()
    );
    let rebuilt =
        zeppelin_embed::manifest::GraphManifest::new(state, 37, graph.objects.clone()).unwrap();
    assert_eq!(&rebuilt, graph);
    assert_eq!(encode_manifest(&manifest).unwrap(), bytes);
}

#[test]
fn a_graph_free_store_still_writes_a_v2_manifest() {
    use zeppelin_embed::lifecycle::durability::{CommitTier, DurabilityMode, DurabilityPolicy};
    use zeppelin_embed::manifest::io::{commit_manifest, load_manifest};
    use zeppelin_embed::vfs::StdVfs;
    let bytes = decode_hex(include_str!("fixtures/format/manifest_v2.hex")).unwrap();
    let manifest = decode_manifest("manifest_v2.hex", &bytes).unwrap();
    #[cfg(feature = "graph-cypher")]
    assert!(manifest.graph.is_none());
    let directory = tempfile::tempdir().unwrap();
    let policy = DurabilityPolicy::new(DurabilityMode::Durable, CommitTier::Ordered).unwrap();
    commit_manifest(&StdVfs, directory.path(), &manifest, policy).unwrap();
    let path = directory.path().join("manifest.ze");
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(load_manifest(&StdVfs, &path, u64::MAX).unwrap(), manifest);
}

// Actual unmodified v0.6.0 frame reader, with its old manifest-only registry.
#[allow(dead_code)]
#[path = "fixtures/format/v2_reader/mod.rs"]
mod v2_reader;

#[test]
fn the_v2_reader_refuses_v3_with_format_check_version() {
    let bytes = golden();
    let error = v2_reader::frame::decode_artifact(
        "manifest_v3.hex",
        v2_reader::FormatFamily::Manifest,
        &bytes,
    )
    .unwrap_err();
    assert_eq!(error.check(), v2_reader::frame::FormatCheck::Version);
    assert_eq!(error.version_range(), Some((3, 2, 2)));
    let old = decode_hex(include_str!("fixtures/format/manifest_v2.hex")).unwrap();
    v2_reader::frame::decode_artifact("manifest_v2.hex", v2_reader::FormatFamily::Manifest, &old)
        .unwrap();
}

#[cfg(feature = "graph-cypher")]
#[test]
fn manifest_v3_round_trips_after_the_clustering_extension() {
    use zeppelin_embed::segment::{ClusteringKeyRange, SegmentId, SegmentMeta};
    let mut manifest = decode_manifest("golden", &golden()).unwrap();
    manifest.segments.push(SegmentMeta {
        id: SegmentId::new(1, [2; 10]),
        row_count: 3,
        scheme: 0,
        dims: 4,
        file_size: 4096,
        epoch_id: None,
        clustering_key_range: ClusteringKeyRange::Bounded {
            min_ts: -7,
            max_ts: 19,
        },
    });
    let encoded = encode_manifest(&manifest).unwrap();
    assert_eq!(decode_manifest("with-tsr1", &encoded).unwrap(), manifest);
}

#[cfg(feature = "graph-cypher")]
#[test]
fn manifest_v3_rejects_malformed_graph_sections() {
    use xxhash_rust::xxh3::xxh3_64;
    use zeppelin_embed::format::FormatFamily;
    use zeppelin_embed::format::frame::{decode_artifact, encode_artifact};
    use zeppelin_embed::manifest::ManifestError;
    let bytes = golden();
    let original = decode_artifact("golden", FormatFamily::Manifest, &bytes)
        .unwrap()
        .payload;
    // v2 prefix 56 bytes; graph magic/length 8 bytes; graph body 1440 bytes.
    // Recompute enclosing checksums so each mutation reaches its intended check.
    for (offset, replacement, repair_section, expected) in [
        (28, 1, true, "reserved field"),
        (56, b'?', true, "magic"),
        (60, 0, false, "checksum"),
        (64, 2, true, "presence"),
        (64, 0, true, "presence"),
        (65, 1, true, "reserved"),
        (80, 0, true, "count/length"),
        (188, 2, true, "graph WAL"),
        (1436, 3, true, "count/length"),
        (1440, 0, true, "zero artifact identity"),
        (1456, 1, true, "differs from inventory"),
        (1504, 1, false, "checksum"),
    ] {
        let mut payload = original.to_vec();
        assert_ne!(payload[offset], replacement);
        payload[offset] = replacement;
        if repair_section {
            let hash = xxh3_64(&payload[56..1504]);
            payload[1504..1512].copy_from_slice(&hash.to_le_bytes());
        }
        let mut encoded = encode_artifact(FormatFamily::Manifest, 0, &payload);
        encoded[10..12].copy_from_slice(&3_u16.to_le_bytes());
        let end = encoded.len() - 8;
        let hash = xxh3_64(&encoded[..end]);
        encoded[end..].copy_from_slice(&hash.to_le_bytes());
        let error = decode_manifest("mutated", &encoded).unwrap_err();
        assert!(
            matches!(&error, ManifestError::Decode(detail) if detail.contains(expected))
                || matches!(&error, ManifestError::Format(error) if error.to_string().contains(expected)),
            "offset {offset}: {error}, expected {expected}"
        );
    }
}

#[cfg(feature = "graph-cypher")]
#[test]
fn manifest_v3_refuses_separate_search_roots_and_incomplete_inventory() {
    use zeppelin_embed::manifest::GraphManifest;
    let manifest = decode_manifest("golden", &golden()).unwrap();
    let graph = manifest.graph.unwrap();
    for vector in [true, false] {
        let mut state = graph.state().unwrap();
        if vector {
            state.vector = Some(state.catalog);
        } else {
            state.text = Some(state.catalog);
        }
        assert!(GraphManifest::new(state, 37, graph.objects.clone()).is_err());
    }
    assert!(GraphManifest::new(graph.state().unwrap(), 37, Vec::new()).is_err());
    let mut duplicates = graph.objects.clone();
    duplicates.push(duplicates[0]);
    assert!(GraphManifest::new(graph.state().unwrap(), 37, duplicates).is_err());
}

#[test]
fn existing_manifest_goldens_are_unchanged() {
    // Pinned from the pre-U6 HEAD, including rejected v1 inputs.
    for (text, expected) in [
        (
            include_str!("fixtures/format/manifest_v1.hex"),
            4556058009399764488_u64,
        ),
        (
            include_str!("fixtures/format/manifest_v2.hex"),
            15445090099742069595_u64,
        ),
        (
            include_str!("fixtures/format/manifest_clustering_ranges_v1.hex"),
            6724958261673345612_u64,
        ),
        (
            include_str!("fixtures/format/manifest_clustering_ranges_v2.hex"),
            16491714693973491312_u64,
        ),
    ] {
        assert_eq!(
            xxhash_rust::xxh3::xxh3_64(&decode_hex(text).unwrap()),
            expected
        );
    }
}
