"""Independent synthetic planning records; no private tracker fixtures or writes."""
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("graph_backlog", Path(__file__).with_name("validate-backlog.py"))
validator = importlib.util.module_from_spec(spec)
spec.loader.exec_module(validator)

# This explicit original inventory is test data, not imported validator policy.
WORK = {
    "identity": "domain-types canonical-values key-revisions catalog identity-oracle",
    "writes": "batch-staging wal-atomicity generation-publication recovery write-faults",
    "storage": "artifact-codec immutable-directory adjacency read-view consolidation storage-faults",
    "execution": "typed-ir-values batch-runtime pattern-operators relational-operators mutation-barriers completed-results",
    "cypher": "lexer-parser binder-profile read-lowering write-lowering search-calls profile-conformance",
    "retrieval": "read-adapter sparse-membership eligible-ranking hybrid-populations search-operator retrieval-oracle",
    "bindings": "rust-surface c-types result-ownership ffi-marshalling swift-surface shipping-package bindings-parity",
    "qualification": "fixture-oracle conformance-parity fault-registry resource-counters benchmark-host release-evidence",
}
ORIGINAL_ELIGIBILITY = (
    "Implement internal view-stamped EligibleNodeSet, full-width sorted-ID baseline and per-source masks, "
    "keep exhaustive ranking. Account construction/masks and fail before ranking a partial set."
)
APPROVED_ELIGIBILITY = (
    "Borrow execution-owned view-stamped EligibleNodeSet from ZE-51; implement per-source masks and membership translation, "
    "keep exhaustive ranking. Execution charges set construction once; retrieval charges only additional masks/translation capacity "
    "to the same QueryResources and fail before ranking a partial set."
)


def fixture():
    manifest = {"setup": "ZE-20", "review": "ZE-29", "integration": "ZE-30", "audit": "ZE-31", "workstreams": []}
    index = {"review": "ZE-29", "tickets": {}}
    backlogs = {}
    snapshot = {"prefix": "ZE", "epics": [{"id": n} for n in range(1, 10)], "tickets": [], "dependencies": []}

    def ticket(number, epic, status="todo", **fields):
        result = dict(id=number, key=f"ZE-{number}", title=f"Ticket {number}", description="Planning record",
                      epic_id=epic, type="story", status=status, themes=["graph"], labels=["implementation"])
        result.update(fields)
        snapshot["tickets"].append(result)
        return result

    for number in list(range(20, 32)) + [97, 101, 102, 103, 113, 114, 115, 116]:
        ticket(number, 1, "done")
    previous = None
    number = 32
    for epic, (work, slugs) in enumerate(WORK.items(), 2):
        manifest["workstreams"].append(dict(slug=work, epic=f"E{epic}", draft=f"ZE-{epic + 19}"))
        backlogs[work] = []
        for slug in slugs.split():
            name = f"{work}/{slug}"
            candidate = dict(slug=slug, title=f"Build {name}", source_plan=f"docs/graph/plans/{work}.md",
                             description=ORIGINAL_ELIGIBILITY if number == 62 else f"Preserve contract {number}.",
                             depends_on=[previous] if previous else [])
            backlogs[work].append(candidate)
            blockers = ["ZE-29"] + ([index["tickets"][previous]["key"]] if previous else [])
            index["tickets"][name] = dict(key=f"ZE-{number}", title=candidate["title"], epic=f"E{epic}",
                                         source_plan=candidate["source_plan"], depends_on=candidate["depends_on"].copy(),
                                         blocked_by=blockers, candidate_sha256=hashlib.sha256(json.dumps(candidate, sort_keys=True).encode()).hexdigest())
            ticket(number, epic, "done" if number == 32 else "in_progress" if number == 33 else "todo",
                   title=candidate["title"], description=(APPROVED_ELIGIBILITY if number == 62 else candidate["description"]) + "\n\nProgress can be appended.")
            for blocker in blockers:
                snapshot["dependencies"].append(dict(ticket_id=number, blocked_by_id=int(blocker[3:])))
            previous = name
            number += 1
    for target, blockers in {32: [113, 114, 115, 116], 62: [51], 71: [107], 78: [106, 107],
                             106: [29, 39], 107: [29, 39, 54, 67], 108: [78]}.items():
        if target >= 106:
            ticket(target, 3 if target == 106 else 8)
        for blocker in blockers:
            snapshot["dependencies"].append(dict(ticket_id=target, blocked_by_id=blocker))
    return manifest, index, backlogs, snapshot


