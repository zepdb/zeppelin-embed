# ZE124 main integration

Candidate06de2eb1942a75917a3f20ce4c7eb988238e461e was cherry-picked
onto mainfadef2cb52d0dddf262bd8503102739da6f3f427. Independent initial
and final reviews are retained here. Allocation-feature test typo was fixed
and independently rerun successfully before integration; production codec and
merge match both reviewed snapshots.

All59 candidate paths were checked.54 are byte-identical;5 shared runner/module
files equal the complete previous main plus exact candidate additions. The
source-audit.json records every hash and the merge basis. No shared suffix was
lost. Existing local changes remain outside the commit.

Main focused nextest:28 core/artifact/allocation tests plus4 oracle/PG12/PG13
runner checks pass. The named real runner episodes retain all earlier modules,
run59 operations and record zero violations. Strict core all-target Clippy with
test-support,allocation-audit and changed-source whitespace checks pass.
Commands/results are in core-nextest.log,runner-nextest.log,clippy.log.

Worker evidence retains six runtime source-mutant failures with exact restore,
literal base/delta binary goldens and2,794,685 parser/merge fuzz iterations in61s.
No repeated broad campaign was needed for this integration. Format tags13/14
are append-only; ZE43 owns11/12 and will merge additively when integrated.

This is the allocation-free codec/merge kernel. Real paired OUT/IN directory
publication, endpoint liveness, source identity/control/accounting, reopen and
recovery remain ZE44/45/47 acceptance. No product integration or performance
claim follows. Broad workspace/adversarial/per-crate coverage remainsZE118.
