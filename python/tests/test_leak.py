"""Ten-thousand-round Python/native footprint gate."""

from __future__ import annotations

import gc
import shutil
import tracemalloc
from pathlib import Path

import numpy as np

import zeppelin_embed as ze

ROUNDS = 10_000
MAX_GROWTH_BYTES = 8 * 1024 * 1024


def _round(store_path: Path, vector: np.ndarray) -> int | None:
    try:
        with ze.open(store_path) as store:
            store.ingest([1], vector)
            result = store.query(vector=vector[0], k=1)
            assert len(result.hits) == 1
            return store.stats().phys_footprint
    finally:
        if store_path.exists():
            shutil.rmtree(store_path)


def test_ten_thousand_open_ingest_query_close_rounds_are_flat(tmp_path: Path) -> None:
    vector = np.asarray([[0.125, 0.25, 0.5, 1.0]], dtype=np.float32)
    store_path = tmp_path / "leak-store"

    tracemalloc.start()
    for _ in range(20):
        _round(store_path, vector)
    gc.collect()
    python_start = tracemalloc.get_traced_memory()[0]
    footprint_start = _round(store_path, vector)

    for _ in range(ROUNDS):
        _round(store_path, vector)

    gc.collect()
    python_end = tracemalloc.get_traced_memory()[0]
    footprint_end = _round(store_path, vector)
    tracemalloc.stop()

    python_growth = max(0, python_end - python_start)
    if footprint_start is None or footprint_end is None:
        footprint_growth = 0
        assert footprint_start is None and footprint_end is None
    else:
        footprint_growth = max(0, footprint_end - footprint_start)

    print(
        f"rounds={ROUNDS} python_start={python_start} python_end={python_end} "
        f"python_growth={python_growth} phys_start={footprint_start} "
        f"phys_end={footprint_end} phys_growth={footprint_growth}"
    )
    assert python_growth <= MAX_GROWTH_BYTES
    assert footprint_growth <= MAX_GROWTH_BYTES
