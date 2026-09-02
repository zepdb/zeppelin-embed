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

## Task 19-M2 fixed-stride graph format invariants

- Region kind id 7 is the fixed-stride graph node-block region. Task 07
  originally reserved that numeric id under the obsolete
  `graph-adjacency-csr` name; the id is preserved while the source name and
  family-12 registry entry describe the format that actually shipped.
- Graph node id is exactly the dense segment-local row id. Block address is
  `region_base + row_id * stride`; block zero starts at region offset zero, so
  Task 07's 16-KB region alignment also aligns every block to 128 bytes.
- V1 stride is `round_up_128(ceil(padded_dims / 2) + 16 + 4 * max_degree)`.
  Padded dimensions are a multiple of 128 and every padded Bit4 nibble is zero.
  Bit4 codes retain the frozen MSB-first layout and the following three f32s
  retain `Bit4Factors::persisted_fields()` order.
- Degree is u8, flags use only bits 0 (entry seed) and 1 (hub), the following
  two bytes are zero, active neighbours are little-endian dense row ids, and
  every unused slot is `u32::MAX`. Cache-line tail padding is zero.
- A 128-byte trailer follows `node_count * stride` block bytes. Its xxh3-64
  authenticates every block plus the interpretation-critical trailer prefix;
  the task-07 directory checksum, 64-KB chunk checksums, and whole-file
  checksum remain independently required.
- Corrupt graph bytes surface a typed error. The only recovery exception to
  fail-loudly is this derived accelerator: the query-facing graph-load seam
  retains that error and returns complete exact-scan results. It does not
  generalize fallback behavior to another segment contract.

## Task 19-M5 query-default invariants

- `GraphSearchRequest::new` is the no-tuning shipped path. SIFT-class adaptive
  ef is `max(2*k, max(ceil(1.4*k), 140))`; angular is the provisional `4*k`
  profile. Both clamp to graph rows. An explicit ef uses `with_ef` and is
  typed-closed when it is below k or beyond the graph.
- Traversal exactly rescores the complete retained pool from f32 rows through
  `quant::rescore::rescore_top_k`; no graph-local exact-rescore loop may fork
  ordering, byte accounting, row validation, or prefetch behavior again.
- One alpha=1.0 build pass is the shipped default. The explicit alpha=1.2
  second-pass arm stays available for research, but M5 measured it slower at
  matched recall despite saving 2.643% hops. Constants and the load-tainted
  10-process evidence live in `docs/19-m5-defaults.md`.

## Task 19-M6 filtered-graph invariants

- A filtered traversal navigates through matching and non-matching graph rows,
  but inserts only rows in the planner's effective `alive AND filter` bitmap
  into the retained pool. That complete retained pool still uses M5's sole
  `quant::rescore::rescore_top_k` path.
- The fail-closed `4 * ef * max_degree` visited cap remains a typed
  `VisitedCapExceeded` error. A separate filter-only visited budget is a normal
  selectivity disposition and must answer exactly from the allow-list with a
  `VisitedBudget`/`GraphExactFallback` plan report.
- `FilteredGraph`, `GraphExactFallback`, the requested/effective ef pair, and
  any explicit-ef widening are reported from the branch that actually ran.
  Low-cardinality graph filters go directly to exact allow-list execution.

## Task 19-M7 multi-segment graph-search invariants

- One query-local exact-distance bound is tightened after each serial segment.
  Sealed segments are ordered by descending row count, then ascending segment
  id, so the most likely source of an early global top-k runs first using only
  manifest metadata.
- A segment may be skipped only when its persisted original-row norm enclosure
  proves every exact squared-L2 result strictly worse than the current global
  k-th distance. Equality is never pruned, tombstones remain conservative, and
  the independent per-segment path remains available only to unit tests as the
  byte-exact differential oracle.
- The shared bound is stack-owned by `search_pinned`; reusable graph cache state
  contains only query-independent validation, entry seeds, norm ranges, and
  exactly accounted scratch. Concurrent or later queries never inherit a
  competitive distance.

## Task 19-M8 consolidation invariants

- Consolidation is a segment merge, never a cross-segment graph: live rows
  from every published sealed graph segment are copied into one new segment
  with new dense row ids, then one graph is built over the union. No second
  graph id space exists; the output is an ordinary task-07 segment.
