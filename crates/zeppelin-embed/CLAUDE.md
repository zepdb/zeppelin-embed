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
  every unused slot is `u32::MAX`. An active neighbour slot must not name
  its owning node. Cache-line tail padding is zero.
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
- A call that publishes a consolidation returns its final generation after any
  same-call refinements through `MaintenanceReport.consolidation_generation`;
  the merge is admitted whole against the byte budget (charged as the sum of
  input file sizes) and the graph phase resumes through the existing graph
  checkpoint.

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
  open errors. Recovery never publishes a partial prefix. ZE-216 narrows one
  case: before replay, the writable open cuts a final record that is only
  shorter than its declared length (at most the 16 MiB group bound), through
  a verified temporary and rename. A read-only open still refuses it.
- Task 10-A has no sealed-segment fold boundary. Every clean WAL mutation is
  therefore rebuilt into the active segment on open. Task 10-B must persist an
  explicit absorbed-through boundary before it may exclude records already
  represented by a sealed segment.
- The active-state generation is authoritative while a writer is open: it is
  initialized from the manifest generation and advanced by mutations. Sealing
  advances from that value, never from the possibly stale published snapshot.
- `Store::stats().active_segment_bytes` names only the currently published
  active generation. Active allocations retained by an admitted query after a
  mutation are reported separately as `retired_active_segment_bytes`; their sum
  must equal the exact `Active` accounting component until the last owner drops.

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

## ZE-217 delete-by-predicate invariants

- `delete_matching` holds maintenance -> state -> writer -> WAL for the
  whole call. Order is fixed: resolve live ids, write the purge intent,
  one `DELETE_V1` tombstone record, then the physical purge. Intent before
  tombstone is what makes a crash reclaim the bytes; the reverse order
  leaves tombstoned text on disk forever (the crash sweep catches it).
- A failed tombstone removes the intent before returning. A failed purge
  after the tombstone keeps it, so the next writable open completes it.
- The `*_locked` purge/delete helpers take no lock the caller holds; keep
  them lock-free on maintenance/state/writer/WAL or they deadlock.

## ZE-224 expected-revision invariants

- `ExpectedRevision` conditions are checked by `check_revision_conditions`
  under the WAL writer lock, against the committed active segment and
  published snapshot, before any segment copy, tombstone file or WAL
  append. A failure is `RevisionConflict` and must write nothing. Keep the
  check ahead of every side effect when reordering `ingest`/`delete`.
- "Live" means what `get_documents` returns: a tombstoned active row or a
  dead sealed row is absent. Conditions are never persisted or replayed.

## ZE-233 WAL rotation after seal

- A non-empty seal rotates `wal.ze` only after its manifest (with
  `log_seq = absorbed_through`) is durable: it writes and syncs a header-only
  log with `first_seq = absorbed_through + 1` to `.wal.ze.purge.tmp`,
  validates it, renames it over `wal.ze`, reopens and swaps the writer, then
  syncs the directory. Either file recovers the same state.
- The temp sync before the rename is load-bearing: without it a power cut
  can leave an empty `wal.ze` beside a manifest with `log_seq > 0`, which
  open refuses as ahead of the log. The in-crate CrashVfs matrix
  `every_crash_state_of_the_wal_truncation_resumes_after_the_boundary`
  fails on exactly that state if the sync is removed.
- `StoreWal::rewrite` (seal and purge): after the rename the old writer's
  handle names an unlinked file, so the swap may not wait behind the
  directory sync, and a failed reopen poisons the old writer so later
  writes fail loudly instead of landing in the replaced file.

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

## Astra 16 lexical contribution cache

- Reuse sealed live statistics only for the same immutable file metadata,
  verified postings length/checksum and exact live membership. Publication
  remaps reader objects; current reader validation must precede cache reuse.
- Rebuild source ordinals from the pinned snapshot. Cached contributions never
  carry ordinals from a retired assembly. Active keys are exact Weak identities.
- Contribution reservations belong to their Arc lifetime, including evicted
  values held by old queries. Serialize builders and refuse stale-generation
  replacement; do not drop a live reservation to satisfy the next admission.

## Native property-graph domain (ZE-32)

- `property_graph` is separate from the ANN `graph` module. ZE-350 / E15
  decision E.2 supersedes the native node/document identity split: `NodeId`
  and `DocId` share one u128 identity and convert explicitly. Graph allocation
  remains positive and monotone, checks live documents, and never recycles an
  allocated ID. `RelId` remains an independent, nonzero u128 domain.
- A document-backed node records its `DocumentVersion`. Legacy alive document
  rows without a node record have the implicit `Document` label and typed
  column properties. Reads pin active, sealed, and graph state together; open
  does not migrate rows. The first graph touch adopts the node record.
- Document deletion and its node tombstone publish in one mixed WAL run.
  E.6 applies DETACH and incoming Restrict to every document removal path,
  including delete_matching, retention, and purge. A definite refusal must
  precede all durable writes and leave the shared writer usable.
