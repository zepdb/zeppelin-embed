"""Setuptools hooks for the platform-specific Zeppelin Embed wheel."""

from __future__ import annotations

import json
import os
import platform as platform_module
import shutil
import subprocess
import sys
from pathlib import Path

from setuptools import Distribution, setup
from setuptools.command.bdist_wheel import bdist_wheel
from setuptools.command.build_py import build_py

PYTHON_ROOT = Path(__file__).resolve().parent
WORKSPACE_ROOT = PYTHON_ROOT.parents[1]
PACKAGE_NAME = "zeppelin_embed"


def _native_filename() -> str:
    if sys.platform == "darwin":
        return "libzeppelin_embed_ffi.dylib"
    if sys.platform.startswith("linux"):
        return "libzeppelin_embed_ffi.so"
    raise RuntimeError(f"Zeppelin Embed wheels are not configured for {sys.platform}")


def _cargo_target_dir() -> Path:
    configured = Path(os.environ.get("CARGO_TARGET_DIR", "target"))
    if configured.is_absolute():
        return configured
    return WORKSPACE_ROOT / configured


def _build_text() -> bool:
    return sys.platform == "darwin" and platform_module.machine() == "arm64"


def _configure_text_deployment_target() -> None:
    if not _build_text():
        return
    target = os.environ.setdefault("MACOSX_DEPLOYMENT_TARGET", "14.0")
    components = target.split(".")
    if len(components) != 2 or not all(part.isdigit() for part in components):
        raise RuntimeError(f"invalid MACOSX_DEPLOYMENT_TARGET: {target!r}")
    if tuple(map(int, components)) < (14, 0):
        raise RuntimeError(
            f"text wheels require MACOSX_DEPLOYMENT_TARGET >= 14.0; got {target!r}"
        )


class BinaryDistribution(Distribution):
    """Mark the wheel as platform-specific even though ctypes loads the binary."""

    def has_ext_modules(self) -> bool:
        return True


class PlatformWheel(bdist_wheel):
    """The Python code is generic while the bundled C ABI library is platform-specific."""

    def get_tag(self) -> tuple[str, str, str]:
        _configure_text_deployment_target()
        _python, _abi, platform = super().get_tag()
        deployment_target = os.environ.get("MACOSX_DEPLOYMENT_TARGET")
        if sys.platform == "darwin" and deployment_target is not None:
            components = deployment_target.split(".")
            if len(components) != 2 or not all(part.isdigit() for part in components):
                raise RuntimeError(
                    f"invalid MACOSX_DEPLOYMENT_TARGET: {deployment_target!r}"
                )
            version = "_".join(components)
            machine = platform_module.machine().replace("-", "_").replace(".", "_")
            platform = f"macosx_{version}_{machine}"
        return "py3", "none", platform


class BuildPyWithNative(build_py):
    """Build and copy the Rust cdylib into the installed Python package."""

    def _package_output(self, filename: str) -> Path:
        return Path(self.build_lib) / PACKAGE_NAME / filename

    def run(self) -> None:
        _configure_text_deployment_target()
        super().run()
        if self.editable_mode:
            return
        cargo = os.environ.get("CARGO", "cargo")
        command = [
            cargo,
            "build",
            "--locked",
            "--release",
            "-p",
            "zeppelin-embed-ffi",
            "--message-format=json-render-diagnostics",
        ]
        if _build_text():
            command.extend(["--features", "text"])
        result = subprocess.run(
            command,
            check=True,
            cwd=WORKSPACE_ROOT,
            stdout=subprocess.PIPE,
            text=True,
            env=os.environ.copy(),
        )

        native = _native_filename()
        source = _cargo_target_dir() / "release" / native
        if not source.is_file():
            raise RuntimeError(
                f"Cargo did not produce the expected native library: {source}"
            )
        destination = self._package_output(f".dylibs/{native}")
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, destination)
        metallib_destination = destination.with_name("mlx.metallib")
        if _build_text():
            # Use this build's mlx-sys output, including when Cargo reports it fresh.
            # MLX uses dladdr to find mlx.metallib beside the containing dylib.
            mlx_outputs = [
                Path(message["out_dir"])
                for line in result.stdout.splitlines()
                if (message := json.loads(line)).get("reason")
                == "build-script-executed"
                and "#mlx-sys@" in message.get("package_id", "")
            ]
            if len(mlx_outputs) != 1:
                raise RuntimeError(
                    f"expected one mlx-sys build output, got {mlx_outputs}"
                )
            metallib = mlx_outputs[0] / "build" / "lib" / "mlx.metallib"
            if not metallib.is_file():
                raise RuntimeError(f"MLX runtime shader library is missing: {metallib}")
            shutil.copy2(metallib, metallib_destination)
        else:
            metallib_destination.unlink(missing_ok=True)

        license_source = WORKSPACE_ROOT / "LICENSE"
        shutil.copy2(license_source, self._package_output("LICENSE"))

    def get_outputs(self, include_bytecode: bool = True) -> list[str]:
        outputs = super().get_outputs(include_bytecode=include_bytecode)
        if self.editable_mode:
            return outputs
        outputs.append(str(self._package_output(f".dylibs/{_native_filename()}")))
        if _build_text():
            outputs.append(str(self._package_output(".dylibs/mlx.metallib")))
        outputs.append(str(self._package_output("LICENSE")))
        return outputs


setup(
    cmdclass={"bdist_wheel": PlatformWheel, "build_py": BuildPyWithNative},
    distclass=BinaryDistribution,
)
