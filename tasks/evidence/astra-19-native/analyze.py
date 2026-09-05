"""Validate matched whole-API results before reporting any timing changes."""
import hashlib, json, math, re, statistics, struct, sys
from pathlib import Path

r = Path(sys.argv[1])
diagnostic = len(sys.argv) > 2 and sys.argv[2] == "diagnostic"
manifest = json.loads((r / ("diagnostic-manifest.json" if diagnostic else "manifest.json")).read_text())
runroot = r / ("diagnostic-runs" if diagnostic else "ordinary-runs")
receipts = json.loads((runroot / "receipts.json").read_text())
assert json.loads((runroot / "complete.json").read_text()) == {"complete": True, "processes": len(manifest)}
assert len(receipts) == len(manifest)


def quantile(values, p):
    return sorted(values)[max(0, math.ceil(len(values) * p) - 1)]


def quality(samples, fixture):
    lookup = {str(d["id"]): d["source_id"] for d in fixture["documents"]}
    qrels = {}
    for row in fixture["qrels"]:
        qrels.setdefault(row["query-id"], {})[row["corpus-id"]] = int(row["score"])
    ndcg, recall = [], []
    for row in samples:
        rel = qrels[row["query"]]
        seen, returned = set(), []
        for hit in row["hits"]:
            doc = lookup[hit["id"]]
            if doc not in seen:
                seen.add(doc)
                returned.append(doc)
        gains = [rel.get(doc, 0) for doc in returned[:10]]
        ideal = sum(g / math.log2(i + 2) for i, g in enumerate(sorted(rel.values(), reverse=True)[:10]))
        ndcg.append(sum(g / math.log2(i + 2) for i, g in enumerate(gains)) / ideal if ideal else 0)
        recall.append(sum(g > 0 for g in gains) / sum(g > 0 for g in rel.values()))
    return statistics.mean(ndcg), statistics.mean(recall)


canonical, identities, groups, span_rows, canonical_work = {}, {}, {}, [], {}
for config, receipt in zip(manifest, receipts):
    assert receipt["label"] == config["label"] and receipt["exit_code"] == 0
    assert hashlib.sha256(Path(config["command"][0]).read_bytes()).hexdigest() == receipt["binary_sha256"]
    data = json.loads((Path(config["command"][3]) / "results.json").read_text())
    fixture = json.loads(Path(config["command"][2]).read_text())
    assert data["warmups"] == 20 and len(data["samples"]) == 64
    assert [x["query"] for x in data["samples"]] == [x["id"] for x in fixture["queries"]]
    identity = (data["token_ids"], data["epoch"], data["backend"])
    state, api, arm = config["state"], config["api"], config["arm"]
    if state in identities:
        assert identity == identities[state]
    else:
        identities[state] = identity
    assert "coreml-cpu-ane" in data["backend"] and "Some(64)" in data["backend"]
    key = (state, api)
    hits = [(x["query"], x["hits"]) for x in data["samples"]]
    for _, rows in hits:
        for hit in rows:
            assert struct.unpack(">Q", struct.pack(">d", hit["score"]))[0] == hit["score_bits"]
    if key in canonical:
        assert hits == canonical[key], config["label"]
    else:
        canonical[key] = hits
    log = (runroot / (config["label"] + ".log")).read_text()
    rss = re.search(r"(\d+)\s+maximum resident set size", log)
    assert rss
    times = [x["ms"] for x in data["samples"]]
    ndcg, recall = quality(data["samples"], fixture)
    row = {**{k: config[k] for k in ["label", "arm", "state", "api", "rep"]},
           "p50_ms": quantile(times, .5), "p95_ms": quantile(times, .95),
           "ndcg10": ndcg, "recall10": recall, "max_rss_bytes": int(rss.group(1))}
    groups.setdefault((state, api, arm), []).append(row)
    if diagnostic:
        def clean_work(sample):
            w = dict(sample["work"])
            w["counters"] = re.sub(r"worker_thread_ids: \[[^]]*\]", "worker_thread_ids: []", w["counters"])
            return sample["query"], w
        work = [clean_work(x) for x in data["samples"]]
        if key in canonical_work:
            assert work == canonical_work[key], ("work differs", config["label"])
        else:
            canonical_work[key] = work
        active = None
        parsed = {}
        for line in log.splitlines():
            if line.startswith("ASTRA19_QUERY_BEGIN "):
                active = json.loads(line.split(" ", 1)[1])
                assert active not in parsed
                parsed[active] = {}
            elif line == "ASTRA19_QUERY_END":
                active = None
            elif line.startswith("ASTRA19_SPAN ") and active is not None:
                _, stage, a, b = line.split()
                a, b = int(a), int(b)
                assert b >= a
                parsed[active].setdefault(stage, []).append([a, b])
        assert set(parsed) == {x["query"] for x in data["samples"]}
        for sample in data["samples"]:
            spans = parsed[sample["query"]]
            assert len(spans["native"]) == 1 and spans["lexical"]
            a, b = spans["native"][0]
            overlap = sum(max(0, min(b, d) - max(a, c)) for c, d in spans["lexical"])
            if arm == "before":
                assert overlap == 0, ("serial control overlapped", config["label"], sample["query"])
            else:
                assert len(spans["embedding"]) == 1 and len(spans["vector"]) == 1
                assert spans["embedding"][0][0] <= a <= b <= spans["embedding"][0][1]
                assert spans["vector"][0][0] >= spans["embedding"][0][1]
            span_rows.append({"state": state, "arm": arm, "rep": config["rep"],
                "query": sample["query"], "native_lexical_overlap_ns": overlap,
                "spans": spans, "stages": sample["stages"]})

