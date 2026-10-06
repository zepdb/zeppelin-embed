#[allow(dead_code)]
mod common;
use zeppelin_embed::format::frame::FormatCheck;
use zeppelin_embed::lifecycle::{Store, StoreError};
use zeppelin_embed::manifest::ManifestError;
use zeppelin_embed_ffi::*;
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("store path");
    let read_only = args.get(2).expect("access mode") == "ro";
    let version_refused = match Store::open(path, common::options(read_only)) {
        Ok(store) => {
            assert_eq!(common::text_hits(&store, "orchard").first(), Some(&1));
            store.close().expect("close");
            false
        }
        Err(StoreError::Manifest(ManifestError::Format(error))) => {
            assert_eq!(error.check(), FormatCheck::Version);
            assert_eq!(error.version_range(), Some((3, 2, 2)));
            true
        }
        Err(error) => panic!("unexpected open error: {error:?}"),
    };
    if !version_refused {
        println!("{{\"version_refused\": false, \"abi_code\": null}}");
        return;
    }
    let request = ZeOpenRequest {
        abi_size: std::mem::size_of::<ZeOpenRequest>() as u32,
        abi_reserved: 0,
        path: path.as_ptr(),
        path_len: path.len(),
        access_mode: i32::from(read_only),
        durability_mode: 0,
        commit_tier: 0,
        reader_drain_timeout_ms: 0,
        max_resident_bytes: 1 << 30,
        max_temp_bytes: 1 << 30,
    };
    let mut handle = 0;
    let code = ze_open(&request, &mut handle);
    assert_eq!(code, ZeErrorCode::ZeErrFormatTooNew);
    assert_eq!(code as i32, 56);
    assert_eq!(handle, 0);
    println!(
        "{{\"version_refused\": {version_refused}, \"abi_code\": {}}}",
        code as i32
    );
}
