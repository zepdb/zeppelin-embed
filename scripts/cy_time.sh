#!/bin/bash
# Usage: scripts/cy_time.sh <worktree> <read-only stores directory>
set -euo pipefail
W=$(cd "${1:?worktree required}" && pwd)
STORES=${2:?read-only stores directory required}
cd "$W/bindings/node"
if [ ! -d node_modules ]; then
  npm ci --prefer-offline --no-audit --no-fund
fi
npm run build:native
case "$(uname -s)/$(uname -m)" in
  Darwin/arm64) P=darwin-arm64 ;;
  Darwin/x86_64) P=darwin-x64 ;;
  *) echo 'Unsupported timing host' >&2; exit 1 ;;
esac
printf 'HEAD_SHA=%s\n' "$(git -C "$W" rev-parse HEAD)"
printf 'ADDON_SHA256=%s\n' "$(shasum -a 256 "prebuilds/$P/zeppelin_embed.node" | cut -d ' ' -f 1)"
uptime
node bench/relationship-timing.mjs "$STORES"
