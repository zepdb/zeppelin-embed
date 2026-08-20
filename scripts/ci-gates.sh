#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$PROJECT_ROOT"

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo test --workspace
cargo test -p zeppelin-embed-workspace-tests \
    --test scaffold_gates deny_blacklist_rejects_banned_dep \
    -- --ignored --exact
"$SCRIPT_DIR/coverage.sh"
cargo deny check
"$SCRIPT_DIR/size-budget.sh"