- Arm the shared ManifestPublication fence before durable mutations and
  complete it after both participants are published. An absent document
  reference retains the prior node layout; document-bound layouts are explicit
  and old readers must refuse them. Graph-free v2 bytes remain unchanged.
- Input constructors borrow caller storage and allocate nothing. Staging must
  reserve and own copies, charge aggregate canonical bytes including framing,
  and validate local-slot existence and vector-space identity before publication.
- Scalar/list F64 properties preserve all IEEE bits. Vector coordinates remain
  finite-only. Do not use floating equality for exact retry contents.
- Preserve typed empty lists and the count-zero untyped EmptyList tag. Stored
  null is absence. Names/keys are exact UTF-8, including embedded NUL, without
  path rules or normalization. Metadata lives outside the user property map.
- Batch-local references carry an invariant callback scope; write APIs must
  retain that scope rather than accepting unbranded slot integers from Rust.

## ZE-33: canonical graph contents and replay evidence

- Canonical images describe logical contents, never physical references or
  quantized search codes. Hash/length metadata only rejects mismatches; matching
  metadata always requires exact lossless stream comparison. Storage owns
  validated descriptors, bounded readers and leases across physical relocation.
- Normalize borrowed descriptors in place: byte-sort names, deduplicate labels
  and reject every duplicate property, even equal values. Preserve explicit
  scalar/list tags, typed empties, all F64 payload bits, original finite F32
  vector bits, full u128 endpoints, and absent-versus-present empty text.
- Embedding contents borrow the complete existing document `EmbeddingTower`;
  query-only tower/alignment changes do not change stored document interpretation.
  Catalog compatibility is a later admission responsibility.
- Admit at most 8 MiB including complete logical framing; bound supplied name
  descriptors before sorting. Streaming emits chunks of at most 64 KiB. Exact
  equality uses 2–65536 caller scratch bytes and allocates nothing, including its
  own malformed-input errors. Caller-owned descriptors/backing values remain
  charged to staging. Streaming/comparison poll cancellation between chunks.
- Versioned `OperationProvenance` retains every installing-operation field,
  including explicit preconditions and original changed generation. Missing or
  unsupported versions fail; nothing is inferred. ZE-34 classifies lifecycle
  legality; ZE-38/43 carry these logical records in durable envelopes/checkpoints.
  The ZE-33 `ZGCI`/`ZGOP` logical image framing is not a persisted family decoder
  and does not implement inventory, reclamation, publication or recovery.

## ZE-42: native graph artifact framing

- Required format families 17 (`NativeGraphObject`) and 18
  (`NativeGraphRoot`) use version 1. Existing families 1–16 are unchanged.
  A root envelope contains exactly one nonempty `CheckpointPayload`; successful
  framing validation does not admit a complete checkpoint or GraphStore.
  Writes owns required logical roots, provenance/high-watermarks, and validation
  before replay or cleanup. The read-only codec never repairs incompatible data.
- Objects have a 96-byte header, contiguous 24-byte-header blocks, one ordered
  24-byte directory entry per block, and an 8-byte whole-file checksum trailer.
  The complete object, including all overhead, is at most 4 MiB. Block checksums
  cover payload bytes; directory checksums copy them; the trailer covers every
  preceding byte. References name entire framed blocks, never arbitrary record
  extents. Store/artifact identities preserve all u128 bits.
- Pages are 16 KiB with a 64-byte header, packed 8-byte slots, packed cells and
  zero tail. A `FramedPage` validates geometry and descriptors only. ZE-43 must
  resolve overflow keys and verify strict kind-specific ordering before routing.
  Inline/overflow key descriptors and explicit final-child infinity are distinct.
  Numeric comparators use full u128/u64 fields, never LE lexicographic order.
- Key descriptor logical length includes the kind/namespace prefix and is at
  most 8 MiB. Individual name/key constructor limits do not promise a whole
  request fits: staging must charge key/provenance/framing and content together.
  Overflow extent-list interpretation and streaming comparison remain ZE-43's.
- Fresh store/object nonces use a fallible injected provider, with OS getentropy
  on macOS/Linux and explicit Unsupported elsewhere. Graph platform qualification
  is separate from legacy platform support. There is no timestamp/counter fallback.
  `Vfs::create_new` must preserve existing bytes and return AlreadyExists; its
  default is Unsupported, never a check followed by truncating write. Allocation
  errors distinguish an unowned collision from a possibly partial failed create.
  A reported attempted ID alone never grants cleanup ownership. Writes owns
  sync/publication and recovery classification.
- Exact layouts and reviewed tag assignments are recorded in
  `tasks/evidence/ze-42-native-graph-artifact-codecs.md`; independent golden files
  freeze bytes. No new dependency or foreign persisted format was introduced.

## ZE-35: logical graph catalog participant

