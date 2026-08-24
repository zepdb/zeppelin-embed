#!/usr/bin/env python3
"""SQLite FTS5's BM25 on the same BEIR corpora.

Usage:
    python3 tools/lexical-bakeoff/fts5_bakeoff.py <beir-dir> <corpus-name>

Prints one line: `corpus <name> ndcg10 <v> docs <n> queries <n> index_ms <n>
query_ms <n>`.

Fairness notes
--------------
* Same corpus, queries, qrels, and nDCG@10 definition, recomputed here so a
  bug in one evaluator cannot flatter both.
* FTS5's `bm25()` returns a NEGATIVE score, more negative meaning better, so
  that `ORDER BY rank` ascending puts the best row first. That sign trap is
  handled explicitly below. Getting it backwards silently inverts relevance
  and is the single easiest way to publish a wrong FTS5 comparison.
* `unicode61` is FTS5's default tokenizer. It has no stemming and no
  stopword list, and it shatters identifiers. Those differences are
  REPORTED, not corrected: they are real properties of the competitor.
"""

import json
import math
import re
import sqlite3
import sys
import time
from collections import defaultdict
from pathlib import Path


def read_jsonl(path):
    with open(path, encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if line:
                yield json.loads(line)


def read_qrels(path):
    qrels = defaultdict(dict)
    with open(path, encoding="utf-8") as handle:
        for index, line in enumerate(handle):
            line = line.rstrip("\n")
            if not line.strip():
                continue
            parts = line.split("\t")
            if len(parts) < 3:
                continue
            try:
                grade = int(parts[2])
            except ValueError:
                if index == 0:
                    continue
                raise
            # TREC convention: a negative judgement is non-relevant.
            qrels[parts[0]][parts[1]] = max(grade, 0)
    return qrels


def dcg(grades, k=10):
    return sum(g / math.log2(i + 2) for i, g in enumerate(grades[:k]))


def ndcg_at_10(run, qrels):
    total, counted = 0.0, 0
    for query, judged in qrels.items():
        ideal = sorted((g for g in judged.values() if g > 0), reverse=True)
        if not ideal:
            continue
        ideal_dcg = dcg(ideal)
        if ideal_dcg <= 0:
            continue
        ranked = run.get(query, [])
        grades = [judged.get(doc, 0) for doc in ranked[:10]]
        total += dcg(grades) / ideal_dcg
        counted += 1
    return total / counted if counted else 0.0


def main():
    if len(sys.argv) < 3:
        print("usage: fts5_bakeoff.py <beir-dir> <corpus-name>", file=sys.stderr)
        return 2
    base = Path(sys.argv[1]) / sys.argv[2]
    name = sys.argv[2]

    connection = sqlite3.connect(":memory:")
    connection.execute("PRAGMA journal_mode=OFF")
    connection.execute("PRAGMA synchronous=OFF")
    # Two columns so the comparison matches the two-field configuration the
    # other two engines used.
    connection.execute(
        "CREATE VIRTUAL TABLE docs USING fts5(doc_id UNINDEXED, title, body)"
    )

    started = time.perf_counter()
    rows = 0
    batch = []
    for record in read_jsonl(base / "corpus.jsonl"):
        batch.append(
            (record.get("_id", ""), record.get("title", ""), record.get("text", ""))
        )
        rows += 1
        if len(batch) >= 5000:
            connection.executemany("INSERT INTO docs VALUES (?,?,?)", batch)
            batch.clear()
    if batch:
        connection.executemany("INSERT INTO docs VALUES (?,?,?)", batch)
    connection.commit()
    index_ms = int((time.perf_counter() - started) * 1000)

    qrels = read_qrels(base / "qrels" / "test.tsv")
    judged = set(qrels)

    run = {}
    executed = 0
    query_started = time.perf_counter()
    for record in read_jsonl(base / "queries.jsonl"):
        query_id = record.get("_id", "")
        if query_id not in judged:
            continue
        # FTS5 MATCH is a query language. BEIR queries are natural language,
        # so every token is quoted to force a literal OR-of-terms rather than
        # letting punctuation be parsed as syntax.
        tokens = re.findall(r"\w+", record.get("text", ""), flags=re.UNICODE)
        if not tokens:
            continue
        expression = " OR ".join(f'"{token}"' for token in tokens)
        try:
            # bm25() is NEGATIVE, best-first ascending. Ordering by rank
            # ascending is therefore the correct "best first".
            cursor = connection.execute(
                "SELECT doc_id FROM docs WHERE docs MATCH ? ORDER BY rank LIMIT 10",
                (expression,),
            )
            run[query_id] = [row[0] for row in cursor.fetchall()]
            executed += 1
        except sqlite3.OperationalError:
            continue
    query_ms = int((time.perf_counter() - query_started) * 1000)

    print(
        f"corpus {name} ndcg10 {ndcg_at_10(run, qrels):.4f} docs {rows} "
        f"queries {executed} index_ms {index_ms} query_ms {query_ms}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
