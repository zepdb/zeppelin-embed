#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use zeppelin_embed::lifecycle::{OpenOptions, Store};

fn snapshot(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    std::fs::read_dir(path)
        .expect("store directory")
        .map(|entry| {
            let path = entry.expect("store entry").path();
            let bytes = std::fs::read(&path).expect("store file");
            (path, bytes)
        })
        .collect()
}

#[test]
fn legacy_open_refuses_native_graph_directory_before_any_mutation() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let native = parent.path().join("native");
    std::fs::create_dir(&native).expect("native directory");
    std::fs::write(native.join("graph-root.ze"), [0x40_u8; 120])
        .expect("recognizable native selector");
    let before = snapshot(&native);
    assert!(Store::open(&native, OpenOptions::new()).is_err());
    assert_eq!(snapshot(&native), before);

    let interrupted = parent.path().join("interrupted-native");
    std::fs::create_dir(&interrupted).expect("interrupted native directory");
    std::fs::write(
        interrupted.join("graph-wal-00000000000000000000000000000001.ze"),
        b"partial native initialization",
    )
    .expect("recognizable selector-absent native artifact");
    let before = snapshot(&interrupted);
    assert!(Store::open(&interrupted, OpenOptions::new()).is_err());
    assert_eq!(snapshot(&interrupted), before);

    let legacy = parent.path().join("legacy");
    let store = Store::open(&legacy, OpenOptions::new()).expect("ordinary legacy store");
    store.close().expect("close ordinary legacy store");
    assert!(legacy.is_dir());
    assert!(!snapshot(&legacy).keys().any(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("graph-") || name == "graph-root.ze")
    }));
}