- LabelId, RelTypeId, PropertyKeyId and NamespaceId are separate nonzero u64
  domains. Reconstruction rejects duplicate names/IDs inside each domain and
  high-waters below retained IDs; gaps and exhausted high-waters survive. Names
  remain exact UTF-8, including empty strings and NUL. Intern only names required
  by normalized mutations, never names mentioned by an effect-free removal.
- SymbolCatalog is optional bounded staging/reconstruction scratch, not a
  mandatory resident catalog or a whole-catalog copy on each write. Its fixed
  descriptor capacity is reserved fallibly against the caller's already-reserved
  shared allowance and reports actual Vec capacity bytes. It never grows
  implicitly. Borrowed name/tower backing and caller output remain charged to
  their staging owner or immutable storage lease. Later storage owns pages.
- ZGCA v1 is a logical snapshot participant, not a new global persisted family
  or graph root. Its 120-byte little-endian prefix is magic ZGCA, codec u16 and
  required interpretation u16 (both 1), complete byte length u64, StoreInstanceId
  u128, node/relationship high-waters u128 each, four symbol high-waters u64,
  TokenizerEpoch u64, symbol count u64, embedding-present u8 and seven zero bytes.
  The optional document tower follows: u64-length-prefixed model id, version,
  weights digest; dims u32; normalization u16; prefixed prompt; max_tokens u32;
  runtime/compute-units u16; OS-build-present u8 and optional prefixed UTF-8 build.
  Each symbol is domain u8, seven zero bytes, ID u64, name length u64, UTF-8 name.
  The final xxh3-64 covers every preceding byte. Unknown versions/tags, nonzero
  reserved bytes, invalid UTF-8, bad extents/checksums and trailing bytes reject.
  Symbol record order has no logical meaning; no canonical-byte identity is
  inferred from this physical arrangement. Frozen logical fixtures pin both
  optional embedding arms; existing persisted format goldens are unchanged.
- Graph interpretation has one lexical identity and zero or one complete
  document tower. No mandatory timestamp, vector, text membership or graph epoch
  alias is introduced. Query-only tower/alignment changes do not change stored
  document interpretation. Admission calls controlled validate_for rather than
  derived Eq; it checks full fields, not a hash. Sorting, name comparison, UTF-8
  validation, descriptor loops and encoding poll cancellation during real work.
- CatalogDeclaration preserves the storage-owned StoreInstanceId without any
  entropy fallback. Its pure validation seam has no filesystem side effects.
  ZE-38/40 still own durable carriage, coherent high-waters, recovery and refusing
  incompatible interpretation before replay/cleanup; codec success alone does
  not admit a GraphStore or establish whole-store reopen/compaction correctness.

## ZE-34: pure graph key lifecycle classification

- `classify_key` consumes one coherent admitted key state and explicit request.
  Preserve full incarnation IDs, operation/precondition/delete-mode provenance,
  revision and original generation. Exact retry requires actual canonical bytes;
  a larger revision never lets an old incarnation mutate its replacement.
- Deleted keys retain fences after entity bytes are swept. Only explicit recreate
  naming the current deletion revision installs a fresh ID at a newer revision.
  Ordinary create/put cannot resurrect a key. Same-incarnation relationships keep
  directed endpoints and exact type; properties may change.
- Classify Cypher once from its complete final normalized entity image after all
  expressions have been evaluated. Unchanged final contents are NoOp; changed
  contents/delete use checked revision+1, including unkeyed entities. Cypher
  provenance is not a structured retry receipt or generic idempotency token.
- Reject every repeated full key or resolved entity in a structured batch, even
  identical requests. Preserve each replay's original generation in mixed work;
  allocate a checked changed generation only for actual durable participants.
  Net-empty create/delete still changes allocator/fence state supplied by staging.
- Descriptor sorts are allocation-free, fallible and poll at each heap operation;
  exact name comparisons poll every 64 KiB. Batch summaries and private logical
  finalization also forward cancellation. Scratch may be reordered on refusal;
  no admitted state is mutated. Controlled provenance admission uses the same
  complete framing/version/kind rules as the original constructor.
- `PendingKeyChange::install` is private-work logical finalization, not a public
  caller-chosen-ID store operation. The coordinator owns allocator high-water and
  global nonreuse, endpoint existence, Restrict adjacency checks, publication and
  durable provenance carriage. ZE-109 DETACH is one node tombstone with endpoint
  liveness filtering; this classifier never enumerates incident edges. Storage,
  sweep/reclamation, no-WAL/no-generation public paths and crash recovery remain
  their later owners' proofs. PG5 checks pure history, not those durable effects.

## ZE-48: typed plans and query values

`property_graph::query` owns allocation-free borrowed value semantics and
immutable typed plan validation. Query equality is three-valued; grouping
coalesces nulls/NaNs/numerically equal values and uses matching hashes. Neither
uses canonical replay bytes. Exact I64/F64 comparison does not round I64 to F64;
checked arithmetic intentionally permits the specified mixed-number rounding.
`QueryView` pointer identity distinguishes admissions even at the same store and
generation. Constructing a token/reference acquires no lease and proves no
liveness. Public execution/parameter/result owners must not treat it as one.

