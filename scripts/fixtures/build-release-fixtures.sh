#!/usr/bin/env bash
set -euo pipefail
repo=$(git rev-parse --show-toplevel)
out=${1:-"$repo/tests/fixtures/releases"}
mkdir -p "$out"
out=$(cd "$out" && pwd)
scratch=$(mktemp -d "${TMPDIR:-/tmp}/ze-release-fixtures.XXXXXX")
# Worktrees and their build outputs live outside the repository tree.
cleanup() {
  for tag in v0.4.2 v0.5.0 v0.6.0; do
    git -C "$scratch/repo" worktree remove --force "$scratch/$tag" 2>/dev/null || true
  done
  rm -rf "$scratch"
}
trap cleanup EXIT
# A private local clone permits worktree metadata writes in restricted sandboxes
# without touching the caller's shared git administration directory.
git clone --quiet --shared --no-checkout "$repo" "$scratch/repo"
export CARGO_BUILD_JOBS=3 ZE_TEST_SEED=0
for tag in v0.4.2 v0.5.0 v0.6.0; do
  git -C "$scratch/repo" worktree add --quiet --detach "$scratch/$tag" "$tag"
  release="$scratch/$tag"
  sha=$(git -C "$release" rev-parse HEAD)
  mkdir -p "$release/crates/zeppelin-embed/examples"
  cp "$repo/scripts/fixtures/generator.rs" "$release/crates/zeppelin-embed/examples/release_fixture.rs"
  cp "$repo/scripts/fixtures/common.rs" "$release/crates/zeppelin-embed/examples/common.rs"
  # common.rs is a module, not a standalone example.
  mkdir -p "$release/crates/zeppelin-embed/examples/common"
  mv "$release/crates/zeppelin-embed/examples/common.rs" "$release/crates/zeppelin-embed/examples/common/mod.rs"
  flags=""
  if [[ $tag == v0.6.0 ]]; then flags="--cfg release_namespace"; fi
  (cd "$release" && RUSTFLAGS="${flags}" CARGO_TARGET_DIR="$scratch/target/$tag" cargo build --offline --locked -p zeppelin-embed --example release_fixture)
  fixture="$out/$tag"
  if [[ -e $fixture ]]; then echo "Refusing to overwrite $fixture" >&2; exit 1; fi
  mkdir -p "$fixture"
  generator="$scratch/target/$tag/debug/examples/release_fixture"
  "$generator" seed "$fixture"
  "$generator" oracle "$fixture"
  cat > "$fixture/README.md" <<README
# $tag release fixture

Built by scripts/fixtures/build-release-fixtures.sh using the public Rust API
at tag $tag, commit $sha. ZE_TEST_SEED=0; fixed ids, vectors and timestamps;
no randomized inputs (the seeded_rng convention therefore needs no RNG).

Documents 1, 2, 3 were sealed; 3 was then deleted; 4 remains in the unsealed
WAL tail. The seeding process leaks the durable writer and exits without close.
Every live row carries text, vector, timestamp, rank=id and opaque metadata.
expected.json records queries and generation from this release's own reader.

For this plain store, fixed inputs and deterministic seal ids make manifest,
segment and WAL bytes deterministic by construction; repeated-build byte
identity has not been measured. No data files are known to be non-byte-stable.
Empty lock files contain no data. No namespaces or op 9 are present.
README
  if [[ $tag == v0.6.0 ]]; then
    namespace_fixture="$out/v0.6.0-namespaces"
    if [[ -e $namespace_fixture ]]; then echo "Refusing to overwrite $namespace_fixture" >&2; exit 1; fi
    mkdir -p "$namespace_fixture"
    "$generator" namespace-seed "$namespace_fixture"
    "$generator" namespace-commit "$namespace_fixture"
    cp "$fixture/README.md" "$namespace_fixture/README.md"
    cat >> "$namespace_fixture/README.md" <<'README'

Namespace variant (supersedes the plain-store description above): a and b
participated in a committed public namespace_batch (op 9 prepared mutation).
The batch revises id 4 to revision 2 with identical payload; both WALs retain op 9.

NOT relocatable today: bug ZE-370. Read-only open after copying fails with
"namespace requires its original transaction root"; rewriting the reference
alone then fails op 9 replay with TransactionBinding. The ignored format_compat
relocation test checks the refusal and hashes every file to prove no mutation.

Namespace identities bind the absolute root path; transaction ids include PID
and clock time. Namespace records, reference files, op-9 WAL records and
transaction-named accepted/manifest artifacts are not byte-stable. Prepared
segment names and manifests also vary with the participant binding. No bytes
are patched; this namespace fixture is separate from the plain-store gate.
README
  fi
  echo "$tag $sha generated at $fixture"
done