- The store-level policy is pure manifest metadata: merge at three or more
  graph segments, or at exactly two when the largest holds under 70% of the
  total rows. Scan-tier segments are excluded from merges in v1.
- Postings are never copy-forwarded across a merge. Region kind 6 is rebuilt
  from carried stored text through the store's frozen tokenizer, exactly as
  seal builds it; an input carrying postings without stored text defers the
  consolidation with a typed report.
- The N-to-1 manifest commit precedes every unlink. The merge intermediate
  is an unpublished `segment-*.zseg` orphan until then, reclaimed by the
  existing open-time reachability sweep, and readers holding the old
  snapshot keep their mmaps through the lease mechanism.
- `.consolidate.checkpoint` is the only new persisted artifact: a private
  `ZECONCP1` resumability file (fixed name, one writer, at most one
  consolidation in flight), refused-and-cleared on any validation failure,
  never read by queries. A resumed merge revalidates the intermediate's
  xxh3-64 against the checkpoint before skipping the merge pass.
- The mutation returns the generation it changed through
  `MaintenanceReport.consolidation_generation`; the merge is admitted whole
  against the byte budget (charged as the sum of input file sizes) and the
  graph phase resumes through the existing graph checkpoint.

## Exact top-k tie invariant

- Per-segment exact cuts retain every candidate tied with the k-th score.
  Document-presenting paths discard ties only through
  `compare_search_candidates`, after document identity has been joined.
- Physical seams remain exact-k and preserve descending score then ascending
  row id: `scan::top_k` and `Store::top_k_with_options` truncate boundary ties
  before returning.

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
  bytes at a 1 MiB default, except that a resolved data-file requirement of
  `SyncRequirement::Sync(SyncKind::Full)` uses 16 MiB. An explicit
  `create_with_max_group_bytes` override always wins, and the same leader drains
  the group immediately.
- WAL visibility is published in memory before append/sync. `commit_durable`
  additionally waits for the selected tier; barriers order without promoting
  `FaultVfs` bytes to media, while full sync does.
- Checked recovery returns only the trusted sequence prefix and preserves its
  exact replay terminator for diagnostics. `WalReader` and `WalWriter` are the
  real `DurableLog` implementations used by manifest ahead-of-log rejection.
- Single-writer ownership pairs an `fcntl(F_SETLK)` record lock with an
  in-process registry keyed by the store directory inode. Record locks are not
  inherited across `fork`; the registry preserves same-process exclusion.
  Because closing any descriptor for `writer.lock` releases the process's
  record lock, no code outside `StoreLock` may open that file. The file may
  remain after death; kernel lock ownership must not, so deletion is never a
  recovery prerequisite.
- Do not add `F_PREALLOCATE` until WAL rotation defines a finite full extent;
  allocating an arbitrary amount does not turn an unbounded append into an
  overwrite, despite the bounded crash model's 54-to-34 state reduction.

## Task 09 Part B accounting invariants

- `Store::stats()` is admitted only while `Open`. Every numeric field is an
  exact component counter, a `mincore` residency count, or (for Darwin
  `phys_footprint`) the existing `TASK_VM_INFO` kernel counter; never substitute
  a process-wide estimate.
- Anonymous byte accounting is attached to fixed-capacity component vectors.
  A capacity is budgeted before `try_reserve_exact`, and `Accounted<Vec<_>>`
  exposes no growing mutation beyond its pre-accounted element limit.
- `max_resident_bytes` covers all component-owned anonymous arenas;
  `max_temp_bytes` is an additional ceiling on the temporary component. A
  rejected reservation changes neither counter and leaves the handle usable.
- The production allocator remains unchanged. The global allocation wrapper is
  compiled only by the `allocation-audit` feature, and its isolated CI test must
  stay single-threaded.

## Task 09 Part C snapshot-remap invariants

- `Store::prepare_segment` is an ownership-transfer seam for already-built
  fixed-capacity vector buffers, not an ingest API. Metadata and alive state
  remain borrowed until task 10 supplies the mutation path.
- `Store::seal_snapshot` writes through task 07, commits a complete one-segment
  snapshot, remaps it only through `SegmentReader`, publishes the read-only
  mapping, and then drops the accounted anonymous vector buffers. It replaces
  the complete snapshot; it does not append or compact segments.
