# ZE-132 main integration

Source candidate5c5c3487c0e143084828a09ee45b1242f9cf0d8d was applied to exact main51f657c60b2bfde991887643126e88e279a75063. All13 candidate paths are byte-identical; the five helpers now add a unique AtomicU64 invocation number to their existing process-specific directory prefixes. Their existing create/remove ownership, store lifecycle, allocation and semantic assertions are unchanged. The two single-directory targets remain unchanged.

Independent source review found no blocker. The counter needs uniqueness only, so Relaxed ordering is sufficient; this test-only helper introduces no lock, dependency or production behavior.

Main focused verification passed: the five affected targets passed31/31 with four same-process libtest threads, and31/31 with normal nextest-j4 process isolation. Strict scoped Clippy --no-deps, targeted rustfmt and git diff --cached --check passed. Commands are identical to the candidate report's named five-target checks. Original RED101 (three AlreadyExists failures) remains in the candidate raw evidence.

All45 inherited files matched their existing hashes. No push or broad workspace/adversarial/coverage/fuzz/size campaign was performed. Unchanged dependency-wide Clippy warnings are recorded in the source report; this integration verifies changed test targets only.

Exact raw output is preserved in raw-logs.tar.gz. Display logs remove only trailing blank lines so the repository whitespace gate can pass; the first commit preflight stopped on that log-only EOF whitespace before committing.
