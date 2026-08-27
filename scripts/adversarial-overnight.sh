#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ -z "${ZE_ADV_ARTIFACTS:-}" ]]; then
  campaign_stamp="$(date -u +%Y%m%dT%H%M%SZ)"
  export ZE_ADV_ARTIFACTS="target/adversarial/campaign-$campaign_stamp"
fi
exec "$repo_root/scripts/adversarial.sh" campaign \
  --campaign overall --qualification release "$@"
