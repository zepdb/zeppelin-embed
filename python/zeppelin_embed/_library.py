"""Deterministic loading of the Zeppelin Embed shared library."""

from __future__ import annotations

import ctypes as ct
import os
import sys
from pathlib import Path


def _library_filename() -> str:
    if sys.platform == "darwin":
        return "libzeppelin_embed_ffi.dylib"
    if sys.platform == "win32":
        return "zeppelin_embed_ffi.dll"
    return "libzeppelin_embed_ffi.so"


def _candidates() -> tuple[Path, ...]:
    override = os.environ.get("ZEPPELIN_EMBED_LIBRARY")
    if override is not None:
        return (Path(override).expanduser(),)
    filename = _library_filename()
    package = Path(__file__).resolve().parent
    workspace = package.parents[1]
    return (
        package / ".dylibs" / filename,
        workspace / "target" / "debug" / filename,
        workspace / "target" / "release" / filename,
    )


def _load() -> tuple[ct.CDLL, Path]:
    candidates = _candidates()
    for candidate in candidates:
        if candidate.is_file():
            return ct.CDLL(str(candidate)), candidate.resolve()
    rendered = ", ".join(str(path) for path in candidates)
    raise ImportError(
        "Zeppelin Embed shared library not found; set "
        f"ZEPPELIN_EMBED_LIBRARY or place it at one of: {rendered}"
    )


LIBRARY, LIBRARY_PATH = _load()
