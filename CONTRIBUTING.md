# Contributing to Zeppelin Embed

Thank you for helping improve Zeppelin Embed. The project targets macOS and
Apple silicon while keeping the Rust core portable and covered by CI on macOS
and Linux.

## Set up the repository

Install Rust 1.93 or newer. The checked-in `rust-toolchain.toml` selects the
required compiler and components.

```bash
git clone https://github.com/zepdb/zeppelin-embed.git
cd zeppelin-embed
cargo test --workspace
```

Read [AGENTS.md](AGENTS.md) before changing the code. More specific
`CLAUDE.md` files inside a crate or module add rules for that subtree.

## Develop a change

Keep each change focused. For behavior changes, work RED to GREEN: add or
identify the test that demonstrates the missing behavior, observe the intended
failure, make the smallest correction, and rerun the affected tests.

Run focused tests while iterating. For example:

```bash
cargo test -p zeppelin-embed test_name
```

Every randomized test must use the repository's seeded test support so a
failure can be reproduced with `ZE_TEST_SEED`. Changes to operation ordering,
concurrency, persistence, or failure handling also need the relevant
adversarial coverage described in [AGENTS.md](AGENTS.md).

## Check the change

At minimum, run the formatting, Clippy, and workspace test gates before opening
a pull request:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Run `cargo deny check` when dependencies or license policy change. Run the
coverage, size, sanitizer, fuzz, and other affected gates when the change
touches those contracts. `scripts/ci-gates.sh` is the full local qualification
entry point; some of its tools require separate installation.

## Preserve the architecture

- Fail loudly when a contract is violated; do not hide it behind a fallback.
- Keep sealed segments and published artifacts immutable, and preserve the
  single-writer store model.
- Use strong domain types at public seams and keep production engine code free
  of panics.
- Keep persisted formats explicit and hand-written. Format changes require a
  versioned compatibility decision and tests against persisted bytes.
- Keep threading and SIMD dispatch explicit. Do not add Rayon, build-host CPU
  probing, or a non-system allocator.
- Do not add a core dependency without updating the dependency decision and
  passing the deny-policy audit in [AGENTS.md](AGENTS.md).
- Treat the C ABI as append-only. Regenerate its header with cbindgen and pass
  the header-drift and ABI contract tests for every public ABI change.

## Prepare a pull request

A pull request should explain the problem, the resulting behavior, and the
validation performed. Include RED and GREEN evidence for behavior fixes, call
out persisted-format or ABI effects, and identify any checks that could not be
run. Put benchmark methodology and raw measurements under `tasks/evidence/`
before making a performance claim.

Avoid unrelated cleanup and generated build output. Keep commits small enough
to review as one coherent change.

## License

Zeppelin Embed is licensed under the [GNU General Public License v3.0](LICENSE).
By submitting a contribution, you agree that it may be distributed under that
license and confirm that you have the right to submit it.