- Read-only protection is a kernel-observed contract. The integration test must
  inspect the live VM region. The BL-105 production-`mincore` test uses a
  no-cache mapping larger than the 1,024-page status batch, observes at most 10%
  residency before touching it, volatile-reads every page, and then requires at
  least 90% residency plus an 80%-of-mapping rise on that same mapping. Do not
  claim deterministic post-touch eviction on macOS: successful public discard
  advice can leave clean file-backed pages resident in the unified cache.
- `rss_flatness` is ignored outside the macOS measurement lane. Until task 10,
  it loops open/publish/lease/mmap-query/close over one task-07 fixture; it does
  not claim to cover per-iteration ingest, WAL mutation, or sealing. Its 5 MiB
  `phys_footprint` target remains supporting evidence beside a non-ignored
  20-iteration gate that requires the exact accounting sources for
  `Stats::mapped_bytes` and `Stats::resident_owned_bytes` to return to zero
  after every close; `Store::stats()` itself remains unavailable once closed.

## Task 09 Part D query-lifecycle invariants

- Every store-admitted parallel scan carries exactly one `Deadline` or
  `CancelToken`, holds its snapshot lease until every partition has stopped,
  and returns no candidates on timeout, caller cancellation, or close
  cancellation. All three typed errors report `partial: false`.
- Scan partitions check cancellation before work and every 64 rows (every 16
  Bit4 four-row batches). The query caller converts a monotonic deadline into
  the same relaxed atomic state while workers run, so hot loops do not read the
  clock or take a mutex. Cancellation is never deferred to a partition edge.
- Parallel scans run only on the store's lazily started persistent explicit
  worker pool. Worker ids come from the threads that execute production
  partitions, and `close()` cancels admitted queries, joins every parked
  worker, then releases the mapped snapshot. The low-level `scan::top_k`
  primitive remains single-threaded and outside store lifecycle admission.
- The persistent worker-registry vector is an accounted store arena:
  `Stats::query_pool_bytes` contributes exactly to `resident_owned_bytes` for
  the pool's lifetime and returns to zero when close drops the pool.

## Task 10 Part A active-mutation invariants

- Opening a non-empty `wal.ze` requires replay to reach `CleanEnd`; invalid
  headers, torn tails, checksum failures, and sequence corruption are typed
  open errors. Recovery never truncates the file or publishes a partial prefix.
- Task 10-A has no sealed-segment fold boundary. Every clean WAL mutation is
  therefore rebuilt into the active segment on open. Task 10-B must persist an
  explicit absorbed-through boundary before it may exclude records already
  represented by a sealed segment.
- The active-state generation is authoritative while a writer is open: it is
  initialized from the manifest generation and advanced by mutations. Sealing
  advances from that value, never from the possibly stale published snapshot.

## Task 10 Part B seal invariants

- `Manifest::log_seq` is the durable absorbed-through boundary. Open rejects it
  when it exceeds the checked WAL end, rebuilds the active segment only from
  greater sequences, and retires the absorbed in-memory WAL prefix only after
  the appended manifest and read-only snapshot are published.
- Active sealing appends one task-07 immutable segment to the complete manifest
  segment set, folds the active alive/tombstone state, empties active storage,
  and advances from the authoritative active generation. It never builds a
  graph. Task 10-C now stamps the canonical `ts` clustering-key range at this
  existing seal boundary without adding another payload pass.
- Sealing an empty active segment is an idempotent no-op that returns the
  current generation without writing a segment or manifest. Lifecycle,
  writer-ownership, and cancellation checks still run first.
- Task-10 sealed rows carry optional region kind 12 / family 13 with exact
  24-byte little-endian `(doc_id:u128, revision:u64)` records. Existing task-07
  segments omit it and continue to return no application document identity.

## Task 10 Part C partition-retention invariants

- `ts:i64` is the canonical clustering key. Active rows account its storage,
  timestamped WAL upserts use append-only operation id 4, and seal computes the
  inclusive live-row `[min_ts, max_ts]` while the rows are already in memory.
  An all-tombstoned segment stamps `ClusteringKeyRange::Empty`; `Unstamped` is
  reserved for older/general segment writers and is never guessed from payload.
- `drop_partition(start..end)` is half-open and drops only an `Empty` segment
  or a bounded segment with `min_ts >= start && max_ts < end`. Overlapping
  boundary straddlers remain reachable and are returned in the typed report.
  Selection reads only the manifest; reading a segment to recover `ts` is a
  performance-contract violation.
