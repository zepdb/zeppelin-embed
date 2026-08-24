#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
    echo "error: cargo-llvm-cov is required; install it with 'cargo install cargo-llvm-cov'" >&2
    exit 2
fi

cd "$PROJECT_ROOT"

# Line coverage is the contract. LLVM's function count includes closures,
# generic instantiations, and duplicate test/library symbols, so it is not a
# source-function coverage percentage.
ZE_COVERAGE_SMALL_FIXTURE=1 cargo llvm-cov \
    --workspace \
    --fail-under-lines 90 \
    --ignore-filename-regex '(^|/)(registry/|crates/zeppelin-embed-bench|fuzz/|target/)' \
    "$@"

ZE_COVERAGE_SMALL_FIXTURE=1 cargo llvm-cov \
    -p zeppelin-embed-bench \
    --lib \
    --test frontier \
    --fail-under-lines 90 \
    --ignore-filename-regex '(^|/)(registry/|crates/zeppelin-embed/|crates/zeppelin-embed-bench/src/(bin|platform|recall)/|fuzz/|target/)' \
    "$@"
