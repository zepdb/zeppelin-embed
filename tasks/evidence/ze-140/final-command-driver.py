#!/usr/bin/env python3
"""Run ZE-140's exact focused final commands and retain unabridged receipts."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
from typing import Any


ROOT = Path(__file__).resolve().parents[3]
RAW_LOG = Path(__file__).with_name("final-command-raw.log")
RECEIPTS = Path(__file__).with_name("final-command-receipts.json")

OWNED_RUST_PATHS = [
    "crates/zeppelin-embed-cypher/src/lib.rs",
    "crates/zeppelin-embed-cypher/src/lowering/mod.rs",
    "crates/zeppelin-embed-cypher/src/lowering/mutation.rs",
    "crates/zeppelin-embed-cypher/tests/binding.rs",
    "crates/zeppelin-embed-cypher/tests/lowering_allocation.rs",
    "crates/zeppelin-embed-cypher/tests/mutation_lowering.rs",
    "crates/zeppelin-embed-cypher/tests/runtime_lowering.rs",
    "crates/zeppelin-embed/src/property_graph/query/plan/mod.rs",
    "crates/zeppelin-embed/src/property_graph/query/plan/mutation.rs",
    "crates/zeppelin-embed/tests/graph_query_plan.rs",
    "tests/adversarial-oracle/src/graph_mutation_lowering.rs",
    "tests/adversarial-oracle/src/lib.rs",
    "tests/adversarial/coverage.rs",
    "tests/adversarial/graph_mutation_lowering.rs",
    "tests/adversarial/mod.rs",
    "tests/adversarial/runner.rs",
    "tests/adversarial_tests.rs",
]


def nextest(*arguments: str) -> list[str]:
    return [
        "env",
        "NEXTEST_RETRIES=0",
        "cargo",
        "nextest",
        "run",
        "-j",
        "4",
        "--status-level",
        "fail",
        "--final-status-level",
        "fail",
        *arguments,
    ]


CASES: list[dict[str, Any]] = [
    {
        "name": "core-plan",
        "argv": nextest(
            "-p", "zeppelin-embed", "--features", "graph-cypher", "--test", "graph_query_plan"
        ),
        "needles": ["27 tests run: 27 passed, 0 skipped"],
    },
    {
        "name": "compiler",
        "argv": nextest(
            "-p", "zeppelin-embed-cypher", "--test", "binding", "--test", "mutation_lowering",
            "--test", "read_lowering", "--test", "lowering_semantics", "--test", "search_lowering",
            "--test", "lowering_allocation", "--test", "runtime_lowering",
        ),
        "needles": ["69 tests run: 69 passed, 0 skipped"],
    },
    {
        "name": "graph-adversarial",
        "argv": nextest(
            "-p", "zeppelin-embed-workspace-tests", "--features", "graph-cypher", "--test",
            "adversarial_tests", "-E",
            "test(=property_graph_binding_probe_preserves_profile_types_and_faults) | test(=property_graph_lowering_probe_checks_complete_plans_and_inflight_faults) | test(=property_graph_search_lowering_probe_checks_typed_plans_and_inflight_faults) | test(=property_graph_mutation_lowering_probe_checks_typed_plans_and_inflight_faults) | test(=one_runner_episode_reaches_required_mutation_lowering_contracts) | test(=native_graph_runner_keys_are_active_with_graph_feature) | test(=graph_response_runner_keys_are_absent_without_test_hook)",
        ),
        "needles": ["7 tests run: 7 passed, 470 skipped"],
    },
    {
        "name": "hook-registry-controls",
        "argv": nextest(
            "-p", "zeppelin-embed-workspace-tests", "--features", "graph-result-test-support", "--test",
            "adversarial_tests", "-E",
            "test(=native_graph_runner_keys_are_active_with_graph_feature) | test(=graph_response_runner_keys_are_active_with_test_hook)",
        ),
        "needles": ["2 tests run: 2 passed, 477 skipped"],
    },
    {
        "name": "no-graph-registry-control",
        "argv": nextest(
            "-p", "zeppelin-embed-workspace-tests", "--no-default-features", "--test", "adversarial_tests",
            "-E", "test(=native_graph_runner_keys_are_absent_without_graph_feature)",
        ),
        "needles": ["1 test run: 1 passed, 433 skipped"],
    },
    {
        "name": "documentation",
        "argv": ["cargo", "test", "-p", "zeppelin-embed-cypher", "--doc", "--", "--test-threads=1"],
        "needles": [
            "test result: ok. 1 passed; 0 failed",
            "test result: ok. 5 passed; 0 failed",
        ],
    },
    {
        "name": "serial-compiler-isolation",
        "argv": [
            "cargo", "test", "-p", "zeppelin-embed-cypher", "--test", "mutation_lowering", "--test",
            "lowering_allocation", "--test", "runtime_lowering", "--", "--test-threads=1",
        ],
        "needles": [
            "test result: ok. 3 passed; 0 failed",
            "test result: ok. 12 passed; 0 failed",
            "test result: ok. 7 passed; 0 failed",
        ],
    },
    {
        "name": "pg21-fixed-seeds",
        "argv": [
            "cargo", "test", "-p", "zeppelin-embed-workspace-tests", "--features", "graph-cypher", "--test",
            "adversarial_tests", "property_graph_mutation_lowering_probe_checks_typed_plans_and_inflight_faults",
            "--", "--exact", "--test-threads=1", "--nocapture",
        ],
        "needles": [
            "PG21 seed=0 ProbeReport { observations: 6, orientations: 2",
            "PG21 seed=1 ProbeReport { observations: 6, orientations: 2",
            "PG21 seed=140 ProbeReport { observations: 6, orientations: 2",
            "PG21 seed=18446744073709551615 ProbeReport { observations: 6, orientations: 2",
        ],
    },
    {
        "name": "allocator-accounting",
        "argv": [
            "cargo", "test", "-p", "zeppelin-embed-cypher", "--test", "lowering_allocation",
            "mutation_lowering_real_allocator_fail_at_each_site_releases_all_backing", "--", "--exact",
            "--test-threads=1", "--nocapture",
        ],
        "needles": ["mutation_allocator_sites="],
    },
    {
        "name": "close-boundary",
        "argv": [
            "cargo", "test", "-p", "zeppelin-embed-cypher", "--test", "runtime_lowering",
            "compiled_mutation_lowering_late_checkpoint_preserves_close_before_cancel", "--", "--exact",
            "--test-threads=1", "--nocapture",
        ],
        "needles": ["binder_poll=", "first_lowering_poll=", "consumer_poll=", "fire="],
    },
    {
        "name": "core-clippy",
        "argv": [
            "cargo", "clippy", "-p", "zeppelin-embed", "--features", "graph-cypher", "--test",
            "graph_query_plan", "--", "-D", "warnings",
        ],
    },
    {
        "name": "compiler-clippy",
        "argv": [
            "cargo", "clippy", "-p", "zeppelin-embed-cypher", "--lib", "--test", "binding", "--test",
            "mutation_lowering", "--test", "read_lowering", "--test", "lowering_semantics", "--test",
            "search_lowering", "--test", "lowering_allocation", "--test", "runtime_lowering", "--no-deps",
            "--", "-D", "warnings",
        ],
    },
    {
        "name": "adversarial-clippy",
        "argv": [
            "cargo", "clippy", "-p", "zeppelin-embed-workspace-tests", "--features", "graph-cypher", "--test",
            "adversarial_tests", "--", "-D", "warnings",
        ],
    },
    {
        "name": "workspace-fmt-boundary",
        "argv": ["cargo", "fmt", "--all", "--check"],
        "expected_exit": 1,
        "needles": ["crates/zeppelin-embed/src/property_graph/storage/records.rs"],
        "only_fmt_path": "crates/zeppelin-embed/src/property_graph/storage/records.rs",
    },
    {
        "name": "owned-rustfmt",
        "argv": ["rustfmt", "--edition", "2024", "--check", *OWNED_RUST_PATHS],
    },
    {
        "name": "diff-check",
        "argv": ["git", "diff", "--check"],
    },
]


def persist(raw_sections: list[str], receipts: list[dict[str, Any]]) -> None:
    RAW_LOG.write_text("".join(raw_sections).rstrip("\n") + "\n")
    RECEIPTS.write_text(
        json.dumps(
            {
                "driver": str(Path(__file__).relative_to(ROOT)),
                "owned_rust_paths": OWNED_RUST_PATHS,
                "receipts": receipts,
            },
            indent=2,
        )
        + "\n"
    )


def main() -> int:
    raw_sections: list[str] = []
    receipts: list[dict[str, Any]] = []
    for index, case in enumerate(CASES, start=1):
        argv = case["argv"]
        command = shlex.join(argv)
        result = subprocess.run(
            argv,
            cwd=ROOT,
            env=os.environ.copy(),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            check=False,
        )
        expected_exit = case.get("expected_exit", 0)
        missing = [needle for needle in case.get("needles", []) if needle not in result.stdout]
        fmt_paths = re.findall(r"^Diff in (.+?):", result.stdout, flags=re.MULTILINE)
        only_fmt_path = case.get("only_fmt_path")
        fmt_ok = only_fmt_path is None or (fmt_paths and set(fmt_paths) == {str(ROOT / only_fmt_path)})
        passed = result.returncode == expected_exit and not missing and fmt_ok
        run_ids = re.findall(r"Nextest run ID ([0-9a-f-]+)", result.stdout)
        receipt = {
            "index": index,
            "name": case["name"],
            "command": command,
            "expected_exit": expected_exit,
            "exit": result.returncode,
            "output_sha256": hashlib.sha256(result.stdout.encode()).hexdigest(),
            "run_ids": run_ids,
            "required_output": case.get("needles", []),
            "missing_required_output": missing,
            "observed_fmt_paths": fmt_paths,
            "passed": passed,
            "raw_log": RAW_LOG.name,
        }
        receipts.append(receipt)
        raw_sections.append(
            f"===== COMMAND {index}: {case['name']} =====\n"
            f"COMMAND: {command}\n"
            f"EXPECTED_EXIT: {expected_exit}\n"
            "RAW_OUTPUT_BEGIN\n"
            f"{result.stdout}"
            "RAW_OUTPUT_END\n"
            f"EXIT_CODE: {result.returncode}\n"
            f"OUTPUT_SHA256: {receipt['output_sha256']}\n"
            f"REQUIRED_OUTPUT_OBSERVED: {not missing}\n"
            f"COMMAND_PASSED: {passed}\n\n"
        )
        persist(raw_sections, receipts)
        print(f"{index:02d} {case['name']}: exit={result.returncode} passed={passed}")
        if not passed:
            if missing:
                print(f"missing output: {missing}", file=sys.stderr)
            if not fmt_ok:
                print(f"unexpected fmt paths: {fmt_paths}", file=sys.stderr)
            return 1
    print(f"ALL_FINAL_COMMANDS_PASSED: {len(CASES)}/{len(CASES)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
