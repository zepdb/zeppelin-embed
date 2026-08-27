#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
artifacts_root="${ZE_ADV_MACOS_ARTIFACTS:-$repo_root/target/adversarial-macos}"
overall_artifacts="$artifacts_root/overall-layer"
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
  echo "ADV_MACOS_START campaign=overall episodes=1000 layer=overall"
  "$repo_root/scripts/adversarial.sh" campaign \
    --campaign overall \
    --qualification exploratory \
    --min-seconds 0 \
    --min-episodes 1000 \
    --retain-successful 256 \
    --artifacts "$overall_artifacts" &
  overall_pid=$!

  for campaign in "${campaigns[@]}"; do
    echo "ADV_MACOS_START campaign=$campaign episodes=1000"
    if ! "$repo_root/scripts/adversarial.sh" campaign \
      --campaign "$campaign" \
      --qualification exploratory \
      --min-seconds 0 \
      --min-episodes 1000 \
      --retain-successful 256 \
      --artifacts "$artifacts_root/$campaign"; then
      run_failed=1
    fi
  done
  if ! wait "$overall_pid"; then
    run_failed=1
  fi
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
total_feature_episodes = 0

def fnv1a64(data):
    value = 0xCBF29CE484222325
    for byte in data:
        value ^= byte
        value = (value * 0x00000100000001B3) & 0xFFFFFFFFFFFFFFFF
    value ^= 0xFF
    value = (value * 0x00000100000001B3) & 0xFFFFFFFFFFFFFFFF
    return f"fnv1a64:{value:016x}"

def nonempty_lines(data):
    return [line for line in data.splitlines() if line]

def verify_zero_and_coverage(summary, label):
    for field in (
        "failed_episodes",
        "violations",
        "execution_errors",
        "panics",
        "unfired_scheduled_faults",
    ):
        if summary.get(field) != 0:
            errors.append(f"{label}: {field}={summary.get(field)!r}")
    for field in (
        "missing_coverage",
        "missing_invariants",
        "missing_feature_faults",
        "missing_operations",
        "missing_profiles",
        "missing_generic_faults",
    ):
        if summary.get(field) != []:
            errors.append(f"{label}: {field} is not empty")
    for field in ("languages", "backends"):
        if summary.get(field, {}).get("missing") != []:
            errors.append(f"{label}: {field}.missing is not empty")

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
    if episodes != 1000:
        errors.append(f"{campaign}: expected exactly 1000 episodes, got {episodes!r}")
    if isinstance(episodes, int):
        total_feature_episodes += episodes
    verify_zero_and_coverage(summary, campaign)
    if summary.get("qualification_passed") is not True:
        errors.append(f"{campaign}: qualification did not pass")
    if summary.get("host", {}).get("os") != "macos":
        errors.append(f"{campaign}: evidence was not produced on macOS")
    attestation = summary.get("attestation", {})
    if attestation.get("oracle_contract_version") != 1:
        errors.append(f"{campaign}: oracle contract attestation is missing")
    if not attestation.get("valid"):
        errors.append(f"{campaign}: independent-oracle attestation is invalid")
    comparisons = attestation.get("comparison_counts", {})
    for invariant in summary.get("required_invariants", []):
        if comparisons.get(invariant) != 1000:
            errors.append(
                f"{campaign}: {invariant} comparison count={comparisons.get(invariant)!r}"
            )
    selected = attestation.get("selected_feature_fault_events")
    if attestation.get("same_seed_clean_controls") != selected:
        errors.append(f"{campaign}: same-seed control count does not match selected faults")
    if attestation.get("integrated_feature_fault_receipts") != selected:
        errors.append(f"{campaign}: receipt count does not match selected faults")
    merged = attestation.get("merged_evidence", {})
    if merged.get("episodes") != 1000 or merged.get("complete") is not True:
        errors.append(f"{campaign}: merged evidence does not cover 1000 episodes")
    index_path = root / campaign / "merged-index.jsonl"
    try:
        index_rows = nonempty_lines(index_path.read_bytes())
        if len(index_rows) != 1000:
            errors.append(f"{campaign}: merged index has {len(index_rows)} rows")
    except Exception as error:
        errors.append(f"{campaign}: cannot read merged index: {error}")
    for stream in ("oracle", "controls", "receipts", "mutations"):
        stream_path = root / campaign / f"merged-{stream}.jsonl"
        try:
            data = stream_path.read_bytes()
        except Exception as error:
            errors.append(f"{campaign}: cannot read merged {stream}: {error}")
            continue
        claimed = merged.get("streams", {}).get(stream, {})
        actual_rows = len(nonempty_lines(data))
        if claimed.get("records") != actual_rows:
            errors.append(f"{campaign}: merged {stream} record count mismatch")
        if claimed.get("bytes") != len(data):
            errors.append(f"{campaign}: merged {stream} byte count mismatch")
        if claimed.get("digest") != fnv1a64(data):
            errors.append(f"{campaign}: merged {stream} digest mismatch")

if len(summaries) != 11:
    errors.append(f"expected 11 summaries, found {len(summaries)}")
if total_feature_episodes != 11000:
    errors.append(f"expected exactly 11000 feature episodes, got {total_feature_episodes}")

overall_path = root / "overall-layer" / "campaign-summary.json"
overall_summary = None
overall_episodes = 0
try:
    overall_summary = json.loads(overall_path.read_text(encoding="utf-8"))
except Exception as error:
    errors.append(f"overall: cannot read {overall_path}: {error}")
if overall_summary is not None:
    if overall_summary.get("version") != 3:
        errors.append("overall: summary version is not 3")
    if overall_summary.get("campaign") != "overall":
        errors.append("overall: campaign identity drifted")
    if overall_summary.get("complete") is not True:
        errors.append("overall: campaign is incomplete")
    overall_episodes = overall_summary.get("episodes", 0)
    if overall_episodes != 1000:
        errors.append(f"overall: expected exactly 1000 episodes, got {overall_episodes!r}")
    verify_zero_and_coverage(overall_summary, "overall")
    if overall_summary.get("qualification_passed") is not True:
        errors.append("overall: qualification did not pass")
    if overall_summary.get("host", {}).get("os") != "macos":
        errors.append("overall: evidence was not produced on macOS")

aggregate = {
    "schema": "zeppelin-embed-adversarial-macos-aggregate",
    "version": 1,
    "campaigns": campaigns,
    "campaign_count": len(summaries),
    "feature_episodes": total_feature_episodes,
    "overall_episodes": overall_episodes,
    "passed": not errors,
    "errors": errors,
    "summaries": summaries,
    "overall_summary": overall_summary,
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
print("ADV_MACOS_COMPLETE campaigns=11 feature_episodes=11000 overall_episodes=1000 failures=0 panics=0")
PY
then
  run_failed=1
fi

(( run_failed == 0 )) || exit 1
