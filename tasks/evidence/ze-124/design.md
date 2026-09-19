# ZE-124 adjacency kernel format and API review

Candidate design, based on main 7028a42f2f4898bb7aa729dc935bd51b4033f2ba.
This independent kernel consumes already admitted immutable block payloads. It
neither resolves PhysicalRef nor claims native view/publication acceptance.

## Version 1 bytes

Both inner payloads have a 96-byte header, followed immediately by entries.
All integer fields are little endian. There is no implicit alignment or padding.
Outer artifact framing supplies the checksum; this module never invokes an
uncontrolled outer decoder. Exact length is 96 + count * entry width.

| Offset | Width | Field |
|---|---:|---|
| 0 | 4 | ASCII ZADJ |
| 4 | 2 | inner version = 1 |
| 6 | 2 | kind = 13 base, 14 delta |
| 8 | 16 | nonzero bound NodeId |
| 24 | 8 | nonzero RelTypeId |
| 32 | 16 | nonzero inclusive lower RelId |
| 48 | 16 | exclusive upper RelId; zero only with infinity tag |
| 64 | 8 | base consolidation watermark or delta WAL commit sequence |
| 72 | 4 | entry count |
| 76 | 1 | direction: 1 OUT, 2 IN |
| 77 | 1 | upper tag: 0 finite, 1 infinity |
| 78 | 18 | required zero reserved bytes |

Base entries are exactly 32 bytes: nonzero RelId then nonzero other NodeId.
Delta entries are exactly 40 bytes: the same IDs, action (1 insert, 2 delete),
then seven required zero bytes. Zero entries are legal; zero-width/reversed
ranges are not. Infinity carries zero upper bits and includes u128::MAX.
No inner checksum duplicates the already authenticated immutable outer frame.
Unknown version/kind/direction/action/tag, reserved bytes, zero IDs, trailing or
truncated bytes, count/length errors, numeric ordering/uniqueness, range mismatch,
and sequence violations are typed errors.

Outer tags 13/14 are appended to BlockKind and both artifact/WAL decoders.
Reserved ZE-43 tags 11/12 remain untouched. No PayloadRef role is widened.

## Compiled seam to build and review

Public strong data: Direction::{Out,In}, UpperBound::{Exclusive(RelId),Infinity},
RangeKey { node, rel_type, direction, lower, upper }, Edge { rel, neighbor },
DeltaEntry { edge, action }, and Action::{Insert,Delete}. Range validation occurs
at every external codec/merge boundary; no invalid public struct is trusted.

encode_base(key, watermark, edges, output_bytes, checkpoint) and
encode_delta(key, sequence, entries, output_bytes, checkpoint) return the exact
written borrowed byte prefix only after validation and a final checkpoint.
Output may have extra capacity; decoded input must have exact length.

merge(key, watermark, cutoff, base_bytes, delta_byte_slices, output_edges,
checkpoint) returns Merged containing only the written borrowed Edge prefix,
up to two nonempty BasePartition values and the cutoff as output watermark.
A BasePartition carries the new RangeKey and an index range into that prefix.
Caller-owned Edge scratch is initialized by its caller; the kernel needs no
allocator, MaybeUninit/unsafe conversion, recursive cursor or hidden backing.
The caller can encode each partition with encode_base. No row callback exists.
All error returns invalidate all private output scratch.

The kernel first validates every byte/entry of every input, exact group identity,
base watermark, watermark <= cutoff, watermark < run sequence <= cutoff, and
nondecreasing run sequences. Then a bounded nine-cursor pass validates cross-run
neighbor consistency at every sequence and equal-sequence action consistency,
and determines output size/split. Only then does the second merge pass copy
surviving edges into private scratch. Final control runs before returning a
valid output borrow. Every RelId compares all 128 bits; latest sequence wins,
deletes suppress output, and exact same-sequence duplicates coalesce. Different
neighbors remain corruption even when a later delete hides the edge.

A nonallocating Error<E> distinguishes Format(FormatIssue), Limit(LimitIssue)
and Control(E), retaining the caller's typed error without formatting. Work
checkpoints identify HeaderBytes, EntryBytes, Compare, CopyBytes and Finish,
with the actual bounded byte amount or one comparison. Poll each actual entry,
comparison and copy; no indivisible byte operation exceeds 96 bytes. Control
errors never turn into missing edges. The real adapter preserves close-first
semantics and maps these work units to its authentic counters/resources.

## Threshold and partition behavior

MAX_BASE_ENTRIES = 4096, MAX_DELTA_RUNS = 8, MAX_PENDING_ENTRIES = 2048.
A checked append_admission(existing_runs, pending_entries, incoming_entries)
returns Append or ConsolidateFirst before a ninth run/2049th pending entry.
An incoming batch larger than 2048 is a typed limit, requiring caller spatial range
partitioning or bounded-base preparation under its original atomic publication; no silent truncation or
budget relaxation. A zero-entry property-only update returns NoChange and
creates no new delta. Invalid existing counts remain errors.

merge rejects a supplied over-limit group; callers consolidate the currently
admitted group before appending. Maximum merged output is 6144 edges and thus
at most two bases. Split at the actual RelId of edge 4097: first upper equals
second lower, preserving the original outer interval endpoints. An empty
merged group returns zero partitions. No MAX+1, overlap or degree limit.
Storage's real directory owner handles neighboring empty intervals and routing.

## Proof and ownership boundaries

Focused tests cover exact byte goldens, all truncations/reserved geometry,
high IDs/infinity, sparse/empty groups, mixed changes, all-sequence topology,
equal sequence, 4097 splitting, 8/9 and 2048/2049 admission, small output,
late corruption, every checkpoint refusal including final completion, real
allocator counts and an independent BTreeMap edge oracle. Directed faulty
merge/delete and missing-reverse comparator controls accompany same-seed clean
controls in the seeded runner. Synthetic OUT/IN arrays prove comparator ability
only; ZE-44 remains responsible for real authoritative OUT/IN publication,
endpoint liveness and self/parallel consistency. ZE-45/47 retain leases/reopen.

No new dependency. Storage32MiB/writer64MiB/shared256MiB are unchanged; callers
account their real input/output capacity and the fixed kernel state. This
component cannot claim actual adapter resource admission before ZE-44. Broad
nonessential qualification is tracked in ZE-118.

Storage owner and root reviewed version-one geometry on 2026-09-19. A per-run
limit is never a product batch or degree cap; ZE-44 preserves one atomic publication.
