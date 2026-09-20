#!/usr/bin/env python3
"""Run ZE-141's bounded semantic mutants and restore exact source bytes."""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path


ROOT = Path(__file__).resolve().parents[3]
EVIDENCE = ROOT / "tasks/evidence/ze-141"
ORIGINALS = Path("/tmp/ze-141-mutation-originals")
ENV = os.environ | {"ZE_TEST_SEED": "141"}
NEXTEST = ["cargo", "nextest", "run", "--profile", "default", "-j", "4", "--retries", "0"]

FFI = "crates/zeppelin-embed-ffi/src/graph_result/conversion.rs"
OWNER = "crates/zeppelin-embed-ffi/src/graph_result/registration.rs"
ARENA = "crates/zeppelin-embed-ffi/src/graph_result.rs"
PROBE = "tests/adversarial/graph_response.rs"

OWNED_SOURCES = [
    ARENA,
    OWNER,
    FFI,
    "crates/zeppelin-embed-ffi/src/graph_result/conversion/tests.rs",
    "crates/zeppelin-embed-ffi/src/graph_result/test_support.rs",
    "crates/zeppelin-embed-ffi/src/graph_result/test_support/conversion.rs",
    "tests/adversarial-oracle/src/graph_response.rs",
    "tests/adversarial/coverage.rs",
    PROBE,
    "tests/adversarial_tests.rs",
]


def ffi_test(name: str) -> list[str]:
    return NEXTEST + [
        "-p",
        "zeppelin-embed-ffi",
        "--features",
        "graph-cypher",
        "--lib",
        "-E",
        f"test(=graph_result::conversion::tests::{name})",
    ]


def hash_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


@dataclass(frozen=True)
class Replacement:
    path: str
    before: str
    after: str
    count: int = 1


@dataclass(frozen=True)
class Mutation:
    key: str
    property: str
    replacements: tuple[Replacement, ...]
    command: tuple[str, ...]


