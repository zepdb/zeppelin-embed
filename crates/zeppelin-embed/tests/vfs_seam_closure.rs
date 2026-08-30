#![allow(clippy::expect_used)]

fn contains_direct_call(source: &str, call: &str) -> bool {
    source.match_indices(call).any(|(offset, _)| {
        source
            .get(..offset)
            .and_then(|prefix| prefix.chars().next_back())
            .is_none_or(|character| !character.is_ascii_alphanumeric() && character != '_')
    })
}

#[test]
fn ci_gate_denies_direct_std_fs_in_segment_reader_and_graph_build() {
    let gate = include_str!("../../../scripts/ci-gates.sh");
    assert!(
        gate.contains("ZE_VFS_SEAM_CLOSURE")
            && gate.contains("segment/reader.rs")
            && gate.contains("graph/build.rs"),
        "ci-gates.sh does not guard both closed VFS seams"
    );

    for (path, source) in [
        (
            "segment/reader.rs",
            include_str!("../src/segment/reader.rs")
                .split("\nmod tests {")
                .next()
                .expect("source"),
        ),
        (
            "graph/build.rs",
            include_str!("../src/graph/build.rs")
                .split("\nmod tests {")
                .next()
                .expect("source"),
        ),
    ] {
        for denied in [
            "use std::fs;",
            "std::fs::read(",
            "std::fs::write(",
            "std::fs::rename(",
            "std::fs::remove_file(",
            "fs::read(",
            "fs::write(",
            "fs::rename(",
            "fs::remove_file(",
        ] {
            assert!(
                !source.contains(denied),
                "{path} contains forbidden direct filesystem operation {denied}"
            );
        }
        for denied in ["File::open(", "File::create("] {
            assert!(
                !contains_direct_call(source, denied),
                "{path} contains forbidden direct filesystem operation {denied}"
            );
        }
    }
}
