"""Typed exceptions generated from the live append-only C error table."""

from __future__ import annotations

import ctypes as ct
from enum import IntEnum
from typing import NoReturn, cast

from ._library import LIBRARY

LIBRARY.ze_error_code_name.argtypes = [ct.c_int32]
LIBRARY.ze_error_code_name.restype = ct.c_char_p
LIBRARY.ze_last_error_message.argtypes = [
    ct.c_uint64,
    ct.POINTER(ct.c_char),
    ct.c_size_t,
    ct.POINTER(ct.c_size_t),
]
LIBRARY.ze_last_error_message.restype = ct.c_int32


def error_code_name(code: int) -> str:
    """Return the exact static C name for an ABI status code."""

    raw = cast(bytes | None, LIBRARY.ze_error_code_name(code))
    if raw is None:
        raise RuntimeError("ze_error_code_name returned null")
    return raw.decode("ascii")


def _read_error_names() -> tuple[str, ...]:
    names: list[str] = []
    for code in range(1_024):
        name = error_code_name(code)
        if name == "ZE_ERR_UNKNOWN":
            return tuple(names)
        names.append(name)
    raise ImportError("Zeppelin Embed error table has no bounded terminator")


ERROR_NAMES = _read_error_names()
ErrorCode = IntEnum(  # type: ignore[misc]
    "ErrorCode",
    {name.removeprefix("ZE_"): code for code, name in enumerate(ERROR_NAMES)},
    module=__name__,
)


class ZeppelinError(Exception):
    """Base class for every non-success Zeppelin Embed status."""

    code: int = -1
    code_name: str = "ZE_ERR_UNKNOWN"

    def __init__(
        self,
        message: str,
        *,
        code: int | None = None,
        code_name: str | None = None,
    ) -> None:
        if code is not None:
            self.code = code
        if code_name is not None:
            self.code_name = code_name
        self.message = message
        super().__init__(f"{self.code_name}: {message}")


def _class_name(c_name: str) -> str:
    words = c_name.removeprefix("ZE_ERR_").lower().split("_")
    return "".join(word.capitalize() for word in words)


ERROR_TYPES: dict[int, type[ZeppelinError]] = {}
for _code, _name in enumerate(ERROR_NAMES):
    if _code == 0:
        continue
    _type = type(
        _class_name(_name),
        (ZeppelinError,),
        {
            "__module__": __name__,
            "code": ErrorCode(_code),
            "code_name": _name,
        },
    )
    globals()[_type.__name__] = _type
    ERROR_TYPES[_code] = _type


def last_error_message(handle: int = 0) -> str:
    """Copy the per-handle or process-global last error from the ABI."""

    required = ct.c_size_t()
    status = LIBRARY.ze_last_error_message(handle, None, 0, ct.byref(required))
    if status != 0:
        return f"ze_last_error_message failed with {error_code_name(status)}"
    buffer = ct.create_string_buffer(required.value + 1)
    status = LIBRARY.ze_last_error_message(
        handle,
        buffer,
        len(buffer),
        ct.byref(required),
    )
    if status != 0:
        return f"ze_last_error_message failed with {error_code_name(status)}"
    return buffer.raw[: required.value].decode("utf-8", errors="strict")


def raise_for_status(status: int, handle: int = 0) -> None:
    """Raise the generated typed exception for one non-OK ABI status."""

    if status == 0:
        return
    message = last_error_message(handle)
    error_type = ERROR_TYPES.get(status)
    if error_type is not None:
        raise error_type(message)
    raise ZeppelinError(
        message,
        code=status,
        code_name=error_code_name(status),
    )


def invalid_argument(message: str) -> NoReturn:
    """Raise the ABI-equivalent typed validation error before a C call."""

    error_type = ERROR_TYPES[1]
    raise error_type(message)


__all__ = [  # noqa: PLE0604
    "ERROR_NAMES",
    "ERROR_TYPES",
    "ErrorCode",
    "ZeppelinError",
    "error_code_name",
    "last_error_message",
    "raise_for_status",
    *[error_type.__name__ for error_type in ERROR_TYPES.values()],
]
