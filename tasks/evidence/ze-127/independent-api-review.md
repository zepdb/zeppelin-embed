# ZE-127 frozen native owner review

Reviewer: /root/ze67_contracts, delegated by root under ZE-127. Read-only source
review; no product/tracker changes, new claim, agent, build or broad suite.
Snapshot: /tmp/ze-127-review-1. Manifest SHA-256:
fa8d476d78bcc5cfc09cc84d7c984fd9f4a1b5dd8cc5eeada41970bdf4cc22ed.
All eight file hashes verified exactly. The source contains the APIs described
below; I did not independently compile this partial snapshot. The source owner
reports it compiles; this review does not invent APIs absent from the snapshot.

Disposition: two concrete representation/validation corrections and one small
prepared-metadata access gap should be resolved before the final interface
freeze. The owner acknowledged the first two and is correcting them. No broad
integration acceptance is granted by this review.

## Concrete findings

1. **Owned receipts omit authenticated delete and item metadata.**
   completed.rs Pools.receipts, OwnedElement and both result owners reuse
   staging::ItemReceipt. The snapshot's literal fixtures show only entity,
   revision, generation and replayed. Fixed C ZeGraphReceipt additionally needs
   `item` and `deleted`. Array position only recovers the former under a declared
   complete original-order invariant; deleted cannot be reconstructed from those
   fields or absence from copied entity pools. An owned receipt wrapper should
   retain original item ordinal and authenticated deletion outcome alongside the
   actual ItemReceipt, with complete order validation. Source owner confirmed
   this change; it must preserve producer/coordinator authority, not infer delete.

2. **The report validator accepts impossible modality metadata.**
   completed/validate.rs:225-262 checks several cross-field rules but Lexical
   permits lexical_leg=NotRequested, and Vector permits actual_tier=None and
   precision=NotApplicable with vector_leg=Nonempty. Hybrid also permits absent
   actual vector route, and NoQueryMatches is accepted as a vector-leg reason.
   This contradicts the record meanings/fixed C provenance where absent actual
   vector tier is for lexical-only execution and NoQueryMatches is lexical-only.
   Require literal invalid fixtures and consistent mode/route/requested-leg/
   precision rules. Empty vector precision remains explicit producer state;
   do not invent Original from coverage or counts. Source owner confirmed fixes.

3. **C conversion before native detach needs prepared metadata access.**
   PreparedGraphResult exposes pools and represented_bytes but no metadata
   accessor; only CompletedGraphResult exposes metadata. The safe C overlap
   route is to convert from the prepared owner while every native QueryArena
   guard remains retained. Add read-only access to the prepared owner's stable
   copied row/admitted-generation/outcome metadata; do not re-invoke ResultSource
   or detach merely to learn it. Clearly distinguish unfinished pre-detach work/
   peak from the final driver-provided counters. Sent to owner for resolution.

## Ownership and geometry reviewed

- Value contains only scalars, exact F64 bits, strong-index references and tagged
  list spans. QueryValue/view pointers cannot enter the sealed Copy + 'static
  OwnedElement set. All twelve typed pools own actual QueryArena backing.
- copy_from binds the exact QueryView pointer, preserving ForeignView separately
  from Missing/Deleted/Storage producer failures. This is a producer contract;
  the synthetic fixture's no-op RetainedView is not real graph admission proof.
- Rows/columns/cells have checked geometry and a complete represented 4 MiB cap
  including CompletedGraphResult's descriptor. Values use postorder child
  references, bounded expanded descendant totals and depth; all children,
  entities, properties, names and vectors are validated even when unreferenced.
- Nodes/relationships are strictly ordered by full strong IDs. Properties/labels
  are byte-ordered and unique; UTF-8 checks, list traversal, comparisons and
  copying poll in bounded chunks. Relationship endpoints preserve original
  direction; full revision/key/generation and selected text/vector presence
  survive owned copying. Empty/NUL names and keys remain representable.
- The copied native vectors store original f32 bits, with finite validation;
  scalar F64 remains exact unrestricted bits. No C layout or alignment assumption
  is imported into core. Conversion into ZE-128 therefore requires real typed C
  construction, never casting native Value or Vec<u8> allocations.
- SearchReport has separate optional requested/actual route, precision, coverage,
  all three epochs, alpha bits, normalization/rules versions, component states,
  candidate/cross-score counts, fallback count and per-call WorkCounters. The
  storage shape can preserve these distinctions once finding 2 is corrected.
- QueryArena::detach_owned destructures the real Vec and charge, drops the
  authentic charge once and returns the same Vec. PreparedGraphResult consumes
  all twelve owners, then its outer control drops. CompletedGraphResult has no
  lifetime and exposes only immutable pools. There is no recursive graph/list
  destruction or arbitrary borrowed-type detach.
- The actual allocator fixtures deny each observed allocation ordinal, check
  before-guard backing cleanup, retained pointers and zero allocation attempts
  during detach. These are inspected assertions, not independently rerun results.

## Required real integration boundaries

ZE-68 should convert PreparedGraphResult pools while the native owner and all
source charges remain live. Detach is final application ownership transfer;
CompletedGraphResult exposes slice lengths but no whole-capacity/adoption proof.
It cannot be treated as a prepaid query allocation after its guards are released.
Any alternative path that re-admits detached backing needs an authentic complete
capacity owner/proof interface, not slice-length credits.

Both native and C preparation charge actual copying. Existing driver/coordinator
alone charges completed counters exactly once from measured representation sizes.
Final native metadata accepts the actual counters and measured peak, without
creating a new counter authority. ZE-53/68 must determine exact final ordering,
including counters affected by C preparation and C work/report serialization.

ZE-53 retains actual admitted base/overlay QueryValue/entity resolution,
missing/deleted/storage semantics, payload selection, limits and public execution.
ZE-64 remains authentic ranking/report production. ZE-68 retains full source/
core/C capacity overlap, aligned staging receipt adaptation, true commit-window
allocation denial and real coordinator outcome faults. ZE-69 retains public
marshalling/free/lifecycle and Busy/Poisoned wrapper policy. Actual graph close/
reopen and public ABI acceptance are not replaced by a lifetime-free struct or
allocator-denied component test. Broad qualification remains ZE-118.

## Narrow corrected-snapshot recheck (review 2)

Snapshot /tmp/ze-127-review-2, manifest SHA-256
0c62f71de1713ceb5c2a2275477e32bee2f48b99402d7695a4540ddedacb0372:
all eight hashes verified. All three findings above are cleared in the frozen
source. Receipt now owns item_index, deleted and the authentic ItemReceipt;
validation pins complete original ordinal order. Report validation rejects the
identified impossible lexical/vector/hybrid route/leg/precision combinations,
including lexical-only NoQueryMatches on a vector leg. PreparedGraphResult now
exposes metadata() documenting copied stable admission/outcome/rows and unfinished
counter/peak fields, allowing conversion without source reinvocation or detach.
Literal receipt and contradiction tests cover the changed cases in the snapshot.
The owner reports focused GREEN; I did not independently rerun this partial
snapshot. Prior real producer, capacity overlap, final counter sequencing,
commit/lifecycle and public-wrapper qualifications remain unchanged. No remaining
concrete blocker from this bounded representation review.
