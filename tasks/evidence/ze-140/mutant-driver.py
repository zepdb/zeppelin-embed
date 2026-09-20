#!/usr/bin/env python3
"""Apply every ZE-140 source mutant, prove its exact RED, restore, and prove GREEN."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
from typing import Any


ROOT = Path(__file__).resolve().parents[3]
RAW_LOG = ROOT / "tasks/evidence/ze-140/mutant-raw.log"
JSON_LOG = ROOT / "tasks/evidence/ze-140/mutant-receipts.json"


def exact_test(package: str, test_file: str, test_name: str, *extra: str) -> list[str]:
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
        "-p",
        package,
        *extra,
        "--test",
        test_file,
        "-E",
        f"test(={test_name})",
    ]


MUTATION = "crates/zeppelin-embed-cypher/src/lowering/mutation.rs"
LOWERING = "crates/zeppelin-embed-cypher/src/lowering/mod.rs"
CORE = "crates/zeppelin-embed/src/property_graph/query/plan/mutation.rs"

CASES: list[dict[str, Any]] = [
    {
        "id": 1,
        "name": "relationship-before-fresh-right-endpoint",
        "path": MUTATION,
        "old": """                self.ensure_created_node(bound, right_id, right)?;
                self.create_relationship(bound, left_id, relationship_id, right_id, relationship)?;
""",
        "new": """                self.create_relationship(bound, left_id, relationship_id, right_id, relationship)?;
                self.ensure_created_node(bound, right_id, right)?;
""",
        "command": exact_test(
            "zeppelin-embed-cypher",
            "mutation_lowering",
            "create_endpoint_dependencies_preserve_textual_property_order",
        ),
        "assertion": "Plan(Scope)",
    },
    {
        "id": 2,
        "name": "swap-incoming-source-target",
        "path": MUTATION,
        "old": "            crate::Direction::Incoming => (right, left),",
        "new": "            crate::Direction::Incoming => (left, right),",
        "command": exact_test(
            "zeppelin-embed-cypher",
            "mutation_lowering",
            "create_lowers_all_shapes_slots_names_and_directions",
        ),
        "assertion": "CreateRelationship",
    },
    {
        "id": 3,
        "name": "right-property-before-relationship",
        "path": MUTATION,
        "old": """                self.create_relationship(bound, left_id, relationship_id, right_id, relationship)?;
                self.inline_properties(bound, relationship_id, relationship)?;
                self.inline_properties(bound, right_id, right)?;
""",
        "new": """                self.inline_properties(bound, right_id, right)?;
                self.create_relationship(bound, left_id, relationship_id, right_id, relationship)?;
                self.inline_properties(bound, relationship_id, relationship)?;
""",
        "command": exact_test(
            "zeppelin-embed-cypher",
            "mutation_lowering",
            "create_endpoint_dependencies_preserve_textual_property_order",
        ),
        "assertion": "Plan(Scope)",
    },
    {
        "id": 4,
        "name": "omit-eager-barrier",
        "path": MUTATION,
        "old": "        let eager = self.operator(DraftOp::Eager, &[current], clause.span)?;",
        "new": "        let eager = current;",
        "command": exact_test(
            "zeppelin-embed-cypher",
            "mutation_lowering",
            "mutation_clauses_keep_immediate_eager_and_downstream_projections",
        ),
        "assertion": "Plan(Barrier)",
    },
    {
        "id": 5,
        "name": "reverse-label-present-bit",
        "path": MUTATION,
        "old": "                    let present = matches!(item.kind, NodeKind::SetLabels { .. });",
        "new": "                    let present = !matches!(item.kind, NodeKind::SetLabels { .. });",
        "command": exact_test(
            "zeppelin-embed-cypher",
            "mutation_lowering",
            "mutation_items_preserve_set_remove_delete_and_detach_order",
        ),
        "assertion": "SetLabel",
    },
    {
        "id": 6,
        "name": "rhs-through-invariant-alias-lowering",
        "path": MUTATION,
        "old": "        self.lower_expression(bound, id, span)",
        "new": "        self.lower_invariant_expression(bound, id, span)",
        "command": exact_test(
            "zeppelin-embed-cypher",
            "mutation_lowering",
            "mutation_rhs_keeps_fresh_properties_and_frozen_alias_slots",
        ),
        "assertion": "row-dependent search expression",
    },
    {
        "id": 7,
        "name": "restore-node-only-detach-validation",
        "path": CORE,
        "old": """            Mutation::Delete {
                entity: receiver, ..
            } => entity(
                description,
                receiver,
                output,
                seen,
                context,
                ValueKinds::NODE
                    .union(ValueKinds::REL)
                    .union(ValueKinds::NULL),
            )?,
