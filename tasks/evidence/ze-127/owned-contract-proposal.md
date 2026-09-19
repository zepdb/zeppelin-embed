# ZE-127 / PG15 native completed storage interface

Base main `0fff7f4d92843c207251a152b4dff26da3198966`. This is the compiled
component seam. It does not establish real graph-read integration acceptance.

The owner stores fixed-capacity typed arrays for values, child indices, UTF-8
bytes, copied nodes/relationships, properties, label names, column descriptors,
root cell indices, search reports and write receipts. Scalars retain exact bits;
all entities use native strong IDs and complete revision/key/generation metadata.
List tags preserve heterogeneous query lists and stored scalar/EmptyList tags.
Value references use postorder: every list child precedes its parent. Copied
entity pools are strictly sorted by full native ID; cells retain independent
row order and bag duplicates.
Strings/labels/keys use checked ranges into the one owned byte array. Records
have no view pointers, lazy lookups or recursive neighbors. Default node copies
carry no text/vector; explicitly selected text/vector pools retain presence.

The first compiled seam is a complete typed borrowed pool description plus
metadata, validated and copied into genuine QueryArena owners using the supplied
RuntimeContext. This is representation construction: producer input capacities
remain separately owned/charged by their real producer. ZE-53 owns conversion
from actual admitted base/allowed overlay entities and QueryValue rows; this
module must not manufacture GraphStore or treat supplied logical rows as proof
of real storage. Entity admission is explicit and typed: the producer supplies
the exact view token and returns deleted/missing/storage failure rather than
inventing empty records; foreign-view input rejects before copying.

Validation checks every range/index, UTF-8 span, row/column shape, list DAG and
depth/expanded descendant count, scalar property/list kinds, entity/key kinds,
label/property order uniqueness and report call identity/generation. Represented
root plus initialized pool lengths fit 4 MiB and rows fit 65536; full capacities,
control and validation scratch remain separately charged inside query/shared
limits. Each bounded validation/copy chunk checks the same close-first context.

PreparedGraphResult retains genuine QueryArena charges. A narrow sealed owned-
element detach permits only the native Copy/'static descriptor types, bytes and
indices. It consumes each arena into its existing Vec and releases its original
guard exactly once. CompletedGraphResult has no lifetime parameter and read-only
pool accessors. Detach allocates/copies no payload, performs no callback/control
check, and preserves pointers/capacities. Failure drops actual buffers before
their guards; no partial completed owner can be returned.

The existing runtime driver owns CompletedRows/CompletedBytes/CompletedAbiBytes
charges. The builder enforces its geometry limits and charges actual CopiedBytes
plus value/validation work, then supplies initialized represented bytes to the
driver's FrozenOutput. Final consuming detach accepts the final actual counters
and peak reservation measurement as fixed metadata, after driver admission.
Standalone/write integration must likewise designate one completion-counter
authority. No actual write outcome is inferred: owned metadata retains the
coordinator-supplied disposition/admitted/changed generation and item receipts.
Each Receipt preserves the authenticated original item ordinal and deleted flag
with the full staging ItemReceipt; complete original receipt order is validated.
The read-only prepared metadata accessor exposes stable rows, admitted generation
and outcome without invoking the source again. Its counter/peak fields remain
unfinished until final detach. Native-to-C conversion must read prepared pools
and metadata while every real source/native charge remains live. Detached slice
lengths are not complete backing-capacity credit or an adoption proof.

Reports keep distinct requested/actual route, original/quantized precision,
exact/approximate coverage, component/empty-leg state, document/query/tokenizer
epochs, normalization/rules versions, query-level alpha and per-call work.
Impossible kind/route/precision/requested-leg combinations reject before copy;
empty vector precision stays explicit producer state. ZE-64 remains the producer
of ranking truth; ZE-128 remains aligned C ownership.

Focused proofs use literal tables, malformed ranges/depth/counts, foreign/deleted
producer results, every actual allocation failure, bounded copy cancellation,
unchanged pointers/capacities under allocator-denied detach, and released real
accounting. Broad suites remain ZE-118; ZE-53/68 real-producer acceptance remains.
