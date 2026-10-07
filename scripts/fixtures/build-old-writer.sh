#!/usr/bin/env bash
set -euo pipefail
repo=$(git rev-parse --show-toplevel)
scratch=$(mktemp -d "${TMPDIR:-/tmp}/ze-old-writer.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
git clone --quiet --shared --no-checkout "$repo" "$scratch/repo"
git -C "$scratch/repo" worktree add --quiet --detach "$scratch/release" v0.6.0
mkdir -p "$scratch/release/crates/zeppelin-embed/examples/common"
cp "$repo/scripts/fixtures/generator.rs" "$scratch/release/crates/zeppelin-embed/examples/release_fixture.rs"
cp "$repo/scripts/fixtures/common.rs" "$scratch/release/crates/zeppelin-embed/examples/common/mod.rs"
RUSTFLAGS="--cfg release_namespace" CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR="$scratch/target" cargo build --offline --locked --manifest-path "$scratch/release/Cargo.toml" -p zeppelin-embed --example release_fixture >&2
cp "$scratch/target/debug/examples/release_fixture" "$1"
git -C "$scratch/release" rev-parse HEAD >&2