""",
        "new": """            Mutation::Delete {
                entity: receiver,
                detach,
            } => {
                let kinds = if detach {
                    ValueKinds::NODE.union(ValueKinds::NULL)
                } else {
                    ValueKinds::NODE
                        .union(ValueKinds::REL)
                        .union(ValueKinds::NULL)
                };
                entity(description, receiver, output, seen, context, kinds)?;
            }
""",
        "command": exact_test(
            "zeppelin-embed",
            "graph_query_plan",
            "detach_delete_accepts_node_relationship_and_null_targets",
            "--features",
            "graph-cypher",
        ),
        "assertion": "left: Err(Type)",
    },
    {
        "id": 8,
        "name": "force-dynamic-deleted-flag-false",
        "path": LOWERING,
        "old": "                bound.requires_deleted_runtime_validation(),",
        "new": "                false,",
        "command": exact_test(
            "zeppelin-embed-cypher",
            "mutation_lowering",
            "mutation_deleted_boundaries_keep_static_errors_and_dynamic_obligation",
        ),
        "assertion": "OPTIONAL MATCH (n) DELETE n RETURN n",
    },
    {
        "id": 9,
        "name": "omit-mutation-arena-owner",
        "path": LOWERING,
        "old": "            RetainedAllocation::arena(&mutations).map_err(memory_error)?,\n",
        "new": "",
        "command": exact_test(
            "zeppelin-embed-cypher",
            "mutation_lowering",
            "mutation_owner_inventory_proves_full_capacity_and_facts",
        ),
        "assertion": "left: 34",
    },
    {
        "id": 10,
        "name": "omit-mutation-span-owner",
        "path": LOWERING,
        "old": "            RetainedAllocation::arena(&self.mutation_spans.arena).map_err(memory_error)?,",
        "new": "            RetainedAllocation::arena(&mutations).map_err(memory_error)?,",
        "command": exact_test(
            "zeppelin-embed-cypher",
            "mutation_lowering",
            "mutation_owner_inventory_proves_full_capacity_and_facts",
        ),
        "assertion": "UnprovedInput",
    },
    {
        "id": 11,
        "name": "omit-final-mutation-capacity",
        "path": LOWERING,
        "old": """        let mut mutations = QueryArena::new(memory, self.mutations.len()).map_err(memory_error)?;
""",
        "new": """        let mut mutations = QueryArena::new(memory, self.mutations.len().saturating_sub(1))
            .map_err(memory_error)?;
