# ZE-48 correction review

Read-only snapshot pinned by hashes.json; every source remained unchanged during
its individual copy. No source edits, Cargo builds, new probes, tests or tracker
changes were performed by this review. Existing red/green logs were read.

No further concrete product finding in the corrected three invariants.

- Accounting: required sorted/disjoint owner-declared retained ranges are summed
  once, and each visible descriptor/text span is checked for interval coverage.
  Address+capacity and aggregate byte arithmetic are checked; zero-size visible
  spans require no region; inventory zero ranges/overlap/unsorted ranges reject.
  Inventory full capacity and fixed validator scratch are charged separately, and
  retained ranges cannot overlap the inventory. Adjacent ranges can cover a span
  while gaps reject. Region loops, binary search and interval traversal poll work
  and cancellation. Actual hidden allocator capacity/ownership remains a truthful
  owner assertion, explicitly reserved for ZE49; this is not allocator proof.
- Relationship origins: the immutable validated DAG supplies all inherited
  origins. Both join inputs, direct slot renames and group-key forwarding are
  traversed; a lookup with no origin no longer erases the other dependency.
  Different origins in one pattern reject while the same inherited origin is
  accepted. The fixed pending stack is limited to64 frames with checked pushes;
  validated input DAG depth bounds pending branches, and repeated shared subgraphs
  are bounded by mandatory work/cancellation checks rather than skipped.
- Nullability: inner joins narrow compatible nonnull common kinds; optional joins
  preserve left-side kinds. Numeric equality keeps the left numeric representation
  without conflating canonical replay tags. Incompatible nonnull kinds still reject.

Original three named tests have recorded intended RED and terminal16-test GREEN
logs in /tmp/ze-48-evidence. This review does not claim that source is fully
qualified: the owner still plans targeted malformed-proof/unused-capacity, rename/
multiple-pattern/lineage-depth and cancellation controls, PG6 and scoped lint.
The current test name mentioning slot renames does not yet contain a rename case;
that pending control is already identified in the handoff. Broad qualification
is deferred by user direction. No full-suite or coverage acceptance is implied.
