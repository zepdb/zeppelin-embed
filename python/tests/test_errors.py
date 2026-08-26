"""Typed exception generation and C-header drift tests."""

from __future__ import annotations

import ctypes as ct
import re
from pathlib import Path

import pytest

import zeppelin_embed as ze
from zeppelin_embed._library import LIBRARY

HEADER = (
    Path(__file__).resolve().parents[2]
    / "crates"
    / "zeppelin-embed-ffi"
    / "include"
    / "zeppelin_embed.h"
)


def _header_errors() -> tuple[str, ...]:
    body = HEADER.read_text(encoding="utf-8").split("enum ze_error_code", 1)[1]
    body = body.split("};", 1)[0]
    rows = re.findall(r"^\s*(ZE_[A-Z_]+)\s*=\s*(\d+),", body, re.MULTILINE)
    assert [int(value) for _, value in rows] == list(range(len(rows)))
    return tuple(name for name, _ in rows)


def test_error_hierarchy_matches_exported_c_table_and_header() -> None:
    assert ze.ERROR_NAMES == _header_errors()
    assert len(ze.ERROR_NAMES) == 29
    assert ze.ErrorCode.OK == 0
    assert ze.ErrorCode.ERR_UNSEALED_WRITES == 28
    for code, name in enumerate(ze.ERROR_NAMES):
        assert ze.error_code_name(code) == name
    assert ze.error_code_name(29) == "ZE_ERR_UNKNOWN"
    assert issubclass(ze.Cancelled, ze.ZeppelinError)
    assert ze.Cancelled.code == ze.ErrorCode.ERR_CANCELLED


def test_non_ok_status_raises_typed_subclass_with_last_error_message() -> None:
    handle = ct.c_uint64()
    LIBRARY.ze_open.argtypes = [ct.c_void_p, ct.POINTER(ct.c_uint64)]
    LIBRARY.ze_open.restype = ct.c_int32
    status = LIBRARY.ze_open(None, ct.byref(handle))
    with pytest.raises(ze.InvalidArgument) as caught:
        ze.raise_for_status(status, handle.value)
    assert caught.value.code == ze.ErrorCode.ERR_INVALID_ARGUMENT
    assert caught.value.code_name == "ZE_ERR_INVALID_ARGUMENT"
    assert caught.value.message
