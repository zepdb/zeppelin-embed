#!/usr/bin/env python3
"""Run isolated ZE-33 faults, requiring intended test failure and exact restore.

Run from the worktree root after all other compiles against it have stopped.
Logs default to /tmp/ze-33-evidence; targets are local to this worktree.
"""
import hashlib
import json
import os
from pathlib import Path
import subprocess

ROOT = Path.cwd()
OUT = Path(os.environ.get("ZE33_EVIDENCE", "/tmp/ze-33-evidence"))
OUT.mkdir(parents=True, exist_ok=True)
CANONICAL = "crates/zeppelin-embed/src/property_graph/canonical.rs"
PROVENANCE = "crates/zeppelin-embed/src/property_graph/provenance.rs"
ORACLE = "tests/adversarial-oracle/src/graph_contents.rs"
BASE = {name: (ROOT / name).read_bytes() for name in [CANONICAL, PROVENANCE, ORACLE]}

# (name, source, exact replaced text, mutant text, named public test, package/target)
cases = [
    ("property-order", CANONICAL, "properties.sort_unstable_by_key(|property| property.name);", "// deliberately omit property sorting", "canonical_property_order_and_labels_are_byte_exact", "core"),
    ("duplicate-property", CANONICAL, "if previous == Some(property.name) {", "if false && previous == Some(property.name) {", "canonical_property_order_and_labels_are_byte_exact", "core"),
    ("signed-zero", CANONICAL, "e.emit(&value.to_bits().to_le_bytes())\n", "e.emit(&(if value == 0.0 { 0_u64 } else { value.to_bits() }).to_le_bytes())\n", "scalar_list_tags_and_float_payloads_have_literal_goldens", "core"),
    ("vector-original-bits", CANONICAL, "encoder.emit(&value.to_bits().to_le_bytes())?;", "encoder.emit(&(value.to_bits() & !1).to_le_bytes())?;", "original_vector_bits_and_every_document_field_determine_contents", "core"),
    ("hash-equality-shortcut", CANONICAL, "let width = scratch.len() / 2;", "if left_fingerprint == right_fingerprint { return Ok(CanonicalComparison { equal: true, bytes_compared: 0 }); }\n    let width = scratch.len() / 2;", "forced_hash_collision_requires_every_byte_and_relocated_sources_compare_equal", "core"),
    ("uncharged-framing", CANONICAL, "if total > MAX_GRAPH_INPUT_BYTES as u64 {", "if false && total > MAX_GRAPH_INPUT_BYTES as u64 {", "framing_is_charged_to_the_eight_mib_limit", "core"),
    ("ignored-cancellation", CANONICAL, "(self.checkpoint)()?;", "let _ = (self.checkpoint)();", "stream_failures_cancellation_and_boundaries_fail_loudly", "core"),
    ("provenance-generation", PROVENANCE, "e.emit(&self.fields.original_generation.get().to_le_bytes())?;", "e.emit(&0_u64.to_le_bytes())?;", "replay_provenance_fields_are_versioned_and_never_defaulted", "core"),
    ("provenance-shortcut", PROVENANCE, "if left.provenance != right.provenance {", "if false && left.provenance != right.provenance {", "same_revision_replay_requires_contents_and_all_operation_fields", "core"),
    ("oracle-cannot-fire", ORACLE, "if observed == expected {", "if true || observed == expected {", "adversarial::graph_contents::canonical_oracle_rejects_changed_observations", "runner"),
]

results = []
try:
    for name, source, before, after, test, target in cases:
        original = BASE[source]
        text = original.decode()
        if text.count(before) != 1:
            raise RuntimeError(f"{name}: expected one exact replacement, got {text.count(before)}")
        path = ROOT / source
        path.write_text(text.replace(before, after, 1))
        command = ["cargo", "test", "-p", "zeppelin-embed" if target == "core" else "zeppelin-embed-workspace-tests", "--test", "graph_canonical" if target == "core" else "adversarial_tests", test, "--", "--exact", "--nocapture"]
        env = dict(os.environ, CARGO_TARGET_DIR="target/ze33")
        log = OUT / f"red-mutant-{name}.log"
        try:
            with log.open("wb") as stream:
                run = subprocess.run(command, cwd=ROOT, env=env, stdout=stream, stderr=subprocess.STDOUT)
        finally:
            path.write_bytes(original)
        output = log.read_text()
        if run.returncode != 101 or f"test {test} ... FAILED" not in output or "panicked at" not in output:
            raise RuntimeError(f"{name}: intended named assertion did not fail; see {log}")
        restored = hashlib.sha256(path.read_bytes()).hexdigest()
        if restored != hashlib.sha256(original).hexdigest():
            raise RuntimeError(f"{name}: source restoration mismatch")
        results.append({"mutation": name, "test": test, "command": command, "exit": run.returncode, "restored_sha256": restored, "log": str(log)})
        print(f"{name}: intended RED exit101; restored {restored}", flush=True)
finally:
    for name, original in BASE.items():
        (ROOT / name).write_bytes(original)
    (OUT / "mutation-results.json").write_text(json.dumps(results, indent=2) + "\n")

for package, target, name in [("zeppelin-embed", "graph_canonical", ""), ("zeppelin-embed-workspace-tests", "adversarial_tests", "adversarial::graph_contents::canonical_oracle_rejects_changed_observations")]:
    command = ["cargo", "test", "-p", package, "--test", target]
    if name:
        command += [name, "--", "--exact", "--nocapture"]
    with (OUT / f"green-restored-{target}.log").open("wb") as stream:
        subprocess.run(command, cwd=ROOT, env=dict(os.environ, CARGO_TARGET_DIR="target/ze33"), stdout=stream, stderr=subprocess.STDOUT, check=True)
print("all mutants restored; complete canonical suite and oracle can-fire GREEN", flush=True)
