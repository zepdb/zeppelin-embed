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

## Task 03 invariants

- Kernel callers validate equal dimensions and batch shapes before the hot
  path. The six public kernels use `debug_assert!` only; i8 dimensions are
  bounded by `MAX_DOT_I8_DIMENSION = 65,536`, whose worst-case sum is `2^30`.
- SIMD selection is runtime-only and cached once. Engine initialization must
  call `kernels::initialize()` so `ZE_KERNEL=scalar|neon|avx2` failures remain
  typed; raw kernel calls safely select the best detected arm when no override
  is requested.
- Stable Rust 1.93 still marks `vdotq_s32` unstable. The SDOT-equivalent NEON
  baseline therefore emits exactly one runtime-gated `sdot` instruction with
  `asm!`; never move that instruction outside the detected DotProd table.
- The scalar f16 bit conversion defines zeros, subnormals, normals, infinities,
  and NaNs without `half`. NEON f16/f32 variants use four independent vector
  accumulators; equivalence uses a fixed `1e-5` dot-product backward-error
  bound scaled by `SUM|a_i * b_i|`, not by the cancellation-sensitive result.
- Stable Rust 1.93 also marks `vcvt_f32_f16` unstable. The runtime-FP16 table
  emits `fcvtl` through documented inline assembly; the fallback keeps the
  scalar bit conversion but still accumulates converted blocks in vectors.
- `KERNEL_KNOB_SPACE` and `BASELINE_KERNEL_CONFIG` are data contracts for Task
  27-H. I8MM and SME2 tiers remain detected/reserved but unimplemented; no
  tuner, ledger, roofline calculator, L2 distance, or PDX scan belongs here.
