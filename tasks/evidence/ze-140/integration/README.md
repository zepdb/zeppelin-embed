# ZE-140 main integration

ZE-140 source commits `18457d5934a04e81abee7731fee078b3edf5ece7`,
`cb491c51677a2469a1136ec2c024a2e09a13ee28`,
`bde36aa86906f3e39b59e2e50ae19fc307de0f7b`, and
`992f9502e3eb0d943ed5a4d75765f72da8812a7c` were independently reviewed and
cherry-picked onto main as `3ef7ccb`, `f72225b`, `f015573`, and `ca21b94`.

Final reviews were clean:

- Standards: `/tmp/ze-140-standards-review-final3.md`, SHA256
  `1628580db0c03ef12372d4d3de734c8f93fce2b614d4a7818c9668a2a781d804`.
- Spec: `/tmp/ze-140-spec-review-final3.md`, SHA256
  `b50b7cea6b1c884e883eaad69ed2577b382eef57962fd633a95616369a61c7ad`.

The additive PG21 merge preserved ZE-135 query storage and ZE-141 PG16 native
conversion registration. Exact stale-count REDs were observed after the source
cherry-pick: graph-only reported 200 actual versus 186 expected in run
`81bad88b-e28f-4304-ae2f-4488668dbd09`; graph-result-test-support reported 220
actual versus 206 expected in run `d7a84423-db6a-46f5-8a8e-cbaf2a98a262`.
The only registry correction is to the exact combined totals 200 and 220.

Focused main-source GREEN:

- core mutation plan: 27/27, run
  `fec14301-9e24-4b8a-ba31-1adc095d81ba`;
- compiler/binder/runtime surface: 69/69, run
  `751a48ac-02d5-4068-8b36-a80854a9041c`;
- graph adversarial/PG21/graph-only registry: 7/7, run
  `40d804ff-0c0c-43f1-b164-90e5327b6dcc`;
- hook registry controls: 2/2, run
  `e7b5a7e8-c7cd-4d9b-bfac-23aa1a723678`;
- no-graph registry control: 1/1, run
  `56596302-79ba-444b-968d-6f9831a84469`.

Raw output is retained beside this file. These compiler-plan checks perform no
physical mutation. Writer/base-view admission, eager drain, staging,
revision/fence outcomes, commit/reopen, public ABI/TCK, platform/release, and
broad workspace/coverage/sanitizer/size qualification remain with their owning
tickets and ZE-118.
