#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

: "${ZE_ADV_ARTIFACTS:=target/adversarial}"
export ZE_ADV_ARTIFACTS

cargo test -p zeppelin-embed-workspace-tests \
  --test adversarial_tests smoke -- --exact --nocapture
