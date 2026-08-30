#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$PROJECT_ROOT"

for source in tests/adversarial/*.rs tests/adversarial_tests.rs; do
    if tr -d '[:space:]' < "$source" | grep -E 'clean_control_passed(:|=)(true|!false)'; then
        echo "family clean controls must be measured, not literals: $source" >&2
        exit 1
    fi
done

if grep -n 'clean_control_passed' \
    tests/adversarial/{tiering_maintenance,fts,vamana_graph,lifecycle_accounting,diagnostics_health,ffi_bindings}.rs; then
    echo "family clean controls must come from run_with_clean_control" >&2
    exit 1
fi

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
cargo clippy -p zeppelin-embed --all-targets --features allocation-audit -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo test --workspace
cargo test -p zeppelin-embed --features allocation-audit \
    lifecycle::stats::tests::nothing_allocates_outside_accounting \
    -- --exact --test-threads=1
# The query-path allocation gate (FTS optimization plan P0.4). Deterministic
# allocator call counts, so zero flake budget; single-threaded for the same
# reason the accounting audit above is.
cargo test -p zeppelin-embed --features allocation-audit --lib \
    fts::alloc_gate -- --test-threads=1
cargo test -p zeppelin-embed-workspace-tests \
    --test scaffold_gates deny_blacklist_rejects_banned_dep \
    -- --ignored --exact
if [[ "$(uname -s)" == "Darwin" ]]; then
    cargo run -p zeppelin-embed-bench --bin platform-truth -- --smoke
    # BL-023: all Task-03 hot kernels carry evidence-derived roofline floors,
    # and equal-dimension float/i8 ratios automate the physical-byte sanity check.
    cargo run --profile bench -p zeppelin-embed-bench --bin kernel-roofline-gate
    # BL-101/BL-103: the traversal-relevant DRAM pair stays visible, and every
    # result is a median across independent processes with its between-process spread.
    cargo build --profile bench -p zeppelin-embed-bench \
        --bin gather-kernel --bin graph-search --bin platform-truth --bin dram-process-audit
    "${CARGO_TARGET_DIR:-$PROJECT_ROOT/target}/release/dram-process-audit" --processes 5
fi
cargo run -p zeppelin-embed-bench --bin wal-throughput -- --smoke
"$SCRIPT_DIR/coverage.sh"
cargo deny check
"$SCRIPT_DIR/size-budget.sh"