MUTATIONS = [
    Mutation(
        "01-id-high-half",
        "all-pool literal oracle rejects a zeroed node-id high half",
        (
            Replacement(
                FFI,
                "fn node_id(value: u128) -> ZeNodeId {\n    ZeNodeId {\n        high: (value >> 64) as u64,",
                "fn node_id(value: u128) -> ZeNodeId {\n    ZeNodeId {\n        high: 0,",
            ),
        ),
        tuple(ffi_test("graph_result_native_all_pools_match_literal_field_oracle")),
    ),
    Mutation(
        "02-f64-bits",
        "all-pool literal oracle rejects normalized negative-zero or NaN bits",
        (
            Replacement(
                FFI,
                "            output.floating = f64::from_bits(*bits);",
                "            output.floating = 0.0;",
            ),
        ),
        tuple(ffi_test("graph_result_native_all_pools_match_literal_field_oracle")),
    ),
    Mutation(
        "03-presence-bits",
        "literal pool and report oracles reject cleared present-empty text and Some(0) epoch flags",
        (
            Replacement(FFI, "        Some(text) => (1, range(text)),", "        Some(text) => (0, range(text)),"),
            Replacement(FFI, "        Some(value) => (1, value),", "        Some(value) => (0, value),"),
        ),
        tuple(
            NEXTEST
            + [
                "-p",
                "zeppelin-embed-ffi",
                "--features",
                "graph-cypher",
                "--lib",
                "-E",
                "test(=graph_result::conversion::tests::graph_result_native_all_pools_match_literal_field_oracle) | test(=graph_result::conversion::tests::graph_result_native_reports_receipts_outcomes_are_lossless)",
            ]
        ),
    ),
    Mutation(
        "04-empty-list-tag",
        "literal value oracle rejects mapping the stored Empty sentinel as Query",
        (
            Replacement(
                FFI,
                "                ListKind::Empty => ZeGraphListKind::ZeGraphListEmpty,",
                "                ListKind::Empty => ZeGraphListKind::ZeGraphListQuery,",
            ),
        ),
        tuple(ffi_test("graph_result_native_all_pools_match_literal_field_oracle")),
    ),
    Mutation(
        "05-receipt-disposition",
        "receipt oracle rejects a replayed item forced to Committed",
        (
            Replacement(
                FFI,
                "        disposition: if value.receipt.replayed {\n            ZeGraphDisposition::ZeGraphDispositionReplayed as u32",
                "        disposition: if value.receipt.replayed {\n            ZeGraphDisposition::ZeGraphDispositionCommitted as u32",
            ),
        ),
        tuple(ffi_test("graph_result_native_reports_receipts_outcomes_are_lossless")),
    ),
    Mutation(
        "06-report-work-range",
        "eight-report oracle rejects ranges starting at 22*i instead of 23+22*i",
        (
            Replacement(
                FFI,
                "            let value = GLOBAL_WORK_COUNT\n                .checked_add(",
                "            let value = 0_usize\n                .checked_add(",
            ),
        ),
        tuple(ffi_test("graph_result_native_reports_receipts_outcomes_are_lossless")),
    ),
    Mutation(
        "07-completed-abi-counter",
        "actual-driver oracle rejects CompletedAbiBytes copied from CompletedBytes",
        (
            Replacement(
                FFI,
                "        WorkKind::CompletedAbiBytes,\n        ZeGraphWorkKind::ZeGraphWorkCompletedAbiBytes as u32,",
                "        WorkKind::CompletedBytes,\n        ZeGraphWorkKind::ZeGraphWorkCompletedAbiBytes as u32,",
            ),
        ),
        tuple(ffi_test("graph_result_native_driver_finalizes_all_23_exact_counters")),
    ),
    Mutation(
        "08-peak-counter",
        "actual-driver oracle rejects a zero peak-owned value",
        (
            Replacement(FFI, "                peak_query_bytes as u64;", "                0;"),
        ),
        tuple(ffi_test("graph_result_native_driver_finalizes_all_23_exact_counters")),
    ),
    Mutation(
        "09-copied-byte-charge",
        "known copied-byte delta rejects a removed direct-map chunk charge",
        (
            Replacement(
                ARENA,
                "        while initialized < count {\n            let length = (count - initialized).min(chunk_elements);\n            let chunk_bytes = element_size.checked_mul(length).ok_or(OwnerError::Limit)?;\n            context.charge(WorkKind::CopiedBytes, chunk_bytes as u64)?;\n            let end = initialized + length;",
                "        while initialized < count {\n            let length = (count - initialized).min(chunk_elements);\n            let chunk_bytes = element_size.checked_mul(length).ok_or(OwnerError::Limit)?;\n            let _ = chunk_bytes;\n            let end = initialized + length;",
            ),
        ),
        tuple(ffi_test("graph_result_native_driver_finalizes_all_23_exact_counters")),
    ),
    Mutation(
        "10-overlap-accounting",
        "measured peak-minus-one overlap test rejects uncharged live C arena capacity",
        (
            Replacement(
                OWNER,
                "        let bytes = plan\n            .layout\n            .size()\n            .checked_add(size_of::<Node>())",
                "        let bytes = 0_usize\n            .checked_add(size_of::<Node>())",
            ),
        ),
        tuple(ffi_test("graph_result_native_geometry_limits_and_real_overlap_reject")),
    ),
    Mutation(
        "11-post-registration-checkpoint",
        "scheduled final-checkpoint cancellation rejects success after checkpoint removal",
        (
            Replacement(
                OWNER,
                "        context.checkpoint()?;\n        Ok(prepared)\n    }\n    /// Validates every immutable root/pool field",
                "        Ok(prepared)\n    }\n    /// Validates every immutable root/pool field",
            ),
        ),
        tuple(ffi_test("graph_result_native_every_allocation_and_copy_checkpoint_cleans")),
    ),
    Mutation(
        "12-finalizer-allocation",
        "deny-all finalization audit rejects a retained Vec allocation",
        (
            Replacement(
                FFI,
                "    global_work.finalize(execution.counters, execution.peak_query_bytes);\n    FinalizedNativeResponse { response, outcome }",
                "    global_work.finalize(execution.counters, execution.peak_query_bytes);\n    let retained = Vec::<u8>::with_capacity(1);\n    std::hint::black_box(&retained);\n    FinalizedNativeResponse { response, outcome }",
            ),
        ),
        tuple(ffi_test("graph_result_native_driver_finalizes_all_23_exact_counters")),
    ),
    Mutation(
        "13-native-probe-route",
        "canonical seed-141 runner rejects removal of the additive native probe",
        (
            Replacement(
                PROBE,
                "    native_probe(seed, coverage, &mut report)?;\n",
                "",
            ),
        ),
        tuple(
            NEXTEST
            + [
                "-p",
                "zeppelin-embed-workspace-tests",
                "--features",
                "graph-result-test-support",
                "--test",
                "adversarial_tests",
                "-E",
                "test(=one_runner_episode_reaches_required_native_response_contracts)",
            ]
        ),
    ),
    Mutation(
        "14-c-allocation-receipt",
        "four-seed primitive fault checker rejects suppressed real C allocation injection",
        (
            Replacement(
                ARENA,
                "    if test_support::refuse_allocation() {",
                "    if false && test_support::refuse_allocation() {",
            ),
        ),
        tuple(
            NEXTEST
            + [
                "-p",
                "zeppelin-embed-workspace-tests",
                "--features",
                "graph-result-test-support",
                "--test",
                "adversarial_tests",
                "-E",
                "test(=property_graph_native_response_probe_checks_conversion_and_paired_faults)",
            ]
        ),
    ),
]


