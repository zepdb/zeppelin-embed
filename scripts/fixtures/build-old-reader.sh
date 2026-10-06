#!/usr/bin/env bash
set -euo pipefail
repo=$(git rev-parse --show-toplevel)
scratch=$(mktemp -d "${TMPDIR:-/tmp}/ze-old-reader.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
git clone --quiet --shared --no-checkout "$repo" "$scratch/repo"
git -C "$scratch/repo" worktree add --quiet --detach "$scratch/release" v0.6.0
mkdir -p "$scratch/release/crates/zeppelin-embed-ffi/examples/common"
cp "$repo/scripts/fixtures/old-reader.rs" "$scratch/release/crates/zeppelin-embed-ffi/examples/old_reader.rs"
cp "$repo/scripts/fixtures/common.rs" "$scratch/release/crates/zeppelin-embed-ffi/examples/common/mod.rs"
CARGO_BUILD_JOBS=3 CARGO_TARGET_DIR="$scratch/target" cargo build --offline --locked --manifest-path "$scratch/release/Cargo.toml" -p zeppelin-embed-ffi --example old_reader >&2
cp "$scratch/target/debug/examples/old_reader" "$1"
git -C "$scratch/release" rev-parse HEAD >&2
