"""Validate the built wheel as an installed, self-contained distribution."""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
import tempfile
import venv
import zipfile
from pathlib import Path


def _native_filename() -> str:
    if sys.platform == "darwin":
        return "libzeppelin_embed_ffi.dylib"
    if sys.platform.startswith("linux"):
        return "libzeppelin_embed_ffi.so"
    raise RuntimeError(f"wheel verification is unsupported on {sys.platform}")


def _venv_python(root: Path) -> Path:
    if sys.platform == "win32":
        return root / "Scripts" / "python.exe"
    return root / "bin" / "python"


def verify(wheel: Path) -> None:
    native_member = f"zeppelin_embed/.dylibs/{_native_filename()}"
    with zipfile.ZipFile(wheel) as archive:
        names = archive.namelist()
        assert native_member in names, f"wheel is missing {native_member}"
        assert "zeppelin_embed/LICENSE" in names
        wheel_metadata = next(
            name for name in names if name.endswith(".dist-info/WHEEL")
        )
        wheel_contents = archive.read(wheel_metadata).decode("utf-8")
        assert "Root-Is-Purelib: false" in wheel_contents
        assert "Tag: py3-none-" in wheel_contents
        assert "Tag: py3-none-any" not in wheel_contents
        project_metadata = next(
            name for name in names if name.endswith(".dist-info/METADATA")
        )
        metadata_contents = archive.read(project_metadata).decode("utf-8")
        assert "License-Expression: GPL-3.0-only" in metadata_contents
        assert "Requires-Python: >=3.11" in metadata_contents

    with tempfile.TemporaryDirectory(prefix="zeppelin-wheel-") as temporary:
        root = Path(temporary)
        environment = root / "venv"
        venv.EnvBuilder(with_pip=True).create(environment)
        python = _venv_python(environment)
        subprocess.run(
            [
                str(python),
                "-m",
                "pip",
                "install",
                "--disable-pip-version-check",
                str(wheel),
            ],
            check=True,
            cwd=root,
        )

        clean_env = os.environ.copy()
        clean_env.pop("PYTHONPATH", None)
        clean_env.pop("ZEPPELIN_EMBED_LIBRARY", None)
        smoke = Path(__file__).with_name("installed_smoke.py")
        subprocess.run(
            [str(python), "-I", str(smoke)], check=True, cwd=root, env=clean_env
        )

        missing = root / "missing-native-library"
        override_env = clean_env | {"ZEPPELIN_EMBED_LIBRARY": str(missing)}
        overridden = subprocess.run(
            [str(python), "-I", "-c", "import zeppelin_embed"],
            check=False,
            cwd=root,
            env=override_env,
            capture_output=True,
            text=True,
        )
        assert overridden.returncode != 0
        assert str(missing) in overridden.stderr


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("wheel", type=Path)
    arguments = parser.parse_args()
    verify(arguments.wheel.resolve())


if __name__ == "__main__":
    main()