Private `QueryList` geometry checks depth 16, 524,288 descendants and borrowed span
bounds; packed node/relationship lists retain full 16-byte IDs plus one view token.
`ValueContext` uses existing `QueryControl`, checks work before each unit and
checks byte comparisons/hashes in at most 64 KiB chunks. Caller-owned backing,
output scratch and eventual result/lease ownership remain separately reserved.
Property assignment validates complete homogeneous lists before copying; null
removes, query empty becomes EmptyList, explicit stored typed-empty identity and
IEEE payload bits remain unchanged. ID text formats all 128 bits as 32 lowercase hex
characters without a storage fetch.

`GraphPlan` borrows immutable typed arenas and caller facts. It checks every
expression use in that scope, including shared expressions after WITH; bounds
4096 operators/expressions, depth 64 and 256 columns per scope (SlotId remains u32).
Common slots in joins are equality keys, with null nonjoining; disjoint inputs
form a cross product before the residual predicate. Optional predicates precede
null extension. Relationship origins follow the immutable DAG through joins and
slot renaming under work/cancellation limits, preserving each MATCH PatternId.
Semantic barriers and explicit order facts must survive later optimization.
Mutation inputs require an immediate Eager operator; later base-view reading
clauses reject. Search and mutation cannot share a statement. Each syntactic call
has a checked source-order identity and mandatory eager obligation, including
when LIMIT 0 or empty row inputs would otherwise skip it. Arguments require a
proven singleton source; grouped/per-row contexts reject. `SearchBounds` validates
evaluated k/window without clamping; runtime must call it before retrieval and
validate actual vector/eligibility contents and provenance. No operators execute
in this component.

`PlanBacking` proves visible address spans are included in sorted disjoint
owner-attested retained regions. Aliased/subslice occurrences are charged once;
adjacent regions may cover a continuous span. Addresses are comparison-only and
never dereferenced. Charge region capacities, inventory full capacity and the
separate 64 KiB validator stack envelope against declared retained bytes <= 24 MiB.
Inventory storage may not overlap retained regions. Hidden allocation capacity
and allocator ownership remain truthful owner assertions; ZE-49 supplies actual
runtime reservations and shared-store accounting. This is not whole-query memory
or admission enforcement. Broad qualification deferred by owner to ZE-118 does
not turn focused component evidence into coverage/platform/GraphStore acceptance.

## ZE-38: native graph WAL participant

- NativeGraphWal family19/v1 is separate from legacy WAL family11. Its exact
  hand-written headers, frame tags1..6, full state and provenance carriage are
  frozen in tasks/evidence/ze-38/schema-review.md and the independently minted
  tests/fixtures/graph-wal/complete-v1.bin. Existing family/block IDs stay fixed;
  object BlockKind10 is CommitParticipant with required ZGCP role and version.
- Encode only complete Begin/Change/Commit envelopes, at most16MiB including all
  framing and at most16,384 mutations. Every frame binds full BatchId, sequence,
  index, length and checksum; Commit repeats count and hashes all prior encoded
  frames. Full StoreInstanceId, graph roots, catalog/search/reclaim/inventory
  participants and all logical/physical high-waters are explicit. High-waters
  never regress, generations/sequences never wrap. An empty normalized mutation
  set may still commit allocator/catalog/fence changes.
- Replay validates complete envelopes into a private recovery view. Every
  required semantic/object hook must succeed before a batch escapes, followed
  by a final cancellation check. Missing proof/live objects are errors; deletion
  targets in a validated intent are not implicitly required-live references.
  Codec validation grants no deletion authority. Later corruption invalidates
  publication of the entire private recovery result, including earlier batches.
- Replay::at_watermark checks all complete old framing/scalar chains through the
  exact complete checkpoint boundary and binds its full state. Retired historical
  objects need not exist. A cut header starts at checkpoint.sequence+1; gaps,
  forged/misaligned cutoffs, incomplete historical prefixes and mismatched state
  reject. Replay never truncates, rewrites, copies or cleans up the log.
- WAL owns no heap allocation. Caller buffers/descriptor capacities stay charged
  to their real staging/storage owner; a64KiB fixed stack allowance and finite
  work budget are required. Hashing/copy/UTF-8 work polls in at most64KiB chunks.
  Existing ArtifactFrame admission is a separate storage responsibility: its
  current decoder allocates error strings and scans up to4MiB without in-work
  cancellation. The WAL helper consumes already validated frames and does not
  pretend a caller pre-poll fixes that boundary. Mandatory resolver integration
  must provide controlled admitted objects/extents and complete semantic checks.
- ZE39/40 own writer/publication/recovery coordination; ZE43/46 own record and
  mark/proof semantics. Fixture resolver tests are not whole-store recovery or
  reclamation acceptance. Broad release qualification remains ZE118/E12.

## ZE-49: bounded query runtime and actual resource owners

