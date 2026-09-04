#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
"$repo_root/scripts/adversarial.sh" smoke "$@"
cargo test --manifest-path "$repo_root/Cargo.toml" -p zeppelin-embed-text \
  --test adversarial -- --nocapture
