"""The graph-free build must reject text before touching files or symbols."""

from pathlib import Path

import pytest

import zeppelin_embed as ze
from zeppelin_embed import _ffi


def test_text_entry_points_raise_typed_platform_error(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(_ffi, "TEXT_AVAILABLE", False)
    with pytest.raises(
        ze.TextUnavailable, match="text API is available on macOS arm64 only"
    ):
        ze.open_text(tmp_path / "store", tmp_path / "missing.zem")
    with pytest.raises(
        ze.TextUnavailable, match="text API is available on macOS arm64 only"
    ):
        ze.open_model(
            tmp_path / "store",
            repo="fixture/tiny",
            revision="a" * 40,
            manifest_sha256="0" * 64,
        )
    with ze.open(tmp_path / "core") as store:
        for call in [
            lambda: store.ingest_text([1], ["text"]),
            lambda: store.query_text("text"),
            lambda: store.delete_text([1]),
            lambda: store.health(),
        ]:
            with pytest.raises(ze.TextUnavailable):
                call()
    assert not (tmp_path / "store").exists()
