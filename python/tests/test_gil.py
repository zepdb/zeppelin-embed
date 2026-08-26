"""GIL-release and cross-thread cancellation proof."""

from __future__ import annotations

import ctypes as ct
import threading
import time
from pathlib import Path

import numpy as np
import pytest

import zeppelin_embed as ze
from zeppelin_embed._library import LIBRARY


def test_cancel_mid_query_returns_cancelled_from_another_python_thread(
    tmp_path: Path,
) -> None:
    rows = 100
    dimensions = 8_192
    vectors = np.full((rows, dimensions), 0.25, dtype=np.float32)
    with ze.open(tmp_path / "store") as store, ze.CancelToken() as token:
        store.ingest(list(range(1, rows + 1)), vectors)

        def cancel_after_query_starts() -> None:
            time.sleep(0.001)
            token.cancel()

        canceller = threading.Thread(target=cancel_after_query_starts)
        canceller.start()
        with pytest.raises(ze.Cancelled):
            store.query(
                vector=vectors[0],
                k=rows,
                thread_budget=1,
                tier=ze.Tier.SCAN,
                cancel_token=token,
            )
        canceller.join(timeout=5)
        assert not canceller.is_alive()


def test_all_abi_calls_use_cdll_not_pydll() -> None:
    assert type(LIBRARY) is ct.CDLL
