#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
    echo "error: cargo-llvm-cov is required; install it with 'cargo install cargo-llvm-cov'" >&2
    exit 2
fi

cd "$PROJECT_ROOT"
exec cargo llvm-cov \
    --workspace \
    --fail-under-lines 90 \
    --ignore-filename-regex '(^|/)(crates/zeppelin-embed-bench|fuzz|target)/' \
    "$@"

