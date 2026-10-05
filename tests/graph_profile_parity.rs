#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::result_large_err
)]
#[path = "support/graph_profile.rs"]
mod graph_profile;
#[test]
fn ze74_local_profile_uses_independent_rust_and_c_plans() {
    let manifest = graph_profile::manifest();
    for case in manifest["local"].as_array().unwrap() {
        if case["structured"].is_null() {
            continue;
        }
        for path in ["rust-structured", "rust-cypher", "c-structured", "c-cypher"] {
            let r = graph_profile::run_local(case, path)
                .unwrap_or_else(|e| panic!("{} {path}: {e}", case["id"]));
            println!("{r}");
        }
    }
}

#[path = "../crates/zeppelin-embed-cypher/tests/support/conformance.rs"]
mod conformance;
#[path = "../crates/zeppelin-embed-cypher/tests/support/graph.rs"]
mod graph;
#[path = "../crates/zeppelin-embed-cypher/tests/support/mod.rs"]
mod support;
use graph_profile::tck;
#[test]
fn ze74_original_rust_cypher_profile_preserves_pinned_expectations() {
    for (write, fixture) in [
        (
            false,
            include_str!("../crates/zeppelin-embed-cypher/tests/fixtures/read-tck-execution.txt"),
        ),
        (
            true,
            include_str!("../crates/zeppelin-embed-cypher/tests/fixtures/write-tck-execution.txt"),
        ),
    ] {
        for scenario in tck::scenarios(fixture) {
            conformance::run_scenario(&scenario, write);
        }
    }
}
#[test]
fn ze74_public_rejections_preserve_state() {
    let manifest = graph_profile::manifest();
    for case in manifest["local"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| !c["error"].is_null())
    {
        for path in ["rust-cypher", "c-cypher"] {
            println!(
                "{}",
                graph_profile::run_local(case, path)
                    .unwrap_or_else(|e| panic!("{} {path}: {e}", case["id"]))
            );
        }
    }
}
#[test]
fn ze74_original_c_cypher_profile_preserves_pinned_expectations() {
    let mut failures = vec![];
    for case in graph_profile::original_cases() {
        match graph_profile::run_local(&case, "c-cypher") {
            Ok(r) => println!("{r}"),
            Err(e) => failures.push(format!("{}: {e}", case["id"])),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
#[test]
fn ze74_high_ids_and_owned_values_survive_c_release_and_reopen() {
    graph_profile::high_id_roundtrip().unwrap();
}

#[test]
fn ze74_invalid_typed_with_scope_is_atomic() {
    let manifest = graph_profile::manifest();
    let mut case = manifest["local"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "local/with-scope")
        .unwrap()
        .clone();
    case["structured"]["expressions"][1]["value"] = zeppelin_embed_bench::harness_json::json!(999);
    case["error"] = zeppelin_embed_bench::harness_json::json!({"rust":"Scope","c":"ZeErrInvalidArgument","stage":"typed-plan-validation"});
    for path in ["rust-structured", "c-structured"] {
        graph_profile::run_local(&case, path).unwrap_or_else(|e| panic!("{path}: {e}"));
    }
}