""",
        "command": exact_test(
            "zeppelin-embed-cypher",
            "lowering_allocation",
            "mutation_lowering_real_allocator_fail_at_each_site_releases_all_backing",
        ),
        "assertion": "read lowering reservation",
    },
]


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def run(command: list[str]) -> tuple[int, str]:
    result = subprocess.run(
        command,
        cwd=ROOT,
        env=os.environ.copy(),
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        check=False,
    )
    return result.returncode, result.stdout


def append_raw(lines: list[str], value: str = "") -> None:
    lines.append(value)
    print(value, flush=True)


def main() -> int:
    raw: list[str] = []
    receipts: list[dict[str, Any]] = []
    for case in CASES:
        path = ROOT / case["path"]
        before = path.read_bytes()
        before_text = before.decode("utf-8")
        count = before_text.count(case["old"])
        if count != 1:
            raise RuntimeError(f"case {case['id']} expected one checked replacement, got {count}")
        before_hash = sha256(before)
        mutated = before_text.replace(case["old"], case["new"], 1).encode("utf-8")
        mutation_command = (
            f"checked_replace {shlex.quote(case['path'])} "
            f"--before-sha256 {before_hash} --old {json.dumps(case['old'])} "
            f"--new {json.dumps(case['new'])}"
        )
        append_raw(raw, f"===== MUTANT {case['id']}: {case['name']} =====")
        append_raw(raw, f"MUTATION_COMMAND: {mutation_command}")
        append_raw(raw, f"BEFORE_SHA256: {before_hash}")
        try:
            path.write_bytes(mutated)
            mutant_hash = sha256(path.read_bytes())
            command_text = shlex.join(case["command"])
            append_raw(raw, f"MUTANT_SHA256: {mutant_hash}")
            append_raw(raw, f"RED_COMMAND: {command_text}")
            red_exit, red_output = run(case["command"])
            append_raw(raw, f"RED_EXIT: {red_exit}")
            append_raw(raw, "RED_OUTPUT_BEGIN")
            append_raw(raw, red_output.rstrip())
            append_raw(raw, "RED_OUTPUT_END")
            fired = red_exit != 0 and case["assertion"] in red_output
            append_raw(raw, f"INTENDED_ASSERTION: {case['assertion']}")
            append_raw(raw, f"INTENDED_ASSERTION_FIRED: {str(fired).lower()}")
            if not fired:
                raise RuntimeError(
                    f"case {case['id']} did not fire intended assertion {case['assertion']!r}"
                )
        finally:
            path.write_bytes(before)
        restored_hash = sha256(path.read_bytes())
        append_raw(raw, f"RESTORED_SHA256: {restored_hash}")
        if restored_hash != before_hash:
            raise RuntimeError(f"case {case['id']} restoration hash mismatch")
        append_raw(raw, f"GREEN_COMMAND: {command_text}")
        green_exit, green_output = run(case["command"])
        append_raw(raw, f"GREEN_EXIT: {green_exit}")
        append_raw(raw, "GREEN_OUTPUT_BEGIN")
        append_raw(raw, green_output.rstrip())
        append_raw(raw, "GREEN_OUTPUT_END")
        if green_exit != 0:
            raise RuntimeError(f"case {case['id']} restoration did not return GREEN")
        receipts.append(
            {
                "id": case["id"],
                "name": case["name"],
                "source": case["path"],
                "mutation_command": mutation_command,
                "test_command": command_text,
                "before_sha256": before_hash,
                "mutant_sha256": mutant_hash,
                "red_exit": red_exit,
                "intended_assertion": case["assertion"],
                "intended_assertion_fired": fired,
                "red_output": red_output,
                "restored_sha256": restored_hash,
                "green_exit": green_exit,
                "green_output": green_output,
            }
        )
        append_raw(raw)
        RAW_LOG.write_text("\n".join(raw) + "\n", encoding="utf-8")
        JSON_LOG.write_text(
            json.dumps({"driver": Path(__file__).name, "receipts": receipts}, indent=2) + "\n",
            encoding="utf-8",
        )
    append_raw(raw, f"ALL_MUTANTS_RESTORED: {len(receipts)}/{len(CASES)}")
    RAW_LOG.write_text("\n".join(raw) + "\n", encoding="utf-8")
    JSON_LOG.write_text(
        json.dumps({"driver": Path(__file__).name, "receipts": receipts}, indent=2) + "\n",
        encoding="utf-8",
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        print(f"MUTANT_DRIVER_ERROR: {error}", file=sys.stderr)
        raise
