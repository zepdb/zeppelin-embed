# ZE-141 Standards review

Range verified: `e2d16b106593050711bc055d46d5673af0848bd7...65fbbbe853a89ca9e1342bb2be0182b48531fac2` is non-empty and contains exactly `65fbbbe ZE-141: convert native graph results directly to C`. None of the checkout's eight dirty paths intersects the 94 committed paths.

## Findings

- **Medium — hard breach:** `tasks/evidence/ze-141/run-mutations.py:296-307` does not compose multiple replacements to one file. `data = originals.setdefault(replacement.path, path.read_bytes())` returns the initially saved bytes on the second replacement, so mutation 03 writes the `Some(value)` change over the original file and silently discards the earlier `Some(text)` change. The committed RED confirms this: `mutation-03-presence-bits-red.log` shows `graph_result_native_all_pools_match_literal_field_oracle` passed while only the report test failed. This contradicts the AGENTS.md engineering rule to observe the intended RED and the exact two-part presence-coverage claim in `tasks/evidence/ze-141/README.md:47-55`. Read current path bytes for each replacement while saving the original once, then rerun mutation 03 and regenerate its evidence/hashes.

- **Low — judgement call, Duplicated Code:** `crates/zeppelin-embed-ffi/src/graph_result.rs:284-367` adds `map` and `generate` with duplicate checked-size, extent/alignment, 64-KiB chunk, `CopiedBytes` charge, and pointer-write loops (`let element_size ... context.charge(...) ... pointer.add(initialized).write(...)`). A common checked arena-writer primitive would keep this unsafe accounting invariant in one place.

- **Low — judgement call, Primitive Obsession/Data Clumps:** `crates/zeppelin-embed-ffi/src/graph_result/conversion.rs:132-179` and `registration.rs:133-161` pass arena geometry as positional `counts: [usize; 14]`, then recover meaning through a 14-slot destructure dominated by `_`. A named counts type would make pool correspondence explicit at this ownership/accounting seam.

No other documented-standard breach was found in production, tests, feature gates, ownership/accounting, panic-free paths, adversarial routing, or committed evidence. Dependency and public-ABI policy files are unchanged; recorded log hashes verify.
