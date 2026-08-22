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

## Task 06 metadata invariants

- Metadata filters accept only the closed typed `Predicate` AST keyed by
  `ColumnId`; no parser, string DSL, unsupported marker, LIKE, regex, or GLOB
  operation belongs in the engine.
- Every metadata array, including the required `ts: i64` column, stays aligned
  to segment-local u32 document IDs. Nullness is explicit and never encoded as
  a sentinel value.
- Predicate results are always bounded by the supplied `AliveSet`. In
  particular, negation subtracts from the alive scope and can never resurrect a
  tombstoned row.
- Metadata owns compressed sets through `meta::bitmap::DocBitmap`; direct
  Roaring calls stay behind that boundary. Dictionary codes widen from u16 to
  u32 when cardinality exceeds `u16::MAX` and report typed overflow beyond the
  u32 cardinality limit.

## Task 07 persisted-format invariants

- A row id is the dense segment-local `u32` insertion position. The alive
  bitmap masks that id, and **graph node id equals row id**; no second graph id
  space may be introduced.
- Segment payloads are directory-addressed 16-KB-aligned regions. Unknown kind
  ids are length-bounded and skipped. Reserved ids name postings, graph CSR,
  graph-colocated codes, sign planes, optional clustered PDX blocks, checksum
  tables, and additive vector-space-N triples; they do not reserve file space.
- Vector codes, factors, and f32 rescore rows are separate structure-of-arrays
  regions. Code rows are unpadded, factor records are exactly 12 bytes for
  Bit4 and 8 bytes for Int8, and validated mmap slices feed kernels directly.
- Vector geometry stores scheme, dims, row stride, factor stride, vector space,
  and the identity transform descriptor explicitly. V1 requires vector space,
  transform kind, and transform seed to be zero.
- Xxh3-64 is the only checksum family: every region, every 64-KB chunk, framed
  blocks, segment/manifest headers, and complete files use u64 checksums.
- Manifest rename is the sole commit point. Open reads the manifest and bounded
  segment headers only, refuses snapshots ahead of the durable log, and derives
  orphan reachability from the manifest while honoring active-writer exclusions.

## Task 08 Part A WAL invariants

- Every synchronization names `SyncKind`; Darwin maps barrier/full to
  `F_BARRIERFSYNC`/`F_FULLFSYNC`, while Linux maps them to `fdatasync`/`fsync`.
  Direct standard-library file synchronization calls are CI-forbidden in core
  source.
- Every WAL begins with a 40-byte header: the shared 32-byte `ZEPEMBED` header
  declares family 11, registry version 1, total header length 40, and a
  must-be-zero file length; WAL-owned bytes 32..40 carry `first_seq`. There is
  no whole-file checksum trailer.
- WAL records after that header are little-endian payload-length, `LogSeq`,
  operation, payload, and an xxh3-64 covering every preceding record byte. The
  checked-in WAL goldens freeze this layout.
- Replay validates the file header before any record, requires the first record
  to match its declared `first_seq` and exact increments thereafter, and stops
  before the first invalid record. A checksum-valid successor classifies the
  failure as middle corruption but is never skipped or returned.

## Task 08 Part B1 crash-harness invariants

- `CrashVfs` is test-support code over `MemoryVfs`; `vfs::crash` exists only
  under `cfg(test)` or the explicit `test-support` feature, so ordinary debug
  and optimized shipping builds expose the same public API.
- Crash enumeration is deterministic and capped at 4,096 states. Hitting the
  cap sets `was_capped()` and prints a loud truncation line; protocol matrices
  must reject capped runs.
- Byte-operation damage uses at most 96 deterministic semantic points: format
  transitions, region and selected 64-KB checksum-chunk boundaries, first/last
  region sectors, sub-sector edges, and a fixed-seed interior sample. Each
  point emits prefix, suffix, garbage-tail, zero-tail, and interior-damage
  states. Remaining checksum chunks and sector boundaries are sampled rather
  than uniformly crossed.
- Every operation prefix, every ordered subset of writes since the latest
  sync, and unsafe rename-with-old-content states remain distinct auditable
  schedules even when bytes coincide.
- A sync closes the global reorder epoch. Never enumerate a state that omits or
  reorders a pre-sync write behind a later write.

## Task 08 Part B2a durability-policy invariants

- `DurabilityPolicy` resolves data-file and directory synchronization as
  separate named `SyncRequirement` questions. `Derived` and the `None` tier
  skip explicitly; ordered uses barriers; durable uses full syncs; `Attached`
  returns `AttachedNotYetSupported` and never falls back.
- `DurabilityMode` defaults to `Derived` as a deliberate product contract after
  the repository owner reviewed the durability behavior of competing embedded
  vector stores. The default asserts that another store is authoritative and
  this store is rebuildable; it ignores `CommitTier` and issues no
  synchronization primitive. This is not a gate relaxation. Callers whose only
  copy is this store must select `Durable` explicitly.
- `(Derived, any tier)` and `(Durable, None)` must resolve to identical policies.
  The `none` benchmark column therefore exercises the same resolved policy, but
  it must not be relabeled or reported as a measurement of `Derived`.
- Segment and manifest publication take the validated policy explicitly. Keep
  both directory syncs: their ablations did not fail because the current matrix
  permits an old prefix and does not model post-return directory-entry loss,
  so that result is matrix weakness rather than deletion evidence.

## Task 08 Part B2b WAL write-path invariants

- A complete unmodified operation prefix is a successful-return liveness state:
  manifest and segment publication must expose the new state there. Crashed,
  torn, and reordered states retain the prior prefix safety rules.
- Group commit has no timer or background flusher. The idle caller leads
  immediately; arrivals during its sync form the next group, capped by encoded
  bytes at a default 1 MiB, and the same leader drains it immediately.
- WAL visibility is published in memory before append/sync. `commit_durable`
  additionally waits for the selected tier; barriers order without promoting
  `FaultVfs` bytes to media, while full sync does.
- Checked recovery returns only the trusted sequence prefix and preserves its
  exact replay terminator for diagnostics. `WalReader` and `WalWriter` are the
  real `DurableLog` implementations used by manifest ahead-of-log rejection.
- Single-writer ownership is `flock` on a held open descriptor. The lock file
  may remain after death; kernel lock ownership must not, so deletion is never
  a recovery prerequisite.
- Do not add `F_PREALLOCATE` until WAL rotation defines a finite full extent;
  allocating an arbitrary amount does not turn an unbounded append into an
  overwrite, despite the bounded crash model's 54-to-34 state reduction.
