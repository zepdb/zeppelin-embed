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
fn ci_gate_denies_direct_std_fs_in_closed_vfs_modules() {
    let gate = include_str!("../../../scripts/ci-gates.sh");
    const DENIED_REGEX: &str = "VFS_SEAM_DENIED='(^|[^[:alnum:]_])((std::)?fs::(read|read_to_string|write|rename|remove_file|metadata|copy|hard_link|create_dir_all)|File::(open|create|options))\\(|(std::)?fs::OpenOptions::new\\(|use[[:space:]]+std::fs([[:space:]]+as[[:space:]]+[[:alnum:]_]+)?[[:space:]]*;|use[[:space:]]+std::fs::(OpenOptions|read|read_to_string|write|rename|remove_file|metadata|copy|hard_link|create_dir_all)[[:space:]]*;'";
    assert!(
        gate.contains("ZE_VFS_SEAM_CLOSURE") && gate.contains(DENIED_REGEX),
        "ci-gates.sh does not pin the complete direct-filesystem deny regex"
    );

    for (path, source) in [
        (
            "segment/reader.rs",
            include_str!("../src/segment/reader.rs"),
        ),
        ("graph/build.rs", include_str!("../src/graph/build.rs")),
        ("lifecycle/mod.rs", include_str!("../src/lifecycle/mod.rs")),
    ] {
        for denied in [
            "use std::fs;",
            "use std::fs as ",
            "use std::fs::OpenOptions;",
            "std::fs::read(",
            "std::fs::read_to_string(",
            "std::fs::write(",
            "std::fs::rename(",
            "std::fs::remove_file(",
            "std::fs::metadata(",
            "std::fs::copy(",
            "std::fs::hard_link(",
            "std::fs::create_dir_all(",
            "std::fs::OpenOptions::new(",
            "fs::read(",
            "fs::read_to_string(",
            "fs::write(",
            "fs::rename(",
            "fs::remove_file(",
            "fs::metadata(",
            "fs::copy(",
            "fs::hard_link(",
            "fs::create_dir_all(",
            "fs::OpenOptions::new(",
        ] {
            assert!(
                !source.contains(denied),
                "{path} contains forbidden direct filesystem operation {denied}"
            );
        }
        for denied in ["File::open(", "File::create(", "File::options("] {
            assert!(
                !contains_direct_call(source, denied),
                "{path} contains forbidden direct filesystem operation {denied}"
            );
        }
    }
}