- The manifest commit omitting dropped segments precedes snapshot publication
  and every unlink. Store admission is held across commit/publication so later
  queries cannot pin the old generation; earlier queries retain the open file
  and mmap, whose inode survives POSIX unlink until their snapshot is released.
- Retention is a pure window-to-range decision plus an explicit
  `apply_retention`/`drop_partition` call. No engine timer, daemon, scheduler,
  rewrite, WAL scrub, or physical-purge token belongs to Part C.

## Task 10 Part D physical-purge invariants

- `purge(ids)` durably records one pending intent and returns its token before
  rewriting artifacts. `await_physical_purge(token)` resolves only after every
  affected sealed segment has been replaced in the manifest, each old segment
  path has been unlinked, the WAL has been atomically replaced, and the intent
  has been removed. Open detects a surviving intent and completes or cleanly
  restarts the same idempotent protocol.
- Before writing the intent or any replacement, purge checks every affected
  sealed segment and rejects with `InsufficientTempSpace` unless filesystem
  free bytes are strictly greater than 120% of that segment's file size. A
  rejection is mutation-free.
- Segment replacement compacts survivors into dense row ids and rewrites codes,
  quantization factors, f32 rescore rows, alive state, document versions,
  columns, stored metadata, and an existing lexical Postings region in that
  same survivor order. Graph node blocks
  are deliberately omitted because their neighbor ids name the old dense row
  space; `maintain()` is responsible for rebuilding a graph later.
- WAL retirement is not physical erasure. Purge serializes only the surviving
  active state into a new WAL image, syncs the temporary file as required,
  atomically renames it over `wal.ze`, syncs the directory as required, and
  retires the old writer only after replacement. Therefore no purged payload
  record remains reachable by any path when the token resolves.
- Manifest replacement commits before old-segment unlink, one segment at a
  time. Orphan replacement files and old segments are swept against the current
  manifest during recovery; the durable intent is the sole completion marker.

## Task 21 Part A epoch-identity invariants

- Open enforces the complete four-branch identity table before WAL recovery or
  creation: a non-empty registry plus the same declared identity opens; a
  non-empty registry plus a different declaration is `EpochMismatch`; a
  non-empty registry without a declaration is `EpochUndeclared`; and an empty
  registry preserves legacy behavior only when no identity is declared. A
  read-write store created with a declared epoch commits the registry at
  creation, before any WAL write is admitted, so an unsealed store cannot be
  reopened under a different epoch; this creation-time stamp is necessary
  because `close` does not seal. A read-only declaration without a manifest and
  an existing empty-registry manifest both reject as `EpochUnstamped`. Every
  rejected open leaves the manifest and WAL untouched.
- `OpenOptions` is no longer `Copy` because it carries a declared `StoreEpoch`;
  this is a deliberate breaking public API change.
- A stamped store requires every embedding write to declare the matching
  embedding/tokenizer identity. Once a migration starts, the old epoch becomes
  read-only; that is the recorded product rule even though Part A does not
  implement migration.
- `EpochId` is an xxh3-64 digest over the canonical identity fields only. Store
  generations, document counts, row counts, timestamps, and every other kind
  of mutable state never enter the digest.
- Manifest family 10 v2 is the epoch-carriage layout; v1 is rejected at the
  version boundary with `FormatCheck::Version`. Its prefix remains
  `generation:u64`, `log_seq:u64`, the three u32 segment/epoch/schema counts,
  and a reserved-zero u32. The published alias follows as `present:u8`, seven
  reserved-zero bytes, `embedding_epoch:u64`, `tokenizer_epoch:u64`; absent
  requires both ids to be zero. Each segment's existing 36-byte record is
  followed by `present:u8`, seven reserved-zero bytes, `embedding_epoch:u64`.
- Each v2 `EpochMeta` is `embedding_epoch:u64`, `tokenizer_epoch:u64`, the full
  document tower, the full query tower, then a length-prefixed alignment
  digest. Each tower is length-prefixed model id, model version, and weights
  digest; `dims:u32`; `normalization:u16`; length-prefixed prompt/prefix;
  `max_tokens:u32`; `runtime:u16`; `compute_units:u16`; then OS-build
  `present:u8`, three reserved-zero bytes, and an optional length-prefixed OS
  build. Both towers and the alignment digest enter `EpochId`; mutable store
  state never does. Schema records and the trailing optional `TSR1` clustering
  extension keep their prior encoding and order.
