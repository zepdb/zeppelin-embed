# ZE-43 tree implementation review proposal

This records the tree algorithm before implementation. The current 16 KiB page,
PhysicalRef and TreeKind formats from ZE-42 remain unchanged. Record and overflow
payload layouts will receive a separate review before new tags are allocated.

A bounded directory module under storage/tree owns routing, immutable replacement,
lookup and range traversal. It consumes a required immutable block-source adapter
and a private block-sink adapter. A resolved block can be constructed only from
an ArtifactFrame plus an exact directory-member PhysicalRef, and retains the
validated store/artifact/reference identity. Consumers compare the requested ref,
store, generation and kind; a source cannot substitute another validated block.
The source/lease owner accounts its mappings/cache, and the sink accounts all
new retained capacity and keeps explicit abort inventory. Neither seam publishes,
syncs, unlinks, grants liveness, or manufactures a GraphReadView. Tests use actual
framed immutable artifacts, not a fake successful production resolver.

Each operation receives mandatory QueryControl plus checked work/byte counters.
Caller scratch is admitted at its complete capacity (not current contents), and
private preparation remains within 32 MiB and the caller's shared store allowance.
The tree itself does not make that allowance into a second independent budget.
Tree-owned scratch will be fixed capacity, fallibly reserved once, reused, and
reported exactly. All additional sink/cache allocations remain owned/charged by
those adapters. Tests audit the actual allocator; no Vec growth after admission.

Eight GraphRoots slots are fixed in TreeKind order: node_directory,
rel_directory, key_fences, label_index, rel_type_index, out_ranges, in_ranges,
object_inventory. Optional roots have explicit absence; typed root construction
checks comparator role and reference kind/version. ZE-38 owns its wire carrier,
not a second semantic graph-root definition.

Numeric keys compare all u128/u64 bits. Key/fence keys compare entity tag, namespace
u64, then exact UTF-8 bytes; validate nonzero symbols/IDs and kind tags. Inline keys
emitted by this tree are at most 512 bytes; longer legal key/fence inputs are
streamed into reviewed overflow descriptors (up to the existing 8 MiB total key
cap). This is a physical representation threshold, not a new input rejection.
Fixed-size directory/membership keys remain inline. Leaf values are compact
references/metadata, not whole properties; values over the reviewed compact cell
bound must use the record/payload reference representation. No hidden user-value
limit follows from this physical bound.

Read validates complete page framing and all local keys/values in strict order
before routing; child level must be parent level minus one, and each visited page
must obey the ancestor's lower-inclusive/upper-exclusive bounds. Explicit depth
cap is 64; cycles therefore cannot loop. A separate bounded, cancellation-aware
whole-tree verifier checks every child, range fence and record reference for
admission/reopen qualification. A successful point read validates its visited
path, not unvisited arbitrary media; no silent empty result on a visited error.
Later ZE-45 binds these components to the admitted coherent lease. Cursor errors
latch, with no continuation after corrupt/missing data.

COW update saves only an ancestor path. Copy one leaf's cells into bounded scratch,
insert/replace/delete, then emit one replacement leaf or two byte-balanced split
leaves. The right leaf's exact first key is the promoted exclusive-upper separator.
For a branch split, promote the left partition's last upper key and replace that
left page's last child bound with explicit infinity. Propagate only replacement
children to ancestors; share every untouched page/record PhysicalRef. All pages
are immutable, including prior private candidates. Empty leaves/branches disappear
from the candidate; a single-child root collapses. Sparse nonempty pages may
remain until later consolidation; no foreground whole-tree rebalance/copy.
Any failure leaves original roots usable and returns no publishable candidate;
the sink retains its explicit private abort inventory.

Range cursor retains its exact root and a bounded ancestor stack, never mutable
sibling links. It seeks a lower bound and advances by parent-child order. Local
page validation plus ancestor range checks precede yielding any row. Cursor and
scratch byte accounting is explicit; byte comparisons poll at most every 64 KiB.

Required incremental RED/GREEN: full-u128 ordering and isolation; leaf/branch
splits and reopened framed objects; one-path property replacement sharing;
range parity with a primitive ordered-map oracle; deletion/root collapse;
label/type tuple order; retained key fences after all live directories empty;
overflow exact/unequal-last-byte/chunk corruption/cancellation; malformed local
order and repaired-checksum wrong child bounds, excessive depth and cursor
latching. PG8 will carry independent primitive map/fence checks and actual directed
source/sink faults with same-seed controls. Necessary behavioral mutants restore
exact bytes; broad qualification stays in ZE-118.

Reference inspection: local redb is exactly
9e8302b17877fd315e726361dfaa5d9cdd9199cc. Read the cited separator/checksum,
leaf/branch split, immutable leaf replacement, delete/root-collapse, and ancestor
cursor source blocks. Our implementation uses the already frozen Zeppelin page
format, opposite exclusive-upper separator convention, checked errors and explicit
budgets. No redb dependency or platform/throughput result is implied.


## Review incorporated

Independent review: /tmp/ze43-tree-review/review.md. Deleting an empty final
child explicitly retags its preceding surviving child infinity while retaining
its ancestor bounds. Codec-valid inline keys larger than 512 bytes remain valid
on read; COW/split promotion normalizes them into the reviewed overflow
representation rather than rejecting them. Add near-page-size legacy-inline,
multi-level last-child delete/reinsert and mixed-generation sharing controls.
Page generation must match its artifact and may predate the admitted view;
future pages reject. The depth cap counts the root and rejects excess root growth.