`property_graph::resources::GraphResources` clones the existing bounded Store
Accounting Arc without allocating an independent budget. It is an accounting
adapter only: it admits no view and proves no store/entity liveness. All graph
participants retain the same <=256 MiB aggregate owner; the Accounting peak is a
monotone real-reservation peak since that owner was created. QueryMemory adds a
caller-thread <=24 MiB sublimit, not a separate aggregate allowance.

QueryArena reserves before fallible Vec allocation, reconciles actual capacity,
and never grows implicitly. It has no bulk-growth API; operators that replace
storage must keep both reservations and implement bounded controlled movement.
QueryInputs accepts lifetime-retained actual Vec/String/Box/array owners or
same-query arenas, unions aliases once, accounts inventory/control capacity, and
checks every visible plan span. Numeric region declarations cannot mint ownership.
GraphPlan::validate_with_fact_vec captures actual facts Vec capacity before its
retained loan; raw-slice validate remains structural-only. The binder uses the Vec
constructor. RuntimePlan keeps all owners plus plan descriptors charged, including
the simultaneous 64 KiB validator scratch; no capacity bytes are dereferenced.

QueryMemory::adopt_shared consumes an authentic same-store GraphReservation into
immutable joint ownership, adding only the query-local charge over its existing
aggregate charge. Failure returns the original reservation. Keep this owner with
the real frozen buffer and free backing first; there is no resize/extraction path.
Writer-local guards remain alongside it. This is not an arbitrary prepaid address
certificate; ordinary Vec proofs would charge backing again. Prefer QueryArena
for query/compiler-owned storage whose borrowed proof is reused downstream.

The pull driver owns one required RetainedView adapter through drain, completion
and final view-first control checks. The later storage owner supplies the actual
GraphReadView/lifecycle capability; QueryView tokens alone do not admit execution.
Operators allocate through that same QueryMemory and count actual examined work.
Flat batches retain bags, full-width IDs and private owned variable arenas; packed
ID lists use 16 bytes per ID. Every 64 KiB byte chunk and each list/row unit checks
control. String cells exist only after exact complete copies of valid &str bytes;
checked private ranges use this invariant without an unpolled UTF-8 rescan.

Every eager source executes once in validated source order before row pulling,
even LIMIT 0. Empty More is invalid. Row and logical prepared-byte limits are
checked before collector copying; failure returns only diagnostics/counters, never
partial rows. Completion is an internal engine adapter, not a host callback. Its
output type cannot borrow the temporary view/rows. FrozenOutput byte metadata is
supplied by that owner and counts initialized represented core/ABI bytes including
in-arena descriptors; each arena has its own 4 MiB limit. Logical collector payload
is a separate PreparedPayloadBytes counter. Retained capacities and external
registration/control overhead remain additionally charged during construction.
ZE-52 owns the actual copied representation and application-accounting transfer.
All fallible freeze work precedes the final check; any failure drops the output.
This preparation driver does not define rollback after an irreversible write.

ZE-51 owns EligibleNodeSet construction/dedup/view semantics and the 524,288-entry
cap; retrieval borrows it. PG9 is runtime bag/work/fault proof, not GraphStore,
query-language, retrieval, public completed-result or durability acceptance.
Broad qualification remains ZE-118 under the owner's deferral. Cold legacy lease
release has separately measured first-use platform allocations; runtime audits
compare an independent cold Store baseline, never warm that path out of evidence.

## ZE-37 private write-staging invariants

- `property_graph::staging` consumes a retained, coherent store/generation/root
  base. Its adapters supply live records and cancellable bounded reads; the
  staging seam neither acquires leases nor publishes graph/search state.
- Classify every structured item against that base, including retries, before
  checked generation/identity selection. Exact retries retain each original
  generation and produce no participant delta. Reject all repeated key/entity
  targets. New local relationship framing may follow private node assignment;
  every rejection drops that preparation without exposing IDs or high-waters.
  Normalize and validate every payload field and aggregate framed input first;
  only resolved endpoint bytes may wait for private IDs. Provenance sizing and
  installed evidence share one encoder. Unresolved fixed-width zeroes occur
  only in length-counting sinks, never canonical output or fingerprints.
- Cypher's private property/text overlay preserves absence, empty text and
  deleted-access errors. Final canonical equality is NoOp; changed revisions
  advance once. Unkeyed create/delete still carries consumed allocator fences.
  The overlay provides no traversal or new binding lookup semantics.
- DETACH emits one node tombstone plus that node's search-membership removal,
  without incident enumeration. Restrict checks alive incidents against the
  admitted base, excluding validated explicit relationship deletions. Physical
  adjacency cleanup and end-to-end search publication remain later owners.
- Every arena charges actual capacity to the existing Store accounting owner
  and the writer's 64 MiB limit. Input canonical/provenance framing is bounded
  by 8 MiB. All loops poll; byte comparisons/copies poll within 64 KiB.
