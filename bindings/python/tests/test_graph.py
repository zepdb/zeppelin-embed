"""Graph-free wheels refuse graph methods before touching the store or ABI."""

from pathlib import Path

import numpy as np
import pytest

import zeppelin_embed as ze


@pytest.mark.parametrize(
    ("method", "args", "kwargs"),
    [
        ("enable_graph", (), {}),
        ("graph_apply", ([{"id": 123, "metadata": b"document"}],), {}),
        ("graph_query", ({"op": "scan"},), {"parameters": {"ids": [123]}}),
        ("cypher", ("MATCH (n) RETURN n",), {"max_rows": 10}),
        ("get_nodes", ([123],), {"text": True, "vector": True}),
        ("get_relationships", ([123],), {}),
        ("graph_resources", (), {}),
    ],
)
@pytest.mark.parametrize("closed", [False, True])
def test_graph_free_methods_raise_unsupported_build_without_abi_calls(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    method: str,
    args: tuple,
    kwargs: dict,
    closed: bool,
) -> None:
    with ze.open(tmp_path / "store") as store:
        store.ingest(
            [123], np.ones((1, 4), dtype=np.float32), texts=["existing document"]
        )
        if closed:
            store.close()
        before = {
            p.relative_to(tmp_path): p.read_bytes()
            for p in tmp_path.rglob("*")
            if p.is_file()
        }
        calls = store.abi_call_count

        def forbidden(*args: object, **kwargs: object) -> None:
            pytest.fail("graph refusal touched the ABI or live handle")

        with monkeypatch.context() as patch:
            patch.setattr(store, "_call", forbidden)
            patch.setattr(store, "_live_handle", forbidden)
            with pytest.raises(ze.UnsupportedBuild) as caught:
                getattr(store, method)(*args, **kwargs)
        assert caught.value.code == 59
        assert caught.value.code_name == "ZE_ERR_GRAPH_UNSUPPORTED_BUILD"
        assert store.abi_call_count == calls
        assert {
            p.relative_to(tmp_path): p.read_bytes()
            for p in tmp_path.rglob("*")
            if p.is_file()
        } == before
        if not closed:
            assert store.query(text="missing", k=1).hits == ()


def test_unsupported_build_is_the_native_error_mapping(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    from zeppelin_embed import _errors

    assert issubclass(ze.UnsupportedBuild, ze.ZeppelinError)
    assert ze.ERROR_TYPES[59] is ze.UnsupportedBuild
    assert ze.GraphUnsupportedBuild is ze.UnsupportedBuild
    monkeypatch.setattr(
        _errors, "last_error_message", lambda handle: "graph-free build"
    )
    with pytest.raises(ze.UnsupportedBuild):
        ze.raise_for_status(59)
