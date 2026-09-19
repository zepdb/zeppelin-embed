#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"
probe_root="$repo_root/tools/graph-feature-probe"
target_root="${CARGO_TARGET_DIR:-$repo_root/target}/graph-feature-boundary"
temporary="$(mktemp -d)"
trap 'rm -rf "$temporary"' EXIT

export CARGO_TERM_COLOR=never

"$script_dir/check-graph-feature-routing.sh"

default_core_log="$temporary/default-core.log"
if cargo check \
    --manifest-path "$probe_root/core/Cargo.toml" \
    --target-dir "$target_root/core-default" \
    >"$default_core_log" 2>&1; then
    echo "default core consumer unexpectedly imported property_graph" >&2
    exit 1
fi
if ! grep -q 'could not find `property_graph` in `zeppelin_embed`' "$default_core_log"; then
    cat "$default_core_log" >&2
    echo "default core failure was not the intended unavailable-module diagnostic" >&2
    exit 1
fi

cargo check \
    --manifest-path "$probe_root/core-graph/Cargo.toml" \
    --target-dir "$target_root/core-graph"

check_ffi_dependencies() {
    local selection="$1"
    shift
    cargo metadata \
        --manifest-path "$probe_root/ffi/Cargo.toml" \
        --format-version 1 \
        --filter-platform aarch64-apple-darwin \
        "$@" \
        | python3 -c '
import json
import pathlib
import sys

selection = sys.argv[1]
metadata = json.load(sys.stdin)
packages = {package["id"]: package for package in metadata["packages"]}
ffi_id = next(
    package_id
    for package_id, package in packages.items()
    if package["name"] == "zeppelin-embed-ffi"
    and pathlib.Path(package["manifest_path"]).parent.name == "zeppelin-embed-ffi"
)
nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
direct = sorted(dependency["name"] for dependency in nodes[ffi_id]["deps"])
expected = (
    ["zeppelin_embed"]
    if selection == "default"
    else ["zeppelin_embed", "zeppelin_embed_cypher"]
)
if direct != expected:
    raise SystemExit(
        f"{selection} FFI direct dependencies: expected {expected!r}, observed {direct!r}"
    )
if selection == "graph":
    cypher_id = next(
        dependency["pkg"]
        for dependency in nodes[ffi_id]["deps"]
        if dependency["name"] == "zeppelin_embed_cypher"
    )
    cypher_direct = sorted(dependency["name"] for dependency in nodes[cypher_id]["deps"])
    if cypher_direct != ["zeppelin_embed"]:
        raise SystemExit(
            "graph compiler dependencies: expected internal core only, "
            f"observed {cypher_direct!r}"
        )
print(f"{selection} FFI direct dependencies: {direct}")
' "$selection"
}

check_ffi_dependencies default
check_ffi_dependencies graph --features graph-cypher

cargo metadata \
    --manifest-path "$repo_root/Cargo.toml" \
    --format-version 1 \
    --no-deps \
    | python3 -c '
import json
import sys

metadata = json.load(sys.stdin)
packages = {package["name"]: package for package in metadata["packages"]}

core_graph_tests = {
    "graph_adjacency",
    "graph_artifact",
    "graph_canonical",
    "graph_catalog",
    "graph_checked_utf8",
    "graph_compiled_context",
    "graph_completed_predicate",
    "graph_completed_results",
    "graph_directories",
    "graph_key_lifecycle",
    "graph_query_allocation",
    "graph_query_external_capacity",
    "graph_query_plan",
    "graph_query_runtime",
    "graph_query_runtime_control",
    "graph_query_values",
    "graph_relational",
    "graph_storage_failures",
    "graph_storage_prepare",
    "graph_wal",
    "graph_write_staging",
}

def selected_targets(package_name, feature):
    package = packages[package_name]
    return {
        target["name"]
        for target in package["targets"]
        if (target.get("required-features") or []) == [feature]
    }