- Core and ABI result arenas each have a separate 4 MiB cap; registry/control
  backing is additional writer/shared capacity. Materialization, registration,
  consuming query-budget adoption and final view checks finish before handoff.
  Complete output layout/row/known writer-overlap limits are admitted from the
  actual pending receipt count before any generation or fresh identity exists;
  materialization consumes those retained capacities without recalculating them.
  Backing drops before its capacity owners on abort. Publication/WAL, real ABI
  integration, graph-store admission and crash/reopen proofs are later tickets.
- ZE-118 retains deferred broad workspace/adversarial and per-crate coverage
  qualification. ZE-37 focused nextest and PG10 results do not replace it.


## ZE-55 scoped frontend capacity

`QueryMemory::reserve_external_capacity` reserves an opaque grow-only guard for
outer compiler backing. It reserves its descriptor, shares the same query and
store account, and cannot create RetainedAllocation or alias/prepayment credit.
The owner must reserve before allocation, reconcile actual capacities and hold
old/new growth reservations during bounded moves; all backing must drop before
the guard. Caller-borrowed source/parameters do not establish owner proofs.
Cypher compile_in uses this seam for frontend allocations and 64 KiB compiler
scratch. Later execution still requires QueryInputs actual-owner capabilities
or separately charged copies. A binding callback is not writer/runtime admission.


## ZE-180 native graph registry accounting

The native graph publication's four fixed-capacity registries (read leases,
mappings, preparations, spills) are a store-lifetime arena, not scratch. They
charge `AllocationComponent::NativeGraph` and surface as
`Stats::native_graph_bytes`, which is always present and is zero unless
`graph-cypher` is enabled. Never charge them to `Temporary`: `Temporary` means
live scratch, it is the only component `Budgets::check` gates against
`max_temp_bytes`, and charging a fixed 852,496-byte arena there both broke
every `temporary_bytes == 0` assertion and made `with_max_temp_bytes(n)` refuse
to open for any `n` below that arena.

Per Task 09 Part C the component returns to zero at close. `drain_and_clear`,
the `Store::close` path, empties the four registries and zeroes the charge
after its final unconditional wait for every read lease. This is safe only
because that path has drained: admission refuses while `closing`.
`cancel_and_clear_best_effort` runs from `Store::drop`, cancels without
draining, and therefore keeps its registries so a survivor registration still
finds its slot; the charge goes with the publication when the last survivor
releases it. Never release registries on a path that has not drained.

`graph-cypher` library tests are part of what must pass. `scripts/ci-gates.sh`
runs them explicitly and unconditionally. The workspace run enables the feature
on macOS and Windows x64 hosts, through
`zeppelin-embed-workspace-tests/graph-result-test-support`, so before ZE-180 no
gate proved these tests on any other host.

## ZE-56 Cypher statement seam

`zeppelin_embed_cypher::execute` compiles Cypher text inside each admission
of `Store::execute_graph_statement` (a doc-hidden wrapper with view-bound search over
the ZE-53 seam) with the admitted `RuntimeContext`, so plan, facts and owners
are charged to that statement. A write statement is compiled at least twice
(read classification, then writer, and once more per writer checkpoint
retry). A compile refusal runs no operator and commits nothing;
compilation happens inside an already-open read admission, so admission
refusals (cancel, closed) take precedence over compile errors and surface
as `StatementError::Query`, not `StatementError::Compile`. The seam types
re-exported from `query::completed` are an internal-crate seam, not the
release API: ZE-66's `GraphStore` owns public lifecycle, query and typed
errors. `Store::create_graph_store`/`open_graph_store` exist only under
`test-support` for tests outside this crate.

A write statement without RETURN lowers to a plan whose root is `Mutate`.
Its result has zero columns and zero rows and its outcome is Committed or
NoOp; the write source drains intermediate rows without retaining result rows.
Pattern work and mutation capacities still apply; result row/payload limits
apply only to returning statements.

### ZE-57 write conformance

The Cypher write harness compares public snapshots before/after execution and
again after reopen. Its eight TCK side-effect counts use entity identity,
global live label-name sets, and (entity, property key, typed value) tuples;
these are tooling-derived counts, not a production counter API. NoOp and
refused writes keep the generation unchanged. `ListKind::Empty` appears only
inside copied entities for canonical stored empty lists; projected query lists
use their query representation. Local extensions cover list refusal, deleted
results, default-budget atomicity, indeterminate recovery and mixed revisions.

## ZE-218 additive schema evolution

- A declared schema reconciles with the manifest schema by column id through
  `Schema::additive_evolution`: persisted columns unchanged, added columns
  nullable, order irrelevant. Anything else is `SchemaMismatch`, whose
  message names the column. A read-write open commits an addition as one
  manifest (next generation, same segments, epochs and `log_seq`) before
  publishing; a read-only open refuses it. No format change: the manifest
  schema list and each segment's columns region already carry any count.
