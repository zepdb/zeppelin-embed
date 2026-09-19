# ZE126 bounded independent core-adapter review

Root reviewed the four frozen source paths against immutable imported ZE125
base, all four SHA256 values verified against source-hashes.json (manifest
SHA256 aa241cea33ba700d007024ab46533ba7f5be8621e81d743306fc484969583f24).
No concrete blocker found in these two additive adapters. This is independent
source/evidence review; terminal runs remain contributor evidence, not a fresh
root rerun. Full frontend lower/admit/execute acceptance remains pending.

QueryArena<NodeFacts>::validate_plan obtains both full backing capacity and
charged QueryMemory identity from its actual Vec/reservation internals. Existing
validate_with_fact_vec computes its actual region then delegates through a
mutable slice; it cannot grow/reallocate the Vec. The returned fact loan and
capability each borrow the mutable arena for the declared facts lifetime;
private span construction and invariant fields prevent numeric prepayment from
creating this certificate. Existing QueryInputs admission checks owner identity
and rejects foreign-memory charge claims. Validator scratch and all other plan
backing remain separately reserved by the caller. The credit test deliberately
compares the same facts with conservative plan_facts rather than assuming a
specific allocator capacity; the borrow doctest retains the certificate after
dropping the plan and rejects mutation of the owner.

execute_in directly invokes the existing drain with the supplied actual
RuntimeContext; no new context, work limit, counter, value account, view or lease
is created. Existing owned-view execute/execute_factory paths are unchanged.
The shared drain validates query-memory identity/source root, charges real
buffers, runs eager preparation and completion, and enforces final close-first
check with private output dropped on error. The borrowed adapter clearly leaves
actual view retention/charge/release to its caller; the focused test uses a real
SnapshotLease and charged output then observes real close/cancel cleanup.

Inspected evidence covers initial missing API RED, actual fact credit omission
and value-account-reset mutants with runtime RED100, exact restoration,
23 terminal focused tests, lifetime rejection and scoped strict lint/format.
The compiler owner must still prove one actual compile_read_in callback carries
this same context through copied plan validation, QueryInputs admission and
execute_in. Original native graph/operator/public TCK gates are unchanged.