observed_core = selected_targets("zeppelin-embed", "graph-cypher")
if observed_core != core_graph_tests:
    raise SystemExit(
        "core graph test targets drifted: "
        f"missing={sorted(core_graph_tests - observed_core)!r} "
        f"unexpected={sorted(observed_core - core_graph_tests)!r}"
    )

core_targets = {
    target["name"]: target
    for target in packages["zeppelin-embed"]["targets"]
}
if core_targets["graph_node_blocks"].get("required-features"):
    raise SystemExit("legacy ANN graph_node_blocks became graph-feature-only")

ffi_graph_tests = {
    "ffi_graph_contract",
    "ffi_graph_header",
    "ffi_graph_layout",
}
observed_ffi = selected_targets("zeppelin-embed-ffi", "graph-cypher")
if observed_ffi != ffi_graph_tests:
    raise SystemExit(
        "FFI graph test targets drifted: "
        f"expected={sorted(ffi_graph_tests)!r} observed={sorted(observed_ffi)!r}"
    )

workspace_targets = {
    target["name"]: target
    for target in packages["zeppelin-embed-workspace-tests"]["targets"]
}
for target_name in ["graph_fixture", "graph-fixture"]:
    required = workspace_targets[target_name].get("required-features") or []
    if required != ["graph-cypher"]:
        raise SystemExit(
            f"workspace target {target_name} has required features {required!r}"
        )

features = packages["zeppelin-embed-ffi"]["features"]
if sorted(features["graph-cypher"]) != [
    "dep:zeppelin-embed-cypher",
    "zeppelin-embed/graph-cypher",
]:
    raise SystemExit(f"FFI graph feature drifted: {features['"'"'graph-cypher'"'"']!r}")
if features["graph-result-test-support"] != ["graph-cypher"]:
    raise SystemExit(
        "FFI graph result hook no longer stays separate from ordinary graph selection"
    )

compiler = packages["zeppelin-embed-cypher"]
core_dependency = next(
    dependency
    for dependency in compiler["dependencies"]
    if dependency["name"] == "zeppelin-embed"
)
if core_dependency["features"] != ["graph-cypher"]:
    raise SystemExit(
        f"compiler does not select core graph feature: {core_dependency['"'"'features'"'"']!r}"
    )

print(f"core graph-required test targets: {len(observed_core)}")
print(f"FFI graph-required test targets: {len(observed_ffi)}")
'

cargo metadata \
    --manifest-path "$repo_root/fuzz/Cargo.toml" \
    --format-version 1 \
    --no-deps \
    | python3 -c '
import json
import sys

package = json.load(sys.stdin)["packages"][0]
expected = {
    "cypher_parser",
    "graph_catalog",
    "native_graph_adjacency",
    "native_graph_artifact",
    "native_graph_records",
    "native_graph_wal",
}
observed = {
    target["name"]
    for target in package["targets"]
    if (target.get("required-features") or []) == ["graph-cypher"]
}
if observed != expected:
    raise SystemExit(
        "fuzz graph targets drifted: "
        f"expected={sorted(expected)!r} observed={sorted(observed)!r}"
    )
print(f"fuzz graph-required targets: {len(observed)}")
'

cargo check \
    --manifest-path "$repo_root/crates/zeppelin-embed/Cargo.toml" \
    --target x86_64-apple-darwin \
    --target-dir "$target_root/x86_64-legacy"

x86_graph_log="$temporary/x86-graph.log"
if cargo check \
    --manifest-path "$repo_root/crates/zeppelin-embed/Cargo.toml" \
    --features graph-cypher \
    --target x86_64-apple-darwin \
    --target-dir "$target_root/x86_64-graph" \
    >"$x86_graph_log" 2>&1; then
    echo "x86_64 graph feature unexpectedly compiled" >&2
    exit 1
fi
if ! grep -q 'graph-cypher requires macOS arm64' "$x86_graph_log"; then
    cat "$x86_graph_log" >&2
    echo "x86_64 graph failure was not the intentional unsupported-target diagnostic" >&2
    exit 1
fi

echo "graph feature boundary: PASS"
