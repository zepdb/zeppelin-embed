# Core crate guide

## Task 01 invariants

- This crate is the engine boundary. FFI and benchmarks remain separate so
  their dependencies cannot leak into production.
- The source tree contains no engine behavior yet: only empty documented module
  shells, `VERSION`, and deterministic test support are permitted.
- Keep production paths panic-free under the crate-level deny lints. Any test
  exemption must be scoped to that test module or function.
- Every randomized test calls `test_support::seeded_rng` with its fully
  qualified test name. Set `ZE_TEST_SEED` to replay a run; the harness prints
  the derived seed into captured test output so failures expose it.
- Do not enable Roaring's Serde feature. Persisted formats remain explicit.
- Runtime SIMD dispatch is a later task. Never add host CPU probes, a build
  script, `target-cpu=native`, Metal, BLAS/OpenMP, or C++ interop.
- Update this file when a later task discovers a core-specific trap or adds an
  invariant.

## Local gates

Run `scripts/ci-gates.sh` at the repository root. For focused work, run
`cargo test -p zeppelin-embed`, `cargo clippy -p zeppelin-embed --all-targets
-- -D warnings`, and `RUSTDOCFLAGS="-D warnings" cargo doc -p zeppelin-embed
--no-deps`.

## Task 02 invariants

- Darwin integration lives behind `sys::darwin` and is absent from non-macOS
  builds. Keep the safe public wrappers typed and panic-free; every libc or
  Mach call stays inside a documented `unsafe` block.
- The installed SDK values used by the durability wrappers are
  `F_FULLFSYNC=51` and `F_BARRIERFSYNC=85`. Re-verify them against the active
  SDK header when changing toolchains rather than introducing host probing or
  a build script.
- `task_info(TASK_VM_INFO)` is requested with the stable rev1 struct prefix,
  which includes both resident size and `phys_footprint`. The core `sys` lines
  remain subject to the 90% coverage gate; do not exclude platform code from
  coverage.