class BacklogContracts(unittest.TestCase):
    def setUp(self):
        self.manifest, self.index, self.backlogs, self.snapshot = fixture()

    def validate(self):
        return validator.validate_backlog(self.manifest, self.index, self.backlogs, self.snapshot)

    def ticket(self, number):
        return next(t for t in self.snapshot["tickets"] if t["id"] == number)

    def test_current_backlog_accepts_normal_progress(self):
        result = self.validate()
        self.assertEqual(result["implementation_tickets"], 47)
        self.assertEqual(result["implementation_status_counts"], {"todo": 45, "in_progress": 1, "done": 1})
        self.ticket(33)["status"] = "done"
        self.ticket(34)["status"] = "in_progress"
        self.assertEqual(self.validate()["implementation_status_counts"], {"todo": 44, "in_progress": 1, "done": 2})

    def test_unrelated_epics_tickets_and_frontier_are_benign(self):
        self.snapshot["epics"].append({"id": 99})
        for number, status in [(200, "todo"), (201, "in_progress"), (202, "done")]:
            self.snapshot["tickets"].append(dict(self.ticket(33), id=number, key=f"ZE-{number}", epic_id=99, status=status))
        self.assertEqual(self.validate()["implementation_tickets"], 47)

    def test_missing_required_ticket_and_replacement_key_are_rejected(self):
        self.snapshot["tickets"].remove(self.ticket(32))
        self.snapshot["dependencies"] = [e for e in self.snapshot["dependencies"] if 32 not in e.values()]
        with self.assertRaisesRegex(ValueError, "missing required implementation ticket"):
            self.validate()
        self.setUp()
        self.index["tickets"]["identity/domain-types"]["key"] = "ZE-999"
        with self.assertRaisesRegex(ValueError, "required implementation inventory changed"):
            self.validate()

    def test_missing_or_extra_live_prerequisite_is_rejected(self):
        self.snapshot["dependencies"].remove({"ticket_id": 34, "blocked_by_id": 29})
        with self.assertRaisesRegex(ValueError, "live prerequisites changed: ZE-34"):
            self.validate()
        self.setUp()
        self.snapshot["dependencies"].append({"ticket_id": 34, "blocked_by_id": 31})
        with self.assertRaisesRegex(ValueError, "live prerequisites changed: ZE-34"):
            self.validate()

    def test_missing_qualification_gate_is_rejected(self):
        self.snapshot["dependencies"].remove({"ticket_id": 32, "blocked_by_id": 116})
        with self.assertRaisesRegex(ValueError, "live prerequisites changed: ZE-32"):
            self.validate()

    def test_cycle_and_unresolved_endpoint_are_rejected(self):
        self.snapshot["dependencies"].append({"ticket_id": 29, "blocked_by_id": 78})
        with self.assertRaisesRegex(ValueError, "dependency cycle"):
            self.validate()
        self.setUp()
        self.snapshot["dependencies"].append({"ticket_id": 29, "blocked_by_id": 999})
        with self.assertRaisesRegex(ValueError, "missing dependency endpoint"):
            self.validate()

    def test_scope_type_epic_label_and_candidate_hash_are_preserved(self):
        for field, value, error in [("type", "research", "ticket scope"), ("epic_id", 8, "ticket scope"),
                                    ("labels", [], "ticket metadata"), ("description", "Omit the contract", "ticket scope text")]:
            with self.subTest(field=field):
                self.setUp()
                self.ticket(34)[field] = value
                with self.assertRaisesRegex(ValueError, error):
                    self.validate()
        self.setUp()
        self.index["tickets"]["identity/domain-types"]["candidate_sha256"] = "0" * 64
        with self.assertRaisesRegex(ValueError, "candidate hash changed: ZE-32"):
            self.validate()

    def test_started_or_done_work_requires_review_and_every_prerequisite(self):
        for status in ("in_progress", "done"):
            for prerequisite in (29, 32):
                with self.subTest(status=status, prerequisite=prerequisite):
                    self.setUp()
                    self.ticket(33)["status"] = status
                    self.ticket(prerequisite)["status"] = "todo"
                    if prerequisite == 29:
                        self.ticket(32)["status"] = "todo"
                    with self.assertRaisesRegex(ValueError, "implementation started before"):
                        self.validate()
        self.setUp()
        self.ticket(116)["status"] = "todo"
        with self.assertRaisesRegex(ValueError, "implementation started before prerequisites: ZE-32"):
            self.validate()

    def test_waiting_implementation_is_valid_before_review(self):
        self.ticket(29)["status"] = "todo"
        self.ticket(32)["status"] = self.ticket(33)["status"] = "todo"
        self.assertEqual(self.validate()["implementation_frontier"], [])

    def test_approved_eligibility_scope_does_not_allow_arbitrary_replacement(self):
        self.ticket(62)["description"] = ORIGINAL_ELIGIBILITY
        with self.assertRaisesRegex(ValueError, "ticket scope text changed: ZE-62"):
            self.validate()
        self.ticket(62)["description"] = APPROVED_ELIGIBILITY.replace("same QueryResources", "unaccounted memory")
        with self.assertRaisesRegex(ValueError, "ticket scope text changed: ZE-62"):
            self.validate()

    def test_original_release_closure_remains_complete(self):
        candidate = self.backlogs["qualification"][-1]
        candidate["depends_on"] = []
        record = self.index["tickets"]["qualification/release-evidence"]
        record.update(depends_on=[], blocked_by=["ZE-29"], candidate_sha256=hashlib.sha256(json.dumps(candidate, sort_keys=True).encode()).hexdigest())
        self.snapshot["dependencies"].remove({"ticket_id": 78, "blocked_by_id": 77})
        with self.assertRaisesRegex(ValueError, "candidate outside release closure"):
            self.validate()

    def test_first_release_keeps_process_lock_and_packaging_followups(self):
        self.snapshot["dependencies"].remove({"ticket_id": 107, "blocked_by_id": 67})
        with self.assertRaisesRegex(ValueError, "follow-up prerequisites changed: ZE-107"):
            self.validate()
        self.setUp()
        self.snapshot["dependencies"].append({"ticket_id": 78, "blocked_by_id": 108})
        with self.assertRaisesRegex(ValueError, "dependency cycle"):
            self.validate()

    def test_missing_epic_or_workstream_is_rejected(self):
        self.snapshot["epics"].remove({"id": 2})
        with self.assertRaisesRegex(ValueError, "missing graph epic"):
            self.validate()
        self.setUp()
        self.manifest["workstreams"].pop()
        with self.assertRaisesRegex(ValueError, "workstream scope changed"):
            self.validate()


