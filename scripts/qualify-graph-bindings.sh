#!/bin/sh
# Developer bounded parity only. No release publication or package acceptance.
set -eu
export CARGO_BUILD_JOBS=3 ZE_TEST_SEED=72
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
persistent=0
final=0
output=
manifest=
decisions=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --final) final=1; shift ;;
    --output-dir|--release-manifest|--decisions)
      option=$1
      [ "$#" -ge 2 ] || { echo "missing value for $option" >&2; exit 2; }
      case "$option" in
        --output-dir) output=$2; persistent=1 ;;
        --release-manifest) manifest=$2 ;;
        --decisions) decisions=$2 ;;
      esac
      shift 2 ;;
    *) echo "usage: $0 [--output-dir DIR] [--final --release-manifest FILE [--decisions FILE]]" >&2; exit 2 ;;
  esac
done
if [ "$persistent" -eq 1 ]; then
  mkdir -p "$output"
  output=$(CDPATH= cd -- "$output" && pwd)
  [ ! -e "$output/applications.tsv" ] || { echo "refusing to overwrite retained corpus: $output" >&2; exit 1; }
  mkdir -p "$output/fixtures"
  export TMPDIR="$output/fixtures"
else
  output=$(mktemp -d "${TMPDIR:-/tmp}/ze72-parity.XXXXXX")
fi
export ZE72_CORPUS_OUTPUT="$output/applications.tsv"
cleanup() {
  [ "$persistent" -eq 0 ] || return 0
  for suffix in .path .twins.path; do
    if [ -f "$ZE72_CORPUS_OUTPUT$suffix" ]; then
      path=$(cat "$ZE72_CORPUS_OUTPUT$suffix")
      rm -rf -- "$(dirname -- "$path")"
    fi
  done
  rm -rf -- "$output"
}
trap cleanup EXIT HUP INT TERM
run() {
  name=$1
  shift
  printf '%s\n' "$*" > "$output/$name.command"
  if "$@" > "$output/$name.log" 2>&1; then
    echo 0 > "$output/$name.status"
    cat "$output/$name.log"
  else
    status=$?
    echo "$status" > "$output/$name.status"
    cat "$output/$name.log" >&2
    exit "$status"
  fi
}
{
  echo 'Scope: focused source parity; not installed/native-platform acceptance'
  echo "CARGO_BUILD_JOBS=$CARGO_BUILD_JOBS ZE_TEST_SEED=$ZE_TEST_SEED"
  git rev-parse HEAD
  uname -a
  rustc --version
  swift --version
} > "$output/identity.log"
run rust cargo test -p zeppelin-embed-workspace-tests --features graph-result-test-support --test graph_bindings ze72_ -- --test-threads=1
run c cargo test -p zeppelin-embed-ffi --features graph-bindings-test-support --test ffi_graph_full ze72_ -- --test-threads=1
run archive cargo build -p zeppelin-embed-ffi --release --features graph-bindings-test-support
run swift env ZE_USE_LOCAL_FFI=1 ZE_ENABLE_GRAPH=1 CLANG_MODULE_CACHE_PATH="$output/clang-cache" SWIFTPM_MODULECACHE_OVERRIDE="$output/swift-cache" swift test --disable-sandbox --jobs 3 --scratch-path "$output/swift-build" --package-path . --filter GraphBindingsParityTests -Xswiftc -DZE72_TEST_BRIDGE
run swift-summary python3 scripts/check_swift_qualification.py "$output/swift.log" GraphBindingsParityTests
if [ "$final" -eq 1 ]; then
  # Execute the landed ZE-76 resources and ZE-278 structured public surfaces.
  run surfaces env ZE_USE_LOCAL_FFI=1 ZE_ENABLE_GRAPH=1 CLANG_MODULE_CACHE_PATH="$output/clang-cache" SWIFTPM_MODULECACHE_OVERRIDE="$output/swift-cache" swift test --disable-sandbox --jobs 3 --scratch-path "$output/swift-build" --package-path . --filter 'GraphStoreTests.testZE76ResourceParity|GraphStructuredQueryTests' -Xswiftc -DZE72_TEST_BRIDGE
  run resources-summary python3 scripts/check_swift_qualification.py "$output/surfaces.log" GraphStoreTests
  run structured-summary python3 scripts/check_swift_qualification.py "$output/surfaces.log" GraphStructuredQueryTests
  if [ -z "$manifest" ]; then
    echo 'BLOCKED ZE-108 native-platform evidence: no retained release manifest supplied. Installed consumers (ZE-71), footprint and acceptance records must be verified by scripts/release/qualify-graph.py; focused source parity cannot prove them.' > "$output/final-blockers.log"
    cat "$output/final-blockers.log" >&2
    exit 1
  fi
  # The existing evidence gate checks actual receipts, hashes and platform cells.
  if [ -n "$decisions" ]; then
    run final-evidence python3 scripts/release/qualify-graph.py --manifest "$manifest" --decisions "$decisions" --results "$output/final-results.md"
  else
    run final-evidence python3 scripts/release/qualify-graph.py --manifest "$manifest" --results "$output/final-results.md"
  fi
fi
echo 'PASS focused Rust/C/Swift parity; no qualification skips' > "$output/outcomes.txt"