def main() -> None:
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    if ORIGINALS.exists():
        shutil.rmtree(ORIGINALS)
    ORIGINALS.mkdir(parents=True)
    original_manifest: dict[str, str] = {}
    for relative in OWNED_SOURCES:
        source = ROOT / relative
        data = source.read_bytes()
        original_manifest[relative] = hash_bytes(data)
        target = ORIGINALS / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(data)
    (ORIGINALS / "sha256.json").write_text(
        json.dumps(original_manifest, indent=2, sort_keys=True) + "\n"
    )

    records: list[dict[str, object]] = []
    for mutation in MUTATIONS:
        originals: dict[str, bytes] = {}
        before_hashes: dict[str, str] = {}
        mutant_hashes: dict[str, str] = {}
        restored_hashes: dict[str, str] = {}
        try:
            for replacement in mutation.replacements:
                path = ROOT / replacement.path
                data = originals.setdefault(replacement.path, path.read_bytes())
                text = data.decode()
                actual = text.count(replacement.before)
                if actual != replacement.count:
                    raise RuntimeError(
                        f"{mutation.key}: {replacement.path}: expected {replacement.count} occurrences, found {actual}"
                    )
                before_hashes[replacement.path] = hash_bytes(data)
                changed = text.replace(replacement.before, replacement.after, replacement.count).encode()
                path.write_bytes(changed)
                mutant_hashes[replacement.path] = hash_bytes(changed)

            red = subprocess.run(
                mutation.command,
                cwd=ROOT,
                env=ENV,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                timeout=180,
                check=False,
            )
            (EVIDENCE / f"mutation-{mutation.key}-red.log").write_bytes(red.stdout)
            if red.returncode == 0:
                raise RuntimeError(f"{mutation.key}: semantic mutant survived")
        finally:
            for relative, data in originals.items():
                path = ROOT / relative
                path.write_bytes(data)
                restored_hashes[relative] = hash_bytes(path.read_bytes())
                if restored_hashes[relative] != before_hashes[relative]:
                    raise RuntimeError(f"{mutation.key}: exact restoration failed for {relative}")

        green = subprocess.run(
            mutation.command,
            cwd=ROOT,
            env=ENV,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=180,
            check=False,
        )
        (EVIDENCE / f"mutation-{mutation.key}-green.log").write_bytes(green.stdout)
        if green.returncode != 0:
            raise RuntimeError(f"{mutation.key}: restored GREEN failed")
        records.append(
            {
                "key": mutation.key,
                "expected_assertion": mutation.property,
                "command": list(mutation.command),
                "red_exit_status": red.returncode,
                "green_exit_status": green.returncode,
                "before_sha256": before_hashes,
                "mutant_sha256": mutant_hashes,
                "restored_sha256": restored_hashes,
                "replacement_count": sum(item.count for item in mutation.replacements),
            }
        )

    diff = subprocess.run(
        ["git", "diff", "--check"],
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )
    if diff.returncode != 0:
        raise RuntimeError(diff.stdout.decode(errors="replace"))
    (EVIDENCE / "mutations.json").write_text(
        json.dumps(records, indent=2, sort_keys=True) + "\n"
    )


if __name__ == "__main__":
    main()
