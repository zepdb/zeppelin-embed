#![allow(clippy::expect_used)]

use std::process::Command;

#[test]
fn text_feature_off_leaves_the_library_byte_identical() {
    let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = std::fs::read_to_string(crate_dir.join("Cargo.toml")).expect("read manifest");
    assert!(manifest.lines().any(|line| line.starts_with("text =")));
    let workspace = crate_dir
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace");
    let target = tempfile::tempdir().expect("target tempdir");
    let build = || {
        let status = Command::new("cargo")
            .current_dir(workspace)
            .env("CARGO_TARGET_DIR", target.path())
            .args(["build", "-p", "zeppelin-embed-ffi", "--release"])
            .status()
            .expect("run cargo build");
        assert!(status.success());
        std::fs::read(target.path().join("release/libzeppelin_embed_ffi.a"))
            .expect("read release archive")
    };

    let first_archive = build();
    let second_archive = build();
    assert_eq!(first_archive, second_archive);
    let tree = Command::new("cargo")
        .current_dir(workspace)
        .args([
            "tree",
            "-p",
            "zeppelin-embed-ffi",
            "--no-default-features",
            "--edges",
            "normal",
        ])
        .output()
        .expect("inspect feature-off dependency tree");
    assert!(tree.status.success());
    let tree = String::from_utf8(tree.stdout).expect("UTF-8 cargo tree");
    assert!(!tree.contains("zeppelin-embed-text"));
    assert!(!tree.contains("mlx-rs"));
    assert!(!tree.contains("mlx-sys"));
    assert!(!tree.contains("cmake"));
    assert!(!tree.contains("bindgen"));

    let core_tree = Command::new("cargo")
        .current_dir(workspace)
        .args(["tree", "-p", "zeppelin-embed", "--edges", "normal,build"])
        .output()
        .expect("inspect core dependency tree");
    assert!(core_tree.status.success());
    let core_tree = String::from_utf8(core_tree.stdout).expect("UTF-8 core cargo tree");
    for forbidden in [
        "mlx-",
        "cmake",
        "bindgen",
        "serde",
        "safetensors",
        "tokenizers",
    ] {
        assert!(
            !core_tree.contains(forbidden),
            "core tree contains {forbidden}"
        );
    }
}
