# ZE-43 bounded tree proposal review

Read-only review on 2026-09-19. Frozen inputs and SHA-256 values are in
`hashes.json`. No worktree edits, builds, tests, tracker changes or new ticket
claim. Review covers tree algorithm, checked source/sink, budgets and proposed
proof cases; record/overflow payload tags remain a separate review.

## Verdict

The exclusive-upper split convention, immutable ancestor-path replacement,
level/bounds validation, latched cursor errors and separate whole-tree verifier
fit the existing storage contract and frozen ZE-42 framing. No reason to change
page/PhysicalRef/TreeKind tags or the accepted request bounds. Two details should
be explicit before implementation; these are proposal gaps, not observed product
bugs.

## Required clarifications and focused tests

1. Proposal lines 58-59: deleting an empty final child must retag the preceding
   surviving child with explicit infinity before encoding the replacement branch.
   Merely removing the final cell leaves a finite last upper bound, which frozen
   tree.rs validate_cell rejects. Preserve inherited ancestor bounds when that
   local infinity is installed. Test delete-last-leaf/subtree at multiple levels,
   then lookup/insert above the prior finite separator, range parity, collapse and
   unchanged old-root reads. Deleting middle/first children should also preserve
   the routing interval; sparse nonempty non-root branches retain their level.

2. Proposal lines 34-36: 512 bytes is an emission policy, whereas frozen page-v1
   accepts larger inline KeyFences. State that decoding does not reject otherwise
   valid >512-byte inline keys. For COW/promotion, normalize these keys into the
   reviewed overflow representation before sizing/splitting or establish another
   safe algorithm. An empty-value leaf can frame an inline key of 16,292 bytes
   (64 header + 8 slot + 20 key/leaf framing), but promoting that same inline key
   into a branch with a final infinity child requires 16,464 bytes and cannot fit
   a 16 KiB page. A write must not introduce a hidden 512-byte logical input cap or
   falsely classify existing codec-valid inline data as corruption. Pin 512/513,
   a codec-valid near-page-sized inline key, and subsequent split/replacement.
   The existing 8 MiB key bound counts entity/namespace prefix; full request
   staging remains separately charged and may reject a smaller usable key.

## Additional integration assertions

- Make generation comparison precise: unchanged COW children may predate the
  current view. Check page creation generation against its containing artifact;
  reject future generations relative to the view/parent construction authority,
  without requiring every shared page to equal the current target generation.
  A new parent plus an older untouched child is a necessary positive control.
- The checked resolved-block capability must retain immutable bytes and validated
  store/artifact/reference identity; actual consumers compare the requested ref.
  Include a source returning another completely valid same-kind block and a
  sink failing after earlier private blocks were created. Neither grants cleanup
  authority over existing published bytes. Abort inventory remains explicit.
- Charge complete scratch capacity, path/row descriptor capacity and all source
  pins/sink retained objects concurrently. Preparation's 32 MiB is inside writer
  and shared store limits; read cursor pins/scratch belong inside query quota.
  Include exact/one-over reservation, fallible allocation failure and exhaustion
  after a split has emitted a private child, followed by original-root reads.
- Max depth 64 must include the root and reject root growth beyond that bound.
  Level decrement rejects cycles immediately, while work checks bound repeated
  DAG references and huge verifier traversals. Whole-tree verifier must follow
  overflow streams and record references, not only validate their descriptors.
- Required tests already cover the important corruption boundary. Add explicit
  duplicate/equal separators, first/last seek bounds, empty tree, future-page
  generation and exact source substitution. Byte-poll evidence should include a
  long common key prefix and split UTF-8 sequence across overflow chunks.

The proposed focused ordered-map, sharing, fault-fire and same-seed controls are
suitable for the component commit. They do not replace ZE-118 broad qualification,
ZE-45 coherent lease admission, record-schema review or later GraphStore reopen.