- Sealed segments keep the schema they were sealed with. Snapshot readers
  (`open_accounted`) decode columns against the manifest schema, so added
  columns read as all-null and consolidation merges old and new segments
  under one schema. A segment column that differs from the manifest fails
  loudly. Raw `SegmentReader::open` decodes the segment's own schema.
- `tests/fixtures/schema-v0.4.2` is a store written by the v0.4.2 core; the
  `schema_evolution` suite opens it with added attributes.

## ZE-216 batch atomicity

- One `Store::ingest` or `Store::delete` is one crash-atomic batch. A delete
  is one record. An upsert with two or more changed records writes WAL op 8
  (`UPSERT_V2_BATCH_MEMBER`: `index:u32`, `count:u32`, upsert-v2 payload);
  a one-record batch keeps op 7. Sequence numbers are unchanged.
- `ingest::atomic_batch::committed_mutations` is the only way to read
  mutations out of the WAL: it drops a member run that the log end, a new
  member 0 or a standalone record cuts short (that append never returned),
  and fails loudly on a member that continues nothing.
- Atomicity here is per store. Namespaces have independent WALs and manifests;
  ZE-239/ZE-256 root-coordinated namespace batches provide cross-namespace
  atomicity.

## ZE-240 id128 attributes

- `ColumnType::Id128` uses schema tag 7 in manifests and sealed columns.
  WAL metadata/scalar tag 6 carries exactly 16 little-endian bytes; existing
  tags and encodings are unchanged. Sealed arrays use 16 bytes per row, zero
  bytes for null placeholders, with the existing separate presence bitmap.
- Public core values use `DocId`. Equality/membership preserve all 128 bits;
  ID attributes do not support numeric range, scan ordering or grouping.
- C `ZeAttributeValue` tag 6 uses `u64_value` for the low half and the unsigned
  bit pattern of `i64_value` for the high half. Existing repr(C) layouts stay
  frozen. Node uses unsigned 128-bit bigints and the existing UUID helpers.

## ZE-228: relationship reference policies

- Creation may declare immutable per-type incoming-reference policies in the
  graph catalog. Edges point child -> parent. Restrict rejects a surviving
  referencing child; cascade closes transitively over source nodes, including
  cycles. An explicit edge deletion releases that dependency. Both structured
  writes and Cypher enforce declarations, including explicit Detach, before
  result materialization/publication. Undeclared types retain existing behavior.
- ZGCA v1 remains byte-for-byte unchanged for catalogs without declarations.
  Declared catalogs use codec/required interpretation 2 with the same prefix
  and symbol records, followed by policy records before the existing checksum:
  action u8 (1 restrict, 2 cascade), name length u64, UTF-8 name. Record extent
  ends at the checksum. Empty v2, duplicate names, unknown tags, invalid extents
  or UTF-8 reject. Existing v1 stores remain readable; older readers fail loudly
  on declared catalogs. Policies are retained with every catalog replacement.
- Implicit cascade tombstones use normalized edit provenance (CypherEdit),
  increment revisions and preserve key fences; they have no separate caller
  retry receipt. Explicit input receipts remain one per item. A conflicting
  write or an exhausted revision/memory/work budget rejects the whole mutation.

## ZE-239 root namespace commit

- `namespace_batch` prepares complete private store snapshots, including sealed
  tombstones, then selects them with one `.ze-namespaces` root-record rename.
  The checksummed `ZENS0001` envelope wraps canonical routes, root references,
  intents and participant preparation identities; no existing WAL op changes.
- All prepared files/directories must be fully durable before that rename. A
  writable open adopting a route syncs its root before subsequent writes, so an
  interrupted decision sync cannot strand an acknowledged later mutation.
- Logical namespace writer locks survive redirection for the entire handle
  lifetime. Close releases physical ownership before logical ownership.
- Read-only opens select a complete prepared store without sibling recovery or
  filesystem writes. Old readers retain old snapshots. Missing/corrupt referenced
  metadata fails; original store contents are never a recovery fallback.
- Historically, this first API required closed participant writers, retained
  old/abandoned stores and performed logical deletes. ZE-256 supersedes those
  limits with live participation, reclamation and physical deletion. Export
  through snapshot; moving an enlisted namespace away from its root is refused.

## ZE-256 live namespace batches

Owner decision 2026-10-05 (Anup): record the implemented ZE-256 formats,
reclamation files and failure contract.

- `.ze-namespaces` uses `ZENS0002` for staged participant selections and the
  complete route table. After participant acceptance, normalization publishes
  `ZENS0001` again.
- Append-only WAL op 9, `PREPARED_MUTATION_V1`, carries transaction-bound
  prepared evidence. Preparation alone is not commitment; replay requires the
  matching root decision and complete prepared mutation run.
- Root `.ze-cleanup` records a durable, resumable reclamation intent.
  `.ze-retired` marks retired storage and prevents reopening it as fallback
  storage.
- `.ze-readers.lock` protects reader admission and retained storage against
  reclamation. Lock stubs are never unlinked.
