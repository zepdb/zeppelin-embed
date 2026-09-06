"""Add five precomputed vectors to Zeppelin Embed and search them."""

from __future__ import annotations

import tempfile
from pathlib import Path

import numpy as np
import zeppelin_embed as ze

DOCUMENTS = (
    (1, [0.90, 0.10, 0.05, 0.00]),
    (2, [0.85, 0.15, 0.10, 0.05]),
    (3, [0.10, 0.90, 0.05, 0.00]),
    (4, [0.05, 0.10, 0.90, 0.00]),
    (5, [0.00, 0.05, 0.10, 0.90]),
)

QUERY_VECTOR = np.asarray([0.88, 0.12, 0.07, 0.02], dtype=np.float32)


def main() -> int:
    doc_ids = [doc_id for doc_id, _ in DOCUMENTS]
    vectors = np.asarray([vector for _, vector in DOCUMENTS], dtype=np.float32)

    with (
        tempfile.TemporaryDirectory() as scratch,
        ze.open(Path(scratch) / "store") as store,
    ):
        mutation = store.ingest(
            doc_ids,
            vectors,
            revisions=[1] * len(doc_ids),
            timestamps=[10, 20, 30, 40, 50],
        )
        print(
            f"ingested {store.stats().active_row_count} vectors "
            f"at generation {mutation.generation}"
        )

        for rank, hit in enumerate(store.search(QUERY_VECTOR, k=3).hits, start=1):
            print(f"{rank}. document {hit.doc_id}, score {hit.score:.6f}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
