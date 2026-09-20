# ZE-141 Final Spec Review

Fixed refs, the exact two-commit three-dot diff, and the nonempty/diff-check gates were confirmed. Three evidence gaps remain:

- **MEDIUM — expired-deadline cleanup still does not exercise a registered owner.** The governing matrix requires an expired deadline to “drop already registered owner” (`/tmp/ze-141-astra-conversion-plan.md:185`). The only deadline case creates an already-expired control and expects `RuntimeContext::new` itself to fail (`crates/zeppelin-embed-ffi/src/graph_result/conversion/tests.rs:1993-2018`), before native preparation or registry insertion. It therefore proves no registered-owner cleanup or refund.

- **MEDIUM — copied-byte mutation still does not prove the required C-work-refusal assertion can fire.** The matrix requires removing one C chunk charge to fail both the known-byte-delta and C-work-refusal assertions (`/tmp/ze-141-astra-conversion-plan.md:225`). Mutation 09 now selects both tests (`tasks/evidence/ze-141/run-mutations.py:177-198`), but its cleanup test aborts first on the changed empty-path checkpoint count (`tasks/evidence/ze-141/mutation-09-copied-byte-charge-red.log:50-56`; assertion at `crates/zeppelin-embed-ffi/src/graph_result/conversion/tests.rs:1684`). It never reaches the intended C-stage work-refusal assertions at `crates/zeppelin-embed-ffi/src/graph_result/conversion/tests.rs:1876-1890`, so that mandatory control remains unproved.

- **MEDIUM — the required present-empty vector rejection case is absent.** The all-pools requirement explicitly calls for separate `Some(empty vector)` Shape-rejection evidence (`/tmp/ze-141-astra-conversion-plan.md:175`). The malformed-input table tests a bad string span, list cycle, invalid cell, committed outcome, and report contradiction (`crates/zeppelin-embed-ffi/src/graph_result/conversion/tests.rs:999-1085`); its vector fixtures are nonempty (`:353`, `:1293`).

The prior source/native memory-pair and typed-error/context gaps are otherwise resolved. No scope creep or confirmed production-mapping defect was found; downstream public ABI, coordinator, TCK, and broad qualification remain properly excluded.
