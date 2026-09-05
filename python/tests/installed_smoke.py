"""Exercise the public package from an installed wheel."""

from __future__ import annotations

import tempfile
from pathlib import Path

import zeppelin_embed as ze


def main() -> None:
    package = Path(ze.__file__).resolve().parent
    assert ze.LIBRARY_PATH.parent == package / ".dylibs", ze.LIBRARY_PATH
    assert ze.LIBRARY_PATH.is_file(), ze.LIBRARY_PATH
    assert ze.ABI_VERSION == 1
    with tempfile.TemporaryDirectory() as temporary:
        with ze.open(Path(temporary) / "store") as store:
            assert store.state().state == ze.StoreState.OPEN


if __name__ == "__main__":
    main()
