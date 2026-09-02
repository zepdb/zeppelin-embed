#![no_main]

use libfuzzer_sys::fuzz_target;
use zeppelin_embed::graph::consolidate::validate_consolidation_checkpoint;

fuzz_target!(|data: &[u8]| {
    let _ = validate_consolidation_checkpoint(data);
});
