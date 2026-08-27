#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
artifacts_root="${ZE_ADV_MACOS_ARTIFACTS:-$repo_root/target/adversarial-macos}"
verify_only=0

if (( $# > 1 )); then
  echo "usage: scripts/adversarial-macos.sh [--verify-only]" >&2
  exit 2
fi
if (( $# == 1 )); then
  [[ "$1" == "--verify-only" ]] || {
    echo "usage: scripts/adversarial-macos.sh [--verify-only]" >&2
    exit 2
  }
  verify_only=1
fi

[[ "$(uname -s)" == "Darwin" ]] || {
  echo "adversarial-macos: local macOS execution is required" >&2
  exit 2
}

campaigns=(
  storage-durability
  ingest-retention
  vector-execution
  vamana-graph
  metadata-filter-planner
  fts
  hybrid-fusion
  tiering-maintenance
  lifecycle-accounting
  diagnostics-health
  ffi-bindings
)

mkdir -p "$artifacts_root"
run_failed=0
if (( verify_only == 0 )); then
  for campaign in "${campaigns[@]}"; do
    echo "ADV_MACOS_START campaign=$campaign episodes=500"
    if ! "$repo_root/scripts/adversarial.sh" campaign \
      --campaign "$campaign" \
      --qualification exploratory \
      --min-seconds 0 \
      --min-episodes 500 \
      --retain-successful 256 \
      --artifacts "$artifacts_root/$campaign"; then
      run_failed=1
    fi
  done
fi

if ! python3 - "$artifacts_root" "${campaigns[@]}" <<'PY'
import json
import os
import pathlib
import sys
import tempfile

root = pathlib.Path(sys.argv[1])
campaigns = sys.argv[2:]
errors = []
summaries = []
total_episodes = 0

for campaign in campaigns:
    path = root / campaign / "campaign-summary.json"
    try:
        summary = json.loads(path.read_text(encoding="utf-8"))
    except Exception as error:
        errors.append(f"{campaign}: cannot read {path}: {error}")
        continue
    summaries.append(summary)
    if summary.get("version") != 3:
        errors.append(f"{campaign}: summary version is not 3")
    if summary.get("campaign") != campaign:
        errors.append(f"{campaign}: summary campaign identity drifted")
    if summary.get("complete") is not True:
        errors.append(f"{campaign}: campaign is incomplete")
    episodes = summary.get("episodes")
    if episodes != 500:
        errors.append(f"{campaign}: expected exactly 500 episodes, got {episodes!r}")
    if isinstance(episodes, int):
        total_episodes += episodes
    for field in (
        "failed_episodes",
        "violations",
        "execution_errors",
        "panics",
        "unfired_scheduled_faults",
    ):
        if summary.get(field) != 0:
            errors.append(f"{campaign}: {field}={summary.get(field)!r}")
    for field in (
        "missing_coverage",
        "missing_invariants",
        "missing_feature_faults",
        "missing_operations",
        "missing_profiles",
        "missing_generic_faults",
    ):
        if summary.get(field) != []:
            errors.append(f"{campaign}: {field} is not empty")
    for field in ("languages", "backends"):
        if summary.get(field, {}).get("missing") != []:
            errors.append(f"{campaign}: {field}.missing is not empty")
    if summary.get("qualification_passed") is not True:
        errors.append(f"{campaign}: qualification did not pass")
    if summary.get("host", {}).get("os") != "macos":
        errors.append(f"{campaign}: evidence was not produced on macOS")

if len(summaries) != 11:
    errors.append(f"expected 11 summaries, found {len(summaries)}")
if total_episodes != 5500:
    errors.append(f"expected exactly 5500 aggregate episodes, got {total_episodes}")

aggregate = {
    "schema": "zeppelin-embed-adversarial-macos-aggregate",
    "version": 1,
    "campaigns": campaigns,
    "campaign_count": len(summaries),
    "episodes": total_episodes,
    "passed": not errors,
    "errors": errors,
    "summaries": summaries,
}
root.mkdir(parents=True, exist_ok=True)
descriptor, temporary_name = tempfile.mkstemp(
    dir=root, prefix=".adversarial-macos-aggregate.", suffix=".tmp"
)
try:
    with os.fdopen(descriptor, "w", encoding="utf-8") as output:
        json.dump(aggregate, output, sort_keys=True, separators=(",", ":"))
        output.write("\n")
        output.flush()
        os.fsync(output.fileno())
    os.replace(temporary_name, root / "aggregate.json")
    directory = os.open(root, os.O_RDONLY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)
finally:
    if os.path.exists(temporary_name):
        os.unlink(temporary_name)

if errors:
    for error in errors:
        print(f"adversarial-macos: {error}", file=sys.stderr)
    raise SystemExit(1)
print("ADV_MACOS_COMPLETE campaigns=11 episodes=5500 failures=0 panics=0")
PY
then
  run_failed=1
fi

(( run_failed == 0 )) || exit 1