class DocumentContracts(unittest.TestCase):
    def test_extra_plan_documents_are_validated_without_losing_required_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            paths = [root / f"docs/graph/plans/{name}.md" for name in (*WORK, "README", "review")]
            paths += [root / name for name in ("docs/graph/native-graph-plan.md", "docs/graph/planning-briefs.md", "docs/agents/issue-tracker.md")]
            for path in paths:
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("# Valid heading\n")
            extra = root / "docs/graph/plans/extra.md"
            extra.write_text("[missing](no-file.md)\n")
            with self.assertRaisesRegex(ValueError, "broken local link"):
                validator.validate_documents(root)
            extra.write_text("# Extra plan\n")
            self.assertEqual(len(validator.validate_documents(root)), 14)
            paths[0].unlink()
            with self.assertRaisesRegex(ValueError, "missing required plan"):
                validator.validate_documents(root)

    def test_links_anchors_titles_and_whitespace_remain_checked(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            paths = [root / f"docs/graph/plans/{name}.md" for name in (*WORK, "README", "review")]
            paths += [root / name for name in ("docs/graph/native-graph-plan.md", "docs/graph/planning-briefs.md", "docs/agents/issue-tracker.md")]
            for path in paths:
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("# Valid heading\n\n[Local heading](#valid-heading)\n")
            self.assertEqual(len(validator.validate_documents(root)), 13)
            for bad, error in [("[missing](no-file.md)", "broken local link"), ("[missing](#other)", "broken local anchor"),
                               ("[ZE-32](http://localhost:4731)", "key-only title link"), ("extra space ", "trailing whitespace")]:
                with self.subTest(error=error):
                    paths[0].write_text("# Valid heading\n\n" + bad + "\n")
                    with self.assertRaisesRegex(ValueError, error):
                        validator.validate_documents(root)
            paths[0].write_text("# Valid heading\n")
            self.assertEqual(len(validator.validate_documents(root)), 13)


if __name__ == "__main__":
    unittest.main()
