#![allow(clippy::expect_used, clippy::indexing_slicing)]
//! Query paths must never call a checksum-verifying segment accessor.
//!
//! Each verifying accessor re-hashes an entire region. On a query path
//! that cost lands on every call: hybrid cross-fill once called the
//! verifying rescore accessor per cross-filled document and spent 219 ms
//! of a 221 ms query re-hashing 181 MB. The same mistake was made four
//! times in one day, in `stored_text`, the hybrid rescore, the scan
//! path's codes and factors, and `exact_vector_ceiling`. It is invisible
//! to every other test: nothing fails, the query is simply slow.
//!
//! Validation, diagnostics and maintenance still verify, so this guard
//! covers only the files a query executes in.

use std::path::Path;

/// Accessors that hash a whole region before returning it.
const VERIFYING: [&str; 8] = [
    ".f32_codes()",
    ".bit4_codes()",
    ".int8_codes()",
    ".bit4_factors()",
    ".int8_factors()",
    ".rescore_f32()",
    ".stored_text()",
    ".graph_node_blocks()",
];

/// Files a query executes in. Maintenance files are deliberately absent.
const QUERY_PATH_FILES: [&str; 4] = [
    "src/planner/exec.rs",
    "src/planner/lexical.rs",
    "src/lifecycle/mod.rs",
    "src/lifecycle/hybrid.rs",
];

/// Lines that legitimately verify, with the reason they may.
fn is_allowed(file: &str, line: &str) -> bool {
    // The snapshot gate proves a region is semantically valid before the
    // snapshot becomes reachable by any query. Once per snapshot.
    file.ends_with("snapshot.rs") && line.contains("stored_text")
}

#[test]
fn query_paths_never_call_a_verifying_segment_accessor() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut offenders = Vec::new();
    for relative in QUERY_PATH_FILES {
        let path = root.join(relative);
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        for (number, line) in source.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with("///") {
                continue;
            }
            for accessor in VERIFYING {
                if line.contains(accessor) && !is_allowed(relative, line) {
                    offenders.push(format!(
                        "{relative}:{} calls {accessor}; use the query_* twin",
                        number.saturating_add(1)
                    ));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "query paths must use validate-once accessors:\n  {}",
        offenders.join("\n  ")
    );
}
