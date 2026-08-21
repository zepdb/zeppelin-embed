#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$PROJECT_ROOT"

if grep -R -n -E 'sync_(all|data)' crates/zeppelin-embed/src; then
    echo "forbidden direct file synchronization under crates/zeppelin-embed/src" >&2
    exit 1
fi

if grep -R -n -E 'sleep|Duration|Instant|timeout' crates/zeppelin-embed/src/wal \
    | grep -v 'ZE_AMENDMENT_N_LIVENESS_WATCHDOG'; then
    echo "forbidden timer-derived WAL control; amendment N watchdogs require ZE_AMENDMENT_N_LIVENESS_WATCHDOG" >&2
    exit 1
fi

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo test --workspace
cargo test -p zeppelin-embed-workspace-tests \
    --test scaffold_gates deny_blacklist_rejects_banned_dep \
    -- --ignored --exact
if [[ "$(uname -s)" == "Darwin" ]]; then
    cargo run -p zeppelin-embed-bench --bin platform-truth -- --smoke
fi
"$SCRIPT_DIR/coverage.sh"
cargo deny check
"$SCRIPT_DIR/size-budget.sh"
