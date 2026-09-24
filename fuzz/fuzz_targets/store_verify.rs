//! Fuzzes store verification and the recovery path it vouches for.
//!
//! Input layout: `[manifest_len: u32 LE][wal_len: u32 LE][manifest][wal]
//! [segment]`. The manifest and WAL bytes become `manifest.ze` and `wal.ze`;
//! when the manifest decodes and names a segment, the remaining bytes become
//! that segment's file. Verification must never panic, and a store it calls
//! clean must open writable through the real recovery path. The only allowed
//! refusal is `EpochUndeclared`, which is the caller's missing declaration,
//! not damage.
#![no_main]

use std::path::PathBuf;
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use zeppelin_embed::lifecycle::{OpenOptions, Store, StoreError};
use zeppelin_embed::manifest::decode_manifest;
use zeppelin_embed::verify::verify_store;

fn directory() -> &'static PathBuf {
    static DIRECTORY: OnceLock<PathBuf> = OnceLock::new();
    DIRECTORY.get_or_init(|| {
        std::env::temp_dir().join(format!("ze-fuzz-store-verify-{}", std::process::id()))
    })
}

fn split(data: &[u8]) -> Option<(&[u8], &[u8], &[u8])> {
    let manifest_len = u32::from_le_bytes(data.get(0..4)?.try_into().ok()?) as usize;
    let wal_len = u32::from_le_bytes(data.get(4..8)?.try_into().ok()?) as usize;
    let rest = data.get(8..)?;
    let manifest = rest.get(..manifest_len.min(rest.len()))?;
    let rest = rest.get(manifest.len()..)?;
    let wal = rest.get(..wal_len.min(rest.len()))?;
    let segment = rest.get(wal.len()..)?;
    Some((manifest, wal, segment))
}

fuzz_target!(|data: &[u8]| {
    let Some((manifest, wal, segment)) = split(data) else {
        return;
    };
    let directory = directory();
    let _ = std::fs::remove_dir_all(directory);
    std::fs::create_dir_all(directory).expect("fuzz store directory");
    if !manifest.is_empty() {
        std::fs::write(directory.join("manifest.ze"), manifest).expect("write manifest");
    }
    if !wal.is_empty() {
        std::fs::write(directory.join("wal.ze"), wal).expect("write WAL");
    }
    if let Ok(decoded) = decode_manifest("fuzz", manifest)
        && let Some(first) = decoded.segments.first()
        && !segment.is_empty()
    {
        std::fs::write(directory.join(first.id.file_name()), segment).expect("write segment");
    }

    let report = verify_store(directory).expect("verify runs on an existing directory");
    if report.is_clean() {
        match Store::open(directory, OpenOptions::default()) {
            Ok(store) => {
                let _ = store.close();
            }
            Err(StoreError::EpochUndeclared) => {}
            Err(error) => panic!("verify called a store clean that recovery refuses: {error}"),
        }
    }
});
