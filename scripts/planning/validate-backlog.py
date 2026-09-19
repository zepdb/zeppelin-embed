"""Validate graph planning contracts and current tracker state, never product behavior.

Read-only successor to tracker/planning/validate-backlog.py. Planning artifacts
remain local. --root selects their repository without relocating this source.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import sqlite3
import subprocess
import urllib.request


WORKSTREAMS = {
    "identity": ("E2", "ZE-21"), "writes": ("E3", "ZE-22"),
    "storage": ("E4", "ZE-23"), "execution": ("E5", "ZE-24"),
    "cypher": ("E6", "ZE-25"), "retrieval": ("E7", "ZE-26"),
    "bindings": ("E8", "ZE-27"), "qualification": ("E9", "ZE-28"),
}
# The approved original inventory is fixed; whole-tracker totals are not.
IMPLEMENTATION_KEYS = {f"ZE-{number}" for number in range(32, 79)}
# Retain exact dependency comparison, including the recorded later amendments.
# ZE-101 owns ZE-62/ZE-51; ZE-97/102 own ZE-106/107; ZE-113..116 are
# ZE-32's independently verified qualification prerequisites.
EXTRA_PREREQUISITES = {
    "ZE-32": {"ZE-113", "ZE-114", "ZE-115", "ZE-116"},
    "ZE-62": {"ZE-51"}, "ZE-71": {"ZE-107"},
    "ZE-78": {"ZE-106", "ZE-107"},
}
FOLLOWUP_PREREQUISITES = {
    "ZE-106": {"ZE-29", "ZE-39"},
    "ZE-107": {"ZE-29", "ZE-39", "ZE-54", "ZE-67"},
    "ZE-108": {"ZE-78"},
}
ZE101_REPLACEMENTS = (
    (
        "Implement internal view-stamped EligibleNodeSet, full-width sorted-ID baseline and per-source masks,",
        "Borrow execution-owned view-stamped EligibleNodeSet from ZE-51; implement per-source masks and membership translation,",
    ),
    (
        "Account construction/masks and fail before ranking a partial set.",
        "Execution charges set construction once; retrieval charges only additional masks/translation capacity to the same QueryResources and fail before ranking a partial set.",
    ),
)


def require(condition, message):
    if not condition:
        raise ValueError(message)


def cli(root, *args):
    env = os.environ.copy()
    env.pop("TRACKER_DB", None)
    env.pop("TRACKER_PREFIX", None)
    return json.loads(subprocess.run(
        ["node", "--no-warnings", "tracker/cli.mjs", *args],
        cwd=root, env=env, text=True, capture_output=True, check=True,
    ).stdout)


def closure(graph, start):
    """Reject unresolved edges and cycles, and return the reachable key set."""
    seen, visiting = set(), set()

    def visit(key):
        require(key in graph, f"missing dependency target: {key}")
        require(key not in visiting, f"dependency cycle: {key}")
        if key in seen:
            return
        visiting.add(key)
        for dependency in graph[key]:
            visit(dependency)
        visiting.remove(key)
        seen.add(key)

    for key in start:
        visit(key)
    return seen


def validate_backlog(manifest, index, backlogs, snapshot):
    """Check one exported snapshot against the recorded planning artifacts."""
    require(snapshot["prefix"] == "ZE", "wrong tracker prefix")
    tickets = {ticket["key"]: ticket for ticket in snapshot["tickets"]}
    ids = {ticket["id"]: ticket["key"] for ticket in snapshot["tickets"]}
    epics = {f"E{epic['id']}" for epic in snapshot["epics"]}
    require(len(tickets) == len(ids) == len(snapshot["tickets"]), "duplicate ticket key/id")
    require(len(epics) == len(snapshot["epics"]), "duplicate epic id")
    graph = {key: set() for key in tickets}
    for edge in snapshot["dependencies"]:
        require(edge["ticket_id"] in ids and edge["blocked_by_id"] in ids,
                "missing dependency endpoint")
        key, prerequisite = ids[edge["ticket_id"]], ids[edge["blocked_by_id"]]
        require(prerequisite not in graph[key], f"duplicate dependency: {key}/{prerequisite}")
        graph[key].add(prerequisite)
    closure(graph, graph)
    require(all(ticket["status"] in {"todo", "in_progress", "done"}
                for ticket in tickets.values()), "invalid ticket status")
    require({name: manifest[name] for name in ("setup", "review", "integration", "audit")} ==
            {"setup": "ZE-20", "review": "ZE-29", "integration": "ZE-30", "audit": "ZE-31"},
            "planning control key changed")
    workstreams = {ws["slug"]: (ws["epic"], ws["draft"]) for ws in manifest["workstreams"]}
    require(len(manifest["workstreams"]) == len(WORKSTREAMS) and workstreams == WORKSTREAMS,
            "workstream scope changed")
    require(set(backlogs) == set(WORKSTREAMS), "backlog workstream inventory changed")
    require({epic for epic, _ in WORKSTREAMS.values()} <= epics, "missing graph epic")
    require(index["review"] == manifest["review"], "index review changed")
    require({row["key"] for row in index["tickets"].values()} == IMPLEMENTATION_KEYS and
            len(index["tickets"]) == len(IMPLEMENTATION_KEYS), "required implementation inventory changed")
    require(IMPLEMENTATION_KEYS <= tickets.keys(), "missing required implementation ticket")
    for key in ("ZE-20", "ZE-30", "ZE-31", "ZE-97", "ZE-101", "ZE-102", "ZE-103"):
        require(key in tickets and tickets[key]["status"] == "done", f"planning prerequisite incomplete: {key}")
    require("ZE-29" in tickets, "missing integrated review")
    review_done = tickets["ZE-29"]["status"] == "done"
    candidates = {}
    for slug, (epic, draft) in WORKSTREAMS.items():
        require(draft in tickets and tickets[draft]["status"] == "done", f"draft incomplete: {draft}")
        for candidate in backlogs[slug]:
            name = slug + "/" + candidate["slug"]
            require(name not in candidates, f"duplicate candidate: {name}")
            candidates[name] = candidate
            require(name in index["tickets"], f"missing candidate index: {name}")
            record = index["tickets"][name]
            key = record["key"]
            ticket = tickets[key]
            require(ticket["type"] == "story" and f"E{ticket['epic_id']}" == epic,
                    f"ticket scope changed: {key}")
            require(ticket["themes"] and "implementation" in ticket["labels"], f"ticket metadata changed: {key}")
            require(ticket["title"] == record["title"] == candidate["title"], f"ticket title changed: {key}")
            require(record["epic"] == epic and record["source_plan"] == candidate["source_plan"] ==
                    f"docs/graph/plans/{slug}.md", f"plan ownership changed: {key}")
            require(record["depends_on"] == candidate["depends_on"], f"candidate prerequisites changed: {key}")
            require(all(dep in index["tickets"] for dep in candidate["depends_on"]),
                    f"missing candidate prerequisite: {key}")
            expected = {manifest["review"], *(index["tickets"][dep]["key"] for dep in candidate["depends_on"])}
            require(expected == set(record["blocked_by"]), f"original prerequisites changed: {key}")
            require(graph[key] == expected | EXTRA_PREREQUISITES.get(key, set()),
                    f"live prerequisites changed: {key}")
            description = candidate["description"].rstrip()
            if key == "ZE-62":
                for old, new in ZE101_REPLACEMENTS:
                    require(description.count(old) == 1, "ZE-101 original scope changed")
                    description = description.replace(old, new)
            require(ticket["description"].startswith(description), f"ticket scope text changed: {key}")
            require(record["candidate_sha256"] == hashlib.sha256(
                json.dumps(candidate, sort_keys=True).encode()).hexdigest(), f"candidate hash changed: {key}")
            require(review_done or ticket["status"] == "todo", f"implementation started before review: {key}")
            if ticket["status"] != "todo":
                require(all(tickets[dep]["status"] == "done" for dep in graph[key]),
                        f"implementation started before prerequisites: {key}")
    candidate_graph = {name: candidate["depends_on"] for name, candidate in candidates.items()}
    seen = closure(candidate_graph, ["qualification/release-evidence"])
    require(seen == set(candidates) == set(index["tickets"]), "candidate outside release closure")
    for key, expected in FOLLOWUP_PREREQUISITES.items():
        require(key in graph and graph[key] == expected, f"follow-up prerequisites changed: {key}")
    release = closure(graph, ["ZE-78"])
    require(IMPLEMENTATION_KEYS | {"ZE-106", "ZE-107"} <= release, "required ticket outside live release closure")
    require("ZE-108" not in release, "deferred platform scope entered first release")
    frontier = sorted(key for key in IMPLEMENTATION_KEYS if tickets[key]["status"] == "todo" and
                      all(tickets[dep]["status"] == "done" for dep in graph[key]))
    return {
        "scope": "Current documentary planning/backlog checks only; no product execution or runtime acceptance",
        "total_epics": len(epics), "total_tickets": len(tickets),
        "workstream_epics": len(WORKSTREAMS), "detailed_plans": len(WORKSTREAMS),
        "implementation_tickets": len(candidates),
        "candidate_prerequisite_edges": sum(len(candidate["depends_on"]) for candidate in candidates.values()),
        "original_implementation_edges_with_review": sum(len(record["blocked_by"]) for record in index["tickets"].values()),
        "current_implementation_edges_with_review": sum(len(graph[key]) for key in IMPLEMENTATION_KEYS),
        "release_closure": len(seen), "cycles": 0, "unresolved_dependencies": 0,
        "all_implementation_require_review": True, "review_status": tickets["ZE-29"]["status"],
        "implementation_status_counts": {status: sum(tickets[key]["status"] == status for key in IMPLEMENTATION_KEYS)
                                         for status in ("todo", "in_progress", "done")},
        "implementation_frontier": frontier, "required_release_followups": ["ZE-106", "ZE-107"],
        "deferred_after_first_release": ["ZE-108"], "export_prefix": snapshot["prefix"],
    }


def validate_documents(root):
    required = {root / f"docs/graph/plans/{slug}.md" for slug in (*WORKSTREAMS, "README", "review")}
    paths = sorted((root / "docs/graph/plans").glob("*.md"))
    require(required <= set(paths), "missing required plan Markdown file")
    paths += [root / path for path in (
        "docs/graph/native-graph-plan.md", "docs/graph/planning-briefs.md", "docs/agents/issue-tracker.md")]
    for path in paths:
        contents = path.read_text()
        require(not re.search(r"\[(?:ZE-\d+|E\d+)\]\(", contents), f"key-only title link: {path}")
        for target in re.findall(r"\]\(([^)]+)\)", contents):
            if "://" in target:
                continue
            raw, _, anchor = target.partition("#")
            target_path = path.parent / raw if raw else path
            require(target_path.exists(), f"broken local link: {path}: {target}")
            if anchor and target_path.suffix == ".md":
                headings = re.findall(r"^#{1,6}\s+(.+)$", target_path.read_text(), re.M)
                slugs = {re.sub(r"[^\w\- ]", "", heading.lower()).replace(" ", "-") for heading in headings}
                require(anchor in slugs, f"broken local anchor: {path}: {target}")
        require(all(line == line.rstrip() for line in contents.splitlines()), f"trailing whitespace: {path}")
    return {str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest() for path in paths}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[2])
    args = parser.parse_args()
    root = args.root.resolve()
    planning = root / "tracker/planning"
    manifest = json.loads((planning / "manifest.json").read_text())
    index = json.loads((planning / "implementation-index.json").read_text())
    backlogs = {slug: json.loads((planning / f"{slug}-backlog.json").read_text()) for slug in WORKSTREAMS}
    # Export to stdout: inspect the current snapshot without overwriting the
    # historical backup or validation.json and without importing private data.
    report = validate_backlog(manifest, index, backlogs, cli(root, "export"))
    with sqlite3.connect(f"file:{root / 'tracker/data/tracker.db'}?mode=ro", uri=True) as connection:
        require(connection.execute("pragma integrity_check").fetchall() == [("ok",)], "SQLite integrity failure")
        require(connection.execute("pragma foreign_key_check").fetchall() == [], "SQLite foreign-key failure")
    with urllib.request.urlopen("http://localhost:4731", timeout=5) as response:
        require(response.status == 200, "tracker HTTP unavailable")
    hashes = validate_documents(root)
    subprocess.run(["git", "diff", "--check"], cwd=root, check=True)
    report.update(sqlite_integrity="ok", foreign_key_violations=0, http_status=200,
                  checked_markdown_files=len(hashes), broken_local_links_or_anchors=0,
                  diff_check="passed", sha256=hashes)
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