cells = []
for state in ["all", "tomb", "graph"]:
    for api in (["hybrid"] if diagnostic else (["dense", "lexical", "hybrid"] if state == "graph" else ["dense", "exact", "lexical", "hybrid"])):
        sides = {}
        for arm in ["before", "after"]:
            values = groups[state, api, arm]
            assert len(values) == 3
            sides[arm] = {
                "p50_ms": statistics.median(x["p50_ms"] for x in values),
                "p95_ms": statistics.median(x["p95_ms"] for x in values),
                "p95_range_ms": [min(x["p95_ms"] for x in values), max(x["p95_ms"] for x in values)],
                "median_max_rss_bytes": statistics.median(x["max_rss_bytes"] for x in values),
                "ndcg10": values[0]["ndcg10"], "recall10": values[0]["recall10"],
                "repetitions": values,
            }
        b, a = sides["before"], sides["after"]
        cells.append({"state": state, "api": api, **sides,
            "p50_delta_ms": a["p50_ms"] - b["p50_ms"],
            "p95_delta_ms": a["p95_ms"] - b["p95_ms"],
            "p95_change_percent": 100 * (a["p95_ms"] / b["p95_ms"] - 1)})
result = {"status": "COMPLETE", "diagnostic": diagnostic, "processes": len(manifest),
    "timed_calls": len(manifest) * 64, "full_payload_parity": True,
    "token_epoch_backend_parity": True, "quantiles": "nearest rank per process; median of three process quantiles",
    "quality": "official linear-gain nDCG and Recall on first-parent dedup without refill; fixture judgments",
    "cells": cells}
if diagnostic:
    result["full_work_generation_plan_parity"] = True
    result["span_summary"] = []
    for state in ["all", "tomb", "graph"]:
        for arm in ["before", "after"]:
            rows = [x for x in span_rows if x["state"] == state and x["arm"] == arm]
            values = [x["native_lexical_overlap_ns"] / 1e6 for x in rows]
            result["span_summary"].append({"state": state, "arm": arm, "queries": len(rows),
                "queries_with_overlap": sum(x > 0 for x in values),
                "overlap_p50_ms": quantile(values, .5), "overlap_p95_ms": quantile(values, .95),
                "embedding_queue_p50_ms": quantile([x["stages"]["embedding_queue_ns"]/1e6 for x in rows], .5),
                "embedding_queue_p95_ms": quantile([x["stages"]["embedding_queue_ns"]/1e6 for x in rows], .95),
                "lexical_queue_p50_ms": quantile([x["stages"]["lexical_queue_ns"]/1e6 for x in rows], .5),
                "lexical_queue_p95_ms": quantile([x["stages"]["lexical_queue_ns"]/1e6 for x in rows], .95)})
    (r / "actual-spans.json").write_text(json.dumps(span_rows, indent=2) + "\n")
(r / ("diagnostic-summary.json" if diagnostic else "ordinary-summary.json")).write_text(json.dumps(result, indent=2) + "\n")
for row in cells:
    print(row["state"], row["api"], row["before"]["p50_ms"], row["after"]["p50_ms"],
          row["before"]["p95_ms"], row["after"]["p95_ms"], row["p95_change_percent"])
if diagnostic:
    print(json.dumps(result["span_summary"], indent=2))
