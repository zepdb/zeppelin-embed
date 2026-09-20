# ZE-44 core qualification

Local macOS arm64: Darwin27.0.0, Mac15,9 / AppleM3Max /128GiB RAM;
rustc1.93.0(254b59607), cargo-nextest0.9.145(00af4550e). Parent commit
273eb33e66c281da18264d7f5eb7c08165e63ddf imports reviewed ZE-129 onto
0e0064ddb3b1630e8c18050cb78c30ac7bcfb638. No third-party dependency changes.

The real StagedBatch participant now prepares authoritative records, OUT and IN
as one private candidate. Complete 40-byte numeric keys and 328-byte descriptors
reuse ZE-124 inner codecs and ZE-43 immutable sources, checked tree COW, combined
storage ownership and abort inventory. Every retained value is validated under
its original leaf generation; every new range is fully validated before insert.
Predecessor/successor routing preserves disjoint finite/infinite intervals.

Read methods compare authoritative relationship topology and check both endpoint
states. A tombstone hides an edge; a missing required endpoint is corruption.
Point, scan, count, expansion, degree and incident checks apply that rule. Output
capacity follows visibility filtering. Property-only changes preserve OUT/IN;
DETACH changes one node without resolving any incident base/delta. Plain DELETE
checks pre-batch live incidents excluding only explicit relationship removals.

## Runtime and fault evidence

Every command below is scoped to changed paths; nextest isolates tests in separate
processes with no retries. Broad campaigns/workspace qualification remain ZE-118.
Raw logs and hashes accompany the final candidate; unsuccessful exploratory
fixture runs remain labelled separately from passing production evidence.

- Same-batch new node endpoints and new label/type symbols, full-u128 IDs,
  self-loops and parallel relationships; exact directed rows and independent
  generation1/WALsequence101, including no-op sequence preservation.
- Root independent reader/WAL controls, preserved and adopted verbatim, plus
  two matching-root/WAL offset/length substitutions. Thirteen malformed
  metadata/source bindings fail before any private append. Both real tombstones
  and missing logical/payload endpoint forms are exercised.
- Nine real generations reach the ninth-run trigger and emit base9/runs0 at
  sequence109 in both directions.
- Eight real generations [300,300,300,300,300,300,248,1] reach2049 pending and
  emit base2049/runs0 in both directions. The explicit fixture allowance is400M;
  current split-pack participant max264081021 measured work and24191040B storage.
  Fresh-file authoritative relationship and raw OUT/IN reads verify2049 rows.
- The separate2049-change batch at200M returns TreeError::Work, retaining complete
  abort inventory and returning all private charges after drop: storage peak
 15320115B, writer16240940B,28packs. This is refusal evidence, not threshold proof.
- Thirty-six real generations [128×31,125,1,1,1,1] reach4097 survivors at a
  consolidation. Both directions emit exact bases[4096,1], no deltas, with the
  real full-u128 second lower bound and tagged upper infinity. Earlier snapshots
  retain their exact immutable files and physical identities.
- Existing property edit plus same-batch DETACH resolves zero AdjacencyBase/Delta
  inputs; constant root metadata binding remains. Later raw edge deletion,
  explicit self-loop removal and plain node DELETE preserve only the paired
  dead-far-endpoint edge; old views still return their earlier visible counts.
- Real I/O, cancellation and work faults at first IN base append fire after the
  real OUT path. Each returns no candidate, leaves base roots unchanged, retains
  all allocated abort identities, and releases every private owner on drop.
- Allocation audit:12 actual attributed allocations,12 deliberate refusal fires,
  zero unattributed bytes,1591648B storage peak. Every refusal returns typed Memory
  with no candidate; a clean identical run succeeds afterward.

Named owner source controls independently removed run admission, retained a
deleted edge in preflight, and accepted reserved descriptor bytes. All three
produced intended runtime RED(exit100), sources were restored byte-exactly, and
terminal three-test GREEN is retained. Independent predecessor/range/read reviews
also preserve their original REDs, mutants and corrected final receipts.