- A non-empty registry requires an alias naming one complete embedding/tokenizer
  identity and an embedding epoch tag on every segment. A segment tag may name
  any registered embedding epoch so migration can later carry old and new
  segment sets together. An empty registry requires no alias and only unstamped
  segments. Unknown aliases, unknown segment ids, duplicate identities, and an
  `EpochMeta.id` that disagrees with its full embedding description fail loudly.
- D1 lands only manifest carriage. Migration execution, epoch transitions,
  rollback, `drop_epoch`, and per-record WAL epoch tags remain unimplemented;
  WAL op-7 bit 5 stays reserved-unused.
- Persisted-format changes are AUTHORIZED, including minting a region kind, a
  format family, or a WAL op id. Do not stop and ask. This crate previously
  recorded a "last free format change" that "lapses" at the migration task;
  that framing was retracted by the owner on 2026-08-25. There are no live
  customers and nothing is shipped, so a wrong shape is a delete and a re-mint,
  not a permanent scar. The only trigger that ends this is a real user holding
  real data, or a published binary someone has installed -- never a task number.
  Authorization does not excuse the verification that has value: read the id
  file immediately before minting, keep existing goldens byte-identical unless
  deliberately breaking one and say so when one moves, prove old artifacts are
  explicitly opened or rejected, give every new region its own frozen golden,
  and record the change here.

## Task 21 D6/D9 epoch-transition invariants

- `Embedder::embed` returns either one complete owned vector or a typed
  `EmbedderError`; delegate failure and timeout are distinct variants, and no
  partial output buffer crosses the seam.
- Alias switch, rollback, and `drop_epoch` require an empty active segment and
  `Manifest.log_seq == WalWriter::durable_end()`. They never infer ownership of
  unabsorbed WAL records.
- Alias switch compares the target epoch's complete live document/revision
  multiset with the published epoch before commit; missing, unexpected, or
  duplicate target rows are a typed `IncompleteEpoch` rejection.
- A published snapshot keeps every epoch's segment mapping alive for exact
  accounting, health checks, and physical purge, but its public query segment
  set contains only the atomically published alias. The current response epoch
  changes with that same manifest publication.
- `drop_epoch` refuses the published alias, commits a manifest omitting the old
  epoch's segments, publishes that snapshot, and only then unlinks their files.
  Registry history remains, but an alias target without retained segments is a
  typed `EpochUnavailable` error, so rollback after drop cannot succeed.

## Task 17 Part B text and typed-column ingest invariants

- New document upserts use WAL operation id 7 only. Its payload begins with a
  little-endian `u32` field-presence bitmap, then the fixed
  `(doc_id:u128, revision:u64)` identity. Bit 0 carries the vector, bit 1 UTF-8
  text, bit 2 the canonical timestamp, bit 3 opaque stored metadata, and bit 4
  typed column values. Bit 5 is reserved-unused for a future per-record epoch
  tag; bits 6 through 31 are reserved-unused. A set field follows in bit order.
  Operations 1 through 6 remain readable and are never reinterpreted.
- The optional sealed lexical container is region kind 6 / format family 8.
  Version 1 is hand-written little-endian bytes: the `ZFTS` header freezes row,
  span, field, term-byte, and postings-byte counts; fixed 48-byte term/field
  spans locate each embedded posting list; each field carries one dense u32
  length per row; concatenated term bytes and the existing validated postings
  blob follow. Reserved words are zero. Open validates ordering, row bounds,
  lengths, every embedded posting stream, and cross-region row count before use.
  Older segments omit the region and continue to open.
- Store creation may commit a typed `Schema`; reopen uses that manifest schema
  and rejects an explicitly different declaration. Ingest validates unknown,
  duplicate, missing-required, and type-mismatched values before WAL append.
  Seal materializes the same values through `ColumnStoreBuilder`; the manifest
  schema encoding is unchanged.
- Lexical local rows join to the existing document-version region and hybrid
  fusion joins on stable `DocId`. A postings-bearing segment without document
  identity is a typed error, never a dropped candidate. The existing fusion
  module remains the sole owner of normalization, alpha policy, RRF fallback,
  termination, and `FusionReport`.
