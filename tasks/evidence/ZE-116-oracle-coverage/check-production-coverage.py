"""Compare actual LLVM exports without changing oracle source inventory."""

import gzip
import json
from pathlib import Path
import sys


INVENTORY = {
    "diagnostics_health.rs", "ffi_bindings.rs", "fts.rs", "hybrid_fusion.rs",
    "ingest_retention.rs", "lib.rs", "lifecycle_accounting.rs",
    "metadata_filter_planner.rs", "property_graph.rs", "storage_durability.rs",
    "tiering_maintenance.rs", "vamana_graph.rs", "vector_execution.rs",
}


def read(path):
    raw = Path(path).read_bytes()
    if path.endswith(".gz"):
        raw = gzip.decompress(raw)
    report = json.loads(raw)
    files = report["files"] if "files" in report else report["data"][0]["files"]
    selected = [f for f in files if "/tests/adversarial-oracle/src/" in f["filename"]]
    result = {Path(f["filename"]).name: f["summary"]["lines"] for f in selected}
    assert len(selected) == len(INVENTORY) and result.keys() == INVENTORY
    return result


def subtotal(rows, names):
    covered = sum(rows[name]["covered"] for name in names)
    count = sum(rows[name]["count"] for name in names)
    return {"covered": covered, "count": count, "percent": 100 * covered / count}


baseline, final = map(read, sys.argv[1:3])
assert all(baseline[name]["count"] == final[name]["count"] for name in INVENTORY)
clean = INVENTORY - {"property_graph.rs"}
result = {
    "inventory": {
        name: {"baseline": baseline[name], "final": final[name]}
        for name in sorted(INVENTORY)
    },
    "baseline_clean": subtotal(baseline, clean),
    "baseline_integrated": subtotal(baseline, INVENTORY),
    "final_clean": subtotal(final, clean),
    "final_integrated": subtotal(final, INVENTORY),
}
print(json.dumps(result, indent=2))
assert result["final_clean"]["covered"] * 10 >= result["final_clean"]["count"] * 9, (
    "independent oracle production coverage below 90%", result["final_clean"]
)
