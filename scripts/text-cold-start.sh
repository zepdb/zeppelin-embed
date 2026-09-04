#!/bin/sh

set -u

if [ "${1:-}" != "--runs" ] || [ -z "${2:-}" ] || [ "${3:-}" != "--" ]; then
  echo "usage: $0 --runs N -- COMMAND [ARG ...]" >&2
  exit 2
fi

runs=$2
shift 3
if [ "$#" -eq 0 ]; then
  echo "cold command is required" >&2
  exit 2
fi
if ! printf '%s\n' "$runs" | grep -Eq '^[1-9][0-9]*$'; then
  echo "--runs must be a positive integer" >&2
  exit 2
fi

run_dir=$(mktemp -d /private/tmp/ze-cold-start.XXXXXX)
run=1
while [ "$run" -le "$runs" ]; do
  before=$(uptime)
  load=$(printf '%s\n' "$before" | sed -E 's/.*load averages?: ([0-9]+([.][0-9]+)?).*/\1/')
  if ! printf '%s\n' "$load" | grep -Eq '^[0-9]+([.][0-9]+)?$'; then
    echo "could not parse one-minute load from: $before" >&2
    exit 2
  fi
  if ! awk -v load="$load" 'BEGIN { exit !(load <= 3.0) }'; then
    echo "cold run $run refused: one-minute load $load exceeds 3.0" >&2
    exit 75
  fi
  printf '%s\n' "run $run uptime before: $before"
  printf '%s\n' "run $run pmset before:"
  pmset -g therm
  "$@" > "$run_dir/run-$run.json"
  status=$?
  cat "$run_dir/run-$run.json"
  after=$(uptime)
  printf '%s\n' "run $run uptime after: $after"
  printf '%s\n' "run $run pmset after:"
  pmset -g therm
  if [ "$status" -ne 0 ]; then
    exit "$status"
  fi
  run=$((run + 1))
done

python3 - "$run_dir" "$runs" <<'PY' | tee "$run_dir/summary.json"
import json
import statistics
import sys
from pathlib import Path

root = Path(sys.argv[1])
runs = int(sys.argv[2])
rows = [json.loads((root / f"run-{index}.json").read_text()) for index in range(1, runs + 1)]
if not rows or any(row.get("kind") != "cold" for row in rows):
    raise SystemExit("cold output is missing or malformed")
backend = rows[0]["backend"]
if any(row["backend"] != backend for row in rows):
    raise SystemExit("cold outputs mix backends")

def select(row):
    return {
        "open_ms": row["open_ms"],
        "first_query_ms": row["first_query_ms"],
        "total_ms": row["total_ms"],
    }

relaunches = rows[1:]
median = {
    key: statistics.median(row[key] for row in relaunches)
    for key in ("open_ms", "first_query_ms", "total_ms")
} if relaunches else select(rows[0])
summary = {
    "kind": "cold-summary",
    "backend": backend,
    "runs": runs,
    "first_ever": select(rows[0]),
    "relaunch_median": median,
    "raw_directory": str(root),
}
print(json.dumps(summary, sort_keys=True))
PY
