#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
source "$SCRIPT_DIR/graph-feature-target.sh"

if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
    echo "error: cargo-llvm-cov is required; install it with 'cargo install cargo-llvm-cov'" >&2
    exit 2
fi

cd "$PROJECT_ROOT"

REPOSITORY_MANIFEST=true
ARGS=("$@")
for ((i = 0; i < ${#ARGS[@]}; i++)); do
    manifest=""
    case "${ARGS[i]}" in
        --manifest-path) manifest="${ARGS[i+1]:?missing manifest path}" ;;
        --manifest-path=*) manifest="${ARGS[i]#--manifest-path=}" ;;
    esac
    if [[ -n "$manifest" ]]; then
        manifest="$(cd "$(dirname "$manifest")" && pwd)/$(basename "$manifest")"
        [[ "$manifest" == "$PROJECT_ROOT/Cargo.toml" ]] || REPOSITORY_MANIFEST=false
    fi
done

WORKSPACE_ARGS=(--workspace)
if $REPOSITORY_MANIFEST; then
GRAPH_TARGET="$(graph_effective_target "$@")"
if graph_target_supports_native_graph "$GRAPH_TARGET"; then
    WORKSPACE_ARGS+=(--features zeppelin-embed-workspace-tests/graph-result-test-support)
    echo "native graph coverage: selected for $GRAPH_TARGET"
else
    if graph_host_is_darwin; then
        WORKSPACE_ARGS+=(--exclude zeppelin-embed-cypher)
    else
        WORKSPACE_ARGS+=(
            --exclude zeppelin-embed-bench
            --exclude zeppelin-embed-cypher
            --exclude zeppelin-embed-text
        )
    fi
    echo "native graph coverage: not selected for $GRAPH_TARGET; running legacy coverage"
fi

fi

# Line coverage is the contract. LLVM's function count includes closures,
# generic instantiations, and duplicate test/library symbols, so it is not a
# source-function coverage percentage.
#
# BL-049: do not read the FUNCTION column as a count of source functions.
# Measured on segment/reader.rs: 16 `fn` definitions in source, 79 "functions"
# in the llvm-cov report. The inflation is (a) closures counted as functions,
# (b) one record per generic instantiation, and (c) the same code emitted
# under two crate disambiguators -- the lib build and the test build -- so
# every symbol is double-counted. A low function-coverage percentage on a
# module with heavy generics or map_err/ok_or_else chains reads as alarming
# and may mean nothing; BL-047 was filed P1 on exactly that misreading.
# Use LINE coverage for the headline (which is what --fail-under-lines gates
# below, and that stays), and for "is this path tested" aggregate zero-hit
# records by SOURCE DEFINITION after demangling rather than trusting the
# function column.
cargo llvm-cov \
    "${WORKSPACE_ARGS[@]}" \
    --fail-under-lines 90 \
    --ignore-filename-regex '(^|/)(registry/|crates/zeppelin-embed-bench|fuzz/|target/)' \
    "$@"

# Reuse the just-produced profiles; this adds no test/coverage campaign.
# External manifests keep their original behavior. Routine platform exclusions
# remain explicit; the release gate separately requires all seven crates.
if $REPOSITORY_MANIFEST; then
    COVERAGE_DIR="${CARGO_TARGET_DIR:-$PROJECT_ROOT/target}/ze78-coverage"
    mkdir -p "$COVERAGE_DIR"
    COVERAGE_CRATES=(zeppelin-embed zeppelin-embed-ffi
        zeppelin-embed-adversarial-oracle zeppelin-embed-workspace-tests)
    if graph_target_supports_native_graph "$GRAPH_TARGET"; then
        COVERAGE_CRATES+=(zeppelin-embed-cypher zeppelin-embed-text)
    elif graph_host_is_darwin; then
        COVERAGE_CRATES+=(zeppelin-embed-text)
    fi
    REPORT_ARGS=(--manifest-path "$PROJECT_ROOT/Cargo.toml")
    for ((i = 0; i < ${#ARGS[@]}; i++)); do
        case "${ARGS[i]}" in
            --target|--profile) REPORT_ARGS+=("${ARGS[i]}" "${ARGS[i+1]}"); i=$((i+1)) ;;
            --target=*|--profile=*|--release|--no-default-features|--all-features|--remap-path-prefix)
                REPORT_ARGS+=("${ARGS[i]}") ;;
        esac
    done
    PACKAGE_ARGS=()
    for crate in "${COVERAGE_CRATES[@]}"; do PACKAGE_ARGS+=(-p "$crate"); done
    cargo llvm-cov report "${PACKAGE_ARGS[@]}" "${REPORT_ARGS[@]}" --json --summary-only \
        --ignore-filename-regex '(^|/)(registry/|crates/zeppelin-embed-bench|fuzz/|target/)' \
        --output-path "$COVERAGE_DIR/workspace.json"
    python3 scripts/release/qualify-graph.py --coverage-json "$COVERAGE_DIR/workspace.json" \
        --coverage-output "$COVERAGE_DIR/workspace-crates.json" --coverage-crates "${COVERAGE_CRATES[@]}"
fi

if $REPOSITORY_MANIFEST && [[ "$(uname -s)" == "Darwin" ]]; then
    cargo llvm-cov \
        -p zeppelin-embed-bench \
        --lib \
        --test frontier \
        --fail-under-lines 90 \
        --ignore-filename-regex '(^|/)(registry/|crates/zeppelin-embed/|crates/zeppelin-embed-bench/src/(bin|platform|recall)/|fuzz/|target/)' \
        "$@"
    cargo llvm-cov report -p zeppelin-embed-bench "${REPORT_ARGS[@]}" --json --summary-only \
        --ignore-filename-regex '(^|/)(registry/|crates/zeppelin-embed/|crates/zeppelin-embed-bench/src/(bin|platform|recall)/|fuzz/|target/)' \
        --output-path "$COVERAGE_DIR/benchmark.json"
    python3 scripts/release/qualify-graph.py --coverage-json "$COVERAGE_DIR/benchmark.json" \
        --coverage-output "$COVERAGE_DIR/benchmark-crates.json" --coverage-crates zeppelin-embed-bench
fi
