# ZE-125 main integration

Source `05a49fa36570926135502842c5aeff92aadd388c` was individually
cherry-picked onto main parent `e015b3018326ddec0210d90859444321abd87838`.
All54 candidate paths were audited:49 remain byte-exact; five shared runner,
registry and oracle module files preserve complete main content plus exact
candidate additions. Shared-file conflicts were resolved from complete source
revisions. No production change was required during integration.

The independent review and signed primitive-oracle probe are included here.
Review found an incorrect signed sort expectation, which was reproduced with
literal mixed-signed input and fixed in the candidate before integration.
Owner mutation, allocation sweep and RED evidence remain in the parent
evidence directory; these are distinguished from root's final main checks.

Final main:37 focused nextest tests pass (32 core/runtime,3 PG9/PG14 runner,
2 independent oracle); one OperatorFactory lifetime compile-fail doctest
passes; scoped allocation-audit Clippy passes with warnings denied. PG14
reports six cases, three actual fault fires and three same-seed controls for
each of four seeds; the actual runner has59 operations and zero violations.
The first combined runner invocation selected only the adversarial binary
because --test restricted targets; the oracle was then run separately, with
its two tests actually selected. Commands, counts, host and raw log hashes
are in verification.json. Default nextest uses four isolated processes,
zero retries and one libtest test per process.

All45 inherited-file current hashes match the prior ZE73 integration
snapshot. Earlier independently concurrent AGENTS/wayfinder changes are
already represented in that baseline; no inherited path was staged here.

ZE51 retains real evaluator/pattern composition and ZE64 real ranking;
ZE53 retains completed producer/lifecycle acceptance. This proves relational
components and retained runtime composition, not public GraphStore/TCK or
recovery. Broad workspace/adversarial/per-crate coverage stays on ZE118.
