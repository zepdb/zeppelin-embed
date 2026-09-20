# ZE-44 predecessor extension independent review

Verdict: PASS after the empty-root probe validation correction. This is a bounded predecessor/tree review, not acceptance of the whole adjacency producer or public read view.

Frozen original directory source SHA-256: `6b3cee9d5b585cf0a2f5daf4128579d01d6264247e6db2b17ab438f26b8ddab7`. Corrected source SHA-256: `b63cb2aa5835792b0e0fd2cf6a3bdd76473818127f6283efb9dceca890093549`. Root reviewed the full seek/retreat/descend-right/validate-path flow in an isolated archive of committed base0e0064d with only frozen source/test overlays. The active worktree was never mutated by this review.

The original implementation correctly preserves numeric key ordering, full ancestry, original containing-leaf generation, and source-borrowed entry lifetimes. Returning a borrowed entry drops only cursor-owned reservation; source-owned immutable backing remains borrowed. The previous subtree is descended along its rightmost path, and the complete modified ancestry is revalidated before returning any row. No previous directory population scan or write occurs.

One observed bug: `lookup_predecessor` accepted an empty malformed key on an empty Nodes root, while existing `lookup_entry` correctly rejected it. The independent named test failed with exit100 (`root-independent-initial.log`). The owner moved existing lower-key validation before the empty-cursor return and reservation, with the same one validation on populated roots and the control checkpoint still first. The corrected source passes the original independent repro and the owner's named empty-root check. The full focused directory regression covers the changed forward seek as well.

Independent directed checks prove a two-level tree floor lookup crosses ancestor/subtree boundaries correctly, including probes before the first valid node and at u128::MAX; no entry loses its actual leaf generation. Work limits from0through1,000,000 include both actual Work refusals and clean success, with cursor reservation released on every result. A missing predecessor leaf produces Missing even though an unrelated exact leaf remains readable. Cancellation fired by the source while reading the previous leaf returns Control with no partial entry and no retained cursor reservation.

The first cross-subtree fixture incorrectly used numeric NodeId0, which the unchanged key validator rejects. That fixture-only mistake is retained in the initial log and was corrected to valid NodeId1; it is not a product defect. The corrected independent routing/control checks pass.

A deliberate scratch-only mutation removed only the original-parent generation check in validate_path. The existing selected/previous-path test then failed with exit100, specifically on the previous path. Restoring the exact source hash returned GREEN. No mutation remains. `root-parent-generation-control.json` records exact commands and restored hash.

Terminal corrected-source verification: six focused predecessor/independent checks pass, followed by all34tests in the focused graph_directories binary, required because the correction changes existing DirectoryCursor::seek. Exact nextest commands/results/raw logs are adjacent. Tests use separate nextest processes, four concurrent tests, no retries. No full workspace/adversarial/coverage campaign ran.

`root-independent-tests.patch` contains only the root's four directed tests and their tiny hand-built tree fixture, relative to the frozen owner test file. The owner can retain these concrete regressions without copying over later edits. `root-independent-tests.rs` is the exact tested scratch file; tests are review artifacts, not claims of actual native adjacency, storage admission, publication or lease behavior. Whole ZE44 acceptance remains open.