- An indeterminate publication/adoption failure fences participant writes by
  clearing their WAL writer slots and preserves prepared evidence. Participants
  must close/reopen before further writes.

## ZE-383 portable namespace roots

- New namespace roots use an OS-random, nonzero identity in `ZENS0003`;
  publications and retirement preserve that identity around the existing
  `ZENS0001`/`ZENS0002` descriptor. Legacy roots retain their path-bound layout.
- Bootstrap durably publishes the empty root and every `ZENR0002` participant
  reference before writing any portable-bound op 9 frame. The staged decision
  remains the commit point; acceptance and normalization keep their order.
- Every portable reference is published via `.ze-namespace-root.tmp`: write,
  full file sync, rename to `.ze-namespace-root`, full directory sync. Readers
  ignore the temp; namespace reclaim sweeps it under its existing locks.
- A reference claims portable membership: its root ID, name and relative
  location must match, including the selected route for depth 2. Absent
  references denote plain stores. Legacy references under portable roots fail.
  Removing a reference cannot authorize remaining portable-bound evidence.
- Unpublished depth-2 copies open only with the coordinator's internal
  preparation capability. Ordinary opens still require the published route.
  Copies preserve local acceptance evidence for unabsorbed op 9 records.
- Whole completed roots can be copied or renamed; isolated participants still
  require their root. Pending inode-bound cleanup must finish before copying.
  No implicit conversion of legacy roots or standalone snapshots is provided.

## ZE-260 S6c bookkeeping (S7 maintenance foundation)

- Reclaim intent/completion candidate rows may carry family 18 (immutable root
  envelopes) or 19 (superseded graph WALs), using the existing descriptor layout.
  They never enter ObjectInventory; only family 17 has inventory transitions.
  Older intents containing only family 17 remain readable.
- Root candidates pass the ordinary root-envelope codec. WAL candidates bind
  the canonical filename identity, valid same-store WAL header, exact length,
  and xxh3 digest of every observed byte. WAL headers have no allocation serial
  or generation; those descriptor fields record the capture fences instead.
- Current WAL authority, every captured checkpoint WAL, and open proofs' WAL
  authorities remain marked. History uses the existing bounded candidate list,
  durable intent, revalidation, unlink, directory sync, completion, and retirement;
  there is no independent cleanup/unlink path.

## ZE-260 automatic reclamation

Graph writers default to automatic reclamation after 64 MiB of committed
artifact bytes or 32 successful publications, including maintenance commits.
The byte policy is per open writer, with a minimum threshold of 1 MiB;
read-only handles cannot change it. The count cadence is internal. Structured
and query mutations stage first; refusals, replays and no-ops run no maintenance.
A staged change runs due maintenance outside the writer lock, then rebuilds
against the maintained view. A maintenance refusal commits none of the
requested write. Each automatic trigger runs one cycle of at most four bounded
steps. Stale or exhausted cycles
retain debt for the next write. Only successful clear publication and its
checkpoint reset both counters; ordinary checkpoints preserve debt. Nonempty
writable reopen starts count-due. `GraphStore::maintain` exposes one step;
`maintain_cycle` exposes one cycle.

## ZE-64 composed search statements

The statement seam runs vector/text/hybrid retrieval on the Store's lexical
and vector segments, using the document snapshot pinned by its graph lease.
Canonical graph payloads without document rows are not indexed. Ranking, eligibility, expansion and copied results share one admitted
view. Reports retain per-call work including preparation. Vector and text
candidate_count count eligible candidates before top-k: live eligible vector
members and eligible text matches, respectively. These counts are independent
of k, downstream LIMIT, projection and row multiplication. They do not certify
exhaustive ANN scoring; coverage remains authoritative. Hybrid counts its
retained candidate union before final top-k; its ranking policy is unchanged.
A store without a vector space refuses vector/hybrid with NoVectorSpace
(Constraint); search/write mixing and row correlation remain invalid plans.

Owner decision ZE-305 (Anup, 2026-10-05): search through Cypher and GraphStore
stays enabled. The 2026-09-26 MVP cut is lifted; earlier MVP-cut statements
in the graph plans are superseded.


## ZE-58 Cypher search execution proofs

Cypher search runs through the existing borrowed ranking adapter and one
admitted view. Independent calls remain eager query-level sources, including
empty input and LIMIT 0; eligibility is omitted, literal empty, or a singleton
global DISTINCT node aggregate. Projection and aggregation retain reports.
Finite numeric vectors round into the declared f32 domain; the adapter rejects
nonfinite values and f32 overflow. No exact f64 round-trip restriction applies.

Once the statement executor returns a failure, execute preserves that typed
cause and its work counters even if a trailing compiler checkpoint also fails.
Compiler failures before execution retain their original stage and spans.
The seeded GraphStore constructor is doc-hidden and available only in tests
or test-support builds. It adds no release API or prepared-plan interface.
See tasks/evidence/ze-58/README.md for focused proofs and excluded gates.
