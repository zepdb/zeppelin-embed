# ZE-141 main integration

ZE-141 source commits `65fbbbe853a89ca9e1342bb2be0182b48531fac2`,
`a28a091e030ee8c1f18c6abd28dec088500e7b01`, and
`5634a4c4f6883862177661c9cb7fc2e1afb7c0d1` were independently reviewed and
cherry-picked onto main as `49cff7f`, `ddef7ef`, and `72e95b8`.

Final review reports were clean:

- Standards: `/tmp/ze-141-standards-review-final2.md`, SHA256
  `451e4397c34429ebc4b44f47b9dd1bd4552309d9d2956996b5b533643f406d76`.
- Spec: `/tmp/ze-141-spec-review-final2.md`, SHA256
  `fbeb87c1e31cd7032991859df7df20f0c1a55b64035cbfb327a5ef3e31a19fa3`.

The graph-only registry remained exactly 186. The first integrated
`graph-result-test-support` registry check produced the intended stale-count RED:
206 actual versus 198 expected, nextest run
`1a7e4fac-bc7a-4996-9bbc-74ea2104e967`. Updating only the exact additive total
to 206 made graph-only run `c601016d-60c2-482f-839b-b18c924c5558` and hook run
`df39392b-1ce4-447c-a293-e23bec05af1d` pass 1/1.

Focused main-source GREEN:

- native conversion, graph-only: 8/8, run
  `7deb07d1-e53d-41f5-87dc-8404dd2ca5a1`;
- native conversion, hook-enabled: 8/8, run
  `2fae26f9-0790-490c-bc9f-504a5aef5855`;
- PG16 native direct and real runner: 2/2, run
  `c080bee9-c37d-4bd2-bf50-d51bbb2ecc93`;
- independent response oracle: 1/1, run
  `55009d4f-056f-467b-b6a7-92d6c424c93c`.

Raw command output is retained beside this file. Broad workspace, coverage,
sanitisers, release packaging, public coordinator/ABI, TCK, and platform
qualification remain outside this component and are retained by ZE-118 and the
original downstream tickets.
