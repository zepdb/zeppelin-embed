#!/bin/sh
# Developer bounded parity only. No release publication or package acceptance.
set -eu
export CARGO_BUILD_JOBS=3 ZE_TEST_SEED=72
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
if [ "${1:-}" = --final ]; then
  echo 'ZE-72 final qualification unavailable: missing ZE-76 counter handoff, ZE-278 Swift structured wrappers, ZE-71 executed installed artifacts, ZE-290 write-limit recovery, ZE-287 legacy Intel size decision, ZE-108 runtime/support brief and recorded ZE-29 revisions. See tasks/evidence/ze-72-bindings.md.' >&2
  exit 1
fi
output=$(mktemp -d "${TMPDIR:-/tmp}/ze72-parity.XXXXXX")
export ZE72_CORPUS_OUTPUT="$output/applications.tsv"
cleanup() {
  if [ -f "$ZE72_CORPUS_OUTPUT.path" ]; then
    path=$(cat "$ZE72_CORPUS_OUTPUT.path")
    rm -rf -- "$(dirname -- "$path")"
  fi
  if [ -f "$ZE72_CORPUS_OUTPUT.twins.path" ]; then
    path=$(cat "$ZE72_CORPUS_OUTPUT.twins.path")
    rm -rf -- "$(dirname -- "$path")"
  fi
  rm -rf -- "$output"
}
trap cleanup EXIT HUP INT TERM
cargo test -p zeppelin-embed-workspace-tests --features graph-result-test-support --test graph_bindings ze72_ -- --test-threads=1
cargo test -p zeppelin-embed-ffi --features graph-bindings-test-support --test ffi_graph_full ze72_ -- --test-threads=1
cargo build -p zeppelin-embed-ffi --release --features graph-bindings-test-support
ZE_USE_LOCAL_FFI=1 CLANG_MODULE_CACHE_PATH="$output/clang-cache" SWIFTPM_MODULECACHE_OVERRIDE="$output/swift-cache" swift test --disable-sandbox --jobs 3 --scratch-path "$output/swift-build" --package-path bindings/swift/graph --filter GraphBindingsParityTests -Xswiftc -DZE72_TEST_BRIDGE
