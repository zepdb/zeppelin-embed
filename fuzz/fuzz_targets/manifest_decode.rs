#![no_main]

use libfuzzer_sys::fuzz_target;
use zeppelin_embed::manifest::decode_manifest;

fuzz_target!(|data: &[u8]| {
    let _ = decode_manifest("fuzz-manifest", data);
});