## Final scoped check and fixture correction

`cargo nextest run -p zeppelin-embed --features allocation-audit --test graph_storage_prepare --test graph_adjacency_store --test graph_directories --success-output final --failure-output final`

The scoped run passed50/51 tests. The newly expanded recursive **test fixture**
then overflowed its ordinary libtest stack at generation33 in the4097 case.
Production hashes remained identical. Moving final file verification into a
nonrecursive helper removed the added per-history-frame stack load; no thread
stack size or production limit changed. The exact failure is retained.

`cargo nextest run -p zeppelin-embed --features allocation-audit --test graph_storage_prepare -E 'test(actual_native_producer_splits_4097_edges_from_bounded_batches)' --success-output final --failure-output final`

Corrected result:1/1GREEN, run d36b52f7-ed9e-4bd9-b084-30c8d575a134. Final4097
fresh-file verification charged24046273 work; complete monotone shared peak
98105711B and storage peak11285696B. Pending2049 fresh-file verification charged
7338172 work with44968892B full shared peak. Ninth-run full shared peak2744812B.
These are component fixture reservations on the real store owner, not process RSS.

`cargo clippy -p zeppelin-embed --features allocation-audit --test graph_storage_prepare --test graph_adjacency_store --test graph_directories -- -D warnings`

Strict scoped lint passes. The original50passing cases, corrected4097 case, and
narrow subsequent matching-root offset/length checks are reported as separate
runs; this evidence does not relabel the original51-test run as all-green.

## Parser boundary

The existing native_graph_adjacency fuzz target now also parses the outer328-byte
range descriptor and asserts exact canonical round trips for both direction roles.
Literal valid OUT/IN, reserved-byte and raw-empty seeds have directed classification
checks. Raw-empty was also replayed once directly through the compiled harness.

`cargo fuzz run native_graph_adjacency --jobs 1 -- -max_total_time=30 -max_len=164000 -print_final_stats=1`

325570 executions in31seconds,10502executions/s,176new units,385MiB harness RSS;
no failure. Harness RSS is tooling and is not the charged production graph owner.
This short parser run is not a broad state/fault campaign.

## Scope and remaining gate

This evidence uses explicit AdmittedBase/catalog fixtures and exact produced
objects. It proves neither GraphStore admission nor a retained lease, the sole
writer's exclusive-create/fsync/WAL publication protocol, public query quotas,
recovery/locking, or production GC. Those owners remain ZE-39/40/45/46.

The separate primitive ZE-129 model is landed. Flat ZE-133 owns the real
producer/file/observer adapter, runtime registry and independent missing-reverse/
ignored-delete controls; these remain mandatory before whole ZE-44 closure.
Independent Sol fixture/accounting review is PASS (report SHA256
e02e35f9112d1f861715f605603ed11a11f94abda699f5a2112f2deee3bcc783),
with all43 frozen entries and all12 unchanged production files verified. The
source/evidence candidate excludes ZE-133 files and the imported ZE-129 commit.
Full workspace/adversarial/long-running campaigns remain deferred via ZE-118.

Root owns two main-only ZE-131 integration edits absent from this older baseline:
add required-features=["graph-cypher"] for graph_adjacency_store in core Cargo.toml,
and add that target to scripts/check-graph-feature-boundary.sh (21 to22 core
targets). Root has observed the exact target-inventory RED then22-target GREEN.
The candidate does not import or overwrite ZE-131 tooling. All45 inherited files
are hash-verified unchanged in inherited-preservation.json.

The range review's commands were saved as original tool-event rows rather than
redirected .log files. reviews/range/original-tool-receipts.jsonl contains the
exact original rows; receipt-origin.json records their original line numbers
and hashes. No output was reconstructed and no review suite was rerun.
