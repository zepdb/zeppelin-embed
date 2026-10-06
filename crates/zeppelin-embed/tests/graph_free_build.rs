#![cfg(not(feature = "graph-cypher"))]
#![allow(clippy::expect_used)]

use zeppelin_embed::format::golden::decode_hex;
use zeppelin_embed::lifecycle::{OpenOptions, Store};

#[test]
fn a_v3_manifest_is_refused_with_graph_unsupported_build() {
    for options in [OpenOptions::read_only(), OpenOptions::new()] {
        let directory = tempfile::tempdir().expect("directory");
        let bytes = decode_hex(include_str!("fixtures/format/manifest_v3.hex")).expect("golden");
        std::fs::write(directory.path().join("manifest.ze"), &bytes).expect("manifest");
        let error = Store::open(directory.path(), options)
            .err()
            .expect("refusal");
        assert!(matches!(
            error,
            zeppelin_embed::lifecycle::StoreError::Manifest(
                zeppelin_embed::manifest::ManifestError::GraphUnsupportedBuild
            )
        ));
        assert_eq!(
            error.kind(),
            zeppelin_embed::lifecycle::StoreErrorKind::Unsupported
        );
        let files = std::fs::read_dir(directory.path())
            .expect("files")
            .map(|entry| {
                let entry = entry.expect("entry");
                (
                    entry.file_name(),
                    std::fs::read(entry.path()).expect("bytes"),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(files, vec![("manifest.ze".into(), bytes)]);
    }
}
