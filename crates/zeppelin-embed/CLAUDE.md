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

## Task 04 narrow-width invariants

- Persisted quantization identifiers are append-only: `F32 = 0`, `F16 = 1`,
  `Int8 = 2`, and `Bit4 = 4`. Ids 3 and 5 are retired and permanently reserved;
  see `docs/adr/ADR-002-retire-bit1-bit2.md`. They must never be reused.
- Bit4 packs two coordinates per byte, MSB-first. An unused low-order field in
  a final partial byte is zero on encode and is rejected when non-zero during
  scoring or reconstruction.
- Extended-RaBitQ uses the exact critical-value rescale search and stores the
  row norm plus estimator correction. Random rotation is optional and off by
  default in v1; recall validation remains per embedding model.
- The coordinate-order public Bit4 kernel uses an appended runtime-dispatch
  slot. AArch64 NEON extracts fields with shift/mask ladders and ZIP
  interleaving, then accumulates directly from registers; scalar is the oracle
  and non-NEON architectures use that allocation-free fallback. Never
  materialize an expanded row while scoring.
- The optimized Bit4 estimator prepares an in-memory-only query layout with
  even then odd coordinates per 32-value block. NEON scores unsigned high/low
  nibble vectors and applies `2*dot - 15*query_sum` once per row. The persisted
  MSB-first row layout and the public coordinate-order `dot_bit4` kernel remain
  unchanged; provenance is `tasks/evidence/opt-ledger/B1-bit4.md`.
- Repeated Bit4 estimation uses one validated prepared batch dispatch. The
  retained DotProd microkernel interleaves four rows over 64 dimensions,
  shares prepared-query loads, reduces four row sums with pairwise ADDP, and
  applies correction in f64x2 pairs. Query scale includes the exact power-of-two
  `0.5` factor at preparation. The coordinate-order `dot_bit4_batch` entry also
  scores four rows per call without changing its public semantics. Provenance
  is `tasks/evidence/opt-ledger/bit4-parity.md`.

## Task 04 quantization and recall invariants

- Bit4 is the default quantization scheme. Int8 remains a configurable,
  non-default alternative.
- Int8 rows use one per-vector signed affine map plus exactly two `f32`
  factors. Queries use a symmetric signed-byte representation prepared once;
  row scoring is one native i8 dot plus the precomputed query-code sum.
- Recall is strongly corpus-size dependent and small fixtures invert the
  conclusion. A result from a small fixture does not establish behavior at a
  production corpus size. Never quote a recall number without its row count.
- Recall byte counters include every stored code and factor byte read in the
  coarse stage plus every f32 corpus-row byte read in exact rescore. Query bytes
  and output metadata are common across schemes and excluded. No Task 04
  command records wall-clock performance.
