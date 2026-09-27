#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
temporary="$(mktemp -d)"
trap 'rm -rf "$temporary"' EXIT

stub_dir="$temporary/bin"
mkdir -p "$stub_dir"

cat > "$stub_dir/uname" <<'EOF'
#!/usr/bin/env bash
case "${1:-}" in
  -s) printf '%s\n' "$STUB_UNAME_S" ;;
  -m) printf '%s\n' "$STUB_UNAME_M" ;;
  *) printf '%s %s\n' "$STUB_UNAME_S" "$STUB_UNAME_M" ;;
esac
EOF

cat > "$stub_dir/rustc" <<'EOF'
#!/usr/bin/env bash
printf 'rustc 1.93.0\nhost: %s\n' "$STUB_RUST_HOST"
EOF

cat > "$stub_dir/cargo" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$STUB_CARGO_LOG"
if [[ "${1:-}" == "$STUB_STOP_COMMAND" ]]; then
  exit 97
fi
EOF

cat > "$stub_dir/cargo-llvm-cov" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
chmod +x \
  "$stub_dir/uname" \
  "$stub_dir/rustc" \
  "$stub_dir/cargo" \
  "$stub_dir/cargo-llvm-cov"

route_log=""
route_output=""

run_route() {
  local label="$1"
  local host_os="$2"
  local host_arch="$3"
  local rust_host="$4"
  local cargo_target="$5"
  local stop_command="$6"
  local script="$7"
  shift 7

  route_log="$temporary/$label.cargo"
  route_output="$temporary/$label.output"
  : > "$route_log"

  set +e
  if [[ "$cargo_target" == "<unset>" ]]; then
    (
      unset CARGO_BUILD_TARGET
      PATH="$stub_dir:$PATH" \
        STUB_UNAME_S="$host_os" \
        STUB_UNAME_M="$host_arch" \
        STUB_RUST_HOST="$rust_host" \
        STUB_CARGO_LOG="$route_log" \
        STUB_STOP_COMMAND="$stop_command" \
        /bin/bash "$script_dir/$script" "$@"
    ) > "$route_output" 2>&1
  else
    PATH="$stub_dir:$PATH" \
      CARGO_BUILD_TARGET="$cargo_target" \
      STUB_UNAME_S="$host_os" \
      STUB_UNAME_M="$host_arch" \
      STUB_RUST_HOST="$rust_host" \
      STUB_CARGO_LOG="$route_log" \
      STUB_STOP_COMMAND="$stop_command" \
      /bin/bash "$script_dir/$script" "$@" \
      > "$route_output" 2>&1
  fi
  local status=$?
  set -e

  if (( status != 97 )); then
    cat "$route_output" >&2
    echo "$label: expected stub stop status 97, observed $status" >&2
    exit 1
  fi
}

require_text() {
  local file="$1"
  local expected="$2"
  if ! grep -F -q -- "$expected" "$file"; then
    cat "$file" >&2
    echo "missing route text: $expected" >&2
    exit 1
  fi
}

reject_text() {
  local file="$1"
  local rejected="$2"
  if grep -F -q -- "$rejected" "$file"; then
    cat "$file" >&2
    echo "unexpected route text: $rejected" >&2
    exit 1
  fi
}

run_route ci-arm Darwin arm64 aarch64-apple-darwin '<unset>' clippy ci-gates.sh
require_text "$route_output" 'native graph qualification: selected for aarch64-apple-darwin'
require_text "$route_log" '--features zeppelin-embed-workspace-tests/graph-result-test-support'

run_route coverage-arm Darwin arm64 aarch64-apple-darwin '<unset>' llvm-cov coverage.sh
require_text "$route_output" 'native graph coverage: selected for aarch64-apple-darwin'
require_text "$route_log" '--features zeppelin-embed-workspace-tests/graph-result-test-support'

run_route adversarial-arm Darwin arm64 aarch64-apple-darwin '<unset>' test adversarial.sh smoke
require_text "$route_output" 'native graph campaigns: selected for aarch64-apple-darwin'
require_text "$route_log" '--features graph-result-test-support'

run_route ci-intel Darwin x86_64 x86_64-apple-darwin '<unset>' clippy ci-gates.sh
require_text "$route_output" 'native graph qualification: selected for x86_64-apple-darwin'
require_text "$route_log" 'graph-result-test-support'

run_route coverage-intel Darwin x86_64 x86_64-apple-darwin '<unset>' llvm-cov coverage.sh
require_text "$route_output" 'native graph coverage: selected for x86_64-apple-darwin'
require_text "$route_log" 'graph-result-test-support'

run_route adversarial-intel Darwin x86_64 x86_64-apple-darwin '<unset>' test adversarial.sh smoke
require_text "$route_output" 'native graph campaigns: selected for x86_64-apple-darwin'
require_text "$route_log" 'graph-result-test-support'

run_route ci-linux Linux x86_64 x86_64-unknown-linux-gnu '<unset>' clippy ci-gates.sh
require_text "$route_log" '--exclude zeppelin-embed-cypher'
reject_text "$route_log" 'graph-result-test-support'

run_route coverage-linux Linux x86_64 x86_64-unknown-linux-gnu '<unset>' llvm-cov coverage.sh
require_text "$route_log" '--exclude zeppelin-embed-bench'
require_text "$route_log" '--exclude zeppelin-embed-cypher'
require_text "$route_log" '--exclude zeppelin-embed-text'
reject_text "$route_log" 'graph-result-test-support'

run_route adversarial-linux Linux x86_64 x86_64-unknown-linux-gnu '<unset>' test adversarial.sh smoke
reject_text "$route_log" 'graph-result-test-support'

run_route adversarial-windows MINGW64_NT x86_64 x86_64-pc-windows-msvc '<unset>' test adversarial.sh smoke
require_text "$route_log" '--features graph-result-test-support'

run_route ci-arm-x86-target Darwin arm64 aarch64-apple-darwin x86_64-apple-darwin clippy ci-gates.sh
require_text "$route_output" 'native graph qualification: selected for x86_64-apple-darwin'
require_text "$route_log" 'graph-result-test-support'

run_route adversarial-arm-x86-target Darwin arm64 aarch64-apple-darwin x86_64-apple-darwin test adversarial.sh smoke
require_text "$route_output" 'native graph campaigns: selected for x86_64-apple-darwin'
require_text "$route_log" 'graph-result-test-support'

run_route coverage-cli-target Darwin arm64 aarch64-apple-darwin aarch64-apple-darwin llvm-cov \
  coverage.sh --target x86_64-apple-darwin
require_text "$route_output" 'native graph coverage: selected for x86_64-apple-darwin'
require_text "$route_log" 'graph-result-test-support'

run_route ci-linux-arm-target Linux x86_64 x86_64-unknown-linux-gnu aarch64-apple-darwin clippy ci-gates.sh
require_text "$route_output" 'native graph qualification: not selected for aarch64-apple-darwin'
require_text "$route_log" '--exclude zeppelin-embed-cypher'
reject_text "$route_log" 'graph-result-test-support'

run_route ci-arm-x86-rustc Darwin arm64 x86_64-apple-darwin '<unset>' clippy ci-gates.sh
require_text "$route_output" 'native graph qualification: selected for x86_64-apple-darwin'
require_text "$route_log" 'graph-result-test-support'

echo 'graph feature command routing: PASS'
