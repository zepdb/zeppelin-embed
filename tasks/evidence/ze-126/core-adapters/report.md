# ZE-126 core adapter handoff

Root delegated this implementation after the initial independent review.
The reviewer therefore no longer independently reviews these four changed
files; root reviews them. The initial review artifact is
/tmp/ze126-interface-independent-review.md. No complete lowerer, actual native
operator composition, public execution or TCK acceptance is claimed.

Allowlist in WT126: resources/inputs.rs, runtime/driver.rs, runtime.rs and
new tests/graph_compiled_context.rs under crates/zeppelin-embed. No other worker
files were edited, no tracker changes, no staging and no separate commit.
Root/ZE126 owner incorporates these files into the single ticket commit.

QueryArena<NodeFacts>::validate_plan borrows the actual mutable arena, validates
its initialized facts through existing GraphPlan::validate_with_fact_vec, and
returns (GraphPlan, RetainedAllocation). The token derives full actual capacity
and query-memory identity solely from arena/charge internals. Both loans retain
the arena; no caller numeric region/prepayment can provide this authority.
Existing plan_facts remains conservative for externally owned Vec inputs.

execute_in borrows the existing RuntimeContext and directly calls the unchanged
shared drain. It never creates/reset a ValueContext, work counters, view, budget
or limits. Existing execute/execute_factory implementations remain unchanged.
The caller owns and charges the actual view around this borrowed scoped call;
owned-view entrypoint release semantics are unchanged. The new signature uses
the existing execution-bound PullOperator lifetimes and Completion contract.

Named missing API tests first failed compiler RED101 (01). Four directed tests
then pass (03). They prove exact fact backing credit compared with uncredited
plan_facts; wrong QueryMemory rejection/release; nonzero prior + validation +
pull/completion value work and prior operator counters; actual value-work
exhaustion at 8,000,000 with no context reset; and actual retained SnapshotLease
close preceding simultaneous caller cancel after completion, dropping a real
charged completed allocation with no output prefix. The caller drops the view
after execute_in returns and the actual close thread then completes.

All mutation controls were confined to /tmp/ze-126-core-adapter-scratch, built
from immutable imported base beb1a1e44730b56494ec2cb4aafc5d9ea5419763 plus the four
owned files and context.rs from immutable /tmp/ze-126-interface-review-1. WT126
never contained mutants while the frontend owner built. Removing charged_owner
from the actual fact capability fails runtime RED100 (05). Replacing the
existing value context with a fresh one before drain makes both cumulative
work/exhaustion tests fail runtime RED100 (06). Both files restored by exact
hash before terminal checks. The final doc-only addition is the explicit
certificate borrow rejection.

Terminal frozen-core checks:

    cargo nextest run -p zeppelin-embed --features allocation-audit \
      --test graph_compiled_context --test graph_query_runtime \
      --test graph_query_runtime_control --success-output final

23/23 pass (07): four new adapter tests plus 19 existing runtime/ownership/
eager/close-first controls. Original GAT/stateless behavior remains exercised.

    cargo test -p zeppelin-embed --doc validate_plan -- --show-output

One intended lifetime rejection passes (08): E0499 specifically prevents
mutating the actual fact arena while the returned capability remains live,
even when the returned GraphPlan was immediately dropped.

    cargo clippy -p zeppelin-embed --features allocation-audit --lib \
      --test graph_compiled_context -- -D warnings

Strict scoped lint passes (09); all four source files pass rustfmt check (10).
Final four source hashes match isolated terminal-tested files byte-for-byte
(source-hashes.json). Frozen files and adapters.patch are included for review.

Frontend owner still owns the controlled compile_read_in -> QueryInputs ->
execute_in tracer and exact sealed ReadContext/control/error mapping. That
later trace must retain prior + lowering/validation + execution work in one
account. These core tests don't substitute for it. Nonessential broad campaigns
remain ZE118; no new suite or coverage qualification is claimed here.
