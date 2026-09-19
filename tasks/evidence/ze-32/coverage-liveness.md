# Coverage-run liveness observation

The unchanged full-workspace coverage run (session 29949, test process 42643)
continued after its log went quiet in the last long-running test,
`crash_preset_fires_in_at_least_forty_percent_of_each_family`. This test runs
48 crash seeds across all 11 `CampaignKind::FEATURES` families.

Read-only check on the host recorded in the parent evidence:

```sh
/usr/bin/sample 42643 1 1 -file /tmp/ze-32-qualification/live-stack.txt
lsof -p 42643 -Fn
ps -p 42643 -o pid,etime,time,%cpu
```

The sampled active test thread was in
`run_graph_campaign_operation -> observe_on_store -> maintain_refinements
-> refine_graph_checkpointed -> advance_alpha -> robust_prune_rows`, with
scoring work below it. Across separate open-file observations its temporary
fixture directories changed from `.tmp2jkXx8`/`.tmp6MJtTV` to
`.tmp175vcs`/`.tmpiOsx5F` and later `.tmpBpT2yE`/`.tmpJFkNsu`, demonstrating
that the campaign continued to advance. The log had no failure marker at
these observations. `coverage-live-stack.txt.gz` retains the stack sample.

This is only liveness evidence. It neither proves terminal test success nor
replaces the required coverage report, and it makes no performance claim.
No product/test source, test count, gate, environment or live process was
changed or restarted. The first unqualified `sample` command resolved to a
broken Python helper and failed to import its module; the absolute system
tool above completed successfully. No bug or code correction was inferred
from that PATH issue.
