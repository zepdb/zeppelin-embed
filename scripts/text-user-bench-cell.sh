#!/bin/sh

set -u

if [ "${1:-}" != "--load-limit" ] || [ -z "${2:-}" ] || [ "${3:-}" != "--" ]; then
  echo "usage: $0 --load-limit LIMIT -- COMMAND [ARG ...]" >&2
  exit 2
fi

limit=$2
shift 3
if [ "$#" -eq 0 ]; then
  echo "measurement command is required" >&2
  exit 2
fi

before=$(uptime)
load=$(printf '%s\n' "$before" | sed -E 's/.*load averages?: ([0-9]+([.][0-9]+)?).*/\1/')
if ! printf '%s\n' "$load" | grep -Eq '^[0-9]+([.][0-9]+)?$'; then
  echo "could not parse one-minute load from: $before" >&2
  exit 2
fi
if ! awk -v load="$load" -v limit="$limit" 'BEGIN { exit !(load <= limit) }'; then
  echo "measurement refused: one-minute load $load exceeds $limit" >&2
  exit 75
fi

printf '%s\n' "uptime before: $before"
printf '%s\n' "pmset before:"
pmset -g therm

"$@"
status=$?

after=$(uptime)
printf '%s\n' "uptime after: $after"
printf '%s\n' "pmset after:"
pmset -g therm
exit "$status"
