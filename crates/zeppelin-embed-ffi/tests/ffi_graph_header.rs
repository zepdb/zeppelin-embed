//! Compile the opt-in graph data contract as an actual external C consumer.
#![cfg(target_os = "macos")]
use std::path::Path;
use std::process::Command;

#[test]
fn graph_c_consumer_preserves_strong_full_width_identity_layouts() {
    let directory = tempfile::tempdir().expect("external C consumer");
    let source = directory.path().join("graph.c");
    let executable = directory.path().join("graph");
    std::fs::write(
        &source,
        r#"#include "zeppelin_graph_contracts.h"
_Static_assert(sizeof(ZeNodeId) == 16, "node width");
_Static_assert(sizeof(ZeRelId) == 16, "relationship width");
_Static_assert(offsetof(ZeNodeId, high) == 0, "high word first");
_Static_assert(offsetof(ZeNodeId, low) == 8, "low word second");
_Static_assert(sizeof(ZeGraphHandle) == 8, "graph handle wrapper");
int main(void) {
    ZeNodeId a = {UINT64_C(0x8000000000000000), UINT64_C(7)};
    ZeNodeId b = {UINT64_C(0x8000000000000001), UINT64_C(7)};
    ZeRelId r = {UINT64_MAX, UINT64_MAX};
    ZeGraphHandle handle = {UINT64_C(19)};
    return !(a.low == b.low && a.high != b.high &&
             r.high == UINT64_MAX && r.low == UINT64_MAX && handle.token == 19);
}
"#,
    )
    .expect("C source");
    let compile = Command::new("clang")
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-I"])
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("include"))
        .arg(&source)
        .arg("-o")
        .arg(&executable)
        .output()
        .expect("C compiler");
    assert!(
        compile.status.success(),
        "{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    assert!(
        Command::new(executable)
            .status()
            .expect("C consumer")
            .success()
    );
}

#[test]
fn graph_c_identity_kinds_cannot_be_assigned_to_each_other() {
    let directory = tempfile::tempdir().expect("external C type rejection");
    let source = directory.path().join("invalid.c");
    std::fs::write(
        &source,
        r#"#include "zeppelin_graph_contracts.h"
int main(void) {
    ZeRelId relationship = {1, 7};
    ZeNodeId node = relationship;
    return node.low == 7;
}
"#,
    )
    .expect("invalid cross-kind C source");
    let compile = Command::new("clang")
        .args([
            "-std=c11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-fsyntax-only",
            "-I",
        ])
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("include"))
        .arg(source)
        .output()
        .expect("C compiler");
    assert!(
        !compile.status.success(),
        "C accepted a relationship as a node identity"
    );
    assert!(String::from_utf8_lossy(&compile.stderr).contains("incompatible type"));
}

#[test]
fn graph_contract_header_is_exact_separate_cbindgen_output() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let version = Command::new("cbindgen")
        .arg("--version")
        .output()
        .expect("cbindgen installed");
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("cbindgen 0.29."));
    let generated = Command::new("cbindgen")
        .current_dir(root)
        .args(["--config", "cbindgen.graph.toml", "src/graph_contracts.rs"])
        .output()
        .expect("graph cbindgen");
    assert!(
        generated.status.success(),
        "{}",
        String::from_utf8_lossy(&generated.stderr)
    );
    assert_eq!(
        generated.stdout,
        std::fs::read(root.join("include/zeppelin_graph_contracts.h")).expect("graph header")
    );
}

#[test]
fn legacy_header_and_exports_do_not_advertise_graph_contracts() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let header =
        std::fs::read_to_string(root.join("include/zeppelin_embed.h")).expect("legacy header");
    for forbidden in [
        "ZeGraph",
        "ZeNodeId",
        "ZeRelId",
        "ze_graph_",
        "zeppelin_graph_contracts.h",
    ] {
        assert!(
            !header.contains(forbidden),
            "legacy header advertises {forbidden}"
        );
    }
    let symbols = std::fs::read_to_string(root.join("symbols.allowlist")).expect("legacy symbols");
    // Graph exports are allowlisted separately from the legacy header surface.
    assert_eq!(
        symbols
            .lines()
            .filter(|s| s.starts_with("ze_graph_"))
            .collect::<Vec<_>>(),
        ["ze_graph_open_with_relationship_types"]
    );
}

fn compile_and_run(name: &str, source_text: &str) {
    let directory = tempfile::tempdir().expect("external graph consumer");
    let source = directory.path().join(format!("{name}.c"));
    let executable = directory.path().join(name);
    std::fs::write(&source, source_text).unwrap();
    let result = Command::new("clang")
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-I"])
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("include"))
        .arg(source)
        .arg("-o")
        .arg(&executable)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(Command::new(executable).status().unwrap().success());
}

#[test]
fn graph_c_batch_pool_distinguishes_empty_payloads_and_local_endpoints() {
    compile_and_run("batch", include_str!("c/graph_batch.c"));
}

#[test]
fn graph_c_typed_plan_retains_search_presence_components_and_path_predicates() {
    compile_and_run("plan", include_str!("c/graph_plan.c"));
}

#[test]
fn graph_c_requests_and_response_preserve_disposition_and_search_provenance() {
    compile_and_run("response", include_str!("c/graph_response.c"));
}

#[test]
fn graph_c_frozen_sizes_offsets_and_discriminants_match_the_reviewed_contract() {
    compile_and_run("layout", include_str!("c/graph_layout.c"));
}

#[test]
fn graph_contract_header_is_usable_from_cpp_without_legacy_export_changes() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("graph.cpp");
    std::fs::write(
        &source,
        r#"#include "zeppelin_graph_contracts.h"
#include <type_traits>
static_assert(!std::is_same<ZeNodeId, ZeRelId>::value, "distinct identity domains");
int main() { ZeGraphDiagnostic diagnostic{}; diagnostic.operator_index.present = 1;
return diagnostic.operator_index.present != 1; }
"#,
    )
    .unwrap();
    let result = Command::new("clang++")
        .args([
            "-std=c++17",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-fsyntax-only",
            "-I",
        ])
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("include"))
        .arg(source)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn graph_entry_prototypes_are_callable_from_c() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let result = Command::new("clang")
        .args([
            "-std=c11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-fsyntax-only",
            "-I",
        ])
        .arg(root.join("include"))
        .arg(root.join("tests/c/graph_entry.c"))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
