#![no_main]

use libfuzzer_sys::fuzz_target;
use zeppelin_embed::segment::reader::validate_segment_bytes;

fuzz_target!(|data: &[u8]| {
    let _ = validate_segment_bytes(data);
});
