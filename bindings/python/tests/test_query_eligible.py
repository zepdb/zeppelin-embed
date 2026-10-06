from pathlib import Path

import numpy as np
import pytest

import zeppelin_embed as ze


def test_query_eligible_ids(tmp_path: Path) -> None:
    vectors = np.asarray([[1.0, 1.0], [2.0, 1.0], [3.0, 1.0]], dtype=np.float32)
    with ze.open(tmp_path / "eligible") as store:
        store.ingest([1, 2, 3], vectors, texts=["amber cedar"] * 3)
        store.seal()
        for legs in ({"text": "amber"}, {"vector": vectors[0]}, {"text": "amber", "vector": vectors[0]}):
            assert len(store.query(**legs, k=3).hits) == 3
            assert store.query(**legs, eligible_ids=[]).hits == ()
            assert [hit.doc_id for hit in store.query(**legs, eligible_ids=[3, 3, 999], k=1).hits] == [3]
        for ids in ([-1], [1 << 128], ["bad"]):
            with pytest.raises(ze.ZeppelinError):
                store.query(text="amber", eligible_ids=ids)
