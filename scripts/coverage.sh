#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
    echo "error: cargo-llvm-cov is required; install it with 'cargo install cargo-llvm-cov'" >&2
    exit 2
fi

cd "$PROJECT_ROOT"

WORKSPACE_ARGS=(--workspace)
if [[ "$(uname -s)" != "Darwin" ]]; then
    WORKSPACE_ARGS+=(--exclude zeppelin-embed-text --exclude zeppelin-embed-bench)
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

if [[ "$(uname -s)" == "Darwin" ]]; then
    cargo llvm-cov \
        -p zeppelin-embed-bench \
        --lib \
        --test frontier \
        --fail-under-lines 90 \
        --ignore-filename-regex '(^|/)(registry/|crates/zeppelin-embed/|crates/zeppelin-embed-bench/src/(bin|platform|recall)/|fuzz/|target/)' \
        "$@"
fi
