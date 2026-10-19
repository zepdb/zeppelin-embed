"""Exercise the public package from an installed wheel."""

from __future__ import annotations

import argparse
import platform
import sys
import tempfile
from pathlib import Path

import zeppelin_embed as ze


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--bundle", type=Path)
    bundle = parser.parse_args().bundle
    package = Path(ze.__file__).resolve().parent
    assert ze.LIBRARY_PATH.parent == package / ".dylibs", ze.LIBRARY_PATH
    assert ze.LIBRARY_PATH.is_file(), ze.LIBRARY_PATH
    assert ze.ABI_VERSION == 1
    with tempfile.TemporaryDirectory() as temporary:
        with ze.open(Path(temporary) / "store") as store:
            assert store.state().state == ze.StoreState.OPEN
        if sys.platform == "darwin" and platform.machine() == "arm64":
            if bundle is None:
                bundle = (
                    Path(__file__).resolve().parents[3]
                    / "crates/zeppelin-embed-text/tests/fixtures/ze115/gte.zem"
                )
            assert ze.TEXT_AVAILABLE
            assert (ze.LIBRARY_PATH.parent / "mlx.metallib").is_file()
            with ze.open_text(Path(temporary) / "text-store", bundle) as store:
                store.ingest_text(
                    [7, 9], ["the bronze zeppelin", "marine biology field notes"]
                )
                for legs in ("dense", "lexical", "hybrid"):
                    result = store.query_text("the bronze zeppelin", k=1, legs=legs)
                    assert [hit.doc_id for hit in result.hits] == [7], (legs, result)
                    assert result.hits[0].text == "the bronze zeppelin"
        else:
            assert not ze.TEXT_AVAILABLE
            try:
                ze.open_text(Path(temporary) / "text-store", bundle)
            except ze.TextUnavailable as error:
                assert "text API is available on macOS arm64 only" in str(error)
            else:
                raise AssertionError("unsupported wheel accepted open_text")


if __name__ == "__main__":
    main()
